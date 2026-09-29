//! The melee and grenade keys (BF3-style quick actions): the knife or the kit's grenade comes
//! out, swings or is thrown (held: cooked, with the usual wind-up), and the weapon it
//! interrupted comes back.
//!
//! It is all ordinary input: the selected weapon and the fire button, plus
//! `Buttons::QUICK`, which makes the switch quicker on both sides
//! (`weapons::WeaponState::switch_with`). So the server checks the knife and the throw like
//! any other shot, and prediction runs the same rules. This only drives
//! [`WeaponSelection`] one tick ahead of the input frame built from it (`local_input`).

use bevy::{prelude::*, window::CursorOptions};
use game_shared::{
    revive::Downed,
    vehicle::Seated,
    weapons::{Armory, Inventory, Loadout, QUICK_DEPLOY, cooks},
};

use crate::{
    combat::{CombatFeedback, WeaponSelection},
    local_input::{LocalInputSystems, cursor_locked},
    net::LocalSoldier,
    settings::{Action, Actions},
};

pub struct QuickActionsPlugin;

impl Plugin for QuickActionsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<QuickAction>()
            .add_systems(FixedUpdate, quick_actions.before(LocalInputSystems));
    }
}

/// Seconds the trigger stays released after the knife or grenade is taken out, so that the
/// swing or wind-up starts with a fresh press once it is ready (a little over
/// [`QUICK_DEPLOY`]).
const READY_AFTER: f32 = QUICK_DEPLOY + 0.05;
/// Seconds from the knife's swing until the previous weapon comes back.
const SWING: f32 = 0.45;
/// Seconds from the grenade leaving the hand until the previous weapon comes back.
const THROW_RECOVERY: f32 = 0.3;
/// Gives up on a knife that doesn't swing or a grenade that isn't wound up by then (on a
/// ladder, sprinting).
const GIVE_UP: f32 = 1.5;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    /// The knife is out: swinging once it is ready.
    Melee { swung_at: Option<f32> },
    /// A grenade is out: wound up while the key is held, thrown when let go.
    Throw { wound_up: bool, left_at: Option<f32> },
    /// The previous weapon is selected, still with `QUICK` for a few ticks (should the
    /// server miss the tick of the switch, the next frames carry it too).
    Back { ticks: u8 },
}

