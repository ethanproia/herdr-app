//! Page dialogs for a browser block (I13, I14): JavaScript alert/confirm/prompt/beforeunload as
//! a modal over the block, `<input type=file>` through the native open panel, and downloads
//! (a list for `browser.downloads` and a notice when one finishes). OWNER: fork-host.
//!
//! `PageDialogs` is one entity per `BrowserView`: the view hands it every host message
//! (`on_host`), renders it on top of the page (it draws nothing when no dialog is open) and lets
//! it take the keyboard while a dialog is up. `Downloads` is one list for the whole app.

use super::HostLink;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_base::input::{Input, InputEvent, InputState};
use herdr_browser_protocol::{AppToHost, BrowserId, DialogKind, DownloadState, HostToApp};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A notice the window shows as a toast (`{"kind":"toast","notification": notice}`).
pub type Notice = Value;

#[derive(Debug, Clone, PartialEq)]
pub struct JsDialog {
    pub req: u64,
    pub kind: DialogKind,
    pub message: String,
    pub default_text: Option<String>,
}

pub enum DialogsEvent {
    /// A download finished or failed: show this toast.
    Notice(Notice),
}

pub struct PageDialogs {
    id: BrowserId,
    link: Arc<dyn HostLink>,
    downloads: Downloads,
    /// The dialog on screen; later ones wait their turn (pages rarely stack them).
    open: Option<JsDialog>,
    queue: std::collections::VecDeque<JsDialog>,
    prompt: Option<Entity<InputState>>,
    focus: FocusHandle,
    _prompt_sub: Option<Subscription>,
}

impl EventEmitter<DialogsEvent> for PageDialogs {}

impl PageDialogs {
    pub fn new(id: &str, link: Arc<dyn HostLink>, downloads: Downloads, cx: &mut Context<Self>) -> Self {
        Self { id: id.into(), link, downloads, open: None, queue: Default::default(), prompt: None, focus: cx.focus_handle(), _prompt_sub: None }
    }

    /// The dialog on screen, if any (tests and the view's key routing read it).
    pub fn current(&self) -> Option<&JsDialog> {
        self.open.as_ref()
    }

    /// True while a dialog wants the keyboard instead of the page.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Every host message for this browser. Returns true if it was a dialog-ish message.
    pub fn on_host(&mut self, msg: &HostToApp, cx: &mut Context<Self>) -> bool {
        match msg {
            HostToApp::Dialog { req, kind, message, default_text, .. } => {
                let d = JsDialog { req: *req, kind: *kind, message: message.clone(), default_text: default_text.clone() };
                if self.open.is_some() {
                    self.queue.push_back(d);
                } else {
                    self.show(d);
                }
                cx.notify();
                true
            }
            HostToApp::FileChooser { req, multiple, accept, .. } => {
                self.choose_files(*req, *multiple, accept.clone(), cx);
                true
            }
            HostToApp::Download { .. } => {
                if let Some(notice) = self.downloads.on_host(msg) {
                    cx.emit(DialogsEvent::Notice(notice));
                }
                true
            }
            // The page went away: its dialogs went with it (the host already cancelled them).
            HostToApp::Crashed { .. } => {
                self.open = None;
                self.queue.clear();
                self.prompt = None;
                cx.notify();
                false
            }
            _ => false,
        }
    }

    fn show(&mut self, d: JsDialog) {
        self.prompt = None; // made in render (needs the window), seeded from default_text
        self.open = Some(d);
    }

    /// Answer the open dialog: OK (`accept`) or Cancel; a prompt sends its text.
    pub fn answer(&mut self, accept: bool, cx: &mut Context<Self>) {
        let Some(d) = self.open.take() else { return };
        let text = (d.kind == DialogKind::Prompt && accept).then(|| self.prompt.as_ref().map(|p| p.read(cx).value().to_string()).unwrap_or_default());
        self.link.send(AppToHost::DialogResult { id: self.id.clone(), req: d.req, accept, text });
        self.prompt = None;
        self._prompt_sub = None;
        if let Some(next) = self.queue.pop_front() {
            self.show(next);
        }
        cx.notify();
    }

    /// `<input type=file>`: the native open panel, then the picks that match `accept` back to the
    /// page. Cancel (or nothing matching) answers an empty list, which the host turns into cancel.
    fn choose_files(&mut self, req: u64, multiple: bool, accept: Vec<String>, cx: &mut Context<Self>) {
        let (id, link) = (self.id.clone(), self.link.clone());
        // Headless tests answer without a panel: HERDR_BROWSER_FILE_CHOOSER="a.txt:b.png" (or "" to cancel).
        if let Some(fake) = std::env::var_os("HERDR_BROWSER_FILE_CHOOSER") {
            let picked: Vec<PathBuf> = std::env::split_paths(&fake).filter(|p| !p.as_os_str().is_empty()).collect();
            link.send(AppToHost::FileChooserResult { id, req, paths: filter_picks(&picked, &accept, multiple) });
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple, prompt: Some("Choose".into()) });
        cx.spawn(async move |_, _| {
            let picked = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                _ => Vec::new(),
            };
            link.send(AppToHost::FileChooserResult { id, req, paths: filter_picks(&picked, &accept, multiple) });
        })
        .detach();
    }
}

