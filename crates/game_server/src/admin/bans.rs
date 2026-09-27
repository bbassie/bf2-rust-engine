//! Banned players, by name and address, kept in a RON file (`ban_file` in the server
//! config). Banned players are kicked as soon as they connect or say their name.

use std::{
    net::IpAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bevy::prelude::*;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use bevy_replicon_renet::netcode::NetcodeServerTransport;
use game_shared::protocol::Player;
use serde::{Deserialize, Serialize};

use crate::PlayerClient;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ban {
    pub name: String,
    /// The player's IP address when banned.
    pub address: Option<IpAddr>,
    pub reason: String,
    /// Unix time the ban ends; never if unset.
    pub until: Option<u64>,
}

impl Ban {
    fn active(&self, now: u64) -> bool {
        self.until.is_none_or(|until| until > now)
    }

    /// `name (1.2.3.4) until ...: reason`, for listings.
    pub fn describe(&self) -> String {
        let address = self.address.map_or(String::new(), |a| format!(" ({a})"));
        let until = match self.until {
            Some(until) => format!(", {} min left", until.saturating_sub(unix_now()).div_ceil(60)),
            None => String::new(),
        };
        let reason = if self.reason.is_empty() { String::new() } else { format!(": {}", self.reason) };
        format!("{}{address}{until}{reason}", self.name)
    }
}

#[derive(Resource, Default)]
pub struct BanList {
    pub bans: Vec<Ban>,
    file: Option<PathBuf>,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl BanList {
    pub fn load(file: Option<PathBuf>) -> Self {
        let bans = match &file {
            Some(path) if path.exists() => game_data::read_ron(path).unwrap_or_else(|err| {
                warn!("{err}; starting with no bans");
                Vec::new()
            }),
            _ => Vec::new(),
        };
        if !bans.is_empty() {
            info!("{} bans", bans.len());
        }
        Self { bans, file }
    }

    fn save(&mut self) {
        let now = unix_now();
        self.bans.retain(|ban| ban.active(now));
        if let Some(path) = &self.file
            && let Err(err) = game_data::write_ron(path, &self.bans)
        {
            warn!("can't save bans: {err}");
        }
    }

    /// The ban that applies to a player with this name or address.
    pub fn find(&self, name: &str, address: Option<IpAddr>) -> Option<&Ban> {
        let now = unix_now();
        self.bans.iter().find(|ban| {
            ban.active(now)
                && (ban.name.eq_ignore_ascii_case(name)
                    || (ban.address.is_some() && ban.address == address))
        })
    }

    pub fn add(&mut self, ban: Ban) {
        self.bans.push(ban);
        self.save();
    }

    /// Lifts the bans matching a name or address; returns how many.
    pub fn remove(&mut self, key: &str) -> usize {
        let before = self.bans.len();
        self.bans.retain(|ban| {
            !ban.name.eq_ignore_ascii_case(key) && ban.address.map(|a| a.to_string()).as_deref() != Some(key)
        });
        let removed = before - self.bans.len();
        if removed > 0 {
            self.save();
        }
        removed
    }
}

/// The IP address a client connects from.
pub fn client_address(world: &World, client: Entity) -> Option<IpAddr> {
    let id = world.get::<NetworkId>(client)?.get();
    world
        .get_resource::<NetcodeServerTransport>()?
        .client_addr(id)
        .map(|a| a.ip())
}

/// Kicks banned players when they connect and again once they have told us their name.
pub(super) fn enforce_bans(
    mut commands: Commands,
    bans: Res<BanList>,
    players: Query<(Entity, &Player), (With<PlayerClient>, Or<(Changed<Player>, Added<PlayerClient>)>)>,
) {
    if bans.bans.is_empty() {
        return;
    }
    for (entity, player) in &players {
        let name = player.name.clone();
        commands.queue(move |world: &mut World| {
            let Some(client) = world.get::<PlayerClient>(entity).map(|c| c.0) else {
                return;
            };
            let address = client_address(world, client);
            let Some(ban) = world.resource::<BanList>().find(&name, address) else {
                return;
            };
            let reason = if ban.reason.is_empty() {
                "You are banned from this server".to_string()
            } else {
                format!("You are banned from this server: {}", ban.reason)
            };
            info!("{name} is banned, kicking");
            let _ = super::commands::kick(world, entity, &reason, false);
        });
    }
}
