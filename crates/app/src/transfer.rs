//! Exporting clips to a `.hasteclip` file and importing one (see
//! `archive.rs`), or importing clips from other apps (`import_apps.rs`):
//! the work on a thread of its own, and a toast that shows how far it's
//! come (with Cancel, for other apps' clips: there may be hundreds of GB),
//! then how it went.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_phosphor::regular as icon;

use crate::App;
use crate::archive::{self, Imported, Progress};

/// The export or import running, if any (one at a time).
pub struct Transfer {
    what: Kind,
    progress: Progress,
    /// Set to stop it (other apps' clips only).
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    done: std::sync::mpsc::Receiver<Result<Outcome, String>>,
}

enum Kind {
    Export(usize),
    Import,
    /// This many clips from other apps.
    FromApps(usize),
}

enum Outcome {
    Exported(usize, PathBuf),
    Imported(Imported),
    FromApps(crate::import_apps::Done),
}

impl App {
    /// Ask where, then write these clips (with their edits, markers and
    /// collections) to a `.hasteclip` file.
    pub(crate) fn export_clips(&mut self, paths: &[PathBuf]) {
        if self.transfer.is_some() {
            self.toast_error("Wait for the export or import that's running to finish");
            return;
        }
        let clips: Vec<crate::clips::Clip> = self.clips.iter().filter(|c| paths.contains(&c.path)).cloned().collect();
        if clips.is_empty() {
            return;
        }
        let name = if clips.len() == 1 { clips[0].editable_name() } else { format!("{} clips", clips.len()) };
        let Some(mut out) = rfd::FileDialog::new()
            .set_title("Export clips")
            .set_file_name(format!("{name}.{}", archive::EXT))
            .add_filter("HasteClip clips", &[archive::EXT])
            .save_file()
        else {
            return;
        };
        if !out.extension().is_some_and(|e| e.eq_ignore_ascii_case(archive::EXT)) {
            let mut s = out.into_os_string();
            s.push(format!(".{}", archive::EXT));
            out = s.into();
        }
        let lib = self.settings.output_dir.clone();
        // The names of the collections each clip is in.
        let collections: Vec<Vec<String>> = clips.iter().map(|c| self.collections.list().iter().filter(|col| self.collections.contains(&col.id, &c.path)).map(|col| col.name.clone()).collect()).collect();
        let n = clips.len();
        self.start_transfer(Kind::Export(n), move |progress| archive::export(&lib, &clips, &collections, &out, progress).map(|n| Outcome::Exported(n, out)));
    }

    /// Ask for `.hasteclip` files, then import them.
    pub(crate) fn import_clips_dialog(&mut self) {
        let Some(files) = rfd::FileDialog::new().set_title("Import clips").add_filter("HasteClip clips", &[archive::EXT, archive::OTHER_EXT]).pick_files() else { return };
        self.import_clips(files);
    }

    /// Put the clips in these `.hasteclip` files into the library.
    pub(crate) fn import_clips(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        if self.transfer.is_some() {
            self.toast_error("Wait for the export or import that's running to finish");
            return;
        }
        let lib = self.settings.output_dir.clone();
        self.start_transfer(Kind::Import, move |progress| {
            let mut all = Imported::default();
            for f in &files {
                let done = archive::import(f, &lib, progress)?;
                all.added.extend(done.added);
                all.skipped += done.skipped;
            }
            Ok(Outcome::Imported(all))
        });
    }

    /// Bring these clips from other apps into the library, copied or moved.
    pub(crate) fn import_from_apps(&mut self, clips: Vec<crate::import_apps::Found>, mode: crate::import_apps::Mode) {
        if clips.is_empty() {
            return;
        }
        if self.transfer.is_some() {
            self.toast_error("Wait for the export or import that's running to finish");
            return;
        }
        let lib = self.settings.output_dir.clone();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancel.clone();
        self.start_transfer_with(Kind::FromApps(clips.len()), cancel, move |progress| Ok(Outcome::FromApps(crate::import_apps::import(&clips, &lib, mode, progress, &stop))));
    }

    /// Stop the import from other apps that's running (what's in already stays).
    pub(crate) fn cancel_transfer(&mut self) {
        if let Some(t) = &self.transfer {
            t.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_transfer(&mut self, what: Kind, work: impl FnOnce(&Progress) -> Result<Outcome, String> + Send + 'static) {
        self.start_transfer_with(what, Default::default(), work);
    }

    fn start_transfer_with(&mut self, what: Kind, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>, work: impl FnOnce(&Progress) -> Result<Outcome, String> + Send + 'static) {
        let progress = Progress::default();
        let (tx, done) = std::sync::mpsc::channel();
        let (p, ctx) = (progress.clone(), self.ctx());
        std::thread::spawn(move || {
            let _ = tx.send(work(&p));
            ctx.request_repaint();
        });
        self.transfer = Some(Transfer { what, progress, cancel, done });
        self.show_transfer();
    }

    /// `.hasteclip` files dropped on the window are imported.
    pub(crate) fn import_dropped(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| archive::is_archive(p)).collect());
        self.import_clips(files);
    }

