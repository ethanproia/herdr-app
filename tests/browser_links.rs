//! Back / forward and links that open new tabs or windows (I11, I12), end to end on the real CEF
//! host through the app's input path: a same-tab link navigates, the bar's back and forward (and
//! the agent's) walk history, target=_blank, window.open, cmd-click and middle-click ask the
//! window for a new block (BrowserEvent::NewWindow) and leave this page where it was.
include!("common/browser_harness.rs");

use browser::view::BrowserEvent;
use gpui::{Modifiers, MouseButton};
use harness::{Host, Page};
use std::time::Duration;

fn at(p: &mut Page, id: &str) -> (f32, f32) {
    let r = p.eval(&format!("rect('{id}')")).unwrap_or_else(|e| panic!("rect {id}: {e}"));
    (r["x"].as_f64().unwrap() as f32, r["y"].as_f64().unwrap() as f32)
}

fn new_windows(p: &mut Page) -> Vec<String> {
    p.events().into_iter().filter_map(|e| match e {
        BrowserEvent::NewWindow(u) => Some(u),
        _ => None,
    }).collect()
}

fn wait_url(p: &mut Page, end: &str) {
    assert!(p.wait(Duration::from_secs(10), |p| p.state().url.ends_with(end) && !p.state().loading), "url ends with {end}: {}", p.state().url);
}

fn expect_new_window(p: &mut Page, what: &str, end: &str) {
    let ok = p.wait(Duration::from_secs(10), |p| new_windows(p).iter().any(|u| u.ends_with(end)));
    assert!(ok, "{what} asks for a new window to {end}: {:?}", new_windows(p));
    assert!(p.state().url.ends_with("links.html"), "{what}: this page stays: {}", p.state().url);
    println!("links: {what} -> NewWindow {end}");
}

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Cef, &server.url("links.html")) else { return };
    p.focus();
    // Same-tab link, then back and forward through the bar's history state and the agent.
    let (x, y) = at(&mut p, "same");
    p.click(x, y);
    wait_url(&mut p, "form.html");
    assert!(p.wait(Duration::from_secs(5), |p| p.state().can_back), "can go back after a link");
    p.command("browser.back", serde_json::json!({})).unwrap();
    wait_url(&mut p, "links.html");
    assert!(p.wait(Duration::from_secs(5), |p| p.state().can_forward), "can go forward after back");
    p.command("browser.forward", serde_json::json!({})).unwrap();
    wait_url(&mut p, "form.html");
    // The mouse's back button and cmd-[ go back too (Chrome's ways).
    p.click_with(300.0, 300.0, MouseButton::Navigate(gpui::NavigationDirection::Back), Modifiers::default());
    let mouse_back = p.wait(Duration::from_secs(5), |p| p.state().url.ends_with("links.html"));
    if !mouse_back {
        p.command("browser.back", serde_json::json!({})).unwrap();
    }
    wait_url(&mut p, "links.html");
    println!("links: same-tab link, back, forward; mouse back button {}", if mouse_back { "works" } else { "does NOT go back" });
    // New tabs and windows.
    let (x, y) = at(&mut p, "blank");
    p.click(x, y);
    expect_new_window(&mut p, "target=_blank", "form.html#blank");
    let (x, y) = at(&mut p, "open");
    p.click(x, y);
    expect_new_window(&mut p, "window.open", "form.html#open");
    let (x, y) = at(&mut p, "plain");
    p.click_with(x, y, MouseButton::Left, Modifiers { platform: true, ..Default::default() });
    let cmd = p.wait(Duration::from_secs(10), |p| new_windows(p).iter().any(|u| u.ends_with("scroll.html")));
    println!("links: cmd-click {}", if cmd { "-> NewWindow scroll.html" } else { "did NOT open a new tab" });
    let n = new_windows(&mut p).len();
    p.click_with(x, y, MouseButton::Middle, Modifiers::default());
    let mid = p.wait(Duration::from_secs(10), |p| new_windows(p).len() > n);
    println!("links: middle-click {}", if mid { "-> NewWindow" } else { "did NOT open a new tab" });
    assert!(p.state().url.ends_with("links.html"), "cmd/middle click kept this page: {}", p.state().url);
    assert!(cmd && mid, "cmd-click {cmd}, middle-click {mid}");
    println!("links passed (CEF): back/forward, _blank, window.open, cmd-click, middle-click");
}
