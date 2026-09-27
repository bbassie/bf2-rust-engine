//! Infantry movement.
//!
//! [`step_soldier`] is the single source of truth for how a soldier moves. The server runs
//! it for authority, clients run it for prediction and replay, and bots feed it the same
//! [`InputFrame`]s humans do.
//!
//! The soldier is a kinematic capsule moved with avian's move-and-slide. On the ground it
//! walks along the surface (constant horizontal speed on slopes), steps up ledges up to
//! [`SoldierTuning::step_height`] and snaps down to stay glued to slopes and stairs. Walking
//! into a ladder climbs it (see [`crate::ladder`]). Numbers follow BF2's engine defaults
//! where they are known (see [`SoldierTuning`]).

use core::time::Duration;

use avian3d::prelude::*;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    input::{Buttons, InputFrame},
    ladder::{Ladder, add_ladder_volume},
    physics::GameLayer,
};

/// Capsule radius for movement and hitboxes (BF2 `coll-soldier-radius`).
pub const SOLDIER_RADIUS: f32 = 0.25;
pub const SOLDIER_HEIGHT: f32 = 1.8;
/// Offset from the feet to the standing capsule's center.
pub const SOLDIER_CENTER: Vec3 = Vec3::new(0.0, SOLDIER_HEIGHT * 0.5, 0.0);
/// Gap kept between the capsule and the ground it stands on.
const SKIN: f32 = 0.01;

pub struct SoldierPlugin;

impl Plugin for SoldierPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SoldierTuning>()
            .init_resource::<SoldierShapes>()
            .add_observer(add_soldier_physics)
            .add_observer(add_ladder_volume)
            .add_plugins(crate::rope::RopePlugin)
            .add_systems(FixedPostUpdate, fit_hitboxes_to_stance);
    }
}

/// A soldier body. Replicated; the owning player is in [`crate::protocol::ControlledBy`].
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default)]
#[require(Transform, SoldierMotion, InputAck, Health)]
pub struct Soldier;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stance {
    #[default]
    Standing,
    Crouching,
    Prone,
}

impl Stance {
    pub fn eye_height(self) -> f32 {
        match self {
            Stance::Standing => 1.65,
            Stance::Crouching => 1.05,
            Stance::Prone => 0.35,
        }
    }

    /// Height of the movement capsule (BF2 `coll-soldier-crouch-height`/`prone-height`).
    pub fn collision_height(self) -> f32 {
        match self {
            Stance::Standing => SOLDIER_HEIGHT,
            Stance::Crouching => 1.4,
            Stance::Prone => 0.8,
        }
    }

    /// Offset from the feet to the movement capsule's center.
    pub fn collision_center(self) -> Vec3 {
        Vec3::Y * self.collision_height() * 0.5
    }
}

/// Complete movement state of a soldier. Authoritative on the server, replicated to clients.
///
/// Everything [`step_soldier`] depends on is in here, so a client can replay inputs from
/// any received state. Timers count down to zero and then stay put, so a soldier standing
/// still stops changing (and replicating).
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SoldierMotion {
    /// Feet position.
    pub position: Vec3,
    /// On the ground: horizontal velocity plus the vertical speed of walking along the slope.
    pub velocity: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub grounded: bool,
    pub stance: Stance,
    /// Seconds of air control left. Set to [`SoldierTuning::air_control_time`] by a jump,
    /// so `air_control_time - air_control` is the time since jumping while it runs.
    pub air_control: f32,
    /// Seconds of landing recovery left after a hard landing: slower, can't jump.
    pub recovery: f32,
    /// Seconds until a soldier that was prone may jump.
    pub prone_lock: f32,
    /// Jump was held last tick: jumping needs a fresh press.
    pub jump_held: bool,
    /// On a ladder. Which one is found again from the position every tick.
    pub climbing: bool,
    /// Sprint stamina, 1 full, 0 empty.
    pub stamina: f32,
    pub sprinting: bool,
    /// Seconds until stamina recovers again after a jump.
    pub stamina_delay: f32,
    /// Heavy kit (BF2's `*_heavy_soldier`): less stamina, see [`SoldierTuning::heavy`].
    pub heavy: bool,
    /// Seconds until the weapon may fire again (after jumping or getting up from prone).
    pub fire_lock: f32,
    /// Seconds until a prone soldier may get up, or a soldier who got up may go prone.
    pub stance_lock: f32,
    /// What is climbed (see `climbing`) is a grappling rope rather than a ladder.
    pub on_rope: bool,
    /// Sliding down a zipline, hanging from the wire at [`SoldierTuning::zipline_hang`].
    pub riding: bool,
}

impl Default for SoldierMotion {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            grounded: false,
            stance: Stance::Standing,
            air_control: 0.0,
            recovery: 0.0,
            prone_lock: 0.0,
            jump_held: false,
            climbing: false,
            stamina: 1.0,
            sprinting: false,
            stamina_delay: 0.0,
            heavy: false,
            fire_lock: 0.0,
            stance_lock: 0.0,
            on_rope: false,
            riding: false,
        }
    }
}

impl SoldierMotion {
    pub fn at(position: Vec3, yaw: f32) -> Self {
        Self {
            position,
            yaw,
            ..default()
        }
    }

    /// Whether the soldier's hands are free to fire: not on a ladder, and not just after
    /// jumping or getting up from prone (BF2's `fire-delay-after-jump` and
    /// `fire-delay-from-prone`).
    pub fn can_fire(&self) -> bool {
        !self.climbing && !self.riding && self.fire_lock <= 0.0
    }

    pub fn eye_position(&self) -> Vec3 {
        self.position + Vec3::Y * self.stance.eye_height()
    }

