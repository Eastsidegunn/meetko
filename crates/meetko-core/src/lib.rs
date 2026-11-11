//! meetko core: contract 0.1, whisper.cpp engine, storage, plugin host, export.

pub mod batch;
pub mod capture;
pub mod capture_run;
pub mod contract;
pub mod diarize;
pub mod error;
pub mod export;
pub mod host;
pub mod live;
pub mod manifest;
pub mod mcp;
pub mod model_setup;
pub mod overlay;
pub mod pipeline;
pub mod plugin_install;
pub mod plugin_source;
pub mod sandbox;
pub mod speech_gate;
pub mod secrets;
pub mod store;
pub mod text;
pub mod util;
pub mod vocabulary;
pub mod whisper;
pub mod whisper_prompt;

pub use error::{Error, Result};
