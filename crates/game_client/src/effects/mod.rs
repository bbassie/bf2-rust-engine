//! Particle effects: sprite emitters, light flashes, flash meshes, debris and sounds, as the
//! importer converted them from BF2 (`imported/effects/<name>.ron`, see `game_data::effect`).
//!
//! How to play one (effects are cosmetic, they never affect gameplay):
//! - on the client, write a [`SpawnEffect`] message:
//!   `effects.write(SpawnEffect::new("e_exp_grenade", point).with_up(normal))`;
//! - for something that trails or burns while it exists (rockets, wrecks), insert an
//!   [`EffectEmitter`] on its entity; the effect follows it and stops when it goes away;
//! - on the server, send `ToClients<PlayEffect>` (`game_shared::effects`) to clients that
//!   can't work the effect out from other messages.
//!
//! [`EffectLibrary`] also answers which effect a weapon's muzzle, a detonation or a bullet
//! hitting some surface makes (see [`impacts`]).
//!
//! Particles are simulated on the CPU in one resource; the GPU expands them into quads from
//! a storage buffer, one draw per texture (BF2 packs nearly all sprites into one atlas),
//! sorted far to near and blended premultiplied so alpha and additive sprites share a draw.
//! Where the camera has a depth prepass (with SSAO) sprites fade out softly where they meet
//! geometry.

use std::{
    collections::HashMap,
    f32::consts::TAU,
    path::PathBuf,
    sync::Arc,
};

use avian3d::prelude::*;
use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::RenderLayers,
    light::NotShadowCaster,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};
use game_data::{Blend, EffectDesc, EmitShape, EmitterDesc, Facing, ImpactTable, WeaponEffectTable};
use game_shared::{
    config::GamePaths,
    effects::PlayEffect,
    level::{LevelEntity, LoadedLevel},
    physics::GameLayer,
    statics::StaticMesh,
};

use crate::{audio::PlaySound, camera::PlayerCamera, render::viewmodel::VIEW_MODEL_LAYER};

pub mod impacts;
mod render;

pub use impacts::SurfaceQuery;

pub struct EffectsPlugin;

