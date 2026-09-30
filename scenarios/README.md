# Scenarios

Scripted client runs that check a feature without a human: load a level, wait until it is ready,
then move the camera, press keys, take screenshots and measure frame times. The format is
documented at the top of `crates/game_client/src/scenario.rs`, and the header comment of each
file says what it checks and how to run it.

```
client --scenario scenarios/animation/viewmodel.ron      # writes target/scenarios/viewmodel/
```

`cargo test -p game_client --bin client all_scenarios_parse` checks that every file here still
parses.

Scenarios can check things, not just take screenshots: `ExpectLog("round started", 10.0)` waits
for a log line (and doubles as a condition wait: it returns as soon as the line appears) and
`ForbidLog("ERROR ")` fails on one. A run exits with code 1 on a failure and writes
`result.txt` (`PASS` / `FAIL: reason`). `scripts/smoke.sh` runs a quick suite of them;
`scripts/sheet.py` and `scripts/compare.py` turn screenshot folders into one image to review.

A scenario's own local server (singleplayer or listen; not `--connect`) uses a fast deploy by
default (a fraction of a second, instead of the real game's 10 s), so `WaitSpawned`,
`WaitDeployScreen`, `WaitInVehicle` and `WaitBotsDeployed` return almost at once instead of the
fixed `Wait` a scenario used to guess for a respawn, a death, boarding a vehicle or bots
spawning in. A scenario that checks the deploy countdown itself sets `respawn_time: Some(10.0)`
to get the real timing back. See the top of `crates/game_client/src/scenario.rs` for all of it.

| Folder | What the scenarios check |
|---|---|
| `abilities/` | Man-down and revive, medic, support and engineer gadgets |
| `ai/` | Bot strategy (`capture_flow`: squads move on once their flag is taken), navigation, infantry tactics (`bots_tactics`: cover, bounding squads), bots in vehicles (`heli_dropoff`: transport helicopters land and everyone gets out), carriers |
| `animation/` | Third- and first-person animation, view model, scopes |
| `audio/` | Sounds, checked through the log |
| `combat/` | Grenades, launchers, explosives, hit zones and hit registration against target dummies (`hitreg`, `hitreg_net`), a despawn stress (`despawn_stress`: bots killed at close range over and over, which must not panic the client), lag compensation, destruction, effects, SF gadgets, the weapon list and the melee and grenade keys (`quick_weapons`) |
| `commander/` | Commander screen and assets, commo rose and spotting, squads, a bot commander handing the post to a player and taking it back (`commander_bot_handover`) |
| `conquest/` | Flags, capture, HUD, deploy screen (loadouts: `loadout_deploy`), minimap and big map, the map styles (`map_style`: classic/tactical at three minimap sizes, pinned objectives, order lines, eased vehicle zoom; `map_capture`: capture pie), co-op |
| `infra/` | Chat, admin, stats, server browser, content download and the server's content check (trust, repair, refusal, map change), accounts on ranked servers, soak client, the sample mod |
| `lighting/` | Per-map lighting checks and A/B runs, tonemapping, materials, lamps (`lamps_*`), night versions of day levels |
| `lod/` | Static, vehicle and soldier levels of detail |
| `menu/` | Main menu, hosting and joining, settings |
| `modes/` | Rush (arming a charge, HUD, maps, deploy screen), Breakthrough (sectors, locked flags), the level page's modes, the tactical top bar in both (`objective_bar_rush`, `objective_bar_breakthrough`), spawns following the front at every stage for both sides (`rush_spawns_karkand`, `rush_spawns_leviathan` with Leviathan's stage 2 charges up close, `breakthrough_spawns`) |
| `movement/` | Soldier movement, ladders, ropes (`grapple`: the grappling hook thrown, its rope swinging, settling and climbed, a miss, a sloped roof, frame times with five ropes), jumping and mantling (`jump_mantle`), stamina, roadkill, carrier ladders |
| `perf/` | Frame times with many bots (Karkand, AIX 2 Archipelago): at spawn, a fixed view, in the fight |
| `tactical_map/` | The generated tactical map (`tactical_map.rs`) of Karkand, Gulf of Oman, Dalian Plant and AIX 2 Archipelago, written to `target/scenarios/tactical_map/<level>.png`; a second run shows the cache hit in the log |
| `terrain/` | Terrain, undergrowth, trees, water, horizon, view distance |
| `vehicles/` | Driving, flying (jet mouse control: `jet_mouse`, `jet_mouse_f35`), boats, seats, sights, damage, tracks, countermeasures, the minimap's air zoom (`minimap_air`), players taking seats from bots (`seats_bots`, `seats_bots_heli`, `seats_bots_squad`) and handing the controls to a bot on the move (`seats_bots_takeover`, `seats_bots_takeover_land`) |
| `voice/` | Voice chat, two clients and a dedicated server: squad and command channels, enemies, muting (`BF2_VOICE_TEST_INPUT`) |

Files ending in `_host`/`_remote` or `_net` go in pairs or against a dedicated server; their
headers say which command to start first.