    pub fn view_rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }

    pub fn body_transform(&self) -> Transform {
        Transform::from_translation(self.position).with_rotation(Quat::from_rotation_y(self.yaw))
    }
}

/// Sequence number of the last input the server applied to this soldier.
/// Clients use it to know which of their predicted inputs to replay.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAck(pub u32);

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            current: 100.0,
            max: 100.0,
        }
    }
}

/// Movement tuning. Values in quotes are BF2 engine variables (defaults compiled into
/// `BF2.exe`, overrides from `objects/soldiers/common/Common.con`).
#[derive(Resource, Clone, Debug)]
pub struct SoldierTuning {
    /// `phy-soldier-run-speed`.
    pub run_speed: f32,
    /// `phy-soldier-sprint-speed`.
    pub sprint_speed: f32,
    /// `phy-soldier-crouch-speed`.
    pub crouch_speed: f32,
    /// `phy-soldier-crawl-speed`.
    pub prone_speed: f32,
    /// Share of the gap to the wanted velocity closed every 1/30 s when speeding up
    /// (`phy-soldier-acceleration`) and when slowing down or turning (`phy-soldier-deceleration`).
    pub acceleration: f32,
    pub deceleration: f32,
    /// Speed a jumping soldier can steer towards (`phy-soldier-inair-speed`). Air control
    /// fades out over `air_control_time` after the jump; falling off a ledge gives none.
    pub air_speed: f32,
    pub air_control_time: f32,
    /// How quickly air control changes the velocity, m/s².
    pub air_acceleration: f32,
    /// Vertical launch speed. BF2 sets 6 × `phy-soldier-jump-factor`.
    pub jump_speed: f32,
    /// Share of the ground velocity kept when jumping (`phy-soldier-jump-length-factor`).
    pub jump_momentum: f32,
    /// Soldier gravity, m/s². BF2's world gravity is 10 and ground vehicles use 1.5-2×;
    /// 20 turns the 6 m/s launch into a short 0.9 m hop.
    pub gravity: f32,
    /// `jump-delay-after-prone`.
    pub jump_delay_after_prone: f32,
    /// Landing faster than this (m/s) starts a recovery: `landing_time` seconds during
    /// which speed starts at `landing_speed` × normal and jumping is blocked.
    pub landing_impact: f32,
    pub landing_time: f32,
    pub landing_speed: f32,
    /// Steepest walkable slope, radians. BF2's `phy-soldier-feet-contact-normal` is 0.5 (60°).
    pub max_slope: f32,
    /// Highest ledge a soldier walks up without jumping.
    pub step_height: f32,
    /// How far the soldier snaps down to stay on the ground walking down slopes and steps.
    pub snap_distance: f32,
    /// Ladder speeds, m/s: up at the pace of BF2's climbing animation (`3p_climbup` moves
    /// the feet about 1.25 m/s). Going down, BF2 soldiers slide (its `3p_climb_skid`
    /// animation is made for 3 m/s).
    pub climb_speed: f32,
    pub climb_down_speed: f32,
    /// Speed away from the ladder when jumping off it.
    pub ladder_jump_speed: f32,
    /// Climbing speed on a grappling rope, up and down (BF2 `GrapplingHookRope.climbingSpeed`).
    pub rope_climb_speed: f32,
    /// Ziplines: the feet hang this far below the wire (BF2's hanging animation holds the
    /// handle 1.2 m above the hips).
    pub zipline_hang: f32,
    /// Not getting on closer than this to the wire's low end (`distanceCannotEnter`).
    pub zipline_no_entry: f32,
    /// Sliding down: gravity along the wire (BF2's world gravity), less a drag in 1/s,
    /// between a crawl on nearly level wires and a top speed, m/s.
    pub zipline_gravity: f32,
    pub zipline_drag: f32,
    pub zipline_min_speed: f32,
    pub zipline_max_speed: f32,
    /// Sprint stamina of light and heavy kits.
    pub light: StaminaTuning,
    pub heavy: StaminaTuning,
    /// Stamina needed to start sprinting (soldier template `SprintLimit`). Running out
    /// stops the sprint until it has recovered this far.
    pub sprint_min_stamina: f32,
    /// Seconds stamina doesn't recover after a jump (`sprint-recharge-delay-after-jump`).
    pub stamina_delay_after_jump: f32,
    /// Seconds the weapon can't fire after jumping (`fire-delay-after-jump`) and after
    /// getting up from prone (`fire-delay-from-prone`).
    pub fire_delay_after_jump: f32,
    pub fire_delay_after_prone: f32,
    /// Seconds a soldier who went prone stays down (`stand-delay-from-prone`), and a soldier
    /// who got up can't go prone again (`prone-delay-from-stand`).
    pub prone_switch_delay: f32,
    /// Seconds after a jump before going prone (`prone-delay-after-jump`).
    pub prone_delay_after_jump: f32,
}

/// Sprint stamina, from BF2's soldier templates (`us_light_soldier.tweak` and friends).
#[derive(Clone, Copy, Debug)]
pub struct StaminaTuning {
    /// Seconds of sprinting on a full stamina (`SprintDissipationTime`).
    pub sprint_time: f32,
    /// Seconds from empty to full (`SprintRecoverTime`).
    pub recover_time: f32,
    /// Stamina a jump costs (`SprintLossAtJump`).
    pub jump_cost: f32,
}

impl SoldierTuning {
    pub fn stamina(&self, heavy: bool) -> &StaminaTuning {
        if heavy { &self.heavy } else { &self.light }
    }
}

/// Whether a kit class carries BF2's heavy soldier (body armour, less stamina): the level
/// scripts give it to Assault, Support and AT.
pub fn heavy_kit(kind: &str) -> bool {
    matches!(kind, "Assault" | "Support" | "AT")
}