impl Plugin for EffectsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(render::ParticleRenderPlugin)
            .add_message::<SpawnEffect>()
            .init_resource::<EffectWorld>()
            .init_resource::<impacts::Surfaces>()
            .init_resource::<EffectStats>()
            .add_systems(Startup, (load_library, spawn_light_pool))
            .add_systems(
                Update,
                (
                    (clear_effects, impacts::load_surfaces, preload_level_effects, set_particle_light)
                        .run_if(resource_exists_and_changed::<LoadedLevel>),
                    clear_effects.run_if(resource_removed::<LoadedLevel>),
                    receive_server_effects,
                    (update_flashes, update_debris),
                ),
            )
            .add_systems(
                PostUpdate,
                (spawn_effects, follow_emitters, simulate, update_lights, render::upload)
                    .chain()
                    .after(crate::camera::CameraSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Plays an effect once.
#[derive(Message, Clone, Debug)]
pub struct SpawnEffect {
    /// Effect name: a file in `imported/effects/`, e.g. `e_exp_grenade`.
    pub name: String,
    pub position: Vec3,
    /// Turns the effect's frame: +Y up (impacts: the surface normal), -Z forward (muzzle
    /// flashes: the barrel).
    pub rotation: Quat,
    /// Shows the first-person parts instead of the third-person ones, drawn with the view
    /// model and following the camera (our own muzzle flash).
    pub first_person: bool,
    /// Keep emitting for this many seconds (smoke grenades) instead of the effect's own
    /// length: looping emitters loop, others fire again after 40% of their particle life.
    pub duration: Option<f32>,
}

impl SpawnEffect {
    pub fn new(name: impl Into<String>, position: Vec3) -> Self {
        Self {
            name: name.into(),
            position,
            rotation: Quat::IDENTITY,
            first_person: false,
            duration: None,
        }
    }

    /// Turned like the object it belongs to (destroyed objects).
    #[allow(dead_code)]
    pub fn with_rotation(mut self, rotation: Quat) -> Self {
        self.rotation = rotation;
        self
    }

    /// +Y along `up` (a surface normal).
    pub fn with_up(mut self, up: Vec3) -> Self {
        self.rotation = Quat::from_rotation_arc(Vec3::Y, up.normalize_or(Vec3::Y));
        self
    }

    /// -Z along `forward`, +Y as close to up as it gets.
    pub fn with_forward(mut self, forward: Vec3) -> Self {
        let forward = forward.normalize_or(Vec3::NEG_Z);
        let up = if forward.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
        self.rotation = Transform::IDENTITY.looking_to(forward, up).rotation;
        self
    }

    pub fn first_person(mut self) -> Self {
        self.first_person = true;
        self
    }

    pub fn lasting(mut self, seconds: f32) -> Self {
        self.duration = Some(seconds);
        self
    }
}

/// Plays `count` copies of an effect at random places up to 20 m around `center`
/// (horizontally; the first right at it), emitting for `seconds` (0: the effect's own
/// length). For tests and measurements.
pub fn spawn_around(effects: &mut MessageWriter<SpawnEffect>, name: &str, center: Vec3, count: u32, seconds: f32) {
    for index in 0..count {
        let offset = if index == 0 { Vec3::ZERO } else { Vec3::new(signed(), 0.0, signed()) * 20.0 };
        let mut effect = SpawnEffect::new(name, center + offset);
        if seconds > 0.0 {
            effect = effect.lasting(seconds);
        }
        effects.write(effect);
    }
}

/// Plays a looping effect that follows this entity (rocket trails, burning wrecks) until
/// the component or the entity goes away; its particles then fade out on their own.
#[derive(Component, Clone, Debug)]
pub struct EffectEmitter(pub String);

/// Numbers for the HUD and measurements.
#[derive(Resource, Default, Debug)]
pub struct EffectStats {
    pub effects: usize,
    pub particles: usize,
    /// CPU time of the last simulation step.
    pub simulate_ms: f32,
    since_log: f32,
}

/// Particles alive at most; beyond this, emitters skip particles.
const MAX_PARTICLES: usize = 60_000;
/// Point lights for flashes at once; the brightest near the camera win.
const MAX_LIGHTS: usize = 8;
/// Effects this far from the camera aren't started.
const MAX_DISTANCE: f32 = 600.0;
/// Sustained one-shot emitters fire again after this fraction of their particles' life.
const SUSTAIN_PERIOD: f32 = 0.4;
/// Point light lumens per unit of BF2 light color and square meter of radius.
const LIGHT_LUMENS: f32 = 25_000.0;

/// Effect descriptions with their textures and meshes, loaded when first used.
#[derive(Resource)]
pub struct EffectLibrary {
    dir: PathBuf,
    effects: HashMap<String, Option<Arc<Prepared>>>,
    textures: HashMap<String, Handle<Image>>,
    pub impacts: ImpactTable,
    pub weapons: WeaponEffectTable,
}

/// An effect ready to play.
struct Prepared {
    desc: EffectDesc,
    /// Per emitter.
    textures: Vec<Handle<Image>>,
    /// Per flash mesh.
    flashes: Vec<(Handle<Mesh>, Handle<StandardMaterial>)>,
    /// Longest light, flash or particle life: how long the effect can last.
    lights_until: f32,
}

impl EffectLibrary {
    fn prepare(
        &mut self,
        name: &str,
        asset_server: &AssetServer,
        meshes: &mut Assets<Mesh>,
        materials: &mut Assets<StandardMaterial>,
    ) -> Option<Arc<Prepared>> {
        let name = name.to_ascii_lowercase();
        if let Some(prepared) = self.effects.get(&name) {
            return prepared.clone();
        }
        let path = self.dir.join(format!("{name}.ron"));
        let prepared = game_data::read_ron::<EffectDesc>(&path)
            .map_err(|err| {
                if path.exists() {
                    warn!("effect {name}: {err}");
                }
            })
            .ok()
            .map(|desc| {
                let mut texture = |path: &str| {
                    self.textures
                        .entry(path.to_string())
                        .or_insert_with(|| asset_server.load(format!("imported://{path}")))
                        .clone()
                };
                let textures = desc.emitters.iter().map(|e| texture(&e.texture)).collect();
                let flashes = desc
                    .meshes
                    .iter()
                    .map(|flash| {
                        let normals = vec![[0.0, 0.0, 1.0]; flash.positions.len()];
                        let mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
                            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, flash.positions.clone())
                            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
                            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, flash.uvs.clone())
                            .with_inserted_indices(Indices::U32(flash.indices.clone()));
                        let material = StandardMaterial {
                            base_color: Color::linear_rgb(1.0, 1.0, 1.0),
                            base_color_texture: Some(texture(&flash.texture)),
                            unlit: true,
                            alpha_mode: AlphaMode::Add,
                            cull_mode: None,
                            ..default()
                        };
                        (meshes.add(mesh), materials.add(material))
                    })
                    .collect();
                let lights_until = desc
                    .lights
                    .iter()
                    .map(|l| l.life)
                    .chain(desc.meshes.iter().map(|m| m.life))
                    .fold(0.0, f32::max);
                Arc::new(Prepared {
                    desc,
                    textures,
                    flashes,
                    lights_until,
                })
            });
        self.effects.insert(name, prepared.clone());
        prepared
    }

    /// A weapon's muzzle flash, and where the muzzle is on the weapon model.
    pub fn muzzle(&self, weapon: &str) -> Option<(&str, Vec3)> {
        let effects = self.weapons.weapons.get(weapon)?;
        Some((effects.muzzle.as_deref()?, Vec3::from_array(effects.muzzle_offset)))
    }

    /// Where a weapon's projectile detonates (grenades, rockets).
    pub fn detonation(&self, weapon: &str) -> Option<&str> {
        self.weapons.weapons.get(weapon)?.detonation.as_deref()
    }

    /// The effect of a projectile of material `projectile` hitting `surface`.
    pub fn impact(&self, projectile: u32, surface: u32) -> Option<&str> {
        self.impacts.effect(projectile, surface).or_else(|| {
            // Cells without an effect: the closest common surface.
            let fallback = match surface {
                impacts::HUMAN_BODY | impacts::HUMAN_HEAD | impacts::HUMAN_LIMBS => impacts::HUMAN_BODY,
                2..=9 | 58 | 85 | 95 => impacts::DIRT,
                _ => impacts::CONCRETE,
            };
            self.impacts.effect(projectile, fallback)
        })
    }
}

