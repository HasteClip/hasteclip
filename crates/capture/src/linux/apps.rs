//! Games and apps (`VideoSource::Apps`) on KDE Plasma: whichever of the
//! picked programs was last in focus, one window at a time, streamed by KWin
//! at the window's own size (`super::kwin`), found in KWin's window list
//! (`super::kwin_windows`).
//!
//! A thread follows the window list. When a window of a picked program comes
//! into focus, its stream replaces the one before; clicking into something
//! else keeps it. When it's minimized, the away screen shows instead (or its
//! last picture stays, without the away screen); when it closes, or while
//! none of them is open, the away screen (else black) shows.
//!
//! A program is known by its file name (`cs2`, or `Game.exe` for a Proton
//! game), as games are everywhere else (`super::focus::program_of`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::screen::{self, Frame, Order, Screen};
use super::{focus, kwin, kwin_windows};
use crate::{Device, StillImage};

/// Whether games and apps can be recorded here: KDE Plasma's Wayland
/// session, with its screencast and window list ours.
pub(crate) fn available() -> bool {
    kwin::available() && kwin_windows::following()
}

/// Running programs with a window in the taskbar (not ours), by program,
/// sorted by name.
pub(crate) fn list() -> Vec<Device> {
    let me = std::process::id();
    let mut programs = Programs::default();
    let mut out: Vec<Device> = Vec::new();
    for w in kwin_windows::windows().unwrap_or_default() {
        if w.skip_taskbar || w.pid.is_none_or(|p| p == me) {
            continue;
        }
        let Some(id) = programs.of(w.pid) else { continue };
        if out.iter().any(|d| d.id.eq_ignore_ascii_case(&id)) {
            continue;
        }
        let name = desktop_name(&w.app_id).or_else(|| Some(w.title.trim().to_owned()).filter(|t| !t.is_empty())).unwrap_or_else(|| id.clone());
        out.push(Device { id, name });
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// The name in an app's `.desktop` file (`Firefox` for `org.mozilla.firefox`).
fn desktop_name(app_id: &str) -> Option<String> {
    if app_id.is_empty() || app_id.contains('/') {
        return None;
    }
    let home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let file = format!("{app_id}.desktop");
    home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).find_map(|dir| {
        let text = std::fs::read_to_string(dir.join("applications").join(&file)).ok()?;
        let entry = text.split("\n[").next()?;
        entry.lines().find_map(|l| l.strip_prefix("Name=")).map(|n| n.trim().to_owned()).filter(|n| !n.is_empty())
    })
}

/// Which program each process runs, remembered (reading `/proc` for every
/// window every few milliseconds would add up).
#[derive(Default)]
struct Programs(HashMap<u32, Option<String>>);

impl Programs {
    fn of(&mut self, pid: Option<u32>) -> Option<String> {
        let pid = pid?;
        self.0
            .entry(pid)
            .or_insert_with(|| focus::program_of(pid).and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned())))
            .clone()
    }
}

/// How often the window list is looked at.
const FOLLOW_EVERY: Duration = Duration::from_millis(50);

/// After a window's stream fails to open, how long until it's tried again.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Games and apps being followed: the picture to record now, whichever
/// window it's from.
pub(crate) struct Apps {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    /// The programs to record.
    ids: Mutex<Vec<String>>,
    /// Show the away screen while the window is minimized (else its last picture).
    away_when_unfocused: AtomicBool,
    /// Shown while none of them is: the away screen, or black.
    away: Arc<Frame>,
    now: Mutex<Now>,
    /// Counts changes of what's shown, so a new window's (or the away
    /// screen's) pictures read as new.
    generation: AtomicU64,
}

/// What's shown now.
#[derive(Default)]
struct Now {
    /// The window streamed: its uuid, its program and the stream.
    window: Option<(String, String, Arc<Screen>)>,
    /// It's minimized: nothing of it shows.
    minimized: bool,
}

