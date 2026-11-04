# Aura rendering

`LiquidGlassPipeline` executes Dual-Kawase downsample and upsample passes followed
by the refractive glass pass. It owns reusable per-output intermediate textures;
it does not create a window, capture arbitrary Wayland clients, or present frames.
The compositor supplies the correctly ordered backdrop below each panel.

```rust,ignore
let mut glass = LiquidGlassPipeline::new(&device, output_size, output_format)?;
// On output resize, outside an active render pass:
glass.resize(&device, output_size)?;
glass.encode(&device, &mut encoder, &backdrop_view, &output_view, PanelRender {
    rect: panel_in_physical_pixels,
    material: &material,
    elevation_px: 12.0,
})?;
queue.submit([encoder.finish()]);
```

The destination must match the configured dimensions/format and already contain
the unblurred frame (or be cleared when rendering a separate overlay). Do not use
the same texture as backdrop and destination in a pass. Backdrops use top-left
pixel coordinates and cover the entire output. Supply sRGB texture views for
sRGB-encoded sources and an sRGB destination for display output; intermediates
are linear `Rgba16Float`. Glass and shadow output is premultiplied. Existing
background pixels remain visible outside the panel.

The caller schedules damaged frames and retains the captured backdrop. A panel
move must invalidate its old and new bounds expanded for blur/shadow. Capture the
backdrop at the panel's stacking position, rather than including windows above
it. Ordinary shell clients do not have access to other clients' buffers; their
opaque fallback can be drawn locally, but cross-window glass belongs in the
compositor.

`try_from_material_and_rect` returns errors for invalid material, geometry,
viewport, and elevation before GPU commands are recorded. The legacy
`from_material_and_rect` assumes constant valid data and panics on invalid input;
runtime configuration should always use the fallible constructor.

For Smithay 0.7 GLES integration, `smithay_glass_shader()` and
`smithay_kawase_down_shader()` / `smithay_kawase_up_shader()` adapt the canonical
GLES shaders to `compile_custom_texture_shader`. They implement the sampler,
opacity, and EXTERNAL/NO_ALPHA/DEBUG define ABI. Register glass uniform names
`u_panel_center_and_half_size`, `u_viewport_and_ior`, `u_bevel_and_specular`, and
`u_tint_rgba` as four-component float uniforms. Blur programs use `u_half_pixel`
(two floats calculated from the **source** size) and `u_offset` (one float).
Pass a full-output blurred texture with identity texture coordinates when
rendering glass. GLES framebuffer color-space setup remains the compositor's
responsibility.

CPU/shader validation:

```sh
cargo test -p aura-render --locked
```

Driver tests are explicit so math-only development does not require a GPU.
They fail if no suitable driver is available; no successful result is fabricated.
Mesa llvmpipe/lavapipe can exercise them without a physical GPU:

```sh
cargo test -p aura-render --locked --test gpu -- --ignored --nocapture
cargo test -p aura-render --locked --test gles -- --ignored --nocapture
```

The wgpu test reads back rendered pixels and checks actual blur, zero-pass sharp
output, clipping/premultiplied alpha, opaque fallback and output resize. The GLES
test compiles and links the canonical GLES3 programs and Smithay ES100 variants
in a surfaceless EGL context. These tests establish software driver execution,
not native DRM/KMS support or a hardware frame-time guarantee.
