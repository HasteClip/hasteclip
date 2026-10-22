//! The webcam on the Sources page: picking a camera, and placing it on the live preview the way OBS does — drag
//! to move, any handle to resize (always keeping its shape: it's never
//! stretched), past the opposite side to flip it, Alt with a handle to crop,
//! right-click to flip or turn it a quarter. Moves snap to the frame's edges and centre (Ctrl to place
//! freely).
//!
//! Placement is fractions of the frame (see `capture::webcam::Placement`); it's
//! shared with the capture, so what you drag is what's recorded, live. So is
//! everything else here: a running capture follows whichever camera is open,
//! so the camera, its format, and adding or removing it all apply at once.

use capture::webcam::{Placement, Status};
use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, vec2};

use crate::App;
use capture::webcam::{CameraControl, ControlKind};
/// The webcam's box outline on the preview.
const ACCENT: Color32 = Color32::WHITE;
use crate::settings::{PlacementCfg, WebcamCfg};

/// Smallest the webcam's box gets, as a fraction of the frame.
const MIN_SIZE: f32 = 0.03;
/// Snap distance, as a fraction of the frame.
const SNAP: f32 = 0.012;
/// Handle size, in points.
const HANDLE: f32 = 8.0;
/// OBS-ish crop colour.
const CROP: Color32 = Color32::from_rgb(80, 220, 120);

#[derive(Default)]
pub(crate) struct WebcamView {
    /// Cameras for the picker, refreshed when it opens.
    cameras: Vec<capture::Device>,
    /// A drag in progress: what's held, where it started, and the placement then.
    drag: Option<(Handle, Pos2, Placement)>,
    /// Just added, or a new camera or format: size its box to the camera's
    /// shape once the camera delivers it.
    fit_pending: bool,
    /// The camera's size when that was asked for: until it changes, the
    /// camera is still the old one (it takes a moment to reopen), and fitting
    /// to it would shape the box for the format just left.
    fit_from: Option<(u32, u32)>,
    /// The right-click menu is open (it was opened on the box).
    menu: bool,
}

impl WebcamView {
    /// Fit the box to the camera's shape once the camera reopens.
    fn ask_fit(&mut self) {
        self.fit_pending = true;
        self.fit_from = match capture::webcam::status() {
            Status::Live { width, height } => Some((width, height)),
            _ => None,
        };
    }
}

/// What part of the webcam's box is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    const ALL: [Handle; 8] = [Handle::NW, Handle::N, Handle::NE, Handle::E, Handle::SE, Handle::S, Handle::SW, Handle::W];

    /// Which sides it moves: left, top, right, bottom.
    fn sides(self) -> [bool; 4] {
        match self {
            Handle::Move => [false; 4],
            Handle::N => [false, true, false, false],
            Handle::S => [false, false, false, true],
            Handle::E => [false, false, true, false],
            Handle::W => [true, false, false, false],
            Handle::NE => [false, true, true, false],
            Handle::NW => [true, true, false, false],
            Handle::SE => [false, false, true, true],
            Handle::SW => [true, false, false, true],
        }
    }

    /// Where it sits on a box.
    fn pos(self, r: Rect) -> Pos2 {
        let [l, t, rr, b] = self.sides();
        let x = if l { r.left() } else if rr { r.right() } else { r.center().x };
        let y = if t { r.top() } else if b { r.bottom() } else { r.center().y };
        Pos2::new(x, y)
    }

    fn cursor(self) -> CursorIcon {
        match self {
            Handle::Move => CursorIcon::Grab,
            Handle::N | Handle::S => CursorIcon::ResizeVertical,
            Handle::E | Handle::W => CursorIcon::ResizeHorizontal,
            Handle::NE | Handle::SW => CursorIcon::ResizeNeSw,
            Handle::NW | Handle::SE => CursorIcon::ResizeNwSe,
        }
    }
}

/// How a drag behaves.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Mods {
    /// Crop instead of resizing (Alt).
    pub crop: bool,
    /// Snap to the frame's edges and centre (unless Ctrl).
    pub snap: bool,
}

/// The placement after dragging `handle` by `(dx, dy)` (fractions of the
/// frame) from where it was at `start`.
///
/// Resizing never stretches: every handle scales the whole picture, from the
/// opposite corner or edge (an edge handle keeps the other axis centred).
/// Dragging past that opposite side flips the picture on that axis.
///
/// The box stays inside the frame: moving stops at its edges, and so does
/// growing (what's not wanted of the picture is cropped instead).
pub(crate) fn drag(start: Placement, handle: Handle, dx: f32, dy: f32, mods: Mods) -> Placement {
    let mut p = start;
    if handle == Handle::Move {
        p.x = start.x + dx;
        p.y = start.y + dy;
        if mods.snap {
            p.x += snap_offset(&[p.x, p.x + p.w], p.x + p.w / 2.0);
            p.y += snap_offset(&[p.y, p.y + p.h], p.y + p.h / 2.0);
        }
        p.x = p.x.clamp(0.0, (1.0 - p.w).max(0.0));
        p.y = p.y.clamp(0.0, (1.0 - p.h).max(0.0));
        return p;
    }
    let [l, t, r, b] = handle.sides();
    if mods.crop {
        return crop(start, [l, t, r, b], dx, dy);
    }
    // Per axis: the fixed side (anchor), and how far the dragged side is from
    // it, signed, before and after. A sign change means it crossed: a flip.
    let axis = |near: bool, far: bool, pos: f32, size: f32, d: f32| -> Option<(f32, f32)> {
        if !near && !far {
            return None;
        }
        let (anchor, moving) = if near { (pos + size, pos) } else { (pos, pos + size) };
        let before = moving - anchor;
        Some((anchor, (before + d) / before))
    };
    let kx = axis(l, r, start.x, start.w, dx);
    let ky = axis(t, b, start.y, start.h, dy);
    // One uniform scale: the axis that moved more decides (a corner), or the
    // only one dragged (an edge).
    let k = match (kx, ky) {
        (Some((_, a)), Some((_, b))) => {
            if (a.abs() - 1.0).abs() >= (b.abs() - 1.0).abs() { a.abs() } else { b.abs() }
        }
        (Some((_, a)), None) | (None, Some((_, a))) => a.abs(),
        (None, None) => 1.0,
    };
    // No bigger than fits between its anchor and the frame's edge it grows
    // toward (both edges, for an axis kept centred).
    let room = |near: bool, ka: Option<(f32, f32)>, pos: f32, size: f32| -> f32 {
        match ka {
            Some((anchor, k)) => {
                if near != (k < 0.0) { anchor } else { 1.0 - anchor }
            }
            None => {
                let c = pos + size / 2.0;
                2.0 * c.min(1.0 - c)
            }
        }
    };
    let most = (room(l, kx, start.x, start.w) / start.w).min(room(t, ky, start.y, start.h) / start.h);
    let k = k.min(most).max(MIN_SIZE / start.w.min(start.h));
    let (w, h) = (start.w * k, start.h * k);
    // Place each axis: from its anchor, on the side the drag ended up on; an
    // axis that wasn't dragged stays centred.
    let place = |near: bool, ka: Option<(f32, f32)>, pos: f32, size: f32, new: f32| -> (f32, bool) {
        match ka {
            Some((anchor, k)) => {
                let flipped = k < 0.0;
                // The dragged side was before the anchor (near) or after it.
                let before = near != flipped;
                (if before { anchor - new } else { anchor }, flipped)
            }
            None => (pos + size / 2.0 - new / 2.0, false),
        }
    };
    let (x, fx) = place(l, kx, start.x, start.w, w);
    let (y, fy) = place(t, ky, start.y, start.h, h);
    p.x = x;
    p.y = y;
    p.w = w;
    p.h = h;
    p.flip_h = start.flip_h ^ fx;
    p.flip_v = start.flip_v ^ fy;
    p
}

/// Crop the sides in `sides` (as seen: a flipped picture's left side is the
/// camera's right) as they're dragged: the picture keeps its scale, the box
/// shrinks (or grows back) with the crop, like OBS's Alt-drag.
fn crop(start: Placement, [l, t, r, b]: [bool; 4], dx: f32, dy: f32) -> Placement {
    let mut p = start;
    let c = start.crop;
    // Which camera side each visible side is.
    let (left, right) = if start.flip_h { (2, 0) } else { (0, 2) };
    let (top, bottom) = if start.flip_v { (3, 1) } else { (1, 3) };
    // Frame fraction per camera fraction, across and down.
    let sx = start.w / (1.0 - c[0] - c[2]).max(0.01);
    let sy = start.h / (1.0 - c[1] - c[3]).max(0.01);
    let most = 0.95;
    // Uncropping stops where the box would leave the frame.
    let low = |side: f32, room: f32, scale: f32| (side - room / scale).max(0.0);
    if l {
        let new = (c[left] + dx / sx).clamp(low(c[left], start.x, sx), (most - c[right]).max(0.0));
        let d = (new - c[left]) * sx;
        p.crop[left] = new;
        p.x = start.x + d;
        p.w = start.w - d;
    }
    if r {
        let new = (c[right] - dx / sx).clamp(low(c[right], 1.0 - start.x - start.w, sx), (most - c[left]).max(0.0));
        p.crop[right] = new;
        p.w += (c[right] - new) * sx;
    }
    if t {
        let new = (c[top] + dy / sy).clamp(low(c[top], start.y, sy), (most - c[bottom]).max(0.0));
        let d = (new - c[top]) * sy;
        p.crop[top] = new;
        p.y = start.y + d;
        p.h = start.h - d;
    }
    if b {
        let new = (c[bottom] - dy / sy).clamp(low(c[bottom], 1.0 - start.y - start.h, sy), (most - c[top]).max(0.0));
        p.crop[bottom] = new;
        p.h += (c[bottom] - new) * sy;
    }
    p
}

