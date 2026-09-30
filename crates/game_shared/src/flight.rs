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
//!   the nose closely, and scaled with the world gravity like BF2's, [`jet_lift_scale`]) and
//!   their induced drag (BF2's, [`bf2_lift_sum`]) makes hard turns cost speed; the fin keeps the
//!   nose into the airflow ([`JET_WEATHERVANE`]), so rudder yaw and banked turns stay
//!   coordinated. Too slow or past the stall angle the jet stalls ([`jet_stall`]): the stick
//!   loses authority and the nose drops towards where it is going, until it has speed again.
//! * Helicopters: the stick and pedals get their rates BF3-quick ([`HELI_RESPONSE`]); the
//!   stick's pitch and roll rates fade out towards [`HELI_MAX_TILT`] (far nose down, for
//!   rocket runs: it dives), the helicopter levels itself when the stick is let go
//!   ([`HELI_LEVELING`]), it turns into its bank and the tail keeps it into the airflow at
//!   speed, while a pedal turn banks it a little and the fuselage carries the flight path
//!   round after the nose, flies backwards and sideways only slowly, and hovers steadily hands
//!   off ([`HELI_HOVER_ASSIST`]).

use bevy::prelude::*;
use game_data::VehicleCategory;

use crate::{
    input::{Buttons, InputFrame},
    vehicle::VehicleModel,
};

/// A stalled wing still brakes the flow through it like a plate, this much of its lift.
const PLATE: f32 = 0.25;
/// Helicopters' and boats' drag from lift, per m/s² of lift (turning costs speed). Jets get
/// BF2's instead (see [`bf2_lift_sum`]).
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
pub const VTOL_SPEEDS: [f32; 2] = [35.0, 50.0];
/// Near the ground the collective sinks at most this fast (m/s), plus this much per meter of
/// height, so a helicopter held down touches down gently instead of bouncing off its skids.
const LANDING_SINK: f32 = 1.5;
const LANDING_SINK_PER_METER: f32 = 0.35;
/// Share of its weight the rotor carries while a helicopter sits on the ground.
const GROUNDED_LIFT: f32 = 0.5;

/// Jets' pitch, yaw and roll rates at full stick and rudder at the corner speed, radians per
/// second (57°/s, 29°/s, 170°/s: a loop in about 7 s, a roll in about 2 s). BF2's raw wings
/// pitch its jets at 40-55°/s at the corner speed and 45-80°/s faster (`tests/flight.rs`,
/// `jet_mouse`, measures them).
pub const JET_RATES: Vec3 = Vec3::new(1.0, 0.5, 2.97);
/// How quickly a jet reaches the rates it's asked for, 1/s (pitch, yaw, roll).
const JET_RESPONSE: Vec3 = Vec3::new(9.0, 4.0, 10.0);
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

