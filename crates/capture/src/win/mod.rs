//! Native Windows capture backend: Windows Graphics Capture + Media Foundation +
//! WASAPI, all in-process — no external tools.
//!
//! - **Video**: WGC delivers the monitor, or one app's window, as BGRA textures
//!   on the GPU (`d3d`); a monitor can be captured with Desktop Duplication
//!   instead (`dupl`), which draws no yellow border on Windows 10. A pacer thread converts the newest one to NV12 at the
//!   output size with the GPU's video processor and hands it to a Media
//!   Foundation H.264 encoder (`h264`) at exactly `fps`, so the output is
//!   constant frame rate even when the screen is static and WGC sends nothing.
//!   Frames never leave the GPU unless the software encoder is used.
//! - **Desktop and app audio**: WASAPI process loopback (`loopback`). **Mics**
//!   use cpal.
//! - **Mixing** happens in Rust (`crate::mixer`), which also drives the live
//!   meters; each track is AAC-encoded with Media Foundation (`aac`).
//! - **Writing** (`crate::writer`): our own MP4 muxer (`file`), either straight
//!   to a fragmented MP4 (record) or from an in-memory replay ring when a clip
//!   is saved.
//!
//! Everything is timestamped on QueryPerformanceCounter, the clock WGC and
//! WASAPI use: the first video frame is t=0.

mod aac;
pub mod anticheat;
mod browser;
mod camera;
mod changes;
mod cursor;
mod d3d;
mod dupl;
pub(crate) mod export;
pub mod feed;
pub mod decode;
pub(crate) mod direct;
pub mod gpu;
mod h264;
pub mod hook;
mod layers;
mod loopback;
mod quad;
mod sound_file;
pub(crate) mod system;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub(crate) use loopback::SystemAudio;
pub(crate) use system::host_now;
pub use camera::{keep_open as keep_camera_open, list_cameras, open_camera_settings};
pub(crate) use camera::{controls as camera_controls, picture as camera_picture, set_control as set_camera_control};
pub use system::{app_window_size, cursor_state, display_bounds, foreground_app_path, foreground_exe, list_apps, list_screens, list_windowed_apps};
pub(crate) use system::display_pixels;

use crate::mixer::{self, Clock, LiveAudio, MixChange, MixInput};
use crate::mp4mux::Spec;
use crate::outputs::{Capture, Outputs};
use crate::sources::{AudioCapture, AudioSource, mix_inputs, track_layout};
use crate::stream::StreamSettings;
use crate::writer::{self, Media, Writer};
use crate::{BlendShape, EncodeSettings, Mode, Recorder, Running, VideoSource, eprintln_later};
use aac::AacEncoder;
use crate::mp4file::Layout;

/// AAC bitrate per audio track.
const AUDIO_BITRATE: u32 = 192_000;

pub struct WinRecorder {
    outputs: Outputs<Session>,
    live: Arc<LiveAudio>,
}

impl WinRecorder {
    /// `live` receives the meters and supplies each source's volume.
    pub fn new(live: Arc<LiveAudio>) -> Self {
        Self { outputs: Outputs::new(), live }
    }
}

impl Recorder for WinRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        let live = self.live.clone();
        self.outputs.start(mode, settings, |s, mov| Session::start(s, &live, mov))
    }

    fn save_clip(&mut self, dir: &Path) -> Result<crate::PendingClip> {
        self.outputs.save_clip(dir)
    }

    fn stop(&mut self, mode: Mode, dir: Option<&Path>) -> Result<Option<PathBuf>> {
        self.outputs.stop(mode, dir)
    }

    fn stop_all(&mut self) {
        self.outputs.stop_all();
    }

    fn running(&self) -> Running {
        self.outputs.running()
    }

    fn set_replay_seconds(&mut self, seconds: u32) {
        self.outputs.set_replay_seconds(seconds);
    }

    fn update_video(&mut self, video: &VideoSource) -> bool {
        self.outputs.session.as_mut().is_some_and(|s| s.update_video(video))
    }

    fn set_mic(&mut self, id: &str, device: &str) -> bool {
        self.outputs.session.as_mut().and_then(|s| s.audio.as_mut()).is_some_and(|a| a.set_mic(id, device))
    }

    fn set_blend(&mut self, looks: u32, shape: BlendShape) -> bool {
        self.outputs.session.as_mut().is_some_and(|s| s.set_blend(looks, shape))
    }

    fn set_sources(&mut self, sources: &[AudioSource]) -> bool {
        self.outputs.session.as_mut().is_some_and(|s| s.set_sources(sources, &self.live))
    }

    fn screenshot(&mut self, video: &VideoSource) -> Result<crate::PendingShot> {
        if let Some(sw) = self.outputs.session.as_ref().and_then(|s| s.switch.as_ref()) {
            return shot_of(&sw.latest);
        }
        // Nothing running: a capture of its own, for one picture.
        system::com_init();
        let gpu = d3d::Gpu::for_capture()?;
        let (source, (w, h)) = PictureSource::plan(video)?;
        let picture = source.open(&gpu, w, h, 60, None)?;
        // A screen has its picture by now; games and apps take a moment to
        // find the one that's showing.
        let deadline = Instant::now() + Duration::from_secs(2);
        while (!picture.latest.has_frame() || picture.latest.waiting()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let shot = shot_of(&picture.latest);
        picture.video.stop();
        shot
    }
}

/// A screenshot of what's up in `latest`, unless that's the away screen or
/// black: none of the games and apps recorded is showing.
fn shot_of(latest: &d3d::Latest) -> Result<crate::PendingShot> {
    if !latest.has_frame() {
        bail!("nothing has been captured yet. Try again in a moment");
    }
    if latest.waiting() {
        bail!("none of the games and apps you record is showing");
    }
    latest.snapshot()
}

impl Drop for WinRecorder {
    /// Dropping (app quit) must still finalise the file.
    fn drop(&mut self) {
        self.outputs.stop_all();
    }
}

// ---------------------------------------------------------------------------
// Session: one running capture
// ---------------------------------------------------------------------------

/// How far behind real time the mixer runs, so every source's audio has arrived
/// (WASAPI delivers in ~10 ms packets, sometimes late). Timestamps are exact, so
/// this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    video: Option<Video>,
    /// The games and apps being followed, when recording apps.
    apps: Option<AppList>,
    /// What starting another kind of picture takes (whole screen, or games
    /// and apps), to switch while it runs.
    switch: Option<Switch>,
    pacer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    audio: Option<AudioCapture>,
    mixer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// The sources added and taken out while it runs, for the mixer.
    mix_changes: Option<Sender<MixChange>>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
    /// Frames a second, and the frame's size.
    fps: u32,
    size: (u32, u32),
    /// Frame blending changed while it runs, for the pacer to take up.
    blend_ctl: Arc<std::sync::Mutex<Option<(u32, BlendShape)>>>,
    /// The stream's own pictures, for the pacer to take up: one per
    /// rendition (`stream::renditions`) when it starts, none when it stops.
    stream_ctl: Arc<std::sync::Mutex<Option<Vec<StreamOut>>>>,
    /// What the stream's pictures are made from.
    stream_from: Option<StreamFrom>,
}

/// What the stream's own pictures are made from: the frame the pacer
/// makes (its average), with the other layers over it or under it and the
/// screen placed, as the recording's.
struct StreamFrom {
    gpu: d3d::Gpu,
    average: Arc<d3d::Latest>,
    feeds: Arc<layers::Feeds>,
    screen: crate::SharedScreen,
    hardware: bool,
}

/// One of the stream's pictures (a rendition: `stream::renditions`), in the
/// pacer: a frame for every `every` of the pacer's, in its own size,
/// through an encoder of its own.
struct StreamOut {
    /// Made from the pacer's frame as it is: one in every `every`.
    direct: d3d::Converter,
    /// At a fraction of the frame rate, with frame blending on: the frames
    /// in its own time averaged (`StreamBlend`), and converted from that.
    blended: Option<(StreamBlend, d3d::Converter)>,
    encoder: h264::Encoder,
    every: i64,
    /// Frames its encoder couldn't take.
    dropped: u64,
}

impl StreamOut {
    /// Whether frame `k` is wanted: every one while it blends its own,
    /// else the ones at its own frames' times.
    fn wants(&self, k: i64, blending: bool) -> bool {
        (blending && self.blended.is_some()) || k.rem_euclid(self.every) == 0
    }

    /// Frame `k` is made (in the pacer's average, `content` of it valid):
    /// the stream's frame from it, once one's due.
    fn frame(&mut self, gpu: &d3d::Gpu, k: i64, blending: bool, content: (u32, u32)) {
        let phase = k.rem_euclid(self.every);
        if !blending && let Some((b, _)) = &mut self.blended {
            b.count = 0;
        }
        let converter = match self.blended.as_mut().filter(|_| blending) {
            Some((b, c)) => {
                b.add(gpu);
                if phase != self.every - 1 {
                    return;
                }
                b.finish(gpu, content);
                &*c
            }
            None => {
                if phase != 0 {
                    return;
                }
                &self.direct
            }
        };
        let Some(slot) = self.encoder.pool.take() else {
            self.dropped += 1;
            return;
        };
        match converter.convert(slot) {
            Ok(()) => {
                if !self.encoder.encode(slot, k / self.every) {
                    self.dropped += 1;
                }
            }
            Err(e) => {
                self.encoder.pool.give(slot);
                eprintln_later!("stream: {e:#}");
            }
        }
    }
}

/// A stream frame made of every frame in its own time (frame blending on,
/// and the stream at a fraction of the frame rate: 48 fps from 144): their
/// average, as light, as frames are made of their looks, so its motion is
/// blurred over all of its time as the recording's is over each frame's.
/// Taking one frame in three instead blurred a third of it.
struct StreamBlend {
    quad: quad::Quad,
    light: (windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView),
    /// The pacer's frame, read as each is added.
    from: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    /// The stream frame, as its converter reads it.
    out: Arc<d3d::Latest>,
    out_target: windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
    size: (u32, u32),
    /// Frames in the one being made so far.
    count: u32,
}

impl StreamBlend {
    fn new(gpu: &d3d::Gpu, average: &Arc<d3d::Latest>) -> Result<Self> {
        use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT};
        let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        let size = (average.width, average.height);
        let light = gpu.texture(size.0, size.1, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT, bind, D3D11_USAGE_DEFAULT, 0)?;
        let out = d3d::Latest::new(gpu, size.0, size.1)?;
        Ok(Self {
            quad: quad::Quad::new(gpu)?,
            light: (quad::target_view(gpu, &light)?, quad::shader_view(gpu, &light)?),
            from: quad::shader_view(gpu, &average.texture)?,
            out_target: quad::target_view(gpu, &out.texture)?,
            out,
            size,
            count: 0,
        })
    }

    /// The pacer's frame, added (each the same in the average).
    fn add(&mut self, gpu: &d3d::Gpu) {
        self.count += 1;
        self.quad.average_light(gpu, &self.light.0, self.size, &self.from, 1.0 / self.count as f32, self.size);
    }

    /// The stream frame's done: its average, for the converter.
    fn finish(&mut self, gpu: &d3d::Gpu, content: (u32, u32)) {
        self.quad.from_light(gpu, &self.out_target, self.size, &self.light.1, self.size);
        self.out.mark_frame(content);
        self.count = 0;
    }
}

impl Capture for Session {
    fn writer(&self) -> &Sender<writer::Command> {
        &self.writer_tx
    }

    fn stream_video(&mut self, stream: Option<&StreamSettings>) -> Result<()> {
        let Some(st) = stream else {
            *self.stream_ctl.lock().unwrap() = Some(Vec::new());
            return Ok(());
        };
        let from = self.stream_from.as_ref().context("the capture can't stream")?;
        let camera = || Some(from.feeds.clone());
        let (renditions, _) = crate::stream::renditions(st, self.fps);
        let mut outs = Vec::with_capacity(renditions.len());
        for (i, r) in renditions.iter().enumerate() {
            let every = (self.fps / r.fps.max(1)).max(1) as i64;
            let (width, height) = crate::output_size(self.size.0, self.size.1, r.short_side);
            let setup = h264::Setup { fps: r.fps, kbps: r.kbps, hardware: from.hardware, export: false, stream: Some(i) };
            let encoder = h264::Encoder::start_with(&from.gpu, width, height, setup, self.writer_tx.clone())?;
            let direct = d3d::Converter::new(&from.gpu, &from.average, camera(), true, &encoder.pool.textures, true, width, height, r.fps, from.screen.clone())?;
            let blended = if every > 1 {
                let b = StreamBlend::new(&from.gpu, &from.average)?;
                let c = d3d::Converter::new(&from.gpu, &b.out, camera(), true, &encoder.pool.textures, true, width, height, r.fps, from.screen.clone())?;
                Some((b, c))
            } else {
                None
            };
            eprintln!("stream: {} at {width}×{height}, {} fps, {} kb/s", encoder.name, r.fps, r.kbps);
            outs.push(StreamOut { direct, blended, encoder, every, dropped: 0 });
        }
        *self.stream_ctl.lock().unwrap() = Some(outs);
        Ok(())
    }

    fn finish(self) -> Result<()> {
        self.end()
    }
}

impl Session {
    fn start(s: &EncodeSettings, live: &Arc<LiveAudio>, mov: bool) -> Result<Self> {
        crate::capture_started();
        system::com_init();
        system::mf_startup()?;
        let gpu = d3d::Gpu::for_capture()?;
        let (picture_source, (canvas_w, canvas_h)) = PictureSource::plan(&s.video)?;
        let (width, height) = s.size.of(canvas_w, canvas_h);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let aac: Vec<AacEncoder> = titles.iter().map(|_| AacEncoder::new(AUDIO_BITRATE)).collect::<Result<_>>()?;
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov, reorder: 0 },
        };
        let writer = Writer::new(layout, s.fps);
        let writer_tx = writer.tx.clone();

        let mut session = Self {
            video: None,
            apps: None,
            switch: None,
            pacer: None,
            audio: None,
            mixer: None,
            mix_changes: None,
            encoders: Vec::new(),
            writer_tx,
            writer: Some(writer),
            fps: s.fps,
            size: (width, height),
            blend_ctl: Default::default(),
            stream_ctl: Default::default(),
            stream_from: None,
        };
        let started = (|| -> Result<()> {
            // One PCM channel + AAC thread per track, in layout order: the mix,
            // each own track, the rest.
            let mut pcm_senders = Vec::new();
            for (track, enc) in aac.into_iter().enumerate() {
                let (tx, rx) = mpsc::channel::<Vec<f32>>();
                pcm_senders.push(tx);
                session.encoders.push(spawn_aac(track, enc, rx, session.writer_tx.clone()));
            }
            let mut tracks = pcm_senders.into_iter();
            let mix_track = tracks.next();
            let audio = AudioCapture::start(&s.sources, live, clock.clone())?;
            let inputs = mix_inputs(&s.sources, &audio.feeds, live, |_| tracks.next());
            let rest_track = if has_rest { tracks.next() } else { None };
            session.audio = Some(audio);
            let stop = Arc::new(AtomicBool::new(false));
            let (mix_changes, changes) = mpsc::channel();
            session.mix_changes = Some(mix_changes);
            let mixer = mixer::spawn_changing_mixer(
                inputs,
                changes,
                mix_track,
                rest_track,
                live.clone(),
                clock.clone(),
                host_now,
                MIX_LATENCY,
                stop.clone(),
            );
            session.mixer = Some((stop, mixer));

            let encoder = h264::Encoder::start(&gpu, width, height, s, session.writer_tx.clone())?;
            session.apps = picture_source.apps();
            // The screen is captured fast enough for every look to find a new
            // picture: the capture's rate limits are set from the looks per
            // second, not the frame rate (at 60 fps they'd hold it to 240).
            let looks_per_second = s.fps * s.blend_samples.max(1);
            let picture = picture_source.open(&gpu, canvas_w, canvas_h, looks_per_second, s.away_screen.clone())?;
            let latest = picture.latest.clone();
            session.video = Some(picture.video);
            session.switch = Some(Switch { gpu: gpu.clone(), latest: latest.clone(), fps: looks_per_second, away: s.away_screen.clone(), source: s.video.clone() });
            // The other layers: a webcam or display that can't open doesn't
            // stop the recording (its status says why), the picture goes on
            // without it.
            let feeds = layers::Feeds::new(&gpu, s.layers.clone(), looks_per_second);
            let overlay = Some(feeds.clone());
            // Each frame comes from the pictures shown during it, timed by
            // when each came up (see `FrameBlend`): with blending their
            // average, without it the one up at the frame's time. Taking the
            // newest picture on the clock instead repeated and skipped
            // pictures of a game at the recording's rate (a 300 fps game
            // recorded at 300: up to 22% of frames), as the clock and the
            // game's pictures drifted past each other.
            let looks = s.blend_samples.max(1);
            let shutter = crate::shutter::Shutter::new(looks, s.blend_shape);
            let blend = FrameBlend::new(&gpu, &latest, looks)?;
            latest.set_wants_queue(true);
            // Each Display and Window source drawn from its own blend, as
            // the recording's own capture is (see `Source`).
            let converter = d3d::Converter::new(&gpu, &blend.average, overlay.clone(), true, &encoder.pool.textures, true, width, height, s.fps, s.screen.clone())?;
            let preview = d3d::Previewer::new(&gpu, &latest, overlay, width, height, s.fps, s.screen.clone(), crate::preview::new_producer())?;
            session.stream_from = Some(StreamFrom {
                gpu: gpu.clone(),
                average: blend.average.clone(),
                feeds: feeds.clone(),
                screen: s.screen.clone(),
                hardware: s.use_hardware,
            });
            let ctl = PacerCtl { blend: session.blend_ctl.clone(), stream: session.stream_ctl.clone() };
            session.pacer = Some(spawn_blend_pacer(s.fps, looks, shutter, latest, blend, converter, encoder, preview, Some(feeds), s.screen.clone(), clock, ctl)?);
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.end();
            return Err(e);
        }
        Ok(session)
    }

    /// Change frame blending while it runs: the pacer takes it up at its
    /// next frame, and the screen is captured as fast as the looks need
    /// (its capture started again at that rate). False if it can't.
    fn set_blend(&mut self, looks: u32, shape: BlendShape) -> bool {
        if self.pacer.is_none() {
            return false;
        }
        let looks = looks.max(1);
        *self.blend_ctl.lock().unwrap() = Some((looks, shape));
        let rate = self.fps * looks;
        // Displays and windows shown too, as fast as the looks.
        if let Some(from) = &self.stream_from {
            from.feeds.set_fps(rate);
        }
        if self.switch.as_ref().is_some_and(|sw| sw.fps != rate) {
            if let Some(sw) = &mut self.switch {
                sw.fps = rate;
            }
            self.restart_video();
        }
        true
    }

    /// Start the picture's capture again as it is (at the rate `switch` has
    /// now), into the same picture.
    fn restart_video(&mut self) {
        let Some(sw) = &self.switch else { return };
        let source = match PictureSource::plan(&sw.source) {
            Ok((source, _)) => source,
            Err(e) => {
                eprintln!("can't start the capture again: {e:#}");
                return;
            }
        };
        if let Some(old) = self.video.take() {
            old.stop();
        }
        let apps = source.apps();
        match source.start(&sw.gpu, &sw.latest, sw.fps, sw.away.clone()) {
            Ok(v) => {
                // The games and apps followed, as they were.
                if let (Some(new), Some(old)) = (&apps, &self.apps) {
                    let config = old.lock().unwrap().clone();
                    *new.lock().unwrap() = config;
                }
                self.video = Some(v);
                self.apps = apps;
            }
            Err(e) => eprintln!("can't start the capture again: {e:#}"),
        }
    }

    /// Record something else without restarting: another list of games and
    /// apps, or the whole screen instead of them (or the other way round, or
    /// another display), into the same picture. False if it couldn't.
    fn update_video(&mut self, video: &VideoSource) -> bool {
        if let (Some(config), Some(list)) = (AppsConfig::of(video), self.apps.as_ref()) {
            *list.lock().unwrap() = config;
            if let Some(sw) = &mut self.switch {
                sw.source = video.clone();
            }
            return true;
        }
        let Some(sw) = &mut self.switch else { return false };
        if sw.source == *video {
            return true;
        }
        let source = match PictureSource::plan(video) {
            Ok((source, _)) => source,
            Err(e) => {
                eprintln!("can't switch what's recorded: {e:#}");
                return false;
            }
        };
        // The old one stops first: they'd both fill the picture.
        if let Some(old) = self.video.take() {
            old.stop();
        }
        let apps = source.apps();
        match source.start(&sw.gpu, &sw.latest, sw.fps, sw.away.clone()) {
            Ok(v) => {
                self.video = Some(v);
                self.apps = apps;
                sw.source = video.clone();
                true
            }
            Err(e) => {
                eprintln!("can't switch what's recorded: {e:#}");
                // Back to what it was recording.
                if let Ok((old, _)) = PictureSource::plan(&sw.source) {
                    self.apps = old.apps();
                    self.video = old.start(&sw.gpu, &sw.latest, sw.fps, sw.away.clone()).ok();
                }
                false
            }
        }
    }

    /// Capture these audio sources from now on, without restarting (see
    /// [`Recorder::set_sources`]): the capture starts and stops them, and
    /// the mixer takes them in and out.
    fn set_sources(&mut self, sources: &[AudioSource], live: &LiveAudio) -> bool {
        let (Some(audio), Some(changes)) = (self.audio.as_mut(), &self.mix_changes) else { return false };
        let (added, gone) = audio.set_sources(sources, live);
        for source in gone {
            eprintln!("audio source \"{}\": taken out while capturing", source.name);
            let _ = changes.send(MixChange::Remove(source.id));
        }
        for (source, feed) in added {
            eprintln!("audio source \"{}\": added while capturing, into the mix", source.name);
            let input = MixInput { channel: live.channel(&source.id), id: source.id, feed, in_mix: source.in_mix, track: None };
            let _ = changes.send(MixChange::Add(input));
        }
        true
    }

    /// Stop capturing and finish what the writer was doing (a recording's
    /// file; the ring's let go).
    fn end(mut self) -> Result<()> {
        // Each step logged with its time: a step that never ends is a frozen
        // quit, and the log then says which.
        let t = Instant::now();
        let step = |what: &str| eprintln!("stopping capture: {what} ({} ms)", t.elapsed().as_millis());
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        // A stream the pacer stopped before it took it up: finished here
        // (its encoders would hold the writer open).
        let pending = self.stream_ctl.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        for s in pending.into_iter().flatten() {
            s.encoder.finish();
        }
        step("frame pacer stopped");
        if let Some(video) = self.video.take() {
            video.stop();
        }
        step("video capture stopped");
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        step("audio capture stopped");
        // The mixer writes what's left and drops its senders; the AAC threads then
        // flush and exit.
        if let Some((stop, handle)) = self.mixer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        step("mixer stopped");
        for h in self.encoders.drain(..) {
            let _ = h.join();
        }
        step("audio encoders stopped");
        drop(self.writer_tx);
        let r = self.writer.take().map_or(Ok(()), Writer::join);
        step("file writer done");
        r
    }
}

