//! Flying and floating: the forces of wings, jet engines, rotors and floaters.
//!
//! Pure functions of a vehicle's motion and its pilot's controls, shared by the server's
//! simulation and the driver's prediction on clients. Like BF2's, the forces are
//! accelerations times the vehicle's mass: a heavier aircraft with the same wings and engines
//! flies the same.
//!
//! * **Wings** (lifting surfaces, fins, rudders, boat keels) push along their normal against
//!   the flow through them in proportion to the airspeed along the hull (lift from the angle
//!   of attack, up to the stall angle; beyond it they only brake like a plate). Control
//!   surfaces add lift in proportion to their deflection, which the stick, rudder and
//!   throttle turn like any other joint.
//! * **Thrusters** (jet engines, ship propellers) push along their axis; the push fades out
//!   towards their top speed. Jets spool their throttle; the afterburner is BF2's vehicle
//!   sprint meter.
//! * **Rotors** hold the helicopter's altitude while the collective is centred and its tilt is
//!   moderate (BF2's vertical regulation), climb and sink with it, and turn the helicopter at
//!   the stick's and rudder's rates, levelling it out when the stick is centred. Jump jets
//!   (the F-35B) have one for hovering: slow, with S held (or parked and holding S), the lift
//!   fan takes over and the jet flies like a helicopter (W/S climb and sink, the stick tilts
//!   it, the engine idles) until it goes fast again or the afterburner is lit.
//! * **Floaters** lift in proportion to how deep they are in the water, and the submerged
//!   hull drags (a keel sideways, little forwards).
//!
//! **Gameplay layer for piloted aircraft** (BF3/BF4-style handling on top of BF2's data; BF2's
//! raw wing torques made jets roll at 20°/s at take-off speed and wander in pitch, and its
//! helicopters flip over and fly backwards at 200 km/h):
//!
//! * Jets fly by wire: the stick and rudder ask for pitch, roll and yaw *rates*
//!   ([`JET_RATES`]), which a controller holds; centred, the jet keeps its attitude. How much
//!   of the rate it gets depends on the airspeed ([`jet_authority`]): best at the corner speed
//!   (a share of the engines' BF2 top speed, see [`JetEnvelope`]), mushy when slow, wider turns
//!   when fast, so throttle and afterburner change the turn radius. BF2's wings still carry
//!   the jet (lift from the angle of attack, times [`JET_LIFT_GAIN`] so the flight path follows
//!   the nose closely) and their induced drag makes hard turns cost speed; the fin keeps the
//!   nose into the airflow ([`JET_WEATHERVANE`]), so rudder yaw and banked turns stay
//!   coordinated. Too slow or past the stall angle the jet stalls ([`jet_stall`]): the stick
//!   loses authority and the nose drops towards where it is going, until it has speed again.
//! * Helicopters: the stick's pitch and roll rates fade out towards [`HELI_MAX_TILT`], the
//!   helicopter levels itself when the stick is let go ([`HELI_LEVELING`]), it turns into its
//!   bank and the tail keeps it into the airflow at speed, flies backwards and sideways only
//!   slowly, and hovers steadily hands off ([`HELI_HOVER_ASSIST`]).

use bevy::prelude::*;
use game_data::VehicleCategory;

use crate::{
    input::{Buttons, InputFrame},
    vehicle::VehicleModel,
};

/// Standard gravity; vehicles multiply it by their gravity modifier.
pub const GRAVITY: f32 = 9.81;

/// A stalled wing still brakes the flow through it like a plate, this much of its lift.
const PLATE: f32 = 0.25;
/// Drag from lift, per m/s² of lift (turning costs speed).
const INDUCED_DRAG: f32 = 0.06;
/// Jets in the air hold this throttle while neither W nor S is pressed.
const CRUISE_THROTTLE: f32 = 0.5;
/// Share of full throttle a jet engine spools per second.
const SPOOL_RATE: f32 = 0.8;
/// Air brake: drag multiplier while slowing down.
const AIR_BRAKE: f32 = 5.0;
/// Landing flaps are fully out below the first speed and in above the second (m/s).
const FLAP_SPEEDS: [f32; 2] = [45.0, 75.0];
/// How quickly the collective corrects the vertical speed, 1/s.
const COLLECTIVE_RESPONSE: f32 = 1.5;
/// Helicopters regulate their altitude and level out only this high above where they sit on
/// their skids (m); lower down they settle onto the ground unless the pilot climbs.
const HOVER_HEIGHT: f32 = 0.6;
/// Jump jets start hovering below the first airspeed (m/s) and fly as jets again above the
/// second.
const VTOL_SPEEDS: [f32; 2] = [35.0, 50.0];
/// Near the ground the collective sinks at most this fast (m/s), plus this much per meter of
/// height, so a helicopter held down touches down gently instead of bouncing off its skids.
const LANDING_SINK: f32 = 1.5;
const LANDING_SINK_PER_METER: f32 = 0.35;
/// Share of its weight the rotor carries while a helicopter sits on the ground.
const GROUNDED_LIFT: f32 = 0.5;

