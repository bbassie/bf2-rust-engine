#!/usr/bin/env bash
# Soak test: a dedicated server with bots for a while, with a client connecting part of the
# time, then a summary of the logs: panics, warnings and errors, the server's health per map
# (`server --soak`: entities, memory, frame times, idle bots), rounds and map changes, stuck
# bots and the clients' prediction corrections and frame times.
#
#   scripts/soak.sh [--minutes 10] [--port 27777] [--client-runs 2] [--client-gap 30]
#                   [--scenario scenarios/soak_client.ron] [--out DIR] [--above-normal]
#                   [-- server options]
#   scripts/soak.sh --summarize DIR     # summarize an earlier run again
#
# Examples (build first, per package: `rustup run stable cargo build -p game_server`, and
# `-p game_client` for the client runs):
#
#   # Karkand with 32 bots for 15 minutes; a client connects twice (so also reconnects).
#   scripts/soak.sh --minutes 15 -- --level strike_at_karkand --bots 32
#   # The whole rotation of scripts/soak_rotation.ron (Karkand, Dalian Plant, Gulf of Oman,
#   # Warlord, co-op Karkand, the sample mod), 10 minutes per map, a client on every map.
#   scripts/soak.sh --minutes 61 --client-runs 6 --client-gap 360 -- \
#       --config scripts/soak_rotation.ron --soak-rotate 10
#   # Server only.
#   scripts/soak.sh --minutes 30 --client-runs 0 -- --level dalian_plant --size 64 --bots 32
#
# The server reports every 30 s (`--soak-every`), quits after --minutes and `--soak-rotate N`
# moves to the next map of the rotation after N minutes on one. The client (one at a time)
# first connects after a minute, plays --scenario (about 4 minutes: running, fighting, the
# big map, scoreboard and commo rose, and prediction traces), then waits --client-gap
# seconds before connecting again. Admin commands can be sent meanwhile with
# `target/debug/server rcon` if the server has an admin password. --above-normal raises the
# server's priority (Windows), so that builds running meanwhile disturb the frame times less.
#
# Runs target/debug/deps/{server,client}.exe (they work while the user's game locks
# target/debug/client.exe), or $SERVER_EXE and $CLIENT_EXE: copies keep a long run safe
# from rebuilds. Everything goes to DIR (default target/soak/<time>): server.log,
# clientN.log, clientN/ (screenshots, report.txt) and summary.txt, which is also printed.
# The exit code is 1 if anything panicked.
set -u
cd "$(dirname "$0")/.."

minutes=10
port=27777
client_runs=2
client_gap=30
scenario=scenarios/soak_client.ron
out=""
summarize=0
above_normal=0
while [ $# -gt 0 ]; do
    case "$1" in
        --minutes) minutes=$2; shift 2 ;;
        --port) port=$2; shift 2 ;;
        --client-runs) client_runs=$2; shift 2 ;;
        --client-gap) client_gap=$2; shift 2 ;;
        --client) shift ;; # the default; kept for older command lines
        --scenario) scenario=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --summarize) out=$2; summarize=1; shift 2 ;;
        --above-normal) above_normal=1; shift ;;
        --) shift; break ;;
        *) echo "unknown option $1 (server options go after --)"; exit 2 ;;
    esac
