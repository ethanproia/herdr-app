//! S6 regions and S7 annotations wired into the block (stub host; the host's PickInfo answer is
//! played by the test, as the stub answers ok:false). A pick with comment-on-pick opens a comment
//! field on the target, Enter saves a numbered pin, the badge counts across pages, Send hands the
//! batch to the window; a drag in pick draws a rubber band and keeps an anchored region.
include!("common/browser_harness.rs");

use browser::input::Owner;
use browser::protocol::{CssRect, HostToApp};
use browser::view::BrowserEvent;
use harness::{Host, Page};
use std::time::Duration;

fn owner(p: &mut Page) -> Owner {
    p.cx.update(|cx| p.view.read(cx).owner(cx))
}

/// Answer the view's newest PickInfo as the host would. The stub answers every PickInfo with
/// ok:false, which can arrive first; `again` repeats the gesture until the test's answer wins.
fn answer(p: &mut Page, selector: &str, label: &str, rect: CssRect, mut again: impl FnMut(&mut Page)) {
    let v = p.view.clone();
    for _ in 0..30 {
        if let Some(req) = p.cx.update(|cx| v.read(cx).pending_pick_reqs()).last().copied() {
            p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::PickInfoResult { id: "page".into(), req, ok: true, selector: selector.into(), label: label.into(), rect }, cx)));
            p.pump();
            return;
        }
        again(p);
    }
    panic!("the stub's ok:false always answered first");
}

fn pick_at(p: &mut Page, x: f32, y: f32) {
    let v = p.view.clone();
    if p.cx.update(|cx| v.read(cx).owner(cx)) != Owner::Pick {
        p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    }
    p.click(x, y);
}

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) else { return };
    p.focus();
    let v = p.view.clone();
    // S7: comment-on-pick, pick a button, the field opens on it.
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(true, cx)));
    pick_at(&mut p, 210.0, 130.0);
    answer(&mut p, "#go", "button Go", CssRect { x: 200.0, y: 120.0, w: 80.0, h: 30.0 }, |p| pick_at(p, 210.0, 130.0));
    assert_eq!(owner(&mut p), Owner::Comment, "the comment field takes the keyboard");
    let t = p.cx.update(|cx| v.read(cx).comment_target().cloned()).expect("a target");
    assert_eq!((t.selector.as_str(), t.rect), ("#go", (200.0, 120.0, 80.0, 30.0)));
    // The field draws under the target: white card at (210, 160).
    let px = p.page_pixel(300.0, 175.0);
    assert!(px[0] > 240 && px[1] > 240 && px[2] > 240, "comment field drawn: {px:?}");
    let before = p.console().iter().filter(|l| l.contains(r#""type":"text""#)).count();
    p.text("make it blue");
    p.key("enter");
    assert_eq!(owner(&mut p), Owner::Browse, "Enter saves and closes the field and pick");
    let a = p.cx.update(|cx| v.read(cx).annotations().pending().cloned().collect::<Vec<_>>());
    assert_eq!(a.len(), 1);
    assert_eq!((a[0].n, a[0].comment.as_str(), a[0].target.selector.as_str()), (1, "make it blue", "#go"));
    p.run_for(Duration::from_millis(200));
    assert_eq!(p.console().iter().filter(|l| l.contains(r#""type":"text""#)).count(), before, "the page got none of the comment");
    // The pin: a red disc at the target's top-left.
    // Left of the white digit in the pin's middle.
    let pin = p.page_pixel(194.0, 120.0);
    assert!(pin[0] > 220 && pin[1] < 90 && pin[2] < 90, "pin 1 drawn: {pin:?}");
    // A second pin, then the page changes: pins hide but the badge keeps counting.
    pick_at(&mut p, 400.0, 300.0);
    answer(&mut p, "#x", "div x", CssRect { x: 390.0, y: 290.0, w: 40.0, h: 20.0 }, |p| pick_at(p, 400.0, 300.0));
    p.text("and this");
    p.key("enter");
    let n = p.cx.update(|cx| v.read(cx).annotations().count());
    assert_eq!(n, 2);
    p.navigate(&server.url("raf.html")).unwrap();
    assert!(p.wait(Duration::from_secs(5), |p| p.state().url.ends_with("raf.html")));
    assert_eq!(p.cx.update(|cx| v.read(cx).annotations().visible().count()), 0, "pins hide on another page");
    assert_eq!(p.cx.update(|cx| v.read(cx).annotations().count()), 2, "the badge keeps counting");
    // Send: the batch goes to the window, the queue empties.
    p.cx.update(|cx| v.update(cx, |v, cx| v.send_annotations(cx)));
    let sent = p.events().into_iter().find_map(|e| match e {
        BrowserEvent::Annotations(b) => Some(b),
        _ => None,
    });
    let sent = sent.expect("Annotations event");
    assert_eq!(sent.iter().map(|a| a.comment.as_str()).collect::<Vec<_>>(), ["make it blue", "and this"]);
    assert_eq!(p.cx.update(|cx| v.read(cx).annotations().count()), 0);
    // S6: a drag in pick draws a rubber band and leaves an anchored region.
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(false, cx)));
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.mouse_move(100.0, 100.0, None);
    p.mouse_down(100.0, 100.0, gpui::MouseButton::Left, 1, gpui::Modifiers::default());
    p.mouse_move(200.0, 160.0, Some(gpui::MouseButton::Left));
    p.pump();
    let band = p.page_region(140.0, 100.0, 1.0, 1.0);
    let edge = band.get_pixel(0, 0).0;
    assert!(edge[2] > 200 && edge[0] < 120, "rubber band top edge drawn: {edge:?}");
    p.mouse_up(200.0, 160.0, gpui::MouseButton::Left, 1, gpui::Modifiers::default());
    p.pump();
    assert!(p.events().iter().any(|e| matches!(e, BrowserEvent::Region(r) if r.w == 100.0 && r.h == 60.0)), "region event: {:?}", p.events());
    // The region is kept either way; with the stub's ok:false first it anchors to the document.
    let won = p.cx.update(|cx| v.read(cx).pending_pick_reqs()).last().copied();
    if let Some(req) = won {
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::PickInfoResult { id: "page".into(), req, ok: true, selector: "#anchor".into(), label: "section".into(), rect: CssRect { x: 90.0, y: 80.0, w: 400.0, h: 300.0 } }, cx)));
        p.pump();
    }
    assert!(p.wait(Duration::from_secs(5), |p| !p.cx.update(|cx| p.view.read(cx).regions.is_empty())));
    let regions = p.cx.update(|cx| v.read(cx).regions.clone());
    assert_eq!(regions.len(), 1);
    if won.is_none() {
        assert_eq!((regions[0].anchor.as_deref(), regions[0].offset, regions[0].size), (None, (100.0, 100.0), (100.0, 60.0)), "document-anchored");
        println!("annotate: region anchored to the document (the stub answered first)");
    } else {
    assert_eq!((regions[0].anchor.as_deref(), regions[0].offset, regions[0].size), (Some("#anchor"), (10.0, 20.0), (100.0, 60.0)));
    }
    assert_eq!(owner(&mut p), Owner::Browse);
    println!("annotate passed: comment-on-pick field, Enter saves pin 1 (red), page untouched, badge counts across pages, Send hands 2 to the window; region drag draws a band and keeps an anchored region");
}
