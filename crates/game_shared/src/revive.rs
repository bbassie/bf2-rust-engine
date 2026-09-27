//! Man down and revive, after BF2: a soldier whose health runs out is critically wounded
//! for a while instead of dying, unless the blow took him far below zero. A medic's shock
//! paddles bring him back; otherwise he bleeds out (or gives up) and only then dies, which
//! costs his team a ticket. Also the notices for healing, resupplying, repairing and
//! reviving. The rules run on the server (`game_server::abilities`).

use bevy::{ecs::entity::MapEntities, prelude::*};
use serde::{Deserialize, Serialize};

use crate::input::{Buttons, InputFrame};

/// Seconds a critically wounded soldier can be revived (BF2 `sv.manDownTime`).
pub const MAN_DOWN_SECONDS: f32 = 15.0;
/// How far below 0 hit points a soldier can go and still be revived (every BF2 soldier's
/// `armor.wreckHitPoints`): a blow that goes further kills outright.
pub const WRECK_HIT_POINTS: f32 = 320.0;

/// A critically wounded soldier: lying where he fell, unable to move or fire, until a medic
/// revives him or he bleeds out. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Downed {
    /// Whole seconds until he bleeds out (clients count down in between).
    pub left: f32,
    /// Facing when he fell: the body keeps it while the player looks around.
    pub yaw: f32,
}

/// What a downed soldier's input amounts to: lying still where he fell, no weapon use. The
/// server applies it to his inputs and his client to the frames it predicts with, so both
/// agree. The view stays free (the camera looks around; the body doesn't turn).
pub fn downed_input(frame: InputFrame, downed: &Downed) -> InputFrame {
    InputFrame {
        seq: frame.seq,
        yaw: downed.yaw,
        pitch: 0.0,
        weapon: frame.weapon,
        buttons: Buttons::PRONE,
        ..default()
    }
}

/// Client → server: stop waiting for a medic and die now (the deploy screen opens).
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct GiveUp;

/// What a [`ReplenishNotice`] is about.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Heal,
    Resupply,
    Repair,
    Revive,
    /// Damage we did to someone a teammate then killed.
    KillAssist,
}

/// Server → one player: what they healed, resupplied, repaired or revived, or what was
/// done for them, gathered over a second or so, and the score it earned.
#[derive(Message, Serialize, Deserialize, Clone, Debug, MapEntities)]
pub struct ReplenishNotice {
    pub kind: NoticeKind,
    /// We were healed, resupplied or revived (else we did it).
    pub received: bool,
    /// The other player, if a player (a vehicle's repair has none, or its crew's).
    #[entities]
    pub other: Option<Entity>,
    /// Hit points healed or repaired, percent of full ammo given, damage dealt for an
    /// assist; 0 for a revive.
    pub amount: f32,
    /// Score it earned us.
    pub score: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downed_soldiers_lie_still() {
        let frame = InputFrame {
            seq: 7,
            movement: [127, 0],
            yaw: 1.0,
            pitch: 0.4,
            buttons: Buttons::FIRE | Buttons::JUMP | Buttons::USE,
            weapon: 2,
            seat: 1,
            ..default()
        };
        let downed = Downed { left: 10.0, yaw: -0.5 };
        let input = downed_input(frame, &downed);
        assert_eq!(input.seq, 7);
        assert_eq!(input.movement, [0, 0]);
        assert_eq!(input.yaw, -0.5);
        assert_eq!(input.buttons, Buttons::PRONE);
        assert_eq!(input.weapon, 2);
        assert_eq!(input.seat, 0);
    }
}
