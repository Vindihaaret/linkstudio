#!/bin/sh
# Removes Link Studio from ~/.local. Pass --purge to also delete saved
# settings (positions, profiles). Photos in ~/Pictures are never touched.
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.linkstudio.LinkStudio
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/linkstudio"

rm -fv "$PREFIX/bin/linkstudio" \
       "$PREFIX/share/applications/$APP_ID.desktop" \
       "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
       "$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"

gtk4-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true

if [ "${1:-}" = "--purge" ]; then
    rm -rfv "$CONFIG_DIR"
    echo "Link Studio and all its settings have been removed."
else
    echo "Link Studio has been removed. Settings are kept in $CONFIG_DIR"
    echo "(run with --purge to delete them too)."
fi