/// Helicopters: how far the stick tilts them (degrees: nose down, nose up, bank); its rates
/// fade out over the last `HELI_TILT_BAND` degrees. Far nose down for rocket runs: past
/// [`HELI_DIVE`] the collective lets go of the altitude and it dives.
pub const HELI_MAX_TILT: Vec3 = Vec3::new(75.0, 35.0, 55.0);
const HELI_TILT_BAND: f32 = 10.0;
/// Hovering jump jets: pitch either way and bank (degrees), and their band.
const VTOL_MAX_TILT: Vec2 = Vec2::new(30.0, 50.0);
const VTOL_TILT_BAND: f32 = 15.0;
/// How quickly a piloted helicopter reaches the rates the stick and pedals ask for (pitch,
/// yaw, roll), 1/s: like BF3/BF4, half the rate in about 60 ms and nearly all by 200 ms
/// (BF2's 4/s took over half a second). BF2's `response` is used if quicker.
pub const HELI_RESPONSE: Vec3 = Vec3::new(11.0, 12.0, 12.0);
/// Nose down past the first pitch (degrees) the collective stops holding the altitude, fully
/// by the second: a dive. Its forward push grows no further than at `HELI_DIVE[0]`'s tilt.
const HELI_DIVE: [f32; 2] = [35.0, 60.0];
/// In forward flight the pedals bank the helicopter this far into their turn (radians, at full
/// pedal, with the stick's roll centred), so a pedal turn looks and flies coordinated.
const HELI_PEDAL_BANK: f32 = 0.2;
/// Share of the pedals' turn rate left in fast forward flight (by this airspeed, m/s).
const HELI_PEDAL_FAST: f32 = 0.6;
const HELI_PEDAL_FAST_SPEED: f32 = 50.0;
/// In forward flight the fuselage and tail turn sideways drift into the nose's direction
/// (1/s, times the slip angle), costing a little speed (1/s, times the slip angle): the
/// flight path follows a pedal turn instead of skidding.
const HELI_FUSELAGE_GRIP: f32 = 2.5;
const HELI_SLIP_DRAG: f32 = 0.3;
/// Piloted, the collective holds the altitude up to this tilt and fades out by the second
/// (degrees; at the stick's pitch and bank limits together it's tilted 56°).
const HELI_REGULATION: [f32; 2] = [58.0, 75.0];
/// How quickly a helicopter levels itself with the stick let go (pitch, roll), 1/s; BF2's
/// `leveling` is used if stronger.
pub const HELI_LEVELING: Vec2 = Vec2::new(0.8, 1.2);
/// Above this forward airspeed (m/s, fully by twice it) a helicopter turns into its bank and
/// its tail keeps it into the airflow (1/s).
const HELI_FORWARD_FLIGHT: f32 = 10.0;
const HELI_WEATHERVANE: f32 = 1.5;
/// Extra drag flying backwards and sideways, 1/s.
const HELI_BACKWARD_DRAG: f32 = 0.5;
const HELI_SIDEWAYS_DRAG: f32 = 0.5;
/// With the stick centred and slow (below the speed, m/s), the helicopter's drift dies out
/// (1/s at a standstill, fading out by the speed).
pub const HELI_HOVER_ASSIST: f32 = 0.45;
const HELI_HOVER_SPEED: f32 = 20.0;
/// Share of a tilted rotor's sideways thrust that pushes it along (and turns it in a
/// bank). The rotor holds the altitude against BF2's world gravity (14.73 m/s²), so its
/// sideways thrust at a tilt is 1.5× what it was when helicopters were tuned under 9.81:
/// this share keeps their speeds and turns (an AH-1Z held 30° nose down settles at
/// 246 km/h, 253 before, 319 with all of it; a UH-60 at 153 km/h, 151 before).
const ROTOR_TILT_PUSH: f32 = 0.7;

/// The gravity BF2's wing lift is made for, m/s². `BF2.exe` scales every wing's lift by the
/// world gravity over this (its wing update multiplies `(wingLift + flapLift)` by the physics
/// world's gravity and by 1/9.82), so its jets carry their weight at the same speeds whatever
/// the world gravity; the engines' thrust and the drag aren't scaled.
pub const BF2_LIFT_GRAVITY: f32 = 9.82;

/// How much more a jet's wings lift under BF2's world gravity than the numbers they were
/// imported with (fitted by flying under 9.81, where BF2's scale is 1): world gravity / 9.82.
pub fn jet_lift_scale() -> f32 {
    crate::physics::WORLD_GRAVITY / BF2_LIFT_GRAVITY
}

/// BF2's lift coefficient of a wing at an angle of attack (radians), as `BF2.exe` computes it:
/// 0.25 sin α plus 0.75 times a curve rising from 0 at 0° to 1 at 22.5° and back to 0 at 45°
/// (its wing then pushes `coefficient × v² × (wingLift + flapLift) × 0.0025 × gravity / 9.82`
/// m/s² along its normal).
pub fn bf2_lift_coefficient(aoa: f32) -> f32 {
    let deg = aoa.to_degrees().clamp(-90.0, 90.0);
    let curve = if deg.abs() < 45.0 { deg * (45.0 - deg.abs()) * 4.0 / 2025.0 } else { 0.0 };
    0.25 * aoa.sin() + 0.75 * curve
}

/// [`bf2_lift_coefficient`]'s slope at small angles, per radian.
pub const BF2_LIFT_SLOPE: f32 = 0.25 + 0.75 * 4.0 * 45.0 / 2025.0 * (180.0 / std::f32::consts::PI);

/// The importer's units for BF2's `setWingLift` and `setFlapLift` (`bf2_import::vehicles`) and
/// the share it gives landing flaps, to recover BF2's numbers from a jet's wings.
const IMPORT_LIFT_PER_WING_LIFT: f32 = 0.01;
const IMPORT_LIFT_PER_FLAP_LIFT: f32 = 0.003;
const IMPORT_LANDING_FLAP_SHARE: f32 = 0.3;

