#!/usr/bin/env bash
# Build target/release/Rega.app (proper app name in the menu bar, a stable
# identity for macOS Screen Recording / Accessibility permissions, Dock icon).
set -euo pipefail
cd "$(dirname "$0")/.."

# Universal binary: Apple Silicon + Intel, both for the minimum macOS in Info.plist.
export MACOSX_DEPLOYMENT_TARGET=12.3
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
APP=target/release/Rega.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/Rega" \
  target/aarch64-apple-darwin/release/rega target/x86_64-apple-darwin/release/rega

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

# Sign. With a "Developer ID Application" certificate in the keychain (or
# REGA_SIGN_IDENTITY set), sign for distribution: the hardened runtime and a
# secure timestamp, which notarization requires (see package-macos.sh). Its
# designated requirement names the team, so permissions survive updates.
IDENTITY="${REGA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"')}"
if [[ -n "$IDENTITY" ]]; then
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
  codesign --verify --strict "$APP"
  echo "Signed with $IDENTITY"
else
  # No certificate (CI, other machines): ad hoc. Ad-hoc signatures change with
  # every build and macOS ties the Screen Recording permission to the
  # signature; a requirement on the bundle identifier keeps it across rebuilds.
  codesign --force --sign - --identifier app.rega.Rega \
    --requirements '=designated => identifier "app.rega.Rega"' "$APP" >/dev/null
  echo "Signed ad hoc (no Developer ID Application certificate found)"
fi

# The bundle is recreated from scratch each time; tell Launch Services and
# Finder about it so they show the new icon instead of a cached blank one.
touch "$APP"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" || true
echo "Built $APP"
