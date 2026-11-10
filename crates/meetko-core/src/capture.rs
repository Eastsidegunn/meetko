//! capture.json (docs/capture-interface.md): validate a capture provider's manifest and
//! turn it into an import. The manifest *is* the alignment statement; nothing is guessed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

use crate::contract::{schema_errors, Span};
use crate::error::{Error, Result};
use crate::store::{ImportChannel, ImportOptions};

const CAPTURE_SCHEMA: &str = include_str!("../../../capture/macos/capture.schema.json");

pub fn capture_validator() -> &'static Validator {
    static V: OnceLock<Validator> = OnceLock::new();
    V.get_or_init(|| {
        let schema: Value = serde_json::from_str(CAPTURE_SCHEMA).expect("embedded schema is JSON");
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .should_validate_formats(true)
            .build(&schema)
            .expect("embedded schema compiles")
    })
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Capture(msg.into())
}

/// Gap reasons that zero-fill audio at its place; these may not overlap each other.
fn zero_filled(reason: &str) -> bool {
    matches!(reason, "late_start" | "end_pad" | "idle" | "no_data" | "dropout" | "restart" | "conversion_failed" | "write_failed")
}

/// `[0, duration]` minus the `loss` and `trim` spans: those are audio that is missing or was
/// cut (a trim is a 1 ms point marker). `fill` and `no_data` are silence that is plausibly
/// real, so they stay covered. Literal per docs/capture-interface.md; `pipeline::batch_channel`
/// bridges the 1 ms marker holes before it uses this, so speech across a marker survives.
pub fn coverage_from_gaps(duration_ms: i64, gaps: &[(i64, i64, String)]) -> Vec<Span> {
    let mut holes: Vec<(i64, i64)> = gaps
        .iter()
        .filter(|(s, e, kind)| (kind == "loss" || kind == "trim") && e > s)
        .map(|(s, e, _)| (*s, *e))
        .collect();
    holes.sort();
    let (mut out, mut at) = (Vec::new(), 0);
    for (s, e) in holes {
        if s > at {
            out.push(Span { start_ms: at, end_ms: s });
        }
        at = at.max(e);
    }
    if at < duration_ms {
        out.push(Span { start_ms: at, end_ms: duration_ms });
    }
    out
}

pub struct CaptureImport {
    pub channels: Vec<ImportChannel>,
    pub options: ImportOptions,
    pub warnings: Vec<String>,
}

