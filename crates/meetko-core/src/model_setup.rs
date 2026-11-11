//! Models the app keeps for the user: the pinned models it can download from their publisher, or
//! copy from a file the user already has, into an app-managed folder of the data dir
//! (`<data dir>/models/<model id>/<file>`). `live::LiveConfig::from_env` and
//! `diarize::DiarConfig::from_env` look there when their environment variable is unset, so a user
//! gets from zero to live captions without variables or config files.
//!
//! Only model *data* lives here. Executables (engines, whisper-cli, capture) are never located
//! through the data dir (see `capture_run::pick_bin`). A file in this folder is still only a
//! candidate: every use hashes it against the pins in `engines/*/models.toml`, so a file that was
//! swapped afterwards is refused, exactly like a directory named by `MEETKO_STREAM_MODEL`.
//!
//! Network: only when the user clicks, only HTTPS (plain HTTP only toward a loopback host, for the
//! tests), only to the hosts of the offer's allow-list on every redirect hop (redirects are followed
//! here, not by the HTTP client), never more bytes than the pinned size, and the file is renamed
//! into place only after its sha256 matched. A `.part` file is resumed with a `Range` request.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::live::{self, ROLE_BATCH, ROLE_DIARIZATION, ROLE_STREAM};
use crate::util;

/// The folder under the data dir that holds app-managed models.
pub const MODELS_DIR: &str = "models";

/// Hugging Face answers on its own host and redirects the file to its CDN (`*.hf.co`, older files
/// `cdn-lfs*.huggingface.co`). An entry starting with `.` matches any subdomain of it.
pub const HF_HOSTS: &[&str] = &["huggingface.co", ".hf.co", ".huggingface.co"];

/// A pinned model file the app can fetch from its publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// stable key for the UI: `stream` | `diarization` | `qwen3-asr` | `qwen3-aligner`
    pub key: &'static str,
    /// the role of the engine that loads it (`live::ROLE_*`)
    pub role: &'static str,
    /// the pin id in `models.toml`; also the folder name under `models/`
    pub model_id: &'static str,
    /// the file name inside the model directory (the key of the pin)
    pub file: &'static str,
    /// Hugging Face repository and the exact commit the URL is pinned to
    pub repo: &'static str,
    pub revision: &'static str,
    /// the file name in that repository
    pub remote_file: &'static str,
    /// bytes, as published at `revision` (checked again by the sha256)
    pub size: u64,
    pub license: &'static str,
    pub publisher: &'static str,
}

/// What the app offers. Every file was checked against the pins on 2026-11-10: Hugging Face's
/// `paths-info` (the model API's LFS sha256) for these revisions reports exactly the pinned sha256 and
/// these sizes. The two Qwen3 files make up one model directory (one pin, two offers): the batch
/// engine needs both, the transcript model and the forced aligner that times its words. They are the
/// GGUF conversions by CrispASR's author (Qwen publishes no GGUF), which the UI says.
pub const OFFERS: [Offer; 4] = [
    Offer {
        key: "stream",
        role: ROLE_STREAM,
        model_id: "nemotron-3.5-asr-streaming-0.6b-q8_0",
        file: "model.q8_0.gguf",
        repo: "nvidia/nemotron-3.5-asr-streaming-0.6b",
        revision: "ea30d66debe3740a08b573244286791d423d6b3e",
        remote_file: "nemotron-3.5-asr-streaming-0.6b.q8_0.gguf",
        size: 742_090_464,
        license: "OpenMDW-1.1",
        publisher: "NVIDIA",
    },
    Offer {
        key: "diarization",
        role: ROLE_DIARIZATION,
        model_id: "nemotron-3-diarization-q8_0",
        file: "Nemotron-3-Diarization.q8_0.gguf",
        repo: "nvidia/Nemotron-3-Diarization",
        revision: "f667ed73aee57d40cc39428eb768b4fd87a0a29e",
        remote_file: "Nemotron-3-Diarization.q8_0.gguf",
        size: 107_012_128,
        license: "OpenMDW-1.1",
        publisher: "NVIDIA",
    },
    Offer {
        key: "qwen3-asr",
        role: ROLE_BATCH,
        model_id: "qwen3-asr-1.7b-crispasr-q8_0",
        file: "qwen3-asr-1.7b-q8_0.gguf",
        repo: "cstr/qwen3-asr-1.7b-GGUF",
        revision: "674df5d44b50a63e7102a18895ed20e3f91de301",
        remote_file: "qwen3-asr-1.7b-q8_0.gguf",
        size: 2_506_723_200,
        license: "Apache-2.0",
        publisher: "Qwen (GGUF 변환: CrispASR)",
    },
    Offer {
        key: "qwen3-aligner",
        role: ROLE_BATCH,
        model_id: "qwen3-asr-1.7b-crispasr-q8_0",
        file: "qwen3-forced-aligner-0.6b-q8_0.gguf",
        repo: "cstr/qwen3-forced-aligner-0.6b-GGUF",
        revision: "1ec5110602ccab18c878ddebedab0891e290a95c",
        remote_file: "qwen3-forced-aligner-0.6b-q8_0.gguf",
        size: 985_594_624,
        license: "Apache-2.0",
        publisher: "Qwen (GGUF 변환: CrispASR)",
    },
];