/// The box inside the frame: shrunk about its centre if it's bigger than the
/// frame (keeping its shape), then moved in. For a box that may have left it
/// (turned a quarter, which swaps its sides).
pub(crate) fn contain(mut p: Placement) -> Placement {
    let k = (1.0 / p.w).min(1.0 / p.h).min(1.0);
    if k < 1.0 {
        let (cx, cy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
        p.w *= k;
        p.h *= k;
        p.x = cx - p.w / 2.0;
        p.y = cy - p.h / 2.0;
    }
    p.x = p.x.clamp(0.0, 1.0 - p.w);
    p.y = p.y.clamp(0.0, 1.0 - p.h);
    p
}

/// How far to move so the nearest of `edges` (to 0 or 1) or `center` (to 0.5)
/// lands on it, if one is close enough.
fn snap_offset(edges: &[f32], center: f32) -> f32 {
    let mut best: Option<f32> = None;
    let mut consider = |d: f32| {
        if d.abs() < SNAP && best.is_none_or(|b| d.abs() < b.abs()) {
            best = Some(d);
        }
    };
    for &e in edges {
        consider(0.0 - e);
        consider(1.0 - e);
    }
    consider(0.5 - center);
    best.unwrap_or(0.0)
}

impl App {
    /// The webcam's card: add one, or pick the camera and its format, open
    /// its own settings, reset where it sits, remove it.
    pub(crate) fn webcam_card(&mut self, ui: &mut egui::Ui) {
        let frame = capture::preview::latest();
        let status = capture::webcam::status();
        let frame_aspect = frame.as_deref().map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);

        // Just added, or a new camera or format: once its shape is known, give
        // the box that shape (same width, same top-left; crop kept).
        if self.webcam_view.fit_pending {
            let live = match &status {
                Status::Live { width, height } => Some((*width, *height)),
                _ => None,
            };
            let wanted = self.settings.webcam.as_ref().and_then(|w| w.format).map(|f| (f.width, f.height));
            // The new camera or format is open: its size is the one asked
            // for, or (Automatic, another camera) not the one before.
            let reopened = live.is_some_and(|l| match wanted {
                Some(want) => l == want,
                None => Some(l) != self.webcam_view.fit_from,
            });
            if !reopened {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
            if let (true, Status::Live { width, height }, Some(w)) = (reopened, &status, self.settings.webcam.as_mut()) {
                let p = &mut w.placement;
                let (cw, ch) = if p.turns % 2 == 1 { (*height, *width) } else { (*width, *height) };
                let camera_aspect = cw as f32 / ch.max(1) as f32;
                let visible = (1.0 - p.crop[0] - p.crop[2]).max(0.05) / (1.0 - p.crop[1] - p.crop[3]).max(0.05);
                p.h = p.w * frame_aspect / (camera_aspect * visible);
                p.frame_aspect = Some(frame_aspect);
                self.webcam_view.fit_pending = false;
                self.webcam_view.fit_from = None;
            }
        }

        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let cam = self.settings.webcam.clone();
        // The heading, with the webcam's switch (works while capturing too:
        // off only hides it, and its place, size, crop and format are kept).
        crate::sources_ui::column_head(ui, "Webcam", |ui| {
            let mut on = cam.as_ref().is_some_and(|c| c.enabled);
            let tip = if on { "Turn the webcam off: it leaves your clips, and its place, size, crop and format are kept" } else { "Put your webcam in your clips" };
            if crate::ui_kit::switch(ui, &mut on).on_hover_text(tip).changed() {
                match self.settings.webcam.as_mut() {
                    Some(w) => w.enabled = on,
                    // None set up yet: the first camera, in the corner.
                    None => {
                        self.webcam_view.cameras = capture::webcam::list_cameras();
                        if let Some(c) = self.webcam_view.cameras.first().cloned() {
                            self.settings.webcam = Some(WebcamCfg {
                                id: c.id,
                                name: c.name,
                                format: None,
                                placement: PlacementCfg::laid_out(Placement::default_for(16.0 / 9.0, frame_aspect), frame_aspect),
                                enabled: true,
                            });
                            self.webcam_view.ask_fit();
                        } else {
                            self.toast_error("No cameras found");
                        }
                    }
                }
            }
            if cam.as_ref().is_some_and(|c| c.enabled) {
                let (color, text) = match &status {
                    Status::Live { .. } => (pal.text3, "In your clips"),
                    Status::Opening => (pal.text3, "Opening…"),
                    Status::Unavailable(_) => (pal.warn, "Not working"),
                    _ => (pal.text3, ""),
                };
                ui.label(theme::text(text, 12.5, W::Regular).color(color));
            }
        });

        let Some(cam) = cam.filter(|c| c.enabled) else {
            crate::sources_ui::group(ui, "webcam", |ui| {
                ui.label(theme::text("Put your camera in your clips, placed and sized on the preview.", 13.0, W::Regular).color(pal.text3));
            });
            return;
        };
        crate::sources_ui::group(ui, "webcam", |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if let Status::Unavailable(why) = &status {
                let mut why = why.clone();
                if let Some(first) = why.get(..1) {
                    why.replace_range(..1, &first.to_uppercase());
                }
                ui.add(egui::Label::new(theme::text(format!("{why}."), 12.5, W::Regular).color(pal.warn)).wrap());
                ui.add_space(8.0);
            }
            let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.horizontal(|ui| {
                    ui.set_min_height(42.0);
                    ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
                });
            };
            row(ui, "Camera", &mut |ui| {
                let r = crate::ui_kit::select(ui, &cam.name, 180.0);
                if r.clicked() {
                    self.webcam_view.cameras = capture::webcam::list_cameras();
                }
                crate::ui_kit::menu_popup(&r, false, 240.0, |ui| {
                    for c in self.webcam_view.cameras.clone() {
                        if Item::new(c.name.clone()).check(c.id == cam.id).show(ui).clicked() && c.id != cam.id {
                            if let Some(w) = self.settings.webcam.as_mut() {
                                w.id = c.id;
                                w.name = c.name;
                                w.format = None; // that camera's own formats
                            }
                            self.webcam_view.ask_fit();
                        }
                    }
                    if self.webcam_view.cameras.is_empty() {
                        crate::ui_kit::menu_head(ui, "No cameras found.");
                    }
                    crate::ui_kit::menu_sep(ui);
                    if Item::new("Remove the webcam").icon(icon::TRASH).danger().tip("Take it out of your clips, and forget its place").show(ui).clicked() {
                        self.settings.webcam = None;
                    }
                });
            });
            // Where there's no Camera page (its settings are the driver's
            // window, or none), the format is picked here.
            if !capture::webcam::HAS_CONTROLS {
                let formats = capture::webcam::formats(&cam.id);
                let current = cam.format.map(capture::webcam::Format::from);
                let mut pick: Option<Option<capture::webcam::Format>> = None;
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Resolution", &mut |ui| {
                    let label = current.map_or_else(|| "Automatic".to_owned(), |f| size_label(f.width, f.height));
                    let r = crate::ui_kit::select(ui, &label, 180.0);
                    crate::ui_kit::menu_popup(&r, false, 250.0, |ui| {
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            if Item::new("Automatic").check(current.is_none()).tip("The sharpest picture up to 1080p at 30 fps or more").show(ui).clicked() {
                                pick = Some(None);
                            }
                            if formats.is_empty() {
                                crate::ui_kit::menu_head(ui, "The camera's sizes show once it's open.");
                            }
                            for (w, h) in sizes(&formats) {
                                let on = current.is_some_and(|f| (f.width, f.height) == (w, h));
                                let fastest = formats.iter().filter(|f| (f.width, f.height) == (w, h)).map(|f| f.fps()).fold(0.0, f32::max);
                                if Item::new(size_label(w, h)).value(format!("up to {}", fps_label(fastest))).check(on).show(ui).clicked() && !on {
                                    pick = Some(rate_for(&formats, (w, h), current));
                                }
                            }
                        });
                    });
                });
                if let Some(f) = current {
                    crate::ui_kit::divider(ui, 0.0);
                    row(ui, "Frame rate", &mut |ui| {
                        let r = crate::ui_kit::select(ui, &fps_label(f.fps()), 180.0);
                        crate::ui_kit::menu_popup(&r, false, 200.0, |ui| {
                            for o in formats.iter().filter(|o| (o.width, o.height) == (f.width, f.height)) {
                                if Item::new(fps_label(o.fps())).check(*o == f).show(ui).clicked() && *o != f {
                                    pick = Some(Some(*o));
                                }
                            }
                        });
                    });
                }
                if let Some(p) = pick {
                    if let Some(w) = self.settings.webcam.as_mut() {
                        w.format = p.map(Into::into);
                    }
                    self.webcam_view.ask_fit();
                }
            }
            // Its own settings: a page of them (Linux), else the driver's window.
            if capture::webcam::HAS_SETTINGS || capture::webcam::HAS_CONTROLS {
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Camera settings", &mut |ui| {
                    let tip = "The camera's own settings: exposure, focus, white balance. They apply right away, in every app.";
                    let open = if capture::webcam::HAS_CONTROLS {
                        Btn::normal("Adjust").trail(icon::CARET_RIGHT).small().tip(tip).show(ui).clicked()
                    } else {
                        Btn::normal("Open…").small().tip(tip).show(ui).clicked()
                    };
                    if open {
                        if capture::webcam::HAS_CONTROLS {
                            self.camera_panel = Some(CameraPanel::open(&cam.id));
                        } else {
                            capture::webcam::open_settings(&cam.id, &cam.name);
                        }
                    }
                });
            }
            // Where it sits in the clips: set on the preview, reset here.
            crate::ui_kit::divider(ui, 0.0);
            row(ui, "Position", &mut |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if Btn::normal("Reset").small().tip("Back to the bottom-right corner, uncropped").show(ui).clicked() {
                    let camera_aspect = match status {
                        Status::Live { width, height } => width as f32 / height.max(1) as f32,
                        _ => 16.0 / 9.0,
                    };
                    if let Some(w) = self.settings.webcam.as_mut() {
                        w.placement = PlacementCfg::laid_out(Placement::default_for(camera_aspect, frame_aspect), frame_aspect);
                    }
                }
                ui.label(theme::text("Drag it on the preview", 12.5, W::Regular).color(pal.text3)).on_hover_text(
                    "Drag a corner or edge to resize it, and past the opposite side to flip it. Hold Alt to crop, Ctrl to stop snapping. Right-click it to mirror, flip or turn it a quarter.",
                );
            });
        });
    }

    /// The webcam's box on the preview, its handles, and dragging them.
    pub(crate) fn webcam_on_preview(&mut self, ui: &mut egui::Ui, frame_rect: Rect, response: &egui::Response) {
        let Some(cam) = self.settings.webcam.as_ref().filter(|c| c.enabled) else { return };
        let p: Placement = cam.placement.into();
        let to_screen = |x: f32, y: f32| Pos2::new(frame_rect.left() + x * frame_rect.width(), frame_rect.top() + y * frame_rect.height());
        let bx = Rect::from_min_max(to_screen(p.x, p.y), to_screen(p.x + p.w, p.y + p.h));
        let modifiers = ui.input(|i| i.modifiers);
        let pointer = ui.input(|i| i.pointer.hover_pos());

        // What's under the pointer: a handle, else the box itself.
        let hit = |at: Pos2| -> Option<Handle> {
            Handle::ALL
                .into_iter()
                .find(|h| h.pos(bx).distance(at) <= HANDLE)
                .or_else(|| bx.expand(2.0).contains(at).then_some(Handle::Move))
        };

        // A drag only starts once the pointer has moved a little, so what was
        // grabbed is judged by where the button went down, not where it is now.
        if response.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
            if let Some((h, at)) = origin.and_then(|at| hit(at).map(|h| (h, at))) {
                self.webcam_view.drag = Some((h, at, p));
            }
        }
        if let Some((handle, from, start)) = self.webcam_view.drag {
            if response.dragged() {
                if let Some(now) = response.interact_pointer_pos() {
                    let mods = Mods { crop: modifiers.alt, snap: !modifiers.ctrl };
                    let (dx, dy) = ((now.x - from.x) / frame_rect.width(), (now.y - from.y) / frame_rect.height());
                    let moved = drag(start, handle, dx, dy, mods);
                    let aspect = frame_rect.width() / frame_rect.height().max(1.0);
                    if let Some(w) = self.settings.webcam.as_mut() {
                        w.placement = PlacementCfg::laid_out(moved, aspect);
                    }
                }
                ui.ctx().set_cursor_icon(if handle == Handle::Move { CursorIcon::Grabbing } else { handle.cursor() });
            }
            if response.drag_stopped() || !response.dragged() && !ui.input(|i| i.pointer.any_down()) {
                self.webcam_view.drag = None;
            }
        } else if let Some(h) = pointer.filter(|at| frame_rect.contains(*at)).and_then(hit) {
            ui.ctx().set_cursor_icon(h.cursor());
        }

        // Right-click on it: flip or turn it. The menu stays tied to a click
        // that landed on the box.
        if response.secondary_clicked() && ui.input(|i| i.pointer.interact_pos()).is_some_and(|at| hit(at).is_some()) {
            self.webcam_view.menu = true;
        }
        if self.webcam_view.menu {
            let mut keep = false;
            response.context_menu(|ui| {
                keep = true;
                let aspect = frame_rect.width() / frame_rect.height().max(1.0);
                let mut change = |f: &dyn Fn(Placement) -> Placement| {
                    if let Some(w) = self.settings.webcam.as_mut() {
                        w.placement = PlacementCfg::laid_out(f(w.placement.into()), aspect);
                    }
                };
                if ui.button("Turn clockwise").clicked() {
                    change(&|p| contain(p.turn(1, aspect)));
                    ui.close();
                }
                if ui.button("Turn anticlockwise").clicked() {
                    change(&|p| contain(p.turn(-1, aspect)));
                    ui.close();
                }
                ui.separator();
                if ui.button("Mirror").on_hover_text("Flip left to right").clicked() {
                    change(&|p| Placement { flip_h: !p.flip_h, ..p });
                    ui.close();
                }
                if ui.button("Flip upside down").clicked() {
                    change(&|p| Placement { flip_v: !p.flip_v, ..p });
                    ui.close();
                }
            });
            self.webcam_view.menu = keep;
        }

        // The box and its handles, over the picture.
        let p: Placement = self.settings.webcam.as_ref().map_or(p, |c| c.placement.into());
        let bx = Rect::from_min_max(to_screen(p.x, p.y), to_screen(p.x + p.w, p.y + p.h));
        let painter = ui.painter_at(frame_rect);
        // The handles turn green while Alt is held (they'll crop); each side of
        // the box is green only if it's cropped.
        let color = if modifiers.alt { CROP } else { ACCENT };
        // Outlined only while it's held or the pointer is over it.
        let bx_hit = |at: Pos2| Handle::ALL.iter().any(|h| h.pos(bx).distance(at) <= HANDLE) || bx.expand(2.0).contains(at);
        let active = self.webcam_view.drag.is_some() || pointer.is_some_and(|at| frame_rect.contains(at) && bx_hit(at));
        if active {
            // The camera's own sides, as they show: a mirrored picture's left is
            // the camera's right.
            let [cl, ct, cr, cb] = p.crop.map(|c| c > 0.0);
            let (left, right) = if p.flip_h { (cr, cl) } else { (cl, cr) };
            let (top, bottom) = if p.flip_v { (cb, ct) } else { (ct, cb) };
            let side = |cropped: bool| Stroke::new(2.0, if cropped { CROP } else { ACCENT });
            painter.line_segment([bx.left_top(), bx.right_top()], side(top));
            painter.line_segment([bx.left_bottom(), bx.right_bottom()], side(bottom));
            painter.line_segment([bx.left_top(), bx.left_bottom()], side(left));
            painter.line_segment([bx.right_top(), bx.right_bottom()], side(right));
            for h in Handle::ALL {
                let c = h.pos(bx);
                let r = Rect::from_center_size(c, vec2(HANDLE, HANDLE));
                painter.rect_filled(r, 1.0, Color32::WHITE);
                painter.rect_stroke(r, 1.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, w: f32, h: f32) -> Placement {
        Placement { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0 }
    }

    const PLAIN: Mods = Mods { crop: false, snap: false };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn move_and_snap() {
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.1, -0.1, PLAIN);
        assert!(close(p.x, 0.6) && close(p.y, 0.4) && close(p.w, 0.2));
        // Right edge lands 0.005 from the frame's edge: snaps onto it.
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.295, 0.0, Mods { snap: true, ..PLAIN });
        assert!(close(p.x + p.w, 1.0));
        // Centre near the middle: snaps to it.
        let p = drag(at(0.0, 0.0, 0.2, 0.2), Handle::Move, 0.405, 0.0, Mods { snap: true, ..PLAIN });
        assert!(close(p.x + p.w / 2.0, 0.5));
    }

    /// The box stays in the frame: moving stops at the edge, growing stops at
    /// it (shape kept), uncropping stops at it, and a turned box is put back.
    #[test]
    fn stays_inside_the_frame() {
        let in_frame = |p: Placement| p.x >= -1e-4 && p.y >= -1e-4 && p.x + p.w <= 1.0 + 1e-4 && p.y + p.h <= 1.0 + 1e-4;
        let p = drag(at(0.7, 0.7, 0.2, 0.2), Handle::Move, 0.5, -0.9, PLAIN);
        assert!(in_frame(p) && close(p.x, 0.8) && close(p.y, 0.0));
        // From the top-left, the bottom-right corner can only grow to the frame's corner.
        let p = drag(at(0.6, 0.7, 0.2, 0.1), Handle::SE, 0.9, 0.9, PLAIN);
        assert!(in_frame(p) && close(p.x, 0.6) && close(p.y, 0.7) && close(p.w / p.h, 2.0));
        assert!(close(p.x + p.w, 1.0) || close(p.y + p.h, 1.0));
        // An edge handle grows both ways about the middle, as far as the nearer edge.
        let p = drag(at(0.1, 0.4, 0.2, 0.2), Handle::E, 0.9, 0.0, PLAIN);
        assert!(in_frame(p), "{p:?}");
        // Flipped past its anchor, toward the left edge: only as far as it.
        let p = drag(at(0.3, 0.3, 0.2, 0.2), Handle::E, -0.9, 0.0, PLAIN);
        assert!(in_frame(p) && p.flip_h, "{p:?}");
        // Uncropping the left side stops at the frame's left edge.
        let cropped = Placement { crop: [0.5, 0.0, 0.0, 0.0], ..at(0.05, 0.5, 0.2, 0.2) };
        let p = drag(cropped, Handle::W, -0.5, 0.0, Mods { crop: true, ..PLAIN });
        assert!(in_frame(p) && close(p.x, 0.0) && p.crop[0] > 0.0, "{p:?}");
        // Turned near the edge, a tall box is moved back in.
        let p = contain(at(0.8, 0.0, 0.3, 1.2));
        assert!(in_frame(p) && close(p.w / p.h, 0.25), "{p:?}");
    }

    #[test]
    fn every_handle_keeps_the_shape() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        let p = drag(start, Handle::SE, 0.1, 0.0, PLAIN);
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y, 0.5));
        // From the top-left corner: the bottom-right stays put.
        let p = drag(start, Handle::NW, -0.1, 0.0, PLAIN);
        assert!(close(p.x + p.w, 0.7) && close(p.y + p.h, 0.6) && close(p.w / p.h, 2.0));
        // An edge scales the whole picture too, from the opposite edge, centred
        // on the other axis.
        let p = drag(start, Handle::E, 0.1, 0.3, PLAIN);
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y + p.h / 2.0, 0.55));
        let p = drag(start, Handle::N, 0.0, -0.05, PLAIN);
        assert!(close(p.h, 0.15) && close(p.w, 0.3) && close(p.y + p.h, 0.6) && close(p.x + p.w / 2.0, 0.6));
        assert!(!p.flip_h && !p.flip_v);
    }

    #[test]
    fn dragging_past_the_other_side_flips() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        // Right edge dragged 0.3 left: past the left edge, the same size, mirrored.
        let p = drag(start, Handle::E, -0.4, 0.0, PLAIN);
        assert!(p.flip_h && !p.flip_v);
        assert!(close(p.w, 0.2) && close(p.x + p.w, 0.5));
        // Bottom-right corner up past the top only: flipped vertically.
        let p = drag(start, Handle::SE, 0.0, -0.2, PLAIN);
        assert!(p.flip_v && !p.flip_h && close(p.y + p.h, 0.5));
        // Past both: both.
        let p = drag(start, Handle::SE, -0.4, -0.2, PLAIN);
        assert!(p.flip_h && p.flip_v);
        // Flipping back undoes it.
        let back = drag(p, Handle::NW, 0.4, 0.2, PLAIN);
        assert!(!back.flip_h && !back.flip_v);
    }

    #[test]
    fn crop_keeps_the_scale() {
        // Box 0.2 wide shows the whole camera: frame fraction 0.2 per camera width.
        let start = at(0.5, 0.5, 0.2, 0.2);
        let crop = Mods { crop: true, ..PLAIN };
        let p = drag(start, Handle::W, 0.05, 0.0, crop);
        assert!(close(p.crop[0], 0.25) && close(p.x, 0.55) && close(p.w, 0.15));
        // Dragging the right edge outward past the uncropped size doesn't uncrop
        // beyond the picture.
        let p = drag(start, Handle::E, 0.1, 0.0, crop);
        assert!(close(p.crop[2], 0.0) && close(p.w, 0.2));
        // Crop the right, then pull it back: the box grows back.
        let cropped = drag(start, Handle::E, -0.05, 0.0, crop);
        assert!(close(cropped.crop[2], 0.25) && close(cropped.w, 0.15));
        let back = drag(cropped, Handle::E, 0.05, 0.0, crop);
        assert!(close(back.crop[2], 0.0) && close(back.w, 0.2));
    }

    #[test]
    fn crop_follows_a_flipped_picture() {
        // Mirrored: the visible left side is the camera's right side.
        let start = Placement { flip_h: true, ..at(0.5, 0.5, 0.2, 0.2) };
        let p = drag(start, Handle::W, 0.05, 0.0, Mods { crop: true, ..PLAIN });
        assert!(close(p.crop[2], 0.25) && close(p.crop[0], 0.0) && close(p.x, 0.55));
    }
}

