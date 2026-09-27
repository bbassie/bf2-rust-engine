//! What the commander's assets look and sound like: the UAV circling over its target, the
//! supply crate floating down, and the shells of an artillery strike whistling in just
//! before they land (on everyone's side: the enemy's only warning).

use bevy::prelude::*;
use game_data::AssetKind;
use game_shared::{
    commander::{ARTILLERY_DELAY, Asset, AssetEffect, CommanderAssets},
    statics::DestroyedStatics,
};

use crate::audio::PlaySound;

pub struct VisualsPlugin;

impl Plugin for VisualsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (dress_effects, drop_crates, fly_uavs, whistle));
    }
}

/// The whistle starts this long before its shell lands [our choice].
const WHISTLE_LEAD: f32 = 1.2;
/// Seconds between the guns' first shells, as the server fires them.
const GUN_STAGGER: f32 = 0.4;

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
                if artillery.incoming.is_none() {
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
                        let when = ARTILLERY_DELAY + shell as f32 * artillery.interval + gun as f32 * GUN_STAGGER;
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