pub fn offer(key: &str) -> Option<&'static Offer> {
    OFFERS.iter().find(|o| o.key == key)
}

impl Offer {
    /// The publisher's file at the pinned commit (never a branch name).
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, self.remote_file)
    }

    /// The pinned sha256 of the file, from `models.toml` (the one source of truth).
    pub fn sha256(&self) -> &'static str {
        live::pinned_file_sha256(self.model_id, self.file).expect("every offer names a pinned file")
    }

    /// What the download must deliver.
    pub fn want(&self) -> Want<'static> {
        Want { size: self.size, sha256: self.sha256() }
    }

    pub fn source(&self) -> Source<'static> {
        Source { url: self.url(), hosts: HF_HOSTS, loopback_http_for_tests: false }
    }

    /// The model directory the app manages for this offer.
    pub fn dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(MODELS_DIR).join(self.model_id)
    }

    /// Where the finished file lives.
    pub fn path(&self, data_dir: &Path) -> PathBuf {
        self.dir(data_dir).join(self.file)
    }

    /// Bytes of an interrupted download that the next one resumes from (0 when there is none).
    pub fn partial(&self, data_dir: &Path) -> u64 {
        std::fs::metadata(part_path(&self.path(data_dir))).map(|m| m.len().min(self.size)).unwrap_or(0)
    }

    /// This offer's own file in the app's folder: missing, or present and matching its pin, or present
    /// and not (`Bad`). Judged per file, so the second file of a two-file model is not "bad" just
    /// because the first is not there yet. The hash is cached (size, mtime, inode), as for the status.
    pub fn file_state(&self, data_dir: &Path) -> FileState {
        let path = self.path(data_dir);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return if path.exists() { FileState::Bad } else { FileState::Missing };
        }
        match live::cached_sha256(&path, &data_dir.join("model-sha256-cache.json")) {
            Ok(sha) if sha == self.sha256() => FileState::Ready,
            _ => FileState::Bad,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Missing,
    Ready,
    Bad,
}

/// The app-managed model directory for `role` when it holds the file of an offer. Whether the file
/// matches its pin is decided by whoever loads it (`live::check_model_for`, staging).
pub fn managed_model_dir(data_dir: &Path, role: &str) -> Option<PathBuf> {
    OFFERS.iter().filter(|o| o.role == role).find(|o| o.path(data_dir).is_file()).map(|o| o.dir(data_dir))
}

/// The size and sha256 a fetched or copied file must have.
#[derive(Debug, Clone, Copy)]
pub struct Want<'a> {
    pub size: u64,
    pub sha256: &'a str,
}

/// Where to fetch from: the first URL and the hosts every hop (the first included) must be on.
#[derive(Debug, Clone)]
pub struct Source<'a> {
    pub url: String,
    pub hosts: &'a [&'a str],
    /// Tests only: also accept plain http to a loopback host, on any port. Every offer has it off,
    /// so the app itself only ever speaks https on the default port.
    pub loopback_http_for_tests: bool,
}

const MAX_HOPS: usize = 5;
const BUF: usize = 1 << 20;

