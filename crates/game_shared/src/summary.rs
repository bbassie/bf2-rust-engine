//! The end-of-round summary: the best players of the round, and each player's own round
//! and career stats (kept by the server, see `game_server::stats`).

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::Team;

/// Server -> each player when a round ends. Personal: `you` differs per recipient.
#[derive(Message, Serialize, Deserialize, Clone, Debug, Default)]
pub struct RoundSummary {
    pub winner: Team,
    /// Best players of the round, best first.
    pub top: Vec<SummaryRow>,
    /// The recipient's own numbers; `None` for spectators.
    pub you: Option<PersonalSummary>,
    /// The map played next: level display name, mode and size.
    pub next_map: Option<(String, String, u32)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SummaryRow {
    pub name: String,
    pub team: Team,
    pub score: i32,
    pub kills: u32,
    pub deaths: u32,
    pub is_bot: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PersonalSummary {
    /// Rank on the scoreboard this round, 1-based.
    pub rank: u32,
    pub round: StatLine,
    /// All rounds on this server, this one included. `None` when the server keeps no
    /// stats for this player (bots).
    pub career: Option<StatLine>,
}

/// Kills, deaths and so on over some stretch of play.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StatLine {
    pub rounds: u32,
    pub score: i32,
    pub kills: u32,
    pub deaths: u32,
    pub captures: u32,
    pub seconds_played: f32,
    /// Kit type played the longest (`Assault`, `Sniper`, ...).
    pub favourite_kit: Option<String>,
    /// Weapon with the most kills.
    pub favourite_weapon: Option<String>,
}
