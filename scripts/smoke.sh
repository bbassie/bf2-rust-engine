#!/usr/bin/env bash
# Smoke suite: runs a few quick scenarios that together touch most of the game (menu flow,
# deploy and respawn, vehicles, combat, commander, night lighting) and the unit tests on the
# Linux machine at the same time, then prints one pass/fail table. Use it before committing a
# batch of changes; it takes about 3-4 minutes.
#
#   scripts/smoke.sh                 # everything
#   scripts/smoke.sh --no-remote     # skip the Linux tests
#   CLIENT_EXE=target-b/debug/client.exe scripts/smoke.sh
#
# A scenario fails if the client exits non-zero, panics, logs an ERROR, or writes fewer
# screenshots than its Screenshot steps. Output: target/smoke/<scenario>/ with the screenshots,
# log.txt and a contact sheet (_sheet.png, see scripts/sheet.py).
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

SCENARIOS=(
    scenarios/menu/menu_play.ron
    scenarios/conquest/conquest.ron
    scenarios/vehicles/vehicle2_interiors.ron
    scenarios/combat/projectile_specops.ron
    scenarios/commander/commander.ron
    scenarios/lighting/light_night_flight.ron
)

exe="${CLIENT_EXE:-}"
if [[ -z "$exe" ]]; then
    exe=target/debug/deps/client.exe
    [[ -f "$exe" ]] || exe=target/debug/client.exe
    [[ -f "$exe" ]] || exe=target/debug/client
fi
[[ -x "$exe" || -f "$exe" ]] || { echo "no client binary; build it first"; exit 2; }

remote_pid=""
if [[ "${1:-}" != "--no-remote" ]]; then
    mkdir -p target/smoke
    scripts/remote.sh cargo test -p bf2_formats -p bf2_import -p game_data -p game_shared -p game_server \
        > target/smoke/remote_tests.log 2>&1 &
    remote_pid=$!
fi

failed=0
printf '%-28s %-6s %6s  %s\n' scenario result time notes
for scenario in "${SCENARIOS[@]}"; do
    name=$(basename "$scenario" .ron)
    out="target/smoke/$name"
    rm -rf "$out"
    mkdir -p "$out"
    start=$(date +%s)
    "$exe" --scenario "$scenario" --out "$out" > "$out/log.txt" 2>&1
    code=$?
    secs=$(( $(date +%s) - start ))
    expected=$(grep -cE '^\s*Screenshot\(' "$scenario")
    shots=$(find "$out" -maxdepth 1 -name '*.png' | wc -l)
    notes=()
    (( code != 0 )) && notes+=("exit $code")
    grep -q 'panicked' "$out/log.txt" && notes+=("panic")
    errors=$(grep -c ' ERROR ' "$out/log.txt")
    (( errors > 0 )) && notes+=("$errors ERROR lines")
    (( shots < expected )) && notes+=("$shots/$expected screenshots")
    if (( ${#notes[@]} == 0 )); then
        result=ok
    else
        result=FAIL
        failed=1
    fi
    (( shots > 0 )) && python scripts/sheet.py "$out" > /dev/null 2>&1
    printf '%-28s %-6s %5ss  %s\n' "$name" "$result" "$secs" "${notes[*]:-}"
done

if [[ -n "$remote_pid" ]]; then
    if wait "$remote_pid" && ! grep -qE 'FAILED|panicked|error(\[|:)' target/smoke/remote_tests.log; then
        passed=$(grep -oE '[0-9]+ passed' target/smoke/remote_tests.log | awk '{s+=$1} END {print s}')
        printf '%-28s %-6s %6s  %s\n' linux_tests ok "" "$passed tests passed"
    else
        printf '%-28s %-6s %6s  %s\n' linux_tests FAIL "" "see target/smoke/remote_tests.log"
        failed=1
    fi
fi
echo "sheets: target/smoke/<scenario>/_sheet.png"
exit $failed
