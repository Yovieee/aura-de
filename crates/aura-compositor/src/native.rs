//! Feature-gated, single-connector DRM/GBM presentation with a libseat lease.
//!
//! This is a real device backend, not a headless substitute. Startup fails before
//! publishing Wayland/control sockets when the seat, GPU or connector is absent.
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface, NodeType},
        egl::{EGLContext, EGLDisplay, fence::EGLFence},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind,
            gles::{GlesRenderer, GlesTarget},
            sync::SyncPoint,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    reexports::{
        drm::control::{self, Device as _, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Physical, Rectangle, Size},
};
use std::{
    cell::RefCell,
    error::Error,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Surface = GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>;

pub(crate) struct NativeOutput {
    pub name: String,
    pub physical_size: (i32, i32),
    pub make: String,
    pub model: String,
    pub refresh: i32,
}

pub(crate) enum NativeEvent {
    Input(InputEvent<LibinputInputBackend>),
    Activated(bool),
}

#[derive(Default)]
struct EventState {
    inputs: Vec<InputEvent<LibinputInputBackend>>,
    sessions: Vec<SessionEvent>,
    pending_flip: bool,
    submitted_at: Option<Instant>,
    vt_requested_at: Option<Instant>,
    error: Option<String>,
}

impl EventState {
    fn flip_completed(&mut self) {
        self.pending_flip = false;
        self.submitted_at = None;
    }

    fn flip_expired(&self, active: bool, now: Instant) -> bool {
        active
            && self.pending_flip
            && self.submitted_at.is_some_and(|submitted| {
                now.saturating_duration_since(submitted) >= Duration::from_secs(3)
            })
    }

    fn rendering_allowed(&self, active: bool) -> bool {
        active && !self.pending_flip && self.vt_requested_at.is_none()
    }

    fn release_noop_vt_wait(&mut self, active: bool, now: Instant) {
        // A switch to the current VT can succeed without any seat transition.
        // Release that no-op after polling the notifier for a bounded interval.
        if active
            && self.vt_requested_at.is_some_and(|requested| {
                now.saturating_duration_since(requested) >= Duration::from_secs(2)
            })
        {
            self.vt_requested_at = None;
        }
    }
}

struct Resources {
    surface: Rc<RefCell<Surface>>,
    renderer: GlesRenderer,
    drm: Rc<RefCell<DrmDevice>>,
    input: Libinput,
    // The framebuffer target must outlive every GlesTarget borrowed from bind().
    target: Option<Dmabuf>,
}

/// Releases sources before resources even when source registration fails midway.
struct StartupDevice {
    handle: LoopHandle<'static, EventState>,
    sources: Vec<RegistrationToken>,
    resources: Option<Resources>,
    lease: Option<DeviceFd>,
    session: LibSeatSession,
}

fn release_device(
    handle: &LoopHandle<'static, EventState>,
    sources: &mut Vec<RegistrationToken>,
    resources: &mut Option<Resources>,
    lease: &mut Option<DeviceFd>,
    session: &mut LibSeatSession,
) {
    for token in sources.drain(..) {
        handle.remove(token);
    }
    if let Some(resources) = resources.take() {
        if session.is_active() {
            let _ = resources.drm.borrow_mut().reset_state();
        }
        drop(resources);
    }
    if let Some(lease) = lease.take() {
        match lease.try_into() {
            Ok(fd) => {
                if let Err(error) = session.close(fd) {
                    tracing::warn!(%error, "Could not release the native DRM seat device");
                }
            }
            Err(_) => tracing::warn!("Native DRM device still referenced during shutdown"),
        }
    }
}

impl Drop for StartupDevice {
    fn drop(&mut self) {
        release_device(
            &self.handle,
            &mut self.sources,
            &mut self.resources,
            &mut self.lease,
            &mut self.session,
        );
    }
}

pub(crate) struct NativeBackend {
    events: EventLoop<'static, EventState>,
    state: EventState,
    sources: Vec<RegistrationToken>,
    resources: Option<Resources>,
    lease: Option<DeviceFd>,
    session: LibSeatSession,
    output: NativeOutput,
    size: Size<i32, Physical>,
    active: bool,
}

fn requested_device() -> Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os("AURA_DRM_DEVICE") else {
        if !Path::new("/dev/dri").is_dir() {
            return Err("Native DRM/KMS requires a DRM primary device under /dev/dri and an active libseat session; no /dev/dri is available".into());
        }
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("AURA_DRM_DEVICE must be an absolute DRM primary-device path, for example /dev/dri/card0".into());
    }
    let metadata = path.metadata().map_err(|error| {
        format!(
            "Native DRM device {} is unavailable: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_char_device() {
        return Err(format!(
            "Native DRM device {} must be a character device",
            path.display()
        )
        .into());
    }
    let node = DrmNode::from_path(&path).map_err(|error| {
        format!(
            "Native DRM device {} is not a DRM primary node: {error}",
            path.display()
        )
    })?;
    if node.ty() != NodeType::Primary {
        return Err(format!("Native DRM device {} must be a DRM primary node; render nodes cannot drive KMS outputs", path.display()).into());
    }
    Ok(Some(path.canonicalize()?))
}

fn usable_mode(mode: &&control::Mode) -> bool {
    let (width, height) = mode.size();
    (64..=8192).contains(&width)
        && (64..=8192).contains(&height)
        && mode.hsync().2 != 0
        && mode.vsync().2 != 0
}

fn select_connector(
    device: &DrmDevice,
    requested: Option<&str>,
) -> Result<(connector::Info, crtc::Handle, control::Mode)> {
    let resources = device.resource_handles()?;
    for handle in resources.connectors() {
        let info = device.get_connector(*handle, true)?;
        if requested.is_some_and(|name| name != info.to_string())
            || info.state() != connector::State::Connected
        {
            continue;
        }
        let Some(mode) = info
            .modes()
            .iter()
            .filter(usable_mode)
            .find(|mode| mode.mode_type().contains(control::ModeTypeFlags::PREFERRED))
            .or_else(|| info.modes().iter().find(usable_mode))
            .copied()
        else {
            continue;
        };
        for encoder in info.encoders() {
            let encoder = device.get_encoder(*encoder)?;
            for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
                if device
                    .planes(&crtc)
                    .is_ok_and(|planes| !planes.primary.is_empty())
                {
                    return Ok((info, crtc, mode));
                }
            }
        }
    }
    Err(match requested {
        Some(name) => {
            format!("Requested DRM connector {name:?} is not connected or has no usable mode/CRTC")
        }
        None => "No connected DRM connector with a usable mode/CRTC was found".into(),
    }
    .into())
}

impl NativeBackend {
    pub fn new() -> Result<Self> {
        // Do these checks before opening a seat, so bad explicit selections are
        // deterministic and harmless in CI and in an existing desktop session.
        let requested = requested_device()?;
        let connector_name = std::env::var_os("AURA_DRM_CONNECTOR")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| "AURA_DRM_CONNECTOR must be valid UTF-8")
            })
            .transpose()?;
        if connector_name.as_deref().is_some_and(str::is_empty) {
            return Err("AURA_DRM_CONNECTOR must name a connector, for example DP-1".into());
        }
        let (mut session, notifier) = LibSeatSession::new().map_err(|error| format!("Cannot open a native libseat session: {error}. Start from an active local VT with logind or seatd access"))?;
        let mut events = EventLoop::try_new()?;
        events
            .handle()
            .insert_source(notifier, |event, _, state: &mut EventState| {
                state.sessions.push(event)
            })?;
        let mut state = EventState::default();
        let activation_deadline = Instant::now() + Duration::from_secs(2);
        while !session.is_active() {
            let remaining = activation_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("The native libseat session did not become active within 2 seconds; start Aura on the active local seat".into());
            }
            // Seat grants can arrive asynchronously after LibSeatSession::new.
            // No protocol sockets or frame callbacks exist during this wait.
            events.dispatch(remaining.min(Duration::from_millis(50)), &mut state)?;
        }
        state.sessions.clear();
        let udev = UdevBackend::new(session.seat())?;
        let mut candidates: Vec<_> = udev
            .device_list()
            .map(|(id, path)| (id, path.to_owned()))
            .collect();
        candidates.sort_by(|a, b| a.1.cmp(&b.1));
        candidates.retain(|(_, path)| requested.as_ref().is_none_or(|requested| path == requested));
        if candidates.is_empty() {
            return Err("No DRM primary device belongs to the active libseat seat; check AURA_DRM_DEVICE and seat permissions".into());
        }
        let mut failures = Vec::new();
        for (device_id, path) in candidates {
            let fd = match session.open(&path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK) {
                Ok(fd) => fd,
                Err(error) => {
                    failures.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let lease = DeviceFd::from(fd);
            let initialized =
                Self::initialize_device(&lease, connector_name.as_deref(), session.clone());
            let (resources, drm_notifier, connector, crtc, mode) = match initialized {
                Ok(result) => result,
                Err(error) => {
                    failures.push(format!("{}: {error}", path.display()));
                    if let Ok(fd) = lease.try_into() {
                        let _ = session.close(fd);
                    }
                    continue;
                }
            };
            let mut startup = StartupDevice {
                handle: events.handle(),
                sources: Vec::new(),
                resources: Some(resources),
                lease: Some(lease),
                session: session.clone(),
            };
            let resources = startup.resources.as_ref().unwrap();
            let surface = resources.surface.clone();
            let drm_token = events.handle().insert_source(
                drm_notifier,
                move |event, _, state| match event {
                    DrmEvent::VBlank(event_crtc) if event_crtc == crtc => {
                        if let Err(error) = surface.borrow_mut().frame_submitted() {
                            state.error = Some(format!("DRM page-flip completion failed: {error}"));
                        }
                        state.flip_completed();
                    }
                    DrmEvent::Error(error) => {
                        state.error = Some(format!("DRM event processing failed: {error}"))
                    }
                    _ => {}
                },
            )?;
            startup.sources.push(drm_token);
            let input_token = events.handle().insert_source(
                LibinputInputBackend::new(resources.input.clone()),
                |event, _, state| state.inputs.push(event),
            )?;
            startup.sources.push(input_token);
            let drm = resources.drm.clone();
            let selected_connector = connector.handle();
            let selected_mode = mode;
            let udev_token = events.handle().insert_source(udev, move |event, _, state| match event {
                UdevEvent::Removed { device_id: removed } if removed == device_id => state.error = Some("The native DRM device was removed; restart Aura after reconnecting it".into()),
                UdevEvent::Changed { device_id: changed } if changed == device_id => {
                    match drm.borrow().get_connector(selected_connector, true) {
                        Ok(info) if info.state() == connector::State::Connected && info.modes().contains(&selected_mode) => {},
                        _ => state.error = Some("The selected native output disconnected or its mode disappeared; restart Aura to select an output".into()),
                    }
                }
                _ => {},
            })?;
            startup.sources.push(udev_token);
            // Drain initial device-added input without claiming it was delivered
            // until the shared compositor protocol state is ready.
            events.dispatch(Duration::ZERO, &mut state)?;
            let size = (i32::from(mode.size().0), i32::from(mode.size().1)).into();
            let physical = connector.size().unwrap_or((0, 0));
            tracing::info!(device=%path.display(), connector=%connector, width=mode.size().0, height=mode.size().1, "Native DRM/GBM presenter initialized; hardware behavior requires validation");
            return Ok(Self {
                events,
                state,
                sources: std::mem::take(&mut startup.sources),
                resources: startup.resources.take(),
                lease: startup.lease.take(),
                session,
                output: NativeOutput {
                    name: connector.to_string(),
                    physical_size: (physical.0 as i32, physical.1 as i32),
                    make: "DRM".into(),
                    model: connector.to_string(),
                    refresh: smithay::output::Mode::from(mode).refresh,
                },
                size,
                active: true,
            });
        }
        Err(format!(
            "No native DRM device could be initialized: {}",
            failures.join("; ")
        )
        .into())
    }

    fn initialize_device(
        lease: &DeviceFd,
        connector: Option<&str>,
        session: LibSeatSession,
    ) -> Result<(
        Resources,
        smithay::backend::drm::DrmDeviceNotifier,
        connector::Info,
        crtc::Handle,
        control::Mode,
    )> {
        let fd = DrmDeviceFd::new(lease.clone());
        let (mut drm, notifier) = DrmDevice::new(fd.clone(), true)?;
        let (connector, crtc, mode) = select_connector(&drm, connector)?;
        let gbm = GbmDevice::new(fd)?;
        // SAFETY: the GBM device owns its DRM fd and is retained by EGLDisplay.
        let display = unsafe { EGLDisplay::new(gbm.clone()) }?;
        let context = EGLContext::new(&display)?;
        // SAFETY: this context is current only on the compositor's main thread.
        let renderer = unsafe { GlesRenderer::new(context) }?;
        let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        let drm_surface = drm.create_surface(crtc, mode, &[connector.handle()])?;
        let surface = GbmBufferedSurface::new(
            drm_surface,
            allocator,
            &[Fourcc::Argb8888, Fourcc::Xrgb8888],
            display.dmabuf_render_formats().iter().copied(),
        )?;
        let mut input = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        input
            .udev_assign_seat(&session.seat())
            .map_err(|_| "libinput could not bind the active libseat seat")?;
        Ok((
            Resources {
                surface: Rc::new(RefCell::new(surface)),
                renderer,
                drm: Rc::new(RefCell::new(drm)),
                input,
                target: None,
            },
            notifier,
            connector,
            crtc,
            mode,
        ))
    }

    pub fn output_info(&self) -> &NativeOutput {
        &self.output
    }
    pub fn size(&self) -> Size<i32, Physical> {
        self.size
    }
    pub fn can_render(&self) -> bool {
        self.state
            .rendering_allowed(self.active && self.session.is_active())
    }

    pub fn poll_events(&mut self) -> Result<Vec<NativeEvent>> {
        self.events.dispatch(Duration::ZERO, &mut self.state)?;
        if let Some(error) = self.state.error.take() {
            return Err(error.into());
        }
        let resources = self.resources.as_mut().unwrap();
        let mut events = Vec::new();
        for event in std::mem::take(&mut self.state.sessions) {
            self.state.vt_requested_at = None;
            match event {
                SessionEvent::PauseSession => {
                    resources.input.suspend();
                    resources.drm.borrow_mut().pause();
                    self.active = false;
                    self.state.submitted_at = None;
                    self.state.inputs.clear();
                    events.push(NativeEvent::Activated(false));
                }
                SessionEvent::ActivateSession => {
                    if !self.active {
                        resources.drm.borrow_mut().activate(true)?;
                        // A flip submitted before VT loss may never deliver its
                        // completion. Retire it only after the device is reset.
                        if self.state.pending_flip {
                            resources.surface.borrow_mut().frame_submitted()?;
                        }
                        resources.surface.borrow_mut().reset_buffers();
                        self.state.flip_completed();
                        resources.input.resume().map_err(
                            |_| "libinput could not resume after native seat activation",
                        )?;
                    }
                    self.active = true;
                    events.push(NativeEvent::Activated(true));
                }
            }
        }
        self.state
            .release_noop_vt_wait(self.active && self.session.is_active(), Instant::now());
        if self
            .state
            .flip_expired(self.active && self.session.is_active(), Instant::now())
        {
            return Err("Native DRM page flip did not complete within 3 seconds; check the DRM driver, active seat and connected output".into());
        }
        if self.active {
            events.extend(self.state.inputs.drain(..).map(NativeEvent::Input));
        } else {
            self.state.inputs.clear();
        }
        Ok(events)
    }

    pub fn bind(&mut self) -> Result<(&mut GlesRenderer, GlesTarget<'_>)> {
        if !self.can_render() {
            return Err(
                "Native output is inactive or waiting for a page flip/VT transition".into(),
            );
        }
        let resources = self.resources.as_mut().unwrap();
        if resources.target.is_none() {
            resources.target = Some(resources.surface.borrow_mut().next_buffer()?.0);
        }
        let framebuffer = resources
            .renderer
            .bind(resources.target.as_mut().unwrap())?;
        Ok((&mut resources.renderer, framebuffer))
    }

    pub fn submit(&mut self, damage: Option<&[Rectangle<i32, Physical>]>) -> Result<()> {
        let resources = self.resources.as_mut().unwrap();
        let display = resources.renderer.egl_context().display().clone();
        let sync = resources.renderer.with_context(|gl| {
            if let Ok(fence) = EGLFence::create(&display) {
                // SAFETY: with_context made this thread's EGL context current.
                unsafe {
                    gl.Flush();
                }
                SyncPoint::from(fence)
            } else {
                // SAFETY: finish is the conservative fallback without EGL fences.
                unsafe {
                    gl.Finish();
                }
                SyncPoint::signaled()
            }
        })?;
        resources
            .surface
            .borrow_mut()
            .queue_buffer(Some(sync), damage.map(<[_]>::to_vec), ())?;
        resources.target = None;
        self.state.pending_flip = true;
        self.state.submitted_at = Some(Instant::now());
        Ok(())
    }

    pub fn change_vt(&mut self, vt: i32) -> Result<()> {
        if !(1..=12).contains(&vt) {
            return Err("Native VT must be between 1 and 12".into());
        }
        self.session.change_vt(vt)?;
        // libseat grants/revokes the seat asynchronously. Do not render another
        // frame in the current runtime iteration after requesting the switch.
        self.state.vt_requested_at = Some(Instant::now());
        Ok(())
    }
}

