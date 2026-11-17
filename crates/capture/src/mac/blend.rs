//! Live frame blending on macOS: each frame is the average of a fixed number
//! of looks at the screen, evenly spaced over the frame's time (the blend
//! rate over the frame rate: 120 fps blending at 40 fps is 3), each weighed
//! the same, so every frame shows the same number of steps of motion. Or,
//! blended softly, of the looks from the middle of the frame before to the
//! middle of the one after, the nearer the frame's middle the more (see
//! [`crate::BlendShape`]): two frames are made at once.
//!
//! ScreenCaptureKit only sees what the display shows, so the blend rate is
//! at most the display's refresh rate (`super::display_refresh`): a game
//! drawing 300 pictures a second on a 120 Hz display gets 120 of them seen.
//!
//! A look takes the picture that was up at its time. When the screen changes
//! as fast as the blend rate, each look should get a different picture, but
//! with looks right next to the pictures' times, chance would decide which
//! picture a look gets, one twice and the next not at all. So the looks are
//! kept halfway between the pictures, locked to their pace as on Windows and
//! Linux (`crate::shutter::follow_picture`). When the screen changes faster,
//! the looks pick evenly spaced ones of its pictures; slower, some picture
//! is seen twice (as it was up that long).
//!
//! The pictures are averaged as light, not as pixel values (which are
//! gamma-encoded): a screen showing them in turn gives off the average of
//! their light. Averaging the values would give a bright thing moving over a
//! dark background ghosts far too faint, and a dark thing over a bright one
//! ghosts too strong. The same as on Windows (`crate::win`).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use super::export::Scaler;
use super::gpu::{FramePool, kernels, plane_texture};
use super::video::{Buffer, Frames, Latest};
use crate::mixer::Clock;
use crate::sck::{Encoder, host_now};
use crate::shutter::Shutter;

/// Adds looks at a picture to the frame's average, on the GPU, and turns the
/// average back into an NV12 frame to encode.
///
/// Each frame being made has an average of its own ([`Slot`]), taken as the
/// frame's first look comes: one at a time blended evenly, two softly, and
/// one more while another source's looks are behind this one's (live, each
/// source the frames are made of is blended by a blender of its own, its
/// looks kept halfway between its own pictures, a fraction of a look from
/// another's: one can start a frame before another has finished the one
/// before). Live, a frame all of whose looks saw one picture is that
/// picture as it is, no work for the graphics card: a still source costs
/// nothing.
pub(crate) struct Blender {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    add_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    luma_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    chroma_pass: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    /// The size its averages and frames are (its pictures').
    size: (usize, usize),
    /// The averages of the frames being made, and those let go of (kept to
    /// make the next ones in).
    slots: Vec<Slot>,
    /// Finished frames (the encoder holds each a while).
    pool: FramePool,
    /// The picture that's up, and a number for each picture.
    held: Option<(Arc<Buffer>, u64)>,
    pictures: u64,
    /// When it waits for the GPU.
    waits: Waits,
    /// Its GPU work given and not done yet, oldest first (live).
    behind: std::collections::VecDeque<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// What the GPU was last given, not waiting for it.
    last: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// The GPU work making the frame last finished (none: it's a picture as
    /// it is), and a number for that frame that's another whenever it's
    /// another picture (see [`Mix::made`]).
    made_work: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    made: u64,
}

/// One frame's average being made ([`Blender`]).
struct Slot {
    /// The frame (none: free for the next).
    frame: Option<i64>,
    /// The average, as light (16-bit float, so faint ghosts and dark colours
    /// keep their precision).
    light: Retained<ProtocolObject<dyn MTLTexture>>,
    /// The looks' weight in it so far, and the weight of looks at the
    /// picture that's up not added yet: added in one pass when another
    /// comes up or the frame is done (a screen slower than the blend rate is
    /// seen several times in a row).
    weight: f32,
    pending: f32,
    /// The picture its first look saw: all of them did, while nothing's
    /// been mixed into it (`weight` is 0).
    first: Option<(Arc<Buffer>, u64)>,
}

/// The most frames a blender makes at once (see [`Blender`]).
const SLOTS_MOST: usize = 4;

/// When a blender waits for the graphics card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waits {
    /// Never (exports): each picture is kept until the GPU is done with it,
    /// and a frame finished is made once [`Blender::last_work`] is done.
    Never,
    /// As exports do, but once this much of its work is waiting for the GPU,
    /// for the oldest: each piece holds a picture of the stream's, which has
    /// 8 for everything that holds them (live).
    Behind(usize),
}

/// The most of a live blender's work that may wait for the GPU before it
/// waits for some of it. Waiting at each step instead (a picture added, a
/// frame made), the pacer was held up as long as the GPU was busy with a
/// game: kept busy in 30 ms pieces, up to 23 ms a picture and 39 ms a frame,
/// so pictures were late for their looks (33 in 5 s at 60 fps, 2 looks) and
/// 40% of frames got both their pictures.
const MOST_BEHIND: usize = 4;

/// Whether the GPU is done with `work` (or gave up on it).
pub(crate) fn gpu_done(work: &ProtocolObject<dyn MTLCommandBuffer>) -> bool {
    matches!(work.status(), MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error)
}

// Metal devices, queues, pipelines and textures are thread-safe; the buffers
// are CoreVideo's (see `Buffer`).
unsafe impl Send for Blender {}

