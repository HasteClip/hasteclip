//! Clips from other clipping apps, brought into the library (#75): for
//! someone switching to HasteClip, so all their clips are in one place.
//!
//! Each app is found by where it keeps its clips: its own settings say
//! where (the registry, a JSON, INI or VDF file, Discord's local storage),
//! else its default folder. A clip goes into the library's folder for its
//! game (one already there by the same name, give or take case, spaces and
//! punctuation: "Repo" goes in "R.E.P.O."), named as our own clips are
//! (`clip_YYYY-MM-DD_HH-MM-SS`, with a title the other app had after
//! ` - `), and dated when it was captured. Its game comes from, in order:
//! the app's own list of clips (Medal), the nearest folder it's in that
//! names a game, its file name (each app's pattern, see [`parse_stem`]).
//! Its date: Medal's list, its file name, else the file's date.
//!
//! | App | OS | Where | Names |
//! |---|---|---|---|
//! | NVIDIA (GeForce Experience, NVIDIA App) | Windows | `NVSPCAPS\DefaultPathW`, `GallerySettings.json`; Videos\NVIDIA, Videos | `<Game> 2024.01.02 - 12.34.56.78.DVR.mp4` in `<Game>\` |
//! | AMD Adrenalin (ReLive) | Windows | `HKCU\Software\AMD\DVR\CaptureFileOutput`; Videos\Radeon ReLive | `<Game>_replay_2024.01.02-12.34.56.mp4` in `<Game>\` |
//! | Xbox Game Bar | Windows | the Captures known folder | `<Game> 2024-01-02 12-34-56.mp4` |
//! | Discord | Windows, macOS | `clipsSettings.storageLocation` in its local storage; Videos\Discord Clips | `<Game_name>_<id>.mp4`; an older id is a snowflake, which holds the time |
//! | Medal | Windows | `store\settings.json` clipFolder; C:\Medal\Clips | `store\clips.json` has each clip's game, title and date |
//! | Outplayed | Windows | Overwolf's `SettingsPageGeneral.xml`; Videos\Overwolf\Outplayed | `<Game> 01-02-2024_12-34-56-789.mp4` in `<Game>\` |
//! | SteelSeries Moments | Windows | Videos\SteelSeries Moments | in `<Game>\` |
//! | Steam game recording | all | `localconfig.vdf` BackgroundRecordPath; `userdata\<id>\gamerecordings` | `clips\clip_<appid>_20240102_123456\` (video in pieces, joined on import) |
//! | OBS Studio | all | each profile's `basic.ini`; Videos, Movies, home | `Replay 2024-01-02 12-34-56.mp4` |
//! | GPU Screen Recorder | Linux | `~/.config/gpu-screen-recorder/config_ui` | `Replay_2024-01-02_12-34-56.mp4`, `<Game> - 2024-01-02 12-34-56.mp4` |
//! | ReplaySorcery | Linux | `replay-sorcery.conf` outputFile | `2024-01-02_12-34-56.mp4` |
//!
//! Anything else: a folder of your own choosing ([`AppKind::Folder`]), its
//! subfolders taken as games.
//!
//! A folder many apps share (Videos, home) only gives the clips named in
//! that app's pattern, so OBS doesn't take every video in Videos; a folder
//! another app found is left to it; and the library's own folder is never
//! looked in.
//!
//! Clips are copied, or moved when asked to. A move on one drive is a
//! rename: nothing is copied or deleted, and it needs no space. A move to
//! another drive copies, checks the copy against the original byte for
//! byte, and only then deletes the original (for good, not to the Recycle
//! Bin: a move is to free that space). Steam's are always copied: they're
//! pieces Steam keeps its own index of. An import stops when the disk fills
//! up. What's been imported is noted after each clip
//! (`.hasteclip/imported.json`), so importing again only brings what's new.

use std::collections::{HashMap, HashSet};
use std::hash::Hasher as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};

use crate::archive::Progress;

/// An app clips can be imported from. (Each OS looks for some of them: the
/// others are never made there.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum AppKind {
    Nvidia,
    Amd,
    GameBar,
    Discord,
    Medal,
    Outplayed,
    SteelSeries,
    Steam,
    Obs,
    GpuScreenRecorder,
    ReplaySorcery,
    /// A folder chosen by hand.
    Folder,
}

/// The apps looked for on this computer, in the order they're listed.
#[cfg(windows)]
pub const HERE: &[AppKind] = &[AppKind::Nvidia, AppKind::Amd, AppKind::GameBar, AppKind::Discord, AppKind::Medal, AppKind::Outplayed, AppKind::SteelSeries, AppKind::Steam, AppKind::Obs];
#[cfg(target_os = "macos")]
pub const HERE: &[AppKind] = &[AppKind::Discord, AppKind::Steam, AppKind::Obs];
#[cfg(target_os = "linux")]
pub const HERE: &[AppKind] = &[AppKind::Steam, AppKind::Obs, AppKind::GpuScreenRecorder, AppKind::ReplaySorcery];

impl AppKind {
    pub fn name(self) -> &'static str {
        match self {
            AppKind::Nvidia => "NVIDIA",
            AppKind::Amd => "AMD Adrenalin",
            AppKind::GameBar => "Xbox Game Bar",
            AppKind::Discord => "Discord",
            AppKind::Medal => "Medal",
            AppKind::Outplayed => "Outplayed",
            AppKind::SteelSeries => "SteelSeries Moments",
            AppKind::Steam => "Steam",
            AppKind::Obs => "OBS Studio",
            AppKind::GpuScreenRecorder => "GPU Screen Recorder",
            AppKind::ReplaySorcery => "ReplaySorcery",
            AppKind::Folder => "Folder",
        }
    }

    /// How deep under its folder the app keeps clips: 0 all in it, 1 in a
    /// folder per game, 2 a folder further. NVIDIA's are 2: in Videos it's
    /// `NVIDIA\<Game>\` (or `<Game>\` from GeForce Experience, or a folder
    /// they were moved into, `<Folder>\<Game>\`).
    fn depth(self) -> usize {
        match self {
            AppKind::GameBar | AppKind::Obs | AppKind::ReplaySorcery | AppKind::Discord => 0,
            AppKind::GpuScreenRecorder => 1,
            _ => 2,
        }
    }

    /// The names of this app's that a shared folder (Videos, home) gives:
    /// in a folder of its own, any video is the app's.
    fn names(self, pattern: Pattern, game: bool) -> bool {
        match self {
            AppKind::Nvidia => pattern == Pattern::Nvidia,
            AppKind::Amd => pattern == Pattern::Amd,
            AppKind::GameBar => pattern == Pattern::Spaced && game,
            AppKind::Discord => pattern == Pattern::Discord,
            AppKind::Medal => pattern == Pattern::Medal,
            AppKind::Outplayed => pattern == Pattern::Outplayed,
            AppKind::Obs => pattern == Pattern::Spaced && !game,
            AppKind::GpuScreenRecorder => matches!(pattern, Pattern::Underscore | Pattern::Spaced),
            AppKind::ReplaySorcery => pattern == Pattern::Underscore && !game,
            AppKind::SteelSeries | AppKind::Steam | AppKind::Folder => pattern != Pattern::None,
        }
    }
}

/// An app found here, and the folders it keeps clips in.
#[derive(Debug, Clone)]
pub struct Source {
    pub kind: AppKind,
    pub folders: Vec<PathBuf>,
}

/// A clip found in another app's folder.
#[derive(Debug, Clone)]
pub struct Found {
    /// The video, or for Steam the clip's folder.
    pub path: PathBuf,
    /// The source folder it was found in.
    pub folder: PathBuf,
    pub game: Option<String>,
    pub captured: NaiveDateTime,
    /// A title given to it in the other app.
    pub title: Option<String>,
    pub bytes: u64,
    /// The extension it'll have here (without the dot).
    pub ext: String,
    /// A Steam clip: pieces to join into one video.
    pub steam: bool,
    /// On the same drive as the library: moving it there is a rename.
    pub on_library_drive: bool,
}

impl Found {
    /// The bytes importing it takes on the library's drive: none to move
    /// it within one drive.
    pub fn needs(&self, mode: Mode) -> u64 {
        if mode == Mode::Move && self.on_library_drive && !self.steam { 0 } else { self.bytes }
    }
}

/// The apps found here with somewhere they keep clips. Reads their settings:
/// call it off the UI thread.
pub fn detect() -> Vec<Source> {
    HERE.iter()
        .filter_map(|&kind| {
            let mut folders: Vec<PathBuf> = Vec::new();
            for f in folders_of(kind).into_iter().map(|f| clean(&f)) {
                if f.is_dir() && !folders.iter().any(|g| same_path(g, &f)) {
                    folders.push(f);
                }
            }
            (!folders.is_empty()).then_some(Source { kind, folders })
        })
        .collect()
}

/// A path as the OS writes it: one kind of slash, none doubled (settings
/// files have `E:\\Videos`, `E:/Videos`, `c:/program files (x86)/steam`).
pub fn clean(p: &Path) -> PathBuf {
    p.components().collect()
}

