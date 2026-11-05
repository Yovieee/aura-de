#!/usr/bin/env python3
"""Exercise a real nested compositor and SHM client, using Xvfb when needed."""

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import struct
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


def _read_xvfb_display(read_fd: int, process: subprocess.Popen, deadline: float) -> str:
    """Read the complete displayfd line, noticing process death even without EOF."""
    os.set_blocking(read_fd, False)
    response = bytearray()
    while True:
        code = process.poll()
        if code is not None:
            raise RuntimeError(f"Xvfb exited before reporting a display (exit {code})")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError("Xvfb did not report a display before the startup deadline")
        if not select.select([read_fd], [], [], min(remaining, 0.1))[0]:
            continue
        chunk = os.read(read_fd, 64)
        if not chunk:
            raise RuntimeError(f"Xvfb closed displayfd before readiness (exit {process.poll()})")
        response.extend(chunk)
        if len(response) > 32:
            raise RuntimeError("Xvfb reported an oversized displayfd response")
        if b"\n" in response:
            line = bytes(response).split(b"\n", 1)[0]
            if not line or any(byte < 48 or byte > 57 for byte in line) or int(line) > 65535:
                raise RuntimeError(f"Xvfb reported an invalid display number: {line!r}")
            return line.decode('ascii')


def _wait_x11(display: str, process: subprocess.Popen, deadline: float) -> None:
    """Require a successful X11 setup response, not just a PID or socket file."""
    last_error = 'no connection attempt'
    while time.monotonic() < deadline:
        code = process.poll()
        if code is not None:
            raise RuntimeError(f"Xvfb exited while waiting for X11 readiness (exit {code})")
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(min(0.2, max(0.001, deadline - time.monotonic())))
                connection.connect(f'/tmp/.X11-unix/X{display}')
                connection.sendall(struct.pack('<BBHHHHH', ord('l'), 0, 11, 0, 0, 0, 0))
                def receive(length):
                    result = bytearray()
                    while len(result) < length:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise TimeoutError('X11 setup deadline exceeded')
                        connection.settimeout(min(0.2, remaining))
                        chunk = connection.recv(length - len(result))
                        if not chunk:
                            raise OSError('X11 closed an incomplete setup response')
                        result.extend(chunk)
                    return result
                header = receive(8)
                status, reason_length, major, _, units = struct.unpack('<BBHHH', header)
                setup = receive(units * 4)
                if status != 1:
                    reason = bytes(setup[:reason_length]).decode('utf-8', errors='replace')
                    raise RuntimeError(f'Xvfb rejected X11 setup (status {status}): {reason}')
                if major != 11 or len(setup) < 32 or setup[20] == 0:
                    raise RuntimeError('Xvfb returned an invalid X11 setup response')
                return
        except OSError as error:
            last_error = str(error)
            time.sleep(min(0.02, max(0, deadline - time.monotonic())))
    raise RuntimeError(f'Xvfb did not accept an X11 setup before the startup deadline: {last_error}')


def _stop_failed_xvfb(process: subprocess.Popen | None) -> None:
    if process is None:
        return
    # This process starts in its own session. Also stop initialization helpers
    # that may retain displayfd after the server itself has already exited.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass
    # The parent may have exited while a helper still retains displayfd.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=1)


def start_xvfb(environment: dict[str, str], *, timeout: float = 30,
               log_dir: Path | None = None) -> subprocess.Popen:
    program = shutil.which('Xvfb', path=environment.get('PATH'))
    if program is None:
        raise RuntimeError('Xvfb is missing; install testing dependencies')
    if timeout <= 0 or not timeout < float('inf'):
        raise ValueError('Xvfb startup timeout must be positive and finite')
    log_dir = log_dir if log_dir is not None else ROOT / 'target' / 'xvfb-logs'
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f'xvfb-{os.getpid()}-{time.monotonic_ns()}.log'
    read_fd = write_fd = None
    process = None
    start = time.monotonic()
    try:
        read_fd, write_fd = os.pipe()
        with log_path.open('xb') as log:
            process = subprocess.Popen(
                [program, '-displayfd', str(write_fd), '-screen', '0', '1280x720x24',
                 '-nolisten', 'tcp', '-noreset'],
                pass_fds=(write_fd,), env=environment, start_new_session=True,
                stdout=subprocess.DEVNULL, stderr=log,
            )
        os.close(write_fd)
        write_fd = None
        deadline = start + timeout
        display = _read_xvfb_display(read_fd, process, deadline)
        _wait_x11(display, process, deadline)
        if process.poll() is not None:
            raise RuntimeError(f'Xvfb exited during readiness (exit {process.returncode})')
        environment['DISPLAY'] = f':{display}'
        environment.pop('WAYLAND_DISPLAY', None)
        return process
    except BaseException as error:
        _stop_failed_xvfb(process)
        if not isinstance(error, Exception):
            raise
        code = None if process is None else process.returncode
        diagnostic = tail(log_path)
        raise RuntimeError(
            f'{error}; Xvfb exit={code}, elapsed={time.monotonic() - start:.2f}s; '
            f'log: {log_path}\n{diagnostic or "(no Xvfb stderr)"}'
        ) from error
    finally:
        if write_fd is not None:
            os.close(write_fd)
        if read_fd is not None:
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
