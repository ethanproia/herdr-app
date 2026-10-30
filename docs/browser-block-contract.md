# Browser block: contract between the five parts

Status: v1, written before any part starts. The plan (CEF off-screen rendering, IOSurface frames into GPUI, full input and IME, agent hooks) is the goal; this file fixes the seams so five workers can build in parallel. The wire types live in `crates/herdr-browser-protocol` (owner: the lead; change only through the lead).

## Shape

```
the app (GPUI, main process)                           herdr-browser-host (CEF OSR, own process)
  layout block {"browser": {...}}                         one CEF browser per block id, windowless
  BrowserView entity (per block id)  <-- host.sock -->    protocol server, JSON lines
  paints IOSurface via gpui surface()  <-- IOSurface --   OnAcceleratedPaint -> shared IOSurface
  input + IME -> InputEvent  --------------------------->  SendMouse/Key/Ime events
  herdr-app do browser.*  ----> BrowserView ----------->  Eval/Screenshot/Navigate...
  CDP port (agents) <-------------------------------------  CEF remote debugging port
```

## Decisions (the lead made these; change only with a written reason)
- **D1 Separate host process.** CEF runs in `herdr-browser-host`, not inside the app: CEF on macOS wants its own NSApplication subclass (CefAppProtocol) and message loop, which GPUI owns in-process; a host also isolates Chromium crashes (`Crashed` message, the block shows a reload card). Open question OQ1 below can reverse this.
- **D2 Control channel:** Unix socket `$HERDR_APP_HOME/browser/host.sock`, newline-delimited JSON, types in the protocol crate. The app spawns the host lazily on the first browser block and restarts it on crash; one host per app, many browsers.
- **D3 Frames never cross the socket.** The host renders into IOSurfaces (BGRA, `size * scale` pixels) and sends a `SurfaceRef` (default: global IOSurface id; fallback: mach port). CEF's accelerated-paint surface is only valid inside its callback, so the host does one GPU blit per frame into its own pooled surface (no CPU copy). GPUI's macOS `surface()` only paints video formats, so painting BGRA needs a small marked fork patch in `vendor/gpui-pre-apple` (not `gpui-pre-macos`). The app wraps it as a CVPixelBuffer and paints it with GPUI's `surface()` element: no per-frame copy. One frame in flight per browser; the host keeps only the newest pending frame until `FrameAck` (T3 Code's scheme).
- **D4 Layout block:** `{ "browser": { "url": "https://...", "profile": "default" } }` as a new `Content::Browser` variant beside terminal/mod/context/agent, one CEF browser per block id; the url persists in the layout document. A browser can be a pane in a tab, a floating block or a layout block.
- **D5 Input:** all coordinates are DIPs from the block's top-left; the app sends mouse, wheel, key (DOM `key`/`code`/native code), text and IME events (`InputEvent`); the host answers `ImeCaret` so GPUI can position the candidate window and `Cursor` for the pointer shape.
- **D6 Profile and logins:** one CEF root cache at `$HERDR_APP_HOME/browser/profile`, shared by all blocks; the fork never reads another app's profile.
- **D7 Agents:** (a) `herdr-app do browser.open|navigate|back|forward|reload|stop|screenshot|eval|find|devtools`, (b) a CDP port (CEF remote debugging, default 0 = pick) advertised in `$HERDR_APP_HOME/browser/cdp.json` as `{ "port": N, "targets": { "<block id>": "<cdp target id>" } }`. The old mod's `herdr-browser` CLI, MCP and `browser_*` tool names retarget to that port; its `selftest.agent.mjs` is the behavior spec.
- **D8 Out of sight.** Nothing runs a visible window for tests: parts test against the **stub host** (`herdr-browser-host --stub`, serves deterministic IOSurfaces and echoes events, no CEF) and against the offscreen render-to-image harness (see `docs/fork-dev.md`). Real CEF runs only headless under a separate `HERDR_APP_HOME`. Nothing ever touches the user's running Herdr.
- **D9 Spec:** the 13-item browser parity checklist (kept with the project notes) and the old `browser_*` agent tool tests are the acceptance spec; every part says which items it moves.

