//! Shared drawing and hit-test geometry for the software-rendered Dock.
use aura_core::{DockConfig, DockMagnifier};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && x < f64::from(self.x + self.width)
            && y >= f64::from(self.y)
            && y < f64::from(self.y + self.height)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ItemLayout {
    pub index: usize,
    pub rect: Rect,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockLayout {
    pub launcher: Option<Rect>,
    pub apps: Vec<ItemLayout>,
    pub windows: Vec<ItemLayout>,
}

/// Keep the launcher stable while app tiles follow the core magnification model.
/// Every returned rectangle is bounded by this layer's logical viewport.
pub fn layout(
    width: u32,
    height: u32,
    app_count: usize,
    window_count: usize,
    cursor: Option<(f64, f64)>,
) -> DockLayout {
    let mut result = DockLayout::default();
    let (width, height) = (width as f32, height as f32);
    if width <= 8. || height <= 8. {
        return result;
    }
    result.launcher = Some(Rect {
        x: 8.,
        y: 8.,
        width: (width - 8.).min(56.),
        height: (height - 8.).min(48.),
    });
    let magnifier = DockMagnifier::new(DockConfig {
        base_icon_size: 44.,
        max_icon_size: 64.,
        icon_spacing: 10.,
        pill_padding_x: 16.,
        pill_padding_y: 0.,
        magnification_radius: 140.,
    });
    let count = app_count.min(5);
    let app_start = 76.;
    let available = (width - app_start - 8.).max(0.);
    let baseline = (height - 44.).clamp(0., 68.);
    if count > 0 && available >= 1. && baseline >= 1. {
        let (_, base_width, _) = magnifier.compute_layout(count, None);
        let base_scale = (available / base_width).min(1.);
        let cursor_x = cursor
            .filter(|(x, y)| {
                x.is_finite()
                    && y.is_finite()
                    && *x >= f64::from(app_start)
                    && *x < f64::from(width - 8.)
                    && *y >= 0.
                    && *y < f64::from(baseline)
            })
            .map(|(x, _)| (x as f32 - app_start) / base_scale - base_width * 0.5);
        let (items, expanded_width, _) = magnifier.compute_layout(count, cursor_x);
        let scale = (available / expanded_width)
            .min(baseline / magnifier.config.max_icon_size)
            .min(1.);
        result.apps = items
            .into_iter()
            .map(|item| {
                let size = item.size * scale;
                ItemLayout {
                    index: item.index,
                    rect: Rect {
                        x: app_start + (item.center_x + expanded_width * 0.5) * scale - size * 0.5,
                        y: baseline - size,
                        width: size,
                        height: size,
                    },
                }
            })
            .collect();
    }
    let count = window_count.min(6);
    if count > 0 && width >= 24. && height >= 96. {
        let step = ((width - 24.) / count as f32).min(112.);
        let tile_width = (step - 6.).max(1.);
        result.windows = (0..count)
            .map(|index| ItemLayout {
                index,
                rect: Rect {
                    x: 12. + index as f32 * step,
                    y: 72.,
                    width: tile_width,
                    height: 20.,
                },
            })
            .collect();
    }
    // Drawing and hit testing use the same integer logical pixel rectangles.
    for rect in result
        .launcher
        .iter_mut()
        .chain(result.apps.iter_mut().map(|item| &mut item.rect))
        .chain(result.windows.iter_mut().map(|item| &mut item.rect))
    {
        rect.x = rect.x.floor();
        rect.y = rect.y.floor();
        rect.width = rect.width.floor();
        rect.height = rect.height.floor();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_magnifies_actual_targets_and_leaving_restores_them() {
        let idle = layout(720, 112, 5, 6, None);
        let first = idle.apps[0].rect;
        let cursor = (f64::from(first.x + first.width * 0.5), 40.);
        let hover = layout(720, 112, 5, 6, Some(cursor));
        assert!(hover.apps[0].rect.width > first.width);
        assert_eq!(hover.apps[0].rect.width, 64.);
        assert!(hover.apps[0].rect.contains(cursor.0, cursor.1));
        assert!(hover.apps[1].rect.width > idle.apps[1].rect.width);
        assert_eq!(idle.launcher, hover.launcher);
        assert!(hover.launcher.unwrap().contains(28., 22.));
        assert_eq!(layout(720, 112, 5, 6, None), idle);
        assert_eq!(layout(720, 112, 5, 6, Some((200., 100.))), idle);
    }

    #[test]
    fn every_drawn_target_fits_narrow_and_small_viewports_without_overlap() {
        for width in [0, 8, 24, 80, 100, 180, 320, 720] {
            for height in [0, 8, 32, 64, 96, 112] {
                let geometry = layout(width, height, 8, 9, Some((114., 40.)));
                for rect in geometry
                    .launcher
                    .iter()
                    .chain(geometry.apps.iter().map(|item| &item.rect))
                    .chain(geometry.windows.iter().map(|item| &item.rect))
                {
                    assert!(rect.x >= 0. && rect.y >= 0.);
                    assert!(rect.x + rect.width <= width as f32 + 0.001);
                    assert!(rect.y + rect.height <= height as f32 + 0.001);
                }
                for pair in geometry.apps.windows(2) {
                    assert!(pair[0].rect.x + pair[0].rect.width <= pair[1].rect.x);
                }
            }
        }
    }

    #[test]
    fn gaps_and_window_rows_have_distinct_stable_hit_targets() {
        let geometry = layout(720, 112, 3, 2, Some((114., 40.)));
        let app = geometry.apps[0].rect;
        assert!(!app.contains(f64::from(app.x + app.width + 1.), 40.));
        let window = geometry.windows[1].rect;
        let hit = geometry.windows.iter().find(|item| {
            item.rect.contains(
                f64::from(window.x + window.width * 0.5),
                f64::from(window.y + 5.),
            )
        });
        assert_eq!(hit.unwrap().index, 1);
        assert!(
            geometry
                .apps
                .iter()
                .all(|item| { !item.rect.contains(f64::from(window.x), f64::from(window.y)) })
        );
    }
}
