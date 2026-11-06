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
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
type DecorationMode = zxdg_toplevel_decoration_v1::Mode;

#[derive(Debug)]
struct Options {
    frames: u32,
    title: String,
    stay: bool,
    idle_on_ready: bool,
    exercise_lifecycle: bool,
    exercise_popup: bool,
    exercise_fullscreen: bool,
    start_fullscreen: bool,
    decoration_mode: Option<DecorationMode>,
    observe_pointer: bool,
}

impl Options {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut options = Self {
            frames: 2,
            title: "Aura SHM smoke client".into(),
            stay: false,
            idle_on_ready: false,
            exercise_lifecycle: false,
            exercise_popup: false,
            exercise_fullscreen: false,
            start_fullscreen: false,
            decoration_mode: None,
            observe_pointer: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--frames" => {
                    options.frames = args.next().ok_or("--frames requires a count")?.parse()?;
                }
                "--title" => options.title = args.next().ok_or("--title requires a value")?,
                "--stay" => options.stay = true,
                "--idle-on-ready" => {
                    options.idle_on_ready = true;
                    options.stay = true;
                }
                "--exercise-lifecycle" => options.exercise_lifecycle = true,
                "--exercise-popup" => options.exercise_popup = true,
                "--exercise-fullscreen" => options.exercise_fullscreen = true,
                "--start-fullscreen" => options.start_fullscreen = true,
                "--observe-pointer" => options.observe_pointer = true,
                "--decoration-mode" => {
                    options.decoration_mode = Some(match args.next().as_deref() {
                        Some("server") => DecorationMode::ServerSide,
                        Some("client") => DecorationMode::ClientSide,
                        _ => return Err("--decoration-mode requires server or client".into()),
                    });
                }
                "--help" | "-h" => {
                    println!(
                        "smoke_client [--frames N (>=2)] [--title TITLE] [--stay] [--idle-on-ready] [--exercise-lifecycle] [--exercise-popup] [--exercise-fullscreen | --start-fullscreen] [--decoration-mode server|client] [--observe-pointer]"
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
        if options.start_fullscreen && options.exercise_fullscreen {
            return Err(
                "--start-fullscreen and --exercise-fullscreen are mutually exclusive".into(),
            );
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
    FullscreenSync(FullscreenPhase),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullscreenPhase {
    NotStarted,
    EnterSync,
    EnterFrame,
    RepeatEnterSync,
    RepeatEnterFrame,
    LeaveSync,
    LeaveFrame,
    RepeatLeaveSync,
    RepeatLeaveFrame,
    PersistentSync,
    PersistentFrame,
    PersistentComplete,
    Complete,
}

impl FullscreenPhase {
    fn waiting_for_sync(self) -> bool {
        matches!(
            self,
            Self::EnterSync
                | Self::RepeatEnterSync
                | Self::LeaveSync
                | Self::RepeatLeaveSync
                | Self::PersistentSync
        )
    }
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
    output: Option<wl_output::WlOutput>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    output_size: Option<(i32, i32)>,
    output_scale: i32,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    decoration_mode: Option<DecorationMode>,
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
    fullscreen: bool,
    fullscreen_phase: FullscreenPhase,
    fullscreen_restore_size: (i32, i32),
    fullscreen_restore_maximized: bool,
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
            output: None,
            seat: None,
            pointer: None,
            output_size: None,
            output_scale: 1,
            decoration_manager: None,
            decoration: None,
            decoration_mode: None,
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
            fullscreen: false,
            fullscreen_phase: FullscreenPhase::NotStarted,
            fullscreen_restore_size: (0, 0),
            fullscreen_restore_maximized: false,
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
        if self.options.decoration_mode.is_some() && self.decoration_manager.is_none() {
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
        if self.options.start_fullscreen {
            // Exercise the protocol request before the initial bufferless commit.
            // Readiness later verifies that this request survived mapping.
            toplevel.set_fullscreen(None);
        }
        if let Some(mode) = self.options.decoration_mode {
            let decoration = self
                .decoration_manager
                .as_ref()
                .unwrap()
                .get_toplevel_decoration(&toplevel, qh, ());
            decoration.set_mode(mode);
            self.decoration = Some(decoration);
        }
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

    fn lifecycle_complete(&self) -> bool {
        if self.options.exercise_lifecycle {
            self.lifecycle == Lifecycle::Complete
        } else {
            self.callbacks >= self.options.frames
        }
    }

    fn main_complete(&self) -> bool {
        self.lifecycle_complete()
            && (!self.options.exercise_fullscreen
                || self.fullscreen_phase == FullscreenPhase::Complete)
            && (!self.options.start_fullscreen
                || (self.fullscreen_phase == FullscreenPhase::PersistentComplete
                    && self.fullscreen))
    }

    fn animate_when_ready(&self) -> bool {
        self.options.stay && !self.options.idle_on_ready
    }

    fn complete(&self) -> bool {
        self.main_complete()
            && (self.options.decoration_mode.is_none()
                || self.decoration_mode == self.options.decoration_mode)
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
        if self.options.exercise_fullscreen || self.options.start_fullscreen {
            match self.fullscreen_phase {
                FullscreenPhase::PersistentFrame => {
                    self.fullscreen_phase = FullscreenPhase::PersistentComplete;
                    eprintln!("fullscreen: output-covering buffer presented; remaining fullscreen");
                    self.maybe_start_popup(qh);
                    if self.animate_when_ready() {
                        self.submit(qh)?;
                    }
                    return Ok(());
                }
                FullscreenPhase::EnterFrame => {
                    self.fullscreen_phase = FullscreenPhase::RepeatEnterSync;
                    self.toplevel.as_ref().unwrap().set_fullscreen(None);
                    connection
                        .display()
                        .sync(qh, Callback::FullscreenSync(self.fullscreen_phase));
                    return Ok(());
                }
                FullscreenPhase::RepeatEnterFrame => {
                    self.fullscreen_phase = FullscreenPhase::LeaveSync;
                    self.toplevel.as_ref().unwrap().unset_fullscreen();
                    connection
                        .display()
                        .sync(qh, Callback::FullscreenSync(self.fullscreen_phase));
                    return Ok(());
                }
                FullscreenPhase::LeaveFrame => {
                    self.fullscreen_phase = FullscreenPhase::RepeatLeaveSync;
                    self.toplevel.as_ref().unwrap().unset_fullscreen();
                    connection
                        .display()
                        .sync(qh, Callback::FullscreenSync(self.fullscreen_phase));
                    return Ok(());
                }
                FullscreenPhase::RepeatLeaveFrame => {
                    self.fullscreen_phase = FullscreenPhase::Complete;
                    eprintln!(
                        "fullscreen: repeated enter/leave preserved output coverage and restore geometry"
                    );
                    self.maybe_start_popup(qh);
                    if self.animate_when_ready() {
                        self.submit(qh)?;
                    }
                    return Ok(());
                }
                phase if phase.waiting_for_sync() => {
                    return Err(io::Error::other(
                        "frame arrived during a fullscreen barrier",
                    ));
                }
                _ => {}
            }
        }
        self.after_lifecycle_frame(connection, qh)?;
        if (self.options.exercise_fullscreen || self.options.start_fullscreen)
            && self.fullscreen_phase == FullscreenPhase::NotStarted
            && self.lifecycle_complete()
        {
            self.fullscreen_restore_size = self.size;
            self.fullscreen_restore_maximized = self.maximized;
            self.fullscreen_phase = if self.options.start_fullscreen {
                FullscreenPhase::PersistentSync
            } else {
                FullscreenPhase::EnterSync
            };
            if !self.options.start_fullscreen {
                self.toplevel.as_ref().unwrap().set_fullscreen(None);
            }
            connection
                .display()
                .sync(qh, Callback::FullscreenSync(self.fullscreen_phase));
        }
        Ok(())
    }

    fn after_lifecycle_frame(
        &mut self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) -> io::Result<()> {
        if !self.options.exercise_lifecycle {
            self.maybe_start_popup(qh);
            if !self.lifecycle_complete() || (self.animate_when_ready() && self.main_complete()) {
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
                    if self.animate_when_ready() && self.main_complete() {
                        self.submit(qh)?;
                    }
                } else {
                    self.submit(qh)?;
                }
            }
            Lifecycle::Complete => {
                self.maybe_start_popup(qh);
                if self.animate_when_ready() && self.main_complete() {
                    self.submit(qh)?;
                }
            }
            Lifecycle::InitialFrames => self.submit(qh)?,
            _ => return Err(io::Error::other("frame arrived during a lifecycle barrier")),
        }
        Ok(())
    }

    fn fullscreen_sync(
        &mut self,
        phase: FullscreenPhase,
        qh: &QueueHandle<Self>,
    ) -> io::Result<()> {
        if self.fullscreen_phase != phase {
            return Err(io::Error::other(
                "unexpected fullscreen synchronization callback",
            ));
        }
        self.fullscreen_phase = match phase {
            FullscreenPhase::EnterSync
            | FullscreenPhase::RepeatEnterSync
            | FullscreenPhase::PersistentSync => {
                let output = self
                    .output_size
                    .ok_or_else(|| io::Error::other("wl_output mode unavailable"))?;
                let logical_output = (output.0 / self.output_scale, output.1 / self.output_scale);
                if !self.fullscreen || self.size != logical_output {
                    return Err(io::Error::other(format!(
                        "fullscreen request must cover output: fullscreen={} size={:?} output={logical_output:?}",
                        self.fullscreen, self.size,
                    )));
                }
                match phase {
                    FullscreenPhase::EnterSync => FullscreenPhase::EnterFrame,
                    FullscreenPhase::PersistentSync => FullscreenPhase::PersistentFrame,
                    _ => FullscreenPhase::RepeatEnterFrame,
                }
            }
            FullscreenPhase::LeaveSync | FullscreenPhase::RepeatLeaveSync => {
                if self.fullscreen
                    || self.size != self.fullscreen_restore_size
                    || self.maximized != self.fullscreen_restore_maximized
                {
                    return Err(io::Error::other(format!(
                        "leaving fullscreen did not restore previous state: fullscreen={} size={:?} expected={:?} maximized={} expected={}",
                        self.fullscreen,
                        self.size,
                        self.fullscreen_restore_size,
                        self.maximized,
                        self.fullscreen_restore_maximized,
                    )));
                }
                if phase == FullscreenPhase::LeaveSync {
                    FullscreenPhase::LeaveFrame
                } else {
                    FullscreenPhase::RepeatLeaveFrame
                }
            }
            _ => return Err(io::Error::other("invalid fullscreen synchronization phase")),
        };
        self.submit(qh)
    }

    fn cleanup(&mut self) {
        if let Some(pointer) = self.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
        if let Some(seat) = self.seat.take()
            && seat.version() >= 5
        {
            seat.release();
        }
        // Destroy popup role/surface before its parent xdg_toplevel.
        if let Some(popup) = self.popup.take() {
            popup.role.destroy();
            popup.xdg_surface.destroy();
            popup.surface.destroy();
            if let Some(buffer) = popup.buffer {
                buffer.destroy();
            }
        }
        if let Some(decoration) = self.decoration.take() {
            decoration.destroy();
        }
        if let Some(manager) = self.decoration_manager.take() {
            manager.destroy();
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
                "wl_output" if state.output.is_none() => {
                    state.output = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_seat" if state.options.observe_pointer && state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()));
                }
                "zxdg_decoration_manager_v1" if state.decoration_manager.is_none() => {
                    state.decoration_manager = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
            state.initialize(qh);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
            && capabilities.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => println!("POINTER_ENTER"),
            wl_pointer::Event::Leave { .. } => println!("POINTER_LEAVE"),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(state),
                ..
            } => {
                println!("POINTER_BUTTON button={button} state={state:?}");
            }
            _ => return,
        }
        let _ = std::io::stdout().flush();
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

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                state.output_size = Some((width, height))
            }
            wl_output::Event::Scale { factor } if factor > 0 => state.output_scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure {
            mode: WEnum::Value(mode),
        } = event
        {
            state.decoration_mode = Some(mode);
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
            if state.fullscreen_phase.waiting_for_sync() {
                return;
            }
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
                state.fullscreen = states.as_chunks::<4>().0.iter().any(|bytes| {
                    u32::from_ne_bytes(*bytes) == xdg_toplevel::State::Fullscreen as u32
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
                Callback::FullscreenSync(phase) => state.fullscreen_sync(phase, qh),
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
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);

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
                    "SMOKE_CLIENT_OK frames={} commits={} lifecycle={} popup_frames={} fullscreen={} decoration={}",
                    state.callbacks,
                    state.commits,
                    if state.options.exercise_lifecycle {
                        "passed"
                    } else {
                        "disabled"
                    },
                    state.popup.as_ref().map_or(0, |popup| popup.callbacks),
                    if state.options.exercise_fullscreen {
                        "passed"
                    } else if state.options.start_fullscreen {
                        "active"
                    } else {
                        "disabled"
                    },
                    match state.decoration_mode {
                        Some(DecorationMode::ServerSide) => "server",
                        Some(DecorationMode::ClientSide) => "client",
                        _ => "none",
                    },
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
