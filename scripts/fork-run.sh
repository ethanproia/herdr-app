#!/usr/bin/env bash
# Runs this checkout's herdr-app on its own home, core and state, so it can
# never attach to or disturb the Herdr you are using. See docs/fork-dev.md.
#
#   scripts/fork-run.sh <name> status|do|shot|ls ...   talk to the fork's window
#   scripts/fork-run.sh <name> hc <args>               hc against the fork's core
#   scripts/fork-run.sh <name> up                      start the window hidden (HERDR_APP_HEADLESS=1)
#   scripts/fork-run.sh <name> render --layout f.json --out f.png   render a layout offscreen
#   FORK_RUN_WINDOW=1 scripts/fork-run.sh <name> window   open the window (visible, see below)
#   scripts/fork-run.sh <name> down                    stop the fork's window and core
#
# Home: ~/.herdr-fork-home/<name>/home (FORK_RUN_HOME picks another; the real
# ~/.config/herdr-app is refused). `up` never shows a window or takes
# focus; `window` shows and activates it, so it is opt-in.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
name=${1:?usage: fork-run.sh <name> <command> ...}
shift
case $name in */*|.*|"") echo "fork-run: bad name $name" >&2; exit 2 ;; esac
root=$HOME/.herdr-fork-home/$name
home=${FORK_RUN_HOME:-$root/home}
real=${XDG_CONFIG_HOME:-$HOME/.config}/herdr-app
canon() { python3 -c 'import os,sys; print(os.path.realpath(os.path.expanduser(sys.argv[1])))' "$1"; }
if [[ $(canon "$home") == "$(canon "$real")" || $(canon "$home") == "$(canon "$HOME/.config/herdr-app")" ]]; then
  echo "fork-run: refusing to use the real home $real" >&2
  exit 1
fi
bin=${HERDR_APP_BIN:-$here/target/debug/herdr-app}

export HERDR_APP_HOME=$home
export HC_SESSION=herdr-fork-$name
# Short: the core socket lives under it and macOS caps socket paths at 103 bytes.
export XDG_STATE_HOME=/tmp/hf-$name
export HERDR_APP_ID=herdr-app-fork-$name
export HERDR_APP_NO_SOUND=1
mkdir -p "$home" "$XDG_STATE_HOME" "$root/bin"
# Opening a link would reach the real browser: log it instead.
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >>%q\n' "$root/opened" >"$root/bin/open"
chmod +x "$root/bin/open"
export PATH=$root/bin:$PATH

cmd=${1:-status}
case $cmd in
  up)
    [[ -x $bin ]] || (cd "$here" && cargo build --locked)
    HERDR_APP_HEADLESS=1 nohup "$bin" >"$root/app.log" 2>&1 </dev/null &
    echo $! >"$root/pid"
    for _ in $(seq 100); do "$bin" status >/dev/null 2>&1 && break; sleep 0.2; done
    "$bin" status >/dev/null 2>&1 || { echo "fork-run: window did not answer; see $root/app.log" >&2; exit 1; }
    echo "fork $name up headless, pid $(cat "$root/pid")"
    ;;
  window)
    if [[ ${FORK_RUN_WINDOW:-} != 1 ]]; then
      echo "fork-run: the window appears on screen and takes focus (no headless mode on macOS)." >&2
      echo "fork-run: set FORK_RUN_WINDOW=1 to open it anyway." >&2
      exit 1
    fi
    [[ -x $bin ]] || (cd "$here" && cargo build --locked)
    nohup "$bin" >"$root/app.log" 2>&1 </dev/null &
    echo $! >"$root/pid"
    echo "fork window pid $(cat "$root/pid"), log $root/app.log"
    ;;
  down)
    if [[ -f $root/pid ]]; then
      pid=$(cat "$root/pid")
      # Only our own process: its command line is this build.
      if ps -p "$pid" -o command= 2>/dev/null | grep -qF "$bin"; then kill "$pid"; fi
      rm -f "$root/pid"
    fi
    hc server stop >/dev/null 2>&1 || true
    echo "fork $name stopped"
    ;;
  hc) shift; exec hc "$@" ;;
  status|do|shot|ls|commands|render) exec "$bin" "$@" ;;
  *) echo "fork-run: unknown command $cmd" >&2; exit 2 ;;
esac
