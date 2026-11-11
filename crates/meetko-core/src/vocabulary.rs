//! The user's vocabulary (용어 사전): hint terms and a replacement list. One file in the data folder
//! (`vocabulary.json`), the same versioned JSON the app imports and exports:
//!
//! ```json
//! {"version": 1, "hints": ["meetko", "Kubernetes"], "replacements": [{"find": "쿠버네티스", "replace": "Kubernetes"}]}
//! ```
//!
//! - Hints are given to a batch engine that can take them before it transcribes (whisper-cli's
//!   initial prompt, see `whisper_prompt`). They change what the engine writes; nothing is changed after.
//! - Replacements change only what is shown and exported: the stored transcript is never altered.
//!   The rules run top to bottom, each once over the text the rules above it left: every occurrence of
//!   `find`, left to right and not overlapping, becomes `replace` (empty deletes). A rule never sees its
//!   own output again, the next rule does. Matching is literal and exact (case, spaces).
//!
//! Display order agreed with manual transcript editing: a turn's shown text is its manual edit when one
//! exists (the replacements never touch it), else these replacements applied to the original words.
//! `overlay::display_text` is the one place that composes the two for exports and copies (through
//! `overlay::View`); [`display_words`] gives the app's transcript the same words, one by one.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::contract::MeetingData;
use crate::error::{Error, Result};
use crate::util;

pub const FILE: &str = "vocabulary.json";
pub const VERSION: u32 = 1;

/// Caps on what is stored and imported.
pub const MAX_FILE_BYTES: usize = 1 << 20;
pub const MAX_HINTS: usize = 500;
pub const MAX_HINT_CHARS: usize = 100;
pub const MAX_REPLACEMENTS: usize = 1000;
pub const MAX_FIND_CHARS: usize = 200;
pub const MAX_REPLACE_CHARS: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub find: String,
    pub replace: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vocabulary {
    pub hints: Vec<String>,
    pub replacements: Vec<Replacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    version: u32,
    #[serde(default)]
    hints: Vec<String>,
    #[serde(default)]
    replacements: Vec<Replacement>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    hints: &'a [String],
    replacements: &'a [Replacement],
}

/// A character that may not be in a term: control characters (line breaks and tabs included), the line
/// and paragraph separators and the bidi controls that make text read differently from how it is
/// stored. Joiners and variation selectors stay allowed (emoji sequences).
fn is_forbidden_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Why a vocabulary is refused. Positions are 1-based, in the list as given. The app words these for
/// the user; `Display` is the English for logs and the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// the file or the stored JSON is larger than [`MAX_FILE_BYTES`]
    TooLarge,
    /// not JSON of this shape (the detail is serde's)
    NotVocabulary(String),
    /// a `version` this app does not read
    Version(u64),
    TooManyHints,
    TooManyReplacements,
    HintTooLong(usize),
    HintHidden(usize),
    EmptyFind(usize),
    FindTooLong(usize),
    ReplaceTooLong(usize),
    ReplacementHidden(usize),
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::TooLarge => write!(f, "the vocabulary is larger than {} KiB", MAX_FILE_BYTES >> 10),
            Problem::NotVocabulary(d) => write!(f, "not a vocabulary file: {d}"),
            Problem::Version(n) => write!(f, "vocabulary version {n} is not supported (this app reads version {VERSION})"),
            Problem::TooManyHints => write!(f, "more than {MAX_HINTS} hint terms"),
            Problem::TooManyReplacements => write!(f, "more than {MAX_REPLACEMENTS} replacements"),
            Problem::HintTooLong(i) => write!(f, "hint {i} is longer than {MAX_HINT_CHARS} characters"),
            Problem::HintHidden(i) => write!(f, "hint {i} contains a control character"),
            Problem::EmptyFind(i) => write!(f, "replacement {i} has nothing to find"),
            Problem::FindTooLong(i) => write!(f, "replacement {i}: find is longer than {MAX_FIND_CHARS} characters"),
            Problem::ReplaceTooLong(i) => write!(f, "replacement {i}: replace is longer than {MAX_REPLACE_CHARS} characters"),
            Problem::ReplacementHidden(i) => write!(f, "replacement {i} contains a control character"),
        }
    }
}

