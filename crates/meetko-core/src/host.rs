//! Plugin host: one plugin process over protocol 0.1 (port of reference/host.py).
//!
//! Newline-delimited JSON-RPC 2.0 on the plugin's stdin/stdout; stderr is its log.
//! One request in flight at a time, each with a time budget. A plugin that misbehaves
//! (silence, garbage, a flood, a closed stdout) is killed with its whole process tree;
//! the host never waits unboundedly on a child that may still be alive.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::contract::{output_errors, MeetingData, CONTRACT};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::sandbox::{self, Isolation};
use crate::secrets::{Kind, Redactor, RunConfig};

/// Bytes of plugin stderr kept (the tail); error messages quote the last 2000 of them.
const STDERR_KEEP: usize = 8192;

/// The kept end of a plugin's stderr, and whether its start was cut off.
#[derive(Default)]
struct StderrTail {
    bytes: Vec<u8>,
    cut: bool,
}
use crate::util::{kill_tree, own_process_group};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// one reply line; a whole meeting-data input is far smaller than this
pub const MAX_LINE: usize = 16 * 1024 * 1024;

fn protocol_error(code: i64) -> &'static str {
    match code {
        -32601 => "method not found",
        -32001 => "unsupported contract",
        -32010 => "input rejected",
        _ => "error",
    }
}

/// PATH lookup; on Windows a bare name also tries each PATHEXT extension.
pub fn find_on_path(name: &Path) -> Option<PathBuf> {
    find_in(name, &std::env::var_os("PATH")?)
}

fn find_in(name: &Path, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let mut names = vec![name.to_path_buf()];
    if cfg!(windows) && name.extension().is_none() {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for e in exts.split(';').filter(|e| !e.is_empty()) {
            let mut n = name.as_os_str().to_owned();
            n.push(e);
            names.push(n.into());
        }
    }
    std::env::split_paths(&path).flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

/// Apple's python3 shim; the only Python the macOS sandbox accepts without a narrow install tree.
const APPLE_PYTHON3: &str = "/usr/bin/python3";

/// Without Command Line Tools the shim opens a GUI install prompt instead of running.
fn clt_installed() -> bool {
    cfg!(not(target_os = "macos"))
        || Path::new("/Library/Developer/CommandLineTools/usr/bin/python3").is_file()
        || Command::new("xcode-select").arg("-p").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Resolve a bare `run.command` name. With the macOS sandbox active, `python3` prefers Apple's
/// `/usr/bin/python3` (Homebrew and home-dir interpreters are refused by the sandbox); if that
/// exists but Command Line Tools are missing, fail clearly rather than trigger the install dialog.
fn resolve_bare(prog: &Path, sandboxed: bool, path: Option<&std::ffi::OsStr>, apple_python: &Path, clt: bool) -> Result<PathBuf> {
    if cfg!(target_os = "macos") && sandboxed && prog == Path::new("python3") && apple_python.is_file() {
        if !clt {
            return Err(Error::Plugin(
                "python3 needs the Xcode Command Line Tools, which are not installed; run `xcode-select --install` and try again".into(),
            ));
        }
        return Ok(apple_python.to_path_buf());
    }
    let find = |n: &Path| path.and_then(|p| find_in(n, p));
    // Windows installs python.exe, not python3.exe; manifests written for macOS say python3
    find(prog)
        .or_else(|| (cfg!(windows) && prog == Path::new("python3")).then(|| find(Path::new("python"))).flatten())
        .ok_or_else(|| Error::Plugin(format!("{} not found on PATH", prog.display())))
}

#[cfg(windows)]
mod job {
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;

    /// Closing the last handle kills every process in the job.
    pub struct Job(HANDLE);
    unsafe impl Send for Job {}

    impl Job {
        pub fn new() -> Option<Job> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h))
            }
        }

        pub fn assign(&self, child: &Child) -> bool {
            use std::os::windows::io::AsRawHandle;
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
        }

        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub(crate) type Line = std::result::Result<String, String>;

/// Lines of at most MAX_LINE bytes, valid UTF-8; the first problem is sent as Err and
/// ends the stream. The channel is bounded so a flooding plugin is throttled, not buffered.
pub(crate) fn read_lines(stdout: impl Read, tx: SyncSender<Line>) {
    let mut r = BufReader::new(stdout);
    loop {
        let mut buf = Vec::new();
        let n = match (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Err(format!("cannot read plugin output: {e}")));
                return;
            }
        };
        if n == 0 {
            return;
        }
        let complete = buf.last() == Some(&b'\n');
        if !complete && buf.len() > MAX_LINE {
            let _ = tx.send(Err(format!("plugin output line exceeds {} MB", MAX_LINE >> 20)));
            return;
        }
        if complete {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        match String::from_utf8(buf) {
            Ok(s) => {
                if tx.send(Ok(s)).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("plugin output is not valid UTF-8: {e}")));
                return;
            }
        }
    }
}

