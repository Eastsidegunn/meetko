//! The pure part of the Qwen3-ASR batch engine: the NDJSON JSON-RPC plumbing, reading the wav piece by
//! piece, choosing where to cut, and turning aligned words into a contract `asr.batch` engine result.
//! Nothing here calls the CrispASR library (that is `ffi.rs` of the binary), so these tests run without it.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value};

pub const CONTRACT: &str = "0.1";
pub const SAMPLE_RATE: usize = 16_000;

// ---------------------------------------------------------------- protocol

pub mod rpc {
    use std::io::Write;

    use serde_json::{json, Value};

    /// One message on the protocol stream, flushed. `out` is the duplicated original stdout: the
    /// process's fd 1 is pointed at stderr, so a library that prints cannot break the framing.
    pub fn send(out: &mut dyn Write, v: &Value) {
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    pub fn reply(out: &mut dyn Write, id: &Value, result: Result<Value, (i64, String)>) {
        match result {
            Ok(r) => send(out, &json!({ "jsonrpc": "2.0", "id": id, "result": r })),
            Err((code, message)) => send(out, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })),
        }
    }

    pub fn notify(out: &mut dyn Write, method: &str, params: Value) {
        send(out, &json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

/// `audio_sha256` as the contract wants it: 64 lowercase hex digits.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The language the request names, as the model's decoder prompt takes it: `auto` (or nothing) lets
/// the model detect it (empty string); a language tag gives its base language (`ko-KR`, `ko_KR`, `KO` are
/// `ko`). Anything that is not a two- or three-letter language with an optional region is refused.
pub fn language(req: Option<&str>) -> Result<String, String> {
    let req = req.unwrap_or("auto");
    if req.is_empty() || req.eq_ignore_ascii_case("auto") {
        return Ok(String::new());
    }
    let mut parts = req.split(['-', '_']);
    let base = parts.next().unwrap_or("");
    let region_ok = parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()));
    if (2..=3).contains(&base.len()) && base.bytes().all(|b| b.is_ascii_alphabetic()) && region_ok {
        Ok(base.to_ascii_lowercase())
    } else {
        Err("language must be auto or a language tag such as ko or ko-KR".into())
    }
}

// ---------------------------------------------------------------- wav

/// A 16 kHz mono PCM16 wav, read a stretch at a time (memory stays bounded by one piece, whatever the
/// length of the recording). Anything else is refused: the core stores channels in this format, and an
/// imported file of another rate would need resampling, which this engine does not do.
pub struct Wav<R> {
    src: R,
    data_start: u64,
    /// samples in the data chunk (a data length past the end, as in a recording still being written, is
    /// cut to what the file holds)
    pub samples: usize,
}

impl Wav<std::fs::File> {
    pub fn open(path: &Path) -> Result<Self, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("cannot open the wav: {}", e.kind()))?;
        let len = f.metadata().map_err(|e| format!("cannot read the wav: {}", e.kind()))?.len();
        Wav::new(f, len)
    }
}

impl<R: Read + Seek> Wav<R> {
    pub fn new(mut src: R, file_len: u64) -> Result<Self, String> {
        let bad = |m: &str| format!("not a usable wav: {m}");
        let io = |e: std::io::Error| format!("cannot read the wav: {}", e.kind());
        let mut head = [0u8; 12];
        src.read_exact(&mut head).map_err(|_| bad("no RIFF/WAVE header"))?;
        if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(bad("no RIFF/WAVE header"));
        }
        let (mut pos, mut fmt_ok) = (12u64, false);
        loop {
            let mut ch = [0u8; 8];
            if src.read_exact(&mut ch).is_err() {
                return Err(bad("no data chunk"));
            }
            let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
            let body = pos + 8;
            if &ch[0..4] == b"fmt " {
                if len < 16 {
                    return Err(bad("short fmt chunk"));
                }
                let mut f = [0u8; 16];
                src.read_exact(&mut f).map_err(|_| bad("short fmt chunk"))?;
                let u16_at = |o: usize| u16::from_le_bytes([f[o], f[o + 1]]);
                let rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                // 1 = PCM; 0xFFFE = extensible (16-bit integer is required either way)
                if !matches!(u16_at(0), 1 | 0xFFFE) || u16_at(2) != 1 || rate as usize != SAMPLE_RATE || u16_at(14) != 16 {
                    return Err(bad("it must be 16 kHz mono 16-bit PCM"));
                }
                fmt_ok = true;
            } else if &ch[0..4] == b"data" {
                if !fmt_ok {
                    return Err(bad("no fmt chunk before the data"));
                }
                let samples = (len.min(file_len.saturating_sub(body)) / 2) as usize;
                return Ok(Wav { src, data_start: body, samples });
            }
            pos = body + len + (len & 1);
            src.seek(SeekFrom::Start(pos)).map_err(io)?;
        }
    }

    /// `n` samples from sample `start` (fewer at the end), as f32 in [-1, 1).
    pub fn read(&mut self, start: usize, n: usize) -> Result<Vec<f32>, String> {
        let n = n.min(self.samples.saturating_sub(start));
        let mut b = vec![0u8; n * 2];
        self.src.seek(SeekFrom::Start(self.data_start + start as u64 * 2)).and_then(|_| self.src.read_exact(&mut b)).map_err(|e| format!("cannot read the wav: {}", e.kind()))?;
        Ok(b.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s) as f32 / 32768.0).collect())
    }
}