/// Where a session's picture comes from: a display, or an app's window.
enum PictureSource {
    /// A display (its name, and its capture item and handle now), when to
    /// switch to the game capture hook, and how to capture it.
    Screen(String, windows::Graphics::Capture::GraphicsCaptureItem, windows::Win32::Graphics::Gdi::HMONITOR, crate::GameHook, crate::ScreenCapture),
    /// The apps to follow, changeable while capturing.
    Apps(AppList),
}

/// The games and apps a capture follows, and what to show while you're in
/// something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppsConfig {
    ids: Vec<String>,
    away_when_unfocused: bool,
    hook: crate::GameHook,
}

impl AppsConfig {
    fn of(source: &VideoSource) -> Option<Self> {
        match source {
            VideoSource::Apps { ids, away_when_unfocused, hook } => {
                Some(Self { ids: ids.clone(), away_when_unfocused: *away_when_unfocused, hook: hook.clone() })
            }
            VideoSource::Screen { .. } => None,
        }
    }
}

/// Shared with the thread following the apps, so it can change while it runs.
type AppList = Arc<std::sync::Mutex<AppsConfig>>;

/// What starting another kind of picture into a running capture takes.
struct Switch {
    gpu: d3d::Gpu,
    latest: Arc<d3d::Latest>,
    fps: u32,
    away: Option<Arc<crate::StillImage>>,
    /// What it's recording now.
    source: VideoSource,
}

/// A running capture and the texture it fills.
struct Picture {
    video: Video,
    latest: Arc<d3d::Latest>,
}

impl PictureSource {
    /// The source, and the frame size: the display's — for apps, the main
    /// display's — so it can't change mid-file, whatever the windows do.
    fn plan(source: &VideoSource) -> Result<(Self, (u32, u32))> {
        Ok(match source {
            VideoSource::Screen { id, hook, method, .. } => {
                let monitor = system::find_monitor(id);
                let item = d3d::monitor_item(monitor)?;
                let size = item.Size()?;
                (PictureSource::Screen(id.clone(), item, monitor, hook.clone(), *method), (size.Width.max(2) as u32, size.Height.max(2) as u32))
            }
            VideoSource::Apps { .. } => {
                let (_, w, h) = system::primary_monitor();
                let config = AppsConfig::of(source).unwrap_or_default();
                (PictureSource::Apps(Arc::new(std::sync::Mutex::new(config))), (w, h))
            }
        })
    }

    fn open(self, gpu: &d3d::Gpu, width: u32, height: u32, fps: u32, away: Option<Arc<crate::StillImage>>) -> Result<Picture> {
        // What's captured is copied 1:1 before it's scaled into the frame, so the
        // copy must hold all of it. A display is its own size; an app's window
        // can be on any display — on a portrait one, taller than the frame — so
        // it gets room for a window on the biggest of them. A picture bigger
        // still (a display whose mode grew since) is shrunk to fit it.
        // Room for a window whatever it records now: it can switch to games
        // and apps while it runs.
        let (copy_w, copy_h) = {
            let (w, h) = system::largest_display_box();
            (w.max(width), h.max(height))
        };
        let latest = d3d::Latest::new(gpu, copy_w, copy_h)?;
        let video = self.start(gpu, &latest, fps, away)?;
        Ok(Picture { video, latest })
    }

    /// Start recording into `latest`.
    fn start(self, gpu: &d3d::Gpu, latest: &Arc<d3d::Latest>, fps: u32, away: Option<Arc<crate::StillImage>>) -> Result<Video> {
        Ok(match self {
            PictureSource::Apps(list) => {
                // The away screen (black with it off) until one of the
                // apps' windows shows up.
                let on = list.lock().unwrap().away_when_unfocused;
                match away.as_ref().filter(|_| on) {
                    Some(image) => latest.show_still(image)?,
                    None => latest.clear()?,
                }
                Video::follow_apps(gpu.clone(), latest.clone(), list, fps, away)
            }
            PictureSource::Screen(id, item, monitor, hook, method) => {
                let by = NEXT_FOLLOWER.fetch_add(1, Ordering::Relaxed);
                let capture = DisplayCapture::start(gpu, &item, monitor, latest, fps, method, by)?;
                // Wait for the first frame so a capture that can't see the screen
                // fails here, visibly, instead of producing an empty file.
                let deadline = Instant::now() + Duration::from_secs(3);
                while !latest.has_frame() {
                    if Instant::now() >= deadline {
                        bail!("the screen isn't delivering frames");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Video::follow_screen(gpu.clone(), latest.clone(), id, item, monitor, capture, fps, hook, method, by)
            }
        })
    }

    /// The app list, when following apps.
    fn apps(&self) -> Option<AppList> {
        match self {
            PictureSource::Apps(list) => Some(list.clone()),
            PictureSource::Screen(..) => None,
        }
    }
}

/// A capture made only for the preview (`crate::preview::VideoPreview`). Runs
/// entirely on its own thread: starting doesn't wait for the capture to open,
/// and dropping doesn't wait for it to close.
pub(crate) struct PreviewCapture {
    stop: Arc<AtomicBool>,
    error: Arc<std::sync::Mutex<Option<String>>>,
    /// The app list being followed, when previewing apps.
    apps: Option<AppList>,
}

impl PreviewCapture {
    pub(crate) fn start(
        source: VideoSource,
        size: crate::VideoSize,
        fps: u32,
        away: Option<Arc<crate::StillImage>>,
        layers: crate::SharedLayers,
        screen: crate::SharedScreen,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(std::sync::Mutex::new(None));
        let (stop2, error2) = (stop.clone(), error.clone());
        let generation = crate::preview::new_producer();
        let apps = AppsConfig::of(&source).map(|c| Arc::new(std::sync::Mutex::new(c)));
        let apps2 = apps.clone();
        thread::spawn(move || {
            system::com_init();
            let opened = (|| -> Result<(Picture, d3d::Previewer, Arc<layers::Feeds>)> {
                let gpu = d3d::Gpu::for_capture()?;
                let (mut source, (w, h)) = PictureSource::plan(&source)?;
                if let (PictureSource::Apps(list), Some(shared)) = (&mut source, apps2) {
                    *list = shared; // so `update` reaches the running follower
                }
                let picture = source.open(&gpu, w, h, fps, away)?;
                let (width, height) = size.of(w, h);
                let feeds = layers::Feeds::new(&gpu, layers, fps);
                let preview = d3d::Previewer::new(&gpu, &picture.latest, Some(feeds.clone()), width, height, fps, screen, generation)?;
                Ok((picture, preview, feeds))
            })();
            let (picture, preview, feeds) = match opened {
                Ok(p) => p,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    crate::preview::clear(generation);
                    return;
                }
            };
            // Paced like the recording would be.
            let every = 1.0 / fps.max(1) as f64;
            let t0 = host_now();
            let mut n = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                feeds.update();
                if let Err(e) = preview.submit() {
                    eprintln!("preview: {e:#}");
                }
                n += 1;
                let wait = t0 + n as f64 * every - host_now();
                if wait > 0.0 {
                    thread::sleep(Duration::from_secs_f64(wait));
                } else if wait < -0.5 {
                    n = ((host_now() - t0) / every) as u64; // fell far behind: skip ahead
                }
            }
            drop(preview);
            drop(feeds);
            picture.video.stop();
        });
        Self { stop, error, apps }
    }

    pub(crate) fn update(&self, source: &VideoSource) -> bool {
        match (AppsConfig::of(source), &self.apps) {
            (Some(config), Some(list)) => {
                *list.lock().unwrap() = config;
                true
            }
            _ => false,
        }
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

/// One app window's capture: Windows' own (also watched for freezing, when
/// the hook may take over), or the game capture hook.
enum WindowCapture {
    Wgc(d3d::Capture, bool),
    Hook(hook::HookCapture),
}

impl WindowCapture {
    fn is_closed(&self) -> bool {
        match self {
            WindowCapture::Wgc(c, _) => c.is_closed(),
            WindowCapture::Hook(h) => h.ended().is_some(),
        }
    }
}

/// Exclusive fullscreen: a fullscreen game in focus is drawing (on the
/// graphics card, `gpu`) while its pictures haven't reached the capture for
/// this long (see [`Sight`]). Windows' capture gets a frame for every frame a
/// window presents, the same picture again too (measured: a still window
/// drawn 180 times a second, 180 frames a second), and Desktop Duplication a
/// new picture, so a game on a still screen isn't mistaken for this; one that
/// stops drawing isn't busy on the graphics card.
const FROZEN: Duration = Duration::from_secs(2);

/// Percent of the graphics card's 3D engine that counts as drawing. A guess:
/// a window that does nothing but clear itself every frame measured 1%, one
/// that doesn't draw 0.
const DRAWING: f64 = 0.3;

/// How busy a fullscreen game keeps the graphics card drawing, in percent
/// (`gpu`, sampled every call while it's watched), if that can be told.
fn drawing(gpu: &mut Option<gpu::GpuUse>, window: windows::Win32::Foundation::HWND) -> Option<f64> {
    Some(gpu.as_mut()?.percent(system::window_pid(window)))
}

/// What tells how busy games keep the graphics card (see [`drawing`]); said
/// in the log (once) where Windows can't: without it, no fullscreen game is
/// switched to the game capture hook for going unseen.
fn gpu_meter() -> Option<gpu::GpuUse> {
    let found = gpu::GpuUse::new();
    if found.is_none() {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| eprintln!("can't tell how busy games keep the graphics card (Windows' GPU counters): a fullscreen game Windows can't see won't switch to the game capture hook"));
    }
    found
}

/// How long after a game comes up, or its window moves, to look at it: a
/// window just gone fullscreen can still be shown by the compositor for its
/// first frames (measured: 0.2 s), and the look would end there. A guess.
const FIRST_LOOK: Duration = Duration::from_secs(1);

/// How long after a look that saw the game to look again: at first, then
/// twice as long each time, up to the second.
const LOOK_AGAIN: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(60));

/// Whether a fullscreen game in focus that Windows' capture records reaches
/// it. Windows' capture can't tell by itself: it hands over a frame each time
/// the cursor moves too, even over a game it can't see (its last picture
/// again, the cursor moved on it), and a game being played moves one of its
/// own (Minecraft 1.12 and before an invisible one, put back in the middle
/// every frame), which brings it frames even with the cursor left out of it.
/// That kept a frozen Minecraft's recording from ever switching to the hook
/// (issue #11). Desktop Duplication tells ([`dupl::Probe`]), so one looks:
/// when the game comes up, when its window moves or changes size (going
/// exclusive fullscreen), and now and then after (see [`FIRST_LOOK`],
/// [`LOOK_AGAIN`]), till a new picture shows the game seen. Not one kept
/// open: that costs the compositor about as much as Windows' capture does
/// (measured with a game at 540 fps: 15% more of a core on top of Windows'
/// capture, 37% alone, however seldom it's asked); a look keeps one open a
/// fifth of a second, and makes a frame or two come a refresh late. Where
/// the display can't be duplicated, Windows' capture's own frames tell, as
/// they can.
#[derive(Default)]
struct Sight {
    /// The game's window and when it came up there (another, or moved, is
    /// looked at soon).
    window: Option<(WindowAt, Instant)>,
    /// The look going on.
    probe: Option<dupl::Probe>,
    /// The display can't be duplicated.
    cannot: bool,
    /// When to look again (none: at once), and how long after that.
    next: Option<Instant>,
    wait: Duration,
}

/// A window, and where it is on the desktop (see [`system::window_place`]).
type WindowAt = (usize, (i32, i32, i32, i32));

impl Sight {
    /// How long the game in `window` hasn't reached the capture, since it
    /// came up: told by looks with a Desktop Duplication of the display
    /// (`probe`: Windows' capture records it), else by how long the capture's
    /// had nothing new (`own`: Desktop Duplication's own pictures, or Windows'
    /// capture's frames where the display can't be duplicated). None: it was
    /// seen lately, and isn't looked at now.
    fn unseen(&mut self, gpu: &d3d::Gpu, window: WindowAt, probe: Option<windows::Win32::Graphics::Gdi::HMONITOR>, own: Duration) -> Option<Duration> {
        let now = Instant::now();
        if self.window.is_none_or(|(w, _)| w != window) {
            *self = Sight { window: Some((window, now)), next: Some(now + FIRST_LOOK), ..Default::default() };
        }
        let up = self.window.map_or(Duration::ZERO, |(_, at)| now - at);
        let Some(monitor) = probe.filter(|_| !self.cannot) else {
            return Some(own.min(up));
        };
        if self.probe.is_none() {
            if self.next.is_some_and(|next| now < next) {
                return None;
            }
            match dupl::Probe::start(gpu, monitor) {
                Ok(probe) => self.probe = Some(probe),
                Err(why) => {
                    // Once: it's the same each time a game comes up.
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| eprintln!("can't look at the display with Desktop Duplication ({why}): a fullscreen game Windows' capture can't see is told by its frames alone, which moving the cursor brings too"));
                    self.cannot = true;
                    return Some(own.min(up));
                }
            }
        }
        let (unseen, seen) = self.probe.as_mut()?.since_last_picture();
        if seen {
            self.look_later();
            return None;
        }
        Some(unseen)
    }

    /// The look's over (the game's seen, or doesn't draw: nothing to see):
    /// the next, later.
    fn look_later(&mut self) {
        self.probe = None;
        self.wait = (self.wait * 2).clamp(LOOK_AGAIN.0, LOOK_AGAIN.1);
        self.next = Some(Instant::now() + self.wait);
    }
}

