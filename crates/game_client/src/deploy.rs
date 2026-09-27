//! The deploy screen: pick a kit and a spawn point on the map. Opens when we die, toggles
//! with Enter while alive, and closes when we spawn. Choices go to the server right away
//! as a [`DeployRequest`]; the replicated [`Deployment`] shows what the server has.

use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    conquest::{ControlPoint, DeployRequest, Deployment, FlagState, RoundState},
    level::LoadedLevel,
    protocol::Team,
    weapons::Armory,
};

use crate::{
    combat::weapon_display_name,
    conquest_hud::{ENEMY, FRIENDLY, NEUTRAL, team_color},
    net::{LocalPlayer, LocalSoldier},
};

pub struct DeployPlugin;

impl Plugin for DeployPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DeployScreen>()
            .add_systems(Startup, spawn_deploy_screen)
            .add_systems(
                Update,
                (
                    open_and_close,
                    set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                    rebuild_markers,
                    rebuild_kits,
                    pick_kit,
                    pick_control_point,
                    send_choice,
                    update_markers,
                    update_kits,
                    update_status,
                )
                    .chain()
                    .after(crate::scenario::ScenarioSystems),
            );
    }
}

/// Whether the deploy screen is showing (other input, like grabbing the mouse, checks
/// this), and the choice being made.
#[derive(Resource, Default)]
pub struct DeployScreen {
    pub open: bool,
    /// Kit and control point picked here; sent when changed.
    choice: Option<(u8, Option<u8>, bool)>,
    changed: bool,
}

impl DeployScreen {
    /// The current choice, starting from what the server has.
    fn choice(&mut self, server: &Deployment) -> &mut (u8, Option<u8>, bool) {
        self.choice
            .get_or_insert((server.kit, server.control_point, server.on_squad_leader))
    }
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);
const MAP_SIZE: f32 = 520.0;
const MARKER: f32 = 24.0;

#[derive(Component)]
struct DeployRoot;
#[derive(Component)]
struct MapImage;
#[derive(Component)]
struct KitList;
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct KitButton(u8);
#[derive(Component)]
struct PointMarker {
    entity: Entity,
    index: u8,
}

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn spawn_deploy_screen(mut commands: Commands) {
    commands
        .spawn((
            DeployRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.55)),
            // Above the HUD.
            GlobalZIndex(10),
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::all(px(20)),
                    column_gap: px(20),
                    border_radius: BorderRadius::all(px(12)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.92)),
            ))
            .with_children(|panel| {
                panel.spawn((
                    MapImage,
                    Node {
                        width: px(MAP_SIZE),
                        height: px(MAP_SIZE),
                        border_radius: BorderRadius::all(px(8)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.16, 0.17, 0.18)),
                ));
                panel
                    .spawn(Node {
                        width: px(320),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        ..default()
                    })
                    .with_children(|side| {
                        side.spawn((Text::new("DEPLOY"), font(22.0), TextColor(TEXT)));
                        side.spawn((
                            Text::new("Pick a kit, then a flag on the map."),
                            font(13.0),
                            TextColor(DIM),
                        ));
                        side.spawn((
                            KitList,
                            Node {
                                flex_direction: FlexDirection::Column,
                                row_gap: px(4),
                                margin: UiRect::vertical(px(6)),
                                ..default()
                            },
                        ));
                        side.spawn((StatusText, Text::new(""), font(15.0), TextColor(TEXT)));
                    });
            });
        });
}

/// Opens on death and on Enter, closes on spawn (grabbing the mouse again) and on
/// Enter/Escape while alive.
#[allow(clippy::too_many_arguments)]
fn open_and_close(
    keys: Res<ButtonInput<KeyCode>>,
    actions: crate::settings::Actions,
    mut screen: ResMut<DeployScreen>,
    player: Query<(), With<LocalPlayer>>,
    soldier: Query<(), With<LocalSoldier>>,
    mut had_soldier: Local<bool>,
    mut root: Single<&mut Visibility, With<DeployRoot>>,
    mut cursor: Single<&mut CursorOptions>,
    window: Single<&Window>,
) {
    let alive = !soldier.is_empty();
    if player.is_empty() {
        screen.open = false;
    } else if *had_soldier && !alive {
        screen.open = true;
    } else if !*had_soldier && alive && screen.open {
        screen.open = false;
        if window.focused {
            cursor.visible = false;
            cursor.grab_mode = CursorGrabMode::Locked;
        }
    } else if actions.just_pressed(crate::settings::Action::Deploy) {
        screen.open = !screen.open || !alive;
    } else if keys.just_pressed(KeyCode::Escape) && alive {
        screen.open = false;
    }
    *had_soldier = alive;

    if screen.open && cursor.grab_mode != CursorGrabMode::None {
        cursor.visible = true;
        cursor.grab_mode = CursorGrabMode::None;
    }
    root.set_if_neq(if screen.open { Visibility::Inherited } else { Visibility::Hidden });
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    map: Single<Entity, With<MapImage>>,
) {
    match &level.desc.minimap {
        Some(path) => {
            commands
                .entity(*map)
                .insert(ImageNode::new(asset_server.load(format!("imported://{path}"))));
        }
        None => {
            commands.entity(*map).remove::<ImageNode>();
        }
    }
}

