//! How good bots are: one server-wide skill ([`crate::ServerSettings::bot_skill`], BF2's
//! `aiSettings.setBotSkill`) and a personality per bot that varies around it.

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

    /// Effective skill, 0..1.
    pub fn skill(&self, server_skill: f32) -> Skill {
        Skill((server_skill + self.skill_offset).clamp(0.0, 1.0))
    }
}

/// Aim and reaction parameters for a skill level (0..1).
#[derive(Clone, Copy, Debug)]
pub struct Skill(pub f32);

impl Skill {
    fn lerp(self, worst: f32, best: f32) -> f32 {
        worst + (best - worst) * self.0
    }

    /// Seconds before shooting at a newly seen enemy.
    pub fn reaction_time(self) -> f32 {
        self.lerp(0.9, 0.2) * (0.8 + 0.5 * fastrand::f32())
    }

    /// Aim error when a target is acquired, radians.
    pub fn aim_error(self) -> f32 {
        self.lerp(0.22, 0.04)
    }

    /// How fast the aim error shrinks while tracking, per second.
    pub fn aim_settle(self) -> f32 {
        self.lerp(0.5, 2.2)
    }

    /// Typical aim error once settled on a target at `distance`, radians: how far off the
    /// aim wanders, from over a meter at 50 m for the worst bots to a quarter meter for the
    /// best.
    pub fn aim_spread(self, distance: f32) -> f32 {
        self.lerp(1.2, 0.25) * (0.5 + distance / 100.0) / distance.max(1.0)
    }

    /// Turn rate while aiming, radians per second.
    pub fn turn_rate(self) -> f32 {
        self.lerp(3.0, 7.0)
    }

    /// How far enemies are noticed, meters.
    pub fn sight_range(self) -> f32 {
        self.lerp(100.0, 170.0)
    }

    /// Half the field of view in which enemies are noticed, radians.
    pub fn view_angle(self) -> f32 {
        self.lerp(55.0, 75.0).to_radians()
    }
}
