#version 300 es
precision highp float;

in vec2 v_tex_coords;
out vec4 frag_color;

uniform sampler2D u_backdrop_blur;
uniform vec4 u_panel_center_and_half_size;
uniform vec4 u_viewport_and_ior;
uniform vec4 u_bevel_and_specular;
uniform vec4 u_tint_rgba;

float squircle_sdf(vec2 p, vec2 half_extents, float radius) {
    float r = min(radius, min(half_extents.x, half_extents.y));
    vec2 q = abs(p) - (half_extents - vec2(r));
    vec2 q_max = max(q, vec2(0.0));
    float l4 = pow(pow(q_max.x, 4.0) + pow(q_max.y, 4.0), 0.25);
    return l4 + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 screen_px = v_tex_coords * u_viewport_and_ior.xy;
    vec2 center = u_panel_center_and_half_size.xy;
    vec2 half_extents = u_panel_center_and_half_size.zw;
    float radius = u_bevel_and_specular.x;
    float bevel_width = max(u_bevel_and_specular.y, 1.0);
    float ior = u_viewport_and_ior.z;
    float dispersion = u_viewport_and_ior.w;

    vec2 local_p = screen_px - center;
    float dist = squircle_sdf(local_p, half_extents, radius);
    float edge_alpha = 1.0 - smoothstep(-0.75, 0.75, dist);

    if (edge_alpha <= 0.001) {
        float shadow = 0.28 * exp(-max(dist, 0.0) / max(u_bevel_and_specular.w, 8.0));
        frag_color = vec4(0.0, 0.0, 0.0, shadow);
        return;
    }

    float bevel_t = clamp((dist + bevel_width) / bevel_width, 0.0, 1.0);
    float theta = bevel_t * 1.287;
    float eps = 0.5;
    float dx = squircle_sdf(local_p + vec2(eps, 0.0), half_extents, radius)
             - squircle_sdf(local_p - vec2(eps, 0.0), half_extents, radius);
    float dy = squircle_sdf(local_p + vec2(0.0, eps), half_extents, radius)
             - squircle_sdf(local_p - vec2(0.0, eps), half_extents, radius);
    vec2 grad = normalize(vec2(dx, dy) + vec2(1e-5));
    vec3 normal = normalize(vec3(grad * sin(theta), cos(theta)));

    vec3 incident = vec3(0.0, 0.0, -1.0);
    vec2 inv_vp = 1.0 / max(u_viewport_and_ior.xy, vec2(1.0));
    vec3 ray_r = refract(incident, normal, 1.0 / (ior - dispersion));
    vec3 ray_g = refract(incident, normal, 1.0 / ior);
    vec3 ray_b = refract(incident, normal, 1.0 / (ior + dispersion));

    vec3 backdrop_rgb = vec3(
        texture(u_backdrop_blur, clamp(v_tex_coords + ray_r.xy * bevel_width * inv_vp, 0.001, 0.999)).r,
        texture(u_backdrop_blur, clamp(v_tex_coords + ray_g.xy * bevel_width * inv_vp, 0.001, 0.999)).g,
        texture(u_backdrop_blur, clamp(v_tex_coords + ray_b.xy * bevel_width * inv_vp, 0.001, 0.999)).b
    );

    vec3 light_dir = normalize(vec3(-0.55, -0.65, 0.52));
    vec3 view_dir = vec3(0.0, 0.0, 1.0);
    vec3 half_vec = normalize(light_dir + view_dir);
    float fresnel = pow(1.0 - max(dot(normal, view_dir), 0.0), 3.0);
    float spec_lobe = pow(max(dot(normal, half_vec), 0.0), 28.0);
    float rim = (fresnel * 0.55 + spec_lobe * 0.85) * u_bevel_and_specular.z;

    vec3 tinted = mix(backdrop_rgb, u_tint_rgba.rgb, u_tint_rgba.a);
    frag_color = vec4(clamp(tinted + vec3(rim), 0.0, 1.0), edge_alpha);
}
