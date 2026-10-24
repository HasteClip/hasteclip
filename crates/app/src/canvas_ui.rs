//! The picture's sources on the Sources page's preview, placed the way OBS
//! places them: the screen and the webcam, each dragged to move it and by a
//! handle to resize it (never stretched), one in front of the other, and
//! cropped with Alt and a handle (the picture keeps its scale, its box
//! shrinks with it). The webcam also flips (past its opposite side) and
//! turns (its right-click menu): see `webcam_ui`. Moves snap to the video's
//! edges and middle and to the other source's edges (Ctrl to place freely).
//!
//! The screen fills the video until a corner makes it smaller (or it's
//! cropped here): then it's in a box of its own showing all of it, in its
//! own shape (`settings::ScreenCfg::placed`), and Reset puts it back. Its
//! crop is the crop's page's (`settings::ScreenCrop`). Both placements are
//! shared with the capture, so what's dragged is what's recorded, live.
//! Which is in front is the order of their cards under the preview (see
//! `sources_ui`), or their menus' Bring to front.

use capture::webcam::Placement;
use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, vec2};
use egui_phosphor::regular as icon;

use crate::App;
use crate::settings::{BoxCfg, Framing, PlacementCfg, ScreenCfg, ScreenCrop};
use crate::ui_kit::Item;
use crate::webcam_ui::{self, Handle, Lines, Mods};

/// Handle size, in points.
const HANDLE: f32 = 8.0;

/// How the screen can fill a video of another shape: the choices, as
/// they're named side by side and in a menu, and what each does.
pub(crate) const FRAMINGS: [(Framing, &str, &str, &str); 3] = [
    (Framing::Fill, "Fill", "Fill the video", "Its middle covers all of the video: what's past the video's shape is cut off"),
    (Framing::Fit, "Fit", "Fit in the video", "All of it, as big as fits, with black bars where the shapes differ"),
    (Framing::Stretch, "Stretch", "Stretch to the video", "All of it over all of the video, stretched or squeezed to its shape"),
];

/// Fill, Fit or Stretch, side by side: the one clicked. None is lit while
/// the screen's placed by hand (`now` is `None`).
pub(crate) fn framing_choice(ui: &mut egui::Ui, id: &str, now: Option<Framing>) -> Option<Framing> {
    let opts: Vec<crate::ui_kit::Seg> = FRAMINGS.iter().map(|(_, label, _, tip)| crate::ui_kit::Seg { icon: None, label, tip: Some(tip) }).collect();
    let chosen = now.and_then(|f| FRAMINGS.iter().position(|(g, ..)| *g == f)).unwrap_or(usize::MAX);
    let enabled = ui.is_enabled();
    crate::ui_kit::segmented_opts(ui, id, &opts, chosen, None, enabled).map(|i| FRAMINGS[i].0)
}
/// A source's outline on the preview.
const ACCENT: Color32 = Color32::WHITE;
/// OBS-ish crop colour.
const CROP: Color32 = Color32::from_rgb(80, 220, 120);
/// Smallest the screen's box gets, as a fraction of the video.
const MIN_SIZE: f32 = 0.05;

/// One of the picture's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Layer {
    Screen,
    Webcam,
}

/// A drag on the preview.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// The webcam: what's held, where it was taken hold of, its placement then.
    Webcam(Handle, Pos2, Placement),
    /// The screen's box: what's held, where, the box then (fractions of the video).
    Screen(Handle, Pos2, Rect),
    /// The screen, cropped: what's held, where, its box then, and the part
    /// of the display it showed (in its pixels, of a display this big).
    ScreenCrop(Handle, Pos2, Rect, Rect, (u32, u32)),
    /// A source filling the video, held in its middle: it has nowhere to move.
    Stuck(Layer),
}

/// The preview's own state on the Sources page.
#[derive(Default)]
pub(crate) struct CanvasView {
    drag: Option<Drag>,
    /// The right-click menu is open, on this source.
    menu: Option<Layer>,
    /// The screen's picture size (what's kept of it), asked for now and then.
    picture: Option<(capture::VideoSource, Option<(u32, u32)>, std::time::Instant)>,
    /// Arrangements to go back to (Ctrl+Z), and forward again.
    undo: Vec<Arrangement>,
    redo: Vec<Arrangement>,
    /// The source whose card (under the preview) the pointer is on: it's
    /// outlined on the preview, so it's clear which it is.
    pub(crate) card_hover: Option<Layer>,
}

/// Where the sources are, as undo keeps it: the screen (its box or how it
/// fills the video, in front or behind, its crop) and the webcam's placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Arrangement {
    screen: ScreenCfg,
    framing: Framing,
    crop: Option<ScreenCrop>,
    webcam: Option<PlacementCfg>,
}

/// What's at `p` on the preview (`to_screen` puts a box there): a handle of a
/// source (front first), else the source in front there.
fn hit(layers: &[(Layer, Rect)], to_screen: &dyn Fn(Rect) -> Rect, p: Pos2) -> Option<(Layer, Handle)> {
    for (l, r) in layers {
        let bx = to_screen(*r);
        if let Some(h) = Handle::ALL.into_iter().find(|h| h.pos(bx).distance(p) <= HANDLE) {
            return Some((*l, h));
        }
    }
    layers.iter().find(|(_, r)| to_screen(*r).expand(2.0).contains(p)).map(|(l, _)| (*l, Handle::Move))
}

