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
// - Bundled and skinned meshes (soldiers, vehicles, props) whose `MeshTag` has SKY_TAG get its
//   low byte, their measured sky visibility (sky_occlusion.rs), as ambient occlusion.
// - Static meshes with lightmap UVs (`BF2_LIGHTMAP_UV`, their own vertex shader below) whose
//   `MeshTag` has LIGHTMAP_TAG read their baked sky visibility from the level's atlas array
//   (static_lightmaps.rs) as ambient occlusion, relative to what an unoccluded surface facing
//   the same way has (BF2 baked the surface's orientation in; the sky light here has it).
//
// One file for the main pass and the prepass (`PREPASS_PIPELINE`: alpha test, and the normals
// that SSAO and the main pass read), bindless (the normal case) or bound. Layer textures are
// sampled with explicit gradients because the flags that select them aren't uniform.

#import bevy_pbr::{
    mesh_bindings::mesh,
    mesh_functions,
    mesh_view_bindings::view,
    pbr_bindings,
    pbr_functions,
    pbr_types,
    view_transformations::position_world_to_clip,
}
#import bevy_render::bindless::{
    bindless_samplers_filtering, bindless_textures_2d, bindless_textures_2d_array, bindless_textures_cube,
}

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
    // Strength of the baked sky occlusion (0: off).
    baked_sky: f32,
    // This kind of surface's light relative to the level's (trees: BF2's tree colours):
    // xyz scale the albedo, so sunlight, and the diffuse occlusion, so ambient light.
    light_sun: vec4<f32>,
    light_ambient: vec4<f32>,
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
    lightmap: u32,       // 58
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
@group(#{MATERIAL_BIND_GROUP}) @binding(58) var lightmap_texture: texture_2d_array<f32>;
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
const DYNAMIC: u32 = 65536u;
const LIGHTMAPPED: u32 = 131072u;
// MeshTag bits (sky_occlusion.rs, static_lightmaps.rs).
const SKY_TAG: u32 = 0x80000000u;
const LIGHTMAP_TAG: u32 = 0x40000000u;
// Baked sky visibility of unoccluded static surfaces facing down and up, and the darkest
// occlusion (BakedSky in materials.rs).
const OPEN_SKY_DOWN: f32 = 0.1;
const OPEN_SKY_UP: f32 = 0.95;
const SKY_FLOOR: f32 = 0.12;

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

// Sky visibility in the static lightmap atlas array.
fn sample_lightmap(slot: u32, uv: vec2<f32>, layer: u32, ddx: vec2<f32>, ddy: vec2<f32>) -> f32 {
#ifdef BINDLESS
    let indices = bf2_indices[slot];
    return textureSampleGrad(
        bindless_textures_2d_array[indices.lightmap],
        bindless_samplers_filtering[indices.layer_sampler],
        uv,
        layer,
        ddx,
        ddy,
    ).r;
#else
    return textureSampleGrad(lightmap_texture, layer_sampler, uv, layer, ddx, ddy).r;
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
#ifdef VISIBILITY_RANGE_DITHER
    // Level-of-detail cross-fades (distant trees hand over to their stand-ins).
    pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
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

#ifdef BF2_LIGHTMAP_UV
// Static meshes with lightmap UVs: Bevy's vertex shader (without skinning and morphing, which
// statics don't have) passing the lightmap UVs on.
struct LightmapVertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
#ifdef VERTEX_NORMALS
    @location(1) normal: vec3<f32>,
#endif
#ifdef VERTEX_UVS_A
    @location(2) uv: vec2<f32>,
#endif
#ifdef VERTEX_UVS_B
    @location(3) uv_b: vec2<f32>,
#endif
#ifdef VERTEX_TANGENTS
    @location(4) tangent: vec4<f32>,
#endif
#ifdef VERTEX_COLORS
    @location(5) color: vec4<f32>,
#endif
    @location(8) lightmap_uv: vec2<f32>,
}

// `forward_io::VertexOutput` plus the lightmap UVs.
struct LightmapVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec4<f32>,
    @location(1) world_normal: vec3<f32>,
#ifdef VERTEX_UVS_A
    @location(2) uv: vec2<f32>,
#endif
#ifdef VERTEX_UVS_B
    @location(3) uv_b: vec2<f32>,
#endif
#ifdef VERTEX_TANGENTS
    @location(4) world_tangent: vec4<f32>,
#endif
#ifdef VERTEX_COLORS
    @location(5) color: vec4<f32>,
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    @location(6) @interpolate(flat) instance_index: u32,
#endif
#ifdef VISIBILITY_RANGE_DITHER
    @location(7) @interpolate(flat) visibility_range_dither: i32,
#endif
    @location(8) lightmap_uv: vec2<f32>,
}

