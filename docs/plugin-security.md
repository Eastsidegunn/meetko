# Plugin security, contract 0.1

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

What meetko 0.1 does and does not do about a plugin you add. Read this before adding code you did not write.

## Enforced

- **Consent.** `meetko plugin add` prints the declared permissions and needs `--yes` (or `y` at a prompt on a terminal).
- **Verified copy.** `add` (CLI and app) copies the plugin directory into `<data>/plugins/<id>@<version>-<code sha prefix>/`
  (mode 0700) and reads the manifest, computes both hashes and runs from that copy, never from the directory the user
  picked. A source that differs from the hash shown to the user is refused. Symlinks in a plugin are refused. A compiled
  plugin's executable under `target/` is copied by the name `run.command` gives it.
- **Pinning.** `add` records a manifest hash (sha256 of `meetko.toml`) and a code hash (sha256 over the sorted relative
  paths and contents of every file in the plugin directory, excluding `target/`, `.git/` and `__pycache__/`; symlinks
  are hashed by their target text, not followed). Re-adding the same `id@version` with a different directory, manifest
  or code is refused. Before every run the code and manifest are hashed again and the run is refused if either changed.
  The code hash also covers the files `run.command` names, wherever they are in the plugin directory (so a compiled
  plugin's binary under `target/` is hashed, and a missing program is an error).
  Every stored result keeps a snapshot: manifest hash, code hash, declared network, declared audio, and the sandbox it ran under.
- **No shell.** `run.command` is an array; it is run directly. A program with a path separator (`/` or `\`) must be
  relative and inside the plugin directory (no absolute path, drive letter or `..`); a bare name is looked up on `PATH`.
- **Minimal environment.** The plugin gets `PATH` and `LANG` from the parent, a fresh empty `HOME` (mode 0700, deleted after
  the run; on Windows also `TEMP`, `TMP`, `USERPROFILE` pointing there, plus `SystemRoot` and `PATHEXT`), `PYTHONUTF8=1`
  and `PYTHONIOENCODING=utf-8`. Nothing else of the parent's environment (tokens, API keys) is passed on to the plugin
  process. This is about what is handed over at start, not about what the plugin can read afterwards: see the
  process-info line under the OS sandbox and the environment-variable gap below.
- **Resource limits that exist:** a per-request time budget (default 60 s; the plugin and its whole process tree are
  killed on expiry), a 16 MB cap on one reply line, valid UTF-8 only, bounded buffering. Plugin output is validated
  against the plugin-output schema and the anchors it uses; anything wrong rejects the whole result.
- **The plugin sees only the meeting data.** It is given the stored transcript document on stdin. Audio is never passed.
- **OS sandbox (macOS only).** Every plugin run goes through `/usr/bin/sandbox-exec` with a generated profile
  (`crates/meetko-core/src/sandbox.rs`). The profile is **deny-by-default** (`(version 1) (deny default)`): a plugin can
  compute on its stdin and write its stdout, and nothing else is allowed unless listed here. It does not
  `(import "bsd.sb")`: that pulls in `system.sb`, which allows `mach-lookup` of all XPC service names, `mach-bootstrap` and
  all raw syscalls, the very channels this profile closes.
  - **Programs:** `process-fork` is **denied**: a plugin cannot start a child process of any kind (`fork`, `subprocess`,
    `posix_spawn`, `os.system`); threads are fine. The plugin is the only process of its run, so the timeout kill reaches
    all of it (no fork+`setsid` escape). `process-exec` only for the plugin program itself (as resolved, both the
    path given and its canonical form) and, for Apple's `/usr/bin/python3`, the Python framework of the active developer
    tools (reached by exec, not by fork). That shim runs xcrun logic, then execs `<developer dir>/Library/Frameworks/Python3.framework/.../python3.9`,
    which execs `.../Resources/Python.app/Contents/MacOS/Python`; the developer dir is the Command Line Tools
    (`/Library/Developer/CommandLineTools`) and the one `xcode-select` points at. Nothing else can be started:
    `/usr/bin/open` (hands a URL to LaunchServices, so the browser, outside the sandbox, makes the request), `osascript`,
    `pbcopy`, `curl`, `/bin/sh`, `env`, `sleep`, and a copy of its own interpreter all fail with a permission error. A script plugin whose `#!` line names another interpreter is not supported (the interpreter is not
    allowed); use `["python3", ...]` or a compiled program as the manifest does.
  - **Mach services:** none by default (`mach-lookup` is not allowed at all), so pasteboard, LaunchServices, AppleEvents,
    notifications, logd and directory services are unreachable. Consequences, measured with Python and a Rust binary:
    no `os_log`, no `pwd.getpwuid()` (needs `opendirectoryd.libinfo`; `$HOME` and `os.getlogin()` still work), and the
    xcrun shim cannot create its cache in the per-user temp dir, so it prints harmless `xcrun`/SDK warnings to stderr.
    `libinfo`, `logd` and `notification_center` are not allowed, because no plugin needs them.
  - **Reads:** `/System`, `/usr/lib`, `/usr/share`, `/private/var/db/timezone` and `/private/var/select` (dyld, libc,
    the interpreter's libraries, time zones, the xcode-select link), a few `/dev` nodes (`urandom`, `random`, `zero`,
    `null`), and `/private/etc/{localtime,passwd,protocols,services}` (libc lookups; the same set `system.sb` allows). Plus
    the plugin's own installed copy and its per-run HOME, and for the interpreter its install prefix (see
    "Interpreters" below; mappable as code, which Python's extension modules need). `/System/Volumes` (Preboot with account
    plists and login preferences, Update logs) is denied inside `/System`. Not readable: `/Library`, `/Applications`, `/opt`,
    other of `/private/etc` (including `/private/etc/ssl` unless the network is declared), `/private`, `/Volumes`,
    `/Users`. Metadata (existence, stat) of the directories above an allowed path is readable, nothing more.
  - **Extra deny layer (kept):** the user's home, all of `/Users`, `/Volumes`, `/private/tmp`, `/private/var/tmp`, the
    per-user temp/cache root `/private/var/folders/<xx>/<yy>` and the meetko data dir are denied for reading, ahead of the
    re-allows for the plugin's copy, run HOME and interpreter prefix (later rules win). Under deny-default these are
    redundant; they stay so a later widening of the baseline cannot expose them.
  - **Interpreters:** only the **canonical** (symlink-resolved) interpreter path counts, never what a symlink on `PATH`
    points through. Its install tree is opened read-only and mappable: `…/<prefix>/bin/python3` gives `<prefix>`, a
    python.org framework `…/Python.framework/Versions/<v>`. meetko **refuses to run** (error suggests `/usr/bin/python3`)
    when that tree would be the home directory, a direct child of it or less than three levels below it (`~/.local`,
    `~/bin`, `~/.cargo`), anything inside a credential dir (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config`, `~/.cargo`,
    `~/.docker`, `~/.kube`, `~/.azure`, `~/.cache`, `~/.npm`), a broad root (`/`, `/opt`, `/usr`, `/usr/local`,
    `/opt/homebrew`, `/Library`, …), or any Homebrew / `/usr/local` install. Homebrew interpreters are refused because
    their dylibs live in other kegs that cannot be derived reliably; use `/usr/bin/python3` or a compiled plugin. A
    pyenv or uv Python at `~/.pyenv/versions/3.12` or `~/.local/share/uv/python/<build>` is allowed (tree only; untested here).
    Apple's `/usr/bin/python3` is the tested case.
  - **Writes:** only the per-run HOME and `/dev/null`. A plugin cannot modify its own installed copy or `config.toml`.
    Nothing under the run HOME can be mapped as code (`file-map-executable` denied): a library the plugin writes
    there cannot be `dlopen`ed.
  - **Network:** nothing unless `permissions.network = true` (consented at add time). Then IP only:
    `(allow network-outbound (remote ip))`, `network-bind` and `network-inbound` with `(local ip)`; **not** `network*`, which
    would also open every Unix-domain socket (`docker.sock`, `usbmuxd`, cupsd, syslog, sockets in `/tmp`). Plus what name
    resolution needs: `mach-lookup com.apple.dnssd.service` (mDNSResponder), `/private/etc/hosts`, `resolv.conf` and
    `/private/etc/ssl` (OpenSSL roots). Verified with Python (`getaddrinfo` and an HTTPS request). A TLS client that goes
    through Security.framework (for example Rust `native-tls`) may need `trustd` mach services that are **not** allowed;
    that was not tested, use a client with bundled roots (rustls) or Python.
  - **Other processes:** `process-info*` is limited to the plugin itself and `sysctl-read` of `kern.proc*` is denied.
    Together these stop `proc_listpids`/`proc_pidinfo`, `KERN_PROC_ALL` (the process list) and `KERN_PROCARGS2` (the
    command line **and environment** of other processes, including the meetko host's). Verified by a test that reads a
    marker variable in the environment of another process unsandboxed and fails to do so in the sandbox. Neither rule
    alone blocks `KERN_PROCARGS2`; the pair does.
  - **Path and hardware identifiers:** the syscalls `fsgetpath`/`fsgetpath_ext` (inode to path, which would list names
    in denied trees) and `gethostuuid` are denied, as are `sysctl-read` of `net.route*` (the interface list, so no MAC
    addresses via `getifaddrs`) and `kern.apfsprebootuuid`. These are denied with and without the network permission:
    Python's sockets, TLS, name resolution and localhost TCP work without them (verified). Verified with an unsandboxed
    control that succeeds.
  - **Other:** `sysctl-read` (libc and Python read `hw.*`, `kern.*`, except `kern.proc*`), `signal` to itself only.
    `iokit-get-properties` is denied (no hardware serial number or registry properties). No `iokit`, `system-socket` or
    `user-preference` access. System V shared memory cannot be blocked, so the host removes the segments a plugin created
    when the run ends (see Known gaps for semaphores and message queues).
  - **Fail closed:** if `sandbox-exec` is missing or cannot apply a profile (for example when meetko itself already runs
    inside a sandbox that forbids nesting), the plugin is not run and the error says why. The check applies a
    `(deny default)` profile like a plugin's, so a nested or restrictive environment is found before the first plugin.
  - **Dev opt-out:** `MEETKO_PLUGIN_SANDBOX=off` runs the plugin without a sandbox. Meant for development only.
  - **Recorded:** each stored result keeps `sandbox` = `macos-sbpl`, `none` (no sandbox exists on this OS) or `off`
    (opted out); `meetko run` prints it. Results stored before this field existed show `none`.
  - Verified by tests with a real `sandbox-exec` (`crates/meetko-core/tests/sandbox.rs`): reads in a denied tree fail, the
    plugin's own dir is readable, writes outside the run HOME fail, `open`/`osascript`/`pbcopy`/`curl`/`sh`/`env` cannot
    be started, `/bin/dash` as plugin program cannot start children, `fork`, `subprocess` and `posix_spawn` fail and a
    timed-out plugin is gone, pasteboard/LaunchServices/AppleEvents mach lookups fail, the process list and another
    process's environment cannot be read (with an unsandboxed control that can), `/System/Volumes/Preboot` and the IOKit
    serial number are unreadable (controls: readable unsandboxed), a library written to the run HOME cannot be loaded, a
    System V shared memory segment is removed after the run,
    TCP to a local listener is refused without the network permission and allowed with it (name resolution, UDP and
    listening on localhost work too, but a Unix-domain socket connect is denied even with the network permission),
    an interpreter under a home dot-dir, or whose install prefix overlaps a denied tree or is a whole `/Volumes` drive or
    `/Users` entry, is refused,
    Python under `-I` imports the common standard library, both bundled examples pass conformance under the production
    policy, and the plugin's pid is the spawned pid (so the timeout kill still reaches the whole process tree).

## Text shown to people

Manifest fields, gallery rows and the CLI's error messages are text from a remote author. Fields that are shown (id,
version, description, license, homepage, display lines, command) are refused when they contain a character of Unicode
category Cc, Cf, Zl or Zp, a Hangul filler, U+034F, a variation selector or a tag character, or more than 8 consecutive
combining marks (`crates/meetko-core/src/text.rs`; `tools/plugin-index/build_index.py` applies the same rule). Error
messages, including a TOML parse error that echoes a source line and archive entry names, are escaped as `\u{..}` before
they reach the terminal; long names are truncated.

## Declared, not enforced

- `input.audio` is a declaration. 0.1 never passes audio to a plugin, and the sandbox denies the data dir where recordings live.
- On **Windows and Linux there is no sandbox**: `permissions.network` is only a declaration there, and a plugin runs as
  your user and can read every file your account can read. Runs are recorded as `sandbox: none`.

## Known gaps

- **Where the data dir is not protected** (Windows/Linux): a plugin can read the data dir and write `config.toml` there. meetko
  never takes an **executable** from `config.toml` (capture provider only from env `MEETKO_CAPTURE_BIN`, a sibling of the
  app executable, or the debug dev tree; `whisper-cli` only from env, a sibling or PATH). `config.toml` still holds data
  settings (model path and its sha256), which is why the model's sha256 is recorded in the version's provenance.
  The same plugin can also write `<data dir>/models/` (the live-caption and diarization models the app downloaded) and
  `app-settings.json` (meeting prompt switches, the setup card). A swapped model file is not used: every file in
  `models/` is hashed against the pins in `engines/*/models.toml` before each use and refused on a mismatch
  (`docs/model-setup.md`). Changing `app-settings.json` can turn the meeting prompt on or off; it never starts a
  recording. No executable is ever taken from the data dir.
- **sandbox-exec is deprecated** by Apple. It works on current macOS and is what meetko uses; the supported replacement is an
  App Sandbox helper with restricted entitlements, which has not been tried.
- **Recordings outside known trees.** The profile is deny-by-default, but the extra deny layer only names the home, `/Users`,
  `/Volumes`, temp dirs and the data dir. Everything else is already unreadable by default; the only readable
  system locations are those listed under Enforced.
- **Interpreter lookup.** A bare name is found on `PATH` by meetko, then resolved to its canonical path (see
  "Interpreters" above). Interpreters outside `/usr/bin` are allowed only for narrow install trees; pyenv/uv/python.org
  paths are untested here, only Apple's `/usr/bin/python3` is. Homebrew Pythons are refused, so with the sandbox on meetko resolves a bare
  `python3` to Apple's `/usr/bin/python3` before looking at `PATH`; that shim needs the Xcode Command Line Tools
  (`xcode-select --install`), and without them meetko refuses to run the plugin instead of opening the install dialog. A script whose `#!/usr/bin/env` finds another interpreter, or an interpreter that needs
  files elsewhere (for example `~/.cache`, user site-packages, `/Library/Python`), will fail inside the sandbox.
- **Environment variables of your other processes (macOS).** The sandbox denies the process list and `KERN_PROCARGS2`
  (see Enforced), verified for same-user non-system processes. This relies on the interaction of two profile rules that
  Apple does not document; a future macOS may change it. Keep secrets out of environment variables: treat anything in
  the command line or environment of a process you run as readable by code running as your user, which on Windows and
  Linux (no sandbox) includes plugins.
- **System V semaphores and message queues.** No SBPL rule stops `semget`/`msgget`, and the sandboxed plugin cannot
  remove what it made. Shared memory is removed by the host after each run (by creator pid); semaphore sets and message
  queues carry no creator pid, so they cannot be attributed to a run safely and may leak until reboot (small kernel
  tables: a hostile plugin can exhaust them and break other software's System V IPC). Remove with `ipcs`/`ipcrm`.
- **Kernel surface.** The profile allows `sysctl-read` and does not stop a plugin from using the kernel directly (it is
  only a filter on named operations); a sandbox escape in the kernel or in `sandbox-exec` itself is out of scope.
  Unix-domain sockets are denied with or without the network permission.
- **Not a resource limit.** CPU, memory, process count and disk in the per-run HOME are not capped (only the time budget).
  A plugin cannot fork, but can still use CPU, memory and threads until the time budget kills it.
- **Declared network is all-or-nothing.** A plugin with `permissions.network` can reach any host, including localhost services.
  It can also send out the transcript it was given: the sandbox cannot tell its own traffic from an upload.
- A plugin that is added once can change what it does through anything outside its directory (system Python packages,
  network content). Hashing covers its own files only.
- The code hash is rechecked before every run; a file swapped between that check and the interpreter reading it is a
  time-of-check/time-of-use race that is not closed (the add-time race is closed by the verified copy above). On macOS the sandbox
  makes the installed copy read-only to the plugin; elsewhere a plugin can modify its own installed copy.

## Enforcement path (still open)

- **Windows:** start the plugin in an AppContainer (or a restricted token with a low integrity level) inside the Job
  Object the host already uses, with no network capability unless declared, and file access limited to the plugin
  directory and the run directory. Needs a Windows machine to build and test.
- **Linux:** not started (candidates: landlock + seccomp, or bubblewrap).
- **macOS:** replace sandbox-exec with an App Sandbox helper.
- **All:** pass audio, when `input.audio = "full"` is approved, as a file descriptor or a path inside the allowed
  directory, not through the data dir.
