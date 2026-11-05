//! Authenticated compositor frames exported as a real PipeWire video source.
//!
//! Each stream has a fixed size. Resizing, source loss, cancellation, or a
//! PipeWire error tears down its node; a new portal session must select again.
use std::{
    cell::Cell,
    fmt,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use pipewire::{self as pw, properties::properties, spa};

const MAX_DIMENSION: u32 = 4096;
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const START_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Video is copied on a normal worker/main-loop thread, without RT_PROCESS.
/// Avoid unnecessary realtime portal/RTKit negotiation, including teardown
/// waiting for an unrelated desktop frontend. `support.dbus` also covers older
/// client.conf files (e.g. PipeWire 0.3.65) without the module.rt condition.
pub(crate) fn context_properties() -> pw::properties::PropertiesBox {
    properties! { "module.rt" => "false", "support.dbus" => "false" }
}

#[derive(Clone)]
pub struct CaptureConfig {
    pub socket: PathBuf,
    pub token: String,
    pub remote: Option<String>,
}

impl fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("socket", &self.socket)
            .field("token", &"<redacted>")
            .field("remote", &self.remote)
            .finish()
    }
}

impl CaptureConfig {
    fn validate(&self) -> io::Result<()> {
        if !self.socket.is_absolute()
            || self.token.len() != 64
            || !self.token.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.remote.as_ref().is_some_and(|name| name.contains('\0'))
        {
            return Err(invalid("invalid capture configuration"));
        }
        Ok(())
    }

    fn remote_properties(&self) -> Option<pw::properties::PropertiesBox> {
        self.remote.as_ref().map(|remote| {
            properties! {
                *pw::keys::REMOTE_NAME => remote.as_str(),
            }
        })
    }
}

/// Check both authenticated output availability and a real daemon roundtrip.
/// This handshake never subscribes to or requests compositor pixels.
pub fn probe(config: &CaptureConfig) -> io::Result<()> {
    config.validate()?;
    let stopping = AtomicBool::new(false);
    let mut socket = source_socket(config, &stopping)?;
    socket.write_all(format!("PROBE {}\n", config.token).as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut line = Vec::with_capacity(64);
    loop {
        let mut byte = [0];
        read_exact_until(&mut socket, &mut byte, &stopping, deadline)?;
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= 64 {
            return Err(invalid("invalid capture probe response"));
        }
        line.push(byte[0]);
    }
    let line = std::str::from_utf8(&line).map_err(|_| invalid("invalid capture probe response"))?;
    let mut fields = line.split(' ');
    if fields.next() != Some("AURACAP1") {
        return Err(invalid("capture authentication failed"));
    }
    let width = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let height = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    checked_size(width, height, width.saturating_mul(4), 0, false)?;
    if fields.next().is_some() {
        return Err(invalid("invalid capture probe response"));
    }

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context =
        pw::context::ContextRc::new(&mainloop, Some(context_properties())).map_err(pw_error)?;
    let core = context
        .connect_rc(config.remote_properties())
        .map_err(pw_error)?;
    let pending = core.sync(0).map_err(pw_error)?;
    let done = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let done_callback = done.clone();
    let failed_callback = failed.clone();
    let _listener = core
        .add_listener_local()
        .done(move |id, sequence| {
            if id == pw::core::PW_ID_CORE && sequence == pending {
                done_callback.set(true);
            }
        })
        .error(move |_, _, _, _| failed_callback.set(true))
        .register();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !done.get() && !failed.get() && Instant::now() < deadline {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(POLL_INTERVAL));
    }
    if done.get() && !failed.get() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "PipeWire daemon unavailable",
        ))
    }
}

pub struct CaptureStream {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    stopping: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl CaptureStream {
    /// Blocking startup for standalone consumers; portal callers use the
    /// cancellable form on their blocking executor.
    #[allow(dead_code)]
    pub fn start(config: CaptureConfig) -> io::Result<Self> {
        Self::start_with_cancel(config, Arc::new(AtomicBool::new(false)))
    }

