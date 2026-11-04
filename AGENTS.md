# AGENTS.md — Working Safely in `aura-de`

## 1. Core Engineering Rules

1. **Keep `crates/aura-core` 100% I/O-Free**: Never add `smithay`, `wayland-*`, `wgpu`, `tokio`, or `zbus` dependencies to `aura-core`. All window placement, Stage Manager grouping, parabolic Dock sizing, spring animation math, and `LiquidGlassMaterial` validation must remain pure Rust so `cargo test -p aura-core` runs in milliseconds on any host without a GPU or Wayland socket.
2. **Shader Parity (`crates/aura-render`)**: When modifying `LiquidGlassUniforms` in `crates/aura-render/src/lib.rs`, update both `shaders/liquid_glass.wgsl` (used by `wgpu` shell clients) and `shaders/liquid_glass.frag` (used by Smithay's `GlesRenderer`) in lockstep so `#[repr(C)]` std140/std430 alignment matches exactly.
3. **Never Block the Calloop Event Loop**: In `crates/aura-compositor`, all D-Bus calls, file I/O, and heavy asset decoding must run on background threads or async channels bridged via `calloop::channel::Channel`, never synchronously inside frame callbacks or input handlers.

## 2. Commands (Run on Target Linux Host)

| Task | Command | Notes |
| :--- | :--- | :--- |
| **Workspace Structure** | `python3 scripts/verify_workspace.py` | Manifest, dependency-boundary and source-path checks only; does not prove runtime behavior |
| **Core Unit Tests** | `cargo test -p aura-core --locked` | Must build independently without GPU, Wayland socket or feature overrides |
| **Workspace Tests** | `cargo test --workspace --locked` | Rust implementation tests; retain per-suite counts and distinguish skips |
| **Type & Borrow Check** | `cargo check --workspace --all-targets --locked` | Check all crates and examples |
| **Formatting** | `cargo fmt --all -- --check` | Check without rewriting unrelated files |
| **Linting** | `cargo clippy --workspace --all-targets --locked -- -D warnings` | Zero warnings before committing |
| **Nested Integration Smoke** | `python3 scripts/smoke_nested.py --software` | Real SHM client configure/frame callback test; uses Xvfb if needed |

## 3. Development and Evidence

Use the existing checkout. Cloud tasks are already isolated; do not create a Git worktree unless explicitly requested. Preserve user changes. The Rust toolchain is pinned by `rust-toolchain.toml`; keep `Cargo.lock` tracked and use `--locked` for repeatable builds.

Install Debian/Ubuntu host dependencies through `scripts/install-deps.sh`. Add `--native` only for optional native-backend dependencies and `--testing` for software-rendered integration checks. A cloud user-local sysroot can be selected through `AURA_SYSROOT` and `scripts/dev-env.sh`.

Keep [implementation status](docs/implementation-status.md) honest: unit tests, shader validation, GPU rendering, real Wayland client integration and hardware session tests provide different evidence. Never infer a working service from a log message, fake resource ID, open port, or a zero-exit demonstration. Fail explicitly when a requested backend or capability is unsupported.
