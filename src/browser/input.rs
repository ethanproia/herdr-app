//! Part 4 (LAB-422): GPUI mouse, wheel, key and IME events to protocol `InputEvent`s,
//! sent through `HostLink`. Coordinates are DIPs from the block's top-left.
//!
//! `BrowserView` (part 1) owns one `Entity<BrowserInput>` per block and wires it in three places:
//! - render: `input::listen(div, &input)` adds the mouse, wheel and key listeners;
//! - paint: `input::paint(&input, &focus, bounds, window, cx)` records the block origin and registers
//!   the entity as the window's text input handler (IME), the same way the terminal does;
//! - host events: `set_focused` on focus change, `on_caret` for `ImeCaret`, `cursor_style` for `Cursor`.
//!
//! Herdr keys still win: GPUI matches keybindings before any `on_key_down` listener runs, so a key
//! bound in keys.json (global, layout or a future `browser` section) is never seen here.
//! Printable keys send `Key` down and let the event continue, so the platform's text input
//! (layouts, dead keys, IME) delivers the characters as `Text` through `replace_text_in_range`.

use super::protocol::{self, AppToHost, BrowserId, EditCommand, ImeEvent, InputEvent, KeyKind, MouseKind};
use super::HostLink;
use gpui::{
    Bounds, Context, CursorStyle, Entity, EntityInputHandler, FocusHandle, KeyDownEvent, KeyUpEvent,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    Pixels, Point, ScrollDelta, ScrollWheelEvent, StatefulInteractiveElement, UTF16Selection, Window, point, px,
};
use std::ops::Range;
use std::sync::Arc;

/// One browser block's input state: what is held, where the block is, the IME composition.
pub struct BrowserInput {
    id: BrowserId,
    link: Arc<dyn HostLink>,
    /// The block's top-left in window coordinates, updated every paint.
    origin: Point<Pixels>,
    focused: bool,
    caps_lock: bool,
    /// Keys sent down and not yet up, by DOM code, so blur can release them.
    held_keys: Vec<(String, String, u32)>,
    held_buttons: Vec<protocol::MouseButton>,
    last_pos: (f32, f32),
    /// The uncommitted IME text, if a composition is open.
    marked: Option<String>,
    /// The page's caret from `ImeCaret`, in DIPs from the block's top-left.
    caret: Option<Bounds<Pixels>>,
}

impl BrowserInput {
    pub fn new(id: BrowserId, link: Arc<dyn HostLink>) -> Self {
        Self { id, link, origin: Point::default(), focused: false, caps_lock: false, held_keys: Vec::new(), held_buttons: Vec::new(), last_pos: (0.0, 0.0), marked: None, caret: None }
    }

    fn send(&self, event: InputEvent) {
        self.link.send(AppToHost::Input { id: self.id.clone(), event });
    }

    fn local(&self, p: Point<Pixels>) -> (f32, f32) {
        (f32::from(p.x - self.origin.x), f32::from(p.y - self.origin.y))
    }

    fn mods(&self, m: &gpui::Modifiers) -> protocol::Modifiers {
        protocol::Modifiers { shift: m.shift, control: m.control, alt: m.alt, platform: m.platform, caps_lock: self.caps_lock }
    }

    pub fn set_origin(&mut self, origin: Point<Pixels>) {
        self.origin = origin;
    }

    /// Focus moved to or away from the block. Losing it releases every held key and button
    /// and cancels an open composition, so the page never sees a stuck key.
    pub fn set_focused(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        if !focused {
            self.release_all();
        }
        self.link.send(AppToHost::Focus { id: self.id.clone(), focused });
    }

