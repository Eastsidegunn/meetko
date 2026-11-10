use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Canonical JSON: object keys sorted (serde_json's map is ordered), no whitespace.
pub fn canonical_json(v: &Value) -> String {
    v.to_string()
}

/// sha256 of the canonical meeting-data without its `version` object: two versions
/// with the same transcript have the same content hash whatever their ids.
pub fn content_sha256(meeting_data: &Value) -> String {
    let mut v = meeting_data.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("version");
    }
    sha256_bytes(canonical_json(&v).as_bytes())
}

/// Create (truncate) a file readable only by the owner on Unix.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Create a private file only if it does not exist yet; `AlreadyExists` if somebody else did.
/// Never truncates, so racing creators cannot wipe each other's work.
pub fn create_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// True when `path` is a directory that group or others can access (Unix; always false elsewhere).
pub fn is_loose_dir(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.is_dir() && m.permissions().mode() & 0o077 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Tighten a data dir that is looser than 0700 (Unix; only one we own). Callers decide whether
/// the dir is theirs to change: never call this on a folder the user pointed us at.
pub fn tighten_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = std::fs::metadata(path)?;
        // SAFETY: geteuid has no preconditions
        if m.is_dir() && m.permissions().mode() & 0o077 != 0 && m.uid() == unsafe { libc::geteuid() } {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    create_private(path)?.write_all(data)
}

/// Create a directory (and parents) with mode 0700 on Unix.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" without a time crate (civil-from-days, H. Hinnant).
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The data dir: `MEETKO_DATA_DIR`, else the per-user local data dir
/// (macOS `~/Library/Application Support/meetko`, Windows `%LOCALAPPDATA%\meetko`).
pub fn data_dir() -> PathBuf {
    match std::env::var_os("MEETKO_DATA_DIR") {
        Some(d) if !d.is_empty() => d.into(),
        _ => default_data_dir(),
    }
}

/// The per-user default data dir (ignores `MEETKO_DATA_DIR`).
pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("meetko")
}

/// Parent variables a child may inherit. Everything else is dropped, so secrets in the user's
/// environment never reach plugins, engines or the capture provider.
const ENV_INHERIT: &[&str] = &["PATH", "LANG"];
#[cfg(windows)]
const ENV_INHERIT_WINDOWS: &[&str] = &["SystemRoot", "PATHEXT"];

/// Give `cmd` a minimal environment: the allowlist above, and HOME (on Windows also TEMP, TMP,
/// USERPROFILE) pointing at `home`, a private directory the caller keeps alive for the run.
/// Every spawner uses this, so the policy lives in one place.
pub fn sandbox_env(cmd: &mut std::process::Command, home: &Path) {
    cmd.env_clear();
    for k in ENV_INHERIT {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.env("HOME", home);
    #[cfg(windows)]
    {
        for k in ENV_INHERIT_WINDOWS {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        cmd.env("TEMP", home).env("TMP", home).env("USERPROFILE", home);
    }
}

/// Kill a child and everything it started. Unix: the child leads its own process
/// group (set at spawn), so the whole group gets SIGKILL. Windows children that need
/// tree-kill are put in a Job Object by the host; here the child itself is killed.
pub fn kill_tree(child: &mut std::process::Child) {
    kill_group(child);
    let _ = child.kill(); // a no-op once reaped
    let _ = child.wait();
}

/// SIGKILL the child's process group, but only while the pid is still ours. A child that exited
/// and was reaped (by `try_wait`, `wait`) frees its pid, which may then be another process's
/// group id. `waitid(WNOWAIT)` looks without reaping: a live child or an unreaped zombie still
/// owns the pid (so stragglers in its group can be killed safely); ECHILD means it is gone.
pub fn kill_group(child: &std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: zeroed siginfo is a valid out-parameter; waitid with WNOHANG|WNOWAIT only observes
        let mine = unsafe {
            let mut si: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut si, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) == 0
        };
        if mine {
            // SAFETY: signalling the group this child leads; ESRCH is ignored
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// True once the child has exited, without reaping it (so its group can still be killed safely).
pub fn has_exited(child: &std::process::Child) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: zeroed siginfo is a valid out-parameter; WNOWAIT only observes
        unsafe {
            let mut si: libc::siginfo_t = std::mem::zeroed();
            let r = libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut si, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT);
            r != 0 || si.si_pid() != 0 // an error (ECHILD) means it is already reaped
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
        false
    }
}

/// Put the child in its own process group so `kill_tree` reaches its descendants.
pub fn own_process_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Test support: run the calling test again in a child process that has a secret in its
/// environment (changing our own environment is not safe beside other threads). True in the
/// child, where the test goes on to assert; false in the parent, which has checked the child passed.
#[cfg(all(test, unix))]
pub(crate) fn in_secret_child(test: &str) -> bool {
    if std::env::var_os("MEETKO_TEST_SECRET").is_some() {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env("MEETKO_TEST_SECRET", "hunter2")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
}
