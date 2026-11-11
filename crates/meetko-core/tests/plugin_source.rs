//! `github:OWNER/REPO@REF` install against an in-process HTTP server (no real network).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use meetko_core::plugin_source::{extract_tar_gz, fetch, Endpoints, Limits, SourceSpec};
use meetko_core::store::Store;
use tar::{Builder, EntryType, Header};

const MANIFEST: &str = r#"[package]
id = "someone/hello"
version = "0.2.0"
kind = "plugin"
contract = "0.1"
license = "MIT"
description = "hello"
homepage = "https://example.com/hello"

[run]
command = ["python3", "-I", "-S", "plugin.py"]
protocol = "stdio-jsonrpc"

[permissions]
network = true
"#;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn raw(b: &mut Builder<Vec<u8>>, name: &str, ty: EntryType, data: &[u8], link: Option<&str>, mode: u32) {
    let mut h = Header::new_gnu();
    h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
    if let Some(l) = link {
        h.as_old_mut().linkname[..l.len()].copy_from_slice(l.as_bytes());
    }
    h.set_entry_type(ty);
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_cksum();
    b.append(&h, data).unwrap();
}

fn gz(b: Builder<Vec<u8>>) -> Vec<u8> {
    let tar = b.into_inner().unwrap();
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&tar).unwrap();
    e.finish().unwrap()
}

fn good_archive() -> Vec<u8> {
    let mut b = Builder::new(Vec::new());
    let top = format!("someone-hello-{SHA}");
    raw(&mut b, &format!("{top}/"), EntryType::Directory, b"", None, 0o755);
    raw(&mut b, &format!("{top}/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/plugin.py"), EntryType::Regular, b"print('hi')\n", None, 0o755);
    raw(&mut b, &format!("{top}/sub/data.txt"), EntryType::Regular, b"x", None, 0o4755);
    gz(b)
}

