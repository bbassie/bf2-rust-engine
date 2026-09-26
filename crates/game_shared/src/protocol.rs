//! Network protocol: which components replicate and which messages exist.
//!
//! Registration order must be identical on client and server; keeping it all in this one
//! plugin guarantees that. Replicon hashes the protocol and refuses mismatched clients.

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    input::InputPacket,
    soldier::{Health, InputAck, Soldier, SoldierMotion},
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
            .add_client_message::<InputPacket>(Channel::Unreliable)
            .add_client_message::<ClientHello>(Channel::Ordered);
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
#[require(Team, Score)]
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
