//! The interface's flag and vehicle icons, imported from BF2's HUD and menus (see
//! `game_data::ui`), and the map markers for flags and vehicles.
//!
//! Flags show as their owner's flag on a pole (a crossed-out one for main bases, which can't
//! be captured) on a dot of the owner's colour. Vehicles show as BF2's white silhouettes
//! (a box shaped by their class without one), sized by class and turned with the hull: the
//! one we sit in in white (like our own marker), our team's in our colour (green when a
//! squad mate is in), empty ones grey (only those our side or both sides spawn), enemy ones
//! in red while spotted; tanks, APCs and anti-air with a line for the turret.

use bevy::{platform::collections::HashMap, prelude::*};
use game_data::VehicleClass;
use game_shared::{
    conquest::{ControlPoint, FlagState},
    level::LoadedLevel,
    protocol::{ControlledBy, MatchInfo, Team},
    radio::Spotted,
    squad::SquadMember,
    vehicle::{Seated, Vehicle, VehicleData, VehicleHealth},
};

use crate::{
    conquest_hud::{FRIENDLY, NEUTRAL, SQUAD, team_color},
    map_markers::{CONTROL_POINT_LABEL, FLAG_LAYER, MapMarker, MapMarkers, MarkerSystems, VEHICLE_LAYER},
    net::{LocalPlayer, LocalSoldier},
    vehicles::VehicleView,
};

pub struct MapIconsPlugin;

impl Plugin for MapIconsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiIcons>()
            .add_systems(Update, load_icons.run_if(resource_exists_and_changed::<LoadedLevel>))
            .add_systems(PostUpdate, (flag_markers, vehicle_markers).in_set(MarkerSystems));
    }
}

/// A side's flag icons (see [`game_data::TeamIcons`]).
#[derive(Clone, Default)]
pub struct IconSet {
    pub flag: Option<Handle<Image>>,
    pub map: Option<Handle<Image>>,
    pub base: Option<Handle<Image>>,
    pub large: Option<Handle<Image>>,
}

impl IconSet {
    pub fn load(asset_server: &AssetServer, icons: &game_data::TeamIcons) -> Self {
        let load = |path: &Option<String>| path.as_ref().map(|p| asset_server.load(format!("imported://{p}")));
        Self {
            flag: load(&icons.flag),
            map: load(&icons.map),
            base: load(&icons.base),
            large: load(&icons.large),
        }
    }

    /// A control point's map icon.
    pub fn map_flag(&self, uncapturable: bool) -> Option<Handle<Image>> {
        if uncapturable { self.base.clone().or_else(|| self.map.clone()) } else { self.map.clone() }
    }
}

struct VehicleMapIcon {
    image: Option<Handle<Image>>,
    class: VehicleClass,
    turret: bool,
}

/// The loaded level's icons.
#[derive(Resource, Default)]
pub struct UiIcons {
    /// Neutral, team 1, team 2.
    sides: [IconSet; 3],
    vehicles: HashMap<String, VehicleMapIcon>,
    /// The side whose spawners make each vehicle template, when only one side's do.
    template_team: HashMap<String, Team>,
}

impl UiIcons {
    pub fn side(&self, team: Team) -> &IconSet {
        match team {
            Team::Spectator => &self.sides[0],
            Team::One => &self.sides[1],
            Team::Two => &self.sides[2],
        }
    }
}

fn load_icons(
    level: Res<LoadedLevel>,
    info: Query<&MatchInfo>,
    asset_server: Res<AssetServer>,
    mut icons: ResMut<UiIcons>,
) {
    let desc = &level.desc;
    let team = |i: usize| desc.teams.get(i).map(|t| IconSet::load(&asset_server, &t.icons)).unwrap_or_default();
    icons.sides = [IconSet::load(&asset_server, &desc.neutral_icons), team(0), team(1)];
    icons.vehicles = desc
        .vehicle_icons
        .iter()
        .map(|(name, icon)| {
            let image = icon.icon.as_ref().map(|p| asset_server.load(format!("imported://{p}")));
            (
                name.clone(),
                VehicleMapIcon {
                    image,
                    class: icon.class,
                    turret: icon.turret,
                },
            )
        })
        .collect();
    icons.template_team.clear();
    let layout = info.single().ok().and_then(|info| level.game_mode(&info.mode, info.size));
    let mut sides: HashMap<String, [bool; 2]> = HashMap::default();
    for spawner in layout.iter().flat_map(|l| &l.vehicle_spawners) {
        for (side, template) in spawner.templates.iter().enumerate() {
            if let Some(template) = template {
                sides.entry(template.to_ascii_lowercase()).or_default()[side] = true;
            }
        }
    }
    for (template, [one, two]) in sides {
        match (one, two) {
            (true, false) => icons.template_team.insert(template, Team::One),
            (false, true) => icons.template_team.insert(template, Team::Two),
            _ => None,
        };
    }
}

fn local_side(local: &Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>) -> (Team, Option<SquadMember>) {
    local.single().map(|(t, s)| (*t, s.copied())).unwrap_or_default()
}

fn flag_markers(
    icons: Res<UiIcons>,
    local: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    control_points: Query<(Entity, &ControlPoint, &FlagState)>,
    mut markers: ResMut<MapMarkers>,
) {
    let (team, _) = local_side(&local);
    for (entity, cp, state) in &control_points {
        let image = icons.side(state.owner).map_flag(cp.uncapturable);
        // The icon's pole stands on the point; its cloth gets the owner's colour behind it.
        let (size, frame) = if image.is_some() { (24.0, Some(FLAG_CLOTH)) } else { (12.0, None) };
        markers.0.push(MapMarker {
            image,
            frame,
            bounds: Some(FLAG_BOUNDS),
            layer: FLAG_LAYER,
            ..MapMarker::dot(entity, cp.position, team_color(state.owner, team), size)
                .label(cp.name.clone())
                .priority(CONTROL_POINT_LABEL)
        });
    }
}