impl Blender {
    /// Live: `open` frames made at once (more as they're needed). A frame
    /// finished is made once [`Self::last_work`] is done; the GPU is waited
    /// for only when it's [`MOST_BEHIND`] behind.
    pub(crate) fn new(width: usize, height: usize, open: usize) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        Self::on(device, queue, width, height, open, Waits::Behind(MOST_BEHIND))
    }

    /// For exports: on `queue`, where the pictures it's given are made (so
    /// they're made before it reads them), never waiting for the GPU: each
    /// picture is kept until the GPU is done with it, and a frame finished
    /// is made once [`Self::last_work`] is done.
    pub(crate) fn queued(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        width: usize,
        height: usize,
        open: usize,
    ) -> Result<Self> {
        Self::on(device, queue, width, height, open, Waits::Never)
    }

    fn on(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        width: usize,
        height: usize,
        open: usize,
        waits: Waits,
    ) -> Result<Self> {
        let mut k = kernels(&device, KERNELS, &["add_look", "light_to_luma", "light_to_chroma"])?.into_iter();
        let mut b = Self {
            add_pass: k.next().unwrap(),
            luma_pass: k.next().unwrap(),
            chroma_pass: k.next().unwrap(),
            size: (width, height),
            slots: Vec::new(),
            pool: FramePool::new(width, height)?,
            held: None,
            pictures: 0,
            waits,
            behind: Default::default(),
            last: None,
            made_work: None,
            made: 0,
            device,
            queue,
        };
        // As many as are made at once, at the start: the rest as they're needed.
        for _ in 0..open.clamp(1, 2) {
            let light = b.light()?;
            b.slots.push(Slot { frame: None, light, weight: 0.0, pending: 0.0, first: None });
        }
        Ok(b)
    }

    /// Another blender like this one (on its queue, with its kernels, so
    /// it's made at once), for pictures `width`×`height`: another source's.
    pub(crate) fn sibling(&self, width: usize, height: usize) -> Result<Self> {
        Ok(Self {
            device: self.device.clone(),
            queue: self.queue.clone(),
            add_pass: self.add_pass.clone(),
            luma_pass: self.luma_pass.clone(),
            chroma_pass: self.chroma_pass.clone(),
            size: (width, height),
            slots: Vec::new(),
            pool: FramePool::new(width, height)?,
            held: None,
            pictures: 0,
            waits: self.waits,
            behind: Default::default(),
            last: None,
            made_work: None,
            made: 0,
        })
    }

    /// The size its pictures are.
    pub(crate) fn size(&self) -> (usize, usize) {
        self.size
    }

    /// An average's texture, its size.
    fn light(&self) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
        unsafe {
            let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA16Float, self.size.0, self.size.1, false);
            desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            desc.setStorageMode(MTLStorageMode::Private);
            self.device.newTextureWithDescriptor(&desc).context("no GPU memory for frame blending")
        }
    }

    /// The GPU work given last (not waiting for it): once it's done, so is
    /// the frame last finished.
    pub(crate) fn last_work(&self) -> Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
        self.last.clone()
    }

    /// `picture` is the one that's up now.
    pub(crate) fn hold(&mut self, picture: Arc<Buffer>) {
        self.flush(None);
        self.pictures += 1;
        self.held = Some((picture, self.pictures));
    }

    /// Frame `k`'s average, made (or taken from those let go of) if it
    /// isn't being made. None if it can't be (no room on the graphics card,
    /// or too many frames at once: its looks are let go of).
    fn slot_for(&mut self, k: i64) -> Option<&mut Slot> {
        if let Some(i) = self.slots.iter().position(|s| s.frame == Some(k)) {
            return self.slots.get_mut(i);
        }
        let i = match self.slots.iter().position(|s| s.frame.is_none()) {
            Some(i) => i,
            None if self.slots.len() < SLOTS_MOST => match self.light() {
                Ok(light) => {
                    self.slots.push(Slot { frame: None, light, weight: 0.0, pending: 0.0, first: None });
                    self.slots.len() - 1
                }
                Err(e) => {
                    crate::eprintln_later!("frame blending: {e:#}");
                    return None;
                }
            },
            None => return None,
        };
        let s = &mut self.slots[i];
        (s.frame, s.weight, s.pending, s.first) = (Some(k), 0.0, 0.0, None);
        Some(s)
    }

    /// Add a look at the picture that's up to frame `k`'s average, by `weight`.
    pub(crate) fn add(&mut self, k: i64, weight: f32) {
        let Some(held) = self.held.clone() else { return };
        if let Some(s) = self.slot_for(k) {
            if s.weight + s.pending == 0.0 {
                s.first = Some(held);
            }
            s.pending += weight;
        }
    }

    /// Mix the looks at the picture that's up into the averages (`only`
    /// that one of `slots`), at once: the picture is kept until the GPU is
    /// done with it. A frame made mixes in only its own looks: the next
    /// frame's (a source ahead of another starts it before this one is made)
    /// wait for the next picture, so a next frame all of whose looks see this
    /// one is still this picture as it is.
    fn flush(&mut self, only: Option<usize>) {
        let Some((held, _)) = self.held.clone() else { return };
        // Mixing in by its share of the total so far keeps it the average.
        let mut mixes = Vec::new();
        for (i, s) in self.slots.iter_mut().enumerate().filter(|(i, s)| only.is_none_or(|o| o == *i) && s.frame.is_some() && s.pending > 0.0) {
            let weight = std::mem::take(&mut s.pending);
            s.weight += weight;
            mixes.push((i, weight / s.weight));
        }
        if mixes.is_empty() {
            return;
        }
        if let Err(e) = self.run(Some(held.clone()), |enc, this| {
            let luma = plane_texture(&this.device, &held.0, 0, false)?;
            let chroma = plane_texture(&this.device, &held.0, 1, false)?;
            enc.setComputePipelineState(&this.add_pass);
            for (i, share) in &mixes {
                unsafe {
                    enc.setTexture_atIndex(Some(&luma), 0);
                    enc.setTexture_atIndex(Some(&chroma), 1);
                    enc.setTexture_atIndex(Some(&this.slots[*i].light), 2);
                    enc.setBytes_length_atIndex(NonNull::from(share).cast::<c_void>(), 4, 0);
                }
                dispatch(enc, luma.width(), luma.height());
            }
            Ok(())
        }) {
            crate::eprintln_later!("frame blending: {e:#}");
        }
    }

    /// Frame `k` is done: its average as a frame to encode, and its average
    /// let go of. None if nothing was added (no picture yet). Live, a frame
    /// all of whose looks saw the picture that's up (nothing's mixed in yet)
    /// is that picture, as it is.
    pub(crate) fn finish(&mut self, k: i64) -> Option<Arc<Buffer>> {
        let i = self.slots.iter().position(|s| s.frame == Some(k))?;
        let s = &self.slots[i];
        if matches!(self.waits, Waits::Behind(_))
            && s.weight == 0.0
            && s.pending > 0.0
            && let Some((picture, n)) = s.first.clone()
        {
            self.slots[i].frame = None;
            (self.made_work, self.made) = (None, n);
            return Some(picture);
        }
        self.flush(Some(i));
        let s = &mut self.slots[i];
        s.frame = None;
        if s.weight == 0.0 {
            return None;
        }
        let light = s.light.clone();
        let made = self.pool.take().and_then(|out| {
            self.run(None, |enc, this| {
                for (plane, pass) in [(0, &this.luma_pass), (1, &this.chroma_pass)] {
                    let dst = plane_texture(&this.device, &out, plane, true)?;
                    enc.setComputePipelineState(pass);
                    unsafe {
                        enc.setTexture_atIndex(Some(&light), 0);
                        enc.setTexture_atIndex(Some(&dst), 1);
                    }
                    dispatch(enc, dst.width(), dst.height());
                }
                Ok(())
            })?;
            Ok(out)
        });
        match made {
            Ok(out) => {
                // A number no picture has: blended frames are each another.
                (self.made_work, self.made) = (self.last.clone(), u64::MAX / 2 + k as u64);
                Some(Arc::new(Buffer(out)))
            }
            Err(e) => {
                crate::eprintln_later!("frame blending: {e:#}");
                None
            }
        }
    }

    /// Encode one compute pass with `f` and give it to the GPU, keeping
    /// `reads` until the GPU is done with it; live, once [`MOST_BEHIND`] of
    /// its work is waiting for the GPU, wait for the oldest.
    fn run(&mut self, reads: Option<Arc<Buffer>>, f: impl FnOnce(&ProtocolObject<dyn MTLComputeCommandEncoder>, &Self) -> Result<()>) -> Result<()> {
        let cmd = self.queue.commandBuffer().context("no command buffer")?;
        let enc = cmd.computeCommandEncoder().context("no compute encoder")?;
        let result = f(&enc, self);
        enc.endEncoding();
        if let Some(reads) = reads {
            // Kept until the GPU, done, lets go of the block.
            let keep = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                let _ = &reads;
            });
            unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&keep)) };
        }
        cmd.commit();
        if let Waits::Behind(most) = self.waits {
            // One queue: the GPU does its work in order.
            while self.behind.front().is_some_and(|w| gpu_done(w)) {
                self.behind.pop_front();
            }
            self.behind.push_back(cmd.clone());
            while self.behind.len() > most {
                if let Some(oldest) = self.behind.pop_front() {
                    oldest.waitUntilCompleted();
                }
            }
        }
        self.last = Some(cmd);
        result
    }
}

