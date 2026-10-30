# Fork development (browser block)

Notes for building and checking the browser-block fork on a Mac without
touching the Herdr you use every day.

## Toolchain

The versions CI uses (`.github/workflows/ci.yml`):

- Rust 1.95.0: `curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.95.0 --profile minimal --no-modify-path`, then `. ~/.cargo/env`
- Zig 0.16.0: `brew install zig@0.16` (plain `zig` in Homebrew is newer)
- `just`: `brew install just`
- Submodules: `git submodule update --init --recursive` (vendor/ghostty)

Then `cargo build --locked` and `cargo test --locked`. Measured 2026-10-30 on
Apple Silicon, cold: build 885 s, test 170 s more, `target/` 11 GB; all tests
pass, 4 warnings, all in vendor/quickjs-jit-runtime.

## Running the fork: `scripts/fork-run.sh`

`scripts/fork-run.sh <name> <command>` sets its own `HERDR_APP_HOME`
(`~/.herdr-fork-home/<name>/home`), `HC_SESSION` (`herdr-fork-<name>`, a
separate core), `XDG_STATE_HOME`, `HERDR_APP_ID` (its own dock icon) and an
`open` stub that logs links instead of opening them. It refuses
`~/.config/herdr-app` as a home.

Out of sight: `HERDR_APP_HEADLESS=1` (src/main.rs, src/control.rs) opens the
window with `show: false, focus: false`, skips `cx.activate`, sets the macOS
activation policy to prohibited (no Dock icon, never frontmost), never
raises a running window and quits instead of showing the setup screen.
`scripts/fork-run.sh <name> up` starts it that way; `shot`, `do` and
`render` work against it. `window` (visible, takes focus) stays behind
`FORK_RUN_WINDOW=1`.

Why the guard exists: on 2026-10-30 a first probe ran a build from before
the switch. Its core failed to start, so it showed the setup screen, possibly
on Ethan's screen, for up to about a minute before it was killed.
`fork-run.sh up` now refuses a binary that does not contain
`HERDR_APP_HEADLESS`.

Check it yourself: `scripts/fork-window-probe <pid>` (pid from
`~/.herdr-fork-home/<name>/pid`) prints the process's windows, how many are
on screen, its activation policy (2: no Dock icon) and whether it is active,
from CGWindowList. Expect `windows=1 onscreen=0 activation=2 active=false`. It
reads the window list only; System Events via osascript can raise a
permission prompt, so don't use it.

The core's state lives in `/tmp/hf-<name>`: macOS caps socket paths at 103
bytes, and a path under the home folder is too long.

Builds from `target/` never install the CLI link (`src/core_bin/cli_link.rs`)
and never self-update (`update::start`, "a build from source").

`scripts/fork-run.sh <name> down` stops that fork's window (only if its
command line is this build) and its core.

## Offscreen render: `scripts/fork-render.sh`

`scripts/fork-render.sh <layout.json> <out.png> [name]` starts the app
headless on its own home and core, renders the layout and stops it. Under
the hood: `herdr-app render --layout <file.json> --out <file.png>` copies the
layout to `$HERDR_APP_HOME/layouts/render.json`, runs `layout.switch` to it
(a personal layout, so the whole tree applies) and saves a shot through
`control::save_shot`.

With no arguments it runs `tests/shot_pixels.rs` on gpui's headless
renderer, a GPU readback check with no app.

Diffing: keep a reference PNG next to the test and compare pixels
(the `image` crate is already a dependency) with a small tolerance, as
`shot_pixels.rs` does with exact pixels.

For CEF later: the browser block draws a frame that CEF hands over. In an
offscreen render, use CEF's software `OnPaint` path (a BGRA buffer) or a
fixed test frame, since a headless gpui renderer has no IOSurface from a
live CEF.

## CEF on macOS (not integrated yet)

Checked 2026-10-30. Items marked unconfirmed need the CEF headers or
cef-rs examples.

- Crate: `cef` (tauri-apps/cef-rs), 156.2.0+156.0.14, published 2026-10-25;
  the part after `+` is the CEF version it wraps. Actively maintained.
  <https://crates.io/crates/cef>, <https://github.com/tauri-apps/cef-rs>
