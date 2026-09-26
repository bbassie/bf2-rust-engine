#!/usr/bin/env python3
"""
bf2mesh.py - reference parser for Battlefield 2 visible meshes
(.staticmesh / .bundledmesh / .skinnedmesh) and compiled road meshes
(Levels/<map>/client.zip : Roads/<name>_compiled.mesh).

Standalone, stdlib only.  All values little-endian.  See mesh_formats.md.

    python bf2mesh.py                          # parse + validate every mesh in the install
    python bf2mesh.py --dump <zip> <member>    # dump one visible mesh
    python bf2mesh.py --roads                  # parse + validate every compiled road mesh
"""
import glob
import math
import os
import struct
import sys
import zipfile
from array import array
from collections import Counter, defaultdict

# --------------------------------------------------------------------------
# D3D enums
# --------------------------------------------------------------------------
D3DDECLTYPE = {
    0: ('FLOAT1', 4), 1: ('FLOAT2', 8), 2: ('FLOAT3', 12), 3: ('FLOAT4', 16),
    4: ('D3DCOLOR', 4), 5: ('UBYTE4', 4), 6: ('SHORT2', 4), 7: ('SHORT4', 8),
    8: ('UBYTE4N', 4), 9: ('SHORT2N', 4), 10: ('SHORT4N', 8),
    11: ('USHORT2N', 4), 12: ('USHORT4N', 8), 13: ('UDEC3', 4), 14: ('DEC3N', 4),
    15: ('FLOAT16_2', 4), 16: ('FLOAT16_4', 8), 17: ('UNUSED', 0),
}
# usage: low byte = D3DDECLUSAGE, high byte = usage index.  BF2 only uses the
# high byte for TEXCOORD: 0x0005, 0x0105, 0x0205, 0x0305, 0x0405 = TEXCOORD0..4
D3DDECLUSAGE = {
    0: 'POSITION', 1: 'BLENDWEIGHT', 2: 'BLENDINDICES', 3: 'NORMAL', 4: 'PSIZE',
    5: 'TEXCOORD', 6: 'TANGENT', 7: 'BINORMAL', 8: 'TESSFACTOR', 9: 'POSITIONT',
    10: 'COLOR', 11: 'FOG', 12: 'DEPTH', 13: 'SAMPLE',
}
USAGE_POSITION, USAGE_BLENDWEIGHT, USAGE_BLENDINDICES, USAGE_NORMAL, USAGE_TANGENT = 0, 1, 2, 3, 6


def USAGE_TEXCOORD(n):
    return (n << 8) | 5


ALPHA_MODES = {0: 'NONE', 1: 'ALPHA_BLEND', 2: 'ALPHA_TEST'}


class BF2FormatError(Exception):
    pass


class Reader:
    __slots__ = ('d', 'p')

    def __init__(self, data, pos=0):
        self.d = data
        self.p = pos

    def need(self, n):
        if self.p + n > len(self.d):
            raise BF2FormatError(f'unexpected EOF at 0x{self.p:x} (need {n}, size {len(self.d)})')

    def u8(self):
        self.need(1); v = self.d[self.p]; self.p += 1; return v

    def u16(self):
        self.need(2); v = struct.unpack_from('<H', self.d, self.p)[0]; self.p += 2; return v

    def u32(self):
        self.need(4); v = struct.unpack_from('<I', self.d, self.p)[0]; self.p += 4; return v

    def f32(self):
        self.need(4); v = struct.unpack_from('<f', self.d, self.p)[0]; self.p += 4; return v

    def fmt(self, f):
        n = struct.calcsize('<' + f); self.need(n)
        v = struct.unpack_from('<' + f, self.d, self.p); self.p += n; return v

    def vec3(self):
        return self.fmt('3f')

    def mat4(self):
        v = self.fmt('16f')
        return [list(v[0:4]), list(v[4:8]), list(v[8:12]), list(v[12:16])]

    def raw(self, n):
        self.need(n); v = self.d[self.p:self.p + n]; self.p += n; return v

    def string(self, maxlen=4096):
        n = self.u32()
        if n > maxlen:
            raise BF2FormatError(f'implausible string length {n} at 0x{self.p - 4:x}')
        return self.raw(n).decode('latin-1')

    def count(self, limit, what):
        n = self.u32()
        if n > limit:
            raise BF2FormatError(f'implausible {what} count {n} at 0x{self.p - 4:x}')
        return n

    def eof(self):
        return self.p == len(self.d)


