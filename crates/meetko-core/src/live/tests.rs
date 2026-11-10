use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::capture_run::CaptureOutcome;
use crate::contract::meeting_data_validator;
use crate::manifest::Manifest;

/// A fake engine in Python speaking the real protocol. It emits a partial every second of audio it
/// was fed and a final every third word, and says nothing else. `mode`: "ok"; "die" exits abruptly
/// after 45 chunks (4.5 s of audio); "bad" also sends one event that breaks the schema.
fn fake_engine(dir: &Path, mode: &str) -> PathBuf {
    let src = r#"#!/usr/bin/env python3
import base64, json, os, sys, time
MODE = "MODE"
fed = pushes = seg = rev = seg_start = 0
words = []
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def ev(final):
    global rev
    params = {"contract": "0.1", "kind": "asr.stream", "segment": seg, "revision": rev, "final": final,
              "start_ms": seg_start, "text": " ".join(w.strip() for w in words), "emitted_at_ms": fed * 1000 // 16000,
              "units": [{"text": w, "start_ms": seg_start + i * 1000} for i, w in enumerate(words)]}
    out({"jsonrpc": "2.0", "method": "stream.event", "params": params})
    rev += 1
for line in sys.stdin:
    m = json.loads(line)
    meth, id = m.get("method"), m.get("id")
    if meth == "initialize":
        if MODE == "flood_late":
            time.sleep(3)  # the feed loop is then long done when the guard trips
        out({"jsonrpc": "2.0", "id": id, "result": {"contract": "0.1", "engine": {"id": "fake/stream", "version": "0"}, "role": "asr.stream", "model": {"id": "fake-model"}}})
    elif meth == "stream.open":
        if MODE == "hang_open":
            time.sleep(600)
        out({"jsonrpc": "2.0", "id": id, "result": {"params": {"decoding": "fake"}}})
    elif meth == "stream.push":
        fed += len(base64.b64decode(m["params"]["pcm_s16le_b64"])) // 2
        pushes += 1
        if MODE == "slow":
            time.sleep(0.03)
        if MODE == "fault" and pushes == 5:
            out({"jsonrpc": "2.0", "method": "stream.error", "params": {"message": "stream.push carried invalid PCM"}})
            sys.exit(1)
        if MODE == "units" and pushes == 3:
            out({"jsonrpc": "2.0", "method": "stream.event", "params": {"contract": "0.1", "kind": "asr.stream", "segment": 99, "revision": 0, "final": False,
                 "start_ms": 0, "text": "x", "emitted_at_ms": 300, "units": [{"text": " a", "start_ms": i} for i in range(2001)]}})
        if MODE in ("flood", "flood_late") and pushes == 3:
            # count-based: past MAX_SEGMENTS distinct segments whatever the machine's speed
            for i in range(10100):
                out({"jsonrpc": "2.0", "method": "stream.event", "params": {"contract": "0.1", "kind": "asr.stream", "segment": i, "revision": 0, "final": False,
                     "start_ms": 0, "text": "x", "emitted_at_ms": 300, "units": []}})
        if MODE == "die" and pushes == 45:
            os._exit(3)
        if fed % 16000 == 0:
            words.append(" w%d_%d" % (seg, len(words)))
            ev(False)
            if MODE == "bad" and seg == 0 and len(words) == 1:
                out({"jsonrpc": "2.0", "method": "stream.event", "params": {"kind": "nope"}})
            if len(words) == 3:
                ev(True)
                seg += 1; rev = 0; words = []; seg_start = fed * 1000 // 16000
    elif meth == "stream.close":
        if MODE == "stuck_close":
            time.sleep(600)
        if words:
            ev(True)
        out({"jsonrpc": "2.0", "id": id, "result": {"fed_ms": fed * 1000 // 16000}})
    elif meth == "shutdown":
        out({"jsonrpc": "2.0", "id": id, "result": {}})
        break
"#;
    let p = dir.join(format!("fake-engine-{mode}.py"));
    std::fs::write(&p, src.replace("MODE", mode)).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// The files of the known model that `engine` loads.
fn model_files(engine: &str) -> Vec<String> {
    pins().iter().find(|p| p.engine == engine && p.role == ROLE_STREAM).unwrap().files.keys().cloned().collect()
}

/// A model directory of dummy files next to the fake engine; unpinned, so staging accepts it.
fn cfg(bin: PathBuf) -> LiveConfig {
    let dir = bin.parent().unwrap().join("model-x");
    std::fs::create_dir_all(&dir).unwrap();
    for f in model_files("sherpa-stream") {
        std::fs::write(dir.join(&f), &f).unwrap();
    }
    LiveConfig { engine_bin: bin, model_dir: dir, language: "ko".into(), unpinned: true, queue: WRITE_QUEUE }
}

fn header(data_bytes: u32) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&1u16.to_le_bytes()); // mono
    h.extend_from_slice(&16000u32.to_le_bytes());
    h.extend_from_slice(&32000u32.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_bytes.to_le_bytes());
    h
}

/// Like the capture provider: the header is patched only now and then, the data grows continuously.
fn write_growing(path: PathBuf, chunks: usize, pause: Duration, patch_every: usize) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&header(0)).unwrap();
        for i in 0..chunks {
            // a sloped signal, so a misplaced byte would show in the equality test
            let pcm: Vec<u8> = (0..1600).flat_map(|k| (((i * 1600 + k) % 20000) as i16).to_le_bytes()).collect();
            f.write_all(&pcm).unwrap();
            if (i + 1) % patch_every == 0 {
                use std::os::unix::fs::FileExt;
                f.write_all_at(&header(((i + 1) * 3200) as u32), 0).unwrap();
            }
            std::thread::sleep(pause);
        }
        use std::os::unix::fs::FileExt;
        f.write_all_at(&header((chunks * 3200) as u32), 0).unwrap(); // the final patch
    })
}

