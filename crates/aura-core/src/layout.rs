//! Hybrid Floating, Stage Manager strip, and Smart-Snap Tiling window layout engine.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

pub type WindowId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Keep externally supplied output geometry finite and nonempty.
    pub fn sanitized(self) -> Self {
        Self {
            x: if self.x.is_finite() { self.x } else { 0.0 },
            y: if self.y.is_finite() { self.y } else { 0.0 },
            width: finite_positive(self.width, 1.0).max(1.0),
            height: finite_positive(self.height, 1.0).max(1.0),
        }
    }
}

fn finite_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WindowStateFlags: u32 {
        const FOCUSED     = 0b0000_0001;
        const MINIMIZED   = 0b0000_0010;
        const MAXIMIZED   = 0b0000_0100;
        const FULLSCREEN  = 0b0000_1000;
        const STAGED      = 0b0001_0000;
        const LIQUID_BLUR = 0b0010_0000;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode {
    Floating,
    StageManager,
    DynamicTiling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrafficLightAction {
    Close,
    MinimizeToDock,
    ToggleZoomFullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapZone {
    LeftHalf,
    RightHalf,
    TopLeftQuarter,
    TopRightQuarter,
    BottomLeftQuarter,
    BottomRightQuarter,
    MaximizeFill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowNode {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub floating_rect: Rect,
    pub computed_rect: Rect,
    pub flags: WindowStateFlags,
}

pub struct WorkspaceManager {
    pub mode: LayoutMode,
    pub output_rect: Rect,
    pub menu_bar_height: f32,
    pub dock_reserved_height: f32,
    pub outer_gap: f32,
    pub inner_gap: f32,
    windows: Vec<WindowNode>,
}

impl WorkspaceManager {
    pub fn new(output_rect: Rect) -> Self {
        Self {
            mode: LayoutMode::Floating,
            output_rect: output_rect.sanitized(),
            menu_bar_height: 32.0,
            dock_reserved_height: 84.0,
            outer_gap: 14.0,
            inner_gap: 12.0,
            windows: Vec::new(),
        }
    }

    pub fn usable_work_area(&self) -> Rect {
        let output = self.output_rect.sanitized();
        // Keep at least one logical pixel even when reservations exceed a tiny output.
        let menu = nonnegative(self.menu_bar_height).min((output.height - 1.0).max(0.0));
        let dock =
            nonnegative(self.dock_reserved_height).min((output.height - menu - 1.0).max(0.0));
        Rect::new(
            output.x,
            output.y + menu,
            output.width,
            output.height - menu - dock,
        )
    }

    pub fn windows(&self) -> &[WindowNode] {
        &self.windows
    }

    /// Map a new ID exactly once. Duplicate mapping leaves existing state unchanged.
    pub fn add_window(
        &mut self,
        id: WindowId,
        app_id: impl Into<String>,
        title: impl Into<String>,
        initial_size: (f32, f32),
    ) -> bool {
        if self.contains_window(id) {
            return false;
        }
        let work = self.usable_work_area();
        let w = finite_positive(initial_size.0, 640.0).min(work.width);
        let h = finite_positive(initial_size.1, 480.0).min(work.height);
        let cascade_offset = (self.windows.len() as f32 * 26.0) % 130.0;
        let rect = Rect::new(
            work.x + ((work.width - w) * 0.5 + cascade_offset).min(work.width - w),
            work.y + ((work.height - h) * 0.5 + cascade_offset).min(work.height - h),
            w,
            h,
        );

        for win in &mut self.windows {
            win.flags.remove(WindowStateFlags::FOCUSED);
        }

        self.windows.push(WindowNode {
            id,
            app_id: app_id.into(),
            title: title.into(),
            floating_rect: rect,
            computed_rect: rect,
            flags: WindowStateFlags::FOCUSED | WindowStateFlags::LIQUID_BLUR,
        });
        self.recompute_layout();
        true
    }

    pub fn contains_window(&self, id: WindowId) -> bool {
        self.windows.iter().any(|window| window.id == id)
    }

    /// Focus a visible window. Restore is explicit so minimized windows cannot steal focus.
    pub fn focus_window(&mut self, id: WindowId) -> bool {
        if !self
            .windows
            .iter()
            .any(|w| w.id == id && !w.flags.contains(WindowStateFlags::MINIMIZED))
        {
            return false;
        }
        for window in &mut self.windows {
            window.flags.set(WindowStateFlags::FOCUSED, window.id == id);
        }
        self.recompute_layout();
        true
    }

    pub fn restore_window(&mut self, id: WindowId) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        window.flags.remove(WindowStateFlags::MINIMIZED);
        self.focus_window(id)
    }

    pub fn remove_window(&mut self, id: WindowId) -> bool {
        let before = self.windows.len();
        self.windows.retain(|window| window.id != id);
        if self.windows.len() == before {
            return false;
        }
        self.repair_focus();
        self.recompute_layout();
        true
    }

    fn repair_focus(&mut self) {
        let focused = self
            .windows
            .iter()
            .rev()
            .find(|w| {
                w.flags.contains(WindowStateFlags::FOCUSED)
                    && !w.flags.contains(WindowStateFlags::MINIMIZED)
            })
            .or_else(|| {
                self.windows
                    .iter()
                    .rev()
                    .find(|w| !w.flags.contains(WindowStateFlags::MINIMIZED))
            })
            .map(|w| w.id);
        for window in &mut self.windows {
            window
                .flags
                .set(WindowStateFlags::FOCUSED, focused == Some(window.id));
        }
    }

    pub fn resize_output(&mut self, output: Rect) {
        self.output_rect = output.sanitized();
        let work = self.usable_work_area();
        for window in &mut self.windows {
            let rect = &mut window.floating_rect;
            rect.width = rect.width.min(work.width);
            rect.height = rect.height.min(work.height);
            rect.x = rect.x.clamp(work.x, work.x + work.width - rect.width);
            rect.y = rect.y.clamp(work.y, work.y + work.height - rect.height);
        }
        self.recompute_layout();
    }

    /// Apply interactive move/resize geometry in logical output coordinates.
    /// Layout modes still determine computed geometry; callers can select Floating for dragging.
    pub fn set_floating_rect(&mut self, id: WindowId, rect: Rect) -> bool {
        if !rect.x.is_finite()
            || !rect.y.is_finite()
            || !rect.width.is_finite()
            || !rect.height.is_finite()
            || rect.width <= 0.0
            || rect.height <= 0.0
        {
            return false;
        }
        let work = self.usable_work_area();
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        let width = rect.width.min(work.width);
        let height = rect.height.min(work.height);
        window.floating_rect = Rect::new(
            rect.x.clamp(work.x, work.x + work.width - width),
            rect.y.clamp(work.y, work.y + work.height - height),
            width,
            height,
        );
        window
            .flags
            .remove(WindowStateFlags::MAXIMIZED | WindowStateFlags::FULLSCREEN);
        self.recompute_layout();
        true
    }

    /// Set the protocol fullscreen state without changing focus or restore geometry.
    /// Repeated requests are idempotent. The previous maximized state remains set so
    /// leaving fullscreen restores either the work area or the original floating rect.
    pub fn set_fullscreen(&mut self, id: WindowId, fullscreen: bool) -> bool {
        let Some(window) = self.windows.iter_mut().find(|window| window.id == id) else {
            return false;
        };
        if window.flags.contains(WindowStateFlags::FULLSCREEN) != fullscreen {
            window.flags.set(WindowStateFlags::FULLSCREEN, fullscreen);
            self.recompute_layout();
        }
        true
    }

    pub fn update_window_metadata(
        &mut self,
        id: WindowId,
        app_id: impl Into<String>,
        title: impl Into<String>,
    ) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        window.app_id = app_id.into();
        window.title = title.into();
        true
    }

    pub fn apply_traffic_light(&mut self, id: WindowId, action: TrafficLightAction) -> bool {
        match action {
            TrafficLightAction::Close => self.remove_window(id),
            TrafficLightAction::MinimizeToDock => {
                if let Some(win) = self.windows.iter_mut().find(|w| w.id == id) {
                    win.flags.insert(WindowStateFlags::MINIMIZED);
                    win.flags
                        .remove(WindowStateFlags::FOCUSED | WindowStateFlags::STAGED);
                    self.repair_focus();
                    self.recompute_layout();
                    true
                } else {
                    false
                }
            }
            TrafficLightAction::ToggleZoomFullscreen => {
                let Some(win) = self.windows.iter_mut().find(|w| w.id == id) else {
                    return false;
                };
                win.flags.toggle(WindowStateFlags::MAXIMIZED);
                self.recompute_layout();
                true
            }
        }
    }

    pub fn snap_window(&mut self, id: WindowId, zone: SnapZone) -> Option<Rect> {
        let work = self.usable_work_area();
        let g = nonnegative(self.outer_gap).min(work.width.min(work.height) * 0.2);
        let half_w = (work.width - 3.0 * g) * 0.5;
        let half_h = (work.height - 3.0 * g) * 0.5;

        let target = match zone {
            SnapZone::LeftHalf => Rect::new(work.x + g, work.y + g, half_w, work.height - 2.0 * g),
            SnapZone::RightHalf => Rect::new(
                work.x + 2.0 * g + half_w,
                work.y + g,
                half_w,
                work.height - 2.0 * g,
            ),
            SnapZone::TopLeftQuarter => Rect::new(work.x + g, work.y + g, half_w, half_h),
            SnapZone::TopRightQuarter => {
                Rect::new(work.x + 2.0 * g + half_w, work.y + g, half_w, half_h)
            }
            SnapZone::BottomLeftQuarter => {
                Rect::new(work.x + g, work.y + 2.0 * g + half_h, half_w, half_h)
            }
            SnapZone::BottomRightQuarter => Rect::new(
                work.x + 2.0 * g + half_w,
                work.y + 2.0 * g + half_h,
                half_w,
                half_h,
            ),
            SnapZone::MaximizeFill => Rect::new(
                work.x + g,
                work.y + g,
                work.width - 2.0 * g,
                work.height - 2.0 * g,
            ),
        };

        let win = self.windows.iter_mut().find(|w| w.id == id)?;
        win.flags
            .remove(WindowStateFlags::MAXIMIZED | WindowStateFlags::FULLSCREEN);
        win.floating_rect = target;
        win.computed_rect = target;
        Some(target)
    }

    pub fn recompute_layout(&mut self) {
        self.repair_focus();
        let work = self.usable_work_area();
        for win in &mut self.windows {
            win.flags.remove(WindowStateFlags::STAGED);
        }
        let visible_indices: Vec<usize> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.flags.contains(WindowStateFlags::MINIMIZED))
            .map(|(i, _)| i)
            .collect();

        if visible_indices.is_empty() {
            return;
        }

        match self.mode {
            LayoutMode::Floating => {
                for idx in visible_indices {
                    let win = &mut self.windows[idx];
                    win.flags.remove(WindowStateFlags::STAGED);
                    if win.flags.contains(WindowStateFlags::MAXIMIZED) {
                        win.computed_rect = work;
                    } else {
                        win.computed_rect = win.floating_rect;
                    }
                }
            }
            LayoutMode::StageManager => {
                let gap = nonnegative(self.outer_gap).min(work.width.min(work.height) * 0.1);
                let strip_width = 196.0_f32.min(work.width * 0.25);
                let stage_x = work.x + strip_width + gap * 2.0;
                let stage_w = work.width - strip_width - gap * 3.0;
                let stage_h = work.height - gap * 2.0;
                let strip_count = visible_indices.len().saturating_sub(1).max(1) as f32;
                let inner = nonnegative(self.inner_gap).min(stage_h / strip_count * 0.25);
                let thumb_h = 132.0_f32.min((stage_h - inner * (strip_count - 1.0)) / strip_count);

                let mut strip_slot = 0usize;
                for &idx in &visible_indices {
                    let win = &mut self.windows[idx];
                    if win.flags.contains(WindowStateFlags::FOCUSED) {
                        win.flags.remove(WindowStateFlags::STAGED);
                        win.computed_rect = Rect::new(stage_x, work.y + gap, stage_w, stage_h);
                    } else {
                        win.flags.insert(WindowStateFlags::STAGED);
                        let thumb_y = work.y + gap + (strip_slot as f32) * (thumb_h + inner);
                        win.computed_rect = Rect::new(work.x + gap, thumb_y, strip_width, thumb_h);
                        strip_slot += 1;
                    }
                }
            }
            LayoutMode::DynamicTiling => {
                let n = visible_indices.len();
                let g = nonnegative(self.outer_gap).min(work.width.min(work.height) * 0.1);
                let stack_count = n.saturating_sub(1).max(1) as f32;
                let ig = nonnegative(self.inner_gap)
                    .min((work.height - 2.0 * g) / stack_count * 0.25)
                    .min((work.width - 2.0 * g) * 0.25);
                if n == 1 {
                    let win = &mut self.windows[visible_indices[0]];
                    win.computed_rect = Rect::new(
                        work.x + g,
                        work.y + g,
                        work.width - 2.0 * g,
                        work.height - 2.0 * g,
                    );
                } else {
                    let master_w = (work.width - 2.0 * g - ig) * 0.55;
                    let stack_w = work.width - 2.0 * g - ig - master_w;
                    let stack_count = (n - 1) as f32;
                    let stack_h = (work.height - 2.0 * g - ig * (stack_count - 1.0)) / stack_count;

                    for (pos, &idx) in visible_indices.iter().enumerate() {
                        let win = &mut self.windows[idx];
                        if pos == 0 {
                            win.computed_rect =
                                Rect::new(work.x + g, work.y + g, master_w, work.height - 2.0 * g);
                        } else {
                            let s_idx = (pos - 1) as f32;
                            win.computed_rect = Rect::new(
                                work.x + g + master_w + ig,
                                work.y + g + s_idx * (stack_h + ig),
                                stack_w,
                                stack_h,
                            );
                        }
                    }
                }
            }
        }
        // Explicit maximize/fullscreen requests remain stable across layout changes.
        for win in &mut self.windows {
            if win.flags.contains(WindowStateFlags::MINIMIZED) {
                continue;
            }
            if win.flags.contains(WindowStateFlags::FULLSCREEN) {
                win.computed_rect = self.output_rect.sanitized();
                win.flags.remove(WindowStateFlags::STAGED);
            } else if win.flags.contains(WindowStateFlags::MAXIMIZED) {
                win.computed_rect = work;
                win.flags.remove(WindowStateFlags::STAGED);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_manager_centers_active_window_and_strips_background_windows() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 1920.0, 1080.0));
        wm.mode = LayoutMode::StageManager;
        wm.add_window(1, "org.aura.Terminal", "Terminal", (800.0, 600.0));
        wm.add_window(2, "org.aura.Browser", "Browser", (1000.0, 700.0));

        let w1 = &wm.windows()[0];
        let w2 = &wm.windows()[1];
        assert!(w1.flags.contains(WindowStateFlags::STAGED));
        assert!(w2.flags.contains(WindowStateFlags::FOCUSED));
        assert!(w2.computed_rect.x > w1.computed_rect.x + w1.computed_rect.width);
    }

    #[test]
    fn traffic_light_minimize_and_close_preserve_focus_invariants() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 1920.0, 1080.0));
        wm.add_window(10, "app.one", "One", (600.0, 400.0));
        wm.add_window(20, "app.two", "Two", (600.0, 400.0));
        assert!(wm.apply_traffic_light(20, TrafficLightAction::MinimizeToDock));
        assert!(wm.windows()[1].flags.contains(WindowStateFlags::MINIMIZED));
        assert!(wm.apply_traffic_light(20, TrafficLightAction::Close));
        assert_eq!(wm.windows().len(), 1);
        assert!(wm.windows()[0].flags.contains(WindowStateFlags::FOCUSED));
    }

    fn focused_visible_ids(wm: &WorkspaceManager) -> Vec<WindowId> {
        wm.windows()
            .iter()
            .filter(|w| {
                w.flags.contains(WindowStateFlags::FOCUSED)
                    && !w.flags.contains(WindowStateFlags::MINIMIZED)
            })
            .map(|w| w.id)
            .collect()
    }

    #[test]
    fn minimize_restore_and_close_keep_focus_on_visible_windows() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 1920.0, 1080.0));
        wm.mode = LayoutMode::StageManager;
        wm.add_window(1, "one", "One", (800.0, 600.0));
        wm.add_window(2, "two", "Two", (800.0, 600.0));
        assert!(wm.apply_traffic_light(2, TrafficLightAction::MinimizeToDock));
        assert_eq!(focused_visible_ids(&wm), vec![1]);
        assert!(!wm.windows()[0].flags.contains(WindowStateFlags::STAGED));
        assert!(!wm.focus_window(2));
        assert!(wm.restore_window(2));
        assert_eq!(focused_visible_ids(&wm), vec![2]);
        wm.apply_traffic_light(2, TrafficLightAction::MinimizeToDock);
        assert!(wm.remove_window(1));
        assert!(focused_visible_ids(&wm).is_empty());
        assert!(!wm.windows()[0].flags.contains(WindowStateFlags::FOCUSED));
        assert!(wm.restore_window(2));
        assert_eq!(focused_visible_ids(&wm), vec![2]);
    }

    #[test]
    fn duplicate_ids_and_unknown_actions_do_not_mutate_workspace() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        assert!(wm.add_window(1, "one", "One", (500.0, 300.0)));
        let snapshot = wm.windows().to_vec();
        assert!(!wm.add_window(1, "different", "Duplicate", (900.0, 900.0)));
        assert!(!wm.remove_window(99));
        assert!(!wm.restore_window(99));
        assert!(!wm.focus_window(99));
        assert_eq!(wm.windows(), snapshot);
    }

    #[test]
    fn every_layout_fits_small_outputs_and_many_windows() {
        for (width, height) in [(240.0, 100.0), (1.0, 1.0), (800.0, 600.0)] {
            let mut wm = WorkspaceManager::new(Rect::new(40.0, 80.0, width, height));
            for id in 0..12 {
                wm.add_window(id, "test", "Test", (900.0, 900.0));
            }
            let work = wm.usable_work_area();
            for mode in [
                LayoutMode::Floating,
                LayoutMode::StageManager,
                LayoutMode::DynamicTiling,
            ] {
                wm.mode = mode;
                wm.recompute_layout();
                for window in wm.windows() {
                    let r = window.computed_rect;
                    assert!(r.width > 0.0 && r.height > 0.0, "{mode:?}: {r:?}");
                    assert!(
                        r.x >= work.x - 0.001 && r.y >= work.y - 0.001,
                        "{mode:?}: {r:?}"
                    );
                    assert!(
                        r.x + r.width <= work.x + work.width + 0.001,
                        "{mode:?}: {r:?}"
                    );
                    assert!(
                        r.y + r.height <= work.y + work.height + 0.001,
                        "{mode:?}: {r:?}"
                    );
                    if mode != LayoutMode::StageManager {
                        assert!(!window.flags.contains(WindowStateFlags::STAGED));
                    }
                }
            }
        }
    }

    #[test]
    fn resize_and_invalid_geometry_keep_windows_finite_and_inside_work_area() {
        let mut wm = WorkspaceManager::new(Rect::new(f32::NAN, f32::INFINITY, -3.0, 0.0));
        assert!(wm.add_window(1, "test", "Test", (f32::NAN, f32::INFINITY)));
        wm.resize_output(Rect::new(20.0, 30.0, 240.0, 180.0));
        let work = wm.usable_work_area();
        let r = wm.windows()[0].computed_rect;
        assert!(r.x.is_finite() && r.y.is_finite());
        assert!(r.width > 0.0 && r.height > 0.0);
        assert!(r.x >= work.x && r.y >= work.y);
        assert!(r.x + r.width <= work.x + work.width);
        assert!(r.y + r.height <= work.y + work.height);
    }

    #[test]
    fn snapping_a_maximized_window_persists_after_layout_recompute() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        wm.add_window(1, "one", "One", (500.0, 300.0));
        wm.apply_traffic_light(1, TrafficLightAction::ToggleZoomFullscreen);
        let snapped = wm.snap_window(1, SnapZone::LeftHalf).unwrap();
        wm.recompute_layout();
        assert_eq!(wm.windows()[0].computed_rect, snapped);
        assert!(!wm.windows()[0].flags.contains(WindowStateFlags::MAXIMIZED));
    }

    #[test]
    fn interactive_geometry_and_metadata_updates_preserve_identity() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        wm.add_window(1, "old", "Old", (400.0, 300.0));
        assert!(wm.update_window_metadata(1, "app.new", "New"));
        assert!(wm.set_floating_rect(1, Rect::new(-100.0, -100.0, 900.0, 900.0)));
        assert_eq!(wm.windows()[0].computed_rect, wm.usable_work_area());
        assert_eq!(wm.windows()[0].id, 1);
        assert_eq!(wm.windows()[0].app_id, "app.new");
        assert_eq!(wm.windows()[0].title, "New");
        let snapshot = wm.windows().to_vec();
        assert!(!wm.set_floating_rect(1, Rect::new(f32::NAN, 0.0, 5.0, 5.0)));
        assert!(!wm.set_floating_rect(99, Rect::new(0.0, 0.0, 5.0, 5.0)));
        assert_eq!(wm.windows(), snapshot);
    }

    #[test]
    fn maximized_geometry_survives_recompute_in_all_layout_modes() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        wm.add_window(1, "one", "One", (400.0, 300.0));
        wm.apply_traffic_light(1, TrafficLightAction::ToggleZoomFullscreen);
        for mode in [
            LayoutMode::Floating,
            LayoutMode::StageManager,
            LayoutMode::DynamicTiling,
        ] {
            wm.mode = mode;
            wm.recompute_layout();
            assert_eq!(wm.windows()[0].computed_rect, wm.usable_work_area());
        }
    }

    #[test]
    fn fullscreen_is_idempotent_and_restores_floating_geometry_without_stealing_focus() {
        let mut wm = WorkspaceManager::new(Rect::new(25.0, 50.0, 800.0, 600.0));
        wm.add_window(1, "one", "One", (400.0, 300.0));
        wm.add_window(2, "two", "Two", (300.0, 200.0));
        let restore_rect = wm.windows()[0].floating_rect;
        assert!(wm.set_fullscreen(1, true));
        assert_eq!(wm.windows()[0].computed_rect, wm.output_rect);
        assert_eq!(wm.windows()[0].floating_rect, restore_rect);
        assert_eq!(focused_visible_ids(&wm), vec![2]);
        let fullscreen = wm.windows().to_vec();
        assert!(wm.set_fullscreen(1, true));
        assert_eq!(wm.windows(), fullscreen);
        assert!(wm.set_fullscreen(1, false));
        assert_eq!(wm.windows()[0].computed_rect, restore_rect);
        assert_eq!(wm.windows()[0].floating_rect, restore_rect);
        assert_eq!(focused_visible_ids(&wm), vec![2]);
        let restored = wm.windows().to_vec();
        assert!(wm.set_fullscreen(1, false));
        assert!(!wm.set_fullscreen(99, true));
        assert_eq!(wm.windows(), restored);
    }

    #[test]
    fn fullscreen_restores_maximize_and_survives_layout_and_output_changes() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        wm.add_window(1, "one", "One", (400.0, 300.0));
        let floating = wm.windows()[0].floating_rect;
        wm.apply_traffic_light(1, TrafficLightAction::ToggleZoomFullscreen);
        wm.set_fullscreen(1, true);
        for mode in [
            LayoutMode::Floating,
            LayoutMode::StageManager,
            LayoutMode::DynamicTiling,
        ] {
            wm.mode = mode;
            wm.recompute_layout();
            assert_eq!(wm.windows()[0].computed_rect, wm.output_rect);
            assert!(wm.windows()[0].flags.contains(WindowStateFlags::MAXIMIZED));
            assert!(!wm.windows()[0].flags.contains(WindowStateFlags::STAGED));
        }
        wm.resize_output(Rect::new(20.0, 40.0, 1200.0, 900.0));
        assert_eq!(wm.windows()[0].computed_rect, wm.output_rect);
        wm.set_fullscreen(1, false);
        assert_eq!(wm.windows()[0].computed_rect, wm.usable_work_area());
        assert_eq!(wm.windows()[0].floating_rect, floating);
        assert_eq!(focused_visible_ids(&wm), vec![1]);
    }

    #[test]
    fn fullscreen_request_does_not_restore_a_minimized_window() {
        let mut wm = WorkspaceManager::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        wm.add_window(1, "one", "One", (400.0, 300.0));
        wm.add_window(2, "two", "Two", (300.0, 200.0));
        wm.apply_traffic_light(1, TrafficLightAction::MinimizeToDock);
        let restore_rect = wm.windows()[0].floating_rect;
        wm.set_fullscreen(1, true);
        assert!(wm.windows()[0].flags.contains(WindowStateFlags::MINIMIZED));
        assert_eq!(focused_visible_ids(&wm), vec![2]);
        assert_eq!(wm.windows()[0].floating_rect, restore_rect);
        wm.restore_window(1);
        assert_eq!(wm.windows()[0].computed_rect, wm.output_rect);
        wm.set_fullscreen(1, false);
        assert_eq!(wm.windows()[0].computed_rect, restore_rect);
    }
}
