"""Generates mods/sample: a small custom level (Sample Valley) built only from our own
formats, with its own meshes, collision meshes and textures, plus a new weapon and kit made
from imported ones by patch files. See docs/MODDING.md.

    python tools/make_sample_mod.py

Everything is written from scratch here (no BF2 data), so the output can be committed. The
level's soldiers, kits and weapons refer to imported ones by name; without an import the
game falls back to its test kit.
"""

import json
import math
import os
import struct
import zlib

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "mods", "sample")
LEVEL = "sample_valley"

# Terrain: 257 x 257 samples, 2 m apart: 512 m square around the origin.
RES = 257
SPACING = 2.0
ORIGIN = (-256.0, 0.0, -256.0)
HEIGHT_SCALE = 0.001  # meters per unit: up to 65.5 m
WATER_HEIGHT = 3.0


def write(path, data):
    path = os.path.join(ROOT, path)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    mode = "wb" if isinstance(data, (bytes, bytearray)) else "w"
    with open(path, mode, **({} if mode == "wb" else {"newline": "\n", "encoding": "utf-8"})) as f:
        f.write(data)


# ---------------------------------------------------------------------------------------
# Terrain


def smoothstep(e0, e1, x):
    t = min(max((x - e0) / (e1 - e0), 0.0), 1.0)
    return t * t * (3 - 2 * t)


def terrain_height(x, z):
    """Ground height in meters at world (x, z): a valley running north-south between
    hills, flat bases at both ends and a lake in the north-east."""
    side = abs(x) / 256.0
    h = 7.0 + 30.0 * side ** 1.6
    h += 2.5 * math.sin(x * 0.031) * math.cos(z * 0.027) + 1.2 * math.sin(x * 0.083 + 1.7) * math.sin(z * 0.071)
    # Bases: flat ground around (0, -200) and (0, 200).
    for bz in (-200.0, 200.0):
        d = math.hypot(x, z - bz)
        h = h + (9.0 - h) * (1.0 - smoothstep(35.0, 70.0, d))
    # West hill for a control point.
    h += 12.0 * (1.0 - smoothstep(0.0, 60.0, math.hypot(x + 120.0, z - 40.0)))
    # Lake.
    h -= 9.0 * (1.0 - smoothstep(20.0, 55.0, math.hypot(x - 110.0, z + 60.0)))
    return max(h, 0.0)


def heightmap():
    samples = []
    for zi in range(RES):
        for xi in range(RES):
            x = ORIGIN[0] + xi * SPACING
            z = ORIGIN[2] + zi * SPACING
            samples.append(min(int(round(terrain_height(x, z) / HEIGHT_SCALE)), 65535))
    return samples


def ground(samples, x, z):
    """Height like the engine's `Heightmap::height_at`: cells split along (x,z)-(x+1,z+1)."""
    fx = min(max((x - ORIGIN[0]) / SPACING, 0.0), RES - 1.0)
    fz = min(max((z - ORIGIN[2]) / SPACING, 0.0), RES - 1.0)
    x0, z0 = min(int(fx), RES - 2), min(int(fz), RES - 2)
    u, v = fx - x0, fz - z0
    s = lambda xi, zi: samples[zi * RES + xi] * HEIGHT_SCALE
    h00, h10, h01, h11 = s(x0, z0), s(x0 + 1, z0), s(x0, z0 + 1), s(x0 + 1, z0 + 1)
    if u >= v:
        return ORIGIN[1] + h00 + (h10 - h00) * u + (h11 - h10) * v
    return ORIGIN[1] + h00 + (h01 - h00) * v + (h11 - h01) * u


# ---------------------------------------------------------------------------------------
# Images


def png(width, height, pixel):
    """RGBA PNG from `pixel(x, y) -> (r, g, b, a)`."""
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            rows.extend(bytes(max(0, min(255, int(c))) for c in pixel(x, y)))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b"")


def noise(x, y, seed):
    n = (x * 374761393 + y * 668265263 + seed * 144269504) & 0xFFFFFFFF
    n = ((n ^ (n >> 13)) * 1274126177) & 0xFFFFFFFF
    return ((n ^ (n >> 16)) & 0xFF) / 255.0


