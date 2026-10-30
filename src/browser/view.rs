//! Part 1 (LAB-419): `BrowserView`, the GPUI entity for one browser block.
//! Owns the host connection (`HostLink`), `BrowserState`, resize and focus.
//!
//! One `HostClient` per app: it starts `herdr-browser-host` on the first send, talks JSON lines
//! over `$HERDR_APP_HOME/browser/host.sock`, and after the host dies starts it again on the next
//! send, replaying every open browser. Each block gets a `BrowserView` that turns host events into
//! `BrowserState`, sizes the page to the block, hands frames to a `FrameSink` and shows a reload
//! card after a crash. `Browsers` keeps one view per browser block in the layouts.
use super::agent::{Agent, Focus, Reply, Step};
use super::geometry::{Change, Epoch, Geometry, HoverSend, OverlayAnswer, OverlayState, Pt, Rect, zoom_factor};
use super::input::{self, BrowserInput, OverlayEvent, Owner};
use super::dialogs::{DialogsEvent, Downloads, PageDialogs};
use super::markup::{self, Markup, Tool};
use super::zoom;
use super::{BrowserRegistry, BrowserState, FrameSink, FrameView, HostLink};
use crate::layout::BrowserBlock;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_base::input::{Input, InputEvent, InputState};
use herdr_browser_protocol::{self as protocol, DownloadState, MouseKind, OverlayQuery, AppToHost, BrowserId, HostToApp, PixelSize, Size as Dips};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Console lines a block keeps.
const CONSOLE_KEEP: usize = 200;

/// How long a fresh host gets to open its socket.
const HOST_START: Duration = Duration::from_secs(10);

/// Where the host lives and how to start it.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// `$HERDR_APP_HOME/browser`: `host.sock` and the shared `profile` cache.
    pub dir: PathBuf,
    /// `herdr-browser-host`: `$HERDR_BROWSER_HOST`, else beside the app's binary.
    pub program: PathBuf,
    /// Extra arguments (`--stub` from `$HERDR_BROWSER_HOST_ARGS`, for tests).
    pub args: Vec<String>,
    /// The CEF remote debugging port agents use (contract D7): 0 picks a free one (default),
    /// `$HERDR_BROWSER_CDP_PORT` sets one, `off` turns it off.
    pub cdp_port: Option<u16>,
}

impl HostConfig {
    pub fn from_env() -> Self {
        let program = std::env::var_os("HERDR_BROWSER_HOST").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("herdr-browser-host"))).unwrap_or_else(|| "herdr-browser-host".into())
        });
        let args = std::env::var("HERDR_BROWSER_HOST_ARGS").map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
        let cdp_port = match std::env::var("HERDR_BROWSER_CDP_PORT").ok().as_deref() {
            Some("off") => None,
            Some(p) => p.parse().ok().or(Some(0)),
            None => Some(0),
        };
        Self { dir: crate::mods::app_dir().join("browser"), program, args, cdp_port }
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("host.sock")
    }
}

/// What a browser needs to be made again after the host restarts.
#[derive(Clone, Debug, PartialEq)]
struct Open {
    url: Option<String>,
    profile: Option<String>,
    size: Dips,
    scale: f32,
}

type Subscriber = async_channel::Sender<HostToApp>;

#[derive(Default)]
struct Shared {
    open: HashMap<BrowserId, Open>,
    subs: HashMap<BrowserId, Subscriber>,
    /// Bumped each time a host connection ends; readers of an older one stay quiet.
    generation: u64,
    /// Hears every host message (the agent commands, part 5).
    tap: Option<Tap>,
}

pub type Tap = Arc<dyn Fn(&HostToApp) + Send + Sync>;

/// Hand `msg` to the tap, outside the lock.
fn tell_tap(shared: &Mutex<Shared>, msg: &HostToApp) {
    let tap = shared.lock().unwrap().tap.clone();
    if let Some(tap) = tap {
        tap(msg);
    }
}

/// The app's one connection to `herdr-browser-host` (the `HostLink` every part sends through).
pub struct HostClient {
    config: HostConfig,
    shared: Arc<Mutex<Shared>>,
    queue: mpsc::Sender<AppToHost>,
}

impl HostClient {
    pub fn new(config: HostConfig) -> Arc<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (queue, rx) = mpsc::channel();
        let (cfg, sh) = (config.clone(), shared.clone());
        std::thread::Builder::new()
            .name("browser-host".into())
            .spawn(move || Writer { config: cfg, shared: sh, conn: None, replayed: HashSet::new() }.run(rx))
            .expect("browser host thread");
        Arc::new(Self { config, shared, queue })
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Host events for browser `id` (and the app-wide ones: a crash, an error without an id).
    pub fn subscribe(&self, id: &str) -> async_channel::Receiver<HostToApp> {
        let (tx, rx) = async_channel::unbounded();
        self.shared.lock().unwrap().subs.insert(id.to_owned(), tx);
        rx
    }

    /// The block is gone: close its page and stop its events.
    pub fn close(&self, id: &str) {
        let was_open = {
            let mut s = self.shared.lock().unwrap();
            s.subs.remove(id);
            s.open.contains_key(id)
        };
        if was_open {
            self.send(AppToHost::Close { id: id.to_owned() });
        }
    }

    /// Every host message also goes to `tap` (after the views get theirs).
    pub fn set_tap(&self, tap: Tap) {
        self.shared.lock().unwrap().tap = Some(tap);
    }