    pub fn start_with_cancel(config: CaptureConfig, stopping: Arc<AtomicBool>) -> io::Result<Self> {
        config.validate()?;
        if stopping.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        let running = Arc::new(AtomicBool::new(false));
        let (ready, receiver) = mpsc::sync_channel(1);
        let worker_stop = stopping.clone();
        let worker_running = running.clone();
        thread::Builder::new()
            .name("aura-pipewire".into())
            .spawn(move || {
                let _lifetime = WorkerLifetime {
                    stopping: worker_stop.clone(),
                    running: worker_running.clone(),
                };
                if let Err(error) = run(config, worker_stop, worker_running, &ready) {
                    // Errors never include the capability token or source frame data.
                    tracing::warn!(%error, "screen capture stream stopped");
                    let _ = ready.try_send(Err(error));
                }
            })?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if stopping.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            if Instant::now() >= deadline {
                stopping.store(true, Ordering::Release);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "capture startup timed out",
                ));
            }
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(Ok((node_id, width, height))) => {
                    return Ok(Self {
                        node_id,
                        width,
                        height,
                        stopping,
                        running,
                    });
                }
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("capture worker exited during startup"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire) && !self.stopping.load(Ordering::Acquire)
    }

    /// Signal cleanup without blocking a D-Bus executor thread. The worker
    /// observes this within one loop poll, and source reads within 100ms.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.stop();
    }
}

struct WorkerLifetime {
    stopping: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.running.store(false, Ordering::Release);
    }
}

struct Frame {
    width: u32,
    height: u32,
    stride: u32,
    pixels: Vec<u8>,
}

type Ready = SyncSender<io::Result<(u32, u32, u32)>>;

fn run(
    config: CaptureConfig,
    stopping: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    ready: &Ready,
) -> io::Result<()> {
    let mut source = source_socket(&config, &stopping)?;
    if stopping.load(Ordering::Acquire) {
        return Err(cancelled());
    }
    source.write_all(format!("{}\n", config.token).as_bytes())?;
    let first = read_frame(&mut source, &stopping)?;
    let (width, height, stride) = (first.width, first.height, first.stride);
    let latest = Arc::new(Mutex::new(Arc::new(first)));
    let source_stop = stopping.clone();
    let source_latest = latest.clone();
    // Retain only the latest frame and the frame under construction, plus a
    // temporary Arc during processing. The callback never waits on IO.
    let _reader = thread::Builder::new()
        .name("aura-frame-reader".into())
        .spawn(move || {
            while !source_stop.load(Ordering::Acquire) {
                match read_frame(&mut source, &source_stop) {
                    Ok(frame) if (frame.width, frame.height) == (width, height) => {
                        match source_latest.lock() {
                            Ok(mut latest) => *latest = Arc::new(frame),
                            Err(_) => break,
                        }
                    }
                    Ok(_) => {
                        tracing::info!("capture output resized; closing fixed-size stream");
                        break;
                    }
                    Err(error) => {
                        if !source_stop.load(Ordering::Acquire) {
                            tracing::warn!(%error, "compositor capture source disconnected");
                        }
                        break;
                    }
                }
            }
            source_stop.store(true, Ordering::Release);
        })?;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context =
        pw::context::ContextRc::new(&mainloop, Some(context_properties())).map_err(pw_error)?;
    let core = context
        .connect_rc(config.remote_properties())
        .map_err(pw_error)?;
    let core_failed = Rc::new(Cell::new(false));
    let core_failed_callback = core_failed.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, _| core_failed_callback.set(true))
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "Aura screen capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => "aura-screen-capture",
            "node.description" => "Aura composed output",
            "node.always-process" => "true",
            "node.want-driver" => "true",
        },
    )
    .map_err(pw_error)?;
    let process_stop = stopping.clone();
    let format_stop = stopping.clone();
    let _stream_listener = stream
        .add_local_listener_with_user_data(latest)
        .state_changed(|_, _, old, new| tracing::debug!(?old, ?new, "capture PipeWire state"))
        .param_changed(move |stream, _, id, param| {
            if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            let mut format = spa::param::video::VideoInfoRaw::new();
            if format.parse(param.unwrap()).is_err()
                || format.format() != spa::param::video::VideoFormat::RGBA
                || format.size() != (spa::utils::Rectangle { width, height })
            {
                format_stop.store(true, Ordering::Release);
                return;
            }
            let values = buffer_params(width * height * 4, stride);
            if stream
                .update_params(&mut [spa::pod::Pod::from_bytes(&values).unwrap()])
                .is_err()
            {
                format_stop.store(true, Ordering::Release);
            }
        })
        .process(move |stream, latest| {
            if process_stop.load(Ordering::Acquire) {
                return;
            }
            let frame = match latest.try_lock() {
                Ok(frame) => frame.clone(),
                Err(_) => return,
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let length = frame.pixels.len();
            if let Some(bytes) = data.data().filter(|bytes| bytes.len() >= length) {
                bytes[..length].copy_from_slice(&frame.pixels);
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = frame.stride as i32;
                *chunk.size_mut() = length as u32;
            } else {
                *data.chunk_mut().size_mut() = 0;
                process_stop.store(true, Ordering::Release);
            }
        })
        .register()
        .map_err(pw_error)?;
    let values = format_params(width, height);
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [spa::pod::Pod::from_bytes(&values).unwrap()],
        )
        .map_err(pw_error)?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut announced = false;
    let mut last_process = Instant::now();
    while !stopping.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(POLL_INTERVAL));
        if core_failed.get() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "PipeWire connection lost",
            ));
        }
        match stream.state() {
            pw::stream::StreamState::Error(error) => {
                return Err(io::Error::other(format!("PipeWire stream: {error}")));
            }
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming if !announced => {
                let node_id = stream.node_id();
                if node_id != u32::MAX {
                    running.store(true, Ordering::Release);
                    ready
                        .send(Ok((node_id, width, height)))
                        .map_err(|_| cancelled())?;
                    announced = true;
                }
            }
            _ => {}
        }
        if !announced && Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PipeWire node registration timed out",
            ));
        }
        if stream.state() == pw::stream::StreamState::Streaming
            && stream.is_driving()
            && last_process.elapsed() >= FRAME_INTERVAL
        {
            stream.trigger_process().map_err(pw_error)?;
            last_process = Instant::now();
        }
    }
    let _ = stream.disconnect();
    Ok(())
}