/// Jets' pitch, yaw and roll rates at full stick and rudder at the corner speed, radians per
/// second (48°/s, 29°/s, 170°/s: a loop in about 8 s, a roll in about 2 s).
pub const JET_RATES: Vec3 = Vec3::new(0.84, 0.5, 2.97);
/// How quickly a jet reaches the rates it's asked for, 1/s (pitch, yaw, roll).
const JET_RESPONSE: Vec3 = Vec3::new(6.0, 4.0, 10.0);
/// Jets' corner speed (the best turn rate) as a share of their engines' top speed, and the
/// stall speed as a share of the corner speed.
const JET_CORNER_SHARE: f32 = 0.7;
const JET_STALL_SHARE: f32 = 0.4;
/// Jets' wings lift this much more than BF2's for the angle of attack, so the flight path
/// follows the nose within a few degrees.
pub const JET_LIFT_GAIN: f32 = 1.8;
/// The fin turns the nose into sideways airflow (1/s), and the fuselage pushes against it
/// (1/s of sideways speed).
pub const JET_WEATHERVANE: f32 = 2.5;
const JET_SIDE_GRIP: f32 = 1.8;
/// Stalled, the nose drops towards the flight path this quickly (1/s), aimed this many m/s
/// below it (so a jet that has lost all its speed points down).
const JET_STALL_DROP: f32 = 1.4;
const JET_STALL_SINK: f32 = 12.0;
/// Jets count as airborne (can stall) this high above the ground (m).
const JET_AIRBORNE: f32 = 4.0;

/// Helicopters: the stick's pitch and roll rates fade out over the last `band` degrees before
/// these tilts (pitch, roll).
pub const HELI_MAX_TILT: Vec2 = Vec2::new(30.0, 50.0);
const HELI_TILT_BAND: f32 = 15.0;
/// How quickly a helicopter levels itself with the stick let go (pitch, roll), 1/s; BF2's
/// `leveling` is used if stronger.
pub const HELI_LEVELING: Vec2 = Vec2::new(0.8, 1.2);
/// Above this forward airspeed (m/s, fully by twice it) a helicopter turns into its bank and
/// its tail keeps it into the airflow (1/s).
const HELI_FORWARD_FLIGHT: f32 = 10.0;
const HELI_WEATHERVANE: f32 = 1.5;
/// Extra drag flying backwards and sideways, 1/s.
const HELI_BACKWARD_DRAG: f32 = 0.35;
const HELI_SIDEWAYS_DRAG: f32 = 0.25;
/// With the stick centred and slow (below the speed, m/s), the helicopter's drift dies out
/// (1/s at a standstill, fading out by the speed).
pub const HELI_HOVER_ASSIST: f32 = 0.45;
const HELI_HOVER_SPEED: f32 = 20.0;

/// A jet's speeds that matter to its handling (m/s), from its engines' BF2 top speed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JetEnvelope {
    /// The best turn rate.
    pub corner: f32,
    /// Slower than this it stalls.
    pub stall: f32,
}

impl JetEnvelope {
    pub fn of(desc: &game_data::VehicleDesc) -> Self {
        let top = desc
            .thrusters
            .iter()
            .filter(|t| !t.water)
            .map(|t| t.max_speed)
            .fold(0.0, f32::max);
        let top = if top > 1.0 { top } else { desc.engine.top_speed.max(40.0) };
        let corner = top * JET_CORNER_SHARE;
        Self {
            corner,
            stall: corner * JET_STALL_SHARE,
        }
    }
}

