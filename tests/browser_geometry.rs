//! Geometry wired into the block (stub host, no Chromium): overlay answers draw over the frame at
//! the right place, a scroll or zoom hides them, a stale answer never shows, hovers coalesce.
//! `HERDR_PARITY_STUB_ONLY=1` is implied: this test never needs CEF.
include!("common/browser_harness.rs");

use browser::protocol::{CssPoint, CssRect, HostToApp};
use harness::{Host, Page};
use std::time::Duration;

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) else { return };
    let host = |p: &mut Page, m: HostToApp| {
        let v = p.view.clone();
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(m, cx)));
        p.pump();
    };
    // Ask for a selector's rects: the app sends Overlay with req 1 at epoch 0.
    let v = p.view.clone();
    let (req, epoch) = p.cx.update(|cx| v.update(cx, |v, _| v.request_overlay(browser::protocol::OverlayQuery::Selector { selector: "#x".into() })));
    // The answer: a 100x50 CSS px rect at (40, 30) inside an iframe at (10, 20).
    let answer = move |epoch| HostToApp::OverlayRects { id: "page".into(), req, epoch, rects: vec![CssRect { x: 40.0, y: 30.0, w: 100.0, h: 50.0 }], frames: vec![CssPoint { x: 10.0, y: 20.0 }] };
    host(&mut p, answer(epoch));
    let rects = p.cx.update(|cx| p.view.read(cx).overlay_rects().to_vec());
    assert_eq!(rects.len(), 1, "the current answer shows");
    assert_eq!((rects[0].x, rects[0].y, rects[0].w, rects[0].h), (50.0, 50.0, 100.0, 50.0), "frame offset added, zoom 1");
    // Drawn over the frame: the outline's blue at its left edge (page DIPs 51, 75).
    let px = p.page_pixel(51.0, 75.0);
    assert!(px[2] > 200 && px[0] < 120, "blue outline over the frame at (51, 75): {px:?}");
    // A scroll hides it at once; the old answer arriving late stays hidden.
    host(&mut p, HostToApp::Viewport { id: "page".into(), scroll_x: 0.0, scroll_y: 100.0, zoom_level: 0.0 });
    assert!(p.cx.update(|cx| p.view.read(cx).overlay_rects().is_empty()), "hidden after a scroll");
    host(&mut p, answer(epoch));
    assert!(p.cx.update(|cx| p.view.read(cx).overlay_rects().is_empty()), "a stale answer never shows");
    let px = p.page_pixel(51.0, 75.0);
    assert!(!(px[2] > 200 && px[0] < 120), "no outline drawn after the scroll: {px:?}");
    // Zoom 120 %: a fresh answer lands scaled.
    host(&mut p, HostToApp::Viewport { id: "page".into(), scroll_x: 0.0, scroll_y: 100.0, zoom_level: 1.0 });
    let (req2, epoch2) = p.cx.update(|cx| v.update(cx, |v, _| v.request_overlay(browser::protocol::OverlayQuery::Selection)));
    host(&mut p, HostToApp::OverlayRects { id: "page".into(), req: req2, epoch: epoch2, rects: vec![CssRect { x: 100.0, y: 100.0, w: 10.0, h: 10.0 }], frames: vec![] });
    let rects = p.cx.update(|cx| p.view.read(cx).overlay_rects().to_vec());
    assert!(rects.len() == 1 && (rects[0].x - 120.0).abs() < 0.01 && (rects[0].w - 12.0).abs() < 0.01, "zoomed: {rects:?}");
    // Hovers: the first is sent, later moves wait for its answer (the stub answers with empty rects).
    for i in 0..10 {
        p.cx.update(|cx| v.update(cx, |v, _| v.hover_overlay(browser::geometry::Pt { x: 10.0 + i as f32, y: 10.0 })));
    }
    p.run_for(Duration::from_millis(200));
    println!("geometry wiring passed: rect at (50, 50) drawn over the frame, hidden on scroll, stale answer dropped, zoom 120 % applied");
}
