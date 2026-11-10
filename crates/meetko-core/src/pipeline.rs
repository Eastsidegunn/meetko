//! Wiring: stored channel files -> engine -> compose -> a stored draft version; plugin runs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::contract::{compose, engine_result_validator, schema_errors, ChannelInput, EngineWord, MeetingData, Span, VersionKind};
use crate::diarize::{self, DiarEngine, DiarRequest};
use crate::error::{Error, Result};
use crate::manifest::plugin_code_sha256;
use crate::store::Store;
use crate::util;
use crate::whisper::{BatchEngine, BatchRequest, Progress};

/// A version-level producer entry from an engine result (as `tools/build_fixtures.py::producer`).
/// `network` is the engine's own statement; a result that does not make one counts as external.
pub fn producer(result: &Value, role: &str, channel: u32) -> Value {
    let network = result.get("network").and_then(Value::as_bool).unwrap_or(true);
    let mut p = json!({ "role": role, "engine": result["engine"], "channels": [channel], "network": network });
    for k in ["model", "params", "word_timing"] {
        if let Some(v) = result.get(k) {
            p[k] = v.clone();
        }
    }
    p
}

fn intersect(a: &[Span], b: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let (s, e) = (x.start_ms.max(y.start_ms), x.end_ms.min(y.end_ms));
            if e > s {
                out.push(Span { start_ms: s, end_ms: e });
            }
        }
    }
    out.sort_by_key(|s| s.start_ms);
    out
}

/// An asr.batch engine result as compose input. Without a diarization (see `with_diarization`)
/// the channel decides the speaker and there are no speaker spans. `capture_coverage` (from a capture
/// manifest) narrows what counts as transcribed; words an engine put into spans that were
/// never recorded are dropped and counted in the producer.
pub fn batch_channel(
    result: &Value,
    channel: u32,
    role: &str,
    label: Option<&str>,
    capture_coverage: Option<&[Span]>,
) -> Result<ChannelInput> {
    let mut words: Vec<EngineWord> = serde_json::from_value(result["words"].clone())?;
    let mut coverage: Vec<Span> = serde_json::from_value(result["coverage"].clone())?;
    let mut asr = producer(result, "asr", channel);
    // engine-result words are words by definition; an engine may say otherwise
    asr["granularity"] = result.get("granularity").cloned().unwrap_or_else(|| json!("word"));
    if let Some(cap) = capture_coverage {
        // a point marker (a 1 ms trim hole) cuts no audio that is in the file: bridge it, or
        // every word spoken across a drift correction would be dropped
        let mut bridged: Vec<Span> = Vec::new();
        for s in cap {
            match bridged.last_mut() {
                Some(last) if s.start_ms - last.end_ms <= 1 => last.end_ms = last.end_ms.max(s.end_ms),
                _ => bridged.push(s.clone()),
            }
        }
        coverage = intersect(&coverage, &bridged);
        let before = words.len();
        // engines pad word ends (10 ms grid, segment end, endpoint silence) past the audio that
        // exists: a word that starts in coverage ends at most where its span does
        words.retain_mut(|w| match coverage.iter().find(|c| c.start_ms <= w.start_ms && w.start_ms < c.end_ms) {
            Some(c) => {
                w.end_ms = w.end_ms.min(c.end_ms).max(w.start_ms);
                true
            }
            None => false,
        });
        if words.len() < before {
            asr["dropped_words_outside_coverage"] = json!(before - words.len());
        }
    }
    let mut c = ChannelInput::new(role, words, coverage, vec![asr]);
    c.label = label.map(String::from);
    Ok(c)
}

