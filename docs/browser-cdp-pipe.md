# Browser block: CDP over a pipe, the app as the only client (spec)

Status: spec, 2026-10-30. Closes the remaining SEC1c gap. Today the CEF host opens a TCP CDP port on 127.0.0.1 and `cdp.json` names it. Any process running as this user can scan for that port and attach with full control of every page and the profile; hiding or randomizing the port only delays it. Fix: no TCP port at all. The host serves CDP on a pipe that only the app holds, and the app brokers it behind the token, the grants and the method allow-list.

## Shape
```
herdr-browser CLI/MCP --(0600 unix socket + token)--> app broker (src/browser/broker.rs)
                                                          | grants, allow-list, redaction
                                                          v
                                   app host client (view.rs) --(fd 3/4 pipe, CDP)--> herdr-browser-host (CEF)
```

## Pieces, by owner

### fork-host: `crates/herdr-browser-host/src/cef/mod.rs`
- When `Hello.cdp` is `Pipe`, set no `remote_debugging_port` (leave it 0) and append the Chromium switch `remote-debugging-pipe` in `on_before_command_line_processing`. Chromium then reads CDP from fd 3 and writes to fd 4: NUL-terminated JSON, browser-level, the same shape Puppeteer's `pipe: true` uses.
- With `Hello.cdp` = `None`, no debugging at all.
- `Ready.cdp_port` stays `None`. Add `Ready.cdp_pipe: bool` (true once CEF has started with the switch).
- Keep sending `CdpTarget { id, target_id }` for each block.
- The DevTools message (`cef/mod.rs:528`) changes from "use the CDP port" to "inspect comes from a built-in panel later".
- Unverified: that CEF 156 passes `remote-debugging-pipe` through, and that fds 3/4 survive into the browser process when it is launched from the .app bundle. Probe this first: a 20-line host test that starts CEF with the switch and answers `Browser.getVersion` over fds 3/4.

### lead: `crates/herdr-browser-protocol/src/lib.rs`
```diff
-    Hello { version: u32, profile_dir: String, cdp_port: Option<u16> },
+    Hello { version: u32, profile_dir: String, cdp_port: Option<u16>, #[serde(default)] cdp_pipe: bool },
-    Ready { version: u32, cef_version: String, cdp_port: Option<u16> },
+    Ready { version: u32, cef_version: String, cdp_port: Option<u16>, #[serde(default)] cdp_pipe: bool },
```
`cdp_port` stays for the stand-in and old hosts. The app sends `cdp_port: None, cdp_pipe: true` by default; `HERDR_BROWSER_CDP_PORT=<n>` remains a dev-only opt-in (documented as unsafe).

### fork-block: `src/browser/view.rs` (host client)
- Before `Command::new(program)` (view.rs:261), make two pipes with `libc::pipe`: app-to-host and host-to-app. In `CommandExt::pre_exec`, `dup2` the host's read end to fd 3 and its write end to fd 4, and close the app's ends in the child. The app keeps the other two ends with `FD_CLOEXEC` set.
- Hold the ends in a `CdpPipe` that `HostClient` owns:
  - one reader thread splits on NUL and dispatches by `id` (replies) or `sessionId` (events);
  - `call(session: Option<&str>, method, params) -> Result<Value>` with a timeout;
  - `subscribe(session) -> Receiver<Value>` for events.
- On a host restart the pipe is new. Calls in flight fail with "host restarted", matching the reply generation in agent.rs.
- Expose `fn cdp(&self) -> Option<Arc<CdpPipe>>` to the broker.

### fork-agent (me): `src/browser/broker.rs` (new), `src/browser/agent.rs`, `scripts/herdr-browser/**`
- The broker listens on `$HERDR_APP_HOME/browser/agent.sock`: a 0600 socket in a 0700 dir. Token in `browser/agent.token` (0600), made by the app at start.
- Wire format: newline JSON.
  - First line `{ "token": "..." }`. A wrong or missing token closes the connection.
  - Then `{ "id": n, "block": "<block id>", "method": "...", "params": {...} }`, answered `{ "id": n, "result" | "error" }`.
  - Events arrive as `{ "block": "...", "method": "...", "params": {...} }`.
- Per block, the broker attaches once with `Target.attachToTarget { targetId, flatten: true }` (target id from `CdpTarget`) and keeps the `sessionId`. Clients never see target or session ids, and can't name a target that isn't a block.
- What the broker enforces:
  - `control` grant, or the block was opened by an agent;
  - the method allow-list (today's `CDP_ALLOWED`, moved to Rust);
  - the `capture` grant on pages with secret fields;
  - redaction of secret field values in results.
  - Browser-level methods (`Target.*` except `getTargetInfo`, `Browser.*`, `Storage.*`, `Network.getAllCookies`/`getCookies`/`setCookie`, `SystemInfo.*`) are always refused.
- Opening a page for an agent stays `browser.open` (an app command), never `Target.createTarget`.
- `cdp.json` becomes `{ "broker": "<abs path to agent.sock>", "token_file": "<abs path to agent.token>", "targets": { "<block id>": true } }`, with no port. The node daemon's `attach()` connects to the broker instead of `ws://.../devtools/page/<id>`; `agent.mjs` keeps its commands, console and network logs as they are.

### fork-toolchain: real-site scripts
Scripts that open the CDP port move to the broker: `herdr-browser <cmd>`, or the broker's socket with the token. They also need the person's grant, given by the test harness writing `grants.json` as the stand-in for the UI.

## What we lose
- The CEF DevTools frontend over HTTP (`/json`, `devtools://...?ws=`). Inspect comes later as a built-in panel driven through the broker. Acceptable per the lead.
- External CDP tools (chrome-devtools-mcp, Playwright `connectOverCDP`, Chrome's remote inspector) cannot attach to Herdr's browser. That's intended: they go through `herdr-browser` and its MCP.
- `/json/new` and `/json/close`: tests that need extra targets on the app use `browser.open` / `browser.close`.

## Tests
- X5a (host): `lsof -a -p <host pid> -iTCP -sTCP:LISTEN` prints nothing while browsers are open (and the same for every CEF helper pid of that host).
- X5b (broker):
  - a client without the token, or with a wrong one, is disconnected before any reply;
  - a plain WebSocket client given `cdp.json` has nothing to connect to (no port in it), and a ws upgrade on the broker socket is refused;
  - the socket and token files are 0600 and the dir is 0700.
- X1, X2, X3: the existing selftest checks run through the broker unchanged.
- Agent selftest:
  - `scripts/fork-browser-agent.sh` (headless fork, real CEF host, broker) runs the full `selftest.agent.mjs`;
  - the stand-in Chrome run (`node scripts/herdr-browser/selftest.agent.mjs`) stays for checks that need no app; its stand-in keeps a raw port, which is fine because it is a throwaway headless Chrome with a temp profile.
- A pipe unit test (fork-block): a fake host child that echoes NUL-framed JSON on fds 3/4; `call` round-trips, the reader splits on NUL, and calls in flight fail when the child dies.

## Order
1. fork-host probe (pipe works in CEF 156 from the bundle).
2. Lead: the protocol fields.
3. fork-block: `CdpPipe`.
4. Me: the broker, the cdp.json change, the daemon switch, X5a/X5b.
5. fork-toolchain: scripts.

Until step 4 lands, the TCP port stays as a dev-only path, and the browser is not shown to anyone else (X5 is a gate in the parity plan).
