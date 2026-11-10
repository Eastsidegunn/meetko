//! Raw recognizer snapshot -> contract `asr.stream` event (the Rust form of
//! `reference/adapters.py::sherpa_stream`; the facts it documents apply here too).

use serde_json::{json, Value};

/// Characters the contract forbids inside a unit (schema `units[].text` pattern).
fn forbidden(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{20}' | '\u{7f}'..='\u{a0}' | '\u{1680}' | '\u{180e}' | '\u{2000}'..='\u{200b}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// Units with a leading space on a word start. A bare " " token opens a word for the next
/// token. Tokens carry a start only (no end). The model often drops spaces between words;
/// that is its output and is not repaired here.
pub fn units(tokens: &[String], timestamps: &[f64], base_s: f64) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending_space = false;
    for (tok, ts) in tokens.iter().zip(timestamps) {
        let opens = tok.starts_with(' ') || tok.starts_with('\u{2581}');
        let body: String = tok.chars().filter(|c| *c != '\u{2581}' && !forbidden(*c)).collect();
        if body.is_empty() {
            pending_space |= !tok.is_empty();
            continue;
        }
        let text = if pending_space || opens { format!(" {body}") } else { body };
        pending_space = false;
        let start_ms = ((base_s + ts) * 1000.0).round().max(0.0) as i64;
        out.push(json!({ "text": text, "start_ms": start_ms }));
    }
    out
}

pub struct EventIn<'a> {
    pub segment: i64,
    pub revision: i64,
    pub is_final: bool,
    pub base_s: f64,
    pub text: &'a str,
    pub tokens: &'a [String],
    pub timestamps: &'a [f64],
    pub fed_ms: i64,
}

pub fn event(e: &EventIn) -> Value {
    json!({
        "contract": "0.1", "kind": "asr.stream", "segment": e.segment, "revision": e.revision,
        "final": e.is_final, "start_ms": (e.base_s * 1000.0).round().max(0.0) as i64,
        "text": e.text.trim(), "emitted_at_ms": e.fed_ms,
        "units": units(e.tokens, e.timestamps, e.base_s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bare_space_token_opens_the_next_word() {
        let u = units(&t([" 지", "난", "주", " ", "N", "C"].as_slice()), &[0.72, 0.96, 1.04, 1.28, 1.32, 1.4], 0.0);
        let texts: Vec<&str> = u.iter().map(|x| x["text"].as_str().unwrap()).collect();
        assert_eq!(texts, [" 지", "난", "주", " N", "C"]);
        assert_eq!(u[3]["start_ms"], 1320);
    }

    #[test]
    fn times_are_absolute_from_the_segment_start() {
        let u = units(&t(["가"].as_slice()), &[0.5], 3.25);
        assert_eq!(u[0]["start_ms"], 3750);
    }

    #[test]
    fn units_never_carry_forbidden_characters() {
        let u = units(&t(["\u{3000}", "a\tb", "\u{2581}"].as_slice()), &[0.0, 0.1, 0.2], 0.0);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0]["text"], " ab", "an ideographic space opens a word");
    }
}
