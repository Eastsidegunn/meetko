//! The whisper-cli runner against fake binaries: arguments, provenance, cancel, errors.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use meetko_core::error::Error;
use meetko_core::whisper::{BatchEngine, BatchRequest, Progress, WhisperCli};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn req<'a>(wav: &'a Path, data: &'a Path) -> BatchRequest<'a> {
    BatchRequest { wav, audio_sha256: SHA, language: "ko", channel: 0, channels: 2, data_dir: data }
}

const JSON: &str = r#"{"result":{"language":"ko"},"transcription":[{"offsets":{"from":0,"to":1000},"tokens":[{"text":" 안녕","p":0.9,"t_dtw":10,"offsets":{"from":0,"to":500}}]}]}"#;

fn fake(dir: &Path, cli_body: &str) -> WhisperCli {
    let model = dir.join("ggml-fake.bin");
    std::fs::write(&model, b"weights").unwrap();
    WhisperCli { cli: script(dir, "whisper-cli", cli_body), model, model_sha256: None, dtw: "large.v3.turbo".into() }
}

#[test]
fn every_recorded_parameter_is_passed_and_the_version_is_probed() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::write(t.join("out.json"), JSON).unwrap();
    let body = format!(
        r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 7.8.9"; exit 0; fi
echo "$@" > "{0}/args.txt"
while [ $# -gt 0 ]; do if [ "$1" = "-of" ]; then out="$2"; fi; shift; done
echo "whisper_print_progress_callback: progress =  50%" >&2
cp "{0}/out.json" "$out.json""#,
        t.display()
    );
    let cli = fake(t, &body);
    let wav = t.join("input.wav");
    std::fs::write(&wav, b"x").unwrap();
    let seen = std::sync::Mutex::new(Vec::new());
    let res = cli.transcribe(&req(&wav, t), &|p: Progress| seen.lock().unwrap().push(p.percent), &AtomicBool::new(false)).unwrap();
    assert_eq!(res["engine"]["version"], "7.8.9");
    assert_eq!(res["model"]["id"], "ggml-fake");
    assert_eq!(res["model"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(seen.lock().unwrap().last(), Some(&50));
    let args = std::fs::read_to_string(t.join("args.txt")).unwrap();
    let words: Vec<&str> = args.split_whitespace().collect();
    let flag = |f: &str| words.iter().position(|w| *w == f).map(|i| words[i + 1].to_string());
    // each parameter recorded in the result was passed on the command line, with that value
    let p = &res["params"];
    assert_eq!(flag("-l").unwrap(), p["language"].as_str().unwrap());
    assert_eq!(flag("-mc").unwrap(), "0");
    assert_eq!(flag("-dtw").unwrap(), p["dtw"].as_str().unwrap());
    assert!(words.contains(&"-nfa") && p["flash_attn"] == false);
    for (flag_name, key) in [("-tp", "temperature"), ("-tpi", "temperature_inc"), ("-et", "entropy_thold"), ("-lpt", "logprob_thold"), ("-nth", "no_speech_thold")] {
        assert_eq!(flag(flag_name).unwrap().parse::<f64>().unwrap(), p[key].as_f64().unwrap(), "{key}");
    }
    assert_eq!(flag("-bs").unwrap().parse::<u64>().unwrap(), p["beam_size"].as_u64().unwrap());
    assert_eq!(flag("-bo").unwrap().parse::<u64>().unwrap(), p["best_of"].as_u64().unwrap());
    assert!(words.contains(&"-pp"));
    // nothing is recorded that was not passed
    assert_eq!(p.as_object().unwrap().len(), 11);
    assert!(!res.to_string().contains(t.to_str().unwrap()), "no local path in the result");
}

#[test]
fn cancel_kills_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let body = format!(
        r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 7.8.9"; exit 0; fi
echo $$ > "{}/engine.pid"
exec sleep 60"#,
        t.display()
    );
    let cli = fake(t, &body);
    let wav = t.join("input.wav");
    std::fs::write(&wav, b"x").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (c2, pidfile) = (cancel.clone(), t.join("engine.pid"));
    std::thread::spawn(move || {
        // cancel only once the engine is really running
        while !pidfile.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        c2.store(true, Ordering::SeqCst);
    });
    let t0 = Instant::now();
    let err = cli.transcribe(&req(&wav, t), &|_| {}, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(10));
    let pid = std::fs::read_to_string(t.join("engine.pid")).unwrap();
    let alive = std::process::Command::new("kill").args(["-0", pid.trim()]).output().unwrap().status.success();
    assert!(!alive, "engine {pid} survived the cancel");
}

#[test]
fn typed_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let wav = t.join("input.wav");
    std::fs::write(&wav, b"x").unwrap();
    // engine binary missing
    let mut cli = fake(t, "exit 0");
    cli.cli = t.join("nope");
    assert!(matches!(cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::EngineNotFound(_)));
    // model missing
    let mut cli = fake(t, r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.0"; exit 0; fi"#);
    cli.model = t.join("missing.bin");
    assert!(matches!(cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::ModelMissing(_)));
    // runs but writes no JSON
    let cli = fake(t, r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.0"; exit 0; fi"#);
    assert!(matches!(cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::BadEngineOutput(_)));
    // no version in --version output
    let cli = fake(t, "echo hello");
    assert!(matches!(cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::BadEngineOutput(_)));
    // a claimed model hash must match the file
    let mut cli = fake(t, r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.0"; exit 0; fi"#);
    cli.model_sha256 = Some("0".repeat(64));
    assert!(matches!(cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::Engine(_)));
}

#[test]
fn model_hash_cache_is_private_and_path_free() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let cli = fake(t, "exit 0");
    let sha = cli.model_hash(t).unwrap();
    assert_eq!(sha, meetko_core::util::sha256_file(&cli.model).unwrap());
    let cache = t.join("model-sha256-cache.json");
    let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(!std::fs::read_to_string(&cache).unwrap().contains(t.to_str().unwrap()));
    // a changed file is hashed again
    std::fs::write(&cli.model, b"other weights").unwrap();
    assert_ne!(cli.model_hash(t).unwrap(), sha);
}
