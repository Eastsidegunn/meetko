// Link the NeMo-Speech.cpp C API from vendor/lib (fetched by fetch-libs.sh) and let the binary find
// it at run time with relative rpaths only, so no machine path is embedded.
//
// Release builds get only the shipped layouts (an app bundle's Frameworks/ or a lib/ beside the
// binary). The repository-relative dev rpaths are emitted for debug builds, or when
// MEETKO_ENGINE_DEV_RPATH=1: a shipped binary must not load libraries from a directory that
// happens to sit two levels up. (The libraries find each other through their own @loader_path.)
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib = dir.join("vendor/lib");
    let found = ["libnemo_speech_asr_c.dylib", "libnemo_speech_asr_c.so", "nemo_speech_asr_c.lib"].iter().any(|f| lib.join(f).exists());
    if !found {
        panic!("NeMo-Speech.cpp libraries are missing: run engines/nemotron-stream/fetch-libs.sh first");
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=nemo_speech_asr_c");
    let dev = std::env::var("PROFILE").is_ok_and(|p| p == "debug") || std::env::var_os("MEETKO_ENGINE_DEV_RPATH").is_some_and(|v| v == "1");
    // macOS dyld prefixes; the ELF equivalent is $ORIGIN (Linux is not exercised yet)
    let (exe, mut rpaths) = if cfg!(target_os = "macos") { ("@executable_path", vec!["/../Frameworks", "/../lib"]) } else { ("$ORIGIN", vec!["/../lib"]) };
    if dev {
        // target/<profile>/bin and target/<profile>/deps/test -> engines/nemotron-stream/vendor/lib
        rpaths.extend(["/../../vendor/lib", "/../../../vendor/lib"]);
    }
    if !cfg!(windows) {
        for rp in rpaths {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{exe}{rp}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/.version");
    println!("cargo:rerun-if-env-changed=MEETKO_ENGINE_DEV_RPATH");
}
