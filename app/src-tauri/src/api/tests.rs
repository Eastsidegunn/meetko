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
    let core = Core { data_dir: data.clone() };
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
    let core = Core { data_dir: tmp.path().join("data") };
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
    let core = Core { data_dir: tmp.path().join("data") };
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
    let core = Core { data_dir: tmp.path().join("data") };
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
    let core = Core { data_dir: tmp.path().to_path_buf() };
    assert!(!core.model_setup().card_dismissed);
    core.dismiss_setup_card().unwrap();
    assert!(core.model_setup().card_dismissed);
    assert!(crate::meeting_watch::load_settings(tmp.path()).enabled, "the meeting detection default is untouched");
    let d = model_setup::offer("diarization").unwrap();
    assert_eq!(model_piece(d, None, false, tmp.path(), false).state, "missing");
}
