//! `meetko`: import, transcribe, approve, run plugins, show, export.
//!
//! stdout carries ids and requested content (so scripts can use it); progress goes to stderr.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use meetko_core::contract::MeetingData;
use meetko_core::diarize::{DiarConfig, DiarProcess};
use meetko_core::manifest::{plugin_code_sha256, Manifest};
use meetko_core::plugin_source::{self, Endpoints, PluginSource, SourceSpec};
use meetko_core::store::{ImportChannel, ImportOptions, ResultRow, Store};
use meetko_core::whisper::WhisperCli;
use meetko_core::{capture, export, host, pipeline, util};

// the Korean-first channel names live here, not in core
const MIC_LABEL: &str = "나";
const LOOPBACK_LABEL: &str = "상대";

/// Shown at `plugin add`, `plugin list` and in --help. Plain on purpose.
const PERMISSION_NOTICE: &str = "on macOS a plugin runs in an OS sandbox: no network unless it declares it, no reading your home or the meetko data dir, \
writes only to a private temp dir. Other systems have no sandbox yet (the plugin runs as your user); audio is never passed \
to a plugin in 0.1. Add only plugins you trust.";

#[derive(Parser)]
#[command(name = "meetko", version, about = "local-first Korean-English meeting transcripts")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// copy and hash channel files and create a meeting; prints the meeting id.
    /// Importing a recording that was imported before returns the existing meeting.
    Import {
        #[arg(long, requires = "loopback", conflicts_with = "capture")]
        mic: Option<PathBuf>,
        #[arg(long, requires = "mic", conflicts_with = "capture")]
        loopback: Option<PathBuf>,
        /// a capture directory or its capture.json (from a capture provider)
        #[arg(long)]
        capture: Option<PathBuf>,
        /// import a capture whose manifest says complete: false
        #[arg(long, requires = "capture")]
        allow_incomplete: bool,
        #[arg(long)]
        title: Option<String>,
        /// spoken language hint (whisper code), default ko
        #[arg(long)]
        language: Option<String>,
    },
    /// run whisper on each channel and store a draft version; prints the version id.
    /// With MEETKO_DIAR_MODEL set (and the engine found: MEETKO_DIAR_ENGINE), each channel is also
    /// split into its speakers ("상대 1", "상대 2"); without it every channel is one speaker.
    Transcribe {
        meeting_id: String,
        #[arg(long)]
        language: Option<String>,
    },
    /// store a new approved version with the draft's content; prints the version id
    Approve { version_id: String },
    /// manage plugins. NOTE: macOS runs plugins in a sandbox, other systems do not (see `plugin add --help`)
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// run a plugin on a version and store the result
    Run {
        plugin_id: String,
        version_id: String,
        /// per-request time budget in seconds (env MEETKO_PLUGIN_TIMEOUT_SECS)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// print turns with speaker and mm:ss
    Show { version_id: String },
    Export {
        version_id: String,
        #[arg(long, value_enum)]
        format: Format,
        /// channel for ctm, rttm and text
        #[arg(long, default_value_t = 0)]
        channel: u32,
    },
    /// plugin results bound to a version, with their stale flag
    Results {
        version_id: String,
        /// every result of the version's meeting
        #[arg(long)]
        all: bool,
        /// print the stored plugin output too
        #[arg(long)]
        json: bool,
    },
    /// meetings and their versions
    List,
    /// find text in each meeting's current version (approved, else draft, else live captions).
    /// The query is literal and case-insensitive; prints one line per meeting, then its hits as
    /// `  mm:ss  <word id>..<word id>  …context…`.
    Search { query: String },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// register the plugin in a directory that holds meetko.toml. It shows the declared
    /// permissions first and needs --yes (or a y at the prompt). On macOS the declared network
    /// permission is enforced by a sandbox (see docs/plugin-security.md); on other systems a
    /// plugin can still use the network and read your files. Code and manifest hashes are recorded; a changed plugin will not run
    /// until it is added again.
    Add {
        dir: PathBuf,
        #[arg(long)]
        yes: bool,
        /// replace an installed plugin with the same id that came from a different source
        #[arg(long)]
        replace: bool,
    },
    /// install from a source: `github:OWNER/REPO@REF` (REF = tag or full commit sha) or a local
    /// folder (same as `add`). A tag is resolved to a commit sha and the install is pinned to it.
    /// The network is used only by this command. Nothing is built: the plugin must run as checked
    /// out. Same consent, permissions notice and hashes as `add`.
    Install {
        spec: String,
        #[arg(long)]
        yes: bool,
        /// replace an installed plugin with the same id that came from a different source
        /// (another repository, or a local folder instead of GitHub)
        #[arg(long)]
        replace: bool,
    },
    /// fetch `github:OWNER/REPO@REF` and print its manifest and permissions; installs nothing
    Info { spec: String },
    /// list plugins with their declared permissions
    List,
}

