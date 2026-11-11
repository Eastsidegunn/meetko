//! Plugin secrets (API keys) and settings. Smallest safe mechanism, see docs/plugin-security.md:
//!
//! - A plugin declares names: `[permissions] secrets = ["ANTHROPIC_API_KEY"]` (values that must
//!   stay secret: `*_KEY`, `*_TOKEN`, `*_SECRET`) and `settings = ["ANTHROPIC_MODEL"]` (plain
//!   configuration: `*_BASE_URL`, `*_MODEL`). Either requires `network = true` (`check_declared`).
//! - The user sets each value (app: "API 키 넣기"; CLI: `meetko plugin secret set`, value on stdin).
//!   Both live in the OS secret store only (macOS: the login keychain, `keychain`), never in the
//!   data dir. A secret is never sent back to the webview (callers learn only whether it is set);
//!   a setting is not secret and is shown.
//! - A value is keyed by plugin id, the plugin's origin (`local` or `github:owner/repo`) and the
//!   name. A plugin with the same id from another origin finds nothing: a replacement never inherits
//!   the old plugin's values, and `Prior::forget_if_moved` deletes the old ones after a replace.
//! - A run gets only its own declared names that are set (`for_run`), in the `initialize` request
//!   on its stdin, never in its environment or command line (both readable by other processes of
//!   the user). Every secret value is redacted (exact match) from the plugin's output, error
//!   messages and stderr before anything is stored or shown (`Redactor`); settings are not.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::manifest::{Manifest, Permissions};
use crate::store::Store;

pub const MAX_SECRETS: usize = 4;
pub const MAX_SETTINGS: usize = 4;
pub const MAX_NAME: usize = 64;
/// bytes; an API key or a base URL is far shorter
pub const MAX_VALUE: usize = 4096;
/// Shorter secrets are refused: every secret is redacted from plugin output by exact match, and a
/// short value would mangle ordinary text. Real API keys are far longer.
pub const MIN_SECRET: usize = 8;
/// what a redacted value becomes; shorter than `MIN_SECRET`, so redaction never makes text longer
pub const REDACTED: &str = "***";

/// What a declared name holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// a credential: never shown again, redacted from everything the plugin says
    Secret,
    /// plain configuration (a model name, a base URL): shown, not redacted
    Setting,
}

/// Allowed endings, so a name says what it is.
const SECRET_SUFFIX: &[&str] = &["_KEY", "_TOKEN", "_SECRET"];
const SETTING_SUFFIX: &[&str] = &["_BASE_URL", "_MODEL"];

/// A backstop under the suffix rule: names that steer the interpreter, the loader, toolchains, TLS,
/// proxies, name resolution, shells or meetko itself, whatever they end with.
const RESERVED: &[&str] = &[
    "HOME", "PATH", "TMPDIR", "TEMP", "TMP", "USER", "LOGNAME", "SHELL", "LANG", "TZ", "TZDIR", "TERM", "USERPROFILE",
    "SYSTEMROOT", "COMSPEC", "PATHEXT", "APPDATA", "LOCALAPPDATA", "IFS", "ENV", "BASH_ENV", "DEVELOPER_DIR", "SDKROOT",
    "TOOLCHAINS", "SSLKEYLOGFILE", "CLASSPATH", "JDK_JAVA_OPTIONS", "VIRTUAL_ENV", "ZDOTDIR", "SHELLOPTS", "BASHOPTS",
    "PS4", "CDPATH", "GCONV_PATH", "HOSTALIASES", "RES_OPTIONS", "LOCALDOMAIN", "NLSPATH",
];
const RESERVED_PREFIX: &[&str] = &[
    "DYLD_", "LD_", "PYTHON", "MEETKO_", "LC_", "NODE_", "PERL", "RUBY", "JAVA_", "SSL_", "OPENSSL_", "CURL_", "GIT_",
    "XDG_", "GEM_", "BUNDLE_", "NPM_CONFIG_", "DENO_", "BUN_", "UV_", "CONDA_", "LUA_", "__",
];
const RESERVED_SUFFIX: &[&str] = &["_PROXY", "_CA_BUNDLE", "_CA_FILE", "_CA_PATH"];

