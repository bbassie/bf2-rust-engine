//! Real-time lamps: point (and spot) lights at the level's lamps ([`game_data::LampDesc`])
//! where they matter, instead of BF2's baked lamp light.
//!
//! - Night levels light all their lamps; day levels only those indoors (few rays up into the
//!   world's collision find little sky, measured once near the camera), where the ambient is
//!   occluded and a ceiling light still shows. Night versions of day levels also light the
//!   street lamps BF2 gave no light (`LampDesc::unlit`).
//! - Brightness: BF2 baked `strength x (1 - (d - near) / (far - near))` (clamped to 1) times
//!   the level's lamp colour as gamma-space light. [`LevelLight::lamp`] is that colour at 1 as
//!   linear light; each lamp's lumens fit that falloff (a geometric mean over the lit range),
//!   with the physically based inverse-square falloff Bevy draws, windowed to end at `far`.
//! - Cost: clustered forward shading only pays for the lamps around each pixel (ranges are 5
//!   to 25 m). Lamps farther than [`CULL_DISTANCE`] are hidden (fading out over the last
//!   [`FADE`] meters), so the lights Bevy clusters stay few; at night the fog hides most of that
//!   distance anyway. With the setting "on with shadows", the [`MAX_SHADOW_LAMPS`] nearest lamps
//!   within [`SHADOW_DISTANCE`] (those BF2 made cast dynamic shadows count as closer) cast
//!   shadows, switching with some hysteresis.
//! - BF2's baked lamp light (the lightmaps' red channel) isn't used: the real-time lamps reach as
//!   far as anyone sees at night, and mixing both would light surfaces twice or show a seam
//!   where one hands over to the other.

use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use bevy::{light::PointLightShadowMap, prelude::*};
use game_data::LampDesc;
use game_shared::{level::LoadedLevel, physics::GameLayer};
use serde::{Deserialize, Serialize};

use super::environment::{LIGHT_UNIT, LevelLight};
use crate::{camera::PlayerCamera, settings::Settings};

pub struct LampsPlugin;

impl Plugin for LampsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PointLightShadowMap { size: SHADOW_MAP_SIZE })
            .add_systems(
                Update,
                spawn_lamps.run_if(resource_exists_and_changed::<LevelLight>),
            )
            .add_systems(
                PostUpdate,
                update_lamps
                    .after(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate)
                    .run_if(resource_exists::<LevelLight>),
            );
    }
}

/// The "Dynamic lamps" setting.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DynamicLamps {
    Off,
    #[default]
    On,
    /// On, and the nearest few cast shadows.
    Shadows,
}

impl DynamicLamps {
    // `ALL` and `label` are for the settings page.
    #[allow(dead_code)]
    pub const ALL: [DynamicLamps; 3] = [DynamicLamps::Off, DynamicLamps::On, DynamicLamps::Shadows];

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            DynamicLamps::Off => "Off",
            DynamicLamps::On => "On",
            DynamicLamps::Shadows => "On with shadows",
        }
    }

    /// `off`, `on` or `shadows`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().replace(['_', '-', ' '], "").as_str() {
            "off" | "false" | "0" | "no" => Some(DynamicLamps::Off),
            "on" | "true" | "1" | "yes" => Some(DynamicLamps::On),
            "shadows" | "onwithshadows" => Some(DynamicLamps::Shadows),
            _ => None,
        }
    }
}

/// Lamps farther from the camera than this are hidden.
const CULL_DISTANCE: f32 = 220.0;
/// Lamps fade out over this many meters before [`CULL_DISTANCE`].
const FADE: f32 = 60.0;
/// At most this many lamps are lit at once (the nearest).
const MAX_LIT: usize = 192;
/// Lamps casting shadows at most, and how near they must be.
const MAX_SHADOW_LAMPS: usize = 2;
const SHADOW_DISTANCE: f32 = 35.0;
/// A lamp BF2 made cast dynamic shadows counts as this much closer when picking casters.
const SHADOW_PREFERENCE: f32 = 0.6;
/// A new caster replaces a current one only if it's this much closer (by score).
const SHADOW_HYSTERESIS: f32 = 0.75;
/// Side of each face of a lamp's shadow cube map (each caster redraws everything within its
/// range six times; lamp shadows are soft and near, so a small map does).
const SHADOW_MAP_SIZE: usize = 512;
/// Casters closer to the lamp than this don't shadow it (its own housing and glass).
const SHADOW_NEAR: f32 = 0.35;
/// Day levels light lamps that see less sky than this (0..1).
const INDOOR_SKY: f32 = 0.35;
/// Day levels measure lamps within this distance of the camera, this many per frame, and
/// measure those found outdoors again after this many seconds (the level's collision may not
/// have been complete).
const MEASURE_DISTANCE: f32 = 150.0;
const MEASURE_BUDGET: usize = 8;
const OUTDOOR_RECHECK: f32 = 5.0;
/// Rays longer than this count as reaching the sky.
const RAY_LENGTH: f32 = 40.0;
/// Strength a lamp is fitted with at most: BF2's strengths up to 6 mostly widened the fully lit
/// area of its saturating light; as inverse-square light they would blind.
const MAX_STRENGTH: f32 = 1.5;

