//! The browser parity harness: one browser block in an offscreen GPUI window, driven through
//! the app's real paths (input through `BrowserInput`, commands through the agent), against the
//! real CEF host (`HERDR_BROWSER_CEF_HOST`) or the stub (`HERDR_BROWSER_HOST`).
//! Nothing here shows a window or takes focus. API: docs/browser-parity-harness.md.

use crate::browser::agent::Step;
use crate::browser::protocol::PixelSize;
use crate::browser::view::{BrowserEvent, BrowserView, Browsers, FrameEvent, HostClient, HostConfig, CHROME_HEIGHT};
use crate::layout::BrowserBlock;
use gpui::{prelude::*, *};
use image::RgbaImage;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which host serves the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Host {
    /// The real CEF host bundle (`HERDR_BROWSER_CEF_HOST`).
    Cef,
    /// The stub host (`HERDR_BROWSER_HOST`): deterministic frames, echoes input as console lines.
    Stub,
}

impl Host {
    fn program(self) -> Option<PathBuf> {
        let var = match self {
            Host::Cef => "HERDR_BROWSER_CEF_HOST",
            Host::Stub => "HERDR_BROWSER_HOST",
        };
        std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
}

/// One frame's life: when it arrived from the host, was first painted, was acked.
#[derive(Clone, Debug)]
pub struct FrameRec {
    pub seq: u64,
    pub size: PixelSize,
    pub arrived: Instant,
    pub painted: Option<Instant>,
    pub acked: Option<Instant>,
    /// Content hash of the frame's pixels (only with `Page::hash_frames(true)`).
    pub hash: Option<u64>,
}

/// Frames in a time window.
#[derive(Clone, Debug, Default)]
pub struct FrameStats {
    pub count: usize,
    pub secs: f64,
    pub fps: f64,
    /// Gaps between arrivals, ms.
    pub p50_gap_ms: f64,
    pub p95_gap_ms: f64,
    pub max_gap_ms: f64,
    /// Distinct content hashes (when hashing).
    pub distinct: usize,
}

/// CPU and memory of the host and its helper processes.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    /// Summed `ps %cpu` (100 = one core).
    pub cpu: f64,
    pub rss_mb: f64,
    pub processes: usize,
}

struct Root {
    browsers: Browsers,
    events: Vec<BrowserEvent>,
    /// The block's size in DIPs (None: fills the window).
    block: Option<(f32, f32)>,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let v = self.browsers.view(ID).cloned();
        let el = div().children(v);
        let el = match self.block {
            Some((w, h)) => el.w(px(w)).h(px(h)),
            None => el.size_full(),
        };
        div().size_full().bg(rgb(0x1e1e1e)).child(el)
    }
}

const ID: &str = "page";

/// Print one metric line: `METRIC <id> <value> <unit> (target <t>)`.
pub fn metric(id: &str, value: f64, unit: &str, target: &str) {
    println!("METRIC {id} {value:.2} {unit} (target {target})");
}

/// A browser block showing one page, offscreen.
pub struct Page {
    pub cx: HeadlessAppContext,
    window: WindowHandle<Root>,
    pub view: Entity<BrowserView>,
    pub link: Arc<HostClient>,
    frames: Arc<Mutex<Vec<FrameRec>>>,
    hashing: Arc<Mutex<bool>>,
    pub dir: PathBuf,
    pub host: Host,
    /// The window size in DIPs.
    pub size: (f32, f32),
}

impl Page {
    /// Open `url` at 1024x768 DIPs. None (and a "skipped" line) when that host isn't configured.
    pub fn open(host: Host, url: &str) -> Option<Page> {
        Self::open_sized(host, url, 1024.0, 768.0)
    }

