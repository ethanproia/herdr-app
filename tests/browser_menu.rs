//! The context menu (I5), app side, against the stub host: the host's menu is drawn over the
//! page, a click runs a host command (MenuResult Some), the app's own rows run in the app
//! (new tab, copy link), Escape and a click outside dismiss (MenuResult None). The CEF side
//! (RunContextMenu -> ContextMenu) is fork-host's; its real-CEF test lives there.
include!("common/browser_harness.rs");

use browser::protocol::{HostToApp, MenuItem};
use browser::view::{BrowserEvent, MENU_ROW};
use harness::{Host, Page};
use std::time::Duration;

fn item(command: i32, label: &str, enabled: bool) -> MenuItem {
    MenuItem { command, label: label.into(), enabled, checked: false, submenu: vec![] }
}

fn host_menu(p: &mut Page, req: u64, link: Option<&str>) {
    let items = vec![
        item(100, "Back", true),
        item(101, "Forward", false),
        item(102, "Reload", true),
        item(0, "", false),
        item(110, "Cut", false),
        item(111, "Copy", true),
        item(112, "Paste", true),
        item(113, "Select All", true),
        item(0, "", false),
        MenuItem { command: 0, label: "More".into(), enabled: true, checked: false, submenu: vec![item(200, "Inspect", true)] },
    ];
    let m = HostToApp::ContextMenu { id: "page".into(), req, x: 100.0, y: 80.0, kind: vec![if link.is_some() { "link".into() } else { "page".into() }], link_url: link.map(Into::into), src_url: None, selection: None, items };
    let v = p.view.clone();
    p.cx.update(|cx| v.update(cx, |v, cx| v.on_host(m, cx)));
    p.pump();
}

/// Page point of row `i` of the open menu.
fn row(p: &mut Page, i: usize) -> (f32, f32) {
    let (x, y) = p.cx.update(|cx| p.view.read(cx).menu().unwrap().at);
    (x + 40.0, y + 4.0 + MENU_ROW * i as f32 + MENU_ROW / 2.0)
}

fn rows(p: &mut Page) -> Vec<String> {
    p.cx.update(|cx| p.view.read(cx).menu().map(|m| m.rows.iter().map(|r| if r.command == 0 { "--".into() } else { r.label.clone() }).collect()).unwrap_or_default())
}

fn saw(p: &mut Page, line: &str) -> bool {
    p.wait(Duration::from_secs(5), |p| p.console().iter().any(|l| l == line))
}

fn main() {
    let server = fixtures::Server::start();
    let Some(mut p) = Page::open(Host::Stub, &server.url("index.html")) else { return };
    p.focus();
    // A page menu: host rows only, the submenu flattened, separators kept tidy.
    host_menu(&mut p, 1, None);
    assert_eq!(rows(&mut p), ["Back", "Forward", "Reload", "--", "Cut", "Copy", "Paste", "Select All", "--", "More › Inspect"]);
    // Drawn over the frame: the menu's white background where the stub frame is coloured.
    let (x, y) = row(&mut p, 0);
    let px = p.page_pixel(x + 150.0, y);
    assert!(px[0] > 240 && px[1] > 240 && px[2] > 240, "menu drawn over the page at its first row: {px:?}");
    // A disabled row does nothing; Reload goes to the host.
    let (x, y) = row(&mut p, 1);
    p.click(x, y);
    assert!(p.cx.update(|cx| p.view.read(cx).menu().is_some()), "a disabled row keeps the menu open");
    let (x, y) = row(&mut p, 2);
    p.click(x, y);
    assert!(saw(&mut p, "menu Some(102)"), "Reload chosen: {:?}", p.console());
    assert!(p.cx.update(|cx| p.view.read(cx).menu().is_none()));
    // Escape dismisses before the page sees the key.
    host_menu(&mut p, 2, None);
    p.key("escape");
    assert!(saw(&mut p, "menu None"), "Escape dismisses: {:?}", p.console());
    assert!(!p.console().iter().any(|l| l.contains(r#""key":"Escape""#)), "the page never saw that Escape");
    // A click outside dismisses too.
    host_menu(&mut p, 3, None);
    let before = p.console().iter().filter(|l| *l == "menu None").count();
    p.click(600.0, 500.0);
    assert!(p.wait(Duration::from_secs(5), |p| p.console().iter().filter(|l| *l == "menu None").count() > before), "click outside dismisses");
    // A link menu: the app's rows first. Open Link in New Tab is the window's job (NewWindow).
    host_menu(&mut p, 4, Some("http://127.0.0.1/x.html"));
    assert_eq!(&rows(&mut p)[..3], ["Open Link in New Tab", "Copy Link Address", "--"]);
    let (x, y) = row(&mut p, 0);
    p.click(x, y);
    assert!(p.events().iter().any(|e| matches!(e, BrowserEvent::NewWindow(u) if u == "http://127.0.0.1/x.html")), "new tab asked of the window");
    // Copy Link Address: the clipboard holds it (where the test platform has one).
    host_menu(&mut p, 5, Some("http://127.0.0.1/y.html"));
    let (x, y) = row(&mut p, 1);
    p.click(x, y);
    let clip = p.cx.update(|cx| cx.read_from_clipboard().and_then(|c| c.text()));
    println!("clipboard after Copy Link Address: {clip:?}");
    if let Some(c) = clip {
        assert_eq!(c, "http://127.0.0.1/y.html");
    }
    let nones = p.console().iter().filter(|l| *l == "menu None").count();
    assert!(nones >= 4, "app rows close the host's menu with None: {nones}");
    println!("menu passed: drawn over the page, host command 102 sent, disabled row inert, Escape and click-outside dismiss, open-link and copy-link run in the app");
}