impl From<Problem> for Error {
    fn from(p: Problem) -> Error {
        Error::Invalid(p.to_string())
    }
}

pub type Checked<T> = std::result::Result<T, Problem>;

impl Vocabulary {
    pub fn is_empty(&self) -> bool {
        self.hints.is_empty() && self.replacements.is_empty()
    }

    /// The vocabulary as it is kept: hints trimmed, empty and repeated ones dropped (the first stays);
    /// everything checked against the caps. `find` is kept exactly (its spaces are part of the match)
    /// and must not be empty.
    pub fn validated(self) -> Checked<Vocabulary> {
        if self.hints.len() > MAX_HINTS {
            return Err(Problem::TooManyHints);
        }
        if self.replacements.len() > MAX_REPLACEMENTS {
            return Err(Problem::TooManyReplacements);
        }
        let hidden = |s: &str| s.chars().any(is_forbidden_char);
        let long = |s: &str, max: usize| s.chars().count() > max;
        let mut hints: Vec<String> = Vec::with_capacity(self.hints.len());
        for (i, h) in self.hints.iter().enumerate() {
            let h = h.trim();
            if long(h, MAX_HINT_CHARS) {
                return Err(Problem::HintTooLong(i + 1));
            }
            if hidden(h) {
                return Err(Problem::HintHidden(i + 1));
            }
            if !h.is_empty() && !hints.iter().any(|x| x == h) {
                hints.push(h.to_string());
            }
        }
        for (i, r) in self.replacements.iter().enumerate() {
            if r.find.is_empty() {
                return Err(Problem::EmptyFind(i + 1));
            }
            if long(&r.find, MAX_FIND_CHARS) {
                return Err(Problem::FindTooLong(i + 1));
            }
            if long(&r.replace, MAX_REPLACE_CHARS) {
                return Err(Problem::ReplaceTooLong(i + 1));
            }
            if hidden(&r.find) || hidden(&r.replace) {
                return Err(Problem::ReplacementHidden(i + 1));
            }
        }
        let v = Vocabulary { hints, replacements: self.replacements };
        if to_json(&v).len() > MAX_FILE_BYTES {
            return Err(Problem::TooLarge);
        }
        Ok(v)
    }
}

/// Parse and check the versioned JSON (an imported file or the stored one).
pub fn parse(json: &str) -> Checked<Vocabulary> {
    if json.len() > MAX_FILE_BYTES {
        return Err(Problem::TooLarge);
    }
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| Problem::NotVocabulary(e.to_string()))?;
    match v.get("version").and_then(serde_json::Value::as_u64) {
        Some(n) if n == u64::from(VERSION) => {}
        Some(n) => return Err(Problem::Version(n)),
        None => return Err(Problem::NotVocabulary("no version".into())),
    }
    let f: FileV1 = serde_json::from_value(v).map_err(|e| Problem::NotVocabulary(e.to_string()))?;
    debug_assert_eq!(f.version, VERSION);
    Vocabulary { hints: f.hints, replacements: f.replacements }.validated()
}

/// The versioned JSON of `v` (what is stored and exported).
pub fn to_json(v: &Vocabulary) -> String {
    let out = FileOut { version: VERSION, hints: &v.hints, replacements: &v.replacements };
    serde_json::to_string_pretty(&out).expect("a vocabulary serializes") + "\n"
}

/// A vocabulary file read and checked like an import: read up to one byte past the cap (so a larger
/// one is refused without reading all of it); not UTF-8 is `NotVocabulary`.
pub fn read_file(path: &Path) -> Result<Checked<Vocabulary>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut buf)?;
    Ok(match String::from_utf8(buf) {
        Ok(t) => parse(&t),
        Err(_) => Err(Problem::NotVocabulary("not UTF-8 text".into())),
    })
}

