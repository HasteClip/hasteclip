//! The picture's sources on the Sources page's preview, placed the way OBS
//! places them: the screen and the webcam, each dragged to move it and by a
//! handle to resize it (never stretched), one in front of the other. The
//! webcam also crops (Alt with a handle), flips (past its opposite side) and
//! turns (its right-click menu): see `webcam_ui`. Moves snap to the video's
//! edges and middle and to the other source's edges (Ctrl to place freely).
//!
//! The screen fills the video until a corner makes it smaller: then it's in
//! a box of its own showing all of it, in its own shape
//! (`settings::ScreenCfg::placed`), and Reset puts it back. Both placements
//! are shared with the capture, so what's dragged is what's recorded, live.

use capture::webcam::Placement;
use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, vec2};
use egui_phosphor::regular as icon;

use crate::App;
use crate::settings::{BoxCfg, PlacementCfg, ScreenCfg};
use crate::ui_kit::Item;
use crate::webcam_ui::{self, Handle, Lines, Mods};

/// Handle size, in points.
const HANDLE: f32 = 8.0;
/// A source's outline on the preview.
const ACCENT: Color32 = Color32::WHITE;
/// OBS-ish crop colour.
const CROP: Color32 = Color32::from_rgb(80, 220, 120);
/// Smallest the screen's box gets, as a fraction of the video.
const MIN_SIZE: f32 = 0.05;

/// One of the picture's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Where the sources are, as undo keeps it: the screen (its box, in front
/// or behind) and the webcam's placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Arrangement {
    screen: ScreenCfg,
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

impl App {
    /// The screen's picture's size: what's kept of it (the main display's,
    /// for games and apps), asked for now and then.
    pub(crate) fn screen_picture(&mut self) -> Option<(u32, u32)> {
        let source = self.video_source();
        if self.canvas.picture.as_ref().is_none_or(|(s, _, at)| *s != source || at.elapsed() > std::time::Duration::from_secs(2)) {
            let size = capture::frame_size(&source, Default::default());
            self.canvas.picture = Some((source, size, std::time::Instant::now()));
        }
        self.canvas.picture.as_ref().and_then(|(_, size, _)| *size)
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
        Arrangement { screen: self.settings.screen, webcam: self.settings.webcam.as_ref().map(|w| w.placement) }
    }

    /// `was` (how the sources were before a change just made) can be gone
    /// back to.
    pub(crate) fn keep_for_undo(&mut self, was: Arrangement) {
        if self.canvas.undo.last() != Some(&was) {
            self.canvas.undo.push(was);
            if self.canvas.undo.len() > 100 {
                self.canvas.undo.remove(0);
            }
        }
        self.canvas.redo.clear();
    }

