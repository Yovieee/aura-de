# Liquid Glass Shader & Optical Pipeline Guide

## Physical Inspiration & Optical Layers

Aura DE's **Liquid Glass** aesthetic models each translucent shell surface (Top Menu Bar, Floating Dock, Control Center, Notification Toast, and Window Titlebar) as a curved dielectric slab with index of refraction $n_{\text{glass}} \in [1.33, 1.65]$ (default $1.48$, matching crown/acrylic optical glass).

| Layer | Mathematical Model | Visual Effect |
| :--- | :--- | :--- |
| **1. Backdrop Blur Pyramid** | 3-pass [Dual-Kawase](https://blog.frost.kiwi/dual-kawase/) downsample + upsample | Smooth, flicker-free frosted background without boxy artifacts |
| **2. Continuous Squircle Mask** | $L_4$ Superellipse SDF: $(\|q_x\|^4 + \|q_y\|^4)^{1/4} - r$ | Eliminates harsh curvature jumps where straight edges meet corners |
| **3. Bevel Edge Refraction** | 3D normal reconstruction + Snell's law (`refract(I, N, 1.0 / ior)`) | Bends background Wallpaper/Window colors inward along the curved glass rim |
| **4. Chromatic Dispersion** | Per-channel IOR offsets ($\Delta n = \pm 0.018$) | Subtle prismatic color separation on high-contrast background edges |
| **5. Fresnel Specular Rim** | Schlick approximation $R_0 + (1 - R_0)(1 - \mathbf{N}\cdot\mathbf{V})^3$ | Crisp top-left light catch and soft bottom edge luminosity |
