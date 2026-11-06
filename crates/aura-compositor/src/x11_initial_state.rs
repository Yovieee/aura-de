//! Bounded background reads of pre-map X11 state, which Smithay does not cache.
use super::{App, X11Surface};
use calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::{
    rustix::event::{PollFd, PollFlags, Timespec, poll},
    x11rb::{
        errors::ReplyError,
        protocol::xproto::{AtomEnum, ConnectionExt},
        rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
        utils::RawFdContainer,
    },
};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(1);
const PENDING_LIMIT: usize = 64;

struct MapRequest {
    window: X11Surface,
    generation: u64,
}
#[derive(Default)]
struct InitialState {
    maximized: bool,
    fullscreen: bool,
}
enum Update {
    Ready(u32),
    Map(Box<MapRequest>, Option<InitialState>),
    Failed(String),
}

pub(crate) struct InitialStateWorker {
    requests: SyncSender<MapRequest>,
    pending: PendingMaps,
    stopped: Arc<AtomicBool>,
    handle: LoopHandle<'static, App>,
    source: Option<RegistrationToken>,
}

#[derive(Default)]
struct PendingMaps {
    windows: HashMap<u32, u64>,
    next_generation: u64,
}

impl PendingMaps {
    fn begin(&mut self, id: u32) -> Result<u64, String> {
        if self.windows.len() >= PENDING_LIMIT && !self.windows.contains_key(&id) {
            return Err("Too many pending X11 initial-state reads".into());
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.windows.insert(id, self.next_generation);
        Ok(self.next_generation)
    }
    fn complete(&mut self, id: u32, generation: u64) -> bool {
        if self.windows.get(&id) != Some(&generation) {
            return false;
        }
        self.windows.remove(&id);
        true
    }
    fn cancel(&mut self, id: u32) {
        self.windows.remove(&id);
    }
}

impl InitialStateWorker {
    pub fn new(
        display: u32,
        handle: LoopHandle<'static, App>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (requests, receiver) = mpsc::sync_channel(PENDING_LIMIT);
        let (updates, source) = calloop::channel::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let source = handle.insert_source(source, |event, _, app: &mut App| {
            let calloop::channel::Event::Msg(update) = event else { return };
            match update {
                Update::Ready(display_number) => {
                    app.xwayland_display = Some(display_number);
                    tracing::info!(display=display_number, "XWayland window manager and initial-state reader ready");
                }
                Update::Failed(error) => app.xwayland_error = Some(error),
                Update::Map(request, state) => {
                    let Some(worker) = &mut app.x11_metadata_worker else { return };
                    let id = request.window.window_id();
                    if !worker.pending.complete(id, request.generation) { return; }
                    let Some(state) = state.filter(|_| request.window.alive()) else { return };
                    let result = (|| {
                        if state.maximized { request.window.set_maximized(true)?; }
                        if state.fullscreen { request.window.set_fullscreen(true)?; }
                        request.window.set_mapped(true)
                    })();
                    match result {
                        Ok(()) => app.x11_map(&request.window),
                        Err(error) => tracing::warn!(%error, window=id, "Could not map X11 window after initial-state read"),
                    }
                }
            }
        })?;
        let worker_stopped = stopped.clone();
        let thread_result = std::thread::Builder::new()
            .name("aura-x11-state".into())
            .spawn(move || {
                if let Err(error) = run(display, receiver, &updates, worker_stopped.clone())
                    && !worker_stopped.load(Ordering::Relaxed)
                {
                    let _ = updates.send(Update::Failed(format!(
                        "XWayland initial-state worker failed: {error}"
                    )));
                }
            });
        if let Err(error) = thread_result {
            handle.remove(source);
            return Err(error.into());
        }
        // The worker owns no compositor state. Drop only signals cancellation;
        // its polling deadline bounds exit without joining on the event thread.
        Ok(Self {
            requests,
            pending: PendingMaps::default(),
            stopped,
            handle,
            source: Some(source),
        })
    }
    pub fn map(&mut self, window: X11Surface) -> Result<(), String> {
        let generation = self.pending.begin(window.window_id())?;
        let request = MapRequest { window, generation };
        let id = request.window.window_id();
        if let Err(error) = self.requests.try_send(request) {
            self.pending.cancel(id);
            return Err(format!("Could not queue X11 initial-state read: {error}"));
        }
        Ok(())
    }
    pub fn cancel(&mut self, window: &X11Surface) {
        self.pending.cancel(window.window_id());
    }
}

impl Drop for InitialStateWorker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(source) = self.source.take() {
            self.handle.remove(source);
        }
    }
}

struct Atoms {
    state: u32,
    max_horz: u32,
    max_vert: u32,
    fullscreen: u32,
}