#[test]
fn tail_follows_a_growing_wav_with_a_lagging_header() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a.wav");
    let w = write_growing(path.clone(), 40, Duration::from_millis(5), 1000); // header stays at 0 bytes
    let mut tail = loop {
        if let Ok(t) = WavTail::open(&path) {
            break t;
        }
    };
    let (mut got, mut reads) = (Vec::new(), 0);
    while !w.is_finished() || reads < 3 {
        let b = tail.read_new().unwrap();
        assert_eq!(b.len() % 2, 0, "only whole samples");
        got.extend(b);
        reads += 1;
        std::thread::sleep(Duration::from_millis(3));
    }
    w.join().unwrap();
    got.extend(tail.read_new().unwrap());
    let expect: Vec<u8> = (0..40 * 1600).flat_map(|k| ((k % 20000) as i16).to_le_bytes()).collect();
    assert_eq!(got.len(), expect.len());
    assert!(got == expect, "bytes differ");
}

#[test]
fn tail_waits_for_a_complete_header_and_rejects_other_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("b.wav");
    std::fs::write(&p, &header(0)[..20]).unwrap();
    let mut t = WavTail::open(&p).unwrap();
    assert!(t.read_new().unwrap().is_empty());
    let mut full = header(0);
    full.extend_from_slice(&[1, 0, 2, 0, 3]); // two samples and half of a third
    std::fs::write(&p, &full).unwrap();
    assert_eq!(t.read_new().unwrap(), vec![1, 0, 2, 0]);
    let mut stereo = header(0);
    stereo[22] = 2;
    std::fs::write(&p, &stereo).unwrap();
    assert!(WavTail::open(&p).unwrap().read_new().is_err());
}

fn ev(seg: i64, rev: i64, fin: bool, text: &str) -> Value {
    json!({ "segment": seg, "revision": rev, "final": fin, "text": text, "emitted_at_ms": 100 * (rev + 1) })
}

#[test]
fn folding_is_replace_by_revision() {
    let mut f = Folder::default();
    assert!(f.apply(&ev(0, 0, false, "a")));
    assert!(f.apply(&ev(0, 2, false, "abc")), "a later revision replaces, it does not append");
    assert!(!f.apply(&ev(0, 1, false, "ab")), "a stale revision is ignored");
    assert!(!f.apply(&ev(0, 2, false, "again")), "a repeat changes nothing");
    assert!(f.apply(&ev(0, 3, true, "abcd")));
    assert!(!f.apply(&ev(0, 9, false, "late")), "a final never changes again");
    assert!(f.apply(&ev(1, 0, false, "x")));
    let evs: Vec<&Value> = f.events().collect();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0]["text"], "abcd");
    assert_eq!(f.last_final_ms(), 400);
}

