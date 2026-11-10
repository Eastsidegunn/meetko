//! Which characters may appear in text that is shown to a person (a terminal, a consent dialog):
//! manifest fields, gallery rows, error messages that echo remote input.
//!
//! Hidden = Unicode general category Cc (control), Cf (format: bidi marks and overrides, zero-width,
//! soft hyphen, tags, Arabic/Syriac number signs, ...), Zl and Zp (line and paragraph separators), plus
//! the characters that render as blank or invisible though they are letters or marks: the Hangul fillers,
//! the combining grapheme joiner, the variation selectors and the whole tag block (unassigned code
//! points in it included). Categories come from the
//! `unicode-general-category` crate (Unicode tables of that crate's version), so new Cf characters are
//! covered by a dependency update, not by a hand-kept list.

use unicode_general_category::{get_general_category, GeneralCategory as Gc};

/// More consecutive combining marks (Mn/Me) than this is "zalgo" text that overflows its line.
pub const MAX_MARK_RUN: usize = 8;

/// A character that must not be shown as it is.
pub fn is_hidden_char(c: char) -> bool {
    matches!(get_general_category(c), Gc::Control | Gc::Format | Gc::LineSeparator | Gc::ParagraphSeparator)
        || matches!(c,
            '\u{34f}' | '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}'
            | '\u{fe00}'..='\u{fe0f}' | '\u{e0000}'..='\u{e007f}' | '\u{e0100}'..='\u{e01ef}')
}

fn is_mark(c: char) -> bool {
    matches!(get_general_category(c), Gc::NonspacingMark | Gc::EnclosingMark)
}

/// No hidden characters and no run of more than [`MAX_MARK_RUN`] combining marks. Newlines count as control.
pub fn is_plain_text(s: &str) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if is_hidden_char(c) {
            return false;
        }
        run = if is_mark(c) { run + 1 } else { 0 };
        if run > MAX_MARK_RUN {
            return false;
        }
    }
    true
}

/// `s` made safe for a terminal: hidden characters and the marks beyond a run of [`MAX_MARK_RUN`] become
/// `\u{..}`. `keep_newline` leaves `\n` (multi-line messages).
pub fn escape(s: &str, keep_newline: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        run = if is_mark(c) { run + 1 } else { 0 };
        if (c == '\n' && keep_newline) || !(is_hidden_char(c) || run > MAX_MARK_RUN) {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hidden_class_is_refused_and_ordinary_text_is_not() {
        let hidden = [
            '\u{1b}', '\u{7}', '\n', '\u{85}', '\u{ad}', '\u{34f}', '\u{61c}', '\u{180e}', '\u{200b}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202e}',
            '\u{2066}', '\u{feff}', '\u{fff9}', '\u{115f}', '\u{1160}', '\u{3164}', '\u{ffa0}', '\u{fe00}', '\u{fe0f}', '\u{e0100}', '\u{e01ef}',
            '\u{e0000}', '\u{e0001}', '\u{e007f}', '\u{600}', '\u{605}', '\u{6dd}', '\u{70f}', '\u{8e2}', '\u{110bd}', '\u{110cd}', '\u{13430}', '\u{1343f}',
            '\u{1bca0}', '\u{1bca3}', '\u{1d173}', '\u{1d17a}',
        ];
        for c in hidden {
            assert!(!is_plain_text(&format!("a{c}b")), "U+{:04X} must be refused", c as u32);
            assert!(!escape(&format!("a{c}b"), false).contains(c) || c == '\n', "U+{:04X} must be escaped", c as u32);
        }
        for s in ["plain ascii", "한국어 설명", "café", "日本語 ー テキスト", "emoji 🙂", "e\u{301}", "ก็", "a-b_c.d/e"] {
            assert!(is_plain_text(s), "{s}");
            assert_eq!(escape(s, false), s);
        }
    }

    #[test]
    fn long_runs_of_combining_marks_are_refused_but_a_few_are_fine() {
        let marks = |n: usize| format!("a{}", "\u{301}".repeat(n));
        assert!(is_plain_text(&marks(MAX_MARK_RUN)));
        assert!(!is_plain_text(&marks(MAX_MARK_RUN + 1)));
        // a base character resets the run
        assert!(is_plain_text(&format!("{}{}", marks(MAX_MARK_RUN), marks(MAX_MARK_RUN))));
        // enclosing marks count too
        assert!(!is_plain_text(&format!("a{}", "\u{20dd}".repeat(MAX_MARK_RUN + 1))));
        let e = escape(&marks(MAX_MARK_RUN + 2), false);
        assert_eq!(e.matches("\\u{301}").count(), 2);
    }

    #[test]
    fn escape_keeps_or_escapes_newlines() {
        assert_eq!(escape("a\nb\u{1b}[31m", true), "a\nb\\u{1b}[31m");
        assert_eq!(escape("a\nb", false), "a\\u{a}b");
    }
}