/// What a move of `me` snaps to besides the video's edges and middle: the
/// other sources' edges and middles (not one that fills the video: its
/// edges are the video's).
fn lines_of(layers: &[(Layer, Rect)], me: Layer) -> Lines {
    let mut lines = Lines::default();
    for (_, r) in layers.iter().filter(|(l, r)| *l != me && !fills(*r)) {
        lines.xs.extend([r.min.x, r.center().x, r.max.x]);
        lines.ys.extend([r.min.y, r.center().y, r.max.y]);
    }
    lines
}

/// Whether a box is all of the video.
fn fills(r: Rect) -> bool {
    r.min.x <= 1e-4 && r.min.y <= 1e-4 && r.max.x >= 1.0 - 1e-4 && r.max.y >= 1.0 - 1e-4
}

/// The lines a box moved onto, to draw: the video's middle, or another
/// source's edge or middle, across (`true`) or down, at.
fn snapped(r: Rect, lines: &Lines) -> Vec<(bool, f32)> {
    let on = |v: f32, at: f32| (v - at).abs() < 1e-4;
    let mut out = Vec::new();
    for at in lines.xs.iter().copied().chain([0.5]) {
        if [r.min.x, r.center().x, r.max.x].iter().any(|v| on(*v, at)) {
            out.push((true, at));
        }
    }
    for at in lines.ys.iter().copied().chain([0.5]) {
        if [r.min.y, r.center().y, r.max.y].iter().any(|v| on(*v, at)) {
            out.push((false, at));
        }
    }
    out
}

/// Where a picture of shape `picture_aspect` goes in a frame of shape
/// `frame_aspect` as big as fits, in the middle (fractions of the frame).
pub(crate) fn fitted(picture_aspect: f32, frame_aspect: f32) -> Rect {
    let size = if picture_aspect > frame_aspect { vec2(1.0, frame_aspect / picture_aspect) } else { vec2(picture_aspect / frame_aspect, 1.0) };
    Rect::from_center_size(Pos2::new(0.5, 0.5), size)
}

/// The screen's box after dragging `handle` by `(dx, dy)` (fractions of the
/// video) from `start`: moved (snapping to the video and to `lines`, unless
/// `snap` is off), or scaled from the opposite corner or side, keeping its
/// shape. It stays inside the video.
pub(crate) fn screen_drag(start: Rect, handle: Handle, dx: f32, dy: f32, snap: bool, lines: &Lines) -> Rect {
    if handle == Handle::Move {
        let mut r = start.translate(vec2(dx, dy));
        if snap {
            let sx = webcam_ui::snap_offset(&[r.min.x, r.max.x], r.center().x, &lines.xs);
            let sy = webcam_ui::snap_offset(&[r.min.y, r.max.y], r.center().y, &lines.ys);
            r = r.translate(vec2(sx, sy));
        }
        let x = r.min.x.clamp(0.0, (1.0 - r.width()).max(0.0));
        let y = r.min.y.clamp(0.0, (1.0 - r.height()).max(0.0));
        return Rect::from_min_size(Pos2::new(x, y), r.size());
    }
    let bounds = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    webcam_ui::resize_within(start, handle.sides(), vec2(dx, dy), true, bounds, vec2(MIN_SIZE, MIN_SIZE))
}

/// The screen's box and crop after cropping it by dragging `handle` by
/// `(dx, dy)` (fractions of the video), from its box `start` showing `part`
/// of a `dw`×`dh` display (in its pixels): the picture keeps its scale and
/// place, the box follows the crop's edges, inside the video and the
/// display (it can't be uncropped past it), at least 64 of its pixels each
/// way, on even pixels. With `keep`, the box keeps its shape. `None`: all
/// of the display.
pub(crate) fn screen_crop_drag(start: Rect, part: Rect, (dw, dh): (u32, u32), handle: Handle, dx: f32, dy: f32, keep: bool) -> (Rect, Option<ScreenCrop>) {
    let (sx, sy) = (start.width() / part.width().max(1.0), start.height() / part.height().max(1.0));
    // All of the display, on the video.
    let whole = Rect::from_min_size(Pos2::new(start.min.x - part.min.x * sx, start.min.y - part.min.y * sy), vec2(dw as f32 * sx, dh as f32 * sy));
    let video = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let to = webcam_ui::resize_within(start, handle.sides(), vec2(dx, dy), keep, whole.intersect(video), vec2(64.0 * sx, 64.0 * sy));
    // In the display's pixels, even.
    let even = |v: f32, most: u32| (((v.max(0.0) / 2.0).round() as u32) * 2).min(most & !1);
    let (x0, x1) = (even((to.min.x - whole.min.x) / sx, dw), even((to.max.x - whole.min.x) / sx, dw));
    let (y0, y1) = (even((to.min.y - whole.min.y) / sy, dh), even((to.max.y - whole.min.y) / sy, dh));
    let at = |x: u32, y: u32| Pos2::new(whole.min.x + x as f32 * sx, whole.min.y + y as f32 * sy);
    let b = Rect::from_min_max(at(x0, y0), at(x1, y1)).intersect(video);
    let all = (x0, y0, x1, y1) == (0, 0, dw & !1, dh & !1);
    (b, (!all).then_some(ScreenCrop { w: x1 - x0, h: y1 - y0, at: Some([x0, y0]) }))
}

