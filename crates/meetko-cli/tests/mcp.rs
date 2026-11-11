//! `meetko mcp` driven over stdio against a temp data dir holding the contract fixture.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use meetko_core::contract::{MeetingData, VersionKind};
use meetko_core::manifest::{plugin_code_sha256, Manifest};
use meetko_core::store::{ImportChannel, ImportOptions, Store};
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wav(path: &Path, seed: i16) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..16000).for_each(|i| w.write_sample((i % 100) as i16 + seed).unwrap());
    w.finalize().unwrap();
}

fn fixture(meeting: &str) -> MeetingData {
    let text = std::fs::read_to_string(root().join("contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
    let mut md: MeetingData = serde_json::from_str(&text).unwrap();
    md.meeting.id = meeting.into();
    md
}

/// a meeting with the fixture draft; returns (meeting id, version id)
fn meeting(tmp: &Path, s: &Store, seed: i16, title: &str) -> (String, String) {
    let (a, b) = (tmp.join(format!("a{seed}.wav")), tmp.join(format!("b{seed}.wav")));
    wav(&a, seed);
    wav(&b, seed);
    let ch = |index, role: &str, path: &Path| ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None };
    let opts = ImportOptions { title: Some(title.into()), started_at: Some(format!("2026-11-01T{:02}:{:02}:00Z", seed / 60, seed % 60)), ..Default::default() };
    let m = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], opts).unwrap().meeting.id;
    let v = s.create_version(fixture(&m)).unwrap().version.id;
    (m, v)
}

struct Mcp {
    child: Child,
    tx: ChildStdin,
    rx: BufReader<ChildStdout>,
    next: i64,
}