// ---------------------------------------------------------------------------
// Camera page (Linux)
// ---------------------------------------------------------------------------

/// The Camera page's state: the camera's settings as last read, and its picture.
pub(crate) struct CameraPanel {
    device: String,
    controls: Vec<CameraControl>,
    picture: Option<egui::TextureHandle>,
    seq: u64,
    /// The camera's real frame rate, measured: new pictures (not repeats)
    /// since when, from how many, and the last second's.
    new_pictures: u64,
    counted: Option<(std::time::Instant, u64)>,
    fps: Option<f32>,
    /// Cropping on the picture: the whole of it shown, its edges dragged.
    cropping: bool,
    /// The crop's edge (or all of it) being dragged.
    crop_drag: Option<CropGrip>,
}

/// What of the crop is being dragged, on the picture as shown.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CropGrip {
    Left,
    Top,
    Right,
    Bottom,
    /// All of it, moved.
    Move,
}

impl CameraPanel {
    /// Start cropping on the picture (or not).
    pub(crate) fn set_cropping(&mut self, on: bool) {
        self.cropping = on;
    }

    pub(crate) fn open(device: &str) -> Self {
        Self { device: device.to_owned(), controls: capture::webcam::camera_controls(device), picture: None, seq: 0, new_pictures: 0, counted: None, fps: None, cropping: false, crop_drag: None }
    }
}

