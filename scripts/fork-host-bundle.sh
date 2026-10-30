#!/bin/sh
# Build herdr-browser-host with CEF and lay it out as a macOS .app (framework + 5 helper apps).
# Usage: scripts/fork-host-bundle.sh [release]   -> prints the path of the bundle's executable.
# Env: CEF_PATH (CEF cache, default $HERDR_FORK_DIR/cef), CARGO_TARGET_DIR (default
# $HERDR_FORK_DIR/target); HERDR_FORK_DIR defaults to ~/work.noindex/herdr-fork (Spotlight skips it).
# Never installs anything and never touches a running Herdr. See docs/fork-host.md.
set -eu
profile=debug
flag=
if [ "${1:-}" = release ]; then profile=release; flag=--release; fi
: "${HERDR_FORK_DIR:=$HOME/work.noindex/herdr-fork}"
: "${CEF_PATH:=$HERDR_FORK_DIR/cef}"
export CEF_PATH
root=$(cd "$(dirname "$0")/.." && pwd)
target=${CARGO_TARGET_DIR:-$HERDR_FORK_DIR/target}
export CARGO_TARGET_DIR=$target
cargo build -q -p herdr-browser-host --features cef $flag --manifest-path "$root/Cargo.toml" >&2
framework=$(find "$CEF_PATH" -maxdepth 3 -type d -name 'Chromium Embedded Framework.framework' | head -n 1)
[ -n "$framework" ] || { echo "no CEF framework under $CEF_PATH" >&2; exit 1; }

name=herdr-browser-host
app="$target/$profile/$name.app"
plist() { # path, executable, identifier
  cat > "$1" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$2</string>
  <key>CFBundleIdentifier</key><string>$3</string>
  <key>CFBundleName</key><string>$2</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict></plist>
PLIST
}

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"
cp "$target/$profile/$name" "$app/Contents/MacOS/$name"
plist "$app/Contents/Info.plist" "$name" com.herdr.browser-host
ditto "$framework" "$app/Contents/Frameworks/Chromium Embedded Framework.framework"
for kind in "" " (GPU)" " (Renderer)" " (Plugin)" " (Alerts)"; do
  helper="$name Helper$kind"
  h="$app/Contents/Frameworks/$helper.app"
  mkdir -p "$h/Contents/MacOS"
  cp "$target/$profile/$name" "$h/Contents/MacOS/$helper"
  suffix=$(printf '%s' "$kind" | tr -d ' ()' | tr 'A-Z' 'a-z')
  plist "$h/Contents/Info.plist" "$helper" "com.herdr.browser-host.helper${suffix:+.$suffix}"
done
echo "$app/Contents/MacOS/$name"