def planks(x, y):
    board = (y // 16) % 2
    grain = 0.85 + 0.15 * math.sin(x * 0.35 + board * 2.0 + math.sin(y * 0.5) * 1.5)
    edge = 0.55 if y % 16 in (0, 15) or x % 64 in (0, 63) else 1.0
    base = (150, 108, 62) if board else (140, 100, 57)
    shade = grain * edge * (0.92 + 0.08 * noise(x, y, 1))
    return (base[0] * shade, base[1] * shade, base[2] * shade, 255)


def concrete(x, y):
    shade = 0.8 + 0.15 * noise(x // 2, y // 2, 2) + 0.05 * noise(x, y, 3)
    seam = 0.7 if y % 64 in (0, 1) else 1.0
    return (165 * shade * seam, 162 * shade * seam, 155 * shade * seam, 255)


def metal(x, y):
    stripe = 0.9 + 0.1 * math.sin(y * 0.8)
    rust = noise(x // 4, y // 4, 4)
    r, g, b = (95, 105, 98) if rust < 0.8 else (120, 80, 50)
    shade = stripe * (0.9 + 0.1 * noise(x, y, 5))
    return (r * shade, g * shade, b * shade, 255)


# ---------------------------------------------------------------------------------------
# glTF


def box(min_c, max_c, uv_scale=1.0):
    """Positions, normals, uvs and indices of an axis-aligned box, UVs in meters/uv_scale."""
    (x0, y0, z0), (x1, y1, z1) = min_c, max_c
    faces = [
        ((1, 0, 0), [(x1, y0, z1), (x1, y0, z0), (x1, y1, z0), (x1, y1, z1)]),
        ((-1, 0, 0), [(x0, y0, z0), (x0, y0, z1), (x0, y1, z1), (x0, y1, z0)]),
        ((0, 1, 0), [(x0, y1, z1), (x1, y1, z1), (x1, y1, z0), (x0, y1, z0)]),
        ((0, -1, 0), [(x0, y0, z0), (x1, y0, z0), (x1, y0, z1), (x0, y0, z1)]),
        ((0, 0, 1), [(x0, y0, z1), (x1, y0, z1), (x1, y1, z1), (x0, y1, z1)]),
        ((0, 0, -1), [(x1, y0, z0), (x0, y0, z0), (x0, y1, z0), (x1, y1, z0)]),
    ]
    positions, normals, uvs, indices = [], [], [], []
    for normal, corners in faces:
        base = len(positions)
        a, b, d = corners[0], corners[1], corners[3]
        width = math.dist(a, b) / uv_scale
        height = math.dist(a, d) / uv_scale
        for corner, uv in zip(corners, [(0, height), (width, height), (width, 0), (0, 0)]):
            positions.append(corner)
            normals.append(normal)
            uvs.append(uv)
        indices += [base, base + 1, base + 2, base, base + 2, base + 3]
    return positions, normals, uvs, indices


def merge(*meshes):
    out = ([], [], [], [])
    for positions, normals, uvs, indices in meshes:
        base = len(out[0])
        out[0].extend(positions)
        out[1].extend(normals)
        out[2].extend(uvs)
        out[3].extend(i + base for i in indices)
    return out


def glb(meshes, texture=None):
    """A binary glTF with one mesh per `(name, (positions, normals, uvs, indices))`, each on
    its own node. With a `texture` (a URI relative to the file) the meshes share a material
    using it."""
    buffer = bytearray()
    views, accessors, gltf_meshes = [], [], []

    def add(data, target, component, kind, count, bounds=None):
        while len(buffer) % 4:
            buffer.append(0)
        views.append({"buffer": 0, "byteOffset": len(buffer), "byteLength": len(data), "target": target})
        buffer.extend(data)
        accessor = {"bufferView": len(views) - 1, "componentType": component, "count": count, "type": kind}
        if bounds:
            accessor["min"], accessor["max"] = bounds
        accessors.append(accessor)
        return len(accessors) - 1

    for name, (positions, normals, uvs, indices) in meshes:
        bounds = ([min(p[i] for p in positions) for i in range(3)], [max(p[i] for p in positions) for i in range(3)])
        attributes = {
            "POSITION": add(b"".join(struct.pack("<3f", *p) for p in positions), 34962, 5126, "VEC3", len(positions), bounds),
            "NORMAL": add(b"".join(struct.pack("<3f", *n) for n in normals), 34962, 5126, "VEC3", len(normals)),
            "TEXCOORD_0": add(b"".join(struct.pack("<2f", *uv) for uv in uvs), 34962, 5126, "VEC2", len(uvs)),
        }
        index = add(b"".join(struct.pack("<I", i) for i in indices), 34963, 5125, "SCALAR", len(indices))
        primitive = {"attributes": attributes, "indices": index}
        if texture:
            primitive["material"] = 0
        gltf_meshes.append({"name": name, "primitives": [primitive]})
    doc = {
        "asset": {"version": "2.0", "generator": "tools/make_sample_mod.py"},
        "scene": 0,
        "scenes": [{"nodes": list(range(len(meshes)))}],
        "nodes": [{"name": name, "mesh": i} for i, (name, _) in enumerate(meshes)],
        "meshes": gltf_meshes,
        "buffers": [{"byteLength": len(buffer)}],
        "bufferViews": views,
        "accessors": accessors,
    }
    if texture:
        doc["images"] = [{"uri": texture}]
        doc["samplers"] = [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}]
        doc["textures"] = [{"source": 0, "sampler": 0}]
        doc["materials"] = [
            {
                "name": os.path.basename(texture).rsplit(".", 1)[0],
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}, "metallicFactor": 0.0, "roughnessFactor": 0.85},
            }
        ]
    json_bytes = json.dumps(doc, separators=(",", ":")).encode()
    json_bytes += b" " * (-len(json_bytes) % 4)
    while len(buffer) % 4:
        buffer.append(0)
    body = struct.pack("<II", len(json_bytes), 0x4E4F534A) + json_bytes + struct.pack("<II", len(buffer), 0x004E4942) + buffer
    return struct.pack("<III", 0x46546C67, 2, 12 + len(body)) + body


