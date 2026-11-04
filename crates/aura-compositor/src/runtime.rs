//! Real Wayland protocols, input routing and nested EGL presentation.
use crate::{
    Config,
    glass::{GlassPanel, GlassRenderer},
};
use aura_compositor::state::AuraCompositorState;
use aura_core::{GlassQualityPreset, LayoutMode, Rect, TrafficLightAction, WindowStateFlags};
use smithay::wayland::shell::xdg::SurfaceCachedState as XdgSurfaceCachedState;
use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, ButtonState, Event, InputEvent, KeyboardKeyEvent,
            PointerAxisEvent, PointerButtonEvent,
        },
        renderer::{
            Color32F, Frame, Renderer,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
            utils::{draw_render_elements, on_commit_buffer_handler, with_renderer_surface_state},
        },
        winit::{self, WinitEvent},
    },
    delegate_compositor, delegate_data_device, delegate_layer_shell, delegate_output,
    delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{
        LayerSurface as DesktopLayer, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab,
        WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
        utils::under_from_surface_tree,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, Focus, MotionEvent},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    utils::{Logical, Point, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            TraversalAction, get_parent, with_states, with_surface_tree_downward,
        },
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceCachedState,
                LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                XdgToplevelSurfaceData,
            },
        },
        shm::{ShmHandler, ShmState},
    },
};
use std::{
    collections::HashSet,
    error::Error,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, io::OwnedFd, net::UnixListener},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::{
    Client, Display, ListeningSocket, Resource,
    backend::{ClientData, ClientId, DisconnectReason},
    protocol::{wl_buffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface},
};

struct Toplevel {
    id: u64,
    surface: ToplevelSurface,
    mapped: bool,
}
struct Drag {
    id: u64,
    origin: Point<f64, Logical>,
    rect: Rect,
    edges: Option<xdg_toplevel::ResizeEdge>,
}
struct Command {
    text: String,
    response: mpsc::Sender<String>,
}

struct App {
    dh: wayland_server::DisplayHandle,
    compositor: CompositorState,
    xdg: XdgShellState,
    shm: ShmState,
    seats: SeatState<Self>,
    data: DataDeviceState,
    layers: WlrLayerShellState,
    layer_views: Vec<DesktopLayer>,
    mapped_layer_surfaces: HashSet<WlSurface>,
    seat: Seat<Self>,
    output: Output,
    model: AuraCompositorState,
    toplevels: Vec<Toplevel>,
    next_id: u64,
    popups: PopupManager,
    pointer: Point<f64, Logical>,
    drag: Option<Drag>,
    scale: i32,
    glass_ready: bool,
}