impl Drop for NativeBackend {
    fn drop(&mut self) {
        // Keep the libseat notifier alive until every GPU reference is released
        // and close_device has returned ownership of the device to the seat.
        release_device(
            &self.events.handle(),
            &mut self.sources,
            &mut self.resources,
            &mut self.lease,
            &mut self.session,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_flip_deadline_pauses_with_the_seat_and_clears_on_completion() {
        let start = Instant::now();
        let mut state = EventState {
            pending_flip: true,
            submitted_at: Some(start),
            ..Default::default()
        };
        assert!(!state.flip_expired(true, start + Duration::from_secs(2)));
        assert!(state.flip_expired(true, start + Duration::from_secs(3)));
        assert!(!state.flip_expired(false, start + Duration::from_secs(30)));
        // VT pause removes the clock; an unreported old flip is retired on resume.
        state.submitted_at = None;
        assert!(!state.flip_expired(true, start + Duration::from_secs(30)));
        state.flip_completed();
        assert!(!state.pending_flip);
        assert!(!state.flip_expired(true, start + Duration::from_secs(30)));
    }

    #[test]
    fn successful_vt_request_inhibits_frames_and_recovers_from_a_bounded_noop() {
        let start = Instant::now();
        let mut state = EventState {
            vt_requested_at: Some(start),
            ..Default::default()
        };
        assert!(!state.rendering_allowed(true));
        state.release_noop_vt_wait(true, start + Duration::from_secs(1));
        assert!(!state.rendering_allowed(true));
        state.release_noop_vt_wait(false, start + Duration::from_secs(30));
        assert!(!state.rendering_allowed(false));
        assert!(state.vt_requested_at.is_some());
        state.release_noop_vt_wait(true, start + Duration::from_secs(2));
        assert!(state.rendering_allowed(true));
    }
}
