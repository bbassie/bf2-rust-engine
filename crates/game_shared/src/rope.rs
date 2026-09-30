//! Ropes strung by BF2 SF's gadgets: the grappling hook's rope hanging over a ledge, climbed
//! like a ladder, and the zipline crossbow's wire from the shooter to where the bolt hit,
//! slid down (see [`crate::soldier::step_soldier`]).
//!
//! The grappling hook flies as a projectile. Where it holds (on a surface at most 60° steep,
//! BF2's `minYNormal 0.5`, or over the top of a wall it hits just below the top) the server
//! looks for the lip it catches on, dragging it back
//! towards the thrower like BF2's rope tugs its links, and strings a rope from there; with
//! nothing to hook over, the hook lies where it fell for a moment and is gone.
//!
//! [`Rope`] is replicated; on both sides it gets a collider on [`GameLayer::Rope`] or
//! [`GameLayer::Zipline`], which movement finds through spatial queries, so prediction sees
//! the same rope as the server. What soldiers climb is the straight line the settled rope
//! hangs along; clients draw the rope as BF2 builds it, a chain of links simulated by
//! [`RopeSim`] that swings, drapes over the edge, piles up on the ground and goes to sleep.
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
                (catch_on_walls, string_ropes).chain().run_if(resource_exists::<Armory>),
                take_down_ropes,
                clear_missed_hooks,
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
    /// Grappling rope: its whole length from the hook (BF2's `setMaxRopeLength`); what the
    /// hanging part doesn't need lies on the ground.
    pub length: f32,
    /// Grappling rope: the links it is made of (`setNumberOfLinks`).
    pub links: u16,
}

impl Rope {
    /// The part soldiers hold on to: the hanging part, or the whole wire.
    pub fn climbed(&self) -> (Vec3, Vec3) {
        (self.top, self.end)
    }

    /// Horizontal, from the wall out towards where it is climbed from (grappling ropes).
    pub fn out(&self) -> Vec3 {
        Vec3::new(self.top.x - self.anchor.x, 0.0, self.top.z - self.anchor.z).normalize_or(Vec3::Z)
    }

    /// Where the rope entity (and its collider) is: the middle of the climbable part, a
    /// grappling rope's +Z away from the wall, a zipline's Y along the wire.
    pub fn transform(&self) -> Transform {
        let (top, end) = self.climbed();
        let rotation = match self.kind {
            RopeKind::Grapple => Quat::from_rotation_arc(Vec3::Z, self.out()),
            RopeKind::Zipline => {
                Quat::from_rotation_arc(Vec3::Y, (end - top).normalize_or(Vec3::Y))
            }
        };
        Transform::from_translation((top + end) * 0.5).with_rotation(rotation)
    }

    /// The line the settled rope hangs along, as `links + 1` points from the hook: over the
    /// edge, straight down the climbable part, and what is left lying on the ground away
    /// from the wall. Clients start drawing a rope from this when they didn't see it thrown.
    pub fn settled_points(&self) -> Vec<Vec3> {
        let links = self.links.max(1) as usize;
        let link = self.length / links as f32;
        let out = self.out();
        let path = [self.anchor, self.top, self.end];
        let path_length: f32 = path.windows(2).map(|w| w[0].distance(w[1])).sum();
        (0..=links)
            .map(|i| {
                let wanted = i as f32 * link;
                let mut start = 0.0;
                for pair in path.windows(2) {
                    let length = pair[0].distance(pair[1]);
                    if wanted <= start + length && length > 1e-4 {
                        return pair[0].lerp(pair[1], (wanted - start) / length);
                    }
                    start += length;
                }
                self.end + out * (wanted - path_length)
            })
            .collect()
    }
}

/// Server-side: whose rope it is and how long it stays.
#[derive(Component, Debug)]
pub struct RopeLife {
    pub owner: Entity,
    pub remaining: f32,
}

/// Server-side: a grappling hook that found nothing to hold on to, lying where it fell for
/// this many more seconds.
#[derive(Component, Debug)]
pub struct MissedHook(pub f32);

