// Headless test for agent control (agent.mjs + core.mjs + bin/herdr-browser), ported from the browser mod's selftest.
// Run:  node scripts/herdr-browser/selftest.agent.mjs     (exit 0 = pass)
// Backend: a headless Google Chrome stands in for the CEF host (standin-chrome.mjs) until the host serves CDP; the
// daemon finds it through a temp cdp.json, never the running app's. Serves a local form page, then through the CLI:
// open, snapshot, click ref, type, press, wait, text, console, network, eval, screenshot (2x PNG), permission refusal
// on a person's block until its switch is on, DevTools refusal, and the MCP wrapper.
import { spawn, execFile, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
import { startStandin } from "./standin-chrome.mjs";
const dir = mkdtempSync(join(tmpdir(), "browser-agenttest-"));
const env = { ...process.env, HERDR_BROWSER_DIR: dir, HERDR_BROWSER_BACKEND: "standin" };
const standin = await startStandin(dir);
const cdpHttp = `http://127.0.0.1:${standin.port}`;
// What the app does when the person opens a block: a target plus its id in cdp.json.
const personBlock = async (id) => { const t = await (await fetch(`${cdpHttp}/json/new?about:blank`, { method: "PUT" })).json();
  const c = JSON.parse(readFileSync(join(dir, "cdp.json"), "utf8")); c.targets[id] = t.id; writeFileSync(join(dir, "cdp.json"), JSON.stringify(c)); };
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failed++; };
// Async on purpose: a sync exec would freeze this process, and with it the local page server.
const cli = (...a) => new Promise((r) => execFile("node", [join(here, "bin/herdr-browser"), ...a], { env, encoding: "utf8" },
  (e, out) => r(e ? `EXIT ${e.code} ${out}` : out.trim())));
const json = async (...a) => { const o = (await cli(...a)).replace(/^EXIT \S+ /, ""); try { return JSON.parse(o); } catch { return { raw: o }; } };

const html = `<!doctype html><title>Form</title><body style="margin:0;font:16px sans-serif">
<h1>Agent test</h1><input id=q placeholder="Your name"><button id=go onclick="document.getElementById('out').textContent='Hello '+q.value;console.log('clicked',q.value)">Greet</button>
<p id=out>nobody yet</p><select id=pick><option value=a>Apple</option><option value=b>Banana</option></select>
<input id=f placeholder="Fill me" oninput="document.title='filled '+this.value">
<div id=src draggable=true style="width:40px;height:20px;background:red">drag</div><div id=dst style="width:60px;height:30px;background:#ccc" ondragover="event.preventDefault()">drop</div><img src="/missing.png" alt=x><script>console.warn('page ready')</script></body>`;
const web = createServer((req, res) => { if (req.url === "/missing.png") { res.statusCode = 404; res.end(); return; } res.setHeader("content-type", "text/html"); res.end(html); });
await new Promise((r) => web.listen(0, "127.0.0.1", r));
const site = `http://127.0.0.1:${web.address().port}/`;

