//! The recording's frame made on the graphics card (`super::gpu`): the
//! picture put into the frame's size (`crate::placement`), the webcam over
//! it, and NV12 for the encoder, in compute shaders; only the NV12 frame
//! comes down to the CPU (an eighth of the screen's pixels at 1440p → 1080p).
//! The same as `super::image` does on the CPU, which this replaces when the
//! screen's pictures are on the card.
//!
//! A picture that arrives in our memory (the away screen, X11) is uploaded
//! first. The webcam is drawn by the CPU's code (`Canvas::draw_camera`, only
//! its box, only when it changes) into a layer laid over the frame here.

use std::sync::Arc;

use super::gpu::Gpu;
use super::image::{CameraPicture, Canvas};
use super::screen::{Frame, Order};
use crate::webcam::Placement;

const SCALE_SHADER: &str = r#"
struct Scale {
    // The part of the source shown (x, y, w, h), and where in the frame (x, y, w, h).
    src: vec4<f32>,
    dst: vec4<f32>,
    // The frame's size, and whether a webcam layer is laid over it.
    size: vec2<u32>,
    layer: u32,
    _pad: u32,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var frame: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> s: Scale;
@group(0) @binding(3) var overlay: texture_2d<f32>;

// The source interval an output sample covers, at least one sample wide
// (enlarging then blends the nearest two), as `Taps` does on the CPU.
fn span(i: f32, scale: f32, n: f32) -> vec2<f32> {
    var a = i * scale;
    var b = (i + 1.0) * scale;
    if (b - a < 1.0) {
        let c = (a + b) / 2.0;
        a = max(c - 0.5, 0.0);
        b = min(a + 1.0, n);
        a = b - 1.0;
    }
    return vec2<f32>(a, b);
}

// Each frame pixel: the average of the source area it covers; black outside
// the picture's place; the webcam layer over it where it's drawn.
@compute @workgroup_size(8, 8)
fn scale(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= s.size.x || id.y >= s.size.y) {
        return;
    }
    var c = vec3<f32>(0.0);
    let p = vec2<f32>(id.xy);
    if (p.x >= s.dst.x && p.y >= s.dst.y && p.x < s.dst.x + s.dst.z && p.y < s.dst.y + s.dst.w) {
        let sx = span(p.x - s.dst.x, s.src.z / s.dst.z, s.src.z) + s.src.x;
        let sy = span(p.y - s.dst.y, s.src.w / s.dst.w, s.src.w) + s.src.y;
        var sum = vec3<f32>(0.0);
        var total = 0.0;
        for (var y = floor(sy.x); y < sy.y; y += 1.0) {
            let wy = min(sy.y, y + 1.0) - max(sy.x, y);
            for (var x = floor(sx.x); x < sx.y; x += 1.0) {
                let w = (min(sx.y, x + 1.0) - max(sx.x, x)) * wy;
                sum += textureLoad(source, vec2<i32>(i32(x), i32(y)), 0).rgb * w;
                total += w;
            }
        }
        c = sum / max(total, 1e-6);
    }
    if (s.layer != 0u) {
        let l = textureLoad(overlay, vec2<i32>(id.xy), 0);
        if (l.a > 0.5) {
            c = l.rgb;
        }
    }
    textureStore(frame, vec2<i32>(id.xy), vec4<f32>(c, 1.0));
}
"#;

