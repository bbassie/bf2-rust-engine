//! Name tags over soldiers' heads and vehicles, and the moment a crosshair reveals an enemy's
//! name (BF3/BF4/2042-style). The enemy "spotted" triangle is [`crate::radio::spots`]; this
//! module only adds the settings gate to it (a small hook, see `radio::spots::update_markers`) plus
//! everything about *friendly* tags and the brief red name when aiming at a nearby enemy.
//!
//! - **Teammates**: full tag (name, in the team colour) within [`NEAR_RANGE`] or while aimed
//!   at; farther out, a small fading dot, up to [`ICON_FADE_END`]. Requires line of sight
//!   (a cheap raycast against world colliders only, throttled over a few teammates a frame:
//!   [`LosCache`]) - without it, nothing shows at all.
//! - **Squad mates**: the same, but in the squad colour, and the dot shows through walls
//!   (no line-of-sight check) when the full tag isn't earned. The full tag still needs it,
//!   same as any teammate: a squad mate on the other side of a wall is a dot, not a name.
//! - **Downed teammates**: no tag here; [`crate::wounded`] already marks them (a revive icon,
//!   with distance and time left for medics), so a plain name tag would just be noise on top
//!   of it.
//! - **Enemies**: nothing, unless the crosshair rests on one within [`ENEMY_AIM_RANGE`] for a
//!   moment ([`AimState`]) - then their name flashes up in red, briefly. Never through walls:
//!   it's the same raycast that finds them.
//! - **Vehicles**: friendly ones (no enemy aboard) get the vehicle's name and its occupants',
//!   in the team colour, while someone we're not riding with is aboard and it isn't the one
//!   we're in ourselves.
//!
//! Screen position comes from the interpolated render state ([`SoldierRender`], `VehicleView`),
//! never the replicated tick position, so tags don't jitter with it. One UI entity is kept per
//! target and reused frame to frame (as `radio::spots` and `wounded`'s markers already do). A
//! tag moves by its `UiTransform`, not `left`/`top` (`map_markers::place_icon` does the same
//! for the minimap, for the same reason: a changed `Node` lays its whole tree out again, and a
//! tag moves every frame the camera does); the dot's size and the name text are `Node`/`Text`
//! and so only written when their rounded or string value actually changes, which is what this
//! is measured against (`scenarios/perf/perf_karkand.ron`).

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use game_shared::{
    physics::GameLayer,
    protocol::{ControlledBy, Player, Team},
    revive::Downed,
    soldier::{Hitbox, Soldier},
    squad::SquadMember,
    vehicle::{Seated, VehicleData},
};

use crate::{
    camera::PlayerCamera,
    conquest_hud::{ENEMY, FRIENDLY, SQUAD},
    deploy::DeployScreen,
    menu::{Menu, Screen},
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
    render::scope::Zoom,
    settings::{NameTagMode, Settings},
    ui_theme::{font, shadow},
    vehicles::VehicleView,
};

pub struct NameTagsPlugin;

impl Plugin for NameTagsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AimState>()
            .init_resource::<LosCache>()
            .add_systems(Startup, spawn_root)
            .add_systems(Update, (update_aim, update_los, update_tags).chain());
    }
}

/// A teammate (not a squad mate) shows a tag within this many meters even without aiming.
const NEAR_RANGE: f32 = 45.0;
/// Beyond this, even the dot disappears.
const ICON_FADE_END: f32 = 250.0;
/// How close an enemy must be for the crosshair to reveal their name.
const ENEMY_AIM_RANGE: f32 = 50.0;
/// How far the crosshair's probe ray reaches, resolving both the friendly "aimed at" tag and
/// the enemy reveal.
const AIM_MAX_RANGE: f32 = 80.0;
/// Seconds the crosshair must rest on an enemy before their name is revealed (so sweeping the
/// view across one doesn't flash names everywhere).
const AIM_DWELL_SHOW: f32 = 0.15;
/// Seconds an aim target is kept after the crosshair moves off it, so the reveal doesn't
/// flicker off on a jittery frame.
const AIM_HOLD: f32 = 0.6;
/// Teammates re-tested for line of sight each frame (the rest keep last frame's answer); cheap
/// and unnoticeable to cycle through even 63 bots over a handful of frames.
const LOS_BATCH: usize = 10;
/// A friendly vehicle's name and occupants show within this many meters.
const VEHICLE_RANGE: f32 = 150.0;
/// How far above a soldier's feet the tag hangs (capsule top plus a little air).
const ANCHOR_MARGIN: f32 = 0.15;
/// How far above a vehicle's origin its tag hangs.
const VEHICLE_ABOVE: f32 = 3.4;
/// The dot's width/height in pixels, near to far.
const DOT_SIZES: [f32; 3] = [9.0, 6.5, 4.5];

