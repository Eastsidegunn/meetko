//! Plugin host: both real plugins against both fixtures (conformance as in
//! tests/test_contract.py), the time budget, the minimal environment.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use meetko_core::contract::MeetingData;
use meetko_core::error::Error;
use meetko_core::host::Plugin;
use meetko_core::manifest::Manifest;
use meetko_core::sandbox::Isolation;
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(version: &str) -> MeetingData {
    let p = root().join(format!("contract/fixtures/meeting01/meeting-data.{version}.json"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Returns None (with a message) when the plugin is a binary that was not built.
fn load_plugin(name: &str) -> Option<(Manifest, PathBuf)> {
    let dir = root().join("tests/fixtures/plugins").join(name);
    let (m, _) = Manifest::load(&dir).unwrap();
    let (prog, _) = m.resolve_command(&dir).unwrap();
    // a compiled plugin is `target/release/<name>.exe` on Windows
    let built = prog.exists() || (cfg!(windows) && prog.with_extension("exe").exists());
    if prog.components().count() > 1 && !built {
        // CI sets MEETKO_REQUIRE_PLUGINS so that a missing build fails instead of passing unseen
        assert!(std::env::var_os("MEETKO_REQUIRE_PLUGINS").is_none(), "{name} is not built, but MEETKO_REQUIRE_PLUGINS is set");
        eprintln!("SKIP {name}: build it first (cd tests/fixtures/plugins/{name} && cargo build --release --locked)");
        return None;
    }
    Some((m, dir))
}

fn run_json(p: &mut Plugin, input: &Value, md: &MeetingData) -> Value {
    let out = p.run_raw(input).unwrap();
    assert_eq!(meetko_core::contract::output_errors(md, &out), Vec::<String>::new());
    out
}

fn conformance(name: &str) {
    let Some((m, dir)) = load_plugin(name) else { return };
    for version in ["draft", "live"] {
        let md = fixture(version);
        let mut p = Plugin::spawn(&m, &dir, Duration::from_secs(30)).unwrap();
        let out = p.run(&md).unwrap();
        assert!(!out["items"].as_array().unwrap().is_empty(), "plugin returned nothing on a real meeting");
        // unknown fields must not change the result
        let mut input = md.to_value();
        input["future_branch"] = json!({"anything": [1, 2]});
        input["meeting"]["future"] = json!(true);
        for w in input["words"].as_array_mut().unwrap() {
            w["lang"] = json!("ko");
        }
        assert_eq!(run_json(&mut p, &input, &md), out, "unknown fields changed the result");
        assert_eq!(p.run(&md).unwrap(), out, "plugin is not a function of its input");
        p.close().unwrap();
    }
}

#[test]
fn action_items_py_conformance() {
    conformance("action-items-py");
}

#[test]
fn talk_time_rs_conformance() {
    conformance("talk-time-rs");
}

#[test]
fn plugin_refuses_unknown_contract() {
    // a host that offers only a contract the plugin does not speak: build one by hand
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("p.py"),
        r#"import sys, json
for line in sys.stdin:
    m = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32001, "message": "no"}}), flush=True)
"#,
    )
    .unwrap();
    let m = fake_manifest(&[python(), "-I", "-S", "p.py"]);
    let err = Plugin::spawn(&m, dir.path(), Duration::from_secs(10)).err().expect("must fail");
    assert!(err.to_string().contains("-32001"), "{err}");
}

fn fake_manifest(cmd: &[&str]) -> Manifest {
    let list: Vec<String> = cmd.iter().map(|s| format!("{s:?}")).collect();
    Manifest::parse(&format!(
        "[package]\nid=\"t/fake\"\nversion=\"0\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[{}]\nprotocol=\"stdio-jsonrpc\"\n",
        list.join(",")
    ))
    .unwrap()
}

const SLEEPY: &str = r#"import sys, json, time
for line in sys.stdin:
    m = json.loads(line)
    if m["method"] == "initialize":
        r = {"contract": "0.1", "plugin": {"id": "t/fake", "version": "0"}, "outputs": []}
    else:
        time.sleep(30)
        r = None
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

