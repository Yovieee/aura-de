//! Authenticated, bounded readback of the composed output.
//!
//! Frame callbacks only enqueue GLES pixel-pack commands and poll signaled
//! fences. The worker owns the Unix socket, authentication, row conversion and
//! all client I/O. Both the supervisor PID and its capability must authenticate
//! before probing output information or subscribing.
use std::{
    env, fmt, fs, io,
    io::{Read, Write},
    os::unix::{fs::MetadataExt, fs::PermissionsExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use smithay::{
    backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, ffi},
    reexports::rustix,
    utils::{Physical, Size},
};

const MAX_DIMENSION: i32 = 4096;
const RING_SIZE: usize = 3;
const MAX_CLIENTS: usize = 4;
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
const WRITE_BUDGET: usize = 4 * 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(3);
const MAGIC: &[u8; 8] = b"AURAFRM1";
const HEADER_LEN: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Gles(#[from] GlesError),
    #[error("capture transport: {0}")]
    Io(#[from] io::Error),
    #[error("capture requires AURA_CAPTURE_SOCKET, AURA_CAPTURE_TOKEN and AURA_CAPTURE_OWNER_PID")]
    IncompleteConfig,
    #[error("capture token must contain exactly 64 hexadecimal characters")]
    InvalidToken,
    #[error("capture owner PID must be a positive Linux process ID")]
    InvalidOwnerPid,
    #[error("capture socket must be an absolute path")]
    InvalidSocket,
    #[error("capture dimensions must be between 1 and 4096 pixels")]
    InvalidSize,
    #[error("asynchronous capture requires GLES 3 pixel-pack buffers and fences")]
    Unsupported,
    #[error("GLES capture readback failed (error {0:#x})")]
    Readback(u32),
}

pub struct CaptureConfig {
    pub socket: PathBuf,
    pub token: String,
    pub owner_pid: u32,
    pub size: Size<i32, Physical>,
}

impl fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("socket", &self.socket)
            .field("token", &"<redacted>")
            .field("owner_pid", &self.owner_pid)
            .field("size", &self.size)
            .finish()
    }
}

impl CaptureConfig {
    /// Absence disables capture; a partial configuration is an explicit error.
    pub fn from_env(size: Size<i32, Physical>) -> Result<Option<Self>, CaptureError> {
        match (
            env::var_os("AURA_CAPTURE_SOCKET"),
            env::var_os("AURA_CAPTURE_TOKEN"),
            env::var_os("AURA_CAPTURE_OWNER_PID"),
        ) {
            (None, None, None) => Ok(None),
            (Some(socket), Some(token), Some(owner_pid)) => {
                let token = token
                    .into_string()
                    .map_err(|_| CaptureError::InvalidToken)?;
                let owner_pid = owner_pid
                    .into_string()
                    .map_err(|_| CaptureError::InvalidOwnerPid)?
                    .parse()
                    .map_err(|_| CaptureError::InvalidOwnerPid)?;
                let config = Self {
                    socket: socket.into(),
                    token,
                    owner_pid,
                    size,
                };
                config.validate()?;
                Ok(Some(config))
            }
            _ => Err(CaptureError::IncompleteConfig),
        }
    }

    fn validate(&self) -> Result<(), CaptureError> {
        byte_len(self.size)?;
        if self.owner_pid == 0 || self.owner_pid > i32::MAX as u32 {
            return Err(CaptureError::InvalidOwnerPid);
        }
        if !self.socket.is_absolute() {
            return Err(CaptureError::InvalidSocket);
        }
        if self.token.len() != 64 || !self.token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CaptureError::InvalidToken);
        }
        Ok(())
    }
}

struct Pending {
    epoch: u64,
    fence: ffi::types::GLsync,
    sequence: u64,
    timestamp_ns: u64,
}

#[derive(Default)]
struct Slot {
    buffer: u32,
    pending: Option<Pending>,
}

/// Single-slot mailbox: the renderer never waits for the worker or a reader.
#[derive(Default)]
struct FrameMailbox(Mutex<Option<CapturedFrame>>);

impl FrameMailbox {
    fn publish(&self, frame: CapturedFrame) {
        if let Ok(mut latest) = self.0.try_lock() {
            *latest = Some(frame);
        }
    }

    fn take(&self) -> Option<CapturedFrame> {
        self.0.lock().ok()?.take()
    }

    fn discard_revoked(&self, epoch: u64) {
        if let Ok(mut latest) = self.0.try_lock()
            && latest.as_ref().is_some_and(|frame| frame.epoch != epoch)
        {
            latest.take();
        }
    }
}

/// The worker alone changes `count`. A new epoch hides the old count
/// immediately, without racing decrements while the worker closes clients.
#[derive(Default)]
struct Subscribers {
    epoch: AtomicU64,
    applied_epoch: AtomicU64,
    count: AtomicUsize,
}

impl Subscribers {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn count(&self) -> usize {
        let epoch = self.epoch();
        if self.applied_epoch.load(Ordering::Acquire) != epoch {
            return 0;
        }
        let count = self.count.load(Ordering::Acquire);
        if self.epoch() == epoch { count } else { 0 }
    }
}

