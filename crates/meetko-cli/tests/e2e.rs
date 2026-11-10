//! The CLI flow against a temp data dir, with a fake whisper-cli (no 1.6 GB model needed).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn silent_wav(path: &Path, secs: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..16000 * secs {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
}

// -ojf shape, trimmed: "<next step> 액션 아이템은 내가 정리할게요" in two segments
const CANNED: &str = r#"{"result":{"language":"ko"},"transcription":[
 {"offsets":{"from":0,"to":2000},"tokens":[
  {"text":"[_BEG_]","p":0.9,"t_dtw":-1,"offsets":{"from":0,"to":0}},
  {"text":" 다음","p":0.9,"t_dtw":20,"offsets":{"from":0,"to":500}},
  {"text":" 액션","p":0.8,"t_dtw":70,"offsets":{"from":500,"to":900}},
  {"text":" 아이템은","p":0.7,"t_dtw":110,"offsets":{"from":900,"to":1500}}]},
 {"offsets":{"from":2000,"to":4000},"tokens":[
  {"text":" 내가","p":0.9,"t_dtw":230,"offsets":{"from":2000,"to":2500}},
  {"text":" 정리","p":0.9,"t_dtw":280,"offsets":{"from":2500,"to":3000}},
  {"text":"할게요","p":0.9,"t_dtw":320,"offsets":{"from":3000,"to":3500}}]}]}"#;

const CANNED_LOOPBACK: &str = r#"{"result":{"language":"ko"},"transcription":[
 {"offsets":{"from":4000,"to":5000},"tokens":[
  {"text":" 알겠습니다","p":0.9,"t_dtw":410,"offsets":{"from":4000,"to":5000}}]}]}"#;

