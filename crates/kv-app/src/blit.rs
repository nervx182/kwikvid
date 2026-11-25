//! The GPU half of presenting preview frames, without any window system:
//! the device the compositor and the window surface share
//! ([`SurfaceGpu`]), and [`Blitter`], which keeps the last preview frame
//! and draws it letterboxed into a target (a swapchain image on macOS).
//! Portable, so the gate tests it headless on lavapipe.

// Only the macOS native layer uses this outside tests for now; the WebGL
// fallback (#76) does not need a GPU.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use kv_render::gpu::{GpuContext, GpuError, OUTPUT_FORMAT};
use kv_render::preview::RenderedFrame;

use crate::preview::{letterbox, PixelRect};

/// A [`GpuContext`] whose device can also present to a window surface. The
/// macOS layer creates it with `SurfaceGpu::for_surface` (in `native_view`);
/// the preview compositor runs on the same device, so its output texture
/// is drawn to the screen without leaving the GPU.
#[derive(Debug)]
pub struct SurfaceGpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl SurfaceGpu {
    /// An adapter able to present to `surface` (any adapter when `None`),
    /// hardware preferred, and a device on it.
    pub(crate) fn request(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<SurfaceGpu, GpuError> {
        let request = |force_fallback_adapter| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter,
                compatible_surface: surface,
            }))
        };
        let adapter = match request(false) {
            Ok(a) => a,
            Err(first) => {
                request(true).map_err(|e| GpuError::NoAdapter(format!("{first}; {e}")))?
            }
        };
        let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("kv-app preview"),
            required_limits: limits,
            ..Default::default()
        }))
        .map_err(|e| GpuError::Device(e.to_string()))?;
        Ok(SurfaceGpu {
            adapter,
            device,
            queue,
        })
    }

    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }
}

impl GpuContext for SurfaceGpu {
    fn headless() -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        SurfaceGpu::request(&instance, None)
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

/// The swapchain settings of the native preview layer: vsync
/// (`PresentMode::Fifo`, the preview's pacing) with at most one frame
/// queued, which the ADR 0007 spike found cuts a frame of latency over
/// wgpu's default of two. Opaque when the surface allows it.
pub fn surface_config(
    format: wgpu::TextureFormat,
    alpha_modes: &[wgpu::CompositeAlphaMode],
    size: (u32, u32),
) -> wgpu::SurfaceConfiguration {
    let alpha_mode = if alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto)
    };
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.0.max(1),
        height: size.1.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 1,
        alpha_mode,
        view_formats: Vec::new(),
    }
}

const SHADER: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@fragment fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

/// The last frame, kept so the viewer can be redrawn after a resize while
/// paused.
struct Last {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
}

/// Draws the last preview frame into a target view: black everywhere, the
/// frame scaled (bilinear) into the [`letterbox`] of its aspect ratio.
/// Values are copied as they are, so the target format must not be sRGB
/// (see [`crate::preview::pick_surface_format`]).
pub struct Blitter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    last: Option<Last>,
}

impl Blitter {
    /// A blitter drawing into views of `target_format`.
    pub fn new(ctx: &impl GpuContext, target_format: wgpu::TextureFormat) -> Blitter {
        let device = ctx.device().clone();
        let queue = ctx.queue().clone();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kv-app blit"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kv-app blit"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kv-app blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kv-app blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(target_format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kv-app blit"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Blitter {
            device,
            queue,
            pipeline,
            layout,
            sampler,
            last: None,
        }
    }

