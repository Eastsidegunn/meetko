//! The diarization binary over the stdio protocol. The tests that need the model and a recording
//! pass after saying so when they are not there, unless MEETKO_REQUIRE_MODELS=1 (as in CI), when
//! their absence fails. Inputs, by default from the repository's gitignored models/ and raw/:
//!   MEETKO_DIAR_MODEL      the model directory (holds Nemotron-3-Diarization.q8_0.gguf)
//!   MEETKO_DIAR_WAV        a 16 kHz mono PCM16 wav (default raw/meeting01/loopback.wav; its two
//!                          speakers are macOS TTS voices which the model merges into one)
//!   MEETKO_DIAR_MULTI_WAV  a recording with several real speakers (no default: an AIHub meeting clip)
//!   MEETKO_DIAR_MIC_WAV    the mic channel of the live test (default raw/meeting01/mic.wav: four HiKE speakers)
//!   MEETKO_STREAM_MODEL    the streaming caption model, for the live test (default models/nemotron-3.5-asr-streaming-0.6b)
//! The protocol tests (rejections, fatal paths) need only the binary. `-- --nocapture` shows the turns.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meetko_core::contract::{engine_result_validator, schema_errors};
use serde_json::{json, Value};

const SHA: &str = "ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34";

fn repo(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn required() -> bool {
    std::env::var_os("MEETKO_REQUIRE_MODELS").is_some_and(|v| v == "1")
}

/// `what` (a model directory or a wav) or None after saying so; a failure when the models are required.
fn input(what: &str, p: Option<PathBuf>) -> Option<PathBuf> {
    match p.filter(|p| p.exists()) {
        Some(p) => Some(p),
        None => {
            assert!(!required(), "{what} is missing, but MEETKO_REQUIRE_MODELS=1");
            eprintln!("skipped: {what} is not there (see the header of this file)");
            None
        }
    }
}

fn model() -> Option<PathBuf> {
    input("the diarization model directory", Some(env_path("MEETKO_DIAR_MODEL").unwrap_or_else(|| repo("models/nemotron-3-diarization"))))
}

fn wav_path() -> Option<PathBuf> {
    input("the wav", Some(env_path("MEETKO_DIAR_WAV").unwrap_or_else(|| repo("raw/meeting01/loopback.wav"))))
}

fn write_wav(path: &Path, channels: u16, frames: usize) {
    let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    b.extend(16u32.to_le_bytes());
    b.extend([1u16, channels].iter().flat_map(|v| v.to_le_bytes()));
    b.extend(16000u32.to_le_bytes());
    b.extend((32000 * u32::from(channels)).to_le_bytes());
    b.extend((2 * channels).to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend((frames as u32 * 2 * u32::from(channels)).to_le_bytes());
    b.extend(vec![0u8; frames * 2 * channels as usize]);
    std::fs::write(path, b).unwrap();
}

struct Eng {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    errors: Vec<Value>,
    events: Vec<Value>,
    next_id: u64,
}

impl Eng {
    /// `model`: the value of MEETKO_DIAR_MODEL, or the variable unset.
    fn start(model: Option<&Path>) -> Eng {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_meetko-nemotron-diar"));
        cmd.env_remove("MEETKO_DIAR_MODEL");
        if let Some(m) = model {
            cmd.env("MEETKO_DIAR_MODEL", m);
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("stdout carried something that is not JSON: {line}"));
                let _ = tx.send(v);
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let (mut err, sink) = (child.stderr.take().unwrap(), stderr.clone());
        std::thread::spawn(move || {
            let (mut b, mut chunk) = (Vec::new(), [0u8; 4096]);
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                b.extend_from_slice(&chunk[..n]);
                *sink.lock().unwrap() = String::from_utf8_lossy(&b).into_owned();
            }
        });
        Eng { child, stdin, rx, stderr, errors: vec![], events: vec![], next_id: 0 }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).and_then(|_| self.stdin.flush()).unwrap();
    }

    /// A request and its reply: Ok(result) or Err(error object).
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).as_bytes());
        loop {
            let v = self.rx.recv_timeout(Duration::from_secs(300)).expect("the engine answered");
            if v["id"] == json!(id) {
                return if v.get("error").is_some() { Err(v["error"].clone()) } else { Ok(v["result"].clone()) };
            }
            self.note(v);
        }
    }

    fn note(&mut self, v: Value) {
        if v["method"] == "engine.error" {
            self.errors.push(v["params"].clone());
        } else if v["method"] == "diarize.event" {
            self.events.push(v["params"].clone());
        } else {
            panic!("an unexpected message: {v}");
        }
    }

    fn init(&mut self) -> Value {
        self.call("initialize", json!({ "contract": ["0.1"] })).unwrap()
    }

    fn diarize(&mut self, wav: &Path) -> Result<Value, Value> {
        self.call("diarize.run", json!({ "wav": wav, "audio_sha256": SHA }))
    }

    /// Read until the engine closes stdout, then its exit code.
    fn die(&mut self) -> Option<i32> {
        while let Ok(v) = self.rx.recv_timeout(Duration::from_secs(60)) {
            self.note(v);
        }
        let code = self.child.wait().unwrap().code();
        std::thread::sleep(Duration::from_millis(100)); // the stderr reader catches up
        code
    }

    fn shutdown(&mut self) {
        self.call("shutdown", json!({})).unwrap();
        assert_eq!(self.die(), Some(0), "a polite end is exit code 0");
    }
}

