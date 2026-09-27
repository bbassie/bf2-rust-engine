//! The Join page's server list: servers on this machine and on the LAN, the favourite and
//! recent servers from the settings, and those a master server knows (`master_server` in the
//! settings, off by default), with their map, players and ping (see
//! `game_shared::discovery`). The LAN is asked by UDP broadcast, which only the menu does:
//! scripted runs (scenarios) only ask this machine (and a master on it), so they never open
//! a network socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use game_shared::discovery::{
    DISCOVERY_PORTS, LIST_QUERY, MASTER_PORT, ServerInfo, encode_query, parse_reply, parse_server_list,
};

use super::*;
use crate::settings::SavedServer;

/// Found servers and the sockets asking for them.
#[derive(Resource, Default)]
pub struct ServerBrowser {
    /// Asks servers on this machine, over loopback only.
    local: Option<UdpSocket>,
    /// Broadcasts to the LAN and asks favourites on other machines.
    network: Option<UdpSocket>,
    pub entries: Vec<BrowserEntry>,
    /// Token of the last query and when it went out (real seconds).
    query: Option<(u64, f32)>,
    /// When servers a master listed were asked, for their ping.
    asked: Vec<((IpAddr, u16), f32)>,
    /// Bumped when the list changes.
    pub version: u32,
}

#[derive(Clone, Debug)]
pub struct BrowserEntry {
    /// As saved or typed: IP or host name.
    pub address: String,
    /// Game port.
    pub port: u16,
    ip: Option<IpAddr>,
    /// Its answer, if it answered.
    pub info: Option<ServerInfo>,
    pub ping_ms: Option<f32>,
    pub favourite: bool,
}

impl BrowserEntry {
    fn is(&self, ip: IpAddr, port: u16) -> bool {
        self.port == port && self.ip == Some(ip)
    }
}

/// The server list on the Join page, rebuilt when it changes.
#[derive(Component)]
pub(super) struct ServerList;

/// "Searching..." and such, under the list.
#[derive(Component)]
pub(super) struct BrowserStatus;

fn resolve(address: &str, port: u16) -> Option<IpAddr> {
    use std::net::ToSocketAddrs;
    let addresses: Vec<SocketAddr> = (address.trim(), port).to_socket_addrs().ok()?.collect();
    addresses.iter().find(|a| a.is_ipv4()).or(addresses.first()).map(|a| a.ip())
}

fn bind(ip: Ipv4Addr, broadcast: bool) -> Option<UdpSocket> {
    let socket = UdpSocket::bind((ip, 0)).ok()?;
    socket.set_nonblocking(true).ok()?;
    if broadcast {
        socket.set_broadcast(true).ok()?;
    }
    Some(socket)
}

impl ServerBrowser {
    /// Asks every server again. Only this machine unless `network`.
    pub fn refresh(&mut self, settings: &Settings, now: f32, network: bool) {
        let token = fastrand::u64(..);
        self.query = Some((token, now));
        self.asked.clear();
        self.version += 1;
        // Saved servers are listed even while (or if) they don't answer.
        self.entries.clear();
        let saved = settings.favourite_servers.iter().chain(&settings.recent_servers);
        for server in saved {
            if self.entries.iter().any(|e| e.address.eq_ignore_ascii_case(&server.address) && e.port == server.port) {
                continue;
            }
            self.entries.push(BrowserEntry {
                address: server.address.clone(),
                port: server.port,
                ip: resolve(&server.address, server.port),
                info: None,
                ping_ms: None,
                favourite: settings.favourite_servers.iter().any(|f| f.is(&server.address, server.port)),
            });
        }
        let query = encode_query(token);
        if self.local.is_none() {
            self.local = bind(Ipv4Addr::LOCALHOST, false);
        }
        if let Some(socket) = &self.local {
            for port in DISCOVERY_PORTS {
                let _ = socket.send_to(&query, (Ipv4Addr::LOCALHOST, port));
            }
        }
        let master = settings.master_server.as_deref().and_then(|m| {
            use std::net::ToSocketAddrs;
            (m.trim(), MASTER_PORT).to_socket_addrs().ok().or_else(|| m.trim().to_socket_addrs().ok())?.next()
        });
        if let Some(master) = master.filter(|m| m.ip().is_loopback())
            && let Some(socket) = &self.local
        {
            let _ = socket.send_to(LIST_QUERY, master);
        }
        if !network {
            return;
        }
        if self.network.is_none() {
            self.network = bind(Ipv4Addr::UNSPECIFIED, true);
        }
        if let Some(master) = master.filter(|m| !m.ip().is_loopback())
            && let Some(socket) = &self.network
        {
            let _ = socket.send_to(LIST_QUERY, master);
        }
        let Some(socket) = &self.network else {
            return;
        };
        let remote: Vec<IpAddr> = self
            .entries
            .iter()
            .filter_map(|e| e.ip)
            .filter(|ip| !ip.is_loopback())
            .collect();
        for port in DISCOVERY_PORTS {
            let _ = socket.send_to(&query, (Ipv4Addr::BROADCAST, port));
            for ip in &remote {
                let _ = socket.send_to(&query, (*ip, port));
            }
        }
    }

