//! The utterance-by-utterance decode loop, over any [`Recognize`] (the library's stream, or a
//! scripted one in tests).
//!
//! Every utterance is decoded by a fresh library stream. When the library ends an utterance (a
//! final with words), the stream is closed and a new one opened, and the audio from the end of
//! that final's last word up to what was pushed is fed to the new stream again. Without this the
//! model carries its encoder cache and decoder state across utterances: after a switch between
//! Korean and English it then writes the next utterance in the wrong script or, more often, emits
//! nothing for whole sentences (measured on synthetic code-switched meetings: most of the missing
//! text came back once each utterance started from a clean state, see the README).
//!
//! `fed` counts the samples given to the engine, which is the audio clock: `emitted_at_ms` is how
//! much audio the engine had when it said something. Word times from the library count from the
//! start of its current stream; `base` (the sample where that stream began) is added to them.

use serde_json::Value;

use crate::events;
use meetko_nemotron_stream::ffi::{Hypothesis, Word};

/// What the session needs from a recognizer stream.
pub trait Recognize {
    /// 16 kHz mono; buffers only, `next` decodes.
    fn push(&mut self, samples: &[f32]) -> Result<(), String>;
    /// Decode what was pushed and return the next result, `None` when more audio is needed.
    fn next(&mut self) -> Result<Option<Hypothesis>, String>;
    fn force_endpoint(&mut self) -> Result<(), String>;
    fn finish(&mut self) -> Result<(), String>;
    /// Drop the stream state and start a new stream whose times count from zero.
    fn restart(&mut self) -> Result<(), String>;
}

impl Recognize for meetko_nemotron_stream::ffi::Decoder {
    fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        self.push(samples)
    }
    fn next(&mut self) -> Result<Option<Hypothesis>, String> {
        self.next()
    }
    fn force_endpoint(&mut self) -> Result<(), String> {
        self.force_endpoint()
    }
    fn finish(&mut self) -> Result<(), String> {
        self.finish()
    }
    fn restart(&mut self) -> Result<(), String> {
        self.restart()
    }
}

const RATE: u64 = 16000;
/// a segment whose text nears the schema's 4000 characters is closed by force
pub const MAX_SEGMENT_CHARS: usize = 3000;
/// At most this much audio is fed again to a new stream. An endpoint comes ~0.8 s (the
/// endpointing silence) plus the look-ahead after the last word, so this is never reached in
/// speech; it bounds the replay after a long stretch in which the model said nothing.
pub const MAX_REPLAY_SAMPLES: usize = 10 * RATE as usize;

pub struct Session<R: Recognize> {
    dec: R,
    /// samples pushed so far (the audio clock)
    fed: u64,
    /// the sample at which the current library stream began
    base: u64,
    /// the last pushed samples (at most 2 * MAX_REPLAY_SAMPLES), ending at `fed`
    tail: Vec<f32>,
    segment: i64,
    revision: i64,
    /// where the current segment begins as far as is known: the end of the previous final's last word
    seg_start_ms: i64,
    last_text: String,
    /// events not yet written out
    pub out: Vec<Value>,
}

fn ms(samples: u64) -> i64 {
    (samples * 1000 / RATE) as i64
}

impl<R: Recognize> Session<R> {
    pub fn new(dec: R) -> Self {
        Session { dec, fed: 0, base: 0, tail: Vec::new(), segment: 0, revision: 0, seg_start_ms: 0, last_text: String::new(), out: Vec::new() }
    }

    pub fn fed_ms(&self) -> i64 {
        ms(self.fed)
    }

    pub fn segments(&self) -> i64 {
        self.segment
    }

    fn emit(&mut self, is_final: bool, text: &str, words: &[Word]) {
        self.out.push(events::event(&events::EventIn { segment: self.segment, revision: self.revision, is_final, start_ms: self.seg_start_ms, text, words, fed_ms: self.fed_ms() }));
        self.revision += 1;
    }

    /// One library result. Returns the sample the next stream should start at when this result
    /// ended an utterance with words.
    fn handle(&mut self, mut h: Hypothesis) -> Option<u64> {
        if !h.is_final {
            if h.text != self.last_text {
                self.last_text = h.text.clone();
                self.emit(false, &h.text, &[]);
            }
            return None;
        }
        let base_ms = ms(self.base);
        for w in &mut h.words {
            w.start_ms += base_ms;
            w.end_ms += base_ms;
        }
        if !h.text.trim().is_empty() {
            self.emit(true, &h.text, &h.words);
            self.segment += 1;
            self.revision = 0;
        }
        self.last_text.clear();
        let end_ms = h.words.iter().map(|w| w.end_ms).max()?;
        self.seg_start_ms = end_ms.max(self.seg_start_ms);
        // the next stream starts where this utterance's last word ended (never before this
        // stream's start, never after what was pushed)
        let start = ((end_ms.max(0) as u64) * RATE / 1000).clamp(self.base, self.fed);
        // a new stream must start later than this one, or the same audio would be decoded again
        (start > self.base).then_some(start)
    }

