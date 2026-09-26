//! BF2-style materials built on Bevy's `StandardMaterial`, so they keep PBR lighting,
//! shadows and fog.

use bevy::{
    asset::embedded_asset,
    image::{ImageAddressMode, ImageSamplerDescriptor},
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};

pub struct MaterialsPlugin;

impl Plugin for MaterialsPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "shaders/static_layers.wgsl");
        embedded_asset!(app, "shaders/terrain_layers.wgsl");
        app.add_plugins((
            MaterialPlugin::<StaticMaterial>::default(),
            MaterialPlugin::<TerrainMaterial>::default(),
        ));
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

/// Static meshes: base color (UV0) multiplied by a tiling detail texture (UV1).
pub type StaticMaterial = ExtendedMaterial<StandardMaterial, StaticLayers>;

/// Bindless, so the thousands of static objects with different textures still batch into
/// few draw calls (in the main pass and every shadow cascade).
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
#[data(50, StaticLayersUniform, binding_array(101))]
#[bindless(index_table(range(50..53), binding(100)))]
pub struct StaticLayers {
    pub flags: u32,
    pub detail_scale: f32,
    #[texture(51)]
    #[sampler(52)]
    pub detail: Option<Handle<Image>>,
}

impl StaticLayers {
    pub const HAS_DETAIL: u32 = 1;
    pub const ALPHA_FROM_DETAIL: u32 = 2;
}

#[derive(ShaderType, Clone, Default)]
pub struct StaticLayersUniform {
    pub flags: u32,
    pub detail_scale: f32,
    pub _pad: Vec2,
}

impl From<&StaticLayers> for StaticLayersUniform {
    fn from(layers: &StaticLayers) -> Self {
        Self {
            flags: layers.flags,
            detail_scale: layers.detail_scale,
            _pad: Vec2::ZERO,
        }
    }
}

impl MaterialExtension for StaticLayers {
    fn fragment_shader() -> ShaderRef {
        // `embedded_asset!` names paths after the crate, which is the `client` binary.
        "embedded://client/render/shaders/static_layers.wgsl".into()
    }
}