/// BF2's lift of a jet's horizontal wings together under the world gravity: the level lift
/// (m/s²) is this times [`bf2_lift_coefficient`] times the airspeed squared.
pub fn bf2_lift_sum(model: &VehicleModel) -> f32 {
    let desc = &model.desc;
    desc.wings
        .iter()
        .filter(|w| (model.rest_rotation(w.part as usize) * Vec3::Y).y > 0.7)
        .map(|w| {
            let share = if w.landing_flap { IMPORT_LANDING_FLAP_SHARE } else { 1.0 };
            (w.lift / IMPORT_LIFT_PER_WING_LIFT + w.flap_lift / IMPORT_LIFT_PER_FLAP_LIFT) / share
        })
        .sum::<f32>()
        * 0.0025
        * jet_lift_scale()
}

/// BF2's `drag` of the fighters (all 0.05), which the importer's drag units were fitted to.
const FIGHTER_DRAG: f32 = 0.05;

/// Share of its imported air drag a jet flies with: 1 for the fighters, the square root of
/// their `drag` over its own for the heavier ones (the A-10's and Su-39's 0.5 and 0.4, as the
/// helicopters' `drag` counts). BF2 gives them the fighters' engines (the A-10's two are the
/// F/A-18's) and the same 125 m/s where the thrust ends, twice the gravity and wings to carry
/// it; ten times the fighters' drag held them below 180 km/h and to a 3 m/s climb even under
/// 9.81, with the square root they cruise near 290 km/h.
pub fn jet_drag_scale(desc: &game_data::VehicleDesc) -> f32 {
    if desc.category == VehicleCategory::Air {
        (FIGHTER_DRAG / desc.physics.drag.max(1e-3)).sqrt().min(1.0)
    } else {
        1.0
    }
}

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
/// The pitch rate stays near BF2's at every speed: about 32°/s just above the stall, 42°/s at
/// 60 m/s, 43°/s at full throttle, 34°/s with the afterburner (where the load limit holds the
/// flight path to about 30°/s anyway).
pub fn jet_authority(envelope: &JetEnvelope, airspeed: f32) -> Vec3 {
    let JetEnvelope { corner, stall } = *envelope;
    let airflow = (airspeed / stall).clamp(0.0, 1.0).powi(2);
    let fast = if airspeed > corner { corner / airspeed } else { 1.0 };
    let pitch = (0.5 + 0.5 * smoothstep(stall, corner, airspeed)) * fast.powf(0.8);
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

/// A jet's pitch, yaw and roll rates at full stick and rudder at an airspeed (radians per
/// second, before a stall takes its share).
pub fn jet_full_rates(desc: &game_data::VehicleDesc, airspeed: f32) -> Vec3 {
    JET_RATES * jet_authority(&JetEnvelope::of(desc), airspeed)
}

/// Mouse flying for jets, BF3/BF4-style: the mouse turns the nose the way it turns the view on
/// foot. Each count asks for [`JET_MOUSE_GAIN`] radians of turn (pitch, yaw, roll; times the
/// jet mouse sensitivity) whatever the airspeed, which the jet flies off at up to its rates
/// ([`jet_full_rates`]), catching up with the mouse in about 1/[`JET_MOUSE_FOLLOW`] s. When
/// the mouse stops the turn stops and the fly-by-wire holds the attitude: a climb is one
/// mouse movement, not a spring to hold against (the helicopters' mouse stick centres itself
/// in 1/8 s, so a jet's nose only kept coming up while the mouse kept moving), and a quick
/// movement isn't cut off at full stick: up to [`JET_MOUSE_BACKLOG`] seconds of the jet's
/// full rate wait to be flown (moving the mouse faster than the jet turns, that much turn
/// follows after the mouse stops; more is dropped).
pub const JET_MOUSE_GAIN: Vec3 = Vec3::new(0.0035, 0.0035, 0.007);
pub const JET_MOUSE_FOLLOW: f32 = 15.0;
pub const JET_MOUSE_BACKLOG: f32 = 0.3;
/// The mouse is flown off as if the jet had at least this share of [`JET_RATES`] (parked or
/// slow, the rates are near zero and the backlog would build up for the take-off).
const JET_MOUSE_MIN_RATES: f32 = 0.3;

/// The turn the mouse has asked a jet for and it hasn't flown yet (see [`JET_MOUSE_GAIN`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JetMouse {
    /// Radians: pitch up, yaw right, roll right.
    pub owed: Vec3,
}

