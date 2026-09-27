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

## Checking changes

- `cargo test -p bf2_formats -p bf2_import` for the format code.
- `bf2-import --bf2 "<install>" check` parses every mesh/collision mesh.
- The client can verify itself without a human. Scenarios (`scenarios/*.ron`, format in
  `crates/game_client/src/scenario.rs`) load a level once, wait until it is fully loaded
  and every shader compiled (usually 2-3 s), then script camera placement, input,
  render toggles, screenshots and frame time measurements:
  `client --scenario scenarios/viewmodel.ron` writes `target/scenarios/viewmodel/*.png`
  (and `report.txt` for `Measure` steps). Prefer one scenario with several screenshots
  over several launches. `client --screenshot out.png` is the one-shot shorthand.
- `--debug-walk` exercises prediction; the HUD shows RTT and prediction corrections.
- Test with dev builds (`target/debug/client.exe`); release builds take much longer.
- Networking: start `server --bots 8` then `client --connect 127.0.0.1`.
