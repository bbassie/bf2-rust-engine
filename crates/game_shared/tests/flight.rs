//! Flight model checks on imported aircraft: flies them in the air (no ground, no Bevy app)
//! with the same forces the server uses, integrating the rigid body like avian does, and
//! prints what they do. Skipped when `imported/vehicles/` lacks the aircraft.
//!
//! `cargo test -p game_shared --test flight -- --nocapture`

use std::path::Path;

use bevy::prelude::*;
use game_data::VehicleCategory;
use game_shared::{
    config::GamePaths,
    flight::{BodyState, Controls, FlightState, JetEnvelope, JetMouse, Surroundings, flight_forces, jet_full_rates, limit_pitch},
    vehicle::{VehicleModel, integrate, step_joints},
};

const DT: f32 = 1.0 / 60.0;

fn load(name: &str) -> Option<VehicleModel> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported");
    let desc = game_data::read_ron(root.join("vehicles").join(format!("{name}.ron"))).ok()?;
    Some(VehicleModel::new(desc, &GamePaths { imported: root, mods: Vec::new() }))
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
                spin: if model.desc.category == VehicleCategory::Helicopter { 1.0 } else { 0.0 },
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
    for name in ["usair_f18", "air_j10", "ruair_mig29", "ruair_su34", "air_f35b", "air_a10", "air_su39"] {
        jet(name);
    }
    for name in ["ahe_ah1z", "usthe_uh60", "ahe_havoc"] {
        helicopter(name);
    }
}

/// A mouse model for [`jet_mouse`]: this tick's counts (right, up) in, the stick (roll right,
/// pitch up) out.
trait MouseStick {
    fn stick(&mut self, sim: &Sim, counts: Vec2) -> Vec2;
}

/// The helicopters' mouse stick (`game_client::vehicles`), which jets used before: counts
/// deflect a stick that centres itself in 1/8 s.
struct Spring(Vec2);

impl MouseStick for Spring {
    fn stick(&mut self, _: &Sim, counts: Vec2) -> Vec2 {
        const STICK_PER_COUNT: f32 = 0.032;
        const STICK_CENTERING: f32 = 8.0;
        self.0 = (self.0 + counts * STICK_PER_COUNT).clamp(Vec2::NEG_ONE, Vec2::ONE);
        self.0 *= (-STICK_CENTERING * DT).exp();
        self.0
    }
}

/// Jets' mouse now: [`JetMouse`], mouse X on the roll.
struct Jet(JetMouse);

impl MouseStick for Jet {
    fn stick(&mut self, sim: &Sim, counts: Vec2) -> Vec2 {
        let airspeed = -(sim.body.rotation.inverse() * sim.body.velocity).z;
        let rates = jet_full_rates(&sim.model.desc, airspeed.max(0.0));
        let stall_angle = sim.model.desc.aero.as_ref().map_or(90.0, |aero| aero.stall_angle);
        let stick = self.0.update(Vec3::new(counts.y, 0.0, counts.x), rates, DT, |pitch| limit_pitch(&sim.body, stall_angle, pitch));
        Vec2::new(stick.z, stick.x)
    }
}

/// Flies `seconds` with the mouse moving at `speed` counts per second (right, up) while
/// `moving(t)`; returns the samples (time, pitch, roll), the pitch unwrapped through the
/// vertical (integrated from the pitch rate, for loops).
fn mouse_run(sim: &mut Sim, mouse: &mut dyn MouseStick, seconds: f32, speed: Vec2, moving: impl Fn(f32) -> bool) -> Vec<(f32, f32, f32)> {
    // Full throttle (the loop) if it's already open, else cruise.
    let throttle = if sim.flight.throttle > 0.9 { 1.0 } else { 0.0 };
    let mut samples = Vec::new();
    let start = sim.time;
    let mut pitch = sim.pitch();
    for _ in 0..(seconds / DT).round() as usize {
        let t = sim.time - start;
        let counts = if moving(t) { speed * DT } else { Vec2::ZERO };
        let stick = mouse.stick(sim, counts);
        sim.step(&pilot(throttle, stick.y, stick.x, 0.0));
        pitch += sim.local_rates().x * DT;
        samples.push((sim.time - start, pitch, sim.roll()));
    }
    samples
}

