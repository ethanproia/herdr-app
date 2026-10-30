// Dynamic-site checks (parity plan section W) against a headless fork's browser
// block, over its CDP port. Input goes through CDP Input.* (the agent path);
// screenshots through the app's browser.screenshot. See docs/fork-dev.md.
//
//   bun scripts/fork-web.ts <fork name> <W1|W2|W3|W4|W5> [out dir]
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { homedir } from "node:os";

const [name, scenario, outDir = `/tmp/hf-${process.argv[2]}-shots`] = process.argv.slice(2);
if (!name || !scenario) throw new Error("usage: bun scripts/fork-web.ts <fork name> <W1..W5> [out dir]");
const here = new URL("..", import.meta.url).pathname;
const home = `${homedir()}/.herdr-fork-home/${name}/home`;

function app(...args: string[]): any {
  const r = spawnSync(`${here}scripts/fork-run.sh`, [name, ...args], { encoding: "utf8" });
  try { return JSON.parse(r.stdout); } catch { return { raw: r.stdout.trim(), err: r.stderr.trim() }; }
}

const cdpInfo = JSON.parse(readFileSync(`${home}/browser/cdp.json`, "utf8"));
const blockId = Object.keys(cdpInfo.targets)[0];
const targetId = cdpInfo.targets[blockId];
// HERDR: part 5. CDP is on the app's pipe by default: talk to the broker (agent.sock + token), shaped like a
// page WebSocket; a dev TCP port (HERDR_BROWSER_CDP_PORT) still works as before.
function brokerSocket(): any {
  const { createConnection } = require("node:net");
  const t: any = { readyState: 0 };
  const sock = createConnection(cdpInfo.broker);
  let buf = "";
  sock.on("connect", () => { sock.write(JSON.stringify({ token: readFileSync(cdpInfo.token_file, "utf8").trim() }) + "\n"); t.readyState = 1; t.onopen?.(); });
  sock.on("data", (d: Buffer) => {
    buf += d; let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const m = JSON.parse(buf.slice(0, i)); buf = buf.slice(i + 1);
      const msg = m.id ? (m.error !== undefined ? { id: m.id, error: { message: m.error } } : { id: m.id, result: m.result }) : { method: m.method, params: m.params };
      t.onmessage?.({ data: JSON.stringify(msg) });
    }
  });
  sock.on("error", (e: any) => t.onerror?.(e));
  t.send = (line: string) => sock.write(JSON.stringify({ ...JSON.parse(line), block: blockId }) + "\n");
  t.close = () => sock.destroy();
  return t;
}
const ws: any = cdpInfo.broker ? brokerSocket() : new WebSocket(`ws://127.0.0.1:${cdpInfo.port}/devtools/page/${targetId}`);
await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });

let seq = 0;
const waiting = new Map<number, (v: any) => void>();
const consoleErrors: string[] = [];
const listeners: ((m: any) => void)[] = [];
ws.onmessage = e => {
  const m = JSON.parse(String(e.data));
  if (m.id && waiting.has(m.id)) { waiting.get(m.id)!(m); waiting.delete(m.id); return; }
  if (m.method === "Runtime.exceptionThrown") consoleErrors.push(m.params.exceptionDetails?.exception?.description?.split("\n")[0] ?? m.params.exceptionDetails?.text);
  if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error") consoleErrors.push(m.params.args.map((a: any) => a.value ?? a.description).join(" ").slice(0, 200));
  if (m.method === "Log.entryAdded" && m.params.entry.level === "error") consoleErrors.push(`${m.params.entry.source}: ${m.params.entry.text}${m.params.entry.url ? " " + m.params.entry.url : ""}`.slice(0, 240));
  for (const l of listeners) l(m);
};
function cdp(method: string, params: any = {}): Promise<any> {
  const id = ++seq;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((ok, no) => {
    const t = setTimeout(() => no(new Error(`${method} timed out`)), 30000);
    waiting.set(id, m => { clearTimeout(t); m.error ? no(new Error(`${method}: ${m.error.message}`)) : ok(m.result); });
  });
}
async function evaluate(expr: string): Promise<any> {
  const r = await cdp("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
  return r.result?.value;
}
const sleep = (ms: number) => new Promise<true>(r => setTimeout(() => r(true), ms));
async function load(url: string): Promise<number> {
  const t = performance.now();
  const done = new Promise<void>(ok => listeners.push(m => { if (m.method === "Page.loadEventFired") ok(); }));
  await cdp("Page.navigate", { url });
  await Promise.race([done, sleep(30000)]);
  return Math.round(performance.now() - t);
}
async function waitFor(expr: string, ms = 15000): Promise<boolean> {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (await evaluate(`!!(${expr})`)) return true; await sleep(250); }
  return false;
}
async function key(k: string, code: string, vk: number) {
  for (const type of ["rawKeyDown", "keyUp"]) await cdp("Input.dispatchKeyEvent", { type, key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
  await cdp("Input.dispatchKeyEvent", { type: "char", text: k === "Enter" ? "\r" : k, key: k });
}
async function type(text: string) { await cdp("Input.insertText", { text }); }
async function click(x: number, y: number) {
  for (const type of ["mousePressed", "mouseReleased"]) await cdp("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
}
async function center(sel: string): Promise<[number, number] | null> {
  return evaluate(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null; e.scrollIntoView({block:"center", behavior:"instant"}); const r = e.getBoundingClientRect(); return [r.x + r.width/2, r.y + r.height/2]; })()`);
}
// Frames the page painted (rAF) while wheel-scrolling for ms.
async function scrollFps(ms = 3000, dy = 400): Promise<{ fps: number; scrolled: number }> {
  await evaluate(`window.__f = 0; window.__y0 = scrollY; (function t(){ window.__f++; window.__raf = requestAnimationFrame(t); })(); 0`);
  const end = Date.now() + ms;
  const t0 = Date.now();
  while (Date.now() < end) { await cdp("Input.dispatchMouseEvent", { type: "mouseWheel", x: 400, y: 400, deltaX: 0, deltaY: dy }); await sleep(50); }
  const r = await evaluate(`(() => { cancelAnimationFrame(window.__raf); return [window.__f, scrollY - window.__y0]; })()`);
  return { fps: Math.round((r[0] * 1000) / (Date.now() - t0)), scrolled: Math.round(r[1]) };
}
function shot(tag: string): string {
  const path = `${outDir}/${scenario}-${tag}.png`;
  spawnSync("mkdir", ["-p", outDir]);
  // Headless windows never draw, so frames are never acked and the host stops
  // sending (view.rs send_ack): one app draw lets the newest frame through.
  if (!process.env.FORK_WEB_NO_DRAW) { app("shot", `${outDir}/.app.png`); spawnSync("sleep", ["0.5"]); app("shot", `${outDir}/.app.png`); spawnSync("sleep", ["0.3"]); }
  const r = app("do", "browser.screenshot", "--id", blockId, "--path", path);
  return r.path ?? r.error ?? r.raw ?? path;
}

await cdp("Page.enable"); await cdp("Log.enable"); await cdp("Log.clear"); await cdp("Runtime.enable");
consoleErrors.length = 0;
const result: any = { scenario, ua: await evaluate("navigator.userAgent"), webdriver: await evaluate("navigator.webdriver") };
const checks: Record<string, boolean> = {};
try {
  if (scenario === "W1") {
    result.load_ms = await load("https://www.google.com/?hl=en");
    // A consent page in some regions: accept nothing, reject if offered.
    const reject = await center('button[aria-label*="Reject"], #W0wltc');
    if (reject) { await click(...reject); await sleep(1500); }
    const box = await center('textarea[name="q"], input[name="q"]');
    checks.search_box = !!box;
    if (box) {
      await click(...box);
      await type("herdr terminal");
      const t = performance.now();
      await key("Enter", "Enter", 13);
      checks.results = await waitFor(`location.pathname === "/search" && document.querySelectorAll("#search a h3, #rso a h3").length > 0`, 20000);
      result.results_ms = Math.round(performance.now() - t);
      result.result_count = await evaluate(`document.querySelectorAll("#search a h3, #rso a h3").length`);
    }
    result.url = await evaluate("location.href");
    result.captcha = await evaluate(`/sorry|unusual traffic/i.test(location.href + document.body.innerText.slice(0, 2000))`);
    checks.no_captcha = !result.captcha;
    await sleep(1500);
    result.shot = shot("results");
  } else if (scenario === "W2") {
    result.load_ms = await load("https://github.com/ethanproia/herdr-app");
    checks.repo_page = await waitFor(`document.querySelector('[data-testid="latest-commit"], #readme, article.markdown-body')`);
    result.scroll = await scrollFps(2500, 300);
    checks.scrolled = result.scroll.scrolled > 0;
    result.shot_scrolled = shot("scrolled");
    const before = await evaluate("location.href");
    // A visible link to the src folder in the file list (a different page).
    await evaluate(`scrollTo(0, 0)`); await sleep(500);
    const link = await evaluate(`(() => { const a = [...document.querySelectorAll('a[href$="/tree/master/src"]')].find(a => a.offsetParent && a.getBoundingClientRect().width > 0); if (!a) return null; a.scrollIntoView({block:"center", behavior:"instant"}); return true; })()`) && await sleep(800) && await evaluate(`(() => { const a = [...document.querySelectorAll('a[href$="/tree/master/src"]')].find(a => a.offsetParent); const r = a.getBoundingClientRect(); return [r.x + r.width/2, r.y + r.height/2]; })()`);
    checks.link_found = !!link;
    if (link) result.under_point = await evaluate(`(() => { const e = document.elementFromPoint(${link[0]}, ${link[1]}); return e && (e.closest("a")?.getAttribute("href") || e.outerHTML.slice(0, 120)); })()`);
    if (link) {
      await click(...link);
      checks.followed = await waitFor(`location.href !== ${JSON.stringify(before)}`);
      result.followed_to = await evaluate("location.href");
      await sleep(1500);
      result.shot_followed = shot("followed");
      app("do", "browser.back", "--id", blockId);
      checks.back = await waitFor(`location.href === ${JSON.stringify(before)}`);
    }
  } else if (scenario === "W3") {
    result.load_ms = await load("https://www.youtube.com/watch?v=aqz-KE-bpKQ"); // Big Buck Bunny, public
    await sleep(3000);
    const consent = await center('button[aria-label*="Reject all"], form[action*="consent"] button');
    result.consent_shown = !!consent;
    if (consent) { await click(...consent); await sleep(3000); }
    result.codecs = await evaluate(`(() => { const v = document.createElement("video"); const ms = window.MediaSource; const t = s => ms && MediaSource.isTypeSupported(s); return { h264: v.canPlayType('video/mp4; codecs="avc1.42E01E"'), vp9: v.canPlayType('video/webm; codecs="vp9"'), av1: v.canPlayType('video/mp4; codecs="av01.0.05M.08"'), mse_h264: t('video/mp4; codecs="avc1.42E01E"'), mse_vp9: t('video/webm; codecs="vp9"'), mse_av1: t('video/mp4; codecs="av01.0.05M.08"'), aac: v.canPlayType('audio/mp4; codecs="mp4a.40.2"'), opus: v.canPlayType('audio/webm; codecs="opus"') }; })()`);
    checks.video_element = await waitFor(`document.querySelector("video")`, 15000);
    // Muted first: autoplay with sound needs a user gesture.
    await evaluate(`(() => { const v = document.querySelector("video"); if (v) { v.muted = true; return v.play().then(() => "ok", e => String(e)); } })()`);
    const btn = await center(".ytp-large-play-button, .ytp-play-button");
    if (btn && await evaluate(`document.querySelector("video")?.paused`)) await click(...btn);
    // Warm-up: the adaptive player starts low and climbs; wait for HD or the time limit.
    const warm = Number(process.env.FORK_WEB_WARMUP_S ?? 15) * 1000;
    const tw = Date.now();
    while (Date.now() - tw < warm && !(await evaluate(`(document.querySelector("video")?.videoHeight ?? 0) >= 720`))) await sleep(500);
    await sleep(2000);
    result.warmup_ms = Date.now() - tw;
    result.rendition_start = await evaluate(`(() => { const v = document.querySelector("video"); return v.videoWidth + "x" + v.videoHeight; })()`);
    const q0 = await evaluate(`(() => { const v = document.querySelector("video"); const q = v.getVideoPlaybackQuality(); return [v.currentTime, q.totalVideoFrames, q.droppedVideoFrames, performance.now()]; })()`);
    await sleep(10000);
    const q1 = await evaluate(`(() => { const v = document.querySelector("video"); const q = v.getVideoPlaybackQuality(); return [v.currentTime, q.totalVideoFrames, q.droppedVideoFrames, performance.now(), v.videoWidth, v.videoHeight, v.paused, v.error && v.error.message]; })()`);
    const secs = (q1[3] - q0[3]) / 1000;
    result.playback = { advanced_s: +(q1[0] - q0[0]).toFixed(2), fps: Math.round((q1[1] - q0[1]) / secs), dropped: q1[2] - q0[2], size: `${q1[4]}x${q1[5]}`, paused: q1[6], error: q1[7] };
    result.stream_mime = await evaluate(`(() => { try { const r = document.querySelector("#movie_player")?.getStatsForNerds?.(); return r && [r.codecs, r.resolution, r.optimal_format, r.current_quality ?? ""].filter(Boolean).join(" | "); } catch (e) { return String(e); } })()`);
    checks.plays = result.playback.advanced_s > 6;
    result.shot = shot("playing");
  } else if (scenario === "W4") {
    result.load_ms = await load("https://todomvc.com/examples/react/dist/");
    checks.app = await waitFor(`document.querySelector("input.new-todo, #todo-input")`);
    const input = await center("input.new-todo, #todo-input");
    if (input) {
      await click(...input);
      for (const item of ["write tests", "ship block", "measure fps"]) { await type(item); await key("Enter", "Enter", 13); await sleep(200); }
      checks.added = (await evaluate(`document.querySelectorAll(".todo-list li").length`)) === 3;
      const toggle = await center(".todo-list li:nth-child(2) input.toggle");
      if (toggle) await click(...toggle);
      await sleep(300);
      result.state = await evaluate(`[...document.querySelectorAll(".todo-list li")].map(li => [li.innerText.trim(), li.classList.contains("completed")])`);
      checks.toggled = JSON.stringify(result.state?.map((s: any) => s[1])) === "[false,true,false]";
      result.left = await evaluate(`document.querySelector(".todo-count")?.innerText`);
    }
    result.shot = shot("todos");
  } else if (scenario === "W5") {
    // The infinite-scroll library's own demo: loads more pages as you near the end.
    result.load_ms = await load("https://infinite-scroll.com/demo/full-page/");
    const h0 = await evaluate("document.documentElement.scrollHeight");
    result.scroll = await scrollFps(8000, 600);
    const h1 = await evaluate("document.documentElement.scrollHeight");
    result.height = [h0, h1];
    checks.scrolled = result.scroll.scrolled > 0;
    checks.loaded_more = h1 > h0;
    result.shot = shot("scrolled");
  } else throw new Error(`unknown scenario ${scenario}`);
} catch (e) {
  result.error = String(e);
}
await sleep(500);
result.console_errors = [...new Set(consoleErrors)].slice(0, 10);
result.console_error_count = consoleErrors.length;
result.checks = checks;
result.pass = !result.error && Object.values(checks).length > 0 && Object.values(checks).every(Boolean);
// The harness's METRIC lines (docs/browser-parity-harness.md), then the full JSON.
const metric = (id: string, v: any, unit: string, target: string) => v != null && console.log(`METRIC web_${scenario.toLowerCase()}_${id} ${v} ${unit} (target ${target})`);
metric("pass", result.pass ? 1 : 0, "bool", "1");
metric("load", result.load_ms, "ms", "<3000");
metric("scroll_fps", result.scroll?.fps, "fps", ">=55");
metric("play_fps", result.playback?.fps, "fps", ">=55");
metric("dropped", result.playback?.dropped, "frames", "0");
metric("results", result.results_ms, "ms", "<3000");
metric("console_errors", result.console_error_count, "count", "0 own");
console.log(JSON.stringify(result, null, 1));
ws.close();
process.exit(0);
