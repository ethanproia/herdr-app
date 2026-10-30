//! Measured interaction parity (plan section I) against the real CEF host: every event goes
//! through the app's real input path (`BrowserInput` listeners and its `EntityInputHandler`),
//! and the page's answer is read back with `browser.eval`. Prints `METRIC <id> <value> <unit> (target <t>)`.
//! Needs `HERDR_BROWSER_CEF_HOST`; offscreen, no window. Main thread, so `harness = false`.
#[path = "../src/layout.rs"]
#[allow(dead_code)]
mod layout;
#[path = "../src/browser/mod.rs"]
mod browser;
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

use browser::input::{self, BrowserInput};
use browser::view::{BrowserView, Browsers, HostClient, HostConfig};
use gpui::EntityInputHandler as _;
use gpui::{prelude::*, *};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The fixture: a form, a textarea, a paragraph to select, cursor targets and a tall page.
/// Every key and composition event the page sees is logged in `window.log`.
const PAGE: &str = r#"<!doctype html><meta charset=utf-8><body style='margin:0;font:16px/20px Helvetica'>
<input id=f style='position:absolute;left:10px;top:10px;width:300px;height:24px'>
<textarea id=t style='position:absolute;left:10px;top:44px;width:300px;height:40px'></textarea>
<p id=p style='position:absolute;left:10px;top:100px;width:320px;margin:0'>alpha beta gamma<br>second line here</p>
<div id=cur style='position:absolute;left:10px;top:150px'></div>
<div style='height:5000px'></div>
<script>
window.log=[];
for (const t of ['keydown','keyup','compositionstart','compositionupdate','compositionend'])
  addEventListener(t, e => log.push([t, e.key ?? '', e.code ?? '', e.repeat ?? false, e.shiftKey ?? false, e.data ?? '']), true);
const names=['pointer','text','grab','grabbing','ew-resize','ns-resize','nesw-resize','nwse-resize','not-allowed','crosshair','copy','alias','context-menu','vertical-text','default'];
names.forEach((c,i)=>{const d=document.createElement('div');d.id='c-'+c;d.style.cssText=`position:absolute;left:${i*20}px;top:0;width:20px;height:20px;cursor:${c}`;cur.append(d)});
</script>"#;

struct Scene {
    browsers: Browsers,
}

impl Render for Scene {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().bg(rgb(0x1e1e1e)).child(div().w(px(100.))).child(div().flex_1().h_full().children(self.browsers.view("web").cloned()))
    }
}

struct S {
    cx: HeadlessAppContext,
    window: WindowHandle<Scene>,
    browser: Entity<BrowserView>,
    input: Entity<BrowserInput>,
    fails: Vec<String>,
}

fn metric(id: &str, value: impl std::fmt::Display, unit: &str, target: &str) {
    println!("METRIC {id} {value} {unit} (target {target})");
}

impl S {
    fn pump(&mut self) {
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let _ = window.draw(cx);
            })
            .unwrap();
        self.cx.run_until_parked();
    }

    fn settle(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn eval(&mut self, expr: &str) -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        let a = json!({ "id": "web", "expr": expr });
        self.window.update(&mut self.cx, |s, _, cx| s.browsers.run("browser.eval", &a, &|_| None, Box::new(move |r| { let _ = tx.send(r); }), cx)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(r) = rx.try_recv() {
                return r.unwrap_or_else(|e| panic!("eval {expr}: {e}"))["value"].clone();
            }
            assert!(Instant::now() < deadline, "eval {expr}: no answer");
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Polls `expr` until it equals `want` (or 3 s); returns what it read and the time it took.
    fn until(&mut self, expr: &str, want: &Value) -> (Value, Duration) {
        let start = Instant::now();
        loop {
            let v = self.eval(expr);
            if &v == want || start.elapsed() > Duration::from_secs(3) {
                return (v, start.elapsed());
            }
        }
    }

    fn check(&mut self, id: &str, what: &str, got: &Value, want: &Value) -> bool {
        let ok = got == want;
        println!("{} {id} {what}: got {got} want {want}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.fails.push(format!("{id} {what}: got {got} want {want}"));
        }
        ok
    }

    /// A point in the page (CSS px from its top-left) in window coordinates.
    fn at(&mut self, x: f32, y: f32) -> Point<Pixels> {
        let o = self.cx.update(|cx| self.input.read(cx).origin());
        point(o.x + px(x), o.y + px(y))
    }

    fn event(&mut self, e: PlatformInput) {
        self.cx.update_window(self.window.into(), |_, window, cx| { window.dispatch_event(e, cx); }).unwrap();
        self.pump();
    }

    fn click(&mut self, x: f32, y: f32, clicks: usize) {
        let p = self.at(x, y);
        for n in 1..=clicks {
            self.event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: n, first_mouse: false }));
            self.event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: n }));
        }
    }

    /// Keystrokes as GPUI delivers them (`dispatch_keystroke`: key-down, then text through the input handler).
    fn keys(&mut self, keys: &str) {
        for k in keys.split(' ') {
            let ks = Keystroke::parse(k).unwrap();
            self.cx.update_window(self.window.into(), |_, window, cx| { window.dispatch_keystroke(ks.clone(), cx); }).unwrap();
            self.event(PlatformInput::KeyUp(KeyUpEvent { keystroke: ks }));
        }
    }

    /// Calls the platform text input the way macOS does (through the `EntityInputHandler`).
    fn ime(&mut self, f: impl FnOnce(&mut BrowserInput, &mut Window, &mut Context<BrowserInput>)) {
        let input = self.input.clone();
        self.cx.update_window(self.window.into(), |_, window, cx| input.update(cx, |i, cx| f(i, window, cx))).unwrap();
        self.pump();
    }

    /// Client rect of the first `len` chars from `start` in the paragraph's first text node.
    fn text_rect(&mut self, start: u32, len: u32) -> (f32, f32, f32, f32) {
        let v = self.eval(&format!("(()=>{{const r=document.createRange(),n=p.firstChild;r.setStart(n,{start});r.setEnd(n,{});const b=r.getBoundingClientRect();return [b.left,b.top,b.width,b.height]}})()", start + len));
        let f = |i: usize| v[i].as_f64().unwrap() as f32;
        (f(0), f(1), f(2), f(3))
    }
}

