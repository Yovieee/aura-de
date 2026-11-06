# Session and portal data

These files are templates for development packaging. The default session entry
launches the nested winit backend and requires an existing Wayland/X11 graphical
host. The optional experimental DRM/KMS backend is described in
`docs/native-backend.md`; it has not been validated on target hardware. The
installed entry is **not** a working bare-metal display-manager session. Do not
install it as the default login session before native hardware validation.

`aura-session` starts the compositor, waits until its Wayland socket accepts a
connection, starts the shell, and terminates/reaps its child process groups when
SIGINT/SIGTERM arrives or a child exits. With no existing session bus, it replaces
itself with `dbus-run-session -- aura-session ...`. Run `aura-session --help` for
binary path overrides and startup timeout. It preserves the host display for
winit and removes host display variables for the DRM child. Shell/application
processes use Aura's Wayland socket after the compositor is ready.

The optional `--xwayland` session mode additionally waits for its rootless X11
server and window manager before starting the shell. Exec-launched applications
receive that server's `DISPLAY`; the host display remains reserved for
the nested compositor. The XWayland package and `aura-compositor/xwayland`
feature are opt-in. See [XWayland setup](../docs/xwayland.md).

Portal activation data belongs under `$prefix/share/dbus-1/services/` and
`$prefix/share/xdg-desktop-portal/portals/`. Ensure the built `aura-session` binary
is installed in PATH; package managers may replace Exec with an absolute path.
No process or D-Bus state survives a cloud snapshot; start the session again.

The supervised portal service probes its authenticated compositor source and a
real PipeWire daemon before advertising a monitor capability. ScreenCast needs
explicit consent; the shell indicates active sharing and provides Stop. Close,
revocation and frontend disconnect remove the compositor subscription and actual
PipeWire node. Standalone `aura-session --portal` has no supervisor capability and
does not grant capture access. No simulated node IDs are returned.

Capture supports one monitor with hidden cursor, fixed-size SDR RGBA pixels and
at most 4096 pixels per axis. Output resize ends a stream and requires fresh
selection/consent. This is a CPU-copy path, not zero-copy DMA-BUF capture. Native
backend capture is disabled pending validation; HDR and multioutput are unsupported.
Installing descriptors alone does not create a supervised source, daemon or user consent. See
`docs/screencast.md` and the observed evidence in `docs/implementation-status.md`.

To test activation on a desktop host, install descriptors to a user or system
XDG data directory and run `aura-session --portal` on a private session bus. Do not
replace or restart a user's existing desktop portal as part of automated setup.

## Settings application entry

The standard development install includes `aura-settings` and
`share/applications/org.aura.Settings.desktop`. The entry is shown in an Aura
session and launches controls for the current session's layout, glass quality
and Do Not Disturb state. It needs the running compositor and `org.aura.Shell1`
on the same session bus. Settings changes persist layout, glass quality and Do
Not Disturb in the user's versioned preferences file; load/save failures appear
in the panel. It does not configure system hardware. See
`docs/desktop-controls.md`.
