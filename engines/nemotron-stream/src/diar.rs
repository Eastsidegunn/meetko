//! The part of the NeMo-Speech.cpp C API (Apache-2.0, v0.2.0) that standalone speaker diarization
//! needs, declared by hand like `ffi.rs`: the structs are copied field for field from `diar.h` of the
//! pinned release (fetch-libs.sh keeps a copy in vendor/diar.h), so a different library version needs
//! this file checked again. Plus the two things the batch engine does around it: read a WAV, and turn
//! segments into contract diarization turns.

use std::ffi::{c_char, CStr};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value};

use crate::ffi::check;

#[repr(C)]
struct ModelConfig {
    size: usize,
    model_path: *const c_char,
    gpu: i32, // -1 = CPU
    preset: *const c_char,
    chunk_frames: i32,
    right_context_frames: i32,
    left_context_frames: i32,
    fifo_frames: i32,
    spkcache_frames: i32,
    update_period_frames: i32,
}

/// `nemo_speech_diar_segment`: seconds, speaker 1-based.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start_time: f64,
    pub end_time: f64,
    pub speaker: i32,
}

#[repr(C)]
struct ModelOpaque([u8; 0]);
#[repr(C)]
struct JobOpaque([u8; 0]);

extern "C" {
    fn nemo_speech_diar_create(cfg: *const ModelConfig, out: *mut *mut ModelOpaque) -> i32;
    fn nemo_speech_diar_destroy(m: *mut ModelOpaque);
    fn nemo_speech_diar_num_speakers(m: *const ModelOpaque) -> i32;
    fn nemo_speech_diar_stream_open(m: *mut ModelOpaque, out: *mut *mut JobOpaque) -> i32;
    fn nemo_speech_diar_stream_push_f32(s: *mut JobOpaque, samples: *const f32, n: usize, sample_rate: i32) -> i32;
    fn nemo_speech_diar_stream_finish(s: *mut JobOpaque) -> i32;
    fn nemo_speech_diar_stream_close(s: *mut JobOpaque);
    fn nemo_speech_diar_segments(s: *const JobOpaque, cfg: *const u8, out: *mut Segment, capacity: usize, count: *mut usize) -> i32;
}

/// Audio is pushed the way a live source would, in slices of this length (what the library's own
/// example and the benchmark that chose the model did).
pub const PUSH_MS: usize = 160;

/// A loaded diarization model. Each `run` is one stream over one whole recording; the model is kept
/// for the next one.
pub struct Diarizer {
    model: *mut ModelOpaque,
}

impl Diarizer {
    /// `gpu`: a device index, -1 = CPU. The geometry is the model's own low-latency default (the
    /// streaming pass: the offline full-attention path is limited to a few minutes of audio).
    pub fn new(model: &CStr, gpu: i32) -> Result<Diarizer, String> {
        let cfg = ModelConfig {
            size: std::mem::size_of::<ModelConfig>(),
            model_path: model.as_ptr(),
            gpu,
            preset: std::ptr::null(),
            chunk_frames: 0,
            right_context_frames: 0,
            left_context_frames: -1, // < 0 keeps the preset's
            fifo_frames: 0,
            spkcache_frames: 0,
            update_period_frames: 0,
        };
        let mut m = std::ptr::null_mut();
        // SAFETY: cfg and the path live through the call; the library copies what it needs
        check(unsafe { nemo_speech_diar_create(&cfg, &mut m) }, "cannot load the model")?;
        Ok(Diarizer { model: m })
    }

    /// How many speakers the model can tell apart.
    pub fn capacity(&self) -> i32 {
        // SAFETY: the model is live
        unsafe { nemo_speech_diar_num_speakers(self.model) }
    }

