#!/usr/bin/env bash
# Soak test: a dedicated server with bots for a while, optionally with a client connected
# part of the time (scenarios/soak_client.ron, twice, so it also reconnects), then a summary
# of the logs: panics, warnings and errors, the server's health over time (`server --soak`),
# rounds and map changes, stuck bots and the client's prediction corrections.
#
#   scripts/soak.sh [--minutes 10] [--port 27777] [--client] [--out DIR] [-- server options]
#   scripts/soak.sh --minutes 15 --client -- --level strike_at_karkand --bots 32
#   scripts/soak.sh --minutes 40 -- --config soak.ron --ticket-ratio 20   # a map rotation
#
# Runs target/debug/deps/{server,client}.exe (build them first, per package). Logs go to
# DIR (default target/soak/<time>). The exit code is 1 if anything panicked.
set -u
cd "$(dirname "$0")/.."

minutes=10
port=27777
client=0
out=""
while [ $# -gt 0 ]; do
    case "$1" in
        --minutes) minutes=$2; shift 2 ;;
        --port) port=$2; shift 2 ;;
        --client) client=1; shift ;;
        --out) out=$2; shift 2 ;;
        --) shift; break ;;
        *) echo "unknown option $1 (server options go after --)"; exit 2 ;;
    esac
done
out=${out:-target/soak/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out"
server_exe=target/debug/deps/server.exe
client_exe=target/debug/deps/client.exe
[ -x "$server_exe" ] || server_exe=target/debug/server
[ -x "$client_exe" ] || client_exe=target/debug/client

strip() { sed 's/\x1b\[[0-9;]*m//g' "$@"; }

echo "soak: $minutes min on port $port, logs in $out"
"$server_exe" --port "$port" --soak "$minutes" "$@" > "$out/server.log" 2>&1 &
server_pid=$!

if [ "$client" = 1 ]; then
    # Connect after a minute, play the scenario (about 4 minutes), wait, and do it again.
    (
        sleep 60
        for run in 1 2; do
            kill -0 "$server_pid" 2>/dev/null || break
            "$client_exe" --connect 127.0.0.1 --port "$port" --scenario scenarios/soak_client.ron \
                --out "$out/client$run" > "$out/client$run.log" 2>&1 &
            echo $! > "$out/client.pid"
            wait $!
            sleep 30
        done
    ) &
    client_loop=$!
fi

wait "$server_pid"
server_status=$?
if [ "$client" = 1 ]; then
    kill "$client_loop" 2>/dev/null
    [ -f "$out/client.pid" ] && kill "$(cat "$out/client.pid")" 2>/dev/null
fi

echo
echo "== server (exit $server_status)"
logs=("$out/server.log")
[ "$client" = 1 ] && logs+=("$out"/client*.log)
panics=$(strip "${logs[@]}" 2>/dev/null | grep -c "panicked" || true)
echo "panics: $panics"
strip "${logs[@]}" 2>/dev/null | grep "panicked" -A3 | head -20
echo "warnings and errors (count, message):"
for log in "${logs[@]}"; do
    [ -f "$log" ] || continue
    strip "$log" | grep -E " (WARN|ERROR) " | sed -E 's/^[^ ]+ +//; s/[0-9]+(\.[0-9]+)?/N/g' \
        | sort | uniq -c | sort -rn | head -15 | sed "s|^|  $(basename "$log"): |"
done
echo "rounds and maps:"
strip "$out/server.log" | grep -E "changing map|round over|round started|loaded level" | sed -E 's/^([^ ]+) .*(conquest|rotation|level): /\1 /' | cut -c12-19,28-
echo "stuck bots per minute:"
strip "$out/server.log" | grep -oE "bots: [0-9]+ stuck events|[0-9]+ goals out of reach" | paste -sd' ' | fold -w 160
echo "health:"
strip "$out/server.log" | grep -E "soak( summary)?:" | sed -E 's/^.*soak/soak/' | awk 'NR % 4 == 1 || /summary/'

if [ "$client" = 1 ]; then
    for run in 1 2; do
        report="$out/client$run/report.txt"
        [ -f "$report" ] || { echo "== client $run: no report"; continue; }
        echo "== client $run"
        grep -E "^frame" "$report"
        awk '/ correction /{n++; c=$NF+0; if (c > 0.01) big++; if (c > max) max = c}
             END {printf "prediction: %d traced frames, %d corrections over 1 cm, largest %.3f m\n", n, big, max}' "$report"
    done
fi
[ "$panics" = 0 ]
