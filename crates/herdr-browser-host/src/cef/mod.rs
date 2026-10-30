//! The real host: one windowless (OSR) CEF browser per block id, frames by accelerated paint.
//!
//! Threads: CEF runs on the main thread with an external message pump; a reader thread decodes
//! socket lines into a channel the main loop drains between `do_message_loop_work` calls. Every
//! CEF callback therefore runs on the main thread too, so host state lives in a thread-local.
//! Rule for that state: never call into CEF while it is borrowed (CEF may call back synchronously).

mod blit;
mod handlers;
mod keys;
mod mac_app;

use cef::args::Args;
use cef::*;
use herdr_browser_protocol::{
    decode, encode, AppToHost, EditCommand, HostToApp, ImeEvent, InputEvent, KeyKind, Modifiers, MouseButton, MouseKind, PixelSize, Size,
    PROTOCOL_VERSION,
};
use herdr_browser_surface::{FramePool, Surface, Transport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub(crate) struct Entry {
    browser: Option<Browser>,
    size: Size,
    scale: f32,
    pool: FramePool,
    /// The newest frame sent, kept for `Screenshot`.
    last: Option<Surface>,
    /// Keeps the DevTools observer that reports the CDP target id alive.
    _devtools: Option<Registration>,
    /// Input waits here until the page can take it: Chromium drops events sent between the first
    /// load finishing and the first frame painted after it. See `input_ready`.
    queued: Vec<InputEvent>,
    loaded: bool,
    input_ready: bool,
    /// Copies of the newest page and popup paints, composed into each frame sent.
    view: Option<Surface>,
    popup: Option<Surface>,
    popup_shown: bool,
    /// The popup's bounds in DIPs (x, y, w, h).
    popup_rect: Option<(i32, i32, i32, i32)>,
}

pub(crate) struct Host {
    out: UnixStream,
    browsers: HashMap<String, Entry>,
    blitter: Option<blit::Blitter>,
    downloads: PathBuf,
    next_req: u64,
    dialogs: HashMap<u64, JsdialogCallback>,
    choosers: HashMap<u64, FileDialogCallback>,
    quit: bool,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    /// Browsers CEF has created and not yet finished closing (`on_before_close`).
    pub(crate) static OPEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Set by the cookie store's flush callback.
    pub(crate) static FLUSHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How long `cef::shutdown` may take before the host exits on its own (Chromium's watchdog is longer).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

unsafe extern "C" {
    fn _exit(code: i32) -> !;
}

fn debug(args: std::fmt::Arguments) {
    if std::env::var_os("HERDR_BROWSER_HOST_DEBUG").is_some() {
        eprintln!("herdr-browser-host: {args}");
    }
}

/// Borrow host state briefly. Never call CEF inside `f`.
pub(crate) fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    HOST.with_borrow_mut(|h| h.as_mut().map(f))
}

pub(crate) fn send(msg: HostToApp) {
    with_host(|h| {
        let mut line = encode(&msg);
        line.push('\n');
        if h.out.write_all(line.as_bytes()).is_err() {
            h.quit = true;
        }
    });
}

/// The DIP size and scale of a browser, for `view_rect` / `screen_info`.
pub(crate) fn view_of(id: &str) -> Option<(Size, f32)> {
    with_host(|h| h.browsers.get(id).map(|e| (e.size, e.scale))).flatten()
}

pub(crate) fn next_req() -> u64 {
    with_host(|h| {
        h.next_req += 1;
        h.next_req
    })
    .unwrap_or(0)
}

pub(crate) fn downloads_dir() -> PathBuf {
    with_host(|h| h.downloads.clone()).unwrap_or_else(std::env::temp_dir)
}

pub(crate) fn keep_dialog(req: u64, cb: JsdialogCallback) {
    with_host(|h| h.dialogs.insert(req, cb));
}

pub(crate) fn keep_chooser(req: u64, cb: FileDialogCallback) {
    with_host(|h| h.choosers.insert(req, cb));
}

/// A painted CEF surface arrived: blit it into a pool surface and send it if nothing is in flight.
pub(crate) fn on_frame(id: &str, src: *mut std::ffi::c_void, w: u32, h: u32) {
    paint(id, Some((false, src, w, h)));
}

/// A `<select>` list, datalist or other popup widget painted (PET_POPUP).
pub(crate) fn on_popup_frame(id: &str, src: *mut std::ffi::c_void, w: u32, h: u32) {
    paint(id, Some((true, src, w, h)));
}

/// The popup widget opened or closed (`OnPopupShow`). Closing repaints the page without it.
pub(crate) fn on_popup_show(id: &str, show: bool) {
    let changed = with_host(|h| {
        h.browsers.get_mut(id).map(|e| {
            let was = e.popup_shown;
            e.popup_shown = show;
            if !show {
                e.popup = None;
                e.popup_rect = None;
            }
            was && !show
        })
    })
    .flatten()
    .unwrap_or(false);
    if changed {
        paint(id, None);
    }
}

/// Where the popup goes, in DIPs from the view's top-left (`OnPopupSize`).
pub(crate) fn on_popup_size(id: &str, x: i32, y: i32, w: i32, h: i32) {
    with_host(|host| host.browsers.get_mut(id).map(|e| e.popup_rect = Some((x, y, w.max(0), h.max(0)))));
}

/// Keep a copy of a freshly painted layer (CEF's surface dies with the callback), then compose the
/// page and, if open, its popup at the popup's bounds into a pool surface and send it.
fn paint(id: &str, layer: Option<(bool, *mut std::ffi::c_void, u32, u32)>) {
    let ready = with_host(|host| {
        let entry = host.browsers.get_mut(id)?;
        let blitter = host.blitter.as_ref()?;
        let res = (|| -> Result<_, String> {
            if let Some((popup, src, w, h)) = layer {
                let px = PixelSize { w, h };
                let slot = if popup { &mut entry.popup } else { &mut entry.view };
                if slot.as_ref().is_none_or(|s| s.size() != px) {
                    *slot = Some(herdr_browser_surface::create_pixels(px).map_err(|e| format!("surface: {e}"))?);
                }
                let copy = slot.as_ref().expect("just made");
                blitter.copy(src, copy.as_ptr(), w as usize, h as usize)?;
            }
            let Some(view) = entry.view.as_ref() else { return Ok(None) };
            let vs = view.size();
            let dst = entry.pool.acquire(vs).map_err(|e| format!("surface: {e}"))?;
            blitter.copy(view.as_ptr(), dst.as_ptr(), vs.w as usize, vs.h as usize)?;
            if let (true, Some(popup), Some(rect)) = (entry.popup_shown, entry.popup.as_ref(), entry.popup_rect) {
                let ps = popup.size();
                let place = popup_place(rect, entry.scale, (ps.w, ps.h), (vs.w, vs.h));
                if let Some((from, to, w, h)) = place {
                    blitter.copy_region(popup.as_ptr(), (ps.w as usize, ps.h as usize), from, dst.as_ptr(), (vs.w as usize, vs.h as usize), to, w, h)?;
                }
            }
            Ok(entry.pool.submit(dst).map(|f| (f.seq, f.surface.clone(), entry.scale)))
        })();
        Some(res)
    })
    .flatten();
    let woke = with_host(|h| h.browsers.get_mut(id).filter(|e| e.loaded && !e.input_ready).map(|e| {
        e.input_ready = true;
        std::mem::take(&mut e.queued)
    })).flatten();
    if let Some(events) = woke {
        replay(id, events);
    }
    match ready {
        Some(Ok(Some((seq, s, scale)))) => send_frame(id, seq, s, scale),
        Some(Err(e)) => send(HostToApp::Error { id: Some(id.into()), message: format!("frame: {e}") }),
        _ => {}
    }
}

/// Where a popup of `ps` pixels lands in a frame of `vs` pixels, from its DIP `rect` at `scale`:
/// (source origin, destination origin, width, height), clipped to both. None if fully outside.
pub(crate) fn popup_place(rect: (i32, i32, i32, i32), scale: f32, ps: (u32, u32), vs: (u32, u32)) -> Option<((usize, usize), (usize, usize), usize, usize)> {
    let (x, y) = ((rect.0 as f32 * scale).round() as i64, (rect.1 as f32 * scale).round() as i64);
    // Source offset when the popup starts left of / above the frame.
    let (sx, sy) = ((-x).max(0), (-y).max(0));
    let (dx, dy) = (x.max(0), y.max(0));
    let w = (ps.0 as i64 - sx).min(vs.0 as i64 - dx);
    let h = (ps.1 as i64 - sy).min(vs.1 as i64 - dy);
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(((sx as usize, sy as usize), (dx as usize, dy as usize), w as usize, h as usize))
}

/// The page finished (or started) loading. Input becomes deliverable on the next frame.
pub(crate) fn on_loading(id: &str, loading: bool) {
    if !loading {
        with_host(|h| h.browsers.get_mut(id).map(|e| e.loaded = true));
    }
}

fn replay(id: &str, events: Vec<InputEvent>) {
    let Some(host) = browser(id).and_then(|b| b.host()) else { return };
    for ev in events {
        input(&host, ev);
    }
}

fn send_frame(id: &str, seq: u64, mut s: Surface, scale: f32) {
    match s.export(Transport::GlobalId) {
        Ok(surface) => {
            let size = s.size();
            with_host(|h| h.browsers.get_mut(id).map(|e| e.last = Some(s)));
            send(HostToApp::Frame { id: id.into(), seq, surface, size, scale });
        }
        Err(e) => send(HostToApp::Error { id: Some(id.into()), message: format!("export: {e}") }),
    }
}

enum Incoming {
    Msg(AppToHost),
    Bad(String),
    Gone,
}

/// Entry point for every process of the bundle. Helpers (`--type=...`) run CEF's subprocess and
/// exit; the browser process serves `socket`.
pub fn main(socket: Option<&Path>) -> i32 {
    let is_helper = std::env::args().any(|a| a.starts_with("--type="));
    let exe = std::env::current_exe().expect("current exe");
    let loader = library_loader::LibraryLoader::new(&exe, is_helper);
    if !loader.load() {
        eprintln!("herdr-browser-host: cannot load the CEF framework (run from the .app bundle, see docs/fork-host.md)");
        return 1;
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = Args::new();
    if is_helper {
        return execute_process(Some(args.as_main_args()), None::<&mut App>, std::ptr::null_mut());
    }
    let Some(socket) = socket else {
        eprintln!("usage: herdr-browser-host --socket <path>");
        return 2;
    };
    match serve(socket, &args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("herdr-browser-host: {e}");
            1
        }
    }
}

fn cef_version() -> String {
    let v = String::from_utf8_lossy(sys::CEF_VERSION);
    v.trim_end_matches('\0').to_string()
}

fn pick_port(want: Option<u16>) -> io::Result<Option<u16>> {
    match want {
        None => Ok(None),
        Some(0) => Ok(Some(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())),
        Some(p) => Ok(Some(p)),
    }
}

fn serve(socket: &Path, args: &Args) -> io::Result<()> {
    mac_app::setup();
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    let (stream, _) = listener.accept()?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // Wait for Hello: CEF's profile and CDP port come from it.
    let (profile_dir, cdp) = loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        match decode::<AppToHost>(&line) {
            Ok(AppToHost::Hello { profile_dir, cdp_port, .. }) => break (PathBuf::from(profile_dir), cdp_port),
            Ok(AppToHost::Shutdown) => return Ok(()),
            Ok(_) => writeln!(out, "{}", encode(&HostToApp::Error { id: None, message: "send hello first".into() }))?,
            Err(e) => writeln!(out, "{}", encode(&HostToApp::Error { id: None, message: format!("bad message: {e}") }))?,
        }
    };
    let cdp = pick_port(cdp)?;
    std::fs::create_dir_all(&profile_dir)?;
    let downloads = crate::downloads::dir();

    let mut app = handlers::app();
    let settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        root_cache_path: CefString::from(profile_dir.to_string_lossy().as_ref()),
        cache_path: CefString::from(profile_dir.join("default").to_string_lossy().as_ref()),
        persist_session_cookies: 1,
        remote_debugging_port: cdp.map_or(0, i32::from),
        log_severity: LogSeverity::from(sys::cef_log_severity_t::LOGSEVERITY_WARNING),
        ..Default::default()
    };
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        let msg = "CEF failed to initialize (is another host using this profile?)";
        writeln!(out, "{}", encode(&HostToApp::Crashed { id: None, reason: msg.into() }))?;
        return Err(io::Error::other(msg));
    }

    HOST.with_borrow_mut(|h| {
        *h = Some(Host {
            out: out.try_clone().expect("socket clone"),
            browsers: HashMap::new(),
            blitter: blit::Blitter::new(),
            downloads,
            next_req: 0,
            dialogs: HashMap::new(),
            choosers: HashMap::new(),
            quit: false,
        })
    });
    if with_host(|h| h.blitter.is_none()).unwrap_or(true) {
        send(HostToApp::Error { id: None, message: "no Metal device: frames cannot be copied".into() });
    }
    send(HostToApp::Ready { version: PROTOCOL_VERSION, cef_version: cef_version(), cdp_port: cdp });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let m = match decode::<AppToHost>(&line) {
                Ok(m) => Incoming::Msg(m),
                Err(e) => Incoming::Bad(e.to_string()),
            };
            if tx.send(m).is_err() {
                return;
            }
        }
        let _ = tx.send(Incoming::Gone);
    });

    loop {
        do_message_loop_work();
        while let Ok(m) = rx.try_recv() {
            match m {
                Incoming::Msg(AppToHost::Shutdown) | Incoming::Gone => {
                    with_host(|h| h.quit = true);
                }
                Incoming::Msg(m) => handle(m),
                Incoming::Bad(e) => send(HostToApp::Error { id: None, message: format!("bad message: {e}") }),
            }
        }
        if with_host(|h| h.quit).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(4));
    }

    // Close every browser, let CEF finish closing, then shut down.
    let browsers: Vec<Browser> = with_host(|h| h.browsers.drain().filter_map(|(_, e)| e.browser).collect()).unwrap_or_default();
    for b in &browsers {
        if let Some(host) = b.host() {
            host.close_browser(1);
        }
    }
    drop(browsers);
    // CEF must see every browser closed before `shutdown`, or its teardown watchdog kills us.
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while OPEN.get() > 0 && std::time::Instant::now() < end {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    // Write cookies to disk and wait (bounded) for CEF to confirm, so logins never depend on
    // how teardown goes.
    FLUSHED.set(false);
    let flushing = cookie_manager_get_global_manager(None).is_some_and(|cm| cm.flush_store(Some(&mut handlers::flush_done())) == 1);
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while flushing && !FLUSHED.get() && std::time::Instant::now() < end {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    // Let CEF finish the close and flush tasks it queued; `shutdown` hangs if they are pending.
    for _ in 0..40 {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    HOST.with_borrow_mut(|h| {
        if let Some(h) = h.as_mut() {
            h.dialogs.clear();
            h.choosers.clear();
        }
    });
    let _ = std::fs::remove_file(socket);
    // Chromium's own teardown watchdog can kill a slow `shutdown` (exit 2) when the machine is
    // loaded. Everything that matters is closed and flushed by now, so bound the teardown
    // ourselves: if CEF has not returned within SHUTDOWN_GRACE, exit 0 from a side thread
    // before Chromium's watchdog fires. Helpers exit when the browser process goes away.
    std::thread::spawn(|| {
        std::thread::sleep(SHUTDOWN_GRACE);
        debug(format_args!("CEF shutdown exceeded {SHUTDOWN_GRACE:?}; exiting"));
        unsafe { _exit(0) };
    });
    let t = std::time::Instant::now();
    cef::shutdown();
    debug(format_args!("CEF shut down in {:?} (cookies flushed: {})", t.elapsed(), FLUSHED.get()));
    HOST.with_borrow_mut(|h| *h = None);
    Ok(())
}

fn browser(id: &str) -> Option<Browser> {
    with_host(|h| h.browsers.get(id).and_then(|e| e.browser.clone())).flatten()
}

fn unknown(id: &str) {
    send(HostToApp::Error { id: Some(id.into()), message: format!("no browser {id}") });
}

fn handle(m: AppToHost) {
    use AppToHost as M;
    match m {
        M::Hello { .. } => send(HostToApp::Error { id: None, message: "hello already received".into() }),
        M::Shutdown => {}
        M::Create { id, url, size, scale, .. } => {
            let exists = with_host(|h| h.browsers.contains_key(&id)).unwrap_or(false);
            if exists {
                send(HostToApp::Error { id: Some(id.clone()), message: format!("browser {id} exists") });
                return;
            }
            with_host(|h| h.browsers.insert(id.clone(), Entry { browser: None, size, scale, pool: FramePool::new(), last: None, _devtools: None, queued: Vec::new(), loaded: false, input_ready: false, view: None, popup: None, popup_shown: false, popup_rect: None }));
            let window_info = WindowInfo { windowless_rendering_enabled: 1, shared_texture_enabled: 1, ..Default::default() };
            let settings = BrowserSettings { windowless_frame_rate: 60, ..Default::default() };
            let mut client = handlers::client(&id);
            let url = CefString::from(url.as_deref().unwrap_or("about:blank"));
            match browser_host_create_browser_sync(Some(&window_info), Some(&mut client), Some(&url), Some(&settings), None, None) {
                Some(b) => {
                    // Ask DevTools for this page's target id; the observer sends CdpTarget.
                    let reg = b.host().and_then(|host| {
                        let mut obs = handlers::devtools_observer(&id);
                        let reg = host.add_dev_tools_message_observer(Some(&mut obs));
                        host.execute_dev_tools_method(handlers::TARGET_INFO_MSG, Some(&CefString::from("Target.getTargetInfo")), None);
                        reg
                    });
                    with_host(|h| {
                        h.browsers.get_mut(&id).map(|e| {
                            e.browser = Some(b);
                            e._devtools = reg;
                        })
                    });
                }
                None => {
                    with_host(|h| h.browsers.remove(&id));
                    send(HostToApp::Error { id: Some(id), message: "CEF could not create the browser".into() });
                }
            }
        }
        M::FileChooserResult { id, req, paths } => {
            let Some(cb) = with_host(|h| h.choosers.remove(&req)).flatten() else {
                return send(HostToApp::Error { id: Some(id), message: format!("no file chooser {req}") });
            };
            if paths.is_empty() {
                cb.cancel();
            } else {
                let mut list = CefStringList::new();
                for p in &paths {
                    list.append(p);
                }
                cb.cont(Some(&mut list));
            }
        }
        M::DialogResult { id, req, accept, text } => {
            let Some(cb) = with_host(|h| h.dialogs.remove(&req)).flatten() else {
                return send(HostToApp::Error { id: Some(id), message: format!("no dialog {req}") });
            };
            cb.cont(accept as i32, text.as_deref().map(CefString::from).as_ref());
        }
        M::FrameAck { id, seq } => {
            let next = with_host(|h| h.browsers.get_mut(&id).map(|e| (e.pool.ack(seq).map(|f| (f.seq, f.surface.clone())), e.scale))).flatten();
            match next {
                Some((Some((seq, s)), scale)) => send_frame(&id, seq, s, scale),
                Some((None, _)) => {}
                None => unknown(&id),
            }
        }
        M::Close { id } => {
            let Some(entry) = with_host(|h| h.browsers.remove(&id)).flatten() else { return unknown(&id) };
            if let Some(host) = entry.browser.as_ref().and_then(|b| b.host()) {
                host.close_browser(1);
            }
        }
        M::Screenshot { id, req, path } => {
            let Some(last) = with_host(|h| h.browsers.get(&id).map(|e| e.last.clone())).flatten() else { return unknown(&id) };
            let res = match last {
                Some(s) => write_png(&s, Path::new(&path)),
                None => Err("no frame painted yet".into()),
            };
            send(HostToApp::ScreenshotDone { id, req, ok: res.is_ok(), error: res.err() });
        }
        other => browser_msg(other),
    }
}

fn browser_msg(m: AppToHost) {
    use AppToHost as M;
    let id = match &m {
        M::Navigate { id, .. } | M::Back { id } | M::Forward { id } | M::Reload { id, .. } | M::Stop { id } | M::Resize { id, .. }
        | M::Focus { id, .. } | M::SetZoom { id, .. } | M::Input { id, .. } | M::Edit { id, .. } | M::Find { id, .. } | M::StopFind { id, .. }
        | M::DevTools { id, .. } | M::Eval { id, .. } => id.clone(),
        _ => return,
    };
    let Some(b) = browser(&id) else { return unknown(&id) };
    let Some(host) = b.host() else { return unknown(&id) };
    match m {
        M::Navigate { url, .. } => {
            if let Some(f) = b.main_frame() {
                f.load_url(Some(&CefString::from(url.as_str())));
            }
        }
        M::Back { .. } => b.go_back(),
        M::Forward { .. } => b.go_forward(),
        M::Reload { ignore_cache, .. } => {
            if ignore_cache {
                b.reload_ignore_cache()
            } else {
                b.reload()
            }
        }
        M::Stop { .. } => b.stop_load(),
        M::Resize { size, scale, .. } => {
            let scale_changed = with_host(|h| {
                h.browsers.get_mut(&id).map(|e| {
                    let changed = e.scale != scale;
                    e.size = size;
                    e.scale = scale;
                    changed
                })
            })
            .flatten()
            .unwrap_or(false);
            if scale_changed {
                host.notify_screen_info_changed();
            }
            host.was_resized();
        }
        M::Focus { focused, .. } => host.set_focus(focused as i32),
        M::SetZoom { level, .. } => host.set_zoom_level(level),
        M::Input { event, .. } => {
            let queued = with_host(|h| {
                h.browsers.get_mut(&id).filter(|e| !e.input_ready).map(|e| e.queued.push(event.clone())).is_some()
            })
            .unwrap_or(false);
            if !queued {
                input(&host, event);
            }
        }
        M::Edit { command, .. } => {
            // OSR on macOS ignores cmd-c/x/v/a/z sent as keys; run the frame's own commands.
            let Some(f) = b.focused_frame().or_else(|| b.main_frame()) else { return };
            match command {
                EditCommand::Copy => f.copy(),
                EditCommand::Cut => f.cut(),
                EditCommand::Paste => f.paste(),
                EditCommand::SelectAll => f.select_all(),
                EditCommand::Undo => f.undo(),
                EditCommand::Redo => f.redo(),
            }
        }
        M::Find { text, forward, match_case, find_next, .. } => {
            host.find(Some(&CefString::from(text.as_str())), forward as i32, match_case as i32, find_next as i32)
        }
        M::StopFind { clear_selection, .. } => host.stop_finding(clear_selection as i32),
        M::DevTools { .. } => send(HostToApp::Error {
            id: Some(id),
            message: "unsupported: the DevTools window is not available in the headless browser host yet; attach a DevTools client to the CDP port (Ready.cdp_port) instead".into(),
        }),
        M::Eval { req, expr, .. } => {
            let call = handlers::eval_message(&id, req, &expr);
            if host.send_dev_tools_message(Some(&call)) == 0 {
                handlers::eval_unsent(&id);
                send(HostToApp::EvalResult { id, req, ok: false, value: "DevTools is not available for this page".into() });
            }
        }
        _ => {}
    }
}

fn flags(m: &Modifiers) -> u32 {
    use sys::cef_event_flags_t as F;
    let mut f = 0;
    if m.caps_lock {
        f |= F::EVENTFLAG_CAPS_LOCK_ON.0;
    }
    if m.shift {
        f |= F::EVENTFLAG_SHIFT_DOWN.0;
    }
    if m.control {
        f |= F::EVENTFLAG_CONTROL_DOWN.0;
    }
    if m.alt {
        f |= F::EVENTFLAG_ALT_DOWN.0;
    }
    if m.platform {
        f |= F::EVENTFLAG_COMMAND_DOWN.0;
    }
    f
}

fn input(host: &BrowserHost, ev: InputEvent) {
    use sys::cef_mouse_button_type_t as B;
    match ev {
        InputEvent::Mouse { kind, x, y, button, clicks, mods } => {
            // HERDR: the held button rides on the event's flags; without it a move is a hover, not a drag.
            let held = match (kind, button) {
                (MouseKind::Up | MouseKind::Leave, _) | (_, MouseButton::None) => 0,
                (_, MouseButton::Left) => sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0,
                (_, MouseButton::Middle) => sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0,
                (_, MouseButton::Right) => sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON.0,
            };
            let e = MouseEvent { x: x.round() as i32, y: y.round() as i32, modifiers: flags(&mods) | held };
            let btn = match button {
                MouseButton::Left | MouseButton::None => B::MBT_LEFT,
                MouseButton::Middle => B::MBT_MIDDLE,
                MouseButton::Right => B::MBT_RIGHT,
            };
            match kind {
                MouseKind::Move => host.send_mouse_move_event(Some(&e), 0),
                MouseKind::Leave => host.send_mouse_move_event(Some(&e), 1),
                MouseKind::Down | MouseKind::Up => {
                    host.send_mouse_click_event(Some(&e), MouseButtonType::from(btn), (kind == MouseKind::Up) as i32, clicks.max(1) as i32)
                }
            }
        }
        InputEvent::Wheel { x, y, dx, dy, precise, mods } => {
            let k = if precise { 1.0 } else { 40.0 };
            let e = MouseEvent { x: x.round() as i32, y: y.round() as i32, modifiers: flags(&mods) };
            host.send_mouse_wheel_event(Some(&e), (dx * k).round() as i32, (dy * k).round() as i32);
        }
        InputEvent::Key { kind, key, code, native, repeat, mods } => {
            use sys::cef_key_event_type_t as K;
            let ch = if key.chars().count() == 1 { key.encode_utf16().next().unwrap_or(0) } else { keys::mac_character(&code) };
            let e = KeyEvent {
                type_: KeyEventType::from(if kind == KeyKind::Down { K::KEYEVENT_RAWKEYDOWN } else { K::KEYEVENT_KEYUP }),
                // HERDR: auto-repeat reaches the page as KeyboardEvent.repeat.
                modifiers: flags(&mods) | if repeat { sys::cef_event_flags_t::EVENTFLAG_IS_REPEAT.0 } else { 0 },
                windows_key_code: keys::windows_key_code(&code, &key),
                native_key_code: native as i32,
                character: ch,
                unmodified_character: ch,
                ..Default::default()
            };
            host.send_key_event(Some(&e));
        }
        InputEvent::Text { text } => {
            for unit in text.encode_utf16() {
                let e = KeyEvent {
                    type_: KeyEventType::from(sys::cef_key_event_type_t::KEYEVENT_CHAR),
                    windows_key_code: unit as i32,
                    character: unit,
                    unmodified_character: unit,
                    ..Default::default()
                };
                host.send_key_event(Some(&e));
            }
        }
        InputEvent::Ime(ImeEvent::Set { text, sel_start, sel_end, replace_start, replace_end }) => {
            let len = text.encode_utf16().count() as u32;
            let underline = CompositionUnderline {
                range: Range { from: 0, to: len },
                color: 0xFF00_0000,
                background_color: 0,
                thick: 0,
                ..Default::default()
            };
            let replace = match (replace_start, replace_end) {
                (Some(from), Some(to)) => Range { from, to },
                _ => Range { from: u32::MAX, to: u32::MAX },
            };
            host.ime_set_composition(
                Some(&CefString::from(text.as_str())),
                Some(&[underline]),
                Some(&replace),
                Some(&Range { from: sel_start, to: sel_end }),
            );
        }
        InputEvent::Ime(ImeEvent::Commit { text }) => {
            host.ime_commit_text(Some(&CefString::from(text.as_str())), Some(&Range { from: u32::MAX, to: u32::MAX }), 0)
        }
        InputEvent::Ime(ImeEvent::Cancel) => host.ime_cancel_composition(),
    }
}

fn write_png(s: &Surface, path: &Path) -> Result<(), String> {
    let px = s.size();
    let mut rgba = s.read_bgra().map_err(|e| e.to_string())?;
    for p in rgba.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(io::BufWriter::new(file), px.w, px.h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut w| w.write_image_data(&rgba)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::popup_place;

    #[test]
    fn popups_land_at_their_bounds_clipped_to_the_frame() {
        // A 100x40 DIP list at (10, 20) on a 2x frame of 400x300 px.
        assert_eq!(popup_place((10, 20, 100, 40), 2.0, (200, 80), (400, 300)), Some(((0, 0), (20, 40), 200, 80)));
        // Hanging off the right and bottom edges: clipped.
        assert_eq!(popup_place((150, 120, 100, 40), 2.0, (200, 80), (400, 300)), Some(((0, 0), (300, 240), 100, 60)));
        // Starting above / left of the frame: the source is offset.
        assert_eq!(popup_place((-5, -10, 100, 40), 1.0, (100, 40), (400, 300)), Some(((5, 10), (0, 0), 95, 30)));
        // Entirely outside.
        assert_eq!(popup_place((500, 0, 10, 10), 1.0, (10, 10), (400, 300)), None);
    }
}
