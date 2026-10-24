//! Recording configuration, edited on the Settings and Sources pages and
//! persisted as JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which encoder to drive. `Auto` picks the best hardware encoder for the platform
/// (VideoToolbox on macOS, NVENC/AMF/QSV on Windows, VAAPI/NVENC on Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoder {
    Auto,
    Hardware,
    Software,
}

impl Encoder {
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Auto => "Auto (best hardware)",
            Encoder::Hardware => "Hardware",
            Encoder::Software => "Software (slower, uses the CPU)",
        }
    }
}

/// Light or dark look: the system's, or one chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Theme::System => egui::ThemePreference::System,
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// File format for recordings. Both survive a crash mid-recording (written as
/// fragmented files). MKV used to be offered; old settings load as MP4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    /// Plays everywhere. The default.
    #[serde(alias = "Mkv")]
    Mp4,
    /// QuickTime; handy for Final Cut / Apple workflows.
    Mov,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
        }
    }
}

/// File format for screenshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenshotFormat {
    /// Every pixel as it was. The default.
    Png,
    /// About a quarter of the size, for sharing.
    Jpg,
}

impl ScreenshotFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ScreenshotFormat::Png => "png",
            ScreenshotFormat::Jpg => "jpg",
        }
    }
}

/// Kept so saved settings still load; not offered in the UI, since the
/// VideoToolbox encoder only does average-bitrate today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate — predictable file size, best for streaming/replay.
    Cbr,
    /// Constant quality — best quality per byte, variable size.
    Cqp,
}

/// What [`RecordSettings::size_version`] is for settings saved now.
pub const SIZE_VERSION: u32 = 1;

/// Output resolution. `Native` records at the capture source's resolution;
/// the p ones bring its short side down to that many pixels on encode (never
/// up: a smaller display stays its size), so 1080p is 1920×1080 widescreen
/// and 1080×1920 upright. `Exact` is that size whatever the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputResolution {
    Native,
    P480,
    P720,
    P900,
    P1080,
    P1440,
    P2160,
    /// Any short side, in pixels (the other follows the recording's shape).
    Custom(u32),
    /// Exactly this width and height.
    Exact(u32, u32),
}

impl OutputResolution {
    pub fn label(self) -> String {
        match self {
            OutputResolution::Native => "Native".into(),
            OutputResolution::P2160 => "2160p (4K)".into(),
            OutputResolution::Custom(h) => format!("{h}p (custom)"),
            OutputResolution::Exact(w, h) => format!("{w}×{h}"),
            r => format!("{}p", r.short_side().unwrap_or(0)),
        }
    }

    /// What it's good for, next to it in the list.
    pub fn note(self) -> &'static str {
        match self {
            OutputResolution::Native => "every pixel of your display",
            OutputResolution::P2160 => "for 4K displays",
            OutputResolution::P1440 => "sharp, bigger files",
            OutputResolution::P1080 => "sharp, good for sharing",
            OutputResolution::P900 => "",
            OutputResolution::P720 => "small files, quick to upload",
            OutputResolution::P480 => "smallest files",
            OutputResolution::Custom(_) | OutputResolution::Exact(..) => "",
        }
    }

    /// The short side to bring the recording down to, in pixels; `None` for
    /// native (no rescale) and for an exact size.
    pub fn short_side(self) -> Option<u32> {
        match self {
            OutputResolution::Native | OutputResolution::Exact(..) => None,
            OutputResolution::P480 => Some(480),
            OutputResolution::P720 => Some(720),
            OutputResolution::P900 => Some(900),
            OutputResolution::P1080 => Some(1080),
            OutputResolution::P1440 => Some(1440),
            OutputResolution::P2160 => Some(2160),
            OutputResolution::Custom(h) => Some(h),
        }
    }

    /// The exact size asked for, if it's one.
    pub fn exact(self) -> Option<(u32, u32)> {
        match self {
            OutputResolution::Exact(w, h) => Some((w, h)),
            _ => None,
        }
    }

    /// The choices in the list, best first (a custom height is set apart).
    pub const PRESETS: [OutputResolution; 7] = [
        OutputResolution::Native,
        OutputResolution::P2160,
        OutputResolution::P1440,
        OutputResolution::P1080,
        OutputResolution::P900,
        OutputResolution::P720,
        OutputResolution::P480,
    ];
}

/// The screen (or games and apps) as a source in the video: whether it's
/// in it, in front of the webcam or behind it, and where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenCfg {
    /// In the video (its switch). Off, the webcam shows on black.
    pub enabled: bool,
    /// In front of the webcam, rather than behind it.
    pub in_front: bool,
    /// Placed by hand: its box. `None`: it fills the video, as `framing`
    /// (with an aspect ratio of its own) says.
    pub placed: Option<BoxCfg>,
}

impl Default for ScreenCfg {
    fn default() -> Self {
        Self { enabled: true, in_front: false, placed: None }
    }
}

/// A box in the video: left, top, width, height as fractions of the video,
/// and the shape (width / height) of the video it was laid out in, to keep
/// its own shape in a video of another (see [`PlacementCfg::refit`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoxCfg {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub frame_aspect: f32,
}

/// What the video shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// The display chosen by `display_id`.
    #[default]
    Screen,
    /// Games and apps: whichever is in focus is recorded (see
    /// `capture::VideoSource::Apps`). With `away_screen`, tabbing out to
    /// anything else shows the away screen instead of the last one.
    Apps {
        apps: Vec<CaptureApp>,
        #[serde(default = "yes")]
        away_screen: bool,
    },
    /// One app, from before several could be chosen; read only, turned into
    /// `Apps` on load.
    App { id: String, name: String },
}

/// A webcam drawn over the recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebcamCfg {
    /// Backend id (`capture::webcam::list_cameras`).
    pub id: String,
    /// What to call it while it isn't connected.
    pub name: String,
    /// The format to open it in; `None` picks the best.
    #[serde(default)]
    pub format: Option<FormatCfg>,
    pub placement: PlacementCfg,
    /// In the clips. Off keeps the camera, format and placement for later
    /// (and closes the camera).
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// A camera format: see `capture::webcam::Format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatCfg {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl From<FormatCfg> for capture::webcam::Format {
    fn from(f: FormatCfg) -> Self {
        Self { width: f.width, height: f.height, fps_num: f.fps_num, fps_den: f.fps_den }
    }
}

