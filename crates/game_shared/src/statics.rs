//! Static level objects: one entity per object part, with collision where the template
//! has it. Visuals are added by the client (see [`StaticMesh`]).
//!
//! Destroyable objects are destroyed by the server, which lists them in the replicated
//! [`DestroyedStatics`]. Both sides then swap the object's parts for its wreck (or take them
//! away), so movement prediction sees the same world as the server.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};

use avian3d::prelude::*;
use bevy::prelude::*;
use game_data::{ArmorDesc, ObjectDesc, StaticInstance};
use serde::{Deserialize, Serialize};

use crate::{
    config::GamePaths,
    level::{LevelEntity, placement_transform},
    physics::GameLayer,
};

/// Applies [`DestroyedStatics`] on client and server. Added by the server's and the client's
/// plugins, whichever comes first.
pub struct StaticsPlugin;

impl Plugin for StaticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<StaticDestroyed>()
            .add_systems(FixedUpdate, apply_destroyed_statics);
    }
}

/// A visual mesh to draw for this entity: mesh `index` of the `.glb` at `path` (relative to
/// the imported assets root). The server ignores it; the client renders it.
#[derive(Component, Clone, Debug)]
pub struct StaticMesh {
    pub path: String,
    pub index: u32,
}

/// Lower-detail meshes of a [`StaticMesh`] (the same `index` in each) and how far away it
/// is drawn at all, from the object template. The server ignores it; the client picks the
/// mesh by camera distance.
#[derive(Component, Clone, Debug, Default)]
pub struct StaticMeshLods {
    pub lods: Vec<game_data::MeshLod>,
    /// Beyond this distance (m) from the camera the object isn't drawn.
    pub draw_distance: Option<f32>,
}

/// A part of a destroyable object.
#[derive(Component, Clone, Debug)]
pub struct Destructible {
    /// The object's index in the level's statics.
    pub instance: u32,
    pub armor: Arc<ArmorDesc>,
    /// Damage table column for direct hits on this part.
    pub hit_material: u32,
    /// This part is the wreck that replaces the intact parts once the object is destroyed.
    pub wreck: bool,
}

/// A destroyable part that isn't there right now: an intact part of a destroyed object, or
/// the wreck of an intact one. Its collider is disabled and the client hides it.
#[derive(Component, Debug)]
pub struct Inactive;

/// Marks the vehicle-only sibling body a destroyable part's [`Destructible`] entity gets
/// alongside it when BF2 gives that part vehicle collision (see `vehicle_collision_layers`):
/// tells [`apply_destroyed_statics`] to restore it on [`GameLayer::VehicleGround`] rather than
/// [`GameLayer::World`].
#[derive(Component, Debug)]
struct VehicleGroundPart;

/// The level statics that are destroyed, by index, on the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DestroyedStatics(pub BTreeSet<u32>);

/// A destroyable object was destroyed just now (not: found destroyed on joining).
#[derive(Message, Clone, Copy, Debug)]
pub struct StaticDestroyed {
    pub instance: u32,
}

/// Preference order for the [`GameLayer::World`] collider: BF2's most detailed mesh a part
/// has, in order (soldier includes stairs and interiors; vehicle is hull only; projectile is
/// the most detailed but only meant for bullets). Soldiers, projectiles, the camera,
/// footsteps and the infantry nav grid all collide with whichever this picks.
const WORLD_PREFERENCE: &[&str] = &["soldier", "vehicle", "projectile"];

/// Loads templates and colliders once per level load.
struct Cache<'a> {
    paths: &'a GamePaths,
    templates: HashMap<String, Option<ObjectDesc>>,
    colliders: HashMap<(String, u32), Option<Collider>>,
    /// The [`GameLayer::VehicleGround`] collider, built only from BF2's vehicle-type
    /// (col type 1) mesh: `None` when a part has no vehicle collision at all (small plants),
    /// so vehicles pass through it instead of colliding with whatever `colliders` picked.
    vehicle_colliders: HashMap<(String, u32), Option<Collider>>,
}

