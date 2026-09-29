//! The loadout panel of the deploy screen: for the kit picked there, a list of every weapon
//! of its class (of any faction and expansion, see `game_shared::arsenal`) and of every
//! sidearm, with their icons, factions and stats. Picks are kept per class in the settings
//! and sent to the server ([`LoadoutRequest`]), which says what it accepted
//! ([`LoadoutPicks`]); the next soldier of that class carries them.

use bevy::{prelude::*, ui_widgets::ScrollArea};
use game_data::{FireMode, WeaponDesc};
use game_shared::{
    arsenal::{Arsenal, ClassPick, LoadoutPicks, LoadoutRequest, LoadoutRules, PickSlot, PoolWeapon, Refusal, team_factions},
    conquest::{Deployment, team_index},
    join::AccountBadge,
    protocol::Team,
    weapons::Armory,
};

use crate::{
    combat::weapon_display_name,
    conquest_hud::FRIENDLY,
    deploy::DeployScreen,
    net::LocalPlayer,
    settings::Settings,
    ui_theme::font,
};

pub struct LoadoutPlugin;

impl Plugin for LoadoutPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LoadoutTab>().add_systems(
            Update,
            (send_picks, fill_panel, pick_weapon, update_rows, update_stats, log_spawn)
                .chain()
                .after(crate::scenario::ScenarioSystems),
        );
    }
}

/// Where the deploy screen puts the loadout panel (see `deploy::spawn_deploy_screen`).
#[derive(Component)]
pub struct LoadoutPanel;

/// Which pool the panel lists.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
struct LoadoutTab(Tab);

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    #[default]
    Primary,
    Sidearm,
}

const TEXT: Color = Color::srgb(0.95, 0.96, 0.98);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.6);
const FAINT: Color = Color::srgba(0.85, 0.87, 0.9, 0.3);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);
const WARN: Color = Color::srgb(0.95, 0.45, 0.35);
const ROW: Color = Color::srgba(1.0, 1.0, 1.0, 0.04);
const ROW_HOVER: Color = Color::srgba(1.0, 1.0, 1.0, 0.1);
const LIST_HEIGHT: f32 = 236.0;

/// A button of the panel.
#[derive(Component, Clone, PartialEq, Eq, Debug)]
enum LoadoutButton {
    Tab(Tab),
    /// The kit's own weapon.
    Default,
    Weapon(String),
}

/// Why a row can't be picked, if it can't.
#[derive(Component)]
struct Unavailable;

#[derive(Component)]
struct StatsText;
#[derive(Component)]
struct StatBar(usize);
#[derive(Component)]
struct StatusText;

/// The class of our kit on the deploy screen: (lowercase kind, kit).
fn our_kit<'a>(armory: &'a Armory, team: Team, deployment: &Deployment) -> Option<(String, &'a game_data::KitDesc)> {
    let kit = armory.kit_for(team_index(team)?, deployment.kit as usize)?;
    Some((kit.kind.to_ascii_lowercase(), kit))
}

/// Class names for BF2's `kitType`s.
pub fn class_title(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "specops" => "Special Forces".into(),
        "sniper" => "Sniper".into(),
        "assault" => "Assault".into(),
        "support" => "Support".into(),
        "engineer" => "Engineer".into(),
        "medic" => "Medic".into(),
        "at" => "Anti-Tank".into(),
        other => other.to_string(),
    }
}

/// Silenced pistols share their plain twin's name.
fn silenced(weapon: &str) -> &'static str {
    if weapon.contains("silenc") || weapon.contains("silens") { " SD" } else { "" }
}

/// Faction names for kit prefixes.
fn faction_title(faction: &str) -> String {
    match faction {
        "us" => "USMC".into(),
        "mec" => "MEC".into(),
        "ch" => "China".into(),
        "eu" => "EU".into(),
        "sas" => "SAS".into(),
        "seal" => "Navy SEAL".into(),
        "spetsnaz" => "Spetsnaz".into(),
        "mecsf" => "MEC SF".into(),
        "chinsurgent" => "Rebels".into(),
        "meinsurgent" => "Insurgents".into(),
        "un" => "UN".into(),
        other => other.to_uppercase(),
    }
}

/// The weapons a soldier of `kit` carries with the picks the server accepted for its class
/// (the deploy screen's kit buttons list them).
pub fn kit_weapons_with_picks(
    kit: &game_data::KitDesc,
    picks: Option<&LoadoutPicks>,
    armory: &Armory,
    arsenal: &Arsenal,
) -> Vec<String> {
    match picks.and_then(|p| p.0.get(&kit.kind.to_ascii_lowercase())) {
        Some(pick) => arsenal.kit_weapons(kit, pick, armory),
        None => kit.weapons.clone(),
    }
}