pub struct Plugin {
    child: Child,
    to_child: Option<Sender<String>>,
    from_child: Receiver<Line>,
    stderr: Arc<Mutex<StderrTail>>,
    next_id: u64,
    timeout: Duration,
    #[cfg(windows)]
    job: Option<job::Job>,
    /// HOME for this run; removed when the plugin is dropped
    _home: tempfile::TempDir,
    /// the `initialize` result: `{contract, plugin, outputs}`
    pub info: Value,
    /// the isolation actually applied: `macos-sbpl`, `none` or `off` (see `sandbox`)
    pub sandbox: &'static str,
    /// the secret values this run was given; removed from everything the plugin says
    redact: Redactor,
    /// the manifest's id and version: what the output's `plugin` must say
    package: (String, String),
}

impl Plugin {
    /// Spawn under the production isolation (home and the data dir unreadable).
    pub fn spawn(manifest: &Manifest, dir: &Path, timeout: Duration) -> Result<Plugin> {
        Plugin::spawn_with(manifest, dir, timeout, &Isolation::production(&crate::util::data_dir()))
    }

    pub fn spawn_with(manifest: &Manifest, dir: &Path, timeout: Duration, isolation: &Isolation) -> Result<Plugin> {
        Plugin::spawn_with_secrets(manifest, dir, timeout, isolation, &RunConfig::default())
    }

    /// `spawn_with`, giving the plugin its secrets and settings in the `initialize` request on its
    /// stdin (never in its environment, which other processes of the user can read). Only names the
    /// manifest declares, each under its own kind, are accepted; every secret value is redacted from
    /// the plugin's replies, errors and stderr (see `crate::secrets`).
    pub fn spawn_with_secrets(manifest: &Manifest, dir: &Path, timeout: Duration, isolation: &Isolation, run: &RunConfig) -> Result<Plugin> {
        manifest.require_plugin()?;
        let declared = |name: &str, kind| crate::secrets::kind_of(&manifest.permissions, name) == Some(kind);
        for (name, _) in &run.secrets {
            if !declared(name, Kind::Secret) {
                return Err(Error::Plugin(format!("{name} is not a secret this plugin declares")));
            }
        }
        for (name, _) in &run.settings {
            if !declared(name, Kind::Setting) {
                return Err(Error::Plugin(format!("{name} is not a setting this plugin declares")));
            }
        }
        let redact = Redactor::new(run.secrets.iter().map(|(_, v)| v.as_str()));
        let (prog, args) = manifest.resolve_command(dir)?;
        // resolve ourselves: PATH lookup in the child would depend on the cleared env
        let prog = if prog.components().count() == 1 {
            let sandboxed = isolation.label() == sandbox::MACOS_SBPL;
            resolve_bare(&prog, sandboxed, std::env::var_os("PATH").as_deref(), Path::new(APPLE_PYTHON3), clt_installed())?
        } else {
            prog
        };
        let home = tempfile::Builder::new().prefix("meetko-plugin-").tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let label = isolation.label();
        let mut cmd = if label == sandbox::MACOS_SBPL {
            #[cfg(target_os = "macos")]
            {
                sandbox::preflight()?;
                let dir_c = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                let policy = sandbox::policy_for_run(isolation, manifest.permissions.network, &dir_c, home.path(), &prog)?;
                // sandbox-exec applies the profile and execs the program: the pid (and so the
                // process group set below) is the plugin's own
                let mut c = Command::new(sandbox::SANDBOX_EXEC);
                c.arg("-p").arg(policy.to_sbpl()?).arg(&prog);
                c
            }
            #[cfg(not(target_os = "macos"))]
            unreachable!("only macOS labels a run macos-sbpl")
        } else {
            Command::new(&prog)
        };
        cmd.args(args).current_dir(dir);
        crate::util::sandbox_env(&mut cmd, home.path());
        cmd.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
        own_process_group(&mut cmd);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Plugin(format!("cannot start plugin: {e}")))?;
        #[cfg(windows)]
        let job = job::Job::new().filter(|j| j.assign(&child));

