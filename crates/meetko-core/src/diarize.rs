//! Batch speaker diarization: who spoke when on one channel of a finished recording.
//!
//! Like the other engines it is a separate native process (role `diarization`, NDJSON JSON-RPC
//! over stdio, `engines/nemotron-stream` builds the one that exists) and it only attaches by the
//! contract: a request names a staged wav, the reply is an engine result of kind `diarization`
//! (turns in integer ms, engine-local speaker labels). `pipeline::transcribe` feeds those turns to
//! compose, which assigns the words and records the producer.
//!
//! The model is native-parser input, so it is handled like the streaming model (`live.rs`): copied
//! into a private directory while it is hashed, checked against the pin in `models.toml`, and only
//! that copy is loaded. One process serves all channels of a run (the model loads once) and ends
//! with the `DiarProcess`. Nothing here is required: no configuration, no diarization, and any
//! failure is the caller's to turn into a warning.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::contract::{engine_result_validator, schema_errors, Diarization, DiarTurn};
use crate::error::{Error, Result};
use crate::live::{self, EngineClient, SharedModel, ROLE_DIARIZATION};
use crate::util;

/// The binary the nemotron-stream crate builds next to its streaming engine.
const ENGINE_DIR: &str = "nemotron-stream";
const ENGINE_BIN: &str = "meetko-nemotron-diar";

/// Time budget of a request: this, plus the length of the audio (a recording is diarized several
/// times faster than real time on the machines measured; the first request also loads the model).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// An engine result with more turns than this is not a diarization of a meeting.
pub const MAX_TURNS: usize = 200_000;

pub struct DiarRequest<'a> {
    /// the file the engine reads; the caller has hashed exactly this file
    pub wav: &'a Path,
    pub audio_sha256: &'a str,
    pub channel: u32,
    pub duration_ms: i64,
}

/// A diarization engine: one mono channel file in, one `diarization` engine result out.
pub trait DiarEngine {
    fn diarize(&self, req: &DiarRequest, cancel: &AtomicBool) -> Result<Value>;

    /// Speakers with less talk time than this on a channel are merged into the others (`merge_small_speakers`).
    fn min_speaker_ms(&self) -> i64 {
        min_speaker_ms()
    }
}

#[derive(Debug, Clone)]
pub struct DiarConfig {
    pub engine_bin: PathBuf,
    pub model_dir: PathBuf,
    /// accept a model that is not in `models.toml` (development only; recorded in provenance)
    pub unpinned: bool,
    /// added to the audio length to make the time budget of a request
    pub timeout: Duration,
    /// see `MIN_SPEAKER_MS`
    pub min_speaker_ms: i64,
}

