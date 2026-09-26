//! Level loading shared by client and server.
//!
//! When a [`MatchInfo`] appears (spawned by the server, or replicated to a client) the level
//! it names is loaded from the imported assets folder and its physics is spawned. The client
//! adds rendering on top by observing the entities spawned here.

use std::{path::PathBuf, sync::Arc};

use avian3d::prelude::*;
use bevy::prelude::*;
use game_data::{
    ControlPointDesc, GameModeDesc, LevelDesc, Placement, SpawnPointDesc, TerrainDesc,
};

use crate::{config::GamePaths, physics::GameLayer, protocol::MatchInfo};

/// Name of the built-in level that works without any imported assets.
pub const TEST_RANGE: &str = "test_range";

pub struct LevelPlugin;

impl Plugin for LevelPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(load_level_for_match);
    }
}

/// The currently loaded level.
#[derive(Resource)]
pub struct LoadedLevel {
    pub desc: LevelDesc,
    /// Folder the level was loaded from (`None` for built-in levels).
    pub dir: Option<PathBuf>,
    pub heightmap: Option<Arc<Heightmap>>,
}

impl LoadedLevel {
    /// The layout for a game mode and size, falling back to the closest one available.
    pub fn game_mode(&self, mode: &str, size: u32) -> Option<&GameModeDesc> {
        let candidates = self.desc.game_modes.iter().filter(|g| g.mode == mode);
        candidates
            .min_by_key(|g| g.size.abs_diff(size))
            .or_else(|| self.desc.game_modes.first())
    }
}

/// Marks every entity belonging to the loaded level so it can be unloaded.
#[derive(Component)]
pub struct LevelEntity;

/// Terrain heightfield. The client builds the render mesh from the same data.
#[derive(Component, Clone)]
pub struct Terrain(pub Arc<Heightmap>);

/// A simple primitive prop, used by the built-in test level.
#[derive(Component, Clone, Copy, Debug)]
pub struct BoxProp {
    pub size: Vec3,
    pub color: Color,
}

/// Terrain heights in meters.
pub struct Heightmap {
    pub resolution: u32,
    pub spacing: f32,
    /// World position of sample (0, 0).
    pub origin: Vec3,
    /// Heights relative to `origin.y`, row-major: `heights[z * resolution + x]`.
    pub heights: Vec<f32>,
}

impl Heightmap {
    pub fn world_size(&self) -> f32 {
        (self.resolution - 1) as f32 * self.spacing
    }

    pub fn center(&self) -> Vec3 {
        let half = self.world_size() * 0.5;
        Vec3::new(self.origin.x + half, self.origin.y, self.origin.z + half)
    }

    pub fn sample(&self, x: u32, z: u32) -> f32 {
        let r = self.resolution;
        self.heights[(z.min(r - 1) * r + x.min(r - 1)) as usize]
    }

    /// World height at a world XZ position, exactly matching the triangles of the render
    /// mesh and the physics collider.
    ///
    /// Every cell is split along the diagonal from sample `(x, z)` to `(x + 1, z + 1)`. That is
    /// how BF2 triangulates its terrain once its rows are flipped into our -Z-forward space.
    pub fn height_at(&self, world_x: f32, world_z: f32) -> f32 {
        let max = (self.resolution - 1) as f32;
        let fx = ((world_x - self.origin.x) / self.spacing).clamp(0.0, max);
        let fz = ((world_z - self.origin.z) / self.spacing).clamp(0.0, max);
        let (x0, z0) = ((fx.floor() as u32).min(self.resolution - 2), (fz.floor() as u32).min(self.resolution - 2));
        let (u, v) = (fx - x0 as f32, fz - z0 as f32);
        let h00 = self.sample(x0, z0);
        let h10 = self.sample(x0 + 1, z0);
        let h01 = self.sample(x0, z0 + 1);
        let h11 = self.sample(x0 + 1, z0 + 1);
        let h = if u >= v {
            // Triangle (x0,z0) (x1,z0) (x1,z1).
            h00 + (h10 - h00) * u + (h11 - h10) * v
        } else {
            // Triangle (x0,z0) (x0,z1) (x1,z1).
            h00 + (h01 - h00) * v + (h11 - h01) * u
        };
        self.origin.y + h
    }

