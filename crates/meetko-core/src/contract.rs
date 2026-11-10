//! Contract 0.1 in Rust: types, schema validation, compose, invariants, plugin-output
//! checks. A port of `reference/core.py`; the golden test proves they agree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use jsonschema::Validator;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CONTRACT: &str = "0.1";
/// a pause this long ends a turn even when the speaker continues
pub const TURN_GAP_MS: i64 = 2000;

const MEETING_DATA_SCHEMA: &str = include_str!("../../../contract/schemas/meeting-data.schema.json");
const PLUGIN_OUTPUT_SCHEMA: &str = include_str!("../../../contract/schemas/plugin-output.schema.json");
const ENGINE_RESULT_SCHEMA: &str = include_str!("../../../contract/schemas/engine-result.schema.json");

// ---------------------------------------------------------------- schemas

fn build(src: &str) -> Validator {
    let schema: Value = serde_json::from_str(src).expect("embedded schema is JSON");
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .expect("embedded schema compiles")
}

macro_rules! validator {
    ($name:ident, $src:expr) => {
        pub fn $name() -> &'static Validator {
            static V: OnceLock<Validator> = OnceLock::new();
            V.get_or_init(|| build($src))
        }
    };
}
validator!(meeting_data_validator, MEETING_DATA_SCHEMA);
validator!(plugin_output_validator, PLUGIN_OUTPUT_SCHEMA);
validator!(engine_result_validator, ENGINE_RESULT_SCHEMA);

/// `path: message` per violation, like the reference's `schema_errors`.
pub fn schema_errors(v: &Validator, doc: &Value) -> Vec<String> {
    v.iter_errors(doc)
        .map(|e| {
            let path = e.instance_path().to_string();
            let path = path.trim_start_matches('/');
            format!("{}: {}", if path.is_empty() { "<root>" } else { path }, e)
        })
        .collect()
}

// ---------------------------------------------------------------- meeting-data types
// Typed for the core's own use. Readers elsewhere (plugins) stay tolerant;
// the core only reads what it wrote, so unknown fields are not carried.

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub index: u32,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_hint: Option<String>,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub sha256: String,
    pub sample_rate: u32,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionKind {
    Live,
    Draft,
    Approved,
}

