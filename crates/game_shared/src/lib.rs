//! Code shared by the client and the dedicated server.
//!
//! Everything that must be identical on both sides lives here: the network protocol
//! (component/message registration order matters!), the fixed-tick simulation used for
//! server authority and client prediction, and level loading.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;

pub mod arsenal;
pub mod cache;
pub mod chat;
pub mod commander;
pub mod config;
pub mod conquest;
pub mod content;
pub mod discovery;
pub mod effects;
pub mod flight;
pub mod gear;
pub mod hitzones;
pub mod input;
pub mod join;
pub mod ladder;
pub mod level;
pub mod modes;
pub mod mods;
pub mod physics;
pub mod projectile;
pub mod protocol;
pub mod radio;
pub mod revive;
pub mod rope;
pub mod skeleton;
pub mod soldier;
pub mod squad;
pub mod statics;
pub mod summary;
pub mod validate;
pub mod vehicle;
pub mod voice;
pub mod weapons;

/// Simulation rate for gameplay and physics.
pub const TICK_HZ: f64 = 60.0;

/// BF2's classic game port, as a nod to the original.
pub const DEFAULT_PORT: u16 = 16567;

/// Bump whenever the wire protocol changes in a way the protocol hash can't detect.
pub const PROTOCOL_ID: u64 = 0x4246_325f_0006; // "BF2_" + version (6: BF2's world gravity for everything, soldiers standing in crevices)

/// Adds everything both client and server need. The messaging backend (renet) is added by
/// the binaries so a future Steam backend can be swapped in.
pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Time::<Fixed>::from_hz(TICK_HZ))
            // Vehicles fall with it times their `GravityScale` (BF2's `gravityModifier`).
            .insert_resource(Gravity(Vec3::NEG_Y * physics::WORLD_GRAVITY))
            .add_systems(Startup, config::log_mods)
            .add_plugins((
                // The server authorizes clients itself, after the join handshake (`join`).
                RepliconPlugins.set(RepliconSharedPlugin {
                    auth_method: AuthMethod::Custom,
                }),
                PhysicsPlugins::default(),
                protocol::ProtocolPlugin,
                soldier::SoldierPlugin,
                level::LevelPlugin,
                weapons::WeaponsPlugin,
                skeleton::SkeletonPlugin,
                arsenal::ArsenalPlugin,
                vehicle::VehiclePlugin,
                commander::CommanderPlugin,
            ));
    }
}