/// Says what our new soldier carries (scenarios check it).
fn log_spawn(soldier: Query<&game_shared::weapons::Loadout, Added<crate::net::LocalSoldier>>) {
    for loadout in &soldier {
        info!("spawned as {} with {}", loadout.kit, loadout.weapons.join(", "));
    }
}

/// Sends our picks when we join and whenever they change.
fn send_picks(
    settings: Res<Settings>,
    player: Query<Entity, With<LocalPlayer>>,
    mut requests: MessageWriter<LoadoutRequest>,
    mut sent: Local<Option<(Entity, Vec<(String, ClassPick)>)>>,
) {
    let Ok(player) = player.single() else {
        *sent = None;
        return;
    };
    let picks: Vec<(String, ClassPick)> = settings
        .loadouts
        .iter()
        .filter(|(_, pick)| !pick.is_empty())
        .map(|(class, pick)| (class.clone(), pick.clone()))
        .collect();
    if sent.as_ref().is_some_and(|(p, s)| *p == player && *s == picks) {
        return;
    }
    // Nothing to ask for on a first join with the kits as they are.
    if sent.is_none() && picks.is_empty() {
        *sent = Some((player, picks));
        return;
    }
    requests.write(LoadoutRequest { picks: picks.clone() });
    *sent = Some((player, picks));
}

/// What the panel shows, to rebuild it only when that changes.
#[derive(PartialEq, Default)]
struct Shown {
    class: String,
    tab: Option<Tab>,
    rules: Option<LoadoutRules>,
    team: Option<Team>,
    pool: usize,
    rank: Option<u32>,
}

