#!/usr/bin/env bash
# Cross-build the Windows installer from macOS/Linux.
# Needs: rustup target x86_64-pc-windows-msvc, cargo-xwin, llvm (clang + lld), makensis.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/llvm/bin:$PATH"
export XWIN_ACCEPT_LICENSE=1

# The icon and installer artwork are drawn by the app itself; use a native build
# to write them. The multi-size .ico goes into assets/ first so rega.exe embeds it.
mkdir -p dist
cargo run --release -q -- --write-icon assets/rega.ico 256
cp assets/rega.ico dist/rega.ico
cargo run --release -q -- --write-branding dist

cargo xwin build --release --target x86_64-pc-windows-msvc
EXE=target/x86_64-pc-windows-msvc/release/rega.exe
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

makensis -V2 -DVERSION="$VERSION" -DEXE="../$EXE" scripts/installer.nsi
echo "Built dist/Rega-${VERSION}-windows-x64-setup.exe"