/// Checks that hold for any recording, and the speakers found.
fn check_result(r: &Value, wav: &Path) -> Vec<String> {
    let errs = schema_errors(engine_result_validator(), r);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!((r["kind"].as_str(), r["audio_sha256"].as_str(), r["network"].as_bool()), (Some("diarization"), Some(SHA), Some(false)));
    assert_eq!(r["params"]["max_speakers"], 8);
    let duration_ms = (std::fs::metadata(wav).unwrap().len().saturating_sub(44) / 32) as i64;
    let turns = r["turns"].as_array().unwrap();
    assert!(!turns.is_empty(), "no speech found");
    let mut prev = 0;
    for t in turns {
        let (s, e) = (t["start_ms"].as_i64().unwrap(), t["end_ms"].as_i64().unwrap());
        assert!(prev <= s && s < e && e <= duration_ms + 100, "turn out of order or outside the audio ({duration_ms} ms): {t}");
        assert!(t["speaker"].as_str().unwrap().starts_with("spk"));
        prev = s;
    }
    let mut speakers: Vec<String> = turns.iter().map(|t| t["speaker"].as_str().unwrap().to_string()).collect();
    speakers.sort();
    speakers.dedup();
    speakers
}

#[test]
fn a_real_recording_through_the_real_binary() {
    let (Some(model), Some(wav)) = (model(), wav_path()) else { return };
    let mut eng = Eng::start(Some(&model));
    let init = eng.init();
    assert_eq!((init["role"].as_str(), init["contract"].as_str()), (Some("diarization"), Some("0.1")));
    let start = Instant::now();
    let first = eng.diarize(&wav).unwrap();
    let wall = start.elapsed().as_secs_f64();
    let speakers = check_result(&first, &wav);
    // the model stays loaded for the next request and the answer does not change
    let second = eng.diarize(&wav).unwrap();
    assert_eq!(first["turns"], second["turns"], "the same audio, the same turns");
    eprintln!("{} turns, speakers {speakers:?}, {wall:.1} s including the model load; first turns {}", first["turns"].as_array().unwrap().len(), first["turns"].as_array().unwrap().iter().take(3).map(|t| t.to_string()).collect::<Vec<_>>().join(" "));
    eng.shutdown();
    assert!(eng.errors.is_empty());
    assert!(!speakers.is_empty());
}

