//! Plugin secrets and settings: only declared names are passed, only for the plugin's own origin,
//! over stdin (never the environment), and secret values never come back out (output, errors,
//! stderr). Uses the in-memory store, never the keychain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use meetko_core::contract::MeetingData;
use meetko_core::error::Error;
use meetko_core::host::Plugin;
use meetko_core::manifest::Manifest;
use meetko_core::pipeline;
use meetko_core::plugin_source::PluginSource;
use meetko_core::sandbox::Isolation;
use meetko_core::secrets::{self, Entry, Kind, MemoryStore, Prior, RunConfig, SecretKey, SecretStore};
use meetko_core::store::{ImportChannel, ImportOptions, Store};

const KEY: &str = "sk-test-not-a-real-key-123";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// A plugin that reports what `initialize` gave it and what its environment holds. `run` answers
/// with that in its output (mode "out"), or writes it to stderr and fails (mode "err").
const PLUGIN: &str = r#"import sys, json, os
def reply(m, r=None, err=None):
    d = {"jsonrpc": "2.0", "id": m["id"]}
    if err is None:
        d["result"] = r
    else:
        d["error"] = {"code": -32010, "message": err}
    sys.stdout.write(json.dumps(d) + "\n"); sys.stdout.flush()
given = {}
for line in sys.stdin:
    m = json.loads(line)
    if m["method"] == "initialize":
        given = m["params"]
        reply(m, {"contract": "0.1", "plugin": {"id": "t/keys", "version": "0.1.0"}, "outputs": []})
    elif m["method"] == "run":
        sec, st = given.get("secrets", {}), given.get("settings", {})
        leaked = [k for k in os.environ if k.endswith(("_KEY", "_MODEL"))] + [k for k, v in os.environ.items() if v in sec.values()]
        seen = "key=%s other=%s model=%s env=%s budget=%s" % (sec.get("TEST_API_KEY", "missing"), "present" if "OTHER_API_KEY" in sec else "absent",
            st.get("TEST_MODEL", "missing"), "leaked" if leaked else "clean", given.get("time_budget_ms"))
        mode = open("mode").read().strip()
        card = {"type": "summary_card", "title": seen, "points": [{"text": seen, "evidence": [{"from": "w00000", "to": "w00000"}]}]}
        plugin = {"id": "t/keys", "version": "0.1.0"}
        if mode == "err":
            sys.stderr.write("debug: " + sec.get("TEST_API_KEY", "") + "\n"); sys.stderr.flush()
            reply(m, err="auth failed for " + seen)
            continue
        if mode == "id":  # smuggle the key into the output's plugin id
            plugin = {"id": sec.get("TEST_API_KEY", "t/keys"), "version": "0.1.0"}
        if mode == "other":  # claim to be another plugin
            plugin = {"id": "t/other", "version": "0.1.0"}
        reply(m, {"contract": "0.1", "plugin": plugin, "input": {"version_id": m["params"]["input"]["version"]["id"]}, "items": [card]})
    else:
        reply(m, None)
        break
"#;

fn manifest_text(id: &str) -> String {
    format!(
        "[package]\nid=\"{id}\"\nversion=\"0.1.0\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"{}\", \"-I\", \"-S\", \"p.py\"]\nprotocol=\"stdio-jsonrpc\"\n[permissions]\nnetwork = true\nsecrets = [\"TEST_API_KEY\", \"OTHER_API_KEY\"]\nsettings = [\"TEST_MODEL\"]\n",
        python()
    )
}

fn plugin_dir(id: &str, mode: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("meetko.toml"), manifest_text(id)).unwrap();
    std::fs::write(d.path().join("p.py"), PLUGIN).unwrap();
    std::fs::write(d.path().join("mode"), mode).unwrap();
    d
}

fn wav(path: &Path) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..16000 * 70).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

/// A store with one draft (the committed fixture) and the test plugin installed from a local folder.
fn setup(mode: &str) -> (tempfile::TempDir, Store, String, tempfile::TempDir) {
    let (tmp, s, v) = store_with_draft();
    let src = plugin_dir("t/keys", mode);
    s.install_plugin(src.path(), None).unwrap();
    (tmp, s, v, src)
}