# --------------------------------------------------------------------------
# Visible mesh data model
# --------------------------------------------------------------------------
class VertexElement:
    """8 bytes: u16 flag (0 = used, 0xFF = unused terminator), u16 byte offset,
    u16 D3DDECLTYPE, u16 usage (low byte D3DDECLUSAGE, high byte usage index)."""
    __slots__ = ('flag', 'offset', 'type', 'usage')

    def __init__(self, flag, offset, type_, usage):
        self.flag, self.offset, self.type, self.usage = flag, offset, type_, usage

    @property
    def unused(self):
        return self.flag == 0xFF

    @property
    def type_name(self):
        return D3DDECLTYPE.get(self.type, ('?%d' % self.type, 0))[0]

    @property
    def size(self):
        return D3DDECLTYPE.get(self.type, ('?', 0))[1]

    @property
    def usage_name(self):
        base = D3DDECLUSAGE.get(self.usage & 0xFF, '?%d' % (self.usage & 0xFF))
        idx = self.usage >> 8
        return f'{base}{idx}' if base == 'TEXCOORD' else (base if idx == 0 else f'{base}{idx}')

    def __repr__(self):
        return f'<{self.usage_name} {self.type_name} @{self.offset}{" UNUSED" if self.unused else ""}>'


class Material:
    def __init__(self):
        self.alpha_mode = None      # u32; absent in skinnedmesh
        self.fx = ''                # shader file name ("StaticMesh.fx" ...), informational only
        self.technique = ''
        self.maps = []              # texture paths (no fixed slot count, see md)
        self.vstart = self.istart = self.inum = self.vnum = 0
        self.u4 = self.u5 = 0       # unused by engine (garbage in old files)
        self.bmin = self.bmax = None  # staticmesh v11 only (+ orphan gpu_ v5 file)
        self.extra = None


class Rig:
    def __init__(self):
        self.bones = []   # list of (ske_node_index, 4x4 matrix as stored = rows)


class Lod:
    def __init__(self):
        self.bmin = self.bmax = None
        self.radius = None     # version <= 6: (sphere radius about origin, half-diagonal of AABB, junk)
        self.extra = None      # orphan gpu_* variant (head[3] == 10): (f32, f32, u8)
        self.nodes = []        # staticmesh: 4x4 matrices -- UNUSED by the engine (verified)
        self.node_count = 0    # staticmesh: len(nodes); bundledmesh: number of geometry parts
        self.rigs = []         # skinnedmesh: normally one Rig per material
        self.materials = []


class VisibleMesh:
    def __init__(self):
        self.kind = None       # 'staticmesh' | 'bundledmesh' | 'skinnedmesh'
        self.head = None       # (u0, version, u1, u2, u3)
        self.version = 0
        self.u8flag = 0
        self.geoms = []        # list[list[Lod]]
        self.elements = []     # VertexElement list (includes the terminating UNUSED one)
        self.vertformat = 0    # always 4 (bytes per component; == D3DPT_TRIANGLELIST too)
        self.stride = 0
        self.vnum = 0
        self.vbuf = b''
        self.indices = None    # array('H')
        self.alpha_sets = None  # u32 (static/bundled only), always 8 in shipped files

    def element(self, usage):
        for e in self.elements:
            if not e.unused and e.usage == usage:
                return e
        return None

    def texcoord_sets(self):
        return [i for i in range(8) if self.element(USAGE_TEXCOORD(i))]

    def lods(self):
        for gi, g in enumerate(self.geoms):
            for li, l in enumerate(g):
                yield gi, li, l

    def floats(self):
        """whole vertex buffer as array('f') (stride is always a multiple of 4)."""
        a = array('f')
        a.frombytes(self.vbuf)
        if sys.byteorder != 'little':
            a.byteswap()
        return a

    def attribute(self, usage, first, count):
        """decode one vertex attribute for vertices [first, first+count).
        D3DCOLOR is returned in MEMORY byte order (b0,b1,b2,b3) which is what the
        shaders see after D3DCOLORtoUBYTE4()."""
        e = self.element(usage)
        if e is None:
            return None
        fmt = {'FLOAT1': '<f', 'FLOAT2': '<2f', 'FLOAT3': '<3f', 'FLOAT4': '<4f',
               'D3DCOLOR': '<4B', 'UBYTE4': '<4B'}[e.type_name]
        base = e.offset
        return [struct.unpack_from(fmt, self.vbuf, v * self.stride + base) for v in range(first, first + count)]


