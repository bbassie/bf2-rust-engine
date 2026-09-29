//! How good bots are: a server-wide difficulty ([`BotDifficulty`], `--bot-difficulty`), one
//! server-wide skill ([`crate::ServerSettings::bot_skill`], BF2's `aiSettings.setBotSkill`,
//! which the difficulty sets unless given) and a personality per bot that varies around it.

use serde::{Deserialize, Serialize};

/// How hard the bots are, a server setting (`--bot-difficulty`, the host menu). It sets the
/// default [`crate::ServerSettings::bot_skill`] (aim and reaction) and scales reaction time,
/// aim error, how often bots use their tactics (cover, flanking, suppressive fire, grenades)
/// and how far they notice enemies (sight, hearing, memory).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotDifficulty {
    Easy,
    #[default]
    Normal,
    Hard,
    Expert,
}

/// What a [`BotDifficulty`] changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DifficultyParams {
    /// Default bot skill, 0..1 (aim, reaction, turn rate; see [`Skill`]).
    pub skill: f32,
    /// Reaction time factor.
    pub reaction: f32,
    /// Aim error factor.
    pub aim: f32,
    /// Tactical depth, 0..1: the chance a bot takes cover in a firefight, flanks when pinned,
    /// lays down suppressive fire, and throws grenades behind cover.
    pub tactics: f32,
    /// Awareness factor: sight and hearing range, how long enemies are remembered.
    pub awareness: f32,
}

impl BotDifficulty {
    pub const ALL: [BotDifficulty; 4] = [Self::Easy, Self::Normal, Self::Hard, Self::Expert];

    pub fn params(self) -> DifficultyParams {
        let (skill, reaction, aim, tactics, awareness) = match self {
            Self::Easy => (0.25, 1.4, 1.5, 0.3, 0.8),
            Self::Normal => (0.5, 1.0, 1.0, 0.65, 1.0),
            Self::Hard => (0.7, 0.8, 0.8, 0.85, 1.15),
            Self::Expert => (0.9, 0.65, 0.65, 1.0, 1.3),
        };
        DifficultyParams { skill, reaction, aim, tactics, awareness }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Easy => "Easy",
            Self::Normal => "Normal",
            Self::Hard => "Hard",
            Self::Expert => "Expert",
        }
    }

    /// `easy`, `normal`, `hard`, `expert` (any case), or a digit 0..3.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "easy" | "0" => Some(Self::Easy),
            "normal" | "medium" | "1" => Some(Self::Normal),
            "hard" | "2" => Some(Self::Hard),
            "expert" | "3" => Some(Self::Expert),
            _ => None,
        }
    }
}

impl std::str::FromStr for BotDifficulty {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Self::parse(text).ok_or_else(|| format!("unknown bot difficulty `{text}` (easy, normal, hard, expert)"))
    }
}

impl std::fmt::Display for BotDifficulty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Rolled once per bot.
#[derive(Clone, Copy, Debug)]
pub struct Personality {
    /// Added to the server's skill.
    pub skill_offset: f32,
    /// 0..1: pushes, flanks and throws grenades rather than holding back.
    pub aggression: f32,
    /// 0..1: keeps fighting when hurt rather than looking for cover.
    pub courage: f32,
    /// 0..1: keeps close to its squad.
    pub teamwork: f32,
    /// Kit kind it likes to play (`KitDesc::kind`), if any.
    pub favourite_kit: Option<&'static str>,
}

const KIT_KINDS: [&str; 7] = ["Assault", "Support", "Medic", "Specops", "Engineer", "Sniper", "AT"];

impl Personality {
    pub fn roll() -> Self {
        let spread = |width: f32| (fastrand::f32() + fastrand::f32() - 1.0) * width;
        Self {
            skill_offset: spread(0.15),
            aggression: fastrand::f32(),
            courage: fastrand::f32(),
            teamwork: 0.4 + 0.6 * fastrand::f32(),
            favourite_kit: (fastrand::f32() < 0.5).then(|| KIT_KINDS[fastrand::usize(..KIT_KINDS.len())]),
        }
    }

