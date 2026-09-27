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
- [x] Movement like BF2: stairs and ledges, slopes, jumping, stance capsules (values from
  the original executable)
- [x] Ladders
- [x] Sprint stamina, stance/fire delays, soldiers vs vehicles (pushes, roadkill, riding on
  vehicles), step smoothing, ladder climbing animation
- [x] Smooth animation blending, first and third person: crossfades, directional
  movement, speed-matched playback, jumps, turning in place, remote fire/reload
- [x] Scopes: BF2's zoom models with reticles, bolt-action rifles leave the scope
- [x] Destructible objects (barricades, barrels, signs, wrecks, chain explosions) and
  BF2's material damage table
- Thrown and launched projectiles (in progress): hand grenades (cooking, bouncing, fuse), smoke grenades
  with smoke that blocks sight, under-barrel grenade launchers, rocket launchers, C4 and
  claymores, shotgun pellets
- Lag compensation
- Per-bone hit zones
- [x] Man-down (BF2's 15 s, 320 HP wreck threshold) and revive with shock paddles
- [x] Medic, support and engineer abilities: medic/ammo bags (held or thrown), wrench repairs
  of vehicles and objects, kit ability charge, BF2 scoring

## M2: Conquest (in progress)

- [x] Control points: flags lowered and raised by the team with more soldiers in the radius,
  using each point's capture times (BF2's `gpm_cq` rules)
- [x] Ticket bleed by area value, a ticket per death, win conditions, round restart
- [x] Scoreboard, kill feed, capture notifications, flag and ticket HUD
- [x] Flag models moving on their poles
- [x] Minimap (north up or turning with the view)
- [x] Commander announcements (captures, losses, ticket bleed) in the team's language
- [x] Full-screen map (hold M)
- [x] Squads: create/join/leave, squad leader spawning, bots in squads
- [x] Commo rose (Q) with BF2's radio voice-overs, spotting (HUD, minimap, map), automatic soldier
  call-outs (reloading, grenade out, man down)
- [x] Commander: post, mutiny, squad orders, artillery, UAV, satellite scan, supply drops,
  commander screen (Caps Lock)

## M3: Vehicles (in progress)

- [x] Vehicle template trees imported (`vehicles/<name>.ron`): part tree, joints (turrets,
  barrels, steering), wheels, seats with soldier/camera/exit points, entry points, guns
- [x] Land vehicles as avian rigid bodies: raycast suspension, tyre grip, engine and brakes;
  skid-steered tracks. HMMWV/Vodnik, LAV-25/BTR-90/WZ551, M1A2/T-90/Type 98 and the rest of
  the land vehicles drive (14 types over Karkand, Dalian Plant and Gulf of Oman)
- [x] Vehicle spawners per control point owner, respawn after destruction or abandonment
- [x] Entering (E), seat switching (F1..F8), exiting beside the vehicle; seated soldiers ride
  along and capture flags
- [x] Seat cameras (first person) and chase camera (V); turrets follow the gunner's aim
- [x] Vehicle guns (main guns, coaxial and pintle MGs, firing ports), hit points through an
  excerpt of the material damage table, wrecks
- Vehicle prediction on clients (vehicles are interpolated 100 ms in the past for now)
  (phase 2 in progress: aircraft, helicopters, boats, stationary weapons, prediction)
- Engine/gearbox/tyre model fitted to the original (`newCar2.*`, gear ratios); per-face armour
  materials (front/side/rear/tracks) for direct hits
- Wings for jets, rotors for helicopters, floating for boats and amphibious APCs
- Seated soldier poses, first-person cockpit models (geom 0), track texture scrolling,
  guided missiles (TOW/HJ8), smoke launchers, vehicle HUD (weapon, ammo, heat)
- Stationary weapons (TOW, bipods, artillery) and commander assets

## M4: BF2-style bots

- [x] Infantry navigation from our collision data: layered walkability grid, cached,
  background A*; stuck events down from ~250/min to 1-3/min
- Path costs per vehicle class
- [x] Strategy after BF2's StrategicAreas.ai and AI strategies (commander AI per team), squad
  orders and wedge formations, utility behaviours (cover, flank, grenades, revive, bags), skill
  settings, ladders in navigation
- AI commander using assets, bots using launchers/repairs (in progress)
- Strategic layer from `StrategicAreas.ai`, squad orders, individual utility behaviours
  seeded from `AIBehaviours.ai` and per-object AI templates
- Bots driving vehicles and flying through the same input channels as players

## M5: Presentation

- [x] BF2-accurate materials: dirt/crack layers on statics, normal-mapped bundled and skinned
  meshes, environment maps
- [x] Water, undergrowth/overgrowth, surrounding terrain, view distance setting (grows with
  altitude), BF2's distant tree stand-ins
- Mesh LODs with BF2's LOD distances and cull radii for statics, vehicles and soldiers (in progress)
- Effects: particles for muzzle flashes, impacts, explosions, smoke (in progress)
- Audio: footsteps, distance falloff, flybys, ambience (in progress)
- [x] Main menu (play/host/join, level and mode selection), server browser, settings (graphics,
  audio, controls, key bindings, mouse sensitivity, field of view) saved to disk, pause menu:
  a modern design of our own, not a copy of BF2's UI
- [x] Server browser (LAN discovery, favourites), text chat (all/team/squad), end-of-round
  summary with career stats

## Later

- [x] Special Forces night vision, gas masks and tear gas, flashbangs, blast tinnitus
- [x] Zipline and grappling hook (Special Forces): BF2 rope lengths and lifetimes, climbing, sliding, over the network
- [x] Co-op: bots fill both teams around the humans, who play one side
- [x] Modding workflow: mods authored directly in glTF + RON (`mods/`, see docs/MODDING.md)
- [x] Admin tools: BF2-compatible RCON, chat admin commands, kick/ban; map rotation with live
  map changes; server config file; persistent player stats
- [x] Master server: servers register and heartbeat, the browser lists internet servers
