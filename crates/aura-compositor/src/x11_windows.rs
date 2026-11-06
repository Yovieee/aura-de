//! X11 window policy, reusing the same core IDs, geometry, chrome and focus.
use super::*;
use smithay::{utils::Rectangle, xwayland::X11Surface};

impl App {
    fn x11_id(&self, window: &X11Surface) -> Option<u64> {
        self.toplevels.iter().find_map(|top| match &top.surface {
            WindowSurface::X11 {
                window: candidate, ..
            } if candidate == window => Some(top.id),
            _ => None,
        })
    }
    pub(crate) fn x11_map(&mut self, window: &X11Surface) {
        if !window.is_mapped() {
            return;
        }
        let Some(surface) = window.wl_surface() else {
            if !self.x11_pending.contains(window) {
                self.x11_pending.push(window.clone());
            }
            return;
        };
        self.x11_pending.retain(|pending| pending != window);
        if window.is_override_redirect() {
            if !self.x11_overrides.contains(window) {
                self.x11_overrides.push(window.clone());
            }
            self.output.enter(&surface);
        } else if let Some(id) = self.x11_id(window) {
            if let Some(top) = self.toplevels.iter_mut().find(|top| top.id == id) {
                top.surface = WindowSurface::X11 {
                    window: window.clone(),
                    surface: surface.clone(),
                };
            }
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.toplevels.push(Toplevel {
                id,
                surface: WindowSurface::X11 {
                    window: window.clone(),
                    surface: surface.clone(),
                },
                mapped: false,
                requested_fullscreen: window.is_fullscreen(),
            });
        }
        self.reconcile_toplevel_buffer(&surface);
        self.x11_metadata(window);
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
    }
    pub(crate) fn x11_associated(&mut self, window: &X11Surface, _: &WlSurface) {
        self.x11_map(window);
    }
    pub(crate) fn x11_remove(&mut self, window: &X11Surface) {
        self.x11_pending.retain(|pending| pending != window);
        self.x11_overrides.retain(|candidate| candidate != window);
        if let Some(surface) = window.wl_surface() {
            self.output.leave(&surface);
        }
        if let Some(id) = self.x11_id(window) {
            self.toplevels.retain(|top| top.id != id);
            self.model.workspace.remove_window(id);
            self.model.window_springs.remove(&id);
            if self.drag.as_ref().is_some_and(|drag| drag.id == id) {
                self.drag = None;
            }
            self.sync_focus();
        }
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
    }
    pub(crate) fn x11_metadata(&mut self, window: &X11Surface) {
        if let Some(id) = self.x11_id(window) {
            self.model
                .workspace
                .update_window_metadata(id, window.class(), window.title());
            self.frame_dirty = true;
        }
    }
    pub(crate) fn x11_configure(
        &mut self,
        window: &X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
    ) {
        let Some(id) = self.x11_id(window) else {
            let mut rect = window.geometry();
            if let Some(value) = x {
                rect.loc.x = value;
            }
            if let Some(value) = y {
                rect.loc.y = value;
            }
            if let Some(value) = width {
                rect.size.w = value.clamp(1, 8192) as i32;
            }
            if let Some(value) = height {
                rect.size.h = value.clamp(1, 8192) as i32;
            }
            let _ = window.configure(rect);
            return;
        };
        let Some(node) = self
            .model
            .workspace
            .windows()
            .iter()
            .find(|node| node.id == id)
        else {
            return;
        };
        if self.model.workspace.mode != LayoutMode::Floating
            || node
                .flags
                .intersects(WindowStateFlags::FULLSCREEN | WindowStateFlags::MAXIMIZED)
        {
            self.configure_windows();
            return;
        }
        let mut rect = node.computed_rect;
        let inset = if !window.is_decorated() {
            decorations::TITLEBAR_HEIGHT
        } else {
            0.
        };
        if let Some(value) = x {
            rect.x = value as f32;
        }
        if let Some(value) = y {
            rect.y = value as f32 - inset;
        }
        if let Some(value) = width {
            rect.width = value.clamp(1, 8192) as f32;
        }
        if let Some(value) = height {
            rect.height = value.clamp(1, 8192) as f32 + inset;
        }
        self.model.workspace.set_floating_rect(id, rect);
        self.configure_windows();
        self.frame_dirty = true;
    }
    pub(crate) fn x11_configure_notify(
        &mut self,
        _: &X11Surface,
        _: Rectangle<i32, Logical>,
        _: Option<u32>,
    ) {
        self.frame_dirty = true;
        self.scene_generation = self.scene_generation.wrapping_add(1);
    }
    pub(crate) fn x11_fullscreen(&mut self, window: &X11Surface, enabled: bool) {
        if let Some(id) = self.x11_id(window) {
            self.model.workspace.set_fullscreen(id, enabled);
            if enabled {
                self.focus(id);
            }
            self.configure_windows();
            self.frame_dirty = true;
        } else {
            let _ = window.set_fullscreen(enabled);
        }
    }
    pub(crate) fn x11_maximize(&mut self, window: &X11Surface, enabled: bool) {
        if self.x11_id(window).is_none() {
            let _ = window.set_maximized(enabled);
            return;
        }
        if let Some(id) = self.x11_id(window)
            && let Some(node) = self
                .model
                .workspace
                .windows()
                .iter()
                .find(|node| node.id == id)
            && node.flags.contains(WindowStateFlags::MAXIMIZED) != enabled
        {
            self.action(id, TrafficLightAction::ToggleZoomFullscreen);
        }
    }
    pub(crate) fn x11_minimize(&mut self, window: &X11Surface) {
        if let Some(id) = self.x11_id(window) {
            self.action(id, TrafficLightAction::MinimizeToDock);
        }
    }
    pub(crate) fn x11_restore(&mut self, window: &X11Surface) {
        if let Some(id) = self.x11_id(window) {
            self.model.workspace.restore_window(id);
            self.focus(id);
        }
    }
    pub(crate) fn x11_move(&mut self, window: &X11Surface, button: u32) {
        self.x11_drag(window, button, None);
    }
    pub(crate) fn x11_resize(
        &mut self,
        window: &X11Surface,
        button: u32,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        self.x11_drag(window, button, Some(edges));
    }
    fn x11_drag(
        &mut self,
        window: &X11Surface,
        button: u32,
        edges: Option<xdg_toplevel::ResizeEdge>,
    ) {
        let button = match button {
            1 => 272,
            2 => 274,
            3 => 273,
            8 => 275,
            9 => 276,
            _ => return,
        };
        let pointer = self.seat.get_pointer().unwrap();
        if !self.pressed_pointer_buttons.contains(&button) {
            return;
        }
        let Some((surface, _)) = pointer.grab_start_data().and_then(|grab| grab.focus) else {
            return;
        };
        if window.wl_surface().as_ref() != Some(&surface) {
            return;
        }
        if let Some(id) = self.x11_id(window)
            && let Some(node) = self
                .model
                .workspace
                .windows()
                .iter()
                .find(|node| node.id == id)
        {
            self.drag = Some(Drag {
                id,
                origin: self.pointer,
                rect: node.computed_rect,
                edges,
            });
        }
    }
    pub(super) fn x11_override_under(&self) -> Option<(WlSurface, Point<f64, Logical>)> {
        for window in self
            .x11_overrides
            .iter()
            .rev()
            .filter(|window| window.is_mapped())
        {
            if let Some(surface) = window.wl_surface()
                && let Some((surface, offset)) = under_from_surface_tree(
                    &surface,
                    self.pointer,
                    window.geometry().loc,
                    WindowSurfaceType::ALL,
                )
            {
                return Some((surface, offset.to_f64()));
            }
        }
        None
    }
}