fn shape_ok(name: &str) -> bool {
    (2..=MAX_NAME).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn reserved(name: &str) -> bool {
    // Go's own variables (GOPATH, GOFLAGS, GOPROXY ...) all start with GO; GOOGLE_* is a vendor name
    let go = name.starts_with("GO") && !name.starts_with("GOOGLE_");
    go || RESERVED.contains(&name) || RESERVED_PREFIX.iter().any(|p| name.starts_with(p)) || RESERVED_SUFFIX.iter().any(|s| name.ends_with(s))
}

/// `ANTHROPIC_API_KEY`: A-Z, 0-9, `_`, starts with a letter, 2..=64 chars, ends with `_KEY`,
/// `_TOKEN` or `_SECRET`, not reserved.
pub fn valid_secret_name(name: &str) -> bool {
    shape_ok(name) && SECRET_SUFFIX.iter().any(|s| name.ends_with(s) && name.len() > s.len()) && !reserved(name)
}

/// `ANTHROPIC_MODEL`, `OPENAI_BASE_URL`: the same shape, ending with `_MODEL` or `_BASE_URL`.
pub fn valid_setting_name(name: &str) -> bool {
    shape_ok(name) && SETTING_SUFFIX.iter().any(|s| name.ends_with(s) && name.len() > s.len()) && !reserved(name)
}

/// The kind `name` has in these declared permissions, if it is declared.
pub fn kind_of(p: &Permissions, name: &str) -> Option<Kind> {
    if p.secrets.iter().any(|n| n == name) {
        Some(Kind::Secret)
    } else if p.settings.iter().any(|n| n == name) {
        Some(Kind::Setting)
    } else {
        None
    }
}

/// The manifest rule: at most `MAX_SECRETS` secrets and `MAX_SETTINGS` settings, distinct valid
/// names, and only with `network = true` (a key or an endpoint is only useful to send somewhere).
pub fn check_declared(p: &Permissions) -> Result<()> {
    if p.secrets.is_empty() && p.settings.is_empty() {
        return Ok(());
    }
    if !p.network {
        return Err(Error::Manifest("permissions.secrets and permissions.settings need network = true (a key is only useful for a network service)".into()));
    }
    let lists = [("secrets", &p.secrets, MAX_SECRETS, valid_secret_name as fn(&str) -> bool, "end with _KEY, _TOKEN or _SECRET"), ("settings", &p.settings, MAX_SETTINGS, valid_setting_name, "end with _MODEL or _BASE_URL")];
    for (what, names, max, valid, ending) in lists {
        if names.len() > max {
            return Err(Error::Manifest(format!("permissions.{what} declares {} names; at most {max}", names.len())));
        }
        for n in names {
            if !valid(n) {
                return Err(Error::Manifest(format!(
                    "permissions.{what}: {:?} is not a valid name (A-Z, 0-9, _; starts with a letter; at most {MAX_NAME}; must {ending}; not a system variable such as PATH, DYLD_*, PYTHON*, *_PROXY)",
                    crate::text::escape(n, false)
                )));
            }
        }
    }
    let all: Vec<&String> = p.secrets.iter().chain(&p.settings).collect();
    for (i, n) in all.iter().enumerate() {
        if all[..i].contains(n) {
            return Err(Error::Manifest(format!("permissions: {n} is declared twice")));
        }
    }
    Ok(())
}

/// A value the user typed (trailing line breaks dropped by the caller): no control characters or
/// surrounding spaces, at most `MAX_VALUE` bytes, a secret at least `MIN_SECRET`.
pub fn check_value(kind: Kind, v: &str) -> Result<()> {
    let min = if kind == Kind::Secret { MIN_SECRET } else { 1 };
    if v.len() < min {
        return Err(Error::Secret(format!("the value is too short (at least {min} characters)")));
    }
    if v.len() > MAX_VALUE {
        return Err(Error::Secret(format!("the value is too long (at most {MAX_VALUE} bytes)")));
    }
    if v.chars().any(char::is_control) || v.trim() != v {
        return Err(Error::Secret("the value has line breaks, control characters or surrounding spaces".into()));
    }
    Ok(())
}

/// Where one value lives: plugin id, origin (`local` or `github:owner/repo`, lowercase), name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretKey {
    pub plugin_id: String,
    pub origin: String,
    pub name: String,
}

impl SecretKey {
    /// The keychain account string: a JSON array, so no id or origin can be confused with another.
    pub fn account(&self) -> String {
        serde_json::to_string(&[&self.plugin_id, &self.origin, &self.name]).expect("strings serialize")
    }
}

/// The OS secret store. Implementations never log values.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &SecretKey) -> Result<Option<String>>;
    /// Whether a value is set, without reading it (no keychain access prompt for the value).
    fn has(&self, key: &SecretKey) -> Result<bool>;
    fn set(&self, key: &SecretKey, value: &str) -> Result<()>;
    /// Deleting a value that is not there is not an error.
    fn delete(&self, key: &SecretKey) -> Result<()>;
}

/// The store this build uses: the login keychain on macOS; elsewhere none yet (reads find nothing,
/// writes fail with a clear error), never a file.
pub fn system() -> Arc<dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(keychain::Keychain::new(keychain::SERVICE))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(Unsupported)
    }
}

/// No secret store on this OS yet.
pub struct Unsupported;

impl SecretStore for Unsupported {
    fn get(&self, _: &SecretKey) -> Result<Option<String>> {
        Ok(None)
    }
    fn has(&self, _: &SecretKey) -> Result<bool> {
        Ok(false)
    }
    fn set(&self, _: &SecretKey, _: &str) -> Result<()> {
        Err(Error::Secret("this system has no supported secret store yet (meetko keeps keys only in the macOS keychain)".into()))
    }
    fn delete(&self, _: &SecretKey) -> Result<()> {
        Ok(())
    }
}

