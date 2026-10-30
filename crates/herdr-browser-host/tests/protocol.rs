//! The protocol suite: passes against the stub (default) and the real CEF host
//! (`HERDR_BROWSER_HOST=<bundle exe>`). Asserts only what both backends promise.

mod common;
use common::*;
use herdr_browser_protocol::*;
use std::io::Write;
use std::time::Duration;

const SEC: Duration = Duration::from_secs(1);

fn pages() -> Pages {
    Pages::start(vec![("a", solid("A", "#3366cc")), ("b", solid("B", "#cc6633"))])
}

#[test]
fn frames_have_the_block_size_times_scale() {
    let p = pages();
    let mut h = Host::start("size");
    assert!(matches!(h.ready, HostToApp::Ready { version: PROTOCOL_VERSION, .. }));
    create(&mut h, "b1", &p.url("a"), 200.0, 100.0, 2.0);
    let (_, surface, size) = h.frame("b1");
    assert_eq!(size, PixelSize { w: 400, h: 200 });
    assert_eq!(herdr_browser_surface::import(&surface).unwrap().size(), size);
    h.shutdown();
}

#[test]
fn one_frame_in_flight_until_ack() {
    let p = pages();
    let mut h = Host::start("flow");
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    let (seq, _, _) = h.frame("b1");
    // Without an ack nothing more may arrive, even across a resize.
    h.send(AppToHost::Resize { id: "b1".into(), size: Size { w: 120.0, h: 60.0 }, scale: 1.0 });
    let end = std::time::Instant::now() + 2 * SEC;
    while let Some(m) = h.next(end.saturating_duration_since(std::time::Instant::now())) {
        assert!(!matches!(m, HostToApp::Frame { .. }), "frame sent while {seq} was in flight: {m:?}");
    }
    h.send(AppToHost::FrameAck { id: "b1".into(), seq });
    let (_, surface) = h.frame_of_size("b1", PixelSize { w: 120, h: 60 });
    assert_eq!(herdr_browser_surface::import(&surface).unwrap().size(), PixelSize { w: 120, h: 60 });
    h.shutdown();
}

#[test]
fn navigate_back_forward_report_url_and_loading() {
    let p = pages();
    let mut h = Host::start("nav");
    let (a, b) = (p.url("a"), p.url("b"));
    create(&mut h, "b1", &a, 100.0, 50.0, 1.0);
    h.until(10 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, .. }));
    h.send(AppToHost::Navigate { id: "b1".into(), url: b.clone() });
    h.until(10 * SEC, |m| matches!(m, HostToApp::Url { url, .. } if *url == b));
    h.until(10 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, can_back: true, can_forward: false, .. }));
    h.send(AppToHost::Back { id: "b1".into() });
    h.until(10 * SEC, |m| matches!(m, HostToApp::Url { url, .. } if *url == a));
    h.until(10 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, can_back: false, can_forward: true, .. }));
    h.shutdown();
}

#[test]
fn eval_answers_with_the_same_req() {
    let p = pages();
    let mut h = Host::start("eval");
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    h.until(10 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, .. }));
    let (ok, _) = h.eval("b1", 41, "1+1");
    assert!(ok);
    h.shutdown();
}

#[test]
fn unknown_browsers_and_bad_lines_are_errors_not_crashes() {
    let p = pages();
    let mut h = Host::start("errors");
    h.send(AppToHost::Navigate { id: "nope".into(), url: p.url("a") });
    assert!(matches!(h.next(10 * SEC), Some(HostToApp::Error { id: Some(ref i), .. }) if i == "nope"));
    writeln!(h.out, "not json").unwrap();
    assert!(matches!(h.next(10 * SEC), Some(HostToApp::Error { id: None, .. })));
    create(&mut h, "b1", &p.url("a"), 10.0, 10.0, 1.0);
    h.frame("b1");
    h.send(AppToHost::Close { id: "b1".into() });
    h.send(AppToHost::Close { id: "b1".into() });
    h.until(10 * SEC, |m| matches!(m, HostToApp::Error { id: Some(i), .. } if i == "b1"));
    h.shutdown();
}

#[test]
fn create_reports_a_cdp_target_and_edit_is_accepted() {
    let p = pages();
    let mut h = Host::start("cdp-target");
    create(&mut h, "b1", &p.url("a"), 100.0, 50.0, 1.0);
    match h.until(10 * SEC, |m| matches!(m, HostToApp::CdpTarget { .. })) {
        HostToApp::CdpTarget { id, target_id } => assert!(id == "b1" && !target_id.is_empty()),
        _ => unreachable!(),
    }
    h.until(10 * SEC, |m| matches!(m, HostToApp::Loading { loading: false, .. }));
    h.send(AppToHost::Edit { id: "b1".into(), command: EditCommand::SelectAll });
    // Edit has no reply of its own; a following eval proves the host is still serving.
    assert!(h.eval("b1", 2, "1").0);
    h.shutdown();
}
