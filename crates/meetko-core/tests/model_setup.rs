//! Model download and import against a local server with small fixture files (never the real
//! model). The real offers are checked in the unit tests of `model_setup`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use meetko_core::model_setup::{download, import_file, Source, Want};
use meetko_core::util::sha256_bytes;
use meetko_core::Error;

fn body() -> Vec<u8> {
    (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
}

struct Server {
    base: String,
    /// the Range header of each request to /file
    ranges: Arc<Mutex<Vec<Option<String>>>>,
    hits: Arc<AtomicUsize>,
}

/// `/file` serves `data` (honouring `Range` unless `/plain`), `/plain` ignores ranges, `/big` sends
/// more than asked, `/chunkbig` the same without a length, `/short` stops early, `/to/<url>`
/// redirects there, `/loop` redirects to itself, `/badrange` resumes at the wrong place, `/r416`
/// refuses any range, `/missing` is a 404.
fn server(data: Vec<u8>) -> Server {
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let (rs, hs) = (ranges.clone(), hits.clone());
    std::thread::spawn(move || {
        for req in srv.incoming_requests() {
            hs.fetch_add(1, Ordering::SeqCst);
            let url = req.url().to_string();
            let range = req.headers().iter().find(|h| h.field.equiv("Range")).map(|h| h.value.to_string());
            let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k, v).unwrap();
            let resp = if url == "/loop" {
                tiny_http::Response::from_data(Vec::new()).with_status_code(302).with_header(header("Location", "/loop"))
            } else if url == "/badrange" && range.is_some() {
                tiny_http::Response::from_data(data[10..].to_vec()).with_status_code(206).with_header(header("Content-Range", &format!("bytes 10-{}/{}", data.len() - 1, data.len())))
            } else if url == "/r416" && range.is_some() {
                rs.lock().unwrap().push(range.clone());
                tiny_http::Response::from_data(Vec::new()).with_status_code(416)
            } else if url == "/r416" {
                rs.lock().unwrap().push(None);
                tiny_http::Response::from_data(data.clone())
            } else if url == "/chunkbig" {
                let mut more = data.clone();
                more.extend_from_slice(b"extra");
                // no length: tiny_http sends it chunked
                let r = tiny_http::Response::new(tiny_http::StatusCode(200), vec![], std::io::Cursor::new(more), None, None);
                let _ = req.respond(r);
                continue;
            } else if let Some(to) = url.strip_prefix("/to/") {
                tiny_http::Response::from_data(Vec::new()).with_status_code(302).with_header(header("Location", &to.replace("%3A", ":")))
            } else if url == "/file" || url == "/plain" {
                rs.lock().unwrap().push(range.clone());
                let from = range.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.strip_suffix('-')).and_then(|n| n.parse::<usize>().ok());
                match from {
                    Some(n) if url == "/file" && n < data.len() => tiny_http::Response::from_data(data[n..].to_vec())
                        .with_status_code(206)
                        .with_header(header("Content-Range", &format!("bytes {n}-{}/{}", data.len() - 1, data.len()))),
                    _ => tiny_http::Response::from_data(data.clone()),
                }
            } else if url == "/big" {
                let mut more = data.clone();
                more.extend_from_slice(b"extra");
                tiny_http::Response::from_data(more)
            } else if url == "/short" {
                tiny_http::Response::from_data(data[..data.len() / 2].to_vec())
            } else {
                tiny_http::Response::from_data(b"nope".to_vec()).with_status_code(404)
            };
            let _ = req.respond(resp);
        }
    });
    Server { base, ranges, hits }
}

const LOCAL: &[&str] = &["127.0.0.1"];

fn src(s: &Server, path: &str) -> Source<'static> {
    Source { url: format!("{}{path}", s.base), hosts: LOCAL, loopback_http_for_tests: true }
}

fn code(e: Error) -> &'static str {
    match e {
        Error::Download(c, _) => c,
        Error::Cancelled => "cancelled",
        e => panic!("unexpected error {e}"),
    }
}

