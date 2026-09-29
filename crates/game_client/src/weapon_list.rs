//! The weapon list (BF3 style): at the bottom right, above the ammo, whenever a weapon is
//! picked with the wheel, a number key or a quick key, the kit's weapons show in slot order
//! with their slot number, BF2's icon and name, the one picked highlighted; it fades after a
//! moment. The wheel steps through the same order (`combat::select_weapon`).

use std::sync::Arc;

use bevy::prelude::*;
use game_data::WeaponDesc;
use game_shared::{
    vehicle::Seated,
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    combat::{WeaponSelection, weapon_display_name},
    net::LocalSoldier,
    settings::{Action, Actions},
    ui_theme::{font, shadow},
};

pub struct WeaponListPlugin;

impl Plugin for WeaponListPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_list)
            .add_systems(Update, (rebuild_rows, update_rows).chain());
    }
}

/// Seconds the list stays after the last switch, then how long it takes to fade out.
const SHOW: f32 = 1.8;
const FADE: f32 = 0.6;

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.55);
const PICKED: Color = Color::srgba(0.95, 0.75, 0.3, 0.28);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);
const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);

/// Screens over the game (deploy, commander, the Esc menu, the main menu and loading) and
/// typing in the chat: the weapon list hides and the weapon keys, the wheel and the quick keys
/// leave the weapon alone meanwhile.
#[derive(bevy::ecs::system::SystemParam)]
pub struct Overlays<'w> {
    deploy: Res<'w, crate::deploy::DeployScreen>,
    commander: Option<Res<'w, crate::commander::CommanderScreen>>,
    menu: Option<Res<'w, crate::menu::Menu>>,
    screen: Option<Res<'w, State<crate::menu::Screen>>>,
    chat: Option<Res<'w, crate::chat::ChatBox>>,
}

impl Overlays<'_> {
    pub fn covered(&self) -> bool {
        self.deploy.open
            || self.commander.as_ref().is_some_and(|c| c.open)
            || self.menu.as_ref().is_some_and(|m| m.paused)
            || self.screen.as_ref().is_some_and(|s| *s.get() != crate::menu::Screen::InGame)
            || self.chat.as_ref().is_some_and(|c| c.typing.is_some())
    }
}

/// What can be taken in hand, in slot order (then kit order): the list's rows and the
/// wheel's steps. Worn gear and the parachute are left out.
pub fn weapon_order(loadout: &Loadout, armory: &Armory) -> Vec<(u8, Arc<WeaponDesc>)> {
    let mut order: Vec<(u8, Arc<WeaponDesc>)> = loadout
        .weapons
        .iter()
        .enumerate()
        .filter_map(|(index, name)| Some((index as u8, armory.weapon(name)?.clone())))
        .filter(|(_, w)| w.selectable())
        .collect();
    order.sort_by_key(|(index, w)| (w.slot.max(1), *index));
    order
}

#[derive(Component)]
struct WeaponList;

/// A row, for loadout index `.0`.
#[derive(Component)]
struct Row(u8);

/// A coloured part of a row and its colour at full opacity (the list fades as a whole).
#[derive(Component, Clone, Copy)]
enum Part {
    Background,
    Accent,
    Key,
    Name,
    Count,
    Icon,
}

/// The grenade and launcher round counts.
#[derive(Component)]
struct Count(u8);

