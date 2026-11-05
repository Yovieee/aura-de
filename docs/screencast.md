# ScreenCast scope and validation

Aura's ScreenCast backend exports frames from the running nested compositor into an actual PipeWire video source. The supported source is one monitor with a hidden cursor. The stream format is fixed-size SDR RGBA, up to 4096 pixels on each axis. Output resize, source loss or stream failure ends the stream; a fresh portal selection and consent are needed to start again.

GPU readback uses pixel-buffer objects and nonblocking fence polling, followed by an owned CPU frame transport to PipeWire. This is not a zero-copy or DMA-BUF capture path. HDR, cursor embedding/metadata, window-source capture, persistent permissions, multioutput and a native DRM/KMS session remain outside the supported scope.

## User and process lifecycle

Run `aura-session` from an existing graphical desktop with a reachable PipeWire daemon. The supervisor prepares the compositor's authenticated source and exports capture capabilities only after the source and daemon probes succeed. Standalone `aura-session --portal` does not inherit that capability and does not grant capture access. Internal capability values must not be copied to application environments or logs.

The raw compositor source requires both its per-session capability and Linux `SO_PEERCRED` matching the supervisor's PID and UID. Possessing the capability in another process is insufficient. Portal requests must come from the current owner of `org.freedesktop.portal.Desktop`, and request/session ownership is bound to that unique bus sender and its supplied application ID. This is a bus-owner trust boundary: the backend does not independently authenticate arbitrary native application identities or attest the frontend executable. The standard frontend remains responsible for the application identity it supplies.

A ScreenCast request opens a readable consent overlay displaying the requesting application. Deny is the default selection; Allow requires an explicit user action. Cancellation removes the overlay without subscribing to frames. The shell displays active sharing and a Stop action. Stop, explicit session Close and frontend disconnect must remove both the compositor subscription and PipeWire node. After revocation, a new request needs new consent.

The test frontend exercises the backend method/session contract on a private bus. This does not establish compatibility with every application or desktop portal frontend; broader application compatibility still needs testing on a desktop host. Installing a `.portal` descriptor is distinct from running a supervised capture source.

## Separate checks for separate claims

Install testing dependencies with `scripts/install-deps.sh --testing` or the Debian 13 rootless equivalent. This includes libclang and PipeWire development headers for binding generation, plus daemon/modules/tools for runtime checks. Build actual executables first:

```bash
cargo build --workspace --bins --examples --locked
```

The capture checks each cover a different boundary:

| Command | Evidence |
| --- | --- |
| `python3 crates/aura-session/tests/pipewire_roundtrip.py` | A deterministic authenticated 2×2 transport fixture produces real PipeWire buffers with exact RGBA bytes, then removes the node; it does not read the desktop |
| `python3 scripts/screencast_smoke.py --no-build` | Real supervised compositor readback, default consent denial, cancellation, approved PipeWire frames, changed scene pixels, shell Stop, fresh consent, explicit close and frontend-disconnect cleanup |
| `python3 scripts/window_render_smoke.py --no-build` | Idle rendering/capture/blur counters, retained-cache invalidation and real server-side window-control input on an isolated Xvfb display |

Run the asynchronous readback test explicitly, because ordinary CPU tests intentionally ignore driver-dependent cases:

```bash
LIBGL_ALWAYS_SOFTWARE=1 EGL_PLATFORM=surfaceless \
  cargo test -p aura-compositor --bin aura-compositor \
  capture::tests::gles_pbo_capture_preserves_pack_state_and_reads_composed_rows \
  --locked -- --ignored --exact --nocapture

cargo test -p aura-render --test gpu --locked -- --ignored --nocapture
cargo test -p aura-render --test gles --locked -- --ignored --nocapture

cargo test -p aura-session \
  capture::tests::saturated_backlog_connect_is_bounded_and_cancellable \
  --locked -- --ignored --exact
```

The PBO test validates composed row order and pack-state restoration through a real EGL/GLES driver, including revocation with queued GPU work. The render tests separately validate blur/refraction output and shader compilation. The Unix-socket backlog test verifies that source connection timeout and cancellation stay bounded when the listener queue is saturated. A passing software driver run does not measure hardware performance or establish native session support.

`window_render_smoke.py` writes logs and counters to `target/window-render-smoke-logs`; `screencast_smoke.py` writes logs and frame evidence to `target/screencast-smoke-logs`. Both use isolated displays/buses; the ScreenCast check additionally creates its own PipeWire daemon. The CI workflow explicitly executes the ignored driver/private-bus tests and uploads these artifacts. Current observed results, including incomplete gates, are recorded in [implementation status](implementation-status.md).
