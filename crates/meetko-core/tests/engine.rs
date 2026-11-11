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
    BatchRequest { wav, audio_sha256: SHA, language: "ko", language_explicit: true, channel: 0, channels: 2, data_dir: data }
}

const JSON: &str = r#"{"result":{"language":"ko"},"transcription":[{"offsets":{"from":0,"to":1000},"tokens":[{"text":" 안녕","p":0.9,"t_dtw":10,"offsets":{"from":0,"to":500}}]}]}"#;

fn fake(dir: &Path, cli_body: &str) -> WhisperCli {
    let model = dir.join("ggml-fake.bin");
    std::fs::write(&model, b"weights").unwrap();
    WhisperCli { cli: script(dir, "whisper-cli", cli_body), model, model_sha256: None, dtw: "large.v3.turbo".into(), vad: None, hints: vec![] }
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
    // VAD is opt-in: no VAD flags, the log stays off, and the producer says so
    assert!(words.contains(&"-np") && !words.contains(&"--vad"));
    assert_eq!((&p["vad"], &p["vad_unused"]), (&serde_json::json!(false), &serde_json::json!("not_enabled")));
    // the input is no WAV the speech check reads: it goes to whisper all the same
    assert_eq!(p["speech_gate"]["result"], "unread");
    // nothing is recorded that was not passed (or is not meetko's own: the speech check and the VAD state)
    assert_eq!(p.as_object().unwrap().len(), 14);
    assert!(!res.to_string().contains(t.to_str().unwrap()), "no local path in the result");
}

/// A ggml whisper model header with this vocabulary (and n_text_ctx 448), then a few weight bytes.
fn ggml_model(tokens: &[&str]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend(0x6767_6d6cu32.to_le_bytes());
    for v in [tokens.len() as i32, 1500, 1280, 20, 32, 448, 1280, 20, 4, 1, 1] {
        f.extend(v.to_le_bytes());
    }
    f.extend(1i32.to_le_bytes());
    f.extend(2i32.to_le_bytes());
    f.extend([0u8; 8]);
    f.extend((tokens.len() as i32).to_le_bytes());
    for t in tokens {
        f.extend((t.len() as u32).to_le_bytes());
        f.extend(t.as_bytes());
    }
    f.extend(b"weights");
    f
}

/// A fake whisper-cli that writes its arguments one per line; `help` is what its `--help` prints.
fn args_cli(t: &Path, help: &str) -> String {
    format!(
        r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.9.5"; exit 0; fi
if [ "$1" = "--help" ]; then echo "{1}" >&2; exit 0; fi
for a in "$@"; do printf '%s\n' "$a"; done > "{0}/args.txt"
while [ $# -gt 0 ]; do if [ "$1" = "-of" ]; then out="$2"; fi; shift; done
cp "{0}/out.json" "$out.json""#,
        t.display(),
        help
    )
}

#[test]
fn hint_terms_are_the_prompt_for_every_window_and_recorded_by_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::write(t.join("out.json"), JSON).unwrap();
    let mut cli = fake(t, &args_cli(t, "  --prompt PROMPT\n  --carry-initial-prompt")).with_hints(vec!["미트코".into(), "Kubernetes".into(), "Sprint Review".into()]);
    // one token per term part: "미트코" "," " Kubernetes" "," " Sprint" " Review"
    std::fs::write(&cli.model, ggml_model(&["미트코", ",", " Kubernetes", " Sprint", " Review"])).unwrap();
    let wav = t.join("input.wav");
    std::fs::write(&wav, b"x").unwrap();
    let res = cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap();
    let args: Vec<String> = std::fs::read_to_string(t.join("args.txt")).unwrap().lines().map(String::from).collect();
    let flag = |f: &str| args.iter().position(|w| w == f).map(|i| args[i + 1].clone());
    let prompt = "미트코, Kubernetes, Sprint Review";
    assert_eq!(flag("--prompt").as_deref(), Some(prompt));
    assert!(args.iter().any(|a| a == "--carry-initial-prompt"));
    // room for exactly the prompt: no decoded text is carried between windows
    assert_eq!(flag("-mc").as_deref(), Some("7"));
    let p = &res["params"];
    assert_eq!(p["max_context"], 7);
    assert_eq!(p["carry_initial_prompt"], true);
    assert_eq!(p["prompt"], serde_json::json!({"terms": 3, "of": 3, "tokens": 6}), "counts only: no terms, no hash");
    assert!(!res.to_string().contains("Kubernetes"), "the terms are not in the provenance");

    // a model whose vocabulary cannot be read: no prompt, and that is recorded
    std::fs::write(&cli.model, b"weights").unwrap();
    let res = cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap();
    let args = std::fs::read_to_string(t.join("args.txt")).unwrap();
    assert!(!args.contains("--prompt") && args.contains("-mc\n0\n"), "{args}");
    assert_eq!(res["params"]["prompt_unused"], "no_tokenizer");
    assert!(res["params"].get("prompt").is_none());

    // an older whisper-cli without --carry-initial-prompt (it would print its usage and exit 0 on the
    // unknown option): no prompt flags at all, and that is recorded
    let old = fake(t, &args_cli(t, "  --prompt PROMPT")).with_hints(cli.hints.clone());
    std::fs::write(&old.model, ggml_model(&["미트코"])).unwrap();
    let res = old.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap();
    let args = std::fs::read_to_string(t.join("args.txt")).unwrap();
    assert!(!args.contains("--prompt") && !args.contains("--carry-initial-prompt") && args.contains("-mc\n0\n"), "{args}");
    assert_eq!(res["params"]["prompt_unused"], "unsupported");

    // no hints: nothing about a prompt at all
    cli.hints.clear();
    let res = cli.transcribe(&req(&wav, t), &|_| {}, &AtomicBool::new(false)).unwrap();
    assert!(res["params"].get("prompt_unused").is_none() && res["params"].get("prompt").is_none());
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

fn wav(path: &Path, samples: impl Iterator<Item = i16>) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
}

