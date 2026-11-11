//! Headless tests of the command layer: the same functions the Tauri commands call.

use std::sync::atomic::AtomicBool;

use meetko_core::store::{ImportChannel, ImportOptions};
use meetko_core::whisper::{BatchEngine, BatchRequest, Progress};
use serde_json::{json, Value};

use super::*;

/// Returns the same Korean sentence on every channel; `text` varies the content.
struct Fake(&'static [&'static str]);

impl BatchEngine for Fake {
    fn transcribe(&self, req: &BatchRequest, progress: &dyn Fn(Progress), _cancel: &AtomicBool) -> meetko_core::Result<Value> {
        progress(Progress { channel: req.channel, channels: req.channels, percent: 100, diarizing: false });
        let words: Vec<Value> = self.0.iter().enumerate().map(|(i, t)| json!({"start_ms": i * 300, "end_ms": i * 300 + 250, "text": t})).collect();
        Ok(json!({
            "contract": "0.1", "kind": "asr.batch", "engine": {"id": "fake", "version": "1"},
            "model": {"id": "fake-model"}, "audio_sha256": req.audio_sha256, "params": {}, "word_timing": "token",
            "network": false, "coverage": [{"start_ms": 0, "end_ms": 2000}], "words": words
        }))
    }
}

fn wav(path: &std::path::Path, seed: i16) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..32000i32 {
        w.write_sample(((i % 100) as i16 - 50) * seed).unwrap();
    }
    w.finalize().unwrap();
}

fn setup() -> (tempfile::TempDir, Core, String) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let (m, l) = (tmp.path().join("m.wav"), tmp.path().join("l.wav"));
    wav(&m, 100);
    wav(&l, 200);
    let ch = |index, role: &str, path, label: &str| ImportChannel {
        index,
        role: role.into(),
        label: Some(label.into()),
        path,
        expected_sha256: None,
        coverage: None,
    };
    let core = Core::new(data.clone());
    let id = core
        .store()
        .unwrap()
        .import_meeting(vec![ch(0, "mic", m, MIC_LABEL), ch(1, "loopback", l, LOOPBACK_LABEL)], ImportOptions { title: Some("테스트".into()), ..Default::default() })
        .unwrap()
        .meeting
        .id;
    (tmp, core, id)
}

fn no_progress(_: Progress) {}

#[test]
fn transcribe_approve_resolve_and_list() {
    let (_t, core, id) = setup();
    assert_eq!(core.list_meetings().unwrap()[0].latest_kind, None);
    let draft = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let list = core.list_meetings().unwrap();
    assert_eq!((list[0].latest_kind, list[0].latest_version_id.as_deref()), (Some("draft"), Some(draft.as_str())));

    let raw = core.get_version(&draft).unwrap();
    assert_eq!(raw["version"]["producers"][0]["engine"]["id"], "fake");
    let words = raw["words"].as_array().unwrap();
    let (a, b) = (words[0]["id"].as_str().unwrap(), words.last().unwrap()["id"].as_str().unwrap());
    let anchor = core.resolve_anchor(&draft, a, b).unwrap();
    assert_eq!((anchor.start_ms, anchor.end_ms), (0, 850));
    assert_eq!(core.resolve_anchor(&draft, b, a).err().unwrap().code, "invalid_data");

    let approved = core.approve(&draft).unwrap();
    assert!(approved.contains("approved"));
    assert_eq!(core.approve(&approved).err().unwrap().code, "invalid_data", "only drafts are approved");
    let detail = core.get_meeting(&id).unwrap();
    assert_eq!(detail.versions.iter().map(|v| v.kind).collect::<Vec<_>>(), ["draft", "approved"]);
    assert_eq!(detail.channels[0].label.as_deref(), Some("나"));
}