/// Give a channel the speakers of a diarization engine result. The producer is recorded with the
/// words it speaks for, and with the rule that merged small speakers (`min_speaker_ms`, and how
/// many were merged). One speaker (or none) is the channel's own speaker, as without diarization,
/// with the turns kept as its speech spans; several become "<channel label> 1", "<label> 2" ...
/// in order of first speech (compose names them). `duration_ms` is the recording's. Fails, leaving
/// the channel as it was, when the result is not a diarization of exactly this audio.
pub fn with_diarization(c: &mut ChannelInput, result: &Value, audio_sha256: &str, duration_ms: i64, channel: u32, min_speaker_ms: i64) -> Result<()> {
    let mut d = diarize::checked(result, audio_sha256, duration_ms)?;
    let merged = diarize::merge_small_speakers(&mut d, min_speaker_ms);
    let mut p = producer(result, "diarization", channel);
    if !p["params"].is_object() {
        p["params"] = json!({});
    }
    p["params"]["min_speaker_ms"] = json!(min_speaker_ms);
    p["params"]["merged_speakers"] = json!(merged);
    c.single_speaker = diarize::speaker_count(&d) <= 1;
    c.diarization = Some(d);
    c.producers.push(p);
    Ok(())
}

/// Copy a stored channel file to a private temp dir under an ASCII name while hashing it.
/// The engine reads this copy, so the sha256 recorded as provenance is the sha256 of the
/// bytes it read (a stored file replaced after import is caught here).
fn stage_for_engine(f: &crate::store::ChannelFile, dir: &std::path::Path) -> Result<()> {
    let mut src = std::fs::File::open(&f.path)?;
    let mut dst = util::create_private(&dir.join("input.wav"))?;
    let mut h = sha2::Sha256::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut h, &buf[..n]);
        dst.write_all(&buf[..n])?;
    }
    let have = util::hex(&sha2::Digest::finalize(h));
    if have != f.sha256 {
        return Err(Error::Corrupt(format!("channel {} file no longer matches its stored sha256; re-import the recording", f.index)));
    }
    Ok(())
}

/// Compose and store the draft. Nothing a diarization engine says may cost the transcript: if the
/// draft does not store with its speakers, it is stored without them (`plain`) and a warning says so.
fn store_draft(
    create: &dyn Fn(MeetingData) -> Result<MeetingData>,
    meeting: crate::contract::Meeting,
    recording: crate::contract::Recording,
    channels: &BTreeMap<u32, ChannelInput>,
    plain: &BTreeMap<u32, ChannelInput>,
    warnings: &mut Vec<String>,
) -> Result<MeetingData> {
    let version = || ("pending".to_string(), VersionKind::Draft);
    match create(compose(meeting.clone(), recording.clone(), version(), channels)) {
        Err(e) if channels.values().any(|c| c.diarization.is_some()) && !matches!(e, Error::Cancelled) => {
            warnings.push(format!("speaker separation was dropped, the draft did not validate with it: {e}"));
            create(compose(meeting, recording, version(), plain))
        }
        r => r,
    }
}

/// A stored draft and what went wrong without costing the transcript.
pub struct Transcribed {
    pub data: MeetingData,
    /// one line per channel whose speaker separation failed; that channel keeps its one speaker
    pub warnings: Vec<String>,
}

