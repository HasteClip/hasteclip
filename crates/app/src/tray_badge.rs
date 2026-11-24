//! What the tray icon says about capture: the glyph with a small badge in its
//! bottom-right corner. Drawn here, over the tray's own glyph images, rather
//! than shipped as more image files: one set of shapes for every size trays
//! use (16 px and up), and nothing to regenerate.
//!
//! - Idle: the glyph alone.
//! - Clipping (the replay buffer is on): a green dot.
//! - Recording: a red dot. Streaming: a purple dot (the app's own colours).
//! - A clip just saved: a green disc with a check, for a couple of seconds.
//!
//! macOS shows the menu bar icon as a template image, which macOS tints itself
//! and so has no colours: there the badges are shapes in one colour instead
//! (a ring for clipping, a solid dot for recording, a check for saved).

use std::time::Duration;

use image::{Rgba, RgbaImage};

/// How long the "saved" check stays up.
pub const SAVED_FOR: Duration = Duration::from_secs(2);

/// What the icon shows. Ordered by what wins when several are true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Clipping,
    Recording,
    Streaming,
    /// A clip just finished saving.
    Saved,
}

impl Status {
    /// What's on: recording and streaming over the replay buffer; a clip that
    /// just saved over them all, for its moment.
    pub fn of(clipping: bool, recording: bool, streaming: bool, just_saved: bool) -> Self {
        if just_saved {
            Self::Saved
        } else if streaming {
            Self::Streaming
        } else if recording {
            Self::Recording
        } else if clipping {
            Self::Clipping
        } else {
            Self::Idle
        }
    }

    /// The tray's tooltip.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Idle => "HasteClip: not capturing",
            Self::Clipping => "HasteClip: clipping (replay buffer on)",
            Self::Recording => "HasteClip: recording",
            Self::Streaming => "HasteClip: streaming",
            Self::Saved => "HasteClip: clip saved",
        }
    }

    // The app's own colours for each (theme.rs: live, rec, stream).
    const CLIPPING: [u8; 3] = [0x3D, 0xD6, 0x8C];
    const RED: [u8; 3] = [0xFF, 0x50, 0x50];
    const PURPLE: [u8; 3] = [0xA9, 0x70, 0xFF];
    /// The saved check's disc: a deeper green than clipping's dot, so the
    /// check (and its white mark) reads as its own thing.
    const GREEN: [u8; 3] = [0x12, 0xA4, 0x67];
}

/// `base` (the glyph, at the size it's shown) with `status`'s badge on it.
/// `mono`: one colour, for a template image.
pub fn badged(base: &RgbaImage, status: Status, mono: bool) -> RgbaImage {
    let mut img = base.clone();
    if status == Status::Idle {
        return img;
    }
    // In colour, what's capturing tints the whole glyph (white times the
    // colour; the dark keyline stays dark): a dot covered part of it, and it
    // was hard to tell what the icon was. A template has no colour to tint
    // with, and a saved clip is a badge on the white glyph: both below.
    let tint = match status {
        Status::Clipping => Some(Status::CLIPPING),
        Status::Recording => Some(Status::RED),
        Status::Streaming => Some(Status::PURPLE),
        _ => None,
    };
    if let (Some(rgb), false) = (tint, mono) {
        for p in img.pixels_mut() {
            for i in 0..3 {
                p[i] = (u16::from(p[i]) * u16::from(rgb[i]) / 255) as u8;
            }
        }
        return img;
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let size = w.min(h);
    // About a third of the icon across, no smaller than 6 px: legible at 16.
    let r = (size * 0.2).max(3.0);
    let (cx, cy) = (w - r - 0.5, h - r - 0.5);
    // Clear a gap round the badge, so it doesn't run into the glyph.
    let gap = r + (size / 16.0).clamp(1.0, 2.0);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let d = dist(x, y, cx, cy);
        let keep = 1.0 - cover(d - gap);
        p[3] = (f32::from(p[3]) * keep).round() as u8;
    }
    let ring_width = (r * 0.45).max(1.4);
    let stroke = (r * 0.2).max(0.7);
    // The check, in the badge's own units (the badge's radius is 1).
    let check = [((-0.48, 0.02), (-0.12, 0.4)), ((-0.12, 0.4), (0.5, -0.36))];
    for (x, y, p) in img.enumerate_pixels_mut() {
        let d = dist(x, y, cx, cy);
        let (px, py) = ((x as f32 + 0.5 - cx) / r, (y as f32 + 0.5 - cy) / r);
        let on_check = || check.iter().map(|&(a, b)| segment_dist((px, py), a, b)).fold(f32::MAX, f32::min) * r - stroke;
        let (colour, a): ([u8; 3], f32) = match (status, mono) {
            (Status::Idle, _) => continue,
            (Status::Clipping, false) => (Status::CLIPPING, cover(d - r)),
            (Status::Recording, false) => (Status::RED, cover(d - r)),
            (Status::Streaming, false) => (Status::PURPLE, cover(d - r)),
            (Status::Saved, false) => {
                // A green disc, the check white on it.
                over(p, Status::GREEN, cover(d - r));
                ([255, 255, 255], cover(on_check()) * cover(d - r))
            }
            // Template: only the alpha counts.
            (Status::Clipping, true) => ([0; 3], cover(d - r) - cover(d - (r - ring_width))),
            (Status::Recording | Status::Streaming, true) => ([0; 3], cover(d - r)),
            (Status::Saved, true) => ([0; 3], cover(on_check()) * cover(d - r)),
        };
        over(p, colour, a.clamp(0.0, 1.0));
    }
    img
}

