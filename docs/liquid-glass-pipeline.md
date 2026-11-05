# Liquid Glass Shader & Optical Pipeline Guide

The optical layers below describe the design. Shader parsing/validation, successful GPU render/read-back and live compositor backdrop integration are separate checks; consult [implementation status](implementation-status.md) for observed results. A shell client has no automatic access to other Wayland clients' pixels; the nested implementation captures the scene and renders authorized Aura glass surfaces in the compositor.

The nested implementation retains compositor-owned scene and blur textures. Scene generation, output dimensions/scale and clear color invalidate capture; material blur changes invalidate blur while foreground-only shell changes reuse it. Unchanged outputs skip GPU rendering, capture and blur work. Dirty outputs are still redrawn as full frames; this is not a complete regional-damage renderer. Active ScreenCast deliberately requests periodic frames and has separate asynchronous PBO readback. The current compositor format is SDR RGBA8; no HDR or target-hardware performance claim follows from the software-renderer checks.

## Physical Inspiration & Optical Layers

Aura DE's **Liquid Glass** aesthetic models each translucent shell surface (Top Menu Bar, Floating Dock, Control Center, Notification Toast, and Window Titlebar) as a curved dielectric slab with index of refraction $n_{\text{glass}} \in [1.33, 1.65]$ (default $1.48$, matching crown/acrylic optical glass).

| Layer | Mathematical Model | Visual Effect |
| :--- | :--- | :--- |
| **1. Backdrop Blur Pyramid** | 3-pass [Dual-Kawase](https://blog.frost.kiwi/dual-kawase/) downsample + upsample | Smooth, flicker-free frosted background without boxy artifacts |
| **2. Continuous Squircle Mask** | $L_4$ Superellipse SDF: $(\|q_x\|^4 + \|q_y\|^4)^{1/4} - r$ | Eliminates harsh curvature jumps where straight edges meet corners |
| **3. Bevel Edge Refraction** | 3D normal reconstruction + Snell's law (`refract(I, N, 1.0 / ior)`) | Bends background Wallpaper/Window colors inward along the curved glass rim |
| **4. Chromatic Dispersion** | Per-channel IOR offsets ($\Delta n = \pm 0.018$) | Subtle prismatic color separation on high-contrast background edges |
| **5. Fresnel Specular Rim** | Schlick approximation $R_0 + (1 - R_0)(1 - \mathbf{N}\cdot\mathbf{V})^3$ | Crisp top-left light catch and soft bottom edge luminosity |
