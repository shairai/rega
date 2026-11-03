#!/usr/bin/env bash
# Build target/release/Rega.app (proper app name in the menu bar, a stable
# identity for macOS Screen Recording / Accessibility permissions, Dock icon).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
APP=target/release/Rega.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/rega "$APP/Contents/MacOS/Rega"

# Icon: render the in-app icon to PNG sizes, then pack an .icns.
ICONSET=$(mktemp -d)/Rega.iconset
mkdir -p "$ICONSET"
"$APP/Contents/MacOS/Rega" --write-icon "$ICONSET/icon_512x512@2x.png" 1024
for s in 16 32 128 256 512; do
  sips -z $s $s "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Rega.icns"

VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Rega</string>
  <key>CFBundleDisplayName</key><string>Rega</string>
  <key>CFBundleIdentifier</key><string>app.rega.Rega</string>
  <key>CFBundleExecutable</key><string>Rega</string>
  <key>CFBundleIconFile</key><string>Rega</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.3</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSUIElement</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Rega captures your screen to create screenshots and recordings.</string>
</dict>
</plist>
PLIST

# Ad-hoc signatures change with every build, and macOS ties the Screen
# Recording permission to the signature. A designated requirement based on
# the bundle identifier keeps the permission valid across rebuilds.
codesign --force --sign - --identifier app.rega.Rega \
  --requirements '=designated => identifier "app.rega.Rega"' "$APP" >/dev/null
echo "Built $APP"
