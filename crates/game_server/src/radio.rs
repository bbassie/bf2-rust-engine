//! The server side of the radio (see `game_shared::radio`): checks what players say, works out
//! what they spotted, marks it for their team and passes the message on to every client.

use avian3d::prelude::*;
use bevy::{platform::collections::HashMap, prelude::*};
use bevy_replicon::prelude::*;
use game_shared::{
    physics::GameLayer,
    projectile::Smoke,
    protocol::{ControlledBy, Team},
    radio::{RadioCommand, RadioMessage, RadioRequest, SPOT_CONE, SPOT_RANGE, SPOT_SECONDS, Spotted},
    revive::Downed,
    soldier::{Soldier, SoldierMotion},
    vehicle::{Seated, Vehicle, VehicleMotion},
    weapons::Loadout,
};

use crate::{ClientPlayer, Controls, HostPlayer, sender_player};

pub struct RadioPlugin;

impl Plugin for RadioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            receive_radio
                .after(ServerSystems::Receive)
                .run_if(in_state(ClientState::Disconnected)),
        )
        .add_systems(Update, expire_spots.run_if(in_state(ClientState::Disconnected)));
    }
}

/// BF2's radio spam rules (`ServerSettings.con`): a message within `sv.radioSpamInterval`
/// seconds of the last one is a spam flag; `sv.radioMaxSpamFlagCount` flags block the
/// player's radio for `sv.radioBlockedDurationTime` seconds.
const SPAM_INTERVAL: f32 = 6.0;
const MAX_SPAM_FLAGS: u32 = 6;
const BLOCKED_SECONDS: f32 = 30.0;

/// Server-side: seconds until a [`Spotted`] mark goes.
#[derive(Component)]
struct SpotExpiry(f32);

#[derive(Default)]
struct SpamState {
    last: f32,
    flags: u32,
    blocked_until: f32,
}

impl SpamState {
    /// Whether a message may go out now (and counts it).
    fn allow(&mut self, now: f32) -> bool {
        if now < self.blocked_until {
            return false;
        }
        if now - self.last < SPAM_INTERVAL {
            self.flags += 1;
            if self.flags >= MAX_SPAM_FLAGS {
                self.flags = 0;
                self.blocked_until = now + BLOCKED_SECONDS;
                return false;
            }
        } else {
            self.flags = 0;
        }
        self.last = now;
        true
    }
}

/// Something a player could spot.
struct Candidate {
    entity: Entity,
    center: Vec3,
    command: RadioCommand,
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn receive_radio(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut requests: MessageReader<FromClient<RadioRequest>>,
    clients: Query<&ClientPlayer>,
    host: Option<Res<HostPlayer>>,
    players: Query<(&Team, Option<&Controls>)>,
    soldiers: Query<(Entity, &SoldierMotion, &ControlledBy, Option<&Loadout>, Option<&Seated>, Has<Downed>), With<Soldier>>,
    vehicles: Query<(&Vehicle, &VehicleMotion)>,
    spatial: SpatialQuery,
    smoke: Smoke,
    mut messages: MessageWriter<ToClients<RadioMessage>>,
    mut spam: Local<HashMap<Entity, SpamState>>,
) {
    let now = time.elapsed_secs();
    for request in requests.read() {
        let Some(player) = sender_player(request.client_id, &clients, host.as_deref()) else {
            continue;
        };
        let Ok((&team, Some(controls))) = players.get(player) else {
            continue;
        };
        let Ok((soldier, motion, _, _, _, false)) = soldiers.get(controls.0) else {
            continue;
        };
        if !spam.entry(player).or_default().allow(now) {
            debug!("radio from {player}: blocked for spamming");
            continue;
        }
        let mut command = request.message.command;
        let mut target = None;
        if command == RadioCommand::Spotted {
            let eye = motion.eye_position();
            let aim = motion.view_rotation() * Vec3::NEG_Z;
            let enemy_team = |controlled_by: &ControlledBy| {
                players.get(controlled_by.0).is_ok_and(|(t, _)| *t != team && *t != Team::Spectator)
            };
            // Enemies on foot, and vehicles with enemies in them.
            let mut candidates: Vec<Candidate> = Vec::new();
            for (entity, other, controlled_by, loadout, seated, _) in &soldiers {
                if entity == soldier || !enemy_team(controlled_by) {
                    continue;
                }
                match seated.and_then(|s| vehicles.get(s.vehicle).ok().map(|v| (s.vehicle, v))) {
                    Some((vehicle, (desc, vehicle_motion))) => candidates.push(Candidate {
                        entity: vehicle,
                        center: vehicle_motion.position + Vec3::Y,
                        command: RadioCommand::vehicle_spotted(&desc.template),
                    }),
                    None => {
                        let sniper = loadout.is_some_and(|l| l.kit.to_ascii_lowercase().contains("sniper"));
                        candidates.push(Candidate {
                            entity,
                            center: other.position + Vec3::Y * (other.stance.eye_height() * 0.7),
                            command: if sniper { RadioCommand::SpottedSniper } else { RadioCommand::SpottedInfantry },
                        });
                    }
                }
            }
            let visible = |to: Vec3| {
                let Ok(direction) = Dir3::new(to - eye) else { return true };
                let distance = eye.distance(to);
                let filter = SpatialQueryFilter::from_mask(GameLayer::World);
                spatial.cast_ray(eye, direction, distance, true, &filter).is_none() && !smoke.blocks(eye, to)
            };
            let best = candidates
                .into_iter()
                .filter_map(|c| {
                    let offset = c.center - eye;
                    let distance = offset.length();
                    let angle = aim.angle_between(offset).to_degrees();
                    (distance <= SPOT_RANGE && angle <= SPOT_CONE).then_some((angle, c))
                })
                .filter(|(_, c)| visible(c.center))
                .min_by(|a, b| a.0.total_cmp(&b.0));
            command = match best {
                Some((_, spotted)) => {
                    commands.entity(spotted.entity).try_insert((Spotted { by: team }, SpotExpiry(SPOT_SECONDS)));
                    target = Some(spotted.entity);
                    spotted.command
                }
                None => RadioCommand::EnemySpotted,
            };
        }
        debug!("radio from {player}: {command:?}{}", if target.is_some() { " (marked)" } else { "" });
        messages.write(ToClients {
            targets: SendTargets::All,
            message: RadioMessage {
                player,
                command,
                position: motion.position,
                target,
            },
        });
    }
    spam.retain(|player, _| players.contains(*player));
}

fn expire_spots(mut commands: Commands, time: Res<Time>, mut spots: Query<(Entity, &mut SpotExpiry)>) {
    for (entity, mut expiry) in &mut spots {
        expiry.0 -= time.delta_secs();
        if expiry.0 <= 0.0 {
            commands.entity(entity).remove::<(Spotted, SpotExpiry)>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_spammers() {
        let mut spam = SpamState::default();
        assert!(spam.allow(10.0));
        // Five quick ones pass, the sixth flag blocks for 30 s.
        for i in 1..=5 {
            assert!(spam.allow(10.0 + i as f32), "message {i}");
        }
        assert!(!spam.allow(16.0));
        assert!(!spam.allow(40.0));
        assert!(spam.allow(47.0));
    }
}
