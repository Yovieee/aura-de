//! Real xdg-shell/SHM client used by the nested-compositor integration smoke test.
//!
//! A successful connection alone is insufficient: at least two committed buffers
//! must receive frame callbacks. All protocol setup is covered by a ten-second timeout.
use std::{
    error::Error,
    io::{self, Write},
    os::fd::AsFd,
    time::{Duration, Instant},
};

use smithay::reexports::rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};

#[derive(Debug)]
struct Options {
    frames: u32,
    title: String,
    stay: bool,
    exercise_lifecycle: bool,
    exercise_popup: bool,
}

impl Options {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut options = Self {
            frames: 2,
            title: "Aura SHM smoke client".into(),
            stay: false,
            exercise_lifecycle: false,
            exercise_popup: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--frames" => {
                    options.frames = args.next().ok_or("--frames requires a count")?.parse()?;
                }
                "--title" => options.title = args.next().ok_or("--title requires a value")?,
                "--stay" => options.stay = true,
                "--exercise-lifecycle" => options.exercise_lifecycle = true,
                "--exercise-popup" => options.exercise_popup = true,
                "--help" | "-h" => {
                    println!(
                        "smoke_client [--frames N (>=2)] [--title TITLE] [--stay] [--exercise-lifecycle] [--exercise-popup]"
                    );
                    std::process::exit(0);
                }
                _ if arg.starts_with("--frames=") => options.frames = arg[9..].parse()?,
                _ if arg.starts_with("--frames") => options.frames = arg[8..].parse()?,
                _ => return Err(format!("unknown argument: {arg}").into()),
            }
        }
        if options.frames < 2 {
            return Err("--frames must be at least 2 to verify repeated presentation".into());
        }
        Ok(options)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    InitialFrames,
    FirstMaximizeSync,
    FirstMaximizeFrame,
    SecondMaximizeSync,
    SecondMaximizeFrame,
    RemapConfigure,
    RemapFrames,
    Complete,
}

#[derive(Debug, Clone, Copy)]
enum Callback {
    Frame(u32),
    PopupFrame(u32),
    FirstMaximizeSync,
    SecondMaximizeSync,
}

#[derive(Debug, Clone, Copy)]
enum SurfaceRole {
    Main,
    Popup,
}

struct Popup {
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    role: xdg_popup::XdgPopup,
    buffer: Option<wl_buffer::WlBuffer>,
    size: (i32, i32),
    configured: bool,
    callback_pending: bool,
    commits: u32,
    callbacks: u32,
}

struct State {
    options: Options,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    buffer: Option<wl_buffer::WlBuffer>,
    retired_buffers: Vec<wl_buffer::WlBuffer>,
    size: (i32, i32),
    buffer_size: (i32, i32),
    configured: bool,
    callback_pending: bool,
    commits: u32,
    callbacks: u32,
    lifecycle: Lifecycle,
    maximized: bool,
    remap_callbacks: u32,
    popup: Option<Popup>,
    closed: bool,
    error: Option<String>,
}

impl State {
    fn new(options: Options) -> Self {
        Self {
            options,
            compositor: None,
            shm: None,
            wm_base: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            buffer: None,
            retired_buffers: Vec::new(),
            size: (320, 240),
            buffer_size: (0, 0),
            configured: false,
            callback_pending: false,
            commits: 0,
            callbacks: 0,
            lifecycle: Lifecycle::InitialFrames,
            maximized: false,
            remap_callbacks: 0,
            popup: None,
            closed: false,
            error: None,
        }
    }

