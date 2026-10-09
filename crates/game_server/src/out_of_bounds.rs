//! BF2's out-of-bounds warning and countdown: a soldier on foot, or a vehicle (with its
//! occupants), outside the combat area that applies to it gets a warning and dies once the
//! countdown runs out; returning inside cancels it. Levels without combat areas (no
//! `CombatArea` in the layout) are unaffected, and the whole feature can be turned off with
//! [`crate::ServerSettings::out_of_bounds`].
//!
//! **Countdown length**: BF2's `CombatAreaManager.timeAllowedOutside` is 10 seconds in every
//! example found across the retail, Special Forces and booster levels (e.g. Karkand's
//! `CombatAreaManager.use 1 / timeAllowedOutside 10`, see
//! `docs/formats/levels-terrain-scripts.md` §"Combat area").
//!
//! **Which area applies**: the same per-traveller rules the navigation grids use
//! ([`crate::nav::area::Traveller`]) — a soldier on foot measures against the soldiers' area
//! (falling back the way [`crate::nav::area::Traveller::kinds`] does), a vehicle's occupants
//! against its vehicle class's area. The containment check is grown by the same margin the
//! grids are ([`crate::nav::area::COMBAT_AREA_MARGIN`]), so bots legitimately pathing the
//! strip just outside the strict polygon (a path cutting a corner, cover just outside — see
//! that module's own doc comment) never trigger it; only really leaving the play area does.
//! Jets, helicopters and boats keep to their own (usually much larger) areas, so ordinary
//! flight or sailing inside them never comes close to the line either.

use std::sync::Arc;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_data::VehicleCategory;
use game_shared::{
    level::LoadedLevel,
    protocol::{ControlledBy, MatchInfo, OutOfBoundsWarning, Player},
    soldier::{Soldier, SoldierMotion},
    vehicle::{Seated, VehicleData, VehicleHealth},
};

use crate::{
    HostPlayer, PlayerClient, ServerSettings,
    abilities::Deaths,
    combat::{CombatSystems, VehicleDestroyed},
    nav::area::{COMBAT_AREA_MARGIN, PlayArea, Traveller},
    player_client,
};

pub struct OutOfBoundsPlugin;

impl Plugin for OutOfBoundsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OutOfBoundsAreas>().add_systems(
            FixedUpdate,
            (rebuild_areas, (check_soldiers_out_of_bounds, check_vehicles_out_of_bounds))
                .chain()
                .after(CombatSystems)
                .run_if(in_state(ClientState::Disconnected))
                .run_if(|settings: Res<ServerSettings>| settings.out_of_bounds),
        );
    }
}

/// BF2's `CombatAreaManager.timeAllowedOutside` (see the module docs).
pub const OUT_OF_BOUNDS_SECONDS: f32 = 10.0;

/// The kill feed's weapon string for an out-of-bounds death.
const CAUSE: &str = "Out of Bounds";

/// Server-side: a soldier on foot, or a vehicle, counting down outside the area that applies
/// to it. Removed (cancelling the warning) once back inside.
#[derive(Component)]
struct OutOfBounds {
    left: f32,
}

/// The current layout's play areas, one per kind of traveller (grown by the nav grids'
/// margin, see the module docs), cached until the match's mode or size changes.
#[derive(Resource, Default)]
struct OutOfBoundsAreas {
    built_for: Option<(String, u32)>,
    soldier: Option<PlayArea>,
    land: Option<PlayArea>,
    boat: Option<PlayArea>,
    jet: Option<PlayArea>,
    helicopter: Option<PlayArea>,
}

impl OutOfBoundsAreas {
    fn area(&self, traveller: Traveller) -> Option<&PlayArea> {
        match traveller {
            Traveller::Soldier => self.soldier.as_ref(),
            Traveller::Land => self.land.as_ref(),
            Traveller::Boat => self.boat.as_ref(),
            Traveller::Jet => self.jet.as_ref(),
            Traveller::Helicopter => self.helicopter.as_ref(),
        }
    }

    fn rebuild(&mut self, level: &LoadedLevel, match_info: &MatchInfo) {
        let key = (match_info.mode.clone(), match_info.size);
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        self.built_for = Some(key);
        let layout = level.game_mode(&match_info.mode, match_info.size);
        let area = |traveller| layout.and_then(|l| PlayArea::for_layout(l, traveller, COMBAT_AREA_MARGIN));
        self.soldier = area(Traveller::Soldier);
        self.land = area(Traveller::Land);
        self.boat = area(Traveller::Boat);
        self.jet = area(Traveller::Jet);
        self.helicopter = area(Traveller::Helicopter);
    }
}

