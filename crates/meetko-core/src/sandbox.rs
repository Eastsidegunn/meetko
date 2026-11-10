//! OS isolation for plugin processes.
//!
//! macOS: the plugin runs under `/usr/bin/sandbox-exec -p <profile>`. The profile is generated here
//! from a [`SandboxPolicy`]: network off unless declared and consented, reads of the user's home and
//! the meetko data dir denied except the plugin's own installed copy (and its interpreter prefix),
//! writes denied except the per-run HOME. Other systems have no sandbox yet (recorded as `none`).
//!
//! The profile is deny-by-default (`(deny default)`): a plugin may compute on its stdin and write its
//! stdout, nothing else. What is added back, and why, is listed in `to_sbpl` and docs/plugin-security.md.
//! It does not `(import "bsd.sb")`: that pulls in `system.sb`, which allows `mach-lookup` of every XPC
//! service name, `mach-bootstrap` and all raw syscalls, i.e. exactly the indirect channels (LaunchServices,
//! AppleEvents, pasteboard) the deny-default profile exists to close.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Recorded in the result snapshot.
pub const MACOS_SBPL: &str = "macos-sbpl";
pub const NONE: &str = "none";
pub const OFF: &str = "off";

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub network: bool,
    pub deny_read: Vec<PathBuf>,
    /// readable trees (the plugin's own copy, the run HOME)
    pub allow_read: Vec<PathBuf>,
    /// single directories that may be opened (not their children)
    pub allow_read_literal: Vec<PathBuf>,
    /// readable and mappable as code: interpreter prefixes and the developer tools tree behind the
    /// `/usr/bin/python3` shim (stdlib extension modules are mapped executable)
    pub allow_read_exec: Vec<PathBuf>,
    /// programs that may be exec'd, by exact path
    pub allow_exec: Vec<PathBuf>,
    /// trees whose programs may be exec'd (the Python framework the shim hands over to)
    pub allow_exec_tree: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
}

/// How a run is isolated: which trees are off limits (the policy's `deny_read`), or `Off`.
#[derive(Debug, Clone)]
pub struct Isolation {
    pub off: bool,
    pub deny_read: Vec<PathBuf>,
    /// the user's home (canonical): an interpreter whose install prefix would be this directory, a
    /// direct child of it or a credential dir inside it is refused rather than opened up
    pub home: Option<PathBuf>,
}

impl Isolation {
    /// The real policy: the user's home and the data dir are unreadable. `MEETKO_PLUGIN_SANDBOX=off`
    /// is a development opt-out; the run then proceeds and is recorded as `off`.
    pub fn production(data_dir: &Path) -> Isolation {
        let home = dirs::home_dir();
        let mut deny_read: Vec<PathBuf> = home.iter().cloned().collect();
        deny_read.push(data_dir.to_path_buf());
        #[cfg(target_os = "macos")]
        {
            // other users' homes, external drives (recordings, a MEETKO_DATA_DIR), shared temp dirs and
            // the per-user temp/cache root, where other apps keep private files
            for p in ["/Users", "/Volumes", "/private/tmp", "/private/var/tmp"] {
                deny_read.push(PathBuf::from(p));
            }
            deny_read.extend(user_temp_root());
        }
        Isolation { off: env_is_off(), deny_read, home: home.map(|h| canon(&h)) }
    }

    /// Explicit trees to protect (tests, or callers with their own layout).
    pub fn deny(deny_read: Vec<PathBuf>) -> Isolation {
        Isolation { off: false, deny_read, home: None }
    }

    /// Name the home directory for the interpreter check (tests, or callers with their own layout).
    pub fn with_home(mut self, home: PathBuf) -> Isolation {
        self.home = Some(canon(&home));
        self
    }

    pub fn off() -> Isolation {
        Isolation { off: true, deny_read: vec![], home: None }
    }

    /// What this run will be recorded as.
    pub fn label(&self) -> &'static str {
        if self.off {
            OFF
        } else if cfg!(target_os = "macos") {
            MACOS_SBPL
        } else {
            NONE
        }
    }
}

