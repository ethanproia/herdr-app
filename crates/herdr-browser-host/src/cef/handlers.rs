//! CEF callbacks, each turned into a protocol message for the app.

use super::{downloads_dir, keep_chooser, keep_dialog, next_req, on_frame, on_popup_frame, on_popup_show, on_popup_size, send, view_of};
use cef::*;
use herdr_browser_protocol::{DialogKind, DownloadState, HostToApp};

thread_local! {
    /// Download paths handed out this run (CEF creates the file later).
    static RESERVED: std::cell::RefCell<std::collections::HashSet<std::path::PathBuf>> = Default::default();
}

/// DevTools message ids for evals start here (below are fixed ids such as `TARGET_INFO_MSG`).
const FIRST_EVAL_MSG: i32 = 1000;

thread_local! {
    static NEXT_MSG: std::cell::Cell<i32> = const { std::cell::Cell::new(FIRST_EVAL_MSG) };
    /// Runtime.evaluate calls in flight: DevTools message id -> (browser id, app request id, what for).
    static EVALS: std::cell::RefCell<std::collections::HashMap<i32, (String, u64, Op)>> = Default::default();
}

/// A `Runtime.evaluate` DevTools message for `expr` in browser `id`, answered by
/// `on_dev_tools_method_result`. The answer travels browser process to host only: nothing the page
/// can print or call reaches it, so a page cannot forge an `EvalResult` (SEC3).
pub fn eval_message(id: &str, req: u64, expr: &str) -> Vec<u8> {
    evaluate(id, req, Op::Eval, expr)
}

/// What a Runtime.evaluate answer turns into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Eval,
    Overlay { epoch: u64 },
}

/// Overlay geometry, measured by a privileged DevTools evaluate (the page cannot answer for it).
/// Rects are viewport CSS px of the top document; `frames` stays empty (no iframe descent yet).
pub fn overlay_message(id: &str, req: u64, epoch: u64, query: &herdr_browser_protocol::OverlayQuery) -> Vec<u8> {
    use herdr_browser_protocol::OverlayQuery as Q;
    const BOX: &str = "r=>({x:r.x,y:r.y,w:r.width,h:r.height})";
    let expr = match query {
        Q::HitTest { x, y } => format!("(()=>{{const e=document.elementFromPoint({x},{y});return{{rects:e?[({BOX})(e.getBoundingClientRect())]:[]}}}})()"),
        Q::Selector { selector } => {
            let sel = serde_json::to_string(selector).expect("string encodes");
            format!("(()=>{{let l;try{{l=document.querySelectorAll({sel})}}catch(e){{return{{rects:[],error:String(e)}}}}return{{rects:[...l].slice(0,500).map(e=>({BOX})(e.getBoundingClientRect()))}}}})()")
        }
        Q::Selection => format!("(()=>{{const s=getSelection();if(!s||!s.rangeCount)return{{rects:[]}};return{{rects:[...s.getRangeAt(0).getClientRects()].map({BOX})}}}})()"),
    };
    evaluate(id, req, Op::Overlay { epoch }, &expr)
}

/// `{rects: [{x, y, w, h}]}` from an overlay evaluate.
pub fn overlay_rects(value: &serde_json::Value) -> Vec<herdr_browser_protocol::CssRect> {
    let f = |v: &serde_json::Value| v.as_f64().unwrap_or(0.0) as f32;
    value["rects"]
        .as_array()
        .map(|a| a.iter().map(|r| herdr_browser_protocol::CssRect { x: f(&r["x"]), y: f(&r["y"]), w: f(&r["w"]), h: f(&r["h"]) }).collect())
        .unwrap_or_default()
}

fn evaluate(id: &str, req: u64, op: Op, expr: &str) -> Vec<u8> {
    let msg = NEXT_MSG.get();
    NEXT_MSG.set(if msg == i32::MAX { FIRST_EVAL_MSG } else { msg + 1 });
    EVALS.with_borrow_mut(|e| e.insert(msg, (id.to_string(), req, op)));
    let call = serde_json::json!({
        "id": msg,
        "method": "Runtime.evaluate",
        "params": { "expression": expr, "awaitPromise": true, "returnByValue": true, "userGesture": true },
    });
    serde_json::to_vec(&call).expect("json encodes")
}