    /// Browsers the host should have open now.
    pub fn open_ids(&self) -> Vec<BrowserId> {
        let mut ids: Vec<_> = self.shared.lock().unwrap().open.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl HostLink for HostClient {
    fn send(&self, msg: AppToHost) {
        {
            let mut s = self.shared.lock().unwrap();
            match &msg {
                AppToHost::Create { id, url, size, scale, profile } => {
                    s.open.insert(id.clone(), Open { url: url.clone(), profile: profile.clone(), size: *size, scale: *scale });
                }
                AppToHost::Resize { id, size, scale } => {
                    if let Some(o) = s.open.get_mut(id) {
                        (o.size, o.scale) = (*size, *scale);
                    }
                }
                AppToHost::Navigate { id, url } => {
                    if let Some(o) = s.open.get_mut(id) {
                        o.url = Some(url.clone());
                    }
                }
                AppToHost::Close { id } => {
                    s.open.remove(id);
                }
                _ => {}
            }
        }
        let _ = self.queue.send(msg);
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        let _ = self.queue.send(AppToHost::Shutdown);
    }
}

struct Conn {
    stream: UnixStream,
    child: Option<Child>,
    generation: u64,
}

/// The thread that owns the socket: starts the host when there is none, writes in order.
struct Writer {
    config: HostConfig,
    shared: Arc<Mutex<Shared>>,
    conn: Option<Conn>,
    /// Pages made again at connect whose own queued `Create` is still to come: it is skipped.
    replayed: HashSet<BrowserId>,
}

impl Writer {
    fn run(mut self, rx: mpsc::Receiver<AppToHost>) {
        while let Ok(msg) = rx.recv() {
            if matches!(msg, AppToHost::Shutdown) {
                self.shutdown();
                return;
            }
            // Closing a page on a host that isn't running needs no host.
            let dead = self.conn.as_ref().is_none_or(|c| c.generation != self.shared.lock().unwrap().generation);
            if dead && matches!(msg, AppToHost::Close { .. } | AppToHost::FrameAck { .. } | AppToHost::Focus { .. }) {
                continue;
            }
            if dead {
                self.drop_conn(None);
                match self.connect() {
                    Ok(c) => {
                        self.conn = Some(c);
                        // The host forgot everything: make every open page again, at its newest
                        // url and size. Creates already queued behind this are then skipped.
                        let mut open: Vec<_> = self.shared.lock().unwrap().open.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        open.sort_by(|a, b| a.0.cmp(&b.0));
                        self.replayed = open.iter().map(|(id, _)| id.clone()).collect();
                        for (id, o) in open {
                            self.write(&AppToHost::Create { id, url: o.url, size: o.size, scale: o.scale, profile: o.profile });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("browser host: {e:#}");
                        self.broadcast_crash(format!("{e:#}"));
                        continue;
                    }
                }
            }
            match &msg {
                AppToHost::Create { id, .. } if self.replayed.remove(id) => continue,
                AppToHost::Close { id } => {
                    self.replayed.remove(id);
                }
                _ => {}
            }
            self.write(&msg);
        }
        self.shutdown();
    }

    fn write(&mut self, msg: &AppToHost) {
        let Some(c) = &mut self.conn else { return };
        let mut line = protocol::encode(msg);
        line.push('\n');
        if let Err(e) = c.stream.write_all(line.as_bytes()) {
            self.drop_conn(Some(format!("host went away: {e}")));
        }
    }

    fn connect(&mut self) -> anyhow::Result<Conn> {
        std::fs::create_dir_all(&self.config.dir)?;
        let sock = self.config.socket();
        // A host left from an earlier run of this app is reused; otherwise start one.
        let (stream, child) = match UnixStream::connect(&sock) {
            Ok(s) => (s, None),
            Err(_) => {
                let _ = std::fs::remove_file(&sock);
                let mut child = Command::new(&self.config.program)
                    .arg("--socket")
                    .arg(&sock)
                    .args(&self.config.args)
                    .stdin(Stdio::null())
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("can't start {}: {e}", self.config.program.display()))?;
                let deadline = Instant::now() + HOST_START;
                loop {
                    if let Ok(s) = UnixStream::connect(&sock) {
                        break (s, Some(child));
                    }
                    if let Ok(Some(status)) = child.try_wait() {
                        anyhow::bail!("browser host exited at start ({status})");
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!("browser host didn't open {} in time", sock.display());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let generation = self.shared.lock().unwrap().generation;
        let reader = stream.try_clone()?;
        let shared = self.shared.clone();
        std::thread::Builder::new().name("browser-host-read".into()).spawn(move || read(reader, shared, generation))?;
        let mut conn = Conn { stream, child, generation };
        let hello = AppToHost::Hello {
            version: protocol::PROTOCOL_VERSION,
            // Resolved (/tmp is /private/tmp on macOS): CEF wants its cache under this exact path.
            profile_dir: std::fs::canonicalize(&self.config.dir).unwrap_or_else(|_| self.config.dir.clone()).join("profile").to_string_lossy().into_owned(),
            cdp_port: self.config.cdp_port,
            cdp_pipe: false, // HERDR: the pipe plumbing (docs/browser-cdp-pipe.md) sets this true
        };
        conn.stream.write_all(format!("{}\n", protocol::encode(&hello)).as_bytes())?;
        Ok(conn)
    }

    /// The connection is over (an error, or the host died): kill what we started.
    fn drop_conn(&mut self, why: Option<String>) {
        if let Some(mut c) = self.conn.take() {
            let _ = c.stream.shutdown(std::net::Shutdown::Both);
            if let Some(child) = &mut c.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let current = {
                let mut s = self.shared.lock().unwrap();
                let current = s.generation == c.generation;
                if current {
                    s.generation += 1;
                }
                current
            };
            if let (true, Some(why)) = (current, why) {
                self.broadcast_crash(why);
            }
        }
    }

    fn broadcast_crash(&self, reason: String) {
        let crash = HostToApp::Crashed { id: None, reason };
        for tx in self.shared.lock().unwrap().subs.values() {
            let _ = tx.try_send(crash.clone());
        }
        tell_tap(&self.shared, &crash);
    }

    fn shutdown(&mut self) {
        if self.conn.is_some() {
            self.write(&AppToHost::Shutdown);
        }
        if let Some(c) = &mut self.conn
            && let Some(child) = &mut c.child
        {
            // A moment to close cleanly, then it goes.
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.drop_conn(None);
    }
}

/// Host lines to the views. At the end (the host died) every view hears `Crashed`.
fn read(stream: UnixStream, shared: Arc<Mutex<Shared>>, generation: u64) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let msg = match protocol::decode::<HostToApp>(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("browser host sent {line:?}: {e}");
                continue;
            }
        };
        let mut s = shared.lock().unwrap();
        if s.generation != generation {
            return;
        }
        if let HostToApp::Url { id, url } = &msg
            && let Some(o) = s.open.get_mut(id)
        {
            o.url = Some(url.clone());
        }
        match target(&msg) {
            Some(id) => {
                if let Some(tx) = s.subs.get(id) {
                    let _ = tx.try_send(msg.clone());
                }
            }
            None => {
                for tx in s.subs.values() {
                    let _ = tx.try_send(msg.clone());
                }
            }
        }
        drop(s);
        tell_tap(&shared, &msg);
    }
    let mut s = shared.lock().unwrap();
    if s.generation == generation {
        s.generation += 1;
        let crash = HostToApp::Crashed { id: None, reason: "the browser host stopped".into() };
        for tx in s.subs.values() {
            let _ = tx.try_send(crash.clone());
        }
        drop(s);
        tell_tap(&shared, &crash);
    }
}

fn target(msg: &HostToApp) -> Option<&BrowserId> {
    use HostToApp as H;
    match msg {
        H::Ready { .. } => None,
        H::Crashed { id, .. } | H::Error { id, .. } => id.as_ref(),
        H::Frame { id, .. }
        | H::Title { id, .. }
        | H::ContextMenu { id, .. }
        | H::StylesResult { id, .. }
        | H::PickInfoResult { id, .. }
        | H::Url { id, .. }
        | H::Loading { id, .. }
        | H::Cursor { id, .. }
        | H::Favicon { id, .. }
        | H::NewWindow { id, .. }
        | H::Download { id, .. }
        | H::FileChooser { id, .. }
        | H::Dialog { id, .. }
        | H::ImeCaret { id, .. }
        | H::FindResult { id, .. }
        | H::Console { id, .. }
        | H::EvalResult { id, .. }
        | H::ScreenshotDone { id, .. }
        | H::CdpTarget { id, .. }
        | H::OverlayRects { id, .. }
        | H::Viewport { id, .. } => Some(id),
    }
}

actions!(browser, [FocusAddress, OpenFind, ZoomIn, ZoomOut, ZoomReset]);

/// What was typed into the address bar, as a page to open: a url as it is, a bare host
/// (`example.com/x`, `localhost:3000`) over https, anything else searched. None: nothing typed.
pub fn address_to_url(typed: &str, engine: crate::settings::SearchEngine) -> Option<String> {
    let t = typed.trim();
    if t.is_empty() {
        return None;
    }
    let scheme = t.split_once(':').is_some_and(|(s, rest)| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) && (rest.starts_with("//") || ["about", "data", "file", "javascript", "mailto", "chrome", "view-source"].contains(&s.to_ascii_lowercase().as_str()))
    });
    if scheme {
        return Some(t.to_owned());
    }
    let host = t.split(['/', '?', '#']).next().unwrap_or("");
    let name = host.rsplit_once(':').filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
    let hostlike = !t.contains(char::is_whitespace)
        && !name.is_empty()
        && (name.eq_ignore_ascii_case("localhost") || (name.contains('.') && !name.starts_with('.') && !name.ends_with('.')))
        && name.chars().all(|c| c.is_alphanumeric() || "-.[]:".contains(c));
    if hostlike {
        let local = name.eq_ignore_ascii_case("localhost") || name.split('.').all(|p| p.parse::<u8>().is_ok());
        return Some(format!("{}://{t}", if local { "http" } else { "https" }));
    }
    Some(engine.url(t))
}

/// What the block tells its window: the page moved, so the layout document follows.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserEvent {
    Url(String),
    /// The page took the keyboard: it is the browser agent commands act on by default.
    Focused,
    /// A link or script asked for a new window or tab: the window opens a block beside this one.
    NewWindow(String),
    /// Pick mode: the user clicked an element at (x, y) page DIPs (its highlight is the overlay).
    Picked { x: f32, y: f32 },
    /// Show a toast (a download finished or failed): `{"title","body","level"}`.
    Notice(serde_json::Value),
}

/// One browser block on screen.
pub struct BrowserView {
    id: BrowserId,
    block: BrowserBlock,
    link: Arc<HostClient>,
    pub state: BrowserState,
    /// The host or the page died; the block shows a reload card until clicked.
    crashed: Option<String>,
    /// Size and scale the host was last told; None: no page made yet.
    sized: Option<(Dips, f32)>,
    /// The newest frame from the host, painted into the page area (part 3).
    frames: FrameView,
    /// The last frame acked (each frame is acked once, however often it's painted).
    acked: Option<u64>,
    focus: FocusHandle,
    input: Entity<BrowserInput>,
    was_focused: bool,
    /// The page's newest console lines (at most `CONSOLE_KEEP`), oldest first.
    console: std::collections::VecDeque<String>,
    /// The address bar; shows the page's url unless someone is typing in it.
    address: Entity<InputState>,
    address_stale: bool,
    states: Arc<Mutex<HashMap<BrowserId, BrowserState>>>,
    _events: Task<()>,
    _focus: [Subscription; 2],
    _address: Subscription,
    _visibility: Subscription,
    probe: Option<FrameProbe>,
    /// Page overlays (highlight, pick, find matches): epochs and hover coalescing (geometry.rs).
    overlay: OverlayState,
    /// Chrome zoom level: the one set here, or the page reported (`Viewport`).
    zoom_level: f64,
    /// Zoom per host (shared by every block through `Browsers`).
    zoom_prefs: Arc<Mutex<zoom::ZoomPrefs>>,
    /// The page's scroll offset the host last reported (CSS px).
    scroll: (f32, f32),
    /// The open context menu (the host asked; the app draws it).
    menu: Option<Menu>,
    /// The find bar, when open (I9).
    find: Option<FindBar>,
    /// Markup drawn over the page (markup.rs); shown while marking up or when shapes exist.
    markup: Entity<Markup>,
    /// When the user's input was last refused under an agent lease (the strip flashes).
    refused_at: Option<std::time::Instant>,
    /// Whether the mode strip (markup tools, the agent lease) was drawn last render.
    strip_shown: bool,
    /// JS dialogs, the file chooser and download notices (dialogs.rs, fork-host's).
    dialogs: Entity<PageDialogs>,
    /// The app's downloads (shared by every block); the bar's ⬇ opens the list.
    downloads: Downloads,
    downloads_open: bool,
    _dialogs: Subscription,
}

/// The find bar: its input, what was last searched and the host's "active of count".
pub struct FindBar {
    pub input: Entity<InputState>,
    pub text: String,
    pub count: u32,
    pub active: u32,
    _typed: Subscription,
}

/// App-handled context menu commands (CEF's MENU_ID_USER_FIRST and up): the app does these itself.
pub const MENU_OPEN_LINK_IN_NEW_TAB: i32 = 26500;
pub const MENU_COPY_LINK_ADDRESS: i32 = 26501;
pub const MENU_SAVE_IMAGE_AS: i32 = 26502;

/// Height of one context menu row and the menu's width, in DIPs.
pub const MENU_ROW: f32 = 24.0;
pub const MENU_WIDTH: f32 = 220.0;

/// A context menu the host asked for, as the app shows it.
#[derive(Clone, Debug)]
pub struct Menu {
    pub req: u64,
    /// Where it opens, page DIPs.
    pub at: (f32, f32),
    /// Rows, submenus flattened (their labels prefixed); command 0 is a separator.
    pub rows: Vec<herdr_browser_protocol::MenuItem>,
    pub link_url: Option<String>,
    pub src_url: Option<String>,
}

impl Menu {
    /// The host's items plus the app's own for a link or an image, in Chrome's order.
    pub fn build(req: u64, x: f32, y: f32, link_url: Option<String>, src_url: Option<String>, items: Vec<herdr_browser_protocol::MenuItem>) -> Menu {
        use herdr_browser_protocol::MenuItem;
        let item = |command, label: &str| MenuItem { command, label: label.into(), enabled: true, checked: false, submenu: vec![] };
        let sep = || MenuItem { command: 0, label: String::new(), enabled: false, checked: false, submenu: vec![] };
        let mut rows = Vec::new();
        if link_url.is_some() {
            rows.push(item(MENU_OPEN_LINK_IN_NEW_TAB, "Open Link in New Tab"));
            rows.push(item(MENU_COPY_LINK_ADDRESS, "Copy Link Address"));
            rows.push(sep());
        }
        if src_url.is_some() {
            rows.push(item(MENU_SAVE_IMAGE_AS, "Save Image As…"));
            rows.push(sep());
        }
        fn flatten(out: &mut Vec<herdr_browser_protocol::MenuItem>, items: Vec<herdr_browser_protocol::MenuItem>, prefix: &str) {
            for mut i in items {
                let sub = std::mem::take(&mut i.submenu);
                if sub.is_empty() {
                    if !prefix.is_empty() && i.command != 0 {
                        i.label = format!("{prefix} › {}", i.label);
                    }
                    out.push(i);
                } else {
                    flatten(out, sub, &i.label);
                }
            }
        }
        flatten(&mut rows, items, "");
        // No leading, trailing or doubled separators.
        let mut clean: Vec<MenuItem> = Vec::new();
        for r in rows {
            if r.command == 0 && clean.last().is_none_or(|l| l.command == 0) {
                continue;
            }
            clean.push(r);
        }
        while clean.last().is_some_and(|l| l.command == 0) {
            clean.pop();
        }
        Menu { req, at: (x, y), rows: clean, link_url, src_url }
    }

