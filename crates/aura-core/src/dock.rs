//! macOS-inspired Floating Dock continuous parabolic magnification solver.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DockConfig {
    pub base_icon_size: f32,
    pub max_icon_size: f32,
    pub icon_spacing: f32,
    pub pill_padding_x: f32,
    pub pill_padding_y: f32,
    pub magnification_radius: f32,
}

impl Default for DockConfig {
    fn default() -> Self {
        Self {
            base_icon_size: 52.0,
            max_icon_size: 84.0,
            icon_spacing: 10.0,
            pill_padding_x: 16.0,
            pill_padding_y: 10.0,
            magnification_radius: 156.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DockItemLayout {
    pub index: usize,
    pub center_x: f32,
    pub size: f32,
    pub scale_factor: f32,
}

pub struct DockMagnifier {
    pub config: DockConfig,
}

impl DockMagnifier {
    pub fn new(config: DockConfig) -> Self {
        Self { config }
    }

    pub fn compute_layout(
        &self,
        item_count: usize,
        cursor_x_from_center: Option<f32>,
    ) -> (Vec<DockItemLayout>, f32, f32) {
        if item_count == 0 {
            return (
                Vec::new(),
                self.config.pill_padding_x * 2.0,
                self.config.base_icon_size + self.config.pill_padding_y * 2.0,
            );
        }

        let step = self.config.base_icon_size + self.config.icon_spacing;
        let base_total_width = (item_count as f32) * self.config.base_icon_size
            + ((item_count.saturating_sub(1)) as f32) * self.config.icon_spacing;
        let start_center_x = -base_total_width * 0.5 + self.config.base_icon_size * 0.5;

        let mut sizes = Vec::with_capacity(item_count);
        let mut scales = Vec::with_capacity(item_count);
        for i in 0..item_count {
            let unmagnified_center = start_center_x + (i as f32) * step;
            let scale = match cursor_x_from_center {
                Some(cx) => {
                    let dist = (unmagnified_center - cx).abs();
                    if dist >= self.config.magnification_radius {
                        0.0
                    } else {
                        let norm = dist / self.config.magnification_radius;
                        0.5 * (1.0 + (norm * std::f32::consts::PI).cos())
                    }
                }
                None => 0.0,
            };
            let size = self.config.base_icon_size
                + scale * (self.config.max_icon_size - self.config.base_icon_size);
            sizes.push(size);
            scales.push(scale);
        }

        let expanded_icons_width: f32 = sizes.iter().sum::<f32>()
            + ((item_count.saturating_sub(1)) as f32) * self.config.icon_spacing;
        let mut cursor_edge = -expanded_icons_width * 0.5;
        let mut items = Vec::with_capacity(item_count);

        for i in 0..item_count {
            let size = sizes[i];
            let center_x = cursor_edge + size * 0.5;
            items.push(DockItemLayout {
                index: i,
                center_x,
                size,
                scale_factor: scales[i],
            });
            cursor_edge += size + self.config.icon_spacing;
        }

        let pill_width = expanded_icons_width + self.config.pill_padding_x * 2.0;
        let pill_height = self.config.base_icon_size + self.config.pill_padding_y * 2.0;
        (items, pill_width, pill_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_magnification_peaks_under_cursor_and_expands_pill_symmetrically() {
        let mag = DockMagnifier::new(DockConfig::default());
        let (unmag_items, unmag_width, _) = mag.compute_layout(5, None);
        let (mag_items, mag_width, _) = mag.compute_layout(5, Some(0.0));

        assert!(mag_width > unmag_width);
        assert!((mag_items[2].size - 84.0).abs() < 1e-4);
        assert!(mag_items[2].size > mag_items[1].size);
        assert!(mag_items[1].size > mag_items[0].size);
        assert!((mag_items[0].center_x + mag_items[4].center_x).abs() < 1e-4);
        assert_eq!(unmag_items.len(), 5);
    }
}
