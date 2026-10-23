//! Frames decoded on the CPU (ffmpeg's, as NV12 at the video's own size),
//! uploaded to draw with the same filter as the hardware decoder's: sharp at
//! any size, where egui drawing a smaller RGBA picture was soft. Two sets of
//! textures, in turns, so each new frame is one the stage's light hasn't
//! taken in yet. Uploads and draws reach the GPU in the order they're made:
//! a set isn't written again before the frame in it has been drawn.

use eframe::wgpu;

use super::{Planes, RENDER, Shown};

/// Whether frames can be uploaded and drawn: the renderer is up.
pub fn can_upload() -> bool {
    RENDER.get().is_some()
}

/// One player's textures, made for its video's size. Freed when dropped.
#[derive(Default)]
pub struct Uploads {
    /// A unique number per player, for its bind groups (`forget_pools`).
    pool: usize,
    /// Luma and chroma, per set.
    sets: Vec<(wgpu::TextureView, wgpu::TextureView)>,
    next: usize,
}

impl Uploads {
    /// `nv12` (`width`×`height`, studio range) uploaded, ready to draw.
    /// None without the renderer, or with too few bytes.
    pub fn show(&mut self, width: u32, height: u32, nv12: &[u8], bt601: bool) -> Option<Shown> {
        let rs = RENDER.get()?;
        if nv12.len() < media::nv12_len(width, height) {
            return None;
        }
        if self.pool == 0 {
            // Numbered apart from the hardware decoder's pools, which count from 1.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1 << (usize::BITS - 2));
            self.pool = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if self.sets.first().is_none_or(|(y, _)| y.texture().size() != wgpu::Extent3d { width, height, depth_or_array_layers: 1 }) {
            super::forget_pools(&[self.pool]);
            self.sets = (0..2).map(|_| planes(&rs.device, width, height)).collect();
        }
        let slot = self.next;
        self.next = (slot + 1) % self.sets.len();
        let (y, cbcr) = self.sets[slot].clone();
        write(&rs.queue, (&y, &cbcr), width, height, nv12);
        Some(Shown { key: (self.pool, slot), planes: Planes::Nv12 { y, cbcr, bt601 } })
    }
}

impl Drop for Uploads {
    fn drop(&mut self) {
        if self.pool != 0 {
            super::forget_pools(&[self.pool]);
        }
    }
}

/// Textures for a frame's luma and chroma.
fn planes(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::TextureView, wgpu::TextureView) {
    let texture = |format, w, h| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("uploaded frame"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
            .create_view(&Default::default())
    };
    (texture(wgpu::TextureFormat::R8Unorm, width, height), texture(wgpu::TextureFormat::Rg8Unorm, width.div_ceil(2), height.div_ceil(2)))
}

/// Write an NV12 frame into its textures.
fn write(queue: &wgpu::Queue, (y, cbcr): (&wgpu::TextureView, &wgpu::TextureView), width: u32, height: u32, nv12: &[u8]) {
    let luma = (width * height) as usize;
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let layout = |row| wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None };
    queue.write_texture(y.texture().as_image_copy(), &nv12[..luma], layout(width), y.texture().size());
    queue.write_texture(cbcr.texture().as_image_copy(), &nv12[luma..luma + (cw * ch * 2) as usize], layout(cw * 2), cbcr.texture().size());
}

#[cfg(test)]
pub(super) fn upload_for_test(device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32, nv12: &[u8]) -> (wgpu::TextureView, wgpu::TextureView) {
    let set = planes(device, width, height);
    write(queue, (&set.0, &set.1), width, height, nv12);
    set
}

/// The in-process decoder's frames (`capture::decode`: on Linux, NV12 in
/// memory), uploaded to draw.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct Frames(Uploads);

#[cfg(target_os = "linux")]
impl Frames {
    /// `s`, uploaded and ready to draw.
    pub fn show(&mut self, s: &capture::decode::Surface, width: u32, height: u32) -> Option<Shown> {
        self.0.show(width, height, &s.nv12, s.bt601)
    }
}