struct Server {
    base: String,
    api_hits: Arc<AtomicUsize>,
    compare_hits: Arc<AtomicUsize>,
    accept_seen: Arc<std::sync::Mutex<Vec<String>>>,
    /// what `compare/main...<sha>` answers (`behind`, `identical`, `diverged`, ...)
    compare: Arc<std::sync::Mutex<String>>,
    /// bodies of `git/matching-refs/tags`, one per page
    tags: Arc<std::sync::Mutex<Vec<String>>>,
    /// annotated tag objects by sha, for `git/tags/<sha>`
    tag_objects: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

/// Any repo `o/r`: `commits/v0.2.0` and `commits/deadbee` -> SHA, `repos/o/r` -> default branch
/// main, `compare/main...SHA` -> `compare`, `matching-refs/tags` -> `tags`, `/o/r/tar.gz/SHA` -> tarball.
fn serve(tarball: Vec<u8>) -> Server {
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
    let api_hits = Arc::new(AtomicUsize::new(0));
    let compare_hits = Arc::new(AtomicUsize::new(0));
    let accept_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let compare = Arc::new(std::sync::Mutex::new("behind".to_string()));
    let tags = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tag_objects = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<String, String>::new()));
    let (hits, chits, seen, cmp, tg, tobj) = (api_hits.clone(), compare_hits.clone(), accept_seen.clone(), compare.clone(), tags.clone(), tag_objects.clone());
    std::thread::spawn(move || {
        for req in srv.incoming_requests() {
            let full = req.url().to_string();
            let (url, query) = full.split_once('?').unwrap_or((full.as_str(), ""));
            let page: usize = query.split('&').find_map(|kv| kv.strip_prefix("page=")).and_then(|v| v.parse().ok()).unwrap_or(1);
            let parts: Vec<&str> = url.trim_start_matches('/').split('/').collect();
            let json = |b: String| tiny_http::Response::from_string(b).with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
            let resp = match parts.as_slice() {
                ["repos", _, _, "commits", r @ ("v0.2.0" | "deadbee")] => {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = r;
                    if let Some(h) = req.headers().iter().find(|h| h.field.equiv("Accept")) {
                        seen.lock().unwrap().push(h.value.to_string());
                    }
                    tiny_http::Response::from_string(format!("{SHA}\n"))
                }
                ["repos", _, _] => json(r#"{"default_branch":"main"}"#.into()),
                ["repos", _, _, "compare", range] if range.starts_with("main...") => {
                    chits.fetch_add(1, Ordering::SeqCst);
                    json(format!(r#"{{"status":"{}"}}"#, cmp.lock().unwrap()))
                }
                ["repos", _, _, "git", "matching-refs", "tags"] => json(tg.lock().unwrap().get(page - 1).cloned().unwrap_or_else(|| "[]".into())),
                ["repos", _, _, "git", "tags", t] => match tobj.lock().unwrap().get(*t) {
                    Some(body) => json(body.clone()),
                    None => tiny_http::Response::from_string("nope").with_status_code(404),
                },
                [_, _, "tar.gz", sha] if *sha == SHA || *sha == OTHER_SHA => tiny_http::Response::from_data(tarball.clone()),
                _ => tiny_http::Response::from_string("nope").with_status_code(404),
            };
            let _ = req.respond(resp);
        }
    });
    Server { base, api_hits, compare_hits, accept_seen, compare, tags, tag_objects }
}

fn endpoints(s: &Server) -> Endpoints {
    Endpoints { api: s.base.clone(), codeload: s.base.clone() }
}

fn spec(r: &str) -> SourceSpec {
    SourceSpec::parse(&format!("github:someone/hello@{r}")).unwrap().unwrap()
}

#[test]
fn parse_specs() {
    assert!(SourceSpec::parse("./local/dir").unwrap().is_none());
    assert!(SourceSpec::parse("/abs/path").unwrap().is_none());
    let s = spec("v1.0.0");
    assert_eq!((s.owner.as_str(), s.repo.as_str(), s.reference.as_str()), ("someone", "hello", "v1.0.0"));
    assert_eq!(s.repo_id(), "github:someone/hello");
    assert_eq!(spec("release/1.2+rc").reference, "release/1.2+rc");
    for bad in [
        "github:someone/hello",            // no ref
        "github:someone/hello@",           // empty ref
        "github:someone@v1",               // no repo
        "github:../hello@v1",              // bad owner
        "github:someone/hello@../x",       // dotdot ref
        "github:someone/hello@-rf",        // option-like
        "github:someone/hello@v1?x=1",     // query chars
        "github:some one/hello@v1",
    ] {
        assert!(SourceSpec::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn installs_from_a_tag_and_records_the_source() {
    let srv = serve(good_archive());
    let fetched = fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap();
    assert_eq!(fetched.source.sha, SHA);
    assert_eq!(fetched.source.repo, "github:someone/hello");
    assert_eq!(fetched.source.reference, "v0.2.0");
    assert_eq!(srv.accept_seen.lock().unwrap().as_slice(), ["application/vnd.github.sha"]);
    assert!(fetched.dir.join("meetko.toml").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| std::fs::metadata(fetched.dir.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("plugin.py"), 0o755);
        assert_eq!(mode("sub/data.txt"), 0o755, "setuid is dropped, exec kept");
        assert_eq!(mode("meetko.toml"), 0o644);
    }

    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    // the hash shown to the user must match, same as the local path
    let (m, _) = meetko_core::manifest::Manifest::load(&fetched.dir).unwrap();
    assert_eq!(m.package.homepage, "https://example.com/hello");
    let code = meetko_core::manifest::plugin_code_sha256(&m, &fetched.dir).unwrap();
    assert!(store.install_fetched(&fetched, Some("0".repeat(64).as_str()), None).is_err(), "wrong hash is refused");
    assert!(store.plugin_source("someone/hello").unwrap().is_none(), "a refused install records nothing");
    let row = store.install_fetched(&fetched, Some(&code), None).unwrap();
    assert_eq!(row.id, "someone/hello");
    assert!(row.network);
    assert!(row.dir.starts_with(dunce::canonicalize(data.path()).unwrap()), "runs from the data dir copy");
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap(), fetched.source);
    drop(fetched);
    drop(store);
    // reopening keeps the row (migration is idempotent)
    let store = Store::open(data.path()).unwrap();
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().sha, SHA);
}

#[test]
fn a_full_sha_is_verified_against_the_repo_not_just_looked_up() {
    let srv = serve(good_archive());
    // an ancestor of the default branch: accepted, and no name-resolution request is made
    let f = fetch(&endpoints(&srv), &spec(SHA)).unwrap();
    assert_eq!(f.source.sha, SHA);
    assert_eq!(srv.api_hits.load(Ordering::SeqCst), 0);
    assert_eq!(srv.compare_hits.load(Ordering::SeqCst), 1);
    *srv.compare.lock().unwrap() = "identical".into();
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_ok());

    // a commit that only exists in a fork of the network: compare says diverged/ahead, no tag has it
    for status in ["diverged", "ahead"] {
        *srv.compare.lock().unwrap() = status.into();
        let e = fetch(&endpoints(&srv), &spec(SHA)).unwrap_err().to_string();
        assert!(e.contains("cannot verify") && e.contains("fork"), "{status}: {e}");
    }
    // ... unless a tag of the repo points at it (a release branch)
    *srv.tags.lock().unwrap() = vec![format!(r#"[{{"ref":"refs/tags/v1","object":{{"sha":"{SHA}","type":"commit"}}}}]"#)];
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_ok());
    // a tag for some other commit does not vouch for it
    *srv.tags.lock().unwrap() = vec![format!(r#"[{{"ref":"refs/tags/v1","object":{{"sha":"{OTHER_SHA}","type":"commit"}}}}]"#)];
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_err());
}

#[test]
fn hex_looking_tags_resolve_as_refs_and_are_verified() {
    let srv = serve(good_archive());
    // "deadbee" is 7 hex characters: a tag name here, not an abbreviated sha to refuse
    *srv.compare.lock().unwrap() = "diverged".into();
    *srv.tags.lock().unwrap() = vec![format!(r#"[{{"ref":"refs/tags/deadbee","object":{{"sha":"{SHA}","type":"commit"}}}}]"#)];
    let f = fetch(&endpoints(&srv), &spec("deadbee")).unwrap();
    assert_eq!((f.source.sha.as_str(), f.source.reference.as_str()), (SHA, "deadbee"));
    assert_eq!(srv.api_hits.load(Ordering::SeqCst), 1);
    // the same name resolving to a commit the repo does not own is refused
    *srv.tags.lock().unwrap() = vec![];
    assert!(fetch(&endpoints(&srv), &spec("deadbee")).unwrap_err().to_string().contains("cannot verify"));
}

#[test]
fn missing_things_are_refused() {
    let srv = serve(good_archive());
    let e = fetch(&endpoints(&srv), &spec("v9")).unwrap_err().to_string();
    assert!(e.contains("404"), "{e}");
    let other = SourceSpec::parse("github:someone/other@v9").unwrap().unwrap();
    assert!(fetch(&endpoints(&srv), &other).is_err());
}

#[test]
fn endpoints_must_be_https_unless_loopback() {
    use meetko_core::plugin_source::check_url;
    assert!(check_url("x", "https://api.github.com").is_ok());
    assert!(check_url("x", "http://127.0.0.1:8080").is_ok());
    assert!(check_url("x", "http://localhost:9").is_ok());
    assert!(check_url("x", "http://[::1]:9/index.json").is_ok());
    assert!(check_url("x", "ftp://example.com").is_err());
    assert!(check_url("x", "file:///etc/passwd").is_err());
    assert!(check_url("x", "https://user:pw@example.com").is_err(), "credentials in the URL");
    assert!(check_url("x", "http://127.0.0.1@evil.example/").is_err(), "loopback-looking userinfo");
    // plain http to a non-loopback host is accepted only by debug builds
    assert_eq!(check_url("x", "http://example.com").is_ok(), cfg!(debug_assertions));
}

#[test]
fn a_non_plugin_tree_is_stopped_by_the_normal_install() {
    let mut b = Builder::new(Vec::new());
    raw(&mut b, "t/readme.md", EntryType::Regular, b"hi", None, 0o644);
    let srv = serve(gz(b));
    let f = fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    assert!(store.install_fetched(&f, None, None).is_err(), "no meetko.toml");
}

/// Extract into `<root>/box/out` and report what exists next to it: nothing may appear outside `out`.
fn extract_in_box(data: &[u8], limits: &Limits) -> Result<(), String> {
    let root = tempfile::tempdir().unwrap();
    let boxed = root.path().join("box");
    std::fs::create_dir(&boxed).unwrap();
    let r = extract_tar_gz(data, &boxed.join("out"), limits).map_err(|e| e.to_string());
    let names = |p: &std::path::Path| std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(names(root.path()), ["box"], "nothing outside the box");
    assert!(names(&boxed).iter().all(|n| n == "out"), "nothing beside the target: {:?}", names(&boxed));
    r
}

fn extract(b: Builder<Vec<u8>>, limits: &Limits) -> Result<(), String> {
    extract_in_box(&gz(b), limits)
}

fn unsafe_ok(b: Builder<Vec<u8>>) -> String {
    extract(b, &Limits::default()).unwrap_err()
}

#[test]
fn hostile_archives_are_refused() {
    let one = |name: &str, ty: EntryType, link: Option<&str>| {
        let mut b = Builder::new(Vec::new());
        raw(&mut b, name, ty, b"", link, 0o644);
        b
    };
    // each runs through `extract_in_box`, which asserts nothing was written outside the target dir
    assert!(unsafe_ok(one("top/../../evil", EntryType::Regular, None)).contains("unsafe"));
    assert!(unsafe_ok(one("/etc/evil", EntryType::Regular, None)).contains("unsafe"));
    assert!(unsafe_ok(one("top/a\\b", EntryType::Regular, None)).contains("unsafe"));
    assert!(unsafe_ok(one("top/link", EntryType::Symlink, Some("/etc/passwd"))).contains("unsafe"));
    assert!(unsafe_ok(one("top/hard", EntryType::Link, Some("top/meetko.toml"))).contains("unsafe"));
    assert!(unsafe_ok(one("top/dev", EntryType::Char, None)).contains("unsafe"));
    assert!(unsafe_ok(one("top/dev", EntryType::Block, None)).contains("unsafe"));
    assert!(unsafe_ok(one("top/pipe", EntryType::Fifo, None)).contains("unsafe"));
    assert!(unsafe_ok(one("loosefile", EntryType::Regular, None)).contains("unsafe"));
    // the entry name is echoed, but never with raw control or bidi characters
    let m = unsafe_ok(one("top/../\u{1b}[31m\u{7}\u{202e}x", EntryType::Regular, None));
    assert!(m.contains("unsafe") && !m.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{m:?}");
}

#[test]
fn pax_and_gnu_long_names_cannot_traverse() {
    // PAX `path` override
    let mut b = Builder::new(Vec::new());
    let mut h = Header::new_ustar();
    h.set_path("top/ok").unwrap();
    h.set_size(1);
    h.set_entry_type(EntryType::Regular);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_pax_extensions([("path", &b"top/../../escaped"[..])]).unwrap();
    b.append(&h, &b"x"[..]).unwrap();
    assert!(unsafe_ok(b).contains("unsafe"));

    // GNU ././@LongLink entry naming a traversing path
    let long = format!("top/{}/../../../escaped", "a".repeat(120));
    let mut b = Builder::new(Vec::new());
    let mut lh = Header::new_gnu();
    lh.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"././@LongLink");
    lh.set_entry_type(EntryType::GNULongName);
    lh.set_size(long.len() as u64 + 1);
    lh.set_mode(0o644);
    lh.set_cksum();
    let mut name = long.into_bytes();
    name.push(0);
    b.append(&lh, name.as_slice()).unwrap();
    raw(&mut b, "top/ok", EntryType::Regular, b"x", None, 0o644);
    assert!(unsafe_ok(b).contains("unsafe"));

    // and an absolute PAX path
    let mut b = Builder::new(Vec::new());
    b.append_pax_extensions([("path", &b"/etc/escaped"[..])]).unwrap();
    raw(&mut b, "top/ok", EntryType::Regular, b"x", None, 0o644);
    assert!(unsafe_ok(b).contains("unsafe"));
}

#[test]
fn names_windows_cannot_hold_are_refused() {
    for bad in ["top/CON", "top/con.txt", "top/Nul.tar.gz", "top/aux", "top/prn.py", "top/COM1", "top/lpt9.log", "top/COM1 .txt", "top/com0", "top/LPT0.txt", "top/evil.", "top/evil ", "top/x/...", "top/a:b", "top/a*b", "top/q?", "top/CONIN$"] {
        let mut b = Builder::new(Vec::new());
        raw(&mut b, bad, EntryType::Regular, b"z", None, 0o644);
        assert!(unsafe_ok(b).contains("unsafe"), "{bad} should be refused");
    }
    // near misses stay legal
    for ok in ["top/console.py", "top/com10", "top/auxiliary", "top/a.b.c", "top/.hidden", "top/lpt"] {
        let mut b = Builder::new(Vec::new());
        raw(&mut b, ok, EntryType::Regular, b"z", None, 0o644);
        extract(b, &Limits::default()).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn limits_and_duplicates() {
    let many = || {
        let mut b = Builder::new(Vec::new());
        for i in 0..5 {
            raw(&mut b, &format!("t/f{i}"), EntryType::Regular, b"abc", None, 0o644);
        }
        b
    };
    let e = extract(many(), &Limits { max_files: 4, ..Limits::default() }).unwrap_err();
    assert!(e.contains("more than 4 files"), "{e}");
    let e = extract(many(), &Limits { max_bytes: 10, ..Limits::default() }).unwrap_err();
    assert!(e.contains("larger"), "{e}");
    let e = extract(many(), &Limits { max_depth: 0, ..Limits::default() }).unwrap_err();
    assert!(e.contains("too deep"), "{e}");
    let mut dup = Builder::new(Vec::new());
    raw(&mut dup, "t/a", EntryType::Regular, b"1", None, 0o644);
    raw(&mut dup, "t/a", EntryType::Regular, b"2", None, 0o644);
    assert!(extract(dup, &Limits::default()).unwrap_err().contains("duplicate"));
    assert!(extract_tar_gz(&b"not gzip at all"[..], &tempfile::tempdir().unwrap().path().join("x"), &Limits::default()).is_err());
}

#[test]
fn directories_and_metadata_entries_count_toward_the_entry_cap() {
    let mut dirs = Builder::new(Vec::new());
    for i in 0..10 {
        raw(&mut dirs, &format!("t/d{i}/"), EntryType::Directory, b"", None, 0o755);
    }
    let e = extract(dirs, &Limits { max_files: 5, ..Limits::default() }).unwrap_err();
    assert!(e.contains("more than 5 files"), "{e}");
    // PAX global headers are skipped, not free
    let mut globals = Builder::new(Vec::new());
    for _ in 0..10 {
        let mut h = Header::new_ustar();
        h.set_path("pax_global_header").unwrap();
        h.set_entry_type(EntryType::XGlobalHeader);
        h.set_size(0);
        h.set_cksum();
        globals.append(&h, &[][..]).unwrap();
    }
    assert!(extract(globals, &Limits { max_files: 5, ..Limits::default() }).unwrap_err().contains("more than 5 files"));
}

/// gzip of: a tar header, then `zeros` bytes of zero. Compresses ~1000:1.
fn gz_header_then_zeros(h: &Header, zeros: u64) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(h.as_bytes()).unwrap();
    let chunk = vec![0u8; 1 << 20];
    let mut left = zeros;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        e.write_all(&chunk[..n]).unwrap();
        left -= n as u64;
    }
    e.finish().unwrap()
}

#[test]
fn a_pax_extension_bomb_is_refused_before_it_is_read() {
    // a PAX `x` entry claiming 2 GiB: the tar crate would read all of it into memory
    let mut h = Header::new_ustar();
    h.set_path("PaxHeaders/x").unwrap();
    h.set_entry_type(EntryType::XHeader);
    h.set_size(2 << 30);
    h.set_mode(0o644);
    h.set_cksum();
    // only 64 MiB of the claimed 2 GiB is present (the bomb need not be completed to hurt)
    let bomb = gz_header_then_zeros(&h, 64 << 20);
    assert!(bomb.len() < 1 << 20, "{} bytes", bomb.len());
    let t = std::time::Instant::now();
    let e = extract_in_box(&bomb, &Limits::default()).unwrap_err();
    assert!(e.contains("extension entry claims"), "{e}");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());

    // the same for GNU long name / long link entries
    for ty in [EntryType::GNULongName, EntryType::GNULongLink] {
        let mut h = Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"././@LongLink");
        h.set_entry_type(ty);
        h.set_size(2 << 30);
        h.set_cksum();
        assert!(extract_in_box(&gz_header_then_zeros(&h, 8 << 20), &Limits::default()).unwrap_err().contains("extension entry claims"));
    }
}

#[test]
fn a_stream_that_inflates_past_the_cap_is_refused_quickly() {
    // legitimate-looking entries (a skipped PAX global header) whose data is far more than the cap allows
    let mut h = Header::new_ustar();
    h.set_path("top/dir/").unwrap();
    h.set_entry_type(EntryType::Directory);
    h.set_size(900 << 20);
    h.set_mode(0o644);
    h.set_cksum();
    // 1 MiB cap + 16 MiB slack; 40 MiB of zeros is well over it, and compresses to ~40 KB
    let bomb = gz_header_then_zeros(&h, 40 << 20);
    let t = std::time::Instant::now();
    let e = extract_in_box(&bomb, &Limits { max_bytes: 1 << 20, ..Limits::default() }).unwrap_err();
    assert!(e.contains("unpacks to more than"), "{e}");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn index_parsing_drops_bad_entries() {
    use meetko_core::plugin_source::{fetch_index, parse_index};
    let good = |repo: &str, sha: &str| {
        format!(
            r#"{{"id":"a/b","name":"b","version":"1.0.0","description":"d","repo":"{repo}","ref":"v1","sha":"{sha}",
            "permissions":{{"network":true,"audio":false}},"runtime":"python","license":"MIT","stars":2,"updated":"x"}}"#
        )
    };
    let text = format!(
        r#"{{"generated":"x","plugins":[{}, {}, {}, {{"id":"only-an-id"}}]}}"#,
        good("acme/x", SHA),
        good("acme/x", "short"),
        good("../x", SHA)
    );
    let v = parse_index(&text).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].reference, "v1");
    assert!(v[0].permissions.network && !v[0].permissions.audio);
    assert_eq!(v[0].spec().unwrap().reference, SHA, "installs pin the sha, not the tag");
    assert!(parse_index("not json").is_err());

    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/index.json", srv.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        if let Some(req) = srv.incoming_requests().next() {
            let _ = req.respond(tiny_http::Response::from_string(text));
        }
    });
    assert_eq!(fetch_index(&url).unwrap().len(), 1);
    assert!(fetch_index("file:///etc/passwd").is_err());
}

