//! Zoom (I10): cmd-plus / cmd-minus / cmd-0 step through Chrome's presets, the bar shows the
//! level, each host keeps its own (saved to zoom.json). Stub part checks the messages; the CEF
//! part (HERDR_BROWSER_CEF_HOST) checks the page really zoomed (devicePixelRatio, innerWidth).
include!("common/browser_harness.rs");

use harness::{Host, Page};
use std::time::Duration;

fn zoom_pct(p: &mut Page) -> String {
    let l = p.cx.update(|cx| p.view.read(cx).zoom_level());
    browser::zoom::percent(l)
}

fn last_set_zoom(p: &mut Page) -> Option<f64> {
    p.console().iter().rev().find_map(|l| {
        let i = l.find(r#""type":"set_zoom""#)?;
        let v: serde_json::Value = serde_json::from_str(l[l[..i].rfind('{')?..].trim()).ok()?;
        v["level"].as_f64()
    })
}

fn main() {
    let server = fixtures::Server::start();
    if let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) {
        p.focus();
        p.key("cmd-=");
        p.key("cmd-=");
        assert_eq!(zoom_pct(&mut p), "125%");
        assert!(p.wait(Duration::from_secs(5), |p| last_set_zoom(p).is_some_and(|l| (browser::zoom::percent(l)) == "125%")), "SetZoom 125 %: {:?}", p.console());
        p.key("cmd--");
        assert_eq!(zoom_pct(&mut p), "110%");
        // Another host starts at its own level (100 %), and coming back restores 110 %.
        let other = server.url("form.html").replace("127.0.0.1", "localhost");
        p.navigate(&other).unwrap();
        assert!(p.wait(Duration::from_secs(5), |p| p.state().url.contains("localhost")));
        assert_eq!(zoom_pct(&mut p), "100%", "a new host is not zoomed");
        p.navigate(&server.url("raf.html")).unwrap();
        assert!(p.wait(Duration::from_secs(5), |p| p.state().url.contains("127.0.0.1")));
        assert_eq!(zoom_pct(&mut p), "110%", "the host's zoom comes back");
        let saved = std::fs::read_to_string(p.dir.join("zoom.json")).unwrap_or_default();
        assert!(saved.contains("127.0.0.1") && !saved.contains("localhost"), "zoom.json keeps zoomed hosts only: {saved}");
        p.key("cmd-0");
        assert_eq!(zoom_pct(&mut p), "100%");
        println!("zoom passed (stub): 125 % then 110 %, per host, saved, cmd-0 resets");
    }
    let Some(mut p) = Page::open(Host::Cef, &server.url("index.html")) else { return };
    p.focus();
    let dpr0 = p.eval("devicePixelRatio").unwrap().as_f64().unwrap();
    let w0 = p.eval("innerWidth").unwrap().as_f64().unwrap();
    p.key("cmd-=");
    p.key("cmd-=");
    let dpr = p.eval_until("devicePixelRatio", Duration::from_secs(5), |v| v.as_f64().is_some_and(|d| d > dpr0 * 1.2));
    let w = p.eval("innerWidth").unwrap().as_f64().unwrap();
    let ratio = dpr.as_f64().unwrap() / dpr0;
    assert!((ratio - 1.25).abs() < 0.02, "devicePixelRatio x1.25: {dpr0} -> {dpr}");
    assert!((w0 / w - 1.25).abs() < 0.05, "innerWidth / 1.25: {w0} -> {w}");
    p.key("cmd-0");
    let back = p.eval_until("devicePixelRatio", Duration::from_secs(5), |v| v.as_f64() == Some(dpr0));
    assert_eq!(back.as_f64(), Some(dpr0));
    println!("zoom passed (CEF): devicePixelRatio {dpr0} -> {dpr} (x{ratio:.2}), innerWidth {w0} -> {w}, cmd-0 back to {dpr0}");
}
