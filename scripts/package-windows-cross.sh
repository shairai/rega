#!/usr/bin/env bash
# Cross-build the Windows installer from macOS/Linux.
# Needs: rustup target x86_64-pc-windows-msvc, cargo-xwin, llvm (clang + lld), makensis.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/llvm/bin:$PATH"
export XWIN_ACCEPT_LICENSE=1

cargo xwin build --release --target x86_64-pc-windows-msvc
EXE=target/x86_64-pc-windows-msvc/release/snapquill.exe
VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

mkdir -p dist
# The icon is drawn by the app itself; use a native build to write the .ico.
cargo run --release -q -- --write-icon dist/snapquill.ico 256
makensis -V2 -DVERSION="$VERSION" -DEXE="../$EXE" scripts/installer.nsi
echo "Built dist/Snapquill-${VERSION}-windows-x64-setup.exe"
