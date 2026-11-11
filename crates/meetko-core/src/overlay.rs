//! What a person changed on top of a transcript: speaker names, speakers merged into another, and
//! the text of a turn. These are overlays: the stored version (the engines' output) is never
//! altered, every change can be undone, and plugins keep reading the version as it was made.
//!
//! An overlay belongs to one transcript, identified by the version's `content_sha256`: approving
//! a draft keeps its content, so the approved copy shows the same names and edits. A new transcript
//! (다시 받아쓰기, a new diarization) has new speaker ids and turn ids that need not mean the same
//! person or sentence, so the overlays of the old transcript do not apply to it. They stay with the
//! old version until the user discards them (`Store::discard_set_aside`); nothing is carried
//! over by guessing.
//!
//! Display order of a turn's text (agreed with the custom-vocabulary feature): a manual edit when
//! there is one (the vocabulary never touches it), else the original words with the vocabulary
//! replacements applied word by word (`vocabulary::apply_to_words`, each word keeps its time),
//! else the original words. [`display_text`] is the one place this is decided for every
//! reader-facing text the core makes; the app's transcript view follows the same rule.
//!
//! An edited turn keeps its turn-level time (start, end, first and last word) but has no word
//! times of its own: nothing here invents them. Word-level views (CTM, the word list of the MCP
//! `get_transcript`) stay the original words.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::contract::{MeetingData, Turn, Word};
use crate::error::{Error, Result};
use crate::text::is_plain_text;
use crate::vocabulary::{self, Vocabulary};

/// A speaker name has the length the contract allows a label.
pub const MAX_NAME_CHARS: usize = 100;
/// An edited turn's text.
pub const MAX_EDIT_CHARS: usize = 10_000;
/// Merges are stored one step deep; a longer (or circular) chain from elsewhere stops here.
const MAX_MERGE_STEPS: usize = 8;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SpeakerOverlay {
    /// the name shown instead of the label; `None` when only merged
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// another speaker id of the same transcript this one is shown as
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnEdit {
    pub text: String,
    pub edited_at: String,
}

/// The overlays of one transcript (one `content_sha256`), keyed by speaker id and turn id.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Overlay {
    pub speakers: BTreeMap<String, SpeakerOverlay>,
    pub edits: BTreeMap<String, TurnEdit>,
}

/// No overlay: the transcript as stored.
pub static EMPTY: Overlay = Overlay { speakers: BTreeMap::new(), edits: BTreeMap::new() };

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.speakers.is_empty() && self.edits.is_empty()
    }

    /// The speaker `id` is shown as: itself, or the speaker it was merged into.
    pub fn target<'a>(&'a self, id: &'a str) -> &'a str {
        let mut cur = id;
        for _ in 0..MAX_MERGE_STEPS {
            match self.speakers.get(cur).and_then(|s| s.merged_into.as_deref()) {
                Some(next) if next != cur && next != id => cur = next,
                _ => break,
            }
        }
        cur
    }

    /// The name speaker `id` is shown with: the name given to it (or to the speaker it was merged
    /// into), else that speaker's label, else its id.
    pub fn speaker_name<'a>(&'a self, md: &'a MeetingData, id: &'a str) -> &'a str {
        let t = self.target(id);
        if let Some(n) = self.speakers.get(t).and_then(|s| s.name.as_deref()) {
            return n;
        }
        md.speakers.iter().find(|s| s.id == t).map(|s| s.label.as_deref().unwrap_or(&s.id)).unwrap_or(t)
    }
}

/// The overlay plus the vocabulary whose replacements apply to unedited turns: everything a
/// reader-facing view needs.
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub overlay: &'a Overlay,
    pub vocabulary: Option<&'a Vocabulary>,
}

impl<'a> View<'a> {
    /// The transcript as stored: no names, no edits, no replacements.
    pub fn plain() -> View<'static> {
        View { overlay: &EMPTY, vocabulary: None }
    }

    pub fn of(overlay: &'a Overlay) -> View<'a> {
        View { overlay, vocabulary: None }
    }

    /// Names and edits of `overlay`, and `vocabulary`'s replacements on every turn not edited.
    pub fn shown(overlay: &'a Overlay, vocabulary: &'a Vocabulary) -> View<'a> {
        View { overlay, vocabulary: Some(vocabulary) }
    }

    /// The name a turn's speaker is shown with; `None` for a turn without a speaker.
    pub fn speaker(&self, md: &'a MeetingData, speaker: Option<&'a str>) -> Option<&'a str> {
        speaker.map(|s| self.overlay.speaker_name(md, s))
    }

    /// See [`display_text`].
    pub fn text(&self, md: &MeetingData, pos: &HashMap<&str, usize>, t: &Turn) -> Result<String> {
        display_text(md, pos, t, self.overlay, self.vocabulary)
    }
}