/// Transcribe every channel of a meeting and store the result as a new draft, each channel one
/// speaker. The version id is allocated only after the (long) transcription, inside the store's
/// write lock.
pub fn transcribe(
    store: &Store,
    engine: &dyn BatchEngine,
    meeting_id: &str,
    language: Option<&str>,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<MeetingData> {
    Ok(transcribe_with(store, engine, None, meeting_id, language, progress, cancel)?.data)
}

/// `transcribe`, then (when `diar` is given) separate the speakers of each channel. Diarization is
/// an addition to a transcript that exists: an engine that fails, times out or returns something
/// that breaks the contract costs that channel its speakers and adds a warning, never the draft.
/// Cancelling still cancels the whole run.
pub fn transcribe_with(
    store: &Store,
    engine: &dyn BatchEngine,
    diar: Option<&dyn DiarEngine>,
    meeting_id: &str,
    language: Option<&str>,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Transcribed> {
    let (meeting, recording, files) = store.meeting_and_recording(meeting_id)?;
    let lang = language.or(meeting.language_hint.as_deref()).unwrap_or("ko").to_string();
    let mut channels = BTreeMap::new();
    let mut warnings = Vec::new();
    // the channels as they are without diarization: what the draft falls back to
    let mut plain = BTreeMap::new();
    for f in &files {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let tmp = tempfile::tempdir()?;
        stage_for_engine(f, tmp.path())?;
        let req = BatchRequest {
            wav: &tmp.path().join("input.wav"),
            audio_sha256: &f.sha256,
            language: &lang,
            channel: f.index,
            channels: files.len() as u32,
            data_dir: store.dir(),
        };
        let res = engine.transcribe(&req, progress, cancel)?;
        let errs = schema_errors(engine_result_validator(), &res);
        if !errs.is_empty() {
            return Err(Error::BadEngineOutput(errs.into_iter().take(5).collect::<Vec<_>>().join("; ")));
        }
        let mut input = batch_channel(&res, f.index, &f.role, f.label.as_deref(), f.coverage.as_deref())?;
        if let Some(d) = diar {
            let req = DiarRequest { wav: &tmp.path().join("input.wav"), audio_sha256: &f.sha256, channel: f.index, duration_ms: f.duration_ms };
            progress(Progress { channel: f.index, channels: files.len() as u32, percent: 100, diarizing: true });
            match d.diarize(&req, cancel).and_then(|r| with_diarization(&mut input, &r, &f.sha256, recording.duration_ms, f.index, d.min_speaker_ms())) {
                Ok(()) => {}
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => warnings.push(format!("speaker separation failed on channel {}: {e}", f.index + 1)),
            }
        }
        if diar.is_some() {
            plain.insert(f.index, ChannelInput { diarization: None, single_speaker: false, producers: input.producers.iter().filter(|p| p["role"] != "diarization").cloned().collect(), ..input.clone() });
        }
        channels.insert(f.index, input);
    }
    let data = store_draft(&|md| store.create_version(md), meeting, recording, &channels, &plain, &mut warnings)?;
    Ok(Transcribed { data, warnings })
}

/// The compose input of each channel of a stored version, read back from it: its words (meeting
/// time; offsets are 0), coverage, label and producers. Composing these again without a diarization
/// gives the same content. Speaker spans and diarization producers are not carried: a caller that
/// recomposes adds its own diarization. A later edit step (for example echo removal) works here:
/// it drops words from these inputs and appends its own `edit` producer before compose.
///
/// Only the channels the version has are returned: one that a producer names, or that has coverage
/// or a speaker. `store_live` leaves out a channel whose engine never ran; emitting it anyway would
/// add a phantom speaker and renumber the others. A producer naming a channel the meeting does not
/// have is `Invalid`.
pub fn channels_of(md: &MeetingData) -> Result<BTreeMap<u32, ChannelInput>> {
    let first_of = |p: &Value| p["channels"].as_array().and_then(|a| a.first()).and_then(Value::as_u64).map(|c| c as u32);
    let known: BTreeSet<u32> = md.meeting.channels.iter().map(|c| c.index).collect();
    let mut present: BTreeSet<u32> = BTreeSet::new();
    for p in &md.version.producers {
        for c in p["channels"].as_array().into_iter().flatten() {
            let c = c.as_u64().map(|c| c as u32).filter(|c| known.contains(c));
            present.insert(c.ok_or_else(|| Error::Invalid(format!("a producer of {} names a channel the meeting does not have", md.version.id)))?);
        }
    }
    present.extend(md.coverage.iter().map(|c| c.channel));
    present.extend(md.speakers.iter().map(|s| s.channel));
    let mut out = BTreeMap::new();
    for ch in md.meeting.channels.iter().filter(|c| present.contains(&c.index)) {
        let words = md
            .words
            .iter()
            .filter(|w| w.channel == ch.index)
            .map(|w| EngineWord { start_ms: w.start_ms, end_ms: w.end_ms, text: w.text.clone(), p: w.p, end_estimated: w.end_estimated })
            .collect();
        let coverage = md.coverage.iter().filter(|c| c.channel == ch.index).map(|c| Span { start_ms: c.start_ms, end_ms: c.end_ms }).collect();
        let mut c = ChannelInput::new(&ch.role, words, coverage, vec![]);
        c.label = ch.label.clone();
        out.insert(ch.index, c);
    }
    // a producer belongs to the first channel it names, one that names none to the first channel
    // there is (compose concatenates them in channel order)
    let first = out.keys().next().copied();
    for p in md.version.producers.iter().filter(|p| p["role"] != "diarization") {
        if let Some(c) = first_of(p).or(first).and_then(|i| out.get_mut(&i)) {
            c.producers.push(p.clone());
        }
    }
    Ok(out)
}

/// Make the first draft of a meeting from its `live` version, so the live captions can be reviewed,
/// approved and given to plugins without a batch transcription. The draft's parent is the live
/// version and its producers are the live engine's own (role `asr`, `word_timing` transducer,
/// `granularity` token, the engine's `network` flag): nothing claims a batch engine ran.
///
/// Without `diar` the draft is the live content unchanged. With `diar` each channel is separated
/// into speakers like a batch draft (`with_diarization`); a channel whose separation fails keeps its
/// one speaker and adds a warning. Idempotent: a draft already made from this live version is
/// returned without running anything; a meeting that already has another draft or an approval is
/// refused with `StaleDraft` (see `Store::create_draft_from`).
///
/// So a draft made without speaker separation (none configured at the time, or it failed) does not
/// gain speakers later from this function: it is the first draft, and the next one comes only from
/// a batch transcription (`transcribe_with`, the app's 다시 받아쓰기).
pub fn draft_from_live(
    store: &Store,
    live_id: &str,
    diar: Option<&dyn DiarEngine>,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Transcribed> {
    let row = store.version_row(live_id)?;
    if row.kind != VersionKind::Live {
        return Err(Error::Invalid(format!("{live_id} is {}, a draft is made from a live version", row.kind.as_str())));
    }
    if let Some(id) = store.draft_of(live_id)? {
        return Ok(Transcribed { data: store.version(&id)?, warnings: vec![] });
    }
    if store.has_draft_or_approved(&row.meeting_id)? {
        return Err(Error::StaleDraft(format!("the meeting already has a draft or an approved version; {live_id} is not made a draft over it")));
    }
    let live = store.version(live_id)?;
    let create = |md: MeetingData| store.create_draft_from(live_id, md);
    let Some(diar) = diar else {
        return Ok(Transcribed { data: create(live)?, warnings: vec![] });
    };
    let mut channels = channels_of(&live)?;
    let mut warnings = Vec::new();
    // only the channels the live version has are separated: one without captions has no words to give speakers
    // progress names the real channel out of all of them, as the batch path does
    let all = store.channel_files(&row.meeting_id)?;
    let total = all.len() as u32;
    let files: Vec<_> = all.into_iter().filter(|f| channels.contains_key(&f.index)).collect();
    for f in &files {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let Some(input) = channels.get_mut(&f.index) else { continue };
        let step = |percent| progress(Progress { channel: f.index, channels: total, percent, diarizing: true });
        step(0);
        let tmp = tempfile::tempdir()?;
        let separated = stage_for_engine(f, tmp.path()).and_then(|()| {
            let req = DiarRequest { wav: &tmp.path().join("input.wav"), audio_sha256: &f.sha256, channel: f.index, duration_ms: f.duration_ms };
            let r = diar.diarize(&req, cancel)?;
            with_diarization(input, &r, &f.sha256, live.recording.duration_ms, f.index, diar.min_speaker_ms())
        });
        match separated {
            Ok(()) => {}
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => warnings.push(format!("speaker separation failed on channel {}: {e}", f.index + 1)),
        }
        step(100);
    }
    // a cancel that came during the last channel's separation still cancels
    if cancel.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    if channels.values().all(|c| c.diarization.is_none()) {
        return Ok(Transcribed { data: create(live)?, warnings });
    }
    // as `store_draft`: speakers never cost the transcript; the fallback is the live content as is
    let data = match create(compose(live.meeting.clone(), live.recording.clone(), ("pending".into(), VersionKind::Draft), &channels)) {
        Err(e) if !matches!(e, Error::Cancelled | Error::StaleDraft(_)) => {
            warnings.push(format!("speaker separation was dropped, the draft did not validate with it: {e}"));
            create(live)?
        }
        r => r?,
    };
    Ok(Transcribed { data, warnings })
}

/// Run a registered plugin on a stored version and store the result. Refuses when the
/// plugin's code or manifest no longer matches what was added.
pub fn run_plugin(store: &Store, plugin_id: &str, version_id: &str, timeout: Duration) -> Result<i64> {
    let (row, manifest) = store.plugin(plugin_id)?;
    let manifest_now = std::fs::read(row.dir.join("meetko.toml")).map(|b| util::sha256_bytes(&b))?;
    if manifest_now != row.manifest_sha256 || plugin_code_sha256(&manifest, &row.dir)? != row.code_sha256 {
        return Err(Error::Plugin(format!(
            "{plugin_id}: the plugin's code or manifest changed since it was added; review it and add it again"
        )));
    }
    let raw = store.version_raw(version_id)?; // the plugin gets the document as stored
    let iso = crate::sandbox::Isolation::production(store.dir());
    let (out, sandbox) = crate::host::run_once_with(&manifest, &row.dir, &raw, timeout, &iso)?;
    store.add_result_sandboxed(&row, version_id, &out, sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Basis, Diarization, DiarTurn, Meeting, Recording};

    fn diarized_and_plain() -> (BTreeMap<u32, ChannelInput>, BTreeMap<u32, ChannelInput>) {
        let words = vec![EngineWord { start_ms: 0, end_ms: 100, text: "가".into(), p: None, end_estimated: false }];
        let plain = ChannelInput::new("loopback", words, vec![Span { start_ms: 0, end_ms: 1000 }], vec![]);
        let mut d = plain.clone();
        d.diarization = Some(Diarization { turns: vec![DiarTurn { start_ms: 0, end_ms: 100, speaker: "a".into() }, DiarTurn { start_ms: 200, end_ms: 300, speaker: "b".into() }] });
        (BTreeMap::from([(1, d)]), BTreeMap::from([(1, plain)]))
    }

    fn meeting_and_recording() -> (Meeting, Recording) {
        let md = crate::contract::compose(
            serde_json::from_str::<MeetingData>(include_str!("../../../contract/fixtures/meeting01/meeting-data.draft.json")).unwrap().meeting,
            serde_json::from_str::<MeetingData>(include_str!("../../../contract/fixtures/meeting01/meeting-data.draft.json")).unwrap().recording,
            ("x".into(), VersionKind::Draft),
            &BTreeMap::new(),
        );
        (md.meeting, md.recording)
    }

    /// The safety net: a draft that does not validate with its speakers is stored without them, with a warning.
    #[test]
    fn a_draft_that_fails_with_diarization_is_stored_without_it() {
        let ((channels, plain), (m, r)) = (diarized_and_plain(), meeting_and_recording());
        let create = |md: MeetingData| -> Result<MeetingData> {
            if md.speakers.iter().any(|s| s.basis == Basis::Diarization) {
                return Err(Error::Invalid("I-something".into()));
            }
            Ok(md)
        };
        let mut warnings = Vec::new();
        let md = store_draft(&create, m.clone(), r.clone(), &channels, &plain, &mut warnings).unwrap();
        assert!(md.speakers.iter().all(|s| s.basis == Basis::Channel) && md.words.len() == 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("speaker separation was dropped"));
        // without diarization in the draft a failure is the store's failure, not hidden
        let mut w = Vec::new();
        assert!(store_draft(&|_| Err(Error::Invalid("x".into())), m, r, &plain, &plain, &mut w).is_err());
        assert!(w.is_empty());
    }

    fn result(words: Value) -> Value {
        json!({"contract": "0.1", "kind": "asr.batch", "engine": {"id": "e", "version": "1"}, "network": false,
               "coverage": [{"start_ms": 0, "end_ms": 1010}], "words": words})
    }

    /// Capture coverage ends at floor(frames*1000/16000); engines pad word ends past it.
    #[test]
    fn the_last_word_is_clamped_to_coverage_not_dropped() {
        let w = |s: i64, e: i64, t: &str| json!({"start_ms": s, "end_ms": e, "text": t, "end_estimated": true});
        let r = result(json!([w(300, 400, "mid"), w(900, 1010, "last"), w(1000, 1010, "tail"), w(1005, 1010, "outside")]));
        let cap = [Span { start_ms: 0, end_ms: 1004 }];
        let c = batch_channel(&r, 0, "mic", None, Some(&cap)).unwrap();
        let got: Vec<_> = c.words.iter().map(|w| (w.text.as_str(), w.start_ms, w.end_ms)).collect();
        assert_eq!(got, [("mid", 300, 400), ("last", 900, 1004), ("tail", 1000, 1004)]);
        assert_eq!(c.producers[0]["dropped_words_outside_coverage"], 1, "only the word that starts outside is dropped");
        assert!(c.words.iter().all(|w| c.coverage.iter().any(|s| s.start_ms <= w.start_ms && w.end_ms <= s.end_ms)), "I5 holds");
    }

    // ---------------------------------------------------------------- draft from live

    use crate::store::{ImportChannel, ImportOptions};

    fn wav(path: &std::path::Path, frames: u32) {
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        (0..frames).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
        w.finalize().unwrap();
    }

    /// An imported two-channel meeting (4375 ms) with a live version shaped like `store_live`'s.
    fn with_live() -> (tempfile::TempDir, Store, String, String) {
        with_live_on(&[0, 1])
    }

    /// `with_live` where only the channels in `on` had a live engine (as `store_live` leaves out the others).
    fn with_live_on(on: &[u32]) -> (tempfile::TempDir, Store, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
        wav(&a, 70_000);
        wav(&b, 70_000);
        let s = Store::open(&tmp.path().join("data")).unwrap();
        let ch = |index, role: &str, label: &str, path: &std::path::Path| ImportChannel { index, role: role.into(), label: Some(label.into()), path: path.into(), expected_sha256: None, coverage: None };
        let m = s.import_meeting(vec![ch(0, "mic", "나", &a), ch(1, "loopback", "상대", &b)], ImportOptions::default()).unwrap().meeting.id;
        let (meeting, recording, _) = s.meeting_and_recording(&m).unwrap();
        let w = |s: i64, e: i64, t: &str| EngineWord { start_ms: s, end_ms: e, text: t.into(), p: None, end_estimated: true };
        let asr = |c: u32| json!({"role": "asr", "engine": {"id": "nemotron-stream", "version": "0.1.0"}, "channels": [c], "network": false,
                                  "word_timing": "transducer", "granularity": "token", "params": {"chunk_ms": 100}});
        let cov = vec![Span { start_ms: 0, end_ms: 4375 }];
        let mut mic = ChannelInput::new("mic", vec![w(100, 400, "안녕하세요"), w(400, 900, "리뷰")], cov.clone(), vec![asr(0)]);
        mic.label = Some("나".into());
        let mut lb = ChannelInput::new("loopback", vec![w(1000, 1500, "네"), w(3000, 3500, "좋아요")], cov, vec![asr(1)]);
        lb.label = Some("상대".into());
        let mut inputs = BTreeMap::from([(0, mic), (1, lb)]);
        inputs.retain(|c, _| on.contains(c));
        let md = compose(meeting, recording, ("pending".into(), VersionKind::Live), &inputs);
        let live = s.create_version(md).unwrap().version.id;
        (tmp, s, m, live)
    }

    fn no_progress(_: Progress) {}

    #[test]
    fn a_live_version_becomes_the_first_draft_once() {
        let (_tmp, s, m, live) = with_live();
        let no = AtomicBool::new(false);
        let d = draft_from_live(&s, &live, None, &no_progress, &no).unwrap();
        assert!(d.warnings.is_empty());
        assert_eq!((d.data.version.kind, d.data.version.parent.as_deref()), (VersionKind::Draft, Some(live.as_str())));
        let lv = s.version(&live).unwrap();
        assert_eq!(d.data.version.producers, lv.version.producers, "the live engine is the producer, network flag included");
        assert!(d.data.version.producers.iter().all(|p| p["network"] == false && p["word_timing"] == "transducer"));
        assert_eq!(s.version_row(&d.data.version.id).unwrap().content_sha256, s.version_row(&live).unwrap().content_sha256, "content is the live content");
        // again: the same draft, nothing stored
        assert_eq!(draft_from_live(&s, &live, None, &no_progress, &no).unwrap().data.version.id, d.data.version.id);
        assert_eq!(s.create_draft_from(&live, lv.clone()).unwrap().version.id, d.data.version.id);
        assert_eq!(s.versions_of(&m).unwrap().len(), 2);
        // and it is a draft like any other: it can be approved
        assert_eq!(s.approve(&d.data.version.id).unwrap().version.kind, VersionKind::Approved);
    }

    #[test]
    fn a_meeting_with_a_draft_is_not_given_one_from_live() {
        let (_tmp, s, m, live) = with_live();
        let mut batch = s.version(&live).unwrap();
        batch.version.kind = VersionKind::Draft;
        batch.words[0].text = "배치".into();
        s.create_version(batch).unwrap();
        let e = draft_from_live(&s, &live, None, &no_progress, &AtomicBool::new(false)).err().unwrap();
        assert!(matches!(e, Error::StaleDraft(_)), "{e:?}");
        assert!(matches!(s.create_draft_from(&live, s.version(&live).unwrap()), Err(Error::StaleDraft(_))));
        assert_eq!(s.versions_of(&m).unwrap().len(), 2);
        // only a live version is a source
        let draft = s.versions_of(&m).unwrap()[1].id.clone();
        assert!(matches!(draft_from_live(&s, &draft, None, &no_progress, &AtomicBool::new(false)), Err(Error::Invalid(_))));
    }

    struct FakeDiar(bool);
    impl DiarEngine for FakeDiar {
        fn diarize(&self, req: &DiarRequest, _cancel: &AtomicBool) -> Result<Value> {
            if !self.0 {
                return Err(Error::Engine("boom".into()));
            }
            Ok(two_speakers(req))
        }
        fn min_speaker_ms(&self) -> i64 {
            0
        }
    }

    /// Answers, and the user cancels while it runs (an engine that does not watch the flag itself).
    struct CancelledDuring;
    impl DiarEngine for CancelledDuring {
        fn diarize(&self, req: &DiarRequest, cancel: &AtomicBool) -> Result<Value> {
            cancel.store(true, Ordering::SeqCst);
            Ok(two_speakers(req))
        }
        fn min_speaker_ms(&self) -> i64 {
            0
        }
    }

    fn two_speakers(req: &DiarRequest) -> Value {
        let t = |s: i64, e: i64, sp: &str| json!({"start_ms": s, "end_ms": e, "speaker": sp});
        json!({"contract": "0.1", "kind": "diarization", "engine": {"id": "fake-diar", "version": "1"}, "network": false,
               "audio_sha256": req.audio_sha256, "turns": [t(900, 2000, "a"), t(2900, 4000, "b")]})
    }

    /// `store_live` leaves out a channel whose engine never ran: no phantom speaker, nothing renumbered.
    #[test]
    fn a_live_version_with_one_of_two_channels_round_trips() {
        let (_tmp, s, _m, live) = with_live_on(&[1]);
        let md = s.version(&live).unwrap();
        let ch = channels_of(&md).unwrap();
        assert_eq!(ch.keys().copied().collect::<Vec<_>>(), [1]);
        let again = compose(md.meeting.clone(), md.recording.clone(), (md.version.id.clone(), md.version.kind), &ch);
        assert_eq!((&again.speakers, &again.words, &again.turns, &again.coverage), (&md.speakers, &md.words, &md.turns, &md.coverage));
        assert_eq!(again.version.producers, md.version.producers);
    }

    #[test]
    fn diarization_runs_only_on_the_channels_the_live_version_has() {
        let (_tmp, s, _m, live) = with_live_on(&[1]);
        let seen = std::cell::RefCell::new(Vec::new());
        let progress = |p: Progress| seen.borrow_mut().push((p.channel, p.channels, p.percent));
        let md = draft_from_live(&s, &live, Some(&FakeDiar(true)), &progress, &AtomicBool::new(false)).unwrap().data;
        assert_eq!(*seen.borrow(), [(1, 2, 0), (1, 2, 100)], "only channel 1 is separated, named as such");
        assert!(md.speakers.iter().all(|x| x.channel == 1 && x.basis == Basis::Diarization), "{:?}", md.speakers);
        assert_eq!(md.speakers.len(), 2);
        assert!(md.speaker_spans.iter().all(|x| x.channel == 1));
        assert!(md.version.producers.iter().all(|p| p["channels"] == json!([1])), "{:?}", md.version.producers);
        assert!(md.coverage.iter().all(|c| c.channel == 1));
    }

    #[test]
    fn a_producer_naming_a_channel_the_meeting_lacks_is_invalid() {
        let (_tmp, s, _m, live) = with_live();
        let mut md = s.version(&live).unwrap();
        md.version.producers[0]["channels"] = json!([7]);
        assert!(matches!(channels_of(&md), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_cancel_during_the_last_separation_still_cancels() {
        let (_tmp, s, m, live) = with_live();
        let r = draft_from_live(&s, &live, Some(&CancelledDuring), &no_progress, &AtomicBool::new(false));
        assert!(matches!(r, Err(Error::Cancelled)), "{:?}", r.map(|t| t.data.version.id));
        assert_eq!(s.versions_of(&m).unwrap().len(), 1, "nothing stored");
    }

    #[test]
    fn diarization_applies_to_a_draft_from_live_like_to_a_batch_draft() {
        let (_tmp, s, _m, live) = with_live();
        let seen = std::cell::RefCell::new(0);
        let progress = |p: Progress| {
            assert!(p.diarizing);
            *seen.borrow_mut() += 1;
        };
        let d = draft_from_live(&s, &live, Some(&FakeDiar(true)), &progress, &AtomicBool::new(false)).unwrap();
        assert_eq!(*seen.borrow(), 4, "0 and 100 per channel");
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
        let md = d.data;
        assert_eq!(md.version.parent.as_deref(), Some(live.as_str()));
        let labels: Vec<_> = md.speakers.iter().filter(|x| x.channel == 1).map(|x| (x.basis, x.label.clone().unwrap())).collect();
        assert_eq!(labels, [(Basis::Diarization, "상대 1".to_string()), (Basis::Diarization, "상대 2".to_string())]);
        assert!(md.version.producers.iter().any(|p| p["role"] == "diarization" && p["engine"]["id"] == "fake-diar"));
        assert_eq!(md.version.producers.iter().filter(|p| p["role"] == "asr" && p["engine"]["id"] == "nemotron-stream").count(), 2);
        assert_eq!(md.words.len(), 4, "the live words, all of them");
    }

    #[test]
    fn a_failed_diarization_leaves_the_live_content_and_warns() {
        let (_tmp, s, _m, live) = with_live();
        let d = draft_from_live(&s, &live, Some(&FakeDiar(false)), &no_progress, &AtomicBool::new(false)).unwrap();
        assert_eq!(d.warnings.len(), 2);
        assert_eq!(s.version_row(&d.data.version.id).unwrap().content_sha256, s.version_row(&live).unwrap().content_sha256);
        let again = draft_from_live(&s, &live, Some(&FakeDiar(true)), &no_progress, &AtomicBool::new(true)).unwrap();
        assert_eq!(again.data.version.id, d.data.version.id, "the draft exists already: returned without running anything");
    }

    #[test]
    fn a_cancel_stops_before_anything_is_stored() {
        let (_tmp, s, m, live) = with_live();
        assert!(matches!(draft_from_live(&s, &live, Some(&FakeDiar(true)), &no_progress, &AtomicBool::new(true)), Err(Error::Cancelled)));
        assert_eq!(s.versions_of(&m).unwrap().len(), 1);
    }

    /// Reading the channels back and composing them again gives the same words, turns and coverage.
    #[test]
    fn channels_of_round_trips_through_compose() {
        let md: MeetingData = serde_json::from_str(include_str!("../../../contract/fixtures/meeting01/meeting-data.live.json")).unwrap();
        let again = compose(md.meeting.clone(), md.recording.clone(), (md.version.id.clone(), md.version.kind), &channels_of(&md).unwrap());
        assert_eq!(again.words, md.words);
        assert_eq!(again.turns, md.turns);
        assert_eq!(again.coverage, md.coverage);
        assert_eq!(again.speakers, md.speakers);
        let asr: Vec<_> = md.version.producers.iter().filter(|p| p["role"] != "diarization").cloned().collect();
        assert_eq!(again.version.producers, asr, "speaker spans and their producers are left to the caller's diarization");
    }
}