impl App {
    fn id(&self, surface: &ToplevelSurface) -> Option<u64> {
        self.toplevels
            .iter()
            .find(|t| t.surface == *surface)
            .map(|t| t.id)
    }
    fn exclusive_layer(&self) -> Option<WlSurface> {
        let map = layer_map_for_output(&self.output);
        for layer in [Layer::Overlay, Layer::Top] {
            if let Some(surface) = map.layers_on(layer).rev().find(|l| {
                with_renderer_surface_state(l.wl_surface(), |s| s.buffer().is_some())
                    .unwrap_or(false)
                    && with_states(l.wl_surface(), |s| {
                        s.cached_state
                            .get::<LayerSurfaceCachedState>()
                            .current()
                            .keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                    })
            }) {
                return Some(surface.wl_surface().clone());
            }
        }
        None
    }
    fn arrange_layers(&mut self) {
        let mut map = layer_map_for_output(&self.output);
        map.arrange();
        let zone = map.non_exclusive_zone();
        drop(map);
        self.model.workspace.menu_bar_height = zone.loc.y as f32;
        self.model.workspace.dock_reserved_height =
            (self.model.workspace.output_rect.height - (zone.loc.y + zone.size.h) as f32).max(0.0);
        self.model.workspace.recompute_layout();
        self.configure_windows();
    }
    fn owning_toplevel(&self, surface: &WlSurface) -> Option<u64> {
        let mut surface = surface.clone();
        for _ in 0..64 {
            if let Some(top) = self
                .toplevels
                .iter()
                .find(|t| t.surface.wl_surface() == &surface)
            {
                return Some(top.id);
            }
            if let Some(popup) = self.popups.find_popup(&surface) {
                surface = find_popup_root_surface(&popup).ok()?;
            } else {
                surface = get_parent(&surface)?;
            }
        }
        None
    }
    fn configure_windows(&mut self) {
        for node in self.model.workspace.windows() {
            if let Some(top) = self.toplevels.iter().find(|t| t.id == node.id) {
                top.surface.with_pending_state(|state| {
                    state.size = Some(
                        (
                            node.computed_rect.width.round() as i32,
                            node.computed_rect.height.round() as i32,
                        )
                            .into(),
                    );
                    for (flag, value) in [
                        (WindowStateFlags::FOCUSED, xdg_toplevel::State::Activated),
                        (WindowStateFlags::MAXIMIZED, xdg_toplevel::State::Maximized),
                        (
                            WindowStateFlags::FULLSCREEN,
                            xdg_toplevel::State::Fullscreen,
                        ),
                    ] {
                        if node.flags.contains(flag) {
                            state.states.set(value);
                        } else {
                            state.states.unset(value);
                        }
                    }
                });
                top.surface.send_pending_configure();
            }
        }
    }
    fn focus(&mut self, id: u64) {
        if self.model.workspace.focus_window(id) {
            if let Some(pos) = self.toplevels.iter().position(|t| t.id == id) {
                let top = self.toplevels.remove(pos);
                let surface = top.surface.wl_surface().clone();
                self.toplevels.push(top);
                self.seat.get_keyboard().unwrap().set_focus(
                    self,
                    self.exclusive_layer().or(Some(surface)),
                    SERIAL_COUNTER.next_serial(),
                );
            }
            self.configure_windows();
        }
    }
    fn sync_focus(&mut self) {
        let surface = self
            .model
            .workspace
            .windows()
            .iter()
            .find(|w| {
                w.flags.contains(WindowStateFlags::FOCUSED)
                    && !w.flags.contains(WindowStateFlags::MINIMIZED)
            })
            .and_then(|w| self.toplevels.iter().find(|t| t.id == w.id))
            .map(|t| t.surface.wl_surface().clone());
        self.seat.get_keyboard().unwrap().set_focus(
            self,
            self.exclusive_layer().or(surface),
            SERIAL_COUNTER.next_serial(),
        );
        self.configure_windows();
    }
    fn action(&mut self, id: u64, action: TrafficLightAction) {
        if action == TrafficLightAction::Close {
            if let Some(t) = self.toplevels.iter().find(|t| t.id == id) {
                t.surface.send_close();
            }
        } else {
            self.model.handle_traffic_light(id, action);
            self.sync_focus();
        }
    }
    fn metadata(&mut self, surface: &ToplevelSurface) {
        if let Some(id) = self.id(surface) {
            let (app, title) = with_states(surface.wl_surface(), |s| {
                let data = s
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap();
                (
                    data.app_id.clone().unwrap_or_default(),
                    data.title.clone().unwrap_or_default(),
                )
            });
            self.model.workspace.update_window_metadata(id, app, title);
        }
    }
    fn surface_under(&self) -> Option<(WlSurface, Point<f64, Logical>)> {
        let map = layer_map_for_output(&self.output);
        for layer in [Layer::Overlay, Layer::Top] {
            if let Some(surface) = map.layer_under(layer, self.pointer) {
                let loc = map.layer_geometry(surface)?.loc.to_f64();
                if let Some((surface, offset)) =
                    surface.surface_under(self.pointer - loc, WindowSurfaceType::ALL)
                {
                    return Some((surface, loc + offset.to_f64()));
                }
            }
        }
        drop(map);
        for top in self.toplevels.iter().rev().filter(|t| t.mapped) {
            if let Some(node) = self
                .model
                .workspace
                .windows()
                .iter()
                .find(|w| w.id == top.id && !w.flags.contains(WindowStateFlags::MINIMIZED))
            {
                let rect = self
                    .model
                    .blur_regions
                    .iter()
                    .find(|r| r.window_id == node.id)
                    .map(|r| r.rect)
                    .unwrap_or(node.computed_rect);
                let loc: Point<i32, Logical> = (rect.x as i32, rect.y as i32).into();
                let geometry = with_states(top.surface.wl_surface(), |s| {
                    s.cached_state
                        .get::<XdgSurfaceCachedState>()
                        .current()
                        .geometry
                        .unwrap_or_default()
                });
                for (popup, location) in PopupManager::popups_for_surface(top.surface.wl_surface())
                {
                    if let Some((surface, offset)) = under_from_surface_tree(
                        popup.wl_surface(),
                        self.pointer,
                        loc + geometry.loc + location - popup.geometry().loc,
                        WindowSurfaceType::ALL,
                    ) {
                        return Some((surface, offset.to_f64()));
                    }
                }
                if let Some((surface, offset)) = under_from_surface_tree(
                    top.surface.wl_surface(),
                    self.pointer,
                    loc,
                    WindowSurfaceType::ALL,
                ) {
                    return Some((surface, offset.to_f64()));
                }
            }
        }
        None
    }
    fn start_drag(
        &mut self,
        surface: &ToplevelSurface,
        serial: Serial,
        seat: WlSeat,
        edges: Option<xdg_toplevel::ResizeEdge>,
    ) {
        if Seat::from_resource(&seat).as_ref() != Some(&self.seat) {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        if !pointer.has_grab(serial) {
            return;
        }
        let valid_client = pointer
            .grab_start_data()
            .and_then(|d| d.focus)
            .is_some_and(|(focus, _)| focus.id().same_client_as(&surface.wl_surface().id()));
        if !valid_client {
            return;
        }
        if let Some(id) = self.id(surface)
            && let Some(node) = self.model.workspace.windows().iter().find(|w| w.id == id)
        {
            self.drag = Some(Drag {
                id,
                origin: self.pointer,
                rect: node.computed_rect,
                edges,
            });
        }
    }
    fn control(&mut self, text: &str) -> Result<String, String> {
        let parts: Vec<_> = text.split_whitespace().collect();
        match parts.as_slice() {
            ["status"] => Ok(serde_json::json!({
                "layout": match self.model.workspace.mode { LayoutMode::Floating=>"floating",LayoutMode::StageManager=>"stage-manager",LayoutMode::DynamicTiling=>"tiling" },
                "quality": if self.model.glass_material.kawase_blur_passes==0 {"reduced-transparency"} else if self.model.glass_material.chromatic_dispersion==0.0 {"balanced"} else {"ultra"},
                "mapped_windows": self.toplevels.iter().filter(|t|t.mapped).count(),
                "visible_windows": self.model.workspace.windows().iter().filter(|w|!w.flags.contains(WindowStateFlags::MINIMIZED)).count(),
                "reserved_top": self.model.workspace.menu_bar_height, "reserved_bottom":self.model.workspace.dock_reserved_height, "layers": self.layers.layer_surfaces().count(), "mapped_layers":self.layers.layer_surfaces().filter(|l|with_renderer_surface_state(l.wl_surface(),|s|s.buffer().is_some()).unwrap_or(false)).count(), "glass_active":self.glass_ready
            }).to_string()),
            ["windows"] => Ok(serde_json::Value::Array(self.model.workspace.windows().iter().map(|w| serde_json::json!({"id":w.id,"app_id":w.app_id,"title":w.title,"minimized":w.flags.contains(WindowStateFlags::MINIMIZED),"focused":w.flags.contains(WindowStateFlags::FOCUSED),"rect":{"x":w.computed_rect.x,"y":w.computed_rect.y,"width":w.computed_rect.width,"height":w.computed_rect.height}})).collect()).to_string()),
            ["layout", mode] => { self.model.set_layout_mode(match *mode { "floating"=>LayoutMode::Floating,"stage-manager"=>LayoutMode::StageManager,"tiling"=>LayoutMode::DynamicTiling,_=>return Err("invalid layout".into()) }); self.configure_windows(); Ok("OK".into()) }
            ["quality", quality] => { self.model.glass_material = aura_core::LiquidGlassMaterial::from_preset(match *quality { "ultra"=>GlassQualityPreset::Ultra,"balanced"=>GlassQualityPreset::Balanced,"reduced-transparency"=>GlassQualityPreset::ReducedTransparency,_=>return Err("invalid quality".into()) }); Ok("OK".into()) }
            [command @ ("focus"|"restore"|"minimize"|"close"), id] => {
                let id=id.parse::<u64>().map_err(|_| "invalid window id")?;
                if !self.model.workspace.windows().iter().any(|w| w.id==id) { return Err("window not found".into()); }
                match *command { "focus"=>self.focus(id), "restore"=>{self.model.workspace.restore_window(id);self.focus(id);}, "minimize"=>self.action(id,TrafficLightAction::MinimizeToDock),_=>self.action(id,TrafficLightAction::Close) }
                Ok("OK".into())
            }
            ["quit"] => { self.model.should_exit=true; Ok("OK".into()) }
            _ => Err("unknown command".into()),
        }
    }
    fn resize(&mut self, size: smithay::utils::Size<i32, smithay::utils::Physical>) {
        let mode = Mode {
            size,
            refresh: 60_000,
        };
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        self.model.workspace.resize_output(Rect::new(
            0.,
            0.,
            size.w as f32 / self.scale as f32,
            size.h as f32 / self.scale as f32,
        ));
        self.arrange_layers();
    }
}

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _: &wl_buffer::WlBuffer) {}
}
impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl OutputHandler for App {}
impl SelectionHandler for App {
    type SelectionUserData = ();
}
impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data
    }
}
impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {
    fn send(&mut self, _: String, _: OwnedFd, _: Seat<Self>) {}
}
impl SeatHandler for App {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        set_data_device_focus(&self.dh, seat, focused.and_then(|s| s.client()));
    }
    fn cursor_image(&mut self, _: &Seat<Self>, _: CursorImageStatus) {}
}
impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, c: &'a Client) -> &'a CompositorClientState {
        &c.get_data::<ClientState>().unwrap().compositor
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);
        let has_buffer =
            with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
        if let Some(pos) = self
            .toplevels
            .iter()
            .position(|t| t.surface.wl_surface() == surface)
        {
            let top = &mut self.toplevels[pos];
            let id = top.id;
            let was_mapped = top.mapped;
            top.mapped = has_buffer;
            let shell = top.surface.clone();
            if has_buffer && !was_mapped {
                let size = with_renderer_surface_state(surface, |s| s.buffer_size())
                    .flatten()
                    .unwrap_or_else(|| (640, 480).into());
                self.model
                    .map_xdg_window(id, "", "", (size.w as f32, size.h as f32));
                self.output.enter(surface);
                self.metadata(&shell);
                self.focus(id);
            } else if !has_buffer && was_mapped {
                self.model.workspace.remove_window(id);
                self.model.window_springs.remove(&id);
                self.output.leave(surface);
                self.drag = None;
                self.sync_focus();
            }
            if !has_buffer && !shell.is_initial_configure_sent() {
                shell.with_pending_state(|s| s.size = Some((640, 480).into()));
                shell.send_configure();
            }
        }
        if let Some(layer) = self
            .layers
            .layer_surfaces()
            .find(|l| l.wl_surface() == surface)
        {
            let was_mapped = self.mapped_layer_surfaces.contains(surface);
            let view = self
                .layer_views
                .iter()
                .find(|l| l.layer_surface() == &layer)
                .cloned();
            if !has_buffer && was_mapped {
                self.mapped_layer_surfaces.remove(surface);
                if let Some(view) = view {
                    layer_map_for_output(&self.output).unmap_layer(&view);
                }
                with_states(surface, |s| {
                    if let Some(data) = s.data_map.get::<LayerSurfaceData>() {
                        let mut attributes = data.lock().unwrap();
                        attributes.configured = false;
                        attributes.initial_configure_sent = false;
                        attributes.last_acked = None;
                        attributes.current = Default::default();
                    }
                    let mut cached = s.cached_state.get::<LayerSurfaceCachedState>();
                    *cached.current() = Default::default();
                    *cached.pending() = Default::default();
                });
            } else {
                if let Some(view) = view {
                    let _ = layer_map_for_output(&self.output).map_layer(&view);
                }
                if has_buffer {
                    self.mapped_layer_surfaces.insert(surface.clone());
                }
                self.arrange_layers();
                let needs_initial = with_states(surface, |s| {
                    s.data_map
                        .get::<LayerSurfaceData>()
                        .is_some_and(|d| !d.lock().unwrap().initial_configure_sent)
                });
                if needs_initial {
                    layer.send_configure();
                }
            }
            self.arrange_layers();
            self.sync_focus();
        }
    }
}
impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| {
            s.size = Some((640, 480).into());
        });
        let id = self.next_id;
        self.next_id += 1;
        self.toplevels.push(Toplevel {
            id,
            surface,
            mapped: false,
        });
    }
    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
        });
        let _ = surface.send_configure();
        if let Err(error) = self.popups.track_popup(surface.into()) {
            tracing::warn!(%error,"popup tracking failed");
        }
    }
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        if Seat::from_resource(&seat).as_ref() != Some(&self.seat) {
            surface.send_popup_done();
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        if !pointer.has_grab(serial) {
            surface.send_popup_done();
            return;
        }
        let popup = PopupKind::Xdg(surface.clone());
        let Ok(root) = find_popup_root_surface(&popup) else {
            surface.send_popup_done();
            return;
        };
        if !pointer
            .grab_start_data()
            .and_then(|d| d.focus)
            .is_some_and(|(focus, _)| focus.id().same_client_as(&root.id()))
        {
            surface.send_popup_done();
            return;
        }
        if let Ok(grab) = self.popups.grab_popup(root, popup, &self.seat, serial) {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| s.geometry = positioner.get_geometry());
        surface.send_repositioned(token);
    }
    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        self.start_drag(&surface, serial, seat, None);
    }
    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        self.start_drag(&surface, serial, seat, Some(edges));
    }
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id(&surface) {
            self.action(id, TrafficLightAction::MinimizeToDock);
        }
    }
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id(&surface)
            && self
                .model
                .workspace
                .windows()
                .iter()
                .any(|w| w.id == id && !w.flags.contains(WindowStateFlags::MAXIMIZED))
        {
            self.action(id, TrafficLightAction::ToggleZoomFullscreen);
        }
    }
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id(&surface)
            && self
                .model
                .workspace
                .windows()
                .iter()
                .any(|w| w.id == id && w.flags.contains(WindowStateFlags::MAXIMIZED))
        {
            self.action(id, TrafficLightAction::ToggleZoomFullscreen);
        }
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id(&surface) {
            self.model.workspace.remove_window(id);
            self.model.window_springs.remove(&id);
        }
        self.toplevels.retain(|t| t.surface != surface);
        self.sync_focus();
    }
    fn popup_destroyed(&mut self, _: PopupSurface) {
        self.popups.cleanup();
    }
    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
}
impl WlrLayerShellHandler for App {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layers
    }
    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _: Option<WlOutput>,
        _: Layer,
        namespace: String,
    ) {
        let layer = DesktopLayer::new(surface.clone(), namespace);
        self.layer_views.push(layer.clone());
        if let Err(e) = layer_map_for_output(&self.output).map_layer(&layer) {
            tracing::warn!(%e,"layer mapping failed");
            surface.send_close();
        }
    }
    fn layer_destroyed(&mut self, surface: LayerSurface) {
        self.mapped_layer_surfaces.remove(surface.wl_surface());
        self.layer_views.retain(|l| l.layer_surface() != &surface);
        let mut map = layer_map_for_output(&self.output);
        let layer = map
            .layers()
            .find(|l| l.layer_surface() == &surface)
            .cloned();
        if let Some(layer) = layer {
            map.unmap_layer(&layer);
        }
        drop(map);
        self.arrange_layers();
        self.sync_focus();
    }
}
#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}
delegate_compositor!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_seat!(App);
delegate_data_device!(App);
delegate_output!(App);
delegate_layer_shell!(App);

