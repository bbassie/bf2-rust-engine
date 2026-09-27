//! Destroyed objects: hides what's gone and plays BF2's destruction effects (the armor
//! effects at 0 hit points: dust, splinters, fire, the debris they throw and their sounds)
//! through `effects`. Objects whose effects weren't imported throw their debris meshes and
//! play their sounds directly.

use bevy::{gltf::Gltf, prelude::*};
use game_shared::statics::{Destructible, Inactive, StaticDestroyed, StaticsPlugin};

use crate::{
    audio::PlaySound,
    effects::{EffectLibrary, SpawnEffect, throw_debris},
};

pub struct DestructionRenderPlugin;

impl Plugin for DestructionRenderPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<StaticsPlugin>() {
            app.add_plugins(StaticsPlugin);
        }
        app.init_resource::<PreloadedDebris>()
            .add_systems(Update, (preload_debris, hide_inactive, play_destruction));
    }
}

/// Debris meshes of the level's destroyable objects, loaded before anything breaks so the
/// pieces show up at once.
#[derive(Resource, Default)]
struct PreloadedDebris(Vec<Handle<Gltf>>);

fn preload_debris(
    parts: Query<&Destructible, Added<Destructible>>,
    asset_server: Res<AssetServer>,
    mut preloaded: ResMut<PreloadedDebris>,
) {
    for part in &parts {
        for piece in &part.armor.effect.debris {
            let handle = asset_server.load(format!("imported://{}", piece.mesh));
            if !preloaded.0.contains(&handle) {
                preloaded.0.push(handle);
            }
        }
    }
}

fn hide_inactive(mut parts: Query<(&mut Visibility, Has<Inactive>), With<Destructible>>) {
    for (mut visibility, inactive) in &mut parts {
        let wanted = if inactive { Visibility::Hidden } else { Visibility::Inherited };
        visibility.set_if_neq(wanted);
    }
}

fn play_destruction(
    mut commands: Commands,
    mut destroyed: MessageReader<StaticDestroyed>,
    parts: Query<(&Destructible, &Transform)>,
    library: Option<Res<EffectLibrary>>,
    mut effects: MessageWriter<SpawnEffect>,
    mut sounds: MessageWriter<PlaySound>,
) {
    for event in destroyed.read() {
        let Some((part, transform)) = parts.iter().find(|(p, _)| p.instance == event.instance && !p.wreck) else {
            continue;
        };
        let effect = &part.armor.effect;
        let known: Vec<_> = effect
            .effects
            .iter()
            .filter(|e| library.as_ref().is_some_and(|l| l.has(&e.name)))
            .collect();
        if !known.is_empty() {
            for placed in known {
                let position = transform.transform_point(Vec3::from_array(placed.position));
                let rotation = transform.rotation * Quat::from_array(placed.rotation);
                effects.write(SpawnEffect::new(placed.name.clone(), position).with_rotation(rotation));
            }
            continue;
        }
        for piece in &effect.debris {
            throw_debris(&mut commands, piece, transform);
        }
        for sound in &effect.sounds {
            sounds.write(PlaySound::at(sound, transform.translation).reason("destruction"));
        }
    }
}
