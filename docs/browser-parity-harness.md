# Browser parity harness

One browser block in an offscreen GPUI window (nothing on screen, no focus), driven through the
app's real paths: input as window events into `BrowserInput`, commands through the agent
(`browser.*`), frames through `FrameView`. Spec: the parity test plan in the Hub
(projects/herdr-browser-fork/parity-test-plan.md). Code: `tests/common/`.

## Run

```
export HERDR_FORK_DIR=~/work.noindex/herdr-fork CARGO_TARGET_DIR=$HERDR_FORK_DIR/target
export HERDR_BROWSER_CEF_HOST=$HERDR_FORK_DIR/host-cef/herdr-browser-host.app/Contents/MacOS/herdr-browser-host
export HERDR_BROWSER_HOST=$HERDR_FORK_DIR/host-stub/herdr-browser-host     # optional, stub parts
scripts/browser-test-videos.sh                                              # once, for video tests
cargo test --locked --test browser_parity_smoke
```

Each test file is its own `[[test]]` with `harness = false` (GPUI on macOS needs the main thread):
add one to Cargo.toml per file.

## Validity rules (Astra review, spec section (f))
- **No CEF host is a failure**, not a skip: `Page::open(Host::Cef, ..)` panics unless
  `HERDR_PARITY_STUB_ONLY=1` marks the run stub-only (then it returns None).
- **Patterned pages for picture checks**: use a fixture with colour and text and call
  `harness::assert_patterned(&img, n, what)`; a solid colour (blank or stale frame) must fail.
- **Exact sizes after a resize**: `wait_frame_size(w, h, ..)` with the exact backing pixels
  (page area = block minus the 28 DIP bar, times the scale), not "a newer frame".
- **Numbers through the GPUI input path**: drive with `click`, `wheel`, `key`, `text`, `ime_*`,
  never by eval-dispatched DOM events. Eval only reads results back.
- Real sites (W1 to W5) are separate smoke tests, never mixed into fixture numbers.

## API (`harness::Page`, coordinates in page DIPs from the page's top-left)

| call | does |
|---|---|
| `Page::open(Host::Cef \| Host::Stub, url)`, `open_sized(.., w, h)` | window 1024x768 (or w x h), block filling it, waits until painted and loaded |
| `pump()`, `wait(timeout, \|p\| cond)`, `run_for(d)`, `wait_loaded(t)` | draw + run due work; poll a condition |
| `command(name, args)`, `eval(expr)`, `eval_until(expr, t, \|v\| ok)`, `navigate(url)` | `browser.*` through the agent |
| `click`, `click_with(x, y, button, mods)`, `double_click`, `right_click`, `mouse_down/up/move`, `drag(from, to, steps)` | mouse |
| `wheel(x, y, dx, dy, precise)` | precise: trackpad pixels; else wheel lines |
| `key("cmd-a")`, `text("héllo")`, `ime_set(text, sel)`, `ime_commit(text)`, `ime_cancel()`, `focus()` | keys (Keystroke syntax), platform text, IME |
| `resize_block(Some((w, h)))` | block size in DIPs (bar included); None fills |
| `frames()`, `clear_frames()`, `hash_frames(true)`, `wait_frame_after(seq, t)`, `wait_frame_size(w, h, t)`, `frame_stats(since)` | per frame: seq, size, arrived/painted/acked instants, content hash; fps, p50/p95/max gap, distinct hashes |
| `shot()`, `page_pixel(x, y)`, `page_region(x, y, w, h)`, `colours(img)`, `assert_patterned(img, n, what)` | pixels |
| `usage()`, `usage_over(d)` | CPU % (CPU-time delta / wall time) and RSS MB of the host and its helpers |
| `events()`, `state()`, `console()` | block events (Url, Focused, NewWindow), BrowserState, console lines |
| `harness::metric(id, value, unit, target)` | prints `METRIC <id> <value> <unit> (target <t>)` |

Frame hashing reads each shared surface (a copy per frame): it lowers measured fps, so leave it
off for frame-rate numbers and on for "did the content change" checks.

## Fixtures (`fixtures::Server::start().url("form.html")`)
`tests/fixtures/browser/`: `scroll` (2000 coloured rows), `animation` (CSS spin), `raf` (`rafCount`,
`rafTimes`), `form` (input, textarea, select, contenteditable, password; `window.log` of key,
input and composition events), `select` (paragraph, drag box, drop zone, link, resize cursor;
`selected()`, `dropped`, `menu`), `links` (same tab, `_blank`, `window.open`, plain link),
`download` (`/download/<name>` attachments), `file` (single, multiple; `files(id)`), `login`
(`POST /login` sets the `session` cookie, `/whoami`), `dark` (`scheme()`, black or white body),
`video` (`?src=test-60-vp9.webm`; `videoStats()`, `canPlay(type)`). Each page has
`rect(id)` for click targets. Server routes: `/video/<name>` (Range), `/download/<name>`,
`/login`, `/whoami`, `/slow?ms=N`. Videos: `scripts/browser-test-videos.sh` writes
`test-{30,60}-{vp9.webm,h264.mp4}` to `$HERDR_FORK_DIR/scratch/videos`.

## Example

```rust
include!("common/browser_harness.rs");
use harness::{Host, Page, metric};
use std::time::{Duration, Instant};

fn main() {
    let server = fixtures::Server::start();
    let mut p = Page::open(Host::Cef, &server.url("scroll.html")).expect("stub-only run");
    p.clear_frames();
    let start = Instant::now();
    for _ in 0..300 {
        p.wheel(300.0, 300.0, 0.0, -20.0, true); // 60 Hz trackpad scroll
        p.run_for(Duration::from_millis(16));
    }
    let s = p.frame_stats(start);
    assert!(p.eval("scrollY").unwrap().as_f64().unwrap() > 1000.0);
    metric("R1-scroll-fps", s.fps, "fps", ">= 55");
    metric("R1-p95-gap", s.p95_gap_ms, "ms", "<= 18");
}
```

## Notes
- The CEF host's windowless frame rate is 60 (`BrowserSettings.windowless_frame_rate` in
  crates/herdr-browser-host/src/cef/mod.rs).
- R6 baseline (smoke, 2026-10-30, debug build, loaded machine): one tab = 6 processes, 689 MB RSS,
  2.5 % idle CPU. Watch the RSS: a Chrome/Cursor idle tab is well under that.
- Known findings from the smoke run: drag-select selects nothing on the CEF host although the app
  sends button-held moves (I4); CEF has no default context menu offscreen (I5 needs the app menu).
