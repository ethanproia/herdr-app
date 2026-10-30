//! Part 5: the CDP broker, the one door agents have to the browser (docs/browser-cdp-pipe.md).
//!
//! The host serves CDP only on a pipe the app holds (`cdp::CdpPipe`, no TCP port). This serves it to
//! agents on `$HERDR_APP_HOME/browser/agent.sock` (0600, in a 0700 dir) to callers that show the token in
//! `browser/broker.token` (0600). Clients name browser blocks, never targets or sessions: the broker
//! attaches one flatten session per block and refuses browser-level and profile methods outright.
//! The finer per-block grants (control, cdp, capture), the method allow-list and redaction are applied by
//! the agent tools on top (scripts/herdr-browser), which are the broker's only expected client.
//!
//! Wire: newline JSON. First line `{"token": "..."}` (anything else closes the connection). Then
//! `{"id": n, "block": "<block id>", "method": "...", "params": {...}}`, answered
//! `{"id": n, "result": ...}` or `{"id": n, "error": "..."}`; events of every block the connection has
//! used arrive as `{"block": "...", "method": "...", "params": {...}}`.

use super::cdp::CdpPipe;
use herdr_browser_protocol::BrowserId;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The host's pipe now (a new one after a restart), if there is one.
pub type PipeNow = Arc<dyn Fn() -> Option<Arc<CdpPipe>> + Send + Sync>;
/// The CDP target that shows a block (from `HostToApp::CdpTarget`).
pub type TargetOf = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Methods no agent gets through the broker, whatever its grants: other tabs and the browser itself
/// (`Target.*` but `getTargetInfo`, `Browser.*`), the profile (cookies, storage), the machine.
pub fn refused(method: &str) -> bool {
    const DOMAINS: &[&str] = &["Browser.", "Storage.", "SystemInfo.", "IO.", "Tethering.", "Memory.", "Cast.", "BackgroundService."];
    const METHODS: &[&str] = &[
        "Network.getAllCookies", "Network.getCookies", "Network.setCookie", "Network.setCookies", "Network.deleteCookies",
        "Network.clearBrowserCookies", "Network.clearBrowserCache", "Page.setDownloadBehavior",
    ];
    (method.starts_with("Target.") && method != "Target.getTargetInfo") || DOMAINS.iter().any(|d| method.starts_with(d)) || METHODS.contains(&method)
}

pub struct Broker {
    dir: PathBuf,
    token: String,
    pipe: PipeNow,
    target_of: TargetOf,
    /// block -> (target, session, the pipe it was attached on)
    sessions: Mutex<HashMap<BrowserId, (String, String, usize)>>,
}

impl Broker {
    pub fn socket(dir: &Path) -> PathBuf {
        dir.join("agent.sock")
    }

    pub fn token_file(dir: &Path) -> PathBuf {
        dir.join("broker.token")
    }

