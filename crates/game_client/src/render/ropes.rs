//! Grappling ropes and ziplines.
//!
//! A grappling rope is drawn the way BF2 builds it: a chain of links (`setNumberOfLinks`,
//! each `setMaxRopeLength / links` long) simulated with [`RopeSim`]. It trails the hook as
//! it flies; where the hook catches, the server strings the [`Rope`] and the links carry on
//! from where they were: the hook drags back to the lip, the rope swings in over the edge,
//! the hanging part settles along the line soldiers climb and the rest piles up on the
//! ground, and after a few seconds it goes to sleep. A climber's hands and feet hold it (BF2
//! clips the rope onto the climber), which wakes it up. A hook that finds nothing to hold
//! on to lies where it fell with its rope until the server takes it away.
//!
//! Ropes are drawn as smooth tubes through the links, with the xpack's rope texture and the
//! thickness of its `ropelink` model; the hook is the xpack's hook model. Ziplines are thin
//! cylinders along the wire, with the shooter's stand.

use avian3d::prelude::*;
use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::NoFrustumCulling,
    gltf::GltfAssetLabel,
    light::NotShadowCaster,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};
use game_data::{RopeDesc, RopeKind};
use game_shared::{
    rope::{HangLine, Pin, Rope, RopeSim, ZIPLINE_STAND, world_cast},
    weapons::Armory,
};

use super::projectiles::{ProjectileVisual, move_visuals};
use crate::prediction::SoldierRender;

pub struct RopeRenderPlugin;

impl Plugin for RopeRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, load_rope_assets)
            .init_resource::<RopeClock>()
            .add_observer(draw_zipline)
            .add_systems(
                Update,
                (start_flight_ropes, string_grapple_ropes, simulate_ropes)
                    .chain()
                    .after(move_visuals),
            );
    }
}

/// Seconds between rope simulation steps.
const STEP: f32 = 1.0 / 60.0;
/// Most steps a frame (a long frame slows the rope down rather than piling up work).
const MAX_STEPS: u32 = 3;
/// Sides of the tube drawn around the links, and points drawn per link.
const SIDES: usize = 6;
const SUBDIVISIONS: usize = 3;
/// A flying hook that disappeared waits this long for the server's rope before its links
/// fall (it found nothing to hold on to), and lies this long more before it is gone.
const ADOPT_WAIT: f32 = 0.4;
const DROPPED_TIME: f32 = 1.5;
/// A rope strung within this distance of where a flying hook disappeared took its place.
const ADOPT_DISTANCE: f32 = 16.0;
/// A rope we didn't see thrown starts hanging this far out at its bottom (m).
const UNSEEN_SWING: f32 = 1.2;
/// Seconds the hook takes to drag back from where it held to the lip.
const DRAG_TIME: f32 = 0.35;
/// How fast the hanging part settles along the climbed line (1/s).
const HANG_RATE: f32 = 2.5;
/// Where a climber holds the rope: hands and feet above his feet (the rope clip holds the
/// body low; see `soldiers::ROPE_BODY_RAISE`), and how far from the rope's line he still
/// counts as on it.
const CLIMBER_HANDS: f32 = 2.05;
const CLIMBER_FEET: f32 = 0.75;
const CLIMBER_REACH: f32 = 1.2;

#[derive(Resource, Default)]
struct RopeClock {
    accumulator: f32,
}

#[derive(Resource)]
struct RopeAssets {
    /// A unit cylinder along Y, scaled to each piece (ziplines).
    cylinder: Handle<Mesh>,
    rope: Handle<StandardMaterial>,
    metal: Handle<StandardMaterial>,
    /// The rope's own texture, once the armory says which (see [`rope_material`]).
    textured: Option<Handle<StandardMaterial>>,
}

fn load_rope_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(RopeAssets {
        cylinder: meshes.add(Cylinder::new(1.0, 1.0)),
        rope: materials.add(StandardMaterial {
            base_color: Color::srgb(0.42, 0.36, 0.26),
            perceptual_roughness: 0.9,
            ..default()
        }),
        metal: materials.add(StandardMaterial {
            base_color: Color::srgb(0.3, 0.31, 0.32),
            metallic: 0.8,
            perceptual_roughness: 0.4,
            ..default()
        }),
        textured: None,
    });
}

