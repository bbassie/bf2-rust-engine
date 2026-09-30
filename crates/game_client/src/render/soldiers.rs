//! Soldier visuals: the imported BF2 soldier model that matches the soldier's actual kit (see
//! [`kit_body`]: a plain `<faction>_heavy_soldier`/`_light_soldier`, or, once the importer
//! resolved the kit's own gear, `<body>__<kit>`, e.g. `us_heavy_soldier__us_assault` — a vest,
//! pack, helmet or ghillie suit skinned onto the same body, see
//! `bf2_import::soldiers::import_kit_gear`) with movement animations and the weapon in hand,
//! or a team-colored capsule when no model is available (test range).
//!
//! Animation is layered like BF2: the legs play the soldier's movement clips, the upper
//! body plays the matching clip of the current weapon's animation set. Every change
//! crossfades: movement blends the four directional clips by direction and plays them at
//! the ground speed, turning on the spot steps the feet round, and jumps go through
//! take-off, airborne and landing clips. Shots, reloads and weapon switches play the
//! weapon's one-shots on the upper body; a switch lowers the old weapon, swaps the model
//! out of view and raises the new one. Soldiers in vehicles sit at their seat, drawn where
//! the vehicle is drawn, in their seat's pose (BF2's seat animations).
//!
//! The body's lower levels of detail (meshes `body_lod1`, ... in the model, on the same
//! skeleton) are drawn by distance, cross-fading, and soldiers with their weapons fade out
//! past BF2's cull distance for them (see `unit_lods`). Soldiers nobody sees (culled or out of
//! view) aren't posed until they are seen again: animating the skeletons of a 32-bot battle is
//! most of its animation work.

