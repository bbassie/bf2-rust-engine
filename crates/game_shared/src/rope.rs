//! Ropes strung by BF2 SF's gadgets: the grappling hook's rope hanging over a ledge, climbed
//! like a ladder, and the zipline crossbow's wire from the shooter to where the bolt hit,
//! slid down (see [`crate::soldier::step_soldier`]).
//!
//! The server strings a rope where a rope-carrying projectile ([`ProjectileDesc::rope`])
//! sticks and takes it down after its lifetime. [`Rope`] is replicated; on both sides it
//! gets a collider on [`GameLayer::Rope`] or [`GameLayer::Zipline`], which movement finds
//! through spatial queries, so prediction sees the same rope as the server.
//!
//! [`ProjectileDesc::rope`]: game_data::ProjectileDesc::rope

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::RopeKind;
use serde::{Deserialize, Serialize};

use crate::{
    physics::GameLayer,
    projectile::{Projectile, ProjectileMotion},
    protocol::ControlledBy,
    soldier::{Soldier, SoldierMotion},
    weapons::Armory,
};

pub struct RopePlugin;

impl Plugin for RopePlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(add_rope_collider).add_systems(
            FixedPostUpdate,
            (
                // Tests and tools may run movement without weapons.
                string_ropes.run_if(resource_exists::<Armory>),
                take_down_ropes,
            )
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// A rope in the world. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Rope {
    pub kind: RopeKind,
    /// Grappling rope: where the hook holds. Zipline: the shooter's end, on top of its stand.
    pub anchor: Vec3,
    /// Grappling rope: where it goes over the ledge, the top of the part hanging down.
    /// Zipline: the anchor.
    pub top: Vec3,
    /// Grappling rope: the bottom of the hanging part. Zipline: where the bolt hit.
    pub end: Vec3,
}

impl Rope {
    /// The part soldiers hold on to: the hanging part, or the whole wire.
    pub fn climbed(&self) -> (Vec3, Vec3) {
        (self.top, self.end)
    }

    /// Where the rope entity (and its collider) is: the middle of the climbable part, a
    /// grappling rope's +Z away from the wall, a zipline's Y along the wire.
    pub fn transform(&self) -> Transform {
        let (top, end) = self.climbed();
        let rotation = match self.kind {
            RopeKind::Grapple => {
                let out = Vec3::new(top.x - self.anchor.x, 0.0, top.z - self.anchor.z)
                    .normalize_or(Vec3::Z);
                Quat::from_rotation_arc(Vec3::Z, out)
            }
            RopeKind::Zipline => {
                Quat::from_rotation_arc(Vec3::Y, (end - top).normalize_or(Vec3::Y))
            }
        };
        Transform::from_translation((top + end) * 0.5).with_rotation(rotation)
    }
}

/// Server-side: whose rope it is and how long it stays.
#[derive(Component, Debug)]
pub struct RopeLife {
    pub owner: Entity,
    pub remaining: f32,
}

/// Height of a zipline's stand: where the wire starts above the shooter's feet.
pub const ZIPLINE_STAND: f32 = 2.3;
/// Distance of a grappling rope's hanging part from the wall it hangs down.
const OFF_WALL: f32 = 0.35;
/// Grappling ropes shorter than this aren't worth climbing.
const MIN_CLIMB: f32 = 1.5;

/// Half the width of a grappling rope's box: how far to either side of it a soldier can
/// still grab it (BF2's `attachClimberRadius` is 2 m).
pub const ROPE_HALF_WIDTH: f32 = 0.3;

/// Gives a rope its collider for movement queries. Grappling ropes are boxes like ladders,
/// climbed on their +Z side (away from the wall); ziplines a thin capsule along the wire.
fn add_rope_collider(add: On<Add, Rope>, mut commands: Commands, ropes: Query<&Rope>) {
    let Ok(rope) = ropes.get(add.entity) else {
        return;
    };
    let (top, end) = rope.climbed();
    let (collider, layer) = match rope.kind {
        RopeKind::Grapple => (
            Collider::cuboid(2.0 * ROPE_HALF_WIDTH, (top.y - end.y).max(0.1), 0.04),
            GameLayer::Rope,
        ),
        RopeKind::Zipline => (
            Collider::capsule(0.05, top.distance(end)),
            GameLayer::Zipline,
        ),
    };
    commands.entity(add.entity).insert((
        RigidBody::Static,
        collider,
        rope.transform(),
        CollisionLayers::new(layer, LayerMask::NONE),
    ));
}

