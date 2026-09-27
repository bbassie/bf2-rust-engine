//! Vehicles: `vehicles/<name>.ron`.
//!
//! A vehicle is a tree of parts (hull, turret, barrel, wheels, ...) placed relative to their
//! parents. Some parts are joints turned by a seat's input (turrets, barrels, steering,
//! control surfaces), some are wheels. Seats say where their occupant sits, looks from and
//! gets out. Aircraft and boats add wings (lifting surfaces), thrusters, a rotor or floaters.
//! The physics values are in our own terms (newtons, meters per second); the importer derives
//! them from the BF2 templates and they can be tuned by hand.

use serde::{Deserialize, Serialize};

use crate::{Placement, VehicleSounds, WeaponDesc};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VehicleDesc {
    /// Template name, lowercase.
    pub name: String,
    /// Human readable name, e.g. `HMMWV`.
    #[serde(default)]
    pub display_name: String,
    /// What it is, for the controls, camera and HUD.
    #[serde(default)]
    pub category: VehicleCategory,
    pub drive: DriveKind,
    pub physics: VehiclePhysics,
    pub engine: EngineDesc,
    /// Lifting surfaces: wings, control surfaces, fins, rudders (in air and water alike).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wings: Vec<WingDesc>,
    /// Jet engines and ship propellers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thrusters: Vec<ThrusterDesc>,
    /// Helicopters: main and tail rotor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotor: Option<RotorDesc>,
    /// Boats and amphibious vehicles: where the hull floats.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub floaters: Vec<FloaterDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing_gear: Option<LandingGearDesc>,
    /// Jets: extra thrust while sprint is held (BF2 `sprintFactor` and its meter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub afterburner: Option<AfterburnerDesc>,
    /// How aircraft and boats move through air and water.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aero: Option<AeroDesc>,
    #[serde(default = "default_hit_points")]
    pub hit_points: f32,
    /// Material of the hull for direct hits, and for blast damage (ids of the material
    /// damage table, e.g. 26 metal plating, 27 light armour, 29 tank sides; 71 soft, 72 hard,
    /// 110 light vehicle).
    #[serde(default)]
    pub armor_material: u32,
    #[serde(default)]
    pub blast_material: u32,
    /// What's left after destruction: one mesh per piece, piece `n` in place of the part
    /// drawn with mesh index `n` of the hull's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wreck_mesh: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub wreck_pieces: u32,
    /// Parent parts come before their children; part 0 is the hull.
    pub parts: Vec<VehiclePart>,
    #[serde(default)]
    pub wheels: Vec<WheelDesc>,
    /// Seat 0 is the driver.
    #[serde(default)]
    pub seats: Vec<SeatDesc>,
    #[serde(default)]
    pub entry_points: Vec<EntryPointDesc>,
    #[serde(default)]
    pub weapons: Vec<VehicleWeaponDesc>,
    #[serde(default)]
    pub sounds: VehicleSounds,
}

impl VehicleDesc {
    /// Whether a part moves rigidly with the hull's collision: the hull and anything
    /// attached to it without joints in between, plus yaw-only joints (turrets). Wheels are
    /// left to the suspension; rotor blades and landing gear don't collide.
    pub fn is_hull_part(&self, index: usize) -> bool {
        let mut i = index;
        loop {
            let part = &self.parts[i];
            if let Some(joint) = &part.joint {
                let [yaw, pitch, roll] = &joint.axes;
                let turret = matches!(yaw.input, Some(JointInput::AimYaw | JointInput::Steer))
                    && pitch.input.is_none()
                    && roll.input.is_none();
                if !turret || i != index {
                    return false;
                }
            }
            if self.wheels.iter().any(|w| w.part as usize == i) {
                return false;
            }
            match part.parent {
                Some(parent) => i = parent as usize,
                None => return true,
            }
        }
    }

