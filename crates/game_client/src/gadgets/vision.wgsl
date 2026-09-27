// What the eyes see through gadgets and after hits. After BF2's PostProduction shaders:
// night vision is `TVEffect_Gradient_Tex` (brightness plus interference noise, mapped
// through a color ramp), tear gas `WaveDistortion` (the picture wobbles by a pixel-scaled
// sine field), a flashbang `Flashbang` (the frame of the flash burnt in, fading), a blast
// `Tinnitus` (blurred, washed out). The gas mask's two lenses are ours.

#import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput

struct Settings {
    time: f32,
    night_vision: f32,
    gas: f32,
    mask: f32,
    white: f32,
    glow: f32,
    afterimage: f32,
    shock: f32,
    capture: u32,
};

@group(0) @binding(0) var screen: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@group(0) @binding(2) var gradient: texture_2d<f32>;
@group(0) @binding(3) var burnt_in: texture_2d<f32>;
@group(0) @binding(4) var<uniform> settings: Settings;

const PI: f32 = 3.14159265;
// BF2's TV effect defaults: interference and the ambient floor of the picture.
const INTERFERENCE: f32 = 0.05;
const TV_AMBIENT: f32 = 0.15;
// Night vision amplifies light: brightness b becomes 1 - exp(-b * GAIN).
const NV_GAIN: f32 = 4.0;

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453);
}

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3(0.3, 0.59, 0.11));
}

// A soft disc blur of `radius` pixels.
fn blurred(uv: vec2<f32>, pixel: vec2<f32>, radius: f32) -> vec3<f32> {
    var sum = textureSample(screen, screen_sampler, uv).rgb;
    for (var i = 0; i < 8; i++) {
        let angle = f32(i) * PI * 0.25;
        let offset = vec2(cos(angle), sin(angle)) * pixel * radius;
        sum += textureSample(screen, screen_sampler, uv + offset).rgb;
        sum += textureSample(screen, screen_sampler, uv + offset * 0.5).rgb;
    }
    return sum / 17.0;
}

@fragment
fn fragment(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(screen));
    let pixel = 1.0 / dims;
    let t = settings.time;
    var uv = in.uv;

    // Tear gas: the picture swims.
    if settings.gas > 0.0 {
        let p = uv * 2.0 - 1.0;
        let wave = vec2(
            cos(p.x * 5.7 + p.y * 2.8 + t * PI * 4.0),
            sin(p.x * 2.9 + p.y * 3.3 + t * PI * 2.0),
        );
        uv += wave * pixel * 8.0 * settings.gas;
    }

    var color = textureSample(screen, screen_sampler, uv).rgb;
    let blur = max(settings.gas * 0.85, settings.shock);
    if blur > 0.0 {
        color = mix(color, blurred(uv, pixel, 2.0 + 10.0 * blur), min(blur * 1.5, 1.0));
    }
    if settings.gas > 0.0 {
        // Teary: bright, washed out, reddened at the edges.
        let edge = length(in.uv - 0.5) * 1.4;
        color = mix(color, vec3(luminance(color)) * 1.25 + 0.08, 0.4 * settings.gas);
        color *= mix(vec3(1.0), vec3(1.1, 0.8, 0.8), clamp(edge * settings.gas, 0.0, 1.0));
    }
    if settings.shock > 0.0 {
        color = mix(color, vec3(luminance(color)), 0.6 * settings.shock);
    }

    // Night vision: amplified brightness with interference, through the color ramp.
    if settings.night_vision > 0.0 {
        var brightness = luminance(color);
        // Bright lights bloom.
        let glow = luminance(blurred(uv, pixel, 6.0));
        brightness = max(brightness, glow * 0.8) + max(glow - 0.5, 0.0);
        brightness = 1.0 - exp(-brightness * NV_GAIN);
        let grain = hash(floor(in.uv * dims / 1.5) + vec2(fract(t * 7.1) * 311.0, fract(t * 3.7) * 173.0)) - 0.2;
        let intensity = clamp(INTERFERENCE * grain + brightness * (1.0 - TV_AMBIENT) + TV_AMBIENT, 0.0, 1.0);
        var nv = textureSample(gradient, screen_sampler, vec2(intensity, 0.5)).rgb;
        // The round field of view of the goggles.
        let r = length((in.uv - 0.5) * vec2(dims.x / dims.y, 1.0));
        nv *= smoothstep(0.95, 0.6, r);
        color = mix(color, nv, settings.night_vision);
    }

    // Gas mask: the world through two lenses in a dark rubber frame.
    if settings.mask > 0.0 {
        let q = (in.uv - 0.5) * vec2(dims.x / dims.y, 1.0);
        let lens = min(length((q - vec2(-0.34, 0.03)) * vec2(1.0, 0.9)), length((q - vec2(0.34, 0.03)) * vec2(1.0, 0.9)));
        let inside = smoothstep(0.5, 0.45, lens);
        // Grimy rim, a faint tint of the glass.
        let rim = smoothstep(0.32, 0.49, lens) * 0.55;
        let through = color * vec3(0.92, 0.97, 0.94) * (1.0 - rim);
        color = mix(color, mix(vec3(0.015), through, inside), settings.mask);
    }

    // Flashbang: the picture of the flash burnt in, a glow over it, and white.
    if settings.afterimage > 0.0 {
        let kept = textureSample(burnt_in, screen_sampler, in.uv).rgb;
        color = mix(color, max(kept, color), settings.afterimage);
    }
    color += vec3(0.9, 0.9, 0.85) * settings.glow * 0.6;
    color = mix(color, vec3(1.0), settings.white);

    return vec4(color, 1.0);
}
