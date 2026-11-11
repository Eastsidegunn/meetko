//! The part of the CrispASR C API (MIT, v0.8.53) this engine uses, declared by hand from
//! `crispasr_session.h` / `crispasr.h` of the pinned release (fetch-libs.sh keeps both in vendor/).
//! Every type is opaque and every call passes plain pointers and integers, so no struct layout has to
//! match; a different library version still needs these signatures checked again.

use std::ffi::{c_char, c_int, CStr, CString};

#[repr(C)]
pub struct SessionOpaque([u8; 0]);
#[repr(C)]
pub struct ResultOpaque([u8; 0]);
#[repr(C)]
pub struct AlignOpaque([u8; 0]);

extern "C" {
    fn crispasr_c_api_version() -> *const c_char;
    fn crispasr_set_gpu_backend(name: *const c_char);
    fn crispasr_session_open_explicit(model_path: *const c_char, backend_name: *const c_char, n_threads: c_int) -> *mut SessionOpaque;
    fn crispasr_session_close(s: *mut SessionOpaque);
    fn crispasr_session_set_autochunk(s: *mut SessionOpaque, enabled: c_int, chunk_seconds: c_int) -> c_int;
    fn crispasr_session_set_temperature(s: *mut SessionOpaque, temperature: f32, seed: u64) -> c_int;
    fn crispasr_session_set_max_new_tokens(s: *mut SessionOpaque, n: c_int) -> c_int;
    fn crispasr_session_transcribe_lang(s: *mut SessionOpaque, pcm: *const f32, n_samples: c_int, language: *const c_char) -> *mut ResultOpaque;
    fn crispasr_session_result_n_segments(r: *mut ResultOpaque) -> c_int;
    fn crispasr_session_result_segment_text(r: *mut ResultOpaque, i: c_int) -> *const c_char;
    fn crispasr_session_result_n_tokens(r: *mut ResultOpaque, i_seg: c_int) -> c_int;
    fn crispasr_session_result_free(r: *mut ResultOpaque);
    fn crispasr_align_words_abi(aligner_model: *const c_char, transcript: *const c_char, samples: *const f32, n_samples: i32, t_offset_cs: i64, n_threads: i32) -> *mut AlignOpaque;
    fn crispasr_align_result_n_words(r: *mut AlignOpaque) -> c_int;
    fn crispasr_align_result_word_text(r: *mut AlignOpaque, i: c_int) -> *const c_char;
    fn crispasr_align_result_word_t0(r: *mut AlignOpaque, i: c_int) -> i64;
    fn crispasr_align_result_word_t1(r: *mut AlignOpaque, i: c_int) -> i64;
    fn crispasr_align_result_free(r: *mut AlignOpaque);
    fn crispasr_free_aligner_cache();
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the library returns NUL-terminated UTF-8 (U+FFFD for bad bytes), valid until its result is freed
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "a string for the library contains NUL".to_string())
}

/// The library's own version (`crispasr_c_api_version`).
pub fn version() -> String {
    // SAFETY: a static string
    cstr(unsafe { crispasr_c_api_version() })
}

/// Pick the GPU backend before any session opens ("metal" on macOS).
pub fn set_gpu_backend(name: &str) {
    if let Ok(n) = c(name) {
        // SAFETY: called before any session exists; the library copies the name
        unsafe { crispasr_set_gpu_backend(n.as_ptr()) }
    }
}

/// A loaded ASR model.
pub struct Session(*mut SessionOpaque);

impl Session {
    pub fn open(model: &str, backend: &str, threads: i32) -> Result<Session, String> {
        let (m, b) = (c(model)?, c(backend)?);
        // SAFETY: both strings live through the call
        let s = unsafe { crispasr_session_open_explicit(m.as_ptr(), b.as_ptr(), threads) };
        if s.is_null() {
            return Err("the library could not load the ASR model".into());
        }
        // the engine cuts the audio itself (see `chunks`): no second chunking inside the library
        // SAFETY: s is live
        unsafe { crispasr_session_set_autochunk(s, 0, 0) };
        Ok(Session(s))
    }

    /// Greedy decoding: temperature 0 (the seed is then unused). Returns the library's code (-2: no-op).
    pub fn set_temperature(&mut self, t: f32, seed: u64) -> i32 {
        // SAFETY: the session is live
        unsafe { crispasr_session_set_temperature(self.0, t, seed) }
    }

    /// The text of `pcm` (16 kHz mono), its segments joined by a space, and how many tokens the library
    /// reports for it (`None` when it reports none for this backend). `language` empty = the model detects it.
    pub fn transcribe(&mut self, pcm: &[f32], language: &str, max_new_tokens: i32) -> Result<(String, Option<usize>), String> {
        let lang = c(language)?;
        let n = c_int::try_from(pcm.len()).map_err(|_| "the audio piece is too long".to_string())?;
        // SAFETY: the session is live, pcm is valid for n samples, lang lives through the call
        let r = unsafe {
            crispasr_session_set_max_new_tokens(self.0, max_new_tokens);
            crispasr_session_transcribe_lang(self.0, pcm.as_ptr(), n, if language.is_empty() { std::ptr::null() } else { lang.as_ptr() })
        };
        if r.is_null() {
            return Err("the library could not transcribe a piece of the audio".into());
        }
        // SAFETY: r is a live result, freed below
        let (text, tokens) = unsafe {
            let n = crispasr_session_result_n_segments(r);
            let parts: Vec<String> = (0..n).map(|i| cstr(crispasr_session_result_segment_text(r, i))).collect();
            let tokens: usize = (0..n).map(|i| crispasr_session_result_n_tokens(r, i).max(0) as usize).sum();
            crispasr_session_result_free(r);
            (parts, tokens)
        };
        Ok((text.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" "), (tokens > 0).then_some(tokens)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: owned and closed once
        unsafe { crispasr_session_close(self.0) }
    }
}

/// One aligned word: text and its times in milliseconds of the whole recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Aligned {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Word times for `transcript` spoken in `pcm` (16 kHz mono, at most a few minutes), shifted by
/// `offset_ms`. The aligner model is loaded on the first call and kept by the library (process-wide).
pub fn align(aligner_model: &str, transcript: &str, pcm: &[f32], offset_ms: i64, threads: i32) -> Result<Vec<Aligned>, String> {
    let (m, t) = (c(aligner_model)?, c(transcript)?);
    let n = i32::try_from(pcm.len()).map_err(|_| "the audio piece is too long".to_string())?;
    // SAFETY: the strings and pcm live through the call; the library times are centiseconds
    let r = unsafe { crispasr_align_words_abi(m.as_ptr(), t.as_ptr(), pcm.as_ptr(), n, offset_ms / 10, threads) };
    if r.is_null() {
        return Err("the aligner could not place the words".into());
    }
    // SAFETY: r is a live result, freed below
    let words = unsafe {
        let n = crispasr_align_result_n_words(r);
        let w = (0..n)
            .map(|i| Aligned { text: cstr(crispasr_align_result_word_text(r, i)), start_ms: crispasr_align_result_word_t0(r, i) * 10, end_ms: crispasr_align_result_word_t1(r, i) * 10 })
            .collect();
        crispasr_align_result_free(r);
        w
    };
    Ok(words)
}

/// Unload the aligner (before the process ends: the ggml backends' static destructors must not run
/// while a model is loaded).
pub fn free_aligner() {
    // SAFETY: safe to repeat, per the header
    unsafe { crispasr_free_aligner_cache() }
}