fn fail(code: &'static str, message: impl Into<String>) -> Error {
    Error::Download(code, message.into())
}

/// Disk full is its own message in the UI; other I/O errors keep their text.
fn io_fail(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(28) || e.raw_os_error() == Some(112) {
        fail("disk_full", "not enough free disk space")
    } else {
        Error::Io(e)
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

/// Where an import is copied before it is verified: never the download's `.part`, so picking a
/// file does not wipe a download that is half done.
fn import_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".import");
    dest.with_file_name(name)
}

fn host_allowed(host: &str, hosts: &[&str]) -> bool {
    let host = host.to_ascii_lowercase();
    hosts.iter().any(|h| match h.strip_prefix('.') {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => host == *h,
    })
}

fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether `url` may be fetched: https on the default port to an allowed host, or (only with the
/// test switch) plain http to an allowed loopback host. No credentials in the URL.
pub fn check_hop(url: &str, hosts: &[&str], loopback_http_for_tests: bool) -> Result<()> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| fail("host_not_allowed", "not an absolute URL"))?;
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    if auth.is_empty() || auth.contains('@') {
        return Err(fail("host_not_allowed", "the URL needs a plain host"));
    }
    let (host, port) = match auth.strip_prefix('[') {
        Some(r) => {
            let (h, after) = r.split_once(']').unwrap_or((r, ""));
            (h, after.strip_prefix(':'))
        }
        None => match auth.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (auth, None),
        },
    };
    let https = scheme.eq_ignore_ascii_case("https");
    let test_http = loopback_http_for_tests && scheme.eq_ignore_ascii_case("http") && is_loopback(host);
    if !https && !test_http {
        return Err(fail("host_not_allowed", format!("{scheme}:// is not allowed (https only)")));
    }
    if https && port.is_some_and(|p| p != "443") {
        return Err(fail("host_not_allowed", "only the default https port is allowed"));
    }
    if !host_allowed(host, hosts) {
        return Err(fail("host_not_allowed", format!("{host} is not one of the publisher's hosts")));
    }
    Ok(())
}

/// True when `location` starts with a URL scheme (`https://…`), not when `://` only appears later,
/// say in a query of a relative reference.
fn has_scheme(location: &str) -> bool {
    let Some(i) = location.find("://") else { return false };
    let s = &location[..i];
    !s.is_empty() && s.bytes().enumerate().all(|(j, b)| b.is_ascii_alphabetic() || (j > 0 && (b.is_ascii_digit() || b"+-.".contains(&b))))
}

/// A redirect target, made absolute against the URL that answered.
fn absolute(base: &str, location: &str) -> String {
    if has_scheme(location) {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    if location.starts_with("//") {
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        format!("{scheme}://{auth}{location}")
    } else {
        let path = base.split(['?', '#']).next().unwrap_or(base);
        let dir = path.rsplit_once('/').map_or(path, |(d, _)| d);
        format!("{dir}/{location}")
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0) // every hop is checked against the allow-list here
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(30))) // a stalled link fails; a slow one may take its time
        .accept_encoding("identity")
        .user_agent("meetko-model-setup")
        .build()
        .into()
}

fn net_fail(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => io_fail(io),
        e => fail("network", e.to_string()),
    }
}

/// Free bytes on the volume of `dir` (unix; `None` when unknown).
fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: a zeroed statvfs is a valid out-parameter; the path is NUL-terminated
        unsafe {
            let mut s: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut s) == 0 {
                return Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

fn check_space(dir: &Path, need: u64) -> Result<()> {
    match free_space(dir) {
        Some(free) if free < need => Err(fail("disk_full", format!("{need} bytes are needed, {free} are free"))),
        _ => Ok(()),
    }
}

/// Make the folder of `dest` and check it and its parent (`models/`): real directories, not links
/// (a link would send the file somewhere else), and private (tightened when they are ours and loose).
fn prepare_dir(dest: &Path) -> Result<&Path> {
    let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).ok_or_else(|| fail("io", "no folder for the model"))?;
    let both = [dir.parent().filter(|p| !p.as_os_str().is_empty()), Some(dir)];
    let real_dir = |d: &Path| std::fs::symlink_metadata(d).map(|m| m.file_type().is_dir());
    // before creating anything: an existing link is refused, so nothing is made through it
    for d in both.into_iter().flatten() {
        if matches!(real_dir(d), Ok(false)) {
            return Err(fail("io", "the model folder is a link or not a folder"));
        }
    }
    util::create_private_dir(dir).map_err(io_fail)?;
    for d in both.into_iter().flatten() {
        if !real_dir(d).map_err(io_fail)? {
            return Err(fail("io", "the model folder is a link or not a folder"));
        }
        util::tighten_private_dir(d).map_err(io_fail)?;
    }
    Ok(dir)
}