fn smoothstep(from: f32, to: f32, x: f32) -> f32 {
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Share of [`JET_RATES`] (pitch, yaw, roll) a jet gets at an airspeed: control surfaces need
/// airflow, the turn rate peaks at the corner speed and falls off above it (the load limit).
pub fn jet_authority(envelope: &JetEnvelope, airspeed: f32) -> Vec3 {
    let JetEnvelope { corner, stall } = *envelope;
    let airflow = (airspeed / stall).clamp(0.0, 1.0).powi(2);
    let fast = if airspeed > corner { corner / airspeed } else { 1.0 };
    let pitch = (0.35 + 0.65 * smoothstep(stall, corner, airspeed)) * fast.powf(1.2);
    let roll = (0.45 + 0.55 * smoothstep(stall, corner * 0.75, airspeed)) * fast.sqrt().max(0.75);
    let yaw = 0.5 + 0.5 * smoothstep(stall, corner * 0.6, airspeed);
    Vec3::new(pitch, yaw, roll) * airflow
}

/// How far a jet is stalled, 0..1: too slow along its nose, or past its stall angle. Only
/// meaningful in the air.
pub fn jet_stall(desc: &game_data::VehicleDesc, body: &BodyState) -> f32 {
    let (Some(aero), true) = (&desc.aero, desc.category == VehicleCategory::Air) else {
        return 0.0;
    };
    let envelope = JetEnvelope::of(desc);
    let local = body.rotation.inverse() * body.velocity;
    let airspeed = -local.z;
    let aoa = (-local.y).atan2(airspeed.max(0.1)).to_degrees().abs();
    let slow = 1.0 - smoothstep(envelope.stall * 0.75, envelope.stall * 1.05, airspeed);
    let steep = smoothstep(aero.stall_angle * 1.05, aero.stall_angle * 1.5, aoa);
    slow.max(steep)
}

/// Whether a jet is in the air for [`jet_stall`] (not rolling on its wheels).
pub fn jet_airborne(altitude: f32) -> bool {
    altitude > JET_AIRBORNE
}

/// A rigid body's motion in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyState {
    pub position: Vec3,
    pub rotation: Quat,
    pub velocity: Vec3,
    pub angular_velocity: Vec3,
}

impl Default for BodyState {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
        }
    }
}

impl BodyState {
    /// Velocity of a point of the body, given the world-space center of mass.
    pub fn velocity_at(&self, point: Vec3, center_of_mass: Vec3) -> Vec3 {
        self.velocity + self.angular_velocity.cross(point - center_of_mass)
    }
}

/// Forces for one tick: pushes (newtons) at world points, plus a torque.
#[derive(Clone, Debug, Default)]
pub struct Push {
    pub forces: Vec<(Vec3, Vec3)>,
    pub torque: Vec3,
}

/// What the driver or pilot asks for this tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Controls {
    /// W/S: throttle, collective (forward, up positive).
    pub throttle: f32,
    /// A/D: steering, rudder, tail rotor (right positive).
    pub steer: f32,
    /// The stick: pitch up (pulled back) and roll right positive.
    pub pitch: f32,
    pub roll: f32,
    /// Sprint: afterburner.
    pub boost: bool,
    /// Jump: handbrake, wheel brakes.
    pub brake: bool,
    /// Someone is in the driver's seat.
    pub occupied: bool,
}

impl Controls {
    pub fn from_input(input: Option<&InputFrame>) -> Self {
        let Some(input) = input else {
            return Self::default();
        };
        let movement = input.movement_vec();
        let stick = input.stick_vec();
        Self {
            throttle: movement.y,
            steer: movement.x,
            pitch: stick.y,
            roll: stick.x,
            boost: input.pressed(Buttons::SPRINT),
            brake: input.pressed(Buttons::JUMP),
            occupied: true,
        }
    }
}

/// What around the vehicle matters to the forces.
#[derive(Clone, Copy, Debug, Default)]
pub struct Surroundings {
    /// Water surface height, if the level has water.
    pub water: Option<f32>,
    /// Height of the center of mass above the ground or water below (capped by the probe).
    pub altitude: f32,
}

/// Engine state carried from tick to tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlightState {
    /// Jet throttle setting, 0..1 (ships: the throttle input, -1..1).
    pub throttle: f32,
    /// Rotor speed, 0..1.
    pub spin: f32,
    /// Afterburner meter, 0..1.
    pub boost: f32,
    pub boosting: bool,
    /// Landing gear retracted.
    pub gear_up: bool,
    /// Jump jets: the lift fan carries it.
    pub hovering: bool,
}

