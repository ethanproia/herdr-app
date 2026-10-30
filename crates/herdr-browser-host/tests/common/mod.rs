//! Shared harness: start a host binary on a private socket and drive it with the protocol crate.
//!
//! Which host: `HERDR_BROWSER_HOST=<path to the .app's executable>` runs the real CEF host
//! (build it with scripts/fork-host-bundle.sh); unset runs this crate's binary with `--stub`.
//! Pages come from a local HTTP server (`Pages`), never the internet.
#![allow(dead_code)]

use herdr_browser_protocol::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn real_host() -> Option<PathBuf> {
    std::env::var_os("HERDR_BROWSER_HOST").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("host-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Host {
    child: Child,
    pub out: UnixStream,
    rx: mpsc::Receiver<String>,
    pub ready: HostToApp,
}

impl Host {
    /// Start a host whose socket and profile live under `dir`.
    pub fn start_in(dir: &Path) -> Host {
        let sock = dir.join("host.sock");
        let _ = std::fs::remove_file(&sock);
        let mut cmd = match real_host() {
            Some(exe) => Command::new(exe),
            None => {
                let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-browser-host"));
                c.arg("--stub");
                c
            }
        };
        let child = cmd.arg("--socket").arg(&sock).spawn().unwrap();
        let start = Instant::now();
        let out = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) if start.elapsed() < Duration::from_secs(20) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => panic!("host never listened: {e}"),
            }
        };
        let (tx, rx) = mpsc::channel();
        let reader = BufReader::new(out.try_clone().unwrap());
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut h = Host { child, out, rx, ready: HostToApp::Error { id: None, message: String::new() } };
        h.send(AppToHost::Hello { version: PROTOCOL_VERSION, profile_dir: dir.join("profile").display().to_string(), cdp_port: Some(0), cdp_pipe: false });
        h.ready = h.until(Duration::from_secs(30), |m| matches!(m, HostToApp::Ready { .. }));
        h
    }

    pub fn start(name: &str) -> Host {
        Host::start_in(&scratch(name))
    }

    pub fn send(&mut self, m: AppToHost) {
        writeln!(self.out, "{}", encode(&m)).unwrap();
    }

    /// The next message, or None after `wait`.
    pub fn next(&mut self, wait: Duration) -> Option<HostToApp> {
        let line = self.rx.recv_timeout(wait).ok()?;
        if std::env::var_os("HERDR_TEST_TRACE").is_some() {
            eprintln!("<- {line}");
        }
        Some(decode(&line).unwrap_or_else(|e| panic!("bad reply {line:?}: {e}")))
    }

    /// Read until a message matches; panics on timeout or on an unexpected host `Error`.
    /// Frames skipped on the way are acked, as the app would, so the host keeps painting.
    pub fn until(&mut self, wait: Duration, f: impl Fn(&HostToApp) -> bool) -> HostToApp {
        let end = Instant::now() + wait;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let Some(m) = self.next(left) else { panic!("timed out after {wait:?}") };
            if f(&m) {
                return m;
            }
            match &m {
                HostToApp::Error { message, .. } => panic!("host error: {message}"),
                HostToApp::Frame { id, seq, .. } => self.send(AppToHost::FrameAck { id: id.clone(), seq: *seq }),
                _ => {}
            }
        }
    }

    pub fn frame(&mut self, id: &str) -> (u64, SurfaceRef, PixelSize) {
        match self.until(Duration::from_secs(20), |m| matches!(m, HostToApp::Frame { id: i, .. } if i == id)) {
            HostToApp::Frame { seq, surface, size, .. } => (seq, surface, size),
            _ => unreachable!(),
        }
    }

    /// Ack frames until one of `size` arrives; returns it (still unacked).
    pub fn frame_of_size(&mut self, id: &str, size: PixelSize) -> (u64, SurfaceRef) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let (seq, surface, got) = self.frame(id);
            if got == size {
                return (seq, surface);
            }
            assert!(Instant::now() < end, "no {size:?} frame, last {got:?}");
            self.send(AppToHost::FrameAck { id: id.into(), seq });
        }
    }

    /// Ack frames until the page has settled and the newest frame satisfies `ok` (pixels at (x, y)).
    pub fn frame_where(&mut self, id: &str, x: u32, y: u32, ok: impl Fn([u8; 4]) -> bool) -> (u64, [u8; 4]) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let (seq, surface, _) = self.frame(id);
            let px = herdr_browser_surface::import(&surface).unwrap().pixel(x, y).unwrap();
            if ok(px) {
                return (seq, px);
            }
            assert!(Instant::now() < end, "pixel at ({x},{y}) never matched, last {px:?}");
            self.send(AppToHost::FrameAck { id: id.into(), seq });
        }
    }

    pub fn eval(&mut self, id: &str, req: u64, expr: &str) -> (bool, serde_json::Value) {
        self.send(AppToHost::Eval { id: id.into(), req, expr: expr.into() });
        match self.until(Duration::from_secs(20), |m| matches!(m, HostToApp::EvalResult { req: r, .. } if *r == req)) {
            HostToApp::EvalResult { ok, value, .. } => (ok, value),
            _ => unreachable!(),
        }
    }

    pub fn shutdown(mut self) {
        self.send(AppToHost::Shutdown);
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "host exit {status}");
                return;
            }
            assert!(Instant::now() < end, "host did not exit after Shutdown");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A tiny local HTTP server for test pages: `GET /<name>` serves `pages[name]`.
pub struct Pages {
    pub port: u16,
}

impl Pages {
    pub fn start(pages: Vec<(&'static str, String)>) -> Pages {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut buf = [0u8; 4096];
                let n = conn.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let (status, body) = match pages.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", "not found".to_string()),
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Pages { port }
    }

    pub fn url(&self, page: &str) -> String {
        format!("http://127.0.0.1:{}/{page}", self.port)
    }
}

/// A full-bleed page in one colour.
pub fn solid(title: &str, css: &str) -> String {
    format!("<!doctype html><html><head><title>{title}</title></head><body style=\"margin:0;background:{css};height:100vh\"></body></html>")
}

pub fn create(h: &mut Host, id: &str, url: &str, w: f32, hgt: f32, scale: f32) {
    h.send(AppToHost::Create { id: id.into(), url: Some(url.into()), size: Size { w, h: hgt }, scale, profile: None });
}
