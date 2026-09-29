//! Target dummies for hit registration tests: bots that stop thinking and stand, crouch,
//! lie, strafe, aim, fire or reload in front of the nearest human, and can't be killed.
//!
//! A dedicated server turns every bot into one with `BF2_DUMMY_BOTS=<poses>` (comma separated,
//! each held for `BF2_DUMMY_SECONDS`, default 6, then the next, round and round); a scenario
//! on a listen server sets [`DummyControl::pose`] (the `Dummy` step). A dummy is placed
//! [`DummyControl::distance`] meters in front of the nearest human and placed again when he
//! moves away. Poses: `stand`, `crouch`, `prone`, `strafe`, `crouchstrafe`, `pronestrafe`,
//! `walk`, `aim`, `fire`, `reload`, `turn`; `@<degrees>` turns it that far to its left from
//! facing the human (`stand@90` shows its right side).

use bevy::prelude::*;
use game_data::FireKind;
use game_shared::{
    input::{Buttons, InputFrame},
    protocol::Player,
    revive::Downed,
    soldier::{Health, Soldier, SoldierMotion},
    vehicle::Seated,
    weapons::{Armory, Inventory, Loadout},
};

use crate::{Controls, InputBuffer, ServerSimSystems};

pub struct DummyPlugin;

impl Plugin for DummyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DummyControl::from_env()).add_systems(
            FixedUpdate,
            puppet
                .after(ServerSimSystems::Think)
                .before(ServerSimSystems::ApplyInputs)
                .run_if(|control: Res<DummyControl>| control.active())
                .run_if(in_state(bevy_replicon::prelude::ClientState::Disconnected)),
        );
    }
}

/// What target dummies do.
#[derive(Resource, Debug, Clone)]
pub struct DummyControl {
    /// Set by a scenario: every bot holds this pose.
    pub pose: Option<String>,
    /// From `BF2_DUMMY_BOTS`: poses taken in turn, each for `seconds`.
    pub cycle: Vec<String>,
    pub seconds: f32,
    /// Meters in front of the nearest human.
    pub distance: f32,
}

