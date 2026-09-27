//! The loading screen, and stopping singleplayer while the Esc menu is open.

use super::*;

/// When the level finished loading, and the last time an asset arrived or a shader was
/// compiling.
#[derive(Resource, Default)]
pub(super) struct LoadingProgress {
    level_loaded: Option<f32>,
    last_busy: f32,
}

/// Pipelines still compiling, counted in the render world. The world renders behind the
/// loading screen, so what it needs compiles while that is up.
#[derive(Resource, Clone, Default)]
pub(super) struct CompilingPipelines(Arc<AtomicUsize>);

pub(super) fn count_compiling_pipelines(
    cache: Res<PipelineCache>,
    compiling: Res<CompilingPipelines>,
) {
    compiling
        .0
        .store(cache.waiting_pipelines().count(), Ordering::Relaxed);
}

#[derive(Component)]
pub(super) struct LoadingTitle;

#[derive(Component)]
pub(super) struct LoadingStatus;

#[derive(Component)]
pub(super) struct LoadingMap;

#[derive(Component)]
pub(super) struct LoadingBar;

pub(super) fn spawn_loading_screen(mut commands: Commands, mut progress: ResMut<LoadingProgress>) {
    *progress = LoadingProgress::default();
    commands
        .spawn((
            DespawnOnExit(Screen::Loading),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: px(14),
                ..default()
            },
            background(),
            GlobalZIndex(20),
        ))
        .with_children(|root| {
            root.spawn((
                LoadingMap,
                Node {
                    width: px(320),
                    height: px(320),
                    // Shown once we know the level has a map.
                    display: Display::None,
                    border_radius: BorderRadius::all(px(10)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(MAP_BACKGROUND),
            ));
            root.spawn((LoadingTitle, text("", 30.0, TEXT)));
            root.spawn((LoadingStatus, text("", 15.0, DIM)));
            root.spawn((
                Node {
                    width: px(320),
                    height: px(4),
                    border_radius: BorderRadius::all(px(2)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(TRACK),
            ))
            .with_child((
                LoadingBar,
                Node {
                    position_type: PositionType::Absolute,
                    width: percent(30),
                    height: percent(100),
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                BackgroundColor(ACCENT),
            ));
            root.spawn(Node {
                margin: UiRect::top(px(10)),
                ..default()
            })
            .with_children(|b| button(b, MenuButton::CancelLoading, Look::Plain, "Cancel"));
        });
}

/// The server changed the map while we play (rotation or admin): back to the loading
/// screen until the new level is there, and into the game again after.
pub(super) fn level_changed(
    add: On<Add, MatchInfo>,
    infos: Query<&MatchInfo>,
    screen: Res<State<Screen>>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut menu: ResMut<Menu>,
    mut active: ResMut<ActiveMatch>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
) {
    if *screen.get() != Screen::InGame {
        return;
    }
    let Ok(info) = infos.get(add.entity) else {
        return;
    };
    info!("map change: loading {}", info.level);
    (*next_screen).set_if_neq(Screen::Loading);
    menu.paused = false;
    // Take the mouse again afterwards if we had it.
    menu.grab_on_start = cursor.single().is_ok_and(crate::local_input::cursor_locked);
    if let Some(MatchSetup::Local(settings)) = active.setup.as_mut() {
        settings.level = info.level.clone();
        settings.mode = info.mode.clone();
        settings.size = info.size;
    }
}

pub(super) fn start_pending(mut commands: Commands, mut menu: ResMut<Menu>) {
    let Some((_, frames)) = menu.pending.as_mut() else {
        return;
    };
    if *frames > 0 {
        *frames -= 1;
        return;
    }
    let (setup, _) = menu.pending.take().unwrap();
    commands.queue(move |world: &mut World| net::start_match(world, setup));
}

/// In game once the level is there, we are (or watch), and assets and shaders are done.
#[allow(clippy::too_many_arguments)]
pub(super) fn track_loading(
    time: Res<Time<Real>>,
    mut images: MessageReader<AssetEvent<Image>>,
    mut meshes: MessageReader<AssetEvent<Mesh>>,
    compiling: Res<CompilingPipelines>,
    scripted: Option<Res<ScenarioInput>>,
    mut progress: ResMut<LoadingProgress>,
    mut menu: ResMut<Menu>,
    active: Res<ActiveMatch>,
    level: Option<Res<LoadedLevel>>,
    player: Query<(), With<LocalPlayer>>,
    mut next_screen: ResMut<NextState<Screen>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    let now = time.elapsed_secs();
    let compiling = compiling.0.load(Ordering::Relaxed) > 0;
    if images.read().count() + meshes.read().count() > 0 || compiling {
        progress.last_busy = now;
    }
    if active.setup.is_none() || menu.pending.is_some() {
        return;
    }
    let Some(_) = level else {
        return;
    };
    let loaded = *progress.level_loaded.get_or_insert(now);
    let quiet = now - progress.last_busy > 0.4 || now - loaded > 20.0;
    if !quiet || (player.is_empty() && !active.spectating()) {
        return;
    }
    (*next_screen).set_if_neq(Screen::InGame);
    let (window, mut cursor) = window.into_inner();
    // Not in scripted runs: the person at the computer is doing something else.
    let grab = std::mem::take(&mut menu.grab_on_start) && scripted.is_none();
    if grab && window.focused && !active.spectating() {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_loading_screen(
    mut commands: Commands,
    time: Res<Time<Real>>,
    menu: Res<Menu>,
    active: Res<ActiveMatch>,
    client: Res<State<ClientState>>,
    level: Option<Res<LoadedLevel>>,
    matches: Query<&MatchInfo>,
    catalog: Res<LevelCatalog>,
    compiling: Res<CompilingPipelines>,
    asset_server: Res<AssetServer>,
    mut title: Single<&mut Text, (With<LoadingTitle>, Without<LoadingStatus>)>,
    mut status: Single<&mut Text, (With<LoadingStatus>, Without<LoadingTitle>)>,
    map: Single<(Entity, &mut Node), (With<LoadingMap>, Without<LoadingBar>)>,
    mut bar: Single<&mut Node, (With<LoadingBar>, Without<LoadingMap>)>,
    mut shown_map: Local<Option<(Entity, String)>>,
) {
    let setup = menu
        .pending
        .as_ref()
        .map(|(s, _)| s)
        .or(active.setup.as_ref());
    // The level: known up front when we run the server, from the server when joining.
    let level_name = match setup {
        Some(MatchSetup::Local(settings)) => Some(settings.level.clone()),
        _ => matches.iter().next().map(|m| m.level.clone()),
    };
    let info = level_name.as_deref().and_then(|name| catalog.get(name));
    let heading = level
        .as_ref()
        .map(|l| l.desc.display_name.clone())
        .or_else(|| info.map(|i| i.display_name.clone()))
        .unwrap_or_else(|| "Joining".into());
    if title.0 != heading {
        title.0 = heading;
    }
    let line = match setup {
        Some(MatchSetup::Join { server, .. }) if *client.get() != ClientState::Connected => {
            format!("Connecting to {server}...")
        }
        _ if level.is_none() => "Loading level...".to_string(),
        _ if compiling.0.load(Ordering::Relaxed) > 0 => "Compiling shaders...".to_string(),
        _ => "Loading assets...".to_string(),
    };
    if status.0 != line {
        status.0 = line;
    }
    let minimap = level
        .as_ref()
        .and_then(|l| l.desc.minimap.clone())
        .or_else(|| info.and_then(|i| i.minimap.clone()));
    let (map, mut map_node) = map.into_inner();
    let display = if minimap.is_some() {
        Display::Flex
    } else {
        Display::None
    };
    if map_node.display != display {
        map_node.display = display;
    }
    if let Some(path) = minimap
        && shown_map.as_ref() != Some(&(map, path.clone()))
    {
        commands
            .entity(map)
            .despawn_related::<Children>()
            .with_child((
                ImageNode::new(asset_server.load(format!("imported://{path}"))),
                Node {
                    width: percent(100),
                    height: percent(100),
                    ..default()
                },
            ));
        *shown_map = Some((map, path));
    }
    // An indeterminate bar sweeping across.
    let t = (time.elapsed_secs() * 0.6).fract();
    bar.left = percent(t * 130.0 - 30.0);
}

/// Singleplayer stops the world while the Esc menu is open.
pub(super) fn pause_time(
    menu: Res<Menu>,
    active: Res<ActiveMatch>,
    mut time: ResMut<Time<Virtual>>,
) {
    let singleplayer = matches!(&active.setup, Some(MatchSetup::Local(s)) if !s.network);
    let pause = menu.paused && singleplayer;
    if pause != time.is_paused() {
        if pause {
            time.pause();
        } else {
            time.unpause();
        }
    }
}
