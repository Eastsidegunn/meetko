//! The user's hint terms (`vocabulary`) as whisper-cli's initial prompt, within whisper's budget.
//!
//! How whisper.cpp 1.9.5 uses a prompt (`whisper_full_with_state`): only when `n_max_text_ctx` (`-mc`)
//! is above 0, and then at most `min(-mc, n_text_ctx / 2)` tokens of context per 30 s window: the
//! `--carry-initial-prompt` tokens first (cut to that budget minus one, keeping their end), the rest
//! filled with the text decoded in the window before. meetko decodes with `-mc 0` (no carried text, see
//! `whisper::decode_args`), which drops a prompt entirely. So a run with hints passes
//! `--prompt <terms> --carry-initial-prompt -mc <prompt tokens + 1>`: the prompt is given to every window
//! and leaves no room for decoded text, which keeps the `-mc 0` behaviour otherwise.
//!
//! That needs the prompt's exact token count, so the model's own vocabulary is read from the file
//! (the ggml header: hparams, mel filters, then the token strings) and the text is split the way
//! whisper.cpp's `tokenize` does: its GPT-2 pattern under `std::regex` in the C locale (letters and digits
//! are ASCII only), then the longest vocabulary entry from the left. The terms are joined with ", " in
//! the user's order and taken while the whole prompt fits `n_text_ctx / 2 - 1` tokens; the first term that
//! does not fit ends the list (deterministic, never reordered).
//!
//! Recorded in the producer's params (`prompt`): how many terms were used of how many and the token
//! count, never the terms or a hash of them (a transcript's provenance is exported and shared; the
//! vocabulary may hold names, and an unkeyed hash would let anyone confirm a guessed list).

use std::collections::HashSet;
use std::io::{BufReader, Read};
use std::path::Path;

use serde_json::{json, Value};

use crate::error::{Error, Result};

const GGML_MAGIC: u32 = 0x6767_6d6c;
const MAX_VOCAB: i32 = 1 << 20;
const MAX_TOKEN_BYTES: u32 = 1 << 10;
const MAX_FILTER_FLOATS: u64 = 1 << 20;

/// What the prompt is made of: the terms that fit, as one text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    /// whisper's token count of `text`
    pub tokens: usize,
    /// terms used, of `of` configured
    pub terms: usize,
    pub of: usize,
}

impl Prompt {
    /// The producer params entry: counts only.
    pub fn params(&self) -> Value {
        json!({ "terms": self.terms, "of": self.of, "tokens": self.tokens })
    }
}

/// A whisper model's tokenizer: its vocabulary strings, and the text context it was trained with.
pub struct Tokenizer {
    vocab: HashSet<Vec<u8>>,
    longest: usize,
    pub n_text_ctx: usize,
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32(r: &mut impl Read) -> Result<i32> {
    Ok(read_u32(r)? as i32)
}

impl Tokenizer {
    /// Read the vocabulary from a ggml whisper model file (only its first megabytes are read).
    pub fn from_model(path: &Path) -> Result<Tokenizer> {
        let f = std::fs::File::open(path).map_err(|e| Error::ModelMissing(format!("cannot read the whisper model: {e}")))?;
        Self::read(&mut BufReader::new(f))
    }

