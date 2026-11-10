//! OS isolation of plugins (macOS: real sandbox-exec). Fake plugins probe what they may do at
//! `initialize` and report it back in `outputs`.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use meetko_core::host::Plugin;
use meetko_core::manifest::Manifest;
use meetko_core::sandbox::Isolation;
use serde_json::{json, Value};

fn manifest(cmd: &[&str], network: bool) -> Manifest {
    let list: Vec<String> = cmd.iter().map(|s| format!("{s:?}")).collect();
    Manifest::parse(&format!(
        "[package]\nid=\"t/fake\"\nversion=\"0\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[{}]\nprotocol=\"stdio-jsonrpc\"\n[permissions]\nnetwork={network}\n",
        list.join(",")
    ))
    .unwrap()
}

/// A fake plugin whose `initialize` runs `probe` (Python; fills the dict `res`) and answers with it.
/// Layout: `<root>/plugin/p.py`, with `<root>/secret.txt` beside it; `<root>` is the denied tree.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new(probe: &str) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("plugin")).unwrap();
        std::fs::write(root.path().join("secret.txt"), "top secret").unwrap();
        std::fs::write(
            root.path().join("plugin/p.py"),
            format!(
                r#"import sys, json, os, time, subprocess, socket
res = {{}}
def attempt(name, f):
    try:
        res[name] = ["ok", f()]
    except Exception as e:
        res[name] = ["err", type(e).__name__]
{probe}
for line in sys.stdin:
    m = json.loads(line)
    print(json.dumps({{"jsonrpc": "2.0", "id": m["id"], "result": {{"contract": "0.1", "plugin": {{"id": "t/fake", "version": "0"}}, "outputs": res}}}}), flush=True)
"#
            ),
        )
        .unwrap();
        Fixture { root }
    }
    fn dir(&self) -> PathBuf {
        self.root.path().join("plugin")
    }
    fn secret(&self) -> PathBuf {
        self.root.path().join("secret.txt")
    }
    fn iso(&self) -> Isolation {
        Isolation::deny(vec![self.root.path().to_path_buf()])
    }
    fn spawn(&self, args: &[&str], network: bool) -> Plugin {
        self.spawn_iso(args, network, &self.iso())
    }
    fn spawn_iso(&self, args: &[&str], network: bool, iso: &Isolation) -> Plugin {
        let mut cmd = vec!["python3", "-I", "-S", "p.py"];
        cmd.extend_from_slice(args);
        Plugin::spawn_with(&manifest(&cmd, network), &self.dir(), Duration::from_secs(20), iso).unwrap()
    }
}

fn res(p: &Plugin, k: &str) -> (String, Value) {
    let v = &p.info["outputs"][k];
    (v[0].as_str().unwrap().to_string(), v[1].clone())
}

fn paths(f: &Fixture) -> [String; 2] {
    [f.secret().to_str().unwrap().into(), f.root.path().to_str().unwrap().into()]
}

#[test]
fn reads_in_the_denied_tree_fail_but_the_plugins_own_dir_is_readable() {
    let f = Fixture::new(
        "attempt('secret', lambda: open(sys.argv[1]).read())\n\
         attempt('own', lambda: len(open('p.py').read()) > 0)\n\
         attempt('list_root', lambda: os.listdir(sys.argv[2]))\n\
         attempt('list_own', lambda: sorted(os.listdir('.')))",
    );
    let [secret, root] = paths(&f);
    let p = f.spawn(&[&secret, &root], false);
    assert_eq!(p.sandbox, "macos-sbpl");
    assert_eq!(res(&p, "secret"), ("err".into(), json!("PermissionError")));
    assert_eq!(res(&p, "list_root"), ("err".into(), json!("PermissionError")));
    assert_eq!(res(&p, "own"), ("ok".into(), json!(true)));
    assert_eq!(res(&p, "list_own"), ("ok".into(), json!(["p.py"])));
}

#[test]
fn the_real_home_is_unreadable_under_the_production_policy() {
    let home = dirs::home_dir().unwrap();
    let f = Fixture::new("attempt('home', lambda: os.listdir(sys.argv[1]))\nattempt('stat', lambda: os.path.exists(sys.argv[1]))");
    let m = manifest(&["python3", "-I", "-S", "p.py", home.to_str().unwrap()], false);
    let p = Plugin::spawn_with(&m, &f.dir(), Duration::from_secs(20), &Isolation::production(&PathBuf::from("/nonexistent-data"))).unwrap();
    assert_eq!(res(&p, "home"), ("err".into(), json!("PermissionError")));
}

