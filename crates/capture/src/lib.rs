//! Capture backend abstraction.
//!
//! macOS uses `sck` (ScreenCaptureKit + VideoToolbox), Windows uses `win`
//! (Windows Graphics Capture + Media Foundation + WASAPI), Linux uses `linux`
//! (the desktop portal + PipeWire, encoded by ffmpeg), all behind the same
//! `Recorder` trait so the app never depends on which one is live. Mics come
//! from cpal on macOS and Windows, from PipeWire on Linux, and are mixed in
//! `mixer`.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod aac;
pub mod audio;
#[cfg(target_os = "macos")]
mod avwriter;
pub mod mixer;
pub mod monitor;
pub mod mp4meta;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) mod mp4read;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod mp4file;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
mod mp4mux;
pub mod output;
pub mod preview;
pub mod webcam;
pub mod sources;
mod shutter;
mod steady;
pub mod stores;

#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(target_os = "windows")]
pub mod win;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod writer;

/// The in-process hardware video decoder, for playback (same API on both).
pub use shutter::BlendShape;
#[cfg(target_os = "macos")]
pub use mac::decode;
#[cfg(target_os = "windows")]
pub use win::decode;

#[cfg(target_os = "macos")]
pub use sck::SckRecorder;
#[cfg(target_os = "windows")]
pub use win::WinRecorder;
#[cfg(target_os = "linux")]
pub use linux::LinuxRecorder;

/// The capture backend for this platform. `live` gets the meters and supplies
/// each source's volume.
pub fn default_recorder(live: std::sync::Arc<mixer::LiveAudio>) -> Box<dyn Recorder> {
    #[cfg(target_os = "macos")]
    {
        Box::new(SckRecorder::new(live))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(WinRecorder::new(live))
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(LinuxRecorder::new(live))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = live;
        Box::new(Unsupported)
    }
}

/// Stand-in where there's no backend yet, so the app runs (library, editor)
/// and says why capture won't start.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl Recorder for Unsupported {
    fn start(&mut self, _: Mode, _: &EncodeSettings) -> anyhow::Result<()> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn save_clip(&mut self, _: &Path) -> anyhow::Result<PendingClip> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn stop(&mut self, _: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
    fn is_running(&self) -> bool {
        false
    }
}

/// The largest side the H.264 hardware encoders take: VideoToolbox refuses
/// anything wider or taller than 4096 (error -12903; measured with
/// `examples/vt_limits`), as do NVENC, AMF, Quick Sync and VA-API for H.264.
pub const MAX_SIDE: u32 = 4096;

/// The recording's size for a source `w`×`h`: downscaled to `target_height`
/// if that's smaller, and to fit [`MAX_SIDE`] (a 5K2K ultrawide, 6720×2836
/// natively, records at 4096×1728). Keeps the shape; even dimensions, as
/// H.264 4:2:0 requires.
pub fn output_size(w: u32, h: u32, target_height: Option<u32>) -> (u32, u32) {
    let (w, h) = (w.max(2) as f64, h.max(2) as f64);
    let mut scale = target_height.map_or(1.0, |t| (t as f64 / h).min(1.0));
    scale = scale.min(MAX_SIDE as f64 / (w * scale).max(1.0)).min(1.0).min(MAX_SIDE as f64 / h);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(w * scale), even(h * scale))
}

/// How the screen goes into a recording of another shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// Its middle, as much as fills the shape: what's past that is cut off
    /// (an ultrawide screen's sides, recorded 16:9).
    #[default]
    Fill,
    /// All of it, with black bars where the shapes differ.
    Fit,
    /// All of it, stretched or squeezed into the shape.
    Stretch,
}

/// A recording's shape when it isn't the screen's: `w`:`h`, and how the
/// screen goes into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub w: u32,
    pub h: u32,
    pub framing: Framing,
}

