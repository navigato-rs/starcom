// Post effects on the already drawn UI.
// Wave refracts around the pointer. A narrow spectrum (a red line) stays
// that color and only shifts; white and gray light spreads into real
// chromatic fringes. Matrix is a green rain mask on the existing picture:
// dst * (0.5 + src). Trails lift the green channel and the rest sits dimmer.
// Retro draws the frame as a curved CRT.

struct Uniforms {
    // xy: physical screen size, zw: pointer in physical pixels.
    screen_pointer: vec4<f32>,
    // x: seconds. y: 0 wave, 1 matrix, 2 retro. z: retro curvature.
    time: vec4<f32>,
};
var<uniform> r_uniforms: Uniforms;
var r_scene: texture_2d<f32>;
var r_sampler: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let x = 4.0 * f32(vertex_index & 1u) - 1.0;
    let y = 4.0 * f32(vertex_index >> 1u) - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn scene_at(uv: vec2<f32>) -> vec4<f32> {
    return textureSample(r_scene, r_sampler, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)));
}

fn straight(color: vec4<f32>) -> vec3<f32> {
    return color.rgb / max(color.a, 0.001);
}

fn saturation(rgb: vec3<f32>) -> f32 {
    let hi = max(rgb.r, max(rgb.g, rgb.b));
    let lo = min(rgb.r, min(rgb.g, rgb.b));
    return (hi - lo) / max(hi, 1e-4);
}

// +1 is red, 0 is green, -1 is blue.
fn spectral_bias(rgb: vec3<f32>) -> f32 {
    let sum = rgb.r + rgb.g + rgb.b;
    return select(0.0, (rgb.r - rgb.b) / sum, sum > 1e-4);
}

fn gray_level(rgb: vec3<f32>) -> f32 {
    return min(rgb.r, min(rgb.g, rgb.b));
}

// Saturated light stays on the ray of its own hue, so a green glyph stays
// green. Gray light is broadband and fringes.
fn hue_on_ray(rgb: vec3<f32>, ray: f32) -> vec3<f32> {
    let gray = gray_level(rgb);
    let sat = saturation(rgb);
    let align = 1.0 - abs(spectral_bias(rgb) - ray);
    let keep = sat * smoothstep(0.45, 0.9, align);
    return (rgb - vec3<f32>(gray)) * keep;
}

fn wave(px: vec2<f32>, screen: vec2<f32>, pointer: vec2<f32>, time: f32) -> vec4<f32> {
    let delta = px - pointer;
    let dist = length(delta);
    let dir = select(delta / dist, vec2<f32>(0.0), dist < 0.5);
    let tangent = vec2<f32>(-dir.y, dir.x);
    let wave = sin(dist * 0.055 - time * 9.0);
    let wave2 = sin(dist * 0.023 - time * 4.6 + 1.7);
    let near = exp(-dist * dist / (140.0 * 140.0));
    let mid = exp(-dist * dist / (280.0 * 280.0));
    let bend = wave * near + wave2 * mid * 0.35;
    let shift = (dir * 14.0 + tangent * 4.0) * bend;
    let chroma = dir * (2.0 + 11.0 * abs(bend));
    let px_to_uv = vec2<f32>(1.0) / screen;
    let uv = px * px_to_uv;

    let red_sample = scene_at(uv + (shift + chroma) * px_to_uv);
    let green_sample = scene_at(uv + shift * px_to_uv);
    let blue_sample = scene_at(uv + (shift - chroma) * px_to_uv);
    let at_red = straight(red_sample);
    let at_green = straight(green_sample);
    let at_blue = straight(blue_sample);
    // Each focal plane gets that wavelength of the gray light, plus a
    // saturated color only when the color itself belongs on that plane.
    let rgb = vec3<f32>(gray_level(at_red), gray_level(at_green), gray_level(at_blue))
        + hue_on_ray(at_red, 1.0)
        + hue_on_ray(at_green, 0.0)
        + hue_on_ray(at_blue, -1.0);
    let alpha = max(red_sample.a, max(green_sample.a, blue_sample.a));
    return vec4<f32>(rgb * alpha, alpha);
}

// Barrel in centered coordinates. Dividing by the scale at an edge midpoint
// keeps the middle of each side on the frame. Corners land past ±1, off the glass.
fn tube_centered(centered: vec2<f32>, curvature: f32) -> vec2<f32> {
    let bulge = dot(centered, centered);
    return centered * (1.0 + curvature * bulge) / (1.0 + curvature);
}

