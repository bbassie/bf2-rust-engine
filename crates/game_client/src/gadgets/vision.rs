//! What the eyes see through gadgets and after hits, as a post-process on the last camera
//! (the view model's, so the weapon is part of the picture): night vision, tear gas, the gas
//! mask's lenses, a flashbang's white-out and afterimage, the blur of a nearby blast.
//! See `vision.wgsl`; BF2's shaders are `PostProduction_*.fx` (`TVEffect_Gradient_Tex`,
//! `WaveDistortion`, `Flashbang`, `Tinnitus`).

use bevy::{
    asset::embedded_asset,
    core_pipeline::{Core3dSystems, FullscreenShader, schedule::Core3d, tonemapping::tonemapping},
    prelude::*,
    render::{
        GpuResourceAppExt, Render, RenderApp, RenderStartup, RenderSystems,
        extract_component::{
            ComponentUniforms, DynamicUniformIndex, ExtractComponent, ExtractComponentPlugin,
            UniformComponentPlugin,
        },
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        render_asset::RenderAssets,
        render_resource::{
            BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, CachedRenderPipelineId,
            ColorTargetState, ColorWrites, Extent3d, FilterMode, FragmentState, Operations, PipelineCache,
            RenderPassColorAttachment, RenderPassDescriptor, RenderPipelineDescriptor, Sampler,
            SamplerBindingType, SamplerDescriptor, ShaderStages, ShaderType, SpecializedRenderPipeline,
            SpecializedRenderPipelines, Texture, TextureDescriptor, TextureDimension, TextureFormat,
            TextureSampleType, TextureUsages, TextureView,
            binding_types::{sampler, texture_2d, uniform_buffer},
        },
        renderer::{RenderContext, RenderDevice, ViewQuery},
        texture::{FallbackImage, GpuImage},
        view::{ExtractedView, ViewTarget},
    },
    shader::Shader,
};

pub struct VisionPlugin;

impl Plugin for VisionPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "vision.wgsl");
        app.add_plugins((
            ExtractComponentPlugin::<VisionSettings>::default(),
            UniformComponentPlugin::<VisionSettings>::default(),
            ExtractResourcePlugin::<VisionTextures>::default(),
        ));
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_gpu_resource::<SpecializedRenderPipelines<VisionPipeline>>()
            .init_gpu_resource::<Afterimage>()
            .add_systems(RenderStartup, init_pipeline)
            .add_systems(Render, prepare_pipelines.in_set(RenderSystems::Prepare))
            .add_systems(Core3d, vision.in_set(Core3dSystems::PostProcess).after(tonemapping));
    }
}

/// How much of each effect shows, on the camera that draws last. All zero: no pass.
#[derive(Component, Clone, Copy, Default, Debug, ExtractComponent, ShaderType)]
pub struct VisionSettings {
    /// Seconds, for noise and wobble.
    pub time: f32,
    pub night_vision: f32,
    /// Tear gas in the eyes.
    pub gas: f32,
    /// Looking through a gas mask.
    pub mask: f32,
    /// Flashbang: white over everything, a glow added, the burnt-in image of the flash.
    pub white: f32,
    pub glow: f32,
    pub afterimage: f32,
    /// Shaken by a blast: blurred, washed out.
    pub shock: f32,
    /// 1 on the frame the afterimage is taken.
    pub capture: u32,
}

impl VisionSettings {
    fn any(&self) -> bool {
        self.night_vision + self.gas + self.mask + self.white + self.glow + self.afterimage + self.shock > 0.001
            || self.capture != 0
    }
}

/// The night vision color ramp (a 1D texture: brightness → color).
#[derive(Resource, Clone, ExtractResource)]
pub struct VisionTextures {
    pub gradient: Handle<Image>,
}

#[derive(Resource)]
struct VisionPipeline {
    layout: BindGroupLayoutDescriptor,
    sampler: Sampler,
    fullscreen: FullscreenShader,
    shader: Handle<Shader>,
}

#[derive(Component)]
struct VisionPipelineId(CachedRenderPipelineId);

/// The picture kept at the moment of a flash.
#[derive(Resource, Default)]
struct Afterimage(Option<(Texture, TextureView, Extent3d, TextureFormat)>);