/// The clips in a source's folders, newest first. `others` are the other
/// sources' folders, left to them; nothing in the library at `lib` is
/// taken. Reads every folder: off the UI thread.
pub fn scan(src: &Source, others: &[PathBuf], lib: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    for folder in &src.folders {
        if within(folder, lib) {
            continue;
        }
        let start = found.len();
        if src.kind == AppKind::Steam {
            found.extend(scan_steam(folder));
        } else {
            let strict = is_shared(folder);
            let skip: Vec<&Path> = others.iter().filter(|o| !same_path(o, folder)).map(PathBuf::as_path).collect();
            walk(src.kind, folder, folder, src.kind.depth(), strict, &skip, lib, &mut found);
        }
        let on_drive = same_drive(folder, lib);
        for f in &mut found[start..] {
            f.on_library_drive = on_drive;
        }
    }
    if src.kind == AppKind::Medal {
        medal_details(&mut found);
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.captured));
    found
}

/// Clips in `dir` (`root` or under it), down `depth` more folders, not
/// going into the folders in `skip` (other apps', reached from a folder
/// above theirs) or anywhere in the library.
#[allow(clippy::too_many_arguments)]
fn walk(kind: AppKind, root: &Path, dir: &Path, depth: usize, strict: bool, skip: &[&Path], lib: &Path, out: &mut Vec<Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files, and files still being written by some apps.
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let lower = name.to_lowercase();
            if depth > 0 && !SKIP_DIRS.contains(&lower.as_str()) && !skip.iter().any(|s| same_path(&path, s)) && !within(&path, lib) {
                walk(kind, root, &path, depth - 1, strict, skip, lib, out);
            }
            continue;
        }
        if !crate::clips::is_video(&path) {
            continue;
        }
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = parse_stem(&stem);
        if strict && !kind.names(parsed.pattern, parsed.game.is_some()) {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Found {
            game: folder_game(root, &path).or(parsed.game),
            captured: parsed.at.unwrap_or_else(|| DateTime::<Local>::from(modified).naive_local()),
            title: None,
            bytes: meta.len(),
            ext: path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "mp4".into()),
            steam: false,
            on_library_drive: false,
            folder: root.to_path_buf(),
            path,
        });
    }
}

/// The game a clip's folders name: the nearest one under `root` that's a
/// game's (`osu!` in `NvidiaClips\osu!\`, `VALORANT` in `VALORANT\2024-05-01\`).
fn folder_game(root: &Path, file: &Path) -> Option<String> {
    let rel = file.parent()?.strip_prefix(root).ok()?;
    let name = rel.components().rev().map(|c| c.as_os_str().to_string_lossy().into_owned()).find(|n| is_game_folder(n))?;
    Some(if name.eq_ignore_ascii_case("desktop") { crate::clips::DESKTOP.to_owned() } else { name })
}

/// Folders that hold clips but aren't a game, or hold nothing finished.
const SKIP_DIRS: &[&str] = &["temp-capture", "tmp", "temp", "thumbnails", "thumbs", "cache"];

/// Whether a folder clips are in names their game: not a date (GPU Screen
/// Recorder's folder per day), one of the generic names apps use, or an app
/// NVIDIA didn't know.
fn is_game_folder(name: &str) -> bool {
    const GENERIC: &[&str] = &["clips", "replays", "replay", "recordings", "videos", "highlights", "captures", "instant replays", "outplayed", "medal", "nvidia", "nvidiaclips", "nvidia clips"];
    let lower = name.to_lowercase();
    if GENERIC.contains(&lower.as_str()) || unknown_app(name) {
        return false;
    }
    let date = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d").is_ok() || chrono::NaiveDate::parse_from_str(name, "%Y.%m.%d").is_ok() || chrono::NaiveDate::parse_from_str(name, "%m-%d-%Y").is_ok();
    !date
}

/// What NVIDIA names an app it has no name for: a hash (40 hex digits), or
/// its drivers' "Base Profile" (the settings any unknown app gets).
fn unknown_app(name: &str) -> bool {
    let hash = (32..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_hexdigit());
    hash || name.eq_ignore_ascii_case("base profile")
}

/// Which app's naming a file name follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// `<Game> 2024.01.02 - 12.34.56.78[.DVR]`
    Nvidia,
    /// `<Game>_replay_2024.01.02-12.34[.56]`
    Amd,
    /// `[<Game> [- ]|Replay ]2024-01-02 12-34-56`: Game Bar, OBS, GPU Screen Recorder.
    Spaced,
    /// `[Replay_|Video_]2024-01-02_12-34-56`: GPU Screen Recorder, ReplaySorcery.
    Underscore,
    /// `<Game> 01-02-2024_12-34-56[-789]`
    Outplayed,
    /// `MedalTV<Game>20240102123456`
    Medal,
    /// `<Game_name>_<snowflake or UUID>`
    Discord,
    /// Our own `clip_2024-01-02_12-34-56`.
    Own,
    None,
}

/// What a file name says about a clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub game: Option<String>,
    pub at: Option<NaiveDateTime>,
    pub pattern: Pattern,
}

/// The game and capture time in a clip's file name (without its
/// extension), in any of the apps' patterns.
pub fn parse_stem(stem: &str) -> Parsed {
    let hit = |pattern, prefix: &str, at| Parsed { game: game_of(prefix), at: Some(at), pattern };
    // NVIDIA: an index (2 digits) after the seconds, `.DVR` on instant replays.
    let s = stem.strip_suffix(".DVR").unwrap_or(stem);
    if let Some((a, idx)) = s.rsplit_once('.')
        && (1..=3).contains(&idx.len())
        && idx.bytes().all(|b| b.is_ascii_digit())
        && let Some((pre, at)) = tail(a, "%Y.%m.%d - %H.%M.%S", 21)
    {
        return hit(Pattern::Nvidia, pre, at);
    }
    // AMD: `_replay_` or `_record_`, the seconds sometimes left out.
    if let Some((pre, at)) = tail(stem, "%Y.%m.%d-%H.%M.%S", 19).or_else(|| tail(stem, "%Y.%m.%d-%H.%M", 16))
        && let Some(pre) = pre.strip_suffix('_')
    {
        let game = pre.strip_suffix("_replay").or_else(|| pre.strip_suffix("_record")).unwrap_or(pre);
        return hit(Pattern::Amd, game, at);
    }
    if let Some((pre, at)) = tail(stem, "%Y-%m-%d %H-%M-%S", 19) {
        return hit(Pattern::Spaced, pre, at);
    }
    if let Some((pre, at)) = tail(stem, "%Y-%m-%d_%H-%M-%S", 19) {
        if pre == "clip_" {
            return Parsed { game: None, at: Some(at), pattern: Pattern::Own };
        }
        return hit(Pattern::Underscore, pre, at);
    }
    // Outplayed: month first, milliseconds after.
    let s = match stem.rsplit_once('-') {
        Some((a, ms)) if ms.len() == 3 && ms.bytes().all(|b| b.is_ascii_digit()) => a,
        _ => stem,
    };
    if let Some((pre, at)) = tail(s, "%m-%d-%Y_%H-%M-%S", 19) {
        return hit(Pattern::Outplayed, pre, at);
    }
    if let Some(rest) = stem.strip_prefix("MedalTV")
        && let Some((pre, at)) = tail(rest, "%Y%m%d%H%M%S", 14)
    {
        return hit(Pattern::Medal, pre, at);
    }
    // Discord: the game with `_` for spaces, then the clip's id. Clipping a
    // screen, not a game, it's `Screen_1`.
    if let Some((pre, id)) = stem.rsplit_once('_')
        && !pre.is_empty()
    {
        let snowflake = (15..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
        let uuid = id.len() == 36 && id.bytes().filter(|&b| b == b'-').count() == 4 && id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit());
        if snowflake || uuid {
            let name = pre.replace('_', " ");
            let screen = name.strip_prefix("Screen").is_some_and(|n| n.trim().bytes().all(|b| b.is_ascii_digit()));
            let game = if screen { Some(crate::clips::DESKTOP.to_owned()) } else { game_of(&name) };
            let at = if snowflake { id.parse().ok().and_then(snowflake_time) } else { None };
            return Parsed { game, at, pattern: Pattern::Discord };
        }
    }
    Parsed { game: None, at: None, pattern: Pattern::None }
}

/// When a Discord id was made: its top bits are milliseconds since 2015.
fn snowflake_time(id: u64) -> Option<NaiveDateTime> {
    let ms = (id >> 22) as i64 + 1_420_070_400_000;
    // Anything after tomorrow isn't a snowflake.
    if ms > Local::now().timestamp_millis() + 86_400_000 {
        return None;
    }
    Local.timestamp_millis_opt(ms).single().map(|d| d.naive_local())
}

/// The time in the last `len` bytes of `s`, in `fmt`, and what's before it.
fn tail<'a>(s: &'a str, fmt: &str, len: usize) -> Option<(&'a str, NaiveDateTime)> {
    let cut = s.len().checked_sub(len)?;
    if !s.is_char_boundary(cut) {
        return None;
    }
    // Only digits where the format has numbers (chrono takes a number of
    // fewer digits, or with a sign, where it can).
    let t = &s[cut..];
    if !t.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    let at = NaiveDateTime::parse_from_str(t, fmt).ok()?;
    Some((&s[..cut], at))
}

