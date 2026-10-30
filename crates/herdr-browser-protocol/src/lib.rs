//! Wire contract between `herdr-app` (the GPUI app) and `herdr-browser-host` (CEF off-screen host).
//!
//! One Unix socket per app: `$HERDR_APP_HOME/browser/host.sock`. Newline-delimited JSON, one
//! message per line, each a `Frame`-less control message; pixels never travel on the socket,
//! only a `SurfaceRef` to a shared IOSurface. See docs/browser-block-contract.md.
//!
//! OWNER: the lead. This file is the seam between five workers: change it only through the lead.
//! Coordinates are DIPs (logical points) from the top-left of the browser view; `scale` is the
//! backing scale factor (2.0 on Retina), so a frame is `size * scale` pixels.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// The layout block id that owns a browser; also the CDP target key.
pub type BrowserId = String;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelSize {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers {
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub control: bool,
    #[serde(default)]
    pub alt: bool,
    /// Command on macOS, Windows key elsewhere.
    #[serde(default)]
    pub platform: bool,
    #[serde(default)]
    pub caps_lock: bool,
}

/// A shared frame buffer the app can paint without copying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceRef {
    /// A global IOSurface looked up with `IOSurfaceLookup(id)`; default transport.
    GlobalId { id: u32 },
    /// A mach port registered with the bootstrap server under `name` (fallback transport).
    MachPort { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Move,
    Down,
    Up,
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Down,
    Up,
}

/// IME composition, mirroring CEF's `ImeSetComposition` / `ImeCommitText` / `ImeCancelComposition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ImeEvent {
    /// Marked (uncommitted) text. Selection and replacement ranges are UTF-16 offsets into `text`.
    Set { text: String, sel_start: u32, sel_end: u32, replace_start: Option<u32>, replace_end: Option<u32> },
    Commit { text: String },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    Mouse { kind: MouseKind, x: f32, y: f32, button: MouseButton, clicks: u8, mods: Modifiers },
    /// `precise` is true for trackpads (pixel deltas), false for wheels (line deltas).
    Wheel { x: f32, y: f32, dx: f32, dy: f32, precise: bool, mods: Modifiers },
    /// A physical key. `key` is the DOM `key` value, `code` the DOM `code`, `native` the platform key code.
    Key { kind: KeyKind, key: String, code: String, native: u32, repeat: bool, mods: Modifiers },
    /// Text the page should receive as typed characters (after IME, dead keys and layout).
    Text { text: String },
    Ime(ImeEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Started,
    Progress,
    Done,
    Failed,
}

/// An editing command the host runs on the focused frame (CEF off-screen on macOS does not act on
/// the cmd-c/x/v/a/z chords sent as keys, so the app sends these instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditCommand {
    Copy,
    Cut,
    Paste,
    SelectAll,
    Undo,
    Redo,
}

/// One entry of a context menu the host built from CEF's model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MenuItem {
    /// The host's command id to send back; 0 for a separator.
    pub command: i32,
    pub label: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub submenu: Vec<MenuItem>,
}

/// A rectangle in CSS pixels (an element box, a selection, a hit-test result).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CssRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A point in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CssPoint {
    pub x: f32,
    pub y: f32,
}

/// What an overlay request asks the page. Coordinates: viewport CSS px.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayQuery {
    HitTest { x: f32, y: f32 },
    Selector { selector: String },
    Selection,
}