/// The eval for DevTools message `msg` failed to send: forget it.
pub fn eval_unsent(id: &str) -> Option<u64> {
    EVALS.with_borrow_mut(|e| {
        let msg = e.iter().filter(|(_, (b, _, _))| b == id).map(|(m, _)| *m).max()?;
        e.remove(&msg).map(|(_, req, _)| req)
    })
}

/// Turn a `Runtime.evaluate` result into (ok, value).
pub fn eval_outcome(success: bool, result: &[u8]) -> (bool, serde_json::Value) {
    let v: serde_json::Value = serde_json::from_slice(result).unwrap_or_default();
    if !success {
        let msg = v["message"].as_str().unwrap_or("DevTools refused the eval");
        return (false, serde_json::Value::String(msg.into()));
    }
    if let Some(ex) = v.get("exceptionDetails") {
        let text = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("exception");
        return (false, serde_json::Value::String(text.into()));
    }
    (true, v["result"].get("value").cloned().unwrap_or(serde_json::Value::Null))
}

fn s(v: Option<&CefString>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

fn uf(v: &CefStringUserfree) -> String {
    CefString::from(v).to_string()
}

#[derive(Clone)]
pub struct HostApp;

wrap_app! {
    pub(crate) struct AppBuilder {
        app: HostApp,
    }

    impl App {
        fn on_before_command_line_processing(&self, _process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            let Some(cl) = command_line else { return };
            // Never touch the login keychain (no prompt on the user's screen); no first-run UI.
            for sw in ["use-mock-keychain", "no-startup-window", "noerrdialogs", "hide-crash-restore-bubble", "disable-features=MacAppCodeSignClone"] {
                cl.append_switch(Some(&sw.into()));
            }
        }
    }
}

pub fn app() -> App {
    AppBuilder::new(HostApp)
}

wrap_render_handler! {
    pub(crate) struct Render {
        id: String,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let (Some(rect), Some((size, _))) = (rect, view_of(&self.id)) else { return };
            rect.x = 0;
            rect.y = 0;
            rect.width = (size.w.round() as i32).max(1);
            rect.height = (size.h.round() as i32).max(1);
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            let (Some(info), Some((_, scale))) = (info, view_of(&self.id)) else { return 0 };
            info.device_scale_factor = scale;
            1
        }

        fn on_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty_rects: Option<&[Rect]>, _buffer: *const u8, width: ::std::os::raw::c_int, height: ::std::os::raw::c_int) {
            super::debug(format_args!("{}: software paint type {:?} {width}x{height} (unused)", self.id, *type_.as_ref()));
            // Software frames are not used: shared_texture_enabled routes paints to on_accelerated_paint.
        }

        fn on_accelerated_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty_rects: Option<&[Rect]>, info: Option<&AcceleratedPaintInfo>) {
            let Some(info) = info else { return };
            if info.shared_texture_io_surface.is_null() {
                return;
            }
            let (w, h) = (info.extra.coded_size.width as u32, info.extra.coded_size.height as u32);
            match *type_.as_ref() {
                sys::cef_paint_element_type_t::PET_VIEW => on_frame(&self.id, info.shared_texture_io_surface, w, h),
                sys::cef_paint_element_type_t::PET_POPUP => on_popup_frame(&self.id, info.shared_texture_io_surface, w, h),
                _ => {}
            }
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            on_popup_show(&self.id, show != 0);
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            if let Some(r) = rect {
                on_popup_size(&self.id, r.x, r.y, r.width, r.height);
            }
        }

        fn on_scroll_offset_changed(&self, _browser: Option<&mut Browser>, x: f64, y: f64) {
            super::on_scroll(&self.id, x, y);
        }

        fn on_ime_composition_range_changed(&self, _browser: Option<&mut Browser>, _selected_range: Option<&Range>, character_bounds: Option<&[Rect]>) {
            let Some(r) = character_bounds.and_then(|b| b.last()) else { return };
            send(HostToApp::ImeCaret { id: self.id.clone(), x: r.x as f32, y: r.y as f32, w: r.width as f32, h: r.height as f32 });
        }
    }
}