impl JetMouse {
    /// Adds this frame's mouse movement (counts times the sensitivity: up, right for the
    /// rudder, right for the roll) and returns the stick share that flies it off (pitch up,
    /// rudder right, roll right; -1..1 in the input frame's steps), for a jet that turns at
    /// `rates` (pitch, yaw, roll, radians per second) at full stick. `limit_pitch` is what the
    /// fly-by-wire's angle of attack limit leaves of a pitch stick ([`limit_pitch`]), so a
    /// pull or push it holds back is flown once it allows.
    pub fn update(&mut self, counts: Vec3, rates: Vec3, dt: f32, limit_pitch: impl Fn(f32) -> f32) -> Vec3 {
        let rates = rates.max(JET_RATES * JET_MOUSE_MIN_RATES);
        let backlog = rates * JET_MOUSE_BACKLOG;
        self.owed = (self.owed + counts * JET_MOUSE_GAIN).clamp(-backlog, backlog);
        // Quantised like `InputFrame::set_stick`, so what's taken off is what the jet gets.
        let stick = (self.owed * JET_MOUSE_FOLLOW / rates).clamp(Vec3::NEG_ONE, Vec3::ONE);
        let stick = (stick * 127.0).round() / 127.0;
        let flown = Vec3::new(limit_pitch(stick.x), stick.y, stick.z);
        let left = self.owed - flown * rates * dt;
        // Too little left to move the stick a step (or flown past): done.
        self.owed = Vec3::select(stick.cmpeq(Vec3::ZERO) | (left * self.owed).cmplt(Vec3::ZERO), Vec3::ZERO, left);
        stick
    }
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
    let g = crate::physics::gravity(desc.physics.gravity);
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
        // BF2 scales jets' wing lift with the world gravity (helicopters keep the lift they
        // were tuned with).
        let lift_scale = if jet { jet_lift_scale() } else { 1.0 };
        let mut lift = Vec3::ZERO;
        let mut wings = Vec::with_capacity(desc.wings.len());
        // A piloted helicopter's fins: BF2's are made for its own model; nose down in forward
        // flight and banked, the air seems to come from the side and they yawed it out of its
        // turns. The tail weathervaning below keeps it straight instead.
        let assisted_helicopter = desc.category == VehicleCategory::Helicopter && controls.occupied;
        for wing in &desc.wings {
            let rest_normal = model.rest_rotation(wing.part as usize) * Vec3::Y;
            if assisted_helicopter && rest_normal.y.abs() < 0.5 {
                continue;
            }
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
            let accel = (-wing.lift * (gain * lifting * along + PLATE * stalled * stalled.abs())
                + wing.flap_lift * deflection * along * along)
                * lift_scale;
            // Landing flaps only add lift; where BF2 puts them would pitch the jet over.
            // Flying by wire, the wings only carry the jet; the controller turns it.
            let at_com = wing.landing_flap || fly_by_wire || assisted_helicopter;
            wings.push((normal * accel, if at_com { com } else { point }));
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
            let lift = lift.length() * scale;
            let drag = if jet {
                // BF2's: its wings push along their normals, so their lift leans back by the
                // angle of attack it takes them ([`bf2_lift_sum`]); ours lean back by the
                // hull's (the trim carries the weight nose on), this makes up the difference,
                // at most INDUCED_DRAG of the lift (hard turns slow, bleeding speed as they
                // were tuned to).
                let needed = lift / (bf2_lift_sum(model) * BF2_LIFT_SLOPE * speed * speed).max(1e-3);
                let aoa = (-local_velocity.y / airspeed.max(1.0)).max(0.0);
                lift * (needed - aoa).clamp(0.0, INDUCED_DRAG)
            } else {
                lift * INDUCED_DRAG
            };
            push.forces.push((-direction * drag * mass, com));
        }
    }

    // Air drag, and the air brake of jets slowing down.
    let brake = if desc.category == VehicleCategory::Air && controls.throttle < 0.0 { AIR_BRAKE } else { 1.0 };
    let drag = -local_velocity * speed * Vec3::from(desc.physics.drag_modifier) * aero.drag * brake * jet_drag_scale(desc);
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
        let helicopter = desc.category == VehicleCategory::Helicopter && controls.occupied;
        let right = rotation * Vec3::X;
        let pitch = forward.y.clamp(-1.0, 1.0).asin();
        let roll = (-right.y).clamp(-1.0, 1.0).asin();
        // Piloted helicopters hold their altitude as far as the stick banks them (BF2's 35°
        // lost height in every banked turn), but steeply nose down they dive.
        let (regulated_to, unregulated_from) = if helicopter {
            (rotor.regulation_angle.max(HELI_REGULATION[0]), rotor.no_regulation_angle.max(HELI_REGULATION[1]))
        } else {
            (rotor.regulation_angle, rotor.no_regulation_angle)
        };
        let span = (unregulated_from - regulated_to).max(1.0);
        let dive = if helicopter { smoothstep(HELI_DIVE[0], HELI_DIVE[1], -pitch.to_degrees()) } else { 0.0 };
        let regulation = if airborne { (1.0 - ((tilt - regulated_to) / span).clamp(0.0, 1.0)) * (1.0 - dive) } else { 0.0 };
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
        // Tilting further than the regulation reaches doesn't push harder sideways, nor does
        // diving steeper than a dive begins.
        let horizontal = (up - Vec3::Y * up.y).clamp_length_max(rotor.regulation_angle.to_radians().sin());
        let steep = if helicopter { (up.y / HELI_DIVE[0].to_radians().cos()).min(1.0) } else { 1.0 };
        let direction = Vec3::Y * up.y + horizontal * rotor.horizontal_magnifier * steep * ROTOR_TILT_PUSH;
        let flat_velocity = body.velocity - Vec3::Y * body.velocity.y;
        let mut drag = flat_velocity * rotor.horizontal_damping;
        let flat_forward = (forward - Vec3::Y * forward.y).normalize_or_zero();
        // Level right of the nose (the right wing's own direction swings forward or back
        // when the helicopter is both pitched and banked).
        let flat_right = flat_forward.cross(Vec3::Y);
        let centred = controls.pitch.abs() < 0.05 && controls.roll.abs() < 0.05;
        // Measured level (a helicopter flies nose down: in its own frame the air comes from
        // below, and banked, from the side).
        let ahead = flat_velocity.dot(flat_forward);
        let slip = flat_velocity.dot(flat_right).atan2(ahead.max(0.1));
        let forward_flight = if airborne { smoothstep(HELI_FORWARD_FLIGHT, HELI_FORWARD_FLIGHT * 2.0, ahead) } else { 0.0 };
        if airborne {
            // Backwards and sideways it's slow; hands off and slow, the drift dies out.
            drag += flat_forward * ahead.min(0.0) * HELI_BACKWARD_DRAG
                + flat_right * flat_velocity.dot(flat_right) * HELI_SIDEWAYS_DRAG;
            if centred && controls.occupied {
                let slow = 1.0 - (flat_velocity.length() / HELI_HOVER_SPEED).min(1.0);
                drag += flat_velocity * HELI_HOVER_ASSIST * slow;
            }
            // In forward flight the fuselage carries the flight path round after the nose
            // (turning the velocity rather than braking it), for a little speed.
            let speed = flat_velocity.length();
            if helicopter && forward_flight > 0.0 && speed > 1.0 {
                let along = flat_velocity / speed;
                let toward = (flat_forward - along * along.dot(flat_forward)).normalize_or_zero();
                let grip = slip.abs() * speed * forward_flight;
                drag -= toward * grip * HELI_FUSELAGE_GRIP;
                drag += along * grip * HELI_SLIP_DRAG;
            }
        }
        push.forces.push(((direction * thrust - drag * power) * mass, com));

        // The stick and pedals turn it at their rates (the stick less towards its tilt
        // limits); centred, it levels out.
        let rates = Vec3::from(rotor.turn_rates);
        // Room left towards a limit (degrees below and above 0), 0..1.
        let room = |angle: f32, [below, above]: [f32; 2], band: f32, ask: f32| {
            let toward = if ask > 0.0 { above - angle.to_degrees() } else { below + angle.to_degrees() };
            (toward / band).clamp(0.0, 1.0)
        };
        let (pitch_limits, bank, band) = if desc.category == VehicleCategory::Helicopter {
            ([HELI_MAX_TILT.x, HELI_MAX_TILT.y], HELI_MAX_TILT.z, HELI_TILT_BAND)
        } else {
            ([VTOL_MAX_TILT.x; 2], VTOL_MAX_TILT.y, VTOL_TILT_BAND)
        };
        // The pedals turn it quickest in a hover; fast, the fin holds against them.
        let pedal_authority = if helicopter { 1.0 - (1.0 - HELI_PEDAL_FAST) * smoothstep(HELI_FORWARD_FLIGHT, HELI_PEDAL_FAST_SPEED, ahead) } else { 1.0 };
        let mut wanted = Vec3::new(
            controls.pitch * rates.x * room(pitch, pitch_limits, band, controls.pitch),
            -controls.steer * rates.y * pedal_authority,
            -controls.roll * rates.z * room(roll, [bank; 2], band, controls.roll),
        );
        if airborne {
            if controls.pitch.abs() < 0.05 {
                wanted.x -= pitch * rotor.leveling.max(HELI_LEVELING.x);
            }
            if controls.roll.abs() < 0.05 {
                // Levels out, or at speed banks a little into a pedal turn.
                let pedal_bank = if helicopter { controls.steer.clamp(-1.0, 1.0) * HELI_PEDAL_BANK * forward_flight } else { 0.0 };
                wanted.z += (roll - pedal_bank) * rotor.leveling.max(HELI_LEVELING.y);
            }
            // In forward flight it turns into its bank (about the vertical), and the tail keeps
            // it into the airflow, except while the pedals turn it out of it.
            if forward_flight > 0.0 {
                let bank_turn = g * ROTOR_TILT_PUSH * roll.clamp(-1.2, 1.2).tan() / ahead.max(1.0);
                let pedals = if helicopter { controls.steer.abs().min(1.0) } else { 0.0 };
                let turn = (bank_turn.clamp(-rates.y, rates.y) + slip * HELI_WEATHERVANE * (1.0 - pedals)) * forward_flight;
                wanted += inverse * (Vec3::NEG_Y * turn);
            }
        }
        let response = if helicopter { HELI_RESPONSE.max(Vec3::splat(rotor.response)) } else { Vec3::splat(rotor.response) };
        push.torque += rotation * ((wanted - omega) * response * model.inertia * state.spin);
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

    #[test]
    fn jets_turn_best_at_the_corner_speed() {
        let envelope = JetEnvelope { corner: 87.5, stall: 35.0 };
        let corner = jet_authority(&envelope, 87.5);
        assert!((corner - Vec3::ONE).length() < 1e-4, "{corner}");
        for slower_or_faster in [20.0, 50.0, 70.0, 110.0, 150.0] {
            assert!(jet_authority(&envelope, slower_or_faster).x < corner.x);
        }
        // No airflow, no control.
        assert_eq!(jet_authority(&envelope, 0.0), Vec3::ZERO);
    }

    #[test]
    fn jet_mouse_flies_what_the_mouse_moved() {
        // 100 counts up spread over 0.2 s, at rates the jet can follow: all of it is flown
        // (0.35 rad), and the stick is back in the middle soon after the mouse stops.
        let rates = JET_RATES;
        let dt = 1.0 / 60.0;
        let mut mouse = JetMouse::default();
        let mut turned = 0.0;
        let mut centred_after = None;
        for tick in 0..60 {
            let counts = if tick < 12 { 100.0 / 12.0 } else { 0.0 };
            let stick = mouse.update(Vec3::new(counts, 0.0, 0.0), rates, dt, |p| p);
            turned += stick.x * rates.x * dt;
            if tick >= 12 && stick.x == 0.0 && centred_after.is_none() {
                centred_after = Some((tick - 12) as f32 * dt);
            }
        }
        let asked = 100.0 * JET_MOUSE_GAIN.x;
        assert!((turned - asked).abs() < asked * 0.03, "{turned} of {asked}");
        assert!(centred_after.is_some_and(|t| t < 0.5), "{centred_after:?}");
        // Much faster than the jet turns, only the backlog is kept.
        let mut mouse = JetMouse::default();
        mouse.update(Vec3::new(10_000.0, 0.0, 0.0), rates, dt, |p| p);
        assert!(mouse.owed.x <= rates.x * JET_MOUSE_BACKLOG + 1e-4);
        // Held back by the angle of attack limit, it waits to be flown.
        let mut mouse = JetMouse::default();
        mouse.update(Vec3::new(50.0, 0.0, 0.0), rates, dt, |_| 0.0);
        let owed = mouse.owed.x;
        mouse.update(Vec3::ZERO, rates, dt, |_| 0.0);
        assert_eq!(mouse.owed.x, owed);
    }
}

