//! Design slice S10 v1 (part 4), the panel model: ten properties of the picked element, each with
//! its computed value, its own inline value and the user's edit; badges say which is which;
//! Revert puts back the original `style` attribute exactly. The host reads and writes through
//! typed messages (Styles, SetStyle, RevertStyle; protocol pending with the lead); this model only
//! decides what to ask and what to show. Values are CSS only, never input values or text.

use super::edits::StyleEdit;
use super::protocol::{AppToHost, BrowserId, StyleRow};
use super::HostLink;
use gpui::{Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px, rgb};
use gpui_base::input::{Input, InputEvent, InputState};
use std::sync::Arc;

/// The v1 catalogue, in panel order.
pub const PROPS: [&str; 10] = ["font-size", "font-weight", "color", "background-color", "border-radius", "padding", "margin", "gap", "width", "height"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Changed in this panel.
    Edited,
    /// Set on the element's own style attribute before the panel opened.
    Inline,
    /// Comes from stylesheets or defaults.
    Computed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub prop: String,
    /// What the page computes now.
    pub computed: String,
    /// The element's inline value when the panel opened.
    pub inline: Option<String>,
    /// The user's edit: Some(Some(v)) set, Some(None) removed, None untouched.
    pub edit: Option<Option<String>>,
    /// The value when the panel opened (inline first, else computed): the "from" of the edits line.
    pub opened: String,
}

impl Row {
    pub fn badge(&self) -> Badge {
        match (&self.edit, &self.inline) {
            (Some(_), _) => Badge::Edited,
            (None, Some(_)) => Badge::Inline,
            (None, None) => Badge::Computed,
        }
    }

    /// The value the panel shows.
    pub fn shown(&self) -> &str {
        match &self.edit {
            Some(Some(v)) => v,
            _ => &self.computed,
        }
    }
}

/// What the panel asks the host to do (the view turns these into protocol messages).
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Set { prop: String, value: Option<String> },
    Revert,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StylePanel {
    /// The picked element (pick's selector).
    pub node: String,
    /// The `style` attribute when the panel opened, restored by Revert.
    pub original_style: Option<String>,
    pub rows: Vec<Row>,
}

impl StylePanel {
    /// Opens on the host's answer: `(prop, computed, inline)` for the requested props.
    pub fn open(node: &str, original_style: Option<String>, values: Vec<(String, String, Option<String>)>) -> Self {
        let mut rows: Vec<Row> = values.into_iter().map(|(prop, computed, inline)| Row { opened: inline.clone().unwrap_or_else(|| computed.clone()), prop, computed, inline, edit: None }).collect();
        rows.sort_by_key(|r| PROPS.iter().position(|p| *p == r.prop).unwrap_or(PROPS.len()));
        StylePanel { node: node.to_owned(), original_style, rows }
    }

    /// The user typed a value (empty removes it). Returns the request, or None for a no-op.
    pub fn set(&mut self, prop: &str, value: &str) -> Option<Request> {
        let row = self.rows.iter_mut().find(|r| r.prop == prop)?;
        let value = value.trim();
        let new = (!value.is_empty()).then(|| value.to_owned());
        let before = row.edit.clone().unwrap_or_else(|| Some(row.computed.clone()));
        if before == new {
            return None;
        }
        // Back to where it started: the edit is gone.
        row.edit = if new.as_deref() == Some(row.opened.as_str()) && row.edit.is_some() { None } else { Some(new.clone()) };
        Some(Request::Set { prop: prop.to_owned(), value: new })
    }