/// Height of a zipline's stand: where the wire starts above the shooter's feet.
pub const ZIPLINE_STAND: f32 = 2.3;
/// Distance of a grappling rope's hanging part from the wall it hangs down.
pub const OFF_WALL: f32 = 0.25;
/// Grappling ropes shorter than this aren't worth climbing.
const MIN_CLIMB: f32 = 1.5;
/// Seconds a hook that didn't catch lies on the ground before it is gone.
const MISSED_HOOK_TIME: f32 = 2.5;
/// The drop behind a lip the hook catches on, at least (m).
const LIP_DROP: f32 = 1.0;
/// The hook is dragged over parapets up to this high (m).
const PARAPET_REACH: f32 = 2.0;
/// Something sticking out of the wall below a lip less far than this, with at least this
/// much room under it, is hung (and climbed) past (m).
const OVERHANG_REACH: f32 = 0.6;
const OVERHANG_ROOM: f32 = 1.8;

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
/// one of the kind: one rope of each kind per player, like BF2), or leaves a grappling hook
/// that found nothing to hold on to lying there for a moment.
#[allow(clippy::type_complexity)]
fn string_ropes(
    mut commands: Commands,
    armory: Res<Armory>,
    spatial: SpatialQuery,
    projectiles: Query<(Entity, &Projectile, &ProjectileMotion), (Changed<ProjectileMotion>, Without<MissedHook>)>,
    soldiers: Query<(&SoldierMotion, &ControlledBy), With<Soldier>>,
    ropes: Query<(Entity, &Rope, &RopeLife)>,
) {
    for (entity, projectile, motion) in &projectiles {
        if !motion.resting {
            continue;
        }
        let Some(desc) = armory
            .weapon(&projectile.weapon)
            .and_then(|w| w.projectile.rope.as_ref())
        else {
            continue;
        };
        let shooter = soldiers
            .iter()
            .find(|(_, c)| c.0 == projectile.player)
            .map(|(m, _)| m.position);
        let rope = shooter.and_then(|shooter| match desc.kind {
            RopeKind::Grapple => grapple(&spatial, motion.position, shooter, desc.max_length).map(|rope| Rope {
                links: desc.links.clamp(1, 200) as u16,
                ..rope
            }),
            RopeKind::Zipline => {
                let start = shooter + Vec3::Y * ZIPLINE_STAND;
                (start.distance(motion.position) <= desc.max_length).then_some(Rope {
                    kind: RopeKind::Zipline,
                    anchor: start,
                    top: start,
                    end: motion.position,
                    length: start.distance(motion.position),
                    links: 1,
                })
            }
        });
        let Some(rope) = rope else {
            info!(
                "{}: nothing to string a rope over at {:.1}",
                projectile.weapon, motion.position
            );
            match desc.kind {
                RopeKind::Grapple => {
                    commands.entity(entity).insert(MissedHook(MISSED_HOOK_TIME));
                }
                RopeKind::Zipline => commands.entity(entity).despawn(),
            }
            continue;
        };
        commands.entity(entity).despawn();
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

/// A flying grappling hook about to hit a wall just below its top catches over the top, as
/// BF2's thrown rope links do: the hook lands on the lip (and [`string_ropes`] strings the
/// rope from there). Lower down it bounces off and falls back.
fn catch_on_walls(
    armory: Res<Armory>,
    spatial: SpatialQuery,
    mut hooks: Query<(&Projectile, &mut ProjectileMotion), Without<MissedHook>>,
) {
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    for (projectile, mut motion) in &mut hooks {
        if motion.resting
            || !armory
                .weapon(&projectile.weapon)
                .and_then(|w| w.projectile.rope.as_ref())
                .is_some_and(|r| r.kind == RopeKind::Grapple)
        {
            continue;
        }
        let Ok(direction) = Dir3::new(motion.velocity) else {
            continue;
        };
        let reach = motion.velocity.length() * WALL_LOOKAHEAD;
        let Some(wall) = spatial.cast_ray(motion.position, direction, reach, true, &filter) else {
            continue;
        };
        if wall.normal.y >= 0.5 {
            // Something it holds on to by itself.
            continue;
        }
        let into = -Vec3::new(wall.normal.x, 0.0, wall.normal.z).normalize_or_zero();
        let probe = motion.position + direction * wall.distance + into * 0.15 + Vec3::Y * WALL_CATCH;
        let Some(top) = spatial.cast_ray(probe, Dir3::NEG_Y, WALL_CATCH, true, &filter) else {
            continue;
        };
        if top.distance < 1e-3 || top.normal.y < 0.5 {
            continue;
        }
        motion.position = probe - Vec3::Y * (top.distance - 0.02);
        motion.velocity = Vec3::ZERO;
        motion.rotation = Quat::from_rotation_arc(Vec3::Y, top.normal) * motion.rotation;
        motion.resting = true;
        info!("{}: caught the top of a wall at {:.1}", projectile.weapon, motion.position);
    }
}

/// How far ahead of a flying hook (seconds of flight) walls are looked for, and how far
/// below its top a wall it hits still lets it catch over it (m).
const WALL_LOOKAHEAD: f32 = 2.5 / 60.0;
const WALL_CATCH: f32 = 1.0;

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

fn clear_missed_hooks(mut commands: Commands, time: Res<Time>, mut hooks: Query<(Entity, &mut MissedHook)>) {
    for (entity, mut hook) in &mut hooks {
        hook.0 -= time.delta_secs();
        if hook.0 <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// A grappling rope from a hook that holds at `hook`, thrown from `thrower`. Pulled on, the
/// hook drags back towards the thrower, along the roof (a sloping one too), until it catches
/// on a lip with a drop of at least a meter behind it (BF2's rope tugs its links the same
/// way), and the rope hangs straight down from there, to the ground or as far as it reaches.
/// `None` when there is no such drop between the hook and the thrower: nothing to climb.
pub fn grapple(spatial: &SpatialQuery, hook: Vec3, thrower: Vec3, max_length: f32) -> Option<Rope> {
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    let out = Vec3::new(thrower.x - hook.x, 0.0, thrower.z - hook.z).normalize_or(Vec3::Z);
    let flat_distance = Vec3::new(thrower.x - hook.x, 0.0, thrower.z - hook.z).length();
    const STEP: f32 = 0.1;
    let mut lip = None;
    let mut surface = hook;
    for i in 1..=(flat_distance.min(max_length) / STEP) as usize {
        let probe = hook + out * (i as f32 * STEP);
        let down = |above: f32| {
            spatial.cast_ray(Vec3::new(probe.x, surface.y + above, probe.z), Dir3::NEG_Y, above + LIP_DROP, true, &filter)
        };
        // A parapet (or a step up) in the way: onto its top, up to 2 m higher; a higher wall
        // holds the hook back and there's nothing to hook over. (Rays find the faces of the
        // level's meshes, not their insides: this looks for the wall, then its top.)
        let wall = spatial.cast_ray(Vec3::new(surface.x, surface.y + 0.25, surface.z), Dir3::new(out).ok()?, STEP, true, &filter);
        if wall.is_some() {
            match down(PARAPET_REACH) {
                Some(hit) if surface.y + PARAPET_REACH - hit.distance > surface.y + 0.2 && hit.distance > 1e-3 => {
                    surface = Vec3::new(probe.x, surface.y + PARAPET_REACH - hit.distance, probe.z);
                    continue;
                }
                _ => return None,
            }
        }
        // Follows a roof sloping up to 60°; a drop of more than a meter is the ledge.
        match down(0.5) {
            Some(hit) => surface = Vec3::new(probe.x, surface.y + 0.5 - hit.distance, probe.z),
            None => {
                lip = Some(surface);
                break;
            }
        }
    }
    let lip = lip?;
    let top = lip + out * OFF_WALL;
    // The hanging part reaches as far down as the rope, less what lies over the roof.
    // The hook holds at the lip: all but the bit over the edge hangs down.
    let hanging = max_length - OFF_WALL;
    let down = |from: Vec3| spatial.cast_ray(from, Dir3::NEG_Y, hanging, true, &filter).map(|hit| from.y - hit.distance);
    let mut end = top - Vec3::Y * down(top).map_or(hanging, |y| top.y - y);
    // A cornice, sill or awning sticking out of the wall below the lip: the rope drapes over
    // it and on down to the ground, and soldiers climb past it, if a little further out
    // there is room to stand under it.
    if let Some(ground) = down(top + out * OVERHANG_REACH)
        && ground < end.y - OVERHANG_ROOM
        && top.y - ground <= hanging
    {
        end.y = ground;
    }
    (top.y - end.y >= MIN_CLIMB).then_some(Rope {
        kind: RopeKind::Grapple,
        anchor: lip,
        top,
        end,
        length: max_length,
        links: 26,
    })
}

/// A rope as a chain of points `link` apart (BF2's rope links), simulated with Verlet
/// integration and position constraints: gravity, air friction, links that don't stretch
/// (but go slack), collisions with the world, pins (the hook, a climber's hands). Cheap and
/// stable at any step; after a few seconds, or once it has settled, it goes to sleep.
#[derive(Clone, Debug)]
pub struct RopeSim {
    pub points: Vec<Vec3>,
    previous: Vec<Vec3>,
    /// Rest length of each link.
    pub link: f32,
    /// Distance kept from surfaces.
    pub radius: f32,
    /// Points held in place this step.
    pub pins: Vec<Pin>,
    /// Share of the speed kept every 1/30 s in the air (BF2's `airFriction`).
    pub air_friction: f32,
    /// Share of the speed into a surface kept bouncing off it (BF2's `elasticity`).
    pub elasticity: f32,
    /// Seconds it moves after being woken before it sleeps anyway (BF2's `AwakeTime`).
    pub awake_time: f32,
    /// A strung grappling rope: the line its hanging part settles along.
    pub hang: Option<HangLine>,
    awake: f32,
    still: f32,
}

/// The vertical line the hanging part of a strung grappling rope settles along (the line
/// soldiers climb, [`Rope::climbed`]): links between `bottom` and `top` height are drawn
/// towards it horizontally at `rate` (1/s), like BF2 drops the thrown links straight down
/// from the hook (`dropStrength`). What lies on the ground stays where it fell.
#[derive(Clone, Copy, Debug)]
pub struct HangLine {
    pub top: Vec3,
    pub bottom: f32,
    pub rate: f32,
}

/// A point of a [`RopeSim`] held at `position`: exactly with `stiffness` 1, pulled that
/// share of the way there every iteration below.
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    pub index: usize,
    pub position: Vec3,
    pub stiffness: f32,
}

/// Gravity on rope links: BF2's world gravity.
pub const ROPE_GRAVITY: f32 = crate::physics::WORLD_GRAVITY;
/// Constraint iterations per step.
const ROPE_ITERATIONS: usize = 12;
/// Share of the speed along a surface lost every step touching it.
const ROPE_CONTACT_FRICTION: f32 = 0.3;
/// Moving slower than this (m/s) everywhere for `ROPE_STILL_TIME` seconds, it sleeps.
const ROPE_STILL_SPEED: f32 = 0.03;
const ROPE_STILL_TIME: f32 = 0.5;
/// How far a link cutting through a corner pushes its ends out per step.
const ROPE_WRAP_PUSH: f32 = 0.01;

impl RopeSim {
    pub fn new(points: Vec<Vec3>, link: f32, radius: f32, air_friction: f32, elasticity: f32, awake_time: f32) -> Self {
        Self {
            previous: points.clone(),
            points,
            link,
            radius,
            pins: Vec::new(),
            air_friction,
            elasticity,
            awake_time,
            hang: None,
            awake: awake_time,
            still: 0.0,
        }
    }

    /// Sets each point's velocity (m/s) as if it moved that way over the last `dt`.
    pub fn set_velocities(&mut self, dt: f32, velocity: impl Fn(usize) -> Vec3) {
        for (i, (point, previous)) in self.points.iter().zip(&mut self.previous).enumerate() {
            *previous = *point - velocity(i) * dt;
        }
    }

    pub fn velocity(&self, index: usize, dt: f32) -> Vec3 {
        (self.points[index] - self.previous[index]) / dt
    }

    pub fn asleep(&self) -> bool {
        self.awake <= 0.0
    }

    /// Moves again for up to [`Self::awake_time`] from now.
    pub fn wake(&mut self) {
        self.awake = self.awake_time;
        self.still = 0.0;
    }

    /// Moves for at least `seconds` more (while something holds it).
    pub fn keep_awake(&mut self, seconds: f32) {
        self.awake = self.awake.max(seconds);
        self.still = 0.0;
    }

    fn hard_pin(&self, index: usize) -> Option<Vec3> {
        self.pins
            .iter()
            .find(|p| p.index == index && p.stiffness >= 1.0)
            .map(|p| p.position)
    }

    /// Advances it by `dt` (at most about 1/30 s for stable contacts). `cast(from, to)`
    /// finds the first surface on the segment: its point and normal.
    pub fn step(&mut self, dt: f32, cast: &mut impl FnMut(Vec3, Vec3) -> Option<(Vec3, Vec3)>) {
        if self.asleep() || dt <= 0.0 {
            return;
        }
        let keep = self.air_friction.clamp(0.0, 1.0).powf(dt * 30.0);
        let gravity = Vec3::NEG_Y * ROPE_GRAVITY * dt * dt;
        let start = self.points.clone();
        let radius = self.radius;

        // Integrate, stopping at surfaces.
        for i in 0..self.points.len() {
            if let Some(pin) = self.hard_pin(i) {
                self.previous[i] = self.points[i];
                self.points[i] = pin;
                continue;
            }
            let point = self.points[i];
            let velocity = (point - self.previous[i]) * keep;
            let mut target = point + velocity + gravity;
            if let Some(line) = self.hang
                && point.y < line.top.y - self.link * 0.5
                && point.y > line.bottom + self.link * 0.5
            {
                let pull = 1.0 - (-line.rate * dt).exp();
                target.x += (line.top.x - target.x) * pull;
                target.z += (line.top.z - target.z) * pull;
            }
            self.previous[i] = point;
            self.points[i] = target;
            if let Some((hit, normal)) = sweep(cast, point, target, radius) {
                let into = velocity.dot(normal);
                let along = (velocity - normal * into) * (1.0 - ROPE_CONTACT_FRICTION);
                let bounce = normal * (-into * self.elasticity).max(0.0);
                self.points[i] = hit + normal * radius;
                self.previous[i] = self.points[i] - (along + bounce);
            }
        }
        let integrated = self.points.clone();

        // Links: never longer than their length (a rope goes slack but doesn't stretch).
        for _ in 0..ROPE_ITERATIONS {
            for pin in &self.pins {
                if let Some(point) = self.points.get_mut(pin.index) {
                    *point += (pin.position - *point) * pin.stiffness.clamp(0.0, 1.0);
                }
            }
            for i in 0..self.points.len().saturating_sub(1) {
                let (a, b) = (self.points[i], self.points[i + 1]);
                let d = b - a;
                let length = d.length();
                if length <= self.link || length < 1e-6 {
                    continue;
                }
                let wa = if self.hard_pin(i).is_some() { 0.0 } else { 1.0 };
                let wb = if self.hard_pin(i + 1).is_some() { 0.0 } else { 1.0 };
                if wa + wb == 0.0 {
                    continue;
                }
                let correction = d * ((length - self.link) / length / (wa + wb));
                self.points[i] += correction * wa;
                self.points[i + 1] -= correction * wb;
            }
        }

        // What the links pulled into a surface comes back out, and links cutting through a
        // corner (over the edge of a roof) push their ends out until they wrap around it.
        for i in 0..self.points.len() {
            if self.hard_pin(i).is_some() {
                continue;
            }
            if let Some((hit, normal)) = sweep(cast, integrated[i], self.points[i], radius) {
                self.points[i] = hit + normal * radius;
            }
        }
        for i in 0..self.points.len().saturating_sub(1) {
            let (a, b) = (self.points[i], self.points[i + 1]);
            if let Some((_, normal)) = cast(a, b) {
                for j in [i, i + 1] {
                    if self.hard_pin(j).is_none() {
                        self.points[j] += normal * ROPE_WRAP_PUSH;
                    }
                }
            }
        }

        // Sleep once still, or after its awake time.
        let fastest = self
            .points
            .iter()
            .zip(&start)
            .map(|(a, b)| a.distance_squared(*b))
            .fold(0.0, f32::max)
            .sqrt()
            / dt;
        self.still = if fastest < ROPE_STILL_SPEED { self.still + dt } else { 0.0 };
        self.awake -= dt;
        if self.still >= ROPE_STILL_TIME {
            self.awake = 0.0;
        }
        if self.asleep() {
            self.previous.clone_from(&self.points);
        }
    }

    /// Length along the points.
    pub fn length(&self) -> f32 {
        self.points.windows(2).map(|w| w[0].distance(w[1])).sum()
    }
}

/// The first surface on the way from `from` to `to`, looking `radius` further.
fn sweep(
    cast: &mut impl FnMut(Vec3, Vec3) -> Option<(Vec3, Vec3)>,
    from: Vec3,
    to: Vec3,
    radius: f32,
) -> Option<(Vec3, Vec3)> {
    let d = to - from;
    let length = d.length();
    if length < 1e-6 {
        return None;
    }
    cast(from, to + d / length * radius).filter(|(_, normal)| normal.length_squared() > 0.5)
}

/// [`RopeSim::step`]'s `cast` against the level (and vehicles).
pub fn world_cast<'a>(spatial: &'a SpatialQuery<'_, '_>) -> impl FnMut(Vec3, Vec3) -> Option<(Vec3, Vec3)> + 'a {
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]);
    move |from: Vec3, to: Vec3| {
        let d = to - from;
        let length = d.length();
        let direction = Dir3::new(d).ok()?;
        spatial
            .cast_ray(from, direction, length, true, &filter)
            .map(|hit| (from + direction * hit.distance, hit.normal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Segment against the ground (y = 0) and boxes: the first hit and its face normal.
    fn cast_boxes(boxes: &[(Vec3, Vec3)]) -> impl FnMut(Vec3, Vec3) -> Option<(Vec3, Vec3)> + '_ {
        move |from, to| {
            let d = to - from;
            let mut best: Option<(f32, Vec3)> = None;
            let mut consider = |t: f32, normal: Vec3| {
                if (0.0..=1.0).contains(&t) && best.is_none_or(|(b, _)| t < b) {
                    best = Some((t, normal));
                }
            };
            if from.y >= 0.0 && to.y < 0.0 {
                consider(from.y / (from.y - to.y), Vec3::Y);
            } else if from.y < 0.0 {
                consider(0.0, Vec3::Y);
            }
            for (min, max) in boxes {
                // Slabs.
                let (mut enter, mut exit, mut normal) = (f32::MIN, f32::MAX, Vec3::ZERO);
                for axis in 0..3 {
                    let (o, v) = (from[axis], d[axis]);
                    if v.abs() < 1e-9 {
                        if o < min[axis] || o > max[axis] {
                            enter = f32::MAX;
                        }
                        continue;
                    }
                    let (t1, t2) = ((min[axis] - o) / v, (max[axis] - o) / v);
                    let (near, far) = if t1 < t2 { (t1, t2) } else { (t2, t1) };
                    if near > enter {
                        enter = near;
                        normal = Vec3::ZERO;
                        normal[axis] = if v > 0.0 { -1.0 } else { 1.0 };
                    }
                    exit = exit.min(far);
                }
                if enter <= exit && exit >= 0.0 {
                    if enter < 0.0 {
                        // Starting inside: out through the nearest face upwards.
                        consider(0.0, Vec3::Y);
                    } else {
                        consider(enter, normal);
                    }
                }
            }
            best.map(|(t, normal)| (from + d * t, normal))
        }
    }

    fn inside(p: Vec3, boxes: &[(Vec3, Vec3)]) -> bool {
        p.y < -1e-3 || boxes.iter().any(|(min, max)| p.cmpgt(*min + 1e-3).all() && p.cmplt(*max - 1e-3).all())
    }

    fn run(sim: &mut RopeSim, seconds: f32, cast: &mut impl FnMut(Vec3, Vec3) -> Option<(Vec3, Vec3)>) -> f32 {
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        while t < seconds && !sim.asleep() {
            sim.step(dt, cast);
            t += dt;
            assert!(sim.points.iter().all(|p| p.is_finite()), "blew up at {t}");
        }
        t
    }

    #[test]
    fn a_hanging_rope_swings_down_and_sleeps() {
        // Thrown out sideways from a hook 20 m up, nothing below for 14 m.
        let link = 14.0 / 26.0;
        let points = (0..=26).map(|i| Vec3::new(i as f32 * link, 20.0, 0.0)).collect();
        let mut sim = RopeSim::new(points, link, 0.015, 0.95, 0.2, 6.0);
        sim.pins.push(Pin { index: 0, position: Vec3::new(0.0, 20.0, 0.0), stiffness: 1.0 });
        let mut cast = cast_boxes(&[]);
        let slept_after = run(&mut sim, 30.0, &mut cast);
        assert!(sim.asleep(), "still moving");
        assert!(slept_after <= 6.0 + 1e-3, "slept after {slept_after} s");
        let bottom = *sim.points.last().unwrap();
        println!("slept after {slept_after:.2} s, bottom {bottom}");
        // Hanging straight down, as long as it is (within a stretch of 2%).
        assert!(bottom.x.abs() < 0.5 && (20.0 - bottom.y - 14.0).abs() < 0.3, "{bottom}");
        assert!((sim.length() - 14.0).abs() < 0.3, "length {}", sim.length());
    }

    #[test]
    fn a_rope_drapes_over_a_ledge_and_piles_up_below() {
        // A 6 m high building (x < 0), the hook just back from its edge at x = 0; 14 m of
        // rope thrown out over the street.
        let building = [(Vec3::new(-10.0, 0.0, -5.0), Vec3::new(0.0, 6.0, 5.0))];
        let link = 14.0 / 26.0;
        let hook = Vec3::new(-0.2, 6.02, 0.0);
        let points = (0..=26).map(|i| hook + Vec3::new(i as f32 * link * 0.7, 0.3, 0.0)).collect();
        let mut sim = RopeSim::new(points, link, 0.015, 0.95, 0.2, 6.0);
        sim.pins.push(Pin { index: 0, position: hook, stiffness: 1.0 });
        sim.hang = Some(HangLine { top: Vec3::new(OFF_WALL, 6.0, 0.0), bottom: 0.0, rate: 3.0 });
        let mut cast = cast_boxes(&building);
        for step in 0..600 {
            sim.step(1.0 / 60.0, &mut cast);
            for (i, p) in sim.points.iter().enumerate() {
                assert!(!inside(*p, &building), "point {i} inside at step {step}: {p}");
            }
        }
        println!("{:?}", sim.points);
        assert!(sim.asleep());
        // Down the wall, close to it, and the rest lying on the street.
        let on_ground = sim.points.iter().filter(|p| p.y < 0.1).count();
        let along_wall = sim.points.iter().filter(|p| p.y > 0.5 && p.y < 5.5).collect::<Vec<_>>();
        assert!(on_ground >= 10, "{on_ground} links on the ground");
        assert!(along_wall.iter().all(|p| p.x >= 0.0 && (p.x - OFF_WALL).abs() < 0.15), "{along_wall:?}");
        assert!(along_wall.len() >= 7);
    }

    #[test]
    fn a_climber_holds_the_rope_out() {
        let link = 0.5;
        let points = (0..=20).map(|i| Vec3::new(0.0, 10.0 - i as f32 * link, 0.0)).collect();
        let mut sim = RopeSim::new(points, link, 0.015, 0.95, 0.2, 6.0);
        sim.pins.push(Pin { index: 0, position: Vec3::new(0.0, 10.0, 0.0), stiffness: 1.0 });
        sim.pins.push(Pin { index: 6, position: Vec3::new(0.4, 7.0, 0.0), stiffness: 1.0 });
        let mut cast = cast_boxes(&[]);
        for _ in 0..240 {
            sim.keep_awake(0.5);
            sim.step(1.0 / 60.0, &mut cast);
        }
        assert!(sim.points[6].distance(Vec3::new(0.4, 7.0, 0.0)) < 1e-4);
        // Below the hands it hangs from them.
        let below = sim.points[20];
        assert!((below.x - 0.4).abs() < 0.2 && below.y < 7.0 - 6.0, "{below}");
    }

    #[test]
    fn settled_points_follow_the_climbed_line() {
        let rope = Rope {
            kind: RopeKind::Grapple,
            anchor: Vec3::new(0.0, 4.0, -2.0),
            top: Vec3::new(0.0, 4.0, -2.0) + Vec3::Z * OFF_WALL,
            end: Vec3::new(0.0, 0.0, -2.0) + Vec3::Z * OFF_WALL,
            length: 14.0,
            links: 26,
        };
        let points = rope.settled_points();
        assert_eq!(points.len(), 27);
        assert_eq!(points[0], rope.anchor);
        let length: f32 = points.windows(2).map(|w| w[0].distance(w[1])).sum();
        assert!((length - 14.0).abs() < 0.3, "{length}");
        // What doesn't hang lies on the ground, away from the wall.
        let last = points.last().unwrap();
        assert!(last.y.abs() < 1e-4 && last.z > 5.0, "{last}");
    }
}
