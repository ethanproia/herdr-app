//! Design slice S7 (part 4): the annotation queue. Numbered pins on picked targets, each with a
//! comment, kept per page; the queue survives navigation (pins hide on other pages, the count
//! badge keeps counting) and goes to the agent as one batch on Send.
//!
//! The target comes from pick (S4 payload, fork-agent); the comment text from the comment field
//! that `BrowserInput` owns while its owner is `Owner::Comment`. The agent payload is S8's.

use serde::{Deserialize, Serialize};

/// What an annotation points at, as pick reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// A CSS selector that resolves to exactly the picked element.
    pub selector: String,
    /// What the pin's chip shows (tag, component name or short text).
    pub label: String,
    /// The element's rect in page DIPs when it was picked; S15 rebinding moves it.
    pub rect: (f32, f32, f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Kept for the next Send.
    Pending,
    /// Handed to the agent; no longer shown or counted.
    Sent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// The pin number, 1-based, in the order pending annotations were made.
    pub n: u32,
    /// The page it belongs to (without the #fragment).
    pub page: String,
    pub target: Target,
    pub comment: String,
    pub state: State,
}

/// The queue for one browser block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Annotations {
    list: Vec<Annotation>,
    page: String,
}

/// Pages are compared without their fragment, so in-page anchors keep the pins.
pub fn page_key(url: &str) -> String {
    url.split('#').next().unwrap_or(url).to_owned()
}

impl Annotations {
    /// The block navigated: pins of other pages hide, this page's pins come back.
    pub fn set_page(&mut self, url: &str) {
        self.page = page_key(url);
    }

    /// Adds an annotation on the current page and returns its number.
    pub fn add(&mut self, target: Target, comment: &str) -> u32 {
        let n = self.count() as u32 + 1;
        self.list.push(Annotation { n, page: self.page.clone(), target, comment: comment.trim().to_owned(), state: State::Pending });
        n
    }

    pub fn set_comment(&mut self, n: u32, comment: &str) -> bool {
        self.pending_mut(n).map(|a| a.comment = comment.trim().to_owned()).is_some()
    }

    /// Removes pin `n`; later pins move up a number so the list has no gaps.
    pub fn remove(&mut self, n: u32) -> bool {
        let Some(i) = self.list.iter().position(|a| a.state == State::Pending && a.n == n) else { return false };
        self.list.remove(i);
        self.renumber();
        true
    }

    fn renumber(&mut self) {
        for (i, a) in self.list.iter_mut().filter(|a| a.state == State::Pending).enumerate() {
            a.n = i as u32 + 1;
        }
    }

    fn pending_mut(&mut self, n: u32) -> Option<&mut Annotation> {
        self.list.iter_mut().find(|a| a.state == State::Pending && a.n == n)
    }

    /// The count badge: every pending annotation, on any page.
    pub fn count(&self) -> usize {
        self.list.iter().filter(|a| a.state == State::Pending).count()
    }

    /// The pins to draw on the current page.
    pub fn visible(&self) -> impl Iterator<Item = &Annotation> {
        self.list.iter().filter(|a| a.state == State::Pending && a.page == self.page)
    }

    /// Every pending annotation, in pin order (the list panel).
    pub fn pending(&self) -> impl Iterator<Item = &Annotation> {
        self.list.iter().filter(|a| a.state == State::Pending)
    }

    /// Send: the pending batch (for S8 to format), marked sent. Empty comments are sent too:
    /// a pin alone says "this one".
    pub fn send(&mut self) -> Vec<Annotation> {
        let batch: Vec<Annotation> = self.pending().cloned().collect();
        for a in self.list.iter_mut() {
            a.state = State::Sent;
        }
        batch
    }

    /// Clear without sending.
    pub fn discard(&mut self) {
        self.list.retain(|a| a.state != State::Pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(sel: &str) -> Target {
        Target { selector: sel.into(), label: sel.into(), rect: (10., 20., 100., 40.) }
    }

    #[test]
    fn pins_number_in_order_and_survive_navigation() {
        let mut q = Annotations::default();
        q.set_page("https://a.test/page#top");
        assert_eq!(q.add(target("#one"), "  bigger  "), 1);
        assert_eq!(q.add(target("#two"), "red"), 2);
        assert_eq!(q.add(target("#three"), ""), 3);
        assert_eq!(q.visible().map(|a| a.n).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(q.visible().next().unwrap().comment, "bigger", "comments are trimmed");
        // Another page: the list is hidden there, the badge still counts 3.
        q.set_page("https://b.test/");
        assert_eq!((q.visible().count(), q.count()), (0, 3));
        assert_eq!(q.add(target("#b"), "on b"), 4, "numbering continues across pages");
        // Back: the pins return; an in-page anchor is the same page.
        q.set_page("https://a.test/page#section-2");
        assert_eq!(q.visible().map(|a| a.target.selector.as_str()).collect::<Vec<_>>(), ["#one", "#two", "#three"]);
        assert_eq!(q.count(), 4);
    }

    #[test]
    fn edit_remove_renumber_send_and_discard() {
        let mut q = Annotations::default();
        q.set_page("https://a.test/");
        for s in ["#1", "#2", "#3"] {
            q.add(target(s), s);
        }
        assert!(q.set_comment(2, "changed"));
        assert!(q.remove(1));
        assert_eq!(q.pending().map(|a| (a.n, a.comment.as_str())).collect::<Vec<_>>(), [(1, "changed"), (2, "#3")], "no gaps after a removal");
        assert!(!q.remove(9));
        let batch = q.send();
        assert_eq!(batch.iter().map(|a| a.n).collect::<Vec<_>>(), [1, 2]);
        assert_eq!((q.count(), q.visible().count()), (0, 0), "sent pins are gone from view and badge");
        assert!(!q.set_comment(1, "late"), "a sent annotation is not editable");
        assert_eq!(q.add(target("#4"), ""), 1, "a new batch starts at 1");
        q.discard();
        assert_eq!(q.count(), 0);
        assert!(q.send().is_empty());
    }

    #[test]
    fn the_queue_round_trips_as_json() {
        let mut q = Annotations::default();
        q.set_page("https://a.test/");
        q.add(target("#x"), "note");
        let back: Annotations = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back, q);
    }
}
