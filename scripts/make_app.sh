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

# Working dir = where the torrents live, so a relative torrent_dir (default ".")
# resolves correctly. Order: $RATIOUP_TORRENT_DIR, else torrent_dir from the XDG
# config, else $HOME.
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/RatioUp/config.toml"
WORKDIR="${RATIOUP_TORRENT_DIR:-}"
if [[ -z "$WORKDIR" && -f "$CFG" ]]; then
  WORKDIR="$(sed -n 's/^[[:space:]]*torrent_dir[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$CFG" | head -1)"
fi
[[ -z "$WORKDIR" || ! -d "$WORKDIR" ]] && WORKDIR="$HOME"

# Open a Terminal window running RatioUp in that dir. `exec` makes RatioUp the
# controlling process, so closing the window delivers SIGHUP and RatioUp shuts
# down cleanly (announce stopped + state saved + terminal restored).
#
# If Terminal is launched fresh, it auto-opens one empty window — running
# `do script` then would leave TWO windows (the empty one + ours). So we detect
# whether Terminal was already running: if not, reuse the window the activation
# just created (`do script ... in window 1`); if yes, open a new window.
#
# The shell command just runs `clear; cd; exec`. We do NOT inline escape
# sequences here: backslashes in the string break AppleScript's parser (the
# whole `do script` then fails to compile and nothing opens). The dashboard
# itself purges the scrollback on startup (draw::enter_screen emits ESC[3J via
# Clear(Purge)), so a plain `clear` here is enough.
CMD="clear; cd '$WORKDIR'; exec '$BIN'"
osascript <<APPLESCRIPT
tell application "Terminal"
    set wasRunning to running
    activate
    if wasRunning then
        do script "$CMD"
    else
        -- reuse the empty window Terminal just opened on launch
        do script "$CMD" in window 1
    end if
    set custom title of front window to "RatioUp"
    set number of columns of front window to 110
    set number of rows of front window to 34
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