/// A game's name from what comes before the time in a file name, if it's
/// one: not empty, not a word apps put there themselves, not an app NVIDIA
/// didn't know.
fn game_of(prefix: &str) -> Option<String> {
    const NOT_GAMES: &[&str] = &["replay", "video", "clip", "recording", "screenshot", "instant replay"];
    let name = prefix.trim_end_matches([' ', '-', '_', '.']).trim();
    if name.eq_ignore_ascii_case("desktop") {
        return Some(crate::clips::DESKTOP.to_owned());
    }
    if name.is_empty() || NOT_GAMES.contains(&name.to_lowercase().as_str()) || unknown_app(name) {
        return None;
    }
    Some(name.to_owned())
}

// --- Where each app keeps its clips ---

/// The folders an app keeps clips in, from its settings, else its default
/// (whether or not they exist).
fn folders_of(kind: AppKind) -> Vec<PathBuf> {
    let videos = dirs::video_dir();
    let mut out: Vec<PathBuf> = Vec::new();
    match kind {
        AppKind::Nvidia => {
            #[cfg(windows)]
            out.extend(registry::value(r"Software\NVIDIA Corporation\Global\ShadowPlay\NVSPCAPS", "DefaultPathW").map(PathBuf::from));
            if let Some(local) = dirs::data_local_dir()
                && let Ok(text) = std::fs::read_to_string(local.join("NVIDIA Corporation").join("NVIDIA Overlay").join("GallerySettings.json"))
            {
                out.extend(nvidia_gallery(&text));
            }
            out.extend(videos.iter().map(|v| v.join("NVIDIA")));
            out.extend(videos.clone());
        }
        AppKind::Amd => {
            #[cfg(windows)]
            out.extend(registry::value(r"Software\AMD\DVR", "CaptureFileOutput").map(|v| PathBuf::from(expand_vars(&v))));
            out.extend(videos.iter().map(|v| v.join("Radeon ReLive")));
        }
        AppKind::GameBar => {
            #[cfg(windows)]
            {
                // The Captures known folder, where it's been moved to.
                out.extend(registry::value(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders", "{EDC0FE71-98D8-4F4A-B920-C8DC133CB165}").map(|v| PathBuf::from(expand_vars(&v))));
                out.extend(registry::value(r"Software\Microsoft\Windows\CurrentVersion\GameDVR", "AppCaptureFolder").map(|v| PathBuf::from(expand_vars(&v))));
            }
            out.extend(videos.iter().map(|v| v.join("Captures")));
        }
        AppKind::Discord => {
            if let Some(config) = dirs::config_dir() {
                for app in ["discord", "discordptb", "discordcanary"] {
                    out.extend(discord_folder(&config.join(app).join("Local Storage").join("leveldb")));
                }
            }
            out.extend(videos.iter().map(|v| v.join("Discord Clips")));
            out.extend(videos.iter().map(|v| v.join("DiscordClips")));
        }
        AppKind::Medal => {
            if let Some(roaming) = dirs::config_dir()
                && let Ok(text) = std::fs::read_to_string(roaming.join("Medal").join("store").join("settings.json"))
            {
                out.extend(medal_folder(&text));
            }
            out.push(PathBuf::from(r"C:\Medal\Clips"));
            out.extend(videos.iter().map(|v| v.join("Medal")));
        }
        AppKind::Outplayed => {
            if let Some(local) = dirs::data_local_dir()
                && let Ok(text) = std::fs::read_to_string(local.join("Overwolf").join("Settings").join("SettingsPageGeneral.xml"))
            {
                out.extend(overwolf_folder(&text).map(|f| f.join("Outplayed")));
            }
            out.extend(videos.iter().map(|v| v.join("Overwolf").join("Outplayed")));
        }
        AppKind::SteelSeries => out.extend(videos.iter().map(|v| v.join("SteelSeries Moments"))),
        AppKind::Steam => out.extend(steam_folders()),
        AppKind::Obs => out.extend(obs_folders()),
        AppKind::GpuScreenRecorder => {
            if let Some(config) = dirs::config_dir() {
                for file in ["config_ui", "config"] {
                    if let Ok(text) = std::fs::read_to_string(config.join("gpu-screen-recorder").join(file)) {
                        out.extend(gsr_folders(&text));
                    }
                }
            }
            out.extend(videos.clone());
        }
        AppKind::ReplaySorcery => {
            let home = dirs::home_dir();
            let mut confs = vec![PathBuf::from("/etc/replay-sorcery.conf"), PathBuf::from("/usr/local/etc/replay-sorcery.conf")];
            confs.extend(dirs::config_dir().map(|c| c.join("replay-sorcery.conf")));
            // The user's own settings win, so theirs is looked at first.
            for conf in confs.iter().rev() {
                if let Ok(text) = std::fs::read_to_string(conf) {
                    out.extend(replay_sorcery_folder(&text, home.as_deref()));
                }
            }
            out.extend(videos.iter().map(|v| v.join("ReplaySorcery")));
        }
        AppKind::Folder => {}
    }
    out
}

/// The NVIDIA App's gallery folder (`settings.currentDirectoryV2`).
fn nvidia_gallery(json: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let dir = v.pointer("/settings/currentDirectoryV2").or_else(|| find_key(&v, "currentDirectoryV2"))?.as_str()?;
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// Medal's clip folder (`recorder.clipFolder`, wherever newer versions keep it).
fn medal_folder(json: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let dir = v.pointer("/recorder/clipFolder").or_else(|| find_key(&v, "clipFolder"))?.as_str()?;
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// Discord's clips folder: `"storageLocation"` in its clip settings, kept
/// in its local storage (a LevelDB folder). The newest file's last
/// mention is the setting as it is now.
fn discord_folder(leveldb: &Path) -> Option<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(leveldb)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log" || x == "ldb"))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
        .collect();
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    files.iter().find_map(|(_, p)| std::fs::read(p).ok().and_then(|b| storage_location(&b)))
}

/// The last `"storageLocation":"…"` in these bytes, unescaped.
fn storage_location(bytes: &[u8]) -> Option<PathBuf> {
    const KEY: &[u8] = b"\"storageLocation\":\"";
    let at = bytes.windows(KEY.len()).rposition(|w| w == KEY)? + KEY.len();
    // Up to the closing quote (one not escaped).
    let mut end = at;
    while end < bytes.len() && !(bytes[end] == b'"' && bytes[end - 1] != b'\\') {
        end += 1;
    }
    let raw = std::str::from_utf8(bytes.get(at..end)?).ok()?;
    let value: String = serde_json::from_str(&format!("\"{raw}\"")).ok()?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// The first value named `key` anywhere in `v`.
fn find_key<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match v {
        serde_json::Value::Object(map) => map.get(key).or_else(|| map.values().find_map(|c| find_key(c, key))),
        serde_json::Value::Array(items) => items.iter().find_map(|c| find_key(c, key)),
        _ => None,
    }
}

/// Overwolf's captures folder: the `<Value>` of the setting named
/// `Folders_VideoCapturesFolder` (just before its `<Name>`, or after it).
fn overwolf_folder(xml: &str) -> Option<PathBuf> {
    let at = xml.find("Folders_VideoCapturesFolder")?;
    let value = |s: &str| -> Option<String> {
        let start = s.find("<Value>")? + "<Value>".len();
        let end = s[start..].find("</Value>")? + start;
        Some(xml_unescape(s[start..end].trim()))
    };
    // The value nearest before the name, within its setting.
    let before = &xml[..at];
    let setting_start = before.rfind("<Value>").map(|i| &before[i..]);
    let found = setting_start.and_then(value).filter(|v| !v.is_empty()).or_else(|| value(&xml[at..]).filter(|v| !v.is_empty()))?;
    Some(PathBuf::from(found))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// GPU Screen Recorder's save folders (`replay.save_directory`,
/// `record.save_directory`; not screenshots'), from `key value` lines.
fn gsr_folders(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once(' ')?;
            (key.ends_with("save_directory") && !key.starts_with("screenshot")).then(|| PathBuf::from(value.trim()))
        })
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// ReplaySorcery's output folder: the folder of `outputFile` (a strftime
/// pattern, `~` for home), up to the first part with a `%` in it.
fn replay_sorcery_folder(text: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "outputFile").then(|| v.trim().trim_matches('"').to_owned())
    })?;
    let value = match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(value),
    };
    let mut dir = PathBuf::new();
    let parent = value.parent()?;
    for c in parent.components() {
        if c.as_os_str().to_string_lossy().contains('%') {
            break;
        }
        dir.push(c);
    }
    (!dir.as_os_str().is_empty()).then_some(dir)
}

