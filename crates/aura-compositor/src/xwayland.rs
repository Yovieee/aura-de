//! Rootless XWayland lifecycle and X11 window-manager protocol dispatch.
use crate::runtime::App;
use calloop::{LoopHandle, RegistrationToken};
use smithay::{
    delegate_xwayland_shell,
    input::{
        Seat, SeatHandler,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    utils::{IsAlive, Logical, Rectangle, Serial},
    wayland::{
        seat::WaylandFocus,
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandClientData, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};
use std::{
    borrow::Cow,
    error::Error,
    ffi::OsString,
    io,
    os::fd::OwnedFd,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::{DisplayHandle, Resource, protocol::wl_surface::WlSurface};

#[path = "x11_initial_state.rs"]
pub(crate) mod initial_state;

/// The X11 target performs ICCCM input-focus/WM_TAKE_FOCUS handling before
/// forwarding keyboard events to XWayland's underlying Wayland surface.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum KeyboardFocus {
    Wayland(WlSurface),
    X11 {
        window: Box<X11Surface>,
        surface: WlSurface,
    },
}

impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl From<smithay::desktop::PopupKind> for KeyboardFocus {
    fn from(popup: smithay::desktop::PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Wayland(surface) | KeyboardFocus::X11 { surface, .. } => surface,
        }
    }
}

impl KeyboardFocus {
    pub fn wl_surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(surface) => Some(surface.clone()),
            Self::X11 { surface, .. } => Some(surface.clone()),
        }
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => wayland_server::Resource::is_alive(surface),
            Self::X11 { window, surface } => window.alive() && surface.is_alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            Self::X11 { surface, .. } => Some(Cow::Borrowed(surface)),
        }
    }
}

impl<D: SeatHandler + 'static> KeyboardTarget<D> for KeyboardFocus {
    fn enter(&self, seat: &Seat<D>, data: &mut D, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
            Self::X11 { window, .. } => {
                KeyboardTarget::enter(window.as_ref(), seat, data, keys, serial)
            }
        }
    }
    fn leave(&self, seat: &Seat<D>, data: &mut D, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            Self::X11 { window, .. } => KeyboardTarget::leave(window.as_ref(), seat, data, serial),
        }
    }
    fn key(
        &self,
        seat: &Seat<D>,
        data: &mut D,
        key: KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            Self::X11 { window, .. } => {
                KeyboardTarget::key(window.as_ref(), seat, data, key, state, serial, time)
            }
        }
    }
    fn modifiers(&self, seat: &Seat<D>, data: &mut D, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            Self::X11 { window, .. } => {
                KeyboardTarget::modifiers(window.as_ref(), seat, data, modifiers, serial)
            }
        }
    }
}

pub(crate) fn focus_changed(app: &mut App, focused: Option<&WlSurface>) {
    // Ordinary Wayland surfaces have no X11 serial. Never ask the XWM to match
    // them against its still-unpaired X11 windows.
    if let Some(surface) = focused
        && surface
            .client()
            .is_some_and(|client| client.get_data::<XWaylandClientData>().is_some())
        && let Some(wm) = &mut app.xwm
        && let Err(error) = wm.raise_window(surface)
    {
        tracing::warn!(%error, "Could not synchronize X11 window stacking after focus");
    }
}

/// Removing the process event source drops XWayland, disconnecting its special
/// Wayland client. Smithay reaps the child on a background thread.
pub(crate) struct XWaylandGuard {
    handle: LoopHandle<'static, App>,
    token: Option<RegistrationToken>,
    started: Instant,
    _abstract_reservation: OwnedFd,
}

impl XWaylandGuard {
    /// Standalone compositor startup must be bounded as well as supervised startup.
    pub fn check_startup(&self, app: &App) -> Result<(), Box<dyn Error>> {
        if app.xwayland_display.is_none() && self.started.elapsed() >= Duration::from_secs(10) {
            return Err("XWayland did not become ready within 10 seconds".into());
        }
        Ok(())
    }
}

impl Drop for XWaylandGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.handle.remove(token);
        }
    }
}

/// Reserve the abstract namespace without listening. Normal X11 clients fall
/// back to the private filesystem socket, while another X server cannot choose
/// this display. Keep the reservation until the filesystem endpoint is removed.
fn reserve_display() -> Result<(u32, OwnedFd), Box<dyn Error>> {
    reserve_display_in(Path::new("/tmp/.X11-unix"), Path::new("/tmp"))
}

