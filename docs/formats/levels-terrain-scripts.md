# Battlefield 2 levels, terrain and the .con script language: a spec for a static importer

Checked against a retail install: patch 1.5, `mods/bf2` (including the Euro Force and Armored Fury boosters) and `mods/xpack` (Special Forces). All 30 levels were loaded with the companion scripts `bf2con.py` and `bf2terrain.py`.

**Evidence tags used below:**
- **[V]** I verified it against the real files (the method is noted where it matters).
- **[C]** Community sources: marekzajac97/bf2-blender, art567/BfMeshView, the bf2tech wiki mirror, and OpenBattlefield2 (OBF2) RE notes. Where two independent code bases agree it is noted.
- **[I]** My own inference. Treat it as a hypothesis.

---

## 0. Key facts at a glance

| Topic | Fact |
|---|---|
| World axes | Left-handed (D3D). +X = east, +Y = up, +Z = north. Units are metres. The origin is the centre of the playable (primary) terrain. **[V]** The minimap is north-up and matches the colormap mosaic flipped vertically, so +Z is at the top of the map. **[C]** OBF2/RendDX9 uses only `*LH` D3DX functions. |
| Rotation `Object.rotation Y/P/R` | Degrees. `M = D3DXMatrixRotationYawPitchRoll(yaw, pitch, roll)` with row vectors (`v' = v·M`), i.e. `M = Rz(roll)·Rx(pitch)·Ry(yaw)`. Yaw is about +Y, and +yaw turns north→east (clockwise seen from above). +pitch tilts the nose down. +roll lifts the +X (right) side. **[C]** BfMeshView and bf2-blender agree. The matrix layout is **[V]**. |
| `Object.absoluteTransformation` | `[r0][r1][r2][r3]`, four rows of `a/b/c/d`. The first three rows are the local X/Y/Z axes in world space; row 3 is the translation. **[V]** row1 · terrain normal = 0.998 over 1399 tilted trees. |
| Heightmap | `HeightmapPrimary.raw` is N×N `u16` little-endian, N = 513 or 1025, with no header. `worldX = col·sx − (N−1)·sx/2`, `worldZ = row·sz − (N−1)·sz/2`, `Y = sample·sy`. Row 0 is the south edge (min Z). **[V]** Editor-snapped gameplay objects match the terrain to ≤1 mm. |
| Ground triangles | Each cell is split along the diagonal (col+1,row)–(col,row+1). **[V]** 52 of 67 discriminating samples match this exactly; 2 match the other diagonal and bilinear matches none. **[C]** OBF2 cites `Heightmap::getHeightAndNormalInLocalCoords`. |
| Surrounding terrain | Up to 8 × 257² `u8` maps. Cluster cell (cx,cy) is centred at (cx·S, cy·S), S = `setHeightmapSize`, and **cy grows toward +Z**, so `U1` = south, `D1` = north, `L1` = west, `R1` = east. **[V]** Edges are continuous (median seam error 0.00 m). |
| Terrain textures | `txXXxZZ.dds`: XX = patch column (+X), ZZ = patch row (+Z). Image row 0 is at min Z, the same as the heightmap. **[V]** Correlation of lightmap/detail/lowdetail channels with slope and hillshade was 0.7. |
| Detail-weight channels | Detail texture i (0..5, listed in `terraindata.raw`) is weighted by `Detailmaps/tx..._{1+i/3}.dds`, channel **B,G,R** for i%3 = 0,1,2. **[V]** Rock (i=0) → `_1`.B correlates with slope at 0.7. Tarmac (i=3) → `_2`.B correlates with undergrowth "tarmac" at 0.94. |
| Level archives | `server.zip` and `client.zip` are mounted at `Levels/<Name>/` **and** overlaid on the VFS root. Booster levels ship `objects/...` and `common/...` inside them, and those files are referenced by root-relative paths. **[V]** |
| Mount priority | The earliest `fileManager.mountArchive` wins. xpack mounts its own archives first, then `mods/bf2/...`. **[V/I]** This is consistent with the xpack overrides; there is no counter-example. |
| Template lookup | In the game branch, `StaticObjects.con` has no `run` lines. Templates are resolved by file name: `Object.create foo` loads `objects/**/foo.con`, or `foo_compiled.con` for roads. **[V]** 257 of 258 Karkand templates are found this way; the remaining one is the editor-only DefaultEnvMap. |
| Level load order | `Init.con` (game branch, empty args), then `StaticObjects.con`, then `Triggerables.con` if present, then `GameModes/<mode>/<size>/GamePlayObjects.con` with `v_arg1 = host`. **[V]** Exe strings plus the file contents. **[C]** OBF2 says 8 empty args. |

---

## 1. Virtual file system

### 1.1 Mod archive mounts [V]

`mods/<mod>/ServerArchives.con` and `ClientArchives.con` contain only `fileManager.mountArchive <zip> <mountpoint>` lines. A zip path is relative to the mod dir, unless it starts with `mods/`, in which case it is relative to the game root.

```
mods/bf2/ClientArchives.con                        mods/bf2/ServerArchives.con
fileManager.mountArchive Objects_client.zip Objects fileManager.mountArchive Objects_server.zip Objects
fileManager.mountArchive Common_client.zip Common   fileManager.mountArchive Menu_server.zip Menu
fileManager.mountArchive Menu_client.zip Menu       fileManager.mountArchive Common_server.zip Common
fileManager.mountArchive Fonts_client.zip Fonts     fileManager.mountArchive Booster_server.zip Objects
fileManager.mountArchive Shaders_client.zip Shaders
fileManager.mountArchive Booster_client.zip Objects

mods/xpack/ClientArchives.con                      mods/xpack/ServerArchives.con
... Objects_client.zip Objects                     ... Objects_server.zip Objects
... Common_client.zip Common                       ... Menu_server.zip Menu
... Menu_client.zip Menu                           ... Common_server.zip Common
... Shaders_client.zip Shaders                     ... mods/bf2/Objects_server.zip Objects
... Fonts_client.zip Fonts                         ... mods/bf2/Menu_server.zip Menu
... mods/bf2/Objects_client.zip Objects            ... mods/bf2/Common_server.zip Common
... mods/bf2/Common_client.zip Common
... mods/bf2/Menu_client.zip Menu
... mods/bf2/Fonts_client.zip Fonts
```

- A mount maps zip member `a/b.c` to virtual `<mountpoint>/a/b.c`. For example, `Objects_server.zip:staticobjects/.../gas_station.con` becomes `objects/staticobjects/.../gas_station.con`.
- **Priority: the first mount of a path wins [V/I].**
  - xpack lists its own archives before bf2's. 43 files in `xpack/Objects_server.zip` differ from the same path in `bf2/Objects_server.zip` (e.g. `common/flags/flagpole/flagpole.con`, road splines), so the xpack copies must win.
  - In bf2, `Objects_*` and `Booster_*` share no paths at all.
- xpack does **not** mount bf2's `Shaders_client.zip` or the `Booster_*.zip` archives. It has its own `Shaders_client.zip`, and 80 of its 109 shared shader files differ.
- `Mod.desc` of xpack has `<parentmod> bf2 </parentmod>`. That is the only inheritance mechanism besides the explicit `mods/bf2/...` mounts. Loose files fall back to `mods/<mod>/`, then the parent mod, then the game root. For example, `run /mods/bf2/ai/AIDefaultStrategies.ai` in `Mashtuur_City/ai/AI.ai` resolves against the game root.
- Server and client archives for Objects: every `.con/.tweak/.inc/.ai/.collisionmesh/.baf/.ske/.desc/.tai` exists in **both** and is byte-identical. Meshes (`.staticmesh/.bundledmesh/.skinnedmesh`), textures, sounds, `.dat` and `.occ` are client-only. **[V]** 2109 `.con` files, all identical. A static importer can mount server + client together.
- Some `.con` files are loose on disk: `mods/<mod>/AI/*.ai`, `GameLogicInit.con`, `Init.con`, `Settings/*`, `Objects/Weapons/**/info/weaponIcon.png`.

### 1.2 Booster packs: where they live [V]

**Levels:** booster levels are ordinary folders in `mods/bf2/Levels/`:
- Euro Force: `GreatWall`, `OperationSmokeScreen`, `Taraba_Quarry`.
- Armored Fury: `Midnight_Sun`, `OperationHarvest`, `OperationRoadRage`.

**`Booster_server.zip` / `Booster_client.zip`:**
- They are mounted at `Objects` like `Objects_*.zip`.
- They contain only shared object content: `StaticObjects/America/*`, `vegitation/america/*`, `Vehicles/xpak2_vehicles/*`, effects and road splines.
- The Armored Fury levels use them (58, 80 and 91 static templates only found there).
- The exe hard-codes the strings `Booster_client.zip`, `Booster_server.zip`, `OperationRoadRage`, `OperationHarvest`, `Midnight_Sun` next to each other. **[I]** This is probably an ownership check or a mount tied to those three levels. For import, just mount them.

**Euro Force levels** carry their own objects inside the level zips:
- 38 (GreatWall), 42 (SmokeScreen) and 12 (Taraba) `objects/**.con` files, plus meshes and textures under `objects/` and `common/` in `client.zip`.
- Their `Init.con` explicitly `run`s those templates in the game branch, e.g. `run Objects\Vehicles\xpak2_vehicles\xpak2_tnkl2a6\xpak2_tnkl2a6.con`.
- Highway_Tampa (6 files) and Operation_Blue_Pearl (1) do the same.

