//! Demo recordings: the real app, driven by a script and recorded to a video,
//! for the website. Only on when `HASTECLIP_DEMO_SCRIPT=<script.json>` is set
//! (with `HASTECLIP_LIBRARY` pointing at a copy of a library, so nothing real
//! changes); otherwise every call here is one cheap check.
//!
//! The script moves a drawn pointer, clicks, drags and presses keys, fed to
//! egui as input, so the app does exactly what it does for a person: hovers,
//! menus, animations. It aims at named spots (`mark`): buttons and menu items
//! by their label, clip cards by their title, the library's filters by name,
//! and a few controls (play, the trim handles) by a fixed name. Each frame the
//! app draws is read back from the GPU and piped to ffmpeg, so the window
//! needn't be in front.
//!
//! The script is a JSON list of steps (see `Step`):
//!
//! ```json
//! { "out": "demo.mp4", "steps": [
//!   { "wait": 0.8 },
//!   { "move": "clip:Ace on Ancient", "secs": 0.9 },
//!   { "click": true },
//!   { "seek": "marker:1", "before": 1.0 },
//!   { "key": "space" },
//!   { "drag": "trim:in", "by": [120, 0], "secs": 0.9 },
//!   { "wait": 2 }
//! ] }
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc;

use egui::{Color32, Context, Event, Id, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};
use serde::Deserialize;

/// The script, when `HASTECLIP_DEMO_SCRIPT` names one.
fn script_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("HASTECLIP_DEMO_SCRIPT").map(PathBuf::from)).as_ref()
}

/// Whether a demo is being recorded.
pub fn on() -> bool {
    script_path().is_some()
}

/// Name a spot on screen, this frame, for the script to aim at. Free when no
/// demo is running.
pub fn mark(ctx: &Context, name: impl FnOnce() -> String, rect: Rect) {
    if !on() {
        return;
    }
    let name = name();
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Marks>(marks_id()).now.insert(name, rect));
}

/// The open clip's timeline, as the ruler draws it: where it is, the span of
/// time it shows (start, length) and the markers' times, so `seek:
/// "marker:N"` can click exactly where a marker is.
pub fn timeline(ctx: &Context, rect: Rect, view: (f64, f64), markers: impl Iterator<Item = f64>) {
    if !on() {
        return;
    }
    let mut m: Vec<f64> = markers.collect();
    m.sort_by(f64::total_cmp);
    ctx.data_mut(|d| d.insert_temp(markers_id(), Timeline { rect, view, markers: m }));
}

#[derive(Clone)]
struct Timeline {
    rect: Rect,
    view: (f64, f64),
    markers: Vec<f64>,
}

/// Whether the open clip is playing, as its play button shows: so `play` can
/// press it only when it needs to.
pub fn playing(ctx: &Context, playing: bool) {
    if on() {
        ctx.data_mut(|d| d.insert_temp(playing_id(), playing));
    }
}

fn playing_id() -> Id {
    Id::new("demo_playing")
}
fn marks_id() -> Id {
    Id::new("demo_marks")
}
fn markers_id() -> Id {
    Id::new("demo_markers")
}

/// This frame's marks, and last frame's (what the script aims with, since
/// this frame's are still being drawn).
#[derive(Clone, Default)]
struct Marks {
    now: HashMap<String, Rect>,
    last: HashMap<String, Rect>,
}

#[derive(Deserialize)]
struct Script {
    /// Where the video goes (relative to the script).
    out: PathBuf,
    /// Frames a second.
    #[serde(default = "default_fps")]
    fps: u32,
    steps: Vec<Step>,
}

fn default_fps() -> u32 {
    60
}

