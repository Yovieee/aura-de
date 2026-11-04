#!/usr/bin/env bash
# Stage or install development binaries. Does not start/restart desktop services.
set -euo pipefail
repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
prefix=/usr/local
staging_root="${DESTDIR:-}"
profile=release
build=true
with_session=false
with_portal=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix|--destdir|--profile)
            [[ $# -ge 2 ]] || { echo "$1 needs a value" >&2; exit 2; }
            case "$1" in
                --prefix) prefix="$2" ;;
                --destdir) staging_root="$2" ;;
                --profile) profile="$2" ;;
            esac
            shift 2 ;;
        --no-build) build=false; shift ;;
        --with-session) with_session=true; shift ;;
        --with-portal) with_portal=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--prefix /usr/local] [--destdir PATH] [--profile release|dev]"
            echo "          [--no-build] [--with-session] [--with-portal]"
            echo "Session descriptor is a nested preview, not a bare-metal login session."
            echo "Portal descriptor reports capture unsupported; it does not enable screen sharing."
            exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done
[[ "$prefix" == /* ]] || { echo "--prefix must be absolute" >&2; exit 2; }
[[ -z "$staging_root" || "$staging_root" == /* ]] || { echo "--destdir must be absolute" >&2; exit 2; }
[[ "$profile" == release || "$profile" == dev ]] || { echo "--profile must be release or dev" >&2; exit 2; }
cd "$repository_root"
if "$build"; then
    cargo build --workspace --locked --profile "$profile"
fi
target_directory="${CARGO_TARGET_DIR:-$repository_root/target}"
binary_directory="$target_directory/$profile"
[[ "$profile" != dev ]] || binary_directory="$target_directory/debug"
destination="${staging_root}${prefix%/}"
for binary in aura-compositor aura-shell aura-launcher aura-session; do
    [[ -x "$binary_directory/$binary" ]] || { echo "Missing built executable: $binary_directory/$binary" >&2; exit 1; }
done
install -d "$destination/bin" "$destination/share/licenses/aura-de" "$destination/share/doc/aura-de"
for binary in aura-compositor aura-shell aura-launcher aura-session; do
    install -m 755 "$binary_directory/$binary" "$destination/bin/$binary"
done
install -m 644 LICENSE-MIT LICENSE-APACHE "$destination/share/licenses/aura-de/"
install -m 644 README.md docs/implementation-status.md "$destination/share/doc/aura-de/"
if "$with_session"; then
    install -d "$destination/share/wayland-sessions"
    install -m 644 data/wayland-sessions/aura.desktop "$destination/share/wayland-sessions/"
fi
if "$with_portal"; then
    install -d "$destination/share/dbus-1/services" "$destination/share/xdg-desktop-portal/portals"
    install -m 644 data/dbus-1/services/org.freedesktop.impl.portal.desktop.aura.service "$destination/share/dbus-1/services/"
    install -m 644 data/xdg-desktop-portal/portals/aura.portal "$destination/share/xdg-desktop-portal/portals/"
fi
echo "Installed development files under $destination"