#[test]
fn the_per_user_temp_dir_and_shared_dirs_are_unreadable_under_the_production_policy() {
    let root = meetko_core::sandbox::user_temp_root().expect("per-user temp root");
    // a file in the real per-user temp dir, outside the run HOME (which is inside the same root)
    let tmp = tempfile::tempdir().unwrap();
    let secret = tmp.path().join("other-app-secret.txt");
    std::fs::write(&secret, "x").unwrap();
    let f = Fixture::new(
        "attempt('temp_secret', lambda: open(sys.argv[1]).read())\n\
         attempt('root_list', lambda: os.listdir(sys.argv[2]))\n\
         attempt('tmp', lambda: os.listdir('/private/tmp'))\n\
         attempt('vartmp', lambda: os.listdir('/private/var/tmp'))\n\
         attempt('volumes', lambda: os.listdir('/Volumes'))\n\
         attempt('users', lambda: os.listdir('/Users'))\n\
         attempt('home_env', lambda: os.listdir(os.environ['HOME']))",
    );
    let m = manifest(&["python3", "-I", "-S", "p.py", secret.to_str().unwrap(), root.to_str().unwrap()], false);
    let p = Plugin::spawn_with(&m, &f.dir(), Duration::from_secs(20), &Isolation::production(&PathBuf::from("/nonexistent-data"))).unwrap();
    for k in ["temp_secret", "root_list", "tmp", "vartmp", "volumes", "users"] {
        assert_eq!(res(&p, k), ("err".into(), json!("PermissionError")), "{k}");
    }
    // the run HOME (inside the denied root) stays usable, and so does the plugin dir (also in it)
    assert_eq!(res(&p, "home_env").0, "ok");
}

#[test]
fn writes_outside_the_run_home_are_denied() {
    let f = Fixture::new(
        "attempt('own_dir', lambda: open('new.txt', 'w').write('x'))\n\
         attempt('overwrite', lambda: open('p.py', 'a').write('#'))\n\
         attempt('sibling', lambda: open(os.path.join(sys.argv[2], 'dropped.txt'), 'w').write('x'))\n\
         attempt('tmp', lambda: open('/private/tmp/meetko-sandbox-probe', 'w').write('x'))\n\
         attempt('home', lambda: open(os.path.join(os.environ['HOME'], 'ok.txt'), 'w').write('x'))",
    );
    let [secret, root] = paths(&f);
    let p = f.spawn(&[&secret, &root], false);
    for k in ["own_dir", "overwrite", "sibling", "tmp"] {
        assert_eq!(res(&p, k).0, "err", "{k} must be denied");
    }
    assert_eq!(res(&p, "home").0, "ok");
    assert!(!f.root.path().join("dropped.txt").exists() && !f.dir().join("new.txt").exists());
    assert!(!Path::new("/private/tmp/meetko-sandbox-probe").exists());
}

#[test]
fn tcp_is_denied_without_the_network_permission_and_allowed_with_it() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    let f = Fixture::new("attempt('tcp', lambda: socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=5).close())");
    let denied = f.spawn(&[&port], false);
    assert_eq!(res(&denied, "tcp").0, "err", "network must be denied: {:?}", res(&denied, "tcp"));
    assert!(l.accept().is_err(), "the listener must not have seen a connection");
    let allowed = f.spawn(&[&port], true);
    assert_eq!(res(&allowed, "tcp").0, "ok", "{:?}", res(&allowed, "tcp"));
    assert!(l.accept().is_ok(), "the listener should have seen the connection");
}

#[test]
fn the_plugin_pid_is_the_spawned_pid_it_cannot_fork_and_timeout_kills_it() {
    let f = Fixture::new("pass");
    let m = manifest(&["python3", "-I", "-S", "p.py"], false);
    // answers initialize (reporting what it can start), then hangs in `run`. Without process-fork there
    // is no fork+setsid escape from the timeout kill: the plugin is the only process.
    std::fs::write(
        f.dir().join("p.py"),
        r#"import sys, json, os, time, subprocess
def attempt(f):
    try:
        f(); return "started"
    except Exception as e:
        return type(e).__name__
for line in sys.stdin:
    m = json.loads(line)
    if m["method"] == "initialize":
        r = {"contract": "0.1", "plugin": {"id": "t/fake", "version": "0"}, "outputs": {
            "pid": os.getpid(),
            "fork": attempt(lambda: os._exit(0) if os.fork() == 0 else None),
            "popen": attempt(lambda: subprocess.Popen([sys.executable, "-I", "-S", "-c", "pass"])),
            "spawn": attempt(lambda: os.posix_spawn(sys.executable, [sys.executable, "-c", "pass"], {})),
        }}
    else:
        time.sleep(60)
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#,
    )
    .unwrap();
    let mut p = Plugin::spawn_with(&m, &f.dir(), Duration::from_millis(1500), &f.iso()).unwrap();
    assert_eq!(p.sandbox, "macos-sbpl");
    let o = &p.info["outputs"];
    assert_eq!(o["pid"].as_u64().unwrap(), p.pid() as u64, "sandbox-exec must exec into the plugin");
    for k in ["fork", "popen", "spawn"] {
        assert_eq!(o[k], json!("PermissionError"), "{k} must be denied: {o}");
    }
    let err = p.call("run", json!({})).unwrap_err();
    assert!(matches!(err, meetko_core::error::Error::Timeout(_)), "{err}");
    let t = Instant::now();
    // the killed plugin is reaped: its pid no longer exists
    while unsafe { libc::kill(p.pid() as i32, 0) } == 0 && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_ne!(unsafe { libc::kill(p.pid() as i32, 0) }, 0, "the plugin survived the kill");
}

