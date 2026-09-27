// Water surface: animated normal map waves, Fresnel reflection of the level's environment
// cube map, sun glint, and depth-dependent opacity with soft shorelines.
//
// Depth comes from a map of the water depth over the terrain (always available) and, when
// the camera has a depth prepass, from the scene depth behind the surface (objects in the
// water get soft edges too). Output is premultiplied: the surface colour plus how much of
// the scene below still shows through.

#import bevy_pbr::{
    mesh_functions,
    mesh_types::MESH_FLAGS_SHADOW_RECEIVER_BIT,
    mesh_view_bindings::{globals, lights, view},
    mesh_view_types::DIRECTIONAL_LIGHT_FLAGS_SHADOWS_ENABLED_BIT,
    pbr_functions::{apply_pbr_lighting, calculate_view},
    pbr_types::pbr_input_new,
    shadows::fetch_directional_shadow,
    view_transformations::{position_world_to_clip, position_world_to_view, depth_ndc_to_view_z},
}
#ifdef DISTANCE_FOG
#import bevy_pbr::{mesh_view_bindings::fog, pbr_functions::apply_fog}
#endif
#ifdef DEPTH_PREPASS
#import bevy_pbr::prepass_utils::prepass_depth
#endif

struct WaterParams {
    // rgb: water colour, a: opacity of the shallowest water.
    color: vec4<f32>,
    // rgb: sun glint colour, a: strength.
    specular: vec4<f32>,
    // xy: wave drift (m/s), z: normal map slices per second, w: glint exponent.
    waves: vec4<f32>,
    // x: opaque depth (m), y: 1 with a normal map, z: 1 with a reflection map, w: 1 with a depth map.
    flags: vec4<f32>,
    // xy: world XZ of the depth map's corner, zw: its size.
    depth_rect: vec4<f32>,
    // Reflected without a reflection map.
    sky_color: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> water: WaterParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var normal_map: texture_3d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var normal_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var reflection_map: texture_cube<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var reflection_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(5) var depth_map: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(6) var depth_sampler: sampler;

// BF2 tiles its water normal map every ~30 m.
const TILE: vec2<f32> = vec2(29.13, 31.81);

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
}

@vertex
fn vertex(in: Vertex) -> VertexOutput {
    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    let world = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(in.position, 1.0)).xyz;
    var out: VertexOutput;
    out.position = position_world_to_clip(world);
    out.world_position = world;
    return out;
}

