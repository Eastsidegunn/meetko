//! Installing a plugin from a source spec: `github:OWNER/REPO@REF`.
//!
//! Network happens only when this is called (an explicit command, never in the background).
//! The flow: resolve REF to a full commit sha over HTTPS, download that commit's tarball,
//! extract it strictly into a fresh private temp dir, and hand the directory to the existing
//! install path (`plugin_install`: verified copy, consent, manifest and code hashes). Nothing
//! is built or executed here: a plugin must run as checked out.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::util;

pub const DEFAULT_API: &str = "https://api.github.com";
pub const DEFAULT_CODELOAD: &str = "https://codeload.github.com";
/// compressed download cap
pub const MAX_DOWNLOAD: u64 = 25 * 1024 * 1024;
/// total extracted bytes cap
pub const MAX_EXTRACTED: u64 = 100 * 1024 * 1024;
/// every tar entry counts (files, directories, metadata), not only regular files
pub const MAX_FILES: usize = 5000;
pub const MAX_DEPTH: usize = 32;
/// room on top of the file bytes for tar headers, padding and extension entries
const STREAM_SLACK: u64 = 16 * 1024 * 1024;
/// a PAX / GNU long-name extension entry is a few hundred bytes in any honest archive; the tar
/// crate reads one fully into memory, so a bigger claim is refused before it is read
const MAX_EXTENSION_ENTRY: u64 = 1024 * 1024;

/// Where a plugin came from. Stored with the installed plugin row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginSource {
    /// `github:OWNER/REPO`
    pub repo: String,
    /// the full commit sha that was downloaded
    pub sha: String,
    /// what the user asked for (tag or sha)
    pub reference: String,
}

/// A parsed `github:OWNER/REPO@REF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    pub owner: String,
    pub repo: String,
    pub reference: String,
}

impl SourceSpec {
    /// `Ok(None)` when the text is not a `github:` spec (it is a local path then).
    pub fn parse(text: &str) -> Result<Option<SourceSpec>> {
        let Some(rest) = text.strip_prefix("github:") else { return Ok(None) };
        let bad = |why: &str| Error::Plugin(format!("{text:?}: {why}; expected github:OWNER/REPO@REF"));
        let (path, reference) = rest.split_once('@').ok_or_else(|| bad("a tag or full commit sha is required after @"))?;
        let (owner, repo) = path.split_once('/').ok_or_else(|| bad("missing OWNER/REPO"))?;
        let name_ok = |s: &str| {
            !s.is_empty() && s.len() <= 100 && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };
        if !name_ok(owner) || !name_ok(repo) {
            return Err(bad("owner or repo has characters GitHub does not allow"));
        }
        let ref_ok = !reference.is_empty()
            && reference.len() <= 200
            && !reference.starts_with(['/', '-', '.'])
            && !reference.ends_with(['/', '.'])
            && !reference.contains("..")
            && !reference.contains("//")
            && reference.chars().all(|c| c.is_ascii_alphanumeric() || "._-/+".contains(c));
        if !ref_ok {
            return Err(bad("the ref (tag or commit sha) has unsupported characters"));
        }
        Ok(Some(SourceSpec { owner: owner.into(), repo: repo.into(), reference: reference.into() }))
    }

    pub fn repo_id(&self) -> String {
        format!("github:{}/{}", self.owner, self.repo)
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Base URLs; overridable for tests with MEETKO_GITHUB_API / MEETKO_GITHUB_CODELOAD.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub api: String,
    pub codeload: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { api: DEFAULT_API.into(), codeload: DEFAULT_CODELOAD.into() }
    }
}

impl Endpoints {
    /// The defaults, or the environment overrides when they pass `check_url` (https, or plain
    /// http to a loopback host / in a debug build).
    pub fn from_env() -> Result<Self> {
        let get = |k: &str, d: &str| -> Result<String> {
            let v = std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
            let v = v.trim_end_matches('/').to_string();
            check_url(k, &v)?;
            Ok(v)
        };
        Ok(Endpoints { api: get("MEETKO_GITHUB_API", DEFAULT_API)?, codeload: get("MEETKO_GITHUB_CODELOAD", DEFAULT_CODELOAD)? })
    }
}

fn split_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    Some((scheme, rest.split(['/', '?', '#']).next().unwrap_or("")))
}