/// Map position (0..1, top-left origin) of a world position. The map covers the terrain,
/// north (-Z) up.
fn map_uv(level: &LoadedLevel, position: Vec3) -> Vec2 {
    let Some(heightmap) = &level.heightmap else {
        return Vec2::splat(0.5);
    };
    let size = heightmap.world_size().max(1.0);
    let corner = heightmap.center() - Vec3::new(size, 0.0, size) * 0.5;
    Vec2::new((position.x - corner.x) / size, (position.z - corner.z) / size)
}

fn rebuild_markers(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    added: Query<(), Added<ControlPoint>>,
    mut removed: RemovedComponents<ControlPoint>,
    control_points: Query<(Entity, &ControlPoint)>,
    map: Single<(Entity, Option<&Children>), With<MapImage>>,
) {
    let level_changed = level.as_ref().is_some_and(|l| l.is_changed());
    if added.is_empty() && removed.read().next().is_none() && !level_changed {
        return;
    }
    let Some(level) = level else {
        return;
    };
    let (map, children) = *map;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    for (entity, cp) in &control_points {
        let uv = map_uv(&level, cp.position).clamp(Vec2::ZERO, Vec2::ONE);
        commands.entity(map).with_children(|map| {
            map.spawn((
                PointMarker {
                    entity,
                    index: cp.index,
                },
                Button,
                Name::new(format!("cp:{}", cp.index)),
                Node {
                    position_type: PositionType::Absolute,
                    left: percent(uv.x * 100.0),
                    top: percent(uv.y * 100.0),
                    width: px(MARKER),
                    height: px(MARKER),
                    margin: UiRect {
                        left: px(-MARKER / 2.0),
                        top: px(-MARKER / 2.0),
                        ..default()
                    },
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(MARKER / 2.0)),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                BackgroundColor(NEUTRAL),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
            ))
            .with_child((
                Text::new(cp.name.clone()),
                font(12.0),
                TextColor(TEXT),
                TextShadow {
                    offset: Vec2::splat(1.0),
                    color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                },
                TextLayout::justify(Justify::Center),
                Node {
                    position_type: PositionType::Absolute,
                    top: px(MARKER),
                    left: px(MARKER / 2.0 - 60.0),
                    width: px(120),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ));
        });
    }
}

fn local_team(players: &Query<(&Team, &Deployment), With<LocalPlayer>>) -> Team {
    players.single().map(|(t, _)| *t).unwrap_or_default()
}

/// Kit class names for BF2's `kitType`s.
fn kit_title(kind: &str) -> &str {
    match kind.to_ascii_lowercase().as_str() {
        "specops" => "Special Forces",
        "sniper" => "Sniper",
        "assault" => "Assault",
        "support" => "Support",
        "engineer" => "Engineer",
        "medic" => "Medic",
        "at" => "Anti-Tank",
        _ => kind,
    }
}

fn rebuild_kits(
    mut commands: Commands,
    armory: Res<Armory>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    list: Single<(Entity, Option<&Children>), With<KitList>>,
    mut built_for: Local<Option<(Team, usize)>>,
) {
    let team = local_team(&players);
    let index = if team == Team::Two { 1 } else { 0 };
    let key = (team, armory.team_kits[index].len());
    if armory.is_changed() {
        *built_for = None;
    }
    if *built_for == Some(key) {
        return;
    }
    *built_for = Some(key);
    let (list, children) = *list;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    for (slot, kit_name) in armory.team_kits[index].iter().enumerate() {
        let Some(kit) = armory.kits.get(kit_name) else {
            continue;
        };
        // The weapons worth listing: primary and sidearm first, no knives or parachutes.
        let mut weapons: Vec<_> = kit
            .weapons
            .iter()
            .filter_map(|w| armory.weapon(w))
            .filter(|w| w.slot >= 2 && w.magazine_size > 0)
            .collect();
        weapons.sort_by_key(|w| match w.slot {
            3 => 0,
            2 => 2,
            _ => 1,
        });
        let summary = weapons
            .iter()
            .take(3)
            .map(|w| weapon_display_name(&w.display_name))
            .collect::<Vec<_>>()
            .join("  /  ");
        commands.entity(list).with_children(|list| {
            list.spawn((
                KitButton(slot as u8),
                Button,
                Name::new(format!("kit:{slot}")),
                Node {
                    padding: UiRect::axes(px(12), px(7)),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.05)),
            ))
            .with_children(|button| {
                button.spawn((Text::new(kit_title(&kit.kind)), font(16.0), TextColor(TEXT)));
                button.spawn((Text::new(summary), font(12.0), TextColor(DIM)));
            });
        });
    }
}