    /// Who spoke when in `pcm`, segments in seconds sorted by start, with the library's own
    /// segmentation defaults. The audio is read from the file as it is pushed: memory does not grow
    /// with the length of the recording.
    pub fn run<R: Read>(&mut self, pcm: &mut Pcm<R>) -> Result<Vec<Segment>, String> {
        let mut job = std::ptr::null_mut();
        // SAFETY: the model is live; a non-null job is closed below on every path
        check(unsafe { nemo_speech_diar_stream_open(self.model, &mut job) }, "cannot open the stream")?;
        let r = (|| {
            let slice = (pcm.rate as usize * PUSH_MS / 1000).max(1);
            loop {
                let part = pcm.read(slice).map_err(|e| format!("cannot read the wav: {}", e.kind()))?;
                if part.is_empty() {
                    break;
                }
                // SAFETY: job is open and part is valid for its length
                check(unsafe { nemo_speech_diar_stream_push_f32(job, part.as_ptr(), part.len(), pcm.rate) }, "push failed")?;
            }
            check(unsafe { nemo_speech_diar_stream_finish(job) }, "finish failed")?;
            let mut count = 0usize;
            check(unsafe { nemo_speech_diar_segments(job, std::ptr::null(), std::ptr::null_mut(), 0, &mut count) }, "segment count failed")?;
            let mut segs = vec![Segment { start_time: 0.0, end_time: 0.0, speaker: 0 }; count];
            if count > 0 {
                check(unsafe { nemo_speech_diar_segments(job, std::ptr::null(), segs.as_mut_ptr(), count, &mut count) }, "segments failed")?;
                segs.truncate(count);
            }
            Ok(segs)
        })();
        // SAFETY: the job is open and owned here
        unsafe { nemo_speech_diar_stream_close(job) };
        r
    }
}

impl Drop for Diarizer {
    fn drop(&mut self) {
        // SAFETY: the model is live and owned here; its jobs are closed
        unsafe { nemo_speech_diar_destroy(self.model) }
    }
}

// ---------------------------------------------------------------- audio

/// Why a file cannot be diarized: the caller's input, not a fault of the engine.
pub struct Rejected(pub String);

/// The samples of a mono PCM16 WAV, read on demand.
pub struct Pcm<R> {
    r: R,
    pub rate: i32,
    /// samples left to read
    left: u64,
    total: u64,
}

impl<R: Read> Pcm<R> {
    /// How long the audio is.
    pub fn duration_ms(&self) -> i64 {
        (self.total * 1000 / self.rate as u64) as i64
    }

    /// Up to `max` samples; empty at the end.
    pub fn read(&mut self, max: usize) -> std::io::Result<Vec<f32>> {
        let n = (self.left).min(max as u64) as usize;
        let mut bytes = vec![0u8; n * 2];
        self.r.read_exact(&mut bytes)?;
        self.left -= n as u64;
        Ok(bytes.as_chunks::<2>().0.iter().map(|p| f32::from(i16::from_le_bytes(*p)) / 32768.0).collect())
    }
}

pub fn open_wav(path: &Path) -> Result<Pcm<std::io::BufReader<std::fs::File>>, Rejected> {
    let f = std::fs::File::open(path).map_err(|e| Rejected(format!("cannot read the wav: {}", e.kind())))?;
    open_pcm(std::io::BufReader::with_capacity(1 << 16, f))
}

/// Find the format and the data chunk. The data chunk is cut at the end of the file when its header
/// says more (a recording that was not closed properly).
pub fn open_pcm<R: Read + Seek>(mut r: R) -> Result<Pcm<R>, Rejected> {
    let bad = |m: &str| Rejected(m.to_string());
    let io = |_| Rejected("cannot read the wav".into());
    let end = r.seek(SeekFrom::End(0)).map_err(io)?;
    r.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut head = [0u8; 12];
    if r.read_exact(&mut head).is_err() || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }
    let mut fmt = None;
    let mut pos = 12u64;
    while pos + 8 <= end {
        let mut ch = [0u8; 8];
        r.read_exact(&mut ch).map_err(io)?;
        let len = u64::from(u32::from_le_bytes(ch[4..8].try_into().expect("4 bytes")));
        let body = pos + 8;
        if &ch[0..4] == b"fmt " {
            if !(16..=1 << 16).contains(&len) || body + 16 > end {
                return Err(bad("damaged fmt chunk"));
            }
            let mut f = vec![0u8; len as usize];
            r.read_exact(&mut f).map_err(io)?;
            let u16_at = |o: usize| u16::from_le_bytes([f[o], f[o + 1]]);
            fmt = Some((u16_at(0), u16_at(2), u32::from_le_bytes(f[4..8].try_into().expect("4 bytes")), u16_at(14)));
        } else if &ch[0..4] == b"data" {
            let Some((tag, channels, rate, bits)) = fmt else { return Err(bad("data before fmt")) };
            // 0xFFFE (extensible) is accepted as PCM16 when the sample size says so
            if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels != 1 {
                return Err(bad("the wav must be mono PCM16"));
            }
            if !(8000..=96000).contains(&rate) {
                return Err(bad("the sample rate must be between 8 and 96 kHz"));
            }
            let samples = len.min(end - body) / 2;
            return Ok(Pcm { r, rate: rate as i32, left: samples, total: samples });
        } else {
            r.seek(SeekFrom::Current(len as i64)).map_err(io)?;
        }
        pos = body.saturating_add(len).saturating_add(len & 1);
        r.seek(SeekFrom::Start(pos)).map_err(io)?;
    }
    Err(bad("no data chunk"))
}

