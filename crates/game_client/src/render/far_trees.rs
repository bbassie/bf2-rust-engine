//! Far trees: past [`FAR_TREES_FROM`] detailed trees fade out and BF2's stand-ins fade in (a
//! few crossed planes per tree, textured from the level's overgrowth atlas). Stand-ins are
//! merged into one mesh per 256 m cell, so thousands of distant trees are a handful of draws.

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::VisibilityRange,
    light::NotShadowCaster,
    math::Affine3A,
    mesh::{Indices, MeshVertexBufferLayoutRef},
    pbr::{MaterialPipeline, MaterialPipelineKey},
    platform::collections::{HashMap, HashSet},
    prelude::*,
    render::render_resource::{
        AsBindGroup, PrimitiveTopology, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
    },
    shader::ShaderRef,
};
use game_data::{TreeLodDesc, VegetationDesc};
use game_shared::{
    level::{LevelEntity, LoadedLevel},
    statics::StaticMesh,
};

pub struct FarTreesPlugin;

impl Plugin for FarTreesPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "shaders/far_trees.wgsl");
        app.add_plugins(MaterialPlugin::<FarTreeMaterial>::default()).add_systems(
            Update,
            (
                load_far_trees.run_if(resource_exists_and_changed::<LoadedLevel>),
                (collect_trees, fade_detailed_trees, build_cells).run_if(resource_exists::<FarTrees>),
            )
                .chain(),
        );
    }
}

/// Distance (m) from the camera where detailed trees start handing over to stand-ins...
const FAR_TREES_FROM: f32 = 150.0;
/// ...and where only stand-ins are left.
const FAR_TREES_TO: f32 = 180.0;
/// Stand-ins are merged per cell of this size (m).
const CELL_SIZE: f32 = 256.0;

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
struct FarTreeMaterial {
    #[uniform(0)]
    params: FarTreeParams,
    #[texture(1)]
    #[sampler(2)]
    atlas: Handle<Image>,
}

#[derive(ShaderType, Debug, Clone, Copy)]
struct FarTreeParams {
    /// x: hand-over start, y: end (m), z: alpha cutoff.
    fade: Vec4,
    /// Lit like the trees (`environment::LightScale::uniforms`).
    light_sun: Vec4,
    light_ambient: Vec4,
}

impl Material for FarTreeMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/render/shaders/far_trees.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/far_trees.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Mask(self.params.fade.z)
    }

    // Far away: no contact shadows to compute, no shadows to cast.
    fn enable_prepass() -> bool {
        false
    }

    fn enable_shadows() -> bool {
        false
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(2),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(3),
        ])?];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// Stand-ins of the loaded level and the trees placed so far.
#[derive(Resource)]
struct FarTrees {
    lods: Vec<TreeLodDesc>,
    /// Stand-in index per detailed tree mesh.
    by_mesh: HashMap<String, usize>,
    material: Handle<FarTreeMaterial>,
    cells: HashMap<IVec2, Vec<(usize, Affine3A)>>,
    dirty: HashSet<IVec2>,
    entities: HashMap<IVec2, Entity>,
}

/// A static mesh already checked for a stand-in.
#[derive(Component)]
struct TreeChecked;

/// A detailed tree that has a stand-in: it fades out with distance.
#[derive(Component)]
struct HasStandIn;

#[derive(Component)]
struct FarTreeCell;

fn load_far_trees(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    old_cells: Query<Entity, With<FarTreeCell>>,
    checked: Query<Entity, With<TreeChecked>>,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<FarTreeMaterial>>,
) {
    commands.remove_resource::<FarTrees>();
    for cell in &old_cells {
        commands.entity(cell).despawn();
    }
    for entity in &checked {
        commands.entity(entity).try_remove::<(TreeChecked, HasStandIn)>();
    }
    let (Some(dir), Some(file)) = (&level.dir, &level.desc.vegetation) else {
        return;
    };
    let Ok(vegetation) = game_data::read_ron::<VegetationDesc>(dir.join(file)) else {
        return;
    };
    let Some(atlas) = vegetation.tree_atlas.filter(|_| !vegetation.tree_lods.is_empty()) else {
        return;
    };
    let (light_sun, light_ambient) = super::environment::LevelLight::new(&level.desc.environment).trees.uniforms();
    let material = materials.add(FarTreeMaterial {
        params: FarTreeParams {
            fade: Vec4::new(FAR_TREES_FROM, FAR_TREES_TO, 0.5, 0.0),
            light_sun,
            light_ambient,
        },
        atlas: asset_server.load(format!("imported://levels/{}/{atlas}", level.desc.name)),
    });
    let by_mesh = vegetation
        .tree_lods
        .iter()
        .enumerate()
        .map(|(i, lod)| (lod.mesh.clone(), i))
        .collect();
    commands.insert_resource(FarTrees {
        lods: vegetation.tree_lods,
        by_mesh,
        material,
        cells: HashMap::default(),
        dirty: HashSet::default(),
        entities: HashMap::default(),
    });
}

