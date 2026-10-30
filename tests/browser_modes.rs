//! Part 4's input owners wired into the block (stub host): markup draws over the page and the
//! page gets none of it, pick hovers and picks, an agent lease shows "<agent> is driving ·
//! Take Control", refuses the user's input and ends on Take Control.
include!("common/browser_harness.rs");

use browser::input::Owner;
use browser::view::BrowserEvent;
use harness::{Host, Page};
use std::time::Duration;

fn mouse_lines(p: &mut Page) -> usize {
    p.console().iter().filter(|l| l.contains(r#""type":"mouse""#)).count()
}

fn owner(p: &mut Page) -> Owner {
    p.cx.update(|cx| p.view.read(cx).owner(cx))
}

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) else { return };
    p.focus();
    let v = p.view.clone();
    // Markup: the strip shows (the page moves down under it), a drag draws a rect, the page gets nothing.
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Markup, cx)));
    p.pump();
    assert_eq!(owner(&mut p), Owner::Markup);
    assert_eq!(p.page_top(), 56.0, "markup strip under the bar");
    p.run_for(Duration::from_millis(200));
    let before = mouse_lines(&mut p);
    p.drag((100.0, 100.0), (300.0, 200.0), 8);
    p.pump();
    let shapes = p.cx.update(|cx| v.read(cx).markup().read(cx).shapes().len());
    assert_eq!(shapes, 1, "a drag draws one shape");
    p.run_for(Duration::from_millis(200));
    assert_eq!(mouse_lines(&mut p), before, "the page saw none of the markup drag");
    // The rect's red edge over the frame (antialiased canvas: scan a few pixels).
    let edge = p.page_region(98.0, 150.0, 6.0, 1.0);
    assert!(edge.pixels().any(|q| q.0[0] > 220 && q.0[1] < 90 && q.0[2] < 90), "red markup edge at x 98..104: {:?}", edge.pixels().map(|q| q.0).collect::<Vec<_>>());
    p.key("cmd-z");
    p.pump();
    assert_eq!(p.cx.update(|cx| v.read(cx).markup().read(cx).shapes().len()), 0, "cmd-z undoes it");
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Markup, cx)));
    p.pump();
    assert_eq!(owner(&mut p), Owner::Browse);
    assert_eq!(p.page_top(), 28.0);
    // Pick: moves go to the hover hit test, a click picks and leaves pick; the page gets none.
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    let before = mouse_lines(&mut p);
    p.mouse_move(200.0, 120.0, None);
    p.click(210.0, 130.0);
    p.pump();
    assert!(p.events().iter().any(|e| matches!(e, BrowserEvent::Picked { x, y } if (*x - 210.0).abs() < 0.5 && (*y - 130.0).abs() < 0.5)), "picked at (210, 130): {:?}", p.events());
    assert_eq!(owner(&mut p), Owner::Browse, "a pick ends pick mode");
    p.run_for(Duration::from_millis(200));
    assert_eq!(mouse_lines(&mut p), before, "the page saw none of the pick");
    // Lease: the strip names the agent; the user's click is refused; Take Control ends it.
    let input = p.cx.update(|cx| v.read(cx).input().clone());
    p.cx.update(|cx| input.update(cx, |i, _| i.grant_lease("claude")));
    p.cx.update(|cx| v.update(cx, |v, cx| v.drain_overlay(cx)));
    p.pump();
    assert_eq!(p.cx.update(|cx| input.read(cx).lease().map(str::to_owned)), Some("claude".into()));
    assert_eq!(p.page_top(), 56.0, "lease strip shown");
    let before = mouse_lines(&mut p);
    p.click(300.0, 300.0);
    p.run_for(Duration::from_millis(200));
    assert_eq!(mouse_lines(&mut p), before, "the user's click was refused");
    p.cx.update(|cx| v.update(cx, |v, cx| v.take_control(cx)));
    p.pump();
    assert_eq!(p.cx.update(|cx| input.read(cx).lease().map(str::to_owned)), None, "Take Control ends the lease");
    assert_eq!(p.page_top(), 28.0);
    p.click(300.0, 300.0);
    assert!(p.wait(Duration::from_secs(5), |p| mouse_lines(p) > before), "the user drives again");
    println!("modes passed: markup draws (page untouched, red edge, cmd-z), pick picks at (210, 130), lease strip refuses input until Take Control");
}