/// V4L2's control ids, for the ones grouped and named here.
mod v4l2 {
    pub const BRIGHTNESS: u32 = 0x980900;
    pub const CONTRAST: u32 = 0x980901;
    pub const SATURATION: u32 = 0x980902;
    pub const HUE: u32 = 0x980903;
    pub const AUTO_WHITE_BALANCE: u32 = 0x98090c;
    pub const GAMMA: u32 = 0x980910;
    pub const GAIN: u32 = 0x980913;
    pub const POWER_LINE: u32 = 0x980918;
    pub const WHITE_BALANCE: u32 = 0x98091a;
    pub const SHARPNESS: u32 = 0x98091b;
    pub const BACKLIGHT: u32 = 0x98091c;
    pub const AUTO_EXPOSURE: u32 = 0x9a0901;
    pub const EXPOSURE: u32 = 0x9a0902;
    pub const EXPOSURE_DYNAMIC_FPS: u32 = 0x9a0903;
    pub const PAN: u32 = 0x9a0908;
    pub const TILT: u32 = 0x9a0909;
    pub const FOCUS: u32 = 0x9a090a;
    pub const AUTO_FOCUS: u32 = 0x9a090c;
    pub const ZOOM: u32 = 0x9a090d;
    /// `V4L2_EXPOSURE_MANUAL`, among the automatic exposure menu's items.
    pub const EXPOSURE_MANUAL: i64 = 1;
}

/// One row of the panel.
#[derive(Debug, PartialEq)]
enum PanelRow<'a> {
    /// An automatic mode as a switch, and the setting it takes over (shown while it's off).
    Auto { label: &'static str, auto: &'a CameraControl, manual: Option<(&'static str, &'a CameraControl)> },
    Control { label: String, control: &'a CameraControl },
}

/// The heading of the camera's own settings, not named here.
const OTHER: &str = "Other";

/// The camera's controls in groups (heading, rows), all shown: named for
/// people, each automatic mode with the setting it takes over, and the
/// camera's own under Other.
fn panel_groups(controls: &[CameraControl]) -> Vec<(&'static str, Vec<PanelRow<'_>>)> {
    use v4l2::*;
    let find = |id: u32| controls.iter().find(|c| c.id == id);
    let named = |id: u32, label: &str| find(id).map(|control| PanelRow::Control { label: label.to_owned(), control });
    let auto = |auto: u32, label: &'static str, manual: u32, manual_label: &'static str| {
        // No automatic mode: the manual setting stands alone.
        find(auto).map(|a| PanelRow::Auto { label, auto: a, manual: find(manual).map(|m| (manual_label, m)) }).or_else(|| named(manual, manual_label))
    };
    let groups: Vec<(&'static str, Vec<Option<PanelRow>>)> = vec![
        (
            "Image",
            vec![named(BRIGHTNESS, "Brightness"), named(CONTRAST, "Contrast"), named(SATURATION, "Saturation"), named(SHARPNESS, "Sharpness"), named(HUE, "Hue"), named(GAMMA, "Gamma")],
        ),
        (
            "Exposure",
            vec![
                auto(AUTO_EXPOSURE, "Automatic exposure", EXPOSURE, "Exposure time"),
                named(GAIN, "Gain"),
                named(BACKLIGHT, "Backlight compensation"),
                named(EXPOSURE_DYNAMIC_FPS, "Lower frame rate in the dark"),
                named(POWER_LINE, "Flicker (mains frequency)"),
            ],
        ),
        ("White balance", vec![auto(AUTO_WHITE_BALANCE, "Automatic white balance", WHITE_BALANCE, "Temperature")]),
        ("Focus", vec![auto(AUTO_FOCUS, "Autofocus", FOCUS, "Focus distance")]),
        ("Zoom and framing", vec![named(ZOOM, "Zoom"), named(PAN, "Pan"), named(TILT, "Tilt")]),
        (OTHER, Vec::new()),
    ];
    let known = [BRIGHTNESS, CONTRAST, SATURATION, HUE, AUTO_WHITE_BALANCE, GAMMA, GAIN, POWER_LINE, WHITE_BALANCE, SHARPNESS, BACKLIGHT, AUTO_EXPOSURE, EXPOSURE, EXPOSURE_DYNAMIC_FPS, PAN, TILT, FOCUS, AUTO_FOCUS, ZOOM];
    let own = controls.iter().filter(|c| !known.contains(&c.id)).map(|c| Some(PanelRow::Control { label: c.name.clone(), control: c }));
    groups
        .into_iter()
        .map(|(h, mut rows)| {
            if h == OTHER {
                rows.extend(own.clone());
            }
            (h, rows.into_iter().flatten().collect::<Vec<_>>())
        })
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
}

