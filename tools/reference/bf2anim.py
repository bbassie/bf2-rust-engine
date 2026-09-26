#!/usr/bin/env python3
"""
bf2anim.py - reference parser for Battlefield 2 skeletons (.ske) and
animations (.baf).  Standalone, stdlib only, little-endian.

    python bf2anim.py                      # parse all .ske/.baf in the install, print stats
    python bf2anim.py --ske <zip> <member> # dump a skeleton
    python bf2anim.py --baf <zip> <member> # dump an animation
    python bf2anim.py --selftest           # verify conventions against real data

Conventions (VERIFIED against skinnedmesh inverse-bind matrices, see mesh_formats.md):
  * .ske and .baf store quaternions (x, y, z, w) of the INVERSE (conjugate)
    rotation.  The bone's local rotation, in ordinary column-vector math, is
    q = conj(q_file) = (-x, -y, -z, w):
        p_parent = R(conj(q_file)) * p_local + pos          (column vectors)
    or, D3D row-vector style:
        M_local = D3DXMatrixRotationQuaternion(conj(q_file)) * Translation(pos)
        M_world = M_local * M_world(parent)
  * .ske transforms are parent-relative rest transforms (object space for roots).
  * .baf keys are parent-relative transforms per frame, same convention, and
    replace (not add to) the rest transform of the listed bones.
"""
import math
import os
import struct
import sys
import zipfile
from collections import Counter, defaultdict


class BF2FormatError(Exception):
    pass


# --------------------------------------------------------------------------
# .ske
# --------------------------------------------------------------------------
class SkeNode:
    __slots__ = ('index', 'name', 'parent', 'rot', 'pos')

    def __init__(self, index, name, parent, rot, pos):
        self.index, self.name, self.parent, self.rot, self.pos = index, name, parent, rot, pos

    def __repr__(self):
        return f'<{self.index} {self.name!r} parent={self.parent} rot={self.rot} pos={self.pos}>'


def parse_ske(data):
    """version u32 (=2), count u32, then per node:
       u16 name_len (incl. trailing NUL), name bytes, i16 parent (-1 = root),
       f32 qx,qy,qz,qw, f32 px,py,pz."""
    p = 0
    ver, n = struct.unpack_from('<2I', data, p); p += 8
    if ver != 2:
        raise BF2FormatError(f'ske version {ver}')
    nodes = []
    for i in range(n):
        ln, = struct.unpack_from('<H', data, p); p += 2
        raw = data[p:p + ln]; p += ln
        name = raw.split(b'\0', 1)[0].decode('latin-1')
        parent, = struct.unpack_from('<h', data, p); p += 2
        rot = struct.unpack_from('<4f', data, p); p += 16
        pos = struct.unpack_from('<3f', data, p); p += 12
        nodes.append(SkeNode(i, name, parent, rot, pos))
    if p != len(data):
        raise BF2FormatError(f'{len(data) - p} trailing bytes')
    return nodes


# --------------------------------------------------------------------------
# .baf
# --------------------------------------------------------------------------
def dequant(word, precision):
    """16-bit fixed point -> float.  value = int16 * 2^(15-precision) / 32767.
    (bf2-blender uses 'if w > 32767: w -= 0xFFFF', i.e. off by one versus a
    proper int16 for negatives; the difference is 1 LSB.)"""
    if word >= 0x8000:
        word -= 0x10000
    return word * float(1 << (15 - precision)) / 32767.0


class Baf:
    def __init__(self):
        self.version = 0
        self.bone_ids = []
        self.frames = 0
        self.precision = 0
        # tracks[bone_index][channel] = list of raw u16 per frame, channel 0..6 = qx,qy,qz,qw,px,py,pz
        self.raw = []
        self.data_sizes = []
        self.stream_stats = Counter()

    def rot(self, bone_i, frame):
        r = self.raw[bone_i]
        return tuple(dequant(r[c][frame], 15) for c in range(4))

    def pos(self, bone_i, frame):
        r = self.raw[bone_i]
        return tuple(dequant(r[c][frame], self.precision) for c in range(4, 7))


