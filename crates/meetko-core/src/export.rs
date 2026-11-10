//! Meeting data -> standard interchange formats (port of reference/export.py), plus the
//! reader-facing ones the app saves and copies (Markdown, SRT, text with stamps).
//! One deliberate difference from the reference: WebVTT escapes `&`, `<`, `>` and line breaks
//! in cue text and voice names, so output differs from export.py only for such input.

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{MeetingData, Turn, Word};
use crate::error::{Error, Result};

fn secs(ms: i64) -> String {
    format!("{:.3}", ms as f64 / 1000.0)
}

/// `hh:mm:ss<sep>mmm`: `.` for WebVTT, `,` for SRT.
fn clock_ms(ms: i64, sep: char) -> String {
    let (h, r) = (ms / 3_600_000, ms % 3_600_000);
    format!("{h:02}:{:02}:{:02}{sep}{:03}", r / 60_000, r % 60_000 / 1000, r % 1000)
}

/// `hh:mm:ss`, the per-turn stamp of the text and Markdown exports.
fn clock(ms: i64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

fn labels(md: &MeetingData) -> HashMap<&str, &str> {
    md.speakers.iter().map(|s| (s.id.as_str(), s.label.as_deref().unwrap_or(&s.id))).collect()
}

fn positions(md: &MeetingData) -> HashMap<&str, usize> {
    md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect()
}

fn joined(run: &[Word]) -> String {
    run.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// NIST CTM: `<file> <chan> <start> <dur> <word> [<conf>]`
pub fn ctm(md: &MeetingData, uri: &str, channel: u32) -> String {
    md.words
        .iter()
        .filter(|w| w.channel == channel)
        .map(|w| {
            // `{:?}` keeps "1.0" where `{}` would print "1", as Python's repr does
            let p = w.p.map(|p| format!(" {p:?}")).unwrap_or_default();
            format!("{uri} {} {} {} {}{p}\n", channel + 1, secs(w.start_ms), secs(w.end_ms - w.start_ms), w.text)
        })
        .collect()
}

/// NIST RTTM from speaker_spans.
pub fn rttm(md: &MeetingData, uri: &str, channel: u32) -> String {
    md.speaker_spans
        .iter()
        .filter(|s| s.channel == channel)
        .map(|s| {
            format!(
                "SPEAKER {uri} 1 {} {} <NA> <NA> {} <NA> <NA>\n",
                secs(s.start_ms),
                secs(s.end_ms - s.start_ms),
                s.speaker
            )
        })
        .collect()
}

pub fn text(md: &MeetingData, channel: u32) -> String {
    md.words.iter().filter(|w| w.channel == channel).map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Errors (never panics) when a turn names a word the data does not have.
pub fn webvtt(md: &MeetingData) -> Result<String> {
    webvtt_with(md, "?", None)
}

/// WebVTT with `unassigned` as the voice of a turn without a speaker, and an optional `NOTE`
/// block after the header (players skip it).
pub fn webvtt_with(md: &MeetingData, unassigned: &str, note: Option<&str>) -> Result<String> {
    let mut out: Vec<String> = vec!["WEBVTT".into(), String::new()];
    if let Some(n) = note {
        // one line, and no "-->" (it would end the block): `>` is escaped like in a cue
        out.extend([format!("NOTE {}", cue_escape(n)), String::new()]);
    }
    for (t, who, text) in turn_lines(md, unassigned)? {
        out.push(t.id.clone());
        out.push(format!("{} --> {}", clock_ms(t.start_ms, '.'), clock_ms(t.end_ms, '.')));
        out.push(format!("<v {}>{}", cue_escape(who), cue_escape(&text)));
        out.push(String::new());
    }
    Ok(out.join("\n"))
}

/// Cue text of WebVTT (SRT has its own, `srt_escape`): `&`, `<`, `>` as entities, line breaks as spaces, so a word
/// cannot open a tag, end the cue or start a new one. Plain text passes unchanged.
fn cue_escape(s: &str) -> String {
    s.chars().fold(String::with_capacity(s.len()), |mut o, c| {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => o.push(' '),
            c => o.push(c),
        }
        o
    })
}

/// The words of a turn, or an error when the turn's range does not exist in the data.
pub fn turn_words<'a>(md: &'a MeetingData, pos: &HashMap<&str, usize>, t: &Turn) -> Result<&'a [Word]> {
    match (pos.get(t.first_word.as_str()), pos.get(t.last_word.as_str())) {
        (Some(&a), Some(&b)) if a <= b => Ok(&md.words[a..=b]),
        _ => Err(Error::Corrupt(format!("turn {} points at words that are not in {}", t.id, md.version.id))),
    }
}