    /// The row at page point (x, y), if it's a command row.
    pub fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let (mx, my) = self.at;
        if x < mx || x > mx + MENU_WIDTH || y < my + 4.0 {
            return None;
        }
        let i = ((y - my - 4.0) / MENU_ROW) as usize;
        self.rows.get(i).filter(|r| r.command != 0).map(|_| i)
    }
}

/// What a frame probe hears (`BrowserView::set_frame_probe`). `Painted` comes with every draw
/// that shows the frame, so the first one per `seq` is its paint time.
#[derive(Clone, Debug)]
pub enum FrameEvent {
    Arrived { seq: u64, surface: herdr_browser_protocol::SurfaceRef, size: herdr_browser_protocol::PixelSize },
    Painted { seq: u64 },
    Acked { seq: u64 },
}

pub type FrameProbe = Arc<dyn Fn(&FrameEvent) + Send + Sync>;

/// Height of the bar above the page (back, forward, reload, address), in DIPs.
pub const CHROME_HEIGHT: f32 = 28.0;

/// Height of the find bar under it, when open.
pub const FIND_HEIGHT: f32 = 28.0;

impl EventEmitter<BrowserEvent> for BrowserView {}

impl BrowserView {
    pub fn new(id: &str, block: BrowserBlock, link: Arc<HostClient>, states: Arc<Mutex<HashMap<BrowserId, BrowserState>>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rx = link.subscribe(id);
        let events = cx.spawn(async move |this, cx| {
            while let Ok(msg) = rx.recv().await {
                if this.update(cx, |v, cx| v.on_host(msg, cx)).is_err() {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        // Part 4: mouse, keys and IME for this page; it tells the host about focus too.
        let input = cx.new(|_| BrowserInput::new(id.to_owned(), link.clone() as Arc<dyn HostLink>));
        let on = cx.on_focus(&focus, window, |v, _, cx| {
            v.input.update(cx, |i, _| i.set_focused(true));
            cx.emit(BrowserEvent::Focused);
        });
        let off = cx.on_blur(&focus, window, |v, _, cx| v.input.update(cx, |i, _| i.set_focused(false)));
        bind_keys(cx);
        let downloads = Downloads::default();
        let (dialogs, dialogs_sub) = Self::make_dialogs(id, &link, downloads.clone(), cx);
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter address"));
        let typed = cx.subscribe_in(&address, window, |v, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => {
                let text = input.read(cx).value();
                if let Some(url) = address_to_url(&text, crate::settings::get().search_engine) {
                    v.navigate(url, cx);
                    window.focus(&v.focus, cx);
                }
            }
            // Left without Enter: back to the page's url.
            InputEvent::Blur => {
                v.address_stale = true;
                cx.notify();
            }
            _ => {}
        });
        // Occluded, minimized, another Space, display asleep: the host stops painting until shown.
        let visibility = cx.observe_window_visibility(window, |v, visibility, _, cx| {
            let visible = visibility.is_visible();
            if v.frames.set_visible(visible) {
                v.link.send(AppToHost::SetVisible { id: v.id.clone(), visible });
                if visible {
                    v.acked = None;
                }
                cx.notify();
            }
        });
        let state = BrowserState { url: block.url.clone(), ..Default::default() };
        states.lock().unwrap().insert(id.to_owned(), state.clone());
        Self {
            id: id.to_owned(),
            block,
            link,
            state,
            crashed: None,
            sized: None,
            frames: FrameView::new(),
            acked: None,
            focus,
            input,
            was_focused: false,
            console: Default::default(),
            address,
            address_stale: true,
            states,
            _events: events,
            _focus: [on, off],
            _address: typed,
            _visibility: visibility,
            probe: None,
            overlay: OverlayState::default(),
            zoom_level: 0.0,
            zoom_prefs: Default::default(),
            scroll: (0.0, 0.0),
            menu: None,
            find: None,
            markup: cx.new(|_| Markup::default()),
            refused_at: None,
            strip_shown: false,
            dialogs,
            downloads,
            downloads_open: false,
            _dialogs: dialogs_sub,
        }
    }

    /// The page's input (part 4); the platform's text input reaches it as `EntityInputHandler`.
    pub fn input(&self) -> &Entity<BrowserInput> {
        &self.input
    }

    /// The page's newest console lines, oldest first.
    pub fn console(&self) -> impl Iterator<Item = &str> {
        self.console.iter().map(String::as_str)
    }

    /// The address bar's text state (tests type into it).
    pub fn address(&self) -> &Entity<InputState> {
        &self.address
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn crashed(&self) -> Option<&str> {
        self.crashed.as_deref()
    }

    /// Open `url` in this block (the address bar, agents).
    pub fn navigate(&mut self, url: String, cx: &mut Context<Self>) {
        self.block.url = url.clone();
        if self.sized.is_some() {
            self.link.send(AppToHost::Navigate { id: self.id.clone(), url });
        }
        self.address_stale = true;
        cx.notify();
    }

    /// The layout changed the block (another url typed into the document): follow it.
    pub fn set_block(&mut self, block: BrowserBlock, cx: &mut Context<Self>) {
        if block == self.block {
            return;
        }
        let go = block.url != self.state.url && !block.url.is_empty();
        self.block = block;
        if go && self.sized.is_some() {
            self.link.send(AppToHost::Navigate { id: self.id.clone(), url: self.block.url.clone() });
        }
        cx.notify();
    }

    pub fn on_host(&mut self, msg: HostToApp, cx: &mut Context<Self>) {
        use HostToApp as H;
        // Dialogs, the file chooser and downloads are the dialogs entity's (a crash also clears it).
        let dialog = self.dialogs.update(cx, |d, cx| d.on_host(&msg, cx));
        if dialog {
            cx.notify();
            return;
        }
        match msg {
            H::Url { url, .. } => {
                if url != self.state.url {
                    self.overlay.changed(Change::Navigate);
                    // Another host: its remembered zoom (Chrome keeps zoom per host the same way).
                    if zoom::host_of(&url) != zoom::host_of(&self.state.url) {
                        let level = self.zoom_prefs.lock().unwrap().level_for(&url);
                        if (level - self.zoom_level).abs() > 1e-6 {
                            self.apply_zoom(level);
                        }
                    }
                }
                if url != self.state.url {
                    self.state.url = url.clone();
                    self.block.url = url.clone();
                    self.address_stale = true;
                    cx.emit(BrowserEvent::Url(url));
                }
            }
            H::Title { title, .. } => self.state.title = title,
            H::Loading { loading, can_back, can_forward, .. } => {
                (self.state.loading, self.state.can_back, self.state.can_forward) = (loading, can_back, can_forward);
            }
            H::Cursor { name, .. } => self.state.cursor = name,
            H::Crashed { reason, .. } => {
                // The host forgets the page; the reload card makes it again.
                self.crashed = Some(reason);
                self.sized = None;
                self.frames.clear();
                self.acked = None;
                self.state.loading = false;
            }
            H::Frame { seq, surface, size, scale, .. } => {
                if let Some(p) = &self.probe {
                    p(&FrameEvent::Arrived { seq, surface: surface.clone(), size });
                }
                // Acked once painted (`send_ack` after the draw), so the host sends at most one ahead.
                if let Err(e) = self.frames.on_frame(seq, surface, size, scale) {
                    tracing::warn!("browser {}: frame {seq}: {e:#}", self.id);
                    // Unpaintable: ack anyway, or the host waits on it forever.
                    self.link.send(AppToHost::FrameAck { id: self.id.clone(), seq });
                } else if crate::control::headless() && self.acked != Some(seq) {
                    // A headless window never draws, so nothing would ack: take each frame as
                    // shown, or the host stops after one and screenshots stay stale.
                    self.acked = Some(seq);
                    self.link.send(AppToHost::FrameAck { id: self.id.clone(), seq });
                }
            }
            H::Console { message, .. } => {
                if self.console.len() == CONSOLE_KEEP {
                    self.console.pop_front();
                }
                self.console.push_back(message);
                return;
            }
            H::Viewport { scroll_x, scroll_y, zoom_level, .. } => {
                // Only a real move stales the overlays (the host may repeat itself).
                let why = if zoom_level != self.zoom_level {
                    Some(Change::Zoom)
                } else if (scroll_x, scroll_y) != self.scroll {
                    Some(Change::Scroll)
                } else {
                    None
                };
                (self.zoom_level, self.scroll) = (zoom_level, (scroll_x, scroll_y));
                if let Some(why) = why {
                    self.overlay.changed(why);
                }
                self.send_waiting_hover();
            }
            H::OverlayRects { req, epoch, rects, frames, .. } => {
                let answer = OverlayAnswer {
                    req,
                    epoch: Epoch(epoch),
                    rects: rects.iter().map(|r| Rect::new(r.x, r.y, r.w, r.h)).collect(),
                    frames: frames.iter().map(|p| Pt { x: p.x, y: p.y }).collect(),
                };
                let geo = self.geometry();
                self.overlay.answer(&answer, &geo);
                self.send_waiting_hover();
            }
            H::FindResult { count, active, .. } => {
                if let Some(f) = &mut self.find {
                    (f.count, f.active) = (count, active);
                }
            }
            H::ContextMenu { req, x, y, link_url, src_url, items, .. } => {
                if let Some(old) = self.menu.take() {
                    self.link.send(AppToHost::MenuResult { id: self.id.clone(), req: old.req, command: None });
                }
                self.menu = Some(Menu::build(req, x, y, link_url, src_url, items));
            }
            H::NewWindow { url, .. } => {
                cx.emit(BrowserEvent::NewWindow(url));
                return;
            }
            H::ImeCaret { x, y, w, h, .. } => {
                self.input.update(cx, |i, _| i.on_caret(x, y, w, h));
                return;
            }
            H::Error { message, .. } => tracing::warn!("browser {}: {message}", self.id),
            _ => return,
        }
        self.states.lock().unwrap().insert(self.id.clone(), self.state.clone());
        cx.notify();
    }

    /// The block was laid out at `size` (DIPs) and `scale`: make the page, or size it.
    pub fn laid_out(&mut self, size: Dips, scale: f32) {
        if self.crashed.is_some() || size.w < 1.0 || size.h < 1.0 {
            return;
        }
        match self.sized {
            None => {
                let url = Some(self.block.url.clone()).filter(|u| !u.is_empty());
                // The shared profile is the host's default; another one names its own cache.
                let profile = Some(self.block.profile.clone()).filter(|p| p != "default");
                self.link.send(AppToHost::Create { id: self.id.clone(), url, size, scale, profile });
            }
            Some((s, k)) if (s.w - size.w).abs() < 0.5 && (s.h - size.h).abs() < 0.5 && k == scale => return,
            Some(_) => {
                self.frames.on_resize_pending(size);
                self.overlay.changed(Change::Resize);
                self.link.send(AppToHost::Resize { id: self.id.clone(), size, scale });
            }
        }
        self.sized = Some((size, scale));
    }

    /// An agent drives this page before it was ever shown (a background tab, a headless
    /// window): make it now at a desktop size; the first layout resizes it.
    pub fn ensure_page(&mut self) {
        if self.sized.is_none() && self.crashed.is_none() {
            self.laid_out(Dips { w: 1280.0, h: 800.0 }, 2.0);
        }
    }

    /// The reload card's button: a new page at the last url (starting the host again if needed).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.crashed = None;
        self.sized = None;
        self.frames.clear();
        self.acked = None;
        cx.notify();
    }

    /// After a draw: the frame painted in it may be acked, so the host sends the next.
    fn send_ack(&mut self) {
        if let Some(seq) = self.frames.ack().filter(|s| Some(*s) != self.acked) {
            self.acked = Some(seq);
            self.link.send(AppToHost::FrameAck { id: self.id.clone(), seq });
            if let Some(p) = &self.probe {
                p(&FrameEvent::Acked { seq });
            }
        }
    }

    /// The page's geometry, with the page area's top-left as the origin (overlays draw inside it).
    pub fn geometry(&self) -> Geometry {
        let ((w, h), scale) = self.sized.map_or(((0.0, 0.0), 1.0), |(s, k)| ((s.w, s.h), k));
        Geometry { zoom: zoom_factor(self.zoom_level), origin: Pt::default(), scale, size: Pt { x: w, y: h } }
    }

    /// Ask the page for overlay rects (a selector, the selection); they show when the answer is current.
    /// Returns the request id and epoch it was sent with.
    pub fn request_overlay(&mut self, query: OverlayQuery) -> (u64, u64) {
        let (req, epoch) = self.overlay.request();
        self.link.send(AppToHost::Overlay { id: self.id.clone(), req, epoch: epoch.0, query });
        (req, epoch.0)
    }

    /// The pointer is over the page at `at` (page DIPs): a coalesced hit test for a highlight.
    pub fn hover_overlay(&mut self, at: Pt) {
        let css = self.geometry().page_to_viewport(at);
        if let HoverSend::Now { req, epoch, at } = self.overlay.hover(css) {
            self.link.send(AppToHost::Overlay { id: self.id.clone(), req, epoch: epoch.0, query: OverlayQuery::HitTest { x: at.x, y: at.y } });
        }
    }

    fn send_waiting_hover(&mut self) {
        if let Some(HoverSend::Now { req, epoch, at }) = self.overlay.next_hover() {
            self.link.send(AppToHost::Overlay { id: self.id.clone(), req, epoch: epoch.0, query: OverlayQuery::HitTest { x: at.x, y: at.y } });
        }
    }

    /// Overlay rects to draw now, in page DIPs (empty while stale).
    pub fn overlay_rects(&self) -> &[Rect] {
        self.overlay.shown()
    }

    fn make_dialogs(id: &str, link: &Arc<HostClient>, downloads: Downloads, cx: &mut Context<Self>) -> (Entity<PageDialogs>, Subscription) {
        let link = link.clone() as Arc<dyn HostLink>;
        let dialogs = cx.new(|cx| PageDialogs::new(id, link, downloads, cx));
        let sub = cx.subscribe(&dialogs, |_, _, ev: &DialogsEvent, cx| match ev {
            DialogsEvent::Notice(n) => cx.emit(BrowserEvent::Notice(n.clone())),
        });
        (dialogs, sub)
    }

    /// Share the app's download list (Browsers hands every block the same one).
    pub fn use_downloads(&mut self, downloads: Downloads, cx: &mut Context<Self>) {
        let (dialogs, sub) = Self::make_dialogs(&self.id.clone(), &self.link, downloads.clone(), cx);
        (self.dialogs, self._dialogs, self.downloads) = (dialogs, sub, downloads);
    }

    /// The page's dialogs (a JS dialog on screen, tests answer it).
    pub fn dialogs(&self) -> &Entity<PageDialogs> {
        &self.dialogs
    }

    pub fn toggle_downloads(&mut self, cx: &mut Context<Self>) {
        self.downloads_open = !self.downloads_open;
        cx.notify();
    }

    pub fn downloads_open(&self) -> bool {
        self.downloads_open
    }

    fn downloads_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.downloads_open {
            return None;
        }
        let list = self.downloads.list();
        let rows = list.iter().rev().take(12).map(|d| {
            let state = match d.state {
                DownloadState::Started => "starting".to_string(),
                DownloadState::Progress => match d.total {
                    Some(t) if t > 0 => format!("{} %", d.received * 100 / t),
                    _ => format!("{} KB", d.received / 1024),
                },
                DownloadState::Done => "done".into(),
                DownloadState::Failed => "failed".into(),
            };
            div()
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .child(div().text_sm().child(d.name.clone()))
                .child(div().text_xs().opacity(0.6).child(format!("{state} · {}", d.path.clone().unwrap_or_else(|| d.url.clone()))))
        });
        Some(
            div()
                .id("downloads-panel")
                .absolute()
                .top(px(4.))
                .right(px(8.))
                .w(px(320.))
                .py_1()
                .bg(rgb(0xffffff))
                .text_color(rgb(0x1a1a1a))
                .border_1()
                .border_color(rgb(0xc8c8c8))
                .rounded_md()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                    v.downloads_open = false;
                    cx.notify();
                }))
                .child(div().px_2().text_xs().opacity(0.6).child(if list.is_empty() { "No downloads yet" } else { "Downloads" }))
                .children(rows)
                .into_any_element(),
        )
    }

    /// Drain what `BrowserInput` routed away from the page (part 4's owners): markup draws,
    /// pick hovers the highlight, a refusal flashes the lease strip, an owner change re-renders.
    pub fn drain_overlay(&mut self, cx: &mut Context<Self>) {
        let events = self.input.update(cx, |i, _| i.take_overlay());
        if events.is_empty() {
            return;
        }
        for ev in &events {
            match ev {
                OverlayEvent::Pointer { owner: Owner::Pick, kind, x, y, .. } => match kind {
                    MouseKind::Move => self.hover_overlay(Pt { x: *x, y: *y }),
                    MouseKind::Down => self.hover_overlay(Pt { x: *x, y: *y }),
                    // The click's release picks what is highlighted and ends pick (after the
                    // release, so the page never gets a stray button-up).
                    MouseKind::Up => {
                        self.hover_overlay(Pt { x: *x, y: *y });
                        self.input.update(cx, |i, _| {
                            i.escape();
                        });
                        cx.emit(BrowserEvent::Picked { x: *x, y: *y });
                    }
                    _ => {}
                },
                OverlayEvent::Pointer { owner: Owner::Markup, .. } | OverlayEvent::Edit { owner: Owner::Markup, .. } => {
                    self.markup.update(cx, |m, _| m.on_event(ev));
                }
                OverlayEvent::Refused => self.refused_at = Some(std::time::Instant::now()),
                _ => {}
            }
        }
        // The input owner may have pushed more while handling (an Owner change after escape).
        let more = self.input.update(cx, |i, _| i.take_overlay());
        for ev in &more {
            if let OverlayEvent::Pointer { owner: Owner::Markup, .. } | OverlayEvent::Edit { owner: Owner::Markup, .. } = ev {
                self.markup.update(cx, |m, _| m.on_event(ev));
            }
        }
        cx.notify();
    }

    /// The input owner on top (Browse, Pick, Comment, Markup, Agent).
    pub fn owner(&self, cx: &App) -> Owner {
        self.input.read(cx).owner()
    }

    /// The markup layer (tests and the attach flow read and burn it).
    pub fn markup(&self) -> &Entity<Markup> {
        &self.markup
    }

    /// Toggle a mode (Pick or Markup): enter it, or leave it when it is on top.
    pub fn toggle_mode(&mut self, owner: Owner, cx: &mut Context<Self>) {
        self.input.update(cx, |i, _| {
            if i.owner() == owner {
                i.escape();
            } else {
                i.enter(owner);
            }
        });
        self.drain_overlay(cx);
        cx.notify();
    }

    /// "Take Control": the agent's lease ends, the user has the page again.
    pub fn take_control(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |i, _| i.end_lease());
        self.drain_overlay(cx);
        cx.notify();
    }

    fn mode_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.input.read(cx);
        let owner = input.owner();
        if let Some(agent) = input.lease().map(str::to_owned) {
            let flash = self.refused_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(600));
            return Some(
                div()
                    .id("lease-strip")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .h(px(FIND_HEIGHT))
                    .px_2()
                    .text_xs()
                    .bg(if flash { rgb(0xf59e0b) } else { rgb(0x7c3aed) })
                    .text_color(rgb(0xffffff))
                    .child(format!("{agent} is driving"))
                    .child("·")
                    .child(div().id("take-control").px_1().rounded_sm().cursor_pointer().bg(hsla(0., 0., 1., 0.2)).child("Take Control").on_click(cx.listener(|v, _, _, cx| v.take_control(cx))))
                    .into_any_element(),
            );
        }
        if owner != Owner::Markup {
            return None;
        }
        let tool = self.markup.read(cx).tool;
        let tool_button = |id: &'static str, label: &'static str, t: Tool| {
            div()
                .id(id)
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .when(tool == t, |d| d.bg(hsla(0., 0., 0.5, 0.3)))
                .child(label)
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.markup.update(cx, |m, _| m.tool = t);
                    cx.notify();
                }))
        };
        Some(
            div()
                .id("markup-strip")
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(FIND_HEIGHT))
                .px_2()
                .text_xs()
                .bg(hsla(0.0, 0.9, 0.55, 0.15))
                .child("Markup")
                .child(tool_button("tool-pen", "Pen", Tool::Pen))
                .child(tool_button("tool-rect", "Rect", Tool::Rect))
                .child(tool_button("tool-ellipse", "Ellipse", Tool::Ellipse))
                .child(tool_button("tool-arrow", "Arrow", Tool::Arrow))
                .child(div().id("markup-undo").px_1().cursor_pointer().child("Undo").on_click(cx.listener(|v, _, _, cx| {
                    v.markup.update(cx, |m, _| m.undo());
                    cx.notify();
                })))
                .child(div().id("markup-done").px_1().cursor_pointer().child("Done").on_click(cx.listener(|v, _, _, cx| v.toggle_mode(Owner::Markup, cx))))
                .into_any_element(),
        )
    }

    /// Where the page area starts under the bars, in DIPs from the block's top.
    pub fn page_top(&self) -> f32 {
        CHROME_HEIGHT + if self.find.is_some() { FIND_HEIGHT } else { 0.0 } + if self.strip_shown { FIND_HEIGHT } else { 0.0 }
    }

    /// The find bar's state: (text, active, count), when open.
    pub fn find_state(&self) -> Option<(String, u32, u32)> {
        self.find.as_ref().map(|f| (f.text.clone(), f.active, f.count))
    }

    /// The find bar's input (tests type into it).
    pub fn find_input(&self) -> Option<Entity<InputState>> {
        self.find.as_ref().map(|f| f.input.clone())
    }

    /// cmd-F: open the find bar (or focus it), its text selected.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in page"));
            let typed = cx.subscribe_in(&input, window, |v, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    v.find_text(text, true, false);
                    cx.notify();
                }
                InputEvent::PressEnter { shift, .. } => {
                    let text = input.read(cx).value().to_string();
                    v.find_text(text, !shift, true);
                    cx.notify();
                }
                _ => {
                    let _ = window;
                }
            });
            self.find = Some(FindBar { input, text: String::new(), count: 0, active: 0, _typed: typed });
        }
        if let Some(f) = &self.find {
            f.input.update(cx, |i, cx| {
                i.focus(window, cx);
                i.select_all(window, cx);
            });
        }
        cx.notify();
    }

    /// Search `text` (forward or back); `next` continues the current search.
    pub fn find_text(&mut self, text: String, forward: bool, next: bool) {
        let Some(f) = &mut self.find else { return };
        if text.is_empty() {
            (f.count, f.active) = (0, 0);
            f.text.clear();
            self.link.send(AppToHost::StopFind { id: self.id.clone(), clear_selection: true });
            return;
        }
        let next = next && f.text == text;
        f.text = text.clone();
        self.link.send(AppToHost::Find { id: self.id.clone(), text, forward, match_case: false, find_next: next });
    }

    /// Esc or ×: close the bar, clear the highlights, give the page the keyboard back.
    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            self.link.send(AppToHost::StopFind { id: self.id.clone(), clear_selection: false });
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.find.as_ref()?;
        let label = match (f.text.is_empty(), f.count) {
            (true, _) => String::new(),
            (false, 0) => "No matches".into(),
            // CEF's first answer has no active match yet (ordinal 0).
            (false, n) if f.active == 0 => format!("{n} matches"),
            (false, n) => format!("{} of {n}", f.active),
        };
        let button = |id: &'static str, label: &'static str| div().id(id).px_1().rounded_sm().cursor_pointer().hover(|d| d.bg(hsla(0., 0., 0.5, 0.15))).child(label);
        Some(
            div()
                .id("find-bar")
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(FIND_HEIGHT))
                .px_2()
                .text_xs()
                .bg(hsla(0., 0., 0.5, 0.12))
                .child(div().w(px(220.)).child(Input::new(&f.input)))
                .child(div().id("find-count").min_w(px(72.)).child(label))
                .child(button("find-prev", "‹").on_click(cx.listener(|v, _, _, cx| {
                    let t = v.find.as_ref().map(|f| f.text.clone()).unwrap_or_default();
                    v.find_text(t, false, true);
                    cx.notify();
                })))
                .child(button("find-next", "›").on_click(cx.listener(|v, _, _, cx| {
                    let t = v.find.as_ref().map(|f| f.text.clone()).unwrap_or_default();
                    v.find_text(t, true, true);
                    cx.notify();
                })))
                .child(button("find-close", "×").on_click(cx.listener(|v, _, window, cx| v.close_find(window, cx))))
                .into_any_element(),
        )
    }

    /// The open context menu, if any.
    pub fn menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    /// Run menu row `row` (or dismiss with None): app commands here, the rest by the host.
    pub fn choose_menu(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.take() else { return };
        let item = row.and_then(|r| menu.rows.get(r)).filter(|i| i.enabled && i.command != 0);
        let command = match item.map(|i| i.command) {
            Some(MENU_OPEN_LINK_IN_NEW_TAB) => {
                if let Some(url) = menu.link_url.clone() {
                    cx.emit(BrowserEvent::NewWindow(url));
                }
                None
            }
            Some(MENU_COPY_LINK_ADDRESS) => {
                if let Some(url) = &menu.link_url {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                }
                None
            }
            Some(MENU_SAVE_IMAGE_AS) => {
                // The page downloads it (the host's download flow saves it and tells the app).
                if let Some(src) = &menu.src_url {
                    let expr = format!("(()=>{{const a=document.createElement('a');a.href={};a.download='';document.body.append(a);a.click();a.remove();return true}})()", serde_json::Value::String(src.clone()));
                    self.link.send(AppToHost::Eval { id: self.id.clone(), req: 0, expr });
                }
                None
            }
            other => other,
        };
        self.link.send(AppToHost::MenuResult { id: self.id.clone(), req: menu.req, command });
        cx.notify();
    }

    fn menu_element(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let rows = menu.rows.iter().enumerate().map(|(i, r)| {
            if r.command == 0 {
                return div().h(px(MENU_ROW)).flex().items_center().child(div().w_full().h(px(1.)).bg(rgb(0xd0d0d0))).into_any_element();
            }
            let enabled = r.enabled;
            div()
                .id(("menu-row", i))
                .h(px(MENU_ROW))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded_sm()
                .text_sm()
                .when(!enabled, |d| d.text_color(rgb(0xa0a0a0)))
                .when(enabled, |d| d.hover(|d| d.bg(rgb(0x2563eb)).text_color(rgb(0xffffff))))
                .child(if r.checked { "✓" } else { " " })
                .child(r.label.clone())
                .on_mouse_down(MouseButton::Left, cx.listener(move |v, _, _, cx| {
                    cx.stop_propagation();
                    if enabled {
                        v.choose_menu(Some(i), cx);
                    }
                }))
                .into_any_element()
        });
        Some(
            div()
                .id("context-menu")
                .absolute()
                .left(px(menu.at.0))
                .top(px(menu.at.1))
                .w(px(MENU_WIDTH))
                .py(px(4.))
                .bg(rgb(0xffffff))
                .text_color(rgb(0x1a1a1a))
                .border_1()
                .border_color(rgb(0xc8c8c8))
                .rounded_md()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|v, _, _, cx| v.choose_menu(None, cx)))
                .children(rows)
                .into_any_element(),
        )
    }

    /// Set the page zoom (Chrome level) and remember it for this host.
    pub fn set_zoom(&mut self, level: f64, cx: &mut Context<Self>) {
        self.apply_zoom(level);
        self.zoom_prefs.lock().unwrap().set(&self.state.url, level);
        cx.notify();
    }

    fn apply_zoom(&mut self, level: f64) {
        self.zoom_level = level;
        self.overlay.changed(Change::Zoom);
        self.link.send(AppToHost::SetZoom { id: self.id.clone(), level });
    }

    /// cmd-plus / cmd-minus: Chrome's next preset in or out.
    pub fn zoom_step(&mut self, zoom_in: bool, cx: &mut Context<Self>) {
        let level = zoom::step(self.zoom_level, zoom_in);
        self.set_zoom(level, cx);
    }

    /// Share the app's per-host zoom levels.
    pub fn use_zoom_prefs(&mut self, prefs: Arc<Mutex<zoom::ZoomPrefs>>) {
        self.zoom_prefs = prefs;
    }

    /// The page zoom level (Chrome's: 0 is 100 %).
    pub fn zoom_level(&self) -> f64 {
        self.zoom_level
    }

    /// Tests and the parity harness: hear each frame arrive, paint and get acked.
    pub fn set_frame_probe(&mut self, probe: Option<FrameProbe>) {
        self.probe = probe;
    }

    /// The newest frame's sequence number, if one is shown.
    pub fn frame_seq(&self) -> Option<u64> {
        self.frames.seq()
    }

    fn chrome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |label: &'static str, on: bool| {
            div()
                .id(label)
                .px_1()
                .rounded_sm()
                .when(on, |d| d.cursor_pointer().hover(|d| d.bg(hsla(0., 0., 0.5, 0.15))))
                .when(!on, |d| d.opacity(0.35))
                .child(label)
        };
        let id = self.id.clone();
        let (back, fwd) = (self.state.can_back, self.state.can_forward);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(CHROME_HEIGHT))
            .px_2()
            .text_xs()
            .bg(hsla(0., 0., 0.5, 0.08))
            .child(button("‹", back).when(back, |d| {
                let id = id.clone();
                d.on_click(cx.listener(move |v, _, _, _| v.link.send(AppToHost::Back { id: id.clone() })))
            }))
            .child(button("›", fwd).when(fwd, |d| {
                let id = id.clone();
                d.on_click(cx.listener(move |v, _, _, _| v.link.send(AppToHost::Forward { id: id.clone() })))
            }))
            .child(if self.state.loading {
                let id = id.clone();
                button("×", true).on_click(cx.listener(move |v, _, _, _| v.link.send(AppToHost::Stop { id: id.clone() })))
            } else {
                let id = id.clone();
                button("↻", true).on_click(cx.listener(move |v, _, _, _| v.link.send(AppToHost::Reload { id: id.clone(), ignore_cache: false })))
            })
            .child(div().flex_1().min_w_0().overflow_hidden().child(Input::new(&self.address)))
            .when(self.state.loading, |d| d.child("loading"))
            // Modes (part 4's owners): pick an element, mark up the page.
            .child(button("⌖", true).on_click(cx.listener(|v, _, _, cx| v.toggle_mode(Owner::Pick, cx))))
            .child(button("✎", true).on_click(cx.listener(|v, _, _, cx| v.toggle_mode(Owner::Markup, cx))))
            // The zoom label shows only off 100 %; a click resets it.
            .when(self.zoom_level.abs() > 1e-6, |d| {
                d.child(div().id("zoom-label").px_1().rounded_sm().cursor_pointer().bg(hsla(0., 0., 0.5, 0.15)).child(zoom::percent(self.zoom_level)).on_click(cx.listener(|v, _, _, cx| v.set_zoom(0.0, cx))))
            })
            .child(button("⬇", true).on_click(cx.listener(|v, _, _, cx| v.toggle_downloads(cx))))
    }

    fn shown_url(&self) -> String {
        if self.state.url.is_empty() { "about:blank".into() } else { self.state.url.clone() }
    }
}

