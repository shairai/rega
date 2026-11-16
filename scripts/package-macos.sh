#!/usr/bin/env bash
# Build dist/Rega-<version>-macos-<arch>.dmg: a drag-to-Applications disk image
# with the Rega volume icon, a branded background and a laid-out window.
#
# With a Developer ID certificate (see bundle-macos.sh) and notarization
# credentials stored once as a keychain profile, the app and the disk image
# are also notarized by Apple and stapled, so they open without warnings:
#   xcrun notarytool store-credentials rega-notary \
#     --apple-id <Apple ID> --team-id <Team ID> --password <app-specific password>
# (REGA_NOTARY_PROFILE picks another profile name.)
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/bundle-macos.sh
APP=target/release/Rega.app
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
OUT="dist/Rega-${VERSION}-macos-universal.dmg"
IDENTITY="${REGA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"')}"
PROFILE="${REGA_NOTARY_PROFILE:-rega-notary}"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

NOTARIZE=0
if [[ -n "$IDENTITY" ]] && xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
  NOTARIZE=1
elif [[ -n "$IDENTITY" ]]; then
  echo "warning: no notarization profile '$PROFILE'; the DMG will be signed but not notarized" >&2
fi

# Submit $1 to Apple's notary service and wait; on rejection print Apple's log.
notarize() {
  local result id status
  echo "Notarizing $(basename "$1") (usually a few minutes)…"
  result=$(xcrun notarytool submit "$1" --keychain-profile "$PROFILE" --wait --output-format json)
  id=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' <<<"$result")
  status=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' <<<"$result")
  if [[ "$status" != "Accepted" ]]; then
    echo "Notarization $status ($id):" >&2
    xcrun notarytool log "$id" --keychain-profile "$PROFILE" >&2 || true
    exit 1
  fi
}

# The app first, so the copy in the disk image carries its own ticket and
# opens without a network check once dragged to Applications.
if [[ $NOTARIZE == 1 ]]; then
  ditto -c -k --keepParent "$APP" "$WORK/Rega.zip"
  notarize "$WORK/Rega.zip"
  xcrun stapler staple -q "$APP"
fi

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

# Window: 660x400 content (+ title bar). Icon positions match src/branding.rs.
if ! osascript <<APPLESCRIPT
tell application "Finder"
  -- By mount path: with another "Rega" volume mounted, this one is "Rega 1" on disk
  -- but still named "Rega" in Finder.
  tell item (POSIX file "$MOUNT" as alias)
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

if [[ -n "$IDENTITY" ]]; then
  codesign --force --timestamp --sign "$IDENTITY" "$OUT"
fi
if [[ $NOTARIZE == 1 ]]; then
  notarize "$OUT"
  xcrun stapler staple -q "$OUT"
  # What a downloaded copy will get from Gatekeeper.
  spctl --assess --type open --context context:primary-signature -v "$OUT"
  echo "Built $OUT (signed and notarized)"
elif [[ -n "$IDENTITY" ]]; then
  echo "Built $OUT (signed, not notarized)"
else
  echo "Built $OUT (unsigned: no Developer ID certificate)"
fi
