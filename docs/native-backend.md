# Experimental native backend

The optional `native` feature provides a single-output DRM/KMS backend alongside
the existing nested `winit` backend. Native startup uses libseat for device
ownership, GBM/EGL for composed buffers and libinput for seat input. The backend
shares Wayland protocols, window placement and shell rendering with the nested
path.

This implementation has not been validated on physical DRM hardware. Compiling
the feature and rejecting unavailable devices do not establish a working native
desktop session. The default development session and installed login descriptor
remain nested previews.

## Build and launch

Install the native development libraries, then build the compositor with its
feature enabled:

```bash
sudo bash scripts/install-deps.sh --native --testing
cargo build --workspace --bins --examples --locked
cargo build -p aura-compositor --features native --locked
```

On a Linux machine with a connected display, a supported DRM device and an
available seat, launch the supervisor from a spare virtual terminal:

```bash
aura-session --backend drm
```

Use the same prefix or binary overrides when running uninstalled debug binaries.
For example, add `target/debug` to PATH, or select compositor/shell paths through
the supervisor's `--compositor` and `--shell` flags. See `aura-session --help`.
Seat access is managed by libseat and the host's logind/seatd configuration; the
development installer does not configure a seat daemon or grant device access.

Native output dimensions come from the selected connector's mode.
`AURA_WINIT_RESOLUTION` applies only to the nested backend. The normal integer
`AURA_OUTPUT_SCALE` and `AURA_GLASS_QUALITY` controls apply to both backends.

Two optional selectors make device choice explicit:

```bash
AURA_DRM_DEVICE=/dev/dri/card0 AURA_DRM_CONNECTOR=DP-1 aura-session --backend drm
```

`AURA_DRM_DEVICE` selects an exact DRM primary character device. Missing paths,
regular files, non-DRM character devices and render nodes are rejected before
opening a seat or publishing Wayland/control sockets.
`AURA_DRM_CONNECTOR` selects an exact connected connector name. An unavailable
requested device or connector fails explicitly rather than silently choosing a
different display.

The backend uses a preferred usable mode, falling back to the connector's first
usable mode, and presents one connector on one GPU. Seat disable/enable pauses
and resumes DRM/input processing. Removing the selected GPU or disconnecting its
output ends the session with an error; reconnect it and restart Aura. Continuing
through output hotplug is not implemented.

Initial seat activation and active page-flip completion have bounded deadlines.
A lost page-flip completion fails the session instead of leaving an indefinitely
frozen display. Timeout/recovery behavior still requires hardware validation.

The `AURA_BACKEND=drm` environment variable also selects the backend; an explicit
`--backend` flag takes precedence. A compositor built without `native` explains
the required feature and exits before advertising a desktop.

## Validation in a cloud environment

The current cloud machine exposes no `/dev/dri`, `/dev/input` or `/dev/tty0`, and
has no active VKMS device or installed kernel modules. It cannot validate KMS
scanout, physical input, seat switching or hardware graphics performance.

```bash
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 scripts/native_backend_smoke.py
LIBGL_ALWAYS_SOFTWARE=1 EGL_PLATFORM=surfaceless cargo test -p aura-compositor --bin aura-compositor native_render_tests::composites_glass_scene_for_native_and_nested_output_orientations --locked -- --ignored --exact --nocapture
```

The smoke gate builds both feature variants using Cargo's shared dependency
cache, keeps temporary executable copies and restores the default compositor
afterward. It checks CLI/environment selection, the feature-disabled diagnostic,
and missing, regular-file or non-DRM character-device paths. Every failure must
leave both Wayland and control sockets unpublished. Logs and `results.json` are retained under
`target/native-backend-smoke-logs`; `hardware_tested` is explicitly false.
Prebuilt variants can be supplied with `--no-build --default-bin PATH
--native-bin PATH`.

These checks exercise real argument parsing and real startup error handling;
they do not emulate a DRM device or claim that a frame reached a monitor.

The separate surfaceless GLES test renders an asymmetric scene and glass panel
through both output transforms and reads actual pixels. It checks orientation
and retained-scene rendering on a software driver. This remains distinct from
GBM allocation, KMS presentation and physical display verification.

## Checks required on target hardware

Before treating this as a native desktop release, verify and retain evidence for:

- Actual KMS presentation of the shell and real Wayland application buffers on
  the selected connector, including negotiated window controls and output scale.
- Keyboard, relative and absolute pointer input, buttons, scrolling and physical
  touchpad gestures through libinput.
- Seat disable/enable and VT switching, with input and rendering paused while
  the seat is inactive and restored after reacquisition.
- Clean shutdown and restoration of console/device ownership after SIGTERM,
  startup failure and client/compositor failure.
- Selected-output disconnect, reconnect, suspend/resume and mode/driver failures.
- Performance and rendering correctness on a named GPU/driver combination.

Multiple outputs, hardware DMA-BUF client import, direct scanout and comprehensive
hotplug/power-management support remain separate work. ScreenCast capture is
explicitly disabled for the native backend until its output orientation and
readback are validated. The tested nested ScreenCast path remains available.
Sharing compositor code does not substitute for native application compatibility
tests.
