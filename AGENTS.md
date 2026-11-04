# AGENTS.md — Working Safely in `aura-de`

## 1. Core Engineering Rules

1. **Keep `crates/aura-core` 100% I/O-Free**: Never add `smithay`, `wayland-*`, `wgpu`, `tokio`, or `zbus` dependencies to `aura-core`. All window placement, Stage Manager grouping, parabolic Dock sizing, spring animation math, and `LiquidGlassMaterial` validation must remain pure Rust so `cargo test -p aura-core` runs in milliseconds on any host without a GPU or Wayland socket.
2. **Shader Parity (`crates/aura-render`)**: When modifying `LiquidGlassUniforms` in `crates/aura-render/src/lib.rs`, update both `shaders/liquid_glass.wgsl` (used by `wgpu` shell clients) and `shaders/liquid_glass.frag` (used by Smithay's `GlesRenderer`) in lockstep so `#[repr(C)]` std140/std430 alignment matches exactly.
3. **Never Block the Calloop Event Loop**: In `crates/aura-compositor`, all D-Bus calls, file I/O, and heavy asset decoding must run on background threads or async channels bridged via `calloop::channel::Channel`, never synchronously inside frame callbacks or input handlers.

## 2. Commands (Run on Target Linux Host)

| Task | Command | Notes |
| :--- | :--- | :--- |
| **Workspace & Math Audit** | `python3 scripts/verify_workspace.py` | Verifies all 6 TOML manifests, module paths, shader uniforms, and optical math (Verified) |
| **Unit Tests** | `cargo test --workspace` | Runs unit tests in `aura-core`, `aura-render`, `aura-launcher`, and `aura-session` (Unverified on sandbox; run on Linux host) |
| **Type & Borrow Check** | `cargo check --workspace --all-targets` | Validates all 6 crates including Smithay/SCTK bindings |
| **Linting** | `cargo clippy --workspace --all-targets -- -D warnings` | Enforce zero warnings before committing |
| **Nested Compositor Smoke Run** | `cargo run -p aura-compositor -- --backend winit --smoke-test` | Initializes state, compiles shaders, tests gesture hit-testing, and exits cleanly |
