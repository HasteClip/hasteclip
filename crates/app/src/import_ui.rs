//! The Import clips dialog (Settings → Clips → Import clips, or the
//! library's Import clips…): the clipping apps found here with their clips
//! (how many, how big, from when, and which of the library's folders they'd
//! go in), each with a switch and its folder to change; a folder of your
//! own; a `.hasteclip` file; and whether the clips are copied (the default)
//! or moved, with the space that takes. Importing runs as a transfer
//! (`transfer.rs`): a toast with how far, and Cancel.
//!
//! The apps and their clips are looked for in the background as the dialog
//! opens (`import_apps`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::App;
use crate::clips::human_bytes;
use crate::import_apps::{self, AppKind, CopyOnly, Found, Mode, Source};
use crate::theme;
use crate::ui_kit::{self, Btn, Dismissed as _, Row, Seg};

/// The dialog, while it's open.
pub struct ImportDialog {
    /// The apps found here and the library's drive, once looked at.
    found: Arc<OnceLock<Looked>>,
    /// Folders added by hand, and apps whose folder was changed (by their
    /// place in the list), each scanned on its own.
    changed: Vec<Entry>,
    /// The places switched off.
    off: HashSet<usize>,
    mode: Mode,
}

/// What was found as the dialog opened.
struct Looked {
    entries: Vec<Entry>,
    /// Bytes free on the library's drive (unknown: `None`).
    free: Option<u64>,
    /// That drive, as named to the user ("E:"), where the OS names drives.
    drive: Option<String>,
}

/// One place clips are imported from, and what's in it once looked.
#[derive(Clone)]
struct Entry {
    src: Source,
    /// Its place in the found list it replaces (a changed folder), if any.
    replaces: Option<usize>,
    scan: Arc<OnceLock<Scan>>,
}

/// What a place holds.
struct Scan {
    clips: Vec<Found>,
    /// Which of them were imported before (by the same index).
    before: Vec<bool>,
    /// The new clips by the library's folder they'd go in ("Other": the
    /// library itself), most first.
    games: Vec<(String, usize)>,
    /// The folders the new clips are in, with how many.
    folders: Vec<(PathBuf, usize)>,
}

impl Scan {
    fn new_clips(&self) -> impl Iterator<Item = &Found> {
        self.clips.iter().zip(&self.before).filter(|(_, b)| !**b).map(|(c, _)| c)
    }
}

/// The library's folder for clips with no game, as the sidebar names it.
const OTHER: &str = "Other";

fn scan_now(src: &Source, others: &[PathBuf], lib: &Path, per_game: bool) -> Scan {
    let before = import_apps::imported_before(lib);
    let clips = import_apps::scan(src, others, lib);
    let before: Vec<bool> = clips.iter().map(|c| before.contains(&import_apps::ledger_key(c))).collect();
    let mut games: Vec<(String, usize)> = Vec::new();
    let mut folders: Vec<(PathBuf, usize)> = Vec::new();
    for c in clips.iter().zip(&before).filter(|(_, b)| !**b).map(|(c, _)| c) {
        let into = import_apps::folder_for(lib, c.game.as_deref(), per_game).unwrap_or_else(|| OTHER.to_owned());
        match games.iter_mut().find(|(g, _)| *g == into) {
            Some((_, n)) => *n += 1,
            None => games.push((into, 1)),
        }
        match folders.iter_mut().find(|(f, _)| *f == c.folder) {
            Some((_, n)) => *n += 1,
            None => folders.push((c.folder.clone(), 1)),
        }
    }
    // Most first; Other last, as the sidebar has it.
    games.sort_by_key(|(g, n)| (g == OTHER, std::cmp::Reverse(*n), g.to_lowercase()));
    folders.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    Scan { clips, before, games, folders }
}

impl Entry {
    /// Look in `src`'s folders, in the background (`others`: the other
    /// apps' folders, left to them).
    fn scanned(ctx: &egui::Context, src: Source, others: Vec<PathBuf>, lib: PathBuf, per_game: bool, replaces: Option<usize>) -> Self {
        let scan = Arc::new(OnceLock::new());
        let (out, s, ctx) = (scan.clone(), src.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = out.set(scan_now(&s, &others, &lib, per_game));
            ctx.request_repaint();
        });
        Entry { src, replaces, scan }
    }
}

