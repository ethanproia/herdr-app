#!/usr/bin/env bash
# Headless rehearsal of docs/browser-visible-session.md: the same pages and commands, no window,
# no focus, own home (fork-run.sh "visible-dry"). One picture per step and the stats summary in
# the out folder. Hand-only steps are rehearsed through the commands that reach the same code.
#
#   HERDR_BROWSER_HOST=<bundle exe> HERDR_APP_BIN=<target>/debug/herdr-app scripts/fork-browser-visible-dryrun.sh [out]
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
: "${HERDR_BROWSER_HOST:?set HERDR_BROWSER_HOST to the CEF host bundle executable}"
export HERDR_BROWSER_HOST HERDR_BROWSER_STATS=1
name=visible-dry
out=${1:-${HERDR_FORK_DIR:-$HOME/work.noindex/herdr-fork}/scratch/visible-dry}
mkdir -p "$out"
run() { "$here/scripts/fork-run.sh" "$name" "$@"; }
step() { echo "== $1"; }
shot() { run shot "$out/$1.png" >/dev/null && echo "   picture $out/$1.png"; }
ok() { python3 -c 'import json,sys; r=json.loads(sys.argv[1]); sys.exit(1 if isinstance(r,dict) and "error" in r else 0)' "$1" || { echo "   FAIL: $1" >&2; exit 1; }; }

port=8765
# The fixture pages plus the test videos under /video/ (as the test server serves them).
site=$out/site
rm -rf "$site"; mkdir -p "$site"
ln -s "$here"/tests/fixtures/browser/* "$site"/
ln -s "${HERDR_FORK_DIR:-$HOME/work.noindex/herdr-fork}/scratch/videos" "$site/video"
python3 -m http.server $port -d "$site" >/dev/null 2>&1 &
srv=$!
trap 'kill $srv 2>/dev/null; run down >/dev/null 2>&1 || true' EXIT
base=http://127.0.0.1:$port
run down >/dev/null 2>&1 || true
run up

step "1 space"; r=$(run do space.new '{"name":"browser"}' || true); ok "$r"
step "2 open scroll.html"
r=$(run do browser.open "{\"url\":\"$base/scroll.html\"}" || true); ok "$r"
id=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["id"])' "$r")
a() { python3 -c 'import json,sys; d=json.loads(sys.argv[2]); d["id"]=sys.argv[1]; print(json.dumps(d))' "$id" "$1"; }
for _ in $(seq 40); do r=$(run do browser.eval "$(a '{"expr":"document.title"}')" || true); [[ $r == *scroll* ]] && break; sleep 0.25; done
echo "   title: $r"; shot 02-scroll
step "3 scroll (hand only; rehearsed by script scrolling)"
scroll_js='(async()=>{for(let i=0;i<60;i++){scrollBy(0,40);await new Promise(r=>requestAnimationFrame(r))}return scrollY})()'
args=$(python3 -c 'import json,sys; print(json.dumps({"id":sys.argv[1],"expr":sys.argv[2]}))' "$id" "$scroll_js")
r=$(run do browser.eval "$args" || true); echo "   $r"; shot 03-scrolled
step "4 form"
run do browser.navigate "$(a "{\"url\":\"$base/form.html\"}")" >/dev/null || true; sleep 1; shot 04-form
step "6 find (agent find reaches the host's find)"
run do browser.navigate "$(a "{\"url\":\"$base/scroll.html\"}")" >/dev/null || true; sleep 1
r=$(run do browser.find "$(a '{"text":"row 1"}')" || true); echo "   $r"
step "10 video"
run do browser.navigate "$(a "{\"url\":\"$base/video.html?src=test-60-vp9.webm\"}")" >/dev/null || true; sleep 3
r=$(run do browser.eval "$(a '{"expr":"videoStats()"}')" || true); echo "   $r"; shot 10-video
step "11 links"
run do browser.navigate "$(a "{\"url\":\"$base/links.html\"}")" >/dev/null || true; sleep 1; shot 11-links
step "12 down"
run down >/dev/null
sleep 1
if pgrep -f "herdr-fork-home/$name" >/dev/null; then echo "   FAIL: a process of the fork is left"; exit 1; fi
echo "   nothing left running"
stats=$HOME/.herdr-fork-home/$name/home/browser/stats.log
echo "== stats ($stats)"; grep summary "$stats" 2>/dev/null | tail -5 || echo "   no summary lines (a headless window draws only when asked)"
echo "dry run done"