/// In memory, for tests: nothing persists.
#[derive(Default)]
pub struct MemoryStore(Mutex<BTreeMap<SecretKey, String>>);

impl MemoryStore {
    pub fn keys(&self) -> Vec<SecretKey> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn has(&self, key: &SecretKey) -> Result<bool> {
        Ok(self.0.lock().unwrap().contains_key(key))
    }
    fn set(&self, key: &SecretKey, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(key.clone(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &SecretKey) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// `local` for a plugin added from a folder, else its recorded repo (`github:owner/repo`), lowercase.
pub fn origin_of(store: &Store, plugin_id: &str) -> Result<String> {
    Ok(store.plugin_source(plugin_id)?.map(|s| s.repo.to_ascii_lowercase()).unwrap_or_else(|| "local".into()))
}

/// The installed plugin's key for `name` and what it holds; refuses a name the installed manifest does not declare.
pub fn key_for(store: &Store, plugin_id: &str, name: &str) -> Result<(SecretKey, Kind)> {
    let (_, manifest) = store.plugin(plugin_id)?;
    let kind = kind_of(&manifest.permissions, name)
        .ok_or_else(|| Error::Secret(format!("{plugin_id} does not declare a secret or setting named {}", crate::text::escape(name, false))))?;
    Ok((SecretKey { plugin_id: plugin_id.to_string(), origin: origin_of(store, plugin_id)?, name: name.to_string() }, kind))
}

/// One declared name of an installed plugin. `value` is filled for settings only: a secret's value
/// never leaves the store except into the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub set: bool,
    pub value: Option<String>,
}

/// Each declared name (secrets first) and whether it is set; settings with their value.
pub fn status(store: &Store, secrets: &dyn SecretStore, plugin_id: &str) -> Result<Vec<Entry>> {
    let (_, manifest) = store.plugin(plugin_id)?;
    let origin = origin_of(store, plugin_id)?;
    let key = |n: &str| SecretKey { plugin_id: plugin_id.to_string(), origin: origin.clone(), name: n.to_string() };
    let mut out = Vec::new();
    for n in &manifest.permissions.secrets {
        out.push(Entry { name: n.clone(), kind: Kind::Secret, set: secrets.has(&key(n))?, value: None });
    }
    for n in &manifest.permissions.settings {
        // A setting another meetko program saved, which this one was not allowed to read, is still
        // set: report it without its value instead of failing the whole list (a run says why).
        let (set, value) = match secrets.get(&key(n)) {
            Ok(v) => (v.is_some(), v),
            Err(Error::SecretAccessDenied(_)) => (true, None),
            Err(e) => return Err(e),
        };
        out.push(Entry { name: n.clone(), kind: Kind::Setting, set, value });
    }
    Ok(out)
}

/// Save a value for a declared name of an installed plugin.
pub fn set(store: &Store, secrets: &dyn SecretStore, plugin_id: &str, name: &str, value: &str) -> Result<()> {
    let (key, kind) = key_for(store, plugin_id, name)?;
    check_value(kind, value)?;
    secrets.set(&key, value)
}

pub fn clear(store: &Store, secrets: &dyn SecretStore, plugin_id: &str, name: &str) -> Result<()> {
    secrets.delete(&key_for(store, plugin_id, name)?.0)
}

/// What one run is given: the declared names that are set, for this plugin and origin only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RunConfig {
    pub secrets: Vec<(String, String)>,
    pub settings: Vec<(String, String)>,
}

/// Names only: a `{:?}` of a run's config (a log line, a panic message) never shows a value.
impl std::fmt::Debug for RunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |kv: &[(String, String)]| kv.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        f.debug_struct("RunConfig").field("secrets", &names(&self.secrets)).field("settings", &names(&self.settings)).finish()
    }
}

impl RunConfig {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.settings.is_empty()
    }
}

/// Read the run's values. A plugin that declares nothing never touches the secret store. A value
/// the store would not hand over (`Error::SecretAccessDenied`: the macOS access prompt was denied)
/// fails the run before the plugin starts: it never runs without a key it has.
pub fn for_run(secrets: &dyn SecretStore, plugin_id: &str, origin: &str, manifest: &Manifest) -> Result<RunConfig> {
    let read = |names: &[String]| -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for n in names {
            let key = SecretKey { plugin_id: plugin_id.to_string(), origin: origin.to_string(), name: n.clone() };
            if let Some(v) = secrets.get(&key)? {
                out.push((n.clone(), v));
            }
        }
        Ok(out)
    };
    Ok(RunConfig { secrets: read(&manifest.permissions.secrets)?, settings: read(&manifest.permissions.settings)? })
}

/// What was installed under an id before an install, so its values can be removed when the
/// install moved the id to another origin.
pub struct Prior {
    plugin_id: String,
    origin: String,
    names: Vec<String>,
}

