#!/usr/bin/env python3
"""
bf2coll.py - reference parser for Battlefield 2 .collisionmesh files.

Standalone, stdlib only, little-endian.

    python bf2coll.py                          # parse all collision meshes in the install, print stats
    python bf2coll.py --dump <zip> <member>    # dump one file

See mesh_formats.md for the spec.
"""
import math
import os
import struct
import sys
import zipfile
from array import array
from collections import Counter, defaultdict

# col_type 4 exists in 192 shipped cols (fences, walls, poles, vegetation): always a
# tiny 1-15 face vertical 'blade' in the object's mid plane; purpose unknown
# (probably an AI / navmesh-generation helper).  bf2-blender only knows 0-3.
COL_TYPES = {0: 'PROJECTILE', 1: 'VEHICLE', 2: 'SOLDIER', 3: 'AI', 4: 'UNKNOWN4'}


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

    def u32(self):
        self.need(4); v = struct.unpack_from('<I', self.d, self.p)[0]; self.p += 4; return v

    def fmt(self, f):
        n = struct.calcsize('<' + f); self.need(n)
        v = struct.unpack_from('<' + f, self.d, self.p); self.p += n; return v

    def arr(self, typecode, n):
        a = array(typecode)
        sz = a.itemsize * n
        self.need(sz)
        a.frombytes(self.d[self.p:self.p + sz])
        if sys.byteorder != 'little':
            a.byteswap()
        self.p += sz
        return a

    def count(self, limit, what):
        n = self.u32()
        if n > limit:
            raise BF2FormatError(f'implausible {what} count {n} at 0x{self.p - 4:x}')
        return n


class BSPNode:
    """16 bytes: f32 split, u32 flags, u32 a, u32 b.   (axis-aligned kd-tree)

    flags bits 0-1 : split axis (0=X, 1=Y, 2=Z)
    bit 2 (0x4)    : child 0 is a leaf -> a = first face-ref, count = (flags>>16)&0xFF
    bit 3 (0x8)    : child 1 is a leaf -> b = first face-ref, count = (flags>>24)&0xFF
    otherwise a / b are node indices.  Child 0 holds faces with coord[axis] <= split,
    child 1 faces with coord[axis] >= split; straddling faces are in both (verified).
    Node 0 is the root.
    """
    __slots__ = ('split', 'flags', 'a', 'b')

    def __init__(self, split, flags, a, b):
        self.split, self.flags, self.a, self.b = split, flags, a, b

    @property
    def axis(self):
        return self.flags & 3

    def is_leaf(self, side):
        return bool(self.flags & (4 << side))

    def leaf_count(self, side):
        return (self.flags >> (16 + 8 * side)) & 0xFF

    def child(self, side):
        return self.a if side == 0 else self.b


class BSP:
    def __init__(self):
        self.bmin = self.bmax = None
        self.nodes = []
        self.face_refs = None   # array('H') of face indices


class Col:
    def __init__(self):
        self.col_type = None     # u32 (v>=9); implicit = LOD index for v<9
        self.faces = None        # array('H'), 4 per face: v0, v1, v2, material
        self.verts = None        # array('f'), 3 per vertex
        self.vert_mats = None    # array('H'), one per vertex
        self.bmin = self.bmax = None
        self.bsp = None
        self.face_adj = None     # array('i'), 3 per face (v>=10)

    @property
    def nfaces(self):
        return len(self.faces) // 4

    @property
    def nverts(self):
        return len(self.verts) // 3


class CollisionMesh:
    def __init__(self):
        self.u0 = 0
        self.version = 0
        # geoms[part][geom][i] -> Col
        #   part = ObjectTemplate.collisionPart index (0 = root template)
        #   geom = same meaning as the visible-mesh geom index (vehicles: 0=1P (usually empty), 1=3P, 2=wreck)
        #   i    = list of cols, each with its own col_type (0 projectile, 1 vehicle, 2 soldier, 3 AI, 4 ?)
        self.geoms = []