impl Default for SoldierTuning {
    fn default() -> Self {
        Self {
            run_speed: 3.9,
            sprint_speed: 7.0,
            crouch_speed: 2.0,
            prone_speed: 0.8,
            acceleration: 0.2,
            deceleration: 0.4,
            air_speed: 2.0,
            air_control_time: 2.0,
            air_acceleration: 3.0,
            jump_speed: 6.0,
            jump_momentum: 0.98,
            gravity: 20.0,
            jump_delay_after_prone: 0.8,
            landing_impact: 4.5,
            landing_time: 0.35,
            landing_speed: 0.4,
            max_slope: 55f32.to_radians(),
            step_height: 0.45,
            snap_distance: 0.45,
            climb_speed: 1.25,
            climb_down_speed: 3.0,
            ladder_jump_speed: 3.0,
            rope_climb_speed: 2.3,
            zipline_hang: 2.2,
            zipline_no_entry: 3.5,
            zipline_gravity: 10.0,
            zipline_drag: 0.3,
            zipline_min_speed: 2.0,
            zipline_max_speed: 15.0,
            light: StaminaTuning {
                sprint_time: 10.0,
                recover_time: 17.0,
                jump_cost: 0.15,
            },
            heavy: StaminaTuning {
                sprint_time: 8.0,
                recover_time: 20.0,
                jump_cost: 0.2,
            },
            sprint_min_stamina: 0.05,
            stamina_delay_after_jump: 0.7,
            fire_delay_after_jump: 0.7,
            fire_delay_after_prone: 0.6,
            prone_switch_delay: 0.8,
            prone_delay_after_jump: 0.3,
        }
    }
}

/// Shapes used for soldier movement queries and hitboxes.
#[derive(Resource)]
pub struct SoldierShapes {
    standing: Collider,
    crouching: Collider,
    prone: Collider,
    /// A standing soldier grown by the reach to ladders.
    ladder_probe: Collider,
    /// Around the hands: how far from a zipline's wire a soldier can grab it.
    zipline_probe: Collider,
}

impl SoldierShapes {
    /// The movement capsule for a stance; its bottom is at the feet when placed at
    /// `position + stance.collision_center()`.
    pub fn movement(&self, stance: Stance) -> &Collider {
        match stance {
            Stance::Standing => &self.standing,
            Stance::Crouching => &self.crouching,
            Stance::Prone => &self.prone,
        }
    }
}

impl Default for SoldierShapes {
    fn default() -> Self {
        let capsule = |stance: Stance| {
            Collider::capsule(
                SOLDIER_RADIUS,
                stance.collision_height() - 2.0 * SOLDIER_RADIUS,
            )
        };
        Self {
            standing: capsule(Stance::Standing),
            crouching: capsule(Stance::Crouching),
            prone: capsule(Stance::Prone),
            zipline_probe: Collider::sphere(ZIPLINE_REACH),
            ladder_probe: Collider::capsule(
                SOLDIER_RADIUS + LADDER_REACH,
                SOLDIER_HEIGHT - 2.0 * SOLDIER_RADIUS,
            ),
        }
    }
}

/// The child entity holding a soldier's hitbox collider.
#[derive(Component, Clone, Copy, Debug)]
pub struct Hitbox {
    pub entity: Entity,
    pub stance: Stance,
}

/// Gives every soldier (spawned on the server or replicated to a client) a hitbox.
/// The hitbox does not take part in the physics solver; it is only found by queries.
fn add_soldier_physics(add: On<Add, Soldier>, mut commands: Commands, shapes: Res<SoldierShapes>) {
    let hitbox = commands
        .spawn((
            shapes.movement(Stance::Standing).clone(),
            Transform::from_translation(SOLDIER_CENTER),
            CollisionLayers::new(GameLayer::Soldier, LayerMask::NONE),
            ChildOf(add.entity),
        ))
        .id();
    commands.entity(add.entity).insert((
        RigidBody::Kinematic,
        Hitbox {
            entity: hitbox,
            stance: Stance::Standing,
        },
    ));
}

/// Height of the top of the head above the feet, per stance.
pub fn stance_height(stance: Stance) -> f32 {
    match stance {
        Stance::Standing => SOLDIER_HEIGHT,
        Stance::Crouching => 1.25,
        Stance::Prone => 0.5,
    }
}

/// Crouching and prone soldiers are harder to hit: reshape the hitbox with the stance.
fn fit_hitboxes_to_stance(
    mut soldiers: Query<(&SoldierMotion, &mut Hitbox)>,
    mut hitboxes: Query<(&mut Collider, &mut Transform)>,
) {
    for (motion, mut hitbox) in &mut soldiers {
        if hitbox.stance == motion.stance {
            continue;
        }
        hitbox.stance = motion.stance;
        let Ok((mut collider, mut transform)) = hitboxes.get_mut(hitbox.entity) else {
            continue;
        };
        let (shape, offset) = match motion.stance {
            Stance::Prone => (
                // Lying along the view direction.
                Collider::capsule(0.25, SOLDIER_HEIGHT - 0.5),
                Transform::from_xyz(0.0, 0.25, 0.0)
                    .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
            ),
            stance => {
                let height = stance_height(stance);
                (
                    Collider::capsule(SOLDIER_RADIUS, height - 2.0 * SOLDIER_RADIUS),
                    Transform::from_xyz(0.0, height * 0.5, 0.0),
                )
            }
        };
        *collider = shape;
        *transform = offset;
    }
}

