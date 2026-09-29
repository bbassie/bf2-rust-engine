//! What the modes beyond conquest show besides the bar at the top ([`crate::conquest_hud`]):
//!
//! - Rush's charges in the world: the object of the layout's template (by default BF2's
//!   `xp1_generator`, else a marker box of our own) with a beacon that glows while the charge
//!   can be armed and blinks red, lighting its surroundings, once armed; gone once destroyed
//!   (the server plays the explosion).
//! - A marker over each objective of the current stage on the HUD, with its distance: Rush's
//!   charges, Breakthrough's open flags.
//! - The charges on the minimap, the big map and the commander screen ([`MapMarkers`]).

use bevy::prelude::*;
use game_data::{ObjectDesc, modes::ModeKind};
use game_shared::{
    config::GamePaths,
    conquest::{ControlPoint, FlagState},
    modes::{Charge, ChargeState, Locked, ModeState},
    protocol::Team,
    statics::StaticMesh,
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{DESTROYED, charge_color, team_color},
    map_markers::{CONTROL_POINT_LABEL, FLAG_LAYER, MapMarker, MapMarkers, MarkerSystems},
    net::LocalPlayer,
};

pub struct ModeHudPlugin;

impl Plugin for ModeHudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChargeLooks>()
            .add_systems(Startup, spawn_marker_root)
            .add_systems(Update, (dress_charges, update_beacons).chain())
            .add_systems(PostUpdate, (charge_map_markers, update_objective_markers).in_set(MarkerSystems));
    }
}

/// The template drawn for charges whose layout names none (BF2 Special Forces' generator).
const DEFAULT_TEMPLATE: &str = "xp1_generator";
/// The beacon sits on a thin mast from this height above a charge's foot (about the top of
/// the generator) up to `BEACON_HEIGHT`, meters.
const MAST_FOOT: f32 = 0.7;
const BEACON_HEIGHT: f32 = 1.3;
/// HUD markers hang this far above an objective (meters).
const MARKER_ABOVE: f32 = 2.4;

/// A charge's look, once given: the entity carrying its beacon.
#[derive(Component)]
struct Dressed {
    beacon: Entity,
    /// The model's parts (hidden once destroyed).
    model: Vec<Entity>,
}

/// The light on a charge.
#[derive(Component)]
struct Beacon;

/// Materials of the beacon and of the marker box.
#[derive(Resource)]
struct ChargeLooks {
    mast_mesh: Handle<Mesh>,
    mast_material: Handle<StandardMaterial>,
    beacon_mesh: Handle<Mesh>,
    beacon_off: Handle<StandardMaterial>,
    beacon_ready: Handle<StandardMaterial>,
    beacon_armed: Handle<StandardMaterial>,
    box_mesh: Handle<Mesh>,
    box_material: Handle<StandardMaterial>,
    panel_mesh: Handle<Mesh>,
    panel_material: Handle<StandardMaterial>,
}

/// Gives every new charge its model and beacon, and keeps them where the charge is (the
/// server moves generated charges onto walkable ground once it can).
#[allow(clippy::type_complexity)]
fn dress_charges(
    mut commands: Commands,
    paths: Option<Res<GamePaths>>,
    looks: Res<ChargeLooks>,
    new: Query<(Entity, &Charge), Without<Dressed>>,
    mut moved: Query<(&Charge, &mut Transform), (With<Dressed>, Changed<Charge>)>,
) {
    for (charge, mut transform) in &mut moved {
        *transform = placement(charge);
    }
    for (entity, charge) in &new {
        let template = charge.template.as_deref().unwrap_or(DEFAULT_TEMPLATE);
        let desc: Option<ObjectDesc> = paths
            .as_ref()
            .filter(|p| p.find(format!("templates/{template}.ron")).is_file())
            .and_then(|p| p.read_ron(format!("templates/{template}.ron")).ok());
        let mut model = Vec::new();
        match desc.filter(|d| d.parts.iter().any(|p| p.mesh.is_some())) {
            Some(desc) => {
                for part in desc.parts {
                    let Some(mesh) = part.mesh else { continue };
                    let transform = game_shared::level::placement_transform(&part.placement);
                    model.push(
                        commands
                            .spawn((
                                transform,
                                StaticMesh {
                                    path: mesh,
                                    index: part.mesh_index,
                                },
                                ChildOf(entity),
                            ))
                            .id(),
                    );
                }
            }
            None => {
                // A box with a panel on its front.
                model.push(
                    commands
                        .spawn((
                            Mesh3d(looks.box_mesh.clone()),
                            MeshMaterial3d(looks.box_material.clone()),
                            Transform::from_xyz(0.0, 0.6, 0.0),
                            ChildOf(entity),
                        ))
                        .id(),
                );
                model.push(
                    commands
                        .spawn((
                            Mesh3d(looks.panel_mesh.clone()),
                            MeshMaterial3d(looks.panel_material.clone()),
                            Transform::from_xyz(0.0, 0.75, -0.26),
                            ChildOf(entity),
                        ))
                        .id(),
                );
            }
        }
        model.push(
            commands
                .spawn((
                    Mesh3d(looks.mast_mesh.clone()),
                    MeshMaterial3d(looks.mast_material.clone()),
                    Transform::from_xyz(0.0, (MAST_FOOT + BEACON_HEIGHT) / 2.0, 0.0),
                    ChildOf(entity),
                ))
                .id(),
        );
        let beacon = commands
            .spawn((
                Beacon,
                Mesh3d(looks.beacon_mesh.clone()),
                MeshMaterial3d(looks.beacon_off.clone()),
                Transform::from_xyz(0.0, BEACON_HEIGHT, 0.0),
                PointLight {
                    color: Color::srgb(1.0, 0.15, 0.1),
                    intensity: 0.0,
                    range: 10.0,
                    shadow_maps_enabled: false,
                    ..default()
                },
                ChildOf(entity),
            ))
            .id();
        commands
            .entity(entity)
            .insert((placement(charge), Visibility::default(), Dressed { beacon, model }));
    }
}

