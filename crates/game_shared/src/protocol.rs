//! Network protocol: which components replicate and which messages exist.
//!
//! Registration order must be identical on client and server; keeping it all in this one
//! plugin guarantees that. Replicon hashes the protocol and refuses mismatched clients.

use bevy::{ecs::entity::MapEntities, prelude::*};
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    conquest::{ControlPoint, DeployRequest, Deployment, FlagEvent, FlagState, RoundState, Tickets},
    input::InputPacket,
    soldier::{Health, InputAck, Soldier, SoldierMotion},
    vehicle::{Seated, Vehicle, VehicleHealth, VehicleMotion, VehicleShot, VehicleState, VehicleWeapons},
    weapons::{Inventory, Loadout},
};

pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.replicate::<MatchInfo>()
            .replicate::<Player>()
            .replicate::<PlayerNetId>()
            .replicate::<Team>()
            .replicate::<Score>()
            .replicate::<Soldier>()
            .replicate::<ControlledBy>()
            .replicate::<SoldierMotion>()
            .replicate::<InputAck>()
            .replicate::<Health>()
            .replicate::<Loadout>()
            .replicate::<Inventory>()
            .replicate::<ControlPoint>()
            .replicate::<FlagState>()
            .replicate::<Tickets>()
            .replicate::<RoundState>()
            .replicate::<Deployment>()
            .replicate::<crate::squad::SquadMember>()
            .replicate::<crate::statics::DestroyedStatics>()
            .replicate::<crate::projectile::Projectile>()
            .replicate::<crate::projectile::ProjectileMotion>()
            .replicate::<crate::projectile::SmokeCloud>()
            .replicate::<crate::hitzones::ServerClock>()
            .replicate::<crate::rope::Rope>()
            .replicate::<Vehicle>()
            .replicate::<VehicleMotion>()
            .replicate::<VehicleState>()
            .replicate::<Seated>()
            .replicate::<VehicleHealth>()
            .replicate::<VehicleWeapons>()
            .replicate::<crate::revive::Downed>()
            .replicate::<crate::radio::Spotted>()
            .replicate::<crate::commander::Commander>()
            .replicate::<crate::commander::SquadOrder>()
            .replicate::<crate::commander::TeamAssets>()
            .replicate::<crate::commander::AssetEffect>()
            .replicate::<crate::commander::AssetVehicle>()
            .replicate::<crate::gear::SoldierGear>()
            .replicate::<crate::gear::TearGas>()
            .add_client_message::<crate::gear::GearRequest>(Channel::Ordered)
            .add_client_message::<InputPacket>(Channel::Unreliable)
            .add_client_message::<ClientHello>(Channel::Ordered)
            .add_client_message::<DeployRequest>(Channel::Ordered)
            .add_client_message::<crate::squad::SquadRequest>(Channel::Ordered)
            .add_mapped_server_message::<FlagEvent>(Channel::Ordered)
            .add_mapped_server_message::<ShotFired>(Channel::Unreliable)
            .add_mapped_server_message::<HitConfirmed>(Channel::Unordered)
            .add_mapped_server_message::<KillFeed>(Channel::Ordered)
            .add_mapped_server_message::<VehicleShot>(Channel::Unreliable)
            .add_server_message::<crate::effects::PlayEffect>(Channel::Unordered)
            .add_client_message::<crate::chat::ChatRequest>(Channel::Ordered)
            .add_server_message::<crate::chat::ChatLine>(Channel::Ordered)
            .add_server_message::<crate::chat::Kicked>(Channel::Ordered)
            // No entities in them: sent right away rather than with the next replication
            // tick, so a kick's reason arrives before the disconnect.
            .make_message_independent::<crate::chat::ChatLine>()
            .make_message_independent::<crate::chat::Kicked>()
            .add_server_message::<crate::summary::RoundSummary>(Channel::Ordered)
            .add_client_message::<crate::revive::GiveUp>(Channel::Ordered)
            .add_client_message::<crate::radio::RadioRequest>(Channel::Ordered)
            .add_client_message::<crate::commander::CommanderRequest>(Channel::Ordered)
            .add_server_message::<crate::commander::ScanReport>(Channel::Unordered)
            .add_mapped_server_message::<crate::radio::RadioMessage>(Channel::Ordered)
            .add_mapped_server_message::<crate::revive::ReplenishNotice>(Channel::Unordered);
    }
}

/// The match being played. Exactly one replicated entity carries this.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MatchInfo {
    /// Level folder name, e.g. `strike_at_karkand` or the built-in `test_range`.
    pub level: String,
    /// Game mode id, e.g. `gpm_cq`.
    pub mode: String,
    /// Layout size (16/32/64).
    pub size: u32,
}

/// A participant in the match, human or bot. Outlives the soldiers it controls.
#[derive(Component, Serialize, Deserialize, Clone, Debug)]
#[require(Team, Score, Deployment)]
pub struct Player {
    pub name: String,
    pub is_bot: bool,
}

/// Network id of the human behind a [`Player`], so a client can recognise itself.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayerNetId(pub u64);

impl PlayerNetId {
    /// The player on a listen server / in singleplayer, who has no network connection.
    pub const LOCAL_HOST: Self = Self(0);
}

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Team {
    #[default]
    Spectator,
    One,
    Two,
}

impl Team {
    pub fn opponent(self) -> Self {
        match self {
            Team::One => Team::Two,
            Team::Two => Team::One,
            Team::Spectator => Team::Spectator,
        }
    }
}

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Score {
    pub score: i32,
    pub kills: u32,
    pub deaths: u32,
}

/// Links a soldier body to the [`Player`] controlling it.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ControlledBy(#[entities] pub Entity);

/// First message a client sends after connecting.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct ClientHello {
    pub name: String,
}

/// Server -> clients: a soldier fired. Clients draw the tracer and play the sound; the
/// shooter's own client already did so when it predicted the shot.
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct ShotFired {
    #[entities]
    pub soldier: Entity,
    pub origin: Vec3,
    pub direction: Vec3,
    /// Index into the soldier's loadout.
    pub weapon: u8,
}

/// Server -> the attacker: your shot hit someone (for the hit marker).
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct HitConfirmed {
    #[entities]
    pub victim: Entity,
    pub damage: f32,
    pub headshot: bool,
    pub killed: bool,
}

/// Server -> everyone: someone was killed.
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct KillFeed {
    /// The killer's player, if any.
    #[entities]
    pub killer: Option<Entity>,
    #[entities]
    pub victim: Entity,
    pub weapon: String,
    pub headshot: bool,
}
