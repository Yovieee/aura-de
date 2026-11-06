#!/usr/bin/env python3
"""Verify nested idle rendering, glass cache invalidation and real server-chrome input."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

from integration_desktop import command, require, wait_for
from session_smoke import Input
from smoke_nested import ROOT, start_xvfb, stop, tail


COUNTERS = ("rendered_frames", "scene_captures", "blur_passes", "scene_generation")


def counters(status):
    return {key: status[key] for key in COUNTERS}


def drag(input_, start, end):
    """Deliver a held-button motion through the isolated X server, with input barriers."""
    input_.t.XTestFakeMotionEvent(input_.d, 0, *start, 0)
    input_.x.XFlush(input_.d)
    time.sleep(0.04)
    input_.t.XTestFakeButtonEvent(input_.d, 1, 1, 0)
    input_.x.XFlush(input_.d)
    time.sleep(0.04)
    input_.t.XTestFakeMotionEvent(input_.d, 0, *end, 0)
    input_.x.XFlush(input_.d)
    time.sleep(0.08)
    input_.t.XTestFakeButtonEvent(input_.d, 1, 0, 0)
    input_.x.XFlush(input_.d)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--log-dir", type=Path, default=ROOT / "target" / "window-render-smoke-logs")
    parser.add_argument("--inside-bus", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    require(args.timeout > 0, "--timeout must be positive")
    # The shell notification used below must never reach the user's session bus.
    if not args.inside_bus:
        require(shutil.which("dbus-run-session") is not None, "dbus-run-session is required")
        return subprocess.run(
            ["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()),
             *sys.argv[1:], "--inside-bus"], cwd=ROOT, check=False,
        ).returncode
    require(bool(os.environ.get("DBUS_SESSION_BUS_ADDRESS")), "Private session bus is unavailable")
    require(shutil.which("gdbus") is not None, "gdbus is required")
    if not args.no_build:
        subprocess.run(
            ["cargo", "build", "-p", "aura-compositor", "-p", "aura-shell", "--bins", "--locked"],
            cwd=ROOT, check=True,
        )
        subprocess.run(
            ["cargo", "build", "-p", "aura-compositor", "--example", "smoke_client", "--locked"],
            cwd=ROOT, check=True,
        )
    target = Path(json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"], cwd=ROOT,
    ))["target_directory"]) / "debug"
    binaries = [target / "aura-compositor", target / "aura-shell", target / "examples" / "smoke_client"]
    for binary in binaries:
        require(binary.is_file(), f"Missing binary {binary}; rerun without --no-build")
    args.log_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(LIBGL_ALWAYS_SOFTWARE="1", RUST_LOG="info", AURA_GLASS_QUALITY="balanced")
    # Always create a private display: XTest may only target this test's compositor.
    xvfb = start_xvfb(env)
    processes, files, logs = [], [], {}
    input_ = None
    evidence = {}

    def launch(name, argv, environment):
        logs[name] = args.log_dir / f"{name}.log"
        output = logs[name].open("w")
        files.append(output)
        process = subprocess.Popen(argv, cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    try:
        with tempfile.TemporaryDirectory(prefix="aura-window-render-") as directory:
            runtime = Path(directory)
            os.chmod(runtime, 0o700)
            data = runtime / "data"
            (data / "applications").mkdir(parents=True)
            control = runtime / "control.sock"
            socket_name = "wayland-window-render"
            env.update(
                XDG_RUNTIME_DIR=directory, XDG_CONFIG_HOME=str(runtime / "config"), XDG_DATA_HOME=str(data), XDG_DATA_DIRS=str(data),
                AURA_CONTROL_SOCKET=str(control), AURA_WINIT_RESOLUTION="1024x640", AURA_OUTPUT_SCALE="1",
            )
            # Avoid inherited session capture producers introducing deliberate periodic frames.
            for key in [key for key in env if key.startswith("AURA_CAPTURE_")]:
                env.pop(key)
            server = launch("compositor", [str(binaries[0]), "--socket", socket_name], env)
            wait_for(lambda: control.is_socket() and (runtime / socket_name).is_socket(),
                     "Wayland and control sockets", server, args.timeout)
            client_env = env | {"WAYLAND_DISPLAY": socket_name}
            shell = launch("shell", [str(binaries[1])], client_env)

            def status():
                require(shell.poll() is None, "Shell exited during render checks")
                return json.loads(command(control, "status"))

            def windows():
                return json.loads(command(control, "windows"))

            wait_for(lambda: status()["mapped_layers"] == 2, "real menu bar and Dock", server, args.timeout)
            client = launch("client", [str(binaries[2]), "--stay", "--idle-on-ready", "--frames", "2",
                                      "--decoration-mode", "server", "--title", "AuraIdleChrome"], client_env)

            def client_ready():
                require(client.poll() is None, "Idle client exited before input checks")
                return "SMOKE_CLIENT_OK" in logs["client"].read_text(errors="replace") and len(windows()) == 1

            wait_for(client_ready, "server-decorated client presentation", server, args.timeout)
            input_ = Input(env["DISPLAY"])
            window_id = windows()[0]["id"]

            def window():
                return next((window for window in windows() if window["id"] == window_id), None)

            def settled():
                # Cover the shell's 500ms state poll as well as the compositor's springs.
                deadline = time.monotonic() + args.timeout
                previous = counters(status())
                unchanged_since = time.monotonic()
                while time.monotonic() < deadline:
                    require(server.poll() is None, "Compositor exited while settling")
                    current = counters(status())
                    if current != previous:
                        previous = current
                        unchanged_since = time.monotonic()
                    if time.monotonic() - unchanged_since >= 0.65:
                        return status()
                    time.sleep(0.025)
                raise RuntimeError(f"Rendering never became idle: {previous}")

            def assert_idle(label):
                before = settled()
                require(before["glass_active"], "Real GLES glass pipeline is unavailable")
                time.sleep(0.3)
                after = status()
                require(counters(before) == counters(after),
                        f"{label}: idle counters changed over 300ms: {counters(before)} -> {counters(after)}")
                evidence[label] = {"before": counters(before), "after_300ms": counters(after)}
                return after

            baseline = assert_idle("initial_idle")
            require(baseline["scene_captures"] > 0 and baseline["blur_passes"] > 0,
                    "Expected an actual captured and blurred scene")
            subprocess.run(
                ["gdbus", "call", "--session", "--dest", "org.freedesktop.Notifications",
                 "--object-path", "/org/freedesktop/Notifications", "--method",
                 "org.freedesktop.Notifications.Notify", "AuraRenderSmoke", "0", "",
                 "Foreground cache probe", "Shell-only dirty buffer", "[]", "{}", "0"],
                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=args.timeout,
            )
            wait_for(lambda: status()["rendered_frames"] > baseline["rendered_frames"],
                     "notification-driven shell redraw", server, args.timeout)
            foreground = assert_idle("foreground_idle")
            require(foreground["scene_captures"] == baseline["scene_captures"],
                    "Foreground-only shell redraw recaptured the desktop scene")
            require(foreground["scene_generation"] == baseline["scene_generation"],
                    "Foreground-only shell redraw invalidated desktop generation")
            require(foreground["blur_passes"] == baseline["blur_passes"],
                    "Foreground-only shell redraw repeated cached blur passes")

            require(command(control, "quality ultra") == "OK", "Quality change failed")
            wait_for(lambda: status()["blur_passes"] > foreground["blur_passes"],
                     "quality-triggered blur invalidation", server, args.timeout)
            quality = assert_idle("quality_idle")
            require(quality["quality"] == "ultra", "Quality was not changed")
            require(quality["scene_captures"] == foreground["scene_captures"],
                    "Blur quality change unnecessarily recaptured the desktop scene")

            rect = window()["rect"]
            input_.click(round(rect["x"] + 33), round(rect["y"] + 16))
            wait_for(lambda: window()["minimized"], "yellow traffic-light minimize", server, args.timeout)
            require(status()["visible_windows"] == 0, "Minimized client remained visible")
            require(command(control, f"restore {window_id}") == "OK", "Restore command failed")
            wait_for(lambda: not window()["minimized"] and window()["focused"],
                     "restored visible focused client", server, args.timeout)
            assert_idle("restored_idle")

            before_drag = window()["rect"]
            start = (round(before_drag["x"] + 120), round(before_drag["y"] + 16))
            end = (start[0] - 64, start[1])
            drag(input_, start, end)
            expected_x = before_drag["x"] - 64
            wait_for(lambda: abs(window()["rect"]["x"] - expected_x) <= 1,
                     "titlebar pointer drag changing actual window geometry", server, args.timeout)
            after_drag = window()["rect"]
            require(all(abs(after_drag[key] - before_drag[key]) <= 1 for key in ("y", "width", "height")),
                    f"Titlebar drag changed size or vertical position: {before_drag} -> {after_drag}")
            evidence["drag"] = {"before": before_drag, "after": after_drag}
            assert_idle("dragged_idle")

            rect = window()["rect"]
            input_.click(round(rect["x"] + 11), round(rect["y"] + 16))
            require(client.wait(timeout=args.timeout) == 0, "Red traffic-light close was not handled cleanly")
            wait_for(lambda: not windows(), "closed client removal", server, args.timeout)
            require(command(control, "quit") == "OK", "Compositor quit failed")
            require(server.wait(timeout=args.timeout) == 0, "Compositor did not exit cleanly")
            require(not control.exists() and not (runtime / socket_name).exists(), "Session sockets leaked")
        (args.log_dir / "results.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print("PASS: idle render counters, foreground/quality glass-cache invalidation, real SSD minimize/restore, titlebar drag and close.")
        print(f"Logs and counter evidence: {args.log_dir}")
        return 0
    except (RuntimeError, subprocess.SubprocessError, OSError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        for name, path in logs.items():
            print(f"{name}:\n{tail(path)}", file=sys.stderr)
        return 1
    finally:
        if input_:
            input_.close()
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