const NV12_SHADER: &str = r#"
struct Size {
    width: u32,
    height: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> nv12: array<u32>;
@group(0) @binding(2) var<uniform> n: Size;

fn rgb8(x: u32, y: u32) -> vec3<i32> {
    return vec3<i32>(round(textureLoad(image, vec2<i32>(i32(x), i32(y)), 0).rgb * 255.0));
}

// One byte of NV12 (BT.709, studio range): as `nv12_pair` on the CPU.
fn byte_at(b: u32) -> u32 {
    let w = n.width;
    let h = n.height;
    if (b < w * h) {
        let c = rgb8(b % w, b / w);
        return u32(((47 * c.r + 157 * c.g + 16 * c.b + 128) >> 8u) + 16);
    }
    let k = b - w * h;
    if (k >= w * h / 2u) {
        return 0u;
    }
    let cy = k / w;
    let cx = (k % w) / 2u;
    let y0 = cy * 2u;
    let y1 = min(y0 + 1u, h - 1u);
    let sum = rgb8(cx * 2u, y0) + rgb8(cx * 2u + 1u, y0) + rgb8(cx * 2u, y1) + rgb8(cx * 2u + 1u, y1);
    var v: i32;
    if (k % 2u == 0u) {
        v = 128 + ((-26 * sum.r - 86 * sum.g + 112 * sum.b + 512) >> 10u);
    } else {
        v = 128 + ((112 * sum.r - 102 * sum.g - 10 * sum.b + 512) >> 10u);
    }
    return u32(clamp(v, 16, 240));
}

// Four bytes of NV12 a thread: luma rows, then interleaved U, V rows.
@compute @workgroup_size(64)
fn to_nv12(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x + id.y * 65535u * 64u;
    if (i >= arrayLength(&nv12)) {
        return;
    }
    let b = i * 4u;
    nv12[i] = byte_at(b) | (byte_at(b + 1u) << 8u) | (byte_at(b + 2u) << 16u) | (byte_at(b + 3u) << 24u);
}
"#;

/// The frame being made on the card, and what it shows.
pub(crate) struct GpuPicture {
    gpu: Arc<Gpu>,
    pub width: u32,
    pub height: u32,
    framing: crate::Framing,
    scale: wgpu::ComputePipeline,
    nv12: wgpu::ComputePipeline,
    /// The frame, RGBA.
    frame: wgpu::Texture,
    uniforms: wgpu::Buffer,
    /// NV12 on the card, and where it's read back.
    nv12_buf: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// The webcam's layer: drawn on the CPU (only its box), laid over the frame.
    layer: Layer,
    /// A picture from our memory, uploaded: its texture and its number.
    uploaded: Option<(wgpu::Texture, u64)>,
    nv12_bind: wgpu::BindGroup,
}

/// The webcam over the frame: drawn into a frame-size canvas of ours (clear
/// where it isn't), uploaded where it changed.
struct Layer {
    canvas: Canvas,
    texture: wgpu::Texture,
    /// The rows last drawn on, to clear and upload.
    rows: Option<(usize, usize)>,
    /// Whether anything is drawn.
    on: bool,
}

impl GpuPicture {
    pub(crate) fn new(gpu: Arc<Gpu>, width: u32, height: u32, framing: crate::Framing) -> Self {
        let device = &gpu.device;
        let pipeline = |source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(entry), source: wgpu::ShaderSource::Wgsl(source.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: None, module: &module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let (scale, nv12) = (pipeline(SCALE_SHADER, "scale"), pipeline(NV12_SHADER, "to_nv12"));
        let texture = |label, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        let frame = texture("frame", wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC);
        let overlay = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("webcam layer"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uniform = |label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let (uniforms, sizes) = (uniform("scale"), uniform("size"));
        let bytes = nv12_len(width, height).div_ceil(4) as u64 * 4;
        let nv12_buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12"), size: bytes, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("nv12 read"), size: bytes, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let frame_view = frame.create_view(&Default::default());
        let nv12_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &nv12.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 1, resource: nv12_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: sizes.as_entire_binding() },
            ],
        });
        let size_bytes: Vec<u8> = [width, height, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        gpu.queue.write_buffer(&sizes, 0, &size_bytes);
        let layer = Layer { canvas: Canvas::new(width, height), texture: overlay, rows: None, on: false };
        Self { gpu, width, height, framing, scale, nv12, frame, uniforms, nv12_buf, readback, layer, uploaded: None, nv12_bind }
    }