/// OBS's recording folders, from every profile (Simple and Advanced output
/// both), and its default when a profile has none.
fn obs_folders() -> Vec<PathBuf> {
    // The Flatpak's own on Linux too.
    let flatpak = dirs::home_dir().filter(|_| cfg!(target_os = "linux")).map(|h| h.join(".var/app/com.obsproject.Studio/config/obs-studio"));
    let roots: Vec<PathBuf> = dirs::config_dir().map(|c| c.join("obs-studio")).into_iter().chain(flatpak).collect();
    let default = if cfg!(target_os = "linux") { dirs::home_dir() } else { dirs::video_dir() };
    let mut out = Vec::new();
    for root in roots.iter().filter(|r| r.is_dir()) {
        let Ok(profiles) = std::fs::read_dir(root.join("basic").join("profiles")) else {
            out.extend(default.clone());
            continue;
        };
        for p in profiles.flatten() {
            let Ok(text) = std::fs::read_to_string(p.path().join("basic.ini")) else { continue };
            let found = obs_profile_folders(&text);
            if found.is_empty() {
                out.extend(default.clone());
            }
            out.extend(found);
        }
    }
    out
}

/// The folders one OBS profile (`basic.ini`) records to.
fn obs_profile_folders(ini: &str) -> Vec<PathBuf> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in ini.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let wanted = matches!((section.as_str(), key.trim()), ("SimpleOutput", "FilePath") | ("AdvOut", "RecFilePath") | ("AdvOut", "FFFilePath"));
        if wanted && !value.trim().is_empty() {
            out.push(PathBuf::from(value.trim()));
        }
    }
    out
}

/// Steam's game recording folders: each account's own, or where it's been
/// moved to (`localconfig.vdf`, GameRecording → BackgroundRecordPath).
fn steam_folders() -> Vec<PathBuf> {
    let Some(steam) = crate::game_events::steam_dir() else { return Vec::new() };
    let Ok(users) = std::fs::read_dir(steam.join("userdata")) else { return Vec::new() };
    let mut out = Vec::new();
    for user in users.flatten() {
        let dir = user.path();
        let moved = std::fs::read_to_string(dir.join("config").join("localconfig.vdf")).ok().and_then(|t| vdf_value(&t, "BackgroundRecordPath"));
        out.push(moved.map(PathBuf::from).unwrap_or_else(|| dir.join("gamerecordings")));
    }
    out
}

/// The first `"key" "value"` in a Steam `.vdf`/`.acf` file with this key
/// (its backslashes unescaped).
fn vdf_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        match quoted[..] {
            [k, v] if k.eq_ignore_ascii_case(key) && !v.is_empty() => Some(v.replace("\\\\", "\\")),
            _ => None,
        }
    })
}

/// Steam's clips in a recording folder: `clips/clip_<appid>_<date>_<time>/`,
/// named for their game from the games installed.
fn scan_steam(folder: &Path) -> Vec<Found> {
    let clips = folder.join("clips");
    let dir = if clips.is_dir() { clips } else { folder.to_path_buf() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let names = steam_game_names();
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (app, at) = parse_steam_clip(&name)?;
            let path = e.path();
            steam_stream_dir(&path)?;
            Some(Found {
                game: names.get(&app).cloned(),
                captured: at,
                title: None,
                bytes: tree_bytes(&path),
                ext: "mp4".into(),
                steam: true,
                on_library_drive: false,
                folder: folder.to_path_buf(),
                path,
            })
        })
        .collect()
}

/// A Steam clip folder's app id and time: `clip_<appid>_<YYYYMMDD>_<HHMMSS>`.
fn parse_steam_clip(name: &str) -> Option<(String, NaiveDateTime)> {
    let rest = name.strip_prefix("clip_")?;
    let (app, when) = rest.split_once('_')?;
    if app.is_empty() || !app.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let at = NaiveDateTime::parse_from_str(when.get(..15)?, "%Y%m%d_%H%M%S").ok()?;
    Some((app.to_owned(), at))
}

/// Where in a Steam clip folder its video's pieces are (the folder with
/// `init-stream0.m4s`), up to three folders down.
fn steam_stream_dir(dir: &Path) -> Option<PathBuf> {
    fn find(dir: &Path, depth: usize) -> Option<PathBuf> {
        if dir.join("init-stream0.m4s").is_file() {
            return Some(dir.to_path_buf());
        }
        if depth == 0 {
            return None;
        }
        std::fs::read_dir(dir).ok()?.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).find_map(|e| find(&e.path(), depth - 1))
    }
    find(dir, 3)
}

/// Installed Steam games' names, by app id.
fn steam_game_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    for lib in crate::game_events::steam_libraries() {
        let Ok(entries) = std::fs::read_dir(lib.join("steamapps")) else { continue };
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            let Some(id) = file.strip_prefix("appmanifest_").and_then(|f| f.strip_suffix(".acf")) else { continue };
            if let Ok(text) = std::fs::read_to_string(e.path())
                && let Some(name) = vdf_value(&text, "name")
            {
                names.insert(id.to_owned(), name);
            }
        }
    }
    names
}

/// Medal's own list of clips (`store\clips.json`): each one's game, the
/// title given it and when it was made, matched by its file.
fn medal_details(found: &mut [Found]) {
    let Some(roaming) = dirs::config_dir() else { return };
    let Ok(text) = std::fs::read_to_string(roaming.join("Medal").join("store").join("clips.json")) else { return };
    let details = medal_clips(&text);
    for f in found.iter_mut() {
        if let Some(d) = details.get(&path_key(&f.path)) {
            if d.game.is_some() {
                f.game.clone_from(&d.game);
            }
            f.title.clone_from(&d.title);
            if let Some(at) = d.at {
                f.captured = at;
            }
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct MedalClip {
    game: Option<String>,
    title: Option<String>,
    at: Option<NaiveDateTime>,
}

/// Medal's clips, by [`path_key`]: every object in the file with a
/// `FilePath`.
fn medal_clips(json: &str) -> HashMap<String, MedalClip> {
    let mut out = HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return out };
    fn visit(v: &serde_json::Value, out: &mut HashMap<String, MedalClip>) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(path) = map.get("FilePath").and_then(|p| p.as_str()) {
                    let text = |k: &str| map.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
                    let at = map.get("createdTimestamp").and_then(|t| t.as_f64()).and_then(|t| {
                        // Milliseconds, or seconds in older files.
                        let ms = if t > 1e11 { t } else { t * 1000.0 };
                        Local.timestamp_millis_opt(ms as i64).single().map(|d| d.naive_local())
                    });
                    let game = text("gameName").or_else(|| text("gameCategory"));
                    out.insert(path_key(Path::new(path)), MedalClip { game, title: text("contentTitle"), at });
                } else {
                    map.values().for_each(|c| visit(c, out));
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|c| visit(c, out)),
            _ => {}
        }
    }
    visit(&v, &mut out);
    out
}

// --- Paths and drives ---

/// A path as compared: one kind of slash, none doubled, none at the end,
/// and on Windows and macOS (whose disks don't mind case) in lower case.
fn path_key(p: &Path) -> String {
    let s = clean(p).to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(target_os = "linux") { s.to_owned() } else { s.to_lowercase() }
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// Whether `p` is `dir` or in it.
pub fn within(p: &Path, dir: &Path) -> bool {
    let (p, dir) = (path_key(p), path_key(dir));
    p == dir || p.strip_prefix(&dir).is_some_and(|rest| rest.starts_with('/'))
}

/// A folder all sorts of things go in (Videos, Movies, home, Desktop,
/// Downloads, Documents): there, only the app's own names are its clips.
fn is_shared(folder: &Path) -> bool {
    [dirs::home_dir(), dirs::video_dir(), dirs::desktop_dir(), dirs::download_dir(), dirs::document_dir()].iter().flatten().any(|d| same_path(d, folder))
}

/// `dir`, or the nearest folder above it that exists (the library's folder
/// may not yet).
fn existing(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|a| a.is_dir())
}

/// The bytes free to us on the drive `dir` is on.
pub fn free_space(dir: &Path) -> Option<u64> {
    let dir = existing(dir)?;
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut free = 0u64;
        // SAFETY: a path and somewhere to put the number.
        unsafe { GetDiskFreeSpaceExW(&windows::core::HSTRING::from(dir.as_os_str()), Some(&mut free), None, None) }.ok()?;
        Some(free)
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: a C string and a struct for the answer, as statvfs takes.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)] // the types differ between macOS and Linux
        Some(s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// The drive `dir` is on, as named to the user: "E:" on Windows.
pub fn drive_name(dir: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        volume(existing(dir)?).map(|v| v.trim_end_matches('\\').to_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

/// The volume a path is on (`E:\`, or a folder a drive is mounted on).
#[cfg(windows)]
fn volume(p: &Path) -> Option<String> {
    use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
    let mut buf = [0u16; 1024];
    // SAFETY: a path and a buffer for the answer.
    unsafe { GetVolumePathNameW(&windows::core::HSTRING::from(p.as_os_str()), &mut buf) }.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Whether two folders are on one drive (so moving between them is a
/// rename). Not known: taken as not.
fn same_drive(a: &Path, b: &Path) -> bool {
    let (Some(a), Some(b)) = (existing(a), existing(b)) else { return false };
    #[cfg(windows)]
    {
        match (volume(a), volume(b)) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y),
            _ => false,
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (a.metadata(), b.metadata()) {
            (Ok(x), Ok(y)) => x.dev() == y.dev(),
            _ => false,
        }
    }
}

/// `%NAME%` replaced with the environment variable's value (Windows
/// settings keep paths like `%USERPROFILE%\Videos`).
fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// All the bytes in the files under `dir`.
fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => tree_bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(windows)]
mod registry {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_NOEXPAND, RRF_RT_ANY, RegGetValueW};
    use windows::core::HSTRING;

    /// A text value under HKEY_CURRENT_USER, as text (`REG_SZ`,
    /// `REG_EXPAND_SZ` unexpanded) or UTF-16 bytes (`REG_BINARY`, as NVIDIA
    /// keeps its folder), up to its first NUL.
    pub fn value(key: &str, name: &str) -> Option<String> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let flags = RRF_RT_ANY | RRF_NOEXPAND;
        let mut size = 0u32;
        // SAFETY: asking for the size only.
        unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, flags, None, None, Some(&mut size)) }.ok().ok()?;
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: the buffer and its size in bytes match.
        unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, flags, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
}