impl App {
    /// Open the Import clips dialog, looking for other apps' clips.
    pub(crate) fn open_import(&mut self) {
        let found = Arc::new(OnceLock::new());
        let (out, ctx, lib, per_game) = (found.clone(), self.ctx(), self.settings.output_dir.clone(), self.settings.folder_per_game);
        std::thread::spawn(move || {
            let sources = import_apps::detect();
            let folders: Vec<PathBuf> = sources.iter().flat_map(|s| s.folders.clone()).collect();
            // Each app's clips, all looked for at once on this thread (the
            // dialog shows them together).
            let entries = sources
                .into_iter()
                .map(|src| {
                    let scan = Arc::new(OnceLock::new());
                    let _ = scan.set(scan_now(&src, &folders, &lib, per_game));
                    Entry { src, replaces: None, scan }
                })
                .collect();
            let _ = out.set(Looked { entries, free: import_apps::free_space(&lib), drive: import_apps::drive_name(&lib) });
            ctx.request_repaint();
        });
        self.import = Some(ImportDialog { found, changed: Vec::new(), off: HashSet::new(), mode: Mode::Copy });
    }

    /// The Import clips dialog, while it's open.
    pub(crate) fn import_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.import else { return };
        let busy = self.transfer.is_some();
        let lib = self.settings.output_dir.clone();
        let per_game = self.settings.folder_per_game;
        let looked: Option<&Looked> = d.found.get();
        let found_count = looked.map_or(0, |l| l.entries.len());
        // The places listed: the apps found (or what replaced their folder),
        // then folders added by hand.
        let mut places: Vec<Entry> = Vec::new();
        if let Some(l) = looked {
            for (i, e) in l.entries.iter().enumerate() {
                places.push(d.changed.iter().rev().find(|c| c.replaces == Some(i)).cloned().unwrap_or_else(|| e.clone()));
            }
        }
        places.extend(d.changed.iter().filter(|c| c.replaces.is_none()).cloned());
        let all_folders: Vec<PathBuf> = places.iter().flat_map(|e| e.src.folders.clone()).collect();
        // An app's default folder that holds none of its clips isn't shown
        // (Videos is on every computer, NVIDIA or not); a folder picked by
        // hand always is.
        let shown = |e: &Entry| e.src.kind == AppKind::Folder || e.replaces.is_some() || e.scan.get().is_none_or(|s| !s.clips.is_empty());

        // What would be imported: the new clips of the places switched on,
        // each once (a clip copied to two folders, by its name and size).
        let mut seen = HashSet::new();
        let mut chosen: Vec<Found> = Vec::new();
        let mut looking = looked.is_none();
        for (i, e) in places.iter().enumerate() {
            match e.scan.get() {
                None => looking = true,
                Some(s) if !d.off.contains(&i) => {
                    // A copy (`… (2)`, `… - Copy`) of the same size is the same clip.
                    let key = |c: &Found| {
                        if c.steam {
                            return (c.path.to_string_lossy().to_lowercase(), 0);
                        }
                        let stem = c.path.file_stem().unwrap_or_default().to_string_lossy();
                        (format!("{}.{}", import_apps::copy_of(&stem).to_lowercase(), c.ext), c.bytes)
                    };
                    chosen.extend(s.new_clips().filter(|c| seen.insert(key(c))).cloned());
                }
                Some(_) => {}
            }
        }
        let none_shown = looked.is_some() && !looking && !places.iter().any(shown);
        let needed: u64 = chosen.iter().map(|c| c.needs(d.mode)).sum();
        let move_needed: u64 = chosen.iter().map(|c| c.needs(Mode::Move)).sum();
        let free = looked.and_then(|l| l.free);
        let drive = looked.and_then(|l| l.drive.clone());
        let fits = |n: u64| n == 0 || free.is_none_or(|f| n + import_apps::KEEP_FREE <= f);
        let enough = fits(needed);
        let on_drive = drive.as_ref().map_or("on your clips folder's drive".to_owned(), |d| format!("on {d}"));

