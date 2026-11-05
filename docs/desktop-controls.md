# Notifications, settings and layout controls

These controls apply to the running nested Aura session. They do not establish
native hardware session support, system configuration or persistent preferences.

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
`CloseNotification` reason 3. Do Not Disturb suppresses the card. This remains a
current-notification view, without a full history browser or image-icon support.

## Settings

Open **Settings** from the menu bar or run `aura-settings` with the Aura Wayland
socket and session bus. The settings application is another binary in the
existing `aura-shell` crate. It provides:

- Floating, Stage Manager and Tiling layout selection.
- Ultra, Balanced and Reduced Transparency glass quality.
- Do Not Disturb on/off.

The UI queries actual compositor/shell state on background workers and shows
acknowledged results. An IPC failure is visible instead of appearing as a
successful change. Consecutive changes are queued and confirmed by readback. Pointer input and Tab/arrows/Enter operate controls; Escape
closes the panel. Preferences apply to this session only. Audio, display,
network, power, account and other system settings remain outside this panel.

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
```

The controls harness exercises actual notification signals, pointer-operated
settings and keyboard layout shortcuts on a private D-Bus bus and Xvfb display.
Logs and `results.json` are retained in `target/desktop-controls-smoke-logs`.
Current validation results are recorded in
[implementation status](implementation-status.md).
