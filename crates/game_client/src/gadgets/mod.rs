//! Special Forces gadgets and what hits the senses: night vision goggles, the gas mask and
//! tear gas, flashbangs, and the ringing ears after a blast.
//!
//! - Night vision (BF2 `isNightVision` goggles, [`Action::NightVision`]): the picture's
//!   brightness amplified, with interference, through BF2's `night_vision_gradient`. BF2
//!   gives the goggles no battery, so neither do we. A flashbang blinds from farther with
//!   them on (`flashbangRadiusWithNightVision`).
//! - Gas mask (BF2 `isGasMask`, [`Action::GasMask`]): the view through two lenses; the
//!   server stops tear gas hurting the wearer (`game_server::gear`).
//! - Tear gas ([`TearGas`] clouds): without a mask the picture swims and blurs and the
//!   soldier coughs (others hear it too).
//! - Flashbangs: BF2's three layers (white sheet, glow, burnt-in picture) by strength,
//!   from the distance, whether the flash was in view and not behind a wall; ringing ears.
//! - Blasts within 1.05 times their radius (BF2 `sound-tinnitus-range`): ringing ears with
//!   the world muffled (BF2 `sound.tinnitusSetup`: 88 % quieter) and a blurred view.
//!
//! Night vision and the flash's blinding are the local player's alone; nothing about them
//! is sent anywhere.

use std::collections::HashMap;

use avian3d::prelude::*;
use bevy::{
    asset::RenderAssetUsages,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};
use game_data::{FlashbangDesc, GadgetAssets, SoundDesc};
use game_shared::{
    config::GamePaths,
    effects::PlayEffect,
    gear::{GearRequest, SoldierGear, TearGas, gas_exposure},
    physics::GameLayer,
    projectile::SmokeCloud,
    revive::Downed,
    soldier::SoldierMotion,
    weapons::{Armory, Loadout},
};

use crate::{
    audio::{Muffle, PlaySound},
    camera::PlayerCamera,
    chat::ChatBox,
    deploy::DeployScreen,
    effects::EffectLibrary,
    menu::Menu,
    net::LocalSoldier,
    settings::{Action, Actions},
};

mod vision;

pub use vision::VisionSettings;

pub struct GadgetsPlugin;

impl Plugin for GadgetsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(vision::VisionPlugin)
            .init_resource::<Gadgets>()
            .add_systems(Startup, load_assets)
            .add_systems(
                Update,
                (toggle_gear, take_hits, breathe, remote_coughs, show).chain(),
            );
    }
}

/// Blasts this many times their radius away still ring in the ears (BF2
/// `sound-tinnitus-range`).
const TINNITUS_RANGE: f32 = 1.05;
/// Ringing ears: the world this much quieter at full strength, for this long, then easing
/// back over `TINNITUS_RELEASE` (BF2 `sound.tinnitusSetup ... 1 2 4 0.88`, read as attack,
/// hold, release and damping).
const TINNITUS_DAMPING: f32 = 0.88;
const TINNITUS_HOLD: f32 = 2.0;
const TINNITUS_RELEASE: f32 = 4.0;
/// Seconds between coughs in tear gas, and between breaths in the mask.
const COUGH_INTERVAL: [f32; 2] = [1.8, 3.5];
const BREATH_INTERVAL: f32 = 3.6;
/// Night vision and the mask take this long to come on or go.
const GEAR_FADE: f32 = 0.25;

/// The gadgets' look and sounds.
#[derive(Resource, Default)]
struct Files {
    assets: GadgetAssets,
}

impl Files {
    fn sound(path: &Option<String>) -> Option<SoundDesc> {
        path.as_ref().map(|p| SoundDesc::file(p.clone()))
    }
}

/// The local player's gear and what is happening to their senses.
#[derive(Resource, Default)]
pub struct Gadgets {
    pub night_vision: bool,
    pub gas_mask: bool,
    night_vision_shown: f32,
    mask_shown: f32,
    /// Tear gas in the eyes, 0..1 (lingers after leaving the cloud).
    pub gassed: f32,
    flash: Option<Flash>,
    /// Ringing ears: strength and seconds since.
    tinnitus: Option<(f32, f32)>,
    next_cough: f32,
    next_breath: f32,
    time: f32,
}