fn cursor_name(t: sys::cef_cursor_type_t) -> &'static str {
    use sys::cef_cursor_type_t as C;
    match t {
        C::CT_HAND => "pointer",
        C::CT_IBEAM => "text",
        C::CT_VERTICALTEXT => "vertical-text",
        C::CT_CROSS => "crosshair",
        C::CT_WAIT => "wait",
        C::CT_PROGRESS => "progress",
        C::CT_HELP => "help",
        C::CT_MOVE => "move",
        C::CT_GRAB => "grab",
        C::CT_GRABBING => "grabbing",
        C::CT_NOTALLOWED | C::CT_NODROP => "not-allowed",
        C::CT_COLUMNRESIZE | C::CT_EASTWESTRESIZE | C::CT_EASTRESIZE | C::CT_WESTRESIZE => "ew-resize",
        C::CT_ROWRESIZE | C::CT_NORTHSOUTHRESIZE | C::CT_NORTHRESIZE | C::CT_SOUTHRESIZE => "ns-resize",
        C::CT_NORTHEASTSOUTHWESTRESIZE | C::CT_NORTHEASTRESIZE | C::CT_SOUTHWESTRESIZE => "nesw-resize",
        C::CT_NORTHWESTSOUTHEASTRESIZE | C::CT_NORTHWESTRESIZE | C::CT_SOUTHEASTRESIZE => "nwse-resize",
        C::CT_CONTEXTMENU => "context-menu",
        C::CT_ALIAS => "alias",
        C::CT_COPY => "copy",
        C::CT_CELL => "cell",
        C::CT_ZOOMIN => "zoom-in",
        C::CT_ZOOMOUT => "zoom-out",
        C::CT_NONE => "none",
        _ => "default",
    }
}

wrap_display_handler! {
    pub(crate) struct Display {
        id: String,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() == 1) {
                send(HostToApp::Url { id: self.id.clone(), url: s(url) });
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            send(HostToApp::Title { id: self.id.clone(), title: s(title) });
        }

        fn on_favicon_urlchange(&self, _browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            let Some(list) = icon_urls else { return };
            if let Some(url) = std::mem::take(list).into_iter().next() {
                send(HostToApp::Favicon { id: self.id.clone(), url: url.to_string() });
            }
        }

        fn on_console_message(&self, _browser: Option<&mut Browser>, level: LogSeverity, message: Option<&CefString>, source: Option<&CefString>, line: ::std::os::raw::c_int) -> ::std::os::raw::c_int {
            // Console lines are page text, forwarded as such: never parsed for results.
            let message = s(message);
            use sys::cef_log_severity_t as L;
            let level = match *level.as_ref() {
                L::LOGSEVERITY_VERBOSE => "debug",
                L::LOGSEVERITY_WARNING => "warn",
                L::LOGSEVERITY_ERROR | L::LOGSEVERITY_FATAL => "error",
                _ => "info",
            };
            send(HostToApp::Console { id: self.id.clone(), level: level.into(), message, source: s(source), line: line.max(0) as u32 });
            1
        }

        fn on_cursor_change(&self, _browser: Option<&mut Browser>, _cursor: *mut u8, type_: CursorType, _custom_cursor_info: Option<&CursorInfo>) -> ::std::os::raw::c_int {
            send(HostToApp::Cursor { id: self.id.clone(), name: cursor_name(*type_.as_ref()).into() });
            1
        }
    }
}

