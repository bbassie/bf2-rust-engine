// Undergrowth: grass and small plants baked into per-chunk meshes around the camera.
//
// The vertex shader sways plants in the wind, thins them out with distance and sinks them
// into the ground towards the view distance. The fragment shader cuts out the atlas alpha,
// tints the plant with the terrain colour map it grows on and lights it like the ground.
// Not in the prepass and casts no shadows.

#import bevy_pbr::{
    mesh_functions,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
    mesh_view_bindings::{globals, view},
    pbr_functions::{apply_pbr_lighting, calculate_view, main_pass_post_lighting_processing},
    pbr_types::{pbr_input_new, STANDARD_MATERIAL_FLAGS_FOG_ENABLED_BIT},
    view_transformations::position_world_to_clip,
}
#ifdef SCREEN_SPACE_AMBIENT_OCCLUSION
#import bevy_pbr::mesh_view_bindings::screen_space_ambient_occlusion_texture
#endif

struct UndergrowthParams {
    // xy: world XZ of the colour map's corner, zw: its size in meters.
    ground_rect: vec4<f32>,
    // View distance, distance where fading starts, sway amplitude (m), brightness.
    distances: vec4<f32>,
    // xy: wind direction (XZ), z: alpha cutoff, w: fraction of plants kept at the fade start.
    wind: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: UndergrowthParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var atlas: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var atlas_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var ground: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var ground_sampler: sampler;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // x: sway weight, y: height above the plant's root (m).
    @location(3) sway_height: vec2<f32>,
    // x: ground tint, y: random 0..1 per plant (thinning), z: brightness variation.
    @location(4) plant: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tint: vec2<f32>,
}

@vertex
fn vertex(in: Vertex) -> VertexOutput {
    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    var world = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(in.position, 1.0)).xyz;

    // Plants fade by shrinking into the ground; far ones are thinned out first.
    let view_distance = params.distances.x;
    let fade_start = params.distances.y;
    let distance = length(world.xz - view.world_position.xz);
    var size = 1.0 - smoothstep(fade_start, view_distance, distance);
    let keep = mix(1.0, params.wind.w, smoothstep(fade_start * 0.4, fade_start, distance));
    size *= saturate((keep - in.plant.y) * 8.0);
    let height = in.sway_height.y;
    world.y -= (height + 0.08) * (1.0 - size);

    // Wind: slow gusts rolling over the field plus a faster flutter.
    let t = globals.time;
    let wind = params.wind.xy;
    let phase = dot(world.xz, wind) * 0.35;
    let gust = sin(t * 1.3 - phase) * 0.6 + sin(t * 2.9 - phase * 1.7 + world.x * 0.5) * 0.25 + 0.35;
    let flutter = sin(t * 7.0 + world.x * 2.1 + world.z * 1.7) * 0.15;
    let sway = in.sway_height.x * params.distances.z * size;
    world += vec3(wind.x, 0.0, wind.y) * sway * (gust + flutter);
    world.y -= abs(gust) * sway * 0.2;

    var out: VertexOutput;
    out.position = position_world_to_clip(world);
    out.world_position = world;
    out.world_normal = mesh_functions::mesh_normal_local_to_world(in.normal, in.instance_index);
    out.uv = in.uv;
    out.tint = in.plant.xz;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(atlas, atlas_sampler, in.uv);
    if texel.a < params.wind.z {
        discard;
    }
    // BF2 multiplies the (mostly grey) plant textures with the terrain colour map.
    let ground_uv = (in.world_position.xz - params.ground_rect.xy) / params.ground_rect.zw;
    let ground_color = textureSample(ground, ground_sampler, ground_uv).rgb;
    let tint = mix(vec3(1.0), ground_color, in.tint.x);
    let albedo = min(texel.rgb * tint * params.distances.w * in.tint.y, vec3(1.0));

    var pbr_input = pbr_input_new();
    pbr_input.material.base_color = vec4(albedo, 1.0);
    pbr_input.material.perceptual_roughness = 0.9;
    pbr_input.material.reflectance = vec3(0.2);
    pbr_input.material.flags = STANDARD_MATERIAL_FLAGS_FOG_ENABLED_BIT;
    pbr_input.flags = MESH_FLAGS_SHADOW_RECEIVER_BIT;
    pbr_input.frag_coord = in.position;
    pbr_input.world_position = vec4(in.world_position, 1.0);
    let normal = normalize(in.world_normal);
    pbr_input.world_normal = normal;
    pbr_input.N = normal;
    pbr_input.V = calculate_view(pbr_input.world_position, false);
#ifdef SCREEN_SPACE_AMBIENT_OCCLUSION
    let ao = textureLoad(screen_space_ambient_occlusion_texture, vec2<i32>(in.position.xy), 0).r;
    pbr_input.diffuse_occlusion = vec3(ao);
#endif

    var color = apply_pbr_lighting(pbr_input);
    color = main_pass_post_lighting_processing(pbr_input, color);
    return color;
}
