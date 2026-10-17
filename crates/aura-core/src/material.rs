//! Liquid Glass optical parameters and CPU reference math ($L_4$ squircle SDF & Snell refraction).

use glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum LiquidGlassError {
    #[error("index of refraction ({0}) must lie within physical dielectric range [1.0, 2.5]")]
    InvalidIndexOfRefraction(f32),
    #[error("corner radius ({0}) must be non-negative")]
    InvalidCornerRadius(f32),
    #[error("bevel width ({0}) must be positive and not exceed half the minimum panel dimension")]
    InvalidBevelWidth(f32),
    #[error("Dual-Kawase blur pass count ({0}) must be between 0 and 6")]
    InvalidBlurPasses(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GlassQualityPreset {
    /// Full 3-pass Dual-Kawase blur + RGB chromatic dispersion + Snell rim refraction.
    Ultra,
    /// 2-pass Dual-Kawase blur + single-channel Snell rim refraction (lower GPU bandwidth).
    Balanced,
    /// Accessibility / low-power fallback: high-opacity tinted squircle with specular border.
    ReducedTransparency,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LiquidGlassMaterial {
    /// Optical index of refraction (crown/acrylic glass = 1.48, water = 1.33).
    pub ior: f32,
    /// Chromatic dispersion delta between Red and Blue channels (e.g., 0.018).
    pub chromatic_dispersion: f32,
    /// Continuous-curvature ($L_4$ superellipse) corner radius in logical pixels.
    pub corner_radius: f32,
    /// Width of the curved refractive bevel along the perimeter in logical pixels.
    pub bevel_width: f32,
    /// Number of Dual-Kawase downsample/upsample pyramid passes (0..=6).
    pub kawase_blur_passes: u32,
    /// Normalized filter offset for Dual-Kawase kernels.
    pub kawase_offset: f32,
    /// Fresnel specular rim intensity along the top-left light vector.
    pub specular_intensity: f32,
    /// Base RGBA vibrancy tint (`[r, g, b, alpha]`).
    pub tint_rgba: [f32; 4],
}

impl LiquidGlassMaterial {
    /// Default macOS-inspired Liquid Glass preset for the Floating Dock and Top Menu Bar.
    pub fn macos_dock_default() -> Self {
        Self {
            ior: 1.48,
            chromatic_dispersion: 0.018,
            corner_radius: 24.0,
            bevel_width: 14.0,
            kawase_blur_passes: 3,
            kawase_offset: 1.25,
            specular_intensity: 0.72,
            tint_rgba: [0.94, 0.96, 1.0, 0.28],
        }
    }

    /// Preset for dark-mode Notification Banners and Control Center modules.
    pub fn dark_control_center() -> Self {
        Self {
            ior: 1.50,
            chromatic_dispersion: 0.022,
            corner_radius: 20.0,
            bevel_width: 12.0,
            kawase_blur_passes: 3,
            kawase_offset: 1.50,
            specular_intensity: 0.64,
            tint_rgba: [0.10, 0.12, 0.16, 0.42],
        }
    }

    /// Construct a material from a quality preset.
    pub fn from_preset(preset: GlassQualityPreset) -> Self {
        match preset {
            GlassQualityPreset::Ultra => Self::macos_dock_default(),
            GlassQualityPreset::Balanced => Self {
                chromatic_dispersion: 0.0,
                kawase_blur_passes: 2,
                ..Self::macos_dock_default()
            },
            GlassQualityPreset::ReducedTransparency => Self {
                ior: 1.0,
                chromatic_dispersion: 0.0,
                kawase_blur_passes: 0,
                tint_rgba: [0.16, 0.18, 0.22, 0.92],
                ..Self::macos_dock_default()
            },
        }
    }

    /// Validate physical and geometric invariants before uploading uniforms to the GPU.
    pub fn validate(&self, min_panel_dimension: f32) -> Result<(), LiquidGlassError> {
        if !(1.0..=2.5).contains(&self.ior) || self.ior.is_nan() {
            return Err(LiquidGlassError::InvalidIndexOfRefraction(self.ior));
        }
        if self.corner_radius < 0.0 || self.corner_radius.is_nan() {
            return Err(LiquidGlassError::InvalidCornerRadius(self.corner_radius));
        }
        if self.bevel_width <= 0.0
            || self.bevel_width > min_panel_dimension * 0.5
            || self.bevel_width.is_nan()
        {
            return Err(LiquidGlassError::InvalidBevelWidth(self.bevel_width));
        }
        if self.kawase_blur_passes > 6 {
            return Err(LiquidGlassError::InvalidBlurPasses(self.kawase_blur_passes));
        }
        Ok(())
    }

    /// Evaluate the $L_4$ superellipse (continuous-curvature macOS squircle) Signed Distance Field.
    pub fn squircle_sdf(&self, point_from_center: Vec2, half_extents: Vec2) -> f32 {
        let r = self.corner_radius.min(half_extents.x.min(half_extents.y));
        let q = point_from_center.abs() - (half_extents - Vec2::splat(r));
        let q_max = Vec2::new(q.x.max(0.0), q.y.max(0.0));
        let l4_outside = (q_max.x.powi(4) + q_max.y.powi(4)).powf(0.25);
        let inside = q.x.max(q.y).min(0.0);
        l4_outside + inside - r
    }

    /// Reconstruct the 3D outward surface normal on the curved glass bevel and compute
    /// the 2D UV refraction offset via Snell's law (`eta = 1.0 / ior`).
    pub fn refract_uv_offset(&self, point_from_center: Vec2, half_extents: Vec2) -> Vec2 {
        let sdf = self.squircle_sdf(point_from_center, half_extents);
        if sdf >= 0.0 || sdf <= -self.bevel_width || (self.ior - 1.0).abs() < f32::EPSILON {
            return Vec2::ZERO;
        }

        let bevel_t = ((sdf + self.bevel_width) / self.bevel_width).clamp(0.0, 1.0);
        let theta = bevel_t * (std::f32::consts::FRAC_PI_2 * 0.82);

        let eps = 0.5;
        let dx = self.squircle_sdf(point_from_center + Vec2::new(eps, 0.0), half_extents)
            - self.squircle_sdf(point_from_center - Vec2::new(eps, 0.0), half_extents);
        let dy = self.squircle_sdf(point_from_center + Vec2::new(0.0, eps), half_extents)
            - self.squircle_sdf(point_from_center - Vec2::new(0.0, eps), half_extents);
        let grad = Vec2::new(dx, dy).try_normalize().unwrap_or(Vec2::ZERO);

        let normal = Vec3::new(grad.x * theta.sin(), grad.y * theta.sin(), theta.cos()).normalize();
        let incident = Vec3::new(0.0, 0.0, -1.0);
        let eta = 1.0 / self.ior;

        let cos_i = (-incident.dot(normal)).clamp(-1.0, 1.0);
        let sin2_t = eta * eta * (1.0 - cos_i * cos_i);
        if sin2_t > 1.0 {
            return Vec2::ZERO;
        }
        let cos_t = (1.0 - sin2_t).sqrt();
        let refracted = eta * incident + (eta * cos_i - cos_t) * normal;
        Vec2::new(refracted.x, refracted.y) * self.bevel_width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_physical_parameters() {
        let mat = LiquidGlassMaterial::macos_dock_default();
        assert!(mat.validate(64.0).is_ok());

        let bad_ior = LiquidGlassMaterial { ior: 0.5, ..mat };
        assert_eq!(
            bad_ior.validate(64.0),
            Err(LiquidGlassError::InvalidIndexOfRefraction(0.5))
        );
    }

    #[test]
    fn squircle_sdf_is_negative_inside_and_positive_outside() {
        let mat = LiquidGlassMaterial::macos_dock_default();
        let half = Vec2::new(200.0, 36.0);
        assert!(mat.squircle_sdf(Vec2::ZERO, half) < -30.0);
        assert!(mat.squircle_sdf(Vec2::new(250.0, 0.0), half) > 40.0);
    }

    #[test]
    fn bevel_refraction_bends_inward_near_rim_and_zero_at_center() {
        let mat = LiquidGlassMaterial::macos_dock_default();
        let half = Vec2::new(200.0, 40.0);
        assert_eq!(mat.refract_uv_offset(Vec2::ZERO, half), Vec2::ZERO);
        let offset = mat.refract_uv_offset(Vec2::new(194.0, 0.0), half);
        assert!(offset.x < 0.0, "expected inward refraction along +X rim, got {offset:?}");
    }
}
