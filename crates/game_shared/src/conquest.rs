//! Conquest: control points, tickets and rounds. The server runs the rules
//! (`game_server::conquest`, after BF2's `gpm_cq`); clients read these replicated
//! components for the HUD and the deploy screen.
//!
//! Every game mode has these: flags to take or to spawn at, tickets (only the attackers' in
//! the staged modes) and rounds. What the other modes add is in [`crate::modes`].

use bevy::{ecs::entity::MapEntities, prelude::*};
use serde::{Deserialize, Serialize};

use crate::protocol::Team;

/// A capturable flag. One replicated entity per control point of the current layout.
#[derive(Component, Serialize, Deserialize, Clone, Debug)]
#[require(FlagState)]
pub struct ControlPoint {
    /// Position in the layout's control point list; deploy requests refer to it.
    pub index: u8,
    pub name: String,
    pub position: Vec3,
    pub radius: f32,
    pub uncapturable: bool,
}

impl ControlPoint {
    /// Whether a soldier standing at `feet` is inside the capture radius. BF2 uses a sphere
    /// around the flag.
    pub fn contains(&self, feet: Vec3) -> bool {
        (feet + Vec3::Y * 0.9).distance(self.position) <= self.radius
    }
}

/// Where a control point's flag is. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct FlagState {
    /// Team holding the point (`Spectator` = neutral).
    pub owner: Team,
    /// Team whose flag is on the pole.
    pub flag: Team,
    /// Flag height from 0 (bottom) to 1 (top).
    pub height: f32,
    /// Height change per second; positive while raising.
    pub rate: f32,
}

impl FlagState {
    /// A point held by `owner` with its flag at the top, or neutral with no flag up.
    pub fn held_by(owner: Team) -> Self {
        Self {
            owner,
            flag: owner,
            height: if owner == Team::Spectator { 0.0 } else { 1.0 },
            rate: 0.0,
        }
    }
}

/// Tickets of both teams, on the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Tickets {
    pub remaining: [f32; 2],
    pub start: [f32; 2],
    /// Tickets lost per second right now.
    pub bleed: [f32; 2],
}

impl Tickets {
    pub fn of(&self, team: Team) -> f32 {
        team_index(team).map_or(0.0, |i| self.remaining[i])
    }
}

/// The phase of the round, on the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum RoundState {
    #[default]
    Playing,
    /// `winner` is `Spectator` for a draw. The next round starts after `restart_in` s.
    Ended { winner: Team, restart_in: f32 },
}

/// A player's choices for the next spawn and the time until it. Replicated, so clients
/// show the server's view of their selection.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Deployment {
    /// Kit slot (0..7).
    pub kit: u8,
    /// Preferred control point index; any owned one if unset or lost.
    pub control_point: Option<u8>,
    /// Spawn on our squad leader (when alive), before any control point.
    pub on_squad_leader: bool,
    /// Seconds until the next spawn while dead; 0 while alive.
    pub respawn_in: f32,
}

impl Default for Deployment {
    fn default() -> Self {
        Self {
            // Assault: the most generally useful kit.
            kit: 2,
            control_point: None,
            on_squad_leader: false,
            respawn_in: 0.0,
        }
    }
}

/// Client -> server: kit and spawn point for the next spawn.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct DeployRequest {
    pub kit: u8,
    pub control_point: Option<u8>,
    pub on_squad_leader: bool,
}

/// Server -> everyone: a control point changed hands.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug, MapEntities)]
pub struct FlagEvent {
    #[entities]
    pub control_point: Entity,
    pub kind: FlagEventKind,
    /// The team that captured or neutralized it.
    pub team: Team,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagEventKind {
    Captured,
    Neutralized,
}

/// 0 for team one, 1 for team two.
pub fn team_index(team: Team) -> Option<usize> {
    match team {
        Team::One => Some(0),
        Team::Two => Some(1),
        Team::Spectator => None,
    }
}

/// BF2's team numbers: 0 neutral, 1, 2.
pub fn team_from_id(id: u8) -> Team {
    match id {
        1 => Team::One,
        2 => Team::Two,
        _ => Team::Spectator,
    }
}