use bevy::{
    app::AnimationSystems,
    camera::primitives::MeshAabb,
    gltf::{Gltf, GltfMesh},
    platform::collections::HashMap,
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use std::sync::Arc;

use game_data::{FireKind, SoldierDesc, WeaponDesc};
use game_shared::skeleton::{self, Gait};
use game_shared::{
    config::GamePaths,
    level::LoadedLevel,
    protocol::{ControlledBy, ThrowReleased, Team},
    soldier::{SOLDIER_CENTER, SOLDIER_HEIGHT, SOLDIER_RADIUS, Soldier, Stance},
    statics::StaticMesh,
    vehicle::{Seated, VehicleData},
    weapons::{Armory, Inventory, Loadout},
};

use super::{
    blend::{BlendLayer, Clip, Play},
    materials::Bf2Materials,
    unit_lods::{UnitLod, UnitLodConfig, small_part_draw_distance},
};
use crate::{
    combat::CombatFeedback,
    net::LocalSoldier,
    prediction::{RenderStateSystems, SoldierRender},
    vehicles::{VehicleView, VehicleViewSystems},
};

pub struct SoldierRenderPlugin;

impl Plugin for SoldierRenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SoldierModels>()
            .add_systems(Startup, load_placeholder_assets)
            .add_observer(spawn_visual)
            .add_observer(despawn_visual)
            .add_systems(
                Update,
                (
                    load_team_models.run_if(resource_exists_and_changed::<LoadedLevel>),
                    attach_models,
                    attach_weapons,
                ),
            )
            .add_systems(
                PostUpdate,
                (update_visuals, sleep_unseen.before(animate), animate, show_parachutes.before(update_visuals))
                    .after(RenderStateSystems)
                    .after(VehicleViewSystems)
                    .before(AnimationSystems)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// BF2's parachute canopy (its `animatedparachute` bundle, which sits at the parachute's
/// origin; its idle animation keeps the bind pose, which is what we draw).
const PARACHUTE_MESH: &str = "objects/vehicles/air/parachute/meshes/animatedparachute.glb";
/// BF2's parachute seat relative to the canopy's origin, facing the way it glides
/// (`Parachute.tweak`: `seatInformation Parachute 0.0563561/-2.07613/0.260896`, in our axes:
/// 2.08 m below and 0.26 m ahead).
const PARACHUTE_SEAT: Vec3 = Vec3::new(0.056_356_1, -2.076_13, -0.260_896);
/// BF2 puts the root bone (the hips) of the seat's pose at the seat, not the feet: in
/// `3p_parachute` it is 0.81 m above the soldier's origin, his feet. (The same rule places
/// soldiers in vehicle seats, see `bf2_import::vehicles::pose_root`.)
const PARACHUTE_POSE_HIPS: Vec3 = Vec3::new(0.0, 0.812_738, -0.003_937);
/// Where the canopy's origin is relative to the soldier's feet, facing the way he glides:
/// 2.89 m above them and 0.26 m behind, so its risers meet the harness at his chest.
const PARACHUTE_OFFSET: Vec3 = Vec3::new(
    PARACHUTE_POSE_HIPS.x - PARACHUTE_SEAT.x,
    PARACHUTE_POSE_HIPS.y - PARACHUTE_SEAT.y,
    PARACHUTE_POSE_HIPS.z - PARACHUTE_SEAT.z,
);
/// BF2's parachute poses of the soldier: hanging, and touching down.
const PARACHUTE_POSE: &str = "3p_parachute";
const PARACHUTE_LANDING_POSE: &str = "3p_parachute_landing";
/// After touchdown: how long the landing pose holds, and how long the canopy takes to
/// collapse onto the ground (seconds).
const PARACHUTE_LANDING_TIME: f32 = 0.6;
const CANOPY_COLLAPSE_TIME: f32 = 1.6;
/// How quickly the canopy swings round to where the soldier glides (1/s), and how far it
/// banks into a turn (radians per rad/s of turning, at most `CANOPY_MAX_BANK`).
const CANOPY_TURN_SMOOTHING: f32 = 4.0;
const CANOPY_BANK: f32 = 0.5;
const CANOPY_MAX_BANK: f32 = 0.45;

/// A soldier's open parachute. Its own entity rather than part of the soldier's visual, so it
/// also shows in first person (looking up) and can collapse after he lands.
#[derive(Component)]
struct Canopy {
    soldier: Entity,
    /// The direction it glides (like a soldier's yaw) and its bank.
    heading: f32,
    bank: f32,
    /// Seconds since the soldier touched down; `None` while he hangs under it.
    collapse: Option<f32>,
}

/// Opens, flies and collapses the parachutes of soldiers who have one out. The canopy goes
/// with the soldier's parachute state (`SoldierMotion::parachute`, which the server and
/// prediction end on landing, in water, on a ladder or zipline, when he is wounded and when
/// he gets into a vehicle): after touching down on the ground or in water it collapses
/// behind him; otherwise (in a vehicle, wounded or dead in the air, on a ladder) it is gone
/// at once.
fn show_parachutes(
    mut commands: Commands,
    time: Res<Time>,
    renders: Query<(Entity, &SoldierRender, Has<Seated>)>,
    mut canopies: Query<(Entity, &mut Canopy, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (soldier, render, seated) in &renders {
        if render.parachute
            && !seated
            && !canopies.iter().any(|(_, c, _)| c.soldier == soldier && c.collapse.is_none())
        {
            let heading = glide_heading(render).unwrap_or(render.yaw);
            info!("parachute: canopy opened over {soldier}");
            commands.spawn((
                Canopy {
                    soldier,
                    heading,
                    bank: 0.0,
                    collapse: None,
                },
                canopy_transform(render.position, heading, 0.0),
                Visibility::default(),
                StaticMesh {
                    path: PARACHUTE_MESH.into(),
                    index: 0,
                },
            ));
        }
    }
    for (entity, mut canopy, mut transform) in &mut canopies {
        let render = renders.get(canopy.soldier).ok().filter(|(_, _, seated)| !seated).map(|(_, r, _)| r);
        match (canopy.collapse, render) {
            (None, Some(render)) if render.parachute => {
                // Swing round after the glide, banking into the turn.
                let wanted = glide_heading(render).unwrap_or(canopy.heading);
                let turn = wrap_angle(wanted - canopy.heading) * (1.0 - (-CANOPY_TURN_SMOOTHING * dt).exp());
                canopy.heading = wrap_angle(canopy.heading + turn);
                let rate = if dt > 0.0 { turn / dt } else { 0.0 };
                let bank = (-rate * CANOPY_BANK).clamp(-CANOPY_MAX_BANK, CANOPY_MAX_BANK);
                canopy.bank += (bank - canopy.bank) * (1.0 - (-CANOPY_TURN_SMOOTHING * dt).exp());
                *transform = canopy_transform(render.position, canopy.heading, canopy.bank);
            }
            // Touched down: the canopy collapses.
            (None, Some(render)) if render.grounded || render.swimming => {
                info!("parachute: canopy of {} collapses (landed)", canopy.soldier);
                canopy.collapse = Some(0.0);
            }
            // Into a vehicle, down or dead in the air, onto a ladder or a zipline: gone.
            (None, _) => {
                let why = match renders.get(canopy.soldier) {
                    Ok((_, _, true)) => "in a vehicle",
                    Ok(_) => "parachute ended",
                    Err(_) => "soldier gone",
                };
                info!("parachute: canopy of {} removed ({why})", canopy.soldier);
                commands.entity(entity).despawn();
                continue;
            }
            (Some(elapsed), _) => {
                // Landed: the canopy drifts on a little, sinks behind him and folds up.
                let elapsed = elapsed + dt;
                canopy.collapse = Some(elapsed);
                if elapsed >= CANOPY_COLLAPSE_TIME {
                    commands.entity(entity).despawn();
                    continue;
                }
                let t = elapsed / CANOPY_COLLAPSE_TIME;
                let forward = Quat::from_rotation_y(canopy.heading) * Vec3::NEG_Z;
                transform.translation += forward * 1.5 * (1.0 - t) * dt - Vec3::Y * PARACHUTE_OFFSET.y * dt / CANOPY_COLLAPSE_TIME;
                transform.rotation = Quat::from_rotation_y(canopy.heading) * Quat::from_rotation_x(-t * 1.2);
                transform.scale = Vec3::new(1.0, (1.0 - t).max(0.05), 1.0);
            }
        }
    }
}

/// Where a soldier's canopy hangs over his feet at `position`: its seat at his hips, banking
/// about them (the harness stays on him while the canopy leans into a turn).
fn canopy_transform(position: Vec3, heading: f32, bank: f32) -> Transform {
    let rotation = Quat::from_rotation_y(heading);
    let tilted = rotation * Quat::from_rotation_z(bank);
    let hips = position + rotation * PARACHUTE_POSE_HIPS;
    Transform::from_translation(hips - tilted * PARACHUTE_SEAT).with_rotation(tilted)
}

/// The direction a soldier glides (like a yaw), if he moves enough to tell.
fn glide_heading(render: &SoldierRender) -> Option<f32> {
    let flat = Vec2::new(render.velocity.x, render.velocity.z);
    (flat.length() > 1.0).then(|| (-flat.x).atan2(-flat.y))
}

fn wrap_angle(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (a + PI).rem_euclid(TAU) - PI
}

/// Clips by lowercase BF2 file name: the body's movement clips (legs) and the weapon sets'
/// upper-body clips.
mod clips {
    pub const STAND: &str = "3p_stand";
    pub const CROUCH: &str = "3p_crouchstill";
    pub const PRONE: &str = "3p_pronestill";
    pub const SPRINT: &str = "3p_sprint";
    /// Getting up from lying on the back after a revive. Its first frame, held, is how a
    /// critically wounded soldier lies.
    pub const REVIVE: &str = "3p_reviveonback";
    /// Directional sets, BF2's movement bundles: forward, backward, left, right.
    pub const WALK: [&str; 4] = ["3p_walkforward", "3p_walkbackward", "3p_walkleft", "3p_walkright"];
    pub const RUN: [&str; 4] = ["3p_runforward", "3p_runbackward", "3p_strafeleft", "3p_straferight"];
    pub const CROUCH_MOVE: [&str; 4] =
        ["3p_crouchforward", "3p_crouchbackward", "3p_crouchstrafeleft", "3p_crouchstraferight"];
    pub const PRONE_MOVE: [&str; 4] =
        ["3p_proneforward", "3p_pronebackward", "3p_pronestrafeleft", "3p_pronestraferight"];
    /// Stepping round on the spot, left and right. Prone, BF2 turns with the strafe clips
    /// (the left one backwards).
    pub const STAND_TURN: [&str; 2] = ["3p_standturnleft", "3p_standturnright"];
    pub const CROUCH_TURN: [&str; 2] = ["3p_crouchturnleft", "3p_crouchturnright"];
    /// Take-off, airborne loop and landing per jump direction (see [`super::jump_direction`]).
    pub const JUMP: [[&str; 3]; 5] = [
        ["3p_stilljumpstart", "3p_stilljumploop", "3p_stilljumpend"],
        ["3p_runforwardjumpstart", "3p_runforwardjumploop", "3p_runforwardjumpend"],
        ["3p_runbackwardjumpstart", "3p_runbackwardjumploop", "3p_runbackwardjumpend"],
        ["3p_strafeleftjumpstart", "3p_strafeleftjumploop", "3p_strafeleftjumpend"],
        ["3p_straferightjumpstart", "3p_straferightjumploop", "3p_straferightjumpend"],
    ];

    /// Walk, run, sprint and crawl cycles: they all start with the left foot forward, so
    /// switching between them keeps the step phase.
    pub fn cycles() -> impl Iterator<Item = &'static str> {
        WALK.into_iter().chain(RUN).chain(CROUCH_MOVE).chain(PRONE_MOVE).chain([SPRINT])
    }

    /// Climbing a ladder, and sliding down one (BF2's `objects/common/ladder` clips).
    pub const CLIMB: &str = "3p_climbup";
    pub const SLIDE: &str = "3p_climbdownfast";
    /// Climbing a grappling rope, holding still on one, and hanging from a zipline (Special
    /// Forces soldiers only: BF2 plays them through the rope's and zipline's "seats").
    pub const ROPE_CLIMB: &str = "grapplehook_climb2";
    pub const ROPE_HOLD: &str = "grapplehook_climb2_pause";
    pub const ZIPLINE: &str = "xpak_zipline_hang";

    /// Swimming at the surface (BF2's `objects/soldiers/common` swim clips): treading water,
    /// stroking forward or backward, and sprint-swimming. There's no strafe clip; BF2 doesn't
    /// have one either.
    pub const SWIM_STILL: &str = "3p_swimstill";
    pub const SWIM_FORWARD: &str = "3p_swim";
    pub const SWIM_BACKWARD: &str = "3p_swimbackward";
    pub const SWIM_SPRINT: &str = "3p_swimsprint";

    pub fn legs() -> impl Iterator<Item = &'static str> {
        [
            STAND, CROUCH, PRONE, CLIMB, SLIDE, REVIVE, ROPE_CLIMB, ROPE_HOLD, ZIPLINE,
            SWIM_STILL, SWIM_FORWARD, SWIM_BACKWARD, SWIM_SPRINT,
        ]
        .into_iter()
        .chain(cycles())
        .chain(STAND_TURN)
        .chain(CROUCH_TURN)
        .chain(JUMP.into_iter().flatten())
    }

    /// Upper-body clips named like the movement clip they pair with (`3p_crouchstill` with
    /// `crouchstill`); anything else (jumps) pairs with `stand`.
    pub const UPPER: &[&str] = &[
        "stand", "crouchstill", "pronestill", "sprint",
        "walkforward", "walkbackward", "walkleft", "walkright",
        "runforward", "runbackward", "strafeleft", "straferight",
        "crouchforward", "crouchbackward", "crouchstrafeleft", "crouchstraferight",
        "proneforward", "pronebackward", "pronestrafeleft", "pronestraferight",
        "standturnleft", "standturnright", "crouchturnleft", "crouchturnright",
    ];
    /// Upper-body one-shots, standing (and crouched) and prone.
    pub const DEPLOY: [&str; 2] = ["standdeploy", "pronedeploy"];
    pub const FIRE: [&str; 2] = ["standfire", "pronefire"];
    pub const RELOAD: [&str; 2] = ["reload", "pronereload"];

    pub fn upper_for(legs: &str) -> &'static str {
        let state = legs.trim_start_matches("3p_");
        UPPER.iter().copied().find(|&u| u == state).unwrap_or("stand")
    }
}

/// Ground speeds (m/s) at which the movement clips play at normal speed, from BF2's
/// animation value holders (they match the foot speed in the clips).
const WALK_SPEED: f32 = 1.5;
const RUN_SPEED: f32 = 3.9;
const SPRINT_SPEED: f32 = 6.3;
const CROUCH_SPEED: f32 = 1.7;
const PRONE_SPEED: f32 = 0.7;
/// Swimming and sprint-swimming, from BF2's animation value holders (matches
/// `SoldierTuning::swim_speed`/`swim_sprint_speed`).
const SWIM_SPEED: f32 = 1.9;
const SWIM_SPRINT_SPEED: f32 = 2.4;
/// Climbing speed the ladder clip is made for (its feet move about 1.25 m/s).
const CLIMB_SPEED: f32 = 1.25;
/// Climbing speed the grappling rope clip is made for (about a meter per cycle).
const ROPE_CLIMB_SPEED: f32 = 1.0;
/// The rope and zipline clips hold the body lower than standing: the model goes up by this.
/// BF2 places them at the rope's or the zipline handle's "seat".
const ROPE_BODY_RAISE: f32 = 0.66;