fn format_params(width: u32, height: u32) -> Vec<u8> {
    serialize(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Id,
            spa::param::video::VideoFormat::RGBA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 }
        ),
    ))
}

fn buffer_params(size: u32, stride: u32) -> Vec<u8> {
    let property = |key, value| spa::pod::Property {
        key,
        flags: spa::pod::PropertyFlags::empty(),
        value: spa::pod::Value::Int(value),
    };
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            property(spa::sys::SPA_PARAM_BUFFERS_buffers, 8),
            property(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            property(spa::sys::SPA_PARAM_BUFFERS_size, size as i32),
            property(spa::sys::SPA_PARAM_BUFFERS_stride, stride as i32),
        ],
    })
}

fn serialize(object: spa::pod::Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .expect("valid fixed PipeWire parameter")
    .0
    .into_inner()
}

fn source_socket(config: &CaptureConfig, stopping: &AtomicBool) -> io::Result<UnixStream> {
    source_socket_until(config, stopping, Instant::now() + CONNECT_TIMEOUT)
}

fn source_socket_until(
    config: &CaptureConfig,
    stopping: &AtomicBool,
    deadline: Instant,
) -> io::Result<UnixStream> {
    use rustix::{
        io::Errno,
        net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType},
    };
    let address = SocketAddrUnix::new(&config.socket)?;
    let socket = loop {
        check_connect_deadline(stopping, deadline)?;
        let socket = UnixStream::from(net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?);
        match net::connect(&socket, &address) {
            Ok(()) => break socket,
            // Linux returns EAGAIN when the Unix listener backlog is full.
            // No connection is pending, so close this descriptor and retry.
            Err(Errno::AGAIN | Errno::INTR) => {
                drop(socket);
                thread::park_timeout(POLL_INTERVAL);
            }
            Err(Errno::INPROGRESS) => {
                loop {
                    check_connect_deadline(stopping, deadline)?;
                    if let Some(error) = socket.take_error()? {
                        return Err(error);
                    }
                    match socket.peer_addr() {
                        Ok(_) => break,
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::NotConnected | io::ErrorKind::WouldBlock
                            ) =>
                        {
                            thread::park_timeout(POLL_INTERVAL)
                        }
                        Err(error) => return Err(error),
                    }
                }
                break socket;
            }
            Err(error) => return Err(error.into()),
        }
    };
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    Ok(socket)
}

fn check_connect_deadline(stopping: &AtomicBool, deadline: Instant) -> io::Result<()> {
    if stopping.load(Ordering::Acquire) {
        return Err(cancelled());
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "compositor capture connection timed out",
        ));
    }
    Ok(())
}

