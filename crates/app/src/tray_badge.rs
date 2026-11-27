//! What the tray icon says about capture.
//!
//! - Idle: the glyph as it is.
//! - Clipping (the replay buffer is on): the clapperboard green. Recording:
//!   red. Streaming: purple (the app's own colours: `theme.rs`). The horse
//!   stays white. Windows and Linux show these as ready-made images, one per
//!   state and size trays use (`assets/tray/`, made by `scripts/icons.sh`).
//! - macOS shows the menu bar icon as a template image, which macOS tints
//!   itself and so can't be coloured: there a small badge in one colour sits
//!   bottom-right instead (a ring for clipping, a solid dot for recording, a
//!   ring with a dot in it for streaming).

#[cfg(any(target_os = "macos", test))]
use image::{Rgba, RgbaImage};

/// What the icon shows. What's on, the most important first: streaming wins
/// over recording, recording over clipping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Clipping,
    Recording,
    Streaming,
}

impl Status {
    pub fn of(clipping: bool, recording: bool, streaming: bool) -> Self {
        if streaming {
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
        }
    }
}

/// The glyph's PNGs for `status`, at the sizes trays show (16, 20, 24, 28,
/// 32, 40, 48 and 64 px).
#[cfg(any(windows, target_os = "linux"))]
pub fn glyph_pngs(status: Status) -> [&'static [u8]; 8] {
    macro_rules! sizes {
        ($name:literal) => {
            [
                include_bytes!(concat!("../assets/tray/", $name, "-16.png")).as_slice(),
                include_bytes!(concat!("../assets/tray/", $name, "-20.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-24.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-28.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-32.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-40.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-48.png")),
                include_bytes!(concat!("../assets/tray/", $name, "-64.png")),
            ]
        };
    }
    match status {
        Status::Idle => sizes!("glyph"),
        Status::Clipping => sizes!("clipping"),
        Status::Recording => sizes!("recording"),
        Status::Streaming => sizes!("streaming"),
    }
}

/// macOS: `base` (the template glyph) with `status`'s badge in its corner,
/// in black (only its shape counts, macOS tints it).
#[cfg(any(target_os = "macos", test))]
pub fn mono_badge(base: &RgbaImage, status: Status) -> RgbaImage {
    let mut img = base.clone();
    if status == Status::Idle {
        return img;
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let size = w.min(h);
    let r = (size * 0.2).max(3.0);
    let (cx, cy) = (w - r - 0.5, h - r - 0.5);
    // Clear a gap round the badge, so it doesn't run into the glyph.
    let gap = r + (size / 16.0).clamp(1.0, 2.0);
    let ring_width = (r * 0.45).max(1.4);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
        p[3] = (f32::from(p[3]) * (1.0 - cover(d - gap))).round() as u8;
        let ring = cover(d - r) - cover(d - (r - ring_width));
        let a = match status {
            Status::Idle => 0.0,
            Status::Clipping => ring,
            Status::Recording => cover(d - r),
            Status::Streaming => ring.max(cover(d - r * 0.35)),
        };
        over(p, a.clamp(0.0, 1.0));
    }
    img
}

/// How much of a pixel is inside a shape, given its signed distance to the
/// shape's edge (negative inside).
#[cfg(any(target_os = "macos", test))]
fn cover(signed: f32) -> f32 {
    (0.5 - signed).clamp(0.0, 1.0)
}

/// Black at coverage `a` laid over `p` (straight alpha).
#[cfg(any(target_os = "macos", test))]
fn over(p: &mut Rgba<u8>, a: f32) {
    if a <= 0.0 {
        return;
    }
    let below = f32::from(p[3]) / 255.0;
    let out = a + below * (1.0 - a);
    for c in &mut p.0[..3] {
        *c = (f32::from(*c) * below * (1.0 - a) / out).round() as u8;
    }
    p[3] = (out * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_picks_the_most_important() {
        assert_eq!(Status::of(false, false, false), Status::Idle);
        assert_eq!(Status::of(true, false, false), Status::Clipping);
        assert_eq!(Status::of(true, true, false), Status::Recording);
        assert_eq!(Status::of(true, true, true), Status::Streaming);
        assert_eq!(Status::of(true, false, true), Status::Streaming);
    }

    /// Every state has its images at every size, the size they say, with the
    /// clapperboard in the state's colour and the horse still white.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn state_images_are_there() {
        let want = [(Status::Clipping, [0x3D, 0xD6, 0x8C]), (Status::Recording, [0xFF, 0x50, 0x50]), (Status::Streaming, [0xA9, 0x70, 0xFF])];
        for (status, rgb) in want {
            for (png, size) in glyph_pngs(status).iter().zip([16, 20, 24, 28, 32, 40, 48, 64]) {
                let img = image::load_from_memory(png).unwrap().to_rgba8();
                assert_eq!(img.dimensions(), (size, size), "{status:?}");
                let near = |p: &Rgba<u8>| p[3] == 255 && (0..3).all(|i| p[i].abs_diff(rgb[i]) <= 3);
                assert!(img.pixels().filter(|p| near(p)).count() > (size * size / 12) as usize, "{status:?} at {size}: too little {rgb:?}");
                assert!(img.pixels().any(|p| p[3] == 255 && p.0[..3] == [255, 255, 255]), "{status:?} at {size}: no white horse");
            }
        }
    }

    /// Distinct looks for every state, on a template.
    #[test]
    fn mono_states_differ() {
        let glyph = RgbaImage::from_pixel(36, 36, Rgba([0, 0, 0, 255]));
        let all: Vec<_> = [Status::Idle, Status::Clipping, Status::Recording, Status::Streaming].iter().map(|s| mono_badge(&glyph, *s)).collect();
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j], "{i} vs {j}");
            }
        }
    }
}