fn pick_kit(
    buttons: Query<(&Interaction, &KitButton), Changed<Interaction>>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut screen: ResMut<DeployScreen>,
) {
    let Ok((_, server)) = players.single() else {
        return;
    };
    for (interaction, button) in &buttons {
        if *interaction == Interaction::Pressed {
            screen.choice(server).0 = button.0;
            screen.changed = true;
        }
    }
}

fn pick_control_point(
    markers: Query<(&Interaction, &PointMarker), Changed<Interaction>>,
    flags: Query<&FlagState>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut screen: ResMut<DeployScreen>,
) {
    let Ok((team, server)) = players.single() else {
        return;
    };
    for (interaction, marker) in &markers {
        let ours = flags.get(marker.entity).is_ok_and(|f| f.owner == *team);
        if *interaction == Interaction::Pressed && ours {
            let choice = screen.choice(server);
            choice.1 = Some(marker.index);
            choice.2 = false;
            screen.changed = true;
        }
    }
}

fn send_choice(mut screen: ResMut<DeployScreen>, mut requests: MessageWriter<DeployRequest>) {
    if !screen.changed {
        return;
    }
    screen.changed = false;
    if let Some((kit, control_point, on_squad_leader)) = screen.choice {
        requests.write(DeployRequest {
            kit,
            control_point,
            on_squad_leader,
        });
    }
}

fn update_markers(
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    flags: Query<&FlagState>,
    mut markers: Query<(&PointMarker, &Interaction, &mut BackgroundColor, &mut BorderColor)>,
) {
    let team = local_team(&players);
    let chosen = players.single().ok().and_then(|(_, d)| d.control_point);
    for (marker, interaction, mut background, mut border) in &mut markers {
        let Ok(state) = flags.get(marker.entity) else {
            continue;
        };
        let ours = state.owner == team;
        let mut color = team_color(state.owner, team);
        if ours && *interaction == Interaction::Hovered {
            color = color.lighter(0.12);
        }
        background.0 = color;
        let selected = ours && chosen == Some(marker.index);
        *border = BorderColor::all(if selected { TEXT } else { Color::srgba(0.0, 0.0, 0.0, 0.6) });
    }
}

fn update_kits(
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut buttons: Query<(&KitButton, &Interaction, &mut BackgroundColor)>,
) {
    let kit = players.single().map(|(_, d)| d.kit).unwrap_or(u8::MAX);
    for (button, interaction, mut background) in &mut buttons {
        background.0 = if button.0 == kit {
            FRIENDLY.with_alpha(0.45)
        } else if *interaction == Interaction::Hovered {
            Color::srgba(1.0, 1.0, 1.0, 0.12)
        } else {
            Color::srgba(1.0, 1.0, 1.0, 0.05)
        };
    }
}

fn update_status(
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    soldier: Query<(), With<LocalSoldier>>,
    control_points: Query<(&ControlPoint, &FlagState)>,
    rounds: Query<&RoundState>,
    mut text: Single<(&mut Text, &mut TextColor), With<StatusText>>,
) {
    let Ok((team, deployment)) = players.single() else {
        return;
    };
    let held: Vec<&ControlPoint> = control_points
        .iter()
        .filter(|(_, state)| state.owner == *team)
        .map(|(cp, _)| cp)
        .collect();
    let at = deployment
        .control_point
        .and_then(|i| held.iter().find(|cp| cp.index == i))
        .map_or("any flag we hold".to_string(), |cp| cp.name.clone());
    let (line, color) = if matches!(rounds.single(), Ok(RoundState::Ended { .. })) {
        ("Round over".to_string(), DIM)
    } else if !soldier.is_empty() {
        (format!("Next deploy: {at}\nEnter or Esc to close"), DIM)
    } else if held.is_empty() && !control_points.is_empty() {
        ("No spawn point: your team holds no flag".to_string(), ENEMY)
    } else if deployment.respawn_in > 0.0 {
        (format!("Deploying at {at} in {:.1} s", deployment.respawn_in), TEXT)
    } else {
        (format!("Deploying at {at}..."), TEXT)
    };
    let (text, text_color) = &mut *text;
    if text.0 != line {
        text.0 = line;
    }
    text_color.0 = color;
}
