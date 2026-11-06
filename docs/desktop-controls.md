# Notifications, settings and layout controls

These controls apply to the nested Aura desktop. Layout, glass quality and Do Not
Disturb changes made through Settings are saved for later sessions. Native
hardware session support and broader system configuration remain unfinished.

## Notification actions

The current notification appears in a card below the menu bar, leaving the
screen-sharing Stop control reachable. Actions are displayed two per page; the
service accepts up to eight action pairs, including an optional `default` action
invoked by clicking the body. Malformed action lists are rejected rather than
silently changing their keys. Action labels are application data, never commands
to execute in the shell.

UI input targets the notification ID, generation and action page that were
actually rendered. Replacements, expiry and stale input are checked before emission.
`ActionInvoked` and `NotificationClosed` signals are directed to the originating
application's unique bus sender. Applications cannot close or replace another
sender's notification through its ID.

A successful action closes a non-resident notification with close reason 2.
The `resident` hint keeps it available after an action; the dismiss button still
closes it. Expiry uses reason 1, user dismissal reason 2, and application
`CloseNotification` reason 3. Do Not Disturb suppresses automatic cards without
preventing manual browsing. Image-icon support remains unfinished.

## Notification history

Open **Notifications** from the menu bar to browse active notifications followed
by archived entries, with six rows per page. Selecting an active entry opens its
current action card only while its ID and generation are still valid. Up/Down
select a row, Left/Right change pages, and Enter opens the selection. Escape
returns from archived details to the list, or closes the list. Selecting
an archived entry opens read-only text and its close reason; archived entries do
not retain action keys or application bus ownership and cannot invoke actions.

Expiry, dismissal, application close and successful non-resident actions archive
the notification with the actual close reason. Replacing a live notification
updates that notification rather than adding an archive entry. **Clear history**
removes archived entries while leaving active notifications available.

History is kept only in memory for the current shell session. It stores at most
100 archived entries, newest first, with application name, summary and body
clipped to 128, 256 and 2048 characters respectively. Restarting the shell clears
history; notification text is not persisted alongside settings.

## Settings

Open **Settings** from the menu bar or run `aura-settings` with the Aura Wayland
socket and session bus. The settings application is another binary in the
existing `aura-shell` crate. It provides:

- Floating, Stage Manager and Tiling layout selection.
- Ultra, Balanced and Reduced Transparency glass quality.
- Do Not Disturb on/off.

The UI queries actual compositor/shell state on background workers and shows
acknowledged results. An IPC failure is visible instead of appearing as a
successful change. Consecutive changes are queued and confirmed by readback.
Pointer input and Tab/arrows/Enter operate controls; Escape closes the panel.
Audio, display, network, power, account and other system settings remain outside
this panel.

The normal shell restores preferences at startup from
`$XDG_CONFIG_HOME/aura/preferences.json`, falling back to
`$HOME/.config/aura/preferences.json`. The configuration directory must be an
absolute path. A successful Settings change or Shell1 setter saves a complete
snapshot of the acknowledged layout, glass quality and Do Not Disturb state.
Menu, keyboard and gesture shortcuts remain session-only until the next settings
save includes their current state.

Preferences use strict version 1 JSON, bounded to 16 KiB, with these fields:

```json
{
  "version": 1,
  "layout": "floating",
  "glass_quality": "balanced",
  "do_not_disturb": false
}
```

Valid layouts are `floating`, `stage-manager` and `tiling`; valid glass qualities
are `ultra`, `balanced` and `reduced-transparency`. Writes use a temporary file,
sync and atomic replacement with mode `0600`. Invalid JSON, unknown fields,
unsupported versions and non-regular files, including symbolic links, are
preserved instead of overwritten. Repair or move the existing file before
trying to save again.

The settings panel reports load and save errors. A change can already be applied
to the current session when saving fails; it must not be treated as restored for
the next session until saving succeeds. The readonly `org.aura.Shell1`
`PreferencesEnabled` and `PersistenceError` properties expose this status.
Library tests and examples can still start ephemeral services without reading or
writing user preferences.

The development installer stages `aura-settings` and
`share/applications/org.aura.Settings.desktop`, shown only in an Aura session.

## Layout shortcuts and gesture scope

| Input | Result |
| --- | --- |
| `Super+Up` | Stage Manager |
| `Super+Down` | Floating |
| `Super+Left` or `Super+Right` | Toggle Tiling/Floating |

The raw three-finger swipe handler tracks begin/update/end and changes layout
once on a valid completion. Cancelled, small, invalid or non-three-finger
sequences do not commit a change. It uses the same layout mapping as the keyboard
shortcuts.

Smithay's current winit backend uses `UnusedEvent` for raw swipe events. This
means physical touchpad swipes cannot reach the nested compositor through that
backend. Synthetic backend lifecycle tests validate dispatch logic; Xvfb input
tests validate the keyboard equivalents. Neither is evidence of physical
touchpad operation. A native input backend and hardware checks are still needed.

## Checks

Build binaries and examples before executing the isolated integration harness:

```bash
cargo build --workspace --bins --examples --locked
dbus-run-session -- cargo run -p aura-shell --example dbus_smoke --locked
python3 scripts/desktop_controls_smoke.py --no-build
python3 scripts/preferences_history_smoke.py --no-build
```

The controls harness exercises actual notification signals, pointer-operated
settings and keyboard layout shortcuts on a private D-Bus bus and Xvfb display.
Logs and `results.json` are retained in `target/desktop-controls-smoke-logs`.
The preferences/history harness uses an isolated configuration directory to
check restart restoration, visible persistence failures and notification
browsing. Its logs and `results.json` are retained in
`target/preferences-history-smoke-logs`.
Current validation results are recorded in
[implementation status](implementation-status.md).
