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
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// The style panel (S10): `node` is set for SetStyle (to remember the original attribute)
    /// and RevertStyle (to forget it once restored).
    Styles { set: Option<String>, revert: Option<String> },
    /// A pick (S4): describe the element under a point.
    Pick,
    Eval,
    Overlay { epoch: u64 },
    /// The page-is-live probe for load `generation` (see `super::on_loading`).
    Ready { generation: u64 },
}

/// The readiness probe: answers once a trusted mouse move has reached the page (the host keeps
/// sending harmless moves until then, see `super::nudge_waiting`), or 0 after 5 s. Measured: a
/// first-paint entry was not enough; under CPU load Chromium still dropped clicks sent after it,
/// so this proves the input path itself before held input is replayed.
pub fn ready_message(id: &str, generation: u64) -> Vec<u8> {
    const INPUT_LIVE: &str = "new Promise(r=>{const f=e=>{if(e.isTrusted){removeEventListener('mousemove',f,true);r(1)}};\
        addEventListener('mousemove',f,true);setTimeout(()=>{removeEventListener('mousemove',f,true);r(0)},5000)})";
    evaluate(id, 0, Op::Ready { generation }, INPUT_LIVE)
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

thread_local! {
    /// The style attribute each (browser, node) had before the panel's first SetStyle: None if it
    /// had none. Kept in the host, out of the page's reach, until RevertStyle restores it.
    static ORIGINAL_STYLE: std::cell::RefCell<std::collections::HashMap<(String, String), Option<String>>> = Default::default();
}

/// JS shared by the style requests: find `node`, check property names, build rows. Rows carry
/// CSS values only (computed, inline, best-effort winning stylesheet value); properties that can
/// hold page text (`content`, `quotes`) are refused (rule 1).
fn style_js(node: &str, body: &str) -> String {
    let node = serde_json::to_string(node).expect("string encodes");
    format!(
        "(()=>{{let el;try{{el=document.querySelector({node})}}catch(e){{return{{ok:false}}}}if(!el)return{{ok:false}};\
         const okp=p=>typeof p==='string'&&/^-{{0,2}}[a-z][a-z0-9-]*$/.test(p)&&p!=='content'&&p!=='quotes';\
         const row=p=>{{let m=null;for(const sh of document.styleSheets){{let rs;try{{rs=sh.cssRules}}catch(e){{continue}}\
         for(const r of rs){{if(!r.style||!r.selectorText)continue;let hit=false;try{{hit=el.matches(r.selectorText)}}catch(e){{}}\
         if(hit){{const v=r.style.getPropertyValue(p);if(v)m=v.trim()}}}}}}\
         const i=el.style.getPropertyValue(p);return{{prop:p,computed:getComputedStyle(el).getPropertyValue(p).trim(),inline:i?i.trim():null,matched:m}}}};\
         {body}}})()"
    )
}

/// `Styles`: one row per property.
pub fn styles_message(id: &str, req: u64, node: &str, props: &[String]) -> Vec<u8> {
    let props = serde_json::to_string(props).expect("strings encode");
    let body = format!("const ps={props};if(!ps.every(okp))return{{ok:false}};return{{ok:true,rows:ps.map(row)}}");
    evaluate(id, req, Op::Styles { set: None, revert: None }, &style_js(node, &body))
}

/// `SetStyle`: set (or remove) one inline property; report the attribute it had before.
pub fn set_style_message(id: &str, req: u64, node: &str, prop: &str, value: Option<&str>) -> Vec<u8> {
    let (p, v) = (serde_json::to_string(prop).expect("encodes"), serde_json::to_string(&value).expect("encodes"));
    let body = format!(
        "const p={p},v={v};if(!okp(p))return{{ok:false}};const orig=el.getAttribute('style');\
         if(v===null)el.style.removeProperty(p);else el.style.setProperty(p,v);return{{ok:true,orig,rows:[row(p)]}}"
    );
    evaluate(id, req, Op::Styles { set: Some(node.into()), revert: None }, &style_js(node, &body))
}

/// `RevertStyle`: put back the attribute remembered at the first SetStyle (nothing to do if the
/// panel never changed this node).
pub fn revert_style_message(id: &str, req: u64, node: &str) -> Vec<u8> {
    let original = ORIGINAL_STYLE.with_borrow(|m| m.get(&(id.to_string(), node.to_string())).cloned());
    super::debug(format_args!("{id}: revert {node}: original {original:?}"));
    let body = match original {
        None => "return{ok:true,rows:[]}".to_string(),
        Some(o) => {
            let o = serde_json::to_string(&o).expect("encodes");
            // Reading the attribute first flushes Chromium's lazy inline-style sync; without it a
            // removed attribute comes back as style="" after an el.style change.
            format!("const o={o};el.getAttribute('style');if(o===null)el.removeAttribute('style');else el.setAttribute('style',o);return{{ok:true,rows:[],after:el.getAttribute('style')}}")
        }
    };
    evaluate(id, req, Op::Styles { set: None, revert: Some(node.into()) }, &style_js(node, &body))
}

/// `PickInfo`: the element at (x, y) viewport CSS px (descending open shadow roots), a selector
/// that matches only it (id, else a tag.class:nth-of-type path checked with querySelectorAll),
/// a label (tag plus up to 30 chars of its own text, aria-label or alt; form fields never give
/// their value, password fields give nothing) and its box.
pub fn pick_message(id: &str, req: u64, x: f32, y: f32) -> Vec<u8> {
    let js = format!(
        "(()=>{{const X={x},Y={y};let el=document.elementFromPoint(X,Y);if(!el)return{{ok:false}};\
         while(el.shadowRoot){{const inner=el.shadowRoot.elementFromPoint(X,Y);if(!inner||inner===el)break;el=inner}}\
         const root=el.getRootNode(),esc=s=>CSS.escape(s);\
         const one=s=>{{try{{return root.querySelectorAll(s).length===1}}catch(e){{return false}}}};\
         let selector=null;if(el.id&&one('#'+esc(el.id)))selector='#'+esc(el.id);\
         else{{const parts=[];let n=el;while(n&&n.nodeType===1){{\
           if(n!==el&&n.id&&one('#'+esc(n.id))){{parts.unshift('#'+esc(n.id));if(one(parts.join(' > '))){{selector=parts.join(' > ')}};break}}\
           let p=n.tagName.toLowerCase()+[...n.classList].slice(0,2).map(c=>'.'+esc(c)).join('');\
           const par=n.parentElement;if(par){{const same=[...par.children].filter(c=>c.tagName===n.tagName);if(same.length>1)p+=':nth-of-type('+(same.indexOf(n)+1)+')'}}\
           parts.unshift(p);if(one(parts.join(' > '))){{selector=parts.join(' > ');break}}n=par}}\
           if(!selector)selector=parts.join(' > ')}}\
         const tag=el.tagName.toLowerCase(),field=/^(input|textarea|select|option)$/.test(tag)||el.isContentEditable;\
         let t='';if(!(tag==='input'&&(el.type||'').toLowerCase()==='password')){{\
           t=el.getAttribute('aria-label')||el.getAttribute('alt')||(field?'':[...el.childNodes].filter(c=>c.nodeType===3).map(c=>c.textContent).join(' '))}}\
         t=t.replace(/\\s+/g,' ').trim().slice(0,30);\
         const label=tag==='input'&&(el.type||'').toLowerCase()==='password'?'':(t?tag+' '+t:tag);\
         const r=el.getBoundingClientRect();return{{ok:true,selector,label,rect:{{x:r.x,y:r.y,w:r.width,h:r.height}}}}}})()"
    );
    evaluate(id, req, Op::Pick, &js)
}

fn style_rows(value: &serde_json::Value) -> Vec<herdr_browser_protocol::StyleRow> {
    let text = |v: &serde_json::Value| v.as_str().map(str::to_string);
    value["rows"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| herdr_browser_protocol::StyleRow { prop: text(&r["prop"]).unwrap_or_default(), computed: text(&r["computed"]).unwrap_or_default(), inline: text(&r["inline"]), matched: text(&r["matched"]) })
                .collect()
        })
        .unwrap_or_default()
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