    fn release_all(&mut self) {
        let mods = protocol::Modifiers { caps_lock: self.caps_lock, ..Default::default() };
        for (key, code, native) in std::mem::take(&mut self.held_keys) {
            self.send(InputEvent::Key { kind: KeyKind::Up, key, code, native, repeat: false, mods });
        }
        let (x, y) = self.last_pos;
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEvent::Mouse { kind: MouseKind::Up, x, y, button, clicks: 1, mods });
        }
        if self.marked.take().is_some() {
            self.send(InputEvent::Ime(ImeEvent::Cancel));
        }
    }

    /// `HostToApp::ImeCaret`: where the candidate window goes.
    pub fn on_caret(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.caret = Some(Bounds::new(point(px(x), px(y)), gpui::size(px(w.max(1.0)), px(h))));
    }

    pub fn mouse_down(&mut self, ev: &MouseDownEvent) {
        let (x, y) = self.local(ev.position);
        self.last_pos = (x, y);
        if let MouseButton::Navigate(dir) = ev.button {
            let id = self.id.clone();
            self.link.send(match dir {
                NavigationDirection::Back => AppToHost::Back { id },
                NavigationDirection::Forward => AppToHost::Forward { id },
            });
            return;
        }
        let button = button(ev.button);
        if !self.held_buttons.contains(&button) {
            self.held_buttons.push(button);
        }
        self.send(InputEvent::Mouse { kind: MouseKind::Down, x, y, button, clicks: clicks(ev.click_count), mods: self.mods(&ev.modifiers) });
    }

    pub fn mouse_up(&mut self, ev: &MouseUpEvent) {
        if matches!(ev.button, MouseButton::Navigate(_)) {
            return;
        }
        let (x, y) = self.local(ev.position);
        self.last_pos = (x, y);
        let button = button(ev.button);
        self.held_buttons.retain(|b| *b != button);
        self.send(InputEvent::Mouse { kind: MouseKind::Up, x, y, button, clicks: clicks(ev.click_count), mods: self.mods(&ev.modifiers) });
    }

    /// Moves, and drags when a button is held (the page sees the button, as Chromium expects).
    pub fn mouse_move(&mut self, ev: &MouseMoveEvent) {
        let (x, y) = self.local(ev.position);
        self.last_pos = (x, y);
        let button = ev.pressed_button.filter(|b| !matches!(b, MouseButton::Navigate(_))).map_or(protocol::MouseButton::None, button);
        self.send(InputEvent::Mouse { kind: MouseKind::Move, x, y, button, clicks: 0, mods: self.mods(&ev.modifiers) });
    }

    pub fn mouse_exit(&mut self, ev: &MouseExitEvent) {
        let (x, y) = self.local(ev.position);
        self.send(InputEvent::Mouse { kind: MouseKind::Leave, x, y, button: protocol::MouseButton::None, clicks: 0, mods: self.mods(&ev.modifiers) });
    }

    /// Trackpads give pixels (`precise`), wheels give lines; the host scales lines to its step.
    pub fn scroll(&mut self, ev: &ScrollWheelEvent) {
        let (x, y) = self.local(ev.position);
        let (dx, dy, precise) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y), true),
            ScrollDelta::Lines(p) => (p.x, p.y, false),
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.send(InputEvent::Wheel { x, y, dx, dy, precise, mods: self.mods(&ev.modifiers) });
    }

    pub fn modifiers_changed(&mut self, ev: &ModifiersChangedEvent) {
        self.caps_lock = ev.capslock.on;
    }

    /// Returns true when the key is fully handled here (stop propagation). Printable keys return
    /// false so the platform text input sends their characters to `replace_text_in_range`.
    pub fn key_down(&mut self, ev: &KeyDownEvent) -> bool {
        let k = &ev.keystroke;
        // While composing, the IME owns the keys; GPUI hands its results to the input handler.
        if self.marked.is_some() {
            return false;
        }
        // Clipboard and edit chords become explicit commands: CEF off-screen does not act on them as keys.
        if let Some(command) = edit_command(k) {
            self.link.send(AppToHost::Edit { id: self.id.clone(), command });
            return true;
        }
        let (key, code, native) = dom_key(k);
        if !self.held_keys.iter().any(|(_, c, _)| *c == code) {
            self.held_keys.push((key.clone(), code.clone(), native));
        }
        self.send(InputEvent::Key { kind: KeyKind::Down, key, code, native, repeat: ev.is_held, mods: self.mods(&k.modifiers) });
        !types_text(k)
    }

    pub fn key_up(&mut self, ev: &KeyUpEvent) {
        let k = &ev.keystroke;
        let (key, code, native) = dom_key(k);
        let was_held = self.held_keys.len();
        self.held_keys.retain(|(_, c, _)| *c != code);
        // A key-up for a key the page never saw down (pressed elsewhere, or eaten by a binding) stays here.
        if self.held_keys.len() != was_held {
            self.send(InputEvent::Key { kind: KeyKind::Up, key, code, native, repeat: false, mods: self.mods(&k.modifiers) });
        }
    }
}

