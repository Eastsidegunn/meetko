//! What the two engine binaries of this crate share: the NeMo-Speech.cpp C API (hand-written
//! declarations) and the NDJSON JSON-RPC plumbing. `meetko-nemotron-stream` is the live ASR engine,
//! `meetko-nemotron-diar` the batch diarization engine; both link the same prebuilt libraries.

pub mod diar;
pub mod ffi;
pub mod rpc;
