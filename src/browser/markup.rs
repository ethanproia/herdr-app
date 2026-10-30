//! Design slice S9 (part 4): markup over the page or a screenshot region. Pen, rectangle, ellipse
//! and arrow, with undo and redo, drawn by the app (a GPUI canvas over the page) and burned into a
//! PNG for the agent. Reference style: red #ff3b30, 3 px wide.
//!
//! Input comes only from `BrowserInput` while its owner is `Owner::Markup` (docs/browser-input-owner.md):
//! `BrowserView` drains `input.take_overlay()` and hands each event to `Markup::on_event`.
//! Shapes are kept in DIPs from the page's (or the crop's) top-left; `burn` scales them to pixels.

use super::input::{Owner, OverlayEvent};
use super::protocol::{EditCommand, MouseKind};
use gpui::{Bounds, PathBuilder, Pixels, Window, point, px, rgb};
use image::{Rgba, RgbaImage};

pub const RED: [u8; 4] = [0xff, 0x3b, 0x30, 0xff];
pub const WIDTH: f32 = 3.0;
/// Arrow head: length in DIPs and half-angle.
const HEAD: f32 = 12.0;
const HEAD_ANGLE: f32 = std::f32::consts::PI / 6.0;
/// Rectangles, ellipses and arrows smaller than this (a click, not a drag) are dropped.
const MIN_DRAG: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Rect,
    Ellipse,
    Arrow,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Pen(Vec<(f32, f32)>),
    Rect((f32, f32), (f32, f32)),
    Ellipse((f32, f32), (f32, f32)),
    Arrow((f32, f32), (f32, f32)),
}

impl Shape {
    fn start(tool: Tool, p: (f32, f32)) -> Self {
        match tool {
            Tool::Pen => Shape::Pen(vec![p]),
            Tool::Rect => Shape::Rect(p, p),
            Tool::Ellipse => Shape::Ellipse(p, p),
            Tool::Arrow => Shape::Arrow(p, p),
        }
    }

    fn drag(&mut self, p: (f32, f32)) {
        match self {
            Shape::Pen(pts) => {
                if pts.last() != Some(&p) {
                    pts.push(p);
                }
            }
            Shape::Rect(_, b) | Shape::Ellipse(_, b) | Shape::Arrow(_, b) => *b = p,
        }
    }

    /// A drag, not a click.
    fn is_drawn(&self) -> bool {
        match self {
            Shape::Pen(pts) => pts.len() > 1,
            Shape::Rect(a, b) | Shape::Ellipse(a, b) => (a.0 - b.0).abs() >= MIN_DRAG && (a.1 - b.1).abs() >= MIN_DRAG,
            Shape::Arrow(a, b) => (a.0 - b.0).hypot(a.1 - b.1) >= MIN_DRAG,
        }
    }

    /// The shape as open polylines in DIPs (what both the canvas and the burn draw).
    pub fn polylines(&self) -> Vec<Vec<(f32, f32)>> {
        match self {
            Shape::Pen(pts) => vec![pts.clone()],
            Shape::Rect(a, b) => vec![vec![*a, (b.0, a.1), *b, (a.0, b.1), *a]],
            Shape::Ellipse(a, b) => {
                let (cx, cy, rx, ry) = ((a.0 + b.0) / 2., (a.1 + b.1) / 2., (a.0 - b.0).abs() / 2., (a.1 - b.1).abs() / 2.);
                let n = 96;
                vec![(0..=n).map(|i| {
                    let t = i as f32 / n as f32 * std::f32::consts::TAU;
                    (cx + rx * t.cos(), cy + ry * t.sin())
                }).collect()]
            }
            Shape::Arrow(a, b) => {
                let angle = (b.1 - a.1).atan2(b.0 - a.0);
                let wing = |side: f32| {
                    let t = angle + std::f32::consts::PI + side * HEAD_ANGLE;
                    (b.0 + HEAD * t.cos(), b.1 + HEAD * t.sin())
                };
                vec![vec![*a, *b], vec![wing(1.), *b, wing(-1.)]]
            }
        }
    }
}

/// The markup layer for one browser block.
#[derive(Debug, Clone)]
pub struct Markup {
    pub tool: Tool,
    shapes: Vec<Shape>,
    redo: Vec<Shape>,
    drawing: Option<Shape>,
}

impl Default for Markup {
    fn default() -> Self {
        Self { tool: Tool::Rect, shapes: Vec::new(), redo: Vec::new(), drawing: None }
    }
}

