//! Transform propagation on the main thread alone.
//!
//! Bevy's `mark_dirty_trees` and `propagate_parent_transforms` split their work over every
//! thread of the compute pool and wait for all of them: each spawns a task per thread, and the
//! tasks spin until the whole hierarchy is done. The render thread keeps those threads busy
//! with its own tasks (preparing and recording the frame), so the main thread mostly waits for
//! them to come round, while the work itself (a few thousand bone transforms of the soldiers
//! in view) is small. These are the same two systems, serial, in `PostUpdate` and before
//! avian's physics step in place of Bevy's. Karkand, 63 bots: 1.5 -> 0.6 ms a frame for both
//! (`game_server/profile` timings). `BF2_PERF_EXP=parprop` keeps Bevy's.

use avian3d::physics_transform::{PhysicsTransformConfig, PhysicsTransformSystems};
use bevy::{
    ecs::schedule::ScheduleCleanupPolicy,
    prelude::*,
    transform::{
        TransformSystems,
        components::TransformTreeChanged,
        systems::{StaticTransformOptimizations, mark_dirty_trees, propagate_parent_transforms, sync_simple_transforms},
    },
};

pub struct TransformPropagationPlugin;

impl Plugin for TransformPropagationPlugin {
    fn build(&self, app: &mut App) {
        if crate::perf_experiment("parprop") {
            return;
        }
        // The frame's propagation, and avian's before each physics step (the fixed tick's).
        if replace_bevys(app, PostUpdate) {
            app.add_systems(
                PostUpdate,
                (mark_dirty_trees_serial, propagate_serial)
                    .chain()
                    .before(sync_simple_transforms)
                    .in_set(TransformSystems::Propagate),
            );
        }
        if replace_bevys(app, FixedPostUpdate) {
            app.add_systems(
                FixedPostUpdate,
                (mark_dirty_trees_serial, propagate_serial)
                    .chain()
                    .before(sync_simple_transforms)
                    .in_set(PhysicsTransformSystems::Propagate)
                    .run_if(|config: Res<PhysicsTransformConfig>| config.propagate_before_physics),
            );
        }
    }
}

/// Takes Bevy's `mark_dirty_trees` and `propagate_parent_transforms` out of `schedule`.
fn replace_bevys(app: &mut App, schedule: impl bevy::ecs::schedule::ScheduleLabel + Clone) -> bool {
    for result in [
        app.remove_systems_in_set(schedule.clone(), mark_dirty_trees, ScheduleCleanupPolicy::RemoveSystemsOnly),
        app.remove_systems_in_set(schedule.clone(), propagate_parent_transforms, ScheduleCleanupPolicy::RemoveSystemsOnly),
    ] {
        match result {
            Ok(removed) if removed > 0 => {}
            other => {
                warn!("serial transform propagation: Bevy's systems not found in {schedule:?}: {other:?}");
                return false;
            }
        }
    }
    true
}

/// Bevy's `mark_dirty_trees` without threads: marks the ancestors of every changed transform,
/// so that propagation skips the trees where nothing changed.
fn mark_dirty_trees_serial(
    changed: Query<Entity, Or<(Changed<Transform>, Changed<ChildOf>, Added<GlobalTransform>)>>,
    mut orphaned: RemovedComponents<ChildOf>,
    mut transforms: Query<&mut TransformTreeChanged>,
    parents: Query<&ChildOf>,
    static_optimizations: Res<StaticTransformOptimizations>,
) {
    if !static_optimizations.is_enabled() {
        return;
    }
    for entity in changed.iter().chain(orphaned.read()) {
        let mut next = entity;
        while let Ok(mut tree) = transforms.get_mut(next) {
            if tree.is_changed() && !tree.is_added() {
                // This part of the tree was marked already.
                break;
            }
            tree.set_changed();
            match parents.get(next) {
                Ok(parent) => next = parent.parent(),
                Err(_) => break,
            }
        }
    }
}

/// Bevy's `propagate_parent_transforms` without threads, depth first from every root whose
/// tree changed.
#[allow(clippy::type_complexity)]
fn propagate_serial(
    mut roots: Query<(Ref<Transform>, &mut GlobalTransform, &Children, Ref<TransformTreeChanged>), Without<ChildOf>>,
    mut nodes: Query<(Ref<Transform>, &mut GlobalTransform, Ref<TransformTreeChanged>), With<ChildOf>>,
    hierarchy: Query<(Option<&Children>, &ChildOf)>,
    static_optimizations: Res<StaticTransformOptimizations>,
    mut stack: Local<Vec<(Entity, GlobalTransform, bool)>>,
) {
    let skip_static = static_optimizations.is_enabled();
    for (transform, mut global, children, tree) in &mut roots {
        if skip_static && !tree.is_changed() {
            continue;
        }
        *global = GlobalTransform::from(*transform);
        let changed = global.is_changed();
        stack.extend(children.iter().map(|child| (child, *global, changed)));
        while let Some((entity, parent_global, parent_changed)) = stack.pop() {
            let Ok((transform, mut global, tree)) = nodes.get_mut(entity) else {
                continue;
            };
            if skip_static && !tree.is_changed() && !parent_changed {
                continue;
            }
            global.set_if_neq(parent_global.mul_transform(*transform));
            let changed = global.is_changed();
            let global = *global;
            if let Ok((Some(children), _)) = hierarchy.get(entity) {
                stack.extend(children.iter().map(|child| (child, global, changed)));
            }
        }
    }
}