fn read_frame(socket: &mut impl Read, stopping: &AtomicBool) -> io::Result<Frame> {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut header = [0u8; 40];
    read_exact_until(socket, &mut header, stopping, deadline)?;
    if &header[..8] != b"AURAFRM1" {
        return Err(invalid("invalid compositor frame magic"));
    }
    let u32_at = |start| u32::from_le_bytes(header[start..start + 4].try_into().unwrap());
    let (width, height, stride, len) = (u32_at(8), u32_at(12), u32_at(16), u32_at(28));
    let size = checked_size(width, height, stride, len, true)?;
    let mut pixels = vec![0; size];
    read_exact_until(socket, &mut pixels, stopping, deadline)?;
    Ok(Frame {
        width,
        height,
        stride,
        pixels,
    })
}

fn checked_size(
    width: u32,
    height: u32,
    stride: u32,
    length: u32,
    check_length: bool,
) -> io::Result<usize> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || stride != width * 4
        || (check_length && length != stride * height)
    {
        return Err(invalid("invalid compositor frame dimensions or length"));
    }
    Ok((stride * height) as usize)
}

fn read_exact_until(
    reader: &mut impl Read,
    mut bytes: &mut [u8],
    stopping: &AtomicBool,
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if stopping.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "compositor frame timed out",
            ));
        }
        match reader.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "compositor capture source closed",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "capture cancelled")
}
fn pw_error(error: pw::Error) -> io::Error {
    io::Error::other(format!("PipeWire: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(width: u32, height: u32, stride: u32, length: u32) -> Vec<u8> {
        let mut bytes = b"AURAFRM1".to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes.extend(stride.to_le_bytes());
        bytes.extend(7u64.to_le_bytes());
        bytes.extend(length.to_le_bytes());
        bytes.extend(123u64.to_le_bytes());
        bytes
    }

    #[test]
    fn preserves_exact_top_left_rgba_pixels() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 1, 2, 3, 255];
        let mut bytes = packet(2, 2, 8, 16);
        bytes.extend(pixels);
        let frame = read_frame(&mut bytes.as_slice(), &AtomicBool::new(false)).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (2, 2, 8));
        assert_eq!(frame.pixels, pixels);
    }

    #[test]
    fn rejects_unbounded_or_inconsistent_frames_before_allocation() {
        for (width, height, stride, length) in [
            (0, 1, 0, 0),
            (4097, 1, 16388, 16388),
            (2, 2, 7, 14),
            (2, 2, 8, 15),
            (u32::MAX, u32::MAX, 0, 0),
        ] {
            assert!(
                read_frame(
                    &mut packet(width, height, stride, length).as_slice(),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn cancellation_prevents_reading_pixels() {
        let result = read_frame(&mut io::empty(), &AtomicBool::new(true));
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn cancelled_start_does_not_connect_to_the_source() {
        let config = CaptureConfig {
            socket: "/unavailable/aura-capture".into(),
            token: "a".repeat(64),
            remote: None,
        };
        let result = CaptureStream::start_with_cancel(config, Arc::new(AtomicBool::new(true)));
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn configuration_debug_redacts_capability() {
        let config = CaptureConfig {
            socket: "/tmp/capture".into(),
            token: "a".repeat(64),
            remote: None,
        };
        assert!(!format!("{config:?}").contains(&config.token));
    }

    /// Local Unix socket integration is separate from I/O-free unit coverage.
    #[test]
    #[ignore = "requires local Unix sockets; run explicitly on a Linux host"]
    fn saturated_backlog_connect_is_bounded_and_cancellable() {
        use std::os::unix::net::UnixListener;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aura-connect-{}-{nonce}.sock", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let listener = UnixListener::bind(&path).unwrap();
        let _cleanup = Cleanup(path.clone());
        rustix::net::listen(&listener, 0).unwrap();
        let queued = UnixStream::from(
            rustix::net::socket_with(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::STREAM,
                rustix::net::SocketFlags::NONBLOCK,
                None,
            )
            .unwrap(),
        );
        rustix::net::connect(&queued, &rustix::net::SocketAddrUnix::new(&path).unwrap()).unwrap();
        let config = CaptureConfig {
            socket: path,
            token: "a".repeat(64),
            remote: None,
        };
        let start = Instant::now();
        let error = source_socket_until(
            &config,
            &AtomicBool::new(false),
            start + Duration::from_millis(120),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));

        let stopping = Arc::new(AtomicBool::new(false));
        let cancel = stopping.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            cancel.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let error =
            source_socket_until(&config, &stopping, start + Duration::from_secs(3)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(1));
        canceller.join().unwrap();
    }
}
