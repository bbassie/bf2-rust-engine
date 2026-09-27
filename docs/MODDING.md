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
| `game_modes` | layouts: `mode` (`gpm_cq`, `gpm_coop`), `size` (16/32/64), `control_points`, `spawn_points`, `vehicle_spawners`, extra `statics` |
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
- Clients get the files of the maps the server plays: the current one and the rotation. The
  server works out which files each level uses from the references in the data, so a
  client that joins a BF2 map downloads that map and what it uses (about 0.5 GB with `All`),
  not the whole import (about 4 GB). If an admin changes to a map outside the rotation,
  clients briefly rejoin to fetch its files.
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

### For players

- When you join, the game first asks the server what it shares, compares that with what
  you have, and shows **Download N files, X MB?** with *Download*, *Always download* and
  *Cancel*. The loading screen then shows the download's progress; Cancel (or Esc) stops
  it, and a later join resumes where it stopped. Settings > Game > **Server content**:
  *Ask* (default), *Always* or *Never* (join with your own content only).
  `client --content always|ask|never` overrides it for one run; scenarios and screenshots
  download without asking.
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

## Testing a mod

- `client --level my_level` starts it directly. Use `--mods <dir>` for mods kept elsewhere.
- `server --level my_level --mods <dir>` serves it. Rotation entries in a server config can
  name mod levels.
- Scenarios (`scenarios/mod_sample.ron`) take screenshots without anyone at the keyboard.
- The log says which mods are on, and names every file that fails to parse (patches
  included) with the reason.
