//! Man down and kit abilities on the client: the overlay while we are critically wounded
//! (waiting for a medic, the time left, giving up), the camera over our body meanwhile,
//! markers over downed teammates, notices for healing, resupplying, repairing, reviving and
//! kill assists, and the state of whoever the gadget in our hands works on. The rules are
//! the server's (`game_server::abilities`).

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use game_data::ReplenishKind;
use game_shared::{
    physics::GameLayer,
    protocol::{ControlledBy, Player, Team},
    revive::{Downed, GiveUp, MAN_DOWN_SECONDS, NoticeKind, ReplenishNotice},
    soldier::{Health, Soldier},
    vehicle::{VehicleData, VehicleHealth},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    camera::{CameraSystems, PlayerCamera},
    combat::CombatFeedback,
    local_input::LookState,
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
    settings::{Action, Actions},
};

pub struct WoundedPlugin;

impl Plugin for WoundedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Notices>()
            .add_systems(Startup, spawn_ui)
            .add_systems(
                Update,
                (give_up, receive_notices, update_overlay, update_markers, update_notices, update_target),
            )
            .add_systems(
                PostUpdate,
                man_down_camera
                    .after(CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// The color of the critically wounded: overlay, markers and the minimap.
pub const WOUNDED: Color = Color::srgb(1.0, 0.42, 0.3);
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.7);
const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.6);
const SCORE: Color = Color::srgb(0.95, 0.75, 0.3);
const GOOD: Color = Color::srgb(0.45, 0.9, 0.5);
/// Markers over downed teammates show up to this far away (farther for medics).
const MARKER_RANGE: f32 = 120.0;
const MEDIC_MARKER_RANGE: f32 = 300.0;
const MARKER_WIDTH: f32 = 140.0;
/// Seconds a notice stays, the last of them fading out.
const NOTICE_SECONDS: f32 = 4.0;
const NOTICE_FADE: f32 = 1.0;
const MAX_NOTICES: usize = 5;
/// A notice like the last one within this many seconds adds to it.
const MERGE_SECONDS: f32 = 2.5;
/// Width of the bleeding-out bar, pixels.
const BAR_WIDTH: f32 = 320.0;
/// The man-down camera: distance from the body, and how far above it may look from.
const DOWN_CAMERA_DISTANCE: f32 = 2.8;

#[derive(Component)]
struct Overlay;
#[derive(Component)]
struct OverlayStatus;
#[derive(Component)]
struct OverlayBar;
#[derive(Component)]
struct OverlayTime;
#[derive(Component)]
struct OverlayHint;
#[derive(Component)]
struct MarkerRoot;
#[derive(Component)]
struct Marker(Entity);
#[derive(Component)]
struct MarkerText;
#[derive(Component)]
struct NoticeList;
#[derive(Component)]
struct NoticeLine(usize);
#[derive(Component)]
struct TargetPanel;
#[derive(Component)]
struct TargetText;
#[derive(Component)]
struct TargetFill;

/// A line of [`Notices`].
struct Notice {
    key: (NoticeKind, bool, Option<Entity>),
    amount: f32,
    score: i32,
    text: String,
    color: Color,
    /// Seconds shown.
    age: f32,
}

/// Recent notices, oldest first.
#[derive(Resource, Default)]
struct Notices {
    lines: VecDeque<Notice>,
    changed: bool,
}

fn font(size: f32) -> TextFont {
    TextFont {
        font_size: FontSize::Px(size),
        ..default()
    }
}

fn shadow() -> TextShadow {
    TextShadow {
        offset: Vec2::splat(1.0),
        color: Color::srgba(0.0, 0.0, 0.0, 0.8),
    }
}

