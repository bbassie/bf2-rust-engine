//! Vehicles: `vehicles/<name>.ron`.
//!
//! A vehicle is a tree of parts (hull, turret, barrel, wheels, ...) placed relative to their
//! parents. Some parts are joints turned by a seat's input (turrets, barrels, steering), some
//! are wheels. Seats say where their occupant sits, looks from and gets out. The physics
//! values are in our own terms (newtons, meters per second); the importer derives them from
//! the BF2 templates and they can be tuned by hand.

use serde::{Deserialize, Serialize};

use crate::{Placement, VehicleSounds, WeaponDesc};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VehicleDesc {
    /// Template name, lowercase.
    pub name: String,
    /// Human readable name, e.g. `HMMWV`.
    #[serde(default)]
    pub display_name: String,
    pub drive: DriveKind,
    pub physics: VehiclePhysics,
    pub engine: EngineDesc,
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
    /// left to the suspension.
    pub fn is_hull_part(&self, index: usize) -> bool {
        let mut i = index;
        loop {
            let part = &self.parts[i];
            if let Some(joint) = &part.joint {
                let [yaw, pitch, roll] = &joint.axes;
                let turret = yaw.input.is_some() && pitch.input.is_none() && roll.input.is_none();
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
            center_of_mass: [0.0; 3],
            bounds: [[-1.0, -0.5, -2.0], [1.0, 1.0, 2.0]],
        }
    }
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
    /// Steering (right = positive).
    Steer,
    /// Throttle (forward = positive).
    Throttle,
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

fn one() -> f32 {
    1.0
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}