    /// The seat whose part is the closest ancestor of (or is) `part`.
    pub fn seat_of(&self, part: usize) -> u32 {
        let mut i = part;
        loop {
            if let Some(seat) = self.seats.iter().position(|s| s.part as usize == i) {
                return seat as u32;
            }
            match self.parts[i].parent {
                Some(parent) => i = parent as usize,
                None => return 0,
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DriveKind {
    /// Steered wheels with tyre grip.
    #[default]
    Wheeled,
    /// Skid-steered tracks.
    Tracked,
    /// Wheels (or skids) that only roll, steer and brake: aircraft push with engines or rotors.
    Rolling,
    /// Nothing drives it over ground (boats, stationary weapons).
    None,
}

/// BF2's vehicle categories (`vehicleCategory`), plus stationary weapons.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VehicleCategory {
    #[default]
    Land,
    /// Jets.
    Air,
    Helicopter,
    Sea,
    /// Guns and launchers fixed to the ground (TOW, machine gun nests, AA launchers).
    Stationary,
}

impl VehicleCategory {
    /// Flown with the stick (mouse or arrow keys) by the pilot.
    pub fn flies(self) -> bool {
        matches!(self, Self::Air | Self::Helicopter)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VehiclePhysics {
    /// Kilograms.
    pub mass: f32,
    /// Multiplier on gravity.
    #[serde(default = "one")]
    pub gravity: f32,
    /// Air and rolling drag coefficient.
    #[serde(default)]
    pub drag: f32,
    /// Drag multiplier along the hull's x (right), y (up) and z (back) axes.
    #[serde(default = "ones", skip_serializing_if = "is_ones")]
    pub drag_modifier: [f32; 3],
    /// Multiplier on the inertia of a solid box of `bounds` (BF2 `inertiaModifier`).
    #[serde(default = "ones", skip_serializing_if = "is_ones")]
    pub inertia_modifier: [f32; 3],
    /// Relative to the hull origin.
    #[serde(default)]
    pub center_of_mass: [f32; 3],
    /// Collision box of the hull (min, max corners, hull space), for inertia and placement.
    pub bounds: [[f32; 3]; 2],
}

impl Default for VehiclePhysics {
    fn default() -> Self {
        Self {
            mass: 1000.0,
            gravity: 1.0,
            drag: 0.0,
            drag_modifier: [1.0; 3],
            inertia_modifier: [1.0; 3],
            center_of_mass: [0.0; 3],
            bounds: [[-1.0, -0.5, -2.0], [1.0, 1.0, 2.0]],
        }
    }
}

/// A lifting surface (BF2 `Wing`). It pushes along its normal (the part's up axis) against
/// the flow through it, and control surfaces add lift in proportion to their deflection
/// (their part's joint, turned by the stick, rudder or throttle input).
///
/// Forces are accelerations times the vehicle's mass, like BF2's (a heavier jet with the
/// same wings flies the same): with airspeed `u` along the hull and `w` the flow speed along
/// the normal, the push is `mass * (-lift * w * u + flap_lift * deflection * u²)`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WingDesc {
    /// The wing's part; its joint (if any) is the control surface deflection.
    pub part: u32,
    /// Where the force acts, hull space (BF2 part position plus `setPositionOffset`).
    pub position: [f32; 3],
    /// Lift per angle of attack, 1/m (from `setWingLift`).
    #[serde(default)]
    pub lift: f32,
    /// Lift at full deflection, 1/m (from `setFlapLift`).
    #[serde(default)]
    pub flap_lift: f32,
    /// Lowered automatically at low speed (landing flaps, `setLiftRegulated`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub landing_flap: bool,
}

/// A jet engine or ship propeller: pushes along its forward axis with the throttle.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThrusterDesc {
    /// Hull space.
    pub position: [f32; 3],
    /// Direction of the push, hull space.
    pub direction: [f32; 3],
    /// Acceleration at full throttle and standstill, m/s² (from BF2 `setTorque` ×
    /// `setDifferential`; like lift, thrust doesn't depend on mass in BF2).
    pub acceleration: f32,
    /// The push fades out towards this forward speed (`noPropellerEffectAtSpeed`), m/s.
    pub max_speed: f32,
    /// Share of the push available in reverse.
    #[serde(default)]
    pub reverse: f32,
    /// Only pushes while under water (ship propellers).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub water: bool,
}

/// A helicopter's rotors (BF2 `c_ETHelicopter` engines under tilting rotor heads). The
/// collective holds the altitude unless the pilot climbs or sinks (BF2
/// `regulateVerticalPos`), the stick tilts the helicopter, the rudder turns the tail.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RotorDesc {
    /// Rotor hub, hull space.
    pub position: [f32; 3],
    /// Seconds from standstill to full rotor speed.
    #[serde(default = "default_spin_up")]
    pub spin_up: f32,
    /// Vertical speed at full collective up and down, m/s.
    pub climb_speed: [f32; 2],
    /// Most extra push beyond hovering, as a share of gravity.
    pub lift_margin: f32,
    /// Horizontal share of the rotor's push is magnified by this (`horizontalSpeedMagnifier`).
    #[serde(default = "one")]
    pub horizontal_magnifier: f32,
    /// Damping of horizontal speed, 1/s (from `dampHorizontalVel`).
    #[serde(default)]
    pub horizontal_damping: f32,
    /// Collective regulation holds the altitude up to this tilt (degrees) and fades out by
    /// `no_regulation_angle` (`maxVertRegAngle`, `noVertRegAngle`).
    #[serde(default = "default_regulation_angle")]
    pub regulation_angle: f32,
    #[serde(default = "default_no_regulation_angle")]
    pub no_regulation_angle: f32,
    /// Turn rates at full stick and rudder (pitch, yaw, roll), radians per second.
    pub turn_rates: [f32; 3],
    /// How quickly the turn rates are reached, 1/s.
    #[serde(default = "default_response")]
    pub response: f32,
    /// How strongly it levels out without stick input, 1/s.
    #[serde(default)]
    pub leveling: f32,
    /// Tail rotor, hull space.
    pub tail_position: [f32; 3],
}

/// A buoyant point of the hull (BF2 `FloatingBundle`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FloaterDesc {
    /// Hull space.
    pub position: [f32; 3],
    /// Lift when fully submerged, as a share of the vehicle's weight (from
    /// `setFloatMaxLift`).
    pub lift: f32,
    /// How deep it goes before it is fully submerged, meters (`setHullHeight`).
    pub depth: f32,
}

/// Flight and swimming tuning of aircraft and boats; accelerations, like the wings'.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AeroDesc {
    /// Air drag: deceleration per squared speed along each hull axis, times
    /// `physics.drag_modifier` (from BF2 `drag`).
    pub drag: f32,
    /// Wings gain no more lift beyond this angle of attack, degrees.
    pub stall_angle: f32,
    /// Most acceleration the wings give, m/s².
    pub max_load: f32,
    /// Damping of rotation about the pitch, yaw and roll axes: constant (1/s), and per m/s of
    /// airspeed.
    pub angular_damping: [f32; 3],
    pub speed_damping: [f32; 3],
    /// Water drag of a submerged hull across, up and along it, 1/s.
    #[serde(default)]
    pub water_drag: [f32; 3],
}