wrap_load_handler! {
    pub(crate) struct Load {
        id: String,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, is_loading: ::std::os::raw::c_int, can_go_back: ::std::os::raw::c_int, can_go_forward: ::std::os::raw::c_int) {
            super::on_loading(&self.id, is_loading != 0);
            send(HostToApp::Loading { id: self.id.clone(), loading: is_loading != 0, can_back: can_go_back != 0, can_forward: can_go_forward != 0 });
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct LifeSpan {
        id: String,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            super::OPEN.set(super::OPEN.get() + 1);
            if let Some(b) = browser {
                super::on_created(&self.id, b.clone());
            }
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            super::OPEN.set(super::OPEN.get().saturating_sub(1));
        }

        fn on_before_popup(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, _popup_id: ::std::os::raw::c_int, target_url: Option<&CefString>, _target_frame_name: Option<&CefString>, target_disposition: WindowOpenDisposition, _user_gesture: ::std::os::raw::c_int, _popup_features: Option<&PopupFeatures>, _window_info: Option<&mut WindowInfo>, _client: Option<&mut Option<Client>>, _settings: Option<&mut BrowserSettings>, _extra_info: Option<&mut Option<DictionaryValue>>, _no_javascript_access: Option<&mut ::std::os::raw::c_int>) -> ::std::os::raw::c_int {
            use sys::cef_window_open_disposition_t as W;
            let disposition = match *target_disposition.as_ref() {
                W::CEF_WOD_NEW_FOREGROUND_TAB => "new_foreground_tab",
                W::CEF_WOD_NEW_BACKGROUND_TAB => "new_background_tab",
                W::CEF_WOD_NEW_POPUP => "new_popup",
                W::CEF_WOD_NEW_WINDOW => "new_window",
                _ => "other",
            };
            send(HostToApp::NewWindow { id: self.id.clone(), url: s(target_url), disposition: disposition.into() });
            1 // the app decides; CEF never opens its own window
        }
    }
}

wrap_jsdialog_handler! {
    pub(crate) struct Dialogs {
        id: String,
    }

    impl JsdialogHandler {
        fn on_jsdialog(&self, _browser: Option<&mut Browser>, _origin_url: Option<&CefString>, dialog_type: JsdialogType, message_text: Option<&CefString>, default_prompt_text: Option<&CefString>, callback: Option<&mut JsdialogCallback>, _suppress_message: Option<&mut ::std::os::raw::c_int>) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            use sys::cef_jsdialog_type_t as J;
            let kind = match *dialog_type.as_ref() {
                J::JSDIALOGTYPE_CONFIRM => DialogKind::Confirm,
                J::JSDIALOGTYPE_PROMPT => DialogKind::Prompt,
                _ => DialogKind::Alert,
            };
            let req = next_req();
            keep_dialog(req, cb.clone());
            let default_text = (kind == DialogKind::Prompt).then(|| s(default_prompt_text));
            send(HostToApp::Dialog { id: self.id.clone(), req, kind, message: s(message_text), default_text });
            1
        }

        fn on_before_unload_dialog(&self, _browser: Option<&mut Browser>, message_text: Option<&CefString>, _is_reload: ::std::os::raw::c_int, callback: Option<&mut JsdialogCallback>) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let req = next_req();
            keep_dialog(req, cb.clone());
            send(HostToApp::Dialog { id: self.id.clone(), req, kind: DialogKind::BeforeUnload, message: s(message_text), default_text: None });
            1
        }
    }
}

wrap_dialog_handler! {
    pub(crate) struct Files {
        id: String,
    }

    impl DialogHandler {
        fn on_file_dialog(&self, _browser: Option<&mut Browser>, mode: FileDialogMode, _title: Option<&CefString>, _default_file_path: Option<&CefString>, accept_filters: Option<&mut CefStringList>, _accept_extensions: Option<&mut CefStringList>, _accept_descriptions: Option<&mut CefStringList>, callback: Option<&mut FileDialogCallback>) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let multiple = *mode.as_ref() == sys::cef_file_dialog_mode_t::FILE_DIALOG_OPEN_MULTIPLE;
            let accept = accept_filters.map(|l| std::mem::take(l).into_iter().map(|a| a.to_string()).collect()).unwrap_or_default();
            let req = next_req();
            keep_chooser(req, cb.clone());
            send(HostToApp::FileChooser { id: self.id.clone(), req, multiple, accept });
            1
        }
    }
}

