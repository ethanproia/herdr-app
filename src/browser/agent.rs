//! Part 5 (LAB-423): `herdr-app do browser.*` and the CDP port file.
//!
//! Commands resolve a browser (block id, a pane's block, else the focused browser), check it is
//! live in the `BrowserRegistry` and send through its `HostLink`. Commands the host answers later
//! (eval, screenshot, find) park their reply under a request id until `on_host` sees the answer.
//! `browser.open` needs a new layout block, which the window makes (part 1): `run` hands it back
//! as `Step::Open`.
//!
//! `$HERDR_APP_HOME/browser/cdp.json` = `{ "port": N, "targets": { "<block id>": "<cdp target id>" } }`
//! tells agents (scripts/herdr-browser) where the host's CDP port is and which target is which block.

use super::BrowserRegistry;
use herdr_browser_protocol::{AppToHost, BrowserId, HostToApp};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Answers one `herdr-app do` call; called once.
pub type Reply = Box<dyn FnOnce(Result<Value, String>) + Send>;

/// What the window knows that the registry does not: the focused browser and which block a pane shows.
pub struct Focus<'a> {
    pub focused: Option<BrowserId>,
    pub block_of_pane: &'a dyn Fn(&str) -> Option<BrowserId>,
}

/// The profile (CEF cache) agent-opened browsers use unless the command names another.
pub const AGENT_PROFILE: &str = "agent";

pub enum Step {
    /// The reply was sent or parked.
    Done,
    /// `browser.open`: the window makes a browser block (at `place`, default a new tab) and answers with its id.
    /// `profile` defaults to "agent": a browser an agent opens never shares the person's cookies or logins.
    Open { url: Option<String>, place: Option<String>, profile: String, reply: Reply },
    /// `browser.lock`: the window grants `agent` the block's input lease (the person's input is refused with a
    /// visible cue), or ends it (`agent` None).
    Lease { id: BrowserId, agent: Option<String>, reply: Reply },
    /// `browser.close`: the window closes block `id` (and calls `Agent::closed`) and answers.
    Close { id: BrowserId, reply: Reply },
}

#[derive(Default)]
pub struct Agent {
    next_req: u64,
    pending: HashMap<u64, Pending>,
    /// Host generation: bumped each time a new host process says Ready after an earlier one (a restart).
    /// A reply counts only for a request parked in this generation, for its browser and operation.
    generation: u64,
    ready: bool,
    /// The find waiting on each browser (FindResult carries no request id).
    finds: HashMap<BrowserId, u64>,
    pub cdp: CdpFile,
    pub grants: Grants,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eval,
    Screenshot,
    Find,
}

struct Pending {
    id: BrowserId,
    generation: u64,
    op: Op,
    reply: Reply,
}

fn arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn flag(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}

