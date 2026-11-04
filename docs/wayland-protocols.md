# Wayland Protocols & Session Lifecycle

The nested compositor must publish and exercise protocol globals through a real Wayland client. Treat the following list as the protocol contract and roadmap; exact implemented/tested coverage is tracked in [implementation status](implementation-status.md), rather than inferred from dependency names.

| Protocol | Purpose and current coverage |
| :--- | :--- |
| `wl_compositor` / `wl_shm` | Base surface lifecycle and shared-memory buffers; required by the integration smoke client |
| `linux-dmabuf-v1` | Planned hardware-buffer import; not currently advertised by the nested server |
| `xdg-shell` (`xdg_wm_base`) | Standard application window lifecycle, popups, and resize/move interactive grabs |
| `xdg-decoration-unstable-v1` | Planned server-side decoration negotiation; clients currently provide their own decorations |
| `wlr-layer-shell-unstable-v1` | Anchors shell panel/Dock and launcher overlay, with output bounds and exclusive-zone handling |
| `ext-background-effect-v1` | Planned standard region-mask negotiation; current glass is compositor-owned for Aura shell namespaces only |

## Session lifecycle

`aura-session` supervises a nested compositor and its shell. It preserves the host display environment for winit, waits for the nested Wayland socket, and passes that socket only to client processes. If no session bus exists, `dbus-run-session` creates one. SIGINT/SIGTERM or child failure stops and reaps owned child process groups. See `aura-session --help` for binary overrides and startup timeout.

Run the nested session inside an existing X11/Wayland session. A native DRM/KMS backend, logind/libseat pause/resume and VT switching require separate implementation and hardware evidence. Do not install the nested preview as a machine's default bare-metal login session.

The portal backend exports explicit capabilities and standard method signatures. Until a compositor/PipeWire capture transport exists, ScreenCast operations return `NotSupported`; fabricated node IDs are never a substitute for a stream. Activation descriptors do not enable recording. Lock-screen support is a separate security/protocol task and is not provided by a layer-shell overlay.
