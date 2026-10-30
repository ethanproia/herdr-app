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

pub enum Step {
    /// The reply was sent or parked.
    Done,
    /// `browser.open`: the window makes a browser block (at `place`, default a new tab) and answers with its id.
    Open { url: Option<String>, place: Option<String>, reply: Reply },
}

#[derive(Default)]
pub struct Agent {
    next_req: u64,
    pending: HashMap<u64, (BrowserId, Reply)>,
    /// The find waiting on each browser (FindResult carries no request id).
    finds: HashMap<BrowserId, u64>,
    pub cdp: CdpFile,
}

fn arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn flag(a: &Value, k: &str, default: bool) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(default)
}

impl Agent {
    pub fn new(dir: PathBuf) -> Self {
        Self { cdp: CdpFile::new(dir), ..Default::default() }
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
            return Step::Open { url: arg(a, "url").map(Into::into), place: arg(a, "place").map(Into::into), reply };
        }
        let id = match Self::target(a, reg, focus) {
            Ok(id) => id,
            Err(e) => {
                reply(Err(e));
                return Step::Done;
            }
        };
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
                    let req = self.park(&id, reply);
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
                let req = self.park(&id, Box::new({
                    let path = path.clone();
                    move |r: Result<Value, String>| reply(r.map(|_| json!({ "path": path })))
                }));
                link.send(AppToHost::Screenshot { id, req, path });
            }
            "browser.find" => match arg(a, "text") {
                Some(text) => {
                    let req = self.park(&id, reply);
                    if let Some(old) = self.finds.insert(id.clone(), req) {
                        self.answer(old, Err("a newer find replaced this one".into()));
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

    fn park(&mut self, id: &BrowserId, reply: Reply) -> u64 {
        self.next_req += 1;
        self.pending.insert(self.next_req, (id.clone(), reply));
        self.next_req
    }

    fn answer(&mut self, req: u64, r: Result<Value, String>) {
        if let Some((_, reply)) = self.pending.remove(&req) {
            reply(r);
        }
    }

    /// Every host message passes here: answers parked commands and keeps cdp.json current.
    pub fn on_host(&mut self, msg: &HostToApp) {
        match msg {
            HostToApp::Ready { cdp_port, .. } => self.cdp.set_port(*cdp_port),
            HostToApp::EvalResult { req, ok: true, value, .. } => self.answer(*req, Ok(json!({ "value": value }))),
            HostToApp::EvalResult { req, value, .. } => self.answer(*req, Err(value.as_str().map(Into::into).unwrap_or_else(|| value.to_string()))),
            HostToApp::ScreenshotDone { req, ok: true, .. } => self.answer(*req, Ok(Value::Null)),
            HostToApp::ScreenshotDone { req, error, .. } => self.answer(*req, Err(error.clone().unwrap_or_else(|| "screenshot failed".into()))),
            HostToApp::FindResult { id, count, active, final_update: true } => {
                if let Some(req) = self.finds.remove(id) {
                    self.answer(req, Ok(json!({ "id": id, "count": count, "active": active })));
                }
            }
            HostToApp::Crashed { id, reason } => self.fail_all(id.as_ref(), &format!("browser crashed: {reason}")),
            _ => {}
        }
    }

    /// A browser block closed: fail what waits on it and drop it from cdp.json.
    pub fn closed(&mut self, id: &BrowserId) {
        self.fail_all(Some(id), "the browser closed");
        self.cdp.remove(id);
    }

    fn fail_all(&mut self, id: Option<&BrowserId>, why: &str) {
        let reqs: Vec<u64> = self.pending.iter().filter(|(_, (b, _))| id.is_none_or(|id| b == id)).map(|(r, _)| *r).collect();
        for req in reqs {
            self.answer(req, Err(why.into()));
        }
        self.finds.retain(|b, _| id.is_some_and(|id| b != id));
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

fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
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
        assert!(matches!(step, Step::Open { url: Some(u), place: Some(p), .. } if u == "https://x.test" && p == "right"));
    }

    #[test]
    fn cdp_file_follows_the_host() {
        let (d, mut a) = agent();
        let path = d.path().join("browser/cdp.json");
        let read = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: Some(9333) });
        assert_eq!(read(), json!({ "port": 9333, "targets": {} }));
        a.cdp.set_target(&"b1".into(), "T1");
        a.cdp.set_target(&"b2".into(), "T2");
        assert_eq!(read(), json!({ "port": 9333, "targets": { "b1": "T1", "b2": "T2" } }));
        a.closed(&"b1".into());
        assert_eq!(read(), json!({ "port": 9333, "targets": { "b2": "T2" } }));
        a.on_host(&HostToApp::Ready { version: 1, cef_version: "x".into(), cdp_port: None });
        assert!(!path.exists(), "no port, no file");
    }
}
