//! Real CEF host only (`HERDR_BROWSER_HOST=<bundle exe>`; skipped otherwise). Every page is served
//! locally. Frames are read back from the shared IOSurface, as the app will paint them.

mod common;
use common::*;
use herdr_browser_protocol::*;
use std::io::{Read, Write};
use std::time::Duration;

const SEC: Duration = Duration::from_secs(1);
const BLUE: [u8; 4] = [0xcc, 0x66, 0x33, 0xff]; // #3366cc as BGRA
const ORANGE: [u8; 4] = [0x33, 0x66, 0xcc, 0xff]; // #cc6633 as BGRA

const INPUT_PAGE: &str = r#"<!doctype html><html><head><title>input</title></head><body style="margin:0">
<textarea id=t autofocus style="display:block;width:100vw;height:100vh;border:0;margin:0;padding:0"></textarea>
<script>
const log=(k,o)=>console.log("ev "+k+" "+JSON.stringify(o));
addEventListener("keydown",e=>log("keydown",{key:e.key,code:e.code}),true);
addEventListener("mousedown",e=>log("mousedown",{x:e.clientX,y:e.clientY,b:e.button}),true);
addEventListener("wheel",e=>log("wheel",{dy:Math.sign(e.deltaY)}),{capture:true,passive:true});
t.addEventListener("compositionupdate",e=>log("compositionupdate",{data:e.data}));
t.addEventListener("compositionend",e=>log("compositionend",{data:e.data}));
t.addEventListener("input",()=>log("input",{value:t.value}));
</script></body></html>"#;

/// A hostile page: prints what the old console eval channel accepted as results (SEC3).
const FORGE_PAGE: &str = r#"<!doctype html><html><head><title>forge</title></head><body><script>
const forge=()=>{for(let r=0;r<20;r++){console.log('__herdr_eval__:'+r+':{"ok":true,"v":"forged"}')}};
forge();setInterval(forge,50);
</script></body></html>"#;

/// A `<select>` whose dropdown list is green: the list is a CEF popup widget (PET_POPUP), not page pixels.
const SELECT_PAGE: &str = r#"<!doctype html><html><head><title>select</title><style>
body{margin:0;background:#fff} select{position:absolute;left:10px;top:10px;width:120px;font-size:16px}
option{background:#00ff00;color:#00ff00}
</style></head><body><select id=s><option value=a>a</option><option value=b>b</option><option value=c>c</option></select></body></html>"#;

/// Logs every prefers-color-scheme change, so a test sees it update live.
const SCHEME_PAGE: &str = r#"<!doctype html><html><head><title>scheme</title></head><body><script>
const q=matchMedia('(prefers-color-scheme: dark)');
q.addEventListener('change',e=>console.log('ev scheme '+JSON.stringify({dark:e.matches})));
</script></body></html>"#;

/// A link at the top, a filled text box below it: two different context menus.
const MENU_PAGE: &str = r#"<!doctype html><html><head><title>menu</title></head><body style="margin:0">
<a id=l href="/b" style="display:block;height:30px;font-size:20px">a link</a>
<textarea id=t style="display:block;width:200px;height:60px;margin:0;border:0">hello menu</textarea>
</body></html>"#;

/// Known geometry: a box at (20, 30) 100x50, three items, a tall page to scroll.
const GEO_PAGE: &str = r#"<!doctype html><html><head><title>geo</title><style>
body{margin:0;height:3000px} #box{position:absolute;left:20px;top:30px;width:100px;height:50px;background:#36c}
.item{position:absolute;left:200px;width:40px;height:10px} #p{position:absolute;left:20px;top:120px;font-size:16px}
</style></head><body><div id=box></div><div class=item style="top:0"></div><div class=item style="top:20px"></div>
<div class=item style="top:40px"></div><p id=p>select me please</p></body></html>"#;

fn pages() -> Pages {
    Pages::start(vec![
        ("a", solid("A", "#3366cc")),
        ("b", solid("B", "#cc6633")),
        ("input", INPUT_PAGE.to_string()),
        ("forge", FORGE_PAGE.to_string()),
        ("select", SELECT_PAGE.to_string()),
        ("scheme", SCHEME_PAGE.to_string()),
        ("menu", MENU_PAGE.to_string()),
        ("geo", GEO_PAGE.to_string()),
    ])
}

fn loaded(h: &mut Host) {
    h.until(15 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, .. }));
}