/// A capture directory as the provider leaves it, finished: the manifest is written last.
fn finish_manifest(dir: &Path, frames: usize) {
    use crate::util::sha256_file;
    let sums: Vec<String> = ["mic.wav", "loopback.wav"].iter().map(|n| sha256_file(&dir.join(n)).unwrap()).collect();
    let rec = crate::util::sha256_bytes(format!("0:{}\n1:{}\n", sums[0], sums[1]).as_bytes());
    let ch = |i: usize, role: &str, f: &str| {
        json!({ "index": i, "role": role, "file": f, "frames": frames, "sha256": sums[i], "device": "default_input", "dropped_buffers": 0, "gaps": [] })
    };
    let m = json!({
        "contract": "0.1", "kind": "capture", "provider": { "id": "fake", "version": "0" },
        "started_at": "2026-10-27T09:18:22.781Z", "sample_rate": 16000, "duration_ms": frames * 1000 / 16000,
        "complete": true, "end_reason": "user", "recording_sha256": rec,
        "channels": [ch(0, "mic", "mic.wav"), ch(1, "loopback", "loopback.wav")],
    });
    std::fs::write(dir.join("capture.json"), m.to_string()).unwrap();
}

/// Under load a Python engine can take seconds to start, and a stop during start-up gives up on
/// purpose: wait until each channel has said something before finishing.
fn wait_for_engines(updates: &Arc<Mutex<Vec<(LiveUpdate, bool)>>>) {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let u = updates.lock().unwrap();
        let seen = |c: u32| u.iter().any(|(x, _)| matches!(x, LiveUpdate::Caption { channel, .. } | LiveUpdate::Error { channel, .. } if *channel == c));
        if seen(0) && seen(1) {
            return;
        }
        assert!(Instant::now() <= end, "an engine said nothing within 15 s: the test would not exercise what it claims");
        drop(u);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 8 s of audio per channel, written 100 ms at a time 4x faster than real time.
const CHUNKS: usize = 80;

fn run_session(mode: &str) -> (tempfile::TempDir, Store, String, LiveOutcome, Vec<LiveUpdate>, bool) {
    run_session_with(mode, |_| {})
}

fn run_session_with(mode: &str, tweak: impl FnOnce(&mut LiveConfig)) -> (tempfile::TempDir, Store, String, LiveOutcome, Vec<LiveUpdate>, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let bin = fake_engine(tmp.path(), mode);
    let dir = tmp.path().join("captures").join("c1");
    std::fs::create_dir_all(&dir).unwrap();
    let updates: Arc<Mutex<Vec<(LiveUpdate, bool)>>> = Arc::default();
    let writing = Arc::new(AtomicBool::new(true));
    let (u2, w2) = (updates.clone(), writing.clone());
    let on_update: OnUpdate = Arc::new(move |u| u2.lock().unwrap().push((u, w2.load(Ordering::SeqCst))));
    let mut c = cfg(bin);
    tweak(&mut c);
    let session = LiveSession::start(c, &dir, on_update);
    let writers = [write_growing(dir.join("mic.wav"), CHUNKS, Duration::from_millis(25), 50), write_growing(dir.join("loopback.wav"), CHUNKS, Duration::from_millis(25), 50)];
    for w in writers {
        w.join().unwrap(); // recording completes whatever the engine does
    }
    wait_for_engines(&updates);
    writing.store(false, Ordering::SeqCst);
    finish_manifest(&dir, CHUNKS * 1600);
    let outcome = session.finish();
    let data = tmp.path().join("data");
    let store = Store::open(&data).unwrap();
    let cap = CaptureOutcome { dir, exit_code: Some(0), killed: false, errors: vec![] };
    let (imp, _) = cap.import(&store, ("나", "상대"), None).unwrap();
    let during = updates.lock().unwrap().iter().filter(|(_, w)| *w).count() > 0;
    let all = updates.lock().unwrap().iter().map(|(u, _)| u.clone()).collect();
    (tmp, store, imp.meeting.id, outcome, all, during)
}

#[test]
fn session_end_to_end_with_a_fake_engine() {
    let (_tmp, store, id, outcome, updates, during) = run_session("ok");
    assert!(during, "captions arrived while the audio was still being written");
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Caption { channel: 0, is_final: true, .. })));
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Caption { channel: 1, is_final: false, .. })));
    assert!(!updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })));
    for c in outcome.channels.values() {
        assert!(c.complete && c.error.is_none(), "{:?}", c.error);
        assert_eq!(c.events.len(), 3, "two full segments and the flushed one");
    }
    let md = store_live(&store, &id, &outcome, CHUNK_MS).unwrap().expect("a live version");
    assert_eq!(md.version.kind, VersionKind::Live);
    assert!(md.version.id.ends_with(".live.1"));
    assert_eq!(md.words.len(), 16, "8 words per channel");
    assert!(md.words.iter().all(|w| w.end_estimated && w.speaker.is_some()));
    assert_eq!(md.coverage.len(), 2);
    let p = &md.version.producers[0];
    assert_eq!(p["role"], "asr");
    assert_eq!((p["engine"]["id"].as_str(), p["model"]["id"].as_str()), (Some("fake/stream"), Some("fake-model")));
    assert_eq!((p["word_timing"].as_str(), p["granularity"].as_str(), p["network"].as_bool()), (Some("transducer"), Some("token"), Some(false)));
    assert_eq!(p["params"]["chunk_ms"], 100);
    // stored, and valid again when read back
    let raw = store.version_raw(&md.version.id).unwrap();
    assert!(schema_errors(meeting_data_validator(), &raw).is_empty());
    assert_eq!(store.versions_of(&id).unwrap().last().unwrap().kind, VersionKind::Live);
}

