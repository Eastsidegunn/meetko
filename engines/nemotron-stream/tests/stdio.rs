//! The real binary over the stdio protocol, on a real recording. It needs the model and a wav,
//! so without them it passes after saying so, unless MEETKO_REQUIRE_MODELS=1 (as CI sets
//! MEETKO_REQUIRE_PLUGINS), when their absence fails. By default it looks in the repository's
//! gitignored models/ and raw/ directories; override with
//!   MEETKO_STREAM_MODEL    the model directory (holds model.q8_0.gguf)
//!   MEETKO_CONFORMANCE_WAV a 16 kHz mono PCM16 wav
//!   MEETKO_STREAM_LANGUAGE auto (default), ko, en-US ...
//!   MEETKO_TEST_PACED=1    feed in real time (100 ms per 100 ms) to measure latency in wall time
//! Run with `-- --nocapture` to see the transcript and the measured speed.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use meetko_core::contract::{engine_result_validator, schema_errors};
use serde_json::{json, Value};

/// Hangul syllables and digits of the reference must come back at least this well (measured 0.989
/// on meeting01/loopback.wav; what is missing is a word or two, not whole sentences).
const MIN_REFERENCE_RECALL: f64 = 0.90;
/// Character error rate over Hangul and digits. The reference spells English words in Latin letters,
/// which are dropped, while the engine writes them in Hangul: those extra syllables count as errors,
/// so the measured 0.56 is mostly that. The bound only catches broken output.
const MAX_CER: f64 = 0.70;

fn repo(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

fn env_or(k: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or(default)
}

/// The model directory and the wav, or None (after saying so) when they are not there.
fn inputs() -> Option<(PathBuf, PathBuf)> {
    let model = env_or("MEETKO_STREAM_MODEL", repo("models/nemotron-3.5-asr-streaming-0.6b"));
    let wav = env_or("MEETKO_CONFORMANCE_WAV", repo("raw/meeting01/loopback.wav"));
    if model.exists() && wav.is_file() {
        return Some((model, wav));
    }
    assert!(std::env::var_os("MEETKO_REQUIRE_MODELS").is_none_or(|v| v != "1"), "the model directory or the wav is missing, but MEETKO_REQUIRE_MODELS=1");
    eprintln!("skipped: the model directory or the wav is not there (see the header of this file)");
    None
}

/// 16 kHz mono PCM16 bytes of a wav (the data chunk; sizes in the header are trusted).
fn wav_pcm(path: &PathBuf) -> Vec<u8> {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[0..4], b"RIFF");
    let (mut pos, mut fmt_ok) = (12, false);
    while pos + 8 <= b.len() {
        let (id, len) = (&b[pos..pos + 4], u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize);
        let body = pos + 8;
        if id == b"fmt " {
            let (tag, ch, rate, bits) = (u16::from_le_bytes([b[body], b[body + 1]]), u16::from_le_bytes([b[body + 2], b[body + 3]]), u32::from_le_bytes(b[body + 4..body + 8].try_into().unwrap()), u16::from_le_bytes([b[body + 14], b[body + 15]]));
            fmt_ok = (tag, ch, rate, bits) == (1, 1, 16000, 16);
        } else if id == b"data" {
            assert!(fmt_ok, "the wav must be 16 kHz mono PCM16");
            return b[body..(body + len).min(b.len())].to_vec();
        }
        pos = body + len + (len & 1);
    }
    panic!("no data chunk");
}

fn has_hangul(s: &str) -> bool {
    s.chars().any(|c| ('\u{ac00}'..='\u{d7a3}').contains(&c))
}

/// Hangul syllables and digits only.
fn keep(s: &str) -> Vec<char> {
    s.chars().filter(|c| ('\u{ac00}'..='\u{d7a3}').contains(c) || c.is_ascii_digit()).collect()
}

/// (edit distance, longest common subsequence) of two character strings.
fn distances(a: &[char], b: &[char]) -> (usize, usize) {
    let mut ed: Vec<usize> = (0..=b.len()).collect();
    let mut lcs = vec![0usize; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        let (mut ed_row, mut lcs_row) = (vec![i + 1], vec![0usize]);
        for (j, y) in b.iter().enumerate() {
            ed_row.push((ed[j] + usize::from(x != y)).min(ed[j + 1] + 1).min(ed_row[j] + 1));
            lcs_row.push(if x == y { lcs[j] + 1 } else { lcs[j + 1].max(lcs_row[j]) });
        }
        ed = ed_row;
        lcs = lcs_row;
    }
    (ed[b.len()], lcs[b.len()])
}