/// Advances one soldier by one tick.
pub fn step_soldier(
    m: &mut SoldierMotion,
    input: &InputFrame,
    dt: f32,
    tuning: &SoldierTuning,
    shapes: &SoldierShapes,
    mover: &MoveAndSlide,
) {
    let world = Surroundings {
        mover,
        filter: SpatialQueryFilter::from_mask(GameLayer::soldier_movement_mask()),
        min_normal_y: tuning.max_slope.cos(),
    };

    m.yaw = input.yaw;
    m.pitch = input.pitch.clamp(-1.55, 1.55);
    for timer in [
        &mut m.air_control,
        &mut m.recovery,
        &mut m.prone_lock,
        &mut m.stamina_delay,
        &mut m.fire_lock,
        &mut m.stance_lock,
    ] {
        *timer = (*timer - dt).max(0.0);
    }
    let jump_pressed = input.pressed(Buttons::JUMP);
    let fresh_jump = jump_pressed && !m.jump_held;
    m.jump_held = jump_pressed;

    if m.climbing {
        m.sprinting = false;
        update_stamina(m, tuning, false, dt);
        climb(m, &world, tuning, shapes, input, fresh_jump, dt);
        return;
    }
    if m.riding {
        m.sprinting = false;
        update_stamina(m, tuning, false, dt);
        ride(m, &world, tuning, shapes, fresh_jump, dt);
        return;
    }

    // Stance follows the buttons while on the ground, if there is room to stand up, and
    // not straight back after going prone or getting up.
    if m.grounded {
        let mut wanted = wanted_stance(input);
        let prone = m.stance == Stance::Prone;
        if m.stance_lock > 0.0 && prone != (wanted == Stance::Prone) {
            wanted = m.stance;
        }
        let stance = world.fit_stance(m.position, m.stance, wanted, shapes);
        if (stance == Stance::Prone) != prone {
            m.stance_lock = tuning.prone_switch_delay;
            if prone {
                m.fire_lock = m.fire_lock.max(tuning.fire_delay_after_prone);
            }
        }
        m.stance = stance;
    }
    if m.stance == Stance::Prone {
        m.prone_lock = tuning.jump_delay_after_prone;
    }
    let shape = shapes.movement(m.stance);
    let center = m.stance.collision_center();
    let start = m.position + center;

    // Grounded is re-checked from the position, so a replayed state always agrees. As far
    // down as the snap reaches: resting on an edge, the gap below can be centimeters.
    let ground = if m.grounded {
        world.ground(shape, start, tuning.snap_distance + SKIN)
    } else {
        None
    };
    m.grounded = ground.is_some();

    // Wanted horizontal velocity.
    let intent = input.movement_vec();
    let wish = Quat::from_rotation_y(m.yaw) * Vec3::new(intent.x, 0.0, -intent.y);
    let wants_sprint =
        input.pressed(Buttons::SPRINT) && intent.y > 0.5 && m.stance == Stance::Standing;
    update_stamina(m, tuning, wants_sprint, dt);
    let mut speed = match m.stance {
        Stance::Standing if m.sprinting => tuning.sprint_speed,
        Stance::Standing => tuning.run_speed,
        Stance::Crouching => tuning.crouch_speed,
        Stance::Prone => tuning.prone_speed,
    };
    if m.recovery > 0.0 {
        let recovered = 1.0 - m.recovery / tuning.landing_time;
        speed *= tuning.landing_speed + (1.0 - tuning.landing_speed) * recovered;
    }
    let mut horizontal = Vec3::new(m.velocity.x, 0.0, m.velocity.z);

    // Walking into a ladder or a grappling rope (or over the edge onto one from the top)
    // gets on it.
    if wish != Vec3::ZERO
        && let Some(ladder) = world.ladder(m.position, shapes)
        && let Some(feet) = mount(m, &ladder, wish)
    {
        m.position = feet;
        m.velocity = Vec3::ZERO;
        m.stance = Stance::Standing;
        m.grounded = false;
        m.climbing = true;
        m.on_rope = ladder.rope;
        return;
    }
    // Walking along under a zipline's wire, downhill, grabs it.
    if wish != Vec3::ZERO
        && m.stance == Stance::Standing
        && let Some(wire) = world.zipline(m.position + Vec3::Y * tuning.zipline_hang, shapes)
        && let Some(feet) = grab_wire(m, &wire, wish, tuning)
    {
        m.position = feet;
        m.velocity = wire.down * tuning.zipline_min_speed;
        m.grounded = false;
        m.riding = true;
        return;
    }

    if let Some(ground) = ground {
        let target = wish * speed;
        let factor = if target.length_squared() > horizontal.length_squared() + 1e-4 {
            tuning.acceleration
        } else {
            tuning.deceleration
        };
        horizontal = approach(horizontal, target, factor, dt);

        let jump =
            fresh_jump && m.stance == Stance::Standing && m.recovery == 0.0 && m.prone_lock == 0.0;
        if jump {
            m.velocity = horizontal * tuning.jump_momentum + Vec3::Y * tuning.jump_speed;
            m.air_control = tuning.air_control_time;
            m.grounded = false;
            m.stamina = (m.stamina - tuning.stamina(m.heavy).jump_cost).max(0.0);
            m.stamina_delay = tuning.stamina_delay_after_jump;
            m.fire_lock = m.fire_lock.max(tuning.fire_delay_after_jump);
            m.stance_lock = m.stance_lock.max(tuning.prone_delay_after_jump);
        } else {
            walk(
                m,
                &world,
                tuning,
                shape,
                center,
                horizontal,
                ground.normal,
                dt,
            );
            return;
        }
    } else {
        // Airborne: keep momentum, steer a little right after a jump.
        let control = m.air_control / tuning.air_control_time;
        if control > 0.0 && wish != Vec3::ZERO {
            let limit = horizontal
                .length()
                .max(tuning.air_speed * control * wish.length());
            horizontal = (horizontal + wish * tuning.air_acceleration * control * dt)
                .clamp_length_max(limit);
        }
        m.velocity.x = horizontal.x;
        m.velocity.z = horizontal.z;
    }

    // In the air.
    m.velocity.y -= tuning.gravity * dt;
    let impact = -m.velocity.y;
    let moved = world.slide(shape, start, m.velocity, dt, Contact::Air);
    m.position = moved.center - center;
    m.velocity = moved.velocity;
    if m.velocity.y > 0.01 {
        return;
    }
    if let Some(ground) = world.ground(shape, moved.center, 5.0 * SKIN) {
        m.position.y -= ground.gap();
        m.grounded = true;
        let horizontal = Vec3::new(m.velocity.x, 0.0, m.velocity.z);
        m.velocity = along_ground(horizontal, ground.normal);
        if impact > tuning.landing_impact {
            m.recovery = tuning.landing_time;
        }
    }
}

