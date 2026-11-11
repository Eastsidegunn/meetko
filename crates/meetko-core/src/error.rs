use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// data that breaks the contract (schema or invariant); never stored
    #[error("invalid data: {0}")]
    Invalid(String),
    /// stored data that no longer parses or validates; never papered over
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
    /// the model file named by the config is not there
    #[error("model missing: {0}")]
    ModelMissing(String),
    /// the engine binary cannot be started
    #[error("engine not found: {0}")]
    EngineNotFound(String),
    /// the engine ran but its output is unusable
    #[error("bad engine output: {0}")]
    BadEngineOutput(String),
    #[error("engine: {0}")]
    Engine(String),
    /// an engine that must run sandboxed cannot be (macOS: no sandbox-exec, or the profile does not apply)
    #[error("engine sandbox: {0}")]
    EngineSandbox(String),
    #[error("cancelled")]
    Cancelled,
    #[error("plugin: {0}")]
    Plugin(String),
    /// the plugin gallery index does not exist (HTTP 404), e.g. the gallery is not open yet
    #[error("plugin gallery unavailable: {0}")]
    GalleryUnavailable(String),
    /// the plugin directory is not what the user was shown
    #[error("plugin changed: {0}")]
    PluginChanged(String),
    /// installing would replace a plugin that came from somewhere else; needs an explicit "replace"
    #[error("plugin source changed: {0}")]
    PluginSourceChanged(String),
    /// a plugin secret (API key) could not be checked, saved or read; never carries the value
    #[error("secret: {0}")]
    Secret(String),
    /// the OS secret store refused this program access to a saved value (macOS: the keychain
    /// prompt was denied or could not be shown, or the item belongs to another meetko binary);
    /// the message says what to do. Never carries the value.
    #[error("keychain access not allowed: {0}")]
    SecretAccessDenied(String),
    #[error("plugin timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// fetching or copying a model failed; the first field is a code for the UI
    /// (network, disk_full, checksum_mismatch, too_big, host_not_allowed, wrong_file, io)
    #[error("download: {1}")]
    Download(&'static str, String),
    #[error("capture: {0}")]
    Capture(String),
    /// approving this draft would move the head back to older content
    #[error("older than the current version: {0}")]
    StaleDraft(String),
}

impl Error {
    /// Stable machine-readable code for the GUI and scripts.
    pub fn code(&self) -> &'static str {
        match self {
            Error::ModelMissing(_) => "model_missing",
            Error::EngineNotFound(_) => "engine_not_found",
            Error::BadEngineOutput(_) => "bad_engine_output",
            Error::Cancelled => "cancelled",
            Error::Timeout(_) => "plugin_timeout",
            Error::Invalid(_) => "invalid_data",
            Error::Corrupt(_) => "corrupt_data",
            Error::NotFound(_) => "not_found",
            Error::Capture(_) => "capture",
            Error::GalleryUnavailable(_) => "gallery_unavailable",
            Error::PluginChanged(_) => "plugin_changed",
            Error::PluginSourceChanged(_) => "plugin_source_changed",
            Error::StaleDraft(_) => "stale_draft",
            Error::EngineSandbox(_) => "engine_sandbox",
            Error::Secret(_) => "secret",
            Error::SecretAccessDenied(_) => "secret_access_denied",
            Error::Download(code, _) => code,
            _ => "error",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
