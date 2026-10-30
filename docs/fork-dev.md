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

Out of sight, today:

- No headless mode for the app window on macOS. `src/main.rs` opens the
  window with the default `show`/`focus` and calls `cx.activate(true)`, so a
  launch appears on screen and takes focus. `window` is therefore opt-in
  (`FORK_RUN_WINDOW=1`); do not use it on someone else's screen.
- `herdr-app shot` needs that window, so it is not offscreen either.
- gpui itself can open a window with `show: false, focus: false`
  (`tests/shot_pixels.rs`, the native path) and render it to an image. An
  env switch in `src/main.rs` that does the same for the app window, and
  skips `cx.activate`, would make `fork-run.sh window` + `shot` invisible.
  That is a src change and is not in this PR.
- Builds from `target/` never install the CLI link (`src/core_bin/cli_link.rs`)
  and never self-update (`update::start`, "a build from source").

`scripts/fork-run.sh <name> down` stops that fork's window (only if its
command line is this build) and its core.

## Offscreen render: `scripts/fork-render.sh`

Runs `tests/shot_pixels.rs` on gpui's headless renderer
(`HeadlessAppContext` + `current_headless_renderer`): it draws a scene,
reads back real GPU pixels and checks them. No window, no focus change.

Rendering a whole layout JSON (for example one with a browser block) to a
PNG needs the app's `AppWindow` built inside `HeadlessAppContext`. The app
has no library target, so a test can't reach it; the path is a hidden
subcommand (a proposed render verb taking a layout JSON and an output PNG) that opens the
window headless and calls `control::save_shot`. Until that exists, workers
diff layouts by adding a gpui scene to a test like `shot_pixels.rs`.

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