/// The next `ev <kind> <json>` console line from the page.
fn page_event(h: &mut Host, kind: &str) -> serde_json::Value {
    let prefix = format!("ev {kind} ");
    match h.until(10 * SEC, |m| matches!(m, HostToApp::Console { message, .. } if message.starts_with(&prefix))) {
        HostToApp::Console { message, .. } => serde_json::from_str(&message[prefix.len()..]).unwrap(),
        _ => unreachable!(),
    }
}

#[test]
fn page_pixels_resize_and_navigate() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let dir = scratch("cef-pixels");
    let mut h = Host::start_in(&dir);
    match &h.ready {
        HostToApp::Ready { cef_version, .. } => {
            assert!(cef_version.starts_with("156."), "{cef_version}");
            println!("ready: CEF {cef_version}");
        }
        m => panic!("{m:?}"),
    }
    create(&mut h, "b1", &p.url("a"), 300.0, 200.0, 2.0);
    let (seq, px) = h.frame_where("b1", 300, 200, |px| px == BLUE);
    println!("frame {seq}: 600x400 px, centre pixel BGRA {px:02x?} = #3366cc");
    h.send(AppToHost::FrameAck { id: "b1".into(), seq });

    h.send(AppToHost::Resize { id: "b1".into(), size: Size { w: 160.0, h: 90.0 }, scale: 1.0 });
    let (seq, surface) = h.frame_of_size("b1", PixelSize { w: 160, h: 90 });
    let corner = herdr_browser_surface::import(&surface).unwrap().pixel(159, 89).unwrap();
    assert_eq!(corner, BLUE);
    println!("resize: frame {seq} is 160x90, corner pixel {corner:02x?}");
    h.send(AppToHost::FrameAck { id: "b1".into(), seq });

    h.send(AppToHost::Navigate { id: "b1".into(), url: p.url("b") });
    h.until(10 * SEC, |m| matches!(m, HostToApp::Title { title, .. } if title == "B"));
    let (seq, px) = h.frame_where("b1", 80, 45, |px| px == ORANGE);
    println!("navigate: title B, frame {seq} centre pixel {px:02x?} = #cc6633");

    let shot = dir.join("shot.png");
    h.send(AppToHost::Screenshot { id: "b1".into(), req: 5, path: shot.display().to_string() });
    let done = h.until(10 * SEC, |m| matches!(m, HostToApp::ScreenshotDone { .. }));
    assert_eq!(done, HostToApp::ScreenshotDone { id: "b1".into(), req: 5, ok: true, error: None });
    let png = std::fs::read(&shot).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    println!("screenshot: {} bytes at {}", png.len(), shot.display());
    h.shutdown();
}

#[test]
fn page_sees_mouse_keys_wheel_and_ime() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-input");
    create(&mut h, "b1", &p.url("input"), 200.0, 100.0, 1.0);
    loaded(&mut h);
    h.send(AppToHost::Focus { id: "b1".into(), focused: true });
    // Input goes out the instant Loading(false) arrives: the host must queue it, not drop it.
    let mods = Modifiers::default();
    let mouse = |kind| InputEvent::Mouse { kind, x: 10.0, y: 20.0, button: MouseButton::Left, clicks: 1, mods };
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Move) });
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Down) });
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Up) });
    let e = page_event(&mut h, "mousedown");
    assert_eq!(e, serde_json::json!({"x": 10, "y": 20, "b": 0}));
    let (_, live) = h.eval("b1", 90, "[document.hasFocus(), document.visibilityState, innerWidth, innerHeight]");
    assert_eq!(live, serde_json::json!([true, "visible", 200, 100]));
    println!("mousedown -> page saw {e}");

    let key = |kind| InputEvent::Key { kind, key: "a".into(), code: "KeyA".into(), native: 0, repeat: false, mods };
    h.send(AppToHost::Input { id: "b1".into(), event: key(KeyKind::Down) });
    h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Text { text: "a".into() } });
    h.send(AppToHost::Input { id: "b1".into(), event: key(KeyKind::Up) });
    let e = page_event(&mut h, "keydown");
    assert_eq!(e["code"], "KeyA");
    println!("key a -> page saw keydown {e}");
    let e = page_event(&mut h, "input");
    assert_eq!(e["value"], "a");
    println!("text a -> textarea value {e}");

    let set = ImeEvent::Set { text: "にほん".into(), sel_start: 3, sel_end: 3, replace_start: None, replace_end: None };
    h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(set) });
    let e = page_event(&mut h, "compositionupdate");
    assert_eq!(e["data"], "にほん");
    println!("ime set -> compositionupdate {e}");
    h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(ImeEvent::Commit { text: "日本".into() }) });
    let e = page_event(&mut h, "compositionend");
    assert_eq!(e["data"], "日本");
    println!("ime commit -> compositionend {e}");
    let (ok, v) = h.eval("b1", 7, "t.value");
    assert!(ok);
    assert_eq!(v, "a日本");
    println!("textarea now {v}");

    h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Wheel { x: 50.0, y: 50.0, dx: 0.0, dy: -3.0, precise: false, mods } });
    let e = page_event(&mut h, "wheel");
    assert_eq!(e["dy"], 1);
    println!("wheel dy=-3 lines -> page saw {e} (CEF sign: negative delta scrolls down)");
    h.shutdown();
}

