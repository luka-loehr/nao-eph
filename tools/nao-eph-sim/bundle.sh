#!/usr/bin/env bash
# Builds dist/nao-eph.app: the GPUI app with Webots, HULKs, the world and every asset inside, so it
# runs offline and never reads from your Documents folder.
#
# Usage: ./bundle.sh [path/to/Webots.app]
# Load the world once beforehand (run the dev app) so Webots' asset cache holds everything it needs.
set -euo pipefail

cd "$(dirname "$0")"
REPO="$(cd ../.. && pwd)"
WEBOTS_APP="${1:-/Applications/Webots.app}"
CACHE="$HOME/Library/Caches/Cyberbotics/Webots/assets"
APP="dist/nao-eph.app"
RES="$APP/Contents/Resources"

[ -x "$WEBOTS_APP/Contents/MacOS/webots" ] || { echo "Webots not found at $WEBOTS_APP" >&2; exit 1; }
[ -d "$CACHE" ] || { echo "No Webots asset cache; run the dev app once first." >&2; exit 1; }

echo "==> Building nao-eph-sim and hulk_webots"
cargo build --release
(cd "$REPO" && WEBOTS_HOME="$WEBOTS_APP" cargo build --release -p hulk_webots)

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$RES/hulk" "$RES/webots"
cp target/release/nao-eph-sim "$APP/Contents/MacOS/"
cp assets/AppIcon.icns "$RES/"
cp assets/icon-1024.png "$RES/icon.png"
cp "$REPO/LICENSE" "$REPO/NOTICE" "$RES/"
# Only the Apple Silicon half of Webots' universal binaries.
ditto --arch arm64 "$WEBOTS_APP" "$RES/Webots.app"
cp -R "$REPO/webots/worlds" "$REPO/webots/protos" "$RES/webots/"
rm -f "$RES/webots/worlds/"_* "$RES/webots/worlds/".*.wbproj
# HULKs finds libController through its rpath @executable_path/../Webots.app.
cp "$REPO/target/release/hulk_webots" "$RES/hulk/"
mkdir -p "$RES/hulk/etc"
cp -R "$REPO/etc/parameters" "$REPO/etc/motions" "$REPO/etc/sounds" "$REPO/etc/neural_networks" "$RES/hulk/etc/"
# Webots names cached downloads by the SHA-1 of their URL; the app seeds this into the user cache.
cp -R "$CACHE" "$RES/webots-cache"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>nao-eph</string>
  <key>CFBundleDisplayName</key><string>nao-eph</string>
  <key>CFBundleIdentifier</key><string>com.lukaloehr.nao-eph</string>
  <key>CFBundleExecutable</key><string>nao-eph-sim</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.education</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSQuitAlwaysKeepsWindows</key><false/>
</dict>
</plist>
PLIST

echo "==> Signing (ad-hoc, one identity for the whole bundle)"
xattr -cr "$APP"
codesign --force --deep --sign - "$APP"

echo "==> Zipping"
rm -f dist/nao-eph-macos-arm64.zip
ditto -c -k --keepParent "$APP" dist/nao-eph-macos-arm64.zip
du -sh "$APP" dist/nao-eph-macos-arm64.zip
