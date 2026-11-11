//! Where an installed plugin came from (migration 3). Kept apart from the plugins table so the
//! original schema is untouched: a plugin added from a local folder simply has no row here.
//! `repo` is `github:OWNER/REPO`, or `github:OWNER/REPO/FOLDER` for a plugin in a subfolder.

use rusqlite::{params, OptionalExtension};

use super::Store;
use crate::error::Result;
use crate::plugin_source::PluginSource;
use crate::util::now_rfc3339;

impl Store {
    /// Remember the source of `plugin_id` (replaces an earlier one: the last install wins).
    /// Installs go through `add_plugin_from`, which writes this in the plugin row's transaction.
    pub fn set_plugin_source(&self, plugin_id: &str, s: &PluginSource) -> Result<()> {
        self.write_source(plugin_id, Some(s))
    }

    /// Insert or clear (`None`: a local install) the source row. No transaction of its own.
    pub(super) fn write_source(&self, plugin_id: &str, s: Option<&PluginSource>) -> Result<()> {
        let Some(s) = s else {
            self.conn.execute("DELETE FROM plugin_sources WHERE plugin_id=?1", [plugin_id])?;
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO plugin_sources (plugin_id, repo, sha, ref, installed_at) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(plugin_id) DO UPDATE SET repo=excluded.repo, sha=excluded.sha, ref=excluded.ref, installed_at=excluded.installed_at",
            params![plugin_id, s.repo, s.sha, s.reference, now_rfc3339()],
        )?;
        Ok(())
    }

    /// `None` for a plugin added from a local folder.
    pub fn plugin_source(&self, plugin_id: &str) -> Result<Option<PluginSource>> {
        Ok(self
            .conn
            .query_row("SELECT repo, sha, ref FROM plugin_sources WHERE plugin_id=?1", [plugin_id], |r| {
                Ok(PluginSource { repo: r.get(0)?, sha: r.get(1)?, reference: r.get(2)? })
            })
            .optional()?)
    }
}
