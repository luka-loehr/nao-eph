// Scene pass: PBR shading split into direct light and ambient light (so SSAO only darkens the
// ambient part), plus view-space normal and depth for the SSAO pass.

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    light_view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    // xyz: direction the sunlight travels, w: intensity
    sun_dir: vec4<f32>,
    sun_color: vec4<f32>,
    // rgb: color, a: ambient strength
    sky: vec4<f32>,
    ground: vec4<f32>,
    // xyz: world position, w: range
    light_pos: array<vec4<f32>, 4>,
    // rgb: color * intensity
    light_color: array<vec4<f32>, 4>,
    // x: light count, y: width, z: height, w: tan(fov_y / 2)
    params: vec4<f32>,
    // rgb: fog colour, a: distance where fog starts
    fog: vec4<f32>,
    // x: fog falloff distance, y: 1 in the studio (backdrop colour instead of the sky)
    fog_params: vec4<f32>,
};

struct Instance {
    model: mat4x4<f32>,
    normal: mat4x4<f32>,
    base_color: vec4<f32>,
    // x: roughness, y: metalness, z: has texture
    params: vec4<f32>,
    emissive: vec4<f32>,
    uv_scale: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var shadow_map: texture_depth_2d;
@group(0) @binding(2) var shadow_sampler: sampler_comparison;
@group(1) @binding(0) var<uniform> inst: Instance;
@group(2) @binding(0) var base_map: texture_2d<f32>;
@group(2) @binding(1) var base_sampler: sampler;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) light_space: vec4<f32>,
};

struct FsOut {
    @location(0) direct: vec4<f32>,
    @location(1) ambient: vec4<f32>,
    // xyz: view-space normal, w: view-space depth (positive, metres)
    @location(2) normal_depth: vec4<f32>,
};

const PI: f32 = 3.14159265;

@vertex
fn vs_main(v: VsIn) -> VsOut {
    let world = inst.model * vec4<f32>(v.position, 1.0);
    var out: VsOut;
    out.clip = globals.view_proj * world;
    out.world = world.xyz;
    out.normal = normalize((inst.normal * vec4<f32>(v.normal, 0.0)).xyz);
    out.uv = v.uv * inst.uv_scale.xy;
    out.light_space = globals.light_view_proj * world;
    return out;
}

struct ShadowOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_shadow(v: VsIn) -> ShadowOut {
    var out: ShadowOut;
    out.clip = globals.light_view_proj * inst.model * vec4<f32>(v.position, 1.0);
    out.uv = v.uv * inst.uv_scale.xy;
    return out;
}

// Leaves and other cut-out textures cast shaped shadows instead of solid quads.
@fragment
fn fs_shadow(in: ShadowOut) {
    if (inst.params.z > 0.5 && textureSample(base_map, base_sampler, in.uv).a < 0.5) {
        discard;
    }
}

// Soft shadow: a rotated Poisson-like disc of comparison taps.
fn shadow_factor(light_space: vec4<f32>, world: vec3<f32>) -> f32 {
    let ndc = light_space.xyz / light_space.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, -ndc.y * 0.5 + 0.5);
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 || ndc.z > 1.0) {
        return 1.0;
    }
    let angle = fract(sin(dot(world.xy, vec2<f32>(12.9898, 78.233))) * 43758.5453) * 6.2831853;
    let rot = mat2x2<f32>(cos(angle), -sin(angle), sin(angle), cos(angle));
    let radius = 2.2 / 4096.0;
    var lit = 0.0;
    for (var i = 0; i < 16; i++) {
        let r = sqrt((f32(i) + 0.5) / 16.0);
        let a = f32(i) * 2.39996;
        let offset = rot * vec2<f32>(cos(a), sin(a)) * r * radius;
        lit += textureSampleCompareLevel(shadow_map, shadow_sampler, uv + offset, ndc.z - 0.0008);
    }
    return lit / 16.0;
}

fn ggx(n_dot_h: f32, roughness: f32) -> f32 {
    let a = roughness * roughness;
    let a2 = a * a;
    let d = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    return a2 / (PI * d * d);
}