impl From<capture::webcam::Format> for FormatCfg {
    fn from(f: capture::webcam::Format) -> Self {
        Self { width: f.width, height: f.height, fps_num: f.fps_num, fps_den: f.fps_den }
    }
}

/// Where the webcam sits: see `capture::webcam::Placement`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlacementCfg {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub crop: [f32; 4],
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    /// Quarter turns clockwise.
    #[serde(default)]
    pub turns: u8,
    /// The shape (width / height) of the frame this was laid out in. The box
    /// is fractions of the frame, so in a frame of another shape (another
    /// display, an ultrawide) the same fractions are another shape: it's
    /// refitted then (`refit`), keeping its own shape in pixels.
    #[serde(default)]
    pub frame_aspect: Option<f32>,
}

impl PlacementCfg {
    /// Keep the box right for a frame of shape `frame_aspect`: rescaled to
    /// keep its shape in pixels, as wide a share of the frame (narrower if
    /// that won't fit), at the same place in the room it has to move in (so
    /// a box in a corner stays in it). Its shape is what's left of the
    /// camera's picture once cropped, as every box is made: given the
    /// camera's shape (`camera_aspect`, as turned, when known), a box of
    /// another shape gets it back, one laid out before the frame's shape was
    /// kept or squashed by a frame that changed under it. Returns whether it
    /// changed.
    pub fn refit(&mut self, frame_aspect: f32, camera_aspect: Option<f32>) -> bool {
        if frame_aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return false;
        }
        let wanted = camera_aspect.filter(|&cam| cam > 0.0).map(|cam| {
            cam * (1.0 - self.crop[0] - self.crop[2]).max(0.05) / (1.0 - self.crop[1] - self.crop[3]).max(0.05)
        });
        // Its shape in pixels, in the frame it was made for.
        let had = self.frame_aspect.map(|was| self.w * was / self.h);
        let same_frame = self.frame_aspect.is_some_and(|was| (was - frame_aspect).abs() < 1e-3);
        let box_aspect = match (had, wanted) {
            // Off by more than the preview's rounding.
            (Some(had), Some(wanted)) if (had / wanted - 1.0).abs() > 0.02 => wanted,
            (Some(_), _) if same_frame => return false,
            (Some(had), _) => had,
            (None, Some(wanted)) => wanted,
            (None, None) => return false, // wait for the camera's shape
        };
        // From 0 against the left or top edge to 1 against the other.
        let along = |at: f32, size: f32| if size < 1.0 - 1e-4 { (at / (1.0 - size)).clamp(0.0, 1.0) } else { 0.5 };
        let (ax, ay) = (along(self.x, self.w), along(self.y, self.h));
        let mut w = self.w.min(1.0);
        let mut h = w * frame_aspect / box_aspect;
        if h > 1.0 {
            w *= 1.0 / h;
            h = 1.0;
        }
        self.w = w;
        self.h = h;
        self.x = ax * (1.0 - w);
        self.y = ay * (1.0 - h);
        self.frame_aspect = Some(frame_aspect);
        true
    }
}

impl From<PlacementCfg> for capture::webcam::Placement {
    fn from(p: PlacementCfg) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns }
    }
}

impl PlacementCfg {
    /// `p`, laid out in a frame of shape `frame_aspect`.
    pub fn laid_out(p: capture::webcam::Placement, frame_aspect: f32) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns, frame_aspect: Some(frame_aspect) }
    }
}

/// A game or app to record, by executable. `name` is what to call it while it
/// isn't running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureApp {
    pub id: String,
    pub name: String,
}

/// A game with anti-cheat that game capture may be used on anyway (the user
/// accepted the risk), by executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedGame {
    pub id: String,
    pub name: String,
    /// The anti-cheat it was found to use, to show.
    pub anticheat: String,
}

/// Where an audio source's sound comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceKind {
    /// A mic or audio interface. `device` is a device name, or
    /// `capture::audio::DEFAULT_DEVICE` to follow the OS default.
    Microphone { device: String },
    /// Everything the computer plays. `exclude_apps` leaves out apps that are
    /// added as their own source, so they aren't heard twice.
    Desktop { exclude_apps: bool },
    /// One application, by bundle id (macOS) / executable (Windows). `app_name`
    /// is what to call it while it isn't running.
    App { bundle_id: String, app_name: String },
}

/// The order the library shows clips in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LibrarySort {
    #[default]
    Newest,
    Oldest,
    Longest,
    Largest,
}

impl LibrarySort {
    pub const ALL: [LibrarySort; 4] = [LibrarySort::Newest, LibrarySort::Oldest, LibrarySort::Longest, LibrarySort::Largest];

    pub fn label(self) -> &'static str {
        match self {
            LibrarySort::Newest => "Newest first",
            LibrarySort::Oldest => "Oldest first",
            LibrarySort::Longest => "Longest first",
            LibrarySort::Largest => "Largest first",
        }
    }

    /// Grouped by day (newest or oldest first), or one list.
    pub fn by_day(self) -> bool {
        matches!(self, LibrarySort::Newest | LibrarySort::Oldest)
    }
}

/// One audio source on the Sources page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSourceCfg {
    /// Stable id: keys the live volume/meter and survives renames.
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    /// Captured at all. Off = as if it weren't there (but keeps its settings).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Part of what the clip sounds like (track 1).
    #[serde(default = "yes")]
    pub in_mix: bool,
    /// Also on its own track, to rebalance in the editor.
    #[serde(default = "yes")]
    pub own_track: bool,
    #[serde(default)]
    pub volume_db: f32,
    #[serde(default)]
    pub muted: bool,
    /// How much of a microphone's background noise is taken out. Off unless
    /// chosen.
    #[serde(default)]
    pub noise_reduction: NoiseReduction,
    /// A microphone heard in the middle, its two sides made one: for one
    /// heard on one side only. Off unless turned on.
    #[serde(default)]
    pub mono: bool,
    /// From before there were two: on, which is Strong now. Read, not written.
    #[serde(default, skip_serializing, rename = "noise_removal")]
    old_noise_removal: bool,
}