    /// The host re-read computed values after an edit (e.g. `padding: 1em` became `16px`).
    pub fn refresh(&mut self, prop: &str, computed: &str) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.prop == prop) {
            r.computed = computed.to_owned();
        }
    }

    pub fn is_edited(&self) -> bool {
        self.rows.iter().any(|r| r.edit.is_some())
    }

    /// Drops every edit; the host restores `original_style` exactly.
    pub fn revert(&mut self) -> Request {
        for r in &mut self.rows {
            r.edit = None;
        }
        Request::Revert
    }

    /// The edits for the S11 line: from the value when the panel opened (inline first, else computed).
    pub fn edits(&self) -> Vec<StyleEdit> {
        self.rows
            .iter()
            .filter_map(|r| {
                let to = r.edit.clone()?;
                Some(StyleEdit { prop: r.prop.clone(), from: Some(r.opened.clone()), to })
            })
            .collect()
    }
}

/// What the panel shows while it waits or when the element is gone.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading,
    /// The host had no such element (navigated, removed, or refused).
    Missing,
    Ready,
}

/// The style panel for one picked element: asks the host for its rows, edits one property per
/// Enter, reverts everything. The view places it beside the page and routes `StylesResult` here.
pub struct StylePanelView {
    id: BrowserId,
    link: Arc<dyn HostLink>,
    pub node: String,
    pub status: Status,
    pub panel: StylePanel,
    inputs: Vec<(String, Entity<InputState>)>,
    next_req: u64,
    /// The request that opened the panel; later answers are edits.
    open_req: u64,
    _subs: Vec<gpui::Subscription>,
}

impl StylePanelView {
    /// Opens for `node` and asks the host for the v1 rows.
    pub fn new(id: BrowserId, link: Arc<dyn HostLink>, node: &str) -> Self {
        let mut v = StylePanelView { id, link, node: node.to_owned(), status: Status::Loading, panel: StylePanel::default(), inputs: Vec::new(), next_req: 1, open_req: 0, _subs: Vec::new() };
        v.open_req = v.req();
        v.link.send(AppToHost::Styles { id: v.id.clone(), req: v.open_req, node: v.node.clone(), props: PROPS.iter().map(|p| p.to_string()).collect() });
        v
    }

    fn req(&mut self) -> u64 {
        // Panel requests live in their own range so they never collide with the view's.
        let r = (1 << 40) + self.next_req;
        self.next_req += 1;
        r
    }

    /// Whether `req` is one of this panel's (the view routes by it).
    pub fn owns(&self, req: u64) -> bool {
        req >= 1 << 40 && req < (1 << 40) + self.next_req
    }

    /// `HostToApp::StylesResult` for this panel.
    pub fn on_result(&mut self, req: u64, ok: bool, rows: Vec<StyleRow>, window: &mut Window, cx: &mut Context<Self>) {
        if !ok {
            self.status = Status::Missing;
            cx.notify();
            return;
        }
        if req == self.open_req {
            self.panel = StylePanel::open(&self.node, None, rows.into_iter().map(|r| (r.prop, r.computed, r.inline)).collect());
            self.status = Status::Ready;
            self.inputs.clear();
            self._subs.clear();
            for row in self.panel.rows.clone() {
                let input = cx.new(|cx| InputState::new(window, cx));
                input.update(cx, |i, cx| i.set_value(row.shown().to_owned(), window, cx));
                let prop = row.prop.clone();
                self._subs.push(cx.subscribe_in(&input, window, move |v, input, ev: &InputEvent, _, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        let value = input.read(cx).value().to_string();
                        v.set(&prop, &value, cx);
                    }
                }));
                self.inputs.push((row.prop, input));
            }
        } else {
            for r in rows {
                self.panel.refresh(&r.prop, &r.computed);
            }
        }
        cx.notify();
    }

    /// Sends one edit (what Enter in a row does).
    pub fn set(&mut self, prop: &str, value: &str, cx: &mut Context<Self>) {
        if let Some(Request::Set { prop, value }) = self.panel.set(prop, value) {
            let req = self.req();
            self.link.send(AppToHost::SetStyle { id: self.id.clone(), req, node: self.node.clone(), prop, value });
            cx.notify();
        }
    }

    pub fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel.revert();
        let req = self.req();
        self.link.send(AppToHost::RevertStyle { id: self.id.clone(), req, node: self.node.clone() });
        for (prop, input) in &self.inputs {
            let shown = self.panel.rows.iter().find(|r| &r.prop == prop).map(|r| r.opened.clone()).unwrap_or_default();
            input.update(cx, |i, cx| i.set_value(shown, window, cx));
        }
        cx.notify();
    }
}