impl Prior {
    /// `None` when the id is not installed.
    pub fn of(store: &Store, plugin_id: &str) -> Result<Option<Prior>> {
        match store.plugin(plugin_id) {
            Ok((_, m)) => {
                let names = m.permissions.secrets.into_iter().chain(m.permissions.settings).collect();
                Ok(Some(Prior { plugin_id: plugin_id.to_string(), origin: origin_of(store, plugin_id)?, names }))
            }
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The installed plugin declares keys or settings (their saved values stay when it is re-added
    /// from the same origin).
    pub fn declares_values(&self) -> bool {
        !self.names.is_empty()
    }

    /// After an install: if the id now comes from another origin, delete the old origin's values.
    /// They were already unreachable (values are keyed by origin); this keeps them from lingering.
    pub fn forget_if_moved(&self, store: &Store, secrets: &dyn SecretStore) -> Result<()> {
        if self.names.is_empty() || origin_of(store, &self.plugin_id)? == self.origin {
            return Ok(());
        }
        for n in &self.names {
            secrets.delete(&SecretKey { plugin_id: self.plugin_id.clone(), origin: self.origin.clone(), name: n.clone() })?;
        }
        Ok(())
    }
}

/// Exact-match redaction of the secret values a run was given (settings are not secret).
#[derive(Default, Clone)]
pub struct Redactor {
    /// longest first, so a value that contains another is replaced whole
    values: Vec<String>,
}

impl Redactor {
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Redactor {
        let mut values: Vec<String> = values.into_iter().filter(|v| !v.is_empty()).map(String::from).collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        Redactor { values }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The longest value in bytes (0 without values).
    pub fn longest(&self) -> usize {
        self.values.first().map_or(0, String::len)
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), REDACTED);
            }
        }
        out
    }

    /// The tail of `text` that fits `max` bytes, redacted, with no fragment of a value left at the
    /// cut: `cut_before` says `text` itself is already the end of something longer. The whole text is
    /// redacted before it is cut; then whatever precedes the first line break (or the first
    /// `longest()` bytes) after a cut is dropped, since a value never spans a line break.
    pub fn tail(&self, text: &str, max: usize, cut_before: bool) -> String {
        let red = self.text(text);
        let mut start = red.len().saturating_sub(max);
        while !red.is_char_boundary(start) {
            start += 1;
        }
        let cut = cut_before || start > 0;
        let mut tail = &red[start..];
        if cut && !self.is_empty() {
            let skip = match tail.find('\n') {
                Some(i) if i < self.longest() => i + 1,
                _ => {
                    let mut k = self.longest().min(tail.len());
                    while !tail.is_char_boundary(k) {
                        k += 1;
                    }
                    k
                }
            };
            tail = &tail[skip..];
        }
        if cut { format!("…{tail}") } else { tail.to_string() }
    }

