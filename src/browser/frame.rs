//! Browser frames on screen: an IOSurface the host shared (contract D3) becomes a CVPixelBuffer
//! (`CVPixelBufferCreateWithIOSurface`, no copy) painted with GPUI's `surface()` element.
//! OWNER: part 3 (frames).
//!
//! Lifetimes: `FrameView` holds exactly one frame (the newest). Replacing it drops the old
//! `Surface` and `CVPixelBuffer`; GPUI's scene keeps its own retain until the next scene, and the
//! Metal renderer keeps its CVMetalTextures until the GPU is done (HERDR: patch in
//! vendor/gpui-pre-apple). No image cache is involved, so herdr issue 22 (image cache leak) does
//! not apply here.
//!
//! Three acknowledgements, kept apart (Astra review C4):
//! - receipt: `on_frame` imported the surface (local only; nothing is sent).
//! - presentation: the frame was in a scene GPUI presented while the window was visible.
//!   `element()` only records that a frame went into the scene; BrowserView calls `presented`
//!   from `window.on_next_frame` (after the frame is drawn and presented) with
//!   `window.is_visible()`, and only then does `ack()` yield the seq for `AppToHost::FrameAck`,
//!   which paces the host (one frame in flight). A hidden or occluded window never acks.
//! - safe reuse: never signalled by message. The host pool skips any IOSurface whose kernel-wide
//!   use count is held (our CVPixelBuffer, GPUI's scene, Metal textures until GPU completion).
//! Hidden: `set_visible(false)` forgets undelivered acks; BrowserView tells the host, which
//! stops painting. `set_visible(true)` expects a fresh full frame from the host.
//!
//! Stats mode (`HERDR_BROWSER_STATS=1`, off by default): every view appends to
//! `<app home>/browser/stats.log` one `paint` line per paint of a new frame (interval since the
//! view's previous paint, frame arrival to paint latency, frames the host coalesced away before
//! this one) and a `summary` line every 2 s (paints/s, p50/p95 interval, p95 latency, dropped).

use gpui::{AnyElement, IntoElement, ObjectFit, Styled, surface};
use herdr_browser_protocol::{PixelSize, Size, SurfaceRef};
use herdr_browser_surface::{Surface, import};

/// What BrowserView feeds frames into (re-exported from `src/browser/mod.rs`).
pub trait FrameSink {
    /// A `HostToApp::Frame` arrived: import and show it from the next paint on.
    fn on_frame(&mut self, seq: u64, surface: SurfaceRef, size: PixelSize, scale: f32) -> anyhow::Result<()>;
    /// The block was resized to `dips`; keep stretching the last frame until a matching one arrives.
    fn on_resize_pending(&mut self, dips: Size);
    /// The seq to send as `AppToHost::FrameAck`, once that frame was presented visibly; taken once.
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
    arrived: std::time::Instant,
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
    /// Went into the latest scene; becomes `presented` once that scene is shown visibly.
    drawn: Option<u64>,
    presented: Option<u64>,
    hidden: bool,
    stats: Option<Stats>,
}

impl FrameView {
    pub fn new() -> Self {
        Self { stats: Stats::from_env(), ..Self::default() }
    }

