//! Mirrors tests/test_contract.py, plus the golden rebuild of the fixtures
//! (tools/build_fixtures.py) that proves the Rust port matches the reference.

use std::collections::BTreeMap;
use std::path::PathBuf;

use meetko_core::contract::*;
use meetko_core::pipeline::{batch_channel, producer};
use proptest::prelude::*;
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fix(name: &str) -> Value {
    let p = root().join("contract/fixtures/meeting01").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

fn md_fixture(version: &str) -> MeetingData {
    serde_json::from_value(fix(&format!("meeting-data.{version}.json"))).unwrap()
}

// ------------------------------------------------------------ golden

// metadata from tools/build_fixtures.py (duration comes from the raw reference, which is local)
const DUR: i64 = 69364;
const RECORDING_SHA: &str = "4f4e1620bf9147b330f8756106ea33afaf8ef9f968587de41a59eed0dde2d191";

fn meeting_and_recording() -> (Meeting, Recording) {
    (
        Meeting {
            id: "meeting01".into(),
            title: Some("fixture: incident retro".into()),
            started_at: None,
            duration_ms: DUR,
            language_hint: Some("ko".into()),
            channels: vec![
                Channel { index: 0, role: "mic".into(), label: Some("나".into()) },
                Channel { index: 1, role: "loopback".into(), label: Some("상대".into()) },
            ],
        },
        Recording { sha256: RECORDING_SHA.into(), sample_rate: 16000, duration_ms: DUR },
    )
}

fn diarization(v: &Value) -> Diarization {
    serde_json::from_value(v.clone()).unwrap()
}

fn rebuild_draft() -> MeetingData {
    let (mic, loopback) = (fix("engine.whisper-cpp.mic.json"), fix("engine.whisper-cpp.loopback.json"));
    let (vad_mic, diar) = (fix("engine.sherpa-vad.mic.json"), fix("engine.sherpa-diar.loopback.json"));
    let mut channels = BTreeMap::new();
    for (idx, role, label, asr, d, single) in
        [(0u32, "mic", "나", &mic, &vad_mic, true), (1, "loopback", "상대", &loopback, &diar, false)]
    {
        let mut c = batch_channel(asr, idx, role, Some(label), None).unwrap();
        c.producers.push(producer(d, "diarization", idx));
        c.diarization = Some(diarization(d));
        c.single_speaker = single;
        channels.insert(idx, c);
    }
    let (m, r) = meeting_and_recording();
    compose(m, r, ("meeting01.draft.1".into(), VersionKind::Draft), &channels)
}

fn rebuild_live() -> MeetingData {
    let sherpa_engine = json!({ "id": "sherpa-onnx.online", "version": "2.0.1" });
    let mut channels = BTreeMap::new();
    for (idx, role, label) in [(0u32, "mic", "나"), (1, "loopback", "상대")] {
        let events: Vec<StreamEvent> = serde_json::from_value(fix(&format!("engine.sherpa-stream.{role}.json"))).unwrap();
        let vad = fix(&format!("engine.sherpa-vad.{role}.json"));
        let asr = json!({
            "role": "asr", "engine": sherpa_engine,
            "model": { "id": "sherpa-onnx-streaming-zipformer-korean-2024-06-16" },
            "channels": [idx], "word_timing": "transducer", "granularity": "token",
            "network": false, "params": { "chunk_ms": 100, "endpoint_rule2_s": 0.8 }
        });
        let mut c = ChannelInput::new(
            role,
            live_words(&events),
            vec![Span { start_ms: 0, end_ms: DUR }],
            vec![asr, producer(&vad, "diarization", idx)],
        );
        c.label = Some(label.into());
        c.diarization = Some(diarization(&vad));
        c.single_speaker = true;
        channels.insert(idx, c);
    }
    let (m, r) = meeting_and_recording();
    compose(m, r, ("meeting01.live.1".into(), VersionKind::Live), &channels)
}

#[test]
fn golden_draft_equals_committed_fixture() {
    assert_eq!(rebuild_draft().to_value(), fix("meeting-data.draft.json"));
}

#[test]
fn golden_live_equals_committed_fixture() {
    assert_eq!(rebuild_live().to_value(), fix("meeting-data.live.json"));
}

// ------------------------------------------------------------ schemas and invariants on the fixtures

#[test]
fn fixtures_are_valid() {
    for v in ["draft", "live"] {
        let doc = fix(&format!("meeting-data.{v}.json"));
        assert_eq!(schema_errors(meeting_data_validator(), &doc), Vec::<String>::new());
        let md: MeetingData = serde_json::from_value(doc).unwrap();
        assert_eq!(invariant_errors(&md), Vec::<String>::new());
    }
}

#[test]
fn engine_results_are_valid() {
    let dir = root().join("contract/fixtures/meeting01");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("engine.") {
            continue;
        }
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let docs = doc.as_array().cloned().unwrap_or_else(|| vec![doc]);
        for d in &docs {
            assert_eq!(schema_errors(engine_result_validator(), d), Vec::<String>::new(), "{name}");
        }
        n += 1;
    }
    assert_eq!(n, 7);
}

