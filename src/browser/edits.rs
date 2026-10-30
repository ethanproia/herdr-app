//! Design slice S11 (part 4): the CSS edits line appended to an annotation's comment, in Cursor's
//! wording (`design-css.ts` `OBn`): `CSS edits — prop: from → to; prop2: removed (was X)`.

/// One property the style panel (S10) changed on the picked element.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleEdit {
    pub prop: String,
    /// The value before the edit; None when the property was not set.
    pub from: Option<String>,
    /// The value after; None when the edit removed it.
    pub to: Option<String>,
}

pub const PREFIX: &str = "CSS edits — ";

/// The one-line summary, or None when nothing really changed. Edits that end where they started
/// are dropped; a property with no earlier value reads `prop: unset → to` (Cursor's wording for
/// that case was not found; this is ours).
pub fn css_edits_line(edits: &[StyleEdit]) -> Option<String> {
    let parts: Vec<String> = edits
        .iter()
        .filter(|e| e.from != e.to)
        .map(|e| match (&e.from, &e.to) {
            (Some(from), None) => format!("{}: removed (was {from})", e.prop),
            (from, Some(to)) => format!("{}: {} → {to}", e.prop, from.as_deref().unwrap_or("unset")),
            (None, None) => unreachable!("filtered: from == to"),
        })
        .collect();
    (!parts.is_empty()).then(|| format!("{PREFIX}{}", parts.join("; ")))
}

/// The comment with the edits line appended on its own line (or the line alone).
pub fn with_edits(comment: &str, edits: &[StyleEdit]) -> String {
    match (comment.trim(), css_edits_line(edits)) {
        (c, None) => c.to_owned(),
        ("", Some(line)) => line,
        (c, Some(line)) => format!("{c}\n{line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(prop: &str, from: Option<&str>, to: Option<&str>) -> StyleEdit {
        StyleEdit { prop: prop.into(), from: from.map(Into::into), to: to.map(Into::into) }
    }

    #[test]
    fn golden_lines() {
        let edits = [e("padding", Some("8px"), Some("16px")), e("margin", Some("4px"), None), e("gap", None, Some("12px")), e("color", Some("red"), Some("red"))];
        assert_eq!(css_edits_line(&edits).unwrap(), "CSS edits — padding: 8px → 16px; margin: removed (was 4px); gap: unset → 12px");
        assert_eq!(css_edits_line(&[e("color", Some("red"), Some("red"))]), None, "no-op edits say nothing");
        assert_eq!(css_edits_line(&[]), None);
        assert_eq!(with_edits(" make it roomier ", &edits[..1]), "make it roomier\nCSS edits — padding: 8px → 16px");
        assert_eq!(with_edits("", &edits[1..2]), "CSS edits — margin: removed (was 4px)");
        assert_eq!(with_edits("just a note", &[]), "just a note");
    }
}