impl Apps {
    /// Start following `ids`' windows. Returns at once: the away screen
    /// shows until one of them is found.
    pub(crate) fn start(ids: &[String], away_when_unfocused: bool, away_screen: Option<&StillImage>) -> Arc<Self> {
        let shared = Arc::new(Shared {
            ids: Mutex::new(ids.to_vec()),
            away_when_unfocused: AtomicBool::new(away_when_unfocused),
            away: Arc::new(away_screen.map_or_else(black, still_frame)),
            now: Mutex::new(Now::default()),
            generation: AtomicU64::new(1),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (shared2, stop2) = (shared.clone(), stop.clone());
        let thread = thread::Builder::new().name("apps".into()).spawn(move || follow(&shared2, &stop2)).ok();
        // The window in focus now is picked up before the first picture's asked for.
        thread::sleep(FOLLOW_EVERY * 2);
        Arc::new(Self { shared, stop, thread: Mutex::new(thread) })
    }

    /// Record these programs instead, without restarting.
    pub(crate) fn set(&self, ids: &[String], away_when_unfocused: bool) {
        *self.shared.ids.lock().unwrap() = ids.to_vec();
        self.shared.away_when_unfocused.store(away_when_unfocused, Ordering::Relaxed);
    }

    /// The picture to record now and a number that changes whenever it does.
    pub(crate) fn get(&self) -> Option<(Arc<Frame>, u64)> {
        let now = self.shared.now.lock().unwrap();
        let generation = self.shared.generation.load(Ordering::Acquire) << 40;
        let away = (generation, self.shared.away.clone());
        match &now.window {
            Some((_, _, screen)) if !(now.minimized && self.shared.away_when_unfocused.load(Ordering::Relaxed)) => {
                Some(screen.latest.get().map_or((away.1, away.0), |(f, seq)| (f, generation | seq)))
            }
            _ => Some((away.1, away.0)),
        }
    }

    /// Whether none of them shows (the away screen, or a minimized one's last picture).
    pub(crate) fn waiting(&self) -> bool {
        let now = self.shared.now.lock().unwrap();
        now.window.is_none() || now.minimized
    }

    /// The program whose window is recorded, while it shows.
    pub(crate) fn app(&self) -> Option<String> {
        let now = self.shared.now.lock().unwrap();
        now.window.as_ref().filter(|_| !now.minimized).map(|(_, id, _)| id.clone())
    }
}

impl Drop for Apps {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Follow the window list until told to stop.
fn follow(shared: &Shared, stop: &AtomicBool) {
    let mut programs = Programs::default();
    let mut failed: HashMap<String, Instant> = HashMap::new();
    let me = std::process::id();
    while !stop.load(Ordering::Relaxed) {
        let windows = kwin_windows::windows().unwrap_or_default();
        let ids = shared.ids.lock().unwrap().clone();
        let mut program = |w: &kwin_windows::Window| programs.of(w.pid).filter(|p| ids.iter().any(|i| i.eq_ignore_ascii_case(p)));
        let current = shared.now.lock().unwrap().window.as_ref().map(|(uuid, _, s)| (uuid.clone(), s.latest.ended().is_some()));
        // Its window, if it's still there (and its stream still runs) and still picked.
        let kept = current.as_ref().and_then(|(uuid, ended)| windows.iter().find(|w| &w.uuid == uuid && !ended)).filter(|w| program(w).is_some());
        // One of theirs in focus takes over.
        let focused = windows.iter().find(|w| w.active && !w.skip_taskbar && w.pid != Some(me)).and_then(|w| Some((w, program(w)?)));
        let minimized = kept.is_some_and(|w| w.minimized);
        match focused {
            Some((w, id)) if current.as_ref().is_none_or(|(uuid, _)| uuid != &w.uuid) && failed.get(&w.uuid).is_none_or(|t| t.elapsed() > RETRY_AFTER) => {
                match screen::open_kwin_window(&w.uuid) {
                    Ok(s) => {
                        failed.remove(&w.uuid);
                        let old = std::mem::replace(&mut *shared.now.lock().unwrap(), Now { window: Some((w.uuid.clone(), id, Arc::new(s))), minimized: false });
                        shared.generation.fetch_add(1, Ordering::Release);
                        drop(old);
                    }
                    Err(e) => {
                        eprintln!("couldn't record {id}'s window: {e:#}");
                        failed.insert(w.uuid.clone(), Instant::now());
                    }
                }
            }
            _ if current.is_some() && kept.is_none() => {
                // Closed (or no longer picked): the away screen.
                let old = std::mem::take(&mut *shared.now.lock().unwrap());
                shared.generation.fetch_add(1, Ordering::Release);
                drop(old);
            }
            _ => {
                let mut now = shared.now.lock().unwrap();
                if now.minimized != minimized {
                    now.minimized = minimized;
                    shared.generation.fetch_add(1, Ordering::Release);
                }
            }
        }
        thread::sleep(FOLLOW_EVERY);
    }
}

/// A picture from a still image.
fn still_frame(img: &StillImage) -> Frame {
    Frame { width: img.width, height: img.height, stride: img.width as usize * 4, order: Order::Bgr, data: img.bgra.clone(), texture: None }
}

/// A small black picture: scaled to the frame, all black.
fn black() -> Frame {
    let (width, height) = (16, 9);
    let data = [0, 0, 0, 255].repeat(width * height);
    Frame { width: width as u32, height: height as u32, stride: width * 4, order: Order::Bgr, data, texture: None }
}
