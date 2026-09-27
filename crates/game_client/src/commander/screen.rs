//! The commander screen (Caps Lock): the whole map with flags, our soldiers by squad,
//! spotted enemies, orders, our assets and what they are doing, and a side panel with the
//! squads, the orders and the assets. Pick a squad (in the list, or click one of its
//! soldiers on the map), then an order, then a spot on the map; pick an asset, then its
//! target (a satellite scan needs none). Right-click or Esc drops what was picked.

use bevy::{
    platform::collections::HashMap,
    prelude::*,
    ui::{FocusPolicy, RelativeCursorPosition},
};
use game_shared::{
    commander::{Asset, AssetEffect, CommanderAssets, CommanderRequest, OrderKind, SquadOrder, TeamAssets},
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::{ControlledBy, Player, Team},
    soldier::Soldier,
    squad::{SquadMember, squad_name},
};

use super::{CommanderScreen, GOLD, Tool, font, map_point, map_size, map_uv};
use crate::{
    commander::markers::{asset_color, order_color},
    conquest_hud::{ENEMY, FRIENDLY, NEUTRAL, SQUAD, team_color},
    map_markers::MapMarkers,
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
};

pub struct ScreenPlugin;

impl Plugin for ScreenPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_screen).add_systems(
            Update,
            (
                set_map_image.run_if(resource_exists_and_changed::<LoadedLevel>),
                show_screen,
                press_buttons,
                click_map,
                rebuild_panel,
                update_map,
            )
                .chain()
                .after(super::toggle_screen),
        );
    }
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);
const READY: Color = Color::srgb(0.5, 0.92, 0.45);
const BUTTON: Color = Color::srgba(1.0, 1.0, 1.0, 0.07);
/// Clicks within this share of the map of a squad's soldier pick the squad.
const PICK_RADIUS: f32 = 0.025;

#[derive(Component)]
struct ScreenRoot;
#[derive(Component)]
struct ScreenMap;
#[derive(Component)]
struct ScreenPanel;
#[derive(Component)]
struct ScreenHint;
/// The ring under the mouse while targeting.
#[derive(Component)]
struct TargetRing;

#[derive(Component, Clone, Copy, PartialEq)]
enum ScreenButton {
    Squad(u8),
    Order(OrderKind),
    Cancel,
    Asset(Asset),
    Resign,
    Close,
}

/// What a map icon stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum IconKey {
    Entity(Entity),
    /// The area an asset effect covers.
    Area(Entity),
}

#[derive(Clone, PartialEq)]
struct IconSpec {
    position: Vec3,
    color: Color,
    /// Diameter in pixels; for areas, the radius in meters.
    size: f32,
    label: Option<String>,
    area: bool,
}

#[derive(Component)]
struct MapIcon(IconKey, IconSpec);

fn spawn_screen(mut commands: Commands) {
    commands
        .spawn((
            ScreenRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                column_gap: px(18),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.82)),
            // Above the deploy screen: a dead commander keeps commanding.
            GlobalZIndex(11),
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                ScreenMap,
                Name::new("commander:map"),
                Node {
                    width: vh(90),
                    height: vh(90),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(10)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                RelativeCursorPosition::default(),
                BackgroundColor(Color::srgb(0.14, 0.15, 0.16)),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
            ))
            .with_child((
                TargetRing,
                Node {
                    position_type: PositionType::Absolute,
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                BackgroundColor(Color::NONE),
                BorderColor::all(Color::WHITE),
                FocusPolicy::Pass,
                GlobalZIndex(13),
                Visibility::Hidden,
            ));
            root.spawn((
                Node {
                    width: px(340),
                    height: vh(90),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8),
                    padding: UiRect::all(px(16)),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.05, 0.06, 0.08, 0.94)),
            ))
            .with_children(|side| {
                side.spawn((Text::new("COMMANDER"), font(22.0), TextColor(GOLD)));
                side.spawn((
                    Text::new("Caps Lock or Esc closes the screen."),
                    font(12.0),
                    TextColor(DIM),
                ));
                side.spawn((
                    ScreenPanel,
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(6),
                        margin: UiRect::top(px(6)),
                        flex_grow: 1.0,
                        ..default()
                    },
                ));
                side.spawn((
                    ScreenHint,
                    Text::new(""),
                    font(13.0),
                    TextColor(TEXT),
                    Node {
                        padding: UiRect::all(px(8)),
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.05)),
                ));
            });
        });
}