impl App {
    /// The screen's picture's size: what's kept of it, cropped (the main
    /// display's, for games and apps). The display's is asked for now and then.
    pub(crate) fn screen_picture(&mut self) -> Option<(u32, u32)> {
        let source = self.video_source();
        if self.canvas.picture.as_ref().is_none_or(|(s, _, at)| *s != source || at.elapsed() > std::time::Duration::from_secs(2)) {
            let size = capture::frame_size(&source, Default::default());
            self.canvas.picture = Some((source, size, std::time::Instant::now()));
        }
        let crop = self.settings.screen_placement().crop;
        self.canvas.picture.as_ref().and_then(|(_, size, _)| *size).map(|size| capture::cropped_size(crop, size))
    }

    /// Where the screen's picture is in the video (fractions of it): its box
    /// when it's been placed by hand, else all of the video, or where Fit
    /// puts it (its own shape, as big as fits).
    pub(crate) fn screen_rect(&mut self, frame_aspect: f32) -> Rect {
        let whole = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        if let Some(b) = self.settings.screen.placed {
            return Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h));
        }
        if self.settings.screen_placement().framing != capture::Framing::Fit {
            return whole;
        }
        let Some((w, h)) = self.screen_picture() else { return whole };
        fitted(w as f32 / h.max(1) as f32, frame_aspect)
    }

    pub(crate) fn arrangement(&self) -> Arrangement {
        Arrangement { screen: self.settings.screen, framing: self.settings.framing, crop: self.settings.screen_crop, webcam: self.settings.webcam.as_ref().map(|w| w.placement) }
    }

    /// `was` (how the sources were before a change just made) can be gone
    /// back to.
    pub(crate) fn keep_for_undo(&mut self, was: Arrangement) {
        // Taking the webcam out (or putting one in) isn't a step to go back to.
        if was.webcam.is_some() != self.settings.webcam.is_some() {
            return;
        }
        if self.canvas.undo.last() != Some(&was) {
            self.canvas.undo.push(was);
            if self.canvas.undo.len() > 100 {
                self.canvas.undo.remove(0);
            }
        }
        self.canvas.redo.clear();
    }

    /// The sources where `a` has them (each one on or off as it is now).
    fn arrange(&mut self, a: Arrangement) {
        self.settings.screen = ScreenCfg { enabled: self.settings.screen.enabled, ..a.screen };
        (self.settings.framing, self.settings.screen_crop) = (a.framing, a.crop);
        if let (Some(w), Some(p)) = (self.settings.webcam.as_mut(), a.webcam) {
            w.placement = p;
        }
    }

    /// The arrangement before the last change (Ctrl+Z on Sources).
    pub(crate) fn undo_arrangement(&mut self) {
        if let Some(a) = self.canvas.undo.pop() {
            let now = self.arrangement();
            self.canvas.redo.push(now);
            self.arrange(a);
        }
    }

    /// The change undone last, again (Ctrl+Shift+Z).
    pub(crate) fn redo_arrangement(&mut self) {
        if let Some(a) = self.canvas.redo.pop() {
            let now = self.arrangement();
            self.canvas.undo.push(now);
            self.arrange(a);
        }
    }

    /// Undo and redo, over the preview: shown once there's something to
    /// undo (or redo), ready for a change that went wrong.
    pub(crate) fn undo_buttons(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::{Btn, Kind};
        if self.canvas.undo.is_empty() && self.canvas.redo.is_empty() {
            return;
        }
        let ctrl = crate::shortcuts::ctrl_name();
        let redo = Btn::icon(Kind::Ghost, icon::ARROW_CLOCKWISE).small().enabled(!self.canvas.redo.is_empty()).tip(format!("Redo  ({ctrl}+Shift+Z)"));
        if redo.show(ui).clicked() {
            self.redo_arrangement();
        }
        let undo = Btn::icon(Kind::Ghost, icon::ARROW_COUNTER_CLOCKWISE).small().enabled(!self.canvas.undo.is_empty()).tip(format!("Undo where your sources are  ({ctrl}+Z)"));
        if undo.show(ui).clicked() {
            self.undo_arrangement();
        }
    }

    /// Where the webcam goes.
    fn webcam_placement(&self) -> Option<Placement> {
        self.settings.webcam.as_ref().map(|w| w.placement.into())
    }

    /// The picture's sources, front first, for their cards: the screen, and
    /// the webcam once there is one (off too).
    pub(crate) fn layer_order(&self) -> Vec<Layer> {
        let webcam = self.settings.webcam.is_some().then_some(Layer::Webcam);
        let order = if self.settings.screen.in_front { [Some(Layer::Screen), webcam] } else { [webcam, Some(Layer::Screen)] };
        order.into_iter().flatten().collect()
    }

    /// The sources in this order, front first.
    pub(crate) fn set_layer_order(&mut self, order: &[Layer]) {
        let at = |l: Layer| order.iter().position(|o| *o == l);
        if let (Some(screen), Some(webcam)) = (at(Layer::Screen), at(Layer::Webcam)) {
            self.settings.screen.in_front = screen < webcam;
        }
    }

    /// The sources in the video, front first, and where each is (fractions
    /// of the video).
    fn layers(&mut self, frame_aspect: f32) -> Vec<(Layer, Rect)> {
        let screen = self.settings.screen.enabled.then(|| (Layer::Screen, self.screen_rect(frame_aspect)));
        let webcam = self.webcam_placement().filter(|_| self.settings.webcam.as_ref().is_some_and(|w| w.enabled)).map(|p| (Layer::Webcam, Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h))));
        let order = if self.settings.screen.in_front { [screen, webcam] } else { [webcam, screen] };
        order.into_iter().flatten().collect()
    }

    /// The sources on the preview (`frame_rect`, whose `response` takes the
    /// pointer): their outlines and handles, dragging them, and their menus.
    pub(crate) fn canvas_on_preview(&mut self, ui: &mut egui::Ui, frame_rect: Rect, response: &egui::Response) {
        let aspect = frame_rect.width() / frame_rect.height().max(1.0);
        let at = move |p: Pos2| Pos2::new(frame_rect.left() + p.x * frame_rect.width(), frame_rect.top() + p.y * frame_rect.height());
        let to_screen = move |r: Rect| Rect::from_min_max(at(r.min), at(r.max));
        let layers = self.layers(aspect);
        if layers.is_empty() {
            self.canvas.drag = None;
            return;
        }
        let modifiers = ui.input(|i| i.modifiers);
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| frame_rect.expand(HANDLE).contains(*p));

        // A drag only starts once the pointer has moved a little, so what was
        // grabbed is judged by where the button went down, not where it is now.
        if response.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
            if let Some(((layer, handle), from)) = origin.and_then(|p| hit(&layers, &to_screen, p).map(|h| (h, p))) {
                let was = self.arrangement();
                let crop = (layer == Layer::Screen && modifiers.alt && handle != Handle::Move).then(|| self.screen_crop_hold(aspect)).flatten();
                let drag = match (layer, crop) {
                    (Layer::Webcam, _) => Drag::Webcam(handle, from, self.webcam_placement().unwrap_or_else(Placement::hidden)),
                    (Layer::Screen, Some((start, part, display))) => Drag::ScreenCrop(handle, from, start, part, display),
                    (Layer::Screen, None) => self.take_screen(handle, from, aspect, &layers),
                };
                // Something that moves can be put back (one held that can't, can't).
                if !matches!(drag, Drag::Stuck(_)) {
                    self.keep_for_undo(was);
                }
                self.canvas.drag = Some(drag);
            }
        }
        let mut guides = Vec::new();
        if let Some(drag) = self.canvas.drag {
            if response.dragged()
                && let Some(now) = response.interact_pointer_pos()
            {
                let snap = !modifiers.ctrl;
                let d = |from: Pos2| ((now.x - from.x) / frame_rect.width(), (now.y - from.y) / frame_rect.height());
                match drag {
                    Drag::Webcam(handle, from, start) => {
                        let lines = lines_of(&layers, Layer::Webcam);
                        let (dx, dy) = d(from);
                        let moved = webcam_ui::drag(start, handle, dx, dy, Mods { crop: modifiers.alt, snap, keep: modifiers.shift }, &lines);
                        if let Some(w) = self.settings.webcam.as_mut() {
                            w.placement = PlacementCfg::laid_out(moved, aspect);
                        }
                        if handle == Handle::Move && snap {
                            guides = snapped(Rect::from_min_size(Pos2::new(moved.x, moved.y), vec2(moved.w, moved.h)), &lines);
                        }
                    }
                    Drag::Screen(handle, from, start) => {
                        let lines = lines_of(&layers, Layer::Screen);
                        let (dx, dy) = d(from);
                        let r = screen_drag(start, handle, dx, dy, snap, &lines);
                        self.settings.screen.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect));
                        if handle == Handle::Move && snap {
                            guides = snapped(r, &lines);
                        }
                    }
                    Drag::ScreenCrop(handle, from, start, part, display) => {
                        let (dx, dy) = d(from);
                        let (b, crop) = screen_crop_drag(start, part, display, handle, dx, dy, modifiers.shift);
                        self.settings.screen.placed = Some(BoxCfg::new([b.min.x, b.min.y, b.width(), b.height()], aspect));
                        self.settings.screen_crop = crop;
                    }
                    Drag::Stuck(_) => {}
                }
                ui.ctx().set_cursor_icon(match drag {
                    Drag::Webcam(Handle::Move, ..) | Drag::Screen(Handle::Move, ..) => CursorIcon::Grabbing,
                    Drag::Webcam(h, ..) | Drag::Screen(h, ..) | Drag::ScreenCrop(h, ..) => h.cursor(),
                    Drag::Stuck(_) => CursorIcon::NotAllowed,
                });
            }
            if response.drag_stopped() || !response.dragged() && !ui.input(|i| i.pointer.any_down()) {
                self.canvas.drag = None;
            }
        } else if let Some((_, h)) = pointer.and_then(|p| hit(&layers, &to_screen, p)) {
            ui.ctx().set_cursor_icon(h.cursor());
        }

        // Right-click on a source: its menu (tied to the click that opened it).
        if response.secondary_clicked() {
            self.canvas.menu = ui.input(|i| i.pointer.interact_pos()).and_then(|p| hit(&layers, &to_screen, p)).map(|(l, _)| l);
        }
        if let Some(layer) = self.canvas.menu {
            let was = self.arrangement();
            let open = crate::ui_kit::context_menu(response, 230.0, |ui| self.layer_menu(ui, layer, aspect));
            if self.arrangement() != was {
                self.keep_for_undo(was);
            }
            if open.is_none() {
                self.canvas.menu = None;
            }
        }

        // The source held or under the pointer: its outline and handles, over
        // the picture. Not clipped to the picture: a box against its edge
        // keeps all of its outline.
        let layers = self.layers(aspect);
        let held = self.canvas.drag.map(|d| match d {
            Drag::Webcam(h, ..) => (Layer::Webcam, Some(h)),
            Drag::Screen(h, ..) | Drag::ScreenCrop(h, ..) => (Layer::Screen, Some(h)),
            Drag::Stuck(l) => (l, None),
        });
        let over = pointer.and_then(|p| hit(&layers, &to_screen, p));
        // Else the source whose card the pointer's on: only its outline.
        let card = self.canvas.card_hover.filter(|_| held.is_none() && over.is_none());
        let Some((active, handle)) = held.or(over.map(|(l, h)| (l, Some(h)))).or(card.map(|l| (l, None))) else { return };
        if held.is_none() && self.canvas.menu.is_some() {
            return;
        }
        let Some(rect) = layers.iter().find(|(l, _)| *l == active).map(|(_, r)| *r) else { return };
        let bx = to_screen(rect);
        let painter = ui.painter().clone();
        let webcam: Option<Placement> = (active == Layer::Webcam).then(|| self.webcam_placement()).flatten();
        // The screen's box, the part of the display it shows and the
        // display's size, when it can be cropped (a display, not games and apps).
        let screen = (active == Layer::Screen).then(|| self.screen_crop_hold(aspect)).flatten();
        let croppable = webcam.is_some() || screen.is_some();
        let cropping = modifiers.alt && croppable && handle.is_some_and(|h| h != Handle::Move) || matches!(self.canvas.drag, Some(Drag::ScreenCrop(..)));

        // Cropping: where the whole camera picture (or display) would be,
        // dashed, so it's clear what's cut off and how far it can grow back.
        let whole = match (webcam, screen) {
            (Some(p), _) => {
                let sx = p.w / (1.0 - p.crop[0] - p.crop[2]).max(0.01);
                let sy = p.h / (1.0 - p.crop[1] - p.crop[3]).max(0.01);
                let (cl, cr) = if p.flip_h { (p.crop[2], p.crop[0]) } else { (p.crop[0], p.crop[2]) };
                let (ct, cb) = if p.flip_v { (p.crop[3], p.crop[1]) } else { (p.crop[1], p.crop[3]) };
                Some(Rect::from_min_max(at(Pos2::new(p.x - cl * sx, p.y - ct * sy)), at(Pos2::new(p.x + p.w + cr * sx, p.y + p.h + cb * sy))))
            }
            (None, Some((b, part, (dw, dh)))) => {
                let (sx, sy) = (b.width() / part.width().max(1.0), b.height() / part.height().max(1.0));
                Some(to_screen(Rect::from_min_size(Pos2::new(b.min.x - part.min.x * sx, b.min.y - part.min.y * sy), vec2(dw as f32 * sx, dh as f32 * sy))))
            }
            _ => None,
        };
        if let Some(whole) = whole.filter(|w| cropping && *w != bx) {
            // Kept to the preview: all of a display can be far bigger than it.
            let ghost = Stroke::new(1.0, CROP.gamma_multiply(0.8));
            let c = [whole.left_top(), whole.right_top(), whole.right_bottom(), whole.left_bottom(), whole.left_top()];
            painter.with_clip_rect(frame_rect.expand(1.0)).extend(egui::Shape::dashed_line(&c, ghost, 4.0, 3.0));
        }

        // Moved onto the video's middle, or another source's edge: a guide.
        let guide = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
        for (across, v) in guides {
            let line = if across {
                let x = frame_rect.left() + v * frame_rect.width();
                [Pos2::new(x, frame_rect.top()), Pos2::new(x, frame_rect.bottom())]
            } else {
                let y = frame_rect.top() + v * frame_rect.height();
                [Pos2::new(frame_rect.left(), y), Pos2::new(frame_rect.right(), y)]
            };
            painter.extend(egui::Shape::dashed_line(&line, guide, 5.0, 4.0));
        }

        // The outline, just inside the box, over a soft dark edge so it shows
        // on a bright picture. The camera's own sides, as they show (a
        // mirrored picture's left is the camera's right), and the display's,
        // are green if cropped.
        let [cl, ct, cr, cb] = match (webcam, self.screen_crop_rect()) {
            (Some(p), _) => p.crop.map(|c| c > 0.0),
            (None, Some(([x, y, w, h], (dw, dh)))) if active == Layer::Screen => [x > 0, y > 0, x + w < dw & !1, y + h < dh & !1],
            _ => [false; 4],
        };
        let (flip_h, flip_v) = webcam.map_or((false, false), |p| (p.flip_h, p.flip_v));
        let (left, right) = if flip_h { (cr, cl) } else { (cl, cr) };
        let (top, bottom) = if flip_v { (cb, ct) } else { (ct, cb) };
        let line = bx.shrink(1.0);
        painter.rect_stroke(bx.shrink(0.5), 0.0, Stroke::new(3.0, Color32::from_black_alpha(110)), egui::StrokeKind::Middle);
        let side = |cropped: bool| Stroke::new(2.0, if cropped || cropping { CROP } else { ACCENT });
        painter.line_segment([line.left_top(), line.right_top()], side(top));
        painter.line_segment([line.left_bottom(), line.right_bottom()], side(bottom));
        painter.line_segment([line.left_top(), line.left_bottom()], side(left));
        painter.line_segment([line.right_top(), line.right_bottom()], side(right));
        if card.is_some() {
            return;
        }

        // Handles: round, white, green while Alt is held on something that
        // crops (they'll crop); the one held or under the pointer a little bigger.
        for h in Handle::ALL {
            let r = if Some(h) == handle { HANDLE * 0.65 } else { HANDLE * 0.5 };
            let fill = if modifiers.alt && croppable || cropping { CROP } else { Color32::WHITE };
            painter.circle(h.pos(line), r, fill, Stroke::new(1.0, Color32::from_black_alpha(140)));
        }

        // What the keys do, along the picture's foot (or top, if the box is
        // down there). (Alt is Option on a Mac, Ctrl Control: the keys as
        // they're named there.)
        let (alt, ctrl) = (crate::shortcuts::alt_name(), crate::shortcuts::ctrl_name());
        let boxed = self.settings.screen.placed.is_some();
        let hint = match (self.canvas.drag, active) {
            (Some(Drag::Stuck(_)), _) => "It fills the video: drag a corner to make it smaller".to_owned(),
            (Some(Drag::Webcam(Handle::Move, ..) | Drag::Screen(Handle::Move, ..)), _) => format!("{ctrl}: place freely"),
            (Some(Drag::Webcam(..) | Drag::ScreenCrop(..)), _) if cropping && modifiers.shift => "Cropping, keeping the shape".to_owned(),
            (Some(Drag::Webcam(..) | Drag::ScreenCrop(..)), _) if cropping => "Cropping · Shift keeps the shape".to_owned(),
            (Some(Drag::Webcam(..)), _) => format!("{alt}: crop · past the other side: flip"),
            (Some(Drag::Screen(..) | Drag::ScreenCrop(..)), _) => return,
            (None, Layer::Webcam) => format!("Drag to move · {alt} + handle to crop · right-click for more"),
            (None, Layer::Screen) if croppable && boxed => format!("Drag to move · {alt} + handle to crop · right-click for more"),
            (None, Layer::Screen) if croppable => format!("Drag a corner to make it smaller · {alt} + handle to crop"),
            (None, Layer::Screen) if boxed => "Drag to move · a corner to resize · right-click for more".to_owned(),
            (None, Layer::Screen) => "Drag a corner to make your screen smaller · right-click for more".to_owned(),
        };
        let galley = painter.layout_no_wrap(hint, egui::FontId::proportional(11.5), Color32::from_gray(235));
        let size = galley.size() + vec2(14.0, 6.0);
        if size.x > frame_rect.width() - 8.0 {
            return;
        }
        let low = bx.center().y > frame_rect.center().y && !fills(rect);
        let y = if low { frame_rect.top() + 8.0 + size.y / 2.0 } else { frame_rect.bottom() - 8.0 - size.y / 2.0 };
        let pill = Rect::from_center_size(Pos2::new(frame_rect.center().x, y), size);
        painter.rect_filled(pill, pill.height() / 2.0, Color32::from_black_alpha(170));
        painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
    }

    /// Take hold of the screen: in its box as it is; filling the video, from
    /// all of it in its own shape, as big as fits, into a box of its own
    /// (the same place, when it's the video's shape). Filling the video in
    /// its own shape, it has nowhere to move: held in the middle, it stays.
    fn take_screen(&mut self, handle: Handle, from: Pos2, aspect: f32, layers: &[(Layer, Rect)]) -> Drag {
        if let Some(b) = self.settings.screen.placed {
            return Drag::Screen(handle, from, Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h)));
        }
        let all = self.screen_picture().map_or_else(|| layers.iter().find(|(l, _)| *l == Layer::Screen).map_or(Rect::NOTHING, |(_, r)| *r), |(w, h)| fitted(w as f32 / h.max(1) as f32, aspect));
        if fills(all) && handle == Handle::Move {
            return Drag::Stuck(Layer::Screen);
        }
        self.settings.screen.placed = Some(BoxCfg::new([all.min.x, all.min.y, all.width(), all.height()], aspect));
        Drag::Screen(handle, from, all)
    }

    /// The screen's crop as it is (left, top, width, height in the
    /// display's pixels; all of it when it isn't cropped), and the display's
    /// size: `None` for games and apps, and until the display's size is known.
    fn screen_crop_rect(&self) -> Option<([u32; 4], (u32, u32))> {
        let (dw, dh) = self.crop_whole()?;
        let rect = self.settings.screen_crop.filter(|c| !c.is_none()).map_or([0, 0, dw & !1, dh & !1], |c| capture::Crop::from(c).rect(dw, dh));
        Some((rect, (dw, dh)))
    }

    /// The screen as it's taken hold of to crop it (Alt with a handle): its
    /// box, the part of the display it shows, and the display's size.
    /// Filling the video, that's the part showing (its middle, in the
    /// video's shape) over all of it, so nothing moves as it's taken hold
    /// of. `None` when it can't be cropped (see `screen_crop_rect`).
    fn screen_crop_hold(&mut self, aspect: f32) -> Option<(Rect, Rect, (u32, u32))> {
        let ([x, y, w, h], display) = self.screen_crop_rect()?;
        let part = Rect::from_min_size(Pos2::new(x as f32, y as f32), vec2(w as f32, h as f32));
        let fills_with_middle = self.settings.screen.placed.is_none() && self.settings.screen_placement().framing == capture::Framing::Fill;
        if !fills_with_middle {
            return Some((self.screen_rect(aspect), part, display));
        }
        let size = if part.aspect_ratio() > aspect { vec2(part.height() * aspect, part.height()) } else { vec2(part.width(), part.width() / aspect) };
        Some((Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Rect::from_center_size(part.center(), size), display))
    }

    /// The screen back over all of the video (Reset).
    pub(crate) fn screen_to_whole(&mut self) {
        self.settings.screen.placed = None;
    }

    /// Whether the video has a shape of its own (an aspect ratio, or a size
    /// of its own): then the screen fills it as Fill, Fit or Stretch say.
    pub(crate) fn video_shaped(&self) -> bool {
        self.settings.capture_shape().is_some()
    }

    /// The screen over all of the video again, put in as `framing` says.
    pub(crate) fn screen_framed(&mut self, framing: Framing) {
        self.settings.framing = framing;
        self.settings.screen.placed = None;
    }

    /// A source's menu (right-click it on the preview, or its card's ⋯):
    /// its place back as it was, the webcam's turns and flips, which is in
    /// front, and taking the webcam out.
    pub(crate) fn layer_menu(&mut self, ui: &mut egui::Ui, layer: Layer, aspect: f32) {
        match layer {
            Layer::Screen => {
                // How it fills a video of its own shape; else back over all of it.
                if self.video_shaped() {
                    let now = self.settings.screen.placed.is_none().then_some(self.settings.framing);
                    for (framing, _, label, tip) in FRAMINGS {
                        if Item::new(label).check(now == Some(framing)).tip(tip).show(ui).clicked() {
                            self.screen_framed(framing);
                            ui.close();
                        }
                    }
                } else if Item::new("Reset position").icon(icon::ARROWS_OUT).enabled(self.settings.screen.placed.is_some()).tip("Over all of the video again").show(ui).clicked() {
                    self.screen_to_whole();
                    ui.close();
                }
                if self.crop_whole().is_some() {
                    let cropped = self.settings.screen_crop.is_some_and(|c| !c.is_none());
                    if Item::new("Reset crop").icon(icon::CROP).enabled(cropped).tip("All of the screen again").show(ui).clicked() {
                        self.settings.screen_crop = None;
                        ui.close();
                    }
                }
            }
            Layer::Webcam => {
                if Item::new("Reset position").icon(icon::ARROWS_IN).tip("Back to the bottom-right corner").show(ui).clicked() {
                    self.webcam_to_corner(aspect);
                    ui.close();
                }
                let cropped = self.webcam_placement().is_some_and(|p| p.crop.iter().any(|c| *c > 0.0));
                if Item::new("Reset crop").icon(icon::CROP).enabled(cropped).tip("All of the camera's picture again").show(ui).clicked() {
                    self.webcam_uncropped(aspect);
                    ui.close();
                }
                crate::ui_kit::menu_sep(ui);
                let change = |this: &mut Self, f: &dyn Fn(Placement) -> Placement| {
                    if let Some(w) = this.settings.webcam.as_mut() {
                        w.placement = PlacementCfg::laid_out(f(w.placement.into()), aspect);
                    }
                };
                if Item::new("Turn clockwise").icon(icon::ARROW_CLOCKWISE).show(ui).clicked() {
                    change(self, &|p| webcam_ui::contain(p.turn(1, aspect)));
                    ui.close();
                }
                if Item::new("Turn anticlockwise").icon(icon::ARROW_COUNTER_CLOCKWISE).show(ui).clicked() {
                    change(self, &|p| webcam_ui::contain(p.turn(-1, aspect)));
                    ui.close();
                }
                crate::ui_kit::menu_sep(ui);
                let p: Option<Placement> = self.webcam_placement();
                if Item::new("Mirror").icon(icon::FLIP_HORIZONTAL).check(p.is_some_and(|p| p.flip_h)).tip("Flip left to right").show(ui).clicked() {
                    change(self, &|p| Placement { flip_h: !p.flip_h, ..p });
                    ui.close();
                }
                if Item::new("Flip upside down").icon(icon::FLIP_VERTICAL).check(p.is_some_and(|p| p.flip_v)).show(ui).clicked() {
                    change(self, &|p| Placement { flip_v: !p.flip_v, ..p });
                    ui.close();
                }
            }
        }
        // Which is in front: only with both in the video.
        let both = self.settings.screen.enabled && self.settings.webcam.as_ref().is_some_and(|w| w.enabled);
        if both {
            crate::ui_kit::menu_sep(ui);
            let in_front = (layer == Layer::Screen) == self.settings.screen.in_front;
            let (label, ic) = if in_front { ("Send to back", icon::SELECTION_BACKGROUND) } else { ("Bring to front", icon::SELECTION_FOREGROUND) };
            if Item::new(label).icon(ic).show(ui).clicked() {
                self.settings.screen.in_front = !self.settings.screen.in_front;
                ui.close();
            }
        }
        if layer == Layer::Webcam {
            crate::ui_kit::menu_sep(ui);
            if Item::new("Remove the webcam").icon(icon::TRASH).danger().tip("Take it out of your clips, and forget its place").show(ui).clicked() {
                self.settings.webcam = None;
                ui.close();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), vec2(w, h))
    }

    fn close(a: Rect, b: Rect) -> bool {
        (a.min - b.min).length() < 1e-4 && (a.max - b.max).length() < 1e-4
    }

    /// The screen's box moves inside the video, snapping to its middle and
    /// to the webcam's edges; a corner scales it from the opposite one,
    /// keeping its shape, no smaller than the least.
    #[test]
    fn the_screens_box() {
        let none = Lines::default();
        let start = rect(0.1, 0.1, 0.4, 0.4);
        assert!(close(screen_drag(start, Handle::Move, 0.1, 0.0, false, &none), rect(0.2, 0.1, 0.4, 0.4)));
        assert!(close(screen_drag(start, Handle::Move, 0.9, 0.9, false, &none), rect(0.6, 0.6, 0.4, 0.4)));
        // Its middle onto the video's, from close by.
        assert!(close(screen_drag(start, Handle::Move, 0.195, 0.0, true, &none), rect(0.3, 0.1, 0.4, 0.4)));
        // Its right edge onto the webcam's left.
        let webcam = Lines { xs: vec![0.75], ys: vec![] };
        assert!(close(screen_drag(start, Handle::Move, 0.245, 0.0, true, &webcam), rect(0.35, 0.1, 0.4, 0.4)));
        // The bottom-right corner, dragged out: twice as big from the top-left.
        let r = screen_drag(start, Handle::SE, 0.4, 0.4, false, &none);
        assert!(close(r, rect(0.1, 0.1, 0.8, 0.8)), "{r:?}");
        // Only as big as fits.
        let r = screen_drag(start, Handle::SE, 2.0, 2.0, false, &none);
        assert!(close(r, rect(0.1, 0.1, 0.9, 0.9)), "{r:?}");
        // Never smaller than the least, the other corner where it was.
        let r = screen_drag(start, Handle::NW, 1.0, 1.0, false, &none);
        assert!((r.width() - MIN_SIZE).abs() < 1e-4 && (r.max - start.max).length() < 1e-4, "{r:?}");
    }

    /// Cropped with Alt: the picture keeps its scale and place, the box
    /// follows the crop's edge; it can't be uncropped past the display or
    /// out of the video, and all of the display again is no crop.
    #[test]
    fn cropping_the_screen() {
        let crop = |w, h, x, y| Some(ScreenCrop { w, h, at: Some([x, y]) });
        // Filling the video: the middle 1920×1080 of a 2560×1440 display,
        // its left edge dragged a quarter of the video in.
        let (b, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(320.0, 180.0, 1920.0, 1080.0), (2560, 1440), Handle::W, 0.25, 0.0, false);
        assert!(close(b, rect(0.25, 0.0, 0.75, 1.0)), "{b:?}");
        assert_eq!(c, crop(1440, 1080, 800, 180));
        // Out past the video's edge: the box stays in the video.
        let (b, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(320.0, 180.0, 1920.0, 1080.0), (2560, 1440), Handle::W, -0.5, 0.0, false);
        assert!(close(b, rect(0.0, 0.0, 1.0, 1.0)), "{b:?}");
        assert_eq!(c, crop(1920, 1080, 320, 180));
        // In a box of its own: its right edge out to the display's, then all of it.
        let (b, c) = screen_crop_drag(rect(0.1, 0.1, 0.4, 0.4), rect(0.0, 0.0, 1280.0, 1440.0), (2560, 1440), Handle::E, 0.9, 0.0, false);
        assert!(close(b, rect(0.1, 0.1, 0.8, 0.4)), "{b:?}");
        assert_eq!(c, None);
        // Never less than 64 of its pixels, on even ones.
        let (_, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(0.0, 0.0, 2560.0, 1440.0), (2560, 1440), Handle::SE, -2.0, -2.0, false);
        assert_eq!(c, crop(64, 64, 0, 0));
        let (_, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(0.0, 0.0, 2560.0, 1440.0), (2560, 1440), Handle::N, 0.0, 0.10011, false);
        assert_eq!(c, crop(2560, 1296, 0, 144));
    }

    /// Guides show for the lines a box is on.
    #[test]
    fn guides_for_what_it_snapped_to() {
        let webcam = Lines { xs: vec![0.75], ys: vec![0.2] };
        assert_eq!(snapped(rect(0.35, 0.3, 0.4, 0.4), &webcam), vec![(true, 0.75), (false, 0.5)]);
        assert!(snapped(rect(0.31, 0.31, 0.2, 0.2), &webcam).is_empty());
    }
}