impl DiarConfig {
    /// The real environment: `MEETKO_DIAR_MODEL` (the model directory; unset = the one the app keeps
    /// in `<data dir>/models/` when there is one, else no diarization and the answer is `None`; the
    /// app's own model without an engine is also `None`, and `*_UNPINNED` never applies to it),
    /// `MEETKO_DIAR_ENGINE` (the binary; else a sibling of the executable, else
    /// the dev build), `MEETKO_DIAR_MODEL_UNPINNED=1` (development) and `MEETKO_DIAR_TIMEOUT_SECS`.
    /// A model that is set but unusable is an error, not a silent fallback: the caller decides
    /// whether the transcript goes on without it.
    pub fn from_env() -> Result<Option<DiarConfig>> {
        let env = std::env::var_os("MEETKO_DIAR_MODEL").map(PathBuf::from);
        let Some((model_dir, from_env)) = live::model_dir_for(ROLE_DIARIZATION, env, &util::data_dir()) else { return Ok(None) };
        let engine_bin = match find_engine() {
            Ok(b) => b,
            // the app's own model without an engine (a build that has none): no diarization, quietly.
            // A model the environment names is a request, so its missing engine is an error.
            Err(_) if !from_env => return Ok(None),
            Err(e) => return Err(e),
        };
        let unpinned = from_env && std::env::var("MEETKO_DIAR_MODEL_UNPINNED").is_ok_and(|v| v == "1");
        live::check_model_for(ROLE_DIARIZATION, &model_dir, unpinned, &util::data_dir().join("model-sha256-cache.json"))?;
        let timeout = std::env::var("MEETKO_DIAR_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).map_or(DEFAULT_TIMEOUT, Duration::from_secs);
        Ok(Some(DiarConfig { engine_bin, model_dir, unpinned, timeout, min_speaker_ms: min_speaker_ms() }))
    }
}

/// The diarization engine's binary with the real environment: `MEETKO_DIAR_ENGINE`, else a sibling
/// of the executable, else (debug builds) the dev build. Never from a data-dir file.
pub fn find_engine() -> Result<PathBuf> {
    let explicit = std::env::var_os("MEETKO_DIAR_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    live::pick_bin(
        explicit,
        "MEETKO_DIAR_ENGINE",
        "the diarization engine",
        exe.as_deref().and_then(Path::parent),
        std::env::current_dir().ok().as_deref(),
        ENGINE_DIR,
        &live::exe_name(ENGINE_BIN),
    )
}

/// The engine process behind `DiarEngine`, started on the first request and kept for the next
/// channel. A process that failed is dropped (killed with its group); the next request starts a
/// fresh one. The staged model lives as long as this value.
pub struct DiarProcess {
    cfg: DiarConfig,
    model: SharedModel,
    client: Mutex<Option<EngineClient>>,
}

impl DiarProcess {
    pub fn new(cfg: DiarConfig) -> DiarProcess {
        DiarProcess { cfg, model: SharedModel::new(ROLE_DIARIZATION), client: Mutex::new(None) }
    }
}

impl DiarEngine for DiarProcess {
    fn min_speaker_ms(&self) -> i64 {
        self.cfg.min_speaker_ms
    }

    fn diarize(&self, req: &DiarRequest, cancel: &AtomicBool) -> Result<Value> {
        let abort = || cancel.load(Ordering::SeqCst);
        let staged = self.model.get(&self.cfg.model_dir, self.cfg.unpinned, &abort)?;
        let mut slot = self.client.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            // no stream events exist in this protocol; the queue holds the one request
            *slot = Some(EngineClient::spawn_bin(&self.cfg.engine_bin, ("MEETKO_DIAR_MODEL", &staged.path), ROLE_DIARIZATION, 4, |_| {}, &abort)?);
        }
        let client = slot.as_mut().expect("started above");
        let budget = self.cfg.timeout + Duration::from_millis(req.duration_ms.max(0) as u64);
        let wav = req.wav.to_str().ok_or_else(|| Error::Engine("the wav path is not UTF-8".into()))?;
        let params = json!({ "wav": wav, "audio_sha256": req.audio_sha256 });
        let mut res = match client.call_with("diarize.run", params, budget, &abort) {
            Ok(r) => r,
            Err(e) => {
                // the engine's own words when it died with a message (logged, never a path)
                let detail = client.fault().or_else(|| matches!(e, Error::Engine(_)).then(|| client.stderr_tail()));
                *slot = None; // killed on drop; the next channel starts a new one
                if let Some(d) = detail.filter(|d| !d.trim().is_empty()) {
                    eprintln!("diarization engine: {}", d.trim());
                }
                return Err(e);
            }
        };
        if !res.is_object() {
            *slot = None; // an engine that answers like this is not trusted with the next channel
            return Err(Error::BadEngineOutput("diarization result is not an object".into()));
        }
        // what the core staged is recorded whatever the engine says about itself
        if !res["model"].is_object() {
            res["model"] = json!({ "id": client.info["model"]["id"].as_str().filter(|i| !i.is_empty()).unwrap_or("unknown") });
        }
        res["model"]["sha256"] = json!(staged.check.digest);
        if !res["params"].is_object() {
            res["params"] = json!({});
        }
        res["params"]["model_pinned"] = json!(staged.check.pinned);
        Ok(res)
    }
}

impl Drop for DiarProcess {
    fn drop(&mut self) {
        // a polite end first; EngineClient's drop takes the process group either way
        if let Some(c) = self.client.get_mut().unwrap_or_else(|p| p.into_inner()).take() {
            c.shutdown(&|| false);
        }
    }
}