/// What a frame is made of: its looks blended on the graphics card, or (one
/// look a frame) the picture its look took, as it is.
pub(crate) enum Mix {
    Blend(Blender),
    /// Not blended: each frame is the picture that was up at its time,
    /// taken by its one look, with no graphics card work.
    One {
        /// The picture that's up, and a number for each.
        held: Option<(Arc<Buffer>, u64)>,
        pictures: u64,
        /// The frames whose look has been taken, and the picture each took
        /// (a source ahead of another can take the next frame's before this
        /// one is made, see [`Blender`]).
        took: Vec<(i64, Arc<Buffer>, u64)>,
        /// The number of the picture the frame last finished is.
        made: u64,
    },
}

impl Mix {
    /// One look a frame.
    pub(crate) fn one() -> Self {
        Self::One { held: None, pictures: 0, took: Vec::new(), made: 0 }
    }

    /// `picture` is the one that's up now.
    fn hold(&mut self, picture: Arc<Buffer>) {
        match self {
            Self::Blend(b) => b.hold(picture),
            Self::One { held, pictures, .. } => {
                *pictures += 1;
                *held = Some((picture, *pictures));
            }
        }
    }

    /// A look at the picture that's up, for frame `k`, by `weight`.
    fn add(&mut self, k: i64, weight: f32) {
        match self {
            Self::Blend(b) => b.add(k, weight),
            Self::One { held, took, .. } => {
                if let Some((picture, n)) = held {
                    took.retain(|(frame, ..)| *frame != k);
                    took.push((k, picture.clone(), *n));
                    // Frames never finished (dropped) don't pile up.
                    if took.len() > SLOTS_MOST {
                        took.remove(0);
                    }
                }
            }
        }
    }

    /// Frame `k` is done: what it's made of. None if nothing was added to
    /// it (no picture yet).
    fn finish(&mut self, k: i64) -> Option<Arc<Buffer>> {
        match self {
            Self::Blend(b) => b.finish(k),
            Self::One { took, made, .. } => {
                let i = took.iter().position(|(frame, ..)| *frame == k)?;
                let (_, picture, n) = took.remove(i);
                *made = n;
                Some(picture)
            }
        }
    }

    /// The GPU work that makes the frame last finished, if it has some: the
    /// frame is made once that's done.
    fn work(&self) -> Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
        match self {
            Self::Blend(b) => b.made_work.clone(),
            Self::One { .. } => None,
        }
    }

    /// A number for the frame last finished that's another whenever it's
    /// another picture (a blend's never the same), so a frame made of the
    /// same needn't be made again (see `super::video::Frames::make_with`).
    fn made(&self) -> u64 {
        match self {
            Self::Blend(b) => b.made,
            Self::One { made, .. } => *made,
        }
    }

    /// The size its pictures are, if it blends them.
    fn size(&self) -> Option<(usize, usize)> {
        match self {
            Self::Blend(b) => Some(b.size()),
            Self::One { .. } => None,
        }
    }
}

/// How another source's frames are made, for pictures `size` big, as the
/// recording's own are made by `main`: blended by a blender like its own
/// (made at once, its kernels shared), or one look a frame.
fn mix_like(main: Option<&Mix>, size: (usize, usize)) -> Result<Mix> {
    match main {
        Some(Mix::Blend(b)) => Ok(Mix::Blend(b.sibling(size.0, size.1)?)),
        _ => Ok(Mix::one()),
    }
}

fn dispatch(enc: &ProtocolObject<dyn MTLComputeCommandEncoder>, width: usize, height: usize) {
    enc.dispatchThreads_threadsPerThreadgroup(MTLSize { width, height, depth: 1 }, MTLSize { width: 16, height: 16, depth: 1 });
}

/// NV12 (studio range, BT.709) to light and back. A look is mixed into the
/// average by its weight (1: it starts it). Back from light, chroma is the
/// average of the four pixels it covers.
const KERNELS: &str = r#"
#include <metal_stdlib>
using namespace metal;

float3 to_light(float3 c) { c = saturate(c); return select(pow((c + 0.055) / 1.055, 2.4), c / 12.92, c <= 0.04045); }
float3 from_light(float3 c) { c = saturate(c); return select(1.055 * pow(c, 1.0 / 2.4) - 0.055, c * 12.92, c <= 0.0031308); }

float3 rgb_of(float y, float2 cbcr) {
    float l = (y * 255.0 - 16.0) / 219.0;
    float2 c = (cbcr * 255.0 - 128.0) / 224.0;
    return float3(l + 1.5748 * c.y, l - 0.1873 * c.x - 0.4681 * c.y, l + 1.8556 * c.x);
}

float3 ycc_of(float3 rgb) {
    float l = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    return float3(16.0 + 219.0 * l, 128.0 + 224.0 * (rgb.b - l) / 1.8556, 128.0 + 224.0 * (rgb.r - l) / 1.5748) / 255.0;
}

kernel void add_look(texture2d<float, access::read> luma [[texture(0)]],
                     texture2d<float, access::read> chroma [[texture(1)]],
                     texture2d<float, access::read_write> light [[texture(2)]],
                     constant float& weight [[buffer(0)]],
                     uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= light.get_width() || gid.y >= light.get_height()) return;
    float3 c = to_light(rgb_of(luma.read(gid).x, chroma.read(gid / 2).xy));
    float3 a = weight >= 1.0 ? c : mix(light.read(gid).rgb, c, weight);
    light.write(float4(a, 1.0), gid);
}

kernel void light_to_luma(texture2d<float, access::read> light [[texture(0)]],
                          texture2d<float, access::write> out [[texture(1)]],
                          uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    out.write(float4(ycc_of(from_light(light.read(gid).rgb)).x, 0.0, 0.0, 1.0), gid);
}

kernel void light_to_chroma(texture2d<float, access::read> light [[texture(0)]],
                            texture2d<float, access::write> out [[texture(1)]],
                            uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= out.get_width() || gid.y >= out.get_height()) return;
    uint2 last = uint2(light.get_width(), light.get_height()) - 1;
    float3 sum = 0.0;
    for (uint y = 0; y < 2; y++) {
        for (uint x = 0; x < 2; x++) {
            sum += from_light(light.read(min(gid * 2 + uint2(x, y), last)).rgb);
        }
    }
    out.write(float4(ycc_of(sum / 4.0).yz, 0.0, 1.0), gid);
}
"#;

// ---------------------------------------------------------------------------
// Pacer
// ---------------------------------------------------------------------------

/// How long after a frame's time it's made: a picture reaches us a fraction
/// of a millisecond after the screen showed it (its time), and it must still
/// get into its frame. A guess, with room for a busy system.
const BLEND_LATE: f64 = 0.006;

/// One picture the frames are made of, blended and timed on its own (its
/// [`Mix`]): the recording's own capture, or a Display or Window source's
/// (`crate::LayerSource::frame_blended`), as on Windows (`crate::win`). Its
/// looks take the picture it had up at their time, kept halfway between its
/// own pictures (its `phase`): two sources' pictures needn't come at the
/// same times, so each is locked to its own. Every source's frame `k` is
/// made of its looks `k * looks` on, each source's a fraction of a look
/// apart.
struct Source {
    /// Its layer (`crate::Layer::id`; 0 is the recording's own capture).
    id: u64,
    /// What it is, for the log.
    name: String,
    /// A Display or Window source's capture's pictures (none: the
    /// recording's own capture's, `BlendPacer::latest`).
    pictures: Option<Arc<super::layers::Pictures>>,
    /// How its frames are made; a Display or Window source's made for its
    /// pictures' size (again when that changes: a window resized), once
    /// it has one.
    mix: Option<Mix>,
    /// Whether the frames show it, so its looks are added: a source taken
    /// out of the scene is let go of, but the recording's own capture goes
    /// on, its looks unadded, while the scene doesn't show it.
    shown: bool,
    /// Its look `m` is at `t0 + phase + (m + 0.5) * step`, and its next
    /// look to take.
    phase: f64,
    next: i64,
    /// A picture its stream timed has come up (it set `phase`).
    timed: bool,
    /// The picture that's up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// The picture that's up, for a mix that takes over (blending changed).
    up: Option<Arc<Buffer>>,
    /// The newest picture number taken from its capture.
    seen: u64,
    /// Different pictures in the frame's own time so far.
    different: u32,
    /// Its pictures a second, lately (which source's are told).
    rate: Rate,
    stats: SourceStats,
}

