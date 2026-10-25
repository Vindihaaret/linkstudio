#!/bin/sh
# Sets up the "Link Studio Camera" virtual camera with v4l2loopback.
# Must run as root; Link Studio runs it through pkexec.
set -eu

LABEL="Link Studio Camera"
MODPROBE_CONF=/etc/modprobe.d/linkstudio-v4l2loopback.conf
MODULES_CONF=/etc/modules-load.d/linkstudio-v4l2loopback.conf

if ! modinfo v4l2loopback >/dev/null 2>&1; then
    echo "Installing v4l2loopback…"
    if command -v zypper >/dev/null; then
        # openSUSE ships one kernel module package per kernel flavor.
        flavor="$(uname -r | sed 's/.*-//')"
        zypper --non-interactive install "v4l2loopback-kmp-$flavor"
    elif command -v apt-get >/dev/null; then
        apt-get install -y v4l2loopback-dkms
    elif command -v pacman >/dev/null; then
        pacman -S --needed --noconfirm v4l2loopback-dkms
    elif command -v dnf >/dev/null; then
        # Fedora packages it in RPM Fusion as akmod-v4l2loopback.
        dnf install -y akmod-v4l2loopback || {
            echo "Enable RPM Fusion (https://rpmfusion.org) and run this again." >&2
            exit 1
        }
        akmods --force >/dev/null 2>&1 || true
    else
        echo "Install the v4l2loopback kernel module with your package manager and run this again." >&2
        exit 1
    fi
fi

# exclusive_caps=1 makes browsers and video-call apps recognize the device.
cat > "$MODPROBE_CONF" <<EOF
options v4l2loopback devices=1 video_nr=42 card_label="$LABEL" exclusive_caps=1
EOF
echo v4l2loopback > "$MODULES_CONF"

if lsmod | grep -q '^v4l2loopback'; then
    if ! modprobe -r v4l2loopback 2>/dev/null; then
        echo "v4l2loopback is in use by another program (OBS?). Restart the computer to finish." >&2
        exit 2
    fi
fi
modprobe v4l2loopback
echo "The virtual camera “$LABEL” is ready."