/// The grappling rope's material: the imported rope texture, else a plain rope color.
fn rope_material(
    assets: &mut RopeAssets,
    desc: &RopeDesc,
    asset_server: &AssetServer,
    materials: &mut Assets<StandardMaterial>,
) -> Handle<StandardMaterial> {
    let Some(texture) = &desc.texture else {
        return assets.rope.clone();
    };
    assets
        .textured
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color_texture: Some(asset_server.load(format!("imported://{texture}"))),
                perceptual_roughness: 0.95,
                ..default()
            })
        })
        .clone()
}

/// The first grappling rope any weapon strings (all BF2 has is the one).
fn grapple_desc(armory: &Armory) -> Option<(&RopeDesc, Option<&str>)> {
    armory.weapons.values().find_map(|w| {
        let rope = w.projectile.rope.as_ref().filter(|r| r.kind == RopeKind::Grapple)?;
        Some((rope, w.projectile.mesh.as_deref()))
    })
}

/// A drawn grappling rope: its links and the tube drawn through them.
#[derive(Component)]
struct RopeVisual {
    sim: RopeSim,
    mesh: Handle<Mesh>,
    /// Redraw the tube (the links moved).
    dirty: bool,
}

/// The links trailing a flying hook (the projectile's visual), then, once it is gone,
/// waiting for the server's rope.
#[derive(Component)]
struct FlightRope {
    hook: Option<Entity>,
    /// Where the hook was last.
    last_hook: Vec3,
    /// Seconds since the hook disappeared.
    orphaned: f32,
}

/// On a strung grappling rope: its hook model, and the hook dragging back to the lip.
#[derive(Component)]
struct StrungRope {
    hook_model: Option<Entity>,
    /// Where the hook held before it dragged back, and the seconds since.
    drag: Option<(Vec3, f32)>,
    /// Logged when it first went to sleep.
    settled: bool,
    age: f32,
}

fn new_rope_visual(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    material: Handle<StandardMaterial>,
    sim: RopeSim,
    parent: Option<(Entity, Transform)>,
) -> (RopeVisual, Entity) {
    let mesh = meshes.add(tube_mesh(&sim.points, sim.radius));
    // The tube's vertices are in world space: under a parent, undo its transform.
    let transform = parent.map_or(Transform::IDENTITY, |(_, t)| Transform::from_matrix(t.to_matrix().inverse()));
    let mut entity = commands.spawn((
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material),
        transform,
        NoFrustumCulling,
        Visibility::default(),
    ));
    if let Some((parent, _)) = parent {
        entity.insert(ChildOf(parent));
    }
    let id = entity.id();
    (RopeVisual { sim, mesh, dirty: false }, id)
}

/// Links behind every grappling hook that starts flying, thrown with it: each a little
/// weaker than the one before (BF2's `degradeThrowStrength`), so they string out behind it.
#[allow(clippy::too_many_arguments)]
fn start_flight_ropes(
    mut commands: Commands,
    hooks: Query<(Entity, &ProjectileVisual, &Transform), Added<ProjectileVisual>>,
    mut assets: ResMut<RopeAssets>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (entity, visual, transform) in &hooks {
        let Some(desc) = visual.weapon.projectile.rope.as_ref().filter(|r| r.kind == RopeKind::Grapple) else {
            continue;
        };
        let links = desc.links.max(1) as usize;
        let start = transform.translation;
        let velocity = visual.motion.velocity;
        let mut sim = RopeSim::new(
            vec![start; links + 1],
            desc.max_length / links as f32,
            desc.radius,
            desc.air_friction,
            desc.elasticity,
            desc.awake_time,
        );
        sim.set_velocities(STEP, |i| velocity * THROW_DEGRADE.powi(i as i32));
        sim.pins.push(Pin { index: 0, position: start, stiffness: 1.0 });
        let material = rope_material(&mut assets, desc, &asset_server, &mut materials);
        let (rope, mesh) = new_rope_visual(&mut commands, &mut meshes, material, sim, None);
        commands.entity(mesh).insert((
            rope,
            FlightRope { hook: Some(entity), last_hook: start, orphaned: 0.0 },
            Name::new("flight rope"),
        ));
    }
}

/// Share of the throw each link gets of the one before it (BF2's `degradeThrowStrength`).
const THROW_DEGRADE: f32 = 0.8;

