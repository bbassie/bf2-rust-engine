//! Loadouts: the weapons each kit class may carry, and a player's picks among them.
//!
//! BF2 gives every faction seven kits, each with its own primary weapon, and unlocks one
//! more per kit class (BF2 1.5), plus those of Special Forces and the booster packs
//! (`KitDesc::unlocks`, from BF2's `ItemContainer`s). Here a kit class (its `kind`:
//! Assault, Medic, Support, Engineer, AT, Sniper, Specops) may use the primary weapon of any
//! kit of that class, of any faction and expansion, installed mod kits included: that is the
//! class's **pool** ([`Arsenal`]). Pistols form one pool for every class. The kit's gadgets
//! (medic bag, wrench, ...) never change.
//!
//! The server decides what is allowed ([`LoadoutRules`], replicated on the match entity):
//! BF2's kits as they are ("classic"), or picks from the pools ("arsenal", the default),
//! optionally only weapons of the player's own team's factions, and optionally unlock
//! weapons only from an account rank on. Players send their picks per class
//! ([`LoadoutRequest`]); what the server accepted comes back as [`LoadoutPicks`] on the
//! player, and the next soldier of that class carries it ([`Arsenal::kit_weapons`]).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
};

use bevy::prelude::*;
use game_data::{FireKind, KitDesc, WeaponDesc};
use serde::{Deserialize, Serialize};

use crate::{config::GamePaths, level::LoadedLevel, weapons::Armory};

pub struct ArsenalPlugin;

impl Plugin for ArsenalPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Arsenal>().add_systems(
            Update,
            load_arsenal
                .after(crate::weapons::load_armory)
                .run_if(resource_exists_and_changed::<LoadedLevel>),
        );
    }
}

/// What the server allows players to carry. Replicated on the match entity.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LoadoutRules {
    /// Players pick their kit's primary weapon and sidearm from the class's pool; off: BF2's
    /// kits exactly as they are.
    pub arsenal: bool,
    /// Only weapons that kits of the player's own team's factions carry.
    pub faction_locked: bool,
    /// Unlock weapons need an account rank: the rank index each unlock level needs (the
    /// last entry for higher levels). Empty: everything is unlocked.
    pub unlock_ranks: Vec<u32>,
}

impl Default for LoadoutRules {
    fn default() -> Self {
        Self {
            arsenal: true,
            faction_locked: false,
            unlock_ranks: Vec::new(),
        }
    }
}

/// A player's weapons for one kit class; `None` keeps the kit's own.
#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassPick {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub sidearm: Option<String>,
}

/// Settings files (human readable) leave a `None` out; on the network (postcard, no field
/// names) both are always there: a field left out made the reader run off the end of a
/// [`LoadoutRequest`] with only a primary picked, and the server dropped it.
impl Serialize for ClassPick {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let human = serializer.is_human_readable();
        let fields = if human { usize::from(self.primary.is_some()) + usize::from(self.sidearm.is_some()) } else { 2 };
        let mut state = serializer.serialize_struct("ClassPick", fields)?;
        for (name, value) in [("primary", &self.primary), ("sidearm", &self.sidearm)] {
            if human && value.is_none() {
                state.skip_field(name)?;
            } else {
                state.serialize_field(name, value)?;
            }
        }
        state.end()
    }
}

impl ClassPick {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.sidearm.is_none()
    }
}

/// Client -> server: the weapons the player wants, per kit class (lowercase `kind`).
/// Replaces what was sent before.
#[derive(Message, Serialize, Deserialize, Clone, Debug, Default)]
pub struct LoadoutRequest {
    pub picks: Vec<(String, ClassPick)>,
}

/// At most this many classes per request, and this long a name.
pub const MAX_PICKS: usize = 16;
pub const MAX_NAME: usize = 64;

/// The picks the server accepted, per kit class (lowercase `kind`). Replicated on the
/// player.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LoadoutPicks(pub BTreeMap<String, ClassPick>);