    fn initialize(&mut self, qh: &QueueHandle<Self>) {
        if self.surface.is_some() {
            return;
        }
        let (Some(compositor), Some(wm_base), Some(_)) =
            (&self.compositor, &self.wm_base, &self.shm)
        else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, SurfaceRole::Main);
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_app_id("org.aura.SmokeClient".into());
        toplevel.set_title(self.options.title.clone());
        // The initial role commit must not attach a buffer until configure is acknowledged.
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }

    fn make_buffer(&mut self, qh: &QueueHandle<Self>) -> io::Result<()> {
        if self.buffer.is_some() && self.buffer_size == self.size {
            return Ok(());
        }
        let (width, height) = self.size;
        // Bound memory use even if a broken compositor sends unreasonable dimensions.
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return Err(io::Error::other(
                "invalid xdg_toplevel configure dimensions",
            ));
        }
        let mut file = tempfile::tempfile()?;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let stripe = if (x / 24 + y / 24) % 2 == 0 { 28 } else { 0 };
                let red = (40 + x * 150 / width + stripe) as u8;
                let green = (45 + y * 150 / height + stripe) as u8;
                let blue = (90 + stripe) as u8;
                pixels
                    .extend_from_slice(&u32::from_be_bytes([255, red, green, blue]).to_ne_bytes());
            }
        }
        file.write_all(&pixels)?;
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| io::Error::other("wl_shm unavailable"))?;
        let pool = shm.create_pool(file.as_fd(), width * height * 4, qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        // A buffer retains the pool's backing storage after the pool object is destroyed.
        pool.destroy();
        if let Some(old) = self.buffer.replace(buffer) {
            self.retired_buffers.push(old);
        }
        self.buffer_size = self.size;
        Ok(())
    }

    fn submit(&mut self, qh: &QueueHandle<Self>) -> io::Result<()> {
        if !self.configured || self.callback_pending || self.closed {
            return Ok(());
        }
        self.make_buffer(qh)?;
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| io::Error::other("surface unavailable"))?;
        self.commits = self.commits.saturating_add(1);
        surface.attach(self.buffer.as_ref(), 0, 0);
        surface.damage(0, 0, self.size.0, self.size.1);
        surface.frame(qh, Callback::Frame(self.commits));
        self.callback_pending = true;
        surface.commit();
        Ok(())
    }

    fn main_complete(&self) -> bool {
        if self.options.exercise_lifecycle {
            self.lifecycle == Lifecycle::Complete
        } else {
            self.callbacks >= self.options.frames
        }
    }

    fn complete(&self) -> bool {
        self.main_complete()
            && (!self.options.exercise_popup
                || self
                    .popup
                    .as_ref()
                    .is_some_and(|popup| popup.callbacks >= self.options.frames))
    }

    fn maybe_start_popup(&mut self, qh: &QueueHandle<Self>) {
        if !self.options.exercise_popup || !self.main_complete() || self.popup.is_some() {
            return;
        }
        let wm_base = self.wm_base.as_ref().unwrap();
        let positioner = wm_base.create_positioner(qh, ());
        positioner.set_size(160, 96);
        positioner.set_anchor_rect(20, 20, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let surface = self.compositor.as_ref().unwrap().create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, SurfaceRole::Popup);
        let role = xdg_surface.get_popup(self.xdg_surface.as_ref(), &positioner, qh, ());
        positioner.destroy();
        surface.commit();
        self.popup = Some(Popup {
            surface,
            xdg_surface,
            role,
            buffer: None,
            size: (160, 96),
            configured: false,
            callback_pending: false,
            commits: 0,
            callbacks: 0,
        });
        eprintln!("popup: created xdg_popup; waiting for configure");
    }

    fn submit_popup(&mut self, qh: &QueueHandle<Self>) -> io::Result<()> {
        let Some(popup) = self.popup.as_mut() else {
            return Ok(());
        };
        if !popup.configured || popup.callback_pending {
            return Ok(());
        }
        if popup.buffer.is_none() {
            let (width, height) = popup.size;
            if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
                return Err(io::Error::other("invalid popup configure dimensions"));
            }
            let mut file = tempfile::tempfile()?;
            let pixel = u32::from_be_bytes([255, 230, 175, 40]).to_ne_bytes();
            let pixels: Vec<_> = (0..width * height).flat_map(|_| pixel).collect();
            file.write_all(&pixels)?;
            let pool =
                self.shm
                    .as_ref()
                    .unwrap()
                    .create_pool(file.as_fd(), width * height * 4, qh, ());
            popup.buffer = Some(pool.create_buffer(
                0,
                width,
                height,
                width * 4,
                wl_shm::Format::Argb8888,
                qh,
                (),
            ));
            pool.destroy();
        }
        popup.commits += 1;
        popup.surface.attach(popup.buffer.as_ref(), 0, 0);
        popup.surface.damage(0, 0, popup.size.0, popup.size.1);
        popup.surface.frame(qh, Callback::PopupFrame(popup.commits));
        popup.callback_pending = true;
        popup.surface.commit();
        Ok(())
    }

    fn after_frame(&mut self, connection: &Connection, qh: &QueueHandle<Self>) -> io::Result<()> {
        if !self.options.exercise_lifecycle {
            self.maybe_start_popup(qh);
            if self.options.stay || !self.main_complete() {
                self.submit(qh)?;
            }
            return Ok(());
        }
        match self.lifecycle {
            Lifecycle::InitialFrames if self.callbacks >= self.options.frames => {
                self.lifecycle = Lifecycle::FirstMaximizeSync;
                self.toplevel.as_ref().unwrap().set_maximized();
                connection.display().sync(qh, Callback::FirstMaximizeSync);
            }
            Lifecycle::FirstMaximizeFrame => {
                // Check idempotence after the first maximize has actually configured and drawn.
                self.lifecycle = Lifecycle::SecondMaximizeSync;
                self.toplevel.as_ref().unwrap().set_maximized();
                connection.display().sync(qh, Callback::SecondMaximizeSync);
            }
            Lifecycle::SecondMaximizeFrame => {
                self.lifecycle = Lifecycle::RemapConfigure;
                self.configured = false;
                let surface = self.surface.as_ref().unwrap();
                surface.attach(None, 0, 0);
                surface.commit();
                // xdg-shell requires another bufferless commit/configure/ack sequence after unmap.
                surface.commit();
                eprintln!("lifecycle: null-buffer unmap committed; waiting for remap configure");
            }
            Lifecycle::RemapFrames => {
                self.remap_callbacks += 1;
                if self.remap_callbacks >= self.options.frames {
                    self.lifecycle = Lifecycle::Complete;
                    self.maybe_start_popup(qh);
                    if self.options.stay {
                        self.submit(qh)?;
                    }
                } else {
                    self.submit(qh)?;
                }
            }
            Lifecycle::Complete => {
                self.maybe_start_popup(qh);
                if self.options.stay {
                    self.submit(qh)?;
                }
            }
            Lifecycle::InitialFrames => self.submit(qh)?,
            _ => return Err(io::Error::other("frame arrived during a lifecycle barrier")),
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        // Destroy popup role/surface before its parent xdg_toplevel.
        if let Some(popup) = self.popup.take() {
            popup.role.destroy();
            popup.xdg_surface.destroy();
            popup.surface.destroy();
            if let Some(buffer) = popup.buffer {
                buffer.destroy();
            }
        }
        if let Some(toplevel) = self.toplevel.take() {
            toplevel.destroy();
        }
        if let Some(surface) = self.xdg_surface.take() {
            surface.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        for buffer in self.retired_buffers.drain(..) {
            buffer.destroy();
        }
        if let Some(wm_base) = self.wm_base.take() {
            wm_base.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind(name, 1, qh, ()));
                }
                "xdg_wm_base" if state.wm_base.is_none() => {
                    state.wm_base = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
            state.initialize(qh);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceRole> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        role: &SurfaceRole,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if matches!(role, SurfaceRole::Popup) {
                if let Some(popup) = state.popup.as_mut() {
                    popup.configured = true;
                }
                if let Err(error) = state.submit_popup(qh) {
                    state.error = Some(error.to_string());
                }
                return;
            }
            state.configured = true;
            match state.lifecycle {
                Lifecycle::FirstMaximizeSync | Lifecycle::SecondMaximizeSync => return,
                Lifecycle::RemapConfigure => {
                    state.lifecycle = Lifecycle::RemapFrames;
                    state.remap_callbacks = 0;
                    eprintln!("lifecycle: remap configure acknowledged");
                }
                _ => {}
            }
            if let Err(error) = state.submit(qh) {
                state.error = Some(error.to_string());
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                state.maximized = states.as_chunks::<4>().0.iter().any(|bytes| {
                    u32::from_ne_bytes(*bytes) == xdg_toplevel::State::Maximized as u32
                });

                if width > 0 {
                    state.size.0 = width;
                }
                if height > 0 {
                    state.size.1 = height;
                }
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Callback> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        callback: &Callback,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            let result = match *callback {
                Callback::Frame(commit) => {
                    if commit == 0 || commit != state.commits || !state.callback_pending {
                        state.error =
                            Some("frame callback did not correspond to a committed buffer".into());
                        return;
                    }
                    state.callback_pending = false;
                    state.callbacks = state.callbacks.saturating_add(1);
                    state.after_frame(connection, qh)
                }
                Callback::PopupFrame(commit) => {
                    let Some(popup) = state.popup.as_mut() else {
                        state.error = Some("popup frame arrived without a popup".into());
                        return;
                    };
                    if commit != popup.commits || !popup.callback_pending {
                        state.error =
                            Some("popup frame did not correspond to a buffer commit".into());
                        return;
                    }
                    popup.callback_pending = false;
                    popup.callbacks += 1;
                    if popup.callbacks < state.options.frames {
                        state.submit_popup(qh)
                    } else {
                        eprintln!("popup: two committed frames presented");
                        Ok(())
                    }
                }
                Callback::FirstMaximizeSync => {
                    if state.lifecycle != Lifecycle::FirstMaximizeSync || !state.maximized {
                        Err(io::Error::other(
                            "first set_maximized did not configure MAXIMIZED",
                        ))
                    } else {
                        state.lifecycle = Lifecycle::FirstMaximizeFrame;
                        eprintln!("lifecycle: first maximize confirmed");
                        state.submit(qh)
                    }
                }
                Callback::SecondMaximizeSync => {
                    if state.lifecycle != Lifecycle::SecondMaximizeSync || !state.maximized {
                        Err(io::Error::other(
                            "repeated set_maximized cleared MAXIMIZED (not idempotent)",
                        ))
                    } else {
                        state.lifecycle = Lifecycle::SecondMaximizeFrame;
                        eprintln!("lifecycle: repeated maximize remained MAXIMIZED");
                        state.submit(qh)
                    }
                }
            };
            if let Err(error) = result {
                state.error = Some(error.to_string());
            }
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure { width, height, .. } => {
                if let Some(popup) = state.popup.as_mut() {
                    if width > 0 {
                        popup.size.0 = width;
                    }
                    if height > 0 {
                        popup.size.1 = height;
                    }
                }
            }
            xdg_popup::Event::PopupDone
                if state
                    .popup
                    .as_ref()
                    .is_none_or(|popup| popup.callbacks < state.options.frames) =>
            {
                state.error =
                    Some("popup dismissed before its buffered frames were presented".into());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(index) = state.retired_buffers.iter().position(|old| old == buffer)
        {
            state.retired_buffers.swap_remove(index).destroy();
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore xdg_positioner::XdgPositioner);

fn run(connection: &Connection, state: &mut State) -> Result<(), Box<dyn Error>> {
    let mut queue = connection.new_event_queue::<State>();
    connection.display().get_registry(&queue.handle(), ());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reported = false;
    loop {
        queue.dispatch_pending(state)?;
        if let Some(error) = &state.error {
            return Err(error.clone().into());
        }
        if state.complete() {
            if !reported {
                println!(
                    "SMOKE_CLIENT_OK frames={} commits={} lifecycle={} popup_frames={}",
                    state.callbacks,
                    state.commits,
                    if state.options.exercise_lifecycle {
                        "passed"
                    } else {
                        "disabled"
                    },
                    state.popup.as_ref().map_or(0, |popup| popup.callbacks)
                );
                reported = true;
            }
            if !state.options.stay || state.closed {
                return Ok(());
            }
        }
        if state.closed {
            return Err("compositor closed the client before the requested frames".into());
        }
        if !reported && Instant::now() >= deadline {
            return Err(format!(
                "timed out after 10s: configured={} commits={} callbacks={} lifecycle={:?} popup_frames={}",
                state.configured, state.commits, state.callbacks, state.lifecycle,
                state.popup.as_ref().map_or(0, |popup| popup.callbacks)
            )
            .into());
        }
        queue.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let wait = if reported {
            Duration::from_millis(250)
        } else {
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250))
        };
        let timeout = Timespec::try_from(wait)?;
        let connection_fd = guard.connection_fd();
        let mut fds = [PollFd::new(&connection_fd, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => drop(guard),
            Ok(_) => {
                guard.read()?;
            }
            Err(error) if error == smithay::reexports::rustix::io::Errno::INTR => drop(guard),
            Err(error) => return Err(error.into()),
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse()?;
    let connection = Connection::connect_to_env()?;
    let mut state = State::new(options);
    let result = run(&connection, &mut state);
    state.cleanup();
    // Flush destructor requests before disconnecting, on both success and failure.
    let _ = connection.flush();
    result
}
