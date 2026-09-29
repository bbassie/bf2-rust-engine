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
for a log line and `ForbidLog("ERROR ")` fails on one. A run exits with code 1 on a failure and
writes `result.txt` (`PASS` / `FAIL: reason`). `scripts/smoke.sh` runs a quick suite of them;
`scripts/sheet.py` and `scripts/compare.py` turn screenshot folders into one image to review.

| Folder | What the scenarios check |
|---|---|
| `abilities/` | Man-down and revive, medic, support and engineer gadgets |
| `ai/` | Bot strategy, navigation, bots in vehicles, carriers |
| `animation/` | Third- and first-person animation, view model, scopes |
| `audio/` | Sounds, checked through the log |
| `combat/` | Grenades, launchers, explosives, hit zones, lag compensation, destruction, effects, SF gadgets |
| `commander/` | Commander screen and assets, commo rose and spotting, squads |
| `conquest/` | Flags, capture, HUD, deploy screen, minimap and big map, co-op |
| `infra/` | Chat, admin, stats, server browser, content download and the server's content check (trust, repair, refusal, map change), accounts on ranked servers, soak client, the sample mod |
| `lighting/` | Per-map lighting checks and A/B runs, tonemapping, materials, lamps (`lamps_*`), night versions of day levels |
| `lod/` | Static, vehicle and soldier levels of detail |
| `menu/` | Main menu, hosting and joining, settings |
| `modes/` | Rush (arming a charge, HUD, maps, deploy screen), Breakthrough (sectors, locked flags), the level page's modes |
| `movement/` | Soldier movement, ladders, ropes, stamina, roadkill, carrier ladders |
| `terrain/` | Terrain, undergrowth, trees, water, horizon, view distance |
| `vehicles/` | Driving, flying, boats, seats, sights, damage, tracks, countermeasures |

Files ending in `_host`/`_remote` or `_net` go in pairs or against a dedicated server; their
headers say which command to start first.