        let mut close = false;
        let mut go = false;
        let mut pick_folder: Option<Option<usize>> = None;
        let mut pick_file = false;
        let modal = ui_kit::dialog(ctx, "import_clips", 540.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, "Import clips");
            ui_kit::dialog_text(ui, "Bring in the clips you made with another app. Each goes into your library's folder for its game.");
            ui.add_space(8.0);
            ui_kit::list(ui, "import_places", |l| {
                if looked.is_none() {
                    l.row(Row::new("Looking for clipping apps…"), |ui| {
                        ui.spinner();
                    });
                } else if none_shown {
                    let none = format!("None of the apps HasteClip knows ({}) keeps clips on this computer.", import_apps::HERE.iter().map(|k| k.name()).collect::<Vec<_>>().join(", "));
                    l.row(Row::new("No clipping apps found").note(none), |_| {});
                }
                for (i, e) in places.iter().enumerate().filter(|(_, e)| shown(e)) {
                    let label = if e.src.kind == AppKind::Folder { "Folder" } else { e.src.kind.name() };
                    let (note, new, all_games) = match e.scan.get() {
                        None => (format!("Looking…\n{}", folder_lines(&e.src.folders.iter().map(|f| (f.clone(), 0)).collect::<Vec<_>>())), 0, None),
                        Some(s) => {
                            let mut lines = vec![summary(s)];
                            if !s.games.is_empty() {
                                lines.push(games_line(&s.games));
                            }
                            // Where its new clips are; the folders looked in if none.
                            let folders = if s.folders.is_empty() { e.src.folders.iter().map(|f| (f.clone(), 0)).collect() } else { s.folders.clone() };
                            lines.push(folder_lines(&folders));
                            if d.mode == Mode::Move
                                && let Some(why) = copied_instead(e.src.kind, s)
                            {
                                lines.push(why);
                            }
                            let all = (s.games.len() > GAMES_SHOWN).then(|| s.games.iter().map(|(g, n)| format!("{g}: {}", clips_word(*n))).collect::<Vec<_>>().join("\n"));
                            (lines.join("\n"), s.new_clips().count(), all)
                        }
                    };
                    let tip_title = format!("Where {label}'s clips go");
                    let mut row = Row::new(label).note(note);
                    if let Some(all) = &all_games {
                        row = row.tip(&tip_title, all);
                    }
                    l.row(row, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            // Laid out from the right: the switch on the
                            // edge, as on every settings row.
                            let mut on = !d.off.contains(&i) && new > 0;
                            let was = on;
                            ui.add_enabled_ui(new > 0, |ui| ui_kit::switch(ui, &mut on));
                            if on != was {
                                if on {
                                    d.off.remove(&i);
                                } else {
                                    d.off.insert(i);
                                }
                            }
                            if Btn::ghost("Change folder…").small().id(("import_change", i)).show(ui).clicked() {
                                pick_folder = Some(Some(i));
                            }
                        });
                    });
                }
                l.row(Row::new("Another folder").note("Clips from any other app: its folders are taken as games"), |ui| {
                    if Btn::normal("Choose…").small().id("import_folder").show(ui).clicked() {
                        pick_folder = Some(None);
                    }
                });
                l.row(Row::new("HasteClip file").note("Clips exported from HasteClip on another computer (.hasteclip)"), |ui| {
                    if Btn::normal("Choose…").small().id("import_file").show(ui).clicked() {
                        pick_file = true;
                    }
                });
            });
            ui.add_space(12.0);
            ui_kit::list(ui, "import_mode", |l| {
                let free_text = free.map_or(String::new(), |f| format!(" {} is free {on_drive}.", human_bytes(f)));
                let note = match d.mode {
                    Mode::Copy if !enough => {
                        let instead = if move_needed < needed && fits(move_needed) {
                            if move_needed == 0 { " Moving them needs no space: they're on the same drive.".to_owned() } else { format!(" Moving them needs {}.", human_bytes(move_needed)) }
                        } else {
                            String::new()
                        };
                        format!("Copying needs {}, but only {} is free {on_drive}.{instead}", human_bytes(needed), human_bytes(free.unwrap_or(0)))
                    }
                    Mode::Copy if needed > 0 => format!("Copied into your library, using {}.{free_text} The originals stay where they are.", human_bytes(needed)),
                    Mode::Copy => "Copied into your library. The originals stay where they are.".to_owned(),
                    Mode::Move if !enough => format!("Moving needs {} for the clips that are copied, but only {} is free {on_drive}.", human_bytes(needed), human_bytes(free.unwrap_or(0))),
                    Mode::Move => {
                        let copied = chosen.iter().filter(|c| c.copy_only.is_some()).count();
                        let elsewhere = chosen.iter().filter(|c| c.copy_only.is_none() && !c.on_library_drive).count();
                        let mut text = "Moved into your library: they leave the other apps' folders.".to_owned();
                        if elsewhere > 0 {
                            text += &format!(" The {} on other drives {} copied, checked byte for byte, then deleted there.", clips_word(elsewhere), if elsewhere == 1 { "is" } else { "are" });
                        }
                        if copied > 0 {
                            text += &format!(" {} {} copied instead, and stay{} where {} too (the rows say why).", clips_word(copied), if copied == 1 { "is" } else { "are" }, if copied == 1 { "s" } else { "" }, if copied == 1 { "it is" } else { "they are" });
                        }
                        if needed == 0 {
                            text += " No extra space needed: they're on the same drive.";
                        } else {
                            text += &format!(" Needs {}; {}.", human_bytes(needed), free.map_or("space unknown".to_owned(), |f| format!("{} free {on_drive}", human_bytes(f))));
                        }
                        text
                    }
                };
                l.row(Row::new("Copy or move").note(note).warn(!enough), |ui| {
                    if let Some(k) = ui_kit::segmented_opts(ui, "import_mode_seg", &[Seg::from("Copy"), Seg::from("Move")], usize::from(d.mode == Mode::Move), None, true) {
                        d.mode = if k == 1 { Mode::Move } else { Mode::Copy };
                    }
                });
            });
            if looking {
                ui.add_space(6.0);
                ui.label(theme::text("Still looking: more may be added.", 12.5, theme::W::Regular).color(p.text3));
            }
            ui_kit::dialog_buttons(ui, |ui| {
                let n = chosen.len();
                let label = match n {
                    0 => "Import".to_owned(),
                    1 => "Import 1 clip".to_owned(),
                    n => format!("Import {n} clips"),
                };
                let lock = if busy {
                    Some("Wait for the export or import that's running to finish".to_owned())
                } else if !enough {
                    Some(format!("Not enough space {on_drive}: it needs {}, and {} is free", human_bytes(needed + import_apps::KEEP_FREE), human_bytes(free.unwrap_or(0))))
                } else {
                    None
                };
                if Btn::primary(label).enabled(n > 0).locked(lock).show(ui).clicked() {
                    go = true;
                }
                if Btn::normal("Cancel").show(ui).clicked() {
                    close = true;
                }
            });
        });
        if modal.dismissed() {
            close = true;
        }

        if let Some(which) = pick_folder {
            let title = match which {
                Some(i) => format!("Where {} keeps its clips", places[i].src.kind.name()),
                None => "Folder to import clips from".to_owned(),
            };
            let mut dialog = rfd::FileDialog::new().set_title(title);
            if let Some(start) = which.and_then(|i| places[i].src.folders.first()) {
                dialog = dialog.set_directory(start);
            }
            if let Some(folder) = dialog.pick_folder().map(|f| import_apps::clean(&f)) {
                if import_apps::within(&folder, &lib) {
                    self.toast_error("That folder is in your clips folder: its clips are already in HasteClip");
                    return;
                }
                let kind = which.map_or(AppKind::Folder, |i| places[i].src.kind);
                let others: Vec<PathBuf> = all_folders.iter().filter(|f| **f != folder).cloned().collect();
                let src = Source { kind, folders: vec![folder] };
                // A place replaced keeps its place (a found app's, or one
                // added by hand).
                let replaces = which.and_then(|i| places[i].replaces.or_else(|| (i < found_count).then_some(i)));
                let added_index = which.and_then(|i| d.changed.iter().position(|c| c.replaces.is_none() && Arc::ptr_eq(&c.scan, &places[i].scan)));
                let entry = Entry::scanned(ctx, src, others, lib.clone(), per_game, replaces);
                match added_index {
                    Some(k) => d.changed[k] = entry,
                    None => d.changed.push(entry),
                }
                if let Some(i) = which {
                    d.off.remove(&i);
                }
            }
        }
        if pick_file {
            self.import = None;
            self.import_clips_dialog();
            return;
        }
        if go && !chosen.is_empty() && enough {
            let mode = d.mode;
            self.import = None;
            self.import_from_apps(chosen, mode);
            return;
        }
        if close {
            self.import = None;
        }
    }
}