    /// The element to put inside the block's bounds: the newest frame filling them (stretched
    /// while a resize is pending), or `None` before the first frame. Records it as drawn only.
    pub fn element(&mut self) -> Option<AnyElement> {
        #[cfg(target_os = "macos")]
        {
            let frame = self.frame.as_ref()?;
            self.drawn = Some(frame.seq);
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

    /// After GPUI presented the scene holding the last `element()`: with the window visible,
    /// that frame counts as presented (stats, and `ack()` yields it).
    pub fn presented(&mut self, window_visible: bool) {
        let Some(seq) = self.drawn.take() else { return };
        if !window_visible || self.hidden {
            return;
        }
        if let (Some(stats), Some(f)) = (&mut self.stats, self.frame.as_ref().filter(|f| f.seq == seq)) {
            stats.painted(seq, f.arrived);
        }
        self.presented = Some(seq);
    }

    /// The window (or the block's tab) became hidden or visible. Returns true on a change, when
    /// BrowserView sends `AppToHost::SetVisible`.
    pub fn set_visible(&mut self, visible: bool) -> bool {
        if self.hidden != visible {
            return false;
        }
        self.hidden = !visible;
        if self.hidden {
            self.drawn = None;
            self.presented = None;
        }
        true
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub fn seq(&self) -> Option<u64> {
        self.frame.as_ref().map(|f| f.seq)
    }

    /// Drop the frame (block closed or host crashed).
    pub fn clear(&mut self) {
        self.frame = None;
        self.drawn = None;
        self.presented = None;
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
            arrived: std::time::Instant::now(),
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
        self.presented.take()
    }
}

/// Per-view paint statistics for `HERDR_BROWSER_STATS=1` (see the module docs).
struct Stats {
    view: u32,
    out: std::fs::File,
    start: std::time::Instant,
    last_paint: Option<std::time::Instant>,
    last_seq: u64,
    window_start: std::time::Instant,
    intervals: Vec<f64>,
    latencies: Vec<f64>,
    dropped: u64,
}

impl Stats {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

    fn from_env() -> Option<Self> {
        if std::env::var_os("HERDR_BROWSER_STATS").is_none_or(|v| v.is_empty() || v == "0") {
            return None;
        }
        // Same lookup as `mods::app_dir` (this file is also built alone by tests/browser_frame.rs).
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
        let home = env("HERDR_APP_HOME").or_else(|| Some(env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?.join("herdr-app")))?;
        let dir = home.join("browser");
        std::fs::create_dir_all(&dir).ok()?;
        let out = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("stats.log")).ok()?;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        let now = std::time::Instant::now();
        let mut s = Stats { view: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), out, start: now, last_paint: None, last_seq: 0, window_start: now, intervals: Vec::new(), latencies: Vec::new(), dropped: 0 };
        s.line(format_args!("start pid={}", std::process::id()));
        Some(s)
    }

    fn line(&mut self, what: std::fmt::Arguments) {
        use std::io::Write;
        let _ = writeln!(self.out, "{:.1} view={} {what}", self.start.elapsed().as_secs_f64() * 1e3, self.view);
    }

    /// A paint of frame `seq` (counted once per seq; repaints of the same frame are ignored).
    fn painted(&mut self, seq: u64, arrived: std::time::Instant) {
        if seq == self.last_seq {
            return;
        }
        let now = std::time::Instant::now();
        let dropped = if self.last_seq == 0 { 0 } else { seq.saturating_sub(self.last_seq + 1) };
        let interval = self.last_paint.map(|t| (now - t).as_secs_f64() * 1e3);
        let latency = (now - arrived).as_secs_f64() * 1e3;
        self.line(format_args!("paint seq={seq} interval_ms={} latency_ms={latency:.2} dropped={dropped}", interval.map_or("-".into(), |i| format!("{i:.2}"))));
        if let Some(i) = interval {
            self.intervals.push(i);
        }
        self.latencies.push(latency);
        self.dropped += dropped;
        self.last_seq = seq;
        self.last_paint = Some(now);
        let span = now - self.window_start;
        if span >= Self::WINDOW {
            let pct = |v: &mut Vec<f64>, p: f64| {
                v.sort_by(f64::total_cmp);
                v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
            };
            let fps = self.latencies.len() as f64 / span.as_secs_f64();
            let (mut iv, mut lat) = (std::mem::take(&mut self.intervals), std::mem::take(&mut self.latencies));
            let (p50, p95, l95) = (pct(&mut iv, 0.5), pct(&mut iv, 0.95), pct(&mut lat, 0.95));
            let dropped = std::mem::take(&mut self.dropped);
            self.line(format_args!("summary paints_per_s={fps:.1} interval_p50_ms={p50:.2} interval_p95_ms={p95:.2} latency_p95_ms={l95:.2} dropped={dropped}"));
            self.window_start = now;
        }
    }
}
