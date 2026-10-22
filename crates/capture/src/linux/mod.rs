//! Native Linux capture backend: the screen from the X server on X11, from
//! the compositor on wlroots-style Wayland desktops, KDE Plasma and GNOME,
//! else through the ScreenCast desktop portal + PipeWire, encoded by ffmpeg.
//! Works on every X11 desktop, Sway, Hyprland and their kin, KDE Plasma,
//! GNOME, and other Wayland desktops that have a portal.
//!
//! - **Video**: on X11 the monitor picked in the app is read straight from
//!   the X server (`x11`); on wlroots-style desktops it's copied from the
//!   compositor (`wlr`); on KDE Plasma KWin streams it over PipeWire
//!   (`kwin`), on GNOME Mutter does (`mutter`). Elsewhere the portal asks which screen to share (once; the
//!   choice is remembered) and hands over a PipeWire stream of it (`portal`,
//!   `screen`). A pacer thread takes the newest picture every 1/fps, scales
//!   it to the output size, draws the webcam over it and converts it to NV12
//!   (`image`), so the output is constant frame rate even when the screen is
//!   still and nothing new arrives. ffmpeg encodes it (`ffmpeg`): NVENC or
//!   VA-API on the GPU where it can, x264 or OpenH264 otherwise.
//! - **Audio**: everything through PipeWire (`audio`): mics, the desktop
//!   (every app but us), and single apps. **Mixing** happens in Rust
//!   (`crate::mixer`), which also drives the live meters; each track is
//!   AAC-encoded by ffmpeg.
//! - **Writing** (`crate::writer`): our own MP4 muxer (`crate::mp4file`),
//!   either straight to a fragmented MP4 (record) or from an in-memory replay
//!   ring when a clip is saved — the same as on Windows.
//!
//! Everything is timestamped on CLOCK_MONOTONIC, the clock PipeWire uses: the
//! first video frame is t=0.
//!
//! Recording games and apps by their windows isn't offered: Wayland doesn't
//! let an app see which window is in front, or capture one without asking.

mod apps;
pub mod audio;
mod camera;
pub mod ffmpeg;
mod image;
pub mod focus;
mod kwin;
mod kwin_windows;
mod mutter;
mod portal;
mod screen;
mod wlr;
mod wlr_windows;
mod x11;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub use camera::{keep_open as keep_camera_open, list_cameras};

use crate::mixer::{self, Clock, LiveAudio};
use crate::mp4file::Layout;
use crate::mp4mux::Spec;
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::webcam::{Placement, SharedPlacement};
use crate::writer::{self, Writer};
use crate::{AUDIO_BITRATE, Device, EncodeSettings, Mode, Recorder, VideoSource};
use image::Canvas;

/// CLOCK_MONOTONIC in seconds: PipeWire's clock, and so every timestamp's.
pub(crate) fn host_now() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// The one screen entry on Wayland: which screen is picked in the desktop's own dialog.
pub(crate) const SCREEN_ID: &str = "portal";

/// Whether the screens to record are picked in the app (X11, wlroots, KDE,
/// GNOME), not in the desktop's own dialog (other desktops, via the portal).
pub fn screens_listed() -> bool {
    x11::session() || wlr::available() || kwin::available() || mutter::available()
}

/// Screens to record. On X11, KDE and GNOME, every monitor, the main one
/// first; on wlroots-style desktops every monitor, leftmost first. Elsewhere the
/// desktop doesn't tell apps what screens there are, so there's one entry,
/// and the desktop asks which screen it is the first time.
pub fn list_screens() -> Vec<Device> {
    if x11::session() {
        let mut monitors = x11::monitors();
        monitors.sort_by_key(|m| (!m.primary, m.x, m.y));
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    } else if wlr::available() {
        let monitors = wlr::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| Device { name: format!("{} ({}×{})", m.name, m.width, m.height), id: m.id })
                .collect();
        }
    } else if kwin::available() {
        let monitors = kwin::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    } else if mutter::available() {
        let monitors = mutter::monitors();
        if !monitors.is_empty() {
            return monitors
                .into_iter()
                .map(|m| {
                    let main = if m.primary { ", main" } else { "" };
                    Device { name: format!("{} ({}×{}{main})", m.name, m.width, m.height), id: m.id }
                })
                .collect();
        }
    }
    vec![Device { id: SCREEN_ID.into(), name: "Screen (picked when recording first starts)".into() }]
}