impl Drop for BrowserView {
    fn drop(&mut self) {
        self.link.close(&self.id);
        self.states.lock().unwrap().remove(&self.id);
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.address_stale && !self.address.read(cx).focus_handle(cx).is_focused(window) {
            self.address_stale = false;
            let url = self.state.url.clone();
            self.address.update(cx, |a, cx| a.set_value(url, window, cx));
        }
        // Also checked here: focus events need an active window, which offscreen ones never are.
        let focused = self.focus.is_focused(window);
        if focused && !self.was_focused {
            cx.emit(BrowserEvent::Focused);
        }
        self.was_focused = focused;
        self.input.update(cx, |i, _| i.set_focused(focused));
        let me = cx.entity().downgrade();
        let (input_el, focus) = (self.input.clone(), self.focus.clone());
        let page = div()
            .id("page")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .cursor(input::cursor_style(&self.state.cursor))
            // A click focuses the page, in the capture phase: part 4's listeners stop the event.
            .capture_any_mouse_down(cx.listener(|v, _, window, cx| {
                window.focus(&v.focus, cx);
            }))
            .bg(rgb(0xffffff));
        let page = input::listen(page, &self.input)
            // Measures the page area; a new size reaches the host after this frame.
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let size = Dips { w: f32::from(bounds.size.width), h: f32::from(bounds.size.height) };
                        let scale = window.scale_factor();
                        let me = me.clone();
                        window.defer(cx, move |_, cx| {
                            let _ = me.update(cx, |v, _| v.laid_out(size, scale));
                        });
                    },
                    // Where the page is, and the window's text input (IME) while focused.
                    move |bounds, _, window, cx| input::paint(&input_el, &focus, bounds, window, cx),
                )
                .absolute()
                .size_full(),
            );
        // The page's newest frame; before the first one, a white sheet with its url.
        let frame = if self.crashed.is_none() { self.frames.element() } else { None };
        if let (Some(p), true, Some(seq)) = (&self.probe, frame.is_some(), self.frames.seq()) {
            p(&FrameEvent::Painted { seq });
        }
        if frame.is_some() {
            // After this draw (GPUI presents it in the same tick): acked only if the window is
            // visible (frame.rs: presentation, not receipt). Reuse is fenced separately by the
            // IOSurface use count, so acking before the GPU finishes is safe.
            let me = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                let visible = window.is_visible();
                let _ = me.update(cx, |v, _| {
                    v.frames.presented(visible);
                    v.send_ack();
                });
            });
        }
        let page = match (&self.crashed, frame) {
            (None, Some(frame)) => page.child(div().absolute().size_full().child(frame)),
            (None, None) => page.child(
                div().absolute().size_full().flex().items_center().justify_center().text_color(rgb(0x606060)).text_sm().child(self.shown_url()),
            ),
            (Some(reason), _) => page.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x202020))
                    .text_color(rgb(0xe0e0e0))
                    .text_sm()
                    .child("This page stopped")
                    .child(div().text_xs().opacity(0.6).child(reason.clone()))
                    .child(
                        div()
                            .id("reload")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x3a3a3a))
                            .cursor_pointer()
                            .child("Reload")
                            .on_click(cx.listener(|v, _, _, cx| v.reload(cx))),
                    ),
            ),
        };
        // Current overlay rects (stale ones are already gone), outlined over the frame.
        let page = page.children(self.overlay.shown().iter().map(|r| {
            div().absolute().left(px(r.x)).top(px(r.y)).w(px(r.w)).h(px(r.h)).border_2().border_color(rgb(0x3b82f6)).bg(hsla(0.6, 0.9, 0.6, 0.15))
        }));
        // Markup above the frame and the highlights, while marking up or when shapes exist.
        let marking = self.owner(cx) == Owner::Markup || !self.markup.read(cx).shapes().is_empty();
        let page = page.when(marking, |p| p.child(markup::element(self.markup.clone())));
        // The context menu over everything in the page; it takes the mouse first (occlude).
        let menu = self.menu_element(cx);
        let page = page.children(menu);
        // A JS dialog and the downloads list sit beside the page element, over it: keys and clicks in
        // them must not bubble into the page's input listeners.
        let page = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(page)
            .child(self.dialogs.clone())
            .children(self.downloads_panel(cx));
        let strip = self.mode_strip(cx);
        self.strip_shown = strip.is_some();
        // After any input event, drain what BrowserInput routed away from the page (its own
        // listeners may stop the event, so the drain is deferred to after dispatch).
        let drain = |cx: &mut Context<Self>, window: &mut Window| {
            let me = cx.entity().downgrade();
            window.defer(cx, move |_, cx| {
                let _ = me.update(cx, |v, cx| v.drain_overlay(cx));
            });
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .capture_any_mouse_down(cx.listener(move |_, _, window, cx| drain(cx, window)))
            .capture_any_mouse_up(cx.listener(move |_, _, window, cx| drain(cx, window)))
            .on_mouse_move(cx.listener(move |v, _, window, cx| {
                if v.owner(cx) != Owner::Browse {
                    drain(cx, window);
                }
            }))
            .capture_key_down(cx.listener(move |_, _, window, cx| drain(cx, window)))
            .key_context("Browser")
            // Escape closes an open context menu, else an open find bar, before the page sees it.
            .capture_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key != "escape" {
                    return;
                }
                if v.menu.is_some() {
                    cx.stop_propagation();
                    v.choose_menu(None, cx);
                } else if v.find.is_some() {
                    cx.stop_propagation();
                    v.close_find(window, cx);
                }
            }))
            .on_action(cx.listener(|v, _: &OpenFind, window, cx| v.open_find(window, cx)))
            .on_action(cx.listener(|v, _: &ZoomIn, _, cx| v.zoom_step(true, cx)))
            .on_action(cx.listener(|v, _: &ZoomOut, _, cx| v.zoom_step(false, cx)))
            .on_action(cx.listener(|v, _: &ZoomReset, _, cx| v.set_zoom(0.0, cx)))
            .on_action(cx.listener(|v, _: &FocusAddress, window, cx| {
                v.address.update(cx, |a, cx| {
                    a.focus(window, cx);
                    a.select_all(window, cx);
                });
            }))
            .child(self.chrome(cx))
            .children(self.find_bar(cx))
            .children(strip)
            .child(page)
    }
}