fn set_map_image(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    asset_server: Res<AssetServer>,
    map: Single<Entity, With<ScreenMap>>,
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

fn show_screen(screen: Res<CommanderScreen>, mut root: Single<&mut Visibility, With<ScreenRoot>>) {
    root.set_if_neq(if screen.open { Visibility::Inherited } else { Visibility::Hidden });
}

/// Our team and its assets.
fn our_assets<'a>(local: &Query<&Team, With<LocalPlayer>>, assets: &'a Query<&TeamAssets>) -> (Team, Option<&'a TeamAssets>) {
    let team = local.single().copied().unwrap_or_default();
    (team, assets.iter().find(|a| a.team == team))
}

fn press_buttons(
    buttons: Query<(&Interaction, &ScreenButton), Changed<Interaction>>,
    local: Query<&Team, With<LocalPlayer>>,
    team_assets: Query<&TeamAssets>,
    mut screen: ResMut<CommanderScreen>,
    mut requests: MessageWriter<CommanderRequest>,
) {
    let (_, assets) = our_assets(&local, &team_assets);
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed || !screen.open {
            continue;
        }
        match *button {
            ScreenButton::Squad(squad) => {
                screen.squad = if screen.squad == Some(squad) { None } else { Some(squad) };
                if matches!(screen.tool, Some(Tool::Order(_))) {
                    screen.tool = None;
                }
            }
            ScreenButton::Order(kind) => {
                if screen.squad.is_some() {
                    screen.tool = if screen.tool == Some(Tool::Order(kind)) { None } else { Some(Tool::Order(kind)) };
                }
            }
            ScreenButton::Cancel => {
                if let Some(squad) = screen.squad {
                    requests.write(CommanderRequest::CancelOrder { squad });
                }
            }
            ScreenButton::Asset(asset) => {
                if !assets.is_some_and(|a| a.get(asset).ready()) {
                    continue;
                }
                if !asset.targeted() {
                    requests.write(CommanderRequest::Use {
                        asset,
                        target: Vec3::ZERO,
                    });
                } else {
                    screen.tool = if screen.tool == Some(Tool::Asset(asset)) { None } else { Some(Tool::Asset(asset)) };
                }
            }
            ScreenButton::Resign => {
                requests.write(CommanderRequest::Resign);
            }
            ScreenButton::Close => screen.open = false,
        }
    }
}

/// Meters the target ring covers for a tool (0: a small fixed ring).
fn tool_radius(tool: Tool, assets: &CommanderAssets) -> f32 {
    let desc = &assets.desc;
    match tool {
        Tool::Order(_) => 0.0,
        Tool::Asset(Asset::Artillery) => desc.artillery.spread + desc.artillery.radius,
        Tool::Asset(Asset::Uav) => desc.uav.radius,
        Tool::Asset(Asset::Supply) => desc.supply.radius,
        Tool::Asset(Asset::Scan) => 0.0,
    }
}

fn tool_color(tool: Tool) -> Color {
    match tool {
        Tool::Order(kind) => order_color(kind),
        Tool::Asset(_) => GOLD,
    }
}