### 1.3 Level archives [V]

The .con files reference level data by absolute virtual paths such as `Levels/Strike_at_Karkand/HeightmapPrimary.raw`, and the zip root holds `HeightmapPrimary.raw`. Therefore **`mods/<mod>/Levels/<Name>/server.zip` and `client.zip` are mounted at `Levels/<Name>/`.** The exe contains the strings `Levels/`, `/Levels/`, `/server.zip`, `GameModes/%s/%d/`.

**Root overlay (required).** Level `client.zip` files contain root-relative trees that exist nowhere else. Examples:
- `GreatWall/client.zip:common/textures/sky/Great_Wall_sky.dds` is referenced by `Skydome.skyTexture common\textures\sky\Great_Wall_sky` and is not in `Common_client.zip`.
- The same zip holds `objects/staticobjects/_asia/village/textures/greatwall_de.dds`, which level-local `.staticmesh` files reference as `objects/staticobjects/_asia/village/textures/...`.
- Terrain detail textures such as `common/terrain/textures/detail/p14_dirt.dds` (Jalalabad) also live only in the level zip.

So a level's `objects/**` and `common/**` must also resolve at the root. Overlap with global archives is rare:
- 4, 6 and 37 identical files in GreatWall, Taraba and SmokeScreen.
- 4 files differ: `Highway_Tampa: objects/staticobjects/_middle-east/city/textures/mecity_01_di.dds`, and 3 `xp2_usa_01_*` textures in SmokeScreen.

Give the level overlay **highest priority** **[I]**; those differences look like intentional per-level variants.

**Level-local archive lists.**
- `Highway_Tampa` and `Wake_Island_2007` contain `ServerArchives.con`/`ClientArchives.con` with `fileManager.mountArchive mods/bf2/Objects_drm01_client.zip Objects`. That archive does not exist in 1.5; its content was merged into the main archives. A missing archive is skipped.
- Custom mods use the same mechanism **[C]**, so implement it.

**Loose level files.** `mods/<mod>/Levels/<Name>/Info/` holds `<Name>.desc`, `loadmap.png`, `favoriteMap.png` and `gpm_*_<n>_menuMap.png` on disk. `archive.md5` is a per-level MD5 list used for the anti-cheat check.

### 1.4 Paths and names [V]

- Case-insensitive, both in zip members and in scripts. Examples: `Levels\road_to_jalalabad\...` in its terraindata, `Objects/staticobjects/...` vs `objects/StaticObjects/...`. Treat `\` and `/` the same.
- Normalise by lower-casing, converting `\` to `/`, stripping a leading `/`, and resolving `.` and `..`.
- Script paths:
  - A leading `/` or `\` is relative to the VFS root (e.g. `run /objects/staticobjects/...`, `run /Common/Sky/SkyDome/skydome.con`).
  - Anything else is relative to the directory of the running file (e.g. `run Heightdata.con`, `include gas_station.tweak`, `run Objects\Vehicles\...` from `Levels/GreatWall/Init.con`).
  - 3 retail `run` lines omit `.con`, so try appending it.
- Texture references usually have no extension, e.g. `common\textures\sky\karkand_cloudy` → `.dds`.
  - `texturemanager.customTextureSuffix "Woodland"` (set in 15 level Init.cons) makes the engine try `<name>_woodland.dds` first. 26 textures exist, all soldier and kit camo.
- The level name in paths can differ in case from the folder (`Levels/Dalian_plant/HeightmapPrimary.raw` in `Dalian_Plant`).

---

## 2. The .con / .tweak / .inc / .ai script language

.con, .tweak, .inc and .ai files all share one syntax. The corpus is 11,529 script files in all archives and loose folders, of which 6,746 are unique by content:
- 3,741 `.con`
- 2,578 `.tweak`
- 221 `.inc`
- 206 `.ai`

Every file is CRLF, has no BOM, and is Latin-1. Tabs are common as whitespace.

### 2.1 Lexical grammar [V]

```ebnf
file        = { line } ;
line        = ws* [ statement ] ws* EOL ;
statement   = comment | blockstart | blockend | control | decl | call ;
comment     = "rem" ( ws any* )? ;                (* first token, case-insensitive *)
blockstart  = "beginRem" any* ;                   (* block comment until matching endRem, nestable *)
blockend    = "endRem" any* ;
control     = "if" cond | "elseIf" cond | "else" | "endIf" | "while" cond | "endWhile" | "return" ;
cond        = operand op operand ;                (* op: == != < > <= >=  | equals notEquals greaterThan
                                                     greaterOrEqualThan lessThan lessOrEqualThan *)
decl        = ("var" | "const") name [ "=" value ] ;
call        = command { ws arg } [ ws "->" ws varname ] ;
command     = ident { "." ident } ;               (* Prefix.method or Prefix.sub.method, e.g.
                                                     ObjectTemplate.activeSafe, Object.geometry.loadMesh,
                                                     aiPathfinding.map.maxSlope *)