/// How the looks in a blended frame are weighed (see `capture::BlendShape`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendShape {
    /// Each look in the frame's own time the same.
    #[default]
    Even,
    /// The middle of the frame's time most, fading into the frames either side.
    Soft,
}

impl From<BlendShape> for capture::BlendShape {
    fn from(b: BlendShape) -> Self {
        match b {
            BlendShape::Even => Self::Even,
            BlendShape::Soft => Self::Soft,
        }
    }
}

/// How a whole display is captured on Windows (see `capture::ScreenCapture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenCapture {
    /// Desktop Duplication on Windows 10 (no yellow border), Windows Graphics
    /// Capture on Windows 11.
    #[default]
    Auto,
    Duplication,
    Wgc,
}

impl ScreenCapture {
    pub const ALL: [ScreenCapture; 3] = [ScreenCapture::Auto, ScreenCapture::Duplication, ScreenCapture::Wgc];

    pub fn label(self) -> &'static str {
        match self {
            ScreenCapture::Auto => "Automatic",
            ScreenCapture::Duplication => "Desktop Duplication",
            ScreenCapture::Wgc => "Windows Graphics Capture",
        }
    }
}

impl From<ScreenCapture> for capture::ScreenCapture {
    fn from(m: ScreenCapture) -> Self {
        match m {
            ScreenCapture::Auto => capture::ScreenCapture::Auto,
            ScreenCapture::Duplication => capture::ScreenCapture::Duplication,
            ScreenCapture::Wgc => capture::ScreenCapture::Wgc,
        }
    }
}

/// How the screen goes into a recording of another shape (see
/// `capture::Framing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Framing {
    /// Its middle, the edges past the shape cut off.
    #[default]
    Fill,
    /// All of it, with black bars.
    Fit,
    /// All of it, stretched or squeezed.
    Stretch,
}

impl From<Framing> for capture::Framing {
    fn from(f: Framing) -> Self {
        match f {
            Framing::Fill => Self::Fill,
            Framing::Fit => Self::Fit,
            Framing::Stretch => Self::Stretch,
        }
    }
}

/// How much of a microphone's background noise is taken out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseReduction {
    #[default]
    Off,
    /// Steady noise (fans, hum, hiss): a keyboard stays.
    Light,
    /// Everything but a voice, a keyboard too.
    Strong,
}

impl From<NoiseReduction> for capture::mixer::NoiseReduction {
    fn from(n: NoiseReduction) -> Self {
        match n {
            NoiseReduction::Off => Self::Off,
            NoiseReduction::Light => Self::Light,
            NoiseReduction::Strong => Self::Strong,
        }
    }
}

fn yes() -> bool {
    true
}

impl AudioSourceCfg {
    pub fn new(name: &str, kind: SourceKind) -> Self {
        Self {
            id: new_source_id(),
            name: name.into(),
            kind,
            enabled: true,
            in_mix: true,
            own_track: true,
            volume_db: 0.0,
            muted: false,
            noise_reduction: NoiseReduction::Off,
            mono: false,
            old_noise_removal: false,
        }
    }
}

/// A short random-ish id for a new source.
fn new_source_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("src-{:x}-{}", t.as_millis(), N.fetch_add(1, Ordering::Relaxed))
}

/// The audio sources a new install starts with: your mic and your computer.
pub fn default_sources() -> Vec<AudioSourceCfg> {
    vec![
        AudioSourceCfg::new("Microphone", SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() }),
        AudioSourceCfg::new("Desktop", SourceKind::Desktop { exclude_apps: true }),
    ]
}

/// The audio list before sources (≤ 2026-10-01): inputs/outputs, each always
/// both mixed and on its own track. Read only to migrate.
#[derive(Debug, Clone, Deserialize)]
pub struct LegacyAudioTrack {
    name: String,
    kind: LegacyKind,
    device_id: String,
    enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum LegacyKind {
    Input,
    Output,
}

fn migrate(old: &[LegacyAudioTrack]) -> Vec<AudioSourceCfg> {
    let mut out: Vec<AudioSourceCfg> = Vec::new();
    for t in old {
        let kind = match t.kind {
            LegacyKind::Input => SourceKind::Microphone { device: t.device_id.clone() },
            // There's one desktop capture whatever the output device; keep one.
            LegacyKind::Output if out.iter().any(|s| matches!(s.kind, SourceKind::Desktop { .. })) => continue,
            LegacyKind::Output => SourceKind::Desktop { exclude_apps: true },
        };
        let mut s = AudioSourceCfg::new(&t.name, kind);
        s.enabled = t.enabled;
        out.push(s);
    }
    out
}

/// Which games name clips after what happened, and what osu!'s names show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameTitles {
    pub cs2: bool,
    pub dota2: bool,
    pub league: bool,
    pub osu: bool,
    /// Accuracy and FC (and the combo mid-play).
    pub osu_accuracy: bool,
    pub osu_misses: bool,
    pub osu_pp: bool,
    pub osu_mods: bool,
    pub osu_stars: bool,
    pub osu_map: bool,
    pub osu_artist: bool,
    /// The order of the parts of an osu! name, as dragged in Settings.
    pub osu_order: Vec<OsuPart>,
}

impl Default for GameTitles {
    fn default() -> Self {
        Self {
            cs2: true,
            dota2: true,
            league: true,
            osu: true,
            osu_accuracy: true,
            osu_misses: true,
            osu_pp: true,
            osu_mods: true,
            osu_stars: false,
            osu_map: true,
            osu_artist: false,
            osu_order: OsuPart::ALL.to_vec(),
        }
    }
}

/// The part of the screen kept, in its pixels: `w`×`h`, its top-left
/// corner at `at`, or in the middle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenCrop {
    pub w: u32,
    pub h: u32,
    pub at: Option<[u32; 2]>,
}