/// (turn, speaker label, text) per turn; a turn without a speaker gets `unassigned`.
fn turn_lines<'a>(md: &'a MeetingData, unassigned: &'a str) -> Result<Vec<(&'a Turn, &'a str, String)>> {
    let (labels, pos) = (labels(md), positions(md));
    md.turns
        .iter()
        .map(|t| {
            let who = t.speaker.as_deref().and_then(|s| labels.get(s).copied()).unwrap_or(unassigned);
            Ok((t, who, joined(turn_words(md, &pos, t)?)))
        })
        .collect()
}

/// What a reader sees that the meeting data does not carry. The caller picks the wording (the
/// app passes Korean), so the core stays language-neutral.
pub struct Doc<'a> {
    pub title: &'a str,
    /// the label of a turn that has no speaker
    pub unassigned: &'a str,
    /// a line saying the version is not approved (live, draft); `None` for an approved one
    pub note: Option<&'a str>,
}

/// One turn per line: `[hh:mm:ss] speaker: text`, either part left out on request.
pub fn turns_text(md: &MeetingData, unassigned: &str, stamps: bool, speakers: bool) -> Result<String> {
    Ok(turn_lines(md, unassigned)?
        .into_iter()
        .map(|(t, who, text)| {
            let stamp = if stamps { format!("[{}] ", clock(t.start_ms)) } else { String::new() };
            let who = if speakers { format!("{who}: ") } else { String::new() };
            format!("{stamp}{who}{text}\n")
        })
        .collect())
}

/// Plain text with stamps and speakers, under the title (and the note).
pub fn timed_text(md: &MeetingData, doc: &Doc) -> Result<String> {
    let note = doc.note.map(|n| format!("{n}\n")).unwrap_or_default();
    Ok(format!("{}\n{note}\n{}", doc.title, turns_text(md, doc.unassigned, true, true)?))
}

/// The latest time SRT can write: its hours field has two digits.
const SRT_MAX_MS: i64 = 100 * 3_600_000 - 1;

/// SubRip: numbered cues, one per turn, the speaker in front of the text. Times past
/// 99:59:59,999 are written as that (a meeting that long is not expected).
pub fn srt(md: &MeetingData, unassigned: &str) -> Result<String> {
    let ts = |ms: i64| clock_ms(ms.clamp(0, SRT_MAX_MS), ',');
    Ok(turn_lines(md, unassigned)?
        .into_iter()
        .enumerate()
        .map(|(i, (t, who, text))| format!("{}\n{} --> {}\n{}: {}\n\n", i + 1, ts(t.start_ms), ts(t.end_ms), srt_escape(who), srt_escape(&text)))
        .collect())
}

/// SRT has no entities (`&amp;` would show as is), but players read `<i>`-style tags and ASS
/// override blocks `{\...}`: a zero-width space after the `<` or `{` that could open one keeps
/// it text. Line breaks become spaces, so a word cannot end the cue.
fn srt_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => out.push(' '),
            '<' if it.peek().is_some_and(|n| n.is_ascii_alphabetic() || *n == '/') => out.push_str("<\u{200b}"),
            '{' if it.peek() == Some(&'\\') => out.push_str("{\u{200b}"),
            c => out.push(c),
        }
    }
    out
}

/// A YAML double-quoted scalar: JSON string syntax is valid YAML. JSON leaves C1 controls, the
/// line and paragraph separators and the BOM raw, which YAML refuses or reads as breaks.
fn yaml(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes").chars().fold(String::new(), |mut o, c| {
        if matches!(c, '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}') {
            o += &format!("\\u{:04X}", c as u32);
        } else {
            o.push(c);
        }
        o
    })
}

/// Backslash what Markdown (and Obsidian: `[[links]]`, `#tags`, `==marks==`, `%%comments%%`)
/// would read as syntax, and `&` (entities).
fn md_escape(s: &str) -> String {
    s.chars().fold(String::with_capacity(s.len()), |mut o, c| {
        if "\\`*_[]<>#|~=$&%".contains(c) {
            o.push('\\');
        }
        o.push(c);
        o
    })
}

