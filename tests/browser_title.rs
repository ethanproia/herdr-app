//! Title and favicon follow the page (I17): the block's state and a PageInfo event for the window
//! (which renames an unnamed tab; view::annotate_tab is unit-tested). Stub: events from injected
//! messages; CEF: a real <title>, a script retitle and a <link rel=icon>.
include!("common/browser_harness.rs");

use browser::protocol::HostToApp;
use browser::view::BrowserEvent;
use harness::{Host, Page};
use std::time::Duration;

fn infos(p: &mut Page) -> usize {
    p.events().iter().filter(|e| matches!(e, BrowserEvent::PageInfo)).count()
}

fn main() {
    let server = fixtures::Server::start();
    if let Some(mut p) = Page::open(Host::Stub, &server.url("title.html")) {
        let n = infos(&mut p);
        let v = p.view.clone();
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::Title { id: "page".into(), title: "Hello".into() }, cx)));
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::Favicon { id: "page".into(), url: "http://x.test/f.ico".into() }, cx)));
        p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(HostToApp::Title { id: "page".into(), title: "Hello".into() }, cx)));
        p.pump();
        let st = p.state();
        assert_eq!((st.title.as_str(), st.favicon.as_str()), ("Hello", "http://x.test/f.ico"));
        assert_eq!(infos(&mut p), n + 2, "one PageInfo per change, none for a repeat");
        println!("title passed (stub): title and favicon in state, PageInfo per change");
    }
    let Some(mut p) = Page::open(Host::Cef, &server.url("title.html")) else { return };
    assert!(p.wait(Duration::from_secs(10), |p| p.state().title == "first title"), "title from <title>: {:?}", p.state().title);
    p.eval("retitle('second title')").unwrap();
    assert!(p.wait(Duration::from_secs(5), |p| p.state().title == "second title"), "script retitle: {:?}", p.state().title);
    let fav = p.wait(Duration::from_secs(10), |p| p.state().favicon.ends_with("/fav.svg"));
    let st = p.state();
    assert!(fav, "favicon from <link rel=icon>: {:?}", st.favicon);
    assert!(infos(&mut p) >= 2);
    println!("title passed (CEF): '{}' then '{}', favicon {}", "first title", st.title, st.favicon);
}