pub fn duration_ms(samples: usize) -> i64 {
    (samples as i64) * 1000 / SAMPLE_RATE as i64
}

// ---------------------------------------------------------------- pieces

/// How the audio is cut. Qwen3-ASR decodes a piece in one pass; the library's own reliable window is
/// about 30 s, and the forced aligner is limited to 5 minutes per call.
#[derive(Debug, Clone, Copy)]
pub struct Cut {
    pub max_ms: usize,
    /// the cut is placed at the quietest 20 ms frame of the last `search_ms` of a piece
    pub search_ms: usize,
    /// a 20 ms frame is sound when its RMS is at least this (dBFS)
    pub sound_dbfs: f64,
    /// a piece with less sound than this in total is silence: it is not decoded, so the model cannot
    /// make up words there (a click or a breath alone does not make a piece speech)
    pub min_sound_ms: usize,
}

pub const CUT: Cut = Cut { max_ms: 30_000, search_ms: 4_000, sound_dbfs: -50.0, min_sound_ms: 200 };
const FRAME: usize = SAMPLE_RATE / 50; // 20 ms

fn rms_db(x: &[f32]) -> f64 {
    if x.is_empty() {
        return -120.0;
    }
    let e = x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64;
    if e <= 1e-12 { -120.0 } else { 10.0 * e.log10() }
}

pub fn max_samples(cut: Cut) -> usize {
    cut.max_ms * SAMPLE_RATE / 1000
}

/// Where the piece that starts at `window[0]` ends: all of `window` when it is the rest of the audio
/// (`last`) or not longer than a piece, else the middle of the quietest 20 ms frame of its last
/// `search_ms` (`window` holds at most `max_samples(cut)` samples).
pub fn next_cut(window: &[f32], last: bool, cut: Cut) -> usize {
    let max = max_samples(cut);
    if last || window.len() < max {
        return window.len().min(max);
    }
    let search = (cut.search_ms * SAMPLE_RATE / 1000).min(max / 2);
    let (from, to) = (max - search, max);
    let mut best = (f64::INFINITY, to);
    let mut f = from;
    while f + FRAME <= to {
        let d = rms_db(&window[f..f + FRAME]);
        if d < best.0 {
            best = (d, f + FRAME / 2);
        }
        f += FRAME;
    }
    best.1
}

/// The pieces of `pcm` as sample ranges, in order (what the engine does while reading, on a slice).
pub fn pieces(pcm: &[f32], cut: Cut) -> Vec<(usize, usize)> {
    let max = max_samples(cut);
    let mut out = Vec::new();
    let mut start = 0;
    while start < pcm.len() {
        let end = (start + max).min(pcm.len());
        let n = next_cut(&pcm[start..end], end == pcm.len(), cut);
        out.push((start, start + n));
        start += n;
    }
    out
}

/// Whether a piece is silence: less than `min_sound_ms` of 20 ms frames at or above `sound_dbfs`.
pub fn is_silent(pcm: &[f32], cut: Cut) -> bool {
    let loud = pcm.chunks(FRAME).filter(|f| rms_db(f) >= cut.sound_dbfs).count();
    loud * 20 < cut.min_sound_ms
}

/// The decoder's token budget for a piece: generous for fast speech, small enough that a model stuck
/// repeating itself stops. A piece that uses all of it is counted (`capped_pieces`).
pub fn max_new_tokens(samples: usize) -> i32 {
    let secs = samples as f64 / SAMPLE_RATE as f64;
    (secs * 12.0).ceil().max(64.0) as i32
}

// ---------------------------------------------------------------- words

