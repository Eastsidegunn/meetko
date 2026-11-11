//! `meetko plugin install github:...` and `plugin info` against a local HTTP server.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn tarball() -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/plugins/action-items-py");
    let mut b = tar::Builder::new(Vec::new());
    for name in ["meetko.toml", "plugin.py"] {
        let data = std::fs::read(dir.join(name)).unwrap();
        let mut h = tar::Header::new_gnu();
        let path = format!("meetko-r-{SHA}/{name}");
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, data.as_slice()).unwrap();
    }
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&b.into_inner().unwrap()).unwrap();
    e.finish().unwrap()
}

fn server() -> String {
    server_with(tarball())
}

/// the fixture plugin in `plugins/action-items-py/` of a catalog repo, next to an unrelated file
fn catalog_tarball() -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/plugins/action-items-py");
    let mut b = tar::Builder::new(Vec::new());
    let mut files: Vec<(String, Vec<u8>)> = vec![("README.md".into(), b"catalog".to_vec())];
    for name in ["meetko.toml", "plugin.py"] {
        files.push((format!("plugins/action-items-py/{name}"), std::fs::read(dir.join(name)).unwrap()));
    }
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        let path = format!("meetko-r-{SHA}/{name}");
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, data.as_slice()).unwrap();
    }
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&b.into_inner().unwrap()).unwrap();
    e.finish().unwrap()
}

fn server_with(data: Vec<u8>) -> String {
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        for req in srv.incoming_requests() {
            let url = req.url().to_string();
            let parts: Vec<&str> = url.trim_start_matches('/').split('/').collect();
            // any owner, repo "r": the publisher rule is what stops `other/r`
            let _ = if matches!(parts.as_slice(), ["repos", _, "r", "commits", "v1"]) {
                req.respond(tiny_http::Response::from_string(SHA))
            } else if matches!(parts.as_slice(), ["repos", _, "r"]) {
                req.respond(tiny_http::Response::from_string(r#"{"default_branch":"main"}"#))
            } else if matches!(parts.as_slice(), ["repos", _, "r", "compare", _]) {
                req.respond(tiny_http::Response::from_string(r#"{"status":"behind"}"#))
            } else if matches!(parts.as_slice(), [_, "r", "tar.gz", sha] if *sha == SHA) {
                req.respond(tiny_http::Response::from_data(data.clone()))
            } else {
                req.respond(tiny_http::Response::from_string("no").with_status_code(404))
            };
        }
    });
    base
}

fn meetko(data: &std::path::Path, base: &str, args: &[&str]) -> (String, String, bool) {
    let o = Command::new(env!("CARGO_BIN_EXE_meetko"))
        .args(args)
        .env("MEETKO_DATA_DIR", data)
        .env("MEETKO_GITHUB_API", base)
        .env("MEETKO_GITHUB_CODELOAD", base)
        .output()
        .unwrap();
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned(), o.status.success())
}

#[test]
fn install_and_info_from_a_github_spec() {
    let base = server();
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");

    let (_, err, ok) = meetko(&data, &base, &["plugin", "info", "github:meetko/r@v1"]);
    assert!(ok, "{err}");
    assert!(err.contains("meetko/action-items-py") && err.contains(SHA) && err.contains("declared network: false"), "{err}");
    let (out, _, _) = meetko(&data, &base, &["plugin", "list"]);
    assert!(out.trim().is_empty(), "info installs nothing: {out}");

    // consent is required
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "github:meetko/r@v1"]);
    assert!(!ok && err.contains("--yes"), "{err}");

    let (out, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r@v1"]);
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "meetko/action-items-py");
    let (out, _, _) = meetko(&data, &base, &["plugin", "list"]);
    assert!(out.contains(&format!("from github:meetko/r@{} (v1)", &SHA[..12])), "{out}");

    // another owner cannot ship meetko/...
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:other/r@v1"]);
    assert!(!ok && err.contains("publisher"), "{err}");

    // a local folder with the same id is a different origin: needs --replace
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/plugins/action-items-py");
    let local = local.to_str().unwrap();
    let (_, err, ok) = meetko(&data, &base, &["plugin", "add", "--yes", local]);
    assert!(!ok && err.contains("--replace"), "{err}");
    let (_, err, ok) = meetko(&data, &base, &["plugin", "add", "--yes", "--replace", local]);
    assert!(ok, "{err}");
    let (out, _, _) = meetko(&data, &base, &["plugin", "list"]);
    assert!(!out.contains("from github:"), "the stale source is cleared: {out}");

    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r@nope"]);
    assert!(!ok && err.contains("404"), "{err}");
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r"]);
    assert!(!ok && err.contains("expected github:OWNER/REPO@REF"), "{err}");
}