#[test]
fn extraction_target_must_be_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(extract_tar_gz(gz(Builder::new(Vec::new())).as_slice(), tmp.path(), &Limits::default()).is_err());
}

fn archive_with_id(id: &str) -> Vec<u8> {
    let mut b = Builder::new(Vec::new());
    let manifest = MANIFEST.replace("someone/hello", id);
    raw(&mut b, "top/meetko.toml", EntryType::Regular, manifest.as_bytes(), None, 0o644);
    raw(&mut b, "top/plugin.py", EntryType::Regular, b"print('hi')\n", None, 0o755);
    gz(b)
}

#[test]
fn a_github_plugin_may_only_use_its_owner_as_publisher() {
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    // someone/hello shipping meetko/summary: refused, nothing registered
    let srv = serve(archive_with_id("meetko/summary"));
    let f = fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap();
    let e = store.install_fetched(&f, None, None).unwrap_err().to_string();
    assert!(e.contains("publisher") && e.contains("meetko"), "{e}");
    assert!(store.list_plugins().unwrap().is_empty());
    // an id with no publisher part cannot be tied to an owner
    let srv = serve(archive_with_id("plainname"));
    let f = fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap();
    assert!(store.install_fetched(&f, None, None).unwrap_err().to_string().contains("no publisher"));
    // the owner compare is case-insensitive (GitHub names are, ids are lowercase)
    let srv = serve(archive_with_id("someone/hello"));
    let upper = SourceSpec::parse("github:SomeOne/hello@v0.2.0").unwrap().unwrap();
    let f = fetch(&endpoints(&srv), &upper).unwrap();
    assert_eq!(store.install_fetched(&f, None, None).unwrap().id, "someone/hello");
    // a local folder has no owner: the rule applies to GitHub installs only
    assert!(store.install_plugin(&f.dir, None).is_err(), "(different origin, see the next test)");
}

