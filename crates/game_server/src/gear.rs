//! Gas masks and tear gas on the server: puts [`SoldierGear`] on soldiers and sets it from
//! their players' [`GearRequest`]s, marks the smoke clouds of tear gas grenades
//! (`SmokeCloud::gas_damage`) with [`TearGas`], and hurts soldiers breathing it without a
//! mask.

use std::collections::HashSet;

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::WeaponEffectTable;
use game_shared::{
    config::GamePaths,
    gear::{GearRequest, SoldierGear, TearGas, gas_exposure},
    projectile::SmokeCloud,
    soldier::{Health, Soldier, SoldierMotion},
    weapons::Loadout,
};

use crate::{ClientPlayer, Controls, HostPlayer, combat::CombatSystems, sender_player};

pub struct GearPlugin;

impl Plugin for GearPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GearData>().add_systems(
            FixedUpdate,
            (equip_soldiers, receive_requests, mark_tear_gas, breathe_gas)
                .chain()
                .after(CombatSystems)
                .run_if(in_state(ClientState::Disconnected)),
        );
    }
}

/// The weapons that are gas masks, from the imported weapon effects.
#[derive(Resource, Default)]
struct GearData {
    loaded: bool,
    gas_masks: HashSet<String>,
}

impl GearData {
    fn load(&mut self, paths: &GamePaths) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let table: WeaponEffectTable =
            game_data::read_ron(paths.imported.join("effects/weapons.ron")).unwrap_or_default();
        self.gas_masks = table.weapons.into_iter().filter(|(_, w)| w.gas_mask).map(|(n, _)| n).collect();
    }
}

fn equip_soldiers(mut commands: Commands, soldiers: Query<Entity, (With<Soldier>, Without<SoldierGear>)>) {
    for soldier in &soldiers {
        commands.entity(soldier).insert(SoldierGear::default());
    }
}

fn receive_requests(
    mut requests: MessageReader<FromClient<GearRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    players: Query<&Controls>,
    mut soldiers: Query<(&Loadout, &mut SoldierGear)>,
    paths: Res<GamePaths>,
    mut data: ResMut<GearData>,
) {
    data.load(&paths);
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Some((loadout, mut gear)) = players.get(player).ok().and_then(|c| soldiers.get_mut(c.0).ok()) else {
            continue;
        };
        let has_mask = loadout.weapons.iter().any(|w| data.gas_masks.contains(w));
        let wanted = request.message.gas_mask && has_mask;
        if gear.gas_mask != wanted {
            gear.gas_mask = wanted;
        }
    }
}

/// Marks the smoke clouds of tear gas grenades.
fn mark_tear_gas(mut commands: Commands, clouds: Query<(Entity, &SmokeCloud), (Added<SmokeCloud>, Without<TearGas>)>) {
    for (entity, cloud) in &clouds {
        if cloud.gas_damage > 0.0 {
            commands.entity(entity).insert(TearGas {
                damage: cloud.gas_damage,
            });
        }
    }
}

/// Soldiers without a gas mask lose hit points in tear gas.
fn breathe_gas(
    time: Res<Time>,
    clouds: Query<(&SmokeCloud, &TearGas)>,
    mut soldiers: Query<(&SoldierMotion, &SoldierGear, &mut Health), With<Soldier>>,
) {
    if clouds.is_empty() {
        return;
    }
    let damage = clouds.iter().map(|(_, gas)| gas.damage).fold(0.0, f32::max);
    for (motion, gear, mut health) in &mut soldiers {
        if gear.gas_mask || health.current <= 0.0 {
            continue;
        }
        let exposure = gas_exposure(motion.eye_position(), &clouds);
        if exposure > 0.0 {
            health.current -= damage * exposure * time.delta_secs();
        }
    }
}
