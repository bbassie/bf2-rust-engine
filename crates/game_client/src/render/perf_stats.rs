//! `BF2_PERF_STATS`: logs every 5 s what drives the frame's cost besides the systems' own time:
//! how many entities draw meshes (and how many of them were seen), skinned meshes and animated
//! bones, and how often materials change (a changed bindless material rewrites its whole slab
//! of up to 2048 materials on the GPU and rebuilds the slab's bind group).

use bevy::{
    animation::{AnimatedBy, AnimationTargetId},
    camera::visibility::VisibilityRange,
    light::NotShadowCaster,
    mesh::skinning::SkinnedMesh,
    prelude::*,
};

use super::materials::Bf2Material;

pub struct PerfStatsPlugin;

impl Plugin for PerfStatsPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var_os("BF2_PERF_STATS").is_some() {
            app.init_resource::<Counts>().add_systems(Last, (count_material_events, log_stats).chain());
        }
    }
}

#[derive(Resource, Default)]
struct Counts {
    frames: u32,
    since: f32,
    /// Added, modified, removed per material type.
    bf2: [u32; 3],
    standard: [u32; 3],
    /// A few of the BF2 materials modified lately.
    modified: Vec<AssetId<Bf2Material>>,
}

fn tally<A: Asset>(events: &mut MessageReader<AssetEvent<A>>, into: &mut [u32; 3]) {
    for event in events.read() {
        match event {
            AssetEvent::Added { .. } => into[0] += 1,
            AssetEvent::Modified { .. } => into[1] += 1,
            AssetEvent::Removed { .. } => into[2] += 1,
            _ => {}
        }
    }
}

fn count_material_events(
    mut counts: ResMut<Counts>,
    mut bf2: MessageReader<AssetEvent<Bf2Material>>,
    mut standard: MessageReader<AssetEvent<StandardMaterial>>,
) {
    counts.frames += 1;
    let events: Vec<_> = bf2.read().cloned().collect();
    for event in &events {
        if let AssetEvent::Modified { id } = event
            && counts.modified.len() < 8
            && !counts.modified.contains(id)
        {
            counts.modified.push(*id);
        }
        match event {
            AssetEvent::Added { .. } => counts.bf2[0] += 1,
            AssetEvent::Modified { .. } => counts.bf2[1] += 1,
            AssetEvent::Removed { .. } => counts.bf2[2] += 1,
            _ => {}
        }
    }
    tally(&mut standard, &mut counts.standard);
}

#[allow(clippy::type_complexity)]
fn log_stats(
    time: Res<Time<Real>>,
    mut counts: ResMut<Counts>,
    entities: Query<()>,
    meshes: Query<(&ViewVisibility, Has<VisibilityRange>, Has<NotShadowCaster>, Has<SkinnedMesh>), With<Mesh3d>>,
    targets: Query<(), (With<AnimationTargetId>, With<AnimatedBy>)>,
    players: Query<(), With<AnimationGraphHandle>>,
    materials: Res<Assets<Bf2Material>>,
    bodies: Query<(&avian3d::prelude::RigidBody, Has<avian3d::prelude::Sleeping>)>,
    colliders: Query<Has<avian3d::prelude::Sensor>, With<avian3d::prelude::Collider>>,
) {
    let now = time.elapsed_secs();
    if now - counts.since < 5.0 {
        return;
    }
    let frames = counts.frames.max(1) as f32;
    let (mut total, mut seen, mut ranged, mut casters, mut skinned, mut skinned_seen) = (0, 0, 0, 0, 0, 0);
    for (visibility, range, no_shadow, skin) in &meshes {
        total += 1;
        seen += visibility.get() as u32;
        ranged += range as u32;
        casters += (!no_shadow) as u32;
        skinned += skin as u32;
        skinned_seen += (skin && visibility.get()) as u32;
    }
    let per = |c: [u32; 3]| format!("+{:.1} ~{:.1} -{:.1}", c[0] as f32 / frames, c[1] as f32 / frames, c[2] as f32 / frames);
    info!(
        "perf stats: {} entities; meshes {total} ({seen} seen, {ranged} with visibility ranges, {casters} shadow casters), \
         skinned {skinned} ({skinned_seen} seen), animated targets {} of {} active players; material events per frame: \
         bf2 {}, standard {}",
        entities.iter().count(),
        targets.iter().count(),
        players.iter().count(),
        per(counts.bf2),
        per(counts.standard),
    );
    let mut kinds = [0u32; 4];
    for (body, sleeping) in &bodies {
        let index = match body {
            avian3d::prelude::RigidBody::Static => 0,
            avian3d::prelude::RigidBody::Kinematic => 1,
            avian3d::prelude::RigidBody::Dynamic => 2,
        };
        kinds[index] += 1;
        kinds[3] += sleeping as u32;
    }
    let (collider_count, sensors) = colliders.iter().fold((0, 0), |(n, s), sensor| (n + 1, s + sensor as u32));
    info!(
        "perf stats: bodies {} static, {} kinematic, {} dynamic ({} sleeping); colliders {collider_count} ({sensors} sensors)",
        kinds[0], kinds[1], kinds[2], kinds[3]
    );
    let names: Vec<String> = counts
        .modified
        .iter()
        .filter_map(|id| materials.get(*id)?.base.base_color_texture.as_ref()?.path().map(|p| p.to_string()))
        .collect();
    if !names.is_empty() {
        info!("perf stats: modified BF2 materials, e.g. {names:?}");
    }
    *counts = Counts { since: now, ..default() };
}
