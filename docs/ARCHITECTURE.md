# Architecture

## Goals

1. Play Battlefield 2's maps with BF2-style gameplay, multiplayer first, on a modern engine.
2. Keep EA's content out of the repository: it is imported from the player's own install.
3. Make it easy to extend: open data formats, plain Bevy ECS, no engine forks.

## The two halves: import time and run time

```
 BF2 install (zips, .con scripts, meshes)                 imported/ (local, never committed)
 ┌──────────────────────────────┐   bf2-import   ┌──────────────────────────────────────────┐
 │ mods/bf2/Objects_client.zip  │ ─────────────▶ │ levels/<name>/level.ron  heightmap.r16   │
 │ mods/bf2/Levels/X/server.zip │  bf2_formats   │ levels/<name>/colormaps/*.dds            │
 │ ...                          │                │ templates/<object>.ron                   │
 └──────────────────────────────┘                │ objects/**/meshes/*.glb, *.collision.glb │
                                                 │ objects/**/textures/*.dds                │
                                                 └──────────────────────────────────────────┘
                                                                     │  game_data (RON schema)
                                                                     ▼
                                                        game_shared / game_server / game_client
```

- **`bf2_formats`** understands BF2: the archive VFS (mounted like the game, first mount
  wins), the `.con` script interpreter (templates, instances, settings), visible meshes and
  collision meshes. It has no engine dependency.
- **`bf2_import`** interprets a level's scripts exactly as the game loads them (`Init.con`,
  `StaticObjects.con`, `GamePlayObjects.con` per mode/size), converts coordinates
  (BF2 is left-handed; we mirror Z), writes meshes as glTF and descriptions as RON.
- **`game_data`** is the schema of our own formats. Nothing BF2-specific: new content can be
  authored directly in glTF + RON, and that is the intended modding path.
- **The game never reads BF2 files.** It only reads `game_data` formats. Anything the game
  needs from BF2 must first go through the importer.

## Run time

### Crates

| crate | runs on | contents |
|---|---|---|
| `game_shared` | client + server | protocol registration, `SoldierMotion` + `step_soldier`, level + statics loading (physics) |
| `game_server` | dedicated server, and inside the client when hosting | connections, players, spawning, input application, bots |
| `game_client` | client | input, prediction, interpolation, camera, rendering, HUD |

The dedicated server binary uses an explicit headless plugin list (no window, no renderer).
The client links `game_server` too, so "host" and "singleplayer" are the same code path as a
dedicated server with a local player (replicon's "listen server" model: server logic runs
when `ClientState::Disconnected`).

### Networking

