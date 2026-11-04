# Aura Desktop Environment (`aura-de`)

**Aura DE** is a bleeding-edge Linux Desktop Environment built from scratch in Rust on Wayland. Inspired by macOS spatial ergonomics and modern **Liquid Glass** optical design, Aura combines a [Smithay](https://crates.io/crates/smithay)-powered Wayland compositor with a real-time refractive shader pipeline, a continuous-curvature Menu Bar and parabolic Dock, a Spotlight-style command launcher, and a pure-Rust D-Bus & PipeWire session daemon layer.

## Architecture Highlights

* **Liquid Glass Optical Engine (`crates/aura-render`)**: Multi-pass [Dual-Kawase](https://blog.frost.kiwi/dual-kawase/) backdrop blur paired with $L_4$ superellipse (macOS continuous squircle) Signed Distance Field geometry, Snell's law edge refraction ($n = 1.48$), RGB chromatic dispersion, and Fresnel specular rim lighting in both GLES 3.0 and [wgpu](https://crates.io/crates/wgpu) WGSL.
* **Pure Spatial & Spring Layout Core (`crates/aura-core`)**: Zero-I/O window manager supporting macOS floating stacking, Stage Manager side-strips, smart-snap tiling halves/quarters, parabolic Dock icon magnification, and interruptible 240Hz critically damped spring animations.
* **Wayland Compositor & Touchpad Gestures (`crates/aura-compositor`)**: Built on [Smithay](https://crates.io/crates/smithay) and [Calloop](https://crates.io/crates/calloop) with support for `xdg-shell`, `wlr-layer-shell`, [ext-background-effect-v1](https://wayland.app/protocols/ext-background-effect-v1), 3-finger touchpad swipe gestures (`input.rs`), and Traffic Light window control hit-testing.
* **Layer-Shell Menu Bar, Dock & IPC (`crates/aura-shell`)**: Built on [Smithay Client Toolkit (SCTK)](https://crates.io/crates/smithay-client-toolkit), [wgpu](https://crates.io/crates/wgpu), and [zbus](https://crates.io/crates/zbus) (`org.aura.Shell1` & `org.freedesktop.Notifications`).
* **Spotlight Command Launcher (`crates/aura-launcher`)**: Fuzzy desktop entry search and quick command overlay rendered on the `wlr-layer-shell` `Overlay` layer.
* **Session & PipeWire Portal Daemon (`crates/aura-session`)**: Evaluates fullscreen DRM/KMS direct-scanout bypass and exposes `org.freedesktop.impl.portal.ScreenCast` for zero-copy PipeWire DMA-BUF screen sharing.

## Repository Structure

| Path | Description |
| :--- | :--- |
| `Cargo.toml` | Workspace manifest pinning shared dependencies across all 6 crates |
| `crates/aura-core/` | Pure window layout, Stage Manager, spring physics, Dock magnification, and Liquid Glass material models + unit tests |
| `crates/aura-render/` | Dual-Kawase blur & Liquid Glass refraction shaders (`.wgsl` and `.frag`) and `std140` uniform packers |
| `crates/aura-compositor/` | Smithay + Calloop Wayland compositor binary (`aura-compositor`) + 3-finger gesture & Traffic Light input hit-testing |
| `crates/aura-shell/` | Wayland `wlr-layer-shell` Menu Bar + Dock client and `zbus` D-Bus service (`aura-shell`) |
| `crates/aura-launcher/` | Spotlight-style fuzzy app & command launcher overlay (`aura-launcher`) |
| `crates/aura-session/` | Session bootstrapper, fullscreen direct-scanout evaluator, and PipeWire screencast portal (`aura-session`) |
| `assets/ui-mockup.svg` | Vector architectural mockup of the Aura DE Liquid Glass desktop |
| `DESIGN.md` | Comprehensive architecture specification and Liquid Glass optical math |
| `AGENTS.md` | Contributor and AI agent guide with build, test, and safety boundaries |

## Build, Test, and Run (Linux Host)

```bash
# 1. Copy default development configuration
cp .env.example .env

# 2. Run automated workspace & mathematical verification
python3 scripts/verify_workspace.py

# 3. Run unit tests across the workspace on your Linux host
cargo test --workspace

# 4. Run the compositor smoke test or launch a nested winit development window
cargo run -p aura-compositor -- --backend winit --smoke-test
AURA_BACKEND=winit cargo run -p aura-compositor -- --backend winit

# 5. Launch the Liquid Glass Menu Bar, Parabolic Dock, Spotlight Launcher, and Session Portal
WAYLAND_DISPLAY=wayland-aura-0 cargo run -p aura-shell
WAYLAND_DISPLAY=wayland-aura-0 cargo run -p aura-launcher
cargo run -p aura-session
```