/// One thing the script does. Targets: `"clip:<title>"`, `"filter:<name>"`,
/// `"button:<label>"` (buttons and menu items), `"play"`, `"trim:in"`,
/// `"trim:out"`, `"timeline"`, or a point `[x, y]` in the window's points.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Step {
    /// Hold still.
    Wait { wait: f64 },
    /// Glide the pointer onto a target.
    Move {
        #[serde(rename = "move")]
        to: Target,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Click where the pointer is (twice for a double click).
    Click {
        #[allow(dead_code)]
        click: bool,
        #[serde(default)]
        double: bool,
    },
    /// Press the button on a target (or where the pointer is), move by `by`
    /// points or onto `to`, let go.
    Drag {
        drag: Target,
        #[serde(default)]
        by: Option<[f32; 2]>,
        #[serde(default)]
        to: Option<Target>,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Press a key, with modifiers: `"space"`, `"cmd+s"`, `"alt+f10"`, `"e"`.
    Key { key: String },
    /// Type text into whatever has focus.
    Type {
        #[serde(rename = "type")]
        text: String,
    },
    /// Scroll by points (positive goes down the page).
    Scroll {
        scroll: f32,
        #[serde(default = "default_move")]
        secs: f64,
    },
    /// Move the open clip's playhead to a marker (`"marker:1"` is the first),
    /// `before` seconds ahead of it, by clicking the timeline there.
    Seek {
        seek: String,
        #[serde(default)]
        before: f64,
    },
    /// Make the open clip play (`true`) or pause (`false`): moves to its play
    /// button and clicks it, unless it's already that way.
    Play { play: bool },
    /// Show or hide the pointer.
    Pointer { pointer: bool },
}

fn default_move() -> f64 {
    0.7
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Target {
    Named(String),
    Point([f32; 2]),
}

/// A running demo: the script's place, the pointer, the recorder.
pub struct Demo {
    steps: Vec<Step>,
    step: usize,
    /// When the current step started (egui time), and where the pointer was.
    started: Option<(f64, Pos2)>,
    pointer: Pos2,
    shown: bool,
    down: bool,
    /// Frames waiting to be asked for or written.
    recorder: Option<Recorder>,
    /// The first frame's time: the video's zero.
    zero: Option<f64>,
    done: bool,
    /// Waiting for a target that isn't drawn yet, since when.
    waiting: Option<f64>,
    /// A seek step's target point, once worked out.
    seek_at: Option<Pos2>,
    /// How far into a click: press, release, (press, release).
    click_frame: u8,
    /// Before recording starts: the last frame's time, and since when frames
    /// have come on time.
    last_frame: Option<f64>,
    steady_since: Option<f64>,
}

/// Seconds of steady frames before the recording starts.
const WARM_UP: f64 = 1.0;

impl Demo {
    /// The demo the environment asks for, if any. Exits with a message if the
    /// script can't be read: a demo run has nothing better to do.
    pub fn from_env(ctx: &Context) -> Option<Self> {
        let path = script_path()?;
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("can't read {}: {e}", path.display())));
        let script: Script = serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
        let out = path.parent().unwrap_or(std::path::Path::new(".")).join(&script.out);
        eprintln!("demo: {} steps → {}", script.steps.len(), out.display());
        ctx.request_repaint();
        Some(Self {
            steps: script.steps,
            step: 0,
            started: None,
            pointer: pos2(600.0, 420.0),
            shown: false,
            down: false,
            recorder: Some(Recorder::new(out, script.fps)),
            zero: None,
            done: false,
            waiting: None,
            seek_at: None,
            click_frame: 0,
            last_frame: None,
            steady_since: None,
        })
    }

    /// Before each frame: drop the real mouse and keyboard (a stray hand
    /// mustn't steer the demo), then add this frame's scripted input. Also
    /// hands the recorder the frame the GPU sent back.
    pub fn input(&mut self, ctx: &Context, raw: &mut egui::RawInput) {
        // The GPU's copies of earlier frames arrive as events.
        let mut frames = Vec::new();
        raw.events.retain(|e| match e {
            Event::Screenshot { image, .. } => {
                frames.push(image.clone());
                false
            }
            Event::PointerMoved(_) | Event::PointerButton { .. } | Event::MouseWheel { .. } | Event::Key { .. } | Event::Text(_) | Event::PointerGone => false,
            _ => true,
        });
        let now = raw.time.unwrap_or(0.0);
        if let Some(rec) = &mut self.recorder {
            for f in frames {
                rec.frame(f, now - self.zero.unwrap_or(now));
            }
        }
        if self.done {
            return;
        }
        // Start once the app has settled (the library loaded, thumbnails in):
        // WARM_UP seconds of frames each on time.
        if self.zero.is_none() {
            let steady = self.last_frame.is_some_and(|l| now - l < 0.025);
            self.last_frame = Some(now);
            self.steady_since = if steady { self.steady_since.or(Some(now)) } else { None };
            if self.steady_since.is_none_or(|s| now - s < WARM_UP) {
                return;
            }
            self.zero = Some(now);
        }
        // The step's events go after the pointer's move to where it was, then
        // one to where the step took it: a click lands where the pointer is.
        raw.events.push(Event::PointerMoved(self.pointer));
        let at = raw.events.len();
        self.step_script(ctx, raw, now);
        if raw.events.len() == at || !matches!(raw.events.last(), Some(Event::PointerMoved(_))) {
            raw.events.push(Event::PointerMoved(self.pointer));
        }
    }

    fn step_script(&mut self, ctx: &Context, raw: &mut egui::RawInput, now: f64) {
        let marks = ctx.data(|d| d.get_temp::<Marks>(marks_id())).unwrap_or_default().last;
        let Some(step) = self.steps.get(self.step).cloned() else {
            // Done: a moment for the last frames to come back, then quit.
            if self.started.is_none() {
                self.started = Some((now, self.pointer));
            }
            if now - self.started.map_or(now, |s| s.0) > 0.3 {
                self.done = true;
                if let Some(rec) = self.recorder.take() {
                    rec.finish();
                }
                eprintln!("demo: done");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        };
        let (start, from) = *self.started.get_or_insert((now, self.pointer));
        let t = now - start;
        let next = |s: &mut Self| {
            s.step += 1;
            s.started = None;
            s.waiting = None;
            s.seek_at = None;
        };
        let resolve = |s: &mut Self, target: &Target| -> Option<Pos2> {
            match target {
                Target::Point([x, y]) => Some(pos2(*x, *y)),
                Target::Named(name) => match marks.get(name) {
                    Some(r) => Some(r.center()),
                    None => {
                        let since = *s.waiting.get_or_insert(now);
                        if now - since > 5.0 {
                            let mut known: Vec<&String> = marks.keys().collect();
                            known.sort();
                            fail(&format!("nothing on screen is called \"{name}\". On screen now: {known:?}"));
                        }
                        None
                    }
                },
            }
        };
        let modifiers = Modifiers::NONE;
        match step {
            Step::Wait { wait } => {
                if t >= wait {
                    next(self);
                }
            }
            Step::Pointer { pointer } => {
                self.shown = pointer;
                next(self);
            }
            Step::Play { play } => {
                let playing = ctx.data(|d| d.get_temp::<bool>(playing_id()));
                // Already so (and not mid-press): nothing to do.
                if playing == Some(play) && !self.down && self.click_frame == 0 {
                    next(self);
                    return;
                }
                let Some(to) = resolve(self, &Target::Named("play".into())) else {
                    self.started = None;
                    return;
                };
                let secs = 0.6;
                self.shown = true;
                if t < secs {
                    self.pointer = glide(from, to, t, secs);
                    return;
                }
                // Press, then release a frame later.
                let pressed = self.click_frame == 0;
                raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed, modifiers });
                self.down = pressed;
                if pressed {
                    self.click_frame = 1;
                } else {
                    self.click_frame = 0;
                    next(self);
                }
            }
            Step::Move { to, secs } => {
                let Some(to) = resolve(self, &to) else {
                    self.started = None;
                    return;
                };
                self.shown = true;
                self.pointer = glide(from, to, t, secs);
                if t >= secs {
                    next(self);
                }
            }
            Step::Click { double, .. } => {
                // Press and release on frames of their own, like a hand: a
                // press, a frame held, a release (twice for a double click).
                let frames = if double { 4 } else { 2 };
                let n = self.click_frame;
                if n < frames {
                    let pressed = n.is_multiple_of(2);
                    raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed, modifiers });
                    self.down = pressed;
                    self.click_frame += 1;
                } else {
                    self.click_frame = 0;
                    next(self);
                }
            }
            Step::Drag { drag, by, to, secs } => {
                let Some(grab) = resolve(self, &drag) else {
                    self.started = None;
                    return;
                };
                // First glide onto what's grabbed, then hold and pull.
                let reach = if from.distance(grab) > 1.0 { 0.5 } else { 0.0 };
                if t < reach {
                    self.shown = true;
                    self.pointer = glide(from, grab, t, reach);
                    return;
                }
                if !self.down {
                    self.pointer = grab;
                    raw.events.push(Event::PointerButton { pos: grab, button: PointerButton::Primary, pressed: true, modifiers });
                    self.down = true;
                    return;
                }
                let end = match (&to, by) {
                    (Some(target), _) => match resolve(self, target) {
                        Some(p) => p,
                        None => return,
                    },
                    (None, Some([dx, dy])) => grab + vec2(dx, dy),
                    (None, None) => grab,
                };
                let pull = t - reach;
                self.pointer = glide(grab, end, pull, secs);
                if pull >= secs + 0.05 {
                    raw.events.push(Event::PointerButton { pos: self.pointer, button: PointerButton::Primary, pressed: false, modifiers });
                    self.down = false;
                    next(self);
                }
            }
            Step::Key { key } => {
                let (key, mods) = parse_key(&key);
                for pressed in [true, false] {
                    raw.events.push(Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: mods });
                }
                next(self);
            }
            Step::Type { text } => {
                raw.events.push(Event::Text(text));
                next(self);
            }
            Step::Scroll { scroll, secs } => {
                // Spread over the step's frames, eased, like a trackpad.
                let dt = f64::from(raw.predicted_dt);
                let a = ease(((t - dt) / secs).clamp(0.0, 1.0));
                let b = ease((t / secs).clamp(0.0, 1.0));
                let delta = (b - a) as f32 * scroll;
                raw.events.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -delta), phase: egui::TouchPhase::Move, modifiers });
                if t >= secs {
                    next(self);
                }
            }
            Step::Seek { seek, before } => {
                if self.seek_at.is_none() {
                    let n: usize = seek.strip_prefix("marker:").and_then(|n| n.parse().ok()).unwrap_or_else(|| fail(&format!("seek wants \"marker:N\", not \"{seek}\"")));
                    let Some(tl) = ctx.data(|d| d.get_temp::<Timeline>(markers_id())) else { return };
                    let Some(&at) = tl.markers.get(n.saturating_sub(1)) else {
                        fail(&format!("the open clip has {} markers, not {n} (put them in with B in the player)", tl.markers.len()))
                    };
                    let (v0, span) = tl.view;
                    let f = ((at - before - v0) / span.max(0.001)).clamp(0.0, 1.0) as f32;
                    self.seek_at = Some(pos2(tl.rect.left() + tl.rect.width() * f, tl.rect.center().y));
                }
                let to = self.seek_at.unwrap_or(self.pointer);
                let secs = 0.6;
                self.shown = true;
                self.pointer = glide(from, to, t, secs);
                if t >= secs {
                    // Down this frame, up the next.
                    let pressed = !self.down;
                    raw.events.push(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed, modifiers });
                    self.down = pressed;
                    if !pressed {
                        next(self);
                    }
                }
            }
        }
    }

    /// After the app has drawn: the pointer on top, last frame's marks kept
    /// for aiming, and the frame asked for.
    pub fn after(&mut self, ctx: &Context) {
        ctx.data_mut(|d| {
            let m = d.get_temp_mut_or_default::<Marks>(marks_id());
            m.last = std::mem::take(&mut m.now);
        });
        if self.shown {
            draw_pointer(ctx, self.pointer, self.down);
        }
        if self.recorder.is_some() && self.zero.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        // Every frame, for an even video.
        ctx.request_repaint();
    }
}

