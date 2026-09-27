//! Code shared by the client and the dedicated server.
//!
//! Everything that must be identical on both sides lives here: the network protocol
//! (component/message registration order matters!), the fixed-tick simulation used for
//! server authority and client prediction, and level loading.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;

pub mod config;
pub mod conquest;
pub mod input;
pub mod level;
pub mod physics;
pub mod protocol;
pub mod soldier;
pub mod statics;
pub mod vehicle;
pub mod weapons;

/// Simulation rate for gameplay and physics.
pub const TICK_HZ: f64 = 60.0;

/// BF2's classic game port, as a nod to the original.
pub const DEFAULT_PORT: u16 = 16567;

/// Bump whenever the wire protocol changes in a way the protocol hash can't detect.
pub const PROTOCOL_ID: u64 = 0x4246_325f_0001; // "BF2_" + version

/// Adds everything both client and server need. The messaging backend (renet) is added by
/// the binaries so a future Steam backend can be swapped in.
pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Time::<Fixed>::from_hz(TICK_HZ))
            .add_plugins((
                RepliconPlugins,
                PhysicsPlugins::default(),
                protocol::ProtocolPlugin,
                soldier::SoldierPlugin,
                level::LevelPlugin,
                weapons::WeaponsPlugin,
                vehicle::VehiclePlugin,
            ));
    }
}
