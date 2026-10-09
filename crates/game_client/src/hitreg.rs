//! Hit registration checks for scenarios: how the hit zones bullets are judged against line up
//! with the soldier as drawn.
//!
//! - `HitGeometry(label, seconds)`: every frame, for points on the nearest enemy as drawn (the
//!   middle of each body part, and his gun), whether a ray from our eye through the point
//!   meets the hit zones the server judges against at the drawn moment, and which. Also how
//!   far each zone's capsule is from the same capsule on the drawn skeleton. Logs a summary
//!   (`hitreg geo ...`, `hitreg err ...`).
//! - `ShootAt(label, part, shots)`: aims exactly at a point of the nearest enemy as drawn
//!   (`all` takes the parts in turn) and fires single shots. Logs each shot (`hitreg shot`)
//!   with what the ray meets on the drawn skeleton at the moment it was fired; with
//!   `BF2_HITREG_LOG=1` the tracers (`hitreg tracer`) and the server (`hitreg fire`,
//!   `hitreg verdict`) log theirs.
//!
//! Needs hit zones imported with their bone offsets (`bf2-import soldiers`). Pair with target
//! dummies (`game_server::dummy`) and `BF2_NO_SPREAD=1`.

use bevy::{platform::collections::HashMap, prelude::*, transform::TransformSystems};
use game_data::HitZone;
use game_shared::{
    hitzones::{BodyPose, ray_capsule},
    input::Buttons,
    protocol::{ControlledBy, ShotFired, Team},
    soldier::{Soldier, SoldierMotion, Stance},
    weapons::{Armory, Inventory, Loadout},
};

use crate::{
    combat::{CombatFeedback, DrawnTargets, LocalShot},
    local_input::LookState,
    net::{LocalPlayer, LocalSoldier},
    prediction::SoldierRender,
    render::soldiers::{HeldWeapon, VisualOf},
    scenario::ScenarioInput,
};

pub struct HitregPlugin;

impl Plugin for HitregPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HitregTask>()
            .init_resource::<Drawn>()
            .add_systems(PostUpdate, (measure, run_task).chain().after(TransformSystems::Propagate))
            .add_systems(Update, (note_enemy_shots, aim).before(crate::scenario::ScenarioSystems));
    }
}

/// Body parts aimed at: the middle of a zone's capsule on the drawn skeleton, or a point on
/// the drawn gun (`muzzle`: its last few centimeters; `gunmid`: halfway along it).
pub const PARTS: [(&str, &str); 11] = [
    ("head", "head"),
    ("chest", "spine3"),
    ("belly", "spine2"),
    ("rupperarm", "right_shoulder"),
    ("rforearm", "right_low_arm"),
    ("lupperarm", "left_shoulder"),
    ("lforearm", "left_low_arm"),
    ("thigh", "left_upperleg"),
    ("shin", "right_lowerleg"),
    ("muzzle", ""),
    ("gunmid", ""),
];

#[derive(Clone, Debug)]
pub enum Request {
    Geometry { label: String, seconds: f32 },
    Shoot { label: String, part: String, shots: u32 },
}

/// The step a scenario asked for; `None` once done.
#[derive(Resource, Default)]
pub struct HitregTask {
    pub request: Option<Request>,
    started: Option<f32>,
    geometry: HashMap<(String, String), GeoTally>,
    errors: HashMap<(String, String), ErrTally>,
    shot: ShotState,
}

#[derive(Default)]
struct ShotState {
    fired: u32,
    /// Shots fired so far (`CombatFeedback::shots_fired`) when the trigger went down.
    pressed_at: Option<(u32, f32)>,
    next_at: f32,
    aimed_frames: u32,
    /// The fire mode button is held until then.
    mode_until: f32,
}

#[derive(Default)]
struct GeoTally {
    samples: u32,
    drawn_hits: u32,
    judged_hits: u32,
    same_zone: u32,
    /// Hits and same zones with the stance's capsules.
    stance_hits: u32,
    stance_same: u32,
    /// Judged hits by zone.
    zones: HashMap<String, u32>,
    /// Samples where the point is clear of every drawn capsule (for the gun).
    clear: u32,
}

#[derive(Default)]
struct ErrTally {
    samples: u32,
    sum: f32,
    max: f32,
}

/// The nearest enemy as drawn this frame.
#[derive(Resource, Default)]
struct Drawn {
    target: Option<Seen>,
    /// Last frame's: what was on screen when this frame's input (and shots) were made.
    shown: Option<Seen>,
    /// Seconds (real time) of the last shot the target fired.
    enemy_shot_at: HashMap<Entity, f32>,
}

