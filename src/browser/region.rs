//! Design slice S6 (part 4), app side: in pick mode a drag draws a region instead of picking an
//! element. The region is kept relative to an anchor (the element under its top-left corner, or
//! the document when there is none) so it stays on the same content after scrolling, and the
//! page screenshot is cropped to it for the agent.
//!
//! `BrowserView::drain_overlay` hands Pick pointer events to `RegionDrag::on_event` first: a
//! `Click` is the existing pick; `Region` is a new region. Coordinates are page DIPs (viewport).

use super::input::{Owner, OverlayEvent};
use super::protocol::MouseKind;
use image::RgbaImage;

/// Below this (in DIPs, on either side) a drag is a click (the spec's 3x3 minimum).
pub const MIN: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn spanning(a: (f32, f32), b: (f32, f32)) -> Self {
        Rect { x: a.0.min(b.0), y: a.1.min(b.1), w: (a.0 - b.0).abs(), h: (a.1 - b.1).abs() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing for the view to do.
    None,
    /// A drag in progress: draw this rubber band.
    Dragging(Rect),
    /// A press and release without a drag: pick the element at (x, y).
    Click(f32, f32),
    /// A finished region (viewport DIPs at release time).
    Region(Rect),
}

/// Tells a pick click from a region drag.
#[derive(Debug, Default, Clone)]
pub struct RegionDrag {
    start: Option<(f32, f32)>,
    dragged: bool,
}

impl RegionDrag {
    pub fn on_event(&mut self, ev: &OverlayEvent) -> Outcome {
        let OverlayEvent::Pointer { owner: Owner::Pick, kind, x, y, .. } = ev else {
            if matches!(ev, OverlayEvent::Owner(o) if *o != Owner::Pick) {
                *self = Self::default();
            }
            return Outcome::None;
        };
        let p = (*x, *y);
        match kind {
            MouseKind::Down => {
                *self = RegionDrag { start: Some(p), dragged: false };
                Outcome::None
            }
            MouseKind::Move => {
                let Some(s) = self.start else { return Outcome::None };
                let r = Rect::spanning(s, p);
                self.dragged |= r.w >= MIN || r.h >= MIN;
                if self.dragged { Outcome::Dragging(r) } else { Outcome::None }
            }
            MouseKind::Up => {
                let Some(s) = self.start.take() else { return Outcome::None };
                let r = Rect::spanning(s, p);
                let dragged = std::mem::take(&mut self.dragged) || r.w >= MIN || r.h >= MIN;
                match dragged {
                    // A drag that ends thinner than the minimum on a side is dropped, not a click.
                    true if r.w >= MIN && r.h >= MIN => Outcome::Region(r),
                    true => Outcome::None,
                    false => Outcome::Click(p.0, p.1),
                }
            }
            MouseKind::Leave => {
                *self = Self::default();
                Outcome::None
            }
        }
    }
}

/// A region kept on the content it was drawn over.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The anchor element's selector, if one was under the region's corner.
    pub anchor: Option<String>,
    /// Offset of the region from the anchor's top-left (or from the document origin), DIPs.
    pub offset: (f32, f32),
    pub size: (f32, f32),
}

impl Region {
    /// `rect` in viewport DIPs; `anchor_rect` the anchor's viewport rect at the same moment
    /// (None: anchor to the document, using `scroll`).
    pub fn new(rect: Rect, anchor: Option<(String, Rect)>, scroll: (f32, f32)) -> Self {
        let (anchor, origin) = match anchor {
            Some((sel, a)) => (Some(sel), (a.x, a.y)),
            None => (None, (-scroll.0, -scroll.1)),
        };
        Region { anchor, offset: (rect.x - origin.0, rect.y - origin.1), size: (rect.w, rect.h) }
    }

    /// Where the region is now, in viewport DIPs: from the anchor's current rect, or the scroll.
    pub fn resolve(&self, anchor_rect: Option<Rect>, scroll: (f32, f32)) -> Option<Rect> {
        let origin = match (&self.anchor, anchor_rect) {
            (Some(_), Some(a)) => (a.x, a.y),
            // The anchor is gone: the region is detached (the caller shows it so).
            (Some(_), None) => return None,
            (None, _) => (-scroll.0, -scroll.1),
        };
        Some(Rect { x: origin.0 + self.offset.0, y: origin.1 + self.offset.1, w: self.size.0, h: self.size.1 })
    }
}

