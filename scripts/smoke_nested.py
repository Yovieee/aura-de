#!/usr/bin/env python3
"""Exercise a real nested compositor and SHM client, using Xvfb when needed."""

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def stop(process: subprocess.Popen | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def start_xvfb(environment: dict[str, str]) -> subprocess.Popen:
    if shutil.which("Xvfb") is None:
        raise RuntimeError("DISPLAY is unset and Xvfb is missing; install testing dependencies")
    read_fd, write_fd = os.pipe()
    process = subprocess.Popen(
        ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1280x720x24", "-nolisten", "tcp"],
        pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    os.close(write_fd)
    try:
        if not select.select([read_fd], [], [], 10)[0]:
            raise RuntimeError("Xvfb did not report a display within 10 seconds")
        display = os.read(read_fd, 64).decode().strip()
        if not display.isdigit() or process.poll() is not None:
            raise RuntimeError("Xvfb failed to initialize")
        environment["DISPLAY"] = f":{display}"
        environment.pop("WAYLAND_DISPLAY", None)
        return process
    except Exception:
        stop(process)
        raise
    finally:
        os.close(read_fd)


def tail(path: Path) -> str:
    return "\n".join(path.read_text(errors="replace").splitlines()[-80:]) if path.exists() else ""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true", help="use previously built binaries")
    parser.add_argument("--software", action="store_true", help="force Mesa software rendering")
    parser.add_argument("--timeout", type=float, default=30, help="client/compositor timeout in seconds")
    parser.add_argument("--log-dir", type=Path, default=ROOT / "target" / "smoke-logs")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    if not args.no_build:
        subprocess.run(
            ["cargo", "build", "--locked", "-p", "aura-compositor", "--bin", "aura-compositor",
             "--example", "smoke_client"], cwd=ROOT, check=True,
        )
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT,
    ))
    target = Path(metadata["target_directory"]) / "debug"
    compositor = target / "aura-compositor"
    client = target / "examples" / "smoke_client"
    for binary in [compositor, client]:
        if not binary.is_file():
            raise RuntimeError(f"Missing binary: {binary}; rerun without --no-build")

    args.log_dir.mkdir(parents=True, exist_ok=True)
    compositor_log = args.log_dir / "compositor.log"
    client_log = args.log_dir / "client.log"
    server = None
    xvfb = None
    environment = os.environ.copy()
    environment.setdefault("RUST_LOG", "info")
    if args.software:
        environment["LIBGL_ALWAYS_SOFTWARE"] = "1"
    try:
        if not environment.get("DISPLAY") and not environment.get("WAYLAND_DISPLAY"):
            xvfb = start_xvfb(environment)
        with tempfile.TemporaryDirectory(prefix="aura-smoke-") as runtime:
            # Preserve access to a host Wayland socket when using an isolated server runtime.
            host_wayland = environment.get("WAYLAND_DISPLAY")
            host_runtime = environment.get("XDG_RUNTIME_DIR")
            if host_wayland and host_runtime and not os.path.isabs(host_wayland):
                environment["WAYLAND_DISPLAY"] = str(Path(host_runtime) / host_wayland)
            environment["XDG_RUNTIME_DIR"] = runtime
            socket_name = "wayland-aura-smoke"
            socket_path = Path(runtime) / socket_name
            with compositor_log.open("w") as output:
                server = subprocess.Popen(
                    [str(compositor), "--backend", "winit", "--socket", socket_name, "--smoke-test"],
                    cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT,
                )
                deadline = time.monotonic() + args.timeout
                while not socket_path.exists():
                    if server.poll() is not None:
                        raise RuntimeError(f"Compositor exited before publishing its socket (exit {server.returncode})")
                    if time.monotonic() >= deadline:
                        raise RuntimeError("Timed out waiting for Wayland socket")
                    time.sleep(0.02)
                if not stat.S_ISSOCK(socket_path.stat().st_mode):
                    raise RuntimeError("Compositor readiness path is not a Unix socket")
                client_environment = environment | {"WAYLAND_DISPLAY": socket_name}
                with client_log.open("w") as output:
                    result = subprocess.run(
                        [str(client), "--frames", "2"], cwd=ROOT, env=client_environment,
                        stdout=output, stderr=subprocess.STDOUT, timeout=args.timeout, check=False,
                    )
                if result.returncode != 0:
                    raise RuntimeError(f"Real SHM client failed (exit {result.returncode})")
                status = server.wait(timeout=args.timeout)
                if status != 0:
                    raise RuntimeError(f"Compositor smoke test failed (exit {status})")
        print("PASS: real Wayland client configured, committed a SHM buffer and received frame callbacks.")
        print(f"Logs: {args.log_dir}")
        return 0
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        print(f"Compositor log:\n{tail(compositor_log)}", file=sys.stderr)
        print(f"Client log:\n{tail(client_log)}", file=sys.stderr)
        return 1
    finally:
        stop(server)
        stop(xvfb)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
