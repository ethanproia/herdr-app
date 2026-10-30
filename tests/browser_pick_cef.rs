//! S6 and S7 on the real CEF host: the view's pick asks PickInfo and the host answers it (no
//! answer played by the test, unlike the stub test browser_annotate.rs). A comment-on-pick on
//! #go targets it with its real box; a region drag anchors to the element under its corner.
include!("common/browser_harness.rs");

use browser::input::Owner;
use harness::{Host, Page};
use std::time::Duration;

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Cef, &server.url("pick.html")) else { return };
    assert!(p.wait_loaded(Duration::from_secs(20)), "page loads");
    p.focus();
    let v = p.view.clone();
    // S7: comment-on-pick on #go (page box 200,120 98x43): the host's PickInfo names it.
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(true, cx)));
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.click(240.0, 140.0);
    let got = p.wait(Duration::from_secs(10), |p| p.cx.update(|cx| p.view.read(cx).comment_target().is_some()));
    assert!(got, "the real host answered PickInfo and the comment field opened");
    let t = p.cx.update(|cx| v.read(cx).comment_target().cloned()).unwrap();
    assert_eq!((t.selector.as_str(), t.rect), ("#go", (200.0, 120.0, 98.0, 43.0)), "{t:?}");
    println!("pick (CEF): comment target {} {:?}", t.selector, t.rect);
    p.key("escape");
    p.pump();
    // S6: a drag in pick keeps a region anchored to the element under its top-left corner.
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(false, cx)));
    if p.cx.update(|cx| v.read(cx).owner(cx)) != Owner::Pick {
        p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    }
    p.mouse_move(100.0, 100.0, None);
    p.mouse_down(100.0, 100.0, gpui::MouseButton::Left, 1, gpui::Modifiers::default());
    p.mouse_move(200.0, 160.0, Some(gpui::MouseButton::Left));
    p.pump();
    p.mouse_up(200.0, 160.0, gpui::MouseButton::Left, 1, gpui::Modifiers::default());
    assert!(p.wait(Duration::from_secs(10), |p| !p.cx.update(|cx| p.view.read(cx).regions.is_empty())), "a region is kept");
    let r = p.cx.update(|cx| v.read(cx).regions.clone());
    assert_eq!((r[0].anchor.as_deref(), r[0].offset, r[0].size), (Some("#anchor"), (10.0, 20.0), (100.0, 60.0)), "{:?}", r[0]);
    println!("region (CEF): anchored to {:?} at offset {:?} size {:?}", r[0].anchor, r[0].offset, r[0].size);
    println!("pick_cef passed: real PickInfo for comment-on-pick and region anchors");
}