# Objects: (name, texture, visual boxes, texture scale). Collision uses the same boxes.
OBJECTS = {
    "sample_crate": ("planks.png", [((-0.6, 0.0, -0.6), (0.6, 1.2, 0.6))], 1.2),
    "sample_wall": ("concrete.png", [((-4.0, 0.0, -0.25), (4.0, 2.4, 0.25))], 2.0),
    "sample_tower": (
        "metal.png",
        [
            # Four legs, a platform and a hut on top.
            ((-2.0, 0.0, -2.0), (-1.6, 6.0, -1.6)),
            ((1.6, 0.0, -2.0), (2.0, 6.0, -1.6)),
            ((-2.0, 0.0, 1.6), (-1.6, 6.0, 2.0)),
            ((1.6, 0.0, 1.6), (2.0, 6.0, 2.0)),
            ((-2.4, 6.0, -2.4), (2.4, 6.3, 2.4)),
            ((-2.4, 6.3, -2.4), (2.4, 7.3, -2.2)),
            ((-2.4, 6.3, 2.2), (2.4, 7.3, 2.4)),
            ((-2.4, 6.3, -2.2), (-2.2, 7.3, 2.2)),
            ((2.2, 6.3, -2.2), (2.4, 7.3, 2.2)),
        ],
        1.0,
    ),
    "sample_bunker": (
        "concrete.png",
        [
            ((-3.0, 0.0, -3.0), (3.0, 0.4, 3.0)),
            ((-3.0, 0.4, -3.0), (3.0, 2.2, -2.6)),
            ((-3.0, 0.4, 2.6), (-0.8, 2.2, 3.0)),
            ((0.8, 0.4, 2.6), (3.0, 2.2, 3.0)),
            ((-3.0, 0.4, -2.6), (-2.6, 2.2, 2.6)),
            ((2.6, 0.4, -2.6), (3.0, 2.2, 2.6)),
            ((-3.2, 2.2, -3.2), (3.2, 2.6, 3.2)),
        ],
        2.0,
    ),
}


