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
            .replicate::<crate::soldier::MovementRules>()
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
            .add_mapped_server_message::<ThrowReleased>(Channel::Unreliable)
            .add_mapped_server_message::<HitConfirmed>(Channel::Unordered)
            .add_mapped_server_message::<KillFeed>(Channel::Ordered)
            .add_mapped_server_message::<VehicleShot>(Channel::Unreliable)
            .add_server_message::<crate::effects::PlayEffect>(Channel::Unordered)
            .add_server_message::<crate::hitzones::SoldierImpact>(Channel::Unreliable)
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
            .add_mapped_server_message::<crate::revive::ReplenishNotice>(Channel::Unordered)
            .add_server_message::<OutOfBoundsWarning>(Channel::Unreliable)
            // Game modes beyond conquest (`crate::modes`).
            .replicate::<crate::modes::ModeState>()
            .replicate::<crate::modes::ChargeTimes>()
            .replicate::<crate::modes::Charge>()
            .replicate::<crate::modes::ChargeState>()
            .replicate::<crate::modes::Sector>()
            .replicate::<crate::modes::Locked>()
            .replicate::<crate::modes::SpawnBlocked>()
            .add_mapped_server_message::<crate::modes::ObjectiveEvent>(Channel::Ordered)
            // The join handshake (`crate::join`): before a client is authorized, so the
            // server's answers are independent of replication.
            .replicate::<crate::join::AccountBadge>()
            .add_client_message::<crate::join::JoinRequest>(Channel::Ordered)
            .add_client_message::<crate::join::AccountTicket>(Channel::Ordered)
            .add_client_message::<crate::join::ContentReport>(Channel::Ordered)
            .add_server_message::<crate::join::JoinChallenge>(Channel::Ordered)
            .add_server_message::<crate::join::JoinVerdict>(Channel::Ordered)
            .make_message_independent::<crate::join::JoinChallenge>()
            .make_message_independent::<crate::join::JoinVerdict>()
            // Loadouts (`crate::arsenal`): the server's rules on the match, each player's
            // accepted picks, and the picks a client asks for.
            .replicate::<crate::arsenal::LoadoutRules>()
            .replicate::<crate::arsenal::LoadoutPicks>()
            .add_client_message::<crate::arsenal::LoadoutRequest>(Channel::Ordered)
            // Voice chat (`crate::voice`): each message gets a renet channel of its own, so
            // these are dedicated unreliable channels, registered after the game's so they
            // come last when a packet is filled. The relay holds no entities: out right away.
            .replicate::<crate::voice::VoiceMuted>()
            .add_client_message::<crate::voice::VoicePacket>(Channel::Unreliable)
            .add_server_message::<crate::voice::VoiceRelay>(Channel::Unreliable)
            .make_message_independent::<crate::voice::VoiceRelay>();
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

/// Server -> everyone but the thrower: a throw or a charge just left the wind-up (BF2's
/// `fire.pullBackTime` ended and the trigger let go, or the charge's trigger was pulled) and
/// is on its way out of the hand, `fire.fireLaunchDelay` seconds before it actually appears
/// as a projectile (`ShotFired`, sent once that delay runs out). Starts the third-person
/// throw animation at the right moment; the thrower's own client already started it locally
/// (see `game_client::combat::predict_local_shots`, which times off the same wind-up release
/// rather than the projectile's spawn).
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct ThrowReleased {
    #[entities]
    pub soldier: Entity,
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
///
/// The killer is carried by name, not entity: replicon drops a mapped message outright when
/// one of its entities can't be mapped for a given client (not yet replicated to it, or
/// already gone), which silently ate kill feed lines whenever that happened to be true of the
/// killer. A name always gets there; nothing here needs to look the killer up as an entity
/// (see `game_server::abilities::Deaths::kill`, which still has the real entity for scoring
/// before this message is built).
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct KillFeed {
    /// The killer's name, if any (falls, scripts and wrecks have none).
    pub killer_name: Option<String>,
    #[entities]
    pub victim: Entity,
    pub weapon: String,
    pub headshot: bool,
}

/// Server -> a player outside the combat area that applies to them (on foot, or in a
/// vehicle): `Some(seconds)` while the countdown runs, sent again whenever it changes;
/// `None` once they're back inside, cancelling it (BF2's out-of-bounds warning, see
/// `game_server::out_of_bounds`).
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct OutOfBoundsWarning {
    pub seconds_left: Option<f32>,
}
