//! Destroyable level objects: hit points, damage through the material table, and what
//! happens at 0. A destroyed object is added to the match's [`DestroyedStatics`], which
//! server and clients apply alike (see `game_shared::statics`); explosive ones (fuel barrels,
//! fuel tanks) blow up. Everything comes back when the next round starts.

use std::collections::{HashMap, hash_map::Entry};

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::MaterialTable;
use game_shared::{
    config::GamePaths,
    conquest::RoundState,
    protocol::MatchInfo,
    statics::{Destructible, DestroyedStatics, Inactive, StaticsPlugin},
};

use crate::combat::{Attacker, CombatSystems, Explosion, StaticHit};

pub struct DestructionPlugin;

impl Plugin for DestructionPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<StaticsPlugin>() {
            app.add_plugins(StaticsPlugin);
        }
        app.init_resource::<ObjectHealth>()
            .add_systems(Startup, load_materials)
            .add_systems(
                FixedUpdate,
                (track_match, restore_on_new_round, damage_statics)
                    .chain()
                    .after(CombatSystems)
                    .run_if(in_state(ClientState::Disconnected)),
            );
    }
}

/// The damage table. Empty (everything deals full damage) without imported data.
#[derive(Resource, Default)]
pub struct Materials(pub MaterialTable);

/// Hit points left per damaged object, by level statics index.
#[derive(Resource, Default)]
struct ObjectHealth(HashMap<u32, f32>);

fn load_materials(mut commands: Commands, paths: Res<GamePaths>) {
    let path = paths.imported.join("materials.ron");
    let table = match game_data::read_ron::<MaterialTable>(&path) {
        Ok(table) => {
            info!("damage table: {} materials, {} factors", table.names.len(), table.damage.len());
            table
        }
        Err(err) => {
            warn!("{err}; every hit deals full damage");
            MaterialTable::default()
        }
    };
    commands.insert_resource(Materials(table));
}

/// Gives the match the (replicated) list of destroyed objects.
fn track_match(
    mut commands: Commands,
    matches: Query<Entity, (With<MatchInfo>, Without<DestroyedStatics>)>,
    mut health: ResMut<ObjectHealth>,
) {
    for entity in &matches {
        commands.entity(entity).insert(DestroyedStatics::default());
        health.0.clear();
    }
}

/// A new round rebuilds everything.
fn restore_on_new_round(
    mut matches: Query<(Ref<RoundState>, &mut DestroyedStatics)>,
    mut health: ResMut<ObjectHealth>,
    mut round_over: Local<bool>,
) {
    for (round, mut destroyed) in &mut matches {
        let over = matches!(*round, RoundState::Ended { .. });
        if round.is_changed() && !over && *round_over {
            if !destroyed.0.is_empty() {
                info!("new round: restoring {} destroyed objects", destroyed.0.len());
                destroyed.0.clear();
            }
            health.0.clear();
        }
        *round_over = over;
    }
}

