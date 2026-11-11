//! Speaker separation while a capture runs: one diarization session per channel next to the
//! caption engine, fed the same growing WAV file.
//!
//! The diarization engine (role `diarization`, `crate::diarize`) is opened in its live mode
//! (`diarize.open`, `diarize.push` notifications, `diarize.event` updates, `diarize.close`; the
//! protocol is in `engines/nemotron-stream/README.md`). While it runs, the turns it reports name
//! the speaker of each caption segment provisionally (`LiveUpdate::Speakers`, an app event: nothing
//! of it is stored). When the capture stops the session is closed, and its final turns are a
//! `diarization` engine result that `store_live` gives to compose, exactly as the batch pass does,
//! so the first draft needs no second pass.
//!
//! Nothing here may cost a caption: the session has its own process, file reader and thread, and
//! never waits for the caption engine or makes it wait. A session that cannot start, dies, breaks
//! the protocol or falls more than `max_lag` behind the recording is ended and forgotten: its
//! labels are withdrawn and the draft gets its speakers from the post-recording pass as before.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use super::{EngineClient, Flags, Guard, LiveUpdate, OnUpdate, Push, SharedModel, WavTail, CHUNK_BYTES, CLOSE_TIMEOUT, MAX_BUFFERED, OPEN_TIMEOUT, POLL, ROLE_DIARIZATION};
use crate::contract::{DiarTurn, Diarization};
use crate::diarize::{self, DiarConfig};
use crate::error::Error;

/// Pending pushes for the diarization engine (~5 s of audio). It is small on purpose: audio the
/// engine cannot take waits in the file, where the lag check sees it.
pub const DIAR_QUEUE: usize = 50;
/// How far the session may trail the recording before it is given up.
pub const MAX_LAG: Duration = Duration::from_secs(20);
/// How long a stop waits for the separation after the captions are in.
pub const DIAR_GRACE: Duration = Duration::from_secs(5);
/// Provisional labels are recomputed at most this often per channel.
const LABEL_EVERY: Duration = Duration::from_millis(1000);

/// Where each caption segment of a channel lies (`segment` -> start, end in ms), kept by the
/// caption worker for the labeller.
pub type Spans = Arc<Mutex<BTreeMap<i64, (i64, i64)>>>;

/// The speaker of one caption segment: `speaker` numbers the channel's speakers in order of first
/// speech ("상대 1", "상대 2"); `None` when the channel has one speaker (it is just the channel's) or
/// nobody covers the segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SegmentSpeaker {
    pub segment: i64,
    pub speaker: Option<u32>,
}

/// What one channel's live diarization produced.
#[derive(Debug, Clone)]
pub struct DiarLive {
    pub index: u32,
    /// the engine's `diarize.close` result (a `diarization` engine result without `audio_sha256`:
    /// the session did not read a file, `store_live` names the one it was fed)
    pub result: Option<Value>,
    /// every sample of the channel file was fed and the session closed after the last one
    pub complete: bool,
    /// PCM bytes given to the engine, and their sha256: `store_live` attaches the turns to the stored
    /// file only when they are exactly the PCM of that file
    pub fed_bytes: u64,
    pub fed_sha256: Option<String>,
    pub model_sha256: Option<String>,
    pub model_pinned: bool,
    /// the merge rule the labels used, applied again by `store_live`
    pub min_speaker_ms: i64,
    pub error: Option<String>,
}

/// The turns reported so far, folded: `keep` turns stay, the rest is replaced.
#[derive(Default)]
struct Fold {
    turns: Vec<DiarTurn>,
    labelled_ms: i64,
    /// changed since the labels were last computed
    dirty: bool,
}