/// The words of a turn as the engine wrote them, joined by single spaces.
pub fn original_text(words: &[Word]) -> String {
    words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// The text a turn is shown with everywhere (transcript, exports, copies, search, MCP):
/// the manual edit if there is one (as typed: the vocabulary is not applied to it); else the
/// original words with the `vocabulary` replacements applied word by word, words a replacement
/// emptied left out (an empty string when all were); else the original words. `pos` maps word
/// ids to their index (`export::turn_words`); a turn pointing at words the data does not have is
/// an error.
pub fn display_text(md: &MeetingData, pos: &HashMap<&str, usize>, t: &Turn, overlay: &Overlay, vocabulary: Option<&Vocabulary>) -> Result<String> {
    let words = crate::export::turn_words(md, pos, t)?;
    if let Some(e) = overlay.edits.get(&t.id) {
        return Ok(e.text.clone());
    }
    Ok(match vocabulary {
        Some(v) if !v.replacements.is_empty() => {
            let run: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
            vocabulary::apply_to_words(v, &run).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
        }
        _ => original_text(words),
    })
}

/// Whitespace runs (line breaks included) become one space, the ends are trimmed.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The invisible characters an emoji is built from, allowed in names and edits that a person
/// typed: the zero-width joiner (👩‍💻) and the text/emoji variation selectors (❤️). Skin-tone
/// modifiers are visible symbols and pass anyway. Controls, bidi overrides and isolates and every
/// other hidden character stay refused.
fn emoji_part(c: char) -> bool {
    matches!(c, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}')
}

/// `text::is_plain_text`, with emoji parts allowed.
fn is_typed_text(s: &str) -> bool {
    is_plain_text(&s.chars().filter(|&c| !emoji_part(c)).collect::<String>())
}

fn checked(s: &str, what: &str, max: usize) -> Result<String> {
    let s = squash(s);
    if s.is_empty() {
        return Err(Error::Invalid(format!("{what} is empty")));
    }
    if s.chars().count() > max {
        return Err(Error::Invalid(format!("{what} is longer than {max} characters")));
    }
    if !is_typed_text(&s) {
        return Err(Error::Invalid(format!("{what} has hidden or control characters")));
    }
    Ok(s)
}

/// A speaker name as stored: one line, trimmed, 1..=100 characters, no hidden characters.
pub fn clean_name(s: &str) -> Result<String> {
    checked(s, "the speaker name", MAX_NAME_CHARS)
}

/// An edited turn's text as stored: line breaks and runs of spaces become one space (a turn is one
/// line in every export), trimmed, not empty, no hidden characters.
pub fn clean_edit(s: &str) -> Result<String> {
    checked(s, "the edited text", MAX_EDIT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> MeetingData {
        serde_json::from_str(include_str!("../../../contract/fixtures/meeting01/meeting-data.draft.json")).unwrap()
    }

    fn pos(md: &MeetingData) -> HashMap<&str, usize> {
        md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect()
    }

    #[test]
    fn display_text_prefers_the_edit_then_the_replacement_then_the_words() {
        let md = fixture();
        let p = pos(&md);
        let t = &md.turns[0];
        let upper = Vocabulary { hints: vec![], replacements: vec![vocabulary::Replacement { find: "API".into(), replace: "에이피아이".into() }] };
        let mut o = Overlay::default();
        assert!(display_text(&md, &p, t, &o, None).unwrap().starts_with("지난주 인시던트 회고부터 할게요. API"));
        assert!(display_text(&md, &p, t, &o, Some(&upper)).unwrap().contains("에이피아이 게이트웨이에서"));
        o.edits.insert(t.id.clone(), TurnEdit { text: "고친 API 문장".into(), edited_at: "x".into() });
        // the replacement never touches an edit
        assert_eq!(display_text(&md, &p, t, &o, Some(&upper)).unwrap(), "고친 API 문장");
        // other turns are untouched
        assert!(display_text(&md, &p, &md.turns[1], &o, None).unwrap().starts_with("The marketing"));
    }

    #[test]
    fn names_follow_merges_and_fall_back_to_the_label() {
        let md = fixture();
        let mut o = Overlay::default();
        assert_eq!(o.speaker_name(&md, "s1"), "상대 1");
        assert_eq!(o.speaker_name(&md, "nope"), "nope");
        o.speakers.insert("s1".into(), SpeakerOverlay { name: Some("민수".into()), merged_into: None });
        assert_eq!(o.speaker_name(&md, "s1"), "민수");
        o.speakers.insert("s0".into(), SpeakerOverlay { name: None, merged_into: Some("s1".into()) });
        assert_eq!((o.target("s0"), o.speaker_name(&md, "s0")), ("s1", "민수"));
        // a circular chain (never written by the store) ends instead of looping
        o.speakers.get_mut("s1").unwrap().merged_into = Some("s0".into());
        let _ = o.speaker_name(&md, "s0");
    }

    #[test]
    fn names_and_edits_are_cleaned_and_bounded() {
        assert_eq!(clean_name("  민수\n 님 ").unwrap(), "민수 님");
        assert!(clean_name(" \n ").is_err());
        assert!(clean_name(&"가".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(clean_name("a\u{202e}b").is_err());
        assert_eq!(clean_edit("첫 줄\n\n둘째   줄").unwrap(), "첫 줄 둘째 줄");
        assert!(clean_edit("").is_err());
        assert_eq!(clean_edit(&"가".repeat(MAX_EDIT_CHARS)).unwrap().chars().count(), MAX_EDIT_CHARS);
    }

    #[test]
    fn emoji_sequences_are_allowed_but_controls_and_bidi_are_not() {
        // ZWJ sequence, emoji and text presentation selectors, a skin-tone modifier
        for s in ["민수 👩\u{200d}💻", "❤\u{fe0f}", "☺\u{fe0e}", "👍\u{1f3fd}", "👨\u{1f3fb}\u{200d}🔧"] {
            assert_eq!(clean_name(s).unwrap(), s, "{s:?}");
            assert_eq!(clean_edit(&format!("좋아요 {s}")).unwrap(), format!("좋아요 {s}"));
        }
        // controls, bidi overrides and isolates, other invisible format characters
        for c in ['\u{1b}', '\u{7}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2068}', '\u{2069}', '\u{200b}', '\u{200f}', '\u{feff}', '\u{fe00}'] {
            assert!(clean_name(&format!("a{c}b")).is_err(), "U+{:04X}", c as u32);
            assert!(clean_edit(&format!("a{c}b")).is_err(), "U+{:04X}", c as u32);
        }
    }
}