fn placement(charge: &Charge) -> Transform {
    Transform::from_translation(charge.position).with_rotation(Quat::from_rotation_y(charge.yaw))
}

impl FromWorld for ChargeLooks {
    fn from_world(world: &mut World) -> Self {
        world.resource_scope(|world, mut meshes: Mut<Assets<Mesh>>| {
            let mut materials = world.resource_mut::<Assets<StandardMaterial>>();
            Self::new(&mut meshes, &mut materials)
        })
    }
}

impl ChargeLooks {
    fn new(meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>) -> Self {
        let glow = |materials: &mut Assets<StandardMaterial>, color: Color, strength: f32| {
            materials.add(StandardMaterial {
                base_color: color,
                emissive: LinearRgba::from(color) * strength,
                unlit: false,
                ..default()
            })
        };
        Self {
            mast_mesh: meshes.add(Cylinder::new(0.015, BEACON_HEIGHT - MAST_FOOT)),
            mast_material: materials.add(StandardMaterial {
                base_color: Color::srgb(0.15, 0.15, 0.16),
                perceptual_roughness: 0.5,
                ..default()
            }),
            beacon_mesh: meshes.add(Sphere::new(0.06).mesh().uv(12, 8)),
            beacon_off: glow(materials, Color::srgb(0.25, 0.25, 0.25), 0.0),
            beacon_ready: glow(materials, Color::srgb(1.0, 0.55, 0.1), 1.5),
            beacon_armed: glow(materials, Color::srgb(1.0, 0.05, 0.02), 6.0),
            box_mesh: meshes.add(Cuboid::new(0.9, 1.2, 0.5)),
            box_material: materials.add(StandardMaterial {
                base_color: Color::srgb(0.24, 0.27, 0.22),
                perceptual_roughness: 0.7,
                ..default()
            }),
            panel_mesh: meshes.add(Cuboid::new(0.5, 0.3, 0.02)),
            panel_material: materials.add(StandardMaterial {
                base_color: Color::srgb(0.05, 0.08, 0.06),
                emissive: LinearRgba::rgb(0.1, 0.6, 0.25),
                ..default()
            }),
        }
    }
}

/// The beacon: off while its stage hasn't come, steady amber while it can be armed, blinking
/// red (twice a second) and lighting the ground once armed; the charge is gone once destroyed.
#[allow(clippy::type_complexity)]
fn update_beacons(
    time: Res<Time>,
    looks: Res<ChargeLooks>,
    charges: Query<(&ChargeState, &Dressed)>,
    mut beacons: Query<(&mut MeshMaterial3d<StandardMaterial>, &mut PointLight), With<Beacon>>,
    mut visibility: Query<&mut Visibility>,
) {
    let blink = (time.elapsed_secs() * 2.0).fract() < 0.5;
    for (state, dressed) in &charges {
        let (material, light, shown) = match state {
            ChargeState::Waiting => (&looks.beacon_off, 0.0, true),
            ChargeState::Active { .. } => (&looks.beacon_ready, 0.0, true),
            ChargeState::Armed { .. } if blink => (&looks.beacon_armed, 60_000.0, true),
            ChargeState::Armed { .. } => (&looks.beacon_off, 0.0, true),
            ChargeState::Destroyed => (&looks.beacon_off, 0.0, false),
        };
        if let Ok((mut current, mut point)) = beacons.get_mut(dressed.beacon) {
            if current.0 != *material {
                current.0 = material.clone();
            }
            if point.intensity != light {
                point.intensity = light;
            }
        }
        let wanted = if shown { Visibility::Inherited } else { Visibility::Hidden };
        for part in dressed.model.iter().chain([&dressed.beacon]) {
            if let Ok(mut v) = visibility.get_mut(*part) {
                v.set_if_neq(wanted);
            }
        }
    }
}