/// Rebuilds the panel for our kit's class and the tab.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn fill_panel(
    mut commands: Commands,
    screen: Res<DeployScreen>,
    tab: Res<LoadoutTab>,
    armory: Res<Armory>,
    arsenal: Res<Arsenal>,
    assets: Res<AssetServer>,
    rules: Query<&LoadoutRules>,
    player: Query<(&Team, &Deployment, Option<&AccountBadge>), With<LocalPlayer>>,
    panel: Single<(Entity, Option<&Children>), With<LoadoutPanel>>,
    mut shown: Local<Shown>,
) {
    if !screen.open {
        return;
    }
    let Ok((team, deployment, badge)) = player.single() else {
        return;
    };
    let rules = rules.single().cloned().unwrap_or_default();
    let Some((class, kit)) = our_kit(&armory, *team, deployment) else {
        return;
    };
    let slot = match tab.0 {
        Tab::Primary => PickSlot::Primary,
        Tab::Sidearm => PickSlot::Sidearm,
    };
    let pool: Vec<PoolWeapon> = arsenal.pool(&class, slot).map(<[PoolWeapon]>::to_vec).unwrap_or_default();
    let now = Shown {
        class: class.clone(),
        tab: Some(tab.0),
        rules: Some(rules.clone()),
        team: Some(*team),
        pool: pool.len() + arsenal.classes.len() * 1000,
        rank: badge.map(|b| b.rank),
    };
    if *shown == now && !armory.is_changed() && !arsenal.is_changed() {
        return;
    }
    *shown = now;
    let (panel, children) = *panel;
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let factions = team_index(*team).map(|t| team_factions(&armory, t)).unwrap_or_default();
    let rank = badge.map(|b| b.rank);
    let kit_weapon = kit
        .weapons
        .iter()
        .filter_map(|w| armory.weapon(w))
        .find(|w| w.selectable() && w.slot == if slot == PickSlot::Primary { 3 } else { 2 })
        .cloned();
    commands.entity(panel).with_children(|panel| {
        panel.spawn((
            Text::new(format!("{} LOADOUT", class_title(&class).to_uppercase())),
            font(18.0),
            TextColor(TEXT),
        ));
        if !rules.arsenal {
            panel.spawn((
                Text::new("This server plays BF2's kits as they are."),
                font(13.0),
                TextColor(DIM),
            ));
            return;
        }
        let hint = if rules.faction_locked {
            "Any weapon of this class that your team's factions carry."
        } else {
            "Any weapon of this class, from every faction and expansion."
        };
        panel.spawn((Text::new(hint), font(12.0), TextColor(DIM)));
        panel
            .spawn(Node {
                column_gap: px(6),
                ..default()
            })
            .with_children(|tabs| {
                for (t, label) in [(Tab::Primary, "Primary"), (Tab::Sidearm, "Sidearm")] {
                    tabs.spawn((
                        LoadoutButton::Tab(t),
                        Button,
                        Name::new(format!("loadout:tab:{}", label.to_lowercase())),
                        Node {
                            padding: UiRect::axes(px(12), px(4)),
                            border_radius: BorderRadius::all(px(5)),
                            ..default()
                        },
                        BackgroundColor(if tab.0 == t { FRIENDLY.with_alpha(0.45) } else { ROW }),
                    ))
                    .with_child((Text::new(label), font(13.0), TextColor(TEXT)));
                }
            });
        panel
            .spawn((
                ScrollArea,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    height: px(LIST_HEIGHT),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
            ))
            .with_children(|list| {
                let default_name = kit_weapon
                    .as_ref()
                    .map_or("the kit's own".to_string(), |w| weapon_display_name(&w.display_name));
                row(
                    list,
                    &assets,
                    LoadoutButton::Default,
                    format!("Kit default  ({default_name})"),
                    String::new(),
                    kit_weapon.as_deref(),
                    None,
                );
                let mut entries: Vec<(&PoolWeapon, &WeaponDesc)> =
                    pool.iter().filter_map(|e| Some((e, &**armory.weapon(&e.weapon)?))).collect();
                entries.sort_by_key(|(e, w)| {
                    let own = e.factions.iter().any(|f| factions.contains(f));
                    (!own, e.unlock, weapon_display_name(&w.display_name))
                });
                for (entry, weapon) in entries {
                    let launcher = arsenal
                        .launchers
                        .get(&entry.weapon)
                        .and_then(|l| armory.weapon(l))
                        .map(|l| format!(" + {}", weapon_display_name(&l.display_name)))
                        .unwrap_or_default();
                    let refusal = arsenal.check(&rules, &class, slot, &entry.weapon, &factions, rank).err();
                    // Who carries it; unlocks say so.
                    let tag = match entry.unlock {
                        0 if entry.native.len() > 3 => format!("{} factions", entry.native.len()),
                        0 => entry.native.iter().map(|f| faction_title(f)).collect::<Vec<_>>().join(", "),
                        1 => "Unlock (BF2 1.5)".to_string(),
                        _ => "Unlock (Special Forces, booster packs)".to_string(),
                    };
                    row(
                        list,
                        &assets,
                        LoadoutButton::Weapon(entry.weapon.clone()),
                        format!("{}{}{launcher}", weapon_display_name(&weapon.display_name), silenced(&weapon.name)),
                        tag,
                        Some(weapon),
                        refusal,
                    );
                }
            });
        // Stats of the weapon under the mouse, else the one picked.
        panel
            .spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(3),
                padding: UiRect::top(px(4)),
                ..default()
            })
            .with_children(|stats| {
                stats.spawn((StatsText, Text::new(""), font(13.0), TextColor(TEXT)));
                for (i, label) in ["Damage", "Fire rate", "Accuracy", "Range"].into_iter().enumerate() {
                    stats
                        .spawn(Node {
                            align_items: AlignItems::Center,
                            column_gap: px(8),
                            ..default()
                        })
                        .with_children(|line| {
                            line.spawn((
                                Text::new(label),
                                font(11.0),
                                TextColor(DIM),
                                TextLayout::no_wrap(),
                                Node {
                                    width: px(72),
                                    ..default()
                                },
                            ));
                            line.spawn((
                                Node {
                                    width: px(170),
                                    height: px(5),
                                    border_radius: BorderRadius::all(px(3)),
                                    ..default()
                                },
                                BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.12)),
                            ))
                            .with_child((
                                StatBar(i),
                                Node {
                                    width: percent(0),
                                    height: percent(100),
                                    border_radius: BorderRadius::all(px(3)),
                                    ..default()
                                },
                                BackgroundColor(ACCENT),
                            ));
                        });
                }
            });
        panel.spawn((StatusText, Text::new(""), font(12.0), TextColor(DIM)));
    });
}