/// When the landing gear goes up and down (BF2 `LandingGear`): up above `up_height` meters
/// and faster than `up_speed` m/s, down below `down_height` and slower than `down_speed`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LandingGearDesc {
    pub up_height: f32,
    pub up_speed: f32,
    pub down_height: f32,
    pub down_speed: f32,
}

/// BF2's sprint on vehicles: a meter that empties in `duration` seconds of use, refills in
/// `recover` seconds and can be used again above `min_charge`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AfterburnerDesc {
    /// Thrust multiplier.
    pub factor: f32,
    pub duration: f32,
    pub recover: f32,
    #[serde(default)]
    pub min_charge: f32,
}

/// How hard the vehicle pushes and stops.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EngineDesc {
    /// Top speed forwards on flat ground, m/s.
    pub top_speed: f32,
    /// Top speed in reverse, m/s.
    pub reverse_speed: f32,
    /// Driving force at standstill, newtons. Falls off towards the top speed.
    pub drive_force: f32,
    /// Braking force, newtons.
    pub brake_force: f32,
    /// Tracked vehicles: turn rate at full steering, radians per second.
    #[serde(default)]
    pub turn_rate: f32,
    /// Friction coefficients of the tyres or tracks: along and across the rolling direction.
    pub grip: [f32; 2],
}

impl Default for EngineDesc {
    fn default() -> Self {
        Self {
            top_speed: 20.0,
            reverse_speed: 6.0,
            drive_force: 5000.0,
            brake_force: 10000.0,
            turn_rate: 0.8,
            grip: [1.0, 1.0],
        }
    }
}