fn load_library(mut commands: Commands, paths: Res<GamePaths>) {
    let dir = paths.imported.join("effects");
    let impacts: ImpactTable = game_data::read_ron(dir.join("impacts.ron")).unwrap_or_default();
    let weapons: WeaponEffectTable = game_data::read_ron(dir.join("weapons.ron")).unwrap_or_default();
    if impacts.effects.is_empty() {
        info!("no imported effects in {}", dir.display());
    }
    commands.insert_resource(EffectLibrary {
        dir,
        effects: HashMap::new(),
        textures: HashMap::new(),
        impacts,
        weapons,
    });
}

/// Loads the effects of the weapons and of bullet hits before the first shot.
fn preload_level_effects(
    mut library: ResMut<EffectLibrary>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut names: Vec<String> = library
        .weapons
        .weapons
        .values()
        .flat_map(|w| w.muzzle.iter().chain(&w.detonation).cloned())
        .collect();
    names.extend(
        library
            .impacts
            .effects
            .iter()
            .filter(|((projectile, _), _)| *projectile == impacts::BULLET)
            .map(|(_, name)| name.clone()),
    );
    names.sort();
    names.dedup();
    for name in names {
        library.prepare(&name, &asset_server, &mut meshes, &mut materials);
    }
}

/// Brightness of sprites that are lit by the scene (not glowing ones), from the level's sun
/// and ambient light.
#[derive(Resource, Clone, Copy)]
struct ParticleLight(Vec3);

impl Default for ParticleLight {
    fn default() -> Self {
        Self(Vec3::ONE)
    }
}

fn set_particle_light(mut commands: Commands, level: Res<LoadedLevel>) {
    let env = &level.desc.environment;
    // Sprites are lit from all sides and shadow themselves: darker than sunlit ground.
    let light = Vec3::from_array(env.sun_color) * 0.5 + Vec3::from_array(env.ambient_color) * 0.4;
    commands.insert_resource(ParticleLight(light.clamp(Vec3::splat(0.1), Vec3::ONE)));
}