/// Crossfade times (seconds), roughly BF2's bundle fade times.
const FADE: f32 = 0.2;
const FADE_START_MOVING: f32 = 0.15;
const FADE_STANCE: f32 = 0.3;
const FADE_PRONE: f32 = 0.4;
const FADE_JUMP: f32 = 0.1;
const FADE_TURN_IN: f32 = 0.1;
const FADE_TURN_OUT: f32 = 0.25;
/// Upper-body one-shots: a weapon switch first lowers the old weapon over `FADE_DEPLOY_IN`
/// (into the deploy clip's first, lowest frame), swaps the model and then plays the clip.
const FADE_DEPLOY_IN: f32 = 0.15;
const FADE_FIRE_IN: f32 = 0.05;
const FADE_RELOAD_IN: f32 = 0.15;
const FADE_ACTION_OUT: f32 = 0.2;

/// Turning on the spot faster than this (rad/s) steps the feet round; slower than
/// `TURN_STOP` ends it.
const TURN_START: f32 = 1.0;
const TURN_STOP: f32 = 0.4;
/// Turning speed (rad/s) at which the turn clips play at normal speed (BF2's value holder).
const TURN_SPEED: f32 = 1.0;
/// How quickly the measured turning speed follows the yaw (per second).
const TURN_SMOOTHING: f32 = 6.0;

/// Upward speed that means the soldier jumped rather than stepped off something.
const TAKE_OFF_SPEED: f32 = 1.0;
/// Time off the ground before a fall (not a jump) plays the airborne loop.
const FALL_TIME: f32 = 0.25;
/// Shorter jumps end without a landing.
const MIN_AIR_TIME: f32 = 0.15;
/// How long a landing plays before movement takes over, standing still and moving.
const LAND_TIME: f32 = 0.35;
const LAND_TIME_MOVING: f32 = 0.12;

/// Upper-body set for weapons without their own.
const DEFAULT_WEAPON_ANIMATIONS: &str = game_shared::skeleton::DEFAULT_WEAPON_SET;

/// Loaded soldier models and animation graphs, keyed by soldier body name (`KitSlot::soldier`:
/// a plain `<faction>_heavy_soldier` or, once a kit's gear could be resolved, the combined
/// `<body>__<kit>` the importer writes; see `bf2_import::soldiers::import_kit_gear`). Every
/// kit slot a team actually uses gets its own entry, so each class can look like it does in
/// BF2 instead of the whole team sharing one kit's body.
#[derive(Resource, Default)]
struct SoldierModels {
    /// Loaded body model by soldier name.
    bodies: HashMap<String, Handle<Gltf>>,
    /// Where each body's LODs start and how far it is drawn, by soldier name.
    lods: HashMap<String, BodyLods>,
    /// Weapon upper-body animation sets by path.
    weapon_sets: HashMap<String, Handle<Gltf>>,
    /// One animation graph per soldier name (all clips are baked into every body's own glb,
    /// so there's no sharing to be had between two different bodies).
    graphs: HashMap<String, ModelAnimations>,
}

/// Where a body's levels of detail start (the first at 0; `body`, `body_lod1`, ...) and how
/// far the soldier is drawn, before scaling.
#[derive(Clone, Debug)]
struct BodyLods {
    starts: Arc<[f32]>,
    draw_distance: Option<f32>,
    /// The soldier's cull radius, which small parts (weapons) are measured against.
    cull_radius: f32,
}

impl BodyLods {
    /// The level of detail a mesh of the model draws, by its (or its node's) name.
    fn level(name: &str) -> Option<usize> {
        let node = name.split('.').next()?;
        match node {
            "body" => Some(0),
            _ => node.strip_prefix("body_lod")?.parse().ok(),
        }
    }

    fn lod(&self, level: usize) -> UnitLod {
        UnitLod {
            starts: self.starts.clone(),
            draw_distance: self.draw_distance,
            level,
        }
    }
}

/// A team model's animation graph: its movement clips plus the upper-body clips of every
/// weapon set used so far, so a weapon switch crossfades within one graph.
struct ModelAnimations {
    graph: Handle<AnimationGraph>,
    legs: HashMap<&'static str, Clip>,
    /// Nodes of [`clips::cycles`].
    cycles: Vec<AnimationNodeIndex>,
    /// Per weapon set path, its clips of [`clips::UPPER`] and the one-shots.
    upper: HashMap<String, HashMap<&'static str, Clip>>,
    /// Every other clip of the body (the vehicle seat poses), by name.
    others: HashMap<String, Clip>,
}

#[derive(Resource)]
struct PlaceholderAssets {
    body: Handle<Mesh>,
    visor: Handle<Mesh>,
    visor_material: Handle<StandardMaterial>,
    /// Spectator/unknown, team one, team two.
    team_materials: [Handle<StandardMaterial>; 3],
}

/// The visual entity drawn for a soldier. Kept separate from the simulated entity so
/// smoothing never moves the hitbox.
#[derive(Component)]
pub(crate) struct SoldierVisual {
    soldier: Entity,
}

/// Which body is attached to a visual: `Some(soldier name)` for a model, `None` for the
/// capsule.
#[derive(Component)]
struct AttachedBody(Option<String>);

/// Parts of an attached model found when its scene spawned.
#[derive(Component)]
struct ModelRig {
    player: Entity,
    /// Weapon part bones `mesh1..mesh8`.
    weapon_bones: [Option<Entity>; 8],
    /// The body's meshes (every LOD), to tell whether anyone sees the soldier.
    meshes: Vec<Entity>,
}

/// Animation state of a visual.
#[derive(Component, Default)]
struct SoldierAnimator {
    /// Graph given to the player.
    graph: Option<AssetId<AnimationGraph>>,
    /// Weapon set the upper body plays.
    set: String,
    legs: BlendLayer,
    upper: BlendLayer,
    state: Option<Legs>,
    /// Seconds in `state`.
    state_time: f32,
    /// Seconds since the soldier last stood on the ground.
    airborne: f32,
    /// Last yaw and the smoothed turning speed (rad/s, positive to the left).
    yaw: Option<f32>,
    yaw_rate: f32,
    /// One-shot playing on the upper body over the movement clips.
    action: Option<Action>,
    /// Weapon model to show, when it isn't the active weapon yet: during a switch the old
    /// one stays in the hands until it has been lowered.
    hand: Option<Arc<WeaponDesc>>,
    /// Hands busy (on a ladder, or down): no weapon shown.
    stowed: bool,
    /// Our own soldier: shots fired so far (others' arrive as `ShotFired`, see `combat::ShotSeen`).
    shots_seen: Option<u32>,
    /// Everyone else: seconds since his last shot, and into his reload, last frame.
    fire_seen: Option<f32>,
    reload_seen: Option<f32>,
    was_reloading: bool,
    /// Nobody sees the soldier: its graph, taken off the animation player meanwhile.
    sleeping: Option<Handle<AnimationGraph>>,
    /// The soldier's own copy of the team graph (see [`OwnGraph`]).
    own: Option<OwnGraph>,
}

/// A soldier's own copy of its team's animation graph, with the same nodes (so the team
/// graph's [`Clip`]s address it) but only the clips it is playing linked to the root. Bevy
/// evaluates every node linked to the root for every bone, playing or not, and the team graph
/// holds the body's 129 clips plus every weapon set's (hundreds of nodes): with them all linked
/// posing a 32-bot battle took about 5 ms a frame. Unlinked clips still advance; a clip is
/// linked when it starts and posed from the next frame on (clips start faded out anyway).
struct OwnGraph {
    handle: Handle<AnimationGraph>,
    /// Clips linked to the root, sorted.
    linked: Vec<AnimationNodeIndex>,
}

impl OwnGraph {
    /// A copy of `template` with nothing linked to its root.
    fn new(mut graph: AnimationGraph, graphs: &mut Assets<AnimationGraph>) -> Self {
        graph.graph.clear_edges();
        Self {
            handle: graphs.add(graph),
            linked: Vec::new(),
        }
    }

    /// Links exactly the clips the player is playing (with any weight), and takes over the
    /// clips added to the team graph since (weapon sets load as they are first used).
    fn sync(&mut self, player: &AnimationPlayer, template: Option<&Handle<AnimationGraph>>, graphs: &mut Assets<AnimationGraph>) {
        let mut playing: Vec<AnimationNodeIndex> = player
            .playing_animations()
            .filter(|(_, active)| active.weight() > 0.0)
            .map(|(node, _)| *node)
            .collect();
        playing.sort_unstable();
        let own_nodes = graphs.get(&self.handle).map_or(0, |own| own.graph.node_count());
        let added: Vec<_> = template
            .and_then(|template| graphs.get(template))
            .filter(|template| template.graph.node_count() > own_nodes)
            .map(|template| template.graph.node_weights().skip(own_nodes).cloned().collect())
            .unwrap_or_default();
        if playing == self.linked && added.is_empty() {
            return;
        }
        let Some(mut graph) = graphs.get_mut(&self.handle) else {
            return;
        };
        for node in added {
            graph.graph.add_node(node);
        }
        graph.graph.clear_edges();
        let root = graph.root;
        for &node in &playing {
            if node != root && node.index() < graph.graph.node_count() {
                graph.add_edge(root, node);
            }
        }
        self.linked = playing;
    }
}

