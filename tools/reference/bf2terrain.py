#!/usr/bin/env python3
"""
bf2terrain.py - load a Battlefield 2 level's terrain (heightmap cluster, terrain settings,
terraindata.raw header, water/sky) straight from the level archives and print verified stats;
optionally write small PNG previews.  Python 3.10 stdlib only.

    python bf2terrain.py                      # Strike_at_Karkand, PNGs into ./previews
    python bf2terrain.py Dalian_Plant --no-png
    python bf2terrain.py Night_Flight --mod xpack --out some/dir
Options: --root "<BF2 install>" (default: $BF2_DIR)  --mod bf2|xpack  --out DIR  --no-png  --colormap (also write a
         downscaled colormap-mosaic preview decoded from the DXT1 tiles)

Conventions (verified, see level_formats.md):
  * world: left-handed, +X east, +Y up, +Z north, metres, origin = map centre.
  * HeightmapPrimary.raw: N*N uint16 LE (N = 2^k+1), no header, index = row*N + col,
    worldX = col*scale.x - (N-1)*scale.x/2, worldZ = row*scale.z - (N-1)*scale.z/2,
    worldY = sample*scale.y   (row 0 = minimum Z = south edge = BOTTOM of the minimap)
  * ground triangles: each cell split along the diagonal (col+1,row) - (col,row+1)
  * secondary heightmaps: cluster cell (cx,cy) centred at (cx*S, cy*S) with S = heightmapSize,
    cy -> +Z.  So U1 (cy=-1) is SOUTH, D1 north, L1 west, R1 east.  8-bit, same row/col rule.
"""
import sys, os, re, struct, zlib, array, math, zipfile, glob

DEFAULT_ROOT = os.environ.get('BF2_DIR', r'C:/Program Files (x86)/EA Games/Battlefield 2')


# ----------------------------------------------------------------------------- archive access
class LevelArchives:
    def __init__(self, root, mod, level):
        cands = glob.glob(os.path.join(root, 'mods', mod, 'Levels', '*'))
        hit = [c for c in cands if os.path.basename(c).lower() == level.lower()]
        if not hit:
            raise SystemExit('level %s not found in mod %s' % (level, mod))
        self.dir = hit[0]
        self.name = os.path.basename(self.dir)
        self.server = zipfile.ZipFile(os.path.join(self.dir, 'server.zip'))
        self.client = zipfile.ZipFile(os.path.join(self.dir, 'client.zip'))
        self._s = {n.lower(): n for n in self.server.namelist()}
        self._c = {n.lower(): n for n in self.client.namelist()}

    def _key(self, path):
        p = path.replace('\\', '/').lower().strip('/')
        pre = 'levels/' + self.name.lower() + '/'
        return p[len(pre):] if p.startswith(pre) else p

    def has(self, path):
        k = self._key(path)
        return k in self._s or k in self._c

    def read(self, path):
        k = self._key(path)
        if k in self._s:
            return self.server.read(self._s[k])
        if k in self._c:
            return self.client.read(self._c[k])
        raise FileNotFoundError(path)

    def text(self, path):
        return self.read(path).decode('latin1')


def con_statements(text, args=()):
    """Tiny .con reader for these data files: drops rem/beginRem, evaluates
    'if v_arg1 == X / else / endIf' with the given args (game branch by default)."""
    out, stack, depth = [], [], 0
    argv = {'v_arg%d' % (i + 1): (args[i] if i < len(args) else '') for i in range(8)}
    for line in text.splitlines():
        t = line.split()
        if not t:
            continue
        k = t[0].lower()
        if k == 'beginrem':
            depth += 1; continue
        if k == 'endrem':
            depth -= 1; continue
        if depth or k == 'rem':
            continue
        if k == 'if':
            a = argv.get(t[1].lower(), t[1]).lower()
            stack.append((a == t[3].lower()) if t[2] == '==' else (a != t[3].lower()))
            continue
        if k == 'else':
            stack[-1] = not stack[-1]; continue
        if k == 'endif':
            stack.pop(); continue
        if all(stack):
            m = re.match(r'\s*(\S+)\s*(.*)$', line)
            out.append((m.group(1).lower(), m.group(2).strip()))
    return out


