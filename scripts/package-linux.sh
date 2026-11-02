#!/usr/bin/env bash
# Build dist/snapquill_<version>_<arch>.deb and a .tar.gz. Run on Linux.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
PKG="dist/snapquill_${VERSION}_${ARCH}"

rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/applications" \
  "$PKG/usr/share/icons/hicolor/256x256/apps" "$PKG/usr/share/doc/snapquill"
install -m 755 target/release/snapquill "$PKG/usr/bin/snapquill"
target/release/snapquill --write-icon "$PKG/usr/share/icons/hicolor/256x256/apps/snapquill.png" 256
install -m 644 LICENSE "$PKG/usr/share/doc/snapquill/copyright"

cat > "$PKG/usr/share/applications/snapquill.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Snapquill
Comment=Screenshots, recordings and annotation
Exec=snapquill
Icon=snapquill
Categories=Graphics;Utility;
Terminal=false
DESKTOP

cat > "$PKG/DEBIAN/control" <<CONTROL
Package: snapquill
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Shai Raiten <3217647+shairai@users.noreply.github.com>
Section: graphics
Priority: optional
Depends: libc6, libxcb1, libxkbcommon0, libdbus-1-3, libgtk-3-0, libayatana-appindicator3-1, libxdo3, libpipewire-0.3-0, libgbm1, libegl1
Description: Screenshots, screen recording and annotation
 Capture, annotate and share screenshots and recordings.
CONTROL

dpkg-deb --build --root-owner-group "$PKG" "${PKG}.deb" >/dev/null
rm -rf "$PKG"

TAR="dist/snapquill-${VERSION}-linux-$(uname -m)"
mkdir -p "$TAR"
cp target/release/snapquill LICENSE README.md "$TAR/"
tar -C dist -czf "${TAR}.tar.gz" "$(basename "$TAR")"
rm -rf "$TAR"
echo "Built ${PKG}.deb and ${TAR}.tar.gz"