fn spawn_ui(mut commands: Commands) {
    // Markers over downed teammates, under everything else.
    commands.spawn((
        MarkerRoot,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
    ));

    // Wounded: a red tint over the view and a card at the bottom.
    commands
        .spawn((
            Overlay,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::FlexEnd,
                align_items: AlignItems::Center,
                padding: UiRect::bottom(percent(14)),
                ..default()
            },
            // Blood-red at the edges of the view.
            BackgroundGradient::from(RadialGradient::new(
                UiPosition::CENTER,
                RadialGradientShape::FarthestCorner,
                vec![
                    ColorStop::percent(Color::srgba(0.4, 0.0, 0.0, 0.1), 25),
                    ColorStop::percent(Color::srgba(0.35, 0.0, 0.0, 0.7), 100),
                ],
            )),
            Visibility::Hidden,
        ))
        .with_children(|overlay| {
            overlay
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::Center,
                        row_gap: px(8),
                        padding: UiRect::axes(px(28), px(18)),
                        border_radius: BorderRadius::all(px(10)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ))
                .with_children(|card| {
                    card.spawn((Text::new("CRITICALLY WOUNDED"), font(30.0), TextColor(WOUNDED), shadow()));
                    card.spawn((OverlayStatus, Text::new(""), font(17.0), TextColor(TEXT), shadow()));
                    card.spawn((
                        Node {
                            width: px(BAR_WIDTH),
                            height: px(6),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                    ))
                    .with_child((
                        OverlayBar,
                        Node {
                            width: px(BAR_WIDTH),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(3)),
                            ..default()
                        },
                        BackgroundColor(WOUNDED),
                    ));
                    card.spawn((OverlayTime, Text::new(""), font(14.0), TextColor(DIM), shadow()));
                    card.spawn((OverlayHint, Text::new(""), font(14.0), TextColor(DIM), shadow()));
                });
        });

    // Notices, under the crosshair.
    commands.spawn((
        NoticeList,
        Node {
            position_type: PositionType::Absolute,
            top: percent(62),
            width: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(2),
            ..default()
        },
    ));

    // What the gadget in hand works on: a name and a bar, above the notices.
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: percent(55),
                width: percent(100),
                justify_content: JustifyContent::Center,
                ..default()
            },
        ))
        .with_children(|row| {
            row.spawn((
                TargetPanel,
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: px(4),
                    padding: UiRect::axes(px(12), px(6)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                BackgroundColor(PANEL),
                Visibility::Hidden,
            ))
            .with_children(|panel| {
                panel.spawn((TargetText, Text::new(""), font(14.0), TextColor(TEXT), shadow()));
                panel
                    .spawn((
                        Node {
                            width: px(160),
                            height: px(5),
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                    ))
                    .with_child((
                        TargetFill,
                        Node {
                            width: percent(100),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(GOOD),
                    ));
            });
        });
}

/// The give-up key while critically wounded (not while typing in the chat).
fn give_up(
    actions: Actions,
    chat: Res<crate::chat::ChatBox>,
    soldier: Query<(), (With<LocalSoldier>, With<Downed>)>,
    mut requests: MessageWriter<GiveUp>,
) {
    if !soldier.is_empty() && chat.typing.is_none() && actions.just_pressed(Action::GiveUp) {
        info!("giving up");
        requests.write(GiveUp);
    }
}

fn name_of(players: &Query<&Player>, player: Option<Entity>) -> String {
    player
        .and_then(|p| players.get(p).ok())
        .map_or_else(|| "someone".to_string(), |p| p.name.clone())
}

fn receive_notices(
    mut messages: MessageReader<ReplenishNotice>,
    players: Query<&Player>,
    mut notices: ResMut<Notices>,
) {
    for notice in messages.read() {
        // Healing that goes on adds up in one line.
        let key = (notice.kind, notice.received, notice.other);
        let merge = notice.kind != NoticeKind::Revive
            && notices.lines.back().is_some_and(|last| last.key == key && last.age < MERGE_SECONDS);
        let (amount, score) = match merge.then(|| notices.lines.pop_back()).flatten() {
            Some(last) => (last.amount + notice.amount, last.score + notice.score),
            None => (notice.amount, notice.score),
        };
        let other = name_of(&players, notice.other);
        let shown = amount.round();
        let (text, color) = match (notice.kind, notice.received, notice.other.is_some()) {
            (NoticeKind::Heal, true, false) => (format!("Healed  +{shown} HP"), GOOD),
            (NoticeKind::Heal, true, true) => (format!("{other} healed you  +{shown} HP"), GOOD),
            (NoticeKind::Heal, false, _) => (format!("Healed {other}  +{shown} HP"), TEXT),
            (NoticeKind::Resupply, true, false) => (format!("Resupplied  +{shown}%"), GOOD),
            (NoticeKind::Resupply, true, true) => (format!("{other} resupplied you  +{shown}%"), GOOD),
            (NoticeKind::Resupply, false, _) => (format!("Resupplied {other}  +{shown}%"), TEXT),
            (NoticeKind::Repair, _, true) => (format!("Repaired {other}'s vehicle  +{shown}"), TEXT),
            (NoticeKind::Repair, _, false) => (format!("Repaired  +{shown}"), TEXT),
            (NoticeKind::Revive, true, _) => (format!("Revived by {other}"), GOOD),
            (NoticeKind::Revive, false, _) => (format!("Revived {other}"), TEXT),
            (NoticeKind::KillAssist, ..) => (format!("Kill assist: {other}"), TEXT),
        };
        notices.lines.push_back(Notice {
            key,
            amount,
            score,
            text,
            color,
            age: 0.0,
        });
        while notices.lines.len() > MAX_NOTICES {
            notices.lines.pop_front();
        }
        notices.changed = true;
    }
}

fn update_notices(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut notices: ResMut<Notices>,
    list: Single<Entity, With<NoticeList>>,
    mut lines: Query<(&NoticeLine, &mut TextColor)>,
) {
    let dt = time.delta_secs();
    let before = notices.lines.len();
    for line in &mut notices.lines {
        line.age += dt;
    }
    notices.lines.retain(|line| line.age < NOTICE_SECONDS);
    let color = |line: &Notice| {
        let alpha = ((NOTICE_SECONDS - line.age) / NOTICE_FADE).clamp(0.0, 1.0);
        if line.score > 0 { SCORE } else { line.color }.with_alpha(alpha)
    };
    if notices.changed || before != notices.lines.len() {
        notices.changed = false;
        commands.entity(*list).despawn_related::<Children>();
        for (index, line) in notices.lines.iter().enumerate() {
            let (score, text) = (line.score, &line.text);
            let text = if score != 0 { format!("{score:+}  {text}") } else { text.clone() };
            commands
                .entity(*list)
                .with_child((NoticeLine(index), Text::new(text), font(15.0), TextColor(color(line)), shadow()));
        }
        return;
    }
    // Fading out.
    for (line, mut text_color) in &mut lines {
        if let Some(notice) = notices.lines.get(line.0) {
            let faded = color(notice);
            if text_color.0 != faded {
                text_color.0 = faded;
            }
        }
    }
}

/// Changes a text only when it differs (a change lays the text out again).
fn set_text(text: &mut Text, value: String) {
    if text.0 != value {
        text.0 = value;
    }
}

/// Whether a soldier's kit is the medic's.
fn is_medic(armory: &Armory, loadout: &Loadout) -> bool {
    armory.kits.get(&loadout.kit).is_some_and(|k| k.kind.eq_ignore_ascii_case("medic"))
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_overlay(
    time: Res<Time<Real>>,
    actions: Actions,
    armory: Res<Armory>,
    feedback: Res<CombatFeedback>,
    local: Query<(Ref<Downed>, &SoldierRender), With<LocalSoldier>>,
    local_team: Query<&Team, With<LocalPlayer>>,
    soldiers: Query<(&SoldierRender, &ControlledBy, &Loadout), (With<Soldier>, Without<Downed>, Without<LocalSoldier>)>,
    teams: Query<&Team>,
    players: Query<&Player>,
    mut overlay: Single<&mut Visibility, With<Overlay>>,
    mut texts: ParamSet<(
        Single<&mut Text, With<OverlayStatus>>,
        Single<&mut Text, With<OverlayTime>>,
        Single<&mut Text, With<OverlayHint>>,
    )>,
    mut bar: Single<&mut Node, With<OverlayBar>>,
    deploy: Res<crate::deploy::DeployScreen>,
    mut left: Local<f32>,
) {
    let Ok((downed, body)) = local.single() else {
        overlay.set_if_neq(Visibility::Hidden);
        return;
    };
    // The deploy screen (open while waiting, to pick a kit) takes the view.
    overlay.set_if_neq(if deploy.open { Visibility::Hidden } else { Visibility::Inherited });
    // Whole seconds arrive when they tick over; count down smoothly in between.
    if downed.is_changed() {
        *left = downed.left;
    } else {
        *left = (*left - time.delta_secs()).max(downed.left - 1.0).max(0.0);
    }
    let team = local_team.single().ok().copied();
    let medic = soldiers
        .iter()
        .filter(|(_, owner, loadout)| teams.get(owner.0).ok().copied() == team && is_medic(&armory, loadout))
        .map(|(render, owner, _)| (render.position.distance(body.position), owner.0))
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let status = match medic {
        Some((distance, player)) if distance < MEDIC_MARKER_RANGE => {
            format!("Waiting for a medic: {} is {distance:.0} m away", name_of(&players, Some(player)))
        }
        _ => "Waiting for a medic: none nearby".to_string(),
    };
    let wounded_by = feedback.killed_by.as_deref().map_or(String::new(), |by| format!("Wounded by {by}.  "));
    let time_left = format!("{wounded_by}Bleeding out in {:.0} s", left.ceil());
    let hint = format!(
        "{} give up   |   {} deploy screen",
        actions.label(Action::GiveUp),
        actions.label(Action::Deploy)
    );
    set_text(&mut texts.p0(), status);
    set_text(&mut texts.p1(), time_left);
    set_text(&mut texts.p2(), hint);
    bar.width = px(BAR_WIDTH * (*left / MAN_DOWN_SECONDS).clamp(0.0, 1.0));
}

/// While we're down the camera looks at our body from nearby, turned with the mouse (BF2's
/// man-down camera).
fn man_down_camera(
    look: Res<LookState>,
    spatial: SpatialQuery,
    soldier: Query<&SoldierRender, (With<LocalSoldier>, With<Downed>)>,
    mut camera: Single<&mut Transform, With<PlayerCamera>>,
) {
    let Ok(body) = soldier.single() else {
        return;
    };
    let pivot = body.position + Vec3::Y * 0.4;
    // Looking down raises the camera; it never goes below the body.
    let elevation = (0.45 - look.pitch).clamp(0.2, 1.35);
    let direction = Quat::from_rotation_y(look.yaw) * Vec3::new(0.0, elevation.sin(), elevation.cos());
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    let distance = Dir3::new(direction)
        .ok()
        .and_then(|dir| spatial.cast_ray(pivot, dir, DOWN_CAMERA_DISTANCE, true, &filter))
        .map_or(DOWN_CAMERA_DISTANCE, |hit| (hit.distance - 0.2).max(0.4));
    let eye = pivot + direction * distance;
    **camera = Transform::from_translation(eye).looking_at(pivot, Vec3::Y);
}

/// A marker over every downed teammate: a cross, and for medics the distance and the time
/// left to reach him.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_markers(
    mut commands: Commands,
    armory: Res<Armory>,
    camera: Single<(&Camera, &GlobalTransform), With<PlayerCamera>>,
    local_team: Query<&Team, With<LocalPlayer>>,
    local: Query<&Loadout, With<LocalSoldier>>,
    downed: Query<(Entity, &SoldierRender, &ControlledBy, &Downed), Without<LocalSoldier>>,
    teams: Query<&Team>,
    root: Single<Entity, With<MarkerRoot>>,
    mut markers: Query<(Entity, &Marker, &mut Node, &mut Visibility, &Children)>,
    mut texts: Query<&mut Text, With<MarkerText>>,
) {
    let (camera, view) = *camera;
    let team = local_team.single().ok().copied();
    let medic = local.single().is_ok_and(|loadout| is_medic(&armory, loadout));
    let range = if medic { MEDIC_MARKER_RANGE } else { MARKER_RANGE };
    let mut wanted: Vec<(Entity, Vec2, f32, f32)> = Vec::new();
    for (soldier, render, owner, state) in &downed {
        if team.is_none() || teams.get(owner.0).ok().copied() != team {
            continue;
        }
        let point = render.position + Vec3::Y * 0.6;
        let distance = point.distance(view.translation());
        if distance > range {
            continue;
        }
        if let Ok(at) = camera.world_to_viewport(view, point) {
            wanted.push((soldier, at, distance, state.left));
        }
    }
    for (entity, marker, mut node, mut visibility, children) in &mut markers {
        let Some(index) = wanted.iter().position(|w| w.0 == marker.0) else {
            commands.entity(entity).despawn();
            continue;
        };
        let (_, at, distance, left) = wanted.swap_remove(index);
        node.left = px(at.x - MARKER_WIDTH / 2.0);
        node.top = px(at.y - 14.0);
        visibility.set_if_neq(Visibility::Inherited);
        for child in children.iter() {
            if let Ok(mut text) = texts.get_mut(child) {
                let line = if medic { format!("+\n{distance:.0} m  {left:.0} s") } else { "+".to_string() };
                if text.0 != line {
                    text.0 = line;
                }
            }
        }
    }
    for (soldier, at, ..) in wanted {
        commands.entity(*root).with_child((
            Marker(soldier),
            Node {
                position_type: PositionType::Absolute,
                left: px(at.x - MARKER_WIDTH / 2.0),
                top: px(at.y - 14.0),
                width: px(MARKER_WIDTH),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Visibility::Hidden,
            children![(
                MarkerText,
                Text::new("+"),
                font(18.0),
                TextColor(WOUNDED),
                TextLayout::justify(Justify::Center),
                shadow(),
            )],
        ));
    }
}

/// With a gadget in hand: who (or what) it works on, and how they're doing.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_target(
    armory: Res<Armory>,
    local: Query<(&SoldierRender, &Loadout, &Inventory), (With<LocalSoldier>, Without<Downed>)>,
    local_team: Query<&Team, With<LocalPlayer>>,
    soldiers: Query<
        (&SoldierRender, &ControlledBy, &Health, &Loadout, &Inventory, Option<&Downed>),
        (With<Soldier>, Without<LocalSoldier>),
    >,
    vehicles: Query<(&GlobalTransform, &VehicleData, &VehicleHealth)>,
    teams: Query<&Team>,
    players: Query<&Player>,
    mut panel: Single<&mut Visibility, With<TargetPanel>>,
    mut text: Single<&mut Text, With<TargetText>>,
    mut fill: Single<(&mut Node, &mut BackgroundColor), With<TargetFill>>,
) {
    let target = local.single().ok().and_then(|(me, loadout, inventory)| {
        let desc = loadout
            .weapons
            .get(inventory.active as usize)
            .and_then(|w| armory.weapon(w))
            .and_then(|w| w.replenish.clone())?;
        let team = local_team.single().ok().copied();
        let reach = desc.radius.max(3.0) + 1.5;
        let teammates = soldiers
            .iter()
            .filter(|(_, owner, ..)| teams.get(owner.0).ok().copied() == team)
            .map(|(render, owner, health, loadout, inventory, downed)| {
                (render.position.distance(me.position), owner.0, health, loadout, inventory, downed)
            })
            .filter(|(distance, ..)| *distance <= reach);
        if desc.revive_health > 0.0 {
            let (_, player, _, _, _, downed) = teammates
                .filter(|t| t.5.is_some())
                .min_by(|a, b| a.0.total_cmp(&b.0))?;
            let left = downed.map_or(0.0, |d| d.left);
            return Some((
                format!("{}: critically wounded, {left:.0} s", name_of(&players, Some(player))),
                left / MAN_DOWN_SECONDS,
                WOUNDED,
            ));
        }
        if desc.while_firing {
            let (_, data, health) = vehicles
                .iter()
                .map(|(at, data, health)| (at.translation().distance(me.position), data, health))
                .filter(|(distance, _, health)| *distance <= reach + 3.0 && !health.wrecked())
                .min_by(|a, b| a.0.total_cmp(&b.0))?;
            let desc = &data.0.desc;
            let name = if desc.display_name.is_empty() { &desc.name } else { &desc.display_name };
            let share = health.current / health.max.max(1.0);
            return Some((format!("{name}: {:.0}%", share * 100.0), share, GOOD));
        }
        let (_, player, health, loadout, inventory, _) = teammates
            .filter(|t| t.5.is_none())
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        let name = name_of(&players, Some(player));
        match desc.kind {
            ReplenishKind::Health => {
                let share = health.current / health.max.max(1.0);
                Some((format!("{name}: {:.0} HP", health.current.max(0.0)), share, GOOD))
            }
            ReplenishKind::Ammo => {
                let share = ammo_share(&armory, loadout, inventory);
                Some((format!("{name}: {:.0}% ammo", share * 100.0), share, SCORE))
            }
        }
    });
    let Some((line, share, color)) = target else {
        panel.set_if_neq(Visibility::Hidden);
        return;
    };
    panel.set_if_neq(Visibility::Inherited);
    if text.0 != line {
        text.0 = line;
    }
    let (node, background) = &mut *fill;
    crate::hud::set_width(node, percent(share.clamp(0.0, 1.0) * 100.0));
    background.set_if_neq(BackgroundColor(color));
}

/// Share of the ammo a soldier carries when full that he has (gadgets that recharge don't
/// count).
fn ammo_share(armory: &Armory, loadout: &Loadout, inventory: &Inventory) -> f32 {
    let (mut have, mut full) = (0.0, 0.0);
    for (index, name) in loadout.weapons.iter().enumerate() {
        let Some(weapon) = armory.weapon(name).filter(|w| w.replenish.is_none()) else {
            continue;
        };
        let most = (weapon.magazine_size * weapon.magazines) as f32;
        if most <= 0.0 {
            continue;
        }
        let [a, b] = inventory.ammo.get(index).copied().unwrap_or_default();
        have += ((a as f32 + b as f32) / most).min(1.0);
        full += 1.0;
    }
    if full > 0.0 { have / full } else { 1.0 }
}