/// `role: engine version (model)` per producer, each once (one producer per channel is common).
fn engines(md: &MeetingData) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in &md.version.producers {
        let s = |v: &Value| v.as_str().unwrap_or("?").to_string();
        let mut e = format!("{}: {} {}", s(&p["role"]), s(&p["engine"]["id"]), s(&p["engine"]["version"]));
        if let Some(m) = p["model"]["id"].as_str() {
            e += &format!(" ({m})");
        }
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Markdown with YAML front matter (Obsidian shows it as properties; every value is quoted),
/// then a transcript section with one paragraph per turn: `[hh:mm:ss] **speaker**: text`.
pub fn markdown(md: &MeetingData, doc: &Doc, transcript_heading: &str) -> Result<String> {
    let v = &md.version;
    let mut out = vec!["---".to_string(), format!("title: {}", yaml(doc.title))];
    if let Some(d) = md.meeting.started_at.as_deref().or(v.created_at.as_deref()) {
        out.push(format!("date: {}", yaml(d)));
    }
    out.push(format!("duration: {}", yaml(&clock(md.meeting.duration_ms))));
    out.push(format!("version: {}", yaml(&v.id)));
    out.push(format!("kind: {}", yaml(v.kind.as_str())));
    out.push(format!("contract: {}", yaml(&md.contract)));
    out.push("engines:".into());
    out.extend(engines(md).iter().map(|e| format!("  - {}", yaml(e))));
    out.extend(["---".into(), String::new(), format!("# {}", md_escape(doc.title)), String::new()]);
    if let Some(n) = doc.note {
        out.extend([format!("> {}", md_escape(n)), String::new()]);
    }
    out.extend([format!("## {}", md_escape(transcript_heading)), String::new()]);
    for (t, who, text) in turn_lines(md, doc.unassigned)? {
        out.extend([format!("[{}] **{}**: {}", clock(t.start_ms), md_escape(who), md_escape(&text)), String::new()]);
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> MeetingData {
        serde_json::from_str(include_str!("../../../contract/fixtures/meeting01/meeting-data.draft.json")).unwrap()
    }

    const DOC: Doc = Doc { title: "회의 *retro*", unassigned: "화자 미지정", note: Some("승인 전 초안이에요.") };

    #[test]
    fn clocks_round_down_and_pad() {
        assert_eq!(clock(3_723_999), "01:02:03");
        assert_eq!(clock_ms(3_723_045, ','), "01:02:03,045");
        assert_eq!(clock_ms(700, '.'), "00:00:00.700");
    }

    #[test]
    fn markdown_has_quoted_front_matter_engines_once_and_stamped_turns() {
        let md = fixture();
        let out = markdown(&md, &DOC, "전사").unwrap();
        let (front, body) = out.strip_prefix("---\n").unwrap().split_once("\n---\n").unwrap();
        assert!(front.contains("title: \"회의 *retro*\"\n"));
        assert!(front.contains("duration: \"00:01:09\"\n"));
        assert!(front.contains("version: \"meeting01.draft.1\"\nkind: \"draft\"\ncontract: \"0.1\"\n"));
        // whisper runs once per channel but is listed once
        assert_eq!(front.matches("asr: whisper.cpp 1.9.5 (ggml-large-v3-turbo)").count(), 1);
        assert!(front.contains("  - \"diarization: sherpa-onnx.vad 2.0.1 (silero_vad)\""));
        assert!(body.contains("\n# 회의 \\*retro\\*\n\n> 승인 전 초안이에요.\n\n## 전사\n\n[00:00:00] **상대 1**: 지난주 인시던트"));
        assert_eq!(body.matches("\n[").count(), md.turns.len());
    }

    #[test]
    fn approved_markdown_has_no_note_and_text_is_escaped() {
        let mut md = fixture();
        md.words[0].text = "[[link]] #tag".into();
        let out = markdown(&md, &Doc { note: None, ..DOC }, "전사").unwrap();
        assert!(!out.contains("\n> "));
        assert!(out.contains("**상대 1**: \\[\\[link\\]\\] \\#tag"));
    }

    #[test]
    fn text_variants_and_srt() {
        let mut md = fixture();
        md.turns[1].speaker = None;
        let txt = timed_text(&md, &DOC).unwrap();
        assert!(txt.starts_with("회의 *retro*\n승인 전 초안이에요.\n\n[00:00:00] 상대 1: 지난주 인시던트"));
        assert!(txt.contains("\n[00:00:08] 화자 미지정: "));
        let plain = turns_text(&md, "?", false, false).unwrap();
        assert!(plain.starts_with("지난주 인시던트"));
        assert_eq!(plain.lines().count(), md.turns.len());
        assert!(turns_text(&md, "?", false, true).unwrap().starts_with("상대 1: 지난주"));
        let srt = srt(&md, "화자 미지정").unwrap();
        assert!(srt.starts_with("1\n00:00:00,700 --> 00:00:06,740\n상대 1: 지난주 인시던트"));
        assert!(srt.contains("\n\n2\n00:00:08,800 --> 00:00:16,740\n화자 미지정: "));
    }

    #[test]
    fn webvtt_note_and_unassigned_voice() {
        let mut md = fixture();
        assert_eq!(webvtt_with(&md, "?", None).unwrap(), webvtt(&md).unwrap());
        md.turns[0].speaker = None;
        let vtt = webvtt_with(&md, "화자 미지정", Some("초안 ---> 확인\n\n끝")).unwrap();
        assert!(vtt.starts_with("WEBVTT\n\nNOTE 초안 ---&gt; 확인  끝\n\nt0000\n00:00:00.700 --> 00:00:06.740\n<v 화자 미지정>지난주"));
    }

    #[test]
    fn cue_text_and_voices_cannot_break_out_of_their_cue() {
        let mut md = fixture();
        md.speakers.iter_mut().find(|s| s.id == "s1").unwrap().label = Some("A>B & <C>".into());
        md.words[0].text = "R&D a<b &lt; -->\n\nx".into();
        let vtt = webvtt(&md).unwrap();
        assert!(vtt.contains("\n<v A&gt;B &amp; &lt;C&gt;>R&amp;D a&lt;b &amp;lt; --&gt;  x "), "{vtt}");
        assert_eq!(vtt.matches(" --> ").count(), md.turns.len());
    }

    #[test]
    fn srt_text_stays_text_without_entities() {
        let mut md = fixture();
        md.speakers.iter_mut().find(|s| s.id == "s1").unwrap().label = Some("A>B & <C>".into());
        md.words[0].text = "R&D a<b </i> 1 < 2 {\\an8} {x} &lt;\n\nx".into();
        let srt = srt(&md, "?").unwrap();
        assert!(srt.contains("\nA>B & <\u{200b}C>: R&D a<\u{200b}b <\u{200b}/i> 1 < 2 {\u{200b}\\an8} {x} &lt;  x "), "{srt}");
        // one timing line per cue, and the blank line in the word did not end a cue
        assert_eq!(srt.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit()) && l.contains(" --> ")).count(), md.turns.len());
        assert_eq!(srt.matches("\n\n").count(), md.turns.len());
    }

    #[test]
    fn srt_clamps_times_past_its_two_hour_digits() {
        let mut md = fixture();
        (md.turns[0].start_ms, md.turns[0].end_ms) = (360_000_000, 360_001_000);
        assert!(srt(&md, "?").unwrap().starts_with("1\n99:59:59,999 --> 99:59:59,999\n"));
    }

    #[test]
    fn yaml_escapes_what_json_leaves_raw() {
        assert_eq!(yaml("a\u{85}b\u{2028}c\u{feff}\"\n"), "\"a\\u0085b\\u2028c\\uFEFF\\\"\\n\"");
        assert_eq!(yaml("\u{7f}\u{fffe}\u{ffff}"), "\"\\u007F\\uFFFE\\uFFFF\"");
        assert_eq!(yaml("회의"), "\"회의\"");
    }

    #[test]
    fn markdown_escapes_entities_and_obsidian_comments() {
        assert_eq!(md_escape("R&D %%숨김%%"), "R\\&D \\%\\%숨김\\%\\%");
    }

    #[test]
    fn a_broken_turn_is_an_error_in_every_format() {
        let mut md = fixture();
        md.turns[0].last_word = "missing".into();
        assert!(markdown(&md, &DOC, "전사").is_err());
        assert!(timed_text(&md, &DOC).is_err());
        assert!(srt(&md, "?").is_err());
    }
}