/// One weapon of the list.
fn row(
    list: &mut ChildSpawnerCommands,
    assets: &AssetServer,
    button: LoadoutButton,
    name: String,
    tag: String,
    weapon: Option<&WeaponDesc>,
    refusal: Option<Refusal>,
) {
    let id = match &button {
        LoadoutButton::Weapon(w) => format!("loadout:{w}"),
        _ => "loadout:default".to_string(),
    };
    let available = refusal.is_none();
    let mut entity = list.spawn((
        button,
        Button,
        Name::new(id),
        Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            padding: UiRect::axes(px(8), px(3)),
            min_height: px(32),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(5)),
            ..default()
        },
        BackgroundColor(ROW),
    ));
    if !available {
        entity.insert(Unavailable);
    }
    entity.with_children(|row| {
        let tint = if available { TEXT } else { FAINT };
        match weapon.and_then(|w| w.icon.as_ref()) {
            Some(icon) => {
                row.spawn((
                    ImageNode {
                        image: assets.load(format!("imported://{icon}")),
                        color: tint,
                        ..default()
                    },
                    Node {
                        width: px(77),
                        height: px(22),
                        flex_shrink: 0.0,
                        ..default()
                    },
                ));
            }
            None => {
                row.spawn(Node {
                    width: px(77),
                    flex_shrink: 0.0,
                    ..default()
                });
            }
        }
        row.spawn(Node {
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|text| {
            text.spawn((Text::new(name), font(13.0), TextColor(tint)));
            let tag = match &refusal {
                Some(refusal) => format!("{tag}  |  {refusal}"),
                None => tag,
            };
            if !tag.is_empty() {
                text.spawn((Text::new(tag), font(10.0), TextColor(if available { DIM } else { WARN.with_alpha(0.7) })));
            }
        });
    });
}

/// Clicks: the tabs, and picking a weapon (stored per class in the settings).
#[allow(clippy::type_complexity)]
fn pick_weapon(
    buttons: Query<(&Interaction, &LoadoutButton, Has<Unavailable>), Changed<Interaction>>,
    armory: Res<Armory>,
    player: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut tab: ResMut<LoadoutTab>,
    mut settings: ResMut<Settings>,
) {
    let Ok((team, deployment)) = player.single() else {
        return;
    };
    for (interaction, button, unavailable) in &buttons {
        if *interaction != Interaction::Pressed || unavailable {
            continue;
        }
        if let LoadoutButton::Tab(t) = button {
            tab.0 = *t;
            continue;
        }
        let Some((class, _)) = our_kit(&armory, *team, deployment) else {
            continue;
        };
        let weapon = match button {
            LoadoutButton::Weapon(w) => Some(w.clone()),
            _ => None,
        };
        let mut pick = settings.loadouts.get(&class).cloned().unwrap_or_default();
        match tab.0 {
            Tab::Primary => pick.primary = weapon,
            Tab::Sidearm => pick.sidearm = weapon,
        }
        info!("loadout: {class} picks {:?}", pick);
        if pick.is_empty() {
            settings.loadouts.remove(&class);
        } else {
            settings.loadouts.insert(class, pick);
        }
    }
}

/// The current pick of our kit's class in the tab shown.
fn current_pick(settings: &Settings, class: &str, tab: Tab) -> Option<String> {
    let pick = settings.loadouts.get(class)?;
    match tab {
        Tab::Primary => pick.primary.clone(),
        Tab::Sidearm => pick.sidearm.clone(),
    }
}

/// Highlights the picked row and the one under the mouse.
#[allow(clippy::type_complexity)]
fn update_rows(
    settings: Res<Settings>,
    tab: Res<LoadoutTab>,
    armory: Res<Armory>,
    player: Query<(&Team, &Deployment), With<LocalPlayer>>,
    mut rows: Query<(&LoadoutButton, &Interaction, Has<Unavailable>, &mut BackgroundColor)>,
) {
    let Ok((team, deployment)) = player.single() else {
        return;
    };
    let Some((class, _)) = our_kit(&armory, *team, deployment) else {
        return;
    };
    let picked = current_pick(&settings, &class, tab.0);
    for (button, interaction, unavailable, mut background) in &mut rows {
        let selected = match button {
            LoadoutButton::Tab(t) => *t == tab.0,
            LoadoutButton::Default => picked.is_none(),
            LoadoutButton::Weapon(w) => picked.as_ref() == Some(w),
        };
        let color = if selected {
            FRIENDLY.with_alpha(0.45)
        } else if *interaction == Interaction::Hovered && !unavailable {
            ROW_HOVER
        } else {
            ROW
        };
        background.set_if_neq(BackgroundColor(color));
    }
}