#[test]
fn request_timeout_kills_the_plugin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("p.py"), SLEEPY).unwrap();
    let m = fake_manifest(&[python(), "-I", "-S", "p.py"]);
    let mut p = Plugin::spawn(&m, dir.path(), Duration::from_millis(600)).unwrap();
    let t = Instant::now();
    let err = p.run(&fixture("draft")).unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "{err}");
    assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
    // the process is gone, so the next call cannot be answered
    assert!(p.call("run", json!({})).is_err());
}

/// `python3` on macOS/Linux, `python` on Windows
fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[test]
fn plugin_gets_a_minimal_environment() {
    std::env::set_var("MEETKO_TEST_SECRET", "hunter2");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("p.py"),
        r#"import sys, json, os, stat
home = os.environ.get("HOME", "")
mode = oct(stat.S_IMODE(os.stat(home).st_mode)) if home else ""
state = "empty" if home and not os.listdir(home) else "notempty"
for line in sys.stdin:
    m = json.loads(line)
    keys = ",".join(sorted(os.environ))
    r = {"contract": "0.1", "plugin": {"id": "t/fake", "version": keys}, "outputs": [home, mode, state]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#,
    )
    .unwrap();
    let m = fake_manifest(&[python(), "-I", "-S", "p.py"]);
    let p = Plugin::spawn(&m, dir.path(), Duration::from_secs(10)).unwrap();
    let keys = p.info["plugin"]["version"].as_str().unwrap().to_string();
    assert!(!keys.contains("MEETKO_TEST_SECRET"), "leaked: {keys}");
    // the interpreter (or the macOS /usr/bin/python3 shim) adds a few of its own;
    // nothing of the parent's beyond the allowlist may appear
    let own = ["LC_CTYPE", "__CF_USER_TEXT_ENCODING", "CPATH", "LIBRARY_PATH", "MANPATH", "SDKROOT"];
    for k in keys.split(',') {
        let ours = ["PATH", "LANG", "HOME", "PYTHONUTF8", "PYTHONIOENCODING", "SYSTEMROOT", "TEMP", "TMP", "USERPROFILE", "PATHEXT"];
        assert!(ours.contains(&k.to_uppercase().as_str()) || own.contains(&k), "unexpected env var {k}");
    }
    assert!(keys.contains("PYTHONUTF8") && keys.contains("PYTHONIOENCODING"));
    let out = p.info["outputs"].as_array().unwrap();
    let home = out[0].as_str().unwrap().to_string();
    assert_ne!(Some(home.clone().into()), std::env::var_os("HOME"), "plugin must not see the real HOME");
    assert_eq!(out[2], "empty");
    #[cfg(unix)]
    assert_eq!(out[1], "0o700");
    drop(p);
    assert!(!std::path::Path::new(&home).exists(), "per-run HOME must be removed");
}

/// Run a one-off fake plugin whose `run` method executes `body` (Python).
fn hostile(body: &str, timeout: Duration) -> (Result<Value, Error>, Duration, tempfile::TempDir) {
    hostile_with(body, timeout, &Isolation::production(&meetko_core::util::data_dir()))
}

fn hostile_with(body: &str, timeout: Duration, iso: &Isolation) -> (Result<Value, Error>, Duration, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("p.py"),
        format!(
            r#"import sys, json, os, time, subprocess
def reply(m, r):
    sys.stdout.buffer.write((json.dumps({{"jsonrpc": "2.0", "id": m["id"], "result": r}}) + "\n").encode()); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    if m["method"] == "initialize":
        reply(m, {{"contract": "0.1", "plugin": {{"id": "t/fake", "version": "0"}}, "outputs": []}})
    else:
{}
"#,
            body.lines().map(|l| format!("        {l}")).collect::<Vec<_>>().join("\n")
        ),
    )
    .unwrap();
    let m = fake_manifest(&[python(), "-I", "-S", "p.py"]);
    let mut p = Plugin::spawn_with(&m, dir.path(), timeout, iso).unwrap();
    let t = Instant::now();
    let r = p.run(&fixture("draft"));
    let took = t.elapsed();
    drop(p);
    (r, took, dir)
}

