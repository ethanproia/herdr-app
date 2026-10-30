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

fn pages() -> Pages {
    Pages::start(vec![("a", solid("A", "#3366cc")), ("b", solid("B", "#cc6633")), ("input", INPUT_PAGE.to_string())])
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
    // Loading(false) can arrive before the renderer takes input; one eval round trip proves it is live.
    let (_, live) = h.eval("b1", 90, "[document.hasFocus(), document.visibilityState, innerWidth, innerHeight]");
    assert_eq!(live, serde_json::json!([true, "visible", 200, 100]));
    println!("page live: focused, visible, 200x100 CSS px");
    let mods = Modifiers::default();
    let mouse = |kind| InputEvent::Mouse { kind, x: 10.0, y: 20.0, button: MouseButton::Left, clicks: 1, mods };
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Move) });
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Down) });
    h.send(AppToHost::Input { id: "b1".into(), event: mouse(MouseKind::Up) });
    let e = page_event(&mut h, "mousedown");
    assert_eq!(e, serde_json::json!({"x": 10, "y": 20, "b": 0}));
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
