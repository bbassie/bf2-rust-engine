//! The main menu and its pages, and the Esc menu. A page is rebuilt when it changes.

use super::*;

pub(super) fn spawn_main_menu(mut commands: Commands, mut menu: ResMut<Menu>) {
    menu.page = Page::Home;
    menu.rebinding = None;
    menu.pending = None;
    commands
        .spawn((
            DespawnOnExit(Screen::Menu),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            background(),
            // Above the HUD and the deploy screen.
            GlobalZIndex(20),
        ))
        .with_children(|root| {
            root.spawn(Node {
                width: px(280),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                padding: UiRect::new(px(40), px(24), px(48), px(32)),
                row_gap: px(4),
                ..default()
            })
            .with_children(|nav| {
                nav.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    margin: UiRect::new(px(16), px(0), px(0), px(40)),
                    ..default()
                })
                .with_children(|title| {
                    title.spawn(text("BF2", 56.0, TEXT));
                    title.spawn(text("RUST ENGINE", 15.0, ACCENT));
                });
                button(nav, MenuButton::Page(Page::Play), Look::Nav, "Play");
                button(nav, MenuButton::Page(Page::Host), Look::Nav, "Host");
                button(nav, MenuButton::Page(Page::Join), Look::Nav, "Join");
                button(nav, MenuButton::Page(Page::Settings), Look::Nav, "Settings");
                nav.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                button(nav, MenuButton::Quit, Look::Nav, "Quit");
                nav.spawn((
                    text(format!("v{}", env!("CARGO_PKG_VERSION")), 12.0, DIM),
                    Node {
                        margin: UiRect::new(px(16), px(0), px(12), px(0)),
                        ..default()
                    },
                ));
            });
            root.spawn((
                PageRoot,
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::new(px(24), px(48), px(48), px(40)),
                    min_width: px(0),
                    ..default()
                },
            ));
        });
}

/// Opens and closes the Esc menu's overlay.
pub(super) fn sync_pause_overlay(
    mut commands: Commands,
    menu: Res<Menu>,
    screen: Res<State<Screen>>,
    overlay: Query<Entity, With<PauseRoot>>,
) {
    let open = *screen.get() == Screen::InGame && menu.paused;
    match (open, overlay.single()) {
        (true, Err(_)) => {
            commands
                .spawn((
                    PauseRoot,
                    DespawnOnExit(Screen::InGame),
                    Node {
                        position_type: PositionType::Absolute,
                        width: percent(100),
                        height: percent(100),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.01, 0.015, 0.02, 0.6)),
                    GlobalZIndex(30),
                ))
                .with_child((
                    PageRoot,
                    Node {
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(px(28)),
                        border_radius: BorderRadius::all(px(12)),
                        ..default()
                    },
                    BackgroundColor(PANEL),
                ));
        }
        (false, Ok(entity)) => commands.entity(entity).despawn(),
        _ => {}
    }
}