#[test]
fn a_dead_engine_costs_captions_but_not_the_recording() {
    let (_tmp, store, id, outcome, updates, _) = run_session("die");
    // run_session itself proves the writers (the recording) ran to the end and the capture imported
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })), "a quiet error is reported");
    for c in outcome.channels.values() {
        assert!(!c.complete && c.error.is_some());
        assert!(c.covered_ms <= 3000, "covered only as far as the last final: {}", c.covered_ms);
    }
    let md = store_live(&store, &id, &outcome, CHUNK_MS).unwrap().expect("what was heard is kept");
    assert_eq!(md.words.len(), 6, "the first final of each channel; the unfinished partial is lost");
    assert!(md.coverage.iter().all(|c| c.end_ms <= 3000), "the rest is not claimed as transcribed");
}

#[test]
fn events_that_break_the_schema_are_dropped_and_counted() {
    let (_tmp, store, id, outcome, _, _) = run_session("bad");
    for c in outcome.channels.values() {
        assert_eq!(c.rejected, 1);
        assert_eq!(c.events.len(), 3, "the valid events still arrive");
    }
    assert!(store_live(&store, &id, &outcome, CHUNK_MS).unwrap().is_some());
}

#[test]
fn an_engine_that_cannot_start_is_a_quiet_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("c");
    std::fs::create_dir_all(&dir).unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let session = LiveSession::start(cfg(tmp.path().join("missing")), &dir, Arc::new(move |u| {
        assert!(matches!(u, LiveUpdate::Error { .. }));
        n2.fetch_add(1, Ordering::SeqCst);
    }));
    let w = write_growing(dir.join("mic.wav"), 5, Duration::from_millis(5), 1);
    let w2 = write_growing(dir.join("loopback.wav"), 5, Duration::from_millis(5), 1);
    w.join().unwrap();
    w2.join().unwrap();
    let end = Instant::now() + Duration::from_secs(5); // a stop during start-up gives up silently: let it fail first
    while n.load(Ordering::SeqCst) < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = session.finish();
    assert_eq!(n.load(Ordering::SeqCst), 2);
    assert!(out.channels.values().all(|c| c.engine.is_none() && c.error.is_some()));
}

#[test]
fn finishing_before_any_audio_exists_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let session = LiveSession::start(cfg(tmp.path().join("x")), &tmp.path().join("never"), Arc::new(|_| {}));
    let t = Instant::now();
    let out = session.finish();
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(out.channels.values().all(|c| c.engine.is_none()));
}