/// A new level, or none: what was running belongs to the old one.
fn clear_effects(mut world: ResMut<EffectWorld>) {
    world.instances.clear();
    world.particles = 0;
}

fn receive_server_effects(mut received: MessageReader<PlayEffect>, mut effects: MessageWriter<SpawnEffect>) {
    for effect in received.read() {
        let mut spawn = SpawnEffect::new(effect.name.clone(), effect.position).with_up(effect.up);
        if effect.duration > 0.0 {
            spawn = spawn.lasting(effect.duration);
        }
        effects.write(spawn);
    }
}

/// All running effects.
#[derive(Resource, Default)]
struct EffectWorld {
    instances: Vec<Instance>,
    particles: usize,
}

struct Instance {
    effect: Arc<Prepared>,
    position: Vec3,
    rotation: Quat,
    first_person: bool,
    /// First person: the placement relative to the camera, which it follows.
    camera_local: Option<Transform>,
    /// Follows this entity while it has an [`EffectEmitter`].
    follow: Option<Entity>,
    age: f32,
    /// Emitters keep going until this age.
    sustain: f32,
    emitters: Vec<EmitterState>,
    particles: Vec<Particle>,
}

struct EmitterState {
    /// Instance age the current emission cycle starts at.
    start: f32,
    emitted: u32,
    done: bool,
}

struct Particle {
    position: Vec3,
    velocity: Vec3,
    age: f32,
    life: f32,
    size: f32,
    rotation: f32,
    spin: f32,
    brightness: f32,
    frame: f32,
    emitter: u16,
}

impl Instance {
    fn new(effect: Arc<Prepared>, position: Vec3, rotation: Quat, first_person: bool, sustain: f32) -> Self {
        let emitters = effect
            .desc
            .emitters
            .iter()
            .map(|e| EmitterState {
                start: e.delay,
                emitted: 0,
                done: !e.views.shows(first_person),
            })
            .collect();
        Self {
            effect,
            position,
            rotation,
            first_person,
            camera_local: None,
            follow: None,
            age: 0.0,
            sustain,
            emitters,
            particles: Vec::new(),
        }
    }

    fn layer(&self) -> usize {
        if self.first_person { VIEW_MODEL_LAYER } else { 0 }
    }

    fn finished(&self) -> bool {
        self.particles.is_empty()
            && self.follow.is_none()
            && self.emitters.iter().all(|e| e.done)
            && self.age >= self.effect.lights_until
    }
}

fn random(range: [f32; 2]) -> f32 {
    range[0] + (range[1] - range[0]) * fastrand::f32()
}

fn signed() -> f32 {
    fastrand::f32() * 2.0 - 1.0
}

/// A random unit vector.
fn random_direction() -> Vec3 {
    let z = signed();
    let angle = fastrand::f32() * TAU;
    let r = (1.0 - z * z).max(0.0).sqrt();
    Vec3::new(r * angle.cos(), z, r * angle.sin())
}

/// A random unit vector perpendicular to `axis`.
fn random_perpendicular(axis: Vec3) -> Vec3 {
    let axis = axis.normalize_or(Vec3::Y);
    let (a, b) = axis.any_orthonormal_pair();
    let angle = fastrand::f32() * TAU;
    a * angle.cos() + b * angle.sin()
}

