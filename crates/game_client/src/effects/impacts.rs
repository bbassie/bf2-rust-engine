//! What a bullet hit is made of, for its impact effect: soldiers are flesh, static objects
//! and the terrain have the materials the importer found (`levels/<name>/surfaces.ron`),
//! destroyable parts their hit material. Ids are BF2's material ids (`materials.ron`).

use avian3d::prelude::*;
use bevy::{ecs::system::SystemParam, prelude::*};
use game_data::SurfaceMap;
use game_shared::{
    level::{LoadedLevel, Terrain},
    physics::GameLayer,
    statics::{Destructible, StaticMesh},
};

pub const DIRT: u32 = 8;
pub const HUMAN_BODY: u32 = 24;
pub const HUMAN_HEAD: u32 = 25;
pub const HUMAN_LIMBS: u32 = 77;
pub const CONCRETE: u32 = 78;
/// Small arms bullets, the projectile material of most handheld weapons.
pub const BULLET: u32 = 38;

/// The loaded level's surface materials.
#[derive(Resource, Default)]
pub struct Surfaces {
    map: SurfaceMap,
    terrain: Option<TerrainMaterials>,
}

struct TerrainMaterials {
    ids: Vec<u8>,
    resolution: u32,
    spacing: f32,
    origin: Vec3,
}

pub(super) fn load_surfaces(level: Res<LoadedLevel>, mut surfaces: ResMut<Surfaces>) {
    *surfaces = Surfaces::default();
    let Some(dir) = &level.dir else {
        return;
    };
    let path = dir.join("surfaces.ron");
    if !path.exists() {
        return;
    }
    surfaces.map = game_data::read_ron(&path).unwrap_or_else(|err| {
        warn!("{err}");
        SurfaceMap::default()
    });
    let (Some(file), Some(heightmap)) = (&surfaces.map.terrain, &level.heightmap) else {
        return;
    };
    match std::fs::read(dir.join(file)) {
        Ok(ids) if ids.len() == (heightmap.resolution * heightmap.resolution) as usize => {
            surfaces.terrain = Some(TerrainMaterials {
                ids,
                resolution: heightmap.resolution,
                spacing: heightmap.spacing,
                origin: heightmap.origin,
            });
        }
        Ok(_) => warn!("{file}: size doesn't match the heightmap"),
        Err(err) => warn!("{file}: {err}"),
    }
}

impl Surfaces {
    fn terrain_at(&self, point: Vec3) -> Option<u32> {
        let terrain = self.terrain.as_ref()?;
        let cell = |v: f32| (v / terrain.spacing).round().clamp(0.0, (terrain.resolution - 1) as f32) as usize;
        let (x, z) = (cell(point.x - terrain.origin.x), cell(point.z - terrain.origin.z));
        let id = *terrain.ids.get(z * terrain.resolution as usize + x)?;
        // 0 is BF2's "Default": no particular material painted.
        (id != 0).then_some(id as u32)
    }
}

/// Finds the material of whatever a ray hit.
#[derive(SystemParam)]
pub struct SurfaceQuery<'w, 's> {
    surfaces: Res<'w, Surfaces>,
    layers: Query<'w, 's, &'static CollisionLayers>,
    parts: Query<'w, 's, (Option<&'static StaticMesh>, Option<&'static Destructible>)>,
    terrain: Query<'w, 's, (), With<Terrain>>,
}

impl SurfaceQuery<'_, '_> {
    /// Whether marks stick to what a ray hit: the terrain and static objects (`Some(None)`),
    /// destroyable parts (`Some(Some(part))`: the mark goes with the part), not soldiers or
    /// vehicles (`None`).
    pub fn decal_surface(&self, entity: Entity) -> Option<Option<Entity>> {
        if self.terrain.contains(entity) {
            return Some(None);
        }
        match self.parts.get(entity) {
            Ok((_, Some(_))) => Some(Some(entity)),
            Ok((Some(_), None)) => Some(None),
            _ => None,
        }
    }

    pub fn material(&self, entity: Entity, point: Vec3) -> u32 {
        if self
            .layers
            .get(entity)
            .is_ok_and(|l| l.memberships.has_all(GameLayer::Soldier))
        {
            return HUMAN_BODY;
        }
        if self.terrain.contains(entity) {
            return self.surfaces.terrain_at(point).unwrap_or(DIRT);
        }
        match self.parts.get(entity) {
            Ok((_, Some(destructible))) => destructible.hit_material,
            Ok((Some(mesh), None)) => self
                .surfaces
                .map
                .meshes
                .get(&(mesh.path.clone(), mesh.index))
                .copied()
                .unwrap_or(CONCRETE),
            _ => CONCRETE,
        }
    }
}
