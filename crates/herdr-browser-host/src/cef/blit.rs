//! One GPU copy per frame: CEF's IOSurface (valid only inside OnAcceleratedPaint) into a
//! host-owned pool surface the app can hold until it acks. No CPU copy.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLOrigin, MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use std::ffi::c_void;

pub struct Blitter {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

impl Blitter {
    pub fn new() -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        Some(Self { device, queue })
    }

    fn texture(&self, surface: *mut c_void, w: usize, h: usize) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, w, h, false) };
        desc.setStorageMode(MTLStorageMode::Shared);
        desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
        let surface = unsafe { &*(surface as *const IOSurfaceRef) };
        self.device.newTextureWithDescriptor_iosurface_plane(&desc, surface, 0)
    }

    /// Copy `src` into `dst` (both BGRA, `w` x `h` pixels) and wait for the GPU to finish.
    pub fn copy(&self, src: *mut c_void, dst: *mut c_void, w: usize, h: usize) -> Result<(), String> {
        let s = self.texture(src, w, h).ok_or("cannot wrap CEF's IOSurface as a texture")?;
        let d = self.texture(dst, w, h).ok_or("cannot wrap the pool IOSurface as a texture")?;
        let cmd = self.queue.commandBuffer().ok_or("no command buffer")?;
        let enc = cmd.blitCommandEncoder().ok_or("no blit encoder")?;
        unsafe { enc.copyFromTexture_toTexture(&s, &d) };
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        Ok(())
    }

    /// Copy the `w` x `h` region at `from` in `src` (`ss` pixels) to `to` in `dst` (`ds` pixels).
    /// The caller clips the region to both surfaces.
    pub fn copy_region(&self, src: *mut c_void, ss: (usize, usize), from: (usize, usize), dst: *mut c_void, ds: (usize, usize), to: (usize, usize), w: usize, h: usize) -> Result<(), String> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        let s = self.texture(src, ss.0, ss.1).ok_or("cannot wrap the popup surface as a texture")?;
        let d = self.texture(dst, ds.0, ds.1).ok_or("cannot wrap the frame surface as a texture")?;
        let cmd = self.queue.commandBuffer().ok_or("no command buffer")?;
        let enc = cmd.blitCommandEncoder().ok_or("no blit encoder")?;
        unsafe {
            enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &s,
                0,
                0,
                MTLOrigin { x: from.0, y: from.1, z: 0 },
                MTLSize { width: w, height: h, depth: 1 },
                &d,
                0,
                0,
                MTLOrigin { x: to.0, y: to.1, z: 0 },
            )
        };
        enc.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        Ok(())
    }
}
