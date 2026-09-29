//! Player input as sent over the network. Bots produce the same frames, so every
//! soldier (human or AI) is driven by exactly the same simulation code.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    #[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Buttons: u16 {
        const JUMP   = 1 << 0;
        const SPRINT = 1 << 1;
        const CROUCH = 1 << 2;
        const PRONE  = 1 << 3;
        const FIRE   = 1 << 4;
        const AIM    = 1 << 5;
        const USE    = 1 << 6;
        const RELOAD = 1 << 7;
        /// Cycle the weapon's fire mode (single/burst/auto).
        const FIRE_MODE = 1 << 8;
        /// In a vehicle: decoy flares or smoke grenades.
        const COUNTERMEASURE = 1 << 9;
        /// The melee or grenade key is at work: the knife or grenade selected with it comes
        /// out quickly, and the weapon it interrupted comes back quickly (see
        /// `weapons::WeaponState::switch_with`).
        const QUICK = 1 << 10;
    }
}

/// Input for one simulation tick.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct InputFrame {
    /// Increments by one per tick. Used for acknowledgement and reconciliation.
    pub seq: u32,
    /// Movement intent, quantized: x = right, y = forward, each in `-127..=127`.
    pub movement: [i8; 2],
    /// View yaw in radians (0 = facing -Z, positive turns left).
    pub yaw: f32,
    /// View pitch in radians (positive looks up).
    pub pitch: f32,
    pub buttons: Buttons,
    /// Selected weapon: index into the soldier's loadout.
    pub weapon: u8,
    /// Server tick of the world the player saw when he pressed this (other soldiers are shown
    /// in the past), so hits are judged against what he aimed at. 0 = the present.
    pub view_tick: u32,
    /// In a vehicle: the seat to move to, counting from 1 (0 = stay).
    pub seat: u8,
    /// Flying: the stick, quantized like `movement`: x = roll right, y = pitch up (pulled
    /// back). Throttle and rudder are `movement`.
    pub stick: [i8; 2],
}

impl InputFrame {
    pub fn movement_vec(&self) -> Vec2 {
        Vec2::new(self.movement[0] as f32, self.movement[1] as f32) / 127.0
    }

    pub fn stick_vec(&self) -> Vec2 {
        Vec2::new(self.stick[0] as f32, self.stick[1] as f32) / 127.0
    }

    pub fn set_stick(&mut self, v: Vec2) {
        let v = v.clamp(Vec2::NEG_ONE, Vec2::ONE) * 127.0;
        self.stick = [v.x.round() as i8, v.y.round() as i8];
    }

    pub fn set_movement(&mut self, v: Vec2) {
        let v = v.clamp_length_max(1.0) * 127.0;
        self.movement = [v.x.round() as i8, v.y.round() as i8];
    }

    pub fn pressed(&self, b: Buttons) -> bool {
        self.buttons.contains(b)
    }
}

/// Client → server. Carries the newest frames, with a few older ones repeated so a lost
/// packet rarely loses input.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct InputPacket {
    /// Oldest first.
    pub frames: Vec<InputFrame>,
}

/// How many recent frames each [`InputPacket`] repeats.
pub const INPUT_REDUNDANCY: usize = 4;