#[test]
fn eval_values_dialogs_and_cdp() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-eval");
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    loaded(&mut h);
    assert_eq!(h.eval("b1", 1, "1+1"), (true, serde_json::json!(2)));
    assert_eq!(h.eval("b1", 2, "Promise.resolve({n: document.title})"), (true, serde_json::json!({"n": "A"})));
    let (ok, err) = h.eval("b1", 3, "nope()");
    assert!(!ok && err.as_str().unwrap().contains("ReferenceError"), "{err}");
    println!("eval: 1+1=2, promise resolved, error reported as {err}");

    h.send(AppToHost::Eval { id: "b1".into(), req: 4, expr: "setTimeout(()=>{window.r=confirm('sure?')},0)".into() });
    let req = match h.until(10 * SEC, |m| matches!(m, HostToApp::Dialog { .. })) {
        HostToApp::Dialog { req, kind, message, .. } => {
            assert_eq!((kind, message.as_str()), (DialogKind::Confirm, "sure?"));
            req
        }
        _ => unreachable!(),
    };
    h.send(AppToHost::DialogResult { id: "b1".into(), req, accept: true, text: None });
    std::thread::sleep(SEC / 2);
    assert_eq!(h.eval("b1", 5, "window.r"), (true, serde_json::json!(true)));
    println!("confirm('sure?') -> Dialog, accepted -> page got true");

    let port = match h.ready {
        HostToApp::Ready { cdp_port: Some(p), .. } => p,
        ref m => panic!("no CDP port: {m:?}"),
    };
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(5 * SEC)).unwrap();
    write!(s, "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    // Chromium's DevTools server keeps the connection open; read until the page shows up.
    let mut body = String::new();
    let mut buf = [0u8; 4096];
    while !body.contains(&p.url("a")) {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
    }
    assert!(body.contains(&p.url("a")), "{body}");
    println!("CDP port {port}: /json/list lists the page");
    h.shutdown();
}

#[test]
fn cookie_survives_a_host_restart() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let dir = scratch("cef-cookie");
    let mut h = Host::start_in(&dir);
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    loaded(&mut h);
    let (ok, v) = h.eval("b1", 1, "document.cookie='k=v1; max-age=3600; path=/'; document.cookie");
    assert_eq!((ok, v.clone()), (true, serde_json::json!("k=v1")));
    println!("first host: set cookie, page reads {v}");
    h.shutdown();

    let mut h = Host::start_in(&dir);
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    loaded(&mut h);
    let (ok, v) = h.eval("b1", 2, "document.cookie");
    assert_eq!((ok, v.clone()), (true, serde_json::json!("k=v1")));
    println!("second host, same profile: page reads {v}");
    h.shutdown();
}

#[test]
fn page_console_cannot_forge_eval_results() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-forge");
    create(&mut h, "b1", &p.url("forge"), 100.0, 50.0, 1.0);
    loaded(&mut h);
    // The page keeps printing fake results for requests 0..20; none may come back as EvalResult.
    let end = std::time::Instant::now() + 2 * SEC;
    let mut forged_lines = 0;
    while let Some(m) = h.next(end.saturating_duration_since(std::time::Instant::now())) {
        match m {
            HostToApp::EvalResult { .. } => panic!("a page console line became an EvalResult: {m:?}"),
            HostToApp::Console { message, .. } if message.starts_with("__herdr_eval__:") => forged_lines += 1,
            HostToApp::Frame { id, seq, .. } => h.send(AppToHost::FrameAck { id, seq }),
            _ => {}
        }
    }
    assert!(forged_lines > 0, "the fixture page did not run");
    // A real eval with one of the forged request ids still gets the true answer.
    assert_eq!(h.eval("b1", 5, "6*7"), (true, serde_json::json!(42)));
    println!("forge page printed {forged_lines} fake result lines: all stayed Console; eval req 5 = 42");
    h.shutdown();
}