fn ease(t: f64) -> f64 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn glide(from: Pos2, to: Pos2, t: f64, secs: f64) -> Pos2 {
    let k = if secs <= 0.0 { 1.0 } else { ease((t / secs).clamp(0.0, 1.0)) } as f32;
    from + (to - from) * k
}

fn parse_key(s: &str) -> (Key, Modifiers) {
    let mut mods = Modifiers::NONE;
    let mut key = None;
    for part in s.to_lowercase().split('+') {
        match part {
            "cmd" | "command" => mods |= Modifiers::COMMAND,
            "ctrl" => mods |= Modifiers::CTRL,
            "alt" | "option" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            k => key = Key::from_name(k).or_else(|| Key::from_name(&capitalised(k))),
        }
    }
    (key.unwrap_or_else(|| fail(&format!("not a key: \"{s}\""))), mods)
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The macOS arrow, drawn: white with a dark edge, pressed in a little while
/// the button is down.
fn draw_pointer(ctx: &Context, at: Pos2, down: bool) {
    let layer = egui::LayerId::new(egui::Order::Debug, Id::new("demo_pointer"));
    let painter = ctx.layer_painter(layer);
    let s = if down { 0.92 } else { 1.0 };
    let shape: Vec<Pos2> = [(0.0, 0.0), (0.0, 17.0), (4.6, 12.9), (7.5, 19.5), (10.2, 18.3), (7.3, 11.8), (13.0, 11.8)]
        .iter()
        .map(|&(x, y)| at + vec2(x, y) * s)
        .collect();
    let shadow: Vec<Pos2> = shape.iter().map(|p| *p + vec2(0.0, 1.2)).collect();
    painter.add(egui::Shape::convex_polygon(shadow, Color32::from_black_alpha(70), egui::Stroke::NONE));
    // Not convex (the tail): drawn as two convex halves under one outline.
    painter.add(egui::Shape::convex_polygon(vec![shape[0], shape[1], shape[2], shape[5], shape[6]], Color32::WHITE, egui::Stroke::NONE));
    painter.add(egui::Shape::convex_polygon(vec![shape[2], shape[3], shape[4], shape[5]], Color32::WHITE, egui::Stroke::NONE));
    let mut outline = shape.clone();
    outline.push(shape[0]);
    painter.add(egui::Shape::line(outline, egui::Stroke::new(1.1, Color32::from_gray(20))));
}

fn fail(msg: &str) -> ! {
    eprintln!("demo: {msg}");
    std::process::exit(2)
}

/// Frames to ffmpeg, on their own thread: raw RGBA in, H.264 out. Each frame
/// is written once per 1/fps it covers, so a slow frame shows as a held frame,
/// never as the video speeding up.
struct Recorder {
    tx: Option<mpsc::SyncSender<(std::sync::Arc<egui::ColorImage>, f64)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    fn new(out: PathBuf, fps: u32) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(std::sync::Arc<egui::ColorImage>, f64)>(8);
        let thread = std::thread::spawn(move || {
            let mut ffmpeg: Option<(std::process::Child, [usize; 2])> = None;
            let mut written = 0u64;
            let mut fresh = 0u64;
            let mut held: Option<std::sync::Arc<egui::ColorImage>> = None;
            for (image, at) in rx {
                let size = image.size;
                let (child, want) = ffmpeg.get_or_insert_with(|| {
                    eprintln!("demo: recording {}×{} at {fps} fps", size[0], size[1]);
                    let child = media::ffmpeg()
                        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
                        .arg(format!("{}x{}", size[0], size[1]))
                        .args(["-r", &fps.to_string(), "-i", "-", "-c:v", "libx264", "-preset", "fast", "-crf", "16", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
                        .arg(&out)
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                        .unwrap_or_else(|e| fail(&format!("can't start ffmpeg: {e}")));
                    (child, size)
                });
                if image.size != *want {
                    continue; // the window was resized mid-recording; keep the first size
                }
                let stdin = child.stdin.as_mut().expect("piped");
                // Fill the frames up to this one's time with the last picture.
                let due = (at * f64::from(fps)).round() as u64;
                while written < due {
                    let fill = held.as_ref().unwrap_or(&image);
                    if stdin.write_all(fill.as_raw()).is_err() {
                        return;
                    }
                    written += 1;
                }
                if stdin.write_all(image.as_raw()).is_err() {
                    return;
                }
                written += 1;
                fresh += 1;
                held = Some(image);
            }
            if let Some((mut child, _)) = ffmpeg {
                drop(child.stdin.take());
                let _ = child.wait();
                eprintln!("demo: {written} frames written, {fresh} of them new from the app ({} held over)", written - fresh);
            }
        });
        Self { tx: Some(tx), thread: Some(thread) }
    }

    fn frame(&mut self, image: std::sync::Arc<egui::ColorImage>, at: f64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((image, at));
        }
    }

    fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_read() {
        let s: Script = serde_json::from_str(
            r#"{ "out": "a.mp4", "steps": [
                { "wait": 0.5 }, { "move": "clip:Ace", "secs": 1 }, { "move": [10, 20] }, { "click": true },
                { "drag": "trim:in", "by": [120, 0] }, { "drag": "clip:A", "to": "filter:Best" },
                { "key": "cmd+s" }, { "type": "hi" }, { "scroll": 300 }, { "seek": "marker:1", "before": 1 }, { "pointer": false },
                { "play": true }
            ] }"#,
        )
        .unwrap();
        assert_eq!(s.steps.len(), 12);
        assert!(matches!(s.steps[11], Step::Play { play: true }));
        assert!(matches!(s.steps[0], Step::Wait { .. }));
        assert!(matches!(s.steps[3], Step::Click { .. }));
        assert!(matches!(s.steps[4], Step::Drag { .. }));
        assert!(matches!(s.steps[9], Step::Seek { .. }));
    }

    #[test]
    fn keys_parse() {
        assert_eq!(parse_key("cmd+s"), (Key::S, Modifiers::COMMAND));
        assert_eq!(parse_key("space"), (Key::Space, Modifiers::NONE));
        assert_eq!(parse_key("alt+f10").0, Key::F10);
    }
}