/// Wire the mouse, wheel and key listeners onto the block's element (it needs an `.id`; part 1 calls this in render).
pub fn listen<E: StatefulInteractiveElement>(el: E, input: &Entity<BrowserInput>) -> E {
    let i = input.clone();
    let el = el.on_mouse_move(move |ev, _, cx| i.update(cx, |i, _| i.mouse_move(ev)));
    let i = input.clone();
    let el = el.on_hover(move |hovered, window, cx| {
        if !*hovered {
            let ev = MouseExitEvent { position: window.mouse_position(), pressed_button: None, modifiers: window.modifiers() };
            i.update(cx, |i, _| i.mouse_exit(&ev));
        }
    });
    let mut el = el;
    for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Navigate(NavigationDirection::Back), MouseButton::Navigate(NavigationDirection::Forward)] {
        let i = input.clone();
        el = el.on_mouse_down(b, move |ev, _, cx| {
            cx.stop_propagation();
            i.update(cx, |i, _| i.mouse_down(ev));
        });
        let i = input.clone();
        el = el.on_mouse_up(b, move |ev, _, cx| i.update(cx, |i, _| i.mouse_up(ev)));
    }
    let i = input.clone();
    let el = el.on_scroll_wheel(move |ev, _, cx| {
        cx.stop_propagation();
        i.update(cx, |i, _| i.scroll(ev));
    });
    let i = input.clone();
    let el = el.on_modifiers_changed(move |ev, _, cx| i.update(cx, |i, _| i.modifiers_changed(ev)));
    let i = input.clone();
    let el = el.on_key_down(move |ev, _, cx| {
        if i.update(cx, |i, _| i.key_down(ev)) {
            cx.stop_propagation();
        }
    });
    let i = input.clone();
    el.on_key_up(move |ev, _, cx| i.update(cx, |i, _| i.key_up(ev)))
}

/// Paint-time hook: record the block origin and, while focused, take the window's text input.
pub fn paint(input: &Entity<BrowserInput>, focus: &FocusHandle, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut gpui::App) {
    input.update(cx, |i, _| i.set_origin(bounds.origin));
    window.handle_input(focus, gpui::ElementInputHandler::new(bounds, input.clone()), cx);
}

/// `HostToApp::Cursor` (a CSS cursor name) to the pointer GPUI shows over the block.
pub fn cursor_style(name: &str) -> CursorStyle {
    match name {
        "pointer" | "hand" => CursorStyle::PointingHand,
        "text" => CursorStyle::IBeam,
        "vertical-text" => CursorStyle::IBeamCursorForVerticalLayout,
        "crosshair" | "cell" => CursorStyle::Crosshair,
        "grab" => CursorStyle::OpenHand,
        "grabbing" => CursorStyle::ClosedHand,
        "not-allowed" | "no-drop" => CursorStyle::OperationNotAllowed,
        "copy" => CursorStyle::DragCopy,
        "alias" => CursorStyle::DragLink,
        "context-menu" => CursorStyle::ContextualMenu,
        "col-resize" => CursorStyle::ResizeColumn,
        "row-resize" => CursorStyle::ResizeRow,
        "ew-resize" => CursorStyle::ResizeLeftRight,
        "ns-resize" => CursorStyle::ResizeUpDown,
        "e-resize" => CursorStyle::ResizeRight,
        "w-resize" => CursorStyle::ResizeLeft,
        "n-resize" => CursorStyle::ResizeUp,
        "s-resize" => CursorStyle::ResizeDown,
        "nwse-resize" | "nw-resize" | "se-resize" => CursorStyle::ResizeUpLeftDownRight,
        "nesw-resize" | "ne-resize" | "sw-resize" => CursorStyle::ResizeUpRightDownLeft,
        _ => CursorStyle::Arrow,
    }
}

