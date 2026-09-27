//! What Special Forces gadgets do to bots: flashbangs blind those facing them (see
//! `BotBrain::sense`), tear gas degrades those without a gas mask, and bots that carry one
//! put it on when tear gas is about.

use bevy::{
    platform::collections::{HashMap, HashSet},
    prelude::*,
};
use bevy_replicon::prelude::*;
use game_data::{FlashbangDesc, WeaponEffectTable};
use game_shared::{
    effects::PlayEffect,
    gear::{SoldierGear, TearGas},
    projectile::SmokeCloud,
    soldier::SoldierMotion,
    weapons::Loadout,
};

use crate::{Controls, bots::BotBrain};

/// Tear gas this close to a bot (beyond the cloud's radius) and it puts its mask on; this
/// far away it takes it off again.
const MASK_ON: f32 = 8.0;
const MASK_OFF: f32 = 20.0;

/// Which weapons' detonations blind, and which weapons are gas masks: from the imported
/// weapon effects (`effects/weapons.ron`).
#[derive(Default)]
pub struct GadgetData {
    /// Flashbangs by detonation effect.
    pub flashbangs: HashMap<String, FlashbangDesc>,
    pub gas_masks: HashSet<String>,
}

impl GadgetData {
    pub fn from_table(table: &WeaponEffectTable) -> Self {
        let mut data = Self::default();
        for (name, weapon) in &table.weapons {
            if let (Some(effect), Some(flashbang)) = (&weapon.detonation, weapon.flashbang) {
                data.flashbangs.insert(effect.clone(), flashbang);
            }
            if weapon.gas_mask {
                data.gas_masks.insert(name.clone());
            }
        }
        data
    }
}

/// Flashbangs that went off since the bots last looked.
#[derive(Resource, Default)]
pub struct Flashes(pub Vec<(Vec3, FlashbangDesc)>);

/// Notes the flashbangs going off this tick, from the detonation effects sent to clients.
pub fn watch_detonations(
    data: Res<super::AiData>,
    mut effects: MessageReader<ToClients<PlayEffect>>,
    mut flashes: ResMut<Flashes>,
) {
    for effect in effects.read() {
        if let Some(desc) = data.gadgets.flashbangs.get(&effect.message.name) {
            debug!("ai: flashbang at {:.1}", effect.message.position);
            flashes.0.push((effect.message.position, *desc));
        }
    }
}

/// The bots have seen them.
pub fn forget_flashes(mut flashes: ResMut<Flashes>) {
    flashes.0.clear();
}

/// Bots with a gas mask wear it while tear gas is near.
pub fn wear_gas_masks(
    data: Res<super::AiData>,
    bots: Query<&Controls, With<BotBrain>>,
    mut soldiers: Query<(&SoldierMotion, &Loadout, &mut SoldierGear)>,
    clouds: Query<&SmokeCloud, With<TearGas>>,
) {
    if data.gadgets.gas_masks.is_empty() {
        return;
    }
    for controls in &bots {
        let Ok((motion, loadout, mut gear)) = soldiers.get_mut(controls.0) else {
            continue;
        };
        if !loadout.weapons.iter().any(|w| data.gadgets.gas_masks.contains(w)) {
            continue;
        }
        let eye = motion.eye_position();
        let gap = clouds
            .iter()
            .map(|c| eye.distance(c.position) - c.current_radius())
            .fold(f32::MAX, f32::min);
        let wear = if gear.gas_mask { gap < MASK_OFF } else { gap < MASK_ON };
        if gear.gas_mask != wear {
            gear.gas_mask = wear;
        }
    }
}

/// How blinded a soldier is by a flashbang of strength `strength` (0..1) `age` seconds ago:
/// blind (can't see or shoot) while the white-out is thick, dazed (aiming badly) while the
/// afterimage lasts.
pub fn flash_effect(desc: &FlashbangDesc, strength: f32, age: f32) -> (bool, bool) {
    let blind = desc.white.alpha_at(strength, age) > 0.5;
    let dazed = desc.afterimage.alpha_at(strength, age) > 0.3 || desc.glow.alpha_at(strength, age) > 0.3;
    (blind, dazed)
}

/// Strength (0..1) of a flashbang at `flash` for someone at `eye` looking along `forward`
/// (the client's rules, `game_client::gadgets`); `visible`: nothing solid in between.
pub fn flash_strength(desc: &FlashbangDesc, eye: Vec3, forward: Vec3, flash: Vec3, visible: bool) -> f32 {
    let distance = eye.distance(flash);
    if distance >= desc.radius || !visible {
        return 0.0;
    }
    let near = 1.0 - ((distance - desc.inner_radius) / (desc.radius - desc.inner_radius).max(0.1)).clamp(0.0, 1.0);
    let facing = (flash - eye).normalize_or_zero().dot(forward) >= (desc.view_cone.to_radians() * 0.5).cos();
    near * if facing { 1.0 } else { desc.unseen_strength }
}
