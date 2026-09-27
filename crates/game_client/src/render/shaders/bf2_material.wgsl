// BF2 materials on Bevy's PBR (`Bf2Layers` in materials.rs), after BF2's RaShaderSTM.fx
// (static meshes), RaShaderBM.fx (bundled) and RaShaderSM.fx (skinned):
//
// - Static: color = base (UV0) x detail (UV1) x dirt, the crack blended over by its alpha,
//   all doubled in gamma space (BF2 lights static meshes 2x; folded into the albedo as
//   2^2.2 in linear space, clamped at 1). Detail
//   normal map on UV1 with the crack normal map blended over by the crack alpha; parallax
//   shifts the detail UVs by the normal map's alpha as height. Gloss is the detail alpha,
//   unless base x detail alpha is the alpha test.
// - Bundled and skinned: tangent or object space normal map on UV0, gloss in its alpha or in
//   the color map's alpha (`ColormapGloss`, whose alpha test is the color's brightness).
//   `EnvMap` blends the reflected environment into the color by gloss / 4 and makes
//   transparent surfaces more opaque at grazing angles (BF2's Fresnel term).
// - BF2's Blinn-Phong highlight (exponent 32-36, scaled by gloss) becomes a fixed roughness
//   with the reflectance scaled by gloss.
//
// One file for the main pass and the prepass (`PREPASS_PIPELINE`: alpha test, and the normals
// that SSAO and the main pass read), bindless (the normal case) or bound. Layer textures are
// sampled with explicit gradients because the flags that select them aren't uniform.

#import bevy_pbr::{
    mesh_bindings::mesh,
    mesh_view_bindings::view,
    pbr_bindings,
    pbr_functions,
    pbr_types,
}
#import bevy_render::bindless::{bindless_samplers_filtering, bindless_textures_2d, bindless_textures_cube}

#ifdef PREPASS_PIPELINE
#import bevy_pbr::{
    prepass_io::{VertexOutput, FragmentOutput},
    pbr_prepass_functions,
}
#else
#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_fragment::pbr_input_from_standard_material,
}
#endif

#ifdef BINDLESS
#import bevy_pbr::pbr_bindings::material_indices
#endif

struct Bf2Layers {
    flags: u32,
    gloss: f32,
    albedo_scale: f32,
    _pad: f32,
}

