//! Flight model checks on imported aircraft: flies them in the air (no ground, no Bevy app)
//! with the same forces the server uses, integrating the rigid body like avian does, and
//! prints what they do. Skipped when `imported/vehicles/` lacks the aircraft.
//!
//! `cargo test -p game_shared --test flight -- --nocapture`

use std::path::Path;

use bevy::prelude::*;
use game_data::VehicleCategory;
use game_shared::{
    flight::{BodyState, Controls, FlightState, Surroundings, flight_forces, limit_pitch},
    vehicle::{VehicleModel, integrate, step_joints},
};

const DT: f32 = 1.0 / 60.0;

fn load(name: &str) -> Option<VehicleModel> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported");
    let desc = game_data::read_ron(root.join("vehicles").join(format!("{name}.ron"))).ok()?;
    Some(VehicleModel::new(desc, &root))
}

struct Sim<'a> {
    model: &'a VehicleModel,
    body: BodyState,
    joints: Vec<[f32; 3]>,
    flight: FlightState,
    time: f32,
}

impl<'a> Sim<'a> {
    fn new(model: &'a VehicleModel, altitude: f32, speed: f32) -> Self {
        Self {
            model,
            body: BodyState {
                position: Vec3::Y * altitude,
                velocity: Vec3::NEG_Z * speed,
                ..default()
            },
            joints: vec![[0.0; 3]; model.joint_count],
            flight: FlightState {
                throttle: 0.5,
                spin: 1.0,
                gear_up: true,
                ..FlightState::new()
            },
            time: 0.0,
        }
    }

    fn step(&mut self, controls: &Controls) {
        let desc = &self.model.desc;
        let mut controls = *controls;
        if desc.category == VehicleCategory::Air
            && let Some(aero) = &desc.aero
        {
            controls.pitch = limit_pitch(&self.body, aero.stall_angle, controls.pitch);
        }
        let controls = &controls;
        let (joints, _) = step_joints(self.model, &self.body, &[], &self.joints, controls, &self.flight, DT);
        self.joints = joints;
        let around = Surroundings {
            water: None,
            altitude: self.body.position.y,
        };
        let push = flight_forces(self.model, &self.body, &self.joints, controls, &mut self.flight, &around, DT);
        integrate(self.model, &mut self.body, &push, DT);
        self.time += DT;
    }

    fn run(&mut self, seconds: f32, mut controls: impl FnMut(&Sim) -> Controls) {
        let steps = (seconds / DT).round() as usize;
        for _ in 0..steps {
            let c = controls(self);
            self.step(&c);
        }
    }

    fn speed(&self) -> f32 {
        self.body.velocity.length()
    }

    /// Nose up, degrees.
    fn pitch(&self) -> f32 {
        (self.body.rotation * Vec3::NEG_Z).y.clamp(-1.0, 1.0).asin().to_degrees()
    }

    /// Right wing down, degrees.
    fn roll(&self) -> f32 {
        (-(self.body.rotation * Vec3::X).y).clamp(-1.0, 1.0).asin().to_degrees()
    }

    /// Angle of attack, degrees.
    fn aoa(&self) -> f32 {
        let local = self.body.rotation.inverse() * self.body.velocity;
        (-local.y).atan2(-local.z).to_degrees()
    }

    fn local_rates(&self) -> Vec3 {
        (self.body.rotation.inverse() * self.body.angular_velocity) * (180.0 / std::f32::consts::PI)
    }

    fn line(&self) -> String {
        let r = self.local_rates();
        format!(
            "t {:5.2} speed {:6.1} m/s alt {:7.1} climb {:6.1} pitch {:6.1} roll {:6.1} aoa {:5.1} rates p/y/r {:6.1} {:6.1} {:6.1} thr {:.2} boost {:.2}",
            self.time,
            self.speed(),
            self.body.position.y,
            self.body.velocity.y,
            self.pitch(),
            self.roll(),
            self.aoa(),
            r.x,
            r.y,
            r.z,
            self.flight.throttle,
            self.flight.boost,
        )
    }
}

fn pilot(throttle: f32, pitch: f32, roll: f32, steer: f32) -> Controls {
    Controls {
        throttle,
        pitch,
        roll,
        steer,
        occupied: true,
        ..default()
    }
}

/// Stick that holds the altitude and wings level.
fn hold_altitude(sim: &Sim, target: f32, throttle: f32, boost: bool) -> Controls {
    let climb = sim.body.velocity.y;
    let wanted_climb = ((target - sim.body.position.y) * 0.3).clamp(-15.0, 15.0);
    let pitch_rate = sim.local_rates().x;
    let pitch = ((wanted_climb - climb) * 0.05 - pitch_rate * 0.01).clamp(-1.0, 1.0);
    let roll = (-sim.roll() * 0.03 - sim.local_rates().z * 0.005).clamp(-1.0, 1.0);
    Controls {
        boost,
        ..pilot(throttle, pitch, roll, 0.0)
    }
}

