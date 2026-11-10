//! Notice that a meeting has started and offer to record it (and, once the meeting app lets go of
//! the microphone, offer to stop). The app never starts or stops a recording by itself.
//!
//! Privacy: the only signal is "which processes are reading audio input right now", taken from
//! CoreAudio's process objects (macOS 14+), matched against a fixed list of bundle ids below.
//! Nothing reads window titles, browser tabs, URLs or the screen, and no Accessibility or Screen
//! Recording permission is involved. A browser counts only while the browser itself (or its
//! helper process) is using the microphone.
//!
//! The decision logic (`Watch`) is a plain state machine over a millisecond clock, so it is tested
//! without any device. `mic_users` is the only platform code; elsewhere it reports "unavailable".

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often the microphone is looked at. One CoreAudio query per poll; no loop in between.
pub const POLL: Duration = Duration::from_secs(3);
/// The mic must be in use this long before we ask (a short sound check does not count).
pub const CONFIRM_MS: u64 = 6_000;
/// Without mic use for this long the meeting counts as over (mute in some apps releases the mic briefly).
pub const GRACE_MS: u64 = 30_000;
/// After "이번엔 안 함" nothing is offered for this long; a meeting still on when it ends is offered then.
pub const COOLDOWN_MS: u64 = 10 * 60_000;

/// A known meeting app. `ids` match a bundle id exactly or as a prefix followed by `.`, so a
/// helper process (`com.google.Chrome.helper`) counts as its app.
#[derive(Debug, PartialEq, Eq)]
pub struct App {
    pub name: &'static str,
    ids: &'static [&'static str],
    /// web meetings: the browser is using the mic, which might also be something else
    pub browser: bool,
}

const fn app(name: &'static str, ids: &'static [&'static str]) -> App {
    App { name, ids, browser: false }
}
const fn browser(name: &'static str, ids: &'static [&'static str]) -> App {
    App { name, ids, browser: true }
}

/// Native apps first: when both a meeting app and a browser use the mic, the app is named.
pub static APPS: &[App] = &[
    app("Zoom", &["us.zoom.xos"]),
    app("Microsoft Teams", &["com.microsoft.teams2", "com.microsoft.teams"]),
    app("Slack", &["com.tinyspeck.slackmacgap"]),
    app("Webex", &["Cisco-Systems.Spark", "com.webex.meetingmanager", "com.cisco.webexmeetingsapp"]),
    app("FaceTime", &["com.apple.FaceTime", "com.apple.avconferenced"]),
    app("Discord", &["com.hnc.Discord"]),
    app("WhatsApp", &["net.whatsapp.WhatsApp", "desktop.WhatsApp"]),
    app("Jitsi Meet", &["org.jitsi.jitsi-meet"]),
    browser("Chrome", &["com.google.Chrome"]),
    browser("Edge", &["com.microsoft.edgemac"]),
    browser("Arc", &["company.thebrowser.Browser"]),
    browser("Brave", &["com.brave.Browser"]),
    browser("Firefox", &["org.mozilla.firefox"]),
    browser("Vivaldi", &["com.vivaldi.Vivaldi"]),
    browser("Opera", &["com.operasoftware.Opera"]),
    // Safari captures in the shared WebKit GPU process, which any WebKit-based app may use
    browser("Safari", &["com.apple.Safari", "com.apple.WebKit.GPU"]),
];

/// Our own bundle ids (lower case): meetko never counts as a meeting app.
const OWN_PREFIX: &str = "dev.meetko.";

pub fn classify(bundle_id: &str) -> Option<&'static App> {
    // ignoring case: a helper is not always spelled like its app
    let b = bundle_id.to_ascii_lowercase();
    let hit = |id: &str| {
        let id = id.to_ascii_lowercase();
        b == id || b.strip_prefix(id.as_str()).is_some_and(|rest| rest.starts_with('.'))
    };
    APPS.iter().find(|a| a.ids.iter().any(|id| hit(id)))
}

/// A process reading audio input.
#[derive(Debug, Clone)]
pub struct MicUser {
    pub pid: i32,
    pub bundle_id: String,
}

/// The meeting apps among the mic users, each once, in list order (native apps before browsers):
/// this process excluded, browsers only when allowed.
pub fn pick(users: &[MicUser], own_pid: i32, browsers: bool) -> Vec<&'static App> {
    let mut found: Vec<&'static App> = users
        .iter()
        .filter(|u| u.pid != own_pid && !u.bundle_id.to_ascii_lowercase().starts_with(OWN_PREFIX))
        .filter_map(|u| classify(&u.bundle_id))
        .filter(|a| browsers || !a.browser)
        .collect();
    found.sort_by_key(|a| rank(a));
    found.dedup_by(|a, b| std::ptr::eq(*a, *b));
    found
}

