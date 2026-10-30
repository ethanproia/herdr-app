//! The geometry contract between page, block and window (finding C2). Every overlay the app
//! draws over a page (element highlight, pick, comment pins, find matches, the IME caret) goes
//! through these transforms; nothing else converts coordinates. docs/browser-geometry.md.
//!
//! Spaces, outermost last:
//! - **frame CSS px**: what `getBoundingClientRect()` says inside one (i)frame.
//! - **viewport CSS px**: the top frame's CSS px, iframe offsets added (each offset is the iframe
//!   content box's top-left in its parent's CSS px, so offsets simply sum).
//! - **page DIPs**: from the page area's top-left (under the bar); `css * zoom` where zoom is the
//!   page zoom factor (Chrome level 0 = 1.0, each level x1.2). Input events use this space.
//! - **window points**: GPUI's space; page DIPs + the page area's origin in the window.
//! - **backing pixels**: window points x the scale factor (2 on Retina). Frames are in this
//!   space relative to the page area: a frame is `page DIPs size * scale` pixels.
//! The page's own `devicePixelRatio` is `zoom * scale`.
//!
//! Overlay answers race the page: the app numbers every change that moves content (resize,
//! scroll, zoom, navigation) with an **epoch**. Requests carry the epoch they were made in;
//! a response from an older epoch is dropped, and a selection drawn from an older epoch is hidden
//! until a fresh answer comes. Hover requests are coalesced: one in flight, newest point waits.
#![allow(dead_code)]

/// A point.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

/// An axis-aligned rectangle (origin top-left).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    fn map(self, f: impl Fn(Pt) -> Pt) -> Rect {
        let a = f(Pt { x: self.x, y: self.y });
        let b = f(Pt { x: self.x + self.w, y: self.y + self.h });
        Rect { x: a.x, y: a.y, w: b.x - a.x, h: b.y - a.y }
    }
    /// The part inside `clip` (zero-sized when outside).
    pub fn clip(self, clip: Rect) -> Rect {
        let x0 = self.x.max(clip.x);
        let y0 = self.y.max(clip.y);
        let x1 = (self.x + self.w).min(clip.x + clip.w);
        let y1 = (self.y + self.h).min(clip.y + clip.h);
        Rect { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
    }
    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// Chrome's zoom level to a factor: 0 = 100 %, each level is x1.2 (so 1 = 120 %, -1 = 83.3 %).
pub fn zoom_factor(level: f64) -> f32 {
    1.2f64.powf(level) as f32
}

/// How one block maps: page zoom, the page area's origin in the window, the scale factor and
/// the page area's size (DIPs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Page zoom factor (`zoom_factor(level)`).
    pub zoom: f32,
    /// The page area's top-left in window points.
    pub origin: Pt,
    /// Backing scale factor (window's).
    pub scale: f32,
    /// The page area's size in DIPs.
    pub size: Pt,
}

impl Geometry {
    /// Frame CSS px (inside the iframes `frames`, outermost first) to viewport CSS px.
    pub fn frame_to_viewport(p: Pt, frames: &[Pt]) -> Pt {
        frames.iter().fold(p, |p, o| Pt { x: p.x + o.x, y: p.y + o.y })
    }

    pub fn viewport_to_page(&self, p: Pt) -> Pt {
        Pt { x: p.x * self.zoom, y: p.y * self.zoom }
    }

    pub fn page_to_viewport(&self, p: Pt) -> Pt {
        Pt { x: p.x / self.zoom, y: p.y / self.zoom }
    }

    pub fn page_to_window(&self, p: Pt) -> Pt {
        Pt { x: p.x + self.origin.x, y: p.y + self.origin.y }
    }

    pub fn window_to_page(&self, p: Pt) -> Pt {
        Pt { x: p.x - self.origin.x, y: p.y - self.origin.y }
    }

    /// Window points to backing pixels of the window.
    pub fn window_to_backing(&self, p: Pt) -> Pt {
        Pt { x: p.x * self.scale, y: p.y * self.scale }
    }

    /// Page DIPs to pixels of a frame (relative to the page area).
    pub fn page_to_frame_px(&self, p: Pt) -> Pt {
        Pt { x: p.x * self.scale, y: p.y * self.scale }
    }

    /// The page's `devicePixelRatio`.
    pub fn device_pixel_ratio(&self) -> f32 {
        self.zoom * self.scale
    }

    /// A rect the page reported (frame CSS px, inside `frames`) to window points, clipped to the
    /// page area. Empty when it's scrolled out of view.
    pub fn css_rect_to_window(&self, r: Rect, frames: &[Pt]) -> Rect {
        let page = r.map(|p| self.viewport_to_page(Self::frame_to_viewport(p, frames)));
        let clipped = page.clip(Rect::new(0.0, 0.0, self.size.x, self.size.y));
        clipped.map(|p| self.page_to_window(p))
    }

