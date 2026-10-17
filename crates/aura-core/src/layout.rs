//! Hybrid macOS Floating, Stage Manager strip, and Smart-Snap Tiling window layout engine.

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
        Self { x, y, width, height }
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
            output_rect,
            menu_bar_height: 32.0,
            dock_reserved_height: 84.0,
            outer_gap: 14.0,
            inner_gap: 12.0,
            windows: Vec::new(),
        }
    }

    pub fn usable_work_area(&self) -> Rect {
        let y = self.output_rect.y + self.menu_bar_height;
        let height = (self.output_rect.height - self.menu_bar_height - self.dock_reserved_height)
            .max(100.0);
        Rect::new(self.output_rect.x, y, self.output_rect.width, height)
    }

    pub fn windows(&self) -> &[WindowNode] {
        &self.windows
    }

    pub fn add_window(&mut self, id: WindowId, app_id: impl Into<String>, title: impl Into<String>, initial_size: (f32, f32)) {
        let work = self.usable_work_area();
        let w = initial_size.0.min(work.width - 40.0).max(320.0);
        let h = initial_size.1.min(work.height - 40.0).max(240.0);
        let cascade_offset = (self.windows.len() as f32 * 26.0) % 130.0;
        let rect = Rect::new(
            work.x + (work.width - w) * 0.5 + cascade_offset,
            work.y + (work.height - h) * 0.5 + cascade_offset,
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
    }

    pub fn apply_traffic_light(&mut self, id: WindowId, action: TrafficLightAction) -> bool {
        match action {
            TrafficLightAction::Close => {
                let len_before = self.windows.len();
                self.windows.retain(|w| w.id != id);
                if let Some(last) = self.windows.last_mut() {
                    last.flags.insert(WindowStateFlags::FOCUSED);
                }
                self.recompute_layout();
                self.windows.len() != len_before
            }
            TrafficLightAction::MinimizeToDock => {
                if let Some(win) = self.windows.iter_mut().find(|w| w.id == id) {
                    win.flags.insert(WindowStateFlags::MINIMIZED);
                    win.flags.remove(WindowStateFlags::FOCUSED);
                    self.recompute_layout();
                    true
                } else {
                    false
                }
            }
            TrafficLightAction::ToggleZoomFullscreen => {
                let work = self.usable_work_area();
                if let Some(win) = self.windows.iter_mut().find(|w| w.id == id) {
                    if win.flags.contains(WindowStateFlags::MAXIMIZED) {
                        win.flags.remove(WindowStateFlags::MAXIMIZED);
                        win.computed_rect = win.floating_rect;
                    } else {
                        win.flags.insert(WindowStateFlags::MAXIMIZED);
                        win.computed_rect = work;
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn snap_window(&mut self, id: WindowId, zone: SnapZone) -> Option<Rect> {
        let work = self.usable_work_area();
        let g = self.outer_gap;
        let half_w = (work.width - 3.0 * g) * 0.5;
        let half_h = (work.height - 3.0 * g) * 0.5;

        let target = match zone {
            SnapZone::LeftHalf => Rect::new(work.x + g, work.y + g, half_w, work.height - 2.0 * g),
            SnapZone::RightHalf => Rect::new(work.x + 2.0 * g + half_w, work.y + g, half_w, work.height - 2.0 * g),
            SnapZone::TopLeftQuarter => Rect::new(work.x + g, work.y + g, half_w, half_h),
            SnapZone::TopRightQuarter => Rect::new(work.x + 2.0 * g + half_w, work.y + g, half_w, half_h),
            SnapZone::BottomLeftQuarter => Rect::new(work.x + g, work.y + 2.0 * g + half_h, half_w, half_h),
            SnapZone::BottomRightQuarter => Rect::new(work.x + 2.0 * g + half_w, work.y + 2.0 * g + half_h, half_w, half_h),
            SnapZone::MaximizeFill => Rect::new(work.x + g, work.y + g, work.width - 2.0 * g, work.height - 2.0 * g),
        };

        let win = self.windows.iter_mut().find(|w| w.id == id)?;
        win.floating_rect = target;
        win.computed_rect = target;
        Some(target)
    }

    pub fn recompute_layout(&mut self) {
        let work = self.usable_work_area();
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
                let strip_width = 196.0;
                let stage_x = work.x + strip_width + self.outer_gap * 2.0;
                let stage_w = (work.width - strip_width - self.outer_gap * 3.0).max(320.0);
                let stage_h = (work.height - self.outer_gap * 2.0).max(240.0);

                let mut strip_slot = 0usize;
                for &idx in &visible_indices {
                    let win = &mut self.windows[idx];
                    if win.flags.contains(WindowStateFlags::FOCUSED) {
                        win.flags.remove(WindowStateFlags::STAGED);
                        win.computed_rect = Rect::new(
                            stage_x,
                            work.y + self.outer_gap,
                            stage_w,
                            stage_h,
                        );
                    } else {
                        win.flags.insert(WindowStateFlags::STAGED);
                        let thumb_h = 132.0;
                        let thumb_y = work.y
                            + self.outer_gap
                            + (strip_slot as f32) * (thumb_h + self.inner_gap);
                        win.computed_rect = Rect::new(
                            work.x + self.outer_gap,
                            thumb_y,
                            strip_width,
                            thumb_h,
                        );
                        strip_slot += 1;
                    }
                }
            }
            LayoutMode::DynamicTiling => {
                let n = visible_indices.len();
                let g = self.outer_gap;
                let ig = self.inner_gap;
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
                    let stack_h =
                        (work.height - 2.0 * g - ig * (stack_count - 1.0)) / stack_count;

                    for (pos, &idx) in visible_indices.iter().enumerate() {
                        let win = &mut self.windows[idx];
                        if pos == 0 {
                            win.computed_rect = Rect::new(
                                work.x + g,
                                work.y + g,
                                master_w,
                                work.height - 2.0 * g,
                            );
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
}
