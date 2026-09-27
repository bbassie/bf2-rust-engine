//! BF2's radio: the commo rose's messages (acknowledgements, requests, orders) and spotting.
//!
//! A player sends a [`RadioRequest`]; the server checks it (a living soldier, not spamming:
//! BF2's `sv.radioSpamInterval` rules), turns "spotted" into what was spotted, marks that
//! for the spotter's team with [`Spotted`] for [`SPOT_SECONDS`], and tells every client with
//! a [`RadioMessage`]. Clients play the voice-over to those who'd hear it (the squad over
//! the radio, anyone close by in person) and show the text. The rules run in
//! `game_server::radio`, the voices and markers in `game_client::radio`.

use bevy::{ecs::entity::MapEntities, prelude::*};
use serde::{Deserialize, Serialize};

use crate::protocol::Team;

/// How long a spotted enemy stays marked, seconds [inferred: BF2 players measured about
/// 20 s; the game files don't say].
pub const SPOT_SECONDS: f32 = 20.0;
/// How far off the aim (degrees) and how far away (meters) an enemy can be spotted.
pub const SPOT_CONE: f32 = 6.0;
pub const SPOT_RANGE: f32 = 300.0;

/// A radio message.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RadioCommand {
    Affirmative,
    Negative,
    Thanks,
    Sorry,
    NeedMedic,
    NeedAmmo,
    NeedRepair,
    NeedPickup,
    GoGoGo,
    FollowMe,
    /// What a player asks for; the server answers with what was spotted, or
    /// [`RadioCommand::EnemySpotted`] when nothing in particular was.
    Spotted,
    EnemySpotted,
    SpottedInfantry,
    SpottedSniper,
    SpottedVehicle,
    SpottedApc,
    SpottedTank,
    SpottedAntiAir,
    SpottedHelicopter,
}

impl RadioCommand {
    /// The commo rose, clockwise from the top.
    pub const ROSE: [RadioCommand; 10] = [
        RadioCommand::Spotted,
        RadioCommand::Affirmative,
        RadioCommand::Thanks,
        RadioCommand::GoGoGo,
        RadioCommand::FollowMe,
        RadioCommand::NeedPickup,
        RadioCommand::NeedAmmo,
        RadioCommand::NeedMedic,
        RadioCommand::Sorry,
        RadioCommand::Negative,
    ];

    /// BF2's message id (`common/sound/voicemessages*.con`).
    pub fn message_id(self) -> &'static str {
        match self {
            RadioCommand::Affirmative => "roger_that",
            RadioCommand::Negative => "negative",
            RadioCommand::Thanks => "PLAYER_CONFIRM_thankyou",
            RadioCommand::Sorry => "PLAYER_CONFIRM_sorry",
            RadioCommand::NeedMedic => "medic",
            RadioCommand::NeedAmmo => "need_ammo",
            RadioCommand::NeedRepair => "need_repair",
            RadioCommand::NeedPickup => "req_pickup",
            RadioCommand::GoGoGo => "PLAYER_TACTICS_gogogo",
            RadioCommand::FollowMe => "PLAYER_TACTICS_followme",
            RadioCommand::Spotted | RadioCommand::EnemySpotted => "spotted",
            RadioCommand::SpottedInfantry => "infantry_spotted",
            RadioCommand::SpottedSniper => "sniper_spotted",
            RadioCommand::SpottedVehicle => "vehicle_spotted",
            RadioCommand::SpottedApc => "apc_spotted",
            RadioCommand::SpottedTank => "tank_spotted",
            RadioCommand::SpottedAntiAir => "aa_spotted",
            RadioCommand::SpottedHelicopter => "heli_spotted",
        }
    }

    /// The commo rose's label.
    pub fn label(self) -> &'static str {
        match self {
            RadioCommand::Affirmative => "Affirmative",
            RadioCommand::Negative => "Negative",
            RadioCommand::Thanks => "Thanks",
            RadioCommand::Sorry => "Sorry",
            RadioCommand::NeedMedic => "Need medic",
            RadioCommand::NeedAmmo => "Need ammo",
            RadioCommand::NeedRepair => "Need repairs",
            RadioCommand::NeedPickup => "Need pickup",
            RadioCommand::GoGoGo => "Go go go",
            RadioCommand::FollowMe => "Follow me",
            RadioCommand::Spotted | RadioCommand::EnemySpotted => "Spotted",
            RadioCommand::SpottedInfantry => "Enemy infantry",
            RadioCommand::SpottedSniper => "Enemy sniper",
            RadioCommand::SpottedVehicle => "Enemy vehicle",
            RadioCommand::SpottedApc => "Enemy APC",
            RadioCommand::SpottedTank => "Enemy tank",
            RadioCommand::SpottedAntiAir => "Enemy AA",
            RadioCommand::SpottedHelicopter => "Enemy helicopter",
        }
    }

    /// Reports an enemy: heard by the whole team, not just the squad.
    pub fn is_spot(self) -> bool {
        matches!(
            self,
            RadioCommand::Spotted
                | RadioCommand::EnemySpotted
                | RadioCommand::SpottedInfantry
                | RadioCommand::SpottedSniper
                | RadioCommand::SpottedVehicle
                | RadioCommand::SpottedApc
                | RadioCommand::SpottedTank
                | RadioCommand::SpottedAntiAir
                | RadioCommand::SpottedHelicopter
        )
    }

    /// What spotting a vehicle of this template reports, from BF2's naming (`tnk`, `apc`,
    /// `aav`, `ahe`/`the` helicopters) [inferred].
    pub fn vehicle_spotted(template: &str) -> Self {
        let name = template.to_ascii_lowercase();
        if name.contains("tnk") {
            RadioCommand::SpottedTank
        } else if name.contains("aav") {
            RadioCommand::SpottedAntiAir
        } else if name.contains("apc") || name.contains("bmp") {
            RadioCommand::SpottedApc
        } else if name.contains("ahe_") || name.contains("the_") {
            RadioCommand::SpottedHelicopter
        } else {
            RadioCommand::SpottedVehicle
        }
    }
}

/// Client -> server: say something on the radio.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RadioRequest {
    pub command: RadioCommand,
}

/// Server -> clients: a player said something on the radio.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug, MapEntities)]
pub struct RadioMessage {
    #[entities]
    pub player: Entity,
    pub command: RadioCommand,
    /// Where the speaker was.
    pub position: Vec3,
    /// What was spotted.
    #[entities]
    pub target: Option<Entity>,
}

/// On a soldier or vehicle a team spotted. Replicated (to everyone: clients show it to the
/// spotting team); the server removes it after [`SPOT_SECONDS`].
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spotted {
    pub by: Team,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_vehicles() {
        assert_eq!(RadioCommand::vehicle_spotted("ustnk_m1a2"), RadioCommand::SpottedTank);
        assert_eq!(RadioCommand::vehicle_spotted("tnk_type98"), RadioCommand::SpottedTank);
        assert_eq!(RadioCommand::vehicle_spotted("apc_btr90"), RadioCommand::SpottedApc);
        assert_eq!(RadioCommand::vehicle_spotted("usapc_lav25"), RadioCommand::SpottedApc);
        assert_eq!(RadioCommand::vehicle_spotted("aav_tunguska"), RadioCommand::SpottedAntiAir);
        assert_eq!(RadioCommand::vehicle_spotted("usjep_hmmwv"), RadioCommand::SpottedVehicle);
        assert_eq!(RadioCommand::vehicle_spotted("ahe_havoc"), RadioCommand::SpottedHelicopter);
    }
}
