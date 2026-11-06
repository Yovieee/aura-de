//! Real Wayland protocols, input routing and nested EGL presentation.
use crate::{
    Backend, Config,
    backend::{BackendEvent, Presenter},
    capture::{CaptureConfig, CaptureProducer},
    decorations::{self, DecorationHit},
    glass::{GlassPanel, GlassRenderer},
    scene::SceneElement,
};
use aura_compositor::{
    input::{
        LayoutShortcut, LayoutShortcutState, SwipeGestureState, TouchpadSwipeGesture,
        layout_mode_for_gesture,
    },
    state::AuraCompositorState,
};
use aura_core::{GlassQualityPreset, LayoutMode, Rect, TrafficLightAction, WindowStateFlags};
use smithay::wayland::shell::xdg::SurfaceCachedState as XdgSurfaceCachedState;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::{
    backend::{
        input::{
            AbsolutePositionEvent, Axis, ButtonState, Event, InputBackend, InputEvent,
            KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        renderer::{
            Color32F, Frame, Renderer,
            element::{
                Id, Kind,
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
            utils::{
                CommitCounter, draw_render_elements, on_commit_buffer_handler,
                with_renderer_surface_state,
            },
        },
        winit::WinitEvent,
    },
    delegate_compositor, delegate_data_device, delegate_layer_shell, delegate_output,
    delegate_seat, delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        LayerSurface as DesktopLayer, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab,
        WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
        utils::under_from_surface_tree,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, keysyms},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData, Focus, MotionEvent,
        },
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
    hash::{Hash, Hasher},
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
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
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
    requested_fullscreen: bool,
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
    _decorations: XdgDecorationState,
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
    swipe: SwipeGestureState,
    layout_shortcut_keys: LayoutShortcutState,
    host_focused: bool,
    backend: Backend,
    requested_vt: Option<i32>,
    cursor_status: CursorImageStatus,
    pressed_pointer_buttons: HashSet<u32>,
    scale: i32,
    glass_ready: bool,
    frame_dirty: bool,
    scene_generation: u64,
    rendered_frames: u64,
    scene_captures: u64,
    blur_passes: u64,
    capture_available: bool,
    capture_subscribers: usize,
    capture_stop_requested: bool,
}

