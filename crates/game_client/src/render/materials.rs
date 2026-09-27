//! BF2-style materials built on Bevy's `StandardMaterial`, so they keep PBR lighting,
//! shadows and fog.

use bevy::{
    asset::embedded_asset,
    ecs::system::SystemParam,
    gltf::{GltfMaterialExtras, GltfPrimitive},
    image::{ImageAddressMode, ImageLoaderSettings, ImageSamplerDescriptor},
    mesh::MeshVertexBufferLayoutRef,
    pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline},
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError},
    shader::ShaderRef,
};
use game_shared::{config::GamePaths, level::LoadedLevel};

pub struct MaterialsPlugin;

impl Plugin for MaterialsPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/bf2_material.wgsl");
        embedded_asset!(app, "shaders/terrain_layers.wgsl");
        app.add_plugins((
            MaterialPlugin::<Bf2Material>::default(),
            MaterialPlugin::<TerrainMaterial>::default(),
        ))
        .init_resource::<Bf2MaterialCache>()
        .add_systems(Update, load_env_map.run_if(resource_exists_and_changed::<LoadedLevel>))
        .add_systems(PostUpdate, swap_scene_materials);
    }
}

/// Like [`default_sampler`] but clamped at the edges, for per-patch terrain images that
/// must not bleed into the neighbouring patch.
pub fn clamped_sampler() -> ImageSamplerDescriptor {
    let mut sampler = default_sampler();
    sampler.set_address_mode(ImageAddressMode::ClampToEdge);
    sampler
}

/// Terrain patch: color map (the base color) blended with up to six tiling detail textures.
pub type TerrainMaterial = ExtendedMaterial<StandardMaterial, TerrainLayers>;

/// Six detail textures share `detail_0`'s sampler; both weight maps share `weights_a`'s.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
pub struct TerrainLayers {
    #[uniform(100)]
    pub params: TerrainLayerParams,
    #[texture(101)]
    #[sampler(102)]
    pub weights_a: Option<Handle<Image>>,
    #[texture(103)]
    pub weights_b: Option<Handle<Image>>,
    #[texture(104)]
    #[sampler(110)]
    pub detail_0: Option<Handle<Image>>,
    #[texture(105)]
    pub detail_1: Option<Handle<Image>>,
    #[texture(106)]
    pub detail_2: Option<Handle<Image>>,
    #[texture(107)]
    pub detail_3: Option<Handle<Image>>,
    #[texture(108)]
    pub detail_4: Option<Handle<Image>>,
    #[texture(109)]
    pub detail_5: Option<Handle<Image>>,
}

#[derive(ShaderType, Reflect, Debug, Clone, Copy, Default)]
pub struct TerrainLayerParams {
    /// Meters per repeat (top projection) of detail textures 0..3.
    pub tile_a: Vec4,
    /// Meters per repeat of detail textures 4 and 5.
    pub tile_b: Vec4,
    /// xy: side projection tile size of detail texture 0; z/w: detail fade start/end.
    pub side0_fade: Vec4,
    pub flags: u32,
    pub _pad0: u32,
    pub _pad1: u32,
    pub _pad2: u32,
}

impl TerrainLayerParams {
    pub const TRI_PLANAR_0: u32 = 1;
    pub const HAS_WEIGHTS: u32 = 2;
}

impl MaterialExtension for TerrainLayers {
    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/terrain_layers.wgsl".into()
    }
}

/// Sampler for BF2 textures: tiling, trilinear, 16x anisotropic (ground and walls are
/// mostly seen at grazing angles).
pub fn default_sampler() -> ImageSamplerDescriptor {
    let mut sampler = ImageSamplerDescriptor::linear();
    sampler.set_address_mode(ImageAddressMode::Repeat);
    sampler.set_anisotropic_filter(16);
    sampler
}

/// BF2's materials for static, bundled and skinned meshes: `StandardMaterial` (PBR lighting,
/// shadows, fog) with BF2's texture layers, normal maps and gloss on top, following the
/// game's `RaShaderSTM/BM/SM.fx`. Build them with [`Bf2Materials`].
pub type Bf2Material = ExtendedMaterial<StandardMaterial, Bf2Layers>;