    /// Close the library stream, open a new one at `start`, and feed it the audio since then.
    fn restart_at(&mut self, start: u64) -> Result<(), String> {
        let start = start.max(self.fed.saturating_sub(MAX_REPLAY_SAMPLES as u64)).max(self.fed - self.tail.len() as u64);
        self.dec.restart()?;
        self.base = start;
        let from = self.tail.len() - (self.fed - start) as usize;
        if from < self.tail.len() {
            self.dec.push(&self.tail[from..])?;
        }
        Ok(())
    }

    /// Everything the model can say about the audio pushed so far. True when a new stream was
    /// started on the way.
    fn drain(&mut self) -> Result<bool, String> {
        let mut restarted = false;
        while let Some(h) = self.dec.next()? {
            if let Some(start) = self.handle(h) {
                self.restart_at(start)?;
                restarted = true;
            }
        }
        Ok(restarted)
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        self.dec.push(samples)?;
        self.fed += samples.len() as u64;
        self.tail.extend_from_slice(samples);
        if self.tail.len() > 2 * MAX_REPLAY_SAMPLES {
            self.tail.drain(..self.tail.len() - MAX_REPLAY_SAMPLES);
        }
        self.drain()?;
        if self.last_text.chars().count() > MAX_SEGMENT_CHARS {
            self.dec.force_endpoint()?;
            self.drain()?;
        }
        Ok(())
    }