def _parse_col(r, ver, lod_index):
    c = Col()
    if ver >= 9:
        c.col_type = r.u32()
    else:
        c.col_type = lod_index
    nf = r.count(1 << 20, 'face')
    c.faces = r.arr('H', nf * 4)
    nv = r.count(1 << 20, 'vertex')
    c.verts = r.arr('f', nv * 3)
    c.vert_mats = r.arr('H', nv)
    c.bmin = r.fmt('3f')
    c.bmax = r.fmt('3f')
    flag = r.u8()
    if flag not in (0x30, 0x31):
        raise BF2FormatError(f'bad BSP flag 0x{flag:02x} at 0x{r.p - 1:x}')
    if flag == 0x31:
        b = BSP()
        b.bmin = r.fmt('3f')
        b.bmax = r.fmt('3f')
        nn = r.count(1 << 20, 'bsp node')
        raw = r.arr('I', nn * 4)
        for i in range(nn):
            split = struct.unpack('<f', struct.pack('<I', raw[i * 4]))[0]
            b.nodes.append(BSPNode(split, raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]))
        nr = r.count(1 << 22, 'bsp face ref')
        b.face_refs = r.arr('H', nr)
        c.bsp = b
    if ver >= 10 and flag == 0x31:
        # face adjacency (3 x i32 per face, -1 = open edge).  Only present together
        # with the BSP: the single v10 LOD in the install without BSP
        # (xp1_powerswitch_base, col1) has no adjacency block either.
        na = r.count(1 << 22, 'adjacency')
        c.face_adj = r.arr('i', na)
    return c


def parse_collisionmesh(data):
    r = Reader(data)
    m = CollisionMesh()
    m.u0, m.version = r.fmt('2I')
    ver = m.version
    if ver < 8 or ver > 10:
        return _parse_v4(data, m)
    ng = r.count(256, 'geom')
    for _ in range(ng):
        nsub = r.count(256, 'subgeom')
        subs = []
        for _ in range(nsub):
            nlod = r.count(16, 'lod')
            subs.append([_parse_col(r, ver, li) for li in range(nlod)])
        m.geoms.append(subs)
    if r.p != len(data):
        raise BF2FormatError(f'{len(data) - r.p} trailing bytes at 0x{r.p:x}')
    return m


def _parse_v4(data, m):
    # v4 (2 files, HMG_M134_Barrels/Minigun) and v7 (aircontroltowerstatic, 2 copies)
    # are legacy formats with a different face/BSP layout.  None of them is
    # referenced by any ObjectTemplate/level, so they are not decoded.
    raise BF2FormatError(f'unsupported (legacy, unreferenced) collisionmesh version {m.version}')


# --------------------------------------------------------------------------
# validation
# --------------------------------------------------------------------------
def validate(m):
    errs = []
    st = Counter()
    for gi, subs in enumerate(m.geoms):
        for si, lods in enumerate(subs):
            st['cols_per_geom_%d' % len(lods)] += 1
            for li, c in enumerate(lods):
                tag = f'G{gi}S{si}L{li}'
                st['coltype_%s' % c.col_type] += 1
                # winding: collision faces are wound OPPOSITE to visible meshes
                # (cross(v1-v0, v2-v0) points into the solid)
                if m.version >= 9 and c.col_type != li:
                    st['coltype_ne_lodindex'] += 1
                nv, nf = c.nverts, c.nfaces
                for i in range(nf):
                    a, b, cc = c.faces[i * 4], c.faces[i * 4 + 1], c.faces[i * 4 + 2]
                    if a >= nv or b >= nv or cc >= nv:
                        errs.append(f'{tag}: face {i} index out of range')
                        break
                    if a == b or b == cc or a == cc:
                        st['degenerate_face'] += 1
                mats = set(c.faces[3::4])
                st['max_material'] = max(st['max_material'], max(mats) if mats else 0)
                if list(c.faces[3::4]) != sorted(c.faces[3::4]):
                    st['faces_not_sorted_by_material'] += 1
                # bounds
                if nv:
                    xs, ys, zs = c.verts[0::3], c.verts[1::3], c.verts[2::3]
                    mn = (min(xs), min(ys), min(zs)); mx = (max(xs), max(ys), max(zs))
                    if any(abs(mn[k] - c.bmin[k]) > 1e-3 or abs(mx[k] - c.bmax[k]) > 1e-3 for k in range(3)):
                        st['bounds_mismatch'] += 1
                    if not all(math.isfinite(v) for v in c.verts):
                        errs.append(f'{tag}: non-finite vertex')
                # vertex material ids vs face materials
                # (vert_mats is per-vertex material id; check it matches some face using the vertex)
                vm_ok = vm_bad = 0
                for i in range(nf):
                    mat = c.faces[i * 4 + 3]
                    for k in range(3):
                        if c.vert_mats[c.faces[i * 4 + k]] == mat:
                            vm_ok += 1
                        else:
                            vm_bad += 1
                st['vertmat_match'] += vm_ok
                st['vertmat_mismatch'] += vm_bad
                # BSP
                if c.bsp is None:
                    st['no_bsp'] += 1
                else:
                    b = c.bsp
                    nn = len(b.nodes)
                    nr = len(b.face_refs)
                    if nr and max(b.face_refs) >= nf:
                        errs.append(f'{tag}: bsp face ref out of range')
                    parents = [0] * nn
                    refs_used = 0
                    for n in b.nodes:
                        if n.axis == 3:
                            errs.append(f'{tag}: bsp axis 3')
                        for side in (0, 1):
                            if n.is_leaf(side):
                                s, cnt = n.child(side), n.leaf_count(side)
                                if s + cnt > nr:
                                    errs.append(f'{tag}: bsp leaf range {s}+{cnt} > {nr}')
                                refs_used += cnt
                            else:
                                ch = n.child(side)
                                if ch >= nn:
                                    errs.append(f'{tag}: bsp child {ch} >= {nn}')
                                else:
                                    parents[ch] += 1
                    roots = [i for i in range(nn) if parents[i] == 0]
                    if len(roots) != 1:
                        errs.append(f'{tag}: bsp roots {len(roots)}')
                    elif roots[0] != 0:
                        st['bsp_root_not_0'] += 1
                    if refs_used != nr:
                        st['bsp_refs_unaccounted'] += 1
                    # every face referenced at least once?
                    seen = set(b.face_refs)
                    if len(seen) != nf:
                        st['bsp_faces_missing'] += nf - len(seen)
                if c.face_adj is not None:
                    if len(c.face_adj) != 3 * nf:
                        errs.append(f'{tag}: adjacency len {len(c.face_adj)} != 3*{nf}')
                    elif c.face_adj and (max(c.face_adj) >= nf or min(c.face_adj) < -1):
                        errs.append(f'{tag}: adjacency value out of range')
    return errs, st


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------
BF2 = os.environ.get('BF2_DIR', r'C:\Program Files (x86)\EA Games\Battlefield 2')
ARCHIVES = [
    r'mods\bf2\Objects_client.zip',
    r'mods\bf2\Objects_server.zip',
    r'mods\xpack\Objects_server.zip',
    r'mods\bf2\Booster_server.zip',
]


