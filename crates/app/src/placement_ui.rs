//! Where a source is in the video, as numbers: its position and size in
//! the video's pixels, and how it's turned and flipped. Under its card's
//! Placement row, right under the preview, so what's typed shows there at
//! once and what's dragged there shows here: the preview's drags, its menu
//! and these are three ways to the same place.

use egui::{Pos2, Rect, vec2};

use crate::App;
use crate::canvas_ui::Layer;
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Kind, Row};

/// A box (fractions of the video) in a `vw`×`vh` video's pixels: left,
/// top, width, height.
pub(crate) fn to_pixels(r: Rect, (vw, vh): (u32, u32)) -> [i64; 4] {
    let px = |v: f32, n: u32| (v as f64 * n as f64).round() as i64;
    [px(r.min.x, vw), px(r.min.y, vh), px(r.width(), vw), px(r.height(), vh)]
}

/// A box in a `vw`×`vh` video's pixels back to fractions of it.
pub(crate) fn from_pixels([x, y, w, h]: [i64; 4], (vw, vh): (u32, u32)) -> Rect {
    let f = |v: i64, n: u32| (v as f64 / n.max(1) as f64) as f32;
    Rect::from_min_size(Pos2::new(f(x, vw), f(y, vh)), vec2(f(w, vw), f(h, vh)))
}

/// Where a source is in a few words, for its card: where (`place`, if it's
/// said), turned and flipped. Empty when there's nothing to say.
pub(crate) fn summary(place: Option<&str>, turns: u8, mirrored: bool, upside_down: bool) -> String {
    let turned = match turns % 4 {
        1 => Some("turned 90°"),
        2 => Some("turned 180°"),
        3 => Some("turned 270°"),
        _ => None,
    };
    let parts: Vec<&str> = [place, turned, mirrored.then_some("mirrored"), upside_down.then_some("upside down")].into_iter().flatten().collect();
    let mut text = parts.join(" · ");
    if let Some(first) = text.get(..1) {
        text.replace_range(..1, &first.to_uppercase());
    }
    text
}

impl App {
    /// A source's card open, its Placement numbers open under it, and
    /// brought into view (its menu's Edit placement).
    pub(crate) fn open_placement(&mut self, layer: Layer) {
        let v = &mut self.sources_view;
        for open in [&mut v.open_cards, &mut v.placement_open] {
            if !open.contains(&layer) {
                open.push(layer);
            }
        }
        v.placement_shown = Some(layer);
    }

    /// A source's Placement row on its card: where it is in a few words, how
    /// the video puts the screen in (a video of a shape of its own), and its
    /// numbers, opening under it.
    pub(crate) fn placement_row(&mut self, ui: &mut egui::Ui, layer: Layer) {
        let pal = theme::pal(ui);
        let screen = layer == Layer::Screen;
        let shaped = screen && self.video_shaped();
        let text = match layer {
            Layer::Screen => {
                let s = self.settings.screen;
                // Shaped, Fill, Fit or Stretch beside it say where it is.
                let place = match s.placed {
                    Some(_) => Some("placed by hand"),
                    None if shaped => None,
                    None => Some("fills the video"),
                };
                summary(place, s.turns, s.flip_h, s.flip_v)
            }
            Layer::Webcam => {
                let Some(p) = self.webcam_placement() else { return };
                let (position, size) = self.webcam_resettable();
                summary(Some(if position || size { "placed by hand" } else { "in the bottom-right corner" }), p.turns, p.flip_h, p.flip_v)
            }
        };
        let open = self.sources_view.placement_open.contains(&layer);
        let was = self.arrangement();
        let mut toggle = false;
        let row = ui.horizontal(|ui| {
            ui.set_min_height(crate::sources_ui::GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let pad = if text.is_empty() { 0.0 } else { 6.0 };
                ui.add_space(pad);
                ui.label(theme::text("Placement", 13.5, W::Regular).color(pal.text));
                if !text.is_empty() {
                    ui.label(theme::text(&text, 12.0, W::Regular).color(pal.text3));
                    ui.add_space(pad);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let caret = if open { egui_phosphor::regular::CARET_UP } else { egui_phosphor::regular::CARET_DOWN };
                let tip = if open { "Hide its numbers" } else { "Its position, size, turn and flip, as numbers" };
                toggle = Btn::normal("Adjust").trail(caret).small().tip(tip).show(ui).clicked();
                if shaped && let Some(f) = crate::canvas_ui::framing_choice(ui, "screen_card_framing", self.settings.screen.placed.is_none().then_some(self.settings.framing)) {
                    self.screen_framed(f);
                }
            });
        });
        // Opened from its menu: in view, with room for the numbers under it.
        if self.sources_view.placement_shown == Some(layer) {
            self.sources_view.placement_shown = None;
            ui.scroll_to_rect(row.response.rect, Some(egui::Align::TOP));
        }
        if toggle {
            let v = &mut self.sources_view.placement_open;
            if open { v.retain(|l| *l != layer) } else { v.push(layer) }
        }
        self.placement_numbers(ui, layer, open);
        if self.arrangement() != was {
            self.keep_for_undo(was);
        }
    }