    pub fn open_sized(host: Host, url: &str, w: f32, h: f32) -> Option<Page> {
        let Some(program) = host.program() else {
            println!("SKIP {host:?} host not configured (HERDR_BROWSER_CEF_HOST / HERDR_BROWSER_HOST)");
            return None;
        };
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Short: macOS socket paths stop at 104 bytes.
        let dir = PathBuf::from(format!("/tmp/hp-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let args = if host == Host::Stub { vec!["--stub".into()] } else { vec![] };
        let link = HostClient::new(HostConfig { dir: dir.clone(), program, args, cdp_port: Some(0) });
        let mut cx = HeadlessAppContext::with_platform(gpui_platform::current_platform(true).text_system(), Arc::new(()), gpui_platform::current_headless_renderer);
        cx.update(gpui_base::init);
        let block = BrowserBlock { url: url.into(), profile: "default".into() };
        let l = link.clone();
        let window = cx
            .open_window(size(px(w), px(h)), |window, cx| {
                cx.new(|cx| {
                    let mut browsers = Browsers::with_link(l);
                    browsers.sync(vec![(ID.into(), block)], window, cx, |r: &mut Root, _, ev, _, _| r.events.push(ev.clone()));
                    Root { browsers, events: vec![], block: None }
                })
            })
            .unwrap();
        let view = window.update(&mut cx, |r, _, _| r.browsers.view(ID).cloned().unwrap()).unwrap();
        let frames = Arc::new(Mutex::new(Vec::<FrameRec>::new()));
        let hashing = Arc::new(Mutex::new(false));
        let (f, hsh) = (frames.clone(), hashing.clone());
        let probe: crate::browser::view::FrameProbe = Arc::new(move |ev: &FrameEvent| {
            let now = Instant::now();
            let mut f = f.lock().unwrap();
            match ev {
                FrameEvent::Arrived { seq, surface, size } => {
                    let hash = (*hsh.lock().unwrap()).then(|| hash_surface(surface)).flatten();
                    f.push(FrameRec { seq: *seq, size: *size, arrived: now, painted: None, acked: None, hash });
                }
                FrameEvent::Painted { seq } => {
                    if let Some(r) = f.iter_mut().rev().find(|r| r.seq == *seq) {
                        r.painted.get_or_insert(now);
                    }
                }
                FrameEvent::Acked { seq } => {
                    if let Some(r) = f.iter_mut().rev().find(|r| r.seq == *seq) {
                        r.acked.get_or_insert(now);
                    }
                }
            }
        });
        cx.update(|cx| view.update(cx, |v, _| v.set_frame_probe(Some(probe))));
        let mut page = Page { cx, window, view, link, frames, hashing, dir, host, size: (w, h) };
        page.wait_loaded(Duration::from_secs(30));
        Some(page)
    }

    /// Draw the window (lays the block out, paints the newest frame, acks it) and run what's due.
    pub fn pump(&mut self) {
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();
        self.cx.run_until_parked();
    }

    /// Pump until `done` holds (true) or `timeout` passes (false).
    pub fn wait(&mut self, timeout: Duration, mut done: impl FnMut(&mut Page) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Until a frame is painted and the page stops loading.
    pub fn wait_loaded(&mut self, timeout: Duration) -> bool {
        self.wait(timeout, |p| p.cx.update(|cx| p.view.read(cx).frame_seq().is_some() && !p.view.read(cx).state.loading))
    }

    /// Pump for `d` (frames keep flowing).
    pub fn run_for(&mut self, d: Duration) {
        self.wait(d, |_| false);
    }

    /// Any `browser.*` command through the agent, as `herdr-app do` runs it.
    pub fn command(&mut self, name: &str, mut args: Value) -> Result<Value, String> {
        args["id"] = json!(ID);
        let (tx, rx) = std::sync::mpsc::channel();
        let step = self
            .window
            .update(&mut self.cx, |r, _, cx| r.browsers.run(name, &args, &|_| None, Box::new(move |res| {
                let _ = tx.send(res);
            }), cx))
            .unwrap();
        if !matches!(step, Step::Done) {
            return Err(format!("{name}: the window's job (open/close), not the page's"));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(r) = rx.try_recv() {
                return r;
            }
            if Instant::now() > deadline {
                return Err(format!("{name}: no answer in 30 s"));
            }
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `browser.eval`: the expression's value.
    pub fn eval(&mut self, expr: &str) -> Result<Value, String> {
        self.command("browser.eval", json!({ "expr": expr })).map(|v| v["value"].clone())
    }

    /// Eval `expr` until `ok(value)` or `timeout`; the last value either way.
    pub fn eval_until(&mut self, expr: &str, timeout: Duration, ok: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.eval(expr).unwrap_or(Value::Null);
            if ok(&v) || Instant::now() > deadline {
                return v;
            }
            self.run_for(Duration::from_millis(20));
        }
    }

    pub fn navigate(&mut self, url: &str) -> Result<Value, String> {
        let r = self.command("browser.navigate", json!({ "url": url }));
        self.wait(Duration::from_millis(300), |_| false);
        self.wait_loaded(Duration::from_secs(30));
        r
    }

    // ---- input, through the app's real path (window events -> BrowserInput) --------------
    // Coordinates are page DIPs (from the page's top-left, under the address bar).

    fn at(&self, x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y + CHROME_HEIGHT))
    }

    fn dispatch(&mut self, ev: PlatformInput) {
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                window.dispatch_event(ev, cx);
            })
            .unwrap();
        self.cx.run_until_parked();
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, button: MouseButton, clicks: usize, modifiers: Modifiers) {
        let position = self.at(x, y);
        self.dispatch(PlatformInput::MouseDown(MouseDownEvent { button, position, modifiers, click_count: clicks, first_mouse: false }));
    }