def detect_kind(name):
    n = name.lower()
    for k in ('staticmesh', 'bundledmesh', 'skinnedmesh'):
        if n.endswith('.' + k):
            return k
    raise ValueError(name)


def parse_visible_mesh(data, kind):
    """Parse a .staticmesh/.bundledmesh/.skinnedmesh byte string (kind from extension)."""
    r = Reader(data)
    m = VisibleMesh()
    m.kind = kind
    m.head = r.fmt('5I')
    m.version = ver = m.head[1]
    m.u8flag = r.u8()

    # geometry table
    ngeom = r.count(64, 'geom')
    for _ in range(ngeom):
        nlod = r.count(64, 'lod')
        m.geoms.append([Lod() for _ in range(nlod)])

    # vertex declaration
    nel = r.count(32, 'vertex element')
    for _ in range(nel):
        m.elements.append(VertexElement(*r.fmt('4H')))

    m.vertformat = r.u32()
    m.stride = r.u32()
    m.vnum = r.count(1 << 24, 'vertex')
    if m.stride == 0 or m.stride > 256:
        raise BF2FormatError(f'implausible stride {m.stride}')
    m.vbuf = r.raw(m.stride * m.vnum)
    ninds = r.count(1 << 26, 'index')
    m.indices = array('H')
    m.indices.frombytes(r.raw(ninds * 2))
    if sys.byteorder != 'little':
        m.indices.byteswap()

    if kind != 'skinnedmesh':
        m.alpha_sets = r.u32()

    # per-LOD block: bounds + type specific data
    for gi, li, lod in m.lods():
        lod.bmin = r.vec3()
        lod.bmax = r.vec3()
        if m.head[3] == 10:
            # orphan "gpu_*" test exports in xpack (head = 0,6,69,10,0 / 0,5,819,10,0)
            lod.extra = (r.f32(), r.f32(), r.u8())
        elif ver <= 6:
            lod.radius = r.vec3()
        if kind == 'skinnedmesh':
            nrig = r.count(256, 'rig')
            for _ in range(nrig):
                rig = Rig()
                nb = r.count(256, 'bone')
                for _ in range(nb):
                    bid = r.u32()
                    rig.bones.append((bid, r.mat4()))
                lod.rigs.append(rig)
        elif kind == 'bundledmesh':
            lod.node_count = r.count(1024, 'node')        # number of geometry parts, no matrices
        else:
            lod.node_count = r.count(1024, 'node')
            lod.nodes = [r.mat4() for _ in range(lod.node_count)]

    # materials
    for gi, li, lod in m.lods():
        nmat = r.count(1024, 'material')
        for _ in range(nmat):
            mat = Material()
            if kind != 'skinnedmesh':
                mat.alpha_mode = r.u32()
            mat.fx = r.string()
            mat.technique = r.string()
            nmaps = r.count(32, 'map')
            mat.maps = [r.string() for _ in range(nmaps)]
            mat.vstart, mat.istart, mat.inum, mat.vnum, mat.u4, mat.u5 = r.fmt('6I')
            if kind == 'staticmesh' and ver == 11:
                mat.bmin = r.vec3()
                mat.bmax = r.vec3()
            elif kind == 'staticmesh' and m.head[3] == 10:
                mat.bmin = r.vec3()
                mat.bmax = r.vec3()
                mat.extra = r.fmt('2IB')
            lod.materials.append(mat)

    if not r.eof():
        raise BF2FormatError(f'{len(data) - r.p} trailing bytes at 0x{r.p:x}')
    return m


