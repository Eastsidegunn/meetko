//! Results handed in by an external AI agent over MCP (`crate::mcp`).
//!
//! A result lives in a *slot*: an agent-chosen id in the reserved `mcp:` namespace
//! (`mcp:summary-weekly`). Installed plugin ids can never start with `mcp:` (`add_plugin_from`
//! refuses them), so a slot cannot collide with or impersonate a plugin. Each slot has a row in
//! `plugins` only because `plugin_results.plugin_id` references it: an empty `dir`, the latest
//! provenance record as `manifest_json`, and its sha256 as both hashes. Those rows are hidden from
//! the plugin list and cannot be run.
//!
//! A result row in a slot has `sandbox = 'mcp'` (produced outside meetko, no isolation of ours
//! applied), `declared_network = 1` (meetko cannot know what the agent sent where) and
//! `declared_audio = 'none'` (MCP never hands out audio). Its manifest hash is the sha256 of the
//! provenance record it was stored with, which is kept in `agent_provenance` (migration 5) keyed by
//! that hash and written in the result's transaction; `ResultRow::agent` joins it back. A record no
//! result points at any more is removed with the last such result.
//!
//! Storing into a slot for a version replaces that slot's earlier result for the same version in
//! one transaction; results of the slot on other versions stay, with the usual stale flag.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use super::{ResultRow, Store};
use crate::contract::output_errors;
use crate::error::{Error, Result};
use crate::util::{now_rfc3339, sha256_bytes};

/// The reserved id namespace of agent results.
pub const AGENT_PREFIX: &str = "mcp:";
/// `plugin_results.sandbox` of an agent result.
pub const SANDBOX_MCP: &str = "mcp";
/// Longest slot name after the prefix.
pub const MAX_SLOT_NAME: usize = 64;

/// Migration 5: each agent result's provenance record, keyed by its sha256.
pub(super) const MIGRATION: &str = "
    CREATE TABLE agent_provenance (
        sha256 TEXT PRIMARY KEY,            -- = plugin_results.plugin_manifest_sha256 of an mcp result
        json TEXT NOT NULL                  -- {via, network, protocol, client, agent}
    );
";
/// Records no agent result points at any more.
const DROP_UNUSED_PROVENANCE: &str = "DELETE FROM agent_provenance WHERE sha256 NOT IN
    (SELECT plugin_manifest_sha256 FROM plugin_results WHERE sandbox = 'mcp')";

/// True for an id in the reserved agent namespace (case-insensitive, so `MCP:x` is reserved too).
pub fn is_agent_id(id: &str) -> bool {
    id.get(..AGENT_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(AGENT_PREFIX))
}

/// A slot id from what an agent sent: `summary-weekly` or `mcp:summary-weekly` both give
/// `mcp:summary-weekly`. The name is lowercase ASCII letters, digits, `.`, `_`, `-`, starting
/// with a letter or digit, at most [`MAX_SLOT_NAME`] characters.
pub fn slot_id(raw: &str) -> Result<String> {
    let name = if is_agent_id(raw) { &raw[AGENT_PREFIX.len()..] } else { raw };
    let ok = !name.is_empty()
        && name.len() <= MAX_SLOT_NAME
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if !ok {
        return Err(Error::Invalid(format!(
            "result id must be mcp:<name>, the name 1-{MAX_SLOT_NAME} characters of a-z, 0-9, '.', '_', '-' starting with a letter or digit"
        )));
    }
    Ok(format!("{AGENT_PREFIX}{name}"))
}

/// One meeting for an overview list: what it is and what it holds.
#[derive(Debug, Clone, Serialize)]
pub struct MeetingOverview {
    pub meeting_id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub created_at: String,
    pub duration_ms: i64,
    /// the version the app opens: latest approved, else latest draft, else latest live
    pub head_version_id: Option<String>,
    pub head_kind: Option<String>,
    pub versions: i64,
    pub results: i64,
}

