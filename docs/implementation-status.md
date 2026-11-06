# Implementation and verification status

Updated 2026-11-06. Aura is a **single-output development desktop**, with a tested nested path and an experimental native backend awaiting target-hardware validation. The original `0.1.0` labels described design slices; implementation and validation are recorded separately, with hardware and compatibility limits stated explicitly.

## Implemented behavior and limits

| Area | Implemented and observed | Remaining limits |
| --- | --- | --- |
| Build baseline | Rust 1.99.1 pin, tracked lockfile, standalone core tests, default/native feature checks and separate native startup-failure gate | The CI workflow is configured; no remote CI run is claimed |
| Pure core | Focus/visibility/restore/close invariants, bounded geometry, analytical stall-safe spring, finite/range material validation and fullscreen geometry policy; 22 standalone tests passed | Layout/physics remain independent of I/O and GPU |
| Nested compositor | Real Wayland socket, SHM/xdg-shell, output/seat, keyboard/pointer, move/resize requests, clipboard focus, maximize, map/unmap/remap, popups, fullscreen and negotiated server-side decorations | One output; hardware DMA-BUF import, XWayland and comprehensive client-conformance coverage remain unfinished |
| Experimental native backend | Feature-gated DRM/GBM/EGL presentation, a single connector on one GPU, libseat ownership and pause/resume, libinput event dispatch, software cursor and VT requests share the compositor runtime; invalid device selections fail before socket publication | No physical KMS presentation, input, VT or hardware-performance evidence. Native ScreenCast is disabled; selected-output/device loss exits and requires restart. No multioutput or hotplug continuation |
| Window lifecycle and chrome | Two real clients; focus/minimize/restore; repeated maximize; NULL unmap/new configure/remap; popup buffers/frame callbacks; fullscreen transitions and initial pre-map fullscreen with both client/server decorations; titlebar drag and traffic-light controls | Stage Manager is a layout mode, not full live-thumbnail/grouping UX; position springs render, while size transitions are not resampled animations |
| Shell and launcher | Real layer-shell menu bar/Dock/overlay, exclusive reservation release/remap, live XDG application catalog, terminal entry launch, bounded D-Bus activation/Exec fallback, shell-free argv and magnified Dock app targets | Fixed bitmap font, text app tiles and one default output; settings are limited to layout/glass/DND |
| D-Bus services | `org.aura.Shell1` controls compositor state; notification replacement/close/expiry/DND; action cards and resident/action/close handling implemented with sender and generation checks | No icon/hint coverage beyond resident behavior or broad application compatibility claim |
| Notification history | Menu browser with active entries and up to 100 archived entries, six rows per page, read-only archived details and clear-history behavior that preserves active notifications; replacements are not archived as closed entries | History is held in memory for the shell session, with bounded text. No disk history persistence or archived action replay |
| Desktop settings | `aura-settings` uses background IPC and acknowledged state queries; Settings/Shell1 setters save layout, glass quality and DND as versioned JSON and shell startup restores them. Atomic mode-`0600` snapshots preserve invalid/unsupported existing files and expose persistence errors | No hardware controls or complete settings suite. A failed save can leave a change applied only to the current session; menu/keyboard/gesture shortcuts remain session-only until the next settings save |
| Gesture dispatch | Three-finger begin/update/end tracking commits a layout change once, rejects cancellation/invalid input and shares layout behavior with `Super+Arrow` shortcuts; the native libinput backend routes raw gesture events to this handler | Current winit input events use `UnusedEvent` for raw swipes. Physical touchpad input remains unverified; wiring, keyboard and synthetic-backend evidence must be distinguished |
| Glass and idle rendering | Actual wgpu/GLES blur/refraction; compositor-owned glass; retained scene/blur textures; unchanged-frame suppression and cache invalidation verified through counters | Dirty outputs redraw full frames. Regional damage, HDR/color management and hardware performance are not established; active capture deliberately requests periodic frames |
| Session supervision | Compositor backend selection, native host-display environment removal and existing supervised lifecycle are implemented; nested startup, Dock/launcher input, app execution and shutdown tested | No bare-metal display-manager login, physical VT switch, hotplug or suspend/resume validation |
| Portal ScreenCast | Real composed pixels through asynchronous PBO readback and PipeWire; default-deny consent, sharing indicator/Stop above fullscreen, request cancellation, fresh consent, session Close and frontend-disconnect cleanup passed | One monitor, hidden cursor, fixed-size SDR RGBA up to 4096 pixels per axis. Resize/source loss ends the stream. No persistent permission, zero-copy, HDR or multioutput claim |
| Capture access boundary | Raw source checks a per-session capability plus kernel peer PID/UID for the supervisor. Portal requests check the current standard frontend bus-name owner and bind session ownership to its unique sender/application ID | Application ID is supplied by the trusted frontend; the backend does not independently authenticate arbitrary native app identities or attest the frontend executable |
| Direct scanout | Conservative eligibility model tests require coverage, buffer/plane/output constraints and atomic-test evidence | No direct client-buffer scanout; the native backend presents composed GBM buffers |
| Packaging and setup | Development staging with licenses and opt-in descriptors; repeatable signed Debian rootless setup and native dependency activation | Development packaging is not a native desktop release or a fresh cloud snapshot |