fn store_with_draft() -> (tempfile::TempDir, Store, String) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a);
    wav(&b);
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let ch = |index, role: &str, path: &Path| ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None };
    let out = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    let text = std::fs::read_to_string(root().join("contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
    let mut md: MeetingData = serde_json::from_str(&text).unwrap();
    md.meeting.id = out.meeting.id.clone();
    let v = s.create_version(md).unwrap().version.id;
    (tmp, s, v)
}

fn run(s: &Store, keys: &dyn SecretStore, v: &str) -> Result<String, Error> {
    let id = pipeline::run_plugin_with(s, keys, "t/keys", v, Duration::from_secs(30))?;
    let r = s.results_for_version(v)?.into_iter().find(|r| r.id == id).unwrap();
    Ok(r.output["items"][0]["title"].as_str().unwrap().to_string())
}

fn entry(name: &str, kind: Kind, set: bool, value: Option<&str>) -> Entry {
    Entry { name: name.into(), kind, set, value: value.map(String::from) }
}

#[test]
fn a_run_gets_only_its_declared_set_values_over_stdin_and_never_shows_secrets() {
    let (_t, s, v, _src) = setup("out");
    let keys = MemoryStore::default();
    assert_eq!(run(&s, &keys, &v).unwrap(), "key=missing other=absent model=missing env=clean budget=30000");
    assert_eq!(
        secrets::status(&s, &keys, "t/keys").unwrap(),
        [entry("TEST_API_KEY", Kind::Secret, false, None), entry("OTHER_API_KEY", Kind::Secret, false, None), entry("TEST_MODEL", Kind::Setting, false, None)]
    );
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    secrets::set(&s, &keys, "t/keys", "TEST_MODEL", "fast-1").unwrap();
    // an undeclared name cannot be set, so it can never be passed
    assert!(matches!(secrets::set(&s, &keys, "t/keys", "UNDECLARED_KEY", KEY), Err(Error::Secret(_))));
    assert!(secrets::set(&s, &keys, "t/keys", "OTHER_API_KEY", "short").is_err(), "a secret shorter than 8 is refused");
    let st = secrets::status(&s, &keys, "t/keys").unwrap();
    assert_eq!(st[0], entry("TEST_API_KEY", Kind::Secret, true, None), "a secret's value is never returned");
    assert_eq!(st[2], entry("TEST_MODEL", Kind::Setting, true, Some("fast-1")), "a setting's value is shown");
    // the plugin got both in initialize (not in its environment); the stored copy has the key redacted, the setting not
    assert_eq!(run(&s, &keys, &v).unwrap(), "key=*** other=absent model=fast-1 env=clean budget=30000");
    let stored = serde_json::to_string(&s.results_for_version(&v).unwrap().iter().map(|r| &r.output).collect::<Vec<_>>()).unwrap();
    assert!(!stored.contains(KEY));
    secrets::clear(&s, &keys, "t/keys", "TEST_API_KEY").unwrap();
    assert_eq!(run(&s, &keys, &v).unwrap(), "key=missing other=absent model=fast-1 env=clean budget=30000");
}

/// The reviewer's case: a key in `plugin.id` (a field that is not prose) must not be stored anywhere.
#[test]
fn a_key_in_the_output_plugin_id_is_refused_and_never_stored() {
    let (_t, s, v, _src) = setup("id");
    let keys = MemoryStore::default();
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    let e = run(&s, &keys, &v).unwrap_err().to_string();
    assert!(e.contains("rejected output") && !e.contains(KEY), "{e}");
    assert!(s.results_for_version(&v).unwrap().is_empty(), "nothing is stored");
}

#[test]
fn output_that_names_another_plugin_is_refused() {
    let (_t, s, v, _src) = setup("other");
    let e = run(&s, &MemoryStore::default(), &v).unwrap_err().to_string();
    assert!(e.contains("t/other") && e.contains("t/keys@0.1.0"), "{e}");
    assert!(s.results_for_version(&v).unwrap().is_empty());
}

/// An `initialize` reply that echoes the key as its contract: the error says so without the key.
#[test]
fn a_contract_reply_carrying_a_key_is_redacted() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("p.py"),
        "import sys, json\nm = json.loads(sys.stdin.readline())\nc = {m['params']['secrets']['TEST_API_KEY']: 1}\nsys.stdout.write(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': {'contract': [m['params']['secrets']['TEST_API_KEY'], c], 'plugin': {}, 'outputs': []}}) + '\\n'); sys.stdout.flush()\nsys.stdin.readline()\n",
    )
    .unwrap();
    let m = Manifest::parse(&manifest_text("t/keys")).unwrap();
    let run = RunConfig { secrets: vec![("TEST_API_KEY".to_string(), KEY.to_string())], settings: vec![] };
    let e = Plugin::spawn_with_secrets(&m, d.path(), Duration::from_secs(20), &Isolation::off(), &run).err().expect("refused").to_string();
    assert!(e.contains("plugin chose contract") && e.contains("***"), "{e}");
    assert!(!e.contains(KEY), "{e}");
}