/// What the crosshair is resting on, for the friendly "aimed at" tag and the enemy reveal.
#[derive(Resource, Default)]
struct AimState {
    /// The soldier the crosshair is on right now, if any.
    raw: Option<Entity>,
    /// Kept a little after `raw` is lost (see [`AIM_HOLD`]).
    target: Option<Entity>,
    distance: f32,
    /// Seconds `target` has been continuously aimed at.
    dwell: f32,
    since_lost: f32,
}

/// Line-of-sight answers for teammates, refreshed a few at a time (see [`LOS_BATCH`]).
#[derive(Resource, Default)]
struct LosCache {
    visible: bevy::platform::collections::HashMap<Entity, bool>,
    cursor: usize,
}

#[derive(Component)]
struct TagRoot;

/// What's shown, and the values last written to its children, so unchanged frames touch
/// nothing (`Node` and `Text` writes cost a layout/shaping pass; colour alone doesn't).
#[derive(Component)]
struct Tag {
    of: Entity,
    left: i32,
    top: i32,
    full: bool,
    dot_size: i32,
    name: String,
}

#[derive(Component)]
struct TagDot;
#[derive(Component)]
struct TagName;

fn spawn_root(mut commands: Commands) {
    commands.spawn((
        TagRoot,
        Node {
            position_type: PositionType::Absolute,
            width: percent(100),
            height: percent(100),
            ..default()
        },
        Pickable::IGNORE,
    ));
}

/// Resolves the child hitbox collider a ray hit back to the soldier entity it belongs to.
fn soldier_of(hit: Entity, parents: &Query<&ChildOf>, soldiers: &Query<(), With<Soldier>>) -> Option<Entity> {
    let root = parents.get(hit).map(|c| c.parent()).unwrap_or(hit);
    soldiers.contains(root).then_some(root)
}

/// What the crosshair is on, and how long it's been there.
fn update_aim(
    time: Res<Time>,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    spatial: SpatialQuery,
    local_hitbox: Query<&Hitbox, With<LocalSoldier>>,
    parents: Query<&ChildOf>,
    soldiers: Query<(), With<Soldier>>,
    mut aim: ResMut<AimState>,
) {
    let dt = time.delta_secs();
    let camera = *camera;
    let excluded: Vec<Entity> = local_hitbox.iter().map(|h| h.entity).collect();
    let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Soldier]).with_excluded_entities(excluded);
    let hit = spatial.cast_ray(camera.translation(), camera.forward(), AIM_MAX_RANGE, true, &filter);
    aim.raw = hit.and_then(|hit| soldier_of(hit.entity, &parents, &soldiers));
    match aim.raw {
        Some(entity) => {
            if aim.target != Some(entity) {
                aim.target = Some(entity);
                aim.dwell = 0.0;
            } else {
                aim.dwell += dt;
            }
            aim.distance = hit.map_or(0.0, |h| h.distance);
            aim.since_lost = 0.0;
        }
        None => {
            aim.since_lost += dt;
            if aim.since_lost > AIM_HOLD {
                aim.target = None;
                aim.dwell = 0.0;
            }
        }
    }
}

/// Retests a few teammates' visibility a frame (world colliders only; soldiers don't block
/// each other's tags).
#[allow(clippy::type_complexity)]
fn update_los(
    spatial: SpatialQuery,
    camera: Single<&GlobalTransform, With<PlayerCamera>>,
    local_team: Query<&Team, With<LocalPlayer>>,
    local_hitbox: Query<&Hitbox, With<LocalSoldier>>,
    soldiers: Query<(Entity, &SoldierRender, &ControlledBy), (With<Soldier>, Without<LocalSoldier>, Without<Downed>)>,
    teams: Query<&Team>,
    mut cache: ResMut<LosCache>,
) {
    let Ok(&team) = local_team.single() else {
        cache.visible.clear();
        return;
    };
    let eye = camera.translation();
    let excluded: Vec<Entity> = local_hitbox.iter().map(|h| h.entity).collect();
    let filter = SpatialQueryFilter::from_mask(GameLayer::World).with_excluded_entities(excluded);
    let candidates: Vec<Entity> = soldiers
        .iter()
        .filter(|(_, render, owner)| {
            teams.get(owner.0).ok().copied() == Some(team) && render.position.distance(eye) <= ICON_FADE_END
        })
        .map(|(e, ..)| e)
        .collect();
    if candidates.is_empty() {
        cache.visible.clear();
        return;
    }
    cache.visible.retain(|e, _| candidates.contains(e));
    let start = cache.cursor % candidates.len();
    for step in 0..candidates.len().min(LOS_BATCH) {
        let entity = candidates[(start + step) % candidates.len()];
        let Ok((_, render, _)) = soldiers.get(entity) else { continue };
        let point = render.position + Vec3::Y * (render.stance.collision_height() * 0.5);
        let to = point - eye;
        let visible = Dir3::new(to).is_ok_and(|dir| spatial.cast_ray(eye, dir, to.length(), true, &filter).is_none());
        cache.visible.insert(entity, visible);
    }
    cache.cursor = cache.cursor.wrapping_add(LOS_BATCH);
}

