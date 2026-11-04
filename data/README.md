# Session and portal data

These files are templates for development packaging. Aura currently implements a
nested winit session, so the session entry explicitly requires an existing
Wayland/X11 graphical host. It is **not** a working bare-metal display-manager
session. Do not install it as the default login session until a DRM backend is
implemented and tested.

`aura-session` starts the compositor, waits until its Wayland socket accepts a
connection, starts the shell, and terminates/reaps its child process groups when
SIGINT/SIGTERM arrives or a child exits. With no existing session bus, it replaces
itself with `dbus-run-session -- aura-session ...`. Run `aura-session --help` for
binary path overrides and startup timeout. It preserves the host display for
winit and sets the nested display only on client processes.

Portal activation data belongs under `$prefix/share/dbus-1/services/` and
`$prefix/share/xdg-desktop-portal/portals/`. Ensure the built `aura-session` binary
is installed in PATH; package managers may replace Exec with an absolute path.
No process or D-Bus state survives a cloud snapshot; start the session again.

The portal service exposes the standard ScreenCast backend method signatures,
real control-session Close and disconnect cleanup, and reports
`AvailableSourceTypes = 0`. Capture methods return `NotSupported` because the
compositor does not yet provide a PipeWire capture transport. It never returns
invented stream/node IDs. Installing these descriptors does not enable screen
sharing; retain another portal backend if that functionality is required.

To test activation on a desktop host, install descriptors to a user or system
XDG data directory and run `aura-session --portal` on a private session bus. Do not
replace or restart a user's existing desktop portal as part of automated setup.