/// The stored vocabulary, empty when there is none yet; a damaged file is a `Problem` (shown, not
/// silently replaced).
pub fn load_checked(data_dir: &Path) -> Result<Checked<Vocabulary>> {
    let path = data_dir.join(FILE);
    if !path.exists() {
        return Ok(Ok(Vocabulary::default()));
    }
    read_file(&path)
}

/// The stored vocabulary; empty when there is none yet. A damaged file is an error.
pub fn load(data_dir: &Path) -> Result<Vocabulary> {
    Ok(load_checked(data_dir)??)
}

/// The hint terms a batch engine gets: the stored ones, or none when the file cannot be read (a
/// damaged vocabulary must not stop a transcription; the settings show the error).
pub fn hints_for_engine(data_dir: &Path) -> Vec<String> {
    match load(data_dir) {
        Ok(v) => v.hints,
        Err(e) => {
            eprintln!("vocabulary: not used: {e}");
            Vec::new()
        }
    }
}

/// Check and store `v`: written to a private temp file in the data folder and renamed over the old one.
/// Returns what was stored (hints trimmed and deduplicated).
pub fn save(data_dir: &Path, v: Vocabulary) -> Result<Vocabulary> {
    static SAVE: Mutex<()> = Mutex::new(());
    let v = v.validated()?;
    let json = to_json(&v);
    let _guard = SAVE.lock().unwrap_or_else(|p| p.into_inner());
    util::create_private_dir(data_dir)?;
    let tmp = data_dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = util::create_private(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, data_dir.join(FILE))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(v)
}

// ---------------------------------------------------------------- replacements

/// How long a text may grow under the replacement list: four times its length, or 64 KiB more,
/// whichever is larger. A rule whose replacement contains its own find, repeated down the list, grows
/// the text exponentially; a list that would pass this bound is not applied to that text at all (it is
/// shown as it is), so the result never depends on where a cut fell.
pub fn growth_limit(len: usize) -> usize {
    len.saturating_mul(4).max(len + (64 << 10))
}

/// The text with its origin: for every byte, the byte of the original text it comes from (a replacement
/// comes from where its match started) and whether it is that original byte unchanged. Both are bounded
/// by [`growth_limit`].
struct Traced {
    text: String,
    from: Vec<(usize, bool)>,
}

fn original(s: &str, trace: bool) -> Traced {
    Traced { text: s.to_string(), from: if trace { (0..s.len()).map(|i| (i, true)).collect() } else { Vec::new() } }
}

fn run(v: &Vocabulary, s: &str, trace: bool) -> Traced {
    let limit = growth_limit(s.len());
    let mut t = original(s, trace);
    for r in &v.replacements {
        if r.find.is_empty() || !t.text.contains(r.find.as_str()) {
            continue;
        }
        let mut text = String::with_capacity(t.text.len());
        let mut from = Vec::with_capacity(if trace { t.from.len() } else { 0 });
        let mut last = 0;
        for (at, m) in t.text.match_indices(r.find.as_str()) {
            text.push_str(&t.text[last..at]);
            text.push_str(&r.replace);
            if text.len() > limit {
                return original(s, trace);
            }
            if trace {
                from.extend_from_slice(&t.from[last..at]);
                let origin = t.from[at].0;
                from.extend(std::iter::repeat_n((origin, false), r.replace.len()));
            }
            last = at + m.len();
        }
        text.push_str(&t.text[last..]);
        if text.len() > limit {
            return original(s, trace);
        }
        if trace {
            from.extend_from_slice(&t.from[last..]);
        }
        t = Traced { text, from };
    }
    t
}

/// `s` with the replacement list applied (see the module doc). Pure; `s` is not changed. A list that
/// would grow `s` past [`growth_limit`] leaves it as it is.
pub fn apply_replacements(v: &Vocabulary, s: &str) -> String {
    if v.replacements.is_empty() {
        return s.to_string();
    }
    run(v, s, false).text
}

