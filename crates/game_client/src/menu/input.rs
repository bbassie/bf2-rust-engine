//! Keys and clicks: Esc, rebinding, buttons, sliders and text fields.

use super::*;

/// Esc goes back, opens and closes the in-game menu; rebinding takes the next key. Menus
/// take the keyboard and the wheel from the game.
#[allow(clippy::too_many_arguments)]
pub(super) fn menu_keys(
    mut commands: Commands,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut scroll: ResMut<AccumulatedMouseScroll>,
    screen: Res<State<Screen>>,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    deploy: Res<DeployScreen>,
    soldier: Query<(), With<LocalSoldier>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
    scripted: Option<Res<ScenarioInput>>,
) {
    if let Some(action) = menu.rebinding {
        let key = keys.get_just_pressed().next().copied();
        let button = mouse.get_just_pressed().next().copied();
        match (key, button) {
            (Some(KeyCode::Escape), _) => menu.rebinding = None,
            (Some(key), _) => {
                settings.rebind(action, Binding::Key(key));
                menu.rebinding = None;
            }
            (None, Some(button)) => {
                settings.rebind(action, Binding::Mouse(button));
                menu.rebinding = None;
                menu.swallow_click = true;
            }
            (None, None) => {}
        }
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
        return;
    }

    if keys.just_pressed(KeyCode::Escape) {
        let consumed = match screen.get() {
            Screen::Menu => {
                menu.page = Page::Home;
                true
            }
            Screen::Loading => {
                commands.queue(net::leave_match);
                true
            }
            Screen::InGame if menu.paused => {
                if menu.page == Page::Settings {
                    menu.page = Page::Home;
                } else {
                    let (window, mut cursor) = window.into_inner();
                    resume(
                        &mut menu,
                        window,
                        &mut cursor,
                        !deploy.open && scripted.is_none(),
                    );
                }
                true
            }
            // The deploy screen closes itself.
            Screen::InGame if deploy.open && !soldier.is_empty() => false,
            Screen::InGame => {
                menu.paused = true;
                menu.page = Page::Home;
                true
            }
        };
        if consumed {
            keys.clear_just_pressed(KeyCode::Escape);
        }
    }
    if *screen.get() == Screen::Menu
        && keys.any_just_pressed([KeyCode::Enter, KeyCode::NumpadEnter])
    {
        menu.submit = true;
    }
    if *screen.get() != Screen::InGame || menu.paused {
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn press_buttons(
    mut commands: Commands,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    catalog: Res<LevelCatalog>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut notice: ResMut<MatchNotice>,
    mut exit: MessageWriter<AppExit>,
    buttons: Query<(&Interaction, &MenuButton), Changed<Interaction>>,
    window: Single<(&Window, &mut CursorOptions), With<PrimaryWindow>>,
    deploy: Res<DeployScreen>,
    scripted: Option<Res<ScenarioInput>>,
) {
    let (window, mut cursor) = window.into_inner();
    if std::mem::take(&mut menu.swallow_click) {
        return;
    }
    let submitted = match menu.page {
        Page::Play | Page::Host => Some(MenuButton::Start),
        Page::Join => Some(MenuButton::Connect),
        _ => None,
    }
    .filter(|_| std::mem::take(&mut menu.submit));
    let pressed: Vec<MenuButton> = buttons
        .iter()
        .filter(|(interaction, _)| **interaction == Interaction::Pressed)
        .map(|(_, button)| button.clone())
        .chain(submitted)
        .collect();
    menu.submit = false;
    for button in &pressed {
        if menu.rebinding.is_some() {
            continue;
        }
        match button {
            MenuButton::Page(page) => {
                menu.page = *page;
                if *page != Page::Home {
                    notice.0 = None;
                }
            }
            MenuButton::Back => menu.page = Page::Home,
            MenuButton::Quit => {
                exit.write(AppExit::Success);
            }
            MenuButton::QuickPlay | MenuButton::Start => {
                let host = *button == MenuButton::Start && menu.page == Page::Host;
                let setup = local_setup(&settings, host);
                begin(&mut menu, &mut next_screen, &mut notice, setup);
            }
            MenuButton::Connect => match join_setup(&settings) {
                Ok(setup) => begin(&mut menu, &mut next_screen, &mut notice, setup),
                Err(err) => notice.0 = Some(err),
            },
            MenuButton::Level(name) => {
                if settings.last_match.level != *name {
                    let last = &settings.last_match;
                    let layout = catalog
                        .get(name)
                        .and_then(|l| pick_layout(l, &last.mode, last.size));
                    let last = &mut settings.last_match;
                    last.level = name.clone();
                    if let Some((mode, size)) = layout {
                        last.mode = mode;
                        last.size = size;
                    }
                }
            }
            MenuButton::Layout(mode, size) => {
                settings.last_match.mode = mode.clone();
                settings.last_match.size = *size;
            }
            MenuButton::Team(team) => {
                settings.last_match.team = *team;
                settings.last_match.spectate = false;
            }
            MenuButton::Resume => resume(
                &mut menu,
                window,
                &mut cursor,
                !deploy.open && scripted.is_none(),
            ),
            MenuButton::Leave | MenuButton::CancelLoading => commands.queue(net::leave_match),
            MenuButton::Tab(tab) => menu.tab = *tab,
            MenuButton::Toggle(toggle) => toggle.flip(&mut settings),
            MenuButton::Step(slider, dir) => {
                let (_, _, step) = slider.range();
                let value = slider.get(&settings) + step * *dir as f32;
                slider.set(&mut settings, value);
            }
            MenuButton::Display(mode) => settings.window_mode = *mode,
            MenuButton::WindowSize(w, h) => settings.window_size = (*w, *h),
            MenuButton::Rebind(action) => menu.rebinding = Some(*action),
            MenuButton::ResetBindings => {
                settings.bindings = Settings::default().bindings;
            }
        }
    }
}

/// Closes the Esc menu and gives the mouse back to the game if `grab`.
fn resume(menu: &mut Menu, window: &Window, cursor: &mut CursorOptions, grab: bool) {
    menu.paused = false;
    if grab && window.focused {
        cursor.visible = false;
        cursor.grab_mode = CursorGrabMode::Locked;
    }
}

/// Puts the loading screen up; the match starts once it has been drawn.
fn begin(
    menu: &mut Menu,
    next_screen: &mut NextState<Screen>,
    notice: &mut MatchNotice,
    setup: MatchSetup,
) {
    notice.0 = None;
    menu.pending = Some((setup, 2));
    menu.grab_on_start = true;
    next_screen.set_if_neq(Screen::Loading);
}

/// Singleplayer or a listen server with the menu's choices: the same as
/// `client --level L --mode M --size S --bots B --team T [--spectate] [--host --port P]`.
fn local_setup(settings: &Settings, host: bool) -> MatchSetup {
    let last = &settings.last_match;
    MatchSetup::Local(ServerSettings {
        level: last.level.clone(),
        mode: last.mode.clone(),
        size: last.size,
        bots: last.bots,
        port: last.port,
        network: host,
        // Hosting from the menu is for other players (unless switched off).
        public: host && last.public,
        local_player: (!last.spectate).then(|| settings.player_name.clone()),
        local_team: last.team,
        ..default()
    })
}

/// Joining the menu's address: the same as `client --connect A --port P --name N`, but
/// host names work too.
fn join_setup(settings: &Settings) -> Result<MatchSetup, String> {
    let last = &settings.last_match;
    let address = last.address.trim();
    if address.is_empty() {
        return Err("Enter the server's address.".into());
    }
    let resolved: Vec<SocketAddr> = (address, last.port)
        .to_socket_addrs()
        .map_err(|err| format!("Can't find {address}: {err}"))?
        .collect();
    let server = resolved
        .iter()
        .find(|a| a.is_ipv4())
        .or(resolved.first())
        .copied()
        .ok_or_else(|| format!("Can't find {address}"))?;
    Ok(MatchSetup::Join {
        server,
        name: settings.player_name.clone(),
        spectate: false,
    })
}

pub(super) fn drag_sliders(
    mut settings: ResMut<Settings>,
    bars: Query<(&Interaction, &RelativeCursorPosition, &SliderBar)>,
) {
    for (interaction, cursor, bar) in &bars {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Some(position) = cursor.normalized {
            let (min, max, _) = bar.0.range();
            let fraction = (position.x + 0.5).clamp(0.0, 1.0);
            bar.0.set(&mut settings, min + (max - min) * fraction);
        }
    }
}

pub(super) fn sync_text_fields(
    mut settings: ResMut<Settings>,
    fields: Query<(&EditableText, &TextField), Changed<EditableText>>,
) {
    for (editable, field) in &fields {
        let value = editable.value().to_string();
        match field {
            TextField::PlayerName => {
                let name = value.trim();
                if !name.is_empty() && settings.player_name != name {
                    settings.player_name = name.to_string();
                }
            }
            TextField::Address => {
                if settings.last_match.address != value {
                    settings.last_match.address = value;
                }
            }
            TextField::Port => {
                if let Ok(port) = value.parse::<u16>()
                    && port > 0
                    && settings.last_match.port != port
                {
                    settings.last_match.port = port;
                }
            }
        }
    }
}
