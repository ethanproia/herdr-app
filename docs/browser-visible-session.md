# Browser block: the visible session (about 10 minutes, Ethan with the lead)

The only part of the parity pass that needs a real window and a human hand: on-screen repaint,
trackpad feel, real IME, the native menus and dialogs. Everything here was rehearsed headless first
(`scripts/fork-browser-visible-dryrun.sh`); nothing in it touches the Herdr Ethan uses: the fork
runs on its own home (`~/.herdr-fork-home/visible`), its own core and its own browser host.

## Before (the lead, 2 minutes, nothing on screen yet)
1. Videos (once): `scripts/browser-test-videos.sh`. Pages and videos together in a scratch folder: `S=~/work.noindex/herdr-fork/scratch/visible-site; mkdir -p $S; ln -sf $PWD/tests/fixtures/browser/* $S/; ln -sfn ~/work.noindex/herdr-fork/scratch/videos $S/video`.
2. Serve them: `python3 -m http.server 8765 -d $S &`.
3. Host bundle from current parity: `scripts/fork-host-bundle.sh` → export `HERDR_BROWSER_HOST=<printed path>`.
4. `export HERDR_BROWSER_STATS=1` (repaint statistics to `~/.herdr-fork-home/visible/home/browser/stats.log`).
5. Tell Ethan: a second Herdr window ("fork") opens and takes focus; his own Herdr is untouched.

## The session (Ethan's hands; the lead types the `do` lines in a terminal)
`F=scripts/fork-run.sh visible` below.

| # | Lead runs | Ethan does | Ethan should see |
|---|---|---|---|
| 1 | `FORK_RUN_WINDOW=1 $F window`, then `$F do space.new '{"name":"browser"}'` | close the "Welcome to Herdr" card (×, top right of the card; a fresh fork home shows it once) | a Herdr window with one space |
| 2 | `$F do browser.open '{"url":"http://127.0.0.1:8765/scroll.html"}'` | nothing | a tab with coloured rows, the address bar on top |
| 3 | | two-finger scroll fast down and back up for 5 s | smooth scrolling, no blank frames, no tearing (the stats log measures repaint rate) |
| 4 | `$F do browser.navigate '{"url":"http://127.0.0.1:8765/form.html"}'` | click the first text box, type `héllo`, then switch to Japanese input and type `nihon`, Enter | the text appears as typed; the candidate window sits under the caret; 日本 lands in the box |
| 5 | | right-click in the page | Herdr's menu (Back, Forward, Reload, Copy, Paste…), Esc closes it |
| 6 | | cmd-F, type `row`… (on scroll.html) | a find bar under the address bar, "n of m", Enter steps |
| 7 | | cmd-plus twice, cmd-0 | the page grows (label 125 %), then back to 100 % |
| 8 | | cmd-shift-C, hover elements, click one; 💬 on, pick again, type a note, Enter | a blue highlight follows the pointer; a comment field under the element; a numbered red pin; the badge "1 · Send" |
| 9 | | cmd-shift-M, drag a rectangle, cmd-Z, Done | a red rectangle over the page, then gone |
| 10 | `$F do browser.navigate '{"url":"http://127.0.0.1:8765/video.html?src=test-60-vp9.webm"}'` | watch 5 s | the test pattern plays smoothly |
| 11 | `$F do browser.navigate '{"url":"http://127.0.0.1:8765/links.html"}'` | cmd-click "plain link" | a second browser opens beside the first |
| 12 | `$F down` | nothing | the fork window closes; no browser host left (`pgrep -f herdr-fork-home/visible` prints nothing) |

## After (the lead)
- Repaint rate: `grep summary ~/.herdr-fork-home/visible/home/browser/stats.log | tail` (paints/s, p50/p95 interval, p95 latency, dropped) for step 3 and step 10. Target (R1, R2): >= 55 paints/s while scrolling, >= 95 % of the video's fps.
- Note Ethan's words for each step that felt off; they go into docs/browser-parity-report.md.
- Ask before: running Cursor for a 2-minute reference (vendor app, only with his OK).

## Dry run (done 2026-10-30, headless, real CEF)
Result: the page opened (title "scroll"), a scripted 60-frame scroll reached scrollY 2400, find counted
1111 "row 1" matches, the 60 fps VP9 video decoded 188 frames in 3.0 s (about 62 fps, no error),
`down` left no process. Seen in the pictures: the "Welcome to Herdr" card on a fresh home (now step 1),
the browser tab named after the page title ("video"). The fork's core keeps spaces between runs:
use a new fork name (or `$F hc` to remove old spaces) so Ethan starts clean. The headless window draws only when asked, so stats.log has no summary
lines there: the repaint rate needs the visible session (step 3, step 10).


`scripts/fork-browser-visible-dryrun.sh` runs the same steps headless (no window, no focus) through
`herdr-app do` and `herdr-app shot`, saving one picture per step and the stats summary. Steps that
need a hand (3, 4, 5, 6, 7, 8, 9, 11) are rehearsed through the agent commands that reach the same
code, or marked "hand only".