#[test]
fn errors_and_stderr_never_carry_a_secret() {
    let (_t, s, v, _src) = setup("err");
    let keys = MemoryStore::default();
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    let e = run(&s, &keys, &v).unwrap_err().to_string();
    assert!(e.contains("key=***"), "{e}");
    assert!(!e.contains(KEY), "{e}");
}

/// A plugin that writes `stderr` and exits during `initialize`.
fn dies_with_stderr(stderr: &str) -> String {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("err.txt"), stderr).unwrap();
    std::fs::write(d.path().join("p.py"), "import sys, time\nsys.stdin.readline()\nsys.stderr.write(open('err.txt').read()); sys.stderr.flush()\ntime.sleep(0.3)\nsys.exit(3)\n").unwrap();
    let m = Manifest::parse(&manifest_text("t/keys")).unwrap();
    let run = RunConfig { secrets: vec![("TEST_API_KEY".to_string(), KEY.to_string())], settings: vec![] };
    Plugin::spawn_with_secrets(&m, d.path(), Duration::from_secs(20), &Isolation::off(), &run).err().expect("plugin exits").to_string()
}

fn no_fragment(e: &str) {
    for w in KEY.as_bytes().windows(5) {
        let w = std::str::from_utf8(w).unwrap();
        assert!(!e.contains(w), "fragment {w:?} in {e}");
    }
}

#[test]
fn the_stderr_tail_leaves_no_fragment_of_a_key_cut_at_its_edge() {
    let e = dies_with_stderr(&format!("boom {KEY}\n"));
    assert!(e.contains("boom ***"), "{e}");
    no_fragment(&e);
    // the 2000-byte tail starts in the middle of the key, at every offset
    for k in 1..KEY.len() {
        let e = dies_with_stderr(&format!("{}{KEY}{}", "x".repeat(3000), "y".repeat(2000 - k)));
        no_fragment(&e);
    }
    // the kept 8192 bytes start in the middle of the key
    let e = dies_with_stderr(&format!("{}{KEY}{}\nlast line", "x".repeat(100), "y".repeat(8192 - 20)));
    no_fragment(&e);
}

#[test]
fn only_declared_names_can_be_passed_and_each_under_its_own_kind() {
    let d = plugin_dir("t/keys", "out");
    let m = Manifest::parse(&manifest_text("t/keys")).unwrap();
    let iso = Isolation::off();
    let t = Duration::from_secs(20);
    let secret = |n: &str| RunConfig { secrets: vec![(n.to_string(), KEY.to_string())], settings: vec![] };
    let setting = |n: &str| RunConfig { secrets: vec![], settings: vec![(n.to_string(), "fast-1".to_string())] };
    assert!(Plugin::spawn_with_secrets(&m, d.path(), t, &iso, &secret("PATH")).is_err());
    assert!(Plugin::spawn_with_secrets(&m, d.path(), t, &iso, &secret("UNDECLARED_KEY")).is_err());
    assert!(Plugin::spawn_with_secrets(&m, d.path(), t, &iso, &secret("TEST_MODEL")).is_err(), "a setting passed as a secret");
    assert!(Plugin::spawn_with_secrets(&m, d.path(), t, &iso, &setting("TEST_API_KEY")).is_err(), "a secret passed as a setting");
    assert!(Plugin::spawn_with_secrets(&m, d.path(), t, &iso, &setting("TEST_MODEL")).is_ok());
}

#[test]
fn a_plugin_from_another_origin_does_not_inherit_values() {
    let (_t, s, v, _src) = setup("out");
    let keys = MemoryStore::default();
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    secrets::set(&s, &keys, "t/keys", "TEST_MODEL", "fast-1").unwrap();
    let before = Prior::of(&s, "t/keys").unwrap().unwrap();
    // the same id now recorded as coming from a GitHub repo: its values are a different set
    s.set_plugin_source("t/keys", &PluginSource { repo: "github:Someone/keys".into(), sha: "a".repeat(40), reference: "v1".into() }).unwrap();
    assert_eq!(secrets::origin_of(&s, "t/keys").unwrap(), "github:someone/keys");
    assert_eq!(run(&s, &keys, &v).unwrap(), "key=missing other=absent model=missing env=clean budget=30000");
    assert!(!secrets::status(&s, &keys, "t/keys").unwrap()[0].set);
    // and the old origin's values are removed after the move
    let old = SecretKey { plugin_id: "t/keys".into(), origin: "local".into(), name: "TEST_API_KEY".into() };
    assert!(keys.has(&old).unwrap());
    before.forget_if_moved(&s, &keys).unwrap();
    assert!(keys.keys().is_empty());
}

