# Wayland Protocols & Session Lifecycle

Aura Compositor (`aura-compositor`) implements the following core and staging Wayland protocols:

| Protocol | Purpose in Aura DE |
| :--- | :--- |
| `wl_compositor` / `wl_shm` / `linux-dmabuf-v1` | Zero-copy DMA-BUF texture sharing and shared-memory fallback buffers |
| `xdg-shell` (`xdg_wm_base`) | Standard application window lifecycle, popups, and resize/move interactive grabs |
| `xdg-decoration-unstable-v1` | Negotiates client-side vs. server-side Liquid Glass titlebar decorations with macOS traffic-light buttons |
| `wlr-layer-shell-unstable-v1` | Anchors `aura-shell` surfaces (Background wallpaper, Top Menu Bar with `32px` exclusive zone, Floating Dock, and Overlay Lock Screen) |
| `ext-background-effect-v1` | Standard Wayland staging protocol allowing clients to declare region masks that receive compositor-rendered Dual-Kawase Liquid Glass blur |