arg         = quoted | bare ;
quoted      = '"' { any-but-quote } '"' ;         (* no escapes; the quotes are not part of the value *)
bare        = { any-but-ws }+ ;                   (* numbers, a/b/c vectors, [..][..] matrices, names, paths *)
varname     = ("v_" | "c_") ident ;
```

- Only `rem` as the **first token** starts a comment. There are no inline comments. The retail data has no `rem`-prefixed glued tokens, except one typo `2rem` in Midnight_Sun `Water.con`, which the engine treats as an unknown command.
- `beginRem`/`endRem` appear 98 times, only as whole-line markers.
- `run` and `include` are statements with a path argument followed by optional arguments.
- Other bare keywords: `alias` in `Settings/AliasedCommands.con`, `echo` in `AI/AIDefault.ai`, and `PlayerName=` in settings files. A level importer can ignore all of them.

### 2.2 Control flow and variables [V + C]

**Constructs actually used in retail data** (unique files):
- `if` 608, `else` 150, `endIf` 608.
- `run` 8,096, `include` 2,763.
- `var` 9, always `var v_dist = 20` in `Kits/*_Common.con`.
- `beginRem`/`endRem`.

**Constructs never used in retail data:** `while`/`endWhile`, `elseIf`, `const`, `return`, `->`. Support them anyway for mod data; the grammar is from the bf2tech Console Script Grammar **[C]**.

**Every retail `if` has one of two forms:**
- `if v_arg1 == BF2Editor` (404 occurrences counting duplicate archive copies): editor branch vs game branch.
- `if v_arg1 == host` (340): creation of networked gameplay objects, in every `GamePlayObjects.con`.

**Scopes:**
- `run file a b c` executes the file in a **new scope** with `v_arg1..v_arg8` = the args; missing args are `""`. The exe has exactly `v_arg1`..`v_arg8`, plus `v_result` and `v_no`.
- `include file` runs in the **caller's scope**. **[C]** The wiki says "run seems to create a new scope while include does not". No retail include passes args.

**Variable substitution:**
- A bare token that exactly names a defined `v_`/`c_` variable is replaced by its value.
- Engine-defined enum constants such as `c_ETPlane`, `c_ETHelicopter`, `c_ETTank`, `c_ETNewCar2` and `c_NIGhostAlways` (70 uses in `ObjectTemplate.setEngineType`) are not defined in script. Keep them as literals and map them in the importer.

**Conditions:** compare numerically if both sides parse as numbers, otherwise as strings. Use a case-insensitive string compare **[I]**.

**Other statements:**
- `console.allowMultipleFileLoad 0` … `1` brackets the editor-only template `run` lists. While it is 0, a file already loaded is not executed again.
- Missing `run`/`include` targets are silently ignored. Karkand has 1 (`t_intersection.tweak`); the bf2 Objects templates have 155 dangling `include *.tweak`.

### 2.3 Object model commands a static importer needs [V]

**Template definition** (object `.con` + `.tweak`):

```
GeometryTemplate.create StaticMesh gas_station      -> mesh file = <dir of .con>/meshes/gas_station.staticmesh
CollisionManager.createTemplate gas_station         -> <dir>/meshes/gas_station.collisionmesh
ObjectTemplate.create SimpleObject gas_station      -> new template, becomes "active"
ObjectTemplate.geometry gas_station                 -> link geometry by name
ObjectTemplate.collisionMesh gas_station
ObjectTemplate.anchor 0/-5.9957/0
include gas_station.tweak                           -> tweak re-opens it:
ObjectTemplate.activeSafe SimpleObject gas_station  -> (re)activate existing template (create if absent)
ObjectTemplate.addTemplate <child>                  -> child slot; the next setPosition/setRotation apply to it
ObjectTemplate.setPosition x/y/z                    -> child offset in parent space
ObjectTemplate.setRotation y/p/r                    -> child rotation (degrees, same convention as Object.rotation)
```

**Geometry file rule [V].** A `GeometryTemplate.create <Type> <name>` in `X/Y/foo.con` corresponds to `X/Y/meshes/<name>.<type>`, with type one of `staticmesh`, `bundledmesh` or `skinnedmesh`:
- 1,310 of 1,318 geometries resolve (867 staticmesh, 416 bundledmesh, 27 skinnedmesh); the 8 misses are unused leftovers.
- 1,044 of 1,045 `CollisionManager.createTemplate` calls resolve.
- `RoadCompiled` geometries have no file. Each road instance loads its own mesh (§4.4).

**Instances:**

```
Object.create <template>                      -> new instance (lazy-loads <template>.con if unknown)
Object.absolutePosition x/y/z                 -> world position (m)
Object.rotation yaw/pitch/roll                -> degrees (optional; missing = 0/0/0)
Object.absoluteTransformation [..][..][..][..]  -> full matrix (overgrowth collision trees)
Object.layer n                                -> editor layer id; IGNORE for statics (see below)
Object.isOvergrowth 1 | Object.disableChildren 1 | Object.notInAI 1 | Object.setControlPointId n
Object.setLightSourceMask n | Object.name s | Object.absolutePositionSecondary / rotationSecondary (elevators)
Object.geometry.loadMesh <path>               -> roads (CompiledRoads.con)
```

`Object.layer` is only an editor layer:
- Leviathan puts 1,136 of its 1,142 statics in layers 5–12, while its gameplay files use layers 2–4.
- Gameplay per mode and size lives in separate files, not in layers. So create every static object regardless of layer **[V/I]**.

**Lazy template loading [V + I].** The game branch of every `StaticObjects.con` only has `Object.create` lines; the template `run` list sits in `if v_arg1 == BF2Editor`. The evidence that the engine resolves templates by file name:
- For Karkand, all 257 game-branch templates exist as `objects/**/<name>.con`.
- Templates missing from the global archives (GreatWall's `xp2_*`) are explicitly `run` in that level's `Init.con`.
- The exe has "Loading Templates..", `GSUseObjectCache`, and "Networkable objects can't be created at this stage, possible staticObjects.con holds vehicles".
- Exception: compiled road templates are `objects/roads/splines/<name>_compiled.con`, because `<name>.con` there is the editor `RoadTemplate`.
- Across all 30 levels only 7 template names stay unresolved, and all are dangling in the retail data: `billboard_cream`, `s_ambientmusic_7`, `xp1_mi_antenna_lights`, `xp1_light_cone_04`, `xp1_train_cargowag_0{1,2}_closed_collision`, `nf_five`, `w_light`.
- Recommended: index `objects/**/*.con` by basename, try `<name>`, then `<name>_compiled`.

**Value formats [V]:**
- Numbers: `-134.000`, `1`, `0.00457764`.
- Vectors: `a/b` or `a/b/c` or `a/b/c/d`.
- Matrices: `[m00/m01/m02/m03][m10/..][..][tx/ty/tz/1]`.
- Booleans: `0/1`.
- Enums: either a name or a number (`physicsType Mesh` or `physicsType 3`).
- Colours are 0–1 floats (`Lightmanager.*`, `renderer.waterColor`, `terrain.terrainWaterColor`), **except** `Renderer.fogColor` and `renderer.waterFogColor`, which are 0–255 (`163.00/135.00/86.00`).
  - Beware: `terrainWaterColor` is 0–255 by mistake in Mass_Destruction (`38/35/5`) and Surge. Normalise values above 1.

### 2.4 Level load sequence (what to interpret) [V + C]

1. Mount the mod archives, then the level (§1.3) and any level `*Archives.con`.
2. Run `Levels/<L>/Init.con` with **no arguments**. `v_arg1` ≠ `BF2Editor`, so the game branch runs. **[C]** OBF2: `GameLogic::loadLevel` passes 8 empty args; the editor passes `BF2Editor`.
   The game branch of every Init.con, in order:
   - `run Heightdata.con`
   - `run Terrain.con v_arg2` → `terrain.create Terrain`, `terrain.load Levels/<L>/terraindata.raw`
   - `run Sky.con v_arg2`
   - `run CompiledRoads.con`
   - `run Sounds.con`
   - (xpack only) `run staticglows.con / staticlights.con / staticsprites.con`
   - `run tmp.con v_arg1` (0-byte file)
   - `Undergrowth.load Levels\<L>\`
   - `run Overgrowth/Overgrowth.con`, `run Overgrowth/OvergrowthCollision.con`
   - `run AmbientObjects.con`
   - `run Water.con`
   - (some levels) `run TriggerableTemplates.con`
   - (Euro Force) explicit `run objects/...` template lists

   After the if/else block, Init.con sets `gameLogic.*`: team names/languages/flags, kits (`setKit team slot kitTemplate soldierTemplate`), tickets per size, `setBeforeSpawnCamera`, `MaximumLevelViewDistance`, drop vehicles, `customTextureSuffix`.
3. `Levels/<L>/StaticObjects.con` with no args. The engine runs it; Init.con does not.
4. `Levels/<L>/Triggerables.con` if present (6 levels; exe string).
5. `Levels/<L>/GameModes/<gpm>/<size>/GamePlayObjects.con` with `v_arg1 = host`. The `Object.create` placement blocks sit inside `if v_arg1 == host`. **[C]** OBF2 runs it the same way.
6. AI (not needed for rendering): `AI/AI.ai` and `GameModes/<gpm>/<size>/AI/StrategicAreas.ai`.
7. Night maps (xpack) also have `normal.con`, `nightvision.con`, `helicoptervision.con` and `commandervision.con`: post-process presets that the engine runs.

### 2.5 Command survey (all archives, unique file contents) [V]

The survey found 69 prefixes and 1,760 distinct `Prefix.method` commands. Keyword counts: `run` 8,096, `include` 2,763, `if`/`endIf` 608, `else` 150, `alias` 79, `var` 9, `echo` 3. `rem` lines are not counted.

| prefix | statements | distinct methods | most used methods |
|---|---:|---:|---|
| `objecttemplate` | 377243 | 963 | modifiedbyuser (24130), activesafe (21363), create (20196), hasmobilephysics (12693) |
| `object` | 253110 | 15 | create (65614), absoluteposition (46863), layer (45670), rotation (36370) |
| `materialmanager` | 12819 | 5 | createcell (5054), damagemod (3141), setsoundtemplate (2315), seteffecttemplate (1973) |
| `animationsystem` | 11930 | 11 | createtrigger (4442), createanimation (3704), createbundle (3256), cameraspring.use (71) |
| `geometrytemplate` | 11072 | 22 | create (2144), setsubgeometryloddistance (1764), compressvertexdata (1448), maxtexturerepeat (1447) |
| `combatarea` | 10993 | 8 | addareapoint (9876), create (181), min (181), max (181) |
| `animationbundle` | 10592 | 13 | addanimation (4061), fadeouttime (2002), fadeintime (1998), islooping (1814) |
| `animationtrigger` | 10247 | 8 | addchild (4258), addbundle (3347), valueholder (1243), message (996) |
| `hudbuilder` | 8138 | 104 | setnodecolor (914), createpicturenode (747), setpicturenodetexture (599), setnodeshowvariable (515) |
| `aitemplateplugin` | 4255 | 57 | create (421), setstrategicstrength (236), equipmenttypename (118), driveturncontrol (114) |
| `sound` | 3320 | 14 | tweaktemplate (1920), addtrigger (1243), addsound (72), setproperty (34) |
| `animationmanager` | 2941 | 6 | looping (1532), ignoremotherorientation (761), length (576), noicefreq (33) |
| `swiffhost` | 2886 | 7 | addcredit (2612), addaward (240), addrank (22), addgamemode (6) |
| `gamelogic` | 2499 | 24 | messages.addradiovoice (492), setkit (420), addinvaliddropvehicleobject (390), setdefaultnumberofticketsex (240) |
| `aistrategicarea` | 1884 | 10 | addwaypoint (418), addneighbour (318), setorderposition (288), layer (144) |
| `collisionmanager` | 1616 | 1 | createtemplate (1616) |
| `aitemplate` | 1571 | 8 | addtype (441), addplugin (425), create (127), degeneration (127) |
| `heightmap` | 1410 | 7 | setsize (270), setscale (270), setbitresolution (270), setmodified (270) |
| `weapontemplate` | 1386 | 17 | setstrength (684), create (114), indirect (114), minrange (114) |
| `overgrowthtype` | 1212 | 6 | geometry (202), density (202), normalscale (202), rotationscale (202) |
| `roadtemplate` | 1062 | 8 | setisprimarytexture (236), createinstance (118), setname (118), setwidth (118) |
| `material` | 1019 | 15 | active (143), name (143), type (143), friction (143) |
| `aisettings` | 978 | 29 | activatedefaultinterpreter (320), addbotname (244), addinterpreterentry (132), setvehiclebehaviour (80) |
| `controlmap` | 773 | 9 | addkeytotriggermapping (585), addkeystoaxismapping (58), addbuttontotriggermapping (51), addaxistoaxismapping (42) |
| `overgrowth` | 757 | 10 | addtype (202), setactivetype (202), addmaterial (89), setactivematerial (89) |
| `terrain` | 715 | 26 | create (60), watersunintensity (56), gicolor (35), suncolor (33) |
| `roadtemplatetexture` | 708 | 3 | settexturefile (236), setmirror (236), setscale (236) |
| `lightmanager` | 656 | 31 | sunspeccolor (41), skycolor (35), suncolor (35), staticsuncolor (35) |
| `renderer` | 466 | 35 | fogstartendandbase (35), watercolor (31), waterspecularcolor (31), waterspecularpower (31) |
| `skydome` | 420 | 16 | skytemplate (30), cloudtemplate (30), hascloudlayer (30), hascloudlayer2 (30) |
| `heightmapcluster` | 390 | 5 | addheightmap (270), create (30), setclustersize (30), setheightmapsize (30) |
| `combatareamanager` | 256 | 3 | use (121), timeallowedoutside (121), damage (14) |
| `menuteammanager` | 199 | 4 | addkit (98), addweapon (73), addteam (14), setteamid (14) |
| `game` | 136 | 43 | setgamedisplaymode (5), setdetailtexture (5), setshadows (5), setenvironmentmapping (5) |
| `aistrategy` | 112 | 10 | setstrategicobjectsmodifier (16), addcondition (13), createconstantcondition (12), setconditionstrength (12) |
| `aipathfinding` | 105 | 10 | map.addvehicleforclustercost (41), map.maxslope (27), setactivemap (26), createmap (2) |
| `ragdoll` | 102 | 10 | toskeleton (30), addconstraint (18), addangularconstraint (16), addparticle (13) |
| `console` | 89 | 2 | allowmultiplefileload (88), showstats (1) |
| `sv` | 66 | 66 | servername (1), password (1), internet (1), serverip (1) |
| `kittemplate` | 63 | 3 | setbattlestrength (42), setstrategicstrength (14), create (7) |
| `levelsettings` | 60 | 2 | initworld (30), customtexturesuffix (30) |
| `undergrowth` | 60 | 1 | load (60) |
| `lightsettings` | 60 | 2 | terrainsuncolor (30), terrainskycolor (30) |
| `vars` | 44 | 1 | set (44) |
| `animationvalueholder` | 35 | 3 | values (31), stoponmessage (3), passonmessage (1) |
| `ai` | 34 | 8 | init (15), addsaistrategy (12), addbot (2), saimapxdimension (1) |
| `nametags` | 33 | 3 | settexture (18), createicon (12), createbar (3) |
| `undergrowtheditable` | 30 | 1 | create (30) |
| `texturemanager` | 30 | 1 | customtexturesuffix (30) |
| `hemimapmanager` | 30 | 1 | setbasehemimap (30) |
| `networkableinfo` | 29 | 5 | createnewinfo (11), setpredictionmode (11), setbasepriority (5), setforcenetworkableid (1) |
| `filemanager` | 27 | 1 | mountarchive (27) |
| `lightmapsettings` | 26 | 1 | watersunintensity (26) |
| `scoremanager` | 22 | 22 | setdeath (1), setkill (1), setassistkill (1), setindirectkill (1) |
| `hudmanager` | 18 | 17 | addtextureatlas (2), setmousetextureempty (1), ... |
| `weathermanager` | 18 | 18 | stormenabled (1), lightningmaxinterval (1), ... |
| `aicomm` | 16 | 1 | addairadiomessage (16) |
| `radiointerface`, `radiovehicleinterface`, `squadleaderinterface`, `commanderinterface`, `squadinterface` | 9–11 each | 1 | setstringmap |
| `objectdrawer` | 11 | 1 | collectplanesdistance |
| `bf2engine` | 10 | 1 | playmovie |
| `settingsmanager` | 9 | 4 | boolset, u32set, floatset, stringset |
| `chat` | 8 | 8 | … |
| `aibotmanager` | 7 | 7 | … |
| `maplist` | 2 | 1 | append |
| `commandermenu` | 1 | 1 | vehicledropreloadtime |

Level scripts alone use 333 distinct commands. The most frequent are:
- `Object.create` 66,727, `absolutePosition` 47,976, `layer` 46,783, `rotation` 37,412, `isOvergrowth` 27,147, `absoluteTransformation` 18,751.
- `ObjectTemplate.create`/`activeSafe` 13,573 each, `CombatArea.addAreaPoint` 11,015, `ObjectTemplate.setObjectTemplate` 9,303.
- `Object.setLightSourceMask` 6,577, `Object.setControlPointId` 5,349, `ObjectTemplate.setControlPointId` 5,318, `ObjectTemplate.setSpawnPositionOffset` 4,744.

---

## 3. Terrain

### 3.1 `Heightdata.con` and the primary heightmap

```
heightmapcluster.create HeighmapCluster          (sic)
heightmapcluster.setClusterSize 3                -> 3x3 cluster
heightmapcluster.setHeightmapSize 1024           -> world size S of ONE cluster cell (= primary extent)
heightmapcluster.addHeightmap Heightmap 0 0      -> literal name "Heightmap", cell (cx, cy)
heightmap.setSize 513 513
heightmap.setScale 2/0.00457764/2                -> metres per sample in X / metres per height unit / Z
heightmap.setBitResolution 16
heightmap.loadHeightData Levels/<L>/HeightmapPrimary.raw
heightmap.setMaterialScale 1
heightmap.loadMaterialData Levels/<L>/HeightmapPrimary.mat
... 8 x addHeightmap for the secondaries ...
heightmapcluster.setSeaWaterLevel 146            -> absolute world Y of the water plane
```

**`HeightmapPrimary.raw` byte layout [V]:**
- N·N little-endian `uint16`, no header. N = `setSize` = 513 (1024 m levels) or 1025 (2048 m levels). File size is 2·N², e.g. 526,338 for Karkand.
- `index = row*N + col`.
- Mapping, with `h = (N−1)·sx/2`:
  - `x = col·sx − h`
  - `z = row·sz − h`
  - `y = raw·sy`
- There is no height offset. The terrain spans exactly [−S/2, +S/2] in X and Z.
- Row 0 is the south edge (min Z). An image viewer shows row 0 at the top, so flip vertically to get north-up.

**Verification [V]:**
- 8,235 of the `GamePlayObjects` positions (CPs, spawn points, spawners) across 29 levels lie within 0.3 m of the terrain under this mapping, and their median |y − terrain| is 0.4 mm.
- Per level, the median CP/spawner/spawn-point residual is ≤ 0.01 m (except Iron_Gator, which is on a carrier deck). The flipped or transposed candidates are off by 18–150 m (tested on overgrowth trees).
- The height scale is always `k·2^-16`-ish so that `65535·sy` is a round maximum height (300, 420, 163.84, … m).
- Overgrowth trees sit 0.7–9 m **above** the terrain (per-level constant). That is the tree mesh pivot, not a mapping error.

**Interpolation [V]:** triangles split along (col+1,row)–(col,row+1):
- If `u+v ≤ 1`: `h = h00 + (h01−h00)·u + (h10−h00)·v`.
- Otherwise: `h = h11 + (h10−h11)·(1−u) + (h01−h11)·(1−v)`.
- Here u is the fractional column and v the fractional row.

### 3.2 Secondary (surrounding) heightmaps [V]

- Each is 257×257, `setBitResolution 8` (one byte per sample), with scale `(S/256)/(256·sy)/(S/256)`. For example Karkand uses `4/1.17188/4`, and 1.17188 = 256 × 0.00457764, so both maps cover the same height range.
- Cell (cx,cy) spans X ∈ [cx·S − S/2, cx·S + S/2] and Z ∈ [cy·S − S/2, cy·S + S/2], with row/col oriented as in the primary.
- File names:

| addHeightmap | file | direction |
|---|---|---|
| `-1 -1` | `HeightmapSecondary_L1U1.raw` | SW |
| `0 -1` | `HeightmapSecondary_U1.raw` | S |
| `1 -1` | `HeightmapSecondary_R1U1.raw` | SE |
| `-1 0` | `HeightmapSecondary_L1.raw` | W |
| `1 0` | `HeightmapSecondary_R1.raw` | E |
| `-1 1` | `HeightmapSecondary_L1D1.raw` | NW |
| `0 1` | `HeightmapSecondary_D1.raw` | N |
| `1 1` | `HeightmapSecondary_R1D1.raw` | NE |

"U" means up in the raw image (toward row 0), which is **south**. Placing cy along −Z gives 40–70 m seams; along +Z the seams are 0.00 m.

### 3.3 Per-level terrain parameters [V] (from `bf2terrain.py`/Heightdata.con)

| Level | primary N | world S (m) | sy (m/unit) → max | sea level | min..max terrain (m) |
|---|---|---|---|---|---|
| Strike_at_Karkand | 513 | 1024 | 0.00457764 → 300 | 146 | 83.8..254.9 |
| Operation_Blue_Pearl | 513 | 1024 | 0.0025 → 163.8 | 22.1 | 16.9..31.0 |
| Ghost_Town / Iron_Gator / Leviathan / Mass_Destruction / Surge / Warlord (xpack) | 513 | 1024 | 0.00549 / 0.0025 / 0.00244 / 0.00458 / 0.000916 / 0.00305 | 34 / 50 / 38.5 / 115 / 5 / 80 | |
| all other bf2 levels, Devils_Perch, Night_Flight | 1025 | 2048 | 0.0014 (Oman) … 0.00916 (GreatWall) | Night_Flight −100 (no sea) | |

Every level uses a 3×3 cluster, patchSize 128 and 512 px colormaps. The full table is printed by `python work/hmall.py`, and `bf2terrain.py <Level> --no-png` prints it per level.

### 3.4 `Terrain.con` and `terraindata.raw`

`Terrain.con` has two branches:
- **Editor:** `terrain.create TerrainEditable` + `patchSize 128`, `subdividePatches 1`, `primaryWorldScale`, `secondaryWorldScale`, `patchColormapSize 512`, `lowDetailmapSize 512`, `detailmapBaseName "Levels/<L>/Detailmaps/tx"`, `lowDetailmapBaseName`, `colormapBaseName`, `lightmapBaseName`, `farSideTiling a/b`, `farTopTilingHi`, `farTopTilingLow`, `farYOffset`, `terrainWaterColor r/g/b`, `terrain.init`.
- **Game:** `terrain.create Terrain` + `terrain.load Levels/<L>/terraindata.raw`.

`terraindata.raw` (client.zip) is the compiled terrain: header, then per-patch geomorph/LOD geometry, then a quadtree. **Only the header is needed.** Rebuild the mesh from the heightmaps.

The header was verified on all 30 levels **[V]**; field names are cross-checked with OBF2 RE notes **[C]**:

| offset | type | field | Karkand |
|---|---|---|---|
| 0 | u32 | version | `0x0001001a` |
| 4 | f32[3] | primaryWorldScale | 2 / 0.00457764 / 2 |
| 16 | f32[3] | secondaryWorldScale | 4 / 1.17188 / 4 |
| 28 | u32 | garbage | `0xcdcdcdcd` |
| 32 | f32 | maxHeight | 254.92 |
| 36 | f32 | minHeight | 83.82 |
| 40 | u32 | patchSize (cells) | 128 |
| 44 | u8 | subdividePatches | 1 |
| 45 | u32 | patchesPerSide | 4 (8 for 1025 maps) |
| 49 | u32 | patchColormapSize | 512 |
| 53 | u32 | lowDetailmapSize | 512 |
| 57 | 4 × string + `\n` | colormap, detailmap, lowDetailmap, lightmap base names | `Levels/Strike_at_Karkand/Colormaps/tx` … |
| … | f32[2] | farSideTiling | 5 / 5 |
| … | f32, f32, f32 | farTopTilingHi, farTopTilingLow, farYOffset | 24, 24, 0 |
| … | f32[3] ×3 | sunColor, GIColor, terrainWaterColor | .75/.71/.57, .73/.64/.33, .337/.282/.157 |
| … | u32 | detail texture count | always 6 |
| … | 6 × { string+`\n`, u8 triPlanar, f32 sideTilingU, f32 sideTilingV, f32 topTiling, f32 yOffset, u8 envMap } | detail textures | see below |

The blob wins over Terrain.con. Karkand's Terrain.con says `farTopTilingLow 4`, but the blob has 24.

Karkand's detail textures (VFS path + `.dds`, in `Common_client.zip` or the level overlay), with the weight channel as verified in §0:

| i | texture | triPlanar | side | top | weight |
|---|---|---|---|---|---|
| 0 | `common\terrain\textures\detail\detail_rock04` | 1 | 32/16 | 50 | `_1`.B |
| 1 | `…detail_grass05` | 0 | 2/2 | 64 | `_1`.G |
| 2 | `…detail_gravel` | 0 | 3/2 | 64 | `_1`.R |
| 3 | `…detail_tarmac02` | 0 | 2/2 | 42 | `_2`.B |
| 4 | `…detail_stones03` | 0 | 2/2 | 64 | `_2`.G |
| 5 | `…detail_cobble2` | 0 | 2/2 | 64 | `_2`.R |

- Texture 0 is normally the tri-planar cliff rock.
- `envMap = 1` marks the wet/alpha detail textures (`*_alpha*`, `*_env*`, swampgrass).
- Texture names can be empty (Wake Island has 2 empty slots).
- **[I]** `topTiling` is probably repeats per 256 m patch (Karkand grass repeats every 4 m). Calibrate visually.

### 3.5 Terrain textures in `client.zip` [V]

| file | format | meaning |
|---|---|---|
| `Colormaps/txXXxZZ.dds` | 512² DXT1, 10 mips | Albedo per patch. XX = column (+X), ZZ = row (+Z), `patch = patchSize·sx` m (256 m). Patches fully under water have no file (Dalian 51 of 64, Wake 22). |
| `Colormaps/tx_s0..7.dds` | 512² DXT1 | Colormaps of the 8 surrounding cells. **[V]** They go clockwise from the north-west cell, each image turned a further quarter turn (confirmed by correlating with the secondary heightmaps' slopes on Karkand, Kubra Dam and Gulf of Oman; see `crates/bf2_import/src/terrain.rs`). |
| `Lightmaps/txXXxZZ.dds` | 512² DXT1 | Static terrain light. **G = sun visibility** (static shadows, compared against the dynamic shadow map). **B = sky/GI factor** (×`terrain.GIColor`). R is unused by the game terrain shaders. |
| `Detailmaps/txXXxZZ_1.dds`, `_2.dds` | 256² R5G6B5 (131,200 B) or DXT1 (32,896 B), no mips | Weights of detail textures 0–2 and 3–5 (channels B,G,R). `_2` is absent when the patch uses only textures 0–2. |
| `LowDetailmaps/txXXxZZ.dds`, `tx_s*.dds` | 256² DXT1 | Per-patch weights for the level-wide low-detail texture: R = top-plane term, B = "mountain" (side) term. |
| `lowdetailtexture.dds` | 512² R5G6B5, 5 mips | Level-wide low-detail texture. z = top plane, x/y = side planes. |
| `groundhemi.dds` | 1024² DXT5 | Hemisphere-lighting ground map. `hemiMapManager.setBaseHemiMap Levels/<L>/groundhemi 0/0/0 2048 500` gives path, centre, size and height. |
| `Hud/Minimap/ingameMap.dds` | 512² DXT1 | North-up minimap (= colormap mosaic flipped vertically). Also `commanderMap.dds`, `commanderMicromap.dds`, and `GameModes/gpm_cq/<n>/menuMap{Large,Small}.png`. |
| `Envmaps/EnvMap0.dds`, `Water/EnvMap.dds` | 128² DXT3 cube | Static environment cube maps. `Envmaps/EnvironmentMapInfo.emi` is text: `EnvMap0 0,0,0,1`. |
| `terraindata.raw` | binary | See §3.4. |

**UVs [V]:** a patch-local texture coordinate is `u = (x − patchMinX)/patchWorld` and `v = (z − patchMinZ)/patchWorld`, where `patchMin = −S/2 + index·patchWorld`. v = 0 is image row 0.

The **game shader** (`Shaders_client.zip:TerrainShader_Hi.fx`, `Hi_PS_FullDetail` and `Hi_PS_DirectionalLightShadows`) composes the terrain in additive passes, one pass per detail texture:

```
lightmap        = tex(Lightmaps/tx..)                       ; light pass (render target "accumlights")
accum.rgb       = saturate(lightmap.b * GIColor * 2) * 0.5
accum.a         = min(dynamicShadow, lightmap.g)
light           = 2*accum.a*SunColor + accum.rgb             ; SunColor = terrain.sunColor, GIColor = terrain.GIColor
weight_i        = dot(componentSel_i, tex(Detailmaps/tx.._{1|2}))      ; channel B/G/R per §0
detail_i        = tex(detailTexture_i, worldXZ * nearTiling)  ; tri-planar (x/y/z planes blended by |normal|-0.2) if triPlanar
low             = lerp(0.5, lowdetailtexture.z(topUV), lowComp.r*fade) * 4*lerp(0.5, mountain, lowComp.b)
                  (mountain = side-plane lowdetailtexture.y/x blended by normal)