## Current validation results

The native phase passed 111 default Rust tests and 113 tests with all features;
seven specialized tests remain intentionally ignored in each default command.
All eight nested desktop integration harnesses and nine Xvfb startup tests
passed. The native startup gate passed six cases and left both Wayland and
control sockets unpublished. A separate surfaceless GLES test passed actual
asymmetric scene/glass pixel checks for native and nested output transforms.
None of these results establish physical KMS presentation, seat switching or
native input on hardware.

CI includes both feature variants in checks/Clippy/tests, the native startup gate,
the explicit GLES orientation test and the existing desktop gates. It uploads
`target/native-backend-smoke-logs`, `target/preferences-history-smoke-logs` and
driver logs even on failure.

Notification actions, session settings, keyboard shortcuts and host-focus recovery passed the private-display/private-bus integration. Gesture lifecycle tests use synthetic backend events; physical touchpad input remains unavailable through the current winit backend.

Checks ran on Debian 13, Rust 1.99.1, Mesa 25.0.7 llvmpipe and PipeWire 1.4.2. Rootless setup extracted **85 SHA-256-verified Debian packages**; a clean activation resolved libclang, gdbus, PipeWire and native dependency metadata using the host-overlay sysroot. This prepares compilation/runtime dependencies and does not establish hardware support.