    pub fn read(r: &mut impl Read) -> Result<Tokenizer> {
        let bad = |what: &str| Error::Engine(format!("cannot read the whisper model's vocabulary: {what}"));
        if read_u32(r)? != GGML_MAGIC {
            return Err(bad("not a ggml whisper model"));
        }
        // n_vocab, n_audio_ctx, n_audio_state, n_audio_head, n_audio_layer, n_text_ctx, n_text_state,
        // n_text_head, n_text_layer, n_mels, ftype
        let mut hp = [0i32; 11];
        for v in &mut hp {
            *v = read_i32(r)?;
        }
        let n_text_ctx = usize::try_from(hp[5]).ok().filter(|n| (2..=1 << 16).contains(n)).ok_or_else(|| bad("n_text_ctx"))?;
        let (n_mel, n_fft) = (read_i32(r)?, read_i32(r)?);
        let floats = u64::try_from(n_mel).ok().zip(u64::try_from(n_fft).ok()).map(|(a, b)| a * b).filter(|n| *n <= MAX_FILTER_FLOATS).ok_or_else(|| bad("mel filters"))?;
        std::io::copy(&mut r.by_ref().take(floats * 4), &mut std::io::sink())?;
        let n_vocab = read_i32(r)?;
        if !(1..=MAX_VOCAB).contains(&n_vocab) {
            return Err(bad("n_vocab"));
        }
        let (mut vocab, mut longest) = (HashSet::with_capacity(n_vocab as usize), 0);
        for _ in 0..n_vocab {
            let len = read_u32(r)?;
            if len > MAX_TOKEN_BYTES {
                return Err(bad("token length"));
            }
            let mut w = vec![0u8; len as usize];
            r.read_exact(&mut w)?;
            longest = longest.max(w.len());
            vocab.insert(w);
        }
        Ok(Tokenizer { vocab, longest, n_text_ctx })
    }

    /// For tests: a tokenizer over these strings.
    pub fn from_strings(tokens: &[&str], n_text_ctx: usize) -> Tokenizer {
        let vocab: HashSet<Vec<u8>> = tokens.iter().map(|t| t.as_bytes().to_vec()).collect();
        Tokenizer { longest: vocab.iter().map(Vec::len).max().unwrap_or(0), vocab, n_text_ctx }
    }

    /// The most prompt tokens whisper keeps with `--carry-initial-prompt` and a large enough `-mc`.
    pub fn budget(&self) -> usize {
        (self.n_text_ctx / 2).saturating_sub(1)
    }

