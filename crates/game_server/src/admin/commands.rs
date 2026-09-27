//! Admin commands, the same from the remote console and from the chat (`/kick 3`).

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_shared::{
    chat::Kicked,
    config::GamePaths,
    conquest::{RoundState, Tickets},
    level::LoadedLevel,
    protocol::{MatchInfo, Player, Score, Team},
};

use super::{
    PlayerId,
    bans::{Ban, BanList, client_address, unix_now},
};
use crate::{
    Controls, HostPlayer, PlayerClient, ServerSettings,
    bots::BotBrain,
    chat::announce,
    rotation::{self, MapEntry, MapRotation, level_display_name, mode_label},
};

const HELP: &str = "\
info                             server, map, round and players
players                          everyone, with id, team, score and ping
say <text>                       message to everyone
kick <player> [reason]           a player by id or (part of the) name
ban <player> [minutes] [reason]  kick and keep out (for good without minutes)
unban <name or address>
bans                             the ban list
map <level> [mode] [size]        change the map now
next                             play the next map of the rotation now
rotation                         the map rotation
levels                           levels that can be played
restart                          restart the map
bots <count>                     number of bots
tickets <count> [team]           tickets of both teams, or of team 1 or 2
friendlyfire [on|off]            whether bullets hurt teammates";

/// Runs one admin command line and returns the answer. `admin` names who asked, for the log.
pub fn execute(world: &mut World, line: &str, admin: &str) -> String {
    let line = line.trim();
    let (command, args) = line
        .split_once(char::is_whitespace)
        .map_or((line, ""), |(command, args)| (command, args.trim()));
    if !command.is_empty() {
        info!("admin ({admin}): {line}");
    }
    let result = match command.to_ascii_lowercase().as_str() {
        "" => Ok(String::new()),
        "help" | "?" => Ok(HELP.into()),
        "info" | "serverinfo" | "status" => Ok(info(world)),
        "players" | "list" | "users" => Ok(players(world)),
        "say" => say(world, args),
        "kick" => kick_command(world, args),
        "ban" => ban_command(world, args),
        "unban" => unban(world, args),
        "bans" | "banlist" => Ok(list_bans(world)),
        "map" => map(world, args),
        "next" | "nextmap" | "runnextlevel" => next_map(world),
        "rotation" | "maplist" => Ok(rotation_list(world)),
        "levels" => Ok(rotation::available_levels(world.resource::<GamePaths>()).join("\n")),
        "restart" | "restartmap" => restart(world),
        "bots" => bots(world, args),
        "tickets" => tickets(world, args),
        "friendlyfire" | "ff" => friendly_fire(world, args),
        other => Err(format!("Unknown command `{other}`. Try `help`.")),
    };
    match result {
        Ok(text) | Err(text) => text,
    }
}

type Answer = Result<String, String>;

fn info(world: &mut World) -> String {
    let settings = world.resource::<ServerSettings>().clone();
    let level = world
        .get_resource::<LoadedLevel>()
        .map_or_else(|| "loading".to_string(), |l| l.desc.display_name.clone());
    let round = match world.query::<(&RoundState, &Tickets)>().iter(world).next() {
        Some((RoundState::Playing, tickets)) => format!(
            "playing, tickets {:.0} / {:.0}",
            tickets.remaining[0], tickets.remaining[1]
        ),
        Some((RoundState::Ended { winner, restart_in }, _)) => {
            format!("over (winner {winner:?}), next in {restart_in:.0} s")
        }
        None => "starting".into(),
    };
    let (mut humans, mut bots) = (0, 0);
    for player in world.query::<&Player>().iter(world) {
        if player.is_bot { bots += 1 } else { humans += 1 }
    }
    let rotation = world.resource::<MapRotation>();
    let next = match rotation.next().filter(|_| rotation.moves_on()) {
        Some(map) => format!(
            "{} ({} {})",
            level_display_name(world.resource::<GamePaths>(), &map.level),
            mode_label(&map.mode),
            map.size
        ),
        None => "this map again".into(),
    };
    format!(
        "name: {}\nmap: {level} ({}), {} {}\nround: {round}\nplayers: {humans} / {}, bots: {bots}\nfriendly fire: {}, respawn: {} s, tickets: {}%\nnext map: {next}",
        settings.name,
        settings.level,
        mode_label(&settings.mode),
        settings.size,
        settings.max_clients,
        if settings.friendly_fire { "on" } else { "off" },
        settings.respawn_seconds,
        settings.ticket_ratio,
    )
}

