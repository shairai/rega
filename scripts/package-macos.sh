#!/usr/bin/env bash
# Build dist/Rega-<version>-macos-<arch>.dmg: a drag-to-Applications disk image
# with the Rega volume icon, a branded background and a laid-out window.
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/bundle-macos.sh
APP=target/release/Rega.app
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
ARCH=$(uname -m)
OUT="dist/Rega-${VERSION}-macos-${ARCH}.dmg"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Background artwork is drawn by the app itself (src/branding.rs); 1x + 2x in one TIFF.
"$APP/Contents/MacOS/Rega" --write-branding "$WORK/art"
tiffutil -cathidpicheck "$WORK/art/dmg-background.png" "$WORK/art/dmg-background@2x.png" \
  -out "$WORK/background.tiff" >/dev/null

STAGE="$WORK/stage"
mkdir -p "$STAGE/.background"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
cp "$WORK/background.tiff" "$STAGE/.background/background.tiff"

# Lay the window out on a writable image, then compress it.
RW="$WORK/rw.dmg"
hdiutil create -volname "Rega" -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW" >/dev/null
ATTACH=$(hdiutil attach -readwrite -noverify -noautoopen "$RW")
DEV=$(echo "$ATTACH" | grep -E '^/dev/' | head -1 | awk '{print $1}')
MOUNT=$(echo "$ATTACH" | grep -o '/Volumes/.*' | head -1)
VOL=$(basename "$MOUNT")

# Window: 660x400 content (+ title bar). Icon positions match src/branding.rs.
if ! osascript <<APPLESCRIPT
tell application "Finder"
  tell disk "$VOL"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 860, 548}
    set opts to the icon view options of container window
    set arrangement of opts to not arranged
    set icon size of opts to 128
    set text size of opts to 13
    set background picture of opts to file ".background:background.tiff"
    set position of item "Rega.app" of container window to {180, 205}
    set position of item "Applications" of container window to {480, 205}
    update without registering applications
    delay 1
    close
  end tell
end tell
APPLESCRIPT
then
  echo "warning: Finder layout failed (allow Automation for Finder); DMG keeps default layout" >&2
fi
# Volume icon last: hdiutil's -srcfolder skips .VolumeIcon.icns, and the Finder
# layout above removes it again.
cp "$APP/Contents/Resources/Rega.icns" "$MOUNT/.VolumeIcon.icns"
SetFile -a C "$MOUNT"
rm -rf "$MOUNT/.fseventsd"
chmod -Rf go-w "$MOUNT" || true
sync
hdiutil detach "$DEV" -quiet || hdiutil detach "$DEV" -force -quiet

mkdir -p dist
rm -f "$OUT"
hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -o "$OUT" >/dev/null
echo "Built $OUT"
