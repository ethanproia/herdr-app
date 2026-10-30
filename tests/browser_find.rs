//! The find bar (I9): cmd-F opens it, typing searches, Enter / shift-Enter go next / previous,
//! "n of m" follows the host's FindResult, Esc closes it and clears the find. Stub part checks the
//! messages; the CEF part (when HERDR_BROWSER_CEF_HOST is set) counts real matches.
include!("common/browser_harness.rs");

use browser::protocol::HostToApp;
use harness::{Host, Page};
use std::time::Duration;

fn type_find(p: &mut Page, text: &str) {
    // The input's own editing is gpui-base's (tested there): set its text as typing would.
    let input = p.cx.update(|cx| p.view.read(cx).find_input()).expect("find bar open");
    let window = p.window_handle();
    let t = text.to_owned();
    p.cx.update_window(window, |_, window, cx| input.update(cx, |i, cx| {
        i.set_value(t.clone(), window, cx);
        cx.emit(gpui_base::input::InputEvent::Change);
    })).unwrap();
    p.pump();
    let v = p.cx.update(|cx| input.read(cx).value().to_string());
    assert_eq!(v, text, "the find input holds what was typed");
}

fn saw(p: &mut Page, what: &str, f: impl Fn(&str) -> bool) {
    assert!(p.wait(Duration::from_secs(5), |p| p.console().iter().any(|l| f(l))), "host never saw {what}: {:?}", p.console());
}

fn main() {
    let server = fixtures::Server::start();
    if let Some(mut p) = Page::open(Host::Stub, &server.url("select.html")) {
        p.focus();
        p.key("cmd-f");
        assert!(p.cx.update(|cx| p.view.read(cx).find_state().is_some()), "cmd-F opens the bar");
        assert_eq!(p.page_top(), 56.0, "the page moves under the find bar");
        type_find(&mut p, "fox");
        saw(&mut p, "Find fox", |l| l.contains(r#""type":"find""#) && l.contains(r#""text":"fox""#) && l.contains(r#""find_next":false"#));
        // The host's count shows as "n of m".
        let v = p.view.clone();
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::FindResult { id: "page".into(), count: 3, active: 1, final_update: true }, cx)));
        assert_eq!(p.cx.update(|cx| p.view.read(cx).find_state()), Some(("fox".into(), 1, 3)));
        p.key("enter");
        saw(&mut p, "Find next", |l| l.contains(r#""type":"find""#) && l.contains(r#""find_next":true"#) && l.contains(r#""forward":true"#));
        p.key("shift-enter");
        saw(&mut p, "Find previous", |l| l.contains(r#""type":"find""#) && l.contains(r#""forward":false"#));
        p.key("escape");
        assert!(p.cx.update(|cx| p.view.read(cx).find_state().is_none()), "Esc closes the bar");
        saw(&mut p, "StopFind", |l| l.contains(r#""type":"stop_find""#));
        assert!(!p.console().iter().any(|l| l.contains(r#""key":"Escape""#)), "the page never saw that Esc");
        assert_eq!(p.page_top(), 28.0);
        println!("find passed (stub): cmd-F, typing, n of m, Enter / shift-Enter, Esc");
    }
    let Some(mut p) = Page::open(Host::Cef, &server.url("select.html")) else { return };
    p.focus();
    p.key("cmd-f");
    type_find(&mut p, "o");
    let ok = p.wait(Duration::from_secs(10), |p| p.cx.update(|cx| p.view.read(cx).find_state()).is_some_and(|(_, _, c)| c > 0));
    let st = p.cx.update(|cx| p.view.read(cx).find_state());
    let expect = p.eval("(document.body.innerText.match(/o/gi)||[]).length").unwrap();
    assert!(ok, "real matches counted: {st:?}");
    println!("find (CEF): first answer {st:?}; the page's text has {expect} o's");
    // Enter steps through the matches: the active one moves.
    let first = st.map_or(0, |s| s.1);
    p.key("enter");
    let a1 = p.wait(Duration::from_secs(5), |p| p.cx.update(|cx| p.view.read(cx).find_state()).is_some_and(|(_, a, _)| a != first && a > 0));
    let s1 = p.cx.update(|cx| p.view.read(cx).find_state());
    p.key("enter");
    let s1a = s1.as_ref().map_or(0, |s| s.1);
    let a2 = p.wait(Duration::from_secs(5), |p| p.cx.update(|cx| p.view.read(cx).find_state()).is_some_and(|(_, a, _)| a != s1a));
    let s2 = p.cx.update(|cx| p.view.read(cx).find_state());
    assert!(a1 && a2, "Enter moves the active match: {first} -> {s1:?} -> {s2:?}");
    println!("find passed (CEF): active {first} -> {:?} -> {:?}", s1.map(|s| s.1), s2.map(|s| (s.1, s.2)));
}