/// One tick of walking: slide along the ground, step up ledges, snap down to the ground.
#[allow(clippy::too_many_arguments)]
fn walk(
    m: &mut SoldierMotion,
    world: &Surroundings,
    tuning: &SoldierTuning,
    shape: &Collider,
    center: Vec3,
    horizontal: Vec3,
    ground_normal: Vec3,
    dt: f32,
) {
    let start = m.position + center;
    let wanted = horizontal.length() * dt;
    let travel =
        |moved: &Slide| Vec2::new(moved.center.x - start.x, moved.center.z - start.z).length();

    let mut moved = if horizontal == Vec3::ZERO {
        Slide {
            center: start,
            velocity: Vec3::ZERO,
            blocked: false,
        }
    } else {
        world.slide(
            shape,
            start,
            along_ground(horizontal, ground_normal),
            dt,
            Contact::Walk,
        )
    };
    // Held up by something: maybe it's a step.
    let obstructed = |moved: &Slide, share: f32| moved.blocked && travel(moved) < wanted * share;
    if obstructed(&moved, 0.99)
        && let Some(stepped) = world.step_up(
            shape,
            start,
            m.position.y,
            tuning.step_height,
            horizontal,
            dt,
        )
        && travel(&stepped) > travel(&moved) + 1e-3
    {
        moved = stepped;
    }
    // Walls take away speed; slopes and steps don't, nor does a step that is only reached
    // at the end of the tick (it is taken next tick).
    let mut horizontal = if obstructed(&moved, 0.9) {
        Vec3::new(moved.velocity.x, 0.0, moved.velocity.z)
    } else {
        horizontal
    };
    // Pushing against a wall: stay exactly put rather than creep by rounding errors.
    if moved.blocked && (moved.center - start).abs().max_element() < 1e-4 {
        moved.center = start;
        if horizontal.length_squared() < 0.01 * 0.01 {
            horizontal = Vec3::ZERO;
        }
    }

    m.position = moved.center - center;
    match world.ground(shape, moved.center, tuning.snap_distance + SKIN) {
        Some(ground) => {
            m.position.y -= ground.gap();
            m.velocity = along_ground(horizontal, ground.normal);
        }
        None => {
            // Walked off a ledge (or onto a slope too steep to stand on).
            m.grounded = false;
            m.velocity = horizontal + Vec3::Y * moved.velocity.y.min(0.0);
        }
    }
}

/// How far from a ladder a soldier can get on it.
const LADDER_REACH: f32 = 0.35;
/// Looking further down than this, forward climbs down.
const LADDER_LOOK_DOWN: f32 = -0.45;

/// One tick on a ladder: up or down it, off it at the top, the bottom, or by jumping.
fn climb(
    m: &mut SoldierMotion,
    world: &Surroundings,
    tuning: &SoldierTuning,
    shapes: &SoldierShapes,
    input: &InputFrame,
    fresh_jump: bool,
    dt: f32,
) {
    m.stance = Stance::Standing;
    m.grounded = false;
    let Some(ladder) = world.ladder(m.position, shapes) else {
        m.climbing = false;
        return;
    };
    if fresh_jump {
        m.climbing = false;
        m.velocity = ladder.front * tuning.ladder_jump_speed + Vec3::Y * 2.0;
        return;
    }
    let forward = input.movement_vec().y;
    let dir = if m.pitch < LADDER_LOOK_DOWN {
        -forward
    } else {
        forward
    };
    m.on_rope = ladder.rope;
    let speed = dir
        * if ladder.rope {
            tuning.rope_climb_speed
        } else {
            climb_speed(tuning, dir)
        };

    // Down to the ground: off at the bottom.
    if speed < 0.0 {
        let shape = shapes.movement(Stance::Standing);
        let center = m.position + Stance::Standing.collision_center();
        if let Some(ground) = world.ground(shape, center, -speed * dt + 5.0 * SKIN) {
            m.position.y -= ground.gap();
            m.velocity = Vec3::ZERO;
            m.climbing = false;
            m.grounded = true;
            return;
        }
    }

    // Up or down the rails, pulled onto the line the soldier holds on to. Like BF2's
    // ladder "seat" this ignores collisions: eaves often hang over the top of a ladder.
    let height = ladder.local(m.position).y;
    let hold = ladder.world(Vec3::new(0.0, height, ladder_hold(&ladder)));
    let pull = ((hold - m.position) / dt).clamp_length_max(3.0);
    m.position += (pull + ladder.up * speed) * dt;
    m.velocity = ladder.up * speed;

    let height = ladder.local(m.position).y;
    if speed > 0.0 && height >= ladder.top() {
        // Over the top: hop onto what is behind the ladder.
        m.climbing = false;
        m.velocity = -ladder.front * 3.0 + Vec3::Y * 3.0;
    } else if speed < 0.0 && height < -ladder.top() - 0.2 {
        // Off the bottom of a ladder that ends in the air.
        m.climbing = false;
    }
}

