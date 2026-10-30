# Browser host: getting CEF on macOS (part 2, OQ2)

Measured on Ethan's Mac (macOS 27.0.1, arm64, Rust 1.95.0) on 2026-10-30. Anything not marked measured is from reading the crate source, not from a run.

## Answer
- **Use the `cef` crate from tauri-apps/cef-rs, pinned `=156.2.0+156.0.14`.** It wraps CEF 156.0.14 (Chromium 156.0.8078.48), ships generated bindings for `aarch64-apple-darwin`, and its `cef-dll-sys` build script downloads the matching prebuilt CEF "minimal" archive from `cef-builds.spotifycdn.com` (via the `download-cef` crate). `cef-sys` is unmaintained; hand-fetching a prebuilt CEF and writing bindings buys nothing.
- **Fetch:** set `CEF_PATH` to a cache outside the repo (for the fork: `$HERDR_FORK_DIR/cef`, default `~/work.noindex/herdr-fork/cef`). If the folder is missing, the build script downloads and unpacks it there once (`<CEF_PATH>/156.0.14/cef_macos_aarch64`); later builds reuse it.
- **Build needs `cmake` and `ninja`** (`brew install ninja`): `cef-dll-sys` compiles CEF's C++ wrapper (`libcef_dll`) with CMake + Ninja.

## System installs made for this work
- `brew install ninja` (2026-10-30, ninja for the `cef-dll-sys` CMake build). Nothing else.

## Sizes (measured)
| Item | Size |
|---|---|
| Download (`cef_binary_156.0.14+g4fa4ab5+chromium-156.0.8078.48_macosarm64_minimal.tar.bz2`) | 135 MB |
| Unpacked cache | 482 MB (framework 341 MB) |
| First build of `cef` + `cef-dll-sys` with download | about 2 min download+unpack, about 1 min compile |

## Helper app bundles
Chromium on macOS runs its sub-processes (GPU, renderer, plugin, alerts, utility) from five helper apps inside the main bundle. CEF loads its framework at runtime (`cef::library_loader::LibraryLoader`), not by linking, so a bare `cargo run` binary cannot start CEF: it must run from a bundle laid out as

```
herdr-browser-host.app/Contents/
  MacOS/herdr-browser-host
  Frameworks/Chromium Embedded Framework.framework
  Frameworks/herdr-browser-host Helper.app            (and " (GPU)", " (Renderer)", " (Plugin)", " (Alerts)")
  Info.plist  (LSUIElement = 1: no Dock icon, no menu bar)
```

`cef::build_util::mac::bundle()` (feature `build-util`) builds exactly this; the host crate wraps it as `scripts/fork-host-bundle.sh` (stage C). The helper can be the same binary started in helper mode (`cef::execute_process` returns early) or a tiny second binary; the bundle copies whichever is named.

## Signing and entitlements for local runs
> **Development only (SEC4).** The host runs Chromium with `no_sandbox: 1` (settings) and without the `sandbox` cargo feature: renderer processes are not sandboxed, so a renderer exploit runs with the user's full rights. That is acceptable only for local development on pages you choose. Before the host ships to anyone (or browses arbitrary sites as a daily browser), it must run sandboxed: `sandbox` feature on, `no_sandbox: 0`, every helper signed with the same identity as the main bundle, hardened runtime, and `cef::sandbox::Sandbox` initialized in each helper. The contract records this as a release gate (D10).