impl FlightState {
    pub fn new() -> Self {
        Self {
            boost: 1.0,
            ..default()
        }
    }

    /// Engine speed for the visuals and the HUD: rotor speed, or the throttle.
    pub fn engine(&self) -> f32 {
        self.spin.max(self.throttle.abs())
    }
}

/// Jets' fly-by-wire: the stick can't pull the wings far past their stall angle (nor push
/// them as far the other way), so full stick turns as hard as the jet can without stalling.
pub fn limit_pitch(body: &BodyState, stall_angle: f32, pitch: f32) -> f32 {
    const BAND: f32 = 6.0;
    let local = body.rotation.inverse() * body.velocity;
    if local.z > -10.0 {
        return pitch;
    }
    let aoa = (-local.y).atan2(-local.z).to_degrees();
    let limit = stall_angle * 0.8;
    let room = if pitch > 0.0 { limit - aoa } else { limit * 0.6 + aoa };
    pitch * (room / BAND).clamp(0.0, 1.0)
}

/// The body rates (hull space: pitch up, yaw left, roll left) a piloted jet's fly-by-wire
/// holds: the stick's and rudder's share of [`JET_RATES`], the fin's weathervaning, and the
/// nose dropping in a stall.
fn jet_rates(desc: &game_data::VehicleDesc, body: &BodyState, controls: &Controls, around: &Surroundings, airspeed: f32) -> Vec3 {
    let envelope = JetEnvelope::of(desc);
    let authority = jet_authority(&envelope, airspeed);
    let stall = if jet_airborne(around.altitude) { jet_stall(desc, body) } else { 0.0 };
    let asked = Vec3::new(controls.pitch, -controls.steer, -controls.roll) * JET_RATES * authority;
    let mut wanted = asked * (1.0 - 0.7 * stall);
    let inverse = body.rotation.inverse();
    let local = inverse * body.velocity;
    if airspeed > 5.0 {
        // Sideways airflow from the right (positive slip) yaws the nose right.
        let slip = local.x.atan2(airspeed);
        wanted.y -= slip * JET_WEATHERVANE * (airspeed / envelope.stall).min(1.0);
    }
    if stall > 0.0 {
        let forward = body.rotation * Vec3::NEG_Z;
        let towards = (body.velocity + Vec3::NEG_Y * JET_STALL_SINK).normalize_or(Vec3::NEG_Y);
        let turn = inverse * forward.cross(towards);
        wanted.x += turn.x * JET_STALL_DROP * stall;
        wanted.y += turn.y * JET_STALL_DROP * stall;
    }
    wanted
}

