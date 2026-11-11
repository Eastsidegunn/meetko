//! `meetko.toml` (docs/manifest-draft.md). Plugins are runnable; engine manifests
//! are parsed only (there is no external engine runner yet).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Error, Result};
use crate::util::{sha256_bytes, sha256_file};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub version: String,
    pub kind: String,
    pub contract: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub description: String,
    /// optional, shown in the gallery; not used by the core
    #[serde(default)]
    pub homepage: String,
}

/// the draft shows `command = "bin/x"` for engines and an array for plugins
fn string_or_vec<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum One {
        S(String),
        V(Vec<String>),
    }
    Ok(match One::deserialize(d)? {
        One::S(s) => vec![s],
        One::V(v) => v,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// an array: never passed through a shell
    #[serde(deserialize_with = "string_or_vec")]
    pub command: Vec<String>,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    #[serde(default)]
    pub transcript: bool,
    #[serde(default)]
    pub speakers: bool,
    #[serde(default)]
    pub meeting: bool,
    #[serde(default = "no_audio")]
    pub audio: String,
}

impl Default for Input {
    fn default() -> Self {
        Input { transcript: true, speakers: true, meeting: false, audio: no_audio() }
    }
}

fn no_audio() -> String {
    "none".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Output {
    #[serde(default)]
    pub display: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// enforced by the macOS sandbox only (docs/plugin-security.md); undeclared means no network
    #[serde(default)]
    pub network: bool,
    /// names of the secrets (API keys) the plugin needs, e.g. `ANTHROPIC_API_KEY` (`*_KEY`,
    /// `*_TOKEN`, `*_SECRET`). The user sets each value (OS keychain, see `crate::secrets`); a run
    /// gets the declared ones that are set in its `initialize` request. Requires `network = true`.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// names of plain settings the user may set, e.g. `ANTHROPIC_MODEL` (`*_MODEL`, `*_BASE_URL`):
    /// stored and passed like secrets, but shown and never redacted. Requires `network = true`.
    #[serde(default)]
    pub settings: Vec<String>,
}

/// `[role]` of an engine manifest
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Role {
    #[serde(default)]
    pub provides: String,
    #[serde(default)]
    pub word_timestamps: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package: Package,
    pub run: Run,
    #[serde(default)]
    pub input: Input,
    #[serde(default)]
    pub output: Output,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub role: Option<Role>,
}

/// No control, format, line-separator or invisible filler characters and no long combining-mark run;
/// see [`crate::text`]. Newlines count as control.
pub fn is_plain_text(s: &str) -> bool {
    crate::text::is_plain_text(s)
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let m: Manifest = toml::from_str(text).map_err(|e| Error::Manifest(crate::text::escape(&e.to_string(), true)))?;
        if !matches!(m.package.kind.as_str(), "plugin" | "engine") {
            return Err(Error::Manifest(format!("kind {:?} is not a plugin or engine", m.package.kind)));
        }
        if m.package.contract != crate::contract::CONTRACT {
            return Err(Error::Manifest(format!("contract {:?} is not supported", m.package.contract)));
        }
        if m.package.kind == "plugin" && m.run.protocol != "stdio-jsonrpc" {
            return Err(Error::Manifest(format!("protocol {:?} is not supported", m.run.protocol)));
        }
        if m.run.command.first().is_none_or(|c| c.is_empty()) {
            return Err(Error::Manifest("run.command is empty".into()));
        }
        // everything below is printed to a terminal or shown in a consent dialog: no escape
        // sequences, no characters that reorder or hide text
        let p = &m.package;
        let shown = [("id", &p.id), ("version", &p.version), ("description", &p.description), ("license", &p.license), ("homepage", &p.homepage), ("contract", &p.contract)]
            .into_iter()
            .chain(m.output.display.iter().map(|d| ("output.display", d)))
            .chain(m.run.command.iter().map(|c| ("run.command", c)));
        for (what, v) in shown {
            if !is_plain_text(v) {
                return Err(Error::Manifest(format!("{what} contains control or invisible formatting characters")));
            }
        }
        if !matches!(m.input.audio.as_str(), "none" | "full") {
            return Err(Error::Manifest(format!("input.audio {:?} is not none|full", m.input.audio)));
        }
        crate::secrets::check_declared(&m.permissions)?;
        Ok(m)
    }

    /// Parse and require a runnable plugin.
    pub fn parse_plugin(text: &str) -> Result<Self> {
        let m = Self::parse(text)?;
        m.require_plugin()?;
        Ok(m)
    }

    pub fn require_plugin(&self) -> Result<()> {
        if self.package.kind != "plugin" {
            return Err(Error::Manifest(format!(
                "{} is kind {:?}; only plugins can be added (engine manifests are parsed but not run yet)",
                self.package.id, self.package.kind
            )));
        }
        // checked here so `info`, `add` and the app's preview refuse before anything is copied
        if crate::store::agent::is_agent_id(&self.package.id) {
            return Err(Error::Manifest(format!(
                "{}: plugin ids starting with {} are reserved for results from AI agents (MCP)",
                self.package.id,
                crate::store::agent::AGENT_PREFIX
            )));
        }
        Ok(())
    }

    /// Manifest and the sha256 of its exact bytes.
    pub fn load(dir: &Path) -> Result<(Self, String)> {
        let bytes = std::fs::read(dir.join("meetko.toml"))
            .map_err(|e| Error::Manifest(format!("cannot read meetko.toml: {e}")))?;
        let text = String::from_utf8(bytes.clone()).map_err(|e| Error::Manifest(format!("meetko.toml: {e}")))?;
        Ok((Self::parse(&text)?, sha256_bytes(&bytes)))
    }

    /// Program and arguments. Both `/` and `\` separate path parts. A program with a
    /// separator is relative to the plugin dir and may not be absolute, carry a drive,
    /// or climb out with `..`; a bare name is looked up on PATH.
    pub fn resolve_command(&self, dir: &Path) -> Result<(PathBuf, Vec<String>)> {
        let prog = &self.run.command[0];
        let args = self.run.command[1..].to_vec();
        if !prog.contains(['/', '\\']) {
            if prog.contains(':') {
                return Err(Error::Manifest(format!("run.command {prog:?} must not carry a drive")));
            }
            return Ok((PathBuf::from(prog), args));
        }
        let b = prog.as_bytes();
        let drive = b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic();
        let parts: Vec<&str> = prog.split(['/', '\\']).collect();
        if drive || prog.starts_with(['/', '\\']) || parts.iter().any(|p| *p == ".." || p.contains(':')) {
            return Err(Error::Manifest(format!("run.command {prog:?} must stay inside the plugin dir")));
        }
        let mut path = dir.to_path_buf();
        path.extend(parts.iter().filter(|p| !p.is_empty() && **p != "."));
        Ok((path, args))
    }
}

/// sha256 over the sorted relative paths and contents of every file under `dir`,
/// excluding `target/`, `.git/` and `__pycache__/` at any depth. Symlinks are not
/// followed; their target text is hashed instead, so a link cannot hide changes.
pub fn code_sha256(dir: &Path) -> Result<String> {
    fn walk(base: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
        for e in std::fs::read_dir(base)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), "target" | ".git" | "__pycache__") {
                continue;
            }
            let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let ft = e.file_type()?;
            if ft.is_dir() {
                walk(&e.path(), &rel, out)?;
            } else {
                out.push((rel, e.path()));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(dir, "", &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for (rel, path) in files {
        let meta = std::fs::symlink_metadata(&path)?;
        let content = if meta.file_type().is_symlink() {
            format!("symlink:{}", std::fs::read_link(&path)?.to_string_lossy())
        } else {
            sha256_file(&path)?
        };
        h.update(format!("{rel}\0{content}\n").as_bytes());
    }
    Ok(crate::util::hex(&h.finalize()))
}

/// The files `run.command` names (program and arguments) as paths under `dir`; a bare program
/// name (python3) resolves to PATH and is not among them.
pub fn command_files(manifest: &Manifest, dir: &Path) -> Result<Vec<PathBuf>> {
    let (prog, args) = manifest.resolve_command(dir)?;
    let mut named: Vec<PathBuf> = vec![prog];
    for a in args {
        if !a.contains("..") && !a.starts_with(['/', '\\']) {
            named.push(dir.join(&a));
        }
    }
    Ok(named.into_iter().filter(|p| p.strip_prefix(dir).is_ok()).collect())
}

/// The code hash a plugin is pinned by: `code_sha256(dir)` plus the file `run.command[0]`
/// resolves to and every file named by `run.command` arguments inside the plugin dir, even
/// under an excluded dir such as `target/` (a compiled plugin's executable lives there).
pub fn plugin_code_sha256(manifest: &Manifest, dir: &Path) -> Result<String> {
    let mut h = Sha256::new();
    h.update(code_sha256(dir)?.as_bytes());
    let (prog, _) = manifest.resolve_command(dir)?;
    for path in command_files(manifest, dir)? {
        let is_prog = path == prog;
        // a bare name (python3) resolves to PATH, not to the plugin dir
        let Ok(rel) = path.strip_prefix(dir).map(Path::to_path_buf) else { continue };
        // Windows runs `target/release/x` as `x.exe`: hash the file that will actually run
        let path = if !path.is_file() && cfg!(windows) && path.extension().is_none() {
            path.with_extension("exe")
        } else {
            path
        };
        if path.is_file() {
            h.update(format!("\ncommand-file:{}\0{}", rel.to_string_lossy().replace('\\', "/"), sha256_file(&path)?).as_bytes());
        } else if is_prog {
            // a program that is missing now would be added later unhashed and still run
            return Err(Error::Manifest(format!("run.command program not found in the plugin: {}", rel.display())));
        }
    }
    Ok(crate::util::hex(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_description(d: &str) -> String {
        format!("[package]\nid=\"a/b\"\nversion=\"0\"\nkind=\"plugin\"\ncontract=\"0.1\"\ndescription={d:?}\n[run]\ncommand=[\"x\"]\nprotocol=\"stdio-jsonrpc\"\n")
    }

    #[test]
    fn invisible_and_separator_characters_in_shown_fields_are_refused() {
        assert!(Manifest::parse(&with_description("fine 한국어")).is_ok());
        for c in ['\u{2028}', '\u{2029}', '\u{115f}', '\u{1160}', '\u{3164}', '\u{ffa0}', '\u{34f}', '\u{fe0f}', '\u{e0101}', '\u{e0041}', '\u{600}', '\u{110bd}', '\u{13430}', '\u{1d173}'] {
            assert!(Manifest::parse(&with_description(&format!("x{c}y"))).is_err(), "U+{:04X}", c as u32);
        }
        assert!(Manifest::parse(&with_description(&format!("e{}", "\u{301}".repeat(9)))).is_err());
    }

    #[test]
    fn a_toml_error_never_echoes_raw_control_characters() {
        let e = Manifest::parse("[package]\nid = \"a\" \u{1b}[31m\u{7}\u{202e} oops\n").unwrap_err().to_string();
        assert!(!e.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{e:?}");
        assert!(e.contains("\\u{1b}"));
    }

    fn with_permissions(p: &str) -> String {
        format!("[package]\nid=\"a/b\"\nversion=\"0\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"x\"]\nprotocol=\"stdio-jsonrpc\"\n[permissions]\n{p}\n")
    }

    #[test]
    fn declared_secrets_are_checked() {
        let ok = Manifest::parse(&with_permissions("network = true\nsecrets = [\"ANTHROPIC_API_KEY\", \"OPENAI_API_KEY\"]\nsettings = [\"ANTHROPIC_MODEL\"]")).unwrap();
        assert_eq!(ok.permissions.secrets, ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]);
        assert_eq!(ok.permissions.settings, ["ANTHROPIC_MODEL"]);
        assert!(Manifest::parse(&with_permissions("settings = [\"ANTHROPIC_MODEL\"]")).is_err(), "settings need network too");
        assert!(Manifest::parse(&with_permissions("network = true\nsettings = [\"ANTHROPIC_API_KEY\"]")).is_err(), "a key is not a setting");
        assert!(Manifest::parse(&with_permissions("network = false")).unwrap().permissions.secrets.is_empty());
        let refused = [
            "secrets = [\"ANTHROPIC_API_KEY\"]",                     // no network: a key is useless and refused
            "network = true\nsecrets = [\"anthropic_key\"]",          // lowercase
            "network = true\nsecrets = [\"A-B\"]",
            "network = true\nsecrets = [\"PATH\"]",                   // reserved
            "network = true\nsecrets = [\"DYLD_INSERT_LIBRARIES\"]",
            "network = true\nsecrets = [\"HTTPS_PROXY\"]",
            "network = true\nsecrets = [\"PYTHONPATH\"]",
            "network = true\nsecrets = [\"MEETKO_DATA_DIR\"]",
            "network = true\nsecrets = [\"A_KEY\", \"A_KEY\"]",      // duplicate
            "network = true\nsecrets = [\"A1_KEY\", \"A2_KEY\", \"A3_KEY\", \"A4_KEY\", \"A5_KEY\"]", // too many
            "network = true\nsecrets = [\"ANTHROPIC\"]",            // no key-like ending
        ];
        for p in refused {
            assert!(Manifest::parse(&with_permissions(p)).is_err(), "{p}");
        }
        let long = format!("network = true\nsecrets = [\"{}\"]", "A".repeat(65));
        assert!(Manifest::parse(&with_permissions(&long)).is_err());
    }
}
