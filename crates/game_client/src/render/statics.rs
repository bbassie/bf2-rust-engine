//! Draws static level objects: loads the glTF mesh named by [`StaticMesh`] and spawns its
//! primitives as children once loaded, with BF2 materials.

use bevy::{asset::LoadState, gltf::{GltfAssetLabel, GltfMesh}, prelude::*};
use game_shared::statics::StaticMesh;

use super::materials::Bf2Materials;

pub struct StaticRenderPlugin;

impl Plugin for StaticRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(request_mesh)
            .add_systems(Update, spawn_loaded_meshes);
    }
}

#[derive(Component)]
struct PendingMesh(Handle<GltfMesh>);

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
    mut materials: Bf2Materials,
    asset_server: Res<AssetServer>,
) {
    for (entity, PendingMesh(handle)) in &pending {
        if let LoadState::Failed(err) = asset_server.load_state(handle) {
            warn!("static mesh failed to load: {err}");
            commands.entity(entity).remove::<PendingMesh>();
            continue;
        }
        let Some(mesh) = gltf_meshes.get(handle) else {
            continue;
        };
        // The glTF's own materials may not be ready yet; try again next frame.
        let Some(primitive_materials) = mesh
            .primitives
            .iter()
            .map(|primitive| materials.for_primitive(primitive))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        for (primitive, material) in mesh.primitives.iter().zip(primitive_materials) {
            commands
                .entity(entity)
                .with_child((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material)));
        }
        commands.entity(entity).remove::<PendingMesh>();
    }
}
