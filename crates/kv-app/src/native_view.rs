//! The macOS native preview layer (ADR 0007): an `NSView` inserted *below*
//! the transparent WKWebView, with a wgpu surface (wgpu attaches a
//! `CAMetalLayer`) that preview frames are drawn into. The HTML UI floats
//! on top; the viewer region of the page is transparent.
//!
//! - [`NativeView::install`] adds the view and creates the surface and a
//!   [`SurfaceGpu`] whose device can present to it
//!   ([`SurfaceGpu::for_surface`]). The preview compositor must run on that
//!   device ([`NativeView::gpu`]) so its output never leaves the GPU.
//! - [`NativeView::set_rect`] moves the view onto the viewer rectangle the
//!   UI reports, resizes the surface and redraws the last frame.
//! - [`NativeSink`] is the preview engine's [`FrameSink`]: it keeps the
//!   frame and draws it letterboxed. With `PresentMode::Fifo` and one frame
//!   of latency, acquiring a drawable blocks until vsync, so the presenting
//!   thread is paced by the display.
//! - [`NativeView::start_test_pattern`] (`--preview-test-pattern`) feeds a
//!   moving pattern through the GPU compositor and this sink.
//!
//! The view is kept for the app's lifetime and never removed.

// Raw AppKit pointers from Tauri and wgpu's raw-handle surface constructor
// need `unsafe` (AGENTS.md allows it locally with a reason). Each block
// says why it is sound.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use kv_core::{AspectRatio, Micros};
use kv_media::VideoFrame;
use kv_render::gpu::{GpuCompositor, GpuContext, GpuError};
use kv_render::preview::{FrameSink, PreviewCompositor, RenderedFrame};
use kv_render::Fit;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{NSView, NSWindowOrderingMode};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tauri::WebviewWindow;
use wgpu::rwh::{AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle};

use crate::blit::{surface_config, Blitter, SurfaceGpu};
use crate::preview::{
    appkit_frame, pick_surface_format, render_test_pattern, test_pattern_size, ViewerRect,
};

thread_local! {
    /// The preview view and its parent (the window's content view). Set
    /// and read on the main thread only, which AppKit requires anyway.
    static VIEW: RefCell<Option<(Retained<NSView>, Retained<NSView>)>> = const { RefCell::new(None) };
}

impl SurfaceGpu {
    /// A context on an adapter that can present to `surface`.
    pub fn for_surface(
        instance: &wgpu::Instance,
        surface: &wgpu::Surface<'_>,
    ) -> Result<SurfaceGpu, GpuError> {
        SurfaceGpu::request(instance, Some(surface))
    }
}

/// Runs `f` on the main thread and returns its result (directly when
/// already there).
fn on_main<T: Send + 'static>(
    window: &WebviewWindow,
    f: impl FnOnce(MainThreadMarker) -> T + Send + 'static,
) -> Result<T, String> {
    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(f(mtm));
    }
    let (tx, rx) = mpsc::channel();
    window
        .run_on_main_thread(move || match MainThreadMarker::new() {
            Some(mtm) => {
                let _ = tx.send(f(mtm));
            }
            None => tracing::error!("run_on_main_thread ran off the main thread"),
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|e| format!("the main thread did not answer: {e}"))
}

/// Adds the preview view under every existing subview of the window's
/// content view (so under the webview) and creates a wgpu surface on it.
fn create_view_and_surface(
    window: &WebviewWindow,
    instance: &wgpu::Instance,
    mtm: MainThreadMarker,
) -> Result<wgpu::Surface<'static>, String> {
    let content = window
        .ns_view()
        .map_err(|e| e.to_string())?
        .cast::<NSView>();
    // SAFETY: Tauri returns this window's live content view (an NSView),
    // and we only use it on the main thread (`mtm`).
    let content: &NSView = unsafe { content.as_ref() }.ok_or("the window has no content view")?;
    let view = NSView::initWithFrame(mtm.alloc::<NSView>(), NSRect::ZERO);
    view.setHidden(true);
    // `Below` with no sibling: under all existing subviews.
    content.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, None);
    let ptr = NonNull::new(Retained::as_ptr(&view).cast_mut().cast::<c_void>())
        .ok_or("null preview view")?;
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: RawDisplayHandle::AppKit(AppKitDisplayHandle::new()),
        raw_window_handle: RawWindowHandle::AppKit(AppKitWindowHandle::new(ptr)),
    };
    // SAFETY: the view is never released (one retain is leaked just
    // below), so it outlives the surface; we are on the main thread, as
    // wgpu's Metal backend requires for surface creation.
    let surface = unsafe { instance.create_surface_unsafe(target) }.map_err(|e| e.to_string())?;
    std::mem::forget(view.clone());
    let parent = content.retain();
    VIEW.with(|v| *v.borrow_mut() = Some((view, parent)));
    Ok(surface)
}

/// Moves the preview view onto `rect`; hides it when empty. Main thread.
fn place_view(rect: &ViewerRect) {
    VIEW.with(|v| {
        if let Some((view, parent)) = &*v.borrow() {
            let f = appkit_frame(rect, parent.bounds().size.height, parent.isFlipped());
            view.setFrame(NSRect::new(NSPoint::new(f.x, f.y), NSSize::new(f.w, f.h)));
            view.setHidden(rect.is_empty());
        }
    });
}

/// The surface and what is drawn on it, shared by the sink (presenting
/// thread) and resizes (command thread).
struct Target {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    /// Physical size the surface should have; applied on the next draw.
    wanted: (u32, u32),
    blitter: Blitter,
}