impl Markup {
    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }

    /// Feeds one event from `BrowserInput::take_overlay`; returns true when the drawing changed.
    /// Events for other owners are ignored.
    pub fn on_event(&mut self, ev: &OverlayEvent) -> bool {
        match ev {
            OverlayEvent::Pointer { owner: Owner::Markup, kind, x, y, .. } => match kind {
                MouseKind::Down => {
                    self.drawing = Some(Shape::start(self.tool, (*x, *y)));
                    true
                }
                MouseKind::Move => self.drawing.as_mut().map(|s| s.drag((*x, *y))).is_some(),
                MouseKind::Up => {
                    let Some(mut s) = self.drawing.take() else { return false };
                    s.drag((*x, *y));
                    if s.is_drawn() {
                        self.shapes.push(s);
                        self.redo.clear();
                    }
                    true
                }
                // Leaving the block ends the stroke where it is.
                MouseKind::Leave => {
                    let Some(s) = self.drawing.take() else { return false };
                    if s.is_drawn() {
                        self.shapes.push(s);
                        self.redo.clear();
                    }
                    true
                }
            },
            OverlayEvent::Edit { owner: Owner::Markup, command: EditCommand::Undo } => self.undo(),
            OverlayEvent::Edit { owner: Owner::Markup, command: EditCommand::Redo } => self.redo(),
            // Leaving markup (Esc, a lease) drops a stroke in progress.
            OverlayEvent::Owner(o) if *o != Owner::Markup => self.drawing.take().is_some(),
            _ => false,
        }
    }

    pub fn undo(&mut self) -> bool {
        self.shapes.pop().map(|s| self.redo.push(s)).is_some()
    }

    pub fn redo(&mut self) -> bool {
        self.redo.pop().map(|s| self.shapes.push(s)).is_some()
    }

    pub fn clear(&mut self) {
        self.shapes.clear();
        self.redo.clear();
        self.drawing = None;
    }

    fn visible(&self) -> impl Iterator<Item = &Shape> {
        self.shapes.iter().chain(self.drawing.iter())
    }

    /// A copy of `base` (the page or crop, `scale` pixels per DIP) with the finished shapes burned in.
    /// Solid pixels, no blending, so the result is exact: no shapes means `base` byte for byte.
    pub fn burn(&self, base: &RgbaImage, scale: f32) -> RgbaImage {
        let mut out = base.clone();
        let r = WIDTH * scale / 2.0;
        for shape in &self.shapes {
            for line in shape.polylines() {
                let pts: Vec<(f32, f32)> = line.iter().map(|(x, y)| (x * scale, y * scale)).collect();
                for w in pts.windows(2) {
                    stamp_segment(&mut out, w[0], w[1], r);
                }
                if pts.len() == 1 {
                    stamp_segment(&mut out, pts[0], pts[0], r);
                }
            }
        }
        out
    }

    /// Draws the shapes (and the stroke in progress) over the page; `origin` is the page's top-left.
    pub fn paint(&self, origin: gpui::Point<Pixels>, window: &mut Window) {
        for shape in self.visible() {
            for line in shape.polylines() {
                let mut b = PathBuilder::stroke(px(WIDTH));
                for (i, (x, y)) in line.iter().enumerate() {
                    let p = origin + point(px(*x), px(*y));
                    if i == 0 { b.move_to(p) } else { b.line_to(p) }
                }
                if let Ok(path) = b.build() {
                    window.paint_path(path, rgb(0xff3b30));
                }
            }
        }
    }
}

/// Every pixel whose centre is within `r` of segment a-b becomes `RED`.
fn stamp_segment(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), r: f32) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let x0 = (a.0.min(b.0) - r).floor().max(0.0) as i64;
    let y0 = (a.1.min(b.1) - r).floor().max(0.0) as i64;
    let x1 = ((a.0.max(b.0) + r).ceil() as i64).min(w - 1);
    let y1 = ((a.1.max(b.1) + r).ceil() as i64).min(h - 1);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (px_, py_) = (x as f32 + 0.5, y as f32 + 0.5);
            let t = if len2 == 0.0 { 0.0 } else { (((px_ - a.0) * dx + (py_ - a.1) * dy) / len2).clamp(0.0, 1.0) };
            let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
            if (px_ - qx).hypot(py_ - qy) <= r {
                img.put_pixel(x as u32, y as u32, Rgba(RED));
            }
        }
    }
}

/// The burned picture as PNG bytes, for the agent payload.
pub fn png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).expect("encoding a PNG in memory");
    out.into_inner()
}