/// Direct hits use the part's surface material, explosions the object's armor material,
/// each against the attacker's material in the damage table. Explosions fall off linearly
/// with the distance to the nearest point of the object.
#[allow(clippy::too_many_arguments)]
fn damage_statics(
    mut commands: Commands,
    materials: Option<Res<Materials>>,
    mut health: ResMut<ObjectHealth>,
    mut hits: MessageReader<StaticHit>,
    mut explosions: MessageReader<Explosion>,
    parts: Query<(&Destructible, &Transform, Option<&ColliderAabb>), Without<Inactive>>,
    mut destroyed: Query<&mut DestroyedStatics>,
) {
    let (hits, explosions): (Vec<_>, Vec<_>) = (hits.read().collect(), explosions.read().collect());
    let (Ok(mut destroyed), Some(materials)) = (destroyed.single_mut(), materials) else {
        return;
    };
    if hits.is_empty() && explosions.is_empty() {
        return;
    }
    let table = &materials.0;

    // Damage per object this tick, and who dealt the last of it.
    let mut damage: HashMap<u32, (f32, &Attacker)> = HashMap::new();
    for hit in &hits {
        let Ok((part, ..)) = parts.get(hit.part) else {
            continue;
        };
        if part.wreck {
            continue;
        }
        let factor = table.damage_mod(hit.material, part.hit_material);
        debug!(
            "object {} hit by {} ({}) on {}: {} x {factor}",
            part.instance,
            hit.attacker.weapon,
            table.name(hit.material),
            table.name(part.hit_material),
            hit.damage
        );
        add_damage(&mut damage, part.instance, hit.damage * factor, &hit.attacker);
    }
    if !explosions.is_empty() {
        // Nearest distance from each explosion to each intact object.
        let mut nearest: HashMap<(usize, u32), (f32, &Destructible)> = HashMap::new();
        for (part, transform, aabb) in &parts {
            if part.wreck {
                continue;
            }
            for (index, explosion) in explosions.iter().enumerate() {
                let closest = aabb.map_or(transform.translation, |aabb| {
                    explosion.position.clamp(aabb.min, aabb.max)
                });
                if !explosion.reaches(closest) {
                    continue;
                }
                let distance = closest.distance(explosion.position);
                match nearest.entry((index, part.instance)) {
                    Entry::Occupied(mut e) if distance < e.get().0 => *e.get_mut() = (distance, part),
                    Entry::Occupied(_) => {}
                    Entry::Vacant(e) => {
                        e.insert((distance, part));
                    }
                }
            }
        }
        for ((index, instance), (distance, part)) in nearest {
            let explosion = explosions[index];
            if distance >= explosion.radius {
                continue;
            }
            let factor = table.damage_mod(explosion.material, part.armor.material);
            let amount = explosion.damage * (1.0 - distance / explosion.radius) * factor;
            debug!(
                "object {instance} in a blast ({}) {distance:.1} m away: {amount:.1}",
                table.name(explosion.material)
            );
            add_damage(&mut damage, instance, amount, &explosion.attacker);
        }
    }

    for (instance, (amount, attacker)) in damage {
        if destroyed.0.contains(&instance) {
            continue;
        }
        let Some((part, transform, aabb)) = parts.iter().find(|(p, ..)| p.instance == instance && !p.wreck)
        else {
            continue;
        };
        let armor = &part.armor;
        let left = match health.0.entry(instance) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(armor.hit_points),
        };
        *left -= amount;
        info!("object {instance} took {amount:.1} damage from {}, {:.1} left", attacker.weapon, left.max(0.0));
        if *left > 0.0 {
            continue;
        }
        destroyed.0.insert(instance);
        let center = aabb.map_or(transform.translation, |aabb| (aabb.min + aabb.max) * 0.5);
        info!("object {instance} destroyed at {center}");
        if let Some(blast) = &armor.explosion {
            commands.write_message(Explosion {
                position: center,
                damage: blast.damage,
                radius: blast.radius,
                material: blast.material,
                attacker: Attacker {
                    weapon: "explosion".into(),
                    ..attacker.clone()
                },
                cone: None,
            });
        }
    }
}

fn add_damage<'a>(damage: &mut HashMap<u32, (f32, &'a Attacker)>, instance: u32, amount: f32, attacker: &'a Attacker) {
    if amount > 0.0 {
        let entry = damage.entry(instance).or_insert((0.0, attacker));
        *entry = (entry.0 + amount, attacker);
    }
}

#[cfg(test)]
mod tests {
    use game_shared::protocol::Team;

    use super::*;

    #[test]
    fn new_round_restores_destroyed_objects() {
        let mut world = World::new();
        world.init_resource::<ObjectHealth>();
        let game = world.spawn((RoundState::Playing, DestroyedStatics::default())).id();
        let system = world.register_system(restore_on_new_round);
        world.run_system(system).unwrap();

        world.get_mut::<DestroyedStatics>(game).unwrap().0.insert(7);
        world.resource_mut::<ObjectHealth>().0.insert(3, 10.0);
        world.run_system(system).unwrap();
        assert!(world.get::<DestroyedStatics>(game).unwrap().0.contains(&7));

        let ended = RoundState::Ended { winner: Team::One, restart_in: 20.0 };
        world.entity_mut(game).insert(ended);
        world.run_system(system).unwrap();
        assert!(world.get::<DestroyedStatics>(game).unwrap().0.contains(&7));

        world.entity_mut(game).insert(RoundState::Playing);
        world.run_system(system).unwrap();
        assert!(world.get::<DestroyedStatics>(game).unwrap().0.is_empty());
        assert!(world.resource::<ObjectHealth>().0.is_empty());
    }
}
