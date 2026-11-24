//! GPU compositor for the live preview (ADR 0007), on wgpu.
//!
//! [`GpuCompositor`] draws weighted layers (RGBA8 frames uploaded as
//! textures) into an `out_w x out_h` target with the same placement and
//! resampling as the CPU export compositor ([`crate::composite()`], ADR 0009),
//! so the preview shows what export writes. Every layer is one draw of a
//! full-screen triangle in a single render pass, blended additively; the
//! shader does the bilinear taps itself (see `composite.wgsl`).
//!
//! Differences from the CPU path: each draw is quantised to 8 bits before
//! the next one is added, and the bilinear taps are computed in `f32`
//! instead of `f64`. Parity tests allow a per-channel difference of 3.
//!
//! The device and queue come from a [`GpuContext`]. [`WgpuContext::headless`]
//! picks any adapter, software ones included, so the compositor runs in
//! Linux CI on lavapipe. Presenting to a window is the app's job: it renders
//! into its swapchain texture with [`GpuCompositor::render_to_view`].

use std::collections::HashMap;
use std::sync::mpsc;

use kv_media::{PixelFormat, VideoFrame};

use crate::composite::Placement;
use crate::Fit;

/// The output texture format of [`GpuCompositor::render`].
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Bytes of the `Layer` uniform in `composite.wgsl`. Each layer's copy sits
/// at a multiple of the device's uniform offset alignment.
const UNIFORM_SIZE: u64 = 48;

/// Errors from the GPU compositor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    /// No adapter (hardware or software) is available.
    #[error("no GPU adapter: {0}")]
    NoAdapter(String),
    /// The adapter refused to create a device.
    #[error("cannot create GPU device: {0}")]
    Device(String),
    /// A frame or output size is unusable (empty, inconsistent, too big).
    #[error("invalid {which}: {width}x{height}, {detail}")]
    InvalidSize {
        which: &'static str,
        width: u32,
        height: u32,
        detail: String,
    },
    /// The texture id was never returned by `upload`, or was released.
    #[error("unknown texture {0:?}")]
    UnknownTexture(TextureId),
    /// Waiting for the GPU or mapping the readback buffer failed.
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

/// Owner of the wgpu device and queue the compositor runs on.
///
/// [`WgpuContext`] is the implementation; the app adds a constructor for a
/// window surface (`for_surface`) when it has one (out of scope here).
pub trait GpuContext {
    /// A context on any available adapter, without a surface. Prefers a
    /// hardware adapter and falls back to a software one (lavapipe,
    /// WARP). Fails with [`GpuError::NoAdapter`] when there is none.
    fn headless() -> Result<Self, GpuError>
    where
        Self: Sized;
    fn device(&self) -> &wgpu::Device;
    fn queue(&self) -> &wgpu::Queue;
    /// Adapter name, backend and type, for logs.
    fn adapter_info(&self) -> wgpu::AdapterInfo;
}

/// The standard [`GpuContext`]: one wgpu instance, adapter, device, queue.
#[derive(Debug)]
pub struct WgpuContext {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl WgpuContext {
    fn from_adapter(adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("kv-render"),
            required_limits: limits,
            ..Default::default()
        }))
        .map_err(|e| GpuError::Device(e.to_string()))?;
        Ok(WgpuContext {
            adapter,
            device,
            queue,
        })
    }
}

impl GpuContext for WgpuContext {
    fn headless() -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let request = |force_fallback_adapter| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter,
                compatible_surface: None,
            }))
        };
        let adapter = match request(false) {
            Ok(a) => a,
            Err(first) => {
                request(true).map_err(|e| GpuError::NoAdapter(format!("{first}; {e}")))?
            }
        };
        Self::from_adapter(adapter)
    }

    fn device(&self) -> &wgpu::Device {
        &self.device
    }

    fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    fn adapter_info(&self) -> wgpu::AdapterInfo {
        self.adapter.get_info()
    }
}

/// Handle to a texture uploaded with [`GpuCompositor::upload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId(u64);

struct Source {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Composites weighted layers on the GPU into an `out_w x out_h` target.
pub struct GpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    out_w: u32,
    out_h: u32,
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    layout: wgpu::BindGroupLayout,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    /// One pipeline per target format (the offscreen output, a swapchain).
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    uniforms: wgpu::Buffer,
    uniform_stride: u64,
    uniform_slots: u64,
    live: HashMap<TextureId, Source>,
    /// Released textures, reusable by size.
    free: HashMap<(u32, u32), Vec<Source>>,
    next_id: u64,
}