#[allow(clippy::too_many_arguments)]
fn spawn_effects(
    mut commands: Commands,
    mut requests: MessageReader<SpawnEffect>,
    library: Option<ResMut<EffectLibrary>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut world: ResMut<EffectWorld>,
    camera: Query<(Entity, &Transform), With<PlayerCamera>>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let Some(mut library) = library else {
        requests.clear();
        return;
    };
    let camera = camera.single().ok();
    for request in requests.read() {
        if let Some((_, camera)) = camera
            && !request.first_person
            && camera.translation.distance(request.position) > MAX_DISTANCE
        {
            continue;
        }
        let Some(effect) = library.prepare(&request.name, &asset_server, &mut meshes, &mut materials) else {
            continue;
        };
        trace!("effect {} at {}", request.name, request.position);
        let sustain = request.duration.unwrap_or(0.0);
        let mut instance = Instance::new(effect.clone(), request.position, request.rotation, request.first_person, sustain);
        let camera_local = match (request.first_person, camera) {
            (true, Some((_, camera))) => {
                let world = Transform::from_translation(request.position).with_rotation(request.rotation);
                Some(Transform::from_matrix(camera.to_matrix().inverse() * world.to_matrix()))
            }
            _ => None,
        };
        instance.camera_local = camera_local;

        let layer = RenderLayers::layer(instance.layer());
        let desc = &effect.desc;
        for (flash, (mesh, material)) in desc.meshes.iter().zip(&effect.flashes) {
            if !flash.views.shows(request.first_person) {
                continue;
            }
            let roll = if flash.roll_step > 0.0 {
                let steps = (360.0 / flash.roll_step).round().max(1.0) as u32;
                (fastrand::u32(..steps) as f32 * flash.roll_step).to_radians()
            } else {
                0.0
            };
            let local = Transform::from_translation(Vec3::from_array(flash.position))
                .with_rotation(Quat::from_rotation_z(roll))
                .with_scale(Vec3::splat(random(flash.scale)));
            let mut entity = commands.spawn((
                Mesh3d(mesh.clone()),
                MeshMaterial3d(material.clone()),
                NotShadowCaster,
                layer.clone(),
                Flash { age: 0.0, life: flash.life.max(0.03) },
            ));
            match (camera_local, camera) {
                (Some(camera_local), Some((camera_entity, _))) => {
                    entity.insert((camera_local * local, ChildOf(camera_entity)));
                }
                _ => {
                    let placed = Transform::from_translation(request.position).with_rotation(request.rotation);
                    entity.insert(placed * local);
                }
            }
        }
        for piece in &desc.debris {
            let sample = |spread: &[game_data::Spread; 3]| {
                request.rotation
                    * Vec3::from_array(std::array::from_fn(|i| spread[i].sample(fastrand::f32(), fastrand::bool())))
            };
            commands.spawn((
                LevelEntity,
                Transform::from_translation(request.position + request.rotation * Vec3::from_array(piece.position))
                    .with_rotation(request.rotation),
                StaticMesh {
                    path: piece.mesh.clone(),
                    index: piece.mesh_index,
                },
                Debris {
                    velocity: sample(&piece.velocity),
                    spin: sample(&piece.spin),
                    life: piece.life.sample(fastrand::f32(), false).max(0.2),
                    age: 0.0,
                },
            ));
        }
        for sound in desc.sounds.iter().filter(|s| s.views.shows(request.first_person) && !s.files.is_empty()) {
            sounds.write(PlaySound::at(sound, request.position).reason("effect"));
        }
        world.instances.push(instance);
    }
}

/// Where an emitter's entity is now. Root entities by their `Transform`: their
/// `GlobalTransform` lags a frame (and is the origin in the frame they appear).
type EmitterPlacement = (&'static Transform, &'static GlobalTransform, Has<ChildOf>);

fn placement((transform, global, has_parent): (&Transform, &GlobalTransform, bool)) -> (Vec3, Quat) {
    if has_parent {
        let (_, rotation, position) = global.to_scale_rotation_translation();
        (position, rotation)
    } else {
        (transform.translation, transform.rotation)
    }
}

/// Starts the effect of new [`EffectEmitter`]s.
fn follow_emitters(
    added: Query<(Entity, &EffectEmitter, EmitterPlacement), Added<EffectEmitter>>,
    library: Option<ResMut<EffectLibrary>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut world: ResMut<EffectWorld>,
) {
    let Some(mut library) = library else {
        return;
    };
    for (entity, emitter, placed) in &added {
        let Some(effect) = library.prepare(&emitter.0, &asset_server, &mut meshes, &mut materials) else {
            continue;
        };
        let (position, rotation) = placement(placed);
        let mut instance = Instance::new(effect, position, rotation, false, f32::INFINITY);
        instance.follow = Some(entity);
        world.instances.push(instance);
    }
}

