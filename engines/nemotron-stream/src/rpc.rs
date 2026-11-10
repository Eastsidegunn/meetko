//! The few lines of NDJSON JSON-RPC both engine binaries (stream and diarization) share.

use std::io::Write;

use serde_json::{json, Value};

/// One message on stdout, flushed: stdout carries protocol only.
pub fn out(v: &Value) {
    let mut so = std::io::stdout().lock();
    let _ = writeln!(so, "{v}");
    let _ = so.flush();
}

pub fn reply(id: &Value, result: Result<Value, (i64, String)>) {
    match result {
        Ok(r) => out(&json!({ "jsonrpc": "2.0", "id": id, "result": r })),
        Err((code, message)) => out(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })),
    }
}

extern "C" {
    fn _exit(code: i32) -> !;
}

/// Announce the failure (a notification named `method`, params `{message}`), then end the process
/// with exit code 1. `_exit` once the message is flushed: `exit` would run the static destructors
/// of the ggml backends while the session still holds the model, and ggml-metal asserts (SIGABRT)
/// there.
pub fn fatal(method: &str, message: &str) -> ! {
    out(&json!({ "jsonrpc": "2.0", "method": method, "params": { "message": message } }));
    eprintln!("fatal: {message}");
    // SAFETY: ends the process at once; stdout was flushed by `out`, stderr is unbuffered
    unsafe { _exit(1) }
}
