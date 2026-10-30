// herdr-browser daemon: holds one CDP connection per browser block so agent commands keep their
// console and network logs between CLI calls. Finds the host through $HERDR_APP_HOME/browser/cdp.json
// ({ "port": N, "targets": { "<block id>": "<cdp target id>" } }, written by the app, src/browser/agent.rs).
// Serves POST /_agent on 127.0.0.1 and writes the port to <dir>/agent.port. Never starts a browser itself.
//
// Backends: the app (default): open runs `herdr-app do browser.open`, the app adds the block to cdp.json.
// HERDR_BROWSER_BACKEND=standin: a plain headless Chrome stands in for the CEF host (tests); open and close
// make targets over CDP HTTP and this daemon writes their ids into cdp.json itself.
import { createServer } from "node:http";
import { execFile } from "node:child_process";
import { readFileSync, writeFileSync, renameSync, rmSync } from "node:fs";
import { join } from "node:path";
import register from "./agent.mjs";
import { browserDir } from "./dir.mjs";


const dir = browserDir();
const standin = process.env.HERDR_BROWSER_BACKEND === "standin";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const readCdp = () => { try { return JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")); } catch { return null; } };
const writeCdp = (c) => { const p = join(dir, "cdp.json"); writeFileSync(p + ".tmp", JSON.stringify(c)); renameSync(p + ".tmp", p); };
const endpoint = () => { const c = readCdp(); if (!c?.port) throw new Error(`no browser host: ${join(dir, "cdp.json")} has no port`); return `127.0.0.1:${c.port}`; };

const sessions = new Map(); // block id -> Promise<session>
const events = new Map(); // CDP method -> [fn(params, session)]
const openHooks = [];
const routes = new Map();

function attach(id, targetId) {
  const p = new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://${endpoint()}/devtools/page/${targetId}`);
    let n = 0; const waiting = new Map();
    const s = { id, targetId, ws, state: { url: "", title: "", loading: false }, view: null };
    s.call = (method, params = {}) => new Promise((ok, fail) => {
      if (ws.readyState !== WebSocket.OPEN) return fail(new Error(`tab ${id} is gone`));
      const mid = ++n; waiting.set(mid, { ok, fail }); ws.send(JSON.stringify({ id: mid, method, params }));
    });
    s.send = (method, params) => s.call(method, params).catch(() => {});
    const refresh = () => s.call("Runtime.evaluate", { expression: "[location.href, document.title, document.readyState, innerWidth, innerHeight]", returnByValue: true })
      .then((r) => { const [url, title, ready, w, h] = r?.result?.value ?? []; if (url) Object.assign(s.state, { url, title, loading: ready !== "complete" }); s.view = { w, h }; }).catch(() => {});
    ws.onmessage = (m) => {
      const msg = JSON.parse(m.data);
      if (msg.id) { const w = waiting.get(msg.id); waiting.delete(msg.id); if (msg.error) w?.fail(new Error(msg.error.message)); else w?.ok(msg.result); return; }
      if (msg.method === "Page.frameNavigated" && !msg.params.frame.parentId) s.state.url = msg.params.frame.url;
      if (msg.method === "Page.frameStartedLoading") s.state.loading = true;
      if (msg.method === "Page.loadEventFired" || msg.method === "Page.frameStoppedLoading") { s.state.loading = false; refresh(); }
      for (const fn of events.get(msg.method) ?? []) fn(msg.params, s);
    };
    ws.onerror = () => reject(new Error(`cannot reach tab ${id} (target ${targetId})`));
    ws.onclose = () => { for (const w of waiting.values()) w.fail(new Error(`tab ${id} closed`)); if (sessions.get(id) === p) sessions.delete(id); };
    ws.onopen = async () => {
      await s.call("Page.enable");
      for (const fn of openHooks) fn(s);
      await refresh();
      resolve(s);
    };
  });
  p.catch(() => sessions.delete(id));
  sessions.set(id, p);
  return p;
}

// Follow cdp.json: attach new blocks, drop closed ones.
async function sync() {
  const c = readCdp(), want = c?.targets ?? {};
  for (const [id, p] of sessions) {
    const s = await p.catch(() => null);
    if (!s || want[id] !== s.targetId) { s?.ws.close(); sessions.delete(id); }
  }
  for (const [id, targetId] of Object.entries(want)) if (!sessions.has(id)) await attach(id, targetId).catch(() => {});
}

const targets = async () => (await (await fetch(`http://${endpoint()}/json/list`)).json());

async function openTab(id) {
  if (standin) {
    const t = await (await fetch(`http://${endpoint()}/json/new?about:blank`, { method: "PUT" })).json();
    const c = readCdp(); c.targets = { ...c.targets, [id]: t.id }; writeCdp(c);
  } else {
    const out = await new Promise((ok, fail) => execFile(process.env.HERDR_APP_BIN || "herdr-app", ["do", "browser.open", "--url", "about:blank"],
      { encoding: "utf8" }, (e, o) => (e ? fail(e) : ok(o))));
    id = JSON.parse(out).id ?? id;
  }
  for (let i = 0; i < 100 && !readCdp()?.targets?.[id]; i++) await sleep(100);
  await sync();
  const p = sessions.get(id);
  if (!p) throw new Error(`browser block ${id} did not appear in cdp.json`);
  return p;
}

async function closeTab(id) {
  const s = await sessions.get(id);
  if (standin) {
    await fetch(`http://${endpoint()}/json/close/${s.targetId}`);
    const c = readCdp(); delete c.targets[id]; writeCdp(c);
  } else await new Promise((ok, fail) => execFile(process.env.HERDR_APP_BIN || "herdr-app", ["do", "browser.close", "--id", id], (e) => (e ? fail(e) : ok())));
  s.ws.close(); sessions.delete(id);
}

register({
  sessions, dir, openTab, closeTab, targets,
  onEvent: (method, fn) => events.set(method, [...(events.get(method) ?? []), fn]),
  onOpen: (fn) => openHooks.push(fn),
  route: (method, path, fn) => routes.set(`${method} ${path}`, fn),
});

let last = Date.now();
const server = createServer(async (req, res) => {
  last = Date.now();
  let body = ""; for await (const c of req) body += c;
  const fn = routes.get(`${req.method} ${req.url}`);
  if (!fn) { res.statusCode = 404; return res.end(); }
  await sync().catch(() => {});
  await fn(req, res, body);
});
server.listen(0, "127.0.0.1", () => writeFileSync(join(dir, "agent.port"), String(server.address().port)));
const stop = () => { rmSync(join(dir, "agent.port"), { force: true }); process.exit(0); };
process.on("SIGTERM", stop); process.on("SIGINT", stop);
// Leave when the host is gone or nobody has asked for 30 minutes.
setInterval(() => { if (!readCdp() || Date.now() - last > 30 * 60e3) stop(); }, 5000).unref();