/// One node of the vehicle's part tree.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VehiclePart {
    /// Original template name (seats and weapons refer to parts by index).
    pub name: String,
    /// Index of the parent part; `None` only for the hull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// Relative to the parent, at rest.
    #[serde(flatten)]
    pub placement: Placement,
    /// Visible mesh: `.glb` path relative to the imported root, and the mesh inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub mesh_index: u32,
    /// Collision `.glb` (meshes named `part{N}_{projectile|vehicle|soldier|ai}`) and part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collision: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub collision_part: u32,
    /// Turned by input (turrets, barrels, steering knuckles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joint: Option<JointDesc>,
}

/// A part that rotates relative to its parent: `yaw` about +Y (positive turns left), then
/// `pitch` about +X (positive raises the front), then `roll` about +Z.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct JointDesc {
    /// Yaw, pitch, roll.
    pub axes: [JointAxis; 3],
    /// The seat whose occupant turns it.
    pub seat: u32,
}

/// One axis of a joint; axes without an input stay at 0.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct JointAxis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<JointInput>,
    /// Degrees. Equal limits mean the axis turns freely.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub min: f32,
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub max: f32,
    /// Degrees per second. The sign says which way positive input turns it.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub speed: f32,
    /// Degrees per second squared; steering uses it as its turning rate.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub acceleration: f32,
    /// Returns to 0 without input.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic_reset: bool,
}

impl JointAxis {
    pub fn limited(&self) -> bool {
        self.min != self.max
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointInput {
    /// Follows the occupant's aim horizontally.
    AimYaw,
    /// Follows the occupant's aim vertically.
    AimPitch,
    /// Steering and rudder (right = positive).
    Steer,
    /// Throttle (forward = positive).
    Throttle,
    /// Flight stick forward/back (back = positive = nose up).
    Pitch,
    /// Flight stick left/right (right = positive).
    Roll,
    /// Landing gear: turns to its limit while the gear is up.
    Gear,
    /// Spins with the rotor or engine (rotor blades), at `speed` degrees per second at full
    /// power.
    Spin,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WheelDesc {
    /// The wheel's part (it spins, follows the suspension and any steering joint above it).
    pub part: u32,
    /// Wheel center at rest in hull space.
    pub position: [f32; 3],
    pub radius: f32,
    /// Carries the vehicle. Others are only drawn (tank road wheels between the contact ones).
    pub contact: bool,
    /// Suspension stiffness and damping as tuned in BF2 (`setStrength`, `setDamping`).
    pub strength: f32,
    pub damping: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SeatDesc {
    pub name: String,
    /// Part the seat belongs to. Joints below it are turned from this seat.
    pub part: u32,
    /// Where the occupant sits; `None` for seats inside a closed hull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soldier: Option<Attachment>,
    /// The occupant's view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<SeatCamera>,
    /// Where the occupant gets out, relative to the seat part.
    pub exit: [f32; 3],
    /// The occupant is exposed (can be shot, is drawn).
    #[serde(default)]
    pub open: bool,
}

/// A point on a part.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Attachment {
    pub part: u32,
    #[serde(flatten)]
    pub placement: Placement,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SeatCamera {
    /// First-person eye point.
    pub attachment: Attachment,
    /// Vertical field of view, radians.
    pub fov: f32,
    /// Third-person chase camera: distance behind and offset (x right, y up, z back).
    pub chase_distance: f32,
    pub chase_offset: [f32; 3],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntryPointDesc {
    /// Hull space.
    pub position: [f32; 3],
    pub radius: f32,
}

/// A gun mounted on the vehicle.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VehicleWeaponDesc {
    /// The weapon's part; projectiles leave along its forward axis.
    pub part: u32,
    /// Muzzle position relative to the part.
    pub muzzle: [f32; 3],
    /// The seat that fires it.
    pub seat: u32,
    /// Fired with the secondary button instead of the primary one.
    #[serde(default)]
    pub alt_fire: bool,
    pub weapon: WeaponDesc,
}

fn default_hit_points() -> f32 {
    1000.0
}

fn default_regulation_angle() -> f32 {
    35.0
}

fn default_spin_up() -> f32 {
    6.0
}

fn default_response() -> f32 {
    4.0
}

fn default_no_regulation_angle() -> f32 {
    55.0
}

fn one() -> f32 {
    1.0
}

fn ones() -> [f32; 3] {
    [1.0; 3]
}

fn is_ones(v: &[f32; 3]) -> bool {
    *v == [1.0; 3]
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}
