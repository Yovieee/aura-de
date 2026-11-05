#!/usr/bin/env python3
"""Verify real PipeWire buffers and cleanup with a deterministic transport fixture.

Build first: cargo build -p aura-session --example capture_frames --locked
Run: python3 crates/aura-session/tests/pipewire_roundtrip.py

Requires a local PipeWire daemon binary/modules, but no session manager, display,
or hardware. This tests the session export; compositor GPU readback has separate
coverage and must also be exercised for an end-to-end screen capture claim.
"""
import argparse
import os
from pathlib import Path
import secrets
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    default_target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    parser.add_argument("--binary", type=Path, default=default_target / "debug/examples/capture_frames")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error("build capture_frames before running this check")
    if not shutil.which("pipewire"):
        parser.error("pipewire daemon is required")

    with tempfile.TemporaryDirectory(prefix="aura-pipewire-") as name:
        root = Path(name)
        env = dict(os.environ, XDG_RUNTIME_DIR=name)
        env.pop("PIPEWIRE_REMOTE", None)
        token = secrets.token_hex(32)
        env["AURA_CAPTURE_SOCKET"] = str(root / "capture.sock")
        env["AURA_CAPTURE_TOKEN"] = token
        stopping = threading.Event()
        counts = {"probes": 0, "subscribers": 0}
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(env["AURA_CAPTURE_SOCKET"])
        listener.listen()
        listener.settimeout(0.1)

        def serve():
            while not stopping.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                with connection:
                    connection.settimeout(1)
                    line = bytearray()
                    while len(line) < 80 and not line.endswith(b"\n"):
                        part = connection.recv(1)
                        if not part:
                            break
                        line.extend(part)
                    if line == f"PROBE {token}\n".encode():
                        counts["probes"] += 1
                        connection.sendall(b"AURACAP1 2 2\n")
                        continue
                    if line != f"{token}\n".encode():
                        continue
                    counts["subscribers"] += 1
                    pixels = bytes([255, 0, 0, 255, 0, 255, 0, 255,
                                    0, 0, 255, 255, 1, 2, 3, 255])
                    sequence = 0
                    while not stopping.is_set():
                        header = struct.pack("<8sIIIQIQ", b"AURAFRM1", 2, 2, 8,
                                             sequence, len(pixels), time.time_ns())
                        try:
                            connection.sendall(header + pixels)
                        except (BrokenPipeError, ConnectionResetError):
                            break
                        sequence += 1
                        stopping.wait(1 / 30)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        with (root / "daemon.log").open("w+") as log:
            daemon = subprocess.Popen(["pipewire"], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 3
                while not (root / "pipewire-0").exists() and daemon.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                if not (root / "pipewire-0").exists():
                    raise RuntimeError("isolated PipeWire daemon did not start")
                result = subprocess.run([str(binary), "--export", "--expect-pattern"],
                                        env=env, timeout=15, capture_output=True, text=True)
                if result.returncode:
                    log.seek(0)
                    raise RuntimeError(result.stderr + "\n" + log.read()[-4000:])
                if counts != {"probes": 1, "subscribers": 1} or "cleanup=removed" not in result.stdout:
                    raise RuntimeError("probe/subscription separation or node cleanup failed")
                print(result.stdout, end="")
                print("PASS authenticated no-frame probe; exact RGBA buffers; live-node removal on stop")
            finally:
                stopping.set()
                daemon.terminate()
                daemon.wait(timeout=3)
                worker.join(timeout=2)
                listener.close()


if __name__ == "__main__":
    main()
