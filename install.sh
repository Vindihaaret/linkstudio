#!/bin/sh
# Builds Link Studio and installs it for the current user under ~/.local.
# Missing build tools (Rust, a C compiler, GTK 4 and libadwaita development
# files) are installed first with sudo; Link Studio itself needs no root.
set -eu

cd "$(dirname "$0")"

RUST_MIN=1.88

rust_ok() {
    command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1 || return 1
    version="$(rustc --version | cut -d' ' -f2)"
    [ "$(printf '%s\n%s\n' "$RUST_MIN" "$version" | sort -V | head -n1)" = "$RUST_MIN" ]
}

deps_ok() {
    rust_ok && command -v cc >/dev/null 2>&1 &&
        pkg-config --atleast-version=4.14 gtk4 2>/dev/null &&
        pkg-config --atleast-version=1.5 libadwaita-1 2>/dev/null
}

if ! deps_ok; then
    echo "==> Installing build dependencies (asks for your sudo password)"
    if command -v zypper >/dev/null; then
        sudo zypper --non-interactive install rust cargo gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
    elif command -v dnf >/dev/null; then
        sudo dnf install -y rust cargo gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
    elif command -v apt-get >/dev/null; then
        sudo apt-get install -y cargo rustc gcc pkg-config libgtk-4-dev libadwaita-1-dev
    elif command -v pacman >/dev/null; then
        sudo pacman -S --needed --noconfirm rust gcc pkgconf gtk4 libadwaita
    fi
fi

if ! rust_ok; then
    echo "ERROR: Link Studio needs Rust $RUST_MIN or newer (found: $(rustc --version 2>/dev/null || echo none))."
    echo "       Install a current Rust with https://rustup.rs and run this script again."
    exit 1
fi
if ! deps_ok; then
    echo "ERROR: Link Studio needs a C compiler, GTK >= 4.14 and libadwaita >= 1.5 with development files."
    echo "       Install them with your package manager and run this script again."
    exit 1
fi

echo "==> Building"
cargo build --release

echo "==> Installing"
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.linkstudio.LinkStudio

install -Dm755 "${CARGO_TARGET_DIR:-target}/release/linkstudio" "$PREFIX/bin/linkstudio"
install -Dm644 "data/$APP_ID.desktop" "$PREFIX/share/applications/$APP_ID.desktop"
install -Dm644 "data/$APP_ID.svg" "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 "data/$APP_ID-symbolic.svg" "$PREFIX/share/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"

gtk4-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true

echo "Link Studio is installed. Start it from the app menu or run: linkstudio"

# The build files are only needed to rebuild faster. Ask when run by hand;
# remove them when the in-app updater built into its cache (Link Studio
# 0.2.0's updater does not clean up after itself).
UPDATE_BUILD="${XDG_CACHE_HOME:-$HOME/.cache}/linkstudio/build"
if [ "${CARGO_TARGET_DIR:-}" = "$UPDATE_BUILD" ]; then
    rm -rf "$UPDATE_BUILD"
elif [ -z "${CARGO_TARGET_DIR:-}" ] && [ -d target ] && [ -t 0 ]; then
    size="$(du -sh target 2>/dev/null | cut -f1)"
    printf 'Remove the build files in %s/target (%s)? They are only needed to rebuild faster. [Y/n] ' "$(pwd)" "$size"
    read -r answer || answer=n
    case "$answer" in
        [nN]*) ;;
        *) rm -rf target && echo "Build files removed." ;;
    esac
fi
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Note: $PREFIX/bin is not on your PATH; the app menu entry works regardless." ;;
esac
