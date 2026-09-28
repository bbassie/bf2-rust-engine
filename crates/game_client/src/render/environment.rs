//! Sun, sky, ambient light and fog from the level's environment settings.
//!
//! # Lighting model
//!
//! BF2 lit its world with baked lightmaps and per-kind light colours in gamma space (see
//! [`game_data::WorldLighting`]); we light it in real time with one sun (the moon on night
//! levels) and one ambient light, derived from those colours ([`LevelLight`]):
//!
//! - The global ambient and sun light static objects (whose albedo carries BF2's `x 2`,
//!   see `bf2_material.wgsl`) as BF2's `static_sky` and `static_sun` did: a BF2 light `L`
//!   (gamma space, applied to a gamma-space texture) is `L^2.2` in linear light. The ambient
//!   is the sky light an open surface gets: `0.65` (BF2's sky normal) x `0.85` (sky
//!   visibility of open surfaces in the lightmaps) x `static_sky`.
//! - Terrain and undergrowth scale both by their own factors ([`LightScale`]): BF2 lit them
//!   with `terrain_gi` (x 0.85 sky visibility) and `2 x terrain_sun`, on a colour map
//!   without the statics' `x 2` (`TerrainShader_Hi.fx`: `colormap x detail x 2 x (2 x
//!   lightmap.g x sun + lightmap.b x GI)`). BF2 clamped these bright colours; the part of
//!   the difference to the statics beyond the albedos is compressed ([`CLASS_CONTRAST`],
//!   [`CLASS_SUN_CONTRAST`]).
//! - Trees (and their distant stand-ins) scale them by BF2's tree colours relative to the
//!   static ones (`tree_ambient / 2` and `tree_sun`; within 0.5..2 of them in gamma space).
//! - Soldiers and vehicles use the global light as they are: BF2 lit them about as much
//!   relative to the world as our albedos without the statics' `x 2` do.
//! - BF2's sun and shade differ by up to 20 times in linear light (it clamped, and
//!   saturated sunlit surfaces); we keep a flatter ratio: sun and ambient have separate
//!   exposures ([`AMBIENT_EXPOSURE`], [`SUN_EXPOSURE`]) tuned so Strike at Karkand keeps
//!   the brightness it had. Other levels keep BF2's relative brightness and sun-to-shade
//!   ratio, compressed by [`ADAPTATION`] (an eye adapting between levels) and
//!   [`CONTRAST`], and their tints.
//! - Night levels (BF2 lit statics and terrain without sun, but gave soldiers moonlight):
//!   the moon (the dynamic sun colour) lights everything, at [`MOON`] times the ambient's
//!   brightness, casting shadows. The colours are the faked-HDR "dark-adapted" ones BF2
//!   showed while the player looked at dark surroundings.
//!
//! # Direction and occlusion of the ambient light
//!
//! - The ambient is a sky around the camera ([`SkyLight`], an environment map made from
//!   [`LevelLight::sky_gradient`]): surfaces facing up get [`SKY_UP`] times the ambient,
//!   walls less and undersides the light the lit ground throws back. Setting `sky_light`.
//! - Baked occlusion (setting `baked_ao`) takes BF2's sky visibility, never its baked sun:
//!   the terrain and undergrowth from the terrain lightmaps (`materials::BakedSky`), static
//!   objects from their object lightmaps (`static_lightmaps`), soldiers, vehicles and props
//!   from rays into the collision around them (`sky_occlusion`).
//! Levels without [`game_data::WorldLighting`] (made without BF2's `sky.con`) are lit from
//! the dynamic `ambient_color` and `sun_color` alone. `BF2_LIGHT=ambient=9,sun=1.2,...`
//! overrides the constants for tuning (keys: ambient, sun, adapt, night_adapt, contrast,
//! class, class_sun, moon, terrain_moon, tint, sky_up, horizon, bounce).

use bevy::{
    asset::LoadState,
    gltf::{GltfAssetLabel, GltfMesh},
    light::{
        AmbientLight, CascadeShadowConfig, CascadeShadowConfigBuilder, EnvironmentMapLight, NotShadowCaster,
        NotShadowReceiver,
    },
    mesh::MeshVertexBufferLayoutRef,
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError},
    shader::ShaderRef,
};
use game_shared::level::LoadedLevel;