/// Wings, thrusters, rotor, floaters and air drag for one tick, and the engine state they
/// leave behind. `joints` are the vehicle's current joint angles (control surfaces).
pub fn flight_forces(
    model: &VehicleModel,
    body: &BodyState,
    joints: &[[f32; 3]],
    controls: &Controls,
    state: &mut FlightState,
    around: &Surroundings,
    dt: f32,
) -> Push {
    let desc = &model.desc;
    let mut push = Push::default();
    let Some(aero) = &desc.aero else {
        return push;
    };
    let mass = desc.physics.mass;
    let g = GRAVITY * desc.physics.gravity;
    let rotation = body.rotation;
    let inverse = rotation.inverse();
    let com = body.position + rotation * Vec3::from(desc.physics.center_of_mass);
    let forward = rotation * Vec3::NEG_Z;
    let local_velocity = inverse * body.velocity;
    let airspeed = (-local_velocity.z).max(0.0);
    let speed = body.velocity.length();

    // Floaters.
    let mut in_water = 0.0;
    if let Some(water) = around.water
        && !desc.floaters.is_empty()
    {
        let share = 1.0 / desc.floaters.len() as f32;
        for floater in &desc.floaters {
            // A floater is a column `depth` meters down from its point.
            let point = body.position + rotation * Vec3::from(floater.position);
            let depth = water - (point.y - floater.depth);
            if depth <= 0.0 {
                continue;
            }
            let submerged = (depth / floater.depth).min(1.0);
            in_water += submerged * share;
            let flow = inverse * body.velocity_at(point, com);
            let drag = -flow * Vec3::from(aero.water_drag) * submerged * share;
            push.forces.push(((Vec3::Y * g * floater.lift * submerged + rotation * drag) * mass, point));
        }
    }

    // Landing gear.
    if let Some(gear) = &desc.landing_gear {
        if state.gear_up {
            if around.altitude < gear.down_height && speed < gear.down_speed {
                state.gear_up = false;
            }
        } else if around.altitude > gear.up_height && speed > gear.up_speed {
            state.gear_up = true;
        }
    }

    // A piloted jet flies by wire (see the module docs): its control surfaces only show the
    // stick, the controller below turns it.
    let jet = desc.category == VehicleCategory::Air;
    if jet && desc.rotor.is_some() {
        if state.hovering {
            state.hovering = controls.occupied && !controls.boost && airspeed < VTOL_SPEEDS[1];
        } else {
            state.hovering = controls.occupied && controls.throttle < 0.0 && airspeed < VTOL_SPEEDS[0];
        }
    }
    let fly_by_wire = jet && controls.occupied && !state.hovering;

    // Wings. Land vehicles' rudders only work afloat.
    if desc.category != VehicleCategory::Land || in_water > 0.0 {
        let tan_stall = aero.stall_angle.to_radians().tan();
        let gain = if fly_by_wire { JET_LIFT_GAIN } else { 1.0 };
        let mut lift = Vec3::ZERO;
        let mut wings = Vec::with_capacity(desc.wings.len());
        for wing in &desc.wings {
            let rest_normal = model.rest_rotation(wing.part as usize) * Vec3::Y;
            // BF2 sets some wings a few degrees nose down (the J-10's by 2.5°), which made the
            // jet fly nose high; by wire, the lifting wings lift from the hull's attitude.
            let rest_normal = if fly_by_wire && rest_normal.y > 0.95 { Vec3::Y } else { rest_normal };
            let normal = rotation * rest_normal;
            let point = body.position + rotation * Vec3::from(wing.position);
            let flow = body.velocity_at(point, com);
            let along = flow.dot(forward).max(0.0);
            let through = flow.dot(normal);
            let lifting = through.clamp(-along * tan_stall, along * tan_stall);
            let stalled = through - lifting;
            let deflection = if wing.landing_flap {
                let [full, none] = FLAP_SPEEDS;
                if state.gear_up { 0.0 } else { 1.0 - ((along - full) / (none - full)).clamp(0.0, 1.0) }
            } else if fly_by_wire {
                0.0
            } else {
                model.deflection(joints, wing.part as usize)
            };
            let accel = -wing.lift * (gain * lifting * along + PLATE * stalled * stalled.abs())
                + wing.flap_lift * deflection * along * along;
            // Landing flaps only add lift; where BF2 puts them would pitch the jet over.
            // Flying by wire, the wings only carry the jet; the controller turns it.
            wings.push((normal * accel, if wing.landing_flap || fly_by_wire { com } else { point }));
            lift += normal * accel;
        }
        if fly_by_wire {
            // The fuselage pushes against sideways airflow.
            let side = rotation * Vec3::X;
            let slip = body.velocity.dot(side);
            wings.push((-side * slip * JET_SIDE_GRIP, com));
            // Trimmed: the wings carry the jet's weight with the nose on its flight path (less
            // so banked, not at all towards the stall), so hands off it flies where it points.
            let envelope = JetEnvelope::of(desc);
            let carried = smoothstep(envelope.stall * 0.8, envelope.stall * 1.3, airspeed);
            let up = rotation * Vec3::Y;
            let trim = up * g * up.y.max(0.0) * carried;
            wings.push((trim, com));
            lift += trim;
        }
        // The airframe can't take more than its load limit.
        let scale = if lift.length() > aero.max_load { aero.max_load / lift.length() } else { 1.0 };
        for (accel, point) in wings {
            push.forces.push((accel * scale * mass, point));
        }
        if let Ok(direction) = Dir3::new(body.velocity) {
            push.forces.push((-direction * lift.length() * scale * INDUCED_DRAG * mass, com));
        }
    }

    // Air drag, and the air brake of jets slowing down.
    let brake = if desc.category == VehicleCategory::Air && controls.throttle < 0.0 { AIR_BRAKE } else { 1.0 };
    let drag = -local_velocity * speed * Vec3::from(desc.physics.drag_modifier) * aero.drag * brake;
    push.forces.push((rotation * drag * mass, com));

    // Aerodynamic damping of rotation (flying by wire, the controller damps it).
    let omega = inverse * body.angular_velocity;
    if fly_by_wire {
        push.torque += rotation * (jet_rates(desc, body, controls, around, airspeed) - omega) * JET_RESPONSE * model.inertia;
    } else {
        let damping = Vec3::from(aero.angular_damping) + Vec3::from(aero.speed_damping) * airspeed;
        push.torque += rotation * (-omega * damping * model.inertia);
    }

    // Thrusters.
    if jet {
        // On the ground the engines idle unless the pilot opens the throttle.
        let parked = around.altitude < 5.0 && speed < 20.0;
        let target = match controls.throttle {
            _ if !controls.occupied || state.hovering => 0.0,
            t if t > 0.0 => 1.0,
            t if t < 0.0 => 0.0,
            _ if parked => 0.0,
            _ => CRUISE_THROTTLE,
        };
        state.throttle += (target - state.throttle).clamp(-SPOOL_RATE * dt, SPOOL_RATE * dt);
    } else {
        state.throttle = controls.throttle;
    }
    let mut boost = 1.0;
    if let Some(afterburner) = &desc.afterburner {
        let wanted = controls.boost && controls.occupied && controls.throttle >= 0.0;
        state.boosting = wanted && state.boost > 0.0 && (state.boosting || state.boost >= afterburner.min_charge);
        let rate = if state.boosting { -1.0 / afterburner.duration.max(0.1) } else { 1.0 / afterburner.recover.max(0.1) };
        state.boost = (state.boost + rate * dt).clamp(0.0, 1.0);
        if state.boosting {
            boost = afterburner.factor;
        }
    }
    for thruster in &desc.thrusters {
        let point = body.position + rotation * Vec3::from(thruster.position);
        if thruster.water && around.water.is_none_or(|water| point.y > water - 0.1) {
            continue;
        }
        let direction = rotation * Vec3::from(thruster.direction);
        let along = body.velocity.dot(direction);
        // The afterburner also raises the speed the push fades out at.
        let top = thruster.max_speed.max(1.0) * boost;
        let setting = if state.throttle >= 0.0 { state.throttle * boost } else { state.throttle * thruster.reverse };
        let fade = if setting >= 0.0 {
            1.0 - along / top
        } else {
            1.0 + along / (top * 0.3)
        }
        .clamp(0.0, 1.0);
        push.forces.push((direction * thruster.acceleration * setting * fade * mass, point));
    }

    // Rotor (a jump jet's lift fan only while it hovers).
    if let Some(rotor) = &desc.rotor {
        let running = controls.occupied && (!jet || state.hovering);
        let (target, rate) = if running { (1.0, 1.0) } else { (0.0, 0.5) };
        let rate = rate / rotor.spin_up.max(0.1);
        state.spin += (target - state.spin).clamp(-rate * dt, rate * dt);
        let power = state.spin * state.spin;
        let up = rotation * Vec3::Y;
        // Height above where it sits: its center of mass stands this high over its skids.
        let resting = desc
            .wheels
            .iter()
            .map(|w| desc.physics.center_of_mass[1] - (w.position[1] - w.radius))
            .fold(None, |most: Option<f32>, h| Some(most.map_or(h, |m| m.max(h))))
            .unwrap_or(1.0);
        let clearance = around.altitude - resting;
        let airborne = clearance > HOVER_HEIGHT;
        let tilt = up.angle_between(Vec3::Y).to_degrees();
        let span = (rotor.no_regulation_angle - rotor.regulation_angle).max(1.0);
        let regulation = if airborne { 1.0 - ((tilt - rotor.regulation_angle) / span).clamp(0.0, 1.0) } else { 0.0 };
        let climb = if controls.throttle >= 0.0 {
            controls.throttle * rotor.climb_speed[0]
        } else {
            let limit = LANDING_SINK + clearance.max(0.0) * LANDING_SINK_PER_METER;
            controls.throttle * rotor.climb_speed[1].min(limit)
        };
        // Held altitude: whatever push keeps the vertical speed at what the collective asks,
        // making up for what the wings and drag do.
        let others: f32 = push.forces.iter().map(|(f, _)| f.y).sum::<f32>() / mass;
        let regulated = (g - others + (climb - body.velocity.y) * COLLECTIVE_RESPONSE) / up.y.max(0.35);
        // Sitting on its skids it leans on them (and grips the ground), lifting only once the
        // pilot pulls up.
        let free = if airborne || controls.throttle > 0.0 {
            g * (1.0 + controls.throttle * rotor.lift_margin)
        } else {
            g * GROUNDED_LIFT
        };
        let thrust = (free + (regulated - free) * regulation).clamp(0.0, g * (1.0 + rotor.lift_margin)) * power;
        // Tilting further than the regulation reaches doesn't push harder sideways.
        let horizontal = (up - Vec3::Y * up.y).clamp_length_max(rotor.regulation_angle.to_radians().sin());
        let direction = Vec3::Y * up.y + horizontal * rotor.horizontal_magnifier;
        let flat_velocity = body.velocity - Vec3::Y * body.velocity.y;
        let mut drag = flat_velocity * rotor.horizontal_damping;
        let right = rotation * Vec3::X;
        let flat_forward = (forward - Vec3::Y * forward.y).normalize_or_zero();
        let flat_right = (right - Vec3::Y * right.y).normalize_or_zero();
        let centred = controls.pitch.abs() < 0.05 && controls.roll.abs() < 0.05;
        if airborne {
            // Backwards and sideways it's slow; hands off and slow, the drift dies out.
            let ahead = flat_velocity.dot(flat_forward);
            drag += flat_forward * ahead.min(0.0) * HELI_BACKWARD_DRAG
                + flat_right * flat_velocity.dot(flat_right) * HELI_SIDEWAYS_DRAG;
            if centred && controls.occupied {
                let slow = 1.0 - (flat_velocity.length() / HELI_HOVER_SPEED).min(1.0);
                drag += flat_velocity * HELI_HOVER_ASSIST * slow;
            }
        }
        push.forces.push(((direction * thrust - drag * power) * mass, com));

        // The stick and rudder turn it at their rates (less towards its tilt limits);
        // centred, it levels out.
        let rates = Vec3::from(rotor.turn_rates);
        let pitch = forward.y.clamp(-1.0, 1.0).asin();
        let roll = (-right.y).clamp(-1.0, 1.0).asin();
        let room = |angle: f32, limit: f32, ask: f32| {
            let toward = if ask > 0.0 { limit - angle.to_degrees() } else { limit + angle.to_degrees() };
            (toward / HELI_TILT_BAND).clamp(0.0, 1.0)
        };
        let mut wanted = Vec3::new(
            controls.pitch * rates.x * room(pitch, HELI_MAX_TILT.x, controls.pitch),
            -controls.steer * rates.y,
            -controls.roll * rates.z * room(roll, HELI_MAX_TILT.y, controls.roll),
        );
        if airborne {
            if controls.pitch.abs() < 0.05 {
                wanted.x -= pitch * rotor.leveling.max(HELI_LEVELING.x);
            }
            if controls.roll.abs() < 0.05 {
                wanted.z += roll * rotor.leveling.max(HELI_LEVELING.y);
            }
            // In forward flight it turns into its bank, and the tail keeps it into the
            // airflow.
            let ahead = -local_velocity.z;
            let forward_flight = smoothstep(HELI_FORWARD_FLIGHT, HELI_FORWARD_FLIGHT * 2.0, ahead);
            if forward_flight > 0.0 {
                // Turning about the vertical, whatever the attitude (pitched down and banked,
                // turning about its own yaw axis would swing the nose the wrong way).
                let bank_turn = g * roll.clamp(-1.2, 1.2).tan() / ahead.max(1.0);
                let slip = local_velocity.x.atan2(ahead);
                let turn = (bank_turn.clamp(-rates.y, rates.y) + slip * HELI_WEATHERVANE) * forward_flight;
                wanted += inverse * (Vec3::NEG_Y * turn);
            }
        }
        push.torque += rotation * ((wanted - omega) * rotor.response * model.inertia * state.spin);
    }
    push
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centred_controls_from_no_input() {
        let controls = Controls::from_input(None);
        assert!(!controls.occupied);
        assert_eq!(controls.throttle, 0.0);
    }
}
