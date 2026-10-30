//! A tiny local HTTP server for the browser fixtures (tests/fixtures/browser), one thread per
//! connection, 127.0.0.1 on a free port. Routes:
//! - `/<file>`: a fixture page (`/` is index.html)
//! - `/video/<name>`: a test video from `$HERDR_FORK_DIR/scratch/videos` (Range requests served)
//! - `/download/<name>`: a small file sent as an attachment (`Content-Disposition`)
//! - `POST /login`: sets the `session` cookie from the form's `user`, redirects to /login.html
//! - `/whoami`: the `session` cookie's value, as text (empty when none)
//! - `/slow?ms=N`: answers after N ms (loading states)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

pub struct Server {
    pub port: u16,
}

impl Server {
    /// Starts the server for this process (once; later calls return the same port).
    pub fn start() -> &'static Server {
        static S: std::sync::OnceLock<Server> = std::sync::OnceLock::new();
        S.get_or_init(|| {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = l.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    std::thread::spawn(move || {
                        let _ = serve(s);
                    });
                }
            });
            Server { port }
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, path.trim_start_matches('/'))
    }
}

/// tests/fixtures/browser
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/browser")
}

/// Generated test videos (scripts/browser-test-videos.sh), outside git.
pub fn videos_dir() -> PathBuf {
    let base = std::env::var_os("HERDR_FORK_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("work.noindex/herdr-fork"));
    base.join("scratch/videos")
}

fn mime(p: &str) -> &'static str {
    match p.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn serve(s: TcpStream) -> std::io::Result<()> {
    let mut r = BufReader::new(s.try_clone()?);
    let mut w = s;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("/").to_owned());
        let (mut len, mut range, mut cookie) = (0usize, None::<(u64, Option<u64>)>, String::new());
        loop {
            let mut h = String::new();
            if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
                break;
            }
            let (k, v) = h.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).unwrap_or_default();
            match k.as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "cookie" => cookie = v,
                "range" => {
                    range = v.strip_prefix("bytes=").and_then(|b| {
                        let (a, z) = b.split_once('-')?;
                        Some((a.parse().ok()?, z.parse().ok()))
                    })
                }
                _ => {}
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let session = cookie.split(';').filter_map(|c| c.trim().strip_prefix("session=")).next().unwrap_or("").to_owned();
        match (method.as_str(), path) {
            ("POST", "/login") => {
                let form = String::from_utf8_lossy(&body);
                let user = form.split('&').filter_map(|kv| kv.strip_prefix("user=")).next().unwrap_or("someone");
                write!(w, "HTTP/1.1 303 See Other\r\nLocation: /login.html\r\nSet-Cookie: session={user}; Path=/; Max-Age=86400\r\nContent-Length: 0\r\n\r\n")?;
            }
            (_, "/whoami") => respond(&mut w, 200, "text/plain", session.as_bytes(), &[])?,
            (_, "/slow") => {
                let ms = query.strip_prefix("ms=").and_then(|v| v.parse().ok()).unwrap_or(1000);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                respond(&mut w, 200, "text/html", b"<title>slow</title>slow", &[])?;
            }
            (_, p) if p.starts_with("/download/") => {
                let name = &p["/download/".len()..];
                let data = format!("herdr download test file {name}\n").repeat(64);
                respond(&mut w, 200, "application/octet-stream", data.as_bytes(), &[("Content-Disposition", &format!("attachment; filename=\"{name}\""))])?;
            }
            (_, p) if p.starts_with("/video/") => {
                let file = videos_dir().join(&p["/video/".len()..]);
                match std::fs::read(&file) {
                    Ok(data) => match range {
                        Some((a, z)) => {
                            let total = data.len() as u64;
                            let end = z.unwrap_or(total - 1).min(total - 1);
                            let part = &data[a as usize..=end as usize];
                            let cr = format!("bytes {a}-{end}/{total}");
                            respond(&mut w, 206, mime(p), part, &[("Content-Range", &cr), ("Accept-Ranges", "bytes")])?;
                        }
                        None => respond(&mut w, 200, mime(p), &data, &[("Accept-Ranges", "bytes")])?,
                    },
                    Err(_) => respond(&mut w, 404, "text/plain", b"no such video: run scripts/browser-test-videos.sh", &[])?,
                }
            }
            (_, p) => {
                let rel = if p == "/" { "index.html" } else { p.trim_start_matches('/') };
                if rel.contains("..") {
                    respond(&mut w, 403, "text/plain", b"no", &[])?;
                    continue;
                }
                match std::fs::read(fixtures_dir().join(rel)) {
                    Ok(data) => respond(&mut w, 200, mime(rel), &data, &[])?,
                    Err(_) => respond(&mut w, 404, "text/plain", b"not found", &[])?,
                }
            }
        }
    }
}

fn respond(w: &mut TcpStream, code: u16, ty: &str, body: &[u8], extra: &[(&str, &str)]) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        206 => "Partial Content",
        303 => "See Other",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n", body.len());
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)
}
