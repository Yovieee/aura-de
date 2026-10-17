#version 300 es
precision highp float;
in vec2 v_tex_coords;
out vec4 frag_color;

uniform sampler2D u_source;
uniform vec2 u_half_pixel;
uniform float u_offset;

void main() {
    vec2 d = u_half_pixel * u_offset;
    vec4 sum = texture(u_source, v_tex_coords) * 4.0;
    sum += texture(u_source, v_tex_coords - d);
    sum += texture(u_source, v_tex_coords + d);
    sum += texture(u_source, v_tex_coords + vec2(d.x, -d.y));
    sum += texture(u_source, v_tex_coords - vec2(d.x, -d.y));
    frag_color = sum * 0.125;
}
