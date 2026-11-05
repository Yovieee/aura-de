# Application discovery and launching

The launcher and Dock share a desktop-entry parser and launch path. Each UI
receives immutable application snapshots from a background catalog worker.
Directory traversal and parsing, including the initial scan, do not run in UI
input or rendering callbacks. Changes are normally observed within 500 ms,
followed by the UI's next state poll.

Discovery follows XDG data directories, with user entries taking precedence over
system entries. A user `Hidden=true` entry masks the system copy. Creating,
editing or deleting desktop entries updates an already open launcher and the
running Dock. Search preserves its query and selected application when possible.

## Launch behavior

`Exec` is split into arguments without a shell. Double quotes and escapes group
arguments; supported field codes are expanded as separate application data.
Shell substitutions, variables, wildcards and command separators are not
evaluated. A desktop entry's `Path` sets the spawned process's working directory.
No file/URL selection is supplied by the current launcher.

For `DBusActivatable=true`, the desktop-file ID determines the standard bus name
and object path. For example, `org.example.Editor.desktop` uses
`org.example.Editor`, `/org/example/Editor` and
`org.freedesktop.Application.Activate`. Activation has a three-second timeout;
an available `Exec` command is used if activation fails. Valid D-Bus-only entries
do not need `Exec`. The current UI launches applications without files; `Open`
and desktop action selection remain separate work.

For `Terminal=true`, the command runs through an available supported terminal.
Set `AURA_TERMINAL` to choose a terminal or wrapper explicitly:

```bash
export AURA_TERMINAL='foot'
export AURA_TERMINAL='xfce4-terminal --disable-server -x'
# A custom wrapper must include its command separator:
export AURA_TERMINAL='"/path with spaces/terminal-wrapper" --'
```

This setting is an argv prefix, never a shell command. Known terminal programs
receive the appropriate command separator automatically. Missing executables or
invalid prefixes produce a visible launch error. The launcher performs launch
I/O on a worker so input remains responsive while a D-Bus service is slow.
Dock activation uses separate workers (at most four pending launches), so slow
applications do not hold up layout controls or the screen-sharing Stop action.

## Dock interaction and checks

Dock application tiles use the core magnification model. Drawing and clicks
share bounded logical pixel rectangles; leaving the Dock restores normal tile
sizes. The Apps button stays stationary and the window row restores minimized
windows. Unchanged catalogs and idle pointers do not continuously submit frames.
App tiles still use text labels; image icons, animated transitions and broader
desktop settings are not implemented by this change.

Build binaries/examples, then run the private-display/private-bus integration:

```bash
cargo build --workspace --bins --examples --locked
python3 scripts/launcher_dock_smoke.py --no-build
# Additionally require and exercise an installed Xfce terminal on private X11:
python3 scripts/launcher_dock_smoke.py --no-build --terminal-integration
```

The default check uses a real terminal-wrapper process to inspect exact argv and
working-directory behavior, and real D-Bus receivers for activation, error and
timeout fallback. It checks responsive input while activation is pending.
The optional terminal check exercises Xfce itself. Logs and `results.json` are
saved under `target/launcher-dock-smoke-logs`.