fn spawn_list(mut commands: Commands) {
    commands.spawn((
        WeaponList,
        Name::new("weapon_list"),
        Node {
            position_type: PositionType::Absolute,
            right: px(24),
            // Above the ammo panel.
            bottom: px(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::End,
            row_gap: px(3),
            ..default()
        },
        Visibility::Hidden,
    ));
}

/// The rows of our soldier's weapons, rebuilt when the loadout changes.
#[allow(clippy::type_complexity)]
fn rebuild_rows(
    mut commands: Commands,
    armory: Res<Armory>,
    actions: Actions,
    assets: Res<AssetServer>,
    soldier: Query<Ref<Loadout>, With<LocalSoldier>>,
    list: Single<(Entity, Option<&Children>), With<WeaponList>>,
    mut built: Local<Vec<String>>,
) {
    let weapons = soldier.single().map(|l| l.weapons.clone()).unwrap_or_default();
    if *built == weapons && !armory.is_changed() {
        return;
    }
    *built = weapons;
    let (list, children) = *list;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let Ok(loadout) = soldier.single() else {
        return;
    };
    let order = weapon_order(&loadout, &armory);
    info!(
        "weapon list: {}",
        order
            .iter()
            .map(|(_, w)| format!("{} {}", w.slot, weapon_display_name(&w.display_name)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (index, weapon) in order {
        // The knife and grenades also have quick keys.
        let quick = if weapon.is_melee() {
            Some(actions.label(Action::Melee))
        } else if weapon.is_hand_grenade() {
            Some(actions.label(Action::Grenade))
        } else {
            None
        };
        let key = match quick {
            Some(quick) if !quick.is_empty() && quick != "unbound" => format!("{}  {quick}", weapon.slot),
            _ => weapon.slot.to_string(),
        };
        commands.entity(list).with_children(|list| {
            list.spawn((
                Row(index),
                Part::Background,
                Name::new(format!("weapon_list:{}", weapon.name)),
                Node {
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: px(10),
                    padding: UiRect::new(px(0), px(12), px(3), px(3)),
                    width: px(300),
                    height: px(34),
                    border_radius: BorderRadius::all(px(6)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|row| {
                // The picked row's accent bar.
                row.spawn((
                    Row(index),
                    Part::Accent,
                    Node {
                        width: px(3),
                        height: percent(100),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ));
                row.spawn((
                    Row(index),
                    Part::Key,
                    Text::new(key),
                    font(12.0),
                    TextColor(DIM),
                    Node {
                        min_width: px(34),
                        ..default()
                    },
                ));
                row.spawn((
                    Row(index),
                    Part::Name,
                    Text::new(weapon_display_name(&weapon.display_name)),
                    font(14.0),
                    TextColor(DIM),
                    shadow(),
                    TextLayout::no_wrap(),
                    Node {
                        flex_grow: 1.0,
                        ..default()
                    },
                ));
                row.spawn((Row(index), Part::Count, Count(index), Text::new(""), font(12.0), TextColor(DIM)));
                if let Some(icon) = &weapon.icon {
                    row.spawn((
                        Row(index),
                        Part::Icon,
                        ImageNode {
                            image: assets.load(format!("imported://{icon}")),
                            color: DIM,
                            ..default()
                        },
                        // BF2's icons are 140x40.
                        Node {
                            width: px(98),
                            height: px(28),
                            ..default()
                        },
                    ));
                }
            });
        });
    }
}

/// Shows the list after a switch, highlights the pick, counts grenades, fades it out.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_rows(
    time: Res<Time<Real>>,
    selection: Res<WeaponSelection>,
    soldier: Query<(&Loadout, &Inventory, Has<Seated>), With<LocalSoldier>>,
    armory: Res<Armory>,
    overlays: Overlays,
    mut list: Single<&mut Visibility, With<WeaponList>>,
    mut parts: Query<
        (&Row, &Part, Option<&mut BackgroundColor>, Option<&mut TextColor>, Option<&mut ImageNode>, Option<&Count>, Option<&mut Text>),
    >,
    mut shown: Local<(u32, f32)>,
) {
    let now = time.elapsed_secs();
    if shown.0 != selection.switches {
        *shown = (selection.switches, now);
    }
    let Ok((loadout, inventory, seated)) = soldier.single() else {
        list.set_if_neq(Visibility::Hidden);
        return;
    };
    let age = now - shown.1;
    let alpha = if shown.1 == 0.0 || seated || overlays.covered() {
        0.0
    } else {
        (1.0 - (age - SHOW) / FADE).clamp(0.0, 1.0)
    };
    list.set_if_neq(if alpha > 0.0 { Visibility::Inherited } else { Visibility::Hidden });
    if alpha <= 0.0 {
        return;
    }
    let fade = |color: Color| color.with_alpha(color.alpha() * alpha);
    for (row, part, background, text_color, image, count, text) in &mut parts {
        let picked = row.0 == selection.index;
        let (color, tint) = match part {
            Part::Background => (if picked { PICKED } else { PANEL }, None),
            Part::Accent => (if picked { ACCENT } else { Color::NONE }, None),
            Part::Key | Part::Count => (Color::NONE, Some(if picked { ACCENT } else { DIM })),
            Part::Name | Part::Icon => (Color::NONE, Some(if picked { TEXT } else { DIM })),
        };
        if let Some(mut background) = background {
            background.set_if_neq(BackgroundColor(fade(color)));
        }
        if let (Some(tint), Some(mut text_color)) = (tint, text_color) {
            text_color.set_if_neq(TextColor(fade(tint)));
        }
        if let (Some(tint), Some(mut image)) = (tint, image)
            && image.color != fade(tint)
        {
            image.color = fade(tint);
        }
        // Grenades and launchers: rounds left.
        if let (Some(count), Some(mut text)) = (count, text) {
            let weapon = loadout.weapons.get(count.0 as usize).and_then(|w| armory.weapon(w));
            let value = match (weapon, inventory.ammo.get(count.0 as usize)) {
                (Some(w), Some([mag, spare])) if w.magazine_size == 1 => format!("x{}", mag + spare),
                _ => String::new(),
            };
            if text.0 != value {
                text.0 = value;
            }
        }
    }
}
