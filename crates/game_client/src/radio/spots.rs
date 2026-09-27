//! Enemies our team spotted: a red marker over each on the HUD, with its distance, and on
//! the minimap and the big map ([`MapMarkers`]). The server marks and unmarks them
//! (`Spotted`, replicated to everyone; only the spotting team's are shown).

use bevy::prelude::*;
use game_shared::{protocol::Team, radio::Spotted};

use crate::{
    camera::PlayerCamera,
    conquest_hud::ENEMY,
    map_markers::{MapMarker, MapMarkers, MarkerSystems},
    net::LocalPlayer,
    prediction::SoldierRender,
    vehicles::VehicleView,
};

pub struct SpotsPlugin;

impl Plugin for SpotsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpottedTargets>()
            .add_systems(Startup, spawn_root)
            .add_systems(PostUpdate, (find_targets, update_markers).chain().in_set(MarkerSystems));
    }
}

/// Where the enemies our team spotted are (feet or hull).
#[derive(Resource, Default)]
pub struct SpottedTargets(pub Vec<(Entity, Vec3)>);

const SPOTTED: Color = Color::srgb(1.0, 0.22, 0.18);
/// Markers hang this far above a soldier's feet or a vehicle's origin (meters).
const ABOVE_SOLDIER: f32 = 2.3;
const ABOVE_VEHICLE: f32 = 3.5;

#[derive(Component)]
struct MarkerRoot;

#[derive(Component)]
struct Marker(Entity);

#[derive(Component)]
struct MarkerText;

fn spawn_root(mut commands: Commands) {
    commands.spawn((
        MarkerRoot,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
        Pickable::IGNORE,
    ));
}

#[allow(clippy::type_complexity)]
fn find_targets(
    local: Query<&Team, With<LocalPlayer>>,
    spotted: Query<(Entity, &Spotted, Option<&SoldierRender>, Option<&VehicleView>)>,
    mut targets: ResMut<SpottedTargets>,
    mut markers: ResMut<MapMarkers>,
) {
    targets.0.clear();
    let Ok(&team) = local.single() else {
        return;
    };
    for (entity, spot, soldier, vehicle) in &spotted {
        if spot.by != team || team == Team::Spectator {
            continue;
        }
        let position = match (soldier, vehicle) {
            (Some(soldier), _) => soldier.position,
            (None, Some(vehicle)) => vehicle.transform.translation,
            _ => continue,
        };
        targets.0.push((entity, position));
        markers.0.push(MapMarker {
            key: entity,
            position,
            color: ENEMY,
            size: 7.0,
            label: None,
        });
    }
}

#[allow(clippy::type_complexity)]
fn update_markers(
    mut commands: Commands,
    targets: Res<SpottedTargets>,
    vehicles: Query<(), With<VehicleView>>,
    camera: Single<(&Camera, &GlobalTransform), With<PlayerCamera>>,
    root: Single<Entity, With<MarkerRoot>>,
    mut markers: Query<(Entity, &Marker, &mut Node, &Children)>,
    mut texts: Query<&mut Text, With<MarkerText>>,
) {
    let (camera, view) = *camera;
    let mut wanted: Vec<(Entity, Vec2, f32)> = targets
        .0
        .iter()
        .filter_map(|&(entity, position)| {
            let above = if vehicles.contains(entity) { ABOVE_VEHICLE } else { ABOVE_SOLDIER };
            let point = position + Vec3::Y * above;
            let at = camera.world_to_viewport(view, point).ok()?;
            Some((entity, at, point.distance(view.translation())))
        })
        .collect();
    for (marker_entity, marker, mut node, children) in &mut markers {
        let Some(index) = wanted.iter().position(|w| w.0 == marker.0) else {
            commands.entity(marker_entity).despawn();
            continue;
        };
        let (_, at, distance) = wanted.swap_remove(index);
        node.left = px(at.x - 30.0);
        node.top = px(at.y - 8.0);
        for child in children.iter() {
            if let Ok(mut text) = texts.get_mut(child) {
                let line = format!("{distance:.0} m");
                if text.0 != line {
                    text.0 = line;
                }
            }
        }
    }
    for (entity, at, distance) in wanted {
        commands.entity(*root).with_child((
            Marker(entity),
            Node {
                position_type: PositionType::Absolute,
                left: px(at.x - 30.0),
                top: px(at.y - 8.0),
                width: px(60),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(3),
                ..default()
            },
            children![
                (
                    // A diamond.
                    Node {
                        width: px(11),
                        height: px(11),
                        border: UiRect::all(px(1.5)),
                        ..default()
                    },
                    UiTransform::from_rotation(Rot2::degrees(45.0)),
                    BackgroundColor(SPOTTED.with_alpha(0.85)),
                    BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.75)),
                ),
                (
                    MarkerText,
                    Text::new(format!("{distance:.0} m")),
                    TextFont {
                        font_size: FontSize::Px(12.0),
                        ..default()
                    },
                    TextColor(SPOTTED),
                    TextShadow {
                        offset: Vec2::splat(1.0),
                        color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                    },
                ),
            ],
        ));
    }
}
