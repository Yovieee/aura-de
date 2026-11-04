//! `aura-render`: GPU uniform buffers, Dual-Kawase pyramid planner, and embedded shaders.

use aura_core::{LiquidGlassMaterial, Rect};
use bytemuck::{Pod, Zeroable};

mod pipeline;
pub use pipeline::{LiquidGlassPipeline, PanelRender};

pub const LIQUID_GLASS_WGSL_SOURCE: &str = include_str!("../shaders/liquid_glass.wgsl");
pub const LIQUID_GLASS_GLES_SOURCE: &str = include_str!("../shaders/liquid_glass.frag");
pub const DUAL_KAWASE_DOWN_GLES_SOURCE: &str = include_str!("../shaders/dual_kawase_down.frag");
pub const DUAL_KAWASE_UP_GLES_SOURCE: &str = include_str!("../shaders/dual_kawase_up.frag");
pub const DUAL_KAWASE_WGSL_SOURCE: &str = include_str!("../shaders/dual_kawase.wgsl");

/// Adapt the canonical GLES shader to Smithay 0.7's custom texture shader ABI.
/// Pass the returned source to `GlesRenderer::compile_custom_texture_shader`.
/// The input texture must contain the compositor's already blurred, full-output
/// backdrop; the four additional uniforms have the same names/layout as the
/// canonical shader. Draw the output-sized texture with the identity transform
/// so `v_coords` and the physical-pixel panel rectangle share an origin.
pub fn smithay_glass_shader() -> String {
    smithay_texture_shader(LIQUID_GLASS_GLES_SOURCE, "u_backdrop_blur")
}

/// GLES downsample shader using Smithay's sampler/alpha ABI. `u_half_pixel`
/// describes the *source* texture and `u_offset` is the validated material offset.
pub fn smithay_kawase_down_shader() -> String {
    smithay_texture_shader(DUAL_KAWASE_DOWN_GLES_SOURCE, "u_source")
}

pub fn smithay_kawase_up_shader() -> String {
    smithay_texture_shader(DUAL_KAWASE_UP_GLES_SOURCE, "u_source")
}

fn smithay_texture_shader(source: &str, sampler_name: &str) -> String {
    let source = source
        .replace("#version 300 es", "#version 100\n//_DEFINES_\n#if defined(EXTERNAL)\n#extension GL_OES_EGL_image_external : require\n#endif")
        .replace("in vec2 v_tex_coords;", "varying vec2 v_coords;\nuniform float alpha;")
        .replace("out vec4 frag_color;", "")
        .replace(&format!("uniform sampler2D {sampler_name};"), "#if defined(EXTERNAL)\nuniform samplerExternalOES tex;\n#else\nuniform sampler2D tex;\n#endif")
        .replace(sampler_name, "tex")
        .replace("v_tex_coords", "v_coords")
        .replace("frag_color", "gl_FragColor")
        .replace("texture(", "texture2D(");
    // Canonical programs output premultiplied alpha; apply Smithay's surface
    // opacity to all components. NO_ALPHA refers to the input backdrop, not the
    // generated glass/shadow alpha, so it must not overwrite output alpha.
    let end = source
        .rfind('}')
        .expect("embedded shader has a main function");
    format!(
        "{}\n    gl_FragColor *= alpha;\n{}",
        &source[..end],
        &source[end..]
    )
}

/// Validated boundaries between application state and GPU commands.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Material(#[from] aura_core::LiquidGlassError),
    #[error("panel rectangle must have finite coordinates and positive finite dimensions")]
    InvalidPanel,
    #[error("viewport dimensions must be positive and within the device texture limit")]
    InvalidViewport,
    #[error("elevation must be finite and nonnegative")]
    InvalidElevation,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct LiquidGlassUniforms {
    pub panel_center_and_half_size: [f32; 4],
    pub viewport_and_ior: [f32; 4],
    pub bevel_and_specular: [f32; 4],
    pub tint_rgba: [f32; 4],
}

