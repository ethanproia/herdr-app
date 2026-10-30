//! CDP over a pipe (docs/browser-cdp-pipe.md, step 3): the app is the only CDP client of the
//! host. Chromium's `remote-debugging-pipe` reads CDP from fd 3 and writes to fd 4,
//! NUL-terminated JSON; `spawn_with_pipe` hands a child those two fds and keeps the other ends
//! (close-on-exec) in a `CdpPipe`. One reader thread splits on NUL and dispatches replies by `id`
//! and events by `sessionId`. The broker (fork-agent) is the only caller.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

type Reply = mpsc::Sender<Result<Value, String>>;

/// Why calls fail once the host's end of the pipe is gone (EOF on read or EPIPE on write).
pub const STOPPED: &str = "the browser host stopped";

/// One host's CDP pipe. Dropped (or failed) with its host: calls in flight get an error.
pub struct CdpPipe {
    to_host: Mutex<File>,
    pending: Mutex<HashMap<u64, Reply>>,
    /// Event listeners by session (None: browser-level events).
    subs: Mutex<HashMap<Option<String>, Vec<mpsc::Sender<Value>>>>,
    next: AtomicU64,
    /// Why the pipe is over, once it is.
    dead: Mutex<Option<String>>,
}

impl CdpPipe {
    /// Wrap the app's ends: `to_host` writes (the child's fd 3), `from_host` reads (its fd 4).
    pub fn new(to_host: File, from_host: File) -> Arc<CdpPipe> {
        let pipe = Arc::new(CdpPipe { to_host: Mutex::new(to_host), pending: Mutex::new(HashMap::new()), subs: Mutex::new(HashMap::new()), next: AtomicU64::new(1), dead: Mutex::new(None) });
        let weak = Arc::downgrade(&pipe);
        std::thread::Builder::new()
            .name("browser-cdp-read".into())
            .spawn(move || read(from_host, weak))
            .expect("cdp reader thread");
        pipe
    }

    /// One CDP command (on a page session, or browser-level with None); its `result`, or the
    /// error's message. Fails at once when the pipe is over, or after `timeout`.
    pub fn call(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // Checked after registering, so a pipe that dies now still fails this call (by `fail`).
        if let Some(why) = self.dead.lock().unwrap().clone() {
            self.pending.lock().unwrap().remove(&id);
            return Err(why);
        }
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let mut bytes = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
        bytes.push(0);
        if let Err(e) = self.to_host.lock().unwrap().write_all(&bytes) {
            // The host's end is gone (EPIPE): the same as the reader seeing EOF, whichever
            // noticed first. A restart that already failed the pipe keeps its own reason.
            tracing::debug!("cdp pipe write: {e}");
            self.fail(STOPPED);
            self.pending.lock().unwrap().remove(&id);
            return Err(self.dead.lock().unwrap().clone().unwrap_or_else(|| STOPPED.into()));
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method}: no answer in {} ms", timeout.as_millis()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.dead.lock().unwrap().clone().unwrap_or_else(|| STOPPED.into())),
        }
    }

    /// Events for `session` (None: browser-level), each the whole message (`method`, `params`).
    pub fn subscribe(&self, session: Option<&str>) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        self.subs.lock().unwrap().entry(session.map(str::to_owned)).or_default().push(tx);
        rx
    }

    pub fn is_dead(&self) -> bool {
        self.dead.lock().unwrap().is_some()
    }

    /// The pipe is over (the host stopped or restarted): every call in flight fails with `why`,
    /// later calls too, and subscribers see their channel close.
    pub fn fail(&self, why: &str) {
        let mut dead = self.dead.lock().unwrap();
        if dead.is_none() {
            *dead = Some(why.to_owned());
        }
        drop(dead);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(why.to_owned()));
        }
        self.subs.lock().unwrap().clear();
    }

    fn dispatch(&self, msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                let r = match msg.get("error") {
                    Some(e) => Err(e.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| e.to_string())),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(r);
            }
            return;
        }
        let session = msg.get("sessionId").and_then(Value::as_str).map(str::to_owned);
        if let Some(list) = self.subs.lock().unwrap().get_mut(&session) {
            list.retain(|tx| tx.send(msg.clone()).is_ok());
        }
    }
}