    fn arrange(&mut self, a: Arrangement) {
        self.settings.screen = a.screen;
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
                let drag = match layer {
                    Layer::Webcam => Drag::Webcam(handle, from, self.webcam_placement().unwrap_or_else(Placement::hidden)),
                    Layer::Screen => self.take_screen(handle, from, aspect, &layers),
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
                    Drag::Stuck(_) => {}
                }
                ui.ctx().set_cursor_icon(match drag {
                    Drag::Webcam(Handle::Move, ..) | Drag::Screen(Handle::Move, ..) => CursorIcon::Grabbing,
                    Drag::Webcam(h, ..) | Drag::Screen(h, ..) => h.cursor(),
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
            Drag::Screen(h, ..) => (Layer::Screen, Some(h)),
            Drag::Stuck(l) => (l, None),
        });
        let over = pointer.and_then(|p| hit(&layers, &to_screen, p));
        let Some((active, handle)) = held.or(over.map(|(l, h)| (l, Some(h)))) else { return };
        if held.is_none() && self.canvas.menu.is_some() {
            return;
        }
        let Some(rect) = layers.iter().find(|(l, _)| *l == active).map(|(_, r)| *r) else { return };
        let bx = to_screen(rect);
        let painter = ui.painter().clone();
        let webcam: Option<Placement> = (active == Layer::Webcam).then(|| self.webcam_placement()).flatten();
        let cropping = modifiers.alt && webcam.is_some() && handle.is_some_and(|h| h != Handle::Move);

        // Cropping: where the whole camera picture would be, dashed, so it's
        // clear what's cut off and how far it can grow back.
        if let Some(p) = webcam.filter(|_| cropping) {
            let sx = p.w / (1.0 - p.crop[0] - p.crop[2]).max(0.01);
            let sy = p.h / (1.0 - p.crop[1] - p.crop[3]).max(0.01);
            let (cl, cr) = if p.flip_h { (p.crop[2], p.crop[0]) } else { (p.crop[0], p.crop[2]) };
            let (ct, cb) = if p.flip_v { (p.crop[3], p.crop[1]) } else { (p.crop[1], p.crop[3]) };
            let whole = Rect::from_min_max(at(Pos2::new(p.x - cl * sx, p.y - ct * sy)), at(Pos2::new(p.x + p.w + cr * sx, p.y + p.h + cb * sy)));
            if whole != bx {
                let ghost = Stroke::new(1.0, CROP.gamma_multiply(0.8));
                let c = [whole.left_top(), whole.right_top(), whole.right_bottom(), whole.left_bottom(), whole.left_top()];
                painter.extend(egui::Shape::dashed_line(&c, ghost, 4.0, 3.0));
            }
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
        // mirrored picture's left is the camera's right), are green if cropped.
        let [cl, ct, cr, cb] = webcam.map_or([false; 4], |p| p.crop.map(|c| c > 0.0));
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

        // Handles: round, white, green while Alt is held on the webcam (they'll
        // crop); the one held or under the pointer a little bigger.
        for h in Handle::ALL {
            let r = if Some(h) == handle { HANDLE * 0.65 } else { HANDLE * 0.5 };
            let fill = if modifiers.alt && webcam.is_some() { CROP } else { Color32::WHITE };
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
            (Some(Drag::Webcam(..)), _) if modifiers.alt && modifiers.shift => "Cropping, keeping the shape".to_owned(),
            (Some(Drag::Webcam(..)), _) if modifiers.alt => "Cropping · Shift keeps the shape".to_owned(),
            (Some(Drag::Webcam(..)), _) => format!("{alt}: crop · past the other side: flip"),
            (Some(Drag::Screen(..)), _) => return,
            (None, Layer::Webcam) => format!("Drag to move · {alt} + handle to crop · right-click for more"),
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

    /// The screen back over all of the video (Reset).
    pub(crate) fn screen_to_whole(&mut self) {
        self.settings.screen.placed = None;
    }

    /// The screen's place, on Sources: over all of the video, or in a box
    /// placed on the preview (Reset puts it back); and whether it's in
    /// front of the webcam. Changes apply at once, while clipping too.
    pub(crate) fn screen_place_card(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let placed = self.settings.screen.placed.is_some();
        let webcam_on = self.settings.webcam.as_ref().is_some_and(|w| w.enabled);
        let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                ui.set_min_height(crate::sources_ui::GROUP_ROW_H);
                ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
            });
        };
        let was = self.arrangement();
        crate::sources_ui::group_rows(ui, "screen_place", |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            row(ui, "Position", &mut |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if crate::ui_kit::Btn::normal("Reset").small().enabled(placed).tip("Over all of the video again").show(ui).clicked() {
                    self.screen_to_whole();
                }
                let state = if placed { "Placed by hand · drag it on the preview" } else { "Over all of the video · drag a corner to make it smaller" };
                ui.label(theme::text(state, 12.5, W::Regular).color(pal.text3));
            });
            if webcam_on && self.settings.screen.enabled {
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "In front of the webcam", &mut |ui| {
                    crate::ui_kit::switch(ui, &mut self.settings.screen.in_front);
                });
            }
        });
        if self.arrangement() != was {
            self.keep_for_undo(was);
        }
    }

    /// A source's right-click menu: its place back as it was, the webcam's
    /// turns and flips, and which is in front.
    fn layer_menu(&mut self, ui: &mut egui::Ui, layer: Layer, aspect: f32) {
        match layer {
            Layer::Screen => {
                if Item::new("Reset position").icon(icon::ARROWS_OUT).enabled(self.settings.screen.placed.is_some()).tip("Over all of the video again").show(ui).clicked() {
                    self.screen_to_whole();
                    ui.close();
                }
            }
            Layer::Webcam => {
                if Item::new("Reset position").icon(icon::ARROWS_IN).tip("Back to the bottom-right corner, uncropped").show(ui).clicked() {
                    self.webcam_to_corner(aspect);
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

    /// Guides show for the lines a box is on.
    #[test]
    fn guides_for_what_it_snapped_to() {
        let webcam = Lines { xs: vec![0.75], ys: vec![0.2] };
        assert_eq!(snapped(rect(0.35, 0.3, 0.4, 0.4), &webcam), vec![(true, 0.75), (false, 0.5)]);
        assert!(snapped(rect(0.31, 0.31, 0.2, 0.2), &webcam).is_empty());
    }
}