impl Agent {
    /// A new app has no host yet: a cdp.json left by an earlier run names a dead port, so it goes.
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::remove_file(dir.join("cdp.json"));
        Self { grants: Grants::new(dir.clone()), cdp: CdpFile::new(dir), ..Default::default() }
    }

    /// The browser a command acts on: `id`, else the block in `pane`, else the focused browser.
    fn target(a: &Value, reg: &dyn BrowserRegistry, focus: &Focus) -> Result<BrowserId, String> {
        let id = match (arg(a, "id"), arg(a, "pane")) {
            (Some(id), _) => id.to_string(),
            (None, Some(pane)) => (focus.block_of_pane)(pane).ok_or_else(|| format!("pane {pane:?} shows no browser"))?,
            (None, None) => focus.focused.clone().ok_or("no browser is focused; pass id or pane")?,
        };
        reg.state(&id).map(|_| id.clone()).ok_or_else(|| format!("no browser {id:?}"))
    }

    pub fn run(&mut self, name: &str, a: &Value, reg: &dyn BrowserRegistry, focus: &Focus, reply: Reply) -> Step {
        if name == "browser.open" {
            let profile = arg(a, "profile").unwrap_or(AGENT_PROFILE).to_string();
            return Step::Open { url: arg(a, "url").map(Into::into), place: arg(a, "place").map(Into::into), profile, reply };
        }
        let id = match Self::target(a, reg, focus) {
            Ok(id) => id,
            Err(e) => {
                reply(Err(e));
                return Step::Done;
            }
        };
        if name == "browser.close" {
            return Step::Close { id, reply };
        }
        if name == "browser.lock" {
            let agent = flag(a, "on", true).then(|| arg(a, "agent").unwrap_or("an agent").to_string());
            return Step::Lease { id, agent, reply };
        }
        let link = reg.link();
        let sent = |reply: Reply, msg: AppToHost| {
            link.send(msg);
            reply(Ok(json!({ "id": id })));
        };
        match name {
            "browser.navigate" => match arg(a, "url") {
                Some(url) => sent(reply, AppToHost::Navigate { id: id.clone(), url: url.into() }),
                None => reply(Err("browser.navigate needs url".into())),
            },
            "browser.back" => sent(reply, AppToHost::Back { id: id.clone() }),
            "browser.forward" => sent(reply, AppToHost::Forward { id: id.clone() }),
            "browser.reload" => sent(reply, AppToHost::Reload { id: id.clone(), ignore_cache: flag(a, "ignore_cache", false) }),
            "browser.stop" => sent(reply, AppToHost::Stop { id: id.clone() }),
            "browser.devtools" => sent(reply, AppToHost::DevTools { id: id.clone(), open: flag(a, "open", true) }),
            "browser.eval" => match arg(a, "expr") {
                Some(expr) => {
                    let req = self.park(&id, Op::Eval, reply);
                    link.send(AppToHost::Eval { id, req, expr: expr.into() });
                }
                None => reply(Err("browser.eval needs expr".into())),
            },
            "browser.screenshot" => {
                let path = arg(a, "path").map(PathBuf::from).unwrap_or_else(|| self.cdp.dir.join("shots").join(format!("{id}-{}.png", now_ms())));
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let path = path.to_string_lossy().into_owned();
                let req = self.park(&id, Op::Screenshot, Box::new({
                    let path = path.clone();
                    move |r: Result<Value, String>| reply(r.map(|_| json!({ "path": path })))
                }));
                link.send(AppToHost::Screenshot { id, req, path });
            }
            "browser.find" => match arg(a, "text") {
                Some(text) => {
                    let req = self.park(&id, Op::Find, reply);
                    if let Some(old) = self.finds.insert(id.clone(), req) {
                        self.settle(old, Err("a newer find replaced this one".into()));
                    }
                    link.send(AppToHost::Find { id, text: text.into(), forward: flag(a, "forward", true), match_case: flag(a, "match_case", false), find_next: flag(a, "next", false) });
                }
                None => {
                    link.send(AppToHost::StopFind { id: id.clone(), clear_selection: true });
                    reply(Ok(json!({ "id": id, "stopped": true })));
                }
            },
            other => reply(Err(format!("unknown browser command {other}"))),
        }
        Step::Done
    }

    fn park(&mut self, id: &BrowserId, op: Op, reply: Reply) -> u64 {
        self.next_req += 1;
        self.pending.insert(self.next_req, Pending { id: id.clone(), generation: self.generation, op, reply });
        self.next_req
    }

    /// A host reply: answers request `req` only when it was parked for this browser, this operation and this
    /// host generation; anything else (a forged or stale reply) is dropped and the request keeps waiting.
    fn answer(&mut self, req: u64, id: &BrowserId, op: Op, r: Result<Value, String>) {
        match self.pending.get(&req) {
            Some(p) if &p.id == id && p.op == op && p.generation == self.generation => self.settle(req, r),
            Some(_) => tracing::warn!("dropped a host reply for request {req} that does not match its browser, operation or host"),
            None => {}
        }
    }

    /// Ends request `req` from the app side (failures, replacements).
    fn settle(&mut self, req: u64, r: Result<Value, String>) {
        if let Some(p) = self.pending.remove(&req) {
            (p.reply)(r);
        }
    }

    /// Every host message passes here: answers parked commands and keeps cdp.json current.
    pub fn on_host(&mut self, msg: &HostToApp) {
        match msg {
            HostToApp::Ready { cdp_port, .. } => {
                if std::mem::replace(&mut self.ready, true) {
                    self.new_generation("the browser host restarted");
                }
                self.cdp.set_port(*cdp_port)
            }
            HostToApp::CdpTarget { id, target_id } => self.cdp.set_target(id, target_id),
            HostToApp::EvalResult { id, req, ok: true, value } => self.answer(*req, id, Op::Eval, Ok(json!({ "value": value }))),
            HostToApp::EvalResult { id, req, value, .. } => self.answer(*req, id, Op::Eval, Err(value.as_str().map(Into::into).unwrap_or_else(|| value.to_string()))),
            HostToApp::ScreenshotDone { id, req, ok: true, .. } => self.answer(*req, id, Op::Screenshot, Ok(Value::Null)),
            HostToApp::ScreenshotDone { id, req, error, .. } => self.answer(*req, id, Op::Screenshot, Err(error.clone().unwrap_or_else(|| "screenshot failed".into()))),
            HostToApp::FindResult { id, count, active, final_update: true } => {
                if let Some(req) = self.finds.remove(id) {
                    self.answer(req, id, Op::Find, Ok(json!({ "id": id, "count": count, "active": active })));
                }
            }
            // The whole host died: what it owed will never come, and its late replies must not count.
            HostToApp::Crashed { id: None, reason } => {
                self.ready = false;
                self.new_generation(&format!("browser crashed: {reason}"));
            }
            HostToApp::Crashed { id, reason } => self.fail_all(id.as_ref(), &format!("browser crashed: {reason}")),
            // HERDR: browser block. An error carries no request id: what waits on that browser
            // (or on any, for an error without one) fails now instead of at the reply timeout.
            HostToApp::Error { id, message } => self.fail_all(id.as_ref(), message),
            _ => {}
        }
    }

    /// A browser block closed: fail what waits on it and drop it from cdp.json.
    pub fn closed(&mut self, id: &BrowserId) {
        self.fail_all(Some(id), "the browser closed");
        self.cdp.remove(id);
        self.grants.revoke_all(id);
    }

    fn fail_all(&mut self, id: Option<&BrowserId>, why: &str) {
        let reqs: Vec<u64> = self.pending.iter().filter(|(_, p)| id.is_none_or(|id| &p.id == id)).map(|(r, _)| *r).collect();
        for req in reqs {
            self.settle(req, Err(why.into()));
        }
        self.finds.retain(|b, _| id.is_some_and(|id| b != id));
    }

    fn new_generation(&mut self, why: &str) {
        self.fail_all(None, why);
        self.generation += 1;
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// `$HERDR_APP_HOME/browser/cdp.json`, rewritten (atomically) on every change; removed when the host has no port.
#[derive(Default)]
pub struct CdpFile {
    dir: PathBuf,
    port: Option<u16>,
    targets: BTreeMap<BrowserId, String>,
}

impl CdpFile {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, ..Default::default() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("cdp.json")
    }

    pub fn set_port(&mut self, port: Option<u16>) {
        self.port = port;
        if port.is_none() {
            self.targets.clear();
        }
        self.write();
    }

    /// The host reported which CDP target shows block `id`.
    pub fn set_target(&mut self, id: &BrowserId, target: &str) {
        self.targets.insert(id.clone(), target.into());
        self.write();
    }

    pub fn remove(&mut self, id: &BrowserId) {
        if self.targets.remove(id).is_some() {
            self.write();
        }
    }

    fn write(&self) {
        let path = self.path();
        let Some(port) = self.port else {
            let _ = std::fs::remove_file(&path);
            return;
        };
        let body = json!({ "port": port, "targets": self.targets }).to_string();
        if let Err(e) = write_atomic(&path, body.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", path.display());
        }
    }
}