    /// Favourites first, then by ping, silent servers last.
    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            let key = |e: &BrowserEntry| (!e.favourite, e.ping_ms.is_none(), e.ping_ms.unwrap_or(0.0));
            let (ka, kb) = (key(a), key(b));
            ka.0.cmp(&kb.0).then(ka.1.cmp(&kb.1)).then(ka.2.total_cmp(&kb.2))
        });
    }

    /// Whether a query went out less than a moment ago.
    pub fn searching(&self, now: f32) -> bool {
        self.query.is_some_and(|(_, sent)| now - sent < 1.5)
    }

    /// Closes the sockets.
    pub fn close(&mut self) {
        self.local = None;
        self.network = None;
        self.query = None;
    }
}

/// Refreshes when the Join page opens.
pub(super) fn open_browser(
    time: Res<Time<Real>>,
    menu: Res<Menu>,
    screen: Res<State<Screen>>,
    settings: Res<Settings>,
    scripted: Option<Res<ScenarioInput>>,
    mut browser: ResMut<ServerBrowser>,
    mut was_open: Local<bool>,
) {
    let open = *screen.get() == Screen::Menu && menu.page == Page::Join;
    if open && !*was_open {
        browser.refresh(&settings, time.elapsed_secs(), scripted.is_none());
    }
    if !open && *was_open {
        browser.close();
    }
    *was_open = open;
}

/// Takes in the answers.
pub(super) fn poll_browser(
    time: Res<Time<Real>>,
    mut browser: ResMut<ServerBrowser>,
    mut settings: ResMut<Settings>,
) {
    let Some((token, sent)) = browser.query else {
        return;
    };
    let now = time.elapsed_secs();
    let mut buffer = [0u8; 4096];
    let mut replies = Vec::new();
    let mut listed = Vec::new();
    for socket in [&browser.local, &browser.network].into_iter().flatten() {
        while let Ok((len, from)) = socket.recv_from(&mut buffer) {
            if let Some((answer, info)) = parse_reply(&buffer[..len])
                && answer == token
            {
                replies.push((from.ip(), info));
            } else if let Some(servers) = parse_server_list(&buffer[..len]) {
                info!("browser: the master server lists {} servers", servers.len());
                listed.extend(servers);
            }
        }
    }
    // Servers from the master: listed now, and asked for their details directly.
    let query = encode_query(token);
    for (ip, game_port, query_port) in listed {
        if !browser.entries.iter().any(|e| e.is(ip, game_port)) {
            browser.entries.push(BrowserEntry {
                address: ip.to_string(),
                port: game_port,
                ip: Some(ip),
                info: None,
                ping_ms: None,
                favourite: false,
            });
            browser.version += 1;
        }
        let socket = if ip.is_loopback() { &browser.local } else { &browser.network };
        if let Some(socket) = socket {
            let _ = socket.send_to(&query, (ip, query_port));
            browser.asked.push(((ip, game_port), now));
        }
    }
    if replies.is_empty() {
        return;
    }
    for (ip, info) in replies {
        let asked = browser.asked.iter().find(|(key, _)| *key == (ip, info.port)).map_or(sent, |(_, at)| *at);
        let ping = ((now - asked) * 1000.0).max(1.0);
        info!("browser: {} at {ip}:{} ({} ms)", info.name, info.port, ping.round());
        let known: Vec<SavedServer> = settings
            .favourite_servers
            .iter()
            .chain(&settings.recent_servers)
            .filter(|s| s.port == info.port && resolve(&s.address, s.port) == Some(ip) && s.name != info.name)
            .cloned()
            .collect();
        // Remember the names of saved servers.
        if !known.is_empty() {
            let settings = &mut *settings;
            for server in settings.favourite_servers.iter_mut().chain(settings.recent_servers.iter_mut()) {
                if known.contains(server) {
                    server.name = info.name.clone();
                }
            }
        }
        match browser.entries.iter_mut().find(|e| e.is(ip, info.port)) {
            Some(entry) => {
                entry.ping_ms = Some(ping);
                entry.info = Some(info);
            }
            None => {
                let port = info.port;
                browser.entries.push(BrowserEntry {
                    address: ip.to_string(),
                    port,
                    ip: Some(ip),
                    info: Some(info),
                    ping_ms: Some(ping),
                    favourite: false,
                });
            }
        }
        browser.version += 1;
    }
    browser.sort();
}

