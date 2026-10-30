//! Part 1 (LAB-419): `BrowserView`, the GPUI entity for one browser block.
//! Owns the host connection (`HostLink`), `BrowserState`, resize and focus.
//!
//! One `HostClient` per app: it starts `herdr-browser-host` on the first send, talks JSON lines
//! over `$HERDR_APP_HOME/browser/host.sock`, and after the host dies starts it again on the next
//! send, replaying every open browser. Each block gets a `BrowserView` that turns host events into
//! `BrowserState`, sizes the page to the block, hands frames to a `FrameSink` and shows a reload
//! card after a crash. `Browsers` keeps one view per browser block in the layouts.
use super::agent::{Agent, Focus, Reply, Step};
use super::input::{self, BrowserInput};
use super::{BrowserRegistry, BrowserState, FrameSink, FrameView, HostLink};
use crate::layout::BrowserBlock;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_base::input::{Input, InputEvent, InputState};
use herdr_browser_protocol::{self as protocol, AppToHost, BrowserId, HostToApp, PixelSize, Size as Dips};
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
}

impl HostConfig {
    pub fn from_env() -> Self {
        let program = std::env::var_os("HERDR_BROWSER_HOST").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("herdr-browser-host"))).unwrap_or_else(|| "herdr-browser-host".into())
        });
        let args = std::env::var("HERDR_BROWSER_HOST_ARGS").map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default();
        Self { dir: crate::mods::app_dir().join("browser"), program, args }
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("host.sock")
    }
}

/// What a browser needs to be made again after the host restarts.
#[derive(Clone, Debug, PartialEq)]
struct Open {
    url: Option<String>,
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
                AppToHost::Create { id, url, size, scale } => {
                    s.open.insert(id.clone(), Open { url: url.clone(), size: *size, scale: *scale });
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
                            self.write(&AppToHost::Create { id, url: o.url, size: o.size, scale: o.scale });
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
            cdp_port: None,
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
        | H::CdpTarget { id, .. } => Some(id),
    }
}

actions!(browser, [FocusAddress]);

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
}

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
        match msg {
            H::Url { url, .. } => {
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
                // Acked once painted (`send_ack` after the draw), so the host sends at most one ahead.
                if let Err(e) = self.frames.on_frame(seq, surface, size, scale) {
                    tracing::warn!("browser {}: frame {seq}: {e:#}", self.id);
                    // Unpaintable: ack anyway, or the host waits on it forever.
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
                self.link.send(AppToHost::Create { id: self.id.clone(), url, size, scale });
            }
            Some((s, k)) if (s.w - size.w).abs() < 0.5 && (s.h - size.h).abs() < 0.5 && k == scale => return,
            Some(_) => {
                self.frames.on_resize_pending(size);
                self.link.send(AppToHost::Resize { id: self.id.clone(), size, scale });
            }
        }
        self.sized = Some((size, scale));
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
        }
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
            .h(px(28.))
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
            .flex_1()
            .min_h_0()
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
        if frame.is_some() {
            let me = cx.entity().downgrade();
            window.defer(cx, move |_, cx| {
                let _ = me.update(cx, |v, _| v.send_ack());
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("Browser")
            .on_action(cx.listener(|v, _: &FocusAddress, window, cx| {
                v.address.update(cx, |a, cx| {
                    a.focus(window, cx);
                    a.select_all(window, cx);
                });
            }))
            .child(self.chrome(cx))
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
    cx.bind_keys([KeyBinding::new(if cfg!(target_os = "macos") { "cmd-l" } else { "ctrl-l" }, FocusAddress, Some("Browser"))]);
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
    pub fn run(&mut self, name: &str, args: &serde_json::Value, block_of_pane: &dyn Fn(&str) -> Option<BrowserId>, reply: Reply) -> Step {
        self.link_arc();
        let agent = self.agent.clone().expect("made with the link");
        let focus = Focus { focused: self.focused.clone().filter(|f| self.views.contains_key(f)), block_of_pane };
        agent.lock().unwrap().run(name, args, self, &focus, reply)
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
            let view = cx.new(|cx| BrowserView::new(&id, block, link, states, window, cx));
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
        let link = HostClient::new(HostConfig { dir: dir.clone(), program: "/nonexistent".into(), args: vec![] });
        let (a, b) = (link.subscribe("a"), link.subscribe("b"));
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 2.0 });
        link.send(AppToHost::Create { id: "b".into(), url: Some("https://b.test".into()), size: size(), scale: 2.0 });
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
        let link = HostClient::new(HostConfig { dir, program: "/nonexistent".into(), args: vec![] });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 1.0 });
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
        let link = HostClient::new(HostConfig { dir: dir.clone(), program: "/nonexistent".into(), args: vec![] });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://a.test".into()), size: size(), scale: 2.0 });
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
        let link = HostClient::new(HostConfig { dir, program: "/nonexistent/herdr-browser-host".into(), args: vec![] });
        let a = link.subscribe("a");
        link.send(AppToHost::Create { id: "a".into(), url: None, size: size(), scale: 1.0 });
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
        let link = HostClient::new(HostConfig { dir: dir.clone(), program, args: vec!["--stub".into()] });
        let a = link.subscribe("a");
        assert!(UnixStream::connect(socket_of(&dir)).is_err(), "no host before the first send");
        link.send(AppToHost::Create { id: "a".into(), url: Some("https://example.com".into()), size: size(), scale: 2.0 });
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
