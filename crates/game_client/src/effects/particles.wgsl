// Sprite particles expanded from a storage buffer: every quad of the chunk mesh names its
// particle (position.x) and corner (position.yz), the vertex shader places the corner.
// Colors are premultiplied, with alpha 0 for additive sprites, so both blend in one draw.

#import bevy_pbr::mesh_view_bindings::view
#import bevy_pbr::view_transformations::{depth_ndc_to_view_z, position_world_to_view}
#ifdef DEPTH_PREPASS
#import bevy_pbr::prepass_utils::prepass_depth
#endif
#ifdef DISTANCE_FOG
#import bevy_pbr::{mesh_view_bindings::fog, mesh_view_types::{FOG_MODE_LINEAR, FOG_MODE_EXPONENTIAL, FOG_MODE_EXPONENTIAL_SQUARED}}
#endif
#ifdef TONEMAP_IN_SHADER
#import bevy_core_pipeline::tonemapping::tone_mapping
#endif

struct Particle {
    position: vec3<f32>,
    // Half the width.
    size: f32,
    // Velocity mode: the direction to stretch along. Horizontal mode: the plane's normal.
    axis: vec3<f32>,
    rotation: f32,
    // Straight (not premultiplied) linear color, may exceed 1; alpha is the opacity.
    color: vec4<f32>,
    // Offset and size in the texture.
    uv: vec4<f32>,
    // x: mode (0 camera, 1 velocity, 2 horizontal), y: length / width, z: additive,
    // w: soft fade distance in meters.
    params: vec4<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> particles: array<Particle>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var sprite_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var sprite_sampler: sampler;

struct Vertex {
    @location(0) position: vec3<f32>,
};

struct Varyings {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world_position: vec3<f32>,
    @location(3) @interpolate(flat) params: vec4<f32>,
};

fn perpendicular(n: vec3<f32>) -> vec3<f32> {
    if abs(n.y) < 0.9 {
        return normalize(cross(n, vec3(0.0, 1.0, 0.0)));
    }
    return normalize(cross(n, vec3(1.0, 0.0, 0.0)));
}

@vertex
fn vertex(in: Vertex) -> Varyings {
    var out: Varyings;
    let index = u32(in.position.x);
    if index >= arrayLength(&particles) {
        out.clip = vec4(2.0, 2.0, 2.0, 1.0);
        return out;
    }
    let p = particles[index];
    let corner = in.position.yz * 2.0 - 1.0;
    let c = cos(p.rotation);
    let s = sin(p.rotation);
    let turned = vec2(corner.x * c - corner.y * s, corner.x * s + corner.y * c);
    var offset: vec3<f32>;
    if p.params.x < 0.5 {
        let right = view.world_from_view[0].xyz;
        let up = view.world_from_view[1].xyz;
        offset = (right * turned.x + up * turned.y) * p.size;
    } else if p.params.x < 1.5 {
        let to_camera = view.world_position - p.position;
        var side = cross(p.axis, to_camera);
        if dot(side, side) < 1e-8 {
            side = perpendicular(p.axis);
        }
        side = normalize(side);
        offset = (p.axis * corner.y * p.params.y + side * corner.x) * p.size;
    } else {
        let tangent = perpendicular(p.axis);
        let bitangent = cross(p.axis, tangent);
        offset = (tangent * turned.x + bitangent * turned.y) * p.size;
    }
    let world = p.position + offset;
    out.clip = view.clip_from_world * vec4(world, 1.0);
#ifdef VIEW_MODEL
    // First-person effects: in front of the world like the view model (VIEW_MODEL_DEPTH in
    // bf2_material.wgsl).
    out.clip.z = 0.5 * out.clip.w + 0.5 * out.clip.z;
#endif
    out.uv = p.uv.xy + vec2(in.position.y, 1.0 - in.position.z) * p.uv.zw;
    out.color = p.color;
    out.world_position = world;
    out.params = p.params;
    return out;
}

#ifdef DISTANCE_FOG
fn fog_amount(distance: f32) -> f32 {
    var amount = 0.0;
    if fog.mode == FOG_MODE_LINEAR {
        amount = 1.0 - clamp((fog.be.y - distance) / (fog.be.y - fog.be.x), 0.0, 1.0);
    } else if fog.mode == FOG_MODE_EXPONENTIAL {
        amount = 1.0 - 1.0 / exp(distance * fog.be.x);
    } else if fog.mode == FOG_MODE_EXPONENTIAL_SQUARED {
        let x = distance * fog.be.x;
        amount = 1.0 - 1.0 / exp(x * x);
    }
    return amount * fog.base_color.a;
}
#endif

@fragment
fn fragment(in: Varyings) -> @location(0) vec4<f32> {
    let texel = textureSample(sprite_texture, sprite_sampler, in.uv);
    var opacity = in.color.a * texel.a;
#ifdef DEPTH_PREPASS
    // Fade out where the sprite cuts into geometry instead of showing a hard edge.
    let softness = in.params.w;
    if softness > 0.0 {
        let scene_z = depth_ndc_to_view_z(prepass_depth(in.clip, 0u));
        let sprite_z = position_world_to_view(in.world_position).z;
        opacity *= clamp((sprite_z - scene_z) / softness, 0.0, 1.0);
    }
#endif
    if opacity <= 0.002 {
        discard;
    }
    var rgb = in.color.rgb * texel.rgb;
    let additive = in.params.z;
#ifdef DISTANCE_FOG
    let amount = fog_amount(length(in.world_position - view.world_position));
    // Light fades with the fog instead of taking on its color.
    rgb = select(mix(rgb, fog.base_color.rgb, amount), rgb * (1.0 - amount), additive > 0.5);
#endif
#ifdef TONEMAP_IN_SHADER
    rgb = tone_mapping(vec4(rgb, 1.0), view.color_grading).rgb;
#endif
    return vec4(rgb * opacity, opacity * (1.0 - additive));
}
