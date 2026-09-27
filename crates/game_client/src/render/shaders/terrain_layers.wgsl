// BF2 terrain on top of Bevy's PBR: the patch color map (UV0, the StandardMaterial base
// color) modulated by six tiling detail textures weighted per patch.
//
// BF2 adds `weight_i * 2 * detail_i * colormap` per detail texture; detail textures are
// authored around 0.5 grey. Far away the detail fades out to the plain color map.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    mesh_view_bindings::view,
}
#ifdef VISIBILITY_RANGE_DITHER
#import bevy_pbr::pbr_functions::visibility_range_dither
#endif

struct TerrainLayers {
    // Meters per repeat, top projection, for detail textures 0..3 and 4..5.
    tile_a: vec4<f32>,
    tile_b: vec4<f32>,
    // Side projection tile size (U, V) of detail texture 0; fade start/end distance.
    side0_fade: vec4<f32>,
    // Bit 0: detail texture 0 is tri-planar. Bit 1: detail weights present.
    flags: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
    // The terrain's own light relative to the level's sun and ambient (BF2 lit terrain with
    // other colours than static objects): xyz scale the albedo, so sunlight, and the diffuse
    // occlusion, so ambient light relative to sunlight.
    light_sun: vec4<f32>,
    light_ambient: vec4<f32>,
}

const TRI_PLANAR_0: u32 = 1u;
const HAS_WEIGHTS: u32 = 2u;

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> layers: TerrainLayers;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var weights_a: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var weights_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var weights_b: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var detail_0: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var detail_1: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(106) var detail_2: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(107) var detail_3: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(108) var detail_4: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(109) var detail_5: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(110) var detail_sampler: sampler;

fn top(t: texture_2d<f32>, xz: vec2<f32>, tile: f32) -> vec3<f32> {
    return textureSample(t, detail_sampler, xz / max(tile, 0.01)).rgb;
}

fn gamma(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3(0.0)), vec3(1.0 / 2.2));
}

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
#ifdef VISIBILITY_RANGE_DITHER
    // Cross-fade between terrain levels of detail.
    visibility_range_dither(in.position, in.visibility_range_dither);
#endif
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    let world = in.world_position.xyz;
    let normal = normalize(in.world_normal);

#ifdef VERTEX_UVS_A
    // Weight of detail texture i is channel B, G, R (i % 3) of map i / 3.
    let wa = textureSample(weights_a, weights_sampler, in.uv).rgb;
    let wb = textureSample(weights_b, weights_sampler, in.uv).rgb;
#else
    let wa = vec3(0.0);
    let wb = vec3(0.0);
#endif

    // Detail texture 0 is usually the cliff rock: project it from three sides on slopes.
    let rock_top = top(detail_0, world.xz, layers.tile_a.x);
    let rock_x = textureSample(detail_0, detail_sampler, world.zy / layers.side0_fade.xy).rgb;
    let rock_z = textureSample(detail_0, detail_sampler, world.xy / layers.side0_fade.xy).rgb;
    var blend = max(abs(normal) - vec3(0.2), vec3(0.0));
    blend = blend / max(blend.x + blend.y + blend.z, 0.0001);
    let rock_tri = rock_x * blend.x + rock_top * blend.y + rock_z * blend.z;
    let rock = select(rock_top, rock_tri, (layers.flags & TRI_PLANAR_0) != 0u);

    let d1 = top(detail_1, world.xz, layers.tile_a.y);
    let d2 = top(detail_2, world.xz, layers.tile_a.z);
    let d3 = top(detail_3, world.xz, layers.tile_a.w);
    let d4 = top(detail_4, world.xz, layers.tile_b.x);
    let d5 = top(detail_5, world.xz, layers.tile_b.y);

    // BF2 blends and modulates in gamma space, where the detail textures average 0.5 grey;
    // the same `2 * detail` on linear values would darken the colour map about 2.3 times.
    let weights = array<f32, 6>(wa.b, wa.g, wa.r, wb.b, wb.g, wb.r);
    var detail = gamma(rock) * weights[0] + gamma(d1) * weights[1] + gamma(d2) * weights[2]
        + gamma(d3) * weights[3] + gamma(d4) * weights[4] + gamma(d5) * weights[5];
    // Where the weights don't add up to 1, fill with neutral grey.
    let total = weights[0] + weights[1] + weights[2] + weights[3] + weights[4] + weights[5];
    detail += vec3(0.5) * max(1.0 - total, 0.0);

    let distance = length(world - view.world_position.xyz);
    let fade = smoothstep(layers.side0_fade.z, layers.side0_fade.w, distance);
    var factor = mix(detail * 2.0, vec3(1.0), fade);
    if (layers.flags & HAS_WEIGHTS) == 0u {
        factor = vec3(1.0);
    }

    // The colour map is linear, so the gamma-space factor is applied as pow(factor, 2.2).
    let base = pbr_input.material.base_color;
    pbr_input.material.base_color = vec4(min(base.rgb * pow(factor, vec3(2.2)), vec3(1.0)), base.a);
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);
    pbr_input.material.base_color = vec4(pbr_input.material.base_color.rgb * layers.light_sun.rgb, pbr_input.material.base_color.a);
    pbr_input.diffuse_occlusion *= layers.light_ambient.rgb;

    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