/// The stats of the weapon under the mouse (else the picked one), and what the server made
/// of our pick.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_stats(
    screen: Res<DeployScreen>,
    settings: Res<Settings>,
    tab: Res<LoadoutTab>,
    armory: Res<Armory>,
    arsenal: Res<Arsenal>,
    rules: Query<&LoadoutRules>,
    player: Query<(&Team, &Deployment, Option<&LoadoutPicks>), With<LocalPlayer>>,
    rows: Query<(&LoadoutButton, &Interaction)>,
    mut stats: Single<&mut Text, (With<StatsText>, Without<StatusText>)>,
    mut status: Single<(&mut Text, &mut TextColor), (With<StatusText>, Without<StatsText>)>,
    mut bars: Query<(&StatBar, &mut Node)>,
) {
    if !screen.open {
        return;
    }
    let Ok((team, deployment, accepted)) = player.single() else {
        return;
    };
    let Some((class, kit)) = our_kit(&armory, *team, deployment) else {
        return;
    };
    let slot_number = if tab.0 == Tab::Primary { 3 } else { 2 };
    let kit_weapon = kit
        .weapons
        .iter()
        .filter_map(|w| armory.weapon(w))
        .find(|w| w.selectable() && w.slot == slot_number)
        .map(|w| w.name.clone());
    let picked = current_pick(&settings, &class, tab.0);
    let hovered = rows.iter().find_map(|(button, interaction)| match (button, interaction) {
        (LoadoutButton::Weapon(w), Interaction::Hovered | Interaction::Pressed) => Some(w.clone()),
        (LoadoutButton::Default, Interaction::Hovered | Interaction::Pressed) => kit_weapon.clone(),
        _ => None,
    });
    let shown = hovered.or(picked.clone()).or(kit_weapon.clone());
    let weapon = shown.as_deref().and_then(|w| armory.weapon(w));
    // Bars relative to the best of the pool.
    let pool: Vec<&WeaponDesc> = arsenal
        .pool(&class, if tab.0 == Tab::Primary { PickSlot::Primary } else { PickSlot::Sidearm })
        .into_iter()
        .flatten()
        .filter_map(|p| armory.weapon(&p.weapon).map(|w| &**w))
        .collect();
    let damage = |w: &WeaponDesc| w.projectile.damage * w.projectiles_per_shot.max(1) as f32;
    let accuracy = |w: &WeaponDesc| 1.0 / (0.2 + w.deviation.min * w.deviation.stand);
    let range = |w: &WeaponDesc| if w.projectile.falloff_end > 0.0 { w.projectile.falloff_end } else { w.projectile.velocity * w.projectile.time_to_live.min(3.0) * 0.5 };
    let best = |f: &dyn Fn(&WeaponDesc) -> f32| pool.iter().map(|w| f(w)).fold(1e-3, f32::max);
    let values: [f32; 4] = match weapon {
        Some(w) => [
            damage(w) / best(&damage),
            w.rounds_per_minute / best(&|w: &WeaponDesc| w.rounds_per_minute),
            accuracy(w) / best(&accuracy),
            range(w) / best(&range),
        ],
        None => [0.0; 4],
    };
    for (bar, mut node) in &mut bars {
        let width = percent((values[bar.0].clamp(0.0, 1.0) * 100.0).round());
        if node.width != width {
            node.width = width;
        }
    }
    let line = match weapon {
        Some(w) => {
            let modes = w
                .fire_modes
                .iter()
                .map(|m| match m {
                    FireMode::Single => "single",
                    FireMode::Burst => "burst",
                    FireMode::Auto => "auto",
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let pellets = if w.projectiles_per_shot > 1 { format!(" x{}", w.projectiles_per_shot) } else { String::new() };
            format!(
                "{}   {:.0} dmg{pellets}  |  {:.0} rpm  |  {} x {}  |  {modes}",
                weapon_display_name(&w.display_name),
                w.projectile.damage,
                w.rounds_per_minute,
                w.magazine_size,
                w.magazines,
            )
        }
        None => String::new(),
    };
    if stats.0 != line {
        stats.0 = line;
    }
    // What the server has for this class.
    let rules = rules.single().cloned().unwrap_or_default();
    let server = accepted.and_then(|a| a.0.get(&class)).and_then(|p| match tab.0 {
        Tab::Primary => p.primary.clone(),
        Tab::Sidearm => p.sidearm.clone(),
    });
    let (text, color) = if !rules.arsenal {
        (String::new(), DIM)
    } else {
        match (&picked, &server) {
            (None, _) => ("Kit default".to_string(), DIM),
            (Some(p), Some(s)) if p == s => ("Accepted by the server: your next spawn as this class carries it.".to_string(), DIM),
            (Some(_), _) => ("Not accepted by the server: you spawn with the kit's own.".to_string(), WARN),
        }
    };
    let (status_text, status_color) = &mut *status;
    if status_text.0 != text {
        status_text.0 = text;
    }
    status_color.set_if_neq(TextColor(color));
}