| Command or check | Result |
| --- | --- |
| `python3 -m unittest discover -s scripts/tests -p test_xvfb.py -v` | 9 passed; actual Xlib connection, startup errors/timeouts, partial display reports, environment forwarding, descriptor cleanup and failed helper-process teardown |
| `python3 scripts/verify_workspace.py` | Passed structural checks; no mathematical/runtime claim is inferred from this script |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --workspace --all-targets --all-features --locked` | Native implementation compiles; this does not establish KMS presentation or physical seat/input behavior |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed in the native phase |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed in the native phase |
| `cargo test -p aura-core --locked` | 22 passed, 0 failed |
| `cargo test --workspace --locked` | **111 passed, 0 failed, 7 intentionally ignored** in the native phase. Zero-test binaries/doc-test runs are not counted as behavioral evidence |
| `cargo test --workspace --all-features --locked` | **113 passed, 0 failed, 7 intentionally ignored**; two feature-gated tests cover page-flip deadlines and immediate VT-request render inhibition with bounded no-op recovery, without claiming hardware behavior |
| `cargo test -p aura-render --test gpu --locked -- --ignored --nocapture` | Previous phase: 1 passed; rendered pixels, blur/sharp comparison, clipping/premultiplication, opaque fallback and resize |
| `cargo test -p aura-render --test gles --locked -- --ignored --nocapture` | Previous phase: 1 passed; canonical GLES3 and Smithay shader variants compiled/linked through an actual EGL driver |
| Explicit compositor `capture::tests::gles_pbo_capture_preserves_pack_state_and_reads_composed_rows` with `--ignored --exact` | Passed again in the native phase; real asynchronous PBO readback, composed row order, pack-state preservation and revocation of queued work |
| Private-bus `portal_dbus_lifecycle` and `portal_dbus_consent_denial_and_request_cancellation` with `--ignored` | Previous phase: 2 passed; request/session ownership, capability behavior, consent denial and cancellation |
| Session `capture::tests::saturated_backlog_connect_is_bounded_and_cancellable` with `--ignored --exact` | Previous phase: 1 passed; actual saturated Unix listener queue, bounded timeout and cancellation |
| `dbus-run-session -- cargo run -p aura-shell --example dbus_smoke --locked` | Passed directed action/close signals, sender ownership, replacement generation, resident/default actions, dismissal, expiry, DND and validation |
| `python3 scripts/smoke_nested.py --no-build --software` | Passed real SHM configure/commit/frame smoke; silent-protocol timeout was also checked |
| `python3 scripts/integration_desktop.py --no-build --software` | Passed layer unmap/remap reservation, two-client lifecycle, maximize/remap/popup, focus/minimize/restore, layout/quality and socket cleanup |
| `python3 scripts/session_smoke.py --no-build` | Passed real supervised startup, isolated pointer/keyboard input, Dock/launcher, actual app execution, shutdown/reap and socket cleanup |
| `python3 scripts/launcher_dock_smoke.py --no-build` | Passed again; the optional `--terminal-integration` Xfce result is retained from the previous phase. Verified live install/edit/remove while open, Dock catalog refresh, literal terminal argv/cwd through a fixture and actual Xfce terminal, real D-Bus Activate/fallback, responsive Escape during slow activation, magnified pixel/click targets and window restore |
| `python3 scripts/desktop_controls_smoke.py --no-build` | Passed again after the native phase's focus cleanup: actual client receives pointer release/leave before the physical release, the later release is ignored, and refocus produces a balanced click; settings, DND, notification signals, layout shortcuts, modifier reset and error/cancellation checks also passed |
| `python3 scripts/preferences_history_smoke.py --no-build` | Passed real UI saves and restart restoration, mode-`0600` snapshots, preserved malformed configuration, visible save failure, older live-row actions directed to their owner, read-only archived details, clearing without closing active notifications, close/expiry reasons and history reset independent of preferences; rapid Down/Enter selection, Escape navigation and inert empty-list input |
| `python3 scripts/window_render_smoke.py --no-build` | Passed idle/cache counters, server-chrome minimize/restore, 64 px titlebar drag and close |
| `python3 crates/aura-session/tests/pipewire_roundtrip.py` | Previous phase: passed 10 real PipeWire frames with exact 2×2 RGBA bytes, authenticated no-frame probe separation and node cleanup |
| `python3 scripts/screencast_smoke.py --no-build` | Passed actual consent, compositor pixels over PipeWire, changed-scene checksums, Stop above fullscreen, fresh consent, explicit close, cancellation and disconnect cleanup |
| `python3 scripts/native_backend_smoke.py` | Six passed: feature-disabled CLI/environment requests explain the required build; native-enabled missing, regular-file and non-DRM character-device selections fail before publishing sockets. `hardware_tested: false` |
| Explicit compositor `native_render_tests::composites_glass_scene_for_native_and_nested_output_orientations` with `--ignored --exact` | Passed real surfaceless GLES pixels for both output transforms and retained scene reuse; no DRM device or physical display is exercised |

The five GPU/shader, private-bus and Unix-backlog checks labeled as previous-phase
evidence retain their earlier results. PBO readback and the new orientation test
passed explicitly for this phase. These seven tests remain ignored in the default
commands because they require drivers, private buses or real sockets. CI runs
them explicitly, builds real capture examples, preserves pipeline exit status
and uploads artifacts. See [ScreenCast validation](screencast.md)
for exact driver/socket commands.

## Retained runtime evidence

`target/native-backend-smoke-logs/results.json` records six startup failures,
nonzero exits and unpublished Wayland/control sockets, with `hardware_tested`
set to false. The gate selects private missing/non-device paths before seat
initialization and never substitutes a fake DRM device. See
[native backend validation](native-backend.md) for build/launch commands and the
remaining target-hardware checks.

`target/window-render-smoke-logs/results.json` records stable render/capture/blur counters over 300 ms of idle time. A foreground-only shell update redraws without recapturing or reblurring the scene; changing quality reruns blur while reusing the capture. Real input minimizes/restores a client, drags its titlebar 64 pixels left and closes it.

`target/screencast-smoke-logs/results.json` records default denial and request cancellation with no subscription, followed by a real 1024×640 PipeWire stream. Each consumer verifies ten frames; adding a real client changes the frame checksum. Stop remains usable above a fullscreen client and removes the subscription/node. Fresh consent starts another stream; explicit close and frontend disconnect remove resources again.

`target/launcher-dock-smoke-logs/results.json` records actual Dock pixels growing from y=552 to y=532, resetting on pointer leave, and launching the correct app from outside its unmagnified bounds. It also records live app catalog changes, real activation/fallback, terminal argv/cwd, Escape during a slow launcher D-Bus call and menu controls during a slow Dock activation. The optional Xfce terminal check uses the private X11 display; it does not establish native Wayland terminal compatibility.

`target/desktop-controls-smoke-logs/results.json` records actual settings changes and notification signals, including selection of the sixth action in a paged sample and default body activation. Releasing Super outside the nested host window no longer leaves a shortcut stuck. A silent settings backend still permits Escape to close promptly; failed readback renders an error. These are keyboard/protocol tests, not physical touchpad evidence. Development package staging includes all five binaries and the settings desktop entry.

`target/preferences-history-smoke-logs/results.json` records a real Settings save
of Tiling, Ultra quality and DND, a mode-`0600` snapshot and restoration in a fresh
session without rewriting that snapshot. Manual history browsing works while DND
suppresses automatic cards. Rapid Down/Enter selects an older live notification and invokes only
its owner's action; archived details remain inert and clearing history preserves
the other active notification. Dismissal and expiry archive close reasons 2 and
1. A new session restores preferences with an empty notification archive.
Escape returns to the list or closes its overlay; reopening proves the shell
stays running, and Enter on an empty list remains inert.
Malformed JSON keeps its original bytes and actual defaults while Settings shows
startup/save errors. Replacing the file with a directory causes a visible save
failure while the requested Stage Manager layout remains applied for that
session. These results establish session history behavior, not disk persistence
of notification text.

Other logs are retained in `target/smoke-logs`, `target/desktop-smoke-logs` and `target/session-smoke-logs`. A software GPU provides no evidence about target hardware frame time, DRM/KMS, VT switching or native power management. The PipeWire exact-pixel fixture verifies transport separately from the full compositor integration; neither substitutes for broader desktop application compatibility testing.

## Remaining work

1. Broaden client protocol, popup keyboard-grab, scaling, animation/Stage Manager UX and accessibility coverage. Validate the native libinput gesture path on hardware; winit still does not emit raw swipes.
2. Extend regional damage handling and benchmark a named GPU/driver target; test overlapping glass surfaces and broader color/alpha cases.
3. Validate and harden the experimental DRM/KMS plus libseat/libinput backend on target hardware, including device ownership, cursor orientation, VT recovery and mode/driver failures. This cloud machine exposes no `/dev/dri`, `/dev/input` or `/dev/tty0`.
4. Validate real desktop portal frontend/application compatibility. Extend capture through explicit negotiation/tests before adding other cursor modes, resize continuation, window sources, multioutput, HDR or zero-copy transport.
5. Validate multioutput, suspend/resume, VT switching and installation on target machines before calling this a native alpha desktop release.

Reusable cloud install/start instructions have been saved to the configuration draft. Publication of a new snapshot, a remote CI run and validation in a fresh task are separate operations and are not claimed here.

## Virtual display startup diagnostics

The reported GitHub Actions run timed out before Xvfb supplied a display number, before compositor startup. The old helper discarded server stderr, so that log does not establish the underlying cause. Startup now has a shared 30-second monotonic deadline, notices early process exit, reads the complete displayfd line, and requires a successful X11 setup before publishing `DISPLAY`. Failed startups stop their owned initialization processes and preserve stderr, elapsed time and exit status. Logs under `target/xvfb-logs` are uploaded even when CI fails early.

Independent checks on Ubuntu 24.04 with Xvfb `2:21.1.12-1ubuntu1.9` passed five sequential and four concurrent startups for both the original and updated helper. The updated helper also passed X11 setup and `xdpyinfo` checks. This does not reproduce or explain the original GitHub timeout; the next remote run is needed to establish its result.