fn first(samples: &[(f32, f32, f32)], reached: impl Fn(&(f32, f32, f32)) -> bool) -> f32 {
    samples.iter().find(|s| reached(s)).map_or(f32::NAN, |s| s.0)
}

fn at(samples: &[(f32, f32, f32)], t: f32) -> f32 {
    samples.iter().find(|s| s.0 >= t - 1e-4).map_or(f32::NAN, |s| s.1)
}

/// Jet handling with the mouse, before (the self-centring stick) and now ([`JetMouse`]), and
/// the stick's pitch rate at each speed next to BF2's own (its raw wing torques). Prints
/// tables; asserts that one mouse movement gives a steady climb at the corner speed and fast,
/// that the nose holds once the mouse stops, and that a loop flies with the mouse moving.
/// `cargo test -p game_shared --test flight jet_mouse -- --nocapture`
#[test]
fn jet_mouse() {
    let mut failures = Vec::new();
    for name in ["air_f35b", "usair_f18", "ruair_su34", "air_j10"] {
        let Some(model) = load(name) else {
            println!("{name}: not imported, skipped");
            continue;
        };
        let envelope = JetEnvelope::of(&model.desc);
        let top = envelope.corner / 0.7;
        println!("== {name}: corner {:.0} m/s, stall {:.0} m/s, top {:.0} m/s", envelope.corner, envelope.stall, top);

        // Full stick back for 1 s: the pitch rate, and BF2's raw wings' (the stick deflecting
        // the elevators, nobody flying by wire).
        let mut rates = Vec::new();
        for speed in [envelope.stall * 1.3, 60.0, envelope.corner, top, top * 1.35] {
            let mut sim = Sim::new(&model, 1000.0, speed);
            sim.run(1.0, |_| pilot(0.0, 1.0, 0.0, 0.0));
            rates.push(format!(
                "{speed:.0} m/s {:3.0}°/s (aoa {:4.1}°, BF2 {:3.0})",
                sim.local_rates().x,
                sim.aoa(),
                bf2_raw_pitch(&model, speed)
            ));
        }
        println!("  full stick pitch rate: {}", rates.join(", "));

        let models: [(&str, fn() -> Box<dyn MouseStick>); 2] =
            [("spring", || Box::new(Spring(Vec2::ZERO))), ("now", || Box::new(Jet(JetMouse::default())))];
        for (label, make) in models {
            let mut cells = Vec::new();
            for (speed_label, speed) in [("slow", envelope.stall * 1.6), ("corner", envelope.corner), ("fast", top)] {
                // One mouse movement up of 150 counts, quick (0.1 s) and slow (0.5 s): the
                // climb angle 1 s and 3 s later.
                let mut sim = Sim::new(&model, 1000.0, speed);
                let flick = mouse_run(&mut sim, make().as_mut(), 3.0, Vec2::new(0.0, 1500.0), |t| t < 0.1);
                let mut sim = Sim::new(&model, 1000.0, speed);
                let slow = mouse_run(&mut sim, make().as_mut(), 3.0, Vec2::new(0.0, 300.0), |t| t < 0.5);
                cells.push(format!(
                    "{speed_label} quick {:4.1}°→{:4.1}° slow {:4.1}°→{:4.1}°",
                    at(&flick, 1.0),
                    at(&flick, 3.0),
                    at(&slow, 1.0),
                    at(&slow, 3.0)
                ));
                // Now: the slow movement's climb, the same at every speed but the slowest
                // (where the nose can't come up that quickly), holding afterwards; the quick
                // one outruns the jet, but most of it still counts.
                if label == "now" && speed_label != "slow" {
                    let (a, b, q) = (at(&slow, 1.0), at(&slow, 3.0), at(&flick, 3.0));
                    if !(25.0..35.0).contains(&a) || (b - a).abs() > 3.0 || q < 15.0 {
                        failures.push(format!("{name} {speed_label}: 150 counts up gave {a:.1}° after 1 s, {b:.1}° after 3 s, quickly {q:.1}°"));
                    }
                }
            }
            println!("  {label:6} 150 counts up: {}", cells.join(" | "));

            // The mouse moving up steadily (600 counts/s) at the corner speed: when the climb
            // reaches 20° and 30°; moving up at 250 counts/s (as quickly as the jet turns) and
            // stopping at 30°, where the nose is 1 s later.
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            let steady = mouse_run(&mut sim, make().as_mut(), 4.0, Vec2::new(0.0, 600.0), |_| true);
            let t20 = first(&steady, |s| s.1 >= 20.0);
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            let gentle = mouse_run(&mut sim, make().as_mut(), 4.0, Vec2::new(0.0, 250.0), |_| true);
            let t30 = first(&gentle, |s| s.1 >= 30.0);
            let stopped_at = if t30.is_nan() { f32::NAN } else { at(&gentle, t30) };
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            let hold = mouse_run(&mut sim, make().as_mut(), t30 + 3.0, Vec2::new(0.0, 250.0), |t| t < t30 - DT * 0.5);
            let (after, later) = (at(&hold, t30 + 1.0), at(&hold, t30 + 3.0 - DT));
            // A loop at full throttle, the mouse moving up at 1200 counts/s.
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            sim.flight.throttle = 1.0;
            let lp = mouse_run(&mut sim, make().as_mut(), 12.0, Vec2::new(0.0, 1200.0), |_| true);
            let loop_time = first(&lp, |s| s.1 >= 360.0);
            // Roll: 150 counts right in 0.1 s.
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            let roll = mouse_run(&mut sim, make().as_mut(), 2.0, Vec2::new(1500.0, 0.0), |t| t < 0.1);
            let bank = roll.last().map_or(f32::NAN, |s| s.2);
            // Dive: 300 counts down in 1 s (60°), the nose angle 2 s later.
            let mut sim = Sim::new(&model, 1000.0, envelope.corner);
            let dive = mouse_run(&mut sim, make().as_mut(), 3.0, Vec2::new(0.0, -300.0), |t| t < 1.0);
            let dive = at(&dive, 3.0 - DT);
            println!(
                "  {label:6} mouse up 600/s: 20° in {t20:.2} s; 250/s stopped at {stopped_at:.0}°, 1 s later {after:.0}°, 3 s later {later:.0}°; loop (1200/s) {loop_time:.1} s; 150 counts right: bank {bank:.0}°; 300 down: {dive:.0}°"
            );
            if label == "now" {
                if dive > -50.0 {
                    failures.push(format!("{name}: 300 counts down only pointed the nose {dive:.0}°"));
                }
                if !(t20 < 0.7) {
                    failures.push(format!("{name}: 20° climb took {t20:.2} s with the mouse moving up"));
                }
                // The nose stops within about 0.15 s of its rate and stays there.
                if !((after - stopped_at).abs() < 9.0 && (later - after).abs() < 2.0) {
                    failures.push(format!("{name}: stopped at {stopped_at:.0}°, 1 s later {after:.0}°, 3 s later {later:.0}°"));
                }
                if !(loop_time < 10.0) {
                    failures.push(format!("{name}: loop took {loop_time:.1} s"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// BF2's own pitch rate at an airspeed (degrees per second after 1 s of full stick back): its
/// raw wing forces, the elevators deflected by the stick, without the fly-by-wire.
fn bf2_raw_pitch(model: &VehicleModel, speed: f32) -> f32 {
    let mut sim = Sim::new(model, 1000.0, speed);
    let stick = pilot(0.0, 1.0, 0.0, 0.0);
    // An empty seat for the forces (no fly-by-wire, engines idle), the stick on the joints.
    let empty = Controls { occupied: false, ..stick };
    for _ in 0..(1.0 / DT) as usize {
        let (joints, _) = step_joints(sim.model, &sim.body, &[], &sim.joints, &stick, &sim.flight, DT);
        sim.joints = joints;
        let around = Surroundings { water: None, altitude: sim.body.position.y };
        let push = flight_forces(sim.model, &sim.body, &sim.joints, &empty, &mut sim.flight, &around, DT);
        integrate(sim.model, &mut sim.body, &push, DT);
    }
    sim.local_rates().x
}
