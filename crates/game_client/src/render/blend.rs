//! Crossfades between animation clips on one [`AnimationPlayer`].
//!
//! A layer is told every frame which clips it should play and how strongly. It moves each
//! clip's weight toward that target over the current fade time, starts clips as they fade
//! in and stops them once they have faded out. Bevy normalizes the weights of all clips
//! animating a bone, so targets are relative, and layers on disjoint bones (legs and upper
//! body) blend independently.

use bevy::{animation::RepeatAnimation, prelude::*};

/// A clip node in an animation graph.
#[derive(Clone, Copy, Debug)]
pub struct Clip {
    pub node: AnimationNodeIndex,
    pub duration: f32,
}

impl Clip {
    /// How far through the clip the player is, 0..1, if it is playing.
    pub fn phase(&self, player: &AnimationPlayer) -> Option<f32> {
        let active = player.animation(self.node)?;
        Some(if self.duration > 0.0 {
            (active.seek_time() / self.duration).clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    /// Whether a one-shot has played to its end (or isn't playing).
    pub fn finished(&self, player: &AnimationPlayer) -> bool {
        player.animation(self.node).is_none_or(|a| a.is_finished())
    }
}

/// How a clip plays.
#[derive(Clone, Copy, Debug)]
pub struct Play {
    pub weight: f32,
    pub speed: f32,
    pub repeat: bool,
    /// Where a clip that isn't playing yet starts, as a fraction of its length.
    pub phase: f32,
    /// Start from `phase` even if the clip is playing already (a one-shot played again).
    pub restart: bool,
    /// Held at this time (seconds) instead of playing on: clips timed by the server's state
    /// (the step phase, the server clock), the same for everyone (`game_shared::skeleton`).
    pub at: Option<f32>,
}

impl Play {
    pub fn looping(weight: f32) -> Self {
        Self {
            weight,
            speed: 1.0,
            repeat: true,
            phase: 0.0,
            restart: false,
            at: None,
        }
    }

    pub fn once(restart: bool) -> Self {
        Self {
            repeat: false,
            restart,
            ..Self::looping(1.0)
        }
    }

    pub fn speed(self, speed: f32) -> Self {
        Self { speed, ..self }
    }

    pub fn phase(self, phase: f32) -> Self {
        Self { phase, ..self }
    }

    pub fn at(self, time: f32) -> Self {
        Self {
            at: Some(time),
            speed: 0.0,
            ..self
        }
    }
}

/// Clips crossfading on one set of bones.
#[derive(Default)]
pub struct BlendLayer {
    tracks: Vec<Track>,
    /// Seconds a full crossfade takes.
    fade: f32,
}

struct Track {
    clip: Clip,
    weight: f32,
    target: f32,
}

impl BlendLayer {
    /// Crossfades from now on (including the one in progress) take `seconds`.
    pub fn set_fade(&mut self, seconds: f32) {
        self.fade = seconds;
    }

    /// Starts a frame of targets: clips that aren't played again this frame fade out.
    pub fn begin(&mut self) {
        for track in &mut self.tracks {
            track.target = 0.0;
        }
    }

    /// Fades `clip` toward `play.weight` (added up if played several times a frame),
    /// starting it if needed.
    pub fn play(&mut self, player: &mut AnimationPlayer, clip: Clip, play: Play) {
        let index = match self.tracks.iter().position(|t| t.clip.node == clip.node) {
            Some(index) => index,
            None => {
                self.tracks.push(Track {
                    clip,
                    weight: 0.0,
                    target: 0.0,
                });
                self.tracks.len() - 1
            }
        };
        let active = if play.restart || !player.is_playing_animation(clip.node) {
            let active = player.start(clip.node);
            active
                .set_repeat(if play.repeat { RepeatAnimation::Forever } else { RepeatAnimation::Never })
                .set_seek_time(play.phase.clamp(0.0, 1.0) * clip.duration);
            active
        } else {
            player.play(clip.node)
        };
        if let Some(time) = play.at {
            active.set_seek_time(time);
        }
        active.set_speed(play.speed);
        self.tracks[index].target += play.weight;
    }

    /// Moves weights toward their targets, writes them to the player and stops clips that
    /// have faded out.
    pub fn update(&mut self, player: &mut AnimationPlayer, dt: f32) {
        // With nothing showing yet there is nothing to fade from.
        let silent = self.tracks.iter().all(|t| t.weight <= 0.0);
        let step = if self.fade > 0.0 && !silent { dt / self.fade } else { f32::INFINITY };
        self.tracks.retain_mut(|track| {
            track.weight += (track.target - track.weight).clamp(-step, step);
            if track.weight <= 0.0 && track.target <= 0.0 {
                player.stop(track.clip.node);
                return false;
            }
            if let Some(active) = player.animation_mut(track.clip.node) {
                active.set_weight(track.weight);
            }
            true
        });
    }
}