#[test]
fn search_finds_the_head_and_its_anchor_resolves() {
    let (_t, core, id) = setup();
    assert!(core.search("리뷰").unwrap().meetings.is_empty(), "nothing is transcribed yet");
    let draft = core.transcribe(&id, &Fake(&["API", "서버는", "제가", "볼게요"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let found = core.search("api 서버").unwrap().meetings;
    assert_eq!(found.len(), 1);
    let m = &found[0];
    assert_eq!((m.meeting_id.as_str(), m.version_id.as_str(), m.kind.as_str(), m.title.as_deref()), (id.as_str(), draft.as_str(), "draft", Some("테스트")));
    // both channels said it: one hit per channel
    assert_eq!(m.matches, 2);
    let h = &m.hits[0];
    let a = core.resolve_anchor(&m.version_id, &h.from, &h.to).unwrap();
    assert_eq!((a.start_ms, a.end_ms), (h.start_ms, h.end_ms));
    for q in ["\"", "*", "a OR b", "%_", ""] {
        core.search(q).unwrap_or_else(|e| panic!("{q:?}: {}", e.message));
    }
    let approved = core.approve(&draft).unwrap();
    assert_eq!(core.search("서버").unwrap().meetings[0].version_id, approved);
}

#[test]
fn cancel_flag_reaches_the_pipeline() {
    let (_t, core, id) = setup();
    let e = core.transcribe(&id, &Fake(&["가"]), &no_progress, &AtomicBool::new(true)).err().unwrap();
    assert_eq!(e.code, "cancelled");
}

#[test]
fn an_agent_result_is_listed_with_who_made_it_and_is_not_a_plugin() {
    let (_t, core, id) = setup();
    let d1 = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let words = core.get_version(&d1).unwrap()["words"].clone();
    let (from, to) = (words[0]["id"].as_str().unwrap().to_string(), words[2]["id"].as_str().unwrap().to_string());
    let out = json!({ "contract": "0.1", "plugin": { "id": "mcp:review", "version": "1" }, "input": { "version_id": d1 },
        "items": [{ "type": "summary_card", "title": "요약", "points": [{ "text": "리뷰 담당", "evidence": [{ "from": from, "to": to }] }] }] });
    let prov = json!({ "via": "mcp", "network": true, "client": { "name": "codex", "version": "1" }, "agent": { "name": "gpt", "version": "1" } });
    core.store().unwrap().put_agent_result("mcp:review", &prov, &d1, &out).unwrap();
    let r = &core.list_results(&d1).unwrap()[0];
    assert_eq!((r.plugin_id.as_str(), r.sandbox.as_str(), r.declared_network), ("mcp:review", "mcp", true));
    assert_eq!(r.agent.as_ref().unwrap()["client"]["name"], "codex");
    assert!(core.list_plugins().unwrap().is_empty());
    assert_eq!(core.run_plugin("mcp:review", &d1).err().unwrap().code, "not_found");
}

#[test]
fn plugin_flow_and_stale_badge() {
    let (_t, core, id) = setup();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
    let shown = core.inspect_plugin(&dir).unwrap();
    assert!(!shown.network);
    if cfg!(target_os = "macos") && std::env::var("MEETKO_PLUGIN_SANDBOX").as_deref() != Ok("off") {
        assert_eq!(shown.notice, "sandboxed: no network unless declared");
        assert!(shown.isolated);
    } else {
        assert_eq!(shown.notice, "declared, not enforced");
        assert!(!shown.isolated);
    }
    // a hash that is not the one on screen is refused
    assert_eq!(core.add_plugin(&dir, &"0".repeat(64), None).err().unwrap().code, "plugin_changed");
    let added = core.add_plugin(&dir, &shown.code_sha256, None).unwrap();
    assert_eq!(added.code_sha256, shown.code_sha256, "the installed copy has the shown hash");
    assert_eq!(core.list_plugins().unwrap()[0].code_sha256, shown.code_sha256);

    let d1 = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let r = core.run_plugin("meetko/action-items-py", &d1).unwrap();
    assert!(!r.stale);
    assert_eq!(r.plugin_code_sha256, shown.code_sha256);
    assert!(!r.declared_network);
    assert_eq!(r.declared_audio, "none");
    assert_eq!(r.output["items"][0]["type"], "action_items");
    assert_eq!(core.list_results(&d1).unwrap().len(), 1);
    assert_eq!(core.list_meetings().unwrap()[0].stale_count, 0);

    // a different approved transcript becomes the head: the first result is stale
    let d2 = core.transcribe(&id, &Fake(&["다른", "내용입니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    core.approve(&d2).unwrap();
    assert!(core.list_results(&d1).unwrap()[0].stale);
    assert_eq!(core.list_meetings().unwrap()[0].stale_count, 1);
    assert_eq!(core.list_stale(&id).unwrap().len(), 1);
    assert_eq!(core.list_stale(&id).unwrap()[0].version_id, d1);
}

/// A local server for the gallery path: the index, `meetko/r` (default branch main, `compare` says
/// `behind`) and its tarball for SHA. Endpoints are passed to the calls explicitly (no process-wide
/// environment, so these tests cannot race each other).
struct Gallery {
    base: String,
}

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

impl Gallery {
    fn start(manifest_edit: impl Fn(String) -> String, entry_edit: impl Fn(String) -> String) -> Gallery {
        use std::io::Write;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
        let mut b = tar::Builder::new(Vec::new());
        for name in ["meetko.toml", "plugin.py"] {
            let mut data = std::fs::read(dir.join(name)).unwrap();
            if name == "meetko.toml" {
                data = manifest_edit(String::from_utf8(data).unwrap()).into_bytes();
            }
            let mut h = tar::Header::new_gnu();
            let path = format!("meetko-r-{SHA}/{name}");
            h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, data.as_slice()).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&b.into_inner().unwrap()).unwrap();
        let tarball = gz.finish().unwrap();

        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
        let index = entry_edit(format!(
            r#"{{"generated":"x","plugins":[{{"id":"meetko/action-items-py","name":"action-items-py","version":"0.1.0","description":"d",
            "repo":"meetko/r","ref":"v1","sha":"{SHA}","permissions":{{"network":false,"audio":false}},"runtime":"python","license":"x","stars":1,"updated":"x"}}]}}"#
        ));
        std::thread::spawn(move || {
            for req in srv.incoming_requests() {
                let url = req.url().to_string();
                let _ = if url == "/index.json" {
                    req.respond(tiny_http::Response::from_string(index.clone()))
                } else if url == "/repos/meetko/r" {
                    req.respond(tiny_http::Response::from_string(r#"{"default_branch":"main"}"#))
                } else if url == format!("/repos/meetko/r/compare/main...{SHA}") {
                    req.respond(tiny_http::Response::from_string(r#"{"status":"behind"}"#))
                } else if url == format!("/meetko/r/tar.gz/{SHA}") {
                    req.respond(tiny_http::Response::from_data(tarball.clone()))
                } else {
                    req.respond(tiny_http::Response::from_string("no").with_status_code(404))
                };
            }
        });
        Gallery { base }
    }

    fn index_url(&self) -> String {
        format!("{}/index.json", self.base)
    }

    fn endpoints(&self) -> plugin_source::Endpoints {
        plugin_source::Endpoints { api: self.base.clone(), codeload: self.base.clone() }
    }
}

use meetko_core::plugin_source;

#[test]
fn gallery_list_preview_install() {
    let g = Gallery::start(|m| m, |i| i);
    let (_t, core, _id) = setup();
    let list = core.gallery_list(&g.index_url()).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].permissions.network);
    let (fetched, preview) = core.gallery_preview(&list[0], &g.endpoints()).unwrap();
    assert_eq!(preview.id, "meetko/action-items-py");
    assert_eq!(preview.folder, "r");
    assert!(preview.source.as_deref().unwrap().starts_with("github:meetko/r @ "));
    assert!(preview.replaces.is_none());
    assert_eq!(core.gallery_install(&fetched, &"0".repeat(64), None).err().unwrap().code, "plugin_changed");
    let added = core.gallery_install(&fetched, &preview.code_sha256, None).unwrap();
    assert_eq!(added.code_sha256, preview.code_sha256);
    let listed = core.list_plugins().unwrap();
    assert_eq!(listed[0].source.as_deref(), preview.source.as_deref());
}

/// The tarball (`git archive`, so export-subst applies) says something other than the index row.
#[test]
fn gallery_refuses_a_download_that_differs_from_the_index_entry() {
    let list_of = |g: &Gallery, core: &Core| core.gallery_list(&g.index_url()).unwrap();
    type Edit = Box<dyn Fn(String) -> String>;
    let cases: [(&str, Edit); 4] = [
        ("version", Box::new(|m| m.replace("version = \"0.1.0\"", "version = \"9.9.9\""))),
        ("network", Box::new(|m| if m.contains("[permissions]") { m.replace("network = false", "network = true") } else { m + "\n[permissions]\nnetwork = true\n" })),
        ("audio", Box::new(|m| if m.contains("[input]") { m.replace("audio = \"none\"", "audio = \"full\"") } else { m + "\n[input]\naudio = \"full\"\n" })),
        ("id", Box::new(|m| m.replace("meetko/action-items-py", "meetko/other"))),
    ];
    for (what, edit) in cases {
        let g = Gallery::start(edit, |i| i);
        let (_t, core, _id) = setup();
        let list = list_of(&g, &core);
        let e = core.gallery_preview(&list[0], &g.endpoints()).err().unwrap_or_else(|| panic!("{what} accepted"));
        assert_eq!(e.code, "index_mismatch", "{what}: {}", e.message);
    }
    // an index row that lies about the permission the manifest does not have
    let g = Gallery::start(|m| m, |i| i.replace("\"network\":false", "\"network\":true"));
    let (_t, core, _id) = setup();
    let list = list_of(&g, &core);
    assert_eq!(core.gallery_preview(&list[0], &g.endpoints()).err().unwrap().code, "index_mismatch");
}

/// An id installed from elsewhere is flagged in the preview and needs the second confirmation.
#[test]
fn gallery_install_over_a_local_plugin_needs_replace() {
    let g = Gallery::start(|m| m, |i| i);
    let (_t, core, _id) = setup();
    let local = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
    let shown = core.inspect_plugin(&local).unwrap();
    assert!(shown.replaces.is_none());
    core.add_plugin(&local, &shown.code_sha256, None).unwrap();

    let list = core.gallery_list(&g.index_url()).unwrap();
    let (fetched, preview) = core.gallery_preview(&list[0], &g.endpoints()).unwrap();
    let r = preview.replaces.as_ref().expect("the preview says what would be replaced");
    assert_eq!((r.version.as_str(), r.source.as_deref()), ("0.1.0", None), "a local folder");
    assert_eq!(core.gallery_install(&fetched, &preview.code_sha256, None).err().unwrap().code, "plugin_source_changed");
    assert!(core.list_plugins().unwrap()[0].source.is_none(), "untouched");
    assert_eq!(core.gallery_install(&fetched, &preview.code_sha256, Some("stale|local")).err().unwrap().code, "plugin_source_changed");
    core.gallery_install(&fetched, &preview.code_sha256, Some(&r.key)).unwrap();
    assert!(core.list_plugins().unwrap()[0].source.is_some());
    // and the local pick over the GitHub plugin is flagged the other way
    let shown = core.inspect_plugin(&local).unwrap();
    assert!(shown.replaces.unwrap().source.unwrap().starts_with("github:meetko/r"));
    assert_eq!(core.add_plugin(&local, &preview.code_sha256, None).err().unwrap().code, "plugin_source_changed");
}

/// Only the token of the preview still pending installs; a stale click leaves the current one armed.
#[test]
fn a_gallery_install_needs_the_token_of_the_current_preview() {
    let g = Gallery::start(|m| m, |i| i);
    let (_t, core, _id) = setup();
    let list = core.gallery_list(&g.index_url()).unwrap();
    let mut pending = PendingGallery::default();
    let (f1, _) = core.gallery_preview(&list[0], &g.endpoints()).unwrap();
    let t1 = pending.set(f1);
    let (f2, _) = core.gallery_preview(&list[0], &g.endpoints()).unwrap();
    let t2 = pending.set(f2);
    assert_ne!(t1, t2);
    assert!(pending.take(&t1).is_err(), "the click of the replaced preview");
    assert!(pending.take("").is_err());
    assert!(pending.take(&t2).is_ok(), "still armed after the stale click");
    assert!(pending.take(&t2).is_err(), "taken once");
}

#[test]
fn audio_ranges_single_and_mix() {
    let (_t, core, id) = setup();
    let total = 44 + 64000;
    let all = core.audio(&id, "0", Some("bytes=0-1")).unwrap();
    assert_eq!((all.status, all.body.as_slice(), all.total), (206, &b"RI"[..], total));
    assert_eq!(all.content_range.as_deref(), Some("bytes 0-1/64044"));
    let tail = core.audio(&id, "1", Some("bytes=-4")).unwrap();
    assert_eq!(tail.body.len(), 4);
    assert_eq!(core.audio(&id, "1", Some("bytes=99999999-")).err(), Some(416));
    // a whole-file request without Range is capped, and says 200 only when it fits
    assert_eq!(core.audio(&id, "0", None).unwrap().body.len(), 64044);

    // mix: header from channel 0 with sizes intact, samples summed; odd offsets are aligned
    let m: Vec<i16> = hound::WavReader::open(core.channel_path(&id, 0)).unwrap().samples::<i16>().map(Result::unwrap).collect();
    let l: Vec<i16> = hound::WavReader::open(core.channel_path(&id, 1)).unwrap().samples::<i16>().map(Result::unwrap).collect();
    let full = core.audio(&id, "mix", None).unwrap();
    assert_eq!(&full.body[..4], b"RIFF");
    let odd = core.audio(&id, "mix", Some("bytes=47-52")).unwrap(); // sample 1 high byte .. sample 4 low byte
    let sum = |i: usize| (i32::from(m[i]) + i32::from(l[i])).clamp(-32768, 32767) as i16;
    let expect: Vec<u8> = (1..=4usize).flat_map(|i| sum(i).to_le_bytes()).collect::<Vec<_>>()[1..=6].to_vec();
    assert_eq!(odd.body, expect);
    // the mixed file parses as a WAV of the same length
    let mut mixed = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut mixed, &full.body).unwrap();
    assert_eq!(hound::WavReader::open(mixed.path()).unwrap().duration(), 32000);
}

#[test]
fn audio_refuses_anything_but_a_known_meeting_and_channel() {
    let (_t, core, id) = setup();
    assert_eq!(core.audio("nope", "0", None).err(), Some(404));
    assert_eq!(core.audio(&id, "7", None).err(), Some(404));
    assert_eq!(core.audio(&id, "../0", None).err(), Some(404));
}

#[test]
fn status_reports_setup_without_failing() {
    let (_t, core, _id) = setup();
    let s = core.status();
    // whatever the environment holds, a missing piece carries a code the UI can explain
    for part in [&s.model, &s.whisper_cli, &s.capture, &s.live] {
        assert_eq!(part.ok, part.code.is_none());
    }
}

/// A finished capture directory (manifest last) with 2 s of audio per channel.
fn capture_dir(root: &std::path::Path) -> std::path::PathBuf {
    let dir = root.join("captures").join("c1");
    std::fs::create_dir_all(&dir).unwrap();
    wav(&dir.join("mic.wav"), 100);
    wav(&dir.join("loopback.wav"), 200);
    let sums: Vec<String> = ["mic.wav", "loopback.wav"].iter().map(|n| util::sha256_file(&dir.join(n)).unwrap()).collect();
    let rec = util::sha256_bytes(format!("0:{}\n1:{}\n", sums[0], sums[1]).as_bytes());
    let ch = |i: usize, role: &str, f: &str| json!({"index": i, "role": role, "file": f, "frames": 32000, "sha256": sums[i], "device": "default_input", "dropped_buffers": 0, "gaps": []});
    let m = json!({"contract": "0.1", "kind": "capture", "provider": {"id": "fake", "version": "0"}, "started_at": "2026-10-27T09:18:22.781Z",
        "sample_rate": 16000, "duration_ms": 2000, "complete": true, "end_reason": "user", "recording_sha256": rec,
        "channels": [ch(0, "mic", "mic.wav"), ch(1, "loopback", "loopback.wav")]});
    std::fs::write(dir.join("capture.json"), m.to_string()).unwrap();
    dir
}

fn live_channel(index: u32, text: &str) -> meetko_core::live::ChannelLive {
    let ev = json!({"contract": "0.1", "kind": "asr.stream", "segment": 0, "revision": 3, "final": true, "start_ms": 100, "text": text,
        "emitted_at_ms": 1500, "units": [{"text": " 안녕", "start_ms": 200}, {"text": "하세요", "start_ms": 500}]});
    meetko_core::live::ChannelLive {
        index, events: vec![ev], engine: Some(json!({"id": "fake/stream", "version": "1"})), model: Some(json!({"id": "fake-model"})),
        model_sha256: Some("a".repeat(64)), model_pinned: true, params: json!({}), complete: true, covered_ms: 1500, error: None, rejected: 0,
    }
}

#[test]
fn a_finished_capture_stores_its_live_version_beside_the_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let core = Core::new(tmp.path().join("data"));
    let outcome = CaptureOutcome { dir: capture_dir(tmp.path()), exit_code: Some(0), killed: false, errors: vec![] };
    let mut live = meetko_core::live::LiveOutcome::default();
    live.channels.insert(1, live_channel(1, "안녕하세요"));
    let done = core.finish_capture(&outcome, Some(&live));
    assert!(done.error.is_none() && done.live_note.is_none(), "{:?} {:?}", done.error, done.live_note);
    let id = done.meeting_id.unwrap();
    let detail = core.get_meeting(&id).unwrap();
    assert_eq!(detail.versions.len(), 1);
    assert_eq!(detail.versions[0].kind, "live");
    assert_eq!(done.live_version_id.as_deref(), Some(detail.versions[0].id.as_str()));
    let v = core.get_version(&detail.versions[0].id).unwrap();
    assert_eq!(v["words"].as_array().unwrap().len(), 1, "one word, the two units join");
    assert_eq!(v["words"][0]["channel"], 1);
    assert_eq!(v["version"]["producers"][0]["model"]["sha256"], "a".repeat(64), "the model digest reaches the producer");
}