fn rank(a: &App) -> usize {
    APPS.iter().position(|b| std::ptr::eq(a, b)).unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------- decision logic

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Offer {
    #[default]
    None,
    /// "회의가 시작된 것 같아요. 녹음할까요?"
    Start(&'static App),
    /// the meeting app let go of the mic while we record: "녹음을 정지할까요?"
    End(&'static App),
}

/// What the user did with the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// 녹음 시작 (sent once the recording runs) / 녹음 정지
    Yes,
    /// 이번엔 안 함 / 계속 녹음: a start refused this way also starts the cooldown
    No,
    /// Escape: not now, for this meeting only; no cooldown
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// seen, not yet long enough to ask (or the cooldown is still running)
    Watching,
    Prompted,
    /// the user said no or hid it: silent until this meeting is over
    Dismissed,
    Recording,
    /// recorded (or accepted) and stopped while the meeting goes on: do not ask again
    Done,
}

/// One meeting of one app: a stretch of its mic use with no gap longer than `GRACE_MS`. Each app
/// has its own session, so a browser that keeps the mic never stands in for a Zoom meeting.
#[derive(Debug)]
struct Session {
    app: &'static App,
    /// start of the current unbroken run of sightings; `None` after a poll without one
    streak_since: Option<u64>,
    last_seen: u64,
    phase: Phase,
}

#[derive(Debug, Default)]
pub struct Watch {
    sessions: Vec<Session>,
    /// no start offer before this (set by 이번엔 안 함); meetings are still tracked meanwhile
    cooldown_until: u64,
    /// when the running recording was first seen; it counts only after `MIN_RECORDING_MS`
    recording_since: Option<u64>,
    /// native apps that let go of the mic during the recording, oldest first: one stop question each
    ended: Vec<&'static App>,
    offer: Offer,
    /// bumped at every change of `offer`; the webview shows it and sends it back with its answer
    seq: u64,
}

/// A recording shorter than this (a start that failed on permissions, say) does not cover the meeting.
pub const MIN_RECORDING_MS: u64 = 5_000;

impl Watch {
    pub fn offer(&self) -> Offer {
        self.offer
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// One poll. `hits` are the apps `pick` found now; `recording` = a capture process is running.
    /// Returns the new offer when it changed. `now` must count time asleep (see `clock_ms`): a sleep
    /// longer than the grace ends every meeting.
    pub fn tick(&mut self, now: u64, enabled: bool, recording: bool, hits: &[&'static App]) -> Option<Offer> {
        let before = self.offer;
        if !enabled {
            *self = Watch { seq: self.seq, offer: self.offer, ..Watch::default() }; // cooldown included
            self.derive(false);
            return self.changed(before);
        }
        self.recording_since = if recording { Some(self.recording_since.unwrap_or(now)) } else { None };
        let established = self.recording_since.is_some_and(|t| now.saturating_sub(t) >= MIN_RECORDING_MS);
        // first the meetings that are over, also when the app shows up again after a long stall or sleep
        let ended = &mut self.ended;
        self.sessions.retain(|s| {
            let over = now.saturating_sub(s.last_seen) >= GRACE_MS;
            // only a native app's release is a reliable end; a browser may keep or drop the mic for anything
            if over && s.phase == Phase::Recording && !s.app.browser && !ended.iter().any(|a| std::ptr::eq(*a, s.app)) {
                ended.push(s.app);
            }
            !over
        });
        for s in &mut self.sessions {
            if !hits.iter().any(|a| std::ptr::eq(*a, s.app)) {
                s.streak_since = None;
            }
        }
        for &app in hits {
            match self.sessions.iter_mut().find(|s| std::ptr::eq(s.app, app)) {
                Some(s) => {
                    s.last_seen = now;
                    s.streak_since.get_or_insert(now);
                }
                None => self.sessions.push(Session { app, streak_since: Some(now), last_seen: now, phase: Phase::Watching }),
            }
        }
        for s in &mut self.sessions {
            if established {
                s.phase = Phase::Recording;
            } else if s.phase == Phase::Recording && !recording {
                s.phase = Phase::Done;
            }
        }
        // one question at a time, none while a recording runs or starts, none during the cooldown
        if !recording && now >= self.cooldown_until && !self.sessions.iter().any(|s| s.phase == Phase::Prompted) {
            let ready = self
                .sessions
                .iter_mut()
                .filter(|s| s.phase == Phase::Watching && s.streak_since.is_some_and(|t| now.saturating_sub(t) >= CONFIRM_MS))
                .min_by_key(|s| rank(s.app));
            if let Some(s) = ready {
                s.phase = Phase::Prompted;
            }
        }
        self.derive(recording);
        self.changed(before)
    }

    /// The user answered the view with this `seq`. An answer to an older view (the offer moved on
    /// in between) is ignored, so a click never lands on a question that was not on screen.
    pub fn answer(&mut self, now: u64, seq: u64, answer: Answer, recording: bool) -> Option<Offer> {
        if seq != self.seq {
            return None;
        }
        let before = self.offer;
        match self.offer {
            Offer::Start(_) => {
                for s in &mut self.sessions {
                    let answered = s.phase == Phase::Prompted;
                    // a yes or no covers every app on the mic right now (one meeting); a hide only the one shown
                    if answered || (answer != Answer::Hide && s.phase == Phase::Watching) {
                        s.phase = if answer == Answer::Yes { Phase::Done } else { Phase::Dismissed };
                    }
                }
                if answer == Answer::No {
                    self.cooldown_until = now + COOLDOWN_MS;
                }
            }
            Offer::End(_) => {
                self.ended.remove(0);
            }
            Offer::None => {}
        }
        self.derive(recording);
        self.changed(before)
    }

    /// Stop watching some apps (browsers switched off): their sessions and questions go now.
    pub fn forget(&mut self, drop: impl Fn(&App) -> bool, recording: bool) -> Option<Offer> {
        let before = self.offer;
        self.sessions.retain(|s| !drop(s.app));
        self.ended.retain(|a| !drop(a));
        self.derive(recording);
        self.changed(before)
    }

    fn derive(&mut self, recording: bool) {
        let sessions = &self.sessions;
        // a stop question lasts until answered, the recording stops, or that app takes the mic again
        self.ended.retain(|a| recording && !sessions.iter().any(|s| std::ptr::eq(s.app, *a)));
        self.offer = match (sessions.iter().find(|s| s.phase == Phase::Prompted), self.ended.first()) {
            (Some(s), _) => Offer::Start(s.app),
            (None, Some(a)) => Offer::End(a),
            (None, None) => Offer::None,
        };
    }

    fn changed(&mut self, before: Offer) -> Option<Offer> {
        let changed = self.offer != before;
        if changed {
            self.seq += 1;
        }
        changed.then_some(self.offer)
    }
}

// ---------------------------------------------------------------- setting

/// The two switches, kept in `<data dir>/app-settings.json`. On by default because the check is
/// narrow: the app only asks, never records by itself, and looks at nothing but which process
/// uses the microphone. A switch the user turned off is saved as `false` and stays off.
/// Platforms without the signal are forced off by the callers (`AVAILABLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // a missing key keeps the others (no "browsers" must not turn the whole thing off)
pub struct Settings {
    pub enabled: bool,
    /// also offer when a web browser uses the mic (web meetings)
    pub browsers: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: true, browsers: true }
    }
}

pub const SETTINGS_FILE: &str = "app-settings.json";
const KEY: &str = "meeting_prompt";

/// Both switches off: what a damaged settings file means.
pub const OFF: Settings = Settings { enabled: false, browsers: false };

/// What `app-settings.json` holds: `None` when there is no file, `Some(None)` when there is one that
/// cannot be read as an object (damaged), else the object.
fn read_root(data_dir: &Path) -> Option<Option<serde_json::Map<String, serde_json::Value>>> {
    match std::fs::read(data_dir.join(SETTINGS_FILE)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(None),
        Ok(b) => Some(serde_json::from_slice::<serde_json::Value>(&b).ok().and_then(|v| v.as_object().cloned())),
    }
}

/// No file, or no key of ours: the default (on). A file that exists but cannot be read, or a key
/// that is not two switches, is off: the app does not start looking at the microphone on a guess.
/// The file is an object; this feature owns one key.
pub fn load_settings(data_dir: &Path) -> Settings {
    match read_root(data_dir) {
        None => Settings::default(),
        Some(None) => OFF,
        Some(Some(root)) => match root.get(KEY) {
            None => Settings::default(),
            Some(v) => serde_json::from_value(v.clone()).unwrap_or(OFF),
        },
    }
}

/// One key of `app-settings.json` (another feature's, e.g. `setup_card`); `None` when it is not there.
pub fn read_key(data_dir: &Path, key: &str) -> Option<serde_json::Value> {
    read_root(data_dir).flatten().and_then(|mut r| r.remove(key))
}

/// Rewrites one key; other keys in the file are kept. Written to a temp file and renamed. A damaged
/// file is replaced, and keeps meaning "meeting detection off" (the key is written as off).
pub fn save_key(data_dir: &Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
    // one read-modify-write at a time in this process (two features may save at once)
    static SAVE: Mutex<()> = Mutex::new(());
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let _guard = SAVE.lock().unwrap_or_else(|p| p.into_inner());
    meetko_core::util::create_private_dir(data_dir)?;
    let path = data_dir.join(SETTINGS_FILE);
    let mut root = match read_root(data_dir) {
        None => serde_json::Map::new(),
        Some(Some(r)) => r,
        Some(None) => {
            let mut r = serde_json::Map::new();
            r.insert(KEY.into(), serde_json::to_value(OFF).map_err(std::io::Error::other)?);
            r
        }
    };
    root.insert(key.into(), value);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = data_dir.join(format!("{SETTINGS_FILE}.{}-{n}.tmp", std::process::id()));
    meetko_core::util::write_private(&tmp, serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?.as_bytes())?;
    std::fs::rename(&tmp, &path)
}

pub fn save_settings(data_dir: &Path, s: Settings) -> std::io::Result<()> {
    save_key(data_dir, KEY, serde_json::to_value(s).map_err(std::io::Error::other)?)
}

/// Shared between the poller thread and the commands.
pub struct Shared {
    pub data_dir: PathBuf,
    pub settings: Mutex<Settings>,
    pub watch: Mutex<Watch>,
}

impl Shared {
    pub fn new(data_dir: PathBuf) -> Self {
        let mut s = load_settings(&data_dir);
        s.enabled &= AVAILABLE; // on by default, but never where the signal does not exist
        let settings = Mutex::new(s);
        Shared { data_dir, settings, watch: Mutex::new(Watch::default()) }
    }
    pub fn now(&self) -> u64 {
        clock_ms()
    }
    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn watch(&self) -> std::sync::MutexGuard<'_, Watch> {
        self.watch.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Milliseconds on a clock that keeps counting while the Mac sleeps (`Instant` there is uptime,
/// which would stretch a cooldown or a meeting over a night's sleep).
pub fn clock_ms() -> u64 {
    #[cfg(target_os = "macos")]
    return mac::continuous_ms();
    #[cfg(not(target_os = "macos"))]
    {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
    }
}

/// What the webview shows: `offer` is none | start | end. `seq` grows with every change; the
/// webview ignores a view older than the one it shows.
#[derive(Debug, Clone, Serialize)]
pub struct OfferView {
    pub offer: &'static str,
    pub app: Option<&'static str>,
    pub browser: bool,
    pub seq: u64,
}

impl OfferView {
    pub fn of(w: &Watch) -> Self {
        let (offer, app) = match w.offer() {
            Offer::None => ("none", None),
            Offer::Start(a) => ("start", Some(a)),
            Offer::End(a) => ("end", Some(a)),
        };
        OfferView { offer, app: app.map(|a| a.name), browser: app.is_some_and(|a| a.browser), seq: w.seq() }
    }
}

#[derive(Debug, Serialize)]
pub struct WatchState {
    /// false where detection is not implemented (everything but macOS)
    pub available: bool,
    pub enabled: bool,
    pub browsers: bool,
    pub offer: OfferView,
}

// ---------------------------------------------------------------- platform

/// Whether this build can detect meetings at all.
pub const AVAILABLE: bool = cfg!(target_os = "macos");

/// The processes reading audio input now; `None` when that cannot be asked here.
pub fn mic_users() -> Option<Vec<MicUser>> {
    #[cfg(target_os = "macos")]
    return mac::mic_users();
    #[cfg(not(target_os = "macos"))]
    None
}

#[cfg(target_os = "macos")]
mod mac {
    //! CoreAudio process objects (`kAudioHardwarePropertyProcessObjectList`, macOS 14+). Reading
    //! them records nothing and needs no microphone permission; it is the same information the
    //! system's own "microphone in use" indicator is based on.
    use std::ffi::c_void;
    use std::os::raw::c_char;

    use super::MicUser;

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const fn cc(b: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*b)
    }
    const SYSTEM_OBJECT: AudioObjectID = 1;
    const SCOPE_GLOBAL: u32 = cc(b"glob");
    const PROCESS_LIST: u32 = cc(b"prs#");
    const PROCESS_PID: u32 = cc(b"ppid");
    const PROCESS_BUNDLE_ID: u32 = cc(b"pbid");
    const PROCESS_RUNNING_INPUT: u32 = cc(b"piri");
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(id: AudioObjectID, addr: *const Address, qsize: u32, q: *const c_void, size: *mut u32) -> OSStatus;
        fn AudioObjectGetPropertyData(id: AudioObjectID, addr: *const Address, qsize: u32, q: *const c_void, size: *mut u32, out: *mut c_void) -> OSStatus;
    }
    extern "C" {
        fn clock_gettime_nsec_np(clock_id: u32) -> u64;
    }
    /// `CLOCK_MONOTONIC` on macOS counts time asleep (`CLOCK_UPTIME_RAW`, behind `Instant`, does not).
    const CLOCK_MONOTONIC: u32 = 6;

    pub fn continuous_ms() -> u64 {
        // SAFETY: no preconditions; returns 0 only on an invalid clock id
        let ns = unsafe { clock_gettime_nsec_np(CLOCK_MONOTONIC) };
        ns / 1_000_000
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFRelease(p: *const c_void);
    }

    fn addr(selector: u32) -> Address {
        Address { selector, scope: SCOPE_GLOBAL, element: 0 }
    }

    /// A fixed-size property of `id` (`T` is plain data).
    fn get<T: Copy + Default>(id: AudioObjectID, selector: u32) -> Option<T> {
        let mut out = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: `out` is a valid buffer of `size` bytes; the address lives across the call
        let st = unsafe { AudioObjectGetPropertyData(id, &addr(selector), 0, std::ptr::null(), &mut size, (&mut out as *mut T).cast()) };
        (st == 0 && size as usize == std::mem::size_of::<T>()).then_some(out)
    }

    fn bundle_id(id: AudioObjectID) -> Option<String> {
        let mut s: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        // SAFETY: a CFStringRef-sized out buffer; the returned string is owned by us (released below)
        let st = unsafe { AudioObjectGetPropertyData(id, &addr(PROCESS_BUNDLE_ID), 0, std::ptr::null(), &mut size, (&mut s as *mut *const c_void).cast()) };
        if st != 0 || s.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 256]; // bundle ids are far shorter; a longer one is not on our list anyway
        // SAFETY: `s` is a valid CFString; `buf` has the stated size
        let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) } != 0;
        // SAFETY: we own one reference
        unsafe { CFRelease(s) };
        // SAFETY: CFStringGetCString wrote a NUL-terminated string on success
        ok.then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }

    pub fn mic_users() -> Option<Vec<MicUser>> {
        let mut size = 0u32;
        // SAFETY: plain out parameter
        if unsafe { AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr(PROCESS_LIST), 0, std::ptr::null(), &mut size) } != 0 {
            return None;
        }
        let mut ids = vec![0 as AudioObjectID; size as usize / std::mem::size_of::<AudioObjectID>()];
        let mut size = (ids.len() * std::mem::size_of::<AudioObjectID>()) as u32;
        // SAFETY: `ids` holds `size` bytes
        if unsafe { AudioObjectGetPropertyData(SYSTEM_OBJECT, &addr(PROCESS_LIST), 0, std::ptr::null(), &mut size, ids.as_mut_ptr().cast()) } != 0 {
            return None;
        }
        ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
        Some(
            ids.into_iter()
                .filter(|&id| get::<u32>(id, PROCESS_RUNNING_INPUT).unwrap_or(0) != 0)
                .filter_map(|id| Some(MicUser { pid: get::<i32>(id, PROCESS_PID)?, bundle_id: bundle_id(id).unwrap_or_default() }))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoom() -> &'static App {
        classify("us.zoom.xos").unwrap()
    }
    fn teams() -> &'static App {
        classify("com.microsoft.teams2").unwrap()
    }
    fn chrome() -> &'static App {
        classify("com.google.Chrome.helper").unwrap()
    }
    const S: u64 = 1000;
    const NONE: &[&App] = &[];
    fn z() -> [&'static App; 1] {
        [zoom()]
    }
    fn c() -> [&'static App; 1] {
        [chrome()]
    }

    /// Polls every 3 s from `from` to `to` (inclusive) with `hits`; returns every change.
    fn run(w: &mut Watch, from: u64, to: u64, recording: bool, hits: &[&'static App]) -> Vec<(u64, Offer)> {
        let mut out = vec![];
        let mut t = from;
        while t <= to {
            if let Some(o) = w.tick(t, true, recording, hits) {
                out.push((t, o));
            }
            t += 3 * S;
        }
        out
    }

    /// Answer the view on screen (its current seq), not recording.
    fn ans(w: &mut Watch, now: u64, a: Answer) -> Option<Offer> {
        let seq = w.seq();
        w.answer(now, seq, a, false)
    }

    /// A Zoom meeting seen from t=0 until it is offered (at 6 s).
    fn offered() -> Watch {
        let mut w = Watch::default();
        assert_eq!(run(&mut w, 0, 6 * S, false, &z()), vec![(6 * S, Offer::Start(zoom()))]);
        w
    }

    #[test]
    fn bundle_ids_match_exactly_or_as_a_dotted_prefix_ignoring_case() {
        assert_eq!(classify("com.microsoft.teams2").unwrap().name, "Microsoft Teams");
        assert_eq!(classify("com.microsoft.teams").unwrap().name, "Microsoft Teams");
        assert_eq!(classify("com.microsoft.teams2.helper").unwrap().name, "Microsoft Teams");
        assert_eq!(classify("com.google.Chrome.helper").unwrap().name, "Chrome");
        assert_eq!(classify("company.thebrowser.browser.helper").unwrap().name, "Arc");
        assert_eq!(classify("US.ZOOM.XOS").unwrap().name, "Zoom");
        assert!(classify("us.zoom.xosx").is_none(), "a longer id is another app");
        assert!(classify("com.google.ChromeRemoteDesktop").is_none());
        assert!(classify("com.apple.VoiceMemos").is_none());
        assert!(classify("").is_none());
    }

    #[test]
    fn pick_skips_ourselves_and_lists_native_apps_first() {
        let u = |pid, id: &str| MicUser { pid, bundle_id: id.into() };
        let names = |v: Vec<&App>| v.iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(names(pick(&[u(1, "com.google.Chrome"), u(2, "us.zoom.xos"), u(3, "com.google.Chrome.helper")], 9, true)), ["Zoom", "Chrome"]);
        assert!(pick(&[u(1, "com.google.Chrome")], 9, false).is_empty(), "browsers can be switched off");
        assert!(pick(&[u(9, "us.zoom.xos")], 9, true).is_empty(), "our own pid never counts");
        assert!(pick(&[u(3, "dev.meetko.app"), u(4, "Dev.Meetko.App"), u(5, "")], 9, true).is_empty());
    }

    #[test]
    fn a_meeting_is_offered_once_after_the_confirm_time() {
        let mut w = offered();
        assert_eq!(run(&mut w, 9 * S, 600 * S, false, &z()), vec![], "no repeat while it stands");
        assert_eq!(w.offer(), Offer::Start(zoom()));
    }

    #[test]
    fn a_short_mic_use_is_never_offered() {
        let mut w = Watch::default();
        w.tick(0, true, false, &z());
        assert_eq!(run(&mut w, 3 * S, 120 * S, false, NONE), vec![]);
    }

    #[test]
    fn short_bursts_do_not_add_up_to_the_confirm_time() {
        // seen at 0, gone at 3, seen at 6, gone at 9, ...: never 6 s in a row
        let mut w = Watch::default();
        for k in 0..40u64 {
            let hits: &[&App] = if k % 2 == 0 { &z() } else { NONE };
            assert_eq!(w.tick(k * 3 * S, true, false, hits), None, "at poll {k}");
        }
        // then it stays: offered 6 s into the unbroken run
        assert_eq!(run(&mut w, 120 * S, 126 * S, false, &z()), vec![(126 * S, Offer::Start(zoom()))]);
    }

    #[test]
    fn dismissing_silences_this_meeting_and_starts_a_cooldown() {
        let mut w = offered();
        assert_eq!(ans(&mut w, 7 * S, Answer::No), Some(Offer::None));
        // the same meeting goes on for an hour, with short mic gaps: never asked again
        let mut t = 7 * S;
        while t < 3600 * S {
            t += 3 * S;
            let hits: &[&App] = if (t / S) % 20 < 15 { &z() } else { NONE };
            assert_eq!(w.tick(t, true, false, hits), None, "at {t}");
        }
        // it ends; a new one starts after the cooldown has long passed: asked again
        let end = t + GRACE_MS;
        assert_eq!(w.tick(end, true, false, NONE), None);
        assert_eq!(run(&mut w, end + S, end + S + CONFIRM_MS, false, &z()), vec![(end + S + CONFIRM_MS, Offer::Start(zoom()))]);
    }

    #[test]
    fn a_meeting_that_starts_inside_the_cooldown_is_offered_when_it_ends() {
        let mut w = offered();
        ans(&mut w, 6 * S, Answer::No); // cooldown until 606 s
        w.tick(6 * S + GRACE_MS, true, false, NONE); // over
        let changes = run(&mut w, 60 * S, 1260 * S, false, &z());
        assert_eq!(changes, vec![(606 * S, Offer::Start(zoom()))], "silent during the cooldown, then asked once");
    }

    #[test]
    fn a_dismissed_browser_does_not_swallow_a_later_zoom_meeting() {
        let mut w = Watch::default();
        assert_eq!(run(&mut w, 0, 6 * S, false, &c()), vec![(6 * S, Offer::Start(chrome()))]);
        ans(&mut w, 6 * S, Answer::No);
        // Chrome keeps the mic; 20 minutes later Zoom joins
        run(&mut w, 9 * S, 1200 * S, false, &c());
        let both = [zoom(), chrome()];
        assert_eq!(run(&mut w, 1203 * S, 1209 * S, false, &both), vec![(1209 * S, Offer::Start(zoom()))]);
    }

    #[test]
    fn a_native_app_starting_inside_a_browser_cooldown_is_offered_once_it_runs_out() {
        let mut w = Watch::default();
        run(&mut w, 0, 6 * S, false, &c());
        ans(&mut w, 6 * S, Answer::No); // cooldown until 606 s
        let both = [zoom(), chrome()];
        // Zoom starts 9 minutes later and goes on: asked when the cooldown ends, not silenced for good
        assert_eq!(run(&mut w, 546 * S, 1200 * S, false, &both), vec![(606 * S, Offer::Start(zoom()))]);
    }

    #[test]
    fn a_withdrawn_meeting_takes_its_offer_away() {
        let mut w = offered();
        assert_eq!(w.tick(6 * S + GRACE_MS, true, false, NONE), Some(Offer::None));
    }

    #[test]
    fn accepting_then_recording_then_ending_offers_a_stop_but_never_stops() {
        let mut w = offered();
        assert_eq!(ans(&mut w, 7 * S, Answer::Yes), Some(Offer::None));
        assert_eq!(run(&mut w, 9 * S, 60 * S, true, &z()), vec![], "recording: nothing to offer");
        // Zoom lets go of the mic
        assert_eq!(w.tick(63 * S, true, true, NONE), None, "within the grace");
        assert_eq!(w.tick(60 * S + GRACE_MS, true, true, NONE), Some(Offer::End(zoom())));
        // the user keeps recording: the offer goes, nothing else happens
        let seq = w.seq();
        assert_eq!(w.answer(61 * S + GRACE_MS, seq, Answer::No, true), Some(Offer::None));
        assert_eq!(run(&mut w, 63 * S + GRACE_MS, 200 * S, true, NONE), vec![]);
    }

    #[test]
    fn two_apps_ending_together_get_a_stop_question_each_in_turn() {
        let mut w = Watch::default();
        let both = [zoom(), teams()];
        run(&mut w, 0, 30 * S, true, &both);
        assert_eq!(w.tick(30 * S + GRACE_MS, true, true, NONE), Some(Offer::End(zoom())));
        let seq = w.seq();
        assert_eq!(w.answer(31 * S + GRACE_MS, seq, Answer::No, true), Some(Offer::End(teams())));
        let seq = w.seq();
        assert_eq!(w.answer(32 * S + GRACE_MS, seq, Answer::No, true), Some(Offer::None));
    }

    #[test]
    fn a_zoom_recording_gets_its_stop_offer_even_while_a_browser_keeps_the_mic() {
        let mut w = Watch::default();
        let both = [zoom(), chrome()];
        run(&mut w, 0, 60 * S, true, &both);
        let changes = run(&mut w, 63 * S, 63 * S + GRACE_MS, true, &c());
        assert_eq!(changes, vec![(60 * S + GRACE_MS, Offer::End(zoom()))]);
    }

    #[test]
    fn an_end_offer_goes_away_when_the_mic_returns_or_the_recording_stops() {
        let start = |w: &mut Watch| {
            run(w, 0, 9 * S, true, &z());
            assert_eq!(w.tick(9 * S + GRACE_MS, true, true, NONE), Some(Offer::End(zoom())));
        };
        let mut w = Watch::default();
        start(&mut w);
        assert_eq!(w.tick(12 * S + GRACE_MS, true, true, &z()), Some(Offer::None), "mic came back");
        let mut w = Watch::default();
        start(&mut w);
        assert_eq!(w.tick(12 * S + GRACE_MS, true, false, NONE), Some(Offer::None), "the user stopped it");
    }

    #[test]
    fn a_browser_meeting_is_offered_but_its_end_is_not_guessed() {
        let mut w = Watch::default();
        assert_eq!(run(&mut w, 0, 6 * S, false, &c()), vec![(6 * S, Offer::Start(chrome()))]);
        ans(&mut w, 6 * S, Answer::Yes);
        run(&mut w, 9 * S, 30 * S, true, &c());
        assert_eq!(w.tick(30 * S + GRACE_MS, true, true, NONE), None);
    }

    #[test]
    fn recording_by_hand_suppresses_the_offer_also_after_the_stop() {
        let mut w = Watch::default();
        assert_eq!(run(&mut w, 0, 12 * S, true, &z()), vec![]);
        assert_eq!(run(&mut w, 15 * S, 200 * S, false, &z()), vec![], "stopped, same meeting: not asked");
    }

    #[test]
    fn a_prompt_turns_into_recording_once_a_recording_by_hand_has_run_a_few_seconds() {
        let mut w = offered();
        assert_eq!(w.tick(9 * S, true, true, &z()), None, "just started: the card stays");
        assert_eq!(w.tick(12 * S, true, true, &z()), None);
        assert_eq!(w.tick(15 * S, true, true, &z()), Some(Offer::None), "running 6 s: the meeting is covered");
    }

    #[test]
    fn a_start_that_dies_at_once_does_not_cover_the_meeting() {
        // the provider was running for one poll (permission denied), then gone; the webview sent no yes
        let mut w = offered();
        assert_eq!(w.tick(9 * S, true, true, &z()), None);
        assert_eq!(w.tick(12 * S, true, false, &z()), None);
        assert_eq!(w.offer(), Offer::Start(zoom()), "the question is still open");
        // a meeting recorded by hand that failed the same way is offered later
        let mut w = Watch::default();
        run(&mut w, 0, 3 * S, true, &z());
        assert_eq!(run(&mut w, 6 * S, 12 * S, false, &z()), vec![(6 * S, Offer::Start(zoom()))]);
    }

    #[test]
    fn no_question_while_a_recording_is_starting() {
        let mut w = Watch::default();
        assert_eq!(run(&mut w, 0, 3 * S, true, &z()), vec![]);
        assert_eq!(w.tick(6 * S, true, true, &z()), None, "confirmed, but the recording is starting");
    }

    #[test]
    fn an_answer_to_an_older_view_is_ignored() {
        let mut w = Watch::default();
        let both = [zoom(), chrome()];
        assert_eq!(run(&mut w, 0, 6 * S, false, &c()), vec![(6 * S, Offer::Start(chrome()))]);
        let chrome_view = w.seq();
        run(&mut w, 9 * S, 15 * S, false, &both); // Zoom joins while Chrome's question is up
        // Chrome lets go; when its meeting ends, Zoom's question replaces it in the same poll
        assert_eq!(run(&mut w, 18 * S, 45 * S, false, &z()), vec![(45 * S, Offer::Start(zoom()))]);
        assert_eq!(w.answer(46 * S, chrome_view, Answer::No, false), None, "the click was for Chrome");
        assert_eq!(w.offer(), Offer::Start(zoom()));
        assert_eq!(ans(&mut w, 47 * S, Answer::Yes), Some(Offer::None), "the current view's answer counts");
    }

    #[test]
    fn hiding_silences_only_this_meeting_without_a_cooldown() {
        let mut w = Watch::default();
        let both = [zoom(), chrome()];
        // Chrome first, then Zoom joins while Chrome's question is up
        run(&mut w, 0, 6 * S, false, &c());
        assert_eq!(w.offer(), Offer::Start(chrome()));
        run(&mut w, 9 * S, 15 * S, false, &both);
        assert_eq!(ans(&mut w, 15 * S, Answer::Hide), Some(Offer::None));
        assert_eq!(w.tick(16 * S, true, false, &both), Some(Offer::Start(zoom())), "the slot is free for Zoom on the next poll");
        assert_eq!(ans(&mut w, 16 * S, Answer::Hide), Some(Offer::None));
        assert_eq!(run(&mut w, 18 * S, 600 * S, false, &both), vec![], "both meetings stay silent");
        // a new meeting soon after: no cooldown, asked again
        w.tick(600 * S + GRACE_MS, true, false, NONE);
        assert_eq!(run(&mut w, 640 * S, 646 * S, false, &z()), vec![(646 * S, Offer::Start(zoom()))]);
    }

    #[test]
    fn a_long_gap_such_as_sleep_ends_the_meeting_even_if_the_app_is_back() {
        let mut w = offered();
        ans(&mut w, 7 * S, Answer::No);
        // the Mac sleeps for 8 hours; the clock counts it. Zoom is on the mic right after waking:
        // the old meeting is over and the cooldown long gone, so this is a new meeting
        let wake = 8 * 3600 * S;
        assert_eq!(run(&mut w, wake, wake + CONFIRM_MS, false, &z()), vec![(wake + CONFIRM_MS, Offer::Start(zoom()))]);
    }

    #[test]
    fn a_stall_longer_than_the_grace_ends_a_recorded_meeting_before_the_new_sighting() {
        let mut w = Watch::default();
        run(&mut w, 0, 30 * S, true, &z());
        // no poll for a minute (the poller stalled); the next one sees Zoom again: the old meeting ended
        assert_eq!(w.tick(30 * S + 2 * GRACE_MS, true, true, &z()), None, "end offer then cleared by the new session in the same poll");
        assert_eq!(w.offer(), Offer::None);
    }

    #[test]
    fn turning_it_off_clears_everything_including_the_cooldown() {
        let mut w = offered();
        ans(&mut w, 7 * S, Answer::No);
        assert_eq!(w.tick(9 * S, false, false, &z()), None);
        assert_eq!(w.tick(12 * S, false, false, &z()), None);
        // on again: a fresh look, confirm time and all, and no cooldown left
        assert_eq!(run(&mut w, 15 * S, 21 * S, false, &z()), vec![(21 * S, Offer::Start(zoom()))]);
        assert_eq!(w.tick(24 * S, false, false, &z()), Some(Offer::None), "off takes a shown question away");
    }

    #[test]
    fn switching_browsers_off_takes_a_browser_question_away_at_once() {
        let mut w = Watch::default();
        run(&mut w, 0, 6 * S, false, &c());
        assert_eq!(w.forget(|a| a.browser, false), Some(Offer::None));
        let mut w = offered();
        assert_eq!(w.forget(|a| a.browser, false), None, "a native question stays");
    }

    #[test]
    fn answering_with_nothing_offered_changes_nothing() {
        let mut w = Watch::default();
        assert_eq!(ans(&mut w, 0, Answer::No), None);
        assert_eq!(run(&mut w, 1, 1 + CONFIRM_MS, false, &z()), vec![(1 + CONFIRM_MS, Offer::Start(zoom()))], "no cooldown came of it");
    }

    #[test]
    fn every_change_bumps_the_sequence() {
        let mut w = offered();
        let s1 = w.seq();
        assert_eq!(s1, 1);
        w.tick(9 * S, true, false, &z());
        assert_eq!(w.seq(), s1, "no change, same seq");
        ans(&mut w, 9 * S, Answer::No);
        assert_eq!(w.seq(), s1 + 1);
        assert_eq!(OfferView::of(&w).seq, s1 + 1);
    }

    #[test]
    fn settings_default_on_keep_other_keys_and_fill_missing_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: true }, "no file: on");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"other": 1}"#).unwrap();
        save_settings(&dir, Settings { enabled: true, browsers: true }).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: true });
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(SETTINGS_FILE)).unwrap()).unwrap();
        assert_eq!(v["other"], 1);
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"meeting_prompt": {"enabled": true}}"#).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: true }, "a missing key is the default");
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"meeting_prompt": {"browsers": false}}"#).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: false }, "a missing key does not undo the other");
        // what the user switched off stays off
        save_settings(&dir, Settings { enabled: false, browsers: false }).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: false, browsers: false });
        save_settings(&dir, Settings { enabled: true, browsers: false }).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: false });
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"other": 2}"#).unwrap();
        assert_eq!(load_settings(&dir), Settings { enabled: true, browsers: true }, "no key of ours: on");
        assert!(Settings::default().enabled && Settings::default().browsers);
        // a damaged file is off, not on: the app does not start watching on a guess
        std::fs::write(dir.join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(load_settings(&dir), OFF, "garbage reads as off");
        std::fs::write(dir.join(SETTINGS_FILE), "[1, 2]").unwrap();
        assert_eq!(load_settings(&dir), OFF, "not an object");
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"meeting_prompt": "yes"}"#).unwrap();
        assert_eq!(load_settings(&dir), OFF, "a key that is not two switches");
        // saving another key over a damaged file keeps detection off
        std::fs::write(dir.join(SETTINGS_FILE), "not json").unwrap();
        save_key(&dir, "setup_card", serde_json::json!({"dismissed": true})).unwrap();
        assert_eq!(load_settings(&dir), OFF);
        assert_eq!(read_key(&dir, "setup_card").unwrap()["dismissed"], true);
        // no file at all: on
        std::fs::remove_file(dir.join(SETTINGS_FILE)).unwrap();
        assert_eq!(load_settings(&dir), Settings::default());
        assert!(read_key(&dir, "setup_card").is_none());
        // two features saving at once both land
        let (d1, d2) = (dir.clone(), dir.clone());
        let a = std::thread::spawn(move || for _ in 0..50 { save_key(&d1, "a", serde_json::json!(1)).unwrap() });
        let b = std::thread::spawn(move || for _ in 0..50 { save_settings(&d2, OFF).unwrap() });
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(read_key(&dir, "a"), Some(serde_json::json!(1)));
        assert_eq!(load_settings(&dir), OFF);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mic_query_and_the_clock_answer_on_this_mac() {
        // read-only; nothing is recorded. Only checks that the calls work on this OS.
        // a CI runner VM may have no audio hardware; there only the clock is checked
        if std::env::var_os("CI").is_none() {
            assert!(mic_users().is_some());
        }
        let (a, b) = (clock_ms(), clock_ms());
        assert!(a > 0 && b >= a);
    }
}