/// Clicks on the map: give the order or call in the asset that was picked, or else pick the
/// squad of the soldier clicked. Also moves the target ring with the mouse.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn click_map(
    mouse: Res<ButtonInput<MouseButton>>,
    mut screen: ResMut<CommanderScreen>,
    map: Single<&RelativeCursorPosition, With<ScreenMap>>,
    level: Option<Res<LoadedLevel>>,
    assets: Res<CommanderAssets>,
    local: Query<&Team, With<LocalPlayer>>,
    members: Query<(&Team, &SquadMember)>,
    soldiers: Query<(&SoldierRender, &ControlledBy), With<Soldier>>,
    mut ring: Single<(&mut Node, &mut Visibility, &mut BorderColor), With<TargetRing>>,
    mut requests: MessageWriter<CommanderRequest>,
) {
    let (node, visibility, border) = &mut *ring;
    let Some(level) = level.filter(|_| screen.open) else {
        screen.click = None;
        visibility.set_if_neq(Visibility::Hidden);
        return;
    };
    if mouse.just_pressed(MouseButton::Right) {
        screen.tool = None;
    }
    let under_mouse = map.normalized.filter(|_| map.cursor_over).map(|n| n + Vec2::splat(0.5));

    // The target ring.
    match (screen.tool, under_mouse) {
        (Some(tool), Some(uv)) => {
            let radius = tool_radius(tool, &assets) / map_size(&level);
            let (size, offset) = if radius > 0.004 {
                (percent(radius * 200.0), percent(-radius * 100.0))
            } else {
                (px(18), px(-9))
            };
            node.left = percent(uv.x * 100.0);
            node.top = percent(uv.y * 100.0);
            node.width = size;
            node.height = size;
            node.margin = UiRect {
                left: offset,
                top: offset,
                ..default()
            };
            **border = BorderColor::all(tool_color(tool));
            visibility.set_if_neq(Visibility::Inherited);
        }
        _ => {
            visibility.set_if_neq(Visibility::Hidden);
        }
    }

    let clicked = screen.click.take().or_else(|| {
        under_mouse
            .filter(|_| mouse.just_pressed(MouseButton::Left))
            .map(|uv| map_point(&level, uv))
    });
    let Some(target) = clicked else {
        return;
    };
    match (screen.tool, screen.squad) {
        (Some(Tool::Order(kind)), Some(squad)) => {
            requests.write(CommanderRequest::Order { squad, kind, target });
            screen.tool = None;
        }
        (Some(Tool::Asset(asset)), _) => {
            requests.write(CommanderRequest::Use { asset, target });
            screen.tool = None;
        }
        _ => {
            // Pick the squad of the nearest soldier of ours.
            let team = local.single().copied().unwrap_or_default();
            let uv = map_uv(&level, target);
            let nearest = soldiers
                .iter()
                .filter_map(|(render, controlled_by)| {
                    let (t, member) = members.get(controlled_by.0).ok()?;
                    (*t == team).then(|| (member.squad, map_uv(&level, render.position).distance(uv)))
                })
                .filter(|(_, d)| *d < PICK_RADIUS)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((squad, _)) = nearest {
                screen.squad = Some(squad);
            }
        }
    }
}

fn button(
    parent: &mut ChildSpawnerCommands,
    action: ScreenButton,
    name: String,
    selected: Option<Color>,
    enabled: bool,
    content: impl FnOnce(&mut ChildSpawnerCommands),
) {
    parent
        .spawn((
            action,
            Button,
            Name::new(name),
            Node {
                padding: UiRect::axes(px(10), px(5)),
                border: UiRect::all(px(1.5)),
                border_radius: BorderRadius::all(px(6)),
                column_gap: px(8),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                ..default()
            },
            BackgroundColor(match selected {
                Some(color) => color.with_alpha(0.28),
                None if enabled => BUTTON,
                None => Color::srgba(1.0, 1.0, 1.0, 0.03),
            }),
            BorderColor::all(selected.unwrap_or(Color::NONE)),
        ))
        .with_children(content);
}

fn heading(parent: &mut ChildSpawnerCommands, text: &str) {
    parent.spawn((
        Text::new(text),
        font(13.0),
        TextColor(DIM),
        Node {
            margin: UiRect::top(px(8)),
            ..default()
        },
    ));
}