impl GpuCompositor {
    /// A compositor drawing `out_w x out_h` frames on `ctx`'s device.
    pub fn new(ctx: &impl GpuContext, out_w: u32, out_h: u32) -> Result<Self, GpuError> {
        let device = ctx.device().clone();
        let queue = ctx.queue().clone();
        check_size(&device, "output size", out_w, out_h)?;
        let (output, output_view) = create_texture(
            &device,
            "kv-render output",
            out_w,
            out_h,
            OUTPUT_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kv-render layer"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kv-render composite"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kv-render composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });
        let align = u64::from(device.limits().min_uniform_buffer_offset_alignment);
        let uniform_stride = UNIFORM_SIZE.div_ceil(align) * align;
        let uniform_slots = 2;
        let uniforms = create_uniforms(&device, uniform_stride * uniform_slots);
        let mut compositor = GpuCompositor {
            device,
            queue,
            out_w,
            out_h,
            output,
            output_view,
            layout,
            shader,
            pipeline_layout,
            pipelines: HashMap::new(),
            uniforms,
            uniform_stride,
            uniform_slots,
            live: HashMap::new(),
            free: HashMap::new(),
            next_id: 0,
        };
        compositor.pipeline(OUTPUT_FORMAT);
        Ok(compositor)
    }

    /// Output width in pixels.
    pub fn width(&self) -> u32 {
        self.out_w
    }

    /// Output height in pixels.
    pub fn height(&self) -> u32 {
        self.out_h
    }

    /// The offscreen output texture ([`OUTPUT_FORMAT`]) that
    /// [`render`](Self::render) draws into.
    pub fn output(&self) -> &wgpu::Texture {
        &self.output
    }

    /// Upload an RGBA8 frame. Reuses a released texture of the same size
    /// when there is one; release it with [`release`](Self::release) when
    /// the frame is no longer shown.
    pub fn upload(&mut self, frame: &VideoFrame) -> Result<TextureId, GpuError> {
        let PixelFormat::Rgba8 = frame.format;
        check_size(&self.device, "frame", frame.width, frame.height)?;
        let expected = frame.width as usize * frame.height as usize * 4;
        if frame.data.len() != expected {
            return Err(GpuError::InvalidSize {
                which: "frame",
                width: frame.width,
                height: frame.height,
                detail: format!("holds {} bytes, expected {expected}", frame.data.len()),
            });
        }
        let key = (frame.width, frame.height);
        let source = match self.free.get_mut(&key).and_then(Vec::pop) {
            Some(s) => s,
            None => {
                let (texture, view) = create_texture(
                    &self.device,
                    "kv-render layer",
                    frame.width,
                    frame.height,
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                );
                Source { texture, view }
            }
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &source.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            extent(frame.width, frame.height),
        );
        let id = TextureId(self.next_id);
        self.next_id += 1;
        self.live.insert(id, source);
        Ok(id)
    }

    /// Return a texture to the pool for reuse by a later `upload` of the
    /// same size.
    pub fn release(&mut self, id: TextureId) -> Result<(), GpuError> {
        let source = self.live.remove(&id).ok_or(GpuError::UnknownTexture(id))?;
        let key = (source.texture.width(), source.texture.height());
        self.free.entry(key).or_default().push(source);
        Ok(())
    }

    /// Number of textures allocated (uploaded plus pooled).
    pub fn texture_count(&self) -> usize {
        self.live.len() + self.free.values().map(Vec::len).sum::<usize>()
    }

    /// Draw `layers` (texture, weight) into the offscreen output: each
    /// scaled with `fit`, weighted and summed, like [`crate::composite()`].
    /// No layers draws opaque black.
    pub fn render(&mut self, layers: &[(TextureId, f32)], fit: Fit) -> Result<(), GpuError> {
        let view = self.output_view.clone();
        self.render_to_view(&view, OUTPUT_FORMAT, layers, fit)
    }

    /// Like [`render`](Self::render), into a caller's texture view (a
    /// swapchain image, say) of format `format`. The view must be
    /// `width() x height()` pixels and allow `RENDER_ATTACHMENT`. Use a
    /// non-sRGB format: values are written gamma-encoded, as is.
    pub fn render_to_view(
        &mut self,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        layers: &[(TextureId, f32)],
        fit: Fit,
    ) -> Result<(), GpuError> {
        let mut params = Vec::with_capacity(layers.len());
        for &(id, weight) in layers {
            let source = self.live.get(&id).ok_or(GpuError::UnknownTexture(id))?;
            let (w, h) = (source.texture.width(), source.texture.height());
            params.push(uniform_bytes(w, h, self.out_w, self.out_h, fit, weight));
        }
        let slots = params.len() as u64;
        if slots > self.uniform_slots {
            self.uniform_slots = slots.next_power_of_two();
            self.uniforms = create_uniforms(&self.device, self.uniform_stride * self.uniform_slots);
        }
        for (i, bytes) in params.iter().enumerate() {
            self.queue
                .write_buffer(&self.uniforms, i as u64 * self.uniform_stride, bytes);
        }
        let bind_groups: Vec<wgpu::BindGroup> = layers
            .iter()
            .enumerate()
            .map(|(i, (id, _))| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("kv-render layer"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &self.uniforms,
                                offset: i as u64 * self.uniform_stride,
                                size: wgpu::BufferSize::new(UNIFORM_SIZE),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&self.live[id].view),
                        },
                    ],
                })
            })
            .collect();
        // Layers add up from zero; with none the frame is opaque black,
        // like the CPU compositor.
        let clear = if layers.is_empty() {
            wgpu::Color::BLACK
        } else {
            wgpu::Color::TRANSPARENT
        };
        let pipeline = self.pipeline(format).clone();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kv-render composite"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kv-render composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&pipeline);
            for group in &bind_groups {
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Copy the offscreen output back to the CPU (blocks until the GPU is
    /// done). For tests and stills; the preview never reads back.
    pub fn read_back(&self) -> Result<VideoFrame, GpuError> {
        let data = read_texture(&self.device, &self.queue, &self.output)?;
        Ok(VideoFrame {
            pts: kv_core::Micros(0),
            width: self.out_w,
            height: self.out_h,
            format: PixelFormat::Rgba8,
            data,
        })
    }

    fn pipeline(&mut self, format: wgpu::TextureFormat) -> &wgpu::RenderPipeline {
        let (device, shader, layout) = (&self.device, &self.shader, &self.pipeline_layout);
        self.pipelines.entry(format).or_insert_with(|| {
            let add = wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("kv-render composite"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState {
                            color: add,
                            alpha: add,
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        })
    }
}

fn check_size(device: &wgpu::Device, which: &'static str, w: u32, h: u32) -> Result<(), GpuError> {
    let max = device.limits().max_texture_dimension_2d;
    if w == 0 || h == 0 || w > max || h > max {
        return Err(GpuError::InvalidSize {
            which,
            width: w,
            height: h,
            detail: format!("each side must be in 1..={max}"),
        });
    }
    Ok(())
}

fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

fn create_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn create_uniforms(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("kv-render layers"),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The `Layer` uniform of `composite.wgsl` for a `src_w x src_h` layer.
fn uniform_bytes(
    src_w: u32,
    src_h: u32,
    out_w: u32,
    out_h: u32,
    fit: Fit,
    weight: f32,
) -> [u8; UNIFORM_SIZE as usize] {
    let p = Placement::new(src_w, src_h, out_w, out_h, fit);
    let ratio = |src: u32, scaled: u32| {
        if scaled == 0 {
            1.0
        } else {
            (f64::from(src) / f64::from(scaled)) as f32
        }
    };
    let to_i32 = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
    let ints = [
        to_i32(p.x0 as i64),
        to_i32(p.x1 as i64),
        to_i32(p.y0 as i64),
        to_i32(p.y1 as i64),
        to_i32(p.off_x),
        to_i32(p.off_y),
    ];
    let floats = [ratio(src_w, p.sw), ratio(src_h, p.sh), weight];
    let mut out = [0u8; UNIFORM_SIZE as usize];
    let words = ints
        .iter()
        .map(|v| v.to_ne_bytes())
        .chain(floats.iter().map(|v| v.to_ne_bytes()));
    for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word);
    }
    out
}

/// Read an RGBA8 / BGRA8 texture back as tightly packed rows.
fn read_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Result<Vec<u8>, GpuError> {
    let (w, h) = (texture.width(), texture.height());
    let row = w * 4;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("kv-render readback"),
        size: u64::from(padded) * u64::from(h),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("kv-render readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        extent(w, h),
    );
    queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait)
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    rx.recv()
        .map_err(|e| GpuError::Readback(e.to_string()))?
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    let mut data = Vec::with_capacity(row as usize * h as usize);
    {
        let mapped = slice.get_mapped_range();
        for line in mapped.chunks_exact(padded as usize) {
            data.extend_from_slice(&line[..row as usize]);
        }
    }
    buffer.unmap();
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::Micros;

    fn ctx() -> Option<WgpuContext> {
        match WgpuContext::headless() {
            Ok(c) => Some(c),
            Err(e) => {
                println!("SKIPPED: no GPU adapter ({e})");
                None
            }
        }
    }

    #[test]
    fn uniform_layout_matches_shader() {
        // 4:3 into 16:9 Fill: scaled 64x48 into 64x36, offset (0, -6).
        let b = uniform_bytes(32, 24, 64, 36, Fit::Fill, 0.5);
        let word = |i: usize| <[u8; 4]>::try_from(&b[i * 4..i * 4 + 4]).unwrap();
        let ints: Vec<i32> = (0..6).map(|i| i32::from_ne_bytes(word(i))).collect();
        assert_eq!(ints, [0, 64, 0, 36, 0, -6]);
        assert_eq!(f32::from_ne_bytes(word(6)), 0.5);
        assert_eq!(f32::from_ne_bytes(word(7)), 0.5);
        assert_eq!(f32::from_ne_bytes(word(8)), 0.5);
    }

    #[test]
    fn render_to_caller_view_in_bgra() {
        let Some(ctx) = ctx() else { return };
        let mut c = GpuCompositor::new(&ctx, 8, 4).unwrap();
        let id = c
            .upload(&VideoFrame::solid(Micros(0), 8, 4, [10, 20, 30, 255]))
            .unwrap();
        let (tex, view) = create_texture(
            &c.device,
            "test target",
            8,
            4,
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        c.render_to_view(
            &view,
            wgpu::TextureFormat::Bgra8Unorm,
            &[(id, 1.0)],
            Fit::Fill,
        )
        .unwrap();
        let data = read_texture(&c.device, &c.queue, &tex).unwrap();
        for px in data.chunks_exact(4) {
            assert_eq!(px, [30, 20, 10, 255]);
        }
    }
}
