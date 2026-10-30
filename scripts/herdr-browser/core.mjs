// herdr-browser daemon: holds one CDP connection per browser block so agent commands keep their
// console and network logs between CLI calls. Finds the host through $HERDR_APP_HOME/browser/cdp.json
// ({ "port": N, "targets": { "<block id>": "<cdp target id>" } }, written by the app, src/browser/agent.rs).
// Serves POST /_agent on 127.0.0.1 and writes the port to <dir>/agent.port. Never starts a browser itself.
//
// Backends: the app (default): open runs `herdr-app do browser.open`, the app adds the block to cdp.json.
// HERDR_BROWSER_BACKEND=standin: a plain headless Chrome stands in for the CEF host (tests); open and close
// make targets over CDP HTTP and this daemon writes their ids into cdp.json itself.
import { createServer } from "node:http";
import { createConnection } from "node:net";
import { execFile } from "node:child_process";
import { readFileSync, writeFileSync, renameSync, rmSync, mkdirSync, chmodSync } from "node:fs";
import { randomBytes, timingSafeEqual } from "node:crypto";
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

// One CDP connection to a block, either way the app publishes it in cdp.json: through the broker (agent.sock, a
// token, the block's name; CDP on the app's pipe) or a page WebSocket on a TCP port (the stand-in; dev only on the app).
// Both look like a WebSocket here: send, readyState, onopen/onmessage/onerror/onclose, close.
function connectTo(id, targetId) {
  const c = readCdp();
  if (!c?.broker) return new WebSocket(`ws://${endpoint()}/devtools/page/${targetId}`);
  const t = { readyState: 0, close: () => sock.destroy() };
  const sock = createConnection(c.broker);
  let buf = "";
  sock.on("connect", () => {
    sock.write(JSON.stringify({ token: readFileSync(c.token_file, "utf8").trim() }) + "\n");
    t.readyState = WebSocket.OPEN; t.onopen?.();
  });
  sock.on("data", (d) => {
    buf += d; let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const m = JSON.parse(buf.slice(0, i)); buf = buf.slice(i + 1);
      // The broker answers errors as text and tags events with the block; the WebSocket shape has neither.
      const msg = m.id ? (m.error !== undefined ? { id: m.id, error: { message: m.error } } : { id: m.id, result: m.result }) : { method: m.method, params: m.params };
      t.onmessage?.({ data: JSON.stringify(msg) });
    }
  });
  sock.on("error", () => t.onerror?.());
  sock.on("close", () => { t.readyState = 3; t.onclose?.(); });
  t.send = (line) => { const m = JSON.parse(line); sock.write(JSON.stringify({ ...m, block: id }) + "\n"); };
  return t;
}

function attach(id, targetId) {
  const p = new Promise((resolve, reject) => {
    const ws = connectTo(id, targetId);
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

// `herdr-app do <name> <json>`; answers its JSON. HERDR_BROWSER_APP may name a wrapper (scripts/fork-run.sh <name>).
// Not HERDR_APP_BIN: fork-run.sh reads that as the app binary, so a wrapper there runs itself forever.
const app = (name, args) => new Promise((ok) => execFile(process.env.HERDR_BROWSER_APP || "herdr-app", ["do", name, JSON.stringify(args)],
  { encoding: "utf8", timeout: 30000 }, (e, out) => { try { ok(JSON.parse(out)); } catch { ok({ error: String(e?.message ?? out) }); } }));
// Every target (for the DevTools check). Through the broker there is no target list and no HTTP DevTools frontend.
const targets = async () => (readCdp()?.broker ? [] : (await (await fetch(`http://${endpoint()}/json/list`)).json()));

async function openTab(id, place = "tab") {
  if (standin) {
    const t = await (await fetch(`http://${endpoint()}/json/new?about:blank`, { method: "PUT" })).json();
    const c = readCdp(); c.targets = { ...c.targets, [id]: t.id }; writeCdp(c);
  } else {
    const r = await app("browser.open", { url: "about:blank", place });
    if (!r.id) throw new Error(`browser.open: ${r.error ?? JSON.stringify(r)}`);
    id = r.id;
    // The host makes a page on its first command (an unshown block never paints): poke it until cdp.json lists it.
    for (let i = 0; i < 100 && !readCdp()?.targets?.[id]; i++) { await app("browser.eval", { id, expr: "1" }); await sleep(100); }
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
  } else {
    const r = await app("browser.close", { id });
    if (r.error) throw new Error(`browser.close: ${r.error}`);
  }
  s.ws.close(); sessions.delete(id);
}

register({
  sessions, dir, openTab, closeTab, targets,
  // What the person sent from a block (comments, markup, a picture): the app keeps it (agent.rs).
  annotations: async (args) => { if (standin) throw new Error("annotations come from the app; the stand-in has none"); const r = await app("browser.annotations", args); if (r.error) throw new Error(`browser.annotations: ${r.error}`); return r; },
  lease: async (id, on) => { if (!standin) { const r = await app("browser.lock", { id, on, agent: process.env.HERDR_BROWSER_AGENT || "an agent" }); if (r.error) throw new Error(`browser.lock: ${r.error}`); } },
  onEvent: (method, fn) => events.set(method, [...(events.get(method) ?? []), fn]),
  onOpen: (fn) => openHooks.push(fn),
  route: (method, path, fn) => routes.set(`${method} ${path}`, fn),
});

// Only callers that can read <dir>/agent.token (0600, this user) may drive pages; a request with an Origin header
// comes from a web page (a site probing localhost) and is refused whatever it carries.
mkdirSync(dir, { recursive: true, mode: 0o700 });
const token = randomBytes(32).toString("hex");
writeFileSync(join(dir, "agent.token"), token, { mode: 0o600 });
chmodSync(join(dir, "agent.token"), 0o600);
const authorized = (req) => {
  const got = Buffer.from(String(req.headers.authorization ?? "").replace(/^Bearer /, "")), want = Buffer.from(token);
  return got.length === want.length && timingSafeEqual(got, want);
};

let last = Date.now();
const server = createServer(async (req, res) => {
  if (req.headers.origin !== undefined) { res.statusCode = 403; return res.end('{"ok":false,"error":"web pages may not call the browser agent"}'); }
  if (!authorized(req)) { res.statusCode = 401; return res.end('{"ok":false,"error":"missing or wrong agent token"}'); }
  last = Date.now();
  let body = ""; for await (const c of req) body += c;
  const fn = routes.get(`${req.method} ${req.url}`);
  if (!fn) { res.statusCode = 404; return res.end(); }
  await sync().catch(() => {});
  await fn(req, res, body);
});
server.listen(0, "127.0.0.1", () => writeFileSync(join(dir, "agent.port"), String(server.address().port)));
const stop = () => { rmSync(join(dir, "agent.port"), { force: true }); rmSync(join(dir, "agent.token"), { force: true }); process.exit(0); };
process.on("SIGTERM", stop); process.on("SIGINT", stop);
// Leave when the host is gone or nobody has asked for 30 minutes.
// (On the app, cdp.json appears with the first browser block, so a host never seen is not a host gone.)
let seen = false;
setInterval(() => { const c = readCdp(); seen ||= !!c; if ((seen && !c) || Date.now() - last > 30 * 60e3) stop(); }, 5000).unref();