    /// Size of the kept frame, if any.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.last
            .as_ref()
            .map(|l| (l.texture.width(), l.texture.height()))
    }

    /// Keeps a copy of `frame` for [`draw`](Self::draw). A GPU frame must
    /// come from a compositor on this blitter's device. Returns false (and
    /// keeps the previous frame) for an unusable frame.
    pub fn set_frame(&mut self, frame: &RenderedFrame<'_>) -> bool {
        let (w, h) = frame.size();
        if w == 0 || h == 0 {
            return false;
        }
        if let RenderedFrame::Gpu { texture, .. } = frame {
            if texture.format() != OUTPUT_FORMAT {
                tracing::warn!(format = ?texture.format(), "preview frame in an unexpected format");
                return false;
            }
        }
        if self.frame_size() != Some((w, h)) {
            self.last = Some(self.create_last(w, h));
        }
        let Some(last) = &self.last else {
            return false;
        };
        let dst = wgpu::TexelCopyTextureInfo {
            texture: &last.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        };
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        match frame {
            RenderedFrame::Cpu(f) => {
                self.queue.write_texture(
                    dst,
                    &f.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * 4),
                        rows_per_image: Some(h),
                    },
                    size,
                );
            }
            RenderedFrame::Gpu { texture, .. } => {
                let mut enc = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("kv-app keep frame"),
                    });
                enc.copy_texture_to_texture(texture.as_image_copy(), dst, size);
                self.queue.submit([enc.finish()]);
            }
        }
        true
    }

    fn create_last(&self, w: u32, h: u32) -> Last {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kv-app last frame"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kv-app blit"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Last { texture, bind }
    }

    /// Where the kept frame lands in a `target` sized target (empty when
    /// there is no frame).
    pub fn placement(&self, target: (u32, u32)) -> PixelRect {
        self.frame_size()
            .map_or_else(PixelRect::default, |size| letterbox(target, size))
    }

    /// Clears `view` (`target` pixels, the format given to `new`) to black
    /// and draws the kept frame letterboxed into it.
    pub fn draw(&self, view: &wgpu::TextureView, target: (u32, u32)) {
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kv-app blit"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kv-app blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let r = self.placement(target);
            if let (Some(last), true) = (&self.last, r.w > 0 && r.h > 0) {
                pass.set_viewport(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &last.bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::Micros;
    use kv_media::VideoFrame;
    use kv_render::gpu::GpuCompositor;
    use kv_render::preview::PreviewCompositor;
    use kv_render::Fit;

    fn ctx() -> Option<SurfaceGpu> {
        match SurfaceGpu::headless() {
            Ok(c) => Some(c),
            Err(e) => {
                println!("SKIPPED: no GPU adapter ({e})");
                None
            }
        }
    }

    /// Draws into a fresh `w x h` RGBA8 texture and reads it back.
    fn draw(ctx: &SurfaceGpu, blit: &Blitter, w: u32, h: u32) -> Vec<u8> {
        let device = ctx.device();
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        blit.draw(&target.create_view(&Default::default()), (w, h));
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(padded * h),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            target.size(),
        );
        ctx.queue().submit([enc.finish()]);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::Wait).unwrap();
        let data = buffer.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in data.chunks(padded as usize) {
            out.extend_from_slice(&row[..(w * 4) as usize]);
        }
        out
    }

    fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        img[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn surface_config_is_vsync_with_one_frame_latency() {
        use wgpu::CompositeAlphaMode as A;
        let c = surface_config(
            wgpu::TextureFormat::Bgra8Unorm,
            &[A::PostMultiplied, A::Opaque],
            (0, 720),
        );
        assert_eq!(c.present_mode, wgpu::PresentMode::Fifo);
        assert_eq!(c.desired_maximum_frame_latency, 1);
        assert_eq!(c.alpha_mode, A::Opaque);
        assert_eq!((c.width, c.height), (1, 720));
        assert_eq!(c.format, wgpu::TextureFormat::Bgra8Unorm);
        let c = surface_config(wgpu::TextureFormat::Bgra8Unorm, &[A::PreMultiplied], (4, 4));
        assert_eq!(c.alpha_mode, A::PreMultiplied);
    }

    #[test]
    fn no_frame_draws_black() {
        let Some(ctx) = ctx() else { return };
        let blit = Blitter::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(blit.frame_size(), None);
        let img = draw(&ctx, &blit, 8, 8);
        assert!(img.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
    }

    #[test]
    fn cpu_frame_is_letterboxed_and_redrawn_after_resize() {
        let Some(ctx) = ctx() else { return };
        let mut blit = Blitter::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let red = VideoFrame::solid(Micros(0), 160, 90, [200, 30, 10, 255]);
        assert!(blit.set_frame(&RenderedFrame::Cpu(&red)));
        assert_eq!(blit.frame_size(), Some((160, 90)));

        // Square viewer: 100x56 picture, 22 px bars top and bottom.
        let img = draw(&ctx, &blit, 100, 100);
        assert_eq!(px(&img, 100, 50, 10), [0, 0, 0, 255]);
        assert_eq!(px(&img, 100, 50, 95), [0, 0, 0, 255]);
        assert_eq!(px(&img, 100, 50, 50), [200, 30, 10, 255]);
        assert_eq!(px(&img, 100, 0, 23), [200, 30, 10, 255]);

        // Wider viewer, same kept frame: pillarbox (178x100 at x = 11).
        assert_eq!(
            blit.placement((200, 100)),
            PixelRect {
                x: 11,
                y: 0,
                w: 178,
                h: 100
            }
        );
        let img = draw(&ctx, &blit, 200, 100);
        assert_eq!(px(&img, 200, 5, 50), [0, 0, 0, 255]);
        assert_eq!(px(&img, 200, 195, 50), [0, 0, 0, 255]);
        assert_eq!(px(&img, 200, 100, 50), [200, 30, 10, 255]);
    }

    /// The engine's GPU path: compositor output on the same device.
    #[test]
    fn gpu_compositor_frame_is_presented() {
        let Some(ctx) = ctx() else { return };
        let mut comp = GpuCompositor::new(&ctx, 90, 160).unwrap();
        let green = VideoFrame::solid(Micros(0), 90, 160, [20, 220, 40, 255]);
        let mut blit = Blitter::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        {
            let frame = PreviewCompositor::render(&mut comp, Micros(5), &[(&green, 1.0)], Fit::Fit)
                .unwrap();
            assert!(matches!(frame, RenderedFrame::Gpu { .. }));
            assert!(blit.set_frame(&frame));
        }
        // 9:16 in 160x90: pillarbox, 51 px wide picture.
        let img = draw(&ctx, &blit, 160, 90);
        assert_eq!(px(&img, 160, 10, 45), [0, 0, 0, 255]);
        let c = px(&img, 160, 80, 45);
        assert!(
            c.iter()
                .zip([20, 220, 40, 255])
                .all(|(a, b)| a.abs_diff(b) <= 3),
            "{c:?}"
        );
    }

    #[test]
    fn empty_frame_is_ignored() {
        let Some(ctx) = ctx() else { return };
        let mut blit = Blitter::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let frame = VideoFrame::solid(Micros(0), 4, 4, [1, 2, 3, 255]);
        assert!(blit.set_frame(&RenderedFrame::Cpu(&frame)));
        let empty = VideoFrame {
            width: 0,
            height: 0,
            data: Vec::new(),
            ..frame.clone()
        };
        assert!(!blit.set_frame(&RenderedFrame::Cpu(&empty)));
        assert_eq!(blit.frame_size(), Some((4, 4)));
    }
}