#[derive(Clone, ValueEnum)]
enum Format {
    Webvtt,
    Ctm,
    Rttm,
    Text,
}

fn mmss(ms: i64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn print_show(md: &MeetingData) -> Result<()> {
    println!(
        "{}  {} ({})  {}",
        md.meeting.title.as_deref().unwrap_or(&md.meeting.id),
        md.version.id,
        md.version.kind.as_str(),
        mmss(md.recording.duration_ms)
    );
    let pos: std::collections::HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
    for t in &md.turns {
        let who = t
            .speaker
            .as_deref()
            .and_then(|s| md.speakers.iter().find(|x| x.id == s))
            .map(|s| s.label.as_deref().unwrap_or(&s.id))
            .unwrap_or("?");
        let text: Vec<&str> = export::turn_words(md, &pos, t)?.iter().map(|w| w.text.as_str()).collect();
        println!("[{}] {}: {}", mmss(t.start_ms), who, text.join(" "));
    }
    Ok(())
}

fn print_result(r: &ResultRow, json: bool) {
    let kinds: Vec<String> = r.output["items"]
        .as_array()
        .map(|a| a.iter().map(|i| i["type"].as_str().unwrap_or("?").to_string()).collect())
        .unwrap_or_default();
    println!(
        "#{} {}@{} on {} {} [{}] sandbox: {}",
        r.id,
        r.plugin_id,
        r.plugin_version,
        r.version_id,
        if r.stale { "STALE" } else { "current" },
        kinds.join(", "),
        r.sandbox
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&r.output).unwrap_or_default());
    }
}

/// Text from a manifest or a remote, made safe for a terminal: control and invisible formatting
/// characters become `\u{..}` (the manifest parser already refuses them; this is the second layer).
fn esc(s: &str) -> String {
    meetko_core::text::escape(s, false)
}

/// Every message that can echo remote or plugin-supplied text goes through here: multi-line messages
/// keep their line breaks, nothing else hidden reaches the terminal.
fn esc_msg(s: &str) -> String {
    meetko_core::text::escape(s, true)
}

/// Print what the user is asked to approve; returns the code hash that was shown and the plugin id.
fn show_plugin(dir: &std::path::Path, source: Option<&PluginSource>) -> Result<(String, String)> {
    let (manifest, _) = Manifest::load(dir)?;
    manifest.require_plugin()?;
    let code_sha = plugin_code_sha256(&manifest, dir)?;
    let p = &manifest.package;
    eprintln!("{}@{}  {}", esc(&p.id), esc(&p.version), esc(&p.description));
    if let Some(s) = source {
        eprintln!("  source:           {} @ {} (commit {})", esc(&s.repo), esc(&s.reference), esc(&s.sha));
    }
    eprintln!("  declared network: {}", manifest.permissions.network);
    eprintln!("  declared audio:   {}", manifest.input.audio);
    eprintln!("  code sha256:      {code_sha}");
    Ok((code_sha, p.id.clone()))
}