fn fake_whisper(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("canned-0.json"), CANNED).unwrap();
    std::fs::write(dir.join("canned-1.json"), CANNED_LOOPBACK).unwrap();
    std::fs::write(dir.join("canned-alt-0.json"), CANNED.replace("액션", "액숀")).unwrap();
    std::fs::write(dir.join("canned-alt-1.json"), CANNED_LOOPBACK).unwrap();
    // The engine reads a staged copy named input.wav, so channels are told apart by call
    // order (0, then 1); `count` is reset by the test. The file `set` picks the canned set
    // (not an environment variable: whisper-cli gets a minimal environment).
    // The logic is one Python script; a two-line wrapper makes it directly executable on
    // each OS (a shell script on Unix, a .cmd on Windows).
    let py = dir.join("fake_whisper.py");
    std::fs::write(
        &py,
        format!(
            r#"import os, shutil, sys
d = r"{}"
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("whisper.cpp version: 0.0.0-fake")
    sys.exit(0)
out = None
for i, x in enumerate(a):
    if x == "-of":
        out = a[i + 1]
def rd(n):
    try:
        return open(os.path.join(d, n)).read().strip()
    except OSError:
        return ""
n = int(rd("count") or 0)
open(os.path.join(d, "count"), "w").write(str(n + 1))
s = rd("set")
shutil.copy(os.path.join(d, "canned" + ("-" + s if s else "") + "-" + str(n) + ".json"), out + ".json")
"#,
            dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    let script = {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("whisper-cli");
        std::fs::write(&script, format!("#!/bin/sh\nexec python3 \"{}\" \"$@\"\n", py.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    };
    #[cfg(windows)]
    let script = {
        let script = dir.join("whisper-cli.cmd");
        std::fs::write(&script, format!("@python \"{}\" %*\r\n", py.display())).unwrap();
        script
    };
    script
}

struct Env {
    data: PathBuf,
    whisper: PathBuf,
    model: PathBuf,
}

impl Env {
    fn meetko(&self, args: &[&str]) -> (String, String, bool) {
        let o = Command::new(env!("CARGO_BIN_EXE_meetko"))
            .args(args)
            .env("MEETKO_DATA_DIR", &self.data)
            .env("MEETKO_WHISPER_CLI", &self.whisper)
            .env("MEETKO_WHISPER_MODEL", &self.model)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.success(),
        )
    }

    fn reset_fake(&self) {
        std::fs::write(self.whisper.parent().unwrap().join("count"), "0").unwrap();
    }

    fn ok(&self, args: &[&str]) -> String {
        let (out, err, ok) = self.meetko(args);
        assert!(ok, "meetko {args:?} failed: {err}");
        out
    }
}

#[test]
fn import_transcribe_plugin_approve_stale_export() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let (mic, lb) = (t.join("mic.wav"), t.join("loopback.wav"));
    silent_wav(&mic, 5);
    silent_wav(&lb, 5);
    let model = t.join("ggml-test.bin");
    std::fs::write(&model, b"not a model").unwrap();
    let env = Env { data: t.join("data"), whisper: fake_whisper(t), model };

    let meeting = env.ok(&["import", "--mic", mic.to_str().unwrap(), "--loopback", lb.to_str().unwrap(), "--title", "t"]).trim().to_string();
    assert!(meeting.starts_with('m'), "{meeting}");
    // channel files are copied under recordings/<sha256>/, not referenced in place
    let rec = std::fs::read_dir(env.data.join("recordings")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(rec.file_name().unwrap().len(), 64);
    assert_eq!(std::fs::read_dir(&rec).unwrap().count(), 2);

    let draft = env.ok(&["transcribe", &meeting]).trim().to_string();
    assert_eq!(draft, format!("{meeting}.draft.1"));
    let shown = env.ok(&["show", &draft]);
    assert!(shown.contains("[00:00] 나: 다음 액션 아이템은 내가 정리할게요"), "{shown}");
    assert!(shown.contains("상대: 알겠습니다"), "{shown}");

    // the stored producer records id and sha256 of the model, never its path
    let read = |n: &str| String::from_utf8_lossy(&std::fs::read(env.data.join(n)).unwrap_or_default()).into_owned();
    let db = read("meetko.db") + &read("meetko.db-wal");
    assert!(db.contains("ggml-test") && !db.contains(env.model.to_str().unwrap()));
    let export_ctm = env.ok(&["export", &draft, "--format", "ctm", "--channel", "0"]);
    assert!(export_ctm.contains(" 1 0.200 "), "{export_ctm}");

    let plugin_dir = root().join("plugins/action-items-py");
    // adding needs consent
    let (_, err, ok) = env.meetko(&["plugin", "add", plugin_dir.to_str().unwrap()]);
    assert!(!ok && err.contains("--yes") && err.contains("sandbox"), "{err}");
    let plugin = env.ok(&["plugin", "add", "--yes", plugin_dir.to_str().unwrap()]);
    assert_eq!(plugin.trim(), "meetko/action-items-py");
    assert!(env.ok(&["plugin", "list"]).contains(if cfg!(target_os = "macos") { "declared network=false audio=none (sandbox: macos-sbpl)" } else { "declared network=false audio=none (not enforced on this OS)" }));
    let run1 = env.ok(&["run", "meetko/action-items-py", &draft]);
    assert!(run1.contains("current"), "{run1}");
    assert!(env.ok(&["results", &draft]).contains("current"));

    let approved = env.ok(&["approve", &draft]).trim().to_string();
    assert_eq!(approved, format!("{meeting}.approved.1"));
    // identical content: approving changed the version id, not the transcript, so nothing is stale
    assert!(env.ok(&["results", &draft]).contains("current"));
    // a new transcription with different words, once approved, makes the old result stale
    env.reset_fake();
    std::fs::write(env.whisper.parent().unwrap().join("set"), "alt").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_meetko"))
        .args(["transcribe", &meeting])
        .env("MEETKO_DATA_DIR", &env.data)
        .env("MEETKO_WHISPER_CLI", &env.whisper)
        .env("MEETKO_WHISPER_MODEL", &env.model)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let draft2 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(draft2, format!("{meeting}.draft.2"));
    assert!(env.ok(&["results", &draft]).contains("current"), "an approved head still holds the old content");
    let approved2 = env.ok(&["approve", &draft2]).trim().to_string();
    // search reads the current (approved) head: the new word is found, the replaced one is not
    let found = env.ok(&["search", "액숀 아이템"]);
    assert!(found.contains(&approved2) && found.contains("[액숀 아이템]"), "{found}");
    let (out, err, ok) = env.meetko(&["search", "액션 아이"]);
    assert!(ok && out.is_empty() && err.contains("no matches"), "{out} {err}");
    let results = env.ok(&["results", &draft]);
    assert!(results.contains("STALE"), "{results}");
    // a run on the new approved version is current, and the old one still shows stale
    env.ok(&["run", "meetko/action-items-py", &approved2]);
    let all = env.ok(&["results", &approved2, "--all"]);
    assert!(all.contains("STALE") && all.contains("current"), "{all}");

    // approving an approved version
    let (_, err, ok) = env.meetko(&["approve", &approved]);
    assert!(!ok && err.contains("only a draft"), "{err}");

    let vtt = env.ok(&["export", &approved, "--format", "webvtt"]);
    assert!(vtt.starts_with("WEBVTT\n\nt0000\n00:00:00.200 --> "), "{vtt}");
    assert!(vtt.contains("<v 나>다음 액션 아이템은 내가 정리할게요"), "{vtt}");

    // the approved version has the draft's content under a new id
    let (a, b) = (env.ok(&["export", &draft, "--format", "text"]), env.ok(&["export", &approved, "--format", "text"]));
    assert_eq!(a, b);
    assert!(env.ok(&["list"]).contains(&format!("{approved}  approved  parent={draft}")));

    // importing the same recording again returns the same meeting
    let again = env.ok(&["import", "--mic", mic.to_str().unwrap(), "--loopback", lb.to_str().unwrap()]);
    assert_eq!(again.trim(), meeting);
}