/// An upper-body one-shot.
#[derive(Clone, Copy, Debug)]
struct Action {
    kind: ActionKind,
    clip: Clip,
    /// Seconds since it started.
    time: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ActionKind {
    Deploy,
    Fire,
    Reload,
}

impl Action {
    /// A weapon switch still lowering the old weapon.
    fn lowering(&self) -> bool {
        self.kind == ActionKind::Deploy && self.time < FADE_DEPLOY_IN
    }
}

/// What the legs do.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Legs {
    Still(Stance),
    /// Turning on the spot, to the left if `true`.
    Turn(Stance, bool),
    /// Standing, slower than a run.
    Walk,
    Move(Stance),
    Sprint,
    /// Take-off, then the airborne loop (by [`jump_direction`]).
    Jump(usize),
    /// Stepped off something: straight into the airborne loop.
    Fall(usize),
    Land(usize),
    /// On a ladder: climbing, or sliding down it if `true`.
    Climb(bool),
    /// On a grappling rope.
    Rope,
    /// Hanging from a zipline.
    Hang,
    /// Treading water, or stroking forward (`false`) or backward (`true`).
    SwimStill,
    Swim(bool),
    SwimSprint,
    /// Critically wounded, lying where he fell.
    Down,
    /// Revived: getting up.
    GetUp,
}

impl Legs {
    fn stance(self) -> Stance {
        match self {
            Legs::Still(stance) | Legs::Turn(stance, _) | Legs::Move(stance) => stance,
            Legs::Down | Legs::GetUp => Stance::Prone,
            _ => Stance::Standing,
        }
    }
}

/// The weapon model currently in the soldier's hands.
#[derive(Component)]
pub(crate) struct HeldWeapon {
    name: String,
    gltf: Option<Handle<Gltf>>,
    /// The weapon's meshes (children of the weapon bones).
    pub(crate) parts: Vec<Entity>,
    spawned: bool,
}

/// On a soldier: its [`SoldierVisual`].
#[derive(Component)]
pub(crate) struct VisualOf(pub(crate) Entity);

fn load_placeholder_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut team = |r, g, b| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(r, g, b),
            perceptual_roughness: 0.8,
            ..default()
        })
    };
    let team_materials = [team(0.6, 0.6, 0.6), team(0.25, 0.35, 0.7), team(0.7, 0.3, 0.2)];
    commands.insert_resource(PlaceholderAssets {
        body: meshes.add(Capsule3d::new(SOLDIER_RADIUS, SOLDIER_HEIGHT - 2.0 * SOLDIER_RADIUS)),
        visor: meshes.add(Cuboid::new(0.36, 0.12, 0.2)),
        visor_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.08, 0.08, 0.08),
            perceptual_roughness: 0.3,
            ..default()
        }),
        team_materials,
    });
}

fn load_team_models(
    level: Res<LoadedLevel>,
    paths: Res<GamePaths>,
    asset_server: Res<AssetServer>,
    config: Res<UnitLodConfig>,
    mut models: ResMut<SoldierModels>,
) {
    models.graphs = default();
    models.bodies = default();
    models.lods = default();
    // Every distinct soldier body any kit slot of either team wears (usually one per kit,
    // fewer if several kits share a body, e.g. a mod that never resolved that kit's gear).
    let mut names: Vec<&str> = level
        .desc
        .teams
        .iter()
        .flat_map(|team| &team.kits)
        .map(|kit| kit.soldier.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let Some(desc) = paths
            .read_ron::<SoldierDesc>(format!("soldiers/{name}.ron"))
            .map_err(|err| warn!("soldier model {name}: {err:#}"))
            .ok()
        else {
            continue;
        };
        models.bodies.insert(name.to_string(), asset_server.load(format!("imported://{}", desc.mesh)));
        // Without LODs: one level (the full-detail body) drawn at any distance.
        let lods = match config.enabled {
            true => BodyLods {
                starts: std::iter::once(0.0).chain(desc.lods.iter().copied()).collect(),
                draw_distance: desc.draw_distance,
                cull_radius: desc.cull_radius,
            },
            false => BodyLods {
                starts: Arc::from([0.0]),
                draw_distance: None,
                cull_radius: 0.0,
            },
        };
        models.lods.insert(name.to_string(), lods);
    }
}

/// A soldier body's animations, built once its file has loaded. The weapon set is added to
/// the graph once its file has loaded too (check `upper` for it).
#[allow(clippy::too_many_arguments)]
fn body_animations<'a>(
    models: &'a mut SoldierModels,
    body: &str,
    set: &str,
    asset_server: &AssetServer,
    gltfs: &Assets<Gltf>,
    clip_assets: &Assets<AnimationClip>,
    graphs: &mut Assets<AnimationGraph>,
) -> Option<&'a ModelAnimations> {
    if !models.graphs.contains_key(body) {
        let gltf = gltfs.get(models.bodies.get(body)?)?;
        let mut graph = AnimationGraph::new();
        let mut legs = HashMap::default();
        for name in clips::legs() {
            if let Some(handle) = gltf.named_animations.get(name) {
                legs.insert(name, add_clip(&mut graph, handle, clip_assets));
            }
        }
        let cycles = clips::cycles().filter_map(|name| legs.get(name)).map(|c| c.node).collect();
        let others = gltf
            .named_animations
            .iter()
            .filter(|(name, _)| !legs.contains_key(name.as_ref()))
            .map(|(name, handle)| (name.to_string(), add_clip(&mut graph, handle, clip_assets)))
            .collect();
        models.graphs.insert(
            body.to_string(),
            ModelAnimations {
                graph: graphs.add(graph),
                legs,
                cycles,
                upper: HashMap::default(),
                others,
            },
        );
    }
    let animations = models.graphs.get_mut(body)?;
    if !animations.upper.contains_key(set) {
        preload_set(&mut models.weapon_sets, set, asset_server);
        let handle = &models.weapon_sets[set];
        if let (Some(weapon), Some(mut graph)) = (gltfs.get(handle), graphs.get_mut(&animations.graph)) {
            let mut upper = HashMap::default();
            let one_shots = [clips::DEPLOY, clips::FIRE, clips::RELOAD];
            for &name in clips::UPPER.iter().chain(one_shots.iter().flatten()) {
                if let Some(handle) = weapon.named_animations.get(name) {
                    upper.insert(name, add_clip(&mut graph, handle, clip_assets));
                }
            }
            animations.upper.insert(set.to_string(), upper);
        }
    }
    Some(animations)
}

/// Starts loading a weapon's upper-body set, so switching to the weapon animates at once.
fn preload_set(sets: &mut HashMap<String, Handle<Gltf>>, set: &str, asset_server: &AssetServer) {
    if !sets.contains_key(set) {
        sets.insert(set.to_string(), asset_server.load(format!("imported://{set}")));
    }
}

fn add_clip(graph: &mut AnimationGraph, handle: &Handle<AnimationClip>, clips: &Assets<AnimationClip>) -> Clip {
    let root = graph.root;
    Clip {
        node: graph.add_clip(handle.clone(), 1.0, root),
        duration: clips.get(handle).map_or(1.0, |clip| clip.duration()),
    }
}

fn spawn_visual(add: On<Add, Soldier>, mut commands: Commands) {
    let visual = commands
        .spawn((
            SoldierVisual {
                soldier: add.entity,
            },
            Transform::default(),
            Visibility::default(),
        ))
        .id();
    commands.entity(add.entity).insert(VisualOf(visual));
}

fn despawn_visual(remove: On<Remove, Soldier>, mut commands: Commands, visuals: Query<&VisualOf>) {
    if let Ok(VisualOf(visual)) = visuals.get(remove.entity) {
        commands.entity(*visual).try_despawn();
    }
}

fn soldier_team(soldier: Entity, controllers: &Query<&ControlledBy>, teams: &Query<&Team>) -> Team {
    controllers
        .get(soldier)
        .ok()
        .and_then(|c| teams.get(c.0).ok())
        .copied()
        .unwrap_or_default()
}

/// The soldier body a kit actually wears: the team's kit slot matching `kit` (the equipped
/// kit's name, from `Loadout::kit`), or, before a kit is chosen (or for a kit name the level
/// doesn't know), its first kit slot, same as BF2 shows a default body before spawn.
pub(crate) fn kit_body(level: &LoadedLevel, team_index: usize, kit: Option<&str>) -> Option<String> {
    let kits = &level.desc.teams.get(team_index)?.kits;
    let slot = kit
        .and_then(|kit| kits.iter().find(|slot| slot.kit.eq_ignore_ascii_case(kit)))
        .or_else(|| kits.first())?;
    (!slot.soldier.is_empty()).then(|| slot.soldier.clone())
}