impl Shape {
    /// A `w`×`h` screen in this shape, at the screen's own scale: the part
    /// of it kept (Fill), all of it with bars (Fit), or as tall (Stretch).
    pub fn of(&self, w: u32, h: u32) -> (u32, u32) {
        let shape = self.w.max(1) as f64 / self.h.max(1) as f64;
        let (w, h) = (w as f64, h as f64);
        let wider = w / h > shape;
        let (w, h) = match self.framing {
            Framing::Fill if wider => (h * shape, h),
            Framing::Fill => (w, w / shape),
            Framing::Fit if wider => (w, w / shape),
            Framing::Fit => (h * shape, h),
            Framing::Stretch => (h * shape, h),
        };
        (w.round() as u32, h.round() as u32)
    }
}

/// The size a `w`×`h` screen is recorded at: in `shape` if there is one,
/// then as [`output_size`] makes it.
pub fn recorded_size(w: u32, h: u32, target_height: Option<u32>, shape: Option<Shape>) -> (u32, u32) {
    let (w, h) = shape.map_or((w, h), |s| s.of(w, h));
    output_size(w, h, target_height)
}

/// Where a picture of `content`'s size goes in a frame of `frame`'s, each
/// as (left, top, width, height): the part of the picture used, and where
/// it's drawn. Fit uses all of it, black around it (what a recording
/// without a shape does with a window that isn't the screen's shape); Fill
/// cuts off what's past the frame's shape; Stretch draws all of it over all
/// of the frame. Where it's drawn is even, for the encoder's 4:2:0 colour.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn placement((cw, ch): (u32, u32), (fw, fh): (u32, u32), framing: Framing) -> ([u32; 4], [u32; 4]) {
    let (all, whole) = ([0, 0, cw, ch], [0, 0, fw, fh]);
    if cw == 0 || ch == 0 || fw == 0 || fh == 0 {
        return (all, whole);
    }
    // Wider than the frame, as shapes go.
    let wider = cw as u64 * fh as u64 > ch as u64 * fw as u64;
    match framing {
        Framing::Stretch => (all, whole),
        Framing::Fit => {
            let (w, h) = if wider { (fw, (ch as u64 * fw as u64 / cw as u64) as u32) } else { ((cw as u64 * fh as u64 / ch as u64) as u32, fh) };
            let (w, h) = (w & !1, h & !1);
            (all, [((fw - w) / 2) & !1, ((fh - h) / 2) & !1, w, h])
        }
        Framing::Fill => {
            let (w, h) = if wider { ((ch as u64 * fw as u64 / fh as u64) as u32, ch) } else { (cw, (cw as u64 * fh as u64 / fw as u64) as u32) };
            ([(cw - w) / 2, (ch - h) / 2, w, h], whole)
        }
    }
}

/// The size a recording of `source` will be, without starting anything
/// (cheap: asks the system for display sizes). Games and apps record at the
/// main display's size. `None` if the display isn't known (on Linux, until
/// the screen has been picked in the system's dialog once).
pub fn frame_size(source: &VideoSource, target_height: Option<u32>, shape: Option<Shape>) -> Option<(u32, u32)> {
    let id = match source {
        VideoSource::Screen { id, .. } => Some(id.as_str()),
        VideoSource::Apps { .. } => None,
    };
    #[cfg(target_os = "macos")]
    let native = mac::display_pixels(id);
    #[cfg(target_os = "windows")]
    let native = win::display_pixels(id);
    #[cfg(target_os = "linux")]
    let native = linux::display_pixels(id);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let native: Option<(u32, u32)> = {
        let _ = id;
        None
    };
    native.map(|(w, h)| recorded_size(w, h, target_height, shape))
}

/// The fastest a recording of `source` can look at the screen for frame
/// blending, if there's a limit: on macOS the display's refresh rate (games
/// and apps: the main display's), as ScreenCaptureKit only sees what the
/// display shows. Windows takes the game's own pictures: no limit.
pub fn max_blend_rate(source: &VideoSource) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let id = match source {
            VideoSource::Screen { id, .. } => Some(id.as_str()),
            VideoSource::Apps { .. } => None,
        };
        mac::display_refresh(id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = source;
        None
    }
}

