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
            self.pool = next_pool();
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

/// A number for a set of uploaded textures (their bind groups are kept by
/// it), apart from the hardware decoder's pools, which count from 1.
fn next_pool() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1 << (usize::BITS - 2));
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A ring of frame textures filled ahead of the playhead by a thread of
/// its own (`scrub.rs`'s `Uploader`, Linux), so the UI finds its next
/// frame uploaded: copying a 1440p frame to the GPU's memory costs the
/// thread that does it a millisecond or two, which on the UI thread
/// halved how often it drew while scrubbing. The thread only copies, into
/// a buffer mapped in memory the GPU can read ([`Staging`]); the UI
/// thread has the GPU copy it into the slot's textures ([`Ring::flush`]),
/// with its own frame, in order after the last one drawn from the slot
/// and before the next. (Through `Queue::write_texture` from the thread
/// instead, or its own submits, the two threads held each other up in
/// the queue's locks.)
#[cfg(target_os = "linux")]
pub struct Ring {
    pool: usize,
    sets: Vec<(wgpu::TextureView, wgpu::TextureView)>,
    width: u32,
    height: u32,
    staging: std::sync::Mutex<Staging>,
}

/// The buffers a frame goes through on its way to a slot: each is mapped
/// in memory, written by the uploader (then unmapped, for the GPU),
/// copied from by the GPU, and mapped again when that's done.
#[cfg(target_os = "linux")]
struct Staging {
    buffers: Vec<wgpu::Buffer>,
    state: Vec<State>,
    /// Set when a buffer's mapping (asked for after its copy) is done.
    mapped: Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq)]
enum State {
    /// Mapped (once `mapped` says so): the uploader can write it.
    Free,
    /// Being written by the uploader (without the lock).
    Writing,
    /// Written, for the GPU to copy into this slot.
    Filled(usize),
}

/// Staging buffers: one being written, one being copied, one spare.
#[cfg(target_os = "linux")]
const STAGING: usize = 3;