/// Every new grappling rope: carries on with the links of the flying hook that strung it
/// (the hook dragging back to the lip), or, if we didn't see it thrown, starts settled.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn string_grapple_ropes(
    mut commands: Commands,
    added: Query<(Entity, &Rope), Added<Rope>>,
    mut flights: Query<(Entity, &mut RopeVisual, &FlightRope), Without<Rope>>,
    armory: Option<Res<Armory>>,
    mut assets: ResMut<RopeAssets>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (entity, rope) in &added {
        if rope.kind != RopeKind::Grapple {
            continue;
        }
        let desc = armory.as_deref().and_then(grapple_desc);
        let link = rope.length / rope.links.max(1) as f32;
        // The nearest flying hook that just disappeared, with links to match.
        let adopted = flights
            .iter_mut()
            .filter(|(_, visual, flight)| {
                flight.hook.is_none()
                    && flight.last_hook.distance(rope.anchor) < ADOPT_DISTANCE
                    && visual.sim.points.len() == rope.links as usize + 1
            })
            .min_by(|a, b| {
                a.2.last_hook.distance(rope.anchor).total_cmp(&b.2.last_hook.distance(rope.anchor))
            })
            .map(|(flight, visual, flight_rope)| (flight, visual.sim.clone(), flight_rope.last_hook));
        let (sim, drag) = match adopted {
            Some((flight, mut sim, held)) => {
                commands.entity(flight).despawn();
                sim.pins.clear();
                if let Some((d, _)) = desc {
                    sim.awake_time = d.awake_time;
                }
                sim.wake();
                (sim, Some((held, 0.0)))
            }
            None => {
                let (radius, friction, elasticity, awake) = desc.map_or((0.015, 0.95, 0.2, 6.0), |(d, _)| {
                    (d.radius, d.air_friction, d.elasticity, d.awake_time)
                });
                // Dropping in from a little out from the wall, so it swings in and settles.
                let mut points = rope.settled_points();
                for point in &mut points {
                    let down = ((rope.top.y - point.y) / (rope.top.y - rope.end.y).max(0.1)).clamp(0.0, 1.0);
                    *point += rope.out() * down * UNSEEN_SWING;
                }
                let sim = RopeSim::new(points, link, radius, friction, elasticity, awake);
                (sim, None)
            }
        };
        let mut sim = sim;
        sim.link = link;
        sim.hang = Some(HangLine { top: rope.top, bottom: rope.end.y, rate: HANG_RATE });
        let material = match desc {
            Some((d, _)) => rope_material(&mut assets, d, &asset_server, &mut materials),
            None => assets.rope.clone(),
        };
        let parent = rope.transform();
        let (visual, _) = new_rope_visual(&mut commands, &mut meshes, material, sim, Some((entity, parent)));
        let hook_model = desc.and_then(|(_, mesh)| mesh).map(|mesh| {
            commands
                .spawn((
                    WorldAssetRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(format!("imported://{mesh}")))),
                    hook_transform(rope, drag.map_or(rope.anchor, |(from, _)| from), &parent),
                    NotShadowCaster,
                    ChildOf(entity),
                ))
                .id()
        });
        commands.entity(entity).insert((
            visual,
            StrungRope { hook_model, drag, settled: false, age: 0.0 },
            Visibility::default(),
        ));
    }
}

/// The hook model at `at`, lying on the roof with its shank towards the edge, relative to
/// the rope entity's transform.
fn hook_transform(rope: &Rope, at: Vec3, parent: &Transform) -> Transform {
    // The model's long axis is X, the shank's ring at its -X end.
    let world = Transform::from_translation(at + Vec3::Y * HOOK_LIFT)
        .with_rotation(Quat::from_rotation_arc(Vec3::NEG_X, rope.out()));
    Transform::from_matrix(parent.to_matrix().inverse() * world.to_matrix())
}

/// How high the hook model's center sits above the surface it lies on.
const HOOK_LIFT: f32 = 0.06;

