# Browser parity report: responsiveness (section R)

Run 2026-10-30 16:00, release build, offscreen, real CEF host, through the Chromium slot.
Load average 6.66 at start (just above the quiet bar of 6; the 15-minute average was settling
from a paused job), 7.27 at end. 5 runs per number (R2 and R6 1-tab: 3). Reproduced by the lead
(R1 59.8 fps, R4 27/29 ms, R5 47/83 ms).

Rerun: `scripts/browser-perf-run.sh` (or `scripts/browser-perf-run.sh R1,R4`); test
`tests/browser_perf.rs`.

## Labels
- **delivery**: frames the host delivered to the app (arrival in the app).
- **drawn**: frames the app drew into a GPUI scene, with the harness pumping at 60 Hz.
- Neither is on-screen presentation. On-screen cadence comes from the one visible session with
  `HERDR_BROWSER_STATS=1` (`<app home>/browser/stats.log`).

## Results

| id | measure | label | result (mean ± sd, p50) | target | |
|----|---------|-------|--------------------------|--------|---|
| R1 | scroll, 60 Hz trackpad wheel for 5 s | delivery | 56.5 ± 4.3 fps, p50 58.4 (runs 54 to 60) | >= 55 | PASS |
| R1 | same | drawn | 56.3 fps | >= 55 | PASS |
| R1 | p95 gap between frames | delivery | 25.9 ms, p50 20.4 | (none in plan) | |
| R2 | VP9 30 fps | delivery vs source | 100.7 % | >= 95 % | PASS |
| R2 | VP9 60 fps | delivery vs source | 100.2 % | >= 95 % | PASS |
| R2 | H.264 30 and 60 fps | decode | does not decode: "no supported source" (CEF standard build, no proprietary codecs) | | FAIL |
| R3 | CSS animation | delivery | 60.0 fps | >= 55 | PASS |
| R3 | requestAnimationFrame | page rAF / delivery | 60.0 / 60.3 fps | >= 55 | PASS |
| R4 | click to first changed frame, p95 (50 samples) | delivery / drawn | 28.9 / 32.4 ms (p50 12.9) | p95 <= 50 | PASS |
| R4 | key to first changed frame, p95 | delivery | 28.3 ms (p50 12.7) | p95 <= 50 | PASS |
| R5 | Resize to first frame at the exact new size | delivery | mean 46, p50 34, p95 83 ms | <= 100 | PASS |
| R5 | host: Resize received to first paint at that size | host | p50 23 ms | | |
| R5 | 2 s of continuous resize | delivery | 28.9 fps, 0 blank of 20 shots | no blank | PASS |
| R5 | split and resize of two blocks at once | | not measured: the harness has one block per window | | |
| R6 | idle CPU, 1 tab (form page, blinking caret) | host + helpers | 2.01 ± 0.29 % of a core | <= 2 % | borderline |
| R6 | RSS, 1 tab | host + helpers | 620 MB, 6 processes | report | |
| R6 | 3 tabs | 3 separate hosts | 2.5 % CPU and 622 MB per tab, 18 processes | report | |
| R6 | 5 tabs | 5 separate hosts | 1.7 % CPU and 624 MB per tab, 30 processes | report | |
| R6 | CPU while scrolling | host + helpers | 16 % of a core | | |
| A4 | host frame copy incl. GPU wait, p95 | host | 1.3 ms | | |

Tabs: the harness refuses `browser.open` from the page, so 3 and 5 tabs are separate host
processes. That overstates memory against one shared host.

## Under load
An earlier run under heavy load (load average 19 to 148, another job) gave R1 52.9 fps,
R4 p95 69 ms (click) and 63 ms (key), and a frame copy p95 of 19 ms. R2, R3 and R5 still passed.
The copy waits on the GPU, so GPU contention shows up there first.

## How frames flow (C4)
- Receipt: the app imports the surface; nothing is sent.
- Presentation: `FrameAck` only after a draw in a visible window. At most two frames are unacked.
- Safe reuse: the host pool skips any IOSurface whose kernel-wide use count is held: the app's
  CVPixelBuffer, the scene, and Metal textures until GPU completion.
- Hidden or occluded (`SetVisible`): the host stops painting and copying and drops pending
  frames. It repaints in full on resume. Screenshots still work while hidden.