# --------------------------------------------------------------------------
# convenience helpers
# --------------------------------------------------------------------------
def material_triangles(mesh, mat, alpha_set=0):
    """Triangles of a material as (a,b,c) indices into the GLOBAL vertex buffer.

    Index-buffer values are relative to mat.vstart.  For ALPHA_BLEND (alpha_mode 1)
    materials of static/bundled meshes the index buffer holds mesh.alpha_sets (=8)
    consecutive copies of the triangle list (each mat.inum long, each depth-sorted
    for a different view direction); pick one with alpha_set (0 = any)."""
    start = mat.istart
    if mat.alpha_mode == 1 and mesh.alpha_sets:
        start += alpha_set * mat.inum
    ib = mesh.indices
    vs = mat.vstart
    return [(ib[i] + vs, ib[i + 1] + vs, ib[i + 2] + vs) for i in range(start, start + mat.inum - 2, 3)]


STATIC_LAYERS = ('Base', 'Detail', 'Dirt', 'Crack', 'NDetail', 'NCrack')


def staticmesh_layers(technique):
    """Split a staticmesh technique ('BaseDetailDirtCrackNDetailNCrack', ...)
    into texture layers in map order.  Unknown suffixes (e.g. 'parallaxdetail')
    are returned separately."""
    t = technique
    out, extra = [], ''
    tl = t.lower()
    i = 0
    while i < len(t):
        for name in ('NDetail', 'NCrack', 'Base', 'Detail', 'Dirt', 'Crack'):
            if tl.startswith(name.lower(), i):
                out.append(name)
                i += len(name)
                break
        else:
            extra = t[i:]
            break
    return out, extra


def staticmesh_uv_channels(mesh, mat):
    """Map texture layer -> TEXCOORD set index for one staticmesh material.

    Base -> 0, Detail/NDetail -> 1, Dirt -> 2, Crack/NCrack -> 3 if the material
    uses Dirt else 2 (bf2-blender convention; consistent with shader permutations),
    Lightmap -> LAST texcoord set of the vertex declaration, provided it is not
    already used by a layer (verified: last set is non-overlapping in ~100% of files)."""
    layers, _ = staticmesh_layers(mat.technique)
    has_dirt = 'Dirt' in layers
    ch = {}
    for l in layers:
        ch[l] = {'Base': 0, 'Detail': 1, 'NDetail': 1, 'Dirt': 2}.get(l, 3 if has_dirt else 2)
    sets = mesh.texcoord_sets()
    if sets and sets[-1] > max(ch.values(), default=0):
        ch['LightMap'] = sets[-1]
    return ch


def texture_slots(mesh, mat):
    """Assign each texture path of a material to a semantic slot."""
    maps = [p for p in mat.maps if 'specularlut' not in p.lower()]
    if mesh.kind == 'staticmesh':
        layers, _ = staticmesh_layers(mat.technique)
        return dict(zip(layers, maps))
    out = {}
    if maps:
        out['Color'] = maps[0]
    rest = maps[1:]
    if mesh.kind == 'skinnedmesh':
        if rest:
            out['Normal'] = rest[0]     # *_b = tangent space ('tangent' in technique), *_b_os = object space
        return out
    # bundledmesh: [Color, (Normal), SpecularLUT, (Wreck)]; guess by suffix like bf2-blender
    for p in rest:
        stem = os.path.splitext(p.replace('\\', '/').rsplit('/', 1)[-1])[0].lower()
        if stem.endswith('_b') or stem.endswith('_b_os'):
            out['Normal'] = p
        else:
            out['Wreck'] = p
    return out


# --------------------------------------------------------------------------
# Compiled road mesh (.mesh in Levels/*/client.zip, Roads/<name>_compiled.mesh)
# --------------------------------------------------------------------------
class RoadMesh:
    def __init__(self):
        self.version = 0       # u16, 4 for every mesh referenced by CompiledRoads.con
        self.flags = 0         # u16, always 1
        self.position = None   # world position == 'object.absoluteposition' in CompiledRoads.con
        self.radius = 0.0      # max |local vertex|
        self.bmin = self.bmax = None   # world-space AABB
        self.unknown = 0       # u32 0..10, meaning unknown (not the template SetPrio)
        self.vertices = []     # v4: (x,y,z, u0,v0, u1,v1, alpha) local to position
        self.indices = None
        self.patches = []      # v4: (istart, icount, radius, cx, cy, cz) world-space culling spheres


