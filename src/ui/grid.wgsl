// Terminal cells as textured quads. The color and blending math is blade-egui's,
// so a grid quad and an egui mesh over the same atlas texels look the same.

struct Uniforms {
    screen_size: vec2<f32>,
    atlas_size: vec2<f32>,
};
var<uniform> r_uniforms: Uniforms;

//Note: scalars only, matching the Rust `Quad` layout without vec alignment.
struct Quad {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    uv_min_x: f32,
    uv_min_y: f32,
    uv_max_x: f32,
    uv_max_y: f32,
    clip_min_x: f32,
    clip_min_y: f32,
    clip_max_x: f32,
    clip_max_y: f32,
    color: u32,
    skew: f32,
    pad0: f32,
    pad1: f32,
};
var<storage, read> r_quads: array<Quad>;

struct VertexOutput {
    @location(0) tex_coord: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) point: vec2<f32>,
    @location(3) @interpolate(flat) clip: vec4<f32>,
    @builtin(position) position: vec4<f32>,
};

fn linear_from_gamma(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = srgb < vec3<f32>(0.04045);
    let lower = srgb / vec3<f32>(12.92);
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

@vertex
fn vs_main(@builtin(vertex_index) v_index: u32) -> VertexOutput {
    let quad = r_quads[v_index / 6u];
    // Corners 0..3 are left-top, right-top, left-bottom, right-bottom, in
    // egui's triangle order (0, 1, 2) and (2, 1, 3).
    var corners = array<u32, 6>(0u, 1u, 2u, 2u, 1u, 3u);
    let corner = corners[v_index % 6u];
    let right = (corner & 1u) != 0u;
    let bottom = corner >= 2u;
    // Italics shear the top edge to the right, like epaint.
    let shear = select(quad.skew, 0.0, bottom);
    let point = vec2<f32>(
        select(quad.min_x, quad.max_x, right) + shear,
        select(quad.min_y, quad.max_y, bottom),
    );
    let uv = vec2<f32>(
        select(quad.uv_min_x, quad.uv_max_x, right),
        select(quad.uv_min_y, quad.uv_max_y, bottom),
    );
    var out: VertexOutput;
    out.tex_coord = uv / r_uniforms.atlas_size;
    out.color = unpack4x8unorm(quad.color);
    out.point = point;
    out.clip = vec4<f32>(quad.clip_min_x, quad.clip_min_y, quad.clip_max_x, quad.clip_max_y);
    out.position = vec4<f32>(
        2.0 * point.x / r_uniforms.screen_size.x - 1.0,
        1.0 - 2.0 * point.y / r_uniforms.screen_size.y,
        0.0,
        1.0,
    );
    return out;
}

var r_texture: texture_2d<f32>;
var r_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sample first: textureSample must stay in uniform control flow.
    let blended = in.color * textureSample(r_texture, r_sampler, in.tex_coord);
    // Per-quad clip: a glyph may not spill out of its cell, as with the
    // per-run clip rects this replaces.
    if (any(in.point < in.clip.xy) || any(in.point >= in.clip.zw)) {
        discard;
    }
    return vec4<f32>(linear_from_gamma(blended.xyz), blended.a);
}
