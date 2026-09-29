//! The deploy screen: pick a kit and a spawn point on the map. Opens when we die, toggles
//! with Enter while alive, and closes when we spawn. Choices go to the server right away
//! as a [`DeployRequest`]; the replicated [`Deployment`] shows what the server has.
//!
//! Every mode spawns at the flags a team holds; Rush's flags move with the front (points
//! nobody spawns at are hidden) and its charges show on the map, and attackers out of
//! tickets wait for the round to end.

use bevy::{
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};
use game_shared::{
    conquest::{ControlPoint, DeployRequest, Deployment, FlagState, RoundState, Tickets},
    modes::{Charge, ChargeState, ModeState, SpawnBlocked},
    protocol::Player,
    squad::{MAX_MEMBERS, SquadMember, SquadRequest, squad_name},
    level::LoadedLevel,
    protocol::Team,
    weapons::Armory,
};

use crate::{
    combat::weapon_display_name,
    conquest_hud::{ENEMY, FRIENDLY, NEUTRAL, SQUAD, charge_color, team_color},
    map_markers::{LabelRequest, Obstacle, place_labels},
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
                    rebuild_squads,
                    pick_kit,
                    pick_control_point,
                    pick_squad,
                    send_choice,
                    update_markers,
                    update_charge_markers,
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
struct SquadList;
#[derive(Component, Clone, Copy)]
enum SquadButton {
    Create,
    Join(u8),
    Leave,
    /// Toggles spawning on our squad leader.
    SpawnOnLeader,
}
#[derive(Component)]
struct PointMarker {
    entity: Entity,
    index: u8,
}
/// The owner's flag on a control point's marker.
#[derive(Component)]
struct PointFlag(Entity);
/// A Rush charge on the map.
#[derive(Component)]
struct ChargeMarker(Entity);

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
                        side.spawn((
                            SquadList,
                            Node {
                                flex_direction: FlexDirection::Column,
                                row_gap: px(4),
                                margin: UiRect::bottom(px(6)),
                                ..default()
                            },
                        ));
                        side.spawn((
                            crate::commander::CommanderPanel,
                            Node {
                                flex_direction: FlexDirection::Column,
                                row_gap: px(4),
                                margin: UiRect::bottom(px(6)),
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

#[allow(clippy::too_many_arguments)]
fn rebuild_markers(
    mut commands: Commands,
    level: Option<Res<LoadedLevel>>,
    added: Query<(), Added<ControlPoint>>,
    mut removed: RemovedComponents<ControlPoint>,
    control_points: Query<(Entity, &ControlPoint)>,
    added_charges: Query<(), Added<Charge>>,
    charges: Query<(Entity, &Charge)>,
    map: Single<(Entity, Option<&Children>), With<MapImage>>,
) {
    let level_changed = level.as_ref().is_some_and(|l| l.is_changed());
    if added.is_empty() && added_charges.is_empty() && removed.read().next().is_none() && !level_changed {
        return;
    }
    let Some(level) = level else {
        return;
    };
    let (map, children) = *map;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    // Names where they don't cover each other or the flags (see `map_markers::place_labels`).
    let points: Vec<(Entity, &ControlPoint, Vec2)> = control_points
        .iter()
        .map(|(entity, cp)| (entity, cp, map_uv(&level, cp.position).clamp(Vec2::ZERO, Vec2::ONE)))
        .collect();
    let icon = Rect::from_center_half_size(Vec2::ZERO, Vec2::splat(MARKER / 2.0));
    let obstacles: Vec<Obstacle> = points
        .iter()
        .map(|&(_, _, uv)| Obstacle {
            rect: Rect::from_center_half_size(uv * MAP_SIZE, icon.half_size()),
            hard: true,
        })
        .collect();
    let requests: Vec<LabelRequest> = points
        .iter()
        .enumerate()
        .map(|(i, &(_, cp, uv))| LabelRequest {
            at: uv * MAP_SIZE,
            icon,
            chars: cp.name.chars().count(),
            font: 12.0,
            priority: 0,
            previous: None,
            own: Some(i),
        })
        .collect();
    let spots = place_labels(&requests, &obstacles, Rect::new(0.0, 0.0, MAP_SIZE, MAP_SIZE));
    // Rush's charges: small squares with their letter, under the flags.
    for (entity, charge) in &charges {
        let uv = map_uv(&level, charge.position).clamp(Vec2::ZERO, Vec2::ONE);
        let size = 16.0;
        commands.entity(map).with_child((
            ChargeMarker(entity),
            Node {
                position_type: PositionType::Absolute,
                left: percent(uv.x * 100.0),
                top: percent(uv.y * 100.0),
                width: px(size),
                height: px(size),
                margin: UiRect {
                    left: px(-size / 2.0),
                    top: px(-size / 2.0),
                    ..default()
                },
                border: UiRect::all(px(1.5)),
                border_radius: BorderRadius::all(px(3)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(NEUTRAL),
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
            Visibility::Hidden,
            bevy::ui::FocusPolicy::Pass,
            children![(Text::new(charge.name.clone()), font(10.0), TextColor(TEXT))],
        ));
    }
    for (&(entity, cp, uv), spot) in points.iter().zip(spots) {
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
                PointFlag(entity),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(2.0),
                    top: px(MARKER / 2.0 - 8.5),
                    width: px(MARKER - 8.0),
                    height: px(13.0),
                    border_radius: BorderRadius::all(px(1)),
                    ..default()
                },
                ImageNode::default(),
                Visibility::Hidden,
                bevy::ui::FocusPolicy::Pass,
            ))
            .with_children(|marker| {
                let Some(spot) = spot else { return };
                marker.spawn((
                    Text::new(cp.name.clone()),
                    font(12.0 * spot.scale),
                    TextColor(TEXT),
                    TextShadow {
                        offset: Vec2::splat(1.0),
                        color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                    },
                    TextLayout::new(Justify::Center, LineBreak::NoWrap),
                    Node {
                        position_type: PositionType::Absolute,
                        // From the point (inside the marker's 2 px border).
                        left: px(MARKER / 2.0 - 2.0 + spot.offset.x),
                        top: px(MARKER / 2.0 - 2.0 + spot.offset.y),
                        width: px(spot.size.x),
                        height: px(spot.size.y),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                ));
            });
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
    flags: Query<(&FlagState, Option<&SpawnBlocked>)>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut screen: ResMut<DeployScreen>,
) {
    let Ok((team, server)) = players.single() else {
        return;
    };
    for (interaction, marker) in &markers {
        let ours = flags
            .get(marker.entity)
            .is_ok_and(|(f, blocked)| f.owner == *team && blocked.is_none_or(|b| b.0 != *team));
        if *interaction == Interaction::Pressed && ours {
            let choice = screen.choice(server);
            choice.1 = Some(marker.index);
            choice.2 = false;
            screen.changed = true;
        }
    }
}

fn squad_button(list: &mut ChildSpawnerCommands, action: SquadButton, name: String, label: String) {
    list.spawn((
        action,
        Button,
        Name::new(name),
        Node {
            padding: UiRect::axes(px(10), px(4)),
            border_radius: BorderRadius::all(px(5)),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.08)),
    ))
    .with_child((Text::new(label), font(13.0), TextColor(TEXT)));
}

/// Our squad with its members, or the team's squads to join; rebuilt when they change.
#[allow(clippy::type_complexity)]
fn rebuild_squads(
    mut commands: Commands,
    local: Query<(Entity, &Team, &Deployment, Option<&SquadMember>), With<LocalPlayer>>,
    players: Query<(Entity, &Player, &Team, Option<&SquadMember>)>,
    list: Single<(Entity, Option<&Children>), With<SquadList>>,
    mut built: Local<String>,
) {
    let Ok((me, team, deployment, mine)) = local.single() else {
        return;
    };
    let mut squads: Vec<(u8, Vec<(String, bool, bool)>)> = Vec::new();
    for (entity, player, player_team, member) in &players {
        let Some(member) = member.filter(|_| player_team == team) else {
            continue;
        };
        let entry = match squads.iter_mut().find(|(squad, _)| *squad == member.squad) {
            Some(entry) => entry,
            None => {
                squads.push((member.squad, Vec::new()));
                squads.last_mut().unwrap()
            }
        };
        entry.1.push((player.name.clone(), member.leader, entity == me));
    }
    squads.sort_by_key(|(squad, _)| *squad);
    for (_, members) in &mut squads {
        members.sort_by_key(|(name, leader, _)| (!*leader, name.clone()));
    }
    let key = format!("{mine:?}{squads:?}{}", deployment.on_squad_leader);
    if *built == key {
        return;
    }
    *built = key;

    let (list, children) = *list;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    commands.entity(list).with_children(|list| {
        match mine {
            Some(mine) => {
                let members = squads
                    .iter()
                    .find(|(squad, _)| *squad == mine.squad)
                    .map(|(_, m)| m.as_slice())
                    .unwrap_or_default();
                list.spawn((
                    Text::new(format!(
                        "{} SQUAD  {}/{MAX_MEMBERS}",
                        squad_name(mine.squad).to_uppercase(),
                        members.len()
                    )),
                    font(15.0),
                    TextColor(SQUAD),
                ));
                let names = members
                    .iter()
                    .map(|(name, leader, _)| if *leader { format!("{name} (leader)") } else { name.clone() })
                    .collect::<Vec<_>>()
                    .join(", ");
                list.spawn((Text::new(names), font(12.0), TextColor(DIM)));
                list.spawn(Node {
                    column_gap: px(6),
                    ..default()
                })
                .with_children(|row| {
                    if !mine.leader {
                        let label = if deployment.on_squad_leader {
                            "Spawning on leader: on"
                        } else {
                            "Spawn on leader: off"
                        };
                        squad_button(row, SquadButton::SpawnOnLeader, "spawn:leader".into(), label.into());
                    }
                    squad_button(row, SquadButton::Leave, "squad:leave".into(), "Leave squad".into());
                });
            }
            None => {
                list.spawn((Text::new("SQUADS"), font(15.0), TextColor(TEXT)));
                for (squad, members) in &squads {
                    if members.len() >= MAX_MEMBERS {
                        continue;
                    }
                    squad_button(
                        list,
                        SquadButton::Join(*squad),
                        format!("squad:join:{squad}"),
                        format!("Join {}  ({}/{MAX_MEMBERS})", squad_name(*squad), members.len()),
                    );
                }
                squad_button(list, SquadButton::Create, "squad:create".into(), "Create squad".into());
            }
        }
    });
}

fn pick_squad(
    buttons: Query<(&Interaction, &SquadButton), Changed<Interaction>>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut screen: ResMut<DeployScreen>,
    mut requests: MessageWriter<SquadRequest>,
) {
    let Ok((_, server)) = players.single() else {
        return;
    };
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            SquadButton::Create => {
                requests.write(SquadRequest::Create);
            }
            SquadButton::Join(squad) => {
                requests.write(SquadRequest::Join(*squad));
            }
            SquadButton::Leave => {
                requests.write(SquadRequest::Leave);
            }
            SquadButton::SpawnOnLeader => {
                let choice = screen.choice(server);
                choice.2 = !choice.2;
                screen.changed = true;
            }
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

#[allow(clippy::type_complexity)]
fn update_markers(
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    flags: Query<(&ControlPoint, &FlagState, Option<&SpawnBlocked>)>,
    mut markers: Query<(&PointMarker, &Interaction, &mut BackgroundColor, &mut BorderColor, &mut Visibility), Without<PointFlag>>,
    mut point_flags: Query<(&PointFlag, &mut ImageNode, &mut Visibility)>,
    icons: Res<crate::map_icons::UiIcons>,
) {
    let team = local_team(&players);
    for (flag, mut image, mut visibility) in &mut point_flags {
        let Ok((_, state, _)) = flags.get(flag.0) else { continue };
        match icons.side(state.owner).flag.clone() {
            Some(handle) => {
                if image.image != handle {
                    image.image = handle;
                }
                visibility.set_if_neq(Visibility::Inherited);
            }
            None => {
                visibility.set_if_neq(Visibility::Hidden);
            }
        }
    }
    let chosen = players.single().ok().and_then(|(_, d)| d.control_point);
    for (marker, interaction, mut background, mut border, mut shown) in &mut markers {
        let Ok((cp, state, blocked)) = flags.get(marker.entity) else {
            continue;
        };
        // Rush's points nobody spawns at right now aren't worth showing.
        let hidden = cp.uncapturable && state.owner == Team::Spectator;
        shown.set_if_neq(if hidden { Visibility::Hidden } else { Visibility::Inherited });
        // Ours, unless enemies are at it (staged modes).
        let closed = blocked.is_some_and(|b| b.0 == team);
        let ours = state.owner == team && !closed;
        let mut color = team_color(state.owner, team);
        if closed {
            color = color.with_alpha(0.35);
        }
        if ours && *interaction == Interaction::Hovered {
            color = color.lighter(0.12);
        }
        background.0 = color;
        let selected = ours && chosen == Some(marker.index);
        *border = BorderColor::all(if selected { TEXT } else { Color::srgba(0.0, 0.0, 0.0, 0.6) });
    }
}

/// Rush's charges in their state's colour; those of stages still to come stay hidden.
fn update_charge_markers(
    time: Res<Time>,
    players: Query<(&Team, &Deployment), With<LocalPlayer>>,
    modes: Query<&ModeState>,
    charges: Query<&ChargeState>,
    mut markers: Query<(&ChargeMarker, &mut BackgroundColor, &mut Visibility)>,
) {
    let (Ok(mode), team) = (modes.single(), local_team(&players)) else {
        return;
    };
    for (marker, mut background, mut visibility) in &mut markers {
        let Ok(state) = charges.get(marker.0) else { continue };
        let shown = !matches!(state, ChargeState::Waiting);
        visibility.set_if_neq(if shown { Visibility::Inherited } else { Visibility::Hidden });
        background.0 = charge_color(state, mode, team, time.elapsed_secs());
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
    control_points: Query<(&ControlPoint, &FlagState, Option<&SpawnBlocked>)>,
    rounds: Query<(&RoundState, Option<&ModeState>, Option<&Tickets>)>,
    mut text: Single<(&mut Text, &mut TextColor), With<StatusText>>,
) {
    let Ok((team, deployment)) = players.single() else {
        return;
    };
    let held: Vec<&ControlPoint> = control_points
        .iter()
        .filter(|(_, state, blocked)| state.owner == *team && blocked.is_none_or(|b| b.0 != *team))
        .map(|(cp, ..)| cp)
        .collect();
    let at = deployment
        .control_point
        .and_then(|i| held.iter().find(|cp| cp.index == i))
        .map_or("any flag we hold".to_string(), |cp| cp.name.clone());
    let at = if deployment.on_squad_leader { format!("our squad leader, else {at}") } else { at };
    let round = rounds.single().ok();
    let out_of_tickets = round.is_some_and(|(_, mode, tickets)| mode.is_some_and(|m| !m.can_spawn(*team, tickets)));
    let (line, color) = if matches!(round, Some((RoundState::Ended { .. }, ..))) {
        ("Round over".to_string(), DIM)
    } else if out_of_tickets && soldier.is_empty() {
        ("Out of tickets: no more reinforcements this round".to_string(), ENEMY)
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
