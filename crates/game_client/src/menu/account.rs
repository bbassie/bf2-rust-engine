//! The Account page (optional accounts on a master server, see `crate::account`) and the Join
//! page's Quick join button.
//!
//! - No master server set: a field for its address.
//! - Logged out: name and password, Log in / Register.
//! - Logged in: rank, XP to the next rank, career stats, Log out.
//!
//! Quick join asks the master for servers with free slots (unranked ones only when logged
//! out), pings them and joins the closest; without a master server it takes the closest
//! server with free slots from the Join page's list.
//!
//! Buttons and fields are named for scenarios: `account:save-master`, `account:login`,
//! `account:register`, `account:logout`, `account:refresh`, `quickjoin`,
//! `field:account-master`, `field:account-name`, `field:account-password`,
//! `field:account-email`.

use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::Mutex,
    time::{Duration, Instant},
};

use bevy::text::EditableText;
use game_auth::api::Profile;
use game_shared::discovery::{ServerInfo, encode_query, parse_reply};

use super::*;
use crate::account::{Account, clean_url};

pub(super) struct AccountUiPlugin;

impl Plugin for AccountUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AccountForm>().init_resource::<QuickJoinTask>().add_systems(
            Update,
            (build_account_page, sync_account_fields, press_account_buttons, paint_account_buttons, mask_passwords, poll_quick_join)
                .chain()
                .after(ScenarioSystems),
        );
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AccountButton {
    SaveMaster,
    Login,
    Register,
    Logout,
    Refresh,
    QuickJoin,
}

impl AccountButton {
    fn element_name(self) -> &'static str {
        match self {
            AccountButton::SaveMaster => "account:save-master",
            AccountButton::Login => "account:login",
            AccountButton::Register => "account:register",
            AccountButton::Logout => "account:logout",
            AccountButton::Refresh => "account:refresh",
            AccountButton::QuickJoin => "quickjoin",
        }
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum AccountField {
    Master,
    Name,
    Password,
    Email,
}

/// Shows `*` over a password field's (invisible) text.
#[derive(Component)]
pub(super) struct PasswordMask(Entity);

/// What was typed (the password is never saved).
#[derive(Resource, Default)]
pub(super) struct AccountForm {
    master: String,
    name: String,
    password: String,
    email: String,
}

/// The Account page's content, rebuilt when the account changes.
#[derive(Component)]
pub(super) struct AccountPageRoot;

/// A quick join in progress: the server to join, or why not.
#[derive(Resource, Default)]
pub(super) struct QuickJoinTask(Option<Arc<Mutex<Option<Result<SocketAddr, String>>>>>);

/// The Account page (see `build_account_page` for its content).
pub(super) fn account_page(p: &mut ChildSpawnerCommands) {
    heading(p, "Account", "Optional: career stats and ranks from ranked servers. Playing never needs an account.");
    p.spawn((
        AccountPageRoot,
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            max_width: px(760),
            ..default()
        },
    ));
}

/// The Join page's quick join button.
pub(super) fn quick_join_button(p: &mut ChildSpawnerCommands) {
    p.spawn(Node { width: px(10), ..default() });
    account_button(p, AccountButton::QuickJoin, Look::Plain, "Quick join");
}

fn account_button(p: &mut ChildSpawnerCommands, action: AccountButton, look: Look, label: &str) {
    let (padding, size) = match look {
        Look::Primary => (UiRect::axes(px(28), px(11)), 18.0),
        _ => (UiRect::axes(px(14), px(8)), 15.0),
    };
    p.spawn((
        Name::new(action.element_name()),
        action,
        look,
        Button,
        Node {
            padding,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ))
    .with_child(text(label, size, TEXT));
}

fn account_field(p: &mut ChildSpawnerCommands, field: AccountField, value: &str, width: f32) {
    let mut editable = EditableText::new(value);
    editable.max_characters = Some(match field {
        AccountField::Master => 200,
        AccountField::Name => 24,
        AccountField::Password => 128,
        AccountField::Email => 254,
    });
    let name = match field {
        AccountField::Master => "field:account-master",
        AccountField::Name => "field:account-name",
        AccountField::Password => "field:account-password",
        AccountField::Email => "field:account-email",
    };
    let mut text_entity = Entity::PLACEHOLDER;
    p.spawn((
        Node {
            width: px(width),
            flex_shrink: 0.0,
            padding: UiRect::axes(px(10), px(7)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(FIELD),
        BorderColor::all(Color::NONE),
    ))
    .with_children(|b| {
        let password = field == AccountField::Password;
        text_entity = b
            .spawn((
                Name::new(name),
                field,
                editable,
                font(16.0),
                // A password's letters stay invisible; `PasswordMask` shows stars.
                TextColor(if password { Color::NONE } else { TEXT }),
                Node {
                    width: percent(100),
                    ..default()
                },
            ))
            .id();
        if password {
            b.spawn((
                PasswordMask(text_entity),
                text("", 16.0, TEXT),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(10),
                    top: px(7),
                    ..default()
                },
                Pickable::IGNORE,
            ));
        }
    })
    .insert(TextFieldBox(text_entity));
}

fn stat_tile(p: &mut ChildSpawnerCommands, value: String, label: &str) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::axes(px(14), px(10)),
            min_width: px(120),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        BackgroundColor(CARD),
    ))
    .with_children(|t| {
        t.spawn(text(value, 22.0, TEXT));
        t.spawn(text(label, 13.0, DIM));
    });
}