impl ScreenCrop {
    /// No size: none of it cut off.
    pub fn is_none(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

impl From<ScreenCrop> for capture::Crop {
    fn from(c: ScreenCrop) -> Self {
        capture::Crop { w: c.w, h: c.h, at: c.at.map(|[x, y]| (x, y)) }
    }
}

/// A part of an osu! clip's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsuPart {
    Pp,
    Misses,
    Accuracy,
    Mods,
    Stars,
    Map,
}

impl OsuPart {
    pub const ALL: [OsuPart; 6] = [OsuPart::Pp, OsuPart::Misses, OsuPart::Accuracy, OsuPart::Mods, OsuPart::Stars, OsuPart::Map];

    pub fn label(self) -> &'static str {
        match self {
            OsuPart::Pp => "pp",
            OsuPart::Misses => "Misses",
            OsuPart::Accuracy => "Accuracy",
            OsuPart::Mods => "Mods",
            OsuPart::Stars => "Star rating",
            OsuPart::Map => "Map",
        }
    }
}

impl GameTitles {
    /// The parts in their order: each once, any missing (an older settings
    /// file) at the end.
    pub fn osu_parts(&self) -> Vec<OsuPart> {
        let mut parts: Vec<OsuPart> = Vec::new();
        for p in self.osu_order.iter().chain(OsuPart::ALL.iter()) {
            if !parts.contains(p) {
                parts.push(*p);
            }
        }
        parts
    }

    pub fn osu_on(&self, part: OsuPart) -> bool {
        match part {
            OsuPart::Pp => self.osu_pp,
            OsuPart::Misses => self.osu_misses,
            OsuPart::Accuracy => self.osu_accuracy,
            OsuPart::Mods => self.osu_mods,
            OsuPart::Stars => self.osu_stars,
            OsuPart::Map => self.osu_map,
        }
    }

    pub fn osu_on_mut(&mut self, part: OsuPart) -> &mut bool {
        match part {
            OsuPart::Pp => &mut self.osu_pp,
            OsuPart::Misses => &mut self.osu_misses,
            OsuPart::Accuracy => &mut self.osu_accuracy,
            OsuPart::Mods => &mut self.osu_mods,
            OsuPart::Stars => &mut self.osu_stars,
            OsuPart::Map => &mut self.osu_map,
        }
    }
}

/// Persisted to `<config_dir>/hasteclip/settings.json`. `#[serde(default)]` lets
/// older files load after new fields are added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    // --- Common (always visible on the Settings tab) ---
    /// What the video shows: a whole display or one app's window.
    pub capture: CaptureTarget,
    /// The games and apps list while the whole screen is recorded, so
    /// switching back to games and apps brings it back.
    pub idle_apps: Option<CaptureTarget>,
    /// A webcam over the picture (or under it), if any.
    pub webcam: Option<WebcamCfg>,
    /// The screen as a source in the video: in front or behind, its box.
    pub screen: ScreenCfg,
    /// Each camera's settings changed from its defaults (Linux: V4L2 control
    /// id to value), by camera id: put back each time it's opened, as
    /// cameras forget them when unplugged.
    pub camera_controls: std::collections::BTreeMap<String, std::collections::BTreeMap<u32, i64>>,
    /// The chosen display's id (`capture::Device::id`), so the choice stays
    /// with that display when others are plugged in or listed in another
    /// order. The main display when it isn't connected.
    pub display_id: Option<String>,
    /// Where the chosen display was in the list: what settings from before
    /// `display_id` have, and still written for them.
    pub display_index: usize,
    pub fps: u32,
    /// Live frame blending: look at the screen this many times a second and
    /// record the average of the looks in each frame (a whole multiple of
    /// `fps`), for smoother, motion-blurred video. 0 is off.
    pub blend_fps: u32,
    /// How the looks in a blended frame are weighed.
    pub blend_shape: BlendShape,
    pub resolution: OutputResolution,
    /// What `resolution`'s p means in this file: 0 from before it was the
    /// short side (it was the height), 1 since. Read once, on load.
    #[serde(default)]
    pub size_version: u32,
    /// The shape recordings are made in, width to height, when it isn't the
    /// screen's (16:9 out of an ultrawide screen).
    pub shape: Option<[u32; 2]>,
    /// How the screen goes into that shape.
    pub framing: Framing,
    /// The part of the screen kept (a game at a lower resolution than the
    /// display, black around it): set on Sources. It's recorded as a display
    /// that size would be.
    pub screen_crop: Option<ScreenCrop>,
    pub video_bitrate_mbps: u32,

    pub replay_seconds: u32,
    /// Start the replay buffer as soon as the app opens, so you never miss a moment.
    pub auto_start_buffer: bool,
    /// Download new versions in the background and install them on quit.
    pub auto_update: bool,
    /// Show what you're clipping on your Discord profile (Rich Presence).
    pub discord_presence: bool,
    /// Saying "HasteClip that" saves a clip (while the replay buffer runs).
    pub voice_clip: bool,
    /// Keep short snippets of the microphone around what the voice detector fires on or
    /// nearly fires on, to listen through and label (never sent anywhere by itself).
    pub voice_samples: bool,

    // --- Advanced (collapsible; sane defaults) ---
    pub encoder: Encoder,
    pub container: Container,
    pub rate_control: RateControl,
    pub output_dir: PathBuf,
    /// Save each clip in a folder named after the game it's of (`osu!/`,
    /// `Desktop/` when there's no game), shown as categories in the library.
    pub folder_per_game: bool,
    /// Game folders renamed in the library: the folder a game's clips would
    /// go in (its name, as `clips::folder_name` makes it) → the one they go in.
    pub game_folders: std::collections::BTreeMap<String, String>,
    /// Light, dark, or the system's.
    pub theme: Theme,
    /// Closing the window keeps HasteClip running in the tray (where
    /// there is one) instead of quitting.
    pub close_to_tray: bool,
    /// The first close asked which (then it just does it).
    pub close_asked: bool,
    /// macOS: which time the permissions screen was last shown at launch
    /// ([`crate::setup::SETUP_ROUND`]): it's shown again, once, when a
    /// release needs everyone to allow them again.
    pub setup_shown: u32,
    /// Deleting a clip removes it for good instead of moving it to the
    /// Recycle Bin / Trash (then it asks first).
    pub delete_permanently: bool,
    /// Name clips after what happened in the game ("3 kills on Mirage"), for
    /// games that report it (CS2, Dota 2, League of Legends, osu!).
    pub game_details: bool,
    /// Which games, and what goes in osu!'s names.
    pub game_titles: GameTitles,

    // --- Sources page ---
    pub audio_sources: Vec<AudioSourceCfg>,
    /// Keep the clip's mix from clipping when sources add up too loud.
    pub limiter: bool,
    /// Record games Windows' own capture can't see properly (exclusive
    /// fullscreen, a lost cursor) through the game capture hook, where it's
    /// safe: never on games with anti-cheat unless allowed below (see
    /// `capture::GameHook`). Recording the screen or games and
    /// apps alike.
    pub game_capture: bool,
    /// Games with anti-cheat game capture may be used on anyway.
    pub game_capture_allowed: Vec<AllowedGame>,
    /// How a whole display is captured (Windows).
    pub screen_capture: ScreenCapture,

    // --- Screenshots ---
    pub screenshot_format: ScreenshotFormat,
    /// A screenshot also goes on the clipboard, to paste right away.
    pub copy_screenshots: bool,

    // --- Shortcuts ---
    pub shortcuts: Shortcuts,
    /// The sound played when a clip (or a screenshot) is saved.
    pub save_sound: SaveSound,
    /// The editor's preview height, as a share of the window (the timeline
    /// gets the rest), as last dragged.
    pub editor_preview_share: f32,
    /// The editor's snapping (the magnet, N): trim edges and the playhead
    /// snap to each other, the clip's ends and its moments. Off unless you
    /// turn it on (it caught on kills while trimming), then kept as you left it.
    pub editor_snap: bool,
    /// The clip player's volume (0..=1 on its slider) and mute, as last set,
    /// so a clip never opens at full blast after you'd turned it down.
    pub player_volume: f32,
    pub player_muted: bool,
    /// The library's order.
    pub library_sort: LibrarySort,
    /// Pre-sources audio list; migrated into `audio_sources` on load.
    #[serde(skip_serializing)]
    audio_tracks: Option<Vec<LegacyAudioTrack>>,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            capture: CaptureTarget::Screen,
            idle_apps: None,
            webcam: None,
            screen: ScreenCfg::default(),
            camera_controls: Default::default(),
            display_id: None,
            display_index: 0,
            fps: 60,
            blend_fps: 0,
            blend_shape: BlendShape::Even,
            resolution: OutputResolution::Native,
            size_version: SIZE_VERSION,
            shape: None,
            framing: Framing::Fill,
            screen_crop: None,
            video_bitrate_mbps: 40,

            replay_seconds: 60,
            auto_start_buffer: true,
            auto_update: true,
            // On to begin with: friends see what you clip, and where to get
            // HasteClip. Settings saved before keep theirs (off).
            discord_presence: true,
            voice_clip: true,
            voice_samples: false,
            encoder: Encoder::Auto,
            container: Container::Mp4,
            rate_control: RateControl::Cbr,
            output_dir: default_output_dir(),
            folder_per_game: true,
            game_folders: Default::default(),
            game_details: true,
            delete_permanently: false,
            library_sort: LibrarySort::Newest,
            theme: Theme::System,
            close_to_tray: true,
            close_asked: false,
            setup_shown: 0,
            game_titles: GameTitles::default(),
            audio_sources: default_sources(),
            limiter: true,
            game_capture: true,
            game_capture_allowed: Vec::new(),
            screen_capture: ScreenCapture::Auto,
            screenshot_format: ScreenshotFormat::Png,
            copy_screenshots: true,
            shortcuts: Shortcuts::default(),
            save_sound: SaveSound::default(),
            editor_preview_share: 0.5,
            editor_snap: false,
            player_volume: 1.0,
            player_muted: false,
            audio_tracks: None,
        }
    }
}