def objects():
    write("objects/sample/textures/planks.png", png(128, 128, planks))
    write("objects/sample/textures/concrete.png", png(128, 128, concrete))
    write("objects/sample/textures/metal.png", png(64, 64, metal))
    for name, (texture, boxes, scale) in OBJECTS.items():
        visual = merge(*(box(a, b, scale) for a, b in boxes))
        short = name.removeprefix("sample_")
        write(f"objects/sample/meshes/{short}.glb", glb([(short, visual)], f"../textures/{texture}"))
        # Collision: what soldiers, vehicles and bullets hit, named `part{N}_{kind}`.
        write(
            f"objects/sample/meshes/{short}.collision.glb",
            glb([(f"part0_{kind}", visual) for kind in ("soldier", "vehicle", "projectile")]),
        )
        write(
            f"templates/{name}.ron",
            "(\n"
            f'    name: "{name}",\n'
            '    kind: "SimpleObject",\n'
            "    parts: [\n"
            "        {\n"
            f'            "mesh": Some("objects/sample/meshes/{short}.glb"),\n'
            f'            "collision": Some("objects/sample/meshes/{short}.collision.glb"),\n'
            '            "position": (0.0, 0.0, 0.0),\n'
            "        },\n"
            "    ],\n"
            ")\n",
        )


# ---------------------------------------------------------------------------------------
# Level


def fmt(v):
    return f"{v:.3f}".rstrip("0").rstrip(".") if "." in f"{v:.3f}" else f"{v}"


def vec(*values):
    return "(" + ", ".join(f"{float(v):.3f}" for v in values) + ")"


def yaw_quat(degrees):
    """Rotation about +Y (counter-clockwise seen from above)."""
    half = math.radians(degrees) / 2
    return (0.0, math.sin(half), 0.0, math.cos(half))


CONTROL_POINTS = [
    # id, name, x, z, team, uncapturable
    ("north_base", "North Base", 0.0, -200.0, 1, True),
    ("south_base", "South Base", 0.0, 200.0, 2, True),
    ("village", "Village", 0.0, 0.0, 0, False),
    ("west_hill", "West Hill", -120.0, 40.0, 0, False),
    ("lakeside", "Lakeside", 60.0, -70.0, 0, False),
]