/// The engine process with everything it said so far.
struct Eng {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<(Instant, Value)>,
    stderr: Arc<Mutex<String>>,
    events: Vec<(Instant, Value)>,
    stream_errors: Vec<Value>,
    next_id: u64,
}

impl Eng {
    fn start(model: &PathBuf) -> Eng {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meetko-nemotron-stream"))
            .env("MEETKO_STREAM_MODEL", model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<(Instant, Value)>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("stdout carried something that is not JSON: {line}"));
                let _ = tx.send((Instant::now(), v));
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let (mut err, sink) = (child.stderr.take().unwrap(), stderr.clone());
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                b.extend_from_slice(&chunk[..n]);
                *sink.lock().unwrap() = String::from_utf8_lossy(&b).into_owned();
            }
        });
        Eng { child, stdin, rx, stderr, events: vec![], stream_errors: vec![], next_id: 0 }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").and_then(|_| self.stdin.flush()).unwrap();
    }

    fn push(&mut self, pcm: &[u8]) {
        self.send(json!({ "jsonrpc": "2.0", "method": "stream.push", "params": { "pcm_s16le_b64": base64::engine::general_purpose::STANDARD.encode(pcm) } }));
    }

    /// A request and its reply: Ok(result) or Err(error object). Events on the way are kept.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let (at, v) = self.rx.recv_timeout(Duration::from_secs(300)).expect("the engine answered");
            if v["id"] == json!(id) {
                return if v.get("error").is_some() { Err(v["error"].clone()) } else { Ok(v["result"].clone()) };
            }
            self.note(at, v);
        }
    }

    fn note(&mut self, at: Instant, v: Value) {
        match v["method"].as_str() {
            Some("stream.error") => self.stream_errors.push(v["params"].clone()),
            _ => self.events.push((at, v["params"].clone())),
        }
    }

    /// Read until the engine closes stdout, then its exit code.
    fn die(&mut self) -> Option<i32> {
        while let Ok((at, v)) = self.rx.recv_timeout(Duration::from_secs(60)) {
            self.note(at, v);
        }
        let code = self.child.wait().unwrap().code();
        std::thread::sleep(Duration::from_millis(100)); // the stderr reader catches up
        code
    }

    fn open(&mut self, language: &str) -> Result<Value, Value> {
        self.call("stream.open", json!({ "sample_rate": 16000, "language": language }))
    }
}