fn run(
    display: u32,
    requests: Receiver<MapRequest>,
    updates: &calloop::channel::Sender<Update>,
    stopped: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stream = connect(display, stopped.clone())?;
    let connection = RustConnection::connect_to_stream(stream, 0)?;
    let atom = |name: &[u8]| -> Result<u32, Box<dyn std::error::Error>> {
        Ok(connection.intern_atom(false, name)?.reply()?.atom)
    };
    let atoms = Atoms {
        state: atom(b"_NET_WM_STATE")?,
        max_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
        max_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
        fullscreen: atom(b"_NET_WM_STATE_FULLSCREEN")?,
    };
    updates.send(Update::Ready(display))?;
    while !stopped.load(Ordering::Relaxed) {
        let request = match requests.recv_timeout(Duration::from_millis(50)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !request.window.alive() {
            continue;
        }
        connection.stream().deadline.set(Instant::now() + DEADLINE);
        let reply = connection
            .get_property(
                false,
                request.window.window_id(),
                atoms.state,
                AtomEnum::ATOM,
                0,
                64,
            )?
            .reply();
        let state = match reply {
            Ok(reply) => {
                let values = if reply.type_ == u32::from(AtomEnum::ATOM)
                    && reply.format == 32
                    && reply.bytes_after == 0
                {
                    reply
                        .value32()
                        .map(Iterator::collect::<Vec<_>>)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                Some(InitialState {
                    maximized: values.contains(&atoms.max_horz) && values.contains(&atoms.max_vert),
                    fullscreen: values.contains(&atoms.fullscreen),
                })
            }
            Err(ReplyError::X11Error(_)) => None, // The client can destroy a queued window.
            Err(error) => return Err(error.into()),
        };
        updates.send(Update::Map(Box::new(request), state))?;
    }
    Ok(())
}

/// x11rb reads and writes nonblocking sockets but normally waits indefinitely in
/// Stream::poll. Bound those waits (including the initial handshake) here.
struct DeadlineStream {
    inner: DefaultStream,
    deadline: Cell<Instant>,
    stopped: Arc<AtomicBool>,
}

impl DeadlineStream {
    fn check(&self) -> io::Result<Duration> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(io::Error::other("X11 state worker cancelled"));
        }
        let left = self
            .deadline
            .get()
            .saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 state request timed out",
            ))
        } else {
            Ok(left)
        }
    }
}

impl Stream for DeadlineStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        loop {
            let timeout = Timespec::try_from(self.check()?.min(Duration::from_millis(50))).unwrap();
            let mut interest = PollFlags::empty();
            if mode.readable() {
                interest |= PollFlags::IN;
            }
            if mode.writable() {
                interest |= PollFlags::OUT;
            }
            let mut fds = [PollFd::new(&self.inner, interest)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => return Ok(()),
                Err(smithay::reexports::rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn read(&self, bytes: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.inner.read(bytes, fds)
    }
    fn write(&self, bytes: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.inner.write(bytes, fds)
    }
}

fn connect(display: u32, stopped: Arc<AtomicBool>) -> io::Result<DeadlineStream> {
    use smithay::reexports::rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with,
    };
    let deadline = Instant::now() + DEADLINE;
    let fd = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    let address = SocketAddrUnix::new(format!("/tmp/.X11-unix/X{display}"))?;
    let stream = UnixStream::from(fd);
    loop {
        if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 state connection timed out or was cancelled",
            ));
        }
        match connect(&stream, &address) {
            Ok(()) | Err(smithay::reexports::rustix::io::Errno::ISCONN) => break,
            Err(
                smithay::reexports::rustix::io::Errno::AGAIN
                | smithay::reexports::rustix::io::Errno::INPROGRESS
                | smithay::reexports::rustix::io::Errno::ALREADY,
            ) => std::thread::sleep(Duration::from_millis(5)),
            Err(smithay::reexports::rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(DeadlineStream {
        inner: DefaultStream::from_unix_stream(stream)?.0,
        deadline: Cell::new(deadline),
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn stream(client: UnixStream, deadline: Duration, stopped: Arc<AtomicBool>) -> DeadlineStream {
        DeadlineStream {
            inner: DefaultStream::from_unix_stream(client).unwrap().0,
            deadline: Cell::new(Instant::now() + deadline),
            stopped,
        }
    }

    #[test]
    fn silent_x11_peer_cannot_hold_the_metadata_handshake_past_its_deadline() {
        let (client, _peer) = UnixStream::pair().unwrap();
        let (finished, result) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let connection = RustConnection::connect_to_stream(
                stream(
                    client,
                    Duration::from_millis(75),
                    Arc::new(AtomicBool::new(false)),
                ),
                0,
            );
            finished
                .send((connection.is_err(), started.elapsed()))
                .unwrap();
        });
        let (failed, elapsed) = result.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(failed);
        assert!(elapsed < Duration::from_secs(1));
    }

    #[test]
    fn cancelling_the_metadata_worker_interrupts_a_silent_handshake() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let (finished, result) = mpsc::channel();
        std::thread::spawn(move || {
            let connection = RustConnection::connect_to_stream(
                stream(client, Duration::from_secs(5), worker_stopped),
                0,
            );
            finished.send(connection.is_err()).unwrap();
        });
        // Observe the real X11 setup request before cancellation, so this checks
        // an in-progress poll rather than merely rejecting an already stopped task.
        peer.read_exact(&mut [0; 12]).unwrap();
        stopped.store(true, Ordering::Relaxed);
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn withdrawn_or_replaced_maps_never_accept_an_old_metadata_result() {
        let mut pending = PendingMaps::default();
        let first = pending.begin(42).unwrap();
        pending.cancel(42);
        assert!(!pending.complete(42, first));
        let replacement = pending.begin(42).unwrap();
        assert!(!pending.complete(42, first));
        assert!(pending.complete(42, replacement));
        assert!(!pending.complete(42, replacement));
        for id in 0..PENDING_LIMIT as u32 {
            pending.begin(id).unwrap();
        }
        assert!(pending.begin(1000).is_err());
        pending.cancel(0);
        assert!(pending.begin(1000).is_ok());
    }
}