fn climb_speed(tuning: &SoldierTuning, dir: f32) -> f32 {
    if dir > 0.0 {
        tuning.climb_speed
    } else {
        tuning.climb_down_speed
    }
}

/// How far from a zipline's wire the hands can grab it.
const ZIPLINE_REACH: f32 = 1.0;

/// A zipline's wire in world space.
#[derive(Clone, Copy, Debug)]
struct Wire {
    /// The high end.
    high: Vec3,
    length: f32,
    /// Unit vector from the high end to the low end.
    down: Vec3,
}

impl Wire {
    /// How far along from the high end the point of the wire closest to `point` is.
    fn along(&self, point: Vec3) -> f32 {
        (point - self.high).dot(self.down).clamp(0.0, self.length)
    }

    fn at(&self, along: f32) -> Vec3 {
        self.high + self.down * along
    }
}

/// Getting on a zipline: close enough to the wire, not at its low end, heading down it.
fn grab_wire(m: &SoldierMotion, wire: &Wire, wish: Vec3, tuning: &SoldierTuning) -> Option<Vec3> {
    let hands = m.position + Vec3::Y * tuning.zipline_hang;
    let along = wire.along(hands);
    let flat = Vec3::new(wire.down.x, 0.0, wire.down.z).normalize_or_zero();
    let heading = wish.normalize_or_zero().dot(flat) > 0.3;
    let feet = wire.at(along) - Vec3::Y * tuning.zipline_hang;
    // Not where hanging from it would put the feet in the ground.
    (heading
        && wire.at(along).distance(hands) <= ZIPLINE_REACH
        && wire.length - along >= tuning.zipline_no_entry
        && feet.y >= m.position.y)
        .then_some(feet)
}

/// One tick on a zipline: sliding down the wire, faster the steeper it is, until its end or
/// a jump lets go. Collisions are ignored on the way, like BF2's zipline "seat".
fn ride(
    m: &mut SoldierMotion,
    world: &Surroundings,
    tuning: &SoldierTuning,
    shapes: &SoldierShapes,
    fresh_jump: bool,
    dt: f32,
) {
    m.stance = Stance::Standing;
    m.grounded = false;
    let hands = m.position + Vec3::Y * tuning.zipline_hang;
    let Some(wire) = world.zipline(hands, shapes) else {
        m.riding = false;
        return;
    };
    if fresh_jump {
        m.riding = false;
        m.velocity += Vec3::Y * 2.0;
        return;
    }
    let slope = -wire.down.y;
    let mut speed = m.velocity.dot(wire.down).max(0.0);
    speed += (tuning.zipline_gravity * slope - tuning.zipline_drag * speed) * dt;
    let speed = speed.clamp(tuning.zipline_min_speed, tuning.zipline_max_speed);
    // Off at the end, a little short of where the bolt went in.
    let end = (wire.length - 0.5).max(0.0);
    let along = (wire.along(hands) + speed * dt).min(end);
    let hands = wire.at(along);
    m.position = hands - Vec3::Y * tuning.zipline_hang;
    m.velocity = wire.down * speed;
    if along >= end {
        m.riding = false;
    }
    // Feet on the ground (the wire's low end is often low): standing there.
    let shape = shapes.movement(Stance::Standing);
    let center = Stance::Standing.collision_center();
    if let Some(floor) = world.cast(
        shape,
        hands + center,
        Dir3::NEG_Y,
        tuning.zipline_hang + SKIN,
    ) && floor.distance < tuning.zipline_hang
    {
        m.position.y = hands.y - floor.distance + SKIN;
        m.velocity.y = 0.0;
        m.riding = false;
        m.grounded = true;
    }
}

/// Distance of a climbing soldier's axis from the ladder's center plane.
fn ladder_hold(ladder: &Ladder) -> f32 {
    ladder.half.z + SOLDIER_RADIUS + 0.05
}

/// Where the soldier's feet go when getting on `ladder`, if moving towards it: in front of
/// it, or from the top onto it (BF2 lets you get on from the roof).
fn mount(m: &SoldierMotion, ladder: &Ladder, wish: Vec3) -> Option<Vec3> {
    let local = ladder.local(m.position);
    let hold = ladder_hold(ladder);
    if local.x.abs() > ladder.half.x + 0.15 {
        return None;
    }
    if local.z >= 0.0 {
        let within = local.z <= hold + LADDER_REACH
            && local.y >= -ladder.top() - 0.5
            && local.y <= ladder.top() - 0.6;
        // Not right after jumping off it.
        let towards = wish.dot(-ladder.front) > 0.5 && m.velocity.dot(ladder.front) <= 0.5;
        (within && towards).then_some(m.position)
    } else {
        let on_top = m.grounded
            && local.z >= -(hold + LADDER_REACH + 0.6)
            && (local.y - ladder.top()).abs() < 0.7;
        (on_top && wish.dot(ladder.front) > 0.5)
            .then(|| ladder.world(Vec3::new(0.0, ladder.top() - 1.2, hold)))
    }
}

/// BF2's sprint: stamina drains while sprinting and recovers otherwise (not right after a
/// jump). Starting needs a little stamina; running out stops the sprint.
fn update_stamina(m: &mut SoldierMotion, tuning: &SoldierTuning, wants_sprint: bool, dt: f32) {
    let stamina = tuning.stamina(m.heavy);
    if m.sprinting {
        m.stamina = (m.stamina - dt / stamina.sprint_time).max(0.0);
        if !wants_sprint || m.stamina <= 0.0 {
            m.sprinting = false;
        }
    } else {
        if m.stamina_delay <= 0.0 {
            m.stamina = (m.stamina + dt / stamina.recover_time).min(1.0);
        }
        m.sprinting = wants_sprint && m.stamina >= tuning.sprint_min_stamina;
    }
}

