//! Ladders: BF2 `Ladder` objects soldiers climb (see [`crate::soldier::step_soldier`]).
//!
//! The importer marks ladder parts of objects; loading gives their entity a [`LadderPart`],
//! and this module adds a box on the [`GameLayer::Ladder`] layer around the ladder's
//! collision mesh: a thin plate along the rails, with the wall brackets reaching back to its
//! local -Z (0.5 m for BF2's house ladders), so it is climbed on its +Z side.
//! Movement finds ladders through spatial queries, so client prediction and the server
//! always agree without replicating anything.

use avian3d::prelude::*;
use bevy::prelude::*;

use crate::physics::GameLayer;

/// A level part soldiers can climb. Its collider is the ladder's collision mesh.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LadderPart;

/// Gives a ladder part the box movement looks for.
pub(crate) fn add_ladder_volume(
    add: On<Add, LadderPart>,
    mut commands: Commands,
    parts: Query<&Collider>,
) {
    let Ok(collider) = parts.get(add.entity) else {
        return;
    };
    let aabb = collider.aabb(Vec3::ZERO, Quat::IDENTITY);
    let size = aabb.size();
    commands.spawn((
        Collider::cuboid(size.x, size.y, size.z),
        Transform::from_translation(aabb.center()),
        CollisionLayers::new(GameLayer::Ladder, LayerMask::NONE),
        ChildOf(add.entity),
    ));
}

/// A ladder in world space, from its box.
#[derive(Clone, Copy, Debug)]
pub struct Ladder {
    pub center: Vec3,
    /// Along the rails.
    pub up: Vec3,
    /// Horizontal, out of the wall towards the side it is climbed from.
    pub front: Vec3,
    /// Along the rungs.
    pub side: Vec3,
    /// Half width, half height and half depth of the box.
    pub half: Vec3,
}

impl Ladder {
    pub fn from_box(position: Vec3, rotation: Quat, half_extents: Vec3) -> Self {
        let front = rotation * Vec3::Z;
        Self {
            center: position,
            up: rotation * Vec3::Y,
            front: Vec3::new(front.x, 0.0, front.z).normalize_or(Vec3::Z),
            side: rotation * Vec3::X,
            half: half_extents,
        }
    }

    /// `point` in ladder coordinates: along the rungs, up the rails, out of the wall.
    pub fn local(&self, point: Vec3) -> Vec3 {
        let d = point - self.center;
        Vec3::new(d.dot(self.side), d.dot(self.up), d.dot(self.front))
    }

    pub fn world(&self, local: Vec3) -> Vec3 {
        self.center + self.side * local.x + self.up * local.y + self.front * local.z
    }

    /// Height of the top above the center, along the rails.
    pub fn top(&self) -> f32 {
        self.half.y
    }
}
