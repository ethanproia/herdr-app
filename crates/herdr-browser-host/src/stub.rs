//! The stub backend: deterministic frames, echoed input, fake navigation. No CEF.

use crate::{HostSurface, SurfaceAlloc};
use herdr_browser_protocol::{decode, encode, AppToHost, BrowserId, HostToApp, PixelSize, Size, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// A stable byte for a browser id (FNV-1a folded), so frames show whose they are.
pub fn id_byte(id: &str) -> u8 {
    let mut h: u32 = 0x811c9dc5;
    for b in id.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    (h ^ (h >> 8) ^ (h >> 16) ^ (h >> 24)) as u8
}

/// The BGRA value the stub paints at `(x, y)` of frame `seq` for browser `id` at `w` pixels wide:
/// blue is the low byte of `seq`, green is `id_byte(id)`, red is 0xC0 on the left half and 0x40 on
/// the right (so a test can check the width), alpha is opaque. The height is the surface's height.
pub fn stub_pixel(id: &str, seq: u64, x: u32, _y: u32, w: u32) -> [u8; 4] {
    [seq as u8, id_byte(id), if x < w / 2 { 0xC0 } else { 0x40 }, 0xFF]
}

struct Browser<S> {
    px: PixelSize,
    scale: f32,
    seq: u64,
    /// The unacknowledged frame; kept alive so the app can still look its surface up.
    in_flight: Option<(u64, S)>,
    /// Something changed while a frame was in flight; send the newest frame on ack.
    pending: bool,
    history: Vec<String>,
    pos: usize,
}

pub(crate) struct StubHost<A: SurfaceAlloc> {
    alloc: A,
    browsers: HashMap<BrowserId, Browser<A::Surface>>,
}

fn pixels(size: Size, scale: f32) -> PixelSize {
    PixelSize { w: (size.w * scale).ceil().max(1.0) as u32, h: (size.h * scale).ceil().max(1.0) as u32 }
}

impl<A: SurfaceAlloc> StubHost<A> {
    pub(crate) fn new(alloc: A) -> Self {
        Self { alloc, browsers: HashMap::new() }
    }

    /// Serve one connection. Returns true after `Shutdown`.
    pub(crate) fn serve_conn(&mut self, stream: UnixStream) -> io::Result<bool> {
        let mut out = stream.try_clone()?;
        for line in BufReader::new(stream).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg = match decode::<AppToHost>(&line) {
                Ok(m) => m,
                Err(e) => {
                    send(&mut out, &HostToApp::Error { id: None, message: format!("bad message: {e}") })?;
                    continue;
                }
            };
            if matches!(msg, AppToHost::Shutdown) {
                self.browsers.clear();
                return Ok(true);
            }
            for reply in self.handle(msg) {
                send(&mut out, &reply)?;
            }
        }
        // The app went away: its browsers go with it.
        self.browsers.clear();
        Ok(false)
    }

    fn handle(&mut self, msg: AppToHost) -> Vec<HostToApp> {
        use AppToHost as M;
        let mut out = Vec::new();
        match msg {
            M::Hello { .. } => out.push(HostToApp::Ready { version: PROTOCOL_VERSION, cef_version: "stub".into(), cdp_port: None }),
            M::Create { id, url, size, scale } => {
                let url = url.unwrap_or_else(|| "about:blank".into());
                let b = Browser { px: pixels(size, scale), scale, seq: 0, in_flight: None, pending: false, history: vec![url.clone()], pos: 0 };
                self.browsers.insert(id.clone(), b);
                out.push(HostToApp::CdpTarget { id: id.clone(), target_id: format!("stub-{id}") });
                self.loaded(&id, &mut out);
                self.frame(&id, &mut out);
            }
            M::Close { id } => {
                if self.browsers.remove(&id).is_none() {
                    out.push(unknown(&id));
                }
            }
            M::Shutdown => {}
            other => {
                let id = browser_id(&other).to_string();
                if !self.browsers.contains_key(&id) {
                    out.push(unknown(&id));
                    return out;
                }
                self.browser_msg(other, &mut out);
            }
        }
        out
    }

    fn browser_msg(&mut self, msg: AppToHost, out: &mut Vec<HostToApp>) {
        use AppToHost as M;
        let id = browser_id(&msg).to_string();
        let b = self.browsers.get_mut(&id).expect("checked by caller");
        match msg {
            M::Navigate { url, .. } => {
                b.history.truncate(b.pos + 1);
                b.history.push(url);
                b.pos += 1;
                out.push(HostToApp::Loading { id: id.clone(), loading: true, can_back: b.pos > 0, can_forward: false });
                self.loaded(&id, out);
                self.frame(&id, out);
            }
            M::Back { .. } | M::Forward { .. } => {
                let back = matches!(msg, M::Back { .. });
                let moved = if back && b.pos > 0 {
                    b.pos -= 1;
                    true
                } else if !back && b.pos + 1 < b.history.len() {
                    b.pos += 1;
                    true
                } else {
                    false
                };
                if moved {
                    self.loaded(&id, out);
                    self.frame(&id, out);
                }
            }
            M::Reload { .. } => {
                self.loaded(&id, out);
                self.frame(&id, out);
            }
            M::Resize { size, scale, .. } => {
                b.px = pixels(size, scale);
                b.scale = scale;
                self.frame(&id, out);
            }
            M::FrameAck { seq, .. } => {
                if b.in_flight.as_ref().is_some_and(|(s, _)| *s == seq) {
                    b.in_flight = None;
                    if std::mem::take(&mut b.pending) {
                        self.frame(&id, out);
                    }
                }
            }
            M::Focus { focused, .. } => out.push(echo(&id, format!("focus {focused}"))),
            M::Input { event, .. } => out.push(echo(&id, encode(&event))),
            M::Edit { command, .. } => out.push(echo(&id, format!("edit {}", encode(&command).trim_matches('"')))),
            M::Eval { req, expr, .. } => {
                out.push(HostToApp::EvalResult { id, req, ok: true, value: serde_json::json!({ "stub": true, "expr": expr }) })
            }
            M::Screenshot { req, .. } => {
                out.push(HostToApp::ScreenshotDone { id, req, ok: false, error: Some("the stub host takes no screenshots".into()) })
            }
            other => out.push(echo(&id, format!("ignored {}", encode(&other)))),
        }
    }

    /// Url, Title and Loading(false) for the current history entry.
    fn loaded(&self, id: &str, out: &mut Vec<HostToApp>) {
        let b = &self.browsers[id];
        let url = b.history[b.pos].clone();
        out.push(HostToApp::Url { id: id.into(), url: url.clone() });
        out.push(HostToApp::Title { id: id.into(), title: url });
        out.push(HostToApp::Loading { id: id.into(), loading: false, can_back: b.pos > 0, can_forward: b.pos + 1 < b.history.len() });
    }

    /// Send a new frame now, or mark one pending if a frame is still in flight.
    fn frame(&mut self, id: &str, out: &mut Vec<HostToApp>) {
        let b = self.browsers.get_mut(id).expect("frame for a live browser");
        if b.in_flight.is_some() {
            b.pending = true;
            return;
        }
        b.seq += 1;
        let (px, seq) = (b.px, b.seq);
        let mut buf = Vec::with_capacity(px.w as usize * px.h as usize * 4);
        for y in 0..px.h {
            for x in 0..px.w {
                buf.extend_from_slice(&stub_pixel(id, seq, x, y, px.w));
            }
        }
        let made = self.alloc.create(px).and_then(|mut s| {
            s.write_bgra(&buf)?;
            let r = s.export()?;
            Ok((s, r))
        });
        let b = self.browsers.get_mut(id).expect("frame for a live browser");
        match made {
            Ok((s, surface)) => {
                b.in_flight = Some((seq, s));
                out.push(HostToApp::Frame { id: id.into(), seq, surface, size: px, scale: b.scale });
            }
            Err(e) => out.push(HostToApp::Error { id: Some(id.into()), message: format!("frame: {e}") }),
        }
    }
}