impl Render for StylePanelView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let head = div().flex().justify_between().px_2().py_1().child(SharedString::from(format!("Styles · {}", self.node))).when(self.panel.is_edited(), |d| {
            d.child(div().id("styles-revert").px_1().cursor_pointer().child("Revert").on_click(cx.listener(|v, _, w, cx| v.revert(w, cx))))
        });
        let body = match self.status {
            Status::Loading => div().px_2().child("Reading styles…").into_any_element(),
            Status::Missing => div().px_2().child("This element is no longer on the page.").into_any_element(),
            Status::Ready => div()
                .flex()
                .flex_col()
                .children(self.inputs.iter().map(|(prop, input)| {
                    let badge = self.panel.rows.iter().find(|r| &r.prop == prop).map_or(Badge::Computed, Row::badge);
                    let (label, color) = match badge {
                        Badge::Edited => ("edited", 0xff3b30),
                        Badge::Inline => ("inline", 0x4c9aff),
                        Badge::Computed => ("computed", 0x8a8a8a),
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(div().w(px(120.)).child(SharedString::from(prop.clone())))
                        .child(div().flex_1().child(Input::new(input)))
                        .child(div().w(px(64.)).text_color(rgb(color)).child(label))
                }))
                .into_any_element(),
        };
        div().flex().flex_col().w(px(360.)).bg(rgb(0x1e1e1e)).text_color(rgb(0xe6e6e6)).child(head).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::edits::css_edits_line;
    use gpui::TestAppContext;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<AppToHost>>);
    impl HostLink for Recording {
        fn send(&self, msg: AppToHost) {
            self.0.lock().unwrap().push(msg);
        }
    }

    fn row(prop: &str, computed: &str, inline: Option<&str>) -> StyleRow {
        StyleRow { prop: prop.into(), computed: computed.into(), inline: inline.map(Into::into), matched: None }
    }

    /// The panel against a host that answers like the protocol says: asks for the ten rows,
    /// shows them, sends SetStyle on Enter, refreshes the computed value, reverts.
    #[gpui::test]
    fn the_panel_reads_edits_and_reverts_through_the_host(cx: &mut TestAppContext) {
        cx.update(gpui_base::init);
        let rec = Arc::new(Recording::default());
        let link: Arc<dyn HostLink> = rec.clone();
        let (view, cx) = cx.add_window_view(|_, _| StylePanelView::new("b1".into(), link, "#cta"));
        let sent = rec.0.lock().unwrap().drain(..).collect::<Vec<_>>();
        let AppToHost::Styles { req, node, props, .. } = &sent[0] else { panic!("{sent:?}") };
        assert_eq!((node.as_str(), props.len()), ("#cta", 10));
        let open = *req;
        cx.update(|w, cx| view.update(cx, |v, cx| v.on_result(open, true, vec![row("padding", "8px", None), row("color", "rgb(255, 0, 0)", Some("red"))], w, cx)));
        assert_eq!(view.read_with(cx, |v, _| (v.status.clone(), v.inputs.len())), (Status::Ready, 2));
        // Enter in the padding row: SetStyle goes to the host; its answer refreshes computed.
        let input = view.read_with(cx, |v, _| v.inputs.iter().find(|(p, _)| p == "padding").unwrap().1.clone());
        cx.update(|w, cx| {
            input.update(cx, |i, cx| i.set_value("16px", w, cx));
            view.update(cx, |v, cx| v.set("padding", "16px", cx));
        });
        let sent = rec.0.lock().unwrap().drain(..).collect::<Vec<_>>();
        let AppToHost::SetStyle { req, node, prop, value, .. } = &sent[0] else { panic!("{sent:?}") };
        assert_eq!((node.as_str(), prop.as_str(), value.as_deref()), ("#cta", "padding", Some("16px")));
        let set_req = *req;
        assert!(view.read_with(cx, |v, _| v.owns(set_req) && v.owns(open) && !v.owns(5)));
        cx.update(|w, cx| view.update(cx, |v, cx| v.on_result(set_req, true, vec![row("padding", "16px", Some("16px"))], w, cx)));
        assert_eq!(view.read_with(cx, |v, _| css_edits_line(&v.panel.edits())), Some("CSS edits — padding: 8px → 16px".into()));
        cx.update(|w, cx| view.update(cx, |v, cx| v.revert(w, cx)));
        let sent = rec.0.lock().unwrap().drain(..).collect::<Vec<_>>();
        assert!(matches!(&sent[..], [AppToHost::RevertStyle { node, .. }] if node == "#cta"), "{sent:?}");
        assert!(!view.read_with(cx, |v, _| v.panel.is_edited()));
        assert_eq!(cx.update(|_, cx| input.read(cx).value().to_string()), "8px", "the row shows its value from before the edit");
    }

    /// The stub host answers ok:false (no host side yet): the panel says the element is gone.
    #[gpui::test]
    fn a_refused_or_missing_element_shows_missing(cx: &mut TestAppContext) {
        cx.update(gpui_base::init);
        let link: Arc<dyn HostLink> = Arc::new(Recording::default());
        let (view, cx) = cx.add_window_view(|_, _| StylePanelView::new("b1".into(), link, "#gone"));
        cx.update(|w, cx| view.update(cx, |v, cx| v.on_result(1 << 40 | 1, false, vec![], w, cx)));
        assert_eq!(view.read_with(cx, |v, _| v.status.clone()), Status::Missing);
    }

    fn panel() -> StylePanel {
        StylePanel::open(
            "#cta",
            Some("color: red".into()),
            vec![
                ("padding".into(), "8px".into(), None),
                ("color".into(), "rgb(255, 0, 0)".into(), Some("red".into())),
                ("font-size".into(), "14px".into(), None),
            ],
        )
    }

    #[test]
    fn rows_follow_the_catalogue_and_badge_their_source() {
        let p = panel();
        assert_eq!(p.rows.iter().map(|r| r.prop.as_str()).collect::<Vec<_>>(), ["font-size", "color", "padding"]);
        assert_eq!(p.rows.iter().map(Row::badge).collect::<Vec<_>>(), [Badge::Computed, Badge::Inline, Badge::Computed]);
    }

    #[test]
    fn edits_requests_revert_and_the_edits_line() {
        let mut p = panel();
        assert_eq!(p.set("padding", "16px"), Some(Request::Set { prop: "padding".into(), value: Some("16px".into()) }));
        assert_eq!(p.set("padding", "16px"), None, "same value again is a no-op");
        assert_eq!(p.set("color", ""), Some(Request::Set { prop: "color".into(), value: None }), "empty removes");
        assert_eq!(p.set("nope", "1px"), None, "unknown props are not sent");
        assert_eq!(p.rows.iter().map(Row::badge).collect::<Vec<_>>(), [Badge::Computed, Badge::Edited, Badge::Edited]);
        assert_eq!(p.rows[2].shown(), "16px");
        assert_eq!(css_edits_line(&p.edits()).unwrap(), "CSS edits — color: removed (was red); padding: 8px → 16px");
        // Setting it back to where it started drops the edit.
        p.set("padding", "8px");
        assert_eq!(p.rows[2].badge(), Badge::Computed);
        assert_eq!(p.revert(), Request::Revert);
        assert!(!p.is_edited() && p.edits().is_empty());
        assert_eq!(p.original_style.as_deref(), Some("color: red"), "revert restores this exact attribute");
        p.refresh("font-size", "16px");
        assert_eq!(p.rows[0].shown(), "16px");
    }
}
