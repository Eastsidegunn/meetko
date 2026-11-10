//! meetko core: contract 0.1, whisper.cpp engine, storage, plugin host, export.

pub mod capture;
pub mod capture_run;
pub mod contract;
pub mod diarize;
pub mod error;
pub mod export;
pub mod host;
pub mod live;
pub mod manifest;
pub mod model_setup;
pub mod pipeline;
pub mod plugin_install;
pub mod plugin_source;
pub mod sandbox;
pub mod store;
pub mod text;
pub mod util;
pub mod whisper;

pub use error::{Error, Result};
