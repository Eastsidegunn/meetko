//! A plugin that declares API keys (tests/fixtures/plugins/secrets-py) through the real host:
//! installed, given its declared secrets from an in-memory store, run (under the macOS sandbox
//! there) against a loopback OpenAI-compatible endpoint. No real network and no keychain.

use std::path::{Path, PathBuf};
use std::time::Duration;

use meetko_core::contract::MeetingData;
use meetko_core::pipeline;
use meetko_core::secrets::{self, MemoryStore};
use meetko_core::store::{ImportChannel, ImportOptions, Store};

const KEY: &str = "sk-test-not-a-real-key-123";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wav(path: &Path) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..16000 * 70).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

/// A store with one draft: the committed fixture.
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
/// A key-declaring plugin end to end through the host (under the macOS sandbox there): the declared
/// OPENAI_* secrets point it at a loopback OpenAI-compatible endpoint; the key reaches the endpoint
/// as a Bearer token, and the key the endpoint echoes back is redacted by the host (the fixture
/// returns it unredacted). Anchor checking of a real summary plugin is tested in its own repo.
#[test]
fn a_plugin_runs_with_its_secrets_against_a_loopback_endpoint() {
    let (_t, s, v) = store_with_draft();
    s.install_plugin(&root().join("tests/fixtures/plugins/secrets-py"), None).unwrap();
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", srv.server_addr().to_ip().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut req in srv.incoming_requests() {
            let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
            let mut body = String::new();
            let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
            log.lock().unwrap().push(format!("{} {} {auth}", req.method(), req.url()));
            let reply = serde_json::json!({"choices": [{"message": {"role": "assistant", "content": format!("요약 (echo {KEY})")}}]});
            let _ = req.respond(tiny_http::Response::from_string(reply.to_string()).with_header("Content-Type: application/json".parse::<tiny_http::Header>().unwrap()));
        }
    });
    let keys = MemoryStore::default();
    let id = "meetko/secrets-fixture";
    secrets::set(&s, &keys, id, "OPENAI_API_KEY", KEY).unwrap();
    secrets::set(&s, &keys, id, "OPENAI_BASE_URL", &base).unwrap();
    secrets::set(&s, &keys, id, "OPENAI_MODEL", "test-model").unwrap();
    let rid = pipeline::run_plugin_with(&s, &keys, id, &v, Duration::from_secs(60)).unwrap();
    let r = s.results_for_version(&v).unwrap().into_iter().find(|r| r.id == rid).unwrap();
    let out = r.output.to_string();
    assert!(!out.contains(KEY), "{out}");
    assert!(out.contains("echo ***"), "{out}");
    assert_eq!(r.output["items"][0]["type"], "summary_card", "{out}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0], format!("POST /v1/chat/completions Bearer {KEY}"));
}
