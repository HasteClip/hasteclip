//! Clips from other clipping apps, brought into the library (#75): for
//! someone switching to HasteClip, so all their clips are in one place.
//!
//! Each app is found by where it keeps its clips: its own settings say
//! where (the registry, a JSON or INI file), else its default folder. A clip
//! goes into the library's folder for its game, named as our own clips are
//! (`clip_YYYY-MM-DD_HH-MM-SS`, with a title the other app had after
//! ` - `), and dated when it was captured. Its game and date come from, in
//! order: the app's own list of clips (Medal), the game folder it's in, its
//! file name (each app's pattern, see [`parse_stem`]), else the file's date.
//!
//! | App | OS | Where | Names |
//! |---|---|---|---|
//! | NVIDIA (GeForce Experience, NVIDIA App) | Windows | `NVSPCAPS\DefaultPathW`, `GallerySettings.json`; Videos | `<Game> 2024.01.02 - 12.34.56.78.DVR.mp4` in `<Game>\` |
//! | AMD Adrenalin (ReLive) | Windows | `HKCU\Software\AMD\DVR\CaptureFileOutput`; Videos\Radeon ReLive | `<Game>_replay_2024.01.02-12.34.56.mp4` in `<Game>\` |
//! | Xbox Game Bar | Windows | the Captures known folder | `<Game> 2024-01-02 12-34-56.mp4` |
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
//! that app's pattern, so OBS doesn't take every video in Videos, and a
//! folder another app found is left to it.
//!
//! Clips are copied, or moved when asked to (only after the copy is
//! complete; a move on one drive is a rename). Steam's are always copied:
//! they're pieces Steam keeps its own index of. What's been imported is
//! noted (`.hasteclip/imported.json`), so importing again only brings what's
//! new.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};

use crate::archive::Progress;

