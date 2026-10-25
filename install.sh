#!/bin/sh
# Builds Link Studio and installs it for the current user under ~/.local.
set -eu

cd "$(dirname "$0")"
cargo build --release

PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.linkstudio.LinkStudio

install -Dm755 target/release/linkstudio "$PREFIX/bin/linkstudio"
install -Dm644 "data/$APP_ID.desktop" "$PREFIX/share/applications/$APP_ID.desktop"
install -Dm644 "data/$APP_ID.svg" "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 "data/$APP_ID-symbolic.svg" "$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"

gtk4-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true

echo "Link Studio is installed. Start it from the app menu or run: linkstudio"