    /// Every string and object key in `v`, identifiers and anchors included: a redacted id or
    /// anchor then fails validation and the whole result is refused, which is what should happen
    /// to output that carries a key there.
    pub fn value(&self, v: &mut Value) {
        if self.is_empty() {
            return;
        }
        match v {
            Value::String(s) => {
                if self.values.iter().any(|x| s.contains(x.as_str())) {
                    *s = self.text(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.value(x)),
            Value::Object(o) => {
                let old = std::mem::take(o);
                for (k, mut x) in old {
                    self.value(&mut x);
                    o.insert(self.text(&k), x);
                }
            }
            _ => {}
        }
    }

    /// An error with every value removed from its message.
    pub fn error(&self, e: Error) -> Error {
        if self.is_empty() {
            return e;
        }
        match e {
            Error::Plugin(m) => Error::Plugin(self.text(&m)),
            Error::Invalid(m) => Error::Invalid(self.text(&m)),
            Error::Manifest(m) => Error::Manifest(self.text(&m)),
            Error::Corrupt(m) => Error::Corrupt(self.text(&m)),
            Error::Secret(m) => Error::Secret(self.text(&m)),
            other => {
                // variants that carry no plugin text; anything else is flattened to be safe
                let s = other.to_string();
                if self.values.iter().any(|x| s.contains(x.as_str())) {
                    Error::Plugin(self.text(&s))
                } else {
                    other
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub mod keychain {
    //! The login keychain through Security.framework's SecItem API (generic passwords), declared
    //! here directly: no crate for it is vendored. Items are `kSecClassGenericPassword` with
    //! service `SERVICE` and the account string of a `SecretKey`.
    //!
    //! Access is per program: an item may be read without a prompt only by the binary that saved it
    //! (by code identity: for an unsigned, ad-hoc signed build, its exact code hash, so a rebuild is
    //! another program) or one the user chose "Always Allow" for. Another meetko binary (the CLI vs
    //! the app, a rebuilt app) makes macOS ask for the login password; a denial is
    //! `Error::SecretAccessDenied`. See docs/plugin-security.md ("Keychain prompts").

    use std::ffi::c_void;

    use super::{SecretKey, SecretStore};
    use crate::error::{Error, Result};

    pub const SERVICE: &str = "meetko plugin secret";

    type CFTypeRef = *const c_void;
    type CFIndex = isize;
    type OSStatus = i32;

    #[repr(C)]
    struct Callbacks([u8; 0]);

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFTypeDictionaryKeyCallBacks: Callbacks;
        static kCFTypeDictionaryValueCallBacks: Callbacks;
        static kCFBooleanTrue: CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: CFIndex, encoding: u32, external: u8) -> CFTypeRef;
        fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
        fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
        fn CFDictionaryCreate(alloc: CFTypeRef, keys: *const CFTypeRef, values: *const CFTypeRef, n: CFIndex, kcb: *const Callbacks, vcb: *const Callbacks) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFDataGetTypeID() -> usize;
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecClass: CFTypeRef;
        static kSecClassGenericPassword: CFTypeRef;
        static kSecAttrService: CFTypeRef;
        static kSecAttrAccount: CFTypeRef;
        static kSecAttrLabel: CFTypeRef;
        static kSecValueData: CFTypeRef;
        static kSecReturnData: CFTypeRef;
        static kSecMatchLimit: CFTypeRef;
        static kSecMatchLimitOne: CFTypeRef;
        fn SecItemAdd(attrs: CFTypeRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemCopyMatching(query: CFTypeRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemDelete(query: CFTypeRef) -> OSStatus;
    }

    const UTF8: u32 = 0x0800_0100;
    const ERR_NOT_FOUND: OSStatus = -25300;
    const ERR_DUPLICATE: OSStatus = -25299;

    /// An owned CF object, released on drop.
    struct Cf(CFTypeRef);

    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }

    fn cf_string(s: &str) -> Result<Cf> {
        let r = unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, UTF8, 0) };
        if r.is_null() {
            return Err(Error::Secret("cannot make a keychain query".into()));
        }
        Ok(Cf(r))
    }

    fn cf_data(b: &[u8]) -> Result<Cf> {
        let r = unsafe { CFDataCreate(std::ptr::null(), b.as_ptr(), b.len() as CFIndex) };
        if r.is_null() {
            return Err(Error::Secret("cannot make a keychain value".into()));
        }
        Ok(Cf(r))
    }

    fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Result<Cf> {
        let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
        let r = unsafe { CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), pairs.len() as CFIndex, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks) };
        if r.is_null() {
            return Err(Error::Secret("cannot make a keychain query".into()));
        }
        Ok(Cf(r))
    }

    /// The status codes that mean "not allowed": the access prompt was denied (`errSecAuthFailed`,
    /// also what a program gets that may not even ask), cancelled (`errSecUserCanceled`) or could
    /// not be shown (`errSecInteractionNotAllowed`: a locked keychain without a window, an ssh
    /// session), or the item belongs to another program (`errSecInvalidOwnerEdit`, on delete).
    pub const ERR_USER_CANCELED: OSStatus = -128;
    pub const ERR_AUTH_FAILED: OSStatus = -25293;
    pub const ERR_INTERACTION_NOT_ALLOWED: OSStatus = -25308;
    pub const ERR_INVALID_OWNER_EDIT: OSStatus = -25244;
    const DENIED: [OSStatus; 4] = [ERR_USER_CANCELED, ERR_AUTH_FAILED, ERR_INTERACTION_NOT_ALLOWED, ERR_INVALID_OWNER_EDIT];

    /// What was being done when the keychain answered: the advice differs for reading and changing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Op {
        Read,
        Lookup,
        Save,
        Delete,
    }

    impl Op {
        fn verb(self) -> &'static str {
            match self {
                Op::Read => "read",
                Op::Lookup => "lookup",
                Op::Save => "save",
                Op::Delete => "delete",
            }
        }
    }

    /// The error for a failed keychain call: `SecretAccessDenied` (code `secret_access_denied`) with
    /// what to do when access was not allowed, else `Secret` with the status code. Only the status
    /// code, the plugin id and the name: nothing of the value is in it.
    pub fn error_for(op: Op, status: OSStatus, key: &SecretKey) -> Error {
        if DENIED.contains(&status) {
            return denied(op, Some(status), key);
        }
        let hint = if status == -34018 { " (this build may not use the keychain)" } else { "" };
        Error::Secret(format!("keychain {} failed: OSStatus {status}{hint}", op.verb()))
    }

