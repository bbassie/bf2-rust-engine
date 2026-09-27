//! Hints for AI soldiers: the strategic areas of a level's layouts
//! (`levels/<name>/ai.ron`) and how bots use each weapon (`ai/weapons.ron`).
//!
//! Both are optional: without them the server derives strategic areas from a layout's
//! control points and guesses weapon ranges from the weapon itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Strategic areas of a level: `levels/<name>/ai.ron`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LevelAiDesc {
    #[serde(default)]
    pub layouts: Vec<StrategicLayoutDesc>,
}

impl LevelAiDesc {
    pub fn layout(&self, mode: &str, size: u32) -> Option<&StrategicLayoutDesc> {
        self.layouts.iter().find(|l| l.mode == mode && l.size == size)
    }
}

/// The areas the team AI reasons about in one game mode layout (BF2 `StrategicAreas.ai`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StrategicLayoutDesc {
    pub mode: String,
    pub size: u32,
    #[serde(default)]
    pub areas: Vec<StrategicAreaDesc>,
    /// Places to pass through between two areas, so squads spread over several routes.
    #[serde(default)]
    pub routes: Vec<StrategicRouteDesc>,
}

/// Usually a control point; sometimes a flank or staging position.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StrategicAreaDesc {
    pub name: String,
    /// The control point ([`crate::ControlPointDesc::id`]) the area is about.
    #[serde(default)]
    pub control_point: Option<String>,
    /// Center of the area.
    pub position: [f32; 3],
    /// Where infantry go when ordered here.
    #[serde(default)]
    pub infantry_position: Option<[f32; 3]>,
    /// Areas that are attacked from here (by name): the lines of advance.
    #[serde(default)]
    pub neighbours: Vec<String>,
}

/// Waypoints for moving between two areas (either way).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StrategicRouteDesc {
    pub from: String,
    pub to: String,
    /// Alternatives: a squad picks one to pass through.
    pub waypoints: Vec<[f32; 3]>,
}

/// How bots use weapons, by weapon name: `ai/weapons.ron` (BF2 `weaponTemplate`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AiWeaponsDesc {
    #[serde(default)]
    pub weapons: BTreeMap<String, AiWeaponDesc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AiWeaponDesc {
    /// Meters.
    pub min_range: f32,
    pub max_range: f32,
    /// Where the weapon works best, meters.
    pub optimal_range: f32,
    /// Stance to fire from when standing still.
    #[serde(default)]
    pub pose: FiringPose,
    /// Usefulness against infantry (BF2's scale: rifles 5, sniper rifles 6, grenades 2).
    #[serde(default)]
    pub infantry_strength: f32,
    /// Thrown (grenades): aimed along an arc.
    #[serde(default)]
    pub thrown: bool,
    /// Blast radius, meters, for keeping clear of friends.
    #[serde(default)]
    pub explosion_radius: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FiringPose {
    #[default]
    Standing,
    Crouching,
    Prone,
}