#[test]
fn engine_resolution_order() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("app");
    std::fs::create_dir_all(&exe_dir).unwrap();
    let pick = |dir: &Path, engine: &str| pick_engine(None, Some(dir), None, engine);
    assert!(pick(&exe_dir, "sherpa-stream").is_err());
    #[cfg(debug_assertions)]
    let dev = tmp.path().join("engines/sherpa-stream/target/debug").join(engine_bin("sherpa-stream"));
    #[cfg(not(debug_assertions))]
    let dev = tmp.path().join("dev-engine");
    std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
    std::fs::write(&dev, "").unwrap();
    assert_eq!(pick(&exe_dir, "sherpa-stream").is_ok(), cfg!(debug_assertions));
    assert!(pick(&exe_dir, "nemotron-stream").is_err(), "the other engine's binary is not this one");
    let sibling = exe_dir.join(engine_bin("sherpa-stream"));
    std::fs::write(&sibling, "").unwrap();
    assert_eq!(pick(&exe_dir, "sherpa-stream").unwrap(), sibling);
    assert!(pick(&exe_dir, "nemotron-stream").is_err());
    assert!(engine_bin("nemotron-stream").starts_with("meetko-nemotron-stream"));
    assert!(pick_engine(Some(tmp.path().join("nope")), Some(&exe_dir), None, "sherpa-stream").is_err());
}

#[test]
fn a_second_binary_of_one_engine_directory_is_found_by_its_own_name() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("app");
    std::fs::create_dir_all(&exe_dir).unwrap();
    let pick = |explicit| pick_bin(explicit, "MEETKO_DIAR_ENGINE", "the diarization engine", Some(&exe_dir), None, "nemotron-stream", &exe_name("meetko-nemotron-diar"));
    assert!(pick(None).unwrap_err().to_string().contains("MEETKO_DIAR_ENGINE"));
    // the streaming engine of the same directory is not the diarization engine
    std::fs::write(exe_dir.join(engine_bin("nemotron-stream")), "").unwrap();
    assert!(pick(None).is_err());
    #[cfg(debug_assertions)]
    {
        let dev = tmp.path().join("engines/nemotron-stream/target/debug").join(exe_name("meetko-nemotron-diar"));
        std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
        std::fs::write(&dev, "").unwrap();
        assert_eq!(pick(None).unwrap(), dev);
    }
    let sibling = exe_dir.join(exe_name("meetko-nemotron-diar"));
    std::fs::write(&sibling, "").unwrap();
    assert_eq!(pick(None).unwrap(), sibling);
    assert!(pick(Some(tmp.path().join("nope"))).is_err());
}

#[test]
fn model_directory_check_names_the_missing_file_and_refuses_unpinned() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache.json");
    assert!(check_model(&tmp.path().join("none"), true, &cache).is_err());
    let missing = check_model(tmp.path(), true, &cache).unwrap_err().to_string();
    assert!(missing.contains("encoder") && missing.contains("model.q8_0.gguf"), "it lists the files of every known model: {missing}");
    for f in model_files("sherpa-stream") {
        std::fs::write(tmp.path().join(f), "").unwrap();
    }
    let e = check_model(tmp.path(), false, &cache).unwrap_err();
    assert!(e.to_string().contains("pinned") && e.to_string().contains("UNPINNED"), "{e}");
    let ok = check_model(tmp.path(), true, &cache).unwrap();
    assert!(!ok.pinned);
    assert_eq!(ok.engine, "sherpa-stream");
    assert!(cache.exists(), "hashes are cached");
    #[cfg(unix)]
    assert_eq!(std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(check_model(tmp.path(), true, &cache).unwrap().digest, ok.digest, "the cached digest is the same");
}

#[test]
fn the_pinned_model_is_recognised_and_digests_are_canonical() {
    for pin in pins() {
        assert_eq!(pinned_model(&pin.files), Some(pin.id.as_str()));
        let mut changed = pin.files.clone();
        let first = changed.keys().next().unwrap().clone();
        changed.insert(first, "0".repeat(64));
        assert!(pinned_model(&changed).is_none(), "one differing file is an unpinned model");
    }
    // lines "<file>:<sha256>\n" sorted by file name
    let mut m = BTreeMap::new();
    m.insert("b".to_string(), "2".to_string());
    m.insert("a".to_string(), "1".to_string());
    assert_eq!(model_digest(&m), crate::util::sha256_bytes(b"a:1\nb:2\n"));
}

#[test]
fn staging_copies_the_model_and_reports_what_it_hashed() {
    let tmp = tempfile::tempdir().unwrap();
    let c = cfg(tmp.path().join("e"));
    let staged = stage_model(&c.model_dir, true, &|| false).unwrap();
    assert!(!staged.check.pinned);
    assert_ne!(staged.path, c.model_dir);
    for f in model_files("sherpa-stream") {
        assert_eq!(std::fs::read(staged.path.join(&f)).unwrap(), f.as_bytes());
    }
    assert!(stage_model(&c.model_dir, false, &|| false).is_err());
    assert!(matches!(stage_model(&c.model_dir, true, &|| true), Err(Error::Cancelled)));
}