// ---------------------------------------------------------------- contract

/// Contract diarization turns (integer ms, engine-local labels `spk<n>`), in start order. A segment
/// that rounds to nothing is dropped; times are clamped into the audio.
pub fn turns(segments: &[Segment], duration_ms: i64) -> Vec<Value> {
    let ms = |s: f64| ((s * 1000.0).round() as i64).clamp(0, duration_ms);
    let mut v: Vec<(i64, i64, i32)> = segments.iter().map(|s| (ms(s.start_time), ms(s.end_time), s.speaker)).filter(|(a, b, _)| b > a).collect();
    v.sort_by_key(|(a, b, s)| (*a, *b, *s));
    v.into_iter().map(|(a, b, s)| json!({ "start_ms": a, "end_ms": b, "speaker": format!("spk{s}") })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(channels: u16, rate: u32, bits: u16, samples: &[i16], header_len: Option<u32>) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend(16u32.to_le_bytes());
        for v in [1u16, channels] {
            b.extend(v.to_le_bytes());
        }
        b.extend(rate.to_le_bytes());
        b.extend((rate * u32::from(channels) * 2).to_le_bytes());
        b.extend((channels * 2).to_le_bytes());
        b.extend(bits.to_le_bytes());
        b.extend(b"data");
        b.extend(header_len.unwrap_or(data.len() as u32).to_le_bytes());
        b.extend(data);
        b
    }

    fn read_all(b: Vec<u8>) -> Result<(Vec<f32>, i32, i64), Rejected> {
        let mut p = open_pcm(std::io::Cursor::new(b))?;
        let (rate, ms) = (p.rate, p.duration_ms());
        let mut all = Vec::new();
        loop {
            let part = p.read(2).unwrap(); // tiny reads: the chunking must not matter
            if part.is_empty() {
                return Ok((all, rate, ms));
            }
            all.extend(part);
        }
    }

    #[test]
    fn a_mono_pcm16_wav_is_read_in_pieces() {
        let (s, rate, _) = read_all(wav(1, 16000, 16, &[0, 16384, -32768], None)).ok().unwrap();
        assert_eq!((rate, s), (16000, vec![0.0, 0.5, -1.0]));
        let (s, _, ms) = read_all(wav(1, 16000, 16, &[7; 1600], None)).ok().unwrap();
        assert_eq!((s.len(), ms), (1600, 100));
    }

    #[test]
    fn a_header_that_claims_more_than_the_file_holds_is_cut_at_the_file() {
        let (s, _, _) = read_all(wav(1, 16000, 16, &[1, 2, 3], Some(u32::MAX))).ok().unwrap();
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn chunks_before_the_data_are_skipped() {
        let mut b = wav(1, 16000, 16, &[1, 2], None);
        let data = b.split_off(36);
        b.extend(b"LIST\x03\0\0\0abc\0"); // odd length, padded
        b.extend(data);
        assert_eq!(read_all(b).ok().unwrap().0.len(), 2);
    }

    #[test]
    fn other_audio_is_rejected_with_a_reason() {
        let why = |b: Vec<u8>| read_all(b).err().unwrap().0;
        assert!(why(wav(2, 16000, 16, &[0, 0], None)).contains("mono PCM16"));
        assert!(why(wav(1, 16000, 8, &[0, 0], None)).contains("mono PCM16"));
        assert!(why(wav(1, 4000, 16, &[0], None)).contains("sample rate"));
        assert!(why(b"hello, this is not a wav".to_vec()).contains("not a WAV"));
        assert!(why(wav(1, 16000, 16, &[], None)[..36].to_vec()).contains("no data chunk"));
    }

    #[test]
    fn segments_become_ordered_integer_turns_inside_the_audio() {
        let seg = |a: f64, b: f64, s: i32| Segment { start_time: a, end_time: b, speaker: s };
        let t = turns(&[seg(5.0, 6.2504, 2), seg(0.0004, 3.0, 1), seg(7.0, 7.0004, 1), seg(9.5, 12.0, 1)], 10_000);
        assert_eq!(
            t,
            vec![
                json!({"start_ms": 0, "end_ms": 3000, "speaker": "spk1"}),
                json!({"start_ms": 5000, "end_ms": 6250, "speaker": "spk2"}),
                json!({"start_ms": 9500, "end_ms": 10000, "speaker": "spk1"}),
            ],
            "a turn that rounds to nothing is dropped, one past the end is clamped"
        );
    }
}
