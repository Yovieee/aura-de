#!/usr/bin/env python3
"""Check workspace structure; this is not a Rust, shader, or runtime test."""

import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def verify_toml_manifests() -> None:
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
    members = workspace["members"]
    assert len(members) == 6, f"Expected six workspace crates, found {members}"
    workspace_dependencies = set(workspace["dependencies"])
    for member in members:
        path = ROOT / member / "Cargo.toml"
        data = tomllib.loads(path.read_text())
        for name, spec in data.get("dependencies", {}).items():
            if isinstance(spec, dict) and spec.get("workspace") is True:
                assert name in workspace_dependencies, f"{path}: unknown workspace dependency {name}"
    core = tomllib.loads((ROOT / "crates/aura-core/Cargo.toml").read_text())
    forbidden = {"smithay", "wayland-server", "wayland-client", "wgpu", "tokio", "zbus"}
    assert set(core["dependencies"]).isdisjoint(forbidden), "aura-core must remain free of I/O dependencies"
    print("PASS: workspace manifests and core dependency boundary")


def verify_paths() -> None:
    for path in (ROOT / "crates").rglob("*.rs"):
        source = path.read_text()
        for match in re.finditer(
            r"(?P<attributes>(?:#\[[^\n]*\]\s*)*)(?:pub\s+)?mod\s+(?P<name>[A-Za-z0-9_]+)\s*;",
            source,
        ):
            name = match.group("name")
            explicit = re.search(r'#\[path\s*=\s*"([^\"]+)"\]', match.group("attributes"))
            candidates = [path.parent / explicit.group(1)] if explicit else [
                path.parent / f"{name}.rs", path.parent / name / "mod.rs",
            ]
            assert any(candidate.is_file() for candidate in candidates), f"Unresolved module {name} in {path}"
        for match in re.finditer(r'include_str!\("([^"]+)"\)', source):
            included = (path.parent / match.group(1)).resolve()
            assert included.is_file() and included.stat().st_size > 0, f"Missing embedded file {included}"
    print("PASS: Rust module and embedded file paths")


if __name__ == "__main__":
    verify_toml_manifests()
    verify_paths()
    print("Structure checks passed. Run cargo test and scripts/smoke_nested.py for behavior checks.")