fn players(world: &mut World) -> String {
    let host = world.get_resource::<HostPlayer>().map(|h| h.0);
    let mut rows: Vec<(u32, String)> = Vec::new();
    let mut query = world.query::<(Entity, &Player, &Team, &Score, Option<&PlayerId>, Option<&PlayerClient>)>();
    let found: Vec<_> = query
        .iter(world)
        .map(|(e, p, t, s, id, client)| (e, p.clone(), *t, *s, id.map_or(0, |i| i.0), client.map(|c| c.0)))
        .collect();
    for (entity, player, team, score, id, client) in found {
        let (ping, address) = match client {
            Some(client) => {
                let rtt = world
                    .get::<ConnectedClientStats>(client)
                    .map_or(0.0, |s| s.rtt * 1000.0);
                let address = client_address(world, client).map_or(String::new(), |a| a.to_string());
                (format!("{rtt:.0}"), address)
            }
            None if Some(entity) == host => ("host".into(), String::new()),
            None => ("bot".into(), String::new()),
        };
        let team = match team {
            Team::One => "1",
            Team::Two => "2",
            Team::Spectator => "-",
        };
        let name: String = player.name.chars().take(22).collect();
        rows.push((
            id,
            format!(
                "{id:>3}  {name:<22} {team:>4} {:>6} {:>4} {:>4} {ping:>5}  {address}",
                score.score, score.kills, score.deaths
            ),
        ));
    }
    rows.sort_by_key(|(id, _)| *id);
    let mut out = format!(
        "{:>3}  {:<22} {:>4} {:>6} {:>4} {:>4} {:>5}  address\n",
        "id", "name", "team", "score", "K", "D", "ping"
    );
    for (_, row) in rows {
        out += row.trim_end();
        out.push('\n');
    }
    out
}