/// The sound played when a clip is saved (see `sound.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveSound {
    pub enabled: bool,
    /// A built-in sound's id, or `file:<path>` for one of `custom`.
    pub sound: String,
    /// 0..=1, applied on a curve (see `sound::gain`).
    pub volume: f32,
    /// Sounds you've added, copied into HasteClip' own folder.
    pub custom: Vec<CustomSound>,
    /// Recorded like any other app's sound (in what's recorded after it).
    pub recorded: bool,
}

impl Default for SaveSound {
    fn default() -> Self {
        Self { enabled: true, sound: "horse".into(), volume: 0.5, custom: Vec::new(), recorded: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomSound {
    pub name: String,
    pub file: PathBuf,
}

/// The global shortcuts, as `global_hotkey` strings ("alt+F10"). Empty = off.
/// Only Save clip has one to begin with; the rest are off until chosen:
/// pressed by accident, Start or stop clipping turns clipping off without a
/// word (the clip you wanted isn't there), recording fills the disk, and
/// any key taken is one a game or another app may want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub save_clip: String,
    pub toggle_buffer: String,
    pub toggle_record: String,
    pub screenshot: String,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self { save_clip: "alt+F10".into(), toggle_buffer: String::new(), toggle_record: String::new(), screenshot: String::new() }
    }
}

/// One of the actions a global shortcut can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    SaveClip,
    ToggleBuffer,
    ToggleRecord,
    Screenshot,
}

impl ShortcutAction {
    pub const ALL: [ShortcutAction; 4] = [ShortcutAction::SaveClip, ShortcutAction::Screenshot, ShortcutAction::ToggleBuffer, ShortcutAction::ToggleRecord];

    pub fn label(self) -> &'static str {
        match self {
            ShortcutAction::SaveClip => "Save clip",
            ShortcutAction::ToggleBuffer => "Start or stop clipping",
            ShortcutAction::ToggleRecord => "Start or stop recording",
            ShortcutAction::Screenshot => "Take a screenshot",
        }
    }

    /// Whether it does anything here: screenshots aren't on every platform yet.
    pub fn available(self) -> bool {
        self != ShortcutAction::Screenshot || capture::SCREENSHOTS
    }
}

