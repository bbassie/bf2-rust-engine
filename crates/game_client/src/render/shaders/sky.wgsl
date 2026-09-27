// Sky dome: the level's sky texture, drawn at the far plane so it never hides anything.
//
// Below the horizon it turns into the fog colour (including the fog's sun glow), exactly
// what distant terrain and water fade to, so the world meets the sky without a seam from
// the ground or from the air.

#import bevy_pbr::{
    mesh_functions,
    mesh_view_bindings::view,
    pbr_functions::main_pass_post_lighting_processing,
    pbr_types::pbr_input_new,
    view_transformations::position_world_to_clip,
}
#ifdef DISTANCE_FOG
#import bevy_pbr::{mesh_view_bindings::fog, pbr_functions::apply_fog}
#endif

struct SkyParams {
    // x: height of the view direction (sine of the elevation) above which the sky shows
    // unhazed.
    haze: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var sky_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var sky_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> params: SkyParams;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) uv: vec2<f32>,
}

@vertex
fn vertex(in: Vertex) -> VertexOutput {
    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    let world = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(in.position, 1.0)).xyz;
    var out: VertexOutput;
    out.position = position_world_to_clip(world);
    // Depth 0 is the far plane (reverse Z): only where nothing else is drawn.
    out.position.z = 0.0;
    out.world_position = world;
    out.uv = in.uv;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let sky = textureSample(sky_texture, sky_sampler, in.uv).rgb;
    let direction = normalize(in.world_position - view.world_position.xyz);
    let clear = smoothstep(0.0, params.haze.x, direction.y);
#ifdef DISTANCE_FOG
    // Infinitely far away: the full fog colour with its sun glow in this direction.
    let far = view.world_position.xyz + direction * 1.0e6;
    let fogged = apply_fog(fog, vec4(sky, 1.0), far, view.world_position.xyz, in.position.xy).rgb;
#else
    let fogged = sky;
#endif

    var pbr_input = pbr_input_new();
    pbr_input.material.flags = 0u;
    pbr_input.frag_coord = in.position;
    pbr_input.world_position = vec4(in.world_position, 1.0);
    return main_pass_post_lighting_processing(pbr_input, vec4(mix(fogged, sky, clear), 1.0));
}