/// Steps every awake rope: pins (the hook, climbers) first, then the links, then redraws
/// the tubes that moved.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn simulate_ropes(
    mut commands: Commands,
    time: Res<Time>,
    mut clock: ResMut<RopeClock>,
    spatial: SpatialQuery,
    mut meshes: ResMut<Assets<Mesh>>,
    hooks: Query<&Transform, (With<ProjectileVisual>, Without<RopeVisual>)>,
    climbers: Query<&SoldierRender>,
    mut flights: Query<(Entity, &mut RopeVisual, &mut FlightRope), Without<Rope>>,
    mut strung: Query<(&Rope, &mut RopeVisual, &mut StrungRope)>,
    mut hook_models: Query<&mut Transform, (Without<ProjectileVisual>, Without<RopeVisual>, Without<Rope>)>,
) {
    let dt = time.delta_secs();
    clock.accumulator = (clock.accumulator + dt).min(STEP * MAX_STEPS as f32);
    let steps = (clock.accumulator / STEP) as u32;
    clock.accumulator -= steps as f32 * STEP;

    // Flying hooks pull their links along; gone, they wait for their rope, then drop them.
    for (entity, mut visual, mut flight) in &mut flights {
        match flight.hook.map(|hook| hooks.get(hook)) {
            Some(Ok(hook)) => {
                flight.last_hook = hook.translation;
                visual.sim.pins = vec![Pin { index: 0, position: hook.translation, stiffness: 1.0 }];
                visual.sim.keep_awake(1.0);
            }
            _ => {
                flight.hook = None;
                flight.orphaned += dt;
                if flight.orphaned > ADOPT_WAIT {
                    visual.sim.pins.clear();
                }
                if flight.orphaned > ADOPT_WAIT + DROPPED_TIME {
                    commands.entity(entity).despawn();
                }
            }
        }
    }

    // Strung ropes: the hook drags back to the lip; climbers hold the rope.
    for (rope, mut visual, mut strung) in &mut strung {
        strung.age += dt;
        let link = visual.sim.link;
        let mut hook = rope.anchor;
        if let Some((from, t)) = &mut strung.drag {
            *t += dt;
            let s = (*t / DRAG_TIME).min(1.0);
            hook = from.lerp(rope.anchor, s * s * (3.0 - 2.0 * s));
            if s >= 1.0 {
                strung.drag = None;
            }
            visual.sim.keep_awake(1.0);
            if let Some(model) = strung.hook_model
                && let Ok(mut transform) = hook_models.get_mut(model)
            {
                *transform = hook_transform(rope, hook, &rope.transform());
            }
        }
        let radius = visual.sim.radius;
        visual.sim.pins.clear();
        visual.sim.pins.push(Pin { index: 0, position: hook + Vec3::Y * radius, stiffness: 1.0 });
        let over_edge = rope.anchor.distance(rope.top);
        for climber in &climbers {
            if !(climber.climbing && climber.on_rope) {
                continue;
            }
            let flat = Vec2::new(climber.position.x - rope.top.x, climber.position.z - rope.top.z).length();
            if flat > CLIMBER_REACH || climber.position.y > rope.top.y + 0.5 || climber.position.y < rope.end.y - 1.0 {
                continue;
            }
            let hands = (climber.position.y + CLIMBER_HANDS).min(rope.top.y);
            let feet = (climber.position.y + CLIMBER_FEET).min(hands);
            let last = visual.sim.points.len() - 1;
            let index = |y: f32| (((over_edge + rope.top.y - y) / link).round() as usize).clamp(1, last);
            visual.sim.pins.push(Pin {
                index: index(hands),
                position: Vec3::new(rope.top.x, hands, rope.top.z),
                stiffness: 1.0,
            });
            visual.sim.pins.push(Pin {
                index: index(feet),
                position: Vec3::new(rope.top.x, feet, rope.top.z),
                stiffness: 0.3,
            });
            visual.sim.keep_awake(0.5);
        }
    }

    // The links.
    let mut cast = world_cast(&spatial);
    for _ in 0..steps {
        for (_, mut visual, _) in &mut flights {
            if !visual.sim.asleep() {
                visual.sim.step(STEP, &mut cast);
                visual.dirty = true;
            }
        }
        for (_, mut visual, _) in &mut strung {
            if !visual.sim.asleep() {
                visual.sim.step(STEP, &mut cast);
                visual.dirty = true;
            }
        }
    }
    for (rope, visual, mut strung) in &mut strung {
        if visual.sim.asleep() && !strung.settled {
            strung.settled = true;
            info!(
                "grapple rope settled after {:.1} s, {:.1} m hanging from {:.1}",
                strung.age,
                rope.top.y - rope.end.y,
                rope.anchor
            );
        }
    }

    // The tubes.
    let visuals = flights
        .iter_mut()
        .map(|(_, visual, _)| visual)
        .chain(strung.iter_mut().map(|(_, visual, _)| visual));
    for mut visual in visuals {
        if !visual.dirty {
            continue;
        }
        visual.dirty = false;
        if let Some(mut mesh) = meshes.get_mut(&visual.mesh) {
            *mesh = tube_mesh(&visual.sim.points, visual.sim.radius);
        }
    }
}