impl Source {
    fn new(id: u64, name: String, pictures: Option<Arc<super::layers::Pictures>>, mix: Option<Mix>, phase: f64, next: i64) -> Self {
        Self { id, name, pictures, mix, shown: true, phase, next, timed: false, held: None, last_taken: None, up: None, seen: 0, different: 0, rate: Rate::default(), stats: SourceStats::new() }
    }
}

/// A source's pictures a second, as of the last whole second.
#[derive(Default)]
struct Rate {
    since: f64,
    count: u32,
    last: u32,
}

impl Rate {
    /// A picture came at `now`.
    fn note(&mut self, now: f64) {
        if now - self.since >= 1.0 {
            self.last = if now - self.since < 2.0 { self.count } else { 0 };
            (self.since, self.count) = (now, 0);
        }
        self.count += 1;
    }

    /// Pictures in the last whole second, as of `now` (or so far, before one's gone by).
    fn per_second(&self, now: f64) -> u32 {
        match now - self.since {
            gone if gone >= 2.0 => 0,
            gone if gone >= 1.0 => self.count,
            _ => self.last.max(self.count),
        }
    }
}

struct BlendPacer {
    t0: f64,
    fps: u32,
    /// Seconds between looks (one over the blend rate), and looks per frame.
    step: f64,
    looks: i64,
    /// Which frames each look counts toward.
    shutter: Shutter,
    /// The last frame done (made or empty): later looks don't count toward
    /// it. A frame's done once every source has taken its looks.
    closed: i64,
    /// The recording's own capture.
    latest: Arc<Latest>,
    /// What the frames are made of: the recording's own capture first, then
    /// each Display and Window source that shows (see [`Source`]).
    sources: Vec<Source>,
    /// The source whose pictures how the capture keeps up tells
    /// (`crate::blend_sources`): the recording's own capture while the
    /// scene shows it, else the one showing the most pictures.
    told: u64,
    /// Rung by every shown source's capture when a picture comes.
    bell: Arc<crate::shutter::Bell>,
    /// A frame was made since the sources last followed the layers.
    follow_due: bool,
    frames: Frames,
    encoder: SendEncoder,
    /// The stream's own pictures, while it streams (one per rendition).
    stream: Vec<StreamOut>,
    skipped: u64,
    stats: BlendStats,
    report: EncodeReport,
    /// Blended frames the GPU is still making, oldest first.
    making: std::collections::VecDeque<Making>,
}

/// What the session changes in a running pacer: frame blending, and the
/// stream's pictures (see `Session::set_blend` and `Session::stream_video`
/// in `crate::sck`). Each is taken up at the pacer's next turn.
#[derive(Clone, Default)]
pub(crate) struct PacerCtl {
    pub(crate) blend: Arc<Mutex<Option<BlendChange>>>,
    /// The stream started (one per rendition, `stream::renditions`), or
    /// stopped (none).
    pub(crate) stream: Arc<Mutex<Option<Vec<StreamOut>>>>,
}

/// Frame blending changed: `looks` a frame from the next frame on, where
/// `shutter` says, made by `mix` (made beforehand: a blender's kernels take
/// a moment to build, which the pacer mustn't wait for).
pub(crate) struct BlendChange {
    pub(crate) looks: u32,
    pub(crate) shutter: Shutter,
    pub(crate) mix: Mix,
}

/// One of the stream's pictures (a rendition, `stream::renditions`): a
/// frame for every `every` the pacer makes, made the rendition's size by
/// Metal (`super::export::Scaler`, as an export's) and encoded by a
/// VideoToolbox encoder of its own, all on a thread of its own so the pacer
/// never waits for any of it. A frame that comes while the thread is still
/// busy with those before is left out.
pub(crate) struct StreamOut {
    every: i64,
    /// The pacer's frames, their numbers, and whether it blends.
    tx: Option<SyncSender<(Arc<Buffer>, i64, bool)>>,
    thread: Option<JoinHandle<()>>,
    /// Frames left out, its thread busy.
    dropped: u64,
}

/// A buffer to hand to the GPU, from one that's shared.
fn retain(buf: &objc2_core_video::CVPixelBuffer) -> CFRetained<objc2_core_video::CVPixelBuffer> {
    unsafe { CFRetained::retain(NonNull::from(buf)) }
}

impl StreamOut {
    /// A frame for every `every` of the pacer's, `from` big, made `to` big
    /// and encoded by `encoder` (at the rendition's rate). With frame
    /// blending on, and `every` over one (48 fps from 144), each frame is
    /// the average of all the pacer's in its time, as light, as a frame is
    /// of its looks, so its motion is blurred over all of its time as the
    /// recording's is over each frame's (as Windows' `StreamBlend`); taking
    /// one in three instead blurred a third of it. The average's made on
    /// the scaler's queue, so it's made before it's scaled.
    pub(crate) fn start(every: i64, from: (usize, usize), to: (usize, usize), fps: u32, encoder: Encoder) -> Result<Self> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("no Metal command queue")?;
        let size = |(w, h): (usize, usize)| (w as u32, h as u32);
        let scaler = Scaler::new(&device, &queue, size(from), [0, 0, from.0 as u32, from.1 as u32], size(to), false)?;
        let blender = if every > 1 { Some(Blender::queued(device.clone(), queue.clone(), from.0, from.1, 1)?) } else { None };
        // Room for a stream frame's worth of the pacer's frames, and more.
        let (tx, rx) = mpsc::sync_channel::<(Arc<Buffer>, i64, bool)>((2 * every as usize).clamp(2, 16));
        let encoder = SendEncoder(encoder);
        let thread = thread::Builder::new().name("stream video".into()).spawn(move || {
            // All of it moves here (not just the encoder inside).
            let encoder = encoder;
            let mut blender = blender;
            // The stream frame whose average is being made.
            let mut summing: Option<i64> = None;
            let mut skipped = 0u64;
            for (frame, k, blending) in rx {
                let n = k.div_euclid(every);
                let made = match blender.as_mut().filter(|_| blending) {
                    Some(b) => {
                        // A frame of a later one: the one being made never
                        // got its last (left out), and is let go of.
                        if summing.is_some_and(|s| s != n) {
                            let _ = b.finish(0);
                        }
                        summing = Some(n);
                        b.hold(frame);
                        b.add(0, 1.0);
                        if k.rem_euclid(every) != every - 1 {
                            continue;
                        }
                        summing = None;
                        match b.finish(0) {
                            Some(average) => average,
                            None => continue,
                        }
                    }
                    None => {
                        // Blending stopped partway through one: let go of.
                        if summing.take().is_some()
                            && let Some(b) = blender.as_mut()
                        {
                            let _ = b.finish(0);
                        }
                        if k.rem_euclid(every) != 0 {
                            continue;
                        }
                        frame
                    }
                };
                // Made the rendition's size in a buffer of its own (the
                // screen's own go back to it at once), then encoded.
                let scaled = scaler.scale(retain(&made.0));
                drop(made);
                match scaled.and_then(|m| Ok(encoder.0.encode(m.ready()?, n, fps))) {
                    Ok(true) => {}
                    Ok(false) => skipped += 1,
                    Err(e) => crate::eprintln_later!("stream: {e:#}"),
                }
            }
            if skipped > 0 {
                eprintln!("stream: its encoder couldn't take {skipped} frames");
            }
            encoder.0.finish();
        })?;
        Ok(Self { every, tx: Some(tx), thread: Some(thread), dropped: 0 })
    }

    /// The pacer's frame `k`, `frame` (`blending`: frame blending's on):
    /// handed over if it's wanted, every one while it averages its own,
    /// else those at its own frames' times.
    fn offer(&mut self, k: i64, frame: &Arc<Buffer>, blending: bool) {
        if !(blending && self.every > 1) && k.rem_euclid(self.every) != 0 {
            return;
        }
        if let Some(tx) = &self.tx
            && tx.try_send((frame.clone(), k, blending)).is_err()
        {
            self.dropped += 1;
        }
    }

    /// The frames handed in encoded and sent on (waited for).
    pub(crate) fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.dropped > 0 {
            eprintln!("stream: {} frames left out, it was busy", self.dropped);
        }
    }
}