impl App {
    fn host_focus_changed(&mut self, focused: bool, time: u32) {
        if self.host_focused == focused {
            return;
        }
        self.host_focused = focused;
        if focused {
            self.sync_focus();
            return;
        }
        // Winit's synthetic releases are intentionally filtered by Smithay.
        // Clear both XKB and forwarded-key state before returning focus, so a
        // modifier released outside the nested window cannot trigger shortcuts.
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.unset_grab(self);
        keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        for code in keyboard.pressed_keys() {
            keyboard.input::<(), _>(
                self,
                code,
                smithay::backend::input::KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
        let pointer = self.seat.get_pointer().unwrap();
        for button in std::mem::take(&mut self.pressed_pointer_buttons) {
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state: ButtonState::Released,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: self.pointer,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        self.layout_shortcut_keys = LayoutShortcutState::default();
        self.swipe = SwipeGestureState::default();
        self.drag = None;
    }
    fn apply_layout_gesture(&mut self, gesture: TouchpadSwipeGesture) {
        let mode = layout_mode_for_gesture(self.model.workspace.mode, gesture);
        if mode != self.model.workspace.mode {
            self.model.set_layout_mode(mode);
            self.configure_windows();
            self.frame_dirty = true;
        }
    }
    fn id(&self, surface: &ToplevelSurface) -> Option<u64> {
        self.toplevels
            .iter()
            .find(|t| t.surface == *surface)
            .map(|t| t.id)
    }
    fn exclusive_layer(&self) -> Option<WlSurface> {
        let map = layer_map_for_output(&self.output);
        for layer in [Layer::Overlay, Layer::Top] {
            if layer == Layer::Top
                && self.fullscreen_window().is_some()
                && self.capture_subscribers == 0
            {
                continue;
            }
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
    fn fullscreen_window(&self) -> Option<u64> {
        self.model
            .workspace
            .windows()
            .iter()
            .find(|w| {
                w.flags
                    .contains(WindowStateFlags::FOCUSED | WindowStateFlags::FULLSCREEN)
                    && !w.flags.contains(WindowStateFlags::MINIMIZED)
            })
            .map(|w| w.id)
    }
    fn server_decorated(&self, top: &Toplevel, flags: WindowStateFlags) -> bool {
        decorations::is_server_side(top.surface.current_state().decoration_mode, flags)
    }
    fn visual_rect(&self, node: &aura_core::WindowNode) -> Rect {
        self.model
            .blur_regions
            .iter()
            .find(|r| r.window_id == node.id)
            .map(|r| r.rect)
            .unwrap_or(node.computed_rect)
    }
    fn scene_key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for top in &self.toplevels {
            top.id.hash(&mut h);
            top.mapped.hash(&mut h);
            top.surface
                .current_state()
                .decoration_mode
                .map(|m| m as u32)
                .hash(&mut h);
            if let Some(node) = self
                .model
                .workspace
                .windows()
                .iter()
                .find(|w| w.id == top.id)
            {
                node.flags.bits().hash(&mut h);
                let r = self.visual_rect(node);
                for v in [r.x, r.y, r.width, r.height] {
                    v.to_bits().hash(&mut h);
                }
            }
        }
        let map = layer_map_for_output(&self.output);
        for layer in [Layer::Background, Layer::Bottom] {
            for surface in map.layers_on(layer) {
                if !with_renderer_surface_state(surface.wl_surface(), |s| s.buffer().is_some())
                    .unwrap_or(false)
                {
                    continue;
                }
                surface.wl_surface().id().hash(&mut h);
                (layer as u32).hash(&mut h);
                if let Some(geo) = map.layer_geometry(surface) {
                    geo.loc.x.hash(&mut h);
                    geo.loc.y.hash(&mut h);
                    geo.size.w.hash(&mut h);
                    geo.size.h.hash(&mut h);
                }
            }
        }
        h.finish()
    }
    fn request_fullscreen(&mut self, surface: &ToplevelSurface, enabled: bool) {
        let Some(top) = self
            .toplevels
            .iter_mut()
            .find(|top| top.surface == *surface)
        else {
            return;
        };
        top.requested_fullscreen = enabled;
        let id = top.id;
        if !top.mapped {
            let output = self.model.workspace.output_rect;
            surface.with_pending_state(|state| {
                if enabled {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some((output.width as i32, output.height as i32).into());
                } else {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.size = Some((640, 480).into());
                }
            });
            if surface.is_initial_configure_sent() {
                surface.send_pending_configure();
            }
            return;
        }
        self.model.workspace.set_fullscreen(id, enabled);
        if enabled {
            self.focus(id);
        }
        self.frame_dirty = true;
        self.configure_windows();
    }
    fn configure_windows(&mut self) {
        for node in self.model.workspace.windows() {
            if let Some(top) = self.toplevels.iter().find(|t| t.id == node.id) {
                let desired_mode = top
                    .surface
                    .with_pending_state(|state| state.decoration_mode);
                let content = decorations::content_rect(
                    node.computed_rect,
                    decorations::is_server_side(desired_mode, node.flags),
                );
                top.surface.with_pending_state(|state| {
                    state.size =
                        Some((content.width.round() as i32, content.height.round() as i32).into());
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
                    if self.host_focused {
                        self.exclusive_layer().or(Some(surface))
                    } else {
                        None
                    },
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
            if self.host_focused {
                self.exclusive_layer().or(surface)
            } else {
                None
            },
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
            if layer == Layer::Top
                && self.fullscreen_window().is_some()
                && self.capture_subscribers == 0
            {
                continue;
            }
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
                let content =
                    decorations::content_rect(rect, self.server_decorated(top, node.flags));
                let loc: Point<i32, Logical> = (content.x as i32, content.y as i32).into();
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
    fn chrome_under(&self) -> Option<(u64, Rect, DecorationHit)> {
        // An overlay consumes input before any client chrome.
        let map = layer_map_for_output(&self.output);
        for layer in [Layer::Overlay, Layer::Top] {
            if layer == Layer::Top
                && self.fullscreen_window().is_some()
                && self.capture_subscribers == 0
            {
                continue;
            }
            if map.layer_under(layer, self.pointer).is_some() {
                return None;
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
                let r = self.visual_rect(node);
                if self.server_decorated(top, node.flags)
                    && let Some(hit) = decorations::hit_test(r, (self.pointer.x, self.pointer.y))
                {
                    return Some((node.id, r, hit));
                }
                // A higher client's content occludes lower chrome.
                let content = decorations::content_rect(r, self.server_decorated(top, node.flags));
                if self.pointer.x >= content.x as f64
                    && self.pointer.x < (content.x + content.width) as f64
                    && self.pointer.y >= content.y as f64
                    && self.pointer.y < (content.y + content.height) as f64
                {
                    return None;
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
                "backend": match self.backend { Backend::Winit=>"winit", Backend::Drm=>"drm" },
                "layout": match self.model.workspace.mode { LayoutMode::Floating=>"floating",LayoutMode::StageManager=>"stage-manager",LayoutMode::DynamicTiling=>"tiling" },
                "quality": if self.model.glass_material.kawase_blur_passes==0 {"reduced-transparency"} else if self.model.glass_material.chromatic_dispersion==0.0 {"balanced"} else {"ultra"},
                "mapped_windows": self.toplevels.iter().filter(|t|t.mapped).count(),
                "visible_windows": self.model.workspace.windows().iter().filter(|w|!w.flags.contains(WindowStateFlags::MINIMIZED)).count(),
                "reserved_top": self.model.workspace.menu_bar_height, "reserved_bottom":self.model.workspace.dock_reserved_height, "layers": self.layers.layer_surfaces().count(), "mapped_layers":self.layers.layer_surfaces().filter(|l|with_renderer_surface_state(l.wl_surface(),|s|s.buffer().is_some()).unwrap_or(false)).count(), "glass_active":self.glass_ready, "capture_available":self.capture_available,"capture_subscribers":self.capture_subscribers,
                "rendered_frames":self.rendered_frames,"scene_captures":self.scene_captures,"blur_passes":self.blur_passes,"scene_generation":self.scene_generation
            }).to_string()),
            ["windows"] => Ok(serde_json::Value::Array(self.model.workspace.windows().iter().map(|w| serde_json::json!({"id":w.id,"app_id":w.app_id,"title":w.title,"minimized":w.flags.contains(WindowStateFlags::MINIMIZED),"focused":w.flags.contains(WindowStateFlags::FOCUSED),"fullscreen":w.flags.contains(WindowStateFlags::FULLSCREEN),"maximized":w.flags.contains(WindowStateFlags::MAXIMIZED),"rect":{"x":w.computed_rect.x,"y":w.computed_rect.y,"width":w.computed_rect.width,"height":w.computed_rect.height}})).collect()).to_string()),
            ["layout", mode] => { self.model.set_layout_mode(match *mode { "floating"=>LayoutMode::Floating,"stage-manager"=>LayoutMode::StageManager,"tiling"=>LayoutMode::DynamicTiling,_=>return Err("invalid layout".into()) }); self.configure_windows(); Ok("OK".into()) }
            ["quality", quality] => { self.model.glass_material = aura_core::LiquidGlassMaterial::from_preset(match *quality { "ultra"=>GlassQualityPreset::Ultra,"balanced"=>GlassQualityPreset::Balanced,"reduced-transparency"=>GlassQualityPreset::ReducedTransparency,_=>return Err("invalid quality".into()) }); Ok("OK".into()) }
            [command @ ("focus"|"restore"|"minimize"|"close"), id] => {
                let id=id.parse::<u64>().map_err(|_| "invalid window id")?;
                if !self.model.workspace.windows().iter().any(|w| w.id==id) { return Err("window not found".into()); }
                match *command { "focus"=>self.focus(id), "restore"=>{self.model.workspace.restore_window(id);self.focus(id);}, "minimize"=>self.action(id,TrafficLightAction::MinimizeToDock),_=>self.action(id,TrafficLightAction::Close) }
                Ok("OK".into())
            }
            ["stop-screen-sharing"]=>{self.capture_stop_requested=true;Ok("OK".into())},
            ["quit"] => { self.model.should_exit=true; Ok("OK".into()) }
            _ => Err("unknown command".into()),
        }
    }
    fn resize(&mut self, size: smithay::utils::Size<i32, smithay::utils::Physical>) {
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
        let mode = Mode {
            size,
            refresh: self
                .output
                .current_mode()
                .map_or(60_000, |mode| mode.refresh),
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
    fn cursor_image(&mut self, _: &Seat<Self>, status: CursorImageStatus) {
        self.cursor_status = status;
        if self.backend == Backend::Drm {
            self.frame_dirty = true;
        }
    }
}
impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, c: &'a Client) -> &'a CompositorClientState {
        &c.get_data::<ClientState>().unwrap().compositor
    }
    fn destroyed(&mut self, _surface: &WlSurface) {
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.frame_dirty = true;
        let foreground = self.layers.layer_surfaces().any(|l| {
            l.wl_surface() == surface
                && with_states(surface, |s| {
                    matches!(
                        s.cached_state
                            .get::<LayerSurfaceCachedState>()
                            .current()
                            .layer,
                        Layer::Top | Layer::Overlay
                    )
                })
        });
        if !foreground {
            self.scene_generation = self.scene_generation.wrapping_add(1);
        }
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
            let requested_fullscreen = top.requested_fullscreen;
            if was_mapped && !has_buffer {
                top.requested_fullscreen = false;
            }
            if has_buffer && !was_mapped {
                let size = with_renderer_surface_state(surface, |s| s.buffer_size())
                    .flatten()
                    .unwrap_or_else(|| (640, 480).into());
                self.model
                    .map_xdg_window(id, "", "", (size.w as f32, size.h as f32));
                self.model
                    .workspace
                    .set_fullscreen(id, requested_fullscreen);
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
                let output = self.model.workspace.output_rect;
                shell.with_pending_state(|s| {
                    if requested_fullscreen && !was_mapped {
                        s.states.set(xdg_toplevel::State::Fullscreen);
                        s.size = Some((output.width as i32, output.height as i32).into());
                    } else {
                        s.states.unset(xdg_toplevel::State::Fullscreen);
                        s.size = Some((640, 480).into());
                    }
                });
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
            requested_fullscreen: false,
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
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _: Option<WlOutput>) {
        self.request_fullscreen(&surface, true);
    }
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_fullscreen(&surface, false);
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
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.popups.cleanup();
    }
    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
}
impl XdgDecorationHandler for App {
    fn new_decoration(&mut self, surface: ToplevelSurface) {
        decorations::configure(&surface, None, true);
        self.frame_dirty = true;
        self.configure_windows();
    }
    fn request_mode(&mut self, surface: ToplevelSurface, mode: DecorationMode) {
        decorations::configure(&surface, Some(mode), true);
        self.frame_dirty = true;
        self.configure_windows();
    }
    fn unset_mode(&mut self, surface: ToplevelSurface) {
        decorations::configure(&surface, None, true);
        self.frame_dirty = true;
        self.configure_windows();
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
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
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
delegate_xdg_decoration!(App);
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

pub(crate) fn run(mut config: Config) -> Result<(), Box<dyn Error>> {
    let mut backend = Presenter::new(&config)?;
    let initial_size = backend.size();
    if initial_size.w < 64 || initial_size.h < 64 || initial_size.w > 8192 || initial_size.h > 8192
    {
        return Err("output resolution must be between 64 and 8192 pixels per axis".into());
    }
    config.size = (initial_size.w, initial_size.h);
    let output_info = backend.output_info();
    let mut display = Display::<App>::new()?;
    let mut dh = display.handle();
    let mut seats = SeatState::new();
    let mut seat = seats.new_wl_seat(&dh, "aura-seat");
    seat.add_keyboard(Default::default(), 200, 25)?;
    seat.add_pointer();
    let output = Output::new(
        output_info.name,
        PhysicalProperties {
            size: output_info.physical_size.into(),
            subpixel: Subpixel::Unknown,
            make: output_info.make,
            model: output_info.model,
        },
    );
    output.create_global::<App>(&dh);
    let mode = Mode {
        size: config.size.into(),
        refresh: output_info.refresh,
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
        _decorations: XdgDecorationState::new::<App>(&dh),
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
        swipe: SwipeGestureState::default(),
        layout_shortcut_keys: LayoutShortcutState::default(),
        host_focused: true,
        backend: config.backend,
        requested_vt: None,
        cursor_status: CursorImageStatus::default_named(),
        pressed_pointer_buttons: HashSet::new(),
        scale: config.scale,
        glass_ready: false,
        frame_dirty: true,
        scene_generation: 1,
        rendered_frames: 0,
        scene_captures: 0,
        blur_passes: 0,
        capture_available: false,
        capture_subscribers: 0,
        capture_stop_requested: false,
    };
    app.model.socket_name = config.socket.clone();
    app.model.workspace.menu_bar_height = 0.;
    app.model.workspace.dock_reserved_height = 0.;
    let mut events = calloop::EventLoop::<App>::try_new()?;
    let (tx, rx) = calloop::channel::channel::<Command>();
    events.handle().insert_source(rx, |event, _, app| {
        if let calloop::channel::Event::Msg(command) = event {
            if !matches!(
                command.text.split_whitespace().next(),
                Some("status" | "windows")
            ) {
                app.frame_dirty = true;
            }
            let result = app
                .control(&command.text)
                .unwrap_or_else(|e| format!("ERR {e}"));
            let _ = command.response.send(result);
        }
    })?;
    let _control = control_server(tx)?;
    tracing::info!(socket=%config.socket,backend=?config.backend,"Wayland server listening; GLES renderer active");
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    let start = Instant::now();
    let mut last = start;
    let mut presented = 0u64;
    let mut glass_renderer: Option<GlassRenderer> = None;
    let mut glass_initialization_attempted = false;
    let mut previous_scene_key = app.scene_key();
    let mut capture_producer = None;
    if backend.supports_capture() {
        let (renderer, _) = backend.bind()?;
        match CaptureConfig::from_env(backend_size(config.size)) {
            Ok(Some(config)) => match CaptureProducer::new(renderer, config) {
                Ok(capture) => {
                    capture_producer = Some(capture);
                    app.capture_available = true;
                }
                Err(error) => tracing::warn!(%error,"screen capture unavailable"),
            },
            Ok(None) => {}
            Err(error) => tracing::warn!(%error,"screen capture configuration invalid"),
        }
    }
    while !app.model.should_exit && !stop.load(Ordering::Relaxed) {
        for event in backend.poll_events()? {
            match event {
                BackendEvent::Winit(event) => match event {
                    WinitEvent::Resized { .. } | WinitEvent::Redraw => app.frame_dirty = true,
                    WinitEvent::Input(event) => handle_input(&mut app, event),
                    WinitEvent::Focus(focused) => {
                        app.host_focus_changed(focused, start.elapsed().as_millis() as u32)
                    }
                    WinitEvent::CloseRequested => app.model.should_exit = true,
                },
                #[cfg(feature = "native")]
                BackendEvent::Native(event) => match event {
                    crate::native::NativeEvent::Input(event) => handle_input(&mut app, event),
                    crate::native::NativeEvent::Activated(active) => {
                        app.host_focus_changed(active, start.elapsed().as_millis() as u32);
                        app.frame_dirty = true;
                        if let Some(glass) = &mut glass_renderer {
                            glass.invalidate();
                        }
                    }
                },
            }
        }
        if let Some(vt) = app.requested_vt.take()
            && let Err(error) = backend.change_vt(vt)
        {
            tracing::warn!(%error,vt,"VT switch failed");
        }
        if app.model.should_exit {
            break;
        }
        while let Some(stream) = listener.accept()? {
            dh.insert_client(stream, Arc::new(ClientState::default()))?;
        }
        display.dispatch_clients(&mut app)?;
        app.popups.cleanup();
        events.dispatch(Duration::from_millis(8), &mut app)?;
        let size = backend.size();
        if app.output.current_mode().is_none_or(|m| m.size != size) {
            app.resize(size);
        }
        let now = Instant::now();
        app.model.tick_frame((now - last).as_secs_f32());
        last = now;
        let scene_key = app.scene_key();
        if previous_scene_key != scene_key {
            app.scene_generation = app.scene_generation.wrapping_add(1);
            app.frame_dirty = true;
            previous_scene_key = scene_key;
        }
        if config.smoke && start.elapsed() > Duration::from_secs(15) {
            return Err("smoke timed out without completed real client frames".into());
        }
        if app.capture_stop_requested {
            if let Some(capture) = &mut capture_producer {
                capture.revoke();
            }
            app.capture_stop_requested = false;
        }
        let capture_subscribers = capture_producer
            .as_ref()
            .map_or(0, |c| c.subscriber_count());
        if app.capture_subscribers != capture_subscribers {
            // Keep the sharing indicator and its Stop action reachable even
            // above fullscreen content, and redraw when that visibility changes.
            app.capture_subscribers = capture_subscribers;
            app.frame_dirty = true;
        }
        let capture_due = capture_producer
            .as_ref()
            .is_some_and(|c| c.wants_frame(now));
        if let Some(capture) = &mut capture_producer
            && capture.has_pending()
        {
            let (renderer, _) = backend.bind()?;
            if let Err(error) = capture.poll(renderer) {
                tracing::warn!(%error,"capture completion failed");
            }
        }
        if !backend.can_render() || (!app.frame_dirty && !capture_due) {
            display.flush_clients()?;
            continue;
        }
        let output_transform = backend.output_transform();
        let damage = smithay::utils::Rectangle::from_size(size);
        let mut rendered = Vec::new();
        {
            let (renderer, mut framebuffer) = backend.bind()?;
            let mut foreground: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            let mut scene: Vec<SceneElement> = Vec::new();
            let mut panels = Vec::new();
            let map = layer_map_for_output(&app.output);
            for layer in [Layer::Overlay, Layer::Top] {
                if layer == Layer::Top
                    && app.fullscreen_window().is_some()
                    && app.capture_subscribers == 0
                {
                    continue;
                }
                for surface in map.layers_on(layer).rev() {
                    if !with_renderer_surface_state(surface.wl_surface(), |s| s.buffer().is_some())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if let Some(geo) = map.layer_geometry(surface) {
                        if matches!(
                            surface.namespace(),
                            "aura-menu-bar"
                                | "aura-dock"
                                | "aura-launcher"
                                | "aura-screen-cast-consent"
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
                        foreground.extend(render_elements_from_surface_tree(
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
                    let decorated = app.server_decorated(top, node.flags);
                    let content = decorations::content_rect(rect, decorated);
                    let loc: Point<i32, Logical> = (content.x as i32, content.y as i32).into();
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
                        scene.extend(
                            render_elements_from_surface_tree::<
                                GlesRenderer,
                                WaylandSurfaceRenderElement<GlesRenderer>,
                            >(
                                renderer,
                                popup.wl_surface(),
                                offset.to_physical(config.scale),
                                config.scale as f64,
                                1.,
                                Kind::Unspecified,
                            )
                            .into_iter()
                            .map(SceneElement::from),
                        );
                        rendered.push(popup.wl_surface().clone());
                    }
                    scene.extend(
                        render_elements_from_surface_tree::<
                            GlesRenderer,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >(
                            renderer,
                            top.surface.wl_surface(),
                            loc.to_physical(config.scale),
                            config.scale as f64,
                            1.,
                            Kind::Unspecified,
                        )
                        .into_iter()
                        .map(SceneElement::from),
                    );
                    rendered.push(top.surface.wl_surface().clone());
                    if decorated {
                        for control in decorations::controls(rect) {
                            scene.push(solid(control.rect, control.color, config.scale));
                        }
                        scene.push(solid(
                            decorations::titlebar_rect(rect),
                            if node.flags.contains(WindowStateFlags::FOCUSED) {
                                [0.20, 0.25, 0.33, 1.]
                            } else {
                                [0.13, 0.16, 0.21, 1.]
                            },
                            config.scale,
                        ));
                    }
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
                        scene.extend(
                            render_elements_from_surface_tree::<
                                GlesRenderer,
                                WaylandSurfaceRenderElement<GlesRenderer>,
                            >(
                                renderer,
                                surface.wl_surface(),
                                geo.loc.to_physical(config.scale),
                                config.scale as f64,
                                1.,
                                Kind::Unspecified,
                            )
                            .into_iter()
                            .map(SceneElement::from),
                        );
                        rendered.push(surface.wl_surface().clone());
                    }
                }
            }
            drop(map);
            let has_client = !scene.is_empty() && app.toplevels.iter().any(|t| t.mapped);
            if !glass_initialization_attempted {
                glass_initialization_attempted = true;
                match GlassRenderer::new(renderer, size) {
                    Ok(glass) => glass_renderer = Some(glass),
                    Err(error) => {
                        tracing::warn!(%error,"glass initialization failed; using opaque panels")
                    }
                }
            }
            let result = glass_renderer.as_mut().map(|glass| {
                glass.render_scene(
                    renderer,
                    &mut framebuffer,
                    size,
                    output_transform,
                    config.scale as f64,
                    app.scene_generation,
                    &scene,
                    &foreground,
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
                let mut frame = renderer.render(&mut framebuffer, size, output_transform)?;
                frame.clear(Color32F::new(0.065, 0.085, 0.14, 1.), &[damage])?;
                draw_render_elements(&mut frame, config.scale as f64, &scene, &[damage])?;
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
                draw_render_elements(&mut frame, config.scale as f64, &foreground, &[damage])?;
                let _sync = frame.finish()?;
            }
            if let Some(glass) = &glass_renderer {
                let stats = glass.stats();
                app.scene_captures = stats.scene_captures;
                app.blur_passes = stats.blur_passes;
            }
            if let Some(capture) = &mut capture_producer {
                // Completion polling uses a surfaceless EGL context, so it must
                // happen before binding/rendering the output, never before swap.
                if let Err(error) = capture.set_output_size(size) {
                    tracing::warn!(%error,"capture resize failed");
                }
                if capture_due {
                    let mut frame = renderer.render(&mut framebuffer, size, output_transform)?;
                    if let Err(error) = capture.enqueue(&mut frame, size) {
                        tracing::warn!(%error,"capture enqueue failed");
                    }
                    let _sync = frame.finish()?;
                }
            }
            if app.backend == Backend::Drm {
                let cursor = cursor_elements(&app, renderer);
                if !cursor.is_empty() {
                    let mut frame = renderer.render(&mut framebuffer, size, output_transform)?;
                    draw_render_elements(&mut frame, config.scale as f64, &cursor, &[damage])?;
                    let _sync = frame.finish()?;
                    if let CursorImageStatus::Surface(surface) = &app.cursor_status {
                        rendered.push(surface.clone());
                    }
                }
            }
            if has_client {
                presented += 1;
            }
        }
        app.rendered_frames = app.rendered_frames.wrapping_add(1);
        app.frame_dirty = false;
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
            if let Some(capture) = &mut capture_producer {
                let (renderer, _) = backend.bind()?;
                capture.shutdown(renderer)?;
            }
            return Ok(());
        }
        if config.smoke && start.elapsed() > Duration::from_secs(15) {
            return Err("smoke timed out without real client frames".into());
        }
    }
    if let Some(capture) = &mut capture_producer {
        let (renderer, _) = backend.bind()?;
        capture.shutdown(renderer)?;
    }
    if config.smoke && presented < 2 {
        return Err("compositor closed before smoke completed".into());
    }
    Ok(())
}

fn handle_input<B: InputBackend>(app: &mut App, event: InputEvent<B>) {
    if let Some(gesture) = app.swipe.process(&event) {
        app.apply_layout_gesture(gesture);
    }
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
                    if app.backend == Backend::Drm
                        && mods.ctrl
                        && mods.alt
                        && let Some(vt) = vt_for_keysym(key.modified_sym().raw())
                    {
                        if event.state() == smithay::backend::input::KeyState::Pressed {
                            app.requested_vt = Some(vt);
                        }
                        return FilterResult::Intercept(());
                    }
                    // Nested winit does not expose raw touchpad gestures. These
                    // keyboard alternatives use the same committed layout action.
                    match app.layout_shortcut_keys.process(
                        event.key_code().raw(),
                        key.modified_sym().raw(),
                        event.state() == smithay::backend::input::KeyState::Pressed,
                        mods.logo && !mods.ctrl && !mods.alt && !mods.shift,
                    ) {
                        LayoutShortcut::Apply(gesture) => {
                            app.apply_layout_gesture(gesture);
                            return FilterResult::Intercept(());
                        }
                        LayoutShortcut::Consumed => return FilterResult::Intercept(()),
                        LayoutShortcut::Forward => {}
                    }
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
            move_pointer(app, event.position_transformed(size), event.time_msec());
        }
        InputEvent::PointerMotion { event } => {
            move_pointer(app, app.pointer + event.delta(), event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            let pointer = app.seat.get_pointer().unwrap();
            if event.state() == ButtonState::Released {
                app.drag = None;
                // A physical release may arrive after focus/seat loss already
                // synthesized it, or after a compositor-owned chrome press.
                if !app.pressed_pointer_buttons.contains(&event.button_code()) {
                    return;
                }
            }
            if event.state() == ButtonState::Pressed
                && let Some((id, rect, hit)) = app.chrome_under()
            {
                app.focus(id);
                match hit {
                    DecorationHit::Control(action) => app.action(id, action),
                    DecorationHit::Titlebar => {
                        app.drag = Some(Drag {
                            id,
                            origin: app.pointer,
                            rect,
                            edges: None,
                        });
                    }
                }
                app.frame_dirty = true;
                return;
            }
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
            if event.state() == ButtonState::Pressed {
                app.pressed_pointer_buttons.insert(event.button_code());
            } else {
                app.pressed_pointer_buttons.remove(&event.button_code());
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

fn bounded_pointer(
    location: Point<f64, Logical>,
    size: smithay::utils::Size<i32, Logical>,
) -> Option<Point<f64, Logical>> {
    (location.x.is_finite() && location.y.is_finite()).then(|| {
        (
            location.x.clamp(0., f64::from((size.w - 1).max(0))),
            location.y.clamp(0., f64::from((size.h - 1).max(0))),
        )
            .into()
    })
}
fn move_pointer(app: &mut App, location: Point<f64, Logical>, time: u32) {
    let size = app
        .output
        .current_mode()
        .unwrap()
        .size
        .to_logical(app.scale);
    let Some(location) = bounded_pointer(location, size) else {
        return;
    };
    app.pointer = location;
    if app.backend == Backend::Drm {
        app.frame_dirty = true;
    }
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
            time,
        },
    );
    pointer.frame(app);
}

fn vt_for_keysym(key: u32) -> Option<i32> {
    // The default XKB map translates Ctrl+Alt+Fn to XF86Switch_VT_n.
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&key) {
        Some((key - keysyms::KEY_XF86Switch_VT_1 + 1) as i32)
    } else if (keysyms::KEY_F1..=keysyms::KEY_F12).contains(&key) {
        Some((key - keysyms::KEY_F1 + 1) as i32)
    } else {
        None
    }
}

fn cursor_elements(app: &App, renderer: &mut GlesRenderer) -> Vec<SceneElement> {
    let fallback = CursorImageStatus::default_named();
    let status = if app.chrome_under().is_some() || app.surface_under().is_none() {
        &fallback
    } else {
        &app.cursor_status
    };
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Surface(surface) => {
            let hotspot = with_states(surface, |state| {
                state
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            render_elements_from_surface_tree::<
                GlesRenderer,
                WaylandSurfaceRenderElement<GlesRenderer>,
            >(
                renderer,
                surface,
                (app.pointer - hotspot.to_f64())
                    .to_i32_round()
                    .to_physical(app.scale),
                app.scale as f64,
                1.,
                Kind::Cursor,
            )
            .into_iter()
            .map(SceneElement::from)
            .collect()
        }
        CursorImageStatus::Named(_) => {
            // Dependency-free fallback arrow. Client cursor surfaces retain their own image/hotspot.
            let mut result = Vec::new();
            for row in 0..16 {
                let width = if row < 11 { row + 1 } else { 4 };
                if row > 1 && width > 2 {
                    result.push(solid(
                        Rect::new(
                            app.pointer.x as f32 + 1.,
                            app.pointer.y as f32 + row as f32,
                            (width - 2) as f32,
                            1.,
                        ),
                        [1., 1., 1., 1.],
                        app.scale,
                    ));
                }
                result.push(solid(
                    Rect::new(
                        app.pointer.x as f32,
                        app.pointer.y as f32 + row as f32,
                        width as f32,
                        1.,
                    ),
                    [0., 0., 0., 1.],
                    app.scale,
                ));
            }
            result
        }
    }
}

fn backend_size(size: (i32, i32)) -> smithay::utils::Size<i32, smithay::utils::Physical> {
    size.into()
}
fn solid(rect: Rect, color: [f32; 4], scale: i32) -> SceneElement {
    SolidColorRenderElement::new(
        Id::new(),
        smithay::utils::Rectangle::new(
            (
                (rect.x * scale as f32).round() as i32,
                (rect.y * scale as f32).round() as i32,
            )
                .into(),
            (
                (rect.width * scale as f32).round() as i32,
                (rect.height * scale as f32).round() as i32,
            )
                .into(),
        ),
        CommitCounter::default(),
        Color32F::new(color[0], color[1], color[2], color[3]),
        Kind::Unspecified,
    )
    .into()
}

#[cfg(test)]
mod native_input_tests {
    use super::*;

    #[test]
    fn pointer_motion_stays_inside_logical_output_and_rejects_nonfinite_coordinates() {
        let size = (640, 400).into();
        assert_eq!(
            bounded_pointer((-200., 2000.).into(), size),
            Some((0., 399.).into())
        );
        assert_eq!(
            bounded_pointer((639., 399.).into(), size),
            Some((639., 399.).into())
        );
        assert_eq!(bounded_pointer((f64::NAN, 0.).into(), size), None);
        assert_eq!(bounded_pointer((0., f64::INFINITY).into(), size), None);
    }

    #[test]
    fn vt_shortcuts_map_only_the_twelve_function_keys() {
        use smithay::input::keyboard::xkb;
        assert_eq!(vt_for_keysym(keysyms::KEY_F1), Some(1));
        assert_eq!(vt_for_keysym(keysyms::KEY_F12), Some(12));
        assert_eq!(vt_for_keysym(keysyms::KEY_F1 - 1), None);
        assert_eq!(vt_for_keysym(keysyms::KEY_F13), None);
        assert_eq!(vt_for_keysym(keysyms::KEY_a), None);
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the same default XKB map as the seat");
        let mut state = xkb::State::new(&keymap);
        state.update_key(xkb::Keycode::new(37), xkb::KeyDirection::Down); // Left Ctrl.
        state.update_key(xkb::Keycode::new(64), xkb::KeyDirection::Down); // Left Alt.
        let translated = state.key_get_one_sym(xkb::Keycode::new(67)).raw(); // F1.
        assert_eq!(translated, keysyms::KEY_XF86Switch_VT_1);
        assert_eq!(vt_for_keysym(translated), Some(1));
    }
}
