#!/usr/bin/env bash
# Offscreen render, no window and no focus change. See docs/fork-dev.md.
#
#   scripts/fork-render.sh                       the GPU readback check (tests/shot_pixels.rs)
#   scripts/fork-render.sh <layout.json> <out.png> [name]   the app, headless, renders a layout
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -ge 2 ]]; then
  name=${3:-render}
  layout=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
  out=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
  trap 'scripts/fork-run.sh "$name" down >/dev/null' EXIT
  scripts/fork-run.sh "$name" up
  scripts/fork-run.sh "$name" render --layout "$layout" --out "$out"
  exit
fi
# Unset so the test takes the headless path, never the native window.
unset HERDR_SHOT_NATIVE_TEST
cargo test --locked --test shot_pixels -- "$@"
