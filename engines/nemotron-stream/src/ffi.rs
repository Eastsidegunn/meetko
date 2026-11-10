//! The part of the NeMo-Speech.cpp C API (Apache-2.0, v0.2.0) that a streaming recognizer needs,
//! declared by hand: the structs are copied field for field from `asr.h` of the pinned release
//! (fetch-libs.sh keeps a copy in vendor/asr.h), so a different library version needs this file
//! checked again. bindgen was not used because it would pull libclang into every build for ~15
//! declarations. Config structs are size-prefixed and append-only: zeroed fields mean "default".

use std::ffi::{c_char, c_float, CStr, CString};

#[repr(C)]
struct BackendConfig {
    size: usize,
    gpu: i32, // -1 = CPU
}
#[repr(C)]
struct ModelConfig {
    size: usize,
    path: *const c_char,
    name: *const c_char,
}
#[repr(C)]
struct StreamingConfig {
    size: usize,
    chunk_size: c_float,
    ctc_left_padding: c_float,
    ctc_right_padding: c_float,
    rnnt_right_context: i32,
}
#[repr(C)]
struct EndpointingConfig {
    size: usize,
    enable: bool,
    vad_based: bool,
    stop_history_eou_ms: i32,
}
#[repr(C)]
struct RecognizerConfig {
    size: usize,
    backend: *const BackendConfig,
    model: *const ModelConfig,
    streaming: *const StreamingConfig,
    decoder: *const u8,
    vad: *const u8,
    endpointing: *const EndpointingConfig,
    postproc: *const u8,
    diar: *const u8,
    batching: *const u8,
}
#[repr(C)]
struct Options {
    size: usize,
    request_id: *const c_char,
    language_code: *const c_char,
    interim_results: bool,
    enable_word_time_offsets: bool,
    enable_automatic_punctuation: bool,
    verbatim_transcripts: bool,
    profanity_filter: bool,
    stop_history_eou_ms: i32,
    speech_contexts: *const u8,
    speech_context_count: usize,
    max_alternatives: i32,
    enable_speaker_diarization: bool,
    max_speaker_count: i32,
}

#[repr(C)]
struct RecognizerOpaque([u8; 0]);
#[repr(C)]
struct StreamOpaque([u8; 0]);
#[repr(C)]
struct ResultOpaque([u8; 0]);

const OK: i32 = 0;

extern "C" {
    fn nemo_speech_asr_version() -> *const c_char;
    fn nemo_speech_asr_last_error() -> *const c_char;
    fn nemo_speech_asr_recognition_options_default() -> Options;
    fn nemo_speech_asr_create(cfg: *const RecognizerConfig, out: *mut *mut RecognizerOpaque) -> i32;
    fn nemo_speech_asr_destroy(r: *mut RecognizerOpaque);
    fn nemo_speech_asr_streaming_recognize(r: *mut RecognizerOpaque, o: *const Options, out: *mut *mut StreamOpaque) -> i32;
    fn nemo_speech_asr_stream_push_f32(s: *mut StreamOpaque, samples: *const c_float, n: usize, sample_rate: i32) -> i32;
    fn nemo_speech_asr_stream_force_endpoint(s: *mut StreamOpaque) -> i32;
    fn nemo_speech_asr_stream_finish(s: *mut StreamOpaque) -> i32;
    fn nemo_speech_asr_stream_next(s: *mut StreamOpaque, out: *mut *mut ResultOpaque) -> i32;
    fn nemo_speech_asr_stream_close(s: *mut StreamOpaque);
    fn nemo_speech_asr_result_is_final(r: *const ResultOpaque) -> bool;
    fn nemo_speech_asr_result_transcript(r: *const ResultOpaque, alt: usize) -> *const c_char;
    fn nemo_speech_asr_result_word_count(r: *const ResultOpaque, alt: usize) -> usize;
    fn nemo_speech_asr_result_word_text(r: *const ResultOpaque, alt: usize, i: usize) -> *const c_char;
    fn nemo_speech_asr_result_word_start_time(r: *const ResultOpaque, alt: usize, i: usize) -> i32;
    fn nemo_speech_asr_result_word_end_time(r: *const ResultOpaque, alt: usize, i: usize) -> i32;
    fn nemo_speech_asr_result_destroy(r: *mut ResultOpaque);
}