    /// Loads a little-endian `u16` heightmap as described by `desc`.
    pub fn load(dir: &std::path::Path, desc: &TerrainDesc) -> anyhow::Result<Self> {
        let path = dir.join(&desc.heightmap);
        let bytes = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let expected = (desc.resolution * desc.resolution * 2) as usize;
        anyhow::ensure!(
            bytes.len() == expected,
            "{} is {} bytes, expected {expected} for a {}x{} heightmap",
            path.display(),
            bytes.len(),
            desc.resolution,
            desc.resolution
        );
        let heights = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as f32 * desc.height_scale)
            .collect();
        Ok(Self {
            resolution: desc.resolution,
            spacing: desc.spacing,
            origin: Vec3::from_array(desc.origin),
            heights,
        })
    }

    fn collider(&self) -> Collider {
        use avian3d::parry::{
            shape::{HeightField, HeightFieldCellStatus, SharedShape},
            utils::Array2,
        };
        let r = self.resolution as usize;
        // Parry's matrix: row index along Z, column along X.
        let heights = Array2::from_fn(r, r, |z, x| self.heights[z * r + x]);
        let size = self.world_size();
        let mut field = HeightField::new(heights, Vec3::new(size, 1.0, size));
        // Split cells along (x, z)-(x+1, z+1) like the render mesh (see `height_at`).
        for status in field.cells_statuses_mut().data_mut() {
            *status |= HeightFieldCellStatus::ZIGZAG_SUBDIVISION;
        }
        Collider::from(SharedShape::new(field))
    }
}

fn load_level_for_match(
    add: On<Add, MatchInfo>,
    infos: Query<&MatchInfo>,
    old: Query<Entity, With<LevelEntity>>,
    paths: Res<GamePaths>,
    mut commands: Commands,
) {
    let Ok(info) = infos.get(add.entity) else {
        return;
    };
    for entity in &old {
        commands.entity(entity).despawn();
    }

    let level = match load_level(&paths, &info.level) {
        Ok(level) => level,
        Err(err) => {
            error!(
                "failed to load level `{}`: {err:#}. Falling back to `{TEST_RANGE}`",
                info.level
            );
            test_range()
        }
    };
    info!(
        "loaded level `{}` ({} statics)",
        level.desc.display_name,
        level.desc.statics.len()
    );
    spawn_level(&mut commands, &level, &paths);
    commands.insert_resource(level);
}

/// Loads a level description and its heightmap.
pub fn load_level(paths: &GamePaths, name: &str) -> anyhow::Result<LoadedLevel> {
    if name == TEST_RANGE {
        return Ok(test_range());
    }
    let dir = paths.level_dir(name);
    let desc: LevelDesc = game_data::read_ron(dir.join("level.ron"))?;
    let heightmap = desc
        .terrain
        .as_ref()
        .map(|t| Heightmap::load(&dir, t).map(Arc::new))
        .transpose()?;
    Ok(LoadedLevel {
        desc,
        dir: Some(dir),
        heightmap,
    })
}

fn spawn_level(commands: &mut Commands, level: &LoadedLevel, paths: &GamePaths) {
    if let Some(heightmap) = &level.heightmap {
        commands.spawn((
            LevelEntity,
            Terrain(heightmap.clone()),
            Transform::from_translation(heightmap.center()),
            RigidBody::Static,
            heightmap.collider(),
            CollisionLayers::new(GameLayer::World, LayerMask::ALL),
        ));
    }
    if level.dir.is_none() {
        for (placement, size, color) in test_range_props() {
            commands.spawn((
                LevelEntity,
                BoxProp { size, color },
                placement_transform(&placement),
                RigidBody::Static,
                Collider::cuboid(size.x, size.y, size.z),
                CollisionLayers::new(GameLayer::World, LayerMask::ALL),
            ));
        }
    }
    if !level.desc.statics.is_empty() {
        crate::statics::spawn_statics(commands, &level.desc.statics, paths);
    }
}

pub fn placement_transform(p: &Placement) -> Transform {
    Transform {
        translation: Vec3::from_array(p.position),
        rotation: Quat::from_array(p.rotation),
        scale: Vec3::from_array(p.scale),
    }
}

