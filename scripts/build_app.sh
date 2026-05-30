#!/bin/bash
# Build the REAL native macOS app (egui) and install it to /Applications.
# Double-clicking RatioUp.app opens a native window (auto-detected bundle → GUI).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Generate the .icns from the PNG if missing.
if [[ ! -f assets/RatioUp.icns ]] && command -v sips >/dev/null && command -v iconutil >/dev/null; then
  TMP="$(mktemp -d)/RatioUp.iconset"; mkdir -p "$TMP"
  for sz in 16 32 64 128 256 512; do
    sips -z $sz $sz assets/RatioUp.png --out "$TMP/icon_${sz}x${sz}.png" >/dev/null
    sips -z $((sz*2)) $((sz*2)) assets/RatioUp.png --out "$TMP/icon_${sz}x${sz}@2x.png" >/dev/null
  done
  iconutil -c icns "$TMP" -o assets/RatioUp.icns
fi

command -v cargo-bundle >/dev/null || cargo install cargo-bundle
cargo bundle --release
APP="$ROOT/target/release/bundle/osx/RatioUp.app"
echo "Built: $APP"
if [[ "${1:-}" == "--install" ]]; then
  rm -rf /Applications/RatioUp.app
  cp -R "$APP" /Applications/RatioUp.app
  echo "Installed to /Applications — open it from Launchpad/Spotlight as 'RatioUp'."
fi
