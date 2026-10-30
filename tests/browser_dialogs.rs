//! The app side of JS dialogs, the file chooser and downloads (I13, I14), against the stub host:
//! a dialog draws over the page and takes the keyboard, Enter / Escape / OK answer it (a prompt
//! with its text), file picks go back filtered by `accept`, downloads become a toast and a list.
include!("common/browser_harness.rs");

use browser::protocol::{DialogKind, DownloadState, HostToApp};
use browser::view::BrowserEvent;
use harness::{Host, Page};
use std::time::Duration;

fn inject(p: &mut Page, m: HostToApp) {
    let v = p.view.clone();
    p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(m, cx)));
    p.pump();
}

fn saw(p: &mut Page, what: &str, f: impl Fn(&str) -> bool) {
    assert!(p.wait(Duration::from_secs(5), |p| p.console().iter().any(|l| f(l))), "host never saw {what}: {:?}", p.console());
}

fn dialog_open(p: &mut Page) -> bool {
    let d = p.cx.update(|cx| p.view.read(cx).dialogs().clone());
    p.cx.update(|cx| d.read(cx).is_open())
}

fn main() {
    let dir = fixtures::fixtures_dir();
    let (a, b, c) = (dir.join("form.html"), dir.join("raf.html"), dir.join("../../../Cargo.toml"));
    // The headless file chooser (dialogs.rs): these "picks" answer the page's file input.
    unsafe { std::env::set_var("HERDR_BROWSER_FILE_CHOOSER", std::env::join_paths([&a, &b, &c]).unwrap()) };
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) else { return };
    p.focus();
    // confirm(): drawn over the page (the scrim darkens it), Enter answers OK, the page gets no key.
    let before = p.page_pixel(500.0, 600.0);
    inject(&mut p, HostToApp::Dialog { id: "page".into(), req: 7, kind: DialogKind::Confirm, message: "Sure?".into(), default_text: None });
    assert!(dialog_open(&mut p), "the dialog shows");
    let under = p.page_pixel(500.0, 600.0);
    assert!(under[0] < before[0] && under[1] <= before[1], "the scrim darkens the page: {before:?} -> {under:?}");
    p.key("enter");
    saw(&mut p, "DialogResult accept", |l| l.contains(r#""type":"dialog_result""#) && l.contains(r#""req":7"#) && l.contains(r#""accept":true"#));
    assert!(!dialog_open(&mut p));
    assert!(!p.console().iter().any(|l| l.contains(r#""key":"Enter""#)), "the page never saw that Enter");
    // A confirm cancelled with Escape; a prompt answered with its default text.
    inject(&mut p, HostToApp::Dialog { id: "page".into(), req: 8, kind: DialogKind::Confirm, message: "Again?".into(), default_text: None });
    p.key("escape");
    saw(&mut p, "DialogResult cancel", |l| l.contains(r#""req":8"#) && l.contains(r#""accept":false"#));
    inject(&mut p, HostToApp::Dialog { id: "page".into(), req: 9, kind: DialogKind::Prompt, message: "Name?".into(), default_text: Some("herdr".into()) });
    let d = p.cx.update(|cx| p.view.read(cx).dialogs().clone());
    p.cx.update(|cx| d.update(cx, |d, cx| d.answer(true, cx)));
    saw(&mut p, "prompt text", |l| l.contains(r#""req":9"#) && l.contains(r#""text":"herdr""#));
    // The file chooser: picks filtered by accept (.html only; Cargo.toml dropped).
    inject(&mut p, HostToApp::FileChooser { id: "page".into(), req: 11, multiple: true, accept: vec![".html".into()] });
    saw(&mut p, "FileChooserResult", |l| l.contains(r#""type":"file_chooser_result""#) && l.contains("form.html") && l.contains("raf.html") && !l.contains("Cargo.toml"));
    // A download: progress, then done -> a toast for the window and a row in the list.
    let dl = |state, received, path: Option<&str>| HostToApp::Download { id: "page".into(), req: 3, url: server.url("download/herdr-test.txt"), suggested_name: "herdr-test.txt".into(), state, path: path.map(Into::into), received, total: Some(2048) };
    inject(&mut p, dl(DownloadState::Started, 0, None));
    inject(&mut p, dl(DownloadState::Progress, 1024, None));
    inject(&mut p, dl(DownloadState::Done, 2048, Some("/tmp/herdr-test.txt")));
    let notice = p.events().into_iter().find_map(|e| match e {
        BrowserEvent::Notice(n) => Some(n),
        _ => None,
    });
    assert_eq!(notice.as_ref().and_then(|n| n["title"].as_str()), Some("Downloaded herdr-test.txt"), "{notice:?}");
    let v = p.view.clone();
    p.cx.update(|cx| v.update(cx, |v, cx| v.toggle_downloads(cx)));
    p.pump();
    assert!(p.cx.update(|cx| v.read(cx).downloads_open()));
    // The list's white card at the top right of the page, over the stub's coloured frame.
    let px = p.page_pixel(1024.0 - 160.0, 20.0);
    assert!(px[0] > 240 && px[1] > 240 && px[2] > 240, "downloads list drawn: {px:?}");
    println!("dialogs passed: confirm over the page (Enter OK, Esc cancel, page saw no key), prompt text, file picks filtered, download toast and list");
}
