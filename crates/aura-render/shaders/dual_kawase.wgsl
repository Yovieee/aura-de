// All filtering takes place in linear light. Source textures use clamp sampling.
struct BlurUniforms {
    half_pixel: vec2<f32>,
    offset: f32,
    padding: f32,
};
@group(0) @binding(0) var<uniform> u_blur: BlurUniforms;
@group(0) @binding(1) var t_source: texture_2d<f32>;
@group(0) @binding(2) var s_linear: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    let positions = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    let p = positions[index];
    var out: VertexOutput;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}
@fragment fn fs_down(in: VertexOutput) -> @location(0) vec4<f32> {
    let d = u_blur.half_pixel * u_blur.offset;
    var sum = textureSample(t_source, s_linear, in.uv) * 4.0;
    sum += textureSample(t_source, s_linear, in.uv - d);
    sum += textureSample(t_source, s_linear, in.uv + d);
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(d.x, -d.y));
    sum += textureSample(t_source, s_linear, in.uv - vec2<f32>(d.x, -d.y));
    return sum * 0.125;
}
@fragment fn fs_up(in: VertexOutput) -> @location(0) vec4<f32> {
    let d = u_blur.half_pixel * u_blur.offset;
    var sum = textureSample(t_source, s_linear, in.uv + vec2<f32>(-d.x * 2.0, 0.0));
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(-d.x, d.y)) * 2.0;
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(0.0, d.y * 2.0));
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(d.x, d.y)) * 2.0;
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(d.x * 2.0, 0.0));
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(d.x, -d.y)) * 2.0;
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(0.0, -d.y * 2.0));
    sum += textureSample(t_source, s_linear, in.uv + vec2<f32>(-d.x, -d.y)) * 2.0;
    return sum / 12.0;
}
