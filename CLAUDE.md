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
- The client can verify itself without a human:
  `client --screenshot out.png --screenshot-delay 10` (add `--spectate` for an overview,
  `--debug-walk` to exercise prediction; the HUD shows RTT and prediction corrections).
- Networking: start `server --bots 8` then `client --connect 127.0.0.1`.