// --- Importing ---

/// Copy the clips into the library (the default), or move them there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Copy,
    Move,
}

/// What Settings says can be imported here.
#[cfg(windows)]
pub const FROM: &str = "From NVIDIA, AMD, Xbox Game Bar, Discord, Medal, OBS, Steam and more, or any folder";
#[cfg(target_os = "macos")]
pub const FROM: &str = "From Discord, OBS or Steam, or any folder";
#[cfg(target_os = "linux")]
pub const FROM: &str = "From OBS, Steam, GPU Screen Recorder or ReplaySorcery, or any folder";

/// Space kept free on the library's drive: an import that would leave less
/// isn't started (clipping needs room too).
pub const KEEP_FREE: u64 = 1 << 30;

/// How an import went.
#[derive(Debug, Default)]
pub struct Done {
    /// The clips added, where they are now.
    pub added: Vec<PathBuf>,
    /// Clips that couldn't be imported, and why.
    pub failed: Vec<(PathBuf, String)>,
    /// Stopped before the end: cancelled.
    pub cancelled: bool,
    /// Stopped before the end: the library's drive is full.
    pub disk_full: bool,
}

/// Why a clip wasn't imported.
enum Failed {
    /// This one only, said why.
    One(String),
    /// The drive's full: the rest won't go either.
    DiskFull,
}

impl From<String> for Failed {
    fn from(s: String) -> Self {
        Failed::One(s)
    }
}

/// An error writing: the drive being full stops the import.
fn writing(e: std::io::Error, what: &str) -> Failed {
    // Windows: ERROR_HANDLE_DISK_FULL (39), ERROR_DISK_FULL (112).
    if e.kind() == std::io::ErrorKind::StorageFull || (cfg!(windows) && matches!(e.raw_os_error(), Some(39 | 112))) {
        Failed::DiskFull
    } else {
        Failed::One(format!("{what}: {e}"))
    }
}

/// The file noting what's been imported, in the library's own folder.
fn ledger_path(lib: &Path) -> PathBuf {
    lib.join(crate::store::DIR).join("imported.json")
}

/// How an imported clip is told apart: where it was, and its size.
pub fn ledger_key(f: &Found) -> String {
    format!("{}|{}", path_key(&f.path), f.bytes)
}

/// What's been imported into this library before ([`ledger_key`]s).
pub fn imported_before(lib: &Path) -> HashSet<String> {
    std::fs::read_to_string(ledger_path(lib)).ok().and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()).unwrap_or_default().into_iter().collect()
}