/// Bindless, so the thousands of objects with different textures still batch into few draw
/// calls (in the main pass, the prepass and every shadow cascade). All layers share the
/// detail texture's sampler; the base color is the `StandardMaterial`'s.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
#[data(50, Bf2LayersUniform, binding_array(101))]
#[bindless(index_table(range(50..58), binding(100)))]
pub struct Bf2Layers {
    /// [`Bf2Layers`] flag constants.
    pub flags: u32,
    /// Gloss where no texture provides it (BF2's `StaticGloss`).
    pub gloss: f32,
    /// Albedo factor: BF2 doubles the lighting of static meshes.
    pub albedo_scale: f32,
    /// Detail texture (UV1) or wreck map (UV0), multiplies the base color.
    #[texture(51)]
    #[sampler(52)]
    pub detail: Option<Handle<Image>>,
    /// Dirt texture (UVs in `COLOR_0.xy`), multiplies the color.
    #[texture(53)]
    pub dirt: Option<Handle<Image>>,
    /// Crack texture (UVs in `COLOR_0.zw`), blended over by its alpha.
    #[texture(54)]
    pub crack: Option<Handle<Image>>,
    /// Normal map: static meshes' detail normals (UV1), others' tangent or object space
    /// normals (UV0).
    #[texture(55)]
    pub normal: Option<Handle<Image>>,
    /// Crack normal map, blended over the detail normals by the crack's alpha.
    #[texture(56)]
    pub crack_normal: Option<Handle<Image>>,
    /// Reflection cube map for `EnvMap` techniques.
    #[texture(57, dimension = "cube")]
    pub env_map: Option<Handle<Image>>,
}

impl Default for Bf2Layers {
    fn default() -> Self {
        Self {
            flags: 0,
            gloss: STATIC_GLOSS,
            albedo_scale: 1.0,
            detail: None,
            dirt: None,
            crack: None,
            normal: None,
            crack_normal: None,
            env_map: None,
        }
    }
}

impl Bf2Layers {
    pub const DETAIL: u32 = 1 << 0;
    pub const DIRT: u32 = 1 << 1;
    pub const CRACK: u32 = 1 << 2;
    /// Tangent space normal map.
    pub const NORMAL_MAP: u32 = 1 << 3;
    /// The normal map uses UV1 (static detail normals).
    pub const NORMAL_UV_B: u32 = 1 << 4;
    /// Object space normal map (skinned meshes); the unskinned vertex frame is packed into
    /// `COLOR_0` and `TEXCOORD_1`.
    pub const OBJECT_SPACE: u32 = 1 << 5;
    pub const CRACK_NORMAL: u32 = 1 << 6;
    /// Parallax on the detail UVs, height from the normal map's alpha.
    pub const PARALLAX: u32 = 1 << 7;
    pub const GLOSS_FROM_DETAIL: u32 = 1 << 8;
    pub const GLOSS_FROM_NORMAL: u32 = 1 << 9;
    /// `ColormapGloss`: the color map's alpha is gloss and the surface is opaque.
    pub const GLOSS_FROM_BASE: u32 = 1 << 10;
    /// Alpha test on base alpha x detail alpha.
    pub const ALPHA_FROM_DETAIL: u32 = 1 << 11;
    /// Alpha test on the color's brightness (`ColormapGloss` with `Alpha_Test`).
    pub const ALPHA_FROM_COLOR: u32 = 1 << 12;
    pub const ENV_MAP: u32 = 1 << 13;
    /// `AnimatedUV` at rest: the color map's UV is UV0 + UV1.
    pub const UV_SUM: u32 = 1 << 14;
    /// Wrecks: the `detail` texture is a wreck map on UV0 that darkens color and gloss.
    pub const WRECK: u32 = 1 << 15;
}

#[derive(ShaderType, Clone, Default)]
pub struct Bf2LayersUniform {
    pub flags: u32,
    pub gloss: f32,
    pub albedo_scale: f32,
    pub _pad: f32,
}

impl From<&Bf2Layers> for Bf2LayersUniform {
    fn from(layers: &Bf2Layers) -> Self {
        Self {
            flags: layers.flags,
            gloss: layers.gloss,
            albedo_scale: layers.albedo_scale,
            _pad: 0.0,
        }
    }
}