fn is_loopback_authority(auth: &str) -> bool {
    if auth.contains('@') {
        return false;
    }
    let host = match auth.strip_prefix('[') {
        Some(r) => r.split(']').next().unwrap_or(""),
        None if auth.matches(':').count() == 1 => auth.rsplit_once(':').map(|(h, _)| h).unwrap_or(auth),
        None => auth,
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `https://` always; `http://` only in a debug build or toward a loopback host (local test
/// servers). Credentials in the URL are never accepted.
pub fn check_url(what: &str, url: &str) -> Result<()> {
    let Some((scheme, auth)) = split_url(url) else {
        return Err(Error::Plugin(format!("{what}: {url:?} is not a URL")));
    };
    if auth.is_empty() || auth.contains('@') {
        return Err(Error::Plugin(format!("{what}: the URL needs a plain host without credentials")));
    }
    match scheme {
        "https" => Ok(()),
        "http" if cfg!(debug_assertions) || is_loopback_authority(auth) => Ok(()),
        _ => Err(Error::Plugin(format!("{what} must be an https:// URL (plain http is only accepted for localhost)"))),
    }
}

/// A downloaded, extracted source tree. The temp dir is removed when this is dropped.
#[derive(Debug)]
pub struct Fetched {
    _tmp: tempfile::TempDir,
    pub dir: PathBuf,
    pub source: PluginSource,
}

/// Connect and per-read idle timeouts, so a slow but moving link can finish within the size cap;
/// the global bound only stops a link that trickles forever.
pub(crate) fn agent(url: &str) -> ureq::Agent {
    let plain_http_ok = cfg!(debug_assertions) || split_url(url).is_some_and(|(_, a)| is_loopback_authority(a));
    ureq::Agent::config_builder()
        .https_only(!plain_http_ok)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(30 * 60)))
        .user_agent("meetko-plugin-install")
        .build()
        .into()
}

fn http_err(what: &str, e: ureq::Error) -> Error {
    match e {
        ureq::Error::StatusCode(404) => Error::Plugin(format!("{what}: not found (404). Check the owner, repo and ref; a private repo cannot be installed")),
        ureq::Error::StatusCode(c @ (403 | 429)) => Error::Plugin(format!("{what}: refused ({c}); GitHub may be rate-limiting unauthenticated requests, try again later")),
        e => Error::Plugin(format!("{what}: {e}")),
    }
}

fn api_get(url: &str, what: &str, accept: &str, limit: u64) -> Result<String> {
    check_url(what, url)?;
    let mut resp = agent(url).get(url).header("Accept", accept).call().map_err(|e| http_err(what, e))?;
    let mut text = String::new();
    resp.body_mut().with_config().limit(limit).reader().read_to_string(&mut text).map_err(|e| Error::Plugin(format!("{what}: {e}")))?;
    Ok(text)
}

fn api_json(url: &str, what: &str) -> Result<serde_json::Value> {
    let text = api_get(url, what, "application/vnd.github+json", 4 * 1024 * 1024)?;
    serde_json::from_str(&text).map_err(|e| Error::Plugin(format!("{what}: GitHub answered with something unreadable ({e})")))
}

/// Is `sha` a commit of THIS repository? For a full commit sha GitHub answers from any fork in the
/// repository network under the parent's name, so `commits/<sha>` alone proves nothing. Accepted:
/// an ancestor of (or equal to) the default branch (`compare` says `behind` or `identical`), or
/// the commit a tag of the repo points to. Anything else is refused.
fn verify_commit_in_repo(ep: &Endpoints, spec: &SourceSpec, sha: &str) -> Result<()> {
    let base = format!("{}/repos/{}/{}", ep.api, spec.owner, spec.repo);
    let what = format!("checking that {sha} belongs to {}", spec.repo_id());
    let mut trouble: Option<String> = None;
    match api_json(&base, &what) {
        Ok(v) => {
            let branch = v["default_branch"].as_str().filter(|b| !b.is_empty() && b.len() <= 200 && b.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-/".contains(&c)) && !b.contains(".."));
            if let Some(b) = branch {
                match api_json(&format!("{base}/compare/{b}...{sha}"), &what) {
                    Ok(c) if matches!(c["status"].as_str(), Some("behind" | "identical")) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => trouble = Some(e.to_string()),
                }
            }
        }
        Err(e) => trouble = Some(e.to_string()),
    }
    match tag_commits(&base, &what) {
        Ok((tags, _)) if tags.iter().any(|t| t == sha) => return Ok(()),
        Ok((_, true)) => trouble = Some("the list of tags could not be read completely".into()),
        Ok(_) => {}
        Err(e) => trouble = Some(e.to_string()),
    }
    Err(Error::Plugin(format!(
        "cannot verify that commit {sha} belongs to {} (GitHub also serves commits of forks under the parent's name). \
         Use a release tag of the repository, or a commit that is part of its default branch{}",
        spec.repo_id(),
        trouble.map(|t| format!(" [{t}]")).unwrap_or_default()
    )))
}

/// Commits the repo's tags point to, and whether the list is complete. Tags are read page by page
/// (up to 10 pages of 100); annotated tags are peeled, following tag-of-tag chains up to 5 deep
/// with at most 200 lookups. A lookup that fails marks the answer incomplete instead of vanishing.
fn tag_commits(base: &str, what: &str) -> Result<(Vec<String>, bool)> {
    let (mut out, mut incomplete, mut lookups) = (Vec::new(), false, 0);
    for page in 1..=10 {
        let refs = api_json(&format!("{base}/git/matching-refs/tags?per_page=100&page={page}"), what)?;
        let list = refs.as_array().map(Vec::as_slice).unwrap_or_default();
        for r in list {
            let (Some(mut sha), Some(mut kind)) = (r["object"]["sha"].as_str().map(str::to_string), r["object"]["type"].as_str().map(str::to_string)) else { continue };
            for _ in 0..5 {
                if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    break;
                }
                if kind == "commit" {
                    out.push(sha.to_ascii_lowercase());
                    break;
                }
                if kind != "tag" {
                    break;
                }
                lookups += 1;
                if lookups > 200 {
                    incomplete = true;
                    break;
                }
                match api_json(&format!("{base}/git/tags/{sha}"), what) {
                    Ok(t) => match (t["object"]["sha"].as_str(), t["object"]["type"].as_str()) {
                        (Some(s), Some(k)) => (sha, kind) = (s.to_string(), k.to_string()),
                        _ => {
                            incomplete = true;
                            break;
                        }
                    },
                    Err(_) => {
                        incomplete = true;
                        break;
                    }
                }
            }
        }
        if list.len() < 100 {
            return Ok((out, incomplete));
        }
    }
    Ok((out, true)) // more than 1000 tags: not all were read
}

/// REF -> full commit sha, always verified to belong to the named repository (see
/// `verify_commit_in_repo`). A hex-looking name resolves as a tag or branch first.
pub fn resolve_ref(ep: &Endpoints, spec: &SourceSpec) -> Result<String> {
    // `pull/N/head` and friends name commits of other people's forks, `refs/...` is ambiguous
    if spec.reference.starts_with("pull/") || spec.reference.starts_with("refs/") {
        return Err(Error::Plugin("pull request and raw refs/ names are not installable; use a release tag or a commit of the default branch".into()));
    }
    let sha = if is_full_sha(&spec.reference) {
        spec.reference.clone()
    } else {
        let url = format!("{}/repos/{}/{}/commits/{}", ep.api, spec.owner, spec.repo, spec.reference);
        let text = api_get(&url, &format!("resolving {}@{}", spec.repo_id(), spec.reference), "application/vnd.github.sha", 1024)?;
        let sha = text.trim().to_ascii_lowercase();
        if !is_full_sha(&sha) {
            return Err(Error::Plugin("GitHub did not answer with a commit sha".into()));
        }
        sha
    };
    // every resolved commit is checked, whatever the ref looked like: a name can resolve to a
    // commit that is not part of this repository's own history
    verify_commit_in_repo(ep, spec, &sha)?;
    Ok(sha)
}

/// Resolve, download and extract. Nothing is installed.
pub fn fetch(ep: &Endpoints, spec: &SourceSpec) -> Result<Fetched> {
    let sha = resolve_ref(ep, spec)?;
    let url = format!("{}/{}/{}/tar.gz/{}", ep.codeload, spec.owner, spec.repo, sha);
    check_url("downloading the source", &url)?;
    let mut resp = agent(&url).get(&url).call().map_err(|e| http_err(&format!("downloading {}@{sha}", spec.repo_id()), e))?;
    let reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let tmp = tempfile::Builder::new().prefix("meetko-src-").tempdir()?;
    let dir = tmp.path().join("src");
    extract_tar_gz(reader, &dir, &Limits::default())?;
    Ok(Fetched { _tmp: tmp, dir, source: PluginSource { repo: spec.repo_id(), sha, reference: spec.reference.clone() } })
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_files: usize,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_bytes: MAX_EXTRACTED, max_files: MAX_FILES, max_depth: MAX_DEPTH }
    }
}