/// cmd-L (ctrl-L elsewhere) puts the cursor in the address bar, once per app.
fn bind_keys(cx: &mut App) {
    struct Bound;
    impl Global for Bound {}
    if cx.has_global::<Bound>() {
        return;
    }
    cx.set_global(Bound);
    let cmd = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
    cx.bind_keys([
        KeyBinding::new(&format!("{cmd}-l"), FocusAddress, Some("Browser")),
        KeyBinding::new(&format!("{cmd}-f"), OpenFind, Some("Browser")),
        KeyBinding::new(&format!("{cmd}-="), ZoomIn, Some("Browser")),
        KeyBinding::new(&format!("{cmd}-+"), ZoomIn, Some("Browser")),
        KeyBinding::new(&format!("{cmd}--"), ZoomOut, Some("Browser")),
        KeyBinding::new(&format!("{cmd}-0"), ZoomReset, Some("Browser")),
    ]);
}

/// One `BrowserView` per browser block in the layouts. A view lives while its block does (shown
/// or not: a hidden tab keeps its page); the host page is made at the block's first paint.
#[derive(Default)]
pub struct Browsers {
    link: Option<Arc<HostClient>>,
    views: HashMap<BrowserId, (Entity<BrowserView>, Subscription)>,
    states: Arc<Mutex<HashMap<BrowserId, BrowserState>>>,
    /// `herdr-app do browser.*` (part 5); hears every host message.
    agent: Option<Arc<Mutex<Agent>>>,
    /// The browser that last took the keyboard: agent commands' default target.
    pub focused: Option<BrowserId>,
    /// Zoom per host, `$HERDR_APP_HOME/browser/zoom.json`, loaded with the first block.
    zoom: Option<Arc<Mutex<zoom::ZoomPrefs>>>,
    /// Every download this run, shared by all blocks (`browser.downloads` reads it).
    pub downloads: Downloads,
}

