// Headless test for agent control (agent.mjs + core.mjs + bin/herdr-browser), ported from the browser mod's selftest.
// Run:  node scripts/herdr-browser/selftest.agent.mjs     (exit 0 = pass)
// Backend: by default a headless Google Chrome stands in for the CEF host (standin-chrome.mjs), found through a temp
// cdp.json. HERDR_BROWSER_SELFTEST=app runs against a real app and CEF host instead: HERDR_BROWSER_DIR is that app's
// browser dir and HERDR_APP_BIN runs `herdr-app` for it (scripts/fork-browser-agent.sh sets both for a headless fork). Serves a local form page, then through the CLI:
// open, snapshot, click ref, type, press, wait, text, console, network, eval, screenshot (2x PNG), permission refusal
// on a person's block until its switch is on, DevTools refusal, and the MCP wrapper.
import { spawn, execFile, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, writeFileSync, rmSync, statSync } from "node:fs";
import { connect } from "node:net";
import { join, dirname } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
import { startStandin } from "./standin-chrome.mjs";
const onApp = process.env.HERDR_BROWSER_SELFTEST === "app";
const dir = onApp ? process.env.HERDR_BROWSER_DIR : mkdtempSync(join(tmpdir(), "browser-agenttest-"));
const env = { ...process.env, HERDR_BROWSER_DIR: dir, HERDR_BROWSER_BACKEND: onApp ? "app" : "standin" };
const standin = onApp ? null : await startStandin(dir);
console.log(`backend: ${onApp ? `real app + CEF host (${dir})` : "headless Chrome stand-in"}`);
const cdpHttp = () => `http://127.0.0.1:${JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")).port}`;
const appDo = (name, args) => new Promise((r) => execFile(process.env.HERDR_APP_BIN, ["do", name, JSON.stringify(args)], { encoding: "utf8" }, (_e, out) => r(JSON.parse(out))));
// What the app does when the person opens a block: a page plus its id in cdp.json. Answers the block id.
const personBlock = async (id) => {
  if (onApp) {
    const { id: block } = await appDo("browser.open", { url: "about:blank" });
    for (let i = 0; i < 100 && !JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")).targets[block]; i++) { await appDo("browser.eval", { id: block, expr: "1" }); await new Promise((r) => setTimeout(r, 100)); }
    return block;
  }
  const t = await (await fetch(`${cdpHttp()}/json/new?about:blank`, { method: "PUT" })).json();
  const c = JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")); c.targets[id] = t.id; writeFileSync(join(dir, "cdp.json"), JSON.stringify(c));
  return id;
};
let P; // the person's block
// The person's grant in the block (the app writes grants.json from its UI; the test plays the person).
const grant = (id, g) => { let all = {}; try { all = JSON.parse(readFileSync(join(dir, "grants.json"), "utf8")); } catch {}
  all[id] = { ...all[id], ...g }; writeFileSync(join(dir, "grants.json"), JSON.stringify(all), { mode: 0o600 }); };
let T = "t1"; // the agent's tab: the app picks block ids, so it is whatever open answers
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failed++; };
// Async on purpose: a sync exec would freeze this process, and with it the local page server.
const cli = (...a) => new Promise((r) => execFile("node", [join(here, "bin/herdr-browser"), ...a], { env, encoding: "utf8" },
  (e, out) => r(e ? `EXIT ${e.code} ${out}` : out.trim())));
const json = async (...a) => { const o = (await cli(...a)).replace(/^EXIT \S+ /, ""); try { return JSON.parse(o); } catch { return { raw: o }; } };

