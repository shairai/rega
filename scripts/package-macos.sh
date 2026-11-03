#!/usr/bin/env bash
# Build dist/Rega-<version>-macos-<arch>.dmg (drag-to-Applications disk image).
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/bundle-macos.sh
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
ARCH=$(uname -m)
OUT="dist/Rega-${VERSION}-macos-${ARCH}.dmg"

STAGE=$(mktemp -d)
cp -R target/release/Rega.app "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p dist
rm -f "$OUT"
hdiutil create -volname "Rega" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
rm -rf "$STAGE"
echo "Built $OUT"