/// One of the level's lamps as a light entity.
#[derive(Component)]
struct Lamp {
    /// Lumens at full strength.
    lumens: f32,
    /// BF2 made it cast dynamic shadows.
    preferred_caster: bool,
    /// Day levels: `None` until measured, then whether it's indoors, and when it was measured.
    indoors: Option<bool>,
    measured: f32,
    /// Lit wherever it is (night levels), not only indoors.
    always: bool,
}

/// Lumens of a point light whose inverse-square falloff (windowed to end at `far`, as Bevy
/// draws it) best matches BF2's baked falloff of this lamp, for a lamp colour of luminance
/// `lamp` (linear light on a surface facing it, 1 = [`LIGHT_UNIT`]).
fn lumens(desc: &LampDesc, lamp: f32) -> f32 {
    let [near, far] = desc.range;
    let far = far.max(near + 0.01).max(0.1);
    let strength = desc.strength.min(MAX_STRENGTH);
    let bf2 = |d: f32| (strength * (1.0 - ((d - near) / (far - near)).clamp(0.0, 1.0))).clamp(0.0, 1.0);
    let window = |d: f32| (1.0 - (d / far).powi(4)).clamp(0.0, 1.0).powi(2);
    // Illuminance per lumen at `d`: 1 / (4 pi d^2), windowed.
    let per_lumen = |d: f32| window(d) / (4.0 * std::f32::consts::PI * d * d);
    let (mut log_sum, mut n) = (0.0, 0);
    for i in 1..=8 {
        let d = far * (0.1 + 0.08 * i as f32);
        let target = bf2(d).powf(2.2) * lamp * LIGHT_UNIT * std::f32::consts::PI;
        if target > 1e-6 && per_lumen(d) > 1e-9 {
            log_sum += (target / per_lumen(d)).ln();
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { (log_sum / n as f32).exp() }
}

fn spawn_lamps(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    light: Res<LevelLight>,
    existing: Query<Entity, With<Lamp>>,
) {
    for entity in &existing {
        commands.entity(entity).despawn();
    }
    let lamps = &level.desc.environment.lamps;
    let brightness = light.lamp.max_element();
    if lamps.is_empty() || brightness <= 1e-6 {
        return;
    }
    let mut spawned = 0;
    // Lamp heads without a light of their own only light night versions of day levels.
    for desc in lamps.iter().filter(|l| !l.unlit || light.night_version) {
        let own = Vec3::from_array(desc.color).max(Vec3::ZERO).powf(2.2) * light.lamp;
        let peak = own.max_element();
        if peak <= 1e-6 {
            continue;
        }
        let color = own / peak;
        let lumens = lumens(desc, peak);
        if lumens <= 0.0 {
            continue;
        }
        let range = desc.range[1].max(0.5);
        let lamp = Lamp {
            lumens,
            preferred_caster: desc.shadows,
            indoors: None,
            measured: 0.0,
            always: light.night,
        };
        let position = Vec3::from_array(desc.position);
        let color = Color::linear_rgb(color.x, color.y, color.z);
        let mut entity = commands.spawn((
            lamp,
            Name::new("lamp"),
            Transform::from_translation(position),
            Visibility::Hidden,
        ));
        match desc.direction {
            Some(direction) => {
                let direction = Vec3::from_array(direction).normalize_or(Vec3::NEG_Y);
                entity.insert((
                    SpotLight {
                        color,
                        intensity: 0.0,
                        range,
                        radius: 0.1,
                        outer_angle: (desc.cone[1] * 0.5).to_radians().clamp(0.05, 1.5),
                        inner_angle: (desc.cone[0] * 0.5).to_radians().clamp(0.0, 1.5),
                        shadow_map_near_z: SHADOW_NEAR,
                        ..default()
                    },
                    Transform::from_translation(position).looking_to(direction, perpendicular(direction)),
                ));
            }
            None => {
                entity.insert(PointLight {
                    color,
                    intensity: 0.0,
                    range,
                    radius: 0.1,
                    shadow_map_near_z: SHADOW_NEAR,
                    ..default()
                });
            }
        }
        spawned += 1;
    }
    info!(
        "lamps: {spawned} of {} ({}), colour {:.3?}",
        lamps.len(),
        if light.night_version {
            "night version: all"
        } else if light.night {
            "night: all"
        } else {
            "day: indoors only"
        },
        light.lamp
    );
}

/// Some unit vector perpendicular to `v` (an up vector for `looking_to`).
fn perpendicular(v: Vec3) -> Vec3 {
    if v.y.abs() > 0.9 { Vec3::X } else { Vec3::Y }
}

/// Share of the sky a lamp at `origin` sees (a few rays up into the world's collision).
fn sky_visibility(spatial: &SpatialQuery, filter: &SpatialQueryFilter, origin: Vec3) -> f32 {
    const RAYS: [(f32, f32, f32); 5] = [(0.0, 90.0, 2.0), (0.0, 55.0, 1.0), (90.0, 55.0, 1.0), (180.0, 55.0, 1.0), (270.0, 55.0, 1.0)];
    let (mut open, mut total) = (0.0, 0.0);
    for (azimuth, elevation, weight) in RAYS {
        let (a, e) = (azimuth.to_radians(), elevation.to_radians());
        let direction = Vec3::new(e.cos() * a.cos(), e.sin(), e.cos() * a.sin());
        let hit = Dir3::new(direction)
            .ok()
            .and_then(|dir| spatial.cast_ray(origin, dir, RAY_LENGTH, false, filter));
        if hit.is_none() {
            open += weight;
        }
        total += weight;
    }
    open / total
}

/// A lamp's light entity, whichever kind it is.
#[derive(bevy::ecs::query::QueryData)]
#[query_data(mutable)]
struct LampLight {
    entity: Entity,
    lamp: &'static mut Lamp,
    transform: &'static Transform,
    visibility: &'static mut Visibility,
    point: Option<&'static mut PointLight>,
    spot: Option<&'static mut SpotLight>,
}

/// Shows the lamps near the camera at their faded brightness, hides the rest, and picks the
/// shadow casters.
#[allow(clippy::too_many_arguments)]
fn update_lamps(
    time: Res<Time>,
    settings: Res<Settings>,
    cli: Res<crate::Cli>,
    spatial: SpatialQuery,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    mut lamps: Query<LampLight>,
    mut casters: Local<Vec<Entity>>,
    mut logged: Local<Option<(usize, usize)>>,
) {
    let mode = settings.dynamic_lamps;
    let Ok(camera) = camera.single() else {
        return;
    };
    let eye = camera.translation();

    // Day levels: find out which lamps near the camera are indoors.
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    let now = time.elapsed_secs();
    let mut budget = MEASURE_BUDGET;
    let mut lit: Vec<(f32, Entity)> = Vec::new();
    for mut item in &mut lamps {
        let position = item.transform.translation;
        let distance = position.distance(eye);
        let stale = match item.lamp.indoors {
            None => true,
            Some(indoors) => !indoors && now - item.lamp.measured > OUTDOOR_RECHECK,
        };
        if mode != DynamicLamps::Off && !item.lamp.always && stale && distance < MEASURE_DISTANCE && budget > 0 {
            budget -= 1;
            // From just below the lamp, out of its own housing.
            let sky = sky_visibility(&spatial, &filter, position - Vec3::Y * 0.3);
            item.lamp.indoors = Some(sky < INDOOR_SKY);
            item.lamp.measured = now;
        }
        let wanted = mode != DynamicLamps::Off
            && (item.lamp.always || item.lamp.indoors == Some(true))
            && distance < CULL_DISTANCE;
        if wanted {
            lit.push((distance, item.entity));
        }
    }
    lit.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    lit.truncate(MAX_LIT);

    // Shadow casters: keep the current ones unless a candidate is clearly better.
    let shadows = mode == DynamicLamps::Shadows && settings.shadows_on(&cli);
    let score = |distance: f32, preferred: bool| distance * if preferred { SHADOW_PREFERENCE } else { 1.0 };
    let mut candidates: Vec<(f32, Entity)> = Vec::new();
    if shadows {
        for (distance, entity) in lit.iter().take_while(|(d, _)| *d < SHADOW_DISTANCE / SHADOW_PREFERENCE) {
            if let Ok(item) = lamps.get(*entity) {
                let s = score(*distance, item.lamp.preferred_caster);
                if s < SHADOW_DISTANCE {
                    candidates.push((s, *entity));
                }
            }
        }
        candidates.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let current: Vec<(f32, Entity)> = candidates.iter().filter(|(_, e)| casters.contains(e)).copied().collect();
        let mut chosen: Vec<(f32, Entity)> = current.clone();
        for candidate in &candidates {
            if chosen.iter().any(|(_, e)| *e == candidate.1) {
                continue;
            }
            if chosen.len() < MAX_SHADOW_LAMPS {
                chosen.push(*candidate);
                continue;
            }
            // Replace the worst current caster if this one is clearly better.
            let (worst, _) = chosen
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
                .map(|(i, c)| (i, c.0))
                .unwrap();
            if candidate.0 < chosen[worst].0 * SHADOW_HYSTERESIS {
                chosen[worst] = *candidate;
            }
        }
        *casters = chosen.into_iter().map(|(_, e)| e).collect();
    } else {
        casters.clear();
    }

    let lit_set: bevy::platform::collections::HashMap<Entity, f32> = lit.iter().map(|(d, e)| (*e, *d)).collect();
    for mut item in &mut lamps {
        let Some(distance) = lit_set.get(&item.entity).copied() else {
            if *item.visibility != Visibility::Hidden {
                *item.visibility = Visibility::Hidden;
            }
            continue;
        };
        let fade = ((CULL_DISTANCE - distance) / FADE).clamp(0.0, 1.0);
        let intensity = item.lamp.lumens * fade * fade * (3.0 - 2.0 * fade);
        let cast = casters.contains(&item.entity);
        if let Some(point) = item.point.as_mut() {
            if (point.intensity - intensity).abs() > intensity * 0.01 + 1e-3 {
                point.intensity = intensity;
            }
            if point.shadow_maps_enabled != cast {
                point.shadow_maps_enabled = cast;
            }
        }
        if let Some(spot) = item.spot.as_mut() {
            if (spot.intensity - intensity).abs() > intensity * 0.01 + 1e-3 {
                spot.intensity = intensity;
            }
            if spot.shadow_maps_enabled != cast {
                spot.shadow_maps_enabled = cast;
            }
        }
        if *item.visibility != Visibility::Visible {
            *item.visibility = Visibility::Visible;
        }
    }

    let (last_lit, last_casters) = logged.unwrap_or((usize::MAX, usize::MAX));
    if lit.len().abs_diff(last_lit) >= 10 || casters.len() != last_casters {
        debug!("lamps: {} lit, {} casting shadows", lit.len(), casters.len());
        *logged = Some((lit.len(), casters.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lamp(strength: f32, range: [f32; 2]) -> LampDesc {
        LampDesc {
            position: [0.0; 3],
            direction: None,
            color: [1.0; 3],
            strength,
            range,
            cone: [0.0; 2],
            shadows: false,
            unlit: false,
        }
    }

    #[test]
    fn lumens_follow_bf2() {
        // Brighter lamps and lamps reaching farther need more lumens; none at zero strength.
        let a = lumens(&lamp(1.0, [0.5, 20.0]), 1.0);
        let b = lumens(&lamp(1.0, [0.5, 8.0]), 1.0);
        let c = lumens(&lamp(0.5, [0.5, 20.0]), 1.0);
        assert!(a > b && a > c && b > 0.0 && c > 0.0, "{a} {b} {c}");
        assert_eq!(lumens(&lamp(0.0, [0.5, 20.0]), 1.0), 0.0);
        // At 7 m from a 20 m lamp, about BF2's light: (1 - 6.5 / 19.5)^2.2 of the lamp colour.
        let at = |d: f32| a * (1.0 - (d / 20.0f32).powi(4)).powi(2) / (4.0 * std::f32::consts::PI * d * d);
        let bf2 = (1.0f32 - 6.5 / 19.5).powf(2.2) * LIGHT_UNIT * std::f32::consts::PI;
        let ratio = at(7.0) / bf2;
        assert!((0.5..2.0).contains(&ratio), "{ratio}");
    }

    #[test]
    fn setting_names() {
        assert_eq!(DynamicLamps::parse("on with shadows"), Some(DynamicLamps::Shadows));
        assert_eq!(DynamicLamps::parse("off"), Some(DynamicLamps::Off));
        assert_eq!(DynamicLamps::parse("ON"), Some(DynamicLamps::On));
        assert_eq!(DynamicLamps::parse("bright"), None);
    }
}