const html = `<!doctype html><title>Form</title><body style="margin:0;font:16px sans-serif">
<h1>Agent test</h1><input id=q placeholder="Your name"><button id=go onclick="document.getElementById('out').textContent='Hello '+q.value;console.log('clicked',q.value)">Greet</button>
<p id=out>nobody yet</p><select id=pick><option value=a>Apple</option><option value=b>Banana</option></select>
<input id=f placeholder="Fill me" oninput="document.title='filled '+this.value"><input type=password id=pw placeholder="Password" value="hunter2-secret"><input id=card_number placeholder="Card" value="4111111111111111">
<div id=src draggable=true style="width:40px;height:20px;background:red">drag</div><div id=dst style="width:60px;height:30px;background:#ccc" ondragover="event.preventDefault()">drop</div><img src="/missing.png" alt=x><script>console.warn('page ready')</script></body>`;
const web = createServer((req, res) => { if (req.url === "/missing.png") { res.statusCode = 404; res.end(); return; } res.setHeader("content-type", "text/html"); res.end(html); });
await new Promise((r) => web.listen(0, "127.0.0.1", r));
const site = `http://127.0.0.1:${web.address().port}/`;

try {
  const o = (await json("open", T, `url=${site}`));
  T = o.tab ?? T;
  check(o.ok && o.url === site, `open starts the daemon and loads the page (${o.url})`);
  check((await json("tabs")).tabs?.some((t) => t.tab === T && t.controllable && t.agentOpened), "tabs lists the agent tab as controllable");
  const snap = (await cli("snapshot", T));
  const ref = (re) => snap.split("\n").find((l) => re.test(l))?.match(/ref=(e\d+)/)?.[1];
  const box = ref(/textbox "Your name"/), btn = ref(/button "Greet"/);
  check(box && btn && /heading "Agent test"/.test(snap), `snapshot has refs (textbox ${box}, button ${btn})`);
  check((await json("type", T, `ref=${box}`, "text=Ada")).ok, "type into the textbox by ref");
  check((await json("click", T, `ref=${btn}`)).ok, "click the button by ref");
  check((await json("wait", T, "text=Hello Ada", "timeout=3000")).ok, "wait sees the click result");
  check((await cli("text", T)).includes("Hello Ada"), "text reads the page");
  check((await json("eval", T, "expression=document.getElementById('q').value")).value === "Ada", "eval returns a value");
  (await json("type", T, `ref=${box}`, "text=Bob")); (await json("press", T, "key=Tab")); (await json("press", T, "key=Enter"));
  check((await json("wait", T, "text=Hello Bob", "timeout=3000")).ok, "press Tab then Enter activates the focused button");
  check((await json("click", T, "x=5", "y=5")).ok, "click by coordinates");
  const con = (await json("console", T)).messages ?? [];
  check(con.some((m) => m.text === "page ready" && m.level === "warning") && con.some((m) => m.text === "clicked Ada"), `console messages captured (${con.length})`);
  check(((await json("network", T)).errors ?? []).some((e) => e.status === 404 && e.url.endsWith("/missing.png")), "network errors captured (404)");
  check((await json("wait", T, "selector=#nope", "timeout=500")).ok === false, "wait times out on a missing selector");

  // Cursor-shaped commands (browser_* names).
  const snap2 = (await cli("browser_snapshot", T));
  const r2 = (re) => snap2.split("\n").find((l) => re.test(l))?.match(/ref=(e\d+)/)?.[1];
  const sel = r2(/combobox/), fill = r2(/textbox "Fill me"/);
  // X3: password and card values never reach the agent (the fields still show, marked hidden).
  check(!/hunter2-secret|4111111111111111/.test(snap2) && /textbox "Password" value=<hidden>/.test(snap2), "X3 snapshot hides password and card values");
  check((await json("browser_select_option", T, `ref=${sel}`, 'values=["Banana"]')).selected?.[0] === "b", "select_option by label");
  check((await json("browser_fill", T, `ref=${fill}`, "value=xyz")).ok && (await json("eval", T, "expression=document.title")).value === "filled xyz", "fill sets the value and fires input");
  const bb = await json("browser_get_bounding_box", T, `ref=${sel}`);
  check(bb.width > 10 && bb.height > 5, `get_bounding_box (${bb.width}x${bb.height})`);
  check((await json("browser_highlight", T, `ref=${sel}`, "durationMs=100")).ok, "highlight");
  check((await json("browser_mouse_click_xy", T, "x=3", "y=3")).ok, "mouse_click_xy");
  check((await json("browser_drag", T, `sourceRef=${r2(/text "drag"/)}`, `targetRef=${r2(/text "drop"/)}`)).to?.x > 0, "drag between refs");
  check((await json("browser_press_key", T, "key=Tab")).ok, "press_key");
  check((await json("browser_scroll", T, "direction=down", "amount=50")).ok, "scroll by direction");
  // X2: raw CDP is brokered: refused without the person's grant, and only allowed methods with it.
  check(/needs the person's cdp grant/.test(await cli("browser_cdp", T, "method=Runtime.evaluate", 'params={"expression":"1+1"}')), "X2 raw CDP refused without a grant");
  grant(T, { cdp: true });
  check((await json("browser_cdp", T, "method=Runtime.evaluate", 'params={"expression":"1+1"}')).result?.result?.value === 2, "cdp passthrough with the grant");
  check(/not allowed for agents/.test(await cli("browser_cdp", T, "method=Storage.getCookies")), "X2 cookie and profile methods refused even with the grant");
  check((await json("browser_lock", T, "action=lock")).locked === true && (await json("tabs")).ok, "lock sets the lock flag");
  (await json("browser_lock", T, "action=unlock"));
  const tl = await json("browser_tabs", "action=list");
  check(tl.tabs?.[0]?.tab === T && tl.tabs[0].selected, "tabs list marks the selected tab");
  check((await cli("browser_text")).includes("Hello"), "commands default to the selected tab");
  const aft = await json("browser_click", T, "x=3", "y=3", "take_screenshot_afterwards=true");
  check(/\.png$/.test(aft.screenshot ?? ""), "take_screenshot_afterwards returns a PNG path");
  const el = (await cli("browser_take_screenshot", T, `ref=${sel}`));
  check(el.endsWith(".png"), "element screenshot by ref");

  // A 400x300 block on a 2x screen (the CEF host renders size * scale; here the stand-in emulates it).
  (await json("browser_cdp", T, "method=Emulation.setDeviceMetricsOverride", 'params={"width":400,"height":300,"deviceScaleFactor":2,"mobile":false}'));
  const shot = (await cli("screenshot", T, `path=${join(dir, "s.png")}`));
  const png = readFileSync(shot);
  const w = png.readUInt32BE(16), h = png.readUInt32BE(20);
  check(png.subarray(1, 4).toString() === "PNG" && w === 800 && h === 600, `screenshot is a full-res PNG (${w}x${h} for a 400x300 view)`);
  const auto = (await cli("screenshot", T));
  check(auto.startsWith(join(dir, "shots")) && auto.endsWith(".png"), "screenshot without path saves under the cache dir");

  // A block the person opened refuses until their switch turns it on.
  P = await personBlock("panel");
  check(/not agent-controllable/.test((await cli("text", P))), "panel tab refuses agent commands by default");
  // X1: an agent cannot grant itself control: the old allow command is gone from the agent API.
  check(/unknown cmd allow/.test(await cli("allow", P, "on=true")) && /not agent-controllable/.test(await cli("text", P)), "X1 an agent cannot self-grant");
  grant(P, { control: true });
  check((await json("navigate", P, `url=${site}`)).ok, "toggle on lets the agent drive the panel tab");
  check(/only tabs an agent opened/.test((await cli("close", P))), "close is limited to agent tabs");
  grant(P, { control: false });
  check(/not agent-controllable/.test((await cli("text", P))), "toggle off takes control back");

  // DevTools open for a tab (an inspector page for its target) blocks agent commands on it.
  const tid = (await json("browser_cdp", T, "method=Target.getTargetInfo")).result?.targetInfo?.targetId;
  const dt = await (await fetch(`${cdpHttp()}/json/new?${encodeURIComponent(`${site}devtools/page/${tid}`)}`, { method: "PUT" })).json();
  await new Promise((r) => setTimeout(r, 800));
  check(/DevTools is open/.test((await cli("text", T))), "refuses while DevTools is open for the tab");
  await fetch(`${cdpHttp()}/json/close/${dt.id}`);
  check((await cli("text", T)).includes("Hello"), "works again after DevTools closes");

  // X5: the agent daemon answers only token holders and never a web page.
  const ap = readFileSync(join(dir, "agent.port"), "utf8").trim(), tok = readFileSync(join(dir, "agent.token"), "utf8").trim();
  const st = async (h) => (await fetch(`http://127.0.0.1:${ap}/_agent`, { method: "POST", headers: h, body: '{"cmd":"tabs"}' })).status;
  check(await st({}) === 401 && await st({ authorization: "Bearer nope" }) === 401, "X5 daemon refuses a client without the token");
  check(await st({ authorization: `Bearer ${tok}`, origin: "http://evil.test" }) === 403, "X5 daemon refuses a web page Origin even with the token");
  check((statSync(join(dir, "agent.token")).mode & 0o777) === 0o600, "X5 the token file is owner-only");
  // The raw CDP port refuses a websocket from a foreign Origin (Chromium's --remote-allow-origins check).
  const raw = await new Promise((r) => { const c = connect(JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")).port, "127.0.0.1", () =>
    c.write(`GET /devtools/page/x HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: http://evil.test\r\n\r\n`));
    c.once("data", (d) => { r(String(d).split("\r\n")[0]); c.destroy(); }); c.on("error", (e) => r(String(e))); });
  check(!/ 101 /.test(raw), `X5 raw CDP port refuses a foreign Origin (${raw})`);

  // MCP wrapper: initialize, list tools, call one.
  const m = spawn("node", [join(here, "bin/herdr-browser"), "mcp"], { env, stdio: ["pipe", "pipe", "inherit"] });
  // Wait for the answer to call 3 itself, not a fixed time: under load the server can take seconds to start.
  let buf = "";
  const answered = new Promise((r) => m.stdout.on("data", (d) => { buf += d; if (buf.split("\n").some((l) => /"id":3\b/.test(l))) r(); }));
  for (const [id, method, params] of [[1, "initialize", {}], [2, "tools/list"], [3, "tools/call", { name: "browser_text", arguments: { tab: T } }]])
    m.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  await Promise.race([answered, new Promise((r) => setTimeout(r, 60000))]);
  m.kill();
  const rs = buf.trim().split("\n").map((l) => JSON.parse(l));
  check(rs[1]?.result?.tools?.some((t) => t.name === "browser_take_screenshot") && rs[1].result.tools.length >= 25 && rs[2]?.result?.content?.[0]?.text.includes("Hello Bob"), `MCP lists ${rs[1]?.result?.tools?.length} tools and a call returns page text`);

  check((await json("close", T)).ok && !(await json("tabs")).tabs.some((t) => t.tab === T), "close removes the agent tab");
} catch (e) { check(false, `threw: ${e}`); }

// Shut down the daemon this test started (only the one on this test's port), then the stand-in Chrome.
try { const p = readFileSync(join(dir, "agent.port"), "utf8").trim(); execFileSync("sh", ["-c", `kill $(lsof -ti tcp:${p} -sTCP:LISTEN)`]); } catch {}
if (onApp && P) await appDo("browser.close", { id: P }).catch(() => {});
standin?.stop();
await new Promise((r) => setTimeout(r, 500));
const pids = onApp ? "" : (() => { try { return execFileSync("pgrep", ["-f", standin.profile], { encoding: "utf8" }).trim(); } catch { return ""; } })();
if (pids) execFileSync("kill", pids.split("\n"));
web.close();
if (!onApp) rmSync(dir, { recursive: true, force: true });
console.log(failed ? `${failed} failed` : "all passed");
process.exit(failed ? 1 : 0);