fn open(program: std::path::PathBuf) -> S {
    let dir = std::path::PathBuf::from(format!("/tmp/hb-{}-input", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = std::env::temp_dir().join(format!("hb-{}-input.html", std::process::id()));
    std::fs::write(&page, PAGE).unwrap();
    let link = HostClient::new(HostConfig { dir, program, args: vec![], cdp_port: Some(0) });
    let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(gpui_base::init);
    let block = layout::BrowserBlock { url: format!("file://{}", page.display()), profile: "default".into() };
    let window = cx
        .open_window(size(px(520.), px(360.)), |window, cx| {
            cx.new(|cx| {
                let mut browsers = Browsers::with_link(link.clone());
                browsers.sync(vec![("web".into(), block)], window, cx, |_: &mut Scene, _, _, _, _| {});
                Scene { browsers }
            })
        })
        .unwrap();
    let browser = window.update(&mut cx, |s, _, _| s.browsers.view("web").cloned().unwrap()).unwrap();
    let input = cx.update(|cx| browser.read(cx).input().clone());
    let mut s = S { cx, window, browser, input, fails: vec![] };
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.cx.update(|cx| s.browser.read(cx).frame_seq()).is_none() || s.cx.update(|cx| s.browser.read(cx).state.loading) {
        assert!(Instant::now() < deadline, "the page never painted");
        s.settle(10);
    }
    s
}

/// I1: ASCII by keystrokes, accented text from the layout, a dead key (option-e e) as macOS composes it.
fn i1(s: &mut S) {
    s.click(20., 20., 1);
    let start = Instant::now();
    s.keys("H e l l o , space w o r l d");
    let (v, _) = s.until("f.value", &json!("Hello, world"));
    s.check("I1", "ascii typed into <input>", &v, &json!("Hello, world"));
    // Includes the offscreen pump and one eval round trip per poll.
    metric("I1.ascii_12_keys_to_value", start.elapsed().as_millis(), "ms", "<= 500");
    let codes = s.eval("log.filter(e=>e[0]=='keydown').slice(0,2).map(e=>e[1]+'/'+e[2])");
    s.check("I1", "page keydown key/code", &codes, &json!(["H/KeyH", "e/KeyE"]));
    s.ime(|i, w, cx| i.replace_text_in_range(None, " ñü", w, cx));
    s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, "´", None, w, cx));
    s.ime(|i, w, cx| i.replace_text_in_range(None, "é", w, cx));
    let (v, _) = s.until("f.value", &json!("Hello, world ñüé"));
    s.check("I1", "accented and dead-key text", &v, &json!("Hello, world ñüé"));
    s.eval("log=[];1");
    s.keys("backspace");
    if std::env::var_os("HERDR_INPUT_DEBUG").is_some() {
        s.settle(200);
        println!("debug backspace log {}", s.eval("JSON.stringify(log)"));
    }
    let (v, _) = s.until("f.value", &json!("Hello, world ñü"));
    s.check("I1", "backspace", &v, &json!("Hello, world ñü"));
}

