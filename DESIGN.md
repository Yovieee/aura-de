# Aura DE — Comprehensive Architecture & Liquid Glass Design Plan

## 1. Executive Summary & Architectural Vision

**Aura Desktop Environment (`aura-de`)** is a next-generation Linux desktop environment engineered from scratch in Rust around Wayland. It unites macOS spatial desktop ergonomics—a persistent 32px Top Menu Bar, a parabolic-magnification Floating Dock, traffic-light window controls, Stage Manager side-strips, and interruptible spring animations—with a physical GPU shader pipeline called **Liquid Glass**.

Whereas conventional Linux compositors treat background blur as a flat 2D box/Gaussian filter clipped to circular rounded rectangles, Aura DE models every shell surface and window titlebar as a curved dielectric optical lens ($n_{\text{glass}} = 1.48$). Each surface evaluates a continuous-curvature $L_4$ superellipse (macOS squircle) Signed Distance Field, reconstructs 3D bevel surface normals along its perimeter, bends background rays via Snell's law with RGB chromatic dispersion, and layers a directional Schlick Fresnel specular highlight over a bandwidth-efficient [Dual-Kawase](https://blog.frost.kiwi/dual-kawase/) blur mip pyramid.

## 2. System Architecture & Crate Boundaries

To keep core window layout logic 100% deterministic and testable without a GPU or active Wayland socket, the workspace is partitioned into six modular layers:

| Crate | Layer Role | Primary Rust Dependencies | I/O & Hardware Boundary |
| :--- | :--- | :--- | :--- |
| `crates/aura-core` | Pure spatial layout (`Floating`, `StageManager`, `DynamicTiling`), critically damped spring solver, parabolic Dock magnification, and `LiquidGlassMaterial` math | `glam`, `serde`, `thiserror`, `bitflags` | **Zero I/O** (Pure CPU math; unit tests run in `<10ms` headless) |
| `crates/aura-render` | 64-byte `std140` uniform packers, Dual-Kawase FBO mip planner, and dual-target shaders (`liquid_glass.wgsl` + `liquid_glass.frag`) | `wgpu`, `bytemuck`, `glam`, `aura-core` | GPU shader compilation & uniform buffer alignment |
| `crates/aura-compositor` | Wayland display server (`xdg-shell`, `wlr-layer-shell`, `ext-background-effect-v1`), damage-tracked backdrop pyramid, input routing | `smithay`, `calloop`, `wayland-server`, `aura-core`, `aura-render` | Wayland sockets, DRM/KMS, GBM, EGL, `libinput`, `libseat` |
| `crates/aura-shell` | Client-side layer-shell Menu Bar (`32px` exclusive zone), Floating Liquid Glass Dock, and `org.aura.Shell1` D-Bus service | `smithay-client-toolkit`, `wgpu`, `zbus`, `tokio` | Wayland client socket (`wlr-layer-shell`), D-Bus session bus |
| `crates/aura-launcher` | Spotlight-style command & fuzzy app launcher overlay (`SpotlightIndex`) | `wgpu`, `smithay-client-toolkit`, `serde` | Wayland `wlr-layer-shell` (`Overlay` layer, keyboard interactivity) |
| `crates/aura-session` | Session bootstrapper, fullscreen direct-scanout evaluator, and `org.freedesktop.impl.portal.ScreenCast` PipeWire portal | `zbus`, `tokio`, `aura-core` | `systemd-logind`, PipeWire DMA-BUF screencast streams |

## 3. Liquid Glass Optical Design System & Shader Pipeline

### 3.1 Five-Stage Compositor Rendering Pipeline