#[test]
fn select_dropdown_paints_and_takes_input() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-select");
    create(&mut h, "b1", &p.url("select"), 300.0, 200.0, 1.0);
    loaded(&mut h);
    assert_eq!(h.eval("b1", 1, "s.value"), (true, serde_json::json!("a")));
    let mods = Modifiers::default();
    let click = |kind| InputEvent::Mouse { kind, x: 40.0, y: 18.0, button: MouseButton::Left, clicks: 1, mods };
    h.send(AppToHost::Focus { id: "b1".into(), focused: true });
    h.send(AppToHost::Input { id: "b1".into(), event: click(MouseKind::Down) });
    h.send(AppToHost::Input { id: "b1".into(), event: click(MouseKind::Up) });
    // The open list sits just below the select (CEF puts it at y = 31). Chromium draws the popup
    // list in its own style (option colours do not apply), so check it covers the white page.
    let green = |px: [u8; 4]| px != [0xff, 0xff, 0xff, 0xff];
    if std::env::var_os("HERDR_TEST_TRACE").is_some() {
        std::thread::sleep(SEC);
        let shot = scratch("cef-select-shot").join("open.png");
        h.send(AppToHost::Screenshot { id: "b1".into(), req: 77, path: shot.display().to_string() });
        h.until(10 * SEC, |m| matches!(m, HostToApp::ScreenshotDone { .. }));
        eprintln!("trace: screenshot {} ; focus {:?}", shot.display(), h.eval("b1", 78, "document.activeElement.id"));
    }
    let (seq, px) = h.frame_where("b1", 15, 33, green);
    println!("dropdown open: frame {seq}, pixel under the select {px:02x?} (the list, not page white)");
    h.send(AppToHost::FrameAck { id: "b1".into(), seq });
    // Pick the second option with the keyboard, as a person would.
    for (key, code, native) in [("ArrowDown", "ArrowDown", 125), ("Enter", "Enter", 36)] {
        for kind in [KeyKind::Down, KeyKind::Up] {
            h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Key { kind, key: key.into(), code: code.into(), native, repeat: false, mods } });
        }
    }
    let (seq, px) = h.frame_where("b1", 15, 33, |px| px == [0xff, 0xff, 0xff, 0xff]);
    println!("dropdown closed: frame {seq}, pixel below the select back to white {px:02x?}");
    std::thread::sleep(SEC / 4);
    assert_eq!(h.eval("b1", 2, "s.value"), (true, serde_json::json!("b")));
    println!("select value is now b");
    h.shutdown();
}

#[test]
fn color_scheme_follows_the_app_live() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-scheme");
    h.send(AppToHost::SetColorScheme { dark: false });
    create(&mut h, "b1", &p.url("scheme"), 100.0, 50.0, 1.0);
    loaded(&mut h);
    let dark = "matchMedia('(prefers-color-scheme: dark)').matches";
    assert_eq!(h.eval("b1", 1, dark), (true, serde_json::json!(false)));
    h.send(AppToHost::SetColorScheme { dark: true });
    assert_eq!(page_event(&mut h, "scheme"), serde_json::json!({"dark": true}));
    assert_eq!(h.eval("b1", 2, dark), (true, serde_json::json!(true)));
    println!("SetColorScheme dark -> page saw the change live, matchMedia dark = true");
    // A browser made later starts in the current scheme, in a named profile too.
    h.send(AppToHost::Create { id: "b2".into(), url: Some(p.url("scheme")), size: Size { w: 100.0, h: 50.0 }, scale: 1.0, profile: Some("work".into()) });
    // A new profile starts cold (slow on a first run): give it time.
    h.until(40 * SEC, |m| matches!(m, HostToApp::Loading { id, loading: false, .. } if id == "b2"));
    assert_eq!(h.eval("b2", 3, dark), (true, serde_json::json!(true)));
    h.send(AppToHost::SetColorScheme { dark: false });
    // Both pages flip; wait for each to say so (b2 may still report its own start in dark first).
    let mut flipped = std::collections::HashSet::new();
    while flipped.len() < 2 {
        if let HostToApp::Console { id, .. } = h.until(10 * SEC, |m| matches!(m, HostToApp::Console { message, .. } if message == r#"ev scheme {"dark":false}"#)) {
            flipped.insert(id);
        }
    }
    println!("new browser in profile work starts dark; back to light live");
    h.shutdown();
}

