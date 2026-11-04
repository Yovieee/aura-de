#!/usr/bin/env bash
# Download signed Debian packages and extract a user-local development sysroot.
# No dpkg install, maintainer scripts, host package changes, or host apt locks.
set -euo pipefail

prefix=/workspace/.cloud-setup/sysroot
native=false
testing=false
dry_run=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 && -n "$2" ]] || { echo "--prefix needs an absolute path" >&2; exit 2; }
            prefix="$2"; shift 2 ;;
        --native) native=true; shift ;;
        --testing) testing=true; shift ;;
        --dry-run) dry_run=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--prefix /workspace/.cloud-setup/sysroot] [--native] [--testing] [--dry-run]"
            echo "Debian 13 only. Downloads verified packages and extracts them without host installation."
            exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ "$prefix" == /* ]] || { echo "--prefix must be absolute" >&2; exit 2; }
case "$prefix" in
    *'"'*|*'\'*|*$'\n'*|*$'\r'*) echo "Unsupported characters in --prefix" >&2; exit 2 ;;
esac
for tool in apt-get apt-cache dpkg-deb dpkg python3 realpath; do
    command -v "$tool" >/dev/null || { echo "Required host tool missing: $tool" >&2; exit 1; }
done
# os-release is host configuration, not an application-provided environment file.
source /etc/os-release
if [[ "${ID:-}" != debian || "${VERSION_ID%%.*}" != 13 || "${VERSION_CODENAME:-}" != trixie ]]; then
    echo "Supported host: Debian 13 (trixie); use install-deps.sh for other supported hosts." >&2
    exit 1
fi
[[ -r /usr/share/keyrings/debian-archive-keyring.gpg ]] || {
    echo "Debian archive signing keyring is required; cannot verify repositories." >&2; exit 1;
}
prefix="$(realpath -m -- "$prefix")"
repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
state_root="$(dirname -- "$prefix")/apt"
for directory in "$prefix" "$state_root"; do
    case "$directory/" in
        "$repository_root/"*) echo "Sysroot and APT state must remain outside the repository." >&2; exit 2 ;;
    esac
done

# Keep the dynamically loaded X11 keyboard runtime explicit, including for Xvfb tests.
packages=(build-essential pkg-config python3 curl ca-certificates
          libwayland-dev libegl1-mesa-dev libgles2-mesa-dev
          libxkbcommon-dev libxkbcommon-x11-0)
if "$native"; then
    packages+=(libdrm-dev libgbm-dev libinput-dev libseat-dev libudev-dev libpipewire-0.3-dev)
fi
if "$testing"; then
    packages+=(xvfb xauth mesa-vulkan-drivers libgl1-mesa-dri dbus-x11 wayland-utils libxtst6)
fi
if "$dry_run"; then
    printf 'Prefix: %s\nAPT state: %s\n' "$prefix" "$state_root"
    printf '%s\n' "${packages[@]}"
    exit 0
fi

mkdir -p "$prefix" "$state_root/lists/partial" "$state_root/archives/partial" "$state_root/aura-user-empty"
sources="$state_root/aura-user.sources.list"
configuration="$state_root/aura-user.conf"
cat > "$sources" <<EOF
# Managed by scripts/install-deps-user.sh; official signed Debian HTTPS sources.
deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] https://deb.debian.org/debian $VERSION_CODENAME main
deb [signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] https://security.debian.org/debian-security $VERSION_CODENAME-security main
EOF
cat > "$configuration" <<EOF
Dir::Etc::main "/dev/null";
Dir::Etc::parts "$state_root/aura-user-empty";
Dir::Etc::sourcelist "$sources";
Dir::Etc::sourceparts "$state_root/aura-user-empty";
Dir::State::lists "$state_root/lists";
Dir::State::status "/var/lib/dpkg/status";
Dir::Cache::archives "$state_root/archives";
APT::Get::List-Cleanup "false";
APT::Get::AllowUnauthenticated "false";
Acquire::AllowInsecureRepositories "false";
Acquire::AllowDowngradeToInsecureRepositories "false";
Acquire::Check-Valid-Until "true";
Acquire::https::Verify-Peer "true";
Acquire::https::Verify-Host "true";
Acquire::Retries "2";
EOF

# Preserve the inherited cloud proxy route without persisting proxy credentials.
export http_proxy="${http_proxy:-${HTTP_PROXY:-}}"
export https_proxy="${https_proxy:-${HTTPS_PROXY:-}}"
export APT_CONFIG="$configuration"
export DEBIAN_FRONTEND=noninteractive
apt-get -o Debug::NoLocking=true update
# NoLocking is limited to this download-only operation: never invoke dpkg installation.
apt-get -o Debug::NoLocking=true --download-only -y --no-install-recommends install "${packages[@]}"

python3 - "$state_root/archives" "$prefix" <<'PY'
import hashlib
import os
from pathlib import Path
import subprocess
import sys

archives = Path(sys.argv[1])
prefix = Path(sys.argv[2])
architecture = subprocess.check_output(["dpkg", "--print-architecture"], text=True).strip()
selected = {}


def fields(text):
    return dict(line.split(": ", 1) for line in text.splitlines() if ": " in line and not line.startswith(" "))


for archive in sorted(archives.glob("*.deb")):
    package = fields(subprocess.check_output(
        ["dpkg-deb", "--field", str(archive), "Package", "Version", "Architecture"], text=True,
    ))
    name, version, arch = package["Package"], package["Version"], package["Architecture"]
    if arch not in {architecture, "all"}:
        continue
    # Cached archives may come from previous resolutions. Authenticate every one
    # against the newly signature-verified repository metadata before extracting.
    result = subprocess.run(
        ["apt-cache", "show", f"{name}={version}"], capture_output=True, text=True, check=False,
    )
    expected = {
        record.get("SHA256")
        for record in (fields(block) for block in result.stdout.split("\n\n"))
        if record.get("Package") == name and record.get("Version") == version
        and record.get("Architecture") == arch and record.get("SHA256")
    }
    with archive.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual not in expected:
        # APT validates required downloads itself. Old/unrelated cache entries
        # absent from the current signed indexes must not be used as artifacts.
        print(f"Ignoring unverified or stale cached archive: {archive.name}", file=sys.stderr)
        continue
    key = name, arch
    previous = selected.get(key)
    if previous is None or subprocess.run(
        ["dpkg", "--compare-versions", version, "gt", previous[0]], check=False,
    ).returncode == 0:
        selected[key] = version, archive

for _, archive in selected.values():
    subprocess.run(["dpkg-deb", "--extract", str(archive), str(prefix)], check=True)

# Development packages can point at an ABI runtime that APT correctly skipped
# because the host already provides it. Repair only broken development links,
# and only to an existing matching basename in the host library directory.
triplet = subprocess.check_output(["cc", "-dumpmachine"], text=True).strip()
library_dir = prefix / "usr/lib" / triplet
host_dir = Path("/usr/lib") / triplet
repaired = 0
if library_dir.is_dir():
    for link in library_dir.rglob("*.so"):
        if link.is_symlink() and not link.exists():
            basename = Path(os.readlink(link)).name
            host_runtime = host_dir / basename
            if host_runtime.is_file():
                link.unlink()
                link.symlink_to(host_runtime)
                repaired += 1
print(f"Extracted {len(selected)} SHA-256-verified Debian packages; repaired {repaired} host-runtime links.")
PY

printf '\nPrepared user-local dependencies. In each development shell, run:\n'
printf 'export AURA_SYSROOT=%q\n' "$prefix"
printf 'source %q\n' "$repository_root/scripts/dev-env.sh"