/// The side panel: squads, orders, assets; rebuilt when any of it changes.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn rebuild_panel(
    mut commands: Commands,
    screen: Res<CommanderScreen>,
    local: Query<&Team, With<LocalPlayer>>,
    players: Query<(&Player, &Team, &SquadMember)>,
    orders: Query<&SquadOrder>,
    team_assets: Query<&TeamAssets>,
    assets: Res<CommanderAssets>,
    panel: Single<(Entity, Option<&Children>), With<ScreenPanel>>,
    mut hint: Single<&mut Text, With<ScreenHint>>,
    mut built: Local<String>,
) {
    if !screen.open {
        built.clear();
        return;
    }
    let (team, status) = our_assets(&local, &team_assets);
    let status = status.map(|s| s.status).unwrap_or_default();
    // Squads: number, members, leader, order.
    let mut squads: Vec<(u8, usize, String, Option<OrderKind>)> = Vec::new();
    for (player, player_team, member) in &players {
        if *player_team != team {
            continue;
        }
        let entry = match squads.iter_mut().find(|s| s.0 == member.squad) {
            Some(entry) => entry,
            None => {
                let order = orders.iter().find(|o| o.team == team && o.squad == member.squad).map(|o| o.kind);
                squads.push((member.squad, 0, String::new(), order));
                squads.last_mut().unwrap()
            }
        };
        entry.1 += 1;
        if member.leader {
            entry.2 = player.name.clone();
        }
    }
    squads.sort_by_key(|s| s.0);
    let squad = screen.squad.filter(|s| squads.iter().any(|q| q.0 == *s));

    let text = match (screen.tool, squad) {
        (Some(Tool::Order(kind)), Some(squad)) => format!(
            "Click the map: {} {} there. Right-click cancels.",
            squad_name(squad),
            match kind {
                OrderKind::Attack => "attacks",
                OrderKind::Defend => "defends",
                OrderKind::Move => "moves",
            }
        ),
        (Some(Tool::Asset(Asset::Artillery)), _) => {
            let guns = assets
                .of(team, game_data::AssetKind::Artillery)
                .count();
            format!("Click the target: {guns} gun(s) fire {} shells each. Right-click cancels.", assets.desc.artillery.shells)
        }
        (Some(Tool::Asset(Asset::Uav)), _) => "Click where the UAV should circle: it shows the enemies below it.".into(),
        (Some(Tool::Asset(Asset::Supply)), _) => "Click where the supply crate should land.".into(),
        (_, None) => "Pick a squad (in the list or on the map), then an order and a spot on the map. Or call in an asset.".into(),
        (_, Some(squad)) => format!("{} picked: give it an order.", squad_name(squad)),
    };
    if hint.0 != text {
        hint.0 = text;
    }

    let key = format!("{squads:?}{squad:?}{:?}{status:?}", screen.tool);
    if *built == key {
        return;
    }
    *built = key;
    let (panel, children) = *panel;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    commands.entity(panel).with_children(|panel| {
        heading(panel, "SQUADS");
        if squads.is_empty() {
            panel.spawn((Text::new("Your team has no squads."), font(13.0), TextColor(DIM)));
        }
        for (number, count, leader, order) in &squads {
            let selected = (squad == Some(*number)).then_some(SQUAD);
            button(panel, ScreenButton::Squad(*number), format!("cmd:squad:{number}"), selected, true, |row| {
                row.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    ..default()
                })
                .with_children(|names| {
                    names.spawn((Text::new(format!("{}  {count}/6", squad_name(*number))), font(14.0), TextColor(TEXT)));
                    names.spawn((Text::new(leader.clone()), font(11.0), TextColor(DIM)));
                });
                if let Some(order) = order {
                    row.spawn((Text::new(order.label().to_uppercase()), font(12.0), TextColor(order_color(*order))));
                }
            });
        }

        heading(
            panel,
            &match squad {
                Some(squad) => format!("ORDERS FOR {}", squad_name(squad).to_uppercase()),
                None => "ORDERS".into(),
            },
        );
        panel
            .spawn(Node {
                column_gap: px(6),
                flex_wrap: FlexWrap::Wrap,
                row_gap: px(6),
                ..default()
            })
            .with_children(|row| {
                for kind in OrderKind::ALL {
                    let selected = (screen.tool == Some(Tool::Order(kind))).then_some(order_color(kind));
                    let color = if squad.is_some() { order_color(kind) } else { DIM };
                    let name = format!("cmd:order:{}", kind.label().to_lowercase());
                    button(row, ScreenButton::Order(kind), name, selected, squad.is_some(), |b| {
                        b.spawn((Text::new(kind.label()), font(13.0), TextColor(color)));
                    });
                }
                let has_order = squad.is_some_and(|s| squads.iter().any(|q| q.0 == s && q.3.is_some()));
                button(row, ScreenButton::Cancel, "cmd:cancel".into(), None, has_order, |b| {
                    b.spawn((Text::new("Cancel"), font(13.0), TextColor(if has_order { TEXT } else { DIM })));
                });
            });

        heading(panel, "ASSETS");
        for asset in Asset::ALL {
            let state = status[asset.index()];
            let selected = (screen.tool == Some(Tool::Asset(asset))).then_some(GOLD);
            let (line, color) = if !state.intact {
                ("Destroyed".to_string(), ENEMY)
            } else if state.recharge > 0 {
                (format!("{} s", state.recharge), DIM)
            } else {
                ("Ready".to_string(), READY)
            };
            let name = format!("cmd:asset:{}", format!("{asset:?}").to_lowercase());
            button(panel, ScreenButton::Asset(asset), name, selected, state.ready(), |row| {
                row.spawn((
                    Text::new(asset.label()),
                    font(14.0),
                    TextColor(if state.ready() { TEXT } else { DIM }),
                ));
                row.spawn((Text::new(line), font(12.0), TextColor(color)));
            });
        }

        panel.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        panel
            .spawn(Node {
                column_gap: px(6),
                ..default()
            })
            .with_children(|row| {
                button(row, ScreenButton::Resign, "cmd:resign".into(), None, true, |b| {
                    b.spawn((Text::new("Resign"), font(13.0), TextColor(TEXT)));
                });
                button(row, ScreenButton::Close, "cmd:close".into(), None, true, |b| {
                    b.spawn((Text::new("Close"), font(13.0), TextColor(TEXT)));
                });
            });
    });
}