fn dist(x: u32, y: u32, cx: f32, cy: f32) -> f32 {
    ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt()
}

/// How much of a pixel is inside a shape, given its signed distance to the
/// shape's edge (negative inside).
fn cover(signed: f32) -> f32 {
    (0.5 - signed).clamp(0.0, 1.0)
}

fn segment_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
    ((p.0 - a.0 - abx * t).powi(2) + (p.1 - a.1 - aby * t).powi(2)).sqrt()
}

/// `colour` at coverage `a` laid over `p` (straight alpha, both).
fn over(p: &mut Rgba<u8>, colour: [u8; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let below = f32::from(p[3]) / 255.0;
    let out = a + below * (1.0 - a);
    for i in 0..3 {
        let c = (f32::from(colour[i]) * a + f32::from(p[i]) * below * (1.0 - a)) / out;
        p[i] = c.round().clamp(0.0, 255.0) as u8;
    }
    p[3] = (out * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(size: u32) -> RgbaImage {
        RgbaImage::from_pixel(size, size, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn status_picks_the_most_important() {
        assert_eq!(Status::of(false, false, false, false), Status::Idle);
        assert_eq!(Status::of(true, false, false, false), Status::Clipping);
        assert_eq!(Status::of(true, true, false, false), Status::Recording);
        assert_eq!(Status::of(true, true, true, false), Status::Streaming);
        assert_eq!(Status::of(true, false, true, false), Status::Streaming);
        assert_eq!(Status::of(true, true, true, true), Status::Saved);
    }

    #[test]
    fn idle_is_the_glyph_untouched() {
        assert_eq!(badged(&glyph(16), Status::Idle, false), glyph(16));
    }

    /// What's capturing tints the whole glyph, at every size trays use; a
    /// saved clip is a green badge bottom-right on the white glyph; on a
    /// template, shapes bottom-right.
    #[test]
    fn tints_and_badges() {
        for size in [16, 20, 24, 32, 48, 64] {
            for (status, rgb) in [(Status::Clipping, Status::CLIPPING), (Status::Recording, Status::RED), (Status::Streaming, Status::PURPLE)] {
                let img = badged(&glyph(size), status, false);
                let p = img.get_pixel(0, 0);
                assert_eq!([p[0], p[1], p[2], p[3]], [rgb[0], rgb[1], rgb[2], 255], "{status:?} at {size}");
            }
            let saved = badged(&glyph(size), Status::Saved, false);
            assert_eq!(*saved.get_pixel(0, 0), Rgba([255; 4]));
            let c = size - (size as f32 * 0.2).max(3.0) as u32 - 1;
            let p = saved.get_pixel(c, c + 2.min(size - 1 - c));
            assert!(p[1] > p[0] + 60 && p[1] > p[2] + 30, "saved at {size}: {p:?}");
        }
    }

    /// Distinct looks for every state, on a template too.
    #[test]
    fn states_differ() {
        for mono in [false, true] {
            let all: Vec<_> = [Status::Idle, Status::Clipping, Status::Recording, Status::Saved].iter().map(|s| badged(&glyph(36), *s, mono)).collect();
            for i in 0..all.len() {
                for j in i + 1..all.len() {
                    assert_ne!(all[i], all[j], "mono {mono}: {i} vs {j}");
                }
            }
        }
    }
}
