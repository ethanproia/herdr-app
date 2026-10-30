//! The attach flow (S7 Send with a picture) on the real CEF host: a pin, a markup rectangle and a
//! region, then Send: the batch carries the comments, the region, and a PNG of the page cut to the
//! region with the markup burned in (red pixels exactly where the rectangle was).
include!("common/browser_harness.rs");

use browser::input::Owner;
use browser::view::BrowserEvent;
use harness::{Host, Page};
use std::time::Duration;

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Cef, &server.url("select.html")) else { return };
    p.focus();
    let v = p.view.clone();
    // A comment pin on #drop.
    let rr: Vec<f32> = p.eval("(()=>{const r=document.getElementById('drop').getBoundingClientRect();return [r.x,r.y,r.width,r.height]})()").unwrap().as_array().unwrap().iter().map(|n| n.as_f64().unwrap() as f32).collect();
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(true, cx)));
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.click(rr[0] + rr[2] / 2.0, rr[1] + rr[3] / 2.0);
    assert!(p.wait(Duration::from_secs(10), |p| p.cx.update(|cx| p.view.read(cx).comment_target().is_some())));
    p.text("drop zone too small");
    p.key("enter");
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(false, cx)));
    // Markup: a rectangle from (60, 40) to (160, 90).
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Markup, cx)));
    p.drag((60.0, 40.0), (160.0, 90.0), 6);
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Markup, cx)));
    assert_eq!(p.cx.update(|cx| v.read(cx).markup().read(cx).shapes().len()), 1);
    // A region from (40, 20) to (240, 120) (page top under the bar, now that the strip is gone).
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.drag((40.0, 20.0), (240.0, 120.0), 6);
    assert!(p.wait(Duration::from_secs(10), |p| !p.cx.update(|cx| p.view.read(cx).regions.is_empty())));
    // Send.
    p.cx.update(|cx| v.update(cx, |v, cx| v.send_annotations(cx)));
    assert!(p.wait(Duration::from_secs(20), |p| p.events().iter().any(|e| matches!(e, BrowserEvent::Annotations(_)))), "batch sent");
    let batch = p.events().into_iter().find_map(|e| match e { BrowserEvent::Annotations(b) => Some(b), _ => None }).unwrap();
    assert_eq!(batch.annotations.len(), 1);
    assert_eq!(batch.annotations[0].comment, "drop zone too small");
    assert_eq!(batch.annotations[0].target.selector, "#drop");
    let region = batch.region.clone().expect("region in the batch");
    assert!(!region.detached && (region.rect.2 - 200.0).abs() < 1.0 && (region.rect.3 - 100.0).abs() < 1.0, "{region:?}");
    assert!(batch.untrusted.iter().any(|u| u == "annotations.target.label") && batch.untrusted.iter().any(|u| u == "url"));
    let pic = image::open(batch.picture.clone().expect("a picture")).unwrap().to_rgba8();
    let s = batch.scale;
    assert!((pic.width() as f32 - 200.0 * s).abs() <= 2.0 && (pic.height() as f32 - 100.0 * s).abs() <= 2.0, "cropped to the region: {}x{} at scale {s}", pic.width(), pic.height());
    // The markup rectangle's left edge (x 60 - 40 = 20 DIPs into the crop), between its top and bottom.
    let red = |x: f32, y: f32| { let q = pic.get_pixel((x * s) as u32, (y * s) as u32).0; q[0] > 220 && q[1] < 90 && q[2] < 90 };
    assert!(red(20.0, 45.0), "markup burned in at the rectangle's edge");
    assert!(!red(70.0, 45.0), "nothing burned inside the rectangle");
    // Serializes for the agent side.
    let json = serde_json::to_string(&batch).unwrap();
    assert!(json.contains("\"version\":1") && json.contains("drop zone too small"));
    println!("attach passed (CEF): 1 comment on #drop, region 200x100, picture {}x{} at {s}x with markup burned in, batch {} bytes of JSON", pic.width(), pic.height(), json.len());
}