/// The clipboard and edit chords a focused page should get (cmd on macOS, ctrl elsewhere).
/// The page gets them as `AppToHost::Edit`, not as `Key` events.
pub fn edit_command(k: &gpui::Keystroke) -> Option<EditCommand> {
    let m = k.modifiers;
    let primary = if cfg!(target_os = "macos") { m.platform && !m.control } else { m.control && !m.platform };
    if !primary || m.alt {
        return None;
    }
    Some(match (k.key.as_str(), m.shift) {
        ("c", false) => EditCommand::Copy,
        ("x", false) => EditCommand::Cut,
        ("v", false) => EditCommand::Paste,
        ("a", false) => EditCommand::SelectAll,
        ("z", false) => EditCommand::Undo,
        ("z", true) => EditCommand::Redo,
        _ => return None,
    })
}

fn button(b: MouseButton) -> protocol::MouseButton {
    match b {
        MouseButton::Left => protocol::MouseButton::Left,
        MouseButton::Right => protocol::MouseButton::Right,
        MouseButton::Middle => protocol::MouseButton::Middle,
        MouseButton::Navigate(_) => protocol::MouseButton::None,
    }
}

fn clicks(n: usize) -> u8 {
    n.clamp(1, u8::MAX as usize) as u8
}

/// A key that produces text (no ctrl or cmd); its characters come through the text input.
fn types_text(k: &gpui::Keystroke) -> bool {
    !k.modifiers.control && !k.modifiers.platform && k.key_char.as_deref().is_some_and(|c| !c.chars().any(char::is_control))
}

/// DOM `key`, DOM `code` (US layout) and the macOS virtual key code for a GPUI keystroke.
/// Known limitation: GPUI does not expose the native code, so it is derived from the key name
/// assuming a US layout; on other layouts `code` and `native` can name the wrong physical key.
fn dom_key(k: &gpui::Keystroke) -> (String, String, u32) {
    let name = k.key.as_str();
    let named = |key: &str, code: &str, vk: u32| (key.to_owned(), code.to_owned(), vk);
    match name {
        "enter" => named("Enter", "Enter", 0x24),
        "tab" => named("Tab", "Tab", 0x30),
        "space" => named(" ", "Space", 0x31),
        "backspace" => named("Backspace", "Backspace", 0x33),
        "escape" => named("Escape", "Escape", 0x35),
        "delete" => named("Delete", "Delete", 0x75),
        "left" => named("ArrowLeft", "ArrowLeft", 0x7B),
        "right" => named("ArrowRight", "ArrowRight", 0x7C),
        "down" => named("ArrowDown", "ArrowDown", 0x7D),
        "up" => named("ArrowUp", "ArrowUp", 0x7E),
        "home" => named("Home", "Home", 0x73),
        "end" => named("End", "End", 0x77),
        "pageup" => named("PageUp", "PageUp", 0x74),
        "pagedown" => named("PageDown", "PageDown", 0x79),
        "insert" => named("Insert", "Insert", 0x72),
        "shift" => named("Shift", "ShiftLeft", 0x38),
        "control" => named("Control", "ControlLeft", 0x3B),
        "alt" => named("Alt", "AltLeft", 0x3A),
        "platform" => named("Meta", "MetaLeft", 0x37),
        "function" => named("Fn", "Fn", 0x3F),
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=20).contains(&n)) => {
            const VK: [u32; 20] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A];
            let n: usize = f[1..].parse().unwrap();
            named(&f.to_uppercase(), &f.to_uppercase(), VK[n - 1])
        }
        _ => {
            // The page sees the character the key types (shift-1 is "!"); cmd and ctrl chords keep the base key.
            let key = k.key_char.clone().filter(|c| !c.chars().any(char::is_control)).unwrap_or_else(|| name.to_owned());
            let (code, vk) = char_code(name);
            (key, code, vk)
        }
    }
}