/// A frame waiting to be encoded: a blended one the GPU is still making
/// (`work`), or one made already that waits behind such frames, as frames
/// are encoded in order.
struct Making {
    k: i64,
    /// The recording's own capture's frame, and each Display and Window
    /// source's blend (its layer's id, the picture, its number: see
    /// `Frames::make_with`).
    made: Arc<Buffer>,
    layers: Vec<(u64, Arc<Buffer>, u64)>,
    /// The GPU work making them.
    work: Vec<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// How late it was closed, how long closing it took, and when.
    late: f64,
    mixed: Duration,
    closed: Instant,
}

struct SendEncoder(Encoder);
unsafe impl Send for SendEncoder {}

impl BlendPacer {
    /// When source `i`'s look `m` is.
    fn look_time(&self, i: usize, m: i64) -> f64 {
        self.t0 + self.sources[i].phase + (m as f64 + 0.5) * self.step
    }

    /// When frame `k` is done: every source's last look for it.
    fn frame_done_at(&self, k: i64) -> f64 {
        let last = self.shutter.last_look(k);
        (0..self.sources.len()).map(|i| self.look_time(i, last)).fold(f64::MIN, f64::max)
    }

    /// When the next frame to make is done.
    fn frame_end(&self) -> f64 {
        self.frame_done_at(self.closed + 1)
    }

    /// Take every look before `t`, each source's: each sees the picture it
    /// has up. Then the frames every source has taken its looks for are made.
    fn look_until(&mut self, t: f64) {
        for i in 0..self.sources.len() {
            self.take_looks(i, t);
        }
        self.close_done();
        self.stats.log(&mut self.sources, self.looks as u32);
    }

    /// Take source `i`'s looks before `t`.
    fn take_looks(&mut self, i: usize, t: f64) {
        let mut burst = 0;
        while self.look_time(i, self.sources[i].next) < t && burst < 10_000 {
            self.look(i);
            burst += 1;
        }
    }

