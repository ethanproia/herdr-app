//! Drives `herdr-browser-host --stub` over its socket with the protocol crate, as the app will.

use herdr_browser_host::{id_byte, stub_pixel};
use herdr_browser_protocol::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Host {
    child: Child,
    out: UnixStream,
    lines: BufReader<UnixStream>,
}

impl Host {
    fn start(name: &str) -> Host {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("host-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("host.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_herdr-browser-host")).arg("--stub").arg("--socket").arg(&sock).spawn().unwrap();
        let start = Instant::now();
        let out = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) if start.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("host never listened: {e}"),
            }
        };
        out.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let lines = BufReader::new(out.try_clone().unwrap());
        let mut h = Host { child, out, lines };
        h.send(AppToHost::Hello { version: PROTOCOL_VERSION, profile_dir: dir.join("profile").display().to_string(), cdp_port: None });
        assert!(matches!(h.recv(), HostToApp::Ready { version: PROTOCOL_VERSION, .. }));
        h
    }

    fn send(&mut self, m: AppToHost) {
        writeln!(self.out, "{}", encode(&m)).unwrap();
    }

    fn recv(&mut self) -> HostToApp {
        let mut line = String::new();
        self.lines.read_line(&mut line).expect("host reply");
        decode(&line).unwrap_or_else(|e| panic!("bad reply {line:?}: {e}"))
    }

    /// Read until a message matches, returning it; panics on a host `Error`.
    fn until(&mut self, f: impl Fn(&HostToApp) -> bool) -> HostToApp {
        loop {
            let m = self.recv();
            if let HostToApp::Error { message, .. } = &m {
                panic!("host error: {message}");
            }
            if f(&m) {
                return m;
            }
        }
    }

    fn frame(&mut self) -> (u64, SurfaceRef, PixelSize) {
        match self.until(|m| matches!(m, HostToApp::Frame { .. })) {
            HostToApp::Frame { seq, surface, size, .. } => (seq, surface, size),
            _ => unreachable!(),
        }
    }

    fn shutdown(mut self) {
        self.send(AppToHost::Shutdown);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "host exit {status}");
    }
}

fn create(h: &mut Host, id: &str, w: f32, hgt: f32, scale: f32) {
    h.send(AppToHost::Create { id: id.into(), url: Some("https://a.test/".into()), size: Size { w, h: hgt }, scale });
}

#[test]
fn frame_pixels_encode_id_size_and_seq() {
    let mut h = Host::start("pixels");
    create(&mut h, "b1", 40.0, 30.0, 2.0);
    let (seq, surface, size) = h.frame();
    assert_eq!((seq, size), (1, PixelSize { w: 80, h: 60 }));
    // A real cross-process IOSurface import, as the app does it.
    let s = herdr_browser_surface::import(&surface).unwrap();
    assert_eq!(s.size(), size);
    assert_eq!(s.pixel(0, 0).unwrap(), stub_pixel("b1", 1, 0, 0, 80));
    assert_eq!(s.pixel(79, 59).unwrap(), stub_pixel("b1", 1, 79, 59, 80));
    assert_eq!(s.pixel(79, 59).unwrap(), [1, id_byte("b1"), 0x40, 0xFF]);
    assert_ne!(id_byte("b1"), id_byte("b2"));
    h.shutdown();
}

#[test]
fn one_frame_in_flight_newest_pending() {
    let mut h = Host::start("flow");
    create(&mut h, "b1", 10.0, 10.0, 1.0);
    let (seq, _, _) = h.frame();
    assert_eq!(seq, 1);
    // Two resizes while frame 1 is unacked: no frame until the ack, then only the newest size.
    h.send(AppToHost::Resize { id: "b1".into(), size: Size { w: 20.0, h: 10.0 }, scale: 1.0 });
    h.send(AppToHost::Resize { id: "b1".into(), size: Size { w: 30.0, h: 10.0 }, scale: 1.0 });
    h.send(AppToHost::Eval { id: "b1".into(), req: 9, expr: "1".into() });
    // The eval reply arriving first proves no frame was sent for the resizes.
    assert!(matches!(h.recv(), HostToApp::EvalResult { req: 9, .. }));
    h.send(AppToHost::FrameAck { id: "b1".into(), seq: 1 });
    let (seq, surface, size) = h.frame();
    assert_eq!((seq, size), (2, PixelSize { w: 30, h: 10 }));
    let s = herdr_browser_surface::import(&surface).unwrap();
    assert_eq!(s.pixel(0, 0).unwrap(), stub_pixel("b1", 2, 0, 0, 30));
    // A stale ack changes nothing; nothing is pending, so no frame follows a correct ack.
    h.send(AppToHost::FrameAck { id: "b1".into(), seq: 1 });
    h.send(AppToHost::FrameAck { id: "b1".into(), seq: 2 });
    h.send(AppToHost::Eval { id: "b1".into(), req: 10, expr: "1".into() });
    assert!(matches!(h.recv(), HostToApp::EvalResult { req: 10, .. }));
    h.shutdown();
}

