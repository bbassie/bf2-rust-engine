//! Marks left on surfaces: bullet holes and scorch marks (`effects/decals.ron`), quads laid
//! onto the surface at the hit. Two capped pools, small and large marks; the oldest go first.

use std::collections::{HashMap, VecDeque};

use bevy::{light::NotShadowCaster, math::Affine2, prelude::*};
use game_data::DecalTable;
use game_shared::level::LevelEntity;

/// Leaves a mark where a bullet or blast hit a surface.
#[derive(Message, Clone, Debug)]
pub struct SpawnDecal {
    /// A decal of `effects/decals.ron`, e.g. `decal_s_wood`.
    pub name: String,
    pub position: Vec3,
    pub normal: Vec3,
    /// The mark belongs to this entity (a destroyable part) and goes when it goes.
    pub parent: Option<Entity>,
}

/// Bullet holes at once.
const MAX_SMALL: usize = 300;
/// Scorch marks at once.
const MAX_LARGE: usize = 40;
/// Marks wider than this are large.
const LARGE: f32 = 0.5;

/// The decals and the marks lying around.
#[derive(Resource)]
pub struct Decals {
    table: DecalTable,
    quad: Handle<Mesh>,
    /// Per decal: a material for each variation, made when the level loads.
    materials: HashMap<String, Vec<Handle<StandardMaterial>>>,
    small: VecDeque<Entity>,
    large: VecDeque<Entity>,
}

impl Decals {
    pub(super) fn new(table: DecalTable, meshes: &mut Assets<Mesh>) -> Self {
        Self {
            table,
            quad: meshes.add(Rectangle::new(1.0, 1.0)),
            materials: HashMap::new(),
            small: VecDeque::new(),
            large: VecDeque::new(),
        }
    }

    /// The mark a projectile (or explosion) of material `attacker` leaves on `surface`.
    pub fn decal(&self, attacker: u32, surface: u32) -> Option<&str> {
        self.table.decal(attacker, surface).map(|(name, _)| name)
    }
}

/// Makes the decals' materials (textures load with the level, not on the first hit).
pub(super) fn prepare_decals(
    mut decals: ResMut<Decals>,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !decals.materials.is_empty() {
        return;
    }
    let mut prepared = HashMap::new();
    for (name, desc) in &decals.table.decals {
        let texture: Handle<Image> = asset_server.load(format!("imported://{}", desc.texture));
        let rects: Vec<Affine2> = match desc.frames {
            Some(frames) => (0..frames.count.max(1))
                .map(|frame| {
                    let columns = frames.columns.max(1);
                    let [w, h] = frames.frame_size;
                    let offset = Vec2::new((frame % columns) as f32 * w, (frame / columns) as f32 * h);
                    Affine2::from_scale_angle_translation(Vec2::new(w, h), 0.0, offset)
                })
                .collect(),
            None => vec![Affine2::IDENTITY],
        };
        let [r, g, b] = desc.color;
        let variants = rects
            .into_iter()
            .map(|uv_transform| {
                materials.add(StandardMaterial {
                    base_color: Color::linear_rgb(r, g, b),
                    base_color_texture: Some(texture.clone()),
                    uv_transform,
                    alpha_mode: AlphaMode::Blend,
                    perceptual_roughness: 0.9,
                    reflectance: 0.2,
                    depth_bias: 100.0,
                    ..default()
                })
            })
            .collect();
        prepared.insert(name.clone(), variants);
    }
    decals.materials = prepared;
}

/// A new level: marks of the old one are gone with its entities.
pub(super) fn clear_decals(mut decals: ResMut<Decals>) {
    decals.small.clear();
    decals.large.clear();
}

pub(super) fn spawn_decals(
    mut commands: Commands,
    mut requests: MessageReader<SpawnDecal>,
    mut decals: ResMut<Decals>,
    parents: Query<&GlobalTransform>,
) {
    for request in requests.read() {
        let Some(desc) = decals.table.decals.get(&request.name) else {
            continue;
        };
        let Some(variants) = decals.materials.get(&request.name).filter(|v| !v.is_empty()) else {
            continue;
        };
        trace!("decal {} at {}", request.name, request.position);
        let material = variants[fastrand::usize(..variants.len())].clone();
        let size = desc.size[0] + (desc.size[1] - desc.size[0]) * fastrand::f32();
        let normal = request.normal.normalize_or(Vec3::Y);
        let turn = (fastrand::f32() * 2.0 - 1.0) * desc.rotation.to_radians();
        // Lifted off the surface a little more the larger it is (uneven ground).
        let lift = if size > LARGE { 0.04 } else { 0.006 };
        let world = Transform {
            translation: request.position + normal * lift,
            rotation: Quat::from_rotation_arc(Vec3::Z, normal) * Quat::from_rotation_z(turn),
            scale: Vec3::new(size, size, 1.0),
        };
        let mut entity = commands.spawn((
            Mesh3d(decals.quad.clone()),
            MeshMaterial3d(material),
            NotShadowCaster,
            LevelEntity,
        ));
        match request.parent.and_then(|p| Some((p, parents.get(p).ok()?))) {
            Some((parent, parent_transform)) => {
                let local = parent_transform.affine().inverse() * world.compute_affine();
                entity.insert((Transform::from_matrix(Mat4::from(local)), ChildOf(parent)));
            }
            None => {
                entity.insert(world);
            }
        }
        let id = entity.id();
        let (pool, cap) = if size > LARGE { (&mut decals.large, MAX_LARGE) } else { (&mut decals.small, MAX_SMALL) };
        pool.push_back(id);
        while pool.len() > cap {
            if let Some(old) = pool.pop_front() {
                commands.entity(old).try_despawn();
            }
        }
    }
}
