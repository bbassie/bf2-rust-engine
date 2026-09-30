// A crisp map shape drawn from its distance field (see `map_shapes::ShapeMaterial`): an
// objective's circle, diamond or square with its capture progress as a pie, a soldier's dot
// with a heading triangle, or the player's arrow. Units: the node's half side is 1, y down.

#import bevy_ui::ui_vertex_output::UiVertexOutput
#import bevy_render::globals::Globals

@group(0) @binding(1) var<uniform> globals: Globals;

struct ShapeParams {
    fill: vec4<f32>,
    outline: vec4<f32>,
    accent: vec4<f32>,
    // x: kind (0 circle, 1 diamond, 2 square, 3 arrow), y: radius, z: outline width,
    // w: capture progress 0..1 (negative: none).
    shape: vec4<f32>,
    // x: heading (clockwise radians, on screen), y: 1 with a heading triangle, z: 1 pulsing,
    // w: dark halo width.
    extra: vec4<f32>,
}

@group(1) @binding(0) var<uniform> params: ShapeParams;

// `p` turned counter-clockwise on screen by `angle` (undoes a clockwise turn).
fn unturn(p: vec2<f32>, angle: f32) -> vec2<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return vec2(c * p.x + s * p.y, -s * p.x + c * p.y);
}

fn sd_box(p: vec2<f32>, half: vec2<f32>) -> f32 {
    let d = abs(p) - half;
    return length(max(d, vec2(0.0))) + min(max(d.x, d.y), 0.0);
}

fn sd_triangle(p: vec2<f32>, p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>) -> f32 {
    let e0 = p1 - p0;
    let e1 = p2 - p1;
    let e2 = p0 - p2;
    let v0 = p - p0;
    let v1 = p - p1;
    let v2 = p - p2;
    let pq0 = v0 - e0 * clamp(dot(v0, e0) / dot(e0, e0), 0.0, 1.0);
    let pq1 = v1 - e1 * clamp(dot(v1, e1) / dot(e1, e1), 0.0, 1.0);
    let pq2 = v2 - e2 * clamp(dot(v2, e2) / dot(e2, e2), 0.0, 1.0);
    let s = sign(e0.x * e2.y - e0.y * e2.x);
    let d = min(min(vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
                    vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
                    vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)));
    return -sqrt(d.x) * sign(d.y);
}

// The shape's signed distance (negative inside).
fn shape_distance(q: vec2<f32>) -> f32 {
    let kind = params.shape.x;
    let r = params.shape.y;
    if kind < 0.5 {
        return length(q) - r;
    }
    if kind < 1.5 {
        // Diamond, corners a little further out so it looks as big as the circle.
        return (abs(q.x) + abs(q.y) - r * 1.18) * 0.70710678;
    }
    if kind < 2.5 {
        let round = r * 0.14;
        return sd_box(q, vec2(r * 0.86 - round)) - round;
    }
    // The player's arrow: a chevron pointing along the heading.
    let p = unturn(q, params.extra.x);
    let tip = vec2(0.0, -r);
    let right = vec2(0.78 * r, 0.82 * r);
    let left = vec2(-0.78 * r, 0.82 * r);
    let notch = vec2(0.0, 0.38 * r);
    return max(sd_triangle(p, tip, right, left), -sd_triangle(p, notch, right + vec2(0.0, 0.6 * r), left + vec2(0.0, 0.6 * r)));
}

// The heading triangle beside a dot.
fn pointer_distance(q: vec2<f32>) -> f32 {
    let r = params.shape.y;
    let p = unturn(q, params.extra.x);
    let tip = vec2(0.0, -r * 1.95);
    let base = -r * 1.28;
    return sd_triangle(p, tip, vec2(r * 0.46, base), vec2(-r * 0.46, base));
}

// `top` drawn over `bottom` (straight alpha).
fn over(top: vec4<f32>, bottom: vec4<f32>) -> vec4<f32> {
    let alpha = top.a + bottom.a * (1.0 - top.a);
    if alpha <= 0.0 {
        return vec4(0.0);
    }
    return vec4((top.rgb * top.a + bottom.rgb * bottom.a * (1.0 - top.a)) / alpha, alpha);
}

fn coverage(d: f32, aa: f32) -> f32 {
    return clamp(0.5 - d / aa, 0.0, 1.0);
}

@fragment
fn fragment(in: UiVertexOutput) -> @location(0) vec4<f32> {
    let q = (in.uv - vec2(0.5)) * 2.0;
    // One physical pixel in these units.
    let aa = max(fwidth(q.x) + fwidth(q.y), 1e-4) * 0.7071;
    let d = shape_distance(q);
    let outline_width = params.shape.z;
    let halo_width = params.extra.w;
    let pulsing = params.extra.z > 0.5;
    let pulse = select(1.0, 0.55 + 0.45 * sin(globals.time * 7.0), pulsing);

    var color = vec4(0.0);
    // A dark halo keeps it readable on bright ground.
    var halo = coverage(d - halo_width, aa);
    var pointer = 1e3;
    if params.extra.y > 0.5 {
        pointer = pointer_distance(q);
        halo = max(halo, coverage(pointer - halo_width, aa));
    }
    color = vec4(0.0, 0.0, 0.0, 0.45 * halo);
    // Contested: a soft glow in the outline's colour, breathing.
    if pulsing {
        let glow = clamp(1.0 - d / (params.shape.y * 0.7), 0.0, 1.0) * (1.0 - coverage(d, aa));
        color = over(vec4(params.outline.rgb, glow * glow * 0.8 * (1.0 - pulse * 0.6)), color);
    }
    let inside = coverage(d + outline_width, aa);
    var fill = params.fill;
    // Capture progress: a pie from twelve o'clock, clockwise, in the capturing team's colour.
    let progress = params.shape.w;
    if progress >= 0.0 {
        var angle = atan2(q.x, -q.y);
        if angle < 0.0 {
            angle += 6.2831853;
        }
        // Soft edge along the pie's side, about a pixel wide at the rim.
        let edge = (progress * 6.2831853 - angle) * max(length(q), 0.05);
        let pie = clamp(edge / aa + 0.5, 0.0, 1.0);
        fill = over(vec4(params.accent.rgb, params.accent.a * pie), fill);
    }
    color = over(vec4(fill.rgb, fill.a * inside), color);
    let band = coverage(d, aa) * (1.0 - inside);
    color = over(vec4(params.outline.rgb, params.outline.a * band * pulse), color);
    if params.extra.y > 0.5 {
        color = over(vec4(params.outline.rgb, params.outline.a * coverage(pointer, aa)), color);
    }
    return color;
}
