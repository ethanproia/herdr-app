// Agent control: agents drive and screenshot the browser blocks the person sees, over the host's CDP port.
// Ported from the browser mod (server/agent.mjs); core.mjs now finds pages through $HERDR_APP_HOME/browser/cdp.json.
// HTTP: POST /_agent {"cmd": name, "tab": id, ...args} -> JSON {ok, ...result} or {ok:false, error}. A tab is a block id.
// Agents may act only on tabs they opened (cmd "open") or that the person granted "control" in the block. Grants are
// app-owned: <dir>/grants.json = { "<block id>": { "control": bool, "cdp": bool, "capture": bool } }, written by the app
// (src/browser/agent.rs Grants) from the person's UI; nothing an agent can call here writes it.
// state.agent = true when the tab is agent-controllable; state.agentOpened = true when an agent opened it.
import { mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";

const KEYS = { Enter: 13, Backspace: 8, Tab: 9, Escape: 27, Delete: 46, ArrowLeft: 37, ArrowRight: 39, ArrowUp: 38, ArrowDown: 40,
  Home: 36, End: 35, PageUp: 33, PageDown: 34, " ": 32 };
const MODS = { Alt: 1, Control: 2, Meta: 4, Shift: 8 };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// One rule for "this field holds a secret", used by snapshots, redaction and the capture check.
const SECRET_FIELD = `((c) => c.tagName === "INPUT" && (c.type === "password" || /cc-|password|one-time-code/i.test(c.autocomplete || "")
  || /pass|secret|token|otp|cvv|cvc|ssn|card/i.test((c.name || "") + " " + (c.id || ""))))`;
// The values of the page's secret fields (for redaction) and how many there are (the page is sensitive when any exist).
const SECRETS = `[...document.querySelectorAll("input")].filter(${SECRET_FIELD}).map((c) => c.value)`;
// Result fields that carry page content: untrusted data from the web page, never instructions to the agent.
const PAGE_FIELDS = ["text", "snapshot", "value", "messages", "errors", "title", "url", "result", "selected"];
const CAPTURES = new Set(["eval", "screenshot", "take_screenshot", "cdp"]);

// Walks the DOM, tags visible interactive or text elements with data-hb-ref, returns one indented line each.
const SNAPSHOT = (o) => `(() => {
  const O = ${JSON.stringify(o)}; let n = 0; const out = [];
  const role = (e) => e.getAttribute("role") || ({ A: "link", BUTTON: "button", INPUT: e.type === "checkbox" ? "checkbox" : e.type === "submit" ? "button" : "textbox",
    TEXTAREA: "textbox", SELECT: "combobox", IMG: "img", H1: "heading", H2: "heading", H3: "heading", H4: "heading", LABEL: "label" })[e.tagName];
  const visible = (e) => { const r = e.getBoundingClientRect(), s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility !== "hidden" && s.display !== "none"; };
  const walk = (e, d) => {
    if (d > (O.maxDepth ?? 20)) return;
    for (const c of e.children) {
      if (/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|HEAD)$/.test(c.tagName) || !visible(c)) continue;
      const r = role(c), own = [...c.childNodes].filter((t) => t.nodeType === 3).map((t) => t.textContent.trim()).join(" ").trim();
      if (r || (own && !O.interactive)) {
        const ref = "e" + (++n); c.setAttribute("data-hb-ref", ref);
        const name = (c.getAttribute("aria-label") || c.alt || c.placeholder || (r ? c.innerText : own) || "").replace(/\\s+/g, " ").trim().slice(0, 80);
        // Never hand an agent a secret: passwords, card numbers, one-time codes and anything named like a credential.
        const secret = ${SECRET_FIELD}(c);
        const val = c.value !== undefined && c.tagName !== "BUTTON" && c.value !== "" ? (secret ? " value=<hidden>" : " value=" + JSON.stringify(String(c.value).slice(0, 80))) : "";
        out.push("  ".repeat(d) + "- " + (r || "text") + (name ? " " + JSON.stringify(name) : "") + val + (c.href ? " href=" + c.getAttribute("href") : "") + " [ref=" + ref + "]");
        walk(c, d + 1);
      } else walk(c, d);
    }
  };
  document.querySelectorAll("[data-hb-ref]").forEach((e) => e.removeAttribute("data-hb-ref"));
  const root = O.selector ? document.querySelector(O.selector) : document.body || document.documentElement;
  if (!root) return "no element matches " + O.selector;
  walk(root, 0);
  return out.join("\\n");
})()`;

// Raw CDP an agent may send once the person grants "cdp": page inspection and rendering only; nothing that reaches
// other tabs, the profile (cookies, storage), downloads, the browser process or the file system.
export const CDP_ALLOWED = /^(DOM|CSS|Accessibility|Overlay|DOMSnapshot|Animation|Performance|Log|Debugger)\.|^(Runtime\.(evaluate|callFunctionOn|getProperties|releaseObject)|Page\.(captureScreenshot|getLayoutMetrics|getFrameTree|reload)|Emulation\.(setDeviceMetricsOverride|clearDeviceMetricsOverride|setEmulatedMedia)|Target\.getTargetInfo|Input\.dispatch(Mouse|Key|Touch)Event)$/;

// Replaces every secret field value anywhere in a result (eval, console, text...) with <hidden>.
const redact = (v, secrets) => {
  if (!secrets.length) return v;
  if (typeof v === "string") return secrets.reduce((t, x) => t.split(String(x)).join("<hidden>"), v);
  if (Array.isArray(v)) return v.map((x) => redact(x, secrets));
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, redact(x, secrets)]));
  return v;
};