## Ownership (one owner per file; others request changes through the lead)
| Part | Owns | Hot files it edits (only these lines) |
|---|---|---|
| 0 toolchain | scripts/fork-*.sh, docs/fork-dev.md | none |
| 1 block | `src/browser/mod.rs`, `src/browser/view.rs` (BrowserView entity, host lifecycle client, state), `Content::Browser` in `src/layout.rs`, docs for the layout block | `src/window.rs`: the `render_block` arm, focus and resize hooks; `src/layout.rs` |
| 2 host | `crates/herdr-browser-host/**` (CEF OSR, protocol server, `--stub`), CEF fetch and bundling in `scripts/` and `distribution/` | none in `src/` |
| 3 frames | `crates/herdr-browser-surface/**` (IOSurface create/export/import shared by host and app), `src/browser/frame.rs` (IOSurface to CVPixelBuffer to `surface()`), any GPUI vendor patch it needs | `vendor/gpui-pre-macos` only if unavoidable, as its own PR |
| 4 input | `src/browser/input.rs` (GPUI events to `InputEvent`, IME bridge, cursor mapping, focus) | none (BrowserView calls it) |
| 5 agent | `src/browser/agent.rs`, `scripts/herdr-browser/**` (CLI, MCP, tool names), docs/commands | `src/commands.rs`: the `browser.*` entries only |
| lead | `crates/herdr-browser-protocol/**`, this doc, the skeleton PR | reviews every PR |

Cross-part rules: the protocol crate is the only shared type surface; no part imports another part's internals (BrowserView exposes `FrameSink` and `InputSink` traits defined in `src/browser/mod.rs`; parts 3 and 4 implement against them). A part that needs a protocol change sends the lead the exact diff.

## Open questions (a part answers each in its first report)
- **OQ1 (part 3 + 2): ANSWERED 2026-10-30.** IOSurfaces cross processes on macOS 27: both the global id and the mach port transport work (tested, `cargo test -p herdr-browser-surface`); both APIs are deprecated and untested under sandbox or hardened runtime. Default GlobalId, fallback MachPort; D1 holds.
- **OQ2 (part 2): ANSWERED 2026-10-30.** The tauri-apps `cef` crate =156.2.0 (CEF 156.0.14, Chromium 156.0.8078.48) downloads a prebuilt CEF once (135 MB, 482 MB unpacked); needs cmake and ninja; the host must run from an .app bundle with the framework and helper apps (the crate's bundle helper builds it, LSUIElement so no Dock icon); ad-hoc signing with sandbox off, `--no-sandbox` and `--use-mock-keychain` is enough locally. See docs/fork-host.md.
- **OQ3 (part 4):** where GPUI exposes IME (InputHandler, marked text, candidate window position) and what must change in the vendored gpui.
- **OQ4 (part 1):** how focus and key routing choose between terminal, mod and browser blocks, and which `window.rs` hooks are minimal.
- **OQ5 (lead):** coordination with the separate agent-attention work: where its attention state should read browser events.

## Definition of done per part
A small PR into `ethan/browser-block`, its tests passing under `cargo test --locked` (offscreen, no window), a rendered PNG or a transcript as evidence, the checklist items it moves, and an honest list of what is unverified in a real window.

## Known red check: third-party notices
`tests/notices.rs` hashes every Cargo manifest and Cargo.lock, so any PR that adds a crate or dependency makes it fail until `just notices` is rerun (needs `cargo install cargo-about --version 0.9.3 --locked --features cli`). Feature PRs do not regenerate `THIRD-PARTY-NOTICES`: stacked PRs would conflict on it. The lead regenerates it once, at the top of the stack, and reports the check as expected-red until then.