    /// `status` is `None` when the keychain withheld the value without saying why (the item is
    /// there, its data did not come back).
    pub fn denied(op: Op, status: Option<OSStatus>, key: &SecretKey) -> Error {
        let (plugin, name) = (crate::text::escape(&key.plugin_id, false), crate::text::escape(&key.name, false));
        let code = match status {
            Some(ERR_INTERACTION_NOT_ALLOWED) => format!("OSStatus {ERR_INTERACTION_NOT_ALLOWED}: the keychain may be locked, or macOS could not show its window"),
            Some(s) => format!("OSStatus {s}"),
            None => "the item is there but its value was not returned".into(),
        };
        Error::SecretAccessDenied(match op {
            Op::Read | Op::Lookup => format!(
                "macOS asked whether this meetko program may use {name} of {plugin} saved in the keychain, and access was not \
                 allowed ({code}). Run it again; when macOS asks, enter this Mac's login password (the one you sign in with) and \
                 click \"Always Allow\". A key saved with the meetko CLI, or by another build of the app, asks once per program; \
                 saving the key in the app (Plugins > API 키 넣기) avoids that."
            ),
            Op::Save | Op::Delete => format!(
                "{name} of {plugin} was saved by another meetko program (the CLI, or another build of the app), and macOS did not \
                 allow this one to change it ({code}). When macOS asks, enter this Mac's login password and allow it; or remove it \
                 with the program that saved it (`meetko plugin secret clear {plugin} {name}`) or in Keychain Access (service \
                 \"{SERVICE}\"), then save it again."
            ),
        })
    }

    /// A save that failed after the old item was deleted: say so, the previous value is gone.
    fn removed(e: Error) -> Error {
        let gone = "; the previously saved value was removed, set it again";
        match e {
            Error::Secret(m) => Error::Secret(format!("{m}{gone}")),
            Error::SecretAccessDenied(m) => Error::SecretAccessDenied(format!("{m}{gone}")),
            e => e,
        }
    }

    pub struct Keychain {
        service: String,
    }

    impl Keychain {
        pub fn new(service: &str) -> Keychain {
            Keychain { service: service.to_string() }
        }

        /// `(service, account)` strings, kept alive while a query that points at them is used.
        fn ids(&self, key: &SecretKey) -> Result<(Cf, Cf)> {
            Ok((cf_string(&self.service)?, cf_string(&key.account())?))
        }
    }