/// The replacements applied to a run of words read as one text (joined by single spaces), handed back
/// word by word so that each shown word keeps its place (and its time).
///
/// The replaced text is cut at its spaces into pieces, and each piece goes whole to one word: the word
/// its first byte came from (a replacement comes from where its match started; a match that starts on
/// the space after a word belongs to the next word). Pieces never move backwards, and a word that gets
/// two pieces keeps them with a space. So a match across words merges them into the first ("쿠버
/// 네티스는" with "쿠버 네티스" → "Kubernetes" is "Kubernetes는" on 쿠버's time), text a deletion left
/// stays on its own word ("음 " → "" leaves 그러니까 on 그러니까), and the invariant holds:
/// the non-empty results joined by single spaces are `apply_replacements` of the joined text with its
/// runs of spaces made single and its ends trimmed.
pub fn apply_to_words(v: &Vocabulary, words: &[&str]) -> Vec<String> {
    if v.replacements.is_empty() || words.is_empty() {
        return words.iter().map(|w| w.to_string()).collect();
    }
    let joined = words.join(" ");
    // the byte of the space after word i, for every word but the last
    let mut seps = Vec::with_capacity(words.len());
    let mut at = 0;
    for w in &words[..words.len() - 1] {
        at += w.len();
        seps.push(at);
        at += 1;
    }
    let t = run(v, &joined, true);
    if t.text == joined {
        return words.iter().map(|w| w.to_string()).collect();
    }
    // the word an original byte belongs to; the space after word k leads into word k + 1
    let word_of = |origin: usize| match seps.binary_search(&origin) {
        Ok(k) => k + 1,
        Err(k) => k,
    };
    let mut out = vec![String::new(); words.len()];
    let (mut word, mut start) = (0, 0);
    for piece in t.text.split(' ') {
        if !piece.is_empty() {
            word = word.max(word_of(t.from[start].0));
            if !out[word].is_empty() {
                out[word].push(' ');
            }
            out[word].push_str(piece);
        }
        start += piece.len() + 1;
    }
    out
}