#[allow(clippy::too_many_arguments)]
fn simulate(
    time: Res<Time>,
    mut world: ResMut<EffectWorld>,
    mut batches: ResMut<render::Batches>,
    mut stats: ResMut<EffectStats>,
    light: Option<Res<ParticleLight>>,
    camera: Query<&Transform, With<PlayerCamera>>,
    followed: Query<EmitterPlacement, With<EffectEmitter>>,
) {
    let started = std::time::Instant::now();
    let dt = time.delta_secs().min(0.1);
    let light = light.map_or(Vec3::ONE, |l| l.0);
    let camera = camera.single().copied().unwrap_or_default();
    let camera_forward = camera.forward().as_vec3();
    let mut budget = MAX_PARTICLES.saturating_sub(world.particles);
    let mut alive = 0;
    for instance in &mut world.instances {
        instance.age += dt;
        if let Some(local) = instance.camera_local {
            let placed = camera * local;
            instance.position = placed.translation;
            instance.rotation = placed.rotation;
        }
        if let Some(entity) = instance.follow {
            match followed.get(entity) {
                Ok(placed) => (instance.position, instance.rotation) = placement(placed),
                Err(_) => {
                    instance.follow = None;
                    instance.sustain = 0.0;
                }
            }
        }
        // New particles already have the age they'd have by now.
        integrate(instance, dt);
        emit(instance, &mut budget);
        alive += instance.particles.len();
        draw(instance, &mut batches, light, camera.translation, camera_forward);
    }
    world.instances.retain(|i| !i.finished());
    world.particles = alive;
    stats.effects = world.instances.len();
    stats.particles = alive;
    stats.simulate_ms = started.elapsed().as_secs_f32() * 1000.0;
    stats.since_log += dt;
    if stats.since_log > 2.0 && alive > 0 {
        stats.since_log = 0.0;
        debug!(
            "effects: {} running, {alive} particles, {} drawn; CPU {:.2} ms simulating, {:.2} ms uploading",
            stats.effects,
            batches.particle_count(),
            stats.simulate_ms,
            batches.upload_ms,
        );
    }
}

/// Emits the particles each emitter owes by now.
fn emit(instance: &mut Instance, budget: &mut usize) {
    let effect = instance.effect.clone();
    for (index, desc) in effect.desc.emitters.iter().enumerate() {
        loop {
            let state = &mut instance.emitters[index];
            let t = instance.age - state.start;
            if state.done || t < 0.0 {
                break;
            }
            let window = desc.duration.max(0.0);
            let total = ((desc.rate * window).round() as u32).max(1);
            let burst = (desc.rate * desc.burst.min(window)) as u32 + 1;
            let due = if window <= 0.0 { total } else { (burst + (desc.rate * t) as u32).min(total) };
            while state.emitted < due {
                let emitted_at = if state.emitted < burst { 0.0 } else { (state.emitted - burst) as f32 / desc.rate };
                let progress = state.emitted as f32 / total as f32;
                state.emitted += 1;
                if *budget == 0 {
                    continue;
                }
                *budget -= 1;
                let age = (t - emitted_at).max(0.0);
                instance.particles.push(spawn_particle(desc, index, progress, age, instance.position, instance.rotation));
            }
            let state = &mut instance.emitters[index];
            if t < window || state.emitted < total {
                break;
            }
            // The cycle is over: another one while sustained, else stop.
            if instance.age < instance.sustain {
                let period = if desc.looping { window } else { window.max(desc.life[1] * SUSTAIN_PERIOD) };
                state.start += period.max(0.02);
                state.emitted = 0;
            } else {
                state.done = true;
            }
        }
    }
}

fn spawn_particle(desc: &EmitterDesc, index: usize, progress: f32, age: f32, position: Vec3, rotation: Quat) -> Particle {
    let extent = Vec3::from_array(desc.extent);
    let direction = Vec3::from_array(desc.direction);
    let mut offset = Vec3::new(signed(), signed(), signed()) * extent;
    let base = match desc.shape {
        EmitShape::Box => direction,
        EmitShape::Ring => {
            let radial = random_perpendicular(direction);
            offset = radial * fastrand::f32() * extent.x.max(extent.z);
            radial
        }
        EmitShape::Sphere => offset.try_normalize().unwrap_or_else(random_direction),
    };
    let spread = Vec3::from_array(desc.spread);
    let direction = if spread.min_element() >= 180.0 {
        random_direction()
    } else if spread == Vec3::ZERO {
        base
    } else {
        let angles = spread * Vec3::new(signed(), signed(), signed());
        Quat::from_euler(
            EulerRot::XYZ,
            angles.x.to_radians(),
            angles.y.to_radians(),
            angles.z.to_radians(),
        ) * base
    };
    let speed = random(desc.speed) * desc.speed_curve.at(progress);
    let life = random(desc.life).max(0.01);
    let frame = match desc.frames {
        Some(frames) if frames.random_start => fastrand::u32(..frames.count.max(1)) as f32,
        _ => 0.0,
    };
    Particle {
        position: position + rotation * (Vec3::from_array(desc.position) + offset),
        velocity: rotation * direction * speed,
        // Shown at least once, however short-lived.
        age: age.min(life * 0.5),
        life,
        size: random(desc.size),
        rotation: (signed() * desc.rotation).to_radians(),
        spin: random(desc.spin),
        brightness: 1.0 - fastrand::f32() * desc.brightness_jitter,
        frame,
        emitter: index as u16,
    }
}

