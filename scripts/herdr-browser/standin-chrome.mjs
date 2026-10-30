// A headless Google Chrome standing in for the CEF host (herdr-browser-host) until it serves CDP:
// writes <dir>/cdp.json = { port, targets: {} } like the app does on the host's Ready. Tests only; headless,
// its own temp profile, never a window. Returns { port, stop }.
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const CHROME = process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

export async function startStandin(dir) {
  const profile = join(dir, "standin-profile");
  mkdirSync(profile, { recursive: true });
  const chrome = spawn(CHROME, ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "--no-first-run",
    "--no-default-browser-check", "--use-mock-keychain", "--window-size=900,700", "about:blank"], { stdio: "ignore" });
  let port;
  for (let i = 0; i < 100 && !port; i++) {
    await new Promise((r) => setTimeout(r, 100));
    try { port = Number(readFileSync(join(profile, "DevToolsActivePort"), "utf8").split("\n")[0]); } catch {}
  }
  if (!port) { chrome.kill(); throw new Error("stand-in Chrome did not start"); }
  writeFileSync(join(dir, "cdp.json"), JSON.stringify({ port, targets: {} }));
  return { port, profile, stop: () => chrome.kill() };
}
