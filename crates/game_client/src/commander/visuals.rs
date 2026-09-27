//! What the commander's assets look and sound like: the supply crate floating down, and the
//! shells of an artillery strike whistling in just before they land (on everyone's side: the
//! enemy's only warning). The artillery pieces and the UAV are vehicles, drawn as such;
//! levels imported before that get a UAV model circling here and shells whistling on the
//! strike's schedule.

use bevy::prelude::*;
use game_data::{AssetKind, RemoteKind};
use game_shared::{
    commander::{ARTILLERY_DELAY, ARTILLERY_GUN_STAGGER, Asset, AssetEffect, CommanderAssets},
    level::LoadedLevel,
    projectile::{GRAVITY, Projectile, ProjectileMotion},
    statics::DestroyedStatics,
    vehicle::VehicleData,
    weapons::Armory,
};

use crate::audio::PlaySound;

pub struct VisualsPlugin;

impl Plugin for VisualsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (dress_effects, drop_crates, fly_uavs, whistle, whistle_shells));
    }
}

/// The whistle starts this long before its shell lands [our choice].
const WHISTLE_LEAD: f32 = 1.2;

/// An artillery shell whose whistle has played.
#[derive(Component)]
struct Whistled;

#[derive(Component)]
struct CrateModel;

#[derive(Component)]
struct UavModel {
    angle: f32,
}

/// Shells of a strike still to come in: seconds until each whistle, and where.
#[derive(Component)]
struct Incoming(Vec<(f32, Vec3)>);

fn model(asset_server: &AssetServer, mesh: &Option<String>) -> Option<WorldAssetRoot> {
    let mesh = mesh.as_ref()?;
    Some(WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("imported://{mesh}")))))
}

/// Gives new asset effects their model or their sounds.
#[allow(clippy::too_many_arguments)]
fn dress_effects(
    mut commands: Commands,
    effects: Query<(Entity, &AssetEffect), Added<AssetEffect>>,
    assets: Res<CommanderAssets>,
    destroyed: Query<&DestroyedStatics>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let desc = &assets.desc;
    for (entity, effect) in &effects {
        match effect.asset {
            Asset::Supply => {
                let transform = Transform::from_translation(effect.position);
                let mut crate_entity = commands.entity(entity);
                crate_entity.insert((CrateModel, transform, Visibility::default()));
                match model(&asset_server, &desc.supply.mesh) {
                    Some(scene) => {
                        crate_entity.with_child(scene);
                    }
                    None => {
                        crate_entity.with_child((
                            Mesh3d(meshes.add(Cuboid::new(1.2, 1.0, 1.2))),
                            MeshMaterial3d(materials.add(Color::srgb(0.32, 0.36, 0.22))),
                            Transform::from_xyz(0.0, 0.5, 0.0),
                        ));
                    }
                }
            }
            // It flies as a vehicle.
            Asset::Uav if desc.uav.vehicle.is_some() => {}
            Asset::Uav => {
                let angle = fastrand::f32() * std::f32::consts::TAU;
                let mut uav = commands.entity(entity);
                uav.insert((UavModel { angle }, Transform::from_translation(effect.position), Visibility::default()));
                if let Some(scene) = model(&asset_server, &desc.uav.mesh) {
                    uav.with_child(scene);
                }
            }
            Asset::Artillery => {
                let artillery = &desc.artillery;
                // Real shells whistle by themselves (see `whistle_shells`).
                if artillery.incoming.is_none() || assets.of(effect.team, AssetKind::Artillery).any(|a| a.vehicle) {
                    continue;
                }
                let destroyed = destroyed.single().ok();
                let guns = assets
                    .of(effect.team, AssetKind::Artillery)
                    .filter(|g| !destroyed.is_some_and(|d| d.0.contains(&g.instance)))
                    .count()
                    .max(1);
                let mut shells = Vec::new();
                for gun in 0..guns {
                    for shell in 0..artillery.shells {
                        let offset = Vec2::from_angle(fastrand::f32() * std::f32::consts::TAU)
                            * artillery.spread
                            * fastrand::f32().sqrt();
                        let when = ARTILLERY_DELAY + shell as f32 * artillery.interval + gun as f32 * ARTILLERY_GUN_STAGGER;
                        shells.push(((when - WHISTLE_LEAD).max(0.0), effect.position + Vec3::new(offset.x, 12.0, offset.y)));
                    }
                }
                commands.entity(entity).insert(Incoming(shells));
            }
            Asset::Scan => {}
        }
    }
}

