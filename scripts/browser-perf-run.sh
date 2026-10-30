#!/bin/sh
# Section R rerun: release build, 5 runs per number, through the Chromium slot; logs load before
# and after (a quiet machine is part of the result). Usage: scripts/browser-perf-run.sh [R1,R4,...]
set -eu
: "${HERDR_FORK_DIR:=$HOME/work.noindex/herdr-fork}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HERDR_FORK_DIR/target-frames}"
export PATH="$HOME/.cargo/bin:$PATH" HERDR_PERF_RUNS="${HERDR_PERF_RUNS:-5}"
[ -n "${1:-}" ] && export HERDR_PERF_ONLY="$1"
cd "$(dirname "$0")/.."
export HERDR_BROWSER_CEF_HOST="$(scripts/fork-host-bundle.sh release)"
cargo test --release --locked --test browser_perf --no-run >&2
FORK_WORKER=${FORK_WORKER:-frames} "$HERDR_FORK_DIR/bin/chromium-slot" sh -c 'uptime; cargo test --release --locked --test browser_perf; uptime'