impl Drop for CdpPipe {
    fn drop(&mut self) {
        self.fail("the browser host restarted");
    }
}

/// The reader: NUL-split JSON until the host closes its end (then the pipe fails).
fn read(mut from_host: File, pipe: std::sync::Weak<CdpPipe>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match from_host.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(end) = buf.iter().position(|b| *b == 0) {
            let frame: Vec<u8> = buf.drain(..=end).collect();
            let Some(p) = pipe.upgrade() else { return };
            match serde_json::from_slice::<Value>(&frame[..frame.len() - 1]) {
                Ok(msg) => p.dispatch(msg),
                Err(e) => tracing::warn!("cdp pipe: bad frame: {e}"),
            }
        }
    }
    if let Some(p) = pipe.upgrade() {
        p.fail(STOPPED);
    }
}

/// Serializes pipe creation with the spawns that could inherit a pipe end before it is
/// close-on-exec (`spawn_with_pipe`, and tests' other spawns).
pub static SPAWN: Mutex<()> = Mutex::new(());

/// Spawn `cmd` with CDP on its fds 3 (it reads) and 4 (it writes); the app's ends are
/// close-on-exec, so no other child inherits them.
#[cfg(target_os = "macos")]
pub fn spawn_with_pipe(cmd: &mut Command) -> std::io::Result<(Child, Arc<CdpPipe>)> {
    use std::os::fd::{FromRawFd, RawFd};
    use std::os::unix::process::CommandExt;
    fn pipe() -> std::io::Result<(RawFd, RawFd)> {
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        Ok((fds[0], fds[1]))
    }
    // macOS has no pipe2: close-on-exec comes a moment after pipe(). Pipes made here and the
    // spawn are serialized, and the host closes every fd above 4 before exec, so it never holds
    // another pipe's end (which would keep a dead host's pipe open).
    let _serial = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    // app -> host: the child reads `a_r` as fd 3; host -> app: the child writes `b_w` as fd 4.
    let (a_r, a_w) = pipe()?;
    let (b_r, b_w) = pipe()?;
    unsafe {
        cmd.pre_exec(move || {
            // The ends may themselves be fd 3 or 4 (a process with few files open): move both above
            // 9 first, so placing one never closes the other. dup2 clears close-on-exec on 3 and 4;
            // the loop below closes the copies.
            let r = libc::fcntl(a_r, libc::F_DUPFD, 10);
            let w = libc::fcntl(b_w, libc::F_DUPFD, 10);
            if r < 0 || w < 0 || libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let max = libc::getdtablesize().clamp(5, 65536);
            for fd in 5..max {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let child = cmd.spawn();
    // The child's ends belong to the child now (or to nobody, if it didn't start).
    unsafe {
        libc::close(a_r);
        libc::close(b_w);
    }
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(a_w);
                libc::close(b_r);
            }
            return Err(e);
        }
    };
    let (to_host, from_host) = unsafe { (File::from_raw_fd(a_w), File::from_raw_fd(b_r)) };
    Ok((child, CdpPipe::new(to_host, from_host)))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A fake host: reads NUL-framed JSON on fd 3, answers on fd 4. `Echo` answers its params;
    /// `Emit` first sends an event on the call's session; `Fail` answers an error; `Die` exits
    /// without answering. Two answers go out in one write, so the reader must split them.
    const FAKE: &str = r#"
import json, os, sys
r = os.fdopen(3, 'rb', buffering=0); w = os.fdopen(4, 'wb', buffering=0)
buf = b''
while True:
    c = r.read(65536)
    if not c: break
    buf += c
    while b'\0' in buf:
        f, buf = buf.split(b'\0', 1)
        m = json.loads(f)
        meth, s = m['method'], m.get('sessionId')
        if meth == 'Die': sys.exit(0)
        out = []
        if meth == 'Emit':
            ev = {'method': 'Page.loadEventFired', 'params': {'n': m['params'].get('n')}}
            if s: ev['sessionId'] = s
            out.append(ev)
        if meth == 'Fail':
            out.append({'id': m['id'], 'error': {'code': -32000, 'message': 'no such thing'}})
        else:
            out.append({'id': m['id'], 'result': {'method': meth, 'params': m['params'], 'session': s}})
        w.write(b''.join(json.dumps(o).encode() + b'\0' for o in out))
"#;

    fn fake() -> (Child, Arc<CdpPipe>) {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(FAKE).stdin(Stdio::null());
        spawn_with_pipe(&mut cmd).expect("fake host starts")
    }

    const T: Duration = Duration::from_secs(5);

    #[::core::prelude::v1::test]
    fn calls_round_trip_events_split_and_errors_come_back() {
        let (mut child, pipe) = fake();
        let r = pipe.call(None, "Echo", json!({ "x": 1 }), T).unwrap();
        assert_eq!(r, json!({ "method": "Echo", "params": { "x": 1 }, "session": null }));
        let r = pipe.call(Some("S1"), "Echo", json!({}), T).unwrap();
        assert_eq!(r["session"], "S1", "the session id goes with the call");
        // An event and the reply in one write: both are split out, the event to its session only.
        let s1 = pipe.subscribe(Some("S1"));
        let browser = pipe.subscribe(None);
        pipe.call(Some("S1"), "Emit", json!({ "n": 7 }), T).unwrap();
        let ev = s1.recv_timeout(T).unwrap();
        assert_eq!((ev["method"].as_str(), ev["params"]["n"].as_i64()), (Some("Page.loadEventFired"), Some(7)));
        assert!(browser.try_recv().is_err(), "a session's event is not a browser-level one");
        pipe.call(None, "Emit", json!({ "n": 8 }), T).unwrap();
        assert_eq!(browser.recv_timeout(T).unwrap()["params"]["n"], 8);
        assert_eq!(pipe.call(None, "Fail", json!({}), T).unwrap_err(), "no such thing");
        // Many calls from threads at once: each gets its own answer.
        let handles: Vec<_> = (0..16).map(|i| {
            let p = pipe.clone();
            std::thread::spawn(move || p.call(None, "Echo", json!({ "i": i }), T).unwrap()["params"]["i"].as_i64())
        }).collect();
        let mut got: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
        got.sort();
        assert_eq!(got, (0..16).collect::<Vec<_>>());
        drop(pipe);
        let _ = child.wait();
    }

    #[::core::prelude::v1::test]
    fn calls_in_flight_fail_when_the_host_dies_and_later_calls_fail_at_once() {
        let (mut child, pipe) = fake();
        let p = pipe.clone();
        let waiting = std::thread::spawn(move || p.call(None, "Die", json!({}), Duration::from_secs(30)));
        let r = waiting.join().unwrap();
        assert_eq!(r.unwrap_err(), STOPPED);
        assert!(pipe.is_dead());
        let t = std::time::Instant::now();
        // Whichever side notices first (EPIPE on write, EOF on read): the same answer.
        assert_eq!(pipe.call(None, "Echo", json!({}), T).unwrap_err(), STOPPED);
        assert!(t.elapsed() < Duration::from_millis(100), "no wait on a dead pipe");
        let _ = child.wait();
    }

    #[::core::prelude::v1::test]
    fn a_restart_fails_what_waits_on_the_old_pipe() {
        let (mut child, pipe) = fake();
        pipe.fail("the browser host restarted");
        assert_eq!(pipe.call(None, "Echo", json!({}), T).unwrap_err(), "the browser host restarted");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[::core::prelude::v1::test]
    fn the_apps_ends_do_not_leak_into_other_children() {
        let (mut child, pipe) = fake();
        // Another child (a second host, a shell) must not inherit any pipe end: no fd above 2 is a pipe.
        let out = {
            let _serial = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            Command::new("sh").arg("-c").arg("for i in 3 4 5 6 7 8 9 10 11 12 13 14 15 16; do [ -p /dev/fd/$i ] && echo $i; done; true").output().unwrap()
        };
        let leaked = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert!(leaked.is_empty(), "another child holds pipe fds {leaked:?}");
        drop(pipe);
        let _ = child.wait();
    }
}