/// Word text as the contract allows it: no whitespace, control or invisible characters (the schema's
/// pattern), at most 200 characters. Empty means there is no word.
pub fn clean_word(t: &str) -> String {
    let banned = |c: char| {
        let u = c as u32;
        u <= 0x20 || (0x7f..=0xa0).contains(&u) || matches!(u, 0x1680 | 0x180e | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff) || (0x2000..=0x200b).contains(&u)
    };
    t.chars().filter(|c| !banned(*c)).take(200).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

fn letters(s: &str) -> Vec<char> {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// How far ahead (in aligner letters) a word is looked for after a mismatch.
const RESYNC_LETTERS: usize = 64;

/// The transcript's own words (split at its spaces), each with the times of the aligner units its
/// letters fall in, or `None` when the aligner did not time it.
///
/// The aligner times Korean, Chinese and Japanese per character and Latin script per word, so its units
/// are not the transcript's words: they are matched by their letters and digits (punctuation, spaces and
/// case are ignored). A word whose letters are not next is looked for a little further on, so one word
/// the aligner wrote differently (a number spelled out, say) costs that word its time, not the rest.
/// Punctuation standing alone joins the word before it (or the next one at the start).
pub fn group(transcript: &str, aligned: &[(String, i64, i64)]) -> Vec<(String, Option<(i64, i64)>)> {
    let units: Vec<(char, i64, i64)> = aligned.iter().flat_map(|(t, s, e)| letters(t).into_iter().map(move |c| (c, *s, *e))).collect();
    let mut i = 0;
    let mut out: Vec<(String, Option<(i64, i64)>)> = Vec::new();
    let mut lead = String::new();
    for w in transcript.split_whitespace() {
        let c = letters(w);
        if c.is_empty() {
            match out.last_mut() {
                Some(last) => last.0.push_str(w),
                None => lead.push_str(w),
            }
            continue;
        }
        let at = |k: usize| units.get(k..k + c.len()).is_some_and(|span| span.iter().map(|u| u.0).eq(c.iter().copied()));
        let found = (i..=(i + RESYNC_LETTERS).min(units.len())).find(|&k| at(k));
        let time = found.map(|k| {
            let span = &units[k..k + c.len()];
            i = k + c.len();
            (span[0].1, span.iter().map(|u| u.2).max().unwrap_or(span[0].2))
        });
        out.push((format!("{}{w}", std::mem::take(&mut lead)), time));
    }
    out
}

/// Contract words for one piece (`[lo, hi]` ms). Timed words keep their times, held inside the piece
/// and never before the previous word's start (`floor`); a run of untimed words is spread evenly over
/// the gap between the timed words around it (or the piece's edges). Returns the words and how many
/// were spread: those times are not measured.
pub fn place(grouped: &[(String, Option<(i64, i64)>)], lo: i64, hi: i64, floor: i64) -> (Vec<Word>, usize) {
    let items: Vec<(String, Option<(i64, i64)>)> = grouped.iter().map(|(t, tm)| (clean_word(t), *tm)).filter(|(t, _)| !t.is_empty()).collect();
    let mut out = Vec::new();
    let mut spread = 0;
    let mut prev = floor.max(lo);
    let mut k = 0;
    while k < items.len() {
        if let Some((s, e)) = items[k].1 {
            let start = s.clamp(lo, hi).max(prev);
            out.push(Word { start_ms: start, end_ms: e.clamp(lo, hi).max(start), text: items[k].0.clone() });
            prev = start;
            k += 1;
            continue;
        }
        // a run of untimed words between the previous word's end and the next timed word's start
        let run_end = (k..items.len()).find(|&j| items[j].1.is_some()).unwrap_or(items.len());
        let from = out.last().map_or(prev, |w: &Word| w.end_ms).max(prev);
        let to = items.get(run_end).and_then(|x| x.1).map_or(hi, |(s, _)| s.clamp(lo, hi)).max(from);
        let n = (run_end - k) as i64;
        for (j, item) in items[k..run_end].iter().enumerate() {
            let j = j as i64;
            let start = from + (to - from) * j / n;
            out.push(Word { start_ms: start, end_ms: from + (to - from) * (j + 1) / n, text: item.0.clone() });
            prev = start;
        }
        spread += run_end - k;
        k = run_end;
    }
    (out, spread)
}

/// What one transcription did, for the result's `params` and `word_timing`.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    pub pieces: usize,
    pub silent_pieces: usize,
    /// pieces the aligner could not place at all (all their words spread)
    pub unaligned_pieces: usize,
    /// pieces whose decoding used the whole token budget (possibly cut short or repeating)
    pub capped_pieces: usize,
    /// words whose times were spread, not measured
    pub spread_words: usize,
}

/// The contract `asr.batch` engine result. `word_timing` is `aligned` only when every word was timed by
/// the aligner; with any spread word it is `none` (the contract has one value per result).
#[allow(clippy::too_many_arguments)]
pub fn result(engine: &Value, model_id: &str, audio_sha256: &str, params: Value, language: &str, words: &[Word], duration_ms: i64, tally: &Tally) -> Value {
    let mut params = params;
    params["pieces"] = json!(tally.pieces);
    params["silent_pieces_skipped"] = json!(tally.silent_pieces);
    params["unaligned_pieces"] = json!(tally.unaligned_pieces);
    params["capped_pieces"] = json!(tally.capped_pieces);
    params["spread_words"] = json!(tally.spread_words);
    let mut out = json!({
        "contract": CONTRACT, "kind": "asr.batch",
        "engine": engine,
        "model": { "id": model_id },
        "audio_sha256": audio_sha256,
        "params": params,
        "word_timing": if tally.spread_words > 0 { "none" } else { "aligned" },
        "network": false,
        "coverage": [{ "start_ms": 0, "end_ms": duration_ms }],
        "words": words.iter().map(|w| json!({ "start_ms": w.start_ms, "end_ms": w.end_ms, "text": w.text })).collect::<Vec<_>>(),
    });
    if !language.is_empty() {
        out["language"] = json!(language);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, bits: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        b.extend_from_slice(&(2 * channels).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    fn open(b: Vec<u8>) -> Result<Wav<std::io::Cursor<Vec<u8>>>, String> {
        let n = b.len() as u64;
        Wav::new(std::io::Cursor::new(b), n)
    }

    #[test]
    fn only_16k_mono_pcm16_is_read_and_a_stretch_at_a_time() {
        let mut w = open(wav(16_000, 1, 16, &[0, 16384, -32768, 8192])).unwrap();
        assert_eq!(w.samples, 4);
        assert_eq!(w.read(1, 2).unwrap(), [0.5, -1.0]);
        assert_eq!(w.read(3, 10).unwrap(), [0.25], "fewer at the end");
        assert!(w.read(9, 1).unwrap().is_empty());
        // a header that claims more data than the file holds (a recording still being written)
        let mut b = wav(16_000, 1, 16, &[1, 2, 3]);
        let n = b.len();
        b[n - 10..n - 6].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(open(b).unwrap().samples, 3);
        for bad in [wav(48_000, 1, 16, &[0]), wav(16_000, 2, 16, &[0, 0]), wav(16_000, 1, 8, &[0]), b"RIFFxxxxWAVE".to_vec(), b"nope".to_vec()] {
            assert!(open(bad).is_err());
        }
    }

    #[test]
    fn pieces_cover_the_audio_and_cut_where_it_is_quiet() {
        let cut = Cut { max_ms: 1_000, search_ms: 400, ..CUT };
        // 2.5 s of tone with a quiet 20 ms frame at 0.8 s
        let mut pcm: Vec<f32> = (0..40_000).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        for s in &mut pcm[12_800..13_120] {
            *s = 0.0;
        }
        let p = pieces(&pcm, cut);
        assert_eq!(p.first().unwrap().0, 0);
        assert_eq!(p.last().unwrap().1, pcm.len());
        assert!(p.windows(2).all(|w| w[0].1 == w[1].0), "contiguous: {p:?}");
        assert!(p.iter().all(|(a, b)| b > a && b - a <= 16_000), "{p:?}");
        assert_eq!(p[0].1, 12_800 + 160, "cut in the middle of the quiet frame");
        assert_eq!(pieces(&[], cut), vec![]);
        assert_eq!(pieces(&pcm[..100], cut), vec![(0, 100)]);
    }

    #[test]
    fn silence_and_a_lone_click_are_not_decoded() {
        assert!(is_silent(&vec![0.0; 16_000], CUT));
        assert!(is_silent(&vec![0.001; 16_000], CUT)); // -60 dBFS
        let tone = |from: usize, len: usize| {
            let mut x = vec![0.0; 32_000];
            x[from..from + len].iter_mut().enumerate().for_each(|(i, s)| *s = (i as f32 * 0.2).sin() * 0.1);
            x
        };
        assert!(is_silent(&tone(8_000, 640), CUT), "40 ms of sound is a click");
        assert!(!is_silent(&tone(8_000, 8_000), CUT), "half a second is speech");
    }

    fn units(v: &[(&str, i64, i64)]) -> Vec<(String, i64, i64)> {
        v.iter().map(|(t, s, e)| (t.to_string(), *s, *e)).collect()
    }

    #[test]
    fn character_units_become_the_transcripts_words() {
        let al = units(&[("안", 0, 160), ("녕", 160, 320), ("하", 320, 480), ("세", 480, 640), ("요.", 640, 880), ("API", 900, 1200), ("서", 1200, 1300), ("버", 1300, 1400)]);
        let g = group("— 안녕하세요. API 서버 —", &al);
        assert_eq!(g, vec![("—안녕하세요.".to_string(), Some((0, 880))), ("API".into(), Some((900, 1200))), ("서버—".into(), Some((1200, 1400)))]);
        assert_eq!(group("api", &units(&[("API", 0, 1)]))[0].1, Some((0, 1)), "case does not matter");
    }

    #[test]
    fn a_word_the_aligner_wrote_differently_costs_only_that_word() {
        // the transcript says "3시", the aligner wrote "세 시"
        let al = units(&[("오", 0, 100), ("후", 100, 200), ("세", 200, 300), ("시", 300, 400), ("에", 400, 500), ("만", 600, 700), ("나", 700, 800), ("요", 800, 900)]);
        let g = group("오후 3시에 만나요", &al);
        assert_eq!(g.iter().map(|x| x.1).collect::<Vec<_>>(), [Some((0, 200)), None, Some((600, 900))]);
        let (w, spread) = place(&g, 0, 1000, 0);
        assert_eq!(spread, 1);
        assert_eq!(w.iter().map(|w| (w.text.as_str(), w.start_ms, w.end_ms)).collect::<Vec<_>>(), [("오후", 0, 200), ("3시에", 200, 600), ("만나요", 600, 900)]);
        // nothing timed at all: spread over the piece
        let (w, spread) = place(&group("하나 둘 셋", &[]), 0, 300, 0);
        assert_eq!((spread, w.iter().map(|w| (w.start_ms, w.end_ms)).collect::<Vec<_>>()), (3, vec![(0, 100), (100, 200), (200, 300)]));
    }

    #[test]
    fn words_stay_in_their_piece_and_in_order() {
        let g = vec![("가".to_string(), Some((900, 1500))), ("나".into(), Some((1400, 1300))), ("\u{200b}".into(), Some((1600, 1700))), ("다".into(), Some((2500, 9000)))];
        let (w, spread) = place(&g, 1000, 3000, 0);
        assert_eq!(spread, 0);
        let got: Vec<(&str, i64, i64)> = w.iter().map(|w| (w.text.as_str(), w.start_ms, w.end_ms)).collect();
        assert_eq!(got, [("가", 1000, 1500), ("나", 1400, 1400), ("다", 2500, 3000)]);
        // the previous piece's last start holds
        assert_eq!(place(&[("a".into(), Some((0, 10)))], 0, 100, 50).0[0].start_ms, 50);
        assert_eq!(clean_word(&"가".repeat(300)).chars().count(), 200);
        assert_eq!(clean_word("a\u{3000}b\u{7}"), "ab");
    }

    #[test]
    fn word_timing_says_whether_every_word_was_measured() {
        let engine = json!({"id": "e", "version": "1"});
        let w = vec![Word { start_ms: 0, end_ms: 1, text: "a".into() }];
        let r = result(&engine, "m", &"a".repeat(64), json!({}), "ko", &w, 300, &Tally { pieces: 1, spread_words: 1, ..Default::default() });
        assert_eq!(r["word_timing"], "none");
        assert_eq!(r["params"]["spread_words"], 1);
        let r = result(&engine, "m", &"a".repeat(64), json!({}), "", &w, 300, &Tally::default());
        assert_eq!(r["word_timing"], "aligned");
        assert!(r.get("language").is_none());
    }

    #[test]
    fn languages() {
        assert_eq!(language(None).unwrap(), "");
        assert_eq!(language(Some("auto")).unwrap(), "");
        assert_eq!(language(Some("AUTO")).unwrap(), "");
        assert_eq!(language(Some("ko")).unwrap(), "ko");
        for tag in ["ko-KR", "ko_KR", "KO", "Ko-kr"] {
            assert_eq!(language(Some(tag)).unwrap(), "ko", "{tag}");
        }
        for bad in ["../x", "k", "korean", "ko-", "ko-K R", "ko-KR-\u{0}"] {
            assert!(language(Some(bad)).is_err(), "{bad}");
        }
    }
}