detailout       = lerp(2*detail_i*low, low, farFade)
color          += weight_i * lerp(Fog, 2 * detailout * colormap * light, fogFactor)
```

The low-detail fallback (`Shared_PS_LowDetail`) is `colormap * light * 2 * low…`. Below the water level it lerps to `terrainWaterColor`.

### 3.6 Water [V]

- **Water plane:** at `heightmapcluster.setSeaWaterLevel` (world Y). Karkand is 146. Its brown river and lake appear in the minimap exactly where the terrain is below 146 (see `previews/Strike_at_Karkand_hillshade.png`). Night_Flight uses −100, i.e. no water.
- **`Water.con`:**
  - `renderer.waterScroll 0.1/0.1`, `waterAnimSpeed 50`
  - `renderer.waterColor 0.2549/0.1804/0.0275`
  - `waterSpecularColor r/g/b/a`, `waterSpecularPower 15`
  - `waterFogColor 51.5/51.5/128` (0–255), `waterFogStartEndAndBase 10/250/1/0.5`
  - `terrain.waterSunIntensity 0.8`
- **Terrain under water** uses `terrainWaterColor` from terraindata.raw.
- The water shaders are `RaShaderWater*.fx`.

### 3.7 Sky, sun, fog (`Sky.con`) [V]

```
Lightmanager.skycolor / ambientcolor / sunColor / sunSpecColor      (0..1)
terrain.sunColor, terrain.GIColor                                    (game branch; editor: LightSettings.TerrainSunColor/SkyColor)
Lightmanager.staticSunColor / staticSpecularColor / staticSkyColor   (baked-lighting inputs)
Lightmanager.sunDirection -0.260104/-0.80032/-0.540216               (unit vector FROM the sun, world space)
Lightmanager.effectSunColor / effectShadowColor / treeAmbientColor / treeSunColor / treeSkyColor / hemilerpbias
hemiMapManager.setBaseHemiMap <path> <centre x/y/z> <size> <height>
run /Common/Sky/SkyDome/skydome.con ; run /Common/Sky/Flare/flaresprite.con   (game branch)
Skydome.skyTexture common\textures\sky\karkand_cloudy  (+ cloudTexture[2], hasCloudLayer[2], scrolldirection[2],
        domeRotation 60, fadeCloudsDistances 900/500, cloudLerpFactors, flareTexture, flareDirection)
