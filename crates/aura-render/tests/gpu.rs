//! Explicit GPU contract test, runnable with software Vulkan (Mesa lavapipe).
//! `cargo test -p aura-render --test gpu -- --ignored --nocapture`
use aura_core::{GlassQualityPreset, LiquidGlassMaterial, Rect};
use aura_render::{LiquidGlassPipeline, PanelRender};

#[test]
#[ignore = "requires a Vulkan/GL adapter; run explicitly in the GPU smoke job"]
fn renders_blur_clipping_fallback_and_resize() {
    futures_lite::future::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::from_env_or_default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("GPU smoke requires a Vulkan/GL adapter");
        eprintln!("GPU smoke adapter: {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut pipeline =
            LiquidGlassPipeline::new(&device, (128, 128), wgpu::TextureFormat::Rgba8Unorm).unwrap();
        let panel = Rect::new(32.0, 32.0, 64.0, 64.0);
        let mut glass = LiquidGlassMaterial::macos_dock_default();
        glass.tint_rgba = [0.0; 4];
        glass.specular_intensity = 0.0;
        let blurred = render(&device, &queue, &pipeline, &glass, panel, true);
        let center = pixel(&blurred, 128, 64, 64);
        assert!(
            (85..=170).contains(&center[0]),
            "blur should average checkerboard, got {center:?}"
        );
        assert_eq!(center[3], 255, "panel interior must be opaque replacement");
        let corner = pixel(&blurred, 128, 0, 0);
        assert!(
            corner[3] < 8,
            "outside panel should only contain faint shadow, got {corner:?}"
        );
        assert_eq!(&corner[..3], &[0, 0, 0]);
        // Every color component must be premultiplied in transparent edge pixels.
        assert!(
            blurred
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3])
        );
        glass.kawase_blur_passes = 0;
        let sharp = render(&device, &queue, &pipeline, &glass, panel, true);
        let sharp_center = pixel(&sharp, 128, 64, 64);
        assert!(
            sharp_center[0] < 20 || sharp_center[0] > 235,
            "zero-pass should preserve checkerboard, got {sharp_center:?}"
        );
        assert!(center[0].abs_diff(sharp_center[0]) > 50);
        let mut reduced = LiquidGlassMaterial::from_preset(GlassQualityPreset::ReducedTransparency);
        reduced.specular_intensity = 0.0;
        let black = render(&device, &queue, &pipeline, &reduced, panel, false);
        let checker = render(&device, &queue, &pipeline, &reduced, panel, true);
        assert_eq!(
            pixel(&black, 128, 64, 64),
            pixel(&checker, 128, 64, 64),
            "opaque fallback must not reveal backdrop"
        );
        assert_eq!(pixel(&black, 128, 64, 64)[3], 255);
        pipeline.resize(&device, (96, 80)).unwrap();
        let resized = render(
            &device,
            &queue,
            &pipeline,
            &reduced,
            Rect::new(16.0, 16.0, 64.0, 48.0),
            true,
        );
        assert_eq!(resized.len(), 96 * 80 * 4);
        assert_eq!(pixel(&resized, 96, 48, 40)[3], 255);
        assert!(pipeline.resize(&device, (0, 80)).is_err());
        assert!(
            device.pop_error_scope().await.is_none(),
            "GPU validation error"
        );
    });
}

fn pixel(bytes: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    bytes[(y * width + x) * 4..(y * width + x) * 4 + 4]
        .try_into()
        .unwrap()
}

fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &LiquidGlassPipeline,
    material: &LiquidGlassMaterial,
    rect: Rect,
    checker: bool,
) -> Vec<u8> {
    let (width, height) = pipeline.size();
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let source = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("smoke checkerboard"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let value = if checker && (x / 2 + y / 2) % 2 == 0 {
                255
            } else {
                0
            };
            data.extend([value, value, value, 255]);
        }
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        size,
    );
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("smoke output"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let source_view = source.create_view(&Default::default());
    let target_view = target.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    pipeline
        .encode(
            device,
            &mut encoder,
            &source_view,
            &target_view,
            PanelRender {
                rect,
                material,
                elevation_px: 8.0,
            },
        )
        .unwrap();
    let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("smoke readback"),
        size: u64::from(padded_row * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    queue.submit([encoder.finish()]);
    let (sender, receiver) = std::sync::mpsc::channel();
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).unwrap();
        });
    device.poll(wgpu::PollType::Wait).unwrap();
    receiver.recv().unwrap().unwrap();
    let mapped = readback.slice(..).get_mapped_range();
    let pixels = mapped
        .chunks(padded_row as usize)
        .flat_map(|row| row[..(width * 4) as usize].iter().copied())
        .collect();
    drop(mapped);
    readback.unmap();
    pixels
}
