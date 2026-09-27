//! Orders and assets where players see them: our squad's order on the HUD (with its
//! distance) and on the maps, every squad's order on the commander's maps, our team's asset
//! objects (standing or destroyed) and what the commander called in (artillery target, UAV,
//! supply crate) on the maps. The commander hears when an asset is ready again or lost.

use bevy::prelude::*;
use game_data::AssetKind;
use game_shared::{
    chat::{ChatChannel, ChatLine},
    commander::{Asset, AssetEffect, Commander, CommanderAssets, OrderKind, SquadOrder, TeamAssets},
    protocol::Team,
    squad::{SquadMember, squad_name},
    statics::DestroyedStatics,
};

use super::GOLD;
use crate::{
    camera::PlayerCamera,
    map_markers::{MapMarker, MapMarkers, MarkerSystems},
    net::LocalPlayer,
};

pub struct MarkersPlugin;

impl Plugin for MarkersPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AssetKeys>()
            .add_systems(Startup, spawn_root)
            .add_systems(Update, asset_news)
            .add_systems(PostUpdate, (map_markers, hud_marker).in_set(MarkerSystems));
    }
}

pub fn order_color(kind: OrderKind) -> Color {
    match kind {
        OrderKind::Attack => Color::srgb(1.0, 0.45, 0.2),
        OrderKind::Defend => Color::srgb(0.35, 0.72, 1.0),
        OrderKind::Move => Color::srgb(1.0, 0.88, 0.3),
    }
}

pub fn asset_color(asset: Asset) -> Color {
    match asset {
        Asset::Artillery => Color::srgb(1.0, 0.4, 0.25),
        Asset::Uav => Color::srgb(0.55, 0.8, 1.0),
        Asset::Scan => Color::srgb(0.55, 0.8, 1.0),
        Asset::Supply => Color::srgb(0.5, 0.92, 0.45),
    }
}

const DESTROYED: Color = Color::srgb(0.42, 0.43, 0.45);
/// The HUD's order marker hangs this high above the ordered spot (meters).
const ABOVE: f32 = 3.0;

/// Entities standing for the level's asset objects on the maps.
#[derive(Resource, Default)]
struct AssetKeys(Vec<Entity>);

#[derive(Component)]
struct OrderRoot;
#[derive(Component)]
struct OrderIcon;
#[derive(Component)]
struct OrderText;