/// What the person lets agents do in one browser block. Only the block's own UI calls `Grants::set` (there is no
/// `herdr-app do` command for it), so an agent cannot grant itself anything through the agent tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Drive a block the person opened (agents always drive the ones they opened).
    Control,
    /// Raw CDP on the block (still limited to the allowed methods in scripts/herdr-browser/agent.mjs).
    Cdp,
    /// Screenshots of a page the person marked sensitive.
    Capture,
}

impl Grant {
    fn key(self) -> &'static str {
        match self {
            Grant::Control => "control",
            Grant::Cdp => "cdp",
            Grant::Capture => "capture",
        }
    }
}

/// `$HERDR_APP_HOME/browser/grants.json` = `{ "<block id>": { "control": bool, "cdp": bool, "capture": bool } }`,
/// 0600, app-owned; the agent daemon reads it on every call.
#[derive(Default)]
pub struct Grants {
    dir: PathBuf,
    by_block: BTreeMap<BrowserId, BTreeMap<&'static str, bool>>,
}

impl Grants {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, ..Default::default() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("grants.json")
    }

    pub fn get(&self, id: &BrowserId, grant: Grant) -> bool {
        self.by_block.get(id).and_then(|g| g.get(grant.key())).copied().unwrap_or(false)
    }

    /// The person turned a grant on or off in the block.
    pub fn set(&mut self, id: &BrowserId, grant: Grant, on: bool) {
        self.by_block.entry(id.clone()).or_default().insert(grant.key(), on);
        self.write();
    }

    pub fn revoke_all(&mut self, id: &BrowserId) {
        if self.by_block.remove(id).is_some() {
            self.write();
        }
    }

    fn write(&self) {
        let path = self.path();
        if let Err(e) = write_atomic(&path, json!(self.by_block).to_string().as_bytes()) {
            tracing::warn!("cannot write {}: {e}", path.display());
        }
    }
}

