//! Browser host: serves the browser protocol on a Unix socket. See docs/browser-block-contract.md.
//! OWNER: part 2 (host).
//!
//! Two backends serve the same protocol: `--stub` (no CEF; deterministic frames whose pixels encode
//! the browser id, size and seq, see `stub_pixel`; input echoed back as `Console` messages) and,
//! with the `cef` feature, the real CEF off-screen host in `cef`, which must run from its .app
//! bundle (scripts/fork-host-bundle.sh).

use herdr_browser_protocol::{PixelSize, SurfaceRef};
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

mod stub;
#[cfg(all(feature = "cef", target_os = "macos"))]
pub mod cef;

pub use stub::{id_byte, stub_pixel};

/// Where frames are drawn. The stub writes pixels; the CEF backend will blit into the same surfaces.
pub trait SurfaceAlloc {
    type Surface: HostSurface;
    fn create(&mut self, px: PixelSize) -> io::Result<Self::Surface>;
}

/// One shared frame buffer the host owns until the app acks it.
pub trait HostSurface {
    fn write_bgra(&self, pixels: &[u8]) -> io::Result<()>;
    fn export(&mut self) -> io::Result<SurfaceRef>;
}

/// Real IOSurfaces from `herdr-browser-surface`, exported by global id (contract D3 default).
pub struct IoSurfaces;

impl SurfaceAlloc for IoSurfaces {
    type Surface = herdr_browser_surface::Surface;
    fn create(&mut self, px: PixelSize) -> io::Result<Self::Surface> {
        herdr_browser_surface::create_pixels(px).map_err(io::Error::other)
    }
}

impl HostSurface for herdr_browser_surface::Surface {
    fn write_bgra(&self, pixels: &[u8]) -> io::Result<()> {
        herdr_browser_surface::Surface::write_bgra(self, pixels).map_err(io::Error::other)
    }
    fn export(&mut self) -> io::Result<SurfaceRef> {
        herdr_browser_surface::Surface::export(self, herdr_browser_surface::Transport::GlobalId).map_err(io::Error::other)
    }
}

/// Bind `socket` (replacing a stale file) and serve one app connection at a time until `Shutdown`.
pub fn serve<A: SurfaceAlloc>(socket: &Path, alloc: A) -> io::Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let mut host = stub::StubHost::new(alloc);
    for conn in listener.incoming() {
        if host.serve_conn(conn?)? {
            break;
        }
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}