/// Which part of a kit a pick replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickSlot {
    Primary,
    Sidearm,
}

/// A weapon of a pool.
#[derive(Clone, Debug, PartialEq)]
pub struct PoolWeapon {
    pub weapon: String,
    /// Kit name prefixes (factions) whose kits carry or unlock it: `us`, `mec`, `sas`, ...
    pub factions: Vec<String>,
    /// Those whose kits carry it as their own (not as an unlock).
    pub native: Vec<String>,
    /// 0: some kit carries it as its own; otherwise the lowest unlock level that gives it.
    pub unlock: u32,
}

/// Why a pick was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The server only allows BF2's kits.
    ClassicKits,
    UnknownClass,
    /// Not a weapon of the class's pool.
    NotInPool,
    /// Faction-locked server: no kit of the player's team carries it.
    OtherFaction,
    /// An unlock weapon that needs this account rank.
    Locked { rank: u32 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ClassicKits => write!(f, "the server uses BF2's kits only"),
            Refusal::UnknownClass => write!(f, "no such kit class"),
            Refusal::NotInPool => write!(f, "not a weapon of that class"),
            Refusal::OtherFaction => write!(f, "weapons are restricted to the team's factions"),
            Refusal::Locked { rank } => write!(f, "unlocks at rank {rank}"),
        }
    }
}

/// The weapon pools of every kit class, from every kit on disk (the imported ones and the
/// mods').
#[derive(Resource, Default, Clone, Debug)]
pub struct Arsenal {
    /// Primary weapons by class (lowercase `kind`).
    pub classes: BTreeMap<String, Vec<PoolWeapon>>,
    /// Pistols, for every class.
    pub sidearms: Vec<PoolWeapon>,
    /// Hand grenades that explode, by faction: what an assault kit gets instead of the
    /// grenade launcher of a rifle it no longer carries (as BF2's unlocks do).
    pub frags: Vec<(String, String)>,
    /// The grenade launcher mounted under a rifle, by rifle: the two come together in a kit
    /// or an unlock (`usrif_m203` and `usrgl_m203`).
    pub launchers: BTreeMap<String, String>,
}

/// The faction a kit belongs to: its name's prefix (`us_assault` -> `us`,
/// `mecsf_assault_special` -> `mecsf`).
pub fn kit_faction(kit: &str) -> &str {
    kit.split_once('_').map_or(kit, |(prefix, _)| prefix)
}

/// A weapon name without its prefix (`usrif_m203` -> `m203`): a grenade launcher is
/// mounted on the rifle of the same name (`usrgl_m203`, `rurif_gp30`/`rurgl_gp30`).
fn base_name(weapon: &str) -> &str {
    weapon.split_once('_').map_or(weapon, |(_, rest)| rest)
}

/// A grenade launcher mounted under a rifle: inventory slot 4, a gun whose rounds explode,
/// without a sight of its own (rocket launchers have one).
fn is_under_barrel(weapon: &WeaponDesc) -> bool {
    weapon.slot == 4
        && weapon.fire.kind == FireKind::Gun
        && weapon.projectile.explodes()
        && weapon.zoom.mesh_1p.is_none()
}

/// A kit's primary weapon slot (BF2 `itemIndex` 3) and sidearm (2).
fn pick_slot(weapon: &WeaponDesc) -> Option<PickSlot> {
    if !weapon.selectable() || weapon.fire.kind != FireKind::Gun {
        return None;
    }
    match weapon.slot {
        3 => Some(PickSlot::Primary),
        2 => Some(PickSlot::Sidearm),
        _ => None,
    }
}

