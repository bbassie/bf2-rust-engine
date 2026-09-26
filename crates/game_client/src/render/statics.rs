//! Draws static level objects: loads the glTF mesh named by [`StaticMesh`] and spawns its
//! primitives as children once loaded.

use bevy::{
    asset::LoadState,
    gltf::{GltfAssetLabel, GltfMesh},
    prelude::*,
};
use game_shared::statics::StaticMesh;

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
        for primitive in &mesh.primitives {
            // The PBR plugin stores a StandardMaterial next to every glTF material.
            let material: Handle<StandardMaterial> = primitive
                .material
                .as_ref()
                .and_then(|m| m.path())
                .and_then(|path| {
                    let label = format!("{}/std", path.label()?);
                    Some(asset_server.load(path.clone().with_label(label)))
                })
                .unwrap_or_default();
            commands
                .entity(entity)
                .with_child((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material)));
        }
        commands.entity(entity).remove::<PendingMesh>();
    }
}