#[test]
fn invalid_utf8_reply_is_an_error_not_a_hang() {
    let (r, took, _d) = hostile("sys.stdout.buffer.write(b'\\xff\\n'); sys.stdout.flush(); time.sleep(60)", Duration::from_secs(20));
    let err = r.unwrap_err().to_string();
    assert!(err.contains("UTF-8"), "{err}");
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

#[test]
fn closing_stdout_then_sleeping_does_not_hang_the_host() {
    let (r, took, _d) = hostile("os.close(1); time.sleep(60)", Duration::from_secs(30));
    assert!(r.is_err());
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

#[test]
fn oversized_reply_line_is_cut_off() {
    let (r, took, _d) = hostile(
        "sys.stdout.buffer.write(b'a' * (17 * 1024 * 1024) + b'\\n'); sys.stdout.flush(); time.sleep(60)",
        Duration::from_secs(30),
    );
    let err = r.unwrap_err().to_string();
    assert!(err.contains("exceeds 16 MB"), "{err}");
    assert!(took < Duration::from_secs(15), "took {took:?}");
}

/// pids of processes whose command line contains `needle`
#[cfg(unix)]
fn pgrep(needle: &str) -> Vec<String> {
    let o = std::process::Command::new("pgrep").args(["-f", needle]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect()
}

#[cfg(unix)]
#[test]
fn timeout_kills_grandchildren_too() {
    // the plugin cannot write a pid file (its dir is read-only under the sandbox): find the
    // grandchild by its unique argument instead
    let u = format!("60.{}{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
    // the macOS sandbox denies process-fork altogether (tests/sandbox.rs), so a grandchild exists only
    // without it (Linux/Windows, or MEETKO_PLUGIN_SANDBOX=off): this covers the process-group kill there
    let needle = u.clone();
    let (r, _, _dir) = hostile_with(
        &format!("g = subprocess.Popen([sys.executable, '-I', '-S', '-c', 'import time; time.sleep(60)', '{u}']); time.sleep(0.5)\ntime.sleep(60)"),
        Duration::from_millis(1500),
        &Isolation::off(),
    );
    assert!(matches!(r, Err(Error::Timeout(_))));
    let t = Instant::now();
    while !pgrep(&needle).is_empty() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pgrep(&needle).is_empty(), "grandchild survived the kill");
}

#[test]
fn manifest_command_cannot_escape_or_use_a_shell() {
    for bad in ["../outside", "..\\outside", "/abs/x", "\\abs\\x", "C:\\x\\y", "C:/x/y", "a/../../b"] {
        let m = fake_manifest(&[bad]);
        assert!(m.resolve_command(std::path::Path::new("/p")).is_err(), "{bad}");
    }
    let m = fake_manifest(&["bin\\run"]);
    assert_eq!(m.resolve_command(std::path::Path::new("/p")).unwrap().0, PathBuf::from("/p/bin/run"));
    let m = fake_manifest(&["bin/run", "--x"]);
    let (prog, args) = m.resolve_command(std::path::Path::new("/p")).unwrap();
    assert_eq!((prog, args), (PathBuf::from("/p/bin/run"), vec!["--x".to_string()]));
}

#[test]
fn engine_manifests_parse_but_cannot_be_added_or_run() {
    let m = Manifest::parse(
        "[package]\nid=\"meetko/whisper-cpp\"\nversion=\"0.1.0\"\nkind=\"engine\"\ncontract=\"0.1\"\n\
         [run]\ncommand=\"bin/meetko-whisper-cpp\"\nprotocol=\"ws\"\n[role]\nprovides=\"asr.batch\"\nword_timestamps=\"native\"\n\
         [permissions]\nnetwork=false\n",
    )
    .unwrap();
    assert_eq!(m.package.kind, "engine");
    assert_eq!(m.role.unwrap().provides, "asr.batch");
    let m = Manifest::parse(
        "[package]\nid=\"e\"\nversion=\"1\"\nkind=\"engine\"\ncontract=\"0.1\"\n[run]\ncommand=[\"x\"]\nprotocol=\"ws\"\n",
    )
    .unwrap();
    assert!(m.require_plugin().is_err());
    assert!(Plugin::spawn(&m, std::path::Path::new("."), Duration::from_secs(1)).is_err());
}