/// Without whisper: capture -> live -> first draft -> approve -> plugin. Batch is only an extra.
#[test]
fn the_live_captions_become_the_first_draft_without_whisper() {
    let tmp = tempfile::tempdir().unwrap();
    let core = Core::new(tmp.path().join("data"));
    let outcome = CaptureOutcome { dir: capture_dir(tmp.path()), exit_code: Some(0), killed: false, errors: vec![] };
    let mut live = meetko_core::live::LiveOutcome::default();
    live.channels.insert(1, live_channel(1, "안녕하세요"));
    let done = core.finish_capture(&outcome, Some(&live));
    let (id, live_id) = (done.meeting_id.unwrap(), done.live_version_id.unwrap());
    assert_eq!(core.live_draft_source(&id).unwrap().as_deref(), Some(live_id.as_str()));
    let (draft, warnings) = core.draft_from_live(&id, None, &no_progress, &AtomicBool::new(false)).unwrap();
    assert!(warnings.is_empty());
    let v = core.get_version(&draft).unwrap();
    assert_eq!((v["version"]["kind"].as_str(), v["version"]["parent"].as_str()), (Some("draft"), Some(live_id.as_str())));
    assert_eq!(v["version"]["producers"][0]["engine"]["id"], "fake/stream");
    assert_eq!(v["version"]["producers"][0]["network"], false);
    assert_eq!(core.live_draft_source(&id).unwrap(), None, "made once");
    assert_eq!(core.draft_from_live(&id, None, &no_progress, &AtomicBool::new(false)).unwrap().0, draft, "asking again is the same draft");
    let approved = core.approve(&draft).unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
    let shown = core.inspect_plugin(&dir).unwrap();
    core.add_plugin(&dir, &shown.code_sha256, None).unwrap();
    assert!(!core.run_plugin("meetko/action-items-py", &approved).unwrap().stale);
    // a meeting without live captions has nothing to make a draft from
    let (_t, core2, other) = setup();
    assert_eq!(core2.live_draft_source(&other).unwrap(), None);
    assert_eq!(core2.draft_from_live(&other, None, &no_progress, &AtomicBool::new(false)).err().unwrap().code, "not_found");
}