impl Shortcuts {
    pub fn get(&self, a: ShortcutAction) -> &str {
        match a {
            ShortcutAction::SaveClip => &self.save_clip,
            ShortcutAction::ToggleBuffer => &self.toggle_buffer,
            ShortcutAction::ToggleRecord => &self.toggle_record,
            ShortcutAction::Screenshot => &self.screenshot,
        }
    }

    pub fn set(&mut self, a: ShortcutAction, value: String) {
        match a {
            ShortcutAction::SaveClip => self.save_clip = value,
            ShortcutAction::ToggleBuffer => self.toggle_buffer = value,
            ShortcutAction::ToggleRecord => self.toggle_record = value,
            ShortcutAction::Screenshot => self.screenshot = value,
        }
    }
}

/// The blend rates offered for a frame rate: whole multiples of it, up to 16
/// times and [`MAX_BLEND`], or `max` where the system can't look faster
/// (macOS: the display's refresh rate).
pub fn blend_choices(fps: u32, max: Option<u32>) -> Vec<u32> {
    (2..=16).map(|k| fps * k).filter(|&r| r <= max.unwrap_or(MAX_BLEND).min(MAX_BLEND)).collect()
}

/// The fastest blend rate offered: 60 fps from 1920. Measured on an RTX
/// 4060 Ti: the cost follows the game's pictures, not the looks (repeated
/// looks are added in one pass; the game capture's looks copy only new
/// pictures), and looks at 1200/s timed a 600 fps game's pictures exactly.
/// Beyond, a guess.
pub const MAX_BLEND: u32 = 2000;

impl RecordSettings {
    /// Looks at the screen per recorded frame: the blend rate over the frame
    /// rate, 1 (no blending) when off or no longer a multiple above it.
    /// The shape recordings are made in, if it isn't the screen's: an exact
    /// size's own, else the aspect ratio chosen.
    pub fn capture_shape(&self) -> Option<capture::Shape> {
        match self.resolution.exact() {
            Some((w, h)) => (w > 0 && h > 0).then(|| capture::Shape { w, h, framing: self.framing.into() }),
            None => self.aspect_shape(),
        }
    }

    /// The aspect ratio chosen, if it isn't the screen's (an exact size
    /// aside: it's what a resolution in p records in).
    pub fn aspect_shape(&self) -> Option<capture::Shape> {
        self.shape.filter(|[w, h]| *w > 0 && *h > 0).map(|[w, h]| capture::Shape { w, h, framing: self.framing.into() })
    }

    /// The size recordings are made at.
    pub fn video_size(&self) -> capture::VideoSize {
        capture::VideoSize { short_side: self.resolution.short_side(), shape: self.capture_shape(), exact: self.resolution.exact() }
    }

    /// Where the screen's picture goes in the video, for the capture: its
    /// box, filled (never stretched), when it's been placed by hand; else
    /// all of the video, as the framing says with a shape of the video's
    /// own (else all of the picture: a window that isn't the screen's shape
    /// goes in whole).
    pub fn screen_placement(&self) -> capture::ScreenPlacement {
        let s = &self.screen;
        let (rect, framing) = match s.placed {
            Some(b) => ([b.x, b.y, b.w, b.h], capture::Framing::Fill),
            None => ([0.0, 0.0, 1.0, 1.0], self.video_size().framing()),
        };
        capture::ScreenPlacement { rect, framing, in_front: s.in_front, hidden: !s.enabled }
    }

    pub fn blend_samples(&self) -> u32 {
        if self.blend_fps == 0 { 1 } else { (self.blend_fps as f64 / self.fps.max(1) as f64).round().max(1.0) as u32 }
    }
}

/// Common frame rates for the picker; those above the display's refresh
/// rate (and 240) aren't offered.
pub const FPS_CHOICES: [u32; 9] = [30, 60, 120, 144, 165, 240, 360, 480, 540];

/// The frame rate presets for a display refreshing `hz` times a second
/// (None: not known): up to it (and at least up to 240), and it itself.
pub fn fps_choices(hz: Option<u32>) -> Vec<u32> {
    let top = hz.unwrap_or(240).max(240);
    let mut out: Vec<u32> = FPS_CHOICES.iter().copied().filter(|&f| f <= top).collect();
    if let Some(hz) = hz.filter(|hz| (10..=MAX_FPS).contains(hz) && !out.contains(hz)) {
        out.push(hz);
        out.sort_unstable();
    }
    out
}

/// The highest frame rate that can be typed. Measured: an RTX 4060 Ti's
/// encoder takes 1080p at up to ~540 fps (2 ms a frame, one at a time).
pub const MAX_FPS: u32 = 600;

/// `~/Movies/hasteclip` on macOS, the platform video dir elsewhere, falling back
/// to the current dir if none is known.
pub fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hasteclip")
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hasteclip").join("settings.json"))
}

/// Dev aid: `HASTECLIP_SETTINGS=<file>` loads the settings from that file
/// instead (to check a setup without touching the real one; dev-hook
/// launches never save).
fn load_path() -> Option<PathBuf> {
    std::env::var_os("HASTECLIP_SETTINGS").map(PathBuf::from).or_else(settings_path)
}

impl RecordSettings {
    /// Last saved settings, or defaults on first run / unreadable file.
    pub fn load() -> Self {
        let mut s: Self = load_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        s.upgrade_capture();
        s.upgrade_save_sound();
        s.upgrade_noise_reduction();
        s.upgrade_sizes();
        if let Some(old) = s.audio_tracks.take() {
            s.audio_sources = migrate(&old);
            // Persist now: sources get fresh ids, and their volumes key on them.
            Self::save_json(&s.to_json());
        }
        s
    }

    /// A single chosen app (from before several could be) becomes a list of one.
    fn upgrade_capture(&mut self) {
        if let CaptureTarget::App { id, name } = &self.capture {
            self.capture = CaptureTarget::Apps {
                apps: vec![CaptureApp { id: id.clone(), name: name.clone() }],
                away_screen: true,
            };
        }
    }

    /// A built-in sound that's gone (Silverfish) becomes the default, so the
    /// picker names what plays.
    fn upgrade_save_sound(&mut self) {
        let s = &self.save_sound.sound;
        if !s.starts_with("file:") && !crate::sound::BUILTIN.iter().any(|(id, _)| id == s) {
            self.save_sound.sound = SaveSound::default().sound;
        }
    }