/// Gives each visual its body once the team (and, from its equipped kit, its model) is known.
fn attach_models(
    mut commands: Commands,
    visuals: Query<(Entity, &SoldierVisual, Option<&AttachedBody>)>,
    controllers: Query<&ControlledBy>,
    teams: Query<&Team>,
    loadouts: Query<&Loadout>,
    // Not loaded yet outside a match (the main menu): every soldier (there are none) would
    // otherwise show as a capsule anyway, same as a kit the level doesn't know.
    level: Option<Res<LoadedLevel>>,
    models: Res<SoldierModels>,
    gltfs: Res<Assets<Gltf>>,
    placeholder: Res<PlaceholderAssets>,
) {
    for (entity, visual, attached) in &visuals {
        let team = soldier_team(visual.soldier, &controllers, &teams);
        let team_index = match team {
            Team::One => Some(0),
            Team::Two => Some(1),
            Team::Spectator => None,
        };
        let kit = loadouts.get(visual.soldier).ok().map(|l| l.kit.as_str());
        let body_name = level.as_deref().zip(team_index).and_then(|(level, i)| kit_body(level, i, kit));
        let model = body_name.as_deref().and_then(|name| models.bodies.get(name).map(|m| (name.to_string(), m)));
        let wanted = model.as_ref().map(|(name, _)| name.clone());
        if attached.is_some_and(|a| a.0 == wanted) {
            continue;
        }
        // Wait for the model to load before swapping out the capsule.
        let scene = model.and_then(|(_, m)| gltfs.get(m)).and_then(|g| g.default_scene.clone());
        if wanted.is_some() && scene.is_none() {
            if attached.is_none() {
                attach_capsule(&mut commands, entity, team, &placeholder);
            }
            continue;
        }
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .remove::<(ModelRig, SoldierAnimator, HeldWeapon)>()
            .insert(AttachedBody(wanted.clone()));
        let Some(scene) = scene else {
            attach_capsule(&mut commands, entity, team, &placeholder);
            continue;
        };
        let lods = wanted.as_deref().and_then(|name| models.lods.get(name).cloned());
        commands.spawn((WorldAssetRoot(scene), ChildOf(entity))).observe(
            move |ready: On<WorldInstanceReady>,
                  mut commands: Commands,
                  children: Query<&Children>,
                  players: Query<(), With<AnimationPlayer>>,
                  meshes: Query<(), With<Mesh3d>>,
                  names: Query<&Name>,
                  parents: Query<&ChildOf>| {
                let mut player = None;
                let mut weapon_bones = [None; 8];
                let mut body_meshes = Vec::new();
                for descendant in children.iter_descendants(ready.entity) {
                    if players.contains(descendant) {
                        player = Some(descendant);
                    }
                    if meshes.contains(descendant) {
                        body_meshes.push(descendant);
                    }
                    // Body meshes are drawn in their LOD's distance band.
                    if let Some(lods) = &lods
                        && meshes.contains(descendant)
                    {
                        let name = |e: Entity| names.get(e).ok().and_then(|n| BodyLods::level(n.as_str()));
                        let level = name(descendant).or_else(|| parents.get(descendant).ok().and_then(|p| name(p.parent())));
                        match level {
                            // LODs off: only the full-detail body.
                            Some(level) if level >= lods.starts.len() => {
                                commands.entity(descendant).insert(Visibility::Hidden);
                            }
                            Some(level) => {
                                commands.entity(descendant).insert(lods.lod(level));
                            }
                            None => {}
                        }
                    }
                    if let Ok(name) = names.get(descendant)
                        && let Some(n) = name.as_str().strip_prefix("mesh").and_then(|n| n.parse::<usize>().ok())
                        && (1..=8).contains(&n)
                    {
                        weapon_bones[n - 1] = Some(descendant);
                    }
                }
                let (Some(player), Ok(visual)) = (player, parents.get(ready.entity)) else {
                    return;
                };
                commands.entity(visual.parent()).insert((
                    ModelRig {
                        player,
                        weapon_bones,
                        meshes: body_meshes,
                    },
                    SoldierAnimator::default(),
                ));
            },
        );
    }
}

fn attach_capsule(commands: &mut Commands, visual: Entity, team: Team, assets: &PlaceholderAssets) {
    let material = match team {
        Team::Spectator => 0,
        Team::One => 1,
        Team::Two => 2,
    };
    commands
        .entity(visual)
        .despawn_related::<Children>()
        .insert(AttachedBody(None))
        .with_children(|parent| {
            parent.spawn((
                Mesh3d(assets.body.clone()),
                MeshMaterial3d(assets.team_materials[material].clone()),
                Transform::from_translation(SOLDIER_CENTER),
            ));
            parent.spawn((
                Mesh3d(assets.visor.clone()),
                MeshMaterial3d(assets.visor_material.clone()),
                Transform::from_xyz(0.0, SOLDIER_HEIGHT - 0.25, -SOLDIER_RADIUS + 0.02),
            ));
        });
}

/// The active weapon of a soldier.
fn active_weapon<'a>(
    armory: &'a Armory,
    loadout: Option<&Loadout>,
    inventory: Option<&Inventory>,
) -> Option<&'a Arc<WeaponDesc>> {
    let (loadout, inventory) = (loadout?, inventory?);
    armory.weapon(loadout.weapons.get(inventory.active as usize)?)
}

/// Puts the soldier's weapon model on the weapon bones (part `n` on `mesh{n+1}`): the
/// active weapon, or the one the animator still has in hand during a switch.
#[allow(clippy::type_complexity)]
fn attach_weapons(
    mut commands: Commands,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: Bf2Materials,
    models: Res<SoldierModels>,
    meshes_assets: Res<Assets<Mesh>>,
    soldiers: Query<(Option<&Loadout>, Option<&Inventory>)>,
    mut visuals: Query<(
        Entity,
        &SoldierVisual,
        &ModelRig,
        Option<&SoldierAnimator>,
        Option<&mut HeldWeapon>,
        Option<&AttachedBody>,
    )>,
) {
    for (entity, visual, rig, animator, held, body) in &mut visuals {
        let Ok((loadout, inventory)) = soldiers.get(visual.soldier) else {
            continue;
        };
        let weapon = if animator.is_some_and(|a| a.stowed) {
            None
        } else {
            animator
                .and_then(|a| a.hand.as_ref())
                .or_else(|| active_weapon(&armory, loadout, inventory))
        };
        let name = weapon.map_or(String::new(), |w| w.name.clone());
        let mut held = match held {
            Some(held) if held.name == name => held,
            Some(mut held) => {
                for part in held.parts.drain(..) {
                    commands.entity(part).try_despawn();
                }
                held.name = name;
                held.gltf = weapon.and_then(|w| w.mesh_3p.as_ref()).map(|p| asset_server.load(format!("imported://{p}")));
                held.spawned = false;
                held
            }
            None => {
                commands.entity(entity).insert(HeldWeapon {
                    name,
                    gltf: weapon.and_then(|w| w.mesh_3p.as_ref()).map(|p| asset_server.load(format!("imported://{p}"))),
                    parts: Vec::new(),
                    spawned: false,
                });
                continue;
            }
        };
        if held.spawned {
            continue;
        }
        let Some(gltf) = held.gltf.as_ref().and_then(|h| gltfs.get(h)) else {
            if held.gltf.is_none() {
                held.spawned = true;
            }
            continue;
        };
        let meshes: Vec<_> = gltf
            .meshes
            .iter()
            .enumerate()
            .filter_map(|(index, mesh)| Some((rig.weapon_bones.get(index).copied().flatten()?, gltf_meshes.get(mesh)?)))
            .collect();
        // Wait until the glTF's materials are ready.
        let Some(part_materials) = meshes
            .iter()
            .flat_map(|(_, mesh)| &mesh.primitives)
            .map(|primitive| materials.for_primitive(primitive))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut part_materials = part_materials.into_iter();
        let mut parts = Vec::new();
        // The weapon fades out with the soldier, or sooner as a small part of him (BF2 culls
        // carried weapons on their own).
        let bounds = meshes
            .iter()
            .flat_map(|(_, mesh)| &mesh.primitives)
            .filter_map(|primitive| meshes_assets.get(&primitive.mesh)?.compute_aabb())
            .fold(None, |acc: Option<(Vec3, Vec3)>, aabb| {
                let (min, max) = (Vec3::from(aabb.min()), Vec3::from(aabb.max()));
                Some(acc.map_or((min, max), |(a, b)| (a.min(min), b.max(max))))
            });
        let radius = bounds.map_or(0.0, |(min, max)| (max - min).length() * 0.5);
        let lod = body.and_then(|b| b.0.as_deref()).and_then(|name| models.lods.get(name)).map(|l| UnitLod {
            starts: Arc::from([0.0]),
            draw_distance: small_part_draw_distance(radius, l.cull_radius).or(l.draw_distance),
            level: 0,
        });
        for (bone, mesh) in meshes {
            for (primitive, material) in mesh.primitives.iter().zip(part_materials.by_ref()) {
                let mut part = commands.spawn((Mesh3d(primitive.mesh.clone()), MeshMaterial3d(material), ChildOf(bone)));
                if let Some(lod) = &lod {
                    part.insert(lod.clone());
                }
                parts.push(part.id());
            }
        }
        held.parts = parts;
        held.spawned = true;
    }
}