/// An engine result as the part of it compose reads, after the checks that make it safe to use:
/// the schema, the audio it says it read, and turns inside the recording (`duration_ms`, the
/// recording's, which may be shorter than the channel file). A turn that ends before it starts or
/// starts after the recording is a broken result and rejects it whole. One that merely runs past the
/// end (engines round and pad) is clamped to it, as compose clips words and spans, and a turn left
/// empty is dropped.
pub fn checked(result: &Value, audio_sha256: &str, duration_ms: i64) -> Result<Diarization> {
    let errs = schema_errors(engine_result_validator(), result);
    if !errs.is_empty() || result["kind"] != "diarization" {
        return Err(Error::BadEngineOutput(format!("diarization result: {}", errs.into_iter().take(5).collect::<Vec<_>>().join("; "))));
    }
    if result["audio_sha256"] != audio_sha256 {
        return Err(Error::BadEngineOutput("diarization result is for different audio".into()));
    }
    let mut turns: Vec<DiarTurn> = serde_json::from_value(result["turns"].clone())?;
    if turns.len() > MAX_TURNS {
        return Err(Error::BadEngineOutput(format!("diarization result has {} turns", turns.len())));
    }
    if turns.iter().any(|t| t.start_ms < 0 || t.end_ms < t.start_ms || t.start_ms > duration_ms) {
        return Err(Error::BadEngineOutput("diarization turn outside the recording".into()));
    }
    for t in &mut turns {
        t.end_ms = t.end_ms.min(duration_ms);
    }
    turns.retain(|t| t.end_ms > t.start_ms);
    // speakers are numbered in order of first speech: not by the order the engine listed them
    turns.sort_by(|a, b| (a.start_ms, a.end_ms, &a.speaker).cmp(&(b.start_ms, b.end_ms, &b.speaker)));
    Ok(Diarization { turns })
}

/// Speakers with less talk time than this on a channel are not told apart from the others: on 30
/// AIHub meetings this removed nearly all spurious extra speakers (single-speaker stretches of a
/// minute or more with an extra speaker over 2 %: 21 of 52 became 2) for 0.2 % of true talk time.
/// `MEETKO_DIAR_MIN_SPEAKER_SECS` overrides it (0 turns the rule off).
pub const MIN_SPEAKER_MS: i64 = 10_000;

pub fn min_speaker_ms() -> i64 {
    std::env::var("MEETKO_DIAR_MIN_SPEAKER_SECS").ok().and_then(|v| v.parse::<i64>().ok()).filter(|s| *s >= 0).map_or(MIN_SPEAKER_MS, |s| s * 1000)
}

fn gap(a: &DiarTurn, b: &DiarTurn) -> i64 {
    (a.start_ms.max(b.start_ms) - a.end_ms.min(b.end_ms)).max(0)
}

/// Merge the speakers of one channel whose turns add up to less than `min_ms` into the others:
/// each of their turns goes to the kept speaker it overlaps most, else the one nearest in time, so
/// the words stay attributed. If nobody reaches `min_ms` the one with most talk time is kept (a
/// short channel is one speaker, not none). Returns how many speakers were merged away.
pub fn merge_small_speakers(d: &mut Diarization, min_ms: i64) -> usize {
    let mut total: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for t in &d.turns {
        *total.entry(t.speaker.clone()).or_default() += t.end_ms - t.start_ms;
    }
    let mut kept: Vec<String> = total.iter().filter(|(_, ms)| **ms >= min_ms).map(|(s, _)| s.clone()).collect();
    if kept.is_empty() {
        kept.extend(total.iter().max_by_key(|(s, ms)| (**ms, std::cmp::Reverse((*s).clone()))).map(|(s, _)| s.clone()));
    }
    let moves: Vec<(usize, String)> = d
        .turns
        .iter()
        .enumerate()
        .filter(|(_, t)| !kept.contains(&t.speaker))
        .filter_map(|(i, t)| {
            let by = |f: &dyn Fn(&DiarTurn) -> i64| {
                let mut score: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
                for k in d.turns.iter().filter(|k| kept.contains(&k.speaker)) {
                    *score.entry(k.speaker.as_str()).or_default() += f(k);
                }
                score.into_iter().filter(|(_, v)| *v > 0).max_by_key(|(s, v)| (*v, std::cmp::Reverse(s.to_string()))).map(|(s, _)| s.to_string())
            };
            let overlap = by(&|k| (t.end_ms.min(k.end_ms) - t.start_ms.max(k.start_ms)).max(0));
            // nearest in time: the kept turn with the smallest gap (the turns are few per speaker, the scan is linear)
            let nearest = || d.turns.iter().filter(|k| kept.contains(&k.speaker)).min_by_key(|k| (gap(t, k), k.start_ms)).map(|k| k.speaker.clone());
            overlap.or_else(nearest).map(|s| (i, s))
        })
        .collect();
    let merged = total.len() - kept.len();
    for (i, s) in moves {
        d.turns[i].speaker = s;
    }
    merged
}

