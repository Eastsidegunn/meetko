//! SQLite storage: meetings, recordings, immutable versions, plugins, plugin results.
//!
//! Versions are never updated. Approving copies a draft's stored JSON into a new
//! `approved` version, changing only `version.*`. A plugin result stays bound to the
//! version it read, and is stale when the meeting's head (the latest approved version,
//! else the latest draft) has different content from that version.
//!
//! What a person changes on a transcript (speaker names, merges, edited text) is kept beside the
//! versions as overlays (`store::overlay`), never in them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::contract::{
    invariant_errors, meeting_data_validator, output_errors, schema_errors, Channel, Meeting, MeetingData, Recording, Span,
    VersionKind,
};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::util::{self, content_sha256, create_private, create_private_dir, create_private_new, is_sha256, tighten_private_dir, now_rfc3339, sha256_bytes, sha256_file};

/// Additive only after release. Pre-release: v1 is edited in place instead of adding v2
/// (kind is validated in Rust, not by CHECK).
const MIGRATIONS: &[&str] = &[
    // 1
    "
    CREATE TABLE meetings (
        id TEXT PRIMARY KEY,
        title TEXT,
        language_hint TEXT,
        started_at TEXT,
        duration_ms INTEGER NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE TABLE recordings (
        meeting_id TEXT PRIMARY KEY REFERENCES meetings(id),
        sha256 TEXT NOT NULL UNIQUE,        -- the combined recording
        sample_rate INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL
    );
    CREATE TABLE recording_channels (
        meeting_id TEXT NOT NULL REFERENCES meetings(id),
        channel INTEGER NOT NULL,
        role TEXT NOT NULL,
        label TEXT,
        sha256 TEXT NOT NULL,               -- this channel file
        file TEXT NOT NULL,                 -- relative to the data dir, '/' separators
        sample_rate INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL,
        coverage_json TEXT,                 -- NULL: whole file (no capture gap information)
        PRIMARY KEY (meeting_id, channel)
    );
    CREATE TABLE versions (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE,
        meeting_id TEXT NOT NULL REFERENCES meetings(id),
        kind TEXT NOT NULL,                 -- live | draft | approved, checked in Rust
        parent TEXT REFERENCES versions(id),
        created_at TEXT NOT NULL,
        content_sha256 TEXT NOT NULL,       -- canonical meeting-data without `version`
        json TEXT NOT NULL
    );
    CREATE TRIGGER versions_no_update BEFORE UPDATE ON versions
        BEGIN SELECT RAISE(ABORT, 'versions are immutable'); END;
    CREATE TRIGGER versions_no_delete BEFORE DELETE ON versions
        BEGIN SELECT RAISE(ABORT, 'versions are immutable'); END;
    CREATE TABLE plugins (
        id TEXT PRIMARY KEY,
        version TEXT NOT NULL,
        dir TEXT NOT NULL,
        manifest_json TEXT NOT NULL,
        manifest_sha256 TEXT NOT NULL,
        code_sha256 TEXT NOT NULL,
        network INTEGER NOT NULL,           -- declared permissions.network (not enforced)
        audio TEXT NOT NULL,                -- declared input.audio (not enforced)
        added_at TEXT NOT NULL
    );
    CREATE TABLE plugin_results (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        plugin_id TEXT NOT NULL REFERENCES plugins(id),
        plugin_version TEXT NOT NULL,
        version_id TEXT NOT NULL REFERENCES versions(id),
        meeting_id TEXT NOT NULL REFERENCES meetings(id),
        output_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        plugin_manifest_sha256 TEXT NOT NULL,   -- what ran, at the time it ran
        plugin_code_sha256 TEXT NOT NULL,
        declared_network INTEGER NOT NULL,
        declared_audio TEXT NOT NULL
    );
    CREATE INDEX plugin_results_version ON plugin_results(version_id);
    CREATE INDEX versions_meeting ON versions(meeting_id, seq);
    ",
    // isolation actually applied to a run: macos-sbpl | none | off. Results stored before this ran unsandboxed.
    "ALTER TABLE plugin_results ADD COLUMN sandbox TEXT NOT NULL DEFAULT 'none';",
    // 3: where a plugin installed from a source spec came from (store/sources.rs)
    "
    CREATE TABLE plugin_sources (
        plugin_id TEXT PRIMARY KEY REFERENCES plugins(id),
        repo TEXT NOT NULL,                 -- github:OWNER/REPO
        sha TEXT NOT NULL,                  -- the full commit that was downloaded
        ref TEXT NOT NULL,                  -- what the user asked for (tag or sha)
        installed_at TEXT NOT NULL
    );
    ",
    // 4: full-text search over each meeting's head version (store/search.rs; backfilled in `open`)
    search::MIGRATION,
    // 5: provenance records of results handed in by AI agents over MCP (store/agent.rs)
    agent::MIGRATION,
    // 6: speaker names, merges and edited turn text, overlays on a transcript (store/overlay.rs)
    overlay::MIGRATION,
];

pub(crate) fn source_conflict(id: &str, before: Option<&crate::plugin_source::PluginSource>, new: Option<&crate::plugin_source::PluginSource>) -> Error {
    let show = |s: Option<&crate::plugin_source::PluginSource>| s.map(|s| s.repo.clone()).unwrap_or_else(|| "a local folder".into());
    Error::PluginSourceChanged(format!(
        "{id} is installed from {}; this install comes from {}. Replacing it needs an explicit confirmation of exactly that installed version (--replace, or the second confirmation in the app)",
        show(before),
        show(new)
    ))
}

/// Identifies the installed state a replace confirmation was given for: version and recorded source.
pub fn replace_key(version: &str, source: Option<&crate::plugin_source::PluginSource>) -> String {
    match source {
        Some(s) => format!("{version}|{}@{}", s.repo, s.sha),
        None => format!("{version}|local"),
    }
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelFile {
    pub index: u32,
    pub role: String,
    pub label: Option<String>,
    pub sha256: String,
    pub path: PathBuf,
    pub sample_rate: u32,
    pub duration_ms: i64,
    /// spans the capture says were really recorded; None when nothing is known
    pub coverage: Option<Vec<Span>>,
}

#[derive(Debug, Clone)]
pub struct MeetingRow {
    pub id: String,
    pub title: Option<String>,
    pub language_hint: Option<String>,
    pub duration_ms: i64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct VersionRow {
    pub id: String,
    pub meeting_id: String,
    pub kind: VersionKind,
    pub parent: Option<String>,
    pub created_at: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    pub network: bool,
    pub audio: String,
    pub manifest_sha256: String,
    pub code_sha256: String,
}

#[derive(Debug, Clone)]
pub struct ResultRow {
    pub id: i64,
    pub plugin_id: String,
    pub plugin_version: String,
    pub version_id: String,
    pub created_at: String,
    pub stale: bool,
    pub output: Value,
    pub plugin_code_sha256: String,
    pub plugin_manifest_sha256: String,
    /// the permissions the plugin declared when it ran (declared, not enforced)
    pub declared_network: bool,
    pub declared_audio: String,
    /// the OS isolation the run had: `macos-sbpl`, `none` (not available), `off` (opted out),
    /// `unrecorded` (stored by `add_result` without a host run), `mcp` (handed in by an external
    /// agent, see `store::agent`)
    pub sandbox: String,
    /// for an agent result: the provenance record it was stored with (client, agent, network);
    /// `None` for plugin results
    pub agent: Option<Value>,
}

/// One channel file to import.
#[derive(Debug, Clone)]
pub struct ImportChannel {
    pub index: u32,
    pub role: String,
    pub label: Option<String>,
    pub path: PathBuf,
    /// verified while copying when given
    pub expected_sha256: Option<String>,
    pub coverage: Option<Vec<Span>>,
}

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub title: Option<String>,
    pub language_hint: Option<String>,
    pub started_at: Option<String>,
    /// from a capture manifest; else the longest channel
    pub duration_ms: Option<i64>,
    pub expected_recording_sha256: Option<String>,
}

pub struct ImportOutcome {
    pub meeting: MeetingRow,
    /// the same recording (same hash) was imported before: that meeting is returned
    pub existing: bool,
}

/// The combined recording's hash: sha256 over the lines `"<index>:<channel sha256>\n"` in
/// index order (the definition in docs/capture-interface.md, so a capture's own
/// `recording_sha256` and ours agree).
pub fn recording_sha256(channels: &[(u32, String)]) -> String {
    let mut c: Vec<_> = channels.to_vec();
    c.sort();
    sha256_bytes(c.iter().map(|(i, s)| format!("{i}:{s}\n")).collect::<String>().as_bytes())
}

fn wav_info(path: &Path) -> Result<(u32, i64)> {
    let r = hound::WavReader::open(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let spec = r.spec();
    if spec.channels != 1 {
        return Err(Error::Invalid(format!("{}: expected a mono file, found {} channels", path.display(), spec.channels)));
    }
    let ms = r.duration() as i64 * 1000 / spec.sample_rate as i64; // floor, as the capture manifest does
    Ok((spec.sample_rate, ms))
}

/// Copy to `<dest>.part` while hashing, then rename. Returns the sha256 of what was written.
fn copy_hashing(src: &Path, dest: &Path) -> Result<String> {
    let mut input = std::fs::File::open(src)?;
    let part = dest.with_extension("wav.part");
    let mut out = create_private(&part)?;
    let mut h = sha2::Sha256::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut h, &buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.sync_all()?;
    drop(out);
    std::fs::rename(&part, dest)?;
    Ok(util::hex(&sha2::Digest::finalize(h)))
}

fn rel_to_path(base: &Path, rel: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    if rel.is_empty() || parts.iter().any(|p| p.is_empty() || *p == ".." || p.contains(['\\', ':'])) {
        return Err(Error::Corrupt(format!("stored file path {rel:?}")));
    }
    let mut p = base.to_path_buf();
    p.extend(parts);
    Ok(p)
}

fn kind_of(s: &str) -> Result<VersionKind> {
    VersionKind::parse(s)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Run `f`, trying again with a bounded backoff while SQLite says busy or locked.
fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let mut wait = std::time::Duration::from_millis(2);
    for _ in 0..12 {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) =>
            {
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_millis(250));
            }
            r => return r,
        }
    }
    f()
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        // Only a dir meetko made, or the default data dir, is ours to chmod. A folder the user
        // pointed MEETKO_DATA_DIR at keeps its permissions; they are told instead.
        let created = !dir.exists();
        if created {
            create_private_dir(dir)?;
        }
        let mut warnings = Vec::new();
        if created || same_dir(dir, &util::default_data_dir()) {
            tighten_private_dir(dir)?;
        } else if util::is_loose_dir(dir) {
            warnings.push(format!(
                "data dir {} is accessible to other users; meetko left its permissions unchanged (run chmod 700 on it to make it private)",
                dir.display()
            ));
        }
        let db = dir.join("meetko.db");
        // 0600 before SQLite creates it (and the -wal/-shm that copy its mode). create_new, not
        // truncate: a racing opener may have initialised the database a moment ago.
        match create_private_new(&db) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        // the busy timeout does not cover the switch to WAL on a fresh file, so retry it by hand
        let conn = retry_busy(|| {
            let conn = Connection::open(&db)?;
            conn.busy_timeout(std::time::Duration::from_millis(5000))?;
            let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            Ok(conn)
        })?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut s = Store { conn, dir: dir.to_path_buf(), warnings };
        s.immediate(|| {
            let have: i64 = s.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            if have as usize > MIGRATIONS.len() {
                return Err(Error::Invalid(format!("database schema {have} is newer than this build ({})", MIGRATIONS.len())));
            }
            for (i, sql) in MIGRATIONS.iter().enumerate().skip(have as usize) {
                s.conn.execute_batch(sql)?;
                s.conn.pragma_update(None, "user_version", (i + 1) as i64)?;
            }
            Ok(())
        })?;
        // Search rows for meetings stored before migration 4 (or left by an interrupted fill) are
        // filled outside the schema transaction, one meeting per transaction and only for a
        // moment here; the rest at the next opens and searches. Best effort: a busy database
        // must not fail the open.
        let _ = s.index_pending(Some(search::OPEN_INDEX_BUDGET));
        if let Ok(Some(w)) = s.unreadable_warning() {
            s.warnings.push(w);
        }
        Ok(s)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Non-fatal notes from `open` (for example a shared data dir whose permissions were left alone).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Run `f` inside BEGIN IMMEDIATE: the write lock is taken first, so reads inside
    /// (such as "next version id") cannot be invalidated by another writer.
    fn immediate<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------ meetings

    /// Copy channel files into `recordings/<combined sha256>/` and create the meeting.
    ///
    /// Every file is copied to a staging dir while hashed (and checked against the hash a
    /// capture manifest claimed), then the directory is moved into place. Importing a
    /// recording with the same combined hash again returns the existing meeting
    /// (`existing`), after re-hashing the stored files; a stored file that no longer
    /// matches is an error, never silently reused.
    pub fn import_meeting(&self, mut channels: Vec<ImportChannel>, opts: ImportOptions) -> Result<ImportOutcome> {
        if channels.is_empty() {
            return Err(Error::Invalid("no channel files".into()));
        }
        channels.sort_by_key(|c| c.index);
        if channels.windows(2).any(|w| w[0].index == w[1].index) {
            return Err(Error::Invalid("duplicate channel index".into()));
        }
        for c in &channels {
            if !matches!(c.role.as_str(), "mic" | "loopback" | "other") {
                return Err(Error::Invalid(format!("channel {}: role {:?} is not mic|loopback|other", c.index, c.role)));
            }
        }
        let rec_dir = self.dir.join("recordings");
        create_private_dir(&rec_dir)?;
        let staging = tempfile::Builder::new().prefix(".staging-").tempdir_in(&rec_dir)?; // 0700
        let file_name = |c: &ImportChannel| format!("ch{}-{}.wav", c.index, c.role);
        let mut infos = Vec::new(); // (index, sha, rate, ms)
        for c in &channels {
            let sha = copy_hashing(&c.path, &staging.path().join(file_name(c)))?;
            if let Some(want) = &c.expected_sha256 {
                if *want != sha {
                    return Err(Error::Invalid(format!("channel {}: file hash {sha} differs from the manifest's {want}", c.index)));
                }
            }
            let (rate, ms) = wav_info(&staging.path().join(file_name(c)))?;
            infos.push((c.index, sha, rate, ms));
        }
        if infos.iter().any(|i| i.2 != infos[0].2) {
            return Err(Error::Invalid("channel files have different sample rates".into()));
        }
        let combined = recording_sha256(&infos.iter().map(|i| (i.0, i.1.clone())).collect::<Vec<_>>());
        if let Some(want) = &opts.expected_recording_sha256 {
            if *want != combined {
                return Err(Error::Invalid(format!("recording hash {combined} differs from the manifest's {want}")));
            }
        }
        let dest = rec_dir.join(&combined);
        let moved = !dest.exists() && std::fs::rename(staging.path(), &dest).is_ok();
        if moved {
            let _ = staging.keep(); // the directory now lives at `dest`
        } else {
            // somebody (an earlier import, or a leftover) already holds this hash: trust nothing
            for (c, (_, sha, ..)) in channels.iter().zip(&infos) {
                let have = sha256_file(&dest.join(file_name(c)))
                    .map_err(|e| Error::Corrupt(format!("stored recording {combined}: {}: {e}", file_name(c))))?;
                if have != *sha {
                    return Err(Error::Corrupt(format!(
                        "stored recording {combined}: {} no longer matches its hash; refusing to reuse it",
                        file_name(c)
                    )));
                }
            }
        }
        let duration_ms = opts.duration_ms.unwrap_or_else(|| infos.iter().map(|i| i.3).max().unwrap_or(0));
        let sample_rate = infos[0].2;
        let created = now_rfc3339();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let id = format!("m{}", &sha256_bytes(format!("{combined}{nanos}").as_bytes())[..10]);
        self.immediate(|| {
            if let Some(mid) = self
                .conn
                .query_row("SELECT meeting_id FROM recordings WHERE sha256=?1", [&combined], |r| r.get::<_, String>(0))
                .optional()?
            {
                return Ok(ImportOutcome { meeting: self.meeting(&mid)?, existing: true });
            }
            self.conn.execute(
                "INSERT INTO meetings (id, title, language_hint, started_at, duration_ms, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, opts.title, opts.language_hint, opts.started_at, duration_ms, created],
            )?;
            self.conn.execute(
                "INSERT INTO recordings (meeting_id, sha256, sample_rate, duration_ms) VALUES (?1,?2,?3,?4)",
                params![id, combined, sample_rate, duration_ms],
            )?;
            for (c, (_, sha, rate, ms)) in channels.iter().zip(&infos) {
                let cov = c.coverage.as_ref().map(serde_json::to_string).transpose()?;
                self.conn.execute(
                    "INSERT INTO recording_channels (meeting_id, channel, role, label, sha256, file, sample_rate, duration_ms, coverage_json)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![id, c.index, c.role, c.label, sha, format!("recordings/{combined}/{}", file_name(c)), rate, ms, cov],
                )?;
            }
            Ok(ImportOutcome {
                meeting: MeetingRow { id: id.clone(), title: opts.title.clone(), language_hint: opts.language_hint.clone(), duration_ms, created_at: created.clone() },
                existing: false,
            })
        })
    }

    pub fn meeting(&self, id: &str) -> Result<MeetingRow> {
        self.conn
            .query_row(
                "SELECT id, title, language_hint, duration_ms, created_at FROM meetings WHERE id=?1",
                [id],
                |r| Ok(MeetingRow { id: r.get(0)?, title: r.get(1)?, language_hint: r.get(2)?, duration_ms: r.get(3)?, created_at: r.get(4)? }),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("meeting {id}")))
    }

    #[allow(clippy::type_complexity)] // one-off row tuples
    pub fn channel_files(&self, meeting_id: &str) -> Result<Vec<ChannelFile>> {
        let mut st = self.conn.prepare(
            "SELECT channel, role, label, sha256, file, sample_rate, duration_ms, coverage_json FROM recording_channels
             WHERE meeting_id=?1 ORDER BY channel",
        )?;
        let rows: Vec<(u32, String, Option<String>, String, String, u32, i64, Option<String>)> = st
            .query_map([meeting_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows.into_iter()
            .map(|(index, role, label, sha256, file, sample_rate, duration_ms, cov)| {
                Ok(ChannelFile {
                    index,
                    role,
                    label,
                    sha256,
                    path: rel_to_path(&self.dir, &file)?,
                    sample_rate,
                    duration_ms,
                    coverage: cov
                        .map(|c| serde_json::from_str(&c))
                        .transpose()
                        .map_err(|e| Error::Corrupt(format!("coverage of channel {index}: {e}")))?,
                })
            })
            .collect()
    }

    /// The meeting and recording blocks of meeting-data, from stored rows.
    pub fn meeting_and_recording(&self, meeting_id: &str) -> Result<(Meeting, Recording, Vec<ChannelFile>)> {
        let m = self.meeting(meeting_id)?;
        let started_at: Option<String> =
            self.conn.query_row("SELECT started_at FROM meetings WHERE id=?1", [meeting_id], |r| r.get(0))?;
        let files = self.channel_files(meeting_id)?;
        let rec: (String, u32, i64) = self.conn.query_row(
            "SELECT sha256, sample_rate, duration_ms FROM recordings WHERE meeting_id=?1",
            [meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let meeting = Meeting {
            id: m.id,
            title: m.title,
            started_at, // the capture's start; None for a plain file import
            duration_ms: m.duration_ms,
            language_hint: m.language_hint,
            channels: files.iter().map(|f| Channel { index: f.index, role: f.role.clone(), label: f.label.clone() }).collect(),
        };
        Ok((meeting, Recording { sha256: rec.0, sample_rate: rec.1, duration_ms: rec.2 }, files))
    }

    pub fn list_meetings(&self) -> Result<Vec<MeetingRow>> {
        let mut st = self.conn.prepare("SELECT id, title, language_hint, duration_ms, created_at FROM meetings ORDER BY created_at, id")?;
        let rows = st.query_map([], |r| {
            Ok(MeetingRow { id: r.get(0)?, title: r.get(1)?, language_hint: r.get(2)?, duration_ms: r.get(3)?, created_at: r.get(4)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ------------------------------------------------------------ versions

    /// Next id for `<meeting>.<kind>.<n>`; only meaningful inside `immediate`.
    fn next_version_id(&self, meeting_id: &str, kind: VersionKind) -> Result<String> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM versions WHERE meeting_id=?1 AND kind=?2",
            params![meeting_id, kind.as_str()],
            |r| r.get(0),
        )?;
        Ok(format!("{meeting_id}.{}.{}", kind.as_str(), n + 1))
    }

    /// Schema + invariants, on the stored JSON itself.
    fn validate(raw: &Value) -> Result<MeetingData> {
        let mut errs = schema_errors(meeting_data_validator(), raw);
        let md: Option<MeetingData> = match serde_json::from_value(raw.clone()) {
            Ok(m) => Some(m),
            Err(e) => {
                errs.push(format!("not meeting data: {e}"));
                None
            }
        };
        if let Some(m) = &md {
            errs.extend(invariant_errors(m));
        }
        match (errs.is_empty(), md) {
            (true, Some(m)) => Ok(m),
            _ => Err(Error::Invalid(errs.into_iter().take(10).collect::<Vec<_>>().join("; "))),
        }
    }

    /// Insert a validated version document (already carrying its id). Inside `immediate`.
    fn insert_version(&self, raw: &Value) -> Result<MeetingData> {
        let md = Self::validate(raw)?;
        self.conn.execute(
            "INSERT INTO versions (id, meeting_id, kind, parent, created_at, content_sha256, json) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                md.version.id,
                md.meeting.id,
                md.version.kind.as_str(),
                md.version.parent,
                md.version.created_at.clone().unwrap_or_else(now_rfc3339),
                content_sha256(raw),
                serde_json::to_string(raw)?
            ],
        )?;
        self.index_head(&md.meeting.id, Some(&md))?; // the search row follows the head in this transaction
        Ok(md)
    }

    /// Store a new version of `md.meeting`. The id is chosen here, inside the write lock,
    /// so concurrent writers cannot collide; whatever id `md` carried is replaced.
    pub fn create_version(&self, mut md: MeetingData) -> Result<MeetingData> {
        let kind = md.version.kind;
        self.immediate(|| {
            md.version.id = self.next_version_id(&md.meeting.id, kind)?;
            md.version.created_at = Some(now_rfc3339());
            self.insert_version(&md.to_value())
        })
    }

    pub fn version_row(&self, id: &str) -> Result<VersionRow> {
        let r: (String, String, String, Option<String>, String, String) = self
            .conn
            .query_row("SELECT id, meeting_id, kind, parent, created_at, content_sha256 FROM versions WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("version {id}")))?;
        Ok(VersionRow {
            id: r.0,
            meeting_id: r.1,
            kind: kind_of(&r.2).map_err(|e| Error::Corrupt(format!("version {id}: {e}")))?,
            parent: r.3,
            created_at: r.4,
            content_sha256: r.5,
        })
    }

    /// The version document exactly as stored.
    pub fn version_raw(&self, id: &str) -> Result<Value> {
        let text: String = self
            .conn
            .query_row("SELECT json FROM versions WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("version {id}")))?;
        serde_json::from_str(&text).map_err(|e| Error::Corrupt(format!("version {id}: {e}")))
    }

    pub fn version(&self, id: &str) -> Result<MeetingData> {
        serde_json::from_value(self.version_raw(id)?).map_err(|e| Error::Corrupt(format!("version {id}: {e}")))
    }

    pub fn versions_of(&self, meeting_id: &str) -> Result<Vec<VersionRow>> {
        let mut st = self.conn.prepare("SELECT id FROM versions WHERE meeting_id=?1 ORDER BY seq")?;
        let ids: Vec<String> = st.query_map([meeting_id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        ids.iter().map(|i| self.version_row(i)).collect()
    }

    /// "Approve" never edits: the draft's stored JSON is stored again with a new
    /// `version` object (id, kind approved, parent = the draft) and nothing else changed.
    pub fn approve(&self, draft_id: &str) -> Result<MeetingData> {
        self.immediate(|| {
            let row = self.version_row(draft_id)?;
            if row.kind != VersionKind::Draft {
                return Err(Error::Invalid(format!("{draft_id} is {}, only a draft can be approved", row.kind.as_str())));
            }
            let raw = self.version_raw(draft_id)?;
            let content = content_sha256(&raw);
            // approving twice (or two callers at once) is one approval, not a pile of copies; but
            // an earlier approval that is no longer the head must not come back as if it were
            let same: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM versions WHERE meeting_id=?1 AND kind='approved' AND parent=?2 AND content_sha256=?3 ORDER BY seq LIMIT 1",
                    params![row.meeting_id, draft_id, content],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = &same {
                let head_id: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT id FROM versions WHERE meeting_id=?1 AND kind IN ('approved','draft')
                         ORDER BY (kind='approved') DESC, seq DESC LIMIT 1",
                        [&row.meeting_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if head_id.as_deref() == Some(id.as_str()) {
                    return self.version(id);
                }
                return Err(Error::StaleDraft(format!("{draft_id} is older than the current version of the meeting; approve the newest draft instead")));
            }
            // the head is the latest approved version, else the latest draft; an older draft
            // with other content would move it back
            let draft_seq: i64 = self.conn.query_row("SELECT seq FROM versions WHERE id=?1", [draft_id], |r| r.get(0))?;
            let head: Option<(i64, String)> = self
                .conn
                .query_row(
                    "SELECT seq, content_sha256 FROM versions WHERE meeting_id=?1 AND kind IN ('approved','draft')
                     ORDER BY (kind='approved') DESC, seq DESC LIMIT 1",
                    [&row.meeting_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((head_seq, head_content)) = head {
                if draft_seq < head_seq && head_content != content {
                    return Err(Error::StaleDraft(format!("{draft_id} is older than the current version of the meeting; approve the newest draft instead")));
                }
            }
            let mut new = raw.clone();
            {
                let v = new
                    .get_mut("version")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| Error::Corrupt(format!("version {draft_id} has no version object")))?;
                v.insert("id".into(), json!(self.next_version_id(&row.meeting_id, VersionKind::Approved)?));
                v.insert("kind".into(), json!("approved"));
                v.insert("parent".into(), json!(draft_id));
                v.insert("created_at".into(), json!(now_rfc3339()));
            }
            if content_sha256(&raw) != content_sha256(&new) {
                return Err(Error::Invalid("approve would change more than version.*".into()));
            }
            self.insert_version(&new)
        })
    }

    /// The draft made from `parent_id` earlier (`create_draft_from`), if there is one.
    pub fn draft_of(&self, parent_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM versions WHERE kind='draft' AND parent=?1 ORDER BY seq LIMIT 1", [parent_id], |r| r.get(0))
            .optional()?)
    }

    /// Whether the meeting has a draft or an approved version (anything a plugin result can be stale against).
    pub fn has_draft_or_approved(&self, meeting_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM versions WHERE meeting_id=?1 AND kind IN ('draft','approved')",
            [meeting_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Store `md` as the first draft of its meeting, derived from the `live` version `parent_id`
    /// (`version.parent` = the live version; `md.version.producers` say who made the content).
    ///
    /// Idempotent like `approve`: when a draft with this parent and the same content exists it is
    /// returned, nothing is stored. It is only ever the *first* draft: when the meeting already has
    /// another draft or an approved version (a batch transcription, an approval), the live captions
    /// would move the head back to rougher content, so it is refused with `StaleDraft`.
    pub fn create_draft_from(&self, parent_id: &str, mut md: MeetingData) -> Result<MeetingData> {
        self.immediate(|| {
            let parent = self.version_row(parent_id)?;
            if parent.kind != VersionKind::Live {
                return Err(Error::Invalid(format!("{parent_id} is {}, a draft is made from a live version", parent.kind.as_str())));
            }
            if parent.meeting_id != md.meeting.id {
                return Err(Error::Invalid(format!("{parent_id} belongs to another meeting")));
            }
            md.version.kind = VersionKind::Draft;
            md.version.parent = Some(parent_id.to_string());
            let content = content_sha256(&md.to_value());
            let same: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM versions WHERE meeting_id=?1 AND kind='draft' AND parent=?2 AND content_sha256=?3 ORDER BY seq LIMIT 1",
                    params![parent.meeting_id, parent_id, content],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = same {
                return self.version(&id);
            }
            if self.has_draft_or_approved(&parent.meeting_id)? {
                return Err(Error::StaleDraft(format!(
                    "the meeting already has a draft or an approved version; {parent_id} is not made a draft over it"
                )));
            }
            md.version.id = self.next_version_id(&md.meeting.id, VersionKind::Draft)?;
            md.version.created_at = Some(now_rfc3339());
            self.insert_version(&md.to_value())
        })
    }

    // ------------------------------------------------------------ plugins

    /// Register a plugin. Re-adding the same id@version is only accepted when manifest, code and
    /// directory are all unchanged; a changed plugin must bump its version.
    pub fn add_plugin(&self, manifest: &Manifest, dir: &Path, manifest_sha256: &str, code_sha256: &str) -> Result<PluginRow> {
        self.add_plugin_from(manifest, dir, manifest_sha256, code_sha256, None, None)
    }

    /// `add_plugin` plus where it came from, written in the same transaction as the plugin row
    /// (`None` = a local folder: any earlier source row is cleared, never left stale). Replacing a
    /// plugin whose recorded origin differs (another repo, or github vs local) is refused with
    /// `PluginSourceChanged` unless `replace` is set; a new commit of the same repo is an update.
    pub fn add_plugin_from(
        &self,
        manifest: &Manifest,
        dir: &Path,
        manifest_sha256: &str,
        code_sha256: &str,
        source: Option<&crate::plugin_source::PluginSource>,
        replace: Option<&str>,
    ) -> Result<PluginRow> {
        manifest.require_plugin()?;
        let dir = dunce::canonicalize(dir)?;
        let p = &manifest.package;
        self.immediate(|| {
            let old: Option<(String, String, String, String)> = self
                .conn
                .query_row("SELECT version, dir, manifest_sha256, code_sha256 FROM plugins WHERE id=?1", [&p.id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            if let Some((ver, odir, om, oc)) = old {
                if let Some((over, before)) = self.replaced_origin(&p.id, source)? {
                    // `replace` is the key of the installed state the user was shown; if the
                    // installed plugin is not that any more, the confirmation does not apply
                    if replace != Some(replace_key(&over, before.as_ref()).as_str()) {
                        return Err(source_conflict(&p.id, before.as_ref(), source));
                    }
                }
                if ver == p.version && (odir != dir.to_string_lossy() || om != manifest_sha256 || oc != code_sha256) {
                    return Err(Error::Plugin(format!(
                        "{}@{} is already added with a different directory, manifest or code; bump its version to add the change",
                        p.id, p.version
                    )));
                }
            }
            self.conn.execute(
                "INSERT INTO plugins (id, version, dir, manifest_json, manifest_sha256, code_sha256, network, audio, added_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(id) DO UPDATE SET version=excluded.version, dir=excluded.dir, manifest_json=excluded.manifest_json,
                     manifest_sha256=excluded.manifest_sha256, code_sha256=excluded.code_sha256,
                     network=excluded.network, audio=excluded.audio, added_at=excluded.added_at",
                params![
                    p.id,
                    p.version,
                    dir.to_string_lossy(),
                    serde_json::to_string(manifest)?,
                    manifest_sha256,
                    code_sha256,
                    manifest.permissions.network,
                    manifest.input.audio,
                    now_rfc3339()
                ],
            )?;
            self.write_source(&p.id, source)?;
            Ok(PluginRow {
                id: p.id.clone(),
                version: p.version.clone(),
                dir: dir.clone(),
                network: manifest.permissions.network,
                audio: manifest.input.audio.clone(),
                manifest_sha256: manifest_sha256.to_string(),
                code_sha256: code_sha256.to_string(),
            })
        })
    }

    /// `Some((installed version, its recorded source))` when `id` is installed from a different
    /// origin than `new` (another repo, or GitHub vs a local folder); `None` for a fresh id, a
    /// local-over-local install, or a new commit of the same repo.
    pub fn replaced_origin(&self, id: &str, new: Option<&crate::plugin_source::PluginSource>) -> Result<Option<(String, Option<crate::plugin_source::PluginSource>)>> {
        let version: Option<String> = self.conn.query_row("SELECT version FROM plugins WHERE id=?1", [id], |r| r.get(0)).optional()?;
        let Some(version) = version else { return Ok(None) };
        let before = self.plugin_source(id)?;
        let same = match (&before, new) {
            (None, None) => true,
            (Some(a), Some(b)) => a.repo.eq_ignore_ascii_case(&b.repo),
            _ => false,
        };
        Ok((!same).then_some((version, before)))
    }

    /// `replaced_origin`'s answer as the string a confirmation carries (see `replace_key`).
    pub fn replace_key_for(&self, id: &str, new: Option<&crate::plugin_source::PluginSource>) -> Result<Option<String>> {
        Ok(self.replaced_origin(id, new)?.map(|(v, s)| replace_key(&v, s.as_ref())))
    }

    fn plugin_row(r: &rusqlite::Row) -> rusqlite::Result<PluginRow> {
        Ok(PluginRow {
            id: r.get(0)?,
            version: r.get(1)?,
            dir: PathBuf::from(r.get::<_, String>(2)?),
            network: r.get(3)?,
            audio: r.get(4)?,
            manifest_sha256: r.get(5)?,
            code_sha256: r.get(6)?,
        })
    }

    pub fn plugin(&self, id: &str) -> Result<(PluginRow, Manifest)> {
        if agent::is_agent_id(id) {
            return Err(Error::NotFound(format!("plugin {id}: this id holds results from an AI agent (MCP), it is not a plugin")));
        }
        let (row, json): (PluginRow, String) = self
            .conn
            .query_row(
                "SELECT id, version, dir, network, audio, manifest_sha256, code_sha256, manifest_json FROM plugins WHERE id=?1",
                [id],
                |r| Ok((Self::plugin_row(r)?, r.get(7)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("plugin {id}")))?;
        let m = serde_json::from_str(&json).map_err(|e| Error::Corrupt(format!("manifest of plugin {id}: {e}")))?;
        Ok((row, m))
    }

    pub fn list_plugins(&self) -> Result<Vec<PluginRow>> {
        let mut st = self.conn.prepare("SELECT id, version, dir, network, audio, manifest_sha256, code_sha256 FROM plugins WHERE dir <> '' AND lower(substr(id, 1, 4)) <> 'mcp:' ORDER BY id")?;
        let rows = st.query_map([], Self::plugin_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ------------------------------------------------------------ results

    /// Bind a plugin's output to the version it read. The output is checked again here
    /// against that stored version, so a caller cannot attach an unvalidated result.
    pub fn add_result(&self, plugin: &PluginRow, version_id: &str, output: &Value) -> Result<i64> {
        self.add_result_sandboxed(plugin, version_id, output, "unrecorded")
    }

    /// `add_result`, recording the isolation the run actually had (`sandbox::MACOS_SBPL`, `NONE`, `OFF`).
    pub fn add_result_sandboxed(&self, plugin: &PluginRow, version_id: &str, output: &Value, sandbox: &str) -> Result<i64> {
        let v = self.version_row(version_id)?;
        let md = self.version(version_id)?;
        let errs = output_errors(&md, output);
        if !errs.is_empty() {
            return Err(Error::Plugin(format!("rejected output: {}", errs.into_iter().take(10).collect::<Vec<_>>().join("; "))));
        }
        if !is_sha256(&plugin.manifest_sha256) || !is_sha256(&plugin.code_sha256) {
            return Err(Error::Invalid("plugin hashes are missing".into()));
        }
        self.conn.execute(
            "INSERT INTO plugin_results (plugin_id, plugin_version, version_id, meeting_id, output_json, created_at,
                 plugin_manifest_sha256, plugin_code_sha256, declared_network, declared_audio, sandbox)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                plugin.id,
                plugin.version,
                version_id,
                v.meeting_id,
                serde_json::to_string(output)?,
                now_rfc3339(),
                plugin.manifest_sha256,
                plugin.code_sha256,
                plugin.network,
                plugin.audio,
                sandbox
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// stale = the meeting's head (latest approved, else latest draft) holds different
    /// content than the version the result read. Re-approving identical content is not a change.
    const RESULT_SELECT: &'static str = "
        SELECT r.id, r.plugin_id, r.plugin_version, r.version_id, r.created_at, r.output_json, r.plugin_code_sha256,
               r.plugin_manifest_sha256, r.declared_network, r.declared_audio, r.sandbox,
               COALESCE(
                 (SELECT h.content_sha256 FROM versions h WHERE h.meeting_id = r.meeting_id AND h.kind = 'approved' ORDER BY h.seq DESC LIMIT 1),
                 (SELECT h.content_sha256 FROM versions h WHERE h.meeting_id = r.meeting_id AND h.kind = 'draft' ORDER BY h.seq DESC LIMIT 1),
                 v.content_sha256) <> v.content_sha256,
               ap.json
        FROM plugin_results r JOIN versions v ON v.id = r.version_id
        LEFT JOIN agent_provenance ap ON r.sandbox = 'mcp' AND ap.sha256 = r.plugin_manifest_sha256";

    #[allow(clippy::type_complexity)]
    fn results(&self, where_: &str, arg: &str) -> Result<Vec<ResultRow>> {
        let sql = format!("{} WHERE {where_} ORDER BY r.id", Self::RESULT_SELECT);
        let mut st = self.conn.prepare(&sql)?;
        let rows: Vec<(i64, String, String, String, String, String, String, String, bool, String, String, bool, Option<String>)> = st
            .query_map([arg], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows.into_iter()
            .map(|(id, plugin_id, plugin_version, version_id, created_at, out, code, manifest, net, audio, sandbox, stale, agent)| {
                Ok(ResultRow {
                    id,
                    plugin_id,
                    plugin_version,
                    version_id,
                    created_at,
                    stale,
                    output: serde_json::from_str(&out).map_err(|e| Error::Corrupt(format!("output of result #{id}: {e}")))?,
                    plugin_code_sha256: code,
                    plugin_manifest_sha256: manifest,
                    declared_network: net,
                    declared_audio: audio,
                    sandbox,
                    agent: agent.and_then(|a| serde_json::from_str(&a).ok()),
                })
            })
            .collect()
    }

    /// Results bound to one version, with their stale flag.
    pub fn results_for_version(&self, version_id: &str) -> Result<Vec<ResultRow>> {
        self.results("r.version_id = ?1", version_id)
    }

    /// Results of every version of a meeting.
    pub fn results_for_meeting(&self, meeting_id: &str) -> Result<Vec<ResultRow>> {
        self.results("r.meeting_id = ?1", meeting_id)
    }

    /// Only the stale ones, across a meeting.
    pub fn stale_results(&self, meeting_id: &str) -> Result<Vec<ResultRow>> {
        Ok(self.results_for_meeting(meeting_id)?.into_iter().filter(|r| r.stale).collect())
    }
}

pub mod agent;
pub mod overlay;
pub mod search;
mod sources;
#[cfg(test)]
mod tests;