/// An app clips can be imported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// (Each OS has some of them: the others are never made there.)
#[allow(dead_code)]
pub enum AppKind {
    Nvidia,
    Amd,
    GameBar,
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
pub const HERE: &[AppKind] = &[AppKind::Nvidia, AppKind::Amd, AppKind::GameBar, AppKind::Medal, AppKind::Outplayed, AppKind::SteelSeries, AppKind::Steam, AppKind::Obs];
#[cfg(target_os = "macos")]
pub const HERE: &[AppKind] = &[AppKind::Steam, AppKind::Obs];
#[cfg(target_os = "linux")]
pub const HERE: &[AppKind] = &[AppKind::Steam, AppKind::Obs, AppKind::GpuScreenRecorder, AppKind::ReplaySorcery];

impl AppKind {
    pub fn name(self) -> &'static str {
        match self {
            AppKind::Nvidia => "NVIDIA",
            AppKind::Amd => "AMD Adrenalin",
            AppKind::GameBar => "Xbox Game Bar",
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
    /// folder per game, 2 a folder further.
    fn depth(self) -> usize {
        match self {
            AppKind::GameBar | AppKind::Obs | AppKind::ReplaySorcery => 0,
            AppKind::Nvidia | AppKind::GpuScreenRecorder => 1,
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
    pub game: Option<String>,
    pub captured: NaiveDateTime,
    /// A title given to it in the other app.
    pub title: Option<String>,
    pub bytes: u64,
    /// The extension it'll have here (without the dot).
    pub ext: String,
    /// A Steam clip: pieces to join into one video.
    pub steam: bool,
}

/// The apps found here with somewhere they keep clips. Reads their settings:
/// call it off the UI thread.
pub fn detect() -> Vec<Source> {
    HERE.iter()
        .filter_map(|&kind| {
            let mut folders: Vec<PathBuf> = Vec::new();
            for f in folders_of(kind) {
                if f.is_dir() && !folders.iter().any(|g| same_path(g, &f)) {
                    folders.push(f);
                }
            }
            (!folders.is_empty()).then_some(Source { kind, folders })
        })
        .collect()
}

/// The clips in a source's folders, newest first. `others` are the other
/// sources' folders, left to them. Reads every folder: off the UI thread.
pub fn scan(src: &Source, others: &[PathBuf]) -> Vec<Found> {
    let mut found = Vec::new();
    for folder in &src.folders {
        if src.kind == AppKind::Steam {
            found.extend(scan_steam(folder));
            continue;
        }
        let strict = is_shared(folder);
        let skip: Vec<&PathBuf> = others.iter().filter(|o| !same_path(o, folder)).collect();
        walk(src.kind, folder, folder, src.kind.depth(), strict, &skip, &mut found);
    }
    if src.kind == AppKind::Medal {
        medal_details(&mut found);
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.captured));
    found
}

/// Clips in `dir` (`root` or under it), down `depth` more folders.
fn walk(kind: AppKind, root: &Path, dir: &Path, depth: usize, strict: bool, skip: &[&PathBuf], out: &mut Vec<Found>) {
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
            if depth > 0 && !SKIP_DIRS.contains(&lower.as_str()) && !skip.iter().any(|s| same_path(s, &path)) {
                walk(kind, root, &path, depth - 1, strict, skip, out);
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
        let folder_game = path.parent().and_then(|p| p.strip_prefix(root).ok()).and_then(|rel| rel.components().next()).map(|c| c.as_os_str().to_string_lossy().into_owned()).filter(|n| is_game_folder(n));
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Found {
            game: folder_game.or(parsed.game),
            captured: parsed.at.unwrap_or_else(|| DateTime::<Local>::from(modified).naive_local()),
            title: None,
            bytes: meta.len(),
            ext: path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "mp4".into()),
            steam: false,
            path,
        });
    }
}

/// Folders that hold clips but aren't a game, or hold nothing finished.
const SKIP_DIRS: &[&str] = &["temp-capture", "tmp", "temp", "thumbnails", "thumbs", "cache"];

/// Whether a folder clips are in names their game: not a date (GPU Screen
/// Recorder's folder per day) or one of the generic names apps use.
fn is_game_folder(name: &str) -> bool {
    const GENERIC: &[&str] = &["clips", "replays", "replay", "recordings", "videos", "highlights", "captures", "instant replays", "outplayed", "medal"];
    let lower = name.to_lowercase();
    if GENERIC.contains(&lower.as_str()) {
        return false;
    }
    let date = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d").is_ok() || chrono::NaiveDate::parse_from_str(name, "%Y.%m.%d").is_ok() || chrono::NaiveDate::parse_from_str(name, "%m-%d-%Y").is_ok();
    !date
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
        if pre.ends_with("clip_") && pre.len() == 5 {
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
    Parsed { game: None, at: None, pattern: Pattern::None }
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
/// one: not empty, and not a word apps put there themselves.
fn game_of(prefix: &str) -> Option<String> {
    const NOT_GAMES: &[&str] = &["replay", "video", "clip", "recording", "screenshot", "instant replay", "desktop"];
    let name = prefix.trim_end_matches([' ', '-', '_', '.']).trim();
    if name.is_empty() || NOT_GAMES.contains(&name.to_lowercase().as_str()) {
        return if name.eq_ignore_ascii_case("desktop") { Some(crate::clips::DESKTOP.to_owned()) } else { None };
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
            Some(Found { game: names.get(&app).cloned(), captured: at, title: None, bytes: tree_bytes(&path), ext: "mp4".into(), steam: true, path })
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

// --- Paths ---

/// A path as compared: forward slashes, no slash at the end, and on
/// Windows and macOS (whose disks don't mind case) in lower case.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(target_os = "linux") { s.to_owned() } else { s.to_lowercase() }
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// A folder all sorts of things go in (Videos, Movies, home, Desktop,
/// Downloads, Documents): there, only the app's own names are its clips.
fn is_shared(folder: &Path) -> bool {
    [dirs::home_dir(), dirs::video_dir(), dirs::desktop_dir(), dirs::download_dir(), dirs::document_dir()].iter().flatten().any(|d| same_path(d, folder))
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

/// How an import went.
#[derive(Debug, Default)]
pub struct Done {
    /// The clips added, where they are now.
    pub added: Vec<PathBuf>,
    /// Clips that couldn't be imported, and why.
    pub failed: Vec<(PathBuf, String)>,
    /// Stopped before the end.
    pub cancelled: bool,
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

/// Bring `clips` into the library at `lib`, adding to `progress` (bytes
/// done, of all) as it goes, until `cancel` is set. Runs on its own thread.
pub fn import(clips: &[Found], lib: &Path, mode: Mode, progress: &Progress, cancel: &AtomicBool) -> Done {
    progress.1.store(clips.iter().map(|c| c.bytes).sum::<u64>().max(1), Ordering::Relaxed);
    let mut ledger = imported_before(lib);
    let mut done = Done::default();
    for clip in clips {
        if cancel.load(Ordering::Relaxed) {
            done.cancelled = true;
            break;
        }
        let start = progress.0.load(Ordering::Relaxed);
        match import_one(clip, lib, mode, progress, cancel) {
            Ok(Some(path)) => {
                ledger.insert(ledger_key(clip));
                done.added.push(path);
            }
            Ok(None) => {
                done.cancelled = true;
                break;
            }
            Err(e) => {
                crate::logfile::tell(&format!("import: {} not imported: {e}", clip.path.display()));
                done.failed.push((clip.path.clone(), e));
            }
        }
        // Each clip counts in full once done with, however it went.
        progress.0.store(start + clip.bytes, Ordering::Relaxed);
    }
    if let Err(e) = save_ledger(lib, &ledger) {
        crate::logfile::tell(&format!("import: couldn't note what was imported: {e}"));
    }
    done
}

/// One clip into the library: where it is now, or `None` if cancelled.
fn import_one(clip: &Found, lib: &Path, mode: Mode, progress: &Progress, cancel: &AtomicBool) -> Result<Option<PathBuf>, String> {
    let dir = match &clip.game {
        Some(g) => lib.join(crate::clips::folder_name(g)),
        None => lib.to_path_buf(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make its folder: {e}"))?;
    let dest = crate::clips::free_path(&dir, &library_name(clip));
    // Written to a hidden file first, so a half-copied clip never shows.
    let tmp = dir.join(format!(".import-{}.{}", uuid::Uuid::new_v4().simple(), clip.ext));
    let when = Local.from_local_datetime(&clip.captured).earliest().map(SystemTime::from);
    if clip.steam {
        if let Err(e) = join_steam(&clip.path, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    } else if mode == Mode::Move && std::fs::rename(&clip.path, &dest).is_ok() {
        // The same drive: moved at once.
        set_date(&dest, when);
        return Ok(Some(dest));
    } else {
        match copy_file(&clip.path, &tmp, progress, cancel) {
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
    }
    set_date(&tmp, when);
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("couldn't put it in the library: {e}")
    })?;
    // Moving: the original goes only once its copy is whole. (Steam's are
    // always copied: Steam keeps its own index of them.)
    if mode == Mode::Move && !clip.steam {
        let whole = std::fs::metadata(&dest).is_ok_and(|m| m.len() == clip.bytes);
        if whole && let Err(e) = std::fs::remove_file(&clip.path) {
            crate::logfile::tell(&format!("import: copied {} but couldn't remove the original: {e}", clip.path.display()));
        }
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
/// cancelled.
fn copy_file(from: &Path, to: &Path, progress: &Progress, cancel: &AtomicBool) -> Result<bool, String> {
    let mut src = std::fs::File::open(from).map_err(|e| format!("couldn't open it: {e}"))?;
    let mut dst = std::fs::File::create(to).map_err(|e| format!("couldn't write the copy: {e}"))?;
    let mut buf = vec![0u8; 4 << 20];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let n = src.read(&mut buf).map_err(|e| format!("couldn't read it: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).map_err(|e| format!("couldn't write the copy: {e}"))?;
        progress.0.fetch_add(n as u64, Ordering::Relaxed);
    }
    dst.sync_all().map_err(|e| format!("couldn't write the copy: {e}"))?;
    Ok(true)
}

/// A Steam clip's pieces joined into one MP4 at `out`: each stream's
/// `init-stream<n>.m4s` and then its `chunk-stream<n>-<k>.m4s` in order make
/// a fragmented MP4, and ffmpeg puts the two (video, sound) in one file
/// without re-encoding.
fn join_steam(clip: &Path, out: &Path) -> Result<(), String> {
    let dir = steam_stream_dir(clip).ok_or("its video wasn't found")?;
    let parent = out.parent().ok_or("no folder")?;
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
        Ok(o) => Err(format!("ffmpeg couldn't join its pieces: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(format!("ffmpeg couldn't be run: {e}")),
    }
}

/// One stream's pieces, in order, into one file.
fn join_stream(dir: &Path, n: usize, init: &Path, out: &Path) -> Result<(), String> {
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
    let mut file = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| e.to_string())?);
    for p in std::iter::once(init.to_path_buf()).chain(chunks.into_iter().map(|(_, p)| p)) {
        let mut src = std::fs::File::open(&p).map_err(|e| e.to_string())?;
        std::io::copy(&mut src, &mut file).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())
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

    fn found(path: &Path, game: Option<&str>, title: Option<&str>) -> Found {
        Found {
            path: path.to_path_buf(),
            game: game.map(str::to_owned),
            captured: at("2024-01-02 12:34:56").unwrap(),
            title: title.map(str::to_owned),
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            ext: "mp4".into(),
            steam: false,
        }
    }

    #[test]
    fn names_in_the_library() {
        let f = found(Path::new("x.mp4"), None, Some("Ace: on Ascent"));
        assert_eq!(library_name(&f), "clip_2024-01-02_12-34-56 - Ace. on Ascent.mp4");
        assert_eq!(crate::clips::title_for_stem("clip_2024-01-02_12-34-56 - Ace. on Ascent"), "Ace. on Ascent · 12:34:56");
        assert_eq!(library_name(&found(Path::new("x.mp4"), None, None)), "clip_2024-01-02_12-34-56.mp4");
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
        let src = Source { kind: AppKind::Amd, folders: vec![app.clone()] };
        let found = scan(&src, &[]);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.game.as_deref() == Some("Apex Legends")));
        let named = found.iter().find(|f| f.path.file_name().unwrap().to_string_lossy().contains("replay")).unwrap();
        assert_eq!(named.captured, at("2024-03-04 21:05:33").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copies_and_moves_into_the_library() {
        let root = test_dir("copy");
        let (from, lib) = (root.join("from"), root.join("lib"));
        std::fs::create_dir_all(&from).unwrap();
        let a = from.join("a.mp4");
        let b = from.join("b.mp4");
        std::fs::write(&a, vec![1u8; 1000]).unwrap();
        std::fs::write(&b, vec![2u8; 500]).unwrap();
        let clips = vec![found(&a, Some("Counter-Strike: 2"), None), found(&b, None, Some("Nice"))];
        let progress = Progress::default();
        let done = import(&clips, &lib, Mode::Copy, &progress, &AtomicBool::new(false));
        assert_eq!(done.added.len(), 2, "{:?}", done.failed);
        assert!(a.exists() && b.exists(), "copying keeps the originals");
        let copied = lib.join("Counter-Strike 2").join("clip_2024-01-02_12-34-56.mp4");
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
        // Moving takes the original away, and a name taken gets " (2)".
        let done = import(&clips[..1], &lib, Mode::Move, &Progress::default(), &AtomicBool::new(false));
        assert_eq!(done.added, vec![lib.join("Counter-Strike 2").join("clip_2024-01-02_12-34-56 (2).mp4")]);
        assert!(!a.exists());
        // Cancelled before it starts: nothing done.
        let done = import(&clips[1..], &lib, Mode::Copy, &Progress::default(), &AtomicBool::new(true));
        assert!(done.cancelled && done.added.is_empty());
        // No hidden half-copies left behind.
        assert!(std::fs::read_dir(&lib).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".import-")));
        let _ = std::fs::remove_dir_all(&root);
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
        let found = scan(&Source { kind: AppKind::Steam, folders: vec![root.clone()] }, &[]);
        assert_eq!(found.len(), 1);
        assert!(found[0].steam && found[0].captured == at("2024-01-02 12:34:56").unwrap());
        let lib = root.join("lib");
        let done = import(&found, &lib, Mode::Move, &Progress::default(), &AtomicBool::new(false));
        assert_eq!(done.added.len(), 1, "{:?}", done.failed);
        let info = media::probe(&done.added[0]).expect("a video");
        assert!(info.duration > 2.5, "{}", info.duration);
        assert!(clip.exists(), "Steam's own are never moved");
        let _ = std::fs::remove_dir_all(&root);
    }
}