/// A player by id number, exact name, or a part of the name that only one player has.
fn find_player(world: &mut World, key: &str) -> Result<(Entity, String), String> {
    if key.is_empty() {
        return Err("Which player? Give an id (see `players`) or a name.".into());
    }
    let players: Vec<(Entity, String, u32)> = world
        .query::<(Entity, &Player, Option<&PlayerId>)>()
        .iter(world)
        .map(|(e, p, id)| (e, p.name.clone(), id.map_or(0, |i| i.0)))
        .collect();
    if let Ok(id) = key.parse::<u32>()
        && let Some((e, name, _)) = players.iter().find(|(_, _, i)| *i == id)
    {
        return Ok((*e, name.clone()));
    }
    let key_lower = key.to_lowercase();
    if let Some((e, name, _)) = players.iter().find(|(_, n, _)| n.to_lowercase() == key_lower) {
        return Ok((*e, name.clone()));
    }
    let matches: Vec<_> = players
        .iter()
        .filter(|(_, n, _)| n.to_lowercase().contains(&key_lower))
        .collect();
    match matches.as_slice() {
        [(e, name, _)] => Ok((*e, name.clone())),
        [] => Err(format!("No player `{key}`.")),
        many => Err(format!(
            "`{key}` could be {}.",
            many.iter().map(|(_, n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Splits off the first word.
fn first_word(args: &str) -> (&str, &str) {
    args.split_once(char::is_whitespace)
        .map_or((args, ""), |(word, rest)| (word, rest.trim()))
}

fn say(world: &mut World, text: &str) -> Answer {
    if text.is_empty() {
        return Err("Say what?".into());
    }
    announce(world, format!("Admin: {text}"));
    Ok(format!("Said: {text}"))
}

/// Disconnects `player` with `reason` (bots are just removed). Tells everyone if `announce`.
pub fn kick(world: &mut World, player: Entity, reason: &str, announce_it: bool) -> Answer {
    let name = world.get::<Player>(player).map(|p| p.name.clone()).unwrap_or_default();
    if world.get::<BotBrain>(player).is_some() {
        remove_bots(world, &[player]);
        let mut settings = world.resource_mut::<ServerSettings>();
        settings.bots = settings.bots.saturating_sub(1);
        return Ok(format!("Removed {name}."));
    }
    let Some(client) = world.get::<PlayerClient>(player).map(|c| c.0) else {
        return Err("The host can't be kicked.".into());
    };
    world.write_message(ToClients {
        targets: SendTargets::Single(ClientId::Client(client)),
        message: Kicked {
            reason: reason.to_string(),
        },
    });
    // Disconnects after the message is sent.
    world.write_message(DisconnectRequest { client });
    info!("kicked {name}: {reason}");
    if announce_it {
        announce(world, format!("{name} was kicked: {reason}"));
    }
    Ok(format!("Kicked {name}."))
}

fn kick_command(world: &mut World, args: &str) -> Answer {
    let (key, reason) = first_word(args);
    let (player, _) = find_player(world, key)?;
    let reason = if reason.is_empty() { "Kicked by an admin" } else { reason };
    kick(world, player, reason, true)
}

fn ban_command(world: &mut World, args: &str) -> Answer {
    let (key, rest) = first_word(args);
    let (player, name) = find_player(world, key)?;
    let (minutes, reason) = match first_word(rest) {
        (word, reason) if word.parse::<u64>().is_ok() => (word.parse::<u64>().ok().filter(|m| *m > 0), reason),
        _ => (None, rest),
    };
    let Some(client) = world.get::<PlayerClient>(player).map(|c| c.0) else {
        return Err("Only players on other machines can be banned.".into());
    };
    let address = client_address(world, client);
    world.resource_mut::<BanList>().add(Ban {
        name: name.clone(),
        address,
        reason: reason.to_string(),
        until: minutes.map(|m| unix_now() + m * 60),
    });
    let length = minutes.map_or("for good".to_string(), |m| format!("for {m} min"));
    let why = if reason.is_empty() { String::new() } else { format!(": {reason}") };
    kick(world, player, &format!("Banned {length}{why}"), false)?;
    announce(world, format!("{name} was banned {length}{why}"));
    Ok(format!("Banned {name} {length}."))
}

fn unban(world: &mut World, key: &str) -> Answer {
    if key.is_empty() {
        return Err("Unban whom? Give a name or address (see `bans`).".into());
    }
    match world.resource_mut::<BanList>().remove(key) {
        0 => Err(format!("Nobody banned as `{key}`.")),
        n => Ok(format!("Lifted {n} ban(s).")),
    }
}

fn list_bans(world: &mut World) -> String {
    let now = unix_now();
    let bans: Vec<String> = world
        .resource::<BanList>()
        .bans
        .iter()
        .filter(|b| b.until.is_none_or(|u| u > now))
        .map(Ban::describe)
        .collect();
    if bans.is_empty() { "Nobody is banned.".into() } else { bans.join("\n") }
}

fn map(world: &mut World, args: &str) -> Answer {
    let mut words = args.split_whitespace();
    let Some(key) = words.next() else {
        return Err("Which level? See `levels`.".into());
    };
    let levels = rotation::available_levels(world.resource::<GamePaths>());
    let key_lower = key.to_lowercase();
    let level = match levels.iter().find(|l| **l == key_lower) {
        Some(level) => level.clone(),
        None => {
            let matches: Vec<&String> = levels.iter().filter(|l| l.contains(&key_lower)).collect();
            match matches.as_slice() {
                [level] => (*level).clone(),
                [] => return Err(format!("No level `{key}`. See `levels`.")),
                many => {
                    let names: Vec<&str> = many.iter().map(|l| l.as_str()).collect();
                    return Err(format!("`{key}` could be {}.", names.join(", ")));
                }
            }
        }
    };
    let settings = world.resource::<ServerSettings>();
    let mut entry = MapEntry {
        level,
        mode: settings.mode.clone(),
        size: settings.size,
        bots: None,
    };
    for word in words {
        match word.parse::<u32>() {
            Ok(size) => entry.size = size,
            Err(_) => entry.mode = word.to_string(),
        }
    }
    rotation::change_map(world, &entry);
    Ok(format!("Changing map to {} ({} {}).", entry.level, entry.mode, entry.size))
}

fn next_map(world: &mut World) -> Answer {
    rotation::advance(world);
    let settings = world.resource::<ServerSettings>();
    Ok(format!("Changing map to {} ({} {}).", settings.level, settings.mode, settings.size))
}

fn rotation_list(world: &mut World) -> String {
    let rotation = world.resource::<MapRotation>();
    if rotation.maps.is_empty() {
        return "No rotation: the current map repeats.".into();
    }
    rotation
        .maps
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let marker = if rotation.current == Some(i) { ">" } else { " " };
            let bots = m.bots.map_or(String::new(), |b| format!(", {b} bots"));
            format!("{marker} {}. {} ({} {}{bots})", i + 1, m.level, m.mode, m.size)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn restart(world: &mut World) -> Answer {
    let map = rotation::current_map(world);
    rotation::change_map(world, &map);
    Ok("Restarting the map.".into())
}

/// Takes bots out of the match, with their soldiers.
fn remove_bots(world: &mut World, bots: &[Entity]) {
    for bot in bots {
        if let Some(Controls(soldier)) = world.get::<Controls>(*bot) {
            let soldier = *soldier;
            let _ = world.try_despawn(soldier);
        }
        let _ = world.try_despawn(*bot);
    }
}

fn bots(world: &mut World, args: &str) -> Answer {
    let Ok(count) = args.parse::<u32>() else {
        return Err("How many? `bots <count>`".into());
    };
    let count = count.min(128);
    world.resource_mut::<ServerSettings>().bots = count;
    world.resource_mut::<MapRotation>().default_bots = count;
    let mut bots: Vec<Entity> = world
        .query_filtered::<Entity, With<BotBrain>>()
        .iter(world)
        .collect();
    // The newest go first.
    bots.sort_by_key(|e| std::cmp::Reverse(world.get::<PlayerId>(*e).map_or(0, |i| i.0)));
    let surplus = bots.len().saturating_sub(count as usize);
    remove_bots(world, &bots[..surplus]);
    Ok(format!("{count} bots."))
}

fn tickets(world: &mut World, args: &str) -> Answer {
    let mut words = args.split_whitespace();
    let Some(count) = words.next().and_then(|w| w.parse::<f32>().ok()) else {
        return Err("How many? `tickets <count> [team]`".into());
    };
    let teams: Vec<usize> = match words.next() {
        Some("1") => vec![0],
        Some("2") => vec![1],
        None => vec![0, 1],
        Some(other) => return Err(format!("Team 1 or 2, not `{other}`.")),
    };
    let mut query = world.query_filtered::<&mut Tickets, With<MatchInfo>>();
    let Some(mut tickets) = query.iter_mut(world).next() else {
        return Err("No round is running.".into());
    };
    for team in teams {
        tickets.remaining[team] = count.max(0.0);
        tickets.start[team] = tickets.start[team].max(count);
    }
    Ok(format!("Tickets: {:.0} / {:.0}.", tickets.remaining[0], tickets.remaining[1]))
}

fn friendly_fire(world: &mut World, args: &str) -> Answer {
    let mut settings = world.resource_mut::<ServerSettings>();
    settings.friendly_fire = match args {
        "" => !settings.friendly_fire,
        "on" | "1" | "true" => true,
        "off" | "0" | "false" => false,
        other => return Err(format!("`on` or `off`, not `{other}`.")),
    };
    let state = if settings.friendly_fire { "on" } else { "off" };
    announce(world, format!("Friendly fire is {state}."));
    Ok(format!("Friendly fire {state}."))
}