/// The canvas element the view puts over the page while markup is shown (absolute, full size).
pub fn element(markup: gpui::Entity<Markup>) -> impl gpui::IntoElement {
    use gpui::Styled as _;
    gpui::canvas(|_, _, _| (), move |bounds: Bounds<Pixels>, _, window, cx| markup.read(cx).paint(bounds.origin, window)).absolute().size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::HostLink;
    use crate::browser::input::BrowserInput;
    use crate::browser::protocol::AppToHost;
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
    use std::sync::{Arc, Mutex};

    /// A patterned fixture (8 px checker of two colours, plus a gradient) so a wrong pixel cannot
    /// hide in a flat background.
    fn fixture(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let check = ((x / 8) + (y / 8)) % 2 == 0;
            Rgba([if check { 0x20 } else { 0xe0 }, (x % 256) as u8, (y % 256) as u8, 0xff])
        })
    }

    fn red_count(img: &RgbaImage) -> usize {
        img.pixels().filter(|p| p.0 == RED).count()
    }

    fn changed(a: &RgbaImage, b: &RgbaImage) -> usize {
        a.pixels().zip(b.pixels()).filter(|(p, q)| p != q).count()
    }

    fn draw(m: &mut Markup, tool: Tool, from: (f32, f32), to: (f32, f32)) {
        m.tool = tool;
        let ev = |kind, (x, y): (f32, f32)| OverlayEvent::Pointer { owner: Owner::Markup, kind, x, y, shift: false };
        m.on_event(&ev(MouseKind::Down, from));
        m.on_event(&ev(MouseKind::Move, ((from.0 + to.0) / 2., (from.1 + to.1) / 2.)));
        m.on_event(&ev(MouseKind::Up, to));
    }

    #[test]
    fn rect_then_undo_gives_back_the_unmarked_crop_byte_for_byte() {
        let base = fixture(240, 160);
        let clean = png(&base);
        let mut m = Markup::default();
        assert_eq!(png(&m.burn(&base, 2.0)), clean, "no shapes: the crop as it was");
        draw(&mut m, Tool::Rect, (10., 10.), (100., 60.));
        assert_ne!(png(&m.burn(&base, 2.0)), clean);
        m.on_event(&OverlayEvent::Edit { owner: Owner::Markup, command: EditCommand::Undo });
        assert_eq!(png(&m.burn(&base, 2.0)), clean, "after undo: byte for byte");
        m.on_event(&OverlayEvent::Edit { owner: Owner::Markup, command: EditCommand::Redo });
        assert_eq!(m.shapes().len(), 1, "redo brings it back");
    }

    #[test]
    fn rect_red_pixels_match_the_geometry_and_nothing_else_changes() {
        let base = fixture(200, 120);
        let mut m = Markup::default();
        // On pixel centres, so the 3 px band covers whole pixels: outer 103x63 minus inner 97x57 = 960.
        draw(&mut m, Tool::Rect, (20.5, 20.5), (120.5, 80.5));
        let out = m.burn(&base, 1.0);
        let (got, want) = (red_count(&out) as f32, 960.0);
        assert!((got - want).abs() / want < 0.05, "red pixels {got}, geometry {want}");
        assert_eq!(changed(&base, &out), red_count(&out), "only the red band changed");
        // Negative regions: the inside and the outside away from the band are untouched.
        for (x, y) in [(70, 50), (24, 24), (116, 76), (5, 5), (150, 100), (121 + 3, 50)] {
            assert_eq!(out.get_pixel(x, y), base.get_pixel(x, y), "({x},{y}) untouched");
        }
        for (x, y) in [(20, 50), (120, 50), (70, 20), (70, 80)] {
            assert_eq!(out.get_pixel(x, y).0, RED, "({x},{y}) on the outline");
        }
        // At 2x the band is twice as wide and twice as long: four times the pixels.
        let big = m.burn(&fixture(400, 240), 2.0);
        let ratio = red_count(&big) as f32 / got;
        assert!((ratio - 4.0).abs() < 0.2, "2x scale ratio {ratio}");
    }

    #[test]
    fn ellipse_arrow_and_pen_draw_where_they_should() {
        let base = fixture(200, 200);
        let mut m = Markup::default();
        draw(&mut m, Tool::Ellipse, (20., 20.), (120., 80.));
        let out = m.burn(&base, 1.0);
        // Ramanujan's perimeter of a 50x30 ellipse times the 3 px width.
        let (a, b) = (50.0f32, 30.0f32);
        let perim = std::f32::consts::PI * (3. * (a + b) - ((3. * a + b) * (a + 3. * b)).sqrt());
        let got = red_count(&out) as f32;
        assert!((got - perim * 3.).abs() / (perim * 3.) < 0.08, "ellipse red {got}, geometry {}", perim * 3.);
        assert_eq!(out.get_pixel(70, 50), base.get_pixel(70, 50), "centre untouched");
        assert_eq!(out.get_pixel(22, 22), base.get_pixel(22, 22), "bounding-box corner untouched");
        assert_eq!(out.get_pixel(20, 50).0, RED);
        m.clear();
        draw(&mut m, Tool::Arrow, (20., 150.), (120., 150.));
        let out = m.burn(&base, 1.0);
        assert_eq!(out.get_pixel(70, 150).0, RED, "shaft");
        // Head wings end about 12 px back and 6 px to each side of the tip.
        assert_eq!(out.get_pixel(110, 144).0, RED, "upper wing");
        assert_eq!(out.get_pixel(110, 156).0, RED, "lower wing");
        assert_eq!(out.get_pixel(30, 144), base.get_pixel(30, 144), "no head at the tail");
        m.clear();
        m.tool = Tool::Pen;
        let ev = |kind, x, y| OverlayEvent::Pointer { owner: Owner::Markup, kind, x, y, shift: false };
        m.on_event(&ev(MouseKind::Down, 10., 10.));
        for i in 1..=10 {
            m.on_event(&ev(MouseKind::Move, 10. + i as f32 * 5., 10. + i as f32 * 5.));
        }
        m.on_event(&ev(MouseKind::Up, 60., 60.));
        let out = m.burn(&base, 1.0);
        assert_eq!(out.get_pixel(35, 35).0, RED, "on the stroke");
        assert_eq!(out.get_pixel(35, 10), base.get_pixel(35, 10), "off the stroke");
    }

    #[test]
    fn clicks_and_other_owners_draw_nothing() {
        let mut m = Markup::default();
        draw(&mut m, Tool::Rect, (10., 10.), (11., 11.));
        assert!(m.shapes().is_empty(), "a click is not a rectangle");
        m.on_event(&OverlayEvent::Pointer { owner: Owner::Pick, kind: MouseKind::Down, x: 0., y: 0., shift: false });
        m.on_event(&OverlayEvent::Pointer { owner: Owner::Pick, kind: MouseKind::Up, x: 50., y: 50., shift: false });
        assert!(m.shapes().is_empty(), "pick events are not markup");
        m.on_event(&OverlayEvent::Pointer { owner: Owner::Markup, kind: MouseKind::Down, x: 0., y: 0., shift: false });
        m.on_event(&OverlayEvent::Owner(Owner::Browse));
        m.on_event(&OverlayEvent::Pointer { owner: Owner::Markup, kind: MouseKind::Up, x: 50., y: 50., shift: false });
        assert!(m.shapes().is_empty(), "Esc mid-stroke drops it");
    }

    #[derive(Default)]
    struct Recording(Mutex<Vec<AppToHost>>);
    impl HostLink for Recording {
        fn send(&self, msg: AppToHost) {
            self.0.lock().unwrap().push(msg);
        }
    }

    /// Through `BrowserInput` as the view drives it: in Markup the pointer draws and never reaches
    /// the page; cmd-z undoes the stroke; back in Browse the same drag goes to the page.
    #[test]
    fn markup_is_fed_only_by_browser_input_in_markup_mode() {
        let rec = Arc::new(Recording::default());
        let mut input = BrowserInput::new("b1".into(), rec.clone());
        let mut m = Markup::default();
        let at = |x: f32, y: f32| point(px(x), px(y));
        let drag = |input: &mut BrowserInput| {
            input.mouse_down(&MouseDownEvent { button: MouseButton::Left, position: at(10., 10.), modifiers: Modifiers::default(), click_count: 1, first_mouse: false });
            input.mouse_move(&MouseMoveEvent { position: at(60., 40.), pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() });
            input.mouse_up(&MouseUpEvent { button: MouseButton::Left, position: at(110., 70.), modifiers: Modifiers::default(), click_count: 1 });
        };
        assert!(input.enter(Owner::Markup));
        drag(&mut input);
        for ev in input.take_overlay() {
            m.on_event(&ev);
        }
        assert_eq!(m.shapes(), &[Shape::Rect((10., 10.), (110., 70.))]);
        assert!(rec.0.lock().unwrap().is_empty(), "the page saw none of the drawing");
        let undo = gpui::Keystroke::parse(if cfg!(target_os = "macos") { "cmd-z" } else { "ctrl-z" }).unwrap();
        input.key_down(&gpui::KeyDownEvent { keystroke: undo, is_held: false, prefer_character_input: false });
        for ev in input.take_overlay() {
            m.on_event(&ev);
        }
        assert!(m.shapes().is_empty(), "cmd-z in markup undoes the stroke");
        assert!(input.escape());
        drag(&mut input);
        for ev in input.take_overlay() {
            m.on_event(&ev);
        }
        assert!(m.shapes().is_empty(), "browse drags never draw");
        assert_eq!(rec.0.lock().unwrap().len(), 3, "the page got the browse drag (down, move, up)");
    }
}