def run_all():
    tot = Counter(); ok = Counter(); vers = Counter()
    fails = defaultdict(list)
    agg = Counter()
    per_ver_struct = Counter()
    for a in ARCHIVES:
        p = os.path.join(BF2, a)
        if not os.path.exists(p):
            continue
        z = zipfile.ZipFile(p)
        for info in z.infolist():
            if not info.filename.lower().endswith('.collisionmesh'):
                continue
            data = z.read(info)
            v = struct.unpack_from('<I', data, 4)[0]
            tot[a] += 1
            vers[(a, v)] += 1
            try:
                m = parse_collisionmesh(data)
                errs, st = validate(m)
            except Exception as e:
                fails[a].append((info.filename, v, repr(e)))
                continue
            agg.update({k: v2 for k, v2 in st.items() if k != 'max_material'})
            agg['max_material'] = max(agg['max_material'], st['max_material'])
            per_ver_struct[(v, len(m.geoms), tuple(len(s) for s in m.geoms),
                            tuple(tuple(len(l) for l in s) for s in m.geoms))] += 0
            if errs:
                fails[a].append((info.filename, v, 'VALIDATION ' + '; '.join(errs[:3])))
            else:
                ok[a] += 1
    print('== parse + validation success ==')
    for a in tot:
        print(f'  {a:34s} {ok[a]:5d}/{tot[a]:5d}')
    print('== versions ==')
    for k, v in sorted(vers.items()):
        print('  ', k, v)
    print('== aggregate ==')
    print('  ', dict(sorted(agg.items())))
    print('== failures ==')
    for a, l in fails.items():
        for x in l[:30]:
            print('  ', a, x)
        if len(l) > 30:
            print('   ...', len(l) - 30, 'more')


def dump(zpath, member):
    z = zipfile.ZipFile(zpath)
    m = parse_collisionmesh(z.read(member))
    print(member, 'version', m.version)
    for gi, subs in enumerate(m.geoms):
        for si, lods in enumerate(subs):
            for li, c in enumerate(lods):
                print(f'  G{gi}S{si}L{li} type={c.col_type}({COL_TYPES.get(c.col_type)}) faces={c.nfaces} '
                      f'verts={c.nverts} bounds={c.bmin} {c.bmax} mats={sorted(set(c.faces[3::4]))} '
                      f'bsp={"%d nodes/%d refs" % (len(c.bsp.nodes), len(c.bsp.face_refs)) if c.bsp else None} '
                      f'adj={len(c.face_adj) if c.face_adj is not None else None}')


if __name__ == '__main__':
    if len(sys.argv) >= 4 and sys.argv[1] == '--dump':
        dump(sys.argv[2], sys.argv[3])
    else:
        run_all()