#ifdef BINDLESS
struct Bf2LayersIndices {
    material: u32,       // 50
    detail: u32,         // 51
    layer_sampler: u32,  // 52
    dirt: u32,           // 53
    crack: u32,          // 54
    normal: u32,         // 55
    crack_normal: u32,   // 56
    env_map: u32,        // 57
}
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<storage> bf2_indices: array<Bf2LayersIndices>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<storage> bf2_layers: array<Bf2Layers>;
#else
@group(#{MATERIAL_BIND_GROUP}) @binding(50) var<uniform> bf2_layers_bound: Bf2Layers;
@group(#{MATERIAL_BIND_GROUP}) @binding(51) var detail_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(52) var layer_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(53) var dirt_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(54) var crack_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(55) var normal_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(56) var crack_normal_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(57) var env_texture: texture_cube<f32>;
#endif

const DETAIL: u32 = 1u;
const DIRT: u32 = 2u;
const CRACK: u32 = 4u;
const NORMAL_MAP: u32 = 8u;
const NORMAL_UV_B: u32 = 16u;
const OBJECT_SPACE: u32 = 32u;
const CRACK_NORMAL: u32 = 64u;
const PARALLAX: u32 = 128u;
const GLOSS_FROM_DETAIL: u32 = 256u;
const GLOSS_FROM_NORMAL: u32 = 512u;
const GLOSS_FROM_BASE: u32 = 1024u;
const ALPHA_FROM_DETAIL: u32 = 2048u;
const ALPHA_FROM_COLOR: u32 = 4096u;
const ENV_MAP: u32 = 8192u;
const UV_SUM: u32 = 16384u;
const WRECK: u32 = 32768u;

const LAYER_DETAIL: u32 = 0u;
const LAYER_DIRT: u32 = 1u;
const LAYER_CRACK: u32 = 2u;
const LAYER_NORMAL: u32 = 3u;
const LAYER_CRACK_NORMAL: u32 = 4u;

// BF2's hard-coded parallax scale (FH2_HARDCODED_PARALLAX_BIAS).
const PARALLAX_SCALE: f32 = 0.0025;
// A Blinn-Phong exponent of ~32 as GGX roughness.
const GLOSS_ROUGHNESS: f32 = 0.5;
// BF2's Fresnel constant for EnvMap alpha: R0 from a refraction index ratio of 0.15.
const FRESNEL_R0: f32 = 0.4132;

fn layers_of(slot: u32) -> Bf2Layers {
#ifdef BINDLESS
    var layers = bf2_layers[bf2_indices[slot].material];
#else
    var layers = bf2_layers_bound;
#endif
#ifdef BF2_DEBUG_NOLAYERS
    // Base color only, to measure what the layers cost.
    layers.flags = 0u;
#endif
    return layers;
}

fn sample_layer(slot: u32, layer: u32, uv: vec2<f32>, ddx: vec2<f32>, ddy: vec2<f32>) -> vec4<f32> {
#ifdef BINDLESS
    let indices = bf2_indices[slot];
    var texture = indices.detail;
    if layer == LAYER_DIRT {
        texture = indices.dirt;
    } else if layer == LAYER_CRACK {
        texture = indices.crack;
    } else if layer == LAYER_NORMAL {
        texture = indices.normal;
    } else if layer == LAYER_CRACK_NORMAL {
        texture = indices.crack_normal;
    }
    return textureSampleGrad(
        bindless_textures_2d[texture],
        bindless_samplers_filtering[indices.layer_sampler],
        uv,
        ddx,
        ddy,
    );
#else
    switch layer {
        case 1u: { return textureSampleGrad(dirt_texture, layer_sampler, uv, ddx, ddy); }
        case 2u: { return textureSampleGrad(crack_texture, layer_sampler, uv, ddx, ddy); }
        case 3u: { return textureSampleGrad(normal_texture, layer_sampler, uv, ddx, ddy); }
        case 4u: { return textureSampleGrad(crack_normal_texture, layer_sampler, uv, ddx, ddy); }
        default: { return textureSampleGrad(detail_texture, layer_sampler, uv, ddx, ddy); }
    }
#endif
}

fn sample_env(slot: u32, direction: vec3<f32>) -> vec3<f32> {
    // BF2's cube maps are in its left-handed space: mirror Z back.
    let bf2_direction = direction * vec3(1.0, 1.0, -1.0);
#ifdef BINDLESS
    let indices = bf2_indices[slot];
    return textureSampleLevel(
        bindless_textures_cube[indices.env_map],
        bindless_samplers_filtering[indices.layer_sampler],
        bf2_direction,
        0.0,
    ).rgb;
#else
    return textureSampleLevel(env_texture, layer_sampler, bf2_direction, 0.0).rgb;
#endif
}

struct SurfaceInput {
    slot: u32,
    uv: vec2<f32>,
    uv_b: vec2<f32>,
    // COLOR_0: dirt and crack UVs (static), or the unskinned tangent and normal.x
    // (object space normal maps; normal.yz in `uv_b`).
    packed: vec4<f32>,
    world_normal: vec3<f32>,
    world_tangent: vec4<f32>,
    has_tangents: bool,
    // The normal the prepass wrote, when the main pass reads it (`LOAD_PREPASS_NORMALS`):
    // normal maps are then only sampled for gloss.
    prepass_normal: vec3<f32>,
    has_prepass_normal: bool,
    // Towards the camera.
    V: vec3<f32>,
    front_facing: bool,
    double_sided: bool,
    // Base color texture x factor.
    base: vec4<f32>,
}

struct Surface {
    color: vec4<f32>,
    gloss: f32,
    N: vec3<f32>,
}

fn bf2_surface(in: SurfaceInput, layers: Bf2Layers) -> Surface {
    let flags = layers.flags;
    // Gradients while control flow is still uniform.
    let uv_dx = dpdx(in.uv);
    let uv_dy = dpdy(in.uv);
    let uv_b_dx = dpdx(in.uv_b);
    let uv_b_dy = dpdy(in.uv_b);
    let packed_dx = dpdx(in.packed);
    let packed_dy = dpdy(in.packed);

    var color = in.base;
    var gloss = layers.gloss;
    var gloss_scale = 1.0;

    let N = in.world_normal;
    let T = in.world_tangent.xyz;
    let B = in.world_tangent.w * cross(N, T);
    let normal_mapped = in.has_tangents && (flags & (NORMAL_MAP | OBJECT_SPACE)) != 0u;

    var detail_uv = in.uv_b;
    if normal_mapped && (flags & PARALLAX) != 0u {
        let height = sample_layer(in.slot, LAYER_NORMAL, in.uv_b, uv_b_dx, uv_b_dy).a;
        let Vt = vec2(dot(in.V, T), dot(in.V, B));
        detail_uv += height * PARALLAX_SCALE * vec2(Vt.x, -Vt.y);
    }

    if (flags & DETAIL) != 0u {
        let detail = sample_layer(in.slot, LAYER_DETAIL, detail_uv, uv_b_dx, uv_b_dy);
        color = vec4(color.rgb * detail.rgb, color.a);
        if (flags & GLOSS_FROM_DETAIL) != 0u {
            gloss = detail.a;
        }
        if (flags & ALPHA_FROM_DETAIL) != 0u {
            color.a *= detail.a;
        }
    }
    if (flags & WRECK) != 0u {
        let wreck = sample_layer(in.slot, LAYER_DETAIL, in.uv, uv_dx, uv_dy);
        color = vec4(color.rgb * wreck.rgb, color.a);
        gloss_scale = wreck.g;
    }
    if (flags & GLOSS_FROM_BASE) != 0u {
        gloss = color.a;
        // BF2's brightness alpha test sums gamma-space values.
        let brightness = dot(pow(max(color.rgb, vec3(0.0)), vec3(1.0 / 2.2)), vec3(1.0));
        color.a = select(1.0, brightness, (flags & ALPHA_FROM_COLOR) != 0u);
    }
    var crack_mask = 0.0;
    if (flags & DIRT) != 0u {
        let dirt = sample_layer(in.slot, LAYER_DIRT, in.packed.xy, packed_dx.xy, packed_dy.xy);
        color = vec4(color.rgb * dirt.rgb, color.a);
    }
    if (flags & CRACK) != 0u {
        let crack = sample_layer(in.slot, LAYER_CRACK, in.packed.zw, packed_dx.zw, packed_dy.zw);
        crack_mask = crack.a;
        color = vec4(mix(color.rgb, crack.rgb, crack.a), color.a);
    }
    // BF2 doubles in gamma space, where detail textures average 0.5 grey; the products of
    // textures are the same in linear space, but the factor becomes 2^2.2.
    color = vec4(min(color.rgb * pow(layers.albedo_scale, 2.2), vec3(1.0)), color.a);

    var world_normal = normalize(N);
    let map_normal = !in.has_prepass_normal;
    let normal_texel = map_normal || (flags & GLOSS_FROM_NORMAL) != 0u;
    if normal_mapped && normal_texel && (flags & NORMAL_MAP) != 0u {
        var normal_uv = in.uv;
        var normal_dx = uv_dx;
        var normal_dy = uv_dy;
        if (flags & NORMAL_UV_B) != 0u {
            normal_uv = detail_uv;
            normal_dx = uv_b_dx;
            normal_dy = uv_b_dy;
        }
        let texel = sample_layer(in.slot, LAYER_NORMAL, normal_uv, normal_dx, normal_dy);
        var Nt = texel.rgb;
        if map_normal && (flags & CRACK_NORMAL) != 0u {
            let crack_normal = sample_layer(in.slot, LAYER_CRACK_NORMAL, in.packed.zw, packed_dx.zw, packed_dy.zw);
            Nt = mix(Nt, crack_normal.rgb, crack_mask);
        }
        let n = normalize(Nt * 2.0 - 1.0);
        // BF2's binormal points towards -v like glTF's, so the frame maps over directly.
        world_normal = normalize(n.x * T + n.y * B + n.z * N);
        if (flags & GLOSS_FROM_NORMAL) != 0u {
            gloss = texel.a;
        }
    } else if normal_mapped && normal_texel && (flags & OBJECT_SPACE) != 0u {
        let texel = sample_layer(in.slot, LAYER_NORMAL, in.uv, uv_dx, uv_dy);
        // Bind pose normal, BF2's left-handed space mirrored to ours.
        let n = normalize(texel.rgb * 2.0 - 1.0) * vec3(1.0, 1.0, -1.0);
        // Object to world is the rotation taking the unskinned vertex frame to the skinned
        // one: express `n` in the unskinned frame, then rebuild it from the skinned frame.
        let n_obj = normalize(vec3(in.packed.w, in.uv_b));
        let t_obj = normalize(in.packed.xyz - n_obj * dot(in.packed.xyz, n_obj));
        let b_obj = in.world_tangent.w * cross(n_obj, t_obj);
        let n_world = normalize(N);
        let t_world = normalize(T - n_world * dot(T, n_world));
        let b_world = in.world_tangent.w * cross(n_world, t_world);
        world_normal = normalize(
            dot(n, t_obj) * t_world + dot(n, b_obj) * b_world + dot(n, n_obj) * n_world
        );
        if (flags & GLOSS_FROM_NORMAL) != 0u {
            gloss = texel.a;
        }
    }
    if in.double_sided && !in.front_facing {
        world_normal = -world_normal;
    }
    if in.has_prepass_normal {
        world_normal = in.prepass_normal;
    }
    gloss *= gloss_scale;

    if (flags & ENV_MAP) != 0u {
        let env = sample_env(in.slot, reflect(-in.V, world_normal));
        color = vec4(mix(color.rgb, env, gloss * 0.25), color.a);
        let fresnel = pow(FRESNEL_R0 + (1.0 - FRESNEL_R0) * (1.0 - saturate(dot(in.V, world_normal))), 2.0);
        if (flags & GLOSS_FROM_BASE) == 0u {
            color.a = mix(color.a, 1.0, fresnel);
        }
    }

    var out: Surface;
    out.color = color;
    out.gloss = gloss;
    out.N = world_normal;
    return out;
}

fn material_slot(instance_index: u32) -> u32 {
    return mesh[instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
}

#ifdef PREPASS_PIPELINE

fn standard_material_flags(slot: u32) -> u32 {
#ifdef BINDLESS
    return pbr_bindings::material_array[material_indices[slot].material].flags;
#else
    return pbr_bindings::material.flags;
#endif
}

// Base color factor x texture, for the alpha test.
fn prepass_base_color(slot: u32, uv: vec2<f32>) -> vec4<f32> {
#ifdef BINDLESS
    let material = pbr_bindings::material_array[material_indices[slot].material];
#else
    let material = pbr_bindings::material;
#endif
    var color = material.base_color;
    let uv_t = (material.uv_transform * vec3(uv, 1.0)).xy;
    if (material.flags & pbr_types::STANDARD_MATERIAL_FLAGS_BASE_COLOR_TEXTURE_BIT) != 0u {
#ifdef BINDLESS
        color *= textureSampleBias(
            bindless_textures_2d[material_indices[slot].base_color_texture],
            bindless_samplers_filtering[material_indices[slot].base_color_sampler],
            uv_t,
            view.mip_bias,
        );
#else
        color *= textureSampleBias(pbr_bindings::base_color_texture, pbr_bindings::base_color_sampler, uv_t, view.mip_bias);
#endif
    }
    return color;
}

fn prepass_alpha_cutoff(slot: u32) -> f32 {
#ifdef BINDLESS
    return pbr_bindings::material_array[material_indices[slot].material].alpha_cutoff;
#else
    return pbr_bindings::material.alpha_cutoff;
#endif
}

#ifdef PREPASS_FRAGMENT
@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
#else
@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) {
#endif
    let slot = material_slot(in.instance_index);
    let layers = layers_of(slot);
    let flags = standard_material_flags(slot);

    var surface_in: SurfaceInput;
    surface_in.slot = slot;
#ifdef VERTEX_UVS_A
    surface_in.uv = in.uv;
#endif
#ifdef VERTEX_UVS_B
    surface_in.uv_b = in.uv_b;
#ifdef VERTEX_UVS_A
    if (layers.flags & UV_SUM) != 0u {
        surface_in.uv += in.uv_b;
    }
#endif
#endif
#ifdef VERTEX_COLORS
    surface_in.packed = in.color;
#endif
    surface_in.world_normal = vec3(0.0, 1.0, 0.0);
    surface_in.world_tangent = vec4(1.0, 0.0, 0.0, 1.0);
#ifdef NORMAL_PREPASS_OR_DEFERRED_PREPASS
    surface_in.world_normal = in.world_normal;
#ifdef VERTEX_TANGENTS
    surface_in.world_tangent = in.world_tangent;
    surface_in.has_tangents = true;
#endif
#endif
    surface_in.V = pbr_functions::calculate_view(in.world_position, view.clip_from_view[3].w == 1.0);
    surface_in.front_facing = is_front;
    surface_in.double_sided = (flags & pbr_types::STANDARD_MATERIAL_FLAGS_DOUBLE_SIDED_BIT) != 0u;
    surface_in.base = prepass_base_color(slot, surface_in.uv);
    let surface = bf2_surface(surface_in, layers);

#ifdef MAY_DISCARD
    let alpha_mode = flags & pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_RESERVED_BITS;
    if alpha_mode == pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_MASK {
        if surface.color.a < prepass_alpha_cutoff(slot) {
            discard;
        }
    } else if alpha_mode != pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_OPAQUE && surface.color.a < 0.05 {
        discard;
    }
#endif

#ifdef PREPASS_FRAGMENT
    var out: FragmentOutput;
#ifdef UNCLIPPED_DEPTH_ORTHO_EMULATION
    out.frag_depth = in.unclipped_depth;
#endif
#ifdef NORMAL_PREPASS
    out.normal = vec4(surface.N * 0.5 + vec3(0.5), 1.0);
#endif
#ifdef MOTION_VECTOR_PREPASS
    out.motion_vector = pbr_prepass_functions::calculate_motion_vector(in.world_position, in.previous_world_position);
#endif
    return out;
#endif
}

#else  // PREPASS_PIPELINE

@fragment
fn fragment(vertex: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var in = vertex;
    let slot = material_slot(in.instance_index);
    let layers = layers_of(slot);

    var surface_in: SurfaceInput;
    surface_in.slot = slot;
#ifdef VERTEX_COLORS
    surface_in.packed = in.color;
    // COLOR_0 carries layer UVs or a vertex frame, not a tint.
    in.color = vec4(1.0);
#endif
#ifdef VERTEX_UVS_B
    surface_in.uv_b = in.uv_b;
#ifdef VERTEX_UVS_A
    if (layers.flags & UV_SUM) != 0u {
        in.uv += in.uv_b;
    }
#endif
#endif
#ifdef VERTEX_UVS_A
    surface_in.uv = in.uv;
#endif

    var pbr_input = pbr_input_from_standard_material(in, is_front);

    surface_in.world_normal = in.world_normal;
    surface_in.world_tangent = vec4(1.0, 0.0, 0.0, 1.0);
#ifdef VERTEX_TANGENTS
    surface_in.world_tangent = in.world_tangent;
    surface_in.has_tangents = true;
#endif
#ifdef LOAD_PREPASS_NORMALS
    surface_in.prepass_normal = pbr_input.N;
    surface_in.has_prepass_normal = true;
#endif
    surface_in.V = pbr_input.V;
    surface_in.front_facing = is_front;
    surface_in.double_sided =
        (pbr_input.material.flags & pbr_types::STANDARD_MATERIAL_FLAGS_DOUBLE_SIDED_BIT) != 0u;
    surface_in.base = pbr_input.material.base_color;
    let surface = bf2_surface(surface_in, layers);

    pbr_input.material.base_color = surface.color;
    pbr_input.material.metallic = 0.0;
    pbr_input.material.perceptual_roughness = GLOSS_ROUGHNESS;
    pbr_input.material.reflectance = vec3(saturate(surface.gloss));
#ifndef LOAD_PREPASS_NORMALS
    pbr_input.N = surface.N;
    pbr_input.clearcoat_N = surface.N;
#endif

    pbr_input.material.base_color = pbr_functions::alpha_discard(pbr_input.material, pbr_input.material.base_color);
#ifdef BF2_DEBUG_LIGHTING
    pbr_input.material.base_color = vec4(vec3(0.5), pbr_input.material.base_color.a);
#endif

    var out: FragmentOutput;
    out.color = pbr_functions::apply_pbr_lighting(pbr_input);
    out.color = pbr_functions::main_pass_post_lighting_processing(pbr_input, out.color);
#ifdef BF2_DEBUG_NORMALS
    out.color = vec4(pbr_input.N * 0.5 + vec3(0.5), 1.0);
#endif
#ifdef BF2_DEBUG_GLOSS
    out.color = vec4(vec3(surface.gloss), 1.0);
#endif
#ifdef BF2_DEBUG_ENV
    // The environment map everywhere (black where a material has none).
    out.color = vec4(sample_env(slot, reflect(-pbr_input.V, pbr_input.N)), 1.0);
#endif
    return out;
}

#endif  // PREPASS_PIPELINE