    /// Source `i`'s next look: the picture it has up, added to the frames
    /// the look counts toward, if the frames show it.
    fn look(&mut self, i: usize) {
        let (looks, closed) = (self.looks, self.closed);
        let s = &mut self.sources[i];
        let (first, weights) = self.shutter.split(s.next);
        if s.shown
            && let (Some(id), Some(mix)) = (s.held, s.mix.as_mut())
        {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > closed && weight > 0.0 {
                    mix.add(k, weight);
                }
            }
            if s.last_taken != Some(id) {
                s.different += 1;
                s.last_taken = Some(id);
            }
        }
        s.next += 1;
        if s.next % looks == 0 {
            let different = std::mem::take(&mut s.different);
            if s.shown && s.held.is_some() {
                s.stats.frame(different, looks as u32);
            }
        }
    }

    /// The frames no source's later looks count toward are done: made.
    fn close_done(&mut self) {
        let Some(next) = self.sources.iter().map(|s| s.next).min() else { return };
        let (first, _) = self.shutter.split(next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: encoded (with the other layers over it or under
    /// it) once the GPU has made it, frames in order. The pacer doesn't wait
    /// for the GPU (see `MOST_BEHIND`).
    fn close(&mut self, k: i64) {
        self.closed = k;
        self.follow_due = true;
        let started = Instant::now();
        // After its last look's time and the room a picture has to come in.
        let late = host_now() - self.frame_done_at(k) - BLEND_LATE;
        // The recording's own capture's frame: its blend, while the scene
        // shows it, else its newest picture (not drawn, its frame isn't).
        let main = &mut self.sources[0];
        let mut work = Vec::new();
        let made = if main.shown {
            let Some(mix) = main.mix.as_mut() else { return };
            let Some(made) = mix.finish(k) else { return };
            work.extend(mix.work());
            made
        } else {
            let Some(up) = main.up.clone() else { return };
            up
        };
        // Each Display and Window source's blend.
        let mut layers = Vec::new();
        for s in self.sources[1..].iter_mut().filter(|s| s.shown) {
            let Some(mix) = s.mix.as_mut() else { continue };
            if let Some(blend) = mix.finish(k) {
                layers.push((s.id, blend, mix.made()));
                work.extend(mix.work());
            }
        }
        let mixed = started.elapsed();
        if work.is_empty() && self.making.is_empty() {
            self.encode(k, made, &layers, late, mixed, None);
        } else {
            // After the frames before it: blending turned off, the last
            // blended ones can still be on the GPU, and a frame encoded
            // ahead of them broke the clip or recording it was in
            // (AVAssetWriter's -16364 at its end).
            self.making.push_back(Making { k, made, layers, work, late, mixed, closed: started });
        }
        self.encode_made(false);
    }

    /// Encode the frames that are made, in order; `all`, those the GPU
    /// hasn't made yet too, once it has (stopping).
    fn encode_made(&mut self, all: bool) {
        while let Some(m) = self.making.front() {
            if let Some(work) = m.work.iter().find(|w| !gpu_done(w)) {
                if !all {
                    break;
                }
                work.waitUntilCompleted();
                continue;
            }
            let Some(m) = self.making.pop_front() else { break };
            let gpu = (!m.work.is_empty()).then(|| m.closed.elapsed());
            self.encode(m.k, m.made, &m.layers, m.late, m.mixed, gpu);
        }
    }

    /// Encode frame `k`, `made`, with `layers` blended (closed `late`, in
    /// `mixed`; made by the GPU `gpu` after it was closed, when it's
    /// blended).
    fn encode(&mut self, k: i64, made: Arc<Buffer>, layers: &[(u64, Arc<Buffer>, u64)], late: f64, mixed: Duration, gpu: Option<Duration>) {
        let started = Instant::now();
        let frame = self.frames.make_with(made, layers);
        // The stream's pictures', where they want it (whether or not the
        // recording's encoder takes it).
        let blending = self.looks > 1;
        for s in &mut self.stream {
            s.offer(k, &frame, blending);
        }
        let composed = started.elapsed();
        let free = self.encoder.0.free();
        let encoded = self.encoder.0.encode(&frame.0, k, self.fps);
        let steps = Steps { mix: mixed, make: composed, encode: started.elapsed() - composed };
        if encoded {
            crate::count_encoded();
        } else {
            crate::count_skipped(1);
            self.skipped += 1;
            if self.skipped.is_power_of_two() {
                crate::eprintln_later!("video encoder is behind: {} frames skipped so far", self.skipped);
            }
        }
        self.report.frame(late, steps, gpu, free, !encoded, &self.encoder.0);
    }

    /// Every source's pictures that came up since, in the order they did,
    /// each taken in as it came (see `arrive`). A source's own pictures go
    /// in the order its stream had them, whatever their times say.
    fn take_pictures(&mut self) {
        let now = host_now();
        let mut came: Vec<(f64, usize, Arc<Buffer>, u64, bool)> = Vec::new();
        for (i, s) in self.sources.iter_mut().enumerate() {
            let arrivals = match &s.pictures {
                Some(p) => p.drain(),
                None => self.latest.drain(),
            };
            let mut last = f64::MIN;
            for a in arrivals {
                if a.seq > s.seen {
                    s.seen = a.seq;
                    last = last.max(a.at);
                    came.push((last, i, a.buffer, a.seq, true));
                }
            }
            // Put up some other way (the away screen; or while the scene
            // doesn't show it, every picture): taken from the capture.
            if s.pictures.is_none() {
                let seq = self.latest.seq();
                if seq > s.seen {
                    s.seen = seq;
                    if let Some(b) = self.latest.get() {
                        came.push((now.max(last), i, b, seq, false));
                    }
                }
            }
        }
        came.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (at, i, picture, id, timed) in came {
            self.arrive(i, at, picture, id, timed);
        }
    }

    /// A new picture of source `i` came up at `at`: its looks before it saw
    /// the one before. Then its looks move toward halfway between its
    /// pictures. `timed`: `at` is when the stream says the screen showed it
    /// (else when it was noticed, as the away screen is).
    fn arrive(&mut self, i: usize, at: f64, picture: Arc<Buffer>, id: u64, timed: bool) {
        let (source, shown) = (self.sources[i].id, self.sources[i].shown);
        // Only a source that shows is counted.
        if shown {
            crate::count_picture_of(source);
            if at < self.look_time(i, self.sources[i].next - 1) {
                self.sources[i].stats.late += 1;
                crate::count_missed_of(source, 1);
            }
        }
        self.take_looks(i, at);
        self.close_done();
        // Pictures belong on whole steps of its looks (halfway between
        // two); the first its stream timed puts them there.
        let (step, since) = (self.step, at - self.t0);
        let s = &mut self.sources[i];
        let first = timed && !s.timed;
        s.timed |= timed;
        s.phase = crate::shutter::follow_picture(s.phase, step, since, first);
        s.stats.pictures += 1;
        s.rate.note(host_now());
        let adding = Instant::now();
        self.hold(i, picture, id);
        self.report.added(adding.elapsed());
    }

    /// Source `i`'s picture that's up is `picture` (number `id`). A Display
    /// or Window source's frames are made for its pictures' size: anew
    /// when that changes (a window resized), its frames under way let go
    /// of.
    fn hold(&mut self, i: usize, picture: Arc<Buffer>, id: u64) {
        if i > 0 {
            let size = super::video::size_of(&picture.0);
            let fits = match &self.sources[i].mix {
                Some(m) => m.size().is_none_or(|s| s == size) && m.size().is_some() == (self.looks > 1),
                None => false,
            };
            if !fits {
                match mix_like(self.sources[0].mix.as_ref(), size) {
                    Ok(m) => self.sources[i].mix = Some(m),
                    Err(e) => {
                        crate::eprintln_later!("frame blending of {}: {e:#}", self.sources[i].name);
                        self.sources[i].mix = None;
                    }
                }
            }
        }
        let s = &mut self.sources[i];
        s.up = Some(picture.clone());
        if let Some(m) = s.mix.as_mut() {
            m.hold(picture);
        }
        s.held = Some(id);
    }

    /// The Display and Window sources that show now, each blended on its
    /// own: one that's come into the frames (shown, put in the scene, or
    /// another display or window now) gets its pictures queued and is
    /// blended from its newest; one that's gone is let go of. Then whether
    /// the scene shows the recording's own capture, and whose pictures are
    /// told. Once a frame, between pictures (the sources keep their places
    /// in the list while one's taken in).
    fn follow_layers(&mut self) {
        let want = self.frames.feeds().frame_blended();
        let mut i = 1;
        while i < self.sources.len() {
            let s = &self.sources[i];
            if want.iter().any(|(id, p, _)| *id == s.id && s.pictures.as_ref().is_some_and(|q| Arc::ptr_eq(p, q))) {
                i += 1;
                continue;
            }
            let s = self.sources.remove(i);
            crate::eprintln_later!("frame blending: {} isn't shown, no longer blended", s.name);
            if let Some(p) = &s.pictures {
                p.blend_as(None);
            }
        }
        for (id, pictures, name) in want {
            if self.sources.iter().any(|s| s.id == id) {
                continue;
            }
            pictures.blend_as(Some((id, self.bell.clone())));
            // Its looks from the frame under way, where the recording's own
            // capture's are, till its first picture puts them halfway
            // between its own.
            let main = &self.sources[0];
            let mut s = Source::new(id, name, Some(pictures.clone()), None, main.phase, main.next);
            crate::eprintln_later!("frame blending: {} shown, blended on its own", s.name);
            let newest = pictures.get();
            if let Some((_, n)) = &newest {
                s.seen = *n;
            }
            self.sources.push(s);
            if let Some((picture, n)) = newest {
                let i = self.sources.len() - 1;
                self.hold(i, picture, n);
            }
        }
        // The recording's own capture, while the scene shows it: else its
        // looks would be blended for nothing, and its pictures would wake
        // the pacer for nothing (on Windows, measured, that made another
        // display's pictures come in less evenly). It goes on meanwhile, so
        // it shows again at once, from its newest picture.
        let main_shown = self.frames.main_shown();
        let main = &mut self.sources[0];
        if main.shown != main_shown {
            main.shown = main_shown;
            main.different = 0;
            if main_shown {
                self.latest.queue_arrivals();
                self.latest.set_bell(Some(self.bell.clone()));
            } else {
                self.latest.stop_queueing();
                self.latest.set_bell(None);
            }
        }
        // Whose pictures are told: the recording's own capture's while it
        // shows, else those of the source showing the most (the first of
        // those showing as many).
        let now = host_now();
        let busiest = self.sources[1..].iter().fold(None::<&Source>, |b, s| if b.is_some_and(|b| b.rate.per_second(now) >= s.rate.per_second(now)) { b } else { Some(s) });
        self.told = if main_shown { 0 } else { busiest.map_or(0, |s| s.id) };
        crate::blend_sources(self.sources.iter().filter(|s| s.shown).count(), self.told);
    }

    /// Blend as `c` says from the next frame on: the frame under way is made
    /// from the looks it gets from now (at the new rate, by the new mix),
    /// and each source's looks lock onto its pictures again. The old mixes'
    /// frames still being made are encoded as they're done. The Display and
    /// Window sources' captures look as fast as the looks.
    fn change_blend(&mut self, c: BlendChange) {
        let looks = c.looks.max(1);
        let k = self.closed + 1;
        self.looks = looks as i64;
        self.step = 1.0 / (self.fps.max(1) * looks) as f64;
        self.shutter = c.shutter;
        self.sources[0].mix = Some(c.mix);
        for i in 0..self.sources.len() {
            let s = &mut self.sources[i];
            if i > 0 {
                s.mix = None;
            }
            // Frame `k`'s own time is where it was: looks `k * looks` on, at
            // the new step. The first picture each stream times puts its
            // looks halfway between its pictures again.
            (s.phase, s.timed, s.next, s.different) = (0.0, false, self.shutter.first_look(k), 0);
            s.stats = SourceStats::new();
            if let (Some(up), Some(id)) = (s.up.clone(), s.held) {
                self.hold(i, up, id);
            }
        }
        self.frames.feeds().set_fps(self.fps * looks);
        self.follow_due = true;
        crate::blending(self.fps, looks);
        crate::blend_sources(self.sources.iter().filter(|s| s.shown).count(), self.told);
        crate::eprintln_later!("{}", if looks > 1 { format!("frame blending: {looks} looks a frame from now on") } else { "frame blending: off from now on".to_owned() });
    }

    /// The stream started or stopped, if the session says so. A stopped
    /// stream's pictures are finished on a thread of their own (that waits
    /// for their encoders).
    fn take_stream(&mut self, ctl: &PacerCtl) {
        let Some(change) = ctl.stream.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        let old = std::mem::replace(&mut self.stream, change);
        if old.is_empty() {
            return;
        }
        let _ = thread::Builder::new().name("stream-end".into()).spawn(move || old.into_iter().for_each(StreamOut::finish));
    }
}

/// What the blending saw, printed every few seconds (to the log), a line
/// for each source the frames showed (see [`SourceStats`]). Written by
/// another thread, as on Windows: the pacer has a few milliseconds between
/// pictures.
struct BlendStats {
    since: Instant,
}

/// One source's part of [`BlendStats`]: how many pictures it showed, how
/// many different ones each frame got (all its looks, when it changes at
/// least at the blend rate), and how many came too late for their frame.
/// With one look a frame (not blended), a frame with none is a picture
/// repeated.
struct SourceStats {
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
}

impl SourceStats {
    fn new() -> Self {
        Self { pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0 }
    }

    /// A frame's own looks are in: `different` pictures among them.
    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
    }
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now() }
    }

    /// Every few seconds: a line for each source the frames showed, then
    /// it counts afresh.
    fn log(&mut self, sources: &mut [Source], looks: u32) {
        let gone = self.since.elapsed();
        if gone < Duration::from_secs(5) {
            return;
        }
        let kind = if looks == 1 { "frame timing" } else { "frame blending" };
        for s in sources.iter_mut() {
            let st = std::mem::replace(&mut s.stats, SourceStats::new());
            if st.frames == 0 {
                continue;
            }
            let who = if s.id == 0 { format!("{kind}: the screen") } else { format!("{kind} of {}: it", s.name) };
            crate::eprintln_later!(
                "{who} showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames",
                st.pictures as f64 / gone.as_secs_f64(),
                st.fewest,
                st.most,
                st.full as f64 * 100.0 / st.frames as f64,
                st.late,
                st.frames
            );
        }
        *self = Self::new();
    }
}