impl DummyControl {
    fn from_env() -> Self {
        let cycle = std::env::var("BF2_DUMMY_BOTS")
            .map(|v| v.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
            .unwrap_or_default();
        let number = |name: &str, default: f32| {
            std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        Self {
            pose: None,
            cycle,
            seconds: number("BF2_DUMMY_SECONDS", 6.0),
            distance: number("BF2_DUMMY_DISTANCE", 8.0),
        }
    }

    fn active(&self) -> bool {
        self.pose.is_some() || !self.cycle.is_empty()
    }

    /// The pose at `time` seconds.
    fn pose_at(&self, time: f32) -> Option<&str> {
        if let Some(pose) = &self.pose {
            return Some(pose);
        }
        let index = (time / self.seconds.max(0.1)) as usize % self.cycle.len().max(1);
        self.cycle.get(index).map(String::as_str)
    }
}

/// A bot being a target dummy.
#[derive(Component)]
struct Dummy {
    /// Where it was put, and where the human stood then.
    anchor: Vec3,
    human_at: Vec3,
    pose: String,
    /// Seconds in `pose`.
    time: f32,
    /// Strafing to its right.
    right: bool,
    seq: u32,
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn puppet(
    mut commands: Commands,
    time: Res<Time>,
    elapsed: Res<Time<Real>>,
    control: Res<DummyControl>,
    armory: Res<Armory>,
    players: Query<(&Player, Option<&Controls>)>,
    mut bots: Query<(Entity, &Player, &Controls, &mut InputBuffer, Option<&mut Dummy>)>,
    mut soldiers: Query<(&mut SoldierMotion, &mut Health, &mut Inventory, &Loadout), (With<Soldier>, Without<Seated>, Without<Downed>)>,
) {
    let dt = time.delta_secs();
    let Some(pose) = control.pose_at(elapsed.elapsed_secs()).map(str::to_string) else {
        return;
    };
    // Soldiers of players that aren't bots: where they stand and face.
    let humans: Vec<(Vec3, f32)> = players
        .iter()
        .filter(|(p, _)| !p.is_bot)
        .filter_map(|(_, c)| soldiers.get(c?.0).ok())
        .map(|(m, ..)| (m.position, m.yaw))
        .collect();
    for (bot, player, controls, mut buffer, dummy) in &mut bots {
        if !player.is_bot {
            continue;
        }
        let Ok((mut body, mut health, mut inventory, loadout)) = soldiers.get_mut(controls.0) else {
            continue;
        };
        let motion = *body;
        // Never dies.
        health.current = 10_000.0;
        let Some(&(human, human_yaw)) = humans
            .iter()
            .min_by(|a, b| a.0.distance(motion.position).total_cmp(&b.0.distance(motion.position)))
        else {
            continue;
        };
        let mut dummy = match dummy {
            Some(dummy) => dummy,
            None => {
                commands.entity(bot).insert(Dummy {
                    anchor: Vec3::NAN,
                    human_at: Vec3::NAN,
                    pose: String::new(),
                    time: 0.0,
                    right: true,
                    seq: 0,
                });
                continue;
            }
        };
        // In front of the human, again whenever he moved on.
        if !(dummy.human_at.distance(human) < 3.0) {
            let forward = Quat::from_rotation_y(human_yaw) * Vec3::NEG_Z;
            let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
            let spot = human + flat * control.distance + Vec3::Y * 0.3;
            body.position = spot;
            body.velocity = Vec3::ZERO;
            dummy.anchor = spot;
            dummy.human_at = human;
            info!("dummy: {} placed at {spot:.1}, {:.1} m from {human:.1}", player.name, control.distance);
        }
        if dummy.pose != pose {
            info!("dummy: {} {pose}", player.name);
            dummy.pose = pose.clone();
            dummy.time = 0.0;
        } else {
            dummy.time += dt;
        }
        let (name, degrees) = match pose.split_once('@') {
            Some((name, degrees)) => (name, degrees.parse::<f32>().unwrap_or(0.0)),
            None => (pose.as_str(), 0.0),
        };
        let to = human - motion.position;
        let facing = (-to.x).atan2(-to.z) + degrees.to_radians();
        let mut frame = InputFrame {
            seq: 0,
            yaw: facing,
            pitch: 0.0,
            weapon: inventory.active,
            ..default()
        };
        // The primary weapon in hand.
        let primary = loadout
            .weapons
            .iter()
            .position(|w| armory.weapon(w).is_some_and(|w| w.slot == 3 && w.fire.kind == FireKind::Gun));
        if let Some(primary) = primary {
            frame.weapon = primary as u8;
        }
        let stance = match name {
            n if n.starts_with("crouch") => Buttons::CROUCH,
            n if n.starts_with("prone") => Buttons::PRONE,
            _ => Buttons::empty(),
        };
        frame.buttons |= stance;
        match name {
            "strafe" | "crouchstrafe" | "pronestrafe" => {
                // Back and forth across the spot it was put on.
                let right = Quat::from_rotation_y(facing) * Vec3::X;
                let offset = (motion.position - dummy.anchor).dot(right);
                if offset > 1.5 {
                    dummy.right = false;
                } else if offset < -1.5 {
                    dummy.right = true;
                }
                frame.movement = [if dummy.right { 127 } else { -127 }, 0];
            }
            "walk" => {
                let forward = Quat::from_rotation_y(facing) * Vec3::NEG_Z;
                let offset = (motion.position - dummy.anchor).dot(forward);
                if offset > 1.5 {
                    dummy.right = false;
                } else if offset < -1.5 {
                    dummy.right = true;
                }
                frame.movement = [0, if dummy.right { 127 } else { -127 }];
            }
            "aim" => frame.buttons |= Buttons::AIM,
            "fire" => {
                // Single shots into the ground at its feet, a few a second.
                frame.pitch = -1.4;
                if (dummy.time * 60.0) as u32 % 20 == 0 {
                    frame.buttons |= Buttons::FIRE;
                }
            }
            "reload" => {
                if !inventory.reloading {
                    let active = inventory.active as usize;
                    if let Some(ammo) = inventory.ammo.get_mut(active) {
                        ammo[0] = ammo[0].min(1);
                        ammo[1] = ammo[1].max(60);
                    }
                    frame.buttons |= Buttons::RELOAD;
                }
            }
            "turn" => frame.yaw = facing + (dummy.time * 1.5).sin() * 1.2,
            _ => {}
        }
        dummy.seq = dummy.seq.wrapping_add(1);
        frame.seq = buffer.last_received.unwrap_or(0).wrapping_add(dummy.seq);
        buffer.queue.clear();
        buffer.current = frame;
    }
}