export default function register({ sessions, onEvent, onOpen, route, dir, openTab, closeTab, targets, lease = async () => {}, annotations = async () => ({ batch: null }) }) {
  const opened = new Set(); // tab ids an agent opened
  const grants = () => { try { return JSON.parse(readFileSync(join(dir, "grants.json"), "utf8")); } catch { return {}; } };
  const granted = (id, cap) => grants()[id]?.[cap] === true;
  const controllable = (s) => (s.state.agent = opened.has(s.id) || granted(s.id, "control"));
  const shots = join(dir, "shots");

  onOpen((s) => {
    s.agentLog = { console: [], network: [] };
    s.state.agent = opened.has(s.id); s.state.agentOpened = opened.has(s.id);
    s.send("Runtime.enable"); s.send("Network.enable");
  });
  const push = (s, kind, entry) => { const l = s?.agentLog?.[kind]; if (!l) return; l.push({ t: Date.now(), ...entry }); if (l.length > 200) l.shift(); };
  onEvent("Runtime.consoleAPICalled", (p, s) => push(s, "console", { level: p.type,
    text: p.args.map((a) => a.value ?? a.description ?? a.unserializableValue ?? a.type).join(" ") }));
  onEvent("Runtime.exceptionThrown", (p, s) => push(s, "console", { level: "error", text: p.exceptionDetails.exception?.description ?? p.exceptionDetails.text }));
  const urls = new Map();
  onEvent("Network.requestWillBeSent", (p) => { urls.set(p.requestId, p.request.url); if (urls.size > 2000) urls.delete(urls.keys().next().value); });
  onEvent("Network.responseReceived", (p, s) => { if (p.response.status >= 400) push(s, "network", { url: p.response.url, status: p.response.status }); });
  onEvent("Network.loadingFailed", (p, s) => { if (!p.canceled) push(s, "network", { url: urls.get(p.requestId) ?? "", error: p.errorText }); });


  const evaluate = async (s, expression) => {
    const r = await s.call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (r?.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
    return r?.result?.value;
  };
  const el = (ref) => `document.querySelector('[data-hb-ref="${String(ref).replace(/[^a-z0-9]/gi, "")}"]')`;
  const onEl = async (s, ref, body) => {
    const v = await evaluate(s, `(() => { const e = ${el(ref)}; if (!e) return { missing: true }; ${body} })()`);
    if (v?.missing) throw new Error(`no element with ref ${ref}; take a new snapshot`);
    return v;
  };
  // Box in CSS px of the viewport, after scrolling the element into view.
  const box = (s, ref, scroll = true) => onEl(s, ref, `${scroll ? 'e.scrollIntoView({ block: "center", inline: "center" });' : ""}
    const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height };`);
  const center = async (s, ref) => { const b = await box(s, ref); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; };
  const key = async (s, combo) => {
    const parts = combo.split("+"), k = parts.pop(), modifiers = parts.reduce((m, p) => m | (MODS[p] ?? 0), 0);
    const vk = KEYS[k] ?? (k.length === 1 ? k.toUpperCase().charCodeAt(0) : 0);
    const code = k.length === 1 && /[a-z]/i.test(k) ? `Key${k.toUpperCase()}` : k;
    const text = k.length === 1 && !(modifiers & 7) ? k : k === "Enter" ? "\r" : undefined;
    await s.call("Input.dispatchKeyEvent", { type: text ? "keyDown" : "rawKeyDown", key: k, code, windowsVirtualKeyCode: vk, modifiers, text });
    await s.call("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk, modifiers });
  };
  const waitFor = async (s, { selector, text, timeout = 10000 }) => {
    for (const end = Date.now() + timeout; Date.now() < end; await sleep(100)) {
      const ok = await evaluate(s, selector ? `!!document.querySelector(${JSON.stringify(selector)})`
        : text ? `document.body?.innerText.includes(${JSON.stringify(text)})` : `document.readyState === "complete"`).catch(() => false);
      if (ok && !s.state.loading) return { waited: true };
    }
    throw new Error("timed out");
  };
  // Map values can be the open() promise even after the page is ready (core keeps it), so await them all.
  const pages = async () => (await Promise.all([...sessions.values()])).filter((s) => s?.state);
  const tabs = async () => (await pages()).map((s) => ({ tab: s.id, url: s.state.url, title: s.state.title,
    loading: s.state.loading, controllable: controllable(s), agentOpened: !!s.state.agentOpened }));

  // Each fn(s, args) acts on one tab; the helper checks the tab exists and is controllable first.
  const acts = {
    navigate: async (s, { url }) => { const r = await s.call("Page.navigate", { url }); if (r?.errorText) throw new Error(r.errorText); await waitFor(s, {}); return { url: s.state.url }; },
    back: async (s) => { await evaluate(s, "history.back()"); await sleep(300); await waitFor(s, {}); return { url: s.state.url }; },
    forward: async (s) => { await evaluate(s, "history.forward()"); await sleep(300); await waitFor(s, {}); return { url: s.state.url }; },
    reload: async (s) => { await s.call("Page.reload"); await sleep(300); await waitFor(s, {}); return {}; },
    text: async (s) => ({ text: await evaluate(s, "document.body ? document.body.innerText : ''") }),
    // compact (Cursor): interactive elements only, one flat line each, fewest tokens.
    snapshot: async (s, { interactive, maxDepth, selector, compact }) => {
      const snap = await evaluate(s, SNAPSHOT({ interactive: interactive || compact, maxDepth, selector }));
      return { url: s.state.url, title: s.state.title, snapshot: compact ? snap.split("\n").map((l) => l.trim()).join("\n") : snap };
    },
    hover: async (s, { ref }) => { const c = await center(s, ref); await s.call("Input.dispatchMouseEvent", { type: "mouseMoved", x: c.x, y: c.y }); return c; },
    // fields: [{ ref, value }]; a select takes value as an option value or label.
    fill_form: async (s, { fields = [] }) => {
      for (const f of [].concat(typeof fields === "string" ? JSON.parse(fields) : fields)) {
        const isSelect = await onEl(s, f.ref, `return e.tagName === "SELECT";`);
        if (isSelect) await acts.select_option(s, { ref: f.ref, values: [f.value] }); else await acts.fill(s, f);
      }
      return { filled: [].concat(fields).length };
    },
    click: async (s, { ref, x, y, button = "left", clicks = 1, doubleClick, offsetX, offsetY }) => {
      if (doubleClick) clicks = 2;
      if (ref) { const b = await box(s, ref); x = b.x + (offsetX ?? b.width / 2); y = b.y + (offsetY ?? b.height / 2); }
      if (typeof x !== "number" || typeof y !== "number") throw new Error("click needs ref or x and y");
      await s.call("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
      for (let i = 1; i <= clicks; i++) {
        await s.call("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button, clickCount: i });
        await s.call("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button, clickCount: i });
      }
      return { x, y };
    },
    type: async (s, { ref, text, submit, clear = true, slowly }) => {
      if (ref) { await center(s, ref); await onEl(s, ref, `e.focus(); if (${!!clear} && e.select) e.select();`); }
      if (slowly) for (const ch of String(text ?? "")) { await key(s, ch); await sleep(30); }
      else await s.call("Input.insertText", { text: String(text ?? "") });
      if (submit) await key(s, "Enter");
      return {};
    },
    // Sets the value directly and fires input/change, like Playwright fill.
    fill: async (s, { ref, value = "" }) => { await onEl(s, ref, `e.focus(); const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(e), "value")?.set;
      set ? set.call(e, ${JSON.stringify(String(value))}) : (e.value = ${JSON.stringify(String(value))});
      e.dispatchEvent(new Event("input", { bubbles: true })); e.dispatchEvent(new Event("change", { bubbles: true })); return {};`); return {}; },
    select_option: async (s, { ref, values = [] }) => ({ selected: await onEl(s, ref, `const want = ${JSON.stringify([].concat(values).map(String))};
      if (e.tagName !== "SELECT") throw new Error("ref is not a select");
      for (const o of e.options) o.selected = want.includes(o.value) || want.includes(o.label);
      e.dispatchEvent(new Event("input", { bubbles: true })); e.dispatchEvent(new Event("change", { bubbles: true }));
      return [...e.selectedOptions].map((o) => o.value);`) }),
    mouse_click_xy: (s, { x, y, button }) => acts.click(s, { x, y, button }),
    drag: async (s, { sourceRef, targetRef, targetX, targetY }) => {
      const a = await center(s, sourceRef), b = targetRef ? await center(s, targetRef) : { x: targetX, y: targetY };
      if (typeof b.x !== "number") throw new Error("drag needs targetRef or targetX and targetY");
      await s.call("Input.dispatchMouseEvent", { type: "mouseMoved", x: a.x, y: a.y });
      await s.call("Input.dispatchMouseEvent", { type: "mousePressed", x: a.x, y: a.y, button: "left", clickCount: 1 });
      for (let i = 1; i <= 10; i++) await s.call("Input.dispatchMouseEvent", { type: "mouseMoved", x: a.x + (b.x - a.x) * i / 10, y: a.y + (b.y - a.y) * i / 10, button: "left", buttons: 1 });
      await s.call("Input.dispatchMouseEvent", { type: "mouseReleased", x: b.x, y: b.y, button: "left", clickCount: 1 });
      return { from: a, to: b };
    },
    get_bounding_box: async (s, { ref }) => box(s, ref, false),
    highlight: async (s, { ref, durationMs = 2000 }) => { await onEl(s, ref, `const o = e.style.outline; e.style.outline = "3px solid #f0a";
      setTimeout(() => { e.style.outline = o; }, ${Number(durationMs) | 0}); return {};`); return {}; },
    // Cursor's lock: on the app, the block's input lease (the person sees who is driving and can take control back).
    lock: async (s, { action = "lock" }) => { s.state.agentLock = action === "lock"; await lease(s.id, s.state.agentLock); return { locked: s.state.agentLock }; },
    cdp: async (s, { method, params }) => {
      if (!granted(s.id, "cdp")) throw new Error(`raw CDP on tab ${s.id} needs the person's cdp grant`);
      if (!CDP_ALLOWED.test(String(method))) throw new Error(`CDP method ${method} is not allowed for agents`);
      return { result: await s.call(method, typeof params === "string" ? JSON.parse(params) : params ?? {}) };
    },
    press: async (s, { key: k }) => { await key(s, k); return {}; },
    press_key: (s, a) => acts.press(s, a),
    scroll: async (s, { dx, dy, deltaX, deltaY, direction, amount = 300, ref, scrollIntoView, x, y }) => {
      if (ref && scrollIntoView) { await center(s, ref); return { scrollY: await evaluate(s, "scrollY") }; }
      dx = dx ?? deltaX ?? (direction === "left" ? -amount : direction === "right" ? amount : 0);
      dy = dy ?? deltaY ?? (direction === "up" ? -amount : direction === "down" || !direction && !dx ? amount : 0);
      if (ref) ({ x, y } = await center(s, ref));
      const v = s.view ?? { w: 900, h: 700 };
      await s.call("Input.dispatchMouseEvent", { type: "mouseWheel", x: x ?? v.w / 2, y: y ?? v.h / 2, deltaX: dx, deltaY: dy });
      await sleep(150);
      return { scrollY: await evaluate(s, "scrollY") };
    },
    wait: (s, args) => waitFor(s, args),
    screenshot: async (s, { path, filename, full, fullPage, ref, type = "png" }) => {
      path = path ?? filename;
      const clip = ref ? { ...(await box(s, ref)), scale: 1 } : undefined;
      const r = await s.call("Page.captureScreenshot", { format: type === "jpeg" ? "jpeg" : "png", captureBeyondViewport: !!(full || fullPage), clip });
      if (!r?.data) throw new Error("screenshot failed");
      const buf = Buffer.from(r.data, "base64");
      if (!path) { mkdirSync(shots, { recursive: true }); path = join(shots, `${s.id}-${Date.now()}.${type === "jpeg" ? "jpg" : "png"}`); }
      writeFileSync(path, buf);
      return type === "jpeg" ? { path } : { path, width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
    },
    console: (s, { clear }) => { const l = s.agentLog.console.slice(); if (clear) s.agentLog.console.length = 0; return { messages: l }; },
    network: (s, { clear }) => { const l = s.agentLog.network.slice(); if (clear) s.agentLog.network.length = 0; return { errors: l }; },
    take_screenshot: (s, a) => acts.screenshot(s, a),
    eval: async (s, { expression }) => ({ value: await evaluate(s, expression) }),
    close: async (s) => { if (!s.state.agentOpened) throw new Error("only tabs an agent opened can be closed by an agent"); return { closing: true }; },
  };

  // DevTools for a tab is an inspector target whose url names this page's target.
  const devtoolsOpen = async (s) => (await targets()).some((t) => t.id !== s.targetId && t.url?.includes(`/devtools/page/${s.targetId}`));
  let selected = null; // tab used when a call names none (Cursor's browser_tabs select)
  const run = async ({ cmd, tab, viewId, take_screenshot_afterwards, ...args }) => {
    cmd = String(cmd ?? "").replace(/^browser_/, "");
    tab = tab ?? viewId;
    // Not a page action: what the person sent (the app labels its trusted and untrusted fields).
    if (cmd === "annotations") return annotations({ ...(tab ? { id: String(tab) } : {}), ...(args.wait ? { wait: true } : {}) });
    if (cmd === "tabs") {
      const list = await tabs(), action = args.action ?? "list", pick = args.index !== undefined ? list[args.index]?.tab : tab;
      if (action === "list") return { tabs: list.map((t, index) => ({ index, ...t, selected: t.tab === selected })) };
      if (action === "new") { const r = await run({ cmd: "open", url: args.url, position: args.position }); selected = r.tab; return r; }
      if (!pick) throw new Error(`no tab at index ${args.index}`);
      if (action === "select") { if (!list.find((t) => t.tab === pick)?.controllable) throw new Error(`tab ${pick} is not agent-controllable`); selected = pick; return { selected }; }
      if (action === "close") { if (selected === pick) selected = null; return run({ cmd: "close", tab: pick }); }
      throw new Error("action is list, new, close or select");
    }
    if (cmd !== "open") tab = tab ?? selected;
    if (cmd === "navigate" && !tab) { const r = await run({ cmd: "open", url: args.url, position: args.position }); selected = r.tab; return r; }
    if (cmd === "open") {
      const id = String(tab || `agent-${Date.now().toString(36)}`).replace(/[^a-z0-9_-]/gi, "");
      opened.add(id);
      // position (Cursor): "side" opens beside the focused pane, "active" (default) as a tab.
      const s = await openTab(id, args.position === "side" ? "right" : "tab"); // a new browser block (or, on the stand-in, a new target); the app may pick its id
      opened.add(s.id);
      s.state.agent = true; s.state.agentOpened = true;
      if (args.url) await acts.navigate(s, args);
      selected = s.id;
      return { tab: s.id, url: s.state.url };
    }
    if (!acts[cmd]) throw new Error(`unknown cmd ${cmd}; try: tabs, open, ${Object.keys(acts).join(", ")}`);
    const s = sessions.get(String(tab)) && await sessions.get(String(tab));
    if (!s) throw new Error(`no tab ${tab}; list them with cmd tabs`);
    if (!controllable(s)) throw new Error(`tab ${tab} is not agent-controllable; open your own tab or ask the person to allow agents in that block`);
    if (await devtoolsOpen(s)) throw new Error(`DevTools is open for tab ${tab}; close it before driving the page`);
    // SEC2b: on a page with secret fields, anything that can read or picture them needs the person's capture grant.
    const secrets = (await evaluate(s, SECRETS).catch(() => [])).filter((v) => String(v).length >= 4);
    const sensitive = await evaluate(s, `${SECRETS}.length`).catch(() => 0) > 0;
    if (sensitive && (CAPTURES.has(cmd) || take_screenshot_afterwards) && !granted(s.id, "capture"))
      throw new Error(`tab ${s.id} has password or payment fields: ${cmd} needs the person's capture grant`);
    const r = redact(await acts[cmd](s, args), secrets);
    const untrusted = PAGE_FIELDS.filter((k) => k in r);
    if (untrusted.length) r.untrusted = untrusted;
    if (take_screenshot_afterwards && cmd !== "close") r.screenshot = (await acts.screenshot(s, {})).path;
    if (cmd === "close") { opened.delete(s.id); await closeTab(s.id); }
    return r;
  };

  route("POST", "/_agent", async (_req, res, body) => {
    res.setHeader("content-type", "application/json");
    try { res.end(JSON.stringify({ ok: true, ...(await run(JSON.parse(body || "{}"))) })); }
    catch (e) { res.statusCode = 400; res.end(JSON.stringify({ ok: false, error: e.message ?? String(e) })); }
  });
}