fn init_pipeline(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    fullscreen: Res<FullscreenShader>,
    asset_server: Res<AssetServer>,
) {
    let texture = || texture_2d(TextureSampleType::Float { filterable: true });
    let layout = BindGroupLayoutDescriptor::new(
        "vision bind group layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::FRAGMENT,
            (
                texture(),
                sampler(SamplerBindingType::Filtering),
                texture(),
                texture(),
                uniform_buffer::<VisionSettings>(true),
            ),
        ),
    );
    let sampler = render_device.create_sampler(&SamplerDescriptor {
        min_filter: FilterMode::Linear,
        mag_filter: FilterMode::Linear,
        ..default()
    });
    commands.insert_resource(VisionPipeline {
        layout,
        sampler,
        fullscreen: fullscreen.clone(),
        shader: asset_server.load("embedded://client/gadgets/vision.wgsl"),
    });
}

impl SpecializedRenderPipeline for VisionPipeline {
    type Key = TextureFormat;

    fn specialize(&self, format: Self::Key) -> RenderPipelineDescriptor {
        RenderPipelineDescriptor {
            label: Some("vision".into()),
            layout: vec![self.layout.clone()],
            vertex: self.fullscreen.to_vertex_state(),
            fragment: Some(FragmentState {
                shader: self.shader.clone(),
                targets: vec![Some(ColorTargetState {
                    format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
                ..default()
            }),
            ..default()
        }
    }
}

fn prepare_pipelines(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    mut pipelines: ResMut<SpecializedRenderPipelines<VisionPipeline>>,
    pipeline: Res<VisionPipeline>,
    views: Query<(Entity, &ExtractedView), With<VisionSettings>>,
) {
    for (entity, view) in &views {
        let id = pipelines.specialize(&pipeline_cache, &pipeline, view.target_format);
        commands.entity(entity).insert(VisionPipelineId(id));
    }
}

#[allow(clippy::too_many_arguments)]
fn vision(
    view: ViewQuery<(&ViewTarget, &VisionSettings, &VisionPipelineId, &DynamicUniformIndex<VisionSettings>)>,
    pipeline_cache: Res<PipelineCache>,
    pipeline: Res<VisionPipeline>,
    uniforms: Res<ComponentUniforms<VisionSettings>>,
    textures: Option<Res<VisionTextures>>,
    images: Res<RenderAssets<GpuImage>>,
    fallback: Res<FallbackImage>,
    mut afterimage: ResMut<Afterimage>,
    mut ctx: RenderContext,
) {
    let (target, settings, pipeline_id, uniform_index) = view.into_inner();
    if !settings.any() {
        return;
    }
    let (Some(render_pipeline), Some(uniform)) =
        (pipeline_cache.get_render_pipeline(pipeline_id.0), uniforms.uniforms().binding())
    else {
        return;
    };
    let gradient = textures
        .and_then(|t| images.get(&t.gradient))
        .map_or(&fallback.d2.texture_view, |image| &image.texture_view);

    // Keep (a copy of) the picture for the afterimage, sized like the screen.
    let size = target.main_texture().size();
    let format = target.main_texture_format();
    if afterimage.0.as_ref().is_none_or(|(_, _, s, f)| *s != size || *f != format) {
        let texture = ctx.render_device().create_texture(&TextureDescriptor {
            label: Some("afterimage"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&default());
        afterimage.0 = Some((texture, view, size, format));
    }
    let Some((history, history_view, ..)) = afterimage.0.as_ref() else {
        return;
    };

    let post_process = target.post_process_write();
    if settings.capture != 0 {
        ctx.command_encoder().copy_texture_to_texture(
            post_process.source_texture.as_image_copy(),
            history.as_image_copy(),
            size,
        );
    }
    let bind_group = ctx.render_device().create_bind_group(
        Some("vision bind group"),
        &pipeline_cache.get_bind_group_layout(&pipeline.layout),
        &BindGroupEntries::sequential((post_process.source, &pipeline.sampler, gradient, history_view, uniform)),
    );
    let mut pass = ctx.command_encoder().begin_render_pass(&RenderPassDescriptor {
        label: Some("vision"),
        color_attachments: &[Some(RenderPassColorAttachment {
            view: post_process.destination,
            depth_slice: None,
            resolve_target: None,
            ops: Operations::default(),
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(render_pipeline);
    pass.set_bind_group(0, &bind_group, &[uniform_index.index()]);
    pass.draw(0..3, 0..1);
}
