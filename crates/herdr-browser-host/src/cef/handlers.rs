//! CEF callbacks, each turned into a protocol message for the app.

use super::{downloads_dir, keep_chooser, keep_dialog, next_req, on_frame, send, view_of};
use cef::*;
use herdr_browser_protocol::{DialogKind, DownloadState, HostToApp};

const EVAL_TAG: &str = "__herdr_eval__:";

/// JS that evaluates `expr` (awaiting promises) and reports through a tagged console line,
/// which `on_console_message` turns into `EvalResult` instead of `Console`.
pub fn eval_script(req: u64, expr: &str) -> String {
    let expr = serde_json::to_string(expr).expect("string encodes");
    format!(
        "(async()=>{{let r;try{{let v=await (0,eval)({expr});r={{ok:true,v:v===undefined?null:v}}}}catch(e){{r={{ok:false,v:String(e)}}}}\
         let s;try{{s=JSON.stringify(r)}}catch(e){{s=JSON.stringify({{ok:true,v:String(r.v)}})}}console.log('{EVAL_TAG}{req}:'+s)}})()"
    )
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

        fn on_paint(&self, _browser: Option<&mut Browser>, _type_: PaintElementType, _dirty_rects: Option<&[Rect]>, _buffer: *const u8, _width: ::std::os::raw::c_int, _height: ::std::os::raw::c_int) {
            // Software frames are not used: shared_texture_enabled routes paints to on_accelerated_paint.
        }

        fn on_accelerated_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty_rects: Option<&[Rect]>, info: Option<&AcceleratedPaintInfo>) {
            let Some(info) = info else { return };
            if type_ != PaintElementType::default() || info.shared_texture_io_surface.is_null() {
                return;
            }
            let (w, h) = (info.extra.coded_size.width as u32, info.extra.coded_size.height as u32);
            on_frame(&self.id, info.shared_texture_io_surface, w, h);
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
            let message = s(message);
            if let Some(rest) = message.strip_prefix(EVAL_TAG) {
                if let Some((req, json)) = rest.split_once(':') {
                    let r: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                    send(HostToApp::EvalResult {
                        id: self.id.clone(),
                        req: req.parse().unwrap_or(0),
                        ok: r["ok"].as_bool().unwrap_or(false),
                        value: r["v"].clone(),
                    });
                    return 1;
                }
            }
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
            send(HostToApp::Loading { id: self.id.clone(), loading: is_loading != 0, can_back: can_go_back != 0, can_forward: can_go_forward != 0 });
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct LifeSpan {
        id: String,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            super::OPEN.set(super::OPEN.get() + 1);
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
            let path = dir.join(if name.is_empty() { "download".to_string() } else { name.clone() });
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
    }
}

/// The DevTools message id used for `Target.getTargetInfo` (one per browser, so any fixed id works).
pub const TARGET_INFO_MSG: i32 = 1;

wrap_dev_tools_message_observer! {
    pub(crate) struct DevToolsTarget {
        id: String,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, _browser: Option<&mut Browser>, message_id: ::std::os::raw::c_int, success: ::std::os::raw::c_int, result: Option<&[u8]>) {
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
        Finds::new(id),
    )
}