#[test]
fn profiles_keep_cookies_and_storage_apart_and_across_restarts() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let dir = scratch("cef-profiles");
    let make = |h: &mut Host, id: &str, profile: Option<&str>| {
        h.send(AppToHost::Create { id: id.into(), url: Some(p.url("a")), size: Size { w: 100.0, h: 50.0 }, scale: 1.0, profile: profile.map(Into::into) });
        let id = id.to_string();
        h.until(15 * SEC, |m| matches!(m, HostToApp::Loading { id: i, loading: false, .. } if *i == id));
    };
    let mut h = Host::start_in(&dir);
    make(&mut h, "w", Some("work"));
    make(&mut h, "d", None);
    let set = "document.cookie='who=work; max-age=3600; path=/'; document.cookie='sess=1; path=/'; localStorage.setItem('k','work'); document.cookie";
    assert_eq!(h.eval("w", 1, set).0, true);
    assert_eq!(h.eval("d", 2, "[document.cookie, localStorage.getItem('k')]"), (true, serde_json::json!(["", null])));
    println!("profile work set cookies and localStorage; the default profile sees none");
    h.shutdown();

    let mut h = Host::start_in(&dir);
    make(&mut h, "w", Some("work"));
    let (ok, v) = h.eval("w", 3, "[document.cookie.split('; ').sort().join('; '), localStorage.getItem('k')]");
    assert!(ok);
    assert_eq!(v, serde_json::json!(["sess=1; who=work", "work"]));
    println!("after a host restart, profile work still has {v} (persistent and session cookie, localStorage)");
    h.shutdown();
}

#[test]
fn one_arrow_press_moves_the_caret_once() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-arrow");
    create(&mut h, "b1", &p.url("input"), 200.0, 100.0, 1.0);
    loaded(&mut h);
    h.send(AppToHost::Focus { id: "b1".into(), focused: true });
    h.eval("b1", 1, "t.focus(); t.value='abcd'; t.setSelectionRange(4,4); 0");
    let mods = Modifiers::default();
    for kind in [KeyKind::Down, KeyKind::Up] {
        h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Key { kind, key: "ArrowLeft".into(), code: "ArrowLeft".into(), native: 123, repeat: false, mods } });
    }
    std::thread::sleep(SEC / 2);
    assert_eq!(h.eval("b1", 2, "t.selectionStart"), (true, serde_json::json!(3)));
    println!("one ArrowLeft press: caret 4 -> 3 (it moved twice before the macOS function-key characters)");
    h.shutdown();
}

fn find_item<'a>(items: &'a [MenuItem], want: &str) -> Option<&'a MenuItem> {
    items.iter().find_map(|i| if i.label.eq_ignore_ascii_case(want) { Some(i) } else { find_item(&i.submenu, want) })
}

