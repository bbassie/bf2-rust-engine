"""Turns a scenario report's VehicleTrace lines into rates, per traced step.

    python scripts/flight_rates.py target/scenarios/vfeel/jet/report.txt

For every label: its duration, speed at the start and end (km/h), and the average and peak
turn (heading), pitch and roll rates in degrees per second, plus the altitude change.
"""

import re
import sys

LINE = re.compile(
    r"^(?P<label>.+?)\s+(?P<t>\d+\.\d+): at \((?P<x>[-\d.]+), (?P<y>[-\d.]+), (?P<z>[-\d.]+)\) (?P<speed>[-\d.]+) km/h, "
    r"(?P<alt>[-\d.]+) m up \(climb (?P<climb>[-\d.]+) m/s\), heading (?P<heading>[-\d.]+), pitch (?P<pitch>[-\d.]+), "
    r"roll (?P<roll>[-\d.]+)"
)


def wrap(a):
    return (a + 180.0) % 360.0 - 180.0


def main(path):
    groups = []
    for line in open(path, encoding="utf-8"):
        m = LINE.match(line.strip())
        if not m:
            continue
        sample = {k: (m[k] if k == "label" else float(m[k])) for k in LINE.groupindex}
        if not groups or groups[-1][0] != sample["label"] or sample["t"] < groups[-1][1][-1]["t"]:
            groups.append((sample["label"], []))
        groups[-1][1].append(sample)
    print(f"{'step':<18} {'s':>4} {'km/h':>11} {'turn deg/s':>13} {'pitch deg/s':>13} {'roll deg/s':>13} {'alt m':>7}")
    for label, samples in groups:
        # A step that starts by moving the vehicle (PlaceVehicle): from where it was put.
        if len(samples) > 2:
            a, b = samples[0], samples[1]
            if ((a["x"] - b["x"]) ** 2 + (a["y"] - b["y"]) ** 2 + (a["z"] - b["z"]) ** 2) ** 0.5 > 60.0:
                samples = samples[1:]
        if len(samples) < 2:
            continue
        rates = {"heading": [], "pitch": [], "roll": []}
        for a, b in zip(samples, samples[1:]):
            dt = b["t"] - a["t"]
            if dt <= 0:
                continue
            for key in rates:
                rates[key].append(wrap(b[key] - a[key]) / dt)
        duration = samples[-1]["t"] - samples[0]["t"]
        cells = []
        for key in ("heading", "pitch", "roll"):
            values = rates[key]
            total = sum(values) / len(values) if values else 0.0
            peak = max(values, key=abs) if values else 0.0
            cells.append(f"{total:6.1f}/{peak:6.1f}")
        speeds = f"{samples[0]['speed']:4.0f}->{samples[-1]['speed']:4.0f}"
        alt = samples[-1]["y"] - samples[0]["y"]
        print(f"{label:<18} {duration:4.1f} {speeds:>11} {cells[0]:>13} {cells[1]:>13} {cells[2]:>13} {alt:7.1f}")


if __name__ == "__main__":
    main(sys.argv[1])