/// Whether games and apps can be recorded (KDE Plasma, so far).
pub(crate) fn app_capture() -> bool {
    apps::available()
}

/// Running programs with a window, for `VideoSource::Apps`.
pub(crate) fn list_windowed_apps() -> Vec<Device> {
    if apps::available() { apps::list() } else { Vec::new() }
}

/// Forget which screen was picked, so the next capture (the preview, when
/// it restarts) asks the desktop again.
pub fn choose_screen_again() {
    portal::forget_choice();
    screen::forget();
}

/// The screen's size in pixels: on X11, wlroots, KDE and GNOME the
/// monitor's, else the last one the portal shared.
pub(crate) fn display_pixels(id: Option<&str>) -> Option<(u32, u32)> {
    if x11::session()
        && let Some(m) = x11::find(&x11::monitors(), id.unwrap_or(""))
    {
        return Some((m.width as u32, m.height as u32));
    }
    if !x11::session() && wlr::available() {
        let monitors = wlr::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    if !x11::session() && kwin::available() {
        let monitors = kwin::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    if !x11::session() && mutter::available() {
        let monitors = mutter::monitors();
        if let Some(m) = monitors.iter().find(|m| Some(m.id.as_str()) == id).or(monitors.first()) {
            return Some((m.width, m.height));
        }
    }
    portal::last_size()
}

pub struct LinuxRecorder {
    session: Option<Session>,
    mode: Option<Mode>,
    current_file: Option<PathBuf>,
    replay_seconds: u32,
    output_dir: PathBuf,
    container_ext: String,
    live: Arc<LiveAudio>,
}

impl LinuxRecorder {
    /// `live` receives the meters and supplies each source's volume.
    pub fn new(live: Arc<LiveAudio>) -> Self {
        Self {
            session: None,
            mode: None,
            current_file: None,
            replay_seconds: 60,
            output_dir: PathBuf::from("."),
            container_ext: "mp4".to_string(),
            live,
        }
    }
}

impl Recorder for LinuxRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        if self.session.is_some() {
            let _ = self.stop(None);
        }
        self.replay_seconds = settings.replay_seconds;
        self.output_dir = settings.output_dir.clone();
        // Our muxer writes MP4 and MOV; MKV isn't one of them.
        self.container_ext = if settings.container_ext == "mov" { "mov".into() } else { "mp4".into() };
        std::fs::create_dir_all(&settings.output_dir)?;

        let s = settings.clone();
        let target = match mode {
            Mode::Record => {
                let file = self.output_dir.join(format!("recording_{}.{}", timestamp(), self.container_ext));
                self.current_file = Some(file.clone());
                Target::Record(in_progress(&file))
            }
            Mode::ReplayBuffer => {
                self.current_file = None;
                Target::Replay(settings.replay_seconds as f64)
            }
        };
        self.session = Some(Session::start(&s, target, &self.live, self.container_ext == "mov")?);
        self.mode = Some(mode);
        Ok(())
    }

    fn save_clip(&mut self, dir: &Path) -> Result<crate::PendingClip> {
        let session = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer));
        let session = session.context("replay buffer is not running")?;
        std::fs::create_dir_all(dir)?;
        writer::request_clip(&session.writer_tx, dir, &self.container_ext, self.replay_seconds as f64)
    }

    fn stop(&mut self, dir: Option<&Path>) -> Result<Option<PathBuf>> {
        let finished = self.session.take().map(Session::finish);
        let file = self.current_file.take();
        let was_record = self.mode == Some(Mode::Record);
        self.mode = None;
        if !was_record {
            return Ok(None);
        }
        let Some(file) = file else { return Ok(None) };
        let partial = in_progress(&file);
        if let Some(Err(e)) = finished {
            // A fragmented MP4 is still playable up to the failure: keep what we got.
            if !partial.exists() {
                return Err(e.context("recording failed"));
            }
            eprintln!("recording ended with an error: {e:#}");
        }
        if !partial.exists() {
            bail!("recording failed — nothing was written");
        }
        let file = crate::output::destination(&file, dir);
        crate::output::finish_rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }

    fn set_replay_seconds(&mut self, seconds: u32) {
        self.replay_seconds = seconds;
        if let Some(session) = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer)) {
            let _ = session.writer_tx.send(writer::Command::SetWindow(seconds as f64));
        }
    }

    fn screenshot(&mut self, video: &VideoSource) -> Result<crate::PendingShot> {
        if let Some(feed) = self.session.as_ref().and_then(|s| s.feed.as_ref()) {
            return shot_of(feed);
        }
        // Nothing running: a capture of its own, for one picture. A screen
        // has its picture by now; games and apps take a moment to find the
        // one that's showing.
        let feed = Feed::open(video, None)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while (feed.get().is_none() || feed.waiting()) && Instant::now() < deadline {
            feed.want();
            thread::sleep(Duration::from_millis(10));
        }
        let shot = shot_of(&feed);
        feed.release();
        shot
    }

    fn set_mic(&mut self, id: &str, device: &str) -> bool {
        self.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }

    fn update_video(&mut self, video: &VideoSource) -> bool {
        match (self.session.as_ref().and_then(|s| s.feed.as_ref()), video) {
            (Some(Feed::Apps(a)), VideoSource::Apps { ids, away_when_unfocused, .. }) => {
                a.set(ids, *away_when_unfocused);
                true
            }
            _ => false,
        }
    }
}