/// Show, ask, then install through the verified-copy path. `fetched` is a tree from a source spec.
fn install(store: &Store, dir: &std::path::Path, fetched: Option<&plugin_source::Fetched>, yes: bool, replace: bool) -> Result<()> {
    let source = fetched.map(|f| &f.source);
    let (code_sha, id) = show_plugin(dir, source)?;
    // what an install would replace, shown now; --replace confirms exactly this installed state
    let shown = store.replaced_origin(&id, source)?;
    if let Some((v, old)) = &shown {
        let from = old.as_ref().map(|s| esc(&s.repo)).unwrap_or_else(|| "a local folder".into());
        eprintln!("  REPLACES:         {}@{} installed from {from}", esc(&id), esc(v));
    }
    let replace_key = shown.filter(|_| replace).map(|(v, old)| meetko_core::store::replace_key(&v, old.as_ref()));
    eprintln!("note: {PERMISSION_NOTICE}");
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("not a terminal: pass --yes to add this plugin");
        }
        eprint!("add this plugin? [y/N] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            anyhow::bail!("not added");
        }
    }
    // runs from a verified copy under the data dir, so the directory shown here cannot be swapped later
    let row = match fetched {
        Some(f) => store.install_fetched(f, Some(&code_sha), replace_key.as_deref())?,
        None => store.install_plugin_from(dir, Some(&code_sha), None, replace_key.as_deref())?,
    };
    eprintln!("added {}@{}", row.id, row.version);
    println!("{}", row.id);
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // anyhow's own report would print the raw chain: a toml parse error echoes the source line
            eprintln!("Error: {}", esc_msg(&format!("{e:#}")));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::open(&util::data_dir()).context("cannot open the data dir")?;
    for w in store.warnings() {
        eprintln!("warning: {}", esc_msg(w));
    }
    match cli.cmd {
        Cmd::Import { mic, loopback, capture: cap, allow_incomplete, title, language } => {
            let (mut channels, mut options) = match (cap, mic, loopback) {
                (Some(path), ..) => {
                    let c = capture::load(&path, allow_incomplete)?;
                    for w in &c.warnings {
                        eprintln!("warning: {}", esc_msg(w));
                    }
                    (c.channels, c.options)
                }
                (None, Some(mic), Some(loopback)) => {
                    let ch = |index, role: &str, path| ImportChannel {
                        index,
                        role: role.into(),
                        label: None,
                        path,
                        expected_sha256: None,
                        coverage: None,
                    };
                    (vec![ch(0, "mic", mic), ch(1, "loopback", loopback)], ImportOptions::default())
                }
                _ => anyhow::bail!("give --capture <dir|capture.json>, or both --mic and --loopback"),
            };
            for c in &mut channels {
                c.label = Some(if c.role == "mic" { MIC_LABEL } else { LOOPBACK_LABEL }.to_string());
            }
            options.title = title;
            options.language_hint = language;
            let out = store.import_meeting(channels, options)?;
            if out.existing {
                eprintln!("this recording was imported before: using the existing meeting");
            }
            eprintln!("meeting {} ({})", out.meeting.id, mmss(out.meeting.duration_ms));
            println!("{}", out.meeting.id);
        }
        Cmd::Transcribe { meeting_id, language } => {
            let engine = WhisperCli::load(store.dir())?;
            let cancel = Arc::new(AtomicBool::new(false));
            let c2 = cancel.clone();
            let _ = ctrlc::set_handler(move || c2.store(true, Ordering::SeqCst));
            // diarization is optional and never costs the transcript: a model that is set but unusable
            // only means one speaker per channel, and says why
            let diar = match DiarConfig::from_env() {
                Ok(c) => c.map(DiarProcess::new),
                Err(e) => {
                    eprintln!("warning: speaker separation is off: {}", esc_msg(&e.to_string()));
                    None
                }
            };
            eprintln!("transcribing {meeting_id} ({})", if diar.is_some() { "speakers separated per channel" } else { "each channel is one speaker" });
            let last = std::cell::Cell::new(255u8);
            let progress = |p: meetko_core::whisper::Progress| {
                if p.percent / 10 != last.get() / 10 || last.get() == 255 {
                    eprintln!("  channel {}/{}: {}%", p.channel + 1, p.channels, p.percent);
                }
                last.set(p.percent);
            };
            let done = pipeline::transcribe_with(&store, &engine, diar.as_ref().map(|d| d as &dyn meetko_core::diarize::DiarEngine), &meeting_id, language.as_deref(), &progress, &cancel)?;
            for w in &done.warnings {
                eprintln!("warning: {}", esc_msg(w));
            }
            let md = done.data;
            eprintln!("{} words, {} turns, {} speakers", md.words.len(), md.turns.len(), md.speakers.len());
            println!("{}", md.version.id);
        }
        Cmd::Approve { version_id } => {
            let md = store.approve(&version_id).map_err(|e| match e {
                meetko_core::Error::StaleDraft(_) => anyhow::anyhow!("{e}\n이 초안은 현재 버전보다 오래되어 승인할 수 없습니다. 가장 최근 초안을 승인하세요."),
                e => e.into(),
            })?;
            println!("{}", md.version.id);
        }
        Cmd::Plugin(PluginCmd::Add { dir, yes, replace }) => install(&store, &dir, None, yes, replace)?,
        Cmd::Plugin(PluginCmd::Install { spec, yes, replace }) => match SourceSpec::parse(&spec)? {
            None => install(&store, &PathBuf::from(&spec), None, yes, replace)?,
            Some(sp) => {
                eprintln!("fetching {}@{} ...", sp.repo_id(), sp.reference);
                let f = plugin_source::fetch(&Endpoints::from_env()?, &sp)?;
                install(&store, &f.dir.clone(), Some(&f), yes, replace)?
            }
        },
        Cmd::Plugin(PluginCmd::Info { spec }) => {
            let sp = SourceSpec::parse(&spec)?.context("info takes github:OWNER/REPO@REF")?;
            let f = plugin_source::fetch(&Endpoints::from_env()?, &sp)?;
            let _ = show_plugin(&f.dir, Some(&f.source))?;
            eprintln!("note: {PERMISSION_NOTICE}");
        }
        Cmd::Plugin(PluginCmd::List) => {
            eprintln!("note: {PERMISSION_NOTICE}");
            for p in store.list_plugins()? {
                let how = if cfg!(target_os = "macos") { "sandbox: macos-sbpl" } else { "not enforced on this OS" };
                let from = store.plugin_source(&p.id)?.map(|s| format!("  from {}@{} ({})", s.repo, &s.sha[..12], s.reference)).unwrap_or_default();
                println!("{}@{}  declared network={} audio={} ({how}){from}", p.id, p.version, p.network, p.audio);
            }
        }
        Cmd::Run { plugin_id, version_id, timeout } => {
            let secs = timeout
                .or_else(|| std::env::var("MEETKO_PLUGIN_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()))
                .unwrap_or(host::DEFAULT_TIMEOUT.as_secs());
            let id = pipeline::run_plugin(&store, &plugin_id, &version_id, Duration::from_secs(secs))?;
            if let Some(r) = store.results_for_version(&version_id)?.into_iter().find(|r| r.id == id) {
                print_result(&r, false);
            }
        }
        Cmd::Show { version_id } => print_show(&store.version(&version_id)?)?,
        Cmd::Export { version_id, format, channel } => {
            let md = store.version(&version_id)?;
            let out = match format {
                Format::Webvtt => export::webvtt(&md)?,
                Format::Ctm => export::ctm(&md, &md.meeting.id, channel),
                Format::Rttm => export::rttm(&md, &md.meeting.id, channel),
                Format::Text => export::text(&md, channel) + "\n",
            };
            print!("{out}");
        }
        Cmd::Results { version_id, all, json } => {
            let v = store.version_row(&version_id)?;
            let rows = if all { store.results_for_meeting(&v.meeting_id)? } else { store.results_for_version(&version_id)? };
            if rows.is_empty() {
                eprintln!("no results");
            }
            for r in &rows {
                print_result(r, json);
            }
        }
        Cmd::List => {
            for m in store.list_meetings()? {
                println!("{}  {}  {}", m.id, mmss(m.duration_ms), m.title.as_deref().unwrap_or(""));
                for v in store.versions_of(&m.id)? {
                    println!("  {}  {}{}", v.id, v.kind.as_str(), v.parent.map(|p| format!("  parent={p}")).unwrap_or_default());
                }
            }
        }
        Cmd::Search { query } => {
            // a command line can wait: index every meeting not indexed yet, so the answer is complete
            if store.index_pending(Some(std::time::Duration::ZERO))? > 0 {
                eprintln!("indexing meetings for search…");
                store.index_pending(None)?;
            }
            let found = store.search(&query)?;
            if found.meetings.is_empty() {
                eprintln!("no matches");
            }
            if found.more {
                eprintln!("showing the newest {} meetings; more matched", found.meetings.len());
            }
            for m in &found.meetings {
                println!("{}  {} ({})  {}  {} match(es)", m.meeting_id, m.version_id, m.kind, esc(m.title.as_deref().unwrap_or("")), m.matches);
                for h in &m.hits {
                    println!("  {}  {}..{}  {}[{}]{}", mmss(h.start_ms), esc(&h.from), esc(&h.to), esc(&h.before), esc(&h.text), esc(&h.after));
                }
            }
        }
    }
    Ok(())
}