def parse_road_mesh(data):
    r = Reader(data)
    m = RoadMesh()
    m.version = r.u16()
    m.flags = r.u16()
    if m.flags != 1 or m.version > 4:
        raise BF2FormatError(f'not a compiled road mesh (header {m.version},{m.flags})')
    m.position = r.vec3()
    m.radius = r.f32()
    m.bmin = r.vec3()
    m.bmax = r.vec3()
    m.unknown = r.u32()
    nv = r.count(1 << 20, 'road vertex')
    if m.version == 4:
        m.vertices = [r.fmt('8f') for _ in range(nv)]          # 32-byte vertex (RoadCompiled.fx)
    elif m.version == 2:
        # legacy, unreferenced: 56-byte vertex pos3, side-vector3, uv0, uv1, 4 floats (0)
        m.vertices = [r.fmt('14f') for _ in range(nv)]
    else:
        raise BF2FormatError(f'road mesh version {m.version} not supported (legacy, unreferenced)')
    ni = r.count(1 << 22, 'road index')
    m.indices = array('H')
    m.indices.frombytes(r.raw(ni * 2))
    if m.version == 4:
        npatch = r.count(4096, 'road patch')
        m.patches = [r.fmt('2I4f') for _ in range(npatch)]
    if not r.eof():
        raise BF2FormatError(f'{len(data) - r.p} trailing bytes')
    return m