/// A control's widget, the same width for each kind so they line up: a
/// slider with its value, a switch (also for a slider that's only 0 or 1),
/// or a menu. What's changed goes in `set`.
fn control_widget(ui: &mut egui::Ui, c: &CameraControl, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    match &c.kind {
        ControlKind::Bool | ControlKind::Int { min: 0, max: 1, .. } => {
            let mut on = c.value != 0;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                set.push((c.id, i64::from(on)));
            }
        }
        ControlKind::Int { min, max, step } => {
            // Right to left: the value, then the slider.
            let shown = if c.id == v4l2::WHITE_BALANCE { format!("{} K", c.value) } else { c.value.to_string() };
            ui.add_sized([46.0, 20.0], egui::Label::new(theme::text(&shown, 12.5, W::Regular).color(pal.text3)));
            let span = (max - min).max(1) as f32;
            let mut v = (c.value - min) as f32 / span;
            let slider = crate::transport::slider(ui, &mut v, 150.0);
            if slider.changed() {
                let raw = *min as f32 + v * span;
                let stepped = (min + ((raw - *min as f32) / *step as f32).round() as i64 * step).clamp(*min, *max);
                if stepped != c.value {
                    set.push((c.id, stepped));
                }
            }
            if let Some(d) = arrow_steps(ui, &slider) {
                let to = (c.value + d * step).clamp(*min, *max);
                if to != c.value {
                    set.push((c.id, to));
                }
            }
            if slider.double_clicked() {
                set.push((c.id, c.default));
            }
            slider.on_hover_text("Double-click for the camera's default");
        }
        ControlKind::Menu(items) => {
            let current = items.iter().find(|(v, _)| *v == c.value).map_or_else(|| c.value.to_string(), |(_, n)| n.clone());
            let r = crate::ui_kit::select(ui, &current, 150.0);
            crate::ui_kit::menu_popup(&r, false, 200.0, |ui| {
                for (v, name) in items {
                    if crate::ui_kit::Item::new(name.clone()).check(*v == c.value).show(ui).clicked() {
                        set.push((c.id, *v));
                    }
                }
            });
        }
    }
}

/// Whether an automatic mode is on, and the value that turns it on or off.
fn auto_on(c: &CameraControl) -> bool {
    match c.kind {
        // Automatic exposure is a menu: anything but manual.
        ControlKind::Menu(_) => c.value != v4l2::EXPOSURE_MANUAL,
        _ => c.value != 0,
    }
}

fn auto_value(c: &CameraControl, on: bool) -> i64 {
    match &c.kind {
        ControlKind::Menu(items) if on => {
            if c.default != v4l2::EXPOSURE_MANUAL {
                c.default
            } else {
                items.iter().map(|(v, _)| *v).find(|v| *v != v4l2::EXPOSURE_MANUAL).unwrap_or(c.default)
            }
        }
        ControlKind::Menu(_) => v4l2::EXPOSURE_MANUAL,
        _ => i64::from(on),
    }
}

impl App {
    /// The Camera page, in Sources' place while it's open: the camera's own
    /// picture, held still, beside (or above) its settings, which scroll.
    pub(crate) fn camera_page(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        use crate::ui_kit::Btn;
        // Another camera picked: its settings; none: closed.
        match (&self.settings.webcam, &self.camera_panel) {
            (Some(w), Some(p)) if w.id != p.device => self.camera_panel = Some(CameraPanel::open(&w.id)),
            (None, _) => self.camera_panel = None,
            _ => {}
        }
        if self.camera_panel.is_none() {
            return;
        }
        // Back to Sources: the arrow, or Esc (as Settings' own pages).
        if !crate::ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.camera_panel = None;
            return;
        }
        let mut back = false;
        self.page_head(ui, "", |_, ui| {
            back = Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Sources  (Esc)").show(ui).clicked();
            let p = theme::pal(ui);
            ui.label(theme::text("Camera", 20.0, W::Semibold).color(p.text));
        });
        let name = self.settings.webcam.as_ref().map(|w| w.name.clone()).unwrap_or_default();
        let current = self.settings.webcam.as_ref().and_then(|w| w.format).map(capture::webcam::Format::from);
        let mut pick: Option<Option<capture::webcam::Format>> = None;
        // How it's turned and mirrored in the clips (the webcam's placement).
        let orient = self.settings.webcam.as_ref().map_or_else(Orientation::default, |w| Orientation { flip_h: w.placement.flip_h, flip_v: w.placement.flip_v, turns: w.placement.turns });
        let mut reorient: Option<Orientation> = None;
        // Its crop (the camera's sides cut off), and a new one.
        let crop = self.settings.webcam.as_ref().map_or([0.0; 4], |w| w.placement.crop);
        let mut recrop: Option<[f32; 4]> = None;
        // The frame rate the camera runs at: as picked, or Automatic's pick.
        let fps = current.or_else(|| self.camera_panel.as_ref().and_then(|p| capture::webcam::formats(&p.device).into_iter().max_by_key(|f| f.auto_rank()))).map(|f| f.fps());
        let Some(panel) = self.camera_panel.as_mut() else { return };
        let mut set: Vec<(u32, i64)> = Vec::new();
        let mut reset = false;
        ui.add_space(4.0);
        let w = ui.available_width();
        let h = ui.available_height();
        if w >= crate::sources_ui::TWO_COLUMNS {
            let col = (w - 28.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.allocate_ui_with_layout(vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    ui.add_space(4.0);
                    picture_side(ui, panel, &name, fps, orient, crop, h * 0.7, &mut reset, &mut recrop);
                });
                ui.allocate_ui_with_layout(vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    crate::ui_kit::scroll(ui, "camera_controls", |ui| {
                        format_view(ui, &panel.device, current, &mut pick);
                        orientation_view(ui, orient, &mut reorient, &mut panel.cropping, crop != [0.0; 4], &mut recrop);
                        controls_view(ui, panel, fps, &mut set);
                        ui.add_space(28.0);
                    });
                });
            });
        } else {
            picture_side(ui, panel, &name, fps, orient, crop, h * 0.4, &mut reset, &mut recrop);
            crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
            crate::ui_kit::scroll(ui, "camera_controls", |ui| {
                format_view(ui, &panel.device, current, &mut pick);
                orientation_view(ui, orient, &mut reorient, &mut panel.cropping, crop != [0.0; 4], &mut recrop);
                controls_view(ui, panel, fps, &mut set);
                ui.add_space(28.0);
            });
        }

        let device = panel.device.clone();
        if reset {
            // Manual values first, while they're still taken, then the automatic modes.
            let (autos, rest): (Vec<_>, Vec<_>) = panel.controls.iter().filter(|c| c.value != c.default).partition(|c| !matches!(c.kind, ControlKind::Int { .. }));
            set = rest.into_iter().chain(autos).map(|c| (c.id, c.default)).collect();
            self.settings.camera_controls.remove(&device);
        }
        if !set.is_empty() {
            let saved = self.settings.camera_controls.entry(device.clone()).or_default();
            for (id, value) in &set {
                if let Err(e) = capture::webcam::set_camera_control(&device, *id, *value) {
                    eprintln!("camera setting: {e:#}");
                }
                // Kept only while it isn't the camera's default.
                match panel.controls.iter().find(|c| c.id == *id) {
                    Some(c) if c.default == *value || reset => {
                        saved.remove(id);
                    }
                    _ => {
                        saved.insert(*id, *value);
                    }
                }
            }
            if saved.is_empty() {
                self.settings.camera_controls.remove(&device);
            }
            let values = self.settings.camera_controls.get(&device).map(|m| m.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default();
            capture::webcam::remember_controls(&device, values);
            panel.controls = capture::webcam::camera_controls(&device);
        }
        if let Some(p) = pick {
            if let Some(w) = self.settings.webcam.as_mut() {
                w.format = p.map(Into::into);
            }
            self.webcam_view.ask_fit();
        }
        // Cropped: the box takes the shape of what's left (its width and
        // middle kept), so nothing more is cut off to fill it; inside the frame.
        if let (Some(c), Some(w)) = (recrop, self.settings.webcam.as_mut()) {
            let aspect = w.placement.frame_aspect.unwrap_or(16.0 / 9.0);
            let mut p: Placement = w.placement.into();
            if let capture::webcam::Status::Live { width, height } = capture::webcam::status() {
                let raw = width as f32 / height.max(1) as f32;
                let full = if p.turns % 2 == 1 { 1.0 / raw } else { raw };
                let kept = crop_shown(orient, c);
                let visible = full * kept.width() / kept.height().max(0.01);
                let (cx, cy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
                p.h = p.w * aspect / visible;
                p.x = cx - p.w / 2.0;
                p.y = cy - p.h / 2.0;
            }
            p.crop = c;
            w.placement = PlacementCfg::laid_out(contain(p), aspect);
        }
        // Turned or mirrored: the placement, as the preview's right-click does
        // it (a quarter turn swaps the box's sides).
        if let (Some(o), Some(w)) = (reorient, self.settings.webcam.as_mut()) {
            let aspect = w.placement.frame_aspect.unwrap_or(16.0 / 9.0);
            let mut p: Placement = w.placement.into();
            p.flip_h = o.flip_h;
            p.flip_v = o.flip_v;
            // Turned a quarter at a time, as seen (`Placement::turn`), from how
            // it's seen now to how it's wanted.
            let seen = Orientation { flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns }.seen_turns();
            match (o.turns as i8 - seen as i8).rem_euclid(4) {
                1 => p = p.turn(1, aspect),
                2 => p = p.turn(1, aspect).turn(1, aspect),
                3 => p = p.turn(-1, aspect),
                _ => {}
            }
            w.placement = PlacementCfg::laid_out(contain(p), aspect);
        }
        if back {
            self.camera_panel = None;
        }
    }
}

/// The next exposure time up (`up`) or down from `value`, a step on.
fn exposure_step(value: i64, up: bool, step: i64, min: i64, max: i64) -> i64 {
    (value + if up { step } else { -step }).clamp(min, max)
}

