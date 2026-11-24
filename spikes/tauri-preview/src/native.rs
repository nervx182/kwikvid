//! Path A: wgpu renders frames into a native Metal layer that sits *under*
//! the (transparent) webview. The HTML UI stays on top; the video never
//! crosses into the webview at all.
//!
//! macOS only in this spike. On other platforms `start` returns an error and
//! the benchmark logs the path as skipped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::pattern;
use crate::stats::{PathResult, Samples};

pub const FRAME_W: u32 = 1920;
pub const FRAME_H: u32 = 1080;

type RunOutcome = (wgpu::Surface<'static>, Result<PathResult, String>);

struct Running {
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    started: Instant,
    handle: JoinHandle<RunOutcome>,
}

#[derive(Default)]
struct Inner {
    instance: Option<wgpu::Instance>,
    /// Created once on the main thread and handed to each render thread.
    surface: Option<wgpu::Surface<'static>>,
    running: Option<Running>,
}

#[derive(Default)]
pub struct Native {
    inner: Mutex<Inner>,
}

impl Native {
    /// Starts the render thread. `size` is the drawable size in physical pixels.
    pub fn start(&self, window: &tauri::WebviewWindow, size: (u32, u32)) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.running.is_some() {
            return Err("native path already running".into());
        }
        if inner.surface.is_none() {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
            let surface = create_surface_on_main(window, instance.clone())?;
            inner.instance = Some(instance);
            inner.surface = Some(surface);
        }
        let instance = inner.instance.clone().expect("instance");
        let surface = inner.surface.take().expect("surface");
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let (stop2, frames2) = (stop.clone(), frames.clone());
        let handle = std::thread::Builder::new()
            .name("native-render".into())
            .spawn(move || {
                let res = render_loop(&instance, &surface, size, &stop2, &frames2);
                (surface, res)
            })
            .map_err(|e| e.to_string())?;
        inner.running = Some(Running {
            stop,
            frames,
            started: Instant::now(),
            handle,
        });
        Ok(())
    }

    /// Frames presented so far and seconds since start (for the live HUD).
    pub fn progress(&self) -> (u64, f64) {
        let inner = self.inner.lock().unwrap();
        match &inner.running {
            Some(r) => (
                r.frames.load(Ordering::Relaxed),
                r.started.elapsed().as_secs_f64(),
            ),
            None => (0, 0.0),
        }
    }

    pub fn stop(&self) -> Result<PathResult, String> {
        let running = self
            .inner
            .lock()
            .unwrap()
            .running
            .take()
            .ok_or("native path not running")?;
        running.stop.store(true, Ordering::Relaxed);
        let (surface, res) = running
            .handle
            .join()
            .map_err(|_| "render thread panicked".to_string())?;
        // Keep the surface (and its CAMetalLayer) for the next run.
        self.inner.lock().unwrap().surface = Some(surface);
        res
    }
}

#[cfg(target_os = "macos")]
fn create_surface_on_main(
    window: &tauri::WebviewWindow,
    instance: wgpu::Instance,
) -> Result<wgpu::Surface<'static>, String> {
    let (tx, rx) = mpsc::channel();
    let w = window.clone();
    window
        .run_on_main_thread(move || {
            let _ = tx.send(macos::create_surface(&w, &instance));
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|e| format!("main thread did not answer: {e}"))?
}

#[cfg(not(target_os = "macos"))]
fn create_surface_on_main(
    _window: &tauri::WebviewWindow,
    _instance: wgpu::Instance,
) -> Result<wgpu::Surface<'static>, String> {
    // Keep the imports used on every platform.
    let _ = (mpsc::channel::<()>, Duration::ZERO);
    Err("native wgpu layer is only implemented for macOS in this spike".into())
}

#[cfg(target_os = "macos")]
mod macos {
    // The spike needs a handful of raw AppKit calls; production code would
    // wrap these in a small audited module in kv-app.
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
    use wgpu::rwh::{AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle};

