# Implementation and verification status

Updated 2026-11-05. Aura is a **nested, single-output development desktop**. The original `0.1.0` labels described design slices; the results below are actual current-instance checks, with hardware and compatibility limits stated separately.

## Implemented behavior and limits

| Area | Implemented and observed | Remaining limits |
| --- | --- | --- |
| Build baseline | Rust 1.99.1 pin, tracked lockfile, standalone core tests, workspace check/test/strict Clippy and optional native-dependency feature check | The CI workflow is configured; no remote CI run is claimed |
| Pure core | Focus/visibility/restore/close invariants, bounded geometry, analytical stall-safe spring, finite/range material validation and fullscreen geometry policy; 22 standalone tests passed | Layout/physics remain independent of I/O and GPU |
| Nested compositor | Real Wayland socket, SHM/xdg-shell, output/seat, keyboard/pointer, move/resize requests, clipboard focus, maximize, map/unmap/remap, popups, fullscreen and negotiated server-side decorations | One output; hardware DMA-BUF import, XWayland and comprehensive client-conformance coverage remain unfinished |
| Window lifecycle and chrome | Two real clients; focus/minimize/restore; repeated maximize; NULL unmap/new configure/remap; popup buffers/frame callbacks; fullscreen transitions and initial pre-map fullscreen with both client/server decorations; titlebar drag and traffic-light controls | Stage Manager is a layout mode, not full live-thumbnail/grouping UX; position springs render, while size transitions are not resampled animations |
| Shell and launcher | Real layer-shell menu bar/Dock/overlay, exclusive reservation release/remap, live XDG application catalog, terminal entry launch, bounded D-Bus activation/Exec fallback, shell-free argv and magnified Dock app targets | Fixed bitmap font, text app tiles, one default output and no desktop settings UI |
| D-Bus services | `org.aura.Shell1` controls compositor state; freedesktop notification replacement/close/expiry/DND tested on a private bus | Notification actions, advanced desktop controls and broader application compatibility remain future work |
| Glass and idle rendering | Actual wgpu/GLES blur/refraction; compositor-owned glass; retained scene/blur textures; unchanged-frame suppression and cache invalidation verified through counters | Dirty outputs redraw full frames. Regional damage, HDR/color management and hardware performance are not established; active capture deliberately requests periodic frames |
| Session supervision | Actual compositor/shell startup, Dock pointer input, launcher keyboard/Escape, desktop-entry app execution, SIGTERM/reap and socket cleanup tested | Nested session only; no bare-metal display-manager login, VT switch, hotplug or suspend/resume validation |
| Portal ScreenCast | Real composed pixels through asynchronous PBO readback and PipeWire; default-deny consent, sharing indicator/Stop above fullscreen, request cancellation, fresh consent, session Close and frontend-disconnect cleanup passed | One monitor, hidden cursor, fixed-size SDR RGBA up to 4096 pixels per axis. Resize/source loss ends the stream. No persistent permission, zero-copy, HDR or multioutput claim |
| Capture access boundary | Raw source checks a per-session capability plus kernel peer PID/UID for the supervisor. Portal requests check the current standard frontend bus-name owner and bind session ownership to its unique sender/application ID | Application ID is supplied by the trusted frontend; the backend does not independently authenticate arbitrary native app identities or attest the frontend executable |
| Direct scanout | Conservative eligibility model tests require coverage, buffer/plane/output constraints and atomic-test evidence | No active DRM/KMS scanout implementation |
| Packaging and setup | Development staging with licenses and opt-in descriptors; repeatable signed Debian rootless setup and native dependency activation | Development packaging is not a native desktop release or a fresh cloud snapshot |

## Current validation results

Checks ran on Debian 13, Rust 1.99.1, Mesa 25.0.7 llvmpipe and PipeWire 1.4.2. Rootless setup extracted **85 SHA-256-verified Debian packages**; a clean activation resolved libclang, gdbus, PipeWire and native dependency metadata using the host-overlay sysroot. This prepares compilation/runtime dependencies and does not establish hardware support.