/// Supply crates follow the server down, smoothly.
fn drop_crates(time: Res<Time>, mut crates: Query<(&AssetEffect, &mut Transform), With<CrateModel>>) {
    let blend = 1.0 - (-10.0 * time.delta_secs()).exp();
    for (effect, mut transform) in &mut crates {
        transform.translation = transform.translation.lerp(effect.position, blend);
    }
}

/// UAVs circle their target, banking into the turn.
fn fly_uavs(
    time: Res<Time>,
    assets: Res<CommanderAssets>,
    mut uavs: Query<(&AssetEffect, &mut UavModel, &mut Transform)>,
) {
    let desc = &assets.desc.uav;
    let radius = desc.radius.max(10.0);
    let rate = desc.speed.max(1.0) / radius;
    for (effect, mut uav, mut transform) in &mut uavs {
        uav.angle = (uav.angle + rate * time.delta_secs()) % std::f32::consts::TAU;
        let (sin, cos) = uav.angle.sin_cos();
        transform.translation = effect.position + Vec3::new(cos * radius, desc.height, sin * radius);
        // Flying counter-clockwise seen from above: heading along the tangent.
        let tangent = Vec3::new(-sin, 0.0, cos);
        transform.rotation = Transform::IDENTITY.looking_to(tangent, Vec3::Y).rotation * Quat::from_rotation_z(0.35);
    }
}

/// Plays each incoming shell's whistle.
fn whistle(
    mut commands: Commands,
    time: Res<Time>,
    assets: Res<CommanderAssets>,
    mut strikes: Query<(Entity, &mut Incoming)>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let dt = time.delta_secs();
    let Some(sound) = &assets.desc.artillery.incoming else {
        return;
    };
    for (entity, mut incoming) in &mut strikes {
        incoming.0.retain_mut(|(left, at)| {
            *left -= dt;
            if *left > 0.0 {
                return true;
            }
            sounds.write(PlaySound::at(sound.clone(), *at).reason("artillery"));
            false
        });
        if incoming.0.is_empty() {
            commands.entity(entity).remove::<Incoming>();
        }
    }
}

/// Shells fired by artillery pieces whistle once they are about to come down.
#[allow(clippy::too_many_arguments)]
fn whistle_shells(
    mut commands: Commands,
    assets: Res<CommanderAssets>,
    level: Option<Res<LoadedLevel>>,
    armory: Res<Armory>,
    guns: Query<&VehicleData>,
    shells: Query<(Entity, &Projectile, &ProjectileMotion), Without<Whistled>>,
    mut sounds: MessageWriter<PlaySound>,
    mut names: Local<Vec<String>>,
) {
    let (Some(sound), Some(level)) = (&assets.desc.artillery.incoming, level) else {
        return;
    };
    names.clear();
    for data in &guns {
        if data.0.desc.remote == Some(RemoteKind::Artillery) {
            names.extend(data.0.guns.iter().map(|g| g.name.clone()));
        }
    }
    if names.is_empty() {
        return;
    }
    for (entity, projectile, motion) in &shells {
        if !names.contains(&projectile.weapon) || motion.velocity.y >= 0.0 {
            continue;
        }
        let gravity = GRAVITY * armory.weapon(&projectile.weapon).map_or(1.0, |w| w.projectile.gravity).max(0.1);
        let ground = level.heightmap.as_ref().map_or(0.0, |h| h.height_at(motion.position.x, motion.position.z));
        let fall = (motion.position.y - ground).max(0.0);
        let down = -motion.velocity.y;
        // When it reaches the ground: fall = down t + g t^2 / 2.
        let arrival = (-down + (down * down + 2.0 * gravity * fall).sqrt()) / gravity;
        if arrival > WHISTLE_LEAD {
            continue;
        }
        let at = motion.position + Vec3::new(motion.velocity.x, 0.0, motion.velocity.z) * arrival;
        sounds.write(PlaySound::at(sound.clone(), Vec3::new(at.x, ground, at.z)).reason("artillery"));
        commands.entity(entity).insert(Whistled);
    }
}