#[test]
fn a_live_failure_never_costs_the_saved_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let core = Core::new(tmp.path().join("data"));
    let outcome = CaptureOutcome { dir: capture_dir(tmp.path()), exit_code: Some(0), killed: false, errors: vec![] };
    let mut dead = live_channel(0, "x");
    dead.engine = None; // the engine never started
    let mut live = meetko_core::live::LiveOutcome::default();
    live.channels.insert(0, dead);
    let done = core.finish_capture(&outcome, Some(&live));
    assert!(done.meeting_id.is_some() && done.error.is_none(), "{:?}", done.error);
    assert_eq!(done.live_note, Some("live_unavailable"));
    assert!(core.get_meeting(&done.meeting_id.unwrap()).unwrap().versions.is_empty());
}

#[test]
fn names_and_edits_reach_every_export_and_a_new_transcript_sets_them_aside() {
    let (_t, core, id) = setup();
    let draft = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let md = core.store().unwrap().version(&draft).unwrap();
    let (me, other) = (md.speakers[0].id.clone(), md.speakers[1].id.clone());
    let turn = md.turns[0].id.clone();
    core.change_overlay(&draft, OverlayChange::Rename { speaker_id: other.clone(), name: "민수".into() }).unwrap();
    let v = core.change_overlay(&draft, OverlayChange::Edit { turn_id: turn.clone(), text: "리뷰는 제가 맡을게요".into() }).unwrap();
    assert_eq!(v.overlay.edits[&turn].text, "리뷰는 제가 맡을게요");
    let speakers = core.export_version(&draft, "speakers", None).unwrap().text;
    assert!(speakers.starts_with("나: 리뷰는 제가 맡을게요\n민수: 리뷰는"), "{speakers}");
    for f in ["md", "txt", "srt", "vtt"] {
        let t = core.export_version(&draft, f, None).unwrap().text;
        assert!(t.contains("민수") && !t.contains("상대") && t.contains("리뷰는 제가 맡을게요"), "{f}: {t}");
    }
    // merged and reset
    core.change_overlay(&draft, OverlayChange::Merge { speaker_id: me.clone(), into: other.clone() }).unwrap();
    assert!(core.export_version(&draft, "speakers", None).unwrap().text.lines().all(|l| l.starts_with("민수: ")));
    core.change_overlay(&draft, OverlayChange::ResetSpeaker { speaker_id: me }).unwrap();
    let v = core.change_overlay(&draft, OverlayChange::ResetTurn { turn_id: turn }).unwrap();
    assert!(v.overlay.edits.is_empty() && v.set_aside.edits == 0);
    core.change_overlay(&draft, OverlayChange::Edit { turn_id: md.turns[0].id.clone(), text: "고친 문장".into() }).unwrap();
    // 다시 받아쓰기 with other words: the new draft starts clean and says what it left behind
    let again = core.transcribe(&id, &Fake(&["회의를", "시작할게요"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let v = core.overlay(&again).unwrap();
    assert!(v.overlay.is_empty());
    assert_eq!((v.set_aside.edits, v.set_aside.speakers, v.set_aside.version_id.as_deref()), (1, 1, Some(draft.as_str())));
    assert!(!core.export_version(&again, "txt", None).unwrap().text.contains("민수"));
    let v = core.change_overlay(&again, OverlayChange::DiscardSetAside).unwrap();
    assert_eq!(v.set_aside.edits + v.set_aside.speakers, 0);
    assert!(core.overlay(&draft).unwrap().overlay.is_empty());
    assert_eq!(core.change_overlay(&again, OverlayChange::Rename { speaker_id: "s9".into(), name: "x".into() }).err().unwrap().code, "not_found");
    assert_eq!(core.change_overlay(&again, OverlayChange::Edit { turn_id: v.overlay.edits.keys().next().cloned().unwrap_or("t0000".into()), text: " ".into() }).err().unwrap().code, "invalid_data");
}

#[test]
fn export_says_what_it_is_in_every_format() {
    let (_t, core, id) = setup();
    let draft = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let md = core.export_version(&draft, "md", None).unwrap();
    assert_eq!(md.file_name, "테스트-초안.md");
    assert!(md.text.contains("kind: \"draft\"\n") && md.text.contains(&format!("> {EXPORT_NOTE_DRAFT}")));
    assert!(md.text.contains("  - \"asr: fake 1 (fake-model)\""));
    assert!(md.text.contains("## 전사\n\n[00:00:00] **나**: 리뷰는\n\n[00:00:00] **상대**: 리뷰는\n"), "{}", md.text);
    let vtt = core.export_version(&draft, "vtt", None).unwrap();
    assert!(vtt.text.starts_with(&format!("WEBVTT\n\nNOTE {EXPORT_NOTE_DRAFT}\n")) && vtt.text.contains("<v 나>"));
    assert!(core.export_version(&draft, "txt", None).unwrap().text.starts_with(&format!("테스트\n{EXPORT_NOTE_DRAFT}\n\n[00:00:00] 나: ")));
    assert_eq!(core.export_version(&draft, "srt", None).unwrap().file_name, "테스트-초안.srt");
    assert!(core.export_version(&draft, "plain", None).unwrap().text.starts_with("리뷰는\n리뷰는\n제가"));
    assert!(core.export_version(&draft, "speakers", None).unwrap().text.starts_with("나: 리뷰는"));
    assert_eq!(core.export_version(&draft, "pdf", None).err().unwrap().code, "invalid_data");
    assert_eq!(core.export_version("nope", "md", None).err().unwrap().code, "not_found");

    let approved = core.approve(&draft).unwrap();
    let md = core.export_version(&approved, "md", None).unwrap();
    assert_eq!(md.file_name, "테스트.md");
    assert!(md.text.contains("kind: \"approved\"\n") && !md.text.contains(EXPORT_NOTE_DRAFT));
    assert!(!core.export_version(&approved, "vtt", None).unwrap().text.contains("NOTE"));

    // the screen's title wins, kept on one line
    let md = core.export_version(&approved, "md", Some("회의 11월 10일 14:05\n# 둘째 줄")).unwrap();
    assert_eq!(md.file_name, "회의 11월 10일 14-05 # 둘째 줄.md");
    assert!(md.text.contains("\n# 회의 11월 10일 14:05 \\# 둘째 줄\n"));
    assert_eq!(core.export_version(&approved, "md", Some("  ")).unwrap().file_name, "테스트.md");
}

#[test]
fn file_stems_are_safe_on_every_system() {
    assert_eq!(file_stem("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
    assert_eq!(file_stem(" ..hidden. . "), "hidden");
    assert_eq!(file_stem(". . ."), "meetko");
    assert_eq!(file_stem("  "), "meetko");
    assert_eq!(file_stem("줄\n바꿈"), "줄-바꿈");
    // bidi override, zero-width space, BOM
    assert_eq!(file_stem("a\u{202e}b\u{200b}c\u{feff}"), "a-b-c-");
    // bytes, not characters, and never inside a character
    let long = file_stem(&"가".repeat(200));
    assert_eq!((long.len(), long.chars().count()), (198, 66));
    assert!(file_stem(&format!("a{}", "가".repeat(100))).len() <= FILE_STEM_MAX_BYTES);
    for (name, want) in [("CON", "_CON"), ("nul", "_nul"), ("Com1.txt", "_Com1.txt"), ("lpt9", "_lpt9"), ("aux .md", "_aux .md"), ("COM¹", "_COM¹"), ("lpt³.txt", "_lpt³.txt"), ("conin$", "_conin$"), ("CONOUT$.md", "_CONOUT$.md")] {
        assert_eq!(file_stem(name), want);
    }
    for name in ["CONSOLE", "COM", "LPT10", "comx", "회의 CON", "COM⁴", "CONIN"] {
        assert_eq!(file_stem(name), name);
    }
}

#[test]
fn model_pieces_say_ready_missing_or_bad_without_a_path() {
    let tmp = tempfile::tempdir().unwrap();
    let o = model_setup::offer("stream").unwrap();
    let p = model_piece(o, None, false, tmp.path(), false);
    assert_eq!((p.key, p.state, p.from_env, p.engine, p.partial, p.size), ("stream", "missing", false, false, 0, o.size));
    assert_eq!((p.license, p.publisher, p.host), ("OpenMDW-1.1", "NVIDIA", "huggingface.co"));
    // an interrupted download is reported, so the button can say it resumes
    std::fs::create_dir_all(o.dir(tmp.path())).unwrap();
    std::fs::write(o.dir(tmp.path()).join("model.q8_0.gguf.part"), vec![0u8; 1234]).unwrap();
    assert_eq!(model_piece(o, None, false, tmp.path(), true).partial, 1234);
    // a file in the app's folder that is not the pinned model is "bad", never used
    std::fs::write(o.path(tmp.path()), b"not the model").unwrap();
    let p = model_piece(o, None, false, tmp.path(), true);
    assert_eq!(p.state, "bad");
    let text = serde_json::to_string(&p).unwrap();
    assert!(!text.contains(&tmp.path().display().to_string()), "no path reaches the webview: {text}");
    // the development switch does not cover the app's own folder
    assert_eq!(model_piece(o, None, true, tmp.path(), true).state, "bad");
    // the variable wins, also over a file of the app's
    let p = model_piece(o, Some(tmp.path().join("nowhere")), false, tmp.path(), true);
    assert_eq!((p.state, p.from_env, p.partial), ("bad", true, 0));
    // 나중에 is remembered in app-settings.json, beside the meeting detection switches
    let core = Core::new(tmp.path().to_path_buf());
    assert!(!core.model_setup().card_dismissed);
    core.dismiss_setup_card().unwrap();
    assert!(core.model_setup().card_dismissed);
    assert!(crate::meeting_watch::load_settings(tmp.path()).enabled, "the meeting detection default is untouched");
    let d = model_setup::offer("diarization").unwrap();
    assert_eq!(model_piece(d, None, false, tmp.path(), false).state, "missing");
}

#[test]
fn the_batch_engine_is_a_kept_choice_and_auto_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let core = Core { data_dir: tmp.path().to_path_buf(), secrets: Arc::new(secrets::MemoryStore::default()) };
    assert_eq!(core.batch_choice(), "auto");
    assert_eq!(core.model_setup().batch_engine, "auto");
    assert!(core.auto_retranscribe() && core.model_setup().auto_retranscribe, "on by default");
    assert_eq!(core.set_batch_engine("../x").unwrap_err().code, "invalid_data");
    core.set_batch_engine("qwen3-asr").unwrap();
    assert_eq!(core.batch_choice(), "qwen3-asr");
    assert_eq!(core.model_setup().batch_engine, "qwen3-asr");
    assert!(crate::meeting_watch::load_settings(tmp.path()).enabled, "the meeting detection default is untouched");
    // a value the app does not know (an older or edited file) is the default
    crate::meeting_watch::save_key(tmp.path(), BATCH_ENGINE_KEY, json!("other")).unwrap();
    assert_eq!(core.batch_choice(), "auto");
    core.set_batch_engine("whisper").unwrap();
    assert_eq!(core.batch_choice(), "whisper");
    core.set_batch_engine("auto").unwrap();
    assert_eq!(core.batch_choice(), "auto");
    core.set_auto_retranscribe(false).unwrap();
    assert!(!core.auto_retranscribe());
    assert_eq!(core.batch_choice(), "auto", "the other keys stay");
    crate::meeting_watch::save_key(tmp.path(), AUTO_RETRANSCRIBE_KEY, json!("yes")).unwrap();
    assert!(core.auto_retranscribe(), "a value that is not a boolean is the default");
    // the two Qwen3 files are two pieces of one model folder; neither is there
    for key in ["qwen3-asr", "qwen3-aligner"] {
        let o = model_setup::offer(key).unwrap();
        let p = model_piece(o, None, false, tmp.path(), false);
        assert_eq!((p.state, p.license), ("missing", "Apache-2.0"), "{key}");
        assert!(p.repo.starts_with("cstr/"));
    }
    // each piece is its own file: a wrong aligner file is "bad", the transcript model next to it only "missing"
    let o = model_setup::offer("qwen3-aligner").unwrap();
    std::fs::create_dir_all(o.dir(tmp.path())).unwrap();
    std::fs::write(o.path(tmp.path()), b"x").unwrap();
    assert_eq!(model_piece(model_setup::offer("qwen3-asr").unwrap(), None, false, tmp.path(), false).state, "missing");
    let p = model_piece(o, None, false, tmp.path(), false);
    assert_eq!(p.state, "bad");
    assert!(!p.detail.unwrap().contains(&tmp.path().display().to_string()), "no path");
}

/// A copy of action-items-py that declares network, two API keys and a setting, under `version`.
fn keyed_plugin(version: &str) -> tempfile::TempDir {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
    let d = tempfile::tempdir().unwrap();
    std::fs::copy(src.join("plugin.py"), d.path().join("plugin.py")).unwrap();
    let m = std::fs::read_to_string(src.join("meetko.toml"))
        .unwrap()
        .replace("network = false", "network = true\nsecrets = [\"TEST_API_KEY\", \"OTHER_KEY\"]\nsettings = [\"TEST_MODEL\"]")
        .replace("version = \"0.1.0\"", &format!("version = \"{version}\""));
    std::fs::write(d.path().join("meetko.toml"), m).unwrap();
    d
}

/// The webview sees names, 설정됨/없음 for keys and the value of settings only; a key goes in and never comes back.
#[test]
fn plugin_secrets_are_set_and_reported_without_values() {
    let (_t, mut core, _id) = setup();
    core.secrets = Arc::new(secrets::MemoryStore::default());
    let dir = keyed_plugin("0.1.0");
    let pv = core.inspect_plugin(dir.path()).unwrap();
    assert_eq!((pv.secrets.clone(), pv.settings.clone(), pv.keeps_values), (vec!["TEST_API_KEY".to_string(), "OTHER_KEY".into()], vec!["TEST_MODEL".to_string()], false));
    core.add_plugin(dir.path(), &pv.code_sha256, None).unwrap();
    assert_eq!(core.list_plugins().unwrap()[0].secrets, ["TEST_API_KEY", "OTHER_KEY"]);
    let st = |name: &str, kind, set, value: Option<&str>| SecretState { name: name.into(), kind, set, value: value.map(String::from) };
    use secrets::Kind::{Secret, Setting};
    assert_eq!(core.plugin_secrets(&pv.id).unwrap(), [st("TEST_API_KEY", Secret, false, None), st("OTHER_KEY", Secret, false, None), st("TEST_MODEL", Setting, false, None)]);
    let after = core.set_plugin_secret(&pv.id, "TEST_API_KEY", "sk-test-not-real-1").unwrap();
    assert_eq!(after[0], st("TEST_API_KEY", Secret, true, None));
    assert!(!serde_json::to_string(&after).unwrap().contains("sk-test"));
    let after = core.set_plugin_secret(&pv.id, "TEST_MODEL", "fast-1").unwrap();
    assert_eq!(after[2], st("TEST_MODEL", Setting, true, Some("fast-1")));
    assert_eq!(core.set_plugin_secret(&pv.id, "PATH", "sk-test-not-real-1").err().unwrap().code, "secret");
    assert_eq!(core.set_plugin_secret(&pv.id, "OTHER_KEY", "short").err().unwrap().code, "secret");
    assert_eq!(core.set_plugin_secret("meetko/nope", "TEST_API_KEY", "sk-test-not-real-1").err().unwrap().code, "not_found");
    assert!(!core.clear_plugin_secret(&pv.id, "TEST_API_KEY").unwrap()[0].set);
    // re-adding from a local folder (the same origin) says the saved values stay
    let newer = keyed_plugin("0.2.0");
    assert!(core.inspect_plugin(newer.path()).unwrap().keeps_values);
}

/// A key the keychain would not hand over (the macOS prompt was denied) reaches the webview as
/// `secret_access_denied`, which it words in plain Korean; the plugin is not run without it.
#[test]
fn a_denied_keychain_read_reaches_the_ui_as_its_own_code() {
    struct Denied(secrets::MemoryStore);
    impl SecretStore for Denied {
        fn get(&self, key: &secrets::SecretKey) -> meetko_core::Result<Option<String>> {
            match self.0.has(key)? {
                true => Err(meetko_core::Error::SecretAccessDenied("not allowed".into())),
                false => Ok(None),
            }
        }
        fn has(&self, key: &secrets::SecretKey) -> meetko_core::Result<bool> {
            self.0.has(key)
        }
        fn set(&self, key: &secrets::SecretKey, value: &str) -> meetko_core::Result<()> {
            self.0.set(key, value)
        }
        fn delete(&self, key: &secrets::SecretKey) -> meetko_core::Result<()> {
            self.0.delete(key)
        }
    }
    let (_t, mut core, id) = setup();
    core.secrets = Arc::new(Denied(secrets::MemoryStore::default()));
    let dir = keyed_plugin("0.1.0");
    let pv = core.inspect_plugin(dir.path()).unwrap();
    core.add_plugin(dir.path(), &pv.code_sha256, None).unwrap();
    core.set_plugin_secret(&pv.id, "TEST_API_KEY", "sk-test-not-real-1").unwrap();
    let d1 = core.transcribe(&id, &Fake(&["리뷰는", "제가", "하겠습니다"]), &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(core.run_plugin(&pv.id, &d1).err().unwrap().code, "secret_access_denied");
    assert!(core.store().unwrap().results_for_version(&d1).unwrap().is_empty(), "the plugin did not run");
}

/// The gallery row must list the same keys and settings as the manifest that is downloaded.
#[test]
fn gallery_refuses_a_download_whose_keys_differ_from_the_index() {
    let net = |m: String| if m.contains("[permissions]") { m.replace("network = false", "network = true") } else { m + "\n[permissions]\nnetwork = true\n" };
    let with_keys = move |m: String| net(m).replace("network = true", "network = true\nsecrets = [\"TEST_API_KEY\"]\nsettings = [\"TEST_MODEL\"]");
    let cases = [
        ("\"network\":true", "keys and settings not listed"),
        ("\"network\":true,\"secrets\":[\"TEST_API_KEY\"]", "settings not listed"),
        ("\"network\":true,\"settings\":[\"TEST_MODEL\"]", "keys not listed"),
    ];
    for (perm, why) in cases {
        let g = Gallery::start(with_keys, move |i| i.replace("\"network\":false", perm));
        let (_t, core, _id) = setup();
        let list = core.gallery_list(&g.index_url()).unwrap();
        let e = core.gallery_preview(&list[0], &g.endpoints()).err().unwrap_or_else(|| panic!("{why}: accepted"));
        assert_eq!(e.code, "index_mismatch", "{why}: {}", e.message);
    }
    // and when the index says the same, the preview carries the names
    let g = Gallery::start(with_keys, |i| i.replace("\"network\":false", "\"network\":true,\"secrets\":[\"TEST_API_KEY\"],\"settings\":[\"TEST_MODEL\"]"));
    let (_t, core, _id) = setup();
    let list = core.gallery_list(&g.index_url()).unwrap();
    assert_eq!(list[0].permissions.secrets, ["TEST_API_KEY"]);
    let (_f, pv) = core.gallery_preview(&list[0], &g.endpoints()).unwrap();
    assert_eq!((pv.secrets, pv.settings), (vec!["TEST_API_KEY".to_string()], vec!["TEST_MODEL".to_string()]));
}

// ---------------------------------------------------------------- the automatic 다시 받아쓰기

#[test]
fn the_most_accurate_ready_engine_is_picked() {
    let r = |whisper, qwen| Ready { whisper, qwen };
    let p = |engine, fallback| Some(Pick { engine, fallback });
    // auto: Qwen3-ASR first, then Whisper, then nothing
    assert_eq!(pick_engine("auto", r(true, true)), p("qwen3-asr", false));
    assert_eq!(pick_engine("auto", r(true, false)), p("whisper", false));
    assert_eq!(pick_engine("auto", r(false, true)), p("qwen3-asr", false));
    assert_eq!(pick_engine("auto", r(false, false)), None);
    // an explicit choice is respected; when it is not ready the other one runs and says so
    assert_eq!(pick_engine("whisper", r(true, true)), p("whisper", false));
    assert_eq!(pick_engine("whisper", r(false, true)), p("qwen3-asr", true));
    assert_eq!(pick_engine("qwen3-asr", r(true, true)), p("qwen3-asr", false));
    assert_eq!(pick_engine("qwen3-asr", r(true, false)), p("whisper", true));
    assert_eq!(pick_engine("qwen3-asr", r(false, false)), None);
}

type Hook = Arc<dyn Fn(&AtomicBool) + Send + Sync>;

/// `Fake` that runs `during` first (the user approving or cancelling while it works).
struct Hooked {
    words: &'static [&'static str],
    during: Option<Hook>,
}

impl BatchEngine for Hooked {
    fn transcribe(&self, req: &BatchRequest, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> meetko_core::Result<Value> {
        if let Some(f) = &self.during {
            f(cancel);
        }
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(meetko_core::Error::Cancelled);
        }
        let mut r = Fake(self.words).transcribe(req, progress, cancel)?;
        r["engine"]["id"] = json!("fake-batch");
        Ok(r)
    }
}

struct FakeEngines {
    ready: Ready,
    opened: std::sync::Mutex<Vec<&'static str>>,
    during: Option<Hook>,
}

impl FakeEngines {
    fn new(whisper: bool, qwen: bool) -> Self {
        FakeEngines { ready: Ready { whisper, qwen }, opened: Default::default(), during: None }
    }
    fn opened(&self) -> Vec<&'static str> {
        self.opened.lock().unwrap().clone()
    }
}

impl Engines for FakeEngines {
    fn ready(&self) -> Ready {
        self.ready
    }
    fn open(&self, engine: &'static str) -> ApiResult<Box<dyn BatchEngine>> {
        self.opened.lock().unwrap().push(engine);
        Ok(Box::new(Hooked { words: &["더", "정확한", "초안"], during: self.during.clone() }))
    }
}

/// Separates every channel it is given into two speakers, and remembers which.
#[derive(Default)]
struct CountingDiar(std::sync::Mutex<Vec<u32>>);

impl DiarEngine for CountingDiar {
    fn diarize(&self, req: &meetko_core::diarize::DiarRequest, _cancel: &AtomicBool) -> meetko_core::Result<Value> {
        self.0.lock().unwrap().push(req.channel);
        let t = |s: i64, e: i64, sp: &str| json!({"start_ms": s, "end_ms": e, "speaker": sp});
        Ok(json!({"contract": "0.1", "kind": "diarization", "engine": {"id": "fake-diar", "version": "1"}, "network": false,
                  "audio_sha256": req.audio_sha256, "turns": [t(0, 400, "a"), t(500, 1500, "b")]}))
    }
    fn min_speaker_ms(&self) -> i64 {
        0
    }
}

/// A recorded meeting with its live version, where the live session separated channel 1's speakers.
fn recorded() -> (tempfile::TempDir, Core, String, String) {
    let (t, core, id) = setup();
    let store = core.store().unwrap();
    let (meeting, recording, files) = store.meeting_and_recording(&id).unwrap();
    let w = |s: i64, text: &str| meetko_core::contract::EngineWord { start_ms: s, end_ms: s + 300, text: text.into(), p: None, end_estimated: true };
    let asr = |c: u32| json!({"role": "asr", "engine": {"id": "fake/stream", "version": "1"}, "channels": [c], "network": false, "word_timing": "transducer", "granularity": "token"});
    let cov = vec![meetko_core::contract::Span { start_ms: 0, end_ms: 2000 }];
    let mut inputs = std::collections::BTreeMap::new();
    for f in &files {
        let mut c = meetko_core::contract::ChannelInput::new(&f.role, vec![w(100, "실시간"), w(900, "자막")], cov.clone(), vec![asr(f.index)]);
        c.label = f.label.clone();
        if f.index == 1 {
            let t = |s: i64, e: i64, sp: &str| json!({"start_ms": s, "end_ms": e, "speaker": sp});
            let r = json!({"contract": "0.1", "kind": "diarization", "engine": {"id": "live-diar", "version": "1"}, "network": false,
                           "audio_sha256": f.sha256, "params": {"mode": "streaming", "live": true}, "turns": [t(0, 600, "x"), t(800, 1800, "y")]});
            pipeline::with_diarization(&mut c, &r, &f.sha256, recording.duration_ms, 1, 0).unwrap();
        }
        inputs.insert(f.index, c);
    }
    let md = meetko_core::contract::compose(meeting, recording, ("pending".into(), VersionKind::Live), &inputs);
    let live = store.create_version(md).unwrap().version.id;
    (t, core, id, live)
}

/// Runs a job, returning what it emitted and its last event.
fn run(core: &Core, kind: DraftJob, id: &str, engines: &FakeEngines, diar: Option<&dyn DiarEngine>, cancel: &AtomicBool) -> (Vec<JobEvent>, JobEvent) {
    let seen = std::sync::Mutex::new(Vec::new());
    let last = core.run_draft_job(kind, id, engines, &DiarSetup { engine: diar, ..Default::default() }, &|e| seen.lock().unwrap().push(e), cancel);
    (seen.into_inner().unwrap(), last)
}

fn head(core: &Core, id: &str) -> String {
    core.store().unwrap().head_version(id).unwrap().unwrap()
}

#[test]
fn a_stopped_recording_gets_its_live_draft_then_a_batch_draft_in_one_job() {
    let (_t, core, id, live) = recorded();
    let engines = FakeEngines::new(true, true);
    let diar = CountingDiar::default();
    let (seen, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, Some(&diar), &AtomicBool::new(false));
    // the live draft is announced on the way, the batch draft at the end
    let live_done = seen.iter().find(|e| e.state == "done").expect("live draft done");
    assert!(live_done.from_live && !live_done.auto && live_done.engine.is_none());
    let live_draft = live_done.version_id.clone().unwrap();
    assert!(seen.iter().filter(|e| !e.from_live).all(|e| e.state == "progress" && e.auto && e.engine == Some("qwen3-asr") && !e.fallback));
    assert!(seen.iter().any(|e| e.auto && e.channels == 0), "the batch pass says it is preparing");
    assert_eq!((last.state, last.auto, last.from_live, last.engine), ("done", true, false, Some("qwen3-asr")));
    assert_eq!(engines.opened(), ["qwen3-asr"], "Qwen3-ASR is the most accurate ready engine");
    let draft = last.version_id.unwrap();
    assert_ne!(draft, live_draft);
    assert_eq!(head(&core, &id), draft, "nothing approved: the batch draft is the head");
    // speakers: channel 1 from the live session; channel 0 (no live speakers) separated for each draft as before
    assert_eq!(*diar.0.lock().unwrap(), [0, 0], "channel 1 is never separated again");
    let v = core.get_version(&draft).unwrap();
    let producers = v["version"]["producers"].as_array().unwrap();
    let ch1 = producers.iter().find(|p| p["role"] == "diarization" && p["channels"] == json!([1])).unwrap();
    assert_eq!((ch1["engine"]["id"].as_str(), ch1["params"]["live"].as_bool(), ch1["params"]["reused_from"].as_str()), (Some("live-diar"), Some(true), Some(live.as_str())));
    let ch0 = producers.iter().find(|p| p["role"] == "diarization" && p["channels"] == json!([0])).unwrap();
    assert_eq!((ch0["engine"]["id"].as_str(), ch0["params"].get("reused_from")), (Some("fake-diar"), None));
    assert!(producers.iter().filter(|p| p["role"] == "asr").all(|p| p["engine"]["id"] == "fake-batch"));
}

#[test]
fn an_explicit_choice_runs_and_falls_back_when_it_is_not_ready() {
    let (_t, core, id, _live) = recorded();
    core.set_batch_engine("whisper").unwrap();
    let engines = FakeEngines::new(true, true);
    let (_, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(false));
    assert_eq!((last.engine, last.fallback), (Some("whisper"), false));
    assert_eq!(engines.opened(), ["whisper"]);

    let (_t, core, id, _live) = recorded();
    core.set_batch_engine("whisper").unwrap();
    let engines = FakeEngines::new(false, true);
    let (seen, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(false));
    assert_eq!((last.state, last.engine, last.fallback), ("done", Some("qwen3-asr"), true));
    assert!(seen.iter().filter(|e| e.auto).all(|e| e.fallback), "every event of the pass says so");
    // the same rule for a 다시 받아쓰기 the user starts; that one separates every channel again
    let diar = CountingDiar::default();
    let (_, last) = run(&core, DraftJob::Batch, &id, &engines, Some(&diar), &AtomicBool::new(false));
    assert_eq!((last.state, last.auto, last.engine, last.fallback), ("done", false, Some("qwen3-asr"), true));
    assert_eq!(*diar.0.lock().unwrap(), [0, 1]);
}

#[test]
fn no_batch_pass_when_turned_off_or_nothing_is_ready() {
    for (off, ready) in [(true, true), (false, false)] {
        let (_t, core, id, _live) = recorded();
        if off {
            core.set_auto_retranscribe(false).unwrap();
        }
        let engines = FakeEngines::new(ready, ready);
        let (seen, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(false));
        assert_eq!((last.state, last.from_live, last.auto), ("done", true, false), "the live draft is the last word");
        assert!(seen.iter().all(|e| e.from_live && e.state == "progress"));
        assert!(engines.opened().is_empty());
        assert_eq!(core.get_meeting(&id).unwrap().versions.len(), 2, "live + its draft");
        assert_eq!(head(&core, &id), last.version_id.unwrap());
    }
    // a plain 초안 만들기 never chains
    let (_t, core, id, _live) = recorded();
    let engines = FakeEngines::new(true, true);
    let (_, last) = run(&core, DraftJob::FromLive, &id, &engines, None, &AtomicBool::new(false));
    assert!(last.from_live && engines.opened().is_empty());
}

#[test]
fn cancelling_the_batch_pass_keeps_the_live_draft() {
    let (_t, core, id, _live) = recorded();
    let mut engines = FakeEngines::new(true, true);
    engines.during = Some(Arc::new(|cancel: &AtomicBool| cancel.store(true, std::sync::atomic::Ordering::SeqCst)));
    let (seen, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(false));
    assert_eq!((last.state, last.auto), ("cancelled", true));
    let live_draft = seen.iter().find(|e| e.state == "done").unwrap().version_id.clone().unwrap();
    assert_eq!(core.get_meeting(&id).unwrap().versions.len(), 2, "nothing more is stored");
    assert_eq!(head(&core, &id), live_draft);
    // a cancel that came while the live draft was made stops the chain there
    let (_t, core, id, _live) = recorded();
    let engines = FakeEngines::new(true, true);
    let (_, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(true));
    assert!(last.from_live && engines.opened().is_empty(), "{}", last.state);
}

#[test]
fn a_live_draft_approved_meanwhile_stays_the_head() {
    let (_t, core, id, _live) = recorded();
    let dir = core.data_dir.clone();
    let mid = id.clone();
    let mut engines = FakeEngines::new(true, true);
    // the user approves the live draft while the batch pass runs (once: the engine sees two channels)
    engines.during = Some(Arc::new(move |_: &AtomicBool| {
        let c = Core { data_dir: dir.clone(), secrets: Arc::new(secrets::MemoryStore::default()) };
        let versions = c.get_meeting(&mid).unwrap().versions;
        if !versions.iter().any(|v| v.kind == "approved") {
            c.approve(&versions.iter().find(|v| v.kind == "draft").unwrap().id).unwrap();
        }
    }));
    let (_, last) = run(&core, DraftJob::FromLiveThenBatch, &id, &engines, None, &AtomicBool::new(false));
    assert_eq!((last.state, last.auto), ("done", true));
    let draft = last.version_id.unwrap();
    let h = head(&core, &id);
    assert_ne!(h, draft, "the new draft waits for approval");
    assert_eq!(core.get_version(&h).unwrap()["version"]["kind"], "approved");
    assert_eq!(core.get_version(&draft).unwrap()["version"]["kind"], "draft");
    assert_eq!(core.approve(&draft).unwrap(), head(&core, &id), "approving it makes it the head");
}

#[test]
fn a_failed_batch_pass_is_its_own_error_and_keeps_the_live_draft() {
    struct Broken;
    impl Engines for Broken {
        fn ready(&self) -> Ready {
            Ready { whisper: false, qwen: true }
        }
        fn open(&self, _: &'static str) -> ApiResult<Box<dyn BatchEngine>> {
            Err(ApiError::new("engine_sandbox", "sandbox-exec cannot apply the engine profile"))
        }
    }
    let (_t, core, id, _live) = recorded();
    let last = core.run_draft_job(DraftJob::FromLiveThenBatch, &id, &Broken, &DiarSetup::default(), &|_| {}, &AtomicBool::new(false));
    assert_eq!((last.state, last.auto, last.code), ("error", true, Some("engine_sandbox")));
    assert_eq!(core.get_meeting(&id).unwrap().versions.len(), 2);
}

#[test]
fn the_vocabulary_changes_what_is_shown_and_exported_never_the_stored_text() {
    let (_t, core, id) = setup();
    let draft = core.transcribe(&id, &Fake(&["API", "서버는", "제가", "볼게요"]), &no_progress, &AtomicBool::new(false)).unwrap();
    assert_eq!(core.vocabulary().unwrap(), Vocabulary::default(), "nothing stored yet");
    assert_eq!(core.display_words(&draft).unwrap(), None);
    let rep = |f: &str, r: &str| Replacement { find: f.into(), replace: r.into() };
    let kept = core.set_vocabulary(Vocabulary { hints: vec![" 미트코 ".into()], replacements: vec![rep("서버", "server"), rep("제가", "")] }).unwrap();
    assert_eq!(kept.hints, ["미트코"]);
    assert_eq!(core.vocabulary().unwrap(), kept);
    assert!(core.vocabulary_json().unwrap().starts_with("{\n  \"version\": 1,"));

    let plain = core.export_version(&draft, "plain", None).unwrap().text;
    // (both channels say the same words at the same times: every word is its own turn here)
    assert!(plain.contains("server는\n") && !plain.contains("제가") && !plain.contains("서버") && !plain.contains("\n\n"), "{plain}");
    let shown = core.display_words(&draft).unwrap().expect("changed");
    assert_eq!(shown.iter().filter(|w| *w == "server는").count(), 2);
    assert_eq!(shown.iter().filter(|w| w.is_empty()).count(), 2, "제가, removed on both channels");
    // the stored version is the original
    assert!(core.get_version(&draft).unwrap().to_string().contains("서버는"));
    // the search reads the stored text and shows the replaced one
    let hit = &core.search("서버").unwrap().meetings[0].hits[0];
    assert_eq!(hit.text, "server");

    assert_eq!(core.vocabulary_preview(vec![rep("쿠버네티스", "Kubernetes"), rep("", "x")], "쿠버네티스로 배포해요").unwrap(), "Kubernetes로 배포해요");
    assert_eq!(core.vocabulary_preview(vec![], &"가".repeat(PREVIEW_MAX_CHARS + 1)).err().unwrap().code, "invalid_data");
    let e = core.set_vocabulary(Vocabulary { hints: vec!["a\tb".into()], replacements: vec![] }).err().unwrap();
    assert_eq!((e.code, e.message.as_str()), ("invalid_data", "1번째 용어에 쓸 수 없는 문자(탭 같은 제어 문자)가 있어요."));
    // a list that would blow the text up is not applied (the preview shows the sample as it is)
    let boom = vec![rep("a", &"a".repeat(200)); 5];
    assert_eq!(core.vocabulary_preview(boom, &"a".repeat(1000)).unwrap(), "a".repeat(1000));

    // an imported file is checked and not stored
    let f = core.data_dir.join("import.json");
    std::fs::write(&f, r#"{"version":1,"hints":["k8s"],"replacements":[]}"#).unwrap();
    assert_eq!(core.vocabulary_read_file(&f).unwrap().hints, ["k8s"]);
    assert_eq!(core.vocabulary().unwrap(), kept);
    std::fs::write(&f, r#"{"version":9}"#).unwrap();
    let e = core.vocabulary_read_file(&f).err().unwrap();
    assert_eq!((e.code, e.message.as_str()), ("invalid_data", "버전 9 용어 사전은 읽을 수 없어요. 이 앱은 버전 1을 읽어요."));

    // a damaged file is reported in the settings; the transcript and exports show the original
    std::fs::write(core.data_dir.join(vocabulary::FILE), "{").unwrap();
    assert!(core.vocabulary().is_err());
    assert!(core.export_version(&draft, "plain", None).unwrap().text.contains("서버는\n"));
    assert_eq!(core.display_words(&draft).unwrap(), None);
}

#[test]
fn names_edits_and_vocabulary_compose_the_same_in_exports_search_and_the_transcript() {
    let (_t, core, id) = setup();
    let draft = core.transcribe(&id, &Fake(&["리뷰는", "서버", "제가", "볼게요"]), &no_progress, &AtomicBool::new(false)).unwrap();
    let md = core.store().unwrap().version(&draft).unwrap();
    let other = md.speakers.iter().find(|s| s.label.as_deref() != Some(MIC_LABEL)).unwrap().id.clone();
    // the edited turn has a word the vocabulary would change: the edit must win, as typed
    let edited = md.turns.iter().find(|t| md.words.iter().any(|w| w.id == t.first_word && w.text == "서버")).unwrap().id.clone();
    core.change_overlay(&draft, OverlayChange::Rename { speaker_id: other.clone(), name: "민수".into() }).unwrap();
    let ov = core.change_overlay(&draft, OverlayChange::Edit { turn_id: edited.clone(), text: "서버 제가 고친 문장".into() }).unwrap().overlay;
    let rep = |f: &str, r: &str| Replacement { find: f.into(), replace: r.into() };
    core.set_vocabulary(Vocabulary { hints: vec![], replacements: vec![rep("서버", "server"), rep("제가", "")] }).unwrap();

    // what the transcript view shows (app/src/model.ts `turnText`, `speakerLabel`): the edit, else the
    // shown words (`vocabulary_display`) that are not empty; the name from the overlay
    let shown = core.display_words(&draft).unwrap().expect("the replacements change this version");
    let pos: std::collections::HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    let lines: Vec<(String, String)> = md
        .turns
        .iter()
        .map(|t| {
            let who = ov.speaker_name(&md, t.speaker.as_deref().unwrap()).to_string();
            let text = match ov.edits.get(&t.id) {
                Some(e) => e.text.clone(),
                None => shown[pos[t.first_word.as_str()]..=pos[t.last_word.as_str()]].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" "),
            };
            (who, text)
        })
        .filter(|(_, text)| !text.is_empty())
        .collect();
    assert!(lines.iter().any(|(_, t)| t == "서버 제가 고친 문장"), "{lines:?}");
    assert!(lines.iter().any(|(_, t)| t == "server"), "the replacement elsewhere: {lines:?}");
    assert!(lines.iter().any(|(w, _)| w == "민수"));

    // every export agrees with that, line for line
    let speakers = core.export_version(&draft, "speakers", None).unwrap().text;
    let want: Vec<String> = lines.iter().map(|(w, t)| format!("{w}: {t}")).collect();
    assert_eq!(speakers.lines().collect::<Vec<_>>(), want, "{speakers}");
    let mdown = core.export_version(&draft, "md", None).unwrap().text;
    assert!(mdown.contains("**민수**: server\n") && mdown.contains(": 서버 제가 고친 문장\n"), "{mdown}");
    assert!(!mdown.contains("**상대") && !mdown.contains(": 제가") && !mdown.contains("리뷰는 제가"), "{mdown}");
    let srt = core.export_version(&draft, "srt", None).unwrap().text;
    let cues: Vec<&str> = srt.split("\n\n").filter(|c| !c.is_empty()).map(|c| c.lines().nth(2).unwrap()).collect();
    assert_eq!(cues, want.iter().map(String::as_str).collect::<Vec<_>>(), "{srt}");
    for f in ["txt", "vtt"] {
        let t = core.export_version(&draft, f, None).unwrap().text;
        assert!(t.contains("서버 제가 고친 문장") && t.contains("server") && t.contains("민수"), "{f}: {t}");
    }

    // search: the edited turn's snippet is the edit as typed, the others are replaced
    let hits: Vec<_> = core.search("서버").unwrap().meetings.into_iter().flat_map(|m| m.hits).collect();
    assert!(hits.iter().any(|h| h.edited && h.text == "서버" && h.after.starts_with(" 제가 고친")), "{hits:?}");
    assert!(hits.iter().any(|h| !h.edited && h.text == "server"), "{hits:?}");
    assert!(hits.iter().all(|h| h.edited || !h.before.contains("제가")), "{hits:?}");
    // the stored version is the original
    assert!(core.get_version(&draft).unwrap().to_string().contains("\"서버\""));
}
