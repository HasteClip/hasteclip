//! The Import clips dialog (the library's Import clips…): the clipping apps
//! found here with their clips (how many, how big, from when), each with a
//! switch and its folder to change; a folder of your own; a `.hasteclip`
//! file; and whether the clips are copied (the default) or moved. Importing
//! runs as a transfer (`transfer.rs`): a toast with how far, and Cancel.
//!
//! The apps and their clips are looked for in the background as the dialog
//! opens (`import_apps`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::App;
use crate::import_apps::{self, AppKind, Found, Mode, Source};
use crate::theme;
use crate::ui_kit::{self, Btn, Dismissed as _, Row, Seg};

/// The dialog, while it's open.
pub struct ImportDialog {
    /// The apps found here, once looked for.
    found: Arc<OnceLock<Vec<Entry>>>,
    /// Folders added by hand, and apps whose folder was changed (by their
    /// place in the list), each scanned on its own.
    changed: Vec<Entry>,
    /// The places switched off.
    off: HashSet<usize>,
    mode: Mode,
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
}

impl Scan {
    fn new_clips(&self) -> impl Iterator<Item = &Found> {
        self.clips.iter().zip(&self.before).filter(|(_, b)| !**b).map(|(c, _)| c)
    }
}

impl Entry {
    /// Look in `src`'s folders, in the background (`others`: the other
    /// apps' folders, left to them).
    fn scanned(ctx: &egui::Context, src: Source, others: Vec<PathBuf>, lib: PathBuf, replaces: Option<usize>) -> Self {
        let scan = Arc::new(OnceLock::new());
        let (out, s, ctx) = (scan.clone(), src.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = out.set(scan_now(&s, &others, &lib));
            ctx.request_repaint();
        });
        Entry { src, replaces, scan }
    }
}

fn scan_now(src: &Source, others: &[PathBuf], lib: &std::path::Path) -> Scan {
    let before = import_apps::imported_before(lib);
    let clips = import_apps::scan(src, others);
    let before = clips.iter().map(|c| before.contains(&import_apps::ledger_key(c))).collect();
    Scan { clips, before }
}

impl App {
    /// Open the Import clips dialog, looking for other apps' clips.
    pub(crate) fn open_import(&mut self) {
        let found = Arc::new(OnceLock::new());
        let (out, ctx, lib) = (found.clone(), self.ctx(), self.settings.output_dir.clone());
        std::thread::spawn(move || {
            let sources = import_apps::detect();
            let folders: Vec<PathBuf> = sources.iter().flat_map(|s| s.folders.clone()).collect();
            // Each app's clips, all looked for at once on this thread (the
            // dialog shows them together).
            let entries = sources
                .into_iter()
                .map(|src| {
                    let scan = Arc::new(OnceLock::new());
                    let _ = scan.set(scan_now(&src, &folders, &lib));
                    Entry { src, replaces: None, scan }
                })
                .collect();
            let _ = out.set(entries);
            ctx.request_repaint();
        });
        self.import = Some(ImportDialog { found, changed: Vec::new(), off: HashSet::new(), mode: Mode::Copy });
    }

    /// The Import clips dialog, while it's open.
    pub(crate) fn import_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.import else { return };
        let busy = self.transfer.is_some();
        let lib = self.settings.output_dir.clone();
        let found: Option<&Vec<Entry>> = d.found.get();
        // The places listed: the apps found (or what replaced their folder),
        // then folders added by hand.
        let mut places: Vec<Entry> = Vec::new();
        if let Some(found) = found {
            for (i, e) in found.iter().enumerate() {
                places.push(d.changed.iter().rev().find(|c| c.replaces == Some(i)).cloned().unwrap_or_else(|| e.clone()));
            }
        }
        places.extend(d.changed.iter().filter(|c| c.replaces.is_none()).cloned());
        let all_folders: Vec<PathBuf> = places.iter().flat_map(|e| e.src.folders.clone()).collect();
        // An app's default folder that holds none of its clips isn't shown
        // (Videos is on every computer, NVIDIA or not); a folder picked by
        // hand always is.
        let shown = |e: &Entry| e.src.kind == AppKind::Folder || e.replaces.is_some() || e.scan.get().is_none_or(|s| !s.clips.is_empty());
        let none_shown = found.is_some() && !looking_any(&places) && !places.iter().any(shown);

