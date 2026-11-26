//! Where preview frames go: the compositor that draws them and the sink
//! that shows them.

use std::collections::HashMap;

use kv_core::Micros;
use kv_media::VideoFrame;

use super::PreviewError;
use crate::{composite, overlay, Fit, Overlay, OverlayId};

/// A composited preview frame, valid for the duration of
/// [`FrameSink::present`].
#[derive(Debug)]
pub enum RenderedFrame<'a> {
    /// Pixels in memory (CPU compositor). `pts` is the frame's time on the
    /// section timeline.
    Cpu(&'a VideoFrame),
    /// The GPU compositor's output texture ([`crate::gpu::OUTPUT_FORMAT`]).
    #[cfg(feature = "gpu")]
    Gpu {
        texture: &'a wgpu::Texture,
        pts: Micros,
    },
}

impl RenderedFrame<'_> {
    /// Presentation time on the section timeline.
    pub fn pts(&self) -> Micros {
        match self {
            RenderedFrame::Cpu(f) => f.pts,
            #[cfg(feature = "gpu")]
            RenderedFrame::Gpu { pts, .. } => *pts,
        }
    }

    /// Width and height in pixels.
    pub fn size(&self) -> (u32, u32) {
        match self {
            RenderedFrame::Cpu(f) => (f.width, f.height),
            #[cfg(feature = "gpu")]
            RenderedFrame::Gpu { texture, .. } => (texture.width(), texture.height()),
        }
    }
}

/// Receives the frames the preview engine decides to show, in order.
///
/// Implemented later by the native wgpu layer and the WebGL fallback
/// (ADR 0007). `present` runs on the thread that calls
/// [`PreviewEngine::tick`](super::PreviewEngine::tick); it should hand the
/// frame over and return quickly.
pub trait FrameSink: Send {
    fn present(&mut self, frame: &RenderedFrame<'_>);
}

/// Draws a preview frame from decoded layers. [`CpuCompositor`] and, with
/// the `gpu` feature, [`crate::gpu::GpuCompositor`] implement it.
pub trait PreviewCompositor: Send {
    /// Output size in pixels; the engine decodes sources for this size.
    fn size(&self) -> (u32, u32);

    /// Set (`Some`) or clear (`None`) the image drawn for overlay `id`: a
    /// straight-alpha frame of exactly [`size`](Self::size). The engine
    /// calls it only when the image changes (a new title text), so a GPU
    /// compositor uploads it once and reuses it for every frame.
    fn set_overlay(
        &mut self,
        id: OverlayId,
        image: Option<&VideoFrame>,
    ) -> Result<(), PreviewError>;

    /// Composite `layers` (frame, weight) with `fit` into a frame at `pts`.
    fn render(
        &mut self,
        pts: Micros,
        layers: &[(&VideoFrame, f32)],
        fit: Fit,
    ) -> Result<RenderedFrame<'_>, PreviewError> {
        self.render_with_overlays(pts, layers, &[], fit)
    }

    /// Like [`render`](Self::render), then alpha-blend `overlays` on top in
    /// order, like export ([`crate::overlay()`]). An overlay whose image is
    /// not set is skipped.
    fn render_with_overlays(
        &mut self,
        pts: Micros,
        layers: &[(&VideoFrame, f32)],
        overlays: &[Overlay],
        fit: Fit,
    ) -> Result<RenderedFrame<'_>, PreviewError>;
}

/// The CPU export compositor ([`composite()`], ADR 0009) as a preview
/// compositor: used on machines without a GPU and in tests.
#[derive(Debug, Clone)]
pub struct CpuCompositor {
    out: VideoFrame,
    overlays: HashMap<OverlayId, VideoFrame>,
}

impl CpuCompositor {
    /// A compositor drawing `width x height` frames.
    pub fn new(width: u32, height: u32) -> Result<Self, PreviewError> {
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
            return Err(PreviewError::InvalidSize { width, height });
        }
        Ok(CpuCompositor {
            out: VideoFrame::solid(Micros(0), width, height, [0, 0, 0, 255]),
            overlays: HashMap::new(),
        })
    }
}

impl PreviewCompositor for CpuCompositor {
    fn size(&self) -> (u32, u32) {
        (self.out.width, self.out.height)
    }

    fn set_overlay(
        &mut self,
        id: OverlayId,
        image: Option<&VideoFrame>,
    ) -> Result<(), PreviewError> {
        match image {
            Some(image) => self.overlays.insert(id, image.clone()),
            None => self.overlays.remove(&id),
        };
        Ok(())
    }

    fn render_with_overlays(
        &mut self,
        pts: Micros,
        layers: &[(&VideoFrame, f32)],
        overlays: &[Overlay],
        fit: Fit,
    ) -> Result<RenderedFrame<'_>, PreviewError> {
        composite(layers, &mut self.out, fit)?;
        for o in overlays {
            if let Some(image) = self.overlays.get(&o.id) {
                overlay(&mut self.out, image, o.opacity)?;
            }
        }
        self.out.pts = pts;
        Ok(RenderedFrame::Cpu(&self.out))
    }
}

#[cfg(feature = "gpu")]
impl PreviewCompositor for crate::gpu::GpuCompositor {
    fn size(&self) -> (u32, u32) {
        (self.width(), self.height())
    }

    fn set_overlay(
        &mut self,
        id: OverlayId,
        image: Option<&VideoFrame>,
    ) -> Result<(), PreviewError> {
        if let Some(old) = self.preview_overlays.remove(&id) {
            self.release(old)?;
        }
        if let Some(image) = image {
            let tex = self.upload(image)?;
            self.preview_overlays.insert(id, tex);
        }
        Ok(())
    }

    fn render_with_overlays(
        &mut self,
        pts: Micros,
        layers: &[(&VideoFrame, f32)],
        overlays: &[Overlay],
        fit: Fit,
    ) -> Result<RenderedFrame<'_>, PreviewError> {
        let mut ids = Vec::with_capacity(layers.len());
        let mut result = Ok(());
        for (frame, weight) in layers {
            match self.upload(frame) {
                Ok(id) => ids.push((id, *weight)),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            let over: Vec<_> = overlays
                .iter()
                .filter_map(|o| Some((*self.preview_overlays.get(&o.id)?, o.opacity)))
                .collect();
            // The inherent method: draw the uploaded textures.
            result = crate::gpu::GpuCompositor::render_with_overlays(self, &ids, &over, fit);
        }
        for (id, _) in ids {
            // Returned to the pool for the next frame; the id is ours.
            let _ = self.release(id);
        }
        result?;
        Ok(RenderedFrame::Gpu {
            texture: self.output(),
            pts,
        })
    }
}
