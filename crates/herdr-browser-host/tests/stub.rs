//! Stub-only behaviour: the pixel pattern and Console echo of input. Skipped for the real host.

mod common;
use common::*;
use herdr_browser_host::{id_byte, stub_pixel};
use herdr_browser_protocol::*;
use std::time::Duration;

#[test]
fn stub_pixels_encode_id_size_and_seq() {
    if real_host().is_some() {
        return;
    }
    let mut h = Host::start("stub-pixels");
    create(&mut h, "b1", "https://a.test/", 40.0, 30.0, 2.0);
    let (seq, surface, size) = h.frame("b1");
    assert_eq!((seq, size), (1, PixelSize { w: 80, h: 60 }));
    let s = herdr_browser_surface::import(&surface).unwrap();
    assert_eq!(s.pixel(0, 0).unwrap(), stub_pixel("b1", 1, 0, 0, 80));
    assert_eq!(s.pixel(79, 59).unwrap(), [1, id_byte("b1"), 0x40, 0xFF]);
    assert_ne!(id_byte("b1"), id_byte("b2"));
    h.send(AppToHost::FrameAck { id: "b1".into(), seq });
    h.send(AppToHost::Resize { id: "b1".into(), size: Size { w: 30.0, h: 10.0 }, scale: 1.0 });
    let (seq, surface) = h.frame_of_size("b1", PixelSize { w: 30, h: 10 });
    assert_eq!(herdr_browser_surface::import(&surface).unwrap().pixel(0, 0).unwrap(), stub_pixel("b1", seq, 0, 0, 30));
    h.shutdown();
}

#[test]
fn stub_echoes_input_and_focus_as_console() {
    if real_host().is_some() {
        return;
    }
    let mut h = Host::start("stub-input");
    create(&mut h, "b1", "https://a.test/", 10.0, 10.0, 1.0);
    h.frame("b1");
    let key = InputEvent::Key { kind: KeyKind::Down, key: "a".into(), code: "KeyA".into(), native: 0, repeat: false, mods: Modifiers::default() };
    let ime = InputEvent::Ime(ImeEvent::Commit { text: "日本".into() });
    h.send(AppToHost::Input { id: "b1".into(), event: key.clone() });
    h.send(AppToHost::Input { id: "b1".into(), event: ime.clone() });
    h.send(AppToHost::Focus { id: "b1".into(), focused: true });
    h.send(AppToHost::Edit { id: "b1".into(), command: EditCommand::Paste });
    let mut got = vec![];
    for _ in 0..4 {
        if let HostToApp::Console { message, .. } = h.until(Duration::from_secs(5), |m| matches!(m, HostToApp::Console { .. })) {
            got.push(message);
        }
    }
    assert_eq!(decode::<InputEvent>(&got[0]).unwrap(), key);
    assert_eq!(decode::<InputEvent>(&got[1]).unwrap(), ime);
    assert_eq!(got[2], "focus true");
    assert_eq!(got[3], "edit paste");
    h.shutdown();
}

#[test]
fn stub_reports_color_scheme_changes() {
    if real_host().is_some() {
        return;
    }
    let mut h = Host::start("stub-scheme");
    create(&mut h, "b1", "https://a.test/", 10.0, 10.0, 1.0);
    h.frame("b1");
    h.send(AppToHost::SetColorScheme { dark: true });
    let m = h.until(Duration::from_secs(5), |m| matches!(m, HostToApp::Console { message, .. } if message.starts_with("color-scheme")));
    assert!(matches!(m, HostToApp::Console { message, .. } if message == "color-scheme dark"));
    h.shutdown();
}