#[test]
fn the_model_directory_picks_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache.json");
    let nemotron = tmp.path().join("nemotron");
    std::fs::create_dir(&nemotron).unwrap();
    std::fs::write(nemotron.join("model.q8_0.gguf"), "weights").unwrap();
    let c = check_model(&nemotron, true, &cache).unwrap();
    assert_eq!((c.engine, c.pinned), ("nemotron-stream", false));
    assert!(check_model(&nemotron, false, &cache).is_err(), "dummy weights are not the pinned model");
    let staged = stage_model(&nemotron, true, &|| false).unwrap();
    assert_eq!(std::fs::read(staged.path.join("model.q8_0.gguf")).unwrap(), b"weights");
    assert_eq!(staged.check.digest, c.digest);
    assert_eq!(pins().iter().map(|p| (p.engine, p.role.as_str())).collect::<Vec<_>>(), [("nemotron-stream", ROLE_STREAM), ("nemotron-stream", ROLE_DIARIZATION), ("sherpa-stream", ROLE_STREAM)]);
}

#[test]
fn a_role_only_knows_the_models_of_its_own_engines() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache.json");
    let diar = tmp.path().join("diar");
    std::fs::create_dir(&diar).unwrap();
    std::fs::write(diar.join("Nemotron-3-Diarization.q8_0.gguf"), "weights").unwrap();
    // the diarization model is a diarization model, not a streaming one, and the other way round
    let c = check_model_for(ROLE_DIARIZATION, &diar, true, &cache).unwrap();
    assert_eq!((c.engine, c.pinned), ("nemotron-stream", false));
    let e = check_model_for(ROLE_DIARIZATION, &diar, false, &cache).unwrap_err().to_string();
    assert!(e.contains("diarization model") && e.contains("MEETKO_DIAR_MODEL_UNPINNED"), "{e}");
    let e = check_model(&diar, true, &cache).unwrap_err().to_string();
    assert!(e.contains("streaming model") && e.contains("none of the known"), "{e}");
    let stream = tmp.path().join("stream");
    std::fs::create_dir(&stream).unwrap();
    std::fs::write(stream.join("model.q8_0.gguf"), "weights").unwrap();
    assert!(check_model_for(ROLE_DIARIZATION, &stream, true, &cache).unwrap_err().to_string().contains("diarization model"));
    // staging goes by the same role, and a shared stage remembers it
    let staged = SharedModel::new(ROLE_DIARIZATION).get(&diar, true, &|| false).unwrap();
    assert_eq!(std::fs::read(staged.path.join("Nemotron-3-Diarization.q8_0.gguf")).unwrap(), b"weights");
    assert!(SharedModel::new(ROLE_DIARIZATION).get(&stream, true, &|| false).is_err());
}

#[test]
fn the_pinned_diarization_model_is_the_benchmarked_one() {
    let pin = pins().iter().find(|p| p.role == ROLE_DIARIZATION).unwrap();
    assert_eq!(pin.id, "nemotron-3-diarization-q8_0");
    assert_eq!(pin.files.get("Nemotron-3-Diarization.q8_0.gguf").map(String::as_str), Some("08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1"));
}

#[test]
fn the_diarization_engine_manifest_provides_diarization() {
    let m = Manifest::parse(include_str!("../../../../engines/nemotron-stream/meetko-diar.toml")).unwrap();
    assert_eq!((m.package.kind.as_str(), m.role.unwrap().provides.as_str()), ("engine", "diarization"));
    assert!(!m.permissions.network);
}

#[test]
fn the_engine_manifests_are_asr_stream_engines() {
    for text in [include_str!("../../../../engines/sherpa-stream/meetko.toml"), include_str!("../../../../engines/nemotron-stream/meetko.toml")] {
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.package.kind, "engine");
        assert_eq!(m.role.unwrap().provides, "asr.stream");
        assert!(!m.permissions.network);
    }
}