/// at most ~200 characters of a path in a message (a hostile path can be megabytes)
fn short(path: &Path) -> String {
    let t = path.display().to_string();
    let t = if t.chars().count() > 200 { format!("{}...", t.chars().take(200).collect::<String>()) } else { t };
    crate::text::escape(&t, false)
}

fn refuse(path: &Path, why: &str) -> Error {
    Error::Plugin(format!("unsafe archive entry {}: {why}", short(path)))
}

/// Sits between gzip and tar. It (1) caps the DECOMPRESSED stream, erroring when anything reads
/// past the cap, and (2) watches tar headers go by and refuses a PAX / GNU long-name extension
/// entry that claims more than `MAX_EXTENSION_ENTRY` bytes. The tar crate reads such an entry
/// fully into memory before yielding anything, so a few KB of gzip could otherwise ask for GBs.
struct Guard<R> {
    inner: R,
    left: u64,
    cap: u64,
    block: [u8; 512],
    filled: usize,
    skip: u64,
    data_left: u64,
    collecting: bool,
    collected: Vec<u8>,
    verdict: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}

/// `None` for a base-256 size field: `git archive` never writes one, and the tar crate and this
/// guard could disagree about it, so it is refused outright.
fn tar_size(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        return None;
    }
    let digits: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    let digits = digits.trim();
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