    /// whisper.cpp's token count of `text` (`tokenize`: split into words, then longest match from the
    /// left; a byte no entry starts with is skipped, as whisper.cpp does).
    pub fn count(&self, text: &str) -> usize {
        let mut n = 0;
        for w in split_words(text.as_bytes()) {
            let mut i = 0;
            while i < w.len() {
                let mut j = w.len().min(i + self.longest);
                while j > i && !self.vocab.contains(&w[i..j]) {
                    j -= 1;
                }
                if j > i {
                    n += 1;
                    i = j;
                } else {
                    i += 1;
                }
            }
        }
        n
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// whisper.cpp's word split: `'s|'t|'re|'ve|'m|'ll|'d| ?[[:alpha:]]+| ?[[:digit:]]+| ?[^\s[:alpha:][:digit:]]+|\s+(?!\S)|\s+`
/// searched left to right, alternatives in order, under the C locale (bytes above 0x7f are neither
/// letters, digits nor spaces).
fn split_words(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = 0;
    let run = |from: usize, f: &dyn Fn(u8) -> bool| from + s[from..].iter().take_while(|&&b| f(b)).count();
    let other = |b: u8| !is_space(b) && !b.is_ascii_alphabetic() && !b.is_ascii_digit();
    while p < s.len() {
        let rest = &s[p..];
        let contraction = ["'s", "'t", "'re", "'ve", "'m", "'ll", "'d"].iter().find(|c| rest.starts_with(c.as_bytes())).map(|c| p + c.len());
        let class = |f: &dyn Fn(u8) -> bool| {
            // ` ?X+`: the space is optional and greedy, so with a space first it must be followed by X
            if rest[0] == b' ' && rest.len() > 1 && f(rest[1]) {
                Some(run(p + 1, f))
            } else if f(rest[0]) {
                Some(run(p, f))
            } else {
                None
            }
        };
        let end = contraction
            .or_else(|| class(&|b: u8| b.is_ascii_alphabetic()))
            .or_else(|| class(&|b: u8| b.is_ascii_digit()))
            .or_else(|| class(&other))
            .unwrap_or_else(|| {
                // `\s+(?!\S)`: the whole run if it reaches the end or is longer than one, then less one
                // before a non-space; else `\s+`
                let e = run(p, &is_space);
                if e < s.len() && e - p > 1 {
                    e - 1
                } else {
                    e
                }
            });
        out.push(&s[p..end]);
        p = end;
    }
    out
}

/// The prompt for `hints` (already checked by `vocabulary`): the terms in order, joined by ", ", while
/// the whole fits the budget. `None` when there are no hints or not even the first fits.
pub fn build(tok: &Tokenizer, hints: &[String]) -> Option<Prompt> {
    let budget = tok.budget();
    let mut best: Option<Prompt> = None;
    let mut text = String::new();
    for (i, h) in hints.iter().enumerate() {
        let candidate = if text.is_empty() { h.clone() } else { format!("{text}, {h}") };
        let tokens = tok.count(&candidate);
        if tokens == 0 || tokens > budget {
            break;
        }
        text = candidate;
        best = Some(Prompt { text: text.clone(), tokens, terms: i + 1, of: hints.len() });
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        split_words(s.as_bytes()).into_iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect()
    }

    #[test]
    fn words_split_like_whisper_cpp() {
        assert_eq!(words("Hello world's 42x"), ["Hello", " world", "'s", " 42", "x"]);
        assert_eq!(words("a, b"), ["a", ",", " b"]);
        assert_eq!(words("a  b"), ["a", " ", " b"], "a run of spaces leaves its last one to the next word");
        assert_eq!(words("a  "), ["a", "  "]);
        assert_eq!(words("쿠버네티스, k8s"), ["쿠버네티스,", " k", "8", "s"]);
        assert_eq!(words(" 회의"), [" 회의"]);
    }

    #[test]
    fn tokens_are_the_longest_entries_from_the_left() {
        let t = Tokenizer::from_strings(&["a", "b", "ab", " b", ",", "abc"], 448);
        assert_eq!(t.count("abc"), 1);
        assert_eq!(t.count("abab"), 2);
        assert_eq!(t.count("a, b"), 3);
        assert_eq!(t.count("aXb"), 2, "an unknown byte is skipped");
        assert_eq!(t.budget(), 223);
    }

    #[test]
    fn terms_are_taken_in_order_while_they_fit() {
        let t = Tokenizer::from_strings(&["a", "b", "c", " ", ","], 10); // budget 4
        let h = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let p = build(&t, &h(&["a", "bb", "c"])).unwrap();
        // "a, bb": a , ' ' b b = 5 > 4 → only "a"
        assert_eq!((p.text.as_str(), p.tokens, p.terms, p.of), ("a", 1, 1, 3));
        let p = build(&t, &h(&["a", "b"])).unwrap();
        assert_eq!((p.text.as_str(), p.tokens, p.terms), ("a, b", 4, 2));
        assert!(build(&t, &h(&["aaaaa"])).is_none(), "the first does not fit");
        assert!(build(&t, &[]).is_none());
        // the params name no term and carry no hash of them
        assert_eq!(p.params(), json!({"terms": 2, "of": 2, "tokens": 4}));
    }

    #[test]
    fn the_vocabulary_is_read_from_a_ggml_header() {
        let mut f = Vec::new();
        f.extend(GGML_MAGIC.to_le_bytes());
        for v in [3i32, 1500, 1280, 20, 32, 448, 1280, 20, 4, 2, 1] {
            f.extend(v.to_le_bytes());
        }
        f.extend(2i32.to_le_bytes());
        f.extend(3i32.to_le_bytes());
        f.extend([0u8; 2 * 3 * 4]);
        f.extend(3i32.to_le_bytes());
        for w in ["ab", "a", "회의"] {
            f.extend((w.len() as u32).to_le_bytes());
            f.extend(w.as_bytes());
        }
        f.extend([9u8; 64]); // the tensors follow
        let t = Tokenizer::read(&mut f.as_slice()).unwrap();
        assert_eq!(t.n_text_ctx, 448);
        assert_eq!(t.count("ab회의a"), 3, "three words: ab, 회의, a");
        assert!(Tokenizer::read(&mut &b"GGUF"[..]).is_err());
        assert!(Tokenizer::read(&mut &f[..40]).is_err(), "cut short");
    }
}