/// How long each step of making a frame took (see `BlendPacer::close`).
#[derive(Clone, Copy, Default)]
struct Steps {
    /// The looks' mixing given to the graphics card, or not blended, the
    /// picture taken.
    mix: Duration,
    /// The webcam put over it, or the screen into its box (the graphics card
    /// waited for), and the preview offered it.
    make: Duration,
    /// Handed to the encoder.
    encode: Duration,
}

/// Where frames wait, logged for every 5 s in which frames were skipped
/// (every 5 s with `HASTECLIP_ENCODE_LOG=1`), as on Windows, so a log from a
/// Mac that can't keep up says what it's short of: how late frames were made
/// and how long making one took, and which of its steps the slowest one's
/// time went to; how long after a blended frame was closed the graphics card
/// had made it, and the longest a picture took to be added to a blend (the
/// graphics card is waited for when it's behind); how long the encoder held
/// frames; the fewest free places in its queue; and how many frames were
/// skipped.
struct EncodeReport {
    verbose: bool,
    since: Instant,
    frames: u32,
    late_max: f64,
    close_max: Duration,
    slowest: Steps,
    gpu_max: Duration,
    add_max: Duration,
    free_min: usize,
    skipped: u32,
}

impl EncodeReport {
    fn new() -> Self {
        Self {
            verbose: std::env::var_os("HASTECLIP_ENCODE_LOG").is_some(),
            since: Instant::now(),
            frames: 0,
            late_max: 0.0,
            close_max: Duration::ZERO,
            slowest: Steps::default(),
            gpu_max: Duration::ZERO,
            add_max: Duration::ZERO,
            free_min: usize::MAX,
            skipped: 0,
        }
    }

    /// A picture took `took` to be added to the frames it counts toward.
    fn added(&mut self, took: Duration) {
        self.add_max = self.add_max.max(took);
    }

    /// A frame closed `late` seconds after its time, made by the GPU `gpu`
    /// after (blended), in `steps`, with `free` places in the encoder's
    /// queue; `skipped` if the encoder couldn't take it.
    fn frame(&mut self, late: f64, steps: Steps, gpu: Option<Duration>, free: usize, skipped: bool, encoder: &Encoder) {
        self.frames += 1;
        self.gpu_max = self.gpu_max.max(gpu.unwrap_or_default());
        self.late_max = self.late_max.max(late);
        let took = steps.mix + steps.make + steps.encode;
        if took > self.close_max {
            self.close_max = took;
            self.slowest = steps;
        }
        self.free_min = self.free_min.min(free);
        self.skipped += skipped as u32;
        if self.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let (held, held_most, held_count) = encoder.held();
        if self.verbose || self.skipped > 0 {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            crate::eprintln_later!(
                "encode: {} frames; closed up to {:.1} ms late, making one took up to {:.1} ms (mixing {:.1}, compositor {:.1}, encoder {:.1}); blended frames made by the GPU up to {:.1} ms after closing, a picture took up to {:.1} ms to add; encoder held frames {:.1} ms avg, {:.1} max; fewest free places {}; skipped {}",
                self.frames,
                self.late_max * 1000.0,
                ms(self.close_max),
                ms(self.slowest.mix),
                ms(self.slowest.make),
                ms(self.slowest.encode),
                ms(self.gpu_max),
                ms(self.add_max),
                ms(held) / held_count.max(1) as f64,
                ms(held_most),
                self.free_min,
                self.skipped
            );
        }
        *self = Self { verbose: self.verbose, ..Self::new() };
    }
}