/// A PAX record `size=` changes how tar frames the next entry, so the guard (which follows the
/// framing itself) would lose sync. Refuse any extended header that has one, or is malformed.
fn pax_has_size(data: &[u8]) -> std::result::Result<bool, ()> {
    let mut rest = data;
    while !rest.is_empty() {
        let sp = rest.iter().position(|b| *b == b' ').ok_or(())?;
        let len: usize = std::str::from_utf8(&rest[..sp]).map_err(|_| ())?.parse().map_err(|_| ())?;
        if len <= sp + 1 || len > rest.len() {
            return Err(());
        }
        let rec = &rest[sp + 1..len];
        if rec.starts_with(b"size=") {
            return Ok(true);
        }
        rest = &rest[len..];
    }
    Ok(false)
}

impl<R> Guard<R> {
    fn fail<T>(&self, why: String) -> std::io::Result<T> {
        *self.verdict.borrow_mut() = Some(why.clone());
        Err(std::io::Error::other(why))
    }

    fn header(&mut self) -> std::io::Result<()> {
        let Some(size) = tar_size(&self.block[124..136]) else {
            return self.fail("a tar entry uses a base-256 size field".into());
        };
        let kind = self.block[156];
        if matches!(kind, b'x' | b'g' | b'L' | b'K') && size > MAX_EXTENSION_ENTRY {
            return self.fail(format!("a tar extension entry claims {size} bytes (more than {MAX_EXTENSION_ENTRY})"));
        }
        self.skip = size.saturating_add(511) / 512 * 512;
        self.data_left = size;
        self.collecting = matches!(kind, b'x' | b'g');
        self.collected.clear();
        Ok(())
    }

    fn finish_extension(&mut self) -> std::io::Result<()> {
        self.collecting = false;
        match pax_has_size(&self.collected) {
            Ok(false) => Ok(()),
            Ok(true) => self.fail("a tar extended header sets the entry size".into()),
            Err(()) => self.fail("a tar extended header is malformed".into()),
        }
    }

