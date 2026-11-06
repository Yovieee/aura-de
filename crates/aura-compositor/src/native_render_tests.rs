//! Software-driver evidence for output composition, independent of DRM scanout.
use crate::{
    glass::{GlassPanel, GlassRenderer},
    scene::SceneElement,
};
use aura_core::{GlassQualityPreset, LiquidGlassMaterial, Rect};
use smithay::{
    backend::{
        allocator::Fourcc,
        egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
        renderer::{
            Bind, Color32F, Frame, Offscreen, Renderer,
            element::{Id, Kind, solid::SolidColorRenderElement},
            gles::{GlesRenderer, GlesTexture, ffi},
            utils::CommitCounter,
        },
    },
    utils::{Physical, Rectangle, Size, Transform},
};

fn solid(rect: Rectangle<i32, Physical>, color: [f32; 4]) -> SceneElement {
    SolidColorRenderElement::new(
        Id::new(),
        rect,
        CommitCounter::default(),
        Color32F::new(color[0], color[1], color[2], color[3]),
        Kind::Unspecified,
    )
    .into()
}

fn pixel(bytes: &[u8], x: usize, y: usize, width: usize) -> [u8; 4] {
    bytes[(y * width + x) * 4..(y * width + x + 1) * 4]
        .try_into()
        .unwrap()
}

#[test]
#[ignore = "requires a working EGL/GLES3 driver; does not test DRM hardware"]
fn composites_glass_scene_for_native_and_nested_output_orientations() {
    // SAFETY: the display has no external native handles, and the newly created
    // renderer exclusively owns its EGL context throughout this test.
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
    let context = EGLContext::new(&display).unwrap();
    let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
    let size: Size<i32, Physical> = (128, 128).into();
    let mut output = Offscreen::<GlesTexture>::create_buffer(
        &mut renderer,
        Fourcc::Abgr8888,
        (size.w, size.h).into(),
    )
    .unwrap();
    let mut glass = GlassRenderer::new(&mut renderer, size).unwrap();
    let scene = vec![
        solid(
            Rectangle::new((0, 0).into(), (128, 64).into()),
            [1., 0., 0., 1.],
        ),
        solid(
            Rectangle::new((0, 64).into(), (128, 64).into()),
            [0., 0., 1., 1.],
        ),
    ];
    // This opaque green panel is deliberately above the output's midpoint. Its
    // position checks the glass shader's output-relative coordinates as well as
    // the retained scene texture. A symmetric backdrop alone would miss a flip.
    let material = LiquidGlassMaterial {
        ior: 1.,
        chromatic_dispersion: 0.,
        corner_radius: 4.,
        bevel_width: 2.,
        kawase_blur_passes: 0,
        kawase_offset: 1.,
        specular_intensity: 0.,
        tint_rgba: [0., 1., 0., 1.],
    };
    let panels = [GlassPanel {
        rect: Rect::new(40., 8., 48., 32.),
        material,
        elevation_px: 0.,
    }];
    let blur_material = LiquidGlassMaterial::from_preset(GlassQualityPreset::ReducedTransparency);
    let mut frames = Vec::new();
    for output_transform in [Transform::Normal, Transform::Flipped180] {
        let mut target = renderer.bind(&mut output).unwrap();
        glass
            .render_scene(
                &mut renderer,
                &mut target,
                size,
                output_transform,
                1.,
                1,
                &scene,
                &[],
                &panels,
                &blur_material,
                Color32F::BLACK,
            )
            .unwrap();
        let mut bytes = vec![0u8; size.w as usize * size.h as usize * 4];
        // Rebind the output: glass rendering temporarily binds retained texture
        // framebuffers. ReadPixels is a test-only blocking verification path.
        let mut frame = renderer
            .render(&mut target, size, output_transform)
            .unwrap();
        frame
            .with_context(|gl| {
                // SAFETY: this frame binds the correctly sized RGBA8 test target;
                // the writable vector contains exactly width*height*4 bytes.
                unsafe {
                    gl.PixelStorei(ffi::PACK_ALIGNMENT, 1);
                    gl.ReadPixels(
                        0,
                        0,
                        size.w,
                        size.h,
                        ffi::RGBA,
                        ffi::UNSIGNED_BYTE,
                        bytes.as_mut_ptr().cast(),
                    );
                    assert_eq!(gl.GetError(), ffi::NO_ERROR);
                }
            })
            .unwrap();
        let _sync = frame.finish().unwrap();
        frames.push(bytes);
    }
    let [native, nested]: [Vec<u8>; 2] = frames.try_into().unwrap();
    // Normal offscreen rendering stores logical top-left at GL texture UV 0,0.
    // The nested EGL surface's Flipped180 therefore reverses the readback rows.
    for (bytes, top, bottom, panel) in [(&native, 8, 119, 24), (&nested, 119, 8, 103)] {
        let red = pixel(bytes, 8, top, 128);
        let blue = pixel(bytes, 8, bottom, 128);
        let green = pixel(bytes, 64, panel, 128);
        assert!(red[0] > 240 && red[1] < 8 && red[2] < 8, "top: {red:?}");
        assert!(
            blue[0] < 8 && blue[1] < 8 && blue[2] > 240,
            "bottom: {blue:?}"
        );
        assert!(
            green[0] < 8 && green[1] > 240 && green[2] < 8,
            "panel: {green:?}"
        );
    }
    for y in 0..128 {
        for x in 0..128 {
            let normal = pixel(&native, x, y, 128);
            let flipped = pixel(&nested, x, 127 - y, 128);
            assert!(
                normal.iter().zip(flipped).all(|(a, b)| a.abs_diff(b) <= 2),
                "composition differs at ({x}, {y}): {normal:?} vs {flipped:?}"
            );
        }
    }
    assert_eq!(
        glass.stats().scene_captures,
        1,
        "switching output orientation should reuse the unchanged scene texture"
    );
}
