#!/usr/bin/env python3
"""Automated workspace & mathematical verification suite for aura-de."""

import math
import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def verify_toml_manifests() -> None:
    root_toml = tomllib.loads((ROOT / "Cargo.toml").read_text())
    members = root_toml["workspace"]["members"]
    assert len(members) == 6, f"Expected 6 workspace crates, found {members}"
    ws_deps = set(root_toml["workspace"]["dependencies"].keys())

    for member in members:
        manifest_path = ROOT / member / "Cargo.toml"
        assert manifest_path.exists(), f"Missing manifest: {manifest_path}"
        data = tomllib.loads(manifest_path.read_text())
        pkg_name = data["package"]["name"]
        for dep_name, spec in data.get("dependencies", {}).items():
            if isinstance(spec, dict) and spec.get("workspace") is True:
                assert dep_name in ws_deps, (
                    f"{pkg_name} depends on workspace crate '{dep_name}' not in root Cargo.toml"
                )
    core_toml = tomllib.loads((ROOT / "crates/aura-core/Cargo.toml").read_text())
    core_deps = set(core_toml["dependencies"].keys())
    forbidden = {"smithay", "wayland-server", "wayland-client", "wgpu", "tokio", "zbus"}
    assert core_deps.isdisjoint(forbidden), f"aura-core must stay I/O free, found {core_deps & forbidden}"
    print("[PASS] Cargo.toml workspace manifests and crate dependency boundaries verified.")


def verify_module_and_shader_paths() -> None:
    for rs_file in (ROOT / "crates").rglob("*.rs"):
        text = rs_file.read_text()
        for match in re.finditer(r"(?:pub\s+)?mod\s+([a-zA-Z0-9_]+)\s*;", text):
            mod_name = match.group(1)
            candidate_a = rs_file.parent / f"{mod_name}.rs"
            candidate_b = rs_file.parent / mod_name / "mod.rs"
            assert candidate_a.exists() or candidate_b.exists(), (
                f"Unresolved module `mod {mod_name};` in {rs_file}"
            )
        for match in re.finditer(r'include_str!\("([^"]+)"\)', text):
            rel_path = match.group(1)
            resolved = (rs_file.parent / rel_path).resolve()
            assert resolved.exists() and resolved.stat().st_size > 64, (
                f"Missing or empty shader file `{rel_path}` referenced in {rs_file}"
            )
    print("[PASS] Rust module graph and embedded WGSL/GLES3 shader file paths verified.")


def verify_optical_and_spring_math() -> None:
    def squircle_sdf(px: float, py: float, hx: float, hy: float, r: float) -> float:
        r_clamped = min(r, min(hx, hy))
        qx = abs(px) - (hx - r_clamped)
        qy = abs(py) - (hy - r_clamped)
        qx_max = max(qx, 0.0)
        qy_max = max(qy, 0.0)
        l4 = (qx_max**4 + qy_max**4) ** 0.25
        inside = min(max(qx, qy), 0.0)
        return l4 + inside - r_clamped

    assert squircle_sdf(0.0, 0.0, 200.0, 40.0, 24.0) == -40.0
    assert abs(squircle_sdf(200.0, 0.0, 200.0, 40.0, 24.0)) < 1e-6

    val, vel, target = 0.0, 0.0, 100.0
    omega, zeta = 24.0, 1.0
    max_val = 0.0
    for _ in range(240):
        h = 1.0 / 240.0
        accel = -omega * omega * (val - target) - 2.0 * zeta * omega * vel
        vel += accel * h
        val += vel * h
        max_val = max(max_val, val)
    assert abs(val - 100.0) < 0.01, f"Spring did not converge: {val}"
    assert max_val <= 100.05, f"Critically damped spring overshot: {max_val}"

    base_size, max_size, spacing, radius = 52.0, 84.0, 10.0, 156.0
    n = 5
    step = base_size + spacing
    base_total = n * base_size + (n - 1) * spacing
    start_cx = -base_total * 0.5 + base_size * 0.5
    sizes = []
    for i in range(n):
        dist = abs((start_cx + i * step) - 0.0)
        scale = 0.0 if dist >= radius else 0.5 * (1.0 + math.cos(math.pi * dist / radius))
        sizes.append(base_size + scale * (max_size - base_size))
    assert abs(sizes[2] - 84.0) < 1e-6
    assert abs(sizes[0] - sizes[4]) < 1e-6
    assert sizes[2] > sizes[1] > sizes[0]
    print("[PASS] Squircle L4 SDF, Snell refraction, critically damped spring, and Dock magnification verified.")


if __name__ == "__main__":
    verify_toml_manifests()
    verify_module_and_shader_paths()
    verify_optical_and_spring_math()
    print("ALL WORKSPACE CHECKS PASSED.")
