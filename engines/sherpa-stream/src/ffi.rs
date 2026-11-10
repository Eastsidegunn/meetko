//! The part of the sherpa-onnx C API (Apache-2.0, v2.0.1) that an online transducer needs,
//! declared by hand: the structs are copied field for field from `c-api.h` of the pinned
//! release (fetch-libs.sh), so a different library version needs this file checked again.
//! bindgen was not used because it would pull libclang into every build for ~15 declarations.
//!
//! Zeroed fields mean "not set" in this API (NULL strings, 0 numbers).

#![allow(non_snake_case)]

use std::ffi::{c_char, c_float, c_int, CStr, CString};


#[repr(C)]
#[derive(Clone, Copy)]
struct Transducer {
    encoder: *const c_char,
    decoder: *const c_char,
    joiner: *const c_char,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Paraformer {
    encoder: *const c_char,
    decoder: *const c_char,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct OneModel {
    model: *const c_char,
}
#[repr(C)]
struct ModelConfig {
    transducer: Transducer,
    paraformer: Paraformer,
    zipformer2_ctc: OneModel,
    tokens: *const c_char,
    num_threads: c_int,
    provider: *const c_char,
    debug: c_int,
    model_type: *const c_char,
    modeling_unit: *const c_char,
    bpe_vocab: *const c_char,
    tokens_buf: *const c_char,
    tokens_buf_size: c_int,
    nemo_ctc: OneModel,
    t_one_ctc: OneModel,
}
#[repr(C)]
struct FeatureConfig {
    sample_rate: c_int,
    feature_dim: c_int,
}
#[repr(C)]
struct CtcFst {
    graph: *const c_char,
    max_active: c_int,
}
#[repr(C)]
struct Homophone {
    dict_dir: *const c_char,
    lexicon: *const c_char,
    rule_fsts: *const c_char,
}
#[repr(C)]
struct RecognizerConfig {
    feat_config: FeatureConfig,
    model_config: ModelConfig,
    decoding_method: *const c_char,
    max_active_paths: c_int,
    enable_endpoint: c_int,
    rule1_min_trailing_silence: c_float,
    rule2_min_trailing_silence: c_float,
    rule3_min_utterance_length: c_float,
    hotwords_file: *const c_char,
    hotwords_score: c_float,
    ctc_fst_decoder_config: CtcFst,
    rule_fsts: *const c_char,
    rule_fars: *const c_char,
    blank_penalty: c_float,
    hotwords_buf: *const c_char,
    hotwords_buf_size: c_int,
    hr: Homophone,
}
#[repr(C)]
struct RawResult {
    text: *const c_char,
    tokens: *const c_char,
    tokens_arr: *const *const c_char,
    timestamps: *mut c_float,
    count: c_int,
    json: *const c_char,
}

#[repr(C)]
struct RecognizerOpaque([u8; 0]);
#[repr(C)]
struct StreamOpaque([u8; 0]);

extern "C" {
    fn SherpaOnnxGetVersionStr() -> *const c_char;
    fn SherpaOnnxCreateOnlineRecognizer(c: *const RecognizerConfig) -> *const RecognizerOpaque;
    fn SherpaOnnxDestroyOnlineRecognizer(r: *const RecognizerOpaque);
    fn SherpaOnnxCreateOnlineStream(r: *const RecognizerOpaque) -> *const StreamOpaque;
    fn SherpaOnnxDestroyOnlineStream(s: *const StreamOpaque);
    fn SherpaOnnxOnlineStreamAcceptWaveform(s: *const StreamOpaque, sample_rate: c_int, samples: *const c_float, n: c_int);
    fn SherpaOnnxIsOnlineStreamReady(r: *const RecognizerOpaque, s: *const StreamOpaque) -> c_int;
    fn SherpaOnnxDecodeOnlineStream(r: *const RecognizerOpaque, s: *const StreamOpaque);
    fn SherpaOnnxGetOnlineStreamResult(r: *const RecognizerOpaque, s: *const StreamOpaque) -> *const RawResult;
    fn SherpaOnnxDestroyOnlineRecognizerResult(r: *const RawResult);
    fn SherpaOnnxOnlineStreamIsEndpoint(r: *const RecognizerOpaque, s: *const StreamOpaque) -> c_int;
    fn SherpaOnnxOnlineStreamReset(r: *const RecognizerOpaque, s: *const StreamOpaque);
    fn SherpaOnnxOnlineStreamInputFinished(s: *const StreamOpaque);
}

pub fn version() -> String {
    // SAFETY: a static NUL-terminated string owned by the library
    unsafe { CStr::from_ptr(SherpaOnnxGetVersionStr()) }.to_string_lossy().into_owned()
}

/// Endpoint rules in seconds (the values the Python experiment used; see the core's params).
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    pub rule1_min_trailing_silence: f32,
    pub rule2_min_trailing_silence: f32,
    pub rule3_min_utterance_length: f32,
}

pub struct ModelFiles {
    pub encoder: CString,
    pub decoder: CString,
    pub joiner: CString,
    pub tokens: CString,
}

/// One result snapshot, copied out of the library's buffers.
pub struct Snapshot {
    pub text: String,
    pub tokens: Vec<String>,
    pub timestamps: Vec<f64>,
    /// the segment's absolute start in seconds as the engine reports it (-1 when unknown)
    pub start_time: f64,
}

pub struct Recognizer {
    rec: *const RecognizerOpaque,
    stream: *const StreamOpaque,
}

impl Recognizer {
    pub fn new(files: &ModelFiles, ep: Endpoint, threads: i32) -> Result<Recognizer, String> {
        let greedy = CString::new("greedy_search").unwrap();
        let cpu = CString::new("cpu").unwrap();
        // SAFETY: all-zero is valid for this plain-data struct (NULL pointers, 0 numbers)
        let mut c: RecognizerConfig = unsafe { std::mem::zeroed() };
        c.feat_config = FeatureConfig { sample_rate: 16000, feature_dim: 80 };
        c.model_config.transducer = Transducer { encoder: files.encoder.as_ptr(), decoder: files.decoder.as_ptr(), joiner: files.joiner.as_ptr() };
        c.model_config.tokens = files.tokens.as_ptr();
        c.model_config.num_threads = threads;
        c.model_config.provider = cpu.as_ptr();
        c.decoding_method = greedy.as_ptr();
        c.max_active_paths = 4;
        c.enable_endpoint = 1;
        c.rule1_min_trailing_silence = ep.rule1_min_trailing_silence;
        c.rule2_min_trailing_silence = ep.rule2_min_trailing_silence;
        c.rule3_min_utterance_length = ep.rule3_min_utterance_length;
        // SAFETY: `c` and the strings it points to outlive the call; the library copies what it keeps
        let rec = unsafe { SherpaOnnxCreateOnlineRecognizer(&c) };
        if rec.is_null() {
            return Err("sherpa-onnx could not load the model".into());
        }
        // SAFETY: rec is a live recognizer
        let stream = unsafe { SherpaOnnxCreateOnlineStream(rec) };
        if stream.is_null() {
            unsafe { SherpaOnnxDestroyOnlineRecognizer(rec) };
            return Err("sherpa-onnx could not create a stream".into());
        }
        Ok(Recognizer { rec, stream })
    }