#[test]
fn context_menus_carry_cef_items_and_extras_and_run_commands() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-menu");
    create(&mut h, "b1", &p.url("menu"), 300.0, 200.0, 1.0);
    loaded(&mut h);
    assert!(h.eval("b1", 1, "1").0);
    let mods = Modifiers::default();
    let right = |h: &mut Host, x: f32, y: f32| {
        for kind in [MouseKind::Down, MouseKind::Up] {
            h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Mouse { kind, x, y, button: MouseButton::Right, clicks: 1, mods } });
        }
        match h.until(10 * SEC, |m| matches!(m, HostToApp::ContextMenu { .. })) {
            HostToApp::ContextMenu { req, kind, link_url, items, .. } => (req, kind, link_url, items),
            _ => unreachable!(),
        }
    };
    let t0 = std::time::Instant::now();
    let (req, kind, link, items) = right(&mut h, 20.0, 12.0);
    let ms = t0.elapsed().as_millis();
    assert!(kind.contains(&"link".to_string()), "{kind:?}");
    assert_eq!(link.as_deref(), Some(p.url("b").as_str()));
    let open = find_item(&items, "Open Link in New Tab").expect("extra");
    assert!(open.command >= 26500 && find_item(&items, "Copy Link Address").is_some());
    let cef_items: Vec<&str> = items.iter().filter(|i| i.command > 0 && i.command < 26500).map(|i| i.label.as_str()).collect();
    println!("link menu: kind {kind:?}, {} items, CEF items {cef_items:?}", items.len());
    println!("METRIC context_menu_latency_ms={ms}");
    // An extra is the app's job: it answers None, the host just closes CEF's menu.
    h.send(AppToHost::MenuResult { id: "b1".into(), req, command: None });

    let (req, kind, _, items) = right(&mut h, 40.0, 50.0);
    assert!(kind.contains(&"editable".to_string()), "{kind:?}");
    let all = find_item(&items, "Select All").expect("CEF's Select All");
    assert!(all.command > 0 && all.command < 26500);
    h.send(AppToHost::MenuResult { id: "b1".into(), req, command: Some(all.command) });
    std::thread::sleep(SEC / 2);
    let (ok, v) = h.eval("b1", 2, "[t.selectionStart, t.selectionEnd]");
    assert_eq!((ok, v.clone()), (true, serde_json::json!([0, 10])));
    println!("text box menu: Select All (command {}) selected {v}", all.command);
    // A stale or foreign answer is refused, not run.
    h.send(AppToHost::MenuResult { id: "b1".into(), req, command: Some(all.command) });
    assert!(matches!(h.until(5 * SEC, |m| matches!(m, HostToApp::Error { .. }) || matches!(m, HostToApp::Error { .. })), HostToApp::Error { .. }));
    h.shutdown();
}

fn rects(h: &mut Host, req: u64, query: OverlayQuery) -> Vec<CssRect> {
    h.send(AppToHost::Overlay { id: "b1".into(), req, epoch: 7, query });
    match h.until(10 * SEC, |m| matches!(m, HostToApp::OverlayRects { req: r, .. } if *r == req)) {
        HostToApp::OverlayRects { epoch, rects, .. } => {
            assert_eq!(epoch, 7);
            rects
        }
        _ => unreachable!(),
    }
}

#[test]
fn overlay_geometry_and_viewport() {
    if real_host().is_none() {
        return;
    }
    let p = pages();
    let mut h = Host::start("cef-geo");
    create(&mut h, "b1", &p.url("geo"), 400.0, 300.0, 1.0);
    loaded(&mut h);
    assert!(h.eval("b1", 1, "1").0);
    let t0 = std::time::Instant::now();
    let hit = rects(&mut h, 10, OverlayQuery::HitTest { x: 40.0, y: 40.0 });
    println!("METRIC overlay_hit_test_ms={}", t0.elapsed().as_millis());
    assert_eq!(hit, vec![CssRect { x: 20.0, y: 30.0, w: 100.0, h: 50.0 }]);
    let items = rects(&mut h, 11, OverlayQuery::Selector { selector: ".item".into() });
    assert_eq!(items.iter().map(|r| r.y).collect::<Vec<_>>(), vec![0.0, 20.0, 40.0]);
    assert!(rects(&mut h, 12, OverlayQuery::Selector { selector: "[[bad".into() }).is_empty());
    h.eval("b1", 2, "getSelection().selectAllChildren(p); 0");
    let sel = rects(&mut h, 13, OverlayQuery::Selection);
    assert!(!sel.is_empty() && sel[0].w > 50.0 && (sel[0].y - 120.0).abs() < 25.0, "{sel:?}");
    println!("overlay: hit test box {hit:?}; 3 .item rects; bad selector -> none; selection {} rect(s)", sel.len());

    h.eval("b1", 3, "scrollTo(0, 200); 0");
    let vp = h.until(10 * SEC, |m| matches!(m, HostToApp::Viewport { scroll_y, .. } if *scroll_y == 200.0));
    println!("scroll -> {vp:?}");
    h.send(AppToHost::SetZoom { id: "b1".into(), level: 1.0 });
    let vp = h.until(10 * SEC, |m| matches!(m, HostToApp::Viewport { zoom_level, .. } if (*zoom_level - 1.0).abs() < 1e-6));
    println!("zoom -> {vp:?}");

    // SetVisible's full behaviour (dropping pending frames, a fresh frame on show) is fork-frames'
    // PR #22; this host only passes visibility to CEF, so it is not asserted here.
    h.shutdown();
}