# --------------------------------------------------------------------------
# validation
# --------------------------------------------------------------------------
def validate_visible_mesh(m):
    """Return (errors, warnings, stats) for sanity checks."""
    errs, warns = [], []
    st = Counter()
    used = [e for e in m.elements if not e.unused]
    if m.vertformat != 4:
        errs.append(f'vertformat {m.vertformat} != 4')
    end = 0
    for e in used:
        if e.type not in D3DDECLTYPE:
            errs.append(f'unknown decl type {e.type}')
        end = max(end, e.offset + e.size)
    if end != m.stride:
        errs.append(f'decl end {end} != stride {m.stride}')
    if not m.elements or not m.elements[-1].unused:
        warns.append('decl not terminated with UNUSED element')
    if m.kind != 'skinnedmesh' and m.alpha_sets != 8:
        st['alpha_sets_not_8'] += 1

    f = m.floats()
    sw = m.stride // 4
    pos, nrm, tan = m.element(USAGE_POSITION), m.element(USAGE_NORMAL), m.element(USAGE_TANGENT)
    bw, bi = m.element(USAGE_BLENDWEIGHT), m.element(USAGE_BLENDINDICES)
    sets = m.texcoord_sets()
    if pos is None:
        errs.append('no POSITION')
        return errs, warns, st
    po = pos.offset // 4
    nvtx = m.vnum
    for v in range(nvtx):
        b = v * sw + po
        x, y, z = f[b], f[b + 1], f[b + 2]
        if not (math.isfinite(x) and math.isfinite(y) and math.isfinite(z)) or max(abs(x), abs(y), abs(z)) > 1e5:
            st['bad_pos'] += 1
    if nrm is not None:
        no = nrm.offset // 4
        for v in range(nvtx):
            b = v * sw + no
            l = math.sqrt(f[b] * f[b] + f[b + 1] * f[b + 1] + f[b + 2] * f[b + 2])
            if abs(l - 1.0) > 0.02:
                st['zero_normal' if l < 1e-6 else 'nonunit_normal'] += 1
    if tan is not None:
        to = tan.offset // 4
        for v in range(nvtx):
            b = v * sw + to
            l = math.sqrt(f[b] * f[b] + f[b + 1] * f[b + 1] + f[b + 2] * f[b + 2])
            if abs(l - 1.0) > 0.02:
                st['zero_tangent' if l < 1e-6 else 'nonunit_tangent'] += 1
    for s in sets:
        uo = m.element(USAGE_TEXCOORD(s)).offset // 4
        last_lm = (m.kind == 'staticmesh' and s == sets[-1] and len(sets) >= 3)
        for v in range(nvtx):
            b = v * sw + uo
            u_, v_ = f[b], f[b + 1]
            if not (math.isfinite(u_) and math.isfinite(v_)):
                st['nan_uv'] += 1
            elif abs(u_) > 64 or abs(v_) > 64:
                st['uv_abs_gt_64'] += 1
            if last_lm and not (-0.001 <= u_ <= 1.001 and -0.001 <= v_ <= 1.001):
                st['lightmap_uv_outside_01'] += 1
    if bw is not None:
        wo = bw.offset // 4
        for v in range(nvtx):
            w = f[v * sw + wo]
            if not (-1e-4 <= w <= 1.0001):
                st['bad_weight'] += 1

    nidx = len(m.indices)
    covered = 0
    for gi, li, lod in m.lods():
        if m.kind == 'skinnedmesh' and len(lod.rigs) != len(lod.materials):
            if 0 < len(lod.rigs) < len(lod.materials):
                warns.append(f'G{gi}L{li}: {len(lod.rigs)} rigs for {len(lod.materials)} materials (last rig reused)')
            else:
                errs.append(f'G{gi}L{li}: rigs {len(lod.rigs)} != materials {len(lod.materials)}')
        # LOD bounds must equal the vertex AABB of the LOD's materials
        mn = [1e30] * 3; mx = [-1e30] * 3
        for mat in lod.materials:
            for v in range(mat.vstart, min(mat.vstart + mat.vnum, nvtx)):
                for k in range(3):
                    x = f[v * sw + po + k]
                    if x < mn[k]: mn[k] = x
                    if x > mx[k]: mx[k] = x
        if lod.materials and any(mat.vnum for mat in lod.materials):
            if any(abs(mn[k] - lod.bmin[k]) > 1e-3 or abs(mx[k] - lod.bmax[k]) > 1e-3 for k in range(3)):
                st['lod_bounds_mismatch'] += 1
        for mi, mat in enumerate(lod.materials):
            if m.kind != 'skinnedmesh' and mat.alpha_mode not in ALPHA_MODES:
                errs.append(f'bad alpha mode {mat.alpha_mode}')
            if mat.vstart + mat.vnum > nvtx:
                errs.append(f'G{gi}L{li}M{mi}: vertex range {mat.vstart}+{mat.vnum} > {nvtx}')
                continue
            nsets = m.alpha_sets if (mat.alpha_mode == 1 and m.alpha_sets) else 1
            if mat.inum % 3:
                errs.append(f'G{gi}L{li}M{mi}: inum {mat.inum} not multiple of 3')
            if mat.istart + mat.inum * nsets > nidx:
                errs.append(f'G{gi}L{li}M{mi}: index range {mat.istart}+{mat.inum}x{nsets} > {nidx}')
                continue
            covered += mat.inum * nsets
            seg = m.indices[mat.istart: mat.istart + mat.inum * nsets]
            if seg and max(seg) >= mat.vnum:
                errs.append(f'G{gi}L{li}M{mi}: index {max(seg)} >= material vnum {mat.vnum}')
                continue
            # winding vs. vertex normal: cross(v1-v0, v2-v0) . n  (in file space)
            if nrm is not None and mat.inum:
                no = nrm.offset // 4
                vs = mat.vstart
                agree = disagree = 0
                for t in range(0, mat.inum - 2, 3):
                    a, b_, c = (seg[t] + vs) * sw, (seg[t + 1] + vs) * sw, (seg[t + 2] + vs) * sw
                    e1 = (f[b_ + po] - f[a + po], f[b_ + po + 1] - f[a + po + 1], f[b_ + po + 2] - f[a + po + 2])
                    e2 = (f[c + po] - f[a + po], f[c + po + 1] - f[a + po + 1], f[c + po + 2] - f[a + po + 2])
                    cx = e1[1] * e2[2] - e1[2] * e2[1]
                    cy = e1[2] * e2[0] - e1[0] * e2[2]
                    cz = e1[0] * e2[1] - e1[1] * e2[0]
                    d = (cx * (f[a + no] + f[b_ + no] + f[c + no]) + cy * (f[a + no + 1] + f[b_ + no + 1] + f[c + no + 1])
                         + cz * (f[a + no + 2] + f[b_ + no + 2] + f[c + no + 2]))
                    if d > 0:
                        agree += 1
                    elif d < 0:
                        disagree += 1
                st['winding_agrees_with_normal'] += agree
                st['winding_disagrees_with_normal'] += disagree
            if mat.bmin is not None and mat.vnum and m.version == 11:
                mmn = [min(f[v * sw + po + k] for v in range(mat.vstart, mat.vstart + mat.vnum)) for k in range(3)]
                mmx = [max(f[v * sw + po + k] for v in range(mat.vstart, mat.vstart + mat.vnum)) for k in range(3)]
                if any(abs(mmn[k] - mat.bmin[k]) > 1e-3 or abs(mmx[k] - mat.bmax[k]) > 1e-3 for k in range(3)):
                    st['material_bounds_mismatch'] += 1
            # blend indices
            if bi is not None:
                bo = bi.offset
                nb = 0
                if m.kind == 'skinnedmesh' and lod.rigs:
                    nb = len(lod.rigs[min(mi, len(lod.rigs) - 1)].bones)
                for v in range(mat.vstart, mat.vstart + mat.vnum):
                    i0, i1, i2, i3 = m.vbuf[v * m.stride + bo: v * m.stride + bo + 4]
                    if i2 not in (0, 1):
                        st['binormal_flip_not_01'] += 1
                    if m.kind == 'skinnedmesh':
                        if nb and (i0 >= nb or i1 >= nb):
                            st['bone_index_outside_rig'] += 1
                    elif m.kind == 'bundledmesh':
                        if i0 >= max(lod.node_count, 1):
                            st['part_index_outside_node_count'] += 1
                        if i3 > 6:
                            st['animuv_index_gt_6'] += 1
        if m.kind == 'skinnedmesh':
            for rig in lod.rigs:
                for bid, _ in rig.bones:
                    if bid > 255:
                        errs.append(f'bone id {bid}')
    if covered != nidx:
        st['unreferenced_indices'] += nidx - covered
    return errs, warns, st


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------
BF2 = os.environ.get('BF2_DIR', r'C:\Program Files (x86)\EA Games\Battlefield 2')
MESH_ARCHIVES = [
    r'mods\bf2\Objects_client.zip',
    r'mods\xpack\Objects_client.zip',
    r'mods\bf2\Booster_client.zip',
    r'mods\bf2\Common_client.zip',
]