/// How long after the game capture hook failed on a window to try it there
/// again: at first, then twice as long each time it fails again, up to the
/// second. A guess.
const RETRY_HOOK: (Duration, Duration) = (Duration::from_secs(10), Duration::from_secs(300));

/// Windows the game capture hook failed on (it didn't load in time, sent no
/// picture, or stopped), recorded with Windows' capture or the display's
/// meanwhile. Not for good: a game still loading when it comes up can keep
/// the hook from loading in the time it gets, and one Windows' capture can't
/// see then stayed recorded as the last picture before it, the desktop, till
/// clipping started again (issue #46). The hook's tried again a while later
/// (see [`RETRY_HOOK`]), and as soon as the window's back in focus after
/// being left (alt-tab and back, which got it going before).
#[derive(Default)]
struct HookFailures(std::collections::HashMap<usize, HookFailed>);

struct HookFailed {
    /// When to try again, and how long after that if it fails again.
    again: Instant,
    wait: Duration,
    /// The window's been out of focus since.
    left: bool,
    /// Taken up again (see [`HookFailures::due`]); till it fails again.
    trying: bool,
}

impl HookFailures {
    /// The hook failed on `window`.
    fn failed(&mut self, window: usize, now: Instant) {
        let wait = self.0.get(&window).map_or(RETRY_HOOK.0, |f| (f.wait * 2).min(RETRY_HOOK.1));
        self.0.insert(window, HookFailed { again: now + wait, wait, left: false, trying: false });
    }

    /// Whether `window`'s still to be left to Windows' capture.
    fn holds_off(&self, window: usize, now: Instant) -> bool {
        self.0.get(&window).is_some_and(|f| !f.trying && now < f.again)
    }

    /// Whether to try the hook on `window` again now, where nothing else
    /// would (its Windows' capture is kept while it's in focus): once, till
    /// it fails again.
    fn due(&mut self, window: usize, now: Instant) -> bool {
        match self.0.get_mut(&window) {
            Some(f) if !f.trying && now >= f.again => {
                f.trying = true;
                true
            }
            _ => false,
        }
    }

    /// Which window's in focus now (none: not one of these): one back in
    /// focus after being left is tried again at once. Closed windows are
    /// forgotten.
    fn focus(&mut self, focused: Option<usize>, now: Instant, alive: impl Fn(usize) -> bool) {
        self.0.retain(|w, _| alive(*w));
        for (w, f) in &mut self.0 {
            if Some(*w) != focused {
                f.left = true;
            } else if std::mem::take(&mut f.left) && !f.trying {
                f.again = f.again.min(now);
            }
        }
    }
}

/// A display's capture: Windows' own, or Desktop Duplication (see `dupl`).
enum DisplayCapture {
    Wgc(d3d::Capture),
    Dupl(dupl::DuplCapture),
    /// Windows' capture standing in for Desktop Duplication that couldn't
    /// start or stopped, until it can (see [`Video::follow_screen`]).
    StandIn(d3d::Capture),
}

/// How long Windows' capture stands in for Desktop Duplication before it's
/// tried again: at first, then twice as long each time it still can't, up to
/// the second.
const RETRY_DUPLICATION: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(300));

/// How often a recorded display is looked up, to tell when it went away and
/// came back (see [`Video::follow_screen`]).
const LOOK_AT_DISPLAY: Duration = Duration::from_secs(1);

/// A display's capture starts again when the displays turn back on after
/// Windows turned them off, and once more this long after: a monitor waking
/// can take a few seconds to be back as it was (a guess).
const AGAIN_AFTER_WAKE: Duration = Duration::from_secs(4);

impl DisplayCapture {
    /// Capture the display the way `choice` says (see [`dupl::choose`]).
    /// Desktop Duplication that can't start has Windows' capture stand in.
    fn start(
        gpu: &d3d::Gpu,
        item: &windows::Graphics::Capture::GraphicsCaptureItem,
        monitor: windows::Win32::Graphics::Gdi::HMONITOR,
        latest: &Arc<d3d::Latest>,
        fps: u32,
        choice: crate::ScreenCapture,
        by: u64,
    ) -> Result<Self> {
        let (method, why) = dupl::choose(choice, gpu, monitor);
        if method == dupl::Method::Wgc {
            eprintln!("recording the display with Windows' capture ({why})");
            return Self::wgc(gpu, item, monitor, latest, fps, by, false);
        }
        match Self::duplicate(gpu, monitor, latest, fps, by) {
            Ok(c) => {
                eprintln!("recording the display with Desktop Duplication ({why})");
                Ok(c)
            }
            Err(e) => {
                eprintln!("can't record the display with Desktop Duplication ({e:#}); using Windows' capture until it can");
                Self::wgc(gpu, item, monitor, latest, fps, by, true)
            }
        }
    }

    fn duplicate(gpu: &d3d::Gpu, monitor: windows::Win32::Graphics::Gdi::HMONITOR, latest: &Arc<d3d::Latest>, fps: u32, by: u64) -> Result<Self> {
        let c = dupl::DuplCapture::start(gpu, monitor, latest, fps)?;
        set_in_use(by, crate::ScreenCaptureInUse { duplication: true, fell_back: false });
        Ok(DisplayCapture::Dupl(c))
    }

    /// Windows' capture, standing in for Desktop Duplication or not.
    fn wgc(gpu: &d3d::Gpu, item: &windows::Graphics::Capture::GraphicsCaptureItem, monitor: windows::Win32::Graphics::Gdi::HMONITOR, latest: &Arc<d3d::Latest>, fps: u32, by: u64, stand_in: bool) -> Result<Self> {
        let c = d3d::Capture::start(gpu, item, latest, d3d::Of::Display(monitor), fps)?;
        set_in_use(by, crate::ScreenCaptureInUse { duplication: false, fell_back: stand_in });
        Ok(if stand_in { DisplayCapture::StandIn(c) } else { DisplayCapture::Wgc(c) })
    }
}

/// How the display is being captured (see [`crate::screen_capture_in_use`]),
/// and by which [`Video::follow_screen`] (its number): a preview's closing
/// as a recording's starts mustn't take the recording's away.
static IN_USE: std::sync::Mutex<Option<(u64, crate::ScreenCaptureInUse)>> = std::sync::Mutex::new(None);
static NEXT_FOLLOWER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn set_in_use(by: u64, now: crate::ScreenCaptureInUse) {
    *IN_USE.lock().unwrap() = Some((by, now));
}

/// `by` stopped: it no longer says how the display is captured, if it did.
fn clear_in_use(by: u64) {
    let mut in_use = IN_USE.lock().unwrap();
    if in_use.is_some_and(|(b, _)| b == by) {
        *in_use = None;
    }
}

pub(crate) fn screen_capture_in_use() -> Option<crate::ScreenCaptureInUse> {
    IN_USE.lock().unwrap().map(|(_, u)| u)
}

pub(crate) fn screen_capture_auto_duplicates() -> bool {
    dupl::border_always_shown()
}

/// Keeps the picture coming: one capture of a display, or the windows of a set
/// of apps, following focus between them and each as it closes and reopens.
enum Video {
    /// A display, or (while one's fullscreen on it and needs it) a game
    /// through the hook; or the apps followed. Both on their own thread.
    Screen { stop: Arc<AtomicBool>, thread: JoinHandle<()> },
    App { stop: Arc<AtomicBool>, thread: JoinHandle<()> },
}

impl Video {
    /// How often to look at which app is in focus.
    const RESCAN: Duration = Duration::from_millis(100);