/// App to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppToHost {
    /// First message. `profile_dir` is the shared CEF root cache (logins live here).
    /// `cdp_pipe`: start Chromium with remote-debugging-pipe (CDP on fds 3 and 4, no TCP port).
    Hello { version: u32, profile_dir: String, cdp_port: Option<u16>, #[serde(default)] cdp_pipe: bool },
    /// `profile`: the block's profile name (D4); None or "default" is the shared default profile.
    /// Each name gets its own cookies and storage under `<profile_dir>/<name>`.
    Create { id: BrowserId, url: Option<String>, size: Size, scale: f32, #[serde(default)] profile: Option<String> },
    Close { id: BrowserId },
    Navigate { id: BrowserId, url: String },
    Back { id: BrowserId },
    Forward { id: BrowserId },
    Reload { id: BrowserId, ignore_cache: bool },
    Stop { id: BrowserId },
    Resize { id: BrowserId, size: Size, scale: f32 },
    Focus { id: BrowserId, focused: bool },
    /// Zoom level as Chrome counts it: 0.0 is 100%, each step is about 20%.
    SetZoom { id: BrowserId, level: f64 },
    /// The app's appearance changed (or Hello was just sent): every page's prefers-color-scheme
    /// follows it, live. Applies to browsers created later too.
    SetColorScheme { dark: bool },
    /// Rects for an overlay; answered by `OverlayRects` with the same `req` and `epoch`.
    Overlay { id: BrowserId, req: u64, epoch: u64, query: OverlayQuery },
    Input { id: BrowserId, event: InputEvent },
    /// Run an editing command (copy, paste, ...) in the focused frame.
    Edit { id: BrowserId, command: EditCommand },
    /// Frame `seq` (and every older one) was presented in a visible window: the host may send more
    /// (at most two unacked). Paces delivery only; a surface is reused only once its IOSurface use
    /// count is released.
    FrameAck { id: BrowserId, seq: u64 },
    /// The block's window is occluded or minimized, or its tab is not shown (`false`), or shown again
    /// (`true`). Hidden: the host stops painting and drops pending frames; shown: it sends a fresh
    /// full frame. `Screenshot` still works while hidden.
    SetVisible { id: BrowserId, visible: bool },
    /// The chosen command for menu `req`, or None if dismissed. The host runs it via CEF's
    /// RunContextMenuCallback (Back, Forward, Reload, Copy, Cut, Paste, Select All, Inspect ...).
    MenuResult { id: BrowserId, req: u64, command: Option<i32> },
    Find { id: BrowserId, text: String, forward: bool, match_case: bool, find_next: bool },
    StopFind { id: BrowserId, clear_selection: bool },
    DevTools { id: BrowserId, open: bool },
    /// Run JS in the page; answered by `EvalResult` with the same `req`.
    Eval { id: BrowserId, req: u64, expr: String },
    /// Save a PNG of the current page (full resolution) to `path`; answered by `ScreenshotDone`.
    Screenshot { id: BrowserId, req: u64, path: String },
    FileChooserResult { id: BrowserId, req: u64, paths: Vec<String> },
    DialogResult { id: BrowserId, req: u64, accept: bool, text: Option<String> },
    Shutdown,
}

/// Host to app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToApp {
    /// `cdp_pipe`: true once the host started with the pipe switch.
    Ready { version: u32, cef_version: String, cdp_port: Option<u16>, #[serde(default)] cdp_pipe: bool },
    /// A new frame. At most one frame per browser is unacknowledged at a time: the host holds the
    /// newest pending frame until `FrameAck` and drops older ones.
    Frame { id: BrowserId, seq: u64, surface: SurfaceRef, size: PixelSize, scale: f32 },
    Title { id: BrowserId, title: String },
    Url { id: BrowserId, url: String },
    Loading { id: BrowserId, loading: bool, can_back: bool, can_forward: bool },
    /// CSS cursor name: "default", "pointer", "text", "grab", ...
    Cursor { id: BrowserId, name: String },
    Favicon { id: BrowserId, url: String },
    /// A link or script asked for a new window or tab; the app decides (usually a new browser block).
    NewWindow { id: BrowserId, url: String, disposition: String },
    Download { id: BrowserId, req: u64, url: String, suggested_name: String, state: DownloadState, path: Option<String>, received: u64, total: Option<u64> },
    FileChooser { id: BrowserId, req: u64, multiple: bool, accept: Vec<String> },
    Dialog { id: BrowserId, req: u64, kind: DialogKind, message: String, default_text: Option<String> },
    /// Where the text caret is, in DIPs, so the app can place the IME candidate window.
    ImeCaret { id: BrowserId, x: f32, y: f32, w: f32, h: f32 },
    FindResult { id: BrowserId, count: u32, active: u32, final_update: bool },
    Console { id: BrowserId, level: String, message: String, source: String, line: u32 },
    /// The CDP target id for this browser, so agents can attach to the right page via the CDP port.
    CdpTarget { id: BrowserId, target_id: String },
    /// Rects in the answering frame's CSS px; `frames`: enclosing iframe content-box offsets, outermost first. `epoch` echoed.
    OverlayRects { id: BrowserId, req: u64, epoch: u64, rects: Vec<CssRect>, frames: Vec<CssPoint> },
    /// The page asked for a context menu at (x, y) page DIPs; `kind` flags what is under the pointer:
    /// "link", "image", "selection", "editable", "media", "page" (several may apply).
    /// `link_url`, `src_url`: for Open/Copy link and Save image. The app shows `items` and answers with `MenuResult`.
    /// Commands >= 26500 (CEF's MENU_ID_USER_FIRST) are app-handled extras; the app does them itself,
    /// then answers `MenuResult` with None to close the CEF callback.
    ContextMenu { id: BrowserId, req: u64, x: f32, y: f32, kind: Vec<String>, link_url: Option<String>, src_url: Option<String>, selection: Option<String>, items: Vec<MenuItem> },
    /// Scroll or zoom changed (at most one per frame); the app bumps its overlay epoch.
    Viewport { id: BrowserId, scroll_x: f32, scroll_y: f32, zoom_level: f64 },
    EvalResult { id: BrowserId, req: u64, ok: bool, value: serde_json::Value },
    ScreenshotDone { id: BrowserId, req: u64, ok: bool, error: Option<String> },
    Crashed { id: Option<BrowserId>, reason: String },
    Error { id: Option<BrowserId>, message: String },
}

/// Encode one message as a line (no trailing newline).
pub fn encode<T: Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("protocol messages always serialize")
}

