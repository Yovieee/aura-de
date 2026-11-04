//! Coordinates the Dual-Kawase blur pyramid and Liquid Glass uniform blocks per output frame.

use aura_core::{LiquidGlassMaterial, Rect};
use aura_render::{plan_dual_kawase_pyramid, KawaseMipLevel, LiquidGlassUniforms};
use crate::state::AuraCompositorState;

#[derive(Debug, Clone)]
pub struct FrameRenderPlan {
    pub frame_number: u64,
    pub kawase_mips: Vec<KawaseMipLevel>,
    pub surface_uniforms: Vec<(u64, LiquidGlassUniforms)>,
    pub menu_bar_uniforms: LiquidGlassUniforms,
}

pub fn build_frame_render_plan(state: &AuraCompositorState) -> FrameRenderPlan {
    let out = state.workspace.output_rect;
    let passes = state.glass_material.kawase_blur_passes;
    let kawase_mips = plan_dual_kawase_pyramid(out.width as u32, out.height as u32, passes);

    let viewport = (out.width, out.height);
    let surface_uniforms = state
        .blur_regions
        .iter()
        .map(|region| {
            let uniforms = LiquidGlassUniforms::from_material_and_rect(
                &region.material,
                region.rect,
                viewport,
                region.elevation_px,
            );
            (region.window_id, uniforms)
        })
        .collect();

    let menu_bar_material = LiquidGlassMaterial {
        corner_radius: 0.0,
        bevel_width: 4.0,
        ..state.glass_material
    };
    let menu_bar_rect = Rect::new(0.0, 0.0, out.width, state.workspace.menu_bar_height);
    let menu_bar_uniforms = LiquidGlassUniforms::from_material_and_rect(
        &menu_bar_material,
        menu_bar_rect,
        viewport,
        12.0,
    );

    FrameRenderPlan {
        frame_number: state.frame_counter,
        kawase_mips,
        surface_uniforms,
        menu_bar_uniforms,
    }
}