/// Rows in a buffer the GPU copies from start at multiples of this.
#[cfg(target_os = "linux")]
fn padded_row(bytes: u32) -> u32 {
    bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

#[cfg(target_os = "linux")]
impl Ring {
    /// `slots` textures for frames `width`×`height`; none without the renderer.
    pub fn new(width: u32, height: u32, slots: usize) -> Option<Self> {
        let rs = RENDER.get()?;
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let bytes = (padded_row(width) * height + padded_row(cw * 2) * ch) as u64;
        let buffers = (0..STAGING)
            .map(|_| {
                rs.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("frame staging"),
                    size: bytes,
                    usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: true,
                })
            })
            .collect();
        Some(Self {
            pool: next_pool(),
            sets: (0..slots).map(|_| planes(&rs.device, width, height)).collect(),
            width,
            height,
            staging: std::sync::Mutex::new(Staging {
                buffers,
                state: vec![State::Free; STAGING],
                mapped: (0..STAGING).map(|_| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))).collect(),
            }),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The uploader: write `nv12` (studio range, the ring's size) for
    /// `slot` into a mapped buffer, if one is free (false if none is: the
    /// GPU hasn't copied from them yet).
    pub fn fill(&self, slot: usize, nv12: &[u8]) -> bool {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let luma = (w * h) as usize;
        if nv12.len() < luma + (cw * ch * 2) as usize {
            return true;
        }
        // Claimed under the lock, written without it: the UI thread's flush
        // (every redraw while scrubbing) takes the same lock, and waited out
        // the whole copy (a millisecond or two).
        let (k, buffer) = {
            let mut staging = self.staging.lock().unwrap();
            let Some(k) = (0..STAGING).find(|&k| staging.state[k] == State::Free && staging.mapped[k].load(std::sync::atomic::Ordering::Acquire)) else {
                return false;
            };
            staging.state[k] = State::Writing;
            (k, staging.buffers[k].clone())
        };
        let (row_y, row_c) = (padded_row(w) as usize, padded_row(cw * 2) as usize);
        let chroma_at = row_y * h as usize;
        let written = match buffer.slice(..).get_mapped_range_mut() {
            Ok(mut dst) => {
                let src_y = &nv12[..luma];
                if row_y == w as usize {
                    dst.slice(..luma).copy_from_slice(src_y);
                } else {
                    for (r, row) in src_y.chunks_exact(w as usize).enumerate() {
                        dst.slice(r * row_y..r * row_y + w as usize).copy_from_slice(row);
                    }
                }
                let src_c = &nv12[luma..luma + (cw * ch * 2) as usize];
                if row_c == (cw * 2) as usize {
                    dst.slice(chroma_at..chroma_at + src_c.len()).copy_from_slice(src_c);
                } else {
                    for (r, row) in src_c.chunks_exact((cw * 2) as usize).enumerate() {
                        dst.slice(chroma_at + r * row_c..chroma_at + r * row_c + (cw * 2) as usize).copy_from_slice(row);
                    }
                }
                true
            }
            Err(_) => false,
        };
        if written {
            buffer.unmap();
        }
        self.staging.lock().unwrap().state[k] = if written { State::Filled(slot) } else { State::Free };
        written
    }

    /// The UI thread: have the GPU copy every written buffer into its
    /// slot (in order before the UI's next frame), and map them again.
    /// The slots now holding their frames.
    pub fn flush(&self) -> Vec<usize> {
        let mut staging = self.staging.lock().unwrap();
        let filled: Vec<(usize, usize)> = (0..STAGING).filter_map(|k| match staging.state[k] { State::Filled(slot) => Some((k, slot)), _ => None }).collect();
        if filled.is_empty() {
            return Vec::new();
        }
        let Some(rs) = RENDER.get() else { return Vec::new() };
        let (w, h) = (self.width, self.height);
        let (row_y, row_c) = (padded_row(w) as usize, padded_row(w.div_ceil(2) * 2) as usize);
        let chroma_at = (row_y * h as usize) as u64;
        let mut encoder = rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame upload") });
        let layout = |offset: u64, row: usize| wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(row as u32), rows_per_image: None };
        for &(k, slot) in &filled {
            let buffer = &staging.buffers[k];
            let (y, cbcr) = &self.sets[slot];
            encoder.copy_buffer_to_texture(wgpu::TexelCopyBufferInfo { buffer, layout: layout(0, row_y) }, y.texture().as_image_copy(), y.texture().size());
            encoder.copy_buffer_to_texture(wgpu::TexelCopyBufferInfo { buffer, layout: layout(chroma_at, row_c) }, cbcr.texture().as_image_copy(), cbcr.texture().size());
        }
        rs.queue.submit([encoder.finish()]);
        for &(k, _) in &filled {
            // Mapped again once that copy is done (the UI's submits see to
            // the callback).
            staging.state[k] = State::Free;
            staging.mapped[k].store(false, std::sync::atomic::Ordering::Release);
            let flag = staging.mapped[k].clone();
            staging.buffers[k].slice(..).map_async(wgpu::MapMode::Write, move |r| {
                if r.is_ok() {
                    flag.store(true, std::sync::atomic::Ordering::Release);
                }
            });
        }
        filled.into_iter().map(|(_, slot)| slot).collect()
    }

    /// A buffer is mapped and free for the uploader to write.
    pub fn can_fill(&self) -> bool {
        let staging = self.staging.lock().unwrap();
        (0..STAGING).any(|k| staging.state[k] == State::Free && staging.mapped[k].load(std::sync::atomic::Ordering::Acquire))
    }

    /// Nothing has been mapped again for a while (the UI isn't drawing):
    /// see to the GPU's finished work, so the callbacks run.
    pub fn poll(&self) {
        if let Some(rs) = RENDER.get() {
            let _ = rs.device.poll(wgpu::PollType::Poll);
        }
    }

    /// `slot`, ready to draw.
    pub fn shown(&self, slot: usize, bt601: bool) -> Shown {
        let (y, cbcr) = self.sets[slot].clone();
        Shown { key: (self.pool, slot), planes: Planes::Nv12 { y, cbcr, bt601 } }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Ring {
    fn drop(&mut self) {
        super::forget_pools(&[self.pool]);
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