#[test]
fn an_update_from_the_same_origin_keeps_values() {
    let (_t, s, _v, _src) = setup("out");
    let keys = MemoryStore::default();
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    let before = Prior::of(&s, "t/keys").unwrap().unwrap();
    let newer = plugin_dir("t/keys", "out");
    std::fs::write(newer.path().join("meetko.toml"), manifest_text("t/keys").replace("0.1.0", "0.2.0")).unwrap();
    s.install_plugin(newer.path(), None).unwrap();
    before.forget_if_moved(&s, &keys).unwrap();
    assert!(secrets::status(&s, &keys, "t/keys").unwrap()[0].set);
}

#[test]
fn a_plugin_without_secrets_never_touches_the_store() {
    struct Panics;
    impl SecretStore for Panics {
        fn get(&self, _: &SecretKey) -> meetko_core::Result<Option<String>> {
            panic!("read")
        }
        fn has(&self, _: &SecretKey) -> meetko_core::Result<bool> {
            panic!("has")
        }
        fn set(&self, _: &SecretKey, _: &str) -> meetko_core::Result<()> {
            panic!("set")
        }
        fn delete(&self, _: &SecretKey) -> meetko_core::Result<()> {
            panic!("delete")
        }
    }
    let m = Manifest::load(&root().join("plugins/action-items-py")).unwrap().0;
    assert!(secrets::for_run(&Panics, "meetko/action-items-py", "local", &m).unwrap().is_empty());
}

/// A store whose values exist but are not handed over, as the macOS keychain answers when its
/// access prompt is denied (another meetko binary saved them).
struct Denied(MemoryStore);

impl SecretStore for Denied {
    fn get(&self, key: &SecretKey) -> meetko_core::Result<Option<String>> {
        match self.0.has(key)? {
            true => Err(Error::SecretAccessDenied(format!("{} was not allowed", key.name))),
            false => Ok(None),
        }
    }
    fn has(&self, key: &SecretKey) -> meetko_core::Result<bool> {
        self.0.has(key)
    }
    fn set(&self, key: &SecretKey, value: &str) -> meetko_core::Result<()> {
        self.0.set(key, value)
    }
    fn delete(&self, key: &SecretKey) -> meetko_core::Result<()> {
        self.0.delete(key)
    }
}

/// A denied keychain read fails the run with its own code before the plugin starts: the plugin
/// never runs without the key the user saved, and nothing is stored.
#[test]
fn a_denied_key_stops_the_run_with_its_own_code() {
    let (_t, s, v, _src) = setup("out");
    let keys = Denied(MemoryStore::default());
    secrets::set(&s, &keys, "t/keys", "TEST_API_KEY", KEY).unwrap();
    let e = run(&s, &keys, &v).unwrap_err();
    assert_eq!(e.code(), "secret_access_denied", "{e}");
    assert!(!e.to_string().contains(KEY));
    assert!(s.results_for_version(&v).unwrap().is_empty(), "the plugin did not run");
    // a denied setting is reported as set without its value, and fails the run the same way
    secrets::clear(&s, &keys, "t/keys", "TEST_API_KEY").unwrap();
    secrets::set(&s, &keys, "t/keys", "TEST_MODEL", "fast-1").unwrap();
    assert_eq!(
        secrets::status(&s, &keys, "t/keys").unwrap(),
        [entry("TEST_API_KEY", Kind::Secret, false, None), entry("OTHER_API_KEY", Kind::Secret, false, None), entry("TEST_MODEL", Kind::Setting, true, None)]
    );
    assert_eq!(run(&s, &keys, &v).unwrap_err().code(), "secret_access_denied");
    // with nothing saved there is nothing to deny: the plugin runs and says itself that a key is missing
    secrets::clear(&s, &keys, "t/keys", "TEST_MODEL").unwrap();
    assert_eq!(run(&s, &keys, &v).unwrap(), "key=missing other=absent model=missing env=clean budget=30000");
}
