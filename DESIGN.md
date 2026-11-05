# Aura DE — Comprehensive Architecture & Liquid Glass Design Plan

This document specifies design targets. Its equations, protocol lists and visual descriptions are not evidence of a working implementation. See [implementation and verification status](docs/implementation-status.md) for runtime evidence and unfinished platform work.

## 1. Executive Summary & Architectural Vision

**Aura Desktop Environment (`aura-de`)** is a next-generation Linux desktop environment engineered from scratch in Rust around Wayland. Its shell combines a persistent 32px Top Menu Bar, a parabolic-magnification Floating Dock, traffic-light window controls, Stage Manager side-strips, and interruptible spring animations with a physical GPU shader pipeline called **Liquid Glass**.

Whereas conventional Linux compositors treat background blur as a flat 2D box/Gaussian filter clipped to circular rounded rectangles, Aura DE models every shell surface and window titlebar as a curved dielectric optical lens ($n_{\text{glass}} = 1.48$). Each surface evaluates a continuous-curvature $L_4$ superellipse (squircle) Signed Distance Field, reconstructs 3D bevel surface normals along its perimeter, bends background rays via Snell's law with RGB chromatic dispersion, and layers a directional Schlick Fresnel specular highlight over a bandwidth-efficient [Dual-Kawase](https://blog.frost.kiwi/dual-kawase/) blur mip pyramid.

## 2. System Architecture & Crate Boundaries

To keep core window layout logic 100% deterministic and testable without a GPU or active Wayland socket, the workspace is partitioned into six modular layers:

| Crate | Layer Role | Primary Rust Dependencies | I/O & Hardware Boundary |
| :--- | :--- | :--- | :--- |
| `crates/aura-core` | Pure spatial layout (`Floating`, `StageManager`, `DynamicTiling`), critically damped spring solver, parabolic Dock magnification, and `LiquidGlassMaterial` math | `glam`, `serde`, `thiserror`, `bitflags` | **Zero I/O** (headless CPU-only implementation tests) |
| `crates/aura-render` | 64-byte `std140` uniform packers, Dual-Kawase FBO mip planner, and dual-target shaders (`liquid_glass.wgsl` + `liquid_glass.frag`) | `wgpu`, `bytemuck`, `glam`, `aura-core` | GPU shader compilation & uniform buffer alignment |
| `crates/aura-compositor` | Wayland display server (`xdg-shell`, `wlr-layer-shell`, `ext-background-effect-v1`), damage-tracked backdrop pyramid, input routing | `smithay`, `calloop`, `wayland-server`, `aura-core`, `aura-render` | Wayland sockets, DRM/KMS, GBM, EGL, `libinput`, `libseat` |
| `crates/aura-shell` | Client-side layer-shell Menu Bar (`32px` exclusive zone), Floating Liquid Glass Dock, and `org.aura.Shell1` D-Bus service | `smithay-client-toolkit`, `wgpu`, `zbus`, `tokio` | Wayland client socket (`wlr-layer-shell`), D-Bus session bus |
| `crates/aura-launcher` | Command & fuzzy app launcher overlay (`SpotlightIndex`) | `wgpu`, `smithay-client-toolkit`, `serde` | Wayland `wlr-layer-shell` (`Overlay` layer, keyboard interactivity) |
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

* **Floating Mode (Default)**: Windows spawn cascaded around the workspace center with server-negotiated Liquid Glass titlebars (`xdg-decoration-unstable-v1`), top-left Traffic Light buttons (`Close` in `#FF5F56`, `MinimizeToDock` in `#FFBD2E`, `ToggleZoomFullscreen` in `#27C93F`), and edge-drag smart snap zones.
* **Stage Manager Mode**: Keeps the focused window centered on the main stage while docking background windows into a `196px`-wide left-hand vertical strip of live Liquid Glass thumbnails (`132px` height each).
* **Dynamic Tiling Mode**: Automatically arranges windows into a 55% master / 45% vertical stack split for keyboard-centric engineering workflows.
* **Critically Damped Springs & Parabolic Dock Magnification**: Interruptible second-order springs ($\omega_0 = 24.0\text{ rad/s}, \zeta = 1.0$) must handle variable frame time and suspend/resume without diverging. A raised-cosine Dock icon magnifier targets `52px` to `84px` peak.

## 5. Stepwise Implementation Roadmap & Verification Matrix

| Milestone | Scope & Deliverables | Verification Criteria (Definition of Done) | Current Status |
| :--- | :--- | :--- | :--- |
| **Milestone 1: Core Math & Layout** | Pure layout/focus, material validation, spring solver and Dock geometry | Locked core tests including minimize/restore, stall and invalid-parameter regression cases | See current evidence in implementation status |
| **Milestone 2: Liquid Glass GPU Engine** | Uniforms, shaders, textures and executable render passes | Shader validation, GPU read-back/visual checks, resize and fallback behavior | See current evidence in implementation status |
| **Milestone 3: Nested Compositor & Shell** | Persistent Wayland server, client lifecycle, input and layer-shell panels | Real SHM client configure/commit/frame callbacks; visible shell surfaces and interaction | See current evidence in implementation status |
| **Milestone 4: Input & Gestures** | Pointer/keyboard focus, move/resize, chrome actions and libinput gesture lifecycle | Real input events, gesture cancel behavior and client focus restoration | Native gesture validation outstanding |
| **Milestone 5: Launcher & IPC** | XDG desktop entries, application execution, overlay and D-Bus services | Installed application launch, keyboard navigation and service integration | See current evidence in implementation status |
| **Milestone 6: Native Session & Portals** | DRM/KMS session, supervision, PipeWire ScreenCast and conservative scanout | Named-hardware session tests and real captured frames, including cancellation/cleanup | Nested capture/consent/cleanup passed; native hardware gates outstanding |