        // What would be imported: the new clips of the places switched on,
        // each once.
        let mut seen = HashSet::new();
        let mut chosen: Vec<Found> = Vec::new();
        let mut looking = found.is_none();
        for (i, e) in places.iter().enumerate() {
            match e.scan.get() {
                None => looking = true,
                Some(s) if !d.off.contains(&i) => chosen.extend(s.new_clips().filter(|c| seen.insert(c.path.clone())).cloned()),
                Some(_) => {}
            }
        }
        let bytes: u64 = chosen.iter().map(|c| c.bytes).sum();

        let mut close = false;
        let mut go = false;
        let mut pick_folder: Option<Option<usize>> = None;
        let mut pick_file = false;
        let modal = ui_kit::dialog(ctx, "import_clips", 520.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, "Import clips");
            ui_kit::dialog_text(ui, "Bring in the clips you made with another app. Each goes into your library's folder for its game.");
            ui.add_space(8.0);
            ui_kit::list(ui, "import_places", |l| {
                if found.is_none() {
                    l.row(Row::new("Looking for clipping apps…"), |ui| {
                        ui.spinner();
                    });
                } else if none_shown {
                    let none = format!("None of the apps HasteClip knows ({}) keeps clips on this computer.", import_apps::HERE.iter().map(|k| k.name()).collect::<Vec<_>>().join(", "));
                    l.row(Row::new("No clipping apps found").note(none), |_| {});
                }
                for (i, e) in places.iter().enumerate().filter(|(_, e)| shown(e)) {
                    let label = if e.src.kind == AppKind::Folder { "Folder" } else { e.src.kind.name() };
                    let folders = e.src.folders.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\n");
                    let (note, new) = match e.scan.get() {
                        None => (format!("Looking…\n{folders}"), 0),
                        Some(s) => (format!("{}\n{folders}", summary(s)), s.new_clips().count()),
                    };
                    l.row(Row::new(label).note(note), |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if Btn::ghost("Change folder…").small().id(("import_change", i)).show(ui).clicked() {
                                pick_folder = Some(Some(i));
                            }
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
                let note = match d.mode {
                    Mode::Copy => format!("Copied into your library{}. The originals stay where they are.", if bytes > 0 { format!(" ({} more space)", crate::clips::human_bytes(bytes)) } else { String::new() }),
                    Mode::Move => "Moved into your library, and taken out of the other app's folder once each is in. Steam's are always copied.".to_owned(),
                };
                l.row(Row::new("Copy or move").note(note), |ui| {
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
                let lock = busy.then(|| "Wait for the export or import that's running to finish".to_owned());
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
            if let Some(folder) = dialog.pick_folder() {
                let kind = which.map_or(AppKind::Folder, |i| places[i].src.kind);
                let others: Vec<PathBuf> = all_folders.iter().filter(|f| **f != folder).cloned().collect();
                let src = Source { kind, folders: vec![folder] };
                // A place replaced keeps its place (a found app's, or one
                // added by hand).
                let replaces = which.and_then(|i| places[i].replaces.or_else(|| (i < found.map_or(0, Vec::len)).then_some(i)));
                let added_index = which.and_then(|i| d.changed.iter().position(|c| c.replaces.is_none() && Arc::ptr_eq(&c.scan, &places[i].scan)));
                let entry = Entry::scanned(ctx, src, others, lib.clone(), replaces);
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
        if go && !chosen.is_empty() {
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

/// Whether any place is still being looked in.
fn looking_any(places: &[Entry]) -> bool {
    places.iter().any(|e| e.scan.get().is_none())
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
    let clips = if new.len() == 1 { "1 clip".to_owned() } else { format!("{} clips", new.len()) };
    let bytes = crate::clips::human_bytes(new.iter().map(|c| c.bytes).sum());
    let years = {
        use chrono::Datelike as _;
        let (lo, hi) = new.iter().fold((i32::MAX, i32::MIN), |(lo, hi), c| (lo.min(c.captured.year()), hi.max(c.captured.year())));
        if lo == hi { lo.to_string() } else { format!("{lo}–{hi}") }
    };
    format!("{clips} · {bytes} · {years}{before_text}")
}