#[test]
fn a_meeting_with_several_speakers_is_told_apart() {
    let Some(model) = model() else { return };
    let Some(wav) = input("MEETKO_DIAR_MULTI_WAV (a recording with several speakers)", env_path("MEETKO_DIAR_MULTI_WAV")) else { return };
    let mut eng = Eng::start(Some(&model));
    eng.init();
    let start = Instant::now();
    let r = eng.diarize(&wav).unwrap();
    let speakers = check_result(&r, &wav);
    eprintln!("{} turns, speakers {speakers:?}, {:.1} s", r["turns"].as_array().unwrap().len(), start.elapsed().as_secs_f64());
    assert!(speakers.len() >= 2, "one speaker found in a recording of several: {speakers:?}");
    assert!(speakers.len() <= 8);
    eng.shutdown();
}

/// What the core does with it: stage the pinned model, run the engine, check the result.
#[test]
fn the_core_runs_it_with_the_pinned_model() {
    use meetko_core::diarize::{checked, DiarConfig, DiarEngine, DiarProcess, DiarRequest};
    let (Some(model), Some(wav)) = (model(), wav_path()) else { return };
    let cfg = DiarConfig { engine_bin: env!("CARGO_BIN_EXE_meetko-nemotron-diar").into(), model_dir: model, unpinned: false, timeout: Duration::from_secs(120), min_speaker_ms: 0 };
    let d = DiarProcess::new(cfg);
    let duration_ms = (std::fs::metadata(&wav).unwrap().len().saturating_sub(44) / 32) as i64;
    let req = DiarRequest { wav: &wav, audio_sha256: SHA, channel: 0, duration_ms };
    let r = d.diarize(&req, &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert_eq!(r["params"]["model_pinned"], true, "the staged copy matched the pin in models.toml");
    assert!(r["model"]["sha256"].as_str().is_some_and(|s| s.len() == 64));
    assert!(!checked(&r, SHA, duration_ms).unwrap().turns.is_empty());
}

// ---- protocol: these need the binary only

#[test]
fn requests_it_cannot_serve_are_error_replies_and_the_process_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (stereo, mono) = (tmp.path().join("stereo.wav"), tmp.path().join("mono.wav"));
    write_wav(&stereo, 2, 1600);
    write_wav(&mono, 1, 1600);
    let mut eng = Eng::start(None);
    assert_eq!(eng.call("initialize", json!({ "contract": ["9.9"] })).unwrap_err()["code"], -32001);
    eng.init();
    assert_eq!(eng.call("nope", json!({})).unwrap_err()["code"], -32601);
    assert_eq!(eng.call("diarize.run", json!({ "audio_sha256": SHA })).unwrap_err()["code"], -32602, "no wav");
    assert_eq!(eng.call("diarize.run", json!({ "wav": mono, "audio_sha256": "nope" })).unwrap_err()["code"], -32602, "no sha256");
    let e = eng.diarize(&tmp.path().join("missing.wav")).unwrap_err();
    assert_eq!(e["code"], -32010, "{e}");
    let e = eng.diarize(&stereo).unwrap_err();
    assert!(e["code"] == -32010 && e["message"].as_str().unwrap().contains("mono PCM16"), "{e}");
    let e = eng.diarize(&mono).unwrap_err(); // a good wav, but no model configured
    assert!(e["code"] == -32010 && e["message"].as_str().unwrap().contains("MEETKO_DIAR_MODEL is not set"), "{e}");
    eng.send_raw(b"this is not json\n"); // ignored: every request carries its id
    eng.shutdown();
    assert!(eng.errors.is_empty());
}

/// A fatal path says `engine.error` once, exits with code 1 and does not trip over ggml's static
/// destructors (GGML_ASSERT / SIGABRT).
fn assert_clean_fatal(eng: &mut Eng, what: &str) {
    let code = eng.die();
    assert_eq!(code, Some(1), "{what}: exit code (None = killed by a signal)");
    assert_eq!(eng.errors.len(), 1, "{what}: {:?}", eng.errors);
    assert!(eng.errors[0]["message"].as_str().is_some_and(|m| !m.is_empty()));
    let err = eng.stderr.lock().unwrap().clone();
    assert!(!err.contains("GGML_ASSERT") && !err.contains("abort"), "{what}: stderr: {err}");
}

