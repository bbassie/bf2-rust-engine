//! Footsteps of every soldier, landings and ladder rungs.
//!
//! BF2 picks a soldier's movement sounds from the material manager cell of its feet
//! (material 5500) and the surface under them: walk, run, sprint or prone (`sounds.ron`
//! footsteps). The surface is whatever a ray straight down from the feet hits: the terrain's
//! material map or a static mesh's material (`levels/<name>/surfaces.ron`), water below the
//! water line. Steps come every stride of the soldier's gait; landing from a jump plays a
//! step too (BF2's `sound:0_first` event at the end of its jump animations). Our soldier
//! pants when sprinting has used up its stamina.

use std::collections::HashMap;

use avian3d::prelude::*;
use bevy::prelude::*;
use game_data::FootstepSounds;
use game_shared::{
    level::{LoadedLevel, Terrain},
    physics::GameLayer,
    soldier::{Soldier, SoldierMotion, Stance},
};

use super::{AudioSystems, Sounds, voices::{PlaySound, Sound}};
use crate::{effects::SurfaceQuery, net::LocalSoldier, prediction::SoldierRender};

pub struct FootstepPlugin;

impl Plugin for FootstepPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, (footsteps, breath).in_set(AudioSystems::Trigger));
    }
}

/// Meters per step [inferred from BF2's animation speeds: about 2.8 steps a second running
/// at 3.9 m/s, 3.2 sprinting at 7 m/s, 2 crouching at 2 m/s].
const WALK_STRIDE: f32 = 0.9;
const RUN_STRIDE: f32 = 1.4;
const SPRINT_STRIDE: f32 = 2.2;
const CRAWL_STRIDE: f32 = 0.6;
/// Standing soldiers faster than this run, faster than `SPRINT_SPEED` sprint (m/s).
const RUN_SPEED: f32 = 2.8;
const SPRINT_SPEED: f32 = 5.4;
/// Meters climbed per rung sound.
const RUNG: f32 = 0.45;
/// Seconds in the air before touching down counts as a landing.
const LANDING_AIRTIME: f32 = 0.3;
/// Steps farther away than this would be inaudible anyway (their falloff gives about 3 %).
const HEARING_RANGE: f32 = 60.0;
/// Our soldier pants (BF2's kit `pantingSound`) once sprinting takes the stamina below
/// `OUT_OF_BREATH`, and again only after it recovered above `CAUGHT_BREATH` [our choice].
const OUT_OF_BREATH: f32 = 0.15;
const CAUGHT_BREATH: f32 = 0.5;
/// Material ids (`materials.ron`).
const WATER: u32 = 1;
const DIRT: u32 = 8;
const CONCRETE: u32 = 78;

#[derive(Clone, Copy, Debug)]
enum Gait {
    Walk,
    Run,
    Sprint,
    Prone,
}

impl Gait {
    fn stride(self) -> f32 {
        match self {
            Gait::Walk => WALK_STRIDE,
            Gait::Run => RUN_STRIDE,
            Gait::Sprint => SPRINT_STRIDE,
            Gait::Prone => CRAWL_STRIDE,
        }
    }

    fn pick(self, sounds: &FootstepSounds) -> Option<&String> {
        match self {
            Gait::Walk => sounds.walk.as_ref(),
            Gait::Run => sounds.run.as_ref().or(sounds.walk.as_ref()),
            Gait::Sprint => sounds.sprint.as_ref().or(sounds.run.as_ref()).or(sounds.walk.as_ref()),
            Gait::Prone => sounds.prone.as_ref(),
        }
    }
}

#[derive(Default)]
struct Steps {
    /// Meters since the last step.
    travelled: f32,
    airborne: f32,
    climbed: f32,
}