#[test]
fn off_runs_unsandboxed_and_says_so() {
    let f = Fixture::new("attempt('secret', lambda: open(sys.argv[1]).read())");
    let [secret, _] = paths(&f);
    let m = manifest(&["python3", "-I", "-S", "p.py", &secret], false);
    let p = Plugin::spawn_with(&m, &f.dir(), Duration::from_secs(20), &Isolation::off()).unwrap();
    assert_eq!(p.sandbox, "off");
    assert_eq!(res(&p, "secret"), ("ok".into(), json!("top secret")));
}

// ---- deny-by-default: no other programs, no system services, only the declared channels ----

/// Runs `argv` as a child of the plugin; reports "ok"/"err" and what happened.
const RUN_PROBE: &str = "def run(argv):\n    \
    try:\n        r = subprocess.run(argv, capture_output=True, timeout=10)\n        return ['ran', r.returncode]\n    \
    except Exception as e:\n        return [type(e).__name__]\n";

#[test]
fn a_plugin_cannot_start_other_programs() {
    let f = Fixture::new(&format!(
        "{RUN_PROBE}\
         attempt('open', lambda: run(['/usr/bin/open', 'https://example.invalid/?data=x']))\n\
         attempt('osascript', lambda: run(['/usr/bin/osascript', '-e', 'return 1']))\n\
         attempt('pbcopy', lambda: run(['/usr/bin/pbcopy']))\n\
         attempt('curl', lambda: run(['/usr/bin/curl', '-s', 'http://127.0.0.1:1/']))\n\
         attempt('sh', lambda: run(['/bin/sh', '-c', 'true']))\n\
         attempt('bash', lambda: run(['/bin/bash', '-c', 'true']))\n\
         attempt('env', lambda: run(['/usr/bin/env', 'true']))\n\
         attempt('sleep', lambda: run(['sleep', '0']))\n\
         attempt('os_system', lambda: os.system('/usr/bin/true'))\n\
         attempt('os_exec', lambda: os.execv('/bin/sh', ['sh']))\n\
         attempt('self', lambda: run([sys.executable, '-I', '-S', '-c', 'pass']))\n\
         attempt('fork', lambda: os.fork())"
    ));
    let p = f.spawn(&[], false);
    for k in ["open", "osascript", "pbcopy", "curl", "sh", "bash", "env", "sleep"] {
        assert_eq!(res(&p, k), ("ok".into(), json!(["PermissionError"])), "{k} must not start");
    }
    // os.system goes through /bin/sh: exits non-zero (127/126) instead of running anything
    assert_ne!(res(&p, "os_system"), ("ok".into(), json!(0)));
    assert_eq!(res(&p, "os_exec"), ("err".into(), json!("PermissionError")));
    // not even a copy of its own interpreter, and no bare fork
    assert_eq!(res(&p, "self"), ("ok".into(), json!(["PermissionError"])));
    assert_eq!(res(&p, "fork"), ("err".into(), json!("PermissionError")));
}