Renderer.fogColor 163/135/86            (0..255!)
Renderer.fogStartEndAndBase 0/135/2.3/0.4   (start m / end m / base / ?)   ; GameLogic.MaximumLevelViewDistance 140 (Init.con)
```

### 3.8 Undergrowth, overgrowth and byte grids [V]

All grids below have the heightmap orientation (row = +Z). **[V]** Undergrowth id 4 correlates with the tarmac detail weight at 0.94 in identity orientation, and ≈0 when flipped.

| file | layout | meaning |
|---|---|---|
| `HeightmapPrimary.mat` (server) | N² u8 | Terrain material id per sample (`heightmap.loadMaterialData`), used for physics/effects. Karkand values: 12, 3, 4, 5, 0. |
| `Undergrowth.raw` (client) | N² u8 | Undergrowth material id per sample. Matches `Material <name> <id>` in `Undergrowth.cfg`. Ids 1..6 equal detail texture index+1 on Karkand (rock 1, defaultgrass 2, gravel 3, tarmac 4, stones 5, cobble 6); extra ids are 7 flowers, 13 erase, 14 desertbush. |
| `Undergrowth.cfg` (client) | text | Globals (`ViewDistance 30`, `PatchSubdivide 15`, `SwayScale`, `LightingScale`, `AlphaRef`), then `Material name id { GeneralHeight; Type name { Mesh / CrossSize w h; Texture; RandomSizeScale a b; Density; Variation; TerrainColorScale; TypeSwayScale; Skew } }`. |
| `Undergrowth.dat` (client) | binary | Compiled cfg including the plant meshes BF2 generated (UVs already in the atlas). **[V]** Parses exactly on all 30 levels; the layout is documented in `crates/bf2_import/src/vegetation.rs`, which reads it instead of the cfg. |
| `UndergrowthAtlas.tai` + `UndergrowthAtlas0.dds` (2048×512 DXT5) | text atlas | Grass textures. |
| `Overgrowth/Overgrowth.con` (server) | script | `Overgrowth.addMaterial name id`, `addType`, `OvergrowthType.geometry <template>`, density, normalScale, rotationScale, minRadiusToSame/Others, `Overgrowth.path`, `preLoad`. |
| `Overgrowth/Overgrowth.raw` | N² u8 | Overgrowth (tree) material id per sample. Karkand: 0, 1 (tree1), 2 (pinebushes). The engine scatters trees procedurally. |
| `Overgrowth/OvergrowthCollision.con` | script | `Object.create <tree>` + `Object.absoluteTransformation` + `isOvergrowth 1`. These are the only overgrowth trees with explicit transforms (collision near the play area): 482 on Karkand. **[I]** Trees outside this list are procedural. |
| `Overgrowth/OvergrowthShadowmap.raw` | N² u8 | Tree shadow on terrain. |
| `Overgrowth/OvergrowthAtlas.tai` / `OvergrowthAtlas0.dds` | atlas | Billboard/LOD textures. |
| `SimpleShadowmap.raw` / `SimpleShadowmapio.raw` | M² u8 (M = 512 or 1024), `io` has +1..3 trailing bytes | Low-quality static shadow maps. Not needed. |

---

## 4. Object placement

### 4.1 `StaticObjects.con` [V]

```
if v_arg1 == BF2Editor
console.allowMultipleFileLoad 0
run /objects/staticobjects/_middle-east/city/city_architecture/gas_station/gas_station.con   (x 347 on Karkand)
console.allowMultipleFileLoad 1
endIf