    /// Draw the webcam's layer: `cam` placed at `place`, or none.
    pub(crate) fn set_camera(&mut self, cam: Option<(&CameraPicture, &Placement)>) {
        let l = &mut self.layer;
        let w = l.canvas.width as usize;
        let row = w * 4;
        // Clear what was drawn before.
        if let Some((top, bottom)) = l.rows {
            l.canvas.bgra[top * row..bottom * row].fill(0);
        }
        let drawn = cam.filter(|(_, p)| !p.is_hidden()).and_then(|(c, p)| {
            l.canvas.draw_camera(c, p);
            let (fh, ph) = (l.canvas.height as f32, p.h * l.canvas.height as f32);
            let top = (p.y * fh).max(0.0).floor() as usize;
            let bottom = ((p.y * fh + ph).min(fh).ceil() as usize).min(l.canvas.height as usize);
            (top < bottom).then_some((top, bottom))
        });
        // Upload the rows drawn now and before (cleared).
        let span = match (l.rows, drawn) {
            (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
            (a, b) => a.or(b),
        };
        if let Some((top, bottom)) = span {
            self.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &l.texture, mip_level: 0, origin: wgpu::Origin3d { x: 0, y: top as u32, z: 0 }, aspect: wgpu::TextureAspect::All },
                &l.canvas.bgra[top * row..bottom * row],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row as u32), rows_per_image: Some((bottom - top) as u32) },
                wgpu::Extent3d { width: w as u32, height: (bottom - top) as u32, depth_or_array_layers: 1 },
            );
        }
        l.rows = drawn;
        l.on = drawn.is_some();
    }

    /// Make the frame from `src` (picture number `seq`) on the card. Returns
    /// whether it could.
    pub(crate) fn draw(&mut self, src: &Frame, seq: u64) -> bool {
        let texture = match &src.texture {
            Some(t) => t.clone(),
            None => match self.upload(src, seq) {
                Some(t) => t,
                None => return false,
            },
        };
        let (sw, sh) = (texture.width(), texture.height());
        let ([sx, sy, sw, sh], [dx, dy, dw, dh]) = crate::placement((sw, sh), (self.width, self.height), self.framing);
        let floats: Vec<u8> = [sx, sy, sw, sh, dx, dy, dw, dh].iter().flat_map(|v| (*v as f32).to_le_bytes()).chain([self.width, self.height, u32::from(self.layer.on), 0].iter().flat_map(|v| v.to_le_bytes())).collect();
        self.gpu.queue.write_buffer(&self.uniforms, 0, &floats);
        let device = &self.gpu.device;
        let source_view = texture.create_view(&Default::default());
        let frame_view = self.frame.create_view(&Default::default());
        let overlay_view = self.layer.texture.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scale"),
            layout: &self.scale.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&frame_view) },
                wgpu::BindGroupEntry { binding: 2, resource: self.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&overlay_view) },
            ],
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.scale);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(self.width.div_ceil(8), self.height.div_ceil(8), 1);
        }
        self.gpu.queue.submit([enc.finish()]);
        true
    }

    /// The frame as NV12, into `out`: converted on the card, read back.
    pub(crate) fn nv12(&self, out: &mut Vec<u8>) {
        let device = &self.gpu.device;
        let words = (self.nv12_buf.size() / 4) as u32;
        let groups = words.div_ceil(64);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("nv12") });
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.nv12);
            pass.set_bind_group(0, &self.nv12_bind, &[]);
            // Past 65535 groups a dimension, rows of them.
            pass.dispatch_workgroups(groups.min(65535), groups.div_ceil(65535), 1);
        }
        enc.copy_buffer_to_buffer(&self.nv12_buf, 0, &self.readback, 0, self.nv12_buf.size());
        self.gpu.queue.submit([enc.finish()]);
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        out.clear();
        if let Ok(data) = slice.get_mapped_range() {
            out.extend_from_slice(&data[..nv12_len(self.width, self.height)]);
        }
        self.readback.unmap();
    }

    /// The frame shrunk to fit `max`, as R, G, B, A, for the preview. Its size.
    pub(crate) fn preview(&self, max: (u32, u32), out: &mut Vec<u8>) -> (u32, u32) {
        let mut small = Canvas::new(self.width, self.height);
        // Read back at full size and shrunk on the CPU: only while the
        // preview shows, a few times a second.
        let px = self.gpu.read(&self.frame);
        // The frame is R, G, B, A; the canvas is B, G, R, A.
        for (o, p) in small.bgra.chunks_exact_mut(4).zip(px.chunks_exact(4)) {
            o.copy_from_slice(&[p[2], p[1], p[0], 255]);
        }
        small.to_preview(max, out)
    }

    /// A picture from our memory on the card: uploaded once per picture.
    fn upload(&mut self, src: &Frame, seq: u64) -> Option<wgpu::Texture> {
        if let Some((t, s)) = &self.uploaded
            && *s == seq
            && t.width() == src.width
            && t.height() == src.height
        {
            return Some(t.clone());
        }
        if src.width == 0 || src.height == 0 || src.data.len() < src.stride * (src.height as usize - 1) + src.width as usize * 4 {
            return None;
        }
        let format = match src.order {
            Order::Bgr => wgpu::TextureFormat::Bgra8Unorm,
            Order::Rgb => wgpu::TextureFormat::Rgba8Unorm,
        };
        let reuse = self.uploaded.take().map(|(t, _)| t).filter(|t| t.width() == src.width && t.height() == src.height && t.format() == format);
        let t = reuse.unwrap_or_else(|| {
            self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("uploaded picture"),
                size: wgpu::Extent3d { width: src.width, height: src.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        self.gpu.queue.write_texture(
            t.as_image_copy(),
            &src.data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(src.stride as u32), rows_per_image: Some(src.height) },
            wgpu::Extent3d { width: src.width, height: src.height, depth_or_array_layers: 1 },
        );
        self.uploaded = Some((t.clone(), seq));
        Some(t)
    }
}

/// NV12's size: a full-size luma plane, then a half-size plane of U, V pairs.
fn nv12_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card makes the same NV12 as the CPU, give or take a step from
    /// rounding when scaling.
    #[test]
    #[ignore = "uses the graphics card: cargo test -p capture same_frame_as_the_cpu -- --ignored"]
    fn same_frame_as_the_cpu() {
        let gpu = super::super::gpu::gpu().expect("a graphics card");
        for (sw, sh, dw, dh, framing) in [(64, 36, 64, 36, crate::Framing::Fit), (2560, 1440, 1920, 1080, crate::Framing::Fit), (300, 200, 128, 128, crate::Framing::Fill), (40, 30, 64, 48, crate::Framing::Fit)] {
            let data: Vec<u8> = (0..sw * sh * 4).map(|i| ((i * 7919) % 251) as u8).collect();
            let frame = Frame { width: sw, height: sh, stride: sw as usize * 4, order: Order::Bgr, data, texture: None };
            let mut cpu = Canvas::new(dw, dh);
            cpu.fill_from(&frame, framing);
            let mut want = Vec::new();
            cpu.to_nv12(&mut want);
            let mut pic = GpuPicture::new(gpu.clone(), dw, dh, framing);
            assert!(pic.draw(&frame, 1));
            let mut got = Vec::new();
            pic.nv12(&mut got);
            assert_eq!(got.len(), want.len());
            let worst = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            let off = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            eprintln!("{sw}x{sh} → {dw}x{dh}: {off} of {} bytes differ, by at most {worst}", got.len());
            assert!(worst <= 2, "off by {worst}");
        }
    }
}