/// Apply one `diarize.event`. `Err` is a broken engine.
fn apply(f: &mut Fold, ev: &Value) -> Result<(), String> {
    let keep = ev["keep"].as_u64().filter(|k| *k as usize <= f.turns.len()).ok_or("an update keeps turns that were never sent")? as usize;
    let new: Vec<DiarTurn> = serde_json::from_value(ev["turns"].clone()).map_err(|_| "an update's turns break the schema".to_string())?;
    let labelled = ev["labelled_ms"].as_i64().filter(|l| *l >= f.labelled_ms && *l <= 86_400_000).ok_or("an update's labelled_ms went back")?;
    if keep + new.len() > diarize::MAX_TURNS {
        return Err(format!("more than {} turns", diarize::MAX_TURNS));
    }
    let mut prev = f.turns[..keep].last().map_or(0, |t| t.start_ms);
    for t in &new {
        let label_ok = !t.speaker.is_empty() && t.speaker.chars().count() <= 64 && !t.speaker.chars().any(char::is_control);
        if t.start_ms < prev || t.end_ms <= t.start_ms || t.end_ms > 86_400_000 || !label_ok {
            return Err("an update's turns are out of order or malformed".into());
        }
        prev = t.start_ms;
    }
    f.dirty |= keep < f.turns.len() || !new.is_empty();
    f.turns.truncate(keep);
    f.turns.extend(new);
    f.labelled_ms = labelled;
    Ok(())
}

/// The speaker number of every segment in `spans`, from `turns` after the same merge of small
/// speakers the stored version gets: a channel with one speaker left has no numbers. A segment
/// goes to the speaker who covers most of it.
pub fn label_segments(turns: &[DiarTurn], spans: &BTreeMap<i64, (i64, i64)>, min_speaker_ms: i64) -> BTreeMap<i64, Option<u32>> {
    let mut d = Diarization { turns: turns.to_vec() };
    diarize::merge_small_speakers(&mut d, min_speaker_ms);
    let numbers: BTreeMap<&str, u32> = if diarize::speaker_count(&d) <= 1 {
        BTreeMap::new()
    } else {
        let mut n = BTreeMap::new();
        for t in &d.turns {
            let next = n.len() as u32 + 1;
            n.entry(t.speaker.as_str()).or_insert(next);
        }
        n
    };
    // turns are in start order: those that can overlap [s, e) start before e and are no longer
    // than the longest turn
    let longest = d.turns.iter().map(|t| t.end_ms - t.start_ms).max().unwrap_or(0);
    spans
        .iter()
        .map(|(&seg, &(s, e))| {
            if numbers.is_empty() {
                return (seg, None);
            }
            let upto = d.turns.partition_point(|t| t.start_ms < e.max(s + 1));
            let mut cover: BTreeMap<u32, i64> = BTreeMap::new();
            for t in d.turns[..upto].iter().rev().take_while(|t| t.start_ms + longest > s) {
                let o = t.end_ms.min(e.max(s + 1)) - t.start_ms.max(s);
                if o > 0 {
                    *cover.entry(numbers[t.speaker.as_str()]).or_default() += o;
                }
            }
            (seg, cover.into_iter().max_by_key(|(n, o)| (*o, std::cmp::Reverse(*n))).map(|(n, _)| n))
        })
        .collect()
}

/// How a session knows the captions are up, and tells that it is.
pub(super) struct Signals {
    /// per caption channel: its engine listens, or its worker has ended
    pub captions: Vec<Arc<AtomicBool>>,
    /// this session is open, or its worker has ended
    pub opened: Arc<AtomicBool>,
}

/// Why the engine failed: its own words, unless the stop's deadline ended it (then asking it would
/// only cost the stop time).
fn fault_or(engine: &EngineClient, killed: bool, other: String) -> String {
    if killed {
        "stopped at the finish deadline".into()
    } else {
        engine.fault().unwrap_or(other)
    }
}