fn jet(name: &str) {
    let Some(model) = load(name) else {
        println!("{name}: not imported, skipped");
        return;
    };
    println!("== {name}: inertia {:.0}", model.inertia);

    // Letting go at cruise speed.
    let mut sim = Sim::new(&model, 1000.0, 100.0);
    for _ in 0..5 {
        sim.run(2.0, |_| pilot(0.0, 0.0, 0.0, 0.0));
        println!("hands off   {}", sim.line());
    }

    // Full stick in each axis from level flight at 100 m/s.
    for (label, controls) in [
        ("pull", pilot(0.0, 1.0, 0.0, 0.0)),
        ("push", pilot(0.0, -1.0, 0.0, 0.0)),
        ("roll right", pilot(0.0, 0.0, 1.0, 0.0)),
        ("rudder right", pilot(0.0, 0.0, 0.0, 1.0)),
    ] {
        let mut sim = Sim::new(&model, 1000.0, 100.0);
        for _ in 0..6 {
            sim.run(0.25, |_| controls);
            println!("{label:12}{}", sim.line());
        }
        let mut sim = Sim::new(&model, 1000.0, 100.0);
        sim.run(1.0, |_| controls);
        sim.run(1.5, |_| pilot(0.0, 0.0, 0.0, 0.0));
        println!("{label:12}{} (1 s, then let go)", sim.line());
    }

    // Speeds, holding the altitude.
    for (label, throttle, boost) in [("cruise", 0.0, false), ("full", 1.0, false), ("afterburner", 1.0, true)] {
        let mut sim = Sim::new(&model, 1000.0, 90.0);
        sim.run(40.0, |s| hold_altitude(s, 1000.0, throttle, boost));
        println!("{label:12}{}", sim.line());
    }

    // Slowing down: idle and air brake while holding the altitude, until it can't.
    let mut sim = Sim::new(&model, 1000.0, 100.0);
    let mut stall = None;
    for _ in 0..40 {
        sim.run(0.5, |s| hold_altitude(s, 1000.0, -1.0, false));
        if stall.is_none() && sim.body.position.y < 990.0 {
            stall = Some(sim.speed());
        }
    }
    println!("slowing     {} (couldn't hold the altitude below {:.0} m/s)", sim.line(), stall.unwrap_or(0.0));
}

fn helicopter(name: &str) {
    let Some(model) = load(name) else {
        println!("{name}: not imported, skipped");
        return;
    };
    println!("== {name}: inertia {:.0}", model.inertia);
    let mut sim = Sim::new(&model, 100.0, 0.0);
    sim.run(5.0, |_| pilot(0.0, 0.0, 0.0, 0.0));
    println!("hover       {}", sim.line());
    sim.run(4.0, |_| pilot(1.0, 0.0, 0.0, 0.0));
    println!("climb       {}", sim.line());
    sim.run(4.0, |_| pilot(-1.0, 0.0, 0.0, 0.0));
    println!("sink        {}", sim.line());
    for (label, controls) in [
        ("nose down", pilot(0.0, -1.0, 0.0, 0.0)),
        ("roll right", pilot(0.0, 0.0, 1.0, 0.0)),
        ("yaw right", pilot(0.0, 0.0, 0.0, 1.0)),
    ] {
        let mut sim = Sim::new(&model, 100.0, 0.0);
        sim.run(0.5, |_| controls);
        println!("{label:12}{}", sim.line());
        sim.run(2.0, |_| pilot(0.0, 0.0, 0.0, 0.0));
        println!("{label:12}{} (let go)", sim.line());
    }
    // Forward flight at a held 30 degree nose-down attitude.
    let mut sim = Sim::new(&model, 100.0, 0.0);
    for _ in 0..4 {
        sim.run(5.0, |s| {
            let pitch = ((-30.0 - s.pitch()) * 0.05 - s.local_rates().x * 0.01).clamp(-1.0, 1.0);
            pilot(0.0, pitch, 0.0, 0.0)
        });
        println!("forward     {}", sim.line());
    }
}

#[test]
fn aircraft() {
    for name in ["usair_f18", "air_j10", "ruair_mig29", "ruair_su34", "air_f35b"] {
        jet(name);
    }
    for name in ["ahe_ah1z", "usthe_uh60", "ahe_havoc"] {
        helicopter(name);
    }
}