def level(samples):
    statics = []

    def place(template, x, z, yaw=0.0, sink=0.05):
        y = ground(samples, x, z) - sink
        statics.append((template, (x, y, z), yaw_quat(yaw)))

    # The village: walls around the flag, crates and a bunker.
    for i, (x, z, yaw) in enumerate([(-14, -10, 0), (-2, -16, 0), (14, 8, 90), (10, 16, 0), (-16, 6, 90)]):
        place("sample_wall", x, z, yaw)
    for x, z in [(4, -4), (5.3, -4.2), (4.6, -2.8), (-8, 8), (-6.6, 8.4), (12, -12)]:
        place("sample_crate", x, z, 15 * x)
    place("sample_bunker", -6, -4, 0)
    # Bases: a tower and crates each.
    for bz, side in ((-200.0, 1), (200.0, -1)):
        place("sample_tower", 18, bz + side * 10, 0)
        for x in (-10, -8.7, -9.4):
            place("sample_crate", x, bz, 30 * x)
        place("sample_bunker", -20, bz - side * 12, 180 if side < 0 else 0)
    # West hill: a tower; lakeside: walls and crates.
    place("sample_tower", -120, 44, 30)
    for x, z, yaw in [(55, -62, 20), (66, -80, 110)]:
        place("sample_wall", x, z, yaw)
    for x, z in [(62, -66), (63.2, -66.3)]:
        place("sample_crate", x, z, 40)

    def control_points():
        out = []
        for cid, name, x, z, team, uncapturable in CONTROL_POINTS:
            out.append(
                "                (\n"
                f'                    id: "{cid}",\n'
                f'                    name: "{name}",\n'
                f"                    position: {vec(x, ground(samples, x, z) + 1.0, z)},\n"
                f"                    initial_team: {team},\n"
                f"                    radius: {'25.0' if uncapturable else '14.0'},\n"
                f"                    uncapturable: {'true' if uncapturable else 'false'},\n"
                "                    area_value: (50.0, 50.0),\n"
                "                    time_to_get_control: 12.0,\n"
                "                    time_to_lose_control: 12.0,\n"
                "                ),\n"
            )
        return "".join(out)

    def spawn_points():
        out = []
        for cid, _, cx, cz, team, uncapturable in CONTROL_POINTS:
            count = 8 if uncapturable else 4
            # Face the middle of the map.
            yaw = 180.0 if cz < -50 else (0.0 if cz > 50 else 0.0)
            for i in range(count):
                angle = i / count * math.tau
                x = cx + math.cos(angle) * (10 if uncapturable else 7)
                z = cz + math.sin(angle) * (10 if uncapturable else 7)
                rotation = yaw_quat(yaw)
                out.append(
                    "                {\n"
                    f'                    "control_point": "{cid}",\n'
                    f'                    "position": {vec(x, ground(samples, x, z) + 0.3, z)},\n'
                    f'                    "rotation": {vec(*rotation)},\n'
                    "                },\n"
                )
        return "".join(out)

    def layout(mode):
        return (
            "        (\n"
            f'            mode: "{mode}",\n'
            "            size: 16,\n"
            "            control_points: [\n"
            f"{control_points()}"
            "            ],\n"
            "            spawn_points: [\n"
            f"{spawn_points()}"
            "            ],\n"
            "        ),\n"
        )

    def team(name, language, prefix, rifleman):
        kits = [
            (f"{prefix}_specops", f"{prefix}_light_soldier"),
            (f"{prefix}_sniper", f"{prefix}_light_soldier"),
            (rifleman, f"{prefix}_heavy_soldier"),
            (f"{prefix}_support", f"{prefix}_heavy_soldier"),
            (f"{prefix}_engineer", f"{prefix}_light_soldier"),
            (f"{prefix}_medic", f"{prefix}_light_soldier"),
            (f"{prefix}_at", f"{prefix}_heavy_soldier"),
        ]
        kit_text = "".join(
            f'                (kit: "{kit}", soldier: "{soldier}"),\n' for kit, soldier in kits
        )
        return (
            "        (\n"
            f'            name: "{name}",\n'
            "            kits: [\n"
            f"{kit_text}"
            "            ],\n"
            "            tickets: [(16, 150)],\n"
            "            ticket_loss_per_minute: 10.0,\n"
            f'            language: "{language}",\n'
            "        ),\n"
        )

    static_text = "".join(
        "        {\n"
        f'            "template": "{template}",\n'
        f'            "position": {vec(*position)},\n'
        f'            "rotation": {vec(*rotation)},\n'
        "        },\n"
        for template, position, rotation in statics
    )
    return (
        "// Sample Valley: a level authored directly in the game's formats (generated by\n"
        "// tools/make_sample_mod.py). See docs/MODDING.md.\n"
        "(\n"
        f'    name: "{LEVEL}",\n'
        '    display_name: "Sample Valley",\n'
        "    terrain: Some((\n"
        '        heightmap: "heightmap.r16",\n'
        f"        resolution: {RES},\n"
        f"        spacing: {SPACING},\n"
        f"        height_scale: {HEIGHT_SCALE},\n"
        f"        origin: {vec(*ORIGIN)},\n"
        "    )),\n"
        "    water: Some((\n"
        f"        height: {WATER_HEIGHT},\n"
        "        color: (0.12, 0.28, 0.32, 0.85),\n"
        "    )),\n"
        "    environment: (\n"
        "        sun_direction: (-0.45, -0.55, 0.7),\n"
        "        sun_color: (1.0, 0.86, 0.68),\n"
        "        ambient_color: (0.42, 0.45, 0.52),\n"
        "        sky_color: (0.62, 0.7, 0.82),\n"
        "        fog_color: (0.72, 0.74, 0.78),\n"
        "        fog_range: (150.0, 600.0),\n"
        "        view_distance: 650.0,\n"
        "    ),\n"
        "    statics: [\n"
        f"{static_text}"
        "    ],\n"
        "    game_modes: [\n"
        f"{layout('gpm_cq')}"
        f"{layout('gpm_coop')}"
        "    ],\n"
        "    teams: [\n"
        f"{team('MEC', 'Mec', 'mec', 'mec_assault')}"
        f"{team('US', 'English', 'us', 'sample_rifleman')}"
        "    ],\n"
        f'    minimap: Some("levels/{LEVEL}/minimap.png"),\n'
        "    // BF2's flags, by path: without an import control points have no pole.\n"
        "    flag_models: (\n"
        '        pole: Some("objects/common/flags/flagpole/meshes/flagpole.glb"),\n'
        "        pole_height: 6.03,\n"
        '        flags: (Some("objects/common/flags/flag_neutral/meshes/flag_neutral.glb"), '
        'Some("objects/common/flags/flag_mec/meshes/flag_mec.glb"), '
        'Some("objects/common/flags/flag_us/meshes/flag_us.glb")),\n'
        "    ),\n"
        ")\n"
    )