/// How many distinct speakers the turns name.
pub fn speaker_count(d: &Diarization) -> usize {
    d.turns.iter().map(|t| t.speaker.as_str()).collect::<std::collections::BTreeSet<_>>().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34";

    fn result(turns: Value) -> Value {
        json!({"contract": "0.1", "kind": "diarization", "engine": {"id": "e", "version": "1"}, "audio_sha256": SHA, "turns": turns})
    }

    fn turn(a: i64, b: i64, s: &str) -> Value {
        json!({"start_ms": a, "end_ms": b, "speaker": s})
    }

    fn d(turns: &[(i64, i64, &str)]) -> Diarization {
        Diarization { turns: turns.iter().map(|(a, b, s)| DiarTurn { start_ms: *a, end_ms: *b, speaker: s.to_string() }).collect() }
    }

    fn who(d: &Diarization) -> Vec<&str> {
        d.turns.iter().map(|t| t.speaker.as_str()).collect()
    }

    #[test]
    fn turns_are_held_to_the_recording() {
        // past the end: clamped; left empty by that: dropped
        let ok = checked(&result(json!([turn(0, 900, "a"), turn(1000, 1200, "a")])), SHA, 1000).unwrap();
        assert_eq!(ok.turns.iter().map(|t| (t.start_ms, t.end_ms)).collect::<Vec<_>>(), [(0, 900)]);
        let clamped = checked(&result(json!([turn(500, 5000, "a")])), SHA, 1000).unwrap();
        assert_eq!((clamped.turns[0].start_ms, clamped.turns[0].end_ms), (500, 1000));
        // broken: ends before it starts, starts after the recording
        for bad in [turn(500, 400, "a"), turn(1001, 1200, "a")] {
            assert!(matches!(checked(&result(json!([bad])), SHA, 1000), Err(Error::BadEngineOutput(_))));
        }
        // out of order in, in order out
        let sorted = checked(&result(json!([turn(500, 600, "b"), turn(0, 100, "a")])), SHA, 1000).unwrap();
        assert_eq!(sorted.turns.iter().map(|t| t.speaker.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // other audio
        assert!(checked(&result(json!([])), &"0".repeat(64), 1000).is_err());
    }

    #[test]
    fn small_speakers_go_to_the_one_they_overlap_or_the_nearest() {
        let mut x = d(&[(0, 30_000, "a"), (30_000, 50_000, "b"), (10_000, 12_000, "c"), (60_000, 62_000, "d")]);
        assert_eq!(merge_small_speakers(&mut x, 10_000), 2);
        // c overlaps a; d overlaps nobody and is nearest to b (ends at 50 s)
        assert_eq!(who(&x), ["a", "b", "a", "b"]);
    }

    #[test]
    fn a_channel_where_nobody_is_big_enough_keeps_its_biggest() {
        let mut x = d(&[(0, 4000, "a"), (4000, 9000, "b"), (9000, 9500, "c")]);
        assert_eq!(merge_small_speakers(&mut x, 10_000), 2);
        assert_eq!(who(&x), ["b", "b", "b"]);
        assert_eq!(speaker_count(&x), 1);
        let mut none = d(&[]);
        assert_eq!(merge_small_speakers(&mut none, 10_000), 0);
        let mut off = d(&[(0, 100, "a"), (200, 300, "b")]);
        assert_eq!(merge_small_speakers(&mut off, 0), 0, "0 turns the rule off");
        assert_eq!(speaker_count(&off), 2);
    }
}