def iter_members(archives, exts):
    for a in archives:
        p = a if os.path.isabs(a) else os.path.join(BF2, a)
        if not os.path.exists(p):
            continue
        z = zipfile.ZipFile(p)
        for info in z.infolist():
            if info.filename.lower().endswith(exts):
                yield a, z, info


def run_all():
    exts = ('.staticmesh', '.bundledmesh', '.skinnedmesh')
    ok = Counter(); parsed = Counter(); total = Counter()
    versions = Counter()
    failures = defaultdict(list); warnings = []
    agg = defaultdict(Counter)
    decls = Counter()
    for a, z, info in iter_members(MESH_ARCHIVES, exts):
        kind = detect_kind(info.filename)
        key = (a, kind)
        total[key] += 1
        data = z.read(info)
        try:
            m = parse_visible_mesh(data, kind)
        except Exception as e:  # noqa
            failures[key].append((info.filename, 'PARSE: ' + repr(e)))
            continue
        parsed[key] += 1
        errs, warns, st = validate_visible_mesh(m)
        versions[(kind, m.version, m.head[2:4])] += 1
        decls[(kind, tuple((e.usage_name, e.type_name) for e in m.elements if not e.unused))] += 1
        for k, v in st.items():
            agg[kind][k] += v
        if warns:
            warnings.append((info.filename, warns))
        if errs:
            failures[key].append((info.filename, 'VALIDATION: ' + '; '.join(errs[:3])))
        else:
            ok[key] += 1
    print('== visible meshes: parsed-to-EOF / passed validation / total ==')
    for key in sorted(total):
        print(f'  {key[0]:32s} {key[1]:12s} {parsed[key]:5d} / {ok[key]:5d} / {total[key]:5d}')
    print('== versions (kind, version, head[2:4]) ==')
    for k, v in sorted(versions.items()):
        print('  ', k, v)
    print('== aggregate stats ==')
    for kind, st in agg.items():
        print('  ', kind, dict(sorted(st.items())))
    print('== vertex declarations ==')
    for k, v in sorted(decls.items(), key=lambda x: -x[1]):
        print('  ', v, k[0], ' '.join(f'{u}:{t}' for u, t in k[1]))
    print('== warnings ==')
    for w in warnings:
        print('  ', w)
    print('== failures ==')
    for key, lst in failures.items():
        for n, e in lst:
            print('  ', key, n, e)