    pub fn accept(&mut self, samples: &[f32]) {
        // SAFETY: the slice is valid for `len` floats; the library copies them
        unsafe { SherpaOnnxOnlineStreamAcceptWaveform(self.stream, 16000, samples.as_ptr(), samples.len() as c_int) }
    }

    pub fn decode_ready(&mut self) {
        // SAFETY: live handles
        unsafe {
            while SherpaOnnxIsOnlineStreamReady(self.rec, self.stream) != 0 {
                SherpaOnnxDecodeOnlineStream(self.rec, self.stream);
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        // SAFETY: the result and everything it points to are valid until it is destroyed below;
        // `json` carries start_time, which the plain struct does not expose
        unsafe {
            let r = SherpaOnnxGetOnlineStreamResult(self.rec, self.stream);
            if r.is_null() {
                return Snapshot { text: String::new(), tokens: vec![], timestamps: vec![], start_time: -1.0 };
            }
            let text = if (*r).text.is_null() { String::new() } else { CStr::from_ptr((*r).text).to_string_lossy().into_owned() };
            let n = (*r).count.max(0) as usize;
            // a NULL array or entry means no tokens, never a dereference
            let tokens: Vec<String> = if (*r).tokens_arr.is_null() {
                vec![]
            } else {
                (0..n)
                    .map(|i| {
                        let p = *(*r).tokens_arr.add(i);
                        if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                    })
                    .collect()
            };
            let timestamps = if (*r).timestamps.is_null() || tokens.is_empty() { vec![] } else { (0..n).map(|i| f64::from(*(*r).timestamps.add(i))).collect() };
            let start_time = if (*r).json.is_null() {
                -1.0
            } else {
                serde_json::from_slice::<serde_json::Value>(CStr::from_ptr((*r).json).to_bytes())
                    .ok()
                    .and_then(|v| v["start_time"].as_f64())
                    .unwrap_or(-1.0)
            };
            SherpaOnnxDestroyOnlineRecognizerResult(r);
            Snapshot { text, tokens, timestamps, start_time }
        }
    }

    pub fn is_endpoint(&self) -> bool {
        // SAFETY: live handles
        unsafe { SherpaOnnxOnlineStreamIsEndpoint(self.rec, self.stream) != 0 }
    }

    pub fn reset(&mut self) {
        // SAFETY: live handles
        unsafe { SherpaOnnxOnlineStreamReset(self.rec, self.stream) }
    }

    pub fn input_finished(&mut self) {
        // SAFETY: live handle
        unsafe { SherpaOnnxOnlineStreamInputFinished(self.stream) }
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        // SAFETY: created in `new`, destroyed once, stream before its recognizer
        unsafe {
            SherpaOnnxDestroyOnlineStream(self.stream);
            SherpaOnnxDestroyOnlineRecognizer(self.rec);
        }
    }
}

#[cfg(test)]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The numbers come from compiling `sizeof`/`offsetof` against the pinned c-api.h (v2.0.1).
    #[test]
    fn structs_match_the_c_header() {
        assert_eq!(size_of::<RecognizerConfig>(), 272);
        assert_eq!(size_of::<ModelConfig>(), 136);
        assert_eq!(offset_of!(RecognizerConfig, rule1_min_trailing_silence), 160);
        assert_eq!(offset_of!(RecognizerConfig, hotwords_file), 176);
        assert_eq!(offset_of!(RecognizerConfig, blank_penalty), 224);
        assert_eq!(offset_of!(RecognizerConfig, hr), 248);
        assert_eq!(size_of::<RawResult>(), 48);
    }
}
