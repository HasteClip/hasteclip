//! Frame blending for another program to show: the OBS plugin
//! (`hasteclip-obs`). A game's, window's or display's pictures are taken as
//! a recording takes them (a game through the hook OBS's Game Capture put in
//! it, [`super::hook::HookCapture::join`]; a window or display through
//! Windows' capture) and blended as a recording's are ([`super::FrameBlend`],
//! [`super::Source`]), but on the other program's frame clock, into textures
//! it opens by their handles.
//!
//! The frames are OBS's: frame `k`'s own time starts at `t0 + k * period`,
//! `t0` being one of OBS's frames' times and `period` its frame interval, so
//! frame `k` blends the looks in OBS's frame `k`'s time. A frame is done a
//! little after its time is over (its last look, then a wait for pictures
//! noticed late), so OBS shows each a set number of frames after its own
//! ([`Feed::frame_for`]): the same number every frame, so motion stays even.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex, IDXGIResource1};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::{Interface, PCWSTR};

use super::system::host_now;
use super::{BLEND_LATE, FrameBlend, Lateness, MAX_BEHIND, Source, d3d, quad};
use crate::BlendShape;

/// What a feed takes its pictures from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A game another program's hook captures (OBS's Game Capture), by the
    /// game's window.
    HookedGame { window: isize },
    /// A window's client area, by Windows' capture.
    Window { window: isize },
    /// A display, by Windows' capture.
    Display { monitor: isize },
}

/// How a feed blends, and on whose clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// One of the frames' times, in host seconds (the clock OBS's
    /// `os_gettime_ns` counts: `QueryPerformanceCounter`).
    pub t0: f64,
    /// The time between frames, in nanoseconds (OBS's frame interval).
    pub period_ns: u64,
    /// Looks a frame (one: not blended, each frame the picture up at its
    /// time), and how a frame weighs them.
    pub looks: u32,
    pub shape: BlendShape,
    /// Draw the cursor on a hooked game's pictures (the hook leaves it out).
    pub cursor: bool,
    /// The graphics card the frames are shared on (its LUID): the other
    /// program's. None: the default one.
    pub adapter: Option<u64>,
    /// The biggest picture to make room for (the source's size, as the
    /// other program has it); a bigger one is shrunk to fit. None: room for
    /// any window or display, as a recording has (several hundred MB of
    /// graphics memory at 1440p).
    pub size: Option<(u32, u32)>,
}

/// A finished frame, shared: an NT handle to a texture with a keyed mutex,
/// acquired and released with key 0 around reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub handle: isize,
    /// Which frame it is (see the module's doc).
    pub index: i64,
    /// Its picture's size, from the texture's top-left corner.
    pub width: u32,
    pub height: u32,
}

/// How a feed is doing, for a status line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// Why it stopped; none while it runs.
    pub ended: Option<String>,
    /// Frames made so far.
    pub frames: u64,
    /// New pictures the source brought in the last whole second.
    pub pictures: u32,
    /// Looks a frame, and the different pictures a frame's own looks saw in
    /// the last whole second: on average, and the fewest.
    pub looks: u32,
    pub different: f32,
    pub fewest: u32,
    /// Frames shown this many frames after their own time.
    pub delay: i64,
}

/// Frames kept for the other program to show: the one it shows, the next
/// few being made, and room either side.
const RING: usize = 6;

/// The longest a feed waits for pictures noticed late (seconds), at most:
/// less than a frame, so the frames can be shown the same number of frames
/// late every time (a recording waits up to [`super::BLEND_LATE_MAX`], as
/// its frames' times in the file don't change with when they're made). A
/// picture noticed later counts toward the look after its own.
const FEED_LATE_MAX: f64 = 0.02;

/// A running feed: its pacer's thread, and the frames it's made.
pub struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    t0: f64,
    period: f64,
}

/// What a feed's pacer and its owner share.
struct Shared {
    /// The newest frame in each of the ring's textures.
    frames: Mutex<Vec<Frame>>,
    status: Mutex<Status>,
    /// Blending to change to, taken up at the next frame.
    blend: Mutex<Option<(u32, BlendShape)>>,
    /// Frames are shown this many frames after their own time.
    delay: std::sync::atomic::AtomicI64,
}

