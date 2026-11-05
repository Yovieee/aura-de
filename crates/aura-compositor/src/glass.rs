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
            element::{RenderElement, surface::WaylandSurfaceRenderElement},
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

/// GPU work encoded since the renderer was created. A blur pass is one
/// downsample or upsample draw, rather than one entire blur pyramid.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GlassStats {
    pub scene_captures: u64,
    pub blur_passes: u64,
    /// The scene currently retained in the backdrop texture, if valid.
    pub scene_generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SceneKey {
    generation: u64,
    size: (i32, i32),
    scale_bits: u64,
    clear_bits: [u32; 4],
}

impl SceneKey {
    fn new(generation: u64, size: Size<i32, Physical>, scale: f64, clear: Color32F) -> Self {
        Self {
            generation,
            size: (size.w, size.h),
            scale_bits: scale.to_bits(),
            clear_bits: clear.components().map(f32::to_bits),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlurKey {
    scene: SceneKey,
    passes: u32,
    offset_bits: u32,
}

impl BlurKey {
    fn new(scene: SceneKey, material: &LiquidGlassMaterial) -> Self {
        Self {
            scene,
            passes: material.kawase_blur_passes,
            offset_bits: material.kawase_offset.to_bits(),
        }
    }
}

/// Keys become valid only after the corresponding GPU commands finish encoding.
/// The caller invalidates both keys after any failed frame, because a failed
/// command can leave either retained texture partially overwritten.
#[derive(Debug, Default)]
struct BackdropCache {
    scene: Option<SceneKey>,
    blurred: Option<BlurKey>,
}

impl BackdropCache {
    fn capture_needed(&self, scene: SceneKey) -> bool {
        self.scene != Some(scene)
    }

    fn blur_needed(&self, blur: BlurKey) -> bool {
        self.blurred != Some(blur)
    }

    fn captured(&mut self, scene: SceneKey) {
        self.scene = Some(scene);
        self.blurred = None;
    }

    fn invalidate(&mut self) {
        self.scene = None;
        self.blurred = None;
    }
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
    cache: BackdropCache,
    stats: GlassStats,
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
            cache: BackdropCache::default(),
            stats: GlassStats::default(),
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
            self.cache.invalidate();
        }
        Ok(())
    }

    pub fn stats(&self) -> GlassStats {
        GlassStats {
            scene_generation: self.cache.scene.map(|scene| scene.generation),
            ..self.stats
        }
    }

    /// Capture the real client scene, blur it, then present glass and shell UI.
    /// Both element slices use Smithay front-to-back ordering. `scene` includes
    /// normal windows, compositor chrome and bottom/background layers;
    /// `foreground` includes only top and overlay layers. Use an aggregate
    /// render-element enum to interleave client surfaces and chrome. The caller
    /// can recover from any error by clearing and
    /// drawing its ordinary scene to `target` again.
    ///
    /// Advance `scene_generation` whenever a scene surface commits, its stacking
    /// or placement changes, or an animation changes the scene. Foreground and
    /// panel-only changes can reuse the generation: their final composition
    /// does not require recapturing or blurring the scene. Output size, scale,
    /// clear color and blur settings are checked independently.
    #[allow(clippy::too_many_arguments)]
    pub fn render_scene<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        target: &mut GlesTarget<'_>,
        size: Size<i32, Physical>,
        scale: f64,
        scene_generation: u64,
        scene: &[E],
        foreground: &[WaylandSurfaceRenderElement<GlesRenderer>],
        panels: &[GlassPanel],
        blur_material: &LiquidGlassMaterial,
        clear: Color32F,
    ) -> Result<(), GlassError> {
        let result = self.render_scene_inner(
            renderer,
            target,
            size,
            scale,
            scene_generation,
            scene,
            foreground,
            panels,
            blur_material,
            clear,
        );
        if result.is_err() {
            self.cache.invalidate();
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn render_scene_inner<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        target: &mut GlesTarget<'_>,
        size: Size<i32, Physical>,
        scale: f64,
        scene_generation: u64,
        scene: &[E],
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
        let scene_key = SceneKey::new(scene_generation, size, scale, clear);
        if self.cache.capture_needed(scene_key) {
            let mut capture_target = renderer.bind(&mut self.buffers.scene)?;
            // Normal stores top-left at UV (0,0) in an offscreen GL texture.
            let mut frame = renderer.render(&mut capture_target, size, Transform::Normal)?;
            frame.clear(clear, &[damage])?;
            draw_render_elements(&mut frame, scale, scene, &[damage])?;
            let _sync = frame.finish()?;
            self.stats.scene_captures = self.stats.scene_captures.saturating_add(1);
            self.cache.captured(scene_key);
        }
        let passes = blur_material.kawase_blur_passes as usize;
        let blur_key = BlurKey::new(scene_key, blur_material);
        if !panels.is_empty() && self.cache.blur_needed(blur_key) {
            let mut source = self.buffers.scene.clone();
            for destination in self.buffers.down.iter_mut().take(passes) {
                blur(
                    renderer,
                    &source,
                    destination,
                    &self.down_program,
                    blur_material.kawase_offset,
                )?;
                self.stats.blur_passes = self.stats.blur_passes.saturating_add(1);
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
                self.stats.blur_passes = self.stats.blur_passes.saturating_add(1);
                source = destination.clone();
            }
            self.cache.blurred = Some(blur_key);
        }
        let source = if passes > 0 && !panels.is_empty() {
            &self.buffers.up[0]
        } else {
            &self.buffers.scene
        };
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
            texture(&mut frame, source, size, Some(&self.glass), &values)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(generation: u64) -> SceneKey {
        SceneKey::new(generation, (1920, 1080).into(), 1.0, Color32F::BLACK)
    }

    #[test]
    fn retained_scene_and_blur_skip_unchanged_backdrops() {
        let scene = scene(7);
        let blur = BlurKey::new(scene, &LiquidGlassMaterial::macos_dock_default());
        let mut cache = BackdropCache::default();
        assert!(cache.capture_needed(scene));
        assert!(cache.blur_needed(blur));

        cache.captured(scene);
        assert!(!cache.capture_needed(scene));
        assert!(cache.blur_needed(blur));
        cache.blurred = Some(blur);
        assert!(!cache.capture_needed(scene));
        assert!(!cache.blur_needed(blur));

        let next = super::SceneKey {
            generation: 8,
            ..scene
        };
        assert!(cache.capture_needed(next));
        cache.captured(next);
        assert!(!cache.capture_needed(next));
        assert!(cache.blur_needed(BlurKey {
            scene: next,
            ..blur
        }));
    }

    #[test]
    fn output_dimensions_scale_and_every_clear_channel_invalidate_capture() {
        let original = scene(7);
        let mut cache = BackdropCache::default();
        cache.captured(original);
        let mut changed = vec![
            SceneKey::new(7, (1919, 1080).into(), 1.0, Color32F::BLACK),
            SceneKey::new(7, (1920, 1079).into(), 1.0, Color32F::BLACK),
            SceneKey::new(7, (1920, 1080).into(), 1.25, Color32F::BLACK),
        ];
        for component in 0..4 {
            let mut clear = Color32F::BLACK.components();
            clear[component] = 0.5;
            changed.push(SceneKey::new(7, (1920, 1080).into(), 1.0, clear.into()));
        }
        for key in changed {
            assert!(cache.capture_needed(key), "stale scene reused for {key:?}");
        }
        assert!(!cache.capture_needed(original));
    }

    #[test]
    fn blur_changes_reuse_capture_and_panel_optics_reuse_blur() {
        let scene = scene(7);
        let material = LiquidGlassMaterial::macos_dock_default();
        let blur = BlurKey::new(scene, &material);
        let mut cache = BackdropCache::default();
        cache.captured(scene);
        cache.blurred = Some(blur);

        let panel_only = LiquidGlassMaterial {
            ior: 1.33,
            chromatic_dispersion: 0.01,
            corner_radius: 20.0,
            bevel_width: 10.0,
            specular_intensity: 0.5,
            tint_rgba: [0.1, 0.2, 0.3, 0.6],
            ..material
        };
        assert!(!cache.blur_needed(BlurKey::new(scene, &panel_only)));

        for changed in [
            LiquidGlassMaterial {
                kawase_blur_passes: 0,
                ..material
            },
            LiquidGlassMaterial {
                kawase_blur_passes: 6,
                ..material
            },
            LiquidGlassMaterial {
                kawase_offset: 2.0,
                ..material
            },
        ] {
            assert!(cache.blur_needed(BlurKey::new(scene, &changed)));
            assert!(!cache.capture_needed(scene));
        }
    }

    #[test]
    fn invalidation_discards_both_partially_written_textures() {
        let scene = scene(7);
        let blur = BlurKey::new(scene, &LiquidGlassMaterial::macos_dock_default());
        let mut cache = BackdropCache::default();
        cache.captured(scene);
        cache.blurred = Some(blur);

        cache.invalidate();
        assert_eq!(cache.scene, None);
        assert!(cache.capture_needed(scene));
        assert!(cache.blur_needed(blur));
    }
}
