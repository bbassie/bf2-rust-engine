//! Destroyed objects: hides what's gone and plays the destruction effect: the meshes BF2's
//! effect throws (planks, sign posts, wreck bits), a few dust puffs, a fireball for
//! explosive objects, and the sound.

use avian3d::prelude::*;
use bevy::{gltf::Gltf, prelude::*};
use game_data::Spread;
use game_shared::{
    physics::GameLayer,
    statics::{Destructible, Inactive, StaticDestroyed, StaticMesh, StaticsPlugin},
};

pub struct DestructionRenderPlugin;

impl Plugin for DestructionRenderPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<StaticsPlugin>() {
            app.add_plugins(StaticsPlugin);
        }
        app.init_resource::<PreloadedDebris>()
            .add_systems(Startup, create_assets)
            .add_systems(
                Update,
                (preload_debris, hide_inactive, spawn_effects, update_debris, update_puffs),
            );
    }
}

const GRAVITY: f32 = 9.81;
/// Debris shrinks away over its last this many seconds.
const SHRINK_TIME: f32 = 0.4;

#[derive(Resource)]
struct EffectAssets {
    puff: Handle<Mesh>,
}

/// Dust and smoke are lit and blended, fire glows (additive, unlit).
const DUST: (Color, bool) = (Color::srgba(0.6, 0.55, 0.46, 0.45), false);
const SMOKE: (Color, bool) = (Color::srgba(0.1, 0.09, 0.08, 0.6), false);
const FIRE: (Color, bool) = (Color::srgba(1.0, 0.5, 0.12, 0.9), true);

/// Debris meshes of the level's destroyable objects, loaded before anything breaks so the
/// pieces show up at once.
#[derive(Resource, Default)]
struct PreloadedDebris(Vec<Handle<Gltf>>);

/// A thrown piece. It bounces off the world and comes to rest.
#[derive(Component)]
struct Debris {
    velocity: Vec3,
    /// Angular velocity, world space.
    spin: Vec3,
    life: f32,
    age: f32,
    scale: Vec3,
}

/// Dust, smoke or fire: a sphere that grows, drifts and fades (with a material of its own).
#[derive(Component)]
struct Puff {
    velocity: Vec3,
    size: f32,
    life: f32,
    age: f32,
    material: Handle<StandardMaterial>,
    alpha: f32,
}

/// The flash of an explosion.
#[derive(Component)]
struct Flash {
    intensity: f32,
}

fn create_assets(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    commands.insert_resource(EffectAssets {
        puff: meshes.add(Sphere::new(0.5).mesh().ico(2).unwrap()),
    });
}

fn preload_debris(
    parts: Query<&Destructible, Added<Destructible>>,
    asset_server: Res<AssetServer>,
    mut preloaded: ResMut<PreloadedDebris>,
) {
    for part in &parts {
        for piece in &part.armor.effect.debris {
            let handle = asset_server.load(format!("imported://{}", piece.mesh));
            if !preloaded.0.contains(&handle) {
                preloaded.0.push(handle);
            }
        }
    }
}

fn hide_inactive(mut parts: Query<(&mut Visibility, Has<Inactive>), With<Destructible>>) {
    for (mut visibility, inactive) in &mut parts {
        let wanted = if inactive { Visibility::Hidden } else { Visibility::Inherited };
        visibility.set_if_neq(wanted);
    }
}

