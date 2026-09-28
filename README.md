# bf2-rust-engine

A from-scratch reimplementation of a Battlefield 2 style combined-arms shooter in Rust with the
[Bevy](https://bevyengine.org) engine: multiplayer with dedicated servers, BF2-style bots, and
a codebase built to be extended.

**No Battlefield 2 files are part of this repository.** The game ships with a small built-in
test level. To play on the real maps you point the importer at *your own* BF2 installation;
it converts what it needs into open formats in a local `imported/` folder that is never
committed or redistributed.

## Status

All 30 BF2 levels (including Special Forces and the booster packs) import and play:

- **Conquest and co-op** after BF2's rules: flags, ticket bleed, squads, the commander (orders,
  artillery guns, UAV, satellite scan, supply drops), commo rose and spotting, BF2's voice-overs.
- **Infantry:** BF2's kits and weapons (fire rates, deviation, recoil, scopes), per-bone hit
  zones, lag compensation, grenades, launchers, C4 and mines, man-down and revive, medic,
  support and engineer abilities, Special Forces gadgets (night vision, tear gas, flashbangs,
  grappling hooks, ziplines), BF2's movement values, ladders.
- **Vehicles:** jeeps, APCs, tanks, jets, helicopters, boats and stationary weapons with BF2's
  handling data, gearbox and per-face armour, seats, interiors and sights, a vehicle HUD,
  guided missiles, countermeasures, parachutes, and driver prediction.
- **Bots** that use the same inputs as players: BF2's strategy, squads, an AI commander, and
  vehicles on land, water and in the air.
- **Presentation:** real-time lighting from BF2's per-map values (the sun stays dynamic, so maps
  can be relit), sky light, BF2's baked sky occlusion, SSAO, BF2's materials, particle effects,
  audio, undergrowth, water, LODs, and a modern UI of our own.
- **Servers:** dedicated server, listen server and singleplayer; map rotation, RCON, chat
  admin commands, stats, LAN and internet server browser (master server), and clients that
  download a server's mods (or, if its admin allows, all content) when joining.
- **Modding:** new content authored directly in glTF + RON ([docs/MODDING.md](docs/MODDING.md)).

See [docs/ROADMAP.md](docs/ROADMAP.md) for what comes next and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how it fits together.

## Layout

| Crate | Purpose |
|---|---|
| [`bf2_formats`](crates/bf2_formats) | Reads BF2 files: archives/VFS, `.con` scripts, meshes, terrain. No engine dependency. |
| [`bf2_import`](crates/bf2_import) | CLI: converts a BF2 install into the game's own formats. |
| [`game_data`](crates/game_data) | The game's own open data formats (RON). Written by the importer, hand-editable by modders. |
| [`game_shared`](crates/game_shared) | Code both sides need: network protocol, simulation (soldiers, vehicles, projectiles), level loading. |
| [`game_server`](crates/game_server) | Server authority: connections, rules, bots and navigation. Library + `server` binary. |
| [`game_client`](crates/game_client) | Rendering, audio, input, prediction, UI. `client` binary; can also host. |
| [`master_server`](crates/master_server) | Lists internet servers for the server browser. |

Scripted checks of the client live in [`scenarios/`](scenarios/README.md).

## Running

Requires a recent stable Rust toolchain. The first build takes a while (Bevy); later builds
take seconds.

```sh
cargo run -p game_client                         # main menu (singleplayer, host, join, settings)
cargo run -p game_client -- --level strike_at_karkand --bots 15

# Dedicated server + clients
cargo run -p game_server -- --level strike_at_karkand --bots 10
cargo run -p game_client -- --connect 127.0.0.1 --name Alice
```

Servers listen on localhost only unless started with `--public`.

Default controls (all rebindable in Settings): `WASD` move, `Shift` sprint, `Space` jump, `Ctrl`
crouch, `Z` prone, mouse fire and zoom, `R` reload, `B` fire mode, `1`-`9` or the wheel switch
weapons, `E` enter and leave vehicles (`F1`-`F8` switch seats), `G` countermeasures, `V` third
person, `Q` commo rose, `M` map, `N` minimap rotation, `Caps Lock` commander screen, `Enter`
deploy screen, `Tab` scoreboard, `T`/`Y`/`U` chat, `Esc` menu.

For day-to-day development use the dev profile: dependencies are fully optimized there too, so
it runs nearly as fast as `--release` and rebuilds much faster. `cargo build --profile dist`
makes the fully optimized shipping build.

### Importing BF2 content

```sh
cargo run -p bf2_import -- --bf2 "C:\Program Files (x86)\EA Games\Battlefield 2" level --all
```

This converts every level of every installed mod (a few minutes) into `./imported` (override
with `--out`, and point the game at it with `--imported` or `GAME_IMPORTED_DIR`). `level
<name>...` imports single levels; `list` shows what is installed.

## Legal

Battlefield 2 is a trademark of Electronic Arts. This project is not affiliated with or
endorsed by EA or DICE. It contains no EA code or assets; the importer only reads files
from an installation the user already owns, on the user's own machine.