/// `/private/var/folders/<xx>/<yy>`: the parent of `DARWIN_USER_TEMP_DIR` (`.../T`), canonical.
#[cfg(target_os = "macos")]
pub fn user_temp_root() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; 1024];
    // SAFETY: buf is valid for buf.len() bytes; confstr writes a NUL-terminated string at most that long
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    let t = Path::new(std::ffi::OsStr::from_bytes(&buf[..n - 1]));
    let t = canon(t);
    let root = t.parent()?.to_path_buf();
    // sanity: never deny something as broad as /private/var/folders itself or less
    (root.components().count() >= 5).then_some(root)
}

fn env_is_off() -> bool {
    matches!(std::env::var("MEETKO_PLUGIN_SANDBOX").as_deref(), Ok("off"))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A path as SBPL string literal. Refuses what cannot be escaped safely.
fn lit(p: &Path) -> Result<String> {
    let s = p.to_str().ok_or_else(|| Error::Plugin(format!("sandbox: path is not UTF-8: {}", p.display())))?;
    if s.chars().any(|c| c.is_control()) {
        return Err(Error::Plugin("sandbox: path contains a control character".into()));
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// Both the path as given and its canonical form (`/var` is `/private/var`): the kernel matches the
/// resolved path, but a profile that names only one spelling is a trap.
fn spellings(p: &Path) -> Vec<PathBuf> {
    let c = canon(p);
    if c == p {
        vec![c]
    } else {
        vec![c, p.to_path_buf()]
    }
}

impl SandboxPolicy {
    /// The SBPL profile. Order matters (later rules win): the deny-default baseline and its fixed
    /// allowances, then the extra deny-read layer, then the re-allows for the plugin's own trees.
    pub fn to_sbpl(&self) -> Result<String> {
        let mut s = String::from("(version 1)\n(deny default)\n");
        // no process-fork: a plugin is a pure function and starts no process of any kind (threads are
        // fine). The program the host spawned is the only process, so the timeout kill reaches all of
        // it; a fork+setsid escape does not exist. exec is limited below to the plugin's own program
        // (the python3 shim hands over to the framework interpreter by exec, not by fork)
        s.push_str("(allow signal (target self))\n");
        // kernel parameters libc/dyld/Python read at startup (hw.ncpu, kern.osversion, ...)
        s.push_str("(allow sysctl-read)\n");
        // other processes: not their pid list, not their argv/env (KERN_PROCARGS2 holds the environment of
        // every same-uid process, the meetko host's included). `process-info*` is limited to the plugin
        // itself; the sysctl names are denied as well, because on their own neither rule stops procargs2
        // (verified: only the pair does). `kern.proc*` covers KERN_PROC_ALL and kern.procargs/procargs2.
        s.push_str("(deny process-info*)\n(allow process-info* (target self))\n");
        s.push_str("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))\n");
        // fsgetpath(fsid, inode) turns an inode number into a path with no permission check on the
        // tree, so looping over inodes lists names under the denied trees; the hardware UUID is
        // gethostuuid(). The three syscalls are not needed by libc, Python or a network client (verified)
        s.push_str("(deny syscall-unix (syscall-number SYS_fsgetpath SYS_fsgetpath_ext SYS_gethostuuid))\n");
        // network interface list incl. MAC addresses (getifaddrs reads net.route.*) and the APFS preboot
        // volume UUID. Denied with and without the network permission: name resolution and TCP/UDP to
        // localhost and remote hosts do not need the interface list (verified with Python)
        s.push_str("(deny sysctl-read (sysctl-name-prefix \"net.route\") (sysctl-name \"kern.apfsprebootuuid\"))\n");
        // hardware serial number and other IOKit registry properties
        s.push_str("(deny iokit-get-properties)\n");
        // system files dyld, libc, the timezone database and the interpreter need. Not /Library,
        // /Applications, /opt, /Users, /private/etc (except what is listed), /private/tmp.
        s.push_str(concat!(
            "(allow file-read* (subpath \"/System\") (subpath \"/private/var/select\") (subpath \"/usr/lib\") (subpath \"/usr/share\")",
            " (subpath \"/private/var/db/timezone\")",
            " (literal \"/\") (literal \"/dev/urandom\") (literal \"/dev/random\") (literal \"/dev/zero\") (literal \"/dev/null\")",
            " (literal \"/private/etc/localtime\") (literal \"/private/etc/passwd\") (literal \"/private/etc/protocols\") (literal \"/private/etc/services\"))\n",
        ));
        // /System/Volumes holds Preboot (account plists, login preferences) and Update (logs)
        s.push_str("(deny file-read* (subpath \"/System/Volumes\"))\n");
        s.push_str("(allow file-map-executable (subpath \"/System\") (subpath \"/usr/lib\"))\n");
        // path resolution: lstat of /etc, /var, /tmp and their targets; no file contents
        s.push_str("(allow file-read-metadata (literal \"/etc\") (literal \"/var\") (literal \"/tmp\"))\n");
        if self.network {
            // IP only: `(allow network*)` would also open every Unix-domain socket (docker.sock, cupsd,
            // syslog, sockets in /tmp)
            s.push_str("(allow network-outbound (remote ip))\n(allow network-bind (local ip))\n(allow network-inbound (local ip))\n");
            // name resolution: getaddrinfo asks mDNSResponder (the only mach service needed; Python's
            // urllib over HTTPS works with it alone), /etc/hosts and the resolver config; TLS roots
            // for OpenSSL-based clients are /private/etc/ssl
            s.push_str("(allow mach-lookup (global-name \"com.apple.dnssd.service\"))\n");
            s.push_str("(allow file-read* (literal \"/private/etc/hosts\") (literal \"/private/etc/resolv.conf\") (literal \"/private/var/run/resolv.conf\") (subpath \"/private/etc/ssl\"))\n");
        }
        // programs: the plugin program, its interpreter and what the interpreter re-execs
        for p in &self.allow_exec {
            for q in spellings(p) {
                s.push_str(&format!("(allow process-exec (literal {}))\n", lit(&q)?));
            }
        }
        for p in &self.allow_exec_tree {
            for q in spellings(p) {
                s.push_str(&format!("(allow process-exec (subpath {}))\n", lit(&q)?));
            }
        }
        // extra layer: user data trees stay unreadable whatever else is allowed above
        for p in &self.deny_read {
            for q in spellings(p) {
                s.push_str(&format!("(deny file-read* (subpath {}))\n", lit(&q)?));
            }
        }
        // re-allows, after the denies
        let mut parents: Vec<PathBuf> = Vec::new();
        let mut note_parents = |q: &Path| {
            for a in q.ancestors().skip(1) {
                if a != Path::new("/") && !parents.iter().any(|x| x == a) {
                    parents.push(a.to_path_buf());
                }
            }
        };
        for p in &self.allow_read {
            for q in spellings(p) {
                s.push_str(&format!("(allow file-read* (subpath {}))\n", lit(&q)?));
                note_parents(&q);
            }
        }
        for p in &self.allow_read_literal {
            for q in spellings(p) {
                s.push_str(&format!("(allow file-read* (literal {}))\n", lit(&q)?));
                note_parents(&q);
            }
        }
        for p in &self.allow_read_exec {
            for q in spellings(p) {
                s.push_str(&format!("(allow file-read* file-map-executable (subpath {}))\n", lit(&q)?));
                note_parents(&q);
            }
        }
        for p in self.allow_exec.iter().chain(&self.allow_exec_tree) {
            for q in spellings(p) {
                s.push_str(&format!("(allow file-read* (subpath {}))\n", lit(&q)?));
                note_parents(&q);
            }
        }
        // an allowed tree needs its parents' metadata (realpath, getcwd) to be resolvable, which
        // discloses only that the directories exist
        for a in parents {
            s.push_str(&format!("(allow file-read-metadata (literal {}))\n", lit(&a)?));
        }
        // the only place a plugin can write
        s.push_str("(allow file-write* (literal \"/dev/null\")");
        for p in &self.allow_write {
            for q in spellings(p) {
                s.push_str(&format!(" (subpath {})", lit(&q)?));
            }
        }
        s.push_str(")\n");
        // W^X: nothing the plugin can write may be mapped as code (no dlopen of a dropped library)
        for p in &self.allow_write {
            for q in spellings(p) {
                s.push_str(&format!("(deny file-map-executable (subpath {}))\n", lit(&q)?));
            }
        }
        Ok(s)
    }
}

/// Top-level dot-dirs of a home directory that hold credentials: never opened for an interpreter.
const CREDENTIAL_DIRS: [&str; 10] = [".ssh", ".aws", ".gnupg", ".config", ".cargo", ".docker", ".kube", ".azure", ".cache", ".npm"];

/// System roots that are never an interpreter's install prefix, however the path is spelled.
const BROAD_ROOTS: [&str; 17] = [
    "/", "/opt", "/usr", "/usr/local", "/opt/homebrew", "/Library", "/Library/Frameworks", "/Applications", "/Users", "/Volumes", "/private",
    "/private/var", "/private/tmp", "/var", "/tmp", "/etc", "/System",
];

fn refuse(exe: &Path, why: &str) -> Error {
    Error::Plugin(format!(
        "sandbox: the interpreter {} cannot be opened to a plugin ({why}). Use /usr/bin/python3 or a compiled plugin",
        exe.display()
    ))
}

/// The narrowest tree an interpreter outside the system dirs needs readable and mappable, from its
/// canonical (symlink-resolved) path; `Err` when that tree would be too broad to hand to a plugin:
/// the home directory, a direct child of it, a credential dir inside it, a root such as
/// `/opt/homebrew`, `/usr/local` or `/opt`, or a Homebrew keg (its dylib dependencies cannot be
/// derived reliably). Python.framework installs are narrowed to `Versions/<v>`. A prefix that equals,
/// contains or lies inside any `deny_read` tree is refused as well (opening it would undo the deny), as
/// is a direct child of `/Volumes` or `/Users` (a whole external drive, another user's home); only the
/// depth rule under `home` lets a prefix inside the denied home through.
pub fn interpreter_prefix(exe: &Path, home: Option<&Path>, deny_read: &[PathBuf]) -> Result<PathBuf> {
    // Homebrew (Cellar kegs, linked via /opt/homebrew/bin or /usr/local/bin): the interpreter's dylibs live
    // in other kegs; not derivable without scanning every extension module
    if exe.starts_with("/opt/homebrew") || exe.starts_with("/usr/local") {
        return Err(refuse(exe, "Homebrew and /usr/local installs need other directories that are not opened"));
    }
    let comps: Vec<_> = exe.components().collect();
    let mut pre = None;
    // /Library/Frameworks/Python.framework/Versions/3.12/bin/python3.12 -> .../Versions/3.12
    if let Some(i) = comps.iter().position(|c| c.as_os_str().to_str().is_some_and(|n| n.ends_with(".framework"))) {
        let take = if comps.get(i + 1).map(|c| c.as_os_str()) == Some(std::ffi::OsStr::new("Versions")) && comps.len() > i + 2 { i + 3 } else { i + 1 };
        pre = Some(comps[..take].iter().collect::<PathBuf>());
    }
    let pre = match pre {
        Some(p) => p,
        None => {
            let dir = exe.parent().ok_or_else(|| refuse(exe, "no parent directory"))?;
            match dir.file_name().and_then(|n| n.to_str()) {
                Some("bin" | "shims" | "Scripts") => dir.parent().unwrap_or(dir).to_path_buf(),
                _ => dir.to_path_buf(),
            }
        }
    };
    if BROAD_ROOTS.iter().any(|r| pre == Path::new(r)) || pre.components().count() < 3 {
        return Err(refuse(exe, &format!("it would open {}", pre.display())));
    }
    if pre.parent().is_some_and(|d| d == Path::new("/Volumes") || d == Path::new("/Users")) {
        return Err(refuse(exe, &format!("it would open all of {}", pre.display())));
    }
    let in_home = home.is_some_and(|h| pre.starts_with(h));
    for d in deny_read {
        // the trees that hold home are governed by the depth rule below when the prefix is inside home
        if in_home && home.is_some_and(|h| h.starts_with(d)) {
            continue;
        }
        let c = canon(d);
        if [d.as_path(), c.as_path()].iter().any(|d| pre.starts_with(d) || d.starts_with(&pre)) {
            return Err(refuse(exe, "it overlaps a directory that plugins cannot read"));
        }
    }
    if let Some(h) = home {
        if h.starts_with(&pre) {
            return Err(refuse(exe, "it would open your home directory"));
        }
        if let Ok(rel) = pre.strip_prefix(h) {
            let mut it = rel.components();
            let top = it.next().and_then(|c| c.as_os_str().to_str().map(String::from)).unwrap_or_default();
            // the prefix itself must sit at least three levels below home (~/.pyenv/versions/3.12): a
            // direct child (~/bin, ~/.local, ~/.cargo) would expose far more than the interpreter
            if rel.components().count() < 3 {
                return Err(refuse(exe, &format!("it would open ~/{}", rel.display())));
            }
            if CREDENTIAL_DIRS.contains(&top.as_str()) {
                return Err(refuse(exe, &format!("it lies inside ~/{top}")));
            }
        }
    }
    Ok(pre)
}

/// Directories whose programs ship with the OS: nothing extra needs reading for them.
fn is_system_program(exe: &Path) -> bool {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].iter().any(|d| exe.parent() == Some(Path::new(d)))
}

/// The developer tools trees behind the `/usr/bin` shims: the Command Line Tools and the one
/// `xcode-select` points at (`/var/db/xcode_select_link`; the plugin's env is cleared, so
/// `DEVELOPER_DIR` cannot redirect it).
fn developer_dirs() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Library/Developer/CommandLineTools")];
    if let Ok(t) = std::fs::read_link("/var/db/xcode_select_link") {
        let t = canon(&t);
        if !v.contains(&t) {
            v.push(t);
        }
    }
    v
}

/// The production policy for one run. `Err` when the interpreter would need a tree that is too broad
/// to open (see [`interpreter_prefix`]).
pub fn policy_for_run(
    isolation: &Isolation,
    network: bool,
    plugin_dir: &Path,
    run_home: &Path,
    prog: &Path,
) -> Result<SandboxPolicy> {
    let mut p = SandboxPolicy {
        network,
        deny_read: isolation.deny_read.clone(),
        allow_read: vec![plugin_dir.to_path_buf(), run_home.to_path_buf()],
        allow_write: vec![run_home.to_path_buf()],
        ..Default::default()
    };
    let canonical = canon(prog);
    for exe in [prog.to_path_buf(), canonical.clone()] {
        if !p.allow_exec.contains(&exe) {
            p.allow_exec.push(exe.clone());
        }
    }
    if is_system_program(prog) || is_system_program(&canonical) {
        // Apple's python3 is a shim: it runs xcrun logic, then execs the Python framework of the
        // developer tools (python3.9, which execs Python.app/Contents/MacOS/Python)
        if canonical.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("python")) {
            for d in developer_dirs() {
                // usr/lib holds libxcrun, loaded by the shim
                for r in [d.join("Library/Frameworks/Python3.framework"), d.join("usr/lib")] {
                    if !p.allow_read_exec.contains(&r) {
                        p.allow_read_exec.push(r.clone());
                    }
                    if r.ends_with("Python3.framework") && !p.allow_exec_tree.contains(&r) {
                        p.allow_exec_tree.push(r);
                    }
                }
                // xcrun checks the developer dir itself and looks python3 up in its usr/bin
                // (read only: exec stays limited to the framework)
                if !p.allow_read_literal.contains(&d) {
                    p.allow_read_literal.push(d.clone());
                    p.allow_read.push(d.join("usr/bin"));
                }
            }
        }
    } else if !canonical.starts_with(canon(plugin_dir)) {
        // an interpreter elsewhere (pyenv, a python.org framework): only its own install tree, from the
        // canonical path, readable and mappable. A program inside the plugin's own copy needs nothing.
        let pre = interpreter_prefix(&canonical, isolation.home.as_deref(), &isolation.deny_read)?;
        // a python.org framework's launcher execs Resources/Python.app inside the same tree
        if pre.parent().and_then(Path::parent).is_some_and(|f| f.extension().is_some_and(|e| e == "framework")) {
            p.allow_exec_tree.push(pre.clone());
        }
        if !p.allow_read_exec.contains(&pre) {
            p.allow_read_exec.push(pre);
        }
    }
    Ok(p)
}