/// Arrow keys on a camera slider: clicking or dragging it gives it the keys,
/// then ← and → step it, ten at a time with Shift (↑ and ↓ move between the
/// page's controls, as egui has them). How many steps, if any were pressed.
fn arrow_steps(ui: &egui::Ui, slider: &egui::Response) -> Option<i64> {
    if slider.clicked() || slider.drag_started() {
        slider.request_focus();
    }
    if !slider.has_focus() {
        return None;
    }
    let mut d = 0;
    ui.input_mut(|i| {
        let shift = i.modifiers.shift;
        let by = if shift { 10 } else { 1 };
        let mods = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
        for (key, sign) in [(egui::Key::ArrowLeft, -1), (egui::Key::ArrowRight, 1)] {
            while i.consume_key(mods, key) {
                d += sign * by;
            }
        }
    });
    (d != 0).then_some(d)
}

/// Film's guides for an exposure time at `fps`, in V4L2's units of 100 µs:
/// the 180° shutter (half a frame, for motion blur that looks natural), and
/// the whole frame (longer, and the frame rate drops).
fn exposure_guides(fps: f32) -> (i64, i64) {
    let frame = 10_000.0 / fps as f64;
    ((frame / 2.0).round() as i64, frame.round() as i64)
}

/// The exposure time: − and + (or the arrow keys) to step it finely, to
/// where lights' flicker leaves no bands, found by eye; the slider; its time in ms.
fn exposure_widget(ui: &mut egui::Ui, c: &CameraControl, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let ControlKind::Int { min, max, step } = c.kind else { return control_widget(ui, c, set) };
    let pal = theme::pal(ui);
    // Right to left: the time, +, the slider, −.
    ui.add_sized([56.0, 20.0], egui::Label::new(theme::text(&format!("{:.1} ms", c.value as f64 / 10.0), 12.5, W::Regular).color(pal.text3)));
    let next = |up: bool| Some(exposure_step(c.value, up, step, min, max)).filter(|v| *v != c.value);
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::PLUS).small().tip("Longer").show(ui).clicked()
        && let Some(v) = next(true)
    {
        set.push((c.id, v));
    }
    // On a curve: the times worth having get the slider's room, not the
    // shortest (linear squeezed them into its first fifth; stops gave the
    // too-dark ones as much as the rest).
    let mut v = curve_pos(c.value, min, max);
    let slider = crate::transport::slider(ui, &mut v, 130.0);
    if slider.changed() {
        let to = curve_value(v, min, max);
        if to != c.value {
            set.push((c.id, to));
        }
    }
    if let Some(d) = arrow_steps(ui, &slider) {
        let to = (c.value + d * step).clamp(min, max);
        if to != c.value {
            set.push((c.id, to));
        }
    }
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::MINUS).small().tip("Shorter").show(ui).clicked()
        && let Some(v) = next(false)
    {
        set.push((c.id, v));
    }
}

/// How steep the exposure slider's curve is: near linear at the bottom (the
/// shortest times, too dark indoors, get little room), each step up longer.
/// 5 puts a C922's 0.3–205 ms at: 1 ms 8% along, 16.7 ms half way, 100 ms 85%.
const EXPOSURE_CURVE: f64 = 5.0;

/// Where `value` sits along the exposure slider from `min` to `max` (0 to 1).
fn curve_pos(value: i64, min: i64, max: i64) -> f32 {
    let t = (value - min) as f64 / (max - min).max(1) as f64;
    ((t * (EXPOSURE_CURVE.exp() - 1.0)).ln_1p() / EXPOSURE_CURVE).clamp(0.0, 1.0) as f32
}

/// The value at `pos` (0 to 1) along the exposure slider from `min` to `max`.
fn curve_value(pos: f32, min: i64, max: i64) -> i64 {
    let t = (EXPOSURE_CURVE * pos as f64).exp_m1() / EXPOSURE_CURVE.exp_m1();
    (min + (t * (max - min) as f64).round() as i64).clamp(min, max)
}

/// The Camera page's still side: the picture, which camera it is and in what
/// size, and resetting its settings.
#[allow(clippy::too_many_arguments)]
fn picture_side(ui: &mut egui::Ui, panel: &mut CameraPanel, name: &str, fps: Option<f32>, orient: Orientation, crop: [f32; 4], max_h: f32, reset: &mut bool, recrop: &mut Option<[f32; 4]>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    if let Some(c) = picture_view(ui, panel, orient, crop, max_h) {
        *recrop = Some(c);
    }
    crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
    let status = match (capture::webcam::status(), panel.fps) {
        (capture::webcam::Status::Live { width, height }, Some(fps)) => format!("{name} · {width}×{height} · {fps:.0} new pictures/s"),
        (capture::webcam::Status::Live { width, height }, None) => format!("{name} · {width}×{height}"),
        _ => name.to_owned(),
    };
    ui.horizontal(|ui| {
        ui.label(theme::text(&status, 13.0, W::Regular).color(pal.text2));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !panel.controls.is_empty() && crate::ui_kit::Btn::ghost("Reset to defaults").small().tip("Back to the camera's own defaults").show(ui).clicked() {
                *reset = true;
            }
        });
    });
    // Slower than its format: said, with what can cause it (the camera
    // doesn't say which).
    if let Some(why) = slowdown(panel.fps, fps) {
        ui.add(egui::Label::new(theme::text(why, 12.5, W::Regular).color(pal.warn)).wrap());
    }
    ui.add(egui::Label::new(theme::text("Changes apply right away, in every app, and are put back each time HasteClip opens this camera.", 12.0, W::Regular).color(pal.text3)).wrap());
}

/// When the camera sends clearly fewer new pictures (`measured` a second)
/// than its format's `fps`: saying so, and what can cause it. Which one, the
/// camera doesn't say, and its own limits aren't a frame's length (a C922 at
/// 60 fps kept up at 17.2 ms and slowed at 16.8 in dimmer light).
fn slowdown(measured: Option<f32>, fps: Option<f32>) -> Option<String> {
    let (measured, fps) = (measured?, fps?);
    (measured < fps * 0.85).then(|| {
        format!("Sending {measured:.0} of {fps:.0} pictures a second. Cameras slow down when too little light reaches them (brighter light in front of you helps; gain doesn't), or when the exposure is longer than they can fit in a frame.")
    })
}

/// The camera's own picture, live, as wide as there's room for and at most
/// `max_h` tall.
fn picture_view(ui: &mut egui::Ui, panel: &mut CameraPanel, orient: Orientation, crop: [f32; 4], max_h: f32) -> Option<[f32; 4]> {
    let pal = crate::theme::pal(ui);
    let w = ui.available_width();
    if let Some(p) = capture::webcam::camera_picture((w * ui.ctx().pixels_per_point()) as u32, panel.seq) {
        let image = egui::ColorImage::from_rgba_unmultiplied([p.width as usize, p.height as usize], &p.rgba);
        match &mut panel.picture {
            Some(t) if t.size() == [p.width as usize, p.height as usize] => t.set(image, egui::TextureOptions::LINEAR),
            t => *t = Some(ui.ctx().load_texture("camera page", image, egui::TextureOptions::LINEAR)),
        }
        panel.seq = p.seq;
        panel.new_pictures = p.new_pictures;
    }
    // How many new pictures (not the camera repeating one, as it does with a
    // long exposure) came in the last second: its real frame rate.
    let now = std::time::Instant::now();
    match panel.counted {
        Some((at, from)) if now.duration_since(at).as_secs_f32() >= 1.0 => {
            panel.fps = Some((panel.new_pictures.saturating_sub(from)) as f32 / now.duration_since(at).as_secs_f32());
            panel.counted = Some((now, panel.new_pictures));
        }
        None if panel.new_pictures > 0 => panel.counted = Some((now, panel.new_pictures)),
        _ => {}
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    // As it is in the clips: turned, then mirrored or flipped; cropped,
    // unless the crop's being set (then all of it, the rest dimmed).
    let raw = panel.picture.as_ref().map_or(16.0 / 9.0, |t| t.size()[0] as f32 / t.size()[1].max(1) as f32);
    let sideways = orient.turns % 2 == 1;
    let full_aspect = if sideways { 1.0 / raw } else { raw };
    // The crop, as shown: its sides along and down the shown picture (0 to 1).
    let shown = crop_shown(orient, crop);
    let aspect = if panel.cropping { full_aspect } else { full_aspect * shown.width() / shown.height().max(0.01) };
    let pw = w.min(max_h.max(120.0) * aspect);
    let mut changed = None;
    ui.vertical_centered(|ui| {
        let sense = if panel.cropping { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(vec2(pw, pw / aspect), sense);
        let Some(t) = &panel.picture else {
            ui.painter().rect_filled(rect, crate::ui_kit::RADIUS, pal.control);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Waiting for the camera…", egui::FontId::proportional(13.0), pal.text3);
            return;
        };
        // The whole picture's place: the rect itself while cropping, else as
        // much bigger as puts the crop on the rect (drawn clipped to it).
        let full = if panel.cropping {
            rect
        } else {
            let fw = rect.width() / shown.width().max(0.01);
            let fh = rect.height() / shown.height().max(0.01);
            egui::Rect::from_min_size(egui::pos2(rect.left() - shown.left() * fw, rect.top() - shown.top() * fh), vec2(fw, fh))
        };
        // Drawn unturned, then turned about its middle; the mirror and flip
        // are the clip's (after turning), so sideways they swap axes.
        let unturned = if sideways { egui::Rect::from_center_size(full.center(), vec2(full.height(), full.width())) } else { full };
        let (fu, fv) = if sideways { (orient.flip_v, orient.flip_h) } else { (orient.flip_h, orient.flip_v) };
        let uv = egui::Rect::from_min_max(egui::pos2(if fu { 1.0 } else { 0.0 }, if fv { 1.0 } else { 0.0 }), egui::pos2(if fu { 0.0 } else { 1.0 }, if fv { 0.0 } else { 1.0 }));
        let mut image = egui::Image::from_texture((t.id(), unturned.size())).uv(uv);
        if orient.turns % 4 != 0 {
            image = image.rotate(orient.turns as f32 * std::f32::consts::FRAC_PI_2, vec2(0.5, 0.5));
        }
        let mut clipped = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        clipped.set_clip_rect(rect.intersect(ui.clip_rect()));
        image.paint_at(&mut clipped, unturned);
        if !panel.cropping {
            return;
        }
        // The crop on the whole picture: the rest dimmed, its edges dragged.
        let at = |n: egui::Pos2| egui::pos2(rect.left() + n.x * rect.width(), rect.top() + n.y * rect.height());
        let c = egui::Rect::from_min_max(at(shown.min), at(shown.max));
        let dim = Color32::from_black_alpha(150);
        let p = ui.painter();
        p.rect_filled(egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), c.top())), 0.0, dim);
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), c.bottom()), rect.max), 0.0, dim);
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left(), c.top()), egui::pos2(c.left(), c.bottom())), 0.0, dim);
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(c.right(), c.top()), egui::pos2(rect.right(), c.bottom())), 0.0, dim);
        p.rect_stroke(c, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
        for (a, b) in [(c.left_center(), vec2(4.0, 24.0)), (c.right_center(), vec2(4.0, 24.0)), (c.center_top(), vec2(24.0, 4.0)), (c.center_bottom(), vec2(24.0, 4.0))] {
            p.rect_filled(egui::Rect::from_center_size(a, b), 2.0, Color32::WHITE);
        }
        // Which edge is near the pointer (else inside: all of it).
        let grip_at = |at: egui::Pos2| -> Option<CropGrip> {
            let near = 10.0;
            let between_y = at.y > c.top() - near && at.y < c.bottom() + near;
            let between_x = at.x > c.left() - near && at.x < c.right() + near;
            if between_y && (at.x - c.left()).abs() < near {
                Some(CropGrip::Left)
            } else if between_y && (at.x - c.right()).abs() < near {
                Some(CropGrip::Right)
            } else if between_x && (at.y - c.top()).abs() < near {
                Some(CropGrip::Top)
            } else if between_x && (at.y - c.bottom()).abs() < near {
                Some(CropGrip::Bottom)
            } else {
                c.contains(at).then_some(CropGrip::Move)
            }
        };
        let hover = resp.hover_pos().and_then(grip_at);
        let cursor = |g: CropGrip| match g {
            CropGrip::Left | CropGrip::Right => egui::CursorIcon::ResizeHorizontal,
            CropGrip::Top | CropGrip::Bottom => egui::CursorIcon::ResizeVertical,
            CropGrip::Move => egui::CursorIcon::Grab,
        };
        if resp.drag_started() {
            panel.crop_drag = resp.interact_pointer_pos().and_then(grip_at);
        }
        if let Some(g) = panel.crop_drag.or(hover) {
            ui.ctx().set_cursor_icon(cursor(g));
        }
        if let (Some(g), true) = (panel.crop_drag, resp.dragged()) {
            let d = resp.drag_delta();
            let (dx, dy) = (d.x / rect.width(), d.y / rect.height());
            let mut s = shown;
            let least = 0.1;
            match g {
                CropGrip::Left => s.min.x = (s.min.x + dx).clamp(0.0, s.max.x - least),
                CropGrip::Right => s.max.x = (s.max.x + dx).clamp(s.min.x + least, 1.0),
                CropGrip::Top => s.min.y = (s.min.y + dy).clamp(0.0, s.max.y - least),
                CropGrip::Bottom => s.max.y = (s.max.y + dy).clamp(s.min.y + least, 1.0),
                CropGrip::Move => {
                    let (mx, my) = (dx.clamp(-s.min.x, 1.0 - s.max.x), dy.clamp(-s.min.y, 1.0 - s.max.y));
                    s = s.translate(vec2(mx, my));
                }
            }
            changed = Some(crop_from_shown(orient, s));
        }
        if resp.drag_stopped() {
            panel.crop_drag = None;
        }
    });
    changed
}

