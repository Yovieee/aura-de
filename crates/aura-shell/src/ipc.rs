//! Pure-Rust D-Bus session service (`org.aura.Shell1`) powered by `zbus`.

use aura_core::{GlassQualityPreset, LayoutMode};
use zbus::interface;

#[derive(Debug, Clone)]
pub struct AuraShellService {
    pub active_mode: LayoutMode,
    pub quality_preset: GlassQualityPreset,
    pub do_not_disturb: bool,
    pub unread_notifications: u32,
}

impl Default for AuraShellService {
    fn default() -> Self {
        Self {
            active_mode: LayoutMode::Floating,
            quality_preset: GlassQualityPreset::Ultra,
            do_not_disturb: false,
            unread_notifications: 0,
        }
    }
}

#[interface(name = "org.aura.Shell1")]
impl AuraShellService {
    async fn set_layout_mode(&mut self, mode: &str) -> String {
        self.active_mode = match mode {
            "stage-manager" => LayoutMode::StageManager,
            "tiling" => LayoutMode::DynamicTiling,
            _ => LayoutMode::Floating,
        };
        format!("{:?}", self.active_mode)
    }

    async fn set_glass_quality(&mut self, preset: &str) -> String {
        self.quality_preset = match preset {
            "balanced" => GlassQualityPreset::Balanced,
            "reduced-transparency" => GlassQualityPreset::ReducedTransparency,
            _ => GlassQualityPreset::Ultra,
        };
        format!("{:?}", self.quality_preset)
    }

    async fn notify(&mut self, app_name: &str, summary: &str, _body: &str) -> u32 {
        if !self.do_not_disturb {
            self.unread_notifications += 1;
            tracing::info!(
                app_name,
                summary,
                count = self.unread_notifications,
                "Queued Liquid Glass notification toast"
            );
        }
        self.unread_notifications
    }
}