    pub fn mouse_up(&mut self, x: f32, y: f32, button: MouseButton, clicks: usize, modifiers: Modifiers) {
        let position = self.at(x, y);
        self.dispatch(PlatformInput::MouseUp(MouseUpEvent { button, position, modifiers, click_count: clicks }));
    }

    pub fn mouse_move(&mut self, x: f32, y: f32, pressed: Option<MouseButton>) {
        let position = self.at(x, y);
        self.dispatch(PlatformInput::MouseMove(MouseMoveEvent { position, pressed_button: pressed, modifiers: Modifiers::default() }));
    }

    pub fn click(&mut self, x: f32, y: f32) {
        self.click_with(x, y, MouseButton::Left, Modifiers::default());
    }

    pub fn click_with(&mut self, x: f32, y: f32, button: MouseButton, modifiers: Modifiers) {
        self.mouse_move(x, y, None);
        self.mouse_down(x, y, button, 1, modifiers);
        self.mouse_up(x, y, button, 1, modifiers);
    }

    pub fn double_click(&mut self, x: f32, y: f32) {
        self.mouse_move(x, y, None);
        for n in 1..=2 {
            self.mouse_down(x, y, MouseButton::Left, n, Modifiers::default());
            self.mouse_up(x, y, MouseButton::Left, n, Modifiers::default());
        }
    }

    pub fn right_click(&mut self, x: f32, y: f32) {
        self.click_with(x, y, MouseButton::Right, Modifiers::default());
    }

    /// Press at `from`, move in `steps` to `to`, release.
    pub fn drag(&mut self, from: (f32, f32), to: (f32, f32), steps: usize) {
        self.mouse_move(from.0, from.1, None);
        self.mouse_down(from.0, from.1, MouseButton::Left, 1, Modifiers::default());
        for i in 1..=steps.max(1) {
            let t = i as f32 / steps.max(1) as f32;
            self.mouse_move(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t, Some(MouseButton::Left));
        }
        self.mouse_up(to.0, to.1, MouseButton::Left, 1, Modifiers::default());
    }