/// The crop (left, top, right, bottom cut off) as the part of the picture
/// as shown that's kept. A placement's crop is of the turned picture before
/// it's mirrored or flipped (as the recording draws it, `Canvas::draw_camera`,
/// and as turning moves it round, `Placement::turn`): shown, a mirrored
/// picture's left is the crop's right.
fn crop_shown(o: Orientation, [l, t, r, b]: [f32; 4]) -> egui::Rect {
    let (left, right) = if o.flip_h { (r, l) } else { (l, r) };
    let (top, bottom) = if o.flip_v { (b, t) } else { (t, b) };
    egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(1.0 - right, 1.0 - bottom))
}

/// The crop that keeps this part of the picture as shown.
fn crop_from_shown(o: Orientation, s: egui::Rect) -> [f32; 4] {
    let (left, right) = (s.min.x.max(0.0), (1.0 - s.max.x).max(0.0));
    let (top, bottom) = (s.min.y.max(0.0), (1.0 - s.max.y).max(0.0));
    let (l, r) = if o.flip_h { (right, left) } else { (left, right) };
    let (t, b) = if o.flip_v { (bottom, top) } else { (top, bottom) };
    [l, t, r, b]
}

/// How the webcam is turned (quarter turns clockwise) and mirrored or
/// flipped in the clips: its placement's.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Orientation {
    flip_h: bool,
    flip_v: bool,
    turns: u8,
}

impl Orientation {
    /// Quarter turns clockwise as you see it: a mirrored picture's turns go
    /// the other way under its mirror (`Placement::turn`).
    fn seen_turns(self) -> u8 {
        let t = self.turns % 4;
        if self.flip_h != self.flip_v { (4 - t) % 4 } else { t }
    }
}

/// The webcam's orientation in the clips, after its format: the same as the
/// preview's right-click. What's wanted goes in `reorient`, its `turns` as
/// seen (`Orientation::seen_turns`).
fn orientation_view(ui: &mut egui::Ui, now: Orientation, reorient: &mut Option<Orientation>, cropping: &mut bool, cropped: bool, recrop: &mut Option<[f32; 4]>) {
    use crate::ui_kit::{Item, Row};
    crate::ui_kit::section_head(ui, "Orientation", false, |_| {});
    crate::ui_kit::list(ui, "camera_orientation", |l| {
        l.row(Row::new("Mirror").note("Left and right swapped, in your clips too"), |ui| {
            let mut on = now.flip_h;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                *reorient = Some(Orientation { flip_h: on, turns: now.seen_turns(), ..now });
            }
        });
        l.row(Row::new("Upside down"), |ui| {
            let mut on = now.flip_v;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                *reorient = Some(Orientation { flip_v: on, turns: now.seen_turns(), ..now });
            }
        });
        let tip = "Drag the picture's edges to cut off what you don't want; the rest stays dimmed while you do. The camera's own Zoom crops inside the camera instead, from its sensor, which can be sharper.";
        l.row(Row::new("Crop").tip("Crop", tip), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let label = if *cropping { "Done" } else { "Crop" };
            if crate::ui_kit::Btn::normal(label).small().show(ui).clicked() {
                *cropping = !*cropping;
            }
            if cropped && crate::ui_kit::Btn::ghost("Reset").small().tip("The whole picture again").show(ui).clicked() {
                *recrop = Some([0.0; 4]);
            }
        });
        l.row(Row::new("Rotation"), |ui| {
            let label = |t: u8| format!("{}°", t as u32 % 4 * 90);
            let seen = now.seen_turns();
            let r = crate::ui_kit::select(ui, &label(seen), 150.0);
            crate::ui_kit::menu_popup(&r, false, 160.0, |ui| {
                for t in 0..4u8 {
                    if Item::new(label(t)).check(seen == t).show(ui).clicked() && seen != t {
                        *reorient = Some(Orientation { turns: t, ..now });
                    }
                }
            });
        });
    });
}

/// The camera's format, first on the Camera page: its resolution, then the
/// frame rates that size comes at (once one is picked). What's picked goes in `pick`.
fn format_view(ui: &mut egui::Ui, device: &str, current: Option<capture::webcam::Format>, pick: &mut Option<Option<capture::webcam::Format>>) {
    use crate::ui_kit::{Item, Row};
    let formats = capture::webcam::formats(device);
    crate::ui_kit::section_head(ui, "Format", true, |_| {});
    crate::ui_kit::list(ui, "camera_format", |l| {
        l.row(Row::new("Resolution"), |ui| {
            let label = current.map_or_else(|| "Automatic".to_owned(), |f| size_label(f.width, f.height));
            let r = crate::ui_kit::select(ui, &label, 150.0);
            crate::ui_kit::menu_popup(&r, false, 250.0, |ui| {
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    if Item::new("Automatic").check(current.is_none()).tip("The sharpest picture up to 1080p at 30 fps or more").show(ui).clicked() {
                        *pick = Some(None);
                    }
                    if formats.is_empty() {
                        crate::ui_kit::menu_head(ui, "The camera's sizes show once it's open.");
                    }
                    for (w, h) in sizes(&formats) {
                        let on = current.is_some_and(|f| (f.width, f.height) == (w, h));
                        // How fast it goes at most: some sizes only come slowly.
                        let fastest = formats.iter().filter(|f| (f.width, f.height) == (w, h)).map(|f| f.fps()).fold(0.0, f32::max);
                        if Item::new(size_label(w, h)).value(format!("up to {}", fps_label(fastest))).check(on).show(ui).clicked() && !on {
                            *pick = Some(rate_for(&formats, (w, h), current));
                        }
                    }
                });
            });
        });
        if let Some(f) = current {
            l.row(Row::new("Frame rate"), |ui| {
                let r = crate::ui_kit::select(ui, &fps_label(f.fps()), 150.0);
                crate::ui_kit::menu_popup(&r, false, 200.0, |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for o in formats.iter().filter(|o| (o.width, o.height) == (f.width, f.height)) {
                            if Item::new(fps_label(o.fps())).check(*o == f).show(ui).clicked() && *o != f {
                                *pick = Some(Some(*o));
                            }
                        }
                    });
                });
            });
        }
    });
}