/// The picks that satisfy the page's `accept` list (`.ext`, `type/*`, `type/sub`), at most one
/// unless `multiple`. The open panel cannot grey out other files, so this is where accept holds.
pub fn filter_picks(picked: &[PathBuf], accept: &[String], multiple: bool) -> Vec<String> {
    let ok: Vec<String> = picked.iter().filter(|p| accepts(accept, p)).map(|p| p.to_string_lossy().into_owned()).collect();
    if multiple { ok } else { ok.into_iter().take(1).collect() }
}

/// Does `path` satisfy an `accept` attribute (already split into entries)? Empty accepts all.
pub fn accepts(accept: &[String], path: &std::path::Path) -> bool {
    let entries: Vec<String> = accept.iter().flat_map(|a| a.split(',')).map(|a| a.trim().to_ascii_lowercase()).filter(|a| !a.is_empty()).collect();
    if entries.is_empty() {
        return true;
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let mime = mime_of(&ext);
    entries.iter().any(|a| {
        if let Some(e) = a.strip_prefix('.') {
            e == ext
        } else if let Some(major) = a.strip_suffix("/*") {
            mime.is_some_and(|m| m.split('/').next() == Some(major))
        } else {
            mime == Some(a.as_str())
        }
    })
}

/// The MIME type of common extensions (enough for `accept` checks).
fn mime_of(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "md" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "xml" => "application/xml",
        "js" | "mjs" => "text/javascript",
        _ => return None,
    })
}

impl Render for PageDialogs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(d) = self.open.clone() else { return div().id("browser-dialogs") };
        if d.kind == DialogKind::Prompt && self.prompt.is_none() {
            let text = d.default_text.clone().unwrap_or_default();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
            self._prompt_sub = Some(cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.answer(true, cx);
                }
            }));
            input.update(cx, |i, cx| i.focus(window, cx));
            self.prompt = Some(input);
        } else if d.kind != DialogKind::Prompt && !self.focus.is_focused(window) {
            window.focus(&self.focus, cx);
        }
        let (title, ok, cancel) = match d.kind {
            DialogKind::Alert => ("This page says", "OK", None),
            DialogKind::Confirm => ("This page asks", "OK", Some("Cancel")),
            DialogKind::Prompt => ("This page asks", "OK", Some("Cancel")),
            DialogKind::BeforeUnload => ("Leave this page?", "Leave", Some("Stay")),
        };
        let dark = matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        let button = |label: &'static str, primary: bool| {
            div()
                .id(label)
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(primary, |d| d.bg(hsla(0.6, 0.7, 0.5, 1.)).text_color(white()))
                .when(!primary, |d| d.bg(hsla(0., 0., 0.5, 0.2)))
                .child(label)
        };
        // A scrim over the page, the card centred: the page gets no input while it is up.
        div()
            .id("browser-dialogs")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.35))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "enter" => this.answer(true, cx),
                "escape" => {
                    let accept = this.open.as_ref().is_some_and(|d| d.kind == DialogKind::Alert);
                    this.answer(accept, cx)
                }
                _ => {}
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .w(px(360.))
                    .max_w(relative(0.9))
                    .rounded_lg()
                    .shadow_lg()
                    .bg(if dark { hsla(0., 0., 0.14, 1.) } else { hsla(0., 0., 0.99, 1.) })
                    .child(div().text_xs().opacity(0.6).child(title))
                    .child(div().text_sm().child(d.message.clone()))
                    .when_some(self.prompt.clone(), |el, p| el.child(Input::new(&p)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .when_some(cancel, |el, c| el.child(button(c, false).on_click(cx.listener(|this, _, _, cx| this.answer(false, cx)))))
                            .child(button(ok, true).on_click(cx.listener(|this, _, _, cx| this.answer(true, cx)))),
                    ),
            )
    }
}

/// One download as the app knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub browser: BrowserId,
    pub req: u64,
    pub url: String,
    pub name: String,
    pub path: Option<String>,
    pub state: DownloadState,
    pub received: u64,
    pub total: Option<u64>,
}

/// Every download this run, newest last. Cheap to clone (shared).
#[derive(Clone, Default)]
pub struct Downloads(Arc<Mutex<Vec<Download>>>);

