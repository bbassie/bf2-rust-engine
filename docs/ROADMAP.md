# Roadmap

Milestones are ordered so that each one ends with something playable.

## M0: Foundation (done)

- [x] Cargo workspace: formats / importer / data / shared / server / client
- [x] Dedicated server, listen server, singleplayer
- [x] Replicated players and soldiers, server-authoritative movement
- [x] Client prediction + reconciliation, interpolation of remote soldiers
- [x] Bot skeleton driving soldiers through the same input path as humans
- [x] Built-in procedural test range
- [x] BF2 virtual file system (archives mounted like the game)
- [x] `.con` script interpreter and template store
- [x] Importer: terrain (heightmap + color maps) and static objects (meshes to glTF, collision)
- [x] Load an imported BF2 level (e.g. Strike at Karkand) and walk around it, also networked

## M0.5: Visual pass (done)

- [x] 16x anisotropic filtering
- [x] Static objects: base x detail texture layering (BF2 `BaseDetail*` techniques)
- [x] Terrain: six tiling detail textures blended by per-patch weight maps, tri-planar cliffs
- [x] Road decals from compiled road meshes
- [x] Sky dome with the level's sky texture
- [x] Bindless static materials (4x frame rate: ~270 fps on Karkand in release)
- [x] Screen-space ambient occlusion (with SMAA instead of MSAA; `--no-ssao` to compare)

Still to do for BF2 fidelity: baked lightmaps (statics and terrain), dirt/crack layers,
surrounding (outer) terrain, undergrowth/overgrowth, water shader, LODs.

## M1: Infantry combat (in progress)

- [x] Soldier meshes and animations (skinned mesh + `.ske`/`.baf` import), per-team models
- [x] Third-person movement animations: legs from the soldier set, upper body from weapon sets
- [x] Third-person camera (V)
- [x] Weapon models in hands (third person)
- [x] First-person view: BF2's arms and weapon models with their 1P animations (deploy, fire,
  zoom, reload, run, sprint), drawn by a second camera so they never clip into walls
- [x] Kits (7 classes per faction) and handheld weapons from the original templates:
  rate of fire, fire modes, magazines, recoil, deviation, zoom
- [x] Projectiles with gravity, server-side hit detection, damage falloff, headshots,
  explosions
- [x] Health, death, respawn, kill feed, scoreboard
- [x] Deploy screen with kit and spawn point selection on the level's map
- Lag compensation
- Material damage matrix (143 materials), per-bone hit zones
- Man-down/revive
- Grenades, shotgun pellets, scopes
- Medic, support and engineer abilities (heal/resupply/repair all use the damage matrix)

## M2: Conquest (in progress)

- [x] Control points: flags lowered and raised by the team with more soldiers in the radius,
  using each point's capture times (BF2's `gpm_cq` rules)
- [x] Ticket bleed by area value, a ticket per death, win conditions, round restart
- [x] Scoreboard, kill feed, capture notifications, flag and ticket HUD
- Flag models moving on their poles, capture sounds
- Minimap and full map in the HUD
- Squads and commander (basic)

## M3: Vehicles

- Vehicle template trees: rigid body root, rotational bundles (turrets), seats, entry points
- Engines, gearboxes and springs for land vehicles; wings for jets; rotors for helicopters;
  floating for boats. Tune against the original game's behaviour.
- Vehicle weapons, cameras, damage, wrecks, vehicle spawners

## M4: BF2-style bots

- Navmesh generated from our collision data (Recast), path costs per vehicle class
- Strategic layer from `StrategicAreas.ai`, squad orders, individual utility behaviours
  seeded from `AIBehaviours.ai` and per-object AI templates
- Bots driving vehicles and flying through the same input channels as players

## M5: Presentation

- BF2-accurate materials (detail/dirt/crack/lightmap layers on statics, normal-mapped
  bundled meshes), terrain splatting, water, undergrowth/overgrowth, effects, audio
- Menus, server browser, settings, HUD: a modern design of our own, not a copy of BF2's UI

## Later

- Special Forces gadgets (zipline, grappling hook, night vision), co-op
- Modding workflow: authoring new content directly in glTF + RON
- Admin tools (RCON), master server / server list, stats