/// Where the cloth of BF2's map flag icons is (33x33 icons, the pole's foot in the middle),
/// with a pixel around it.
pub const FLAG_CLOTH: Rect = Rect {
    min: Vec2::new(12.5 / 33.0, 3.5 / 33.0),
    max: Vec2::new(33.0 / 33.0, 20.5 / 33.0),
};

/// What map flag icons show (the pole's foot, the cloth and a main base's crossed-out circle),
/// for keeping labels clear of them.
pub const FLAG_BOUNDS: Rect = Rect {
    min: Vec2::new(6.0 / 33.0, 4.0 / 33.0),
    max: Vec2::new(33.0 / 33.0, 26.0 / 33.0),
};

/// Minimap size of a vehicle's icon (BF2's 16 px silhouettes leave a margin), by class: big
/// armour and aircraft stand out, guns stay small.
pub fn icon_size(class: VehicleClass) -> f32 {
    match class {
        VehicleClass::Tank | VehicleClass::Jet | VehicleClass::Helicopter => 28.0,
        VehicleClass::Apc | VehicleClass::AntiAir => 26.0,
        VehicleClass::Jeep | VehicleClass::Boat => 22.0,
        VehicleClass::Stationary => 17.0,
    }
}

/// Size and shape (width over height) of a vehicle without an icon.
pub fn class_shape(class: VehicleClass) -> (f32, f32) {
    match class {
        VehicleClass::Tank => (19.0, 0.7),
        VehicleClass::Apc => (18.0, 0.6),
        VehicleClass::AntiAir => (18.0, 0.7),
        VehicleClass::Jeep => (15.0, 0.55),
        VehicleClass::Boat => (15.0, 0.45),
        VehicleClass::Jet => (20.0, 0.5),
        VehicleClass::Helicopter => (16.0, 1.0),
        VehicleClass::Stationary => (10.0, 1.0),
    }
}

/// The vehicle we sit in.
const OURS: Color = Color::srgb(0.97, 0.97, 0.98);

/// Clockwise from north of a direction.
fn heading(direction: Vec3) -> f32 {
    direction.x.atan2(-direction.z)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn vehicle_markers(
    icons: Res<UiIcons>,
    local: Query<(&Team, Option<&SquadMember>), With<LocalPlayer>>,
    vehicles: Query<(Entity, &Vehicle, &VehicleView, Option<&VehicleHealth>, Option<&VehicleData>, Option<&Spotted>)>,
    riders: Query<(&Seated, &ControlledBy)>,
    players: Query<(&Team, Option<&SquadMember>)>,
    seat: Query<&Seated, With<LocalSoldier>>,
    mut turrets: Local<HashMap<String, Option<usize>>>,
    mut markers: ResMut<MapMarkers>,
) {
    let (team, squad) = local_side(&local);
    let ours = seat.single().ok().map(|s| s.vehicle);
    // Who is in each vehicle.
    let mut crews: HashMap<Entity, (Team, bool)> = HashMap::default();
    for (seated, controlled_by) in &riders {
        let Ok((rider_team, rider_squad)) = players.get(controlled_by.0) else {
            continue;
        };
        let squad_mate = squad.zip(rider_squad.copied()).is_some_and(|(a, b)| a.squad == b.squad) && *rider_team == team;
        let crew = crews.entry(seated.vehicle).or_insert((*rider_team, false));
        crew.1 |= squad_mate;
    }
    for (entity, vehicle, view, health, data, spotted) in &vehicles {
        if health.is_some_and(|h| h.wrecked()) {
            continue;
        }
        let template = vehicle.template.to_ascii_lowercase();
        let color = match crews.get(&entity) {
            _ if ours == Some(entity) => OURS,
            Some((side, squad_mate)) if team == Team::Spectator => {
                if *squad_mate { SQUAD } else { team_color(*side, team) }
            }
            Some((side, true)) if *side == team => SQUAD,
            Some((side, false)) if *side == team => FRIENDLY,
            Some((side, _)) if spotted.is_some_and(|s| s.by == team) => team_color(*side, team),
            Some(_) => continue,
            None => {
                let owner = icons.template_team.get(&template).copied();
                if team != Team::Spectator && owner.is_some_and(|o| o != team) && !spotted.is_some_and(|s| s.by == team) {
                    continue;
                }
                NEUTRAL
            }
        };
        let icon = icons.vehicles.get(&template);
        let class = icon.map_or(VehicleClass::Jeep, |i| i.class);
        let forward = view.transform.rotation * Vec3::NEG_Z;
        let pointer = icon.filter(|i| i.turret).zip(data).and_then(|(_, data)| {
            let model = &data.0;
            let turret = *turrets.entry(template.clone()).or_insert_with(|| {
                model.desc.parts.iter().position(|p| {
                    p.parent == Some(0)
                        && p.joint
                            .as_ref()
                            .is_some_and(|j| j.axes[0].input == Some(game_data::JointInput::AimYaw))
                })
            });
            let part = model.part_transforms(&view.joints).get(turret?).copied()?;
            Some(heading(view.transform.rotation * part.rotation * Vec3::NEG_Z))
        });
        let (size, aspect) = match icon.and_then(|i| i.image.clone()) {
            Some(_) => (icon_size(class), 1.0),
            None => class_shape(class),
        };
        markers.0.push(MapMarker {
            image: icon.and_then(|i| i.image.clone()),
            tint: true,
            aspect,
            heading: Some(heading(forward)),
            pointer,
            layer: VEHICLE_LAYER,
            ..MapMarker::dot(entity, view.transform.translation, color, size)
        });
    }
}