fn wanted_stance(input: &InputFrame) -> Stance {
    if input.pressed(Buttons::PRONE) {
        Stance::Prone
    } else if input.pressed(Buttons::CROUCH) {
        Stance::Crouching
    } else {
        Stance::Standing
    }
}

/// Velocity walking along a surface with `normal` at the given horizontal velocity.
fn along_ground(horizontal: Vec3, normal: Vec3) -> Vec3 {
    let rise = -(normal.x * horizontal.x + normal.z * horizontal.z) / normal.y;
    horizontal + Vec3::Y * rise
}

/// BF2's acceleration model: every 1/30 s, close `factor` of the gap to the target.
fn approach(current: Vec3, target: Vec3, factor: f32, dt: f32) -> Vec3 {
    let keep = (1.0 - factor).powf(dt * 30.0);
    let next = target + (current - target) * keep;
    if next.distance_squared(target) < 0.05 * 0.05 {
        target
    } else {
        next
    }
}

/// How move-and-slide treats the surfaces it hits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Contact {
    /// Every surface is what it is: land on floors, slide down steep slopes.
    Air,
    /// Slopes too steep to walk up act as walls.
    Walk,
    /// Every upward-facing surface acts as a wall: the sideways part of a step-up.
    Step,
}

struct Slide {
    center: Vec3,
    velocity: Vec3,
    /// Hit a wall (or something else that can't be walked on).
    blocked: bool,
}

struct Ground {
    /// How far the capsule can move down to rest on it, keeping the skin width.
    gap: f32,
    /// Normal of the surface to walk along.
    normal: Vec3,
    /// Height of the surface the capsule rests on.
    height: f32,
}

impl Ground {
    /// How far to move down to rest on it. Sub-millimeter gaps are left alone, so standing
    /// still doesn't jitter.
    fn gap(&self) -> f32 {
        if self.gap > 1e-4 { self.gap } else { 0.0 }
    }
}

/// Collision queries against the world, for one soldier.
struct Surroundings<'a, 'w, 's> {
    mover: &'a MoveAndSlide<'w, 's>,
    filter: SpatialQueryFilter,
    min_normal_y: f32,
}

