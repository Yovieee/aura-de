//! Compositor state bridging `aura-core` layout/springs with Wayland surface lifecycles.

use std::collections::HashMap;
use aura_core::{
    GlassQualityPreset, LayoutMode, LiquidGlassMaterial, Rect, SnapZone, SpringConfig, SpringRect,
    TrafficLightAction, WindowId, WorkspaceManager,
};
use tracing::info;

#[derive(Debug, Clone)]
pub struct BlurRegionSurface {
    pub window_id: WindowId,
    pub rect: Rect,
    pub material: LiquidGlassMaterial,
    pub elevation_px: f32,
}

pub struct AuraCompositorState {
    pub socket_name: String,
    pub workspace: WorkspaceManager,
    pub glass_material: LiquidGlassMaterial,
    pub window_springs: HashMap<WindowId, SpringRect>,
    pub blur_regions: Vec<BlurRegionSurface>,
    pub frame_counter: u64,
    pub should_exit: bool,
}

impl AuraCompositorState {
    pub fn new(output_width: f32, output_height: f32, preset: GlassQualityPreset) -> Self {
        let output_rect = Rect::new(0.0, 0.0, output_width, output_height);
        let glass_material = LiquidGlassMaterial::from_preset(preset);

        Self {
            socket_name: String::from("wayland-aura-0"),
            workspace: WorkspaceManager::new(output_rect),
            glass_material,
            window_springs: HashMap::new(),
            blur_regions: Vec::new(),
            frame_counter: 0,
            should_exit: false,
        }
    }

    pub fn map_xdg_window(
        &mut self,
        id: WindowId,
        app_id: &str,
        title: &str,
        initial_size: (f32, f32),
    ) {
        self.workspace.add_window(id, app_id, title, initial_size);
        if let Some(node) = self.workspace.windows().iter().find(|w| w.id == id) {
            let r = node.computed_rect;
            let spring = SpringRect::new(
                r.x,
                r.y,
                r.width,
                r.height,
                SpringConfig::CRITICALLY_DAMPED,
            );
            self.window_springs.insert(id, spring);
            info!(
                window_id = id,
                app_id,
                title,
                ?r,
                "Mapped xdg_toplevel window with Liquid Glass decoration"
            );
        }
        self.sync_spring_targets();
    }

    pub fn set_layout_mode(&mut self, mode: LayoutMode) {
        self.workspace.mode = mode;
        self.workspace.recompute_layout();
        self.sync_spring_targets();
        info!(?mode, "Switched workspace layout mode");
    }

    pub fn handle_traffic_light(&mut self, id: WindowId, action: TrafficLightAction) {
        if self.workspace.apply_traffic_light(id, action) {
            if action == TrafficLightAction::Close {
                self.window_springs.remove(&id);
            }
            self.sync_spring_targets();
        }
    }

    pub fn snap_window(&mut self, id: WindowId, zone: SnapZone) {
        if self.workspace.snap_window(id, zone).is_some() {
            self.sync_spring_targets();
        }
    }

    fn sync_spring_targets(&mut self) {
        for win in self.workspace.windows() {
            let r = win.computed_rect;
            if let Some(spring) = self.window_springs.get_mut(&win.id) {
                spring.set_target(r.x, r.y, r.width, r.height);
            }
        }
    }

    pub fn tick_frame(&mut self, dt: f32) {
        self.frame_counter += 1;
        self.blur_regions.clear();

        for win in self.workspace.windows() {
            let animated_rect = if let Some(spring) = self.window_springs.get_mut(&win.id) {
                let (x, y, w, h) = spring.step(dt);
                Rect::new(x, y, w, h)
            } else {
                win.computed_rect
            };

            self.blur_regions.push(BlurRegionSurface {
                window_id: win.id,
                rect: animated_rect,
                material: self.glass_material,
                elevation_px: 24.0,
            });
        }
    }
}