use super::sky_light::SkyGradient;
use crate::camera::PlayerCamera;

pub struct EnvironmentPlugin;

impl Plugin for EnvironmentPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "shaders/sky.wgsl");
        app.insert_resource(ClearColor(Color::srgb(0.55, 0.68, 0.85)))
            .add_plugins(MaterialPlugin::<SkyMaterial>::default())
            .add_systems(
                Update,
                (
                    apply_environment.run_if(resource_exists_and_changed::<LoadedLevel>),
                    spawn_sky_mesh,
                    attach_sky_light.after(apply_environment),
                ),
            )
            .add_systems(
                PostUpdate,
                (follow_camera, update_view_distance)
                    .after(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Illuminance (lux) of the sun on levels without world lighting. BF2 adds about 2 x sun on
/// top of 2 x ambient (in gamma terms) and saturates; ~4000 lux adds about 1.3x the
/// face-value luminance of the ambient, so sunlit surfaces stay readable and shade isn't
/// crushed by tonemapping.
const SUN_ILLUMINANCE: f32 = 4_000.0;
/// Luminance (cd/m2) of linear light 1: a white surface shows white at the default exposure.
const LIGHT_UNIT: f32 = 1_000.0;
/// Bevy's uniform ambient light reflects `EnvBRDFApprox(albedo, F_AB(1, n.v))`, about 0.45 x
/// the albedo, where its environment maps reflect about 0.96 x: the environment map's
/// intensity that lights a surface as the ambient light of the same colour did (the light
/// constants were tuned with the ambient light).
const ENVIRONMENT_PER_AMBIENT: f32 = 0.452 / 0.96;

/// Linear ambient light per unit of BF2 light^2.2 (see the module docs).
pub const AMBIENT_EXPOSURE: f32 = 9.2;
/// Linear sunlight (1 = [`LIGHT_UNIT`] on a surface facing it) per unit of BF2 light^2.2.
pub const SUN_EXPOSURE: f32 = 1.17;
/// How much of the brightness difference between a level and Strike at Karkand is
/// compensated (0: none, BF2's; 1: all levels equally bright), in log space.
pub const ADAPTATION: f32 = 0.25;
/// How much of a level's sun-to-shade ratio beyond Strike at Karkand's is kept (0: all
/// levels have Karkand's; 1: BF2's), in log space: BF2's range, from overcast levels with a
/// faint sun to harsh ones with dark shade, is too wide without its clamping.
pub const CONTRAST: f32 = 0.5;
/// How much of BF2's difference between the terrain's (or trees') light and the statics'
/// is kept beyond what the albedos explain (0: none; 1: all), in log space, for the sky
/// light and for the sun. BF2 clamped the terrain's bright sun and GI colours, which here
/// would wash the ground out (sunlit ground on Dalian Plant, Gulf of Oman, Midnight Sun).
pub const CLASS_CONTRAST: f32 = 0.5;
pub const CLASS_SUN_CONTRAST: f32 = 0.25;
/// Highest terrain lightmap sun scale used (Midnight Sun's lightmaps are 1.7 times N.L).
const MAX_TERRAIN_SUN_SCALE: f32 = 1.2;
/// Moonlight on night levels, relative to the ambient's brightness.
pub const MOON: f32 = 0.6;
/// Share of the moonlight the terrain gets (BF2 gave it none; its sky light already
/// matches the statics').
pub const TERRAIN_MOON: f32 = 0.35;
/// [`ADAPTATION`] on night levels: they are meant to be darker.
pub const NIGHT_ADAPTATION: f32 = 0.15;
/// Strength of the levels' light tints (1: BF2's; 0: grey light).
pub const TINT: f32 = 1.0;
/// Sky light on surfaces facing straight up, relative to the uniform ambient it replaces;
/// walls and undersides get less (see [`LevelLight::sky_gradient`]).
pub const SKY_UP: f32 = 1.1;
/// Brightness of the haze at the horizon relative to the zenith.
pub const HORIZON: f32 = 1.3;
/// How much the haze takes the fog's hue (0: the ambient's).
const HORIZON_FOG_TINT: f32 = 0.35;
/// Share of the light on the ground that it throws back up (times its albedo).
pub const BOUNCE: f32 = 1.0;
/// The ground below the horizon is at least and at most this bright relative to the sky
/// light from above (night levels paint their colour maps nearly black).
const GROUND_MIN: f32 = 0.3;
const GROUND_MAX: f32 = 0.5;
/// Ground albedo where the level doesn't give one.
const DEFAULT_GROUND_ALBEDO: f32 = 0.15;

/// Sky light on open static surfaces: BF2's sky normal (0.65) x open sky visibility (0.85).
const OPEN_SKY_STATIC: f32 = 0.65 * 0.85;
/// Sky light on open terrain: the terrain lightmaps' sky visibility there.
const OPEN_SKY_TERRAIN: f32 = 0.85;
/// Strike at Karkand's static sky and sun: the level whose brightness the exposures keep.
const REFERENCE_SKY: [f32; 3] = [0.53, 0.45, 0.28];
const REFERENCE_SUN: [f32; 3] = [0.8, 0.74, 0.58];
const LUMA: Vec3 = Vec3::new(0.2126, 0.7152, 0.0722);

/// Factors on the global sun and ambient light for one kind of surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightScale {
    pub sun: Vec3,
    pub ambient: Vec3,
}

impl LightScale {
    pub const ONE: Self = Self {
        sun: Vec3::ONE,
        ambient: Vec3::ONE,
    };

    /// For shaders: xyz of the first scale the albedo (so direct light), of the second the
    /// diffuse occlusion (so ambient light, which the albedo also scales).
    pub fn uniforms(&self) -> (Vec4, Vec4) {
        let sun = self.sun.max(Vec3::splat(1e-6));
        (sun.extend(1.0), (self.ambient / sun).extend(1.0))
    }

    /// One factor for both, for a surface lit by the ambient and the sun x `n_dot_l`.
    pub fn average(&self, light: &LevelLight, n_dot_l: f32) -> Vec3 {
        let lit = light.ambient + light.sun * n_dot_l;
        (light.ambient * self.ambient + light.sun * n_dot_l * self.sun) / lit.max(Vec3::splat(1e-6))
    }
}

/// The level's light in linear units (see the module docs).
#[derive(Clone, Copy, Debug)]
pub struct LevelLight {
    /// Ambient light, 1 = [`LIGHT_UNIT`].
    pub ambient: Vec3,
    /// Sun (or moon) light on a surface facing it, 1 = [`LIGHT_UNIT`].
    pub sun: Vec3,
    /// Terrain and undergrowth.
    pub terrain: LightScale,
    /// Trees.
    pub trees: LightScale,
    pub night: bool,
}

/// Tuning constants, overridable with `BF2_LIGHT`.
#[derive(Clone, Copy, Debug)]
struct Knobs {
    ambient: f32,
    sun: f32,
    adapt: f32,
    night_adapt: f32,
    contrast: f32,
    class: f32,
    class_sun: f32,
    moon: f32,
    terrain_moon: f32,
    tint: f32,
    sky_up: f32,
    horizon: f32,
    bounce: f32,
}

impl Knobs {
    fn get() -> Self {
        static KNOBS: std::sync::OnceLock<Knobs> = std::sync::OnceLock::new();
        *KNOBS.get_or_init(|| {
            let mut knobs = Knobs {
                ambient: AMBIENT_EXPOSURE,
                sun: SUN_EXPOSURE,
                adapt: ADAPTATION,
                night_adapt: NIGHT_ADAPTATION,
                contrast: CONTRAST,
                class: CLASS_CONTRAST,
                class_sun: CLASS_SUN_CONTRAST,
                moon: MOON,
                terrain_moon: TERRAIN_MOON,
                tint: TINT,
                sky_up: SKY_UP,
                horizon: HORIZON,
                bounce: BOUNCE,
            };
            for pair in std::env::var("BF2_LIGHT").unwrap_or_default().split(',') {
                let Some((key, value)) = pair.split_once('=') else { continue };
                let Ok(value) = value.trim().parse::<f32>() else { continue };
                match key.trim() {
                    "ambient" => knobs.ambient = value,
                    "sun" => knobs.sun = value,
                    "adapt" => knobs.adapt = value,
                    "night_adapt" => knobs.night_adapt = value,
                    "contrast" => knobs.contrast = value,
                    "class" => knobs.class = value,
                    "class_sun" => knobs.class_sun = value,
                    "terrain_moon" => knobs.terrain_moon = value,
                    "moon" => knobs.moon = value,
                    "tint" => knobs.tint = value,
                    "sky_up" => knobs.sky_up = value,
                    "horizon" => knobs.horizon = value,
                    "bounce" => knobs.bounce = value,
                    other => warn!("BF2_LIGHT: unknown key {other}"),
                }
            }
            knobs
        })
    }
}

/// A BF2 (gamma-space) light as linear light, its tint scaled by `tint` at equal luminance.
fn to_linear(c: Vec3, tint: f32) -> Vec3 {
    let linear = c.max(Vec3::ZERO).powf(2.2);
    let y = linear.dot(LUMA);
    if y <= 1e-9 || tint == 1.0 {
        return linear;
    }
    let chroma = (linear / y).powf(tint);
    chroma * (y / chroma.dot(LUMA).max(1e-9))
}

impl LevelLight {
    pub fn new(env: &game_data::EnvironmentDesc) -> Self {
        let Some(lighting) = &env.lighting else {
            return Self::from_dynamic(env);
        };
        let k = Knobs::get();
        let (l, moon) = lighting.dark_adapted(env.sun_color);
        let v = Vec3::from_array;
        let dim = |c: [f32; 3]| v(c).dot(LUMA) < 0.05;
        let night = lighting.dark_adapted.is_some() || (dim(l.static_sun) && dim(l.terrain_sun));

        let ambient_static = to_linear(OPEN_SKY_STATIC * v(l.static_sky), k.tint);
        let ambient_terrain = to_linear(OPEN_SKY_TERRAIN * v(l.terrain_gi), k.tint);
        let (sun_static, sun_terrain) = if night {
            let tint = to_linear(v(moon), k.tint);
            let tint = tint / tint.dot(LUMA).max(1e-9);
            let moon = tint * k.moon * ambient_static.dot(LUMA) * k.ambient / k.sun;
            // On the terrain's albedo, which lacks the statics' x 2.
            (moon, moon * 2f32.powf(2.2) * k.terrain_moon)
        } else {
            // Overcast levels may have (almost) no sun on statics but some on the terrain.
            let terrain = v(l.terrain_sun) * 2.0 * l.terrain_sun_scale.min(MAX_TERRAIN_SUN_SCALE);
            let floor = terrain.normalize_or(Vec3::ONE) * 0.03;
            let statics = if dim(l.static_sun) { v(l.static_sun).max(floor) } else { v(l.static_sun) };
            (to_linear(statics, k.tint), to_linear(terrain, k.tint))
        };
        let ratio = |a: Vec3, b: Vec3| (a + Vec3::splat(1e-7)) / (b + Vec3::splat(1e-7));
        // The terrain's albedo lacks the statics' x 2 (2^2.2 in linear light); only BF2's
        // difference beyond that is compressed.
        let albedo = 2f32.powf(2.2);
        let compress = |r: Vec3, base: f32, amount: f32| base * (r / base).powf(amount);
        let terrain = LightScale {
            sun: if night {
                ratio(sun_terrain, sun_static)
            } else {
                compress(ratio(sun_terrain, sun_static), albedo, k.class_sun)
            },
            ambient: compress(ratio(ambient_terrain, ambient_static), albedo, k.class),
        };
        // BF2's leaf shader: `texture x 2 x (tree_sun x N.L + tree_ambient / 2)`. Its moonlight
        // is the statics' here. Some levels set odd tree colours (Leviathan: black ambient).
        let (low, high) = (Vec3::splat(0.25), Vec3::splat(4.0));
        let trees = LightScale {
            sun: if night {
                Vec3::ONE
            } else {
                compress(ratio(to_linear(v(l.tree_sun), k.tint), sun_static), 1.0, k.class_sun).clamp(low, high)
            },
            ambient: compress(ratio(to_linear(0.5 * v(l.tree_ambient), k.tint), ambient_static), 1.0, k.class)
                .clamp(low, high),
        };

        let (reference_ambient, reference_sun) = (
            k.ambient * to_linear(OPEN_SKY_STATIC * v(REFERENCE_SKY), 1.0).dot(LUMA),
            k.sun * to_linear(v(REFERENCE_SUN), 1.0).dot(LUMA),
        );
        let mut ambient = ambient_static * k.ambient;
        let mut sun = sun_static * k.sun;
        if !night {
            // Towards Karkand's sun-to-shade ratio, keeping their product.
            let ratio = sun.dot(LUMA) / ambient.dot(LUMA).max(1e-9);
            let wanted = reference_sun / reference_ambient;
            let shift = (ratio / wanted).max(1e-9).powf((1.0 - k.contrast) * 0.5);
            ambient *= shift;
            sun /= shift;
        }
        let level = |ambient: f32, sun: f32| ambient + 0.5 * sun;
        let exposure = (level(reference_ambient, reference_sun) / level(ambient.dot(LUMA), sun.dot(LUMA)).max(1e-6))
            .powf(if night { k.night_adapt } else { k.adapt });
        Self {
            ambient: ambient * exposure,
            sun: sun * exposure,
            terrain,
            trees,
            night,
        }
    }

    /// From the dynamic ambient and sun colours alone: BF2 lights in gamma space,
    /// `texture x 2 x (ambient + sun x N.L)`, so shade shows at `(2 x ambient)^2.2` of the
    /// texture's own luminance. Many levels have an overbright sun (up to ~2.3), made for
    /// BF2's clamped lighting: its hue at the usual brightness.
    fn from_dynamic(env: &game_data::EnvironmentDesc) -> Self {
        let sun = Vec3::from_array(env.sun_color);
        let ambient = Vec3::from_array(env.ambient_color);
        Self {
            ambient: (2.0 * ambient).clamp(Vec3::ZERO, Vec3::ONE).powf(2.2),
            sun: sun / sun.max_element().max(1.0) * SUN_ILLUMINANCE / (LIGHT_UNIT * std::f32::consts::PI),
            terrain: LightScale::ONE,
            trees: LightScale::ONE,
            night: false,
        }
    }

    /// The ambient light as the sky and ground around a surface (for an environment map):
    /// the sky tinted like the ambient, hazier towards the fog colour at the horizon, gives
    /// surfaces facing up [`SKY_UP`] times the ambient; the ground reflects the sun and sky
    /// light the terrain gets (times its albedo), so walls get less and undersides least.
    pub fn sky_gradient(&self, env: &game_data::EnvironmentDesc) -> SkyGradient {
        let k = Knobs::get();
        let lum = |c: Vec3| c.dot(LUMA).max(1e-9);
        let chroma = |c: Vec3| c / lum(c);
        let up = self.ambient * k.sky_up;
        // The fog's hue, limited: night fog colours are nearly pure blue.
        let fog = chroma(Vec3::from_array(env.fog_color).max(Vec3::splat(1e-3)).powf(2.2));
        let fog = chroma(fog.clamp(Vec3::splat(0.5), Vec3::splat(1.8)));
        let mut sky = SkyGradient {
            zenith: chroma(self.ambient),
            horizon: chroma(self.ambient).lerp(fog, HORIZON_FOG_TINT) * k.horizon,
            ground: Vec3::ZERO,
        };
        // Surfaces facing up don't see the ground: scale the sky to give them `up`.
        let scale = lum(up) / lum(sky.irradiance(1.0));
        sky.zenith *= scale;
        sky.horizon *= scale;
        let albedo = env.ground_albedo.map_or(Vec3::splat(DEFAULT_GROUND_ALBEDO), Vec3::from_array);
        let height = (-Vec3::from_array(env.sun_direction).normalize_or(Vec3::NEG_Y).y).max(0.0);
        let lit = up * self.terrain.ambient + self.sun * height * self.terrain.sun;
        let mut ground = albedo * lit * k.bounce;
        let (min, max) = (GROUND_MIN * lum(up), GROUND_MAX * lum(up));
        if lum(ground) > max {
            ground *= max / lum(ground);
        } else if lum(ground) < min {
            ground = ground.lerp(chroma(ground.max(Vec3::splat(1e-6))) * min, 1.0 - lum(ground) / min);
        }
        sky.ground = ground;
        sky
    }

    /// The sun as a directional light: colour and illuminance (lux).
    fn sun_light(&self) -> (Color, f32) {
        let max = self.sun.max_element().max(1e-9);
        let color = self.sun / max;
        (
            Color::linear_rgb(color.x, color.y, color.z),
            max * LIGHT_UNIT * std::f32::consts::PI,
        )
    }
}

/// The level's sun (lights the world layer only).
#[derive(Component)]
pub struct Sun;

/// The level's sky light: environment maps made from [`LevelLight::sky_gradient`], lighting
/// every 3D camera's view instead of the uniform [`GlobalAmbientLight`] (unless the
/// `sky_light` setting is off).
#[derive(Resource, Clone)]
pub struct SkyLight {
    pub diffuse: Handle<Image>,
    pub specular: Handle<Image>,
}

/// Puts the level's [`SkyLight`] on the 3D cameras (the player's and the view model's), or
/// takes it off when the setting is off.
fn attach_sky_light(
    mut commands: Commands,
    sky: Option<Res<SkyLight>>,
    settings: Res<crate::settings::Settings>,
    cameras: Query<(Entity, Option<&EnvironmentMapLight>), With<Camera3d>>,
) {
    let wanted = sky.as_deref().filter(|_| settings.sky_light);
    for (camera, current) in &cameras {
        match (wanted, current) {
            (Some(sky), current) if current.is_none_or(|c| c.diffuse_map != sky.diffuse) => {
                commands.entity(camera).insert((
                    EnvironmentMapLight {
                        diffuse_map: sky.diffuse.clone(),
                        specular_map: sky.specular.clone(),
                        intensity: LIGHT_UNIT * ENVIRONMENT_PER_AMBIENT,
                        ..default()
                    },
                    // The environment map replaces the uniform ambient.
                    AmbientLight {
                        color: Color::BLACK,
                        brightness: 0.0,
                        affects_lightmapped_meshes: true,
                    },
                ));
            }
            (None, Some(_)) => {
                commands.entity(camera).remove::<(EnvironmentMapLight, AmbientLight)>();
            }
            _ => {}
        }
    }
}

/// The sky dome; it moves with the camera so it always surrounds it.
#[derive(Component)]
struct SkyDome {
    mesh: Handle<GltfMesh>,
    material: Handle<SkyMaterial>,
    spawned: bool,
}

fn apply_environment(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    suns: Query<Entity, With<Sun>>,
    mut clear: ResMut<ClearColor>,
    mut ambient: ResMut<GlobalAmbientLight>,
    skies: Query<Entity, With<SkyDome>>,
    mut cameras: Query<&mut DistanceFog, With<PlayerCamera>>,
    asset_server: Res<AssetServer>,
    mut sky_materials: ResMut<Assets<SkyMaterial>>,
    mut images: ResMut<Assets<Image>>,
    cli: Res<crate::Cli>,
) {
    let env = &level.desc.environment;
    let rgb = |c: [f32; 3]| Color::srgb(c[0], c[1], c[2]);

    for entity in suns.iter().chain(&skies) {
        commands.entity(entity).despawn();
    }
    let direction = Vec3::from_array(env.sun_direction).normalize_or(Vec3::NEG_Y);
    let light = LevelLight::new(env);
    info!(
        "level light: ambient {:.3?}, sun {:.3?}, terrain x{:.2?} sun x{:.2?}, trees x{:.2?} sun x{:.2?}{}",
        light.ambient,
        light.sun,
        light.terrain.ambient,
        light.terrain.sun,
        light.trees.ambient,
        light.trees.sun,
        if light.night { ", night" } else { "" }
    );
    let (sun_color, illuminance) = light.sun_light();
    commands.spawn((
        Sun,
        DirectionalLight {
            color: sun_color,
            illuminance,
            shadow_maps_enabled: !cli.no_shadows,
            ..default()
        },
        Transform::default().looking_to(direction, Vec3::Y),
        shadow_cascades(0.0),
    ));

    let gradient = light.sky_gradient(env);
    info!(
        "sky light: zenith {:.3?}, horizon {:.3?}, ground {:.3?}",
        gradient.zenith, gradient.horizon, gradient.ground
    );
    let (diffuse, specular) = super::sky_light::cube_maps(&gradient);
    commands.insert_resource(SkyLight {
        diffuse: images.add(diffuse),
        specular: images.add(specular),
    });

    clear.0 = rgb(env.sky_color);
    *ambient = GlobalAmbientLight {
        color: Color::linear_rgb(light.ambient.x, light.ambient.y, light.ambient.z),
        brightness: LIGHT_UNIT,
        ..default()
    };
    // BF2's view distances were tuned for 2005 hardware (Karkand: 140 m). Stretch them;
    // `update_view_distance` scales this by the setting and the camera's height.
    let fog_end = (env.fog_range[1] * 4.0).max(600.0);
    commands.insert_resource(LevelFog { end: fog_end });
    for mut fog in &mut cameras {
        *fog = DistanceFog {
            color: rgb(env.fog_color),
            directional_light_color: sun_color.with_alpha(0.3),
            directional_light_exponent: 20.0,
            falloff: FogFalloff::Linear {
                start: fog_end * FOG_START,
                end: fog_end,
            },
        };
    }

    // Sky dome around the camera; its shader puts it behind everything, so its size only
    // has to keep it inside the view frustum's far plane for culling.
    if let Some(sky) = &env.sky {
        let scale = fog_end * 0.5 / sky.radius.max(1.0);
        commands.spawn((
            SkyDome {
                mesh: asset_server
                    .load(GltfAssetLabel::Mesh(0).from_asset(format!("imported://{}", sky.mesh))),
                material: sky_materials.add(SkyMaterial {
                    texture: asset_server.load(format!("imported://{}", sky.texture)),
                    params: SkyParams { haze: Vec4::new(SKY_HAZE, 0.0, 0.0, 0.0) },
                }),
                spawned: false,
            },
            Transform::from_rotation(Quat::from_rotation_y(-sky.rotation.to_radians()))
                .with_scale(Vec3::splat(scale)),
            Visibility::default(),
        ));
    }
}

/// Height of the view direction (sine of the elevation) up to which the sky fades from the
/// fog colour at the horizon to its own texture.
const SKY_HAZE: f32 = 0.08;

/// The sky texture on the dome mesh, drawn at the far plane and fogged below the horizon.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
struct SkyMaterial {
    #[texture(0)]
    #[sampler(1)]
    texture: Handle<Image>,
    #[uniform(2)]
    params: SkyParams,
}

#[derive(ShaderType, Debug, Clone, Copy)]
struct SkyParams {
    haze: Vec4,
}

impl Material for SkyMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/render/shaders/sky.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/sky.wgsl".into()
    }

    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
        ])?];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// Where fog starts, as a fraction of where it ends.
