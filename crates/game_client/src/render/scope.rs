//! Zoomed first-person view. Once zoomed in, BF2 draws another LOD of the first-person
//! weapon (`zoom.zoomLod`): scoped weapons model the view through the scope there (an
//! opaque tube around an alpha-blended lens with the reticle), the others their sights up
//! close with a blurred rear sight. It hangs on bone `mesh1` in the zoom pose and is built
//! for the unzoomed view model field of view, while the world camera narrows its own.
//!
//! Red dot sights are not in the model: BF2 draws the HUD of the weapon's alternative GUI
//! index while zoomed (`WeaponDesc::zoom.sight`, the M4's, SCAR's, P90's and AK-74U's
//! `AimPoint.tga` dot in the middle of the screen), which [`show_sight`] puts on the screen
//! over the zoom model. Being HUD, it is as bright at night as by day.

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

use super::{
    materials::{Bf2Layers, Bf2Materials},
    viewmodel::VIEW_MODEL_LAYER,
};
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
            (update_zoom.before(apply_zoom), spawn_zoom_model, show_zoom_model, show_sight).chain(),
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
    /// Seconds a weapon that leaves the zoom after firing (`zoom.out_after_fire`, bolt-action
    /// rifles) stays out of it. Set by the view model, which plays the bolt.
    pub bolt: f32,
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

/// Zooming needs the aim button and a weapon that is drawn, not reloading and not working
/// its bolt (with aim still held it zooms back in afterwards). The zoom model appears after
/// the weapon's zoom delay, once the arms have settled into the zoom pose: no one-shot
/// (deploy, hip fire, bolt) playing and no crossfade in progress.
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
        zoom.bolt = 0.0;
    }
    zoom.drawn += time.delta_secs();
    zoom.bolt = (zoom.bolt - time.delta_secs()).max(0.0);
    let aiming = history.latest().is_some_and(|input| input.pressed(Buttons::AIM));
    let can_zoom = aiming
        && zoom.bolt <= 0.0
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

/// A zoom model material showing a lit reticle: holographic and red dot sights modelled in
/// the zoom model (AIX 2's EOTech on the Magpul, `sig552_eotech_c`), by their colour map's
/// name. Drawn unlit, its colour shows as it is at night as by day (a scope's black
/// crosshair stays black). As emissive light the tone mapping washed the red out to white.
fn is_reticle(material: &super::materials::Bf2Material) -> bool {
    material
        .base
        .base_color_texture
        .as_ref()
        .and_then(|texture| texture.path())
        .is_some_and(|path| {
            let path = path.path().to_string_lossy().to_ascii_lowercase();
            ["eotech", "aimpoint", "reddot", "red_dot", "holo", "reticle"]
                .iter()
                .any(|name| path.contains(name))
        })
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
    mut materials: Bf2Materials,
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
    let primitives: Vec<_> = gltf
        .meshes
        .iter()
        .filter_map(|m| gltf_meshes.get(m))
        .flat_map(|mesh| &mesh.primitives)
        .collect();
    // Wait until the glTF's materials are ready.
    let Some(part_materials) = primitives
        .iter()
        .map(|primitive| materials.for_primitive(primitive))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    for (primitive, material) in primitives.into_iter().zip(part_materials) {
        {
            // In front of the world like the rest of the view model. Matte: the scope tubes'
            // colour maps are black with full gloss in their alpha, and a highlight or
            // Fresnel would turn the tube, seen at grazing angles, grey.
            let material = materials.duplicate_with(&material, |m| {
                Bf2Layers::make_view_model(m);
                m.extension.flags &= !(Bf2Layers::GLOSS_FROM_BASE | Bf2Layers::GLOSS_FROM_NORMAL | Bf2Layers::ENV_MAP);
                m.extension.gloss = 0.0;
                m.base.reflectance = 0.0;
                if is_reticle(m) {
                    m.base.unlit = true;
                }
            });
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

/// The zoomed weapon's HUD sight on the screen ([`WeaponDesc::zoom`]`.sight`).
#[derive(Default)]
struct Sight {
    /// What is drawn: weapon and window size.
    shown: Option<(String, UVec2)>,
    root: Option<Entity>,
}

/// Shows the weapon's HUD sight (its red dot) once the zoom has come in: from the zoom delay,
/// when the zoom model replaces the weapon.
fn show_sight(
    mut commands: Commands,
    mut sight: Local<Sight>,
    zoom: Res<Zoom>,
    armory: Res<Armory>,
    third_person: Res<ThirdPerson>,
    asset_server: Res<AssetServer>,
    window: Single<&Window, With<bevy::window::PrimaryWindow>>,
    soldier: Query<(&Loadout, &Inventory), (With<LocalSoldier>, Without<game_shared::vehicle::Seated>)>,
) {
    let weapon = active_weapon(&armory, soldier.single().ok())
        .filter(|w| !w.zoom.sight.is_empty() && !third_person.0 && zoom.held > 0.0 && zoom.held >= w.zoom.delay);
    let size = UVec2::new(window.width() as u32, window.height() as u32);
    let key = weapon.map(|w| (w.name.clone(), size));
    if key == sight.shown {
        return;
    }
    sight.shown = key;
    if let Some(root) = sight.root.take() {
        commands.entity(root).despawn();
    }
    let Some(weapon) = weapon else {
        return;
    };
    info!("zoom sight of {}: {} pictures", weapon.name, weapon.zoom.sight.len());
    // BF2's HUD is laid out on an 800x600 screen: scaled with the height, centred.
    let scale = window.height() / 600.0;
    let left = (window.width() - 800.0 * scale) * 0.5;
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            GlobalZIndex(-1),
            Pickable::IGNORE,
        ))
        .id();
    for picture in &weapon.zoom.sight {
        let [x, y, w, h] = picture.rect;
        let [r, g, b, a] = picture.color;
        commands.spawn((
            ImageNode::new(asset_server.load(format!("imported://{}", picture.texture))).with_color(Color::srgba(r, g, b, a)),
            Node {
                position_type: PositionType::Absolute,
                left: px(left + x * scale),
                top: px(y * scale),
                width: px(w * scale),
                height: px(h * scale),
                ..default()
            },
            Pickable::IGNORE,
            ChildOf(root),
        ));
    }
    sight.root = Some(root);
}
