// The minimap: the level map around the player, turned so `rotation` (clockwise from north,
// radians) points up, `span` of the map's width across the node.

#import bevy_ui::ui_vertex_output::UiVertexOutput

struct MinimapParams {
    // xy: player position on the map (0..1), z: rotation, w: span.
    view: vec4<f32>,
    background: vec4<f32>,
}

@group(1) @binding(0) var<uniform> params: MinimapParams;
@group(1) @binding(1) var map_texture: texture_2d<f32>;
@group(1) @binding(2) var map_sampler: sampler;

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    // Node space around the centre, y down, like the map.
    let p = in.uv - vec2(0.5);
    let c = cos(params.view.z);
    let s = sin(params.view.z);
    let uv = params.view.xy + vec2(c * p.x - s * p.y, s * p.x + c * p.y) * params.view.w;
    if any(uv < vec2(0.0)) || any(uv > vec2(1.0)) {
        return params.background;
    }
    return textureSample(map_texture, map_sampler, uv);
}
