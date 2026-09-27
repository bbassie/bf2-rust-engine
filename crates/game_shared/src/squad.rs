//! Squads, like BF2: up to six players of a team with a leader, named Alpha, Bravo, ...
//! Members can spawn on their leader. The server runs them (`game_server::squads`); the
//! membership replicates on the player entities.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Players per squad, leader included.
pub const MAX_MEMBERS: usize = 6;
/// Squads per team.
pub const MAX_SQUADS: u8 = 9;

const NAMES: [&str; MAX_SQUADS as usize] = [
    "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India",
];

/// Squad name for a squad number (1-based).
pub fn squad_name(squad: u8) -> &'static str {
    NAMES.get(squad.wrapping_sub(1) as usize).copied().unwrap_or("Squad")
}

/// A player's squad within its team. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SquadMember {
    /// 1..=MAX_SQUADS.
    pub squad: u8,
    pub leader: bool,
}

/// Client -> server: squad management from the deploy screen.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SquadRequest {
    /// Start a new squad and lead it.
    Create,
    Join(u8),
    Leave,
}
