#!/usr/bin/env bash
# Runs a command on the Linux build machine against this working tree, uncommitted edits
# included: tests, server builds and dedicated-server soaks run there without loading this PC.
#
#   scripts/remote.sh cargo test -p game_server
#   scripts/remote.sh cargo build -p game_server
#   scripts/remote.sh ./target/debug/server --level strike_at_karkand --bots 32 --soak 5 --port 29000
#
# Every tracked or untracked (not ignored) file is copied over first; the machine keeps its
# own `target/`, `imported/` (copy it with `scripts/remote.sh --sync-imported` after a
# re-import) and the BF2 install for the importer (`BF2_DIR`). One command runs at a time
# (a lock on the remote side); others wait. The client doesn't build there (no audio/input
# development packages).
#
# Environment: BF2_REMOTE (default bbassie@192.168.4.204), BF2_REMOTE_KEY
# (default ~/.ssh/bf2_build_ed25519), BF2_REMOTE_DIR (default bf2-rust-engine).
set -euo pipefail

host="${BF2_REMOTE:-bbassie@192.168.4.204}"
key="${BF2_REMOTE_KEY:-$HOME/.ssh/bf2_build_ed25519}"
dir="${BF2_REMOTE_DIR:-bf2-rust-engine}"
ssh_cmd=(ssh -i "$key" -o BatchMode=yes -o ConnectTimeout=10 "$host")

cd "$(git rev-parse --show-toplevel)"

if [[ "${1:-}" == "--sync-imported" ]]; then
    tar -cf - imported | "${ssh_cmd[@]}" "mkdir -p ~/$dir && cd ~/$dir && rm -rf imported && tar -xf -"
    echo "imported/ copied"
    exit 0
fi
if [[ $# -eq 0 ]]; then
    sed -n '2,20p' "$0"
    exit 1
fi

# Quote the command for the remote shell.
remote_cmd=$(printf '%q ' "$@")

git ls-files -co --exclude-standard -z | tar --null -T - -cf - |
    "${ssh_cmd[@]}" "mkdir -p ~/$dir && cd ~/$dir && flock ~/.bf2-remote.lock tar -xf -"

exec "${ssh_cmd[@]}" "cd ~/$dir && source ~/.cargo/env && export BF2_DIR=\"\$HOME/games/Battlefield 2\" && flock ~/.bf2-remote.lock $remote_cmd"