/// One writer per `.part`, also across processes (a second app window, the CLI). Released when the
/// file is closed. Unix only.
fn lock_part(f: &std::fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on a descriptor we own; non-blocking
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(fail("busy", "this model is already being prepared"));
        }
    }
    #[cfg(not(unix))]
    let _ = f;
    Ok(())
}

/// Progress: bytes so far, bytes in all.
pub type OnProgress<'a> = &'a dyn Fn(u64, u64);

/// Hash what is already in `file` (a resumed `.part`), reporting progress.
fn hash_existing(file: &mut std::fs::File, len: u64, h: &mut sha2::Sha256, total: u64, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    file.seek(SeekFrom::Start(0)).map_err(io_fail)?;
    let (mut buf, mut done) = (vec![0u8; BUF], 0u64);
    while done < len {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let n = file.read(&mut buf[..BUF.min((len - done) as usize)]).map_err(io_fail)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(h, &buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    Ok(())
}

/// Move a verified `.part` into place (private file, synced first, then renamed).
fn finish(part: &Path, dest: &Path, file: std::fs::File, h: sha2::Sha256, want: &Want) -> Result<()> {
    let got = util::hex(&sha2::Digest::finalize(h));
    if got != want.sha256 {
        drop(file);
        let _ = std::fs::remove_file(part);
        return Err(fail("checksum_mismatch", "the file does not match its pinned sha256; it was removed"));
    }
    file.sync_all().map_err(io_fail)?;
    // unix: rename while the lock is still held, then make the rename itself durable. Windows cannot
    // rename a file it holds open, and has no lock to keep.
    #[cfg(not(unix))]
    drop(file);
    std::fs::rename(part, dest).map_err(io_fail)?;
    #[cfg(unix)]
    {
        if let Some(dir) = dest.parent() {
            std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(io_fail)?;
        }
        drop(file);
    }
    Ok(())
}

fn open_part(part: &Path) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW); // never write through a link planted in the folder
    }
    let f = o.open(part).map_err(|e| {
        // O_NOFOLLOW on a link: ELOOP (macOS, Linux)
        if e.raw_os_error() == Some(62) || e.raw_os_error() == Some(40) {
            fail("io", "the partial download is a link")
        } else {
            io_fail(e)
        }
    })?;
    if !f.metadata().map_err(io_fail)?.is_file() {
        return Err(fail("io", "the partial download is not a regular file"));
    }
    lock_part(&f)?;
    Ok(f)
}

