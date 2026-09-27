// Tree stand-ins: BF2's `_lod` tree meshes merged per cell, drawn past the hand-over
// distance. Each vertex carries its tree's root (vertex colour), so a tree fades in as a
// whole, with Bevy's dither pattern for appearing meshes: exactly the pixels the detailed
// tree's visibility range drops.

#import bevy_pbr::{
    mesh_functions,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
    mesh_view_bindings::view,
    pbr_functions::{apply_pbr_lighting, calculate_view, main_pass_post_lighting_processing},
    pbr_types::{pbr_input_new, STANDARD_MATERIAL_FLAGS_FOG_ENABLED_BIT},
    view_transformations::position_world_to_clip,
}

struct FarTreeParams {
    // x: hand-over start, y: end (m), z: alpha cutoff.
    fade: vec4<f32>,
    // Lit like the trees: xyz scale the albedo (sunlight) and the diffuse occlusion (ambient).
    light_sun: vec4<f32>,
    light_ambient: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: FarTreeParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var atlas: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var atlas_sampler: sampler;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // xyz: the tree's root.
    @location(3) root: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) @interpolate(flat) dither: i32,
}

// Bevy's 4x4 ordered dither thresholds (`pbr_functions::visibility_range_dither`).
const DITHER_THRESHOLD_MAP: vec4<u32> = vec4(0x0a020800u, 0x060e040cu, 0x09010b03u, 0x050d070fu);

@vertex
fn vertex(in: Vertex) -> VertexOutput {
    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    let world = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(in.position, 1.0)).xyz;
    let root = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(in.root.xyz, 1.0)).xyz;

    // -16: hidden, -16 < level < 0: fading in, 0: visible (as Bevy counts for appearing LODs).
    let distance = length(view.world_position.xyz - root);
    let level = -16 + clamp(i32(round((distance - params.fade.x) / (params.fade.y - params.fade.x) * 16.0)), 0, 16);

    var out: VertexOutput;
    out.position = position_world_to_clip(world);
    if level <= -16 {
        // Collapse the whole tree to a point: no fragments at all.
        out.position = vec4(0.0, 0.0, 0.0, 1.0);
    }
    out.world_position = world;
    out.world_normal = mesh_functions::mesh_normal_local_to_world(in.normal, in.instance_index);
    out.uv = in.uv;
    out.dither = level;
    return out;
}

fn to_gamma(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3(0.0)), vec3(1.0 / 2.2));
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.dither < 0 {
        let coords = vec2<u32>(floor(in.position.xy)) % 4u;
        let threshold = i32((DITHER_THRESHOLD_MAP[coords.y] >> (coords.x * 8u)) & 0xffu);
        if 1 + in.dither + threshold <= 0 {
            discard;
        }
    }
    let texel = textureSample(atlas, atlas_sampler, in.uv);
    if texel.a < params.fade.z {
        discard;
    }

    var pbr_input = pbr_input_new();
    // BF2 doubles tree textures in gamma space like the detailed trees, but lit from above
    // the planes catch more light than a tree's own leaves: 1.5 matches their brightness.
    pbr_input.material.base_color = vec4(pow(min(to_gamma(texel.rgb) * 1.5, vec3(1.0)), vec3(2.2)) * params.light_sun.rgb, 1.0);
    pbr_input.diffuse_occlusion *= params.light_ambient.rgb;
    pbr_input.material.perceptual_roughness = 0.9;
    pbr_input.material.reflectance = vec3(0.1);
    pbr_input.material.flags = STANDARD_MATERIAL_FLAGS_FOG_ENABLED_BIT;
    pbr_input.flags = MESH_FLAGS_SHADOW_RECEIVER_BIT;
    pbr_input.frag_coord = in.position;
    pbr_input.world_position = vec4(in.world_position, 1.0);
    // The planes' own normals would light each plane differently; a canopy is lit mostly
    // from above.
    let n = normalize(in.world_normal);
    let normal = normalize(vec3(n.x * 0.35, 1.0, n.z * 0.35));
    pbr_input.world_normal = normal;
    pbr_input.N = normal;
    pbr_input.V = calculate_view(pbr_input.world_position, false);

    var color = apply_pbr_lighting(pbr_input);
    return main_pass_post_lighting_processing(pbr_input, color);
}