    /// Writes a fresh token, binds the socket (owner-only) and serves it on a thread.
    pub fn start(dir: PathBuf, pipe: PipeNow, target_of: TargetOf) -> std::io::Result<Arc<Broker>> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let mut raw = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let token_file = Self::token_file(&dir);
        let _ = std::fs::remove_file(&token_file);
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&token_file)?.write_all(token.as_bytes())?;
        let sock = Self::socket(&dir);
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
        let broker = Arc::new(Broker { dir, token, pipe, target_of, sessions: Mutex::new(HashMap::new()) });
        let b = broker.clone();
        std::thread::Builder::new().name("herdr-browser-broker".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let b = b.clone();
                std::thread::spawn(move || b.serve(stream));
            }
        })?;
        Ok(broker)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn serve(self: Arc<Self>, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut lines = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut first = String::new();
        if lines.read_line(&mut first).is_err() || !self.authorized(&first) {
            return; // dropped: no reply, nothing learned
        }
        let _ = stream.set_read_timeout(None);
        let out = Arc::new(Mutex::new(stream));
        let watched: Arc<Mutex<HashSet<String>>> = Default::default();
        for line in lines.lines() {
            let Ok(line) = line else { break };
            let Ok(req) = serde_json::from_str::<Value>(&line) else { break };
            let (b, out, watched) = (self.clone(), out.clone(), watched.clone());
            std::thread::spawn(move || {
                let answer = b.call(&req, &out, &watched);
                send(&out, &match answer {
                    Ok(result) => json!({ "id": req["id"], "result": result }),
                    Err(error) => json!({ "id": req["id"], "error": error }),
                });
            });
        }
    }

    fn authorized(&self, line: &str) -> bool {
        let got = serde_json::from_str::<Value>(line).ok().and_then(|v| v["token"].as_str().map(str::to_owned)).unwrap_or_default();
        // Constant time over the token's length.
        got.len() == self.token.len() && got.bytes().zip(self.token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }

    fn call(&self, req: &Value, out: &Arc<Mutex<UnixStream>>, watched: &Arc<Mutex<HashSet<String>>>) -> Result<Value, String> {
        let method = req["method"].as_str().ok_or("no method")?;
        if refused(method) {
            return Err(format!("{method} is not available to agents"));
        }
        let block = req["block"].as_str().ok_or("no block")?;
        let pipe = (self.pipe)().filter(|p| !p.is_dead()).ok_or("no browser host is running")?;
        let session = self.session(block, &pipe)?;
        // The first use of a block on this connection: its events follow.
        if watched.lock().unwrap().insert(block.to_owned()) {
            let (rx, out, block) = (pipe.subscribe(Some(&session)), out.clone(), block.to_owned());
            std::thread::spawn(move || {
                for ev in rx {
                    if !send(&out, &json!({ "block": block, "method": ev["method"], "params": ev["params"] })) {
                        break;
                    }
                }
            });
        }
        pipe.call(Some(&session), method, req["params"].clone(), CALL_TIMEOUT)
    }

    /// The block's flatten session on this pipe, attaching once (again after a restart or a new target).
    fn session(&self, block: &str, pipe: &Arc<CdpPipe>) -> Result<String, String> {
        let target = (self.target_of)(block).ok_or_else(|| format!("no browser block {block:?}"))?;
        let key = Arc::as_ptr(pipe) as usize;
        if let Some((t, s, p)) = self.sessions.lock().unwrap().get(block)
            && *t == target
            && *p == key
        {
            return Ok(s.clone());
        }
        let r = pipe.call(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true }), CALL_TIMEOUT)?;
        let session = r["sessionId"].as_str().ok_or("attach gave no session")?.to_owned();
        self.sessions.lock().unwrap().insert(block.to_owned(), (target, session.clone(), key));
        Ok(session)
    }
}

fn send(out: &Arc<Mutex<UnixStream>>, msg: &Value) -> bool {
    let mut line = msg.to_string();
    line.push('\n');
    out.lock().unwrap().write_all(line.as_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_level_and_profile_methods_are_refused() {
        for m in ["Target.createTarget", "Target.attachToTarget", "Browser.close", "Storage.getCookies", "Network.getAllCookies", "Network.setCookie", "SystemInfo.getInfo", "IO.read"] {
            assert!(refused(m), "{m}");
        }
        for m in ["Runtime.evaluate", "Page.navigate", "Page.captureScreenshot", "DOM.getDocument", "Input.dispatchMouseEvent", "Target.getTargetInfo", "Network.enable"] {
            assert!(!refused(m), "{m}");
        }
    }

    fn line(s: &mut UnixStream, v: Value) {
        s.write_all(format!("{v}\n").as_bytes()).unwrap();
    }

    fn read(s: &UnixStream) -> Option<Value> {
        let mut l = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut l).ok()?;
        serde_json::from_str(&l).ok()
    }

    #[test]
    fn the_socket_needs_the_token_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        // Short: macOS caps socket paths at 103 bytes.
        let dir = PathBuf::from(format!("/tmp/hbb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = Broker::start(dir.clone(), Arc::new(|| None), Arc::new(|_| Some("T".into()))).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&Broker::socket(&dir)), mode(&Broker::token_file(&dir))), (0o700, 0o600, 0o600));
        let token = std::fs::read_to_string(Broker::token_file(&dir)).unwrap();
        assert_eq!(token.len(), 64);
        // No token, a wrong one, or a websocket upgrade: the connection closes without a reply.
        for first in ["{}".to_string(), json!({ "token": "0".repeat(64) }).to_string(), "GET / HTTP/1.1".to_string()] {
            let mut s = UnixStream::connect(Broker::socket(&dir)).unwrap();
            s.write_all(format!("{first}\nUpgrade: websocket\n\n{}\n", json!({ "id": 1, "block": "b1", "method": "Runtime.evaluate" })).as_bytes()).unwrap();
            assert_eq!(read(&s), None, "refused: {first}");
        }
        // With the token: answered (no host yet), and refused methods never reach a host.
        let mut s = UnixStream::connect(Broker::socket(&dir)).unwrap();
        line(&mut s, json!({ "token": token }));
        line(&mut s, json!({ "id": 7, "block": "b1", "method": "Runtime.evaluate", "params": {} }));
        assert_eq!(read(&s), Some(json!({ "id": 7, "error": "no browser host is running" })));
        line(&mut s, json!({ "id": 8, "block": "b1", "method": "Storage.getCookies", "params": {} }));
        assert_eq!(read(&s), Some(json!({ "id": 8, "error": "Storage.getCookies is not available to agents" })));
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
