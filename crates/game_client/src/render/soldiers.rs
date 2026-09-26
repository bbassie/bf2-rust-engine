//! Placeholder soldier models: a team-colored capsule with a visor showing facing.

use bevy::prelude::*;
use game_shared::{
    protocol::{ControlledBy, Team},
    soldier::{SOLDIER_CENTER, SOLDIER_HEIGHT, SOLDIER_RADIUS, Soldier},
};

use crate::{
    net::LocalSoldier,
    prediction::{RenderStateSystems, SoldierRender},
};

pub struct SoldierRenderPlugin;

impl Plugin for SoldierRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, load_soldier_assets)
            .add_observer(spawn_visual)
            .add_observer(despawn_visual)
            .add_systems(
                PostUpdate,
                update_visuals
                    .after(RenderStateSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

#[derive(Resource)]
struct SoldierAssets {
    body: Handle<Mesh>,
    visor: Handle<Mesh>,
    visor_material: Handle<StandardMaterial>,
    /// Spectator/unknown, team one, team two.
    team_materials: [Handle<StandardMaterial>; 3],
}

/// The visual entity drawn for a soldier. Kept separate from the simulated entity so
/// smoothing never moves the hitbox.
#[derive(Component)]
struct SoldierVisual {
    soldier: Entity,
}

#[derive(Component)]
struct VisualOf(Entity);

fn load_soldier_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut team = |r, g, b| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(r, g, b),
            perceptual_roughness: 0.8,
            ..default()
        })
    };
    let team_materials = [team(0.6, 0.6, 0.6), team(0.25, 0.35, 0.7), team(0.7, 0.3, 0.2)];
    commands.insert_resource(SoldierAssets {
        body: meshes.add(Capsule3d::new(SOLDIER_RADIUS, SOLDIER_HEIGHT - 2.0 * SOLDIER_RADIUS)),
        visor: meshes.add(Cuboid::new(0.36, 0.12, 0.2)),
        visor_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.08, 0.08, 0.08),
            perceptual_roughness: 0.3,
            ..default()
        }),
        team_materials,
    });
}

fn spawn_visual(add: On<Add, Soldier>, mut commands: Commands, assets: Res<SoldierAssets>) {
    let visual = commands
        .spawn((
            SoldierVisual {
                soldier: add.entity,
            },
            Mesh3d(assets.body.clone()),
            MeshMaterial3d(assets.team_materials[0].clone()),
            Transform::default(),
            children![(
                Mesh3d(assets.visor.clone()),
                MeshMaterial3d(assets.visor_material.clone()),
                Transform::from_xyz(0.0, SOLDIER_HEIGHT * 0.5 - 0.25, -SOLDIER_RADIUS + 0.02),
            )],
        ))
        .id();
    commands.entity(add.entity).insert(VisualOf(visual));
}

fn despawn_visual(remove: On<Remove, Soldier>, mut commands: Commands, visuals: Query<&VisualOf>) {
    if let Ok(VisualOf(visual)) = visuals.get(remove.entity) {
        commands.entity(*visual).try_despawn();
    }
}

fn update_visuals(
    assets: Res<SoldierAssets>,
    soldiers: Query<(&SoldierRender, Option<&ControlledBy>, Has<LocalSoldier>)>,
    teams: Query<&Team>,
    mut visuals: Query<(
        &SoldierVisual,
        &mut Transform,
        &mut Visibility,
        &mut MeshMaterial3d<StandardMaterial>,
    )>,
) {
    for (visual, mut transform, mut visibility, mut material) in &mut visuals {
        let Ok((render, controlled_by, local)) = soldiers.get(visual.soldier) else {
            continue;
        };
        // First person: don't draw our own body inside the camera.
        visibility.set_if_neq(if local {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        });
        *transform = Transform::from_translation(render.position + SOLDIER_CENTER)
            .with_rotation(Quat::from_rotation_y(render.yaw));

        let team = controlled_by
            .and_then(|c| teams.get(c.0).ok())
            .copied()
            .unwrap_or_default();
        let index = match team {
            Team::Spectator => 0,
            Team::One => 1,
            Team::Two => 2,
        };
        if material.0 != assets.team_materials[index] {
            material.0 = assets.team_materials[index].clone();
        }
    }
}