impl Arsenal {
    /// Builds the pools from `kits` (by name), looking weapons up with `weapon`.
    pub fn build<'a>(
        kits: impl IntoIterator<Item = &'a KitDesc>,
        weapon: impl Fn(&str) -> Option<Arc<WeaponDesc>>,
    ) -> Self {
        // (slot, class) -> weapon -> (factions, native factions, lowest unlock level)
        let mut found: BTreeMap<(Option<String>, String), (BTreeSet<String>, BTreeSet<String>, u32)> = BTreeMap::new();
        let mut frags = BTreeSet::new();
        let mut launchers = BTreeMap::new();
        for kit in kits {
            // A rifle and a launcher given together go together.
            for list in std::iter::once(&kit.weapons).chain(kit.unlocks.iter().map(|u| &u.weapons)) {
                let descs: Vec<(&String, Arc<WeaponDesc>)> =
                    list.iter().filter_map(|name| Some((name, weapon(name)?))).collect();
                let rifles: Vec<&String> =
                    descs.iter().filter(|(_, d)| pick_slot(d) == Some(PickSlot::Primary)).map(|(n, _)| *n).collect();
                let under: Vec<&String> = descs.iter().filter(|(_, d)| is_under_barrel(d)).map(|(n, _)| *n).collect();
                if let ([rifle], [launcher]) = (rifles.as_slice(), under.as_slice()) {
                    launchers.entry((*rifle).clone()).or_insert_with(|| (*launcher).clone());
                }
            }
            let class = kit.kind.to_ascii_lowercase();
            let faction = kit_faction(&kit.name).to_string();
            let own = kit.weapons.iter().map(|w| (w, 0));
            let unlocked = kit.unlocks.iter().flat_map(|u| u.weapons.iter().map(move |w| (w, u.level.max(1))));
            for (name, level) in own.chain(unlocked) {
                let Some(desc) = weapon(name) else {
                    continue;
                };
                if desc.is_hand_grenade() && desc.projectile.explodes() {
                    frags.insert((faction.clone(), name.clone()));
                }
                let key = match pick_slot(&desc) {
                    Some(PickSlot::Primary) => (Some(class.clone()), name.clone()),
                    Some(PickSlot::Sidearm) => (None, name.clone()),
                    None => continue,
                };
                let entry = found.entry(key).or_insert_with(|| (BTreeSet::new(), BTreeSet::new(), level));
                entry.0.insert(faction.clone());
                if level == 0 {
                    entry.1.insert(faction.clone());
                }
                entry.2 = entry.2.min(level);
            }
        }
        let mut arsenal = Arsenal {
            frags: frags.into_iter().collect(),
            launchers,
            ..default()
        };
        for ((class, name), (factions, native, unlock)) in found {
            let entry = PoolWeapon {
                weapon: name,
                factions: factions.into_iter().collect(),
                native: native.into_iter().collect(),
                unlock,
            };
            match class {
                Some(class) => arsenal.classes.entry(class).or_default().push(entry),
                None => arsenal.sidearms.push(entry),
            }
        }
        arsenal
    }

    /// The pool a pick of `class` draws from.
    pub fn pool(&self, class: &str, slot: PickSlot) -> Option<&[PoolWeapon]> {
        match slot {
            PickSlot::Primary => self.classes.get(&class.to_ascii_lowercase()).map(Vec::as_slice),
            PickSlot::Sidearm => Some(&self.sidearms),
        }
    }

    /// Whether a player on a team with kits of `factions` and account rank `rank` may carry
    /// `weapon` in `slot` of a `class` kit.
    pub fn check(
        &self,
        rules: &LoadoutRules,
        class: &str,
        slot: PickSlot,
        weapon: &str,
        factions: &[String],
        rank: Option<u32>,
    ) -> Result<&PoolWeapon, Refusal> {
        if !rules.arsenal {
            return Err(Refusal::ClassicKits);
        }
        if !self.classes.contains_key(&class.to_ascii_lowercase()) {
            return Err(Refusal::UnknownClass);
        }
        let entry = self
            .pool(class, slot)
            .and_then(|pool| pool.iter().find(|p| p.weapon == weapon))
            .ok_or(Refusal::NotInPool)?;
        if rules.faction_locked && !entry.factions.iter().any(|f| factions.contains(f)) {
            return Err(Refusal::OtherFaction);
        }
        if let Some(need) = required_rank(rules, entry.unlock)
            && rank.is_none_or(|rank| rank < need)
        {
            return Err(Refusal::Locked { rank: need });
        }
        Ok(entry)
    }

    /// The weapons a soldier of `kit` carries with `pick` (already checked): the kit's own,
    /// with its primary (and the grenade launcher under it) and its sidearm swapped for the
    /// picked ones. A rifle with a launcher of its own brings it along; an assault kit that
    /// loses its launcher gets a hand grenade instead, as BF2's unlocks do.
    pub fn kit_weapons(&self, kit: &KitDesc, pick: &ClassPick, armory: &Armory) -> Vec<String> {
        let mut weapons = kit.weapons.clone();
        let desc = |name: &str| armory.weapon(name).cloned();
        if let Some(primary) = pick.primary.as_deref().filter(|p| desc(p).is_some())
            && let Some(index) = weapons
                .iter()
                .position(|w| desc(w).is_some_and(|d| pick_slot(&d) == Some(PickSlot::Primary)))
            && weapons[index] != primary
        {
            let old = weapons[index].clone();
            let mounted_on = |rifle: &str, name: &str| {
                let paired = self.launchers.get(rifle).is_some_and(|l| l == name) || base_name(name) == base_name(rifle);
                paired && desc(name).is_some_and(|d| is_under_barrel(&d))
            };
            let had_launcher = weapons.iter().any(|w| mounted_on(&old, w));
            weapons.retain(|w| !mounted_on(&old, w));
            let index = weapons.iter().position(|w| *w == old).unwrap_or(0).min(weapons.len().saturating_sub(1));
            weapons[index] = primary.to_string();
            // A launcher of its own, from any kit or unlock (`usrif_m203` -> `usrgl_m203`).
            let launcher = self.launchers.get(primary).filter(|l| desc(l).is_some()).cloned().or_else(|| {
                armory
                    .all_weapons()
                    .filter(|(name, d)| base_name(name) == base_name(primary) && is_under_barrel(d))
                    .map(|(name, _)| name.clone())
                    .min()
            });
            match launcher {
                Some(launcher) => weapons.insert(index + 1, launcher),
                None if had_launcher => {
                    let has_frag =
                        weapons.iter().any(|w| desc(w).is_some_and(|d| d.is_hand_grenade() && d.projectile.explodes()));
                    let faction = kit_faction(&kit.name);
                    let frag = self
                        .frags
                        .iter()
                        .find(|(f, _)| f == faction)
                        .or(self.frags.first())
                        .map(|(_, w)| w.clone());
                    if let (false, Some(frag)) = (has_frag, frag) {
                        weapons.insert(index + 1, frag);
                    }
                }
                None => {}
            }
        }
        if let Some(sidearm) = pick.sidearm.as_deref().filter(|p| desc(p).is_some())
            && let Some(slot) = weapons
                .iter_mut()
                .find(|w| desc(w).is_some_and(|d| pick_slot(&d) == Some(PickSlot::Sidearm)))
        {
            *slot = sidearm.to_string();
        }
        weapons
    }
}