/// Rebuilds the page when it, the settings tab or the level list changes.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_pages(
    mut commands: Commands,
    menu: Res<Menu>,
    screen: Res<State<Screen>>,
    catalog: Res<LevelCatalog>,
    settings: Res<Settings>,
    notice: Res<MatchNotice>,
    active: Res<ActiveMatch>,
    level: Option<Res<LoadedLevel>>,
    cli: Res<Cli>,
    asset_server: Res<AssetServer>,
    monitors: Query<&Monitor, With<PrimaryMonitor>>,
    roots: Query<(Entity, Option<&Children>), With<PageRoot>>,
    mut built: Local<Option<(Entity, Page, SettingsTab, u32, Option<String>)>>,
) {
    let Ok((root, children)) = roots.single() else {
        return;
    };
    let key = (root, menu.page, menu.tab, catalog.version, notice.0.clone());
    if built.as_ref() == Some(&key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let in_game = *screen.get() == Screen::InGame;
    let monitor = monitors.iter().next();
    commands
        .entity(root)
        .with_children(|p| match (in_game, menu.page) {
            (true, Page::Settings) => {
                settings_page(p, menu.tab, &settings, &cli, monitor);
                p.spawn(Node {
                    margin: UiRect::top(px(16)),
                    ..default()
                })
                .with_children(|b| button(b, MenuButton::Back, Look::Plain, "Back"));
            }
            (true, _) => pause_page(p, &active, level.as_deref()),
            (false, Page::Home) => {
                home_page(p, &settings, &catalog, notice.0.as_deref(), &asset_server)
            }
            (false, Page::Play) => local_page(p, false, &catalog),
            (false, Page::Host) => local_page(p, true, &catalog),
            (false, Page::Join) => join_page(p, &settings, notice.0.as_deref()),
            (false, Page::Settings) => settings_page(p, menu.tab, &settings, &cli, monitor),
        });
}

fn home_page(
    p: &mut ChildSpawnerCommands,
    settings: &Settings,
    catalog: &LevelCatalog,
    notice: Option<&str>,
    asset_server: &AssetServer,
) {
    heading(
        p,
        &format!("Welcome, {}", settings.player_name),
        "Conquest with bots, a listen server, or someone else's server.",
    );
    if let Some(notice) = notice {
        notice_box(p, notice);
    }
    let last = &settings.last_match;
    let level = catalog.get(&last.level).or(catalog.levels.first());
    if let Some(level) = level {
        section(p, "Continue");
        p.spawn((
            Node {
                padding: UiRect::all(px(16)),
                column_gap: px(20),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(10)),
                max_width: px(640),
                ..default()
            },
            BackgroundColor(CARD),
        ))
        .with_children(|card| {
            map_preview(card, level.minimap.as_deref(), 132.0, asset_server);
            card.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                flex_grow: 1.0,
                ..default()
            })
            .with_children(|info| {
                info.spawn(text(level.display_name.clone(), 22.0, TEXT));
                let (mode, size) = pick_layout(level, &last.mode, last.size).unwrap_or_default();
                let bots = if last.bots == 1 {
                    "1 bot".to_string()
                } else {
                    format!("{} bots", last.bots)
                };
                info.spawn(text(
                    format!("{} {size}  |  {bots}", mode_label(&mode)),
                    15.0,
                    DIM,
                ));
                info.spawn(Node {
                    margin: UiRect::top(px(8)),
                    ..default()
                })
                .with_children(|b| button(b, MenuButton::QuickPlay, Look::Primary, "Play"));
            });
        });
    }
    let imported = catalog.levels.len().saturating_sub(1);
    let status = if catalog.scan.is_some() {
        "Looking for imported levels...".to_string()
    } else if imported == 0 {
        "No imported levels found: run bf2-import. The test range is always available.".to_string()
    } else {
        format!("{imported} imported levels and the test range.")
    };
    p.spawn((
        text(status, 13.0, DIM),
        Node {
            margin: UiRect::top(px(14)),
            ..default()
        },
    ));
}