fn spawn_effects(
    mut commands: Commands,
    mut destroyed: MessageReader<StaticDestroyed>,
    parts: Query<(&Destructible, &Transform, Option<&ColliderAabb>)>,
    assets: Res<EffectAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut sounds: MessageWriter<crate::audio::PlaySound>,
) {
    for event in destroyed.read() {
        let Some((part, transform, aabb)) = parts
            .iter()
            .find(|(p, ..)| p.instance == event.instance && !p.wreck)
        else {
            continue;
        };
        let effect = &part.armor.effect;
        let (center, extent) = aabb.map_or((transform.translation, Vec3::splat(0.5)), |aabb| {
            ((aabb.min + aabb.max) * 0.5, (aabb.max - aabb.min) * 0.5)
        });
        let random = || fastrand::f32();
        let flip = || fastrand::bool();
        let sample = |spread: &[Spread; 3]| {
            transform.rotation * Vec3::from_array(std::array::from_fn(|i| spread[i].sample(random(), flip())))
        };

        for piece in &effect.debris {
            commands.spawn((
                Transform {
                    translation: transform.transform_point(Vec3::from_array(piece.position)),
                    ..*transform
                },
                StaticMesh {
                    path: piece.mesh.clone(),
                    index: piece.mesh_index,
                },
                Debris {
                    velocity: sample(&piece.velocity),
                    spin: sample(&piece.spin),
                    life: piece.life.sample(random(), false),
                    age: 0.0,
                    scale: transform.scale,
                },
            ));
        }

        let size = extent.length().clamp(0.4, 3.0);
        let mut puff = |(color, glow): (Color, bool), velocity: Vec3, size: f32, life: f32| {
            let offset = (Vec3::new(random(), random(), random()) - 0.5) * extent;
            let material = materials.add(StandardMaterial {
                base_color: color,
                alpha_mode: if glow { AlphaMode::Add } else { AlphaMode::Blend },
                unlit: glow,
                perceptual_roughness: 1.0,
                reflectance: 0.0,
                ..default()
            });
            commands.spawn((
                Puff {
                    velocity,
                    size,
                    life,
                    age: 0.0,
                    material: material.clone(),
                    alpha: color.alpha(),
                },
                Mesh3d(assets.puff.clone()),
                MeshMaterial3d(material),
                Transform::from_translation(center + offset).with_scale(Vec3::splat(0.01)),
                bevy::light::NotShadowCaster,
            ));
        };
        if effect.dust || effect.debris.is_empty() {
            for _ in 0..8 {
                let out = Vec3::new(random() - 0.5, random() * 0.6, random() - 0.5) * 2.0;
                puff(DUST, out, size * (0.6 + random() * 0.6), 1.5 + random());
            }
        }
        if let Some(explosion) = &part.armor.explosion {
            let fireball = explosion.radius.clamp(2.0, 6.0) * 0.5;
            for _ in 0..6 {
                let out = Vec3::new(random() - 0.5, random() * 0.8, random() - 0.5) * 3.0;
                puff(FIRE, out, fireball * (0.6 + random() * 0.5), 0.5 + random() * 0.3);
            }
            for _ in 0..8 {
                let rise = Vec3::new(random() - 0.5, 1.5 + random() * 2.0, random() - 0.5);
                puff(SMOKE, rise, fireball * (1.0 + random()), 3.0 + random() * 2.0);
            }
            commands.spawn((
                Flash { intensity: 2.0e7 },
                PointLight {
                    color: Color::srgb(1.0, 0.7, 0.35),
                    intensity: 2.0e7,
                    range: explosion.radius * 4.0,
                    ..default()
                },
                Transform::from_translation(center + Vec3::Y),
            ));
        }
        for sound in &effect.sounds {
            sounds.write(crate::audio::PlaySound::at(sound, center).reason("destruction"));
        }
    }
}

fn update_debris(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    mut debris: Query<(Entity, &mut Debris, &mut Transform)>,
) {
    let dt = time.delta_secs();
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    for (entity, mut piece, mut transform) in &mut debris {
        piece.age += dt;
        if piece.age >= piece.life {
            commands.entity(entity).despawn();
            continue;
        }
        piece.velocity.y -= GRAVITY * dt;
        let step = piece.velocity * dt;
        let hit = Dir3::new(step)
            .ok()
            .and_then(|dir| spatial.cast_ray(transform.translation, dir, step.length(), true, &filter));
        match hit {
            Some(hit) => {
                transform.translation += step.normalize() * hit.distance + hit.normal * 0.02;
                piece.velocity = piece.velocity.reflect(hit.normal) * 0.3;
                piece.spin *= 0.5;
            }
            None => transform.translation += step,
        }
        transform.rotation = (Quat::from_scaled_axis(piece.spin * dt) * transform.rotation).normalize();
        let shrink = ((piece.life - piece.age) / SHRINK_TIME).clamp(0.0, 1.0);
        transform.scale = piece.scale * shrink;
    }
}

fn update_puffs(
    mut commands: Commands,
    time: Res<Time>,
    mut puffs: Query<(Entity, &mut Puff, &mut Transform)>,
    mut flashes: Query<(Entity, &mut Flash, &mut PointLight)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    for (entity, mut puff, mut transform) in &mut puffs {
        puff.age += dt;
        if puff.age >= puff.life {
            materials.remove(&puff.material);
            commands.entity(entity).despawn();
            continue;
        }
        let t = puff.age / puff.life;
        // Quick growth, then a slow drift while it fades.
        transform.scale = Vec3::splat(puff.size * (t * 6.0).clamp(0.01, 1.0) * (1.0 + t));
        if let Some(mut material) = materials.get_mut(&puff.material) {
            material.base_color.set_alpha(puff.alpha * (1.0 - t) * (1.0 - t));
        }
        transform.translation += puff.velocity * dt;
        puff.velocity *= 1.0 - (2.0 * dt).min(1.0);
    }
    for (entity, mut flash, mut light) in &mut flashes {
        flash.intensity *= 1.0 - (10.0 * dt).min(1.0);
        light.intensity = flash.intensity;
        if flash.intensity < 1.0e4 {
            commands.entity(entity).despawn();
        }
    }
}
