//! Co-op (`gpm_coop`), after BF2: humans play together on one team, bots fill both teams.
//! The rules are conquest's (BF2's `gpm_coop.py` is `gpm_cq.py` renamed; the levels have
//! their own co-op layouts and AI areas). The server settings say how many bots there are
//! (BF2's `sv.coopBotCount`), how the soldiers split between the teams
//! (`sv.coopBotRatio`) and how good the bots are (`sv.coopBotDifficulty`).

use bevy::prelude::*;
use game_shared::{
    conquest::Deployment,
    protocol::{MatchInfo, Player, Team},
    squad::SquadMember,
};

use crate::{Controls, ServerSettings};

/// BF2's co-op mode.
pub const COOP: &str = "gpm_coop";

/// Co-op settings.
#[derive(Clone, Debug)]
pub struct CoopSettings {
    /// The humans' team, 1 or 2.
    pub human_team: u8,
    /// Percent of all soldiers (humans and bots) on the bots' team: 50 makes even teams
    /// (BF2's `sv.coopBotRatio`).
    pub bot_ratio: f32,
    /// Bot skill 0..1 on co-op maps (BF2's `sv.coopBotDifficulty`); the server's otherwise.
    pub bot_skill: Option<f32>,
}

impl Default for CoopSettings {
    fn default() -> Self {
        Self {
            human_team: 1,
            bot_ratio: 50.0,
            bot_skill: None,
        }
    }
}

pub fn is_coop(mode: &str) -> bool {
    mode.eq_ignore_ascii_case(COOP)
}

/// The team humans play on in co-op.
pub fn human_team(settings: &ServerSettings) -> Team {
    if settings.coop.human_team == 2 { Team::Two } else { Team::One }
}

/// How many of `bots` belong on the bots' team, with `humans` on the other.
pub fn bots_on_ai_team(humans: usize, bots: usize, ratio: f32) -> usize {
    let soldiers = (humans + bots) as f32;
    ((soldiers * ratio.clamp(0.0, 100.0) / 100.0).round() as usize).min(bots)
}

/// Seconds the teams may stay uneven, waiting for a bot to die and change sides, before a
/// bot leaves (and the bot filler adds one on the other team).
const IMBALANCE_SECONDS: f32 = 10.0;

/// Keeps humans on their team and splits the bots so the teams come out as the bot ratio
/// says. Only players without a soldier change teams (bots move as they die), so nobody
/// switches sides in the middle of a fight; if that takes too long, a bot leaves instead.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(crate) fn balance_teams(
    mut commands: Commands,
    time: Res<Time>,
    settings: Res<ServerSettings>,
    matches: Query<&MatchInfo>,
    mut players: Query<(Entity, &Player, &mut Team, &mut Deployment, Option<&Controls>)>,
    mut uneven_for: Local<f32>,
) {
    let Ok(info) = matches.single() else {
        return;
    };
    if !is_coop(&info.mode) {
        *uneven_for = 0.0;
        return;
    }
    let human = human_team(&settings);
    let ai = human.opponent();
    let mut move_to = |entity: Entity, team: &mut Team, deployment: &mut Deployment, to: Team| {
        *team = to;
        // Squads and spawn points belong to the old team.
        deployment.control_point = None;
        deployment.on_squad_leader = false;
        commands.entity(entity).remove::<SquadMember>();
    };
    let (mut humans, mut bots, mut ai_bots) = (0, 0, 0);
    for (entity, player, mut team, mut deployment, controls) in &mut players {
        let alive = controls.is_some();
        if *team == Team::Spectator {
            continue;
        }
        if player.is_bot {
            bots += 1;
            ai_bots += usize::from(*team == ai);
        } else {
            humans += 1;
            if *team != human && !alive {
                move_to(entity, &mut team, &mut deployment, human);
            }
        }
    }
    let target = bots_on_ai_team(humans, bots, settings.coop.bot_ratio);
    let (from, to, mut count) = match ai_bots.cmp(&target) {
        std::cmp::Ordering::Less => (human, ai, target - ai_bots),
        std::cmp::Ordering::Greater => (ai, human, ai_bots - target),
        std::cmp::Ordering::Equal => {
            *uneven_for = 0.0;
            return;
        }
    };
    let mut leaver = None;
    for (entity, player, mut team, mut deployment, controls) in &mut players {
        if count == 0 {
            break;
        }
        if !player.is_bot || *team != from {
            continue;
        }
        match controls {
            None => {
                move_to(entity, &mut team, &mut deployment, to);
                count -= 1;
            }
            Some(soldier) => leaver = Some((entity, soldier.0)),
        }
    }
    if count == 0 {
        *uneven_for = 0.0;
        return;
    }
    *uneven_for += time.delta_secs();
    if *uneven_for > IMBALANCE_SECONDS
        && let Some((bot, soldier)) = leaver
    {
        *uneven_for = 0.0;
        commands.entity(soldier).try_despawn();
        commands.entity(bot).try_despawn();
        info!("co-op: a bot leaves {from:?} to even the teams");
    }
}

#[cfg(test)]
mod tests {
    use super::bots_on_ai_team;

    #[test]
    fn even_teams() {
        // One human and 15 bots: 8 against 8.
        assert_eq!(bots_on_ai_team(1, 15, 50.0), 8);
        // Four humans and 16 bots: 10 against 10.
        assert_eq!(bots_on_ai_team(4, 16, 50.0), 10);
        // More humans than bots: every bot on the bots' team.
        assert_eq!(bots_on_ai_team(20, 16, 50.0), 16);
        // A tougher ratio: 70% of 20 soldiers against the humans.
        assert_eq!(bots_on_ai_team(4, 16, 70.0), 14);
    }
}