/// A tag to show this frame: where, what it says (only for a full tag) and in what colour.
struct Wanted {
    entity: Entity,
    anchor: Vec3,
    full: bool,
    color: Color,
    name: String,
    /// The brief red reveal on an aimed-at enemy, logged once per new reveal.
    enemy_reveal: bool,
}

fn icon_tier(distance: f32) -> usize {
    let share = ((distance - NEAR_RANGE) / (ICON_FADE_END - NEAR_RANGE).max(1.0)).clamp(0.0, 1.0);
    ((share * (DOT_SIZES.len() - 1) as f32).round() as usize).min(DOT_SIZES.len() - 1)
}

fn icon_alpha(distance: f32) -> f32 {
    let share = ((distance - NEAR_RANGE) / (ICON_FADE_END - NEAR_RANGE).max(1.0)).clamp(0.0, 1.0);
    (1.0 - share * 0.75).clamp(0.2, 1.0)
}

/// Whether the HUD's tags should show at all: not in a menu or the deploy screen, and not
/// looking through a scope (the enemy spot triangle stays up regardless; that's
/// `radio::spots`, unaffected by this).
#[derive(SystemParam)]
struct TagGates<'w> {
    zoom: Res<'w, Zoom>,
    screen: Res<'w, State<Screen>>,
    menu: Res<'w, Menu>,
    deploy: Res<'w, DeployScreen>,
}

impl TagGates<'_> {
    fn active(&self) -> bool {
        *self.screen.get() == Screen::InGame && !self.menu.paused && !self.deploy.open && !self.zoom.scoped
    }
}

