//! Team deathmatch (`gpm_tdm`, as mods like AIX 2 have it): every death costs the team a
//! ticket (conquest's ticket count without the bleed) and the round ends when a team runs
//! out. Flags don't move: the layout's control points only say where each side spawns.

use bevy::prelude::*;
use game_shared::protocol::Team;

use super::RoundSetup;
use crate::ai::strategy::{Objective, OrderKind, PlanView, Posture};

/// The round: conquest's tickets, every flag locked.
pub(crate) fn setup(_: &mut Commands, setup: &mut RoundSetup) {
    crate::conquest::setup(setup);
    for point in &mut setup.points {
        point.locked = true;
    }
}

/// The bots' strategy: go where the enemy is. Every point is worth going to by how close to
/// the enemy it is (where he was seen lately, else the points he spawns at), our own spawns
/// a little less (they get defended on the way), his spawns much less (no spawn camping).
pub(crate) fn objectives(view: &PlanView, team: Team) -> (Posture, Vec<Objective>) {
    let seen = view.intel.recent(team, 30.0);
    let enemy_points: Vec<Vec3> = view
        .map
        .areas
        .iter()
        .zip(view.flags)
        .filter(|(_, flag)| flag.is_some_and(|f| f.owner == team.opponent()))
        .map(|(area, _)| area.position)
        .collect();
    let targets = if seen.is_empty() { enemy_points } else { seen };
    let mut objectives = Vec::new();
    for (a, area) in view.map.areas.iter().enumerate() {
        let Some(flag) = view.flags.get(a).copied().flatten() else {
            continue;
        };
        let distance = targets
            .iter()
            .map(|t| t.distance(area.position))
            .fold(f32::MAX, f32::min)
            .min(2000.0);
        let mut value = 10.0 / (1.0 + distance / 150.0);
        let kind = if flag.owner == team {
            value *= 0.8;
            OrderKind::Defend
        } else {
            if flag.owner == team.opponent() {
                value *= 0.3;
            }
            OrderKind::Attack
        };
        objectives.push(Objective { area: a, kind, value });
    }
    objectives.sort_by(|a, b| b.value.total_cmp(&a.value));
    (Posture::Attack, objectives)
}
