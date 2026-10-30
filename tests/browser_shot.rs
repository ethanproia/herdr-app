//! Offscreen picture of a browser block (no window): a layout row with a terminal-coloured block
//! and a browser block, painted by the GPU, against a fake host on a temp socket.
//! `HERDR_BROWSER_SHOT=<png>` keeps the picture. Main thread, so `harness = false`.
#[path = "../src/layout.rs"]
#[allow(dead_code)]
mod layout;
#[path = "../src/browser/mod.rs"]
mod browser;
/// The app's settings.rs needs the whole app; the block only reads the search engine.
mod settings {
    #[derive(Clone, Copy)]
    pub enum SearchEngine {
        Google,
    }
    impl SearchEngine {
        pub fn url(self, q: &str) -> String {
            format!("https://www.google.com/search?q={q}")
        }
    }
    pub struct Settings {
        pub search_engine: SearchEngine,
    }
    pub fn get() -> Settings {
        Settings { search_engine: SearchEngine::Google }
    }
}
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

use gpui_base::input::InputEvent;
use gpui::EntityInputHandler as _;

struct Scene {
    browser: Entity<BrowserView>,
    /// Width of the block beside the browser (changing it resizes the browser block).
    left: f32,
}

impl Render for Scene {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().bg(rgb(0x123456)).child(div().w(px(self.left)).h_full().bg(rgb(0x1e1e1e))).child(div().flex_1().h_full().child(self.browser.clone()))
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

/// Against the stub host (`HERDR_BROWSER_HOST`): its IOSurface frames are painted in the block
/// (left half red 0xC0, right half 0x40), acked after paint, and a resize brings a newer frame.
fn stub_frames() {
    let Some(program) = std::env::var_os("HERDR_BROWSER_HOST") else {
        println!("HERDR_BROWSER_HOST unset: stub frame test skipped");
        return;
    };
    frames(program.into(), vec!["--stub".into()], "https://example.com", |l, r| {
        let near = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 6;
        near(l[0], 0xC0) && near(r[0], 0x40)
    });
}

/// The real CEF host (`HERDR_BROWSER_CEF_HOST`, the bundle from scripts/fork-host-bundle.sh)
/// paints an orange page into the block. `HERDR_BROWSER_FRAME_SHOT` keeps the picture.
fn cef_frames() {
    let Some(program) = std::env::var_os("HERDR_BROWSER_CEF_HOST") else {
        println!("HERDR_BROWSER_CEF_HOST unset: real CEF frame test skipped");
        return;
    };
    let page = "data:text/html,<body style='margin:0;background:%23ff8000'><h1 style='font:40px sans-serif;color:%23fff;padding:20px'>CEF in Herdr</h1></body>";
    frames(program.into(), vec![], page, |l, r| {
        let orange = |p: [u8; 4]| p[0] > 0xF0 && (0x70..0x90).contains(&p[1]) && p[2] < 0x10;
        orange(l) && orange(r)
    });
}

/// Paints the frames `program` sends for `url` in a block, checks two page pixels with `ok`
/// (left and right halves, low in the page), and that a resize brings a newer frame.
fn frames(program: std::path::PathBuf, args: Vec<String>, url: &str, ok: impl Fn([u8; 4], [u8; 4]) -> bool) {
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-frames-{}", std::process::id(), args.len()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let link = HostClient::new(HostConfig { dir, program, args, cdp_port: None });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(gpui_base::init);
    let block = layout::BrowserBlock { url: url.into(), profile: "default".into() };
    let states = Arc::new(Mutex::new(HashMap::new()));
    let window = cx
        .open_window(size(px(480.), px(300.)), |window, cx| {
            let browser = cx.new(|cx| BrowserView::new("web", block, link.clone(), states.clone(), window, cx));
            cx.new(|_| Scene { browser, left: 120. })
        })
        .unwrap();
    let browser = window.update(&mut cx, |scene, _, _| scene.browser.clone()).unwrap();
    let wait_seq = |cx: &mut HeadlessAppContext, above: u64| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cx.update_window(window.into(), |_, window, cx| {
                let _ = window.draw(cx);
            })
            .unwrap();
            cx.run_until_parked();
            let seq = cx.update(|cx| browser.read(cx).frame_seq());
            if let Some(seq) = seq.filter(|s| *s > above) {
                return seq;
            }
            assert!(Instant::now() < deadline, "no frame above {above} (last {seq:?})");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    // Real pages paint more than once while loading: wait until the check holds.
    let mut first = wait_seq(&mut cx, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    let (image, l, r) = loop {
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
        cx.run_until_parked();
        let image = cx.capture_screenshot(window.into()).unwrap();
        let scale = image.width() as f32 / 480.0;
        let at = |x: f32, y: f32| image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
        // Page area: x 120..480 (DIPs), y 28..300; a pixel low in each half.
        let (l, r) = (at(180., 250.), at(420., 250.));
        if ok(l, r) {
            break (image, l, r);
        }
        assert!(Instant::now() < deadline, "frame {first} painted: left {l:?} right {r:?}");
        first = wait_seq(&mut cx, first);
    };
    if let Some(out) = std::env::var_os("HERDR_BROWSER_FRAME_SHOT") {
        image.save(&out).unwrap();
    }
    let scale = image.width() as f32 / 480.0;
    let at = |x: f32, y: f32| image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
    assert_eq!(at(60., 150.), [0x1e, 0x1e, 0x1e, 255], "the block beside it");
    // The painted frame was acked; a resize then brings a newer frame (the stub holds one until acked).
    window.update(&mut cx, |scene, _, cx| {
        scene.left = 200.;
        cx.notify();
    })
    .unwrap();
    let second = wait_seq(&mut cx, first);
    println!("frames passed ({}):", if url.starts_with("data:") { "real CEF" } else { "stub" }); println!("  frame {first} painted ({l:?} | {r:?}), frame {second} after resize");
    drop(browser);
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
}

/// Clicks, a key and typed text in the block reach the host as `Input` (the stub echoes each
/// back as a console line), the click focuses the page, and the pointer follows `Cursor`.
fn stub_input() {
    let Some(program) = std::env::var_os("HERDR_BROWSER_HOST") else {
        println!("HERDR_BROWSER_HOST unset: stub input test skipped");
        return;
    };
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-input", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let link = HostClient::new(HostConfig { dir, program: program.into(), args: vec!["--stub".into()], cdp_port: None });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(gpui_base::init);
    let block = layout::BrowserBlock { url: "https://example.com".into(), profile: "default".into() };
    let states = Arc::new(Mutex::new(HashMap::new()));
    let window = cx
        .open_window(size(px(480.), px(300.)), |window, cx| {
            let browser = cx.new(|cx| BrowserView::new("web", block, link.clone(), states.clone(), window, cx));
            cx.new(|_| Scene { browser, left: 120. })
        })
        .unwrap();
    let browser = window.update(&mut cx, |scene, _, _| scene.browser.clone()).unwrap();
    let pump = |cx: &mut HeadlessAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    let wait = |cx: &mut HeadlessAppContext, what: &str, want: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(cx);
            if cx.update(|cx| browser.read(cx).console().any(|l| want(l))) {
                return;
            }
            assert!(Instant::now() < deadline, "host never saw {what}: {:?}", cx.update(|cx| browser.read(cx).console().map(str::to_owned).collect::<Vec<_>>()));
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    while cx.update(|cx| browser.read(cx).frame_seq()).is_none() {
        pump(&mut cx);
        std::thread::sleep(Duration::from_millis(10));
    }
    // A click at window (300, 150) is page DIP (180, 122): block x 120, bar 28.
    let at = point(px(300.), px(150.));
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }), cx);
        window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1 }), cx);
    })
    .unwrap();
    wait(&mut cx, "focus", &|l| l == "focus true");
    wait(&mut cx, "the click", &|l| l.contains(r#""kind":"down""#) && l.contains(r#""x":180.0"#) && l.contains(r#""y":122.0"#));
    // A key, then text as the platform's text input delivers it.
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent { keystroke: Keystroke::parse("enter").unwrap(), is_held: false, prefer_character_input: false }), cx);
        let input = browser.read(cx).input().clone();
        input.update(cx, |i, cx| i.replace_text_in_range(None, "hé", window, cx));
    })
    .unwrap();
    wait(&mut cx, "Enter", &|l| l.contains(r#""key":"Enter""#));
    wait(&mut cx, "the text", &|l| l.contains(r#""type":"text""#) && l.contains("hé"));
    println!("stub input passed: click at page (180, 122), focus, Enter and text reached the host");
    drop(browser);
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
}

/// A window holding `Browsers` the way the app's window does (one block, "web").
struct Agents {
    browsers: browser::view::Browsers,
    /// Events the block sent its window.
    events: Vec<browser::view::BrowserEvent>,
}

impl Render for Agents {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let v = self.browsers.view("web").cloned();
        div().size_full().flex().bg(rgb(0x1e1e1e)).child(div().w(px(120.))).child(div().flex_1().h_full().children(v))
    }
}

/// `browser.*` through the agent (part 5) as the window runs it: eval and screenshot get their
/// answers from the host; with real CEF, text typed through the page's input reads back by eval.
fn agent(program: std::path::PathBuf, args: Vec<String>, url: &str, real: bool) {
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-agent-{}", std::process::id(), args.len()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let link = HostClient::new(HostConfig { dir: dir.clone(), program, args, cdp_port: Some(0) });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(gpui_base::init);
    let block = layout::BrowserBlock { url: url.into(), profile: "default".into() };
    let window = cx
        .open_window(size(px(480.), px(300.)), |window, cx| {
            cx.new(|cx| {
                let mut browsers = browser::view::Browsers::with_link(link.clone());
                browsers.sync(vec![("web".into(), block)], window, cx, |a: &mut Agents, _, ev, _, _| a.events.push(ev.clone()));
                Agents { browsers, events: vec![] }
            })
        })
        .unwrap();
    let pump = |cx: &mut HeadlessAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    let browser = window.update(&mut cx, |a, _, _| a.browsers.view("web").cloned().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while cx.update(|cx| browser.read(cx).frame_seq()).is_none() || (real && cx.update(|cx| browser.read(cx).state.loading)) {
        assert!(Instant::now() < deadline, "the page never painted");
        pump(&mut cx);
        std::thread::sleep(Duration::from_millis(10));
    }
    let run = |cx: &mut HeadlessAppContext, name: &str, a: serde_json::Value| -> Result<serde_json::Value, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let step = window.update(cx, |w, _, cx| w.browsers.run(name, &a, &|_| None, Box::new(move |r| { let _ = tx.send(r); }), cx)).unwrap();
        assert!(matches!(step, browser::agent::Step::Done), "{name} is answered by the agent");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(r) = rx.try_recv() {
                return r;
            }
            assert!(Instant::now() < deadline, "{name}: no answer");
            pump(cx);
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let id = serde_json::json!({ "id": "web" });
    let shot = dir.join("page.png");
    let shot_r = run(&mut cx, "browser.screenshot", serde_json::json!({ "id": "web", "path": shot }));
    if real {
        // Type through the page's input (a click to focus the textarea, then text as the
        // platform's text input delivers it) and read it back from the page.
        let at = point(px(200.), px(60.));
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }), cx);
            window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Modifiers::default(), click_count: 1 }), cx);
        })
        .unwrap();
        pump(&mut cx);
        cx.update_window(window.into(), |_, window, cx| {
            let input = browser.read(cx).input().clone();
            input.update(cx, |i, cx| i.replace_text_in_range(None, "typed in herdr", window, cx));
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let value = loop {
            let v = run(&mut cx, "browser.eval", serde_json::json!({ "id": "web", "expr": "document.querySelector('textarea').value" })).unwrap();
            if v["value"] == "typed in herdr" || Instant::now() > deadline {
                break v;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(value["value"], "typed in herdr", "the page's textarea got the typed text: {value}");
        let shot_r = shot_r.unwrap();
        let png = std::fs::read(&shot).expect("the screenshot was written");
        assert!(png.starts_with(b"\x89PNG"), "a PNG at {}", shot.display());
        // A link to a new window: the host asks, the block tells its window (which splits).
        let js = "let a=document.createElement('a');a.href='data:text/html,popup';a.target='_blank';document.body.append(a);a.click();true";
        run(&mut cx, "browser.eval", serde_json::json!({ "id": "web", "expr": js })).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !window.update(&mut cx, |a, _, _| a.events.iter().any(|e| matches!(e, browser::view::BrowserEvent::NewWindow(u) if u == "data:text/html,popup"))).unwrap() {
            assert!(Instant::now() < deadline, "no NewWindow: {:?}", window.update(&mut cx, |a, _, _| a.events.clone()).unwrap());
            pump(&mut cx);
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("agent passed (real CEF): target=_blank link reached the window as NewWindow");
        println!("agent passed (real CEF): typed text read back by browser.eval; browser.screenshot wrote {} ({} bytes) {shot_r}", shot.display(), png.len());
    } else {
        let v = run(&mut cx, "browser.eval", serde_json::json!({ "id": "web", "expr": "1+1" })).unwrap();
        assert_eq!(v["value"]["expr"], "1+1", "{v}");
        assert!(shot_r.unwrap_err().contains("no screenshots"), "the stub's screenshot error comes back");
        let e = run(&mut cx, "browser.eval", serde_json::json!({ "id": "nope", "expr": "1" })).unwrap_err();
        assert!(e.contains("no browser"), "{e}");
        // No id: the focused browser; none focused yet, so the command says so.
        let e = run(&mut cx, "browser.reload", serde_json::json!({})).unwrap_err();
        assert!(e.contains("focused"), "{e}");
        assert!(run(&mut cx, "browser.reload", id.clone()).is_ok());
        // A new-window request from the host reaches the window as an event.
        cx.update(|cx| browser.update(cx, |v, cx| v.on_host(HostToApp::NewWindow { id: "web".into(), url: "https://x.test/".into(), disposition: "new_foreground_tab".into() }, cx)));
        cx.run_until_parked();
        let evs = window.update(&mut cx, |a, _, _| a.events.clone()).unwrap();
        assert!(evs.iter().any(|e| matches!(e, browser::view::BrowserEvent::NewWindow(u) if u == "https://x.test/")), "{evs:?}");
        println!("agent passed (stub): eval answered, screenshot error relayed, unknown and unfocused targets refused");
    }
    // The block goes: the agent's cdp.json forgets it.
    window.update(&mut cx, |w, window, cx| w.browsers.sync(vec![], window, cx, |_: &mut Agents, _, _, _, _| {})).unwrap();
    drop(browser);
    cx.update_window(window.into(), |_, window, _| window.remove_window()).unwrap();
    cx.run_until_parked();
}

fn agents() {
    match std::env::var_os("HERDR_BROWSER_HOST") {
        Some(p) => agent(p.into(), vec!["--stub".into()], "https://example.com", false),
        None => println!("HERDR_BROWSER_HOST unset: stub agent test skipped"),
    }
    match std::env::var_os("HERDR_BROWSER_CEF_HOST") {
        Some(p) => agent(p.into(), vec![], "data:text/html,<body style='margin:0'><textarea style='width:300px;height:100px'></textarea></body>", true),
        None => println!("HERDR_BROWSER_CEF_HOST unset: real CEF agent test skipped"),
    }
}

fn main() {
    agents();
    stub_input();
    stub_frames();
    cef_frames();
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-shot", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let seen = fake_host(dir.join("host.sock"));
    let link = HostClient::new(HostConfig { dir, program: "/nonexistent".into(), args: vec![], cdp_port: None });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(gpui_base::init);
    let block = layout::BrowserBlock { url: "https://example.com".into(), profile: "default".into() };
    let states = Arc::new(Mutex::new(HashMap::new()));
    let window = cx
        .open_window(size(px(480.), px(300.)), |window, cx| {
            let browser = cx.new(|cx| BrowserView::new("web", block, link.clone(), states.clone(), window, cx));
            cx.new(|_| Scene { browser, left: 120. })
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
    // Words typed into the address bar and Enter: the page goes to a search for them.
    let browser = window.update(&mut cx, |scene, _, _| scene.browser.clone()).unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        let address = browser.read(cx).address().clone();
        address.update(cx, |a, cx| {
            a.set_value("cef osr", window, cx);
            cx.emit(InputEvent::PressEnter { secondary: false, shift: false });
        });
    })
    .unwrap();
    cx.run_until_parked();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.lock().unwrap().iter().any(|m| matches!(m, AppToHost::Navigate { id, url } if id == "web" && url == "https://www.google.com/search?q=cef osr")) {
        assert!(Instant::now() < deadline, "Enter in the address bar didn't navigate: {:?}", seen.lock().unwrap());
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    // cmd-L from the page puts the cursor in the address bar.
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        let _ = window.draw(cx);
        window.dispatch_action(Box::new(browser::view::FocusAddress), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let focused = cx.update_window(window.into(), |_, window, cx| browser.read(cx).address().read(cx).focus_handle(cx).is_focused(window)).unwrap();
    assert!(focused, "FocusAddress (cmd-L) focuses the address bar");
    drop(browser);
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
