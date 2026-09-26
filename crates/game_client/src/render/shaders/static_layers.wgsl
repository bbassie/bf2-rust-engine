// BF2 static mesh layering on top of Bevy's PBR: base (UV0) x detail (UV1).
//
// BF2 composes `base * detail` and applies a 2x factor in lighting; we fold that factor into
// the albedo so the result keeps BF2's brightness under physically based lighting.
//
// Works bindless (all static materials in one bind group, the normal case) and bound.

#import bevy_pbr::{
    forward_io::{FragmentOutput, VertexOutput},
    mesh_bindings::mesh,
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
}
#import bevy_render::bindless::{bindless_samplers_filtering, bindless_textures_2d}

struct StaticLayersIndices {
    material: u32,
    detail_texture: u32,
    detail_sampler: u32,
}

struct StaticLayers {
    flags: u32,
    detail_scale: f32,
    _pad: vec2<f32>,
}

const HAS_DETAIL: u32 = 1u;
const ALPHA_FROM_DETAIL: u32 = 2u;

#ifdef BINDLESS
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<storage> layers_indices: array<StaticLayersIndices>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<storage> layers_array: array<StaticLayers>;
#else
@group(#{MATERIAL_BIND_GROUP}) @binding(50) var<uniform> layers_bound: StaticLayers;
@group(#{MATERIAL_BIND_GROUP}) @binding(51) var detail_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(52) var detail_sampler: sampler;
#endif

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

#ifdef BINDLESS
    let slot = mesh[in.instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
    let layers = layers_array[layers_indices[slot].material];
#else
    let layers = layers_bound;
#endif

#ifdef VERTEX_UVS_B
#ifdef BINDLESS
    let detail = textureSample(
        bindless_textures_2d[layers_indices[slot].detail_texture],
        bindless_samplers_filtering[layers_indices[slot].detail_sampler],
        in.uv_b,
    );
#else
    let detail = textureSample(detail_texture, detail_sampler, in.uv_b);
#endif
    if (layers.flags & HAS_DETAIL) != 0u {
        let base = pbr_input.material.base_color;
        var alpha = base.a;
        if (layers.flags & ALPHA_FROM_DETAIL) != 0u {
            alpha = base.a * detail.a;
        }
        pbr_input.material.base_color = vec4(
            min(base.rgb * detail.rgb * layers.detail_scale, vec3(1.0)),
            alpha,
        );
    }
#endif

    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);

    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