/// The account rank an unlock level needs, if any.
pub fn required_rank(rules: &LoadoutRules, unlock: u32) -> Option<u32> {
    if unlock == 0 || rules.unlock_ranks.is_empty() {
        return None;
    }
    let index = (unlock as usize - 1).min(rules.unlock_ranks.len() - 1);
    Some(rules.unlock_ranks[index])
}

/// The factions of a team's kits in the current level (`Armory::team_kits`).
pub fn team_factions(armory: &Armory, team: usize) -> Vec<String> {
    let mut factions: Vec<String> = armory
        .team_kits
        .get(team)
        .into_iter()
        .flatten()
        .map(|kit| kit_faction(kit).to_string())
        .collect();
    factions.sort();
    factions.dedup();
    factions
}

/// The content folders whose kits make the pools: `imported` (BF2, Special Forces, the
/// booster packs) and the **active** mods. A mod is active when the level being played comes
/// from it (AIX 2's weapons on AIX 2's levels only), or when it has no levels of its own (a
/// mod that changes the game everywhere). Server and clients decide alike: they play the
/// same level. Highest priority first, like [`GamePaths::roots`].
pub fn active_roots(paths: &GamePaths, level: &LoadedLevel) -> Vec<PathBuf> {
    let level_dir = level.dir.as_deref();
    paths
        .mods
        .iter()
        .filter(|m| {
            let has_levels = std::fs::read_dir(m.dir.join("levels")).is_ok_and(|mut d| d.next().is_some());
            !has_levels || level_dir.is_some_and(|dir| dir.starts_with(&m.dir))
        })
        .map(|m| m.dir.clone())
        .chain([paths.imported.clone()])
        .collect()
}