fn kind_name(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Artillery => "Artillery",
        AssetKind::Uav => "UAV trailer",
        AssetKind::Radar => "Radar",
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn map_markers(
    mut commands: Commands,
    local: Query<(&Team, Option<&SquadMember>, Has<Commander>), With<LocalPlayer>>,
    orders: Query<(Entity, &SquadOrder)>,
    effects: Query<(Entity, &AssetEffect)>,
    assets: Res<CommanderAssets>,
    destroyed: Query<&DestroyedStatics>,
    mut keys: ResMut<AssetKeys>,
    mut markers: ResMut<MapMarkers>,
) {
    if keys.0.len() != assets.instances.len() {
        for key in keys.0.drain(..) {
            commands.entity(key).despawn();
        }
        keys.0 = assets.instances.iter().map(|_| commands.spawn_empty().id()).collect();
    }
    let Ok((&team, squad, commander)) = local.single() else {
        return;
    };
    if team == Team::Spectator {
        return;
    }
    for (entity, order) in &orders {
        if order.team != team {
            continue;
        }
        let label = if commander {
            format!("{}: {}", squad_name(order.squad), order.kind.label())
        } else if squad.is_some_and(|s| s.squad == order.squad) {
            order.kind.label().to_string()
        } else {
            continue;
        };
        markers.0.push(MapMarker {
            key: entity,
            position: order.position,
            color: order_color(order.kind),
            size: 10.0,
            label: Some(label),
        });
    }
    for (entity, effect) in &effects {
        if effect.team != team {
            continue;
        }
        let label = match effect.asset {
            Asset::Artillery => "Artillery strike",
            Asset::Uav => "UAV",
            Asset::Supply => "Supplies",
            Asset::Scan => continue,
        };
        markers.0.push(MapMarker {
            key: entity,
            position: effect.position,
            color: asset_color(effect.asset),
            size: 9.0,
            label: Some(label.into()),
        });
    }
    let destroyed = destroyed.single().ok();
    for (asset, key) in assets.instances.iter().zip(&keys.0) {
        if asset.team != team {
            continue;
        }
        let down = destroyed.is_some_and(|d| d.0.contains(&asset.instance));
        markers.0.push(MapMarker {
            key: *key,
            position: Vec3::from_array(asset.placement.position),
            color: if down { DESTROYED } else { GOLD },
            size: 8.0,
            label: Some(if down {
                format!("{} (destroyed)", kind_name(asset.kind))
            } else {
                kind_name(asset.kind).to_string()
            }),
        });
    }
}

fn spawn_root(mut commands: Commands) {
    commands
        .spawn((
            OrderRoot,
            Node {
                position_type: PositionType::Absolute,
                width: px(120),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(3),
                ..default()
            },
            Visibility::Hidden,
            Pickable::IGNORE,
        ))
        .with_children(|root| {
            root.spawn((
                OrderIcon,
                Node {
                    width: px(16),
                    height: px(16),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                UiTransform::from_rotation(Rot2::degrees(45.0)),
                BackgroundColor(Color::NONE),
                BorderColor::all(Color::WHITE),
            ));
            root.spawn((
                OrderText,
                Text::new(""),
                TextFont {
                    font_size: FontSize::Px(13.0),
                    ..default()
                },
                TextColor(Color::WHITE),
                TextShadow {
                    offset: Vec2::splat(1.0),
                    color: Color::srgba(0.0, 0.0, 0.0, 0.9),
                },
            ));
        });
}

/// Our squad's order on the HUD: a marker over the spot with the order and its distance.
#[allow(clippy::type_complexity)]
fn hud_marker(
    local: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    orders: Query<&SquadOrder>,
    camera: Single<(&Camera, &GlobalTransform), With<PlayerCamera>>,
    mut root: Single<(&mut Node, &mut Visibility), With<OrderRoot>>,
    mut icon: Single<(&mut BorderColor, &mut BackgroundColor), With<OrderIcon>>,
    mut text: Single<(&mut Text, &mut TextColor), With<OrderText>>,
) {
    let (node, visibility) = &mut *root;
    let order = local.single().ok().and_then(|(team, squad)| {
        let squad = squad?;
        orders.iter().find(|o| o.team == *team && o.squad == squad.squad)
    });
    let (camera, view) = *camera;
    let Some((order, at)) = order.and_then(|o| {
        let at = camera.world_to_viewport(view, o.position + Vec3::Y * ABOVE).ok()?;
        Some((o, at))
    }) else {
        visibility.set_if_neq(Visibility::Hidden);
        return;
    };
    visibility.set_if_neq(Visibility::Inherited);
    node.left = px(at.x - 60.0);
    node.top = px(at.y - 10.0);
    let color = order_color(order.kind);
    let (border, background) = &mut *icon;
    **border = BorderColor::all(color);
    background.0 = color.with_alpha(0.3);
    let distance = order.position.distance(view.translation());
    let line = format!("{}  {distance:.0} m", order.kind.label().to_uppercase());
    let (text, text_color) = &mut *text;
    if text.0 != line {
        text.0 = line;
    }
    text_color.0 = color;
}

/// Tells the commander when an asset is ready again, lost or back.
fn asset_news(
    local: Query<&Team, (With<LocalPlayer>, With<Commander>)>,
    team_assets: Query<&TeamAssets, Changed<TeamAssets>>,
    mut known: Local<Option<(Team, TeamAssets)>>,
    mut chat: MessageWriter<ChatLine>,
) {
    let Ok(&team) = local.single() else {
        *known = None;
        return;
    };
    let Some(current) = team_assets.iter().find(|a| a.team == team).copied() else {
        return;
    };
    if let Some((known_team, before)) = known.as_ref()
        && *known_team == team
    {
        for asset in Asset::ALL {
            let (was, now) = (before.get(asset), current.get(asset));
            let news = if was.intact && !now.intact {
                format!("{} is out: its {} was destroyed.", asset.label(), asset.object().map_or("", kind_name))
            } else if !was.intact && now.intact {
                format!("{} is back.", asset.label())
            } else if !was.ready() && now.ready() {
                format!("{} ready.", asset.label())
            } else {
                continue;
            };
            chat.write(ChatLine {
                channel: ChatChannel::Private,
                sender: None,
                team,
                text: news,
            });
        }
    }
    *known = Some((team, current));
}