impl Mcp {
    fn start(data: &Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meetko"))
            .arg("mcp")
            .env("MEETKO_DATA_DIR", data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (tx, rx) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        Mcp { child, tx, rx, next: 0 }
    }

    fn send_raw(&mut self, line: &str) {
        self.tx.write_all(line.as_bytes()).unwrap();
        self.tx.write_all(b"\n").unwrap();
        self.tx.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut l = String::new();
        self.rx.read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))
    }

    fn raw(&mut self, line: &str) -> Value {
        self.send_raw(line);
        self.recv()
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let r = self.raw(&json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params }).to_string());
        assert_eq!(r["id"], json!(self.next), "{r}");
        r
    }

    fn init(data: &Path) -> Mcp {
        let mut m = Mcp::start(data);
        let r = m.rpc("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-client", "version": "9.9" } }));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        m.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        m
    }

    /// (isError, the tool's text parsed as JSON when it is JSON, else as a string)
    fn call(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.rpc("tools/call", json!({ "name": name, "arguments": args }));
        let res = &r["result"];
        assert!(res.is_object(), "{r}");
        let text = res["content"][0]["text"].as_str().unwrap().to_string();
        (res["isError"].as_bool().unwrap(), serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (err, v) = self.call(name, args);
        assert!(!err, "{name} failed: {v}");
        v
    }

    fn refused(&mut self, name: &str, args: Value) -> String {
        let (err, v) = self.call(name, args);
        assert!(err, "{name} should fail: {v}");
        v.as_str().unwrap().to_string()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn setup() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let s = Store::open(&data).unwrap();
    let (m, v) = meeting(tmp.path(), &s, 0, "배포 회의");
    (tmp, data, m, v)
}

fn summary(from: &str, to: &str) -> Value {
    json!([
        { "type": "summary_card", "title": "요약", "points": [{ "text": "인시던트 회고를 했어요", "evidence": [{ "from": from, "to": to }] }] },
        { "type": "action_items", "items": [{ "text": "런북 정리", "owner": "s0", "evidence": [{ "from": from, "to": to }] }] },
    ])
}

#[test]
fn initialize_and_tools_list() {
    let (_t, data, ..) = setup();
    let mut m = Mcp::init(&data);
    let r = m.rpc("tools/list", json!({}));
    let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_meetings", "search", "get_transcript", "get_results", "get_provenance", "add_result", "list_agent_results", "remove_result"]);
    for t in r["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }
    assert_eq!(m.rpc("ping", json!({}))["result"], json!({}));
    // an unknown revision gets the newest legacy one
    let mut m2 = Mcp::start(&data);
    let r = m2.rpc("initialize", json!({ "protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": { "name": "x", "version": "1" } }));
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(r["result"]["serverInfo"]["name"], "meetko");
}

#[test]
fn modern_requests_carry_their_version() {
    let (_t, data, ..) = setup();
    let mut m = Mcp::start(&data);
    let meta = |v: &str| json!({ "io.modelcontextprotocol/protocolVersion": v, "io.modelcontextprotocol/clientCapabilities": {}, "io.modelcontextprotocol/clientInfo": { "name": "modern", "version": "1" } });
    let r = m.rpc("server/discover", json!({ "_meta": meta("2026-07-28") }));
    assert_eq!(r["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(r["result"]["resultType"], "complete");
    assert_eq!(r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "meetko");
    let r = m.rpc("tools/list", json!({ "_meta": meta("2026-07-28") }));
    assert_eq!(r["result"]["resultType"], "complete");
    let r = m.rpc("ping", json!({ "_meta": meta("1900-01-01") }));
    assert_eq!(r["error"]["code"], -32022);
    assert_eq!(r["error"]["data"]["requested"], "1900-01-01");
    assert!(r["error"]["data"]["supported"].as_array().unwrap().contains(&json!("2025-11-25")));
    let r = m.rpc("ping", json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } }));
    assert_eq!(r["error"]["code"], -32602, "client capabilities are required");
}

#[test]
fn malformed_messages_and_unknown_names() {
    let (_t, data, ..) = setup();
    let mut m = Mcp::init(&data);
    assert_eq!(m.raw("{not json")["error"]["code"], -32700);
    let r = m.raw("[1,2]");
    assert_eq!((r["error"]["code"].clone(), r["id"].clone()), (json!(-32600), Value::Null));
    assert_eq!(m.raw(r#"{"id":5,"method":"ping"}"#)["error"]["code"], -32600, "jsonrpc is required");
    assert_eq!(m.raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)["error"]["code"], -32600);
    assert_eq!(m.raw(r#"{"jsonrpc":"2.0","id":6,"method":"resources/list"}"#)["error"]["code"], -32601);
    let r = m.rpc("tools/call", json!({ "name": "delete_everything", "arguments": {} }));
    assert_eq!(r["error"]["code"], -32602);
    assert!(r["error"]["message"].as_str().unwrap().contains("Unknown tool"));
    // notifications and stray responses get no answer: the next reply is for the next request
    m.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    m.send_raw(r#"{"jsonrpc":"2.0","id":99,"result":{}}"#);
    assert_eq!(m.rpc("ping", json!({}))["result"], json!({}));
    // a line over the request limit is refused without being kept, and the server goes on
    let huge = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"ping","params":{{"pad":"{}"}}}}"#, "x".repeat(5 << 20));
    assert_eq!(m.raw(&huge)["error"]["code"], -32600);
    assert_eq!(m.rpc("ping", json!({}))["result"], json!({}));
    // bad arguments are a tool error the agent can read
    let e = m.refused("list_meetings", json!({ "limit": "ten" }));
    assert!(e.contains("invalid arguments"), "{e}");
    let e = m.refused("get_transcript", json!({ "version_id": "nope.draft.1" }));
    assert!(e.contains("not found"), "{e}");
}

#[test]
fn list_meetings_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let s = Store::open(&data).unwrap();
    for i in 0..3 {
        meeting(tmp.path(), &s, i, &format!("회의 {i}"));
    }
    let mut m = Mcp::init(&data);
    let p1 = m.ok("list_meetings", json!({ "limit": 2 }));
    assert_eq!(p1["total"], 3);
    assert_eq!(p1["meetings"].as_array().unwrap().len(), 2);
    assert_eq!(p1["meetings"][0]["title"], "회의 2", "newest first");
    assert_eq!(p1["meetings"][0]["head_kind"], "draft");
    assert_eq!(p1["meetings"][0]["versions"], 1);
    let p2 = m.ok("list_meetings", json!({ "limit": 2, "cursor": p1["next_cursor"] }));
    assert_eq!(p2["meetings"].as_array().unwrap().len(), 1);
    assert_eq!(p2["next_cursor"], Value::Null);
}

#[test]
fn search_returns_anchors() {
    let (_t, data, m_id, v) = setup();
    let mut m = Mcp::init(&data);
    let r = m.ok("search", json!({ "query": "인시던트" }));
    let hit = &r["meetings"][0];
    assert_eq!(hit["meeting_id"], m_id.as_str());
    assert_eq!(hit["version_id"], v.as_str());
    assert_eq!(hit["hits"][0]["from"], "w00001");
    assert!(hit["hits"][0]["time"].as_str().unwrap().contains(':'));
    assert_eq!(m.ok("search", json!({ "query": "존재하지않는말" }))["meetings"], json!([]));
}

#[test]
fn transcript_formats_and_paging() {
    let (_t, data, m_id, v) = setup();
    let md = fixture(&m_id);
    let mut m = Mcp::init(&data);
    // by meeting: the head
    let t = m.ok("get_transcript", json!({ "meeting_id": m_id }));
    assert_eq!(t["version_id"], v.as_str());
    assert_eq!(t["total_turns"], md.turns.len());
    assert_eq!(t["turns"].as_array().unwrap().len(), md.turns.len());
    assert_eq!(t["turns"][0]["from"], md.turns[0].first_word.as_str());
    assert_eq!(t["turns"][0]["speaker"], "상대 1");
    assert!(t["turns"][0].get("words").is_none());
    let w = m.ok("get_transcript", json!({ "version_id": v, "format": "words", "max_turns": 1 }));
    assert_eq!(w["turns"][0]["words"][0], json!(["w00000", "지난주", 700]));
    // paging
    let mut seen = 0;
    let mut cur = Value::Null;
    loop {
        let p = m.ok("get_transcript", json!({ "version_id": v, "max_turns": 3, "cursor": cur }));
        seen += p["turns"].as_array().unwrap().len();
        cur = p["next_cursor"].clone();
        if cur.is_null() {
            break;
        }
    }
    assert_eq!(seen, md.turns.len());
    let txt = m.ok("get_transcript", json!({ "version_id": v, "format": "text" }));
    assert!(txt["text"].as_str().unwrap().starts_with("[00:00] 상대 1: 지난주"), "{}", txt["text"]);
    assert_eq!(m.refused("get_transcript", json!({ "version_id": v, "cursor": "x" })), "cursor is not one this server handed out");
}

#[test]
fn transcript_shows_the_names_and_edits_made_in_the_app() {
    let (_t, data, _m_id, v) = setup();
    {
        let s = Store::open(&data).unwrap();
        s.rename_speaker(&v, "s1", "민수").unwrap();
        s.merge_speaker(&v, "s0", "s1").unwrap();
        s.edit_turn(&v, "t0000", "고친 첫 문장").unwrap();
    }
    let mut m = Mcp::init(&data);
    let t = m.ok("get_transcript", json!({ "version_id": v }));
    let first = &t["turns"][0];
    assert_eq!((first["speaker"].as_str(), first["speaker_label"].as_str()), (Some("민수"), Some("상대 1")));
    assert_eq!((first["text"].as_str(), first["edited"].as_bool()), (Some("고친 첫 문장"), Some(true)));
    assert!(first["original_text"].as_str().unwrap().starts_with("지난주 인시던트"));
    // the merged speaker carries the name, its own label stays readable
    assert_eq!((t["turns"][1]["speaker"].as_str(), t["turns"][1]["speaker_label"].as_str()), (Some("민수"), Some("나")));
    assert!(t["turns"][1].get("edited").is_none());
    let s0 = t["speakers"].as_array().unwrap().iter().find(|s| s["id"] == "s0").unwrap();
    assert_eq!((s0["name"].as_str(), s0["merged_into"].as_str(), s0["label"].as_str()), (Some("민수"), Some("s1"), Some("나")));
    // word ids and times stay the engine's: evidence still cites them
    let w = m.ok("get_transcript", json!({ "version_id": v, "format": "words", "max_turns": 1 }));
    assert_eq!(w["turns"][0]["words"][0], json!(["w00000", "지난주", 700]));
    let txt = m.ok("get_transcript", json!({ "version_id": v, "format": "text" }));
    assert!(txt["text"].as_str().unwrap().starts_with("[00:00] 민수: 고친 첫 문장\n[00:08] 민수: "), "{}", txt["text"]);
    // search finds the edit, flags it and cites the whole turn
    let r = m.ok("search", json!({ "query": "고친 첫" }));
    let hit = &r["meetings"][0]["hits"][0];
    assert_eq!((hit["from"].as_str(), hit["to"].as_str(), hit["edited"].as_bool()), (Some("w00000"), Some("w00008"), Some(true)));
    let plain = m.ok("search", json!({ "query": "marketing" }));
    assert!(plain["meetings"][0]["hits"][0].get("edited").is_none());
}

#[test]
fn provenance_and_no_paths() {
    let (tmp, data, m_id, v) = setup();
    let mut m = Mcp::init(&data);
    let p = m.ok("get_provenance", json!({ "meeting_id": m_id }));
    assert_eq!(p["version"]["id"], v.as_str());
    assert_eq!(p["transcript_left_device"], false);
    assert_eq!(p["producers"][0]["engine"]["id"], "whisper.cpp");
    let mut all = p.to_string();
    for (tool, args) in [
        ("list_meetings", json!({})),
        ("get_transcript", json!({ "version_id": v, "format": "words" })),
        ("get_results", json!({ "meeting_id": m_id })),
        ("search", json!({ "query": "회의" })),
    ] {
        all += &m.ok(tool, args).to_string();
    }
    let tmp_s = tmp.path().to_string_lossy().to_string();
    assert!(!all.contains(&tmp_s) && !all.contains("recordings/") && !all.contains(".wav"), "a path leaked");
}

#[test]
fn add_result_is_stored_shown_replaced_listed_and_removed() {
    let (_t, data, m_id, v) = setup();
    let mut m = Mcp::init(&data);
    let r = m.ok("add_result", json!({ "id": "weekly-summary", "version_id": v, "agent": { "name": "Codex gpt-x\u{202e}", "version": "1.0" }, "items": summary("w00000", "w00008") }));
    assert_eq!(r["id"], "mcp:weekly-summary");
    let got = m.ok("get_results", json!({ "version_id": v }));
    assert_eq!(got["total_results"], 1);
    let g = &got["results"][0];
    assert_eq!((g["source"].as_str(), g["id"].as_str(), g["network"].as_bool(), g["stale"].as_bool()), (Some("agent"), Some("mcp:weekly-summary"), Some(true), Some(false)));
    assert_eq!(g["agent"]["agent"]["name"], "Codex gpt-x", "hidden characters are dropped from the self-description");
    assert_eq!(g["agent"]["client"], json!({ "name": "test-client", "version": "9.9" }));
    assert_eq!(g["items"][0]["type"], "summary_card");

    // in the store as the app reads it
    let s = Store::open(&data).unwrap();
    let rows = s.results_for_version(&v).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!((row.sandbox.as_str(), row.declared_network, row.declared_audio.as_str(), row.plugin_version.as_str()), ("mcp", true, "none", "1.0"));
    assert_eq!(row.output["plugin"], json!({ "id": "mcp:weekly-summary", "version": "1.0" }));
    let agent = row.agent.as_ref().unwrap();
    assert_eq!((agent["via"].as_str(), agent["network"].as_bool(), agent["protocol"].as_str()), (Some("mcp"), Some(true), Some("2025-06-18")));
    assert!(s.list_plugins().unwrap().is_empty(), "a result slot is not a plugin");
    assert!(s.plugin("mcp:weekly-summary").is_err());

    // the same id on the same version replaces; another id adds
    m.ok("add_result", json!({ "id": "mcp:weekly-summary", "version_id": v, "agent": { "name": "Claude" }, "items": summary("w00009", "w00010") }));
    m.ok("add_result", json!({ "id": "mcp:risks", "version_id": v, "agent": { "name": "Claude" }, "items": [{ "type": "timeline_marker", "kind": "risk", "label": "롤백 없음", "at": { "from": "w00003", "to": "w00004" } }] }));
    let rows = s.results_for_version(&v).unwrap();
    assert_eq!(rows.iter().map(|r| r.plugin_id.as_str()).collect::<Vec<_>>(), ["mcp:weekly-summary", "mcp:risks"]);
    assert_eq!(rows[0].output["items"][0]["points"][0]["evidence"][0]["from"], "w00009");
    assert_eq!(rows[0].agent.as_ref().unwrap()["agent"], json!({ "name": "Claude", "version": "unknown" }));

    // a new transcript makes them stale, like plugin results
    let mut md = fixture(&m_id);
    md.words[0].text = "지지난주".into();
    s.create_version(md).unwrap();
    let listed = m.ok("list_agent_results", json!({ "meeting_id": m_id }));
    assert_eq!(listed["results"].as_array().unwrap().len(), 2);
    assert!(listed["results"].as_array().unwrap().iter().all(|r| r["stale"] == true));
    assert_eq!(m.ok("list_agent_results", json!({ "id": "risks" }))["results"].as_array().unwrap().len(), 1);

    // removal: only that id, and the slot row goes with its last result
    assert_eq!(m.ok("remove_result", json!({ "id": "mcp:risks" }))["removed"], 1);
    assert_eq!(m.ok("remove_result", json!({ "id": "mcp:risks" }))["removed"], 0);
    let rows = s.results_for_version(&v).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plugin_id, "mcp:weekly-summary");
}

#[test]
fn add_result_refusals() {
    let (_t, data, _m, v) = setup();
    let mut m = Mcp::init(&data);
    let add = |items: Value| json!({ "id": "mcp:x", "version_id": v, "agent": { "name": "a" }, "items": items });
    let e = m.refused("add_result", add(summary("w00000", "w99999")));
    assert!(e.contains("unknown word id"), "{e}");
    let e = m.refused("add_result", add(summary("w00008", "w00000")));
    assert!(e.contains("from after to"), "{e}");
    let e = m.refused("add_result", add(json!([{ "type": "slideshow", "slides": [] }])));
    assert!(e.contains("invalid arguments"), "{e}");
    let e = m.refused("add_result", add(json!([{ "type": "summary_card", "title": "a\u{200b}b", "points": [{ "text": "t", "evidence": [{ "from": "w00000", "to": "w00001" }] }] }])));
    assert!(e.contains("hidden characters") && e.contains("items/0/title"), "{e}");
    let e = m.refused("add_result", add(json!([{ "type": "transcript_block", "anchor": { "from": "w00000", "to": "w00001" }, "note": "x".repeat(2001) }])));
    assert!(e.contains("invalid arguments"), "{e}");
    let owner = m.refused("add_result", add(json!([{ "type": "action_items", "items": [{ "text": "t", "owner": "s9", "evidence": [{ "from": "w00000", "to": "w00001" }] }] }])));
    assert!(owner.contains("not a speaker"), "{owner}");
    let e = m.refused("add_result", json!({ "id": "Meetko/Summary", "version_id": v, "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    assert!(e.contains("invalid arguments"), "{e}");
    let e = m.refused("add_result", json!({ "id": "mcp:x", "version_id": "nope.draft.1", "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    assert!(e.contains("not found"), "{e}");
    // too large as a whole
    let row = json!({ "cells": ["x".repeat(1999), "y".repeat(1999)], "evidence": [{ "from": "w00000", "to": "w00001" }] });
    let e = m.refused("add_result", add(json!([{ "type": "table", "columns": ["c", "d"], "rows": vec![row; 300] }])));
    assert!(e.contains("larger than"), "{e}");
    assert!(Store::open(&data).unwrap().results_for_version(&v).unwrap().is_empty(), "nothing was stored");
}

#[test]
fn plugin_results_and_ids_are_out_of_reach() {
    let (tmp, data, _m, v) = setup();
    let s = Store::open(&data).unwrap();
    // a plugin cannot take an id from the agent namespace
    let dir = tmp.path().join("p");
    std::fs::create_dir_all(&dir).unwrap();
    let src = std::fs::read_to_string(root().join("tests/fixtures/plugins/action-items-py/meetko.toml")).unwrap();
    std::fs::write(dir.join("meetko.toml"), src.replace("meetko/action-items-py", "mcp:summary")).unwrap();
    std::fs::write(dir.join("plugin.py"), "").unwrap();
    let (man, sha) = Manifest::load(&dir).unwrap();
    let e = s.add_plugin(&man, &dir, &sha, &plugin_code_sha256(&man, &dir).unwrap()).unwrap_err();
    assert!(e.to_string().contains("reserved"), "{e}");
    // a real plugin result is listed as a plugin result and cannot be removed over MCP
    std::fs::write(dir.join("meetko.toml"), std::fs::read_to_string(root().join("tests/fixtures/plugins/action-items-py/meetko.toml")).unwrap()).unwrap();
    let (man, sha) = Manifest::load(&dir).unwrap();
    let p = s.add_plugin(&man, &dir, &sha, &plugin_code_sha256(&man, &dir).unwrap()).unwrap();
    let out = json!({ "contract": "0.1", "plugin": { "id": p.id, "version": p.version }, "input": { "version_id": v }, "items": summary("w00000", "w00001") });
    s.add_result(&p, &v, &out).unwrap();
    let mut m = Mcp::init(&data);
    let got = m.ok("get_results", json!({ "version_id": v }));
    assert_eq!(got["results"][0]["source"], "plugin");
    assert!(got["results"][0].get("agent").is_none());
    assert!(m.ok("list_agent_results", json!({}))["results"].as_array().unwrap().is_empty());
    let e = m.refused("remove_result", json!({ "id": "meetko/action-items-py" }));
    assert!(e.contains("mcp:"), "{e}");
    assert_eq!(s.results_for_version(&v).unwrap().len(), 1);
    assert_eq!(s.list_plugins().unwrap().len(), 1);
    // versions are untouched by everything above
    assert_eq!(s.versions_of(&_m).unwrap().iter().filter(|r| r.kind == VersionKind::Draft).count(), 1);
}

/// the text of a tool answer as the client hands it to the model
fn answer_bytes(m: &mut Mcp, tool: &str, args: Value) -> (usize, Value) {
    let r = m.rpc("tools/call", json!({ "name": tool, "arguments": args }));
    let text = r["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(r["result"]["isError"], false, "{text}");
    (text.len(), serde_json::from_str(&text).unwrap())
}

const PAGE_LIMIT: usize = 48 * 1024;

#[test]
fn pages_stay_small_and_cover_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let s = Store::open(&data).unwrap();
    let (m_id, _) = meeting(tmp.path(), &s, 0, "긴 회의");
    // every word at the 200-character maximum: the fixture becomes about 65 KB of text
    let mut md = fixture(&m_id);
    for w in &mut md.words {
        w.text = "가".repeat(200);
    }
    let v = s.create_version(md).unwrap().version.id;
    let mut m = Mcp::init(&data);
    for format in ["turns", "words", "text"] {
        let (mut cur, mut turns, mut pages) = (Value::Null, 0, 0);
        loop {
            let (bytes, p) = answer_bytes(&mut m, "get_transcript", json!({ "version_id": v, "format": format, "max_turns": 300, "cursor": cur }));
            assert!(bytes <= PAGE_LIMIT || p["turns"].as_array().is_some_and(|t| t.len() == 1), "{format} page of {bytes} bytes");
            turns += if format == "text" { p["text"].as_str().unwrap().lines().count() } else { p["turns"].as_array().unwrap().len() };
            pages += 1;
            cur = p["next_cursor"].clone();
            if cur.is_null() {
                break;
            }
        }
        assert_eq!(turns, 10, "{format}");
        assert!(pages > 1, "{format} needed paging");
    }

    // a stored result near the 1 MB limit comes back in pieces, none of them whole-result sized
    let row = json!({ "cells": ["x".repeat(1500), "y".repeat(1500)], "evidence": [{ "from": "w00000", "to": "w00001" }] });
    m.ok("add_result", json!({ "id": "big", "version_id": v, "agent": { "name": "a" }, "items": [{ "type": "table", "columns": ["c", "d"], "rows": vec![row; 300] }] }));
    let (mut cur, mut rows, mut pages) = (Value::Null, 0, 0);
    loop {
        let (bytes, p) = answer_bytes(&mut m, "get_results", json!({ "version_id": v, "cursor": cur }));
        assert!(bytes <= PAGE_LIMIT, "results page of {bytes} bytes");
        let r = &p["results"][0];
        assert_eq!(r["id"], "mcp:big");
        assert_eq!(r["continued"].as_bool().unwrap_or(false), pages > 0);
        for it in r["items"].as_array().unwrap() {
            assert!(it["part"].as_str().unwrap().contains('/'));
            rows += it["rows"].as_array().unwrap().len();
        }
        pages += 1;
        cur = p["next_cursor"].clone();
        if cur.is_null() {
            assert!(r.get("continues").is_none());
            break;
        }
        assert_eq!(r["continues"], true);
    }
    assert_eq!(rows, 300);
    assert!(pages > 10);

    // list_agent_results pages too
    for i in 0..55 {
        m.ok("add_result", json!({ "id": format!("n{i}"), "version_id": v, "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    }
    let p1 = m.ok("list_agent_results", json!({}));
    assert_eq!((p1["total"].as_u64(), p1["results"].as_array().unwrap().len()), (Some(56), 50));
    let p2 = m.ok("list_agent_results", json!({ "cursor": p1["next_cursor"] }));
    assert_eq!((p2["results"].as_array().unwrap().len(), p2["next_cursor"].clone()), (6, Value::Null));
}

#[test]
fn cursors_past_the_end_are_refused() {
    let (_t, data, _m, v) = setup();
    let mut m = Mcp::init(&data);
    for big in ["18446744073709551615", "99999999999999999999999", "11"] {
        assert_eq!(m.refused("get_transcript", json!({ "version_id": v, "cursor": big })), "cursor is not one this server handed out", "{big}");
    }
    // the end itself is a valid, empty page
    assert_eq!(m.ok("get_transcript", json!({ "version_id": v, "cursor": "10" }))["turns"], json!([]));
    // keyed cursors: anything that is not one is refused, a key past the end is an empty page
    for bad in ["x", "1.2", "18446744073709551615.0.0", "[1]"] {
        for (tool, args) in [
            ("list_meetings", json!({ "cursor": bad })),
            ("get_results", json!({ "version_id": v, "cursor": bad })),
            ("list_agent_results", json!({ "cursor": bad })),
        ] {
            assert_eq!(m.refused(tool, args), "cursor is not one this server handed out", "{tool} {bad}");
        }
    }
    assert_eq!(m.ok("get_results", json!({ "version_id": v, "cursor": "9223372036854775807.0.0" }))["results"], json!([]));
    assert_eq!(m.ok("list_agent_results", json!({ "cursor": "9223372036854775807" }))["results"], json!([]));
}

#[test]
fn keyed_cursors_survive_changes_between_pages() {
    let (_t, data, m_id, v) = setup();
    let mut m = Mcp::init(&data);
    // list_agent_results: removing a result already shown does not skip the next page's first
    for i in 0..55 {
        m.ok("add_result", json!({ "id": format!("n{i:02}"), "version_id": v, "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    }
    let p1 = m.ok("list_agent_results", json!({}));
    assert_eq!(p1["results"].as_array().unwrap().len(), 50);
    m.ok("remove_result", json!({ "id": "n00" }));
    let p2 = m.ok("list_agent_results", json!({ "cursor": p1["next_cursor"] }));
    assert_eq!(p2["results"][0]["id"], "mcp:n50");
    assert_eq!(p2["results"].as_array().unwrap().len(), 5);
    for i in 0..55 {
        m.ok("remove_result", json!({ "id": format!("n{i:02}") }));
    }

    // get_results: a small result before a big one is removed mid-way; the big one is read once, whole
    m.ok("add_result", json!({ "id": "small", "version_id": v, "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    let row = json!({ "cells": ["x".repeat(1500)], "evidence": [{ "from": "w00000", "to": "w00001" }] });
    m.ok("add_result", json!({ "id": "big", "version_id": v, "agent": { "name": "a" }, "items": [{ "type": "table", "columns": ["c"], "rows": vec![row; 100] }] }));
    let first = m.ok("get_results", json!({ "version_id": v }));
    m.ok("remove_result", json!({ "id": "small" }));
    let mut rows: usize = first["results"].as_array().unwrap().iter().filter(|r| r["id"] == "mcp:big").flat_map(|r| r["items"].as_array().unwrap().clone()).map(|it| it["rows"].as_array().unwrap().len()).sum();
    let mut cur = first["next_cursor"].clone();
    while !cur.is_null() {
        let p = m.ok("get_results", json!({ "version_id": v, "cursor": cur }));
        for r in p["results"].as_array().unwrap() {
            assert_eq!(r["id"], "mcp:big");
            for it in r["items"].as_array().unwrap() {
                assert_eq!(it["item"], 0, "split parts name their item");
                rows += it["rows"].as_array().unwrap().len();
            }
        }
        cur = p["next_cursor"].clone();
    }
    assert_eq!(rows, 100);

    // list_meetings: a meeting imported between pages does not shift the second page
    let tmp2 = tempfile::tempdir().unwrap();
    let s = Store::open(&data).unwrap();
    meeting(tmp2.path(), &s, 1, "둘째");
    meeting(tmp2.path(), &s, 2, "셋째");
    let p1 = m.ok("list_meetings", json!({ "limit": 2 }));
    assert_eq!(p1["meetings"][1]["title"], "둘째");
    meeting(tmp2.path(), &s, 3, "넷째"); // newest: belongs before the first page
    let p2 = m.ok("list_meetings", json!({ "limit": 2, "cursor": p1["next_cursor"] }));
    assert_eq!(p2["meetings"].as_array().unwrap().len(), 1);
    assert_eq!(p2["meetings"][0]["meeting_id"], m_id.as_str());
    assert_eq!(p2["next_cursor"], Value::Null);
}

#[test]
fn search_answers_stay_small() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let s = Store::open(&data).unwrap();
    for i in 0..50 {
        let (m_id, _) = meeting(tmp.path(), &s, i, &format!("{} {i}", "제목".repeat(40)));
        let mut md = fixture(&m_id);
        for w in md.words.iter_mut().step_by(3) {
            w.text = format!("배포{}", "가".repeat(60));
        }
        s.create_version(md).unwrap();
    }
    let mut m = Mcp::init(&data);
    let (bytes, r) = answer_bytes(&mut m, "search", json!({ "query": "배포", "limit": 50 }));
    assert!(bytes <= PAGE_LIMIT, "{bytes} bytes");
    assert_eq!(r["more"], true);
    let n = r["meetings"].as_array().unwrap().len();
    assert!(n > 0 && n < 50, "the cap cut the list ({n})");
}

#[test]
fn small_refusals_and_namespace_case() {
    let (_t, data, _m, v) = setup();
    let mut m = Mcp::init(&data);
    assert!(m.refused("list_agent_results", json!({ "meeting_id": "nope" })).contains("not found"));
    let r = m.ok("add_result", json!({ "id": "MCP:cased", "version_id": v, "agent": { "name": "a" }, "items": summary("w00000", "w00001") }));
    assert_eq!(r["id"], "mcp:cased");
}

#[test]
fn discover_needs_its_meta() {
    let (_t, data, ..) = setup();
    let mut m = Mcp::start(&data);
    assert_eq!(m.rpc("server/discover", json!({}))["error"]["code"], -32602);
    assert_eq!(m.rpc("server/discover", Value::Null)["error"]["code"], -32602);
}

#[test]
fn provenance_paths_never_go_out() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let s = Store::open(&data).unwrap();
    let (m_id, _) = meeting(tmp.path(), &s, 0, "t");
    let mut md = fixture(&m_id);
    md.version.producers[0]["model"]["id"] = json!("/Volumes/Ext/models/ggml.bin");
    md.version.producers[0]["params"]["dtw"] = json!("large /Volumes/Ext/x");
    md.version.producers[0]["params"]["model_path"] = json!("D:\\models\\m.bin");
    md.version.producers[0]["params"]["/Volumes/Ext/key"] = json!(1);
    md.version.producers[0]["params"]["nested"] = json!({ "p": "/home/someone/m" });
    md.version.producers[1]["cache"] = json!("relative/cache/dir");
    let v = s.create_version(md).unwrap().version.id;
    let mut m = Mcp::init(&data);
    let p = m.ok("get_provenance", json!({ "version_id": v }));
    let out = p.to_string();
    for bad in ["Volumes", "models", "someone", "relative", "D:"] {
        assert!(!out.contains(bad), "{bad} in {out}");
    }
    assert_eq!(p["producers"][0]["params"]["language"], "ko");
    assert_eq!(p["producers"][0]["hidden_fields"], 5);
    assert_eq!(p["producers"][1]["hidden_fields"], 1);
}

#[test]
fn meetko_results_names_the_agent() {
    let (_t, data, _m, v) = setup();
    let mut m = Mcp::init(&data);
    m.ok("add_result", json!({ "id": "sum", "version_id": v, "agent": { "name": "Codex", "version": "2" }, "items": summary("w00000", "w00001") }));
    let out = Command::new(env!("CARGO_BIN_EXE_meetko")).args(["results", &v]).env("MEETKO_DATA_DIR", &data).output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("mcp:sum@2"), "{text}");
    assert!(text.contains("made by an AI agent over MCP: Codex 2 via test-client 9.9"), "{text}");
}