fn environment(dir: vec3<f32>) -> vec3<f32> {
    let t = clamp(dir.z * 0.5 + 0.5, 0.0, 1.0);
    return mix(globals.ground.rgb, globals.sky.rgb, smoothstep(0.25, 0.75, t));
}

// Cook-Torrance BRDF times n·l for one light.
fn brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, albedo: vec3<f32>, roughness: f32, metalness: f32) -> vec3<f32> {
    let h = normalize(l + v);
    let n_dot_l = max(dot(n, l), 0.0);
    let n_dot_v = max(dot(n, v), 1e-3);
    let n_dot_h = max(dot(n, h), 0.0);
    let f0 = mix(vec3<f32>(0.04), albedo, metalness);
    let fresnel = f0 + (1.0 - f0) * pow(1.0 - max(dot(h, v), 0.0), 5.0);
    let k = (roughness + 1.0) * (roughness + 1.0) / 8.0;
    let geometry = (n_dot_l / (n_dot_l * (1.0 - k) + k)) * (n_dot_v / (n_dot_v * (1.0 - k) + k));
    let specular = ggx(n_dot_h, roughness) * geometry * fresnel / max(4.0 * n_dot_l * n_dot_v, 1e-3);
    let diffuse = (1.0 - fresnel) * (1.0 - metalness) * albedo / PI;
    return (diffuse + specular) * n_dot_l;
}

@fragment
fn fs_main(in: VsOut, @builtin(front_facing) front: bool) -> FsOut {
    var albedo = inst.base_color.rgb;
    var alpha = inst.base_color.a;
    if (inst.params.z > 0.5) {
        let t = textureSample(base_map, base_sampler, in.uv);
        albedo *= t.rgb;
        alpha *= t.a;
        // Opaque materials with cut-out textures (tree leaves, fences): alpha test, not blending.
        if (inst.base_color.a > 0.99 && t.a < 0.5) {
            discard;
        }
    }
    // Webots leaves many parts at metalness 1 / roughness 0; without an environment map that reads
    // as black chrome, so keep the look grounded.
    let roughness = clamp(inst.params.x, 0.18, 1.0);
    let metalness = clamp(inst.params.y, 0.0, 0.85);

    var n = normalize(in.normal);
    if (!front) {
        n = -n;
    }
    let v = normalize(globals.camera_pos.xyz - in.world);

    // Sun through the window.
    let sun = globals.sun_color.rgb * globals.sun_dir.w * shadow_factor(in.light_space, in.world);
    var direct = brdf(n, v, -globals.sun_dir.xyz, albedo, roughness, metalness) * sun;

    // Lamps: point lights with a smooth inverse-square falloff.
    let count = i32(globals.params.x);
    for (var i = 0; i < count; i++) {
        let to_light = globals.light_pos[i].xyz - in.world;
        let d = length(to_light);
        let range = globals.light_pos[i].w;
        let window = pow(clamp(1.0 - pow(d / range, 4.0), 0.0, 1.0), 2.0);
        let falloff = window / (d * d + 0.25);
        direct += brdf(n, v, to_light / d, albedo, roughness, metalness) * globals.light_color[i].rgb * falloff;
    }
    direct += inst.emissive.rgb;

    // Bounce light: hemisphere ambient plus a blurred environment reflection.
    let n_dot_v = max(dot(n, v), 1e-3);
    let f0 = mix(vec3<f32>(0.04), albedo, metalness);
    let env_fresnel = f0 + (max(vec3<f32>(1.0 - roughness), f0) - f0) * pow(1.0 - n_dot_v, 5.0);
    let ambient = environment(n) * globals.sky.a * albedo * (1.0 - metalness)
        + environment(reflect(-v, n)) * env_fresnel * (1.0 - roughness * 0.75) * 0.25;

    var out: FsOut;
    out.direct = vec4<f32>(direct, alpha);
    out.ambient = vec4<f32>(ambient, alpha);
    let view_pos = globals.view * vec4<f32>(in.world, 1.0);
    let view_normal = normalize((globals.view * vec4<f32>(n, 0.0)).xyz);
    out.normal_depth = vec4<f32>(view_normal, -view_pos.z);
    return out;
}