    /// End of audio: flush what the model still holds as a last final. When that final starts a
    /// new stream (audio after its last word), the new stream is finished too.
    pub fn finish(&mut self) -> Result<(), String> {
        loop {
            self.dec.finish()?;
            let before = self.base;
            if !self.drain()? || self.base <= before || self.base >= self.fed {
                break;
            }
        }
        if !self.last_text.trim().is_empty() {
            // the library said nothing final about the tail: keep the text, without words
            let h = Hypothesis { is_final: true, text: std::mem::take(&mut self.last_text), words: vec![] };
            self.handle(h);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted recognizer: each stream reports what it was fed; results are queued by the test
    /// per stream (index = how many restarts happened).
    #[derive(Default)]
    struct Fake {
        streams: Vec<Vec<f32>>,
        script: Vec<VecDeque<(usize, Hypothesis)>>,
        finished: Vec<bool>,
    }

    impl Fake {
        fn cur(&self) -> usize {
            self.streams.len() - 1
        }
    }

    impl Recognize for Fake {
        fn push(&mut self, s: &[f32]) -> Result<(), String> {
            let c = self.cur();
            self.streams[c].extend_from_slice(s);
            Ok(())
        }
        /// a scripted result comes out once its stream holds the given number of samples
        fn next(&mut self) -> Result<Option<Hypothesis>, String> {
            let c = self.cur();
            let have = self.streams[c].len();
            let fin = self.finished[c];
            match self.script.get_mut(c).and_then(|q| q.front()) {
                Some((at, _)) if *at <= have || fin => Ok(self.script[c].pop_front().map(|x| x.1)),
                _ => Ok(None),
            }
        }
        fn force_endpoint(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn finish(&mut self) -> Result<(), String> {
            let c = self.cur();
            self.finished[c] = true;
            Ok(())
        }
        fn restart(&mut self) -> Result<(), String> {
            self.streams.push(Vec::new());
            self.finished.push(false);
            Ok(())
        }
    }

    fn fake(script: Vec<Vec<(usize, Hypothesis)>>) -> Fake {
        Fake { streams: vec![Vec::new()], finished: vec![false], script: script.into_iter().map(VecDeque::from).collect() }
    }

    fn fin(text: &str, words: &[(&str, i64, i64)]) -> Hypothesis {
        Hypothesis { is_final: true, text: text.into(), words: words.iter().map(|(t, s, e)| Word { text: (*t).into(), start_ms: *s, end_ms: *e }).collect() }
    }

    fn part(text: &str) -> Hypothesis {
        Hypothesis { is_final: false, text: text.into(), words: vec![] }
    }

    /// samples whose value is their index, so a replay can be checked by content
    fn ramp(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|i| i as f32).collect()
    }

    fn push_all(s: &mut Session<Fake>, total: usize) {
        for k in (0..total).step_by(1600) {
            s.push(&ramp(k, 1600.min(total - k))).unwrap();
        }
    }

    #[test]
    fn an_utterance_end_starts_a_new_stream_fed_from_the_last_word() {
        // stream 0: a final at 3.0 s whose last word ends at 1.5 s; stream 1: a final at 2 s of its own audio
        let mut s = Session::new(fake(vec![
            vec![(16000, part("안녕")), (48000, fin("안녕 하세요", &[("안녕", 500, 900), ("하세요", 1000, 1500)]))],
            vec![(32000, fin("next", &[("next", 600, 1100)]))],
        ]));
        push_all(&mut s, 80000);
        s.finish().unwrap();
        let d = &s.dec;
        assert_eq!(d.streams.len(), 3, "each final with words opens a new stream");
        // the second stream got the audio from 1.5 s (sample 24000) on: the replay, then the rest
        assert_eq!(d.streams[1][0], 24000.0);
        assert_eq!(d.streams[1].len(), 32000, "until its final");
        assert!(d.streams.iter().all(|s| s.windows(2).all(|w| w[1] == w[0] + 1.0)), "no audio is skipped or repeated within a stream");
        // the third begins where the second's word ended: 1500 + 1100 ms
        assert_eq!(d.streams[2][0], 41600.0);
        assert_eq!(*d.streams[2].last().unwrap(), 79999.0);
        let finals: Vec<&Value> = s.out.iter().filter(|e| e["final"] == true).collect();
        assert_eq!(finals.len(), 2);
        // times of the second stream are shifted by where it began
        assert_eq!(finals[1]["units"][0]["start_ms"], 1500 + 600);
        assert_eq!(finals[1]["units"][0]["end_ms"], 1500 + 1100);
        assert_eq!(finals[1]["segment"], 1);
        assert_eq!(finals[0]["units"][1]["end_ms"], 1500, "the first stream's times are as given");
    }

    #[test]
    fn partials_after_a_restart_begin_where_the_previous_final_ended() {
        let mut s = Session::new(fake(vec![vec![(16000, fin("a", &[("a", 100, 700)]))], vec![(8000, part("b"))]]));
        push_all(&mut s, 32000);
        let p = s.out.iter().find(|e| e["final"] == false).unwrap();
        assert_eq!((p["segment"].as_i64(), p["start_ms"].as_i64(), p["text"].as_str()), (Some(1), Some(700), Some("b")));
    }

    #[test]
    fn a_final_without_words_keeps_the_stream() {
        let mut s = Session::new(fake(vec![vec![(16000, fin("", &[])), (32000, fin("x", &[("x", 1200, 1900)]))]]));
        push_all(&mut s, 30000);
        assert_eq!(s.dec.streams.len(), 1, "nothing was said: no reason to start over");
        push_all(&mut s, 2000);
        assert_eq!(s.dec.streams.len(), 2);
    }

    #[test]
    fn the_tail_after_the_last_final_is_decoded_again_at_finish() {
        // the library ends the stream at finish with a final that covers 1 s; what follows is
        // decoded by a new stream, which is finished too
        let mut s = Session::new(fake(vec![vec![(usize::MAX, fin("one", &[("one", 200, 1000)]))], vec![(usize::MAX, fin("two", &[("two", 300, 800)]))]]));
        push_all(&mut s, 48000);
        assert!(s.out.is_empty());
        s.finish().unwrap();
        let texts: Vec<&str> = s.out.iter().map(|e| e["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["one", "two"]);
        assert_eq!(s.dec.streams[1].len(), 48000 - 16000);
        assert_eq!(s.out[1]["units"][0]["start_ms"], 1300);
        assert_eq!(s.segments(), 2);
    }

    #[test]
    fn finish_ends_when_a_new_stream_would_get_nothing() {
        // the final's last word ends at the very end of the audio: no new stream is finished
        let mut s = Session::new(fake(vec![vec![(usize::MAX, fin("end", &[("end", 100, 2000)]))], vec![]]));
        push_all(&mut s, 32000);
        s.finish().unwrap();
        assert_eq!(s.out.len(), 1);
        assert!(s.dec.finished.iter().filter(|f| **f).count() <= 1);
    }

    #[test]
    fn the_replay_is_bounded() {
        // a final arrives 30 s after its only word: at most MAX_REPLAY_SAMPLES are fed again
        let mut s = Session::new(fake(vec![vec![(16000 * 31, fin("w", &[("w", 0, 500)]))], vec![]]));
        push_all(&mut s, 16000 * 31);
        assert_eq!(s.dec.streams[1].len(), MAX_REPLAY_SAMPLES);
        assert_eq!(s.dec.streams[1][0], (16000 * 31 - MAX_REPLAY_SAMPLES) as f32);
        assert!(s.tail.len() <= 2 * MAX_REPLAY_SAMPLES);
    }

    #[test]
    fn a_word_time_before_the_stream_start_never_moves_the_start_back() {
        let mut s = Session::new(fake(vec![vec![(16000, fin("a", &[("a", 100, 900)]))], vec![(16000, fin("b", &[("b", -50, -10)]))], vec![]]));
        push_all(&mut s, 48000);
        assert_eq!(s.base, 14400, "base stays at the second stream's start (900 ms)");
        assert_eq!(s.dec.streams.len(), 2, "no new stream at the same start");
    }
}
