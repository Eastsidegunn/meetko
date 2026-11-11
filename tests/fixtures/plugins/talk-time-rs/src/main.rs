//! meetko plugin: talk time per speaker.
//!
//! plugin(meeting data) -> table
//!
//! Reads only the fields it needs through serde_json::Value, so fields added
//! to meeting data later do not break it (tolerant reader).

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

const ID: &str = "meetko/talk-time-rs";
const VERSION: &str = "0.1.0";

fn run(md: &Value) -> Result<Value, String> {
    let speakers = md["speakers"].as_array().ok_or("speakers missing")?;
    let spans = md["speaker_spans"].as_array().ok_or("speaker_spans missing")?;
    let turns = md["turns"].as_array().ok_or("turns missing")?;
    let words = md["words"].as_array().ok_or("words missing")?;

    let mut talk_ms: BTreeMap<&str, i64> = BTreeMap::new();
    for s in spans {
        let id = s["speaker"].as_str().ok_or("span speaker")?;
        let d = s["end_ms"].as_i64().ok_or("end_ms")? - s["start_ms"].as_i64().ok_or("start_ms")?;
        *talk_ms.entry(id).or_default() += d;
    }
    let mut word_count: BTreeMap<&str, usize> = BTreeMap::new();
    for w in words {
        if let Some(id) = w["speaker"].as_str() {
            *word_count.entry(id).or_default() += 1;
        }
    }

    let mut rows = Vec::new();
    for sp in speakers {
        let id = sp["id"].as_str().ok_or("speaker id")?;
        // every row must point at the transcript; a speaker with no turns gets no row
        let evidence: Vec<Value> = turns
            .iter()
            .filter(|t| t["speaker"].as_str() == Some(id))
            .take(32)
            .map(|t| json!({"from": t["first_word"], "to": t["last_word"]}))
            .collect();
        if evidence.is_empty() {
            continue;
        }
        let label = sp["label"].as_str().unwrap_or(id);
        let secs = *talk_ms.get(id).unwrap_or(&0) as f64 / 1000.0;
        rows.push(json!({
            "cells": [label, format!("{:.1}", secs), word_count.get(id).unwrap_or(&0).to_string()],
            "evidence": evidence,
        }));
    }

    Ok(json!({
        "contract": "0.1",
        "plugin": {"id": ID, "version": VERSION},
        "input": {"version_id": md["version"]["id"]},
        "items": [{
            "type": "table",
            "title": "발언 시간",
            "columns": ["화자", "발언 시간(초)", "단어 수"],
            "rows": rows,
        }],
    }))
}

fn handle(msg: &Value) -> Result<Value, (i64, String)> {
    let params = &msg["params"];
    match msg["method"].as_str() {
        Some("initialize") => {
            let ok = params["contract"]
                .as_array()
                .is_some_and(|v| v.iter().any(|c| c == "0.1"));
            if !ok {
                return Err((-32001, "unsupported contract".into()));
            }
            Ok(json!({"contract": "0.1", "plugin": {"id": ID, "version": VERSION}, "outputs": ["table"]}))
        }
        Some("run") => run(&params["input"]).map_err(|e| (-32010, e)),
        Some("shutdown") => Ok(Value::Null),
        other => Err((-32601, format!("method not found: {other:?}"))),
    }
}

fn main() {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("bad json: {e}");
                continue;
            }
        };
        let reply = match handle(&msg) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": msg["id"], "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": code, "message": message}})
            }
        };
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
        if msg["method"] == "shutdown" {
            break;
        }
    }
}