/// The charges of the current stage (and the destroyed ones of earlier stages, dimmed) on the
/// maps.
fn charge_map_markers(
    time: Res<Time>,
    local: Query<&Team, With<LocalPlayer>>,
    modes: Query<&ModeState>,
    charges: Query<(Entity, &Charge, &ChargeState)>,
    mut markers: ResMut<MapMarkers>,
) {
    let (Ok(mode), team) = (modes.single(), local.single().copied().unwrap_or_default()) else {
        return;
    };
    for (entity, charge, state) in &charges {
        let marker = match state {
            ChargeState::Waiting => continue,
            ChargeState::Destroyed => MapMarker::dot(entity, charge.position, DESTROYED, 8.0),
            _ if charge.stage != mode.stage => continue,
            _ => MapMarker::dot(entity, charge.position, charge_color(state, mode, team, time.elapsed_secs()), 13.0)
                .label(format!("Charge {}", charge.name))
                .priority(CONTROL_POINT_LABEL),
        };
        markers.0.push(marker.layer(FLAG_LAYER));
    }
}

#[derive(Component)]
struct MarkerRoot;

/// A HUD marker over an objective (a charge or a flag).
#[derive(Component)]
struct ObjectiveMarker(Entity);

#[derive(Component)]
struct MarkerBadge;

#[derive(Component)]
struct MarkerText;

fn spawn_marker_root(mut commands: Commands) {
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

/// Markers over the current stage's objectives: Rush's charges in play and Breakthrough's
/// open flags, with the letter, the colour of their state and the distance.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_objective_markers(
    mut commands: Commands,
    time: Res<Time>,
    local: Query<&Team, With<LocalPlayer>>,
    modes: Query<&ModeState>,
    charges: Query<(Entity, &Charge, &ChargeState)>,
    flags: Query<(Entity, &ControlPoint, &FlagState), Without<Locked>>,
    camera: Single<(&Camera, &GlobalTransform), With<PlayerCamera>>,
    root: Single<Entity, With<MarkerRoot>>,
    mut markers: Query<(Entity, &ObjectiveMarker, &mut Node, &Children)>,
    mut badges: Query<&mut BackgroundColor, With<MarkerBadge>>,
    mut texts: Query<(&mut Text, &mut TextColor), With<MarkerText>>,
) {
    let (camera, view) = *camera;
    let team = local.single().copied().unwrap_or_default();
    let now = time.elapsed_secs();
    // (objective, where on the screen, letter, colour, distance)
    let mut wanted: Vec<(Entity, Vec2, String, Color, f32)> = Vec::new();
    if let Ok(mode) = modes.single() {
        let mut add = |entity: Entity, position: Vec3, letter: String, color: Color| {
            let point = position + Vec3::Y * MARKER_ABOVE;
            if let Ok(at) = camera.world_to_viewport(view, point) {
                wanted.push((entity, at, letter, color, point.distance(view.translation())));
            }
        };
        match mode.kind {
            ModeKind::Rush => {
                for (entity, charge, state) in &charges {
                    if charge.stage == mode.stage && state.in_play() {
                        add(entity, charge.position, charge.name.clone(), charge_color(state, mode, team, now));
                    }
                }
            }
            ModeKind::Breakthrough => {
                for (entity, cp, flag) in &flags {
                    if !cp.uncapturable {
                        let letter: String = cp.name.chars().find(|c| c.is_alphanumeric()).into_iter().collect();
                        add(entity, cp.position, letter.to_uppercase(), team_color(flag.owner, team));
                    }
                }
            }
            _ => {}
        }
    }
    for (marker_entity, marker, mut node, children) in &mut markers {
        let Some(index) = wanted.iter().position(|w| w.0 == marker.0) else {
            commands.entity(marker_entity).despawn();
            continue;
        };
        let (_, at, _, color, distance) = wanted.swap_remove(index);
        node.left = px(at.x - 30.0);
        node.top = px(at.y - 14.0);
        for child in children.iter() {
            if let Ok(mut background) = badges.get_mut(child) {
                background.0 = color.with_alpha(0.85 * color.alpha().max(0.5));
                // The badge holds the letter.
                continue;
            }
            if let Ok((mut text, mut text_color)) = texts.get_mut(child) {
                let line = format!("{distance:.0} m");
                if text.0 != line {
                    text.0 = line;
                }
                text_color.0 = color.with_alpha(1.0);
            }
        }
    }
    for (entity, at, letter, color, distance) in wanted {
        commands.entity(*root).with_child((
            ObjectiveMarker(entity),
            Node {
                position_type: PositionType::Absolute,
                left: px(at.x - 30.0),
                top: px(at.y - 14.0),
                width: px(60),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(2),
                ..default()
            },
            children![
                (
                    MarkerBadge,
                    Node {
                        width: px(20),
                        height: px(20),
                        border: UiRect::all(px(1.5)),
                        border_radius: BorderRadius::all(px(4)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(color.with_alpha(0.85)),
                    BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
                    children![(
                        Text::new(letter),
                        TextFont {
                            font_size: FontSize::Px(13.0),
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    )],
                ),
                (
                    MarkerText,
                    Text::new(format!("{distance:.0} m")),
                    TextFont {
                        font_size: FontSize::Px(12.0),
                        ..default()
                    },
                    TextColor(color.with_alpha(1.0)),
                    TextShadow {
                        offset: Vec2::splat(1.0),
                        color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                    },
                ),
            ],
        ));
    }
}