/// Where a rope projectile came to a stop, strings its rope (replacing the owner's earlier
/// one of the kind), or drops it if there is nothing to climb.
fn string_ropes(
    mut commands: Commands,
    armory: Res<Armory>,
    spatial: SpatialQuery,
    projectiles: Query<(Entity, &Projectile, &ProjectileMotion), Changed<ProjectileMotion>>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), With<Soldier>>,
    ropes: Query<(Entity, &Rope, &RopeLife)>,
) {
    for (entity, projectile, motion) in &projectiles {
        if !motion.resting {
            continue;
        }
        let Some(desc) = armory
            .weapon(&projectile.weapon)
            .and_then(|w| w.projectile.rope)
        else {
            continue;
        };
        commands.entity(entity).despawn();
        let Some(shooter) = soldiers
            .iter()
            .find(|(_, c)| c.0 == projectile.player)
            .map(|(m, _)| m.position)
        else {
            continue;
        };
        let rope = match desc.kind {
            RopeKind::Grapple => grapple(&spatial, motion.position, shooter, desc.max_length),
            RopeKind::Zipline => {
                let start = shooter + Vec3::Y * ZIPLINE_STAND;
                (start.distance(motion.position) <= desc.max_length).then_some(Rope {
                    kind: RopeKind::Zipline,
                    anchor: start,
                    top: start,
                    end: motion.position,
                })
            }
        };
        let Some(rope) = rope else {
            info!(
                "{}: nothing to string a rope over at {:.1}",
                projectile.weapon, motion.position
            );
            continue;
        };
        for (old, other, life) in &ropes {
            if life.owner == projectile.player && other.kind == rope.kind {
                commands.entity(old).despawn();
            }
        }
        info!(
            "{} strung a {:?} rope {:.1} -> {:.1} -> {:.1}",
            projectile.weapon, rope.kind, rope.anchor, rope.top, rope.end
        );
        commands.spawn((
            rope,
            RopeLife {
                owner: projectile.player,
                remaining: desc.lifetime,
            },
            Replicated,
        ));
    }
}

fn take_down_ropes(
    mut commands: Commands,
    time: Res<Time>,
    mut ropes: Query<(Entity, &mut RopeLife)>,
) {
    for (entity, mut life) in &mut ropes {
        life.remaining -= time.delta_secs();
        if life.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// A grappling rope from a hook at `hook`, thrown from `thrower`. Pulled on, the hook drags
/// back towards the thrower until it catches on the ledge (BF2's rope tugs its links the same
/// way), and the rope hangs straight down from there, to the ground or as far as it reaches.
/// `None` when there is no drop between the hook and the thrower: nothing to climb.
pub fn grapple(spatial: &SpatialQuery, hook: Vec3, thrower: Vec3, max_length: f32) -> Option<Rope> {
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    let out = Vec3::new(thrower.x - hook.x, 0.0, thrower.z - hook.z).normalize_or(Vec3::Z);
    let flat_distance = Vec3::new(thrower.x - hook.x, 0.0, thrower.z - hook.z).length();
    const STEP: f32 = 0.1;
    let mut lip = None;
    let mut surface = hook;
    for i in 1..=(flat_distance / STEP) as usize {
        let probe = hook + out * (i as f32 * STEP);
        // Follows a roof sloping a little; a drop of more than 1 m is the ledge.
        match spatial.cast_ray(
            Vec3::new(probe.x, surface.y + 0.5, probe.z),
            Dir3::NEG_Y,
            1.5,
            true,
            &filter,
        ) {
            Some(hit) => surface = Vec3::new(probe.x, surface.y + 0.5 - hit.distance, probe.z),
            None => {
                lip = Some(surface);
                break;
            }
        }
    }
    let lip = lip?;
    let top = lip + out * OFF_WALL;
    let end = match spatial.cast_ray(top, Dir3::NEG_Y, max_length, true, &filter) {
        Some(hit) => top - Vec3::Y * hit.distance,
        None => top - Vec3::Y * max_length,
    };
    (top.y - end.y >= MIN_CLIMB).then_some(Rope {
        kind: RopeKind::Grapple,
        anchor: lip,
        top,
        end,
    })
}