/// Which play area a vehicle's occupants are bound by (BF2's `vehicleCategory`; see
/// [`Traveller`]). Stationary weapons (artillery, TOW, AA) go by the land vehicles' area like
/// `nav::area` does: they don't move, so it only matters if one was placed outside it.
fn vehicle_traveller(category: VehicleCategory) -> Traveller {
    match category {
        VehicleCategory::Air => Traveller::Jet,
        VehicleCategory::Helicopter => Traveller::Helicopter,
        VehicleCategory::Sea => Traveller::Boat,
        VehicleCategory::Land | VehicleCategory::Stationary => Traveller::Land,
    }
}

/// Tells `player`'s client (if it has one; bots don't) the warning, or `None` to cancel it.
fn warn(
    player: Entity,
    seconds_left: Option<f32>,
    clients: &Query<&PlayerClient>,
    host: Option<&HostPlayer>,
    warnings: &mut MessageWriter<ToClients<OutOfBoundsWarning>>,
) {
    if let Some(client) = player_client(player, clients, host) {
        warnings.write(ToClients {
            targets: SendTargets::Single(client),
            message: OutOfBoundsWarning { seconds_left },
        });
    }
}

/// Rebuilds [`OutOfBoundsAreas`] when the match's mode or size changes (a no-op otherwise),
/// before either of the two checks below reads it. Split out so those two can take it as a
/// plain `Res` (a `ResMut` in both, like a single combined system would need, would make them
/// conflict and panic; see the query conflict this avoids below for soldiers vs. vehicles).
fn rebuild_areas(level: Option<Res<LoadedLevel>>, match_info: Option<Single<&MatchInfo>>, mut areas: ResMut<OutOfBoundsAreas>) {
    if let (Some(level), Some(match_info)) = (level, match_info) {
        areas.rebuild(&level, &match_info);
    }
}

/// Soldiers on foot: seated ones are their vehicle's responsibility
/// ([`check_vehicles_out_of_bounds`]). A separate system from that one so their two `Option<&mut
/// OutOfBounds>` queries, over otherwise-disjoint entities Bevy can't prove disjoint from the
/// query shapes alone, don't conflict within one system (which panics) — two systems touching
/// the same component are merely scheduled one after the other, not panicked on.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn check_soldiers_out_of_bounds(
    mut commands: Commands,
    time: Res<Time>,
    areas: Res<OutOfBoundsAreas>,
    clients: Query<&PlayerClient>,
    host: Option<Res<HostPlayer>>,
    mut warnings: MessageWriter<ToClients<OutOfBoundsWarning>>,
    mut soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&mut OutOfBounds>), (With<Soldier>, Without<Seated>)>,
    players: Query<&Player>,
    mut deaths: Deaths,
) {
    let name = |player: Entity| players.get(player).map_or_else(|_| "?".into(), |p| p.name.clone());
    let dt = time.delta_secs();

    for (soldier, motion, owner, oob) in &mut soldiers {
        let Some(area) = areas.area(Traveller::Soldier) else {
            continue;
        };
        let inside = area.contains(motion.position.xz());
        match oob {
            Some(_) if inside => {
                info!("{} returned to the battlefield", name(owner.0));
                commands.entity(soldier).remove::<OutOfBounds>();
                warn(owner.0, None, &clients, host.as_deref(), &mut warnings);
            }
            Some(mut oob) => {
                oob.left -= dt;
                if oob.left <= 0.0 {
                    info!("{} died: out of bounds", name(owner.0));
                    deaths.kill(soldier, owner.0, None, CAUSE, false);
                    deaths.die(soldier, owner.0, 0.0);
                } else {
                    warn(owner.0, Some(oob.left), &clients, host.as_deref(), &mut warnings);
                }
            }
            None if !inside => {
                info!("{} is out of bounds, {OUT_OF_BOUNDS_SECONDS:.0} s to return", name(owner.0));
                commands.entity(soldier).insert(OutOfBounds { left: OUT_OF_BOUNDS_SECONDS });
                warn(owner.0, Some(OUT_OF_BOUNDS_SECONDS), &clients, host.as_deref(), &mut warnings);
            }
            None => {}
        }
    }
}

