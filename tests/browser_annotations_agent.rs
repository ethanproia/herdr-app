//! S8 on the real CEF host: the person pins a comment and presses Send; the agent side (agent.rs) takes the
//! batch and `browser.annotations` answers it to an agent, with the person's words marked trusted, the page's
//! fields untrusted, and the picture on disk.
include!("common/browser_harness.rs");

use browser::agent::{Agent, Focus};
use browser::input::Owner;
use browser::view::BrowserEvent;
use browser::{BrowserRegistry, BrowserState, HostLink};
use harness::{Host, Page};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `browser.annotations` without id needs no live browser: an empty registry is enough.
struct NoBrowsers;
impl HostLink for NoBrowsers {
    fn send(&self, _: browser::protocol::AppToHost) {}
}
impl BrowserRegistry for NoBrowsers {
    fn state(&self, _: &String) -> Option<BrowserState> {
        None
    }
    fn link(&self) -> &dyn HostLink {
        self
    }
}

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Cef, &server.url("select.html")) else { return };
    p.focus();
    let v = p.view.clone();
    // A comment pin on #drop (Send needs one), then Send.
    let rr: Vec<f32> = p.eval("(()=>{const r=document.getElementById('drop').getBoundingClientRect();return [r.x,r.y,r.width,r.height]})()").unwrap().as_array().unwrap().iter().map(|n| n.as_f64().unwrap() as f32).collect();
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(true, cx)));
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_mode(Owner::Pick, cx)));
    p.click(rr[0] + rr[2] / 2.0, rr[1] + rr[3] / 2.0);
    assert!(p.wait(Duration::from_secs(10), |p| p.cx.update(|cx| p.view.read(cx).comment_target().is_some())));
    p.text("make the drop zone bigger");
    p.key("enter");
    p.cx.update(|cx| v.update(cx, |v, cx| v.set_comment_on_pick(false, cx)));
    p.cx.update(|cx| v.update(cx, |v, cx| v.send_annotations(cx)));
    assert!(p.wait(Duration::from_secs(20), |p| p.events().iter().any(|e| matches!(e, BrowserEvent::Annotations(_)))), "batch sent");
    let batch = p.events().into_iter().find_map(|e| match e { BrowserEvent::Annotations(b) => Some(b), _ => None }).unwrap();

    // What `Browsers::annotations` does with it, then an agent asks.
    let dir = std::env::temp_dir().join(format!("herdr-annotations-agent-{}", std::process::id()));
    let mut agent = Agent::new(dir.join("browser"));
    agent.annotated(serde_json::to_value(&batch).unwrap());
    let got: Arc<Mutex<Option<Result<serde_json::Value, String>>>> = Default::default();
    let g = got.clone();
    let none = |_: &str| None;
    agent.run("browser.annotations", &serde_json::json!({}), &NoBrowsers, &Focus { focused: None, block_of_pane: &none }, Box::new(move |r| *g.lock().unwrap() = Some(r)));
    let r = got.lock().unwrap().take().expect("answered at once").expect("ok");
    let b = &r["batch"];
    assert_eq!(b["version"], 1);
    assert_eq!(b["annotations"][0]["comment"], "make the drop zone bigger");
    assert_eq!(b["annotations"][0]["target"]["selector"], "#drop");
    assert!(b["url"].as_str().unwrap().ends_with("select.html"));
    assert_eq!(b["trusted"], serde_json::json!(["annotations.comment", "css_edits"]));
    let untrusted: Vec<&str> = b["untrusted"].as_array().unwrap().iter().filter_map(|u| u.as_str()).collect();
    assert!(["url", "title", "annotations.target.label", "annotations.target.selector"].iter().all(|u| untrusted.contains(u)), "{untrusted:?}");
    let pic = b["picture"].as_str().expect("a picture path");
    let img = image::open(pic).expect("the picture is a PNG on disk");
    let _ = std::fs::remove_dir_all(&dir);
    println!("annotations agent passed (CEF): the agent got the comment on #drop, trusted [comment, css_edits], {} untrusted fields, picture {}x{}", untrusted.len(), img.width(), img.height());
}