/// The quick action at work.
#[derive(Resource, Default)]
pub struct QuickAction {
    phase: Phase,
    /// The weapon the action interrupted, and the knife or grenade taken out for it.
    previous: u8,
    weapon: u8,
    /// Seconds since the action began.
    time: f32,
    shots: u32,
    keys_were_down: (bool, bool),
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn quick_actions(
    time: Res<Time>,
    actions: Actions,
    armory: Res<Armory>,
    feedback: Res<CombatFeedback>,
    cursor: Single<&CursorOptions>,
    scenario: Option<Res<crate::scenario::ScenarioInput>>,
    soldier: Query<(&Loadout, &Inventory), (With<LocalSoldier>, Without<Seated>, Without<Downed>)>,
    mut selection: ResMut<WeaponSelection>,
    mut state: ResMut<QuickAction>,
) {
    let Ok((loadout, inventory)) = soldier.single() else {
        // Dead, wounded or in a vehicle (where G drops countermeasures).
        if state.phase != Phase::Idle {
            *state = QuickAction::default();
            selection.quick = false;
            selection.quick_fire = None;
        }
        return;
    };
    let dt = time.delta_secs();
    let listening = cursor_locked(&cursor) || scenario.is_some();
    let melee = listening && actions.pressed(Action::Melee);
    let grenade = listening && actions.pressed(Action::Grenade);
    let (melee_was, grenade_was) = state.keys_were_down;
    state.keys_were_down = (melee, grenade);
    let weapon = |index: u8| loadout.weapons.get(index as usize).and_then(|w| armory.weapon(w));
    let name = |index: u8| weapon(index).map_or("?".to_string(), |w| w.name.clone());

    // The player picked another weapon meanwhile: the action is over.
    if !matches!(state.phase, Phase::Idle | Phase::Back { .. }) && selection.index != state.weapon {
        state.phase = Phase::Idle;
        selection.quick = false;
        selection.quick_fire = None;
    }
    state.time += dt;
    let t = state.time;
    match state.phase {
        Phase::Idle => {
            let pick = if melee && !melee_was {
                (0..loadout.weapons.len() as u8)
                    .find(|&i| weapon(i).is_some_and(|w| w.is_melee()))
                    .map(|i| (i, Phase::Melee { swung_at: None }))
            } else if grenade && !grenade_was {
                // The kit's grenade with some left: a frag first, then smoke or flash bangs.
                let left = |i: u8| inventory.ammo.get(i as usize).is_some_and(|[mag, spare]| mag + spare > 0);
                let grenades: Vec<u8> = (0..loadout.weapons.len() as u8)
                    .filter(|&i| weapon(i).is_some_and(|w| w.is_hand_grenade()) && left(i))
                    .collect();
                let frag = grenades
                    .iter()
                    .copied()
                    .find(|&i| weapon(i).is_some_and(|w| w.projectile.explodes() && cooks(&w.projectile)));
                frag.or(grenades.first().copied()).map(|i| (i, Phase::Throw { wound_up: false, left_at: None }))
            } else {
                None
            };
            if let Some((index, phase)) = pick {
                state.previous = selection.index;
                state.weapon = index;
                state.time = 0.0;
                state.shots = feedback.shots_fired;
                state.phase = phase;
                selection.index = index;
                selection.quick = true;
                selection.quick_fire = Some(false);
                selection.switches = selection.switches.wrapping_add(1);
                info!("quick action: {} (from {})", name(index), name(state.previous));
            }
        }
        Phase::Melee { swung_at } => {
            let swung_at = swung_at.or_else(|| {
                (feedback.shots_fired != state.shots).then(|| {
                    info!("quick action: {} swung", name(state.weapon));
                    t
                })
            });
            state.phase = Phase::Melee { swung_at };
            selection.quick_fire = Some(t >= READY_AFTER && swung_at.is_none());
            if swung_at.is_some_and(|at| t - at >= SWING) || (swung_at.is_none() && t > GIVE_UP) {
                go_back(&mut state, &mut selection, &name);
            }
        }
        Phase::Throw { wound_up, left_at } => {
            let wound_up = wound_up || feedback.cooking;
            // Let go of (or never wound up with the key already up): the throw is on its way.
            let released = wound_up && !feedback.cooking;
            let left_at = left_at.or((released && !feedback.launching).then_some(t));
            state.phase = Phase::Throw { wound_up, left_at };
            selection.quick_fire = Some(t >= READY_AFTER && !released && (grenade || !wound_up));
            if left_at.is_some_and(|at| t - at >= THROW_RECOVERY) || (!wound_up && t > GIVE_UP) {
                go_back(&mut state, &mut selection, &name);
            }
        }
        Phase::Back { ticks } => {
            // The switch back went out with `QUICK`; done.
            selection.quick_fire = None;
            if ticks >= 3 || selection.index != state.previous {
                state.phase = Phase::Idle;
                selection.quick = false;
            } else {
                state.phase = Phase::Back { ticks: ticks + 1 };
            }
        }
    }
}

fn go_back(state: &mut QuickAction, selection: &mut WeaponSelection, name: &dyn Fn(u8) -> String) {
    info!("quick action over: back to {}", name(state.previous));
    selection.index = state.previous;
    selection.quick_fire = Some(false);
    selection.switches = selection.switches.wrapping_add(1);
    state.phase = Phase::Back { ticks: 0 };
}

#[cfg(test)]
mod tests {
    use crate::settings::{Action, Binding, Settings};
    use bevy::prelude::KeyCode;

    #[test]
    fn quick_keys_are_free_by_default() {
        let settings = Settings::default();
        assert!(settings.conflicts(Action::Melee).is_empty(), "{:?}", settings.conflicts(Action::Melee));
        // G throws on foot and drops flares in a vehicle.
        assert!(settings.conflicts(Action::Grenade).is_empty(), "{:?}", settings.conflicts(Action::Grenade));
        assert_eq!(settings.binding(Action::Countermeasures), Some(Binding::Key(KeyCode::KeyG)));
        // Rebinding G to the grenade doesn't take it from the countermeasures.
        let mut settings = settings;
        settings.rebind(Action::Grenade, crate::settings::BindSlot::Primary, Binding::Key(KeyCode::KeyG));
        assert_eq!(settings.binding(Action::Countermeasures), Some(Binding::Key(KeyCode::KeyG)));
    }
}