fn browser_id(m: &AppToHost) -> &str {
    use AppToHost as M;
    match m {
        M::Create { id, .. } | M::Close { id } | M::Navigate { id, .. } | M::Back { id } | M::Forward { id } | M::Reload { id, .. }
        | M::Stop { id } | M::Resize { id, .. } | M::Focus { id, .. } | M::SetZoom { id, .. } | M::Input { id, .. } | M::Edit { id, .. }
        | M::FrameAck { id, .. } | M::Find { id, .. } | M::StopFind { id, .. } | M::DevTools { id, .. } | M::Eval { id, .. }
        | M::Screenshot { id, .. } | M::FileChooserResult { id, .. } | M::DialogResult { id, .. } => id,
        M::Hello { .. } | M::Shutdown => "",
    }
}

fn echo(id: &str, message: String) -> HostToApp {
    HostToApp::Console { id: id.into(), level: "debug".into(), message, source: "stub".into(), line: 0 }
}

fn unknown(id: &str) -> HostToApp {
    HostToApp::Error { id: Some(id.into()), message: format!("no browser {id}") }
}

fn send(out: &mut UnixStream, msg: &HostToApp) -> io::Result<()> {
    let mut line = encode(msg);
    line.push('\n');
    out.write_all(line.as_bytes())
}