/// Vehicles, with their crews: the whole vehicle (and everyone in it) shares the fate of
/// wherever its hull is, like BF2. See [`check_soldiers_out_of_bounds`] for why this is a
/// separate system rather than a second query in that one.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn check_vehicles_out_of_bounds(
    mut commands: Commands,
    time: Res<Time>,
    areas: Res<OutOfBoundsAreas>,
    clients: Query<&PlayerClient>,
    host: Option<Res<HostPlayer>>,
    mut warnings: MessageWriter<ToClients<OutOfBoundsWarning>>,
    mut vehicle_destroyed: MessageWriter<VehicleDestroyed>,
    mut vehicles: Query<(Entity, &Position, &VehicleData, &mut VehicleHealth, Option<&mut OutOfBounds>)>,
    seated: Query<(Entity, &Seated, &ControlledBy)>,
    players: Query<&Player>,
    mut deaths: Deaths,
) {
    let name = |player: Entity| players.get(player).map_or_else(|_| "?".into(), |p| p.name.clone());
    let dt = time.delta_secs();

    for (vehicle, position, data, mut health, oob) in &mut vehicles {
        if health.wrecked() {
            if oob.is_some() {
                commands.entity(vehicle).remove::<OutOfBounds>();
            }
            continue;
        }
        let traveller = vehicle_traveller(data.0.desc.category);
        let Some(area) = areas.area(traveller) else {
            continue;
        };
        let inside = area.contains(position.0.xz());
        let occupants: Vec<(Entity, Entity)> = seated
            .iter()
            .filter(|(_, seat, _)| seat.vehicle == vehicle)
            .map(|(soldier, _, owner)| (soldier, owner.0))
            .collect();
        match oob {
            // Back inside, or abandoned: nothing left to warn or kill.
            Some(_) if inside || occupants.is_empty() => {
                commands.entity(vehicle).remove::<OutOfBounds>();
                for &(_, player) in &occupants {
                    info!("{} returned to the battlefield", name(player));
                    warn(player, None, &clients, host.as_deref(), &mut warnings);
                }
            }
            Some(mut oob) => {
                oob.left -= dt;
                if oob.left <= 0.0 {
                    info!("vehicle destroyed: out of bounds, {} aboard", occupants.len());
                    health.current = -health.max.max(1.0);
                    vehicle_destroyed.write(VehicleDestroyed {
                        vehicle,
                        by: None,
                        weapon: Arc::from(CAUSE),
                    });
                    for &(soldier, player) in &occupants {
                        info!("{} died: out of bounds", name(player));
                        deaths.kill(soldier, player, None, CAUSE, false);
                        deaths.die(soldier, player, 0.0);
                    }
                } else {
                    for &(_, player) in &occupants {
                        warn(player, Some(oob.left), &clients, host.as_deref(), &mut warnings);
                    }
                }
            }
            None if !inside && !occupants.is_empty() => {
                commands.entity(vehicle).insert(OutOfBounds { left: OUT_OF_BOUNDS_SECONDS });
                for &(_, player) in &occupants {
                    info!("{} is out of bounds, {OUT_OF_BOUNDS_SECONDS:.0} s to return", name(player));
                    warn(player, Some(OUT_OF_BOUNDS_SECONDS), &clients, host.as_deref(), &mut warnings);
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use game_data::{CombatAreaDesc, GameModeDesc};

    use super::*;

    fn square(vehicles: u8, half: f32) -> CombatAreaDesc {
        CombatAreaDesc {
            team: None,
            vehicles,
            used_by_pathfinding: false,
            points: vec![[-half, -half], [half, -half], [half, half], [-half, half]],
        }
    }

    /// The countdown is BF2's `CombatAreaManager.timeAllowedOutside` default (10 s), and runs
    /// out to a kill; coming back cancels it. This exercises the same state machine
    /// `check_soldiers_out_of_bounds`/`check_vehicles_out_of_bounds` drive, directly against
    /// [`OutOfBounds`] and the area.
    #[test]
    fn countdown_runs_out_and_can_be_cancelled() {
        let area = PlayArea::for_layout(
            &GameModeDesc {
                combat_areas: vec![square(CombatAreaDesc::SOLDIERS, 100.0)],
                ..Default::default()
            },
            Traveller::Soldier,
            COMBAT_AREA_MARGIN,
        )
        .unwrap();
        // Inside the strict polygon, and inside the nav grids' margin: never out of bounds.
        assert!(area.contains(Vec2::new(0.0, 0.0)));
        assert!(area.contains(Vec2::new(110.0, 0.0)), "the margin is fair game, like bots' nav grids");
        // Genuinely outside: starts a countdown of the BF2 default.
        assert!(!area.contains(Vec2::new(500.0, 0.0)));
        let mut oob = OutOfBounds { left: OUT_OF_BOUNDS_SECONDS };
        assert_eq!(oob.left, 10.0);
        oob.left -= 9.9;
        assert!(oob.left > 0.0, "not dead yet");
        oob.left -= 0.2;
        assert!(oob.left <= 0.0, "the countdown ran out");
    }

    #[test]
    fn vehicle_classes_use_their_own_area_like_nav_does() {
        assert_eq!(vehicle_traveller(VehicleCategory::Land), Traveller::Land);
        assert_eq!(vehicle_traveller(VehicleCategory::Stationary), Traveller::Land);
        assert_eq!(vehicle_traveller(VehicleCategory::Sea), Traveller::Boat);
        assert_eq!(vehicle_traveller(VehicleCategory::Air), Traveller::Jet);
        assert_eq!(vehicle_traveller(VehicleCategory::Helicopter), Traveller::Helicopter);
    }
}