// Wave normal (+Y up) at a world position.
fn wave_normal(xz: vec2<f32>, t: f32, distance: f32) -> vec3<f32> {
    var n: vec3<f32>;
    if water.flags.y > 0.5 {
        let drift = water.waves.xy * t;
        let z = t * water.waves.z / 8.0;
        // Two layers at different scales and directions hide the tiling.
        let a = textureSample(normal_map, normal_sampler, vec3((xz + drift) / TILE, z)).xyz * 2.0 - 1.0;
        let b = textureSample(normal_map, normal_sampler, vec3((xz * 0.37 - drift.yx * 0.6) / TILE + 0.5, z * 0.7 + 0.31)).xyz * 2.0 - 1.0;
        // BF2 stores world-space normals in its left-handed space: mirror Z.
        n = vec3(a.x + b.x * 0.6, a.y + b.y * 0.6, -(a.z + b.z * 0.6));
    } else {
        // Procedural ripples: a few travelling sine waves.
        let p = xz;
        let d1 = vec2(0.8, 0.6);
        let d2 = vec2(-0.5, 0.86);
        let d3 = vec2(0.2, -0.98);
        let g = d1 * cos(dot(p, d1) * 0.9 + t * 1.1) * 0.12
            + d2 * cos(dot(p, d2) * 1.7 + t * 1.7) * 0.08
            + d3 * cos(dot(p, d3) * 3.1 + t * 2.3) * 0.05;
        n = vec3(-g.x, 1.0, -g.y);
    }
    // Calm the waves with distance: fine ripples alias and look noisy far away.
    let calm = smoothstep(40.0, 400.0, distance);
    n = normalize(mix(n, vec3(0.0, 1.0, 0.0) * length(n), calm * 0.8));
    return n;
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    let world = in.world_position;
    let to_eye = view.world_position.xyz - world;
    let distance = length(to_eye);
    let V = to_eye / distance;
    let from_below = !front || view.world_position.y < world.y;
    let t = globals.time;

    var n = wave_normal(world.xz, t, distance);
    if from_below {
        n = -n;
    }
    let NdotV = max(dot(n, V), 0.0);

    // Reflection: the level's environment cube (BF2's left-handed space: mirror Z).
    var R = reflect(-V, n);
    R.y = abs(R.y);
    var reflected = water.sky_color.rgb;
    if water.flags.z > 0.5 {
        reflected = textureSample(reflection_map, reflection_sampler, vec3(R.x, R.y, -R.z)).rgb;
    }
    let fresnel = 0.02 + 0.98 * pow(1.0 - NdotV, 5.0);

    // How much water the view ray crosses below the surface.
    var vertical_depth = 1000.0;
    if water.flags.w > 0.5 {
        let uv = (world.xz - water.depth_rect.xy) / water.depth_rect.zw;
        let encoded = textureSample(depth_map, depth_sampler, uv).r;
        vertical_depth = encoded * encoded * 64.0;
        if any(uv < vec2(0.0)) || any(uv > vec2(1.0)) {
            vertical_depth = 1000.0;
        }
    }
    let flat_NdotV = max(abs(V.y), 0.05);
    var ray_depth = vertical_depth / flat_NdotV;
#ifdef DEPTH_PREPASS
    if !from_below {
        let scene_z = depth_ndc_to_view_z(prepass_depth(in.position, 0u));
        let surface_z = position_world_to_view(world).z;
        // View space looks down -Z: the scene behind the surface has a smaller z. Divide by
        // the cosine between the ray and the view axis to get the distance along the ray.
        let axial = max(surface_z - scene_z, 0.0);
        ray_depth = min(ray_depth, axial / max(dot(V, view.world_from_view[2].xyz), 0.05));
    }
#endif
    let opaque_depth = max(water.flags.x, 0.1);
    // Even shallow water is murky (BF2 adds a base opacity); deep water hides the ground.
    let body_alpha = 1.0 - (1.0 - water.color.a) * exp(-3.0 * ray_depth / opaque_depth);
    // Soft shoreline: the surface itself fades out in the last half meter.
    let shore = smoothstep(0.0, 0.5, min(vertical_depth, ray_depth * flat_NdotV));

    // The water body, lit like any surface (dark at night, darker in shadow).
    var pbr_input = pbr_input_new();
    pbr_input.material.base_color = vec4(water.color.rgb, 1.0);
    pbr_input.material.perceptual_roughness = 1.0;
    pbr_input.material.reflectance = vec3(0.0);
    pbr_input.flags = MESH_FLAGS_SHADOW_RECEIVER_BIT;
    pbr_input.frag_coord = in.position;
    pbr_input.world_position = vec4(world, 1.0);
    pbr_input.world_normal = vec3(0.0, 1.0, 0.0);
    pbr_input.N = vec3(0.0, 1.0, 0.0);
    pbr_input.V = calculate_view(pbr_input.world_position, false);
    let body = apply_pbr_lighting(pbr_input).rgb;

    // Sun glint, gone in shadow.
    var glint = vec3(0.0);
    if lights.n_directional_lights > 0u && !from_below {
        let sun = lights.directional_lights[0];
        var visibility = 1.0;
        if (sun.flags & DIRECTIONAL_LIGHT_FLAGS_SHADOWS_ENABLED_BIT) != 0u {
            let view_z = position_world_to_view(world).z;
            visibility = fetch_directional_shadow(0u, vec4(world, 1.0), vec3(0.0, 1.0, 0.0), view_z, in.position.xy);
        }
        let highlight = pow(max(dot(R, sun.direction_to_light), 0.0), water.waves.w);
        glint = sun.color.rgb * view.exposure * water.specular.rgb * water.specular.a * highlight * visibility * 0.25;
    }

    var reflection = fresnel;
    if from_below {
        reflection = 0.0;
    }
    // Premultiplied: surface = body where the water is deep, reflection by Fresnel on top.
    let transmitted = (1.0 - body_alpha) * (1.0 - reflection);
    var color = body * body_alpha * (1.0 - reflection) + reflected * reflection + glint;
    var alpha = 1.0 - transmitted;

#ifdef DISTANCE_FOG
    // Fog the surface part (what shows through is already fogged at about this distance).
    if alpha > 0.001 {
        let fogged = apply_fog(fog, vec4(color / alpha, 1.0), world, view.world_position.xyz, in.position.xy);
        color = fogged.rgb * alpha;
    }
#endif
    return vec4(color, alpha) * shore;
}
