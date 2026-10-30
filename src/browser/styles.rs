//! Design slice S10 v1 (part 4), the panel model: ten properties of the picked element, each with
//! its computed value, its own inline value and the user's edit; badges say which is which;
//! Revert puts back the original `style` attribute exactly. The host reads and writes through
//! typed messages (Styles, SetStyle, RevertStyle; protocol pending with the lead); this model only
//! decides what to ask and what to show. Values are CSS only, never input values or text.

use super::edits::StyleEdit;

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
        let mut rows: Vec<Row> = values.into_iter().map(|(prop, computed, inline)| Row { prop, computed, inline, edit: None }).collect();
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
        row.edit = if new.as_deref() == Some(row.computed.as_str()) && row.edit.is_some() { None } else { Some(new.clone()) };
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
                Some(StyleEdit { prop: r.prop.clone(), from: Some(r.inline.clone().unwrap_or_else(|| r.computed.clone())), to })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::edits::css_edits_line;

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
