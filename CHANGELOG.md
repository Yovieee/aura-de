# Changelog

All notable changes to the **Aura Desktop Environment (`aura-de`)** project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-17

### Added

- **Slice 1 (`crates/aura-core`)**: Pure-Rust macOS-inspired window layout engine (`Floating`, `StageManager`, and `DynamicTiling` modes), traffic-light window controls, 240Hz critically damped spring animation solver (`Spring1D`, `SpringRect`), parabolic Dock icon magnification calculator (`DockMagnifier`), and `LiquidGlassMaterial` optical parameter validator.
- **Slice 2 (`crates/aura-render`)**: GPU shader pipeline featuring 64-byte `std140` `LiquidGlassUniforms`, Dual-Kawase downsample/upsample GLES3 shaders, and $L_4$ squircle SDF + Snell's law refractive + chromatic dispersion shaders in both WGSL (`liquid_glass.wgsl`) and GLES 3.0 (`liquid_glass.frag`).
- **Slice 3 (`crates/aura-compositor` & `crates/aura-shell`)**: [Smithay](https://crates.io/crates/smithay) + [Calloop](https://crates.io/crates/calloop) Wayland compositor supporting nested `winit` development, `xdg-shell` window mapping, and `ext-background-effect-v1` blur region scheduling, paired with the SCTK Top Menu Bar, Floating Liquid Glass Dock, and `org.aura.Shell1` [zbus](https://crates.io/crates/zbus) service.
- **Slice 4 (`crates/aura-compositor/src/input.rs`)**: Three-finger touchpad swipe gesture classifier and 40px Liquid Glass titlebar / Traffic Light pointer hit-testing.
- **Slice 5 (`crates/aura-launcher`)**: Spotlight-style fuzzy application and command launcher (`SpotlightIndex`) with prefix/keyword ranking.
- **Slice 6 (`crates/aura-session`)**: Fullscreen DRM/KMS direct-scanout bypass evaluator (`can_direct_scanout`) and `org.freedesktop.impl.portal.ScreenCast` PipeWire screencast portal interface.
- **Documentation & UI Mockups**: Added `DESIGN.md`, `README.md`, `AGENTS.md`, `docs/`, `assets/ui-mockup.svg`, and `.env.example`.