/// Play (singleplayer) or Host: pick a level, layout, team and bots.
fn local_page(p: &mut ChildSpawnerCommands, host: bool, catalog: &LevelCatalog) {
    if host {
        heading(p, "Host", "A listen server: others join with your address.");
    } else {
        heading(p, "Play", "Singleplayer against bots.");
    }
    p.spawn(Node {
        column_gap: px(20),
        flex_grow: 1.0,
        min_height: px(0),
        ..default()
    })
    .with_children(|columns| {
        columns
            .spawn((
                ScrollArea,
                Node {
                    width: px(250),
                    flex_shrink: 0.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    padding: UiRect::all(px(6)),
                    overflow: Overflow::scroll_y(),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(CARD),
            ))
            .with_children(|list| {
                for level in &catalog.levels {
                    list.spawn((
                        Name::new(MenuButton::Level(level.name.clone()).element_name()),
                        MenuButton::Level(level.name.clone()),
                        Look::Item,
                        Button,
                        Node {
                            padding: UiRect::axes(px(12), px(8)),
                            flex_shrink: 0.0,
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                        children![text(level.display_name.clone(), 15.0, TEXT)],
                    ));
                }
            });
        columns.spawn((
            LevelDetails { host },
            Node {
                flex_grow: 1.0,
                column_gap: px(24),
                min_width: px(0),
                ..default()
            },
        ));
    });
}

/// The level-dependent part of the play/host page, rebuilt when another level is picked.
pub(super) fn build_level_details(
    mut commands: Commands,
    settings: Res<Settings>,
    catalog: Res<LevelCatalog>,
    asset_server: Res<AssetServer>,
    details: Query<(Entity, &LevelDetails, Option<&Children>)>,
    mut built: Local<Option<(Entity, String)>>,
) {
    let Ok((entity, info, children)) = details.single() else {
        return;
    };
    let last = &settings.last_match;
    let key = (entity, last.level.clone());
    if built.as_ref() == Some(&key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let Some(level) = catalog.get(&last.level).or(catalog.levels.first()) else {
        return;
    };
    let host = info.host;
    commands.entity(entity).with_children(|p| {
        map_preview(p, level.minimap.as_deref(), 300.0, &asset_server);
        p.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|options| {
            options.spawn(text(level.display_name.clone(), 24.0, TEXT));
            section(options, "Game mode");
            options
                .spawn(Node {
                    column_gap: px(6),
                    row_gap: px(6),
                    flex_wrap: FlexWrap::Wrap,
                    ..default()
                })
                .with_children(|chips| {
                    for (mode, size) in &level.layouts {
                        let label = format!("{} {size}", mode_label(mode));
                        button(
                            chips,
                            MenuButton::Layout(mode.clone(), *size),
                            Look::Plain,
                            label,
                        );
                    }
                });
            section(options, "Team");
            options
                .spawn(Node {
                    column_gap: px(6),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|chips| {
                    button(
                        chips,
                        MenuButton::Team(1),
                        Look::Plain,
                        level.teams[0].clone(),
                    );
                    button(
                        chips,
                        MenuButton::Team(2),
                        Look::Plain,
                        level.teams[1].clone(),
                    );
                    chips.spawn(Node {
                        width: px(12),
                        ..default()
                    });
                    switch(chips, Toggle::Spectate);
                    chips.spawn(text("Spectate", 15.0, DIM));
                });
            section(options, "Bots");
            options
                .spawn(Node {
                    column_gap: px(8),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|row| slider(row, Slider::Bots));
            if host {
                section(options, "Server");
                options
                    .spawn(Node {
                        column_gap: px(8),
                        align_items: AlignItems::Center,
                        ..default()
                    })
                    .with_children(|row| {
                        text_field(row, TextField::Port, &settings.last_match.port.to_string(), 90.0);
                        row.spawn(Node {
                            width: px(8),
                            ..default()
                        });
                        switch(row, Toggle::Public);
                        row.spawn(text("Allow players from other machines", 14.0, DIM));
                    });
            }
            options.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            options
                .spawn(Node {
                    margin: UiRect::top(px(12)),
                    ..default()
                })
                .with_children(|b| {
                    button(
                        b,
                        MenuButton::Start,
                        Look::Primary,
                        if host { "Host match" } else { "Start" },
                    );
                });
        });
    });
}

fn join_page(p: &mut ChildSpawnerCommands, settings: &Settings, notice: Option<&str>) {
    heading(
        p,
        "Join",
        "Play on a dedicated server or someone's listen server.",
    );
    if let Some(notice) = notice {
        notice_box(p, notice);
    }
    p.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(12),
        max_width: px(760),
        ..default()
    })
    .with_children(|row| {
        section(row, "Servers");
        row.spawn((
            BrowserStatus,
            text("", 13.0, DIM),
            Node {
                flex_grow: 1.0,
                margin: UiRect::top(px(6)),
                ..default()
            },
        ));
        button(row, MenuButton::Refresh, Look::Plain, "Refresh");
    });
    p.spawn((
        ServerList,
        ScrollArea,
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::all(px(6)),
            max_width: px(760),
            min_height: px(120),
            max_height: px(300),
            overflow: Overflow::scroll_y(),
            border_radius: BorderRadius::all(px(10)),
            margin: UiRect::bottom(px(14)),
            ..default()
        },
        BackgroundColor(CARD),
    ));
    let last = &settings.last_match;
    row(p, "Address", |c| {
        text_field(c, TextField::Address, &last.address, 300.0);
        button(c, MenuButton::AddFavourite, Look::Plain, "Add to favourites");
    });
    row(p, "Port", |c| {
        text_field(c, TextField::Port, &last.port.to_string(), 110.0)
    });
    row(p, "Name", |c| {
        text_field(c, TextField::PlayerName, &settings.player_name, 300.0)
    });
    p.spawn(Node {
        margin: UiRect::top(px(20)),
        ..default()
    })
    .with_children(|b| button(b, MenuButton::Connect, Look::Primary, "Connect"));
}

fn settings_page(
    p: &mut ChildSpawnerCommands,
    tab: SettingsTab,
    settings: &Settings,
    cli: &Cli,
    monitor: Option<&Monitor>,
) {
    heading(p, "Settings", "Changes apply right away and are saved.");
    p.spawn(Node {
        column_gap: px(6),
        margin: UiRect::bottom(px(14)),
        ..default()
    })
    .with_children(|tabs| {
        for tab in SettingsTab::ALL {
            button(tabs, MenuButton::Tab(tab), Look::Plain, tab.label());
        }
    });
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        min_width: px(640),
        ..default()
    })
    .with_children(|p| match tab {
        SettingsTab::Game => {
            row(p, "Player name", |c| {
                text_field(c, TextField::PlayerName, &settings.player_name, 300.0)
            });
            row(p, "Mouse sensitivity", |c| slider(c, Slider::Sensitivity));
            row(p, "Invert mouse Y", |c| switch(c, Toggle::InvertY));
            row(p, "Field of view", |c| slider(c, Slider::FieldOfView));
        }
        SettingsTab::Graphics => {
            row(p, "Window mode", |c| {
                for mode in DisplayMode::ALL {
                    button(c, MenuButton::Display(mode), Look::Plain, mode.label());
                }
            });
            row(p, "Window size", |c| {
                // Sizes that fit the monitor, and the current one.
                let fits = |w: u32, h: u32| {
                    monitor.is_none_or(|m| {
                        let scale = m.scale_factor.max(0.5) as f32;
                        w as f32 <= m.physical_width as f32 / scale
                            && h as f32 <= m.physical_height as f32 / scale
                    })
                };
                let mut sizes: Vec<(u32, u32)> =
                    [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440)]
                        .into_iter()
                        .filter(|(w, h)| fits(*w, *h))
                        .collect();
                if !sizes.contains(&settings.window_size) {
                    sizes.push(settings.window_size);
                }
                for (w, h) in sizes {
                    button(
                        c,
                        MenuButton::WindowSize(w, h),
                        Look::Plain,
                        format!("{w}x{h}"),
                    );
                }
            });
            row(p, "VSync", |c| switch(c, Toggle::VSync));
            row(p, "Sun shadows", |c| {
                switch(c, Toggle::Shadows);
                if cli.no_shadows {
                    c.spawn(text("off for this run (--no-shadows)", 13.0, DIM));
                }
            });
            row(p, "Ambient occlusion", |c| {
                switch(c, Toggle::Ssao);
                if cli.no_ssao {
                    c.spawn(text("off for this run (--no-ssao)", 13.0, DIM));
                }
            });
        }
        SettingsTab::Audio => {
            row(p, "Master volume", |c| slider(c, Slider::Volume));
            row(p, "Effects volume", |c| slider(c, Slider::EffectsVolume));
            row(p, "Ambience volume", |c| slider(c, Slider::AmbienceVolume));
        }
        SettingsTab::Controls => {
            p.spawn(Node {
                column_gap: px(28),
                ..default()
            })
            .with_children(|columns| {
                let half = Action::ALL.len().div_ceil(2);
                for chunk in Action::ALL.chunks(half) {
                    columns
                        .spawn(Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: px(3),
                            width: px(320),
                            ..default()
                        })
                        .with_children(|column| {
                            for action in chunk {
                                binding_row(column, *action);
                            }
                        });
                }
            });
            p.spawn(Node {
                margin: UiRect::top(px(12)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|b| {
                button(
                    b,
                    MenuButton::ResetBindings,
                    Look::Plain,
                    "Reset to defaults",
                );
                b.spawn(text(
                    "Click a key, then press the new key or mouse button. Esc cancels.",
                    13.0,
                    DIM,
                ));
            });
        }
    });
}

