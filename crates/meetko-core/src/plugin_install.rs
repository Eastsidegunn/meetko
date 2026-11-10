//! Installing a plugin = copying it into the data dir and running only the copy.
//!
//! Hashing the directory the user picked and running that same directory leaves a window
//! (time of check to time of use) in which a writer swaps files. Here the verified bytes are
//! the ones that run: the directory is copied first, and the manifest, the code hash and the
//! stored path all come from the copy. The per-run hash recheck (`pipeline::run_plugin`) stays.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::plugin_source::PluginSource;
use crate::manifest::{command_files, plugin_code_sha256, Manifest};
use crate::store::{PluginRow, Store};
use crate::util;

/// the same exclusions as the code hash; a compiled plugin's executable is copied by name
fn skipped(name: &str) -> bool {
    matches!(name, "target" | ".git" | "__pycache__")
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    util::create_private_dir(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if skipped(&name.to_string_lossy()) {
            continue;
        }
        let ft = e.file_type()?;
        if ft.is_symlink() {
            // the hash covers a link's text, not what it points at, so a link could hide changes
            return Err(Error::Plugin(format!("{}: symlinks are not allowed in a plugin", name.to_string_lossy())));
        }
        let to = dst.join(&name);
        if ft.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?; // keeps the executable bit
        }
    }
    Ok(())
}

/// `<id>@<version>-<code sha prefix>`, with anything that is not a plain name character replaced.
fn dir_name(id: &str, version: &str, code_sha: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect::<String>();
    format!("{}@{}-{}", clean(id), clean(version), &code_sha[..12])
}

impl Store {
    /// Copy `src` under `<data>/plugins/`, verify the copy, register it. With `expected_code_sha256`
    /// (the hash shown to the user), a source that differs is refused with `PluginChanged`.
    pub fn install_plugin(&self, src: &Path, expected_code_sha256: Option<&str>) -> Result<PluginRow> {
        self.install_plugin_from(src, expected_code_sha256, None, None)
    }

    /// `install_plugin` with a recorded source and the replace switch (see `add_plugin_from`).
    /// For a GitHub source the id's publisher part must be the repo owner (case-insensitive), so
    /// `github:anyone/x` cannot ship `meetko/summary`.
    pub fn install_plugin_from(&self, src: &Path, expected_code_sha256: Option<&str>, source: Option<&PluginSource>, replace: Option<&str>) -> Result<PluginRow> {
        let root = self.dir().join("plugins");
        util::create_private_dir(&root)?;
        let tmp = tempfile::Builder::new().prefix(".install-").tempdir_in(&root)?;
        let staged = tmp.path().join("p");
        copy_tree(src, &staged)?;
        // the manifest is read from the copy, then the files it names (a compiled executable under
        // target/) are copied too; everything below hashes the copy
        let (manifest, _) = Manifest::load(&staged)?;
        manifest.require_plugin()?;
        for f in command_files(&manifest, &staged)? {
            let rel = f.strip_prefix(&staged).map_err(|_| Error::Plugin("command file outside the plugin".into()))?;
            let from = src.join(rel);
            if f.exists() || !from.is_file() {
                continue;
            }
            if std::fs::symlink_metadata(&from)?.file_type().is_symlink() {
                return Err(Error::Plugin(format!("{}: symlinks are not allowed in a plugin", rel.display())));
            }
            if let Some(parent) = f.parent() {
                util::create_private_dir(parent)?;
            }
            std::fs::copy(&from, &f)?;
        }
        let (manifest, manifest_sha) = Manifest::load(&staged)?;
        if let Some(s) = source {
            check_publisher(&manifest.package.id, &s.repo)?;
        }
        let code_sha = plugin_code_sha256(&manifest, &staged)?;
        if let Some(want) = expected_code_sha256 {
            if want != code_sha {
                return Err(Error::PluginChanged("the plugin changed since it was shown; review it again".into()));
            }
        }
        let p = &manifest.package;
        let target: PathBuf = root.join(dir_name(&p.id, &p.version, &code_sha));
        // refuse before anything is moved into place (add_plugin_from checks again, in its transaction)
        if let Some((over, before)) = self.replaced_origin(&p.id, source)? {
            if replace != Some(crate::store::replace_key(&over, before.as_ref()).as_str()) {
                return Err(crate::store::source_conflict(&p.id, before.as_ref(), source));
            }
        }
        let mut created = false;
        if target.exists() {
            // the same bytes were installed before; use that copy, but only if it is still intact
            let (m, msha) = Manifest::load(&target)?;
            if msha != manifest_sha || plugin_code_sha256(&m, &target)? != code_sha {
                return Err(Error::Corrupt(format!("the installed copy {} no longer matches its hash", target.display())));
            }
        } else {
            std::fs::rename(&staged, &target)?;
            created = true;
        }
        let row = self.add_plugin_from(&manifest, &target, &manifest_sha, &code_sha, source, replace);
        if row.is_err() && created {
            let _ = std::fs::remove_dir_all(&target); // do not leave a copy nothing refers to
        }
        row
    }
}

impl Store {
    /// Install a tree fetched from a source spec through the same verified-copy path as a local
    /// folder; the plugin row and its source row are written in one transaction.
    pub fn install_fetched(&self, f: &crate::plugin_source::Fetched, expected_code_sha256: Option<&str>, replace: Option<&str>) -> Result<PluginRow> {
        self.install_plugin_from(&f.dir, expected_code_sha256, Some(&f.source), replace)
    }
}

/// Ids are `publisher/name`. A plugin fetched from `github:OWNER/REPO` may only use its owner as
/// publisher (GitHub names are case-insensitive; ids are lowercase), so a repo cannot take over
/// another publisher's id.
fn check_publisher(id: &str, repo: &str) -> Result<()> {
    let owner = repo.strip_prefix("github:").unwrap_or(repo).split('/').next().unwrap_or("");
    if !crate::plugin_source::valid_plugin_id(id) && id.contains('/') {
        return Err(Error::Plugin(format!("the plugin id {id:?} is not a valid id (publisher/name: lowercase letters, digits, . _ -)")));
    }
    match id.split_once('/') {
        Some((publisher, _)) if publisher.eq_ignore_ascii_case(owner) => Ok(()),
        Some((publisher, _)) => Err(Error::Plugin(format!("the plugin id {id:?} belongs to publisher {publisher:?}, but it comes from github.com/{owner}; the publisher part of the id must be the repository owner"))),
        None => Err(Error::Plugin(format!("the plugin id {id:?} has no publisher part; ids installed from GitHub must look like {}/name", owner.to_ascii_lowercase()))),
    }
}