#[test]
fn a_line_that_is_not_utf8_is_one_engine_error_and_a_clean_exit_1() {
    let mut eng = Eng::start(None);
    eng.init();
    eng.send_raw(b"{\"method\": \"\xff\xfe\"}\n");
    assert_clean_fatal(&mut eng, "non-UTF-8 line");
}

#[test]
fn a_model_file_the_library_cannot_load_is_one_engine_error_and_a_clean_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let (model, wav) = (tmp.path().join("Nemotron-3-Diarization.q8_0.gguf"), tmp.path().join("a.wav"));
    std::fs::write(&model, b"GGUF this is not a model").unwrap();
    write_wav(&wav, 1, 16000);
    let mut eng = Eng::start(Some(tmp.path()));
    eng.init();
    eng.send_raw(format!("{}\n", json!({ "jsonrpc": "2.0", "id": 7, "method": "diarize.run", "params": { "wav": wav, "audio_sha256": SHA } })).as_bytes());
    assert_clean_fatal(&mut eng, "unloadable model");
    assert!(eng.errors[0]["message"].as_str().unwrap().contains("cannot load the model"), "{:?}", eng.errors);
}

// ---- the live session: diarize.open, diarize.push, diarize.event, diarize.close

/// The samples of a 16 kHz mono PCM16 wav with the plain 44-byte header the capture writes.
fn pcm_of(wav: &Path) -> Vec<u8> {
    std::fs::read(wav).unwrap()[44..].to_vec()
}

fn push(eng: &mut Eng, pcm: &[u8]) {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
    eng.send_raw(format!("{}\n", json!({ "jsonrpc": "2.0", "method": "diarize.push", "params": { "pcm_s16le_b64": b64 } })).as_bytes());
}

/// The turns the events add up to, folded as the core folds them: keep that many, the rest is new.
fn fold(events: &[Value]) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    let mut labelled = 0;
    for ev in events {
        let keep = ev["keep"].as_u64().unwrap() as usize;
        assert!(keep <= turns.len(), "keep {keep} of {} turns", turns.len());
        let l = ev["labelled_ms"].as_i64().unwrap();
        assert!(l >= labelled + 1000 && l <= ev["emitted_at_ms"].as_i64().unwrap(), "an event per second of labelled audio, never ahead of the audio: {ev}");
        labelled = l;
        turns.truncate(keep);
        turns.extend(ev["turns"].as_array().unwrap().iter().cloned());
    }
    turns
}

#[test]
fn a_live_session_reports_turns_as_it_goes_and_closes_with_the_batch_answer() {
    let (Some(model), Some(wav)) = (model(), wav_path()) else { return };
    let mut eng = Eng::start(Some(&model));
    eng.init();
    let batch = eng.diarize(&wav).unwrap(); // also loads the model, so the session below is timed alone
    let open = eng.call("diarize.open", json!({ "sample_rate": 16000 })).unwrap();
    assert_eq!((open["params"]["mode"].as_str(), open["params"]["live"].as_bool()), (Some("streaming"), Some(true)));
    assert_eq!(eng.call("diarize.open", json!({ "sample_rate": 16000 })).unwrap_err()["code"], -32602, "one session at a time");
    let pcm = pcm_of(&wav);
    let start = Instant::now();
    for chunk in pcm.chunks(3200) {
        push(&mut eng, chunk); // 100 ms, as the core pushes
    }
    let r = eng.call("diarize.close", json!({ "audio_sha256": SHA })).unwrap();
    let wall = start.elapsed().as_secs_f64();
    let audio_s = pcm.len() as f64 / 32000.0;
    eprintln!("live session: {audio_s:.1} s of audio in {wall:.2} s ({:.0}x real time), {} events", audio_s / wall, eng.events.len());
    check_result(&r, &wav);
    assert_eq!(r["params"]["fed_ms"], json!(pcm.len() as i64 / 32));
    assert!(eng.events.len() as f64 >= audio_s / 2.0, "about one event per second of audio");
    // what the events said along the way is the start of the final answer, give or take the tail
    let folded = fold(&eng.events);
    let finals = r["turns"].as_array().unwrap();
    let same = folded.iter().zip(finals).take_while(|(a, b)| a == b).count();
    eprintln!("{} of {} folded turns agree with the final ones", same, folded.len());
    assert!(!folded.is_empty() && same + 2 >= folded.len(), "the live turns were revised more than at the tail");
    // the same audio in other slices gives the batch answer
    assert_eq!(r["turns"], batch["turns"], "live and batch turns differ");
    // the session is closed: another can be opened, and without a sha256 the result leaves it out
    eng.call("diarize.open", json!({ "sample_rate": 16000 })).unwrap();
    push(&mut eng, &pcm[..32000]);
    let bare = eng.call("diarize.close", json!({})).unwrap();
    assert!(bare.get("audio_sha256").is_none() && bare["params"]["fed_ms"] == 1000, "{bare}");
    eng.shutdown();
    assert!(eng.errors.is_empty());
}