fn update_visuals(
    third_person: Res<crate::camera::ThirdPerson>,
    soldiers: Query<(&SoldierRender, Has<LocalSoldier>, Option<&Seated>, Has<game_shared::revive::Downed>)>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    mut visuals: Query<(Entity, &SoldierVisual, &mut Transform, &mut Visibility, Option<&SoldierAnimator>)>,
    canopies: Query<&Canopy>,
    tuning: Res<game_shared::soldier::SoldierTuning>,
    mut frame: Local<u32>,
) {
    let zipline_hang = tuning.zipline_hang;
    *frame = frame.wrapping_add(1);
    for (entity, visual, mut transform, mut visibility, animator) in &mut visuals {
        let Ok((render, local, seated, downed)) = soldiers.get(visual.soldier) else {
            continue;
        };
        // In a vehicle: at the seat where the vehicle is drawn; not at all inside a closed
        // hull (seats without a soldier position).
        let seat = seated.and_then(|s| {
            let (view, data) = vehicles.get(s.vehicle).ok()?;
            let model = &data.0;
            model.desc.seats.get(s.seat as usize)?.soldier.as_ref()?;
            let transforms = model.part_transforms(&view.joints);
            Some(view.transform * model.seat_transform(&transforms, s.seat as usize))
        });
        // First person: don't draw our own body inside the camera (unless we're down: the
        // camera looks at it then).
        let hidden = (local && !third_person.0 && !downed) || (seated.is_some() && seat.is_none());
        visibility.set_if_neq(if hidden { Visibility::Hidden } else { Visibility::Inherited });
        // Moving a soldier moves its whole skeleton (about a hundred transforms to propagate
        // and meshes to upload). Nobody sees a sleeping one (see `sleep_unseen`), so it only
        // needs to be about where it is, to be seen again when it comes into view: move it
        // every fourth frame.
        let sleeping = animator.is_some_and(|a| a.sleeping.is_some());
        if sleeping && (frame.wrapping_add(entity.index_u32())) % 4 != 0 {
            continue;
        }
        transform.set_if_neq(seat.unwrap_or_else(|| {
            // The zipline clip hangs the body from the handle, the rope clip holds it low.
            let raise = match (render.riding, render.climbing && render.on_rope) {
                (true, _) => zipline_hang,
                (false, true) => ROPE_BODY_RAISE,
                _ => 0.0,
            };
            // Under a parachute he hangs in its seat, facing the way the canopy flies.
            let canopy = || canopies.iter().find(|c| c.soldier == visual.soldier && c.collapse.is_none()).map(|c| c.heading);
            let yaw = if render.parachute { canopy().or_else(|| glide_heading(render)).unwrap_or(render.yaw) } else { render.yaw };
            Transform::from_translation(render.position + Vec3::Y * raise).with_rotation(Quat::from_rotation_y(yaw))
        }));
    }
}


/// Horizontal velocity in the soldier's frame: (right, forward).
fn local_velocity(render: &SoldierRender) -> Vec2 {
    let local = Quat::from_rotation_y(-render.yaw) * render.velocity;
    Vec2::new(local.x, -local.z)
}

/// Weights of the forward, backward, left and right clips for a movement direction.
fn direction_weights(velocity: Vec2) -> [f32; 4] {
    let sum = velocity.x.abs() + velocity.y.abs();
    if sum < 1e-4 {
        return [1.0, 0.0, 0.0, 0.0];
    }
    [velocity.y, -velocity.y, -velocity.x, velocity.x].map(|w| w.max(0.0) / sum)
}

/// Index into [`clips::JUMP`]: still, forward, backward, left, right.
fn jump_direction(velocity: Vec2) -> usize {
    match velocity {
        v if v.length() < 1.0 => 0,
        v if v.y.abs() >= v.x.abs() => {
            if v.y > 0.0 { 1 } else { 2 }
        }
        v => {
            if v.x < 0.0 { 3 } else { 4 }
        }
    }
}

/// The legs state for this frame. Thresholds have some hysteresis so noisy speeds don't
/// flicker between states.
fn next_legs(state: Option<Legs>, time: f32, airborne: f32, yaw_rate: f32, render: &SoldierRender) -> Legs {
    let velocity = local_velocity(render);
    let speed = velocity.length();
    if render.riding {
        return Legs::Hang;
    }
    if render.climbing && render.on_rope {
        return Legs::Rope;
    }
    if render.climbing {
        return Legs::Climb(render.velocity.y < -CLIMB_SPEED * 1.5);
    }
    if render.swimming {
        let sprint_above = if state == Some(Legs::SwimSprint) { 2.0 } else { 2.15 };
        return if speed < 0.3 {
            Legs::SwimStill
        } else if speed > sprint_above {
            Legs::SwimSprint
        } else {
            Legs::Swim(velocity.y < 0.0)
        };
    }
    match state {
        // A blip off the ground (a step or slope edge) is no jump worth landing from.
        Some(Legs::Jump(dir) | Legs::Fall(dir)) if render.grounded => {
            if time > MIN_AIR_TIME {
                return Legs::Land(dir);
            }
        }
        Some(jump @ (Legs::Jump(_) | Legs::Fall(_))) => return jump,
        _ if !render.grounded && render.velocity.y > TAKE_OFF_SPEED => {
            return Legs::Jump(jump_direction(velocity));
        }
        _ if airborne > FALL_TIME => return Legs::Fall(jump_direction(velocity)),
        Some(Legs::Land(dir)) if time < if speed > 1.0 { LAND_TIME_MOVING } else { LAND_TIME } => {
            return Legs::Land(dir);
        }
        _ => {}
    }
    let turning = yaw_rate.abs() > if matches!(state, Some(Legs::Turn(..))) { TURN_STOP } else { TURN_START };
    // The gait the hit zones are posed with too (see `game_shared::skeleton`).
    match skeleton::gait(render.stance, velocity) {
        Gait::Still if turning => Legs::Turn(render.stance, yaw_rate > 0.0),
        Gait::Still => Legs::Still(render.stance),
        Gait::Walk => Legs::Walk,
        Gait::Move => Legs::Move(render.stance),
        Gait::Sprint => Legs::Sprint,
    }
}

fn fade_time(from: Option<Legs>, to: Legs) -> f32 {
    let Some(from) = from else {
        return 0.0;
    };
    match (from, to) {
        (_, Legs::Jump(_) | Legs::Fall(_) | Legs::Land(_) | Legs::Climb(_) | Legs::Rope | Legs::Hang
            | Legs::SwimStill | Legs::Swim(_) | Legs::SwimSprint)
        | (_, Legs::Down | Legs::GetUp)
        | (Legs::Climb(_) | Legs::Rope | Legs::Hang | Legs::SwimStill | Legs::Swim(_) | Legs::SwimSprint, _) => FADE_JUMP,
        _ if from.stance() != to.stance() => {
            if from.stance() == Stance::Prone || to.stance() == Stance::Prone {
                FADE_PRONE
            } else {
                FADE_STANCE
            }
        }
        (_, Legs::Turn(..)) => FADE_TURN_IN,
        (Legs::Turn(..), Legs::Still(_)) => FADE_TURN_OUT,
        (Legs::Still(_) | Legs::Turn(..), _) => FADE_START_MOVING,
        _ => FADE,
    }
}

/// What a soldier's animator needs to know this frame.
struct Cues<'a> {
    render: &'a SoldierRender,
    weapon: Option<&'a Arc<WeaponDesc>>,
    /// The weapon's upper-body set.
    set: &'a str,
    fired: bool,
    reloading: bool,
    /// Seconds into the reload, when it is timed by the server's tick (everyone but us).
    reload_time: Option<f32>,
    /// Seconds since the last shot, the same way.
    fire_time: Option<f32>,
    /// A reload started again right after the last (its time went back).
    reload_again: bool,
    /// The server clock idle loops run on (see `game_shared::skeleton`).
    clock: f32,
    /// Critically wounded.
    downed: bool,
    /// Seated in a vehicle: the seat's pose.
    seat_pose: Option<&'a str>,
}