fn save_ledger(lib: &Path, keys: &HashSet<String>) -> std::io::Result<()> {
    let path = ledger_path(lib);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut list: Vec<&String> = keys.iter().collect();
    list.sort();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

/// The name a clip gets in the library: ours, with the other app's title.
pub fn library_name(f: &Found) -> String {
    let ts = f.captured.format("%Y-%m-%d_%H-%M-%S");
    match f.title.as_deref().map(crate::clips::sanitize_name).filter(|t| !t.is_empty()) {
        Some(t) => format!("clip_{ts} - {}.{}", t.chars().take(80).collect::<String>().trim(), f.ext),
        None => format!("clip_{ts}.{}", f.ext),
    }
}

/// A game's name as matched against the library's folders: its letters
/// and digits, in lower case ("R.E.P.O." and "Repo" are one game).
fn game_key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The library's folders by [`game_key`].
struct Folders {
    lib: PathBuf,
    by_key: HashMap<String, String>,
    per_game: bool,
}

impl Folders {
    fn of(lib: &Path, per_game: bool) -> Self {
        let by_key = crate::clips::folders(lib).into_iter().map(|f| (game_key(&f), f)).filter(|(k, _)| !k.is_empty()).collect();
        Folders { lib: lib.to_path_buf(), by_key, per_game }
    }

    /// Where a clip of this game goes: the folder there already is for it,
    /// else a new one; the library itself for no game (Other), or for every
    /// clip with Folder per game off.
    fn for_game(&mut self, game: Option<&str>) -> PathBuf {
        let Some(game) = game.filter(|_| self.per_game) else { return self.lib.clone() };
        let key = game_key(game);
        let name = match self.by_key.get(&key) {
            Some(f) => f.clone(),
            None => {
                let f = crate::clips::folder_name(game);
                if !key.is_empty() {
                    self.by_key.insert(key, f.clone());
                }
                f
            }
        };
        self.lib.join(name)
    }
}

/// The library's folder a clip of `game` would go in (by name), as
/// [`import`] picks it, for showing beforehand.
pub fn folder_for(lib: &Path, game: Option<&str>, per_game: bool) -> Option<String> {
    let dir = Folders::of(lib, per_game).for_game(game);
    (dir != lib).then(|| dir.file_name().map(|n| n.to_string_lossy().into_owned())).flatten()
}

/// Bring `clips` into the library at `lib`, adding to `progress` (bytes
/// done, of all) as it goes, until `cancel` is set or the drive fills up.
/// `per_game`: the library's Folder per game. Runs on its own thread.
pub fn import(clips: &[Found], lib: &Path, mode: Mode, per_game: bool, progress: &Progress, cancel: &AtomicBool) -> Done {
    progress.1.store(clips.iter().map(|c| c.bytes).sum::<u64>().max(1), Ordering::Relaxed);
    let mut ledger = imported_before(lib);
    let mut folders = Folders::of(lib, per_game);
    let mut done = Done::default();
    for clip in clips {
        if cancel.load(Ordering::Relaxed) {
            done.cancelled = true;
            break;
        }
        let start = progress.0.load(Ordering::Relaxed);
        let dir = folders.for_game(clip.game.as_deref());
        match import_one(clip, &dir, mode, progress, cancel) {
            Ok(Some(path)) => {
                ledger.insert(ledger_key(clip));
                // Noted as each comes in: a crash part way doesn't bring
                // the same clips in again next time.
                if let Err(e) = save_ledger(lib, &ledger) {
                    crate::logfile::tell(&format!("import: couldn't note what was imported: {e}"));
                }
                done.added.push(path);
            }
            Ok(None) => {
                done.cancelled = true;
                break;
            }
            Err(Failed::DiskFull) => {
                crate::logfile::tell(&format!("import: the drive is full; stopped at {}", clip.path.display()));
                done.disk_full = true;
                break;
            }
            Err(Failed::One(e)) => {
                crate::logfile::tell(&format!("import: {} not imported: {e}", clip.path.display()));
                done.failed.push((clip.path.clone(), e));
            }
        }
        // Each clip counts in full once done with, however it went.
        progress.0.store(start + clip.bytes, Ordering::Relaxed);
    }
    done
}

/// One clip into `dir` in the library: where it is now, or `None` if
/// cancelled.
fn import_one(clip: &Found, dir: &Path, mode: Mode, progress: &Progress, cancel: &AtomicBool) -> Result<Option<PathBuf>, Failed> {
    std::fs::create_dir_all(dir).map_err(|e| writing(e, "couldn't make its folder"))?;
    let dest = crate::clips::free_path(dir, &library_name(clip));
    // Written to a hidden file first, so a half-copied clip never shows.
    let tmp = dir.join(format!(".import-{}.{}", uuid::Uuid::new_v4().simple(), clip.ext));
    let when = Local.from_local_datetime(&clip.captured).earliest().map(SystemTime::from);
    let moving = mode == Mode::Move && !clip.steam;
    // Moving within a drive: a rename, nothing copied or deleted.
    if moving && std::fs::rename(&clip.path, &dest).is_ok() {
        set_date(&dest, when);
        return Ok(Some(dest));
    }
    let made = if clip.steam {
        join_steam(&clip.path, &tmp).map(|()| true)
    } else {
        copy_file(&clip.path, &tmp, moving, progress, cancel)
    };
    match made {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(&tmp);
            return Ok(None);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    set_date(&tmp, when);
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(writing(e, "couldn't put it in the library"));
    }
    // Moving to another drive: the original goes only now, its copy whole
    // and checked byte for byte (`copy_file`).
    if moving && let Err(e) = std::fs::remove_file(&clip.path) {
        crate::logfile::tell(&format!("import: copied {} but couldn't remove the original: {e}", clip.path.display()));
    }
    Ok(Some(dest))
}

/// The file's modified time set to when the clip was captured (its date in
/// the library, when its name doesn't say).
fn set_date(path: &Path, when: Option<SystemTime>) {
    if let Some(t) = when
        && let Ok(f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let _ = f.set_modified(t);
    }
}

/// Copy `from` to `to`, adding to `progress` as it goes: `false` if
/// cancelled. `check`: read the copy back afterwards and compare it with
/// what was read of the original (for a move, before the original goes).
fn copy_file(from: &Path, to: &Path, check: bool, progress: &Progress, cancel: &AtomicBool) -> Result<bool, Failed> {
    let mut src = std::fs::File::open(from).map_err(|e| format!("couldn't open it: {e}"))?;
    let mut dst = std::fs::File::create(to).map_err(|e| writing(e, "couldn't write the copy"))?;
    let mut buf = vec![0u8; 4 << 20];
    let mut read_hash = std::hash::DefaultHasher::new();
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let n = src.read(&mut buf).map_err(|e| format!("couldn't read it: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).map_err(|e| writing(e, "couldn't write the copy"))?;
        if check {
            read_hash.write(&buf[..n]);
        }
        total += n as u64;
        progress.0.fetch_add(n as u64, Ordering::Relaxed);
    }
    dst.sync_all().map_err(|e| writing(e, "couldn't write the copy"))?;
    drop(dst);
    if check {
        let mut copy = std::fs::File::open(to).map_err(|e| format!("couldn't read the copy back: {e}"))?;
        let mut copy_hash = std::hash::DefaultHasher::new();
        let mut copied = 0u64;
        loop {
            let n = copy.read(&mut buf).map_err(|e| format!("couldn't read the copy back: {e}"))?;
            if n == 0 {
                break;
            }
            copy_hash.write(&buf[..n]);
            copied += n as u64;
        }
        if copied != total || copy_hash.finish() != read_hash.finish() {
            return Err(Failed::One("its copy didn't match it, so it was left where it was".into()));
        }
    }
    Ok(true)
}

/// A Steam clip's pieces joined into one MP4 at `out`: each stream's
/// `init-stream<n>.m4s` and then its `chunk-stream<n>-<k>.m4s` in order make
/// a fragmented MP4, and ffmpeg puts the two (video, sound) in one file
/// without re-encoding.
fn join_steam(clip: &Path, out: &Path) -> Result<(), Failed> {
    let dir = steam_stream_dir(clip).ok_or_else(|| "its video wasn't found".to_owned())?;
    let parent = out.parent().ok_or_else(|| "no folder".to_owned())?;
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut parts = Vec::new();
    for n in 0..2 {
        let init = dir.join(format!("init-stream{n}.m4s"));
        if !init.is_file() {
            continue;
        }
        let part = parent.join(format!("{stem}.stream{n}.mp4"));
        let joined = join_stream(&dir, n, &init, &part);
        parts.push(part);
        if let Err(e) = joined {
            parts.iter().for_each(|p| drop(std::fs::remove_file(p)));
            return Err(e);
        }
    }
    let mut cmd = media::ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    for p in &parts {
        cmd.arg("-i").arg(p);
    }
    for i in 0..parts.len() {
        cmd.args(["-map", &i.to_string()]);
    }
    cmd.args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"]).arg(out).stdin(std::process::Stdio::null());
    let result = cmd.output();
    parts.iter().for_each(|p| drop(std::fs::remove_file(p)));
    match result {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("ffmpeg couldn't join its pieces: {}", String::from_utf8_lossy(&o.stderr).trim()).into()),
        Err(e) => Err(format!("ffmpeg couldn't be run: {e}").into()),
    }
}

/// One stream's pieces, in order, into one file.
fn join_stream(dir: &Path, n: usize, init: &Path, out: &Path) -> Result<(), Failed> {
    let prefix = format!("chunk-stream{n}-");
    let mut chunks: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let k = name.strip_prefix(&prefix)?.strip_suffix(".m4s")?.parse().ok()?;
            Some((k, e.path()))
        })
        .collect();
    chunks.sort();
    let mut file = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| writing(e, "couldn't write its video"))?);
    for p in std::iter::once(init.to_path_buf()).chain(chunks.into_iter().map(|(_, p)| p)) {
        let mut src = std::fs::File::open(&p).map_err(|e| e.to_string())?;
        std::io::copy(&mut src, &mut file).map_err(|e| writing(e, "couldn't write its video"))?;
    }
    file.flush().map_err(|e| writing(e, "couldn't write its video"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Option<NaiveDateTime> {
        Some(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap())
    }

    fn p(game: Option<&str>, when: &str, pattern: Pattern) -> Parsed {
        Parsed { game: game.map(str::to_owned), at: at(when), pattern }
    }

    #[test]
    fn nvidia_names() {
        assert_eq!(parse_stem("VALORANT 2024.01.02 - 12.34.56.78.DVR"), p(Some("VALORANT"), "2024-01-02 12:34:56", Pattern::Nvidia));
        assert_eq!(parse_stem("Counter-strike 2 2023.11.30 - 23.59.01.02"), p(Some("Counter-strike 2"), "2023-11-30 23:59:01", Pattern::Nvidia));
        assert_eq!(parse_stem("Desktop 2024.05.06 - 07.08.09.10.DVR"), p(Some("Desktop"), "2024-05-06 07:08:09", Pattern::Nvidia));
        // Apps NVIDIA had no name for: no game.
        assert_eq!(parse_stem("56e53accb20696f802d92bd011174126b5e3154e 2025.08.09 - 17.26.25.02.DVR"), p(None, "2025-08-09 17:26:25", Pattern::Nvidia));
        assert_eq!(parse_stem("Base Profile 2025.08.13 - 22.26.42.02.DVR"), p(None, "2025-08-13 22:26:42", Pattern::Nvidia));
    }

    #[test]
    fn amd_names() {
        assert_eq!(parse_stem("Apex Legends_replay_2024.03.04-21.05.33"), p(Some("Apex Legends"), "2024-03-04 21:05:33", Pattern::Amd));
        assert_eq!(parse_stem("Fortnite_record_2024.03.04-21.05"), p(Some("Fortnite"), "2024-03-04 21:05:00", Pattern::Amd));
    }

    #[test]
    fn spaced_names() {
        // Game Bar
        assert_eq!(parse_stem("Minecraft 2024-02-03 10-11-12"), p(Some("Minecraft"), "2024-02-03 10:11:12", Pattern::Spaced));
        // OBS: no game.
        assert_eq!(parse_stem("Replay 2024-02-03 10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Spaced));
        assert_eq!(parse_stem("2024-02-03 10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Spaced));
        // GPU Screen Recorder named after the game.
        assert_eq!(parse_stem("Hollow Knight - 2024-02-03 10-11-12"), p(Some("Hollow Knight"), "2024-02-03 10:11:12", Pattern::Spaced));
    }

    #[test]
    fn underscore_names() {
        assert_eq!(parse_stem("Replay_2024-02-03_10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Underscore));
        assert_eq!(parse_stem("Video_2024-02-03_10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Underscore));
        assert_eq!(parse_stem("2024-02-03_10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Underscore));
        assert_eq!(parse_stem("clip_2024-02-03_10-11-12"), p(None, "2024-02-03 10:11:12", Pattern::Own));
    }

    #[test]
    fn outplayed_and_medal_names() {
        assert_eq!(parse_stem("League of Legends 04-05-2024_19-20-21-123"), p(Some("League of Legends"), "2024-04-05 19:20:21", Pattern::Outplayed));
        assert_eq!(parse_stem("MedalTVVALORANT20241122210533"), p(Some("VALORANT"), "2024-11-22 21:05:33", Pattern::Medal));
    }

    #[test]
    fn discord_names() {
        // A snowflake id holds when it was made: 2023-11-15, 18:26 UTC.
        let old = parse_stem("Fortnite_1174449235970490368");
        assert_eq!((old.game.as_deref(), old.pattern), (Some("Fortnite"), Pattern::Discord));
        let utc = DateTime::<chrono::Utc>::from_timestamp_millis(((1174449235970490368u64 >> 22) + 1_420_070_400_000) as i64).unwrap();
        assert_eq!(old.at, Some(utc.with_timezone(&Local).naive_local()));
        assert_eq!(utc.format("%Y-%m-%d").to_string(), "2023-11-15");
        // A UUID doesn't: the file's date is used.
        let new = parse_stem("Counter-Strike_2_01dfc870-a158-4b21-8d4a-aebe6c6299ae");
        assert_eq!(new, Parsed { game: Some("Counter-Strike 2".into()), at: None, pattern: Pattern::Discord });
        assert_eq!(parse_stem("Lethal_Company_1176970350282407936").game.as_deref(), Some("Lethal Company"));
        // A screen, not a game.
        assert_eq!(parse_stem("Screen_1_1174449235970490368").game.as_deref(), Some("Desktop"));
        // Not Discord's.
        assert_eq!(parse_stem("IMG_1234").pattern, Pattern::None);
    }

    #[test]
    fn other_names() {
        assert_eq!(parse_stem("my best clip"), Parsed { game: None, at: None, pattern: Pattern::None });
        assert_eq!(parse_stem("2024-99-03 10-11-12"), Parsed { game: None, at: None, pattern: Pattern::None });
        assert_eq!(parse_stem("ü 2024-02-03 10-11-12").game.as_deref(), Some("ü"));
    }

    #[test]
    fn shared_folders_only_give_the_apps_names() {
        let nv = parse_stem("VALORANT 2024.01.02 - 12.34.56.78.DVR");
        assert!(AppKind::Nvidia.names(nv.pattern, nv.game.is_some()));
        assert!(!AppKind::Obs.names(nv.pattern, nv.game.is_some()));
        let gb = parse_stem("Minecraft 2024-02-03 10-11-12");
        assert!(AppKind::GameBar.names(gb.pattern, true));
        assert!(!AppKind::Obs.names(gb.pattern, true));
        let obs = parse_stem("Replay 2024-02-03 10-11-12");
        assert!(AppKind::Obs.names(obs.pattern, obs.game.is_some()));
        assert!(!AppKind::Nvidia.names(Pattern::None, false));
    }

    #[test]
    fn game_folders() {
        assert!(is_game_folder("VALORANT"));
        assert!(!is_game_folder("2024-02-03"));
        assert!(!is_game_folder("Clips"));
        assert!(!is_game_folder("56e53accb20696f802d92bd011174126b5e3154e"));
        assert!(!is_game_folder("Base Profile"));
        let root = Path::new("/v");
        assert_eq!(folder_game(root, Path::new("/v/NvidiaClips/osu!/a.mp4")).as_deref(), Some("osu!"));
        assert_eq!(folder_game(root, Path::new("/v/VALORANT/2024-05-01/a.mp4")).as_deref(), Some("VALORANT"));
        assert_eq!(folder_game(root, Path::new("/v/a.mp4")), None);
    }

    #[test]
    fn paths_from_settings_files() {
        assert!(same_path(Path::new(r"E:\\Videos"), Path::new(r"E:\Videos")));
        assert!(same_path(Path::new("E:/Videos/obs_raw/"), Path::new(r"e:\videos\obs_raw")) || cfg!(target_os = "linux"));
        assert!(within(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(within(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!within(Path::new("/a/bc"), Path::new("/a/b")));
    }

    #[test]
    fn settings_files() {
        assert_eq!(nvidia_gallery(r#"{"settings":{"currentDirectoryV2":"D:\\Clips"}}"#), Some(PathBuf::from(r"D:\Clips")));
        assert_eq!(medal_folder(r#"{"recorder":{"clipFolder":"E:\\Medal"}}"#), Some(PathBuf::from(r"E:\Medal")));
        assert_eq!(medal_folder(r#"{"a":{"b":{"clipFolder":"F:\\M"}}}"#), Some(PathBuf::from(r"F:\M")));
        let xml = "<Settings><Setting><Value>D:\\Videos\\Overwolf &amp; co</Value><Name>Folders_VideoCapturesFolder</Name></Setting></Settings>";
        assert_eq!(overwolf_folder(xml), Some(PathBuf::from(r"D:\Videos\Overwolf & co")));
        assert_eq!(gsr_folders("replay.save_directory /home/a/Clips\nscreenshot.save_directory /home/a/Pics\nrecord.save_directory /home/a/Rec\n"), vec![PathBuf::from("/home/a/Clips"), PathBuf::from("/home/a/Rec")]);
        assert_eq!(replay_sorcery_folder("outputFile = ~/Videos/ReplaySorcery/%F_%H-%M-%S.mp4\n", Some(Path::new("/home/a"))), Some(PathBuf::from("/home/a/Videos/ReplaySorcery")));
        assert_eq!(replay_sorcery_folder("outputFile = /data/rs/%Y/%F.mp4\n", None), Some(PathBuf::from("/data/rs")));
        let ini = "[Output]\nMode=Advanced\n[SimpleOutput]\nFilePath=C:/Users/a/Videos\n[AdvOut]\nRecFilePath=D:/OBS\nRecType=Standard\n";
        assert_eq!(obs_profile_folders(ini), vec![PathBuf::from("C:/Users/a/Videos"), PathBuf::from("D:/OBS")]);
        let vdf = "\"UserLocalConfigStore\"\n{\n\t\"GameRecording\"\n\t{\n\t\t\"BackgroundRecordPath\"\t\t\"D:\\\\Steam Clips\"\n\t}\n}\n";
        assert_eq!(vdf_value(vdf, "BackgroundRecordPath").as_deref(), Some(r"D:\Steam Clips"));
        assert_eq!(parse_steam_clip("clip_730_20240102_123456"), Some(("730".to_owned(), at("2024-01-02 12:34:56").unwrap())));
        assert_eq!(parse_steam_clip("bg_730_20240102_123456"), None);
        // Discord's local storage: binary around it, the newest mention last.
        let mut db = b"\x00\x01junk{\"clipsSettings\":{\"storageLocation\":\"C:\\\\Old\"}}\x07".to_vec();
        db.extend_from_slice(b"\x00{\"_state\":{\"clipsSettings\":{\"clipsEnabled\":false,\"storageLocation\":\"E:\\\\Videos\\\\Discord Clips\",\"clipsLength\":30000}}}\xff");
        assert_eq!(storage_location(&db), Some(PathBuf::from(r"E:\Videos\Discord Clips")));
        assert_eq!(storage_location(b"nothing here"), None);
    }

    #[test]
    fn medal_list() {
        let json = r#"{"a1b2":{"FilePath":"C:\\Medal\\Clips\\VALORANT\\MedalTVVALORANT20241122210533.mp4","gameName":"VALORANT","contentTitle":"Ace on Ascent","createdTimestamp":1732309533000},"x":{"FilePath":"C:\\Medal\\Clips\\b.mp4","contentTitle":""}}"#;
        let clips = medal_clips(json);
        let ace = &clips[&path_key(Path::new(r"C:\Medal\Clips\VALORANT\MedalTVVALORANT20241122210533.mp4"))];
        assert_eq!(ace.game.as_deref(), Some("VALORANT"));
        assert_eq!(ace.title.as_deref(), Some("Ace on Ascent"));
        assert!(ace.at.is_some());
        assert_eq!(clips[&path_key(Path::new(r"C:\Medal\Clips\b.mp4"))], MedalClip::default());
    }

    #[test]
    fn vars_expand() {
        let home = std::env::var("PATH").unwrap_or_default();
        assert_eq!(expand_vars("%PATH%\\x"), format!("{home}\\x"));
        assert_eq!(expand_vars("100%"), "100%");
        assert_eq!(expand_vars("%NO_SUCH_VAR_HC%"), "%NO_SUCH_VAR_HC%");
    }

    #[test]
    fn games_find_the_librarys_folders() {
        assert_eq!(game_key("R.E.P.O."), game_key("Repo"));
        assert_eq!(game_key("Counter-Strike 2"), game_key("counter strike 2"));
        let root = test_dir("folders");
        // (Windows drops a folder name's last dot.)
        std::fs::create_dir_all(root.join("R.E.P.O")).unwrap();
        std::fs::create_dir_all(root.join(".hasteclip")).unwrap();
        assert_eq!(folder_for(&root, Some("Repo"), true).as_deref(), Some("R.E.P.O"));
        assert_eq!(folder_for(&root, Some("Phasmophobia"), true).as_deref(), Some("Phasmophobia"));
        assert_eq!(folder_for(&root, None, true), None);
        assert_eq!(folder_for(&root, Some("Repo"), false), None, "Folder per game off: all in the library itself");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn found(path: &Path, game: Option<&str>, title: Option<&str>) -> Found {
        Found {
            path: path.to_path_buf(),
            folder: path.parent().unwrap_or(Path::new("")).to_path_buf(),
            game: game.map(str::to_owned),
            captured: at("2024-01-02 12:34:56").unwrap(),
            title: title.map(str::to_owned),
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            ext: "mp4".into(),
            steam: false,
            on_library_drive: true,
        }
    }

    #[test]
    fn names_in_the_library() {
        let f = found(Path::new("x.mp4"), None, Some("Ace: on Ascent"));
        assert_eq!(library_name(&f), "clip_2024-01-02_12-34-56 - Ace. on Ascent.mp4");
        assert_eq!(crate::clips::title_for_stem("clip_2024-01-02_12-34-56 - Ace. on Ascent"), "Ace. on Ascent · 12:34:56");
        assert_eq!(library_name(&found(Path::new("x.mp4"), None, None)), "clip_2024-01-02_12-34-56.mp4");
    }

    #[test]
    fn space_needed() {
        let mut f = found(Path::new("x.mp4"), None, None);
        f.bytes = 100;
        assert_eq!(f.needs(Mode::Copy), 100);
        assert_eq!(f.needs(Mode::Move), 0, "moving within a drive is a rename");
        f.on_library_drive = false;
        assert_eq!(f.needs(Mode::Move), 100);
        f.on_library_drive = true;
        f.steam = true;
        assert_eq!(f.needs(Mode::Move), 100, "Steam's are always copied");
        let here = std::env::temp_dir();
        assert!(free_space(&here).is_some_and(|b| b > 0));
        assert!(free_space(&here.join("no such folder").join("deeper")).is_some(), "the nearest folder that exists");
        assert!(same_drive(&here, &here.join("no such folder")));
    }

    /// A folder of its own for a test, made empty.
    fn test_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hc-import-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_an_apps_folder() {
        let root = test_dir("scan");
        let app = root.join("Radeon ReLive");
        std::fs::create_dir_all(app.join("Apex Legends")).unwrap();
        std::fs::create_dir_all(app.join("temp-capture")).unwrap();
        std::fs::write(app.join("Apex Legends").join("Apex Legends_replay_2024.03.04-21.05.33.mp4"), b"v").unwrap();
        std::fs::write(app.join("Apex Legends").join("renamed by me.mp4"), b"v").unwrap();
        std::fs::write(app.join("Apex Legends").join("thumb.jpg"), b"i").unwrap();
        std::fs::write(app.join("temp-capture").join("x.mp4"), b"v").unwrap();
        std::fs::write(app.join(".hidden.mp4"), b"v").unwrap();
        // The library inside the app's folder is never looked in.
        let lib = app.join("HasteClip");
        std::fs::create_dir_all(lib.join("Apex Legends")).unwrap();
        std::fs::write(lib.join("Apex Legends").join("Apex Legends_replay_2024.03.04-21.05.34.mp4"), b"v").unwrap();
        let src = Source { kind: AppKind::Amd, folders: vec![app.clone()] };
        let found = scan(&src, &[], &lib);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.game.as_deref() == Some("Apex Legends")));
        let named = found.iter().find(|f| f.path.file_name().unwrap().to_string_lossy().contains("replay")).unwrap();
        assert_eq!(named.captured, at("2024-03-04 21:05:33").unwrap());
        assert!(named.on_library_drive);
        // An app with a folder inside another of its folders (NVIDIA:
        // Videos\NVIDIA and Videos) finds the clips in both, each once.
        let videos = root.join("Videos");
        std::fs::create_dir_all(videos.join("NVIDIA").join("osu!")).unwrap();
        std::fs::create_dir_all(videos.join("osu!")).unwrap();
        std::fs::write(videos.join("NVIDIA").join("osu!").join("osu! 2025.08.08 - 18.09.11.02.mp4"), b"v").unwrap();
        std::fs::write(videos.join("osu!").join("osu! 2024.08.08 - 18.09.11.02.mp4"), b"v").unwrap();
        let nv = Source { kind: AppKind::Nvidia, folders: vec![videos.join("NVIDIA"), videos.clone()] };
        let all = nv.folders.clone();
        let found = scan(&nv, &all, &lib);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.game.as_deref() == Some("osu!")));
        // A folder in the library gives nothing.
        assert!(scan(&Source { kind: AppKind::Folder, folders: vec![lib.join("Apex Legends")] }, &[], &lib).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copies_and_moves_into_the_library() {
        let root = test_dir("copy");
        let (from, lib) = (root.join("from"), root.join("lib"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(lib.join("Counter Strike 2")).unwrap();
        let a = from.join("a.mp4");
        let b = from.join("b.mp4");
        std::fs::write(&a, vec![1u8; 1000]).unwrap();
        std::fs::write(&b, vec![2u8; 500]).unwrap();
        let clips = vec![found(&a, Some("Counter-Strike: 2"), None), found(&b, None, Some("Nice"))];
        let progress = Progress::default();
        let done = import(&clips, &lib, Mode::Copy, true, &progress, &AtomicBool::new(false));
        assert_eq!(done.added.len(), 2, "{:?}", done.failed);
        assert!(a.exists() && b.exists(), "copying keeps the originals");
        // Into the folder the library already has for the game.
        let copied = lib.join("Counter Strike 2").join("clip_2024-01-02_12-34-56.mp4");
        assert_eq!(std::fs::read(&copied).unwrap(), vec![1u8; 1000]);
        assert!(lib.join("clip_2024-01-02_12-34-56 - Nice.mp4").exists());
        assert_eq!(progress.0.load(Ordering::Relaxed), 1500);
        // Dated when captured.
        let modified = DateTime::<Local>::from(std::fs::metadata(&copied).unwrap().modified().unwrap()).naive_local();
        assert_eq!(modified, at("2024-01-02 12:34:56").unwrap());
        // Noted, so they're known next time.
        let before = imported_before(&lib);
        assert!(clips.iter().all(|c| before.contains(&ledger_key(c))));
        // The scan sees them as clips, in their game's folder.
        let scanned = crate::clips::scan(&lib);
        assert_eq!(scanned.len(), 2);
        // Moving (on one drive: a rename) takes the original away, and a
        // name taken gets " (2)".
        let done = import(&clips[..1], &lib, Mode::Move, true, &Progress::default(), &AtomicBool::new(false));
        assert_eq!(done.added, vec![lib.join("Counter Strike 2").join("clip_2024-01-02_12-34-56 (2).mp4")]);
        assert!(!a.exists());
        // A move between drives copies, checks and only then deletes: the
        // copy-and-check part, here.
        let c = from.join("c.mp4");
        std::fs::write(&c, vec![3u8; 9_000_000]).unwrap();
        let out = lib.join(".check.mp4");
        assert!(matches!(copy_file(&c, &out, true, &Progress::default(), &AtomicBool::new(false)), Ok(true)));
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 9_000_000);
        // Cancelled before it starts: nothing done.
        let done = import(&clips[1..], &lib, Mode::Copy, true, &Progress::default(), &AtomicBool::new(true));
        assert!(done.cancelled && done.added.is_empty());
        // No hidden half-copies left behind.
        assert!(std::fs::read_dir(&lib).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".import-")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What importing would bring in from this computer, read-only: each
    /// app's folders, its clips by game and the library folder they'd go
    /// in, where their dates came from, and a few names as read. For
    /// checking the scan on a real machine (any OS):
    /// `cargo test -p hasteclip dry_run_here -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dry_run_here() {
        let settings = crate::settings::RecordSettings::load();
        let lib = settings.output_dir.clone();
        println!("library: {} (free: {:?} bytes)", lib.display(), free_space(&lib));
        let sources = detect();
        let folders: Vec<PathBuf> = sources.iter().flat_map(|s| s.folders.clone()).collect();
        for src in &sources {
            let clips = scan(src, &folders, &lib);
            let moved_free = clips.iter().filter(|c| c.needs(Mode::Move) == 0).count();
            println!("\n## {} ({} clips, {}; {moved_free} would move without a copy)", src.kind.name(), clips.len(), crate::clips::human_bytes(clips.iter().map(|c| c.bytes).sum()));
            for f in &src.folders {
                let n = clips.iter().filter(|c| c.folder == *f).count();
                println!("  folder {} : {n}", f.display());
            }
            let mut games: std::collections::BTreeMap<String, usize> = Default::default();
            for c in &clips {
                let into = folder_for(&lib, c.game.as_deref(), settings.folder_per_game).unwrap_or_else(|| "(library itself: Other)".into());
                *games.entry(format!("{:?} -> {into}", c.game)).or_default() += 1;
            }
            for (g, n) in &games {
                println!("  game {g} : {n}");
            }
            let mut by_pattern: std::collections::BTreeMap<String, Vec<&Found>> = Default::default();
            for c in &clips {
                let stem = c.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let parsed = parse_stem(&stem);
                by_pattern.entry(format!("{:?}, date from {}", parsed.pattern, if parsed.at.is_some() { "name" } else { "file time" })).or_default().push(c);
            }
            for (pat, list) in &by_pattern {
                println!("  pattern {pat} : {}", list.len());
                for c in list.iter().take(3) {
                    println!("    {} -> game {:?}, {} -> {}", c.path.display(), c.game, c.captured, library_name(c));
                }
            }
        }
    }

    /// Steam's pieces, made by ffmpeg's own DASH writer (Steam's are named
    /// the same), are joined back into one MP4. Skipped without ffmpeg.
    #[test]
    fn joins_steam_pieces() {
        let root = test_dir("steam");
        let clip = root.join("clips").join("clip_730_20240102_123456");
        let pieces = clip.join("video").join("bg_730_20240102_123000");
        std::fs::create_dir_all(&pieces).unwrap();
        let made = media::ffmpeg()
            .current_dir(&pieces)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=s=160x90:r=30:d=3", "-f", "lavfi", "-i", "sine=d=3"])
            .args(["-c:v", "mpeg4", "-c:a", "aac", "-seg_duration", "1", "-f", "dash", "session.mpd"])
            .stdin(std::process::Stdio::null())
            .output();
        if !made.is_ok_and(|o| o.status.success()) || !pieces.join("init-stream0.m4s").exists() {
            eprintln!("no ffmpeg with DASH here: skipped");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let lib = root.join("lib");
        let found = scan(&Source { kind: AppKind::Steam, folders: vec![root.clone()] }, &[], &lib);
        assert_eq!(found.len(), 1);
        assert!(found[0].steam && found[0].captured == at("2024-01-02 12:34:56").unwrap());
        let done = import(&found, &lib, Mode::Move, true, &Progress::default(), &AtomicBool::new(false));
        assert_eq!(done.added.len(), 1, "{:?}", done.failed);
        let info = media::probe(&done.added[0]).expect("a video");
        assert!(info.duration > 2.5, "{}", info.duration);
        assert!(clip.exists(), "Steam's own are never moved");
        let _ = std::fs::remove_dir_all(&root);
    }
}
