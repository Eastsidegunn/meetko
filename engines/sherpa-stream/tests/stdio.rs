//! Fatal paths of the real binary over stdio. Needs the sherpa model, so without it the test passes
//! after saying so, unless MEETKO_REQUIRE_MODELS=1 (then it fails). Model directory:
//! MEETKO_STREAM_MODEL, else the repository's gitignored models/ directory.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn model() -> Option<PathBuf> {
    let dir = std::env::var_os("MEETKO_STREAM_MODEL")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/sherpa-onnx-streaming-zipformer-korean-2024-06-16"));
    if dir.is_dir() {
        return Some(dir);
    }
    assert!(std::env::var_os("MEETKO_REQUIRE_MODELS").is_none_or(|v| v != "1"), "the model is missing, but MEETKO_REQUIRE_MODELS=1");
    eprintln!("skipped: no sherpa model directory");
    None
}

/// Send `lines`, then return the engine's stdout lines and exit code.
fn run(model: &PathBuf, lines: &[&str]) -> (Vec<serde_json::Value>, Option<i32>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meetko-sherpa-stream"))
        .env("MEETKO_STREAM_MODEL", model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for l in lines {
        if writeln!(stdin, "{l}").and_then(|_| stdin.flush()).is_err() {
            break; // the engine is already gone
        }
    }
    drop(stdin); // EOF: the engine ends once it has answered
    let out: Vec<serde_json::Value> = BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    (out, child.wait().unwrap().code())
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"contract":["0.1"]}}"#;
const OPEN: &str = r#"{"jsonrpc":"2.0","id":2,"method":"stream.open","params":{"sample_rate":16000,"language":"ko"}}"#;

#[test]
fn a_line_that_is_not_json_is_fatal_only_while_a_stream_is_open() {
    let Some(m) = model() else { return };
    // no stream: ignored, the engine goes on and answers the next request, then ends on stdin EOF
    let (out, code) = run(&m, &[INIT, "not json", OPEN]);
    assert_eq!(code, Some(0));
    assert!(out.iter().all(|v| v["method"] != "stream.error") && out.iter().any(|v| v["id"] == 2));
    // stream open: one stream.error and exit code 1
    let (out, code) = run(&m, &[INIT, OPEN, r#"{"jsonrpc":"2.0","method":"stream.pu"#]);
    assert_eq!(code, Some(1));
    assert_eq!(out.iter().filter(|v| v["method"] == "stream.error").count(), 1, "{out:?}");
}