    /// Effective skill of a bot of `team`: the server's skill (0..1) and difficulty (or the
    /// team's, testing with `--bot-team-difficulty`), varied by this bot.
    pub fn skill(&self, settings: &crate::ServerSettings, team: game_shared::protocol::Team) -> Skill {
        let team_number = game_shared::conquest::team_index(team).map(|t| t as u8 + 1);
        match settings.bot_team_difficulty {
            Some((number, difficulty)) if Some(number) == team_number => {
                let params = difficulty.params();
                Skill((params.skill + self.skill_offset).clamp(0.0, 1.0), params)
            }
            _ => Skill((settings.bot_skill + self.skill_offset).clamp(0.0, 1.0), settings.bot_difficulty.params()),
        }
    }
}

/// Aim and reaction parameters for a skill level (0..1) at a difficulty.
#[derive(Clone, Copy, Debug)]
pub struct Skill(pub f32, pub DifficultyParams);

impl Skill {
    fn lerp(self, worst: f32, best: f32) -> f32 {
        worst + (best - worst) * self.0
    }

    /// Seconds before shooting at a newly seen enemy.
    pub fn reaction_time(self) -> f32 {
        self.lerp(0.9, 0.2) * (0.8 + 0.5 * fastrand::f32()) * self.1.reaction
    }

    /// Aim error when a target is acquired, radians.
    pub fn aim_error(self) -> f32 {
        self.lerp(0.22, 0.04) * self.1.aim
    }

    /// How fast the aim error shrinks while tracking, per second.
    pub fn aim_settle(self) -> f32 {
        self.lerp(0.5, 2.2)
    }

    /// Typical aim error once settled on a target at `distance`, radians: how far off the
    /// aim wanders, from over a meter at 50 m for the worst bots to a quarter meter for the
    /// best.
    pub fn aim_spread(self, distance: f32) -> f32 {
        self.lerp(1.2, 0.25) * (0.5 + distance / 100.0) / distance.max(1.0) * self.1.aim
    }

    /// Turn rate while aiming, radians per second.
    pub fn turn_rate(self) -> f32 {
        self.lerp(3.0, 7.0)
    }

    /// How far enemies are noticed, meters.
    pub fn sight_range(self) -> f32 {
        self.lerp(100.0, 170.0) * self.1.awareness
    }

    /// How far gunfire is heard, meters (footsteps: a fifth of it).
    pub fn hearing_range(self) -> f32 {
        75.0 * self.1.awareness
    }

    /// Seconds an enemy position is remembered once out of sight.
    pub fn memory(self) -> f32 {
        14.0 * self.1.awareness
    }

    /// Tactical depth, 0..1 (see [`DifficultyParams::tactics`]).
    pub fn tactics(self) -> f32 {
        self.1.tactics
    }

    /// Half the field of view in which enemies are noticed, radians.
    pub fn view_angle(self) -> f32 {
        self.lerp(55.0, 75.0).to_radians()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harder_is_harder() {
        let levels = BotDifficulty::ALL.map(|d| d.params());
        for pair in levels.windows(2) {
            let (easier, harder) = (pair[0], pair[1]);
            assert!(harder.skill > easier.skill && harder.reaction < easier.reaction && harder.aim < easier.aim);
            assert!(harder.tactics > easier.tactics && harder.awareness > easier.awareness);
        }
        // Normal keeps the skill settings as they were.
        let normal = BotDifficulty::Normal.params();
        assert_eq!((normal.skill, normal.reaction, normal.aim, normal.awareness), (0.5, 1.0, 1.0, 1.0));
        assert_eq!(BotDifficulty::parse("Expert"), Some(BotDifficulty::Expert));
        assert_eq!("hard".parse::<BotDifficulty>(), Ok(BotDifficulty::Hard));
        assert!(BotDifficulty::parse("impossible").is_none());
    }
}