fn retro(px: vec2<f32>, screen: vec2<f32>, time: f32, curvature: f32) -> vec4<f32> {
    let uv = px / screen;
    let tube = tube_centered(uv * 2.0 - vec2<f32>(1.0), curvature);
    // One pixel past the glass eases to black. Further out is solid black.
    let feather = 2.0 / min(screen.x, screen.y);
    let mask = 1.0 - smoothstep(1.0, 1.0 + feather, max(abs(tube.x), abs(tube.y)));
    let sample_uv = tube * 0.5 + vec2<f32>(0.5);
    let centered = sample_uv * 2.0 - vec2<f32>(1.0);
    let falloff = dot(centered, centered);
    // Guns miss each other toward the rim. The middle of the tube stays aligned.
    let shift = centered * falloff * 0.003;
    let red = scene_at(sample_uv + shift);
    let green = scene_at(sample_uv);
    let blue = scene_at(sample_uv - shift);
    var rgb = vec3<f32>(straight(red).r, straight(green).g, straight(blue).b);
    let px_to_uv = vec2<f32>(1.0) / screen;
    let glow = straight(scene_at(sample_uv + vec2<f32>(1.6, 0.0) * px_to_uv))
        + straight(scene_at(sample_uv - vec2<f32>(1.6, 0.0) * px_to_uv))
        + straight(scene_at(sample_uv + vec2<f32>(0.0, 1.6) * px_to_uv))
        + straight(scene_at(sample_uv - vec2<f32>(0.0, 1.6) * px_to_uv));
    rgb = rgb + glow * 0.06;
    let sine = sin(px.y * 1.5707963);
    let scan = 0.92 + 0.08 * sine * sine;
    let sweep = exp(-pow(fract(uv.y - time * 0.12) - 0.015, 2.0) * 4000.0);
    let flicker = 0.98 + 0.02 * sin(time * 42.0);
    let snow = fract(sin(dot(px + vec2<f32>(time * 17.0, time * 9.0), vec2<f32>(12.9898, 78.233))) * 43758.5453);
    let glass = uv * 2.0 - vec2<f32>(1.0);
    let vignette = mix(0.9, 1.0, smoothstep(1.7, 0.35, dot(glass, glass)));
    rgb = rgb * scan * flicker * vignette;
    rgb = rgb + vec3<f32>((snow - 0.5) * 0.045);
    rgb = rgb * (1.0 + sweep * 0.18);
    return vec4<f32>(max(rgb, vec3<f32>(0.0)) * mask, 1.0);
}

fn hash11(n: f32) -> f32 {
    return fract(sin(n) * 43758.5453);
}

// x: how fully this pixel sits in a falling trail. y: the bright head.
// The head moves down the column. The trail is a tall run of cells above it.
// Nothing new is drawn; the trail only marks pixels that are already lit.
fn matrix_streams(px: vec2<f32>, screen_y: f32, time: f32) -> vec2<f32> {
    let cell_h = 16.0;
    let col = floor(px.x / 12.0);
    let row = floor(px.y / cell_h);
    let rows = max(screen_y / cell_h, 1.0);
    var cover = 0.0;
    var tip = 0.0;
    for (var stream = 0u; stream < 2u; stream = stream + 1u) {
        let seed = col * 2.0 + 1.0 + f32(stream) * 97.0;
        // A long trail, so a passing column lights a whole block of text.
        let span = rows * (0.22 + 0.28 * hash11(seed * 1.7));
        let speed = 6.0 + 10.0 * hash11(seed * 2.3);
        let phase = hash11(seed * 3.1) * rows;
        let lead = fract((time * speed + phase) / rows) * rows;
        let above = select(lead - row + rows, lead - row, lead >= row);
        let fade = clamp(1.0 - above / max(span, 1.0), 0.0, 1.0);
        let on = select(0.0, 1.0, above < span);
        let strength = on * fade;
        cover = max(cover, strength);
        tip = max(tip, select(0.0, fade, above < 1.5));
    }
    return vec2<f32>(cover, tip);
}

fn matrix(px: vec2<f32>, screen: vec2<f32>, time: f32, color: vec4<f32>) -> vec4<f32> {
    let stream = matrix_streams(px, screen.y, time);
    // 0 outside a trail, 1 on the head. The picture itself does not move.
    let mask = clamp(stream.x * 0.8 + stream.y * 0.2, 0.0, 1.0);
    // Theme black is a few percent in linear light. The mask starts above
    // that, so an empty background stays as drawn.
    let luma = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let presence = smoothstep(0.04, 0.14, luma);
    // Green rain. dst * 0.5 + src * dst, only where the pixel already has light.
    let src = vec3<f32>(0.15, 1.0, 0.28) * mask * presence;
    let modulated = color.rgb * (vec3<f32>(0.5) + src);
    return vec4<f32>(mix(color.rgb, modulated, presence), color.a);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let screen = max(r_uniforms.screen_pointer.xy, vec2<f32>(1.0));
    let pointer = r_uniforms.screen_pointer.zw;
    let time = r_uniforms.time.x;
    let mode = r_uniforms.time.y;
    if (mode > 1.5) {
        return retro(frag.xy, screen, time, r_uniforms.time.z);
    }
    if (mode > 0.5) {
        let uv = frag.xy / screen;
        return matrix(frag.xy, screen, time, scene_at(uv));
    }
    return wave(frag.xy, screen, pointer, time);
}