impl LiquidGlassUniforms {
    /// Pack material parameters only after validating them at the upload boundary.
    pub fn try_from_material_and_rect(
        material: &LiquidGlassMaterial,
        panel_rect: Rect,
        viewport_size: (f32, f32),
        elevation_px: f32,
    ) -> Result<Self, RenderError> {
        if ![
            panel_rect.x,
            panel_rect.y,
            panel_rect.width,
            panel_rect.height,
        ]
        .iter()
        .all(|v| v.is_finite())
            || panel_rect.width <= 0.0
            || panel_rect.height <= 0.0
            || !(panel_rect.x + panel_rect.width).is_finite()
            || !(panel_rect.y + panel_rect.height).is_finite()
        {
            return Err(RenderError::InvalidPanel);
        }
        if !viewport_size.0.is_finite()
            || !viewport_size.1.is_finite()
            || viewport_size.0 <= 0.0
            || viewport_size.1 <= 0.0
        {
            return Err(RenderError::InvalidViewport);
        }
        if !elevation_px.is_finite() || elevation_px < 0.0 {
            return Err(RenderError::InvalidElevation);
        }
        material.validate(panel_rect.width.min(panel_rect.height))?;
        Ok(Self::pack(
            material,
            panel_rect,
            viewport_size,
            elevation_px,
        ))
    }

    /// Compatibility helper for callers with statically known valid geometry.
    /// Dynamic runtime inputs should use [`Self::try_from_material_and_rect`].
    pub fn from_material_and_rect(
        material: &LiquidGlassMaterial,
        panel_rect: Rect,
        viewport_size: (f32, f32),
        elevation_px: f32,
    ) -> Self {
        Self::try_from_material_and_rect(material, panel_rect, viewport_size, elevation_px)
            .expect("invalid glass uniform parameters")
    }

    fn pack(
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
    // The material model permits at most six passes. Bound allocation even when
    // this public helper is called directly with an untrusted count.
    let passes = passes.min(6);
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
        assert_eq!(
            std::mem::offset_of!(LiquidGlassUniforms, panel_center_and_half_size),
            0
        );
        assert_eq!(
            std::mem::offset_of!(LiquidGlassUniforms, viewport_and_ior),
            16
        );
        assert_eq!(
            std::mem::offset_of!(LiquidGlassUniforms, bevel_and_specular),
            32
        );
        assert_eq!(std::mem::offset_of!(LiquidGlassUniforms, tint_rgba), 48);
    }

    #[test]
    fn dual_kawase_pyramid_halves_dimensions_each_pass() {
        let mips = plan_dual_kawase_pyramid(1920, 1080, 3);
        assert_eq!(mips.len(), 3);
        assert_eq!((mips[0].width, mips[0].height), (960, 540));
        assert_eq!((mips[1].width, mips[1].height), (480, 270));
        assert_eq!((mips[2].width, mips[2].height), (240, 135));
    }

    #[test]
    fn rejects_invalid_upload_parameters() {
        let mat = LiquidGlassMaterial::macos_dock_default();
        let rect = Rect::new(0.0, 0.0, 100.0, 40.0);
        for bad in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            assert!(
                LiquidGlassUniforms::try_from_material_and_rect(&mat, rect, (bad, 100.0), 1.0)
                    .is_err()
            );
        }
        assert!(
            LiquidGlassUniforms::try_from_material_and_rect(
                &mat,
                Rect::new(0.0, 0.0, 0.0, 40.0),
                (100.0, 100.0),
                1.0
            )
            .is_err()
        );
        assert!(
            LiquidGlassUniforms::try_from_material_and_rect(&mat, rect, (100.0, 100.0), -1.0)
                .is_err()
        );
        let invalid = LiquidGlassMaterial { ior: 0.0, ..mat };
        assert!(
            LiquidGlassUniforms::try_from_material_and_rect(&invalid, rect, (100.0, 100.0), 1.0)
                .is_err()
        );
    }

    #[test]
    fn wgsl_shaders_parse_and_validate() {
        for source in [LIQUID_GLASS_WGSL_SOURCE, DUAL_KAWASE_WGSL_SOURCE] {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|e| panic!("{}", e.emit_to_string(source)));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(source)));
        }
    }
}