/// Fetch `src` into `dest` (a file path; its folder is made private), resuming a `<dest>.part`
/// left by an earlier try. A cancel or a broken connection keeps the `.part` for the next try; a
/// file that is too big or does not match the sha256 is removed. `dest` appears only verified.
pub fn download(src: &Source, want: &Want, dest: &Path, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    let dir = prepare_dir(dest)?;
    let part = part_path(dest);
    let mut file = open_part(&part)?;
    let mut have = file.metadata().map_err(io_fail)?.len();
    if have > want.size {
        file.set_len(0).map_err(io_fail)?;
        have = 0;
    }
    check_space(dir, want.size - have)?;
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    hash_existing(&mut file, have, &mut h, want.size, progress, cancel)?;
    if have == want.size {
        return finish(&part, dest, file, h, want);
    }

    let agent = agent();
    let mut url = src.url.clone();
    let mut hops = 0;
    let resp = loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        check_hop(&url, src.hosts, src.loopback_http_for_tests)?;
        let mut req = agent.get(&url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let resp = req.call().map_err(net_fail)?;
        let status = resp.status().as_u16();
        match status {
            301 | 302 | 303 | 307 | 308 => {
                hops += 1;
                if hops > MAX_HOPS {
                    return Err(fail("network", "too many redirects"));
                }
                let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| fail("network", "a redirect without a location"))?;
                url = absolute(&url, loc);
            }
            200 | 206 => break resp,
            416 if have > 0 => {
                // the server will not resume from here: start over
                file.set_len(0).map_err(io_fail)?;
                have = 0;
                h = <sha2::Sha256 as sha2::Digest>::new();
                hops = 0;
                url = src.url.clone();
            }
            s => return Err(fail("network", format!("the server answered {s}"))),
        }
    };
    if resp.status().as_u16() == 200 && have > 0 {
        // the server sent the whole file instead of the rest: start the file over
        file.set_len(0).map_err(io_fail)?;
        have = 0;
        h = <sha2::Sha256 as sha2::Digest>::new();
        check_space(dir, want.size)?;
    }
    if resp.status().as_u16() == 206 {
        let ok = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).is_some_and(|r| r.starts_with(&format!("bytes {have}-")));
        if !ok {
            return Err(fail("network", "the server resumed at the wrong place"));
        }
    }
    let left = want.size - have;
    if let Some(len) = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
        if len > left {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(fail("too_big", format!("the server offers {len} bytes, {left} were expected")));
        }
    }
    file.seek(SeekFrom::Start(have)).map_err(io_fail)?;
    let mut body = resp.into_body().into_reader();
    let mut buf = vec![0u8; BUF];
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = file.sync_all();
            return Err(Error::Cancelled);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = file.sync_all(); // keep what arrived for the next try
                return Err(fail("network", e.to_string()));
            }
        };
        if have + n as u64 > want.size {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(fail("too_big", "the server sent more than the pinned size; the file was removed"));
        }
        file.write_all(&buf[..n]).map_err(io_fail)?;
        sha2::Digest::update(&mut h, &buf[..n]);
        have += n as u64;
        progress(have, want.size);
    }
    if have != want.size {
        let _ = file.sync_all();
        return Err(fail("network", format!("the download stopped at {have} of {} bytes", want.size)));
    }
    finish(&part, dest, file, h, want)
}

/// Copy a file the user already has into `dest`, hashing it on the way. A file of another size is
/// refused before anything is copied; one that does not match the sha256 leaves nothing behind.
/// The source is only read.
pub fn import_file(src: &Path, want: &Want, dest: &Path, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    // a FIFO or a device would block the open or the read: only a regular file (a link to one is
    // fine, the user picked it) is opened, and without blocking
    if !std::fs::metadata(src).map(|m| m.is_file()).unwrap_or(false) {
        return Err(fail("wrong_file", "not a regular file"));
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NONBLOCK);
    }
    let mut from = o.open(src)?;
    let meta = from.metadata()?;
    if !meta.is_file() {
        return Err(fail("wrong_file", "not a regular file"));
    }
    if meta.len() != want.size {
        return Err(fail("wrong_file", format!("the file has {} bytes, the model has {}", meta.len(), want.size)));
    }
    copy_in(&mut from, want, dest, progress, cancel)
}

/// The copy of `import_file` from any reader: at most `want.size` bytes, hashed, then verified and
/// renamed into place. Anything else (cancel, too long, wrong hash) leaves nothing behind.
fn copy_in(from: &mut dyn Read, want: &Want, dest: &Path, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    let dir = prepare_dir(dest)?;
    check_space(dir, want.size)?;
    let part = import_path(dest);
    let mut to = open_part(&part)?; // locked: one import of this model at a time
    to.set_len(0).map_err(io_fail)?; // an import never resumes
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    let (mut buf, mut done) = (vec![0u8; BUF], 0u64);
    let res = (|| -> Result<()> {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let n = from.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            done += n as u64;
            if done > want.size {
                return Err(fail("wrong_file", "the file grew while it was copied"));
            }
            to.write_all(&buf[..n]).map_err(io_fail)?;
            sha2::Digest::update(&mut h, &buf[..n]);
            progress(done, want.size);
        }
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&part);
        drop(to);
        return Err(e);
    }
    finish(&part, dest, to, h, want).map_err(|e| match e {
        Error::Download("checksum_mismatch", _) => fail("wrong_file", "the file is not the pinned model"),
        e => e,
    })
}

