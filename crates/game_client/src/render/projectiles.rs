//! Grenades, rockets and charges: their models in flight and where they lie, our own throws
//! shown before the server has them, and the view inside smoke.
//!
//! Each projectile the server replicates gets a separate visual that runs the shared flight
//! rules (`game_shared::projectile::step`) between server updates and blends out the
//! difference when one arrives. Our own launches start as predicted visuals that the
//! server's projectile takes over once it shows up.

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::{gltf::GltfAssetLabel, light::NotShadowCaster, prelude::*};
use game_data::{Impact, WeaponDesc};
use game_shared::{
    projectile::{self, Projectile, ProjectileMotion, Smoke, collision_layers},
    soldier::Hitbox,
    weapons::Armory,
};

use crate::{
    camera::PlayerCamera,
    combat::LocalLaunch,
    effects::EffectEmitter,
    net::{LocalPlayer, LocalSoldier},
};

pub struct ProjectileRenderPlugin;

impl Plugin for ProjectileRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (create_assets, spawn_smoke_overlay))
            .add_systems(
                Update,
                (spawn_predicted, track_projectiles, move_visuals, smoke_overlay).chain(),
            );
    }
}

/// A predicted projectile the server never confirmed is dropped after this long.
const UNCONFIRMED: f32 = 1.0;
/// Seconds over which a correction from the server blends out.
const BLEND: f32 = 0.08;

/// What a projectile looks like (and where): its source is the replicated projectile, or
/// none while it is our prediction.
#[derive(Component)]
struct ProjectileVisual {
    source: Option<Entity>,
    weapon: Arc<WeaponDesc>,
    /// Simulated between server updates.
    motion: ProjectileMotion,
    /// Seconds since launch.
    age: f32,
    /// The rest of the last correction, blended out over [`BLEND`].
    offset: Vec3,
    /// Tumbling in flight: axis times radians per second.
    spin: Vec3,
    /// Stopped against something, waiting for the server to say what happened.
    stopped: bool,
}

#[derive(Resource)]
struct ProjectileAssets {
    fallback: Handle<Mesh>,
    fallback_material: Handle<StandardMaterial>,
}

fn create_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(ProjectileAssets {
        fallback: meshes.add(Capsule3d::new(0.035, 0.08)),
        fallback_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.2, 0.24, 0.16),
            perceptual_roughness: 0.8,
            ..default()
        }),
    });
}

fn spawn_visual(
    commands: &mut Commands,
    asset_server: &AssetServer,
    assets: &ProjectileAssets,
    visual: ProjectileVisual,
) -> Entity {
    let transform = Transform::from_translation(visual.motion.position).with_rotation(visual.motion.rotation);
    let mesh = visual.weapon.projectile.mesh.clone();
    let trail = visual.weapon.projectile.trail_effect.clone();
    let mut entity = commands.spawn((visual, transform, GlobalTransform::from(transform), Visibility::default()));
    match mesh {
        Some(mesh) => {
            entity.with_child(WorldAssetRoot(
                asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("imported://{mesh}"))),
            ));
        }
        None => {
            entity.with_child((
                Mesh3d(assets.fallback.clone()),
                MeshMaterial3d(assets.fallback_material.clone()),
                NotShadowCaster,
            ));
        }
    }
    if let Some(trail) = trail {
        entity.insert(EffectEmitter(trail));
    }
    entity.id()
}

fn random_spin(desc: &game_data::ProjectileDesc) -> Vec3 {
    match desc.impact {
        Impact::Stop => Vec3::ZERO,
        _ => Vec3::new(fastrand::f32() - 0.5, fastrand::f32() - 0.5, fastrand::f32() - 0.5).normalize_or(Vec3::X) * 12.0,
    }
}

/// Our own throws and launches, at once.
fn spawn_predicted(
    mut commands: Commands,
    mut launches: MessageReader<LocalLaunch>,
    asset_server: Res<AssetServer>,
    assets: Res<ProjectileAssets>,
) {
    for launch in launches.read() {
        let visual = ProjectileVisual {
            source: None,
            spin: random_spin(&launch.weapon.projectile),
            weapon: launch.weapon.clone(),
            motion: ProjectileMotion::new(launch.origin, launch.velocity, launch.yaw),
            age: 0.0,
            offset: Vec3::ZERO,
            stopped: false,
        };
        spawn_visual(&mut commands, &asset_server, &assets, visual);
    }
}