#[test]
fn input_and_focus_echo_as_console() {
    let mut h = Host::start("input");
    create(&mut h, "b1", 10.0, 10.0, 1.0);
    h.frame();
    let ev = InputEvent::Key { kind: KeyKind::Down, key: "a".into(), code: "KeyA".into(), native: 0, repeat: false, mods: Modifiers::default() };
    h.send(AppToHost::Input { id: "b1".into(), event: ev.clone() });
    h.send(AppToHost::Input { id: "b1".into(), event: InputEvent::Ime(ImeEvent::Commit { text: "日本".into() }) });
    h.send(AppToHost::Focus { id: "b1".into(), focused: true });
    let msgs: Vec<String> = (0..3)
        .map(|_| match h.until(|m| matches!(m, HostToApp::Console { .. })) {
            HostToApp::Console { message, .. } => message,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(decode::<InputEvent>(&msgs[0]).unwrap(), ev);
    assert_eq!(decode::<InputEvent>(&msgs[1]).unwrap(), InputEvent::Ime(ImeEvent::Commit { text: "日本".into() }));
    assert_eq!(msgs[2], "focus true");
    h.shutdown();
}

#[test]
fn navigate_updates_url_title_loading_and_history() {
    let mut h = Host::start("nav");
    create(&mut h, "b1", 10.0, 10.0, 1.0);
    h.frame();
    h.send(AppToHost::Navigate { id: "b1".into(), url: "https://b.test/".into() });
    assert_eq!(h.recv(), HostToApp::Loading { id: "b1".into(), loading: true, can_back: true, can_forward: false });
    assert_eq!(h.recv(), HostToApp::Url { id: "b1".into(), url: "https://b.test/".into() });
    assert_eq!(h.recv(), HostToApp::Title { id: "b1".into(), title: "https://b.test/".into() });
    assert_eq!(h.recv(), HostToApp::Loading { id: "b1".into(), loading: false, can_back: true, can_forward: false });
    h.send(AppToHost::Back { id: "b1".into() });
    assert_eq!(h.until(|m| matches!(m, HostToApp::Url { .. })), HostToApp::Url { id: "b1".into(), url: "https://a.test/".into() });
    assert_eq!(h.until(|m| matches!(m, HostToApp::Loading { .. })), HostToApp::Loading { id: "b1".into(), loading: false, can_back: false, can_forward: true });
    h.send(AppToHost::Eval { id: "b1".into(), req: 3, expr: "document.title".into() });
    assert_eq!(
        h.until(|m| matches!(m, HostToApp::EvalResult { .. })),
        HostToApp::EvalResult { id: "b1".into(), req: 3, ok: true, value: serde_json::json!({ "stub": true, "expr": "document.title" }) }
    );
    h.shutdown();
}

#[test]
fn unknown_browser_and_bad_lines_are_errors_not_crashes() {
    let mut h = Host::start("errors");
    h.send(AppToHost::Navigate { id: "nope".into(), url: "https://x.test/".into() });
    assert!(matches!(h.recv(), HostToApp::Error { id: Some(ref i), .. } if i == "nope"));
    writeln!(h.out, "not json").unwrap();
    assert!(matches!(h.recv(), HostToApp::Error { id: None, .. }));
    create(&mut h, "b1", 4.0, 4.0, 1.0);
    h.send(AppToHost::Close { id: "b1".into() });
    h.send(AppToHost::Close { id: "b1".into() });
    // The first close succeeds silently; the second names the missing browser.
    let err = loop {
        if let m @ HostToApp::Error { .. } = h.recv() {
            break m;
        }
    };
    assert!(matches!(err, HostToApp::Error { id: Some(ref i), .. } if i == "b1"));
    h.shutdown();
}