/// The region's pixels from a viewport screenshot taken at `scale` pixels per DIP. Edges round
/// outward, clamped to the image; None when the region is off the picture.
pub fn crop(page: &RgbaImage, r: Rect, scale: f32) -> Option<RgbaImage> {
    let x0 = ((r.x * scale).floor().max(0.0) as u32).min(page.width());
    let y0 = ((r.y * scale).floor().max(0.0) as u32).min(page.height());
    let x1 = (((r.x + r.w) * scale).ceil().max(0.0) as u32).min(page.width());
    let y1 = (((r.y + r.h) * scale).ceil().max(0.0) as u32).min(page.height());
    (x1 > x0 && y1 > y0).then(|| image::imageops::crop_imm(page, x0, y0, x1 - x0, y1 - y0).to_image())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn ev(kind: MouseKind, x: f32, y: f32) -> OverlayEvent {
        OverlayEvent::Pointer { owner: Owner::Pick, kind, x, y, shift: false }
    }

    #[test]
    fn a_click_still_picks_and_a_drag_makes_a_region() {
        let mut d = RegionDrag::default();
        d.on_event(&ev(MouseKind::Down, 50., 50.));
        assert_eq!(d.on_event(&ev(MouseKind::Move, 51., 51.)), Outcome::None, "jitter is not a drag");
        assert_eq!(d.on_event(&ev(MouseKind::Up, 51., 51.)), Outcome::Click(51., 51.));
        d.on_event(&ev(MouseKind::Down, 10., 20.));
        assert_eq!(d.on_event(&ev(MouseKind::Move, 60., 50.)), Outcome::Dragging(Rect { x: 10., y: 20., w: 50., h: 30. }));
        assert_eq!(d.on_event(&ev(MouseKind::Up, 110., 80.)), Outcome::Region(Rect { x: 10., y: 20., w: 100., h: 60. }));
        // Dragged right-to-left and up: the same rect.
        d.on_event(&ev(MouseKind::Down, 110., 80.));
        d.on_event(&ev(MouseKind::Move, 50., 50.));
        assert_eq!(d.on_event(&ev(MouseKind::Up, 10., 20.)), Outcome::Region(Rect { x: 10., y: 20., w: 100., h: 60. }));
        // Below the 3x3 minimum on one side: dropped, and not a pick either.
        d.on_event(&ev(MouseKind::Down, 10., 20.));
        d.on_event(&ev(MouseKind::Move, 80., 21.));
        assert_eq!(d.on_event(&ev(MouseKind::Up, 80., 21.)), Outcome::None);
        // Leaving pick mid-drag forgets it; markup events are not regions.
        d.on_event(&ev(MouseKind::Down, 0., 0.));
        d.on_event(&OverlayEvent::Owner(Owner::Browse));
        assert_eq!(d.on_event(&ev(MouseKind::Up, 90., 90.)), Outcome::None);
        assert_eq!(d.on_event(&OverlayEvent::Pointer { owner: Owner::Markup, kind: MouseKind::Down, x: 0., y: 0., shift: false }), Outcome::None);
    }

    #[test]
    fn a_region_stays_on_its_content_after_scrolling() {
        let drawn = Rect { x: 40., y: 300., w: 120., h: 80. };
        // Document-anchored: scrolled 200 px further, the region is 200 px higher in the viewport.
        let doc = Region::new(drawn, None, (0., 100.));
        assert_eq!(doc.resolve(None, (0., 300.)), Some(Rect { x: 40., y: 100., w: 120., h: 80. }));
        // Element-anchored: it follows the anchor's rect, wherever layout moved it.
        let card = Rect { x: 20., y: 280., w: 300., h: 200. };
        let el = Region::new(drawn, Some(("#card".into(), card)), (0., 0.));
        assert_eq!(el.offset, (20., 20.));
        assert_eq!(el.resolve(Some(Rect { x: 20., y: 80., ..card }), (0., 200.)), Some(Rect { x: 40., y: 100., w: 120., h: 80. }));
        assert_eq!(el.resolve(None, (0., 200.)), None, "anchor gone: detached");
    }

    #[test]
    fn the_crop_is_the_region_times_the_scale() {
        // A patterned page: every pixel encodes its own position, so a shifted crop is caught.
        let page = RgbaImage::from_fn(800, 600, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, (x / 256 + 4 * (y / 256)) as u8, 255]));
        let r = Rect { x: 10., y: 20., w: 100.5, h: 60. };
        let c = crop(&page, r, 2.0).unwrap();
        assert!((c.width() as f32 - r.w * 2.0).abs() <= 1.0 && (c.height() as f32 - r.h * 2.0).abs() <= 1.0, "{}x{}", c.width(), c.height());
        assert_eq!(c.get_pixel(0, 0), page.get_pixel(20, 40), "top-left is the region's corner");
        assert_eq!(c.get_pixel(c.width() - 1, c.height() - 1), page.get_pixel(20 + c.width() - 1, 40 + c.height() - 1));
        // Clamped at the picture's edge; off the picture is no crop.
        let edge = crop(&page, Rect { x: 350., y: 250., w: 100., h: 100. }, 2.0).unwrap();
        assert_eq!((edge.width(), edge.height()), (100, 100));
        assert!(crop(&page, Rect { x: 500., y: 0., w: 10., h: 10. }, 2.0).is_none());
    }
}