/// The protocol's name for where a link or script wants a page opened.
fn disposition_name(d: &WindowOpenDisposition) -> &'static str {
    use sys::cef_window_open_disposition_t as W;
    match *d.as_ref() {
        W::CEF_WOD_CURRENT_TAB => "current_tab",
        W::CEF_WOD_NEW_FOREGROUND_TAB => "new_foreground_tab",
        W::CEF_WOD_NEW_BACKGROUND_TAB => "new_background_tab",
        W::CEF_WOD_NEW_POPUP => "new_popup",
        W::CEF_WOD_NEW_WINDOW => "new_window",
        _ => "other",
    }
}

wrap_request_handler! {
    pub(crate) struct Requests {
        id: String,
    }

    impl RequestHandler {
        /// Cmd-click, middle-click and shift-click on links arrive here, not in OnBeforePopup:
        /// anything but "current tab" goes to the app as NewWindow and the tab stays put.
        fn on_open_urlfrom_tab(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, target_url: Option<&CefString>, target_disposition: WindowOpenDisposition, _user_gesture: ::std::os::raw::c_int) -> ::std::os::raw::c_int {
            if *target_disposition.as_ref() == sys::cef_window_open_disposition_t::CEF_WOD_CURRENT_TAB {
                return 0;
            }
            send(HostToApp::NewWindow { id: self.id.clone(), url: s(target_url), disposition: disposition_name(&target_disposition).into() });
            1
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
            let disposition = disposition_name(&target_disposition);
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
        requests: RequestHandler,
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
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.requests.clone())
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
                        Op::Styles { set, revert } => {
                            let ok = ok && value["ok"].as_bool() == Some(true);
                            if ok {
                                let key = |node: String| (id.clone(), node);
                                if let Some(node) = set {
                                    let orig = value["orig"].as_str().map(str::to_string);
                                    super::debug(format_args!("{id}: set {node}: page reported original {:?}", value["orig"]));
                                    ORIGINAL_STYLE.with_borrow_mut(|m| {
                                        m.entry(key(node)).or_insert(orig);
                                    });
                                }
                                if let Some(node) = revert.as_ref() {
                                    super::debug(format_args!("{id}: reverted {node}: attribute now {:?}", value["after"]));
                                }
                                if let Some(node) = revert {
                                    ORIGINAL_STYLE.with_borrow_mut(|m| m.remove(&key(node)));
                                }
                            }
                            let rows = if ok { style_rows(&value) } else { vec![] };
                            send(HostToApp::StylesResult { id, req, ok, rows });
                        }
                        Op::Pick => {
                            let ok = ok && value["ok"].as_bool() == Some(true);
                            let text = |k: &str| if ok { value[k].as_str().unwrap_or_default().to_string() } else { String::new() };
                            let f = |k: &str| value["rect"][k].as_f64().unwrap_or(0.0) as f32;
                            let rect = herdr_browser_protocol::CssRect { x: f("x"), y: f("y"), w: f("w"), h: f("h") };
                            send(HostToApp::PickInfoResult { id, req, ok, selector: text("selector"), label: text("label"), rect });
                        }
                        Op::Ready { generation } => {
                            super::debug(format_args!("{id}: input path live: {value}"));
                            super::input_live(&id, generation)
                        }
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
        Menus::new(id.clone()),
        Requests::new(id),
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