        // Every pipe is serviced by a thread so that a plugin which never reads its
        // input, or never writes, cannot block the host past the time budget.
        let mut stdin = child.stdin.take().expect("piped");
        let (to_child, wr) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in wr {
                if stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let stdout = child.stdout.take().expect("piped");
        let (tx, from_child) = mpsc::sync_channel::<Line>(8);
        std::thread::spawn(move || read_lines(stdout, tx));
        let stderr_buf = Arc::new(Mutex::new(StderrTail::default()));
        let (mut err, sink) = (child.stderr.take().expect("piped"), stderr_buf.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = sink.lock().unwrap();
                b.bytes.extend_from_slice(&chunk[..n]);
                let excess = b.bytes.len().saturating_sub(STDERR_KEEP);
                if excess > 0 {
                    b.bytes.drain(..excess); // keep the tail only; `stderr_tail` drops a cut value
                    b.cut = true;
                }
            }
        });

        let mut p = Plugin {
            child,
            to_child: Some(to_child),
            from_child,
            stderr: stderr_buf,
            next_id: 0,
            timeout,
            #[cfg(windows)]
            job,
            _home: home,
            info: Value::Null,
            sandbox: label,
            redact,
            package: (manifest.package.id.clone(), manifest.package.version.clone()),
        };
        let mut params = json!({ "contract": [CONTRACT], "time_budget_ms": timeout.as_millis() as u64 });
        if !run.is_empty() {
            let map = |kv: &[(String, String)]| kv.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<serde_json::Map<_, _>>();
            params["secrets"] = Value::Object(map(&run.secrets));
            params["settings"] = Value::Object(map(&run.settings));
        }
        p.info = match p.call("initialize", params) {
            Ok(v) => v,
            Err(e) if label == sandbox::MACOS_SBPL && e.to_string().contains("sandbox_apply") => {
                return Err(Error::Plugin(format!("the sandbox could not be applied, plugin not run: {e}")));
            }
            Err(e) => return Err(e),
        };
        p.redact.value(&mut p.info);
        if p.info["contract"] != json!(CONTRACT) {
            return Err(p.redact.error(Error::Plugin(format!("plugin chose contract {}", p.info["contract"]))));
        }
        Ok(p)
    }

    /// the plugin process id (the plugin itself: the sandbox wrapper execs into it)
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The last ~2000 bytes of stderr, with every secret redacted before the cut and no fragment
    /// of one left where the text was cut (see `Redactor::tail`).
    fn stderr_tail(&self) -> String {
        let b = self.stderr.lock().unwrap();
        self.redact.tail(&String::from_utf8_lossy(&b.bytes), 2000, b.cut)
    }

    /// Kill the plugin and everything it started, then reap it.
    fn kill(&mut self) {
        #[cfg(windows)]
        if let Some(j) = &self.job {
            j.terminate();
        }
        kill_tree(&mut self.child);
    }

    /// One request. Errors carry no secret value of this run.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let r = self.call_inner(method, params);
        r.map_err(|e| self.redact.error(e))
    }

    fn call_inner(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        let sent = self.to_child.as_ref().is_some_and(|tx| tx.send(format!("{msg}\n")).is_ok());
        if !sent {
            return Err(Error::Plugin("plugin is not accepting input".into()));
        }
        let line = match self.from_child.recv_timeout(self.timeout) {
            Ok(Ok(l)) => l,
            Ok(Err(cause)) => {
                self.kill();
                return Err(Error::Plugin(cause));
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill();
                return Err(Error::Timeout(self.timeout));
            }
            Err(RecvTimeoutError::Disconnected) => {
                // stdout closed, but the process may be alive (closed fd 1 and sleeping):
                // kill it before waiting, never wait on a live child
                self.kill();
                return Err(Error::Plugin(format!("plugin exited: {}", self.stderr_tail())));
            }
        };
        let reply: Value = serde_json::from_str(&line).map_err(|e| Error::Plugin(format!("reply is not JSON: {e}")))?;
        if reply["id"] != json!(self.next_id) {
            return Err(Error::Plugin(format!("reply id {} != {}", reply["id"], self.next_id)));
        }
        if let Some(e) = reply.get("error") {
            let code = e["code"].as_i64().unwrap_or(0);
            return Err(Error::Plugin(format!(
                "{code} ({}): {}",
                protocol_error(code),
                e["message"].as_str().unwrap_or("")
            )));
        }
        Ok(reply["result"].clone())
    }

    /// Run on a stored version, exactly as stored (unknown fields included). Output that
    /// breaks the contract is rejected whole.
    pub fn run_raw(&mut self, raw: &Value) -> Result<Value> {
        let md: MeetingData = serde_json::from_value(raw.clone())
            .map_err(|e| Error::Invalid(format!("input is not meeting data: {e}")))?;
        let mut out = self.call("run", json!({ "input": raw }))?;
        // before the check, so nothing below (stored output, rejection message) can carry a value
        self.redact.value(&mut out);
        // the output speaks for this plugin only: a result stored under another name (or a secret
        // smuggled into `plugin.id`, redacted above) is refused
        if out["plugin"]["id"] != json!(self.package.0) || out["plugin"]["version"] != json!(self.package.1) {
            return Err(self.redact.error(Error::Plugin(format!(
                "rejected output: it names plugin {}@{}, but this is {}@{}",
                crate::text::escape(&out["plugin"]["id"].to_string(), false),
                crate::text::escape(&out["plugin"]["version"].to_string(), false),
                self.package.0,
                self.package.1
            ))));
        }
        let errs = output_errors(&md, &out);
        if !errs.is_empty() {
            return Err(Error::Plugin(format!(
                "rejected output: {}",
                errs.into_iter().take(10).collect::<Vec<_>>().join("; ")
            )));
        }
        Ok(out)
    }

    pub fn run(&mut self, md: &MeetingData) -> Result<Value> {
        self.run_raw(&md.to_value())
    }

    pub fn close(mut self) -> Result<()> {
        let res = self.call("shutdown", Value::Object(Default::default())).map(|_| ());
        self.to_child = None; // closes the plugin's stdin
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if crate::util::has_exited(&self.child) || matches!(self.child.try_wait(), Ok(Some(_))) {
                self.reap_stragglers(); // before the reap that Drop or wait does: the pid is still ours
                let _ = self.child.wait();
                return res;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill();
        res
    }

    /// The plugin is gone; whatever it left running in its group goes too.
    fn reap_stragglers(&self) {
        // only while the pid is still ours: see `util::kill_group`
        crate::util::kill_group(&self.child);
        #[cfg(windows)]
        if let Some(j) = &self.job {
            j.terminate();
        }
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.kill(); // kills the group while the pid is still ours, then reaps
        #[cfg(target_os = "macos")]
        if self.sandbox == sandbox::MACOS_SBPL {
            sandbox::remove_sysv_shm_of(self.child.id());
        }
    }
}