#[test]
fn a_plugin_cannot_reach_system_services() {
    // pasteboard, LaunchServices and AppleEvents are mach services; with no mach-lookup at all they fail
    let f = Fixture::new(
        "import ctypes, ctypes.util\n\
         def mach(name):\n    \
             lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib')\n    \
             port = ctypes.c_uint32(0)\n    \
             bootstrap = ctypes.c_uint32.in_dll(lib, 'bootstrap_port')\n    \
             lib.bootstrap_look_up.argtypes = [ctypes.c_uint32, ctypes.c_char_p, ctypes.POINTER(ctypes.c_uint32)]\n    \
             return lib.bootstrap_look_up(bootstrap, name.encode(), ctypes.byref(port))\n\
         attempt('pboard', lambda: mach('com.apple.pasteboard.1'))\n\
         attempt('lsd', lambda: mach('com.apple.coreservices.launchservicesd'))\n\
         attempt('appleevents', lambda: mach('com.apple.coreservices.appleevents'))\n\
         attempt('notify', lambda: mach('com.apple.usernoted.daemon_client'))\n\
         attempt('dns', lambda: mach('com.apple.dnssd.service'))",
    );
    let p = f.spawn(&[], false);
    for k in ["pboard", "lsd", "appleevents", "notify", "dns"] {
        let (kind, v) = res(&p, k);
        assert!(kind == "ok" && v.as_i64() != Some(0), "{k} must not resolve: {kind} {v}");
    }
}

#[test]
fn a_python_plugin_under_dash_i_can_import_the_stdlib() {
    let f = Fixture::new(
        "import json, re, datetime, collections, unicodedata, math, statistics, itertools, functools, string, textwrap, hashlib, decimal, fractions, heapq, bisect, difflib, unicodedata\n\
         attempt('mods', lambda: [\n    \
             json.loads('[1]')[0],\n    \
             bool(re.search(r'a+', 'baa')),\n    \
             datetime.datetime(2026, 1, 2).isoformat(),\n    \
             collections.Counter('aab').most_common(1)[0][0],\n    \
             unicodedata.normalize('NFC', 'e\\u0301') == '\\u00e9',\n    \
             math.sqrt(16),\n    \
             statistics.mean([1, 2, 3]),\n    \
             len(hashlib.sha256(b'x').hexdigest()),\n])\n\
         attempt('time', lambda: datetime.datetime.now().astimezone().tzinfo is not None)\n\
         attempt('random', lambda: len(os.urandom(8)))",
    );
    // -I but without -S: site is imported, as for a plugin that does not pass -S
    let m = manifest(&["python3", "-I", "p.py"], false);
    let p = Plugin::spawn_with(&m, &f.dir(), Duration::from_secs(20), &f.iso()).unwrap();
    assert_eq!(p.sandbox, "macos-sbpl");
    assert_eq!(res(&p, "mods"), ("ok".into(), json!([1, true, "2026-01-02T00:00:00", "a", true, 4.0, 2, 64])));
    assert_eq!(res(&p, "time"), ("ok".into(), json!(true)));
    assert_eq!(res(&p, "random"), ("ok".into(), json!(8)));
    // and with -S
    let p = f.spawn(&[], false);
    assert_eq!(res(&p, "mods").0, "ok");
}

#[test]
fn a_plugin_with_the_network_permission_can_resolve_and_connect_but_still_start_nothing() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    let f = Fixture::new(&format!(
        "{RUN_PROBE}\
         attempt('tcp', lambda: socket.create_connection(('localhost', int(sys.argv[1])), timeout=5).close())\n\
         attempt('resolve', lambda: bool(socket.getaddrinfo('localhost', 80)))\n\
         attempt('open', lambda: run(['/usr/bin/open', 'https://example.invalid/']))\n\
         attempt('curl', lambda: run(['/usr/bin/curl', '-s', 'http://127.0.0.1:1/']))"
    ));
    let p = f.spawn(&[&port], true);
    assert_eq!(res(&p, "tcp").0, "ok", "{:?}", res(&p, "tcp"));
    assert_eq!(res(&p, "resolve").0, "ok");
    assert!(l.accept().is_ok());
    for k in ["open", "curl"] {
        assert_eq!(res(&p, k), ("ok".into(), json!(["PermissionError"])), "{k}");
    }
}

#[test]
fn only_the_plugin_program_itself_may_be_exec_d() {
    // /bin/dash as the plugin program: it runs (it is the allowed exec), what it starts does not
    use meetko_core::sandbox::{policy_for_run, SANDBOX_EXEC};
    let run = |script: &str| {
        let pol = policy_for_run(&Isolation::deny(vec![]), false, Path::new("/nonexistent/plugin"), Path::new("/nonexistent/home"), Path::new("/bin/dash")).unwrap();
        std::process::Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(pol.to_sbpl().unwrap())
            .args(["/bin/dash", "-c", script])
            .output()
            .unwrap()
    };
    let o = run("echo alive");
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "alive", "{}", String::from_utf8_lossy(&o.stderr));
    let o = run("/usr/bin/true; echo rc=$?; /bin/ls >/dev/null 2>&1; echo rc=$?");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(!out.contains("rc=0"), "a child program started: {out}");
}