    /// Each frame: keep the toast up to date, and say how it went once done.
    pub(crate) fn pump_transfer(&mut self) {
        let Some(t) = &self.transfer else { return };
        match t.done.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.show_transfer();
                self.ctx().request_repaint_after(Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.transfer = None;
                self.toast_error("The export or import stopped unexpectedly");
            }
            Ok(result) => {
                let export = matches!(t.what, Kind::Export(_));
                self.transfer = None;
                match result {
                    Ok(Outcome::Exported(n, out)) => {
                        let what = if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") };
                        let name = out.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        self.toast_icon(icon::EXPORT, format!("Exported {what} to “{name}”"));
                    }
                    Ok(Outcome::Imported(done)) => self.imported(done),
                    Ok(Outcome::FromApps(done)) => self.imported_from_apps(done),
                    Err(e) => self.toast_error(if export { format!("Couldn't export: {e}") } else { format!("Couldn't import: {e}") }),
                }
            }
        }
    }

    /// The toast while it runs: what, and how far.
    fn show_transfer(&mut self) {
        let Some(t) = &self.transfer else { return };
        let (done, total) = (t.progress.0.load(std::sync::atomic::Ordering::Relaxed), t.progress.1.load(std::sync::atomic::Ordering::Relaxed));
        let pct = (done * 100).checked_div(total).map_or(String::new(), |p| format!(" {}%", p.min(99)));
        let text = match t.what {
            Kind::Export(1) => format!("Exporting 1 clip…{pct}"),
            Kind::Export(n) => format!("Exporting {n} clips…{pct}"),
            Kind::Import => format!("Importing clips…{pct}"),
            Kind::FromApps(1) => format!("Importing 1 clip…{pct}"),
            Kind::FromApps(n) => format!("Importing {n} clips…{pct}"),
        };
        let icon = if matches!(t.what, Kind::Export(_)) { icon::EXPORT } else { icon::DOWNLOAD_SIMPLE };
        let action = match t.what {
            Kind::FromApps(_) if !t.cancel.load(std::sync::atomic::Ordering::Relaxed) => Some(("Cancel".to_owned(), crate::shell::ToastAction::CancelTransfer)),
            _ => None,
        };
        // Kept up (refreshed) for as long as it runs.
        self.toast = Some(crate::shell::Toast { text, icon, error: false, at: Instant::now(), action, for_: Duration::from_secs(4) });
    }

    /// Clips from other apps imported (all, some, or stopped part way).
    fn imported_from_apps(&mut self, done: crate::import_apps::Done) {
        self.refresh_clips();
        let n = done.added.len();
        let what = if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") };
        let failed = done.failed.len();
        if failed > 0 {
            let them = if failed == 1 { "1 couldn't be".to_owned() } else { format!("{failed} couldn't be") };
            self.toast_error(format!("Imported {what}; {them} (the log says why)"));
        } else if done.cancelled {
            self.toast_icon(icon::DOWNLOAD_SIMPLE, format!("Stopped: imported {what}"));
        } else if let [one] = done.added.as_slice() {
            let title = crate::clips::title_for_stem(&one.file_stem().unwrap_or_default().to_string_lossy());
            self.toast_action(icon::DOWNLOAD_SIMPLE, format!("Imported {title}"), "Play", crate::shell::ToastAction::Show(one.clone()));
        } else {
            self.toast_icon(icon::DOWNLOAD_SIMPLE, format!("Imported {what}"));
        }
    }

    /// Imported: the library and its collections see the new clips.
    fn imported(&mut self, done: Imported) {
        done.collect(&mut self.collections);
        self.refresh_clips();
        let n = done.added.len();
        let skipped = match done.skipped {
            0 => String::new(),
            1 => " (1 was already here)".to_owned(),
            k => format!(" ({k} were already here)"),
        };
        match (n, done.added.first().map(|(p, _)| p)) {
            (0, _) if done.skipped > 0 => self.toast_icon(icon::CHECK_CIRCLE, "Nothing new: you already have these clips"),
            (0, _) => self.toast_icon(icon::CHECK_CIRCLE, "There were no clips in it"),
            (1, Some(p)) => {
                let title = crate::clips::title_for_stem(&p.file_stem().unwrap_or_default().to_string_lossy());
                self.toast_action(icon::DOWNLOAD_SIMPLE, format!("Imported {title}{skipped}"), "Play", crate::shell::ToastAction::Show(p.clone()));
            }
            _ => self.toast_icon(icon::DOWNLOAD_SIMPLE, format!("Imported {n} clips{skipped}")),
        }
    }
}