wrap_download_handler! {
    pub(crate) struct Downloads {
        id: String,
    }

    impl DownloadHandler {
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>, _request_method: Option<&CefString>) -> ::std::os::raw::c_int {
            1
        }

        fn on_before_download(&self, _browser: Option<&mut Browser>, item: Option<&mut DownloadItem>, suggested_name: Option<&CefString>, callback: Option<&mut BeforeDownloadCallback>) -> ::std::os::raw::c_int {
            let (Some(item), Some(cb)) = (item, callback) else { return 0 };
            let dir = downloads_dir();
            let _ = std::fs::create_dir_all(&dir);
            let name = s(suggested_name);
            // Two downloads of the same name can start before either file exists: reserve names.
            let path = RESERVED.with_borrow_mut(|r| {
                let p = crate::downloads::unique_path_except(&dir, &name, r);
                r.insert(p.clone());
                p
            });
            cb.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
            send(HostToApp::Download {
                id: self.id.clone(),
                req: item.id() as u64,
                url: uf(&item.url()),
                suggested_name: name,
                state: DownloadState::Started,
                path: Some(path.to_string_lossy().into()),
                received: 0,
                total: u64::try_from(item.total_bytes()).ok().filter(|t| *t > 0),
            });
            1
        }

        fn on_download_updated(&self, _browser: Option<&mut Browser>, item: Option<&mut DownloadItem>, _callback: Option<&mut DownloadItemCallback>) {
            let Some(item) = item else { return };
            let state = if item.is_complete() == 1 {
                DownloadState::Done
            } else if item.is_canceled() == 1 || item.is_interrupted() == 1 {
                DownloadState::Failed
            } else {
                DownloadState::Progress
            };
            let path = uf(&item.full_path());
            send(HostToApp::Download {
                id: self.id.clone(),
                req: item.id() as u64,
                url: uf(&item.url()),
                suggested_name: uf(&item.suggested_file_name()),
                state,
                path: (!path.is_empty()).then_some(path),
                received: item.received_bytes().max(0) as u64,
                total: u64::try_from(item.total_bytes()).ok().filter(|t| *t > 0),
            });
        }
    }
}

wrap_find_handler! {
    pub(crate) struct Finds {
        id: String,
    }

    impl FindHandler {
        fn on_find_result(&self, _browser: Option<&mut Browser>, _identifier: ::std::os::raw::c_int, count: ::std::os::raw::c_int, _selection_rect: Option<&Rect>, active_match_ordinal: ::std::os::raw::c_int, final_update: ::std::os::raw::c_int) {
            send(HostToApp::FindResult { id: self.id.clone(), count: count.max(0) as u32, active: active_match_ordinal.max(0) as u32, final_update: final_update != 0 });
        }
    }
}

/// App-handled menu extras (CEF's MENU_ID_USER_FIRST and up): the app acts, then answers None.
pub const MENU_OPEN_LINK_IN_NEW_TAB: i32 = 26500;
pub const MENU_COPY_LINK_ADDRESS: i32 = 26501;
pub const MENU_SAVE_IMAGE_AS: i32 = 26502;

fn menu_items(model: &MenuModel) -> Vec<herdr_browser_protocol::MenuItem> {
    use sys::cef_menu_item_type_t as T;
    (0..model.count())
        .filter(|&i| model.is_visible_at(i) == 1)
        .map(|i| {
            let kind = *model.type_at(i).as_ref();
            let separator = kind == T::MENUITEMTYPE_SEPARATOR;
            herdr_browser_protocol::MenuItem {
                command: if separator { 0 } else { model.command_id_at(i) },
                // CEF labels carry "&" accelerator marks ("&Back"); "&&" is a literal "&".
                label: uf(&model.label_at(i)).replace("&&", "\u{0}").replace('&', "").replace('\u{0}', "&"),
                enabled: !separator && model.is_enabled_at(i) == 1,
                checked: model.is_checked_at(i) == 1,
                submenu: if kind == T::MENUITEMTYPE_SUBMENU { model.sub_menu_at(i).map(|m| menu_items(&m)).unwrap_or_default() } else { vec![] },
            }
        })
        .collect()
}