/// A screenshot of the newest picture: all of the screen, or the game's
/// window, at its own size, without the webcam. Made opaque B, G, R, A rows
/// when finished.
fn shot_of(feed: &Feed) -> Result<crate::PendingShot> {
    if feed.waiting() {
        bail!("none of the games and apps you record is showing");
    }
    let (frame, _) = feed.get().context("nothing has been captured yet. Try again in a moment")?;
    Ok(crate::PendingShot::new(move || {
        let row = frame.width as usize * 4;
        let mut bgra = Vec::with_capacity(row * frame.height as usize);
        for line in frame.data.chunks(frame.stride).take(frame.height as usize) {
            for p in line[..row].chunks_exact(4) {
                match frame.order {
                    screen::Order::Bgr => bgra.extend_from_slice(&[p[0], p[1], p[2], 255]),
                    screen::Order::Rgb => bgra.extend_from_slice(&[p[2], p[1], p[0], 255]),
                }
            }
        }
        Ok(crate::StillImage { width: frame.width, height: frame.height, bgra })
    }))
}

impl Drop for LinuxRecorder {
    /// Dropping (app quit) must still finalise the file.
    fn drop(&mut self) {
        if self.session.is_some() {
            let _ = self.stop(None);
        }
    }
}

/// Where a session's media goes.
enum Target {
    /// Straight to this (hidden, partial) file.
    Record(PathBuf),
    /// An in-memory ring of this many seconds.
    Replay(f64),
}

/// How far behind real time the mixer runs, so every source's audio has
/// arrived (PipeWire delivers every ~10–20 ms, sometimes late). Timestamps
/// are exact, so this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Where the pictures come from: a screen, or games and apps.
#[derive(Clone)]
enum Feed {
    Screen(Arc<screen::Screen>),
    Apps(Arc<apps::Apps>),
}

impl Feed {
    /// Start `source`'s pictures. Games and apps show the away screen (else
    /// black) until one of them is found.
    fn open(source: &VideoSource, away_screen: Option<&crate::StillImage>) -> Result<Self> {
        match source {
            VideoSource::Screen { id, .. } => Ok(Self::Screen(screen::acquire(id)?)),
            VideoSource::Apps { ids, away_when_unfocused, .. } if apps::available() => {
                Ok(Self::Apps(apps::Apps::start(ids, *away_when_unfocused, away_screen)))
            }
            VideoSource::Apps { .. } => bail!("recording games and apps is only available on KDE Plasma so far — record the screen instead"),
        }
    }

    /// The size pictures are fitted into before scaling: the screen's, or
    /// for games and apps the main display's.
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Screen(s) => s.latest.get().map_or((1920, 1080), |(f, _)| (f.width, f.height)),
            Self::Apps(_) => display_pixels(None).unwrap_or((1920, 1080)),
        }
    }

    /// Ask a source that reads the screen on request (X11) for the next picture.
    fn want(&self) {
        if let Self::Screen(s) = self {
            s.latest.want();
        }
    }

    /// The newest picture and a number that changes with it.
    fn get(&self) -> Option<(Arc<screen::Frame>, u64)> {
        match self {
            Self::Screen(s) => s.latest.get(),
            Self::Apps(a) => a.get(),
        }
    }

    /// Why the pictures stopped, if they did (games and apps don't stop).
    fn ended(&self) -> Option<String> {
        match self {
            Self::Screen(s) => s.latest.ended(),
            Self::Apps(_) => None,
        }
    }

    /// Whether none of the games and apps recorded is showing.
    fn waiting(&self) -> bool {
        matches!(self, Self::Apps(a) if a.waiting())
    }

    /// The game or app whose window is recorded, while it shows.
    fn app(&self) -> Option<String> {
        match self {
            Self::Apps(a) => a.app(),
            Self::Screen(_) => None,
        }
    }

    /// Let go. A screen keeps running a little longer, in case it's taken up
    /// again right away (`screen::release`).
    fn release(self) {
        if let Self::Screen(s) = self {
            screen::release(s);
        }
    }
}

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    feed: Option<Feed>,
    pacer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    audio: Option<AudioCapture>,
    mixer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
}

