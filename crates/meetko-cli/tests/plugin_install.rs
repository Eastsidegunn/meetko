//! `meetko plugin install github:...` and `plugin info` against a local HTTP server.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn tarball() -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
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
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
    let data = tarball();
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
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/action-items-py");
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