#[test]
fn a_download_is_verified_and_renamed_into_place() {
    let data = body();
    let sha = sha256_bytes(&data);
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("models/m/model.gguf");
    let last = AtomicU64::new(0);
    let want = Want { size: data.len() as u64, sha256: &sha };
    download(&src(&s, "/file"), &want, &dest, &|d, t| { assert_eq!(t, data.len() as u64); last.store(d, Ordering::SeqCst) }, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(last.load(Ordering::SeqCst), data.len() as u64);
    assert!(!tmp.path().join("models/m/model.gguf.part").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dest.parent().unwrap()).unwrap().permissions().mode() & 0o077, 0, "a private folder");
    }
    assert_eq!(s.ranges.lock().unwrap().as_slice(), &[None]);
}

#[test]
fn a_redirect_is_followed_only_to_an_allowed_host() {
    let data = body();
    let sha = sha256_bytes(&data);
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let want = Want { size: data.len() as u64, sha256: &sha };
    // the same server under another name: not on the list
    let port = s.base.rsplit(':').next().unwrap();
    let elsewhere = Source { url: format!("{}/to/http%3A//localhost:{port}/file", s.base), hosts: LOCAL, loopback_http_for_tests: true };
    let dest = tmp.path().join("a/model");
    assert_eq!(code(download(&elsewhere, &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "host_not_allowed");
    assert!(!dest.exists());
    assert!(s.ranges.lock().unwrap().is_empty(), "the refused host was never asked");
    // allowed: followed
    let allowed = Source { url: format!("{}/to/{}/file", s.base, s.base.replace(':', "%3A")), hosts: &["127.0.0.1", "localhost"], loopback_http_for_tests: true };
    download(&allowed, &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[test]
fn a_wrong_file_is_removed_and_nothing_is_installed() {
    let data = body();
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("m/model");
    let bad = "0".repeat(64);
    let want = Want { size: data.len() as u64, sha256: &bad };
    assert_eq!(code(download(&src(&s, "/file"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "checksum_mismatch");
    assert!(!dest.exists() && !tmp.path().join("m/model.part").exists());

    let sha = sha256_bytes(&data);
    let want = Want { size: data.len() as u64, sha256: &sha };
    assert_eq!(code(download(&src(&s, "/big"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "too_big");
    assert!(!dest.exists() && !tmp.path().join("m/model.part").exists());
    assert_eq!(code(download(&src(&s, "/missing"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "network");
}

#[test]
fn an_interrupted_download_resumes_where_it_stopped() {
    let data = body();
    let sha = sha256_bytes(&data);
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("m/model");
    let want = Want { size: data.len() as u64, sha256: &sha };
    // the server stops half-way: the part is kept
    assert_eq!(code(download(&src(&s, "/short"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "network");
    let part = tmp.path().join("m/model.part");
    assert_eq!(std::fs::metadata(&part).unwrap().len(), data.len() as u64 / 2);
    assert!(!dest.exists());
    download(&src(&s, "/file"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(s.ranges.lock().unwrap().last().unwrap().as_deref(), Some(format!("bytes={}-", data.len() / 2).as_str()));

    // a server that ignores the range sends everything again: the file starts over and is still right
    std::fs::remove_file(&dest).unwrap();
    std::fs::write(&part, &data[..1000]).unwrap();
    download(&src(&s, "/plain"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // a part that is already complete is only verified
    std::fs::remove_file(&dest).unwrap();
    std::fs::write(&part, &data).unwrap();
    let before = s.hits.load(Ordering::SeqCst);
    download(&src(&s, "/file"), &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(s.hits.load(Ordering::SeqCst), before, "no request");
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[test]
fn a_cancelled_download_keeps_its_part_for_the_next_try() {
    let data = body();
    let sha = sha256_bytes(&data);
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("m/model");
    let want = Want { size: data.len() as u64, sha256: &sha };
    let cancel = AtomicBool::new(true);
    assert_eq!(code(download(&src(&s, "/file"), &want, &dest, &|_, _| {}, &cancel).unwrap_err()), "cancelled");
    assert!(!dest.exists());
    assert!(tmp.path().join("m/model.part").exists());
}

#[test]
fn plain_http_is_refused_off_loopback() {
    let tmp = tempfile::tempdir().unwrap();
    let sha = "0".repeat(64);
    let s = Source { url: "http://example.com/model".into(), hosts: &["example.com"], loopback_http_for_tests: true };
    assert_eq!(code(download(&s, &Want { size: 1, sha256: &sha }, &tmp.path().join("m/x"), &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "host_not_allowed");
}

#[test]
fn an_imported_file_must_be_the_pinned_one() {
    let data = body();
    let sha = sha256_bytes(&data);
    let tmp = tempfile::tempdir().unwrap();
    let picked = tmp.path().join("Downloads/whatever-name.gguf");
    std::fs::create_dir_all(picked.parent().unwrap()).unwrap();
    std::fs::write(&picked, &data).unwrap();
    let dest = tmp.path().join("models/m/model.gguf");
    let want = Want { size: data.len() as u64, sha256: &sha };
    import_file(&picked, &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(std::fs::read(&picked).unwrap(), data, "the user's file is only read");

    std::fs::remove_file(&dest).unwrap();
    let other = "1".repeat(64);
    let e = import_file(&picked, &Want { size: data.len() as u64, sha256: &other }, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
    assert_eq!(code(e), "wrong_file");
    assert!(!dest.exists() && !tmp.path().join("models/m/model.gguf.part").exists());
    let e = import_file(&picked, &Want { size: 5, sha256: &sha }, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
    assert_eq!(code(e), "wrong_file", "another size is refused before copying");
    assert!(!tmp.path().join("models/m/model.gguf.part").exists());
}

#[test]
fn servers_that_misbehave_are_caught() {
    let data = body();
    let sha = sha256_bytes(&data);
    let s = server(data.clone());
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("m/model");
    let part = tmp.path().join("m/model.part");
    let want = Want { size: data.len() as u64, sha256: &sha };
    let go = |path: &str| download(&src(&s, path), &want, &dest, &|_, _| {}, &AtomicBool::new(false));

    // more than the pin without a Content-Length: cut off and removed
    assert_eq!(code(go("/chunkbig").unwrap_err()), "too_big");
    assert!(!part.exists() && !dest.exists());
    // a redirect loop ends
    assert_eq!(code(go("/loop").unwrap_err()), "network");
    // https -> plain http to a non-loopback host is refused, even when the host is listed
    let hop = Source { url: format!("{}/to/http%3A//example.com/file", s.base), hosts: &["127.0.0.1", "example.com"], loopback_http_for_tests: true };
    assert_eq!(code(download(&hop, &want, &dest, &|_, _| {}, &AtomicBool::new(false)).unwrap_err()), "host_not_allowed");

    // a resume answered at the wrong place: refused, the part is kept for a later try
    std::fs::create_dir_all(part.parent().unwrap()).unwrap();
    std::fs::write(&part, &data[..1000]).unwrap();
    assert_eq!(code(go("/badrange").unwrap_err()), "network");
    assert_eq!(std::fs::read(&part).unwrap(), &data[..1000]);
    assert!(!dest.exists());
    // a server that refuses the range (416): the file starts over and is right
    go("/r416").unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(s.ranges.lock().unwrap().iter().rev().take(2).cloned().collect::<Vec<_>>(), vec![None, Some("bytes=1000-".to_string())]);
}

#[test]
fn a_swapped_file_does_not_inherit_the_cached_hash() {
    use meetko_core::live::{check_model_for, remember_sha256, ROLE_DIARIZATION};
    use meetko_core::model_setup::offer;
    let tmp = tempfile::tempdir().unwrap();
    let o = offer("diarization").unwrap();
    std::fs::create_dir_all(o.dir(tmp.path())).unwrap();
    std::fs::write(o.path(tmp.path()), b"first").unwrap();
    let cache = tmp.path().join("model-sha256-cache.json");
    // what the app records after a verified install: the status trusts it for this exact file
    remember_sha256(&o.path(tmp.path()), o.sha256(), &cache);
    assert!(check_model_for(ROLE_DIARIZATION, &o.dir(tmp.path()), false, &cache).is_ok());
    // another file put in its place (a new inode and stamp) is hashed again and refused
    let other = tmp.path().join("other");
    std::fs::write(&other, b"second file").unwrap();
    std::fs::rename(&other, o.path(tmp.path())).unwrap();
    assert!(check_model_for(ROLE_DIARIZATION, &o.dir(tmp.path()), false, &cache).is_err());
}
