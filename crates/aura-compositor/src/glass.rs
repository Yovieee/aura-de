//! GLES orchestration for compositor-owned glass behind top/overlay surfaces.
//!
//! The scene excludes foreground shell surfaces: otherwise a panel would sample
//! itself (or an overlay above it). Textures are retained across frames and
//! replaced atomically on output resize. All work here is GPU command encoding;
//! no readback, disk access, or waiting for external services occurs.
use aura_core::{LiquidGlassMaterial, Rect};
use aura_render::{
    LiquidGlassUniforms, RenderError, plan_dual_kawase_pyramid, smithay_glass_shader,
    smithay_kawase_down_shader, smithay_kawase_up_shader,
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Color32F, Frame, Offscreen, Renderer, Texture,
            element::surface::WaylandSurfaceRenderElement,
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTarget, GlesTexProgram, GlesTexture,
                Uniform, UniformName, UniformType, ffi,
            },
            utils::draw_render_elements,
        },
    },
    utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform},
};

#[derive(Debug, thiserror::Error)]
pub enum GlassError {
    #[error(transparent)]
    Gles(#[from] GlesError),
    #[error(transparent)]
    Parameters(#[from] RenderError),
    #[error("glass output size must be positive")]
    InvalidSize,
}

/// A compositor-authorized glass surface in physical output coordinates.
/// Shell clients retain their text/icons in the foreground; this replaces only
/// the backdrop behind them. Do not derive panels from arbitrary app buffers.
pub struct GlassPanel {
    pub rect: Rect,
    pub material: LiquidGlassMaterial,
    pub elevation_px: f32,
}

struct Buffers {
    size: Size<i32, Physical>,
    scene: GlesTexture,
    down: Vec<GlesTexture>,
    up: Vec<GlesTexture>,
}

impl Buffers {
    fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>) -> Result<Self, GlassError> {
        if size.is_empty() {
            return Err(GlassError::InvalidSize);
        }
        let allocate = |renderer: &mut GlesRenderer, w: i32, h: i32| {
            // RGBA8 is the portable Smithay offscreen format. This path follows
            // the compositor's SDR working color space; HDR management requires
            // format/transfer negotiation and is not implied by successful GLES.
            let texture =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, (w, h).into())?;
            renderer.with_context(|gl| {
                // SAFETY: Smithay made its context current; the texture belongs
                // to that context. Restore the binding changed by this block.
                unsafe {
                    let mut previous = 0;
                    gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut previous);
                    gl.BindTexture(ffi::TEXTURE_2D, texture.tex_id());
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_S,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_T,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.BindTexture(ffi::TEXTURE_2D, previous as u32);
                }
            })?;
            Ok::<_, GlesError>(texture)
        };
        let scene = allocate(renderer, size.w, size.h)?;
        let levels = plan_dual_kawase_pyramid(size.w as u32, size.h as u32, 6);
        let down = levels
            .iter()
            .map(|l| allocate(renderer, l.width as i32, l.height as i32))
            .collect::<Result<Vec<_>, _>>()?;
        let up_sizes = std::iter::once((size.w, size.h)).chain(
            levels
                .iter()
                .take(5)
                .map(|l| (l.width as i32, l.height as i32)),
        );
        let up = up_sizes
            .map(|(w, h)| allocate(renderer, w, h))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            size,
            scene,
            down,
            up,
        })
    }
}

pub struct GlassRenderer {
    glass: GlesTexProgram,
    down_program: GlesTexProgram,
    up_program: GlesTexProgram,
    buffers: Buffers,
}