impl Cache<'_> {
    fn template(&mut self, name: &str) -> Option<&ObjectDesc> {
        let paths = self.paths;
        self.templates
            .entry(name.to_string())
            .or_insert_with(|| {
                paths
                    .read_ron(format!("templates/{name}.ron"))
                    .map_err(|err| warn!("{err:#}"))
                    .ok()
            })
            .as_ref()
    }

    fn collider(&mut self, path: &str, part: u32) -> Option<Collider> {
        let paths = self.paths;
        self.colliders
            .entry((path.to_string(), part))
            .or_insert_with(|| {
                load_collider(&paths.find(path), part, WORLD_PREFERENCE)
                    .map_err(|err| warn!("collision {path}: {err:#}"))
                    .ok()
                    .flatten()
            })
            .clone()
    }

    /// The vehicle-only collider for a part, if BF2 gives it one (see
    /// [`Cache::vehicle_colliders`]).
    fn vehicle_collider(&mut self, path: &str, part: u32) -> Option<Collider> {
        let paths = self.paths;
        self.vehicle_colliders
            .entry((path.to_string(), part))
            .or_insert_with(|| {
                load_collider(&paths.find(path), part, &["vehicle"])
                    .map_err(|err| warn!("vehicle collision {path}: {err:#}"))
                    .ok()
                    .flatten()
            })
            .clone()
    }
}

pub fn spawn_statics(commands: &mut Commands, statics: &[StaticInstance], paths: &GamePaths) {
    spawn_objects(commands, statics, 0, paths);
}

/// Spawns objects like [`spawn_statics`], numbering them from `first_instance` (for
/// [`DestroyedStatics`]): objects outside the level's statics, such as the commander's
/// assets, use numbers above the statics'.
pub fn spawn_objects(commands: &mut Commands, statics: &[StaticInstance], first_instance: u32, paths: &GamePaths) {
    let mut cache = Cache {
        paths,
        templates: HashMap::new(),
        colliders: HashMap::new(),
        vehicle_colliders: HashMap::new(),
    };
    let (mut spawned, mut destroyable) = (0, 0);
    for (instance, placed) in statics.iter().enumerate() {
        let instance = first_instance as usize + instance;
        let Some(object) = cache.template(&placed.template).cloned() else {
            continue;
        };
        let armor = object.armor.map(Arc::new);
        destroyable += usize::from(armor.is_some());
        let base = placement_transform(&placed.placement);
        for part in &object.parts {
            let transform = base * placement_transform(&part.placement);
            let mut spawn = |mesh: Option<&String>, collision: Option<&String>, wreck: bool| {
                let mut entity = commands.spawn((LevelEntity, transform));
                if let Some(mesh) = mesh {
                    let lods = if wreck { &part.wreck_lods } else { &part.lods };
                    entity.insert((
                        StaticMesh {
                            path: mesh.clone(),
                            index: part.mesh_index,
                        },
                        StaticMeshLods {
                            lods: lods.clone(),
                            draw_distance: object.draw_distance,
                        },
                    ));
                }
                if let Some(collision) = collision
                    && let Some(collider) = cache.collider(collision, part.collision_part)
                {
                    entity.insert((RigidBody::Static, collider, collision_layers(!wreck)));
                    if part.ladder && !wreck {
                        entity.insert(crate::ladder::LadderPart);
                    }
                }
                if let Some(armor) = &armor {
                    entity.insert(Destructible {
                        instance: instance as u32,
                        armor: armor.clone(),
                        hit_material: part.hit_material.unwrap_or(armor.material),
                        wreck,
                    });
                    if wreck {
                        entity.insert((Inactive, ColliderDisabled));
                    }
                }
                // A second, independent static body for vehicles (see `GameLayer::VehicleGround`):
                // BF2's vehicle (hull-only) collision, when this part has any. It is separate
                // from the entity above so a part with no vehicle mesh (most small plants) gets
                // no vehicle collision at all, even though it still blocks soldiers or stops
                // bullets there; a part with vehicle collision (trees, walls, solid props)
                // blocks vehicles exactly like it does today.
                if let Some(collision) = collision
                    && let Some(collider) = cache.vehicle_collider(collision, part.collision_part)
                {
                    let mut vehicle_entity = commands.spawn((
                        LevelEntity,
                        transform,
                        RigidBody::Static,
                        collider,
                        vehicle_collision_layers(!wreck),
                    ));
                    if let Some(armor) = &armor {
                        vehicle_entity.insert(VehicleGroundPart);
                        vehicle_entity.insert(Destructible {
                            instance: instance as u32,
                            armor: armor.clone(),
                            hit_material: part.hit_material.unwrap_or(armor.material),
                            wreck,
                        });
                        if wreck {
                            vehicle_entity.insert((Inactive, ColliderDisabled));
                        }
                    }
                }
            };
            spawn(part.mesh.as_ref(), part.collision.as_ref(), false);
            if armor.is_some() && (part.wreck_mesh.is_some() || part.wreck_collision.is_some()) {
                spawn(part.wreck_mesh.as_ref(), part.wreck_collision.as_ref(), true);
            }
            spawned += 1;
        }
    }
    info!(
        "spawned {spawned} static parts from {} templates ({} colliders, {} vehicle-only colliders, {destroyable} destroyable objects)",
        cache.templates.len(),
        cache.colliders.values().filter(|c| c.is_some()).count(),
        cache.vehicle_colliders.values().filter(|c| c.is_some()).count()
    );
}