fn reserve_display_in(
    socket_directory: &Path,
    lock_directory: &Path,
) -> Result<(u32, OwnedFd), Box<dyn Error>> {
    use smithay::reexports::rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, socket_with,
    };
    fn absent(path: &std::path::Path) -> io::Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        }
    }
    // Keep Aura's range away from ordinary low-numbered host displays. Never
    // let Smithay's automatic allocator unlink a foreign -displayfd X socket
    // just because that server did not create an X lock file.
    for display in 100..1000 {
        let socket_path = socket_directory.join(format!("X{display}"));
        let lock_path = lock_directory.join(format!(".X{display}-lock"));
        if !absent(&socket_path)? || !absent(&lock_path)? {
            continue;
        }
        let reservation = socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        let address =
            SocketAddrUnix::new_abstract_name(socket_path.as_os_str().as_encoded_bytes())?;
        match bind(&reservation, &address) {
            Ok(()) => {}
            Err(smithay::reexports::rustix::io::Errno::ADDRINUSE) => continue,
            Err(error) => return Err(error.into()),
        }
        if absent(&socket_path)? && absent(&lock_path)? {
            return Ok((display, reservation));
        }
    }
    Err("Could not reserve an unused XWayland display between :100 and :999".into())
}

pub(crate) fn start(
    dh: &DisplayHandle,
    handle: LoopHandle<'static, App>,
) -> Result<XWaylandGuard, Box<dyn Error>> {
    let started = Instant::now();
    // This shared system directory must be provisioned by the host. Creating it
    // under the compositor's private umask would prevent other X servers from
    // working, while following a foreign symlink would defeat socket ownership.
    let directory = std::fs::symlink_metadata("/tmp/.X11-unix").map_err(|error| {
        format!("XWayland requires the host /tmp/.X11-unix directory: {error}; install xserver-common and apply the host's X11 tmpfiles setup")
    })?;
    let owner = directory.uid();
    let uid = smithay::reexports::rustix::process::geteuid().as_raw();
    if !directory.is_dir()
        || (owner != 0 && owner != uid)
        || (directory.mode() & 0o002 != 0 && directory.mode() & 0o1000 == 0)
    {
        return Err("XWayland requires a real, trusted /tmp/.X11-unix directory with sticky permissions when world-writable; install xserver-common and apply the host's X11 tmpfiles setup".into());
    }
    // XWayland has no accelerated client-buffer import in Aura yet. Its SHM
    // path interoperates with the compositor's supported software buffers.
    let mut environment = vec![(OsString::from("XWAYLAND_NO_GLAMOR"), OsString::from("1"))];
    for name in [
        "LD_LIBRARY_PATH",
        "LIBGL_ALWAYS_SOFTWARE",
        "XKB_CONFIG_ROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.push((OsString::from(name), value));
        }
    }
    let (display_number, reservation) = reserve_display()?;
    let (xwayland, client) = XWayland::spawn(
        dh,
        Some(display_number),
        environment,
        false,
        Stdio::null(),
        Stdio::inherit(),
        |_| (),
    )
    .map_err(|error| {
        format!("Could not start XWayland: {error}; install Xwayland and ensure it is on PATH")
    })?;
    // Without an abstract listener, Linux filesystem permissions restrict access
    // to the compositor's uid. Apply them before the XWM accepts X11 clients or
    // a DISPLAY is published to the session. No host Xauthority is inherited.
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{}", xwayland.display_number()));
    let metadata = std::fs::symlink_metadata(&socket)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != smithay::reexports::rustix::process::geteuid().as_raw()
    {
        return Err("XWayland did not create an owned filesystem socket".into());
    }
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let wm_handle = handle.clone();
    let token = handle.insert_source(xwayland, move |event, _, app| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => match X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone()) {
            Ok(wm) => {
                app.xwm = Some(wm);
                match initial_state::InitialStateWorker::new(display_number, wm_handle.clone()) {
                    Ok(worker) => app.x11_metadata_worker = Some(worker),
                    Err(error) => {
                        app.xwayland_error =
                            Some(format!("XWayland metadata worker startup failed: {error}"))
                    }
                }
            }
            Err(error) => {
                app.xwayland_error =
                    Some(format!("XWayland window-manager startup failed: {error}"));
            }
        },
        XWaylandEvent::Error => {
            app.xwayland_error = Some("XWayland exited before becoming ready".into())
        }
    })?;
    Ok(XWaylandGuard {
        handle,
        token: Some(token),
        started,
        _abstract_reservation: reservation,
    })
}