/// A flashbang going off in view.
struct Flash {
    strength: f32,
    age: f32,
    desc: FlashbangDesc,
    captured: bool,
}

impl Flash {
    fn done(&self) -> bool {
        let d = &self.desc;
        [d.white, d.glow, d.afterimage].iter().all(|l| self.age > l.length(self.strength))
    }
}

fn load_assets(mut commands: Commands, paths: Res<GamePaths>, asset_server: Res<AssetServer>, mut images: ResMut<Assets<Image>>) {
    let assets: GadgetAssets = game_data::read_ron(paths.imported.join("effects/gadgets.ron")).unwrap_or_default();
    let gradient = match &assets.night_vision_gradient {
        Some(path) => asset_server.load(format!("imported://{path}")),
        // Without Special Forces: black through green to pale green.
        None => images.add(Image::new(
            Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
            TextureDimension::D2,
            (0..64u32)
                .flat_map(|i| {
                    let t = i as f32 / 63.0;
                    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
                    [c(t * t * 0.7), c(t * 1.1), c(t * t * 0.5), 255]
                })
                .collect(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        )),
    };
    commands.insert_resource(vision::VisionTextures { gradient });
    commands.insert_resource(Files { assets });
}

/// Which gear the local soldier carries: (night vision goggles, gas mask).
fn carried(loadout: Option<&Loadout>, library: Option<&EffectLibrary>) -> (bool, bool) {
    let (Some(loadout), Some(library)) = (loadout, library) else {
        return (false, false);
    };
    let weapon = |w: &String| library.weapons.weapons.get(w);
    (
        loadout.weapons.iter().filter_map(weapon).any(|w| w.night_vision),
        loadout.weapons.iter().filter_map(weapon).any(|w| w.gas_mask),
    )
}

#[allow(clippy::too_many_arguments)]
fn toggle_gear(
    actions: Actions,
    menu: Res<Menu>,
    deploy: Res<DeployScreen>,
    chat: Res<ChatBox>,
    soldier: Query<(&Loadout, Has<Downed>), With<LocalSoldier>>,
    library: Option<Res<EffectLibrary>>,
    files: Res<Files>,
    mut gadgets: ResMut<Gadgets>,
    mut requests: MessageWriter<GearRequest>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let soldier = soldier.single().ok();
    let (goggles, mask) = carried(soldier.map(|s| s.0), library.as_deref());
    // Gear comes off with the soldier.
    if !goggles {
        gadgets.night_vision = false;
    }
    if !mask && gadgets.gas_mask {
        gadgets.gas_mask = false;
        requests.write(GearRequest { gas_mask: false });
    }
    let usable = soldier.is_some_and(|(_, downed)| !downed) && !menu.paused && !deploy.open && chat.typing.is_none();
    if !usable {
        return;
    }
    if goggles && actions.just_pressed(Action::NightVision) {
        gadgets.night_vision = !gadgets.night_vision;
        if gadgets.night_vision
            && let Some(sound) = Files::sound(&files.assets.night_vision_on)
        {
            sounds.write(PlaySound::local(sound).reason("night vision"));
        }
    }
    if mask && actions.just_pressed(Action::GasMask) {
        gadgets.gas_mask = !gadgets.gas_mask;
        requests.write(GearRequest { gas_mask: gadgets.gas_mask });
        if gadgets.gas_mask
            && let Some(sound) = Files::sound(&files.assets.mask_on)
        {
            sounds.write(PlaySound::local(sound).volume(0.7).reason("gas mask"));
        }
    }
}

/// Flashbangs and blasts near the camera, from the detonations the server sends.
#[allow(clippy::too_many_arguments)]
fn take_hits(
    mut detonations: MessageReader<PlayEffect>,
    armory: Res<Armory>,
    library: Option<Res<EffectLibrary>>,
    camera: Query<&GlobalTransform, With<PlayerCamera>>,
    soldier: Query<(), With<LocalSoldier>>,
    spatial: SpatialQuery,
    files: Res<Files>,
    mut gadgets: ResMut<Gadgets>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let (Ok(camera), false) = (camera.single(), soldier.is_empty()) else {
        detonations.clear();
        return;
    };
    let eye = camera.translation();
    let forward = camera.forward().as_vec3();
    for detonation in detonations.read() {
        let Some(weapon) = armory
            .weapons
            .values()
            .find(|w| w.projectile.detonation_effect.as_deref() == Some(detonation.name.as_str()))
        else {
            continue;
        };
        let distance = eye.distance(detonation.position);
        let flashbang = library.as_ref().and_then(|l| l.weapons.weapons.get(&weapon.name)).and_then(|w| w.flashbang);
        let mut ring = 0.0;
        if let Some(desc) = flashbang {
            let radius = if gadgets.night_vision { desc.night_vision_radius } else { desc.radius };
            let to_flash = (detonation.position - eye).normalize_or_zero();
            let facing = to_flash.dot(forward) >= (desc.view_cone.to_radians() * 0.5).cos();
            let filter = SpatialQueryFilter::from_mask(GameLayer::World);
            let hidden = Dir3::new(detonation.position - eye).ok().is_some_and(|dir| {
                spatial.cast_ray(eye, dir, (distance - 0.3).max(0.0), true, &filter).is_some()
            });
            if distance < radius && !hidden {
                let near = 1.0 - ((distance - desc.inner_radius) / (radius - desc.inner_radius).max(0.1)).clamp(0.0, 1.0);
                let strength = near * if facing { 1.0 } else { desc.unseen_strength };
                if gadgets.flash.as_ref().is_none_or(|f| strength >= f.strength * 0.5) {
                    gadgets.flash = Some(Flash { strength, age: 0.0, desc, captured: false });
                }
                ring = near;
            }
        } else if weapon.projectile.explodes() {
            let reach = weapon.projectile.explosion_radius * TINNITUS_RANGE;
            ring = 1.0 - distance / reach.max(0.1);
        }
        if ring > 0.05 {
            let stronger = gadgets.tinnitus.is_none_or(|(s, age)| ring > s * (1.0 - age / (TINNITUS_HOLD + TINNITUS_RELEASE)));
            if stronger {
                gadgets.tinnitus = Some((ring.min(1.0), 0.0));
                if let Some(sound) = Files::sound(&files.assets.tinnitus) {
                    sounds.write(PlaySound::local(sound).volume(ring.min(1.0)).announcement().reason("tinnitus"));
                }
            }
        }
    }
}

/// Tear gas in the local soldier's eyes and lungs, and the gas mask's breathing.
#[allow(clippy::too_many_arguments)]
fn breathe(
    time: Res<Time>,
    clouds: Query<(&SmokeCloud, &TearGas)>,
    soldier: Query<&SoldierMotion, With<LocalSoldier>>,
    files: Res<Files>,
    mut gadgets: ResMut<Gadgets>,
    mut sounds: MessageWriter<PlaySound>,
) {
    let dt = time.delta_secs();
    gadgets.time += dt;
    let now = gadgets.time;
    let exposure = soldier
        .single()
        .ok()
        .filter(|_| !gadgets.gas_mask)
        .map_or(0.0, |motion| gas_exposure(motion.eye_position(), &clouds));
    // Quick to sting, slow to clear.
    let rate = if exposure > gadgets.gassed { 2.0 } else { 0.25 };
    gadgets.gassed += (exposure - gadgets.gassed) * (1.0 - (-rate * dt).exp());
    if gadgets.gassed > 0.2 && now >= gadgets.next_cough && !files.assets.coughs.is_empty() {
        let coughs = &files.assets.coughs;
        let cough = SoundDesc::file(coughs[fastrand::usize(..coughs.len())].clone());
        sounds.write(PlaySound::local(cough).volume(0.8).reason("cough"));
        gadgets.next_cough = now + COUGH_INTERVAL[0] + fastrand::f32() * (COUGH_INTERVAL[1] - COUGH_INTERVAL[0]);
    }
    if gadgets.gas_mask && now >= gadgets.next_breath {
        if let Some(breath) = Files::sound(&files.assets.mask_breathing) {
            sounds.write(PlaySound::local(breath).volume(0.35).reason("gas mask breathing"));
        }
        gadgets.next_breath = now + BREATH_INTERVAL;
    }
}

/// Other soldiers in tear gas without a mask cough where they stand.
fn remote_coughs(
    time: Res<Time>,
    clouds: Query<(&SmokeCloud, &TearGas)>,
    soldiers: Query<(Entity, &SoldierMotion, &SoldierGear), Without<LocalSoldier>>,
    files: Res<Files>,
    mut next: Local<HashMap<Entity, f32>>,
    mut sounds: MessageWriter<PlaySound>,
) {
    if clouds.is_empty() || files.assets.coughs.is_empty() {
        next.clear();
        return;
    }
    let now = time.elapsed_secs();
    for (entity, motion, gear) in &soldiers {
        if gear.gas_mask || gas_exposure(motion.eye_position(), &clouds) < 0.3 {
            continue;
        }
        let due = next.entry(entity).or_insert(now + fastrand::f32() * COUGH_INTERVAL[1]);
        if now >= *due {
            let coughs = &files.assets.coughs;
            let cough = SoundDesc::file(coughs[fastrand::usize(..coughs.len())].clone());
            sounds.write(PlaySound::at(cough, motion.eye_position()).emitter(entity).reason("cough"));
            *due = now + COUGH_INTERVAL[0] + fastrand::f32() * (COUGH_INTERVAL[1] - COUGH_INTERVAL[0]);
        }
    }
}

/// Puts it all on the last camera (the view model's, so the weapon is in the picture) and
/// into the audio mix.
fn show(
    mut commands: Commands,
    time: Res<Time>,
    player_camera: Query<Entity, With<PlayerCamera>>,
    cameras: Query<(Entity, &ChildOf), With<Camera>>,
    mut settings: Query<&mut VisionSettings>,
    mut gadgets: ResMut<Gadgets>,
    mut muffle: ResMut<Muffle>,
) {
    let dt = time.delta_secs();
    let step = dt / GEAR_FADE;
    let ease = |shown: &mut f32, on: bool| {
        *shown = (*shown + if on { step } else { -step }).clamp(0.0, 1.0);
    };
    let (night_vision, gas_mask) = (gadgets.night_vision, gadgets.gas_mask);
    ease(&mut gadgets.night_vision_shown, night_vision);
    ease(&mut gadgets.mask_shown, gas_mask);

    let (mut white, mut glow, mut afterimage, mut capture) = (0.0, 0.0, 0.0, 0);
    if let Some(flash) = &mut gadgets.flash {
        if !flash.captured {
            flash.captured = true;
            capture = 1;
        }
        let (s, age) = (flash.strength, flash.age);
        white = flash.desc.white.alpha_at(s, age);
        glow = flash.desc.glow.alpha_at(s, age);
        afterimage = flash.desc.afterimage.alpha_at(s, age);
        flash.age += dt;
        if flash.done() {
            gadgets.flash = None;
        }
    }
    let mut ringing = 0.0;
    if let Some((strength, age)) = &mut gadgets.tinnitus {
        *age += dt;
        let fade = if *age < TINNITUS_HOLD { 1.0 } else { 1.0 - (*age - TINNITUS_HOLD) / TINNITUS_RELEASE };
        ringing = *strength * fade.max(0.0);
        if *age > TINNITUS_HOLD + TINNITUS_RELEASE {
            gadgets.tinnitus = None;
        }
    }
    muffle.0 = 1.0 - TINNITUS_DAMPING * ringing;

    let vision = VisionSettings {
        time: gadgets.time,
        night_vision: gadgets.night_vision_shown,
        gas: gadgets.gassed.min(1.0),
        mask: gadgets.mask_shown,
        white,
        glow,
        afterimage,
        shock: ringing * 0.6,
        capture,
    };
    // The view model camera draws last; without one, the player camera.
    let Ok(player) = player_camera.single() else {
        return;
    };
    let target = cameras.iter().find(|(_, parent)| parent.parent() == player).map_or(player, |(e, _)| e);
    match settings.get_mut(target) {
        Ok(mut current) => *current = vision,
        Err(_) => {
            commands.entity(target).insert(vision);
        }
    }
}
