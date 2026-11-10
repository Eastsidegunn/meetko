//! Real smoke test of live transcription: a real capture provider, the real engine, and a wav
//! played through the speakers so the loopback channel hears it.
//!
//!   MEETKO_STREAM_ENGINE=... MEETKO_STREAM_MODEL=... MEETKO_CAPTURE_BIN=... \
//!     cargo run --release -p meetko-core --example live_smoke -- <wav to play>
//!
//! Prints whether captions arrived while the audio was playing, the latency of every token
//! (wall-clock of its first appearance minus its audio time) and checks the stored live version.
//! Uses a throw-away data dir; nothing of the user's is touched.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meetko_core::capture_run::{self, CaptureEvent, CaptureSession};
use meetko_core::contract::{meeting_data_validator, schema_errors};
use meetko_core::live::{store_live, LiveConfig, LiveSession, LiveUpdate, CHUNK_MS};
use meetko_core::store::Store;

fn pct(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((s.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let wav = std::env::args().nth(1).expect("usage: live_smoke <wav to play>");
    let data = tempfile::tempdir().unwrap();
    let bin = capture_run::resolve_bin(data.path()).expect("capture provider");
    let cfg = LiveConfig::from_env().expect("live config");

    let t0: Arc<Mutex<Option<Instant>>> = Arc::default();
    let t0c = t0.clone();
    let session = CaptureSession::start(&bin, data.path(), move |e| {
        if matches!(e, CaptureEvent::Started { .. }) {
            *t0c.lock().unwrap() = Some(Instant::now()); // the timeline's zero is fixed just before this event
        }
        if let CaptureEvent::Error { code, message, .. } = e {
            eprintln!("capture error {code}: {message}");
        }
    })
    .expect("capture start");

    // (channel, unit start_ms) -> seconds since t0 of the first event that contained the unit
    let first: Arc<Mutex<BTreeMap<(u32, i64), f64>>> = Arc::default();
    let arrivals: Arc<Mutex<Vec<(u32, bool, f64)>>> = Arc::default();
    let (f2, a2, t02) = (first.clone(), arrivals.clone(), t0.clone());
    let tap = Arc::new(move |ch: u32, ev: &serde_json::Value| {
        let Some(t0) = *t02.lock().unwrap() else { return };
        let now = t0.elapsed().as_secs_f64();
        a2.lock().unwrap().push((ch, ev["final"] == true, now));
        for u in ev["units"].as_array().into_iter().flatten() {
            f2.lock().unwrap().entry((ch, u["start_ms"].as_i64().unwrap())).or_insert(now);
        }
    });
    let errors: Arc<Mutex<Vec<LiveUpdate>>> = Arc::default();
    let e2 = errors.clone();
    let live = LiveSession::start_tapped(
        cfg,
        session.out_dir(),
        Arc::new(move |u| {
            if matches!(u, LiveUpdate::Error { .. }) {
                e2.lock().unwrap().push(u);
            }
        }),
        Some(tap),
    );

    std::thread::sleep(Duration::from_secs(2)); // engines load, recording settles
    let play_start = t0.lock().unwrap().expect("capture started").elapsed().as_secs_f64();
    let status = Command::new("afplay").arg(&wav).status().expect("afplay");
    let play_end = t0.lock().unwrap().unwrap().elapsed().as_secs_f64();
    assert!(status.success());
    std::thread::sleep(Duration::from_secs(3)); // let the last words trail out

    let stop_began = Instant::now();
    let outcome = session.stop(capture_run::STOP_TIMEOUT).expect("capture stop");
    let stop_at = t0.lock().unwrap().unwrap().elapsed().as_secs_f64();
    let live_out = live.finish();
    let drain = t0.lock().unwrap().unwrap().elapsed().as_secs_f64() - stop_at;
    println!("playback {play_start:.1}s..{play_end:.1}s on the capture timeline; live drain after the provider stopped {drain:.2}s; whole stop (provider + drain) {:.2}s", stop_began.elapsed().as_secs_f64());

    let arr = arrivals.lock().unwrap().clone();
    for ch in [0u32, 1] {
        let during = arr.iter().filter(|(c, _, t)| *c == ch && *t >= play_start && *t <= play_end).count();
        let total = arr.iter().filter(|(c, _, _)| *c == ch).count();
        let finals_during = arr.iter().filter(|(c, f, t)| *c == ch && *f && *t >= play_start && *t <= play_end).count();
        println!("channel {ch}: {total} events, {during} during playback ({finals_during} finals)");
    }
    let lat: Vec<f64> = first.lock().unwrap().iter().filter(|((c, _), _)| *c == 1).map(|((_, ms), t)| t - *ms as f64 / 1000.0).collect();
    if !lat.is_empty() {
        println!("loopback token latency over {} tokens: p50 {:.2}s  p95 {:.2}s  max {:.2}s  min {:.2}s", lat.len(), pct(&lat, 0.5), pct(&lat, 0.95), pct(&lat, 1.0), pct(&lat, 0.0));
    }
    for u in errors.lock().unwrap().iter() {
        println!("quiet error: {u:?}");
    }
    for c in live_out.channels.values() {
        println!("channel {} complete={} segments={} rejected={} error={:?}", c.index, c.complete, c.events.len(), c.rejected, c.error.as_ref().map(|e| e.len()));
    }

    let store = Store::open(data.path()).unwrap();
    let done = outcome.import(&store, ("나", "상대"), None).expect("import");
    let md = store_live(&store, &done.0.meeting.id, &live_out, CHUNK_MS).expect("store_live").expect("a live version");
    let raw = store.version_raw(&md.version.id).unwrap();
    let errs = schema_errors(meeting_data_validator(), &raw);
    println!("stored {} kind={:?}: {} words, schema errors: {}", md.version.id, md.version.kind, md.words.len(), errs.len());
    for ch in [0u32, 1] {
        let t: Vec<&str> = md.words.iter().filter(|w| w.channel == ch).map(|w| w.text.as_str()).collect();
        println!("channel {ch} text: {}", t.join(" "));
    }
}