rem *** gas_station ***
Object.create gas_station
Object.absolutePosition -134.000/167.293/-277.000
Object.rotation -180.0/0.0/0.0
Object.layer 1
```

Every instance is `create` + `absolutePosition` + optional `rotation` + `layer` + optional flags. Rotation is always three components when present (37,412 of 37,412 in levels). Object template types placed as statics are `SimpleObject`, `Bundle`, `DestroyableObject`, `Ladder`, `AnimatedBundle` and `RotationalBundle`, plus effects and sound templates in `AmbientObjects.con`.

Child placement: `world(child) = local(child) · world(parent)` in row-vector form, where `local = R(setRotation) + T(setPosition)`.

**Rotation matrix** (row vectors, LH), with cY = cos(yaw) etc. **[C]**:

```
row0 = [ cR·cY + sR·sP·sY,   sR·cP,   −cR·sY + sR·sP·cY ]
row1 = [−sR·cY + cR·sP·sY,   cR·cP,    sR·sY + cR·sP·cY ]
row2 = [ cP·sY,             −sP,       cP·cY            ]
```

This is identical to `D3DXMatrixRotationYawPitchRoll`. It also agrees with the layout of the overgrowth `absoluteTransformation` matrices: pure-yaw trees are `[c 0 −s][0 1 0][s 0 c]`.

### 4.2 Conversion to Bevy (right-handed, Y up, −Z forward)

Mirror Z so that BF2 north (+Z) becomes Bevy forward (−Z) and east stays +X:

- **Position:** `p_bevy = (x, y, −z)`.
- **Yaw/pitch/roll (degrees → radians):**

  ```rust
  let q = Quat::from_rotation_y(-yaw) * Quat::from_rotation_x(-pitch) * Quat::from_rotation_z(roll);
  // equivalently Quat::from_euler(EulerRot::YXZ, -yaw, -pitch, roll)
  ```

  Checks:
  - yaw +90° turns forward (−Z) to +X (east).
  - +pitch tilts forward down.
  - +roll lifts +X.
- **Matrices:** with a BF2 row-vector matrix M (4×4, translation in row 3) and S = diag(1,1,−1,1), use `Mat4_bevy = S · Mᵀ · S`. In other words, transpose, then negate the elements (0,2), (1,2), (2,0), (2,1) and the z translation.
- **Meshes:** vertices and normals become `(x, y, −z)` and the triangle winding must be reversed (a mirror flips handedness).
- **Heightmap:** Bevy Z of row r is `−(r·sz − h)`.

### 4.3 `GameModes/<gpm>/<size>/GamePlayObjects.con` [V]

A file has three sections, each with templates first and then `if v_arg1 == host` placements.

**Vehicle spawners.** The template name is `CPNAME_<map>_<size>_<cp>_<kind>`:

```
ObjectTemplate.create ObjectSpawner CPNAME_SK_64_gasstation_HeavyTank_0
ObjectTemplate.activeSafe ObjectSpawner CPNAME_SK_64_gasstation_HeavyTank_0
ObjectTemplate.isNotSaveable 1 / hasMobilePhysics 0
ObjectTemplate.setObjectTemplate 1 RUTNK_T90         <- vehicle for team 1 (MEC)
ObjectTemplate.setObjectTemplate 2 USTNK_M1A2        <- vehicle for team 2 (US)
ObjectTemplate.minSpawnDelay 20 / maxSpawnDelay 50 / teamOnVehicle 1 / spawnDelayAtStart / maxNrOfObjectSpawned
                  / setSpawnPositionOffset x/y/z / setOnlyForAI|Human
if v_arg1 == host
   Object.create CPNAME_SK_64_gasstation_HeavyTank_0
   Object.absolutePosition -172.823/161.298/-309.137
   Object.rotation 90.000/0.000/0.000
   Object.setControlPointId 13                        <- spawner belongs to CP id 13 (team follows CP owner)
   Object.layer 3
endIf
```

**Soldier spawn points:**

```
ObjectTemplate.create SpawnPoint CPNAME_SK_64_gasstation_1
ObjectTemplate.setControlPointId 13      (+ optional setGroup, setOnlyForAI/Human, setEnterOnSpawn...)
Object.create CPNAME_SK_64_gasstation_1  (outside the host block)  + absolutePosition + rotation (yaw = facing)
```

**Control points.** A control point is a template with an attached `flagpole` child:

```
ObjectTemplate.create ControlPoint CPNAME_SK_64_hotel
ObjectTemplate.setNetworkableInfo ControlPointInfo
ObjectTemplate.hasCollisionPhysics 1 / physicsType Mesh
ObjectTemplate.addTemplate flagpole
ObjectTemplate.setControlPointName CPNAME_SK_64_hotel   (localisation key)
ObjectTemplate.radius 5                     <- capture radius (m)
ObjectTemplate.team 1                       <- initial owner (0 neutral, 1, 2)
ObjectTemplate.controlPointId 26
ObjectTemplate.areaValueTeam1 13 / areaValueTeam2 13
ObjectTemplate.timeToGetControl 25 / timeToLoseControl 15
ObjectTemplate.unableToChangeTeam 1         <- uncappable main base
(also: hoistMinMax, loseControlWhenEnemyClose, showOnMinimap ... )
if v_arg1 == host
   Object.create CPNAME_SK_64_hotel + absolutePosition + layer