def run_roads():
    tot = Counter(); ok = Counter(); refd = Counter(); fails = []
    st = Counter()
    import re
    for sp in glob.glob(os.path.join(BF2, 'mods', '*', 'Levels', '*', 'server.zip')):
        lvl = os.path.dirname(sp)
        cp = os.path.join(lvl, 'client.zip')
        if not os.path.exists(cp):
            continue
        zs, zc = zipfile.ZipFile(sp), zipfile.ZipFile(cp)
        refs = {}
        for n in zs.namelist():
            if n.lower().endswith('compiledroads.con'):
                t = zs.read(n).decode('latin-1')
                for mesh, pos in re.findall(r'(?im)loadMesh\s+(\S+)\s+object\.absoluteposition\s+(\S+)', t):
                    refs[os.path.basename(mesh.replace('\\', '/')).lower()] = tuple(float(x) for x in pos.split('/'))
        for info in zc.infolist():
            fn = os.path.basename(info.filename).lower()
            if not fn.endswith('.mesh'):
                continue
            is_ref = fn in refs
            k = 'referenced' if is_ref else 'unreferenced'
            tot[k] += 1
            try:
                m = parse_road_mesh(zc.read(info))
            except Exception as e:
                fails.append((k, os.path.basename(lvl), info.filename, repr(e)))
                continue
            ok[k] += 1
            st[('version', m.version, k)] += 1
            if m.version == 4:
                if m.indices and max(m.indices) >= len(m.vertices):
                    st['index_oob'] += 1
                if is_ref and max(abs(a - b) for a, b in zip(m.position, refs[fn])) > 0.01:
                    st['position_ne_absoluteposition'] += 1
                if sum(p[1] for p in m.patches) != len(m.indices):
                    st['patches_do_not_cover_indices'] += 1
                rr = max(math.sqrt(v[0] ** 2 + v[1] ** 2 + v[2] ** 2) for v in m.vertices)
                if abs(rr - m.radius) > 0.05:
                    st['radius_mismatch'] += 1
    print('== road meshes: parsed / total ==')
    for k in tot:
        print(f'  {k:12s} {ok[k]:5d} / {tot[k]:5d}')
    print('  stats', dict(st))
    for f_ in fails[:20]:
        print('  FAIL', f_)
    if len(fails) > 20:
        print('   ...', len(fails) - 20, 'more')


def dump(zpath, member):
    z = zipfile.ZipFile(zpath)
    data = z.read(member)
    kind = detect_kind(member)
    m = parse_visible_mesh(data, kind)
    print(f'{member}: {kind} version={m.version} head={m.head} u8={m.u8flag}')
    print(f'  elements: {m.elements}')
    print(f'  vertformat={m.vertformat} stride={m.stride} vnum={m.vnum} inum={len(m.indices)} alpha_sets={m.alpha_sets}')
    for gi, li, lod in m.lods():
        print(f'  G{gi}L{li}: bounds {lod.bmin} {lod.bmax} radius={lod.radius} nodes={lod.node_count} '
              f'nmat={len(lod.materials)} rigs={[len(r.bones) for r in lod.rigs]}')
        for mat4 in lod.nodes[:4]:
            print('      node', mat4)
        for rig in lod.rigs[:2]:
            for bid, mat4 in rig.bones[:3]:
                print('      bone', bid, mat4)
        for mat in lod.materials:
            print(f'    mat alpha={mat.alpha_mode} fx={mat.fx} tech={mat.technique!r} v={mat.vstart}+{mat.vnum} '
                  f'i={mat.istart}+{mat.inum} bounds={mat.bmin} {mat.bmax}')
            print('       slots', texture_slots(m, mat))
            if kind == 'staticmesh':
                print('       uv channels', staticmesh_uv_channels(m, mat))
    for usage in [0, 3, 6, 1, 2, 5, 0x105, 0x205, 0x305, 0x405]:
        vals = m.attribute(usage, 0, min(3, m.vnum))
        if vals:
            print('  ', m.element(usage), vals)


if __name__ == '__main__':
    if len(sys.argv) >= 4 and sys.argv[1] == '--dump':
        dump(sys.argv[2], sys.argv[3])
    elif len(sys.argv) >= 2 and sys.argv[1] == '--roads':
        run_roads()
    else:
        run_all()