fn char_code(name: &str) -> (String, u32) {
    const LETTERS: [u32; 26] = [0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06];
    const DIGITS: [u32; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ 'a'..='z'), None) => (format!("Key{}", c.to_ascii_uppercase()), LETTERS[(c as u8 - b'a') as usize]),
        (Some(c @ '0'..='9'), None) => (format!("Digit{c}"), DIGITS[(c as u8 - b'0') as usize]),
        (Some(c), None) => {
            let (code, vk) = match c {
                '-' => ("Minus", 0x1B),
                '=' => ("Equal", 0x18),
                '[' => ("BracketLeft", 0x21),
                ']' => ("BracketRight", 0x1E),
                '\\' => ("Backslash", 0x2A),
                ';' => ("Semicolon", 0x29),
                '\'' => ("Quote", 0x27),
                ',' => ("Comma", 0x2B),
                '.' => ("Period", 0x2F),
                '/' => ("Slash", 0x2C),
                '`' => ("Backquote", 0x32),
                _ => ("", 0),
            };
            (code.to_owned(), vk)
        }
        _ => (String::new(), 0),
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The platform text input: typed characters become `Text`, compositions become `Ime` events.
/// The page's own text is not mirrored here, so ranges are relative to the marked text.
impl EntityInputHandler for BrowserInput {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
        let units: Vec<u16> = self.marked.as_deref().unwrap_or_default().encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&units[range]))
    }

    fn selected_text_range(&mut self, _ignore_disabled: bool, _w: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        let len = self.marked.as_deref().map_or(0, utf16_len);
        Some(UTF16Selection { range: len..len, reversed: false })
    }

    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_deref().filter(|m| !m.is_empty()).map(|m| 0..utf16_len(m))
    }

    /// The IME finished without inserting (focus change, escape): drop the composition.
    fn unmark_text(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            self.send(InputEvent::Ime(ImeEvent::Cancel));
        }
    }

    fn replace_text_in_range(&mut self, _range: Option<Range<usize>>, text: &str, _w: &mut Window, _cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            self.send(InputEvent::Ime(ImeEvent::Commit { text: text.to_owned() }));
        } else if !text.is_empty() {
            self.send(InputEvent::Text { text: text.to_owned() });
        }
    }

    fn replace_and_mark_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, selected: Option<Range<usize>>, _w: &mut Window, _cx: &mut Context<Self>) {
        if text.is_empty() {
            if self.marked.take().is_some() {
                self.send(InputEvent::Ime(ImeEvent::Cancel));
            }
            return;
        }
        let len = utf16_len(text) as u32;
        let (sel_start, sel_end) = selected.map_or((len, len), |r| ((r.start as u32).min(len), (r.end as u32).min(len)));
        self.marked = Some(text.to_owned());
        self.send(InputEvent::Ime(ImeEvent::Set {
            text: text.to_owned(),
            sel_start,
            sel_end,
            replace_start: range.as_ref().map(|r| r.start as u32),
            replace_end: range.as_ref().map(|r| r.end as u32),
        }));
    }

    /// The candidate window sits under the page's caret (window coordinates), or the block's corner until the host reports one.
    fn bounds_for_range(&mut self, _range: Range<usize>, element: Bounds<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let caret = self.caret.unwrap_or(Bounds::new(Point::default(), gpui::size(px(1.0), px(16.0))));
        Some(Bounds::new(element.origin + caret.origin, caret.size))
    }

    fn character_index_for_point(&mut self, _point: Point<Pixels>, _w: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers, Render, TestAppContext, actions, div, prelude::*};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<AppToHost>>);

    impl HostLink for Recording {
        fn send(&self, msg: AppToHost) {
            self.0.lock().unwrap().push(msg);
        }
    }

    impl Recording {
        fn take(&self) -> Vec<AppToHost> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
        fn inputs(&self) -> Vec<InputEvent> {
            self.take().into_iter().filter_map(|m| match m { AppToHost::Input { event, .. } => Some(event), _ => None }).collect()
        }
    }

    actions!(test_browser, [HerdrBinding]);

    /// A block 300x200 placed at (40, 30) in the window, focused, as BrowserView will lay it out.
    struct Block {
        input: Entity<BrowserInput>,
        focus: FocusHandle,
        bound: usize,
    }

    impl Render for Block {
        fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let (input, focus) = (self.input.clone(), self.focus.clone());
            div().size_full().child(
                listen(div().id("browser").key_context("Browser").track_focus(&self.focus).absolute().left(px(40.)).top(px(30.)).w(px(300.)).h(px(200.)), &self.input)
                    .on_action(cx.listener(|this, _: &HerdrBinding, _, _| this.bound += 1))
                    .child(gpui::canvas(|_, _, _| (), move |bounds, _, window, cx| paint(&input, &focus, bounds, window, cx)).size_full()),
            )
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Arc<Recording>, Entity<Block>, &mut gpui::VisualTestContext) {
        let rec = Arc::new(Recording::default());
        let link: Arc<dyn HostLink> = rec.clone();
        cx.update(|cx| cx.bind_keys([gpui::KeyBinding::new("cmd-t", HerdrBinding, None)]));
        let (block, vcx) = cx.add_window_view(|_, cx| Block { input: cx.new(|_| BrowserInput::new("b1".into(), link)), focus: cx.focus_handle(), bound: 0 });
        vcx.update(|window, cx| {
            let (input, focus) = block.read_with(cx, |b, _| (b.input.clone(), b.focus.clone()));
            window.focus(&focus, cx);
            input.update(cx, |i, _| i.set_focused(true));
        });
        vcx.run_until_parked();
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        rec.take();
        (rec, block, vcx)
    }

    fn mods() -> protocol::Modifiers {
        protocol::Modifiers::default()
    }

    #[gpui::test]
    fn mouse_clicks_buttons_and_drag_are_block_relative(cx: &mut TestAppContext) {
        let (rec, _block, cx) = setup(cx);
        let at = |x: f32, y: f32| point(px(x), px(y));
        cx.simulate_event(MouseDownEvent { button: MouseButton::Left, position: at(50., 40.), modifiers: Modifiers::default(), click_count: 3, first_mouse: false });
        cx.simulate_event(MouseMoveEvent { position: at(60., 45.), pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() });
        cx.simulate_event(MouseUpEvent { button: MouseButton::Left, position: at(60., 45.), modifiers: Modifiers::default(), click_count: 3 });
        cx.simulate_event(MouseDownEvent { button: MouseButton::Right, position: at(41., 31.), modifiers: Modifiers::command(), click_count: 1, first_mouse: false });
        cx.simulate_event(MouseDownEvent { button: MouseButton::Middle, position: at(41., 31.), modifiers: Modifiers::default(), click_count: 2, first_mouse: false });
        let cmd = protocol::Modifiers { platform: true, ..mods() };
        assert_eq!(rec.inputs(), vec![
            InputEvent::Mouse { kind: MouseKind::Down, x: 10., y: 10., button: protocol::MouseButton::Left, clicks: 3, mods: mods() },
            InputEvent::Mouse { kind: MouseKind::Move, x: 20., y: 15., button: protocol::MouseButton::Left, clicks: 0, mods: mods() },
            InputEvent::Mouse { kind: MouseKind::Up, x: 20., y: 15., button: protocol::MouseButton::Left, clicks: 3, mods: mods() },
            InputEvent::Mouse { kind: MouseKind::Down, x: 1., y: 1., button: protocol::MouseButton::Right, clicks: 1, mods: cmd },
            InputEvent::Mouse { kind: MouseKind::Down, x: 1., y: 1., button: protocol::MouseButton::Middle, clicks: 2, mods: mods() },
        ]);
    }

    #[gpui::test]
    fn back_and_forward_buttons_navigate(cx: &mut TestAppContext) {
        let (rec, _block, cx) = setup(cx);
        cx.simulate_event(MouseDownEvent { button: MouseButton::Navigate(NavigationDirection::Back), position: point(px(50.), px(40.)), modifiers: Modifiers::default(), click_count: 1, first_mouse: false });
        cx.simulate_event(MouseDownEvent { button: MouseButton::Navigate(NavigationDirection::Forward), position: point(px(50.), px(40.)), modifiers: Modifiers::default(), click_count: 1, first_mouse: false });
        assert_eq!(rec.take(), vec![AppToHost::Back { id: "b1".into() }, AppToHost::Forward { id: "b1".into() }]);
    }

    #[gpui::test]
    fn wheel_keeps_precise_pixels_and_coarse_lines_apart(cx: &mut TestAppContext) {
        let (rec, _block, cx) = setup(cx);
        let ev = |delta| ScrollWheelEvent { position: point(px(140.), px(130.)), delta, modifiers: Modifiers::default(), touch_phase: gpui::TouchPhase::Moved };
        cx.simulate_event(ev(ScrollDelta::Pixels(point(px(0.), px(-12.5)))));
        cx.simulate_event(ev(ScrollDelta::Lines(point(1., 0.))));
        assert_eq!(rec.inputs(), vec![
            InputEvent::Wheel { x: 100., y: 100., dx: 0., dy: -12.5, precise: true, mods: mods() },
            InputEvent::Wheel { x: 100., y: 100., dx: 1., dy: 0., precise: false, mods: mods() },
        ]);
    }

    #[gpui::test]
    fn printable_keys_send_key_then_text_and_herdr_bindings_are_not_swallowed(cx: &mut TestAppContext) {
        let (rec, block, cx) = setup(cx);
        let primary = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
        cx.simulate_keystrokes(&format!("a enter {primary}-c {primary}-v {primary}-a {primary}-shift-z cmd-t"));
        let edit = |command| AppToHost::Edit { id: "b1".into(), command };
        let input = |event| AppToHost::Input { id: "b1".into(), event };
        assert_eq!(rec.take(), vec![
            input(InputEvent::Key { kind: KeyKind::Down, key: "a".into(), code: "KeyA".into(), native: 0x00, repeat: false, mods: mods() }),
            input(InputEvent::Text { text: "a".into() }),
            input(InputEvent::Key { kind: KeyKind::Down, key: "Enter".into(), code: "Enter".into(), native: 0x24, repeat: false, mods: mods() }),
            edit(EditCommand::Copy),
            edit(EditCommand::Paste),
            edit(EditCommand::SelectAll),
            edit(EditCommand::Redo),
        ]);
        // cmd-t is a Herdr binding: it ran, and the page never saw it.
        assert_eq!(block.read_with(cx, |b, _| b.bound), 1);
        // The page sees the character a shifted key types, with the physical code.
        let bang = Keystroke { modifiers: Modifiers::shift(), key: "1".into(), key_char: Some("!".into()) };
        assert_eq!(dom_key(&bang), ("!".into(), "Digit1".into(), 0x12));
        assert_eq!(edit_command(&Keystroke::parse(if cfg!(target_os = "macos") { "cmd-c" } else { "ctrl-c" }).unwrap()), Some(EditCommand::Copy));
        assert_eq!(edit_command(&Keystroke::parse(if cfg!(target_os = "macos") { "cmd-shift-z" } else { "ctrl-shift-z" }).unwrap()), Some(EditCommand::Redo));
    }

    #[gpui::test]
    fn repeat_key_up_and_blur_release_held_keys_and_buttons(cx: &mut TestAppContext) {
        let (rec, block, cx) = setup(cx);
        let input = block.read_with(cx, |b, _| b.input.clone());
        let ks = Keystroke::parse("left").unwrap();
        cx.simulate_event(KeyDownEvent { keystroke: ks.clone(), is_held: false, prefer_character_input: false });
        cx.simulate_event(KeyDownEvent { keystroke: ks.clone(), is_held: true, prefer_character_input: false });
        cx.simulate_event(KeyUpEvent { keystroke: ks });
        cx.simulate_event(KeyDownEvent { keystroke: Keystroke::parse("shift").unwrap(), is_held: false, prefer_character_input: false });
        cx.simulate_event(MouseDownEvent { button: MouseButton::Left, position: point(px(45.), px(35.)), modifiers: Modifiers::default(), click_count: 1, first_mouse: false });
        let left = |kind, repeat| InputEvent::Key { kind, key: "ArrowLeft".into(), code: "ArrowLeft".into(), native: 0x7B, repeat, mods: mods() };
        assert_eq!(rec.inputs(), vec![
            left(KeyKind::Down, false),
            left(KeyKind::Down, true),
            left(KeyKind::Up, false),
            InputEvent::Key { kind: KeyKind::Down, key: "Shift".into(), code: "ShiftLeft".into(), native: 0x38, repeat: false, mods: mods() },
            InputEvent::Mouse { kind: MouseKind::Down, x: 5., y: 5., button: protocol::MouseButton::Left, clicks: 1, mods: mods() },
        ]);
        cx.update(|_, cx| input.update(cx, |i, _| i.set_focused(false)));
        assert_eq!(rec.take(), vec![
            AppToHost::Input { id: "b1".into(), event: InputEvent::Key { kind: KeyKind::Up, key: "Shift".into(), code: "ShiftLeft".into(), native: 0x38, repeat: false, mods: mods() } },
            AppToHost::Input { id: "b1".into(), event: InputEvent::Mouse { kind: MouseKind::Up, x: 5., y: 5., button: protocol::MouseButton::Left, clicks: 1, mods: mods() } },
            AppToHost::Focus { id: "b1".into(), focused: false },
        ]);
    }

    #[gpui::test]
    fn ime_composition_sets_commits_cancels_and_places_the_candidate_window(cx: &mut TestAppContext) {
        let (rec, block, cx) = setup(cx);
        let input = block.read_with(cx, |b, _| b.input.clone());
        cx.update(|window, cx| {
            input.update(cx, |i, cx| {
                i.replace_and_mark_text_in_range(None, "にほん", Some(1..2), window, cx);
                assert_eq!(i.marked_text_range(window, cx), Some(0..3));
                i.replace_text_in_range(None, "日本", window, cx);
                i.replace_and_mark_text_in_range(None, "か", None, window, cx);
                i.unmark_text(window, cx);
                i.on_caret(10., 20., 0., 18.);
                let element = Bounds::new(point(px(40.), px(30.)), gpui::size(px(300.), px(200.)));
                assert_eq!(i.bounds_for_range(0..0, element, window, cx), Some(Bounds::new(point(px(50.), px(50.)), gpui::size(px(1.), px(18.)))));
            })
        });
        assert_eq!(rec.inputs(), vec![
            InputEvent::Ime(ImeEvent::Set { text: "にほん".into(), sel_start: 1, sel_end: 2, replace_start: None, replace_end: None }),
            InputEvent::Ime(ImeEvent::Commit { text: "日本".into() }),
            InputEvent::Ime(ImeEvent::Set { text: "か".into(), sel_start: 1, sel_end: 1, replace_start: None, replace_end: None }),
            InputEvent::Ime(ImeEvent::Cancel),
        ]);
    }

    #[test]
    fn css_cursors_map_to_gpui_cursors() {
        assert_eq!(cursor_style("pointer"), CursorStyle::PointingHand);
        assert_eq!(cursor_style("text"), CursorStyle::IBeam);
        assert_eq!(cursor_style("grabbing"), CursorStyle::ClosedHand);
        assert_eq!(cursor_style("nwse-resize"), CursorStyle::ResizeUpLeftDownRight);
        assert_eq!(cursor_style("something-new"), CursorStyle::Arrow);
    }
}
