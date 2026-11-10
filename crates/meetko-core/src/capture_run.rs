//! Run a capture provider (docs/capture-interface.md) as a child process: record into a fresh
//! directory under the data dir, parse its NDJSON events, stop it by the `stop` line (then a
//! kill after a timeout) and hand the result to the existing capture import (`capture::load`).
//! No UI here: callers get typed events through a callback. Labels such as "나" stay with the app.
//!
//! This is `capture_run`, not part of `capture`, because `capture` is the manifest validator.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::capture;
use crate::error::{Error, Result};
use crate::store::{ImportOutcome, Store};
use crate::util;

/// The provider's own stop watchdog is 5 s; leave room for hashing a long recording.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(windows)]
const DEFAULT_BIN: &str = "meetko-capture-win.exe";
#[cfg(not(windows))]
const DEFAULT_BIN: &str = "meetko-capture-mac";

/// The dev build of the reference provider, relative to the repository root.
/// Debug builds only: a release app must never run a binary found by walking directories.
#[cfg(debug_assertions)]
const DEV_BIN: &str = "capture/macos/.build/release/meetko-capture-mac";

/// One provider event. Unknown events and fields are ignored, as the interface requires.
/// Device names never appear in events (the provider promises it); none are kept here.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CaptureEvent {
    Started { sample_rate: Option<u32>, started_at: Option<String> },
    Level { channel: u32, rms: f64, at_ms: i64 },
    Gap { channel: u32, start_ms: i64, end_ms: i64, reason: String, kind: String },
    Error { code: String, message: String, recoverable: bool },
    Stopped,
}

/// A fatal or recoverable error the provider reported; the message carries the exact fix text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

/// `None` for a line that is not JSON or an event the core does not know.
pub fn parse_event(line: &str) -> Option<CaptureEvent> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let i = |k: &str| v.get(k).and_then(Value::as_f64).map(|f| f as i64);
    Some(match v.get("event")?.as_str()? {
        "started" => CaptureEvent::Started { sample_rate: i("sample_rate").map(|n| n as u32), started_at: s("started_at") },
        "level" => CaptureEvent::Level {
            channel: i("channel")? as u32,
            rms: v.get("rms").and_then(Value::as_f64)?.clamp(0.0, 1.0),
            at_ms: i("at_ms").unwrap_or(0),
        },
        "gap" => CaptureEvent::Gap {
            channel: i("channel")? as u32,
            start_ms: i("start_ms")?,
            end_ms: i("end_ms")?,
            reason: s("reason").unwrap_or_default(),
            kind: s("kind").unwrap_or_default(),
        },
        "error" => CaptureEvent::Error {
            code: s("code").unwrap_or_else(|| "unknown".into()),
            message: s("message").unwrap_or_default(),
            recoverable: v.get("recoverable").and_then(Value::as_bool).unwrap_or(false),
        },
        "stopped" => CaptureEvent::Stopped,
        _ => return None,
    })
}