#[test]
fn the_bundled_example_plugins_run_sandboxed_and_pass_conformance() {
    use meetko_core::contract::MeetingData;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for name in ["action-items-py", "talk-time-rs"] {
        let dir = root.join("plugins").join(name);
        let (m, _) = Manifest::load(&dir).unwrap();
        let (prog, _) = m.resolve_command(&dir).unwrap();
        if prog.components().count() > 1 && !prog.exists() {
            assert!(std::env::var_os("MEETKO_REQUIRE_PLUGINS").is_none(), "{name} is not built");
            eprintln!("SKIP {name}: cargo build --release in plugins/{name}");
            continue;
        }
        for version in ["draft", "live"] {
            let path = root.join(format!("contract/fixtures/meeting01/meeting-data.{version}.json"));
            let md: MeetingData = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let mut p = Plugin::spawn_with(&m, &dir, Duration::from_secs(30), &Isolation::production(&PathBuf::from("/nonexistent-data"))).unwrap();
            assert_eq!(p.sandbox, "macos-sbpl", "{name}");
            let out = p.run_raw(&md.to_value()).unwrap();
            assert_eq!(meetko_core::contract::output_errors(&md, &out), Vec::<String>::new(), "{name} {version}");
            assert!(!out["items"].as_array().unwrap().is_empty(), "{name} {version}");
            p.close().unwrap();
        }
    }
}

// ---- other processes, hardware, IPC, W^X, sockets, interpreters ----

/// A non-platform child process (this test binary, sleeping) with a marker in its environment. Platform
/// binaries such as /bin/sleep hide their environment from KERN_PROCARGS2 even unsandboxed, which would
/// make a "blocked" result meaningless.
struct MarkerChild {
    child: std::process::Child,
    marker: String,
}

