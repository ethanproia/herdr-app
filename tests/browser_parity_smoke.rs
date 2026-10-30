//! The parity harness checks itself: every helper once, against the real CEF host when
//! `HERDR_BROWSER_CEF_HOST` is set (the stub otherwise, where only frames and echoes exist).
//! Run: `cargo test --locked --test browser_parity_smoke`.
include!("common/browser_harness.rs");

use harness::{Host, Page, metric};
use std::time::{Duration, Instant};

fn main() {
    let server = fixtures::Server::start();
    if let Some(mut p) = Page::open(Host::Stub, &server.url("form.html")) {
        p.focus();
        p.click(100.0, 20.0);
        p.text("hi");
        assert!(p.wait(Duration::from_secs(5), |p| p.console().iter().any(|l| l.contains(r#""type":"text""#))), "stub saw no text: {:?}", p.console());
        // The app's side of a double-click and a drag, as the host receives them.
        p.double_click(50.0, 50.0);
        p.drag((10.0, 10.0), (60.0, 10.0), 2);
        let c = p.console();
        assert!(c.iter().any(|l| l.contains(r#""kind":"down""#) && l.contains(r#""clicks":2"#)), "double-click sends clicks 2: {c:?}");
        assert!(c.iter().any(|l| l.contains(r#""kind":"move""#) && l.contains(r#""button":"left""#)), "drag moves carry the button: {c:?}");
        println!("smoke stub: frames {}, input echoed", p.frames().len());
    }
    let Some(mut p) = Page::open(Host::Cef, &server.url("form.html")) else { return };
    // Form: click the text input, type, read back.
    let r = p.eval("rect('text')").unwrap();
    let (x, y) = (r["x"].as_f64().unwrap() as f32, r["y"].as_f64().unwrap() as f32);
    p.click(x, y);
    p.text("héllo");
    p.key("shift-left");
    let v = p.eval_until("document.getElementById('text').value", Duration::from_secs(5), |v| v == "héllo");
    assert_eq!(v, "héllo");
    // IME into the textarea: composition then commit.
    let r = p.eval("rect('area')").unwrap();
    p.click(r["x"].as_f64().unwrap() as f32, r["y"].as_f64().unwrap() as f32);
    p.ime_set("にほん", 3..3);
    p.ime_commit("日本");
    let v = p.eval_until("document.getElementById('area').value", Duration::from_secs(5), |v| v == "日本");
    assert_eq!(v, "日本", "IME commit; log {:?}", p.eval("window.log.slice(-8)"));
    // Select all with a modifier key.
    p.key("cmd-a");
    let sel = p.eval("document.activeElement.selectionEnd - document.activeElement.selectionStart").unwrap();
    println!("smoke cef: form typed and IME committed; cmd-a selection length {sel}");
    // Scroll: precise wheel events, frames recorded and hashed.
    p.navigate(&server.url("scroll.html")).unwrap();
    p.hash_frames(true);
    let start = Instant::now();
    p.clear_frames();
    for _ in 0..60 {
        p.wheel(300.0, 300.0, 0.0, -40.0, true);
        p.run_for(Duration::from_millis(16));
    }
    let y = p.eval("scrollY").unwrap();
    let st = p.frame_stats(start);
    assert!(y.as_f64().unwrap_or(0.0) > 500.0, "scrolled: scrollY {y}");
    metric("SMOKE-scroll-fps", st.fps, "fps", ">= 55 (R1 measures properly)");
    metric("SMOKE-scroll-p95-gap", st.p95_gap_ms, "ms", "informational");
    println!("smoke cef: scrollY {y}, {} frames, {} distinct", st.count, st.distinct);
    // Drag, double-click, right-click on the selection page.
    p.navigate(&server.url("select.html")).unwrap();
    let r = p.eval("rect('p')").unwrap();
    let (px_, py) = (r["x"].as_f64().unwrap() as f32, r["y"].as_f64().unwrap() as f32);
    p.double_click(px_ - 150.0, py);
    let word = p.eval("selected()").unwrap();
    p.drag((px_ - 200.0, py), (px_ + 100.0, py), 10);
    let span = p.eval("selected()").unwrap();
    p.right_click(px_, py);
    let menu = p.eval_until("window.menu", Duration::from_secs(3), |v| !v.is_null());
    println!("smoke cef: double-click selected {word}, drag selected {} chars, contextmenu on {menu}", span.as_str().map_or(0, str::len));
    // Not asserted here: I3/I4 own these (the app sends clicks 2 and button-held moves, see the stub part).
    metric("SMOKE-dblclick-word-chars", word.as_str().map_or(0, str::len) as f64, "chars", "> 0 (I3)");
    metric("SMOKE-drag-select-chars", span.as_str().map_or(0, str::len) as f64, "chars", "> 10 (I4)");
    // Resize the block, then cost.
    let t = Instant::now();
    p.resize_block(Some((600.0, 400.0)));
    // The page area is the block minus the bar: 600 x 372 DIPs, at 2x.
    let ok = p.wait(Duration::from_secs(5), |p| p.frames().iter().any(|f| f.size.w == 1200 && f.size.h == 744 && f.painted.is_some()));
    assert!(ok, "a frame at the new size: {:?}", p.frames().last().map(|f| f.size));
    println!("smoke cef: frame at the new size 1200x744 after {} ms", t.elapsed().as_millis());
    let u = p.usage_over(Duration::from_secs(2));
    metric("SMOKE-idle-cpu", u.cpu, "%", "<= 2 per tab (R6 measures properly)");
    metric("SMOKE-rss", u.rss_mb, "MB", "report");
    let img = p.shot();
    let out = std::env::var_os("HERDR_PARITY_OUT").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    img.save(out.join("parity-smoke.png")).unwrap();
    println!("smoke cef passed: {} processes, picture {}", u.processes, out.join("parity-smoke.png").display());
}