/// Gives every new replicated projectile a visual: ours take over the matching prediction.
#[allow(clippy::type_complexity)]
fn track_projectiles(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    assets: Res<ProjectileAssets>,
    added: Query<(Entity, &Projectile, &ProjectileMotion), Added<Projectile>>,
    local_player: Query<Entity, With<LocalPlayer>>,
    mut visuals: Query<(&mut ProjectileVisual, &Transform)>,
) {
    let me = local_player.single().ok();
    for (source, projectile, motion) in &added {
        let Some(weapon) = armory.weapon(&projectile.weapon) else {
            continue;
        };
        if Some(projectile.player) == me {
            let prediction = visuals
                .iter_mut()
                .filter(|(v, _)| v.source.is_none() && v.weapon.name == projectile.weapon)
                .max_by(|(a, _), (b, _)| a.age.total_cmp(&b.age));
            if let Some((mut visual, transform)) = prediction {
                visual.source = Some(source);
                visual.offset = transform.translation - motion.position;
                visual.motion = *motion;
                visual.stopped = false;
                continue;
            }
        }
        let visual = ProjectileVisual {
            source: Some(source),
            spin: random_spin(&weapon.projectile),
            weapon: weapon.clone(),
            motion: *motion,
            age: 0.0,
            offset: Vec3::ZERO,
            stopped: false,
        };
        spawn_visual(&mut commands, &asset_server, &assets, visual);
    }
}

fn move_visuals(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    sources: Query<Ref<ProjectileMotion>>,
    local_hitbox: Query<&Hitbox, With<LocalSoldier>>,
    mut visuals: Query<(Entity, &mut ProjectileVisual, &mut Transform, &mut Visibility)>,
) {
    let dt = time.delta_secs();
    let hitbox = local_hitbox.single().ok().map(|h| h.entity);
    for (entity, mut visual, mut transform, mut visibility) in &mut visuals {
        let visual = &mut *visual;
        visual.age += dt;
        let desc = &visual.weapon.projectile;
        let updated = match visual.source {
            Some(source) => match sources.get(source) {
                Ok(server) => server.is_changed().then_some(*server),
                // Gone on the server: it went off, or was picked up.
                Err(_) => {
                    commands.entity(entity).despawn();
                    continue;
                }
            },
            None if visual.age > UNCONFIRMED => {
                commands.entity(entity).despawn();
                continue;
            }
            None => None,
        };
        match updated {
            Some(server) => {
                let shown = visual.motion.position + visual.offset;
                visual.motion = server;
                visual.offset = shown - server.position;
                visual.stopped = false;
            }
            None if !visual.stopped => {
                let mut filter = SpatialQueryFilter::from_mask(collision_layers(desc));
                if let Some(hitbox) = hitbox.filter(|_| visual.source.is_none()) {
                    filter = filter.with_excluded_entities([hitbox]);
                }
                let step = projectile::step(&spatial, &filter, desc, &mut visual.motion, visual.age, dt);
                visual.stopped = step.hit.is_some();
            }
            None => {}
        }
        visual.offset *= (-dt / BLEND).exp();

        // A prediction that hit something waits hidden for the server's detonation.
        let hidden = visual.stopped && visual.source.is_none();
        visibility.set_if_neq(if hidden { Visibility::Hidden } else { Visibility::Inherited });
        transform.translation = visual.motion.position + visual.offset;
        let motion = &visual.motion;
        transform.rotation = if motion.resting {
            motion.rotation
        } else if desc.impact == Impact::Stop {
            // Shells and rockets point along their flight.
            Transform::default().looking_to(motion.velocity.normalize_or(motion.rotation * Vec3::NEG_Z), Vec3::Y).rotation
        } else {
            motion.rotation * Quat::from_scaled_axis(visual.spin * visual.age)
        };
    }
}

/// A gray veil over the screen while the camera is inside smoke, thicker further in.
#[derive(Component)]
struct SmokeOverlay;

fn spawn_smoke_overlay(mut commands: Commands) {
    commands.spawn((
        SmokeOverlay,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
        BackgroundColor(Color::NONE),
        // Under the HUD.
        GlobalZIndex(-1),
    ));
}

fn smoke_overlay(
    smoke: Smoke,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    mut overlay: Query<&mut BackgroundColor, With<SmokeOverlay>>,
) {
    let density = camera.single().map_or(0.0, |c| smoke.density_at(c.translation()));
    let color = Color::srgba(0.66, 0.66, 0.64, density.powf(0.6) * 0.97);
    for mut background in &mut overlay {
        if background.0 != color {
            background.0 = color;
        }
    }
}
