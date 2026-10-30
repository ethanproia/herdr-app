//! Offscreen picture of a browser block (no window): a layout row with a terminal-coloured block
//! and a browser block, painted by the GPU, against a fake host on a temp socket.
//! `HERDR_BROWSER_SHOT=<png>` keeps the picture. Main thread, so `harness = false`.
#[path = "../src/layout.rs"]
#[allow(dead_code)]
mod layout;
#[path = "../src/browser/mod.rs"]
mod browser;
mod mods {
    pub fn app_dir() -> std::path::PathBuf {
        std::env::temp_dir()
    }
}

use browser::protocol::{self, AppToHost, HostToApp};
use browser::view::{BrowserView, HostClient, HostConfig};
use gpui::{prelude::*, *};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Scene {
    browser: Entity<BrowserView>,
}

impl Render for Scene {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().bg(rgb(0x123456)).child(div().w(px(120.)).h_full().bg(rgb(0x1e1e1e))).child(div().flex_1().h_full().child(self.browser.clone()))
    }
}

/// Accepts one connection, records every line, answers `Url` (and a `Title`) for `Create`.
fn fake_host(sock: std::path::PathBuf) -> Arc<Mutex<Vec<AppToHost>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let s = seen.clone();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut out = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            let msg: AppToHost = protocol::decode(&line).unwrap();
            if let AppToHost::Create { id, url: Some(url), .. } = &msg {
                for m in [HostToApp::Url { id: id.clone(), url: url.clone() }, HostToApp::Title { id: id.clone(), title: "Example".into() }] {
                    let _ = writeln!(out, "{}", protocol::encode(&m));
                }
            }
            s.lock().unwrap().push(msg);
        }
    });
    seen
}

fn main() {
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-shot", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let seen = fake_host(dir.join("host.sock"));
    let link = HostClient::new(HostConfig { dir, program: "/nonexistent".into(), args: vec![] });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    let block = layout::BrowserBlock { url: "https://example.com".into(), profile: "default".into() };
    let states = Arc::new(Mutex::new(HashMap::new()));
    let window = cx
        .open_window(size(px(480.), px(300.)), |window, cx| {
            let browser = cx.new(|cx| BrowserView::new("web", block, link.clone(), states.clone(), window, cx));
            cx.new(|_| Scene { browser })
        })
        .unwrap();
    let ready = |s: &Mutex<HashMap<String, browser::BrowserState>>| s.lock().unwrap().get("web").is_some_and(|s| s.title == "Example");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready(&states) {
        assert!(Instant::now() < deadline, "the page never reported its url and title");
        cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    cx.update_window(window.into(), |_, window, _| window.refresh()).unwrap();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window.into()).unwrap();
    if let Some(out) = std::env::var_os("HERDR_BROWSER_SHOT") {
        image.save(&out).unwrap();
    }
    let scale = image.width() as f32 / 480.0;
    let at = |x: f32, y: f32| image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
    assert_eq!(at(60., 150.), [0x1e, 0x1e, 0x1e, 255], "the block beside it");
    assert_eq!(at(300., 280.), [255, 255, 255, 255], "the page area (a white sheet until part 3 paints frames)");
    assert_ne!(at(400., 14.), [255, 255, 255, 255], "the chrome bar above the page");
    // The host was told the page area in DIPs (block minus the 28px bar) at the window's scale.
    let create = seen.lock().unwrap().iter().find_map(|m| match m {
        AppToHost::Create { id, size, scale, url } if id == "web" => Some((*size, *scale, url.clone())),
        _ => None,
    });
    let (size, k, url) = create.expect("Create sent");
    assert_eq!((size.w, size.h, url.as_deref()), (360.0, 272.0, Some("https://example.com")));
    assert_eq!(k, scale);
    // The block goes: its page is closed on the host and nothing stays open.
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.lock().unwrap().iter().any(|m| matches!(m, AppToHost::Close { id } if id == "web")) {
        assert!(Instant::now() < deadline, "no Close after the block went");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(link.open_ids().is_empty());
    println!("browser block shot passed: {}x{}, scale {scale}, page {}x{} DIPs", image.width(), image.height(), size.w, size.h);
}