#[test]
fn invariants_catch_broken_data() {
    let md = md_fixture("draft");
    let mut bad = md.clone();
    bad.words[3].start_ms = bad.words[4].start_ms + 1;
    bad.words[4].start_ms = 0;
    assert!(invariant_errors(&bad).iter().any(|e| e.starts_with("I2")));
    let mut bad = md.clone();
    bad.words[0].speaker = Some("nobody".into());
    assert!(invariant_errors(&bad).iter().any(|e| e.starts_with("I4")));
    let mut bad = md.clone();
    bad.turns.pop();
    assert!(invariant_errors(&bad).iter().any(|e| e.contains("cover every word")));
    // the turn-gap rule: a turn may not span a pause of 2000 ms or more
    let mut bad = md.clone();
    let last = bad.turns[0].last_word.clone();
    let pos = bad.words.iter().position(|w| w.id == last).unwrap();
    bad.words[pos].start_ms = bad.words[pos - 1].end_ms + TURN_GAP_MS;
    bad.words[pos].end_ms = bad.words[pos].start_ms;
    assert!(invariant_errors(&bad).iter().any(|e| e.contains("pause")), "{:?}", invariant_errors(&bad));
}

// ------------------------------------------------------------ the core rejects bad plugin output

fn good_output(md: &MeetingData) -> Value {
    let w = &md.words;
    json!({"contract": "0.1", "plugin": {"id": "t", "version": "0"}, "input": {"version_id": md.version.id},
           "items": [{"type": "action_items", "items": [{"text": "x", "owner": md.speakers[0].id,
                      "evidence": [{"from": w[0].id, "to": w[2].id}]}]}]})
}

#[test]
fn core_rejects_bad_output() {
    type Mutation = Box<dyn Fn(&mut Value, &MeetingData)>;
    let cases: Vec<(&str, Mutation, &str)> = vec![
        ("extra top-level field", Box::new(|o, _| o["extra"] = json!(1)), "Additional properties"),
        ("extra item field", Box::new(|o, _| o["items"][0]["items"][0]["color"] = json!("red")), "Additional properties"),
        ("other version", Box::new(|o, _| o["input"]["version_id"] = json!("other")), "O1"),
        ("unknown word", Box::new(|o, _| o["items"][0]["items"][0]["evidence"][0]["from"] = json!("w99999")), "O2"),
        (
            "from after to",
            Box::new(|o, md| o["items"][0]["items"][0]["evidence"][0]["from"] = json!(md.words[5].id)),
            "from after to",
        ),
        ("unknown owner", Box::new(|o, _| o["items"][0]["items"][0]["owner"] = json!("s99")), "O3"),
        ("empty evidence", Box::new(|o, _| o["items"][0]["items"][0]["evidence"] = json!([])), "less than 1 item"),
        ("html item", Box::new(|o, _| o["items"].as_array_mut().unwrap().push(json!({"type": "html", "html": "<script>"}))), "is not one of"),
    ];
    let md = md_fixture("draft");
    for (name, mutate, rule) in cases {
        let mut out = good_output(&md);
        assert_eq!(output_errors(&md, &out), Vec::<String>::new());
        mutate(&mut out, &md);
        let errs = output_errors(&md, &out);
        assert!(errs.iter().any(|e| e.contains(rule)), "{name}: wanted {rule:?} in {errs:?}");
    }
}