    /// A wheel event at (x, y): `precise` is a trackpad (pixel deltas), else a mouse wheel (lines).
    /// Positive `dy` scrolls content down (GPUI's sign: negative moves toward the page end).
    pub fn wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32, precise: bool) {
        let delta = if precise { ScrollDelta::Pixels(point(px(dx), px(dy))) } else { ScrollDelta::Lines(point(dx, dy)) };
        let position = self.at(x, y);
        self.dispatch(PlatformInput::ScrollWheel(ScrollWheelEvent { position, delta, modifiers: Modifiers::default(), touch_phase: TouchPhase::Moved }));
    }

    /// A key press and release, as `Keystroke::parse` reads it: "a", "enter", "cmd-a", "shift-left".
    /// Printable keys also need `text` (the platform's text input delivers characters).
    pub fn key(&mut self, stroke: &str) {
        let keystroke = Keystroke::parse(stroke).unwrap_or_else(|e| panic!("key {stroke:?}: {e}"));
        self.dispatch(PlatformInput::KeyDown(KeyDownEvent { keystroke: keystroke.clone(), is_held: false, prefer_character_input: false }));
        self.dispatch(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
    }

    /// Typed text, as the platform's text input hands it over (layouts, dead keys resolved).
    pub fn text(&mut self, s: &str) {
        self.with_input(|i, window, cx| i.replace_text_in_range(None, s, window, cx));
    }

    /// IME composition: marked `text` with the caret/selection at `sel` (UTF-16).
    pub fn ime_set(&mut self, text: &str, sel: std::ops::Range<usize>) {
        self.with_input(|i, window, cx| i.replace_and_mark_text_in_range(None, text, Some(sel), window, cx));
    }

    /// IME commit: the composition becomes `text`.
    pub fn ime_commit(&mut self, text: &str) {
        self.with_input(|i, window, cx| i.replace_text_in_range(None, text, window, cx));
    }

    /// IME cancel: the composition goes away.
    pub fn ime_cancel(&mut self) {
        self.with_input(|i, window, cx| i.unmark_text(window, cx));
    }

    fn with_input(&mut self, f: impl FnOnce(&mut crate::browser::input::BrowserInput, &mut Window, &mut Context<crate::browser::input::BrowserInput>)) {
        let view = self.view.clone();
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let input = view.read(cx).input().clone();
                input.update(cx, |i, cx| f(i, window, cx));
            })
            .unwrap();
        self.cx.run_until_parked();
    }

    /// Focus the page (a click at its top-left corner area is not always safe; this focuses directly).
    pub fn focus(&mut self) {
        let view = self.view.clone();
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let f = view.read(cx).focus_handle().clone();
                window.focus(&f, cx);
            })
            .unwrap();
        self.pump();
    }

    // ---- layout ------------------------------------------------------------------------

    /// Size the block (DIPs, address bar included); None fills the window.
    pub fn resize_block(&mut self, size: Option<(f32, f32)>) {
        self.window
            .update(&mut self.cx, |r, _, cx| {
                r.block = size;
                cx.notify();
            })
            .unwrap();
        self.pump();
    }

    /// Events the block sent its window (url changes, focus, new-window requests).
    pub fn events(&mut self) -> Vec<BrowserEvent> {
        self.window.update(&mut self.cx, |r, _, _| r.events.clone()).unwrap()
    }

    pub fn state(&mut self) -> crate::browser::BrowserState {
        self.cx.update(|cx| self.view.read(cx).state.clone())
    }

    pub fn console(&mut self) -> Vec<String> {
        self.cx.update(|cx| self.view.read(cx).console().map(str::to_owned).collect())
    }

    // ---- frames and pixels -------------------------------------------------------------

    /// Hash each arriving frame's pixels (reads the shared surface; costs a copy per frame).
    pub fn hash_frames(&mut self, on: bool) {
        *self.hashing.lock().unwrap() = on;
    }

    pub fn frames(&self) -> Vec<FrameRec> {
        self.frames.lock().unwrap().clone()
    }

    pub fn clear_frames(&self) {
        self.frames.lock().unwrap().clear();
    }

    pub fn last_seq(&mut self) -> Option<u64> {
        self.cx.update(|cx| self.view.read(cx).frame_seq())
    }

    /// Pump until a frame newer than `seq` has been painted; its record.
    pub fn wait_frame_after(&mut self, seq: Option<u64>, timeout: Duration) -> Option<FrameRec> {
        let mut out = None;
        self.wait(timeout, |p| {
            out = p.frames().into_iter().find(|f| Some(f.seq) > seq && f.painted.is_some());
            out.is_some()
        });
        out
    }

    /// Stats of the frames that arrived since `since`.
    pub fn frame_stats(&self, since: Instant) -> FrameStats {
        let f: Vec<FrameRec> = self.frames().into_iter().filter(|f| f.arrived >= since).collect();
        stats(&f, since)
    }

    /// The whole window as pixels (drawn first), physical size.
    pub fn shot(&mut self) -> RgbaImage {
        self.pump();
        self.cx.capture_screenshot(self.window.into()).unwrap()
    }

    /// One pixel of the page, at page DIPs.
    pub fn page_pixel(&mut self, x: f32, y: f32) -> [u8; 4] {
        let img = self.shot();
        let scale = img.width() as f32 / self.size.0;
        img.get_pixel((x * scale) as u32, ((y + CHROME_HEIGHT) * scale) as u32).0
    }

    // ---- cost --------------------------------------------------------------------------

    /// CPU and RSS of this page's host and its helpers, sampled once (ps's %cpu is a decaying
    /// average; `usage_over` measures a window properly).
    pub fn usage(&self) -> Usage {
        host_usage(&self.dir)
    }

    /// Average CPU over `d` (CPU time delta / wall time), while pumping; RSS at the end.
    pub fn usage_over(&mut self, d: Duration) -> Usage {
        let t0 = cpu_seconds(&self.dir);
        let start = Instant::now();
        self.run_for(d);
        let t1 = cpu_seconds(&self.dir);
        let mut u = host_usage(&self.dir);
        u.cpu = (t1 - t0) / start.elapsed().as_secs_f64() * 100.0;
        u
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        let _ = self.cx.update_window(self.window.into(), |_, window, _| window.remove_window());
        self.cx.run_until_parked();
    }
}

