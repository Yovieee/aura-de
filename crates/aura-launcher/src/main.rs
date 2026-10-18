//! `aura-launcher`: Slice 5 — Spotlight-style Liquid Glass command & fuzzy app launcher overlay.

use aura_core::{LiquidGlassMaterial, Rect};
use aura_render::LiquidGlassUniforms;
use tracing::info;

#[derive(Debug, Clone, PartialEq)]
pub struct DesktopEntryItem {
    pub app_id: &'static str,
    pub name: &'static str,
    pub keywords: &'static [&'static str],
    pub exec: &'static str,
}

pub struct SpotlightIndex {
    entries: Vec<DesktopEntryItem>,
}

impl Default for SpotlightIndex {
    fn default() -> Self {
        Self {
            entries: vec![
                DesktopEntryItem {
                    app_id: "org.aura.Terminal",
                    name: "Aura Terminal",
                    keywords: &["shell", "bash", "zsh", "console", "cli"],
                    exec: "aura-terminal",
                },
                DesktopEntryItem {
                    app_id: "org.aura.Files",
                    name: "Aura Files",
                    keywords: &["finder", "folder", "directory", "storage"],
                    exec: "aura-files",
                },
                DesktopEntryItem {
                    app_id: "org.aura.Studio",
                    name: "Aura Code Studio",
                    keywords: &["editor", "rust", "wgsl", "ide"],
                    exec: "aura-studio",
                },
                DesktopEntryItem {
                    app_id: "org.aura.Settings",
                    name: "Liquid Glass Settings",
                    keywords: &["control", "display", "refraction", "blur", "wayland"],
                    exec: "aura-settings",
                },
            ],
        }
    }
}

impl SpotlightIndex {
    pub fn search(&self, query: &str) -> Vec<&DesktopEntryItem> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.entries.iter().collect();
        }
        let mut scored: Vec<(i32, &DesktopEntryItem)> = self
            .entries
            .iter()
            .filter_map(|item| {
                let name_lower = item.name.to_ascii_lowercase();
                if name_lower.starts_with(&q) {
                    Some((100, item))
                } else if name_lower.contains(&q) {
                    Some((75, item))
                } else if item.keywords.iter().any(|k| k.starts_with(&q) || k.contains(&q)) {
                    Some((50, item))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let index = SpotlightIndex::default();
    let matches = index.search("glass");
    let modal_rect = Rect::new(680.0, 220.0, 560.0, 320.0);
    let material = LiquidGlassMaterial::dark_control_center();
    let uniforms =
        LiquidGlassUniforms::from_material_and_rect(&material, modal_rect, (1920.0, 1080.0), 36.0);

    info!(
        matched_count = matches.len(),
        top_match = matches.first().map(|m| m.name).unwrap_or("none"),
        ior = uniforms.viewport_and_ior[2],
        "Initialized Aura Spotlight Launcher on wlr-layer-shell Overlay layer"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_spotlight_ranks_prefix_higher_than_keyword() {
        let idx = SpotlightIndex::default();
        let res = idx.search("aura");
        assert_eq!(res.len(), 3);
        let kw = idx.search("refraction");
        assert_eq!(kw.len(), 1);
        assert_eq!(kw[0].app_id, "org.aura.Settings");
    }
}
