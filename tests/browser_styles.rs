//! The style panel (S10) end to end on the real CEF host: pick #go with the panel on, the panel
//! reads its ten rows, padding set to 16px changes the computed value and grows the element by
//! 32 px, Revert restores the style attribute string-equal. Stub part: the wiring alone.
include!("common/browser_harness.rs");

use browser::input::Owner;
use browser::styles::Status;
use harness::{Host, Page};
use std::time::Duration;

fn panel_status(p: &mut Page) -> Option<Status> {
    p.cx.update(|cx| p.view.read(cx).style_panel().map(|s| s.read(cx).status.clone()))
}

fn computed(p: &mut Page, prop: &str) -> Option<String> {
    p.cx.update(|cx| p.view.read(cx).style_panel().and_then(|s| s.read(cx).panel.rows.iter().find(|r| r.prop == prop).map(|r| r.computed.clone())))
}

fn pick(p: &mut Page, x: f32, y: f32) {
    let v = p.view.clone();
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.click(x, y);
}

fn main() {
    let server = fixtures::Server::start();
    if let Some(mut p) = Page::open(Host::Stub, &server.url("styles.html")) {
        p.focus();
        let v = p.view.clone();
        p.cx.update(|cx| v.update(cx, |v, cx| v.set_styles_on_pick(true, cx)));
        pick(&mut p, 60.0, 50.0);
        // The stub's PickInfo answers ok:false: no element, so no panel.
        p.run_for(Duration::from_millis(300));
        assert!(panel_status(&mut p).is_none(), "nothing picked, no panel");
        assert_eq!(p.cx.update(|cx| v.read(cx).owner(cx)), Owner::Browse);
        println!("styles passed (stub): nothing there opens no panel, pick ends");
    }
    let Some(mut p) = Page::open(Host::Cef, &server.url("styles.html")) else { return };
    p.focus();
    let v = p.view.clone();
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_styles_on_pick(true, cx)));
    let r = p.eval("rect('go')").unwrap();
    let (x, y, w0) = (r["x"].as_f64().unwrap() as f32, r["y"].as_f64().unwrap() as f32, r["w"].as_f64().unwrap());
    pick(&mut p, x, y);
    assert!(p.wait(Duration::from_secs(10), |p| panel_status(p) == Some(Status::Ready)), "panel ready: {:?}", panel_status(&mut p));
    let node = p.cx.update(|cx| v.read(cx).style_panel().map(|s| s.read(cx).node.clone()));
    assert_eq!(node.as_deref(), Some("#go"), "the panel is on the picked element");
    let rows = p.cx.update(|cx| v.read(cx).style_panel().map(|s| s.read(cx).panel.rows.len()));
    assert_eq!(rows, Some(10), "ten rows");
    assert_eq!(computed(&mut p, "padding").as_deref(), Some("0px"));
    // padding 16px: computed follows, the button grows by 2 x 16 px.
    let panel = p.cx.update(|cx| v.read(cx).style_panel().cloned()).unwrap();
    p.cx.update(|cx| panel.update(cx, |s, cx| s.set("padding", "16px", cx)));
    assert!(p.wait(Duration::from_secs(10), |p| computed(p, "padding").as_deref() == Some("16px")), "computed padding: {:?}", computed(&mut p, "padding"));
    let w1 = p.eval("rect('go').w").unwrap().as_f64().unwrap();
    assert!((w1 - w0 - 32.0).abs() < 0.5, "width +32 px: {w0} -> {w1}");
    // Revert: the style attribute is exactly what it was.
    let window = p.window_handle();
    p.cx.update_window(window, |_, w, cx| panel.update(cx, |s, cx| s.revert(w, cx))).unwrap();
    let style = p.eval_until("document.getElementById('go').getAttribute('style')", Duration::from_secs(10), |v| v == "color: red");
    assert_eq!(style, "color: red", "Revert restores the attribute string-equal");
    let w2 = p.eval("rect('go').w").unwrap().as_f64().unwrap();
    assert!((w2 - w0).abs() < 0.5, "width back: {w0} -> {w2}");
    // Esc closes the panel.
    p.key("escape");
    assert!(panel_status(&mut p).is_none(), "Esc closes the panel");
    println!("styles passed (CEF): picked #go, 10 rows, padding 0px -> 16px computed, width {w0} -> {w1} (+32), Revert style attribute \"color: red\", Esc closes");
}