    /// A source's position and size in the video's pixels, each with its
    /// Reset, and how it's turned and flipped: in a well under its Placement
    /// row (sliding open and shut with `open`).
    fn placement_numbers(&mut self, ui: &mut egui::Ui, layer: Layer, open: bool) {
        use egui_phosphor::regular as icon;
        let Some((vw, vh)) = capture::frame_size(&self.video_source(), self.settings.video_size()) else { return };
        let aspect = vw as f32 / vh.max(1) as f32;
        let screen = layer == Layer::Screen;
        let rect = match layer {
            Layer::Screen => self.screen_rect(aspect),
            Layer::Webcam => match self.webcam_placement() {
                Some(p) => Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h)),
                None => return,
            },
        };
        let [x, y, w, h] = to_pixels(rect, (vw, vh));
        let (can_place, can_size) = if screen { self.screen_resettable(aspect) } else { self.webcam_resettable() };
        let (turns, mirrored, upside_down) = match layer {
            Layer::Screen => (self.settings.screen.turns, self.settings.screen.flip_h, self.settings.screen.flip_v),
            Layer::Webcam => self.webcam_placement().map_or((0, false, false), |p| (p.turns, p.flip_h, p.flip_v)),
        };
        let shaped = screen && self.video_shaped();
        let size_id = if screen { "placement_screen_size" } else { "placement_webcam_size" };
        // The screen's size is linked at first (its shape kept, as dragging
        // keeps it); the webcam's always is.
        let linked_once = egui::Id::new((size_id, "linked"));
        if screen && open && ui.data(|d| d.get_temp::<bool>(linked_once)).is_none() {
            ui.data_mut(|d| d.insert_temp(linked_once, true));
            ui_kit::set_size_field_keeps(ui.ctx(), size_id, Some(w as f64 / h.max(1) as f64));
        }
        let pal = theme::pal(ui);
        let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
        const BOX: f32 = 64.0;

        let mut moved: Option<[i64; 4]> = None;
        let (mut reset_place, mut reset_size, mut turn, mut flip) = (false, false, None, None);
        ui_kit::well(ui, ("placement_well", layer), open, |l| {
            // Its top-left corner. The screen can reach past the video's
            // edges (a part of it over all of the video), some of it in it.
            let (xs, ys) = if screen { (2 - w..=vw as i64 - 2, 2 - h..=vh as i64 - 2) } else { (0..=(vw as i64 - w).max(0), 0..=(vh as i64 - h).max(0)) };
            l.row(Row::new("Position").kid(), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                reset_place = Btn::ghost("Reset").small().enabled(can_place).tip(if screen { "Back in the middle of the video, its size kept" } else { "Back to the bottom-right corner, its size kept" }).show(ui).clicked();
                ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(v) = ui_kit::signed_box(ui, egui::Id::new((size_id, "x")), x, xs.clone(), BOX) {
                        moved = Some([v, y, w, h]);
                    }
                    small(ui, ",");
                    if let Some(v) = ui_kit::signed_box(ui, egui::Id::new((size_id, "y")), y, ys.clone(), BOX) {
                        moved = Some([x, v, w, h]);
                    }
                    small(ui, "px");
                });
            });
            // Its size, its top-left corner staying put. The webcam keeps
            // its shape (its crop's); the screen too while it's linked.
            l.row(Row::new("Size").kid(), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let size_tip = match layer {
                    Layer::Screen if shaped => "Back to the size the video gives it, where it is",
                    Layer::Screen => "As big as the video again, where it is",
                    Layer::Webcam => "A quarter of the video wide again, where it is",
                };
                reset_size = Btn::ghost("Reset").small().enabled(can_size).tip(size_tip).show(ui).clicked();
                if screen {
                    let most = (vw.max(vh) * 8).min(capture::MAX_SIDE * 8);
                    if let Some((nw, nh)) = ui_kit::size_field(ui, size_id, (w.max(2) as u32, h.max(2) as u32), 16..=most, 16..=most) {
                        moved = Some([x, y, nw as i64, nh as i64]);
                    }
                } else {
                    ui.allocate_ui_with_layout(vec2(BOX + 6.0 + 80.0, ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if let Some(nw) = ui_kit::number_box(ui, egui::Id::new((size_id, "w")), w.max(2) as u32, 32..=vw, BOX) {
                            let nh = (nw as f64 * h as f64 / w.max(1) as f64).round() as i64;
                            moved = Some([x, y, nw as i64, nh]);
                        }
                        small(ui, &format!("× {h} px"));
                    });
                }
            });
            l.row(Row::new("Turn").kid(), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if Btn::icon(Kind::Normal, icon::ARROW_CLOCKWISE).small().tip("Turn clockwise").show(ui).clicked() {
                    turn = Some(1);
                }
                if Btn::icon(Kind::Normal, icon::ARROW_COUNTER_CLOCKWISE).small().tip("Turn anticlockwise").show(ui).clicked() {
                    turn = Some(-1);
                }
                ui.add_space(4.0);
                small(ui, ["As it is", "90° clockwise", "180°", "90° anticlockwise"][usize::from(turns % 4)]);
            });
            let (mut m, mut u) = (mirrored, upside_down);
            l.row(Row::new("Mirror").kid(), |ui| ui_kit::switch(ui, &mut m).on_hover_text("Left and right swapped"));
            l.row(Row::new("Upside down").kid(), |ui| ui_kit::switch(ui, &mut u));
            if m != mirrored {
                flip = Some(true);
            } else if u != upside_down {
                flip = Some(false);
            }
        });

        if reset_place {
            if screen { self.screen_reset_position(aspect) } else { self.webcam_reset_position(aspect) }
        }
        if reset_size {
            if screen { self.screen_reset_size(aspect) } else { self.webcam_reset_size(aspect) }
        }
        if let Some(px) = moved {
            let r = from_pixels(px, (vw, vh));
            match layer {
                Layer::Screen => self.place_or_frame(r, aspect),
                Layer::Webcam => {
                    if let Some(cam) = self.settings.webcam.as_mut() {
                        let p: capture::webcam::Placement = cam.placement.into();
                        let p = crate::webcam_ui::contain(capture::webcam::Placement { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), ..p });
                        cam.placement = crate::settings::PlacementCfg::laid_out(p, aspect);
                    }
                }
            }
        }
        if let Some(by) = turn {
            self.turn_layer(layer, by, aspect);
        }
        if let Some(across) = flip {
            self.flip_layer(layer, across, aspect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box in the video's pixels and back, past its edges too.
    #[test]
    fn boxes_in_pixels() {
        let video = (1920, 1080);
        let r = Rect::from_min_size(Pos2::new(0.75, 0.75), vec2(0.25, 0.25));
        assert_eq!(to_pixels(r, video), [1440, 810, 480, 270]);
        let past = Rect::from_min_size(Pos2::new(-0.1, 0.0), vec2(1.2, 1.0));
        assert_eq!(to_pixels(past, video), [-192, 0, 2304, 1080]);
        let back = from_pixels([-192, 0, 2304, 1080], video);
        assert!((back.min.x + 0.1).abs() < 1e-6 && (back.width() - 1.2).abs() < 1e-6, "{back:?}");
    }

    /// What a card says: only what's been done, the first word capitalised.
    #[test]
    fn what_a_card_says() {
        assert_eq!(summary(None, 0, false, false), "");
        assert_eq!(summary(Some("fills the video"), 0, false, false), "Fills the video");
        assert_eq!(summary(Some("placed by hand"), 1, true, false), "Placed by hand · turned 90° · mirrored");
        assert_eq!(summary(None, 2, false, true), "Turned 180° · upside down");
    }
}