/// A fake whisper-cli with VAD in its help that writes JSON (one word) and notes every run.
fn noting(t: &Path) -> WhisperCli {
    std::fs::write(t.join("out.json"), JSON).unwrap();
    let body = format!(
        r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.9.5"; exit 0; fi
if [ "$1" = "--help" ]; then echo "  -vm FNAME, --vad-model FNAME  VAD model path" >&2; exit 0; fi
echo "$@" >> "{0}/runs.txt"
while [ $# -gt 0 ]; do if [ "$1" = "-of" ]; then out="$2"; fi; shift; done
cp "{0}/out.json" "$out.json""#,
        t.display()
    );
    fake(t, &body)
}

/// A steady hiss of about -66 dBFS (xorshift, so the test is deterministic), with `word` added.
fn hiss(seconds: usize, word: impl Fn(usize) -> i16) -> impl Iterator<Item = i16> {
    let mut x: u32 = 1;
    (0..16000 * seconds).map(move |i| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        ((x % 37) as i16 - 18) + word(i)
    })
}

#[test]
fn a_channel_without_speech_is_not_sent_to_whisper() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let cli = noting(t);
    // a muted mic: a minute of hiss
    let wav_path = t.join("input.wav");
    wav(&wav_path, hiss(60, |_| 0));
    let seen = std::sync::Mutex::new(Vec::new());
    let res = cli.transcribe(&req(&wav_path, t), &|p: Progress| seen.lock().unwrap().push(p.percent), &AtomicBool::new(false)).unwrap();
    assert!(!t.join("runs.txt").exists(), "whisper-cli was run on a channel without speech");
    assert_eq!(res["words"], serde_json::json!([]));
    assert_eq!(res["params"]["skipped"], "no_speech");
    assert_eq!(res["params"]["speech_gate"]["result"], "no_speech");
    assert_eq!(res["coverage"], serde_json::json!([{ "start_ms": 0, "end_ms": 60000 }]), "the whole channel was looked at");
    assert_eq!(res["engine"]["version"], "1.9.5");
    assert_eq!(seen.lock().unwrap().last(), Some(&100));
    assert!(meetko_core::contract::schema_errors(meetko_core::contract::engine_result_validator(), &res).is_empty());

    // the same hiss with a quarter second of a quiet tone in it (about -45 dBFS) goes to whisper
    wav(&wav_path, hiss(60, |i| if (16000 * 30..16000 * 30 + 4000).contains(&i) { ((i as f32 * 0.1).sin() * 200.0) as i16 } else { 0 }));
    let res = cli.transcribe(&req(&wav_path, t), &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read_to_string(t.join("runs.txt")).unwrap().lines().count(), 1);
    assert_eq!(res["params"]["speech_gate"]["result"], "speech");
    assert!(res["params"].get("skipped").is_none());
    assert_eq!(res["words"][0]["text"], "안녕");
}

#[test]
fn a_vad_model_that_is_not_the_pinned_one() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let mut cli = noting(t);
    let dir = t.join("vad");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(meetko_core::whisper::VAD_MODEL_FILE), b"not silero").unwrap();
    let wav_path = t.join("input.wav");
    std::fs::write(&wav_path, b"x").unwrap();
    // asked for VAD with a file that is not the pin: an error, not a quiet run without it
    cli.vad = Some(dir);
    assert!(matches!(cli.transcribe(&req(&wav_path, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::ModelMissing(_)));
    assert!(!t.join("runs.txt").exists(), "whisper-cli was not run");
}

#[test]
fn vad_asked_of_a_whisper_cli_without_it_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    std::fs::write(t.join("out.json"), JSON).unwrap();
    let body = format!(
        r#"if [ "$1" = "--version" ]; then echo "whisper.cpp version: 1.7.0"; exit 0; fi
if [ "$1" = "--help" ]; then echo "usage: whisper-cli [options]"; exit 0; fi
while [ $# -gt 0 ]; do if [ "$1" = "-of" ]; then out="$2"; fi; shift; done
cp "{0}/out.json" "$out.json""#,
        t.display()
    );
    let mut cli = fake(t, &body);
    let dir = t.join("vad");
    std::fs::create_dir(&dir).unwrap();
    let wav_path = t.join("input.wav");
    std::fs::write(&wav_path, b"x").unwrap();
    cli.vad = Some(dir);
    assert!(matches!(cli.transcribe(&req(&wav_path, t), &|_| {}, &AtomicBool::new(false)).unwrap_err(), Error::Engine(_)));
}
