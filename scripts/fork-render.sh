#!/usr/bin/env bash
# Offscreen render check: gpui's headless renderer draws a scene and reads
# the pixels back, with no window and no focus change. See docs/fork-dev.md.
#
#   scripts/fork-render.sh            the GPU readback check (tests/shot_pixels.rs)
#
# Rendering a whole layout JSON to a PNG offscreen needs an app entry point
# that builds AppWindow in a headless context; that is not in src/ yet.
set -euo pipefail
cd "$(dirname "$0")/.."
# Unset so the test takes the headless path, never the native window.
unset HERDR_SHOT_NATIVE_TEST
cargo test --locked --test shot_pixels -- "$@"
