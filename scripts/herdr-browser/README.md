# herdr-browser

Agents drive Herdr's browser blocks through the CEF host's CDP port. Ported from the browser mod (its `herdr-browser` CLI, MCP server and Cursor-style `browser_*` tools); only endpoint discovery changed.

- `bin/herdr-browser <cmd> [tab] [key=value ...]`: the CLI; `bin/herdr-browser mcp` is an MCP stdio server with the same commands as `browser_*` tools. A tab is a browser block id.
- `core.mjs`: the daemon the CLI starts. It reads `$HERDR_APP_HOME/browser/cdp.json` (`{ "port": N, "targets": { "<block id>": "<cdp target id>" } }`, written by `src/browser/agent.rs`), keeps one CDP connection per block (so console and network logs survive between calls) and serves `POST /_agent` on the port in `browser/agent.port`.
- `agent.mjs`: the commands (snapshot with refs, click, type, fill, select_option, drag, scroll, wait, screenshot, console, network, eval, cdp, lock, tabs). Agents act only on blocks they opened, or ones the person allowed (`allow TAB on=true`).
- `open` runs `herdr-app do browser.open`; set `HERDR_BROWSER_BACKEND=standin` to use a plain headless Chrome instead (tests).

Test (headless, never a window, never the running app's home):

```sh
node scripts/herdr-browser/selftest.agent.mjs
```

It runs against `standin-chrome.mjs` (headless Google Chrome) until the CEF host serves CDP; point it at the real host by writing that host's `cdp.json`.
