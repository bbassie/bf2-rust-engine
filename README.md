# bf2-rust-engine

A from-scratch reimplementation of a Battlefield 2 style combined-arms shooter in Rust with the
[Bevy](https://bevyengine.org) engine: multiplayer with dedicated servers, BF2-style bots, and
a codebase built to be extended.

**No Battlefield 2 files are part of this repository.** The game ships with a small built-in
test level. To play on the real maps you point the importer at *your own* BF2 installation;
it converts what it needs into open formats in a local `imported/` folder that is never
committed or redistributed.

## Status

Playable infantry conquest. What works today:

- Dedicated server (headless) and client, plus listen server and singleplayer from the client.
- Server-authoritative soldier movement with client-side prediction and reconciliation.
- Remote players are interpolated; everything is rendered smoothly independent of the 60 Hz tick.
- Conquest after BF2's rules: capturing flags, ticket bleed, rounds. A deploy screen with the
  level's map for picking a kit and a spawn point.
- Infantry combat: BF2's kits and weapons with their fire rates, deviation, recoil and
  projectiles; first-person arms and weapons with BF2's animations; third-person soldiers.
- Bots that use the same input path as humans: they capture flags and fight.
- Importer for BF2 levels: terrain with detail texture blending, roads, sky, water, ~1800
  static objects on Strike at Karkand (meshes as glTF with BF2's layered materials, collision
  as trimeshes), conquest/co-op layouts, teams, kits, weapons, soldiers and animations,
  English names. All 30 levels import.
- Real-time lighting (sun shadows, SSAO), so maps can be relit (e.g. night versions).
- A procedural test range so everything can be tested without BF2 data.

See [docs/ROADMAP.md](docs/ROADMAP.md) for what comes next.

## Layout

| Crate | Purpose |
|---|---|
| [`bf2_formats`](crates/bf2_formats) | Reads BF2 files: archives/VFS, `.con` scripts, meshes, terrain. No engine dependency. |
| [`bf2_import`](crates/bf2_import) | CLI: converts a BF2 install into the game's own formats. |
| [`game_data`](crates/game_data) | The game's own open data formats (RON). Written by the importer, hand-editable by modders. |
| [`game_shared`](crates/game_shared) | Code both sides need: network protocol, simulation (movement), level loading. |
| [`game_server`](crates/game_server) | Server authority: connections, spawning, rules, bots. Library + `server` binary. |
| [`game_client`](crates/game_client) | Rendering, input, prediction, HUD. `client` binary; can also host. |

## Running

Requires a recent stable Rust toolchain. The first build takes a while (Bevy).

```sh
# Singleplayer on the built-in test range with 7 bots
cargo run -p game_client

# Strike at Karkand (after importing, see below) with 15 bots
cargo run -p game_client -- --level strike_at_karkand --bots 15

# Dedicated server + two clients
cargo run -p game_server -- --level strike_at_karkand --bots 10
cargo run -p game_client -- --connect 127.0.0.1 --name Alice
cargo run -p game_client -- --connect 127.0.0.1 --name Bob

# Listen server
cargo run -p game_client -- --host --bots 8
```

Controls: click to capture the mouse, `Esc` to release. `WASD` move, `Shift` sprint, `Space`
jump, `Ctrl` crouch, `Z` prone. `LMB` fire, `RMB` zoom, `R` reload, `B` fire mode, `1`-`6` or
the mouse wheel switch weapons. `Enter` opens the deploy screen, `Tab` the scoreboard, `V`
toggles third person.

For day-to-day development use the dev profile (`cargo run -p game_client`): dependencies are
fully optimized there too, so it runs nearly as fast as `--release` and rebuilds much faster.
`cargo build --profile dist` makes the fully optimized (slow to compile) shipping build.

### Importing BF2 content

```sh
cargo run -p bf2_import --release -- --bf2 "C:\Program Files (x86)\EA Games\Battlefield 2"
cargo run -p game_client --release -- --level strike_at_karkand
```

Converted files go to `./imported` (override with `--out`, and point the game at it with
`--imported` or the `GAME_IMPORTED_DIR` environment variable).

## Legal

Battlefield 2 is a trademark of Electronic Arts. This project is not affiliated with or
endorsed by EA or DICE. It contains no EA code or assets; the importer only reads files
from an installation the user already owns, on the user's own machine.
