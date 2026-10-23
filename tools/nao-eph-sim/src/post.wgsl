// Post passes: screen-space ambient occlusion, a depth-aware blur, and the final composite.

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    light_view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    sky: vec4<f32>,
    ground: vec4<f32>,
    light_pos: array<vec4<f32>, 4>,
    light_color: array<vec4<f32>, 4>,
    // x: light count, y: width, z: height, w: tan(fov_y / 2)
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var tex_a: texture_2d<f32>;
@group(0) @binding(2) var tex_b: texture_2d<f32>;
@group(0) @binding(3) var tex_c: texture_2d<f32>;
@group(0) @binding(4) var tex_d: texture_2d<f32>;
@group(1) @binding(0) var sky: texture_cube<f32>;
@group(1) @binding(1) var sky_sampler: sampler;

// The sky seen through a pixel: a world-space ray from the camera, looked up in the cube map.
fn sky_color(pixel: vec2<f32>) -> vec3<f32> {
    let dims = vec2<f32>(size());
    let ndc = vec2<f32>(pixel.x / dims.x * 2.0 - 1.0, 1.0 - pixel.y / dims.y * 2.0);
    let far = globals.inv_view_proj * vec4<f32>(ndc, 1.0, 1.0);
    let dir = normalize(far.xyz / far.w - globals.camera_pos.xyz);
    return textureSampleLevel(sky, sky_sampler, vec3<f32>(dir.x, dir.z, -dir.y), 0.0).rgb;
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

fn size() -> vec2<i32> {
    return vec2<i32>(i32(globals.params.y), i32(globals.params.z));
}

fn load(t: texture_2d<f32>, p: vec2<i32>) -> vec4<f32> {
    return textureLoad(t, clamp(p, vec2<i32>(0), size() - 1), 0);
}

// View-space position of a pixel from its depth.
fn view_position(pixel: vec2<f32>, depth: f32) -> vec3<f32> {
    let dims = vec2<f32>(size());
    let ndc = vec2<f32>(pixel.x / dims.x * 2.0 - 1.0, 1.0 - pixel.y / dims.y * 2.0);
    let tan_half = globals.params.w;
    let aspect = dims.x / dims.y;
    return vec3<f32>(ndc.x * tan_half * aspect * depth, ndc.y * tan_half * depth, -depth);
}

fn project(p: vec3<f32>) -> vec2<f32> {
    let dims = vec2<f32>(size());
    let tan_half = globals.params.w;
    let aspect = dims.x / dims.y;
    let ndc = vec2<f32>(p.x / (-p.z * tan_half * aspect), p.y / (-p.z * tan_half));
    return vec2<f32>((ndc.x * 0.5 + 0.5) * dims.x, (0.5 - ndc.y * 0.5) * dims.y);
}

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}

@fragment
fn fs_ssao(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let center = load(tex_a, vec2<i32>(frag.xy));
    let depth = center.w;
    if (depth > 100.0) {
        return vec4<f32>(1.0);
    }
    let n = normalize(center.xyz);
    let p = view_position(frag.xy, depth);
    // Tangent frame with a per-pixel random rotation, so the noise blurs away.
    let angle = hash(frag.xy) * 6.2831853;
    let random = vec3<f32>(cos(angle), sin(angle), 0.0);
    let tangent = normalize(random - n * dot(random, n));
    let bitangent = cross(n, tangent);

    let radius = 0.45;
    let samples = 16;
    var occlusion = 0.0;
    for (var i = 0; i < samples; i++) {
        // Points spread through the hemisphere, denser near the surface.
        let fi = f32(i) + 0.5;
        let phi = fi * 2.39996;
        let cos_theta = sqrt(1.0 - fi / f32(samples));
        let sin_theta = sqrt(1.0 - cos_theta * cos_theta);
        var scale = fi / f32(samples);
        scale = mix(0.1, 1.0, scale * scale);
        let dir = tangent * (cos(phi) * sin_theta) + bitangent * (sin(phi) * sin_theta) + n * cos_theta;
        let s = p + dir * radius * scale;
        let sample_depth = load(tex_a, vec2<i32>(project(s))).w;
        let range = smoothstep(0.0, 1.0, radius / abs(depth - sample_depth));
        if (sample_depth < -s.z - 0.02) {
            occlusion += range;
        }
    }
    let ao = 1.0 - occlusion / f32(samples);
    return vec4<f32>(pow(ao, 1.6));
}

@fragment
fn fs_blur(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(frag.xy);
    let depth = load(tex_b, p).w;
    var sum = 0.0;
    var weight = 0.0;
    for (var x = -3; x <= 3; x++) {
        for (var y = -3; y <= 3; y++) {
            let q = p + vec2<i32>(x, y);
            let d = load(tex_b, q).w;
            let w = exp(-abs(d - depth) * 8.0) * exp(-f32(x * x + y * y) / 12.0);
            sum += load(tex_a, q).r * w;
            weight += w;
        }
    }
    return vec4<f32>(sum / max(weight, 1e-4));
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_composite(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(frag.xy);
    let direct = load(tex_a, p);
    let ambient = load(tex_b, p).rgb;
    let ao = load(tex_c, p).r;
    var color: vec3<f32>;
    // Nothing drawn here (ambient alpha 0): show the sky, already display-referred.
    if (load(tex_b, p).a < 0.01) {
        color = sky_color(frag.xy);
    } else {
        // AO darkens only bounce light; a touch on direct light grounds contact areas.
        color = direct.rgb * mix(1.0, ao, 0.25) + ambient * ao;
        let exposure = 1.15;
        color = aces(color * exposure);
        // Distance haze: the world fades into the sky's own horizon, so it feels endless.
        let depth = load(tex_d, p).w;
        let fog = 1.0 - exp(-pow(max(depth - 16.0, 0.0) / 26.0, 2.0));
        color = mix(color, sky_color(frag.xy), clamp(fog, 0.0, 1.0));
    }
    // Gentle grade: a little contrast and warmth.
    color = mix(vec3<f32>(dot(color, vec3<f32>(0.2126, 0.7152, 0.0722))), color, 1.06);
    color = color * vec3<f32>(1.02, 1.0, 0.97);
    // Vignette.
    let uv = frag.xy / vec2<f32>(size());
    let vignette = 1.0 - 0.28 * pow(length(uv - 0.5) * 1.35, 2.2);
    color *= vignette;
    // Dither to avoid banding in the 8-bit output.
    color += (hash(frag.xy) - 0.5) / 255.0;
    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
