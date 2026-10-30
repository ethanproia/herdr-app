//! Browser frames on screen: an IOSurface the host shared (contract D3) becomes a CVPixelBuffer
//! (`CVPixelBufferCreateWithIOSurface`, no copy) painted with GPUI's `surface()` element.
//! OWNER: part 3 (frames).
//!
//! Lifetimes: `FrameView` holds exactly one frame (the newest). Replacing it drops the old
//! `Surface` and `CVPixelBuffer`; GPUI's scene keeps its own retain until the next scene, and the
//! Metal renderer keeps its CVMetalTextures until the GPU is done (HERDR: patch in
//! vendor/gpui-pre-apple). No image cache is involved, so herdr issue 22 (image cache leak) does
//! not apply here.

use gpui::{AnyElement, IntoElement, ObjectFit, Styled, surface};
use herdr_browser_protocol::{PixelSize, Size, SurfaceRef};
use herdr_browser_surface::{Surface, import};

/// What BrowserView feeds frames into (re-exported from `src/browser/mod.rs`).
pub trait FrameSink {
    /// A `HostToApp::Frame` arrived: import and show it from the next paint on.
    fn on_frame(&mut self, seq: u64, surface: SurfaceRef, size: PixelSize, scale: f32) -> anyhow::Result<()>;
    /// The block was resized to `dips`; keep stretching the last frame until a matching one arrives.
    fn on_resize_pending(&mut self, dips: Size);
    /// The seq to send as `AppToHost::FrameAck`, once that frame has been painted; taken once.
    fn ack(&mut self) -> Option<u64>;
}

#[cfg(target_os = "macos")]
mod cv {
    use core_foundation::base::TCFType;
    use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef};
    use std::ffi::c_void;

    unsafe extern "C" {
        fn CVPixelBufferCreateWithIOSurface(a: *const c_void, s: *const c_void, attrs: *const c_void, out: *mut CVPixelBufferRef) -> i32;
    }

    /// Wrap (not copy) an IOSurface as a CVPixelBuffer; the buffer retains the surface.
    pub fn wrap(surface: &herdr_browser_surface::Surface) -> anyhow::Result<CVPixelBuffer> {
        let mut out: CVPixelBufferRef = std::ptr::null_mut();
        let k = unsafe { CVPixelBufferCreateWithIOSurface(std::ptr::null(), surface.as_ptr(), std::ptr::null(), &mut out) };
        anyhow::ensure!(k == 0 && !out.is_null(), "CVPixelBufferCreateWithIOSurface failed: {k}");
        Ok(unsafe { CVPixelBuffer::wrap_under_create_rule(out) })
    }
}

struct Frame {
    seq: u64,
    size: PixelSize,
    scale: f32,
    #[cfg(target_os = "macos")]
    buffer: core_video::pixel_buffer::CVPixelBuffer,
    // Dropped after `buffer` (field order), releasing our retain of the IOSurface.
    _surface: Surface,
}

#[derive(Default)]
pub struct FrameView {
    frame: Option<Frame>,
    pending_resize: Option<Size>,
    painted: Option<u64>,
}

impl FrameView {
    pub fn new() -> Self {
        Self::default()
    }

    /// The element to put inside the block's bounds: the newest frame filling them (stretched
    /// while a resize is pending), or `None` before the first frame. Marks that frame painted.
    pub fn element(&mut self) -> Option<AnyElement> {
        #[cfg(target_os = "macos")]
        {
            let frame = self.frame.as_ref()?;
            self.painted = Some(frame.seq);
            Some(surface(frame.buffer.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element())
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    /// True while the shown frame does not match the size the block asked for.
    pub fn stretching(&self) -> bool {
        match (&self.frame, self.pending_resize) {
            (Some(f), Some(want)) => {
                let w = (want.w * f.scale).ceil() as u32;
                let h = (want.h * f.scale).ceil() as u32;
                (w, h) != (f.size.w, f.size.h)
            }
            _ => false,
        }
    }

    pub fn seq(&self) -> Option<u64> {
        self.frame.as_ref().map(|f| f.seq)
    }

    /// Drop the frame (block closed or host crashed).
    pub fn clear(&mut self) {
        self.frame = None;
        self.painted = None;
    }
}

impl FrameSink for FrameView {
    fn on_frame(&mut self, seq: u64, surface: SurfaceRef, size: PixelSize, scale: f32) -> anyhow::Result<()> {
        if self.frame.as_ref().is_some_and(|f| f.seq >= seq) {
            return Ok(());
        }
        let imported = import(&surface)?;
        #[cfg(target_os = "macos")]
        let buffer = cv::wrap(&imported)?;
        self.frame = Some(Frame {
            seq,
            size,
            scale,
            #[cfg(target_os = "macos")]
            buffer,
            _surface: imported,
        });
        if !self.stretching() {
            self.pending_resize = None;
        }
        Ok(())
    }

    fn on_resize_pending(&mut self, dips: Size) {
        self.pending_resize = Some(dips);
    }

    fn ack(&mut self) -> Option<u64> {
        self.painted.take()
    }
}
