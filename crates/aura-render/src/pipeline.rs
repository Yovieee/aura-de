//! Offscreen wgpu renderer. Backdrops are supplied by the compositor, never
//! sampled from other clients by an ordinary Wayland client.
use crate::{
    DUAL_KAWASE_WGSL_SOURCE, LIQUID_GLASS_WGSL_SOURCE, LiquidGlassUniforms, RenderError,
    plan_dual_kawase_pyramid,
};
use aura_core::{LiquidGlassMaterial, Rect};
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// One panel, in physical output pixels. The source backdrop must cover the
/// entire viewport, with the same top-left origin as `rect`.
pub struct PanelRender<'a> {
    pub rect: Rect,
    pub material: &'a LiquidGlassMaterial,
    pub elevation_px: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurUniforms {
    half_pixel: [f32; 2],
    offset: f32,
    _padding: f32,
}

struct Target {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
}

impl Target {
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Aura linear HDR blur target"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            _texture: texture,
            view,
            size,
        }
    }
}

/// Reusable Dual-Kawase and glass pipelines for one output.
///
/// Intermediate textures use linear `Rgba16Float`. Upload display-encoded
/// backdrops as `Rgba8UnormSrgb`, and use an sRGB destination for display output;
/// wgpu then performs the decode/encode at the texture boundaries. The output
/// uses premultiplied alpha and can be blended over the unblurred composition.
/// Recreate this object after a device loss, or call `resize` on output changes.
/// Encoding never submits, waits for the GPU, performs readback, or does file I/O.
/// The caller owns presentation and damage scheduling and must not alias the
/// backdrop and destination texture in the same call.
pub struct LiquidGlassPipeline {
    size: (u32, u32),
    glass: wgpu::RenderPipeline,
    down: wgpu::RenderPipeline,
    up: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    down_targets: Vec<Target>,
    up_targets: Vec<Target>,
}

impl LiquidGlassPipeline {
    pub fn new(
        device: &wgpu::Device,
        size: (u32, u32),
        output_format: wgpu::TextureFormat,
    ) -> Result<Self, RenderError> {
        validate_size(device, size)?;
        let glass_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Aura Liquid Glass"),
            source: wgpu::ShaderSource::Wgsl(LIQUID_GLASS_WGSL_SOURCE.into()),
        });
        let blur_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Aura Dual-Kawase"),
            source: wgpu::ShaderSource::Wgsl(DUAL_KAWASE_WGSL_SOURCE.into()),
        });
        let glass = make_pipeline(device, &glass_module, "fs_main", output_format, true);
        let down = make_pipeline(
            device,
            &blur_module,
            "fs_down",
            wgpu::TextureFormat::Rgba16Float,
            false,
        );
        let up = make_pipeline(
            device,
            &blur_module,
            "fs_up",
            wgpu::TextureFormat::Rgba16Float,
            false,
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Aura clamp linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mut pipeline = Self {
            size,
            glass,
            down,
            up,
            sampler,
            down_targets: Vec::new(),
            up_targets: Vec::new(),
        };
        pipeline.resize(device, size)?;
        Ok(pipeline)
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Reuses allocations when the output dimensions have not changed.
    pub fn resize(&mut self, device: &wgpu::Device, size: (u32, u32)) -> Result<(), RenderError> {
        validate_size(device, size)?;
        if self.size == size && !self.down_targets.is_empty() {
            return Ok(());
        }
        self.size = size;
        let levels = plan_dual_kawase_pyramid(size.0, size.1, 6);
        self.down_targets = levels
            .iter()
            .map(|m| Target::new(device, (m.width, m.height)))
            .collect();
        let up_sizes =
            std::iter::once(size).chain(levels.iter().take(5).map(|m| (m.width, m.height)));
        self.up_targets = up_sizes.map(|s| Target::new(device, s)).collect();
        Ok(())
    }

    /// Encodes a panel over the destination's current contents. A per-call
    /// immutable uniform buffer prevents later panels overwriting parameters
    /// before command submission. All invalid model data is rejected before
    /// any commands are encoded. The destination must match `size` and format.
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        backdrop: &wgpu::TextureView,
        destination: &wgpu::TextureView,
        panel: PanelRender<'_>,
    ) -> Result<(), RenderError> {
        let uniforms = LiquidGlassUniforms::try_from_material_and_rect(
            panel.material,
            panel.rect,
            (self.size.0 as f32, self.size.1 as f32),
            panel.elevation_px,
        )?;
        let passes = panel.material.kawase_blur_passes as usize;
        let mut source = backdrop;
        let mut source_size = self.size;
        for target in self.down_targets.iter().take(passes) {
            self.blur_pass(
                device,
                encoder,
                &self.down,
                source,
                source_size,
                target,
                panel.material.kawase_offset,
            );
            source = &target.view;
            source_size = target.size;
        }
        for target in self.up_targets.iter().take(passes).rev() {
            self.blur_pass(
                device,
                encoder,
                &self.up,
                source,
                source_size,
                target,
                panel.material.kawase_offset,
            );
            source = &target.view;
            source_size = target.size;
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Aura panel uniforms"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = self.bind_group(device, &self.glass, &uniform, source);
        draw(
            encoder,
            &self.glass,
            &bind_group,
            destination,
            wgpu::LoadOp::Load,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn blur_pass(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::RenderPipeline,
        source: &wgpu::TextureView,
        source_size: (u32, u32),
        target: &Target,
        offset: f32,
    ) {
        let params = BlurUniforms {
            half_pixel: [0.5 / source_size.0 as f32, 0.5 / source_size.1 as f32],
            offset,
            _padding: 0.0,
        };
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Aura blur uniforms"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = self.bind_group(device, pipeline, &uniform, source);
        draw(
            encoder,
            pipeline,
            &group,
            &target.view,
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );
    }

    fn bind_group(
        &self,
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        uniform: &wgpu::Buffer,
        source: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Aura sampled backdrop"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

fn validate_size(device: &wgpu::Device, size: (u32, u32)) -> Result<(), RenderError> {
    let limit = device.limits().max_texture_dimension_2d;
    if size.0 == 0 || size.1 == 0 || size.0 > limit || size.1 > limit {
        return Err(RenderError::InvalidViewport);
    }
    Ok(())
}

fn make_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    entry: &str,
    format: wgpu::TextureFormat,
    blend: bool,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry),
        layout: None,
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: blend.then_some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    })
}

fn draw(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Aura optical pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, group, &[]);
    pass.draw(0..3, 0..1);
}
