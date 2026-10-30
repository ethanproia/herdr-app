#!/usr/bin/env bash
# End to end through the app's real command path, out of sight: starts this checkout's
# herdr-app headless on its own home and core (scripts/fork-run.sh), then drives a browser
# block with `herdr-app do browser.*` against the real CEF host and checks every answer.
#
#   HERDR_BROWSER_HOST=<bundle>/Contents/MacOS/herdr-browser-host \
#   HERDR_APP_BIN=<target>/debug/herdr-app scripts/fork-browser-e2e.sh [out-dir]
#
# Never touches the Herdr you use: fork-run.sh refuses the real home, and `down` stops only
# the fork's own window, core and browser host.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
: "${HERDR_BROWSER_HOST:?set HERDR_BROWSER_HOST to the executable inside the CEF host bundle}"
export HERDR_BROWSER_HOST
name=browser-e2e
out=${1:-$HOME/.herdr-fork-home/$name/out}
mkdir -p "$out"
run() { "$here/scripts/fork-run.sh" "$name" "$@"; }
# `do` answers with JSON; a failed check prints it and stops.
do_() { run do "$1" "$2" || true; }
check() { # json, python expression over `r`, what
  python3 -c 'import json,sys; r=json.loads(sys.argv[1]); sys.exit(0 if eval(sys.argv[2]) else 1)' "$1" "$2" \
    || { echo "FAIL $3: $1" >&2; exit 1; }
  echo "ok   $3"
}

trap 'run down >/dev/null 2>&1 || true' EXIT
run down >/dev/null 2>&1 || true
run up

r=$(do_ space.new '{"name":"e2e"}'); check "$r" '"error" not in r' "space.new"
r=$(do_ browser.open '{"url":"data:text/html,<title>one</title><textarea></textarea>","place":"tab"}')
check "$r" '"id" in r' "browser.open makes a browser pane"
id=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["id"])' "$r")
args() { python3 -c 'import json,sys; d=json.loads(sys.argv[2]); d["id"]=sys.argv[1]; print(json.dumps(d))' "$id" "$1"; }

# The page is made on the host by the first command (the headless window never paints it);
# eval until it has loaded.
for _ in $(seq 60); do
  r=$(do_ browser.eval "$(args '{"expr":"document.title"}')")
  python3 -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1]).get("value")=="one" else 1)' "$r" && break
  sleep 0.5
done
check "$r" 'r.get("value") == "one"' "browser.eval reads the page (title one)"

r=$(do_ browser.navigate "$(args '{"url":"data:text/html,<title>two</title><h1 style=\"color:red\">two</h1>"}')")
check "$r" '"error" not in r' "browser.navigate"
for _ in $(seq 60); do
  r=$(do_ browser.eval "$(args '{"expr":"document.title"}')")
  python3 -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1]).get("value")=="two" else 1)' "$r" && break
  sleep 0.5
done
check "$r" 'r.get("value") == "two"' "the page navigated (title two)"

r=$(do_ browser.eval "$(args '{"expr":"6*7"}')"); check "$r" 'r.get("value") == 42' "browser.eval answers 6*7 = 42"

shot=$out/page.png
r=$(do_ browser.screenshot "$(args "{\"path\":\"$shot\"}")")
check "$r" 'r.get("path")' "browser.screenshot answers its path"
python3 -c 'import sys; d=open(sys.argv[1],"rb").read(8); sys.exit(0 if d.startswith(b"\x89PNG") else 1)' "$shot" \
  || { echo "FAIL screenshot is not a PNG: $shot" >&2; exit 1; }
echo "ok   screenshot is a PNG ($(wc -c <"$shot" | tr -d ' ') bytes): $shot"

cdp=$HOME/.herdr-fork-home/$name/home/browser/cdp.json
r=$(cat "$cdp" 2>/dev/null || echo '{}'); check "$r" "'$id' in r.get('targets', {})" "cdp.json lists the page's CDP target"
run shot "$out/window.png" >/dev/null && echo "ok   window picture: $out/window.png"

r=$(do_ browser.close "$(args '{}')"); check "$r" 'r.get("closed") is True' "browser.close"
sleep 0.5
r=$(cat "$cdp" 2>/dev/null || echo '{}'); check "$r" "'$id' not in r.get('targets', {})" "the closed page left cdp.json"
r=$(do_ browser.eval "$(args '{"expr":"1"}')"); check "$r" '"error" in r' "a closed browser refuses commands"
echo "browser e2e passed"
