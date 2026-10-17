// Aura DE — Liquid Glass Refractive & Chromatic Dispersion Shader (WGSL / wgpu)

struct LiquidGlassUniforms {
    panel_center_and_half_size: vec4<f32>,
    viewport_and_ior: vec4<f32>,
    bevel_and_specular: vec4<f32>,
    tint_rgba: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u_glass: LiquidGlassUniforms;
@group(0) @binding(1) var t_backdrop_blur: texture_2d<f32>;
@group(0) @binding(2) var s_linear: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) screen_px: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0)
    );
    let p = pos[vertex_index];
    var out: VertexOutput;
    out.clip_position = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    out.screen_px = out.uv * u_glass.viewport_and_ior.xy;
    return out;
}

fn squircle_sdf(p: vec2<f32>, half_extents: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_extents.x, half_extents.y));
    let q = abs(p) - (half_extents - vec2<f32>(r));
    let q_max = max(q, vec2<f32>(0.0));
    let l4 = pow(pow(q_max.x, 4.0) + pow(q_max.y, 4.0), 0.25);
    return l4 + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let center = u_glass.panel_center_and_half_size.xy;
    let half_extents = u_glass.panel_center_and_half_size.zw;
    let radius = u_glass.bevel_and_specular.x;
    let bevel_width = max(u_glass.bevel_and_specular.y, 1.0);
    let ior = u_glass.viewport_and_ior.z;
    let dispersion = u_glass.viewport_and_ior.w;

    let local_p = in.screen_px - center;
    let dist = squircle_sdf(local_p, half_extents, radius);

    let edge_alpha = 1.0 - smoothstep(-0.75, 0.75, dist);
    if (edge_alpha <= 0.001) {
        let shadow_alpha = 0.28 * exp(-max(dist, 0.0) / max(u_glass.bevel_and_specular.w, 8.0));
        return vec4<f32>(0.0, 0.0, 0.0, shadow_alpha);
    }

    let bevel_t = clamp((dist + bevel_width) / bevel_width, 0.0, 1.0);
    let theta = bevel_t * 1.287;

    let eps = 0.5;
    let dx = squircle_sdf(local_p + vec2<f32>(eps, 0.0), half_extents, radius)
           - squircle_sdf(local_p - vec2<f32>(eps, 0.0), half_extents, radius);
    let dy = squircle_sdf(local_p + vec2<f32>(0.0, eps), half_extents, radius)
           - squircle_sdf(local_p - vec2<f32>(0.0, eps), half_extents, radius);
    let grad = normalize(vec2<f32>(dx, dy) + vec2<f32>(1e-5, 1e-5));
    let normal = normalize(vec3<f32>(grad * sin(theta), cos(theta)));

    let incident = vec3<f32>(0.0, 0.0, -1.0);
    let inv_vp = 1.0 / max(u_glass.viewport_and_ior.xy, vec2<f32>(1.0));
    let ray_r = refract(incident, normal, 1.0 / (ior - dispersion));
    let ray_g = refract(incident, normal, 1.0 / ior);
    let ray_b = refract(incident, normal, 1.0 / (ior + dispersion));

    let uv_r = clamp(in.uv + ray_r.xy * bevel_width * inv_vp, vec2<f32>(0.001), vec2<f32>(0.999));
    let uv_g = clamp(in.uv + ray_g.xy * bevel_width * inv_vp, vec2<f32>(0.001), vec2<f32>(0.999));
    let uv_b = clamp(in.uv + ray_b.xy * bevel_width * inv_vp, vec2<f32>(0.001), vec2<f32>(0.999));

    let backdrop_rgb = vec3<f32>(
        textureSample(t_backdrop_blur, s_linear, uv_r).r,
        textureSample(t_backdrop_blur, s_linear, uv_g).g,
        textureSample(t_backdrop_blur, s_linear, uv_b).b
    );

    let light_dir = normalize(vec3<f32>(-0.55, -0.65, 0.52));
    let view_dir = vec3<f32>(0.0, 0.0, 1.0);
    let half_vec = normalize(light_dir + view_dir);
    let fresnel = pow(1.0 - max(dot(normal, view_dir), 0.0), 3.0);
    let spec_lobe = pow(max(dot(normal, half_vec), 0.0), 28.0);
    let rim_highlight = (fresnel * 0.55 + spec_lobe * 0.85) * u_glass.bevel_and_specular.z;

    let tinted = mix(backdrop_rgb, u_glass.tint_rgba.rgb, u_glass.tint_rgba.a);
    let final_rgb = clamp(tinted + vec3<f32>(rim_highlight), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(final_rgb, edge_alpha);
}