#[test]
fn replacing_a_plugin_from_another_source_needs_an_explicit_replace() {
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let srv = serve(good_archive());
    let ep = endpoints(&srv);
    let other = |r: &str| SourceSpec::parse(&format!("github:someone/other@{r}")).unwrap().unwrap();

    let f1 = fetch(&ep, &spec("v0.2.0")).unwrap();
    store.install_fetched(&f1, None, None).unwrap();
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/hello");

    // the same id from another repo of the same owner: refused, the old source stays
    let f2 = fetch(&ep, &other("v0.2.0")).unwrap();
    let e = store.install_fetched(&f2, None, None).unwrap_err();
    assert_eq!(e.code(), "plugin_source_changed");
    assert!(e.to_string().contains("github:someone/hello") && e.to_string().contains("github:someone/other"), "{e}");
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/hello");
    // a local folder over a GitHub plugin is also a different origin
    assert_eq!(store.install_plugin(&f1.dir, None).unwrap_err().code(), "plugin_source_changed");
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/hello");

    // a new commit of the SAME repo is an update, no flag needed, and the source row follows it
    let newer = SourceSpec::parse(&format!("github:someone/hello@{OTHER_SHA}")).unwrap().unwrap();
    let f3 = fetch(&ep, &newer).unwrap();
    store.install_fetched(&f3, None, None).unwrap();
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().sha, OTHER_SHA);

    // with the flag, the other repo replaces it, in one transaction with the source row
    // the confirmation is bound to the installed state that was shown: a stale key is refused
    assert_eq!(store.install_fetched(&f2, None, Some("0.1.0|local")).unwrap_err().code(), "plugin_source_changed");
    let key = store.replace_key_for("someone/hello", Some(&f2.source)).unwrap().unwrap();
    store.install_fetched(&f2, None, Some(&key)).unwrap();
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/other");

    // a local install over it, with the flag, clears the source row instead of leaving a stale one
    let key = store.replace_key_for("someone/hello", None).unwrap().unwrap();
    store.install_plugin_from(&f1.dir, None, None, Some(&key)).unwrap();
    assert!(store.plugin_source("someone/hello").unwrap().is_none(), "stale source cleared");
    // and GitHub over local is refused again without the flag
    assert_eq!(store.install_fetched(&f1, None, None).unwrap_err().code(), "plugin_source_changed");
    assert!(store.plugin_source("someone/hello").unwrap().is_none());
}