#[test]
fn table_row_width_is_o4() {
    let md = md_fixture("draft");
    let out = json!({"contract": "0.1", "plugin": {"id": "t", "version": "0"}, "input": {"version_id": md.version.id},
        "items": [{"type": "table", "columns": ["a"],
                   "rows": [{"cells": ["1", "2"], "evidence": [{"from": md.words[0].id, "to": md.words[0].id}]}]}]});
    let errs = output_errors(&md, &out);
    assert!(errs.iter().any(|e| e.contains("O4")), "{errs:?}");
}

// ------------------------------------------------------------ invariants hold for any engine output

fn channel_strategy(diarized: bool) -> impl Strategy<Value = ChannelInput> {
    // exactly the class the schemas exclude (explicit, since \s differs between regex engines)
    let word = (0i64..=60_000, 0i64..=3_000, "[^\\x{0}-\\x{20}\\x{7f}-\\x{a0}\\x{1680}\\x{180e}\\x{2000}-\\x{200b}\\x{2028}\\x{2029}\\x{202f}\\x{205f}\\x{3000}\\x{feff}]{1,8}");
    let words = proptest::collection::vec(word, 0..40);
    let turns = proptest::collection::vec((0i64..=60_000, 1i64..=5_000, prop::sample::select(vec!["A", "B", "C"])), 0..10);
    (words, turns).prop_map(move |(ws, ts)| {
        let words = ws
            .into_iter()
            .map(|(s, d, t)| EngineWord { start_ms: s, end_ms: s + d, text: t.trim().to_string(), p: None, end_estimated: false })
            .collect();
        let asr = json!({"role": "asr", "engine": {"id": "x", "version": "0"}, "network": false});
        let mut c = ChannelInput::new("loopback", words, vec![Span { start_ms: 0, end_ms: 63_000 }], vec![asr]);
        if diarized {
            c.diarization = Some(Diarization {
                turns: ts.into_iter().map(|(s, d, k)| DiarTurn { start_ms: s, end_ms: s + d, speaker: k.into() }).collect(),
            });
        }
        c
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn compose_always_satisfies_invariants(mut mic in channel_strategy(false), loopback in channel_strategy(true)) {
        mic.role = "mic".into();
        let meeting = Meeting {
            id: "m".into(), title: None, started_at: None, duration_ms: 63_000, language_hint: None,
            channels: vec![Channel { index: 0, role: "mic".into(), label: None },
                           Channel { index: 1, role: "loopback".into(), label: None }],
        };
        let rec = Recording { sha256: "0".repeat(64), sample_rate: 16000, duration_ms: 63_000 };
        let channels = BTreeMap::from([(0, mic), (1, loopback)]);
        let md = compose(meeting, rec, ("v1".into(), VersionKind::Draft), &channels);
        prop_assert_eq!(schema_errors(meeting_data_validator(), &md.to_value()), Vec::<String>::new());
        prop_assert_eq!(invariant_errors(&md), Vec::<String>::new());
        for w in &md.words {
            let r = resolve(&md, &w.id, &w.id).unwrap();
            prop_assert_eq!((r.1, r.2), (w.start_ms, w.end_ms));
        }
    }
}

#[test]
fn export_formats_match_reference_shape() {
    let md = md_fixture("draft");
    let vtt = meetko_core::export::webvtt(&md).unwrap();
    assert!(vtt.starts_with("WEBVTT\n\nt0000\n00:00:00.700 --> 00:00:06.740\n<v 상대 1>지난주 인시던트"));
    assert!(vtt.ends_with('\n'));
    let ctm = meetko_core::export::ctm(&md, "meeting01", 1);
    assert!(ctm.starts_with("meeting01 2 0.700 0.480 지난주 0.9897\n"));
    let rttm = meetko_core::export::rttm(&md, "meeting01", 1);
    assert!(rttm.starts_with("SPEAKER meeting01 1 0.470 6.531 <NA> <NA> s1 <NA> <NA>\n"));
}