impl MaterialExtension for Bf2Layers {
    // `embedded_asset!` names paths after the crate, which is the `client` binary. One file
    // serves both pipelines (`PREPASS_PIPELINE`).
    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/bf2_material.wgsl".into()
    }

    fn prepass_fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/bf2_material.wgsl".into()
    }

    /// `BF2_MATERIAL_DEBUG=lighting` (grey albedo), `normals`, `gloss` or `env` shows one term;
    /// `nolayers` draws the base color only (to measure the layers' cost).
    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        static DEBUG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let debug = DEBUG.get_or_init(|| std::env::var("BF2_MATERIAL_DEBUG").ok());
        if let (Some(debug), Some(fragment)) = (debug, descriptor.fragment.as_mut()) {
            fragment
                .shader_defs
                .push(format!("BF2_DEBUG_{}", debug.to_ascii_uppercase()).as_str().into());
        }
        Ok(())
    }
}

/// One BF2 material per glTF material, shared by everything using it.
#[derive(Resource, Default)]
struct Bf2MaterialCache {
    materials: HashMap<AssetId<StandardMaterial>, Handle<Bf2Material>>,
    untextured: Option<Handle<Bf2Material>>,
    /// The level's environment cube map and the materials reflecting it.
    env_map: Option<Handle<Image>>,
    env_users: Vec<Handle<Bf2Material>>,
}

/// Makes BF2 materials from glTF materials: the `StandardMaterial` Bevy's glTF loader
/// creates for each, plus the material's `bf2` extras written by the importer
/// (`kind`, `technique`, `maps`).
#[derive(SystemParam)]
pub struct Bf2Materials<'w> {
    cache: ResMut<'w, Bf2MaterialCache>,
    standard: Res<'w, Assets<StandardMaterial>>,
    materials: ResMut<'w, Assets<Bf2Material>>,
    asset_server: Res<'w, AssetServer>,
}

impl Bf2Materials<'_> {
    /// The material of a glTF primitive; `None` while the glTF's materials are loading.
    pub fn for_primitive(&mut self, primitive: &GltfPrimitive) -> Option<Handle<Bf2Material>> {
        let Some(gltf_material) = &primitive.material else {
            let materials = &mut self.materials;
            return Some(
                self.cache
                    .untextured
                    .get_or_insert_with(|| materials.add(Bf2Material::default()))
                    .clone(),
            );
        };
        // Bevy's glTF loader stores a StandardMaterial next to every glTF material.
        let path = gltf_material.path()?;
        let standard = self
            .asset_server
            .load(path.clone().with_label(format!("{}/std", path.label()?)));
        self.from_standard(&standard, primitive.material_extras.as_ref().map(|e| e.value.as_str()))
    }

    /// The material for a glTF material's `StandardMaterial` and extras JSON; `None` while
    /// the `StandardMaterial` is loading.
    pub fn from_standard(
        &mut self,
        standard: &Handle<StandardMaterial>,
        extras: Option<&str>,
    ) -> Option<Handle<Bf2Material>> {
        if let Some(done) = self.cache.materials.get(&standard.id()) {
            return Some(done.clone());
        }
        let base = self.standard.get(standard)?.clone();
        let bf2 = extras
            .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
            .map(|v| v["bf2"].clone())
            .unwrap_or_default();
        let path = standard
            .path()
            .map(|p| p.path().to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let material = describe(base, &bf2, &path, &self.asset_server, self.cache.env_map.as_ref());
        let reflects = technique_reflects(&bf2);
        let handle = self.materials.add(material);
        if reflects {
            self.cache.env_users.push(handle.clone());
        }
        self.cache.materials.insert(standard.id(), handle.clone());
        Some(handle)
    }
}

/// The layers of a static mesh technique (`BaseDetailDirtCrackNDetailNCrack`, ...) in the
/// order its maps are listed, and whether it has the `parallaxdetail` suffix.
fn static_layers(technique: &str) -> (Vec<&'static str>, bool) {
    let technique = technique.to_ascii_lowercase();
    let mut rest = technique.as_str();
    let mut layers = Vec::new();
    'layers: loop {
        for name in ["ndetail", "ncrack", "base", "detail", "dirt", "crack"] {
            if let Some(after) = rest.strip_prefix(name) {
                layers.push(name);
                rest = after;
                continue 'layers;
            }
        }
        break;
    }
    (layers, rest.contains("parallax"))
}