/// The pacer: `looks` looks at the screen per frame, each taking the picture
/// that was up at its time (by when the screen showed it), kept halfway
/// between the screen's pictures; the frame is made of them as `mix` says
/// (blended, or one look's picture as it is). Frame `k` is stamped `k / fps`
/// on the host clock, the audio's: a frame is made every 1/fps whether the
/// screen changed or not (constant frame rate), and when the encoder can't
/// take one it's skipped, not queued, so video never falls behind. What
/// `ctl` says changed (frame blending, the stream) is taken up at its next
/// turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_blend_pacer(
    fps: u32,
    looks: u32,
    shutter: Shutter,
    latest: Arc<Latest>,
    mix: Mix,
    frames: Frames,
    encoder: Encoder,
    clock: Arc<Clock>,
    ctl: PacerCtl,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    latest.queue_arrivals();
    let encoder = SendEncoder(encoder);
    let handle = thread::spawn(move || {
        crate::blending(fps, looks);
        let t0 = host_now();
        clock.set(t0);
        let bell = Arc::new(crate::shutter::Bell::default());
        latest.set_bell(Some(bell.clone()));
        let mut p = BlendPacer {
            t0,
            fps,
            step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
            looks: looks.max(1) as i64,
            shutter,
            closed: -1,
            sources: vec![Source::new(0, "the screen".to_owned(), None, Some(mix), 0.0, 0)],
            latest: latest.clone(),
            told: 0,
            bell,
            follow_due: true,
            frames,
            encoder,
            stream: Vec::new(),
            skipped: 0,
            stats: BlendStats::new(),
            report: EncodeReport::new(),
            making: Default::default(),
        };
        let seen = latest.seq();
        p.sources[0].seen = seen;
        if let Some(b) = latest.get() {
            p.hold(0, b, seen);
        }
        // Rung by any source's capture once something new is there: read
        // before the pictures are taken, so one that comes meanwhile wakes
        // the wait at once.
        let mut rung = p.bell.rung();
        while !stop2.load(Ordering::Relaxed) {
            // Frame blending changed, the stream started or stopped.
            let change = ctl.blend.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(c) = change {
                p.change_blend(c);
            }
            p.take_stream(&ctl);
            if std::mem::take(&mut p.follow_due) {
                p.follow_layers();
            }
            let wait = p.frame_end() + BLEND_LATE - host_now();
            if wait > 0.0 {
                p.bell.wait(rung, Duration::from_secs_f64(wait.min(0.05)));
            }
            rung = p.bell.rung();
            p.take_pictures();
            // Looks whose time is past (and late pictures' too) see what's up.
            p.look_until(host_now() - BLEND_LATE);
            p.encode_made(false);
        }
        p.encode_made(true);
        // The layers' captures queue no more pictures (they run on until
        // the session's done with them).
        for s in &p.sources {
            if let Some(pictures) = &s.pictures {
                pictures.blend_as(None);
            }
        }
        latest.set_bell(None);
        for s in std::mem::take(&mut p.stream) {
            s.finish();
        }
        p.encoder.0.finish();
    });
    (stop, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::*;

    fn filled(w: usize, h: usize, y: u8, cb: u8, cr: u8) -> Arc<Buffer> {
        let b = FramePool::new(w, h).unwrap().take().unwrap();
        unsafe {
            CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0));
            for (plane, vals) in [(0usize, vec![y]), (1, vec![cb, cr])] {
                let base = CVPixelBufferGetBaseAddressOfPlane(&b, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&b, plane);
                let cols = CVPixelBufferGetWidthOfPlane(&b, plane) * vals.len();
                for r in 0..CVPixelBufferGetHeightOfPlane(&b, plane) {
                    for x in 0..cols {
                        *base.add(r * stride + x) = vals[x % vals.len()];
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0));
        }
        Arc::new(Buffer(b))
    }

    /// Frame `k` finished, once the GPU has made it (the blender doesn't
    /// wait for it).
    fn made(b: &mut Blender, k: i64) -> Option<Arc<Buffer>> {
        let out = b.finish(k)?;
        if let Some(work) = b.last_work() {
            work.waitUntilCompleted();
        }
        Some(out)
    }

    fn sample(b: &CVPixelBuffer) -> (u8, u8, u8) {
        unsafe {
            CVPixelBufferLockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            let y = *(CVPixelBufferGetBaseAddressOfPlane(b, 0) as *const u8);
            let c = CVPixelBufferGetBaseAddressOfPlane(b, 1) as *const u8;
            let out = (y, *c, *c.add(1));
            CVPixelBufferUnlockBaseAddress(b, CVPixelBufferLockFlags::ReadOnly);
            out
        }
    }

    /// One picture blended with itself comes out as it went in.
    #[test]
    fn one_picture_is_unchanged() {
        let mut b = Blender::new(64, 32, 1).unwrap();
        b.hold(filled(64, 32, 120, 100, 150));
        b.add(0, 1.0);
        b.add(0, 1.0);
        let out = made(&mut b, 0).unwrap();
        let (y, cb, cr) = sample(&out.0);
        assert!(y.abs_diff(120) <= 1 && cb.abs_diff(100) <= 1 && cr.abs_diff(150) <= 1, "{y} {cb} {cr}");
        assert!(b.finish(1).is_none(), "the next frame starts empty");
    }

    /// White seen one look in four over black: a quarter of white's light,
    /// which is far brighter than a quarter of its value.
    #[test]
    fn averages_light() {
        let mut b = Blender::new(64, 32, 1).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add(0, 1.0);
        b.hold(filled(64, 32, 16, 128, 128));
        b.add(0, 1.0);
        b.add(0, 1.0);
        b.add(0, 1.0);
        let (y, cb, cr) = sample(&made(&mut b, 0).unwrap().0);
        // sRGB of 0.25 light is 0.537: 16 + 219 * 0.537 = 134.
        assert!(y.abs_diff(134) <= 1, "{y}");
        assert_eq!((cb, cr), (128, 128));
    }

    /// A source whose looks are ahead of another's starts the next frame
    /// before the one before is made: each frame keeps its own looks (on
    /// Windows, one average a frame mixed a fifth picture into some). A
    /// frame all of whose looks saw one picture is that picture as it is.
    #[test]
    fn the_next_frame_starts_before_this_one_is_made() {
        let mut b = Blender::new(64, 32, 1).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add(0, 1.0);
        b.add(0, 1.0);
        let black = filled(64, 32, 16, 128, 128);
        b.hold(black.clone());
        b.add(1, 1.0);
        let (y0, _, _) = sample(&made(&mut b, 0).unwrap().0);
        assert!(y0.abs_diff(235) <= 1, "frame 0 is its own white looks: {y0}");
        b.add(1, 1.0);
        let one = made(&mut b, 1).unwrap();
        assert!(Arc::ptr_eq(&one, &black), "frame 1 is the black picture itself");
        let mut m = Mix::one();
        m.hold(filled(64, 32, 235, 128, 128));
        m.add(0, 1.0);
        m.hold(black.clone());
        m.add(1, 1.0);
        let (y0, _, _) = sample(&m.finish(0).unwrap().0);
        assert!(y0.abs_diff(235) <= 1, "one look a frame: frame 0 is the picture its look saw: {y0}");
        assert!(m.finish(1).is_some_and(|p| Arc::ptr_eq(&p, &black)));
    }

    /// Two frames made at once (blended softly) each get their own looks,
    /// by their own weights.
    #[test]
    fn two_frames_at_once() {
        let mut b = Blender::new(64, 32, 2).unwrap();
        b.hold(filled(64, 32, 235, 128, 128));
        b.add(0, 0.25);
        b.add(1, 0.75);
        b.hold(filled(64, 32, 16, 128, 128));
        b.add(0, 0.75);
        b.add(1, 0.25);
        let (y0, _, _) = sample(&made(&mut b, 0).unwrap().0);
        let (y1, _, _) = sample(&made(&mut b, 1).unwrap().0);
        // sRGB of 0.75 light is 0.881: 16 + 219 * 0.881 = 209.
        assert!(y0.abs_diff(134) <= 1, "{y0}");
        assert!(y1.abs_diff(209) <= 1, "{y1}");
    }
}