try {
  const o = (await json("open", "t1", `url=${site}`));
  check(o.ok && o.url === site, `open starts the daemon and loads the page (${o.url})`);
  check((await json("tabs")).tabs?.some((t) => t.tab === "t1" && t.controllable && t.agentOpened), "tabs lists the agent tab as controllable");
  const snap = (await cli("snapshot", "t1"));
  const ref = (re) => snap.split("\n").find((l) => re.test(l))?.match(/ref=(e\d+)/)?.[1];
  const box = ref(/textbox "Your name"/), btn = ref(/button "Greet"/);
  check(box && btn && /heading "Agent test"/.test(snap), `snapshot has refs (textbox ${box}, button ${btn})`);
  check((await json("type", "t1", `ref=${box}`, "text=Ada")).ok, "type into the textbox by ref");
  check((await json("click", "t1", `ref=${btn}`)).ok, "click the button by ref");
  check((await json("wait", "t1", "text=Hello Ada", "timeout=3000")).ok, "wait sees the click result");
  check((await cli("text", "t1")).includes("Hello Ada"), "text reads the page");
  check((await json("eval", "t1", "expression=document.getElementById('q').value")).value === "Ada", "eval returns a value");
  (await json("type", "t1", `ref=${box}`, "text=Bob")); (await json("press", "t1", "key=Tab")); (await json("press", "t1", "key=Enter"));
  check((await json("wait", "t1", "text=Hello Bob", "timeout=3000")).ok, "press Tab then Enter activates the focused button");
  check((await json("click", "t1", "x=5", "y=5")).ok, "click by coordinates");
  const con = (await json("console", "t1")).messages ?? [];
  check(con.some((m) => m.text === "page ready" && m.level === "warning") && con.some((m) => m.text === "clicked Ada"), `console messages captured (${con.length})`);
  check(((await json("network", "t1")).errors ?? []).some((e) => e.status === 404 && e.url.endsWith("/missing.png")), "network errors captured (404)");
  check((await json("wait", "t1", "selector=#nope", "timeout=500")).ok === false, "wait times out on a missing selector");

  // Cursor-shaped commands (browser_* names).
  const snap2 = (await cli("browser_snapshot", "t1"));
  const r2 = (re) => snap2.split("\n").find((l) => re.test(l))?.match(/ref=(e\d+)/)?.[1];
  const sel = r2(/combobox/), fill = r2(/textbox "Fill me"/);
  check((await json("browser_select_option", "t1", `ref=${sel}`, 'values=["Banana"]')).selected?.[0] === "b", "select_option by label");
  check((await json("browser_fill", "t1", `ref=${fill}`, "value=xyz")).ok && (await json("eval", "t1", "expression=document.title")).value === "filled xyz", "fill sets the value and fires input");
  const bb = await json("browser_get_bounding_box", "t1", `ref=${sel}`);
  check(bb.width > 10 && bb.height > 5, `get_bounding_box (${bb.width}x${bb.height})`);
  check((await json("browser_highlight", "t1", `ref=${sel}`, "durationMs=100")).ok, "highlight");
  check((await json("browser_mouse_click_xy", "t1", "x=3", "y=3")).ok, "mouse_click_xy");
  check((await json("browser_drag", "t1", `sourceRef=${r2(/text "drag"/)}`, `targetRef=${r2(/text "drop"/)}`)).to?.x > 0, "drag between refs");
  check((await json("browser_press_key", "t1", "key=Tab")).ok, "press_key");
  check((await json("browser_scroll", "t1", "direction=down", "amount=50")).ok, "scroll by direction");
  check((await json("browser_cdp", "t1", "method=Runtime.evaluate", 'params={"expression":"1+1"}')).result?.result?.value === 2, "cdp passthrough");
  check((await json("browser_lock", "t1", "action=lock")).locked === true && (await json("tabs")).ok, "lock sets the lock flag");
  (await json("browser_lock", "t1", "action=unlock"));
  const tl = await json("browser_tabs", "action=list");
  check(tl.tabs?.[0]?.tab === "t1" && tl.tabs[0].selected, "tabs list marks the selected tab");
  check((await cli("browser_text")).includes("Hello"), "commands default to the selected tab");
  const aft = await json("browser_click", "t1", "x=3", "y=3", "take_screenshot_afterwards=true");
  check(/\.png$/.test(aft.screenshot ?? ""), "take_screenshot_afterwards returns a PNG path");
  const el = (await cli("browser_take_screenshot", "t1", `ref=${sel}`));
  check(el.endsWith(".png"), "element screenshot by ref");

  // A 400x300 block on a 2x screen (the CEF host renders size * scale; here the stand-in emulates it).
  (await json("browser_cdp", "t1", "method=Emulation.setDeviceMetricsOverride", 'params={"width":400,"height":300,"deviceScaleFactor":2,"mobile":false}'));
  const shot = (await cli("screenshot", "t1", `path=${join(dir, "s.png")}`));
  const png = readFileSync(shot);
  const w = png.readUInt32BE(16), h = png.readUInt32BE(20);
  check(png.subarray(1, 4).toString() === "PNG" && w === 800 && h === 600, `screenshot is a full-res PNG (${w}x${h} for a 400x300 view)`);
  const auto = (await cli("screenshot", "t1"));
  check(auto.startsWith(join(dir, "shots")) && auto.endsWith(".png"), "screenshot without path saves under the cache dir");

  // A block the person opened refuses until their switch turns it on.
  await personBlock("panel");
  check(/not agent-controllable/.test((await cli("text", "panel"))), "panel tab refuses agent commands by default");
  (await json("allow", "panel", "on=true"));
  check((await json("navigate", "panel", `url=${site}`)).ok, "toggle on lets the agent drive the panel tab");
  check(/only tabs an agent opened/.test((await cli("close", "panel"))), "close is limited to agent tabs");
  (await json("allow", "panel", "on=false"));
  check(/not agent-controllable/.test((await cli("text", "panel"))), "toggle off takes control back");

  // DevTools open for a tab (an inspector page for its target) blocks agent commands on it.
  const tid = (await json("browser_cdp", "t1", "method=Target.getTargetInfo")).result?.targetInfo?.targetId;
  const dt = await (await fetch(`${cdpHttp}/json/new?${encodeURIComponent(`${site}devtools/page/${tid}`)}`, { method: "PUT" })).json();
  await new Promise((r) => setTimeout(r, 800));
  check(/DevTools is open/.test((await cli("text", "t1"))), "refuses while DevTools is open for the tab");
  await fetch(`${cdpHttp}/json/close/${dt.id}`);
  check((await cli("text", "t1")).includes("Hello"), "works again after DevTools closes");

  // MCP wrapper: initialize, list tools, call one.
  const m = spawn("node", [join(here, "bin/herdr-browser"), "mcp"], { env, stdio: ["pipe", "pipe", "inherit"] });
  let buf = ""; m.stdout.on("data", (d) => (buf += d));
  for (const [id, method, params] of [[1, "initialize", {}], [2, "tools/list"], [3, "tools/call", { name: "browser_text", arguments: { tab: "t1" } }]])
    m.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  for (let i = 0; i < 50 && buf.split("\n").length < 4; i++) await new Promise((r) => setTimeout(r, 100));
  m.kill();
  const rs = buf.trim().split("\n").map((l) => JSON.parse(l));
  check(rs[1]?.result?.tools?.some((t) => t.name === "browser_take_screenshot") && rs[1].result.tools.length >= 25 && rs[2]?.result?.content?.[0]?.text.includes("Hello Bob"), `MCP lists ${rs[1]?.result?.tools?.length} tools and a call returns page text`);

  check((await json("close", "t1")).ok && !(await json("tabs")).tabs.some((t) => t.tab === "t1"), "close removes the agent tab");
} catch (e) { check(false, `threw: ${e}`); }

// Shut down the daemon this test started (only the one on this test's port), then the stand-in Chrome.
try { const p = readFileSync(join(dir, "agent.port"), "utf8").trim(); execFileSync("sh", ["-c", `kill $(lsof -ti tcp:${p} -sTCP:LISTEN)`]); } catch {}
standin.stop();
await new Promise((r) => setTimeout(r, 500));
const pids = (() => { try { return execFileSync("pgrep", ["-f", standin.profile], { encoding: "utf8" }).trim(); } catch { return ""; } })();
if (pids) execFileSync("kill", pids.split("\n"));
web.close();
rmSync(dir, { recursive: true, force: true });
console.log(failed ? `${failed} failed` : "all passed");
process.exit(failed ? 1 : 0);