impl Store {
    /// Meetings newest first (by start, else import time, then id): `limit` of them after the
    /// meeting `after` = (its sort time, its id), and the total. Keyed, not counted, so a meeting
    /// imported between two pages does not shift the second.
    #[allow(clippy::type_complexity)]
    pub fn meeting_overview(&self, after: Option<(&str, &str)>, limit: usize) -> Result<(Vec<MeetingOverview>, usize)> {
        let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM meetings", [], |r| r.get(0))?;
        let mut st = self.conn.prepare(
            "SELECT m.id, m.title, m.started_at, m.created_at, m.duration_ms,
                    (SELECT v.id FROM versions v WHERE v.meeting_id = m.id
                       ORDER BY CASE v.kind WHEN 'approved' THEN 2 WHEN 'draft' THEN 1 ELSE 0 END DESC, v.seq DESC LIMIT 1),
                    (SELECT COUNT(*) FROM versions v WHERE v.meeting_id = m.id),
                    (SELECT COUNT(*) FROM plugin_results r WHERE r.meeting_id = m.id)
             FROM meetings m
             WHERE ?2 IS NULL OR COALESCE(m.started_at, m.created_at) < ?2 OR (COALESCE(m.started_at, m.created_at) = ?2 AND m.id > ?3)
             ORDER BY COALESCE(m.started_at, m.created_at) DESC, m.id LIMIT ?1",
        )?;
        let rows: Vec<(String, Option<String>, Option<String>, String, i64, Option<String>, i64, i64)> = st
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX), after.map(|a| a.0), after.map(|a| a.1)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (meeting_id, title, started_at, created_at, duration_ms, head, versions, results) in rows {
            let head_kind = match &head {
                Some(h) => Some(self.version_row(h)?.kind.as_str().to_string()),
                None => None,
            };
            out.push(MeetingOverview { meeting_id, title, started_at, created_at, duration_ms, head_version_id: head, head_kind, versions, results });
        }
        Ok((out, total as usize))
    }

    /// The head version id of a meeting (as `meeting_overview`), if it has any version.
    pub fn head_version(&self, meeting_id: &str) -> Result<Option<String>> {
        self.meeting(meeting_id)?;
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM versions WHERE meeting_id = ?1
                 ORDER BY CASE kind WHEN 'approved' THEN 2 WHEN 'draft' THEN 1 ELSE 0 END DESC, seq DESC LIMIT 1",
                [meeting_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store an agent's result in `slot` (an id from [`slot_id`]) for `version_id`, replacing the
    /// slot's earlier result for that version. `output` is a full plugin-output document and is
    /// validated against the stored version exactly like a plugin's; `provenance` is the record
    /// kept with it. Returns the new result id.
    pub fn put_agent_result(&self, slot: &str, provenance: &Value, version_id: &str, output: &Value) -> Result<i64> {
        if slot_id(slot)? != slot {
            return Err(Error::Invalid(format!("{slot} is not a result id in the mcp: namespace")));
        }
        if output["plugin"]["id"].as_str() != Some(slot) {
            return Err(Error::Invalid("the output must name its result id as plugin.id".into()));
        }
        let version = output["plugin"]["version"].as_str().unwrap_or("").to_string();
        let record = serde_json::to_string(provenance)?;
        let hash = sha256_bytes(record.as_bytes());
        self.immediate(|| {
            let v = self.version_row(version_id)?;
            let md = self.version(version_id)?;
            let errs = output_errors(&md, output);
            if !errs.is_empty() {
                return Err(Error::Invalid(format!("rejected output: {}", errs.into_iter().take(10).collect::<Vec<_>>().join("; "))));
            }
            // a real plugin row under this id (installed before the namespace was reserved) is never touched
            let dir: Option<String> = self.conn.query_row("SELECT dir FROM plugins WHERE id=?1", [slot], |r| r.get(0)).optional()?;
            if dir.is_some_and(|d| !d.is_empty()) {
                return Err(Error::Invalid(format!("{slot} is an installed plugin's id; choose another result id")));
            }
            self.conn.execute(
                "INSERT INTO plugins (id, version, dir, manifest_json, manifest_sha256, code_sha256, network, audio, added_at)
                 VALUES (?1, ?2, '', ?3, ?4, ?4, 1, 'none', ?5)
                 ON CONFLICT(id) DO UPDATE SET version=excluded.version, manifest_json=excluded.manifest_json,
                     manifest_sha256=excluded.manifest_sha256, code_sha256=excluded.code_sha256, added_at=excluded.added_at",
                params![slot, version, record, hash, now_rfc3339()],
            )?;
            self.conn.execute(
                "DELETE FROM plugin_results WHERE plugin_id=?1 AND version_id=?2 AND sandbox=?3",
                params![slot, version_id, SANDBOX_MCP],
            )?;
            self.conn.execute("INSERT OR IGNORE INTO agent_provenance (sha256, json) VALUES (?1, ?2)", params![hash, record])?;
            self.conn.execute(
                "INSERT INTO plugin_results (plugin_id, plugin_version, version_id, meeting_id, output_json, created_at,
                     plugin_manifest_sha256, plugin_code_sha256, declared_network, declared_audio, sandbox)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 1, 'none', ?8)",
                params![slot, version, version_id, v.meeting_id, serde_json::to_string(output)?, now_rfc3339(), hash, SANDBOX_MCP],
            )?;
            let id = self.conn.last_insert_rowid();
            self.conn.execute(DROP_UNUSED_PROVENANCE, [])?;
            Ok(id)
        })
    }

    /// Every agent result, optionally only of one slot and/or one meeting, oldest first.
    pub fn agent_results(&self, slot: Option<&str>, meeting_id: Option<&str>) -> Result<Vec<ResultRow>> {
        let rows = match meeting_id {
            Some(m) => self.results_for_meeting(m)?,
            None => self.results("r.sandbox = ?1", SANDBOX_MCP)?,
        };
        Ok(rows.into_iter().filter(|r| r.sandbox == SANDBOX_MCP && is_agent_id(&r.plugin_id) && slot.is_none_or(|s| r.plugin_id == s)).collect())
    }

    /// Remove the agent results of `slot` (only on `version_id` when given). Plugin results and
    /// versions are never touched. The slot's row goes once it holds no result. Returns how many
    /// results were removed.
    pub fn remove_agent_results(&self, slot: &str, version_id: Option<&str>) -> Result<usize> {
        if slot_id(slot)? != slot {
            return Err(Error::Invalid(format!("{slot} is not a result id in the mcp: namespace")));
        }
        self.immediate(|| {
            let n = match version_id {
                Some(v) => self.conn.execute(
                    "DELETE FROM plugin_results WHERE plugin_id=?1 AND sandbox=?2 AND version_id=?3",
                    params![slot, SANDBOX_MCP, v],
                )?,
                None => self.conn.execute("DELETE FROM plugin_results WHERE plugin_id=?1 AND sandbox=?2", params![slot, SANDBOX_MCP])?,
            };
            let left: i64 = self.conn.query_row("SELECT COUNT(*) FROM plugin_results WHERE plugin_id=?1", [slot], |r| r.get(0))?;
            if left == 0 {
                self.conn.execute("DELETE FROM plugins WHERE id=?1 AND dir=''", [slot])?;
            }
            self.conn.execute(DROP_UNUSED_PROVENANCE, [])?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_ids_are_namespaced_and_plain() {
        assert_eq!(slot_id("weekly-summary").unwrap(), "mcp:weekly-summary");
        assert_eq!(slot_id("mcp:a.b_c-1").unwrap(), "mcp:a.b_c-1");
        assert_eq!(slot_id("MCP:x").unwrap(), "mcp:x");
        for bad in ["", "mcp:", "Summary", "meetko/summary", "-x", "a b", "mcp:mcp:x", &"a".repeat(MAX_SLOT_NAME + 1), "요약"] {
            assert!(slot_id(bad).is_err(), "{bad}");
        }
        assert!(is_agent_id("mcp:x") && is_agent_id("Mcp:x") && !is_agent_id("meetko/x") && !is_agent_id("mc"));
    }
}