impl Session {
    fn start(s: &EncodeSettings, target: Target, live: &Arc<LiveAudio>, mov: bool) -> Result<Self> {
        let feed = Feed::open(&s.video, s.away_screen.as_deref())?;
        let (canvas_w, canvas_h) = feed.size();
        let (width, height) = crate::recorded_size(canvas_w, canvas_h, s.target_height, s.shape);
        let framing = s.shape.map_or(crate::Framing::Fit, |shape| shape.framing);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let tracks = titles.len();
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session = Self {
            feed: Some(feed.clone()),
            pacer: None,
            audio: None,
            mixer: None,
            encoders: Vec::new(),
            writer_tx,
            writer: Some(writer),
        };
        let started = (|| -> Result<()> {
            // One PCM channel + AAC encoder per track, in layout order: the
            // mix, each own track, the rest.
            let mut pcm_senders = Vec::new();
            for track in 0..tracks {
                let (tx, rx) = mpsc::channel::<Vec<f32>>();
                pcm_senders.push(tx);
                session.encoders.push(ffmpeg::spawn_aac(track, AUDIO_BITRATE, rx, session.writer_tx.clone())?);
            }
            let mut tracks = pcm_senders.into_iter();
            let mix_track = tracks.next();
            let audio = AudioCapture::start(&s.sources, live, clock.clone())?;
            let inputs = mix_inputs(&s.sources, &audio.feeds, live, |_| tracks.next());
            let rest_track = if has_rest { tracks.next() } else { None };
            session.audio = Some(audio);
            let stop = Arc::new(AtomicBool::new(false));
            let mixer = mixer::spawn_mixer(inputs, mix_track, rest_track, live.clone(), clock.clone(), host_now, MIX_LATENCY, stop.clone());
            session.mixer = Some((stop, mixer));

            let encoder = ffmpeg::H264::start(width, height, s, session.writer_tx.clone())?;
            let picture = Picture::new(feed.clone(), width, height, framing, s.webcam.as_ref());
            session.pacer = Some(spawn_pacer(s.fps, picture, encoder, clock));
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.finish();
            return Err(e);
        }
        Ok(session)
    }

    /// Stop capturing and finish the file (record) or drop the ring (replay).
    fn finish(mut self) -> Result<()> {
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        if let Some(feed) = self.feed.take() {
            feed.release();
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        // The mixer writes what's left and drops its senders; the AAC
        // encoders then flush and exit.
        if let Some((stop, handle)) = self.mixer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        for h in self.encoders.drain(..) {
            let _ = h.join();
        }
        drop(self.writer_tx);
        self.writer.take().map_or(Ok(()), Writer::join)
    }
}

/// Makes the frame: the screen put into the output size (`framing`), the
/// webcam on top. Redraws only when one of them changed.
struct Picture {
    feed: Feed,
    canvas: Canvas,
    framing: crate::Framing,
    camera: Option<(camera::CameraLayer, SharedPlacement)>,
    /// What the canvas shows: the screen picture's number, the camera picture
    /// and where it was placed.
    shows: Option<(u64, Option<usize>, Option<Placement>)>,
    /// The canvas as NV12, made when it changes.
    nv12: Vec<u8>,
    /// Counts the canvas's changes, so the preview is only sent new ones.
    version: u64,
}

impl Picture {
    fn new(feed: Feed, width: u32, height: u32, framing: crate::Framing, webcam: Option<&crate::webcam::Webcam>) -> Self {
        let camera = webcam.map(|w| (camera::CameraLayer::new(w), w.placement.clone()));
        let mut canvas = Canvas::new(width, height);
        canvas.clear();
        Self { feed, canvas, framing, camera, shows: None, nv12: Vec::new(), version: 0 }
    }

    /// Bring the canvas up to date. Returns whether there's a picture yet.
    fn update(&mut self, nv12: bool) -> bool {
        // A source that reads the screen on request (X11) reads the next one now.
        self.feed.want();
        let Some((frame, seq)) = self.feed.get() else { return false };
        let (cam, place) = match &mut self.camera {
            Some((layer, place)) => {
                let place = *place.lock().unwrap();
                (layer.latest().filter(|_| !place.is_hidden()), Some(place))
            }
            None => (None, None),
        };
        let now = (seq, cam.as_ref().map(|c| Arc::as_ptr(c) as usize), place);
        if self.shows.as_ref() != Some(&now) {
            self.canvas.fill_from(&frame, self.framing);
            if let (Some(cam), Some(place)) = (&cam, &place) {
                self.canvas.draw_camera(cam, place);
            }
            self.shows = Some(now);
            self.nv12.clear();
            self.version += 1;
        }
        if nv12 && self.nv12.is_empty() {
            self.canvas.to_nv12(&mut self.nv12);
        }
        true
    }
}

/// The largest the preview is made: it's shown in a box a third of the
/// window tall, so more is only more to convert and upload every frame.
const PREVIEW_MAX: (u32, u32) = (1280, 720);

/// What's been sent to the preview.
struct Previewed {
    generation: u64,
    /// The canvas version last sent.
    version: Option<u64>,
    /// The last frame sent, whose memory the next one reuses once the UI is
    /// done with it.
    spare: Option<Arc<crate::preview::PreviewFrame>>,
}

impl Previewed {
    /// For the producer of this generation (`crate::preview::new_producer`).
    fn new(generation: u64) -> Self {
        Self { generation, version: None, spare: None }
    }
}

/// Hand the preview the picture, if it's wanted and has changed since the
/// last one (a still screen sends nothing new: no work here, and none for
/// the UI to upload).
fn submit_preview(picture: &Picture, sent: &mut Previewed) {
    if sent.version == Some(picture.version) || !crate::preview::wants_frame() {
        return;
    }
    sent.version = Some(picture.version);
    let canvas = &picture.canvas;
    let mut rgba = sent.spare.take().and_then(|f| Arc::try_unwrap(f).ok()).map(|f| f.rgba).unwrap_or_default();
    let (w, h) = canvas.to_preview(PREVIEW_MAX, &mut rgba);
    let frame = crate::preview::PreviewFrame {
        width: w,
        height: h,
        recorded: (canvas.width, canvas.height),
        rgba,
        seq: 0,
        waiting: picture.feed.waiting(),
        app: picture.feed.app(),
    };
    sent.spare = Some(crate::preview::publish_frame(sent.generation, frame));
}

/// Encode the newest picture every 1/fps on the host clock. The screen only
/// sends pictures when something changes; repeating the last one keeps the
/// output constant frame rate. Frame N is stamped N/fps seconds after t0, the
/// same clock the audio uses.
fn spawn_pacer(fps: u32, mut picture: Picture, encoder: ffmpeg::H264, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::Builder::new()
        .name("pacer".into())
        .spawn(move || {
            let mut sent = Previewed::new(crate::preview::new_producer());
            let fps_f = fps as f64;
            let t0 = host_now();
            clock.set(t0);
            let mut next: i64 = 0;
            let mut dropped = 0u64;
            let mut ended = false;
            while !stop2.load(Ordering::Relaxed) {
                let due = ((host_now() - t0) * fps_f) as i64;
                // After a stall, catch up gradually rather than in one huge burst.
                let mut burst = 0;
                while next <= due && burst < fps * 2 {
                    if picture.update(true) {
                        let mut frame = encoder.buffer();
                        frame.clear();
                        frame.extend_from_slice(&picture.nv12);
                        // The encoder can't keep up: skip this frame (the file
                        // shows the previous one a little longer).
                        if !encoder.encode(frame, next) {
                            dropped += 1;
                            if dropped.is_power_of_two() {
                                eprintln!("video encoder is behind: {dropped} frames skipped");
                            }
                        }
                    }
                    next += 1;
                    burst += 1;
                }
                if !ended {
                    if let Some(why) = picture.feed.ended() {
                        // Sharing was stopped from the desktop: the file keeps the last picture.
                        eprintln!("{why}");
                        ended = true;
                    }
                }
                // The Sources page is showing the preview: hand it this picture too.
                submit_preview(&picture, &mut sent);
                let wait = t0 + next as f64 / fps_f - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                }
            }
            encoder.finish();
            crate::preview::clear(sent.generation);
        })
        .expect("spawning the pacer thread");
    (stop, handle)
}

/// A capture made only for the preview (`crate::preview::VideoPreview`). Runs
/// entirely on its own thread: starting doesn't wait for the capture to open
/// (or for the desktop's "which screen?" dialog), and dropping doesn't wait
/// for it to close.
pub(crate) struct PreviewCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl PreviewCapture {
    pub(crate) fn start(source: VideoSource, target_height: Option<u32>, shape: Option<crate::Shape>, fps: u32, webcam: Option<crate::webcam::Webcam>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (stop2, error2) = (stop.clone(), error.clone());
        let generation = crate::preview::new_producer();
        thread::spawn(move || {
            let feed = match Feed::open(&source, None) {
                Ok(f) => f,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    return;
                }
            };
            let (w, h) = feed.size();
            let (width, height) = crate::recorded_size(w, h, target_height, shape);
            let framing = shape.map_or(crate::Framing::Fit, |shape| shape.framing);
            let mut picture = Picture::new(feed.clone(), width, height, framing, webcam.as_ref());
            let mut sent = Previewed::new(generation);
            // Paced like the recording would be.
            let every = 1.0 / fps.max(1) as f64;
            let t0 = host_now();
            let mut n = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                if let Some(why) = picture.feed.ended() {
                    *error2.lock().unwrap() = Some(why);
                    break;
                }
                if crate::preview::wants_frame() && picture.update(false) {
                    submit_preview(&picture, &mut sent);
                }
                n += 1;
                let wait = t0 + n as f64 * every - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                } else if wait < -0.5 {
                    n = ((host_now() - t0) / every) as u64; // fell far behind: skip ahead
                }
            }
            drop(picture);
            feed.release();
        });
        Self { stop, error }
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for PreviewCapture {
    /// Tell the thread to stop; it closes the capture by itself.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording games and apps follows the picked program's window once
    /// it's in focus, at the window's own size, and says which it is.
    #[test]
    #[ignore = "opens a window and captures it: cargo test -p capture records_an_apps_window -- --ignored"]
    fn records_an_apps_window() {
        let listed = list_windowed_apps();
        eprintln!("apps: {:?}", listed.iter().map(|d| format!("{} = {}", d.id, d.name)).collect::<Vec<_>>());
        // Konsole: one program that isn't otherwise running, so closing it leaves none.
        let mut term = std::process::Command::new("konsole").args(["--separate", "-e", "sh", "-c", "while true; do date +%T.%N; sleep 0.02; done"]).spawn().expect("konsole");
        thread::sleep(Duration::from_millis(1500));
        let video = VideoSource::Apps { ids: vec!["konsole".into()], away_when_unfocused: true, hook: crate::GameHook { auto: false, allowed: Vec::new() } };
        let feed = Feed::open(&video, None).unwrap();
        let t = Instant::now();
        while feed.app().is_none() && t.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        let app = feed.app();
        let (frame, seq) = feed.get().unwrap();
        thread::sleep(Duration::from_millis(500));
        let (_, seq2) = feed.get().unwrap();
        let lit = frame.data.chunks_exact(4).filter(|p| p[0] > 16 || p[1] > 16 || p[2] > 16).count();
        eprintln!("app {app:?} after {} ms: {}x{}, {lit} lit pixels, pictures {seq:#x} → {seq2:#x}", t.elapsed().as_millis(), frame.width, frame.height);
        let _ = term.kill();
        thread::sleep(Duration::from_millis(500));
        eprintln!("after closing: app {:?}, waiting {}", feed.app(), feed.waiting());
        assert_eq!(app.as_deref(), Some("konsole"));
        assert!(lit > 0, "all black");
        assert!(seq2 > seq, "no new pictures");
        assert!(feed.waiting());
    }

    /// A screenshot is all of the screen at its own size, opaque.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_of_the_screen -- --ignored"]
    fn screenshot_of_the_screen() {
        let screen = list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let mut rec = LinuxRecorder::new(Arc::new(LiveAudio::default()));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{} {}×{} in {} ms", screen.name, img.width, img.height, t.elapsed().as_millis());
        assert_eq!(img.bgra.len(), (img.width * img.height * 4) as usize);
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let (w, h) = (img.width, img.height);
            let rgb: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let header = format!("P6\n{w} {h}\n255\n");
            std::fs::write(out, [header.as_bytes(), &rgb].concat()).unwrap();
        }
    }
}