def parse_baf(data):
    """
    u32 version (=4)
    u16 bone_count
    u16 bone_ids[bone_count]           (indices into the .ske node list)
    u32 frame_count
    u8  precision                      (fraction bits for positions)
    per bone:
        u16 data_size                  (sum of the 7 stream sizes below, in 16-bit words)
        7 x stream (qx, qy, qz, qw, px, py, pz):
            u16 stream_size            (in 16-bit words, including the 1-word block headers)
            blocks until stream_size consumed:
                u8 head   : bit7 = RLE flag, bits0-6 = number of frames n (1..127)
                u8 next   : size of this block in words (RLE: 2, raw: n+1)
                RLE : u16 value, repeated for n frames
                raw : n x u16 values
    """
    b = Baf()
    p = 0
    b.version, = struct.unpack_from('<I', data, p); p += 4
    if b.version != 4:
        raise BF2FormatError(f'baf version {b.version}')
    nb, = struct.unpack_from('<H', data, p); p += 2
    b.bone_ids = list(struct.unpack_from(f'<{nb}H', data, p)); p += 2 * nb
    b.frames, = struct.unpack_from('<I', data, p); p += 4
    b.precision = data[p]; p += 1
    F = b.frames
    for bi in range(nb):
        dsize, = struct.unpack_from('<H', data, p); p += 2
        b.data_sizes.append(dsize)
        chans = []
        total = 0
        for c in range(7):
            left, = struct.unpack_from('<H', data, p); p += 2
            total += left
            vals = []
            while left > 0:
                head = data[p]; nxt = data[p + 1]; p += 2
                rle = head & 0x80
                n = head & 0x7F
                if rle:
                    v, = struct.unpack_from('<H', data, p); p += 2
                    vals.extend([v] * n)
                    b.stream_stats['rle_blocks'] += 1
                    if nxt != 2:
                        b.stream_stats['rle_next_not_2'] += 1
                else:
                    vals.extend(struct.unpack_from(f'<{n}H', data, p)); p += 2 * n
                    b.stream_stats['raw_blocks'] += 1
                    if nxt != n + 1:
                        b.stream_stats['raw_next_not_n+1'] += 1
                left -= nxt
            if left != 0:
                raise BF2FormatError(f'bone {bi} ch {c}: stream size underflow {left}')
            if len(vals) != F:
                if len(vals) > F:
                    raise BF2FormatError(f'bone {bi} ch {c}: {len(vals)} values > {F} frames')
                b.stream_stats['short_stream'] += 1
                vals.extend([vals[-1] if vals else 0] * (F - len(vals)))
            chans.append(vals)
        if total != dsize:
            b.stream_stats['data_size_mismatch'] += 1
        b.raw.append(chans)
    if p != len(data):
        raise BF2FormatError(f'{len(data) - p} trailing bytes at 0x{p:x}')
    return b


# --------------------------------------------------------------------------
# math helpers (row-vector D3D convention, matrices as 4x4 lists [row][col])
# --------------------------------------------------------------------------
def quat_to_mat_d3dx(q):
    """D3DXMatrixRotationQuaternion: row-vector rotation matrix (v' = v * M)."""
    x, y, z, w = q
    return [[1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w), 0],
            [2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w), 0],
            [2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y), 0],
            [0, 0, 0, 1]]


def mat_mul(a, b):
    return [[sum(a[i][k] * b[k][j] for k in range(4)) for j in range(4)] for i in range(4)]


def local_matrix(rot, pos):
    m = quat_to_mat_d3dx(rot)
    m[3][0], m[3][1], m[3][2] = pos
    return m


def world_matrices(nodes):
    """NAIVE variant using q_file unconjugated -- WRONG, kept only so selftest()
    can show the difference.  Use world_matrices_file_convention()."""
    W = [None] * len(nodes)
    for n in nodes:  # parents always precede children in BF2 skeletons (verified)
        L = local_matrix(n.rot, n.pos)
        W[n.index] = L if n.parent < 0 else mat_mul(L, W[n.parent])
    return W


def world_matrices_file_convention(nodes):
    """Correct rest-pose world matrices (row-vector) using conj(q_file)."""
    W = [None] * len(nodes)
    for n in nodes:
        q = (-n.rot[0], -n.rot[1], -n.rot[2], n.rot[3])
        L = local_matrix(q, n.pos)
        W[n.index] = L if n.parent < 0 else mat_mul(L, W[n.parent])
    return W


def quat_to_gltf(q_file):
    """.ske/.baf quaternion (x,y,z,w) -> glTF rotation (x,y,z,w) for the
    X-mirror conversion  (x,y,z)_bf2 -> (-x,y,z)_gltf  (see mesh_formats.md)."""
    x, y, z, w = q_file
    return (-x, y, z, w)