impl SoldierAnimator {
    /// Picks this frame's clips and weights and advances the crossfades.
    fn update(&mut self, player: &mut AnimationPlayer, animations: &ModelAnimations, cues: &Cues, dt: f32) {
        let render = cues.render;
        if let Some(pose) = cues.seat_pose {
            // The whole body sits, hands on the vehicle's controls.
            if self.state.is_some() {
                self.state = None;
                self.action = None;
                self.legs.set_fade(FADE_STANCE);
                self.upper.set_fade(FADE_STANCE);
            }
            self.legs.begin();
            if let Some(&clip) = animations.others.get(pose).or_else(|| animations.legs.get(pose)) {
                self.legs.play(player, clip, Play::looping(1.0));
            }
            self.legs.update(player, dt);
            self.upper.begin();
            self.upper.update(player, dt);
            self.set.clear();
            self.hand = None;
            self.stowed = true;
            return;
        }
        self.airborne = if render.grounded { 0.0 } else { self.airborne + dt };
        if dt > 0.0 {
            let turned = self.yaw.map_or(0.0, |yaw| angle_between(yaw, render.yaw));
            self.yaw_rate += (turned / dt - self.yaw_rate) * (1.0 - (-TURN_SMOOTHING * dt).exp());
        }
        self.yaw = Some(render.yaw);
        let get_up_time = animations.legs.get(clips::REVIVE).map_or(0.0, |c| c.duration);
        let state = match self.state {
            _ if cues.downed => Legs::Down,
            Some(Legs::Down) => Legs::GetUp,
            Some(Legs::GetUp) if self.state_time < get_up_time => Legs::GetUp,
            _ => next_legs(self.state, self.state_time, self.airborne, self.yaw_rate, render),
        };
        let entered = self.state != Some(state);
        if entered {
            let fade = fade_time(self.state, state);
            self.legs.set_fade(fade);
            if self.action.is_none() {
                self.upper.set_fade(fade);
            }
            self.state = Some(state);
            self.state_time = 0.0;
        } else {
            self.state_time += dt;
        }

        // Legs: up to four clips with weights, all at one speed.
        let velocity = local_velocity(render);
        let mut targets = [("", 0.0); 4];
        let mut speed = 1.0;
        let mut once = None;
        match state {
            Legs::Still(stance) => {
                let clip = match stance {
                    Stance::Standing => clips::STAND,
                    Stance::Crouching => clips::CROUCH,
                    Stance::Prone => clips::PRONE,
                };
                targets[0] = (clip, 1.0);
            }
            Legs::Turn(stance, left) => {
                let side = if left { 0 } else { 1 };
                let clip = match stance {
                    Stance::Standing => clips::STAND_TURN[side],
                    Stance::Crouching => clips::CROUCH_TURN[side],
                    Stance::Prone => clips::PRONE_MOVE[2 + side],
                };
                targets[0] = (clip, 1.0);
                speed = (self.yaw_rate.abs() / TURN_SPEED).clamp(0.6, 1.8);
                if stance == Stance::Prone && left {
                    speed = -speed;
                }
            }
            Legs::Walk | Legs::Move(_) => {
                let (set, normal) = match state {
                    Legs::Move(Stance::Standing) => (clips::RUN, RUN_SPEED),
                    Legs::Move(Stance::Crouching) => (clips::CROUCH_MOVE, CROUCH_SPEED),
                    Legs::Move(Stance::Prone) => (clips::PRONE_MOVE, PRONE_SPEED),
                    _ => (clips::WALK, WALK_SPEED),
                };
                for (target, entry) in targets.iter_mut().zip(set.into_iter().zip(direction_weights(velocity))) {
                    *target = entry;
                }
                speed = (velocity.length() / normal).clamp(0.3, 2.0);
            }
            Legs::Sprint => {
                targets[0] = (clips::SPRINT, 1.0);
                speed = (velocity.length() / SPRINT_SPEED).clamp(0.3, 2.0);
            }
            Legs::Jump(dir) => {
                let [take_off, air, _] = clips::JUMP[dir];
                let take_off_time = animations.legs.get(take_off).map_or(0.0, |c| c.duration);
                if self.state_time < take_off_time {
                    targets[0] = (take_off, 1.0);
                    once = Some(entered);
                } else {
                    targets[0] = (air, 1.0);
                }
            }
            Legs::Fall(dir) => targets[0] = (clips::JUMP[dir][1], 1.0),
            Legs::Land(dir) => {
                targets[0] = (clips::JUMP[dir][2], 1.0);
                once = Some(entered);
            }
            // Holding still on the rungs pauses the climb.
            Legs::Climb(false) => {
                targets[0] = (clips::CLIMB, 1.0);
                speed = (render.velocity.y / CLIMB_SPEED).clamp(-2.0, 2.0);
            }
            Legs::Climb(true) => targets[0] = (clips::SLIDE, 1.0),
            // The rope clips when the soldier has them (Special Forces), else the ladder's.
            Legs::Rope => {
                let climbing = render.velocity.y.abs() > 0.1;
                let (clip, normal) = match (animations.legs.contains_key(clips::ROPE_CLIMB), climbing) {
                    (true, true) => (clips::ROPE_CLIMB, ROPE_CLIMB_SPEED),
                    (true, false) => (clips::ROPE_HOLD, ROPE_CLIMB_SPEED),
                    (false, _) => (clips::CLIMB, CLIMB_SPEED),
                };
                targets[0] = (clip, 1.0);
                speed = if clip == clips::ROPE_HOLD {
                    1.0
                } else {
                    (render.velocity.y / normal).clamp(-2.5, 2.5)
                };
            }
            Legs::Hang => {
                targets[0] = if animations.legs.contains_key(clips::ZIPLINE) {
                    (clips::ZIPLINE, 1.0)
                } else {
                    (clips::CLIMB, 1.0)
                };
                speed = 0.0;
            }
            Legs::SwimStill => targets[0] = (clips::SWIM_STILL, 1.0),
            Legs::Swim(backward) => {
                targets[0] = (if backward { clips::SWIM_BACKWARD } else { clips::SWIM_FORWARD }, 1.0);
                speed = (velocity.length() / SWIM_SPEED).clamp(0.3, 2.0);
            }
            Legs::SwimSprint => {
                targets[0] = (clips::SWIM_SPRINT, 1.0);
                speed = (velocity.length() / SWIM_SPRINT_SPEED).clamp(0.3, 2.0);
            }
            Legs::Down | Legs::GetUp if animations.legs.contains_key(clips::REVIVE) => {
                targets[0] = (clips::REVIVE, 1.0);
                once = Some(entered);
                speed = if state == Legs::Down { 0.0 } else { 1.0 };
            }
            Legs::Down | Legs::GetUp => targets[0] = (clips::PRONE, 1.0),
        }
        let targets = targets.iter().filter(|(_, weight)| *weight > 0.02);

        self.legs.begin();
        for &(name, weight) in targets.clone() {
            let Some(&clip) = animations.legs.get(name) else {
                continue;
            };
            let cycle = animations.cycles.contains(&clip.node);
            let play = match once {
                Some(restart) => Play::once(restart).speed(speed),
                // In step with the replicated step phase (every cycle starts with the left foot
                // forward, so switching between them keeps the step), and idling on the server
                // clock: the same for everyone, and as the hit zones are posed.
                None if cycle => Play::looping(weight).at(skeleton::cycle_time(render.stride, clip.duration)),
                None if matches!(state, Legs::Still(_)) => {
                    Play::looping(weight).at(skeleton::loop_time(cues.clock, clip.duration))
                }
                None => Play::looping(weight).speed(speed),
            };
            self.legs.play(player, clip, play);
        }
        self.legs.update(player, dt);

        // Both hands on the rungs: no weapon, and the ladder clip moves the arms too. Down,
        // the weapon is dropped. Swimming, both arms stroke: the weapon is slung, as in BF2.
        self.stowed = matches!(
            state,
            Legs::Climb(_) | Legs::Rope | Legs::Hang | Legs::Down | Legs::GetUp
                | Legs::SwimStill | Legs::Swim(_) | Legs::SwimSprint
        );
        if self.stowed {
            self.action = None;
            self.upper.begin();
            self.upper.update(player, dt);
            self.hand = None;
            return;
        }

        // Upper body: a one-shot (weapon switch, shot, reload) or else the weapon set's
        // clips paired with the legs clips, in step with them.
        let first_set = self.set.is_empty();
        let switched = self.set != cues.set && animations.upper.contains_key(cues.set);
        if switched {
            self.set = cues.set.to_string();
        }
        let Some(upper) = animations.upper.get(&self.set) else {
            self.hand = None;
            return;
        };
        let pose = usize::from(render.stance == Stance::Prone);
        let one_shot = |names: [&str; 2]| upper.get(names[pose]).copied();
        let started = if switched && !first_set {
            one_shot(clips::DEPLOY).map(|clip| (ActionKind::Deploy, clip, FADE_DEPLOY_IN))
        } else if cues.fired {
            one_shot(clips::FIRE).map(|clip| (ActionKind::Fire, clip, FADE_FIRE_IN))
        } else if (cues.reloading && !self.was_reloading) || cues.reload_again {
            one_shot(clips::RELOAD).map(|clip| (ActionKind::Reload, clip, FADE_RELOAD_IN))
        } else {
            None
        };
        self.was_reloading = cues.reloading;
        if let Some((kind, clip, fade)) = started {
            self.action = Some(Action { kind, clip, time: 0.0 });
            self.upper.set_fade(fade);
        } else if let Some(action) = &mut self.action {
            action.time += dt;
            let timed = match action.kind {
                ActionKind::Reload => cues.reload_time,
                ActionKind::Fire => cues.fire_time,
                ActionKind::Deploy => None,
            };
            let timed_out = timed.is_some_and(|t| t >= action.clip.duration);
            let done = action.clip.finished(player) || (action.kind == ActionKind::Reload && !cues.reloading) || timed_out;
            if done {
                self.action = None;
                self.upper.set_fade(FADE_ACTION_OUT);
            }
        }
        self.upper.begin();
        if let Some(action) = self.action {
            // Held on its first frame while the old weapon goes down.
            let speed = if action.lowering() { 0.0 } else { 1.0 };
            let play = Play::once(started.is_some()).speed(speed);
            let play = match (action.kind, cues.reload_time, cues.fire_time) {
                (ActionKind::Reload, Some(time), _) | (ActionKind::Fire, _, Some(time)) => play.at(time),
                _ => play,
            };
            self.upper.play(player, action.clip, play);
        } else {
            for &(name, weight) in targets {
                let Some(&clip) = upper.get(clips::upper_for(name)).or_else(|| upper.get("stand")) else {
                    continue;
                };
                let phase = animations.legs.get(name).and_then(|legs| legs.phase(player)).unwrap_or(0.0);
                let speed = if once.is_some() { 1.0 } else { speed };
                let cycle = animations.legs.get(name).is_some_and(|legs| animations.cycles.contains(&legs.node));
                let play = match () {
                    _ if once.is_none() && cycle => {
                        Play::looping(weight).at(skeleton::cycle_time(render.stride, clip.duration))
                    }
                    _ if matches!(state, Legs::Still(_)) => {
                        Play::looping(weight).at(skeleton::loop_time(cues.clock, clip.duration))
                    }
                    _ => Play::looping(weight).speed(speed).phase(phase),
                };
                self.upper.play(player, clip, play);
            }
        }
        self.upper.update(player, dt);

        if !self.action.is_some_and(|a| a.lowering()) {
            self.hand = cues.weapon.cloned();
        }
    }
}