#[test]
fn real_recording_through_the_real_binary() {
    let Some((model, wav)) = inputs() else { return };
    let language = std::env::var("MEETKO_STREAM_LANGUAGE").unwrap_or_else(|_| "auto".into());
    let paced = std::env::var("MEETKO_TEST_PACED").is_ok_and(|v| v == "1");
    let pcm = wav_pcm(&wav);
    let duration_ms = (pcm.len() / 2 * 1000 / 16000) as i64;

    let mut eng = Eng::start(&model);
    let init = eng.call("initialize", json!({ "contract": ["0.1"] })).unwrap();
    assert_eq!((init["role"].as_str(), init["contract"].as_str()), (Some("asr.stream"), Some("0.1")));
    let load = Instant::now();
    let open = eng.open(&language).unwrap();
    eprintln!("engine {} / model load {:.1} s / params {}", init["engine"]["version"], load.elapsed().as_secs_f64(), open["params"]);

    let start = Instant::now();
    for (i, chunk) in pcm.chunks(3200).enumerate() {
        if paced {
            std::thread::sleep((start + Duration::from_millis(100 * i as u64)).saturating_duration_since(Instant::now()));
        }
        eng.push(chunk);
    }
    let closed = eng.call("stream.close", json!({})).unwrap();
    let wall = start.elapsed().as_secs_f64();
    eng.call("shutdown", json!({})).unwrap();
    assert_eq!(eng.die(), Some(0));
    assert!(eng.stream_errors.is_empty());

    // every event is valid against the contract schema the core uses
    let events = std::mem::take(&mut eng.events);
    assert!(!events.is_empty());
    for (_, e) in &events {
        let errs = schema_errors(engine_result_validator(), e);
        assert!(errs.is_empty(), "{errs:?} in {e}");
        assert!(e["emitted_at_ms"].as_i64().unwrap() <= duration_ms, "audio the engine was never given: {e}");
    }
    assert_eq!(closed["fed_ms"].as_i64(), Some(duration_ms));

    // the audio clock and the revisions only go forward; a final is the last of its segment
    let evs: Vec<&Value> = events.iter().map(|(_, e)| e).collect();
    let fed: Vec<i64> = evs.iter().map(|e| e["emitted_at_ms"].as_i64().unwrap()).collect();
    assert!(fed.windows(2).all(|w| w[0] <= w[1]));
    let mut last: Option<(i64, i64, bool)> = None; // segment, revision, was final
    for e in &evs {
        let (s, r, f) = (e["segment"].as_i64().unwrap(), e["revision"].as_i64().unwrap(), e["final"].as_bool().unwrap());
        match last {
            Some((ls, lr, lf)) if ls == s => assert!(!lf && r > lr, "segment {s} revision {r} after revision {lr} (final: {lf})"),
            Some((ls, _, _)) => assert!(s == ls + 1 && r == 0, "segments are consecutive and start at revision 0"),
            None => assert_eq!((s, r), (0, 0)),
        }
        last = Some((s, r, f));
    }

    // finals: Korean text, word times in order and inside the audio, text consistent with the words
    let finals: Vec<&Value> = evs.iter().copied().filter(|e| e["final"] == json!(true)).collect();
    assert!(!finals.is_empty(), "no final at all");
    let text: String = finals.iter().map(|e| e["text"].as_str().unwrap()).collect::<Vec<_>>().join(" ");
    if language != "en" && !language.starts_with("en-") {
        assert!(has_hangul(&text), "no Korean in the finals: {text}");
    }
    let mut prev_start = 0;
    let mut words = 0;
    for e in &finals {
        assert!(!e["text"].as_str().unwrap().trim().is_empty());
        assert_eq!(e["start_ms"], e["units"][0]["start_ms"], "a final starts at its first word");
        let joined: Vec<&str> = e["units"].as_array().unwrap().iter().map(|u| u["text"].as_str().unwrap().trim()).collect();
        assert_eq!(joined.join(" "), e["text"].as_str().unwrap(), "a final's text is its words, punctuation included");
        for u in e["units"].as_array().unwrap() {
            let (s, en) = (u["start_ms"].as_i64().unwrap(), u["end_ms"].as_i64().unwrap());
            assert!(s >= prev_start, "word starts go backwards at {u}");
            assert!(s <= en && en <= duration_ms + 200, "word outside the audio ({duration_ms} ms): {u}");
            assert!(u["text"].as_str().unwrap().starts_with(' '));
            prev_start = s;
            words += 1;
        }
    }
    assert!(words > 0);

    // accuracy against the recording's reference (Korean only; the Latin letters of the reference are not scored)
    if let Some(r) = std::fs::read_to_string(wav.with_file_name("reference.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        let channel = match wav.file_stem().and_then(|s| s.to_str()) {
            Some("mic") => 0,
            Some("loopback") => 1,
            _ => -1,
        };
        let reference: String = r["utterances"].as_array().into_iter().flatten().filter(|u| u["channel"] == json!(channel)).map(|u| u["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join(" ");
        let (refc, hyp) = (keep(&reference), keep(&text));
        if !refc.is_empty() {
            let (ed, lcs) = distances(&refc, &hyp);
            let (cer, recall) = (ed as f64 / refc.len() as f64, lcs as f64 / refc.len() as f64);
            eprintln!("against reference.json: CER {cer:.3} (Hangul and digits), reference recall {recall:.3}");
            assert!(recall >= MIN_REFERENCE_RECALL, "recall {recall:.3} < {MIN_REFERENCE_RECALL}");
            assert!(cer <= MAX_CER, "CER {cer:.3} > {MAX_CER}");
        }
    }

    // speed: real-time factor of the unpaced run, and how late the first words appear
    let partials = evs.len() - finals.len();
    let first_start = finals[0]["units"][0]["start_ms"].as_i64().unwrap();
    let first_partial = events.iter().find(|(_, e)| e["final"] == json!(false) && !e["text"].as_str().unwrap().is_empty());
    eprintln!("{} events ({partials} partial), {} finals, {words} words, audio {:.1} s", evs.len(), finals.len(), duration_ms as f64 / 1000.0);
    if let Some((at, p)) = first_partial {
        let audio_latency = p["emitted_at_ms"].as_i64().unwrap() - first_start;
        let wall_latency = if paced { format!("{:.0} ms wall", (at.duration_since(start).as_secs_f64() * 1000.0) - first_start as f64) } else { "n/a (not paced)".into() };
        eprintln!("first word: starts at {first_start} ms, first partial text arrived when {} ms were fed ({audio_latency} ms of audio later), {wall_latency}", p["emitted_at_ms"]);
    }
    eprintln!("{}: {wall:.1} s for {:.1} s of audio, real-time factor {:.3}", if paced { "paced" } else { "unpaced" }, duration_ms as f64 / 1000.0, wall / (duration_ms as f64 / 1000.0));
    eprintln!("transcript: {text}");
}

/// A fatal path says `stream.error` once, exits with code 1 and does not trip over ggml's
/// static destructors (GGML_ASSERT / SIGABRT) while the model is still loaded.
fn assert_clean_fatal(eng: &mut Eng, what: &str) {
    let code = eng.die();
    assert_eq!(code, Some(1), "{what}: exit code (None = killed by a signal)");
    assert_eq!(eng.stream_errors.len(), 1, "{what}: {:?}", eng.stream_errors);
    assert!(eng.stream_errors[0]["message"].as_str().is_some_and(|m| !m.is_empty()));
    let err = eng.stderr.lock().unwrap().clone();
    assert!(!err.contains("GGML_ASSERT") && !err.contains("abort"), "{what}: stderr: {err}");
}

#[test]
fn an_odd_length_chunk_is_one_stream_error_and_a_clean_exit_1() {
    let Some((model, _)) = inputs() else { return };
    let mut eng = Eng::start(&model);
    eng.call("initialize", json!({ "contract": ["0.1"] })).unwrap();
    eng.open("ko").unwrap();
    eng.push(&[0u8; 3201]); // half a sample: the framing is broken
    assert_clean_fatal(&mut eng, "odd PCM");
}

#[test]
fn a_line_that_is_not_json_is_fatal_while_a_stream_is_open_only() {
    let Some((model, _)) = inputs() else { return };
    let mut eng = Eng::start(&model);
    eng.send(json!({ "jsonrpc": "2.0", "id": 99, "method": "initialize", "params": { "contract": ["0.1"] } }));
    writeln!(eng.stdin, "this is not json").unwrap(); // no stream yet: ignored
    eng.stdin.flush().unwrap();
    eng.next_id = 99;
    // the reply to the initialize above arrives as a normal call result
    let (_, v) = eng.rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(v["id"], json!(99));
    eng.open("ko").unwrap();
    writeln!(eng.stdin, "{{\"jsonrpc\": \"2.0\", \"method\": \"stream.pu").unwrap(); // a torn push
    eng.stdin.flush().unwrap();
    assert_clean_fatal(&mut eng, "non-JSON line");
}

#[test]
fn a_push_without_a_stream_is_fatal() {
    let Some((model, _)) = inputs() else { return };
    let mut eng = Eng::start(&model);
    eng.call("initialize", json!({ "contract": ["0.1"] })).unwrap();
    eng.push(&[0u8; 3200]);
    assert_clean_fatal(&mut eng, "push without stream");
}

#[test]
fn a_closed_stream_can_be_opened_again_and_a_second_open_is_refused() {
    let Some((model, wav)) = inputs() else { return };
    let pcm = wav_pcm(&wav);
    let mut eng = Eng::start(&model);
    eng.call("initialize", json!({ "contract": ["0.1"] })).unwrap();
    assert!(eng.call("stream.close", json!({})).is_err(), "nothing to close");
    assert!(eng.open("zz zz; rm").is_err(), "a language that is not a code is refused");
    let mut segment_counts = vec![];
    for round in 0..2 {
        eng.open("auto").unwrap_or_else(|e| panic!("open #{round}: {e}"));
        assert_eq!(eng.open("auto").unwrap_err()["code"], json!(-32602), "a stream is already open");
        for chunk in pcm[..16000 * 2 * 12].chunks(3200) {
            eng.push(chunk);
        }
        let closed = eng.call("stream.close", json!({})).unwrap();
        assert_eq!(closed["fed_ms"], json!(12000));
        segment_counts.push(closed["segments"].as_i64().unwrap());
    }
    assert!(segment_counts[0] > 0 && segment_counts[0] == segment_counts[1], "a fresh stream each time: {segment_counts:?}");
    let first_of_round_two = eng.events.iter().rposition(|(_, e)| e["segment"] == json!(0) && e["revision"] == json!(0)).unwrap();
    assert!(first_of_round_two > 0, "segment numbering restarted at 0");
    eng.call("shutdown", json!({})).unwrap();
    assert_eq!(eng.die(), Some(0));
}
