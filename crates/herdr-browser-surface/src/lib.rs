//! IOSurface frames shared between `herdr-browser-host` (writer) and `herdr-app` (reader).
//! See docs/browser-block-contract.md, D3. OWNER: part 3 (frames).
//!
//! A `Surface` is a retained BGRA IOSurface. The host creates one per frame slot, CEF paints into
//! it, and `export` turns it into a `SurfaceRef` the app can `import` in another process. Pixels
//! never cross the socket and nothing here copies a frame except `read_bgra`, which is for tests.

use herdr_browser_protocol::{PixelSize, SurfaceRef};
use std::fmt;

#[cfg(target_os = "macos")]
mod sys;
mod pool;

pub use pool::{FramePool, PoolFrame};

#[derive(Debug)]
pub enum Error {
    Create(PixelSize),
    Lookup(String),
    Lock(i32),
    Export(String),
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create(s) => write!(f, "IOSurfaceCreate failed for {}x{}", s.w, s.h),
            Error::Lookup(r) => write!(f, "IOSurface import failed: {r}"),
            Error::Lock(k) => write!(f, "IOSurfaceLock failed: kern {k}"),
            Error::Export(r) => write!(f, "IOSurface export failed: {r}"),
            Error::Unsupported => write!(f, "IOSurface needs macOS"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// How the host hands a surface to the app. `GlobalId` is the default (see OQ1 in the contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    GlobalId,
    MachPort,
}

/// A retained IOSurface (released on drop). `Send` because IOSurface refcounting is thread-safe.
pub struct Surface {
    #[cfg(target_os = "macos")]
    raw: sys::IOSurfaceRef,
    size: PixelSize,
    /// Set when exported over the bootstrap server; the registration dies with this surface.
    #[cfg(target_os = "macos")]
    port: Option<(String, u32)>,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

/// Create a BGRA surface for a `size` DIP view at `scale`: `ceil(size * scale)` pixels.
pub fn create(size: herdr_browser_protocol::Size, scale: f32) -> Result<Surface> {
    let px = PixelSize { w: (size.w * scale).ceil().max(1.0) as u32, h: (size.h * scale).ceil().max(1.0) as u32 };
    create_pixels(px)
}

/// Create a BGRA surface of exactly `px` pixels.
pub fn create_pixels(px: PixelSize) -> Result<Surface> {
    #[cfg(target_os = "macos")]
    {
        let raw = unsafe { sys::create_bgra(px.w, px.h) };
        if raw.is_null() {
            return Err(Error::Create(px));
        }
        Ok(Surface { raw, size: px, port: None })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = px;
        Err(Error::Unsupported)
    }
}

/// Import a surface another process exported. Retains it; no pixel copy.
pub fn import(r: &SurfaceRef) -> Result<Surface> {
    #[cfg(target_os = "macos")]
    {
        let raw = unsafe {
            match r {
                SurfaceRef::GlobalId { id } => sys::IOSurfaceLookup(*id),
                SurfaceRef::MachPort { name } => sys::lookup_bootstrap(name).map_err(Error::Lookup)?,
            }
        };
        if raw.is_null() {
            return Err(Error::Lookup(format!("{r:?} not found")));
        }
        let size = unsafe { PixelSize { w: sys::IOSurfaceGetWidth(raw) as u32, h: sys::IOSurfaceGetHeight(raw) as u32 } };
        Ok(Surface { raw, size, port: None })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = r;
        Err(Error::Unsupported)
    }
}

impl Surface {
    pub fn size(&self) -> PixelSize {
        self.size
    }

    /// The IOSurface's id (stable for its lifetime, also the `GlobalId` value).
    pub fn id(&self) -> u32 {
        #[cfg(target_os = "macos")]
        unsafe {
            sys::IOSurfaceGetID(self.raw)
        }
        #[cfg(not(target_os = "macos"))]
        0
    }

    /// The raw `IOSurfaceRef` (borrowed; valid while `self` lives). For CVPixelBuffer wrapping.
    #[cfg(target_os = "macos")]
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.raw as *mut _
    }