/// Gloss of surfaces without a gloss map (BF2's `StaticGloss`, set per material in `.tweak`
/// files; the default isn't known).
const STATIC_GLOSS: f32 = 0.15;
/// Gloss of glass (`AlphaEnvMap`): the reflectance of real glass.
const GLASS_GLOSS: f32 = 0.5;
/// Share of the light on a leaf that passes through to its other side.
const LEAF_TRANSMISSION: f32 = 0.4;

fn technique_reflects(bf2: &serde_json::Value) -> bool {
    bf2["kind"] != "static" && bf2["technique"].as_str().is_some_and(|t| t.to_ascii_lowercase().contains("envmap"))
}

/// Points an `EnvMap` material at the level's cube map (none: no reflection).
fn set_env_map(layers: &mut Bf2Layers, env_map: Option<Handle<Image>>) {
    if env_map.is_some() {
        layers.flags |= Bf2Layers::ENV_MAP;
    } else {
        layers.flags &= !Bf2Layers::ENV_MAP;
    }
    layers.env_map = env_map;
}

fn describe(
    mut base: StandardMaterial,
    bf2: &serde_json::Value,
    path: &str,
    asset_server: &AssetServer,
    env_map: Option<&Handle<Image>>,
) -> Bf2Material {
    let technique = bf2["technique"].as_str().unwrap_or_default().to_ascii_lowercase();
    let maps: Vec<Option<&str>> = bf2["maps"]
        .as_array()
        .map(|maps| maps.iter().map(|m| m["path"].as_str()).collect())
        .unwrap_or_default();
    let map = |i: usize| maps.get(i).copied().flatten().map(|p| format!("imported://{p}"));
    let color = |i: usize| map(i).map(|p| asset_server.load::<Image>(p));
    let linear = |i: usize| {
        map(i).map(|p| {
            asset_server
                .load_builder()
                .with_settings(|s: &mut ImageLoaderSettings| s.is_srgb = false)
                .load::<Image>(p)
        })
    };
    // Older files have no `kind`; static techniques are the ones made of layers.
    let kind = bf2["kind"]
        .as_str()
        .unwrap_or(if technique.starts_with("base") { "static" } else { "bundled" });
    let alpha_test = matches!(base.alpha_mode, AlphaMode::Mask(_));

    let mut layers = Bf2Layers::default();
    // The glTF normal texture (loaded as linear data) is applied by the extension.
    let normal_map = base.normal_map_texture.take();
    base.metallic = 0.0;

    if kind == "static" {
        let (names, parallax) = static_layers(&technique);
        let mut crack = false;
        for (i, name) in names.iter().enumerate() {
            match *name {
                "detail" => layers.detail = color(i),
                "dirt" => layers.dirt = color(i),
                "crack" => {
                    layers.crack = color(i);
                    crack = layers.crack.is_some();
                }
                "ndetail" => layers.normal = linear(i),
                "ncrack" => layers.crack_normal = linear(i),
                _ => {}
            }
        }
        let detail = layers.detail.is_some();
        let normal = layers.normal.is_some();
        // BF2 only draws cracks on its per-pixel lit path (techniques with normal maps).
        let per_pixel = normal || layers.crack_normal.is_some() || parallax;
        for (on, flag) in [
            (detail, Bf2Layers::DETAIL),
            (layers.dirt.is_some(), Bf2Layers::DIRT),
            (crack && per_pixel, Bf2Layers::CRACK),
            (normal, Bf2Layers::NORMAL_MAP | Bf2Layers::NORMAL_UV_B),
            (crack && per_pixel && layers.crack_normal.is_some(), Bf2Layers::CRACK_NORMAL),
            (parallax && normal, Bf2Layers::PARALLAX),
            (detail && !alpha_test, Bf2Layers::GLOSS_FROM_DETAIL),
            (detail && alpha_test, Bf2Layers::ALPHA_FROM_DETAIL),
        ] {
            if on {
                layers.flags |= flag;
            }
        }
        // With a detail layer and no alpha test, BF2 replaces the alpha with the object's
        // transparency (normally opaque).
        if detail && !alpha_test && base.alpha_mode != AlphaMode::Opaque {
            base.alpha_mode = AlphaMode::Opaque;
        }
        if path.contains("vegitation") {
            // Trees use BF2's leaf and trunk shaders: no specular. The leaf shader wraps the
            // sunlight around (`(N.L + 0.6) / 1.4`) so leaves facing away still get some:
            // light shining through them.
            layers.flags &= !Bf2Layers::GLOSS_FROM_DETAIL;
            layers.gloss = 0.0;
            if alpha_test {
                base.diffuse_transmission = LEAF_TRANSMISSION;
            }
        }
        layers.albedo_scale = 2.0;
    } else {
        let colormap_gloss = technique.contains("colormapgloss");
        if let Some(normal) = normal_map {
            layers.normal = Some(normal);
            layers.flags |= if kind == "skinned" && !technique.contains("tangent") {
                Bf2Layers::OBJECT_SPACE
            } else {
                Bf2Layers::NORMAL_MAP
            };
            if !colormap_gloss {
                layers.flags |= Bf2Layers::GLOSS_FROM_NORMAL;
            }
        }
        if colormap_gloss {
            layers.flags |= Bf2Layers::GLOSS_FROM_BASE;
            if alpha_test {
                layers.flags |= Bf2Layers::ALPHA_FROM_COLOR;
            }
        }
        if technique.contains("animateduv") {
            layers.flags |= Bf2Layers::UV_SUM;
        }
        if technique.contains("alpha") && technique.contains("envmap") {
            layers.gloss = GLASS_GLOSS;
        }
        // The map list is color, normal, wreck; wreck maps end in `_w` or `_wreck_c`.
        let wreck = (1..maps.len()).find(|&i| {
            maps[i].is_some_and(|p| {
                let p = p.to_ascii_lowercase();
                let p = p.trim_end_matches(".dds");
                p.ends_with("_w") || p.ends_with("_wreck_c")
            })
        });
        if let Some(wreck) = wreck {
            layers.detail = color(wreck);
            layers.flags |= Bf2Layers::WRECK;
        }
        if technique.contains("envmap") {
            set_env_map(&mut layers, env_map.cloned());
        }
    }
    Bf2Material { base, extension: layers }
}