#[test]
fn install_and_info_from_a_subfolder_spec() {
    let base = server_with(catalog_tarball());
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let spec = "github:meetko/r/plugins/action-items-py@v1";
    let (_, err, ok) = meetko(&data, &base, &["plugin", "info", spec]);
    assert!(ok && err.contains("meetko/action-items-py") && err.contains("github:meetko/r/plugins/action-items-py"), "{err}");
    let (out, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", spec]);
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "meetko/action-items-py");
    let (out, _, _) = meetko(&data, &base, &["plugin", "list"]);
    assert!(out.contains(&format!("from github:meetko/r/plugins/action-items-py@{} (v1)", &SHA[..12])), "{out}");
    // only the folder was installed
    let installed = std::fs::read_dir(data.join("plugins")).unwrap().map(|e| e.unwrap().path()).find(|p| p.join("meetko.toml").is_file()).unwrap();
    assert!(installed.join("plugin.py").is_file() && !installed.join("README.md").exists() && !installed.join("plugins").exists());
    // the same commit again is the same origin (an update), no --replace needed
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", spec]);
    assert!(ok, "{err}");
    // a folder without a manifest; a folder that is not there; a path that tries to leave
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r/plugins@v1"]);
    assert!(!ok && err.contains("meetko.toml"), "a folder without a manifest: {err}");
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r/plugins/nope@v1"]);
    assert!(!ok && err.contains("not in the downloaded source"), "{err}");
    let (_, err, ok) = meetko(&data, &base, &["plugin", "install", "--yes", "github:meetko/r/../x@v1"]);
    assert!(!ok && err.contains("folder path"), "{err}");
}

#[test]
fn error_messages_never_carry_raw_escape_or_bell_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = tmp.path().join("p");
    std::fs::create_dir(&dir).unwrap();
    // the toml parse error echoes the offending source line, escape sequence included
    std::fs::write(dir.join("meetko.toml"), "[package]\nid = \"a/b\" \u{1b}[31m\u{7}\u{202e} not toml\n").unwrap();
    let (out, err, ok) = meetko(&data, "http://127.0.0.1:1", &["plugin", "add", "--yes", dir.to_str().unwrap()]);
    assert!(!ok);
    for (name, text) in [("stdout", &out), ("stderr", &err)] {
        assert!(!text.bytes().any(|b| b == 0x1b || b == 0x07), "raw control byte on {name}");
        assert!(!text.contains('\u{202e}'), "raw bidi override on {name}");
    }
    assert!(err.contains("\\u{1b}"), "the line is shown escaped");
}

/// `plugin secret`: names are checked against the installed manifest before a value is read, and
/// the value is never an argument. (Saving touches the OS keychain, so it is not run here.)
#[test]
fn secret_commands_refuse_undeclared_names_and_never_take_a_value_argument() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/plugins/secrets-py");
    let (_, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "add", plugin.to_str().unwrap(), "--yes"]);
    assert!(ok, "{err}");
    assert!(err.contains("API keys:") && err.contains("ANTHROPIC_API_KEY") && !err.contains("stay (same origin)"), "{err}");
    // adding it again from a local folder: the saved values stay, and the command says so
    let (_, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "add", plugin.to_str().unwrap(), "--yes"]);
    assert!(ok && err.contains("stay (same origin)"), "{err}");
    let (_, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "secret", "set", "meetko/secrets-fixture", "PATH"]);
    assert!(!ok && err.contains("does not declare"), "{err}");
    let (_, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "secret", "set", "meetko/nope", "ANTHROPIC_API_KEY"]);
    assert!(!ok && err.contains("not found"), "{err}");
    // there is no argument for the value: an extra one is a usage error
    let (_, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "secret", "set", "meetko/secrets-fixture", "ANTHROPIC_API_KEY", "sk-test-not-real"]);
    assert!(!ok && !err.contains("saved"), "{err}");
    let local = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/plugins/action-items-py");
    meetko(&data, "http://127.0.0.1:9", &["plugin", "add", local.to_str().unwrap(), "--yes"]);
    let (out, err, ok) = meetko(&data, "http://127.0.0.1:9", &["plugin", "secret", "status", "meetko/action-items-py"]);
    assert!(ok && out.is_empty() && err.contains("declares no API keys"), "{out} {err}");
}