    /// A window point (a click, a hover) to viewport CSS px for a hit test in the page.
    pub fn window_to_css(&self, p: Pt) -> Pt {
        self.page_to_viewport(self.window_to_page(p))
    }

    /// Whether a window point is over the page area.
    pub fn contains(&self, p: Pt) -> bool {
        let q = self.window_to_page(p);
        q.x >= 0.0 && q.y >= 0.0 && q.x < self.size.x && q.y < self.size.y
    }
}

/// Numbers content-moving changes. The app bumps it on resize, scroll, zoom and navigation and
/// stamps every overlay request with the current value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch(pub u64);

/// Why the epoch moved (kept for logs and tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Resize,
    Scroll,
    Zoom,
    Navigate,
}

/// An overlay answer from the host: rects in frame CSS px, for the request made at `epoch`.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayAnswer {
    pub req: u64,
    pub epoch: Epoch,
    pub rects: Vec<Rect>,
    /// Enclosing iframes' offsets, outermost first (empty for the top frame).
    pub frames: Vec<Pt>,
}

/// The overlay the app shows (a highlight, a selection), with the epoch it belongs to.
#[derive(Default)]
pub struct OverlayState {
    epoch: Epoch,
    shown: Option<(Epoch, Vec<Rect>)>,
    next_req: u64,
    /// The hover request in flight, and the newest point waiting behind it.
    hover_in_flight: Option<u64>,
    hover_waiting: Option<Pt>,
}

/// What `OverlayState::hover` says to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HoverSend {
    /// Send a hit test for this point now, with this request id and epoch.
    Now { req: u64, epoch: Epoch, at: Pt },
    /// One is in flight; this point waits (replacing an older waiting one).
    Coalesced,
}

impl OverlayState {
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Content moved: older answers are stale from now on, and what's shown is hidden.
    pub fn changed(&mut self, _why: Change) -> Epoch {
        self.epoch.0 += 1;
        self.shown = None;
        self.epoch
    }

    /// A new request id stamped with the current epoch.
    pub fn request(&mut self) -> (u64, Epoch) {
        self.next_req += 1;
        (self.next_req, self.epoch)
    }

    /// An answer arrived. True when it was taken (current epoch); stale ones are dropped.
    pub fn answer(&mut self, a: &OverlayAnswer, geo: &Geometry) -> bool {
        if self.hover_in_flight == Some(a.req) {
            self.hover_in_flight = None;
        }
        if a.epoch != self.epoch {
            return false;
        }
        let rects = a.rects.iter().map(|r| geo.css_rect_to_window(*r, &a.frames)).filter(|r| !r.is_empty()).collect();
        self.shown = Some((a.epoch, rects));
        true
    }

    /// What to draw now: only rects from the current epoch.
    pub fn shown(&self) -> &[Rect] {
        match &self.shown {
            Some((e, r)) if *e == self.epoch => r,
            _ => &[],
        }
    }

    /// The pointer moved to `at` (viewport CSS px): send a hit test, or wait behind the one in flight.
    pub fn hover(&mut self, at: Pt) -> HoverSend {
        if self.hover_in_flight.is_some() {
            self.hover_waiting = Some(at);
            return HoverSend::Coalesced;
        }
        let (req, epoch) = self.request();
        self.hover_in_flight = Some(req);
        HoverSend::Now { req, epoch, at }
    }

    /// After an answer (or a timeout): the newest waiting point, if any, to send now.
    pub fn next_hover(&mut self) -> Option<HoverSend> {
        if self.hover_in_flight.is_some() {
            return None;
        }
        self.hover_waiting.take().map(|at| self.hover(at))
    }