impl Target {
    /// Draws the kept frame letterboxed. Blocks until a drawable is free
    /// (vsync). False when nothing was presented.
    fn draw(&mut self) -> bool {
        let (w, h) = self.wanted;
        if w == 0 || h == 0 {
            return false;
        }
        if !self.configured || (self.config.width, self.config.height) != (w, h) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
            self.configured = true;
        }
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.configured = false;
                return false;
            }
            Err(e) => {
                tracing::warn!("preview: no drawable: {e}");
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.blitter.draw(&view, (w, h));
        frame.present();
        true
    }
}

fn lock(target: &Mutex<Target>) -> MutexGuard<'_, Target> {
    // Target holds no invariants a panic could break half-way.
    target.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The installed native layer.
pub struct NativeView {
    window: WebviewWindow,
    gpu: Arc<SurfaceGpu>,
    target: Arc<Mutex<Target>>,
}

impl NativeView {
    /// Adds the view under `window`'s webview and sets up the surface.
    pub fn install(window: &WebviewWindow) -> Result<NativeView, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let (w, inst) = (window.clone(), instance.clone());
        let surface = on_main(window, move |mtm| create_view_and_surface(&w, &inst, mtm))??;
        let gpu = SurfaceGpu::for_surface(&instance, &surface).map_err(|e| e.to_string())?;
        let caps = surface.get_capabilities(gpu.adapter());
        let format = match pick_surface_format(&caps.formats) {
            Some(f) => f,
            None => {
                let f = *caps
                    .formats
                    .first()
                    .ok_or("the surface supports no format")?;
                tracing::warn!(format = ?f, "no non-sRGB surface format; colours will be off");
                f
            }
        };
        let config = surface_config(format, &caps.alpha_modes, (1, 1));
        let info = gpu.adapter_info();
        tracing::info!(adapter = %info.name, ?format, "native preview layer installed");
        let target = Target {
            surface,
            device: gpu.device().clone(),
            config,
            configured: false,
            wanted: (0, 0),
            blitter: Blitter::new(&gpu, format),
        };
        Ok(NativeView {
            window: window.clone(),
            gpu: Arc::new(gpu),
            target: Arc::new(Mutex::new(target)),
        })
    }

    /// The device to run the preview compositor on.
    pub fn gpu(&self) -> Arc<SurfaceGpu> {
        Arc::clone(&self.gpu)
    }

    /// A [`FrameSink`] presenting on this layer.
    pub fn sink(&self) -> NativeSink {
        NativeSink {
            target: Arc::clone(&self.target),
        }
    }

    /// Covers the viewer rectangle: moves the view, resizes the surface
    /// and redraws the last frame (letterboxed for the new shape).
    pub fn set_rect(&self, rect: ViewerRect) -> Result<(), String> {
        on_main(&self.window, move |_| place_view(&rect))?;
        let mut target = lock(&self.target);
        target.wanted = rect.physical_size();
        target.draw();
        Ok(())
    }

    /// Plays the moving test pattern at the project's aspect ratio through
    /// the GPU compositor and [`NativeSink`], forever, on its own thread.
    pub fn start_test_pattern(&self, aspect: AspectRatio) {
        let (gpu, sink) = (self.gpu(), self.sink());
        let spawned = std::thread::Builder::new()
            .name("kv-preview-test-pattern".into())
            .spawn(move || run_test_pattern(&gpu, sink, test_pattern_size(aspect)));
        if let Err(e) = spawned {
            tracing::warn!("cannot start the preview test pattern: {e}");
        }
    }
}

fn run_test_pattern(gpu: &SurfaceGpu, mut sink: NativeSink, (w, h): (u32, u32)) {
    let mut compositor = match GpuCompositor::new(gpu, w, h) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("preview test pattern: {e}");
            return;
        }
    };
    tracing::info!(width = w, height = h, "preview test pattern running");
    let mut frame = VideoFrame::solid(Micros(0), w, h, [0, 0, 0, 255]);
    for n in 0.. {
        render_test_pattern(n, &mut frame);
        let shown =
            match PreviewCompositor::render(&mut compositor, frame.pts, &[(&frame, 1.0)], Fit::Fit)
            {
                Ok(rendered) => sink.show(&rendered),
                Err(e) => {
                    tracing::warn!("preview test pattern: {e}");
                    false
                }
            };
        if !shown {
            // Hidden or not sized yet: nothing paces us, so do it here.
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}

/// The preview engine's [`FrameSink`] on the native layer.
pub struct NativeSink {
    target: Arc<Mutex<Target>>,
}

impl NativeSink {
    /// Keeps `frame` and draws it; false when nothing was presented
    /// (hidden viewer, unusable frame). Blocks until vsync otherwise.
    pub fn show(&mut self, frame: &RenderedFrame<'_>) -> bool {
        let mut target = lock(&self.target);
        target.blitter.set_frame(frame) && target.draw()
    }
}

impl FrameSink for NativeSink {
    fn present(&mut self, frame: &RenderedFrame<'_>) {
        self.show(frame);
    }
}

/// Tauri managed state: the native layer, installed on the first viewer
/// rectangle (the page has laid out by then).
#[derive(Default)]
pub struct NativePreview {
    view: OnceLock<Result<NativeView, String>>,
}

impl NativePreview {
    /// The layer, installing it on first use (and starting the test pattern
    /// when `test_pattern` names the project's aspect ratio). An error
    /// means the native path is unavailable for this run.
    pub fn view(
        &self,
        window: &WebviewWindow,
        test_pattern: Option<AspectRatio>,
    ) -> Result<&NativeView, String> {
        self.view
            .get_or_init(|| {
                let view = NativeView::install(window)?;
                if let Some(aspect) = test_pattern {
                    view.start_test_pattern(aspect);
                }
                Ok(view)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}