impl VersionKind {
    /// Unknown kinds are an error, not a default.
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        match s {
            "live" => Ok(VersionKind::Live),
            "draft" => Ok(VersionKind::Draft),
            "approved" => Ok(VersionKind::Approved),
            other => Err(crate::error::Error::Corrupt(format!("unknown version kind {other:?}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            VersionKind::Live => "live",
            VersionKind::Draft => "draft",
            VersionKind::Approved => "approved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub id: String,
    pub kind: VersionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// free-form by design: engines and edit tools add their own keys
    pub producers: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub channel: u32,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Basis {
    Channel,
    Diarization,
    User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: String,
    pub channel: u32,
    pub basis: Basis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerSpan {
    pub speaker: String,
    pub channel: u32,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeakerStatus {
    Assigned,
    Ambiguous,
    Unassigned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub id: String,
    pub channel: u32,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub speaker: Option<String>,
    pub speaker_status: SpeakerStatus,
    #[serde(default, skip_serializing_if = "is_false")]
    pub end_estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub speaker: Option<String>,
    pub first_word: String,
    pub last_word: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingData {
    pub contract: String,
    pub meeting: Meeting,
    pub recording: Recording,
    pub version: VersionInfo,
    pub coverage: Vec<Coverage>,
    pub speakers: Vec<Speaker>,
    pub speaker_spans: Vec<SpeakerSpan>,
    pub words: Vec<Word>,
    pub turns: Vec<Turn>,
}

impl MeetingData {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("meeting data serializes")
    }
}

// ---------------------------------------------------------------- engine-side types

/// One word as an engine reports it, times local to its channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineWord {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub end_estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiarTurn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: String,
}

/// The part of a diarization (or VAD) engine result that compose reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diarization {
    pub turns: Vec<DiarTurn>,
}

/// An asr.stream event; only what `live_words` reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamEvent {
    pub segment: i64,
    pub revision: i64,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub emitted_at_ms: i64,
    pub units: Vec<StreamUnit>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamUnit {
    pub text: String,
    pub start_ms: i64,
}

/// Everything compose needs about one channel.
#[derive(Debug, Clone)]
pub struct ChannelInput {
    pub role: String,
    pub label: Option<String>,
    pub words: Vec<EngineWord>,
    pub coverage: Vec<Span>,
    pub producers: Vec<Value>,
    pub diarization: Option<Diarization>,
    pub single_speaker: bool,
    pub offset_ms: i64,
}

impl ChannelInput {
    pub fn new(role: &str, words: Vec<EngineWord>, coverage: Vec<Span>, producers: Vec<Value>) -> Self {
        ChannelInput {
            role: role.to_string(),
            label: None,
            words,
            coverage,
            producers,
            diarization: None,
            single_speaker: false,
            offset_ms: 0,
        }
    }
}

// ---------------------------------------------------------------- composing

/// Fold asr.stream events into words: the latest revision of each final
/// segment wins (replace semantics). Ends are estimated.
pub fn live_words(events: &[StreamEvent]) -> Vec<EngineWord> {
    let mut latest: BTreeMap<i64, &StreamEvent> = BTreeMap::new();
    for e in events.iter().filter(|e| e.is_final) {
        match latest.get(&e.segment) {
            Some(prev) if e.revision < prev.revision => {}
            _ => {
                latest.insert(e.segment, e);
            }
        }
    }
    let mut words = Vec::new();
    for e in latest.values() {
        let mut seg: Vec<(i64, String)> = Vec::new();
        for u in &e.units {
            match seg.last_mut() {
                Some(last) if !u.text.starts_with(' ') => last.1.push_str(&u.text),
                _ => seg.push((u.start_ms, u.text.trim().to_string())),
            }
        }
        for i in 0..seg.len() {
            let start = seg[i].0;
            let end = if i + 1 < seg.len() { seg[i + 1].0 } else { e.emitted_at_ms };
            words.push(EngineWord {
                start_ms: start,
                end_ms: end.max(start),
                text: seg[i].1.clone(),
                p: None,
                end_estimated: true,
            });
        }
    }
    words.retain(|w| !w.text.is_empty());
    words
}

fn overlap(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// A word goes to a speaker only when exactly one speaker covers at least
/// half of it. Zero-length words use their start point.
pub fn assign_speaker(start: i64, end: i64, spans: &[SpeakerSpan]) -> (Option<String>, SpeakerStatus) {
    let length = end - start;
    let mut covering: BTreeSet<&str> = BTreeSet::new();
    for s in spans {
        if length == 0 {
            if s.start_ms <= start && start < s.end_ms {
                covering.insert(&s.speaker);
            }
        } else if overlap(start, end, s.start_ms, s.end_ms) * 2 >= length {
            covering.insert(&s.speaker);
        }
    }
    match covering.len() {
        1 => (Some(covering.iter().next().unwrap().to_string()), SpeakerStatus::Assigned),
        0 => (None, SpeakerStatus::Unassigned),
        _ => (None, SpeakerStatus::Ambiguous),
    }
}

/// "<label> <n>", with the label shortened so the result stays within the schema's 100 characters.
fn numbered(label: &str, n: usize) -> String {
    let suffix = format!(" {n}");
    let room = 100usize.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", label.chars().take(room).collect::<String>())
}

/// Engine times are channel-local; `offset_ms` maps them to the meeting.
pub fn compose(
    meeting: Meeting,
    recording: Recording,
    version: (String, VersionKind),
    channels: &BTreeMap<u32, ChannelInput>,
) -> MeetingData {
    let (mut speakers, mut spans, mut coverage, mut producers) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // (start, channel, pos) is the sort key; pos keeps engine order for ties
    let mut words: Vec<(usize, Word)> = Vec::new();
    for (&ch, c) in channels {
        let off = c.offset_ms;
        producers.extend(c.producers.iter().cloned());
        coverage.extend(c.coverage.iter().map(|s| Coverage {
            channel: ch,
            start_ms: s.start_ms + off,
            end_ms: s.end_ms + off,
        }));
        let label = c.label.clone().unwrap_or_else(|| c.role.clone());
        let channel_decides = c.diarization.is_none() || c.single_speaker;
        let mut ch_spans: Vec<SpeakerSpan> = Vec::new();
        let mut ch_speaker = String::new();
        if channel_decides {
            // the channel decides the speaker; speech spans come from a one-label
            // diarization (VAD) when there is one, otherwise there are none:
            // coverage is not speech, and using it would count silence as talk
            let sid = format!("s{}", speakers.len());
            speakers.push(Speaker { id: sid.clone(), channel: ch, basis: Basis::Channel, label: Some(label) });
            if let Some(d) = &c.diarization {
                ch_spans = d
                    .turns
                    .iter()
                    .map(|t| SpeakerSpan {
                        speaker: sid.clone(),
                        channel: ch,
                        start_ms: t.start_ms + off,
                        end_ms: t.end_ms + off,
                    })
                    .collect();
            }
            ch_speaker = sid;
        } else if let Some(d) = &c.diarization {
            let mut local: HashMap<&str, String> = HashMap::new();
            for t in &d.turns {
                if !local.contains_key(t.speaker.as_str()) {
                    let id = format!("s{}", speakers.len());
                    local.insert(&t.speaker, id.clone());
                    speakers.push(Speaker {
                        id,
                        channel: ch,
                        basis: Basis::Diarization,
                        label: Some(numbered(&label, local.len())),
                    });
                }
            }
            ch_spans = d
                .turns
                .iter()
                .map(|t| SpeakerSpan {
                    speaker: local[t.speaker.as_str()].clone(),
                    channel: ch,
                    start_ms: t.start_ms + off,
                    end_ms: t.end_ms + off,
                })
                .collect();
        }
        spans.extend(ch_spans.iter().cloned());
        for (pos, w) in c.words.iter().enumerate() {
            let (start, end) = (w.start_ms + off, w.end_ms + off);
            let (speaker, status) = if channel_decides {
                (Some(ch_speaker.clone()), SpeakerStatus::Assigned)
            } else {
                assign_speaker(start, end, &ch_spans)
            };
            words.push((
                pos,
                Word {
                    id: String::new(),
                    channel: ch,
                    start_ms: start,
                    end_ms: end,
                    text: w.text.clone(),
                    speaker,
                    speaker_status: status,
                    end_estimated: w.end_estimated,
                    p: w.p,
                },
            ));
        }
    }
    words.sort_by_key(|(pos, w)| (w.start_ms, w.channel, *pos));
    let mut words: Vec<Word> = words.into_iter().map(|(_, w)| w).collect();
    for (i, w) in words.iter_mut().enumerate() {
        w.id = format!("w{i:05}");
    }
    let mut turns: Vec<Turn> = Vec::new();
    let mut keys: Vec<(u32, Option<String>)> = Vec::new();
    for w in &words {
        let key = (w.channel, w.speaker.clone());
        if let (Some(t), Some(k)) = (turns.last_mut(), keys.last()) {
            if *k == key && w.start_ms - t.end_ms < TURN_GAP_MS {
                t.last_word = w.id.clone();
                t.end_ms = t.end_ms.max(w.end_ms);
                continue;
            }
        }
        turns.push(Turn {
            id: format!("t{:04}", turns.len()),
            speaker: w.speaker.clone(),
            first_word: w.id.clone(),
            last_word: w.id.clone(),
            start_ms: w.start_ms,
            end_ms: w.end_ms,
        });
        keys.push(key);
    }
    // engines round and pad: a span or word may run past the end of the audio.
    // The recording is the authority, so clip to it (found by property testing).
    let dur = recording.duration_ms;
    let clip = |s: &mut i64, e: &mut i64| {
        *s = (*s).min(dur);
        *e = (*e).min(dur);
    };
    spans.iter_mut().for_each(|x: &mut SpeakerSpan| clip(&mut x.start_ms, &mut x.end_ms));
    words.iter_mut().for_each(|x| clip(&mut x.start_ms, &mut x.end_ms));
    turns.iter_mut().for_each(|x| clip(&mut x.start_ms, &mut x.end_ms));
    coverage.iter_mut().for_each(|x: &mut Coverage| clip(&mut x.start_ms, &mut x.end_ms));
    spans.retain(|s| s.end_ms > s.start_ms);
    spans.sort_by_key(|s| (s.start_ms, s.channel));
    MeetingData {
        contract: CONTRACT.to_string(),
        meeting,
        recording,
        version: VersionInfo { id: version.0, kind: version.1, parent: None, created_at: None, producers },
        coverage,
        speakers,
        speaker_spans: spans,
        words,
        turns,
    }
}

// ---------------------------------------------------------------- invariants

/// Rules JSON Schema cannot express. Each message names the rule (I1..I9).
pub fn invariant_errors(md: &MeetingData) -> Vec<String> {
    let mut errs = Vec::new();
    let dur = md.recording.duration_ms;
    let channels: BTreeSet<u32> = md.meeting.channels.iter().map(|c| c.index).collect();
    let speakers: HashMap<&str, &Speaker> = md.speakers.iter().map(|s| (s.id.as_str(), s)).collect();
    let dup = |ids: Vec<&str>| ids.len() != ids.iter().collect::<BTreeSet<_>>().len();
    if dup(md.words.iter().map(|x| x.id.as_str()).collect()) {
        errs.push("I1 duplicate ids in words".to_string());
    }
    if dup(md.speakers.iter().map(|x| x.id.as_str()).collect()) {
        errs.push("I1 duplicate ids in speakers".to_string());
    }
    if dup(md.turns.iter().map(|x| x.id.as_str()).collect()) {
        errs.push("I1 duplicate ids in turns".to_string());
    }
    let index: HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    let mut prev: Option<(i64, u32)> = None;
    for w in &md.words {
        let key = (w.start_ms, w.channel);
        if prev.is_some_and(|p| key < p) {
            errs.push(format!("I2 {} out of order", w.id));
        }
        prev = Some(key);
        if !(0 <= w.start_ms && w.start_ms <= w.end_ms && w.end_ms <= dur) {
            errs.push(format!("I3 {} times {}..{} outside 0..{}", w.id, w.start_ms, w.end_ms, dur));
        }
        if !channels.contains(&w.channel) {
            errs.push(format!("I4 {} unknown channel {}", w.id, w.channel));
        }
        if w.speaker.is_none() != (w.speaker_status != SpeakerStatus::Assigned) {
            errs.push(format!("I4 {} speaker/status mismatch", w.id));
        }
        if let Some(sp) = &w.speaker {
            if speakers.get(sp.as_str()).is_none_or(|s| s.channel != w.channel) {
                errs.push(format!("I4 {} speaker {} missing or on another channel", w.id, sp));
            }
        }
        if !md
            .coverage
            .iter()
            .any(|c| c.channel == w.channel && c.start_ms <= w.start_ms && w.end_ms <= c.end_ms)
        {
            errs.push(format!("I5 {} outside coverage", w.id));
        }
    }
    for s in &md.speaker_spans {
        if speakers.get(s.speaker.as_str()).is_none_or(|x| x.channel != s.channel) {
            errs.push(format!("I6 span speaker {} missing or on another channel", s.speaker));
        }
        if !(0 <= s.start_ms && s.start_ms <= s.end_ms && s.end_ms <= dur) {
            errs.push(format!("I6 span {}..{} outside 0..{}", s.start_ms, s.end_ms, dur));
        }
    }
    let mut covered: Vec<usize> = Vec::new();
    for t in &md.turns {
        let (a, b) = (index.get(t.first_word.as_str()), index.get(t.last_word.as_str()));
        let (a, b) = match (a, b) {
            (Some(&a), Some(&b)) if a <= b => (a, b),
            _ => {
                errs.push(format!("I7 {} bad word range", t.id));
                continue;
            }
        };
        let run = &md.words[a..=b];
        if run.iter().any(|w| (w.channel, &w.speaker) != (run[0].channel, &t.speaker)) {
            errs.push(format!("I7 {} mixes channels or speakers", t.id));
        }
        let lo = run.iter().map(|w| w.start_ms).min().unwrap();
        let hi = run.iter().map(|w| w.end_ms).max().unwrap();
        if t.start_ms != lo || t.end_ms != hi {
            errs.push(format!("I7 {} time is not its words' span", t.id));
        }
        let mut end = run[0].end_ms;
        for w in &run[1..] {
            if w.start_ms - end >= TURN_GAP_MS {
                errs.push(format!("I7 {} spans a pause of {} ms or more", t.id, TURN_GAP_MS));
                break;
            }
            end = end.max(w.end_ms);
        }
        covered.extend(a..=b);
    }
    covered.sort_unstable();
    if covered != (0..md.words.len()).collect::<Vec<_>>() {
        errs.push("I7 turns do not cover every word exactly once".to_string());
    }
    if !md.version.producers.iter().any(|p| p["role"] == "asr") {
        errs.push("I8 no asr producer".to_string());
    }
    for c in &md.coverage {
        if !channels.contains(&c.channel) || c.start_ms > c.end_ms {
            errs.push(format!("I9 bad coverage {:?}", c));
        }
    }
    errs
}

// ---------------------------------------------------------------- plugin output

/// Schema (strict) + references into the input version.
pub fn output_errors(md: &MeetingData, out: &Value) -> Vec<String> {
    let mut errs = schema_errors(plugin_output_validator(), out);
    if !errs.is_empty() {
        return errs;
    }
    if out["input"]["version_id"] != json!(md.version.id) {
        errs.push("O1 output is for another version".to_string());
    }
    let index: HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    let speakers: BTreeSet<&str> = md.speakers.iter().map(|s| s.id.as_str()).collect();

    let anchor = |a: &Value, where_: &str, errs: &mut Vec<String>| {
        let ends = (a["from"].as_str().and_then(|s| index.get(s)), a["to"].as_str().and_then(|s| index.get(s)));
        match ends {
            (Some(f), Some(t)) if f > t => errs.push(format!("O2 {where_}: from after to")),
            (Some(_), Some(_)) => {}
            _ => errs.push(format!("O2 {where_}: unknown word id")),
        }
    };
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    for (n, it) in list(&out["items"]).iter().enumerate() {
        let w = format!("items/{n}");
        match it["type"].as_str().unwrap_or("") {
            "transcript_block" => anchor(&it["anchor"], &w, &mut errs),
            "timeline_marker" => anchor(&it["at"], &w, &mut errs),
            "summary_card" => {
                for p in list(&it["points"]) {
                    list(&p["evidence"]).iter().for_each(|a| anchor(a, &w, &mut errs));
                }
            }
            "action_items" => {
                for x in list(&it["items"]) {
                    list(&x["evidence"]).iter().for_each(|a| anchor(a, &w, &mut errs));
                    if let Some(owner) = x.get("owner").and_then(Value::as_str) {
                        if !speakers.contains(owner) {
                            errs.push(format!("O3 {w}: owner {owner} is not a speaker of this version"));
                        }
                    }
                }
            }
            "table" => {
                let width = list(&it["columns"]).len();
                for r in list(&it["rows"]) {
                    if list(&r["cells"]).len() != width {
                        errs.push(format!("O4 {w}: row width differs from columns"));
                    }
                    list(&r["evidence"]).iter().for_each(|a| anchor(a, &w, &mut errs));
                }
            }
            _ => {}
        }
    }
    errs
}

/// Anchor -> (channels, start_ms, end_ms). The GUI jumps here. None for an unknown or
/// reversed anchor. Ids index like the reference's dict: the last duplicate wins.
pub fn resolve(md: &MeetingData, from: &str, to: &str) -> Option<(Vec<u32>, i64, i64)> {
    let index: HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    let (a, b) = (*index.get(from)?, *index.get(to)?);
    if a > b {
        return None;
    }
    let run = &md.words[a..=b];
    let chans: BTreeSet<u32> = run.iter().map(|w| w.channel).collect();
    Some((
        chans.into_iter().collect(),
        run.iter().map(|w| w.start_ms).min()?,
        run.iter().map(|w| w.end_ms).max()?,
    ))
}