struct CapturedFrame {
    epoch: u64,
    size: Size<i32, Physical>,
    sequence: u64,
    timestamp_ns: u64,
    pixels: Vec<u8>,
}

/// Keep this on the renderer thread, and call `shutdown` before dropping the
/// renderer so its PBOs are deleted with their owning GL context current.
pub struct CaptureProducer {
    slots: [Slot; RING_SIZE],
    allocated_size: Option<Size<i32, Physical>>,
    mailbox: Arc<FrameMailbox>,
    worker: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    subscribers: Arc<Subscribers>,
    output_size: Arc<AtomicUsize>,
    next_frame: Option<Instant>,
    sequence: u64,
    last_completed_sequence: Option<u64>,
    clock_start: Instant,
    unix_start_ns: u64,
}

impl CaptureProducer {
    /// Called during compositor startup, before entering Calloop. Socket setup
    /// runs on the worker; only startup waits for its result.
    pub fn new(renderer: &mut GlesRenderer, config: CaptureConfig) -> Result<Self, CaptureError> {
        config.validate()?;
        let supported = renderer.with_context(|gl| {
            // SAFETY: Smithay made this renderer's context current. GLES 3 is
            // necessary even on drivers exporting the functions as extensions.
            unsafe {
                let mut major = 0;
                gl.GetIntegerv(ffi::MAJOR_VERSION, &mut major);
                let error = gl.GetError();
                major >= 3
                    && error == ffi::NO_ERROR
                    && gl.MapBufferRange.is_loaded()
                    && gl.UnmapBuffer.is_loaded()
                    && gl.FenceSync.is_loaded()
                    && gl.ClientWaitSync.is_loaded()
                    && gl.DeleteSync.is_loaded()
            }
        })?;
        if !supported {
            return Err(CaptureError::Unsupported);
        }
        let subscribers = Arc::new(Subscribers::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let output_size = Arc::new(AtomicUsize::new(pack_size(config.size)));
        let mailbox = Arc::new(FrameMailbox::default());
        let worker_mailbox = mailbox.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_subscribers = subscribers.clone();
        let worker_stopping = stopping.clone();
        let worker_size = output_size.clone();
        let worker = thread::Builder::new()
            .name("aura-capture".into())
            .spawn(move || match SocketOwner::bind(&config.socket) {
                Ok(socket) => {
                    if ready_tx.send(Ok(())).is_ok() {
                        serve(
                            socket,
                            config.token,
                            worker_mailbox,
                            worker_subscribers,
                            worker_stopping,
                            worker_size,
                            config.owner_pid,
                        );
                    }
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error.into());
            }
            Err(_) => {
                let _ = worker.join();
                return Err(io::Error::other("capture worker stopped during startup").into());
            }
        }
        Ok(Self {
            slots: std::array::from_fn(|_| Slot::default()),
            allocated_size: None,
            mailbox,
            worker: Some(worker),
            stopping,
            subscribers,
            output_size,
            next_frame: None,
            sequence: 0,
            last_completed_sequence: None,
            clock_start: Instant::now(),
            unix_start_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(u64::MAX as u128) as u64,
        })
    }

    pub fn wants_frame(&self, now: Instant) -> bool {
        self.subscriber_count() > 0
            && !self.stopping.load(Ordering::Acquire)
            && self.next_frame.is_none_or(|deadline| now >= deadline)
    }

    /// Authenticated active consumers; revoked connections disappear from
    /// this count immediately, before the worker closes their sockets.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.count()
    }

    /// Revoke every current connection without waiting for socket I/O or GL.
    /// The worker closes them asynchronously; the listener and capability stay
    /// available for a subsequent consent flow. Epoch checks discard already
    /// queued CPU frames and PBO completions from the previous sharing session.
    pub fn revoke(&mut self) {
        let epoch = self
            .subscribers
            .epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.mailbox.discard_revoked(epoch);
        self.next_frame = None;
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }

    /// Whether a GPU readback fence still needs nonblocking polling.
    pub fn has_pending(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending.is_some())
    }

    /// Also update this when an idle output is resized so probes stay accurate.
    pub fn set_output_size(&self, size: Size<i32, Physical>) -> Result<(), CaptureError> {
        byte_len(size)?;
        self.output_size.store(pack_size(size), Ordering::Release);
        Ok(())
    }

    /// Called after composing the final output, before `Frame::finish`. The
    /// frame keeps the correct EGL surface and framebuffer current. Full rings
    /// drop a frame instead of waiting for the GPU or another process.
    pub fn enqueue(
        &mut self,
        frame: &mut GlesFrame<'_, '_>,
        size: Size<i32, Physical>,
    ) -> Result<bool, CaptureError> {
        let len = byte_len(size)?;
        self.set_output_size(size)?;
        if !self.wants_frame(Instant::now()) {
            return Ok(false);
        }
        let resized = self.allocated_size != Some(size);
        let sequence = self.sequence;
        let epoch = self.subscribers.epoch();
        let timestamp_ns = self
            .unix_start_ns
            .saturating_add(self.clock_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        let queued = frame.with_context(|gl| {
            // SAFETY: the frame owns the current context and output FBO. PBO
            // binding and all pack state are restored before returning.
            unsafe {
                let state = PackState::save(gl);
                if resized {
                    delete_slots(gl, &mut self.slots);
                    for slot in &mut self.slots {
                        gl.GenBuffers(1, &mut slot.buffer);
                        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, slot.buffer);
                        gl.BufferData(
                            ffi::PIXEL_PACK_BUFFER,
                            len as isize,
                            ptr::null(),
                            ffi::STREAM_READ,
                        );
                    }
                    self.allocated_size = Some(size);
                }
                let Some(slot) = self.slots.iter_mut().find(|slot| slot.pending.is_none()) else {
                    state.restore(gl);
                    return Ok(false);
                };
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, slot.buffer);
                gl.PixelStorei(ffi::PACK_ALIGNMENT, 1);
                gl.PixelStorei(ffi::PACK_ROW_LENGTH, 0);
                gl.PixelStorei(ffi::PACK_SKIP_ROWS, 0);
                gl.PixelStorei(ffi::PACK_SKIP_PIXELS, 0);
                gl.ReadPixels(
                    0,
                    0,
                    size.w,
                    size.h,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    ptr::null_mut(),
                );
                let fence = gl.FenceSync(ffi::SYNC_GPU_COMMANDS_COMPLETE, 0);
                let error = gl.GetError();
                if error != ffi::NO_ERROR || fence.is_null() {
                    if !fence.is_null() {
                        gl.DeleteSync(fence);
                    }
                    state.restore(gl);
                    return Err(CaptureError::Readback(error));
                }
                gl.Flush();
                slot.pending = Some(Pending {
                    epoch,
                    fence,
                    sequence,
                    timestamp_ns,
                });
                state.restore(gl);
                Ok(true)
            }
        })??;
        if queued {
            self.sequence = self.sequence.wrapping_add(1);
            self.next_frame = Some(advance_capture_deadline(self.next_frame, Instant::now()));
        }
        Ok(queued)
    }

    /// Poll every compositor tick, including ticks without scene damage.
    /// `ClientWaitSync(..., 0, 0)` never waits; only signaled PBOs are mapped.
    pub fn poll(&mut self, renderer: &mut GlesRenderer) -> Result<(), CaptureError> {
        let Some(size) = self.allocated_size else {
            return Ok(());
        };
        let len = byte_len(size)?;
        let subscribed = self.subscriber_count() > 0;
        let epoch = self.subscribers.epoch();
        renderer.with_context(|gl| {
            // SAFETY: resources belong to the current renderer. Each buffer is
            // mapped only after its GPU fence has signaled and unmapped before
            // returning. Copying to a bounded CPU queue cannot wait on a peer.
            unsafe {
                let mut previous = 0;
                gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut previous);
                let mut newest = None::<(usize, Pending)>;
                for (index, slot) in self.slots.iter_mut().enumerate() {
                    let Some(pending) = slot.pending.as_ref() else {
                        continue;
                    };
                    let status = gl.ClientWaitSync(pending.fence, 0, 0);
                    if status == ffi::TIMEOUT_EXPIRED {
                        continue;
                    }
                    let pending = slot.pending.take().expect("pending fence was checked");
                    gl.DeleteSync(pending.fence);
                    if status == ffi::WAIT_FAILED {
                        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, previous as u32);
                        return Err(CaptureError::Readback(gl.GetError()));
                    }
                    if subscribed
                        && pending.epoch == epoch
                        && self
                            .last_completed_sequence
                            .is_none_or(|sequence| pending.sequence > sequence)
                        && newest
                            .as_ref()
                            .is_none_or(|(_, ready)| pending.sequence > ready.sequence)
                    {
                        newest = Some((index, pending));
                    }
                }
                if let Some((index, pending)) = newest {
                    self.last_completed_sequence = Some(pending.sequence);
                    gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.slots[index].buffer);
                    let mapped = gl.MapBufferRange(
                        ffi::PIXEL_PACK_BUFFER,
                        0,
                        len as isize,
                        ffi::MAP_READ_BIT,
                    );
                    if mapped.is_null() {
                        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, previous as u32);
                        return Err(CaptureError::Readback(gl.GetError()));
                    }
                    let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), len).to_vec();
                    let valid = gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER) == ffi::TRUE;
                    if !valid {
                        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, previous as u32);
                        return Err(CaptureError::Readback(gl.GetError()));
                    }
                    self.mailbox.publish(CapturedFrame {
                        epoch: pending.epoch,
                        size,
                        sequence: pending.sequence,
                        timestamp_ns: pending.timestamp_ns,
                        pixels,
                    });
                    if let Some(worker) = &self.worker {
                        worker.thread().unpark();
                    }
                }
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, previous as u32);
                Ok(())
            }
        })?
    }

    /// Explicit GL teardown while the renderer is alive. Deleting in-flight
    /// PBOs is deferred by GLES, so shutdown and resize never wait on a fence.
    pub fn shutdown(&mut self, renderer: &mut GlesRenderer) -> Result<(), CaptureError> {
        self.stop_worker();
        renderer.with_context(|gl| {
            // SAFETY: this is the same context that allocated the slots.
            unsafe {
                delete_slots(gl, &mut self.slots);
            }
        })?;
        self.allocated_size = None;
        Ok(())
    }

    fn stop_worker(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

impl Drop for CaptureProducer {
    fn drop(&mut self) {
        self.stop_worker();
        // GL objects require shutdown(renderer); if renderer initialization
        // unwinds, context destruction still releases its remaining objects.
    }
}

/// Preserve the phase across coarse compositor ticks. After a long stall,
/// schedule from now rather than requesting a burst to catch up missed frames.
fn advance_capture_deadline(previous: Option<Instant>, now: Instant) -> Instant {
    previous
        .map(|deadline| deadline + FRAME_INTERVAL)
        .filter(|deadline| *deadline > now)
        .unwrap_or(now + FRAME_INTERVAL)
}

fn byte_len(size: Size<i32, Physical>) -> Result<usize, CaptureError> {
    if !(1..=MAX_DIMENSION).contains(&size.w) || !(1..=MAX_DIMENSION).contains(&size.h) {
        return Err(CaptureError::InvalidSize);
    }
    Ok(size.w as usize * size.h as usize * 4)
}

fn pack_size(size: Size<i32, Physical>) -> usize {
    ((size.w as usize) << 16) | size.h as usize
}

struct PackState {
    buffer: i32,
    alignment: i32,
    row_length: i32,
    skip_rows: i32,
    skip_pixels: i32,
}

impl PackState {
    unsafe fn save(gl: &ffi::Gles2) -> Self {
        let mut state = Self {
            buffer: 0,
            alignment: 0,
            row_length: 0,
            skip_rows: 0,
            skip_pixels: 0,
        };
        // SAFETY: caller holds a current GLES 3 context.
        unsafe {
            gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut state.buffer);
            gl.GetIntegerv(ffi::PACK_ALIGNMENT, &mut state.alignment);
            gl.GetIntegerv(ffi::PACK_ROW_LENGTH, &mut state.row_length);
            gl.GetIntegerv(ffi::PACK_SKIP_ROWS, &mut state.skip_rows);
            gl.GetIntegerv(ffi::PACK_SKIP_PIXELS, &mut state.skip_pixels);
        }
        state
    }

    unsafe fn restore(self, gl: &ffi::Gles2) {
        // SAFETY: saved bindings and pack values came from this context.
        unsafe {
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.buffer as u32);
            gl.PixelStorei(ffi::PACK_ALIGNMENT, self.alignment);
            gl.PixelStorei(ffi::PACK_ROW_LENGTH, self.row_length);
            gl.PixelStorei(ffi::PACK_SKIP_ROWS, self.skip_rows);
            gl.PixelStorei(ffi::PACK_SKIP_PIXELS, self.skip_pixels);
        }
    }
}