endIf
```

**Combat area:**

```
CombatAreaManager.use 1 / timeAllowedOutside 10 / (damage)
CombatArea.create CombatArea_138_64
CombatArea.min 0/0 ; CombatArea.max 0/0     <- always 0 in retail data (bounds computed)
CombatArea.addAreaPoint x/z                 <- 2D polygon in world X/Z (151 points on Karkand 64)
CombatArea.team 0 (all retail) ; CombatArea.vehicles n (values 0,1,2,3,4,5 seen; 4 most common)
CombatArea.layer n ; (CombatArea.usedByPathfinding)
```

**[I]** `vehicles` is a vehicle-class filter; its exact meaning is unverified. Karkand 64 has an empty-polygon area with `vehicles 4` next to the real area with `vehicles 0`.

**Modes and sizes:**
- Only the modes listed in `Info/<L>.desc` are real. Dragon_Valley and Highway_Tampa also contain `gpm_cq/128`, `gpm_sl/*` and `sp2`/`sp3` stubs of 432 bytes with empty host blocks.
- `sp1/16` is a copy of `gpm_coop/16` (single-player).
- Kits and teams come from Init.con `gameLogic.setKit team slot kit soldier` and `setTeamName`/`setTeamFlag`.

### 4.4 Other placement files [V]

- **`CompiledRoads.con`:** `object.create <roadTemplate>` / `object.geometry.loadMesh Levels\<L>\Roads\<name>_compiled.mesh` / `object.absoluteposition x/y/z`, with no rotation. All 980 referenced meshes in all levels are format version 4.

  **`Roads/*_compiled.mesh` byte layout, version 4 [V]** (1,122 of 1,122 v4 files parse exactly; ~260 unreferenced stale files use other versions 0–3 and can be ignored):

  | offset | type | field |
  |---|---|---|
  | 0 | u16 version (=4), u16 (=1) | |
  | 4 | f32[3] | centre = the `absoluteposition` of the object |
  | 16 | f32 | bounding radius |
  | 20 | f32[3], f32[3] | AABB min, AABB max (world) |
  | 44 | u32 | 0 |
  | 48 | u32 | vertex count V |
  | 52 | V × 32 bytes | `f32 pos[3]` (relative to centre, y ≈ +1.5e-5 above terrain), `f32 uv0[2]`, `f32 uv1[2]` (uv1 = uv0 × 1/scale of the 2nd texture), `f32 alpha` (0 at the edges, blend) |
  | … | u32 | index count I (triangle list, multiple of 3) |
  | … | u16[I] | indices |
  | … | u32 | chunk count C |
  | … | C × {u32 firstIndex, u32 indexCount, f32 radius, f32 centre[3]} | culling chunks |

  The road textures come from the editor `RoadTemplateTexture.SetTextureFile` in `objects/roads/splines/<template>.con` (primary + secondary, e.g. `objects\roads\textures\tarmac_mirror`), or from `<template>_compiled.dat` in `Objects_client.zip`: newline-terminated texture names followed by floats.
- **`AmbientObjects.con`:** `Trigger`, `AmbientEffectArea` and `Sound` templates plus instances (dust, birds, sounds).
- **`StaticLights.con`, `StaticGlows.con`, `StaticSprites.con`** (xpack): light-source objects.
- **`TriggerableTemplates.con` / `Triggerables.con`:** elevators, doors and switches. They use `Object.absolutePositionSecondary` and `rotationSecondary` for the second state.
- **Static object lightmaps** (client.zip `lightmaps/objects/LightmapAtlas.tai` + `LightmapAtlas<N>.dds`, 2048² DXT1; Karkand has 21 pages and 1,827 entries):
  - Each line is `levels/<l>/lightmaps/objects/<template>=<GL>=<X>=<Y>=<Z>.dds <atlas>, <idx>, <uOff>, <vOff>, <uScale>, <vScale>`.
  - X/Y/Z are the instance position **truncated toward zero** **[V]**: 1,823 of 1,827 match trunc, 625 match floor, 337 match round.
  - GL is two digits, geometry index and LOD (00..03, 10..12).
  - The shader uses `uvLM' = uvLM · (uScale, vScale) + (uOff, vOff)` (`StaticMesh_r3x0.fx lightmapOffset`).

---

## 5. Other level files, briefly [V unless noted]

| file | what it is | needed? |
|---|---|---|
| `server.zip/Init.con`, `Heightdata.con`, `Terrain.con`, `Sky.con`, `Water.con`, `Sounds.con` (`sound.setReverb *.eax`), `CompiledRoads.con`, `StaticObjects.con`, `AmbientObjects.con`, `GameModes/**` | scripts, §2–4 | yes |
| `map.desc` (server) | same schema as Info desc but `mode type="conquest"/"co-op"`, the older naming | no (use Info desc) |
| `tmp.con` | 0 bytes; `run tmp.con v_arg1` | no |
| `ai/AI.ai`, `GameModes/*/*/AI/StrategicAreas.ai`, `Strategies.ai` | AI scripts (`aiStrategicArea.createFromControlPoint`, neighbours, order positions) | bots only |
| `AIPathFinding/AerialHeighMap.ahm` | `f32 cellSize(16), i32 w, i32 h, f32 originX, f32 originZ, u16 height[w·h]` (row = +Z, metres). Max terrain/object height per 16 m cell; median = terrain max, up to +50 m over buildings. Size check 20+2·64·64 = 8,212 ✓. | AI aircraft |
| `AIPathFinding/{Infantry,Vehicle}.qtr` / `.clb` | Compiled navmesh quadtree and cluster graph (binary, undocumented) **[C]** | bots |
| `GTSData/Output/*.qti/.cls/.vbf` | NavMesh SDK output (quadtree index, clusters, vertex buffer). GreatWall also ships `.txt`/`.obj` debug dumps. **[C]** | bots |
| `Envmaps/EnvironmentMapInfo.emi` | Text: `EnvMap0 0,0,0,1`. **[I]** A probe name plus a position or flag. | optional |
| `Undergrowth.*`, `Overgrowth/*`, `*.raw`, `.mat` | §3.8 | foliage |
| `loadCounter.dat` | 4 bytes, u32 (Karkand 0x2cfb = 11,515). **[I]** Loading-bar step count. | no |
| `lightmaps/objects/*` | §4.4 | lighting |
| `Roads/*_compiled.mesh` | §4.4 | yes |
| `*.tai` (Undergrowth / Overgrowth / Lightmap atlas) | Text: `# ...` header, then `<orig texture>\t\t<atlas dds>, <atlas idx>, <uOff>, <vOff>, <uWidth>, <vHeight>` | yes (lightmaps) |
| `Hud/Minimap/*`, `GameModes/gpm_cq/*/menuMap*.png`, Info PNGs | UI | UI |
| `objects/**`, `common/**` (booster levels) | Level-local content, overlaid on the root | yes |
| level `ClientArchives.con`/`ServerArchives.con` | Extra mounts (§1.3) | yes |

`.vbf`, `.clb`, `.cls` and `.qti` are the extensions asked about; all are AI-navigation data. `.dat` covers `loadCounter.dat`, `Undergrowth.dat` and `GTSData/Meshes/*.dat` (combat area / config for GTS, in 2 levels).

---

## 6. `Info/<Name>.desc`, modes, and the level list

**Info desc [V].** Loosely-formed XML, so parse leniently:

```xml
<map gsid="4">
  <name> Strike At Karkand </name>
  <briefing locid="LOADINGSCREEN_MAPDESCRIPTION_strikeatkarkand">...</briefing>
  <music> common/sound/menu/music/load_MEC_music.ogg </music>
  <isnightmap> 1 </isnightmap>                        (Night_Flight only)
  <modes>
    <mode type="gpm_cq">
      <maptype ai="1" players="16" type="assault" locid="GAMEMODE_DESCRIPTION_assault">…</maptype>
      <maptype players="32" type="assault" …/>  <maptype players="64" …/>
    </mode>
    <mode type="gpm_coop"> <maptype ai="1" players="16" type="assault" …/> </mode>
  </modes>
</map>
```

- `players` selects `GameModes/<mode>/<players>/`.
- `ai="1"` means bots are supported (single-player or co-op).
- `type` is `assault`, `headon` or `doubleassault`: whether each side has an uncappable base.
- Extra `*_AI.desc` files (`Dalian_plant_AI.desc`, `FuShe_Pass_AI.desc`) are old leftovers that still use `conquest`.

**All 30 levels in this install [V]:**

| mod | folder | name | gsid | pack | map | modes (Info desc; `ai` = bots) |
|---|---|---|---|---|---|---|
| bf2 | Dalian_Plant | Dalian Plant | 101 | BF2 | 2 km | cq 16ai(double) 32(headon) 64(headon); coop 16ai |
| bf2 | Daqing_oilfields | Daqing Oilfields | 100 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Dragon_Valley | Dragon Valley | 102 | BF2 | 2 km | cq 16ai/32/64; coop 16ai (+stub dirs) |
| bf2 | FuShe_Pass | FuShe Pass | 103 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Gulf_Of_Oman | Gulf Of Oman | 6 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Kubra_Dam | Kubra Dam | 0 | BF2 | 2 km | cq 16/32/64 |
| bf2 | Mashtuur_City | Mashtuur City | 1 | BF2 | 2 km | cq 16/32/64 |
| bf2 | Operation_Clean_Sweep | Operation Clean Sweep | 2 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Sharqi_Peninsula | Sharqi Peninsula | 5 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Songhua_Stalemate | Songhua Stalemate | 105 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Strike_at_Karkand | Strike At Karkand | 4 | BF2 | 1 km | cq 16ai/32/64; coop 16ai |
| bf2 | Zatar_Wetlands | Zatar Wetlands | 3 | BF2 | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Road_To_Jalalabad | Road To Jalalabad | 12 | patch | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | Operation_Blue_Pearl | Operation Blue Pearl | 120 | patch | 1 km | cq 16/32/64 |
| bf2 | Wake_Island_2007 | Wake Island 2007 | 601 | patch (ex-DRM) | 2 km | cq 64 |
| bf2 | Highway_Tampa | Highway Tampa | 602 | patch (ex-DRM) | 2 km | cq 16/32/64 (+stubs) |
| bf2 | GreatWall | Great Wall | 110 | Euro Force | 2 km | cq 16ai/32; coop 16ai |
| bf2 | OperationSmokeScreen | Operation Smoke Screen | 10 | Euro Force | 2 km | cq 16ai/32; coop 16ai |
| bf2 | Taraba_Quarry | Taraba Quarry | 11 | Euro Force | 2 km | cq 16ai/32; coop 16ai |
| bf2 | Midnight_Sun | Midnight Sun | 200 | Armored Fury | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | OperationRoadRage | Operation Road Rage | 201 | Armored Fury | 2 km | cq 16ai/32/64; coop 16ai |
| bf2 | OperationHarvest | Operation Harvest | 202 | Armored Fury | 2 km | cq 16ai/32/64; coop 16ai |
| xpack | Devils_Perch | Devil's Perch | 300 | Special Forces | 2 km | cq 16ai/32/64; coop 16ai |
| xpack | Iron_Gator | The Iron Gator | 301 | SF | 1 km | cq 16/32/64 |
| xpack | Night_Flight | Night Flight (night) | 302 | SF | 2 km | cq 16ai/32/64; coop 16ai |
| xpack | Warlord | Warlord | 303 | SF | 1 km | cq 16ai/32/64; coop 16ai |
| xpack | Leviathan | Leviathan | 304 | SF | 1 km | cq 16/32/64 |
| xpack | Mass_Destruction | Mass Destruction | 305 | SF | 1 km | cq 16ai/32/64; coop 16ai |
| xpack | Surge | Surge | 306 | SF | 1 km | cq 16ai/32/64; coop 16ai |
| xpack | Ghost_Town | Ghost Town | 307 | SF | 1 km | cq 16ai/32/64; coop 16ai |

---

## 7. Worked example: Strike_at_Karkand (mods/bf2)

**Archives:**
- `client.zip` is 36.1 MB with 186 files: roads 52 (32 referenced), 16+8 colormaps, 28 detailmaps (16 `_1` + 12 `_2`), 16 lightmaps, 16+8 lowdetail, 21 object-lightmap atlas pages, `terraindata.raw` 14.7 MB.
- `server.zip` is 2.7 MB with 52 files.

**Heightmap:**
- 513×513 u16, scale 2/0.00457764/2: 2 m cells, 1024 m square, X/Z from −512 to +512.
- 1 height unit = 4.58 mm (max 300 m). Raw 18310..55689 gives Y 83.82..254.92 m, mean 164.7 m.
- Sea level 146 m; 21.3 % of samples are under water (the river in the north and the lake in the east).
- 8 secondaries of 257² u8 at 4 m, scale y 1.17188, covering a 3072 m square.
- `terraindata.raw`: 4×4 patches of 128 cells (256 m), 512 px colormaps, far tiling 5/5, top 24/24.
- Sun colour .75/.71/.57, GI .73/.64/.33, terrain water colour .337/.282/.157.
- Sun direction (−0.260, −0.800, −0.540), so light comes from the north-east and above.
- Fog 163/135/86, from 0 to 135 m; max view distance 140 m.

**Static objects (game branch):**
- 1,335 `Object.create` (1,336 including the editor-only DefaultEnvMap). The editor block has 347 `run` lines.
- 840 instances have `Object.rotation`, 148 of them with non-zero pitch or roll.
- 265 are flagged `isOvergrowth`, 26 have `disableChildren`, 8 have `notInAI`.
- 257 unique templates: SimpleObject 1,206, Bundle 61, DestroyableObject 47, Ladder 15, AnimatedBundle 6. They reference 253 distinct `.staticmesh`/`.bundledmesh` files, all present in the client archives.
- Other placed objects: 32 compiled roads, 482 explicit overgrowth collision trees, 55 ambient objects/triggers.

**Game modes** (from `bf2con.py`):

| mode/size | CPs | soldier spawns | vehicle spawners | combat area |
|---|---|---|---|---|
| gpm_cq/16 | 4: hotel(1), market(1), square(1), gasstation(2, uncappable) | 23 | 22 | 68-point polygon |
| gpm_cq/32 | 7: +trainaccident, gatehouse, suburb | 37 | 34 | 122 pts (+ empty `vehicles 4` area) |
| gpm_cq/64 | 9: gasstation(US base, id 13, r = 10 m), hotel, square, trainaccident, gatehouse, cementfactory, factory, warehouse, suburb (all MEC) | 48 | 42 | 151 pts (+ empty `vehicles 4` area) |
| gpm_coop/16 = sp1/16 | 4 (ids 501–504) | 23 | 22 | 68 pts |

The CQ64 vehicle pool (team 1 / team 2), counted as spawner slots:
- bipods 13/13
- Vodnik/HMMWV 8/8
- HJ8/TOW 7/7
- BTR-90/LAV-25 4/4
- T-90/M1A2 2/2
- D-30/M198 artillery 2/2
- UAV trailer / radar 1/1

**Teams:** MEC (1) vs US (2). Tickets per size: 16 = 100/110, 32 = 200/220, 64 = 300/330. Drop vehicles are `jep_vodnik` and `usjep_hmmwv`.

**Previews:** `previews/Strike_at_Karkand_{height,hillshade,cluster,colormap}.png`, all north-up. The hillshade water mask matches the in-game minimap.

---

## 8. Companion tools (tested on this install)

**`bf2con.py`** is a VFS, lexer, parser and interpreter. Run `python bf2con.py test`:
- It passes 20/20 checks. It loads Karkand CQ64, xpack Devils_Perch (parent-mod archives) and GreatWall (level-local objects + root overlay), and runs all 2,468 bf2 object `.con` files (4,608 files with includes) in about 5 s.
- The object run produced 10,082 templates and 1,684 geometries; 1,590 of 1,598 mesh files resolved.
- `python bf2con.py level <Level> <gpm> <size> [--mod xpack] [--json out.json]` dumps an importer-ready JSON: static objects, gameplay objects, template→mesh map, combat areas, settings.
  - For Karkand CQ64 this is 0.8 MB (`../work/karkand64.json`). Each entry looks like `{"template","position","rotation","props","type","src"}`.
  - Gameplay entries also carry `template_props` and `children`. `template_meshes` maps each template to `{geometry_type, mesh, children}`.
- It also loads all 30 levels with 0 warnings; only the 7 dangling retail template references listed in §2.3 remain.

**`bf2terrain.py`:**
- Parses the cluster, heightmaps, secondaries and the terraindata header.
- Runs the seam check and a control-point height check.
- Writes north-up PNG previews (height, hillshade with sea level, 3×3 cluster, optional `--colormap` mosaic from the DXT1 tiles).

---

## 9. Open questions / not verified

1. **Yaw/pitch/roll signs and order.** These come from two community code bases plus an RE claim, not from an in-engine test; my own check covers only the matrix layout. A cheap check in the engine: place a known asymmetric object with yaw 90 and see whether it faces east.
2. **`include` vs `run` scoping** is from wiki hearsay. Retail data never relies on the difference.
3. **Exact Init.con arguments in the game** (empty per OBF2) and the contents of the runtime `tmp.con`. The importer only needs the game branch, so this does not matter for it.
4. **Mount priority** is inferred (earliest wins) from data intent. There is no engine test. The level-overlay priority over global archives is inferred from 4 differing files.
5. **`tx_s0..tx_s7` order** (surrounding-terrain colormaps and lowdetail maps). Colour and slope correlation tests were inconclusive.
6. **Units of `topTiling`/`sideTiling`**, per patch vs per metre. Calibrate visually. The same applies to the `farTopTiling*`/`farSideTiling` semantics.
7. **`CombatArea.vehicles` values** (0–5) and the empty-polygon areas.
8. **Road mesh versions 0–3** (unreferenced leftovers only) and `*_compiled.dat` float fields.
9. **The body of `terraindata.raw`** (per-patch geomorph data, quadtree) was not decoded. It is not needed if the terrain is rebuilt from the heightmaps.
10. **Lazy template lookup scope.** It is not verified whether the engine also searches level-local archives. GreatWall runs its level objects explicitly, which suggests it does not, but a community mount test hints that it might. The importer should index both.
11. **Booster ownership logic** hard-coded in the exe (Booster_*.zip + the 3 Armored Fury level names).
