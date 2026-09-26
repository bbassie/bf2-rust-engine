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

Still to do for BF2 fidelity: baked lightmaps (statics and terrain), dirt/crack layers,
surrounding (outer) terrain, undergrowth/overgrowth, water shader, LODs.

## M1: Infantry combat (in progress)

- [x] Soldier meshes and animations (skinned mesh + `.ske`/`.baf` import), per-team models
- [x] Third-person movement animations: legs from the soldier set, upper body from weapon sets
- [x] Third-person camera (V)
- Weapon models in hands and first-person arms/weapon view
- Kits (7 classes per faction) and handheld weapons from the original templates:
  rate of fire, magazines, recoil, deviation, zoom
- Projectiles with gravity, server-side hit detection with lag compensation
- Material damage matrix (143 materials), per-bone hit zones
- Health, death, man-down/revive, respawn screen with spawn point selection
- Medic, support and engineer abilities (heal/resupply/repair all use the damage matrix)

## M2: Conquest

- Control points (capture by majority, flag animation), ticket bleed, win conditions
  ported from the original rules
- Scoreboard, kill feed, minimap
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