unsafe fn delete_slots(gl: &ffi::Gles2, slots: &mut [Slot; RING_SIZE]) {
    for slot in slots {
        // SAFETY: all objects were created by this current context. GLES
        // retains storage referenced by pending commands after deletion.
        unsafe {
            if let Some(pending) = slot.pending.take() {
                gl.DeleteSync(pending.fence);
            }
            if slot.buffer != 0 {
                gl.DeleteBuffers(1, &slot.buffer);
            }
        }
        slot.buffer = 0;
    }
}

struct SocketOwner {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketOwner {
    fn bind(path: &Path) -> io::Result<Self> {
        // bind refuses existing paths; never remove another process's socket.
        let listener = UnixListener::bind(path)?;
        let metadata = fs::symlink_metadata(path)?;
        let owner = Self {
            listener,
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        owner.listener.set_nonblocking(true)?;
        Ok(owner)
    }
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.dev() == self.device && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

enum ClientState {
    Authenticating(Vec<u8>),
    Streaming {
        packet: Option<Arc<Vec<u8>>>,
        offset: usize,
    },
    Probe {
        response: Vec<u8>,
        offset: usize,
    },
}

struct Client {
    stream: UnixStream,
    state: ClientState,
    last_progress: Instant,
    subscribed: bool,
}

fn authenticated(input: &[u8], token: &[u8]) -> bool {
    input.len() == token.len()
        && input
            .iter()
            .zip(token)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// A readable environment token alone is insufficient: only the supervisor
/// process that owns the consent flow may probe or subscribe. SO_PEERCRED is
/// kernel supplied, and all socket operations stay on the worker thread.
fn authorized_peer(stream: &UnixStream, owner_pid: u32) -> bool {
    rustix::net::sockopt::socket_peercred(stream).is_ok_and(|credentials| {
        credentials.pid.as_raw_pid() as u32 == owner_pid
            && credentials.uid == rustix::process::geteuid()
    })
}

fn serve(
    socket: SocketOwner,
    token: String,
    mailbox: Arc<FrameMailbox>,
    subscribers: Arc<Subscribers>,
    stopping: Arc<AtomicBool>,
    output_size: Arc<AtomicUsize>,
    owner_pid: u32,
) {
    let mut clients = Vec::<Client>::new();
    let mut epoch = subscribers.epoch();
    subscribers.applied_epoch.store(epoch, Ordering::Release);
    while !stopping.load(Ordering::Acquire) {
        let requested_epoch = subscribers.epoch();
        if requested_epoch != epoch {
            clients.clear();
            subscribers.count.store(0, Ordering::Release);
            mailbox.discard_revoked(requested_epoch);
            epoch = requested_epoch;
            subscribers.applied_epoch.store(epoch, Ordering::Release);
        }
        // Limit accepted connections and their memory even under local abuse.
        for _ in 0..MAX_CLIENTS {
            match socket.listener.accept() {
                Ok((stream, _)) if clients.len() < MAX_CLIENTS => {
                    if authorized_peer(&stream, owner_pid) && stream.set_nonblocking(true).is_ok() {
                        clients.push(Client {
                            stream,
                            state: ClientState::Authenticating(Vec::with_capacity(72)),
                            last_progress: Instant::now(),
                            subscribed: false,
                        });
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    stopping.store(true, Ordering::Release);
                    break;
                }
            }
        }
        if let Some(frame) = mailbox.take()
            && frame.epoch == epoch
            && subscribers.epoch() == epoch
        {
            let packet = Arc::new(encode_frame(frame));
            for client in &mut clients {
                if subscribers.epoch() != epoch {
                    break;
                }
                if let ClientState::Streaming {
                    packet: pending,
                    offset,
                } = &mut client.state
                    && (pending.is_none() || *offset == 0)
                {
                    *pending = Some(packet.clone());
                    *offset = 0;
                }
            }
        }
        clients.retain_mut(|client| {
            let keep = service_client(client, token.as_bytes(), &subscribers, &output_size, epoch)
                && client.last_progress.elapsed() < CLIENT_TIMEOUT;
            if !keep && client.subscribed {
                subscribers.count.fetch_sub(1, Ordering::AcqRel);
            }
            keep
        });
        thread::park_timeout(Duration::from_millis(1));
    }
    subscribers.count.store(0, Ordering::Release);
}

fn service_client(
    client: &mut Client,
    token: &[u8],
    subscribers: &Subscribers,
    output_size: &AtomicUsize,
    epoch: u64,
) -> bool {
    if subscribers.epoch() != epoch {
        return false;
    }
    if let ClientState::Authenticating(input) = &mut client.state {
        let mut bytes = [0u8; 73];
        match client.stream.read(&mut bytes) {
            Ok(0) => return false,
            Ok(count) => {
                input.extend_from_slice(&bytes[..count]);
                if input.len() > 72 {
                    return false;
                }
                if let Some(newline) = input.iter().position(|b| *b == b'\n') {
                    // One bounded line only; trailing bytes are not commands.
                    if newline + 1 != input.len() {
                        return false;
                    }
                    let line = &input[..newline];
                    if let Some(capability) = line.strip_prefix(b"PROBE ") {
                        if !authenticated(capability, token) {
                            return false;
                        }
                        let size = output_size.load(Ordering::Acquire);
                        client.state = ClientState::Probe {
                            response: format!("AURACAP1 {} {}\n", size >> 16, size & 0xffff)
                                .into_bytes(),
                            offset: 0,
                        };
                    } else {
                        if !authenticated(line, token) {
                            return false;
                        }
                        client.subscribed = true;
                        subscribers.count.fetch_add(1, Ordering::AcqRel);
                        client.state = ClientState::Streaming {
                            packet: None,
                            offset: 0,
                        };
                    }
                    client.last_progress = Instant::now();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    let (data, offset, close_after) = match &mut client.state {
        ClientState::Authenticating(_) => return true,
        ClientState::Probe { response, offset } => (response.as_slice(), offset, true),
        ClientState::Streaming {
            packet: Some(packet),
            offset,
        } => (packet.as_slice(), offset, false),
        ClientState::Streaming { packet: None, .. } => {
            // Detect closed consumers even while no frame is pending. Never
            // accept further commands on an authenticated frame stream.
            let mut byte = [0];
            return matches!(client.stream.read(&mut byte), Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted));
        }
    };
    // A slow consumer can use at most this nonblocking budget per worker
    // iteration; a fast consumer can drain a full HD frame without a fixed
    // 256 KiB / 4 ms throughput ceiling.
    let mut budget = WRITE_BUDGET;
    while *offset < data.len() && budget > 0 {
        if subscribers.epoch() != epoch {
            return false;
        }
        let end = data.len().min(*offset + budget);
        match client.stream.write(&data[*offset..end]) {
            Ok(0) => return false,
            Ok(count) => {
                *offset += count;
                budget -= count;
                client.last_progress = Instant::now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                break;
            }
            Err(_) => return false,
        }
    }
    if *offset == data.len() {
        if close_after {
            return false;
        }
        client.state = ClientState::Streaming {
            packet: None,
            offset: 0,
        };
    }
    true
}

fn encode_frame(mut frame: CapturedFrame) -> Vec<u8> {
    let stride = frame.size.w as usize * 4;
    // The GPU delivers bottom-up RGBA. Convert once on the worker, so all
    // consumers share the same immutable packet with ordinary top-left rows.
    for row in 0..frame.size.h as usize / 2 {
        let opposite = frame.size.h as usize - row - 1;
        let (top, bottom) = frame.pixels.split_at_mut(opposite * stride);
        top[row * stride..(row + 1) * stride].swap_with_slice(&mut bottom[..stride]);
    }
    let mut packet = Vec::with_capacity(HEADER_LEN + frame.pixels.len());
    packet.extend_from_slice(MAGIC);
    packet.extend_from_slice(&(frame.size.w as u32).to_le_bytes());
    packet.extend_from_slice(&(frame.size.h as u32).to_le_bytes());
    packet.extend_from_slice(&(stride as u32).to_le_bytes());
    packet.extend_from_slice(&frame.sequence.to_le_bytes());
    packet.extend_from_slice(&(frame.pixels.len() as u32).to_le_bytes());
    packet.extend_from_slice(&frame.timestamp_ns.to_le_bytes());
    packet.extend_from_slice(&frame.pixels);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_frame_has_exact_header_and_top_left_rows() {
        let packet = encode_frame(CapturedFrame {
            epoch: 0,
            size: (2, 2).into(),
            sequence: 17,
            timestamp_ns: 99,
            pixels: vec![
                0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255,
            ],
        });
        assert_eq!(packet.len(), HEADER_LEN + 16);
        assert_eq!(&packet[..8], MAGIC);
        assert_eq!(u32::from_le_bytes(packet[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(packet[16..20].try_into().unwrap()), 8);
        assert_eq!(u64::from_le_bytes(packet[20..28].try_into().unwrap()), 17);
        assert_eq!(u32::from_le_bytes(packet[28..32].try_into().unwrap()), 16);
        assert_eq!(u64::from_le_bytes(packet[32..40].try_into().unwrap()), 99);
        assert_eq!(&packet[40..48], &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&packet[48..56], &[0, 0, 255, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn rejects_invalid_dimensions_and_redacts_capability() {
        assert!(byte_len((0, 1).into()).is_err());
        assert!(byte_len((4097, 1).into()).is_err());
        assert_eq!(byte_len((4096, 4096).into()).unwrap(), 64 * 1024 * 1024);
        let mut config = CaptureConfig {
            socket: "/tmp/aura-test-capture".into(),
            token: "a".repeat(64),
            owner_pid: std::process::id(),
            size: (2, 2).into(),
        };
        assert!(config.validate().is_ok());
        assert!(!format!("{config:?}").contains(&config.token));
        assert!(!authenticated(b"bad", config.token.as_bytes()));
        config.owner_pid = 0;
        assert!(matches!(
            config.validate(),
            Err(CaptureError::InvalidOwnerPid)
        ));
        config.owner_pid = i32::MAX as u32 + 1;
        assert!(matches!(
            config.validate(),
            Err(CaptureError::InvalidOwnerPid)
        ));
    }

    #[test]
    fn coarse_ticks_keep_thirty_hertz_without_catchup_bursts() {
        let start = Instant::now();
        let mut deadline = None;
        let mut frames = 0;
        for tick in 0..125 {
            let now = start + Duration::from_millis(tick * 8);
            if deadline.is_none_or(|due| now >= due) {
                deadline = Some(advance_capture_deadline(deadline, now));
                frames += 1;
            }
        }
        assert_eq!(frames, 30);
        let resumed = start + Duration::from_secs(10);
        let next = advance_capture_deadline(deadline, resumed);
        assert_eq!(next, resumed + FRAME_INTERVAL);
        assert!(next > resumed);
    }

    #[test]
    fn mailbox_keeps_latest_and_drops_without_waiting() {
        let mailbox = FrameMailbox::default();
        let frame = |sequence| CapturedFrame {
            epoch: 0,
            size: (1, 1).into(),
            sequence,
            timestamp_ns: 0,
            pixels: vec![0; 4],
        };
        mailbox.publish(frame(1));
        mailbox.publish(frame(2));
        assert_eq!(mailbox.take().unwrap().sequence, 2);
        let held = mailbox.0.lock().unwrap();
        mailbox.publish(frame(3));
        drop(held);
        assert!(mailbox.take().is_none());
    }

    /// Run explicitly on a host with EGL (Mesa software rendering is sufficient):
    /// `LIBGL_ALWAYS_SOFTWARE=1 cargo test -p aura-compositor gles_pbo_capture -- --ignored`
    #[test]
    #[ignore = "requires a working EGL/GLES3 driver and Unix sockets"]
    fn gles_pbo_capture_preserves_pack_state_and_reads_composed_rows() {
        use smithay::{
            backend::{
                allocator::Fourcc,
                egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
                renderer::{Bind, Frame, Offscreen, Renderer, gles::GlesTexture},
            },
            utils::Transform,
        };
        // SAFETY: the surfaceless display has no external native handles.
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        // SAFETY: this newly created context is exclusively owned here.
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("driver.sock");
        let mut capture = CaptureProducer::new(
            &mut renderer,
            CaptureConfig {
                socket: path.clone(),
                token: "b".repeat(64),
                owner_pid: std::process::id(),
                size: (2, 2).into(),
            },
        )
        .unwrap();
        assert!(!capture.wants_frame(Instant::now()));
        let mut consumer = UnixStream::connect(&path).unwrap();
        consumer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consumer
            .write_all(format!("{}\n", "b".repeat(64)).as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !capture.wants_frame(Instant::now()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(capture.wants_frame(Instant::now()));
        let size: Size<i32, Physical> = (2, 2).into();
        let mut texture = Offscreen::<GlesTexture>::create_buffer(
            &mut renderer,
            Fourcc::Abgr8888,
            (size.w, size.h).into(),
        )
        .unwrap();
        {
            let mut target = renderer.bind(&mut texture).unwrap();
            let mut frame = renderer
                .render(&mut target, size, Transform::Normal)
                .unwrap();
            frame
                .with_context(|gl| {
                    // SAFETY: the frame binds our two-row test framebuffer.
                    unsafe {
                        gl.Enable(ffi::SCISSOR_TEST);
                        gl.Scissor(0, 0, 2, 1);
                        gl.ClearColor(0., 0., 1., 1.);
                        gl.Clear(ffi::COLOR_BUFFER_BIT);
                        gl.Scissor(0, 1, 2, 1);
                        gl.ClearColor(1., 0., 0., 1.);
                        gl.Clear(ffi::COLOR_BUFFER_BIT);
                        gl.PixelStorei(ffi::PACK_ALIGNMENT, 8);
                        gl.PixelStorei(ffi::PACK_ROW_LENGTH, 5);
                        gl.PixelStorei(ffi::PACK_SKIP_ROWS, 2);
                        gl.PixelStorei(ffi::PACK_SKIP_PIXELS, 1);
                    }
                })
                .unwrap();
            assert!(capture.enqueue(&mut frame, size).unwrap());
            frame
                .with_context(|gl| {
                    // SAFETY: querying pack state in the same current frame.
                    let state = unsafe { PackState::save(gl) };
                    assert_eq!(
                        (
                            state.buffer,
                            state.alignment,
                            state.row_length,
                            state.skip_rows,
                            state.skip_pixels
                        ),
                        (0, 8, 5, 2, 1)
                    );
                })
                .unwrap();
            // The producer polls its own readback fence; no blocking wait.
            let _ = frame.finish().unwrap();
        }
        // Revoke before polling the first PBO, then reconnect. Even an old
        // completion that becomes ready after authentication must be discarded.
        assert!(capture.has_pending());
        capture.revoke();
        assert_eq!(capture.subscriber_count(), 0);
        assert_eq!(consumer.read(&mut [0]).unwrap(), 0);
        consumer = UnixStream::connect(&path).unwrap();
        consumer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consumer
            .write_all(format!("{}\n", "b".repeat(64)).as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while capture.subscriber_count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(capture.subscriber_count(), 1);
        while capture.has_pending() && Instant::now() < deadline {
            capture.poll(&mut renderer).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!capture.has_pending());
        consumer
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(matches!(consumer.read(&mut [0]), Err(error)
            if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)));
        consumer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        {
            let mut target = renderer.bind(&mut texture).unwrap();
            let mut frame = renderer
                .render(&mut target, size, Transform::Normal)
                .unwrap();
            assert!(capture.enqueue(&mut frame, size).unwrap());
            let _ = frame.finish().unwrap();
        }
        while capture.has_pending() && Instant::now() < deadline {
            capture.poll(&mut renderer).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!capture.has_pending());
        let mut packet = [0; HEADER_LEN + 16];
        consumer.read_exact(&mut packet).unwrap();
        assert_eq!(&packet[..8], MAGIC);
        assert_eq!(u64::from_le_bytes(packet[20..28].try_into().unwrap()), 1);
        assert_eq!(
            &packet[HEADER_LEN..HEADER_LEN + 8],
            &[255, 0, 0, 255, 255, 0, 0, 255]
        );
        assert_eq!(&packet[HEADER_LEN + 8..], &[0, 0, 255, 255, 0, 0, 255, 255]);
        capture.shutdown(&mut renderer).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn revocation_closes_consumers_and_filters_old_frames_after_reauthentication() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("revocation.sock");
        let owner = SocketOwner::bind(&path).unwrap();
        let mailbox = Arc::new(FrameMailbox::default());
        let subscribers = Arc::new(Subscribers::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let size = Arc::new(AtomicUsize::new(pack_size((2, 2).into())));
        let (worker_mailbox, worker_subscribers, worker_stopping, worker_size) = (
            mailbox.clone(),
            subscribers.clone(),
            stopping.clone(),
            size.clone(),
        );
        let worker = thread::spawn(move || {
            serve(
                owner,
                "a".repeat(64),
                worker_mailbox,
                worker_subscribers,
                worker_stopping,
                worker_size,
                std::process::id(),
            )
        });
        // This transport-only fixture owns no GL objects, so revocation can be
        // exercised by the ordinary test suite without requiring an EGL driver.
        let mut capture = CaptureProducer {
            slots: std::array::from_fn(|_| Slot::default()),
            allocated_size: None,
            mailbox,
            worker: Some(worker),
            stopping,
            subscribers,
            output_size: size,
            next_frame: None,
            sequence: 0,
            last_completed_sequence: None,
            clock_start: Instant::now(),
            unix_start_ns: 0,
        };
        let connect = || {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .write_all(format!("{}\n", "a".repeat(64)).as_bytes())
                .unwrap();
            stream
        };
        let await_subscriber = |capture: &CaptureProducer| {
            let deadline = Instant::now() + Duration::from_secs(1);
            while capture.subscriber_count() == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(capture.subscriber_count(), 1);
        };
        let mut original = connect();
        await_subscriber(&capture);
        capture.revoke();
        assert_eq!(capture.subscriber_count(), 0);
        assert_eq!(original.read(&mut [0]).unwrap(), 0);
        assert!(path.exists(), "revocation must preserve the listener");
        let mut fresh = connect();
        await_subscriber(&capture);
        // Simulate a completion racing revocation. Its epoch must prevent it
        // from becoming the first packet of the new sharing session.
        capture.mailbox.publish(CapturedFrame {
            epoch: 0,
            size: (2, 2).into(),
            sequence: 1,
            timestamp_ns: 1,
            pixels: vec![9; 16],
        });
        let until = Instant::now() + Duration::from_secs(1);
        while capture.mailbox.0.lock().unwrap().is_some() && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(capture.mailbox.0.lock().unwrap().is_none());
        fresh.set_nonblocking(true).unwrap();
        assert_eq!(
            fresh.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        fresh.set_nonblocking(false).unwrap();
        capture.mailbox.publish(CapturedFrame {
            epoch: capture.subscribers.epoch(),
            size: (2, 2).into(),
            sequence: 2,
            timestamp_ns: 2,
            pixels: vec![7; 16],
        });
        let mut packet = [0; HEADER_LEN + 16];
        fresh.read_exact(&mut packet).unwrap();
        assert_eq!(u64::from_le_bytes(packet[20..28].try_into().unwrap()), 2);
        assert_eq!(&packet[HEADER_LEN..], &[7; 16]);
        capture.revoke();
        assert_eq!(capture.subscriber_count(), 0);
        assert_eq!(fresh.read(&mut [0]).unwrap(), 0);
        drop(capture);
        assert!(!path.exists());
    }

    #[test]
    fn socket_cleanup_preserves_replacement_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.sock");
        let owner = SocketOwner::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        drop(owner);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn socket_peer_pid_probe_authentication_cleanup_and_streaming() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.sock");
        let owner = SocketOwner::bind(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(SocketOwner::bind(&path).is_err());
        let mailbox = Arc::new(FrameMailbox::default());
        let worker_mailbox = mailbox.clone();
        let count = Arc::new(Subscribers::default());
        let stop = Arc::new(AtomicBool::new(false));
        let size = Arc::new(AtomicUsize::new(pack_size((2, 2).into())));
        let worker_count = count.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            serve(
                owner,
                "a".repeat(64),
                worker_mailbox,
                worker_count,
                worker_stop,
                size,
                std::process::id(),
            )
        });
        // Same-UID subprocesses can read an environment token on some hosts.
        // Possession of that token must not permit either protocol request.
        let unauthorized = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                r#"
import os, socket, sys
assert os.getpid() != int(sys.argv[2])
for request in (b'PROBE ' + b'a' * 64 + b'\n', b'a' * 64 + b'\n'):
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    peer.settimeout(1)
    peer.connect(sys.argv[1])
    try:
        peer.sendall(request)
        data = peer.recv(1)
    except (BrokenPipeError, ConnectionResetError):
        pass
    else:
        assert data == b'', 'unauthorized PID received capture information'
    peer.close()
"#,
            )
            .arg(&path)
            .arg(std::process::id().to_string())
            .output()
            .unwrap();
        assert!(
            unauthorized.status.success(),
            "{}",
            String::from_utf8_lossy(&unauthorized.stderr)
        );
        assert_eq!(count.count(), 0);
        let mut invalid = UnixStream::connect(&path).unwrap();
        invalid
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        invalid.write_all(b"wrong\n").unwrap();
        assert_eq!(invalid.read(&mut [0]).unwrap(), 0);
        let mut probe = UnixStream::connect(&path).unwrap();
        probe
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        probe
            .write_all(format!("PROBE {}\n", "a".repeat(64)).as_bytes())
            .unwrap();
        let mut response = String::new();
        probe.read_to_string(&mut response).unwrap();
        assert_eq!(response, "AURACAP1 2 2\n");
        assert_eq!(count.count(), 0);
        let mut consumer = UnixStream::connect(&path).unwrap();
        consumer
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        consumer
            .write_all(format!("{}\n", "a".repeat(64)).as_bytes())
            .unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while count.count() == 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(count.count(), 1);
        mailbox.publish(CapturedFrame {
            epoch: 0,
            size: (2, 2).into(),
            sequence: 1,
            timestamp_ns: 2,
            pixels: vec![7; 16],
        });
        let mut packet = [0; HEADER_LEN + 16];
        consumer.read_exact(&mut packet).unwrap();
        assert_eq!(&packet[..8], MAGIC);
        assert_eq!(&packet[HEADER_LEN..], &[7; 16]);
        drop(consumer);
        while count.count() != 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(count.count(), 0);
        stop.store(true, Ordering::Release);
        worker.thread().unpark();
        worker.join().unwrap();
        assert!(!path.exists());
    }
}
