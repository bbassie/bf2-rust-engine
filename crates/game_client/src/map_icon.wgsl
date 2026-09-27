// A turned map icon (see `map_markers::IconMaterial`): a white silhouette (or a rounded box)
// in a colour with a dark outline, turned with the vehicle's heading, and a white line with a
// dark edge for its turret. The node itself isn't turned (Bevy doesn't clip turned nodes, so
// they would spill out of the minimap): the icon is turned inside it.

#import bevy_ui::ui_vertex_output::UiVertexOutput

struct IconParams {
    color: vec4<f32>,
    halo: vec4<f32>,
    // x, y: the icon's size; z: the node's side; w: the outline's width (logical pixels).
    size: vec4<f32>,
    // x: the icon's heading, y: the turret's (clockwise radians); z, w: the turret line's
    // length and width (pixels, 0 without one).
    turn: vec4<f32>,
    // x: 1 to draw the image, 2 its shape in the colour (images without white), 0 a rounded
    // box.
    kind: vec4<f32>,
}

@group(1) @binding(0) var<uniform> params: IconParams;
@group(1) @binding(1) var icon_texture: texture_2d<f32>;
@group(1) @binding(2) var icon_sampler: sampler;

// `p` turned clockwise on screen (y down) by `angle`.
fn turned(p: vec2<f32>, angle: f32) -> vec2<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return vec2(c * p.x - s * p.y, s * p.x + c * p.y);
}

// The icon at `q`, pixels from its middle in its own frame.
fn icon(q: vec2<f32>) -> vec4<f32> {
    let size = params.size.xy;
    if params.kind.x > 0.5 {
        let uv = q / size + vec2(0.5);
        if any(uv < vec2(0.0)) || any(uv > vec2(1.0)) {
            return vec4(0.0);
        }
        let texel = textureSampleLevel(icon_texture, icon_sampler, uv, 0.0);
        // An image without white: its shape, to colour.
        if params.kind.x > 1.5 {
            return vec4(1.0, 1.0, 1.0, texel.a);
        }
        return texel;
    }
    let radius = min(2.0, min(size.x, size.y) * 0.5);
    let d = abs(q) - (size * 0.5 - vec2(radius));
    let dist = length(max(d, vec2(0.0))) + min(max(d.x, d.y), 0.0) - radius;
    return vec4(1.0, 1.0, 1.0, clamp(0.5 - dist, 0.0, 1.0));
}

// `top` drawn over `bottom` (straight alpha).
fn over(top: vec4<f32>, bottom: vec4<f32>) -> vec4<f32> {
    let alpha = top.a + bottom.a * (1.0 - top.a);
    if alpha <= 0.0 {
        return vec4(0.0);
    }
    return vec4((top.rgb * top.a + bottom.rgb * bottom.a * (1.0 - top.a)) / alpha, alpha);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let p = (in.uv - vec2(0.5)) * params.size.z;
    let q = turned(p, -params.turn.x);
    let body = icon(q);
    // The outline: the icon grown by its width, under it.
    var grown = 0.0;
    for (var i = 0; i < 12; i++) {
        let angle = f32(i) * 0.5235988;
        grown = max(grown, icon(q + vec2(cos(angle), sin(angle)) * params.size.w).a);
    }
    var color = vec4(params.halo.rgb, params.halo.a * grown);
    color = over(vec4(body.rgb * params.color.rgb, body.a * params.color.a), color);
    // The turret: from the middle along its heading, round ends.
    let reach = params.turn.z;
    if reach > 0.0 {
        let direction = turned(vec2(0.0, -1.0), params.turn.y);
        let along = clamp(dot(p, direction), 0.0, reach);
        let dist = length(p - direction * along);
        let half = params.turn.w * 0.5;
        let edge = clamp(half + 0.5 - dist, 0.0, 1.0);
        let core = clamp(half - 0.7 - dist, 0.0, 1.0);
        color = over(vec4(params.halo.rgb, edge * 0.92), color);
        color = over(vec4(1.0, 1.0, 1.0, core), color);
    }
    return color;
}