    /// A resolution from when p was the height keeps making clips the same
    /// size now it's the short side: upright, 1080p was 608×1080, so it's
    /// 608p now (wider shapes' height is their short side already).
    fn upgrade_sizes(&mut self) {
        if self.size_version >= SIZE_VERSION {
            return;
        }
        self.size_version = SIZE_VERSION;
        if let (Some([w, h]), Some(p)) = (self.shape.filter(|[w, h]| *w > 0 && w < h), self.resolution.short_side()) {
            let short = ((p as f64 * w as f64 / h as f64 / 2.0).round() as u32 * 2).max(2);
            self.resolution = OutputResolution::PRESETS.iter().copied().find(|r| r.short_side() == Some(short)).unwrap_or(OutputResolution::Custom(short));
        }
    }

    /// Noise removal that was on, from before there were two, is the one it
    /// was: Strong.
    fn upgrade_noise_reduction(&mut self) {
        for s in &mut self.audio_sources {
            if std::mem::take(&mut s.old_noise_removal) && s.noise_reduction == NoiseReduction::Off {
                s.noise_reduction = NoiseReduction::Strong;
            }
        }
    }

    /// Serialized form, used both to save and to detect changes cheaply.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn save_json(json: &str) {
        if let Some(path) = settings_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod screen_capture_tests {
    use super::*;

    /// Settings saved before the choice was there load as Automatic; a choice
    /// is saved by name.
    #[test]
    fn screen_capture_defaults_to_automatic_and_is_saved() {
        let s: RecordSettings = serde_json::from_str(r#"{"fps": 60}"#).unwrap();
        assert_eq!(s.screen_capture, ScreenCapture::Auto);
        let s: RecordSettings = serde_json::from_str(r#"{"screen_capture": "duplication"}"#).unwrap();
        assert_eq!(capture::ScreenCapture::from(s.screen_capture), capture::ScreenCapture::Duplication);
        assert!(s.to_json().contains(r#""screen_capture": "duplication""#));
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    /// No shape unless chosen; chosen, it's saved and goes to the capture.
    #[test]
    fn shape_is_saved_and_given_to_the_capture() {
        let s = RecordSettings::default();
        assert_eq!(s.capture_shape(), None);
        let s: RecordSettings = serde_json::from_str(r#"{"shape":[16,9],"framing":"fit"}"#).unwrap();
        assert_eq!(s.capture_shape(), Some(capture::Shape { w: 16, h: 9, framing: capture::Framing::Fit }));
        assert!(s.to_json().contains(r#""framing": "fit""#));
    }

    /// An exact size is its own shape, whatever aspect ratio was chosen, and
    /// is kept as it is.
    #[test]
    fn an_exact_size_is_the_shape() {
        let s: RecordSettings = serde_json::from_str(r#"{"shape":[4,3],"resolution":{"Exact":[1080,1350]}}"#).unwrap();
        assert_eq!(s.capture_shape(), Some(capture::Shape { w: 1080, h: 1350, framing: capture::Framing::Fill }));
        assert_eq!(s.video_size().of(2560, 1440), (1080, 1350));
        assert!(s.to_json().contains(r#""Exact": ["#));
    }

    /// From when 1080p was the height, an upright recording keeps its size
    /// (1080p 9:16 was 608×1080); wider ones, new settings and settings
    /// already moved over aren't touched.
    #[test]
    fn old_upright_resolutions_keep_their_size() {
        let load = |json: &str| {
            let mut s: RecordSettings = serde_json::from_str(json).unwrap();
            s.upgrade_sizes();
            s
        };
        let s = load(r#"{"shape":[9,16],"resolution":"P1080"}"#);
        assert_eq!(s.resolution, OutputResolution::Custom(608));
        assert_eq!(s.size_version, SIZE_VERSION);
        assert_eq!(s.video_size().of(2560, 1440), (608, 1080));
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"P1440"}"#).resolution, OutputResolution::Custom(810));
        assert_eq!(load(r#"{"shape":[3,4],"resolution":"P1440"}"#).resolution, OutputResolution::P1080);
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"Native"}"#).resolution, OutputResolution::Native);
        assert_eq!(load(r#"{"shape":[21,9],"resolution":"P1080"}"#).resolution, OutputResolution::P1080);
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"P1080","size_version":1}"#).resolution, OutputResolution::P1080);
        let mut fresh = RecordSettings { shape: Some([9, 16]), resolution: OutputResolution::P1080, ..Default::default() };
        fresh.upgrade_sizes();
        assert_eq!(fresh.resolution, OutputResolution::P1080);
    }
}

#[cfg(test)]
mod noise_reduction_tests {
    use super::*;

    /// Noise removal that was on is Strong, and the old switch isn't written back.
    #[test]
    fn old_noise_removal_is_strong() {
        let mut s = RecordSettings::default();
        let old = |on: bool| serde_json::from_str(&format!(r#"{{"id":"a","name":"Mic","kind":{{"type":"microphone","device":"default"}},"noise_removal":{on}}}"#)).unwrap();
        s.audio_sources = vec![old(true), old(false)];
        s.upgrade_noise_reduction();
        assert_eq!(s.audio_sources[0].noise_reduction, NoiseReduction::Strong);
        assert_eq!(s.audio_sources[1].noise_reduction, NoiseReduction::Off);
        assert!(!s.to_json().contains("noise_removal"));
        assert!(s.to_json().contains(r#""noise_reduction": "strong""#));
    }
}

#[cfg(test)]
mod placement_tests {
    use super::PlacementCfg;

    fn cfg(x: f32, y: f32, w: f32, h: f32, frame_aspect: Option<f32>) -> PlacementCfg {
        PlacementCfg { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, frame_aspect }
    }

    fn pixel_aspect(p: &PlacementCfg, frame: f32) -> f32 {
        p.w * frame / p.h
    }

    #[test]
    fn same_frame_shape_leaves_it_alone() {
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        assert!(!p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
    }

    /// Where it is in the room it has to move in, across and down.
    fn along(p: &PlacementCfg) -> (f32, f32) {
        (p.x / (1.0 - p.w), p.y / (1.0 - p.h))
    }

    fn inside(p: &PlacementCfg) -> bool {
        p.x >= -1e-6 && p.y >= -1e-6 && p.x + p.w <= 1.0 + 1e-6 && p.y + p.h <= 1.0 + 1e-6
    }

    #[test]
    fn moving_to_an_ultrawide_keeps_its_shape() {
        // A 16:9 box on a 16:9 display, then a 6720x2836 ultrawide.
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        let (ax, ay) = along(&p);
        let uw = 6720.0 / 2836.0;
        assert!(p.refit(uw, None));
        assert!((pixel_aspect(&p, uw) - 16.0 / 9.0).abs() < 1e-4);
        let (bx, by) = along(&p);
        assert!((bx - ax).abs() < 1e-5 && (by - ay).abs() < 1e-5);
        assert!(inside(&p));
        assert_eq!(p.frame_aspect, Some(uw));
    }

    #[test]
    fn a_box_in_a_corner_stays_there_on_a_turned_display() {
        // Bottom right on a 16:9 display, then a portrait one, then back.
        let start = cfg(0.75, 0.75, 0.25, 0.25, Some(16.0 / 9.0));
        let mut p = start;
        assert!(p.refit(9.0 / 16.0, Some(16.0 / 9.0)));
        assert!((p.x + p.w - 1.0).abs() < 1e-5 && (p.y + p.h - 1.0).abs() < 1e-5);
        assert!((pixel_aspect(&p, 9.0 / 16.0) - 16.0 / 9.0).abs() < 1e-4);
        assert!(p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
        for (a, b) in [(p.x, start.x), (p.y, start.y), (p.w, start.w), (p.h, start.h)] {
            assert!((a - b).abs() < 1e-5, "{p:?}");
        }
    }

    #[test]
    fn a_squashed_box_gets_the_cameras_shape_back() {
        // The saved box from the bug report: a thin strip of a cropped 16:9
        // camera, squashed by a frame that changed shape under it. On the
        // display it was laid out for, and on another.
        let squashed = PlacementCfg { crop: [0.137, 0.180, 0.105, 0.051], ..cfg(0.4073, 0.4962, 0.2372, 0.00762, Some(0.5625)) };
        let visible = 16.0 / 9.0 * (1.0 - 0.137 - 0.105) / (1.0 - 0.180 - 0.051);
        for frame in [0.5625, 16.0 / 9.0] {
            let mut p = squashed;
            assert!(p.refit(frame, Some(16.0 / 9.0)));
            assert!((pixel_aspect(&p, frame) - visible).abs() < 1e-3, "{frame}: {p:?}");
            assert!((p.w - squashed.w).abs() < 1e-6 && inside(&p), "{frame}: {p:?}");
            assert!(!p.refit(frame, Some(16.0 / 9.0)), "then it's left alone");
        }
    }

    #[test]
    fn rounding_isnt_squashing() {
        // A 16:9 box laid out on a preview a pixel off the frame's shape.
        let mut p = cfg(0.7, 0.7, 0.25, 0.25 * 1.005, Some(16.0 / 9.0));
        assert!(!p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
    }

    #[test]
    fn the_stretched_box_from_before_gets_the_cameras_shape() {
        // The saved box from the bug report: w = h = 0.2718 (the frame's
        // shape on any display), no frame shape kept, a 16:9 camera.
        let mut p = cfg(0.728, 0.707, 0.2718, 0.2718, None);
        let mac = 3024.0 / 1964.0;
        assert!(!p.refit(mac, None), "waits for the camera");
        assert!(p.refit(mac, Some(16.0 / 9.0)));
        assert!((pixel_aspect(&p, mac) - 16.0 / 9.0).abs() < 1e-4);
    }

    #[test]
    fn never_taller_than_the_frame() {
        // A portrait-shaped box moved to a very wide frame.
        let mut p = cfg(0.4, 0.0, 0.2, 1.0, Some(1.0));
        p.refit(4.0, None);
        assert!(p.h <= 1.0 + 1e-6);
        assert!((pixel_aspect(&p, 4.0) - 0.2).abs() < 1e-4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Presets go up to the display's refresh rate (it included), and to 240
    /// on slower or unknown displays.
    #[test]
    fn fps_presets_follow_the_display() {
        assert_eq!(fps_choices(Some(540)), [30, 60, 120, 144, 165, 240, 360, 480, 540]);
        assert_eq!(fps_choices(Some(175)), [30, 60, 120, 144, 165, 175, 240]);
        assert_eq!(fps_choices(Some(60)), [30, 60, 120, 144, 165, 240]);
        assert_eq!(fps_choices(None), [30, 60, 120, 144, 165, 240]);
    }

    #[test]
    fn blend_rates_go_to_sixteen_times_within_the_limit() {
        let at60 = blend_choices(60, None);
        assert_eq!((at60.first(), at60.last(), at60.len()), (Some(&120), Some(&960), 15));
        assert_eq!(blend_choices(300, None).last(), Some(&1800));
        // macOS: no faster than the display.
        assert_eq!(blend_choices(60, Some(120)), [120]);
    }

    #[test]
    fn a_webcam_from_before_the_switch_is_on() {
        let s: RecordSettings = serde_json::from_str(
            r#"{"webcam": {"id": "cam", "name": "Cam", "placement": {"x": 0.7, "y": 0.7, "w": 0.25, "h": 0.25}}}"#,
        )
        .unwrap();
        assert!(s.webcam.unwrap().enabled);
    }

    #[test]
    fn single_app_setting_becomes_a_list() {
        let mut s: RecordSettings =
            serde_json::from_str(r#"{"fps": 144, "capture": {"type": "app", "id": "game.exe", "name": "Game"}}"#).unwrap();
        s.upgrade_capture();
        assert_eq!(s.fps, 144, "the rest of the settings survive");
        assert_eq!(
            s.capture,
            CaptureTarget::Apps {
                apps: vec![CaptureApp { id: "game.exe".into(), name: "Game".into() }],
                away_screen: true,
            }
        );
        let round: RecordSettings = serde_json::from_str(&s.to_json()).unwrap();
        assert_eq!(round.capture, s.capture);
    }
}
