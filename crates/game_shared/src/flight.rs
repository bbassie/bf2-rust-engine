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
//!   the stick's and rudder's rates, levelling it out when the stick is centred.
//! * **Floaters** lift in proportion to how deep they are in the water, and the submerged
//!   hull drags (a keel sideways, little forwards).

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
/// Helicopters regulate their altitude and level out only above this height (m); lower down
/// they settle onto the ground unless the pilot climbs.
const HOVER_HEIGHT: f32 = 1.5;

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

    // Wings. Land vehicles' rudders only work afloat.
    if desc.category != VehicleCategory::Land || in_water > 0.0 {
        let tan_stall = aero.stall_angle.to_radians().tan();
        let mut lift = Vec3::ZERO;
        let mut wings = Vec::with_capacity(desc.wings.len());
        for wing in &desc.wings {
            let normal = rotation * (model.rest_rotation(wing.part as usize) * Vec3::Y);
            let point = body.position + rotation * Vec3::from(wing.position);
            let flow = body.velocity_at(point, com);
            let along = flow.dot(forward).max(0.0);
            let through = flow.dot(normal);
            let lifting = through.clamp(-along * tan_stall, along * tan_stall);
            let stalled = through - lifting;
            let deflection = if wing.landing_flap {
                let [full, none] = FLAP_SPEEDS;
                if state.gear_up { 0.0 } else { 1.0 - ((along - full) / (none - full)).clamp(0.0, 1.0) }
            } else {
                model.deflection(joints, wing.part as usize)
            };
            let accel = -wing.lift * (lifting * along + PLATE * stalled * stalled.abs())
                + wing.flap_lift * deflection * along * along;
            // Landing flaps only add lift; where BF2 puts them would pitch the jet over.
            wings.push((normal * accel, if wing.landing_flap { com } else { point }));
            lift += normal * accel;
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

    // Aerodynamic damping of rotation.
    let omega = inverse * body.angular_velocity;
    let damping = Vec3::from(aero.angular_damping) + Vec3::from(aero.speed_damping) * airspeed;
    push.torque += rotation * (-omega * damping * model.inertia);

    // Thrusters.
    let jet = desc.category == VehicleCategory::Air;
    if jet {
        // On the ground the engines idle unless the pilot opens the throttle.
        let parked = around.altitude < 5.0 && speed < 20.0;
        let target = match controls.throttle {
            _ if !controls.occupied => 0.0,
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

    // Rotor.
    if let Some(rotor) = &desc.rotor {
        let (target, rate) = if controls.occupied { (1.0, 1.0) } else { (0.0, 0.5) };
        let rate = rate / rotor.spin_up.max(0.1);
        state.spin += (target - state.spin).clamp(-rate * dt, rate * dt);
        let power = state.spin * state.spin;
        let up = rotation * Vec3::Y;
        let airborne = around.altitude > HOVER_HEIGHT;
        let tilt = up.angle_between(Vec3::Y).to_degrees();
        let span = (rotor.no_regulation_angle - rotor.regulation_angle).max(1.0);
        let regulation = if airborne { 1.0 - ((tilt - rotor.regulation_angle) / span).clamp(0.0, 1.0) } else { 0.0 };
        let climb = if controls.throttle >= 0.0 {
            controls.throttle * rotor.climb_speed[0]
        } else {
            controls.throttle * rotor.climb_speed[1]
        };
        // Held altitude: whatever push keeps the vertical speed at what the collective asks,
        // making up for what the wings and drag do.
        let others: f32 = push.forces.iter().map(|(f, _)| f.y).sum::<f32>() / mass;
        let regulated = (g - others + (climb - body.velocity.y) * COLLECTIVE_RESPONSE) / up.y.max(0.35);
        let free = if airborne || controls.throttle > 0.0 {
            g * (1.0 + controls.throttle * rotor.lift_margin)
        } else {
            g * 0.9
        };
        let thrust = (free + (regulated - free) * regulation).clamp(0.0, g * (1.0 + rotor.lift_margin)) * power;
        // Tilting further than the regulation reaches doesn't push harder sideways.
        let horizontal = (up - Vec3::Y * up.y).clamp_length_max(rotor.regulation_angle.to_radians().sin());
        let direction = Vec3::Y * up.y + horizontal * rotor.horizontal_magnifier;
        let flat_velocity = body.velocity - Vec3::Y * body.velocity.y;
        push.forces.push(((direction * thrust - flat_velocity * rotor.horizontal_damping * power) * mass, com));

        // The stick and rudder turn it at their rates; centred, it levels out.
        let rates = Vec3::from(rotor.turn_rates);
        let mut wanted = Vec3::new(controls.pitch * rates.x, -controls.steer * rates.y, -controls.roll * rates.z);
        if airborne {
            let right = rotation * Vec3::X;
            if controls.pitch.abs() < 0.05 {
                wanted.x -= forward.y.clamp(-1.0, 1.0).asin() * rotor.leveling;
            }
            if controls.roll.abs() < 0.05 {
                wanted.z += (-right.y).clamp(-1.0, 1.0).asin() * rotor.leveling;
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
