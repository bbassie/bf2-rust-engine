# Modding

The game only reads its own formats: RON descriptions (`game_data` types), glTF 2.0 meshes
(`.glb`), and DDS or PNG textures. `bf2-import` writes BF2's content in these formats to
`imported/`. A **mod** is a folder laid out like `imported/` whose files add to it or replace
parts of it. Mods never change `imported/`, and nothing in them has to come from BF2.

`mods/sample` is a working example (made by `tools/make_sample_mod.py`):

- Sample Valley, a level built from scratch: heightmap, water, its own meshes, collision
  meshes, textures and templates, conquest and co-op layouts, and a PNG minimap.
- A new weapon, `sample_carbine`, made from the imported M4 with a patch file.
- A new kit, `sample_rifleman`, made from the US assault kit. It carries the carbine and is
  the US assault slot on Sample Valley.

Play it with `client --level sample_valley --team 2`, or pick it in the menu.

## Where mods live

```text
mods/                        --mods <dir> or $GAME_MODS_DIR (default: ./mods)
  my_mod/
    mod.ron                  (name: "My mod", description: "...", priority: 0, enabled: true)
    levels/my_level/...      new levels
    templates/*.ron          object templates (statics)
    objects/**/*.glb, *.png  meshes and textures
    kits/, weapons/, soldiers/, vehicles/
```

Every subfolder of the mods folder that has a `mod.ron` is a mod. A mod with
`enabled: false` is ignored. When several mods have the same file, the one with the higher
`priority` wins; on a tie, the folder that sorts first wins. The client and the server log
the mods they use at startup.