/// Files every new static tree with a stand-in under its cell.
fn collect_trees(
    mut commands: Commands,
    mut far: ResMut<FarTrees>,
    statics: Query<(Entity, &StaticMesh, &Transform), Without<TreeChecked>>,
) {
    for (entity, mesh, transform) in &statics {
        // `try_`: debris and other short-lived static meshes may be despawned by their own
        // system in the same frame.
        let mut checked = commands.entity(entity);
        checked.try_insert(TreeChecked);
        let Some(&lod) = far.by_mesh.get(&mesh.path) else {
            continue;
        };
        checked.try_insert(HasStandIn);
        let cell = (transform.translation.xz() / CELL_SIZE).floor().as_ivec2();
        far.cells.entry(cell).or_default().push((lod, transform.compute_affine()));
        far.dirty.insert(cell);
    }
}

/// Detailed trees fade out where their stand-ins fade in (the dither patterns interlock).
fn fade_detailed_trees(
    mut commands: Commands,
    new_meshes: Query<(Entity, &ChildOf), Added<Mesh3d>>,
    new_trees: Query<&Children, Added<HasStandIn>>,
    trees: Query<(), With<HasStandIn>>,
    meshes: Query<(), With<Mesh3d>>,
) {
    let range = VisibilityRange {
        start_margin: 0.0..0.0,
        end_margin: FAR_TREES_FROM..FAR_TREES_TO,
        use_aabb: false,
    };
    for (entity, child_of) in &new_meshes {
        if trees.contains(child_of.parent()) {
            commands.entity(entity).try_insert(range.clone());
        }
    }
    for children in &new_trees {
        for child in children.iter().filter(|c| meshes.contains(*c)) {
            commands.entity(child).try_insert(range.clone());
        }
    }
}

/// (Re)builds the merged mesh of every cell that got trees.
fn build_cells(mut commands: Commands, mut far: ResMut<FarTrees>, mut meshes: ResMut<Assets<Mesh>>) {
    if far.dirty.is_empty() {
        return;
    }
    let far = &mut *far;
    for cell in far.dirty.drain() {
        let origin = Vec3::new(cell.x as f32 * CELL_SIZE, 0.0, cell.y as f32 * CELL_SIZE);
        let mesh = cell_mesh(&far.lods, &far.cells[&cell], origin);
        if let Some(old) = far.entities.remove(&cell) {
            commands.entity(old).despawn();
        }
        let entity = commands
            .spawn((
                FarTreeCell,
                LevelEntity,
                Mesh3d(meshes.add(mesh)),
                MeshMaterial3d(far.material.clone()),
                Transform::from_translation(origin),
                NotShadowCaster,
            ))
            .id();
        far.entities.insert(cell, entity);
    }
}

/// All stand-ins of a cell in one mesh, relative to `origin`. The vertex colour holds each
/// tree's root, so the shader fades a tree as a whole.
fn cell_mesh(lods: &[TreeLodDesc], trees: &[(usize, Affine3A)], origin: Vec3) -> Mesh {
    let vertex_count: usize = trees.iter().map(|(lod, _)| lods[*lod].positions.len()).sum();
    let mut positions = Vec::with_capacity(vertex_count);
    let mut normals = Vec::with_capacity(vertex_count);
    let mut uvs = Vec::with_capacity(vertex_count);
    let mut roots = Vec::with_capacity(vertex_count);
    let mut indices = Vec::new();
    for (lod, transform) in trees {
        let lod = &lods[*lod];
        let base = positions.len() as u32;
        let root = (Vec3::from(transform.translation) - origin).extend(0.0).to_array();
        for ((p, n), uv) in lod.positions.iter().zip(&lod.normals).zip(&lod.uvs) {
            positions.push((transform.transform_point3(Vec3::from_array(*p)) - origin).to_array());
            normals.push(transform.transform_vector3(Vec3::from_array(*n)).normalize_or(Vec3::Y).to_array());
            uvs.push(*uv);
            roots.push(root);
        }
        indices.extend(lod.indices.iter().map(|&i| base + i as u32));
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, roots)
        .with_inserted_indices(Indices::U32(indices))
}
