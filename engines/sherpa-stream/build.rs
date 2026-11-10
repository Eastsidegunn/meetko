// Link the sherpa-onnx C API from vendor/lib (fetched by fetch-libs.sh) and let the binary find
// it at run time with relative rpaths only, so no machine path is embedded.
//
// Release builds get only the shipped layouts (an app bundle's Frameworks/ or a lib/ beside the
// binary). The repository-relative dev rpaths are emitted for debug builds, or when
// MEETKO_ENGINE_DEV_RPATH=1: a shipped binary must not load libraries from a directory that
// happens to sit two levels up.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib = dir.join("vendor/lib");
    if !lib.join("libsherpa-onnx-c-api.dylib").exists() && !lib.join("libsherpa-onnx-c-api.so").exists() {
        panic!("sherpa-onnx libraries are missing: run engines/sherpa-stream/fetch-libs.sh first");
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=sherpa-onnx-c-api");
    let dev = std::env::var("PROFILE").is_ok_and(|p| p == "debug") || std::env::var_os("MEETKO_ENGINE_DEV_RPATH").is_some_and(|v| v == "1");
    let mut rpaths = vec!["@executable_path/../Frameworks", "@executable_path/../lib"];
    if dev {
        // target/<profile>/bin and target/<profile>/deps/test -> engines/sherpa-stream/vendor/lib
        rpaths.extend(["@executable_path/../../vendor/lib", "@executable_path/../../../vendor/lib"]);
    }
    for rp in rpaths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{rp}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/.version");
    println!("cargo:rerun-if-env-changed=MEETKO_ENGINE_DEV_RPATH");
}