The server and every client need the same mods. By default a server shares its mods, and
joining clients download them (see [Sharing content from a server](#sharing-content-from-a-server)),
so players don't install anything. A server that shares nothing needs clients with the same
mods; one without them sees the wrong models, or none, and predicts weapons wrongly.

### How files are found

Every path in the formats is relative to the imported root, e.g.
`objects/weapons/handheld/usrif_m4/meshes/usrif_m4_1p.glb`. The game looks for the file in
each mod, highest priority first, then in `imported/`:

- The client's `imported://` asset source works this way for meshes, textures and sounds.
  Textures referenced from inside a glTF are found the same way, so a mod can swap a
  texture without touching the mesh.
- `GamePaths::find`, `read_ron` and `level_dir` work this way for everything the game reads
  directly: levels, templates, collision meshes, kits, weapons, soldiers and vehicles.

A level lives in a single folder: `levels/<name>/` comes from the first mod that has
`levels/<name>/level.ron`. Paths "relative to the level folder" (heightmap, colour maps)
point into that folder.

### Replacing, patching and deriving

For the RON files the game reads directly (`level.ron`, `templates/`, `kits/`, `weapons/`,
`soldiers/`, `vehicles/`), there are three options:

1. **Replace** the file: put a complete `weapons/usrif_m4.ron` in the mod.
2. **Patch** it: put `weapons/usrif_m4.patch.ron` in the mod with only the fields to
   change. Nested structures merge field by field. Lists and plain values are replaced.

   ```ron
   (rounds_per_minute: 750.0, projectile: (damage: 30.0), fire_modes: ["Single", "Auto"])
   ```

3. **Derive** a new file from another with a patch that names a `base` (a file in the same
   folder, without `.ron`). This is how the sample makes its carbine:

   ```ron
   // weapons/sample_carbine.patch.ron
   (base: "usrif_m4", name: "sample_carbine", display_name: "Sample Carbine", magazine_size: 40)
   ```

Patches stack: each mod's patch applies on top of the mods below it. A complete file in a
mod hides everything below it, including lower patches.

**In patches, write enum values as strings:** `"Auto"`, `"Stop"`, and `{"Some": ...}` for
anything that isn't a plain value. Patches go through a generic value tree that can't tell
a bare `Auto` from `()`. `Some(x)` and `None` work as usual.

Don't copy files from `imported/` into a mod you want to share. They are EA's data, converted.
Patches and derived files only hold your changes, and names or paths of imported content.

## Levels

A level is `levels/<name>/level.ron` (`game_data::LevelDesc`) plus the files it refers to.
The fields, with their doc comments, are in `crates/game_data/src/level.rs`. The important
ones:

| field | meaning |
|---|---|
| `name`, `display_name` | folder name, and the name shown in menus |
| `terrain` | `heightmap` (little-endian `u16`, `resolution`² samples, row-major, row 0 at −Z), `spacing` (m), `height_scale` (m per unit), `origin` (world position of sample 0,0). Optional `color_maps`, `detail_textures`, `lightmaps`: without them the ground is shaded by height and slope. |
| `water` | `height` and optional colours and maps |
| `environment` | sun direction and colours, fog, view distance, optional sky dome |
| `statics` | placed object templates: `{"template": "name", "position": (x, y, z), "rotation": (x, y, z, w), "scale": (1, 1, 1)}` |
| `game_modes` | layouts: `mode` (`gpm_cq`, `gpm_coop`, `gpm_rush`, `gpm_breakthrough`, `gpm_tdm`), `size` (16/32/64), `control_points`, `spawn_points`, `vehicle_spawners`, extra `statics`; `staged` for Rush and Breakthrough (see [Game mode layouts](#game-mode-layouts)) |
| `teams` | two teams: `name`, `kits` (7 slots of `(kit, soldier)`), `tickets` per size, `ticket_loss_per_minute`, `language`, `voice` |
| `minimap` | top-down image (DDS or PNG) covering the terrain, north up, relative to the imported root |
| `flag_models` | flag pole and flag meshes for control points |

A control point has an `id` (spawn points refer to it), `position`, `radius`,
`initial_team` (0 neutral), `uncapturable` for main bases, `area_value` (ticket bleed) and
capture times. Spawn points are `{"control_point": "id", "position": ..., "rotation": ...}`.
Co-op (`gpm_coop`) layouts use the same format. Humans all play on one team and bots fill
both (see `game_server::coop`).

Bots need nothing extra. The server builds their navigation grid from the terrain and the
collision meshes, and their strategy from the control points. An optional
`levels/<name>/ai.ron` adds BF2-style strategic areas (see `game_data::ai`).

### Game mode layouts

Rush (`gpm_rush`) and Breakthrough (`gpm_breakthrough`) are attack/defend modes played in
stages (see `game_data::modes` and ARCHITECTURE.md, "Game modes"). A level needs nothing for
them: when it loads, the game makes a Rush and a Breakthrough layout from every conquest
layout it has (the attackers are the side holding fewer flags at the start; the flags between
the bases are grouped into stages in the order they are fought over; two charges per Rush
stage are placed beside its flags and later moved onto walkable ground). To see what it made,
run `cargo test -p game_data generated_layouts -- --ignored --nocapture` (all imported levels,
or `LEVELS=strike_at_karkand,...`).

To make your own, write the layout by hand. It replaces the generated one of the same mode
and size. Either put it in the level's `game_modes`, or, for a level you don't own (an
imported one), in `levels/<name>/modes.ron` in a mod: its layouts replace the level's of the
same mode and size or add to them, and the mod doesn't touch `level.ron`. A layout
`based_on` another takes the control points, spawn points, vehicle spawners and objects it
doesn't list from that one, so it holds nothing but its own stages (and no EA data):

```ron
// mods/my_rush/levels/strike_at_karkand/modes.ron
(
    game_modes: [
        (
            mode: "gpm_rush",
            size: 32,
            based_on: Some((mode: "gpm_cq", size: 32)),
            staged: Some((
                attacker: 2,                 // the team attacking: 1 or 2
                tickets: 120.0,              // at the start and after every stage taken
                arm_seconds: 4.0,            // optional: holding use to arm, to defuse,
                defuse_seconds: 6.0,         // and the fuse of an armed charge
                fuse_seconds: 30.0,
                stages: [
                    (
                        name: "The Hotel",
                        charges: [
                            (name: "A", position: (-205.0, 156.0, -13.0), yaw: 180.0),
                            (name: "B", position: (-185.0, 156.0, -16.0), yaw: 180.0,
                             template: Some("woodencrate_destructible_tools")),
                        ],
                        // Control point ids: who spawns where during this stage (points in
                        // neither list are neutral; nothing is captured in the Rush).
                        attacker_spawns: ["305"],
                        defender_spawns: ["302", "306", "307"],
                    ),
                    // ... more stages; the attackers win by destroying the last.
                ],
            )),
        ),
        (
            mode: "gpm_breakthrough",
            size: 32,
            based_on: Some((mode: "gpm_cq", size: 32)),
            staged: Some((
                attacker: 2,
                tickets: 170.0,
                stages: [
                    // A sector: its control points (ids), all of which the attackers must
                    // hold at once. Spawns follow who holds the flags.
                    (name: "Old Town", control_points: ["301", "302"]),
                    (name: "Market", control_points: ["306", "307"]),
                ],
            )),
        ),
    ],
)
```

Charges are placed exactly where a hand-made layout says (`approximate: true` lets the server
move one onto walkable ground nearby). `yaw` is in degrees, counter-clockwise from north seen
from above; `template` names any object template (`templates/<name>.ron`), BF2's
`xp1_generator` by default (a marker box of the game's own if there is none). Team deathmatch
(`gpm_tdm`) layouts are ordinary layouts: their control points are where each side spawns.

### Objects (templates)

A static object is `templates/<name>.ron` (`game_data::ObjectDesc`): a list of parts, each
with a visual `mesh` (`.glb` and `mesh_index`, the mesh in that file), an optional
`collision` mesh (`.glb` and `collision_part`) and a placement relative to the object. Parts
are written as maps because their placement is flattened into them:

```ron
(
    name: "sample_crate",
    kind: "SimpleObject",
    parts: [
        {
            "mesh": Some("objects/sample/meshes/crate.glb"),
            "collision": Some("objects/sample/meshes/crate.collision.glb"),
            "position": (0.0, 0.0, 0.0),
        },
    ],
)
```

`armor` makes an object destroyable (`hit_points`, damage `material`, optional explosion). Its
parts' `wreck_mesh` and `wreck_collision` replace the intact ones when it is destroyed.

Levels of detail are optional. A part's `lods` lists lower-detail meshes, most detailed
first, each with the camera distance in meters from which it replaces the one before
(`"lods": [(mesh: "objects/sample/meshes/crate_lod1.glb", distance: 50.0)]`, the same
`mesh_index` in each file; `wreck_lods` does the same for `wreck_mesh`). The object's
`draw_distance` (meters) is how far away it is drawn at all. Leave it out for objects that
should reach the fog. The client cross-fades around each distance. The view distance setting
scales `draw_distance`, and zooming in scales both (BF2's rule).

## glTF conventions (Blender)

Engine space is **meters, right-handed, +Y up, −Z forward (north), +X east**, the same as
glTF. In Blender (Z up, −Y forward) export with the glTF exporter's defaults:

- **Format**: glTF Binary (`.glb`). Textures can be embedded or next to it. A relative URI
  like `../textures/crate.png` is resolved through the mods too.
- **Transform**: `+Y Up` on. Model at real size (1 unit = 1 m) with the origin where the
  object meets the ground. The front of an object faces **−Z in glTF** (Blender's −Y).
  Apply scale and rotation (Ctrl+A) before exporting.
- **Geometry**: triangulated, with UVs and normals. Tangents only if you use a normal map.
  One mesh per object part. `mesh_index` picks a mesh by its order in the file.
- **Materials**: the Principled BSDF becomes glTF metallic-roughness. The game uses the
  base colour (factor and texture), the normal map, roughness, and alpha (Blend or Alpha
  Clip for foliage and fences); metallic is ignored. Imported BF2 meshes carry a `bf2`
  object in the material's extras (BF2's shader technique and map list). Leave it out:
  materials without it are drawn as plain PBR.
- **Collision meshes**: a separate `.glb` (by convention `name.collision.glb`) with one mesh
  per part and kind, named `part{N}_{kind}`: `part0_soldier` (what soldiers walk on and
  bump into), `part0_vehicle`, `part0_projectile` (what bullets hit). Soldiers use
  `soldier` if present, else `vehicle`, else `projectile`. Keep them simple and closed; they
  become triangle-mesh colliders (vehicle hulls become convex hulls).
- **Units in data**: meters, seconds, degrees in RON unless a field says otherwise. Rotations
  are quaternions `(x, y, z, w)` in engine space: a yaw of `a` degrees (counter-clockwise
  seen from above) is `(0, sin(a/2), 0, cos(a/2))`.

### Soldiers

A soldier body is `soldiers/<name>.ron` (`game_data::SoldierDesc`):

- `mesh`: a `.glb` with the skinned third-person mesh, its skeleton and every animation clip.
- `animations`: the clip names.
- `mesh_1p`: the first-person arms, also skinned.
- `hit_zones`: capsules on bones, for where bullets hit.

The skeletons are BF2's: `3p_setup` (80 bones) for bodies and `1p_setup` (70 bones, roots
`Camerabone` and `torus`) for arms. Keep the bone names. Weapons attach to the bones
`mesh1`..`mesh16`, and the first-person camera sits on `Camerabone`.

Clip names follow BF2's, with a `3p_` prefix for the body:

- Stances: `3p_stand`, `3p_crouchstill`, `3p_pronestill`, `3p_sprint`.
- Moving: `3p_walkforward/backward/left/right`, `3p_runforward`, `3p_runbackward`,
  `3p_strafeleft`, `3p_straferight`, `3p_crouchforward`, ..., `3p_proneforward`, ...
- Turning: `3p_standturnleft/right`, `3p_crouchturnleft/right`.
- Jumps: `3p_stilljumpstart/loop/end` and the same for `runforward`, `runbackward`,
  `strafeleft`, `straferight`.
- Ladders: `3p_climbup`, `3p_climbdownfast`.
- Revive: `3p_reviveonback`.

The upper body plays the weapon's clips: `stand`, `standfire`, `reload`, `standdeploy`, and
the prone versions. `crates/game_client/src/render/soldiers.rs` (module `clips`) has the
full list.

### Weapons and kits

A kit is `kits/<name>.ron` (`game_data::KitDesc`): `name`, `kind` (`Assault`, `Medic`, ...)
and its `weapons`, which are names of `weapons/<name>.ron`. Teams list their kits in the
level's `teams[].kits`.

A weapon (`game_data::WeaponDesc`, `crates/game_data/src/weapon.rs`) has:

- Meshes: `mesh_1p` and `mesh_3p`, with parts on the bones `mesh1`..`meshN`.
- Animations: `animations_1p` and `animations_3p` (`.glb` files of clips). First-person
  clips: `stand`, `run`, `sprint`, `fire`, `reload`, `deploy`, `zoom_stand`, `zoom_fire`,
  and for bolt actions `load`.
- Fire: `rounds_per_minute`, `fire_modes`, magazine and reload, the `projectile` (speed,
  damage, falloff, gravity, explosion), `deviation`, `recoil`, `zoom`, `sounds`.

Deriving from an imported weapon (see above) is the easiest way to make a new one: you
inherit its meshes, animations and sounds and change only the numbers.

### Vehicles

A vehicle is `vehicles/<name>.ron` (`game_data::VehicleDesc`, `crates/game_data/src/vehicle.rs`):

- `parts`: a tree whose part 0 is the hull. Each part has a mesh, a collision mesh, a rest
  placement and an optional joint (which input turns which axis, within which limits).
- `wheels` (radius, spring, damping), `seats` (where each occupant sits and looks from),
  `entry_points`, `weapons`, `sounds`.
- Physics: mass, top speed, drive and brake force, grip.

A level places vehicles with `vehicle_spawners` in its layouts. The spawner's `templates`
name one vehicle per team.

## Sharing content from a server

A server can hand its content to the clients that join it: its mods by default, and, if its
admin says so, the imported BF2 assets too. Players then join a modded server without
installing the mod, and a player without Battlefield 2 can join a server that shares
everything. Only data is shared (RON, glTF, textures, sounds, heightmaps: the file types the
game reads), never code, and every file is checked against its hash before it is used.

### For server admins

| setting | config file (`server --config`) | command line | default |
|---|---|---|---|
| what is shared | `content: Off` / `Mods` / `All` | `--content off\|mods\|all` | `Mods` |
| download host | `download_url: "https://..."` | `--download-url <url>` | none |
| endpoint port (TCP) | `content_port: 16567` | `--content-port <port>` | the game port |
| time to get the content right | `content_sync_timeout: 600` | | 600 s |
| identity key | `identity_file: "identity.key"` | `--identity <file>` | `identity.key` in the data folder |

- **`Mods`** shares every enabled mod (`mods/<name>/`). **`All`** also shares the imported
  BF2 assets. **`Off`** shares nothing; clients then need the server's mods themselves.
- **Licensing.** Mods authored for this engine are yours to share, as long as they don't
  contain files copied from `imported/` (see [Replacing, patching and deriving](#replacing-patching-and-deriving)).
  The imported assets are EA's copyrighted content, converted from your own copy of
  Battlefield 2. `All` redistributes them to everyone who joins, so it is a separate choice
  that is never on by default: only turn it on if you may share them (for example on a
  private LAN where every player owns the game).
- The endpoint is a small HTTP server on **TCP** at the game's port number (the game itself
  uses UDP). It listens where the game does: this machine only, unless the server is
  `--public`. For players on other machines, open TCP as well as UDP for the port.
  Browsers (LAN and master server) show what a server shares and roughly how much a client
  downloads, e.g. `shares mods, 9.1 MB`.
- `tiny_http` (what the endpoint is built on) doesn't expose a real per-connection timeout or
  connection cap, so the endpoint caps requests itself instead: per source address, in total,
  and separately for file transfers (a handful of slow downloads can't starve out the small,
  fast manifest/identity checks other joining players need). None of this is configurable;
  it's sized generously for normal play.
- Clients get the files of the maps the server plays: the current one and the rotation. The
  server works out which files each level uses from the references in the data, so a
  client that joins a BF2 map downloads that map and what it uses (about 0.5 GB with `All`),
  not the whole import (about 4 GB). If an admin changes to a map outside the rotation,
  clients fetch its files and briefly rejoin.
- Files are hashed (BLAKE3) when the server starts. The hashes and the levels' files are
  remembered (`content-index.txt`, `content-levels.txt` in the server's data folder,
  `%APPDATA%\bf2-rust-engine\server` on Windows), so only the first start with `All` takes
  a minute or two. Joining clients wait meanwhile ("preparing").
- **Download host.** `server export-content --out <folder> [--content all]` writes the
  shared files named by their hash, plus `manifest.ron`. Upload the folder to a static web
  host or CDN and give its URL as `download_url`. Clients fetch `<url>/<hash>` from there
  and fall back to the server. Export again when the content changes (unchanged files are
  skipped).
- Listen servers (Host in the menu) share their mods the same way.
- **The server's identity.** On its first start a server makes an Ed25519 key,
  `identity.key` in its data folder (`--identity` or `identity_file` put it elsewhere).
  Players see its fingerprint (`3f2a 91bc ...`, also in the server's log) and confirm the
  first download from it. Back the file up and keep it private: a server with a new key is
  a new server to every player, who is asked again.

### The server checks what players have

The server is in charge of what its players load. When a player joins, and again on every
map change, it compares the player's files for the level with its own, and keeps the player
out of the match (no player on joining, no spawning after a map change) until they match:

1. The server tells the client the level and the id of its manifest, and proves who it is
   (it signs the client's random nonce with its key).
2. The client hashes the file its game would load for each of the level's **required files**
   and sends a digest of the list.
3. If the digest differs, the server asks for the per-file hashes, and answers with the
   files that differ. The client downloads them again (from the server or its download
   URL, checked against their hash) and reports again.
4. Once they match, the player is in. A player whose files still differ after 3 downloads,
   who declines to download (downloads set to *Never*), or who doesn't match within
   `content_sync_timeout` is kicked with the reason.

The required files are the files of the manifest the level needs, one per path (the one of
the highest priority layer, which is what the game loads). Only what the server actually
shares is checked:

| `content` | checked | not checked |
|---|---|---|
| `Off` | nothing: players join with their own content, as they have it | everything |
| `Mods` | the mods' files the level needs (every mod's common files, the level's own) | the imported BF2 assets: every player uses their own import |
| `All` | the mods' and the imported files the level needs | files of other levels |

This catches outdated, damaged and locally edited files. It can't stop a modified game,
which can report whatever it likes; the checks that matter for cheating stay on the server
(movement, hits, damage).

### For players

- When you join, the game first asks the server what it shares, compares that with what
  you have, and shows **Download N files, X MB?** with *Download*, *Always download* and
  *Cancel*. The loading screen then shows the download's progress; Cancel (or Esc) stops
  it, and a later join resumes where it stopped. Settings > Game > **Server content**:
  *Ask* (default), *Always* or *Never* (join with your own content only).
  `client --content always|ask|never` overrides it for one run; scenarios and screenshots
  download without asking.
- **New servers.** The first download from a server whose key you haven't trusted asks
  first, even with *Always*: **New server** with the server's name, address and key
  fingerprint, and *Trust and download*. Trusting remembers the key, so the next time only
  the usual question (or none, with *Always*) comes up. A server that shows up with another
  key asks again, and says so if you trusted its address with a different key before.
  Settings > Game lists the **trusted servers**, forgets one or all, and has **New servers**
  (on by default) to switch the question off.
- After you connect, the server checks your files for the level (see
  [The server checks what players have](#the-server-checks-what-players-have)); the loading
  screen says so. If it finds files that differ from its own, you get *Download them again?*
  (nothing to confirm with *Always* and a trusted server); on a map change the game then
  rejoins. With *Never* you are told why you can't play there.
- Files you already have count: a file in your own import or mods with the same hash isn't
  downloaded. If you have Battlefield 2 imported yourself, a server that shares everything
  only costs you its mods.
- While you play on the server, its content is used wherever it differs from yours, and
  your own mods are off for that session, so the server and every client simulate the same
  data. Leaving the server switches back.
- Downloads go to a cache shared by every server: `cache/` next to the settings file
  (`%APPDATA%\bf2-rust-engine\cache` on Windows; `--content-cache <dir>` or
  `$GAME_CONTENT_CACHE` put it elsewhere). It is limited to `content_cache_gb` (10 GB) in
  the settings file: the files used longest ago are deleted first. Deleting the folder is
  safe; files are downloaded again when needed.
- Without Battlefield 2 (no `imported/` folder) the game still starts. The menu offers the
  test range and Join: join a server that shares all its content to play its maps.
- The game only accepts files the server's manifest lists, with the data file types it
  reads, at checked relative paths, of the listed size and hash. Sounds must decode.
  Nothing downloaded is ever run.

## Accounts, stats and ranks (optional)

A master server (`crates/master_server`, binary `master`) can keep accounts, career stats and
ranks, list servers and find one to join. It is always optional: playing on a LAN, hosting,
and every server that isn't *ranked* work without it and without an account.

### For players

- **Account** in the main menu: enter the master server's address (`https://...`, stored as
  `master_url` in the settings), then log in or register (name, password, optional email).
  Logged in, the page shows your rank, the XP to the next one and your career stats. The
  same account logs in on the master's web pages (leaderboards, profiles).
  `master_url` must be `https://`; a plain `http://` address is only accepted to
  `localhost`/`127.0.0.1` (for testing a master on your own machine), since it would
  otherwise send your password and tokens in the clear.
- The game keeps a **refresh token**, never your password, in `account.ron` in the config
  folder (`%APPDATA%\bf2-rust-engine` on Windows; next to `--settings` if given). *Log out*
  revokes it on the master and deletes it. `account.ron` also remembers the master's key
  fingerprint from your last login there; if it ever answers with a different one, the game
  refuses to log in rather than trust a possibly different server, and says so on the
  Account page (log out and back in if the master's key really did change, e.g. after
  restoring `master.key` from a backup).
- Joining a server that takes accounts, the game asks the master for a **ticket** for that
  server (after the server proved its identity). A ticket works on that one server, once,
  for a few minutes. **Ranked** servers need one: log in to play there. On other servers it
  only shows your account name and rank (the scoreboard puts the rank before your name).
- **Quick join** (Join page) asks the master for servers with free slots (only unranked ones
  when you are logged out), pings them and joins the closest. Without a master server it
  joins the closest server with free slots the Join page found on this machine or the LAN.

### For server admins

| setting | config file | command line |
|---|---|---|
| master's web address | `master_url: "https://master.example.com"` | `--master-url <url>` |
| master's public key | `master_key: "<64 hex digits>"` | (default: fetched once from `master_url` and pinned in `master-keys.txt` in the data folder) |
| ranked | `ranked: true` | `--ranked` |
| API key (ranked) | `master_api_key: "bf2r_..."` | `--api-key <key>` |
| region (quick join) | `region: "eu"` | `--region <name>` |
| server list (UDP) | `master_server: "master.example.com"` | `--master <host[:port]>` |
| admin accounts | `admins: ["id:42", "alice"]` | `--admin <entry>` (repeatable) |

- **Unranked** (the default): leave it all out. With `master_url` set, the server checks the
  tickets players offer (offline, with the master's key) and shows their account name and
  rank; anyone can still join. As on the client, `master_url` must be `https://` unless it
  points at `localhost`/`127.0.0.1`; the server refuses to use it otherwise (its own API key,
  when ranked, and the master's key travel over that address).
- **Ranked**: the master's admin runs `master add-server --name "My server"`, which prints
  an API key once. Put it in the config with `ranked: true` and `master_url`. The server then
  requires accounts, sends the master a heartbeat every 30 s (so it is listed as ranked,
  with its region) and reports the stats of its verified players when a round ends: score,
  kills, deaths, captures, time, win or loss, seconds per kit and vehicle, kills per
  weapon. The master only counts players who got a ticket for this server lately, and each
  round once.
- `master_server` (UDP) announces any server to the master's server list, ranked or not;
  it is what the in-game browser reads.
- **Admin rights from accounts.** `admins` lists accounts that get [`Admin`] rights as soon
  as their ticket verifies on join: an entry `id:<account id>` matches by id, anything else
  matches the verified account name case-insensitively. They get a private chat line
  ("Signed in as admin (account ...)."). Removing an entry from the list takes effect on that
  account's next join; nothing revokes it from an admin already connected. On a server that
  requires accounts (`ranked: true`), chat `/login` is refused ("Admin access on this server
  comes from your account.") — put the admin's account in `admins` instead. `admin_password`
  and `/login` keep working exactly as before on LAN, offline and unranked servers. The
  remote console (`rcon_port`) is unaffected either way.

### Running a master server

Build it on the machine that runs it (it needs nothing but a Rust toolchain; SQLite is
built in):

```text
cargo build --release -p master_server        # target/release/master
master --config master.ron                    # see crates/master_server/src/config.rs
master add-server --name "My server"          # a ranked server's API key (shown once)
master list-servers
master remove-server 3                        # its API key stops working
master set-password alice                     # reads the new password from standard input
master promote alice                          # a master admin (see "Master admins" below)
master demote alice
master reset-2fa alice                        # an admin who lost the device and the recovery codes
master list-admins
```

The commands work on the data folder directly, also while the master runs (pass the same
`--config`); a running master sees the change on the next request.

It listens on UDP 16580 (server list) and HTTP 16581 (web pages and API), on 127.0.0.1 by
default. The data folder holds `master.sqlite` (accounts, stats) and `master.key` (the key
that signs tokens, made on the first start). **Back up both**; game servers pin the key, so
a new key means every ranked server has to forget the old one (delete its line in
`master-keys.txt`). `master.key` (and a game server's `identity.key`) are written readable
by your own account only: a Unix file mode, or on Windows a best-effort ACL restriction (the
`icacls` that ships with Windows; if that fails for some reason, the game warns and you
should keep the containing folder private yourself).

**Production needs TLS**: passwords and tokens must not cross the internet in the clear.
Run the master on 127.0.0.1 behind a reverse proxy that terminates HTTPS, set
`public_url: "https://master.example.com"` (cookies are then `Secure`) and `trust_proxy:
true` so rate limits see the players' addresses from `X-Forwarded-For`/`X-Real-IP` — but only
as reported by the proxy itself: the master only honours those headers from a request whose
*immediate* peer is trusted (loopback by default, which is what every setup below uses; add
`trusted_proxies: ["10.0.0.5"]` if the proxy runs on a different host or container, so a
client that somehow reaches 16581 directly can't spoof its address to dodge rate limits). The
proxy is also the place to set a request/response timeout (Caddy already applies sane
defaults; nginx: `client_body_timeout`, `send_timeout`) — the master bounds how long it
waits for a request body itself, but can't fully replace a real socket timeout in front of
it. On a Debian machine, for example:

```text
# /etc/systemd/system/bf2-master.service
[Unit]
Description=BF2 Rust master server
After=network-online.target

[Service]
User=bf2master
ExecStart=/opt/bf2-master/master --config /etc/bf2-master/master.ron
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/bf2-master

[Install]
WantedBy=multi-user.target
```

```text
# /etc/caddy/Caddyfile (Caddy gets and renews the certificate itself)
master.example.com {
    reverse_proxy 127.0.0.1:16581
}
```

With `udp_bind: "0.0.0.0:16580"` in `master.ron` for the server list, open UDP 16580 and
TCP 443 in the firewall; keep 16581 closed. `sudo useradd --system bf2master`, create
`/var/lib/bf2-master` owned by it, `systemctl enable --now bf2-master`. nginx works the same
way (`proxy_pass http://127.0.0.1:16581;` with `proxy_set_header X-Forwarded-For
$proxy_add_x_forwarded_for;`).

**Security**: passwords are hashed with Argon2id; refresh tokens, web sessions and API keys
are stored as hashes; session tokens last 15 minutes, tickets 5, refresh tokens 30 days
(rotated on use); failed logins are limited per address (429 after too many) — repeated
failures against one account *name* only slow it down, never block it outright, so an
attacker can't lock someone else's account out just by guessing wrong a few times; join
tickets are rate-limited per account and pruned after `ticket_window_hours`; registrations
are limited per address; names, passwords, emails and stats reports are validated; the web
pages escape everything, send a strict Content-Security-Policy, and check a CSRF token and
the `Origin` header on login/registration; requests are capped per address and in total
(`tiny_http` doesn't give the master a real connection limit or socket timeout, so this is
the practical substitute — see `master_server::http` and `game_auth::admission`); the UDP
server list caps entries per address, per /24 (or /64) network and in total, ignores the
unauthenticated `BYE` message (a spoofed one could otherwise delist any server; a shutdown
server just times out after 95 s instead), and throttles `LIST` replies per address so the
list can't be used to amplify a spoofed-source flood.

**Web pages**: `/` (overview), `/leaderboard`, `/players/<name>`, `/servers`, `/ranks`,
`/login`, `/register`, `/account` (change your password, log out everywhere), and for master
admins `/admin` (below). **API** for the game: see `game_auth::api` (`/api/v1/login`,
`/refresh`, `/me`, `/ticket`, `/quickjoin`, `/servers?limit=&offset=`, ...); `/servers` is
paginated (100 per page by default, 200 at most).

#### Master admins

Master admins run the master from its web pages: ranked servers, accounts, settings and an
audit log. Every admin needs **two-factor authentication** (an authenticator app); the game's
own login is unchanged, since admins play too, and a password alone never opens the admin
pages.

1. **The first admin comes from the command line** on the master's machine:
   `master --config master.ron promote <account name>` (the account must exist: register it
   on the web page or in the game first). Later admins can be promoted on the admin pages.
2. **Enrol**: log in on the master's web page. A newly promoted admin is sent to *Set up
   two-factor authentication*: scan the QR code with an authenticator app (Aegis, 2FAS,
   Google Authenticator, 1Password, ...; any time-based "TOTP" app), or type the key shown
   under it, and enter the app's current code. The page then shows **10 recovery codes,
   once**: keep them somewhere safe. Until this is done the account has no admin powers, so
   enrol right after promoting (whoever knows the password could otherwise enrol first).
3. **Log in**: from then on the web login asks for the app's code after the password. A code
   works once, within about a minute; wrong codes are limited per account (5 per 15 minutes,
   20 a day) and count against the address like wrong passwords. Admin sessions last 12
   hours.
4. **Lost the device?** Enter a recovery code instead of the app's code: it logs you in and
   turns two-factor authentication off, so you set it up again with the new device right
   away. Another admin can also reset yours (*Accounts*, your account, *Reset two-factor*),
   and `master reset-2fa <name>` does it from the command line if nobody else can. All of
   these are in the audit log.

The admin pages (`/admin`, with a link in the menu for admins):

- **Overview**: accounts (new in the last 24 h, admins, banned), ranked servers, servers and
  players online, ranked rounds in the last 24 h, the latest admin actions. Turn
  **registration** on or off here (this overrides `allow_registration` in `master.ron` until
  changed again); the rank table and XP rules come from `master.ron` and are shown here and
  on `/ranks`.
- **Ranked servers**: each server's address and time of its last heartbeat, its key
  fingerprint and whether it is online. **Add** one (its API key is shown once, with the line
  for the game server's config), **rotate** a key (the old one stops working at once),
  **disable** one (its key is refused and it leaves the server list until enabled again) or
  **remove** it (also deletes its round history).
- **Accounts**: search by name or id; a profile with stats, logins and the admin actions on
  that account. **Ban** with a reason and optionally a number of days (the player sees the
  reason; a banned account can't log in, refresh its game login or get join tickets, and its
  sessions end at once; bans also hide it from the leaderboard). **One-time password**: the
  page shows a password to hand over; it only opens the web page that sets a new one (the game
  refuses it and says so). **Log out everywhere** ends every web session and game login.
  **Rename**, for offensive names. **Make master admin** / **Remove master admin**; the last
  admin can't be removed, and admins can't be banned (remove the role first).
- **Audit log**: every admin action and admin login with who (and from which address),
  when, what and the target, newest first. It can't be edited or deleted from the pages, and
  the database itself refuses to change or delete its rows.

**How the secrets are kept**: the TOTP secret is encrypted in the database
(ChaCha20-Poly1305) with a key derived from `master.key`, so a copy of `master.sqlite` alone
(a backup, a leaked file) doesn't reveal it; `master.sqlite` is also made readable by the
master's own user only. Keep `master.key` safe (as before): restoring the database with a
different `master.key` makes every admin use a recovery code (or `master reset-2fa`).
Recovery codes are stored as hashes. Every admin form checks a CSRF token bound to the
session and the `Origin` header, and pages that show a secret are never cached.

The database has a schema version: a newer master upgrades an older `master.sqlite` in place
on its first start (the log says `schema upgraded from version 0 to 1`); back it up before
upgrading if you want to be able to go back, since an older master refuses a newer database.

**XP and ranks**: XP comes from ranked rounds: by default 1 per point of score, 0.5 per
minute played and 10 for a win, at most 5000 a round; BF2's ranks from Private (0) to
General (250 000). Change both in `master.ron` (`progression: (xp_per_score: ..., ranks:
[(name: "Private", short: "Pvt", xp: 0), ...])`); ranks follow from the XP, so a new table
applies to everyone at once.

## Testing a mod

- `client --level my_level` starts it directly. Use `--mods <dir>` for mods kept elsewhere.
- `server --level my_level --mods <dir>` serves it. Rotation entries in a server config can
  name mod levels.
- Scenarios (`scenarios/infra/mod_sample.ron`) take screenshots without anyone at the keyboard.
- The log says which mods are on, and names every file that fails to parse (patches
  included) with the reason.

## Importing a BF2 mod (AIX 2 and similar)

Don't confuse this with the mods above: a **BF2 mod** (`mods/AIX2` inside your Battlefield 2
install, next to `mods/bf2`) is EA/community content `bf2-import` converts from; a **mod**
above (`mods/aix2` in this project, next to `mods/sample`) is where the converted result goes.
Importing a BF2 mod produces one of our own mods, and the same rules apply: it never touches
`imported/`, and the game layers it on top by the usual priority.

A BF2 mod ships its own levels and, often, its own versions of shared objects, weapons, kits
and vehicles under the same BF2 names the base game uses (an AIX 2 `usrif_m4` is not the
vanilla `usrif_m4`). Importing it into `imported/` directly, or into a mod enabled alongside
vanilla levels, would let those silently replace the vanilla ones everywhere, not just on the
mod's own levels. Two things keep that from happening:

1. **Its own output root.** Point `--out` at the mod's own folder under this project's `mods/`
   (`--out mods/aix2`), never at `imported/`. `bf2-import` writes a complete tree there
   (`levels/`, `templates/`, `objects/`, `weapons/`, `kits/`, `vehicles/`, `soldiers/`) exactly
   like `imported/`'s own layout, plus a `mod.ron` (write one by hand, `priority` high enough
   to beat `imported/` — any positive number does, since the game already checks mods before
   `imported/`). `game_shared::mods::discover` and `GamePaths` then find it like any other mod;
   no runtime code needed for this part.
2. **`--namespace` and `--bf2-mod`.** `bf2-import --bf2 <install> --out mods/aix2 level --all
   --bf2-mod AIX2 --namespace aix2 --mod-title "AIX 2"` imports only that BF2 mod's levels
   (`--bf2-mod`, also scoping which mod's soldier bodies get written, so vanilla ones aren't
   duplicated into `mods/aix2/soldiers/`), and writes each one as `levels/aix2_<name>/` with
   `display_name` prefixed `AIX 2: ` (`--namespace`/`--mod-title`). This is what stops a BF2
   mod's level from colliding with a vanilla one of the same folder name (AIX 2 ships its own
   `Dalian_plant`, `Dragon_Valley`, `Gulf_of_Oman` and `Sharqi_Peninsula`) and lets the CLI
   `--level` and the menu name mod levels unambiguously (`game_data::LevelDesc::mod_title`
   carries "AIX 2" separately from the already-prefixed `display_name`, for a menu that wants
   to group by it — see `crates/game_client/src/menu/levels.rs`'s `LevelSummary`).

   **What `--namespace` does not yet do**: rename the mod's own shared-name templates,
   weapons, kits and vehicles (only levels). If the mod is enabled at the same time as vanilla
   levels are played, any of those it redefines under a vanilla name still wins for every
   level, not just its own, exactly as any two mods with the same file would. Check what a
   mod actually redefines (not just what it shares a name with — most shared names turn out
   byte-identical, since a BF2 mod usually reuses the base game's object library unchanged):

   ```sh
   for kind in templates weapons kits vehicles soldiers; do
     for f in $(comm -12 <(ls imported/$kind) <(ls mods/<name>/$kind)); do
       cmp -s "imported/$kind/$f" "mods/<name>/$kind/$f" || echo "$kind/$f differs"
     done
   done
   ```

   For AIX 2's real 24-level import this found almost nothing: of 622 shared templates, 45
   weapons, 21 kits, 64 vehicles and 8 soldiers, only two files actually differ —
   `weapons/nsrif_crossbow.ron` (AIX repurposes the crossbow's `slot` and projectile mesh,
   apparently for its zipline launcher) and a template with a float rounding difference in
   the 6th decimal place (not worth caring about). All 8 shared soldier bodies differ only by
   one *added* animation clip name (`xpak_zipline_hang`, for AIX's zipline), which is additive
   and harmless on vanilla levels. So for AIX 2 specifically, running it alongside vanilla
   levels in the same session is safe in practice, with one known exception: `nsrif_crossbow`
   behaves like AIX's version everywhere while the mod is enabled. Don't assume this holds for
   every BF2 mod without checking; a total conversion that redoes the base kits/weapons from
   scratch would show far more diffs, and then a `--mods` directory with only that mod enabled
   (or a `bf2-import` pass that renames + rewrites references, not yet written) is the safe
   option.
3. **Not every folder under `mods/` in a BF2 install is a mod to import.** Some ship
   non-game tools there (AIX 2's `mods/stats`, its bundled offline stats server) with a
   `mod.desc` but no `ClientArchives.con`/`ServerArchives.con` to mount; `Bf2Install::mods()`
   skips those, and one mod's archives failing to mount no longer aborts every other mod's
   soldier import.