impl MarkerChild {
    fn start() -> MarkerChild {
        let marker = format!("meetko-marker-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["marker_sleeper", "--ignored", "--exact", "--nocapture"])
            .env_clear()
            .env("MEETKO_PROBE_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        MarkerChild { child, marker }
    }
    fn pid(&self) -> u32 {
        self.child.id()
    }
    /// Unsandboxed control: can this process read the child's environment at all?
    fn readable_here(&self) -> bool {
        let mut mib = [libc::CTL_KERN, 49 /* KERN_PROCARGS2 */, self.pid() as i32];
        let mut buf = vec![0u8; 1 << 16];
        let mut len = buf.len();
        // SAFETY: mib has 3 entries, buf is valid for len bytes
        let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        r == 0 && String::from_utf8_lossy(&buf[..len]).contains(&self.marker)
    }
}

impl Drop for MarkerChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore]
fn marker_sleeper() {
    std::thread::sleep(Duration::from_secs(120));
}

const PROCARGS_PROBE: &str = "import ctypes\n\
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)\n\
    libproc = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)\n\
    def sysctl_bytes(mib, n):\n    \
        m = (ctypes.c_int * n)(*mib)\n    \
        sz = ctypes.c_size_t(0)\n    \
        if libc.sysctl(m, n, None, ctypes.byref(sz), None, 0) != 0: raise OSError(ctypes.get_errno(), 'sysctl')\n    \
        buf = ctypes.create_string_buffer(sz.value)\n    \
        if libc.sysctl(m, n, buf, ctypes.byref(sz), None, 0) != 0: raise OSError(ctypes.get_errno(), 'sysctl')\n    \
        return buf.raw[:sz.value]\n\
    def listpids():\n    \
        n = libproc.proc_listpids(1, 0, None, 0)\n    \
        if n <= 0: raise OSError('proc_listpids')\n    \
        return n\n\
    attempt('procargs', lambda: sys.argv[2].encode() in sysctl_bytes([1, 49, int(sys.argv[1])], 3))\n\
    attempt('proc_all', lambda: len(sysctl_bytes([1, 14, 0], 3)) > 0)\n\
    attempt('listpids', listpids)\n\
    attempt('own_pidinfo', lambda: libproc.proc_pidpath(os.getpid(), ctypes.create_string_buffer(1024), 1024) > 0)";

#[test]
fn a_plugin_cannot_read_other_processes_command_line_or_environment_or_list_them() {
    let target = MarkerChild::start();
    std::thread::sleep(Duration::from_millis(500));
    let f = Fixture::new(PROCARGS_PROBE);
    let (pid, marker) = (target.pid().to_string(), target.marker.clone());
    // control: the same probe without the sandbox does read the other process's environment, so a
    // "blocked" result below is not an artefact of the probe
    assert!(target.readable_here(), "the marker must be readable unsandboxed for this test to mean anything");
    let off = f.spawn_iso(&[&pid, &marker], false, &Isolation::off());
    assert_eq!(res(&off, "procargs"), ("ok".into(), json!(true)), "control");
    assert_eq!(res(&off, "listpids").0, "ok", "control");
    assert_eq!(res(&off, "proc_all").0, "ok", "control");
    drop(off);
    let p = f.spawn(&[&pid, &marker], false);
    assert_eq!(p.sandbox, "macos-sbpl");
    for k in ["procargs", "proc_all", "listpids"] {
        assert_eq!(res(&p, k).0, "err", "{k} must be denied: {:?}", res(&p, k));
    }
    // the plugin's own process info still works (libc and Python need it)
    assert_eq!(res(&p, "own_pidinfo"), ("ok".into(), json!(true)));
}

#[test]
fn system_volumes_and_the_hardware_serial_are_unreadable() {
    let f = Fixture::new(
        "import ctypes\n\
         cf = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')\n\
         io = ctypes.CDLL('/System/Library/Frameworks/IOKit.framework/IOKit')\n\
         cf.CFStringCreateWithCString.restype = ctypes.c_void_p\n\
         cf.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]\n\
         io.IOServiceMatching.restype = ctypes.c_void_p\n\
         io.IOServiceMatching.argtypes = [ctypes.c_char_p]\n\
         io.IOServiceGetMatchingService.restype = ctypes.c_uint32\n\
         io.IOServiceGetMatchingService.argtypes = [ctypes.c_uint32, ctypes.c_void_p]\n\
         io.IORegistryEntryCreateCFProperty.restype = ctypes.c_void_p\n\
         io.IORegistryEntryCreateCFProperty.argtypes = [ctypes.c_uint32, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]\n\
         def serial():\n    \
             svc = io.IOServiceGetMatchingService(0, io.IOServiceMatching(b'IOPlatformExpertDevice'))\n    \
             if not svc: raise OSError('no service')\n    \
             key = cf.CFStringCreateWithCString(None, b'IOPlatformSerialNumber', 0x08000100)\n    \
             if not io.IORegistryEntryCreateCFProperty(svc, key, None, 0): raise OSError('no property')\n    \
             return True\n\
         attempt('serial', serial)\n\
         attempt('preboot', lambda: len(os.listdir('/System/Volumes/Preboot')) >= 0)\n\
         attempt('update', lambda: len(os.listdir('/System/Volumes/Update')) >= 0)\n\
         attempt('system', lambda: len(os.listdir('/System/Library')) > 0)",
    );
    // control (unsandboxed): the serial (only success/failure is recorded, never the value) and Preboot are reachable
    let off = f.spawn_iso(&[], false, &Isolation::off());
    assert_eq!(res(&off, "serial"), ("ok".into(), json!(true)), "control");
    assert_eq!(res(&off, "preboot").0, "ok", "control");
    drop(off);
    let p = f.spawn(&[], false);
    assert_eq!(res(&p, "serial").0, "err");
    assert_eq!(res(&p, "preboot"), ("err".into(), json!("PermissionError")));
    assert_eq!(res(&p, "update"), ("err".into(), json!("PermissionError")));
    assert_eq!(res(&p, "system").0, "ok", "the rest of /System stays readable");
}

#[test]
fn code_cannot_be_loaded_from_the_writable_run_home() {
    // a real dylib on disk, readable to the sandboxed python: the developer tools' libxcrun
    let lib = ["/Library/Developer/CommandLineTools/usr/lib/libxcrun.dylib"].into_iter().map(PathBuf::from).find(|p| p.exists());
    let Some(lib) = lib else {
        eprintln!("SKIP: no libxcrun.dylib (Command Line Tools) to copy");
        return;
    };
    let f = Fixture::new(
        "import ctypes, shutil\n\
         def load():\n    \
             dst = os.path.join(os.environ['HOME'], 'dropped.dylib')\n    \
             shutil.copyfile(sys.argv[1], dst)\n    \
             ctypes.CDLL(dst)\n    \
             return True\n\
         attempt('dlopen_home', load)",
    );
    let off = f.spawn_iso(&[lib.to_str().unwrap()], false, &Isolation::off());
    assert_eq!(res(&off, "dlopen_home"), ("ok".into(), json!(true)), "control: the copy loads without the sandbox");
    drop(off);
    let p = f.spawn(&[lib.to_str().unwrap()], false);
    assert_eq!(res(&p, "dlopen_home").0, "err", "{:?}", res(&p, "dlopen_home"));
}

#[test]
fn system_v_shared_memory_made_by_a_plugin_is_removed_with_the_run() {
    let f = Fixture::new(
        "import ctypes\n\
         libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)\n\
         libc.shmget.argtypes = [ctypes.c_int, ctypes.c_size_t, ctypes.c_int]\n\
         attempt('shm', lambda: libc.shmget(0, 4096, 0o1000 | 0o600))",
    );
    let p = f.spawn(&[], false);
    let (kind, id) = res(&p, "shm");
    assert_eq!(kind, "ok");
    let id = id.as_i64().unwrap() as i32;
    assert!(id >= 0, "the sandbox cannot stop shmget, so the host must clean up");
    let exists = |id: i32| {
        let mut ds = std::mem::MaybeUninit::<libc::shmid_ds>::zeroed();
        // SAFETY: IPC_STAT writes a shmid_ds into the valid out-buffer
        unsafe { libc::shmctl(id, libc::IPC_STAT, ds.as_mut_ptr()) == 0 }
    };
    assert!(exists(id), "the segment exists while the plugin lives");
    drop(p);
    assert!(!exists(id), "the segment must be removed when the run ends");
}

#[test]
fn unix_sockets_stay_denied_even_with_the_network_permission() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    // a Unix-domain listener in a short path (sun_path is 104 bytes), outside the denied fixture tree
    let dir = tempfile::Builder::new().prefix("mk").tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("s");
    let ul = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    ul.set_nonblocking(true).unwrap();
    let f = Fixture::new(
        "attempt('unix', lambda: socket.socket(socket.AF_UNIX).connect(sys.argv[2]))\n\
         attempt('tcp', lambda: socket.create_connection(('localhost', int(sys.argv[1])), timeout=5).close())\n\
         attempt('udp', lambda: socket.socket(socket.AF_INET, socket.SOCK_DGRAM).sendto(b'x', ('127.0.0.1', 9)))\n\
         attempt('resolve', lambda: bool(socket.getaddrinfo('localhost', 80)))\n\
         attempt('listen', lambda: (lambda s: (s.bind(('127.0.0.1', 0)), s.listen(1)))(socket.socket()))",
    );
    let off = f.spawn_iso(&[&port, sock.to_str().unwrap()], true, &Isolation::off());
    assert_eq!(res(&off, "unix").0, "ok", "control: the Unix socket is reachable unsandboxed");
    drop(off);
    while ul.accept().is_ok() {}
    let p = f.spawn(&[&port, sock.to_str().unwrap()], true);
    assert_eq!(res(&p, "unix"), ("err".into(), json!("PermissionError")));
    assert!(ul.accept().is_err(), "the Unix listener must not have seen a connection");
    // IP keeps working: TCP to localhost, UDP, name resolution, listening
    for k in ["tcp", "udp", "resolve", "listen"] {
        assert_eq!(res(&p, k).0, "ok", "{k}: {:?}", res(&p, k));
    }
    assert!(l.accept().is_ok());
}