@vertex
fn vertex(vertex: LightmapVertex) -> LightmapVertexOutput {
    var out: LightmapVertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
#ifdef VERTEX_NORMALS
    out.world_normal = mesh_functions::mesh_normal_local_to_world(vertex.normal, vertex.instance_index);
#endif
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(vertex.position, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
#ifdef VERTEX_UVS_A
    out.uv = vertex.uv;
#endif
#ifdef VERTEX_UVS_B
    out.uv_b = vertex.uv_b;
#endif
#ifdef VERTEX_TANGENTS
    out.world_tangent = mesh_functions::mesh_tangent_local_to_world(world_from_local, vertex.tangent, vertex.instance_index);
#endif
#ifdef VERTEX_COLORS
    out.color = vertex.color;
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    out.instance_index = vertex.instance_index;
#endif
#ifdef VISIBILITY_RANGE_DITHER
    out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(
        vertex.instance_index,
        world_from_local[3],
    );
#endif
    out.lightmap_uv = vertex.lightmap_uv;
    return out;
}

fn standard_vertex_output(lightmapped: LightmapVertexOutput) -> VertexOutput {
    var out: VertexOutput;
    out.position = lightmapped.position;
    out.world_position = lightmapped.world_position;
    out.world_normal = lightmapped.world_normal;
#ifdef VERTEX_UVS_A
    out.uv = lightmapped.uv;
#endif
#ifdef VERTEX_UVS_B
    out.uv_b = lightmapped.uv_b;
#endif
#ifdef VERTEX_TANGENTS
    out.world_tangent = lightmapped.world_tangent;
#endif
#ifdef VERTEX_COLORS
    out.color = lightmapped.color;
#endif
#ifdef VERTEX_OUTPUT_INSTANCE_INDEX
    out.instance_index = lightmapped.instance_index;
#endif
#ifdef VISIBILITY_RANGE_DITHER
    out.visibility_range_dither = lightmapped.visibility_range_dither;
#endif
    return out;
}
#endif  // BF2_LIGHTMAP_UV

@fragment
#ifdef BF2_LIGHTMAP_UV
fn fragment(lightmapped: LightmapVertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var in = standard_vertex_output(lightmapped);
    let lightmap_uv = clamp(lightmapped.lightmap_uv, vec2(0.0), vec2(1.0));
    let lightmap_dx = dpdx(lightmapped.lightmap_uv);
    let lightmap_dy = dpdy(lightmapped.lightmap_uv);
#else
fn fragment(vertex: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    var in = vertex;
#endif
#ifdef VISIBILITY_RANGE_DITHER
    pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
#endif
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
    pbr_input.material.base_color = vec4(pbr_input.material.base_color.rgb * layers.light_sun.rgb, pbr_input.material.base_color.a);
    pbr_input.diffuse_occlusion *= layers.light_ambient.rgb;
    let tag = mesh[in.instance_index].tag;
    // The baked and measured occlusion alone, for BF2_MATERIAL_DEBUG=sky.
    var sky_debug = vec3(1.0);
    if (layers.flags & DYNAMIC) != 0u && (tag & SKY_TAG) != 0u {
        let visibility = f32(tag & 0xffu) / 255.0;
        sky_debug = vec3(visibility, visibility, 1.0);
        pbr_input.diffuse_occlusion *= visibility;
        pbr_input.specular_occlusion *= visibility;
    }
#ifdef BF2_LIGHTMAP_UV
    if (layers.flags & LIGHTMAPPED) != 0u && (tag & (LIGHTMAP_TAG | SKY_TAG)) == LIGHTMAP_TAG && layers.baked_sky > 0.0 {
        let layer = (tag >> 24u) & 63u;
        let scale = vec2(exp2(-f32((tag >> 20u) & 15u)), exp2(-f32((tag >> 16u) & 15u)));
        let cell = vec2(f32((tag >> 8u) & 255u), f32(tag & 255u));
        let atlas_uv = (cell + lightmap_uv) * scale;
        let sky = sample_lightmap(slot, atlas_uv, layer, lightmap_dx * scale, lightmap_dy * scale);
        let facing = normalize(pbr_input.world_normal).y * 0.5 + 0.5;
        let open = mix(OPEN_SKY_DOWN, OPEN_SKY_UP, facing);
        let occlusion = mix(1.0, clamp(sky / open, SKY_FLOOR, 1.0), layers.baked_sky);
        pbr_input.diffuse_occlusion *= occlusion;
        pbr_input.specular_occlusion *= occlusion;
        sky_debug = vec3(occlusion);
#ifdef BF2_DEBUG_LIGHTMAP
        // Atlas position (red, green) and the baked sky visibility (blue).
        sky_debug = vec3(fract(atlas_uv * 8.0), sky);
#endif
    }
#else
    if (tag & (LIGHTMAP_TAG | SKY_TAG)) == LIGHTMAP_TAG {
        // A lightmap placement but no lightmap UVs in the mesh.
        sky_debug = vec3(1.0, 0.0, 1.0);
    }
#endif
#ifdef BF2_DEBUG_LIGHTING
    pbr_input.material.base_color = vec4(vec3(0.5), pbr_input.material.base_color.a);
#endif

    var out: FragmentOutput;
    out.color = pbr_functions::apply_pbr_lighting(pbr_input);
    out.color = pbr_functions::main_pass_post_lighting_processing(pbr_input, out.color);
#ifdef BF2_DEBUG_NORMALS
    out.color = vec4(pbr_input.N * 0.5 + vec3(0.5), 1.0);
#endif
#ifdef BF2_DEBUG_LIGHTMAP
    out.color = vec4(sky_debug, 1.0);
#endif
#ifdef BF2_DEBUG_SKY
    // White: open; grey: baked occlusion (statics); blue tint: measured (soldiers, vehicles);
    // magenta: lightmap placement without lightmap UVs.
    out.color = vec4(sky_debug, 1.0);
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