const FOG_START: f32 = 0.3;
/// The fog never ends further away than this (the world ends about there).
const MAX_FOG_END: f32 = 15_000.0;
/// Height above the ground (m) that adds the level's fog distance once more, up to
/// `MAX_ALTITUDE_BOOST` times: pilots see the ground they fly over.
const ALTITUDE_PER_BOOST: f32 = 150.0;
const MAX_ALTITUDE_BOOST: f32 = 4.0;

/// The level's fog end (m) at the normal view distance, on the ground.
#[derive(Resource)]
pub struct LevelFog {
    pub end: f32,
}

/// Real-time sun shadows. Near the ground they only cover 120 m, like BF2 (it baked distant
/// shadows into lightmaps); they reach further as the camera climbs, so the ground below
/// an aircraft still has shadows. Every cascade redraws all casters inside it.
fn shadow_cascades(height: f32) -> CascadeShadowConfig {
    CascadeShadowConfigBuilder {
        num_cascades: 3,
        first_cascade_far_bound: 15.0 + height * 0.25,
        maximum_distance: (120.0 + height * 1.5).min(700.0),
        ..default()
    }
    .build()
}

/// Scales fog, far plane and shadow range with the view distance setting and the camera's
/// height above the terrain (smoothed, so climbing opens the view gradually).
fn update_view_distance(
    time: Res<Time>,
    settings: Res<crate::settings::Settings>,
    level_fog: Option<Res<LevelFog>>,
    level: Option<Res<LoadedLevel>>,
    mut cameras: Query<(&Transform, &mut DistanceFog, &mut Projection), With<PlayerCamera>>,
    mut suns: Query<(&mut CascadeShadowConfig, Ref<Sun>)>,
    mut height: Local<Option<f32>>,
    mut shadow_height: Local<f32>,
) {
    let Some(level_fog) = level_fog else {
        return;
    };
    for (transform, mut fog, mut projection) in &mut cameras {
        let eye = transform.translation;
        let ground = level
            .as_ref()
            .and_then(|l| l.heightmap.as_ref())
            .map_or(0.0, |h| h.height_at(eye.x, eye.z));
        let target = (eye.y - ground).max(0.0);
        let smoothed = match *height {
            Some(h) => h + (target - h) * (1.0 - (-time.delta_secs() * 2.0).exp()),
            None => target,
        };
        *height = Some(smoothed);

        let boost = (1.0 + smoothed / ALTITUDE_PER_BOOST).min(MAX_ALTITUDE_BOOST);
        let end = (level_fog.end * settings.view_distance.scale() * boost).min(MAX_FOG_END);
        // Skip tiny changes: every write re-uploads the view.
        let differs = |a: f32, b: f32| (a - b).abs() > b * 0.005;
        if let FogFalloff::Linear { end: old_end, .. } = fog.falloff
            && differs(end, old_end)
        {
            fog.falloff = FogFalloff::Linear {
                start: end * FOG_START,
                end,
            };
        }
        if let Projection::Perspective(perspective) = projection.as_mut()
            && differs(end + 100.0, perspective.far)
        {
            perspective.far = end + 100.0;
        }
        let new_sun = suns.iter().any(|(_, sun)| sun.is_added());
        if new_sun || (smoothed - *shadow_height).abs() > 5.0 + *shadow_height * 0.1 {
            *shadow_height = smoothed;
            for (mut cascades, _) in &mut suns {
                *cascades = shadow_cascades(smoothed);
            }
        }
    }
}

fn spawn_sky_mesh(
    mut commands: Commands,
    mut skies: Query<(Entity, &mut SkyDome)>,
    meshes: Res<Assets<GltfMesh>>,
    asset_server: Res<AssetServer>,
) {
    for (entity, mut sky) in &mut skies {
        if sky.spawned {
            continue;
        }
        if let LoadState::Failed(err) = asset_server.load_state(&sky.mesh) {
            warn!("sky dome failed to load: {err}");
            sky.spawned = true;
            continue;
        }
        let Some(mesh) = meshes.get(&sky.mesh) else {
            continue;
        };
        for primitive in &mesh.primitives {
            commands.entity(entity).with_child((
                Mesh3d(primitive.mesh.clone()),
                MeshMaterial3d(sky.material.clone()),
                NotShadowCaster,
                NotShadowReceiver,
            ));
        }
        sky.spawned = true;
    }
}

fn follow_camera(
    camera: Single<&Transform, (With<PlayerCamera>, Without<SkyDome>)>,
    mut skies: Query<&mut Transform, With<SkyDome>>,
) {
    for mut transform in &mut skies {
        transform.translation = camera.translation;
    }
}
