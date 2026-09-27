//! Zoomed first-person view. Once zoomed in, BF2 draws another LOD of the first-person
//! weapon (`zoom.zoomLod`): scoped weapons model the view through the scope there (an
//! opaque tube around an alpha-blended lens with the reticle), the others their sights up
//! close with a blurred rear sight. It hangs on bone `mesh1` in the zoom pose and is built
//! for the unzoomed view model field of view, while the world camera narrows its own.

use bevy::{
    animation::RepeatAnimation,
    camera::visibility::RenderLayers,
    gltf::{Gltf, GltfMesh},
    light::NotShadowCaster,
    prelude::*,
};
use game_data::WeaponDesc;
use game_shared::{
    input::Buttons,
    weapons::{Armory, Inventory, Loadout},
};

use super::viewmodel::VIEW_MODEL_LAYER;
use crate::{
    camera::ThirdPerson,
    combat::{CombatFeedback, apply_zoom},
    local_input::InputHistory,
    net::LocalSoldier,
};

pub struct ScopePlugin;

impl Plugin for ScopePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Zoom>().add_systems(
            Update,
            (update_zoom.before(apply_zoom), spawn_zoom_model, show_zoom_model).chain(),
        );
    }
}

/// Our weapon's zoom, shared by the world camera (`combat::apply_zoom`) and the view model.
#[derive(Resource, Default)]
pub struct Zoom {
    /// Seconds zoomed in so far, 0 while not zoomed.
    pub held: f32,
    /// The zoom model replaces the weapon.
    pub scoped: bool,
    weapon: String,
    /// Seconds since switching to the weapon.
    drawn: f32,
}

/// First-person weapon part `n` (on bone `mesh{n+1}`), hidden while scoped.
#[derive(Component)]
pub struct WeaponPart(pub usize);

/// A primitive of the zoom model.
#[derive(Component)]
struct ZoomPart;

fn active_weapon<'a>(armory: &'a Armory, soldier: Option<(&Loadout, &Inventory)>) -> Option<&'a WeaponDesc> {
    let (loadout, inventory) = soldier?;
    armory
        .weapon(loadout.weapons.get(inventory.active as usize)?)
        .map(|w| w.as_ref())
}

/// Zooming needs the aim button and a weapon that is drawn and not reloading. The zoom model
/// appears after the weapon's zoom delay, once the arms have settled into the zoom pose:
/// no one-shot (deploy, hip fire) playing and no crossfade in progress.
#[allow(clippy::too_many_arguments)]
fn update_zoom(
    time: Res<Time>,
    history: Res<InputHistory>,
    armory: Res<Armory>,
    feedback: Res<CombatFeedback>,
    third_person: Res<ThirdPerson>,
    soldier: Query<(&Loadout, &Inventory), With<LocalSoldier>>,
    parts: Query<Entity, With<WeaponPart>>,
    parents: Query<&ChildOf>,
    players: Query<&AnimationPlayer>,
    mut zoom: ResMut<Zoom>,
) {
    let Some(weapon) = active_weapon(&armory, soldier.single().ok()) else {
        zoom.held = 0.0;
        zoom.scoped = false;
        return;
    };
    if zoom.weapon != weapon.name {
        zoom.weapon = weapon.name.clone();
        zoom.drawn = 0.0;
    }
    zoom.drawn += time.delta_secs();
    let aiming = history.latest().is_some_and(|input| input.pressed(Buttons::AIM));
    let can_zoom = aiming
        && !feedback.reloading
        && zoom.drawn >= weapon.deploy_time
        && weapon.zoom_factors.iter().any(|&f| f > 0.0);
    zoom.held = if can_zoom { zoom.held + time.delta_secs() } else { 0.0 };

    let moving = parts
        .iter()
        .next()
        .and_then(|part| parents.iter_ancestors(part).find_map(|e| players.get(e).ok()))
        .is_some_and(|player| {
            let (mut total, mut heaviest, mut one_shot) = (0.0, 0.0f32, false);
            for (_, active) in player.playing_animations() {
                one_shot |= active.repeat_mode() != RepeatAnimation::Forever && !active.is_finished();
                total += active.weight();
                heaviest = heaviest.max(active.weight());
            }
            one_shot || heaviest < 0.98 * total
        });
    zoom.scoped = can_zoom
        && weapon.zoom.mesh_1p.is_some()
        && zoom.held >= weapon.zoom.delay
        && !third_person.0
        && (zoom.scoped || !moving);
}