#[test]
fn session_requests_it_cannot_serve_are_error_replies() {
    let mut eng = Eng::start(None);
    eng.init();
    assert_eq!(eng.call("diarize.close", json!({})).unwrap_err()["code"], -32602, "no session");
    assert_eq!(eng.call("diarize.open", json!({ "sample_rate": 4000 })).unwrap_err()["code"], -32602);
    assert_eq!(eng.call("diarize.open", json!({})).unwrap_err()["code"], -32602);
    let e = eng.call("diarize.open", json!({ "sample_rate": 16000 })).unwrap_err(); // no model configured
    assert!(e["code"] == -32010 && e["message"].as_str().unwrap().contains("MEETKO_DIAR_MODEL is not set"), "{e}");
    eng.shutdown();
    assert!(eng.errors.is_empty());
}

#[test]
fn a_push_without_a_session_is_one_engine_error_and_a_clean_exit_1() {
    let mut eng = Eng::start(None);
    eng.init();
    push(&mut eng, &[0u8; 3200]);
    assert_clean_fatal(&mut eng, "push without a session");
}

#[test]
fn invalid_pcm_or_a_torn_line_in_a_session_is_one_engine_error_and_a_clean_exit_1() {
    let Some(model) = model() else { return };
    for (what, line) in [("odd-length PCM", json!({ "jsonrpc": "2.0", "method": "diarize.push", "params": { "pcm_s16le_b64": "AA==" } }).to_string()), ("a torn line", "{\"jsonrpc\": \"2.0\", \"method\": \"diarize.pu".to_string())] {
        let mut eng = Eng::start(Some(&model));
        eng.init();
        eng.call("diarize.open", json!({ "sample_rate": 16000 })).unwrap();
        push(&mut eng, &[0u8; 32000]);
        eng.send_raw(format!("{line}\n").as_bytes());
        assert_clean_fatal(&mut eng, what);
    }
}

// ---- the core's live session with both real engines

