# XWayland application support

Aura can optionally start a rootless XWayland server for X11 applications. The
default build and session keep this compatibility layer disabled. XWayland
windows share Aura's window model, rendering and input routing with Wayland
windows; this is separate from running Aura itself inside an X11 host through
the nested `winit` backend.

## Build and start

Install the optional server package and build the compositor feature:

```bash
sudo bash scripts/install-deps.sh --testing --xwayland
cargo build --workspace --bins --examples --features aura-compositor/xwayland --locked
```

For the Debian 13 user-local sysroot, use
`bash scripts/install-deps-user.sh --native --testing --xwayland` and activate
that sysroot as described in the main README. The rootless installer still uses
signed repository metadata and verifies package SHA-256 values before
extraction.

The host must provide a real `/tmp/.X11-unix` directory owned by root or the
session user. A world-writable directory must have its sticky bit set. Aura
rejects missing directories, symlinks and unsafe ownership/modes before
starting XWayland. Install the host's `xserver-common` package and apply its X11
tmpfiles setup when this prerequisite is absent; the host setup manages this
shared directory. User-local package extraction does not run that setup.

Start a supervised session from an existing desktop:

```bash
target/debug/aura-session --backend winit --xwayland
```

The supervisor waits for XWayland and its window manager to become ready before
starting the shell. The shell, launcher and applications started through Exec
receive the new server's `DISPLAY`. The compositor retains the host display
needed by `winit`; an X11 application must not receive that host display as a substitute
for an unavailable Aura XWayland server. Inherited host X11 authentication is
not reused for the new server.

Activation through an existing D-Bus service uses that service's own environment.
Aura does not rewrite a shared session bus's activation environment, so an
externally activated X11 application is not guaranteed to select Aura's display.
Use an Exec launch or an explicit `DISPLAY` when checking a particular X11
application. See [application launching](application-launching.md).

The optional `AURA_XWAYLAND=1` setting also enables this path; `--no-xwayland`
explicitly disables it. A build without the feature reports the required build
option. An enabled session fails explicitly
if the XWayland executable cannot start or readiness is not reached. Default
Wayland-only sessions do not require the XWayland package.

XWayland uses an owned, mode-`0600` filesystem X11 socket. Display allocation
skips existing socket and lock paths to preserve host displays. A bound,
non-listening abstract Unix socket reserves the matching abstract address;
clients are accepted only through the filesystem socket. TCP X11 listening is
disabled. The compositor control status reports the selected
`xwayland_display` and whether its window manager is ready; applications
launched outside the supervised environment must select that display explicitly.

The initial path disables Glamor and uses shared-memory buffers. Hardware
DMA-BUF import, DRI3/Glamor acceleration and accelerated X11 application
compatibility are not established by this implementation.

Initial X11 window state is read on a bounded background worker before the
managed window is mapped. Deadlines, cancellation and generation checks keep
these reads out of frame/input handlers and discard late results for windows
that have already been unmapped or destroyed. The integration gate covers
initial maximization and Motif client-decoration hints as well as ordinary
window-state changes.

## Validation

```bash
python3 scripts/xwayland_smoke.py
# After building the feature-enabled binaries and examples:
python3 scripts/xwayland_smoke.py --no-build
```

The dedicated integration harness uses an isolated Xvfb host, session bus and
configuration directory. Its X11 clients connect to the actual Aura XWayland
display through libX11. Logs and `results.json` are retained in
`target/xwayland-smoke-logs`. Connecting to an X socket alone does not establish
that a window was mapped, rendered or given input; the implementation status
records the checks that actually passed.

This is initial X11 compatibility support. X11 clients use their X server for
X11-to-X11 selections; a selection/clipboard bridge between Wayland and X11 is
not implemented. Broad application compatibility, drag-and-drop and
application-specific window behavior need separate validation. Native DRM
hardware, multioutput and IME support are not established by a nested XWayland
test.