| Command or check | Result |
| --- | --- |
| `python3 scripts/verify_workspace.py` | Passed structural checks; no mathematical/runtime claim is inferred from this script |
| `cargo fmt --all -- --check` | Passed |
| `cargo check --workspace --all-targets --all-features --locked` | Passed; optional native feature means dependency compilation, not a hardware backend |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| `cargo test -p aura-core --locked` | 22 passed, 0 failed |
| `cargo test --workspace --locked` | **78 passed, 0 failed, 6 intentionally ignored**; all six ignored tests were explicitly executed below. Zero-test binaries/doc-test runs are not counted as behavioral evidence |
| `cargo test -p aura-render --test gpu --locked -- --ignored --nocapture` | 1 passed; rendered pixels, blur/sharp comparison, clipping/premultiplication, opaque fallback and resize |
| `cargo test -p aura-render --test gles --locked -- --ignored --nocapture` | 1 passed; canonical GLES3 and Smithay shader variants compiled/linked through an actual EGL driver |
| Explicit compositor `capture::tests::gles_pbo_capture_preserves_pack_state_and_reads_composed_rows` with `--ignored --exact` | 1 passed; real asynchronous PBO readback, composed row order, pack-state preservation and revocation of queued work |
| Private-bus `portal_dbus_lifecycle` and `portal_dbus_consent_denial_and_request_cancellation` with `--ignored` | 2 passed; request/session ownership, capability behavior, consent denial and cancellation |
| Session `capture::tests::saturated_backlog_connect_is_bounded_and_cancellable` with `--ignored --exact` | 1 passed; actual saturated Unix listener queue, bounded timeout and cancellation |
| `dbus-run-session -- cargo run -p aura-shell --example dbus_smoke --locked` | Passed actual notification and shell D-Bus checks |
| `python3 scripts/smoke_nested.py --no-build --software` | Passed real SHM configure/commit/frame smoke; silent-protocol timeout was also checked |
| `python3 scripts/integration_desktop.py --no-build --software` | Passed layer unmap/remap reservation, two-client lifecycle, maximize/remap/popup, focus/minimize/restore, layout/quality and socket cleanup |
| `python3 scripts/session_smoke.py --no-build` | Passed real supervised startup, isolated pointer/keyboard input, Dock/launcher, actual app execution, shutdown/reap and socket cleanup |
| `python3 scripts/launcher_dock_smoke.py --no-build --terminal-integration` | Passed live install/edit/remove while open, Dock catalog refresh, literal terminal argv/cwd through a fixture and actual Xfce terminal, real D-Bus Activate/fallback, responsive Escape during slow activation, magnified pixel/click targets and window restore |
| `python3 scripts/window_render_smoke.py --no-build` | Passed idle/cache counters, server-chrome minimize/restore, 64 px titlebar drag and close |
| `python3 crates/aura-session/tests/pipewire_roundtrip.py` | Passed 10 real PipeWire frames with exact 2×2 RGBA bytes, authenticated no-frame probe separation and node cleanup |
| `python3 scripts/screencast_smoke.py --no-build` | Passed actual consent, compositor pixels over PipeWire, changed-scene checksums, Stop above fullscreen, fresh consent, explicit close, cancellation and disconnect cleanup |

The six explicit ignored-test passes comprise three GPU/PBO tests, two private-bus tests and one Unix-backlog test. They remain ignored in the default command because they require drivers, private buses or real sockets. CI runs them explicitly, builds real capture examples, preserves pipeline exit status and uploads artifacts. See [ScreenCast validation](screencast.md) for exact driver/socket commands.

## Retained runtime evidence

`target/window-render-smoke-logs/results.json` records stable render/capture/blur counters over 300 ms of idle time. A foreground-only shell update redraws without recapturing or reblurring the scene; changing quality reruns blur while reusing the capture. Real input minimizes/restores a client, drags its titlebar 64 pixels left and closes it.

`target/screencast-smoke-logs/results.json` records default denial and request cancellation with no subscription, followed by a real 1024×640 PipeWire stream. Each consumer verifies ten frames; adding a real client changes the frame checksum. Stop remains usable above a fullscreen client and removes the subscription/node. Fresh consent starts another stream; explicit close and frontend disconnect remove resources again.

`target/launcher-dock-smoke-logs/results.json` records actual Dock pixels growing from y=552 to y=532, resetting on pointer leave, and launching the correct app from outside its unmagnified bounds. It also records live app catalog changes, real activation/fallback, terminal argv/cwd, Escape during a slow launcher D-Bus call and menu controls during a slow Dock activation. The optional Xfce terminal check uses the private X11 display; it does not establish native Wayland terminal compatibility.

Other logs are retained in `target/smoke-logs`, `target/desktop-smoke-logs` and `target/session-smoke-logs`. A software GPU provides no evidence about target hardware frame time, DRM/KMS, VT switching or native power management. The PipeWire exact-pixel fixture verifies transport separately from the full compositor integration; neither substitutes for broader desktop application compatibility testing.

## Remaining work

1. Broaden client protocol, popup keyboard-grab, scaling, animation/Stage Manager UX and accessibility coverage.
2. Extend regional damage handling and benchmark a named GPU/driver target; test overlapping glass surfaces and broader color/alpha cases.
3. Implement and validate DRM/KMS plus libseat/logind/libinput on hardware. This cloud machine exposes no `/dev/dri`.
4. Validate real desktop portal frontend/application compatibility. Extend capture through explicit negotiation/tests before adding other cursor modes, resize continuation, window sources, multioutput, HDR or zero-copy transport.
5. Validate multioutput, suspend/resume, VT switching and installation on target machines before calling this a native alpha desktop release.

Reusable cloud install/start instructions have been saved to the configuration draft. Publication of a new snapshot, a remote CI run and validation in a fresh task are separate operations and are not claimed here.