/// Static geometry collides with everything; parts that aren't there with nothing (the
/// navigation grid skips them too).
fn collision_layers(present: bool) -> CollisionLayers {
    if present {
        CollisionLayers::new(GameLayer::World, LayerMask::ALL)
    } else {
        CollisionLayers::NONE
    }
}

/// The vehicle-only sibling collider's layers (see [`GameLayer::VehicleGround`]): present
/// when BF2 gives this part vehicle collision and it's actually there right now.
fn vehicle_collision_layers(present: bool) -> CollisionLayers {
    if present {
        CollisionLayers::new(GameLayer::VehicleGround, LayerMask::ALL)
    } else {
        CollisionLayers::NONE
    }
}

/// Takes destroyed objects away (or shows their wreck) and brings restored ones back.
fn apply_destroyed_statics(
    mut commands: Commands,
    destroyed: Query<Ref<DestroyedStatics>>,
    parts: Query<(Entity, &Destructible, Has<Inactive>, Has<VehicleGroundPart>)>,
    new_parts: Query<(), Added<Destructible>>,
    mut news: MessageWriter<StaticDestroyed>,
) {
    let destroyed = destroyed.single().ok();
    if !destroyed.as_ref().is_some_and(Ref::is_changed) && new_parts.is_empty() {
        return;
    }
    // Changes during the match are news; the state found when joining or loading is not.
    let live = destroyed.as_ref().is_some_and(|d| !d.is_added()) && new_parts.is_empty();
    let none = BTreeSet::new();
    let destroyed = destroyed.as_ref().map_or(&none, |d| &d.0);
    let mut just_destroyed = BTreeSet::new();
    for (entity, part, inactive, vehicle_ground) in &parts {
        let is_destroyed = destroyed.contains(&part.instance);
        let present = is_destroyed == part.wreck;
        if present != inactive {
            continue;
        }
        let mut entity = commands.entity(entity);
        if present {
            entity.remove::<(Inactive, ColliderDisabled)>();
        } else {
            entity.insert((Inactive, ColliderDisabled));
        }
        // The vehicle-only sibling body (see `GameLayer::VehicleGround`) is restored on its
        // own layer, not the main entity's `GameLayer::World`.
        entity.insert(if vehicle_ground {
            vehicle_collision_layers(present)
        } else {
            collision_layers(present)
        });
        if live && is_destroyed {
            just_destroyed.insert(part.instance);
        }
    }
    news.write_batch(just_destroyed.into_iter().map(|instance| StaticDestroyed { instance }));
}

/// Reads a collision `.glb` written by the importer and builds a triangle mesh collider for
/// one part: the first of `preference` (`part{N}_{kind}`) the part actually has, or `None` if
/// it has none of them. [`WORLD_PREFERENCE`] falls back through BF2's meshes in detail order;
/// `["vehicle"]` (see [`Cache::vehicle_collider`]) takes only the exact vehicle-type mesh, with
/// no fallback, so a part BF2 gives no vehicle collision gets none from us either.
fn load_collider(path: &Path, part: u32, preference: &[&str]) -> anyhow::Result<Option<Collider>> {
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let blob = gltf.blob.as_deref().unwrap_or_default();

    let mesh = preference.iter().find_map(|kind| {
        let name = format!("part{part}_{kind}");
        gltf.meshes().find(|m| m.name() == Some(name.as_str()))
    });
    let Some(mesh) = mesh else {
        return Ok(None);
    };

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for primitive in mesh.primitives() {
        let reader = primitive.reader(|_| Some(blob));
        let base = vertices.len() as u32;
        let Some(positions) = reader.read_positions() else {
            continue;
        };
        vertices.extend(positions.map(Vec3::from_array));
        // Checked, not trusted: the mesh may come from a server (see `content`).
        let count = vertices.len() as u32 - base;
        anyhow::ensure!(vertices.iter().all(|v| v.is_finite()), "{}: vertices that aren't numbers", path.display());
        if let Some(read) = reader.read_indices() {
            let flat: Vec<u32> = read.into_u32().collect();
            indices.extend(
                flat.chunks_exact(3)
                    .filter(|t| t.iter().all(|&i| i < count))
                    .map(|t| [base + t[0], base + t[1], base + t[2]]),
            );
        }
    }
    if vertices.is_empty() || indices.is_empty() {
        return Ok(None);
    }
    Collider::try_trimesh(vertices, indices)
        .map(Some)
        .map_err(|err| anyhow::anyhow!("{}: {err:?}", path.display()))
}