def pos_to_gltf(p):
    return (-p[0], p[1], p[2])


def mat_inv_affine(m):
    """inverse of a row-vector rigid transform."""
    r = [[m[j][i] for j in range(3)] for i in range(3)]  # transpose of 3x3
    t = m[3][:3]
    it = [-(t[0] * r[0][j] + t[1] * r[1][j] + t[2] * r[2][j]) for j in range(3)]
    return [r[0] + [0], r[1] + [0], r[2] + [0], it + [1]]


# --------------------------------------------------------------------------
# CLI / validation
# --------------------------------------------------------------------------
BF2 = os.environ.get('BF2_DIR', r'C:\Program Files (x86)\EA Games\Battlefield 2')
ARCHIVES = [r'mods\bf2\Objects_server.zip', r'mods\xpack\Objects_server.zip', r'mods\bf2\Booster_server.zip',
            r'mods\bf2\Objects_client.zip']


def run_all():
    skes = {}
    tot = Counter(); ok = Counter(); fails = defaultdict(list)
    st = Counter()
    prec = Counter(); frames = []
    maxbone = Counter()
    for a in ARCHIVES:
        z = zipfile.ZipFile(os.path.join(BF2, a))
        for info in z.infolist():
            n = info.filename.lower()
            if n.endswith('.ske'):
                tot[(a, 'ske')] += 1
                try:
                    nodes = parse_ske(z.read(info))
                    bad = [x for x in nodes if not (-1 <= x.parent < x.index)]
                    # weapon-part bones 'mesh1'..'mesh16' of 1p/3p_setup carry uninitialised
                    # garbage transforms (driven purely by animation) -> excluded
                    garbage = [x for x in nodes if x.name.startswith('mesh') and abs(math.sqrt(sum(c * c for c in x.rot)) - 1) > 1e-3]
                    st['ske_garbage_mesh_bones'] += len(garbage)
                    qbad = [x for x in nodes if x not in garbage and abs(math.sqrt(sum(c * c for c in x.rot)) - 1) > 1e-3]
                    if bad or qbad:
                        fails[(a, 'ske')].append((info.filename, f'parent-order violations {len(bad)} non-unit quats {len(qbad)}'))
                    else:
                        ok[(a, 'ske')] += 1
                    skes[os.path.basename(n)] = nodes
                except Exception as e:
                    fails[(a, 'ske')].append((info.filename, repr(e)))
    for a in ARCHIVES:
        z = zipfile.ZipFile(os.path.join(BF2, a))
        for info in z.infolist():
            n = info.filename.lower()
            if not n.endswith('.baf'):
                continue
            tot[(a, 'baf')] += 1
            try:
                b = parse_baf(z.read(info))
            except Exception as e:
                fails[(a, 'baf')].append((info.filename, repr(e)))
                continue
            st.update(b.stream_stats)
            prec[b.precision] += 1
            frames.append(b.frames)
            errs = []
            # quaternion length check
            worst = 0.0
            for bi in range(len(b.bone_ids)):
                for f in range(0, b.frames, max(1, b.frames // 8)):
                    q = b.rot(bi, f)
                    worst = max(worst, abs(math.sqrt(sum(c * c for c in q)) - 1))
            if worst > 0.01:
                errs.append(f'quat length error {worst:.4f}')
                st['bad_quat_files'] += 1
            st['max_quat_len_err_x1e4'] = max(st['max_quat_len_err_x1e4'], int(worst * 1e4))
            # bone id range vs. skeleton
            mb = max(b.bone_ids) if b.bone_ids else 0
            if 'soldiers/' in n or '/animations/1p' in n or '/animations/3p' in n or 'weapons/handheld' in n:
                sk = '1p_setup.ske' if '/1p' in n or '1p_' in os.path.basename(n) else '3p_setup.ske'
                if sk in skes and mb >= len(skes[sk]):
                    errs.append(f'bone id {mb} >= {sk} size {len(skes[sk])}')
            if len(set(b.bone_ids)) != len(b.bone_ids):
                st['duplicate_bone_ids'] += 1
            maxbone[mb] += 1
            if errs:
                fails[(a, 'baf')].append((info.filename, '; '.join(errs)))
            else:
                ok[(a, 'baf')] += 1
    print('== parse + validation success ==')
    for k in sorted(tot):
        print(f'  {k[0]:34s} {k[1]:4s} {ok[k]:5d}/{tot[k]:5d}')
    print('== baf stats ==', dict(st))
    print('   precision histogram', dict(sorted(prec.items())))
    frames.sort()
    print('   frames min/median/max', frames[0], frames[len(frames) // 2], frames[-1])
    print('== skeletons ==')
    for k, v in skes.items():
        print('  ', k, len(v), 'bones; roots:', [x.name for x in v if x.parent < 0][:4])
    print('== failures ==')
    for k, l in fails.items():
        for x in l[:20]:
            print('  ', k, x)
        if len(l) > 20:
            print('   ...', len(l) - 20, 'more')


def selftest():
    """1) rest pose (conj convention) reproduces the skinnedmesh inverse-bind matrices
       2) quat_to_gltf() equals S*R*S for S = diag(-1,1,1)."""
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import bf2mesh
    z = zipfile.ZipFile(os.path.join(BF2, 'mods', 'bf2', 'Objects_client.zip'))
    for ske, mesh in (('soldiers/Common/Animations/3p_setup.ske', 'soldiers/Us/Meshes/us_light_soldier.skinnedmesh'),
                      ('soldiers/Common/Animations/1p_setup.ske', 'soldiers/Us/Meshes/us_light_soldier.skinnedmesh'),
                      ('Common/Flags/flag_setup.ske', 'Common/Flags/flag_US/meshes/flag_us.skinnedmesh')):
        nodes = parse_ske(z.read(ske))
        good = world_matrices_file_convention(nodes)
        naive = world_matrices(nodes)
        m = bf2mesh.parse_visible_mesh(z.read(mesh), 'skinnedmesh')
        for gi, g in enumerate(m.geoms):
            lod = g[0]
            dg, dn = [], []
            for rig in lod.rigs:
                for bid, mat in rig.bones:
                    if bid >= len(nodes) or nodes[bid].name.startswith('mesh'):
                        continue
                    inv = mat_inv_affine(mat)
                    dg.append(max(abs(inv[i][j] - good[bid][i][j]) for i in range(4) for j in range(3)))
                    dn.append(max(abs(inv[i][j] - naive[bid][i][j]) for i in range(4) for j in range(3)))
            if dg:
                dg.sort(); dn.sort()
                print(f'{os.path.basename(mesh)} G{gi} vs {os.path.basename(ske)}: bones={len(dg)} '
                      f'conj(q): median {dg[len(dg)//2]:.5f} max {dg[-1]:.4f} | raw q: median {dn[len(dn)//2]:.4f}')
    # glTF quaternion check
    import random
    random.seed(1)
    worst = 0
    for _ in range(1000):
        q = [random.uniform(-1, 1) for _ in range(4)]
        l = math.sqrt(sum(c * c for c in q)); q = [c / l for c in q]
        R = quat_to_mat_d3dx((-q[0], -q[1], -q[2], q[3]))       # row-vector matrix of the true rotation
        S = [[-1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]]
        RS = mat_mul(mat_mul(S, R), S)
        g = quat_to_gltf(q)                                        # glTF quat, standard (column) convention
        G = quat_to_mat_d3dx(g)                                    # row-vector matrix of rotation g = transpose(R_col(g))
        worst = max(worst, max(abs(RS[i][j] - G[i][j]) for i in range(3) for j in range(3)))
    print('quat_to_gltf max matrix error:', worst)


if __name__ == '__main__':
    if len(sys.argv) >= 2 and sys.argv[1] == '--selftest':
        selftest()
        sys.exit(0)
    if len(sys.argv) >= 4 and sys.argv[1] == '--ske':
        for node in parse_ske(zipfile.ZipFile(sys.argv[2]).read(sys.argv[3])):
            print(node)
    elif len(sys.argv) >= 4 and sys.argv[1] == '--baf':
        b = parse_baf(zipfile.ZipFile(sys.argv[2]).read(sys.argv[3]))
        print('bones', b.bone_ids, 'frames', b.frames, 'precision', b.precision, dict(b.stream_stats))
        for bi, bid in enumerate(b.bone_ids[:6]):
            print(' bone', bid, 'f0 rot', b.rot(bi, 0), 'pos', b.pos(bi, 0))
    else:
        run_all()