/// Run one channel's session; `path` is the channel's growing WAV, `spans` its caption segments.
/// It starts once every caption engine listens (or gave up): the captions warm up first.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_channel(cfg: &DiarConfig, model: &SharedModel, index: u32, path: &Path, flags: &Flags, on_update: &OnUpdate, spans: &Spans, max_lag: Duration, signals: &Signals) -> DiarLive {
    let mut res = DiarLive { index, result: None, complete: false, fed_bytes: 0, fed_sha256: None, model_sha256: None, model_pinned: false, min_speaker_ms: cfg.min_speaker_ms, error: None };
    let cancelled = || flags.cancel.load(Ordering::SeqCst);
    let abort_loading = || cancelled() || flags.stop.load(Ordering::SeqCst) || flags.kill.load(Ordering::SeqCst);
    let killed = || flags.kill.load(Ordering::SeqCst) || flags.diar_kill.load(Ordering::SeqCst);
    let abort_late = || cancelled() || killed();

    let mut tail = loop {
        if abort_loading() {
            return res;
        }
        match WavTail::open(path) {
            Ok(t) => break t,
            Err(_) => std::thread::sleep(POLL),
        }
    };
    while !signals.captions.iter().all(|c| c.load(Ordering::SeqCst)) {
        if abort_loading() {
            return res;
        }
        std::thread::sleep(POLL);
    }
    let staged = match model.get(&cfg.model_dir, cfg.unpinned, &abort_loading) {
        Ok(m) => m,
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            return res;
        }
    };
    res.model_sha256 = Some(staged.check.digest.clone());
    res.model_pinned = staged.check.pinned;

    let fold = Arc::new(Mutex::new(Fold::default()));
    let bad: Arc<Mutex<Option<String>>> = Arc::default();
    let (f2, b2) = (fold.clone(), bad.clone());
    let mut guard = Guard::default();
    let started = EngineClient::spawn_bin(
        &cfg.engine_bin,
        ("MEETKO_DIAR_MODEL", &staged.path),
        ROLE_DIARIZATION,
        DIAR_QUEUE,
        move |ev| {
            if b2.lock().unwrap().is_some() {
                return;
            }
            let r = guard.rate(Instant::now()).and_then(|()| apply(&mut f2.lock().unwrap(), &ev));
            if let Err(why) = r {
                *b2.lock().unwrap() = Some(why);
            }
        },
        &abort_loading,
    );
    let mut engine = match started {
        Ok(e) => e,
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            return res;
        }
    };
    match engine.call_with("diarize.open", json!({ "sample_rate": 16000 }), OPEN_TIMEOUT, &abort_loading) {
        Ok(_) => signals.opened.store(true, Ordering::SeqCst),
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            return res;
        }
    }

    // labels sent so far, to send only what changed and to withdraw them all if the session fails
    let mut sent: BTreeMap<i64, Option<u32>> = BTreeMap::new();
    let mut last_label = Instant::now();
    let relabel = |force: bool, sent: &mut BTreeMap<i64, Option<u32>>| {
        let turns = {
            let mut f = fold.lock().unwrap();
            if !f.dirty && !force {
                return;
            }
            f.dirty = false;
            f.turns.clone()
        };
        let spans = spans.lock().unwrap().clone();
        let now = label_segments(&turns, &spans, cfg.min_speaker_ms);
        let changed: Vec<SegmentSpeaker> = now.iter().filter(|(seg, n)| sent.get(seg).copied().unwrap_or(None) != **n).map(|(&segment, &speaker)| SegmentSpeaker { segment, speaker }).collect();
        if !changed.is_empty() {
            sent.extend(changed.iter().map(|c| (c.segment, c.speaker)));
            on_update(LiveUpdate::Speakers { channel: index, labels: changed });
        }
    };

    let mut buf: Vec<u8> = Vec::new();
    let (mut read_bytes, mut fed_bytes) = (0u64, 0u64);
    let mut fed_hash = sha2::Sha256::default();
    let mut failure: Option<String> = None;
    let mut drained = false;
    let mut first_read: Option<(Instant, i64)> = None;
    loop {
        if cancelled() {
            return res;
        }
        if killed() {
            failure = Some("stopped at the finish deadline".into());
            break;
        }
        if let Some(why) = bad.lock().unwrap().clone() {
            failure = Some(why);
            break;
        }
        if engine.is_dead() {
            failure = Some(fault_or(&engine, killed(), "the diarization engine stopped".into()));
            break;
        }
        let stopping = flags.stop.load(Ordering::SeqCst);
        let idle;
        if buf.len() < MAX_BUFFERED {
            match tail.read_new() {
                Ok(bytes) => {
                    idle = bytes.is_empty();
                    read_bytes += bytes.len() as u64;
                    buf.extend_from_slice(&bytes);
                }
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        } else {
            idle = false;
        }
        // behind by more than max_lag of audio: give up rather than fall further behind (a stop
        // would then have to wait for all of it). The audio recorded while the model was staged
        // and loaded comes in one read; the engine is given that much extra to catch up, up to
        // max_lag again. What is recorded is what was read, or (when reading paused because the
        // engine is that far behind) at least the first read plus the time since: a capture
        // writes in real time.
        if first_read.is_none() && read_bytes > 0 {
            first_read = Some((Instant::now(), (read_bytes / 32) as i64));
        }
        if let Some((at, first_ms)) = first_read {
            let read_ms = (read_bytes / 32) as i64;
            // only while reading pauses: a provider that writes nothing for a while (a gap) or has stopped is not behind
            let paused = buf.len() >= MAX_BUFFERED && !stopping;
            let recorded = if paused { read_ms.max(first_ms + at.elapsed().as_millis() as i64) } else { read_ms };
            let labelled = fold.lock().unwrap().labelled_ms;
            if recorded - labelled > max_lag.as_millis() as i64 + first_ms.min(max_lag.as_millis() as i64) {
                failure = Some(format!("fell more than {} s behind the recording", max_lag.as_secs()));
                break;
            }
        }
        let (mut sent_bytes, mut full) = (0, false);
        while buf.len() - sent_bytes >= CHUNK_BYTES {
            match engine.push_as("diarize.push", &buf[sent_bytes..sent_bytes + CHUNK_BYTES]) {
                Ok(Push::Queued) => {
                    sha2::Digest::update(&mut fed_hash, &buf[sent_bytes..sent_bytes + CHUNK_BYTES]);
                    sent_bytes += CHUNK_BYTES;
                }
                Ok(Push::Full) => {
                    full = true;
                    break;
                }
                Err(e) => {
                    failure = Some(fault_or(&engine, killed(), e.to_string()));
                    break;
                }
            }
        }
        buf.drain(..sent_bytes);
        fed_bytes += sent_bytes as u64;
        if failure.is_some() {
            break;
        }
        if last_label.elapsed() >= LABEL_EVERY {
            last_label = Instant::now();
            relabel(false, &mut sent);
        }
        if full {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        if idle {
            if stopping {
                drained = true;
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    if failure.is_none() && !buf.is_empty() {
        loop {
            if abort_late() {
                failure = Some("stopped at the finish deadline".into());
                break;
            }
            match engine.push_as("diarize.push", &buf) {
                Ok(Push::Queued) => {
                    sha2::Digest::update(&mut fed_hash, &buf);
                    fed_bytes += buf.len() as u64;
                    break;
                }
                Ok(Push::Full) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
    }
    res.fed_bytes = fed_bytes;
    res.fed_sha256 = Some(crate::util::hex(&sha2::Digest::finalize(fed_hash)));
    if failure.is_none() {
        match engine.call_with("diarize.close", json!({}), CLOSE_TIMEOUT, &abort_late) {
            Ok(r) if r.is_object() => {
                res.result = Some(r);
                res.complete = drained;
            }
            Ok(_) => failure = Some("diarize.close did not return a result".into()),
            Err(Error::Cancelled) if cancelled() => return res,
            Err(e) => failure = Some(fault_or(&engine, killed(), e.to_string())),
        }
        if let Some(why) = bad.lock().unwrap().clone() {
            failure.get_or_insert(why);
        }
    }
    if let Some(why) = failure {
        res.error = Some(why);
        res.result = None;
        res.complete = false;
        // withdraw the provisional labels: the draft's speakers will come from the batch pass
        let withdrawn: Vec<SegmentSpeaker> = sent.iter().filter(|(_, n)| n.is_some()).map(|(&segment, _)| SegmentSpeaker { segment, speaker: None }).collect();
        if !withdrawn.is_empty() {
            on_update(LiveUpdate::Speakers { channel: index, labels: withdrawn });
        }
        engine.kill();
        return res;
    }
    // the last labels from the final turns, the ones the stored version gets
    if let Some(turns) = res.result.as_ref().and_then(|r| serde_json::from_value::<Vec<DiarTurn>>(r["turns"].clone()).ok()) {
        let mut f = fold.lock().unwrap();
        f.turns = turns;
        f.dirty = true;
    }
    relabel(true, &mut sent);
    engine.kill(); // the result is in hand: no polite shutdown for the stop to wait on
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(a: i64, b: i64, s: &str) -> DiarTurn {
        DiarTurn { start_ms: a, end_ms: b, speaker: s.into() }
    }

    fn spans(v: &[(i64, i64, i64)]) -> BTreeMap<i64, (i64, i64)> {
        v.iter().map(|&(seg, a, b)| (seg, (a, b))).collect()
    }

    #[test]
    fn segments_go_to_the_speaker_who_covers_most_of_them() {
        let turns = [t(0, 20_000, "spk2"), t(20_000, 45_000, "spk1"), t(45_000, 50_000, "spk2")];
        let l = label_segments(&turns, &spans(&[(0, 1000, 5000), (1, 18_000, 26_000), (2, 44_000, 49_000), (3, 60_000, 61_000), (4, 30_000, 30_000)]), 10_000);
        // spk2 speaks first: 1; spk1: 2
        assert_eq!(l, BTreeMap::from([(0, Some(1)), (1, Some(2)), (2, Some(1)), (3, None), (4, Some(2))]));
    }

    #[test]
    fn one_speaker_left_after_the_merge_means_no_numbers() {
        // spk2 has 3 s in all: merged into spk1, so the channel is one speaker
        let turns = [t(0, 20_000, "spk1"), t(20_000, 23_000, "spk2")];
        let l = label_segments(&turns, &spans(&[(0, 0, 5000), (1, 20_500, 22_000)]), 10_000);
        assert_eq!(l, BTreeMap::from([(0, None), (1, None)]));
        assert_eq!(label_segments(&turns, &spans(&[(1, 20_500, 22_000)]), 0)[&1], Some(2), "without the rule spk2 is a speaker");
        assert!(label_segments(&[], &spans(&[(0, 0, 1)]), 0)[&0].is_none());
    }

    #[test]
    fn a_long_turn_far_back_still_covers_a_late_segment() {
        let turns = [t(0, 600_000, "a"), t(10_000, 20_000, "b"), t(30_000, 31_000, "b")];
        let l = label_segments(&turns, &spans(&[(7, 500_000, 501_000)]), 0);
        assert_eq!(l[&7], Some(1));
    }

    #[test]
    fn updates_fold_by_keep_and_broken_ones_are_refused() {
        let mut f = Fold::default();
        let ev = |keep: usize, turns: Value, l: i64| json!({ "keep": keep, "turns": turns, "labelled_ms": l, "emitted_at_ms": l });
        apply(&mut f, &ev(0, json!([{"start_ms": 0, "end_ms": 900, "speaker": "spk1"}]), 1000)).unwrap();
        apply(&mut f, &ev(0, json!([{"start_ms": 0, "end_ms": 1500, "speaker": "spk1"}, {"start_ms": 1600, "end_ms": 1900, "speaker": "spk2"}]), 2000)).unwrap();
        assert_eq!(f.turns, [t(0, 1500, "spk1"), t(1600, 1900, "spk2")]);
        assert!(f.dirty);
        f.dirty = false;
        apply(&mut f, &ev(2, json!([]), 3000)).unwrap();
        assert!(!f.dirty, "a heartbeat changes nothing");
        assert_eq!(f.labelled_ms, 3000);
        for (bad, why) in [
            (ev(3, json!([]), 4000), "keeps more than there is"),
            (ev(2, json!([]), 2500), "labelled_ms went back"),
            (ev(2, json!([{"start_ms": 100, "end_ms": 200, "speaker": "x"}]), 4000), "starts before the kept turns"),
            (ev(2, json!([{"start_ms": 2000, "end_ms": 2000, "speaker": "x"}]), 4000), "empty"),
            (ev(2, json!([{"start_ms": 2000, "end_ms": 2100, "speaker": "a\nb"}]), 4000), "control character"),
            (ev(2, json!([{"start_ms": "soon", "end_ms": 2100, "speaker": "x"}]), 4000), "schema"),
            (json!({ "turns": [] }), "no keep"),
        ] {
            assert!(apply(&mut f, &bad).is_err(), "{why}");
        }
    }
}