/// Where the provider binary is: `explicit` (env `MEETKO_CAPTURE_BIN`), else a sibling of the app
/// executable, else (debug builds only) the dev build under a parent of the executable or of the
/// working directory. Never read from config.toml: a plugin can write that file, and a path in it
/// would run at the next recording. Pure over its inputs so it can be tested.
pub fn pick_bin(explicit: Option<PathBuf>, exe_dir: Option<&Path>, cwd: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return if p.is_file() { Ok(p) } else { Err(Error::Capture(format!("MEETKO_CAPTURE_BIN does not point to a file: {}", p.display()))) };
    }
    if let Some(p) = exe_dir.map(|d| d.join(DEFAULT_BIN)).filter(|p| p.is_file()) {
        return Ok(p);
    }
    // a release build must not run whatever sits under the working directory
    #[cfg(debug_assertions)]
    for root in [exe_dir, cwd].into_iter().flatten() {
        if let Some(p) = root.ancestors().map(|a| a.join(DEV_BIN)).find(|p| p.is_file()) {
            return Ok(p);
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = cwd;
    Err(Error::Capture(format!("no capture provider found: set MEETKO_CAPTURE_BIN or put {DEFAULT_BIN} next to the app")))
}

/// `pick_bin` with the real environment (`_data_dir` is unused on purpose: see `pick_bin`).
pub fn resolve_bin(_data_dir: &Path) -> Result<PathBuf> {
    let from_env = std::env::var_os("MEETKO_CAPTURE_BIN").filter(|v| !v.is_empty()).map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    pick_bin(from_env, exe.as_deref().and_then(Path::parent), std::env::current_dir().ok().as_deref())
}

pub struct CaptureSession {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: Option<JoinHandle<Vec<ProviderError>>>,
    out_dir: PathBuf,
    /// the provider's private HOME; removed when the session ends
    _home: tempfile::TempDir,
}

/// What a finished session leaves behind.
#[derive(Debug)]
pub struct CaptureOutcome {
    pub dir: PathBuf,
    pub exit_code: Option<i32>,
    /// the provider ignored `stop` and was killed; its manifest may be provisional
    pub killed: bool,
    pub errors: Vec<ProviderError>,
}

impl CaptureSession {
    /// Spawn `<bin> record --out <fresh dir under data_dir/captures>`. `on_event` runs on the
    /// reader thread for every parsed event.
    pub fn start(bin: &Path, data_dir: &Path, on_event: impl Fn(CaptureEvent) + Send + 'static) -> Result<Self> {
        let parent = data_dir.join("captures");
        util::create_private_dir(&parent)?;
        // the provider creates the dir itself (`--out` must not exist or be empty); the pid and
        // a counter keep two starts within one second apart
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let out_dir = parent.join(format!("{}-{}-{n}", util::now_rfc3339().replace(':', ""), std::process::id()));
        let home = tempfile::Builder::new().prefix("meetko-capture-").tempdir()?;
        let mut cmd = Command::new(bin);
        util::sandbox_env(&mut cmd, home.path());
        cmd.arg("record").arg("--out").arg(&out_dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // its own process group: a Ctrl-C meant for the app does not end the recording half-way,
        // and `kill_tree` reaches everything it started
        util::own_process_group(&mut cmd);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW; stop goes through stdin
        }
        let mut child = cmd.spawn().map_err(|e| Error::Capture(format!("cannot start the capture provider: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| Error::Capture("no stdout from the provider".into()))?;
        let reader = std::thread::spawn(move || {
            let mut errors = Vec::new();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(ev) = parse_event(&line) {
                    if let CaptureEvent::Error { code, message, recoverable } = &ev {
                        errors.push(ProviderError { code: code.clone(), message: message.clone(), recoverable: *recoverable });
                    }
                    on_event(ev);
                }
            }
            errors
        });
        Ok(CaptureSession { child, stdin, reader: Some(reader), out_dir, _home: home })
    }

    /// The directory the provider writes into; the live session tails the channel files there.
    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// False once the provider exited by itself (a failed start, `stream_lost`).
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ask the provider to stop and wait for it; kill it when it does not exit in `timeout`.
    pub fn stop(mut self, timeout: Duration) -> Result<CaptureOutcome> {
        if let Some(mut stdin) = self.stdin.take() {
            // a provider that already exited has closed the pipe: nothing to tell it
            let _ = stdin.write_all(b"stop\n").and_then(|_| stdin.flush());
        }
        let deadline = Instant::now() + timeout;
        let (mut killed, mut status) = (false, None);
        loop {
            match self.child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    util::kill_tree(&mut self.child);
                    killed = true;
                    break;
                }
            }
        }
        // stdout closes with the process; a straggler holding it open would block the join
        let errors = self.reader.take().map(|h| h.join().unwrap_or_default()).unwrap_or_default();
        Ok(CaptureOutcome { dir: self.out_dir.clone(), exit_code: status.and_then(|s| s.code()), killed, errors })
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // dropped without `stop` (app closing): do not leave a recorder running unseen
        if matches!(self.child.try_wait(), Ok(None)) {
            util::kill_tree(&mut self.child);
        }
    }
}

impl CaptureOutcome {
    /// Import the recording. A provisional manifest is accepted (the data is worth keeping) and
    /// says so in the warnings. `labels` are the speaker labels for (mic, loopback).
    pub fn import(&self, store: &Store, labels: (&str, &str), title: Option<String>) -> Result<(ImportOutcome, Vec<String>)> {
        if !self.dir.join("capture.json").is_file() {
            return Err(Error::Capture(self.errors.first().map_or_else(|| "the provider wrote no recording".into(), |e| e.message.clone())));
        }
        let mut c = capture::load(&self.dir, true)?;
        if c.options.duration_ms.unwrap_or(0) <= 0 {
            return Err(Error::Capture("nothing was recorded".into()));
        }
        for ch in &mut c.channels {
            ch.label = Some(if ch.role == "mic" { labels.0 } else { labels.1 }.to_string());
        }
        c.options.title = title;
        let mut warnings = c.warnings;
        if self.killed {
            warnings.push("the provider did not stop in time and was killed".into());
        }
        let imported = store.import_meeting(c.channels, c.options)?;
        // the audio now lives under recordings/: the provider's copy would only be a second place
        // to leave it. Best effort, and only for a dir this runner made (under `captures/`).
        if self.dir.parent().and_then(Path::file_name).is_some_and(|n| n == "captures") {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        Ok((imported, warnings))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    /// A fake provider in Python (hashlib/wave give a valid manifest in a few lines).
    /// `mode`: "ok" records 1 s on stop; "perm" fails to start with a permission error;
    /// "hang" ignores stop.
    fn fake(dir: &Path, mode: &str) -> PathBuf {
        let src = r#"#!/usr/bin/env python3
import hashlib, json, os, sys, wave
mode = "MODE"
out = sys.argv[sys.argv.index("--out") + 1]
def emit(o):
    print(json.dumps(o), flush=True)
os.makedirs(out)
if mode == "env":
    json.dump(dict(os.environ), open(os.path.join(os.path.dirname(out), "env.json"), "w"))
def finish(frames, reason):
    sums = []
    for name in ("mic", "loopback"):
        with wave.open(os.path.join(out, name + ".wav"), "wb") as w:
            w.setnchannels(1); w.setsampwidth(2); w.setframerate(16000)
            w.writeframes(b"\0\0" * frames)
        sums.append(hashlib.sha256(open(os.path.join(out, name + ".wav"), "rb").read()).hexdigest())
    rec = hashlib.sha256(("0:%s\n1:%s\n" % tuple(sums)).encode()).hexdigest()
    ch = lambda i, role, f, s, dev: {"index": i, "role": role, "file": f, "frames": frames, "sha256": s, "device": dev, "dropped_buffers": 0, "gaps": []}
    json.dump({"contract": "0.1", "kind": "capture", "provider": {"id": "fake", "version": "0"},
               "started_at": "2026-10-27T09:18:22.781Z", "sample_rate": 16000, "duration_ms": frames * 1000 // 16000,
               "complete": True, "end_reason": reason, "recording_sha256": rec,
               "channels": [ch(0, "mic", "mic.wav", sums[0], "default_input"), ch(1, "loopback", "loopback.wav", sums[1], "system_output")]},
              open(os.path.join(out, "capture.json"), "w"))
if mode == "perm":
    emit({"event": "error", "code": "permission_microphone", "message": "Enable Microphone for your terminal in System Settings"})
    finish(0, "error")
    sys.exit(2)
emit({"event": "started", "sample_rate": 16000, "started_at": "2026-10-27T09:18:22.781Z", "mic_device": "default_input"})
emit({"event": "level", "channel": 0, "rms": 0.25, "frames": 16000, "at_ms": 1000})
emit({"event": "level", "channel": 1, "rms": 0.0, "frames": 16000, "at_ms": 1000})
emit({"event": "mystery", "x": 1})
if mode == "hang":
    while True:
        sys.stdin.readline()
        import time; time.sleep(0.05)
for line in sys.stdin:
    if line.strip() == "stop":
        break
finish(16000, "user")
emit({"event": "stopped", "manifest": "capture.json"})
"#;
        let p = dir.join(format!("fake-{mode}.py"));
        std::fs::write(&p, src.replace("MODE", mode)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn collect() -> (Arc<Mutex<Vec<CaptureEvent>>>, impl Fn(CaptureEvent) + Send + 'static) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        (seen, move |e| s2.lock().unwrap().push(e))
    }

    #[test]
    fn records_stops_and_imports() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake(tmp.path(), "ok");
        let data = tmp.path().join("data");
        let (seen, cb) = collect();
        let mut s = CaptureSession::start(&bin, &data, cb).unwrap();
        assert!(s.is_running());
        std::thread::sleep(Duration::from_millis(300));
        let out = s.stop(Duration::from_secs(10)).unwrap();
        assert!(!out.killed);
        assert_eq!(out.exit_code, Some(0));
        let ev = seen.lock().unwrap().clone();
        assert!(matches!(ev.first(), Some(CaptureEvent::Started { sample_rate: Some(16000), .. })));
        assert!(ev.contains(&CaptureEvent::Level { channel: 0, rms: 0.25, at_ms: 1000 }));
        assert_eq!(ev.last(), Some(&CaptureEvent::Stopped));
        assert!(out.dir.starts_with(data.join("captures")));
        assert!(out.dir.join("capture.json").exists());
        let store = Store::open(&data).unwrap();
        let (imp, warnings) = out.import(&store, ("나", "상대"), Some("t".into())).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(imp.meeting.duration_ms, 1000);
        let files = store.channel_files(&imp.meeting.id).unwrap();
        assert_eq!(files[0].label.as_deref(), Some("나"));
        assert_eq!(files[1].label.as_deref(), Some("상대"));
        assert!(!out.dir.exists(), "the provider's copy is removed after a successful import");
    }

    #[test]
    fn the_provider_does_not_inherit_the_environment() {
        if !util::in_secret_child("capture_run::tests::the_provider_does_not_inherit_the_environment") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake(tmp.path(), "env");
        let s = CaptureSession::start(&bin, tmp.path(), |_| {}).unwrap();
        let dump = tmp.path().join("captures/env.json"); // written once the fake has started
        let end = Instant::now() + Duration::from_secs(30);
        while !dump.exists() {
            assert!(Instant::now() < end, "the fake provider never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100)); // the dump is written in one go; let it finish
        s.stop(Duration::from_secs(10)).unwrap();
        let env: serde_json::Value = serde_json::from_slice(&std::fs::read(&dump).unwrap()).unwrap();
        let keys: Vec<&String> = env.as_object().unwrap().keys().collect();
        assert!(!keys.iter().any(|k| k.starts_with("MEETKO_TEST")), "the secret reached the provider: {keys:?}");
        assert!(env.get("PATH").is_some(), "PATH is on the allowlist");
        assert_ne!(env["HOME"], std::env::var("HOME").unwrap(), "HOME is private to the run");
    }

    /// Needs the built macOS provider and microphone/system-audio permission for the terminal:
    /// `cargo test -p meetko-core real_provider -- --ignored`.
    #[test]
    #[ignore]
    fn real_provider_records_with_the_minimal_environment() {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../capture/macos/.build/release/meetko-capture-mac");
        let tmp = tempfile::tempdir().unwrap();
        let (seen, cb) = collect();
        let s = CaptureSession::start(&bin, tmp.path(), cb).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let out = s.stop(Duration::from_secs(15)).unwrap();
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(!out.killed && out.exit_code == Some(0), "{:?}", out.exit_code);
        let m: serde_json::Value = serde_json::from_slice(&std::fs::read(out.dir.join("capture.json")).unwrap()).unwrap();
        assert!(m["duration_ms"].as_i64().unwrap() > 1000, "{m}");
        assert!(seen.lock().unwrap().iter().any(|e| matches!(e, CaptureEvent::Started { .. })));
    }

    #[test]
    fn permission_error_is_reported_with_its_message() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake(tmp.path(), "perm");
        let (seen, cb) = collect();
        let mut s = CaptureSession::start(&bin, tmp.path(), cb).unwrap();
        while s.is_running() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = s.stop(Duration::from_secs(5)).unwrap();
        assert_eq!(out.exit_code, Some(2));
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].code, "permission_microphone");
        assert!(out.errors[0].message.contains("System Settings"));
        assert!(matches!(seen.lock().unwrap()[0], CaptureEvent::Error { .. }));
        let store = Store::open(&tmp.path().join("data")).unwrap();
        let e = out.import(&store, ("a", "b"), None).err().unwrap();
        assert!(e.to_string().contains("nothing was recorded"), "{e}");
    }

    #[test]
    fn a_provider_that_ignores_stop_is_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake(tmp.path(), "hang");
        let s = CaptureSession::start(&bin, tmp.path(), |_| {}).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        let out = s.stop(Duration::from_millis(500)).unwrap();
        assert!(out.killed);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(!out.dir.join("capture.json").exists());
    }

    #[test]
    fn parses_known_events_and_ignores_the_rest() {
        assert_eq!(
            parse_event(r#"{"event":"gap","channel":1,"start_ms":5,"end_ms":9,"reason":"no_data","kind":"no_data","extra":1}"#),
            Some(CaptureEvent::Gap { channel: 1, start_ms: 5, end_ms: 9, reason: "no_data".into(), kind: "no_data".into() })
        );
        assert_eq!(parse_event(r#"{"event":"level","channel":0,"rms":7,"at_ms":1}"#), Some(CaptureEvent::Level { channel: 0, rms: 1.0, at_ms: 1 }));
        assert_eq!(parse_event(r#"{"event":"device_changed","channel":0}"#), None);
        assert_eq!(parse_event("not json"), None);
        assert_eq!(parse_event(r#"{"event":"level","channel":0}"#), None, "a level without rms is unusable");
    }

    #[test]
    fn bin_resolution_order() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();
        assert!(pick_bin(None, Some(&exe_dir), None).is_err());
        let dev = tmp.path().join(DEV_BIN);
        std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
        std::fs::write(&dev, "").unwrap();
        // the dev build is found in debug builds only
        assert_eq!(pick_bin(None, Some(&exe_dir), None).is_ok(), cfg!(debug_assertions));
        assert_eq!(pick_bin(None, None, Some(&exe_dir)).is_ok(), cfg!(debug_assertions));
        let sibling = exe_dir.join(DEFAULT_BIN);
        std::fs::write(&sibling, "").unwrap();
        assert_eq!(pick_bin(None, Some(&exe_dir), None).unwrap(), sibling);
        let own = tmp.path().join("mine");
        std::fs::write(&own, "").unwrap();
        assert_eq!(pick_bin(Some(own.clone()), Some(&exe_dir), None).unwrap(), own);
        assert!(pick_bin(Some(tmp.path().join("nope")), Some(&exe_dir), None).is_err());
    }
}
