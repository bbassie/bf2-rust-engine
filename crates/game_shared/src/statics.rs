//! Static level objects: one entity per object part, with collision where the template
//! has it. Visuals are added by the client (see [`StaticMesh`]).
//!
//! Destroyable objects are destroyed by the server, which lists them in the replicated
//! [`DestroyedStatics`]. Both sides then swap the object's parts for its wreck (or take them
//! away), so movement prediction sees the same world as the server.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
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

/// The level statics that are destroyed, by index, on the match entity. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DestroyedStatics(pub BTreeSet<u32>);

/// A destroyable object was destroyed just now (not: found destroyed on joining).
#[derive(Message, Clone, Copy, Debug)]
pub struct StaticDestroyed {
    pub instance: u32,
}

/// Loads templates and colliders once per level load.
struct Cache<'a> {
    root: &'a Path,
    templates: HashMap<String, Option<ObjectDesc>>,
    colliders: HashMap<(String, u32), Option<Collider>>,
}

impl Cache<'_> {
    fn template(&mut self, name: &str) -> Option<&ObjectDesc> {
        let root = self.root;
        self.templates
            .entry(name.to_string())
            .or_insert_with(|| {
                let path = root.join("templates").join(format!("{name}.ron"));
                game_data::read_ron(&path)
                    .map_err(|err| warn!("{err}"))
                    .ok()
            })
            .as_ref()
    }

    fn collider(&mut self, path: &str, part: u32) -> Option<Collider> {
        let root = self.root;
        self.colliders
            .entry((path.to_string(), part))
            .or_insert_with(|| {
                load_collider(&root.join(path), part)
                    .map_err(|err| warn!("collision {path}: {err:#}"))
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
    let root: PathBuf = paths.imported.clone();
    let mut cache = Cache {
        root: &root,
        templates: HashMap::new(),
        colliders: HashMap::new(),
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
                    entity.insert(StaticMesh {
                        path: mesh.clone(),
                        index: part.mesh_index,
                    });
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
            };
            spawn(part.mesh.as_ref(), part.collision.as_ref(), false);
            if armor.is_some() && (part.wreck_mesh.is_some() || part.wreck_collision.is_some()) {
                spawn(part.wreck_mesh.as_ref(), part.wreck_collision.as_ref(), true);
            }
            spawned += 1;
        }
    }
    info!(
        "spawned {spawned} static parts from {} templates ({} colliders, {destroyable} destroyable objects)",
        cache.templates.len(),
        cache.colliders.values().filter(|c| c.is_some()).count()
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

/// Takes destroyed objects away (or shows their wreck) and brings restored ones back.
fn apply_destroyed_statics(
    mut commands: Commands,
    destroyed: Query<Ref<DestroyedStatics>>,
    parts: Query<(Entity, &Destructible, Has<Inactive>)>,
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
    for (entity, part, inactive) in &parts {
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
        entity.insert(collision_layers(present));
        if live && is_destroyed {
            just_destroyed.insert(part.instance);
        }
    }
    news.write_batch(just_destroyed.into_iter().map(|instance| StaticDestroyed { instance }));
}

/// Reads a collision `.glb` written by the importer and builds a triangle mesh collider for
/// one part. Prefers the soldier mesh (it includes stairs and interiors), then vehicle, then
/// projectile.
fn load_collider(path: &Path, part: u32) -> anyhow::Result<Option<Collider>> {
    let bytes = std::fs::read(path)?;
    let gltf = gltf::Gltf::from_slice(&bytes)?;
    let blob = gltf.blob.as_deref().unwrap_or_default();

    let preference = ["soldier", "vehicle", "projectile"];
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
        if let Some(read) = reader.read_indices() {
            let flat: Vec<u32> = read.into_u32().collect();
            indices.extend(
                flat.chunks_exact(3)
                    .map(|t| [base + t[0], base + t[1], base + t[2]]),
            );
        }
    }
    if vertices.is_empty() || indices.is_empty() {
        return Ok(None);
    }
    Ok(Some(Collider::trimesh(vertices, indices)))
}