/// Signed angle from `a` to `b`, radians.
fn angle_between(a: f32, b: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (b - a + PI).rem_euclid(TAU) - PI
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn animate(
    mut commands: Commands,
    time: Res<Time>,
    mut models: ResMut<SoldierModels>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    clip_assets: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    (feedback, view): (Res<CombatFeedback>, Res<crate::combat::ViewTick>),
    mut throws: MessageReader<ThrowReleased>,
    soldiers: Query<(
        &SoldierRender,
        Option<Ref<Loadout>>,
        Option<&Inventory>,
        Has<LocalSoldier>,
        Has<game_shared::revive::Downed>,
        Option<&Seated>,
        Option<&crate::combat::ShotSeen>,
    )>,
    vehicles: Query<&VehicleData>,
    canopies: Query<&Canopy>,
    mut visuals: Query<(&SoldierVisual, &AttachedBody, &ModelRig, &mut SoldierAnimator)>,
    mut players: Query<&mut AnimationPlayer>,
) {
    // Throws and placed charges animate from the wind-up releasing, not from the moment the
    // projectile actually appears (`ShotFired`, which for these is delayed by
    // `fire.fireLaunchDelay` and would otherwise restart the arm swing mid-air).
    let released: Vec<Entity> = throws.read().map(|t| t.soldier).collect();
    for (visual, body, rig, mut animator) in &mut visuals {
        let (Ok((render, loadout, inventory, local, downed, seated, shot)), Some(body_name)) =
            (soldiers.get(visual.soldier), body.0.as_deref())
        else {
            continue;
        };
        let seat_pose = seated.and_then(|s| {
            let data = vehicles.get(s.vehicle).ok()?;
            data.0.desc.seats.get(s.seat as usize)?.pose.clone()
        });
        // Under a parachute (BF2 plays these through the parachute's seat): hanging, and
        // touching down for a moment after landing.
        let landing = || {
            canopies
                .iter()
                .any(|c| c.soldier == visual.soldier && c.collapse.is_some_and(|t| t < PARACHUTE_LANDING_TIME))
        };
        let seat_pose = seat_pose.or_else(|| match render.parachute {
            true => Some(PARACHUTE_POSE.to_string()),
            false if landing() => Some(PARACHUTE_LANDING_POSE.to_string()),
            false => None,
        });
        let loadout_changed = loadout.as_ref().is_some_and(|l| l.is_changed());
        let loadout = loadout.as_deref();
        let weapon = active_weapon(&armory, loadout, inventory);
        let set = weapon.and_then(|w| w.animations_3p.as_deref()).unwrap_or(DEFAULT_WEAPON_ANIMATIONS);
        if animator.graph.is_none() || loadout_changed {
            for name in loadout.map_or(&[][..], |l| &l.weapons) {
                if let Some(set) = armory.weapon(name).and_then(|w| w.animations_3p.as_deref()) {
                    preload_set(&mut models.weapon_sets, set, &asset_server);
                }
            }
        }
        let animations = body_animations(&mut models, body_name, set, &asset_server, &gltfs, &clip_assets, &mut graphs);
        let (Some(animations), Ok(mut player)) = (animations, players.get_mut(rig.player)) else {
            animator.hand = None;
            continue;
        };
        if animator.graph != Some(animations.graph.id()) {
            let Some(template) = graphs.get(&animations.graph) else {
                continue;
            };
            let own = OwnGraph::new(template.clone(), &mut graphs);
            commands.entity(rig.player).insert(AnimationGraphHandle(own.handle.clone()));
            player.stop_all();
            *animator = SoldierAnimator {
                graph: Some(animations.graph.id()),
                own: Some(own),
                ..default()
            };
        }
        // Our own shots are predicted locally; everyone else's arrive from the server. A
        // throw or a charge (`ThrowReleased`) animates from the wind-up releasing; anything
        // else (`ShotFired`) from the shot itself.
        let (fired, reloading) = if local {
            let fired = animator.shots_seen.is_some_and(|seen| seen != feedback.shots_fired);
            animator.shots_seen = Some(feedback.shots_fired);
            (fired, feedback.reloading)
        } else {
            let is_throw = weapon.is_some_and(|w| matches!(w.fire.kind, FireKind::Thrown | FireKind::Explosives));
            let fired = if is_throw {
                released.contains(&visual.soldier)
            } else {
                // From the tick of the shot, once we draw that moment: a new shot is a time
                // that went back.
                let fire_time = shot.and_then(|s| s.since(view.seconds));
                let new_shot = fire_time.is_some_and(|t| animator.fire_seen.is_none_or(|seen| t < seen));
                animator.fire_seen = fire_time;
                new_shot
            };
            (fired, inventory.is_some_and(|i| i.reloading))
        };
        // Everyone else's reload runs from the tick it started (we learn of it before we draw
        // that moment); ours as predicted.
        let reload_time = match (local, inventory) {
            (false, Some(i)) => skeleton::reload_elapsed(i.reloading, i.reload_started, view.seconds),
            _ => None,
        };
        let reloading = if local { reloading } else { reload_time.is_some() };
        let fire_time = if local { None } else { shot.and_then(|s| s.since(view.seconds)) };
        let reload_again = reload_time.is_some_and(|t| animator.reload_seen.is_some_and(|seen| t < seen));
        animator.reload_seen = reload_time;
        let cues = Cues {
            render,
            weapon,
            set,
            fired,
            reloading,
            reload_time,
            fire_time,
            reload_again,
            clock: skeleton::clock(view.seconds),
            downed,
            seat_pose: seat_pose.as_deref(),
        };
        // Unseen: posed again once seen (the animation picks up from where it is then).
        if animator.sleeping.is_none() {
            animator.update(&mut player, animations, &cues, time.delta_secs());
            if let Some(own) = &mut animator.own {
                own.sync(&player, Some(&animations.graph), &mut graphs);
            }
        }
    }
}

/// Soldiers nobody sees (every body mesh culled by distance or outside the view last frame)
/// are left unposed: their animation graph comes off the animation player, which Bevy then
/// skips, and goes back on once one of the meshes is seen again. BF2 doesn't animate culled
/// soldiers either. Off with the unit LODs.
fn sleep_unseen(
    mut commands: Commands,
    config: Res<UnitLodConfig>,
    mut visuals: Query<(&ModelRig, &mut SoldierAnimator)>,
    seen: Query<&ViewVisibility>,
    graphs: Query<&AnimationGraphHandle>,
) {
    if !config.enabled {
        return;
    }
    for (rig, mut animator) in &mut visuals {
        let visible = rig.meshes.iter().any(|mesh| seen.get(*mesh).is_ok_and(|v| v.get()));
        match (visible, animator.sleeping.take()) {
            (false, None) => {
                if let Ok(graph) = graphs.get(rig.player) {
                    animator.sleeping = Some(graph.0.clone());
                    commands.entity(rig.player).remove::<AnimationGraphHandle>();
                }
            }
            (false, Some(graph)) => animator.sleeping = Some(graph),
            (true, Some(graph)) => {
                commands.entity(rig.player).insert(AnimationGraphHandle(graph));
            }
            (true, None) => {}
        }
    }
}