#[test]
fn index_rows_with_odd_ids_or_hidden_characters_are_dropped() {
    use meetko_core::plugin_source::parse_index;
    let row = |id: &str, extra: &str| {
        format!(
            r#"{{"id":"{id}","name":"n","version":"1.0.0","description":"d{extra}","repo":"acme/x","ref":"v1","sha":"{SHA}",
            "permissions":{{"network":false,"audio":false}}}}"#
        )
    };
    let list = |rows: Vec<String>| parse_index(&format!(r#"{{"generated":"x","plugins":[{}]}}"#, rows.join(","))).unwrap();
    assert_eq!(list(vec![row("acme/ok", "")]).len(), 1);
    for id in ["Acme/ok", "acme", "acme/", "/ok", "acme/ok/extra", "acme/o k", "-acme/ok", &format!("acme/{}", "a".repeat(65))] {
        assert!(list(vec![row(id, "")]).is_empty(), "{id}");
    }
    assert!(list(vec![row("acme/ok", "\u{202e} evil")]).is_empty(), "bidi override");
    assert!(list(vec![row("acme/ok", "\\nsecond line")]).is_empty(), "control character");
    assert!(list(vec![row("acme/ok", "\u{200b}")]).is_empty(), "zero width");
    assert!(list(vec![row("acme/ok", &"x".repeat(300))]).is_empty(), "over-long description");
    // line/paragraph separators, fillers, selectors, tags, more Cf characters, long mark runs
    for c in ["2028", "2029", "115f", "1160", "3164", "ffa0", "034f", "fe0f", "0600", "06dd", "070f", "08e2", "db40\\udd00", "db40\\udc01"] {
        assert!(list(vec![row("acme/ok", &format!("\\u{c}"))]).is_empty(), "{c}");
    }
    assert!(list(vec![row("acme/ok", &"\\u0301".repeat(9))]).is_empty(), "mark run");
    assert_eq!(list(vec![row("acme/ok", " e\\u0301")]).len(), 1);
    assert_eq!(list(vec![row("acme/ok", " 한국어")]).len(), 1);
    // one row per id: the first wins
    assert_eq!(list(vec![row("acme/ok", ""), row("acme/ok", " second")]).len(), 1);
}

const TAG_OBJ: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAG_OBJ2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn tag_ref(kind: &str, sha: &str) -> String {
    format!(r#"[{{"ref":"refs/tags/v1","object":{{"sha":"{sha}","type":"{kind}"}}}}]"#)
}

#[test]
fn tag_verification_pages_peels_and_reports_what_it_could_not_read() {
    let srv = serve(good_archive());
    *srv.compare.lock().unwrap() = "diverged".into();
    let obj = |kind: &str, sha: &str| format!(r#"{{"object":{{"sha":"{sha}","type":"{kind}"}}}}"#);
    // an annotated tag
    *srv.tags.lock().unwrap() = vec![tag_ref("tag", TAG_OBJ)];
    srv.tag_objects.lock().unwrap().insert(TAG_OBJ.into(), obj("commit", SHA));
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_ok());
    // a tag of a tag
    *srv.tags.lock().unwrap() = vec![tag_ref("tag", TAG_OBJ2)];
    srv.tag_objects.lock().unwrap().insert(TAG_OBJ2.into(), obj("tag", TAG_OBJ));
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_ok());
    // the tag is on the second page of 100
    let filler: Vec<String> = (0..100).map(|i| format!(r#"{{"ref":"refs/tags/t{i}","object":{{"sha":"{OTHER_SHA}","type":"commit"}}}}"#)).collect();
    *srv.tags.lock().unwrap() = vec![format!("[{}]", filler.join(",")), tag_ref("commit", SHA)];
    assert!(fetch(&endpoints(&srv), &spec(SHA)).is_ok());
    // a peel that fails is not silently ignored: the refusal says the tags could not be read completely
    *srv.tags.lock().unwrap() = vec![tag_ref("tag", "cccccccccccccccccccccccccccccccccccccccc")];
    let e = fetch(&endpoints(&srv), &spec(SHA)).unwrap_err().to_string();
    assert!(e.contains("cannot verify") && e.contains("completely"), "{e}");
}

#[test]
fn every_ref_is_verified_and_pull_refs_are_refused() {
    let srv = serve(good_archive());
    for r in ["pull/123/head", "refs/pull/1/head", "refs/heads/main"] {
        let e = fetch(&endpoints(&srv), &spec(r)).unwrap_err().to_string();
        assert!(e.contains("not installable"), "{r}: {e}");
    }
    // a plain tag name that resolves to a commit outside the repo's history is refused too
    *srv.compare.lock().unwrap() = "diverged".into();
    assert!(fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap_err().to_string().contains("cannot verify"));
    assert_eq!(srv.compare_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_ids_and_stale_confirmations_leave_nothing_behind() {
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let plugin_dirs = || std::fs::read_dir(data.path().join("plugins")).map(|d| d.count()).unwrap_or(0);
    // an id that matches the owner only by case-folding tricks is not a valid id at all
    let srv = serve(archive_with_id("SomeOne/hello"));
    let f = fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap();
    assert!(store.install_fetched(&f, None, None).unwrap_err().to_string().contains("not a valid id"));
    // a source conflict is refused BEFORE a copy is moved into plugins/
    let srv = serve(good_archive());
    store.install_fetched(&fetch(&endpoints(&srv), &spec("v0.2.0")).unwrap(), None, None).unwrap();
    let before = plugin_dirs();
    let mut other = good_archive_with_extra();
    let srv2 = serve(std::mem::take(&mut other));
    let f2 = fetch(&endpoints(&srv2), &SourceSpec::parse("github:someone/other@v0.2.0").unwrap().unwrap()).unwrap();
    assert_eq!(store.install_fetched(&f2, None, None).unwrap_err().code(), "plugin_source_changed");
    assert_eq!(plugin_dirs(), before, "no orphan copy");
}

/// the good plugin with one more file, so its code hash (and plugins/ directory name) differs
fn good_archive_with_extra() -> Vec<u8> {
    let mut b = Builder::new(Vec::new());
    raw(&mut b, "top/meetko.toml", EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, "top/plugin.py", EntryType::Regular, b"print('other')\n", None, 0o755);
    gz(b)
}

#[test]
fn the_tar_guard_cannot_be_desynchronised() {
    // a base-256 size field: the tar crate and a naive guard would disagree about it
    let mut h = Header::new_gnu();
    h.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"././@LongLink");
    h.set_entry_type(EntryType::GNULongName);
    h.as_old_mut().size = [0x80, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x00, 0x00, 0x00];
    h.set_cksum();
    let e = extract_in_box(&gz_header_then_zeros(&h, 8 << 20), &Limits::default()).unwrap_err();
    assert!(e.contains("base-256"), "{e}");

    // a PAX `size=` record changes how tar frames the next entry
    let mut b = Builder::new(Vec::new());
    b.append_pax_extensions([("size", &b"0"[..])]).unwrap();
    raw(&mut b, "top/f", EntryType::Regular, &[b'A'; 512], None, 0o644);
    assert!(unsafe_ok(b).contains("sets the entry size"));
    // other PAX records are fine
    let mut b = Builder::new(Vec::new());
    b.append_pax_extensions([("comment", &b"hi"[..]), ("path", &b"top/ok.txt"[..])]).unwrap();
    raw(&mut b, "top/f", EntryType::Regular, b"x", None, 0o644);
    extract(b, &Limits::default()).unwrap();

    // a hostile path is not echoed back in full
    let long = format!("top/{}/../x", "a".repeat(5000));
    let mut b = Builder::new(Vec::new());
    b.append_pax_extensions([("path", long.as_bytes())]).unwrap();
    raw(&mut b, "top/f", EntryType::Regular, b"x", None, 0o644);
    let e = unsafe_ok(b);
    assert!(e.len() < 700, "{} chars", e.len());
}

#[test]
fn manifests_with_terminal_escapes_or_hidden_characters_are_refused() {
    use meetko_core::manifest::Manifest;
    let with = |field: &str, value: &str| MANIFEST.replace(field, value);
    assert!(Manifest::parse(MANIFEST).is_ok());
    for (field, value) in [
        ("hello\"", "hello\\u001b[2K\\u001b[1Adeclared network: false\""),
        ("version = \"0.2.0\"", "version = \"0.2.0\\u202e\""),
        ("someone/hello", "some\\u200bone/hello"),
        ("description = \"hello\"", "description = \"a\\nb\""),
    ] {
        let e = Manifest::parse(&with(field, value));
        assert!(e.is_err(), "{field} -> {value}");
    }
}

// ---- subfolders: github:OWNER/REPO/FOLDER@REF ----

#[test]
fn parse_subfolder_specs() {
    let s = SourceSpec::parse("github:someone/hello/plugins/hello-py@v1.0.0").unwrap().unwrap();
    assert_eq!((s.owner.as_str(), s.repo.as_str(), s.path.as_deref(), s.reference.as_str()), ("someone", "hello", Some("plugins/hello-py"), "v1.0.0"));
    assert_eq!(s.repo_id(), "github:someone/hello/plugins/hello-py");
    assert_eq!(spec("v1").path, None, "the root form is unchanged");
    assert_eq!(SourceSpec::parse("github:a/b/c_d.e-F@release/1.2").unwrap().unwrap().reference, "release/1.2");
    let deep = format!("github:a/b/{}@v1", ["x"; 8].join("/"));
    assert!(SourceSpec::parse(&deep).unwrap().is_some());
    let at_cap = format!("github:a/b/{}/{}@v1", "y".repeat(99), "y".repeat(100));
    assert!(SourceSpec::parse(&at_cap).unwrap().is_some(), "200 bytes");
    for bad in [
        "github:a/b/@v1".to_string(),                        // empty path
        "github:a/b//c@v1".into(),                           // empty segment
        "github:a/b/c/@v1".into(),                           // trailing slash
        "github:a/b/../c@v1".into(),                         // parent
        "github:a/b/c/..@v1".into(),
        "github:a/b/./c@v1".into(),                          // dot segment
        "github:a/b/.github@v1".into(),                      // hidden folder
        "github:a/b/c\\d@v1".into(),                         // backslash
        "github:a/b/c d@v1".into(),                          // space
        "github:a/b/c:d@v1".into(),                          // colon
        "github:a/b/c%2e%2e@v1".into(),                      // encoded
        "github:a/b/c./d@v1".into(),                         // trailing dot (Windows drops it)
        "github:a/b/con@v1".into(),                          // reserved Windows name
        "github:a/b/x/nul.txt@v1".into(),
        "github:a/b/é@v1".into(),                            // non-ASCII
        format!("github:a/b/{}@v1", ["x"; 9].join("/")),     // too deep
        format!("github:a/b/{}@v1", "z".repeat(101)),        // segment too long
        format!("github:a/b/{}/{}@v1", "y".repeat(100), "y".repeat(100)),   // 201 bytes
    ] {
        assert!(SourceSpec::parse(&bad).is_err(), "{bad}");
    }
}

/// a repo with two plugins in subfolders, a symlink and a stray file elsewhere
fn catalog_archive(sha: &str) -> Vec<u8> {
    let mut b = Builder::new(Vec::new());
    let top = format!("someone-hello-{sha}");
    raw(&mut b, &format!("{top}/"), EntryType::Directory, b"", None, 0o755);
    raw(&mut b, &format!("{top}/README.md"), EntryType::Regular, b"catalog", None, 0o644);
    raw(&mut b, &format!("{top}/link"), EntryType::Symlink, b"", Some("/etc/passwd"), 0o777);
    raw(&mut b, &format!("{top}/plugins/"), EntryType::Directory, b"", None, 0o755);
    raw(&mut b, &format!("{top}/plugins/hello/"), EntryType::Directory, b"", None, 0o755);
    raw(&mut b, &format!("{top}/plugins/hello/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/plugins/hello/plugin.py"), EntryType::Regular, b"print('hi')\n", None, 0o755);
    raw(&mut b, &format!("{top}/plugins/hello/lib/x.py"), EntryType::Regular, b"x = 1\n", None, 0o644);
    raw(&mut b, &format!("{top}/plugins/hello2/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/plugins/hello2/plugin.py"), EntryType::Regular, b"print('hi')\n", None, 0o755);
    raw(&mut b, &format!("{top}/plugins/hello2/lib/x.py"), EntryType::Regular, b"x = 1\n", None, 0o644);
    raw(&mut b, &format!("{top}/plugins/hellofile"), EntryType::Regular, b"not a folder", None, 0o644);
    // the same folder name with another case: a different folder in Git
    raw(&mut b, &format!("{top}/Plugins/hello/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/Plugins/hello/plugin.py"), EntryType::Regular, b"print('hi')\n", None, 0o755);
    raw(&mut b, &format!("{top}/Plugins/hello/lib/x.py"), EntryType::Regular, b"x = 1\n", None, 0o644);
    gz(b)
}

fn sub_spec(path: &str, r: &str) -> SourceSpec {
    SourceSpec::parse(&format!("github:someone/hello/{path}@{r}")).unwrap().unwrap()
}

#[test]
fn installs_only_the_subfolder_and_records_it_as_the_source() {
    let srv = serve(catalog_archive(SHA));
    let f = fetch(&endpoints(&srv), &sub_spec("plugins/hello", "v0.2.0")).unwrap();
    assert_eq!((f.source.repo.as_str(), f.source.sha.as_str()), ("github:someone/hello/plugins/hello", SHA));
    let mut got: Vec<String> = walk(&f.dir);
    got.sort();
    assert_eq!(got, ["lib", "lib/x.py", "meetko.toml", "plugin.py"], "only the folder, prefix removed; the symlink elsewhere was skipped");
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    assert_eq!(store.install_fetched(&f, None, None).unwrap().id, "someone/hello");
    let src = store.plugin_source("someone/hello").unwrap().unwrap();
    assert_eq!(src.repo, "github:someone/hello/plugins/hello");
    // the publisher rule still uses the repo owner
    let srv = serve(catalog_archive(SHA));
    let f = fetch(&endpoints(&srv), &SourceSpec::parse("github:other/hello/plugins/hello@v0.2.0").unwrap().unwrap()).unwrap();
    assert!(store.install_fetched(&f, None, None).unwrap_err().to_string().contains("publisher"));
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type().unwrap().is_dir() {
            out.extend(walk(&e.path()).into_iter().map(|c| format!("{name}/{c}")));
        }
        out.push(name);
    }
    out
}

#[test]
fn a_missing_or_unsafe_subfolder_is_refused() {
    let srv = serve(catalog_archive(SHA));
    let ep = endpoints(&srv);
    for (path, why) in [("plugins/nope", "not in the downloaded source"), ("plugins/hellofile", "not a directory"), ("PLUGINS/hello", "not in the downloaded source")] {
        let e = fetch(&ep, &sub_spec(path, "v0.2.0")).unwrap_err().to_string();
        assert!(e.contains(why), "{path}: {e}");
    }
    // a prefix of a folder name is not that folder
    assert!(fetch(&ep, &sub_spec("plugins/hell", "v0.2.0")).is_err());
    // inside the folder the usual rules apply: a link, a parent path anywhere, a file too deep
    let top = format!("someone-hello-{SHA}");
    let mut b = Builder::new(Vec::new());
    raw(&mut b, &format!("{top}/p/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/p/evil"), EntryType::Symlink, b"", Some("../../x"), 0o777);
    let srv = serve(gz(b));
    assert!(fetch(&endpoints(&srv), &sub_spec("p", "v0.2.0")).unwrap_err().to_string().contains("no links"));
    let mut b = Builder::new(Vec::new());
    raw(&mut b, &format!("{top}/p/meetko.toml"), EntryType::Regular, MANIFEST.as_bytes(), None, 0o644);
    raw(&mut b, &format!("{top}/q/../p/x"), EntryType::Regular, b"x", None, 0o644);
    let srv = serve(gz(b));
    assert!(fetch(&endpoints(&srv), &sub_spec("p", "v0.2.0")).unwrap_err().to_string().contains("parent"));
    let tmp = tempfile::tempdir().unwrap();
    let mut b = Builder::new(Vec::new());
    raw(&mut b, "top/p/a/b/c/x", EntryType::Regular, b"x", None, 0o644);
    let lim = Limits { max_depth: 3, ..Limits::default() };
    assert!(meetko_core::plugin_source::extract_tar_gz_sub(gz(b).as_slice(), &tmp.path().join("d"), &lim, Some("p")).is_err(), "depth counts inside the folder");
    let mut b = Builder::new(Vec::new());
    raw(&mut b, "top/p/a/b/x", EntryType::Regular, b"x", None, 0o644);
    meetko_core::plugin_source::extract_tar_gz_sub(gz(b).as_slice(), &tmp.path().join("e"), &lim, Some("p")).unwrap();
    // an invalid folder handed straight to the extractor is refused too
    assert!(meetko_core::plugin_source::extract_tar_gz_sub(gz(Builder::new(Vec::new())).as_slice(), &tmp.path().join("f"), &lim, Some("../p")).is_err());
}

#[test]
fn a_subfolder_updates_in_place_and_another_folder_is_another_origin() {
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let srv = serve(catalog_archive(SHA));
    let ep = endpoints(&srv);
    let f1 = fetch(&ep, &sub_spec("plugins/hello", "v0.2.0")).unwrap();
    store.install_fetched(&f1, None, None).unwrap();
    // a new commit of the same folder is an update: no flag, the source row follows it
    let f2 = fetch(&ep, &sub_spec("plugins/hello", OTHER_SHA)).unwrap();
    store.install_fetched(&f2, None, None).unwrap();
    let src = store.plugin_source("someone/hello").unwrap().unwrap();
    assert_eq!((src.repo.as_str(), src.sha.as_str()), ("github:someone/hello/plugins/hello", OTHER_SHA));
    // the same id from another folder of the repo, or from the repo root, is a different origin
    let f3 = fetch(&ep, &sub_spec("plugins/hello2", "v0.2.0")).unwrap();
    assert_eq!(store.install_fetched(&f3, None, None).unwrap_err().code(), "plugin_source_changed");
    let srv_root = serve(good_archive());
    let f4 = fetch(&endpoints(&srv_root), &spec("v0.2.0")).unwrap();
    assert_eq!(store.install_fetched(&f4, None, None).unwrap_err().code(), "plugin_source_changed");
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/hello/plugins/hello");
    // with the confirmation it moves to the other folder
    let key = store.replace_key_for("someone/hello", Some(&f3.source)).unwrap().unwrap();
    store.install_fetched(&f3, None, Some(&key)).unwrap();
    assert_eq!(store.plugin_source("someone/hello").unwrap().unwrap().repo, "github:someone/hello/plugins/hello2");
}

#[test]
fn index_rows_may_name_a_subfolder() {
    use meetko_core::plugin_source::parse_index;
    let row = |id: &str, repo: &str, path: Option<&str>| {
        let path = path.map(|p| format!(r#""path":"{p}","#)).unwrap_or_default();
        format!(
            r#"{{"id":"{id}","name":"n","version":"1.0.0","description":"d","repo":"{repo}",{path}"ref":"main","sha":"{SHA}",
            "permissions":{{"network":false,"audio":false}},"runtime":"python","license":"MIT","stars":0,"updated":""}}"#
        )
    };
    let text = format!(
        r#"{{"generated":"x","plugins":[{},{},{},{},{},{}]}}"#,
        row("acme/a", "acme/catalog", Some("a-py")),
        row("acme/b", "acme/b", None),
        row("acme/c", "acme/catalog", Some("../c")),
        row("acme/d", "acme/catalog", Some("x//d")),
        row("acme/e", "acme/catalog/e", None),
        row("acme/f", "acme/catalog", Some("/f"))
    );
    let v = parse_index(&text).unwrap();
    assert_eq!(v.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["acme/a", "acme/b"]);
    let s = v[0].spec().unwrap();
    assert_eq!((s.repo_id().as_str(), s.reference.as_str()), ("github:acme/catalog/a-py", SHA));
    assert_eq!(v[1].spec().unwrap().path, None);
}

#[test]
fn owner_and_repo_compare_case_insensitively_the_folder_exactly() {
    use meetko_core::plugin_source::origin_key;
    assert_eq!(origin_key("github:SomeOne/Hello"), "github:someone/hello", "root sources: as before");
    assert_eq!(origin_key("github:SomeOne/Hello/Plugins/X"), "github:someone/hello/Plugins/X");
    assert_ne!(origin_key("github:a/b/Plugin"), origin_key("github:a/b/plugin"));

    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let srv = serve(catalog_archive(SHA));
    let ep = endpoints(&srv);
    let f1 = fetch(&ep, &sub_spec("plugins/hello", "v0.2.0")).unwrap();
    store.install_fetched(&f1, None, None).unwrap();
    // another case of the owner or repo is the same origin: an update
    let same = fetch(&ep, &SourceSpec::parse("github:SomeOne/HELLO/plugins/hello@v0.2.0").unwrap().unwrap()).unwrap();
    assert!(store.replaced_origin("someone/hello", Some(&same.source)).unwrap().is_none());
    store.install_fetched(&same, None, None).unwrap();
    assert_eq!(meetko_core::secrets::origin_of(&store, "someone/hello").unwrap(), "github:someone/hello/plugins/hello");
    // another case of the folder is another folder: another origin, and keys do not follow
    let other = fetch(&ep, &sub_spec("Plugins/hello", "v0.2.0")).unwrap();
    assert_eq!(store.install_fetched(&other, None, None).unwrap_err().code(), "plugin_source_changed");
    let key = store.replace_key_for("someone/hello", Some(&other.source)).unwrap().unwrap();
    store.install_fetched(&other, None, Some(&key)).unwrap();
    assert_eq!(meetko_core::secrets::origin_of(&store, "someone/hello").unwrap(), "github:someone/hello/Plugins/hello");
}