/// Writes owner-only (0600 file, 0700 dir): these files name the CDP port and what agents may do.
fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        std::io::Write::write_all(&mut f, body)?;
    }
    #[cfg(not(unix))]
    {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, body)?;
    }
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::super::{BrowserState, HostLink};
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Link(Mutex<Vec<AppToHost>>);
    impl HostLink for Link {
        fn send(&self, msg: AppToHost) {
            self.0.lock().unwrap().push(msg);
        }
    }

    struct Reg {
        live: Vec<&'static str>,
        link: Link,
    }
    impl BrowserRegistry for Reg {
        fn state(&self, id: &BrowserId) -> Option<BrowserState> {
            self.live.contains(&id.as_str()).then(BrowserState::default)
        }
        fn link(&self) -> &dyn HostLink {
            &self.link
        }
    }

    fn reg() -> Reg {
        Reg { live: vec!["b1", "b2"], link: Link::default() }
    }

    type Got = Arc<Mutex<Option<Result<Value, String>>>>;
    fn catch() -> (Got, Reply) {
        let got: Got = Default::default();
        let g = got.clone();
        (got, Box::new(move |r| *g.lock().unwrap() = Some(r)))
    }

    fn run(agent: &mut Agent, reg: &Reg, name: &str, a: Value) -> Option<Result<Value, String>> {
        let pane = |p: &str| (p == "p2").then(|| "b2".to_string());
        let focus = Focus { focused: Some("b1".into()), block_of_pane: &pane };
        let (got, reply) = catch();
        assert!(matches!(agent.run(name, &a, reg, &focus, reply), Step::Done));
        got.lock().unwrap().take()
    }

    fn sent(reg: &Reg) -> Vec<AppToHost> {
        std::mem::take(&mut *reg.link.0.lock().unwrap())
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn agent() -> (Tmp, Agent) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = Tmp(std::env::temp_dir().join(format!("herdr-browser-agent-{}-{n}", std::process::id())));
        let a = Agent::new(dir.path().join("browser"));
        (dir, a)
    }

    #[test]
    fn resolves_id_then_pane_then_focus() {
        let (_d, mut a) = agent();
        let reg = reg();
        assert_eq!(run(&mut a, &reg, "browser.back", json!({})), Some(Ok(json!({ "id": "b1" }))));
        assert_eq!(run(&mut a, &reg, "browser.back", json!({ "pane": "p2" })), Some(Ok(json!({ "id": "b2" }))));
        assert_eq!(run(&mut a, &reg, "browser.back", json!({ "id": "b2", "pane": "nope" })), Some(Ok(json!({ "id": "b2" }))));
        assert_eq!(sent(&reg), vec![AppToHost::Back { id: "b1".into() }, AppToHost::Back { id: "b2".into() }, AppToHost::Back { id: "b2".into() }]);
        assert_eq!(run(&mut a, &reg, "browser.back", json!({ "id": "gone" })), Some(Err("no browser \"gone\"".into())));
        assert_eq!(run(&mut a, &reg, "browser.back", json!({ "pane": "p1" })), Some(Err("pane \"p1\" shows no browser".into())));
        let none = |_: &str| None;
        let (got, reply) = catch();
        a.run("browser.stop", &json!({}), &reg, &Focus { focused: None, block_of_pane: &none }, reply);
        assert!(got.lock().unwrap().take().unwrap().unwrap_err().contains("no browser is focused"));
        assert!(sent(&reg).is_empty());
    }

    #[test]
    fn fire_and_forget_commands_send_their_message() {
        let (_d, mut a) = agent();
        let reg = reg();
        for (name, args) in [
            ("browser.navigate", json!({ "url": "https://x.test" })),
            ("browser.forward", json!({})),
            ("browser.reload", json!({ "ignore_cache": true })),
            ("browser.stop", json!({})),
            ("browser.devtools", json!({})),
            ("browser.devtools", json!({ "open": false })),
        ] {
            assert_eq!(run(&mut a, &reg, name, args), Some(Ok(json!({ "id": "b1" }))), "{name}");
        }
        let id = || "b1".to_string();
        assert_eq!(sent(&reg), vec![
            AppToHost::Navigate { id: id(), url: "https://x.test".into() },
            AppToHost::Forward { id: id() },
            AppToHost::Reload { id: id(), ignore_cache: true },
            AppToHost::Stop { id: id() },
            AppToHost::DevTools { id: id(), open: true },
            AppToHost::DevTools { id: id(), open: false },
        ]);
        assert_eq!(run(&mut a, &reg, "browser.navigate", json!({})), Some(Err("browser.navigate needs url".into())));
    }

    #[test]
    fn eval_waits_for_the_host() {
        let (_d, mut a) = agent();
        let reg = reg();
        assert_eq!(run(&mut a, &reg, "browser.eval", json!({ "expr": "1+1" })), None, "parked until the host answers");
        let [AppToHost::Eval { id, req, expr }] = &sent(&reg)[..] else { panic!() };
        assert_eq!((id.as_str(), expr.as_str()), ("b1", "1+1"));
        let pane = |_: &str| None;
        let (got, reply) = catch();
        a.run("browser.eval", &json!({ "expr": "boom()" }), &reg, &Focus { focused: Some("b2".into()), block_of_pane: &pane }, reply);
        let [AppToHost::Eval { req: req2, .. }] = &sent(&reg)[..] else { panic!() };
        a.on_host(&HostToApp::EvalResult { id: "b2".into(), req: *req2, ok: false, value: json!("ReferenceError: boom") });
        assert_eq!(got.lock().unwrap().take(), Some(Err("ReferenceError: boom".into())));
        // The first one is still waiting; a crash of b1 fails it.
        a.on_host(&HostToApp::Crashed { id: Some("b1".into()), reason: "gpu".into() });
        assert!(a.pending.is_empty(), "{req}");
    }

    #[test]
    fn a_host_error_fails_what_waits_on_that_browser_at_once() {
        let (_d, mut a) = agent();
        let reg = reg();
        let pane = |_: &str| None;
        let (got1, r1) = catch();
        let (got2, r2) = catch();
        a.run("browser.eval", &json!({ "expr": "1" }), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, r1);
        a.run("browser.eval", &json!({ "expr": "2" }), &reg, &Focus { focused: Some("b2".into()), block_of_pane: &pane }, r2);
        a.on_host(&HostToApp::Error { id: Some("b1".into()), message: "no browser b1".into() });
        assert_eq!(got1.lock().unwrap().take(), Some(Err("no browser b1".into())));
        assert_eq!(got2.lock().unwrap().take(), None, "b2 still waits");
        a.on_host(&HostToApp::Error { id: None, message: "bad message".into() });
        assert_eq!(got2.lock().unwrap().take(), Some(Err("bad message".into())));
        assert!(a.pending.is_empty());
    }

    #[test]
    fn eval_result_answers_the_parked_reply() {
        let (_d, mut a) = agent();
        let reg = reg();
        let pane = |_: &str| None;
        let (got, reply) = catch();
        a.run("browser.eval", &json!({ "expr": "1+1" }), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, reply);
        let [AppToHost::Eval { req, .. }] = &sent(&reg)[..] else { panic!() };
        a.on_host(&HostToApp::EvalResult { id: "b1".into(), req: *req, ok: true, value: json!(2) });
        assert_eq!(got.lock().unwrap().take(), Some(Ok(json!({ "value": 2 }))));
    }

    #[test]
    fn screenshot_defaults_under_the_browser_dir() {
        let (d, mut a) = agent();
        let reg = reg();
        let pane = |_: &str| None;
        let (got, reply) = catch();
        a.run("browser.screenshot", &json!({}), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, reply);
        let [AppToHost::Screenshot { req, path, .. }] = &sent(&reg)[..] else { panic!() };
        assert!(path.starts_with(&d.path().join("browser/shots/b1-").to_string_lossy().into_owned()) && path.ends_with(".png"), "{path}");
        a.on_host(&HostToApp::ScreenshotDone { id: "b1".into(), req: *req, ok: true, error: None });
        assert_eq!(got.lock().unwrap().take(), Some(Ok(json!({ "path": path }))));
    }

    #[test]
    fn find_answers_on_the_final_update_and_no_text_stops() {
        let (_d, mut a) = agent();
        let reg = reg();
        assert_eq!(run(&mut a, &reg, "browser.find", json!({ "text": "hi", "match_case": true })), None);
        assert_eq!(sent(&reg), vec![AppToHost::Find { id: "b1".into(), text: "hi".into(), forward: true, match_case: true, find_next: false }]);
        a.on_host(&HostToApp::FindResult { id: "b1".into(), count: 3, active: 1, final_update: false });
        assert_eq!(a.pending.len(), 1);
        a.on_host(&HostToApp::FindResult { id: "b1".into(), count: 3, active: 1, final_update: true });
        assert!(a.pending.is_empty());
        assert_eq!(run(&mut a, &reg, "browser.find", json!({})), Some(Ok(json!({ "id": "b1", "stopped": true }))));
        assert_eq!(sent(&reg), vec![AppToHost::StopFind { id: "b1".into(), clear_selection: true }]);
    }

    #[test]
    fn open_goes_to_the_window() {
        let (_d, mut a) = agent();
        let reg = reg();
        let pane = |_: &str| None;
        let (_got, reply) = catch();
        let step = a.run("browser.open", &json!({ "url": "https://x.test", "place": "right" }), &reg, &Focus { focused: None, block_of_pane: &pane }, reply);
        assert!(matches!(step, Step::Open { url: Some(u), place: Some(p), profile, .. } if u == "https://x.test" && p == "right" && profile == "agent"),
            "an agent's browser gets its own profile by default");
        let (_got, reply) = catch();
        let step = a.run("browser.open", &json!({ "profile": "default" }), &reg, &Focus { focused: None, block_of_pane: &pane }, reply);
        assert!(matches!(step, Step::Open { profile, .. } if profile == "default"));
    }

    #[test]
    fn close_goes_to_the_window_for_a_live_browser() {
        let (_d, mut a) = agent();
        let reg = reg();
        let pane = |p: &str| (p == "p2").then(|| "b2".to_string());
        let (_got, reply) = catch();
        let step = a.run("browser.close", &json!({ "pane": "p2" }), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, reply);
        assert!(matches!(step, Step::Close { id, .. } if id == "b2"));
        assert_eq!(run(&mut a, &reg, "browser.close", json!({ "id": "gone" })), Some(Err("no browser \"gone\"".into())));
        assert!(sent(&reg).is_empty(), "the window closes it, not the host link");
    }

    /// A reply counts only for its own browser, operation and host generation (forged-eval hole, app side).
    #[test]
    fn mismatched_or_stale_replies_are_dropped() {
        let (_d, mut a) = agent();
        let reg = reg();
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: None });
        assert_eq!(run(&mut a, &reg, "browser.eval", json!({ "expr": "1" })), None);
        let [AppToHost::Eval { req, .. }] = &sent(&reg)[..] else { panic!() };
        let req = *req;
        // Another browser's answer under this request id: dropped, still waiting.
        a.on_host(&HostToApp::EvalResult { id: "b2".into(), req, ok: true, value: json!("forged") });
        // The right browser but another operation: dropped.
        a.on_host(&HostToApp::ScreenshotDone { id: "b1".into(), req, ok: true, error: None });
        assert_eq!(a.pending.len(), 1, "mismatched replies must not answer");
        // A matching reply answers.
        let pane = |_: &str| None;
        let (got, reply) = catch();
        a.run("browser.eval", &json!({ "expr": "2" }), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, reply);
        let [AppToHost::Eval { req: req2, .. }] = &sent(&reg)[..] else { panic!() };
        a.on_host(&HostToApp::EvalResult { id: "b1".into(), req: *req2, ok: true, value: json!(2) });
        assert_eq!(got.lock().unwrap().take(), Some(Ok(json!({ "value": 2 }))));
        // The host restarts: the old request fails, and its late reply (stale generation) answers nothing.
        let (got, reply) = catch();
        a.run("browser.eval", &json!({ "expr": "3" }), &reg, &Focus { focused: Some("b1".into()), block_of_pane: &pane }, reply);
        let [AppToHost::Eval { req: req3, .. }] = &sent(&reg)[..] else { panic!() };
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: None });
        assert_eq!(got.lock().unwrap().take(), Some(Err("the browser host restarted".into())));
        a.on_host(&HostToApp::EvalResult { id: "b1".into(), req: *req3, ok: true, value: json!("late") });
        assert!(a.pending.is_empty());
        // A request parked in the old generation that somehow survived is not answered by the new host either.
        a.pending.insert(99, Pending { id: "b1".into(), generation: a.generation - 1, op: Op::Eval, reply: Box::new(|_| panic!("stale answered")) });
        a.on_host(&HostToApp::EvalResult { id: "b1".into(), req: 99, ok: true, value: json!("stale") });
        assert!(a.pending.contains_key(&99));
        let _ = req;
    }

    #[test]
    fn lock_goes_to_the_window_as_a_lease() {
        let (_d, mut a) = agent();
        let reg = reg();
        let pane = |_: &str| None;
        let focus = Focus { focused: Some("b1".into()), block_of_pane: &pane };
        let (_g, reply) = catch();
        assert!(matches!(a.run("browser.lock", &json!({ "agent": "claude" }), &reg, &focus, reply),
            Step::Lease { id, agent: Some(who), .. } if id == "b1" && who == "claude"));
        let (_g, reply) = catch();
        assert!(matches!(a.run("browser.lock", &json!({ "on": false }), &reg, &focus, reply), Step::Lease { agent: None, .. }));
        assert!(sent(&reg).is_empty());
    }

    #[test]
    fn grants_are_app_owned_and_private() {
        let (d, mut a) = agent();
        let id: BrowserId = "b1".into();
        assert!(!a.grants.get(&id, Grant::Control), "nothing is granted by default");
        a.grants.set(&id, Grant::Control, true);
        a.grants.set(&id, Grant::Cdp, false);
        let path = d.path().join("browser/grants.json");
        let read = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read(), json!({ "b1": { "cdp": false, "control": true } }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        a.closed(&id);
        assert_eq!(read(), json!({}), "a closed block keeps no grants");
        // No `herdr-app do` command can grant: the agent surface has none.
        assert!(crate::commands::COMMANDS.iter().all(|c| !c.name.contains("allow") && !c.name.contains("grant")));
    }

    #[test]
    fn a_new_app_drops_a_stale_cdp_file() {
        let (d, _a) = agent();
        let path = d.path().join("browser/cdp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"port":55580,"targets":{}}"#).unwrap();
        let _again = Agent::new(d.path().join("browser"));
        assert!(!path.exists(), "a port from an earlier run must not be published");
    }

    #[test]
    fn cdp_file_follows_the_host() {
        let (d, mut a) = agent();
        let path = d.path().join("browser/cdp.json");
        let read = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: Some(9333) });
        assert_eq!(read(), json!({ "port": 9333, "targets": {} }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "cdp.json is owner-only");
        }
        a.on_host(&HostToApp::CdpTarget { id: "b1".into(), target_id: "T1".into() });
        a.on_host(&HostToApp::CdpTarget { id: "b2".into(), target_id: "T2".into() });
        assert_eq!(read(), json!({ "port": 9333, "targets": { "b1": "T1", "b2": "T2" } }));
        a.closed(&"b1".into());
        assert_eq!(read(), json!({ "port": 9333, "targets": { "b2": "T2" } }));
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: None });
        assert!(!path.exists(), "no port, no file");
    }
}