- arm64 binaries are ad-hoc signed by the linker; that is enough to run unsandboxed locally. No developer certificate is needed for `cargo build` + bundle + run.
- Build with `default-features = false` (no `sandbox` feature) and pass `--no-sandbox`; the Chromium sandbox on macOS needs a properly signed bundle with matching helper signatures.
- Pass `--use-mock-keychain` so Chromium never asks the login keychain (no prompt on Ethan's screen). Cookies/logins still persist in the profile, encrypted with a mock key (fine for a personal fork; a signed release would drop the flag).
- Distribution (later, out of scope here): sign the framework, each helper and the main app with the same identity, hardened runtime plus `com.apple.security.cs.allow-jit` and `allow-unsigned-executable-memory` on the renderer helper.

## Shared textures (OnAcceleratedPaint)
- Available: `cef_window_info_t.shared_texture_enabled` and `CefRenderHandler::OnAcceleratedPaint` are in the 156 bindings, and `AcceleratedPaintInfo.shared_texture_io_surface` is an `IOSurfaceRef` on macOS (the crate's own `osr_texture_import/iosurface.rs` imports it into Metal).
- Caveat for D3: CEF owns that IOSurface and recycles it; it is valid only during the callback. The host therefore does one **GPU blit** (Metal) per frame from CEF's surface into a host-owned IOSurface from the part 3 allocator, then sends that surface's ref and keeps it until `FrameAck`. No CPU copy and nothing on the socket, but not literally zero copies. Sending CEF's own surface id would race Chromium's reuse.
- Headless: windowless OSR (`window_info.windowless_rendering_enabled = 1`) never creates an NSWindow. CEF still needs an `NSApplication` subclass implementing `CefAppProtocol` in the host process (`cef::application_mac`); with LSUIElement it has no Dock icon and never takes focus. To confirm in stage C.

## Not verified yet
- That a real page renders off-screen and OnAcceleratedPaint fires on this macOS (stage C).
- That an IOSurface created by the host is visible to the app across processes (OQ1, part 3).

## Running the real host (stage C, measured 2026-10-30)
- Build the bundle: `scripts/fork-host-bundle.sh` (builds into `$CARGO_TARGET_DIR`, default `~/work.noindex/herdr-fork/target`) prints the bundle's executable. It is LSUIElement with the accessory activation policy: no window, no Dock icon, no focus change. Measured: the test runs never opened a window.
- Run the tests against it: `HERDR_BROWSER_HOST=<that path> cargo test -p herdr-browser-host --test protocol --test cef -- --test-threads=1`. Unset, the same protocol suite runs against `--stub`.
- Confirmed in that run: real frames via OnAcceleratedPaint (one Metal blit into a pool surface), pixel checks on local pages, resize, mouse/keys/text/IME/wheel reaching the page, eval, confirm dialog, screenshot PNG, CDP port, CdpTarget, and a cookie surviving a host restart.
- Shutdown: CEF's own teardown takes 0.3 to 4 s under load and Chromium's teardown watchdog used to kill it (exit 2). The host now closes every browser (waits for OnBeforeClose), flushes cookies (waits for the callback), then gives CefShutdown 2 s before exiting 0 itself. Measured: 22 of 22 clean exits during a parallel release build (15 by CEF, 7 by the fallback), cookie test passing.
- Early input: Chromium drops input sent before the first frame painted after the first load finishes; the host queues it until then and replays it.
- Wheel sign: CEF's convention, a negative `dy` scrolls the page down.
- DevTools has no window here: `DevTools` answers an Error pointing at the CDP port.

## CDP: pipe by default, TCP port dev-only
- `Hello.cdp_pipe: true` starts Chromium with `remote-debugging-pipe`: CDP on fds 3 (in) and 4 (out), NUL-framed JSON, and no TCP port (`Ready.cdp_pipe: true`, `cdp_port: None`). The launcher must pass both fds as pipes; otherwise the host answers an Error and runs without CDP. Measured (CEF 156, from the bundle): `Browser.getVersion` answers on fd 4, and `lsof -iTCP -sTCP:LISTEN` is empty for the host and its helpers (`tests/cef.rs` `cdp_over_fds_3_and_4`).
- `Hello.cdp_port` (with `cdp_pipe: false`) is a dev-only opt-in: any process of the user can attach to that port with full control of every page and the profile. The host test harness still uses it for its CDP check; the app does not (docs/browser-cdp-pipe.md).