/// Every kit file (`kits/<name>.ron`) in `roots`, by name.
fn kit_names(roots: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = roots
        .iter()
        .flat_map(|root| std::fs::read_dir(root.join("kits")).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".ron")?;
            let stem = stem.strip_suffix(".patch").unwrap_or(stem);
            Some(stem.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Reads every kit and the weapons they and their unlocks carry (once per set of content
/// folders), lends the weapons to the [`Armory`] (`Armory::pool`) and builds the pools.
fn load_arsenal(
    paths: Res<GamePaths>,
    level: Res<LoadedLevel>,
    mut armory: ResMut<Armory>,
    mut arsenal: ResMut<Arsenal>,
    mut cache: Local<Option<(Vec<PathBuf>, Arsenal, HashMap<String, Arc<WeaponDesc>>)>>,
) {
    let roots = active_roots(&paths, &level);
    if cache.as_ref().is_none_or(|(cached, ..)| *cached != roots) {
        let started = std::time::Instant::now();
        let layers: Vec<&std::path::Path> = roots.iter().map(PathBuf::as_path).collect();
        fn read<T: Serialize + serde::de::DeserializeOwned>(layers: &[&std::path::Path], relative: String) -> anyhow::Result<T> {
            crate::mods::read_layered(layers, std::path::Path::new(&relative))
        }
        let kits: Vec<KitDesc> = kit_names(&roots)
            .iter()
            .filter_map(|name| read::<KitDesc>(&layers, format!("kits/{name}.ron")).ok())
            .collect();
        let mut weapons: HashMap<String, Arc<WeaponDesc>> = HashMap::new();
        let mut missing = BTreeSet::new();
        for kit in &kits {
            let names = kit.weapons.iter().chain(kit.unlocks.iter().flat_map(|u| &u.weapons));
            for name in names {
                if weapons.contains_key(name) || missing.contains(name) {
                    continue;
                }
                match read::<WeaponDesc>(&layers, format!("weapons/{name}.ron")) {
                    Ok(desc) => {
                        weapons.insert(name.clone(), Arc::new(desc));
                    }
                    Err(_) => {
                        missing.insert(name.clone());
                    }
                }
            }
        }
        let built = Arsenal::build(&kits, |name| weapons.get(name).cloned());
        let summary: Vec<String> = built.classes.iter().map(|(class, pool)| format!("{class} {}", pool.len())).collect();
        let mods: Vec<String> = paths
            .mods
            .iter()
            .filter(|m| roots.contains(&m.dir))
            .map(|m| m.info.name.clone())
            .collect();
        info!(
            "arsenal: {} kits (mods: {}), {} weapons; primaries per class: {}; {} sidearms ({:.0} ms)",
            kits.len(),
            if mods.is_empty() { "none".to_string() } else { mods.join(", ") },
            weapons.len(),
            summary.join(", "),
            built.sidearms.len(),
            started.elapsed().as_secs_f32() * 1000.0
        );
        if !missing.is_empty() {
            // Unlock weapons of kits imported before unlocks were (`bf2-import kits`).
            debug!("arsenal: weapons not imported: {}", missing.into_iter().collect::<Vec<_>>().join(", "));
        }
        *cache = Some((roots, built, weapons));
    }
    let Some((_, built, weapons)) = cache.as_ref() else {
        return;
    };
    *arsenal = built.clone();
    armory.pool = weapons.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_data::{KitUnlock, ProjectileDesc};

    fn weapon(name: &str, slot: u32, kind: FireKind, explodes: bool) -> Arc<WeaponDesc> {
        let mut desc: WeaponDesc = ron::from_str(&format!(
            "(name: \"{name}\", rounds_per_minute: 600.0, fire_modes: [Single], magazine_size: 30, magazines: 4, \
             reload_time: 2.0, deploy_time: 1.0, projectile: (velocity: 800.0, damage: 30.0, min_damage: 10.0, time_to_live: 1.0), \
             deviation: (min: 1.0, stand: 1.0, crouch: 1.0, prone: 1.0, zoom: 1.0, fire: (0,0,0), speed: (0,0,0,0), misc: (0,0,0)), \
             recoil: (up: (0,0), left_right: (0,0)))"
        ))
        .unwrap();
        desc.slot = slot;
        desc.fire.kind = kind;
        if explodes {
            desc.projectile = ProjectileDesc {
                explosion_damage: 100.0,
                explosion_radius: 5.0,
                impact: if kind == FireKind::Thrown { game_data::Impact::Bounce } else { game_data::Impact::Stop },
                ..desc.projectile
            };
        }
        Arc::new(desc)
    }

    fn setup() -> (Vec<KitDesc>, Armory) {
        let mut armory = Armory::default();
        for w in [
            weapon("usrif_m203", 3, FireKind::Gun, false),
            weapon("usrgl_m203", 4, FireKind::Gun, true),
            weapon("uspis_92fs", 2, FireKind::Gun, false),
            weapon("usrif_g3a3", 3, FireKind::Gun, false),
            weapon("ushgr_m67", 4, FireKind::Thrown, true),
            weapon("rurif_gp30", 3, FireKind::Gun, false),
            weapon("rurgl_gp30", 4, FireKind::Gun, true),
            weapon("rupis_baghira", 2, FireKind::Gun, false),
            weapon("usrif_m24", 3, FireKind::Gun, false),
            {
                // Rocket launchers have a sight of their own.
                let mut launcher = (*weapon("usatp_predator", 4, FireKind::Gun, true)).clone();
                launcher.zoom.mesh_1p = Some("sight.glb".into());
                Arc::new(launcher)
            },
            weapon("usrif_mp5_a3", 3, FireKind::Gun, false),
        ] {
            armory.weapons.insert(w.name.clone(), w);
        }
        let kit = |name: &str, kind: &str, weapons: &[&str]| KitDesc {
            name: name.into(),
            kind: kind.into(),
            weapons: weapons.iter().map(|w| w.to_string()).collect(),
            ..default()
        };
        let mut us = kit("us_assault", "Assault", &["usrif_m203", "usrgl_m203", "uspis_92fs"]);
        us.unlocks = vec![KitUnlock {
            level: 1,
            weapons: vec!["usrif_g3a3".into(), "ushgr_m67".into()],
            replaces: vec!["usrif_m203".into(), "usrgl_m203".into()],
        }];
        let kits = vec![
            us,
            kit("mec_assault", "Assault", &["rurif_gp30", "rurgl_gp30", "rupis_baghira"]),
            kit("us_sniper", "Sniper", &["usrif_m24", "uspis_92fs", "ushgr_m67"]),
            kit("us_at", "AT", &["usatp_predator", "usrif_mp5_a3", "uspis_92fs"]),
        ];
        (kits, armory)
    }

    #[test]
    fn pools_come_from_every_kit_of_a_class() {
        let (kits, armory) = setup();
        let arsenal = Arsenal::build(&kits, |w| armory.weapon(w).cloned());
        let assault: Vec<&str> = arsenal.classes["assault"].iter().map(|p| p.weapon.as_str()).collect();
        assert_eq!(assault, ["rurif_gp30", "usrif_g3a3", "usrif_m203"]);
        assert_eq!(arsenal.classes["assault"][1].unlock, 1);
        assert_eq!(arsenal.classes["assault"][0].factions, ["mec"]);
        // The AT kit's launcher stays a gadget; its submachine gun is the primary.
        let at: Vec<&str> = arsenal.classes["at"].iter().map(|p| p.weapon.as_str()).collect();
        assert_eq!(at, ["usrif_mp5_a3"]);
        assert_eq!(arsenal.sidearms.len(), 2);
    }

    #[test]
    fn picks_are_checked_against_the_rules() {
        let (kits, armory) = setup();
        let arsenal = Arsenal::build(&kits, |w| armory.weapon(w).cloned());
        let rules = LoadoutRules::default();
        let us = vec!["us".to_string()];
        assert!(arsenal.check(&rules, "Assault", PickSlot::Primary, "rurif_gp30", &us, None).is_ok());
        assert_eq!(
            arsenal.check(&rules, "assault", PickSlot::Primary, "usrif_m24", &us, None),
            Err(Refusal::NotInPool)
        );
        let locked = LoadoutRules { faction_locked: true, ..default() };
        assert_eq!(
            arsenal.check(&locked, "assault", PickSlot::Primary, "rurif_gp30", &us, None),
            Err(Refusal::OtherFaction)
        );
        let ranked = LoadoutRules { unlock_ranks: vec![3], ..default() };
        assert_eq!(
            arsenal.check(&ranked, "assault", PickSlot::Primary, "usrif_g3a3", &us, Some(1)),
            Err(Refusal::Locked { rank: 3 })
        );
        assert!(arsenal.check(&ranked, "assault", PickSlot::Primary, "usrif_g3a3", &us, Some(4)).is_ok());
        let classic = LoadoutRules { arsenal: false, ..default() };
        assert_eq!(
            arsenal.check(&classic, "assault", PickSlot::Primary, "usrif_m203", &us, None),
            Err(Refusal::ClassicKits)
        );
    }

    #[test]
    fn a_picked_rifle_brings_its_own_launcher() {
        let (kits, armory) = setup();
        let arsenal = Arsenal::build(&kits, |w| armory.weapon(w).cloned());
        let pick = ClassPick {
            primary: Some("rurif_gp30".into()),
            sidearm: Some("rupis_baghira".into()),
        };
        assert_eq!(arsenal.kit_weapons(&kits[0], &pick, &armory), ["rurif_gp30", "rurgl_gp30", "rupis_baghira"]);
        // Without a launcher of its own, a hand grenade instead.
        let pick = ClassPick {
            primary: Some("usrif_g3a3".into()),
            sidearm: None,
        };
        assert_eq!(arsenal.kit_weapons(&kits[0], &pick, &armory), ["usrif_g3a3", "ushgr_m67", "uspis_92fs"]);
        // Nothing picked: the kit as it is.
        assert_eq!(arsenal.kit_weapons(&kits[0], &ClassPick::default(), &armory), kits[0].weapons);
        // The anti-tank kit keeps its rocket launcher whatever gun it takes.
        let pick = ClassPick {
            primary: Some("usrif_g3a3".into()),
            sidearm: None,
        };
        assert_eq!(arsenal.kit_weapons(&kits[3], &pick, &armory), ["usatp_predator", "usrif_g3a3", "uspis_92fs"]);
    }
}
