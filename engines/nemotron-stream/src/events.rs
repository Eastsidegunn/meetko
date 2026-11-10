//! Recognizer hypothesis -> contract `asr.stream` event. Unlike the transducer engine, the words
//! here come whole from the library with a start and an end, already in milliseconds from the
//! start of the stream (not from the segment), so nothing is added to them.

use serde_json::{json, Value};

use crate::ffi::Word;

/// Characters the contract forbids inside a unit (schema `units[].text` pattern).
fn forbidden(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{20}' | '\u{7f}'..='\u{a0}' | '\u{1680}' | '\u{180e}' | '\u{2000}'..='\u{200b}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// One unit per word, each with the leading space that marks a word start. A word with nothing
/// left after the forbidden characters is dropped; one longer than the schema allows is cut.
pub fn units(words: &[Word]) -> Vec<Value> {
    words
        .iter()
        .filter_map(|w| {
            let body: String = w.text.chars().filter(|c| !forbidden(*c)).take(199).collect();
            let start_ms = w.start_ms.max(0);
            (!body.is_empty()).then(|| json!({ "text": format!(" {body}"), "start_ms": start_ms, "end_ms": w.end_ms.max(start_ms) }))
        })
        .collect()
}

pub struct EventIn<'a> {
    pub segment: i64,
    pub revision: i64,
    pub is_final: bool,
    /// where the segment is known to begin when it has no words yet (a partial)
    pub start_ms: i64,
    pub text: &'a str,
    pub words: &'a [Word],
    pub fed_ms: i64,
}

pub fn event(e: &EventIn) -> Value {
    let units = units(e.words);
    let start_ms = units.first().map_or(e.start_ms, |u| u["start_ms"].as_i64().unwrap_or(0));
    json!({
        "contract": "0.1", "kind": "asr.stream", "segment": e.segment, "revision": e.revision,
        "final": e.is_final, "start_ms": start_ms.max(0),
        "text": e.text.trim(), "emitted_at_ms": e.fed_ms,
        "units": units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, s: i64, e: i64) -> Word {
        Word { text: text.into(), start_ms: s, end_ms: e }
    }

    #[test]
    fn every_word_opens_a_unit_with_absolute_times() {
        let u = units(&[w("지난주", 720, 1040), w("NCCL", 1280, 1600)]);
        assert_eq!(u[0], json!({ "text": " 지난주", "start_ms": 720, "end_ms": 1040 }));
        assert_eq!(u[1]["text"], " NCCL");
        assert_eq!(u[1]["start_ms"], 1280, "times are not shifted by a segment base");
    }

    #[test]
    fn units_never_carry_forbidden_characters_or_break_the_schema() {
        let u = units(&[w("\u{3000}", 0, 0), w("a\tb", 10, 5), w(&"가".repeat(300), 20, 30)]);
        assert_eq!(u.len(), 2, "a word of only forbidden characters is dropped");
        assert_eq!(u[0]["text"], " ab");
        assert_eq!(u[0]["end_ms"], 10, "an end before the start is raised to it");
        assert_eq!(u[1]["text"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn a_final_starts_at_its_first_word_and_a_partial_where_the_caller_says() {
        let words = [w("가", 3000, 3200), w("나", 3300, 3500)];
        let f = event(&EventIn { segment: 2, revision: 4, is_final: true, start_ms: 2500, text: " 가 나 ", words: &words, fed_ms: 5000 });
        assert_eq!((f["start_ms"].as_i64(), f["text"].as_str(), f["emitted_at_ms"].as_i64(), f["final"].as_bool()), (Some(3000), Some("가 나"), Some(5000), Some(true)));
        assert_eq!(f["units"].as_array().unwrap().len(), 2);
        let p = event(&EventIn { segment: 3, revision: 0, is_final: false, start_ms: 3500, text: "다", words: &[], fed_ms: 5100 });
        assert_eq!((p["start_ms"].as_i64(), p["units"].as_array().unwrap().len()), (Some(3500), 0));
    }
}