impl Downloads {
    /// Record a `Download` message. Returns the toast to show when it finished or failed.
    pub fn on_host(&self, msg: &HostToApp) -> Option<Notice> {
        let HostToApp::Download { id, req, url, suggested_name, state, path, received, total } = msg else { return None };
        let mut list = self.0.lock().unwrap();
        let was = list.iter().position(|d| d.browser == *id && d.req == *req);
        let d = match was {
            Some(i) => &mut list[i],
            None => {
                list.push(Download { browser: id.clone(), req: *req, url: url.clone(), name: suggested_name.clone(), path: None, state: *state, received: 0, total: None });
                list.last_mut().unwrap()
            }
        };
        let finished_before = matches!(d.state, DownloadState::Done | DownloadState::Failed) && was.is_some();
        if !url.is_empty() {
            d.url = url.clone();
        }
        if !suggested_name.is_empty() {
            d.name = suggested_name.clone();
        }
        if path.is_some() {
            d.path = path.clone();
        }
        d.state = *state;
        d.received = *received;
        d.total = total.or(d.total);
        if finished_before {
            return None;
        }
        let file = d.path.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| d.name.clone());
        match d.state {
            DownloadState::Done => Some(json!({
                "title": format!("Downloaded {file}"),
                "body": d.path.clone().unwrap_or_default(),
                "level": "info",
            })),
            DownloadState::Failed => Some(json!({ "title": format!("Download failed: {file}"), "body": d.url.clone(), "level": "error" })),
            _ => None,
        }
    }

    pub fn list(&self) -> Vec<Download> {
        self.0.lock().unwrap().clone()
    }

    /// `browser.downloads`: `[{ id, url, name, path, state, received, total }]`, oldest first.
    pub fn list_json(&self, browser: Option<&str>) -> Value {
        let state = |s: DownloadState| match s {
            DownloadState::Started => "started",
            DownloadState::Progress => "progress",
            DownloadState::Done => "done",
            DownloadState::Failed => "failed",
        };
        Value::Array(
            self.list()
                .into_iter()
                .filter(|d| browser.is_none_or(|b| d.browser == b))
                .map(|d| json!({ "id": d.browser, "url": d.url, "name": d.name, "path": d.path, "state": state(d.state), "received": d.received, "total": d.total }))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings in gpui's `test` attribute in place of the standard one.
    use super::{Downloads, accepts, filter_picks};
    use herdr_browser_protocol::{DownloadState, HostToApp};
    use std::path::{Path, PathBuf};

    fn dl(state: DownloadState, received: u64, path: Option<&str>) -> HostToApp {
        HostToApp::Download {
            id: "b1".into(),
            req: 7,
            url: "http://127.0.0.1/f.zip".into(),
            suggested_name: "f.zip".into(),
            state,
            path: path.map(Into::into),
            received,
            total: Some(10),
        }
    }

    #[test]
    fn downloads_track_progress_and_notify_once() {
        let d = Downloads::default();
        assert_eq!(d.on_host(&dl(DownloadState::Started, 0, Some("/u/Downloads/f (1).zip"))), None);
        assert_eq!(d.on_host(&dl(DownloadState::Progress, 4, None)), None);
        let n = d.on_host(&dl(DownloadState::Done, 10, None)).expect("done notifies");
        assert_eq!(n["title"], "Downloaded f (1).zip");
        assert_eq!(n["body"], "/u/Downloads/f (1).zip");
        assert_eq!(d.on_host(&dl(DownloadState::Done, 10, None)), None, "one notice per download");
        let list = d.list_json(None);
        let one = &list[0];
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!((one["id"].as_str(), one["name"].as_str(), one["state"].as_str()), (Some("b1"), Some("f.zip"), Some("done")));
        assert_eq!((one["path"].as_str(), one["received"].as_u64(), one["total"].as_u64()), (Some("/u/Downloads/f (1).zip"), Some(10), Some(10)));
        assert!(d.list_json(Some("other")).as_array().unwrap().is_empty());
        let fail = HostToApp::Download { id: "b1".into(), req: 8, url: "u".into(), suggested_name: "g.zip".into(), state: DownloadState::Failed, path: None, received: 0, total: None };
        assert_eq!(d.on_host(&fail).unwrap()["level"], "error");
    }

    #[test]
    fn accept_filters() {
        let a = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(accepts(&[], Path::new("/x/any.bin")));
        assert!(accepts(&a(&[".png", ".jpg"]), Path::new("/x/A.PNG")));
        assert!(!accepts(&a(&[".png"]), Path::new("/x/a.gif")));
        assert!(accepts(&a(&["image/*"]), Path::new("/x/a.webp")));
        assert!(!accepts(&a(&["image/*"]), Path::new("/x/a.pdf")));
        assert!(accepts(&a(&["application/pdf"]), Path::new("/x/a.pdf")));
        assert!(accepts(&a(&["image/png, .txt"]), Path::new("/x/notes.txt")));
        let picks = [PathBuf::from("/x/a.png"), PathBuf::from("/x/b.txt"), PathBuf::from("/x/c.png")];
        assert_eq!(filter_picks(&picks, &a(&["image/*"]), true), vec!["/x/a.png", "/x/c.png"]);
        assert_eq!(filter_picks(&picks, &a(&["image/*"]), false), vec!["/x/a.png"]);
        assert_eq!(filter_picks(&picks, &a(&[".pdf"]), true), Vec::<String>::new());
    }
}