    /// Export for another process. `GlobalId` is free; `MachPort` registers a bootstrap name
    /// (unique per surface) that stays valid until this `Surface` drops.
    pub fn export(&mut self, transport: Transport) -> Result<SurfaceRef> {
        match transport {
            Transport::GlobalId => Ok(SurfaceRef::GlobalId { id: self.id() }),
            Transport::MachPort => {
                #[cfg(target_os = "macos")]
                {
                    if let Some((name, _)) = &self.port {
                        return Ok(SurfaceRef::MachPort { name: name.clone() });
                    }
                    let name = format!("com.herdr.browser.surface.{}.{}", std::process::id(), self.id());
                    let port = unsafe { sys::register_bootstrap(self.raw, &name) }.map_err(Error::Export)?;
                    self.port = Some((name.clone(), port));
                    Ok(SurfaceRef::MachPort { name })
                }
                #[cfg(not(target_os = "macos"))]
                Err(Error::Unsupported)
            }
        }
    }

    /// True while another process (or another `Surface`) holds this IOSurface.
    pub fn in_use(&self) -> bool {
        #[cfg(target_os = "macos")]
        unsafe {
            sys::IOSurfaceIsInUse(self.raw) != 0
        }
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Write tightly packed BGRA rows (`w * 4` bytes each) into the surface. For the stub host and tests.
    pub fn write_bgra(&self, pixels: &[u8]) -> Result<()> {
        self.with_lock(false, |base, stride| {
            let row = self.size.w as usize * 4;
            for y in 0..self.size.h as usize {
                unsafe { std::ptr::copy_nonoverlapping(pixels[y * row..].as_ptr(), base.add(y * stride), row) };
            }
        })
    }

    /// Fill every pixel with one BGRA value.
    pub fn fill(&self, bgra: [u8; 4]) -> Result<()> {
        self.with_lock(false, |base, stride| {
            for y in 0..self.size.h as usize {
                for x in 0..self.size.w as usize {
                    unsafe { std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(y * stride + x * 4), 4) };
                }
            }
        })
    }

    /// Read one pixel (BGRA) without copying the frame.
    pub fn pixel(&self, x: u32, y: u32) -> Result<[u8; 4]> {
        let mut out = [0; 4];
        self.with_lock(true, |base, stride| unsafe {
            std::ptr::copy_nonoverlapping(base.add(y as usize * stride + x as usize * 4), out.as_mut_ptr(), 4)
        })?;
        Ok(out)
    }

    /// Copy the whole frame out as packed BGRA. Tests only; the paint path never copies.
    pub fn read_bgra(&self) -> Result<Vec<u8>> {
        let row = self.size.w as usize * 4;
        let mut out = vec![0; row * self.size.h as usize];
        self.with_lock(true, |base, stride| {
            for y in 0..self.size.h as usize {
                unsafe { std::ptr::copy_nonoverlapping(base.add(y * stride), out[y * row..].as_mut_ptr(), row) };
            }
        })?;
        Ok(out)
    }

    fn with_lock(&self, read_only: bool, f: impl FnOnce(*mut u8, usize)) -> Result<()> {
        #[cfg(target_os = "macos")]
        unsafe {
            let opts = if read_only { sys::K_IOSURFACE_LOCK_READ_ONLY } else { 0 };
            let k = sys::IOSurfaceLock(self.raw, opts, std::ptr::null_mut());
            if k != 0 {
                return Err(Error::Lock(k));
            }
            f(sys::IOSurfaceGetBaseAddress(self.raw) as *mut u8, sys::IOSurfaceGetBytesPerRow(self.raw));
            sys::IOSurfaceUnlock(self.raw, opts, std::ptr::null_mut());
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (read_only, f);
            Err(Error::Unsupported)
        }
    }
}

impl Clone for Surface {
    /// Another retain of the same IOSurface (no pixel copy; no bootstrap registration).
    fn clone(&self) -> Self {
        #[cfg(target_os = "macos")]
        unsafe {
            sys::CFRetain(self.raw as *const _);
        }
        Surface {
            #[cfg(target_os = "macos")]
            raw: self.raw,
            size: self.size,
            #[cfg(target_os = "macos")]
            port: None,
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if let Some((_, port)) = self.port.take() {
                sys::release_port(port);
            }
            sys::CFRelease(self.raw as *const _);
        }
    }
}

impl fmt::Debug for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Surface(id={}, {}x{})", self.id(), self.size.w, self.size.h)
    }
}
