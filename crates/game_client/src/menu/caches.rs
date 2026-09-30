//! Settings > Game: the generated-data cache (`game_shared::cache`): its size and folder, and a
//! "Clear caches" button (`cache:clear`). Sizing and clearing run on a background thread.

use bevy::tasks::IoTaskPool;
use game_shared::cache::{Cache, Usage};

use super::*;

pub(super) struct CachesUiPlugin;

impl Plugin for CachesUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CacheUi>()
            .add_systems(Update, (press_cache_button, update_cache_text, paint_cache_button).chain().after(ScenarioSystems));
    }
}

#[derive(Component)]
struct ClearCacheButton;

#[derive(Component)]
struct CacheText;

/// The cache folder, under the row.
#[derive(Component)]
struct CacheFolder;

#[derive(Resource, Default)]
struct CacheUi {
    /// Sizing (after clearing, if it cleared: what went).
    task: Option<Task<(Usage, Option<Usage>)>>,
    text: String,
}

/// The row on the Game tab.
pub(super) fn settings_row(p: &mut ChildSpawnerCommands) {
    row(p, "Caches", |c| {
        c.spawn((
            Name::new("cache:clear"),
            ClearCacheButton,
            Look::Plain,
            Button,
            Node {
                padding: UiRect::axes(px(14), px(8)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(Color::NONE),
        ))
        .with_child(text("Clear caches", 15.0, TEXT));
        c.spawn((CacheText, text("", 14.0, DIM)));
    });
    p.spawn((
        CacheFolder,
        text("", 13.0, DIM),
        Node {
            margin: UiRect::left(px(216)),
            ..default()
        },
    ));
}

fn start(ui: &mut CacheUi, cache: &Cache, clear: bool) {
    let cache = cache.clone();
    ui.text = if clear { "Clearing...".into() } else { "Counting...".into() };
    ui.task = Some(IoTaskPool::get().spawn(async move {
        let cleared = clear.then(|| cache.clear());
        (cache.usage(), cleared)
    }));
}

fn press_cache_button(
    buttons: Query<&Interaction, (Changed<Interaction>, With<ClearCacheButton>)>,
    cache: Option<Res<Cache>>,
    mut ui: ResMut<CacheUi>,
) {
    let (Some(cache), None) = (cache, &ui.task) else { return };
    if buttons.iter().any(|i| *i == Interaction::Pressed) {
        start(&mut ui, &cache, true);
    }
}

fn update_cache_text(
    cache: Option<Res<Cache>>,
    mut ui: ResMut<CacheUi>,
    added: Query<(), Added<CacheText>>,
    mut texts: Query<&mut Text, With<CacheText>>,
    mut folders: Query<&mut Text, (With<CacheFolder>, Without<CacheText>)>,
) {
    let folder = cache.as_ref().map_or(String::new(), |c| {
        format!("Navigation grids and tactical maps, rebuilt when needed. In {}", c.root().display())
    });
    for mut t in &mut folders {
        if t.0 != folder {
            t.0 = folder.clone();
        }
    }
    let Some(cache) = cache else {
        ui.text = "No cache folder: generated data isn't kept".into();
        for mut t in &mut texts {
            if t.0 != ui.text {
                t.0 = ui.text.clone();
            }
        }
        return;
    };
    // Counted again whenever the settings page is built.
    if !added.is_empty() && ui.task.is_none() {
        start(&mut ui, &cache, false);
    }
    if let Some(task) = &mut ui.task
        && let Some((usage, cleared)) = check_ready(task)
    {
        ui.task = None;
        if let Some(cleared) = cleared {
            info!("cache: cleared {cleared} from {}", cache.root().display());
        }
        let size = game_shared::content::format_bytes(usage.bytes);
        ui.text = match cleared {
            Some(cleared) => format!("Freed {}, {size} left", game_shared::content::format_bytes(cleared.bytes)),
            None => format!("{size} used"),
        };
    }
    for mut t in &mut texts {
        if t.0 != ui.text {
            t.0 = ui.text.clone();
        }
    }
}

fn paint_cache_button(
    ui: Res<CacheUi>,
    mut buttons: Query<(&Interaction, &mut BackgroundColor), With<ClearCacheButton>>,
) {
    for (interaction, mut background) in &mut buttons {
        let color = if ui.task.is_some() {
            BUTTON.with_alpha(0.5)
        } else if *interaction != Interaction::None {
            HOVER
        } else {
            BUTTON
        };
        background.set_if_neq(BackgroundColor(color));
    }
}