def fvec(s):
    return [float(v) for v in s.split('/')]


# ----------------------------------------------------------------------------- heightmaps
class Heightmap:
    def __init__(self, cx, cy):
        self.cx, self.cy = cx, cy
        self.size = None
        self.scale = None
        self.bits = 16
        self.path = None
        self.data = None
        self.mat_path = None

    def load(self, arch):
        raw = arch.read(self.path)
        a = array.array('H' if self.bits == 16 else 'B')
        a.frombytes(raw)          # little-endian host assumed (x86)
        if len(a) != self.size * self.size:
            raise ValueError('%s: %d samples, expected %d' % (self.path, len(a), self.size ** 2))
        self.data = a

    @property
    def n(self):
        return self.size - 1

    @property
    def extent(self):
        return self.n * self.scale[0]

    def origin(self, cluster_size_world):
        """World-space (x, z) of sample (row 0, col 0)."""
        return (self.cx * cluster_size_world - self.extent / 2, self.cy * cluster_size_world - self.n * self.scale[2] / 2)

    def sample(self, r, c):
        r = min(max(r, 0), self.n); c = min(max(c, 0), self.n)
        return self.data[r * self.size + c] * self.scale[1]

    def height_local(self, fx, fz):
        """fx, fz in sample units from (row0,col0).  Triangle split (c+1,r)-(c,r+1)."""
        n = self.n
        fx = min(max(fx, 0.0), n - 1e-6); fz = min(max(fz, 0.0), n - 1e-6)
        c, r = int(fx), int(fz)
        u, v = fx - c, fz - r
        h00 = self.sample(r, c); h01 = self.sample(r, c + 1); h10 = self.sample(r + 1, c); h11 = self.sample(r + 1, c + 1)
        if u + v <= 1.0:
            return h00 + (h01 - h00) * u + (h10 - h00) * v
        return h11 + (h10 - h11) * (1.0 - u) + (h01 - h11) * (1.0 - v)


class Terrain:
    def __init__(self, arch):
        self.arch = arch
        st = con_statements(arch.text('Heightdata.con'))
        self.cluster_size = 3
        self.heightmap_size = None
        self.sea_level = None
        self.maps = []
        cur = None
        for cmd, rest in st:
            a = rest.split()
            if cmd == 'heightmapcluster.setclustersize':
                self.cluster_size = int(a[0])
            elif cmd == 'heightmapcluster.setheightmapsize':
                self.heightmap_size = float(a[0])
            elif cmd == 'heightmapcluster.addheightmap':
                cur = Heightmap(int(a[1]), int(a[2]))          # a[0] is the literal name 'Heightmap'
                self.maps.append(cur)
            elif cmd == 'heightmapcluster.setseawaterlevel':
                self.sea_level = float(a[0])
            elif cur is not None and cmd == 'heightmap.setsize':
                cur.size = int(a[0])
            elif cur is not None and cmd == 'heightmap.setscale':
                cur.scale = fvec(a[0])
            elif cur is not None and cmd == 'heightmap.setbitresolution':
                cur.bits = int(a[0])
            elif cur is not None and cmd == 'heightmap.loadheightdata':
                cur.path = a[0]
            elif cur is not None and cmd == 'heightmap.loadmaterialdata':
                cur.mat_path = a[0]
        for m in self.maps:
            m.load(arch)
        self.primary = next(m for m in self.maps if m.cx == 0 and m.cy == 0)
        if self.heightmap_size is None:
            self.heightmap_size = self.primary.extent

    def height(self, x, z):
        """World height with the engine's cluster placement; falls back to secondaries outside."""
        S = self.heightmap_size
        cx = int(math.floor((x + S / 2) / S)); cy = int(math.floor((z + S / 2) / S))
        for m in self.maps:
            if m.cx == cx and m.cy == cy:
                ox, oz = m.origin(S)
                return m.height_local((x - ox) / m.scale[0], (z - oz) / m.scale[2])
        return None

    def normal(self, x, z, d=None):
        d = d or self.primary.scale[0]
        hx = (self.height(x + d, z) - self.height(x - d, z)) / (2 * d)
        hz = (self.height(x, z + d) - self.height(x, z - d)) / (2 * d)
        n = (-hx, 1.0, -hz); l = math.sqrt(sum(v * v for v in n))
        return tuple(v / l for v in n)


