//! Predicting the vehicle we drive, connected to a remote server.
//!
//! Like our soldier (see `prediction`): every tick the client runs the server's
//! [`step_vehicle`] for the vehicle with our latest input and moves its body with
//! [`integrate`], so throttle, steering and the stick take effect at once instead of one round
//! trip plus the interpolation delay later. When the server's state arrives the vehicle is
//! rewound to it and the inputs the server hasn't applied yet are replayed; any difference is
//! blended out on screen. Collisions of the hull aren't simulated here (only the wheels'
//! raycasts), so crashes are corrected by the server.

use std::collections::VecDeque;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    flight::BodyState,
    input::InputFrame,
    level::LoadedLevel,
    vehicle::{Seated, VehicleData, VehicleMotion, VehicleSim, VehicleState, integrate, step_vehicle, water_height},
};

use crate::{
    local_input::{InputHistory, LocalInputSystems},
    net::LocalSoldier,
};

/// How quickly corrections are blended out (per second).
const ERROR_DECAY: f32 = 10.0;
/// Corrections larger than this (meters) snap instead of blending.
const SNAP_DISTANCE: f32 = 6.0;

pub struct VehiclePredictionPlugin;

impl Plugin for VehiclePredictionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VehiclePredictionStats>()
            .add_systems(
                PreUpdate,
                reconcile
                    .after(ClientSystems::Receive)
                    .run_if(in_state(ClientState::Connected)),
            )
            .add_systems(
                FixedUpdate,
                predict
                    .after(LocalInputSystems)
                    .run_if(in_state(ClientState::Connected))
                    .run_if(|cli: Res<crate::Cli>| !cli.no_vehicle_prediction),
            );
    }
}

/// Diagnostics for the HUD and scenarios.
#[derive(Resource, Default, Debug)]
pub struct VehiclePredictionStats {
    /// Inputs replayed at the last reconciliation.
    pub replayed: usize,
    /// Size of the last correction, meters.
    pub last_correction: f32,
}

/// The vehicle we drive, predicted.
#[derive(Component)]
pub struct PredictedVehicle {
    pub previous: BodyState,
    pub current: BodyState,
    pub state: VehicleState,
    sim: VehicleSim,
    /// The engines' and springs' state after each input (not replicated; replays start from
    /// the one after the input the server acknowledged).
    sims: VecDeque<(u32, VehicleSim)>,
    /// Visual offsets left over from corrections, decaying to nothing.
    pub error: Vec3,
    pub rotation_error: Quat,
}

impl PredictedVehicle {
    /// Where to draw the vehicle between the last two ticks.
    pub fn transform(&mut self, alpha: f32, dt: f32) -> Transform {
        let decay = (-ERROR_DECAY * dt).exp();
        self.error *= decay;
        self.rotation_error = Quat::IDENTITY.slerp(self.rotation_error, decay);
        let position = self.previous.position.lerp(self.current.position, alpha) + self.error;
        let rotation = self.rotation_error * self.previous.rotation.slerp(self.current.rotation, alpha);
        Transform::from_translation(position).with_rotation(rotation)
    }

    pub fn velocity(&self) -> Vec3 {
        self.current.velocity
    }
}

/// The seats' inputs as the server sees them from us: ours in the driver's seat.
fn driver_inputs(seats: usize, input: &InputFrame) -> Vec<Option<InputFrame>> {
    let mut inputs = vec![None; seats];
    if let Some(first) = inputs.first_mut() {
        *first = Some(*input);
    }
    inputs
}

#[allow(clippy::type_complexity)]
fn predict(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    level: Option<Res<LoadedLevel>>,
    history: Res<InputHistory>,
    driver: Query<&Seated, With<LocalSoldier>>,
    mut vehicles: Query<(Entity, &VehicleData, &VehicleMotion, &VehicleState, Option<&mut PredictedVehicle>)>,
) {
    let driving = driver.single().ok().filter(|s| s.seat == 0).map(|s| s.vehicle);
    let water = water_height(level.as_deref());
    let dt = time.delta_secs();
    for (entity, data, motion, state, predicted) in &mut vehicles {
        if driving != Some(entity) {
            if predicted.is_some() {
                commands.entity(entity).remove::<PredictedVehicle>();
            }
            continue;
        }
        let Some(mut predicted) = predicted else {
            commands.entity(entity).insert(PredictedVehicle {
                previous: motion.body(),
                current: motion.body(),
                state: state.clone(),
                sim: VehicleSim::new(&data.0.desc),
                sims: VecDeque::new(),
                error: Vec3::ZERO,
                rotation_error: Quat::IDENTITY,
            });
            continue;
        };
        let Some(input) = history.latest() else {
            continue;
        };
        let model = &data.0;
        let inputs = driver_inputs(model.desc.seats.len(), input);
        let PredictedVehicle {
            previous,
            current,
            state,
            sim,
            sims,
            ..
        } = &mut *predicted;
        *previous = *current;
        let push = step_vehicle(model, current, &inputs, state, sim, &spatial, entity, water, dt);
        integrate(model, current, &push, dt);
        sims.push_back((input.seq, sim.clone()));
        while sims.len() > 128 {
            sims.pop_front();
        }
    }
}

#[allow(clippy::type_complexity)]
fn reconcile(
    fixed: Res<Time<Fixed>>,
    spatial: SpatialQuery,
    level: Option<Res<LoadedLevel>>,
    history: Res<InputHistory>,
    mut stats: ResMut<VehiclePredictionStats>,
    mut vehicles: Query<(Entity, &VehicleData, Ref<VehicleMotion>, &VehicleState, &mut PredictedVehicle)>,
) {
    let water = water_height(level.as_deref());
    let dt = fixed.timestep().as_secs_f32();
    for (entity, data, motion, state, mut predicted) in &mut vehicles {
        // The state carries the input it followed from (the soldier's ack may arrive in
        // another update than the vehicle's state).
        let ack = motion.ack;
        if !motion.is_changed() || ack == 0 {
            continue;
        }
        let model = &data.0;
        let mut body = motion.body();
        let mut replayed_state = state.clone();
        // The engines' and springs' own state isn't replicated: ours from back then.
        let mut sim = predicted
            .sims
            .iter()
            .find(|(seq, _)| *seq == ack)
            .map_or_else(|| predicted.sim.clone(), |(_, sim)| sim.clone());
        let mut count = 0;
        for frame in history.frames.iter().filter(|f| f.seq > ack) {
            let inputs = driver_inputs(model.desc.seats.len(), frame);
            let push = step_vehicle(model, &body, &inputs, &mut replayed_state, &mut sim, &spatial, entity, water, dt);
            integrate(model, &mut body, &push, dt);
            count += 1;
        }
        let correction = body.position - predicted.current.position;
        stats.replayed = count;
        stats.last_correction = correction.length();
        let turn = body.rotation * predicted.current.rotation.inverse();
        if correction.length() < SNAP_DISTANCE {
            predicted.error -= correction;
            predicted.rotation_error = predicted.rotation_error * turn.inverse();
        }
        predicted.previous.position += correction;
        predicted.previous.rotation = turn * predicted.previous.rotation;
        predicted.current = body;
        predicted.state = replayed_state;
        predicted.sim = sim;
    }
}
