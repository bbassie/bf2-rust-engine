# Notes for working on this repo

BF2-style shooter in Rust + Bevy 0.19. Read `docs/ARCHITECTURE.md` first; format specs are in
`docs/formats/`.

## Hard rules

- Never commit or copy BF2 game files into the repo. Converted assets live in `imported/`
  (gitignored). Tests and tools read the user's install via `BF2_DIR`.
- The game only reads `game_data` formats (RON + glTF + DDS). BF2 parsing belongs in
  `bf2_formats`/`bf2_import`.
- Protocol registration (replicate/add_*_message) only in `game_shared::protocol`.

## Building (Windows, Git Bash)

- `cargo` in Git Bash resolves to rustup and fails; use `rustup run stable cargo ...`.
- Don't wrap builds in `timeout`: the first build of the client takes 20+ minutes.
- Build per package (`-p game_client`, `-p game_server`); a `--workspace` build unifies Bevy
  features and rebuilds Bevy.
- Iterate on compile errors with `rustup run stable cargo check -p <package>`: no codegen and
  no link, so it takes seconds where a build takes minutes. Build only to run something.
  Build only the packages you run (`game_client` already contains the server library).
- The target dir is shared by everyone working in this tree and cargo serializes builds
  ("Blocking waiting for file lock"). Batch edits so each build is worth it.
- `scripts/remote.sh <command>` runs a command on the Linux build machine (14 threads) against
  this working tree, uncommitted edits included: use it for `cargo test`, server/importer
  builds and dedicated-server soaks so they don't compete with client builds here. It has the
  imported content and the BF2 install (`BF2_DIR`); it can't build or run the client.
- Shaders: register them with `embedded_shader!` (not `embedded_asset!`). Dev builds then read
  the `.wgsl` source file at startup, so a shader edit needs only a restart, no rebuild.
- Changing `[profile.*]` settings, Bevy features or rustflags rebuilds every dependency
  (about 20 minutes for everyone); avoid it unless it's the point of the change.

## Checking changes

- `cargo test -p bf2_formats -p bf2_import` for the format code.
- `bf2-import --bf2 "<install>" check` parses every mesh/collision mesh.
- The client can verify itself without a human. Scenarios (`scenarios/<area>/*.ron`, index in
  `scenarios/README.md`, format in `crates/game_client/src/scenario.rs`) load a level once, wait until it is fully loaded
  and every shader compiled (usually 2-3 s), then script camera placement, input,
  render toggles, screenshots and frame time measurements:
  `client --scenario scenarios/animation/viewmodel.ron` writes `target/scenarios/viewmodel/*.png`
  (and `report.txt` for `Measure` steps). Prefer one scenario with several screenshots
  over several launches. `client --screenshot out.png` is the one-shot shorthand.
- Make scenarios checks, not just screenshots: `ExpectLog("text", seconds)` waits for a log line
  and `ForbidLog("text")` fails on one; a failed run exits with code 1 and writes
  `result.txt` (`PASS` / `FAIL: reason`).
- Review screenshots as one image: `python scripts/sheet.py <dir>` (contact sheet) and
  `python scripts/compare.py <before> <after>` (per-image change plus a before/after/diff sheet).
- `scripts/smoke.sh` runs a quick scenario suite plus the unit tests on the Linux machine
  (about 4 minutes); run it before handing back a batch of changes.
- Frame times: `client --scenario scenarios/perf/perf_karkand.ron --bots 63` (and
  `perf_archipelago.ron` for a big AIX map); see "Frame time" in docs/ARCHITECTURE.md for
  `BF2_PERF_STATS`, `--diagnostics` and per-system timings. Other agents' clients and builds
  skew timings: compare builds in alternating runs on a quiet machine.
- `--debug-walk` exercises prediction; the HUD shows RTT and prediction corrections.
- Test with dev builds (`target/debug/client.exe`); release builds take much longer.
- If the user is playing, `target/debug/client.exe` is locked and the build ends with
  "failed to remove file"; the compile still succeeded, so run `target/debug/deps/client.exe`.
  Never kill the user's game.
- Networking: start `server --bots 8` then `client --connect 127.0.0.1`.
