//! Draws static level objects: loads the glTF mesh named by [`StaticMesh`] and spawns its
//! primitives as children once loaded, with BF2 layered materials.

use bevy::{
    asset::LoadState,
    gltf::{GltfAssetLabel, GltfMaterial, GltfMesh, GltfPrimitive},
    platform::collections::HashMap,
    prelude::*,
};
use game_shared::statics::StaticMesh;

use super::materials::{StaticLayers, StaticMaterial};

pub struct StaticRenderPlugin;

impl Plugin for StaticRenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MaterialCache>()
            .add_observer(request_mesh)
            .add_systems(Update, spawn_loaded_meshes);
    }
}

#[derive(Component)]
struct PendingMesh(Handle<GltfMesh>);

/// One layered material per glTF material, shared by every object using it.
#[derive(Resource, Default)]
struct MaterialCache(HashMap<AssetId<GltfMaterial>, Handle<StaticMaterial>>);

fn request_mesh(
    add: On<Add, StaticMesh>,
    meshes: Query<&StaticMesh>,
    asset_server: Res<AssetServer>,
    mut commands: Commands,
) {
    let Ok(mesh) = meshes.get(add.entity) else {
        return;
    };
    let handle = asset_server.load(
        GltfAssetLabel::Mesh(mesh.index as usize).from_asset(format!("imported://{}", mesh.path)),
    );
    commands
        .entity(add.entity)
        .insert((PendingMesh(handle), Visibility::default()));
}

fn spawn_loaded_meshes(
    mut commands: Commands,
    pending: Query<(Entity, &PendingMesh)>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    standard: Res<Assets<StandardMaterial>>,
    mut layered: ResMut<Assets<StaticMaterial>>,
    mut cache: ResMut<MaterialCache>,
    asset_server: Res<AssetServer>,
) {
    'entities: for (entity, PendingMesh(handle)) in &pending {
        if let LoadState::Failed(err) = asset_server.load_state(handle) {
            warn!("static mesh failed to load: {err}");
            commands.entity(entity).remove::<PendingMesh>();
            continue;
        }
        let Some(mesh) = gltf_meshes.get(handle) else {
            continue;
        };
        let mut materials = Vec::with_capacity(mesh.primitives.len());
        for primitive in &mesh.primitives {
            match layered_material(primitive, &asset_server, &standard, &mut layered, &mut cache) {
                Some(material) => materials.push(material),
                // The glTF's own materials aren't ready yet; try again next frame.
                None => continue 'entities,
            }
        }
        for (primitive, material) in mesh.primitives.iter().zip(materials) {
            commands
                .entity(entity)
                .with_child((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material)));
        }
        commands.entity(entity).remove::<PendingMesh>();
    }
}

/// Builds (or reuses) the layered material for a primitive. `None` while its glTF material
/// is still loading.
fn layered_material(
    primitive: &GltfPrimitive,
    asset_server: &AssetServer,
    standard: &Assets<StandardMaterial>,
    layered: &mut Assets<StaticMaterial>,
    cache: &mut MaterialCache,
) -> Option<Handle<StaticMaterial>> {
    let Some(gltf_material) = &primitive.material else {
        return Some(layered.add(StaticMaterial::default()));
    };
    if let Some(done) = cache.0.get(&gltf_material.id()) {
        return Some(done.clone());
    }

    // Bevy's glTF loader stores a StandardMaterial next to every glTF material.
    let path = gltf_material.path()?;
    let std_path = path.clone().with_label(format!("{}/std", path.label()?));
    let base = standard.get(&asset_server.load::<StandardMaterial>(std_path))?.clone();

    let mut extension = StaticLayers {
        detail_scale: 2.0,
        ..default()
    };
    if let Some(bf2) = primitive
        .material_extras
        .as_ref()
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.value).ok())
        .map(|v| v["bf2"].clone())
    {
        let technique = bf2["technique"].as_str().unwrap_or_default().to_ascii_lowercase();
        // Static mesh maps follow the technique layers: Base, Detail, Dirt, Crack, ...
        let detail = bf2["maps"]
            .get(1)
            .and_then(|m| m["path"].as_str())
            .filter(|_| technique.starts_with("basedetail"));
        if let Some(detail) = detail {
            extension.detail = Some(asset_server.load(format!("imported://{detail}")));
            extension.flags |= StaticLayers::HAS_DETAIL;
            if base.alpha_mode != AlphaMode::Opaque {
                extension.flags |= StaticLayers::ALPHA_FROM_DETAIL;
            }
        }
    }

    let handle = layered.add(StaticMaterial { base, extension });
    cache.0.insert(gltf_material.id(), handle.clone());
    Some(handle)
}