#[allow(clippy::too_many_arguments)]
fn footsteps(
    time: Res<Time>,
    library: Res<Sounds>,
    level: Option<Res<LoadedLevel>>,
    spatial: SpatialQuery,
    surfaces: SurfaceQuery,
    terrain: Query<(), With<Terrain>>,
    listener: Query<&Transform, With<SpatialListener>>,
    soldiers: Query<(Entity, &SoldierRender, Option<&SoldierMotion>), With<Soldier>>,
    mut steps: Local<HashMap<Entity, Steps>>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let dt = time.delta_secs();
    let Some(ear) = listener.iter().next().map(|t| t.translation) else {
        return;
    };
    let water = level.as_ref().and_then(|l| l.desc.water.as_ref()).map(|w| w.height);
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    // What the soldier stands on, and whether that's the terrain.
    let surface = |feet: Vec3| -> (u32, bool) {
        if water.is_some_and(|h| feet.y < h - 0.05) {
            return (WATER, false);
        }
        match spatial.cast_ray(feet + Vec3::Y * 0.5, Dir3::NEG_Y, 1.5, true, &filter) {
            Some(hit) => {
                let point = feet + Vec3::Y * 0.5 - Vec3::Y * hit.distance;
                (surfaces.material(hit.entity, point), terrain.contains(hit.entity))
            }
            None => (DIRT, true),
        }
    };
    let footsteps = &library.0.footsteps;
    let step = |entity: Entity, feet: Vec3, gait: Gait| -> Option<PlaySound> {
        let (material, on_terrain) = surface(feet);
        let fallback = if on_terrain { DIRT } else { CONCRETE };
        let name = footsteps
            .get(&material)
            .and_then(|s| gait.pick(s))
            .or_else(|| footsteps.get(&fallback).and_then(|s| gait.pick(s)))?;
        Some(PlaySound::at(Sound::Named(name.clone()), feet).emitter(entity))
    };

    for (entity, render, motion) in &soldiers {
        let state = steps.entry(entity).or_insert_with(|| Steps {
            // Soldiers don't step in unison.
            travelled: fastrand::f32() * RUN_STRIDE,
            ..default()
        });
        if render.position.distance(ear) > HEARING_RANGE {
            continue;
        }
        let climbing = motion.is_some_and(|m| m.climbing);
        if climbing {
            state.climbed += render.velocity.y.abs() * dt;
            if state.climbed >= RUNG {
                state.climbed -= RUNG;
                if let Some(ladder) = &library.0.soldier.ladder {
                    let at = render.position + Vec3::Y;
                    sounds.write(PlaySound::at(Sound::Named(ladder.clone()), at).emitter(entity).reason("ladder"));
                }
            }
            continue;
        }
        if !render.grounded {
            state.airborne += dt;
            continue;
        }
        if state.airborne > LANDING_AIRTIME {
            if let Some(sound) = step(entity, render.position, Gait::Run) {
                sounds.write(sound.volume(1.2).reason("landing"));
            }
            state.travelled = 0.0;
        }
        state.airborne = 0.0;

        let speed = render.velocity.xz().length();
        let gait = match render.stance {
            Stance::Prone => Gait::Prone,
            Stance::Crouching => Gait::Walk,
            Stance::Standing if speed > SPRINT_SPEED => Gait::Sprint,
            Stance::Standing if speed > RUN_SPEED => Gait::Run,
            Stance::Standing => Gait::Walk,
        };
        if speed < 0.3 {
            // The first step comes soon after starting to move.
            state.travelled = state.travelled.max(gait.stride() * 0.6);
            continue;
        }
        state.travelled += speed * dt;
        if state.travelled >= gait.stride() {
            state.travelled = (state.travelled - gait.stride()).min(gait.stride());
            let reason = match gait {
                Gait::Walk => "walk",
                Gait::Run => "run",
                Gait::Sprint => "sprint",
                Gait::Prone => "crawl",
            };
            if let Some(sound) = step(entity, render.position, gait) {
                sounds.write(sound.reason(reason));
            }
        }
    }
    steps.retain(|entity, _| soldiers.contains(*entity));
}

/// Our soldier out of breath after sprinting.
fn breath(
    library: Res<Sounds>,
    soldier: Query<(Entity, &SoldierMotion), With<LocalSoldier>>,
    mut panting: Local<bool>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let Ok((entity, motion)) = soldier.single() else {
        *panting = false;
        return;
    };
    if *panting {
        *panting = motion.stamina < CAUGHT_BREATH;
    } else if motion.stamina < OUT_OF_BREATH {
        *panting = true;
        if let Some(breath) = &library.0.soldier.sprint_breath {
            sounds.write(PlaySound::local(Sound::Named(breath.clone())).emitter(entity).reason("out of breath"));
        }
    }
}