# ----------------------------------------------------------------------------- terraindata.raw header
def parse_terraindata_header(d):
    o = 0
    r = {}
    r['version'] = struct.unpack_from('<I', d, o)[0]; o += 4                  # 0x0001001a
    r['primaryWorldScale'] = struct.unpack_from('<3f', d, o); o += 12
    r['secondaryWorldScale'] = struct.unpack_from('<3f', d, o); o += 12
    o += 4                                                                    # 0xcdcdcdcd (uninitialised)
    r['maxHeight'], r['minHeight'] = struct.unpack_from('<2f', d, o); o += 8
    r['patchSize'] = struct.unpack_from('<I', d, o)[0]; o += 4
    r['subdividePatches'] = d[o]; o += 1
    r['patchesPerSide'] = struct.unpack_from('<I', d, o)[0]; o += 4
    r['patchColormapSize'], r['lowDetailmapSize'] = struct.unpack_from('<2I', d, o); o += 8
    names = []
    for _ in range(4):
        e = d.index(b'\n', o); names.append(d[o:e].decode('latin1')); o = e + 1
    r['colormapBase'], r['detailmapBase'], r['lowDetailmapBase'], r['lightmapBase'] = names
    r['farSideTiling'] = struct.unpack_from('<2f', d, o); o += 8
    r['farTopTilingHi'], r['farTopTilingLow'], r['farYOffset'] = struct.unpack_from('<3f', d, o); o += 12
    r['sunColor'] = struct.unpack_from('<3f', d, o); o += 12
    r['GIColor'] = struct.unpack_from('<3f', d, o); o += 12
    r['waterColor'] = struct.unpack_from('<3f', d, o); o += 12
    n = struct.unpack_from('<I', d, o)[0]; o += 4
    mats = []
    for i in range(n):
        e = d.index(b'\n', o); tex = d[o:e].decode('latin1'); o = e + 1
        planar = d[o]; o += 1
        side_u, side_v, top, yoff = struct.unpack_from('<4f', d, o); o += 16
        env = d[o]; o += 1
        # detail-map channel holding this material's weight (verified by slope/undergrowth correlation)
        chan = 'Detailmaps/tx??x??_%d.dds %s' % (1 + i // 3, 'BGR'[i % 3])
        mats.append(dict(texture=tex, triPlanar=planar, sideTiling=(side_u, side_v), topTiling=top, yOffset=yoff,
                         envMap=env, weightChannel=chan))
    r['detailTextures'] = mats
    r['headerEnd'] = o
    return r


# ----------------------------------------------------------------------------- PNG writer
def write_png(path, w, h, rows, mode='L'):
    """rows: list of bytes/bytearray, each w*channels long. mode 'L' or 'RGB'."""
    ch = {'L': 1, 'RGB': 3}[mode]
    ct = {'L': 0, 'RGB': 2}[mode]
    raw = b''.join(b'\x00' + bytes(r) for r in rows)
    def chunk(t, data):
        c = struct.pack('>I', len(data)) + t + data
        return c + struct.pack('>I', zlib.crc32(t + data) & 0xffffffff)
    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, ct, 0, 0, 0)) + \
          chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b'')
    with open(path, 'wb') as f:
        f.write(png)


