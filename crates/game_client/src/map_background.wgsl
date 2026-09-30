// A map's background (see `map_background::MapBackground`): the level's map turned and zoomed
// (the minimap) or as it is (the big maps), in the classic style BF2's map image as it is, in
// the tactical style the generated tactical map (or BF2's image, toned down) with hatching
// outside the combat area, main bases tinted in their team's colour, a grid and our squad's
// order lines, dashed and flowing towards the order.

#import bevy_ui::ui_vertex_output::UiVertexOutput
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> globals: Globals;

struct MapParams {
    // xy: the map position (0..1) at the node's centre, z: rotation (clockwise from north),
    // w: span (share of the map across the node).
    view: vec4<f32>,
    background: vec4<f32>,
    // x: 1 tactical, y: map opacity, z: 1 with a combat area mask, w: grid cells (0: none).
    style: vec4<f32>,
    // x: 1 with a map image, y: 1 to tone BF2's image down, z: hatch spacing, w: line count
    // (map shares).
    extra: vec4<f32>,
    // x: line width, y: dash length, z: zone outline width (map shares).
    line: vec4<f32>,
    line_color: vec4<f32>,
    // Order lines: from xy to zw (map positions).
    segments: array<vec4<f32>, 12>,
    // Team zones: centre xy, radius z (map shares); colour in `zone_colors` (alpha 0: unused).
    zones: array<vec4<f32>, 4>,
    zone_colors: array<vec4<f32>, 4>,
}

@group(1) @binding(0) var<uniform> params: MapParams;
@group(1) @binding(1) var map_texture: texture_2d<f32>;
@group(1) @binding(2) var map_sampler: sampler;
@group(1) @binding(3) var bounds_texture: texture_2d<f32>;
@group(1) @binding(4) var bounds_sampler: sampler;

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
    // Node space around the centre, y down, like the map.
    let p = in.uv - vec2(0.5);
    let c = cos(params.view.z);
    let s = sin(params.view.z);
    let uv = params.view.xy + vec2(c * p.x - s * p.y, s * p.x + c * p.y) * params.view.w;
    let tactical = params.style.x > 0.5;
    // Derivatives before any branch.
    let texel = textureSample(map_texture, map_sampler, clamp(uv, vec2(0.0), vec2(1.0)));
    let mask = textureSample(bounds_texture, bounds_sampler, clamp(uv, vec2(0.0), vec2(1.0))).r;
    let mask_width = fwidth(mask);
    let uv_pixel = max(fwidth(uv.x) + fwidth(uv.y), 1e-7) * 0.5;

    let outside_map = any(uv < vec2(0.0)) || any(uv > vec2(1.0));
    if outside_map || params.extra.x < 0.5 {
        return params.background;
    }
    if !tactical {
        return texel;
    }

    var color = vec4(texel.rgb, 1.0);
    if params.extra.y > 0.5 {
        // BF2's painted map, toned down to the tactical slate: less saturated, darker, cooler.
        let luma = dot(texel.rgb, vec3(0.299, 0.587, 0.114));
        let grey = mix(vec3(luma), texel.rgb, 0.35);
        color = vec4(grey * vec3(0.62, 0.68, 0.74) + vec3(0.02, 0.03, 0.05), 1.0);
    }

    // The grid.
    if params.style.w > 0.0 {
        let cells = uv * params.style.w;
        let g = abs(fract(cells + vec2(0.5)) - vec2(0.5)) / max(fwidth(cells), vec2(1e-6));
        let line = 1.0 - clamp(min(g.x, g.y) - 0.5, 0.0, 1.0);
        color = over(vec4(0.85, 0.9, 1.0, 0.09 * line), color);
    }

    // Team zones (main bases): a tint and an outline.
    for (var i = 0; i < 4; i++) {
        let zone_color = params.zone_colors[i];
        if zone_color.a <= 0.0 {
            continue;
        }
        let zone = params.zones[i];
        let d = distance(uv, zone.xy) - zone.z;
        let fill = clamp(0.5 - d / uv_pixel, 0.0, 1.0);
        let ring = clamp(1.0 - abs(d) / max(params.line.z, uv_pixel), 0.0, 1.0);
        color = over(vec4(zone_color.rgb, 0.16 * fill), color);
        color = over(vec4(zone_color.rgb, 0.85 * ring), color);
    }

    // Outside the combat area: darker, hatched, and a light line along its edge.
    if params.style.z > 0.5 {
        let out = clamp((0.5 - mask) / max(mask_width, 1e-4) + 0.5, 0.0, 1.0);
        let spacing = max(params.extra.z, 1e-6);
        let stripe = abs(fract((uv.x + uv.y) / spacing) - 0.5) * spacing / uv_pixel;
        let hatch = clamp(1.6 - stripe, 0.0, 1.0);
        color = over(vec4(0.02, 0.03, 0.05, 0.5 * out), color);
        color = over(vec4(0.75, 0.8, 0.88, 0.22 * hatch * out), color);
        let edge = clamp(1.0 - abs(mask - 0.5) / max(mask_width * 1.2, 1e-4), 0.0, 1.0);
        color = over(vec4(0.9, 0.93, 0.97, 0.7 * edge), color);
    }

    // Order lines, dashed, the dashes flowing towards the order.
    let count = i32(params.extra.w);
    let half_width = params.line.x * 0.5;
    let dash = max(params.line.y, 1e-6);
    for (var i = 0; i < count; i++) {
        let segment = params.segments[i];
        let a = segment.xy;
        let b = segment.zw;
        let ab = b - a;
        let len = max(length(ab), 1e-6);
        let t = clamp(dot(uv - a, ab) / (len * len), 0.0, 1.0);
        let d = distance(uv, a + ab * t);
        let along = t * len / dash - globals.time * 1.2;
        let on = step(fract(along), 0.55);
        let body = clamp(half_width - d, 0.0, uv_pixel) / uv_pixel;
        let edge = clamp(half_width + uv_pixel * 1.5 - d, 0.0, uv_pixel) / uv_pixel;
        color = over(vec4(0.0, 0.0, 0.0, 0.35 * edge * on), color);
        color = over(vec4(params.line_color.rgb, params.line_color.a * body * on), color);
    }

    return vec4(color.rgb, params.style.y);
}