fn integrate(instance: &mut Instance, dt: f32) {
    let emitters = &instance.effect.desc.emitters;
    instance.particles.retain_mut(|p| {
        p.age += dt;
        if p.age >= p.life {
            return false;
        }
        let desc = &emitters[p.emitter as usize];
        let t = p.age / p.life;
        p.velocity.y += desc.gravity * desc.gravity_curve.at(t) * dt;
        let drag = desc.drag * desc.drag_curve.at(t).max(0.0);
        if drag > 0.0 {
            // dv/dt = -drag |v| v, stable for any step.
            p.velocity /= 1.0 + drag * p.velocity.length() * dt;
        }
        p.position += p.velocity * dt;
        p.rotation += p.spin * desc.spin_curve.at(t) * dt;
        true
    });
}

/// Queues the instance's visible particles for drawing.
fn draw(instance: &Instance, batches: &mut render::Batches, light: Vec3, camera: Vec3, forward: Vec3) {
    let effect = &instance.effect;
    let layer = instance.layer();
    let up = instance.rotation * Vec3::Y;
    for p in &instance.particles {
        let desc = &effect.desc.emitters[p.emitter as usize];
        let t = p.age / p.life;
        let size = p.size * desc.size_curve.at(t);
        let opacity = desc.opacity_curve.at(t).clamp(0.0, 1.0);
        if size <= 0.001 || opacity <= 0.004 {
            continue;
        }
        let depth = (p.position - camera).dot(forward);
        if depth < -size {
            continue;
        }
        let blend = desc.color_curve.at(t).clamp(0.0, 1.0);
        let [a, b] = desc.colors.map(Vec3::from_array);
        let additive = desc.blend == Blend::Additive;
        let mut color = a.lerp(b, blend) * p.brightness;
        if !additive {
            color *= light;
        }
        let half = size * 0.5;
        let (mode, axis, length) = match desc.facing {
            Facing::Camera => (0.0, Vec3::Y, 1.0),
            Facing::Velocity => {
                let speed = p.velocity.length();
                let axis = if speed > 1e-4 { p.velocity / speed } else { up };
                (1.0, axis, 1.0 + speed * desc.stretch / half.max(1e-3) * 0.5)
            }
            Facing::Horizontal => (2.0, up, 1.0),
        };
        // Sprites near a surface (most start at one) would fade away with a larger distance.
        let soft = if desc.facing == Facing::Velocity { 0.0 } else { (half * 0.25).min(1.0) };
        batches.push(
            &effect.textures[p.emitter as usize],
            layer,
            depth,
            render::GpuParticle {
                position: p.position,
                size: half,
                axis,
                rotation: p.rotation,
                color: color.extend(opacity),
                uv: frame_rect(desc, p),
                params: Vec4::new(mode, length, if additive { 1.0 } else { 0.0 }, soft),
            },
        );
    }
}

fn frame_rect(desc: &EmitterDesc, p: &Particle) -> Vec4 {
    let [x, y, w, h] = desc.uv_rect;
    let Some(frames) = desc.frames else {
        return Vec4::new(x, y, w, h);
    };
    let count = frames.count.max(1);
    let mut frame = (p.frame + p.age * frames.fps) as u32;
    frame = if frames.once { frame.min(count - 1) } else { frame % count };
    let columns = frames.columns.max(1);
    let (column, row) = (frame % columns, frame / columns);
    let [fw, fh] = frames.frame_size;
    Vec4::new(
        x + column as f32 * fw * w,
        y + row as f32 * fh * h,
        fw * w,
        fh * h,
    )
}