/// Loads the level's environment cube map (`levels/<name>/envmaps/envmap0.dds`, BF2's
/// `Envmaps/EnvMap0.dds`) and hands it to the materials that reflect it.
fn load_env_map(
    level: Res<LoadedLevel>,
    paths: Res<GamePaths>,
    asset_server: Res<AssetServer>,
    mut cache: ResMut<Bf2MaterialCache>,
    mut materials: ResMut<Assets<Bf2Material>>,
) {
    let path = format!("levels/{}/envmaps/envmap0.dds", level.desc.name);
    cache.env_map = paths
        .imported
        .join(&path)
        .exists()
        .then(|| asset_server.load(format!("imported://{path}")));
    for handle in &cache.env_users {
        if let Some(mut material) = materials.get_mut(handle) {
            set_env_map(&mut material.extension, cache.env_map.clone());
        }
    }
}

/// glTF scenes (soldiers, first-person arms, flags) spawn with Bevy's `StandardMaterial`;
/// swap in the BF2 material where the glTF material has `bf2` extras.
#[allow(clippy::type_complexity)]
fn swap_scene_materials(
    mut commands: Commands,
    meshes: Query<(Entity, &MeshMaterial3d<StandardMaterial>, &GltfMaterialExtras), Without<Bf2Checked>>,
    mut bf2: Bf2Materials,
) {
    for (entity, material, extras) in &meshes {
        if !extras.value.contains("\"bf2\"") {
            commands.entity(entity).insert(Bf2Checked);
            continue;
        }
        if let Some(handle) = bf2.from_standard(&material.0, Some(&extras.value)) {
            commands
                .entity(entity)
                .remove::<MeshMaterial3d<StandardMaterial>>()
                .insert((MeshMaterial3d(handle), Bf2Checked));
        }
    }
}

/// A scene mesh [`swap_scene_materials`] has handled.
#[derive(Component)]
struct Bf2Checked;