    /// The in-flight hover never got an answer (host restart): let the next one go.
    pub fn hover_lost(&mut self) {
        self.hover_in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(zoom_level: f64, scale: f32) -> Geometry {
        Geometry { zoom: zoom_factor(zoom_level), origin: Pt { x: 120.0, y: 28.0 }, scale, size: Pt { x: 800.0, y: 600.0 } }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn zoom_levels_are_chromes() {
        assert!(close(zoom_factor(0.0), 1.0));
        assert!(close(zoom_factor(1.0), 1.2));
        assert!(close(zoom_factor(-1.0), 1.0 / 1.2));
        assert!(close(zoom_factor(2.0), 1.44));
    }

    #[test]
    fn a_css_rect_lands_in_window_points_through_zoom_and_origin() {
        let g = geo(1.0, 2.0); // 120 %, Retina
        let r = g.css_rect_to_window(Rect::new(10.0, 20.0, 100.0, 50.0), &[]);
        // css * 1.2, then + origin (120, 28)
        assert!(close(r.x, 132.0) && close(r.y, 52.0) && close(r.w, 120.0) && close(r.h, 60.0), "{r:?}");
        assert!(close(g.device_pixel_ratio(), 2.4));
        // Backing pixels are window points x scale.
        let b = g.window_to_backing(Pt { x: r.x, y: r.y });
        assert!(close(b.x, 264.0) && close(b.y, 104.0));
        // And a frame pixel of the same corner: page DIPs x scale.
        let f = g.page_to_frame_px(g.viewport_to_page(Pt { x: 10.0, y: 20.0 }));
        assert!(close(f.x, 24.0) && close(f.y, 48.0));
    }

    #[test]
    fn iframe_offsets_sum_outermost_first() {
        let g = geo(0.0, 1.0);
        // A rect at (5, 5) inside an iframe at (100, 200) inside another at (10, 20).
        let r = g.css_rect_to_window(Rect::new(5.0, 5.0, 10.0, 10.0), &[Pt { x: 10.0, y: 20.0 }, Pt { x: 100.0, y: 200.0 }]);
        assert!(close(r.x, 120.0 + 115.0) && close(r.y, 28.0 + 225.0), "{r:?}");
    }

    #[test]
    fn rects_clip_to_the_page_area_and_vanish_when_scrolled_away() {
        let g = geo(0.0, 2.0);
        let r = g.css_rect_to_window(Rect::new(780.0, -10.0, 100.0, 30.0), &[]);
        assert!(close(r.x, 900.0) && close(r.w, 20.0) && close(r.y, 28.0) && close(r.h, 20.0), "{r:?}");
        assert!(g.css_rect_to_window(Rect::new(0.0, 700.0, 10.0, 10.0), &[]).is_empty());
    }

    #[test]
    fn window_points_go_back_to_css_for_hit_tests() {
        let g = geo(-1.0, 2.0); // 83 %
        let css = g.window_to_css(Pt { x: 120.0 + 100.0, y: 28.0 + 50.0 });
        assert!(close(css.x, 120.0) && close(css.y, 60.0), "{css:?}");
        // Round trip.
        let back = g.page_to_window(g.viewport_to_page(css));
        assert!(close(back.x, 220.0) && close(back.y, 78.0));
        assert!(g.contains(Pt { x: 121.0, y: 29.0 }) && !g.contains(Pt { x: 119.0, y: 29.0 }) && !g.contains(Pt { x: 921.0, y: 100.0 }));
    }

    #[test]
    fn stale_answers_are_dropped_and_a_stale_selection_is_hidden() {
        let g = geo(0.0, 2.0);
        let mut o = OverlayState::default();
        let (req, epoch) = o.request();
        let a = OverlayAnswer { req, epoch, rects: vec![Rect::new(0.0, 0.0, 10.0, 10.0)], frames: vec![] };
        assert!(o.answer(&a, &g));
        assert_eq!(o.shown().len(), 1);
        // The page scrolls: the shown rect hides at once, and the old answer can't come back.
        o.changed(Change::Scroll);
        assert!(o.shown().is_empty(), "a stale selection is hidden");
        assert!(!o.answer(&a, &g), "a stale answer is dropped");
        assert!(o.shown().is_empty());
        // A fresh request in the new epoch shows again.
        let (req, epoch) = o.request();
        assert!(o.answer(&OverlayAnswer { req, epoch, ..a.clone() }, &g));
        assert_eq!(o.shown().len(), 1);
        for why in [Change::Resize, Change::Zoom, Change::Navigate] {
            let e = o.epoch();
            assert!(o.changed(why) > e);
            assert!(o.shown().is_empty());
        }
    }

    #[test]
    fn hover_requests_coalesce_to_the_newest_point() {
        let g = geo(0.0, 1.0);
        let mut o = OverlayState::default();
        let HoverSend::Now { req, epoch, at } = o.hover(Pt { x: 1.0, y: 1.0 }) else { panic!() };
        assert_eq!(at, Pt { x: 1.0, y: 1.0 });
        // Three moves while it's in flight: only the last will be sent.
        for i in 2..5 {
            assert_eq!(o.hover(Pt { x: i as f32, y: 1.0 }), HoverSend::Coalesced);
        }
        assert_eq!(o.next_hover(), None, "nothing goes while one is in flight");
        o.answer(&OverlayAnswer { req, epoch, rects: vec![], frames: vec![] }, &g);
        let Some(HoverSend::Now { at, .. }) = o.next_hover() else { panic!() };
        assert_eq!(at, Pt { x: 4.0, y: 1.0 });
        assert_eq!(o.next_hover(), None);
        // A lost answer (host restart) doesn't block hovering forever.
        o.hover_lost();
        assert!(matches!(o.hover(Pt { x: 9.0, y: 9.0 }), HoverSend::Now { .. }));
    }
}