/// The point lights flashes borrow.
#[derive(Component)]
struct LightSlot;

fn spawn_light_pool(mut commands: Commands) {
    for _ in 0..MAX_LIGHTS {
        commands.spawn((
            LightSlot,
            PointLight {
                intensity: 0.0,
                shadow_maps_enabled: false,
                ..default()
            },
            Transform::default(),
            Visibility::Hidden,
        ));
    }
}

/// Gives the point lights to the brightest flashes near the camera.
fn update_lights(
    world: Res<EffectWorld>,
    camera: Query<&Transform, (With<PlayerCamera>, Without<LightSlot>)>,
    mut slots: Query<(&mut PointLight, &mut Transform, &mut Visibility), With<LightSlot>>,
) {
    let camera = camera.single().map_or(Vec3::ZERO, |c| c.translation);
    let mut lights: Vec<(f32, Vec3, Vec3, f32)> = Vec::new();
    for instance in &world.instances {
        for light in &instance.effect.desc.lights {
            if !light.views.shows(instance.first_person) || instance.age >= light.life {
                continue;
            }
            let fade = 1.0 - instance.age / light.life;
            let color = Vec3::from_array(light.color) * fade * fade;
            let position = instance.position + instance.rotation * Vec3::from_array(light.position);
            let strength = color.max_element() * light.radius * light.radius;
            let score = strength / (1.0 + position.distance_squared(camera));
            lights.push((score, position, color, light.radius));
        }
    }
    lights.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let mut lights = lights.into_iter();
    for (mut point, mut transform, mut visibility) in &mut slots {
        match lights.next() {
            Some((_, position, color, radius)) => {
                let brightness = color.max_element().max(1e-3);
                point.color = Color::linear_rgb(color.x / brightness, color.y / brightness, color.z / brightness);
                point.intensity = LIGHT_LUMENS * brightness * radius * radius;
                point.range = radius * 2.0;
                transform.translation = position;
                visibility.set_if_neq(Visibility::Visible);
            }
            None => {
                if *visibility != Visibility::Hidden {
                    point.intensity = 0.0;
                    *visibility = Visibility::Hidden;
                }
            }
        }
    }
}

/// A flash mesh, shown for a moment.
#[derive(Component)]
struct Flash {
    age: f32,
    life: f32,
}

fn update_flashes(mut commands: Commands, time: Res<Time>, mut flashes: Query<(Entity, &mut Flash)>) {
    for (entity, mut flash) in &mut flashes {
        flash.age += time.delta_secs();
        if flash.age >= flash.life {
            commands.entity(entity).despawn();
        }
    }
}

/// A thrown mesh. It bounces off the world, comes to rest and shrinks away at the end.
#[derive(Component)]
struct Debris {
    velocity: Vec3,
    /// Angular velocity, world space.
    spin: Vec3,
    life: f32,
    age: f32,
}

fn update_debris(
    mut commands: Commands,
    time: Res<Time>,
    spatial: SpatialQuery,
    mut debris: Query<(Entity, &mut Debris, &mut Transform)>,
) {
    const SHRINK: f32 = 0.4;
    let dt = time.delta_secs();
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    for (entity, mut piece, mut transform) in &mut debris {
        piece.age += dt;
        if piece.age >= piece.life {
            commands.entity(entity).despawn();
            continue;
        }
        piece.velocity.y -= 9.81 * dt;
        let step = piece.velocity * dt;
        let hit = Dir3::new(step)
            .ok()
            .and_then(|dir| spatial.cast_ray(transform.translation, dir, step.length(), true, &filter));
        match hit {
            Some(hit) => {
                transform.translation += step.normalize() * hit.distance + hit.normal * 0.02;
                piece.velocity = piece.velocity.reflect(hit.normal) * 0.3;
                piece.spin *= 0.5;
            }
            None => transform.translation += step,
        }
        transform.rotation = (Quat::from_scaled_axis(piece.spin * dt) * transform.rotation).normalize();
        let shrink = ((piece.life - piece.age) / SHRINK).clamp(0.0, 1.0);
        transform.scale = Vec3::splat(shrink);
    }
}
