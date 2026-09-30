//! What clients see of the game modes beyond conquest's flags, tickets and rounds (which are
//! in [`crate::conquest`] and shared by every mode): the mode and stage of the round, Rush's
//! charges and Breakthrough's sectors. The server runs the rules (`game_server::modes`); ids,
//! labels and layouts are in [`game_data::modes`].

use bevy::{ecs::entity::MapEntities, prelude::*};
use game_data::modes::ModeKind;
use serde::{Deserialize, Serialize};

use crate::{
    conquest::{Tickets, team_index},
    protocol::Team,
};

/// The rules of the round and where it stands, on the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct ModeState {
    pub kind: ModeKind,
    /// Staged modes: the attacking team (`Spectator` in the others).
    pub attacker: Team,
    /// Staged modes: the stage being fought over (0-based), and how many there are.
    pub stage: u8,
    pub stages: u8,
    /// Rush: the attackers are out of tickets but a charge is armed; the round goes on until
    /// it is defused or goes off.
    pub overtime: bool,
}

impl ModeState {
    pub fn staged(&self) -> bool {
        self.kind.staged()
    }

    /// Staged modes: the defending team.
    pub fn defender(&self) -> Team {
        self.attacker.opponent()
    }

    /// Whether `team` has tickets to lose: both in conquest, only the attackers in the staged
    /// modes.
    pub fn has_tickets(&self, team: Team) -> bool {
        !self.staged() || team == self.attacker
    }

    /// Whether players of `team` may spawn: not the attackers once they are out of tickets.
    pub fn can_spawn(&self, team: Team, tickets: Option<&Tickets>) -> bool {
        !(self.staged() && team == self.attacker && tickets.is_some_and(|t| t.of(team) <= 0.0))
    }

    /// `Stage 2` / `Sector 2` of the current stage (1-based).
    pub fn stage_label(&self) -> String {
        format!("{} {}", self.kind.stage_noun(), self.stage + 1)
    }
}

/// Seconds a charge (Rush's "M-COM station") takes to arm, to defuse and to go off, from the
/// layout. On the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ChargeTimes {
    pub arm: f32,
    pub defuse: f32,
    pub fuse: f32,
}

/// A charge. One replicated entity per charge of the layout, all stages.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[require(ChargeState)]
pub struct Charge {
    /// Position in the layout (stage by stage).
    pub index: u8,
    pub stage: u8,
    /// `A`, `B`.
    pub name: String,
    /// Its foot. The server moves generated charges onto walkable ground once the level's
    /// navigation grid is there.
    pub position: Vec3,
    /// Radians, the way the object's front faces (0 = north, counter-clockwise).
    pub yaw: f32,
    /// Object template drawn for it; the game's own marker without one.
    pub template: Option<String>,
}

impl Charge {
    /// How close (meters, from the feet) a soldier must be to work on a charge.
    pub const REACH: f32 = 2.2;

    /// Whether a soldier standing at `feet` can reach it.
    pub fn in_reach(&self, feet: Vec3) -> bool {
        feet.xz().distance(self.position.xz()) <= Self::REACH && (feet.y - self.position.y).abs() < 2.0
    }
}

/// Where a charge stands. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum ChargeState {
    /// Its stage hasn't come yet.
    #[default]
    Waiting,
    /// To be armed: `progress` 0..1 while an attacker holds the use key at it (going back
    /// down when he stops).
    Active { progress: f32 },
    /// Goes off in `fuse` seconds unless defused; `progress` 0..1 while a defender defuses.
    Armed { fuse: f32, progress: f32 },
    Destroyed,
}

impl ChargeState {
    pub fn in_play(&self) -> bool {
        matches!(self, ChargeState::Active { .. } | ChargeState::Armed { .. })
    }

    pub fn armed(&self) -> bool {
        matches!(self, ChargeState::Armed { .. })
    }
}

/// Breakthrough: which sector (stage) a control point belongs to. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sector(pub u8);

/// A control point outside the stage being fought over: its flag stays where it is. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Locked;

/// Staged modes: players of this team (the point's owner) can't spawn at the point right now,
/// for this reason. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnBlocked(pub Team, pub SpawnBlockReason);

impl SpawnBlocked {
    /// Whether `team` can't spawn at the point.
    pub fn blocks(&self, team: Team) -> bool {
        self.0 == team
    }

    /// Closed while the front is where it is (not a spawn option at all), rather than for as
    /// long as enemies are at it.
    pub fn off_front(&self) -> bool {
        self.1 == SpawnBlockReason::Front
    }
}

/// Why a point is closed for spawning.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnBlockReason {
    /// Enemies are at it.
    Enemies,
    /// Too far from the stage being fought over: the owner's spawns follow the front.
    Front,
}

/// Whether players of `team` may spawn at a point held by `owner` and closed by `blocked`.
pub fn can_spawn_at(owner: Team, blocked: Option<&SpawnBlocked>, team: Team) -> bool {
    owner == team && blocked.is_none_or(|b| !b.blocks(team))
}

/// Server -> everyone: an objective changed.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug, MapEntities)]
pub struct ObjectiveEvent {
    pub kind: ObjectiveEventKind,
    /// The charge, for charge events.
    #[entities]
    pub charge: Option<Entity>,
    /// The team that did it.
    pub team: Team,
    /// The stage it happened in (0-based).
    pub stage: u8,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectiveEventKind {
    Armed,
    Defused,
    Destroyed,
    /// The attackers took the stage: the front moves on.
    StageTaken,
}

/// The team index of the attackers, if the mode has any.
pub fn attacker_index(state: &ModeState) -> Option<usize> {
    state.staged().then(|| team_index(state.attacker)).flatten()
}