/// Decode one line.
pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_app(m: AppToHost) {
        let line = encode(&m);
        assert!(!line.contains('\n'), "one message per line: {line}");
        assert_eq!(decode::<AppToHost>(&line).unwrap(), m);
    }

    fn roundtrip_host(m: HostToApp) {
        let line = encode(&m);
        assert!(!line.contains('\n'), "one message per line: {line}");
        assert_eq!(decode::<HostToApp>(&line).unwrap(), m);
    }

    #[test]
    fn app_messages_roundtrip() {
        let mods = Modifiers { platform: true, ..Default::default() };
        roundtrip_app(AppToHost::Hello { version: PROTOCOL_VERSION, profile_dir: "/tmp/p".into(), cdp_port: Some(9222), cdp_pipe: true });
        roundtrip_app(AppToHost::Create { id: "b1".into(), url: Some("https://example.com".into()), size: Size { w: 800.0, h: 600.0 }, scale: 2.0, profile: None });
        roundtrip_app(AppToHost::Resize { id: "b1".into(), size: Size { w: 400.0, h: 300.0 }, scale: 2.0 });
        roundtrip_app(AppToHost::FrameAck { id: "b1".into(), seq: 7 });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Mouse { kind: MouseKind::Down, x: 1.5, y: 2.5, button: MouseButton::Left, clicks: 2, mods } });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Wheel { x: 0.0, y: 0.0, dx: 0.0, dy: -12.0, precise: true, mods } });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Key { kind: KeyKind::Down, key: "a".into(), code: "KeyA".into(), native: 0, repeat: false, mods } });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Text { text: "é".into() } });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(ImeEvent::Set { text: "にほん".into(), sel_start: 3, sel_end: 3, replace_start: None, replace_end: None }) });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(ImeEvent::Commit { text: "日本".into() }) });
        roundtrip_app(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(ImeEvent::Cancel) });
        roundtrip_app(AppToHost::Eval { id: "b1".into(), req: 1, expr: "1+1".into() });
        roundtrip_app(AppToHost::Edit { id: "b1".into(), command: EditCommand::SelectAll });
        roundtrip_app(AppToHost::SetColorScheme { dark: true });
        roundtrip_app(AppToHost::Create { id: "b2".into(), url: None, size: Size { w: 10.0, h: 10.0 }, scale: 1.0, profile: Some("work".into()) });
        roundtrip_app(AppToHost::Overlay { id: "b1".into(), req: 3, epoch: 9, query: OverlayQuery::HitTest { x: 1.5, y: 2.5 } });
        roundtrip_app(AppToHost::Overlay { id: "b1".into(), req: 4, epoch: 9, query: OverlayQuery::Selector { selector: "#a > b".into() } });
        roundtrip_app(AppToHost::Overlay { id: "b1".into(), req: 5, epoch: 9, query: OverlayQuery::Selection });
        roundtrip_app(AppToHost::SetVisible { id: "b1".into(), visible: false });
        roundtrip_app(AppToHost::MenuResult { id: "b1".into(), req: 7, command: Some(26500) });
        roundtrip_app(AppToHost::MenuResult { id: "b1".into(), req: 8, command: None });
        roundtrip_app(AppToHost::Shutdown);
    }

    #[test]
    fn host_messages_roundtrip() {
        roundtrip_host(HostToApp::Ready { version: PROTOCOL_VERSION, cef_version: "0".into(), cdp_port: None, cdp_pipe: false });
        roundtrip_host(HostToApp::Frame { id: "b1".into(), seq: 1, surface: SurfaceRef::GlobalId { id: 42 }, size: PixelSize { w: 1600, h: 1200 }, scale: 2.0 });
        roundtrip_host(HostToApp::Frame { id: "b1".into(), seq: 2, surface: SurfaceRef::MachPort { name: "com.herdr.s".into() }, size: PixelSize { w: 2, h: 2 }, scale: 1.0 });
        roundtrip_host(HostToApp::Loading { id: "b1".into(), loading: true, can_back: false, can_forward: false });
        roundtrip_host(HostToApp::ImeCaret { id: "b1".into(), x: 1.0, y: 2.0, w: 1.0, h: 16.0 });
        roundtrip_host(HostToApp::EvalResult { id: "b1".into(), req: 1, ok: true, value: serde_json::json!({"n": 2}) });
        roundtrip_host(HostToApp::CdpTarget { id: "b1".into(), target_id: "T1".into() });
        roundtrip_host(HostToApp::ContextMenu { id: "b1".into(), req: 7, x: 4.0, y: 5.0, kind: vec!["link".into()], link_url: Some("https://a.test/".into()), src_url: None, selection: None, items: vec![MenuItem { command: 100, label: "Back".into(), enabled: true, checked: false, submenu: vec![] }, MenuItem { command: 0, label: String::new(), enabled: false, checked: false, submenu: vec![] }] });
        roundtrip_host(HostToApp::OverlayRects { id: "b1".into(), req: 3, epoch: 9, rects: vec![CssRect { x: 1.0, y: 2.0, w: 30.0, h: 40.0 }], frames: vec![CssPoint { x: 5.0, y: 6.0 }] });
        roundtrip_host(HostToApp::Viewport { id: "b1".into(), scroll_x: 0.0, scroll_y: 120.5, zoom_level: 1.0 });
        roundtrip_host(HostToApp::Crashed { id: None, reason: "gpu".into() });
    }

    #[test]
    fn wire_shape_is_stable() {
        let line = encode(&AppToHost::Navigate { id: "b1".into(), url: "https://x.test".into() });
        assert_eq!(line, r#"{"type":"navigate","id":"b1","url":"https://x.test"}"#);
        let line = encode(&AppToHost::Edit { id: "b1".into(), command: EditCommand::SelectAll });
        assert_eq!(line, r#"{"type":"edit","id":"b1","command":"select_all"}"#);
        let line = encode(&AppToHost::SetColorScheme { dark: true });
        assert_eq!(line, r#"{"type":"set_color_scheme","dark":true}"#);
        let line = encode(&HostToApp::Cursor { id: "b1".into(), name: "pointer".into() });
        assert_eq!(line, r#"{"type":"cursor","id":"b1","name":"pointer"}"#);
    }
}