#[derive(Clone)]
struct Seen {
    entity: Entity,
    render: SoldierRender,
    reloading: bool,
    firing: bool,
    zones: Vec<HitZone>,
    /// Each zone's capsule on the drawn skeleton.
    drawn: Vec<Option<(Vec3, Vec3)>>,
    /// Each zone's capsule as the server poses it for the drawn moment.
    judged: Vec<(Vec3, Vec3)>,
    /// Each zone's capsule in the stance's pose (how the server posed them before they
    /// followed the animations).
    stance: Vec<(Vec3, Vec3)>,
    points: Vec<(&'static str, Vec3)>,
}

/// The nearest of the capsules a ray meets.
fn capsules_ray(capsules: &[(Vec3, Vec3)], zones: &[HitZone], origin: Vec3, direction: Vec3, max: f32) -> Option<(usize, f32)> {
    capsules
        .iter()
        .enumerate()
        .filter_map(|(i, &(a, b))| {
            let d = ray_capsule(origin, direction, a, b, zones[i].radius)?;
            (d <= max).then_some((i, d))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

impl Seen {
    /// What the pose looks like, from the replicated state.
    fn auto_label(&self) -> String {
        let stance = match self.render.stance {
            Stance::Standing => "stand",
            Stance::Crouching => "crouch",
            Stance::Prone => "prone",
        };
        let speed = Vec2::new(self.render.velocity.x, self.render.velocity.z).length();
        let mut label = stance.to_string();
        if speed > 0.5 {
            label += "-move";
        }
        if self.reloading {
            label += "-reload";
        }
        if self.firing {
            label += "-fire";
        }
        label
    }

    fn point(&self, part: &str) -> Option<Vec3> {
        self.points.iter().find(|(p, _)| *p == part).map(|(_, point)| *point)
    }

    /// The nearest zone the ray meets on the drawn skeleton.
    fn drawn_ray(&self, origin: Vec3, direction: Vec3, max: f32) -> Option<(usize, f32)> {
        self.drawn
            .iter()
            .enumerate()
            .filter_map(|(i, capsule)| {
                let (a, b) = (*capsule)?;
                let d = ray_capsule(origin, direction, a, b, self.zones[i].radius)?;
                (d <= max).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    fn judged_ray(&self, origin: Vec3, direction: Vec3, max: f32) -> Option<(usize, f32)> {
        capsules_ray(&self.judged, &self.zones, origin, direction, max)
    }

    fn stance_ray(&self, origin: Vec3, direction: Vec3, max: f32) -> Option<(usize, f32)> {
        capsules_ray(&self.stance, &self.zones, origin, direction, max)
    }

    /// How far `point` is outside every drawn capsule (negative inside one).
    fn clearance(&self, point: Vec3) -> f32 {
        self.drawn
            .iter()
            .zip(&self.zones)
            .filter_map(|(capsule, zone)| {
                let (a, b) = (*capsule)?;
                let axis = b - a;
                let t = ((point - a).dot(axis) / axis.length_squared().max(1e-8)).clamp(0.0, 1.0);
                Some(point.distance(a + axis * t) - zone.radius)
            })
            .fold(f32::INFINITY, f32::min)
    }
}

/// A zone's capsule on a drawn bone.
pub fn drawn_capsule(zone: &HitZone, bone: &GlobalTransform) -> Option<(Vec3, Vec3)> {
    (zone.length != 0.0).then(|| {
        let offset = Vec3::from(zone.offset);
        (
            bone.transform_point(offset),
            bone.transform_point(offset + Vec3::new(0.0, -zone.length, 0.0)),
        )
    })
}

fn note_enemy_shots(time: Res<Time<Real>>, mut shots: MessageReader<ShotFired>, mut drawn: ResMut<Drawn>) {
    let now = time.elapsed_secs();
    for shot in shots.read() {
        drawn.enemy_shot_at.insert(shot.soldier, now);
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn measure(
    time: Res<Time<Real>>,
    armory: Res<Armory>,
    mut drawn: ResMut<Drawn>,
    task: Res<HitregTask>,
    local_team: Query<&Team, With<LocalPlayer>>,
    local: Query<&SoldierRender, With<LocalSoldier>>,
    soldiers: Query<
        (Entity, &SoldierRender, &Loadout, Option<&Inventory>, &ControlledBy, Option<&VisualOf>),
        (With<Soldier>, Without<LocalSoldier>),
    >,
    teams: Query<&Team>,
    children: Query<&Children>,
    names: Query<(&Name, &GlobalTransform)>,
    held: Query<&HeldWeapon>,
    parts: Query<(&Mesh3d, &GlobalTransform)>,
    meshes: Res<Assets<Mesh>>,
    judged: DrawnTargets,
) {
    drawn.shown = drawn.target.take();
    if task.request.is_none() {
        return;
    }
    let (Ok(me), my_team) = (local.single(), local_team.single().ok().copied()) else {
        drawn.target = None;
        return;
    };
    let eye = me.eye_position();
    let Some((entity, render, loadout, inventory, _, visual)) = soldiers
        .iter()
        .filter(|(.., c, _)| teams.get(c.0).ok().copied() != my_team)
        .filter(|(.., v)| v.is_some())
        .min_by(|a, b| a.1.position.distance(eye).total_cmp(&b.1.position.distance(eye)))
    else {
        drawn.target = None;
        return;
    };
    let visual = visual.map(|v| v.0).unwrap_or(Entity::PLACEHOLDER);
    let zones: Vec<HitZone> = armory.hit_zones(&loadout.kit).to_vec();
    let mut drawn_capsules: Vec<Option<(Vec3, Vec3)>> = vec![None; zones.len()];
    for descendant in children.iter_descendants(visual) {
        if let Ok((name, transform)) = names.get(descendant) {
            for (i, zone) in zones.iter().enumerate() {
                if zone.bone == name.as_str() {
                    drawn_capsules[i] = drawn_capsule(zone, transform);
                }
            }
        }
    }
    let pose = BodyPose {
        position: render.position,
        yaw: render.yaw,
        stance: render.stance,
        anim: None,
    };
    let stance: Vec<(Vec3, Vec3)> = zones.iter().map(|zone| pose.capsule(zone)).collect();
    let targets = judged.collect();
    let judged: Vec<(Vec3, Vec3)> = match targets.iter().find(|t| t.entity == entity) {
        Some(target) => zones.iter().map(|zone| judged.capsule(target, zone)).collect(),
        None => stance.clone(),
    };
    let mut points = Vec::new();
    for (part, bone) in PARTS {
        if let Some((a, b)) = zones
            .iter()
            .position(|z| z.bone == bone)
            .and_then(|i| drawn_capsules[i])
        {
            points.push((part, (a + b) * 0.5));
        }
    }
    // The gun: its vertices in the soldier's frame (forward is -Z).
    let body = Transform::from_translation(render.position).with_rotation(Quat::from_rotation_y(render.yaw));
    let to_body = body.compute_affine().inverse();
    let mut vertices: Vec<Vec3> = Vec::new();
    if let Ok(held) = held.get(visual) {
        for &part in &held.parts {
            let Ok((mesh, transform)) = parts.get(part) else {
                continue;
            };
            let Some(positions) = meshes
                .get(&mesh.0)
                .and_then(|m| m.attribute(Mesh::ATTRIBUTE_POSITION))
                .and_then(|a| a.as_float3())
            else {
                continue;
            };
            vertices.extend(positions.iter().map(|p| to_body.transform_point3(transform.transform_point(Vec3::from(*p)))));
        }
    }
    if !vertices.is_empty() {
        let front = vertices.iter().map(|v| v.z).fold(f32::INFINITY, f32::min);
        let back = vertices.iter().map(|v| v.z).fold(f32::NEG_INFINITY, f32::max);
        let average = |near: f32, band: f32| {
            let chosen: Vec<Vec3> = vertices.iter().copied().filter(|v| (v.z - near).abs() <= band).collect();
            (!chosen.is_empty()).then(|| chosen.iter().sum::<Vec3>() / chosen.len() as f32)
        };
        if let Some(muzzle) = average(front + 0.03, 0.03) {
            points.push(("muzzle", body.transform_point(muzzle)));
        }
        if let Some(middle) = average((front + back) * 0.5, 0.02) {
            points.push(("gunmid", body.transform_point(middle)));
        }
    }
    let now = time.elapsed_secs();
    drawn.target = Some(Seen {
        entity,
        render: *render,
        reloading: inventory.is_some_and(|i| i.reloading),
        firing: drawn.enemy_shot_at.get(&entity).is_some_and(|t| now - t < 0.4),
        zones,
        drawn: drawn_capsules,
        judged,
        stance,
        points,
    });
}

/// Keeps our aim on the part being shot at.
fn aim(task: Res<HitregTask>, drawn: Res<Drawn>, local: Query<&SoldierRender, With<LocalSoldier>>, mut look: ResMut<LookState>) {
    let Some(Request::Shoot { part, .. }) = &task.request else {
        return;
    };
    let (Some(target), Ok(me)) = (&drawn.target, local.single()) else {
        return;
    };
    let part = shot_part(part, task.shot.fired);
    let Some(point) = target.point(part) else {
        return;
    };
    let to = point - me.eye_position();
    look.yaw = (-to.x).atan2(-to.z);
    look.pitch = (to.y / to.length().max(0.01)).asin();
}

fn shot_part(part: &str, index: u32) -> &str {
    if part == "all" { PARTS[index as usize % PARTS.len()].0 } else { part }
}

#[allow(clippy::too_many_arguments)]
fn run_task(
    time: Res<Time<Real>>,
    mut task: ResMut<HitregTask>,
    drawn: Res<Drawn>,
    feedback: Res<CombatFeedback>,
    mut input: ResMut<ScenarioInput>,
    mut local_shots: MessageReader<LocalShot>,
    local: Query<&SoldierMotion, With<LocalSoldier>>,
    view: Res<crate::combat::ViewTick>,
    (armory, weapons, spatial, inputs, ack): (
        Res<Armory>,
        Query<(&Loadout, &Inventory), With<LocalSoldier>>,
        avian3d::prelude::SpatialQuery,
        Res<crate::local_input::InputHistory>,
        Query<&game_shared::soldier::InputAck, With<LocalSoldier>>,
    ),
) {
    // How many ticks our input runs ahead of the server applying it (the round trip plus
    // the server's queue).
    let ahead = match (inputs.latest(), ack.single()) {
        (Some(latest), Ok(ack)) => latest.seq.wrapping_sub(ack.0) as i64,
        _ => -1,
    };
    // Whether the world (or a vehicle) is in the way to a point: those shots and samples say
    // nothing about the hit zones.
    let occluded = |from: Vec3, to: Vec3| {
        let filter = avian3d::prelude::SpatialQueryFilter::from_mask([
            game_shared::physics::GameLayer::World,
            game_shared::physics::GameLayer::Vehicle,
        ]);
        Dir3::new(to - from).is_ok_and(|dir| spatial.cast_ray(from, dir, from.distance(to), true, &filter).is_some())
    };
    let now = time.elapsed_secs();
    // Single shots: the fire mode button until the weapon is on single fire.
    if task.shot.mode_until <= now {
        input.buttons.remove(Buttons::FIRE_MODE);
    }
    let single = weapons.single().ok().and_then(|(loadout, inventory)| {
        let weapon = armory.weapon(loadout.weapons.get(inventory.active as usize)?)?;
        let wanted = weapon.fire_modes.iter().position(|m| *m == game_data::FireMode::Single)?;
        Some(wanted == inventory.fire_mode as usize)
    });
    let Some(request) = task.request.clone() else {
        local_shots.clear();
        return;
    };
    let started = *task.started.get_or_insert(now);
    match request {
        Request::Geometry { label, seconds } => {
            if let (Some(target), Ok(me)) = (&drawn.target, local.single()) {
                let eye = me.eye_position();
                let auto = target.auto_label();
                let label = format!("{label}/{auto}");
                for &(part, point) in &target.points {
                    if occluded(eye, point) {
                        continue;
                    }
                    let direction = (point - eye).normalize();
                    let max = eye.distance(point) + 1.0;
                    let drawn_hit = target.drawn_ray(eye, direction, max);
                    let judged_hit = target.judged_ray(eye, direction, max);
                    let stance_hit = target.stance_ray(eye, direction, max);
                    let tally = task.geometry.entry((label.clone(), part.to_string())).or_default();
                    tally.samples += 1;
                    tally.drawn_hits += u32::from(drawn_hit.is_some());
                    tally.judged_hits += u32::from(judged_hit.is_some());
                    tally.same_zone += u32::from(drawn_hit.is_some() && drawn_hit.map(|h| h.0) == judged_hit.map(|h| h.0));
                    tally.stance_hits += u32::from(stance_hit.is_some());
                    tally.stance_same += u32::from(drawn_hit.is_some() && drawn_hit.map(|h| h.0) == stance_hit.map(|h| h.0));
                    if let Some((zone, _)) = judged_hit {
                        *tally.zones.entry(target.zones[zone].bone.clone()).or_default() += 1;
                    }
                    tally.clear += u32::from(target.clearance(point) > 0.02);
                }
                for (i, zone) in target.zones.iter().enumerate() {
                    let Some((a, b)) = target.drawn[i] else {
                        continue;
                    };
                    for (kind, capsules) in [("judged", &target.judged), ("stance", &target.stance)] {
                        let (ja, jb) = capsules[i];
                        let error = a.distance(ja).max(b.distance(jb));
                        let tally = task.errors.entry((format!("{label} {kind}"), zone.bone.clone())).or_default();
                        tally.samples += 1;
                        tally.sum += error;
                        tally.max = tally.max.max(error);
                    }
                }
            }
            if now - started >= seconds {
                report(&mut task);
                finish(&mut task);
            }
        }
        Request::Shoot { label, part, shots } => {
            // Each shot we fired: what its ray meets on the skeleton drawn this frame.
            for shot in local_shots.read() {
                // The shot was made looking at last frame's picture.
                let Some(target) = drawn.shown.as_ref().or(drawn.target.as_ref()) else {
                    info!("hitreg shot: no target drawn");
                    continue;
                };
                let aimed = shot_part(&part, task.shot.fired.saturating_sub(1));
                let max = 500.0;
                let seen = target.drawn_ray(shot.origin, shot.direction, max);
                let judged = target.judged_ray(shot.origin, shot.direction, max);
                let zone = |hit: Option<(usize, f32)>| hit.map_or("miss".to_string(), |(i, _)| target.zones[i].bone.clone());
                let point = target.point(aimed).unwrap_or(Vec3::NAN);
                // How far the ray passes from the point aimed at.
                let along = (point - shot.origin).dot(shot.direction);
                let miss_by = (shot.origin + shot.direction * along).distance(point);
                info!(
                    "hitreg shot {}: {label}/{} part {aimed} seen {} judged-here {} aim-error {:.3} view_tick {} clearance {:.3} target {:?} ahead {ahead}{} (drawn at {:.3})",
                    task.shot.fired,
                    target.auto_label(),
                    zone(seen),
                    zone(judged),
                    miss_by,
                    view.shown,
                    target.clearance(point),
                    target.entity,
                    if occluded(shot.origin, point) { " occluded" } else { "" },
                    target.render.position,
                );
            }
            let state = &mut task.shot;
            match state.pressed_at {
                Some((before, at)) => {
                    if feedback.shots_fired != before || now - at > 2.0 {
                        input.buttons.remove(Buttons::FIRE);
                        if feedback.shots_fired == before {
                            info!("hitreg shot: the weapon didn't fire");
                        }
                        state.pressed_at = None;
                        state.next_at = now + 0.35;
                        state.aimed_frames = 0;
                    }
                }
                None if state.fired >= shots => {
                    report(&mut task);
                    finish(&mut task);
                }
                None if single == Some(false) && now >= state.next_at => {
                    input.buttons.insert(Buttons::FIRE_MODE);
                    state.mode_until = now + 0.1;
                    state.next_at = now + 0.4;
                }
                None if now >= state.next_at && drawn.target.is_some() => {
                    // A few frames on target first, so the aim has reached the server's ticks.
                    state.aimed_frames += 1;
                    if state.aimed_frames > 4 {
                        input.buttons.insert(Buttons::FIRE);
                        state.pressed_at = Some((feedback.shots_fired, now));
                        state.fired += 1;
                    }
                }
                None => {}
            }
        }
    }
}

fn finish(task: &mut HitregTask) {
    task.request = None;
    task.started = None;
    task.shot = ShotState::default();
}

fn report(task: &mut HitregTask) {
    let mut rows: Vec<_> = task.geometry.drain().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for ((label, part), t) in rows {
        let pct = |n: u32| 100.0 * n as f32 / t.samples.max(1) as f32;
        let mut zones: Vec<_> = t.zones.into_iter().collect();
        zones.sort_by(|a, b| b.1.cmp(&a.1));
        let zones: Vec<String> = zones.iter().map(|(z, n)| format!("{z} {:.0}%", pct(*n))).collect();
        info!(
            "hitreg geo {label} {part}: drawn {:.0}% judged {:.0}% same-zone {:.0}% stance {:.0}% stance-same {:.0}% clear {:.0}% n={} [{}]",
            pct(t.drawn_hits),
            pct(t.judged_hits),
            pct(t.same_zone),
            pct(t.stance_hits),
            pct(t.stance_same),
            pct(t.clear),
            t.samples,
            zones.join(", ")
        );
    }
    let mut rows: Vec<_> = task.errors.drain().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for ((label, zone), t) in rows {
        info!(
            "hitreg err {label} {zone}: mean {:.1} cm max {:.1} cm n={}",
            100.0 * t.sum / t.samples.max(1) as f32,
            100.0 * t.max,
            t.samples
        );
    }
}