/// A smooth tube of `radius` through `points` (Catmull-Rom between them), textured around
/// and along it (one texture repeat per circumference along it, so the weave is square).
fn tube_mesh(points: &[Vec3], radius: f32) -> Mesh {
    // The centerline.
    let mut line = Vec::with_capacity(points.len() * SUBDIVISIONS);
    for i in 0..points.len().saturating_sub(1) {
        let p0 = points[i.saturating_sub(1)];
        let (p1, p2) = (points[i], points[i + 1]);
        let p3 = points[(i + 2).min(points.len() - 1)];
        for s in 0..SUBDIVISIONS {
            let t = s as f32 / SUBDIVISIONS as f32;
            let (t2, t3) = (t * t, t * t * t);
            line.push(
                0.5 * ((2.0 * p1)
                    + (p2 - p0) * t
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
                    + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3),
            );
        }
    }
    if let Some(last) = points.last() {
        line.push(*last);
    }
    line.dedup_by(|a, b| a.distance_squared(*b) < 1e-8);

    let rings = line.len();
    let mut positions = Vec::with_capacity(rings * (SIDES + 1));
    let mut normals = Vec::with_capacity(rings * (SIDES + 1));
    let mut uvs = Vec::with_capacity(rings * (SIDES + 1));
    let circumference = std::f32::consts::TAU * radius.max(1e-3);
    let mut along = 0.0;
    // Parallel transport of a frame along the line, so the tube doesn't twist.
    let mut normal = Vec3::X;
    for i in 0..rings {
        let tangent = if rings < 2 {
            Vec3::Y
        } else if i == 0 {
            (line[1] - line[0]).normalize_or(Vec3::Y)
        } else if i == rings - 1 {
            (line[i] - line[i - 1]).normalize_or(Vec3::Y)
        } else {
            (line[i + 1] - line[i - 1]).normalize_or(Vec3::Y)
        };
        if i > 0 {
            along += line[i].distance(line[i - 1]);
        }
        normal = (normal - tangent * normal.dot(tangent)).normalize_or(tangent.any_orthonormal_vector());
        let binormal = tangent.cross(normal);
        for side in 0..=SIDES {
            let angle = side as f32 / SIDES as f32 * std::f32::consts::TAU;
            let n = normal * angle.cos() + binormal * angle.sin();
            positions.push((line[i] + n * radius).to_array());
            normals.push(n.to_array());
            uvs.push([side as f32 / SIDES as f32, along / circumference]);
        }
    }
    let mut indices = Vec::with_capacity(rings.saturating_sub(1) * SIDES * 6);
    let row = (SIDES + 1) as u32;
    for i in 0..rings.saturating_sub(1) as u32 {
        for side in 0..SIDES as u32 {
            let (a, b) = (i * row + side, i * row + side + 1);
            let (c, d) = (a + row, b + row);
            indices.extend([a, c, b, b, c, d]);
        }
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

/// A zipline: the wire and the shooter's stand, as children of the rope entity (they go
/// when it goes).
fn draw_zipline(
    add: On<Add, Rope>,
    mut commands: Commands,
    ropes: Query<&Rope>,
    assets: Option<Res<RopeAssets>>,
) {
    let (Ok(rope), Some(assets)) = (ropes.get(add.entity), assets) else {
        return;
    };
    if rope.kind != RopeKind::Zipline {
        return;
    }
    // The rope entity carries the climbable part's transform: pieces are placed in world
    // space and parented through its inverse.
    let parent = rope.transform();
    let piece = |commands: &mut Commands,
                 from: Vec3,
                 to: Vec3,
                 radius: f32,
                 material: &Handle<StandardMaterial>| {
        let along = to - from;
        let world = Transform::from_translation((from + to) * 0.5)
            .with_rotation(Quat::from_rotation_arc(
                Vec3::Y,
                along.normalize_or(Vec3::Y),
            ))
            .with_scale(Vec3::new(radius, along.length().max(0.01), radius));
        let local = Transform::from_matrix(parent.to_matrix().inverse() * world.to_matrix());
        commands.spawn((
            Mesh3d(assets.cylinder.clone()),
            MeshMaterial3d(material.clone()),
            local,
            ChildOf(add.entity),
        ));
    };
    commands.entity(add.entity).insert(Visibility::default());
    piece(&mut commands, rope.top, rope.end, 0.01, &assets.metal);
    piece(
        &mut commands,
        rope.top - Vec3::Y * ZIPLINE_STAND,
        rope.top,
        0.04,
        &assets.metal,
    );
}
