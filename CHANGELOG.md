# Changelog

All notable changes to the **Aura Desktop Environment (`aura-de`)** project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Terminal desktop entries and bounded `org.freedesktop.Application.Activate` with shell-free Exec fallback; launcher activation runs on a worker.
- Live application discovery for open launchers and the Dock, preserving user overrides and Hidden masking.
- Pointer-driven Dock magnification, matching pixel drawing/hit geometry, leave reset and real input/activation regression coverage.
- Real nested Smithay Wayland server, layer-shell clients, XDG application launching, D-Bus notifications and process supervision.
- Executable wgpu and GLES Liquid Glass passes, real scene capture, opaque fallback and driver/readback checks.
- Regression coverage for focus, minimize/restore, stall-safe springs, invalid materials, remap/maximize/popups and layer reservation cleanup.
- End-to-end headless input/session tests, development packaging and verified rootless Debian dependency setup.
- Nested fullscreen and negotiated server-side decorations, with traffic-light controls and titlebar input.
- Event-driven output redraw, retained scene/blur caches and counter-based idle/cache integration checks.
- Monitor ScreenCast transport using asynchronous PBO readback and actual PipeWire buffers, explicit consent, a sharing indicator/Stop action and request/session/disconnect cleanup.
- Exact-pixel PipeWire transport and end-to-end consent/composed-frame test harnesses; CI explicitly exercises these and the driver-dependent PBO test.

### Changed

- Pin the development toolchain, retain application dependency locking, and add CI plus Debian/Ubuntu host setup instructions.
- Replace the previous mathematical demonstration's smoke claims with a real nested Wayland client test harness.
- Track implemented behavior and observed validation separately in `docs/implementation-status.md`; native DRM/KMS and complete application compatibility still require separate evidence. ScreenCast is limited to fixed-size, single-monitor, hidden-cursor SDR RGBA capture; resize ends the stream. Zero-copy, HDR and multioutput are not claimed.

### Documentation correction

The `0.1.0` entries below record the initial design/model slices. They did not constitute verified runtime support for a compositor, graphical shell, hardware session or screencast portal. Refer to the current implementation status rather than treating those historical descriptions as feature guarantees.

## [0.1.0] - 2026-10-17

### Added

- **Slice 1 (`crates/aura-core`)**: Pure-Rust window layout engine (`Floating`, `StageManager`, and `DynamicTiling` modes), traffic-light window controls, 240Hz critically damped spring animation solver (`Spring1D`, `SpringRect`), parabolic Dock icon magnification calculator (`DockMagnifier`), and `LiquidGlassMaterial` optical parameter validator.
- **Slice 2 (`crates/aura-render`)**: GPU shader pipeline featuring 64-byte `std140` `LiquidGlassUniforms`, Dual-Kawase downsample/upsample GLES3 shaders, and $L_4$ squircle SDF + Snell's law refractive + chromatic dispersion shaders in both WGSL (`liquid_glass.wgsl`) and GLES 3.0 (`liquid_glass.frag`).
- **Slice 3 (`crates/aura-compositor` & `crates/aura-shell`)**: [Smithay](https://crates.io/crates/smithay) + [Calloop](https://crates.io/crates/calloop) Wayland compositor supporting nested `winit` development, `xdg-shell` window mapping, and `ext-background-effect-v1` blur region scheduling, paired with the SCTK Top Menu Bar, Floating Liquid Glass Dock, and `org.aura.Shell1` [zbus](https://crates.io/crates/zbus) service.
- **Slice 4 (`crates/aura-compositor/src/input.rs`)**: Three-finger touchpad swipe gesture classifier and 40px Liquid Glass titlebar / Traffic Light pointer hit-testing.
- **Slice 5 (`crates/aura-launcher`)**: Fuzzy application and command launcher (`SpotlightIndex`) with prefix/keyword ranking.
- **Slice 6 (`crates/aura-session`)**: Fullscreen DRM/KMS direct-scanout bypass evaluator (`can_direct_scanout`) and `org.freedesktop.impl.portal.ScreenCast` PipeWire screencast portal interface.
- **Documentation & UI Mockups**: Added `DESIGN.md`, `README.md`, `AGENTS.md`, `docs/`, `assets/ui-mockup.svg`, and `.env.example`.