1. **Stage 1 — HDR Wallpaper & Bottom Layer Pass**: The compositor renders the desktop wallpaper and `wlr-layer-shell` background surfaces into an offscreen linear half-float framebuffer (`Rgba16Float`) to preserve highlight dynamic range before refraction.
2. **Stage 2 — Damage-Tracked Dual-Kawase Blur Pyramid**: Whenever a surface carrying an [ext-background-effect-v1](https://wayland.app/protocols/ext-background-effect-v1) blur region intersects output damage rectangles, the compositor executes $N=3$ downsample passes (5-tap hardware-interpolated kernel) followed by $N=3$ upsample passes (8-tap tent filter). A 3-pass pyramid achieves an effective 32px–64px Gaussian sigma using only **3.8 amortized texture fetches per full-resolution pixel** compared to 64 fetches for a separable 1D Gaussian.
3. **Stage 3 — Continuous-Curvature Squircle Geometry ($L_4$ Superellipse SDF)**: Standard circular rounded rectangles suffer from a sudden curvature jump ($G^1$ discontinuity) where the straight edge meets the corner arc. Aura evaluates an $L_4$ norm superellipse SDF in fragment space:
   $$d(\mathbf{p}) = \left( |q_x|^4 + |q_y|^4 \right)^{1/4} + \min(\max(q_x, q_y), 0) - r, \quad \mathbf{q} = |\mathbf{p}| - (\mathbf{b} - r)$$
4. **Stage 4 — 3D Bevel Normal Reconstruction & Snell's Law Chromatic Dispersion**: Within the perimeter bevel band ($-b_{\text{width}} < d(\mathbf{p}) < 0$), normalized bevel progress $t = (d + b_{\text{width}})/b_{\text{width}}$ maps to a lens elevation angle $\theta = 0.82 \cdot \frac{\pi}{2} t$. Using the normalized 2D SDF gradient $\nabla d$, the 3D surface normal is $\mathbf{N} = (\nabla d_x \sin\theta, \nabla d_y \sin\theta, \cos\theta)$. Incident rays $\mathbf{I} = (0, 0, -1)$ refract through per-channel indices ($n_R = 1.462, n_G = 1.480, n_B = 1.498$) to sample separate RGB coordinates from the blurred backdrop texture.
5. **Stage 5 — Directional Fresnel Rim & Adaptive Vibrancy**: A Schlick Fresnel term $(1 - \mathbf{N}\cdot\mathbf{V})^3$ combined with a Blinn-Phong lobe ($\exp = 28$) along top-left light vector $\mathbf{L} = \text{normalize}(-0.55, -0.65, 0.52)$ produces a crisp physical glass rim highlight over the vibrancy tint.

### 3.2 Material Presets & Design Tokens

| Surface Component | Preset Name | IOR ($n$) | Dispersion ($\Delta n$) | Corner Radius ($r$) | Bevel Width ($b$) | Kawase Passes | Base Vibrancy Tint (`RGBA`) |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Floating Dock Pill** | `macos_dock_default` | `1.48` | `0.018` | `24.0 px` | `14.0 px` | `3` (Quarter/Eighth) | `[0.94, 0.96, 1.00, 0.28]` |
| **Top Menu Bar** | `menu_bar_sheet` | `1.45` | `0.012` | `0.0 px` | `4.0 px` | `3` | `[0.95, 0.97, 1.00, 0.24]` |
| **Control Center & Toasts** | `dark_control_center` | `1.50` | `0.022` | `20.0 px` | `12.0 px` | `3` | `[0.10, 0.12, 0.16, 0.42]` |
| **Window Titlebar Chrome** | `window_header_glass` | `1.46` | `0.014` | `14.0 px` | `8.0 px` | `2` | `[0.92, 0.94, 0.98, 0.32]` |
| **Low-Power / Accessibility** | `reduced_transparency` | `1.00` | `0.000` | `24.0 px` | `2.0 px` | `0` (Disabled) | `[0.16, 0.18, 0.22, 0.92]` |

## 4. Spatial Window Management, Stage Manager & Animation Physics

* **Floating Mode (Default macOS Ergonomics)**: Windows spawn cascaded around the workspace center with server-negotiated Liquid Glass titlebars (`xdg-decoration-unstable-v1`), top-left Traffic Light buttons (`Close` in `#FF5F56`, `MinimizeToDock` in `#FFBD2E`, `ToggleZoomFullscreen` in `#27C93F`), and Sequoia-style edge-drag smart snap zones.
* **Stage Manager Mode**: Keeps the focused window centered on the main stage while docking background windows into a `196px`-wide left-hand vertical strip of live Liquid Glass thumbnails (`132px` height each).
* **Dynamic Tiling Mode**: Automatically arranges windows into a 55% master / 45% vertical stack split for keyboard-centric engineering workflows.
* **240Hz Critically Damped Springs & Parabolic Dock Magnification**: Sub-stepped second-order ODE springs ($\omega_0 = 24.0\text{ rad/s}, \zeta = 1.0$) and a raised-cosine Dock icon magnifier (`52px` to `84px` peak).

## 5. Stepwise Implementation Roadmap & Verification Matrix

| Milestone | Scope & Deliverables | Verification Criteria (Definition of Done) | Current Status |
| :--- | :--- | :--- | :--- |
| **Milestone 1: Core Math & Layout Skeleton** | `Cargo.toml`, `aura-core` (`layout.rs`, `material.rs`, `spring.rs`, `dock.rs`) | Deterministic unit tests (`cargo test -p aura-core`) + `verify_workspace.py` mathematical invariant checks | **Completed & Verified** |
| **Milestone 2: Liquid Glass Shader Engine** | `aura-render` (`LiquidGlassUniforms`, Dual-Kawase planner, `.wgsl` & `.frag` shaders) | 64-byte `std140` alignment check, Dual-Kawase mip halving tests, shader path validation | **Completed & Verified** |
| **Milestone 3: Nested Compositor & Shell** | `aura-compositor` (`state.rs`, `render_pass.rs`, `main.rs`) & `aura-shell` (`main.rs`, `ipc.rs`) | `--backend winit --smoke-test` initializes state, maps test windows, and builds `FrameRenderPlan` | **Completed & Verified** |
| **Milestone 4: Interactive Input & Gestures** | `crates/aura-compositor/src/input.rs` (`libinput` 3-finger swipe & Traffic Light hit-testing) | Gesture classification & 40px titlebar hit-testing verified | **Completed & Verified** |
| **Milestone 5: Spotlight Launcher** | `crates/aura-launcher` (`wlr-layer-shell` overlay with fuzzy app search) | Prefix/keyword ranking & uniform packer verified | **Completed & Verified** |
| **Milestone 6: Bare-Metal DRM/KMS & Portals** | `crates/aura-session` (direct scanout bypass & `xdg-desktop-portal` PipeWire screencast) | Fullscreen scanout bypass & D-Bus portal interface verified | **Completed & Verified** |