fn send_frames(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, s, &()| {
            for cb in s
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                cb.done(time);
            }
        },
        |_, _, &()| true,
    );
}

struct ControlServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
fn control_server(
    sender: calloop::channel::Sender<Command>,
) -> Result<ControlServer, Box<dyn Error>> {
    let path = std::env::var_os("AURA_CONTROL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default())
                .join("aura-control.sock")
        });
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let thread = std::thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                    let mut text = String::new();
                    if BufReader::new(&stream)
                        .take(4096)
                        .read_line(&mut text)
                        .is_ok()
                    {
                        let (tx, rx) = mpsc::channel();
                        if sender.send(Command { text, response: tx }).is_ok()
                            && let Ok(response) = rx.recv_timeout(Duration::from_millis(500))
                        {
                            let _ = writeln!(stream, "{response}");
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    Ok(ControlServer {
        path,
        stop,
        thread: Some(thread),
    })
}

pub(crate) fn run(config: Config) -> Result<(), Box<dyn Error>> {
    let mut display = Display::<App>::new()?;
    let mut dh = display.handle();
    let mut seats = SeatState::new();
    let mut seat = seats.new_wl_seat(&dh, "aura-seat");
    seat.add_keyboard(Default::default(), 200, 25)?;
    seat.add_pointer();
    let output = Output::new(
        "AURA-1".into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "Aura".into(),
            model: "Nested".into(),
        },
    );
    output.create_global::<App>(&dh);
    let mode = Mode {
        size: config.size.into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(config.scale)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let listener = ListeningSocket::bind(&config.socket)?;
    let mut app = App {
        dh: dh.clone(),
        compositor: CompositorState::new::<App>(&dh),
        xdg: XdgShellState::new::<App>(&dh),
        shm: ShmState::new::<App>(&dh, vec![]),
        data: DataDeviceState::new::<App>(&dh),
        layers: WlrLayerShellState::new::<App>(&dh),
        layer_views: Vec::new(),
        mapped_layer_surfaces: HashSet::new(),
        seats,
        seat,
        output,
        model: AuraCompositorState::new(
            config.size.0 as f32 / config.scale as f32,
            config.size.1 as f32 / config.scale as f32,
            config.quality,
        ),
        toplevels: vec![],
        next_id: 1,
        popups: PopupManager::default(),
        pointer: (0., 0.).into(),
        drag: None,
        scale: config.scale,
        glass_ready: false,
    };
    app.model.socket_name = config.socket.clone();
    app.model.workspace.menu_bar_height = 0.;
    app.model.workspace.dock_reserved_height = 0.;
    let mut events = calloop::EventLoop::<App>::try_new()?;
    let (tx, rx) = calloop::channel::channel::<Command>();
    events.handle().insert_source(rx, |event, _, app| {
        if let calloop::channel::Event::Msg(command) = event {
            let result = app
                .control(&command.text)
                .unwrap_or_else(|e| format!("ERR {e}"));
            let _ = command.response.send(result);
        }
    })?;
    let _control = control_server(tx)?;
    let attributes = ::winit::window::Window::default_attributes()
        .with_title("Aura DE — Nested")
        .with_inner_size(::winit::dpi::PhysicalSize::new(
            config.size.0 as u32,
            config.size.1 as u32,
        ));
    let (mut backend, mut input) = winit::init_from_attributes::<GlesRenderer>(attributes)?;
    tracing::info!(socket=%config.socket,"Wayland server listening; basic GLES renderer active");
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    let start = Instant::now();
    let mut last = start;
    let mut presented = 0u64;
    let mut glass_renderer: Option<GlassRenderer> = None;
    let mut glass_initialization_attempted = false;
    while !app.model.should_exit && !stop.load(Ordering::Relaxed) {
        let status = input.dispatch_new_events(|event| match event {
            WinitEvent::Resized { .. } => {}
            WinitEvent::Input(event) => handle_input(&mut app, event),
            WinitEvent::CloseRequested => app.model.should_exit = true,
            _ => {}
        });
        if matches!(status, ::winit::platform::pump_events::PumpStatus::Exit(_)) {
            break;
        }
        while let Some(stream) = listener.accept()? {
            dh.insert_client(stream, Arc::new(ClientState::default()))?;
        }
        display.dispatch_clients(&mut app)?;
        app.popups.cleanup();
        events.dispatch(Duration::from_millis(8), &mut app)?;
        let size = backend.window_size();
        if app.output.current_mode().is_none_or(|m| m.size != size) {
            app.resize(size);
        }
        let now = Instant::now();
        app.model.tick_frame((now - last).as_secs_f32());
        last = now;
        let damage = smithay::utils::Rectangle::from_size(size);
        let mut rendered = Vec::new();
        {
            let (renderer, mut framebuffer) = backend.bind()?;
            let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            let mut panels = Vec::new();
            let map = layer_map_for_output(&app.output);
            for layer in [Layer::Overlay, Layer::Top] {
                for surface in map.layers_on(layer).rev() {
                    if !with_renderer_surface_state(surface.wl_surface(), |s| s.buffer().is_some())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if let Some(geo) = map.layer_geometry(surface) {
                        if matches!(
                            surface.namespace(),
                            "aura-menu-bar" | "aura-dock" | "aura-launcher"
                        ) {
                            let mut material = app.model.glass_material;
                            if surface.namespace() == "aura-menu-bar" {
                                material.corner_radius = 0.;
                                material.bevel_width = 4.;
                            }
                            if surface.namespace() == "aura-launcher" {
                                material.tint_rgba = [0.10, 0.12, 0.16, 0.42];
                            }
                            panels.push(GlassPanel {
                                rect: Rect::new(
                                    (geo.loc.x * config.scale) as f32,
                                    (geo.loc.y * config.scale) as f32,
                                    (geo.size.w * config.scale) as f32,
                                    (geo.size.h * config.scale) as f32,
                                ),
                                material,
                                elevation_px: 16.,
                            });
                        }
                        elements.extend(render_elements_from_surface_tree(
                            renderer,
                            surface.wl_surface(),
                            geo.loc.to_physical(config.scale),
                            config.scale as f64,
                            1.,
                            Kind::Unspecified,
                        ));
                        rendered.push(surface.wl_surface().clone());
                    }
                }
            }
            let foreground_count = elements.len();
            for top in app.toplevels.iter().rev().filter(|t| t.mapped) {
                if let Some(node) = app
                    .model
                    .workspace
                    .windows()
                    .iter()
                    .find(|w| w.id == top.id && !w.flags.contains(WindowStateFlags::MINIMIZED))
                {
                    let rect = app
                        .model
                        .blur_regions
                        .iter()
                        .find(|r| r.window_id == node.id)
                        .map(|r| r.rect)
                        .unwrap_or(node.computed_rect);
                    let loc: Point<i32, Logical> = (rect.x as i32, rect.y as i32).into();
                    let geometry = with_states(top.surface.wl_surface(), |s| {
                        s.cached_state
                            .get::<XdgSurfaceCachedState>()
                            .current()
                            .geometry
                            .unwrap_or_default()
                    });
                    for (popup, location) in
                        PopupManager::popups_for_surface(top.surface.wl_surface())
                    {
                        let offset = loc + geometry.loc + location - popup.geometry().loc;
                        elements.extend(render_elements_from_surface_tree(
                            renderer,
                            popup.wl_surface(),
                            offset.to_physical(config.scale),
                            config.scale as f64,
                            1.,
                            Kind::Unspecified,
                        ));
                        rendered.push(popup.wl_surface().clone());
                    }
                    elements.extend(render_elements_from_surface_tree(
                        renderer,
                        top.surface.wl_surface(),
                        loc.to_physical(config.scale),
                        config.scale as f64,
                        1.,
                        Kind::Unspecified,
                    ));
                    rendered.push(top.surface.wl_surface().clone());
                }
            }
            for layer in [Layer::Bottom, Layer::Background] {
                for surface in map.layers_on(layer).rev() {
                    if !with_renderer_surface_state(surface.wl_surface(), |s| s.buffer().is_some())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if let Some(geo) = map.layer_geometry(surface) {
                        elements.extend(render_elements_from_surface_tree(
                            renderer,
                            surface.wl_surface(),
                            geo.loc.to_physical(config.scale),
                            config.scale as f64,
                            1.,
                            Kind::Unspecified,
                        ));
                        rendered.push(surface.wl_surface().clone());
                    }
                }
            }
            drop(map);
            let has_client = !elements.is_empty() && app.toplevels.iter().any(|t| t.mapped);
            if !glass_initialization_attempted {
                glass_initialization_attempted = true;
                match GlassRenderer::new(renderer, size) {
                    Ok(glass) => glass_renderer = Some(glass),
                    Err(error) => {
                        tracing::warn!(%error,"glass initialization failed; using opaque panels")
                    }
                }
            }
            let (foreground, scene) = elements.split_at(foreground_count);
            let result = glass_renderer.as_mut().map(|glass| {
                glass.render_scene(
                    renderer,
                    &mut framebuffer,
                    size,
                    config.scale as f64,
                    scene,
                    foreground,
                    &panels,
                    &app.model.glass_material,
                    Color32F::new(0.065, 0.085, 0.14, 1.),
                )
            });
            app.glass_ready = matches!(result, Some(Ok(())));
            if !app.glass_ready {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error,"glass render failed; using opaque panel fallback");
                }
                let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
                frame.clear(Color32F::new(0.065, 0.085, 0.14, 1.), &[damage])?;
                draw_render_elements(&mut frame, config.scale as f64, scene, &[damage])?;
                for panel in &panels {
                    let r = smithay::utils::Rectangle::new(
                        (panel.rect.x as i32, panel.rect.y as i32).into(),
                        (panel.rect.width as i32, panel.rect.height as i32).into(),
                    );
                    frame.clear(
                        Color32F::new(
                            panel.material.tint_rgba[0],
                            panel.material.tint_rgba[1],
                            panel.material.tint_rgba[2],
                            1.,
                        ),
                        &[r],
                    )?;
                }
                draw_render_elements(&mut frame, config.scale as f64, foreground, &[damage])?;
                let _sync = frame.finish()?;
            }
            if has_client {
                presented += 1;
            }
        }
        backend.submit(Some(&[damage]))?;
        for surface in rendered {
            send_frames(&surface, start.elapsed().as_millis() as u32);
        }
        display.flush_clients()?;
        // Let the real client finish reading callbacks and destroy its role.
        // Closing the server immediately after flush can reset the client's socket
        // while its next buffer commit is still in flight.
        if config.smoke && presented >= 2 && !app.toplevels.iter().any(|t| t.mapped) {
            tracing::info!(
                presented,
                "Smoke test: real client buffers rendered and frame callbacks sent"
            );
            return Ok(());
        }
        if config.smoke && start.elapsed() > Duration::from_secs(15) {
            return Err("smoke timed out without real client frames".into());
        }
    }
    if config.smoke && presented < 2 {
        return Err("compositor closed before smoke completed".into());
    }
    Ok(())
}

