//! Water: one large plane at the level's water height with an animated, reflective,
//! depth-aware surface (see `shaders/water.wgsl`).

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::MeshVertexBufferLayoutRef,
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{
        AsBindGroup, Extent3d, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
        TextureDimension, TextureFormat,
    },
    shader::ShaderRef,
};
use game_shared::level::{Heightmap, LevelEntity, LoadedLevel};

pub struct WaterPlugin;

impl Plugin for WaterPlugin {
    fn build(&self, app: &mut App) {
        embedded_shader!(app, "shaders/water.wgsl");
        app.add_plugins(MaterialPlugin::<WaterMaterial>::default()).add_systems(
            Update,
            spawn_water.run_if(resource_exists_and_changed::<LoadedLevel>),
        );
    }
}

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WaterMaterial {
    #[uniform(0)]
    params: WaterParams,
    #[texture(1, dimension = "3d")]
    #[sampler(2)]
    normal_map: Option<Handle<Image>>,
    #[texture(3, dimension = "cube")]
    #[sampler(4)]
    reflection_map: Option<Handle<Image>>,
    #[texture(5)]
    #[sampler(6)]
    depth_map: Option<Handle<Image>>,
}

#[derive(ShaderType, Debug, Clone, Copy)]
struct WaterParams {
    color: Vec4,
    specular: Vec4,
    waves: Vec4,
    flags: Vec4,
    depth_rect: Vec4,
    sky_color: Vec4,
}

impl Material for WaterMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://client/render/shaders/water.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://client/render/shaders/water.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Premultiplied
    }

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
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?];
        // Seen from below when diving.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

#[derive(Component)]
struct WaterSurface;

/// Depth is stored as `sqrt(depth / DEPTH_RANGE)`: finer steps near the shore.
const DEPTH_RANGE: f32 = 64.0;
/// Opacity of the water body where it is only centimeters deep.
const SHALLOW_OPACITY: f32 = 0.4;

fn spawn_water(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    old: Query<Entity, With<WaterSurface>>,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<WaterMaterial>>,
) {
    for entity in &old {
        commands.entity(entity).despawn();
    }
    let (Some(water), Some(heightmap)) = (&level.desc.water, &level.heightmap) else {
        return;
    };

    let normal_map = water.normal_map.as_ref().map(|path| {
        asset_server
            .load_builder()
            .with_settings(|s: &mut ImageLoaderSettings| {
                s.is_srgb = false;
                let mut sampler = ImageSamplerDescriptor::linear();
                sampler.set_address_mode(ImageAddressMode::Repeat);
                s.sampler = ImageSampler::Descriptor(sampler);
            })
            .load(format!("imported://{path}"))
    });
    let reflection_map = water
        .reflection_map
        .as_ref()
        .map(|path| asset_server.load(format!("imported://{path}")));
    let depth_map = images.add(depth_image(heightmap, water.height));

    // Level colours are display (sRGB) values; the shader works in linear light.
    let linear = |c: [f32; 3]| LinearRgba::from(Color::srgb(c[0], c[1], c[2])).to_vec4();
    let [r, g, b, _] = water.color;
    let [sr, sg, sb, strength] = water.specular;
    let material = materials.add(WaterMaterial {
        params: WaterParams {
            color: linear([r, g, b]).with_w(SHALLOW_OPACITY),
            specular: linear([sr, sg, sb]).with_w(strength),
            waves: Vec4::new(water.wave_drift[0], water.wave_drift[1], water.wave_speed, water.specular_power),
            flags: Vec4::new(
                water.opaque_depth,
                f32::from(u8::from(normal_map.is_some())),
                f32::from(u8::from(reflection_map.is_some())),
                1.0,
            ),
            depth_rect: Vec4::new(
                heightmap.origin.x,
                heightmap.origin.z,
                heightmap.world_size(),
                heightmap.world_size(),
            ),
            sky_color: linear(level.desc.environment.sky_color),
        },
        normal_map,
        reflection_map,
        depth_map: Some(depth_map),
    });

    // Out to the horizon, as far as the surrounding terrain reaches.
    let size = heightmap.world_size() * 3.0 + 2.0 * super::terrain::WORLD_EXTENSION;
    commands.spawn((
        WaterSurface,
        LevelEntity,
        Mesh3d(meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(size * 0.5)).mesh().subdivisions(47))),
        MeshMaterial3d(material),
        Transform::from_translation(heightmap.center().with_y(water.height)),
        NotShadowCaster,
        NotShadowReceiver,
    ));
}

/// Water depth over the terrain, one texel per heightmap sample (0 on land).
fn depth_image(heightmap: &Heightmap, water_height: f32) -> Image {
    let n = heightmap.resolution;
    let data = heightmap
        .heights
        .iter()
        .map(|h| {
            let depth = water_height - (heightmap.origin.y + h);
            ((depth / DEPTH_RANGE).clamp(0.0, 1.0).sqrt() * 255.0).round() as u8
        })
        .collect();
    let mut image = Image::new(
        Extent3d {
            width: n,
            height: n,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::linear();
    image
}
