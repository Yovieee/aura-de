//! Conservative eligibility model only. Actual KMS plane tests belong to the
//! compositor, and must still succeed before it bypasses composition.

use aura_core::{Rect, WindowNode, WindowStateFlags};

#[derive(Debug, Clone, Copy)]
pub struct ScanoutConditions {
    pub output: Rect,
    pub buffer_format_and_modifier_supported: bool,
    pub opaque_buffer: bool,
    pub identity_transform_and_scale: bool,
    pub no_overlays: bool,
    pub no_capture: bool,
    pub color_pipeline_compatible: bool,
    pub kms_atomic_test_passed: bool,
}

#[allow(dead_code)]
pub fn can_direct_scanout(window: &WindowNode, conditions: &ScanoutConditions) -> bool {
    let rect = window.computed_rect;
    let output = conditions.output;
    let geometry = [
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        output.x,
        output.y,
        output.width,
        output.height,
    ];
    geometry.iter().all(|value| value.is_finite())
        && output.width > 0.0
        && output.height > 0.0
        && rect == output
        && window
            .flags
            .contains(WindowStateFlags::FULLSCREEN | WindowStateFlags::FOCUSED)
        && !window.flags.intersects(
            WindowStateFlags::MINIMIZED | WindowStateFlags::STAGED | WindowStateFlags::LIQUID_BLUR,
        )
        && conditions.buffer_format_and_modifier_supported
        && conditions.opaque_buffer
        && conditions.identity_transform_and_scale
        && conditions.no_overlays
        && conditions.no_capture
        && conditions.color_pipeline_compatible
        && conditions.kms_atomic_test_passed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible() -> (WindowNode, ScanoutConditions) {
        let output = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        (
            WindowNode {
                id: 1,
                app_id: "test".into(),
                title: "test".into(),
                floating_rect: output,
                computed_rect: output,
                flags: WindowStateFlags::FOCUSED | WindowStateFlags::FULLSCREEN,
            },
            ScanoutConditions {
                output,
                buffer_format_and_modifier_supported: true,
                opaque_buffer: true,
                identity_transform_and_scale: true,
                no_overlays: true,
                no_capture: true,
                color_pipeline_compatible: true,
                kms_atomic_test_passed: true,
            },
        )
    }

    #[test]
    fn every_plane_capture_and_visibility_constraint_is_required() {
        let (window, conditions) = eligible();
        assert!(can_direct_scanout(&window, &conditions));
        let denied = [
            ScanoutConditions {
                buffer_format_and_modifier_supported: false,
                ..conditions
            },
            ScanoutConditions {
                opaque_buffer: false,
                ..conditions
            },
            ScanoutConditions {
                identity_transform_and_scale: false,
                ..conditions
            },
            ScanoutConditions {
                no_overlays: false,
                ..conditions
            },
            ScanoutConditions {
                no_capture: false,
                ..conditions
            },
            ScanoutConditions {
                color_pipeline_compatible: false,
                ..conditions
            },
            ScanoutConditions {
                kms_atomic_test_passed: false,
                ..conditions
            },
        ];
        for failed in denied {
            assert!(!can_direct_scanout(&window, &failed));
        }
        for flag in [
            WindowStateFlags::MINIMIZED,
            WindowStateFlags::STAGED,
            WindowStateFlags::LIQUID_BLUR,
        ] {
            let mut hidden = window.clone();
            hidden.flags.insert(flag);
            assert!(!can_direct_scanout(&hidden, &conditions));
        }
    }

    #[test]
    fn fullscreen_flag_is_not_enough_without_exact_output_coverage() {
        let (mut window, conditions) = eligible();
        window.computed_rect.width -= 1.0;
        assert!(!can_direct_scanout(&window, &conditions));
        window.computed_rect.width = f32::NAN;
        assert!(!can_direct_scanout(&window, &conditions));
    }
}