/// The camera's settings in groups, each a card of rows. What's changed goes in `set`.
fn controls_view(ui: &mut egui::Ui, panel: &CameraPanel, fps: Option<f32>, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    if panel.controls.is_empty() {
        ui.label(theme::text("This camera has no settings to change, or they couldn't be read.", 13.0, W::Regular).color(pal.text3));
    }
    for (heading, rows) in panel_groups(&panel.controls) {
        crate::ui_kit::section_head(ui, heading, false, |_| {});
        crate::ui_kit::list(ui, ("camera", heading), |l| {
            for r in &rows {
                match r {
                    PanelRow::Control { label, control } => {
                        l.row(crate::ui_kit::Row::new(label), |ui| control_widget(ui, control, set));
                    }
                    PanelRow::Auto { label, auto, manual } => {
                        let on = auto_on(auto);
                        l.row(crate::ui_kit::Row::new(label), |ui| {
                            let mut v = on;
                            if crate::ui_kit::switch(ui, &mut v).changed() {
                                set.push((auto.id, auto_value(auto, v)));
                            }
                        });
                        // The setting it takes over, while it's off.
                        if let Some((label, m)) = manual {
                            l.well(("camera_manual", auto.id), !on, |l| {
                                if m.id == v4l2::EXPOSURE {
                                    let tip = fps.map(|fps| {
                                        let (half, whole) = exposure_guides(fps);
                                        format!(
                                            "How long each picture takes in light. Film's rule of thumb is half a frame, a 180° shutter: {:.1} ms at {fps:.0} fps, for motion blur that looks natural. About a whole frame ({:.1} ms) or longer, a camera can't keep its frame rate; where exactly is the camera's own, as is how much light it wants (watch the pictures a second under the picture). Under lamps, step it with − and + (or ← and →) to where no bands roll through the picture.",
                                            half as f64 / 10.0,
                                            whole as f64 / 10.0
                                        )
                                    });
                                    let row = crate::ui_kit::Row::new(label).kid();
                                    let row = match &tip {
                                        Some(t) => row.tip("Exposure time", t),
                                        None => row,
                                    };
                                    l.row(row, |ui| exposure_widget(ui, m, set));
                                } else {
                                    l.row(crate::ui_kit::Row::new(label).kid(), |ui| control_widget(ui, m, set));
                                }
                            });
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    fn ctl(id: u32, name: &str, kind: ControlKind, value: i64) -> CameraControl {
        CameraControl { id, name: name.into(), kind, value, default: value, inactive: false }
    }

    /// A Logitech C922's controls: named and grouped, automatic modes paired
    /// with the setting they take over, the rest under More.
    /// Half a frame and a whole one: at 30 fps 16.7 and 33.3 ms, at 60 fps 8.3 and 16.7 ms.
    /// On a C922's 0.3–204.7 ms: 1 ms near the start, 16.7 ms (a frame at
    /// 60 fps) half way, 100 ms near the end; and back again.
    #[test]
    fn exposure_slider_curve() {
        let at = |ms: i64| curve_pos(ms * 10, 3, 2047);
        assert!(at(1) < 0.1, "{}", at(1));
        assert!((0.45..0.56).contains(&at(17)), "{}", at(17));
        assert!((0.8..0.9).contains(&at(100)), "{}", at(100));
        assert_eq!(curve_value(curve_pos(167, 3, 2047), 3, 2047), 167);
        assert_eq!(curve_value(0.0, 3, 2047), 3);
        assert_eq!(curve_value(1.0, 3, 2047), 2047);
    }

    /// Only clearly fewer than the format's rate is a slowdown, said with
    /// both causes (which one, the camera doesn't say).
    #[test]
    fn says_when_the_camera_slows() {
        assert_eq!(slowdown(Some(58.0), Some(60.0)), None);
        let said = slowdown(Some(30.0), Some(60.0)).unwrap();
        assert!(said.starts_with("Sending 30 of 60") && said.contains("light") && said.contains("exposure"));
        assert_eq!(slowdown(None, Some(60.0)), None);
    }

    /// A crop set on the picture as shown is the same crop shown again, in
    /// every orientation; mirrored, the shown left is the crop's right (as
    /// the recording draws it).
    #[test]
    fn crop_through_every_orientation() {
        let kept = egui::Rect::from_min_max(egui::pos2(0.1, 0.2), egui::pos2(0.6, 0.9));
        for turns in 0..4u8 {
            for (flip_h, flip_v) in [(false, false), (true, false), (false, true), (true, true)] {
                let o = Orientation { flip_h, flip_v, turns };
                let again = crop_shown(o, crop_from_shown(o, kept));
                assert!((again.min - kept.min).length() < 1e-5 && (again.max - kept.max).length() < 1e-5, "{o:?}");
            }
        }
        let close = |c: [f32; 4], want: [f32; 4]| c.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-5);
        assert!(close(crop_from_shown(Orientation::default(), kept), [0.1, 0.2, 0.4, 0.1]));
        let mirrored = Orientation { flip_h: true, ..Default::default() };
        assert!(close(crop_from_shown(mirrored, kept), [0.4, 0.2, 0.1, 0.1]));
    }

    #[test]
    fn exposure_guides_by_frame_rate() {
        assert_eq!(exposure_guides(30.0), (167, 333));
        assert_eq!(exposure_guides(60.0), (83, 167));
    }

    #[test]
    fn groups_a_webcams_controls() {
        use v4l2::*;
        let int = ControlKind::Int { min: 0, max: 255, step: 1 };
        let menu = |items: &[(i64, &str)]| ControlKind::Menu(items.iter().map(|(v, n)| (*v, n.to_string())).collect());
        let controls = vec![
            ctl(BRIGHTNESS, "Brightness", int.clone(), 128),
            ctl(AUTO_WHITE_BALANCE, "White Balance, Automatic", ControlKind::Bool, 1),
            ctl(POWER_LINE, "Power Line Frequency", menu(&[(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz")]), 2),
            ctl(WHITE_BALANCE, "White Balance Temperature", ControlKind::Int { min: 2000, max: 6500, step: 1 }, 4000),
            ctl(AUTO_EXPOSURE, "Auto Exposure", menu(&[(1, "Manual Mode"), (3, "Aperture Priority Mode")]), 3),
            ctl(EXPOSURE, "Exposure Time, Absolute", ControlKind::Int { min: 3, max: 2047, step: 1 }, 250),
            ctl(0x9a0921, "Something New", int.clone(), 0),
        ];
        let groups = panel_groups(&controls);
        let headings: Vec<_> = groups.iter().map(|(h, _)| *h).collect();
        assert_eq!(headings, ["Image", "Exposure", "White balance", OTHER]);
        let exposure = &groups[1].1;
        assert!(matches!(exposure[0], PanelRow::Auto { label: "Automatic exposure", manual: Some(("Exposure time", _)), .. }));
        assert!(matches!(&exposure[1], PanelRow::Control { label, .. } if label == "Flicker (mains frequency)"));
        assert!(matches!(&groups[3].1[0], PanelRow::Control { label, .. } if label == "Something New"));
        // Aperture priority is automatic; turning it off picks manual, on picks the default.
        let ae = &controls[4];
        assert!(auto_on(ae));
        assert_eq!(auto_value(ae, false), v4l2::EXPOSURE_MANUAL);
        assert_eq!(auto_value(ae, true), 3);
    }
}

/// The sizes a camera offers, each once, biggest first.
fn sizes(formats: &[capture::webcam::Format]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for f in formats {
        if !out.contains(&(f.width, f.height)) {
            out.push((f.width, f.height));
        }
    }
    out.sort_by_key(|(w, h)| std::cmp::Reverse((w * h, *w)));
    out
}

/// The format to take at size `size`: the rate it had, if offered there,
/// else the fastest.
fn rate_for(formats: &[capture::webcam::Format], size: (u32, u32), current: Option<capture::webcam::Format>) -> Option<capture::webcam::Format> {
    let at = formats.iter().filter(|f| (f.width, f.height) == size);
    let same = current.and_then(|c| at.clone().find(|f| (f.fps() - c.fps()).abs() < 0.05));
    same.or_else(|| at.max_by(|a, b| a.fps().total_cmp(&b.fps()))).copied()
}

/// A size: "1920×1080".
fn size_label(w: u32, h: u32) -> String {
    format!("{w}×{h}")
}

fn fps_label(fps: f32) -> String {
    if (fps - fps.round()).abs() < 0.05 { format!("{} fps", fps.round()) } else { format!("{fps:.2} fps") }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    use capture::webcam::Format;

    fn f(width: u32, height: u32, fps: u32) -> Format {
        Format { width, height, fps_num: fps, fps_den: 1 }
    }

    #[test]
    fn sizes_once_biggest_first_and_rates_kept() {
        let formats = [f(640, 480, 30), f(1920, 1080, 30), f(1920, 1080, 60), f(640, 480, 15), f(1280, 720, 60)];
        assert_eq!(sizes(&formats), [(1920, 1080), (1280, 720), (640, 480)]);
        // 30 fps is kept where the new size has it; else the fastest.
        assert_eq!(rate_for(&formats, (640, 480), Some(f(1920, 1080, 30))), Some(f(640, 480, 30)));
        assert_eq!(rate_for(&formats, (1280, 720), Some(f(1920, 1080, 30))), Some(f(1280, 720, 60)));
        assert_eq!(size_label(1920, 1080), "1920×1080");
        // As `Format::label`: within 0.05 of a whole number reads as it.
        assert_eq!(fps_label(29.97), "30 fps");
        assert_eq!(fps_label(7.5), "7.50 fps");
    }
}
