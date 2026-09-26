//! Static level objects: one entity per object part, with collision where the template
//! has it. Visuals are added by the client (see [`StaticMesh`]).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use avian3d::prelude::*;
use bevy::prelude::*;
use game_data::{ObjectDesc, StaticInstance};

use crate::{
    config::GamePaths,
    level::{LevelEntity, placement_transform},
    physics::GameLayer,
};

/// A visual mesh to draw for this entity: mesh `index` of the `.glb` at `path` (relative to
/// the imported assets root). The server ignores it; the client renders it.
#[derive(Component, Clone, Debug)]
pub struct StaticMesh {
    pub path: String,
    pub index: u32,
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
    let root: PathBuf = paths.imported.clone();
    let mut cache = Cache {
        root: &root,
        templates: HashMap::new(),
        colliders: HashMap::new(),
    };
    let mut spawned = 0;
    for instance in statics {
        let Some(object) = cache.template(&instance.template).cloned() else {
            continue;
        };
        let base = placement_transform(&instance.placement);
        for part in &object.parts {
            let transform = base * placement_transform(&part.placement);
            let mut entity = commands.spawn((LevelEntity, transform));
            if let Some(mesh) = &part.mesh {
                entity.insert(StaticMesh {
                    path: mesh.clone(),
                    index: part.mesh_index,
                });
            }
            if let Some(collision) = &part.collision
                && let Some(collider) = cache.collider(collision, part.collision_part)
            {
                entity.insert((
                    RigidBody::Static,
                    collider,
                    CollisionLayers::new(GameLayer::World, LayerMask::ALL),
                ));
            }
            spawned += 1;
        }
    }
    info!(
        "spawned {spawned} static parts from {} templates ({} colliders)",
        cache.templates.len(),
        cache.colliders.values().filter(|c| c.is_some()).count()
    );
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