/// `path` is a capture directory or the `capture.json` inside one.
pub fn load(path: &Path, allow_incomplete: bool) -> Result<CaptureImport> {
    let (dir, manifest_path) = if path.is_dir() { (path.to_path_buf(), path.join("capture.json")) } else {
        (path.parent().map(Path::to_path_buf).unwrap_or_default(), path.to_path_buf())
    };
    let doc: Value = serde_json::from_slice(&std::fs::read(&manifest_path).map_err(|e| bad(format!("cannot read capture.json: {e}")))?)
        .map_err(|e| bad(format!("capture.json is not JSON: {e}")))?;
    let errs = schema_errors(capture_validator(), &doc);
    if !errs.is_empty() {
        return Err(bad(format!("capture.json breaks the capture schema: {}", errs.into_iter().take(5).collect::<Vec<_>>().join("; "))));
    }
    let complete = doc["complete"].as_bool().unwrap_or(false);
    let mut warnings: Vec<String> = doc["warnings"].as_array().map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect()).unwrap_or_default();
    if !complete {
        if !allow_incomplete {
            return Err(bad("the recording is unfinished (complete: false); pass --allow-incomplete to import it anyway"));
        }
        warnings.push("imported from an incomplete capture".into());
    }
    let mut duration_ms = doc["duration_ms"].as_i64().unwrap_or(0);
    let chans = doc["channels"].as_array().expect("schema: two channels");
    let frames: Vec<i64> = chans.iter().map(|c| c["frames"].as_i64().unwrap_or(-1)).collect();
    if complete {
        if frames[0] != frames[1] {
            return Err(bad(format!("channels differ in length: {} vs {} frames", frames[0], frames[1])));
        }
        if duration_ms != frames[0] * 1000 / 16000 {
            return Err(bad(format!("duration_ms {duration_ms} is not floor(frames * 1000 / 16000) for {} frames", frames[0])));
        }
    }
    let mut channels = Vec::new();
    for c in chans {
        let index = c["index"].as_u64().unwrap_or(0) as u32;
        let role = c["role"].as_str().unwrap_or("").to_string();
        let file = c["file"].as_str().unwrap_or("");
        // the schema forbids separators already; "." and ".." are the remaining non-plain names
        if file.is_empty() || file.starts_with('.') || file.contains(['/', '\\', ':']) {
            return Err(bad(format!("channel {index}: file {file:?} is not a plain name")));
        }
        let file_path: PathBuf = dir.join(file);
        let meta = std::fs::symlink_metadata(&file_path).map_err(|e| bad(format!("channel {index}: {file}: {e}")))?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(bad(format!("channel {index}: {file} is a symlink or not a regular file")));
        }
        let reader = hound::WavReader::open(&file_path).map_err(|e| bad(format!("channel {index}: {file}: {e}")))?;
        let spec = reader.spec();
        if spec.channels != 1 || spec.sample_rate != 16000 || spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
            return Err(bad(format!("channel {index}: {file} is not mono 16 kHz PCM16")));
        }
        let have = i64::from(reader.duration());
        if complete && have != frames[0] {
            return Err(bad(format!("channel {index}: WAV holds {have} frames, manifest says {}", frames[0])));
        }
        if !complete {
            // a provisional file may hold less (or more) than the manifest says: believe the file
            duration_ms = duration_ms.min(have * 1000 / 16000);
        }
        let sha = c["sha256"].as_str().unwrap_or("");
        // gaps: ordered by (start_ms, reason), inside the recording, zero-fill spans disjoint
        let mut gaps = Vec::new();
        let (mut prev, mut prev_fill_end) = ((0i64, String::new()), 0i64);
        for g in c["gaps"].as_array().cloned().unwrap_or_default() {
            let (s, e) = (g["start_ms"].as_i64().unwrap_or(-1), g["end_ms"].as_i64().unwrap_or(-1));
            let (reason, kind) = (g["reason"].as_str().unwrap_or(""), g["kind"].as_str().unwrap_or(""));
            if s < 0 || s >= e || e > duration_ms.max(doc["duration_ms"].as_i64().unwrap_or(0)) {
                return Err(bad(format!("channel {index}: gap {s}..{e} ({reason}) is empty, reversed or outside the recording")));
            }
            if (s, reason.to_string()) < prev {
                return Err(bad(format!("channel {index}: gaps are not ordered by (start_ms, reason)")));
            }
            prev = (s, reason.to_string());
            let marker = matches!(reason, "overlap_trimmed" | "discontinuity");
            let removed = g.get("removed_ms").and_then(Value::as_i64);
            if marker != removed.is_some_and(|r| r >= 1) || (!marker && g.get("removed_ms").is_some()) {
                return Err(bad(format!("channel {index}: {reason} gap at {s} ms has a wrong removed_ms")));
            }
            if marker && e != s + 1 {
                return Err(bad(format!("channel {index}: {reason} at {s} ms must be a point marker (end_ms == start_ms + 1)")));
            }
            if matches!(reason, "late_start" | "end_pad") && e - s > 1000 {
                return Err(bad(format!("channel {index}: {reason} of {} ms is longer than the 1000 ms a fill may be", e - s)));
            }
            if zero_filled(reason) {
                if s < prev_fill_end {
                    return Err(bad(format!("channel {index}: zero-fill gaps overlap at {s} ms")));
                }
                prev_fill_end = e;
            }
            gaps.push((s, e, kind.to_string()));
        }
        channels.push(ImportChannel {
            index,
            role,
            label: None,
            path: file_path,
            expected_sha256: if sha.is_empty() { None } else { Some(sha.to_string()) },
            coverage: Some(coverage_from_gaps(duration_ms, &gaps)),
        });
    }
    let recording = doc["recording_sha256"].as_str().map(String::from);
    // the store verifies each channel sha while copying and then the recording hash, which
    // it computes with the same definition (`store::recording_sha256`)
    Ok(CaptureImport {
        channels,
        options: ImportOptions {
            title: None,
            language_hint: None,
            started_at: doc["started_at"].as_str().map(String::from),
            duration_ms: Some(duration_ms),
            expected_recording_sha256: recording,
        },
        warnings,
    })
}