/// Stars or unstars a listed server.
pub(super) fn toggle_favourite(settings: &mut Settings, browser: &mut ServerBrowser, address: &str, port: u16) {
    let Some(entry) = browser.entries.iter_mut().find(|e| e.address == address && e.port == port) else {
        return;
    };
    let favourites = &mut settings.favourite_servers;
    match favourites.iter().position(|f| f.is(&entry.address, entry.port)) {
        Some(i) => {
            favourites.remove(i);
            entry.favourite = false;
        }
        None => {
            favourites.push(SavedServer {
                address: entry.address.clone(),
                port: entry.port,
                name: entry.info.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
            });
            entry.favourite = true;
        }
    }
    browser.sort();
    browser.version += 1;
}

/// Rebuilds the list when servers answer or the favourites change.
pub(super) fn build_server_list(
    mut commands: Commands,
    time: Res<Time<Real>>,
    browser: Res<ServerBrowser>,
    lists: Query<(Entity, Option<&Children>), With<ServerList>>,
    mut status: Query<&mut Text, With<BrowserStatus>>,
    mut built: Local<Option<(Entity, u32)>>,
) {
    let Ok((list, children)) = lists.single() else {
        return;
    };
    let now = time.elapsed_secs();
    let answered = browser.entries.iter().filter(|e| e.info.is_some()).count();
    let line = if browser.searching(now) {
        "Searching...".to_string()
    } else if browser.entries.is_empty() {
        "No servers found on this machine or the LAN. Enter an address below.".to_string()
    } else {
        format!("{answered} of {} servers answered.", browser.entries.len())
    };
    for mut text in &mut status {
        if text.0 != line {
            text.0 = line.clone();
        }
    }
    let key = (list, browser.version);
    if *built == Some(key) {
        return;
    }
    *built = Some(key);
    for child in children.into_iter().flatten() {
        commands.entity(*child).despawn();
    }
    commands.entity(list).with_children(|list| {
        server_row(list, None);
        for entry in &browser.entries {
            server_row(list, Some(entry));
        }
    });
}

/// Column widths of the server list.
const COLUMNS: [f32; 4] = [190.0, 130.0, 90.0, 50.0];

/// A server (or, without one, the header): a favourite star and a row that selects it.
fn server_row(list: &mut ChildSpawnerCommands, entry: Option<&BrowserEntry>) {
    let info = entry.and_then(|e| e.info.as_ref());
    let incompatible = info.is_some_and(|i| i.protocol != game_shared::PROTOCOL_ID);
    let values: [String; 5] = match (entry, info) {
        (None, _) => ["Server".into(), "Map".into(), "Mode".into(), "Players".into(), "Ping".into()],
        (Some(entry), Some(info)) => [
            if incompatible { format!("{} (other version)", info.name) } else { info.name.clone() },
            info.level_name.clone(),
            format!("{} {}", mode_label(&info.mode), info.size),
            format!("{}/{}{}", info.players, info.max_players, if info.bots > 0 { format!(" +{}", info.bots) } else { String::new() }),
            entry.ping_ms.map_or(String::new(), |p| format!("{p:.0}")),
        ],
        (Some(entry), None) => [
            format!("{}:{}", entry.address, entry.port),
            "no answer".into(),
            String::new(),
            String::new(),
            String::new(),
        ],
    };
    let color = if entry.is_none() || info.is_none() || incompatible { DIM } else { TEXT };
    list.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(4),
        flex_shrink: 0.0,
        ..default()
    })
    .with_children(|row| {
        match entry {
            Some(entry) => {
                let action = MenuButton::Favourite(entry.address.clone(), entry.port);
                row.spawn((
                    Name::new(action.element_name()),
                    action,
                    Look::Custom,
                    Button,
                    Node {
                        width: px(28),
                        height: px(28),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                    children![text("*", 18.0, if entry.favourite { ACCENT } else { TRACK })],
                ));
            }
            _ => {
                row.spawn(Node {
                    width: px(28),
                    ..default()
                });
            }
        }
        let mut cells_parent = match entry {
            Some(entry) => {
                let action = MenuButton::Server(entry.address.clone(), entry.port);
                row.spawn((
                    Name::new(action.element_name()),
                    action,
                    Look::Item,
                    Button,
                    Node {
                        flex_grow: 1.0,
                        padding: UiRect::axes(px(10), px(6)),
                        column_gap: px(10),
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ))
            }
            None => row.spawn(Node {
                flex_grow: 1.0,
                padding: UiRect::axes(px(10), px(2)),
                column_gap: px(10),
                ..default()
            }),
        };
        cells_parent.with_children(|cells| {
            for (i, cell) in values.iter().enumerate() {
                let size = if entry.is_none() { 12.0 } else if i == 0 { 15.0 } else { 14.0 };
                let width = if i == 0 { None } else { COLUMNS.get(i - 1).copied() };
                cells.spawn((
                    text(if entry.is_none() { cell.to_uppercase() } else { cell.clone() }, size, color),
                    Node {
                        width: width.map_or(Val::Auto, px),
                        flex_grow: if i == 0 { 1.0 } else { 0.0 },
                        flex_shrink: if i == 0 { 1.0 } else { 0.0 },
                        min_width: px(0),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                ));
            }
        });
    });
}