impl GlassRenderer {
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>) -> Result<Self, GlassError> {
        let glass = renderer.compile_custom_texture_shader(
            smithay_glass_shader(),
            &[
                UniformName::new("u_panel_center_and_half_size", UniformType::_4f),
                UniformName::new("u_viewport_and_ior", UniformType::_4f),
                UniformName::new("u_bevel_and_specular", UniformType::_4f),
                UniformName::new("u_tint_rgba", UniformType::_4f),
            ],
        )?;
        let blur_uniforms = [
            UniformName::new("u_half_pixel", UniformType::_2f),
            UniformName::new("u_offset", UniformType::_1f),
        ];
        let down_program =
            renderer.compile_custom_texture_shader(smithay_kawase_down_shader(), &blur_uniforms)?;
        let up_program =
            renderer.compile_custom_texture_shader(smithay_kawase_up_shader(), &blur_uniforms)?;
        let buffers = Buffers::new(renderer, size)?;
        Ok(Self {
            glass,
            down_program,
            up_program,
            buffers,
        })
    }

    /// Reuses the current textures unless physical output dimensions changed.
    pub fn resize(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
    ) -> Result<(), GlassError> {
        if size != self.buffers.size {
            self.buffers = Buffers::new(renderer, size)?;
        }
        Ok(())
    }

    /// Capture the real client scene, blur it, then present glass and shell UI.
    /// Both element slices use Smithay front-to-back ordering. `scene` includes
    /// normal windows and bottom/background layers, `foreground` only top and
    /// overlay layers. The caller can recover from any error by clearing and
    /// drawing its ordinary scene to `target` again.
    ///
    /// This first implementation redraws the full scene for each scheduled
    /// frame. It reuses allocations but does not claim cached damage/idle reuse.
    #[allow(clippy::too_many_arguments)]
    pub fn render_scene(
        &mut self,
        renderer: &mut GlesRenderer,
        target: &mut GlesTarget<'_>,
        size: Size<i32, Physical>,
        scale: f64,
        scene: &[WaylandSurfaceRenderElement<GlesRenderer>],
        foreground: &[WaylandSurfaceRenderElement<GlesRenderer>],
        panels: &[GlassPanel],
        blur_material: &LiquidGlassMaterial,
        clear: Color32F,
    ) -> Result<(), GlassError> {
        self.resize(renderer, size)?;
        // Validate every panel first, so an invalid configuration cannot leave
        // half a scene rendered. Uploads use precisely the shared uniform ABI.
        let uniforms = panels
            .iter()
            .map(|p| {
                LiquidGlassUniforms::try_from_material_and_rect(
                    &p.material,
                    p.rect,
                    (size.w as f32, size.h as f32),
                    p.elevation_px,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        blur_material
            .validate(size.w.min(size.h) as f32)
            .map_err(RenderError::from)?;
        let damage = Rectangle::from_size(size);
        {
            let mut capture_target = renderer.bind(&mut self.buffers.scene)?;
            // Normal stores top-left at UV (0,0) in an offscreen GL texture.
            let mut frame = renderer.render(&mut capture_target, size, Transform::Normal)?;
            frame.clear(clear, &[damage])?;
            draw_render_elements(&mut frame, scale, scene, &[damage])?;
            let _sync = frame.finish()?;
        }
        let passes = blur_material.kawase_blur_passes as usize;
        let mut source = self.buffers.scene.clone();
        for destination in self.buffers.down.iter_mut().take(passes) {
            blur(
                renderer,
                &source,
                destination,
                &self.down_program,
                blur_material.kawase_offset,
            )?;
            source = destination.clone();
        }
        for destination in self.buffers.up.iter_mut().take(passes).rev() {
            blur(
                renderer,
                &source,
                destination,
                &self.up_program,
                blur_material.kawase_offset,
            )?;
            source = destination.clone();
        }
        // winit presentation uses Flipped180, matching its surface orientation.
        let mut frame = renderer.render(target, size, Transform::Flipped180)?;
        frame.clear(clear, &[damage])?;
        texture(&mut frame, &self.buffers.scene, size, None, &[])?;
        for uniform in uniforms.into_iter().rev() {
            let values = [
                Uniform::new(
                    "u_panel_center_and_half_size",
                    uniform.panel_center_and_half_size,
                ),
                Uniform::new("u_viewport_and_ior", uniform.viewport_and_ior),
                Uniform::new("u_bevel_and_specular", uniform.bevel_and_specular),
                Uniform::new("u_tint_rgba", uniform.tint_rgba),
            ];
            // Draw the full-output backdrop so texture coordinates remain
            // output-relative. The shader clips to the panel and its shadow.
            texture(&mut frame, &source, size, Some(&self.glass), &values)?;
        }
        draw_render_elements(&mut frame, scale, foreground, &[damage])?;
        let _sync = frame.finish()?;
        Ok(())
    }
}

fn blur(
    renderer: &mut GlesRenderer,
    source: &GlesTexture,
    destination: &mut GlesTexture,
    program: &GlesTexProgram,
    offset: f32,
) -> Result<(), GlesError> {
    let src_size = source.size();
    let size: Size<i32, Physical> =
        (destination.width() as i32, destination.height() as i32).into();
    let values = [
        Uniform::new(
            "u_half_pixel",
            [0.5 / src_size.w as f32, 0.5 / src_size.h as f32],
        ),
        Uniform::new("u_offset", offset),
    ];
    let mut target = renderer.bind(destination)?;
    let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
    frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])?;
    texture(&mut frame, source, size, Some(program), &values)?;
    let _sync = frame.finish()?;
    Ok(())
}

fn texture(
    frame: &mut GlesFrame<'_, '_>,
    source: &GlesTexture,
    size: Size<i32, Physical>,
    program: Option<&GlesTexProgram>,
    values: &[Uniform<'_>],
) -> Result<(), GlesError> {
    let src: Rectangle<f64, BufferCoord> = Rectangle::from_size(source.size()).to_f64();
    let dst = Rectangle::from_size(size);
    frame.render_texture_from_to(
        source,
        src,
        dst,
        &[dst],
        &[],
        Transform::Normal,
        1.0,
        program,
        values,
    )
}