/// The shown text of every word of `md` (same order as `md.words`), or `None` when the replacements
/// change nothing in it: each turn's words passed through [`apply_to_words`], as
/// `overlay::display_text` does for an unedited turn ("" for a word a replacement removed). Words
/// outside every turn are left as they are. The stored version is not touched.
pub fn display_words(v: &Vocabulary, md: &MeetingData) -> Option<Vec<String>> {
    if v.replacements.is_empty() {
        return None;
    }
    let pos: std::collections::HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    let mut out: Vec<String> = md.words.iter().map(|w| w.text.clone()).collect();
    let mut changed = false;
    for t in &md.turns {
        let (Some(&a), Some(&b)) = (pos.get(t.first_word.as_str()), pos.get(t.last_word.as_str())) else { continue };
        if a > b {
            continue;
        }
        let run: Vec<&str> = md.words[a..=b].iter().map(|w| w.text.as_str()).collect();
        for (i, s) in apply_to_words(v, &run).into_iter().enumerate() {
            if s != out[a + i] {
                out[a + i] = s;
                changed = true;
            }
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voc(pairs: &[(&str, &str)]) -> Vocabulary {
        Vocabulary { hints: vec![], replacements: pairs.iter().map(|(f, r)| Replacement { find: f.to_string(), replace: r.to_string() }).collect() }
    }

    #[test]
    fn rules_run_top_to_bottom_each_once() {
        // the second rule sees the first one's output
        assert_eq!(apply_replacements(&voc(&[("a", "b"), ("b", "c")]), "ab"), "cc");
        // in the other order it does not
        assert_eq!(apply_replacements(&voc(&[("b", "c"), ("a", "b")]), "ab"), "bc");
        // a rule never reruns over its own output (no loop; the list as a whole is bounded, see below)
        assert_eq!(apply_replacements(&voc(&[("a", "aa")]), "a a"), "aa aa");
        assert_eq!(apply_replacements(&voc(&[("ab", "b")]), "aab"), "ab");
    }

    #[test]
    fn empty_replacement_deletes_and_matching_is_literal() {
        assert_eq!(apply_replacements(&voc(&[("음 ", "")]), "음 그러니까 음 네"), "그러니까 네");
        assert_eq!(apply_replacements(&voc(&[("A", "x")]), "a A"), "a x", "case matters");
        assert_eq!(apply_replacements(&voc(&[(".*", "x")]), "a.*b"), "axb", "no patterns");
        assert_eq!(apply_replacements(&voc(&[]), "그대로"), "그대로");
    }

    #[test]
    fn overlapping_matches_go_left_to_right_without_overlap() {
        assert_eq!(apply_replacements(&voc(&[("aa", "b")]), "aaa"), "ba");
        assert_eq!(apply_replacements(&voc(&[("aba", "x")]), "ababa"), "xba");
        // an earlier rule that takes part of a later rule's match wins
        assert_eq!(apply_replacements(&voc(&[("버네", "X"), ("쿠버네티스", "Kubernetes")]), "쿠버네티스"), "쿠X티스");
    }

    #[test]
    fn korean_and_emoji_text_is_matched_by_characters() {
        let v = voc(&[("쿠버네티스", "Kubernetes"), ("👍", "좋아요"), ("❤️", "♥")]);
        assert_eq!(apply_replacements(&v, "쿠버네티스 배포 👍👍 ❤️"), "Kubernetes 배포 좋아요좋아요 ♥");
        // a find that is only part of a multi-byte sequence never splits a character
        assert_eq!(apply_replacements(&voc(&[("네", "내")]), "쿠버네티스"), "쿠버내티스");
    }

    #[test]
    fn words_keep_their_places() {
        let v = voc(&[("쿠버 네티스", "Kubernetes"), ("음", ""), ("k8s", "Kubernetes")]);
        let out = apply_to_words(&v, &["쿠버", "네티스", "음", "k8s로", "가요"]);
        assert_eq!(out, ["Kubernetes", "", "", "Kubernetes로", "가요"]);
        // the shown text is the joined text with the replacements, without the deleted word's double space
        let shown: Vec<&str> = out.iter().map(String::as_str).filter(|s| !s.is_empty()).collect();
        assert_eq!(shown.join(" "), "Kubernetes Kubernetes로 가요");
        assert_eq!(apply_replacements(&v, "쿠버 네티스 음 k8s로 가요"), "Kubernetes  Kubernetes로 가요");
        // a match across words merges them into the first, with what is left of the last
        assert_eq!(apply_to_words(&voc(&[("쿠버 네티스", "Kubernetes")]), &["쿠버", "네티스는"]), ["Kubernetes는", ""]);
        assert_eq!(apply_to_words(&voc(&[("버 네", "X")]), &["쿠버", "네티스"]), ["쿠X티스", ""]);
        assert_eq!(apply_to_words(&voc(&[("a b", "X")]), &["a", "bc", "d"]), ["Xc", "", "d"]);
        // a deletion that takes the space after a word does not move the next word onto its time
        assert_eq!(apply_to_words(&voc(&[("음 ", "")]), &["음", "그러니까", "음", "네"]), ["", "그러니까", "", "네"]);
        // a replacement that starts on the space before a word belongs to that word
        assert_eq!(apply_to_words(&voc(&[(" 음", " um")]), &["네", "음", "좋아"]), ["네", "um", "좋아"]);
        // nothing changes: the words come back as they are
        assert_eq!(apply_to_words(&voc(&[("zz", "y")]), &["a", "b"]), ["a", "b"]);
        // a replacement that adds a space does not split a word
        assert_eq!(apply_to_words(&voc(&[("ab", "a b")]), &["ab", "c"]), ["a b", "c"]);
        // an original space inside a word is not a word boundary
        assert_eq!(apply_to_words(&voc(&[("c", "C")]), &["a b", "c"]), ["a b", "C"]);
    }

    /// Single spaces, no space at either end: how a run of shown words reads.
    fn tidy(s: &str) -> String {
        s.split(' ').filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn shown_words_read_as_the_preview_in_every_case() {
        // xorshift: the same cases every run
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let parts = ["쿠", "버", "네", "티", "스", "는", "음", "a", "b", "😀", " ", "  "];
        let text = |len: usize, next: &mut dyn FnMut(usize) -> usize| (0..len).map(|_| parts[next(parts.len())]).collect::<String>();
        for case in 0..20_000 {
            let n = 1 + next(5);
            let words: Vec<String> = (0..n).map(|_| {
                let l = next(4);
                text(l, &mut next)
            }).collect();
            let rules: Vec<(String, String)> = (0..1 + next(3)).map(|_| {
                let (f, r) = (1 + next(3), next(3));
                let find = text(f, &mut next);
                let replace = if next(3) == 0 { String::new() } else { text(r, &mut next) };
                (find, replace)
            }).collect();
            let v = Vocabulary { hints: vec![], replacements: rules.iter().map(|(f, r)| Replacement { find: f.clone(), replace: r.clone() }).collect() };
            let w: Vec<&str> = words.iter().map(String::as_str).collect();
            let out = apply_to_words(&v, &w);
            assert_eq!(out.len(), w.len());
            let shown: Vec<&str> = out.iter().map(String::as_str).filter(|s| !s.is_empty()).collect();
            assert_eq!(tidy(&shown.join(" ")), tidy(&apply_replacements(&v, &w.join(" "))), "case {case}: {words:?} {rules:?} -> {out:?}");
        }
    }

    #[test]
    fn display_follows_turns_and_never_changes_the_stored_version() {
        let md: MeetingData = serde_json::from_str(include_str!("../../../contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
        let first = md.words[0].text.clone();
        let v = voc(&[(first.as_str(), "바뀜")]);
        let shown = display_words(&v, &md).expect("changed");
        assert_eq!(shown[0], "바뀜");
        assert_eq!(md.words[0].text, first);
        assert!(display_words(&voc(&[("없는말없는말", "x")]), &md).is_none());
        assert!(display_words(&Vocabulary::default(), &md).is_none());
        // the exports read the shown words: a match across two words, and a deleted word without a gap
        let plain = crate::overlay::View::plain();
        assert!(crate::export::turns_text(&md, "?", false, false, &plain).unwrap().starts_with("지난주 인시던트 "));
        let v = voc(&[("지난주 인시던트", "지난주 장애"), ("지난주 ", "")]);
        let view = crate::overlay::View::shown(&crate::overlay::EMPTY, &v);
        let text = crate::export::turns_text(&md, "?", false, false, &view).unwrap();
        assert!(text.starts_with("장애 ") && !text.starts_with("장애  "), "{text}");
        assert!(crate::export::srt(&md, "?", &view).unwrap().contains(": 장애 "));
    }

    #[test]
    fn import_is_versioned_and_checked() {
        let ok = r#"{"version":1,"hints":[" meetko ","meetko","","Kubernetes"],"replacements":[{"find":"미트코","replace":"meetko"},{"find":"음","replace":""}]}"#;
        let v = parse(ok).unwrap();
        assert_eq!(v.hints, ["meetko", "Kubernetes"], "trimmed, empty and repeated dropped");
        assert_eq!(v.replacements.len(), 2);
        assert_eq!(parse(&to_json(&v)).unwrap(), v, "export reads back");
        assert!(to_json(&v).starts_with("{\n  \"version\": 1,"));
        // only hints, or only replacements
        assert_eq!(parse(r#"{"version":1}"#).unwrap(), Vocabulary::default());

        for bad in [
            r#"{"hints":[]}"#,
            r#"{"version":2,"hints":[]}"#,
            r#"{"version":"1"}"#,
            r#"{"version":1,"hints":"a"}"#,
            r#"{"version":1,"extra":true}"#,
            r#"{"version":1,"replacements":[{"find":"a"}]}"#,
            r#"{"version":1,"replacements":[{"find":"a","replace":"b","x":1}]}"#,
            r#"{"version":1,"replacements":[{"find":"","replace":"b"}]}"#,
            r#"{"version":1,"hints":["a\nb"]}"#,
            r#"{"version":1,"hints":["a\u0000"]}"#,
            r#"{"version":1,"replacements":[{"find":"a","replace":"b\tc"}]}"#,
            r#"{"version":1,"replacements":[{"find":"\u202ea","replace":"b"}]}"#,
            "[1]",
            "not json",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // emoji sequences with a joiner or a variation selector are fine
        assert!(parse(r#"{"version":1,"hints":["👩‍💻","❤️"]}"#).is_ok());
    }

    #[test]
    fn a_list_that_would_blow_up_the_text_leaves_it_as_it_is() {
        // each rule multiplies the text by 200: five of them would be 320 billion times the input
        let big = "a".repeat(MAX_REPLACE_CHARS);
        let v = voc(&[("a", big.as_str()); 5]);
        let s = "a".repeat(1000);
        let t0 = std::time::Instant::now();
        assert_eq!(apply_replacements(&v, &s), s);
        assert_eq!(apply_to_words(&v, &["aa", "a"]), ["aa", "a"], "the traced run is bounded the same way");
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        // ordinary growth is applied: a short word to a long name, up to the bound
        assert_eq!(apply_replacements(&voc(&[("k8s", "Kubernetes")]), "k8s"), "Kubernetes");
        let small = voc(&[("a", "aaaa")]);
        assert_eq!(apply_replacements(&small, "a").len(), 4);
        assert!(growth_limit(10) >= 64 << 10 && growth_limit(1 << 20) == 4 << 20);
    }

    #[test]
    fn problems_name_what_and_where() {
        assert_eq!(voc(&[("a", "b"), ("", "c")]).validated(), Err(Problem::EmptyFind(2)));
        let long = Vocabulary { hints: vec!["a".into(), "가".repeat(MAX_HINT_CHARS + 1)], replacements: vec![] };
        assert_eq!(long.validated(), Err(Problem::HintTooLong(2)));
        assert_eq!(parse(r#"{"version":3}"#), Err(Problem::Version(3)));
        assert!(matches!(parse("nope"), Err(Problem::NotVocabulary(_))));
    }

    #[test]
    fn caps() {
        let many = |n: usize, s: &str| serde_json::to_string(&vec![s; n]).unwrap();
        assert!(parse(&format!(r#"{{"version":1,"hints":{}}}"#, many(MAX_HINTS + 1, "a"))).is_err());
        assert!(parse(&format!(r#"{{"version":1,"hints":["{}"]}}"#, "가".repeat(MAX_HINT_CHARS + 1))).is_err());
        assert!(parse(&format!(r#"{{"version":1,"hints":["{}"]}}"#, "가".repeat(MAX_HINT_CHARS))).is_ok());
        let reps = |n: usize| format!(r#"{{"version":1,"replacements":[{}]}}"#, vec![r#"{"find":"a","replace":"b"}"#; n].join(","));
        assert!(parse(&reps(MAX_REPLACEMENTS)).is_ok());
        assert!(parse(&reps(MAX_REPLACEMENTS + 1)).is_err());
        assert!(parse(&format!(r#"{{"version":1,"replacements":[{{"find":"{}","replace":""}}]}}"#, "a".repeat(MAX_FIND_CHARS + 1))).is_err());
        assert!(parse(&" ".repeat(MAX_FILE_BYTES + 1)).is_err());
    }

    #[test]
    fn stored_privately_and_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("data");
        assert_eq!(load(&d).unwrap(), Vocabulary::default(), "nothing stored yet");
        let v = save(&d, Vocabulary { hints: vec![" a ".into()], replacements: vec![] }).unwrap();
        assert_eq!(v.hints, ["a"]);
        assert_eq!(load(&d).unwrap(), v);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(save(&d, voc(&[("", "x")])).is_err());
        assert_eq!(load(&d).unwrap(), v, "a refused save keeps the old file");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temp file left");
        std::fs::write(d.join(FILE), "{").unwrap();
        assert!(load(&d).is_err(), "a damaged file is an error, not an empty vocabulary");
    }
}