/// I2: Japanese, Chinese and Korean compositions through the input handler, and the candidate window under the caret.
fn i2(s: &mut S) {
    s.click(40., 60., 1);
    s.eval("t.value='';log=[];1");
    for step in ["に", "にほ", "にほん"] {
        s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, step, None, w, cx));
    }
    let (v, _) = s.until("t.value", &json!("にほん"));
    s.check("I2", "japanese marked text shows in the page", &v, &json!("にほん"));
    s.ime(|i, w, cx| i.replace_text_in_range(None, "日本", w, cx));
    s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, "zhong", None, w, cx));
    s.ime(|i, w, cx| i.replace_text_in_range(None, "中", w, cx));
    for (marks, done) in [(&["ㅎ", "하", "한"][..], "한"), (&["ㄱ", "그", "글"][..], "글")] {
        for m in marks {
            s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, m, None, w, cx));
        }
        s.ime(|i, w, cx| i.replace_text_in_range(None, done, w, cx));
    }
    let want = json!("日本中한글");
    let (v, _) = s.until("t.value", &want);
    s.check("I2", "japanese, chinese, korean committed", &v, &want);
    let ends = s.eval("log.filter(e=>e[0]=='compositionend').map(e=>e[5])");
    s.check("I2", "compositionend data", &ends, &json!(["日本", "中", "한", "글"]));
    // A composition that is abandoned leaves nothing behind.
    s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, "か", None, w, cx));
    s.ime(|i, w, cx| i.unmark_text(w, cx));
    let (v, _) = s.until("t.value", &want);
    s.check("I2", "cancelled composition leaves the text", &v, &want);
    // The candidate window: the caret the host reports sits inside the textarea, in window coordinates.
    s.ime(|i, w, cx| i.replace_and_mark_text_in_range(None, "て", None, w, cx));
    s.settle(150);
    let caret = s.cx.update(|cx| s.input.read(cx).caret());
    let area = s.at(10., 44.);
    let inside = caret.is_some_and(|c| c.origin.x >= area.x && c.origin.x <= area.x + px(304.) && c.origin.y >= area.y && c.origin.y <= area.y + px(44.));
    s.check("I2", "candidate window caret inside the textarea", &json!(inside), &json!(true));
    if let Some(c) = caret {
        metric("I2.caret_x_from_textarea", f32::from(c.origin.x - area.x), "px", "0..304");
    }
    s.ime(|i, w, cx| i.unmark_text(w, cx));
}

/// I3 and I4: double-click selects a word, triple-click the line, a drag selects the range dragged over.
fn i3_i4(s: &mut S) {
    let (x, y, w, h) = s.text_rect(6, 4);
    s.click(x + w / 2., y + h / 2., 1);
    s.click(x + w / 2., y + h / 2., 2);
    let (v, _) = s.until("getSelection().toString()", &json!("beta"));
    s.check("I3", "double-click selects the word", &v, &json!("beta"));
    s.click(x + w / 2., y + h / 2., 3);
    let (v, _) = s.until("getSelection().toString().trim()", &json!("alpha beta gamma"));
    s.check("I3", "triple-click selects the line", &v, &json!("alpha beta gamma"));
    s.click(5., 300., 1);
    let (ax, ay, _, ah) = s.text_rect(0, 1);
    let (gx, _, gw, _) = s.text_rect(11, 5);
    let (from, to) = (s.at(ax + 1., ay + ah / 2.), s.at(gx + gw - 1., ay + ah / 2.));
    s.event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: from, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
    for n in 1..=8 {
        let p = point(from.x + (to.x - from.x) * (n as f32 / 8.), from.y);
        s.event(PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() }));
    }
    s.event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: to, modifiers: Modifiers::default(), click_count: 1 }));
    let (v, _) = s.until("getSelection().toString()", &json!("alpha beta gamma"));
    s.check("I4", "drag selects what it crosses", &v, &json!("alpha beta gamma"));
}

fn main() {
    let Some(program) = std::env::var_os("HERDR_BROWSER_CEF_HOST") else {
        println!("HERDR_BROWSER_CEF_HOST unset: interaction parity skipped");
        return;
    };
    let only = std::env::var("HERDR_INPUT_ONLY").unwrap_or_default();
    let mut s = open(program.into());
    let run = |id: &str| only.is_empty() || only.split(',').any(|o| o == id);
    if run("I1") {
        i1(&mut s);
    }
    if run("I2") {
        i2(&mut s);
    }
    if run("I3") {
        i3_i4(&mut s);
    }
    let _ = input::cursor_style;
    metric("I.fails", s.fails.len(), "count", "0");
    s.cx.update_window(s.window.into(), |_, window, _| window.remove_window()).unwrap();
    s.cx.run_until_parked();
    assert!(s.fails.is_empty(), "interaction checks failed:\n{}", s.fails.join("\n"));
}
