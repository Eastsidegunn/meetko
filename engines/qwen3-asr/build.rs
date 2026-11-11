// Link the CrispASR C API from vendor/lib (fetched by fetch-libs.sh) and let the binary find it at run
// time with relative rpaths only, so no machine path is embedded.
//
// Only the binary links the library: the crate's lib (protocol, wav, chunking, words) is pure Rust, so
// its unit tests do not need it. Release builds get only the shipped layouts (an app bundle's
// Frameworks/ or a lib/ beside the binary). The repository-relative dev rpaths are emitted for debug
// builds, or when MEETKO_ENGINE_DEV_RPATH=1, as in engines/nemotron-stream. The libraries find each
// other through their own @loader_path.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib = dir.join("vendor/lib");
    if !lib.join("libcrispasr.1.dylib").exists() {
        panic!("CrispASR libraries are missing: run engines/qwen3-asr/fetch-libs.sh first (macOS arm64 only for now)");
    }
    // the release the libraries came from, recorded in the engine version (the C API has its own number)
    let release = std::fs::read_to_string(dir.join("vendor/.version")).unwrap_or_default();
    println!("cargo:rustc-env=CRISPASR_RELEASE={}", release.trim());
    println!("cargo:rustc-link-search=native={}", lib.display());
    // the library's install name is @rpath/libcrispasr.1.dylib; linking that file directly keeps a
    // second 18 MB copy under the plain link name out of vendor/
    println!("cargo:rustc-link-arg-bins=-lcrispasr.1");
    let dev = std::env::var("PROFILE").is_ok_and(|p| p == "debug") || std::env::var_os("MEETKO_ENGINE_DEV_RPATH").is_some_and(|v| v == "1");
    let mut rpaths = vec!["/../Frameworks", "/../lib"];
    if dev {
        // target/<profile>/bin -> engines/qwen3-asr/vendor/lib
        rpaths.extend(["/../../vendor/lib", "/../../../vendor/lib"]);
    }
    for rp in rpaths {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path{rp}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/.version");
    println!("cargo:rerun-if-env-changed=MEETKO_ENGINE_DEV_RPATH");
}
