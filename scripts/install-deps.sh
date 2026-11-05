#!/usr/bin/env bash
# Install development packages on Debian/Ubuntu. Run with root privileges.
set -euo pipefail

native=false
testing=false
dry_run=false
for argument in "$@"; do
    case "$argument" in
        --native) native=true ;;
        --testing) testing=true ;;
        --dry-run) dry_run=true ;;
        -h|--help)
            echo "Usage: $0 [--native] [--testing] [--dry-run]"
            echo "Default: packages for nested Wayland/EGL development."
            echo "--native: also install DRM/input/session development packages."
            echo "--testing: also install Xvfb, Mesa software renderers and Wayland/DBus tools."
            exit 0 ;;
        *) echo "Unknown argument: $argument" >&2; exit 2 ;;
    esac
done

# winit loads the X11 keyboard library at runtime; libxkbcommon-dev does not provide it.
packages=(build-essential pkg-config python3 curl ca-certificates
          libwayland-dev libegl1-mesa-dev libgles2-mesa-dev
          libxkbcommon-dev libxkbcommon-x11-0 libpipewire-0.3-dev libclang-dev clang)
if "$native"; then
    packages+=(libdrm-dev libgbm-dev libinput-dev libseat-dev libudev-dev)
fi
if "$testing"; then
    packages+=(xvfb xauth mesa-vulkan-drivers libgl1-mesa-dri dbus-x11 libglib2.0-bin wayland-utils libxtst6 pipewire-bin libpipewire-0.3-modules libspa-0.2-modules)
fi
if "$dry_run"; then
    printf '%s\n' "${packages[@]}"
    exit 0
fi
if [[ "$(id -u)" != 0 ]]; then
    echo "Run with root privileges (for example: sudo bash $0 --testing)." >&2
    exit 1
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends "${packages[@]}"
