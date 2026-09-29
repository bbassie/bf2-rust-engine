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

(Dirt/crack layers, surrounding terrain, undergrowth, water, LODs and baked sky occlusion followed
in M5.)

## M1: Infantry combat

- [x] Soldier meshes and animations (skinned mesh + `.ske`/`.baf` import), per-team models
- [x] Third-person movement animations: legs from the soldier set, upper body from weapon sets
- [x] Third-person camera (V)
- [x] Weapon models in hands (third person)
- [x] First-person view: BF2's arms and weapon models with their 1P animations (deploy, fire,
  zoom, reload, run, sprint), drawn shrunk towards the eye so they never clip into walls
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
- [x] Thrown and launched projectiles: hand grenades (cooking, bouncing, fuse), smoke grenades
  with smoke that blocks sight, under-barrel grenade launchers, rocket launchers, C4 and
  claymores, shotgun pellets
- [x] Lag compensation: the server judges hits against where targets were on the shooter's
  screen (up to 250 ms)
- [x] Per-bone hit zones: BF2's skeleton collision capsules and damage table columns (head,
  body, body armour, limbs)
- [x] Man-down (BF2's 15 s, 320 HP wreck threshold) and revive with shock paddles
- [x] Medic, support and engineer abilities: medic/ammo bags (held or thrown), wrench repairs
  of vehicles and objects, kit ability charge, BF2 scoring

## M2: Conquest

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

## M3: Vehicles

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
- [x] Vehicle prediction for drivers (rewind/replay like soldiers): steering shows after
  17-32 ms instead of 200-260 ms over a local dedicated server
- [x] Jets (BF2 wings, control surfaces, engine, afterburner, landing gear, AoA limiter),
  helicopters (rotor lift, collective/cyclic/tail rotor), boats and amphibious APCs
  (floating bundles on the level's water), crash and water damage
- [x] Stationary weapons (TOW/HJ-8, HMGs, AA) from spawners; wire/TV guided missiles follow
  the sight, heat seekers lock on aircraft; gun overheat and weapon choice per trigger
- [x] Vehicle meshes as glTF skins (one joint per part); seated soldier poses; first-person
  interiors (geom 0); BF2 vehicle sights (reticles, periscope frames); vehicle HUD (guns,
  ammo, heat, lock, speed, altitude, throttle); damage smoke, explosions and wreck fires
  (BF2's armor effects)
- [x] Engine/gearbox/tyre model from BF2's `newCar2.*` numbers (gear ratios and shift points,
  gear changes, rev limiter, brake and engine brake torques, dynamic friction), fitted to BF2's
  AI top speeds; per-face armour materials (front/side/rear/top/tracks) for direct hits
- [x] Countermeasures (smoke launchers, decoy flares that break heat seeker locks), bombs,
  track texture scrolling (BF2's UV matrices: belts, rim treads, hubs, sprockets)
- [x] Vehicle HUD in our own style: seats and occupants, hit points, speed and gear, turret
  direction, guns and countermeasures; flight instruments for pilots (horizon and pitch
  ladder, heading, airspeed, throttle, altitude, climb rate, stall and pull-up warnings)
- [x] Artillery and commander assets as vehicles: the artillery pieces turn onto the target and fire
  their bursts as shells arcing to it (a strike needs a living piece, more pieces fire more shells, wrecks
  stay until repaired), the UAV circles the target and can be shot down, satellite scans show the enemy on
  the commander's map for him to spot, supply drops repair vehicles too
- [x] F-35B vertical take-off and hover (BF2's lift fan engine); parachutes when bailing out of
  aircraft (BF2's canopy)
- [x] Helicopter landing: the collective slows the descent near the ground, landed
  helicopters sit on their skids

## M4: BF2-style bots

- [x] Infantry navigation from our collision data: layered walkability grid, cached,
  background A*; stuck events down from ~250/min to 1-3/min
- [x] Vehicle navigation from our collision data: a 1 m vehicle grid with path costs per vehicle
  class (wheeled, tracked, amphibious: slopes, water depth, clearance for the vehicle's width,
  BF2's road meshes), other vehicles driven around, connected areas per class; a water grid for
  boats and an air map for aircraft; paths in 17-29 ms on background threads
- [x] Strategy after BF2's StrategicAreas.ai and AI strategies (commander AI per team), squad
  orders and wedge formations, utility behaviours (cover, flank, grenades, revive, bags), skill
  settings, ladders in navigation
- [x] AI commander (takes the post, gives way to humans, squad orders, artillery, UAV, scans,
  supply drops); bots follow a human commander's orders, use launchers, wire-guided rockets,
  flashbangs, tear gas and gas masks, repair vehicles and assets, throw bags
- [x] Utility behaviours weighted like `AIBehaviours.ai`, BF2's weapon AI templates (ranges,
  firing poses)
- [x] Bots in vehicles through the same use button, seat keys and inputs as players: squads take
  transports, APCs and tanks to distant objectives (claimed seats, drivers wait for riders),
  gunners join teammates' vehicles, stationary weapons are manned; pure pursuit driving with
  stuck recovery, tanks stop to fight, gunners lead their targets; boats, transport helicopters
  that land squads, attack helicopters that circle and fire, jets that take off and patrol;
  countermeasures, getting out at the objective, when wrecked or stuck; infantry shoot exposed
  crews, lay AT mines and use C4. Still rough: tight streets (2-5 stuck events per vehicle-minute
  on Karkand), carrier jump jets, jet dogfights

## M5: Presentation

- [x] BF2-accurate materials: dirt/crack layers on statics, normal-mapped bundled and skinned
  meshes, environment maps
- [x] Water, undergrowth/overgrowth, surrounding terrain, view distance setting (grows with
  altitude), BF2's distant tree stand-ins
- [x] Lighting depth: per-map world lighting from BF2's shader math and values (night maps are
  night), directional sky light, BF2's baked sky visibility as occlusion on statics and terrain
  (the sun stays dynamic), ray-traced sky occlusion for soldiers and vehicles, tonemapping and
  optional bloom
- [x] Static mesh LODs with BF2's switch distances (`setSubGeometryLodDistance`) and draw distances
  (cull radius), dithered cross-fades, zoom-aware like BF2
- [x] Vehicle and soldier LODs: BF2's lower LODs rigged like the full models, switched at BF2's distances
  (plus half the model's box, as the engine does) with dithered cross-fades, BF2's cull rule for player
  control objects and their small parts (carried weapons, pintle guns), zoom-aware; unseen soldiers aren't
  posed
- [x] Effects: BF2's particle effects for muzzle flashes, impacts per surface, explosions with
  their material-manager dust columns, smoke, destruction, scorch decals
- [x] Audio: BF2's sounds with its distance model, weapons (near/far, cracks), footsteps per
  surface, vehicles, ambience, voices, announcements ducking the rest
- [x] Main menu (play/host/join, level and mode selection), server browser, settings (graphics,
  audio, controls, key bindings, mouse sensitivity, field of view) saved to disk, pause menu:
  a modern design of our own, not a copy of BF2's UI
- [x] BF2's team flags and vehicle icons across the HUD, maps and menus; the level page previews
  each layout's objectives
- [x] Server browser (LAN discovery, favourites), text chat (all/team/squad), end-of-round
  summary with career stats

## Later

- [x] Special Forces night vision, gas masks and tear gas, flashbangs, blast tinnitus
- [x] Zipline and grappling hook (Special Forces): BF2 rope lengths and lifetimes, climbing, sliding, over the network
- [x] Co-op: bots fill both teams around the humans, who play one side
- [x] Modding workflow: mods authored directly in glTF + RON (`mods/`, see docs/MODDING.md)
- [x] Admin tools: BF2-compatible RCON, chat admin commands, kick/ban; map rotation with live
  map changes; server config file; persistent player stats
- [x] Content download: clients download a server's mods (and, if its admin opts in, the
  converted BF2 assets) for the maps being played, cached by hash, so modded servers need no
  install step and clients without BF2 can join `all` servers
- [x] Master server: servers register and heartbeat, the browser lists internet servers

## M6: Beyond BF2 (next)

Requests from 2026-09-29, in priority order.

- Iteration speed: shader and asset hot reload, Bevy system hot-patching, the Debian machine
  for Linux builds, tests and soaks
- Lighting: less warm shade than BF2's full tint; real dynamic lights at lamp objects on night
  maps
- Carriers: navigation over the whole ship (catwalks, hangar, deck) so bots use them fully
- Vehicles: small plants must not stop vehicles; climbing slopes
- [x] Game modes beyond conquest that bots play: a mode framework (rules, replicated state,
  HUD, spawns and bot strategy per mode), Rush (pairs of charges per stage to arm and defuse,
  attacker tickets refilled per stage) and Breakthrough (sectors of flags), their layouts
  generated from every level's conquest layouts (hand-made ones in RON take their place),
  team deathmatch for mods' `gpm_tdm` layouts; modes in the menu, server config, rotation and
  browser
- Import of BF2 mods such as AIX 2 (levels, vehicles, weapons)
- Content verification: the server checks the client's content hashes before it lets a player
  in and sends what is missing or outdated; the client confirms downloads from unknown servers
- Optional accounts: a master server with accounts, tokens that game servers verify, stats,
  levels/ranks and a website; LAN play never needs it
- Settings: more graphics options, key bindings for everything, controller support
- UI polish
- Rudder authority tuning (needs someone to fly and compare with BF2)