pub fn version() -> String {
    // SAFETY: a static NUL-terminated string owned by the library
    unsafe { CStr::from_ptr(nemo_speech_asr_version()) }.to_string_lossy().into_owned()
}

pub(crate) fn last_error(what: &str) -> String {
    // SAFETY: thread-local NUL-terminated message, valid until the next call on this thread
    let m = unsafe { CStr::from_ptr(nemo_speech_asr_last_error()) }.to_string_lossy().into_owned();
    format!("{what}: {m}")
}

pub(crate) fn check(status: i32, what: &str) -> Result<(), String> {
    if status == OK {
        Ok(())
    } else {
        Err(last_error(what))
    }
}

/// Settings that shape the stream; echoed to the core as `params`.
pub struct Settings {
    /// -1 = CPU
    pub gpu: i32,
    pub chunk_s: f32,
    /// encoder frames of look-ahead (80 ms each); -1 = the model's default
    pub right_context: i32,
    /// trailing silence that ends an utterance
    pub eou_ms: i32,
}

/// One word of a final result; times are milliseconds from the start of the stream.
pub struct Word {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// One result pulled from the stream. Partials carry the transcript so far and no words.
pub struct Hypothesis {
    pub is_final: bool,
    pub text: String,
    pub words: Vec<Word>,
}

/// A recognizer and its one stream. The stream is closed before the recognizer is destroyed.
pub struct Decoder {
    rec: *mut RecognizerOpaque,
    stream: *mut StreamOpaque,
}

impl Decoder {
    pub fn new(model: &CStr, language: &CStr, s: &Settings) -> Result<Decoder, String> {
        let backend = BackendConfig { size: std::mem::size_of::<BackendConfig>(), gpu: s.gpu };
        let m = ModelConfig { size: std::mem::size_of::<ModelConfig>(), path: model.as_ptr(), name: std::ptr::null() };
        let streaming = StreamingConfig {
            size: std::mem::size_of::<StreamingConfig>(),
            chunk_size: s.chunk_s,
            ctc_left_padding: 1.92,
            ctc_right_padding: 1.92,
            rnnt_right_context: s.right_context,
        };
        let ep = EndpointingConfig { size: std::mem::size_of::<EndpointingConfig>(), enable: true, vad_based: false, stop_history_eou_ms: s.eou_ms };
        let cfg = RecognizerConfig {
            size: std::mem::size_of::<RecognizerConfig>(),
            backend: &backend,
            model: &m,
            streaming: &streaming,
            decoder: std::ptr::null(),
            vad: std::ptr::null(),
            endpointing: &ep,
            postproc: std::ptr::null(),
            diar: std::ptr::null(),
            batching: std::ptr::null(),
        };
        let mut rec = std::ptr::null_mut();
        // SAFETY: cfg and what it points to live through the call; the library copies the config
        check(unsafe { nemo_speech_asr_create(&cfg, &mut rec) }, "cannot load the model")?;
        let mut o = unsafe { nemo_speech_asr_recognition_options_default() };
        o.language_code = language.as_ptr();
        o.interim_results = true;
        o.enable_word_time_offsets = true;
        o.enable_automatic_punctuation = true; // the model punctuates itself; without this the final text loses what the partials and words keep
        o.stop_history_eou_ms = s.eou_ms;
        let mut stream = std::ptr::null_mut();
        // SAFETY: rec is the recognizer just created; o is valid for the call
        if let Err(e) = check(unsafe { nemo_speech_asr_streaming_recognize(rec, &o, &mut stream) }, "cannot open the stream") {
            unsafe { nemo_speech_asr_destroy(rec) };
            return Err(e);
        }
        Ok(Decoder { rec, stream })
    }