fn hash_surface(r: &crate::browser::protocol::SurfaceRef) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let s = herdr_browser_surface::import(r).ok()?;
    let px = s.read_bgra().ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    px.hash(&mut h);
    Some(h.finish())
}

pub fn stats(f: &[FrameRec], since: Instant) -> FrameStats {
    let secs = f.last().map_or(0.0, |l| (l.arrived - since).as_secs_f64()).max(1e-6);
    let mut gaps: Vec<f64> = f.windows(2).map(|w| (w[1].arrived - w[0].arrived).as_secs_f64() * 1000.0).collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| if gaps.is_empty() { 0.0 } else { gaps[((gaps.len() - 1) as f64 * p).round() as usize] };
    let distinct = f.iter().filter_map(|r| r.hash).collect::<std::collections::HashSet<_>>().len();
    FrameStats { count: f.len(), secs, fps: f.len() as f64 / secs, p50_gap_ms: pct(0.5), p95_gap_ms: pct(0.95), max_gap_ms: gaps.last().copied().unwrap_or(0.0), distinct }
}

/// The host serving `dir`'s socket and every process under it (CEF helpers).
fn host_pids(dir: &std::path::Path) -> Vec<u32> {
    let out = std::process::Command::new("ps").args(["-Ao", "pid=,ppid=,command="]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let rows: Vec<(u32, u32, String)> = out
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.collect::<Vec<_>>().join(" ")))
        })
        .collect();
    let sock = dir.join("host.sock").to_string_lossy().into_owned();
    let mut pids: Vec<u32> = rows.iter().filter(|(_, _, c)| c.contains(&sock)).map(|(p, _, _)| *p).collect();
    let mut i = 0;
    while i < pids.len() {
        let parent = pids[i];
        pids.extend(rows.iter().filter(|(_, pp, _)| *pp == parent).map(|(p, _, _)| *p));
        i += 1;
    }
    pids
}

fn ps_fields(pids: &[u32], fields: &str) -> Vec<Vec<String>> {
    if pids.is_empty() {
        return vec![];
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let out = std::process::Command::new("ps").args(["-o", fields, "-p", &list]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines().map(|l| l.split_whitespace().map(str::to_owned).collect()).collect()
}

fn host_usage(dir: &std::path::Path) -> Usage {
    let pids = host_pids(dir);
    let rows = ps_fields(&pids, "%cpu=,rss=");
    let cpu = rows.iter().filter_map(|r| r.first()?.parse::<f64>().ok()).sum();
    let rss_kb: f64 = rows.iter().filter_map(|r| r.get(1)?.parse::<f64>().ok()).sum();
    Usage { cpu, rss_mb: rss_kb / 1024.0, processes: pids.len() }
}

/// Total CPU seconds (user + system) of the host and helpers so far.
fn cpu_seconds(dir: &std::path::Path) -> f64 {
    let pids = host_pids(dir);
    ps_fields(&pids, "time=")
        .iter()
        .filter_map(|r| r.first().cloned())
        .map(|t| {
            // [[dd-]hh:]mm:ss.cc
            let (days, rest) = t.split_once('-').map_or((0.0, t.as_str()), |(d, r)| (d.parse().unwrap_or(0.0), r));
            let parts: Vec<f64> = rest.split(':').filter_map(|p| p.parse().ok()).collect();
            days * 86400.0 + parts.iter().fold(0.0, |acc, p| acc * 60.0 + p)
        })
        .sum()
}