fn wav_header(data_bytes: u32) -> Vec<u8> {
    let mut b = b"RIFF".to_vec();
    b.extend((36 + data_bytes).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend([1u16, 1].iter().flat_map(|v| v.to_le_bytes()));
    b.extend(16000u32.to_le_bytes());
    b.extend(32000u32.to_le_bytes());
    b.extend([2u16, 16].iter().flat_map(|v| v.to_le_bytes()));
    b.extend(b"data");
    b.extend(data_bytes.to_le_bytes());
    b
}

/// Captions and live speaker separation of `raw/meeting01` (mic: four HiKE speakers, loopback: two
/// TTS voices the model merges) through `LiveSession` with the real binaries and pinned models,
/// the channel files growing as a capture writes them: 4x real time, or real time under
/// MEETKO_TEST_PACED=1 (then the numbers printed are what a recording sees). Needs both models
/// (MEETKO_STREAM_MODEL, MEETKO_DIAR_MODEL) and the two wavs (MEETKO_DIAR_MIC_WAV, MEETKO_DIAR_WAV).
#[test]
fn the_core_separates_speakers_live_next_to_the_captions() {
    use meetko_core::capture_run::CaptureOutcome;
    use meetko_core::diarize::DiarConfig;
    use meetko_core::live::{store_live, LiveConfig, LiveSession, LiveUpdate, CHUNK_MS, WRITE_QUEUE};
    use meetko_core::store::Store;
    use std::os::unix::fs::FileExt;

    let Some(diar_model) = model() else { return };
    let Some(stream_model) = input("the streaming model directory", Some(env_path("MEETKO_STREAM_MODEL").unwrap_or_else(|| repo("models/nemotron-3.5-asr-streaming-0.6b")))) else { return };
    let Some(loopback) = wav_path() else { return };
    let Some(mic) = input("the mic wav", Some(env_path("MEETKO_DIAR_MIC_WAV").unwrap_or_else(|| repo("raw/meeting01/mic.wav")))) else { return };
    let paced = std::env::var("MEETKO_TEST_PACED").is_ok_and(|v| v == "1");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("capture");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = LiveConfig { engine_bin: env!("CARGO_BIN_EXE_meetko-nemotron-stream").into(), model_dir: stream_model, language: "auto".into(), unpinned: false, queue: WRITE_QUEUE };
    let dcfg = DiarConfig { engine_bin: env!("CARGO_BIN_EXE_meetko-nemotron-diar").into(), model_dir: diar_model, unpinned: false, timeout: Duration::from_secs(120), min_speaker_ms: 0 }; // the clip's speakers talk for less than the 10 s rule
    let t0 = Instant::now();
    let updates: Arc<Mutex<Vec<(f64, LiveUpdate)>>> = Arc::default();
    let u2 = updates.clone();
    let live = LiveSession::start_with(cfg, Some(dcfg), &dir, Arc::new(move |u| u2.lock().unwrap().push((t0.elapsed().as_secs_f64(), u))), None);
    let (a, b) = (pcm_of(&mic), pcm_of(&loopback));
    let n = a.len().min(b.len()) / 3200 * 3200;
    let files: Vec<std::fs::File> = ["mic.wav", "loopback.wav"].iter().map(|f| std::fs::File::create(dir.join(f)).unwrap()).collect();
    for f in &files {
        f.write_all_at(&wav_header(0), 0).unwrap();
    }
    // the models are hashed and loaded first (slow in a debug build of the core): a stop during
    // loading gives up on purpose, so the audio starts once both caption engines and both speaker
    // separations listen
    let ready = || updates.lock().unwrap().iter().filter(|(_, u)| matches!(u, LiveUpdate::Ready { .. })).count();
    while ready() < 2 {
        assert!(t0.elapsed() < Duration::from_secs(600), "the caption engines did not get ready");
        std::thread::sleep(Duration::from_millis(50));
    }
    while !live.diar_settled() {
        // the separations start once the captions listen
        assert!(t0.elapsed() < Duration::from_secs(600), "the speaker separations did not open");
        std::thread::sleep(Duration::from_millis(50));
    }
    let t_ready = t0.elapsed().as_secs_f64();
    let pause = Duration::from_millis(if paced { 100 } else { 25 });
    let start = Instant::now();
    for (i, off) in (0..n).step_by(3200).enumerate() {
        for (f, pcm) in files.iter().zip([&a, &b]) {
            f.write_all_at(&pcm[off..off + 3200], 44 + off as u64).unwrap();
        }
        if i % 10 == 9 {
            for f in &files {
                f.write_all_at(&wav_header((off + 3200) as u32), 0).unwrap();
            }
        }
        std::thread::sleep((pause * (i as u32 + 1)).saturating_sub(start.elapsed()));
    }
    for f in &files {
        f.write_all_at(&wav_header(n as u32), 0).unwrap();
    }
    let written = t0.elapsed().as_secs_f64();
    // a capture manifest, as the provider writes it last
    let sums: Vec<String> = ["mic.wav", "loopback.wav"].iter().map(|f| meetko_core::util::sha256_file(&dir.join(f)).unwrap()).collect();
    let ch = |i: usize, role: &str, f: &str| json!({ "index": i, "role": role, "file": f, "frames": n / 2, "sha256": sums[i], "device": "default_input", "dropped_buffers": 0, "gaps": [] });
    let manifest = json!({ "contract": "0.1", "kind": "capture", "provider": { "id": "test", "version": "0" }, "started_at": "2026-11-10T09:00:00.000Z", "sample_rate": 16000,
        "duration_ms": n / 32, "complete": true, "end_reason": "user", "recording_sha256": meetko_core::util::sha256_bytes(format!("0:{}\n1:{}\n", sums[0], sums[1]).as_bytes()),
        "channels": [ch(0, "mic", "mic.wav"), ch(1, "loopback", "loopback.wav")] });
    std::fs::write(dir.join("capture.json"), manifest.to_string()).unwrap();
    let outcome = live.finish();
    let drain = t0.elapsed().as_secs_f64() - written;

    let ups = updates.lock().unwrap().clone();
    let labels_during = ups.iter().filter(|(t, u)| *t < written && matches!(u, LiveUpdate::Speakers { channel: 0, labels } if labels.iter().any(|l| l.speaker.is_some()))).count();
    let first_label = ups.iter().find(|(_, u)| matches!(u, LiveUpdate::Speakers { labels, .. } if labels.iter().any(|l| l.speaker.is_some()))).map(|(t, _)| *t);
    eprintln!("caption engines ready after {t_ready:.1} s; {:.1} s of audio written in {:.1} s ({}); drain after the last write {drain:.2} s; mic speaker updates during the recording {labels_during}, first label at {first_label:?} s", n as f64 / 32000.0, written - t_ready, if paced { "real time" } else { "4x real time" });
    // how long after a mic caption became final its speaker was first named (while recording)
    let mut delays: Vec<f64> = Vec::new();
    for (tf, u) in &ups {
        let LiveUpdate::Caption { channel: 0, segment, is_final: true, .. } = u else { continue };
        let named = ups.iter().find(|(_, x)| matches!(x, LiveUpdate::Speakers { channel: 0, labels } if labels.iter().any(|l| l.segment == *segment && l.speaker.is_some())));
        if let Some((tl, _)) = named.filter(|(tl, _)| *tl < written) {
            delays.push((tl - tf).max(0.0));
        }
    }
    delays.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if let (Some(first), Some(last)) = (delays.first(), delays.last()) {
        eprintln!("mic segments named while recording: {}, delay after the final caption: min {first:.2} s, median {:.2} s, max {last:.2} s (wall time)", delays.len(), delays[delays.len() / 2]);
    }
    for d in outcome.diar.values() {
        eprintln!("diarization channel {}: complete {} fed {} ms error {:?}", d.index, d.complete, d.fed_bytes / 32, d.error);
        assert!(d.complete && d.result.is_some(), "the live separation kept up: {:?}", d.error);
    }
    for c in outcome.channels.values() {
        assert!(c.complete && c.error.is_none(), "captions: {:?}", c.error);
    }
    assert!(labels_during > 0, "provisional speakers arrived while recording");

    let store = Store::open(&tmp.path().join("data")).unwrap();
    let cap = CaptureOutcome { dir, exit_code: Some(0), killed: false, errors: vec![] };
    let (imp, _) = cap.import(&store, ("나", "상대"), None).unwrap();
    let md = store_live(&store, &imp.meeting.id, &outcome, CHUNK_MS).unwrap().unwrap();
    let diar: Vec<_> = md.version.producers.iter().filter(|p| p["role"] == "diarization").collect();
    assert_eq!(diar.len(), 2, "both channels carry the live speakers");
    assert!(diar.iter().all(|p| p["params"]["live"] == true && p["params"]["model_pinned"] == true && p["params"]["mode"] == "streaming"));
    let names: Vec<String> = md.speakers.iter().map(|s| s.label.clone().unwrap_or_default()).collect();
    eprintln!("stored speakers {names:?}");
    assert!(names.iter().filter(|n| n.starts_with("나 ")).count() >= 2, "the mic's speakers are told apart: {names:?}");
}