done
out=${out:-target/soak/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out"
server_exe=${SERVER_EXE:-target/debug/deps/server.exe}
client_exe=${CLIENT_EXE:-target/debug/deps/client.exe}
[ -x "$server_exe" ] || server_exe=target/debug/server
[ -x "$client_exe" ] || client_exe=target/debug/client

strip() { sed 's/\x1b\[[0-9;]*m//g' "$@"; }

if [ "$summarize" = 1 ]; then
    server_status="not run here"
    client_runs=$(ls "$out" | grep -cE '^client[0-9]+\.log$')
else
echo "soak: $minutes min on port $port, $client_runs client runs, logs in $out"
"$server_exe" --port "$port" --soak "$minutes" "$@" > "$out/server.log" 2>&1 &
server_pid=$!
echo "$server_pid" > "$out/server.pid"
if [ "$above_normal" = 1 ] && [ -f "/proc/$server_pid/winpid" ]; then
    sleep 1
    powershell -NoProfile -Command "(Get-Process -Id $(cat "/proc/$server_pid/winpid")).PriorityClass = 'AboveNormal'"         > /dev/null 2>&1 || echo "soak: could not raise the server's priority"
fi

client_loop=""
if [ "$client_runs" -gt 0 ]; then
    (
        sleep 60
        for run in $(seq 1 "$client_runs"); do
            kill -0 "$server_pid" 2>/dev/null || break
            echo "$(date +%H:%M:%S) client $run connects" >> "$out/clients.txt"
            "$client_exe" --connect 127.0.0.1 --port "$port" --scenario "$scenario" \
                --out "$out/client$run" > "$out/client$run.log" 2>&1 &
            echo $! > "$out/client.pid"
            wait $!
            echo "$(date +%H:%M:%S) client $run exit $?" >> "$out/clients.txt"
            rm -f "$out/client.pid"
            sleep "$client_gap"
        done
    ) &
    client_loop=$!
fi

wait "$server_pid"
server_status=$?
if [ -n "$client_loop" ]; then
    kill "$client_loop" 2>/dev/null
    [ -f "$out/client.pid" ] && kill "$(cat "$out/client.pid")" 2>/dev/null
fi
fi

logs=("$out/server.log")
for log in "$out"/client*.log; do [ -f "$log" ] && logs+=("$log"); done
panics=$(strip "${logs[@]}" | grep -c "panicked")
{
echo "== server (exit $server_status)"
echo "panics: $panics"
strip "${logs[@]}" | grep -h "panicked" -A3 | head -20
echo "warnings and errors (count, message):"
for log in "${logs[@]}"; do
    strip "$log" | grep -E " (WARN|ERROR) " | grep -vE "soak: slow frame|gilrs" \
        | sed -E 's/^[^ ]+ +//; s/[0-9]+(\.[0-9]+)?/N/g' \
        | sort | uniq -c | sort -rn | head -15 | sed "s|^|  $(basename "$log"): |"
done
echo "slow server frames (over 100 ms, at most 5 logged per report): $(strip "$out/server.log" | grep -c "soak: slow frame")"
strip "$out/server.log" | grep "soak: slow frame" | sed -E 's/^.*soak: //' | head -5 | sed 's/^/  /'
echo "rounds and maps:"
strip "$out/server.log" | grep -E "changing map|round over|round started|loaded level|, next map" \
    | sed -E 's/^[^T]+T([0-9:]+)\.[0-9]+Z +[A-Z]+ [a-z_:]+: /  \1 /'
echo "per map (from the soak reports; frame times over all frames, idle = alive bots that moved under 2 m in a report):"
strip "$out/server.log" | grep -E "soak: [0-9.]+ s," | sed -E 's/^.*soak: //' | awk '
    function num(re, skip) { return match($0, re) ? substr($0, RSTART + skip, RLENGTH - skip) + 0 : 0 }
    function flush() {
        if (cur == "") return
        printf "  %-32s %5.1f min, entities peak %d, memory %d -> %d MB, frame %.2f ms avg %.0f ms max, %d of %d frames over 16.7 ms, %.1f ticks/s min, idle bots max %d avg %.1f\n",
            cur, (last - start + every) / 60, peak, m0, m1, fsum / (frames > 0 ? frames : 1), fmax, over, frames, tmin, imax, isum / (n > 0 ? n : 1)
    }
    {
        t = $1 + 0; map = $3 " " $4 " " $5; sub(/,$/, "", map)
        e = num("entities [0-9]+", 9); m = num("memory [0-9]+", 7); idle = num("idle bots [0-9]+", 10)
        ticks = num("[0-9.]+ ticks/s", 0)
        match($0, /frame [0-9.]+ ms avg, [0-9.]+ ms max, [0-9]+ of [0-9]+/); split(substr($0, RSTART, RLENGTH), f, " ")
        if (map != cur) {
            flush(); every = (n > 0 ? t - last : 30); cur = map; start = t; m0 = m
            peak = 0; fmax = 0; fsum = 0; frames = 0; over = 0; imax = 0; isum = 0; n = 0; tmin = 1000
        }
        last = t; m1 = m; n++
        if (e > peak) peak = e
        if (f[5] + 0 > fmax) fmax = f[5] + 0
        fsum += f[2] * f[10]; frames += f[10]; over += f[8]
        if (idle > imax) imax = idle
        isum += idle
        if (ticks < tmin) tmin = ticks
    }
    END { flush() }'
strip "$out/server.log" | grep "soak summary:" | sed -E 's/^.*soak summary/summary/; s/^/  /'
echo "stuck bots per minute (stuck events, goals out of reach):"
strip "$out/server.log" | grep -E "game_server::bots: bots:" \
    | sed -E 's/^[^T]+T([0-9:]+)\.[0-9]+Z.*bots: ([0-9]+) stuck events.* ([0-9]+) goals out of reach.*/\1 \2\/\3/' \
    | paste -sd" " | fold -s -w 150 | sed 's/^/  /'

for run in $(seq 1 "$client_runs"); do
    [ -f "$out/client$run.log" ] || continue
    report="$out/client$run/report.txt"
    shots=$(ls "$out/client$run" 2>/dev/null | grep -c '\.png$')
    echo "== client $run: $shots screenshots, $(grep -c "" "$out/client$run.log") log lines"
    [ -f "$report" ] || { echo "  no report (the scenario did not finish)"; continue; }
    grep -E "^[a-z_]+: avg" "$report" | sed 's/^/  /'
    awk '/ correction /{n++; c=$NF+0; if (c > 0.01) big++; if (c > max) max = c}
         END {printf "  prediction: %d traced frames, %d with a correction over 1 cm, largest %.3f m\n", n, big, max}' "$report"
done
} | tee "$out/summary.txt"
[ "$panics" = 0 ]