/// The sandbox cannot stop a plugin from creating System V shared memory (no SBPL operation covers
/// `shmget`), and cannot let it remove what it made: the segments would leak until reboot
/// (`kern.sysv.shmmni` is 32). The host removes the segments whose creator pid is the plugin's (one
/// process: `process-fork` is denied). Semaphore sets and message queues carry no creator pid, so
/// they cannot be attributed to a run and are not removed (docs/plugin-security.md, Known gaps).
/// Returns how many segments were removed.
#[cfg(target_os = "macos")]
pub fn remove_sysv_shm_of(pid: u32) -> usize {
    let Ok(out) = std::process::Command::new("/usr/bin/ipcs").args(["-m", "-p"]).output() else {
        return 0;
    };
    let mut n = 0;
    // rows: T ID KEY MODE OWNER GROUP CPID LPID
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 8 && f[0] == "m" && f[6].parse::<u32>() == Ok(pid) {
            if let Ok(id) = f[1].parse::<i32>() {
                // SAFETY: IPC_RMID takes no buffer; a failure (not ours, already gone) is ignored
                if unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) } == 0 {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Fail closed: is sandbox-exec present and does it apply a profile here? (A process that is
/// already sandboxed may not be able to nest.) Checked once per process.
#[cfg(target_os = "macos")]
pub fn preflight() -> Result<()> {
    use std::sync::OnceLock;
    static RESULT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            if !Path::new(SANDBOX_EXEC).exists() {
                return Err(format!("{SANDBOX_EXEC} is missing"));
            }
            // the same kind of profile a plugin gets (deny default), so that an environment which cannot
            // nest, or one where deny-default breaks starting a program, is found before a plugin runs
            let probe = SandboxPolicy { allow_exec: vec![PathBuf::from("/usr/bin/true")], ..Default::default() }
                .to_sbpl()
                .map_err(|e| e.to_string())?;
            let out = std::process::Command::new(SANDBOX_EXEC)
                .args(["-p", &probe, "/usr/bin/true"])
                .output()
                .map_err(|e| format!("cannot run {SANDBOX_EXEC}: {e}"))?;
            if out.status.success() {
                Ok(())
            } else {
                Err(format!("sandbox-exec cannot apply a profile here: {}", String::from_utf8_lossy(&out.stderr).trim()))
            }
        })
        .clone()
        .map_err(|e| {
            Error::Plugin(format!(
                "plugin sandbox unavailable ({e}); refusing to run the plugin unsandboxed. \
                 For development only, set MEETKO_PLUGIN_SANDBOX=off"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_deny_default_orders_denies_before_reallows_and_escapes() {
        let p = SandboxPolicy {
            network: false,
            deny_read: vec![PathBuf::from("/nonexistent/home")],
            allow_read: vec![PathBuf::from("/nonexistent/home/p\"q\\r")],
            allow_write: vec![PathBuf::from("/nonexistent/run")],
            allow_exec: vec![PathBuf::from("/nonexistent/home/prog")],
            ..Default::default()
        };
        let s = p.to_sbpl().unwrap();
        assert!(s.starts_with("(version 1)\n(deny default)\n"), "{s}");
        assert!(!s.contains("(allow default)") && !s.contains("import"));
        let deny = s.find("(deny file-read*").unwrap();
        let allow = s.find("(allow file-read* (subpath \"/nonexistent/home/p").unwrap();
        assert!(deny < allow);
        assert!(s.contains(r#"(subpath "/nonexistent/home/p\"q\\r")"#), "{s}");
        assert!(s.contains("(allow process-exec (literal \"/nonexistent/home/prog\"))"));
        // writes: only /dev/null and the run home
        assert!(s.contains("(allow file-write* (literal \"/dev/null\") (subpath \"/nonexistent/run\"))"));
        assert_eq!(s.matches("file-write*").count(), 1);
        // network: nothing without the permission, network* plus DNS with it
        assert!(!s.contains("network") && !s.contains("mach-lookup"));
        let net = SandboxPolicy { network: true, ..p.clone() }.to_sbpl().unwrap();
        assert!(!net.contains("network*") && net.contains("(allow network-outbound (remote ip))") && net.contains("com.apple.dnssd.service"));
        // no processes, no other processes' info, no IOKit properties, no code from the writable home
        assert!(!s.contains("process-fork"));
        assert!(s.contains("(deny process-info*)\n(allow process-info* (target self))") && s.contains("kern.proc"));
        assert!(s.contains("(deny syscall-unix (syscall-number SYS_fsgetpath SYS_fsgetpath_ext SYS_gethostuuid))"));
        assert!(s.contains("(sysctl-name-prefix \"net.route\")") && s.contains("(sysctl-name \"kern.apfsprebootuuid\")"));
        assert!(net_has_same_deny(&net));
        assert!(s.contains("(deny iokit-get-properties)") && s.contains("(deny file-read* (subpath \"/System/Volumes\"))"));
        assert!(s.trim_end().ends_with("(deny file-map-executable (subpath \"/nonexistent/run\"))"), "{s}");
        assert!(SandboxPolicy { allow_read: vec!["/a\nb".into()], ..p }.to_sbpl().is_err());
    }

    fn net_has_same_deny(net: &str) -> bool {
        net.contains("SYS_fsgetpath SYS_fsgetpath_ext SYS_gethostuuid") && net.contains("net.route")
    }

    #[test]
    #[ignore]
    fn dump_profile() {
        let net = std::env::var("DUMP_NET").is_ok();
        let iso = Isolation::production(Path::new("/nonexistent/data"));
        let prog = Path::new(&std::env::var("DUMP_PROG").unwrap_or("/usr/bin/python3".into())).to_path_buf();
        let pol = policy_for_run(&iso, net, Path::new("/SCRATCH/plugin"), Path::new("/SCRATCH/home"), &prog).unwrap();
        std::fs::write(std::env::var("DUMP_OUT").unwrap(), pol.to_sbpl().unwrap()).unwrap();
    }

    #[test]
    fn interpreter_prefix_outside_the_system_dirs_is_narrow_and_system_ones_get_nothing_extra() {
        let iso = Isolation::deny(vec![PathBuf::from("/nonexistent/home")]).with_home(PathBuf::from("/nonexistent/home"));
        let p = policy_for_run(&iso, false, Path::new("/nonexistent/home/data/p"), Path::new("/run/h"), Path::new("/nonexistent/home/.pyenv/versions/3.12/bin/python3")).unwrap();
        assert!(p.allow_read_exec.contains(&PathBuf::from("/nonexistent/home/.pyenv/versions/3.12")));
        assert!(p.allow_exec.contains(&PathBuf::from("/nonexistent/home/.pyenv/versions/3.12/bin/python3")));
        assert!(!p.allow_read_exec.iter().any(|r| r == Path::new("/nonexistent/home/.pyenv")));
        let p = policy_for_run(&iso, false, Path::new("/x"), Path::new("/run/h"), Path::new("/usr/bin/python3")).unwrap();
        assert!(!p.allow_read.iter().any(|r| r == Path::new("/usr") || r == Path::new("/usr/bin")));
        assert!(!p.allow_read_exec.contains(&PathBuf::from("/usr")));
        assert!(p.allow_exec.contains(&PathBuf::from("/usr/bin/python3")));
        // a system program other than python gets no developer-tools allowance
        let p = policy_for_run(&iso, false, Path::new("/x"), Path::new("/run/h"), Path::new("/usr/bin/awk")).unwrap();
        assert!(p.allow_exec_tree.is_empty());
    }

    #[test]
    fn interpreter_prefix_narrowing_and_refusals() {
        let home = Path::new("/nonexistent/home");
        let deny = vec![PathBuf::from("/nonexistent/home"), PathBuf::from("/nonexistent/home/data")];
        let ok = |s: &str| interpreter_prefix(Path::new(s), Some(home), &deny).unwrap();
        assert_eq!(ok("/nonexistent/home/.pyenv/versions/3.12.1/bin/python3.12"), PathBuf::from("/nonexistent/home/.pyenv/versions/3.12.1"));
        assert_eq!(ok("/nonexistent/home/.local/share/uv/python/cpython-3.12-x/bin/python3.12"), PathBuf::from("/nonexistent/home/.local/share/uv/python/cpython-3.12-x"));
        assert_eq!(ok("/Library/Frameworks/Python.framework/Versions/3.12/bin/python3.12"), PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.12"));
        assert_eq!(ok("/opt/tools/py/bin/python3"), PathBuf::from("/opt/tools/py"));
        let bad = |s: &str| interpreter_prefix(Path::new(s), Some(home), &deny).unwrap_err().to_string();
        for s in [
            "/nonexistent/home/.local/bin/python3.12",     // ~/.local
            "/nonexistent/home/bin/python3",               // the home dir itself
            "/nonexistent/home/python3",                   // the home dir itself
            "/nonexistent/home/.cargo/bin/python3",        // ~/.cargo (credentials)
            "/nonexistent/home/proj/bin/python3",          // a direct child of home
            "/nonexistent/home/.cargo/registry/x/bin/p",   // credential dir, deep
            "/nonexistent/home/.ssh/a/b/bin/p",
            "/opt/homebrew/bin/python3.12",                // Homebrew
            "/opt/homebrew/Cellar/python@3.12/3.12.1/bin/python3.12",
            "/usr/local/bin/python3",
            "/usr/local/Cellar/python@3.12/3.12.1/bin/python3.12",
            "/opt/python3",                                // /opt itself
            "/opt/bin/python3",
            "/python3",
        ] {
            let e = bad(s);
            assert!(e.contains("/usr/bin/python3"), "{s}: {e}");
        }
        // a prefix that contains home
        assert!(interpreter_prefix(Path::new("/nonexistent/bin/python3"), Some(home), &deny).is_err());
        // no home known: the root rules still apply
        assert!(interpreter_prefix(Path::new("/opt/homebrew/bin/python3"), None, &[]).is_err());
    }

    #[test]
    fn interpreter_prefix_refuses_what_overlaps_a_denied_tree_or_a_whole_volume_or_user() {
        let home = Path::new("/nonexistent/home");
        let deny: Vec<PathBuf> = ["/nonexistent/home", "/nonexistent/home/data", "/Users", "/Volumes", "/private/tmp", "/nonexistent/vol/rec"].iter().map(PathBuf::from).collect();
        let bad = |s: &str| interpreter_prefix(Path::new(s), Some(home), &deny).unwrap_err().to_string();
        for s in [
            "/Volumes/Ext/bin/python3",                     // a whole external drive
            "/Users/Shared/bin/python3",                    // another user's tree
            "/Users/Shared/py/bin/python3",                 // inside /Users, outside home
            "/Volumes/Ext/py/bin/python3",                  // inside /Volumes
            "/private/tmp/py/bin/python3",                  // inside a denied tree
            "/nonexistent/home/data/py/bin/python3",        // the data dir inside home
            "/nonexistent/home/data/x/y/z/bin/python3",
            "/nonexistent/vol/rec/py/bin/python3",          // a denied recordings tree
            "/nonexistent/vol/bin/python3",              // a prefix that contains a denied tree
        ] {
            assert!(bad(s).contains("/usr/bin/python3"), "{s}");
        }
        // the depth rule under home still lets a deep prefix through, and unrelated trees are fine
        let ok = |s: &str| interpreter_prefix(Path::new(s), Some(home), &deny);
        assert!(ok("/nonexistent/home/.pyenv/versions/3.12/bin/python3").is_ok());
        assert!(ok("/opt/tools/py/bin/python3").is_ok());
        // without a home the deny list alone decides
        assert!(interpreter_prefix(Path::new("/Users/Shared/py/bin/python3"), None, &deny).is_err());
        assert!(interpreter_prefix(Path::new("/Volumes/Ext/bin/python3"), None, &[]).is_err());
    }

    #[test]
    fn an_interpreter_in_a_home_dot_dir_makes_the_policy_fail() {
        let iso = Isolation::deny(vec![]).with_home(PathBuf::from("/nonexistent/home"));
        let e = policy_for_run(&iso, false, Path::new("/x"), Path::new("/run/h"), Path::new("/nonexistent/home/.local/bin/python3.12")).unwrap_err();
        assert!(e.to_string().contains("~/.local"), "{e}");
    }
}