/// Everyone and everything a tag could be about.
#[derive(SystemParam)]
struct TagSources<'w, 's> {
    local_team: Query<'w, 's, &'static Team, With<LocalPlayer>>,
    local_squad: Query<'w, 's, &'static SquadMember, With<LocalPlayer>>,
    local_soldier: Query<'w, 's, Entity, With<LocalSoldier>>,
    soldiers: Query<
        'w,
        's,
        (Entity, &'static SoldierRender, &'static ControlledBy),
        (With<Soldier>, Without<LocalSoldier>, Without<Downed>),
    >,
    players: Query<'w, 's, (&'static Player, &'static Team, Option<&'static SquadMember>)>,
    vehicles: Query<'w, 's, (Entity, &'static VehicleView, &'static VehicleData)>,
    seated: Query<'w, 's, (&'static Seated, &'static ControlledBy), With<Soldier>>,
}

/// The pooled tag entities: one per target, reused frame to frame.
#[derive(SystemParam)]
struct TagPool<'w, 's> {
    root: Single<'w, 's, Entity, With<TagRoot>>,
    roots: Query<'w, 's, (Entity, &'static mut Tag, &'static mut UiTransform, &'static Children), (Without<TagDot>, Without<TagName>)>,
    dots: Query<'w, 's, (&'static mut Node, &'static mut BackgroundColor), (With<TagDot>, Without<Tag>, Without<TagName>)>,
    names: Query<'w, 's, (&'static mut Text, &'static mut TextColor, &'static mut Node), (With<TagName>, Without<Tag>, Without<TagDot>)>,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_tags(
    mut commands: Commands,
    settings: Res<Settings>,
    gates: TagGates,
    aim: Res<AimState>,
    los: Res<LosCache>,
    camera: Single<(&Camera, &GlobalTransform), With<PlayerCamera>>,
    sources: TagSources,
    mut pool: TagPool,
    mut last_reveal: Local<Option<Entity>>,
) {
    let (camera, view) = *camera;
    let team = sources.local_team.single().ok().copied();
    if !gates.active() || team.is_none() {
        *last_reveal = None;
        for (entity, ..) in &pool.roots {
            commands.entity(entity).try_despawn();
        }
        return;
    }
    let team = team.unwrap();
    let eye = view.translation();
    let my_squad = sources.local_squad.single().ok().map(|s| s.squad);
    let my_soldier = sources.local_soldier.single().ok();
    let mut wanted = Vec::new();

    for (entity, render, owner) in &sources.soldiers {
        let Ok((player, &their_team, squad)) = sources.players.get(owner.0) else { continue };
        let anchor = render.position + Vec3::Y * (render.stance.collision_height() + ANCHOR_MARGIN);
        let distance = anchor.distance(eye);
        let aimed = aim.target == Some(entity);
        if their_team == team {
            if settings.name_tags == NameTagMode::Off {
                continue;
            }
            let is_squad = my_squad.is_some() && squad.map(|s| s.squad) == my_squad;
            if settings.name_tags == NameTagMode::SquadOnly && !is_squad {
                continue;
            }
            let in_los = los.visible.get(&entity).copied().unwrap_or(false);
            let color = if is_squad { SQUAD } else { FRIENDLY };
            if in_los && (distance <= NEAR_RANGE || aimed) {
                wanted.push(Wanted { entity, anchor, full: true, color, name: player.name.clone(), enemy_reveal: false });
            } else if (is_squad || in_los) && distance <= ICON_FADE_END {
                wanted.push(Wanted { entity, anchor, full: false, color, name: String::new(), enemy_reveal: false });
            }
        } else if aim.target == Some(entity) && aim.dwell >= AIM_DWELL_SHOW && aim.distance <= ENEMY_AIM_RANGE {
            wanted.push(Wanted { entity, anchor, full: true, color: ENEMY, name: player.name.clone(), enemy_reveal: true });
        }
    }

    // A newly revealed enemy is worth a log line scenarios can check for
    // (`ExpectLog("nametag: aim reveal")`).
    let revealed = wanted.iter().find(|w| w.enemy_reveal).map(|w| w.entity);
    if revealed != *last_reveal
        && let Some(entity) = revealed
        && let Ok((_, _, owner)) = sources.soldiers.get(entity)
        && let Ok((player, ..)) = sources.players.get(owner.0)
    {
        info!("nametag: aim reveal on {}", player.name);
    }
    *last_reveal = revealed;

    // Friendly vehicles: the vehicle's name and its occupants', while none of the occupants
    // are enemies and we aren't one of them ourselves.
    for (entity, vehicle_view, data) in &sources.vehicles {
        let occupants: Vec<&Player> = sources
            .seated
            .iter()
            .filter(|(s, _)| s.vehicle == entity)
            .filter_map(|(_, owner)| sources.players.get(owner.0).ok().map(|(p, ..)| p))
            .collect();
        if occupants.is_empty() {
            continue;
        }
        let we_are_aboard =
            my_soldier.is_some_and(|me| sources.seated.get(me).is_ok_and(|(s, _)| s.vehicle == entity));
        if we_are_aboard {
            continue;
        }
        let all_friendly = sources
            .seated
            .iter()
            .filter(|(s, _)| s.vehicle == entity)
            .all(|(_, owner)| sources.players.get(owner.0).is_ok_and(|(_, &t, _)| t == team));
        if !all_friendly {
            continue;
        }
        let anchor = vehicle_view.transform.translation + Vec3::Y * VEHICLE_ABOVE;
        let distance = anchor.distance(eye);
        if distance > VEHICLE_RANGE {
            continue;
        }
        let desc = &data.0.desc;
        let vehicle_name = if desc.display_name.is_empty() { &desc.name } else { &desc.display_name };
        let riders = occupants.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        wanted.push(Wanted {
            entity,
            anchor,
            full: true,
            color: FRIENDLY,
            name: format!("{vehicle_name}\n{riders}"),
            enemy_reveal: false,
        });
    }

    apply(
        &mut commands,
        &settings,
        camera,
        view,
        &pool.root,
        &mut pool.roots,
        &mut pool.dots,
        &mut pool.names,
        wanted,
    );
}

/// Projects `wanted` to the screen and updates the pool: matching tags are moved and
/// recoloured in place (position/size/text only touched when their rounded value changed),
/// unmatched ones are despawned, and new targets get a fresh tag.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn apply(
    commands: &mut Commands,
    settings: &Settings,
    camera: &Camera,
    view: &GlobalTransform,
    root: &Entity,
    roots: &mut Query<(Entity, &mut Tag, &mut UiTransform, &Children), (Without<TagDot>, Without<TagName>)>,
    dots: &mut Query<(&mut Node, &mut BackgroundColor), (With<TagDot>, Without<Tag>, Without<TagName>)>,
    names: &mut Query<(&mut Text, &mut TextColor, &mut Node), (With<TagName>, Without<Tag>, Without<TagDot>)>,
    wanted: Vec<Wanted>,
) {
    let scale = settings.name_tag_size.clamp(0.5, 2.0);
    let mut projected: Vec<(Wanted, Vec2, f32)> = wanted
        .into_iter()
        .filter_map(|w| {
            let at = camera.world_to_viewport(view, w.anchor).ok()?;
            let distance = w.anchor.distance(view.translation());
            Some((w, at, distance))
        })
        .collect();

    for (root_entity, mut tag, mut transform, children) in roots.iter_mut() {
        let Some(index) = projected.iter().position(|(w, ..)| w.entity == tag.of) else {
            commands.entity(root_entity).try_despawn();
            continue;
        };
        let (wanted, at, distance) = projected.swap_remove(index);
        let left = (at.x - 60.0 * scale) as i32;
        let top = (at.y - 22.0 * scale) as i32;
        if tag.left != left || tag.top != top {
            // A moved `UiTransform` needs no new layout, unlike `left`/`top` (see the module
            // doc); tags move every frame the camera does, so this is the one that matters.
            transform.translation = Val2::px(left as f32, top as f32);
            tag.left = left;
            tag.top = top;
        }
        if tag.full != wanted.full {
            tag.full = wanted.full;
            if wanted.full {
                info!("nametag: full tag shown for {}", wanted.name.replace('\n', " / "));
            } else {
                info!("nametag: icon shown at {distance:.0} m");
            }
        }
        let tier = icon_tier(distance);
        let dot_size = (DOT_SIZES[tier] * scale) as i32;
        for child in children.iter() {
            if let Ok((mut dot_node, mut color)) = dots.get_mut(child) {
                let display = if wanted.full { Display::None } else { Display::Flex };
                if dot_node.display != display {
                    dot_node.display = display;
                }
                if tag.dot_size != dot_size {
                    dot_node.width = px(dot_size as f32);
                    dot_node.height = px(dot_size as f32);
                    tag.dot_size = dot_size;
                }
                color.0 = wanted.color.with_alpha(icon_alpha(distance));
            } else if let Ok((mut text, mut text_color, mut name_node)) = names.get_mut(child) {
                let display = if wanted.full { Display::Flex } else { Display::None };
                if name_node.display != display {
                    name_node.display = display;
                }
                if wanted.full && tag.name != wanted.name {
                    text.0 = wanted.name.clone();
                    tag.name = wanted.name.clone();
                }
                text_color.0 = wanted.color;
            }
        }
    }

    for (wanted, at, distance) in projected {
        let left = (at.x - 60.0 * scale) as i32;
        let top = (at.y - 22.0 * scale) as i32;
        let tier = icon_tier(distance);
        let dot_size = (DOT_SIZES[tier] * scale) as i32;
        // Once per tag appearing (not every frame it stays up): scenarios watch for this with
        // `ExpectLog`.
        if wanted.full {
            info!("nametag: full tag shown for {}", wanted.name.replace('\n', " / "));
        } else {
            info!("nametag: icon shown at {distance:.0} m");
        }
        commands.entity(*root).with_children(|root| {
            root.spawn((
                Tag {
                    of: wanted.entity,
                    left,
                    top,
                    full: wanted.full,
                    dot_size,
                    name: wanted.name.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    // Fixed forever; the tag moves by `UiTransform` instead (see the module
                    // doc), so this never gets touched again and never re-triggers layout.
                    left: px(0),
                    top: px(0),
                    width: px(120.0 * scale),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    ..default()
                },
                UiTransform {
                    translation: Val2::px(left as f32, top as f32),
                    ..default()
                },
                Pickable::IGNORE,
                children![
                    (
                        TagDot,
                        Node {
                            width: px(dot_size as f32),
                            height: px(dot_size as f32),
                            border_radius: BorderRadius::MAX,
                            border: UiRect::all(px(1.0)),
                            display: if wanted.full { Display::None } else { Display::Flex },
                            ..default()
                        },
                        BackgroundColor(wanted.color.with_alpha(icon_alpha(distance))),
                        BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                    ),
                    (
                        TagName,
                        Text::new(wanted.name.clone()),
                        font(13.0 * scale),
                        TextColor(wanted.color),
                        shadow(),
                        TextLayout::justify(Justify::Center),
                        Node {
                            display: if wanted.full { Display::Flex } else { Display::None },
                            ..default()
                        },
                    ),
                ],
            ));
        });
    }
}
