# Implementation and verification status

Updated 2026-11-04. The original `0.1.0` labels described design slices. This implementation establishes a **nested, single-output development desktop**, with the remaining hardware and capture work listed separately.

## Acceptance gates

| Area | Implemented and observed | Remaining limits |
| --- | --- | --- |
| Build baseline | Rust 1.99.1 pin, tracked lockfile, standalone core tests, workspace check/test/strict Clippy and optional native-dependency feature check | CI configuration is committed as source only; no remote CI run is claimed |
| Pure core | Focus/visibility/restore/close invariants, bounded geometry, analytical stall-safe spring, finite/range material validation; 19 tests passed | Layout/physics remain independent of I/O and GPU |
| Nested compositor | Real Wayland socket, SHM/xdg-shell, output/seat, keyboard/pointer, move/resize requests, clipboard focus, maximize, map/unmap/remap and popups | One output; hardware DMA-BUF import, XWayland, server-side decorations, fullscreen policy and comprehensive protocol conformance remain unfinished |
| Window lifecycle | Two real clients; focus/minimize/restore; repeated maximize; NULL unmap/new configure/remap; popup SHM buffers/frame callbacks; close and disconnect cleanup | Stage Manager is a layout mode, not full live-thumbnail/grouping UX; spring position changes are rendered, size transitions are not resampled animations |
| Shell and launcher | Real layer-shell menu bar/Dock/overlay, NULL unmap releases exclusive reservation and remap restores it; real XDG entry index and shell-free app execution | Fixed bitmap font, one default output, no terminal-only or D-Bus-activation-only entries, no live entry-index reload |
| D-Bus services | `org.aura.Shell1` controls actual compositor state; freedesktop notification replacement/close/expiry/DND tested on a private bus | Notification action handling, advanced desktop controls and broader compatibility remain future work |
| Glass GPU engine | wgpu real blur/refraction/readback tests; GLES canonical/Smithay shader compilation; compositor captures actual scene, executes blur and composites glass behind authorized shell namespaces | Compositor path is SDR RGBA8 and redraws full frames; cached damage/idle scheduling, HDR/color management and hardware performance targets are not complete |
| Session supervision | Actual compositor/shell startup, Dock pointer input, launcher keyboard/Escape, real desktop-entry app execution, SIGTERM/reap and socket cleanup tested | Nested session only; no bare-metal display-manager login, VT switch, hotplug or suspend/resume validation |
| Portal ScreenCast | Standard D-Bus method/session/request lifecycle, ownership and cleanup tested; reports zero capture capabilities and explicit NotSupported | No compositor capture transport, consent UI or PipeWire node delivering frames; screen sharing is not implemented |
| Direct scanout | Conservative eligibility model tests require coverage, buffer/plane/output constraints and atomic-test evidence | No active DRM/KMS scanout implementation |
| Packaging | Development staging script, binaries, licenses and opt-in preview/portal descriptors | A nested development package is not a native desktop release |

## Validation record

Commands run on Debian 13, with verified user-local native dependencies, Rust 1.99.1 and Mesa 25.0.7 llvmpipe software rendering:

- `python3 scripts/verify_workspace.py`: passed structural checks; it does not claim mathematical/runtime verification.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`: passed.
- `cargo check --workspace --all-targets --all-features --locked --offline`: passed; native feature here means optional dependency compilation, not a working hardware backend.
- `cargo test --workspace --locked --offline`: 37 tests passed, 0 failed; 3 driver/private-bus tests intentionally ignored in the default command and explicitly executed below. Zero-test binaries/doc-test runs are not counted as validation.
- `cargo test -p aura-render --test gpu --locked --offline -- --ignored --nocapture`: 1 passed on llvmpipe; verifies rendered pixels, blur versus sharp, clipping/premultiplication, opaque fallback and resize.
- `cargo test -p aura-render --test gles --locked --offline -- --ignored --nocapture`: 1 passed; compiles/links canonical GLES3 programs and Smithay define variants through a real EGL driver.
- `dbus-run-session -- cargo test -p aura-session portal_dbus_lifecycle --locked --offline -- --ignored`: 1 passed; this proves lifecycle and honest unsupported capture behavior, not screen sharing.
- `dbus-run-session -- cargo run -p aura-shell --example dbus_smoke --locked --offline`: passed actual notification and D-Bus behavior checks.
- `python3 scripts/smoke_nested.py --no-build --software`: passed real SHM configure/commit/frame smoke. A silent protocol server also correctly fails the client timeout.
- `python3 scripts/integration_desktop.py --no-build --software`: passed layer NULL-unmap/remap reservation checks, two real clients, maximize idempotence, client remap/popup, focus/minimize/restore, layout/quality, shell/launcher frame callbacks, close/disconnect and socket cleanup.
- `python3 scripts/session_smoke.py --no-build`: passed real supervised startup, synthetic pointer/keyboard input restricted to a private Xvfb, Dock opening launcher, Escape closing it, actual XDG app execution, process shutdown and socket cleanup.

Graphical test logs are retained in `target/smoke-logs`, `target/desktop-smoke-logs` and `target/session-smoke-logs`. A software GPU is useful evidence for this nested path; it provides no evidence about target hardware frame time, DRM/KMS, VT switching or native power management.

## Next required work

1. Finish nested protocol coverage and desktop ergonomics: fullscreen/decorations, popup keyboard-grab coverage, output scaling, complete animation/Stage Manager UX and accessibility.
2. Implement damage/idle scheduling and benchmark a named GPU/driver target; validate blur ordering for overlapping surfaces and broader color/alpha cases.
3. Implement and validate DRM/KMS + libseat/logind/libinput on a machine exposing real GPU/seat devices. This cloud machine exposes no `/dev/dri`.
4. Implement real compositor-to-PipeWire capture plus consent and teardown; then enable portal capture capabilities and verify frames with a real portal consumer.
5. Validate multioutput, suspend/resume, VT switching and packaging before calling this a native alpha desktop release.

The development environment and code are reviewable now. Publication of the cloud snapshot, a remote CI run, and validation in a fresh task are separate operations and are not claimed here.