fn extra(command: i32, label: &str) -> herdr_browser_protocol::MenuItem {
    herdr_browser_protocol::MenuItem { command, label: label.into(), enabled: true, checked: false, submenu: vec![] }
}

wrap_context_menu_handler! {
    pub(crate) struct Menus {
        id: String,
    }

    impl ContextMenuHandler {
        fn run_context_menu(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, model: Option<&mut MenuModel>, callback: Option<&mut RunContextMenuCallback>) -> ::std::os::raw::c_int {
            let (Some(params), Some(model), Some(cb)) = (params, model, callback) else { return 0 };
            use sys::cef_context_menu_type_flags_t as F;
            let flags = params.type_flags().as_ref().0;
            let mut kind: Vec<String> = Vec::new();
            for (flag, name) in [(F::CM_TYPEFLAG_PAGE, "page"), (F::CM_TYPEFLAG_LINK, "link"), (F::CM_TYPEFLAG_MEDIA, "media"), (F::CM_TYPEFLAG_SELECTION, "selection"), (F::CM_TYPEFLAG_EDITABLE, "editable")] {
                if flags & flag.0 != 0 {
                    kind.push(name.into());
                }
            }
            let image = params.has_image_contents() == 1 || *params.media_type().as_ref() == sys::cef_context_menu_media_type_t::CM_MEDIATYPE_IMAGE;
            if image {
                kind.push("image".into());
            }
            let nonempty = |s: String| (!s.is_empty()).then_some(s);
            let link_url = nonempty(uf(&params.link_url()));
            let src_url = nonempty(uf(&params.source_url()));
            let selection = nonempty(uf(&params.selection_text()));
            let mut items = menu_items(model);
            let mut extras = Vec::new();
            if link_url.is_some() {
                extras.push(extra(MENU_OPEN_LINK_IN_NEW_TAB, "Open Link in New Tab"));
                extras.push(extra(MENU_COPY_LINK_ADDRESS, "Copy Link Address"));
            }
            if image && src_url.is_some() {
                extras.push(extra(MENU_SAVE_IMAGE_AS, "Save Image As…"));
            }
            if !extras.is_empty() {
                extras.push(herdr_browser_protocol::MenuItem { command: 0, label: String::new(), enabled: false, checked: false, submenu: vec![] });
                extras.append(&mut items);
                items = extras;
            }
            let req = next_req();
            super::keep_menu(&self.id, req, cb.clone());
            send(HostToApp::ContextMenu { id: self.id.clone(), req, x: params.xcoord() as f32, y: params.ycoord() as f32, kind, link_url, src_url, selection, items });
            1 // the app draws the menu; CEF never opens a native one
        }
    }
}

wrap_client! {
    pub(crate) struct ClientBuilder {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        jsdialog: JsdialogHandler,
        dialog: DialogHandler,
        download: DownloadHandler,
        find: FindHandler,
        menus: ContextMenuHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.jsdialog.clone())
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.dialog.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download.clone())
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(self.find.clone())
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.menus.clone())
        }
    }
}

wrap_completion_callback! {
    pub(crate) struct FlushDone;

    impl CompletionCallback {
        fn on_complete(&self) {
            super::FLUSHED.set(super::FLUSHED.get() + 1);
        }
    }
}

pub fn flush_done() -> CompletionCallback {
    FlushDone::new()
}

/// The DevTools message id for `Emulation.setEmulatedMedia` (its result is not needed).
pub const SCHEME_MSG: i32 = 2;