fn spawn_icon(commands: &mut Commands, map: Entity, level: &LoadedLevel, key: IconKey, spec: IconSpec) {
    let uv = map_uv(level, spec.position).clamp(Vec2::ZERO, Vec2::ONE);
    let (size, offset) = if spec.area {
        let radius = spec.size / map_size(level);
        (percent(radius * 200.0), percent(-radius * 100.0))
    } else {
        (px(spec.size), px(-spec.size / 2.0))
    };
    let mut icon = commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: percent(uv.x * 100.0),
            top: percent(uv.y * 100.0),
            width: size,
            height: size,
            margin: UiRect {
                left: offset,
                top: offset,
                ..default()
            },
            border: UiRect::all(px(if spec.area { 2.0 } else { 1.5 })),
            border_radius: BorderRadius::MAX,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(if spec.area { spec.color.with_alpha(0.14) } else { spec.color }),
        BorderColor::all(if spec.area { spec.color } else { Color::srgba(0.0, 0.0, 0.0, 0.7) }),
        FocusPolicy::Pass,
        ChildOf(map),
    ));
    if let Some(label) = &spec.label {
        let (top, width) = if spec.area { (percent(100), px(160)) } else { (px(spec.size + 2.0), px(160)) };
        icon.with_child((
            Text::new(label.clone()),
            font(12.0),
            TextColor(TEXT),
            TextShadow {
                offset: Vec2::splat(1.0),
                color: Color::srgba(0.0, 0.0, 0.0, 0.9),
            },
            TextLayout::justify(Justify::Center),
            Node {
                position_type: PositionType::Absolute,
                top,
                width,
                justify_content: JustifyContent::Center,
                ..default()
            },
            FocusPolicy::Pass,
        ));
    }
    icon.insert(MapIcon(key, spec));
}