/// The refresh rate of the display a recording shows (the fastest display's
/// when recording games and apps), if known: what the frame rate presets go
/// up to.
pub fn display_refresh(source: &VideoSource) -> Option<u32> {
    let id = match source {
        VideoSource::Screen { id, .. } => Some(id.as_str()),
        VideoSource::Apps { .. } => None,
    };
    #[cfg(target_os = "macos")]
    {
        mac::display_refresh(id)
    }
    #[cfg(target_os = "windows")]
    {
        win::system::display_refresh(id)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = id;
        None
    }
}

/// What a recording's video shows. This is the bottom layer of the picture;
/// overlays (a webcam) will go on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSource {
    /// A whole display, by backend id (see [`list_screens`]), captured the
    /// way `method` says (Windows). A game that needs the game capture hook,
    /// fullscreen and in focus on it, is recorded through the hook while it is.
    Screen { id: String, hook: GameHook, method: ScreenCapture },
    /// Games and apps, by executable name (see [`list_windowed_apps`]): records
    /// whichever of them was last in focus, for as long as it's showing, even
    /// while you click into something else. When it stops showing (minimized,
    /// as games are when you alt-tab), it shows the away screen
    /// (`away_when_unfocused`) or keeps its last picture. Fitted into a frame
    /// the size of the main display. The away screen also shows while none of
    /// them is open; each is picked up as soon as it opens.
    Apps { ids: Vec<String>, away_when_unfocused: bool, hook: GameHook },
}

/// When a game is recorded through the game capture hook (Windows) instead
/// of Windows' own capture (whether recording games and apps or the screen), which can't see some games (exclusive fullscreen
/// OpenGL records one frozen frame) and loses the cursor in others. The hook
/// is OBS Studio's, loaded into the game; it's never used on a game with
/// anti-cheat unless it's in `allowed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameHook {
    /// Where it's needed: games known to need it, and games that turn out to
    /// be exclusive fullscreen. Never on a game with anti-cheat.
    pub auto: bool,
    /// Games with anti-cheat (by executable name) it may be used on anyway,
    /// where they need it: the user accepted the risk.
    pub allowed: Vec<String>,
}

impl Default for GameHook {
    fn default() -> Self {
        Self { auto: true, allowed: Vec::new() }
    }
}

/// How a whole display is captured on Windows. Windows Graphics Capture draws
/// a yellow border around the display on Windows 10 (Windows 11 lets us turn
/// it off); Desktop Duplication draws none, but can only capture a display on
/// the graphics card HasteClip uses. Games and apps are always recorded with
/// Windows Graphics Capture (or the game capture hook): Desktop Duplication
/// only captures whole displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenCapture {
    /// Desktop Duplication on Windows 10, where it can capture the display;
    /// Windows Graphics Capture otherwise.
    #[default]
    Auto,
    Duplication,
    Wgc,
}

/// Which way a display is being captured, and whether that's what was asked
/// for, to say so beside the setting: "Desktop Duplication", or "Windows
/// Graphics Capture" when it couldn't. Windows only; `None` while nothing
/// captures a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCaptureInUse {
    pub duplication: bool,
    /// Desktop Duplication was chosen, or picked by Automatic, but couldn't
    /// start or stopped: Windows Graphics Capture is used until it can (it's
    /// tried again now and then).
    pub fell_back: bool,
}