/// prefers-color-scheme for one page, through DevTools media emulation (live; survives navigation).
pub fn scheme_message(dark: bool) -> Vec<u8> {
    let call = serde_json::json!({
        "id": SCHEME_MSG,
        "method": "Emulation.setEmulatedMedia",
        "params": { "features": [{ "name": "prefers-color-scheme", "value": if dark { "dark" } else { "light" } }] },
    });
    serde_json::to_vec(&call).expect("json encodes")
}

/// The DevTools message id used for `Target.getTargetInfo` (one per browser, so any fixed id works).
pub const TARGET_INFO_MSG: i32 = 1;

wrap_dev_tools_message_observer! {
    pub(crate) struct DevToolsTarget {
        id: String,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, _browser: Option<&mut Browser>, message_id: ::std::os::raw::c_int, success: ::std::os::raw::c_int, result: Option<&[u8]>) {
            if message_id >= FIRST_EVAL_MSG {
                // Only the browser that sent the eval may answer it.
                let owner = EVALS.with_borrow_mut(|e| match e.get(&message_id) {
                    Some((b, _, _)) if *b == self.id => e.remove(&message_id),
                    _ => None,
                });
                if let Some((id, req, op)) = owner {
                    let (ok, value) = eval_outcome(success != 0, result.unwrap_or_default());
                    match op {
                        Op::Eval => send(HostToApp::EvalResult { id, req, ok, value }),
                        Op::Overlay { epoch } => {
                            if !ok || value.get("error").is_some() {
                                super::debug(format_args!("{id}: overlay {req} found nothing: {value}"));
                            }
                            let rects = if ok { overlay_rects(&value) } else { vec![] };
                            send(HostToApp::OverlayRects { id, req, epoch, rects, frames: vec![] });
                        }
                    }
                }
                return;
            }
            if message_id != TARGET_INFO_MSG || success == 0 {
                return;
            }
            let v: serde_json::Value = result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or_default();
            if let Some(t) = v["targetInfo"]["targetId"].as_str() {
                send(HostToApp::CdpTarget { id: self.id.clone(), target_id: t.to_string() });
            }
        }
    }
}

pub fn devtools_observer(id: &str) -> DevToolsMessageObserver {
    DevToolsTarget::new(id.to_string())
}

/// A client for browser `id`: every callback it gets is tagged with that id.
pub fn client(id: &str) -> Client {
    let id = id.to_string();
    ClientBuilder::new(
        Render::new(id.clone()),
        Display::new(id.clone()),
        Load::new(id.clone()),
        LifeSpan::new(id.clone()),
        Dialogs::new(id.clone()),
        Files::new(id.clone()),
        Downloads::new(id.clone()),
        Finds::new(id.clone()),
        Menus::new(id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_outcomes() {
        assert_eq!(eval_outcome(true, br#"{"result":{"type":"number","value":2,"description":"2"}}"#), (true, serde_json::json!(2)));
        assert_eq!(eval_outcome(true, br#"{"result":{"type":"undefined"}}"#), (true, serde_json::Value::Null));
        let (ok, v) = eval_outcome(true, br#"{"result":{"type":"object"},"exceptionDetails":{"text":"Uncaught","exception":{"description":"ReferenceError: nope is not defined"}}}"#);
        assert!(!ok && v == "ReferenceError: nope is not defined");
        assert_eq!(eval_outcome(false, br#"{"code":-32000,"message":"no target"}"#), (false, serde_json::json!("no target")));
    }

    #[test]
    fn eval_messages_are_bound_to_their_browser() {
        let call: serde_json::Value = serde_json::from_slice(&eval_message("b1", 9, "1+1")).unwrap();
        assert_eq!(call["method"], "Runtime.evaluate");
        let msg = call["id"].as_i64().unwrap() as i32;
        assert!(msg >= FIRST_EVAL_MSG);
        EVALS.with_borrow(|e| assert_eq!(e.get(&msg), Some(&("b1".to_string(), 9, Op::Eval))));
        assert_eq!(eval_unsent("b1"), Some(9));
        EVALS.with_borrow(|e| assert!(e.get(&msg).is_none()));
    }
}