    /// follow the 512-byte block structure of what tar is being handed
    fn watch(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            if self.skip > 0 {
                let k = self.skip.min(data.len() as u64);
                if self.collecting {
                    let c = k.min(self.data_left) as usize;
                    self.collected.extend_from_slice(&data[..c]);
                    self.data_left -= c as u64;
                }
                self.skip -= k;
                data = &data[k as usize..];
                if self.skip == 0 && self.collecting {
                    self.finish_extension()?;
                }
                continue;
            }
            let take = (512 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 512 {
                self.filled = 0;
                self.header()?;
                if self.skip == 0 && self.collecting {
                    self.finish_extension()?;
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Guard<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // at the cap: fine if the stream ends here, an error if there is more
            let mut one = [0u8; 1];
            return match self.inner.read(&mut one)? {
                0 => Ok(0),
                _ => self.fail(format!("the archive unpacks to more than {} MB", self.cap / 1024 / 1024)),
            };
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        self.watch(&buf[..n])?;
        Ok(n)
    }
}

/// Strict extraction of a GitHub-style tarball (one top-level directory, which is stripped)
/// into `dest` (created, must not exist). Only regular files and directories are written;
/// absolute paths, `..`, links, device files and special entries are refused, as are archives
/// over the size, entry-count or depth limits, names Windows cannot hold, and streams that
/// inflate past the cap. File modes are normalised to 0644/0755.
pub fn extract_tar_gz(reader: impl Read, dest: &Path, limits: &Limits) -> Result<()> {
    if dest.exists() {
        return Err(Error::Plugin("extraction target already exists".into()));
    }
    util::create_private_dir(dest)?;
    let verdict = std::rc::Rc::new(std::cell::RefCell::new(None));
    let cap = limits.max_bytes.saturating_add(STREAM_SLACK);
    let guard = Guard { inner: flate2::read::GzDecoder::new(reader), left: cap, cap, block: [0; 512], filled: 0, skip: 0, data_left: 0, collecting: false, collected: Vec::new(), verdict: verdict.clone() };
    let result = extract_entries(guard, dest, limits);
    let why = verdict.borrow_mut().take();
    match (result, why) {
        (Err(_), Some(why)) => Err(Error::Plugin(why)),
        (r, _) => r,
    }
}

fn extract_entries(stream: impl Read, dest: &Path, limits: &Limits) -> Result<()> {
    let mut archive = tar::Archive::new(stream);
    let (mut total, mut seen) = (0u64, 0usize);
    let entries = archive.entries().map_err(|e| Error::Plugin(format!("not a readable tar.gz: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Plugin(format!("reading the archive: {e}")))?;
        // every entry counts, including directories and metadata, so a flood of empty folders stops too
        seen += 1;
        if seen > limits.max_files {
            return Err(Error::Plugin(format!("the archive has more than {} files (folders and metadata entries count too)", limits.max_files)));
        }
        let kind = entry.header().entry_type();
        let raw = entry.path().map_err(|e| Error::Plugin(format!("bad path in archive: {}", short(Path::new(&e.to_string())))))?.into_owned();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue; // metadata about the archive (GitHub puts the commit sha in one), not a file
        }
        if !(kind.is_file() || kind.is_dir()) {
            return Err(refuse(&raw, "only regular files and directories are allowed (no links or device files)"));
        }
        let mut parts = Vec::new();
        for c in raw.components() {
            match c {
                Component::Normal(p) => parts.push(p.to_owned()),
                Component::CurDir => {}
                _ => return Err(refuse(&raw, "absolute or parent path")),
            }
        }
        if parts.is_empty() {
            continue;
        }
        if parts.iter().any(|p| !portable_name(&p.to_string_lossy())) {
            return Err(refuse(&raw, "a path part that is not portable (reserved Windows name, trailing dot or space, or a character such as \\ : * ? \" < > |)"));
        }
        // the first component is the repo's top directory
        let rest = &parts[1..];
        if rest.is_empty() {
            if kind.is_file() {
                return Err(refuse(&raw, "a file outside the top directory"));
            }
            continue;
        }
        if rest.len() > limits.max_depth {
            return Err(refuse(&raw, "too deep"));
        }
        let mut out = dest.to_path_buf();
        out.extend(rest);
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > limits.max_bytes {
            return Err(Error::Plugin(format!("the archive is larger than {} MB unpacked", limits.max_bytes / 1024 / 1024)));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exec = entry.header().mode().map(|m| m & 0o111 != 0).unwrap_or(false);
        let mut f = util::create_private_new(&out).map_err(|e| Error::Plugin(format!("{}: {e} (duplicate entry?)", short(&out))))?;
        std::io::copy(&mut (&mut entry).take(size), &mut f)?;
        drop(f);
        set_mode(&out, exec)?;
    }
    Ok(())
}

/// A path part every OS we ship on can hold: no Windows device names (with any extension), no
/// trailing dot or space (Windows drops them, so `x.` and `x` would collide, and `...` is nothing),
/// none of the characters Windows forbids, no control characters.
fn portable_name(part: &str) -> bool {
    if part.is_empty() || part.ends_with(['.', ' ']) || part.chars().any(|c| c.is_control() || "\\:*?\"<>|".contains(c)) {
        return false;
    }
    let stem = part.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |p: &str| stem.strip_prefix(p).is_some_and(|n| matches!(n.chars().collect::<Vec<_>>()[..], [c] if c.is_ascii_digit() || "\u{b9}\u{b2}\u{b3}".contains(c)));
    !(matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT"))
}

#[cfg(unix)]
fn set_mode(p: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: bool) -> Result<()> {
    Ok(())
}

// ------------------------------------------------------------------ gallery (browse)

/// Where the app looks for the gallery index unless MEETKO_PLUGIN_INDEX_URL says otherwise.
/// The owner's gallery repo, built as tools/plugin-index/gallery-workflow.yml shows.
pub const DEFAULT_INDEX_URL: &str = "https://raw.githubusercontent.com/Eastsidegunn/meetko-plugins/main/index.json";
pub const MAX_INDEX_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_INDEX_ENTRIES: usize = 2000;

pub fn index_url() -> String {
    std::env::var("MEETKO_PLUGIN_INDEX_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_INDEX_URL.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GalleryPermissions {
    pub network: bool,
    pub audio: bool,
}

/// One row of `index.json` (tools/plugin-index/index.schema.json). Declared facts about a
/// third-party repo, not a review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GalleryEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub description: String,
    /// OWNER/REPO
    pub repo: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// the commit the entry was read at; installs pin it
    pub sha: String,
    pub permissions: GalleryPermissions,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub stars: u64,
    #[serde(default)]
    pub updated: String,
}

impl GalleryEntry {
    /// The spec this entry installs from: always the pinned commit, never a moving tag.
    pub fn spec(&self) -> Result<SourceSpec> {
        SourceSpec::parse(&format!("github:{}@{}", self.repo, self.sha))?.ok_or_else(|| Error::Plugin("bad gallery entry".into()))
    }

    /// Shape checks on a row of a file anyone may have tampered with: the id looks like a manifest
    /// id, every string is short, and nothing shown to the user carries control or bidi characters.
    fn sane(&self) -> bool {
        let shown_ok = [(&self.name, 64), (&self.version, 64), (&self.description, 300), (&self.reference, 200), (&self.runtime, 32), (&self.license, 64), (&self.updated, 64)]
            .iter()
            .all(|(s, max)| plain_text(s, *max));
        is_full_sha(&self.sha) && valid_plugin_id(&self.id) && self.spec().is_ok() && self.repo.len() <= 201 && shown_ok
    }
}

/// `publisher/name`: lowercase letters, digits, `.`, `_`, `-`, each part 1-64 characters starting
/// with a letter or digit (the same shape the index builder enforces).
pub fn valid_plugin_id(id: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty() && p.len() <= 64 && p.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
    };
    id.split_once('/').is_some_and(|(a, b)| part(a) && part(b))
}

/// Short text without control characters (newlines included) or characters that reorder or hide text.
fn plain_text(s: &str, max_chars: usize) -> bool {
    s.chars().count() <= max_chars && crate::text::is_plain_text(s)
}

/// Download and parse an index. Entries that are malformed are dropped, not repaired.
pub fn fetch_index(url: &str) -> Result<Vec<GalleryEntry>> {
    let what = "fetching the plugin index";
    check_url(what, url)?;
    let mut resp = match agent(url).get(url).header("Accept", "application/json").call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Err(Error::GalleryUnavailable("the index was not found (404); the gallery may not be open yet".into())),
        Err(e) => return Err(http_err(what, e)),
    };
    let mut text = String::new();
    resp.body_mut().with_config().limit(MAX_INDEX_BYTES).reader().read_to_string(&mut text).map_err(|e| Error::Plugin(format!("{what}: {e}")))?;
    parse_index(&text)
}

pub fn parse_index(text: &str) -> Result<Vec<GalleryEntry>> {
    #[derive(serde::Deserialize)]
    struct Doc {
        plugins: Vec<serde_json::Value>,
    }
    let doc: Doc = serde_json::from_str(text).map_err(|e| Error::Plugin(format!("the plugin index is not valid: {e}")))?;
    Ok(doc
        .plugins
        .into_iter()
        .take(MAX_INDEX_ENTRIES)
        .filter_map(|v| serde_json::from_value::<GalleryEntry>(v).ok())
        .filter(GalleryEntry::sane)
        .fold(Vec::<GalleryEntry>::new(), |mut v, e| {
            if !v.iter().any(|o| o.id == e.id) {
                v.push(e); // one row per id: the first wins, so an id names exactly one source
            }
            v
        }))
}
