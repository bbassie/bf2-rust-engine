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
| `game_auth` | client, servers, master | identity keys, signed account tokens, the master's API types, ranks (no engine) |
| `master_server` | an optional web server | server list, accounts, stats, ranks, quick join, web pages (no engine) |

The dedicated server binary uses an explicit headless plugin list (no window, no renderer).
The client links `game_server` too, so "host" and "singleplayer" are the same code path as a
dedicated server with a local player (replicon's "listen server" model: server logic runs
when `ClientState::Disconnected`).

### Networking

- [bevy_replicon](https://github.com/simgine/bevy_replicon) for server-authoritative
  replication, [renet](https://github.com/lucaspoffo/renet) (netcode) over UDP as transport.
  Port 16567 by default.
- The protocol (replicated components, messages) is registered in one place,
  `game_shared::protocol::ProtocolPlugin`, so client and server always agree; the join
  handshake checks the protocol hash.
- Fixed 60 Hz simulation tick; replication runs on the same tick.
- Nothing received is trusted: values are checked where they arrive (`game_shared::validate`:
  finite input angles and targets, clean unique player names, and names from the server that
  become file names only as plain folder or file names), and command-like messages (chat,
  admin logins, commander requests, content reports, browser queries, the remote console)
  go through the shared rate limits and login lockouts of `game_server::limits`.
- Server content: a server shares its mods (optionally the imported assets) over HTTP on TCP
  at the game port. Before connecting, clients download what they lack into a
  content-addressed cache and mount it over their own data for the session
  (`game_shared::content`, `game_server::content`, `game_client::content`; see
  [MODDING.md](MODDING.md#sharing-content-from-a-server)).

#### Joining: the server decides

Replicon runs with custom authorization: a client that connects gets no replication and no
player until the server lets it in (`game_shared::join`, `game_server::join`,
`game_client::join`). Every message of the handshake is independent of replication.

```
client                                        server
  connect (netcode)                ─────────▶  Joining: no replication, no player
  JoinRequest {protocol hash, nonce} ───────▶  another game version: kicked
                                   ◀─────────  JoinChallenge {identity proof, level,
                                               content check, accounts}
  checks the proof (the nonce signed with the server's key, the same key its
  content endpoint showed before downloading)
  AccountTicket {ticket}           ─────────▶  servers with accounts: checked offline
  ContentReport {digest}           ─────────▶  compared with its manifest
                                   ◀─────────  Accepted | SendHashes | ManifestChanged |
                                               Fetch {files}
  ContentReport {per-file hashes}  ─────────▶  (after SendHashes) which files differ
  downloads what Fetch names over HTTP, reports again
                                   ◀─────────  Accepted: replication starts, player created
```

- **Identity.** Every server has an Ed25519 key, made on its first start (`identity.key` in
  its data folder). It proves the key by signing the client's random nonce, together with its
  manifest id and name, so it can't pose as another server by copying its public key. The
  content endpoint does the same before downloads (`/content/identity`), which also ties the
  manifest to the key. Players see the key's fingerprint and confirm downloads from a key
  they haven't trusted before (`game_client::content`).
- **What is checked** (`game_shared::content::verify`): the level's required files, one per
  path (the highest priority layer's), from the server's manifest. Servers that share
  nothing (`Off`) check nothing; `Mods` checks the mods' files and leaves the imported BF2
  assets to each client's own import; `All` checks everything. The client hashes the file
  its game would load for each path and sends a digest; on a mismatch the per-file hashes,
  and the server names the files to download again (by hash, from the same endpoint or the
  download URL). This catches outdated, damaged and locally changed files, not a modified
  client, which can report anything.
- **Map changes** challenge every player again for the new level; nobody spawns
  (`ContentPending`) until their report matches. A repair during a map change ends with the
  client joining again, since the level is already loading.
- **Kicked, with the reason:** another version, no request within 30 s, a declined download
  (downloads off), no valid ticket on a ranked server, still different after 3 downloads, or
  no match within the sync timeout (10 minutes).

#### Accounts (optional)

A master server (`crates/master_server`) can hold accounts, stats and ranks; nothing needs
it. It signs short-lived **session tokens** (15 minutes, for its API) and **join tickets**
(5 minutes, for one game server: the audience is that server's key fingerprint) with its own
Ed25519 key (`game_auth::token`). Game servers check tickets **offline** with the master's
public key (configured, or fetched once and pinned), and accept each ticket once.

- **Unranked** (the default, every LAN and listen server): no accounts. With a master
  configured, verified players show their account name and rank (`AccountBadge`).
- **Ranked** (registered on the master, with an API key): an account is required; the
  server sends heartbeats and, at the end of every round, the stats of its verified players
  (`game_server::accounts`, from `stats`). The master only counts players who got a ticket
  for that server lately, once per round id, and turns score, time and wins into XP and
  ranks (`game_auth::ranks`).

The client keeps a refresh token (never the password) in `account.ron` in the config folder
and asks for a ticket after the server proved its identity (`game_client::account`).

**Master admins** (`master_server::admin`, `account`, `totp`): accounts have a role (player
or admin; the first admin comes from `master promote <name>`). Admin powers need
two-factor authentication (TOTP, RFC 6238) and a web session that passed it at login:
the web login has a second step (`/login/2fa`) for admins, while the game's API login stays
password-only and its tokens never reach the admin pages. TOTP secrets are sealed with a key
derived from `master.key`; recovery codes are hashed. The admin pages manage ranked servers,
accounts (bans, which are checked on every request, refresh and ticket; one-time passwords;
renames; roles) and runtime settings, and append to an audit log the database keeps
append-only. `master_server::db` has a schema version (`PRAGMA user_version`) and upgrades
older databases in place.

**Admin rights.** `ServerSettings.admin.admins` (config `admins`, CLI `--admin`, see
`docs/MODDING.md`) lists accounts that get `Admin` as soon as their ticket verifies on join
(`admin::grant_if_admin`, called from `join::receive_tickets` and
`create_client_player`, whichever order the player and its verified account show up in). On
a server that requires accounts, chat `/login` is refused instead of checking
`admin_password`; LAN, offline and unranked servers keep `/login <password>` exactly as
before (`chat::receive_chat`). RCON (`admin::rcon`) is unaffected.

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

### Hit registration

Bullets hit soldiers in BF2's per-bone capsules (`game_shared::hitzones`), posed on the
skeleton exactly as clients draw it (`game_shared::skeleton`): the server reads the same
`.glb` clips (the body's movement clips, the held weapon's upper-body set), picks the clips,
weights and times from each tick's state the way the client's animator does, and replays
the client's crossfades over its pose history. What makes the two agree: the movement clips
play at the replicated step phase (`SoldierMotion::stride`), idle loops on the server clock,
a reload's clip from its tick (`Inventory::reload_started`). Soldiers off their feet (seated,
climbing, swimming) keep their stance's capsules.

The server judges a shot against the tick the shooter saw (`InputFrame::view_tick`; the host
too, one or two ticks back), rewinding at most what his round trip allows. Clients predict
impacts against the same capsules posed for the moment they draw (`combat::DrawnTargets`):
a tracer flies the server's path (from the eye, drawn from the muzzle at first), stops in
the body without effect, and the blood comes from the server (`SoldierImpact`), where it
really hit. `BF2_SHOW_HITZONES=1` draws those capsules; `scenarios/combat/hitreg.ron` and
`hitreg_net.ron` measure what is seen against what the server counts
(`scripts/hitreg_report.py`), with target dummies (`game_server::dummy`).

### Kits, weapons and loadouts

A kit (`kits/<name>.ron`, `game_data::KitDesc`) lists its weapons in BF2's order and its
unlocks (BF2's `ItemContainer`s: `unlockLevel` 1 for BF2 1.5, 2 for Special Forces and the
booster packs, with the weapons they add and replace). The soldier's `Loadout` names his
weapons; `InputFrame::weapon` picks the one in hand, and the server switches and fires it
with the same `game_shared::weapons` rules the client predicts with.

- **Switching** (`combat::select_weapon`, `weapon_list`): what can be taken in hand
  (`WeaponDesc::selectable`: not worn gear, not the parachute, whose `hidden` flag comes from
  BF2's `SpawnObjectFireComp`) in slot order. Number keys pick a slot (again: the next weapon
  in it); the wheel steps through the list, skipping the knife and grenades unless
  `scroll_quick_weapons` is set. Each switch shows the weapon list at the bottom right
  (slot, BF2's selection icon as a white silhouette, name, grenades left), which fades after
  two seconds.
- **Quick actions** (`quick_actions`): the melee key (C, mouse 4) and the grenade key (G on
  foot; in a vehicle G is still countermeasures, and settings treat on-foot and in-vehicle
  keys as not conflicting) take the knife or the kit's grenade out, swing it or wind it up
  while held (cooking) and throw on release, then bring the previous weapon back. It is
  ordinary input (the weapon index and `FIRE`) plus `Buttons::QUICK`, with which
  `WeaponState::switch_with` shortens the switch (0.15 s out, 0.35 s back), so the server
  judges the swing and the throw like any other shot and prediction stays exact.
- **Loadouts** (`game_shared::arsenal`, `game_server::loadouts`, the client's `loadout`):
  every kit on disk (imported and the mods') adds its primary weapon (slot 3) and its unlocks'
  primaries to its class's pool (by `kind`), and its pistol to the sidearms. On the deploy
  screen a player picks, per class, a primary and a sidearm from those pools; the picks are
  kept in the settings and sent as `LoadoutRequest`. The server checks them against the pool
  and its `LoadoutRules` (replicated on the match: `arsenal` or BF2's kits only, weapons
  restricted to the team's factions, unlock levels needing an account rank), keeps the
  accepted ones in `LoadoutPicks` on the player and checks again at spawn. A picked rifle
  replaces the kit's primary with the launcher under it (a rifle and a launcher given together
  in a kit or unlock go together, e.g. `usrif_m203` and `usrgl_m203`); an assault kit that
  loses its launcher gets a hand grenade, as BF2's unlocks do. Gadgets never change. Pool
  weapons outside the level's own kits are lent to the `Armory` (`Armory::pool`) and not
  preloaded. Bots keep their kits. Server settings: `arsenal`, `faction_locked_weapons`,
  `unlock_ranks` (config) and `--classic-kits`, `--faction-locked-weapons`.

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
  vehicle (out of an aircraft high up, with its speed, under a parachute; the jump key also
  opens one when falling fast). The parachute (`soldier::parachute_glide`) catches the fall
  over a moment (30 m/s²), then glides where the soldier looks (turning at most 70°/s):
  8 m/s forward sinking 4.5 m/s hands off, W dives (13 m/s, 7 m/s), S brakes (3 m/s,
  3.2 m/s), A/D slip sideways; 3 m above the ground it flares (at most 2 m/s down) and he
  lands running with half the glide. No weapons meanwhile. It also ends in deep water, on a
  ladder or zipline, when he is critically wounded and when he gets into a vehicle (the
  server clears `SoldierMotion::parachute`; prediction steps the same rules). The canopy
  hangs where BF2's seat puts it: the seat (2.08 m below the canopy) at the hips of the
  `3p_parachute` pose. A seated soldier stays alive but `apply_inputs` skips it: its
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
    speed (the afterburner raises it), each wing lifts with its BF2 lift, clamped at the
    stall angle; plate and induced drag and a load limit. Piloted, a jet flies by wire, a
    gameplay layer for BF3/BF4-like handling (BF2's raw wing torques rolled the J-10 at
    20°/s at take-off speed and let it wander): the stick and rudder ask for rates (pitch
    48°/s, roll 170°/s, yaw 29°/s at the corner speed) that a controller holds, so centred it
    keeps its attitude, trimmed to fly where it points. The share of those rates depends on
    airspeed: best at the corner speed (70 % of the engines' BF2 top speed, 315 km/h for
    most jets; the HUD lights the airspeed in that band), mushy below, wider above, so the
    throttle and afterburner change the turn radius and hard turns bleed speed (induced
    drag). Wings lift 1.8× BF2's for the angle of attack (the path follows the nose); the fin
    weathervanes the nose into sideways airflow (coordinated banked turns and rudder yaw).
    Below 40 % of the corner speed or past the stall angle it stalls: the stick loses most
    authority, the nose drops towards the flight path, the HUD says STALL and the view
    shakes; with speed back it flies again. Unpiloted jets keep BF2's raw forces. Landing
    gear retracts above its BF2 height.
  - helicopters: the rotor spins up, the collective regulates the climb rate (holding
    altitude hands off), the cyclic tilts the lift within BF2's regulation angles at BF2's
    turn rates, fading out towards 75° nose down, 35° nose up and 55° of bank, and the body
    levels itself when let go (0.8/s pitch, 1.2/s roll). Piloted, the rates follow the
    stick and pedals BF3/BF4-quick (11-12/s: half the rate in about 70 ms, 90 % in about
    190 ms, stopped as quickly; BF2's 4/s took over half a second). Steeper than 35° nose
    down the collective lets go of the altitude by 60°: a rocket-run dive (its forward push
    no stronger than at 35°, so it's no faster way to cruise). In forward flight it turns
    into its bank (about the vertical) and the tail keeps it into the airflow, except while
    the pedals turn it: then the fuselage carries the flight path round after the nose (a
    pedal turn at 180 km/h follows the nose within about 13°), the pedals bank it up to 11°
    into the turn, and they turn it at 60 % of their hover rate by 50 m/s. Backwards and
    sideways it's slow (extra drag), and hands off at low speed the drift dies out. Near the
    ground the collective sinks slower (a held-down helicopter touches down at about 2 m/s),
    and a landed one leans on its skids until the pilot pulls up. The unit test
    `flight::heli_handling` flies the imported helicopters and prints their response times.
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
  the belt around them is skinned to them. Each write of a track material re-uploads its
  whole bindless slab, so only tracks in view are written, only once they've moved 1/512 of
  a texture repeat, and beyond 40 m every other frame.
  First person shows the interior mesh (BF2 geom 0), the seated soldier (its seat pose) and,
  for gunners, the weapon's BF2 HUD sight (reticle, periscope frame) from
  `menu/hud/hudsetup/vehicles`, laid out on BF2's 800x600 screen. The camera uses the seat's
  camera point or chases the vehicle (V). Gunners look where they aim at once, the turret
  and gun following at their BF2 speeds (a ring marks where the gun points meanwhile), and
  the aim stops at the turret's and gun's limits; joints are drawn between ticks. Pilots'
  chase camera trails the aircraft's rotation (helicopters' stiffly, 14/s), pulls back with
  speed and looks into the turn; the cockpit view turns a little into it too
  (`vehicles::pilot_camera`). A helicopter's chase camera takes half its bank and a third of
  its pitch near level flight, but follows a steeper dive fully (at 75° nose down it looks
  62° down). Pilots' free look (Alt) reaches 83° down and 80° up. The vehicle HUD (`vehicle_hud`, our own design) has a panel with
  the vehicle's hit points, who sits in which seat, speed and gear, where the turret points
  and the seat's guns (ammo, heat, lock) and countermeasures; pilots get flight instruments
  (banking horizon and pitch ladder, heading, airspeed, throttle and afterburner, altitude
  and climb rate, a flight path marker, stall and pull-up warnings) and, with bombs selected,
  a bomb marker where they would land if dropped now (BF3's CCIP-like dot: the fall from the
  muzzle with the aircraft's velocity, as `projectile::step` flies it, traced against the
  world; off screen it waits at the edge, so pushing the nose down brings it onto the
  target). BF2's armor effects show the damage state: smoke (and sparks) while the
  hit points are under their thresholds, the explosion and wreck fires at 0; the wreck burns
  down to -100 % over its 10 s and blows apart.
  The outside models' lower LODs (BF2 geom 1 LOD 1..) are rigged like the full model and
  drawn by distance with dithered cross-fades, and the vehicle fades out at BF2's cull
  distance for player control objects (see "Levels of detail").
- **Remote controlled vehicles** (`VehicleDesc::remote`): the commander's artillery pieces
  (BF2 `PlayerControlObject`s with an `RCArtillery` remote control object: `ars_d30`,
  `usart_lw155`) are imported as stationary vehicles without seats, and the UAV
  (`UAVVehicle uav_pred`) as an aircraft; `game_server::commander` works them (see
  "Commander"). Their wrecks stay until repaired (`repairable_wreck`, BF2's
  `armor.canBeDestroyed 0`): a wreck brought back above 0 hit points is a vehicle again.

Controls in vehicles: W/S throttle (jets: hands off holds 50 %, S idles and air brakes, and
slows a jump jet into hover; helicopters and hovering jets: collective), A/D steering, rudder
or tail rotor, mouse, arrow keys or the gamepad's right stick as the stick (pitch and roll;
up raises the nose, like looking up; the "invert jet/helicopter pitch" settings make all
three flight-stick style, BF2's default; settings saved before this swap their old arrow
keys once, `Settings::migrate`; the mouse's stick centres itself in 1/8 s, so the aircraft
stops turning soon after the mouse does; `heli_pedals_roll` ("Helicopter A/D: roll") swaps
helicopters to A/D roll and mouse X on the tail rotor, BF3/BF4's alternative), Alt
free look, Shift afterburner, Space wheel brakes, fire/aim buttons the seat's
primary/secondary guns, weapon keys the gun on a trigger, G countermeasures (flares, smoke),
V chase camera, F1..F8 seats, E enter/exit.

### Game modes

`MatchInfo::mode` names the mode (`game_data::modes`: `gpm_cq` conquest, `gpm_coop` co-op,
`gpm_rush` Rush, `gpm_breakthrough` Breakthrough, `gpm_tdm` team deathmatch; `rush`, `bt`
and the like work where modes are typed). Every mode plays in the same round
(`game_server::modes`): the layout's control points are spawned, the mode's setup adjusts
them and sets the tickets, its systems play the round until one ends it, and after a break
the next round (or the rotation's next map) starts. A mode is a unit of its own:

| part | where |
|---|---|
| id, name, layout data, layout generation | `game_data::modes` |
| what clients see | `game_shared::conquest` (flags, tickets, round, deployment: all modes) and `game_shared::modes` (`ModeState` on the match, charges, sectors, `Locked` flags, `ObjectiveEvent`s) |
| rules | a module in `game_server::modes` with a plugin whose systems run `in_mode(kind)`, and a `ServerMode` entry in `MODES` (round setup, bot strategy) |
| bots | `ServerMode::objectives` values the objectives for each team's commander (`ai::strategy`); squads, the AI commander and vehicles follow its orders as in conquest |
| HUD, maps, deploy screen, menu | `game_client::conquest_hud` (tickets, objectives, stage, progress, feed), `mode_hud` (charges in the world, HUD markers, map markers), `deploy`, `menu::levels` |

- **Conquest and co-op**: BF2's `gpm_cq` (see M2 in the roadmap); co-op keeps the humans on
  one team and balances the bots (`game_server::coop`).
- **Rush**: attackers against defenders, in stages of two charges ("M-COM stations"). An
  attacker arms a charge by holding the use key within 2.2 m for 4 s, a defender defuses an
  armed one in 6 s (progress drains when they let go), and an armed charge goes off after
  30 s (a 12 m blast of 300 damage). With both charges of a stage destroyed the front moves:
  the attackers' tickets are refilled and both sides spawn further on (control points only
  say where each side spawns: nothing is captured). Only the attackers have tickets, one per
  death; they win by destroying the last pair and lose when out of tickets, unless a charge
  is armed (overtime). In both staged modes nobody spawns at a flag with enemies within 30 m
  (`SpawnBlocked`): the defenders spawn at the stage's own flags only while the attackers
  aren't at them. Charges are drawn from a template (BF2's `xp1_generator` by default)
  with a beacon that blinks once armed.
- **Breakthrough**: sectors of conquest flags. Only the open sector's flags move (conquest's
  capture rules; the others are `Locked`), defenders can take back what they lost, and once
  the attackers hold all of a sector's flags it falls: they get their tickets back and the
  next sector opens, the defenders spawning at the flags they still hold. The attackers win
  by taking the last sector.
- **Team deathmatch**: conquest's tickets without the bleed, a ticket per death, flags only
  mark spawns.

BF2's levels have conquest and co-op layouts only. Rush and Breakthrough layouts are made
from the conquest ones when a level loads (`game_data::modes::generate`, on client and server
alike, and cached in the `LoadedLevel`): the side holding fewer flags attacks (Karkand: the US
from the gas station); the other flags are ordered by how far along the way from the
attackers' start to the defenders' base (or the farthest flag) they are, over the relative
neighbourhood graph of the flags; they are grouped into stages or sectors (one to three
flags, at most five stages); a stage's two charges go on either side of a lone flag, or beside
two of its flags about 80 m apart; and each stage lists who spawns where (attackers at their
start and every stage taken, defenders at the stage's flags, the ones behind and their bases). Once the
navigation grid is there the server moves generated charges onto walkable ground in their
flag's walkable region, with room around them and at ground level. A generated layout is
`based_on` its conquest layout, whose navigation grid and BF2 strategic areas it shares.
Layouts written by hand in `level.ron` or `levels/<name>/modes.ron` take the place of the
generated ones (see [MODDING.md](MODDING.md#game-mode-layouts)).

**Bots** play every mode through the same orders. In Rush the charges are strategic areas of
their own: attackers are sent to the charges to arm (both at once, spread by the commander's
crowding rule) and to guard armed ones; defenders guard both and every armed charge comes
first. Near a charge that needs them (attackers: one to arm; defenders: an armed one) bots
walk up to it and hold the use key, crouched, before fighting anyone not close. In
Breakthrough the commanders only value the open sector's flags: attackers take what they
don't hold and hold what is being taken back, defenders hold theirs and retake what they
lost. Orders in the staged modes are dropped once their objective is worth less than 40 % of
the best one, so squads react to an armed charge, except the only squad defending an
objective: defenders keep a squad on every flag of the open sector and on both charges of a
stage while they have squads enough, moving one over from where two or more were sent
(`ai::strategy::cover_defences`). The per-minute `ai objectives` log line has, per objective,
the soldiers there and the bots sent there.

### Bots

Bots are `Player`s whose `InputBuffer` is filled by a `BotBrain` instead of the network.
They therefore obey exactly the same movement rules and use the same weapons and vehicles
code. The roadmap has the BF2-style layers (strategic areas, squads, behaviours).

**Walking** (`game_server::nav`) follows paths on a layered walkability grid rasterized from
the level's collision (0.5 m cells, 0.75 m on the big layouts), with ladders as links and A*
on background tasks. The grid covers the layout's flags and spawns; on levels with combat
areas it only has cells inside the soldiers' area plus 20 m (`nav::area`; BF2's
`usedByPathfinding` area first, else the soldiers', else the land vehicles'), and is cropped
to it. The land vehicle grid keeps to the land vehicles' area and the water grid to the boats'
(each falling back to the main area), helicopters and jets turn back into theirs, and a
flag, spawn, vehicle spawner, strategic area or ladder outside keeps a circle and a corridor
back (logged). Big, intricate statics (the aircraft carriers) get a **detail patch**: a
0.33 m grid in the object's own frame, so its ramps, doors and catwalks run along the cells
whatever its heading. The level grid leaves the patch's rectangle out and portals join the two
along its edge; patch cells live in the same arrays, so paths, regions and every other query
cover both (`nav::patch`). Vehicles standing still are marked on the grid once a second and
paths go around them; a goal at a vehicle's door snaps to the nearest free cell
(`nav::obstacles`). Where bots get stuck again and again (three stuck events on a cell: a
tree's low branches, a railing or a bank the grid doesn't see) the cells just ahead of them
join that set (`nav::StuckCells`), so later paths go round them, and spots where drivers keep
getting stuck (a street too narrow) become obstacles for vehicle paths. A bot that keeps
getting stuck on the way to the same goal leaves it alone for 20 s. A bot knows which walkable
region it is in: that of the nearest cell it can see from its knees (on the coarse grid of a
big map the nearest cell may be behind a thin wall, in a closed room of a house), and its
paths, spots, cover and bounds start there (`NavGrid::find_path_from`). A swimmer only finds
its feet where the bottom is within wading depth (0.4 m) of the surface, so a bot swimming
without getting anywhere (against a hull or a quay) swims straight for the nearest shore
shallower than that, and another one if that fails too.

**Infantry tactics** (`bots::combat`, `ai::cover`, `ai::awareness`, `ai::squad`) sit on top
of the utility behaviours:

- *Cover*: in a firefight a bot rolls (difficulty, courage) whether it fights from cover. The
  search takes cells near an edge of the grid with nothing walkable right beside them towards
  the threat (the grid's cheap line of sight), scores them by distance, the way it is going
  and teammates' spots, and checks the best few with rays: hidden crouching but not standing
  is low cover (stand up to shoot), hidden standing needs a peek spot a step to the side past
  the corner. Rays are rationed per tick (`COVER_RAYS`). With the enemy in sight it only runs
  for cover a few steps away (unless pinned down or hurt), walking and shooting back on the
  way. From cover it is up shooting a few seconds (longer while it wins), down to reload, when
  hurt or when the fire gets close; up again, it looks for its enemy at once, still aimed about
  where he was. Attackers don't take cover on the flag they are taking, pick cover on the way
  to it, and move on from cover to cover towards it (also while trading shots, after a while);
  they shoot as they advance. With the enemy gone it watches a moment.
- *Suppression*: enemy fire whose aim passes within 4 m (the client's flyby crack radius)
  raises it; it sends the bot to cover or low (crouched, prone at range), tells it where the
  shooter is and shakes the aim and slows reactions a little (`SUPPRESSED_AIM`).
- *Grenades*: a live grenade lying close by, once seen for a moment, makes the bot run from
  it; at an enemy in sight it only throws one from cover (winding up in the open is time not
  shooting back).
- *Awareness*: a memory of contacts (seen, heard firing or running, spotted by the team or
  seen by a teammate, shooting at it) that ages and is forgotten; it points holders the way
  the enemy comes, gives grenades and suppressive fire their targets, and a known enemy
  coming back into view, or one the team spotted or saw lately, is shot at sooner. Bots call
  out enemies they see with the commo rose's spot, which marks them for the whole team, and
  pick the wounded among the enemies in sight first.
- *Squads* of bots work as two fire teams: on the last 120 m to their objective, or with
  enemies seen within 50 m, they **bound** (one team moves 25 m while the other holds low,
  covering, then the other team moves past it; 8 s at most per bound); pinned down (members
  fighting under fire for a while), team 0 lays down suppressive fire while team 1 flanks.
  Medics go to downed squad mates first and see a revive through (a teammate down within
  25 m, or a squad mate they can reach in time, even in a firefight unless the enemy is close
  or the fire too heavy); medics and support run their bags to squad mates who need them, and
  defenders take cover facing the way the enemy comes, the leader (whom the squad spawns on)
  behind the flag.
- *Difficulty* (`ai::skill::BotDifficulty`, `--bot-difficulty`, the host menu) sets the
  default skill and scales reaction time, aim error, tactics (how often bots take cover,
  flank, suppress, throw grenades behind cover, spot) and awareness (sight, hearing, memory):
  Easy 0.25 / 1.4 / 1.5 / 0.3 / 0.8, Normal 0.5 / 1 / 1 / 0.65 / 1, Hard 0.7 / 0.8 / 0.8 /
  0.85 / 1.15, Expert 0.9 / 0.65 / 0.65 / 1 / 1.3.
- `--bot-legacy-team 1|2|3` makes a team (3: both) play without these, and
  `--bot-team-difficulty 2:easy` a team at another difficulty, to compare; the
  per-minute `ai team` log line has the time spent in cover while engaged (a ray from the
  threat's eye to the body, sampled for both behaviours), cover runs, suppression, spots,
  bounds and pins, and the `bots:` lines idle bots by what they were doing and where bots
  and drivers got stuck. The `ai combat` and `ai combat2` lines (`ai::stats::CombatStats`,
  both behaviours) break firefights down: time with an enemy in sight and on the trigger,
  rounds, hits and accuracy by distance, time to the first shot, engaged time, hits made and
  taken and deaths by what the bot was doing (in the open, up or down in cover, running to
  cover, bounding, ...), deaths by cause (bullet, vehicle, explosive, artillery), time by
  state, time by distance to the objective, and revives and how they ended.
- Tuning (2026-09-30, A/B soaks against `--bot-legacy-team`, 64-player layouts, 32 bots):
  bounding everywhere near the fight made squads slow to the flags, running for cover under
  fire and ducking lost more than it saved, grenades found bots sitting in cover, and medics
  hardly ever finished a revive; see the statistics above for how to measure.

**Bots in vehicles** (`game_server::bots::vehicle`, `ai::vehicles`, `nav::vehicle`) use the
use button, the seat keys and ordinary `InputFrame`s like players:

- *Getting in* is a utility option like the others: squad leaders and lone bots take a
  transport, APC or tank near them when their objective is far (tanks when attacking), squad
  members get into their leader's vehicle, bots join a teammate's vehicle as gunners and man
  stationary weapons when enemies are about, and bots cut off from their objective (a carrier)
  take any boat or aircraft that gets them off. Seats they walk to are claimed, so they don't
  all run for one jeep; drivers wait for claimed riders. A vehicle's role (transport, APC,
  tank, AA, boat, transport or attack helicopter, jet, stationary) and its guns' kinds follow
  from its data.
- *Driving* follows a path on the vehicle grid with pure pursuit (steering for a point ahead,
  aiming at sharp corners rather than cutting them), slows for corners, the end of the path
  and teammates in the way, backs out of dead ends (a target inside the turning circle, no
  progress, stuck) and gives up after repeated failures. Transports stop short of the flag and
  everyone gets out; tanks and APCs hold an open spot by the flag (never indoors) and stop or
  slow down to fight. Boats sail the water grid to the shore nearest the objective.
- *Gunners* (and tank drivers) aim through their view with lead for the round's flight time
  and drop, main guns and missiles at vehicles and groups, machine guns at soldiers, and fire
  once the turret, which turns at its own speed, is on target; heat seekers wait for a lock.
- *Pilots*: helicopters climb out, cruise above the air map's obstacles, transports land
  their squad by the objective and fly back to where they took off for more passengers
  (parking there when nobody comes), attack helicopters circle the objective and fire; jets
  with a runway take off, climb and circle, diving on targets.
- *Getting out*: at the objective, when the vehicle is badly damaged (aircrews on the ground,
  or high enough up for their parachute), on its roof, stuck for good, or when the driver left
  (aircraft: again only on the ground or high enough). A transport helicopter's pilot, landed
  at the drop-off, tells everyone aboard to get out (`VehicleClaims::drop_off`), whatever their
  own objective, and waits up to 8 s for them before he flies back.

The **vehicle grid** is built like the infantry grid, but only from what actually blocks a
vehicle (terrain and BF2's vehicle-type collision, `GameLayer::VehicleGround`): small plants
that only carry soldier or projectile collision are left out, so vehicles path straight
through them instead of routing around bushes they'd just drive over. It uses vehicle limits
(1 m cells, 3 m head room, 0.5 m steps, 40° slopes); BF2's own `AIPathFinding/Vehicle.qtr` is
an undocumented runtime dump and Wake Island ships none. Per vehicle class (wheeled, tracked,
amphibious, boat) cells cost more on slopes, in water deeper than the wheels, near walls (the
clearance a vehicle's width needs), and less on the level's road meshes; other vehicles are
driven around. Connected areas per class let paths snap to goals they can reach. Boats use a
4 m water grid (depth from the heightmap, nothing solid at the surface), aircraft an air map of
the highest obstacle per 16 m cell (BF2's aerial height map).

### Commander

The commander (`game_shared::commander`, `game_server::commander`, the client's `commander`
module) orders squads and calls in the team's assets through `CommanderRequest`s; an AI
commander (`game_server::ai::commander`) sends the same requests with its bot player. The
assets follow BF2:

- **Artillery**: the layout's artillery pieces are vehicles spawned by their spawners and
  tagged `AssetVehicle` (kind, team, asset number). A strike gives each living piece of the
  team a fire mission: it turns onto the target at its BF2 joint speeds (60°/s) and fires its
  burst (`fire.burstSize` 5 at `roundsPerMinute` 30), each shell a projectile of the gun's own
  shell launched on the arc (BF2 gravity × 5) that lands where and when the strike planned it:
  the first after 5 s, then one every 2 s per gun, 0.4 s between guns, within the gun's
  `deviation.radius` 20 m of the target. More pieces fire more shells; a strike needs at least
  one living piece. Destroyed pieces stay wrecks until an engineer repairs them (about half a
  minute of wrench work) or they come back after 360 s (the spawners' `minSpawnDelay`).
- **UAV**: a `uav_pred` vehicle flies a 60 m circle 120 m above the target at 30 m/s (a
  kinematic body, banked into the turn) and marks the enemies around the target for the team
  every second for 60 s. It has BF2's 200 hit points: shot down, it falls, burns and the sweep
  ends. It needs the UAV trailer standing to be called.
- **Satellite scan**: the commander's maps (and only his) show every enemy, updated every
  second while the scan lasts (BF2: "your map will show the position of every enemy"); he
  spots them for the team by right-clicking them on the commander screen. An AI commander's
  scan spots them for its team directly. Needs the radar.
- **Supply drop**: a crate on a parachute (5 m/s) that heals, resupplies and repairs soldiers
  and vehicles of the team within 5 m (`healSpeed`, `refillAmmoSpeed`, `workOnVehicles`) from a
  shared stock of 500 points for up to 300 s.

The UAV trailer and the radar are destroyable objects numbered from `ASSET_INSTANCE_BASE`;
a destroyed asset (object or artillery wreck) is in `DestroyedStatics`, which the
commander screen, the maps and the bots' engineers read. Levels imported before the pieces
were vehicles keep them as objects, with shells landing on schedule.

### Voice chat

BF2's push-to-talk voice, done in a modern way (`game_shared::voice`, `game_server::voice`,
the client's `voice` module, and the engine-free `game_voice` crate):

- **Channels.** Squad (B): the talker's squad. Command (H): the commander and the team's squad
  leaders, who alone may talk on it; a commander outside any squad talks there with B too.
  Bots never talk. The rules are one pure function (`voice::hears`, unit-tested), applied by
  the **server**: the client only says which key it pressed.
- **Codec.** Opus, 48 kHz mono, 20 ms frames, 24 kbps VBR, VoIP mode with in-band FEC. The
  codec is libopus 1.3.1 translated to Rust (`unsafe-libopus`): the reference implementation
  without a C toolchain or CMake, so the client builds the same on Windows MSVC and Linux.
- **Network.** `VoicePacket` (client to server) and `VoiceRelay` (server to each listener,
  independent of replication, talker by `PlayerNetId`) each get a dedicated unreliable renet
  channel, registered last. The server drops empty or oversized frames (256 bytes), frames
  beyond `Rate::VOICE` (the shared limiter), and frames from bots, spectators, players who may
  not use the channel and players an admin muted (`mute`/`unmute`/`muted` admin commands;
  `VoiceMuted` replicates so the client stops sending). About 30 kbit/s per listener of a
  talker: a squad of six with one talking costs the server 150 kbit/s up.
- **Client.** The microphone (cpal, the device's own rate and format, resampled to 48 kHz) is
  opened only while push to talk is held (plus a 200 ms tail), voice activation is chosen, or
  the mic test runs; voice chat off captures nothing. Playback is an output stream of its
  own: each talker has a jitter buffer (60 ms to start, growing after late packets, FEC for a
  single loss, concealment otherwise) and a decoder, mixed at master times voice volume. The
  HUD lists who talks (ours first) with a channel badge; the scoreboard mutes teammates for
  the session (right-click frees the mouse). Settings > Audio has the devices, gain, volume,
  push to talk or voice activation (with its level) and a mic test that plays you back.
- **Testing.** `BF2_VOICE_TEST_INPUT=tone`, `tone:<Hz>` or a WAV file replaces the
  microphone; the log names every burst sent and heard, which `scenarios/voice/` checks with
  two clients against a dedicated server.

### Levels

`MatchInfo` (replicated) names the level. Client and server both load it from `imported/`:
heightfield collider, static objects (trimesh colliders from BF2's per-actor-type collision:
the soldier mesh where there is one, else vehicle, else projectile, for soldiers, bullets,
the camera, footsteps and the infantry nav grid; separately, only where BF2 gives the part
vehicle (hull) collision, a second collider on its own layer for vehicles, so small plants
with no vehicle collision don't stop or bounce them — see `game_shared::statics` and
`GameLayer::VehicleGround`). The client additionally builds terrain chunks (one per BF2
color-map patch), water, and loads the glTF meshes of static objects.

### Levels of detail

Statics, vehicles and soldiers use BF2's lower LODs and its cull distances, with the rules
found in `RendDX9.dll` and `BF2.exe` 1.5 (docs/formats/meshes.md §2.12, `bf2_import::lods`).
The client draws each LOD within its distance band (Bevy's `VisibilityRange`, dithered
cross-fades around each switch), multiplies the switch distances by the camera zoom and the
draw distances by its square root (so scopes keep full detail), and scales the draw distances
by the view distance setting (`render::statics`, `render::unit_lods`).

- Vehicles switch their 3P model at their `setSubGeometryLodDistance`s plus half the diagonal
  of its box (the M1A2 at 19.5, 34.5 and 99.5 m), their wrecks likewise, and fade out at
  `max(56 · r, 80)` m with `r` BF2's object radius (the M1A2 at 375 m, the HMMWV at 165 m);
  small parts with a model of their own (pintle guns, tail rotors) fade out on their own
  (40 m for a machine gun).
- Soldiers switch the body at 11.2 and 21.2 m (3148, 1540, 360 triangles), carried weapons fade
  out at 40 m, soldiers at 137 m. Soldiers nobody sees (culled or out of view, shadows
  included) aren't posed until they are seen again, and only move every fourth frame; unseen
  vehicles keep their part poses and unseen flags stop waving the same way.
- `BF2_STATIC_LODS=off` / `BF2_UNIT_LODS=off` draw full detail at any distance (for
  comparisons), `BF2_UNIT_LOD_STATS` logs the vehicle and soldier triangles drawn.

### Frame time

What a frame with 63 bots costs, and the rules that keep it low (see `render::perf_stats` and
`scenarios/perf/`):

- Nothing is written unless it changed (`set_if_neq`): every changed `Transform` moves the
  whole hierarchy under it (a parked vehicle is about seventy transforms to propagate and
  meshes to upload, a soldier about a hundred), and every changed BF2 material rewrites its
  bindless slab of up to 2048 materials. Sleeping vehicles get no transform easing.
- Each soldier plays its own copy of the team's animation graph with only the clips it is
  playing linked to the root (`soldiers::OwnGraph`): Bevy evaluates every linked node for
  every bone, and the team graph holds hundreds of clips.
- The main world's schedules run single-threaded (`main::single_threaded_schedules`, like the
  dedicated server): its systems are tiny, and handing each to a worker cost more than it saved
  while the render thread keeps the workers busy. `BF2_SCHEDULES=parallel` switches back.
- One camera draws everything, the first-person view model included (`render::viewmodel`: the
  model is shrunk towards the eye so it never clips into walls, and scaled for BF2's 60°
  first-person field of view): a second camera was a whole extra view, about 1.4 ms of the
  render thread and 0.5 ms of the main thread a frame.
- The player camera and its shadow cascades draw directly (`NoIndirectDrawing`): Bevy's
  GPU-driven indirect draws rebuild bin unpacking bind groups and indirect parameters for
  every batch of every view each frame, which costs more CPU here than the draw calls it
  saves (render thread 8.1 -> 7.2 ms on Karkand with 63 bots). `BF2_PERF_EXP=indirect` for
  comparisons. Most of the render thread's time is wgpu recording and submitting the draws
  (the time after each camera's schedule in `BF2_PERF_STATS`).
- Map markers that move every frame (the minimap's) move by their `UiTransform`, not
  `left`/`top`: a changed `Node` lays out its whole UI tree again.
- Mesh entities nobody sees (other LODs, culled by their `VisibilityRange`) cost little: 4,800
  more on Karkand added about 0.15 ms. What costs is what each view draws.
- Measuring: `client --scenario scenarios/perf/perf_karkand.ron --bots 63` (median and p95 per
  view in `report.txt`); `BF2_PERF_STATS=1` logs the main world's time a frame (by schedule),
  how long it waits for the render world and extracts, the render thread's time by render
  set and camera, mesh (by kind), draw, UI node, bone and body counts and material changes
  (`=full` also what moved);
  `--diagnostics` adds the GPU time of each pass to the report; a build with
  `--features game_server/profile` and `BF2_PROFILE_FRAMES=1` adds every system's time.

### Lighting

Everything is lit in real time from each level's `sky.con` colours (`render::environment`,
whose module docs explain the model): one sun (the moon at night) with cascaded shadows, a
directional sky light as an environment map, and BF2's baked sky visibility (never its baked
sun) as ambient occlusion on statics and terrain.

- BF2's colours keep their hue but only part of their saturation: warm tints much less than
  cool ones (Strike at Karkand is less sepia, Midnight Sun stays blue), night levels most of
  theirs.
- Lamps: `bf2-import` reads the level's `LightSource` objects (and gives day levels' street
  lamps a lamp at their head, for night versions) into `environment.lamps`; `render::lamps`
  draws them as Bevy point and spot lights at night and indoors on day levels, culled beyond
  220 m, with BF2's baked lamp light as brightness reference. Setting "Dynamic lamps": off,
  on, or on with shadows (the two nearest lamps).
- Night versions of day levels (setting `time_of_day`, or `BF2_LIGHT=night=1`) replace the
  level's light with the Special Forces night levels' and light its lamps.
- `BF2_LIGHT=key=value,...` overrides the tuning constants (see `render::environment`).

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