/// How the display being captured now is captured (see [`ScreenCaptureInUse`]).
pub fn screen_capture_in_use() -> Option<ScreenCaptureInUse> {
    #[cfg(target_os = "windows")]
    {
        win::screen_capture_in_use()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// What Automatic picks on this computer, for the setting's note: true when
/// it's Desktop Duplication (Windows 10). Whether a display is on another
/// graphics card is only known when it starts.
pub fn screen_capture_auto_duplicates() -> bool {
    #[cfg(target_os = "windows")]
    {
        win::screen_capture_auto_duplicates()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// How much memory this computer has, in bytes.
pub fn total_memory() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        Some(status.ullTotalPhys)
    }
    #[cfg(target_os = "macos")]
    {
        let mut bytes = 0u64;
        let mut len = std::mem::size_of::<u64>();
        let ok = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut bytes as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
        ok.then_some(bytes)
    }
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Whether the game capture hook can be used here (Windows, with its files
/// next to the app).
pub fn game_hook_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        win::hook::available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// The running apps with a window that use anti-cheat (as far as can be
/// told), and which ("BattlEye").
pub fn running_anticheat_apps() -> Vec<(Device, String)> {
    #[cfg(target_os = "windows")]
    {
        list_windowed_apps()
            .into_iter()
            .filter_map(|app| {
                let ac = win::anticheat::anticheat(&win::system::running_app_path(&app.id)?)?;
                Some((app, ac))
            })
            .collect()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// A still picture shown instead of an app (see [`EncodeSettings::away_screen`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StillImage {
    pub width: u32,
    pub height: u32,
    /// Rows top to bottom, 4 bytes per pixel in B, G, R, A order, opaque.
    pub bgra: Vec<u8>,
}

/// Whether this platform can record games and apps ([`VideoSource::Apps`]).
pub const APP_CAPTURE: bool = cfg!(any(target_os = "windows", target_os = "macos"));

/// Running apps with a window, for [`VideoSource::Apps`], sorted by name.
pub fn list_windowed_apps() -> Vec<Device> {
    #[cfg(target_os = "windows")]
    {
        win::list_windowed_apps()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::list_windowed_apps()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// The icon of an app source's app ([`sources::SourceKind::App`]'s id: a
/// bundle id on macOS, an executable on Windows), as `size`×`size` RGBA with
/// straight alpha. `None` where there isn't one to show: Linux, a program
/// without an app bundle, or on Windows an app that isn't running. Slow-ish
/// (it reads files): call it off the UI thread.
pub fn app_icon(id: &str, size: u32) -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    {
        win::system::app_icon(id, size)
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::app_icon(id, size)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (id, size);
        None
    }
}

/// The app in focus, where the platform can tell: its executable on Windows
/// (`osu!.exe`), its bundle id on macOS (else its executable).
pub fn foreground_exe() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        win::foreground_exe()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::foreground_exe()
    }
    #[cfg(target_os = "linux")]
    {
        linux::focus::foreground_app_path().map(|(id, _)| id)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Whether Windows shows the cursor, and its image's handle (for a debug log).
#[cfg(target_os = "windows")]
pub fn win_cursor_state() -> Option<(bool, usize)> {
    win::cursor_state()
}

/// The app in focus: its id (as [`foreground_exe`]) and where it's installed,
/// to tell a game by its folder (`…/steamapps/common/<game>/…`).
pub fn foreground_app_path() -> Option<(String, PathBuf)> {
    #[cfg(target_os = "windows")]
    {
        win::foreground_app_path()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::foreground_app_path()
    }
    #[cfg(target_os = "linux")]
    {
        linux::focus::foreground_app_path()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// The file name of a running app's bundle (`World of Warcraft.app`), by
/// its id (bundle id, or executable for an app without one).
#[cfg(target_os = "macos")]
pub fn app_file_name(id: &str) -> Option<String> {
    mac::windows::bundle_file_name(id)
}

/// Whether [`list_screens`] lists the actual screens, to pick from in the
/// app. Not on Linux under Wayland, where the desktop's own dialog picks one
/// (see [`choose_screen_again`]).
pub fn screens_listed() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::screens_listed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Have the desktop's dialog ask again which screen to record (Linux under
/// Wayland; elsewhere screens are picked in the app, and this does nothing).
pub fn choose_screen_again() {
    #[cfg(target_os = "linux")]
    linux::choose_screen_again();
}

/// Screens the backend can capture; their ids go in [`VideoSource::Screen`].
pub fn list_screens() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_shareable().screens
    }
    #[cfg(target_os = "windows")]
    {
        win::list_screens()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_screens()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Running apps the user could add as an audio source, sorted by name. Their
/// ids go in `SourceKind::App` (a bundle id on macOS, an executable name on
/// Windows, the program's name on Linux). On Linux only apps that have
/// opened their sound are listed: PipeWire knows nothing of the rest.
pub fn list_apps() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_apps()
    }
    #[cfg(target_os = "windows")]
    {
        win::list_apps()
    }
    #[cfg(target_os = "linux")]
    {
        linux::audio::list_apps()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Screen-capture permission state, so the UI can guide the user instead of
/// silently failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Granted,
    Denied,
    /// Platform doesn't gate screen capture this way (or not yet implemented).
    NotApplicable,
}

/// Current screen-recording permission for this process.
pub fn screen_permission() -> Permission {
    #[cfg(target_os = "macos")]
    {
        if macos::screen_recording_authorized() {
            Permission::Granted
        } else {
            Permission::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permission::NotApplicable
    }
}

/// Ask the OS to prompt for screen-recording permission (macOS: first run only).
pub fn request_screen_permission() {
    #[cfg(target_os = "macos")]
    {
        let _ = macos::request_screen_recording();
    }
}

/// How the recorder is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Continuously buffer the last N seconds; `save_clip` flushes it to a file.
    ReplayBuffer,
    /// Record straight to disk until stopped.
    Record,
}

/// A capture device (screen, app, or audio device) as seen by the backend.
#[derive(Debug, Clone)]
pub struct Device {
    /// Backend-specific id (display id, bundle id or executable, device name).
    pub id: String,
    pub name: String,
}

/// Displays and apps the backend can capture.
#[derive(Debug, Clone, Default)]
pub struct Devices {
    pub screens: Vec<Device>,
    /// Running applications (for per-app audio capture on macOS/Windows).
    pub apps: Vec<Device>,
}

static FRAMES_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FRAMES_ENCODED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many frames the running capture has handed to the video encoder (to
/// tell what share [`frames_skipped`] is). Starts from 0 with each capture;
/// only counted on Windows so far.
pub fn frames_encoded() -> u64 {
    FRAMES_ENCODED.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many frames the running capture has left out because the video
/// encoder, or the graphics card making the frames, couldn't keep up (the
/// file shows the previous one longer). Starts from 0 with each capture; only
/// counted on Windows so far.
pub fn frames_skipped() -> u64 {
    FRAMES_SKIPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// AAC bitrate per audio track, bits per second.
pub const AUDIO_BITRATE: u32 = 192_000;

/// Frames from one keyframe to the next when recording at `fps`: one a
/// second, but never more than 120 frames apart. A clip saved from the replay
/// buffer starts on a keyframe (so at most a second early), and scrubbing and
/// saving an edit decode from the keyframe before a frame: the gap in frames is
/// what they cost (at 240 fps, a keyframe every 2 s made scrubbing back
/// stall). Measured with the NVIDIA hardware encoder at 15 Mbps, 5 s of osu!
/// and of CS2: at 240 fps, 120 frames instead of 480 made files 0.4-4%
/// bigger and VMAF at most 0.5 lower; at 60 fps, a second instead of two,
/// about 1% bigger and at most 0.13 lower.
pub fn keyframe_every(fps: u32) -> u32 {
    fps.clamp(1, 120)
}

/// Everything the backend needs to start a capture. The app builds this from its
/// own richer `RecordSettings`.
#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub output_dir: PathBuf,
    /// Container extension for finished files, e.g. "mp4" or "mkv".
    pub container_ext: String,
    pub fps: u32,
    /// Windows and macOS: look at the picture this many times per frame and record the
    /// average (smooth, like motion blur). 1 records the newest picture as is.
    pub blend_samples: u32,
    /// How the looks in a blended frame are weighed.
    pub blend_shape: BlendShape,
    pub video_bitrate_kbps: u32,
    /// Downscale target height, or `None` to keep native resolution.
    pub target_height: Option<u32>,
    /// A shape to record in, when not the screen's.
    pub shape: Option<Shape>,
    /// Use a hardware encoder (VideoToolbox, or the GPU's Media Foundation
    /// encoder on Windows) vs. software.
    pub use_hardware: bool,
    pub replay_seconds: u32,
    /// What the video shows.
    pub video: VideoSource,
    /// Shown while recording games and apps and there's nothing to show (none
    /// is open, or you've tabbed out); black if `None`. Scaled to the frame.
    pub away_screen: Option<std::sync::Arc<StillImage>>,
    /// A webcam drawn over the picture.
    pub webcam: Option<webcam::Webcam>,
    /// Audio sources, in track order. Track 1 is the mix of every source with
    /// `in_mix` (so the file sounds right in any player); each source with
    /// `own_track` follows on its own track, for rebalancing later.
    pub sources: Vec<sources::AudioSource>,
}

/// A replay clip being saved: what it shows is settled, the file is still
/// being written. [`PendingClip::finish`] waits for it (seconds, for a long
/// buffer), so call it off the UI and capture threads.
pub struct PendingClip(Box<dyn FnOnce() -> anyhow::Result<PathBuf> + Send>);

impl PendingClip {
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<PathBuf> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// Wait for the clip to be written; its path.
    pub fn finish(self) -> anyhow::Result<PathBuf> {
        (self.0)()
    }
}

/// Whether [`Recorder::screenshot`] can take one here (Windows, so far).
pub const SCREENSHOTS: bool = cfg!(target_os = "windows");

/// A screenshot being taken: its picture is the one up when it was asked
/// for, copied aside on the graphics card; [`PendingShot::finish`] reads it
/// back (a few ms), so call it off the capture thread.
pub struct PendingShot(Box<dyn FnOnce() -> anyhow::Result<StillImage> + Send>);

impl PendingShot {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<StillImage> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// The picture, opaque.
    pub fn finish(self) -> anyhow::Result<StillImage> {
        (self.0)()
    }
}

/// A capture backend. Implemented by `SckRecorder` (macOS), `WinRecorder`
/// (Windows) and `LinuxRecorder` (Linux).
pub trait Recorder {
    /// Begin capturing in the given mode with the given settings.
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> anyhow::Result<()>;

    /// In `ReplayBuffer` mode, save the buffered window as a clip in `dir` (a
    /// game's folder in the library, say). Its moment is taken right away; the
    /// returned [`PendingClip`] finishes writing it, which can take a while — so
    /// another can be saved meanwhile.
    fn save_clip(&mut self, dir: &Path) -> anyhow::Result<PendingClip>;

    /// Stop capturing. In `Record` mode, returns the finished file, moved into
    /// `dir` if given (it's written in the output folder: which game it was is
    /// only known at the end).
    fn stop(&mut self, dir: Option<&Path>) -> anyhow::Result<Option<PathBuf>>;

    /// Whether a capture is currently running.
    fn is_running(&self) -> bool;

    /// Change what the running capture records, without restarting it, if the
    /// change allows (a different list of games and apps). Returns whether it
    /// was applied; otherwise it takes effect on the next start.
    fn update_video(&mut self, _video: &VideoSource) -> bool {
        false
    }

    /// Change how far back the running replay buffer reaches (and Save clip
    /// saves), without restarting it.
    fn set_replay_seconds(&mut self, _seconds: u32) {}

    /// Switch the running capture's microphone source `id` to `device`
    /// (its track stays). Returns whether it was applied.
    fn set_mic(&mut self, _id: &str, _device: &str) -> bool {
        false
    }

    /// A screenshot of `video` now, as it's captured: every pixel of the
    /// screen or the game's window, whatever shape and size recordings are
    /// made in, without the webcam. From the running capture, or one of its
    /// own when nothing runs. See [`SCREENSHOTS`].
    fn screenshot(&mut self, _video: &VideoSource) -> anyhow::Result<PendingShot> {
        anyhow::bail!("screenshots aren't available on this platform yet")
    }
}

#[cfg(test)]
mod size_tests {
    use super::{Framing, Shape, output_size, placement, recorded_size};

    #[test]
    fn output_size_keeps_aspect_and_even() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
        assert_eq!(output_size(3440, 1440, Some(721)), (1722, 720));
    }

    #[test]
    fn output_size_fits_the_encoder() {
        assert_eq!(output_size(6720, 2836, None), (4096, 1728));
        assert_eq!(output_size(7680, 4320, Some(2160)), (3840, 2160));
        assert_eq!(output_size(5120, 2880, None), (4096, 2304));
        // Portrait.
        assert_eq!(output_size(2160, 7680, None), (1152, 4096));
        assert_eq!(output_size(4096, 2304, None), (4096, 2304));
    }

    /// An ultrawide screen recorded 16:9 three ways, and at 1080p.
    #[test]
    fn shapes() {
        let wide = |framing| Some(Shape { w: 16, h: 9, framing });
        assert_eq!(recorded_size(3440, 1440, None, wide(Framing::Fill)), (2560, 1440));
        assert_eq!(recorded_size(3440, 1440, None, wide(Framing::Fit)), (3440, 1934));
        assert_eq!(recorded_size(3440, 1440, None, wide(Framing::Stretch)), (2560, 1440));
        assert_eq!(recorded_size(3440, 1440, Some(1080), wide(Framing::Fill)), (1920, 1080));
        // Upright from a 16:9 screen: its middle.
        assert_eq!(recorded_size(1920, 1080, None, Some(Shape { w: 9, h: 16, framing: Framing::Fill })), (608, 1080));
        assert_eq!(recorded_size(3440, 1440, None, None), (3440, 1440));
    }

    #[test]
    fn where_the_picture_goes() {
        // Fill: the middle 2560 of 3440, over all of the frame.
        assert_eq!(placement((3440, 1440), (2560, 1440), Framing::Fill), ([440, 0, 2560, 1440], [0, 0, 2560, 1440]));
        // Fit: all of it, bars above and below.
        assert_eq!(placement((3440, 1440), (1920, 1080), Framing::Fit), ([0, 0, 3440, 1440], [0, 138, 1920, 802]));
        assert_eq!(placement((1600, 1200), (1920, 1080), Framing::Fit), ([0, 0, 1600, 1200], [240, 0, 1440, 1080]));
        assert_eq!(placement((2560, 1440), (1920, 1080), Framing::Fit), ([0, 0, 2560, 1440], [0, 0, 1920, 1080]));
        // Stretch: all of it everywhere.
        assert_eq!(placement((3440, 1440), (1920, 1080), Framing::Stretch), ([0, 0, 3440, 1440], [0, 0, 1920, 1080]));
        // Nothing to place yet.
        assert_eq!(placement((0, 0), (1920, 1080), Framing::Fit), ([0, 0, 0, 0], [0, 0, 1920, 1080]));
    }
}

#[cfg(test)]
mod app_icon_tests {
    /// An app that's installed on every Mac has an icon; a made-up one doesn't.
    #[test]
    #[cfg(target_os = "macos")]
    fn finder_has_an_icon() {
        let px = super::app_icon("com.apple.finder", 32).expect("Finder's icon");
        assert_eq!(px.len(), 32 * 32 * 4);
        assert!(px.chunks_exact(4).any(|p| p[3] == 255), "some of it is opaque");
        assert!(px.chunks_exact(4).filter(|p| p[3] > 0).count() > 200, "it covers most of the square");
        assert!(super::app_icon("no.such.app.hasteclip", 32).is_none());
    }
}