#[test]
fn a_slow_engine_catches_up_without_dying_or_losing_audio() {
    // a 4-message queue and an engine that needs 30 ms per chunk: the queue is full most of the time
    let (_tmp, store, id, outcome, updates, _) = run_session_with("slow", |c| c.queue = 4);
    assert!(!updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })));
    for c in outcome.channels.values() {
        assert!(c.complete && c.error.is_none(), "{:?}", c.error);
        assert_eq!(c.events.len(), 3, "every second of audio still reached the engine");
    }
    assert_eq!(store_live(&store, &id, &outcome, CHUNK_MS).unwrap().unwrap().words.len(), 16);
}

#[test]
fn an_engine_that_announces_a_fault_is_a_failed_engine() {
    let (_tmp, _s, _id, outcome, updates, _) = run_session("fault");
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })));
    for c in outcome.channels.values() {
        assert!(!c.complete);
        assert!(c.error.as_deref().is_some_and(|e| e.contains("invalid PCM")), "{:?}", c.error);
    }
}

#[test]
fn a_flooding_engine_is_cut_off() {
    let t = Instant::now();
    let (_tmp, _s, _id, outcome, updates, _) = run_session("flood");
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })));
    for c in outcome.channels.values() {
        assert!(!c.complete);
        assert!(c.error.as_deref().is_some_and(|e| e.contains("events per second") || e.contains("segments")), "{:?}", c.error);
        assert!(c.events.len() <= MAX_SEGMENTS + 1, "the folded state stays bounded");
    }
    assert!(t.elapsed() < Duration::from_secs(20));
}

/// The guard can trip after the feed loop is over (pushes only enqueue): the channel must still
/// not be stored as complete.
#[test]
fn a_flood_that_trips_after_feeding_ended_is_not_complete() {
    let (_tmp, _s, _id, outcome, updates, _) = run_session("flood_late");
    assert!(updates.iter().any(|u| matches!(u, LiveUpdate::Error { .. })));
    for c in outcome.channels.values() {
        assert!(!c.complete, "a flooded channel must not claim full coverage");
        assert!(c.error.as_deref().is_some_and(|e| e.contains("events per second") || e.contains("segments")), "{:?}", c.error);
    }
}

#[test]
fn an_event_with_too_many_units_is_rejected() {
    let (_tmp, _s, _id, outcome, _, _) = run_session("units");
    for c in outcome.channels.values() {
        assert_eq!(c.rejected, 1);
        assert!(!c.events.iter().any(|e| e["segment"] == 99));
    }
}

#[test]
fn rate_and_size_guards() {
    let mut g = Guard::default();
    let t0 = Instant::now();
    // a burst inside one second is fine
    for i in 0..500 {
        assert!(g.rate(t0 + Duration::from_micros(i)).is_ok());
    }
    // a sustained flood (over the limit in two consecutive seconds) is not
    let mut g = Guard::default();
    let mut failed = false;
    for sec in 0..3u64 {
        for i in 0..300u64 {
            if g.rate(t0 + Duration::from_millis(sec * 1000 + i)).is_err() {
                failed = true;
            }
        }
    }
    assert!(failed);
    // a steady 100/s never fails
    let mut g = Guard::default();
    for i in 0..1000u64 {
        assert!(g.rate(t0 + Duration::from_millis(i * 10)).is_ok());
    }
    let mut f = Folder::default();
    for i in 0..=MAX_SEGMENTS as i64 {
        f.apply(&json!({ "segment": i, "revision": 0, "final": false, "text": "x", "units": [] }));
    }
    assert!(Guard::size(&f).is_err());
}

#[test]
fn a_stop_during_model_load_returns_quickly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("c");
    std::fs::create_dir_all(&dir).unwrap();
    let session = LiveSession::start(cfg(fake_engine(tmp.path(), "hang_open")), &dir, Arc::new(|_| {}));
    let w = write_growing(dir.join("mic.wav"), 5, Duration::from_millis(5), 1);
    let w2 = write_growing(dir.join("loopback.wav"), 5, Duration::from_millis(5), 1);
    w.join().unwrap();
    w2.join().unwrap();
    std::thread::sleep(Duration::from_millis(500)); // the engines are now stuck in stream.open
    let t = Instant::now();
    let out = session.finish();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(out.channels.values().all(|c| !c.complete));
}