/// The zoom model of the weapon in hand, on the bone of weapon part 0.
#[derive(Default)]
struct ZoomModel {
    path: Option<String>,
    bone: Option<Entity>,
    gltf: Option<Handle<Gltf>>,
    parts: Vec<Entity>,
}

#[allow(clippy::too_many_arguments)]
fn spawn_zoom_model(
    mut commands: Commands,
    mut model: Local<ZoomModel>,
    armory: Res<Armory>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    soldier: Query<(&Loadout, &Inventory), With<LocalSoldier>>,
    weapon_parts: Query<(&WeaponPart, &ChildOf)>,
    zoom_parts: Query<(), With<ZoomPart>>,
) {
    let path = active_weapon(&armory, soldier.single().ok()).and_then(|w| w.zoom.mesh_1p.clone());
    let bone = weapon_parts.iter().find(|(part, _)| part.0 == 0).map(|(_, child_of)| child_of.parent());
    // Parts vanish with the arms when those are replaced.
    let intact = model.parts.iter().all(|&part| zoom_parts.contains(part));
    if model.path != path || model.bone != bone || !intact {
        for part in model.parts.drain(..) {
            commands.entity(part).try_despawn();
        }
        model.gltf = path.as_ref().map(|path| asset_server.load(format!("imported://{path}")));
        model.path = path;
        model.bone = bone;
    }
    if !model.parts.is_empty() {
        return;
    }
    let (Some(bone), Some(gltf)) = (model.bone, model.gltf.as_ref().and_then(|h| gltfs.get(h))) else {
        return;
    };
    for mesh in gltf.meshes.iter().filter_map(|m| gltf_meshes.get(m)) {
        for primitive in &mesh.primitives {
            let material: Handle<StandardMaterial> = primitive
                .material
                .as_ref()
                .and_then(|m| m.path())
                .and_then(|path| {
                    let label = format!("{}/std", path.label()?);
                    Some(asset_server.load(path.clone().with_label(label)))
                })
                .unwrap_or_default();
            // BF2 only adds gloss where the color map's alpha asks for it; Bevy's Fresnel
            // would turn the black scope tube, seen at grazing angles, grey.
            let material = match materials.get(&material).cloned() {
                Some(original) => materials.add(StandardMaterial {
                    reflectance: 0.0,
                    ..original
                }),
                None => material,
            };
            let part = commands
                .spawn((
                    ZoomPart,
                    Mesh3d(primitive.mesh.clone()),
                    MeshMaterial3d(material),
                    Visibility::Hidden,
                    RenderLayers::layer(VIEW_MODEL_LAYER),
                    NotShadowCaster,
                    ChildOf(bone),
                ))
                .id();
            model.parts.push(part);
        }
    }
}

fn show_zoom_model(
    zoom: Res<Zoom>,
    mut weapon_parts: Query<&mut Visibility, (With<WeaponPart>, Without<ZoomPart>)>,
    mut zoom_parts: Query<&mut Visibility, With<ZoomPart>>,
) {
    let (weapon, scope) = if zoom.scoped {
        (Visibility::Hidden, Visibility::Inherited)
    } else {
        (Visibility::Inherited, Visibility::Hidden)
    };
    for mut visibility in &mut weapon_parts {
        visibility.set_if_neq(weapon);
    }
    for mut visibility in &mut zoom_parts {
        visibility.set_if_neq(scope);
    }
}