    /// Adds a plain NSView *below* the WKWebView in the window's content view
    /// and creates a wgpu surface on it (wgpu attaches a CAMetalLayer).
    pub fn create_surface(
        window: &tauri::WebviewWindow,
        instance: &wgpu::Instance,
    ) -> Result<wgpu::Surface<'static>, String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        let content = window.ns_view().map_err(|e| e.to_string())? as *mut NSView;
        // SAFETY: Tauri returns the live content view of this window, and we
        // only touch it on the main thread (checked above).
        let content: &NSView = unsafe { content.as_ref() }.ok_or("null content view")?;
        let child = NSView::initWithFrame(mtm.alloc::<NSView>(), content.bounds());
        child.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // `Below` with no sibling puts it under every existing subview,
        // i.e. under the webview.
        content.addSubview_positioned_relativeTo(&child, NSWindowOrderingMode::Below, None);
        // Leak one retain: the view lives as long as the window (the spike
        // never removes it), which the surface below relies on.
        let ptr = Retained::into_raw(child) as *mut c_void;
        let handle = AppKitWindowHandle::new(NonNull::new(ptr).ok_or("null view")?);
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: RawDisplayHandle::AppKit(AppKitDisplayHandle::new()),
            raw_window_handle: RawWindowHandle::AppKit(handle),
        };
        // SAFETY: the view outlives the surface (leaked above), and surface
        // creation happens on the main thread as wgpu's Metal backend requires.
        unsafe { instance.create_surface_unsafe(target) }.map_err(|e| e.to_string())
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

fn render_loop(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'static>,
    size: (u32, u32),
    stop: &AtomicBool,
    frames: &AtomicU64,
) -> Result<PathResult, String> {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(surface),
        force_fallback_adapter: false,
    }))
    .map_err(|e| format!("no adapter: {e}"))?;
    let adapter_name = adapter.get_info().name;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("spike"),
        ..Default::default()
    }))
    .map_err(|e| format!("no device: {e}"))?;

    let mut config = surface
        .get_default_config(&adapter, size.0.max(1), size.1.max(1))
        .ok_or("surface not supported by adapter")?;
    config.present_mode = wgpu::PresentMode::Fifo; // vsync: what a player would use
    surface.configure(&device, &config);

    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame"),
        size: wgpu::Extent3d {
            width: FRAME_W,
            height: FRAME_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("blit"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("blit"),
        layout: None,
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
            targets: &[Some(config.format.into())],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview: None,
        cache: None,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("blit"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });

    let (lat_tx, lat_rx) = mpsc::channel::<f64>();
    let mut samples = Samples::default();
    let mut buf = Vec::new();
    let started = Instant::now();
    let mut last_present: Option<Instant> = None;
    let mut n: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        // t0 = the moment a "decoded frame" starts to exist on the CPU.
        let t0 = Instant::now();
        let gen_start = Instant::now();
        pattern::render(n, FRAME_W, FRAME_H, &mut buf);
        samples.gen_ms.push(ms(gen_start.elapsed()));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &buf,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(FRAME_W * 4),
                rows_per_image: Some(FRAME_H),
            },
            wgpu::Extent3d {
                width: FRAME_W,
                height: FRAME_H,
                depth_or_array_layers: 1,
            },
        );
        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                surface.configure(&device, &config);
                continue;
            }
            Err(e) => return Err(format!("acquire failed: {e}")),
        };
        let target = frame.texture.create_view(&Default::default());
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
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
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([enc.finish()]);
        let tx = lat_tx.clone();
        queue.on_submitted_work_done(move || {
            let _ = tx.send(ms(t0.elapsed()));
        });
        frame.present();
        let now = Instant::now();
        if let Some(prev) = last_present {
            samples.interval_ms.push(ms(now - prev));
        }
        last_present = Some(now);
        let _ = device.poll(wgpu::PollType::Poll);
        samples.latency_ms.extend(lat_rx.try_iter());
        n += 1;
        frames.store(n, Ordering::Relaxed);
    }
    let _ = device.poll(wgpu::PollType::Wait);
    samples.latency_ms.extend(lat_rx.try_iter());
    let secs = started.elapsed().as_secs_f64();
    Ok(PathResult::from_samples(
        "native-wgpu",
        secs,
        n,
        n,
        &samples,
        format!(
            "adapter={adapter_name}; surface={}x{} {:?}; frame {FRAME_W}x{FRAME_H} CPU-generated, uploaded per frame; latency = frame generation start -> GPU work done (add ~1 vsync to glass)",
            config.width, config.height, config.format
        ),
    ))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