/// The hash cache the status checks read (`live::check_model_for`): a file that was just verified
/// is not hashed again for the first status after the download.
fn remember(data_dir: &Path, o: &Offer) {
    live::remember_sha256(&o.path(data_dir), o.sha256(), &data_dir.join("model-sha256-cache.json"));
}

/// `download` of an offer from its publisher into the app-managed folder.
/// A file that is already in place and matches its pin is not fetched again (no network).
pub fn install(o: &Offer, data_dir: &Path, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    if o.file_state(data_dir) == FileState::Ready {
        progress(o.size, o.size);
        return Ok(());
    }
    download(&o.source(), &o.want(), &o.path(data_dir), progress, cancel)?;
    remember(data_dir, o);
    Ok(())
}

/// `import_file` of a file the user picked into the app-managed folder of an offer.
pub fn install_from(o: &Offer, data_dir: &Path, src: &Path, progress: OnProgress, cancel: &AtomicBool) -> Result<()> {
    import_file(src, &o.want(), &o.path(data_dir), progress, cancel)?;
    remember(data_dir, o);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_are_pinned_files_at_exact_revisions() {
        for o in &OFFERS {
            assert!(util::is_sha256(o.sha256()), "{}", o.key);
            assert!(o.revision.len() == 40 && o.revision.bytes().all(|b| b.is_ascii_hexdigit()), "{}: a commit, not a branch", o.key);
            check_hop(&o.url(), HF_HOSTS, false).unwrap();
            let publisher = if o.role == ROLE_BATCH { "cstr/" } else { "nvidia/" };
            assert!(o.url().starts_with(&format!("https://huggingface.co/{publisher}")), "{}", o.url());
            assert!(o.size > 0);
        }
        assert_eq!(offer("stream").unwrap().role, ROLE_STREAM);
        assert_eq!(offer("diarization").unwrap().role, ROLE_DIARIZATION);
        // the two files of the Qwen3 batch model share its one directory and pin
        let (asr, al) = (offer("qwen3-asr").unwrap(), offer("qwen3-aligner").unwrap());
        assert_eq!((asr.role, al.role), (ROLE_BATCH, ROLE_BATCH));
        assert_eq!(asr.model_id, al.model_id);
        assert_ne!(asr.file, al.file);
        assert!(offer("whisper").is_none());
    }

    #[test]
    fn hops_go_to_the_allowed_hosts_only() {
        for ok in ["https://huggingface.co/x", "https://us.aws.cdn.hf.co/a?b", "https://cas-bridge.xethub.hf.co/a", "https://cdn-lfs.huggingface.co/x", "HTTPS://HuggingFace.co/x"] {
            check_hop(ok, HF_HOSTS, false).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "http://huggingface.co/x",
            "https://huggingface.co.evil.example/x",
            "https://evilhf.co/x",
            "https://hf.co/x",
            "https://user@huggingface.co/x",
            "https://github.com/x",
            "ftp://huggingface.co/x",
            "/relative",
            "http://127.0.0.1:9/x",
        ] {
            assert!(matches!(check_hop(bad, HF_HOSTS, false), Err(Error::Download("host_not_allowed", _))), "{bad}");
        }
        // plain http only with the test switch, only toward loopback, only when the host is listed
        check_hop("http://127.0.0.1:9/x", &["127.0.0.1"], true).unwrap();
        assert!(check_hop("http://127.0.0.1:9/x", &["127.0.0.1"], false).is_err(), "never in the app");
        assert!(check_hop("http://10.0.0.1/x", &["10.0.0.1"], true).is_err());
        // https only on the default port
        check_hop("https://huggingface.co:443/x", HF_HOSTS, false).unwrap();
        for bad in ["https://huggingface.co:8443/x", "https://us.aws.cdn.hf.co:80/x", "https://huggingface.co:/x"] {
            assert!(check_hop(bad, HF_HOSTS, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn redirect_targets_are_made_absolute() {
        assert_eq!(absolute("https://a.example/x/y", "https://b.example/z"), "https://b.example/z");
        assert_eq!(absolute("https://a.example/x/y", "/z?q"), "https://a.example/z?q");
        assert_eq!(absolute("https://a.example/x/y", "//c.example/z"), "https://c.example/z");
        assert_eq!(absolute("https://a.example/x/y", "z"), "https://a.example/x/z");
        // "://" inside a relative reference does not make it absolute
        assert_eq!(absolute("https://a.example/x/y", "/r?u=https://evil.example/"), "https://a.example/r?u=https://evil.example/");
        assert_eq!(absolute("https://a.example/x/y", "z?u=https://evil.example/"), "https://a.example/x/z?u=https://evil.example/");
        assert_eq!(absolute("https://a.example/x/y?q=a/b", "z"), "https://a.example/x/z");
    }

    #[test]
    fn the_managed_dir_is_found_only_with_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(managed_model_dir(tmp.path(), ROLE_STREAM).is_none());
        let o = offer("stream").unwrap();
        std::fs::create_dir_all(o.dir(tmp.path())).unwrap();
        assert!(managed_model_dir(tmp.path(), ROLE_STREAM).is_none(), "an empty folder is not a model");
        std::fs::write(part_path(&o.path(tmp.path())), b"abc").unwrap();
        assert!(managed_model_dir(tmp.path(), ROLE_STREAM).is_none(), "a partial download is not a model");
        assert_eq!(o.partial(tmp.path()), 3);
        std::fs::write(o.path(tmp.path()), b"x").unwrap();
        assert_eq!(managed_model_dir(tmp.path(), ROLE_STREAM), Some(o.dir(tmp.path())));
        assert!(managed_model_dir(tmp.path(), ROLE_DIARIZATION).is_none());
    }

    #[test]
    fn disk_full_is_its_own_code() {
        assert!(matches!(io_fail(std::io::Error::from(std::io::ErrorKind::StorageFull)), Error::Download("disk_full", _)));
        assert!(matches!(io_fail(std::io::Error::from_raw_os_error(28)), Error::Download("disk_full", _)), "ENOSPC");
        assert!(matches!(io_fail(std::io::Error::from(std::io::ErrorKind::PermissionDenied)), Error::Io(_)));
        assert!(check_space(Path::new("/"), u64::MAX).is_err_and(|e| matches!(e, Error::Download("disk_full", _))) || free_space(Path::new("/")).is_none());
    }

    #[test]
    fn a_file_that_grows_while_it_is_copied_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models/m/f");
        let data = vec![7u8; 1000];
        let sha = util::sha256_bytes(&data[..900]);
        // the size was checked before the copy; the reader then gives more than that
        let e = copy_in(&mut std::io::Cursor::new(data), &Want { size: 900, sha256: &sha }, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Download("wrong_file", _)), "{e}");
        assert!(!dest.exists() && !import_path(&dest).exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_in_the_model_folders_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let data = b"abc";
        let sha = util::sha256_bytes(data);
        let want = Want { size: 3, sha256: &sha };
        // models/ is a link
        let d1 = tmp.path().join("d1");
        std::fs::create_dir(&d1).unwrap();
        std::os::unix::fs::symlink(&elsewhere, d1.join("models")).unwrap();
        let e = copy_in(&mut &data[..], &want, &d1.join("models/m/f"), &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Download("io", _)), "{e}");
        // the model folder is a link
        let d2 = tmp.path().join("d2/models");
        std::fs::create_dir_all(&d2).unwrap();
        std::os::unix::fs::symlink(&elsewhere, d2.join("m")).unwrap();
        assert!(copy_in(&mut &data[..], &want, &d2.join("m/f"), &|_, _| {}, &AtomicBool::new(false)).is_err());
        // the .part is a link
        let d3 = tmp.path().join("d3/models/m");
        std::fs::create_dir_all(&d3).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("target"), d3.join("f.import")).unwrap();
        let e = copy_in(&mut &data[..], &want, &d3.join("f"), &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Download("io", _)), "{e}");
        assert!(!elsewhere.join("target").exists() && std::fs::read_dir(&elsewhere).unwrap().next().is_none(), "nothing was written through a link");
        // a loose folder of ours is tightened
        use std::os::unix::fs::PermissionsExt;
        let d4 = tmp.path().join("d4/models/m");
        std::fs::create_dir_all(&d4).unwrap();
        std::fs::set_permissions(&d4, std::fs::Permissions::from_mode(0o755)).unwrap();
        copy_in(&mut &data[..], &want, &d4.join("f"), &|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::metadata(&d4).unwrap().permissions().mode() & 0o077, 0);
    }

    #[cfg(unix)]
    #[test]
    fn one_writer_per_part() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("f.part");
        let held = open_part(&part).unwrap();
        #[cfg(unix)]
        {
            // the lock is held through the rename (finish keeps the file open until after it)
            let dest = tmp.path().join("f");
            let sha = util::sha256_bytes(b"");
            let mut h2 = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(tmp.path().join("g.part")).unwrap();
            lock_part(&h2).unwrap();
            h2.write_all(b"").unwrap();
            finish(&tmp.path().join("g.part"), &tmp.path().join("g"), h2, <sha2::Sha256 as sha2::Digest>::new(), &Want { size: 0, sha256: &sha }).unwrap();
            assert!(tmp.path().join("g").exists() && !dest.exists());
        }
        assert!(matches!(open_part(&part), Err(Error::Download("busy", _))));
        drop(held);
        open_part(&part).unwrap();
    }

    #[test]
    fn an_import_leaves_a_half_done_download_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models/m/f");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(part_path(&dest), b"half").unwrap();
        let sha = util::sha256_bytes(b"other");
        assert!(copy_in(&mut &b"wrong"[..], &Want { size: 5, sha256: &sha }, &dest, &|_, _| {}, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(part_path(&dest)).unwrap(), b"half");
        copy_in(&mut &b"other"[..], &Want { size: 5, sha256: &sha }, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(part_path(&dest)).unwrap(), b"half", "still there to resume or remove");
    }

    #[cfg(unix)]
    #[test]
    fn an_import_refuses_a_fifo_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("pipe.gguf");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated path
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let sha = util::sha256_bytes(b"x");
        let e = import_file(&fifo, &Want { size: 1, sha256: &sha }, &tmp.path().join("m/f"), &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Download("wrong_file", _)), "{e}");
    }

    #[test]
    fn each_file_is_judged_on_its_own_and_a_ready_one_is_not_fetched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (asr, al) = (offer("qwen3-asr").unwrap(), offer("qwen3-aligner").unwrap());
        assert_eq!((asr.file_state(tmp.path()), al.file_state(tmp.path())), (FileState::Missing, FileState::Missing));
        std::fs::create_dir_all(al.dir(tmp.path())).unwrap();
        std::fs::write(al.path(tmp.path()), b"not the aligner").unwrap();
        assert_eq!((asr.file_state(tmp.path()), al.file_state(tmp.path())), (FileState::Missing, FileState::Bad), "the other file is only missing");
        // a file the cache already knows as the pinned one (as after a finished download) is ready, and
        // installing it again does not touch the network (the offer's URL is the real one)
        live::remember_sha256(&al.path(tmp.path()), al.sha256(), &tmp.path().join("model-sha256-cache.json"));
        assert_eq!(al.file_state(tmp.path()), FileState::Ready);
        let seen = std::sync::Mutex::new(vec![]);
        install(al, tmp.path(), &|d, t| seen.lock().unwrap().push((d, t)), &AtomicBool::new(false)).unwrap();
        assert_eq!(*seen.lock().unwrap(), [(al.size, al.size)]);
        assert_eq!(std::fs::read(al.path(tmp.path())).unwrap(), b"not the aligner", "left as it was");
    }

    #[test]
    fn an_import_can_be_cancelled_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("models/m/f");
        let sha = util::sha256_bytes(b"abc");
        let e = copy_in(&mut &b"abc"[..], &Want { size: 3, sha256: &sha }, &dest, &|_, _| {}, &AtomicBool::new(true)).unwrap_err();
        assert!(matches!(e, Error::Cancelled));
        assert!(!dest.exists() && !import_path(&dest).exists());
    }
}
