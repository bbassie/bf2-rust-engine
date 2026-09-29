"""Hit registration report from a `scenarios/combat/hitreg*.ron` run.

    python scripts/hitreg_report.py <client log> [<server log>]

Reads the `hitreg` lines (client logs with BF2_HITREG_LOG=1; on a listen server the server's
lines are in the client log too) and prints, per pose and body part:
- `geo`: of the frames checked, how often a ray through the point on the drawn body meets the
  hit zones as drawn and as the server poses them, and the same zone;
- `shots`: of the shots fired at it, how many the ray met on the drawn skeleton when fired
  ("seen"), how many the tracer showed on a soldier, how many the server counted, and how many
  the server and the drawn skeleton agree on (same zone, or both a miss).
"""

import re
import sys
from collections import defaultdict

SHOT = re.compile(r"hitreg shot (\d+): (\S+) part (\S+) seen (\S+) judged-here (\S+) aim-error (\S+) view_tick (\d+) clearance (\S+)")
TRACER = re.compile(r"hitreg tracer: (?:hit \S+ \(material (\d+)\)|nothing hit)")
FIRE = re.compile(r"hitreg fire: (\S+) (\S+) tick (\d+) view (\d+) rewind (\d+)")
VERDICT = re.compile(r"hitreg verdict: \S+ (\S+) (hit \S+ zone (\S+)|miss)")
GEO = re.compile(r"hitreg geo (\S+) (\S+): drawn (\d+)% judged (\d+)% same-zone (\d+)% stance (\d+)% stance-same (\d+)% clear (\d+)% n=(\d+) \[(.*)\]")
ERR = re.compile(r"hitreg err (.+) (\S+): mean (\S+) cm max (\S+) cm")
DUMMY = re.compile(r"dummy: (\S+) ")
HUMAN_BODY = {"24", "25", "77", "23"}


def lines(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            # Strip ANSI colours.
            yield re.sub(r"\x1b\[[0-9;]*m", "", line)


def main():
    client = sys.argv[1]
    server = sys.argv[2] if len(sys.argv) > 2 else client
    shots, tracers, geo, err = [], [], [], []
    for line in lines(client):
        if m := SHOT.search(line):
            shots.append(m.groups())
        elif m := TRACER.search(line):
            tracers.append(m.group(1))
        elif m := GEO.search(line):
            geo.append(m.groups())
        elif m := ERR.search(line):
            err.append(m.groups())
    dummies, fires, verdicts = set(), [], {}
    for line in lines(server):
        if m := DUMMY.search(line):
            dummies.add(m.group(1))
        elif m := FIRE.search(line):
            fires.append(m.groups())
        elif m := VERDICT.search(line):
            verdicts[m.group(1)] = m.group(3) or "miss"
    ours = [f for f in fires if f[0] not in dummies]
    rewinds = [int(f[4]) for f in ours]

    if geo:
        print("GEOMETRY (per frame; drawn / judged = ray meets the drawn / the server's zones)")
        print("(stance / st-same: the same with the stance's capsules, as before they followed the animations)")
        print(f"{'pose':<28} {'part':<10} {'drawn':>6} {'judged':>6} {'same':>6} {'stance':>6} {'st-same':>7} {'n':>5}  judged zones")
        for label, part, drawn, judged, same, stance, stance_same, clear, n, zones in geo:
            print(f"{label:<28} {part:<10} {drawn:>5}% {judged:>5}% {same:>5}% {stance:>5}% {stance_same:>6}% {n:>5}  {zones}")
        print()
    if err:
        print("CAPSULE ERROR (server's zone vs the same zone on the drawn skeleton)")
        rows = defaultdict(dict)
        zones = []
        for label, zone, mean, peak in err:
            rows[label][zone] = (float(mean), float(peak))
            if zone not in zones:
                zones.append(zone)
        print(f"{'pose':<28} " + " ".join(f"{z[:11]:>11}" for z in zones))
        for label, row in rows.items():
            print(f"{label:<28} " + " ".join(f"{row[z][0]:>5.1f}/{row[z][1]:<5.1f}" if z in row else f"{'':>11}" for z in zones))
        # Mean over all zones per pose.
        print()
        print(f"{'pose':<34} {'mean cm':>8} {'max cm':>8}")
        for label, row in rows.items():
            mean = sum(v[0] for v in row.values()) / len(row)
            peak = max(v[1] for v in row.values())
            print(f"{label:<34} {mean:>8.1f} {peak:>8.1f}")
        print("(mean/max cm)")
        print()
    if shots:
        print(f"SHOTS ({len(shots)} fired, {len(ours)} server fire lines, {len(tracers)} tracer lines)")
        table = defaultdict(lambda: defaultdict(int))
        for i, (index, label, part, seen, _judged, _aim, _tick, clearance) in enumerate(shots):
            server_zone = None
            if i < len(ours):
                server_zone = verdicts.get(ours[i][1])
            tracer = tracers[i] if i < len(tracers) else None
            row = table[(label, part)]
            row["n"] += 1
            row["seen"] += seen != "miss"
            row["tracer"] += tracer in HUMAN_BODY
            row["server"] += server_zone not in (None, "miss")
            row["agree"] += (server_zone or "miss") == seen
            row["unknown"] += server_zone is None
        print(f"{'pose':<28} {'part':<10} {'n':>3} {'seen':>5} {'tracer':>6} {'server':>6} {'agree':>6}")
        totals = defaultdict(lambda: defaultdict(int))
        for (label, part), row in sorted(table.items()):
            print(f"{label:<28} {part:<10} {row['n']:>3} {row['seen']:>5} {row['tracer']:>6} {row['server']:>6} {row['agree']:>6}")
            kind = "gun" if part in ("muzzle", "gunmid") else "body"
            for key, value in row.items():
                totals[kind][key] += value
        for kind, row in totals.items():
            n = max(row["n"], 1)
            print(
                f"TOTAL {kind:<4}: {row['n']} shots, seen {100 * row['seen'] / n:.0f}%, tracer on a soldier "
                f"{100 * row['tracer'] / n:.0f}%, server hit {100 * row['server'] / n:.0f}%, "
                f"server agrees with the drawn skeleton {100 * row['agree'] / n:.0f}%"
            )
        if rewinds:
            print(f"rewind ticks: min {min(rewinds)} max {max(rewinds)} mean {sum(rewinds) / len(rewinds):.1f}")


if __name__ == "__main__":
    main()
