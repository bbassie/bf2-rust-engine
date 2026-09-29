//! Keys and clicks: Esc, rebinding, buttons, sliders, text fields and gamepad menu navigation.

use bevy::input::gamepad::{Gamepad, GamepadButton};

use super::*;

/// Esc goes back, opens and closes the in-game menu; rebinding takes the next key or gamepad
/// button. Menus take the keyboard and the wheel from the game. A gamepad's East button (B)
/// does what Esc does; its South button (A) does what Enter does, unless something is
/// rebinding.
#[allow(clippy::too_many_arguments)]
pub(super) fn menu_keys(
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
    gamepads: Query<&Gamepad>,
) {
    // East (B) is also the default gamepad Crouch binding, so it must only act like Esc while
    // a menu is already showing; opening the pause menu from gameplay uses Start instead
    // (mirroring Esc's own behavior below, just on a button gameplay doesn't already use).
    let already_in_menu =
        matches!(screen.get(), Screen::Menu | Screen::Loading) || (*screen.get() == Screen::InGame && menu.paused);
    let gamepad_east = gamepads.iter().any(|g| g.just_pressed(GamepadButton::East));
    let gamepad_escape = gamepads.iter().any(|g| {
        (already_in_menu && g.just_pressed(GamepadButton::East))
            || (!already_in_menu && !deploy.open && g.just_pressed(GamepadButton::Start))
    });
    let gamepad_south = gamepads.iter().any(|g| g.just_pressed(GamepadButton::South));

    if let Some(action) = menu.rebinding_gamepad {
        if gamepad_east {
            menu.rebinding_gamepad = None;
        } else if let Some(button) = gamepads.iter().find_map(|g| g.get_just_pressed().next().copied()) {
            settings.rebind_gamepad(action, button);
            menu.rebinding_gamepad = None;
        }
        return;
    }

    if let Some((action, slot)) = menu.rebinding {
        let key = keys.get_just_pressed().next().copied();
        let button = mouse.get_just_pressed().next().copied();
        match (key, button) {
            (Some(KeyCode::Escape), _) => menu.rebinding = None,
            (Some(key), _) => {
                settings.rebind(action, slot, Binding::Key(key));
                menu.rebinding = None;
            }
            (None, Some(button)) => {
                settings.rebind(action, slot, Binding::Mouse(button));
                menu.rebinding = None;
                menu.swallow_click = true;
            }
            (None, None) => {}
        }
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
        return;
    }

    if keys.just_pressed(KeyCode::Escape) || gamepad_escape {
        let consumed = match screen.get() {
            Screen::Menu => {
                menu.page = Page::Home;
                true
            }
            Screen::Loading => {
                menu.leave = true;
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
        && (keys.any_just_pressed([KeyCode::Enter, KeyCode::NumpadEnter]) || gamepad_south)
    {
        menu.submit = true;
    }
    // Keeps a menu keypress from also being read as a gameplay one: `build_input` still runs
    // (and reads `ButtonInput<KeyCode>`) every `FixedUpdate` tick whenever a match is active,
    // paused or not. Not needed at the main menu itself (no match exists yet, so nothing reads
    // it) or while loading (same reason) — and skipping it there matters now: resetting it
    // every frame would otherwise wipe a modifier's `pressed` state before Bevy's own
    // Tab-navigation observer (Shift+Tab between `menu::text_input` fields) or text editing
    // (Shift+arrow selection, Ctrl+A/C/V/X) get to see it, since both hang off the same
    // `ButtonInput<KeyCode>`/`ButtonInput<Key>` this resets and there's no ordering between a
    // deferred observer trigger and a later system that would guarantee otherwise.
    if (*screen.get() != Screen::InGame && *screen.get() != Screen::Menu) || menu.paused {
        keys.reset_all();
        scroll.delta = Vec2::ZERO;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn press_buttons(
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
    mut browser: ResMut<ServerBrowser>,
    time: Res<Time<Real>>,
    mut fields: Query<(&TextField, &mut EditableText)>,
    gamepads: Query<&Gamepad>,
    focus_targets: Query<&MenuButton>,
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
    // The gamepad's South button (A) presses whatever it last navigated focus to.
    let gamepad_confirmed = (menu.rebinding.is_none() && menu.rebinding_gamepad.is_none())
        .then(|| gamepads.iter().any(|g| g.just_pressed(GamepadButton::South)))
        .unwrap_or(false)
        .then(|| menu.gamepad_focus)
        .flatten()
        .and_then(|entity| focus_targets.get(entity).ok())
        .cloned();
    let pressed: Vec<MenuButton> = buttons
        .iter()
        .filter(|(interaction, _)| **interaction == Interaction::Pressed)
        .map(|(_, button)| button.clone())
        .chain(submitted)
        .chain(gamepad_confirmed)
        .collect();
    menu.submit = false;
    for button in &pressed {
        if menu.rebinding.is_some() || menu.rebinding_gamepad.is_some() {
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
            MenuButton::BotDifficulty(d) => settings.last_match.bot_difficulty = *d,
            MenuButton::Resume => resume(
                &mut menu,
                window,
                &mut cursor,
                !deploy.open && scripted.is_none(),
            ),
            MenuButton::Leave | MenuButton::CancelLoading => menu.leave = true,
            MenuButton::Tab(tab) => menu.tab = *tab,
            MenuButton::Toggle(toggle) => {
                toggle.flip(&mut settings);
                if *toggle == Toggle::Bloom {
                    settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
                }
            }
            MenuButton::Step(slider, dir) => {
                let (_, _, step) = slider.range();
                let value = slider.get(&settings) + step * *dir as f32;
                slider.set(&mut settings, value);
            }
            MenuButton::Display(mode) => settings.window_mode = *mode,
            MenuButton::WindowSize(w, h) => settings.window_size = (*w, *h),
            MenuButton::ViewDistance(distance) => {
                settings.view_distance = *distance;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::ToneMapping(t) => settings.tone_mapping = *t,
            MenuButton::RebindSlot(action, slot) => menu.rebinding = Some((*action, *slot)),
            MenuButton::RebindGamepad(action) => menu.rebinding_gamepad = Some(*action),
            MenuButton::ClearGamepad(action) => {
                let mut set = settings.bindings(*action);
                set.gamepad = None;
                settings.bindings.insert(*action, set);
            }
            MenuButton::ResetBindings => {
                settings.bindings = Settings::default().bindings;
            }
            MenuButton::StanceMode(kind, mode) => kind.set(&mut settings, *mode),
            MenuButton::Preset(preset) => preset.apply(&mut settings),
            MenuButton::ShadowQuality(q) => {
                settings.shadow_quality = *q;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::AntiAliasing(aa) => {
                settings.anti_aliasing = *aa;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::SsaoQuality(q) => {
                settings.ssao_quality = *q;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::Anisotropy(a) => {
                settings.anisotropic_filtering = *a;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::ParticleQuality(q) => {
                settings.particle_quality = *q;
                settings.graphics_preset = crate::settings::GraphicsPreset::Custom;
            }
            MenuButton::CrosshairStyle(style) => settings.crosshair_style = *style,
            MenuButton::FrameCap(fps) => settings.frame_rate_cap = *fps,
            MenuButton::Refresh => browser.refresh(&settings, time.elapsed_secs(), scripted.is_none()),
            MenuButton::Server(address, port) => {
                let last = &mut settings.last_match;
                last.address = address.clone();
                last.port = *port;
                // The address fields show the pick.
                for (field, mut editable) in &mut fields {
                    let value = match field {
                        TextField::Address => address.clone(),
                        TextField::Port => port.to_string(),
                        TextField::PlayerName => continue,
                    };
                    editable.editor_mut().set_text(&value);
                    editable.queue_edit(bevy::text::TextEdit::TextEnd(false));
                }
            }
            MenuButton::Favourite(address, port) => {
                toggle_favourite(&mut settings, &mut browser, address, *port)
            }
            MenuButton::AddFavourite => {
                let last = settings.last_match.clone();
                let address = last.address.trim();
                if address.is_empty() {
                    notice.0 = Some("Enter the server's address.".into());
                } else if !settings.favourite_servers.iter().any(|f| f.is(address, last.port)) {
                    settings.favourite_servers.push(crate::settings::SavedServer {
                        address: address.to_string(),
                        port: last.port,
                        name: String::new(),
                    });
                    browser.refresh(&settings, time.elapsed_secs(), scripted.is_none());
                }
            }
        }
    }
}

/// Moves [`Menu::gamepad_focus`] with the D-pad or the left stick: nearest button whose center
/// lies in the pressed direction, penalizing how far off-axis it is. Debounced so a held
/// direction repeats a few times a second instead of every frame.
pub(super) fn gamepad_menu_nav(
    mut menu: ResMut<Menu>,
    time: Res<Time>,
    gamepads: Query<&Gamepad>,
    mut commands: Commands,
    // UI nodes carry their screen position in `UiGlobalTransform` (a 2D affine transform,
    // separate from the 3D `GlobalTransform`), computed by `bevy_ui`'s layout system.
    nodes: Query<(Entity, &bevy::ui::UiGlobalTransform), (With<MenuButton>, With<Button>)>,
) {
    let Some(gamepad) = gamepads
        .iter()
        .max_by(|a, b| crate::settings::gamepad_activity(a).total_cmp(&crate::settings::gamepad_activity(b)))
    else {
        return;
    };
    menu.nav_cooldown = (menu.nav_cooldown - time.delta_secs()).max(0.0);
    let dpad = gamepad.dpad();
    let stick = gamepad.left_stick();
    let input = if dpad.length() > stick.length() { dpad } else { stick };
    if input.length() < 0.55 {
        menu.nav_cooldown = 0.0;
        return;
    }
    if menu.nav_cooldown > 0.0 {
        return;
    }
    menu.nav_cooldown = 0.28;
    // Gamepad up is +Y; screen space is +Y down.
    let dir = if input.x.abs() > input.y.abs() {
        Vec2::new(input.x.signum(), 0.0)
    } else {
        Vec2::new(0.0, -input.y.signum())
    };
    let positions: Vec<(Entity, Vec2)> =
        nodes.iter().map(|(e, t)| (e, t.translation)).collect();
    if positions.is_empty() {
        return;
    }
    let Some(from) = menu
        .gamepad_focus
        .and_then(|focused| positions.iter().find(|(e, _)| *e == focused))
        .map(|(_, p)| *p)
    else {
        // Nothing focused yet: pick the top-left-most button.
        menu.gamepad_focus = positions
            .iter()
            .min_by(|(_, a), (_, b)| (a.y, a.x).partial_cmp(&(b.y, b.x)).unwrap())
            .map(|(e, _)| *e);
        if let Some(entity) = menu.gamepad_focus {
            commands.trigger(ScrollIntoView { entity });
        }
        return;
    };
    let next = positions
        .iter()
        .filter(|(e, p)| *e != menu.gamepad_focus.unwrap() && (*p - from).dot(dir) > 2.0)
        .min_by(|(_, a), (_, b)| {
            let score = |p: Vec2| {
                let delta = p - from;
                let along = delta.dot(dir);
                let perp = (delta - dir * along).length();
                along + perp * 2.5
            };
            score(*a).total_cmp(&score(*b))
        })
        .map(|(e, _)| *e);
    if let Some(next) = next {
        menu.gamepad_focus = Some(next);
        commands.trigger(ScrollIntoView { entity: next });
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
        bot_difficulty: last.bot_difficulty,
        bot_skill: last.bot_difficulty.params().skill,
        port: last.port,
        network: host,
        // Hosting from the menu is for other players (unless switched off).
        public: host && last.public,
        local_player: (!last.spectate).then(|| settings.player_name.clone()),
        local_team: last.team,
        name: format!("{}'s server", settings.player_name),
        // Co-op: everyone joins our team.
        coop: game_server::coop::CoopSettings {
            human_team: last.team,
            ..default()
        },
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