/// A small procedurally generated level for testing movement and networking without
/// imported assets.
pub fn test_range() -> LoadedLevel {
    const RES: u32 = 257;
    const SPACING: f32 = 2.0;
    let size = (RES - 1) as f32 * SPACING;
    let origin = Vec3::new(-size * 0.5, 0.0, -size * 0.5);

    let mut heights = Vec::with_capacity((RES * RES) as usize);
    for z in 0..RES {
        for x in 0..RES {
            let wx = origin.x + x as f32 * SPACING;
            let wz = origin.z + z as f32 * SPACING;
            let hills = (wx * 0.013).sin() * (wz * 0.011).cos() * 9.0
                + (wx * 0.041 + 1.3).sin() * (wz * 0.037).sin() * 2.5;
            // Flatten the middle so the props sit on level ground.
            let d = Vec2::new(wx, wz).length();
            let flat = ((d - 40.0) / 60.0).clamp(0.0, 1.0);
            heights.push(10.0 + hills * flat);
        }
    }
    let heightmap = Heightmap {
        resolution: RES,
        spacing: SPACING,
        origin,
        heights,
    };

    let base = |id: &str, team: u8, z: f32| ControlPointDesc {
        id: id.into(),
        name: id.into(),
        position: [0.0, heightmap.height_at(0.0, z), z],
        initial_team: team,
        radius: 15.0,
        uncapturable: true,
    };
    let spawns = |cp: &str, z: f32, yaw: f32| -> Vec<SpawnPointDesc> {
        (-3..=3)
            .map(|i| {
                let x = i as f32 * 4.0;
                SpawnPointDesc {
                    control_point: cp.into(),
                    placement: Placement {
                        position: [x, heightmap.height_at(x, z) + 0.2, z],
                        rotation: Quat::from_rotation_y(yaw).to_array(),
                        ..default()
                    },
                }
            })
            .collect()
    };
    let conquest = GameModeDesc {
        mode: "gpm_cq".into(),
        size: 16,
        control_points: vec![
            base("base_one", 1, 200.0),
            base("base_two", 2, -200.0),
            ControlPointDesc {
                id: "center".into(),
                name: "Center".into(),
                position: [0.0, 10.0, 0.0],
                initial_team: 0,
                radius: 12.0,
                uncapturable: false,
            },
        ],
        spawn_points: [
            spawns("base_one", 200.0, 0.0),
            spawns("base_two", -200.0, std::f32::consts::PI),
        ]
        .concat(),
        ..default()
    };

    LoadedLevel {
        desc: LevelDesc {
            name: TEST_RANGE.into(),
            display_name: "Test Range".into(),
            game_modes: vec![conquest],
            ..default()
        },
        dir: None,
        heightmap: Some(Arc::new(heightmap)),
    }
}

fn test_range_props() -> Vec<(Placement, Vec3, Color)> {
    let at = |x: f32, y: f32, z: f32, yaw_deg: f32| Placement {
        position: [x, y, z],
        rotation: Quat::from_rotation_y(yaw_deg.to_radians()).to_array(),
        ..default()
    };
    let concrete = Color::srgb(0.55, 0.53, 0.5);
    let crate_color = Color::srgb(0.45, 0.36, 0.22);
    let mut props = vec![
        // A walled compound in the middle.
        (at(0.0, 12.0, -20.0, 0.0), Vec3::new(30.0, 4.0, 0.6), concrete),
        (at(0.0, 12.0, 20.0, 0.0), Vec3::new(30.0, 4.0, 0.6), concrete),
        (at(-15.0, 12.0, 0.0, 0.0), Vec3::new(0.6, 4.0, 40.0), concrete),
        (at(15.0, 12.0, 8.0, 0.0), Vec3::new(0.6, 4.0, 24.0), concrete),
        // Steps to test step-up / snapping.
        (at(6.0, 10.15, 0.0, 0.0), Vec3::new(2.0, 0.3, 3.0), concrete),
        (at(8.0, 10.3, 0.0, 0.0), Vec3::new(2.0, 0.6, 3.0), concrete),
        (at(10.0, 10.45, 0.0, 0.0), Vec3::new(2.0, 0.9, 3.0), concrete),
    ];
    // Scattered crates for cover.
    for i in 0..8 {
        let a = i as f32 * 0.785;
        props.push((
            at(a.cos() * 8.0, 10.6, a.sin() * 8.0, i as f32 * 20.0),
            Vec3::splat(1.2),
            crate_color,
        ));
    }
    props
}