/// Spawn, run once on the stored JSON, shut down.
pub fn run_once(manifest: &Manifest, dir: &Path, raw: &Value, timeout: Duration) -> Result<Value> {
    run_once_with(manifest, dir, raw, timeout, &Isolation::production(&crate::util::data_dir())).map(|(v, _)| v)
}

/// `run_once` with an explicit isolation; also returns the isolation actually applied.
pub fn run_once_with(manifest: &Manifest, dir: &Path, raw: &Value, timeout: Duration, isolation: &Isolation) -> Result<(Value, &'static str)> {
    run_once_with_secrets(manifest, dir, raw, timeout, isolation, &RunConfig::default())
}

/// `run_once_with`, with the plugin's secrets and settings for this run (see `spawn_with_secrets`).
pub fn run_once_with_secrets(manifest: &Manifest, dir: &Path, raw: &Value, timeout: Duration, isolation: &Isolation, run: &RunConfig) -> Result<(Value, &'static str)> {
    let mut p = Plugin::spawn_with_secrets(manifest, dir, timeout, isolation, run)?;
    let sandbox = p.sandbox;
    let out = p.run_raw(raw);
    let _ = p.close();
    out.map(|v| (v, sandbox))
}

#[cfg(all(test, target_os = "macos"))]
mod resolve_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_path() -> (tempfile::TempDir, std::ffi::OsString) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("python3");
        std::fs::write(&f, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = std::env::join_paths([d.path()]).unwrap();
        (d, p)
    }

    #[test]
    fn sandbox_prefers_apple_python3_over_path() {
        let (d, path) = fake_path();
        let apple = Path::new(APPLE_PYTHON3);
        if !apple.is_file() {
            return;
        }
        let got = resolve_bare(Path::new("python3"), true, Some(&path), apple, true).unwrap();
        assert_eq!(got, apple);
        let off = resolve_bare(Path::new("python3"), false, Some(&path), apple, true).unwrap();
        assert_eq!(off, d.path().join("python3"));
        let other = resolve_bare(Path::new("python3.12"), true, Some(&path), apple, true);
        assert!(other.is_err());
    }

    #[test]
    fn missing_command_line_tools_is_a_clear_error() {
        let (_d, path) = fake_path();
        let apple = Path::new(APPLE_PYTHON3);
        if !apple.is_file() {
            return;
        }
        let e = resolve_bare(Path::new("python3"), true, Some(&path), apple, false).unwrap_err().to_string();
        assert!(e.contains("xcode-select --install"), "{e}");
    }
}