fn duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0) as u64;
    if minutes >= 60 { format!("{}h {:02}m", minutes / 60, minutes % 60) } else { format!("{minutes}m") }
}

fn profile_card(p: &mut ChildSpawnerCommands, profile: &Profile, web: Option<&str>) {
    let rank = &profile.rank;
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            padding: UiRect::all(px(16)),
            border_radius: BorderRadius::all(px(10)),
            max_width: px(640),
            ..default()
        },
        BackgroundColor(CARD),
    ))
    .with_children(|card| {
        card.spawn(text(profile.name.clone(), 26.0, TEXT));
        card.spawn(Node { column_gap: px(8), align_items: AlignItems::Center, ..default() }).with_children(|r| {
            r.spawn((
                Node {
                    padding: UiRect::axes(px(8), px(2)),
                    border_radius: BorderRadius::all(px(5)),
                    ..default()
                },
                BackgroundColor(ACCENT.with_alpha(0.3)),
                children![text(rank.short.clone(), 13.0, TEXT)],
            ));
            r.spawn(text(rank.name.clone(), 18.0, TEXT));
        });
        let next = match (&rank.next_name, rank.next_xp) {
            (Some(name), Some(xp)) => format!("{} XP  |  {} XP to {name}", rank.xp, xp.saturating_sub(rank.xp)),
            _ => format!("{} XP  |  highest rank", rank.xp),
        };
        card.spawn(text(next, 14.0, DIM));
        card.spawn((
            Node {
                width: percent(100),
                height: px(6),
                border_radius: BorderRadius::all(px(3)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(TRACK),
        ))
        .with_child((
            Node {
                width: percent(rank.progress() * 100.0),
                height: percent(100),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(ACCENT),
        ));
        if let Some(web) = web {
            card.spawn(text(format!("On the web: {web}/players/{}", profile.name), 13.0, DIM));
        }
    });
    let s = &profile.stats;
    section(p, "Career (ranked servers)");
    p.spawn(Node { column_gap: px(8), row_gap: px(8), flex_wrap: FlexWrap::Wrap, max_width: px(700), ..default() })
        .with_children(|tiles| {
            stat_tile(tiles, s.score.to_string(), "score");
            stat_tile(tiles, s.kills.to_string(), "kills");
            stat_tile(tiles, s.deaths.to_string(), "deaths");
            stat_tile(tiles, format!("{:.2}", s.kills as f64 / s.deaths.max(1) as f64), "kills per death");
            stat_tile(tiles, format!("{} / {}", s.wins, s.losses), "wins / losses");
            stat_tile(tiles, duration(s.seconds), "played");
        });
}

/// Rebuilds the Account page when the account, the master or the page changes.
#[allow(clippy::too_many_arguments)]
fn build_account_page(
    mut commands: Commands,
    settings: Res<Settings>,
    account: Option<Res<Account>>,
    form: Res<AccountForm>,
    roots: Query<(Entity, Option<&Children>), With<AccountPageRoot>>,
    mut built: Local<Option<(Entity, u32, Option<String>)>>,
) {
    let (Ok((root, children)), Some(account)) = (roots.single(), account) else {
        return;
    };
    let state = account.state();
    let key = (root, state.version, settings.master_url.clone());
    if built.as_ref() == Some(&key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    let master = clean_url(settings.master_url.as_deref());
    commands.entity(root).with_children(|p| {
        section(p, "Master server");
        row(p, "Address", |c| {
            account_field(c, AccountField::Master, settings.master_url.as_deref().unwrap_or(&form.master), 340.0);
            account_button(c, AccountButton::SaveMaster, Look::Plain, "Use");
        });
        let Some(master) = master else {
            p.spawn(text(
                "No master server set: nothing here is needed to play. Enter a master server's address (https://...) to log in, see your stats and use quick join.",
                14.0,
                DIM,
            ));
            return;
        };
        if let Some(error) = &state.error {
            notice_box(p, error);
        }
        if state.logged_in() {
            match &state.profile {
                Some(profile) => profile_card(p, profile, Some(&master)),
                None => {
                    p.spawn(text(format!("Logged in as {}. Loading the profile...", state.name), 15.0, TEXT));
                }
            }
            p.spawn(Node { column_gap: px(8), margin: UiRect::top(px(14)), ..default() }).with_children(|b| {
                account_button(b, AccountButton::Refresh, Look::Plain, "Refresh");
                account_button(b, AccountButton::Logout, Look::Danger, "Log out");
            });
        } else {
            section(p, "Log in or register");
            row(p, "Name", |c| account_field(c, AccountField::Name, &form.name, 300.0));
            row(p, "Password", |c| account_field(c, AccountField::Password, "", 300.0));
            row(p, "Email (optional)", |c| account_field(c, AccountField::Email, &form.email, 300.0));
            p.spawn(Node { column_gap: px(8), margin: UiRect::top(px(10)), align_items: AlignItems::Center, ..default() }).with_children(|b| {
                account_button(b, AccountButton::Login, Look::Primary, "Log in");
                account_button(b, AccountButton::Register, Look::Plain, "Register");
                if state.busy {
                    b.spawn(text("Talking to the master server...", 14.0, DIM));
                }
            });
            p.spawn((
                text(format!("Or register in a browser: {master}/register. Your password goes only to the master server; the game keeps a login token, not the password."), 13.0, DIM),
                Node { margin: UiRect::top(px(10)), ..default() },
            ));
        }
    });
}

fn sync_account_fields(mut form: ResMut<AccountForm>, fields: Query<(&EditableText, &AccountField), Changed<EditableText>>) {
    for (editable, field) in &fields {
        let value = editable.value().to_string();
        match field {
            AccountField::Master => form.master = value,
            AccountField::Name => form.name = value.trim().to_string(),
            AccountField::Password => form.password = value,
            AccountField::Email => form.email = value.trim().to_string(),
        }
    }
}

fn mask_passwords(mut masks: Query<(&PasswordMask, &mut Text)>, fields: Query<&EditableText>) {
    for (mask, mut text) in &mut masks {
        let stars = fields.get(mask.0).map_or(0, |f| f.value().to_string().chars().count());
        let value = "*".repeat(stars);
        if text.0 != value {
            text.0 = value;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn press_account_buttons(
    buttons: Query<(&Interaction, &AccountButton), Changed<Interaction>>,
    account: Option<Res<Account>>,
    mut settings: ResMut<Settings>,
    mut form: ResMut<AccountForm>,
    mut notice: ResMut<MatchNotice>,
    browser: Res<ServerBrowser>,
    mut task: ResMut<QuickJoinTask>,
) {
    let Some(account) = account else {
        return;
    };
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match button {
            AccountButton::SaveMaster => {
                let typed = if form.master.trim().is_empty() { settings.master_url.clone().unwrap_or_default() } else { form.master.clone() };
                match clean_url(Some(&typed)) {
                    Some(url) => settings.master_url = Some(url),
                    None if typed.trim().is_empty() => settings.master_url = None,
                    None => {
                        let mut state = account.state();
                        state.error = Some("The master server's address starts with https:// (or http:// on a LAN).".into());
                        state.version += 1;
                    }
                }
            }
            AccountButton::Login | AccountButton::Register => {
                let register = *button == AccountButton::Register;
                if form.name.is_empty() || form.password.is_empty() {
                    let mut state = account.state();
                    state.error = Some("Enter your name and password.".into());
                    state.version += 1;
                    continue;
                }
                let email = (!form.email.is_empty()).then(|| form.email.clone());
                account.login(form.name.clone(), std::mem::take(&mut form.password), register, email);
            }
            AccountButton::Logout => account.logout(),
            AccountButton::Refresh => account.refresh_profile(),
            AccountButton::QuickJoin => {
                if task.0.is_some() {
                    continue;
                }
                notice.0 = None;
                task.0 = Some(start_quick_join(&account, &browser));
            }
        }
    }
}

fn paint_account_buttons(mut buttons: Query<(&Look, &Interaction, &mut BackgroundColor), With<AccountButton>>) {
    for (look, interaction, mut background) in &mut buttons {
        let hovered = *interaction != Interaction::None;
        let color = match look {
            Look::Primary if hovered => ACCENT.lighter(0.08),
            Look::Primary => ACCENT,
            Look::Danger if hovered => ENEMY.with_alpha(0.55),
            _ if hovered => HOVER,
            _ => BUTTON,
        };
        background.set_if_neq(BackgroundColor(color));
    }
}

/// Pings `candidates` (address, game port, query port) over the discovery protocol and
/// returns those that answered, closest first.
fn ping(candidates: &[(std::net::IpAddr, u16, u16)]) -> Vec<(SocketAddr, ServerInfo, f32)> {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));
    let token = fastrand::u64(..);
    let query = encode_query(token);
    let sent = Instant::now();
    for (ip, _, query_port) in candidates {
        let _ = socket.send_to(&query, (*ip, *query_port));
    }
    let mut answers = Vec::new();
    let mut buffer = [0u8; 4096];
    while sent.elapsed() < Duration::from_millis(800) && answers.len() < candidates.len() {
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if let Some((answer, info)) = parse_reply(&buffer[..len])
            && answer == token
        {
            let ms = sent.elapsed().as_secs_f32() * 1000.0;
            answers.push((SocketAddr::new(from.ip(), info.port), info, ms));
        }
    }
    answers.sort_by(|a, b| a.2.total_cmp(&b.2));
    answers
}

/// Whether a listed server takes one more player of this game.
fn joinable(info: &ServerInfo, logged_in: bool) -> bool {
    info.protocol == game_shared::PROTOCOL_ID && info.players < info.max_players && (logged_in || !info.ranked)
}

fn start_quick_join(account: &Account, browser: &ServerBrowser) -> Arc<Mutex<Option<Result<SocketAddr, String>>>> {
    let slot = Arc::new(Mutex::new(None));
    let logged_in = account.state().logged_in();
    let has_master = account.state().master_url.is_some();
    if !has_master {
        // The Join page's list: the closest server that answered and has room.
        let best = browser
            .entries
            .iter()
            .filter_map(|e| Some((e, e.info.as_ref()?, e.ping_ms?)))
            .filter(|(_, info, _)| joinable(info, logged_in))
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .and_then(|(entry, info, _)| {
                use std::net::ToSocketAddrs;
                (entry.address.as_str(), info.port).to_socket_addrs().ok()?.next()
            });
        *slot.lock().unwrap() = Some(best.ok_or_else(|| "No server with free slots found on this machine or the LAN.".to_string()));
        return slot;
    }
    let (account, result) = (account.clone(), slot.clone());
    let _ = std::thread::Builder::new().name("quick join".into()).spawn(move || {
        let answer = account.quick_join(if logged_in { None } else { Some(false) }).and_then(|list| {
            info!("quick join: the master suggests {} servers ({} players)", list.servers.len(), list.players);
            let candidates: Vec<_> = list
                .servers
                .iter()
                .filter_map(|s| Some((s.address.parse().ok()?, s.port, s.query_port)))
                .collect();
            let answered = ping(&candidates);
            answered
                .iter()
                .find(|(_, info, _)| joinable(info, logged_in))
                .map(|(address, info, ms)| {
                    info!("quick join: {} at {address} ({ms:.0} ms, {}/{} players)", info.name, info.players, info.max_players);
                    *address
                })
                // Nobody answered our ping (firewalls): the master's first choice.
                .or_else(|| candidates.first().map(|(ip, port, _)| SocketAddr::new(*ip, *port)))
                .ok_or_else(|| "No server with free slots right now.".to_string())
        });
        *result.lock().unwrap() = Some(answer);
    });
    slot
}

/// Joins the server a quick join found.
fn poll_quick_join(
    mut task: ResMut<QuickJoinTask>,
    mut menu: ResMut<Menu>,
    mut settings: ResMut<Settings>,
    mut notice: ResMut<MatchNotice>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    let Some(slot) = &task.0 else {
        return;
    };
    let Some(result) = slot.lock().unwrap().take() else {
        return;
    };
    task.0 = None;
    match result {
        Ok(server) => {
            info!("quick join: joining {server}");
            settings.last_match.address = server.ip().to_string();
            settings.last_match.port = server.port();
            notice.0 = None;
            menu.pending = Some((MatchSetup::Join { server, name: settings.player_name.clone(), spectate: false }, 2));
            menu.grab_on_start = true;
            (*next_screen).set_if_neq(Screen::Loading);
        }
        Err(err) => notice.0 = Some(err),
    }
}
