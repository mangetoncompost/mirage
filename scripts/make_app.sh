#!/bin/bash
# Build a macOS RatioUp.app wrapper that launches the release binary in a
# Terminal window (showing the live super-shell dashboard). No GUI code: the
# .app is a thin launcher around the existing TUI.
#
# Usage:  cargo build --release && scripts/make_app.sh [--install]
#   --install : also copy the resulting RatioUp.app into /Applications
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/RatioUp"
APP="$ROOT/RatioUp.app"

if [[ ! -x "$BIN" ]]; then
  echo "error: release binary not found at $BIN" >&2
  echo "run: cargo build --release" >&2
  exit 1
fi

echo "Building $APP ..."
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# 1. Copy the real binary into the bundle so the .app is self-contained.
cp "$BIN" "$APP/Contents/MacOS/ratioup-bin"
chmod +x "$APP/Contents/MacOS/ratioup-bin"

# 2. The bundle's main executable: a launcher that opens Terminal on the binary.
#    Terminal runs the binary so the live TUI dashboard is visible and Ctrl+C /
#    q work. We pass the bundled binary path explicitly.
cat > "$APP/Contents/MacOS/RatioUp" <<'LAUNCHER'
#!/bin/bash
# Resolve the bundled binary next to this launcher.
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/ratioup-bin"
# Open a Terminal window running RatioUp. `exec` keeps the window tied to it;
# on exit (Ctrl+C / q) the dashboard restores the terminal cleanly.
osascript <<APPLESCRIPT
tell application "Terminal"
    activate
    do script "clear; exec '$BIN'"
end tell
APPLESCRIPT
LAUNCHER
chmod +x "$APP/Contents/MacOS/RatioUp"

# 3. Info.plist — makes it a real, double-clickable, Launchpad/Spotlight app.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>RatioUp</string>
    <key>CFBundleDisplayName</key><string>RatioUp</string>
    <key>CFBundleIdentifier</key><string>coop.assembleurs.ratioup</string>
    <key>CFBundleVersion</key><string>1.0.0</string>
    <key>CFBundleShortVersionString</key><string>1.0.0</string>
    <key>CFBundleExecutable</key><string>RatioUp</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>RatioUp.icns</string>
    <key>LSMinimumSystemVersion</key><string>10.13</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# 4. Icon: generate a simple .icns if none provided (a green up-arrow tile).
ICON_SRC="$ROOT/assets/RatioUp.png"
if [[ -f "$ICON_SRC" ]] && command -v sips >/dev/null && command -v iconutil >/dev/null; then
  TMP_ICONSET="$(mktemp -d)/RatioUp.iconset"
  mkdir -p "$TMP_ICONSET"
  for sz in 16 32 64 128 256 512; do
    sips -z $sz $sz "$ICON_SRC" --out "$TMP_ICONSET/icon_${sz}x${sz}.png" >/dev/null
    sips -z $((sz*2)) $((sz*2)) "$ICON_SRC" --out "$TMP_ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
  done
  iconutil -c icns "$TMP_ICONSET" -o "$APP/Contents/Resources/RatioUp.icns" 2>/dev/null || true
fi

echo "Done: $APP"
if [[ "${1:-}" == "--install" ]]; then
  echo "Installing to /Applications ..."
  rm -rf "/Applications/RatioUp.app"
  cp -R "$APP" "/Applications/RatioUp.app"
  echo "Installed. Find it in Launchpad / Spotlight as 'RatioUp'."
fi