impl Feed {
    /// Start taking `target`'s pictures and blending them into frames. It
    /// opens on its own thread: what went wrong is told by [`Feed::status`].
    pub fn start(target: Target, options: Options) -> Feed {
        let period = options.period_ns.max(1) as f64 / 1e9;
        let shared = Arc::new(Shared {
            frames: Mutex::new(Vec::new()),
            status: Mutex::new(Status { looks: options.looks.max(1), ..Status::default() }),
            blend: Mutex::new(None),
            delay: std::sync::atomic::AtomicI64::new(delay_frames(options.looks, options.shape, period)),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, shared2) = (stop.clone(), shared.clone());
        let thread = thread::Builder::new()
            .name("blend feed".into())
            .spawn(move || {
                super::system::multimedia_thread("Capture");
                let result = run(target, options, &shared2, &stop2);
                let why = match result {
                    Ok(()) => "stopped".to_owned(),
                    Err(e) => format!("{e:#}"),
                };
                shared2.status.lock().unwrap().ended = Some(why);
            })
            .ok();
        if thread.is_none() {
            shared.status.lock().unwrap().ended = Some("can't start its thread".to_owned());
        }
        Feed { stop, thread, shared, t0: options.t0, period }
    }

    /// The frame to show at `t` (host seconds; one of the frames' times):
    /// the one made the set number of frames before, or the newest made
    /// before it if that one isn't ready (shown twice). None before the
    /// first.
    pub fn frame_for(&self, t: f64) -> Option<Frame> {
        let n = ((t - self.t0) / self.period).round() as i64;
        let want = n - self.shared.delay.load(Ordering::Relaxed);
        pick(&self.shared.frames.lock().unwrap(), want)
    }

    /// The newest frame made before frame `index`: shown instead when that
    /// one's texture is still being written.
    pub fn frame_before(&self, index: i64) -> Option<Frame> {
        pick(&self.shared.frames.lock().unwrap(), index - 1)
    }

    /// Blend `looks` a frame from the next frame on, weighed by `shape`.
    pub fn set_blend(&self, looks: u32, shape: BlendShape) {
        *self.shared.blend.lock().unwrap() = Some((looks.max(1), shape));
    }

    pub fn status(&self) -> Status {
        let mut s = self.shared.status.lock().unwrap().clone();
        s.delay = self.shared.delay.load(Ordering::Relaxed);
        s
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How many frames after its own time a frame is shown: once its last look
/// is taken (at the end of its own time, blended evenly; half a frame on,
/// softly) and the pictures noticed late are in, at the frame shown next.
fn delay_frames(looks: u32, shape: BlendShape, period: f64) -> i64 {
    let done = if shape == BlendShape::Soft && looks > 1 { 1.5 } else { 1.0 };
    (done + wait_most(period) / period).ceil() as i64
}

/// The longest a feed waits for late pictures, at frames `period` apart:
/// within [`FEED_LATE_MAX`] and under a frame, but never under what a
/// recording waits at least (at 240 fps, three frames).
fn wait_most(period: f64) -> f64 {
    (0.8 * period).clamp(BLEND_LATE, FEED_LATE_MAX)
}

/// The newest of `frames` at or before frame `want`.
fn pick(frames: &[Frame], want: i64) -> Option<Frame> {
    frames.iter().filter(|f| f.index <= want).max_by_key(|f| f.index).copied()
}

/// What the feed's pictures come from.
enum Capture {
    Hook(super::hook::HookCapture),
    Wgc(d3d::Capture),
}

impl Capture {
    /// Over (the game or window closed, the hook failed): why.
    fn ended(&self) -> Option<String> {
        match self {
            Capture::Hook(h) => h.ended(),
            Capture::Wgc(c) => c.is_closed().then(|| "it closed".to_owned()),
        }
    }
}

fn run(target: Target, options: Options, shared: &Arc<Shared>, stop: &AtomicBool) -> Result<()> {
    super::system::com_init();
    let gpu = match options.adapter {
        Some(luid) => d3d::Gpu::on_adapter(luid)?,
        None => d3d::Gpu::new()?,
    };
    let period = options.period_ns.max(1) as f64 / 1e9;
    let looks = options.looks.max(1);
    // The capture is asked for pictures as often as the looks need them.
    let looks_per_second = ((looks as f64 / period).round() as u32).max(1);
    // Room for the source's pictures; or for a window on any display, or
    // any display, as a recording has.
    let (width, height) = options.size.map(|(w, h)| (w.max(2), h.max(2))).unwrap_or_else(super::system::largest_display_box);
    let latest = d3d::Latest::new(&gpu, width, height)?;
    let (capture, name) = match target {
        Target::HookedGame { window } => {
            let hwnd = HWND(window as *mut _);
            (Capture::Hook(super::hook::HookCapture::join(&gpu, hwnd, &latest, looks_per_second, options.cursor)?), "the game")
        }
        Target::Window { window } => {
            let hwnd = HWND(window as *mut _);
            let item = d3d::window_item(hwnd)?;
            (Capture::Wgc(d3d::Capture::start(&gpu, &item, &latest, d3d::Of::Window(hwnd), looks_per_second)?), "the window")
        }
        Target::Display { monitor } => {
            let monitor = HMONITOR(monitor as *mut _);
            let item = d3d::monitor_item(monitor)?;
            (Capture::Wgc(d3d::Capture::start(&gpu, &item, &latest, d3d::Of::Display(monitor), looks_per_second)?), "the display")
        }
    };
    latest.set_wants_queue(true);
    let bell = Arc::new(d3d::Bell::default());
    latest.set_bell(Some(bell.clone()));
    let blend = FrameBlend::new(&gpu, &latest, looks)?;
    let ring = Ring::new(&gpu, width, height)?;
    let quad = quad::Quad::new(&gpu)?;
    let shutter = crate::shutter::Shutter::new(looks, options.shape);
    // From the frame after the one under way now.
    let now_frame = ((host_now() - options.t0) / period).floor() as i64;
    let first = shutter.first_look(now_frame + 1);
    let mut p = Pacer {
        t0: options.t0,
        period,
        step: period / looks as f64,
        looks: looks as i64,
        shutter,
        closed: now_frame,
        source: Source::new(0, name.to_owned(), latest.clone(), blend, 0.0, first),
        quad,
        lateness: Lateness::default(),
        ring,
        shared: shared.clone(),
        second: Second::default(),
    };
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if let Some(why) = capture.ended() {
            break Err(anyhow::anyhow!("{name}: {why}"));
        }
        if let Some((looks, shape)) = shared.blend.lock().unwrap().take() {
            p.change_blend(looks, shape);
        }
        p.catch_up(host_now() - p.wait());
        // Rung by the capture once a picture is there.
        let rung = bell.rung();
        let wait = p.frame_end() + p.wait() - host_now();
        if wait > 0.0 && latest.queue_len() == 0 {
            bell.wait(rung, Duration::from_secs_f64(wait.min(0.05)));
        }
        p.take_pictures();
        // Looks whose time is past (and late pictures' too) see what's up.
        p.look_until(host_now() - p.wait());
    };
    latest.set_bell(None);
    latest.set_wants_queue(false);
    drop(capture);
    result
}

/// The frames being made: [`super::BlendPacer`]'s timing for one source,
/// its frames on the other program's clock, each finished into the ring.
struct Pacer {
    t0: f64,
    period: f64,
    /// Seconds between looks, and looks a frame.
    step: f64,
    looks: i64,
    shutter: crate::shutter::Shutter,
    /// The last frame done (made or skipped).
    closed: i64,
    source: Source,
    quad: quad::Quad,
    lateness: Lateness,
    ring: Ring,
    shared: Arc<Shared>,
    second: Second,
}

/// One second's counts, for the status.
#[derive(Default)]
struct Second {
    since: f64,
    frames: u32,
    different: u32,
    fewest: u32,
}

impl Pacer {
    /// How long after their time looks are taken: long enough for the
    /// pictures that come in latest, with a little to spare, within
    /// [`wait_most`].
    fn wait(&self) -> f64 {
        (self.lateness.worst(host_now()) * 1.2 + 0.002).clamp(BLEND_LATE, wait_most(self.period))
    }

    /// When look `m` is.
    fn look_time(&self, m: i64) -> f64 {
        self.t0 + self.source.phase + (m as f64 + 0.5) * self.step
    }

    /// When the next frame to make is done: its last look.
    fn frame_end(&self) -> f64 {
        self.look_time(self.shutter.last_look(self.closed + 1))
    }

    /// Whether frames are made: once the capture's first picture is in.
    fn started(&self) -> bool {
        self.source.held.is_some() || self.source.latest.has_frame()
    }

    /// Fallen more than [`MAX_BEHIND`] behind `t`: the frames whose time is
    /// past are skipped, and it goes on with the frame `t` is in.
    fn catch_up(&mut self, t: f64) {
        if t - self.frame_end() < MAX_BEHIND {
            return;
        }
        let due = ((t - self.t0 - self.source.phase) / self.step - 0.5).ceil() as i64;
        let (frame, _) = self.shutter.split(due);
        self.closed = frame - 1;
        let s = &mut self.source;
        s.next = self.shutter.first_look(frame);
        s.different = 0;
        s.blend.drop_all();
    }

    /// Blend `looks` a frame from the next frame on, weighed by `shape`.
    fn change_blend(&mut self, looks: u32, shape: BlendShape) {
        let looks = looks.max(1);
        let shutter = crate::shutter::Shutter::new(looks, shape);
        if looks as i64 == self.looks && shutter == self.shutter {
            return;
        }
        self.source.blend.set_looks(looks);
        let k = self.closed + 1;
        self.looks = looks as i64;
        self.step = self.period / looks as f64;
        self.shutter = shutter;
        let first = self.shutter.first_look(k);
        let s = &mut self.source;
        (s.phase, s.timed, s.next, s.different) = (0.0, false, first, 0);
        self.shared.delay.store(delay_frames(looks, shape, self.period), Ordering::Relaxed);
        self.shared.status.lock().unwrap().looks = looks;
    }

    /// Take every look before `t`, then make the frames they finish.
    fn look_until(&mut self, t: f64) {
        let mut burst = 0;
        while self.look_time(self.source.next) < t && burst < 10_000 {
            self.look();
            burst += 1;
        }
        self.close_done();
    }

    /// The next look: the picture that's up, added to the frames it counts
    /// toward.
    fn look(&mut self) {
        let (looks, closed) = (self.looks, self.closed);
        let s = &mut self.source;
        let (first, weights) = self.shutter.split(s.next);
        if let Some(id) = s.held {
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
            if s.held.is_some() {
                let c = &mut self.second;
                c.fewest = if c.frames == 0 { different } else { c.fewest.min(different) };
                c.frames += 1;
                c.different += different;
            }
        }
    }

    /// The frames no later look counts toward are done: made.
    fn close_done(&mut self) {
        let (first, _) = self.shutter.split(self.source.next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: made, and put in the ring.
    fn close(&mut self, k: i64) {
        self.closed = k;
        if !self.started() {
            return;
        }
        self.source.finish(&self.quad, k);
        let average = &self.source.blend.average;
        if let Some(frame) = self.ring.put(&self.source.blend.gpu, k, average) {
            let mut frames = self.shared.frames.lock().unwrap();
            frames.retain(|f| f.handle != frame.handle);
            frames.push(frame);
        }
        let now = host_now();
        let mut status = self.shared.status.lock().unwrap();
        status.frames += 1;
        if now - self.second.since >= 1.0 {
            let c = std::mem::take(&mut self.second);
            status.pictures = self.source.rate.per_second(now);
            status.different = if c.frames == 0 { 0.0 } else { c.different as f32 / c.frames as f32 };
            status.fewest = c.fewest;
            self.second.since = now;
        }
    }

    /// The capture's pictures that came up since, each taken in as it came.
    fn take_pictures(&mut self) {
        let now = host_now();
        let mut last = f64::MIN;
        for q in self.source.latest.drain_queue() {
            last = last.max(q.at);
            self.arrive(last, Some(q));
        }
        // Put up some other way (a still): taken from the capture.
        let others = self.source.latest.others();
        if others != self.source.others {
            self.source.others = others;
            self.arrive(now.max(last), None);
        }
    }

    /// A new picture came up at `at`: the looks before it saw the one
    /// before. Then the looks move toward halfway between the pictures.
    fn arrive(&mut self, at: f64, picture: Option<d3d::Queued>) {
        let now = host_now();
        self.lateness.note(now, now - at);
        let before = self.source.next - 1;
        if at < self.look_time(before) {
            self.source.stats.late += 1;
        }
        let mut burst = 0;
        while self.look_time(self.source.next) < at && burst < 10_000 {
            self.look();
            burst += 1;
        }
        self.close_done();
        let (step, since) = (self.step, at - self.t0);
        let s = &mut self.source;
        let first = picture.is_some() && !s.timed;
        s.timed |= picture.is_some();
        s.phase = crate::shutter::follow_picture(s.phase, step, since, first);
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

/// The textures finished frames go into for the other program, shared by
/// NT handles, each with a keyed mutex (key 0) so neither side reads a
/// frame while the other writes it. Frame `k` goes into texture `k % RING`.
struct Ring {
    slots: Vec<Slot>,
}

struct Slot {
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: HANDLE,
}

impl Ring {
    fn new(gpu: &d3d::Gpu, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut slots = Vec::with_capacity(RING);
        for _ in 0..RING {
            let mut texture = None;
            unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.context("can't make a texture to share")?;
            let texture = texture.context("no texture to share")?;
            let resource: IDXGIResource1 = texture.cast()?;
            let handle = unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0, PCWSTR::null()) }.context("can't share a texture")?;
            let mutex: IDXGIKeyedMutex = texture.cast()?;
            slots.push(Slot { texture, mutex, handle });
        }
        Ok(Self { slots })
    }

    /// Put frame `k` (the picture in `from`) in its texture. None if the
    /// other program holds that texture still (the frame is skipped).
    fn put(&self, gpu: &d3d::Gpu, k: i64, from: &d3d::Latest) -> Option<Frame> {
        let slot = &self.slots[k.rem_euclid(RING as i64) as usize];
        // A timeout is a success code: told apart by its value.
        let got = unsafe { (Interface::vtable(&slot.mutex).AcquireSync)(Interface::as_raw(&slot.mutex), 0, 20) };
        const WAIT_ABANDONED: i32 = 0x80;
        if got.0 != 0 && got.0 != WAIT_ABANDONED {
            return None;
        }
        let (width, height) = from.content();
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        unsafe {
            gpu.context.CopySubresourceRegion(&slot.texture, 0, 0, 0, 0, &from.texture, 0, Some(&region));
            let _ = slot.mutex.ReleaseSync(0);
            gpu.context.Flush();
        }
        Some(Frame { handle: slot.handle.0 as isize, index: k, width, height })
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        for s in &self.slots {
            let _ = unsafe { CloseHandle(s.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(index: i64) -> Frame {
        Frame { handle: index as isize, index, width: 1, height: 1 }
    }

    /// The newest frame at or before the one wanted; an older one if it
    /// isn't made yet; none before the first.
    #[test]
    fn picks_the_newest_made() {
        let frames = [frame(10), frame(12), frame(11), frame(7)];
        assert_eq!(pick(&frames, 12).map(|f| f.index), Some(12));
        assert_eq!(pick(&frames, 14).map(|f| f.index), Some(12));
        assert_eq!(pick(&frames, 9).map(|f| f.index), Some(7));
        assert_eq!(pick(&frames, 6), None);
    }

    /// At 60 fps a frame blended evenly is shown two frames after its own
    /// time (done at its end, then the wait for late pictures), softly three
    /// (its looks run half a frame on); unblended, as blended evenly.
    #[test]
    fn delay_at_60_fps() {
        let period = 1.0 / 60.0;
        assert_eq!(delay_frames(5, BlendShape::Even, period), 2);
        assert_eq!(delay_frames(5, BlendShape::Soft, period), 3);
        assert_eq!(delay_frames(1, BlendShape::Soft, period), 2);
    }

    /// The wait for late pictures stays under a frame, and done plus the
    /// wait always fits the delay.
    #[test]
    fn the_wait_fits_the_delay() {
        for fps in [24.0, 30.0, 50.0, 59.94, 60.0, 120.0, 144.0, 240.0] {
            let period = 1.0 / fps;
            for (looks, shape, done) in [(4, BlendShape::Even, 1.0), (4, BlendShape::Soft, 1.5)] {
                let delay = delay_frames(looks, shape, period) as f64;
                assert!(done + wait_most(period) / period <= delay, "{fps} fps: {delay} frames");
                assert!(delay - (done + wait_most(period) / period) < 1.0, "{fps} fps: {delay} frames is more than needed");
            }
        }
    }
}