impl Browsers {
    pub fn view(&self, id: &str) -> Option<&Entity<BrowserView>> {
        self.views.get(id).map(|(v, _)| v)
    }

    pub fn link_arc(&mut self) -> Arc<HostClient> {
        if self.link.is_none() {
            let link = HostClient::new(HostConfig::from_env());
            let agent = Arc::new(Mutex::new(Agent::new(link.config().dir.clone())));
            let a = agent.clone();
            link.set_tap(Arc::new(move |msg| a.lock().unwrap().on_host(msg)));
            (self.link, self.agent) = (Some(link), Some(agent));
        }
        self.link.clone().expect("made above")
    }

    /// For tests: browsers over a given link (its agent is made here too).
    pub fn with_link(link: Arc<HostClient>) -> Self {
        let agent = Arc::new(Mutex::new(Agent::new(link.config().dir.clone())));
        let a = agent.clone();
        link.set_tap(Arc::new(move |msg| a.lock().unwrap().on_host(msg)));
        Self { link: Some(link), agent: Some(agent), ..Default::default() }
    }

    /// `herdr-app do browser.<name>`: answered through `reply` (now, or when the host answers).
    /// `Open` and `Close` come back for the window, which owns the layout.
    pub fn run(&mut self, name: &str, args: &serde_json::Value, block_of_pane: &dyn Fn(&str) -> Option<BrowserId>, reply: Reply, cx: &mut App) -> Step {
        self.link_arc();
        let agent = self.agent.clone().expect("made with the link");
        // The page the command targets must exist on the host, shown or not.
        let target = args.get("id").and_then(serde_json::Value::as_str).map(str::to_owned)
            .or_else(|| args.get("pane").and_then(serde_json::Value::as_str).and_then(block_of_pane))
            .or_else(|| self.focused.clone());
        if let Some((view, _)) = target.and_then(|t| self.views.get(&t)) {
            view.update(cx, |v, _| v.ensure_page());
        }
        let focus = Focus { focused: self.focused.clone().filter(|f| self.views.contains_key(f)), block_of_pane };
        agent.lock().unwrap().run(name, args, self, &focus, reply)
    }