#[test]
fn an_interpreter_that_would_open_a_home_dot_dir_is_refused_up_front() {
    use meetko_core::sandbox::policy_for_run;
    // the real spawn path: a fake home with ~/.local/bin/python3.12 (a symlink would be resolved; a plain file stays)
    let home = tempfile::tempdir().unwrap();
    let home_c = std::fs::canonicalize(home.path()).unwrap();
    let bin = home_c.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy("/bin/dash", bin.join("python3.12")).unwrap();
    let iso = Isolation::deny(vec![home_c.clone()]).with_home(home_c.clone());
    let e = policy_for_run(&iso, false, Path::new("/nonexistent/plugin"), Path::new("/nonexistent/run"), &bin.join("python3.12")).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("/usr/bin/python3") && msg.contains(".local"), "{msg}");
    // no ~/.local or ~/.cargo tree is ever added to a policy that did succeed
    let ok = policy_for_run(&iso, false, Path::new("/nonexistent/plugin"), Path::new("/nonexistent/run"), Path::new("/usr/bin/python3")).unwrap();
    assert!(ok.allow_read_exec.iter().all(|p| !p.starts_with(&home_c)));
}

/// fsid (two i32) of the volume holding `p` and its inode, as decimal args for the probe
#[cfg(test)]
fn fsid_and_inode(p: &Path) -> [String; 3] {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statfs(c.as_ptr(), &mut st) }, 0);
    let id: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    [id[0].to_string(), id[1].to_string(), p.metadata().unwrap().ino().to_string()]
}