# ----------------------------------------------------------------------------- DXT1 (for the colormap preview)
def dxt1_decode_downsampled(b, step):
    """Decode a DXT1 .dds, returning every `step`-th pixel (step multiple of 4 or 1..4)."""
    h, w = struct.unpack_from('<II', b, 12)
    out = [[None] * (w // step) for _ in range(h // step)]
    o = 128
    for by in range(0, h, 4):
        for bx in range(0, w, 4):
            c0, c1, idx = struct.unpack_from('<HHI', b, o); o += 8
            p = []
            for c in (c0, c1):
                p.append((((c >> 11) & 31) * 255 // 31, ((c >> 5) & 63) * 255 // 63, (c & 31) * 255 // 31))
            if c0 > c1:
                p += [tuple((2 * a + b_) // 3 for a, b_ in zip(p[0], p[1])), tuple((a + 2 * b_) // 3 for a, b_ in zip(p[0], p[1]))]
            else:
                p += [tuple((a + b_) // 2 for a, b_ in zip(p[0], p[1])), (0, 0, 0)]
            for i in range(16):
                y, x = by + i // 4, bx + i % 4
                if y % step == 0 and x % step == 0:
                    out[y // step][x // step] = p[(idx >> (2 * i)) & 3]
    return out


# ----------------------------------------------------------------------------- main
def main(argv):
    def opt(name, default):
        if name in argv:
            i = argv.index(name); v = argv[i + 1]; del argv[i:i + 2]; return v
        return default
    root = opt('--root', DEFAULT_ROOT)
    mod = opt('--mod', 'bf2')
    out = opt('--out', os.path.join(os.path.dirname(os.path.abspath(__file__)), 'previews'))
    no_png = '--no-png' in argv
    want_cm = '--colormap' in argv
    argv = [a for a in argv if not a.startswith('--')]
    level = argv[0] if argv else 'Strike_at_Karkand'

    arch = LevelArchives(root, mod, level)
    T = Terrain(arch)
    P = T.primary
    data = P.data
    hs = [v * P.scale[1] for v in data]
    mn, mx = min(hs), max(hs)
    mean = sum(hs) / len(hs)
    print('== %s (%s)' % (arch.name, mod))
    print('Heightdata.con: clusterSize %d, heightmapSize %g m, seaWaterLevel %s' % (T.cluster_size, T.heightmap_size, T.sea_level))
    print('primary   %s: %dx%d, %d-bit LE, %d bytes, scale %s -> cell %.3g m, extent %.1f m (X/Z %.1f..%.1f), '
          'height unit %.8g m (max representable %.1f m)' % (
              P.path, P.size, P.size, P.bits, len(data) * (P.bits // 8), '/'.join('%g' % v for v in P.scale), P.scale[0],
              P.extent, -P.extent / 2, P.extent / 2, P.scale[1], (2 ** P.bits - 1) * P.scale[1]))
    print('          raw min %d max %d -> world height %.2f .. %.2f m, mean %.2f m' % (min(data), max(data), mn, mx, mean))
    if T.sea_level is not None:
        below = sum(1 for v in hs if v < T.sea_level)
        print('          samples below sea level: %.1f%%' % (100.0 * below / len(hs)))
    for m in T.maps:
        if m is P:
            continue
        v = [x * m.scale[1] for x in m.data]
        ox, oz = m.origin(T.heightmap_size)
        print('secondary (%+d,%+d) %-28s %dx%d %d-bit scale %s  X %.0f..%.0f Z %.0f..%.0f  h %.1f..%.1f' % (
            m.cx, m.cy, os.path.basename(m.path), m.size, m.size, m.bits, '/'.join('%g' % s for s in m.scale),
            ox, ox + m.extent, oz, oz + m.n * m.scale[2], min(v), max(v)))
    # seam check between primary and each edge neighbour (verifies placement)
    seams = []
    for m in T.maps:
        if m is P or (m.cx != 0 and m.cy != 0):
            continue
        errs = []
        for i in range(0, P.size, 4):
            t = -P.extent / 2 + i * P.scale[0]
            x, z = {(-1, 0): (-P.extent / 2, t), (1, 0): (P.extent / 2, t), (0, -1): (t, -P.extent / 2), (0, 1): (t, P.extent / 2)}[(m.cx, m.cy)]
            ox, oz = m.origin(T.heightmap_size)
            hsec = m.height_local((x - ox) / m.scale[0], (z - oz) / m.scale[2])
            hpri = P.height_local((x + P.extent / 2) / P.scale[0], (z + P.extent / 2) / P.scale[2])
            errs.append(abs(hsec - hpri))
        errs.sort()
        seams.append('%s median %.2f m' % (os.path.basename(m.path), errs[len(errs) // 2]))
    print('seam check (primary edge vs neighbour):', '; '.join(seams))

    # Terrain.con / terraindata.raw
    tc = con_statements(arch.text('Terrain.con'), ('BF2Editor',))
    ed = {k.split('.', 1)[1]: v for k, v in tc if k.startswith('terrain.')}
    print('Terrain.con (editor branch values):', {k: ed[k] for k in ('patchsize', 'primaryworldscale', 'secondaryworldscale',
          'patchcolormapsize', 'lowdetailmapsize', 'fartoptilinghi', 'fartoptilinglow', 'terrainwatercolor') if k in ed})
    hdr = parse_terraindata_header(arch.read('terraindata.raw'))
    print('terraindata.raw header: version %#x, maxH %.2f minH %.2f, patchSize %d x %d patches/side (patch = %.0f m), '
          'colormap %d px/patch, far side tiling %s top hi/lo %g/%g yOff %g' % (
              hdr['version'], hdr['maxHeight'], hdr['minHeight'], hdr['patchSize'], hdr['patchesPerSide'],
              hdr['patchSize'] * P.scale[0], hdr['patchColormapSize'], hdr['farSideTiling'], hdr['farTopTilingHi'],
              hdr['farTopTilingLow'], hdr['farYOffset']))
    print('   sun %s GI %s water %s' % tuple(['/'.join('%.3f' % c for c in hdr[k]) for k in ('sunColor', 'GIColor', 'waterColor')]))
    for i, m in enumerate(hdr['detailTextures']):
        print('   detail[%d] %-45s triPlanar=%d side=%s top=%g yOff=%g env=%d weight=%s' % (
            i, m['texture'], m['triPlanar'], m['sideTiling'], m['topTiling'], m['yOffset'], m['envMap'], m['weightChannel']))
    tiles = sorted(k for k in arch._c if re.match(r'colormaps/tx\d\dx\d\d\.dds$', k))
    print('colormap tiles: %d (grid %d x %d, file txXXxZZ: XX = column along +X, ZZ = row along +Z; image row 0 = min Z)' % (
        len(tiles), hdr['patchesPerSide'], hdr['patchesPerSide']))
    # water / sky
    if arch.has('Water.con'):
        print('Water.con:', {k: v for k, v in con_statements(arch.text('Water.con'))})
    sky = dict(con_statements(arch.text('Sky.con')))
    sd = fvec(sky.get('lightmanager.sundirection', '0/-1/0'))
    print('Sky.con: sunDirection %s, fog %s (%s), skyTexture %s' % (sd, sky.get('renderer.fogstartendandbase'),
          sky.get('renderer.fogcolor'), sky.get('skydome.skytexture')))
    # sample heights at control points of the biggest CQ layer (sanity: editor-snapped Y == terrain)
    gpos = [k for k in arch._s if re.match(r'gamemodes/gpm_cq/\d+/gameplayobjects.con$', k)]
    if gpos:
        g = sorted(gpos, key=lambda k: int(k.split('/')[2]))[-1]
        txt = arch.server.read(arch._s[g]).decode('latin1')
        cps = set(m.group(1).lower() for m in re.finditer(r'ObjectTemplate\.create\s+ControlPoint\s+(\S+)', txt, re.I))
        res = []
        for m in re.finditer(r'Object\.create\s+(\S+)\s*\n\s*Object\.absolutePosition\s+(\S+)', txt, re.I):
            if m.group(1).lower() in cps:
                x, y, z = fvec(m.group(2))
                res.append(y - T.height(x, z))
        if res:
            a = sorted(abs(r) for r in res)
            print('control points in %s: %d, |objectY - terrainY| median %.3f m, max %.3f m (CPs on buildings sit higher)' % (
                g, len(res), a[len(a) // 2], a[-1]))

    if no_png:
        return
    os.makedirs(out, exist_ok=True)
    base = os.path.join(out, arch.name)
    N = P.size
    # 1) heightmap, north-up (flip rows: row 0 = south)
    rows = []
    for r in range(N - 1, -1, -1):
        rows.append(bytes(int(255 * (hs[r * N + c] - mn) / (mx - mn or 1)) for c in range(N)))
    write_png(base + '_height.png', N, N, rows, 'L')
    # 2) hillshade with sun from Sky.con, water tinted below sea level
    L = [-v for v in sd]; ll = math.sqrt(sum(v * v for v in L)); L = [v / ll for v in L]
    rows = []
    for r in range(N - 1, -1, -1):
        row = bytearray()
        for c in range(N):
            hx = (P.sample(r, c + 1) - P.sample(r, c - 1)) / (2 * P.scale[0])
            hz = (P.sample(r + 1, c) - P.sample(r - 1, c)) / (2 * P.scale[2])
            n = (-hx, 1.0, -hz); nl = math.sqrt(sum(v * v for v in n))
            s = max(0.0, sum(a * b for a, b in zip(n, L)) / nl)
            v = int(60 + 195 * s)
            if T.sea_level is not None and hs[r * N + c] < T.sea_level:
                row += bytes((v // 4, v // 3, min(255, v // 2 + 60)))
            else:
                row += bytes((v, v, v))
        rows.append(row)
    write_png(base + '_hillshade.png', N, N, rows, 'RGB')
    # 3) whole 3x3 cluster overview (each cell resampled to 256 px), north-up
    C = 256
    W = C * T.cluster_size
    S = T.heightmap_size
    allh = []
    for m in T.maps:
        allh += [min(m.data) * m.scale[1], max(m.data) * m.scale[1]]
    lo, hi = min(allh), max(allh)
    rows = []
    half = S * T.cluster_size / 2
    for py in range(W):
        row = bytearray()
        z = half - (py + 0.5) * S / C
        for px in range(W):
            x = -half + (px + 0.5) * S / C
            h = T.height(x, z)
            v = 0 if h is None else int(255 * (h - lo) / (hi - lo or 1))
            if T.sea_level is not None and h is not None and h < T.sea_level:
                row += bytes((v // 4, v // 3, min(255, v // 2 + 70)))
            else:
                row += bytes((v, v, v))
        rows.append(row)
    write_png(base + '_cluster.png', W, W, rows, 'RGB')
    written = [base + s for s in ('_height.png', '_hillshade.png', '_cluster.png')]
    # 4) optional small colormap mosaic preview (north-up)
    if want_cm:
        Tn = hdr['patchesPerSide']
        tile_px = 512 // Tn
        step = hdr['patchColormapSize'] // tile_px
        mosaic = [[(0, 0, 0)] * (Tn * tile_px) for _ in range(Tn * tile_px)]
        for k in tiles:
            a, b = map(int, re.search(r'tx(\d\d)x(\d\d)', k).groups())
            px = dxt1_decode_downsampled(arch.client.read(arch._c[k]), step)
            for y in range(tile_px):
                for x in range(tile_px):
                    mosaic[b * tile_px + y][a * tile_px + x] = px[y][x]
        rows = [b''.join(bytes(p) for p in r) for r in mosaic[::-1]]
        write_png(base + '_colormap.png', Tn * tile_px, Tn * tile_px, rows, 'RGB')
        written.append(base + '_colormap.png')
    print('wrote', ', '.join(written))


if __name__ == '__main__':
    main(sys.argv[1:])
