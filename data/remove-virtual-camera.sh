#!/bin/sh
# Removes the "Link Studio Camera" setup made by setup-virtual-camera.sh.
# The v4l2loopback package itself is left installed, since other programs
# (such as OBS) may use it. Must run as root.
set -eu

rm -f /etc/modprobe.d/linkstudio-v4l2loopback.conf /etc/modules-load.d/linkstudio-v4l2loopback.conf
modprobe -r v4l2loopback 2>/dev/null || true
echo "The Link Studio virtual camera setup has been removed."