#[test]
fn the_finish_deadline_keeps_what_was_folded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("captures").join("c1");
    std::fs::create_dir_all(&dir).unwrap();
    let seen: Arc<Mutex<Vec<(LiveUpdate, bool)>>> = Arc::default();
    let s2 = seen.clone();
    let session = LiveSession::start(cfg(fake_engine(tmp.path(), "stuck_close")), &dir, Arc::new(move |u| s2.lock().unwrap().push((u, true))));
    let ws = [write_growing(dir.join("mic.wav"), CHUNKS, Duration::from_millis(25), 50), write_growing(dir.join("loopback.wav"), CHUNKS, Duration::from_millis(25), 50)];
    for w in ws {
        w.join().unwrap();
    }
    wait_for_engines(&seen);
    let t = Instant::now();
    let out = session.finish_within(Duration::from_secs(2)); // stream.close never answers
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    for c in out.channels.values() {
        assert!(!c.complete);
        assert_eq!(c.events.iter().filter(|e| e["final"] == true).count(), 2, "the finals before the close were kept");
        assert_eq!(c.covered_ms, 6000, "covered only as far as the last final");
    }
}

#[test]
fn a_truncated_or_replaced_channel_file_is_an_input_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("a.wav");
    let mut data = header(0);
    data.extend(vec![1u8; 4000]);
    std::fs::write(&p, &data).unwrap();
    let mut t = WavTail::open(&p).unwrap();
    assert_eq!(t.read_new().unwrap().len(), 4000);
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_len(1000).unwrap();
    assert!(t.read_new().unwrap_err().to_string().contains("shrank"));
    // replaced: a new file under the same name
    let mut t = WavTail::open(&p).unwrap();
    let q = tmp.path().join("b.wav");
    std::fs::write(&q, &data).unwrap();
    std::fs::rename(&q, &p).unwrap();
    assert!(t.read_new().unwrap_err().to_string().contains("replaced"));
}

#[test]
fn an_input_failure_keeps_captions_and_stops_coverage_there() {
    // the file is cut while the capture is still "running"; the engine is healthy and flushes
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("c");
    std::fs::create_dir_all(&dir).unwrap();
    let seen: Arc<Mutex<Vec<(LiveUpdate, bool)>>> = Arc::default();
    let s2 = seen.clone();
    let session = LiveSession::start(cfg(fake_engine(tmp.path(), "ok")), &dir, Arc::new(move |u| s2.lock().unwrap().push((u, true))));
    let mut body = header(0);
    body.extend((0..16000 * 4 * 2).map(|i| (i % 7) as u8)); // 4 s
    for n in ["mic.wav", "loopback.wav"] {
        std::fs::write(dir.join(n), &body).unwrap();
    }
    wait_for_engines(&seen);
    for n in ["mic.wav", "loopback.wav"] {
        std::fs::OpenOptions::new().write(true).open(dir.join(n)).unwrap().set_len(100).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    let out = session.finish();
    for c in out.channels.values() {
        assert!(!c.complete && c.error.as_deref().is_some_and(|e| e.contains("shrank")), "{:?}", c.error);
        assert_eq!(c.covered_ms, 4000, "what the engine was given");
    }
}

#[test]
fn a_partial_model_directory_names_the_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache.json");
    let files = model_files("sherpa-stream");
    std::fs::write(tmp.path().join(&files[0]), "").unwrap();
    let e = check_model(tmp.path(), true, &cache).unwrap_err().to_string();
    assert!(e.contains(&files[1]) && e.contains("is missing") && !e.contains("none of the known"), "{e}");
}

#[test]
fn channels_share_one_staged_model_and_its_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let c = cfg(tmp.path().join("e"));
    let shared = SharedModel::default();
    let (a, b) = (shared.get(&c.model_dir, true, &|| false).unwrap(), shared.get(&c.model_dir, true, &|| false).unwrap());
    assert!(Arc::ptr_eq(&a, &b), "staged once");
    let dir = a.path.clone();
    assert!(dir.exists());
    drop((a, b));
    assert!(dir.exists(), "the shared cell still holds it");
    drop(shared);
    assert!(!dir.exists(), "gone with the last holder");
    // a refusal is remembered; a cancellation is not
    let shared = SharedModel::default();
    assert!(shared.get(&c.model_dir, false, &|| false).is_err());
    assert!(shared.get(&c.model_dir, true, &|| false).is_err(), "same verdict, no second copy");
    let fresh = SharedModel::default();
    assert!(matches!(fresh.get(&c.model_dir, true, &|| true), Err(Error::Cancelled)));
    assert!(fresh.get(&c.model_dir, true, &|| false).is_ok());
}