- [bevy_replicon](https://github.com/simgine/bevy_replicon) for server-authoritative
  replication, [renet](https://github.com/lucaspoffo/renet) (netcode) over UDP as transport.
  Port 16567 by default.
- The protocol (replicated components, messages) is registered in one place,
  `game_shared::protocol::ProtocolPlugin`, so client and server always agree; replicon
  checks a protocol hash on connect.
- Fixed 60 Hz simulation tick; replication runs on the same tick.
- Server content: a server shares its mods (optionally the imported assets) over HTTP on TCP
  at the game port. Before connecting, clients download what they lack into a
  content-addressed cache and mount it over their own data for the session
  (`game_shared::content`, `game_server::content`, `game_client::content`; see
  [MODDING.md](MODDING.md#sharing-content-from-a-server)).

### Player vs soldier

As in BF2, a **`Player`** (name, team, score) outlives the **`Soldier`** bodies it controls.
`ControlledBy(player)` links them. Players without a soldier respawn after a timer.

### Input, prediction, reconciliation

Every soldier, human or bot, is driven by `InputFrame`s (movement, view angles, buttons,
sequence number):

1. The client samples input once per fixed tick, keeps a history, and sends the last few
   frames redundantly (unreliable channel).
2. The server queues frames per player and applies one per tick with `step_soldier`, then
   replicates `SoldierMotion` and `InputAck` (the last applied sequence number).
3. The client runs the same `step_soldier` for its own soldier immediately (prediction).
   When a server state arrives it rewinds to it, replays unacknowledged inputs, and blends
   out any difference visually.
4. Other soldiers are shown ~100 ms in the past, interpolated between received states.

`step_soldier` is a kinematic move-and-slide (avian3d) against the world collision, so it
is deterministic enough that corrections are normally exactly zero.

Rendering never touches simulated entities: soldier visuals are separate entities placed
from `SoldierRender` each frame, so smoothing never moves hitboxes.

### Vehicles

`bf2-import` turns every vehicle a level's spawners use into `vehicles/<name>.ron`
(`game_data::VehicleDesc`): a part tree (hull, turret, barrel, wheels, ...) with rest
placements and meshes, joints (which input turns which axis within which limits), wheels
(radius measured from the mesh, BF2 spring strength/damping), seats (where the occupant sits,
looks from, gets out and which pose it holds), entry points, guns and physics values in our
own units (mass, top speed, drive and brake force, grip). Aircraft, helicopters and boats add
wings (BF2 `Wing` lift and flap lift at their lift points, which input moves them), thrusters
(engine push and water jets), a rotor, floaters (`FloatingBundle`), landing gear, afterburner
and aerodynamics (drag, stall angle, load limit, angular damping). The `category` (land, air,
helicopter, sea, stationary) comes from the engine type.

- **Server** (`game_server::vehicles`): spawners create vehicles for the team holding their
  control point and respawn them when they are gone or abandoned. The use button near an
  entry point takes the first free seat, F1..F8 change seats, use again gets out beside the
  vehicle (out of an aircraft high up, with its speed and under a parachute that sinks at
  5 m/s, glides where the keys steer and packs away on landing; the jump key also opens one
  when falling fast). A seated soldier stays alive but `apply_inputs` skips it: its
  `InputFrame` goes to the vehicle's `SeatInputs`, and it is carried along at its seat every
  tick. Guns fire from
  their seat's triggers (weapon keys pick among the guns of one trigger) with BF2 overheat;
  wire/TV guided missiles follow the gunner's aim, heat seekers lock on piloted aircraft.
  Direct hits take the damage table's factor for the armour face they strike (BF2's
  per-face collision materials: tank front, sides, rear and top, tracks, glass;
  `vehicles/<name>.armor.glb`); penetrable faces let the shot through to the armour behind.
  Crashes (speed along the contact) and deep water damage the vehicle.
- **Simulation** (`game_shared::vehicle` and `game_shared::flight`): each vehicle is an avian
  dynamic body (compound of convex hulls of the hull and turret collision; stationary weapons
  are static). `step_vehicle` computes its push for one tick from the driver's inputs:
  - land: per wheel a raycast spring holds it up; tyres cancel sideways sliding up to their
    grip (less once sliding, BF2's dynamic friction) and push with the engine or brake.
    Wheeled vehicles drive through BF2's automatic gearbox (`c_ETNewCar2`: gear ratios,
    shift points, no pull while changing gear, a rev limiter, engine braking; forces from
    `setTorque` × `setDifferential` × ratio / wheel radius, top speed from BF2's AI);
    tracked vehicles hold both tracks to a commanded speed and turn rate (skid steering).
  - jets: throttle spools (hands off holds cruise, parked idles), thrust fades towards top
    speed (the afterburner raises it), each wing lifts with its BF2 lift plus flap lift times
    its control surface deflection and speed squared, clamped at the stall angle; plate and
    induced drag, a load limit, an angle of attack limiter and per-axis angular damping keep
    it flyable. Landing gear retracts above its BF2 height.
  - helicopters: the rotor spins up, the collective regulates the climb rate (holding
    altitude hands off), the cyclic tilts the lift within BF2's regulation angles and the
    body levels itself when let go; turn rates follow BF2's engine values. Near the ground
    the collective sinks slower (a held-down helicopter touches down at about 2 m/s), and a
    landed one leans on its skids until the pilot pulls up.
  - jump jets (the F-35B): below 35 m/s, S swings them into hover (also parked on a deck):
    the lift fan (BF2's `c_ETHelicopter` engine on the jet) carries them like a gentle
    helicopter, W/S climb and sink, the stick tilts them to drift; above 50 m/s or with the
    afterburner they fly as jets again.
  - boats and amphibious vehicles: floaters lift as columns below the level's water height,
    water jets push up to their top speed, water drags.

  Turrets and barrels turn towards the gunner's aim (the world direction of their view) at
  their BF2 speeds within their limits; wings, rotors and gear animate from the same joints.
- **Replication**: `Vehicle` (template name), `VehicleMotion` (pose, velocity and the last
  applied driver input), `VehicleState` (joint angles, suspension, engine, afterburner),
  `VehicleWeapons` (rounds, reload, heat, lock per gun) and `Seated` (on the soldier).
- **Prediction** (`game_client::vehicle_prediction`): the driver runs the same `step_vehicle`
  and `integrate` for his vehicle, keeps the state per input, rewinds to each `VehicleMotion`
  and replays the unacknowledged inputs, like soldiers. Over a local dedicated server a
  driver sees steering 17-32 ms and throttle 50-60 ms after the key (200-260 ms interpolated),
  unchanged with 100 ms added latency (270-335 ms interpolated). Other vehicles are shown
  100 ms in the past, interpolated. `--no-vehicle-prediction` turns it off.
- **Client**: vehicle meshes are glTF skins with one joint per part (triangles spanning parts,
  like track belts, bend with them); parts are posed from the replicated joints and wheels.
  Tracks animate like BF2's UV matrices: the importer splits the faces each matrix moves into
  materials of their own (with the scroll direction and hub centres in their extras), and
  each vehicle scrolls its belts and rim treads and turns its hub textures and sprockets by
  the distance each track ran; tank road wheels (`rotateUV`) don't turn their geometry, as
  the belt around them is skinned to them.
  First person shows the interior mesh (BF2 geom 0), the seated soldier (its seat pose) and,
  for gunners, the weapon's BF2 HUD sight (reticle, periscope frame) from
  `menu/hud/hudsetup/vehicles`, laid out on BF2's 800x600 screen. The camera uses the seat's
  camera point (gunners' views turn with their turret) or chases the vehicle (V); pilots get
  a stiff chase camera. The vehicle HUD (`vehicle_hud`, our own design) has a panel with
  the vehicle's hit points, who sits in which seat, speed and gear, where the turret points
  and the seat's guns (ammo, heat, lock) and countermeasures; pilots get flight instruments
  (banking horizon and pitch ladder, heading, airspeed, throttle and afterburner, altitude
  and climb rate, stall and pull-up warnings). BF2's armor effects show the damage state: smoke (and sparks) while the
  hit points are under their thresholds, the explosion and wreck fires at 0; the wreck burns
  down to -100 % over its 10 s and blows apart.

Controls in vehicles: W/S throttle (jets: hands off holds 50 %, S idles and air brakes, and
slows a jump jet into hover; helicopters and hovering jets: collective), A/D steering, rudder
or tail rotor, mouse or arrow keys as the stick (pitch and roll; the down arrow pulls up), Alt
free look, Shift afterburner, Space wheel brakes, fire/aim buttons the seat's
primary/secondary guns, weapon keys the gun on a trigger, G countermeasures (flares, smoke),
V chase camera, F1..F8 seats, E enter/exit.

### Bots

Bots are `Player`s whose `InputBuffer` is filled by a `BotBrain` instead of the network.
They therefore obey exactly the same movement rules and will later use the same weapons and
vehicles code. The roadmap has the BF2-style layers (strategic areas, squads, behaviours).

### Levels

`MatchInfo` (replicated) names the level. Client and server both load it from `imported/`:
heightfield collider, static objects (trimesh colliders from the BF2 soldier collision
meshes). The client additionally builds terrain chunks (one per BF2 color-map patch),
water, and loads the glTF meshes of static objects.

## Coordinate conventions

Engine space: meters, right-handed, +Y up, -Z forward (north), +X east.
BF2 → engine: `(x, y, z) → (x, y, -z)`, rotations
`yaw/pitch/roll → Ry(-yaw) · Rx(-pitch) · Rz(roll)`, triangle winding reversed.
See `bf2_import::coords` and [formats/levels-terrain-scripts.md](formats/levels-terrain-scripts.md).

## Reference material

- [formats/meshes.md](formats/meshes.md): visible/collision meshes, skeletons, animations
- [formats/levels-terrain-scripts.md](formats/levels-terrain-scripts.md): VFS, `.con`, terrain, placement
- [formats/gameplay-data.md](formats/gameplay-data.md): object templates, vehicles, weapons, damage,
  game rules, AI, and what needs reverse engineering
- `tools/reference/*.py`: the tested Python reference parsers these specs were verified with
  (set `BF2_DIR` to your install)