/// Helicopter handling measured offline: the imported helicopters flown by
/// [`flight_forces`] and [`crate::vehicle::integrate`] (the driver's prediction) at 60 Hz, from
/// a hover and from forward flight, printing their response times and rates. Needs the
/// imported vehicles (`imported/vehicles`, or `GAME_IMPORTED_DIR`); skipped without them.
/// `cargo test -p game_shared --lib heli_handling -- --nocapture` shows the table.
#[cfg(test)]
mod heli_handling {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        config::GamePaths,
        vehicle::{VehicleModel, integrate},
    };

    const DT: f32 = 1.0 / 60.0;

    fn model(name: &str) -> Option<VehicleModel> {
        let imported = std::env::var_os(GamePaths::IMPORTED_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../imported"));
        let paths = GamePaths { imported, mods: Vec::new() };
        let desc: game_data::VehicleDesc = paths.read_ron(format!("vehicles/{name}.ron")).ok()?;
        Some(VehicleModel::new(desc, &paths))
    }

    /// One run: per tick, the time and the body after it.
    fn fly(model: &VehicleModel, mut body: BodyState, seconds: f32, controls: impl Fn(f32) -> Controls) -> Vec<(f32, BodyState)> {
        let mut state = FlightState {
            spin: 1.0,
            ..FlightState::new()
        };
        let joints = vec![[0.0; 3]; model.joint_count];
        let around = Surroundings { water: None, altitude: 150.0 };
        let mut samples = Vec::new();
        let ticks = (seconds / DT).round() as usize;
        for tick in 0..ticks {
            let t = tick as f32 * DT;
            let push = flight_forces(model, &body, &joints, &controls(t), &mut state, &around, DT);
            integrate(model, &mut body, &push, DT);
            samples.push((t + DT, body));
        }
        samples
    }

    fn held(throttle: f32, steer: f32, roll: f32, pitch: f32, until: f32) -> impl Fn(f32) -> Controls {
        move |t| {
            let on = t < until;
            Controls {
                throttle: if on { throttle } else { 0.0 },
                steer: if on { steer } else { 0.0 },
                roll: if on { roll } else { 0.0 },
                pitch: if on { pitch } else { 0.0 },
                occupied: true,
                ..default()
            }
        }
    }

    /// (heading, pitch, roll) in degrees: heading left positive, nose up, right wing down.
    fn attitude(rotation: Quat) -> Vec3 {
        let forward = rotation * Vec3::NEG_Z;
        let right = rotation * Vec3::X;
        Vec3::new(
            (-forward.x).atan2(-forward.z).to_degrees(),
            forward.y.clamp(-1.0, 1.0).asin().to_degrees(),
            (-right.y).clamp(-1.0, 1.0).asin().to_degrees(),
        )
    }

    /// When a rate first reaches a share of `full` (ms).
    fn reach(samples: &[(f32, f32)], full: f32, share: f32) -> f32 {
        samples
            .iter()
            .find(|(_, r)| *r * full.signum() >= full.abs() * share)
            .map_or(f32::NAN, |(t, _)| t * 1000.0)
    }

    fn level(speed: f32) -> BodyState {
        BodyState {
            position: Vec3::new(0.0, 150.0, 0.0),
            velocity: Vec3::NEG_Z * speed,
            ..default()
        }
    }

    /// Step responses from a hover: (rate curve in deg/s, settled rate, 50 % and 90 % times).
    fn step(model: &VehicleModel, axis: usize, hold: f32) -> (Vec<(f32, f32)>, f32) {
        let controls = match axis {
            0 => held(0.0, 1.0, 0.0, 0.0, hold),
            1 => held(0.0, 0.0, 0.0, -1.0, hold),
            _ => held(0.0, 0.0, 1.0, 0.0, hold),
        };
        let samples = fly(model, level(0.0), hold * 2.0, controls);
        let rates: Vec<(f32, f32)> = samples
            .iter()
            .map(|(t, b)| {
                let local = b.rotation.inverse() * b.angular_velocity;
                let rate = match axis {
                    0 => -b.angular_velocity.y,
                    1 => local.x,
                    _ => -local.z,
                };
                (*t, rate.to_degrees())
            })
            .collect();
        // Settled: the peak magnitude during the hold (tilt limits stop pitch and roll later).
        let full = rates.iter().filter(|(t, _)| *t <= hold).map(|(_, r)| *r).fold(0.0, |a: f32, r| if r.abs() > a.abs() { r } else { a });
        (rates, full)
    }

    #[test]
    fn heli_handling() {
        let names = ["ahe_z10", "ahe_ah1z", "ahe_havoc", "usthe_uh60", "the_mi17"];
        let models: Vec<(&str, VehicleModel)> = names.iter().filter_map(|n| Some((*n, model(n)?))).collect();
        if models.is_empty() {
            eprintln!("heli_handling: no imported helicopters, skipped");
            return;
        }
        println!(
            "{:<11} {:>22} {:>22} {:>22} {:>9} {:>9} {:>30}",
            "heli", "yaw 50%/90% ms, deg/s", "pitch 50/90, deg/s", "roll 50/90, deg/s", "nose dn", "yaw stop", "pedal@50m/s nose/course/bank"
        );
        let mut failures = Vec::new();
        for (name, model) in &models {
            let mut cells = Vec::new();
            let mut yaw_stop = f32::NAN;
            let mut yaw_90 = f32::NAN;
            for axis in 0..3 {
                let hold = 1.0;
                let (rates, full) = step(model, axis, hold);
                let during: Vec<(f32, f32)> = rates.iter().copied().filter(|(t, _)| *t <= hold).collect();
                cells.push(format!("{:4.0}/{:4.0} {:5.1}", reach(&during, full, 0.5), reach(&during, full, 0.9), full));
                if axis == 0 {
                    yaw_90 = reach(&during, full, 0.9);
                    yaw_stop = rates
                        .iter()
                        .find(|(t, r)| *t > hold && r.abs() < full.abs() * 0.1)
                        .map_or(f32::NAN, |(t, _)| (t - hold) * 1000.0);
                }
            }
            // Full nose down held from a hover: how far down it gets.
            let dive = fly(model, level(0.0), 3.0, held(0.0, 0.0, 0.0, -1.0, 3.0));
            let nose_down = dive.iter().map(|(_, b)| attitude(b.rotation).y).fold(0.0, f32::min);
            // Right pedal for 2 s at 50 m/s: the nose's and the flight path's turn, the bank.
            let pedal = fly(model, level(50.0), 2.0, held(0.0, 1.0, 0.0, 0.0, 2.0));
            let (_, end) = pedal.last().copied().unwrap();
            let a = attitude(end.rotation);
            let course = (-end.velocity.x).atan2(-end.velocity.z).to_degrees();
            println!(
                "{name:<11} {:>22} {:>22} {:>22} {:>9.0} {:>6.0} ms {:>9.0}/{:.0}/{:.0} {:.0} km/h",
                cells[0],
                cells[1],
                cells[2],
                nose_down,
                yaw_stop,
                -a.x,
                -course,
                a.z,
                end.velocity.length() * 3.6
            );
            // Held at a nose-down attitude for 15 s from a hover: the speed and height it ends
            // with (steeper than the old 30° limit mustn't make it much faster, it dives).
            let mut attitudes = Vec::new();
            for target in [-30.0f32, -45.0, -60.0] {
                let mut body = level(0.0);
                let mut state = FlightState { spin: 1.0, ..FlightState::new() };
                let joints = vec![[0.0; 3]; model.joint_count];
                let around = Surroundings { water: None, altitude: 150.0 };
                for _ in 0..(15.0 / DT) as usize {
                    let local = body.rotation.inverse() * body.angular_velocity;
                    let pitch = ((target - attitude(body.rotation).y) * 0.08 - local.x * 0.05).clamp(-1.0, 1.0);
                    let push = flight_forces(model, &body, &joints, &held(0.0, 0.0, 0.0, pitch, 99.0)(0.0), &mut state, &around, DT);
                    integrate(model, &mut body, &push, DT);
                }
                attitudes.push(format!("{target:.0}: {:.0} km/h {:+.0} m", body.velocity.length() * 3.6, body.position.y - 150.0));
            }
            println!("{:<11} nose held down 15 s: {}", "", attitudes.join(", "));
            // BF3-like: the pedals reach 90 % of their rate within 250 ms and stop as
            // quickly; the stick can point the nose far down; a pedal turn at speed carries
            // the flight path round within 25° of the nose.
            if !(yaw_90 < 250.0 && yaw_stop < 250.0) {
                failures.push(format!("{name}: pedals slow ({yaw_90:.0} ms to 90 %, {yaw_stop:.0} ms to stop)"));
            }
            if nose_down > -60.0 {
                failures.push(format!("{name}: nose only {nose_down:.0}° down"));
            }
            if (a.x - course).abs() > 25.0 {
                failures.push(format!("{name}: pedal turn skids (nose {:.0}°, path {:.0}°)", -a.x, -course));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