impl XWaylandShellHandler for App {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }
    fn surface_associated(&mut self, _: XwmId, surface: WlSurface, window: X11Surface) {
        self.x11_associated(&window, &surface);
    }
}

impl XwmHandler for App {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        let state = self.xwm.as_mut().expect("XWM event after readiness");
        assert_eq!(state.id(), xwm);
        state
    }
    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}
    fn map_window_request(&mut self, _: XwmId, window: X11Surface) {
        if let Some(worker) = &mut self.x11_metadata_worker {
            if let Err(error) = worker.map(window) {
                self.xwayland_error = Some(error);
            }
        } else {
            self.xwayland_error =
                Some("XWayland requested a map before its metadata worker was ready".into());
        }
    }
    fn map_window_notify(&mut self, _: XwmId, window: X11Surface) {
        self.x11_map(&window);
    }
    fn mapped_override_redirect_window(&mut self, _: XwmId, window: X11Surface) {
        self.x11_map(&window);
    }
    fn unmapped_window(&mut self, _: XwmId, window: X11Surface) {
        if let Some(worker) = &mut self.x11_metadata_worker {
            worker.cancel(&window);
        }
        self.x11_remove(&window);
    }
    fn destroyed_window(&mut self, _: XwmId, window: X11Surface) {
        if let Some(worker) = &mut self.x11_metadata_worker {
            worker.cancel(&window);
        }
        self.x11_remove(&window);
    }
    fn configure_request(
        &mut self,
        _: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _: Option<Reorder>,
    ) {
        self.x11_configure(&window, x, y, width, height);
    }
    fn configure_notify(
        &mut self,
        _: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        self.x11_configure_notify(&window, geometry, above);
    }
    fn property_notify(&mut self, _: XwmId, window: X11Surface, _: WmWindowProperty) {
        self.x11_metadata(&window);
    }
    fn maximize_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_maximize(&window, true);
    }
    fn unmaximize_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_maximize(&window, false);
    }
    fn fullscreen_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_fullscreen(&window, true);
    }
    fn unfullscreen_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_fullscreen(&window, false);
    }
    fn minimize_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_minimize(&window);
    }
    fn unminimize_request(&mut self, _: XwmId, window: X11Surface) {
        self.x11_restore(&window);
    }
    fn move_request(&mut self, _: XwmId, window: X11Surface, button: u32) {
        self.x11_move(&window, button);
    }
    fn resize_request(&mut self, _: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        let edge = match edge {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
        };
        self.x11_resize(&window, button, edge);
    }
    fn allow_selection_access(&mut self, _: XwmId, _: SelectionTarget) -> bool {
        // No cross-protocol clipboard bridge is advertised until ownership and
        // transfer cancellation are implemented and tested.
        false
    }
    fn disconnected(&mut self, _: XwmId) {
        self.xwayland_display = None;
        self.xwayland_error = Some("XWayland window-manager connection closed".into());
    }
}

delegate_xwayland_shell!(App);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn display_allocation_preserves_a_foreign_socket_without_a_lock_and_reserves_its_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let sockets = directory.path().join("sockets");
        std::fs::create_dir(&sockets).unwrap();
        let foreign_path = sockets.join("X100");
        let foreign = UnixListener::bind(&foreign_path).unwrap();
        let original_inode = std::fs::metadata(&foreign_path).unwrap().ino();
        assert!(!directory.path().join(".X100-lock").exists());
        let (first, reservation) = reserve_display_in(&sockets, directory.path()).unwrap();
        assert_eq!(first, 101);
        assert_eq!(
            std::fs::metadata(&foreign_path).unwrap().ino(),
            original_inode
        );
        UnixStream::connect(&foreign_path).unwrap();
        let (second, second_reservation) = reserve_display_in(&sockets, directory.path()).unwrap();
        assert_eq!(
            second, 102,
            "the first abstract reservation must remain exclusive"
        );
        drop(second_reservation);
        drop(reservation);
        drop(foreign);
    }
}