def minimap(samples):
    size = 256

    def pixel(px, py):
        x = ORIGIN[0] + (px + 0.5) / size * (RES - 1) * SPACING
        z = ORIGIN[2] + (py + 0.5) / size * (RES - 1) * SPACING
        h = ground(samples, x, z)
        # Light from the north-west.
        slope = (ground(samples, x + 2, z + 2) - ground(samples, x - 2, z - 2)) * 0.25
        shade = max(0.55, min(1.15, 0.9 - slope))
        if h < WATER_HEIGHT:
            return (40, 80, 95, 255)
        grass = smoothstep(4.0, 30.0, h)
        r, g, b = 96 + 60 * grass, 118 + 30 * grass, 70 + 40 * grass
        for _, _, cx, cz, _, _ in CONTROL_POINTS:
            if abs(math.hypot(x - cx, z - cz) - 14.0) < 1.6:
                return (230, 230, 230, 255)
        return (r * shade, g * shade, b * shade, 255)

    return png(size, size, pixel)


def main():
    samples = heightmap()
    write(f"levels/{LEVEL}/heightmap.r16", struct.pack(f"<{len(samples)}H", *samples))
    write(f"levels/{LEVEL}/level.ron", level(samples))
    write(f"levels/{LEVEL}/minimap.png", minimap(samples))
    objects()
    write(
        "mod.ron",
        "(\n"
        '    name: "Sample mod",\n'
        '    description: "Sample Valley, a level made from scratch, and a carbine and kit made by patching imported ones.",\n'
        "    priority: 0,\n"
        ")\n",
    )
    # A new weapon made from the imported M4: only what differs.
    write(
        "weapons/sample_carbine.patch.ron",
        "// A new weapon made from the imported M4 (`base`): only the fields that differ.\n"
        "// Enum values are written as strings in patches.\n"
        "(\n"
        '    base: "usrif_m4",\n'
        '    name: "sample_carbine",\n'
        '    display_name: "Sample Carbine",\n'
        "    rounds_per_minute: 850.0,\n"
        "    magazine_size: 40,\n"
        "    magazines: 5,\n"
        '    fire_modes: ["Auto", "Burst", "Single"],\n'
        "    projectile: (damage: 22.0),\n"
        "    recoil: (up: (0.2, 0.35)),\n"
        ")\n",
    )
    write(
        "kits/sample_rifleman.patch.ron",
        "// A new kit made from the imported US assault kit, carrying the sample carbine.\n"
        "(\n"
        '    base: "us_assault",\n'
        '    name: "sample_rifleman",\n'
        '    weapons: ["sample_carbine", "uspis_92fs", "hgr_smoke", "kni_knife", "parachutelauncher"],\n'
        ")\n",
    )
    print(f"wrote {os.path.normpath(ROOT)}")


if __name__ == "__main__":
    main()