    impl SecretStore for Keychain {
        fn get(&self, key: &SecretKey) -> Result<Option<String>> {
            let (service, account) = self.ids(key)?;
            let q = unsafe {
                dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecReturnData, kCFBooleanTrue), (kSecMatchLimit, kSecMatchLimitOne)])?
            };
            let mut out: CFTypeRef = std::ptr::null();
            let status = unsafe { SecItemCopyMatching(q.0, &mut out) };
            if status == ERR_NOT_FOUND {
                // Nothing came back. If the item is there anyway (its attributes are readable without
                // a prompt), the keychain withheld the value: say so instead of running without it.
                return if self.has(key)? { Err(denied(Op::Read, None, key)) } else { Ok(None) };
            }
            if status != 0 {
                return Err(error_for(Op::Read, status, key));
            }
            let data = Cf(out);
            if data.0.is_null() || unsafe { CFGetTypeID(data.0) != CFDataGetTypeID() } {
                return Err(Error::Secret("keychain read returned no data".into()));
            }
            let bytes = unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), CFDataGetLength(data.0) as usize) }.to_vec();
            String::from_utf8(bytes).map(Some).map_err(|_| Error::Secret("the stored value is not text; set it again".into()))
        }

        fn has(&self, key: &SecretKey) -> Result<bool> {
            // attributes only: no data is returned, so no access prompt for the value
            let (service, account) = self.ids(key)?;
            let q = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecMatchLimit, kSecMatchLimitOne)])? };
            match unsafe { SecItemCopyMatching(q.0, std::ptr::null_mut()) } {
                0 => Ok(true),
                ERR_NOT_FOUND => Ok(false),
                s => Err(error_for(Op::Lookup, s, key)),
            }
        }

        /// Delete, then add: a new item carries the access list of the binary that adds it (this
        /// meetko), not whatever an item someone else created under the same name had. A duplicate
        /// (another process added it in between) is deleted and added again, once. An item saved by
        /// another meetko binary may refuse the delete (`SecretAccessDenied`); its value then stays.
        fn set(&self, key: &SecretKey, value: &str) -> Result<()> {
            let (service, account) = self.ids(key)?;
            let data = cf_data(value.as_bytes())?;
            let label = cf_string(&format!("meetko: {} {}", key.plugin_id, key.name))?;
            let add = unsafe {
                dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0), (kSecAttrLabel, label.0), (kSecValueData, data.0)])?
            };
            for _ in 0..2 {
                self.delete(key)?;
                match unsafe { SecItemAdd(add.0, std::ptr::null_mut()) } {
                    0 => return Ok(()),
                    ERR_DUPLICATE => continue,
                    s => return Err(removed(error_for(Op::Save, s, key))),
                }
            }
            Err(removed(error_for(Op::Save, ERR_DUPLICATE, key)))
        }

        fn delete(&self, key: &SecretKey) -> Result<()> {
            let (service, account) = self.ids(key)?;
            let q = unsafe { dict(&[(kSecClass, kSecClassGenericPassword), (kSecAttrService, service.0), (kSecAttrAccount, account.0)])? };
            match unsafe { SecItemDelete(q.0) } {
                0 | ERR_NOT_FOUND => Ok(()),
                s => Err(error_for(Op::Delete, s, key)),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn key() -> SecretKey {
            SecretKey { plugin_id: "meetko/summary-py".into(), origin: "local".into(), name: "ANTHROPIC_API_KEY".into() }
        }

        /// Denied, cancelled, no window, not the owner: one code the app turns into plain words,
        /// with the next step in the message; other failures stay `secret`.
        #[test]
        fn denials_are_secret_access_denied_with_what_to_do() {
            for s in [ERR_AUTH_FAILED, ERR_USER_CANCELED, ERR_INTERACTION_NOT_ALLOWED, ERR_INVALID_OWNER_EDIT] {
                for op in [Op::Read, Op::Lookup, Op::Save, Op::Delete] {
                    let e = error_for(op, s, &key());
                    assert_eq!(e.code(), "secret_access_denied", "{op:?} {s}");
                    let m = e.to_string();
                    assert!(m.contains("ANTHROPIC_API_KEY") && m.contains("meetko/summary-py") && m.contains(&s.to_string()), "{m}");
                    assert!(m.contains("login password"), "{m}");
                }
            }
            let read = error_for(Op::Read, ERR_AUTH_FAILED, &key()).to_string();
            assert!(read.contains("Always Allow") && read.contains("API 키 넣기"), "{read}");
            let change = error_for(Op::Delete, ERR_INVALID_OWNER_EDIT, &key()).to_string();
            assert!(change.contains("meetko plugin secret clear meetko/summary-py ANTHROPIC_API_KEY") && change.contains(SERVICE), "{change}");
            assert!(error_for(Op::Read, ERR_INTERACTION_NOT_ALLOWED, &key()).to_string().contains("locked"));
            // a withheld value (the item exists, no data came back) is a denial too
            assert_eq!(denied(Op::Read, None, &key()).code(), "secret_access_denied");
            // a value lost in a failed save is still said
            assert!(removed(error_for(Op::Save, ERR_AUTH_FAILED, &key())).to_string().contains("previously saved value was removed"));
        }

        #[test]
        fn other_failures_stay_secret() {
            for (s, hint) in [(-34018, "may not use the keychain"), (-25299, ""), (-50, "")] {
                let e = error_for(Op::Read, s, &key());
                assert_eq!(e.code(), "secret", "{s}");
                assert!(e.to_string().contains(&format!("OSStatus {s}")) && e.to_string().contains(hint));
            }
        }

        /// Writes, reads and deletes one throwaway item in the login keychain under a test-only
        /// service. Ignored by default: it touches the user's keychain and may show a prompt.
        /// `cargo test -p meetko-core keychain -- --ignored`
        #[test]
        #[ignore]
        fn round_trip_in_the_login_keychain() {
            let kc = Keychain::new("meetko plugin secret (test)");
            let key = SecretKey { plugin_id: "test/kc".into(), origin: "local".into(), name: "TEST_KEY".into() };
            kc.delete(&key).unwrap();
            assert!(!kc.has(&key).unwrap());
            assert_eq!(kc.get(&key).unwrap(), None);
            kc.set(&key, "not-a-real-key-1").unwrap();
            assert!(kc.has(&key).unwrap());
            assert_eq!(kc.get(&key).unwrap().as_deref(), Some("not-a-real-key-1"));
            kc.set(&key, "not-a-real-key-2").unwrap();
            assert_eq!(kc.get(&key).unwrap().as_deref(), Some("not-a-real-key-2"));
            kc.delete(&key).unwrap();
            assert_eq!(kc.get(&key).unwrap(), None);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names() {
        for ok in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GITHUB_TOKEN", "APP_SECRET", "GOOGLE_API_KEY", "A_KEY"] {
            assert!(valid_secret_name(ok), "{ok}");
            assert!(!valid_setting_name(ok), "{ok}");
        }
        for ok in ["ANTHROPIC_MODEL", "OPENAI_BASE_URL", "X_MODEL"] {
            assert!(valid_setting_name(ok), "{ok}");
            assert!(!valid_secret_name(ok), "{ok}");
        }
        let bad = [
            "", "A", "_KEY", "KEY", "a_key", "1_KEY", "KEY-1_KEY", "PATH", "HOME", "DYLD_X_KEY", "LD_PRELOAD", "PYTHONHOME_KEY", "HTTPS_PROXY",
            "MEETKO_X_KEY", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "OPENSSL_CONF_KEY", "GOPROXY_TOKEN", "GEM_HOME_KEY", "NPM_CONFIG_TOKEN",
            "UV_INDEX_TOKEN", "CONDA_KEY", "LUA_PATH_KEY", "BUN_AUTH_TOKEN", "DENO_AUTH_TOKENS", "BUNDLE_GEMS_KEY", "SOME_URL", "MODEL", "KÉY_KEY",
        ];
        for b in bad {
            assert!(!valid_secret_name(b) && !valid_setting_name(b), "{b}");
        }
    }

    fn perms(secrets: &[&str], settings: &[&str], network: bool) -> Permissions {
        Permissions { network, secrets: secrets.iter().map(|s| s.to_string()).collect(), settings: settings.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn declarations() {
        assert!(check_declared(&perms(&["A_API_KEY"], &["A_MODEL", "A_BASE_URL"], true)).is_ok());
        assert!(check_declared(&perms(&[], &[], false)).is_ok());
        assert!(check_declared(&perms(&["A_API_KEY"], &[], false)).is_err());
        assert!(check_declared(&perms(&[], &["A_MODEL"], false)).is_err());
        assert!(check_declared(&perms(&["A_MODEL"], &[], true)).is_err(), "a setting name declared as a secret");
        assert!(check_declared(&perms(&[], &["A_API_KEY"], true)).is_err(), "a secret name declared as a setting");
        assert!(check_declared(&perms(&["A_KEY", "A_KEY"], &[], true)).is_err());
        assert!(check_declared(&perms(&["A1_KEY", "A2_KEY", "A3_KEY", "A4_KEY", "A5_KEY"], &[], true)).is_err());
        assert!(check_declared(&perms(&[], &["A1_MODEL", "A2_MODEL", "A3_MODEL", "A4_MODEL", "A5_MODEL"], true)).is_err());
    }

    #[test]
    fn values() {
        assert!(check_value(Kind::Secret, "sk-abcdefgh").is_ok());
        assert!(check_value(Kind::Secret, "sk-abcd").is_err(), "shorter than MIN_SECRET");
        assert!(check_value(Kind::Setting, "qwen3").is_ok());
        for bad in ["", " sk-abcdefgh", "sk-abcdefgh ", "sk\nabcdefgh", "sk\u{7}abcdefgh"] {
            assert!(check_value(Kind::Secret, bad).is_err(), "{bad:?}");
        }
        assert!(check_value(Kind::Setting, "").is_err());
        assert!(check_value(Kind::Setting, "a\nb").is_err());
        assert!(check_value(Kind::Secret, &"x".repeat(MAX_VALUE + 1)).is_err());
    }

    #[test]
    fn redaction_is_exact_never_lengthens_and_covers_every_field() {
        let r = Redactor::new(["sk-secret-1234", "sk-secret"]);
        assert_eq!(r.text("key=sk-secret-1234 and sk-secret!"), "key=*** and ***!");
        let mut v = json!({"plugin": {"id": "sk-secret"}, "items": [{"text": "uses sk-secret-1234", "evidence": [{"from": "sk-secret", "to": "sk-secret"}], "owner": "sk-secret"}], "sk-secret": 1, "n": 5});
        r.value(&mut v);
        assert_eq!(v, json!({"plugin": {"id": "***"}, "items": [{"text": "uses ***", "evidence": [{"from": "***", "to": "***"}], "owner": "***"}], "***": 1, "n": 5}));
        let e = r.error(Error::Plugin("401: bad key sk-secret-1234".into())).to_string();
        assert!(!e.contains("sk-secret"), "{e}");
        assert!(Redactor::new([""]).is_empty());
        assert!(REDACTED.len() < MIN_SECRET);
    }

    /// A value cut by the tail limit leaves no fragment, wherever the cut falls.
    #[test]
    fn a_tail_cut_through_a_value_leaves_no_fragment() {
        let key = "SECRETABCDEFGHIJKLMNOPQRSTUV";
        let r = Redactor::new([key]);
        for pad in 0..key.len() + 2 {
            let text = format!("{}{key}{}", "x".repeat(50), "y".repeat(100 + pad));
            // the cut at 110 + pad.. lands inside the key for most pads
            let max = 100 + pad + key.len() / 2;
            let t = r.tail(&text, max, false);
            for w in key.as_bytes().windows(4) {
                assert!(!t.contains(std::str::from_utf8(w).unwrap()), "pad {pad}: {t}");
            }
            // the buffer itself may already start inside a value (its head was dropped earlier)
            let t = r.tail(&text[50 + key.len() / 2..], 10_000, true);
            assert!(!t.contains(&key[key.len() / 2..key.len() / 2 + 4]), "{t}");
        }
        let line = format!("ok line\n{key} tail");
        assert_eq!(r.tail(&line, 10_000, false), "ok line\n*** tail");
    }

    #[test]
    fn run_config_debug_shows_names_only() {
        let r = RunConfig { secrets: vec![("A_KEY".into(), "sk-value-1234".into())], settings: vec![("A_MODEL".into(), "model-x".into())] };
        let d = format!("{r:?}");
        assert!(d.contains("A_KEY") && d.contains("A_MODEL") && !d.contains("sk-value") && !d.contains("model-x"), "{d}");
    }

    #[test]
    fn account_strings_cannot_collide() {
        let a = SecretKey { plugin_id: "a|b".into(), origin: "local".into(), name: "K1".into() };
        let b = SecretKey { plugin_id: "a".into(), origin: "b|local".into(), name: "K1".into() };
        assert_ne!(a.account(), b.account());
    }
}
