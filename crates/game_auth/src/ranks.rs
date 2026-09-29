//! XP and ranks. Players earn XP from their score (like BF2's global score, which decided its
//! ranks), a little per minute played and a bonus for winning. The ranks and how XP is earned
//! are adjustable in the master server's config file (`progression`).

use serde::{Deserialize, Serialize};

use crate::api::RankInfo;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rank {
    pub name: String,
    /// Abbreviation, for scoreboards: `Sgt`.
    pub short: String,
    /// XP needed.
    pub xp: u64,
}

fn rank(name: &str, short: &str, xp: u64) -> Rank {
    Rank { name: name.into(), short: short.into(), xp }
}

/// How XP is earned and the ranks it leads to.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Progression {
    /// Lowest first; the first one needs 0 XP.
    pub ranks: Vec<Rank>,
    /// XP per point of score (negative rounds give nothing, XP is never lost).
    pub xp_per_score: f64,
    /// XP per minute played.
    pub xp_per_minute: f64,
    /// XP for being on the winning team.
    pub win_bonus: u64,
    /// At most this much XP from one round.
    pub max_round_xp: u64,
}

impl Default for Progression {
    fn default() -> Self {
        // BF2's ranks and the global score they needed (the ones that also needed medals or
        // were one of a kind are left out, the generals spread out).
        Self {
            ranks: vec![
                rank("Private", "Pvt", 0),
                rank("Private First Class", "PFC", 150),
                rank("Lance Corporal", "LCpl", 500),
                rank("Corporal", "Cpl", 800),
                rank("Sergeant", "Sgt", 2_500),
                rank("Staff Sergeant", "SSgt", 5_000),
                rank("Gunnery Sergeant", "GySgt", 8_000),
                rank("Master Sergeant", "MSgt", 20_000),
                rank("Master Gunnery Sergeant", "MGySgt", 50_000),
                rank("Second Lieutenant", "2ndLt", 60_000),
                rank("First Lieutenant", "1stLt", 75_000),
                rank("Captain", "Capt", 90_000),
                rank("Major", "Maj", 115_000),
                rank("Lieutenant Colonel", "LtCol", 125_000),
                rank("Colonel", "Col", 150_000),
                rank("Brigadier General", "BGen", 180_000),
                rank("Major General", "MajGen", 200_000),
                rank("Lieutenant General", "LtGen", 225_000),
                rank("General", "Gen", 250_000),
            ],
            xp_per_score: 1.0,
            xp_per_minute: 0.5,
            win_bonus: 10,
            max_round_xp: 5_000,
        }
    }
}

impl Progression {
    /// Sorts the ranks and makes sure there is one for 0 XP.
    pub fn normalize(&mut self) {
        self.ranks.retain(|r| !r.name.trim().is_empty());
        self.ranks.sort_by_key(|r| r.xp);
        if self.ranks.first().is_none_or(|r| r.xp > 0) {
            self.ranks.insert(0, rank("Recruit", "Rct", 0));
        }
        self.xp_per_score = self.xp_per_score.clamp(0.0, 1000.0);
        self.xp_per_minute = self.xp_per_minute.clamp(0.0, 1000.0);
    }

    /// Index of the rank `xp` reaches.
    pub fn rank_index(&self, xp: u64) -> usize {
        self.ranks.iter().rposition(|r| r.xp <= xp).unwrap_or(0)
    }

    pub fn info(&self, xp: u64) -> RankInfo {
        let index = self.rank_index(xp);
        let current = self.ranks.get(index);
        let next = self.ranks.get(index + 1);
        RankInfo {
            index: index as u32,
            name: current.map_or_else(|| "Recruit".into(), |r| r.name.clone()),
            short: current.map_or_else(|| "Rct".into(), |r| r.short.clone()),
            xp,
            rank_xp: current.map_or(0, |r| r.xp),
            next_name: next.map(|r| r.name.clone()),
            next_xp: next.map(|r| r.xp),
        }
    }

    /// XP earned in a round.
    pub fn round_xp(&self, score: i32, seconds: f64, won: bool) -> u64 {
        let from_score = (score.max(0) as f64 * self.xp_per_score).floor() as u64;
        let from_time = (seconds.max(0.0) / 60.0 * self.xp_per_minute).floor() as u64;
        let bonus = if won { self.win_bonus } else { 0 };
        (from_score + from_time + bonus).min(self.max_round_xp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_and_xp() {
        let p = Progression::default();
        assert_eq!(p.info(0).name, "Private");
        assert_eq!(p.info(149).short, "Pvt");
        assert_eq!(p.info(150).short, "PFC");
        let sgt = p.info(3000);
        assert_eq!((sgt.name.as_str(), sgt.rank_xp, sgt.next_xp), ("Sergeant", 2500, Some(5000)));
        let general = p.info(10_000_000);
        assert_eq!(general.short, "Gen");
        assert_eq!(general.next_xp, None);
        assert_eq!(p.round_xp(100, 600.0, true), 100 + 5 + 10);
        assert_eq!(p.round_xp(-50, 0.0, false), 0);
        assert_eq!(p.round_xp(1_000_000, 0.0, false), p.max_round_xp);

        let mut custom = Progression { ranks: vec![rank("Two", "2", 100), rank("One", "1", 10)], ..Default::default() };
        custom.normalize();
        let names: Vec<&str> = custom.ranks.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Recruit", "One", "Two"]);
    }
}
