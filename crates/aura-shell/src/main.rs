//! `aura-shell`: Wayland `wlr-layer-shell` Menu Bar & Parabolic Liquid Glass Dock client.

mod ipc;

use aura_core::{DockConfig, DockMagnifier, LiquidGlassMaterial, Rect};
use aura_render::{LiquidGlassUniforms, LIQUID_GLASS_WGSL_SOURCE};
use ipc::AuraShellService;
use tracing::info;

#[derive(Debug, Clone)]
pub struct DockAppEntry {
    pub app_id: &'static str,
    pub label: &'static str,
    pub is_running: bool,
}

pub struct ShellPanelState {
    pub output_width: f32,
    pub output_height: f32,
    pub dock_entries: Vec<DockAppEntry>,
    pub magnifier: DockMagnifier,
    pub glass_material: LiquidGlassMaterial,
}

impl ShellPanelState {
    pub fn new(output_width: f32, output_height: f32) -> Self {
        Self {
            output_width,
            output_height,
            dock_entries: vec![
                DockAppEntry { app_id: "org.aura.Finder", label: "Files", is_running: true },
                DockAppEntry { app_id: "org.aura.Launcher", label: "Applications", is_running: false },
                DockAppEntry { app_id: "org.aura.Terminal", label: "Terminal", is_running: true },
                DockAppEntry { app_id: "org.aura.Browser", label: "Web", is_running: true },
                DockAppEntry { app_id: "org.aura.Code", label: "Code Editor", is_running: false },
                DockAppEntry { app_id: "org.aura.Music", label: "Music", is_running: false },
                DockAppEntry { app_id: "org.aura.Settings", label: "System Settings", is_running: true },
            ],
            magnifier: DockMagnifier::new(DockConfig::default()),
            glass_material: LiquidGlassMaterial::macos_dock_default(),
        }
    }

    pub fn compute_dock_frame(&self, cursor_x_from_center: Option<f32>) -> (Rect, LiquidGlassUniforms) {
        let (_items, pill_width, pill_height) = self
            .magnifier
            .compute_layout(self.dock_entries.len(), cursor_x_from_center);

        let bottom_margin = 10.0;
        let dock_rect = Rect::new(
            (self.output_width - pill_width) * 0.5,
            self.output_height - pill_height - bottom_margin,
            pill_width,
            pill_height,
        );

        let uniforms = LiquidGlassUniforms::from_material_and_rect(
            &self.glass_material,
            dock_rect,
            (self.output_width, self.output_height),
            28.0,
        );
        (dock_rect, uniforms)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let shell = ShellPanelState::new(1920.0, 1080.0);
    let (dock_rect, dock_uniforms) = shell.compute_dock_frame(Some(0.0));
    let _dbus_service = AuraShellService::default();

    info!(
        wgsl_shader_bytes = LIQUID_GLASS_WGSL_SOURCE.len(),
        dock_apps = shell.dock_entries.len(),
        ?dock_rect,
        ior = dock_uniforms.viewport_and_ior[2],
        "Initialized Aura Shell (Menu Bar + Parabolic Liquid Glass Dock + zbus service)"
    );
    Ok(())
}
