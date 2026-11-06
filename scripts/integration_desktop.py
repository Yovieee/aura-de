#!/usr/bin/env python3
"""Exercise real nested clients, window control, shell/launcher frames and teardown."""

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from smoke_nested import ROOT, start_xvfb, stop, tail


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def wait_for(predicate, description: str, server: subprocess.Popen, timeout: float):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        require(server.poll() is None, f"Compositor exited while waiting for {description}")
        result = predicate()
        if result:
            return result
        time.sleep(0.025)
    raise RuntimeError(f"Timed out waiting for {description}")


def command(path: Path, text: str) -> str:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(3)
        stream.connect(str(path))
        stream.sendall((text + "\n").encode())
        response = bytearray()
        while not response.endswith(b"\n"):
            chunk = stream.recv(4096)
            require(bool(chunk), f"Control socket closed before responding to {text!r}")
            response.extend(chunk)
            require(len(response) <= 65536, "Control response exceeded size limit")
    decoded = response.decode().strip()
    require(not decoded.startswith("ERR"), f"Control {text!r} failed: {decoded}")
    return decoded


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--software", action="store_true")
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--log-dir", type=Path, default=ROOT / "target" / "desktop-smoke-logs")
    parser.add_argument("--inside-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    require(args.timeout > 0, "--timeout must be positive")
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        require(not args.inside_bus, "dbus-run-session did not create a session bus")
        require(shutil.which("dbus-run-session") is not None, "dbus-run-session is required")
        return subprocess.run(
            ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()),
             *sys.argv[1:], "--inside-bus"], cwd=ROOT, check=False,
        ).returncode
    if not args.no_build:
        subprocess.run(["cargo", "build", "--workspace", "--bins", "--locked"], cwd=ROOT, check=True)
        subprocess.run(
            ["cargo", "build", "-p", "aura-compositor", "--example", "smoke_client", "--locked"],
            cwd=ROOT, check=True,
        )
        subprocess.run(["cargo","build","-p","aura-shell","--example","layer_lifecycle","--locked"],cwd=ROOT,check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT,
    ))
    target = Path(metadata["target_directory"]) / "debug"
    binaries = {
        "compositor": target / "aura-compositor",
        "client": target / "examples" / "smoke_client",
        "shell": target / "aura-shell",
        "launcher": target / "aura-launcher",
        "layer_lifecycle": target / "examples" / "layer_lifecycle",
    }
    for binary in binaries.values():
        require(binary.is_file(), f"Missing built binary {binary}")
    args.log_dir.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.setdefault("RUST_LOG", "info")
    if args.software:
        environment["LIBGL_ALWAYS_SOFTWARE"] = "1"
    processes = []
    files = []
    server = None
    xvfb = None
    logs = {}

    def launch(name: str, argv: list[str], env: dict[str, str]):
        path = args.log_dir / f"{name}.log"
        logs[name] = path
        output = path.open("w")
        files.append(output)
        process = subprocess.Popen(argv, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    try:
        if not environment.get("DISPLAY") and not environment.get("WAYLAND_DISPLAY"):
            xvfb = start_xvfb(environment)
        with tempfile.TemporaryDirectory(prefix="aura-integration-") as directory:
            runtime = Path(directory)
            host_wayland = environment.get("WAYLAND_DISPLAY")
            host_runtime = environment.get("XDG_RUNTIME_DIR")
            if host_wayland and host_runtime and not os.path.isabs(host_wayland):
                environment["WAYLAND_DISPLAY"] = str(Path(host_runtime) / host_wayland)
            environment["XDG_RUNTIME_DIR"] = directory
            environment["XDG_CONFIG_HOME"] = str(runtime / "config")
            environment["AURA_WINIT_RESOLUTION"] = "1024x768"
            environment["AURA_OUTPUT_SCALE"] = "1"
            control = runtime / "aura-integration-control.sock"
            environment["AURA_CONTROL_SOCKET"] = str(control)
            socket_name = "wayland-aura-integration"
            server = launch("compositor", [str(binaries["compositor"]), "--socket", socket_name], environment)
            wait_for(lambda: (runtime / socket_name).is_socket() and control.is_socket(),
                     "Wayland and control sockets", server, args.timeout)
            client_environment = environment | {"WAYLAND_DISPLAY": socket_name}
            fixture=launch("layer-lifecycle",[str(binaries["layer_lifecycle"])],client_environment)
            require(fixture.wait(timeout=args.timeout)==0,"layer NULL-buffer unmap/remap regression failed")
            wait_for(lambda:json.loads(command(control,"status"))["layers"]==0,"layer fixture cleanup",server,args.timeout)
            first = launch("client-one", [str(binaries["client"]), "--frames", "2", "--exercise-lifecycle", "--exercise-fullscreen", "--exercise-popup", "--decoration-mode", "client", "--stay", "--title", "AuraSmokeOne"], client_environment)
            second = launch("client-two", [str(binaries["client"]), "--frames", "2", "--exercise-fullscreen", "--decoration-mode", "server", "--stay", "--title", "AuraSmokeTwo"], client_environment)

            def ready():
                for name, process in [("client-one", first), ("client-two", second)]:
                    require(process.poll() is None, f"{name} exited before lifecycle checks")
                    if "SMOKE_CLIENT_OK" not in logs[name].read_text(errors="replace"):
                        return False
                windows = json.loads(command(control, "windows"))
                return windows if len(windows) == 2 else False

            windows = wait_for(ready, "two real client frame callbacks", server, args.timeout)
            by_title = {window["title"]: window for window in windows}
            require(set(by_title) == {"AuraSmokeOne", "AuraSmokeTwo"}, f"Unexpected windows: {windows}")
            one, two = by_title["AuraSmokeOne"]["id"], by_title["AuraSmokeTwo"]["id"]

            def current():
                return {window["id"]: window for window in json.loads(command(control, "windows"))}

            require(sum(window["focused"] for window in windows) == 1, "Expected exactly one focused window")
            require(command(control, f"focus {one}") == "OK", "focus command failed")
            require(current()[one]["focused"], "Focus did not move to first client")
            require(command(control, f"minimize {one}") == "OK", "minimize command failed")
            state = current()
            require(state[one]["minimized"] and not state[one]["focused"], "Minimized window retained focus")
            require(state[two]["focused"], "Minimize did not transfer focus to visible client")
            require(command(control, f"restore {one}") == "OK", "restore command failed")
            state = current()
            require(not state[one]["minimized"] and state[one]["focused"], "Restore did not show/focus first client")
            for mode in ["stage-manager", "tiling", "floating"]:
                require(command(control, f"layout {mode}") == "OK", "Layout command failed")
                status = json.loads(command(control, "status"))
                require(status["layout"] == mode, f"Layout state did not change to {mode}")
            for quality in ["ultra", "balanced", "reduced-transparency"]:
                require(command(control, f"quality {quality}") == "OK", "Quality command failed")
                status = json.loads(command(control, "status"))
                require(status["quality"] == quality, f"Quality state did not change to {quality}")

            for name in ["shell", "launcher"]:
                process = launch(name, [str(binaries[name]), "--smoke-test"], client_environment)
                require(process.wait(timeout=args.timeout) == 0, f"{name} failed real layer-shell frame checks")
            require(len(current()) == 2, "Closing shell/launcher unexpectedly removed client windows")
            require(command(control, f"close {one}") == "OK", "Close command failed")
            require(first.wait(timeout=args.timeout) == 0, "Client did not handle xdg close cleanly")
            wait_for(lambda: one not in current(), "closed client removal", server, args.timeout)
            stop(second)
            wait_for(lambda: not current(), "disconnected client cleanup", server, args.timeout)
            require(command(control, "quit") == "OK", "Compositor quit command failed")
            require(server.wait(timeout=args.timeout) == 0, "Compositor did not exit cleanly")
            require(not control.exists(), "Control socket leaked after compositor shutdown")
            require(not (runtime / socket_name).exists(), "Wayland socket leaked after compositor shutdown")
        print("PASS: layer NULL-unmap/remap reservations, two real SHM clients including maximize/remap/popup, focus/minimize/restore, layout/quality state, shell/launcher frames, close/disconnect and socket cleanup.")
        print(f"Logs: {args.log_dir}")
        return 0
    except (RuntimeError, subprocess.TimeoutExpired, OSError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        for name, path in logs.items():
            print(f"{name}:\n{tail(path)}", file=sys.stderr)
        return 1
    finally:
        for process in reversed(processes):
            stop(process)
        stop(xvfb)
        for output in files:
            output.close()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