#[test]
fn fsgetpath_cannot_turn_an_inode_into_a_path_in_a_denied_tree() {
    let f = Fixture::new(
        "import ctypes\n\
         libc = ctypes.CDLL(None, use_errno=True)\n\
         libc.fsgetpath.restype = ctypes.c_ssize_t\n\
         libc.fsgetpath.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_uint64]\n\
         a, b, ino = (int(x) for x in sys.argv[1:4])\n\
         def fsgp():\n    \
             fsid = (ctypes.c_int32 * 2)(a, b)\n    \
             buf = ctypes.create_string_buffer(1024)\n    \
             n = libc.fsgetpath(buf, 1024, fsid, ino)\n    \
             if n <= 0: raise OSError(ctypes.get_errno(), 'fsgetpath')\n    \
             return buf.value.endswith(b'secret.txt')\n\
         attempt('fsgetpath', fsgp)\n\
         attempt('own_getcwd', lambda: len(os.getcwd()) > 0)",
    );
    let [a, b, ino] = fsid_and_inode(&f.secret());
    let args = [a.as_str(), b.as_str(), ino.as_str()];
    let off = f.spawn_iso(&args, false, &Isolation::off());
    assert_eq!(res(&off, "fsgetpath"), ("ok".into(), json!(true)), "control: unsandboxed, the inode resolves to the path");
    drop(off);
    for network in [false, true] {
        let p = f.spawn(&args, network);
        assert_eq!(res(&p, "fsgetpath").0, "err", "network={network}");
        assert_eq!(res(&p, "own_getcwd").0, "ok");
    }
}

#[test]
fn the_host_uuid_and_mac_addresses_and_preboot_uuid_are_unreadable() {
    let f = Fixture::new(
        "import ctypes\n\
         libc = ctypes.CDLL(None, use_errno=True)\n\
         class TS(ctypes.Structure):\n    _fields_ = [('s', ctypes.c_long), ('n', ctypes.c_long)]\n\
         def hostuuid():\n    \
             buf = (ctypes.c_ubyte * 16)()\n    \
             if libc.gethostuuid(buf, ctypes.byref(TS(5, 0))) != 0: raise OSError(ctypes.get_errno(), 'gethostuuid')\n    \
             return any(buf)\n\
         attempt('hostuuid', hostuuid)\n\
         class Ifa(ctypes.Structure): pass\n\
         Ifa._fields_ = [('next', ctypes.POINTER(Ifa)), ('name', ctypes.c_char_p), ('flags', ctypes.c_uint32), ('addr', ctypes.c_void_p), ('mask', ctypes.c_void_p), ('dst', ctypes.c_void_p), ('data', ctypes.c_void_p)]\n\
         def macs():\n    \
             head = ctypes.POINTER(Ifa)()\n    \
             if libc.getifaddrs(ctypes.byref(head)) != 0: raise OSError(ctypes.get_errno(), 'getifaddrs')\n    \
             n, cur = 0, head\n    \
             while cur:\n        \
                 a = cur.contents.addr\n        \
                 if a:\n            \
                     raw = ctypes.string_at(a, 8)\n            \
                     if raw[1] == 18 and raw[6] == 6: n += 1\n        \
                 cur = cur.contents.next\n    \
             return n\n\
         attempt('macs', macs)\n\
         def sysctl_str(name):\n    \
             n = ctypes.c_size_t(0)\n    \
             if libc.sysctlbyname(name, None, ctypes.byref(n), None, 0) != 0: raise OSError(ctypes.get_errno(), 'sysctl')\n    \
             return n.value > 0\n\
         attempt('preboot', lambda: sysctl_str(b'kern.apfsprebootuuid'))\n\
         attempt('route', lambda: sysctl_str(b'net.route'))\n\
         attempt('ncpu', lambda: sysctl_str(b'hw.ncpu'))\n\
         attempt('resolve', lambda: len(socket.getaddrinfo('localhost', 80)) > 0)",
    );
    let off = f.spawn_iso(&[], false, &Isolation::off());
    assert_eq!(res(&off, "hostuuid"), ("ok".into(), json!(true)), "control");
    assert!(res(&off, "macs").1.as_i64().unwrap_or(0) > 0, "control: unsandboxed, the interface list has a MAC address");
    drop(off);
    for network in [false, true] {
        let p = f.spawn(&[], network);
        assert_eq!(res(&p, "hostuuid").0, "err", "network={network}");
        let macs = res(&p, "macs");
        assert!(macs.0 == "err" || macs.1 == json!(0), "network={network}");
        assert_eq!(res(&p, "preboot").0, "err", "network={network}");
        assert_eq!(res(&p, "route").0, "err", "network={network}");
        assert_eq!(res(&p, "ncpu").0, "ok", "other sysctls stay readable");
        if network {
            assert_eq!(res(&p, "resolve").0, "ok", "name resolution works without the interface list");
        }
    }
}
