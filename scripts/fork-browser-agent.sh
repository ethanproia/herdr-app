#!/usr/bin/env bash
# The agent selftest (scripts/herdr-browser/selftest.agent.mjs, 37 checks) against the REAL CEF host,
# through a headless fork of this checkout (scripts/fork-run.sh): never a window, never the Herdr you use.
#
#   HERDR_BROWSER_HOST=<bundle>/Contents/MacOS/herdr-browser-host scripts/fork-browser-agent.sh [name]
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
: "${HERDR_BROWSER_HOST:?set HERDR_BROWSER_HOST to the executable inside the CEF host bundle}"
export HERDR_BROWSER_HOST
name=${1:-ag2}
root=$HOME/.herdr-fork-home/$name
run() { "$here/scripts/fork-run.sh" "$name" "$@"; }
trap 'run down >/dev/null 2>&1 || true' EXIT
run down >/dev/null 2>&1 || true
run up
run do space.new '{"name":"agent"}' >/dev/null || true # the fork's home keeps spaces between runs
# `herdr-app do` for this fork, for the daemon and the test.
printf '#!/bin/sh\nexec %q %q "$@"\n' "$here/scripts/fork-run.sh" "$name" >"$root/bin/herdr-app-fork"
chmod +x "$root/bin/herdr-app-fork"
HERDR_BROWSER_SELFTEST=app HERDR_BROWSER_DIR="$root/home/browser" HERDR_BROWSER_APP="$root/bin/herdr-app-fork" \
  node "$here/scripts/herdr-browser/selftest.agent.mjs"