    /// 16 kHz mono; buffers only, `next` decodes.
    pub fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        // SAFETY: the stream is open and samples is valid for its length
        check(unsafe { nemo_speech_asr_stream_push_f32(self.stream, samples.as_ptr(), samples.len(), 16000) }, "push failed")
    }

    /// End the current utterance at the next `next`.
    pub fn force_endpoint(&mut self) -> Result<(), String> {
        check(unsafe { nemo_speech_asr_stream_force_endpoint(self.stream) }, "force_endpoint failed")
    }

    /// No more audio: the tail and the last final come out of `next`.
    pub fn finish(&mut self) -> Result<(), String> {
        check(unsafe { nemo_speech_asr_stream_finish(self.stream) }, "finish failed")
    }

    /// Decode what was pushed and return the next result, `None` when more audio is needed.
    #[allow(clippy::should_implement_trait)] // fallible, and not an iterator: it decodes
    pub fn next(&mut self) -> Result<Option<Hypothesis>, String> {
        let mut r = std::ptr::null_mut();
        // SAFETY: the stream is open; a non-null r is owned here and destroyed below
        check(unsafe { nemo_speech_asr_stream_next(self.stream, &mut r) }, "decode failed")?;
        if r.is_null() {
            return Ok(None);
        }
        let s = |p: *const c_char| if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
        // SAFETY: r is a live result; strings are copied before it is destroyed
        let h = unsafe {
            let n = nemo_speech_asr_result_word_count(r, 0);
            Hypothesis {
                is_final: nemo_speech_asr_result_is_final(r),
                text: s(nemo_speech_asr_result_transcript(r, 0)),
                words: (0..n)
                    .map(|i| Word {
                        text: s(nemo_speech_asr_result_word_text(r, 0, i)),
                        start_ms: i64::from(nemo_speech_asr_result_word_start_time(r, 0, i)),
                        end_ms: i64::from(nemo_speech_asr_result_word_end_time(r, 0, i)),
                    })
                    .collect(),
            }
        };
        unsafe { nemo_speech_asr_result_destroy(r) };
        Ok(Some(h))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: both handles are live and owned here; the stream goes first
        unsafe {
            nemo_speech_asr_stream_close(self.stream);
            nemo_speech_asr_destroy(self.rec);
        }
    }
}

/// A language code the library accepts, from what the core or the environment says: nothing or
/// `auto` lets the model detect the language (the default: it copes best with Korean that carries
/// English terms), `ko` and `en` get their usual locale, `xx-YY` passes through.
pub fn language_code(requested: &str) -> Result<CString, String> {
    let r = requested.trim();
    let code = match r {
        "" | "auto" => "auto",
        "ko" => "ko-KR",
        "en" => "en-US",
        other if other.len() <= 16 && other.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => other,
        _ => return Err("language must be a code like ko-KR, en-US or auto".into()),
    };
    Ok(CString::new(code).expect("no NUL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library reports the sizeof it was built with in every size-prefixed struct: ours must agree.
    #[test]
    fn option_layout_matches_the_library() {
        let o = unsafe { nemo_speech_asr_recognition_options_default() };
        assert_eq!(o.size, std::mem::size_of::<Options>());
        assert!(o.language_code.is_null() || unsafe { *o.language_code } == 0);
    }

    #[test]
    fn the_library_is_the_pinned_version() {
        assert!(version().contains("0.2.0"), "{}", version());
    }

    #[test]
    fn language_codes() {
        let c = |s: &str| language_code(s).map(|c| c.into_string().unwrap());
        assert_eq!(c("ko").unwrap(), "ko-KR");
        assert_eq!(c("").unwrap(), "auto");
        assert_eq!(c("auto").unwrap(), "auto");
        assert_eq!(c("en-GB").unwrap(), "en-GB");
        assert!(c("ko KR; rm").is_err());
    }
}