impl Surroundings<'_, '_, '_> {
    fn slide(
        &self,
        shape: &Collider,
        center: Vec3,
        velocity: Vec3,
        dt: f32,
        contact: Contact,
    ) -> Slide {
        let min_normal_y = self.min_normal_y;
        let mut blocked = false;
        let out = self.mover.move_and_slide(
            shape,
            center,
            Quat::IDENTITY,
            velocity,
            Duration::from_secs_f32(dt),
            &MoveAndSlideConfig::default(),
            &self.filter,
            |hit| {
                let normal = **hit.normal;
                if normal.y < min_normal_y {
                    blocked = true;
                }
                let as_wall = match contact {
                    Contact::Air => false,
                    Contact::Walk => normal.y > 0.0 && normal.y < min_normal_y,
                    Contact::Step => normal.y > 0.0,
                };
                if as_wall && let Ok(wall) = Dir3::new(Vec3::new(normal.x, 0.0, normal.z)) {
                    *hit.normal = wall;
                }
                MoveAndSlideHitResponse::Accept
            },
        );
        Slide {
            center: out.position,
            velocity: out.projected_velocity,
            blocked,
        }
    }

    /// Lifts the capsule, moves sideways and puts it down again: gets onto surfaces up to
    /// `height` above `feet`. `None` if there is no walkable surface low enough to put it
    /// down on.
    fn step_up(
        &self,
        shape: &Collider,
        center: Vec3,
        feet: f32,
        height: f32,
        horizontal: Vec3,
        dt: f32,
    ) -> Option<Slide> {
        let lift = match self.cast(shape, center, Dir3::Y, height + SKIN) {
            Some(ceiling) => (ceiling.distance - SKIN).max(0.0),
            None => height,
        };
        if lift < 0.02 {
            return None;
        }
        let mut moved = self.slide(
            shape,
            center + Vec3::Y * lift,
            horizontal,
            dt,
            Contact::Step,
        );
        // A little extra reach so the ground it started on counts too: moving forward a bit
        // there is progress when the capsule's rounded bottom doesn't yet reach the step.
        let ground = self.ground(shape, moved.center, lift + 4.0 * SKIN)?;
        if ground.height > feet + height + SKIN {
            // Hanging on the edge of something taller.
            return None;
        }
        moved.center.y -= ground.gap();
        Some(moved)
    }

    /// The walkable ground below the capsule, within `max_distance`.
    ///
    /// The shape cast finds the contact, a ray just past it finds the surface: contact
    /// normals are tilted on edges (stairs, curbs) and noisy on long thin triangles, face
    /// normals are exact.
    fn ground(&self, shape: &Collider, center: Vec3, max_distance: f32) -> Option<Ground> {
        let hit = self.cast(shape, center, Dir3::NEG_Y, max_distance)?;
        if hit.normal1.y <= 0.0 {
            return None;
        }
        // Off the capsule's axis the contact may be on an edge: look at the surface just past
        // it, seen from the axis.
        let off_axis = Vec3::new(hit.point1.x - center.x, 0.0, hit.point1.z - center.z);
        let mut origin = hit.point1 + Vec3::Y * 0.05;
        if off_axis.length_squared() > 0.02 * 0.02 {
            origin += off_axis.normalize() * 0.02;
        }
        let (normal, height) =
            match self
                .mover
                .spatial_query
                .cast_ray(origin, Dir3::NEG_Y, 0.1, false, &self.filter)
            {
                Some(surface) => (surface.normal, origin.y - surface.distance),
                None => (hit.normal1, hit.point1.y),
            };
        if normal.y < self.min_normal_y {
            return None;
        }
        // The cast's distance is short by up to millimeters, more on long thin triangles.
        // When it agrees with the distance to the surface's plane, the capsule rests on the
        // face: use the exact one (with the skin width above it). Further than the plane,
        // the capsule hangs over an edge: the exact distance keeping the skin width from the
        // contact point. Nearer, it touches something else first.
        let half_height = shape.aabb(Vec3::ZERO, Quat::IDENTITY).size().y * 0.5;
        let sphere = center - Vec3::Y * (half_height - SOLDIER_RADIUS);
        let on_plane = Vec3::new(origin.x, height, origin.z);
        let to_plane = (normal.dot(sphere - on_plane) - SOLDIER_RADIUS) / normal.y;
        let across = Vec2::new(hit.point1.x - sphere.x, hit.point1.z - sphere.z).length();
        let reach = SOLDIER_RADIUS + SKIN;
        let gap = if hit.distance > to_plane + 0.006 && across < reach {
            sphere.y - hit.point1.y - (reach * reach - across * across).sqrt()
        } else {
            // Keeping the skin width from everything on the way down, like move-and-slide
            // does: closer than that to a wall, the next move would push the capsule away.
            let clear = self
                .mover
                .cast_move(
                    shape,
                    center,
                    Quat::IDENTITY,
                    Vec3::NEG_Y * max_distance,
                    SKIN,
                    &self.filter,
                )
                .map_or(max_distance, |h| h.distance);
            if (hit.distance - to_plane).abs() < 0.006 {
                (to_plane - SKIN).min(clear)
            } else {
                clear
            }
        };
        Some(Ground {
            gap,
            // Nearly flat counts as flat, so walking never creeps up or down.
            normal: if normal.y > 0.9995 { Vec3::Y } else { normal },
            height,
        })
    }

    fn cast(
        &self,
        shape: &Collider,
        center: Vec3,
        direction: Dir3,
        max_distance: f32,
    ) -> Option<ShapeHitData> {
        self.mover.spatial_query.cast_shape(
            shape,
            center,
            Quat::IDENTITY,
            direction,
            &ShapeCastConfig {
                ignore_origin_penetration: true,
                ..ShapeCastConfig::from_max_distance(max_distance)
            },
            &self.filter,
        )
    }

    /// The closest ladder (or grappling rope) within reach.
    fn ladder(&self, feet: Vec3, shapes: &SoldierShapes) -> Option<Ladder> {
        let filter = SpatialQueryFilter::from_mask([GameLayer::Ladder, GameLayer::Rope]);
        let center = feet + SOLDIER_CENTER;
        self.mover
            .spatial_query
            .shape_intersections(&shapes.ladder_probe, center, Quat::IDENTITY, &filter)
            .into_iter()
            .filter_map(|entity| {
                let (collider, position, rotation, layers) =
                    self.mover.colliders.get(entity).ok()?;
                let half = collider.aabb(Vec3::ZERO, Quat::IDENTITY).size() * 0.5;
                let rope = layers.is_some_and(|l| l.memberships.has_all(GameLayer::Rope));
                Some(Ladder {
                    rope,
                    ..Ladder::from_box(position.0, rotation.0, half)
                })
            })
            // Query order differs between client and server; distance doesn't.
            .min_by(|a, b| {
                a.center
                    .distance_squared(center)
                    .total_cmp(&b.center.distance_squared(center))
            })
    }

    /// The closest zipline wire within reach of the hands.
    fn zipline(&self, hands: Vec3, shapes: &SoldierShapes) -> Option<Wire> {
        let filter = SpatialQueryFilter::from_mask(GameLayer::Zipline);
        self.mover
            .spatial_query
            .shape_intersections(&shapes.zipline_probe, hands, Quat::IDENTITY, &filter)
            .into_iter()
            .filter_map(|entity| {
                let (collider, position, rotation, _) = self.mover.colliders.get(entity).ok()?;
                let capsule = collider.shape().as_capsule()?;
                let a = position.0 + rotation.0 * capsule.segment.a;
                let b = position.0 + rotation.0 * capsule.segment.b;
                let (high, low) = if a.y >= b.y { (a, b) } else { (b, a) };
                let length = high.distance(low);
                let down = (low - high).normalize_or_zero();
                (length > 0.1).then_some(Wire { high, length, down })
            })
            // Query order differs between client and server; distance doesn't.
            .min_by(|a, b| {
                let d = |w: &Wire| w.at(w.along(hands)).distance_squared(hands);
                d(a).total_cmp(&d(b))
            })
    }

    /// The wanted stance, or the tallest one in between that has room.
    fn fit_stance(
        &self,
        feet: Vec3,
        current: Stance,
        wanted: Stance,
        shapes: &SoldierShapes,
    ) -> Stance {
        let taller = |a: Stance, b: Stance| a.collision_height() > b.collision_height();
        let fits = |stance: Stance| {
            self.mover
                .spatial_query
                .shape_intersections(
                    shapes.movement(stance),
                    feet + stance.collision_center(),
                    Quat::IDENTITY,
                    &self.filter,
                )
                .is_empty()
        };
        if !taller(wanted, current) || fits(wanted) {
            wanted
        } else if taller(Stance::Crouching, current) && fits(Stance::Crouching) {
            Stance::Crouching
        } else {
            current
        }
    }
}