/// The map's icons, while the screen is open.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_map(
    mut commands: Commands,
    screen: Res<CommanderScreen>,
    level: Option<Res<LoadedLevel>>,
    local: Query<&Team, With<LocalPlayer>>,
    members: Query<(&Team, Option<&SquadMember>)>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy, Has<LocalSoldier>), With<Soldier>>,
    effects: Query<(Entity, &AssetEffect)>,
    markers: Res<MapMarkers>,
    map: Single<Entity, With<ScreenMap>>,
    mut icons: Query<(Entity, &MapIcon, &mut Node, &mut BackgroundColor)>,
) {
    let Some(level) = level.filter(|_| screen.open) else {
        return;
    };
    let team = local.single().copied().unwrap_or_default();
    let mut wanted: HashMap<IconKey, IconSpec> = HashMap::default();
    let dot = |position, color, size, label: Option<String>| IconSpec {
        position,
        color,
        size,
        label,
        area: false,
    };
    for (entity, cp, state) in &control_points {
        wanted.insert(
            IconKey::Entity(entity),
            dot(cp.position, team_color(state.owner, team), 16.0, Some(cp.name.clone())),
        );
    }
    for (entity, render, controlled_by, local_soldier) in &soldiers {
        let Ok((soldier_team, member)) = members.get(controlled_by.0) else {
            continue;
        };
        if *soldier_team != team || team == Team::Spectator {
            continue;
        }
        let spec = match member {
            _ if local_soldier => dot(render.position, GOLD, 10.0, Some("You".into())),
            Some(member) => {
                let color = if screen.squad == Some(member.squad) { SQUAD } else { FRIENDLY };
                let label = member.leader.then(|| squad_name(member.squad).to_string());
                dot(render.position, color, if member.leader { 11.0 } else { 8.0 }, label)
            }
            None => dot(render.position, NEUTRAL, 7.0, None),
        };
        wanted.insert(IconKey::Entity(entity), spec);
    }
    for marker in &markers.0 {
        wanted.insert(
            IconKey::Entity(marker.key),
            dot(marker.position, marker.color, marker.size + 3.0, marker.label.clone()),
        );
    }
    for (entity, effect) in &effects {
        if effect.team != team || effect.asset == Asset::Scan {
            continue;
        }
        let ground = Vec3::new(effect.position.x, 0.0, effect.position.z);
        wanted.insert(
            IconKey::Area(entity),
            IconSpec {
                position: ground,
                color: asset_color(effect.asset),
                size: effect.radius.max(4.0),
                label: None,
                area: true,
            },
        );
    }

    for (icon_entity, icon, mut node, mut background) in &mut icons {
        match wanted.remove(&icon.0) {
            Some(spec) if spec.label == icon.1.label && spec.size == icon.1.size && spec.area == icon.1.area => {
                let uv = map_uv(&level, spec.position).clamp(Vec2::ZERO, Vec2::ONE);
                node.left = percent(uv.x * 100.0);
                node.top = percent(uv.y * 100.0);
                if !spec.area {
                    background.0 = spec.color;
                }
            }
            Some(spec) => {
                commands.entity(icon_entity).despawn();
                spawn_icon(&mut commands, *map, &level, icon.0, spec);
            }
            None => {
                commands.entity(icon_entity).despawn();
            }
        }
    }
    for (key, spec) in wanted {
        spawn_icon(&mut commands, *map, &level, key, spec);
    }
}
