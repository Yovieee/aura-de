//! `aura-render`: GPU uniform buffers, Dual-Kawase pyramid planner, and embedded shaders.

use aura_core::{LiquidGlassMaterial, Rect};
use bytemuck::{Pod, Zeroable};

pub const LIQUID_GLASS_WGSL_SOURCE: &str = include_str!("../shaders/liquid_glass.wgsl");
pub const LIQUID_GLASS_GLES_SOURCE: &str = include_str!("../shaders/liquid_glass.frag");
pub const DUAL_KAWASE_DOWN_GLES_SOURCE: &str = include_str!("../shaders/dual_kawase_down.frag");
pub const DUAL_KAWASE_UP_GLES_SOURCE: &str = include_str!("../shaders/dual_kawase_up.frag");

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct LiquidGlassUniforms {
    pub panel_center_and_half_size: [f32; 4],
    pub viewport_and_ior: [f32; 4],
    pub bevel_and_specular: [f32; 4],
    pub tint_rgba: [f32; 4],
}

impl LiquidGlassUniforms {
    pub fn from_material_and_rect(
        material: &LiquidGlassMaterial,
        panel_rect: Rect,
        viewport_size: (f32, f32),
        elevation_px: f32,
    ) -> Self {
        let half_w = panel_rect.width * 0.5;
        let half_h = panel_rect.height * 0.5;
        let center_x = panel_rect.x + half_w;
        let center_y = panel_rect.y + half_h;

        Self {
            panel_center_and_half_size: [center_x, center_y, half_w, half_h],
            viewport_and_ior: [
                viewport_size.0,
                viewport_size.1,
                material.ior,
                material.chromatic_dispersion,
            ],
            bevel_and_specular: [
                material.corner_radius,
                material.bevel_width,
                material.specular_intensity,
                elevation_px,
            ],
            tint_rgba: material.tint_rgba,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KawaseMipLevel {
    pub level: u32,
    pub width: u32,
    pub height: u32,
    pub half_pixel: [f32; 2],
}

pub fn plan_dual_kawase_pyramid(
    output_width: u32,
    output_height: u32,
    passes: u32,
) -> Vec<KawaseMipLevel> {
    let mut levels = Vec::with_capacity(passes as usize);
    let mut w = output_width.max(1);
    let mut h = output_height.max(1);

    for level in 1..=passes {
        w = (w / 2).max(1);
        h = (h / 2).max(1);
        levels.push(KawaseMipLevel {
            level,
            width: w,
            height: h,
            half_pixel: [0.5 / (w as f32), 0.5 / (h as f32)],
        });
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_struct_has_exact_64_byte_std140_layout() {
        assert_eq!(std::mem::size_of::<LiquidGlassUniforms>(), 64);
    }

    #[test]
    fn dual_kawase_pyramid_halves_dimensions_each_pass() {
        let mips = plan_dual_kawase_pyramid(1920, 1080, 3);
        assert_eq!(mips.len(), 3);
        assert_eq!((mips[0].width, mips[0].height), (960, 540));
        assert_eq!((mips[1].width, mips[1].height), (480, 270));
        assert_eq!((mips[2].width, mips[2].height), (240, 135));
    }
}