    fn follow_apps(gpu: d3d::Gpu, latest: Arc<d3d::Latest>, list: AppList, fps: u32, away: Option<Arc<crate::StillImage>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = thread::spawn(move || {
            system::com_init();
            let mut config = AppsConfig::default();
            // The listed app last in focus: recorded until another one is.
            let mut active: Option<String> = None;
            let mut current: Option<(windows::Win32::Foundation::HWND, String, WindowCapture)> = None;
            // Windows the hook failed on (it couldn't load, or stopped): Windows'
            // capture for them till it's tried again.
            let mut hook_failures = HookFailures::default();
            // Apps Windows' capture froze on: hooked from then on.
            let mut frozen: std::collections::HashSet<String> = Default::default();
            let mut sight = Sight::default();
            let mut gpu_use = gpu_meter();
            // A window that couldn't be captured, so it's reported once.
            let mut failed = None;
            // The away screen is up (so it's uploaded once, not every tick).
            let mut away_up = false;
            // Nothing listed is open and the away screen is off: the last
            // picture (or black) stays.
            let mut held = false;
            let show_away = |away_up: &mut bool| {
                if !*away_up {
                    let _ = match &away {
                        Some(image) => latest.show_still(image),
                        None => latest.clear(),
                    };
                    latest.set_app(None);
                    *away_up = true;
                }
            };
            while !stop2.load(Ordering::Relaxed) {
                // The list may have changed: forget apps that left it.
                let now = list.lock().unwrap().clone();
                if now != config {
                    config = now;
                    let still = |a: &String| config.ids.iter().any(|x| x.eq_ignore_ascii_case(a));
                    if active.as_ref().is_some_and(|a| !still(a)) {
                        active = None;
                    }
                }
                let listed = |exe: &str| config.ids.iter().find(|a| a.eq_ignore_ascii_case(exe)).cloned();
                let foreground = system::foreground_app();
                let focused = foreground.as_ref().and_then(|f| Some((f.hwnd, listed(&f.exe)?, f.main)));
                if let Some((_, app, _)) = &focused {
                    active = Some(app.clone());
                }
                hook_failures.focus(focused.as_ref().map(|(h, _, _)| h.0 as usize), Instant::now(), |w| system::window_alive(windows::Win32::Foundation::HWND(w as _)));
                // The hook stopped on a window that's still open: Windows'
                // capture for it instead.
                if let Some((h, app, WindowCapture::Hook(hooked))) = &current
                    && let Some(why) = hooked.ended()
                {
                    if system::window_alive(*h) {
                        eprintln!("{app}: the game capture hook stopped ({why}); recording it with Windows' capture till it's tried again");
                        hook_failures.failed(h.0 as usize, Instant::now());
                    }
                    current = None;
                }
                // Time to try the hook again on a window it failed on: its
                // Windows' capture makes way.
                if let Some((h, app, WindowCapture::Wgc(..))) = &current
                    && hook_failures.due(h.0 as usize, Instant::now())
                {
                    eprintln!("{app}: trying the game capture hook again");
                    current = None;
                }
                // Windows' capture of a fullscreen game in focus gets nothing
                // from it: it can't see the game (exclusive fullscreen, see
                // `Sight`). The hook can.
                let mut unseen = None;
                match &current {
                    Some((h, app, WindowCapture::Wgc(capture, true)))
                        if focused.as_ref().is_some_and(|(f, _, _)| f == h) && !system::window_hidden(*h) && system::covers_display(*h) =>
                    {
                        if let Some(percent) = drawing(&mut gpu_use, *h)
                            && sight
                                .unseen(&gpu, (h.0 as usize, system::window_place(*h)), Some(system::window_monitor(*h)), capture.since_last_frame())
                                .is_some_and(|u| u > FROZEN)
                        {
                            if percent > DRAWING {
                                unseen = Some((app.clone(), percent));
                            } else {
                                // Nothing to see: it doesn't draw.
                                sight.look_later();
                            }
                        }
                    }
                    _ => sight = Sight::default(),
                }
                if let Some((app, percent)) = unseen {
                    eprintln!("{app}: drawing ({percent:.1}% of the graphics card) but Windows' capture has had nothing from it for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
                    frozen.insert(app.to_lowercase());
                    current = None;
                }
                // Keep the window being recorded while it's still the active
                // app's, open, and shown (or focused): switching costs a moment,
                // and flipping between two of an app's windows would flicker.
                let keep = current.as_ref().is_some_and(|(h, app, capture)| {
                    Some(app) == active.as_ref()
                        && !capture.is_closed()
                        && system::window_alive(*h)
                        && (focused.as_ref().is_none_or(|(f, _, main)| !main || f == h) && !system::window_minimized(*h)
                            || focused.as_ref().is_some_and(|(f, _, _)| f == h))
                });
                if !keep {
                    // What to record now: the focused window of a listed app,
                    // else the active app's main window, else the first listed
                    // app that's open.
                    let target = focused
                        .as_ref()
                        .filter(|(_, _, main)| *main)
                        .map(|(h, app, _)| (*h, app.clone()))
                        .or_else(|| active.as_ref().and_then(|a| Some((system::find_app_window(a)?, a.clone()))))
                        .or_else(|| config.ids.iter().find_map(|a| Some((system::find_app_window(a)?, a.clone()))));
                    let same = current.as_ref().zip(target.as_ref()).is_some_and(|((h, ..), (t, _))| h == t);
                    if !same {
                        current = None;
                        match target.filter(|(h, _)| failed != Some(*h)) {
                            // Nothing listed is open.
                            None if config.away_when_unfocused => show_away(&mut away_up),
                            None if held => {}
                            None => {
                                let _ = latest.hold();
                                latest.set_app(None);
                                away_up = false;
                                held = true;
                            }
                            Some((h, app)) => {
                                let (mut plan, mut why) = hook::plan(&app, h, &config.hook);
                                if plan == hook::Plan::Watch && frozen.contains(&app.to_lowercase()) {
                                    plan = hook::Plan::Hook;
                                }
                                if plan == hook::Plan::Hook && hook_failures.holds_off(h.0 as usize, Instant::now()) {
                                    plan = hook::Plan::Wgc;
                                    why = "the hook failed on this window".into();
                                }
                                eprintln!("recording {app} with {} ({why})", if plan == hook::Plan::Hook { "the game capture hook" } else { "Windows' capture" });
                                let started = match plan {
                                    hook::Plan::Hook => hook::HookCapture::start(&gpu, h, &latest, fps).map(WindowCapture::Hook),
                                    p => d3d::window_item(h)
                                        .and_then(|item| d3d::Capture::start(&gpu, &item, &latest, d3d::Of::Window(h), fps))
                                        .map(|c| WindowCapture::Wgc(c, p == hook::Plan::Watch)),
                                };
                                match started {
                                    Ok(capture) => {
                                        latest.set_app(Some(app.clone()));
                                        active = Some(app.clone());
                                        current = Some((h, app, capture));
                                        away_up = false;
                                        held = false;
                                    }
                                    Err(e) => {
                                        eprintln!("can't record {app}'s window: {e:#}");
                                        failed = Some(h);
                                    }
                                }
                            }
                        }
                    }
                }
                // The setting changed while nothing listed is open.
                if current.is_none() && config.away_when_unfocused && held {
                    held = false;
                    show_away(&mut away_up);
                } else if current.is_none() && !config.away_when_unfocused && away_up {
                    let _ = latest.hold();
                    away_up = false;
                    held = true;
                }
                // Tabbed out: the recorded window isn't showing anymore
                // (minimized, as games are when you alt-tab, or hidden). Just
                // clicking elsewhere doesn't count: a window still on screen is
                // still recorded. With the away screen off, the clip keeps its
                // last picture.
                if let Some((h, app, _)) = &current {
                    if system::window_hidden(*h) {
                        if config.away_when_unfocused {
                            show_away(&mut away_up);
                        } else if away_up && latest.restore() {
                            // Turned off while tabbed out: its last picture again.
                            latest.set_app(Some(app.clone()));
                            away_up = false;
                        }
                    } else if away_up {
                        // Showing again: its next frame replaces the away screen.
                        latest.set_app(Some(app.clone()));
                        away_up = false;
                    }
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        Video::App { stop, thread }
    }

    /// Record a display: Windows' capture of it (or Desktop Duplication, see
    /// [`DisplayCapture`]), except while a game that needs the game capture
    /// hook (see [`hook::plan`]) is fullscreen and in focus on it. The
    /// display's capture is stopped meanwhile: while Windows' runs, some games
    /// lose their cursor. Desktop Duplication that couldn't start or stopped
    /// is tried again now and then (see [`RETRY_DUPLICATION`]), never given
    /// up for good: what stopped it (a hitch as a game went fullscreen,
    /// another app duplicating the display) may pass.
    #[allow(clippy::too_many_arguments)]
    fn follow_screen(
        gpu: d3d::Gpu,
        latest: Arc<d3d::Latest>,
        id: String,
        item: windows::Graphics::Capture::GraphicsCaptureItem,
        monitor: windows::Win32::Graphics::Gdi::HMONITOR,
        first: DisplayCapture,
        fps: u32,
        settings: crate::GameHook,
        method: crate::ScreenCapture,
        by: u64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (item, monitor, first) = (d3d::Shared(item), d3d::Shared(monitor), d3d::Shared(first));
        let thread = thread::spawn(move || {
            // Whole wrappers (they make these Send), not their fields.
            let (item, monitor, first) = (item, monitor, first);
            let (mut item, mut monitor) = (item.0, monitor.0);
            system::com_init();
            let mut display = Some(first.0);
            // The display is gone (a monitor that sleeps can go, as if
            // unplugged), which has been said.
            let mut gone = false;
            // Why its capture starts again, as soon as it can.
            let mut redo: Option<&str> = None;
            // Its capture couldn't start again, which has been said.
            let mut said = false;
            // When to look at the display next, how many times the displays
            // had turned back on by the last look, and when to start its
            // capture once more after that (see `AGAIN_AFTER_WAKE`).
            let mut look_at = Instant::now();
            let mut wakes = system::display_wakes();
            let mut again_at: Option<Instant> = None;
            // The PC is locked (by the last look).
            let mut locked = system::session_locked().unwrap_or(false);
            // While Windows' capture stands in for Desktop Duplication: when
            // to try it again, and how long to wait after that if it can't.
            let mut retry = (Instant::now() + RETRY_DUPLICATION.0, RETRY_DUPLICATION.0);
            let mut hooked: Option<(windows::Win32::Foundation::HWND, String, hook::HookCapture)> = None;
            // How to capture each game window, worked out once.
            let mut plans: std::collections::HashMap<usize, hook::Plan> = Default::default();
            let mut hook_failures = HookFailures::default();
            let mut frozen: std::collections::HashSet<String> = Default::default();
            let mut sight = Sight::default();
            let mut gpu_use = gpu_meter();
            while !stop2.load(Ordering::Relaxed) {
                // The display came back from going away, or as another (a
                // monitor that slept, or was unplugged and plugged back in),
                // Windows' capture of it ended, the displays turned back on
                // after Windows turned them off (or the PC woke), or the PC
                // was unlocked: a capture from before could show its last
                // picture for good, until the recording started again (issue
                // #34). Its capture starts again, of the display as it is now.
                // While it's gone, the last picture stays. Each is said with
                // its time, as the displays turning off and the PC sleeping
                // are (see `system::display_wakes`).
                if Instant::now() >= look_at {
                    look_at = Instant::now() + LOOK_AT_DISPLAY;
                    if matches!(&display, Some(DisplayCapture::Wgc(c) | DisplayCapture::StandIn(c)) if c.is_closed()) {
                        redo.get_or_insert("Windows' capture of the display ended");
                    }
                    let woke = system::display_wakes();
                    if woke != wakes {
                        wakes = woke;
                        again_at = Some(Instant::now() + AGAIN_AFTER_WAKE);
                        redo.get_or_insert("the display turned back on");
                    } else if again_at.is_some_and(|t| Instant::now() >= t) {
                        again_at = None;
                        redo.get_or_insert("the display turned back on a moment ago");
                    }
                    if let Some(now) = system::session_locked()
                        && now != locked
                    {
                        locked = now;
                        if locked {
                            eprintln!("{} the PC was locked", system::time_of_day());
                        } else {
                            redo.get_or_insert("the PC was unlocked");
                        }
                    }
                    match system::connected_monitor(&id) {
                        None => {
                            if !std::mem::replace(&mut gone, true) {
                                eprintln!("{} the display went away: its last picture stays until it's back", system::time_of_day());
                            }
                        }
                        Some(now) => {
                            if std::mem::take(&mut gone) || now != monitor {
                                redo = Some("the display came back");
                            }
                            if let Some(why) = redo {
                                match d3d::monitor_item(now) {
                                    Ok(again) => {
                                        eprintln!("{} {why}: capturing it again", system::time_of_day());
                                        (item, monitor, redo, said) = (again, now, None, false);
                                        display = None;
                                    }
                                    Err(e) => {
                                        if !std::mem::replace(&mut said, true) {
                                            eprintln!("{} {why}, but it can't be captured: {e:#}", system::time_of_day());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // A fullscreen window in focus on this display.
                let game = system::foreground_app()
                    .filter(|f| f.main && !system::window_hidden(f.hwnd) && system::fullscreen_on(f.hwnd, monitor))
                    .map(|f| (f.hwnd, f.exe));
                hook_failures.focus(game.as_ref().map(|(h, _)| h.0 as usize), Instant::now(), |w| system::window_alive(windows::Win32::Foundation::HWND(w as _)));
                let plan = game.as_ref().map(|(h, exe)| {
                    *plans.entry(h.0 as usize).or_insert_with(|| {
                        let (plan, why) = hook::plan(exe, *h, &settings);
                        eprintln!("{exe} fullscreen: {} ({why})", if plan == hook::Plan::Hook { "the game capture hook" } else { "the display's capture" });
                        plan
                    })
                });
                // The hook stopped on a game that's still open: the display
                // for it from now on.
                if let Some((h, exe, capture)) = &hooked
                    && let Some(why) = capture.ended()
                {
                    if system::window_alive(*h) {
                        eprintln!("{exe}: the game capture hook stopped ({why}); recording the display till it's tried again");
                        hook_failures.failed(h.0 as usize, Instant::now());
                    }
                    hooked = None;
                }
                let want = game.clone().filter(|(h, exe)| {
                    let hook = plan == Some(hook::Plan::Hook) || plan == Some(hook::Plan::Watch) && frozen.contains(&exe.to_lowercase());
                    hook && !hook_failures.holds_off(h.0 as usize, Instant::now())
                });
                match &want {
                    Some((h, exe)) if hooked.as_ref().is_none_or(|(hh, ..)| hh != h) => {
                        hooked = None;
                        display = None;
                        eprintln!("recording {exe} with the game capture hook while it's fullscreen");
                        match hook::HookCapture::start(&gpu, *h, &latest, fps) {
                            Ok(capture) => hooked = Some((*h, exe.clone(), capture)),
                            Err(e) => {
                                eprintln!("{exe}: can't start the game capture hook: {e:#}");
                                hook_failures.failed(h.0 as usize, Instant::now());
                            }
                        }
                    }
                    None if hooked.is_some() => hooked = None,
                    _ => {}
                }
                // Desktop Duplication stopped (it couldn't start again after
                // the display changed): Windows' capture stands in.
                if let Some(DisplayCapture::Dupl(c)) = &display
                    && let Some(why) = c.ended()
                {
                    eprintln!("desktop duplication stopped ({why}); recording the display with Windows' capture until it can start again");
                    display = DisplayCapture::wgc(&gpu, &item, monitor, &latest, fps, by, true).map_err(|e| eprintln!("can't record the display: {e:#}")).ok();
                    retry.0 = Instant::now() + retry.1;
                }
                if hooked.is_none() && display.is_none() {
                    match DisplayCapture::start(&gpu, &item, monitor, &latest, fps, method, by) {
                        Ok(c) => {
                            if matches!(c, DisplayCapture::StandIn(_)) {
                                retry.0 = Instant::now() + retry.1;
                            }
                            display = Some(c);
                        }
                        Err(e) => eprintln!("can't record the display: {e:#}"),
                    }
                }
                // Desktop Duplication tried again, Windows' capture standing in
                // until it starts (they draw the same picture meanwhile).
                if let Some(DisplayCapture::StandIn(_)) = &display
                    && Instant::now() >= retry.0
                {
                    match DisplayCapture::duplicate(&gpu, monitor, &latest, fps, by) {
                        Ok(c) => {
                            eprintln!("recording the display with Desktop Duplication again");
                            display = Some(c);
                            retry.1 = RETRY_DUPLICATION.0;
                        }
                        Err(e) => {
                            eprintln!("desktop duplication still can't start ({e:#})");
                            retry.1 = (retry.1 * 2).min(RETRY_DUPLICATION.1);
                        }
                    }
                    retry.0 = Instant::now() + retry.1;
                }
                // The display's capture gets nothing new while a fullscreen
                // game is in focus: it can't see it (exclusive fullscreen, or
                // a game that goes to the display past the compositor, issue
                // #11). Desktop Duplication tells; for Windows' capture, one
                // that looks now and then (see `Sight`).
                match (&display, &game, plan) {
                    (Some(capture), Some((h, exe)), Some(hook::Plan::Watch)) if !frozen.contains(&exe.to_lowercase()) => {
                        if let Some(percent) = drawing(&mut gpu_use, *h) {
                            let (own, probe) = match capture {
                                DisplayCapture::Dupl(c) => (c.since_last_picture(), None),
                                DisplayCapture::Wgc(c) | DisplayCapture::StandIn(c) => (c.since_last_frame(), Some(monitor)),
                            };
                            if sight.unseen(&gpu, (h.0 as usize, system::window_place(*h)), probe, own).is_some_and(|u| u > FROZEN) {
                                if percent > DRAWING {
                                    eprintln!("{exe}: drawing ({percent:.1}% of the graphics card) but the display's capture has had nothing from it for {FROZEN:?}: exclusive fullscreen; switching to the game capture hook");
                                    frozen.insert(exe.to_lowercase());
                                } else {
                                    // Nothing to see: it doesn't draw.
                                    sight.look_later();
                                }
                            }
                        }
                    }
                    _ => sight = Sight::default(),
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            clear_in_use(by);
        });
        Video::Screen { stop, thread }
    }

    fn stop(self) {
        let (Video::App { stop, thread } | Video::Screen { stop, thread }) = self;
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }
}

/// AAC-encode one track's PCM and pass the packets to the writer.
fn spawn_aac(track: usize, mut enc: AacEncoder, rx: mpsc::Receiver<Vec<f32>>, out: Sender<writer::Command>) -> JoinHandle<()> {
    thread::spawn(move || {
        system::com_init();
        while let Ok(pcm) = rx.recv() {
            for packet in enc.push(&pcm) {
                if out.send(writer::Command::Media(Media::Audio { track, packet })).is_err() {
                    return;
                }
            }
        }
        // The mixer is done: what the encoder still holds is the last ~50 ms
        // of the recording.
        for packet in enc.finish() {
            let _ = out.send(writer::Command::Media(Media::Audio { track, packet }));
        }
    })
}

// ---------------------------------------------------------------------------
// Pacer: constant frame rate
// ---------------------------------------------------------------------------

/// Live frame blending: each frame is the average of a fixed number of
/// looks at the game, evenly spaced over the frame's time (the blend rate
/// over the frame rate: 300 fps blending at 60 fps is 5), each weighed the
/// same, so every frame shows the same number of steps of motion. Or,
/// blended softly, of the looks from the middle of the frame before to the
/// middle of the one after, the nearer the frame's middle the more (see
/// [`crate::BlendShape`]): two frames are made at once.
///
/// A look takes the picture that was up at its time. When the game runs as
/// fast as the blend rate, each look should get a different picture, but a
/// picture's time is only known to a millisecond or so (when the capture
/// noticed it): with looks right next to the game's pictures, that would decide
/// by chance which picture a look gets, one twice and the next not at all. So
/// the looks are kept halfway between the game's pictures (locked to its pace).
/// When the game runs faster, the looks pick evenly spaced ones of its
/// pictures; slower, some picture is seen twice (as it was up that long).
///
/// The pictures are averaged as light, not as pixel values (which are
/// gamma-encoded): a screen showing them in turn gives off the average of
/// their light. Averaging the values would give a bright thing moving over a
/// dark background ghosts far too faint (a fifth of its value is about 3% of
/// its light, not 20%), and a dark thing over a bright one ghosts too strong.
///
/// Looks are taken a while after their time (at least `BLEND_LATE`, longer
/// while pictures come in later), so that pictures noticed late still count.
///
/// One for each source the frames are made of (see [`Source`]): the
/// recording's own capture, and each Display and Window source. The work
/// follows the pictures each shows: a frame all of whose looks saw one
/// picture is that picture, copied as it is (or not even copied, when the
/// frame before was it too), so a still source costs nothing; and only a
/// picture's own size is drawn (a window's, in a texture with room for the
/// biggest display). Its draws are the pacer's [`quad::Quad`]'s, given to
/// each call: one set of shaders for every source.
///
/// Each frame being made has an average of its own ([`Slot`]), taken as the
/// frame's first look comes: one at a time blended evenly, two softly, and
/// one more while another source's looks are behind this one's (each
/// source's looks are kept halfway between its own pictures, a fraction of a
/// look from another's, so one can start a frame before another has
/// finished the one before).
struct FrameBlend {
    gpu: d3d::Gpu,
    /// Copies of the capture's newest picture, for one it puts up without
    /// queueing (a still): two, taken in turn, so a frame being made keeps
    /// the one its looks saw while the next comes up.
    own: [(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView); 2],
    next_own: usize,
    /// The picture that's up: a copy (`own`) or one the capture handed over
    /// whole (`Queued`).
    held: Seen,
    /// The size every texture here is (the capture's).
    size: (u32, u32),
    /// Whether frames are blended (more than one look a frame), so each
    /// average is made as light.
    blending: bool,
    /// The averages of the frames being made, and those let go of (kept
    /// to make the next ones in).
    slots: Vec<Slot>,
    /// The one `average` is, as it is, if it's one picture (see `finish`).
    copied: Option<u64>,
    /// The finished frame, back in sRGB: the converter reads it.
    average: Arc<d3d::Latest>,
    target: windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
}

/// A picture a look can see: a number for each that's come up, its texture
/// (kept from going away: a queued picture's) and a view to draw it from,
/// and its valid size from the top left corner.
#[derive(Clone)]
struct Seen {
    id: u64,
    texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    view: windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView,
    size: (u32, u32),
}

/// One frame's average being made ([`FrameBlend`]).
struct Slot {
    /// The frame (none: free for the next).
    frame: Option<i64>,
    /// The average as light (16-bit float, so faint ghosts and dark colours
    /// keep their precision); none with one look a frame.
    light: Option<(windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)>,
    /// The looks' weight in it so far, and the weight of looks at the
    /// picture that's up not added yet: added in one draw when another
    /// comes up or the frame is done (a game slower than the blend rate is
    /// seen several times in a row; one draw each was a full-size pass per
    /// look).
    weight: f32,
    pending: f32,
    /// How much of it its pictures cover, from the top left corner (all
    /// that's drawn of it), and the size of the picture its last look saw
    /// (the frame's).
    area: (u32, u32),
    size: (u32, u32),
    /// The picture its first look saw: all of them did, while nothing's
    /// been mixed into it (`weight` is 0).
    first: Option<Seen>,
    /// With one look a frame, a copy of the picture its look saw, made once
    /// another comes up before the frame is made (see `flush_slot`).
    kept: Option<(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView)>,
}

/// The most frames made at once: two blended softly, and one more for
/// another source's looks behind (see [`FrameBlend`]).
const SLOTS_MOST: usize = 4;

impl FrameBlend {
    /// `looks` a frame (one is timing alone: each frame the picture that was
    /// up at its time).
    fn new(gpu: &d3d::Gpu, latest: &Arc<d3d::Latest>, looks: u32) -> Result<Self> {
        use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT};
        let size = (latest.width, latest.height);
        let average = d3d::Latest::new(gpu, size.0, size.1)?;
        let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        let own = || -> Result<_> {
            let t = gpu.texture(size.0, size.1, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, 0)?;
            let view = quad::shader_view(gpu, &t)?;
            Ok((t, view))
        };
        let own = [own()?, own()?];
        Ok(Self {
            gpu: gpu.clone(),
            held: Seen { id: 0, texture: own[0].0.clone(), view: own[0].1.clone(), size },
            own,
            next_own: 0,
            size,
            blending: looks > 1,
            slots: Vec::new(),
            copied: None,
            target: quad::target_view(gpu, &average.texture)?,
            average,
        })
    }

    /// Blend `looks` a frame from now on: every frame being made started
    /// again (its averages made anew if they're made another way).
    fn set_looks(&mut self, looks: u32) {
        if self.blending != (looks > 1) {
            self.blending = looks > 1;
            self.slots.clear();
        }
        self.drop_all();
    }

    /// Frame `k`'s average, if it's being made.
    fn slot(&mut self, k: i64) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.frame == Some(k))
    }

    /// Frame `k`'s average, made (or taken from those let go of) if it
    /// isn't being made. None if it can't be (no room on the graphics card,
    /// or too many frames at once: its looks are let go of).
    fn slot_for(&mut self, k: i64) -> Option<&mut Slot> {
        if let Some(i) = self.slots.iter().position(|s| s.frame == Some(k)) {
            return self.slots.get_mut(i);
        }
        let i = match self.slots.iter().position(|s| s.frame.is_none()) {
            Some(i) => i,
            None if self.slots.len() < SLOTS_MOST => {
                let light = if self.blending {
                    use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT};
                    let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
                    let made = self.gpu.texture(self.size.0, self.size.1, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT, bind, D3D11_USAGE_DEFAULT, 0).and_then(|t| Ok((quad::target_view(&self.gpu, &t)?, quad::shader_view(&self.gpu, &t)?)));
                    match made {
                        Ok(l) => Some(l),
                        Err(e) => {
                            eprintln_later!("frame blending: {e:#}");
                            return None;
                        }
                    }
                } else {
                    None
                };
                self.slots.push(Slot { frame: None, light, weight: 0.0, pending: 0.0, area: (0, 0), size: (0, 0), first: None, kept: None });
                self.slots.len() - 1
            }
            None => return None,
        };
        let s = &mut self.slots[i];
        (s.frame, s.weight, s.pending, s.first) = (Some(k), 0.0, 0.0, None);
        Some(s)
    }

    /// The capture's newest picture is the one that's up now (copied).
    fn hold(&mut self, quad: &quad::Quad, latest: &d3d::Latest) {
        self.flush(quad);
        let (texture, view) = self.own[self.next_own].clone();
        self.next_own = (self.next_own + 1) % self.own.len();
        unsafe { self.gpu.context.CopyResource(&texture, &latest.texture) };
        self.held = Seen { id: self.held.id + 1, texture, view, size: latest.content() };
    }

    /// The same, for a picture the capture handed over whole.
    fn hold_queued(&mut self, quad: &quad::Quad, q: d3d::Queued) {
        self.flush(quad);
        self.held = Seen { id: self.held.id + 1, texture: q.texture, view: q.view, size: q.size };
    }

    /// Add a look at the picture that's up to frame `k`'s average, by `weight`.
    fn add(&mut self, k: i64, weight: f32) {
        let held = self.held.clone();
        if let Some(s) = self.slot_for(k) {
            s.size = held.size;
            if s.weight + s.pending == 0.0 {
                s.first = Some(held);
            }
            s.pending += weight;
        }
    }

    /// No looks in frame `k` so far (no picture has come up yet).
    fn is_empty(&self, k: i64) -> bool {
        self.slots.iter().find(|s| s.frame == Some(k)).is_none_or(|s| s.weight + s.pending == 0.0)
    }

    /// Frame `k` is skipped: its looks are let go of unmixed (the picture
    /// that's up stays up).
    fn drop_frame(&mut self, k: i64) {
        if let Some(s) = self.slot(k) {
            s.frame = None;
        }
    }

    /// Every frame being made is skipped.
    fn drop_all(&mut self) {
        for s in &mut self.slots {
            s.frame = None;
        }
    }

    /// Mix the looks at the picture that's up into the averages, at once
    /// (another picture is coming up).
    fn flush(&mut self, quad: &quad::Quad) {
        for i in 0..self.slots.len() {
            self.flush_slot(quad, i);
        }
    }

    /// The same for one frame's average (`slots[i]`). A frame made mixes in
    /// only its own looks: the next frame's (a source ahead of another starts
    /// it before this one is made) wait for the next picture, so a next frame
    /// all of whose looks see this one is still this picture as it is.
    fn flush_slot(&mut self, quad: &quad::Quad, i: usize) {
        let (size, held) = (self.size, &self.held);
        let s = &mut self.slots[i];
        if s.frame.is_none() || s.pending <= 0.0 {
            return;
        }
        let weight = std::mem::take(&mut s.pending);
        let first = s.weight == 0.0;
        s.weight += weight;
        let (w, h) = held.size;
        s.area = if first { (w, h) } else { (s.area.0.max(w), s.area.1.max(h)) };
        if let Some((target, _)) = &s.light {
            // Mixing in by its share of the total so far keeps it the average.
            quad.average_light(&self.gpu, target, size, &held.view, weight / s.weight, s.area);
        } else if !self.blending && s.first.as_ref().is_some_and(|f| f.id == held.id) {
            // One look a frame: the picture it saw, copied. The capture's
            // texture soon holds a later picture (its queue's are used in
            // turn, `own` has two), and this frame may be made most of a
            // frame later, once another source's look for it is in.
            if s.kept.is_none() {
                use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT};
                let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
                match self.gpu.texture(size.0, size.1, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, 0).and_then(|t| Ok((quad::shader_view(&self.gpu, &t)?, t))) {
                    Ok((view, texture)) => s.kept = Some((texture, view)),
                    Err(e) => eprintln_later!("frame timing: {e:#}"),
                }
            }
            if let Some((texture, view)) = &s.kept {
                let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                unsafe { self.gpu.context.CopySubresourceRegion(texture, 0, 0, 0, 0, &held.texture, 0, Some(&region)) };
                s.first = Some(Seen { id: held.id, texture: texture.clone(), view: view.clone(), size: held.size });
            }
        }
    }

    /// Frame `k` is done: its average is ready for the converter, and it's
    /// let go of. Not for an empty one (see `is_empty`).
    fn finish(&mut self, quad: &quad::Quad, k: i64) {
        let Some(i) = self.slots.iter().position(|s| s.frame == Some(k)) else { return };
        let s = &self.slots[i];
        // One look a frame: the picture it saw, as it is. Or every look saw
        // the picture that's up, none's mixed in yet: that, as it is, no
        // blending to do. Copied, unless the frame before was it too.
        let one = if !self.blending { s.first.clone() } else { s.first.clone().filter(|_| s.weight == 0.0 && s.pending > 0.0) };
        let shown = match one {
            Some(seen) => {
                if self.copied != Some(seen.id) {
                    let (w, h) = seen.size;
                    let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                    unsafe { self.gpu.context.CopySubresourceRegion(&self.average.texture, 0, 0, 0, 0, &seen.texture, 0, Some(&region)) };
                    self.copied = Some(seen.id);
                }
                seen.size
            }
            None => {
                self.flush_slot(quad, i);
                let s = &self.slots[i];
                if let Some((_, view)) = &s.light {
                    quad.from_light(&self.gpu, &self.target, self.size, view, s.area);
                }
                self.copied = None;
                s.size
            }
        };
        self.average.mark_frame(shown);
        self.slots[i].frame = None;
    }

}

/// How long after a frame's time it's made: the game capture knows a picture
/// is new a few milliseconds after it came up (it compares it with the last),
/// and it must still get into its frame; Desktop Duplication asks for new
/// pictures every half of it while the screen is still. A guess. The least it
/// waits: pictures that keep coming later make it wait longer (see
/// `BlendPacer::wait`).
const BLEND_LATE: f64 = 0.012;
/// The longest it waits for late pictures. A game that keeps the graphics card
/// busy (Geometry Dash at 1000 fps) gets the hook's comparisons answered 10-20
/// ms late: waiting 12 ms, a third of its pictures came too late for the looks
/// that should have seen them, which saw the one before instead, and motion
/// came out uneven. Waiting longer only delays when frames are made, not their
/// times in the file.
const BLEND_LATE_MAX: f64 = 0.1;
/// How the wait comes back down once pictures come in sooner: it's long enough
/// for the latest picture of the last one to two of these (seconds). Timed, not
/// counted in pictures: one hitch holds it up a second or two, in a 60 fps game
/// as in one at 1000 fps, and a short quiet stretch doesn't drop it just before
/// the next late picture.
const LATE_WINDOW: f64 = 1.0;

/// How late pictures come in (seconds after their time): the latest in this
/// `LATE_WINDOW` and in the one before.
#[derive(Default)]
struct Lateness {
    since: f64,
    now: f64,
    before: f64,
}

impl Lateness {
    /// A picture came in `late`, at `now`.
    fn note(&mut self, now: f64, late: f64) {
        let gone = now - self.since;
        if gone >= LATE_WINDOW {
            self.before = if gone < 2.0 * LATE_WINDOW { self.now } else { 0.0 };
            self.now = 0.0;
            self.since = now;
        }
        self.now = self.now.max(late);
    }

    /// The latest picture of the last one or two windows, as of `now`.
    fn worst(&self, now: f64) -> f64 {
        let gone = now - self.since;
        if gone >= 2.0 * LATE_WINDOW {
            0.0
        } else if gone >= LATE_WINDOW {
            self.now
        } else {
            self.now.max(self.before)
        }
    }
}
/// How far behind its frames the pacer may fall (seconds) before it skips
/// the ones it missed instead of making each. Made in a burst, they'd mostly
/// be skipped anyway (more than the encoder takes at once) and mostly repeat
/// one picture (the capture keeps only its last few, `QUEUE_MAX`), and
/// blending them is more work for a graphics card that's already behind.
/// Without this, a pacer short of time fell behind for good, and quietly:
/// measured with each frame made to take 20 ms at 60 fps, it was 1.9 s behind
/// after 10 s and had skipped none (the file's video falls behind its sound).
/// A quarter second is 15 frames at 60 fps, about as many as the encoder has
/// (16).
const MAX_BEHIND: f64 = 0.25;

/// One picture the frames are made of, blended and timed on its own (its
/// [`FrameBlend`]): the recording's own capture, or a Display or Window
/// source's (`crate::LayerSource::frame_blended`). Its looks take the
/// picture it had up at their time, and are kept halfway between its own
/// pictures (its `phase`): two sources' pictures needn't come at the same
/// times (a 240 Hz display beside a 540 Hz one, a window that draws when it
/// likes), so each is locked to its own. Every source's frame `k` is made of
/// its looks `k * looks` on, each source's a fraction of a look apart.
struct Source {
    /// Its layer (`crate::Layer::id`; 0 is the recording's own capture).
    id: u64,
    /// What it is, for the log.
    name: String,
    latest: Arc<d3d::Latest>,
    blend: FrameBlend,
    /// Whether the frames show it, so its looks are added: a source taken
    /// out of the scene is let go of, but the recording's own capture goes
    /// on, its looks unadded, while the scene doesn't show it.
    shown: bool,
    /// Its look `m` is at `t0 + phase + (m + 0.5) * step` (see
    /// `crate::shutter::follow_picture`), and its next look to take.
    phase: f64,
    next: i64,
    /// A picture its capture timed has come up (it set `phase`).
    timed: bool,
    /// The picture that's up (a number for each, to count different ones),
    /// and the one the last look took.
    held: Option<u64>,
    pictures: u64,
    last_taken: Option<u64>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    /// Changes put up some other way than queued (a still), as last seen.
    others: u64,
    /// Its pictures a second, lately (which source's are told, see
    /// `BlendPacer::told`).
    rate: Rate,
    stats: SourceStats,
}

impl Source {
    /// Its looks from look `next` on, starting at `phase`.
    fn new(id: u64, name: String, latest: Arc<d3d::Latest>, blend: FrameBlend, phase: f64, next: i64) -> Self {
        Self {
            id,
            name,
            others: latest.others(),
            latest,
            blend,
            shown: true,
            phase,
            next,
            timed: false,
            held: None,
            pictures: 0,
            last_taken: None,
            different: 0,
            rate: Rate::default(),
            stats: SourceStats::new(),
        }
    }

    /// Frame `k` is done: its blend made. One no look saw a picture of (it's
    /// just been shown, or put in the scene) is its newest picture, as it
    /// is; one whose capture has nothing to show (a window that closed)
    /// leaves it out (a Display or Window source's: the recording's own
    /// always has a picture).
    fn finish(&mut self, quad: &quad::Quad, k: i64) {
        if self.id != 0 && !self.latest.has_frame() {
            self.blend.drop_frame(k);
            self.blend.average.forget();
            return;
        }
        if self.blend.is_empty(k) {
            if self.held.is_none() {
                self.hold_newest(quad);
            }
            if self.held.is_none() {
                return;
            }
            self.blend.add(k, 1.0);
        }
        self.blend.finish(quad, k);
    }

    /// Its capture's newest picture is the one that's up (it's just come
    /// into the frames, with no picture of its own yet), if it has one.
    fn hold_newest(&mut self, quad: &quad::Quad) {
        if self.latest.has_frame() {
            self.blend.hold(quad, &self.latest);
            self.pictures += 1;
            self.held = Some(self.pictures);
        }
    }
}

/// A source's pictures a second, as of the last whole second.
#[derive(Default)]
struct Rate {
    since: f64,
    count: u32,
    last: u32,
}

impl Rate {
    /// A picture came at `now`.
    fn note(&mut self, now: f64) {
        if now - self.since >= 1.0 {
            self.last = if now - self.since < 2.0 { self.count } else { 0 };
            (self.since, self.count) = (now, 0);
        }
        self.count += 1;
    }

    /// Pictures in the last whole second, as of `now` (or so far, before one's gone by).
    fn per_second(&self, now: f64) -> u32 {
        match now - self.since {
            gone if gone >= 2.0 => 0,
            gone if gone >= 1.0 => self.count,
            _ => self.last.max(self.count),
        }
    }
}

struct BlendPacer {
    t0: f64,
    /// Frames a second.
    fps: u32,
    /// Seconds between looks (one over the blend rate), and looks per frame.
    step: f64,
    looks: i64,
    /// Which frames each look counts toward.
    shutter: crate::shutter::Shutter,
    /// The last frame done (made, skipped or empty): later looks don't count
    /// toward it. A frame's done once every source has taken its looks.
    closed: i64,
    /// What the frames are made of: the recording's own capture first, then
    /// each Display and Window source that shows (see [`Source`]).
    sources: Vec<Source>,
    /// The source whose pictures how the capture keeps up tells
    /// (`crate::blend_sources`): the recording's own capture while the
    /// scene shows it, else the one showing the most pictures.
    told: u64,
    /// Draws every source's blends.
    quad: quad::Quad,
    /// Rung by every shown source's capture when a picture comes.
    bell: Arc<d3d::Bell>,
    /// Where the recording's own capture goes in the frame (hidden: the
    /// scene doesn't show it).
    screen: crate::SharedScreen,
    /// A frame was made since the sources last followed the layers.
    follow_due: bool,
    /// How late pictures come in, lately.
    lateness: Lateness,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    /// The other layers' pictures, followed each frame.
    feeds: Option<Arc<layers::Feeds>>,
    /// The stream's own pictures, while it streams (one per rendition).
    stream: Vec<StreamOut>,
    dropped: u64,
    stats: BlendStats,
    diag: EncodeDiag,
}

/// What the session changes in a running pacer: frame blending, and the
/// stream's pictures (see `Session::set_blend`, `Session::stream_video`).
struct PacerCtl {
    blend: Arc<std::sync::Mutex<Option<(u32, BlendShape)>>>,
    stream: Arc<std::sync::Mutex<Option<Vec<StreamOut>>>>,
}

/// Where frames wait, logged for every 5 s in which frames were skipped
/// (every 5 s with `HASTECLIP_ENCODE_LOG=1`), so a log from a PC that can't
/// keep up says what it's short of: how late the pacer closed frames and how
/// long closing took (CPU), and which of its steps the slowest one's time
/// went to, how long the GPU took to finish a frame's work after it was
/// submitted, how long the encoder held frames, the fewest free encoder
/// frames, and why frames were skipped. (A real session's log had the pacer
/// held up 349 ms closing one frame, 20 frames skipped, and no way to tell
/// where.)
struct EncodeDiag {
    verbose: bool,
    since: Instant,
    frames: u32,
    late_max: f64,
    close_max: Duration,
    /// Where the slowest closing's time went.
    slowest: CloseSteps,
    /// Queries ending each frame's GPU work, oldest first, with when.
    pending: std::collections::VecDeque<(windows::Win32::Graphics::Direct3D11::ID3D11Query, Instant)>,
    spare: Vec<windows::Win32::Graphics::Direct3D11::ID3D11Query>,
    gpu_ms: Vec<f64>,
    free_min: usize,
    pool_empty: u32,
    queue_full: u32,
    caught_up: u32,
}

/// How long each step of closing a frame took (see `BlendPacer::close`).
#[derive(Clone, Copy, Default)]
struct CloseSteps {
    /// The webcam's newest picture taken in.
    webcam: Duration,
    /// The frame's looks mixed and the average made.
    blend: Duration,
    /// Scaled and converted into the encoder's frame.
    convert: Duration,
    /// Handed to the encoder.
    encode: Duration,
    /// The preview's picture started.
    preview: Duration,
}

impl std::fmt::Display for CloseSteps {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        write!(
            f,
            "webcam {:.1}, blend {:.1}, convert {:.1}, encoder {:.1}, preview {:.1}",
            ms(self.webcam),
            ms(self.blend),
            ms(self.convert),
            ms(self.encode),
            ms(self.preview)
        )
    }
}

impl EncodeDiag {
    fn new() -> Self {
        Self {
            verbose: std::env::var_os("HASTECLIP_ENCODE_LOG").is_some(),
            since: Instant::now(),
            frames: 0,
            late_max: 0.0,
            close_max: Duration::ZERO,
            slowest: CloseSteps::default(),
            pending: Default::default(),
            spare: Vec::new(),
            gpu_ms: Vec::new(),
            free_min: usize::MAX,
            pool_empty: 0,
            queue_full: 0,
            caught_up: 0,
        }
    }

    fn frame(&mut self, gpu: &d3d::Gpu, late: f64, close: Duration, steps: CloseSteps) {
        use windows::Win32::Graphics::Direct3D11::{D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY_DESC, D3D11_QUERY_EVENT};
        self.frames += 1;
        self.late_max = self.late_max.max(late);
        if close > self.close_max {
            self.close_max = close;
            self.slowest = steps;
        }
        unsafe {
            while let Some((q, at)) = self.pending.front() {
                let mut done = 0i32;
                let r = gpu.context.GetData(q, Some(&mut done as *mut i32 as *mut _), 4, D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32);
                if r.is_err() || done == 0 {
                    break;
                }
                self.gpu_ms.push(at.elapsed().as_secs_f64() * 1000.0);
                let (q, _) = self.pending.pop_front().unwrap();
                self.spare.push(q);
            }
            let q = self.spare.pop().or_else(|| {
                let mut q = None;
                gpu.device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q)).ok()?;
                q
            });
            if let Some(q) = q {
                gpu.context.End(&q);
                self.pending.push_back((q, Instant::now()));
            }
        }
        if self.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let held_max = h264::HELD_MAX_US.swap(0, Relaxed) as f64 / 1000.0;
        let held_n = h264::HELD_N.swap(0, Relaxed).max(1);
        let held_avg = h264::HELD_SUM_US.swap(0, Relaxed) as f64 / 1000.0 / held_n as f64;
        if self.verbose || self.pool_empty + self.queue_full + self.caught_up > 0 {
            self.gpu_ms.sort_by(f64::total_cmp);
            let pct = |p: f64| self.gpu_ms.get(((self.gpu_ms.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(f64::NAN);
            // On the pacer: written by another thread.
            eprintln_later!(
                "encode: {} frames; closed up to {:.1} ms late, closing took up to {:.1} ms ({}); GPU done after {:.1}/{:.1}/{:.1} ms (median/99%/max), {} unfinished; encoder held frames {:.1} ms avg, {:.1} max; fewest free frames {}; skipped {} (no free frame) + {} (queue full) + {} (caught up)",
                self.frames,
                self.late_max * 1000.0,
                self.close_max.as_secs_f64() * 1000.0,
                self.slowest,
                pct(0.5),
                pct(0.99),
                pct(1.0),
                self.pending.len(),
                held_avg,
                held_max,
                self.free_min,
                self.pool_empty,
                self.queue_full,
                self.caught_up,
            );
        }
        let pending = std::mem::take(&mut self.pending);
        let spare = std::mem::take(&mut self.spare);
        *self = Self::new();
        self.pending = pending;
        self.spare = spare;
    }
}

impl BlendPacer {
    /// How long after their time looks are taken: long enough for the
    /// pictures that come in latest, with a little to spare.
    fn wait(&self) -> f64 {
        (self.lateness.worst(host_now()) * 1.2 + 0.002).clamp(BLEND_LATE, BLEND_LATE_MAX)
    }

    /// When source `i`'s look `m` is.
    fn look_time(&self, i: usize, m: i64) -> f64 {
        self.t0 + self.sources[i].phase + (m as f64 + 0.5) * self.step
    }

    /// When frame `k` is done: every source's last look for it.
    fn frame_done_at(&self, k: i64) -> f64 {
        let last = self.shutter.last_look(k);
        (0..self.sources.len()).map(|i| self.look_time(i, last)).fold(f64::MIN, f64::max)
    }

    /// When the next frame to make is done.
    fn frame_end(&self) -> f64 {
        self.frame_done_at(self.closed + 1)
    }

    /// Whether frames are made: once the recording's own capture has its
    /// first picture (taken in, or there while the scene doesn't show it).
    fn started(&self) -> bool {
        let main = &self.sources[0];
        main.held.is_some() || main.latest.has_frame()
    }

    /// Fallen more than `MAX_BEHIND` behind `t` (what looks are taken up
    /// to): the frames whose time is past are skipped at once, the one being
    /// made with them, and it goes on with the frame `t` is in.
    fn catch_up(&mut self, t: f64) {
        let behind = t - self.frame_end();
        if behind < MAX_BEHIND {
            return;
        }
        // The first look not due yet (by the recording's own capture's),
        // and its frame.
        let due = ((t - self.t0 - self.sources[0].phase) / self.step - 0.5).ceil() as i64;
        let (frame, _) = self.shutter.split(due);
        let missed = frame - (self.closed + 1);
        self.closed = frame - 1;
        let first = self.shutter.first_look(frame);
        for s in &mut self.sources {
            s.next = first;
            s.different = 0;
            s.blend.drop_all();
        }
        // Before the first picture, no frames would have been made.
        if self.started() {
            skipped(&mut self.dropped, missed as u64);
            self.stats.caught_up(missed as u32, behind);
            self.diag.caught_up += missed as u32;
        }
    }

    /// Blend `looks` a frame from the next frame on, weighed by `shape`:
    /// the frame under way is made from the looks it gets from now (at the
    /// new rate), and each source's looks lock onto its pictures again. Its
    /// averages are made as frames come (see [`FrameBlend`]).
    fn change_blend(&mut self, looks: u32, shape: BlendShape) {
        let looks = looks.max(1);
        let shutter = crate::shutter::Shutter::new(looks, shape);
        if looks as i64 == self.looks && shutter == self.shutter {
            return;
        }
        for s in &mut self.sources {
            s.blend.set_looks(looks);
        }
        let k = self.closed + 1;
        self.looks = looks as i64;
        self.step = 1.0 / (self.fps.max(1) * looks) as f64;
        self.shutter = shutter;
        // Frame `k`'s own time is where it was: looks `k * looks` on, at
        // the new step. The first picture each capture times puts its
        // looks halfway between its pictures again.
        let first = self.shutter.first_look(k);
        for s in &mut self.sources {
            (s.phase, s.timed, s.next, s.different) = (0.0, false, first, 0);
        }
        self.follow_due = true;
        crate::blending(self.fps, looks);
        self.say_sources();
        eprintln_later!("{}", if looks > 1 { format!("frame blending: {looks} looks a frame from now on") } else { "frame blending: off from now on".to_owned() });
    }

    /// The stream started or stopped, if the session says so. A stopped
    /// stream's encoders are finished on a thread of their own (it waits
    /// for them).
    fn take_stream(&mut self, ctl: &PacerCtl) {
        let Some(change) = ctl.stream.lock().unwrap().take() else { return };
        let old = std::mem::replace(&mut self.stream, change);
        if old.is_empty() {
            return;
        }
        let _ = thread::Builder::new().name("stream-end".into()).spawn(move || {
            for s in old {
                if s.dropped > 0 {
                    eprintln!("stream: its encoder couldn't take {} frames", s.dropped);
                }
                s.encoder.finish();
            }
        });
    }

    /// The Display and Window sources that show now, each blended on its
    /// own: one that's come into the frames (shown, put in the scene, or
    /// another display or window now) gets its blend, starting from its
    /// newest picture; one that's gone is let go of. Then whether the scene
    /// shows the recording's own capture, and whose pictures are told.
    /// Once a frame, between pictures, never while one's being taken in
    /// (the sources keep their places in the list meanwhile).
    fn follow_layers(&mut self) {
        let Some(feeds) = self.feeds.clone() else { return };
        let want = feeds.frame_blended();
        let mut i = 1;
        while i < self.sources.len() {
            let s = &self.sources[i];
            if want.iter().any(|(id, latest, _)| *id == s.id && Arc::ptr_eq(latest, &s.latest)) {
                i += 1;
                continue;
            }
            let s = self.sources.remove(i);
            eprintln_later!("frame blending: {} isn't shown, no longer blended", s.name);
            self.let_go(s);
        }
        for (id, latest, name) in want {
            if self.sources.iter().any(|s| s.id == id) {
                continue;
            }
            match FrameBlend::new(&self.sources[0].blend.gpu, &latest, self.looks as u32) {
                Ok(blend) => {
                    latest.set_source(id);
                    latest.set_bell(Some(self.bell.clone()));
                    latest.set_wants_queue(true);
                    feeds.set_blend(id, Some(blend.average.clone()));
                    // Its looks from the frame under way, where the
                    // recording's own capture's are, till its first picture
                    // puts them halfway between its own.
                    let main = &self.sources[0];
                    let mut s = Source::new(id, name, latest, blend, main.phase, main.next);
                    s.hold_newest(&self.quad);
                    eprintln_later!("frame blending: {} shown, blended on its own", s.name);
                    self.sources.push(s);
                }
                Err(e) => eprintln_later!("frame blending of {name}: {e:#}"),
            }
        }
        // The recording's own capture, while the scene shows it: else its
        // looks would be blended for nothing, and its pictures copied for
        // the blend for nothing (all of a 540 Hz display's, while anything
        // on it moves), each waking the pacer (which, measured, made
        // another display's pictures come in less evenly). It goes on
        // meanwhile, so it shows again at once, from its newest picture.
        let main_shown = feeds.layers().iter().any(|l| matches!(l.source, crate::LayerSource::Main)) && !self.screen.lock().unwrap().hidden;
        let main = &mut self.sources[0];
        if main.shown != main_shown {
            main.shown = main_shown;
            main.different = 0;
            main.latest.set_wants_queue(main_shown);
            main.latest.set_bell(main_shown.then(|| self.bell.clone()));
            if main_shown {
                main.hold_newest(&self.quad);
            } else {
                drop(main.latest.drain_queue());
            }
        }
        // Whose pictures are told: the recording's own capture's while it
        // shows, else those of the source showing the most (the first of
        // those showing as many).
        let now = host_now();
        let busiest = self.sources[1..].iter().fold(None::<&Source>, |b, s| if b.is_some_and(|b| b.rate.per_second(now) >= s.rate.per_second(now)) { b } else { Some(s) });
        self.told = if main_shown { 0 } else { busiest.map_or(0, |s| s.id) };
        self.say_sources();
    }

    /// A Display or Window source the frames don't show anymore: its
    /// capture's pictures aren't queued for it, and frames draw it from its
    /// newest picture.
    fn let_go(&self, s: Source) {
        s.latest.set_wants_queue(false);
        s.latest.set_bell(None);
        drop(s.latest.drain_queue());
        if let Some(f) = &self.feeds {
            f.set_blend(s.id, None);
        }
    }

    /// Which sources the looks are at, and whose pictures are told, for how
    /// the capture keeps up (`crate::health`).
    fn say_sources(&self) {
        crate::blend_sources(self.sources.iter().filter(|s| s.shown).count(), self.told);
    }

    /// Take every look before `t`, each source's: each sees the picture it
    /// has up. Then the frames every source has taken its looks for are made.
    fn look_until(&mut self, t: f64) {
        for i in 0..self.sources.len() {
            self.take_looks(i, t);
        }
        self.close_done();
        self.stats.log(&mut self.sources, self.looks as u32, self.told);
    }

    /// Take source `i`'s looks before `t`.
    fn take_looks(&mut self, i: usize, t: f64) {
        let mut burst = 0;
        while self.look_time(i, self.sources[i].next) < t && burst < 10_000 {
            self.look(i);
            burst += 1;
        }
    }

    /// Source `i`'s next look: the picture it has up, added to the frames
    /// the look counts toward, if the frames show it.
    fn look(&mut self, i: usize) {
        let (looks, closed) = (self.looks, self.closed);
        let s = &mut self.sources[i];
        let (first, weights) = self.shutter.split(s.next);
        if s.shown
            && let Some(id) = s.held
        {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > closed && weight > 0.0 {
                    s.blend.add(k, weight);
                }
            }
            if s.last_taken != Some(id) {
                s.different += 1;
                s.last_taken = Some(id);
            }
        }
        s.next += 1;
        if s.next % looks == 0 {
            let different = std::mem::take(&mut s.different);
            if s.shown && s.held.is_some() {
                s.stats.frame(different, looks as u32);
            }
        }
    }

    /// The frames no source's later looks count toward are done: made.
    fn close_done(&mut self) {
        let Some(next) = self.sources.iter().map(|s| s.next).min() else { return };
        let (first, _) = self.shutter.split(next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: encode it.
    fn close(&mut self, k: i64) {
        self.closed = k;
        self.follow_due = true;
        let started = Instant::now();
        let late = host_now() - self.frame_done_at(k);
        if !self.started() {
            return;
        }
        let mut steps = CloseSteps::default();
        let mut lap = Instant::now();
        let mut took = || {
            let now = Instant::now();
            let d = now - lap;
            lap = now;
            d
        };
        if let Some(f) = &self.feeds {
            f.update();
        }
        steps.webcam = took();
        self.diag.free_min = self.diag.free_min.min(self.encoder.pool.free_count());
        // The stream's pictures want this frame too: each blending its own
        // frames wants every one, else those at its frames' times.
        let blending = self.looks > 1;
        let streams_want = self.stream.iter().any(|s| s.wants(k, blending));
        match self.encoder.pool.take() {
            Some(slot) => {
                self.finish_sources(k);
                steps.blend = took();
                let converted = self.converter.convert(slot);
                steps.convert = took();
                match converted {
                    Ok(()) => {
                        if self.encoder.encode(slot, k) {
                            encoded();
                        } else {
                            skipped(&mut self.dropped, 1);
                            self.diag.queue_full += 1;
                        }
                        steps.encode = took();
                    }
                    Err(e) => {
                        self.encoder.pool.give(slot);
                        eprintln_later!("{e:#}");
                    }
                }
            }
            // No frame free for the encoder: skipped, and not blended (work
            // a graphics card that's behind can't spare), unless the stream
            // takes it.
            None => {
                if streams_want {
                    self.finish_sources(k);
                } else {
                    for s in &mut self.sources {
                        s.blend.drop_frame(k);
                    }
                }
                skipped(&mut self.dropped, 1);
                self.diag.pool_empty += 1;
            }
        }
        if streams_want {
            let main = &self.sources[0].blend;
            let content = main.average.content();
            for s in &mut self.stream {
                s.frame(&main.gpu, k, blending, content);
            }
        }
        let _ = took();
        if let Err(e) = self.preview.submit() {
            eprintln_later!("preview: {e:#}");
        }
        steps.preview = took();
        self.diag.frame(&self.sources[0].blend.gpu, late, started.elapsed(), steps);
    }

    /// Frame `k` made of each source the frames show; the looks of one they
    /// don't (the recording's own capture, out of the scene) let go of.
    fn finish_sources(&mut self, k: i64) {
        for s in &mut self.sources {
            if s.shown {
                s.finish(&self.quad, k);
            } else {
                s.blend.drop_frame(k);
            }
        }
    }

    /// Every source's pictures that came up since, in the order they did,
    /// each taken in as it came (see `arrive`). A source's own pictures go
    /// in the order its capture had them, whatever their times say.
    fn take_pictures(&mut self) {
        let now = host_now();
        let mut came: Vec<(f64, usize, Option<d3d::Queued>)> = Vec::new();
        for (i, s) in self.sources.iter_mut().enumerate() {
            let mut last = f64::MIN;
            for q in s.latest.drain_queue() {
                last = last.max(q.at);
                came.push((last, i, Some(q)));
            }
            // Put up some other way (a still): taken from the capture.
            let others = s.latest.others();
            if others != s.others {
                s.others = others;
                came.push((now.max(last), i, None));
            }
        }
        came.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (at, i, picture) in came {
            self.arrive(i, at, picture);
        }
    }

    /// A new picture of source `i` came up at `at`: its looks before it saw
    /// the one before. Then its looks move toward halfway between its
    /// pictures.
    fn arrive(&mut self, i: usize, at: f64, picture: Option<d3d::Queued>) {
        let (id, shown) = (self.sources[i].id, self.sources[i].shown);
        let now = host_now();
        // Only a source that shows is waited for and counted.
        if shown {
            crate::count_picture_of(id);
            self.lateness.note(now, now - at);
            self.stats.wait = self.stats.wait.max(self.wait());
            if at < self.look_time(i, self.sources[i].next - 1) {
                self.sources[i].stats.late += 1;
                crate::count_missed_of(id, 1);
            }
        }
        self.take_looks(i, at);
        self.close_done();
        // Pictures belong on whole steps of its looks (halfway between
        // two); the first its capture timed puts them there.
        let step = self.step;
        let since = at - self.t0;
        let s = &mut self.sources[i];
        let first = picture.is_some() && !s.timed;
        s.timed |= picture.is_some();
        s.phase = crate::shutter::follow_picture(s.phase, step, since, first);
        if picture.is_some() {
            let steps = (since - s.phase) / step;
            s.stats.timing.note(at, steps, step);
        }
        match picture {
            Some(q) => s.blend.hold_queued(&self.quad, q),
            None => s.blend.hold(&self.quad, &s.latest),
        }
        s.pictures += 1;
        s.held = Some(s.pictures);
        s.stats.pictures += 1;
        s.rate.note(now);
    }
}

/// What the blending saw, printed every few seconds (to the log), a line
/// for each source the frames showed (see [`SourceStats`]); and the frames
/// skipped to catch up (and how far behind it was at most).
struct BlendStats {
    since: Instant,
    caught_up: u32,
    behind: f64,
    /// The longest the looks waited for late pictures (see `BlendPacer::wait`).
    wait: f64,
}

/// One source's part of [`BlendStats`]: how many pictures it showed, how
/// many different ones each frame got (all its looks, when it changes at
/// least at the blend rate), and how many came too late for their frame.
struct SourceStats {
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
    /// When its pictures came, with `HASTECLIP_BLEND_LOG=1`.
    timing: Timing,
}

/// When a source's pictures came against the looks (`HASTECLIP_BLEND_LOG=1`
/// logs it): how far apart, in looks (a quarter of one each), where between
/// two looks each fell (a picture halfway between is right for the looks
/// either side of it), and how many came before the one before them.
#[derive(Default)]
struct Timing {
    last: Option<f64>,
    apart: [u32; 9],
    within: [u32; 4],
    backwards: u32,
}

impl Timing {
    /// A picture came at `at`, `steps` looks into the looks' time.
    fn note(&mut self, at: f64, steps: f64, step: f64) {
        if let Some(last) = self.last {
            if at < last {
                self.backwards += 1;
            }
            let apart = ((at - last).max(0.0) / step * 4.0) as usize;
            self.apart[apart.min(8)] += 1;
        }
        self.last = Some(at);
        self.within[((steps.rem_euclid(1.0) * 4.0) as usize).min(3)] += 1;
    }
}

impl SourceStats {
    fn new() -> Self {
        Self { pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0, timing: Timing::default() }
    }

    /// A frame's own looks are in: `different` pictures among them.
    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
    }
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now(), caught_up: 0, behind: 0.0, wait: 0.0 }
    }

    fn caught_up(&mut self, frames: u32, behind: f64) {
        self.caught_up += frames;
        self.behind = self.behind.max(behind);
    }

    /// Every few seconds: a line for each source the frames showed (and,
    /// with `HASTECLIP_BLEND_LOG=1`, when its pictures came), then it
    /// counts afresh. `told`: the source whose pictures the capture's
    /// health tells.
    fn log(&mut self, sources: &mut [Source], looks: u32, told: u64) {
        let gone = self.since.elapsed();
        if gone < Duration::from_secs(5) {
            return;
        }
        let kind = if looks == 1 { "frame timing" } else { "frame blending" };
        for s in sources.iter_mut() {
            let st = std::mem::replace(&mut s.stats, SourceStats::new());
            if st.frames == 0 {
                continue;
            }
            let who = if s.id == 0 { format!("{kind}: the game") } else { format!("{kind} of {}: it", s.name) };
            static TIMING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *TIMING.get_or_init(|| std::env::var_os("HASTECLIP_BLEND_LOG").is_some()) {
                let t = &st.timing;
                eprintln_later!(
                    "{who}: its pictures by how far apart they came, in quarters of a look's time (0 to 8 and more), {:?}; by where between two looks each fell, in quarters from halfway between them (the middle two are near a look), {:?}; {} came before the one before{}",
                    t.apart,
                    t.within,
                    t.backwards,
                    if s.id == told { "; its pictures are told" } else { "" }
                );
            }
            // On the pacer: written by another thread.
            eprintln_later!(
                "{who} showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late (waited up to {:.0} ms), over {} frames{}",
                st.pictures as f64 / gone.as_secs_f64(),
                st.fewest,
                st.most,
                st.full as f64 * 100.0 / st.frames as f64,
                st.late,
                self.wait * 1000.0,
                st.frames,
                match self.caught_up {
                    n if n > 0 && s.id == 0 => format!("; fell up to {:.2} s behind, skipped {n} frames to catch up", self.behind),
                    _ => String::new(),
                }
            );
        }
        *self = Self::new();
    }
}

/// Like [`spawn_pacer`], for frame blending (see [`FrameBlend`]): the
/// recording's own capture's pictures from `latest`, blended by `blend`,
/// and each Display and Window source's in `feeds` blended on its own.
#[allow(clippy::too_many_arguments)]
fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    shutter: crate::shutter::Shutter,
    latest: Arc<d3d::Latest>,
    blend: FrameBlend,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    preview: d3d::Previewer,
    feeds: Option<Arc<layers::Feeds>>,
    screen: crate::SharedScreen,
    clock: Arc<Clock>,
    ctl: PacerCtl,
) -> Result<(Arc<AtomicBool>, JoinHandle<()>)> {
    let quad = quad::Quad::new(&blend.gpu)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        system::multimedia_thread("Capture");
        crate::blending(fps, looks);
        let t0 = host_now();
        clock.set(t0);
        let bell = Arc::new(d3d::Bell::default());
        latest.set_bell(Some(bell.clone()));
        let mut p = BlendPacer {
            t0,
            fps,
            step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
            looks: looks.max(1) as i64,
            shutter,
            closed: -1,
            sources: vec![Source::new(0, "the recording's own capture".to_owned(), latest, blend, 0.0, 0)],
            told: 0,
            quad,
            bell,
            screen,
            follow_due: true,
            lateness: Lateness::default(),
            converter,
            encoder,
            preview,
            feeds,
            stream: Vec::new(),
            dropped: 0,
            stats: BlendStats::new(),
            diag: EncodeDiag::new(),
        };
        p.sources[0].hold_newest(&p.quad);
        while !stop2.load(Ordering::Relaxed) {
            // Frame blending changed, the stream started or stopped.
            if let Some((looks, shape)) = ctl.blend.lock().unwrap().take() {
                p.change_blend(looks, shape);
            }
            p.take_stream(&ctl);
            if std::mem::take(&mut p.follow_due) {
                p.follow_layers();
            }
            p.catch_up(host_now() - p.wait());
            // Rung by any source's capture once something new is there.
            let rung = p.bell.rung();
            let wait = p.frame_end() + p.wait() - host_now();
            if wait > 0.0 && p.sources.iter().all(|s| s.latest.queue_len() == 0) {
                p.bell.wait(rung, Duration::from_secs_f64(wait.min(0.05)));
            }
            p.take_pictures();
            // Looks whose time is past (and late pictures' too) see what's up.
            p.look_until(host_now() - p.wait());
        }
        // The layers' captures queue no more pictures (they run on until
        // the session's done with them).
        let layers: Vec<Source> = p.sources.drain(1..).collect();
        for s in layers {
            p.let_go(s);
        }
        p.sources[0].latest.set_bell(None);
        for s in std::mem::take(&mut p.stream) {
            s.encoder.finish();
        }
        p.encoder.finish();
    });
    Ok((stop, handle))
}

/// A frame went to the encoder: counted for the UI (the share skipped).
fn encoded() {
    crate::count_encoded();
}

/// The encoder can't keep up (no free frame for it, or its queue is full),
/// or the pacer fell far behind: `n` frames are skipped, and the file shows
/// the one before them longer. Counted for the UI, and logged each time the
/// total passes a power of two.
fn skipped(dropped: &mut u64, n: u64) {
    let before = *dropped;
    *dropped += n;
    crate::count_skipped(n);
    if n > 0 && (before == 0 || dropped.ilog2() > before.ilog2()) {
        eprintln_later!("video is behind: {dropped} frames skipped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    /// The picture's one colour, as grey (0–1).
    fn fill(gpu: &d3d::Gpu, latest: &d3d::Latest, grey: f32) {
        let target = quad::target_view(gpu, &latest.texture).unwrap();
        unsafe { gpu.context.ClearRenderTargetView(&target, &[grey, grey, grey, 1.0]) };
    }

    /// The first pixel's green, 0–255.
    fn read(gpu: &d3d::Gpu, latest: &d3d::Latest) -> u8 {
        let staging = gpu.texture(latest.width, latest.height, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32).unwrap();
        unsafe {
            gpu.context.CopyResource(&staging, &latest.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let g = *(mapped.pData as *const u8).add(1);
            gpu.context.Unmap(&staging, 0);
            g
        }
    }

    /// Blended softly, two frames are made at once, each from its own looks
    /// by its own weights: white then black, a quarter and three quarters
    /// of the light.
    #[test]
    fn two_frames_at_once() {
        let Ok(gpu) = d3d::Gpu::new() else {
            eprintln!("no graphics card: skipped");
            return;
        };
        let latest = d3d::Latest::new(&gpu, 8, 8).unwrap();
        let quad = quad::Quad::new(&gpu).unwrap();
        let mut blend = FrameBlend::new(&gpu, &latest, 4).unwrap();
        fill(&gpu, &latest, 1.0);
        blend.hold(&quad, &latest);
        blend.add(0, 0.25);
        blend.add(1, 0.75);
        fill(&gpu, &latest, 0.0);
        blend.hold(&quad, &latest);
        blend.add(0, 0.75);
        blend.add(1, 0.25);
        blend.finish(&quad, 0);
        let first = read(&gpu, &blend.average);
        blend.finish(&quad, 1);
        let second = read(&gpu, &blend.average);
        // sRGB of 0.25 light is 0.537 (137), of 0.75 is 0.881 (225).
        assert!(first.abs_diff(137) <= 1, "{first}");
        assert!(second.abs_diff(225) <= 1, "{second}");
        assert!(blend.is_empty(2) && blend.is_empty(3), "both start empty again");
    }

    /// A source whose looks are ahead of another's starts the next frame
    /// before the one before is made: each frame keeps its own looks,
    /// blended evenly too (one average a frame, a measured clip had five
    /// looks in some frames), and with one look a frame, each frame is the
    /// picture its look saw.
    #[test]
    fn the_next_frame_starts_before_this_one_is_made() {
        let Ok(gpu) = d3d::Gpu::new() else {
            eprintln!("no graphics card: skipped");
            return;
        };
        let latest = d3d::Latest::new(&gpu, 8, 8).unwrap();
        let quad = quad::Quad::new(&gpu).unwrap();
        for looks in [4, 1] {
            let mut blend = FrameBlend::new(&gpu, &latest, looks).unwrap();
            fill(&gpu, &latest, 1.0);
            blend.hold(&quad, &latest);
            for _ in 0..looks {
                blend.add(0, 1.0);
            }
            // Frame 1's first looks, at a black picture, before frame 0 is made.
            fill(&gpu, &latest, 0.0);
            blend.hold(&quad, &latest);
            blend.add(1, 1.0);
            blend.finish(&quad, 0);
            assert_eq!(read(&gpu, &blend.average), 255, "{looks} looks: frame 0 is its own white looks");
            for _ in 1..looks {
                blend.add(1, 1.0);
            }
            blend.finish(&quad, 1);
            assert_eq!(read(&gpu, &blend.average), 0, "{looks} looks: frame 1 is its own black looks");
            // Making frame 0 left frame 1's looks as they were.
            assert_eq!(blend.copied, Some(blend.held.id), "{looks} looks: frame 1 saw only the black picture: copied, not blended");
            assert!(blend.is_empty(0) && blend.is_empty(1));
        }
    }

    /// With one look a frame, a frame made after two more pictures came up
    /// (another source's look for it later) is still the picture its look
    /// saw, although the texture it came in holds another by then.
    #[test]
    fn one_look_keeps_its_picture() {
        let Ok(gpu) = d3d::Gpu::new() else {
            eprintln!("no graphics card: skipped");
            return;
        };
        let latest = d3d::Latest::new(&gpu, 8, 8).unwrap();
        let quad = quad::Quad::new(&gpu).unwrap();
        let mut blend = FrameBlend::new(&gpu, &latest, 1).unwrap();
        fill(&gpu, &latest, 1.0);
        blend.hold(&quad, &latest);
        blend.add(0, 1.0);
        // Two more pictures: the second is copied over the white one.
        fill(&gpu, &latest, 0.0);
        blend.hold(&quad, &latest);
        fill(&gpu, &latest, 0.5);
        blend.hold(&quad, &latest);
        blend.finish(&quad, 0);
        assert_eq!(read(&gpu, &blend.average), 255, "frame 0 is the white picture its look saw");
    }

    /// A frame whose looks all saw one picture is that picture as it is,
    /// copied once: the next such frame of the same picture leaves the
    /// blend as it is (a still source costs nothing). A frame of two
    /// pictures is blended again.
    #[test]
    fn one_picture_is_copied_once() {
        let Ok(gpu) = d3d::Gpu::new() else {
            eprintln!("no graphics card: skipped");
            return;
        };
        let latest = d3d::Latest::new(&gpu, 8, 8).unwrap();
        let quad = quad::Quad::new(&gpu).unwrap();
        let mut blend = FrameBlend::new(&gpu, &latest, 4).unwrap();
        fill(&gpu, &latest, 0.5);
        blend.hold(&quad, &latest);
        for k in 0..2 {
            for _ in 0..4 {
                blend.add(k, 1.0);
            }
            blend.finish(&quad, k);
            assert_eq!(blend.copied, Some(blend.held.id), "frame {k}: the picture as it is");
            let grey = read(&gpu, &blend.average);
            assert!(grey.abs_diff(128) <= 1, "frame {k}: {grey}");
        }
        // Drawn over behind its back: a copy again would undo that, so
        // it's seen whether frame 2 copies.
        fill(&gpu, &blend.average, 0.0);
        for _ in 0..4 {
            blend.add(2, 1.0);
        }
        blend.finish(&quad, 2);
        assert_eq!(read(&gpu, &blend.average), 0, "not copied again: it's the frame before's");
        // Half the frame white: blended, as light.
        blend.add(3, 1.0);
        blend.add(3, 1.0);
        fill(&gpu, &latest, 1.0);
        blend.hold(&quad, &latest);
        blend.add(3, 1.0);
        blend.add(3, 1.0);
        blend.finish(&quad, 3);
        assert_eq!(blend.copied, None);
        // Half of sRGB 0.5's light (0.216) and half of 1: 0.608, sRGB 0.80 (205).
        let mixed = read(&gpu, &blend.average);
        assert!(mixed.abs_diff(205) <= 1, "{mixed}");
    }

    /// A fullscreen game that's seen is looked at again 5 s on, then twice as
    /// long each time, up to a minute.
    #[test]
    fn looks_again_later_and_later() {
        let mut sight = Sight::default();
        let waits: Vec<u64> = (0..6)
            .map(|_| {
                sight.look_later();
                sight.wait.as_secs()
            })
            .collect();
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
    }

    /// A window the hook failed on is left to Windows' capture for 10 s, then
    /// twice as long each time it fails again, up to 5 minutes (issue #46).
    #[test]
    fn hook_tried_again_later_and_later() {
        let (mut failures, t0) = (HookFailures::default(), Instant::now());
        let mut waits = Vec::new();
        let mut now = t0;
        for _ in 0..7 {
            failures.failed(1, now);
            let (wait, again) = (failures.0[&1].wait, failures.0[&1].again);
            waits.push(wait.as_secs());
            assert!(failures.holds_off(1, now));
            assert!(!failures.due(1, now));
            now = again;
            assert!(!failures.holds_off(1, now));
            assert!(failures.due(1, now));
            assert!(!failures.due(1, now), "taken up once, till it fails again");
            assert!(!failures.holds_off(1, now), "not held off while it's tried");
        }
        assert_eq!(waits, [10, 20, 40, 80, 160, 300, 300]);
        assert!(!failures.holds_off(2, t0), "another window: never failed");
    }

    /// Back in focus after being left (alt-tab and back), a window the hook
    /// failed on is tried again at once; staying in focus, or not having
    /// left, doesn't; a closed one is forgotten.
    #[test]
    fn hook_tried_again_back_in_focus() {
        let (mut failures, now) = (HookFailures::default(), Instant::now());
        failures.failed(1, now);
        failures.focus(Some(1), now, |_| true);
        assert!(failures.holds_off(1, now), "still in focus: hasn't left");
        failures.focus(None, now, |_| true);
        assert!(failures.holds_off(1, now), "left, not back yet");
        failures.focus(Some(1), now, |_| true);
        assert!(!failures.holds_off(1, now), "back in focus");
        assert!(failures.due(1, now));
        failures.focus(Some(1), now, |w| w != 1);
        assert!(failures.0.is_empty(), "closed");
    }

    /// With nothing running, a screenshot of the main screen is taken by a
    /// capture of its own: the screen's size, opaque, not all black.
    /// `HASTECLIP_SHOT=<file.png>` keeps it to look at.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_of_the_screen -- --ignored"]
    fn screenshot_of_the_screen() {
        let screen = system::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        let (w, h) = system::display_pixels(Some(&screen.id)).expect("its size");
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.bgra.len(), (w * h * 4) as usize);
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
        if let Some(out) = std::env::var_os("HASTECLIP_SHOT") {
            let rgb: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let header = format!("P6\n{w} {h}\n255\n");
            std::fs::write(out, [header.as_bytes(), &rgb].concat()).unwrap();
        }
    }

    /// Clipping, then going live to two places (RTMP servers here: ffmpegs
    /// listening), each sent a picture of its own, frame blending turned on
    /// and off while it all runs, a recording beside them, a clip saved:
    /// each gets what it should, from the one capture. The places at their
    /// own sizes and rates (720p at 30, its frames blended from the
    /// capture's while blending's on, and 1080p at 60, from a 1080p capture
    /// at 60); the recording and the clip at the capture's.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture everything_at_once -- --ignored --nocapture"]
    fn everything_at_once() {
        use crate::stream::{Destination, Link, Status, StreamSettings};
        /// What's taken of the screen goes, and the servers stop, however
        /// the test ends.
        struct Leave(PathBuf, Vec<std::process::Child>);
        impl Drop for Leave {
            fn drop(&mut self) {
                for c in &mut self.1 {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let screen = system::list_screens().into_iter().next().expect("a screen");
        let dir = std::env::temp_dir().join(format!("hc-at-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut leave = Leave(dir.clone(), Vec::new());
        let mut urls = Vec::new();
        for name in ["small", "full"] {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let url = format!("rtmp://127.0.0.1:{port}/live/test_key");
            let got = dir.join(format!("{name}.flv"));
            leave.1.push(crate::stream::ffmpeg().args(["-v", "error", "-y", "-timeout", "30", "-i"]).arg(&url).args(["-c", "copy"]).arg(&got).stdin(std::process::Stdio::null()).spawn().unwrap());
            urls.push(url);
        }
        thread::sleep(Duration::from_millis(700));
        let status = Status::new();
        let mut settings = EncodeSettings {
            output_dir: dir.clone(),
            container_ext: "mp4".into(),
            fps: 60,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 12000,
            size: crate::VideoSize { short_side: Some(1080), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 10,
            video: VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() },
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        let place = |name: &str, url: &str, short_side, fps, kbps| Destination { name: name.into(), url: url.into(), secret: "test_key".into(), quality: crate::stream::Quality { short_side: Some(short_side), fps, kbps } };
        settings.stream = Some(StreamSettings { destinations: vec![place("Small", &urls[0], 720, 30, 3000), place("Full", &urls[1], 1080, 60, 6000)], status: status.clone() });
        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        thread::sleep(Duration::from_secs(1));
        rec.start(Mode::Stream, &settings).unwrap();
        assert_eq!(rec.running(), Running { replay: true, record: false, stream: true });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !status.get().iter().all(|d| matches!(d.link, Link::Live { .. })) {
            assert!(Instant::now() < deadline, "never live: {:?}", status.get());
            thread::sleep(Duration::from_millis(50));
        }
        // Frame blending on, and off again, while it runs.
        assert!(rec.set_blend(4, crate::BlendShape::Soft));
        thread::sleep(Duration::from_secs(2));
        let blending = crate::health().and_then(|h| h.blend).map(|b| b.looks);
        rec.start(Mode::Record, &settings).unwrap();
        thread::sleep(Duration::from_secs(2));
        assert!(rec.set_blend(1, crate::BlendShape::Even));
        thread::sleep(Duration::from_secs(1));
        let clip = rec.save_clip(&dir).unwrap().finish().unwrap();
        let recording = rec.stop(Mode::Record, None).unwrap().expect("a recording");
        assert_eq!(rec.running(), Running { replay: true, record: false, stream: true });
        rec.stop(Mode::Stream, None).unwrap();
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        assert!(!rec.is_running());
        let deadline = Instant::now() + Duration::from_secs(10);
        while leave.1.iter_mut().any(|c| c.try_wait().unwrap().is_none()) {
            assert!(Instant::now() < deadline, "a server didn't end");
            thread::sleep(Duration::from_millis(50));
        }
        let probe = |f: &Path| {
            let out = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=width,height,avg_frame_rate,nb_read_frames:format=duration", "-of", "default=nw=1"])
                .arg(f)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let (s, full, r, c) = (probe(&dir.join("small.flv")), probe(&dir.join("full.flv")), probe(&recording), probe(&clip));
        eprintln!("blending while it ran: {blending:?} looks\nsmall stream:\n{s}\nfull stream:\n{full}\nrecording:\n{r}\nclip:\n{c}");
        assert_eq!(blending, Some(4), "blending changed while running");
        assert!(s.contains("width=1280") && s.contains("height=720") && s.contains("avg_frame_rate=30/1"), "{s}");
        assert!(full.contains("width=1920") && full.contains("height=1080") && full.contains("avg_frame_rate=60/1"), "{full}");
        assert!(r.contains("height=1080") && r.contains("avg_frame_rate=60/1"), "{r}");
        assert!(c.contains("height=1080"), "{c}");
        let secs = |t: &str| t.lines().find_map(|l| l.strip_prefix("duration=")).and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0);
        assert!(secs(&s) > 4.0, "the stream ran on: {}", secs(&s));
        assert!((2.5..4.5).contains(&secs(&r)), "the recording's 3 s: {}", secs(&r));
    }

    /// While clipping at 720p in 1:1, a screenshot is still all of the screen
    /// at its own size: the recording's shape and size aren't the picture's.
    #[test]
    #[ignore = "captures the screen: cargo test -p capture screenshot_while_clipping -- --ignored"]
    fn screenshot_while_clipping() {
        let screen = system::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-shot-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 4000,
            size: crate::VideoSize { short_side: Some(720), shape: Some(crate::Shape { w: 1, h: 1, framing: crate::Framing::Fill }), exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: video.clone(),
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fill))),
            sources: Vec::new(),
            stream: None,
        };
        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let t = Instant::now();
        let img = rec.screenshot(&video).unwrap().finish().unwrap();
        eprintln!("{}×{} in {} ms", img.width, img.height, t.elapsed().as_millis());
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        let (w, h) = system::display_pixels(Some(&screen.id)).expect("its size");
        assert_eq!((img.width, img.height), (w, h));
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 16 || p[1] > 16 || p[2] > 16), "all black");
    }

    /// A display's mode, changed for as long as this lives: not saved, so
    /// dropping it puts back the one in the registry.
    struct ModeChange(Vec<u16>);

    impl ModeChange {
        fn to(id: &str, w: u32, h: u32) -> Result<Self> {
            use windows::Win32::Graphics::Gdi::{CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH};
            let name: Vec<u16> = id.encode_utf16().chain([0]).collect();
            let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            mode.dmPelsWidth = w;
            mode.dmPelsHeight = h;
            mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
            let r = unsafe { ChangeDisplaySettingsExW(windows::core::PCWSTR(name.as_ptr()), Some(&mode), None, CDS_TYPE(0), None) };
            if r != DISP_CHANGE_SUCCESSFUL {
                bail!("couldn't change {id} to {w}×{h}: {r:?}");
            }
            Ok(Self(name))
        }
    }

    impl Drop for ModeChange {
        fn drop(&mut self) {
            use windows::Win32::Graphics::Gdi::{CDS_TYPE, ChangeDisplaySettingsExW};
            unsafe { ChangeDisplaySettingsExW(windows::core::PCWSTR(self.0.as_ptr()), None, None, CDS_TYPE(0), None) };
        }
    }

    /// A small square whose grey keeps changing, at `left`, `top` on the
    /// desktop, so its display's capture always has a new picture. Closed
    /// once `stop` is set.
    fn changing_square(left: i32, top: i32, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        thread::spawn(move || unsafe {
            use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
            use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
            use windows::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows::Win32::UI::WindowsAndMessaging::*;
            extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
                unsafe { DefWindowProcW(h, m, w, l) }
            }
            // Where it goes is in the display's own pixels.
            windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: windows::core::w!("hc_changing_square"), ..Default::default() };
            RegisterClassW(&class);
            // On top, but never in focus or on the taskbar.
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                windows::core::w!("hc_changing_square"),
                windows::core::w!(""),
                WS_POPUP | WS_VISIBLE,
                left,
                top,
                96,
                96,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let dc = GetDC(Some(hwnd));
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                // A slow ramp of greys, up and down: nothing flashes.
                let g = 64 + (n % 128).abs_diff(64);
                let brush = CreateSolidBrush(COLORREF(g | g << 8 | g << 16));
                FillRect(dc, &RECT { left: 0, top: 0, right: 96, bottom: 96 }, brush);
                let _ = DeleteObject(brush.into());
                n += 1;
                thread::sleep(Duration::from_millis(16));
            }
            ReleaseDC(Some(hwnd), dc);
            let _ = DestroyWindow(hwnd);
        })
    }

    /// Issue #16: a display's mode changing while it's recorded (there from
    /// 1920×1080 to 1440×1080) froze the picture with Desktop Duplication
    /// until the app was started again: each duplication started again was
    /// lost at once. Pictures must keep coming, at the new size, and again
    /// once the mode is back, from Desktop Duplication itself (not Windows'
    /// capture standing in). `SCREEN=<id>`: a display whose mode may change
    /// for half a minute; `MODE=WxH`: a mode the system lists for it (turned,
    /// on a turned display); `METHOD=dupl|wgc`; `HOLD=<s>` in each mode (15).
    #[test]
    #[ignore = "changes a display's mode: SCREEN=\\\\.\\DISPLAY2 MODE=1080x1440 METHOD=dupl cargo test -p capture display_mode_change -- --ignored --nocapture"]
    fn display_mode_change() {
        let env = |k: &str| std::env::var(k).ok();
        let mode = env("MODE").and_then(|m| {
            let (w, h) = m.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
        let (Some(id), Some((w, h))) = (env("SCREEN"), mode) else {
            eprintln!("SCREEN and MODE not set: skipped");
            return;
        };
        // Never the main display instead.
        assert!(system::list_screens().iter().any(|s| s.id == id), "no display {id}");
        let hold = Duration::from_secs_f64(env("HOLD").and_then(|s| s.parse().ok()).unwrap_or(15.0));
        let duplication = env("METHOD").as_deref() != Some("wgc");
        let method = if duplication { crate::ScreenCapture::Duplication } else { crate::ScreenCapture::Wgc };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-mode-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 2000,
            size: crate::VideoSize { short_side: Some(360), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: VideoSource::Screen { id: id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method },
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
        unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(system::find_monitor(&id), &mut info) }.ok().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let square = changing_square(info.rcMonitor.left + 40, info.rcMonitor.top + 40, stop.clone());

        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        let latest = rec.outputs.session.as_ref().unwrap().switch.as_ref().unwrap().latest.clone();
        let t0 = Instant::now();
        let mut seen = latest.arrivals().0;
        // Pictures a second, over each half second for `for_`, and how the
        // display was captured at the end.
        let mut watch = |phase: &str, for_: Duration| {
            let mut rates = Vec::new();
            let end = Instant::now() + for_;
            while Instant::now() < end {
                thread::sleep(Duration::from_millis(500));
                let n = latest.arrivals().0;
                rates.push((n - seen) * 2);
                eprintln!("{:5.1} s {phase}: {:3} pictures/s, {:?}, {:?}", t0.elapsed().as_secs_f64(), (n - seen) * 2, latest.content(), crate::screen_capture_in_use());
                seen = n;
            }
            (rates, crate::screen_capture_in_use())
        };
        let (before, _) = watch("before", Duration::from_secs(3));
        let changed = ModeChange::to(&id, w, h).unwrap();
        let (during, during_in_use) = watch("changed", hold);
        let changed_content = latest.content();
        drop(changed);
        let (back, back_in_use) = watch("back", hold);
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        stop.store(true, Ordering::Relaxed);
        let _ = square.join();

        // The last 3 s of each.
        let rate = |v: &[u64]| v[v.len().saturating_sub(6)..].iter().sum::<u64>() as f64 / v.len().clamp(1, 6) as f64;
        eprintln!("pictures/s: before {:.0}, changed {:.0}, back {:.0}", rate(&before), rate(&during), rate(&back));
        assert!(rate(&during) > 5.0, "no pictures since the mode changed");
        assert!(rate(&back) > 5.0, "no pictures since the mode came back");
        assert_eq!(changed_content, (w, h), "the picture isn't the new mode's size");
        let own = Some(crate::ScreenCaptureInUse { duplication, fell_back: false });
        assert_eq!((during_in_use, back_in_use), (own, own), "not captured the way it was asked to be");
    }

    /// Issue #34: Windows turning the displays off to save power, and on
    /// again, froze a display's recording on its last picture until it was
    /// started again (Windows' capture went on at 4 pictures a second, all
    /// the same). A changing square's pictures must keep coming once the
    /// displays are back. Turns every display off for `OFF=<s>` (30), then
    /// on again. `SCREEN=<id>`; `METHOD=wgc|dupl` (wgc, as on Windows 11).
    #[test]
    #[ignore = "turns every display off for a while: SCREEN=\\\\.\\DISPLAY1 OFF=30 cargo test -p capture display_sleep -- --ignored --nocapture"]
    fn display_sleep() {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Power::{ES_DISPLAY_REQUIRED, SetThreadExecutionState};
        use windows::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, PostMessageW, SC_MONITORPOWER, WM_SYSCOMMAND};
        let env = |k: &str| std::env::var(k).ok();
        let Some(id) = env("SCREEN") else {
            eprintln!("SCREEN not set: skipped");
            return;
        };
        assert!(system::list_screens().iter().any(|s| s.id == id), "no display {id}");
        let off = Duration::from_secs_f64(env("OFF").and_then(|s| s.parse().ok()).unwrap_or(30.0));
        let duplication = env("METHOD").as_deref() == Some("dupl");
        let method = if duplication { crate::ScreenCapture::Duplication } else { crate::ScreenCapture::Wgc };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-sleep-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 2000,
            size: crate::VideoSize { short_side: Some(360), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: VideoSource::Screen { id: id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method },
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
        unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(system::find_monitor(&id), &mut info) }.ok().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let square = changing_square(info.rcMonitor.left + 40, info.rcMonitor.top + 40, stop.clone());

        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        let latest = rec.outputs.session.as_ref().unwrap().switch.as_ref().unwrap().latest.clone();
        let t0 = Instant::now();
        let mut seen = latest.arrivals().0;
        // Pictures a second, over each half second for `for_`.
        let mut watch = |phase: &str, for_: Duration| {
            let mut rates = Vec::new();
            let end = Instant::now() + for_;
            while Instant::now() < end {
                thread::sleep(Duration::from_millis(500));
                let n = latest.arrivals().0;
                rates.push((n - seen) * 2);
                eprintln!("{:5.1} s {phase}: {:3} pictures/s, {:?}", t0.elapsed().as_secs_f64(), (n - seen) * 2, crate::screen_capture_in_use());
                seen = n;
            }
            rates
        };
        let before = watch("before", Duration::from_secs(3));
        // Every display off, as Windows does after the power plan's time.
        unsafe { PostMessageW(Some(HWND_BROADCAST), WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER as usize), LPARAM(2)) }.unwrap();
        watch("off", off);
        // On again, as at a nudge of the mouse.
        unsafe {
            SetThreadExecutionState(ES_DISPLAY_REQUIRED);
            let _ = PostMessageW(Some(HWND_BROADCAST), WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER as usize), LPARAM(-1));
        }
        let back = watch("back on", Duration::from_secs(15));
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        stop.store(true, Ordering::Relaxed);
        let _ = square.join();

        // The last 5 s of each.
        let rate = |v: &[u64]| v[v.len().saturating_sub(10)..].iter().sum::<u64>() as f64 / v.len().clamp(1, 10) as f64;
        eprintln!("pictures/s: before {:.0}, back on {:.0}", rate(&before), rate(&back));
        assert!(rate(&before) > 10.0, "the square's pictures didn't come before the displays went off");
        assert!(rate(&back) > 10.0, "the picture froze once the displays were back on");
    }

    /// Issue #34, if it's the lock screen: the PC locked (as Windows can when
    /// it turns the display off) and unlocked again. Locks the PC as Win+L
    /// does, waits up to `WAIT=<s>` (600) for it to be unlocked by hand, and
    /// wants a changing square's pictures back after. `SCREEN=<id>`;
    /// `METHOD=wgc|dupl` (wgc, as on Windows 11).
    #[test]
    #[ignore = "locks the PC, to be unlocked by hand: SCREEN=\\\\.\\DISPLAY1 cargo test -p capture display_lock -- --ignored --nocapture"]
    fn display_lock() {
        let env = |k: &str| std::env::var(k).ok();
        let Some(id) = env("SCREEN") else {
            eprintln!("SCREEN not set: skipped");
            return;
        };
        assert!(system::list_screens().iter().any(|s| s.id == id), "no display {id}");
        let wait = Duration::from_secs_f64(env("WAIT").and_then(|s| s.parse().ok()).unwrap_or(600.0));
        let duplication = env("METHOD").as_deref() == Some("dupl");
        let method = if duplication { crate::ScreenCapture::Duplication } else { crate::ScreenCapture::Wgc };
        let settings = EncodeSettings {
            output_dir: std::env::temp_dir().join("hc-lock-test"),
            container_ext: "mp4".into(),
            fps: 30,
            blend_samples: 1,
            blend_shape: crate::BlendShape::Even,
            video_bitrate_kbps: 2000,
            size: crate::VideoSize { short_side: Some(360), shape: None, exact: None },
            use_hardware: true,
            replay_seconds: 5,
            video: VideoSource::Screen { id: id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method },
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(std::sync::Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
        unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(system::find_monitor(&id), &mut info) }.ok().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let square = changing_square(info.rcMonitor.left + 40, info.rcMonitor.top + 40, stop.clone());

        let mut rec = WinRecorder::new(Arc::new(LiveAudio::default()));
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        let latest = rec.outputs.session.as_ref().unwrap().switch.as_ref().unwrap().latest.clone();
        let t0 = Instant::now();
        let mut seen = latest.arrivals().0;
        // Pictures a second over the next half second.
        let mut step = |phase: &str| {
            thread::sleep(Duration::from_millis(500));
            let n = latest.arrivals().0;
            let rate = (n - seen) * 2;
            eprintln!("{:6.1} s {phase}: {rate:3} pictures/s, {:?}", t0.elapsed().as_secs_f64(), crate::screen_capture_in_use());
            seen = n;
            rate
        };
        let before: Vec<u64> = (0..6).map(|_| step("before")).collect();
        // Locked, as Win+L does.
        assert!(std::process::Command::new("rundll32.exe").arg("user32.dll,LockWorkStation").status().unwrap().success(), "couldn't lock");
        let (start, mut was) = (Instant::now(), false);
        loop {
            step("locked");
            let now = system::session_locked() == Some(true);
            if was && !now {
                break;
            }
            was |= now;
            assert!(start.elapsed() < wait, "not unlocked within {wait:?}");
        }
        eprintln!("{:6.1} s unlocked", t0.elapsed().as_secs_f64());
        let back: Vec<u64> = (0..30).map(|_| step("unlocked")).collect();
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        stop.store(true, Ordering::Relaxed);
        let _ = square.join();

        // The last 5 s of each.
        let rate = |v: &[u64]| v[v.len().saturating_sub(10)..].iter().sum::<u64>() as f64 / v.len().clamp(1, 10) as f64;
        eprintln!("pictures/s: before {:.0}, unlocked {:.0}", rate(&before), rate(&back));
        assert!(rate(&before) > 10.0, "the square's pictures didn't come before the PC was locked");
        assert!(rate(&back) > 10.0, "the picture froze once the PC was unlocked");
    }

    /// While someone is at the PC, its session reads as unlocked (what tells
    /// a lock, see `display_lock`).
    #[test]
    #[ignore = "needs the PC unlocked: cargo test -p capture session_reads_unlocked -- --ignored"]
    fn session_reads_unlocked() {
        assert_eq!(system::session_locked(), Some(false));
    }

    /// The pixel at `x`, `y`, as BGRA.
    fn pixel(gpu: &d3d::Gpu, latest: &d3d::Latest, x: u32, y: u32) -> [u8; 4] {
        let staging = gpu.texture(1, 1, DXGI_FORMAT_B8G8R8A8_UNORM, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32).unwrap();
        let region = D3D11_BOX { left: x, top: y, front: 0, right: x + 1, bottom: y + 1, back: 1 };
        unsafe {
            gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &latest.texture, 0, Some(&region));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let p = *(mapped.pData as *const [u8; 4]);
            gpu.context.Unmap(&staging, 0);
            p
        }
    }

    /// A display bigger than the picture it's captured into (its mode grew
    /// after the capture started) is shrunk to fit, keeping its shape: drawn
    /// where it fits and nowhere else, not squeezed into all of it (Desktop
    /// Duplication) or cut off (Windows' capture). Nothing of the screen is
    /// kept. `SCREEN=<id>` (the main display).
    #[test]
    #[ignore = "captures the screen: cargo test -p capture shrinks_a_display_too_big -- --ignored"]
    fn shrinks_a_display_too_big() {
        let id = std::env::var("SCREEN").unwrap_or_else(|_| system::list_screens().into_iter().next().expect("a screen").id);
        let monitor = system::find_monitor(&id);
        let (w, h) = system::display_pixels(Some(&id)).expect("its size");
        let gpu = d3d::Gpu::new().unwrap();
        // Half its size, with room left under it.
        let (tw, th) = (w / 2, h / 2 + 64);
        let fitted = quad::fit_within(w, h, tw, th);
        let magenta = [255, 0, 255, 255];
        for duplication in [true, false] {
            let latest = d3d::Latest::new(&gpu, tw, th).unwrap();
            let target = quad::target_view(&gpu, &latest.texture).unwrap();
            unsafe { gpu.context.ClearRenderTargetView(&target, &[1.0, 0.0, 1.0, 1.0]) };
            let method = if duplication { "Desktop Duplication" } else { "Windows' capture" };
            // Kept until its picture's been looked at.
            let (dupl, wgc) = if duplication {
                (Some(dupl::DuplCapture::start(&gpu, monitor, &latest, 30).unwrap()), None)
            } else {
                (None, Some(d3d::Capture::start(&gpu, &d3d::monitor_item(monitor).unwrap(), &latest, d3d::Of::Display(monitor), 30).unwrap()))
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            while !latest.has_frame() {
                assert!(Instant::now() < deadline, "{method}: no picture");
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(latest.content(), fitted, "{method}");
            assert_ne!(pixel(&gpu, &latest, fitted.0 / 2, fitted.1 / 2), magenta, "{method}: nothing drawn where it fits");
            assert_eq!(pixel(&gpu, &latest, fitted.0 / 2, fitted.1 + 32), magenta, "{method}: drawn past where it fits");
            eprintln!("{method}: {w}×{h} into {tw}×{th}, as {}×{}", fitted.0, fitted.1);
            drop((dupl, wgc));
        }
    }

    /// Issue #33: Windows' capture leaves the cursor out, and it's drawn on
    /// here, so a cursor moving over a still picture brings no picture from
    /// Windows. It's put up again with the cursor where it moved: pictures
    /// keep coming while it moves, near the frame rate. Moves the cursor
    /// about the middle of `SCREEN=<id>` for a second, then puts it back.
    /// Nothing of the screen is kept.
    #[test]
    #[ignore = "moves the cursor on a display: SCREEN=\\\\.\\DISPLAY2 cargo test -p capture cursor_over_windows_capture -- --ignored --nocapture"]
    fn cursor_over_windows_capture() {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let Ok(id) = std::env::var("SCREEN") else {
            eprintln!("SCREEN not set: skipped");
            return;
        };
        assert!(system::list_screens().iter().any(|s| s.id == id), "no display {id}");
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let monitor = system::find_monitor(&id);
        let (w, h) = system::display_pixels(Some(&id)).expect("its size");
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
        unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info) }.ok().unwrap();
        let r = info.rcMonitor;
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let gpu = d3d::Gpu::new().unwrap();
        let latest = d3d::Latest::new(&gpu, w, h).unwrap();
        let capture = d3d::Capture::start(&gpu, &d3d::monitor_item(monitor).unwrap(), &latest, d3d::Of::Display(monitor), 30).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !latest.has_frame() {
            assert!(Instant::now() < deadline, "no picture");
            thread::sleep(Duration::from_millis(10));
        }
        let mut was = POINT::default();
        unsafe { GetCursorPos(&mut was) }.unwrap();
        unsafe { SetCursorPos(cx, cy) }.unwrap();
        thread::sleep(Duration::from_millis(300));
        let before = latest.arrivals().0;
        thread::sleep(Duration::from_secs(1));
        let still = latest.arrivals().0 - before;
        let before = latest.arrivals().0;
        for i in 0..60 {
            let a = i as f64 / 60.0 * std::f64::consts::TAU;
            let _ = unsafe { SetCursorPos(cx + (80.0 * a.cos()) as i32, cy + (80.0 * a.sin()) as i32) };
            thread::sleep(Duration::from_millis(16));
        }
        let moving = latest.arrivals().0 - before;
        let _ = unsafe { SetCursorPos(was.x, was.y) };
        drop(capture);
        eprintln!("pictures in a second at 30 fps: {still} with the cursor still, {moving} with it moving");
        assert!(moving >= 15, "the cursor moving brought {moving} pictures");
    }
}
