//! whisper.cpp adapter. The real-output check needs `raw/` (local only) and skips without it.

use std::path::PathBuf;

use meetko_core::whisper::adapt;
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(p: PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn adapted_words_equal_fixture_when_raw_is_present() {
    let mut ran = 0;
    for ch in ["mic", "loopback"] {
        let raw_path = root().join(format!("raw/engines/whisper-cpp/{ch}.json"));
        if !raw_path.exists() {
            eprintln!("SKIP {ch}: raw/ is local-only and absent");
            continue;
        }
        let fixture = read(root().join(format!("contract/fixtures/meeting01/engine.whisper-cpp.{ch}.json")));
        let got = adapt(
            &read(raw_path),
            fixture["audio_sha256"].as_str().unwrap(),
            &fixture["params"],
            fixture["engine"]["version"].as_str().unwrap(),
            fixture["model"]["id"].as_str().unwrap(),
            fixture["model"]["sha256"].as_str(),
        )
        .unwrap();
        assert_eq!(got["words"], fixture["words"], "{ch}");
        assert_eq!(got, fixture, "{ch}");
        ran += 1;
    }
    eprintln!("compared {ran} channel(s) against raw/");
}

fn tok(text: &str, dtw: i64, from: i64) -> Value {
    json!({"text": text, "p": 0.5, "t_dtw": dtw, "offsets": {"from": from, "to": from + 100}})
}

fn raw(tokens: Vec<Value>, seg_end: i64) -> Value {
    json!({"result": {"language": "ko"}, "transcription": [{"offsets": {"from": 0, "to": seg_end}, "tokens": tokens}]})
}

fn run(raw: &Value) -> Value {
    adapt(raw, &"0".repeat(64), &json!({}), "t", "m", None).unwrap()
}

#[test]
fn subwords_join_special_tokens_drop_and_dtw_is_monotonic() {
    let r = run(&raw(
        vec![tok("[_BEG_]", -1, 0), tok(" 올라", 100, 0), tok("오면", 120, 0), tok(" 다음", 90, 0), tok("[_TT_141]", -1, 0)],
        3000,
    ));
    assert_eq!(r["word_timing"], "dtw");
    let w = r["words"].as_array().unwrap();
    assert_eq!(w.len(), 2);
    assert_eq!(w[0]["text"], "올라오면");
    // t_dtw 90 (900 ms) is before the previous word's 1000 ms: clamped
    assert_eq!(w[1]["start_ms"], 1000);
    // the next raw start (900) is not after this start, so the segment end is used (as the reference does)
    assert_eq!(w[0]["end_ms"], 3000);
    assert_eq!(w[1]["end_ms"], 3000);
    assert_eq!(w[1]["end_estimated"], true);
}

#[test]
fn any_missing_dtw_downgrades_to_token_timing() {
    let r = run(&raw(vec![tok(" a", 50, 0), tok(" b", -1, 700)], 1000));
    assert_eq!(r["word_timing"], "token");
    assert_eq!(r["words"][1]["start_ms"], 700);
}