fn handle_input(app: &mut App, event: InputEvent<smithay::backend::winit::WinitInput>) {
    match event {
        InputEvent::Keyboard { event } => {
            let keyboard = app.seat.get_keyboard().unwrap();
            keyboard.input::<(), _>(
                app,
                event.key_code(),
                event.state(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
                |app, mods, key| {
                    if event.state() == smithay::backend::input::KeyState::Pressed && mods.logo {
                        if key.modified_sym() == keysyms::KEY_Escape.into() {
                            app.model.should_exit = true;
                            return FilterResult::Intercept(());
                        }
                        if key.modified_sym() == keysyms::KEY_q.into() {
                            if let Some(id) = app
                                .model
                                .workspace
                                .windows()
                                .iter()
                                .find(|w| w.flags.contains(WindowStateFlags::FOCUSED))
                                .map(|w| w.id)
                            {
                                app.action(id, TrafficLightAction::Close);
                            }
                            return FilterResult::Intercept(());
                        }
                    }
                    FilterResult::Forward
                },
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let size = app
                .output
                .current_mode()
                .unwrap()
                .size
                .to_logical(app.scale);
            app.pointer = event.position_transformed(size);
            if let Some(drag) = &app.drag {
                let dx = (app.pointer.x - drag.origin.x) as f32;
                let dy = (app.pointer.y - drag.origin.y) as f32;
                let mut r = drag.rect;
                if let Some(edges) = drag.edges {
                    let e = edges as u32;
                    if e & 4 != 0 {
                        r.x += dx;
                        r.width -= dx;
                    }
                    if e & 8 != 0 {
                        r.width += dx;
                    }
                    if e & 1 != 0 {
                        r.y += dy;
                        r.height -= dy;
                    }
                    if e & 2 != 0 {
                        r.height += dy;
                    }
                } else {
                    r.x += dx;
                    r.y += dy;
                }
                let id = drag.id;
                app.model.workspace.mode = LayoutMode::Floating;
                app.model.workspace.set_floating_rect(id, r);
                app.configure_windows();
            }
            let focus = app.surface_under();
            let pointer = app.seat.get_pointer().unwrap();
            pointer.motion(
                app,
                focus,
                &MotionEvent {
                    location: app.pointer,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            pointer.frame(app);
        }
        InputEvent::PointerButton { event } => {
            let pointer = app.seat.get_pointer().unwrap();
            pointer.motion(
                app,
                app.surface_under(),
                &MotionEvent {
                    location: app.pointer,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            if event.state() == ButtonState::Pressed {
                if let Some((surface, _)) = app.surface_under() {
                    if let Some(id) = app.owning_toplevel(&surface) {
                        app.focus(id);
                    } else {
                        let interactive = with_states(&surface, |s| {
                            s.cached_state
                                .get::<LayerSurfaceCachedState>()
                                .current()
                                .keyboard_interactivity
                        });
                        if interactive != KeyboardInteractivity::None {
                            app.seat.get_keyboard().unwrap().set_focus(
                                app,
                                app.exclusive_layer().or(Some(surface)),
                                SERIAL_COUNTER.next_serial(),
                            );
                        }
                    }
                }
            } else {
                app.drag = None;
            }
            pointer.button(
                app,
                &ButtonEvent {
                    button: event.button_code(),
                    state: event.state(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            pointer.frame(app);
        }
        InputEvent::PointerAxis { event } => {
            let pointer = app.seat.get_pointer().unwrap();
            let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
            for axis in [Axis::Horizontal, Axis::Vertical] {
                if let Some(value) = event.amount(axis) {
                    frame = frame.value(axis, value);
                }
                if let Some(value) = event.amount_v120(axis) {
                    frame = frame.v120(axis, value as i32);
                }
            }
            pointer.axis(app, frame);
            pointer.frame(app);
        }
        _ => {}
    }
}