/// Why some or all of a place's clips are copied though moving, if any are.
fn copied_instead(kind: AppKind, s: &Scan) -> Option<String> {
    let mut steam = 0;
    let mut records = 0;
    let mut beside = 0;
    for c in s.new_clips() {
        match c.copy_only {
            Some(CopyOnly::Steam) => steam += 1,
            Some(CopyOnly::Records) => records += 1,
            Some(CopyOnly::Sidecar) => beside += 1,
            None => {}
        }
    }
    if steam > 0 {
        return Some("Copied, not moved: Steam keeps its own index of them".to_owned());
    }
    if records > 0
        && let Some(why) = kind.kept_back()
    {
        return Some(format!("Copied, not moved: {why}"));
    }
    (beside > 0).then(|| {
        let (it, has) = if beside == 1 { ("it", "has") } else { ("them", "have") };
        format!("{} copied, not moved: {} a file beside {it} (like a LosslessCut project) that would be left behind", clips_word(beside), if beside == 1 { format!("1 {has}") } else { format!("they {has}") })
    })
}

/// How many games are named on an app's row; the rest are in its ⓘ.
const GAMES_SHOWN: usize = 4;

/// "1 clip", "22 clips".
fn clips_word(n: usize) -> String {
    if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") }
}

/// "Into osu! 22 · Desktop 17 · Phasmophobia 4 · Other 6 · 2 more".
fn games_line(games: &[(String, usize)]) -> String {
    let mut parts: Vec<String> = games.iter().take(GAMES_SHOWN).map(|(g, n)| format!("{g} {n}")).collect();
    if games.len() > GAMES_SHOWN {
        parts.push(format!("{} more", games.len() - GAMES_SHOWN));
    }
    format!("Into {}", parts.join(" · "))
}