    // HERDR: browser block (part 5). The person picked an element: the agent captures it for `browser.pick`.
    pub fn picked(&mut self, id: &str, x: f32, y: f32) {
        let link = self.link_arc();
        if let Some(agent) = &self.agent {
            agent.lock().unwrap().picked(&id.to_owned(), x, y, &*link);
        }
    }

    /// Match the views to `blocks` (every browser block in every layout, as id and block):
    /// new blocks get a view (`on_event` hears its url changes), gone ones close their page.
    pub fn sync<W: 'static>(
        &mut self,
        blocks: Vec<(String, BrowserBlock)>,
        window: &mut Window,
        cx: &mut Context<W>,
        on_event: impl Fn(&mut W, &str, &BrowserEvent, &mut Window, &mut Context<W>) + Clone + 'static,
    ) {
        let keep: HashSet<&str> = blocks.iter().map(|(id, _)| id.as_str()).collect();
        let gone: Vec<BrowserId> = self.views.keys().filter(|id| !keep.contains(id.as_str())).cloned().collect();
        for id in gone {
            self.views.remove(&id);
            if let Some(agent) = &self.agent {
                agent.lock().unwrap().closed(&id);
            }
            if self.focused.as_ref() == Some(&id) {
                self.focused = None;
            }
        }
        for (id, block) in blocks {
            if let Some((v, _)) = self.views.get(&id) {
                v.update(cx, |v, cx| v.set_block(block, cx));
                continue;
            }
            let (link, states) = (self.link_arc(), self.states.clone());
            let downloads = self.downloads.clone();
            let dir = link.config().dir.clone();
            let prefs = self.zoom.get_or_insert_with(|| Arc::new(Mutex::new(zoom::ZoomPrefs::load(dir.join("zoom.json"))))).clone();
            let view = cx.new(|cx| {
                let mut v = BrowserView::new(&id, block, link, states, window, cx);
                v.use_downloads(downloads, cx);
                v.use_zoom_prefs(prefs);
                v
            });
            let key = id.clone();
            let on = on_event.clone();
            let sub = cx.subscribe_in(&view, window, move |w, _, ev, window, cx| on(w, &key, ev, window, cx));
            self.views.insert(id, (view, sub));
        }
    }

    pub fn ids(&self) -> Vec<BrowserId> {
        let mut ids: Vec<_> = self.views.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl BrowserRegistry for Browsers {
    fn state(&self, id: &BrowserId) -> Option<BrowserState> {
        self.states.lock().unwrap().get(id).cloned()
    }
    fn link(&self) -> &dyn HostLink {
        self.link.as_deref().expect("a browser block made the host link")
    }
}

/// Socket path checks for tests and `herdr-app doctor`-style reports.
pub fn socket_of(dir: &Path) -> PathBuf {
    dir.join("host.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_browser_protocol::Size;

    fn tmp(name: &str) -> PathBuf {
        // Short: macOS socket paths stop at 104 bytes.
        let d = PathBuf::from(format!("/tmp/hb-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn recv(rx: &async_channel::Receiver<HostToApp>, want: impl Fn(&HostToApp) -> bool) -> HostToApp {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(m) if want(&m) => return m,
                Ok(_) => {}
                Err(_) if Instant::now() > deadline => panic!("no matching host event in time"),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// A fake host: accepts on the socket, records lines, answers Url for Create and Navigate.
    fn fake_host(sock: PathBuf) -> (Arc<Mutex<Vec<AppToHost>>>, mpsc::Sender<()>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (kill, killed) = mpsc::channel::<()>();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let s = seen.clone();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
            let mut r = BufReader::new(stream);
            let mut line = String::new();
            loop {
                if killed.try_recv().is_ok() {
                    return; // drops the socket: the app sees the host die
                }
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(_) => continue,
                }
                let msg: AppToHost = protocol::decode(&line).unwrap();
                let reply = match &msg {
                    AppToHost::Create { id, url: Some(url), .. } | AppToHost::Navigate { id, url } => Some(HostToApp::Url { id: id.clone(), url: url.clone() }),
                    _ => None,
                };
                s.lock().unwrap().push(msg);
                if let Some(m) = reply {
                    let _ = writeln!(out, "{}", protocol::encode(&m));
                }
            }
        });
        (seen, kill)
    }

    #[::core::prelude::v1::test]
    fn the_address_bar_opens_urls_hosts_and_searches() {
        use crate::settings::SearchEngine::{Bing, Google};
        let go = |t: &str| address_to_url(t, Google);
        assert_eq!(go("  "), None);
        assert_eq!(go("https://example.com/a?b=1").as_deref(), Some("https://example.com/a?b=1"));
        assert_eq!(go("example.com").as_deref(), Some("https://example.com"));
        assert_eq!(go("docs.rs/gpui/latest").as_deref(), Some("https://docs.rs/gpui/latest"));
        assert_eq!(go("localhost:3000/x").as_deref(), Some("http://localhost:3000/x"));
        assert_eq!(go("127.0.0.1:8080").as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(go("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(go("file:///tmp/a.html").as_deref(), Some("file:///tmp/a.html"));
        assert_eq!(go("how do iosurfaces work").as_deref(), Some("https://www.google.com/search?q=how+do+iosurfaces+work"));
        assert_eq!(go("rust").as_deref(), Some("https://www.google.com/search?q=rust"));
        assert_eq!(go("what is 3.5 kg").as_deref(), Some("https://www.google.com/search?q=what+is+3.5+kg"));
        assert_eq!(address_to_url("cef osr", Bing).as_deref(), Some("https://www.bing.com/search?q=cef+osr"));
    }

    fn size() -> Size {
        Size { w: 400.0, h: 300.0 }
    }

    #[::core::prelude::v1::test]
    fn link_says_hello_first_then_routes_events_by_block() {
        let dir = tmp("route");
        let (seen, _kill) = fake_host(socket_of(&dir));
        let link = HostClient::new(HostConfig { dir: dir.clone(), program: "/nonexistent".into(), args: vec![], cdp_port: None });
        let (a, b) = (link.subscribe("a"), link.subscribe("b"));
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 2.0, profile: None });
        link.send(AppToHost::Create { id: "b".into(), url: Some("https://b.test".into()), size: size(), scale: 2.0, profile: None });
        assert_eq!(recv(&a, |m| matches!(m, HostToApp::Url { .. })), HostToApp::Url { id: "a".into(), url: "https://a.test".into() });
        assert_eq!(recv(&b, |m| matches!(m, HostToApp::Url { .. })), HostToApp::Url { id: "b".into(), url: "https://b.test".into() });
        assert!(a.try_recv().is_err(), "a hears only its own events");
        let seen = seen.lock().unwrap().clone();
        assert!(matches!(&seen[0], AppToHost::Hello { profile_dir, .. } if profile_dir.ends_with("profile")), "{seen:?}");
        let creates: Vec<_> = seen.iter().filter_map(|m| match m { AppToHost::Create { id, .. } => Some(id.as_str()), _ => None }).collect();
        assert_eq!(creates, ["a", "b"], "each page made once: {seen:?}");
    }

    #[::core::prelude::v1::test]
    fn closing_a_block_closes_its_page_and_forgets_it() {
        let dir = tmp("close");
        let (seen, _kill) = fake_host(socket_of(&dir));
        let link = HostClient::new(HostConfig { dir, program: "/nonexistent".into(), args: vec![], cdp_port: None });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 1.0, profile: None });
        recv(&a, |m| matches!(m, HostToApp::Url { .. }));
        link.close("a");
        link.close("a"); // twice: one Close
        assert!(link.open_ids().is_empty());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !seen.lock().unwrap().iter().any(|m| matches!(m, AppToHost::Close { .. })) {
            assert!(Instant::now() < deadline, "no Close");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(seen.lock().unwrap().iter().filter(|m| matches!(m, AppToHost::Close { .. })).count(), 1);
    }

    #[::core::prelude::v1::test]
    fn a_dead_host_is_a_crash_and_the_next_send_makes_the_pages_again() {
        let dir = tmp("crash");
        let (_seen, kill) = fake_host(socket_of(&dir));
        let link = HostClient::new(HostConfig { dir: dir.clone(), program: "/nonexistent".into(), args: vec![], cdp_port: None });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 2.0, profile: None });
        link.send(AppToHost::Navigate { id: "a".into(), url: "https://a.test/2".into() });
        recv(&a, |m| matches!(m, HostToApp::Url { url, .. } if url.ends_with("/2")));
        kill.send(()).unwrap();
        recv(&a, |m| matches!(m, HostToApp::Crashed { .. }));
        // A new host on the same socket: the next message replays the open page at its last url.
        let _ = std::fs::remove_file(socket_of(&dir));
        let (seen2, _kill2) = fake_host(socket_of(&dir));
        link.send(AppToHost::Reload { id: "a".into(), ignore_cache: false });
        recv(&a, |m| matches!(m, HostToApp::Url { url, .. } if url.ends_with("/2")));
        let seen2 = seen2.lock().unwrap().clone();
        assert!(matches!(&seen2[0], AppToHost::Hello { .. }), "{seen2:?}");
        assert!(matches!(&seen2[1], AppToHost::Create { id, url: Some(u), scale, .. } if id == "a" && u.ends_with("/2") && *scale == 2.0), "{seen2:?}");
        assert!(matches!(&seen2[2], AppToHost::Reload { .. }), "{seen2:?}");
    }

    #[::core::prelude::v1::test]
    fn no_host_program_is_a_crash_not_a_hang() {
        let dir = tmp("nohost");
        let link = HostClient::new(HostConfig { dir, program: "/nonexistent/herdr-browser-host".into(), args: vec![], cdp_port: None });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: None, size: size(), scale: 1.0, profile: None });
        let m = recv(&a, |m| matches!(m, HostToApp::Crashed { .. }));
        assert!(matches!(m, HostToApp::Crashed { reason, .. } if reason.contains("can't start")));
    }

    /// Against the real stub host binary (part 2), when one is given:
    /// `HERDR_BROWSER_HOST=<path to herdr-browser-host> cargo test --bin herdr-app stub_host`.
    #[::core::prelude::v1::test]
    fn stub_host_spawns_lazily_answers_and_restarts_after_a_kill() {
        let Some(program) = std::env::var_os("HERDR_BROWSER_HOST").map(PathBuf::from) else {
            eprintln!("HERDR_BROWSER_HOST unset: stub host test skipped");
            return;
        };
        let dir = tmp("stub");
        let link = HostClient::new(HostConfig { dir: dir.clone(), program, args: vec!["--stub".into()], cdp_port: None });
        let a = link.subscribe("a");
        assert!(UnixStream::connect(socket_of(&dir)).is_err(), "no host before the first send");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://example.com".into()), size: size(), scale: 2.0, profile: None });
        recv(&a, |m| matches!(m, HostToApp::Url { url, .. } if url == "https://example.com"));
        let frame = recv(&a, |m| matches!(m, HostToApp::Frame { .. }));
        assert!(matches!(frame, HostToApp::Frame { size: PixelSize { w: 800, h: 600 }, .. }), "{frame:?}");
        // Kill the host: the view hears a crash, the next send starts a new one and makes the page again.
        let pid = String::from_utf8(Command::new("pgrep").args(["-f", &format!("{}", socket_of(&dir).display())]).output().unwrap().stdout).unwrap();
        let pid = pid.lines().next().expect("stub host running").trim().to_owned();
        Command::new("kill").args(["-9", &pid]).status().unwrap();
        recv(&a, |m| matches!(m, HostToApp::Crashed { .. }));
        link.send(AppToHost::Reload { id: "a".into(), ignore_cache: false });
        recv(&a, |m| matches!(m, HostToApp::Url { url, .. } if url == "https://example.com"));
        drop(link);
        std::thread::sleep(Duration::from_millis(700));
        let left = Command::new("pgrep").args(["-f", &format!("{}", socket_of(&dir).display())]).output().unwrap();
        assert!(left.stdout.is_empty(), "host left running after the app let go");
    }
}