- Binary: `cef-dll-sys`'s build script downloads CEF into `OUT_DIR`. To keep
  one copy: `cargo run -p export-cef-dir -- --force ~/.local/share/cef` and set
  `CEF_PATH` to that folder.
- Version: CEF 156.0.14+g4fa4ab5, Chromium 156.0.8078.48 (2026-10-24).
  macosarm64 minimal is about 135 MB, standard about 312 MB.
  <https://cef-builds.spotifycdn.com/index.json>
- Bundle: `Chromium Embedded Framework.framework` in `Contents/Frameworks`
  plus a separate `<app> Helper.app`; one executable for everything is not
  supported on macOS. <https://chromiumembedded.github.io/cef/general_usage>
  cef-rs ships `bundle-cef-app` (`[package.metadata.cef.bundle]` in
  Cargo.toml). The GPU/Renderer/Plugin/Alerts helper variants are
  unconfirmed.
- `cargo run` without a bundle: unconfirmed. CEF documents
  `browser_subprocess_path`; whether `framework_dir_path`/`main_bundle_path`
  are enough for an unbundled binary is open. Plan on a bundle step.
- Sandbox and signing: CEF samples load the framework at runtime
  (`CefScopedLibraryLoader`) for the macOS sandbox; `no_sandbox`, local
  ad-hoc signing and entitlements are unconfirmed.
- OSR with shared textures: set `windowless_rendering_enabled` and
  `shared_texture_enabled`; frames arrive in `OnAcceleratedPaint` as an
  IOSurface. The `cef` crate's `accelerated_osr` feature imports it through
  Metal; import inside the callback, on the same GPU device, else it falls
  back to `OnPaint`. <https://github.com/chromiumembedded/cef/issues/2575>,
  <https://docs.rs/crate/cef/latest/source/src/osr_texture_import/mod.rs>
  (docs page is for 148.1.0). The first CEF version with macOS support is
  unconfirmed.

## Known issues
- `files::tests::a_watched_folder_reports_changes_and_misses_none_between_waits` can hang
  (idle at 0% CPU for minutes) on a busy Mac; run the suite with
  `cargo test --locked --bin herdr-app -- --skip a_watched_folder` until it is fixed.
- A headless window (`HERDR_APP_HEADLESS=1`) is never asked to display, so it paints only when
  something asks: `herdr-app shot` draws first (HERDR: in window.rs), so the picture is current.
- Disposable output lives under `$HERDR_FORK_DIR` (default `~/work.noindex/herdr-fork`, which
  Spotlight skips): `target/` (the one `CARGO_TARGET_DIR`), `wt/` (worktrees), `cef/`
  (`CEF_PATH`), `scratch/`. New worktrees take ghostty with
  `git submodule update --init --reference ~/code/herdr-app/.git/modules/vendor/ghostty vendor/ghostty`.

## Dynamic-site checks: `scripts/fork-web.ts`

Parity plan section W, against a headless fork, one site at a time:

```sh
export HERDR_APP_BIN=$HOME/work.noindex/herdr-fork/target/debug/herdr-app
export HERDR_BROWSER_HOST=$HOME/work.noindex/herdr-fork/host-cef/herdr-browser-host.app/Contents/MacOS/herdr-browser-host
scripts/fork-run.sh web up
scripts/fork-run.sh web do space.new
scripts/fork-run.sh web do browser.open --url about:blank
scripts/fork-run.sh web shot /tmp/hf-web-start.png   # the first draw starts the host
bun scripts/fork-web.ts web W1                       # W1..W5; JSON with pass, load_ms, fps, console errors, shots
scripts/fork-run.sh web down
```

Input goes through the CDP port (`Input.*`, the agent path); screenshots
through `browser.screenshot`. Fps is frames the page painted (rAF) while
wheel-scrolling, or decoded video frames for W3, not frames the app drew.
A headless window never draws, so until the app acks frames itself when
headless, the script draws the app before each screenshot
(`FORK_WEB_NO_DRAW=1` skips that).