fn binding_row(p: &mut ChildSpawnerCommands, action: Action) {
    p.spawn(Node {
        align_items: AlignItems::Center,
        justify_content: JustifyContent::SpaceBetween,
        ..default()
    })
    .with_children(|row| {
        row.spawn(text(action.label(), 14.0, DIM));
        let button_action = MenuButton::Rebind(action);
        row.spawn((
            Name::new(button_action.element_name()),
            button_action,
            Look::Plain,
            Button,
            Node {
                min_width: px(110),
                padding: UiRect::axes(px(10), px(5)),
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(Color::NONE),
            children![(Value::Binding(action), text("", 14.0, TEXT))],
        ));
    });
}

fn pause_page(p: &mut ChildSpawnerCommands, active: &ActiveMatch, level: Option<&LoadedLevel>) {
    let title = level.map_or("Match".to_string(), |l| l.desc.display_name.clone());
    let subtitle = match &active.setup {
        Some(MatchSetup::Local(s)) if s.network => format!("Hosting on port {}", s.port),
        Some(MatchSetup::Local(_)) => "Singleplayer - paused".to_string(),
        Some(MatchSetup::Join { server, .. }) => format!("Online at {server}"),
        None => String::new(),
    };
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        width: px(320),
        ..default()
    })
    .with_children(|p| {
        heading(p, &title, &subtitle);
        button(p, MenuButton::Resume, Look::Primary, "Resume");
        button(p, MenuButton::Page(Page::Settings), Look::Plain, "Settings");
        button(p, MenuButton::Leave, Look::Danger, "Leave match");
    });
}