/// The folders, one a line, with how many clips each has when there are
/// several.
fn folder_lines(folders: &[(PathBuf, usize)]) -> String {
    let several = folders.len() > 1 && folders.iter().all(|(_, n)| *n > 0);
    folders
        .iter()
        .map(|(f, n)| if several { format!("{} ({n})", f.display()) } else { f.display().to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// "312 clips · 48 GB · 2019–2024 · 12 imported before", or that there are none.
fn summary(s: &Scan) -> String {
    let new: Vec<&Found> = s.new_clips().collect();
    let before = s.before.iter().filter(|b| **b).count();
    let before_text = match before {
        0 => String::new(),
        1 => " · 1 imported before".to_owned(),
        k => format!(" · {k} imported before"),
    };
    if new.is_empty() {
        return if before > 0 { format!("Nothing new{before_text}") } else { "No clips found".to_owned() };
    }
    let bytes = human_bytes(new.iter().map(|c| c.bytes).sum());
    let years = {
        use chrono::Datelike as _;
        let (lo, hi) = new.iter().fold((i32::MAX, i32::MIN), |(lo, hi), c| (lo.min(c.captured.year()), hi.max(c.captured.year())));
        if lo == hi { lo.to_string() } else { format!("{lo}–{hi}") }
    };
    format!("{} · {bytes} · {years}{before_text}", clips_word(new.len()))
}
