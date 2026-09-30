//! Smooth soldier movement on screen.
//!
//! * **Our soldier, connected to a remote server**: predicted locally with the same
//!   [`step_soldier`] the server uses. When the server's state arrives we rewind to it and
//!   replay the inputs it hasn't processed yet; any correction is blended out visually.
//! * **Other soldiers, connected to a remote server**: buffered and shown slightly in the
//!   past, interpolating between received states.
//! * **Hosting (listen server / singleplayer)**: the simulation is local, so we just
//!   interpolate between the last two fixed ticks.
//!
//! The result of all three is written to [`SoldierRender`], which visuals and the camera use.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::level::LoadedLevel;
use game_shared::vehicle::{Seated, water_height};
use game_shared::soldier::{
    InputAck, Soldier, SoldierMotion, SoldierShapes, SoldierTuning, Stance, step_soldier,
};

use crate::{
    local_input::{InputHistory, LocalInputSystems},
    net::LocalSoldier,
};

/// How far in the past remote soldiers are shown, to always have two states to blend.
pub(crate) const INTERPOLATION_DELAY: f64 = 0.1;
/// How quickly prediction corrections are blended out (per second).
const ERROR_DECAY: f32 = 12.0;

pub struct PredictionPlugin;

impl Plugin for PredictionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PredictionStats>()
            .add_observer(add_render_state)
            .add_systems(
                PreUpdate,
                (reconcile, record_snapshots)
                    .after(ClientSystems::Receive)
                    .run_if(in_state(ClientState::Connected)),
            )
            .add_systems(
                FixedUpdate,
                predict
                    .after(LocalInputSystems)
                    .run_if(in_state(ClientState::Connected)),
            )
            .add_systems(
                FixedPostUpdate,
                record_ticks.run_if(not(in_state(ClientState::Connected))),
            )
            .add_systems(
                PostUpdate,
                update_render_state
                    .in_set(RenderStateSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Computes [`SoldierRender`] in `PostUpdate`. Anything drawing soldiers runs after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderStateSystems;

/// Diagnostics shown on the HUD.
#[derive(Resource, Default, Debug)]
pub struct PredictionStats {
    /// Inputs replayed at the last reconciliation (roughly the round trip in ticks).
    pub replayed: usize,
    /// Size of the last correction in meters.
    pub last_correction: f32,
}

/// Where a soldier should be drawn this frame.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct SoldierRender {
    pub position: Vec3,
    pub yaw: f32,
    pub stance: Stance,
    pub velocity: Vec3,
    pub grounded: bool,
    /// On a ladder, on a grappling rope if `on_rope`, or climbing onto a ledge.
    pub climbing: bool,
    pub on_rope: bool,
    /// Hanging from a zipline.
    pub riding: bool,
    /// Under an open parachute.
    pub parachute: bool,
    /// Swimming at the surface (too deep to stand; see `SoldierTuning::swim_depth`).
    pub swimming: bool,
    /// Eye height above the feet, easing toward the stance's so the view moves with the
    /// body when crouching or going prone instead of jumping.
    pub eye_height: f32,
    /// Vertical offset of the eye easing out steps up and down (stairs, curbs) so the view
    /// doesn't jerk. Part of [`Self::eye_position`].
    pub step_offset: f32,
    /// The legs' step phase ([`SoldierMotion::stride`]).
    pub stride: f32,
}

impl SoldierRender {
    pub fn eye_position(&self) -> Vec3 {
        self.position + Vec3::Y * (self.eye_height + self.step_offset)
    }
}

/// How quickly the eye height follows a stance change (per second; about 0.25 s).
const EYE_HEIGHT_RATE: f32 = 12.0;
/// Most the eye lags behind the feet when they step up or snap down (stairs, curbs).
const STEP_SMOOTHING: f32 = 0.25;

/// Predicted state of our own soldier.
#[derive(Component)]
pub struct Predicted {
    previous: SoldierMotion,
    current: SoldierMotion,
    /// Visual offset left over from corrections, decays to zero.
    error: Vec3,
}

impl Predicted {
    /// Our soldier as of the latest input, ahead of the server's replicated state (for
    /// things that must agree with our own input, like being allowed to fire).
    pub fn motion(&self) -> &SoldierMotion {
        &self.current
    }
}

/// Received states of a remote soldier, by local receive time.
#[derive(Component, Default)]
struct Snapshots(VecDeque<(f64, SoldierMotion)>);

/// The last two simulated ticks, when hosting.
#[derive(Component, Default)]
struct TickHistory {
    previous: SoldierMotion,
    current: SoldierMotion,
}

fn add_render_state(add: On<Add, Soldier>, mut commands: Commands, motions: Query<&SoldierMotion>) {
    let motion = motions.get(add.entity).copied().unwrap_or_default();
    commands.entity(add.entity).insert((
        SoldierRender {
            position: motion.position,
            yaw: motion.yaw,
            stance: motion.stance,
            velocity: motion.velocity,
            grounded: motion.grounded,
            climbing: motion.climbing || motion.mantling(),
            on_rope: motion.on_rope,
            riding: motion.riding,
            parachute: motion.parachute,
            swimming: motion.swimming,
            eye_height: motion.stance.eye_height(),
            step_offset: 0.0,
            stride: motion.stride,
        },
        Snapshots::default(),
        TickHistory {
            previous: motion,
            current: motion,
        },
    ));
}

fn predict(
    mut commands: Commands,
    time: Res<Time>,
    tuning: Res<SoldierTuning>,
    shapes: Res<SoldierShapes>,
    mover: MoveAndSlide,
    history: Res<InputHistory>,
    level: Option<Res<LoadedLevel>>,
    mut soldiers: Query<
        (Entity, &SoldierMotion, Option<&mut Predicted>, Has<Seated>, Has<game_shared::revive::Downed>),
        With<LocalSoldier>,
    >,
) {
    let Some(input) = history.latest() else {
        return;
    };
    let water = water_height(level.as_deref());
    for (entity, motion, predicted, seated, downed) in &mut soldiers {
        // Riding in a vehicle: the server moves us (vehicles aren't predicted yet).
        if seated {
            if predicted.is_some() {
                commands.entity(entity).remove::<Predicted>();
            }
            continue;
        }
        let Some(mut predicted) = predicted else {
            commands.entity(entity).insert(Predicted {
                previous: *motion,
                current: *motion,
                error: Vec3::ZERO,
            });
            continue;
        };
        predicted.previous = predicted.current;
        // Critically wounded: the parachute is let go, as the server does (`apply_inputs`).
        if downed {
            predicted.current.parachute = false;
        }
        step_soldier(
            &mut predicted.current,
            input,
            time.delta_secs(),
            &tuning,
            &shapes,
            &mover,
            water,
        );
    }
}

fn reconcile(
    fixed: Res<Time<Fixed>>,
    tuning: Res<SoldierTuning>,
    shapes: Res<SoldierShapes>,
    mover: MoveAndSlide,
    history: Res<InputHistory>,
    level: Option<Res<LoadedLevel>>,
    mut stats: ResMut<PredictionStats>,
    mut soldiers: Query<(Ref<SoldierMotion>, Ref<InputAck>, &mut Predicted), (With<LocalSoldier>, Without<Seated>)>,
) {
    let dt = fixed.timestep().as_secs_f32();
    let water = water_height(level.as_deref());
    for (motion, ack, mut predicted) in &mut soldiers {
        if !motion.is_changed() && !ack.is_changed() {
            continue;
        }
        let mut replayed = *motion;
        let mut count = 0;
        for frame in history.frames.iter().filter(|f| f.seq > ack.0) {
            step_soldier(&mut replayed, frame, dt, &tuning, &shapes, &mover, water);
            count += 1;
        }
        let correction = replayed.position - predicted.current.position;
        stats.replayed = count;
        stats.last_correction = correction.length();
        if correction.length_squared() > 1e-8 {
            // Teleports (respawn) snap; small corrections blend.
            if correction.length() < 4.0 {
                predicted.error -= correction;
            }
            predicted.previous.position += correction;
        }
        predicted.current = replayed;
    }
}

fn record_snapshots(
    time: Res<Time<Real>>,
    mut soldiers: Query<(Ref<SoldierMotion>, &mut Snapshots), Without<LocalSoldier>>,
) {
    let now = time.elapsed_secs_f64();
    for (motion, mut snapshots) in &mut soldiers {
        if motion.is_changed() {
            snapshots.0.push_back((now, *motion));
        }
        while snapshots.0.len() > 2 && snapshots.0[1].0 < now - 1.0 {
            snapshots.0.pop_front();
        }
    }
}

fn record_ticks(mut soldiers: Query<(&SoldierMotion, &mut TickHistory)>) {
    for (motion, mut ticks) in &mut soldiers {
        ticks.previous = ticks.current;
        ticks.current = *motion;
    }
}

fn update_render_state(
    time: Res<Time>,
    real: Res<Time<Real>>,
    fixed: Res<Time<Fixed>>,
    state: Res<State<ClientState>>,
    mut soldiers: Query<(
        &SoldierMotion,
        &mut SoldierRender,
        Option<&mut Predicted>,
        &Snapshots,
        &TickHistory,
    )>,
) {
    let alpha = fixed.overstep_fraction();
    let connected = *state.get() == ClientState::Connected;
    let render_time = real.elapsed_secs_f64() - INTERPOLATION_DELAY;

    for (motion, mut render, predicted, snapshots, ticks) in &mut soldiers {
        let (from, to, t) = if let Some(mut predicted) = predicted {
            predicted.error *= (-ERROR_DECAY * time.delta_secs()).exp();
            let (from, mut to) = (predicted.previous, predicted.current);
            to.position += predicted.error;
            let mut from = from;
            from.position += predicted.error;
            (from, to, alpha)
        } else if connected {
            interpolate_snapshots(&snapshots.0, render_time).unwrap_or((*motion, *motion, 1.0))
        } else {
            (ticks.previous, ticks.current, alpha)
        };
        let position = from.position.lerp(to.position, t);
        let dt = time.delta_secs();
        let eye_target = to.stance.eye_height();
        let eye_blend = 1.0 - (-EYE_HEIGHT_RATE * dt).exp();
        // Stepping up stairs and ledges or snapping down them moves the feet by up to half
        // a meter in a tick. The eye stays where it was and catches up like it does after a
        // stance change; walking up and down slopes (the vertical velocity) isn't smoothed.
        let mut step = render.step_offset;
        let rise = position.y - render.position.y - to.velocity.y * dt;
        if to.grounded && render.grounded && rise.abs() < 1.0 {
            step -= rise;
        }
        *render = SoldierRender {
            position,
            yaw: lerp_angle(from.yaw, to.yaw, t),
            stance: to.stance,
            velocity: to.velocity,
            grounded: to.grounded,
            // Climbing onto a ledge looks like climbing.
            climbing: to.climbing || to.mantling(),
            on_rope: to.on_rope,
            riding: to.riding,
            parachute: to.parachute,
            swimming: to.swimming,
            eye_height: render.eye_height + (eye_target - render.eye_height) * eye_blend,
            step_offset: (step * (1.0 - eye_blend)).clamp(-STEP_SMOOTHING, STEP_SMOOTHING),
            stride: lerp_phase(from.stride, to.stride, t),
        };
    }
}

fn interpolate_snapshots(
    snapshots: &VecDeque<(f64, SoldierMotion)>,
    at: f64,
) -> Option<(SoldierMotion, SoldierMotion, f32)> {
    let last = snapshots.back()?;
    if at >= last.0 {
        return Some((last.1, last.1, 1.0));
    }
    let i = snapshots.iter().rposition(|(t, _)| *t <= at)?;
    let (t0, a) = snapshots[i];
    let (t1, b) = snapshots[i + 1];
    let t = ((at - t0) / (t1 - t0).max(1e-6)) as f32;
    Some((a, b, t.clamp(0.0, 1.0)))
}

/// Between two step phases (0..1), the short way round.
fn lerp_phase(a: f32, b: f32, t: f32) -> f32 {
    let d = (b - a + 0.5).rem_euclid(1.0) - 0.5;
    (a + d * t).rem_euclid(1.0)
}

fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut d = (b - a) % TAU;
    if d > PI {
        d -= TAU;
    } else if d < -PI {
        d += TAU;
    }
    a + d * t
}
