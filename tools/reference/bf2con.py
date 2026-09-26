#!/usr/bin/env python3
"""
bf2con.py - Battlefield 2 (Refractor 2) virtual file system + .con/.tweak/.inc/.ai
script tokenizer and *static* interpreter.  Python 3.10 stdlib only.

Purpose: reference implementation / executable spec for a Rust importer that
interprets BF2 scripts at import time (not at runtime).  Verified against the
retail 1.5 install (bf2 + xpack + booster levels).

Usage:
    python bf2con.py test                       # run the built-in self tests (Karkand + Objects)
    python bf2con.py level Strike_at_Karkand [gpm_cq] [64] [--json out.json]
    python bf2con.py templates                  # run every objects/**/*.con of the mod
    python bf2con.py survey                     # command-prefix statistics over all archives
Options: --root "<BF2 install>" (default: $BF2_DIR)  --mod bf2|xpack

Key semantics implemented (see level_formats.md for the evidence):
  * VFS: fileManager.mountArchive <zip> <mountpoint> lines from ServerArchives.con /
    ClientArchives.con; the EARLIEST mount of a path wins; case-insensitive; '\\' == '/';
    level archives mounted at levels/<name>/ AND overlaid on the root (they carry
    objects/... and common/... files that are referenced by root-relative paths).
  * Lexer: whitespace separated tokens, "double quoted" tokens (no escapes), rem,
    beginRem/endRem (nestable), trailing '-> v_var' assignment.
  * Control flow: if/elseIf/else/endIf, while/endWhile, return; conditions
    '<operand> <op> <operand>' with == != < > <= >= (and word forms).
  * Variables: var v_x [= value], const c_x = value, v_arg1..v_arg8 per run scope.
    run = new scope; include = shares caller scope.  Unknown c_ tokens (engine
    enums such as c_ETPlane) are kept literally.
  * run/include paths: leading '/' or '\\' = VFS root, otherwise relative to the
    directory of the running file; missing '.con' extension is appended if needed;
    missing files are ignored (the engine does the same) but reported.
  * console.allowMultipleFileLoad 0 => a file is executed at most once.
  * Lazy template loading: Object.create <T> for an unknown template runs the file
    named <T>.con found anywhere under objects/ (this is how the game resolves
    StaticObjects.con - the explicit 'run' lists are inside 'if v_arg1 == BF2Editor').
"""
import sys, os, re, io, json, zipfile, posixpath, collections, math

DEFAULT_ROOT = os.environ.get('BF2_DIR', r'C:/Program Files (x86)/EA Games/Battlefield 2')

# ----------------------------------------------------------------------------
# Virtual file system
# ----------------------------------------------------------------------------

def vnorm(path):
    """Normalise a BF2 virtual path: lower case, forward slashes, no leading '/', no '.'/'..'."""
    p = path.strip().strip('"').replace('\\', '/').lower()
    parts = []
    for seg in p.split('/'):
        if seg in ('', '.'):
            continue
        if seg == '..':
            if parts:
                parts.pop()
            continue
        parts.append(seg)
    return '/'.join(parts)


class VFS:
    """Layered, case-insensitive file system over zip archives and loose directories.

    Lookup order for a normalised path P:
      1. level overlay archives (root-relative)       - highest priority
      2. mounted archives in mount order (first wins)  - 'objects/...', 'common/...', 'levels/<x>/...'
      3. loose directories: mods/<mod>/, parent mods, game root
    """

    def __init__(self, root=DEFAULT_ROOT, mod='bf2', sides=('server', 'client')):
        self.root = root
        self.mod = mod
        self.index = {}          # vpath -> (zipfile, member)   (first wins)
        self.mounts = []         # (zip path, mount point)
        self.overlay = {}        # level overlay, checked first
        self.loose_dirs = [os.path.join(root, 'mods', mod)]
        self.zips = {}
        desc = os.path.join(root, 'mods', mod, 'Mod.desc')
        if os.path.exists(desc):
            m = re.search(r'<parentmod>\s*(\S+)\s*</parentmod>', open(desc, encoding='latin1').read())
            if m:
                self.loose_dirs.append(os.path.join(root, 'mods', m.group(1)))
        self.loose_dirs.append(root)
        for side in sides:
            f = os.path.join(root, 'mods', mod, '%sArchives.con' % side.capitalize())
            if os.path.exists(f):
                self._read_archives_con(open(f, encoding='latin1').read())
        self.level = None

    def _zip(self, path):
        z = self.zips.get(path)
        if z is None:
            z = self.zips[path] = zipfile.ZipFile(path)
        return z

    def _read_archives_con(self, text):
        for line in text.splitlines():
            t = line.split()
            if len(t) >= 3 and t[0].lower() == 'filemanager.mountarchive':
                self.mount_archive(t[1], t[2])

    def mount_archive(self, zpath, mountpoint, overlay=False):
        """zpath: relative to the mod dir, or to the game root if it starts with 'mods/'."""
        zp = zpath.replace('\\', '/')
        full = os.path.join(self.root, zp) if zp.lower().startswith('mods/') else os.path.join(self.root, 'mods', self.mod, zp)
        if not os.path.exists(full):
            return False
        self.mounts.append((full, mountpoint))
        z = self._zip(full)
        mp = vnorm(mountpoint)
        target = self.overlay if overlay else self.index
        for info in z.infolist():
            if info.is_dir():
                continue
            vp = vnorm(mp + '/' + info.filename) if mp else vnorm(info.filename)
            target.setdefault(vp, (z, info.filename))
        return True

    def find_level_dir(self, name):
        for d in self.loose_dirs[:-1]:
            p = os.path.join(d, 'Levels', name)
            if os.path.isdir(p):
                return p
            # case-insensitive fallback
            ld = os.path.join(d, 'Levels')
            if os.path.isdir(ld):
                for e in os.listdir(ld):
                    if e.lower() == name.lower():
                        return os.path.join(ld, e)
        return None

    def mount_level(self, name):
        """Mount Levels/<name>/server.zip + client.zip at levels/<name>/ and as a root overlay."""
        ldir = self.find_level_dir(name)
        if not ldir:
            raise FileNotFoundError('level not found: ' + name)
        self.level = os.path.basename(ldir)
        for zn in ('server.zip', 'client.zip'):
            p = os.path.join(ldir, zn)
            if not os.path.exists(p):
                continue
            z = self._zip(p)
            mp = 'levels/' + self.level.lower()
            for info in z.infolist():
                if info.is_dir():
                    continue
                n = vnorm(info.filename)
                self.index.setdefault(mp + '/' + n, (z, info.filename))
                if n.split('/')[0] in ('objects', 'common'):
                    self.overlay.setdefault(n, (z, info.filename))
        # A level may ship its own ServerArchives.con / ClientArchives.con (retail: Highway_Tampa and
        # Wake_Island_2007 mount mods/bf2/Objects_drm01_*.zip, which no longer exists in 1.5 -> ignored).
        for side in ('server', 'client'):
            vp = 'levels/%s/%sarchives.con' % (self.level.lower(), side)
            if vp in self.index:
                self._read_archives_con(self.read_text(vp))
        return ldir

    def lookup(self, path):
        vp = vnorm(path)
        hit = self.overlay.get(vp) or self.index.get(vp)
        if hit:
            return ('zip',) + hit
        for d in self.loose_dirs:
            p = os.path.join(d, *vp.split('/'))
            if os.path.isfile(p):
                return ('file', p, None)
            # case-insensitive walk (Windows is already case-insensitive)
        return None

    def exists(self, path):
        return self.lookup(path) is not None

    def read(self, path):
        hit = self.lookup(path)
        if hit is None:
            raise FileNotFoundError(path)
        if hit[0] == 'zip':
            return hit[1].read(hit[2])
        return open(hit[1], 'rb').read()

    def read_text(self, path):
        return self.read(path).decode('latin1')

    def listdir_files(self, prefix):
        pre = vnorm(prefix) + '/'
        keys = set(k for k in self.index if k.startswith(pre)) | set(k for k in self.overlay if k.startswith(pre))
        return sorted(keys)

    def template_index(self):
        """basename(without .con) -> vpath, for every objects/**/*.con visible in the VFS.
        Level overlay files win (same priority as lookup())."""
        idx = {}
        for src in (self.overlay, self.index):
            for k in src:
                if k.startswith('objects/') and k.endswith('.con'):
                    idx.setdefault(posixpath.basename(k)[:-4], k)
        return idx

    def find_texture(self, path, custom_suffix=''):
        """Resolve an extension-less texture reference (e.g. 'common\\textures\\sky\\karkand_cloudy').
        customTextureSuffix (texturemanager.customTextureSuffix "Woodland") is tried first."""
        vp = vnorm(path)
        base = vp[:-4] if vp.endswith('.dds') else vp
        cands = []
        if custom_suffix:
            cands.append(base + '_' + custom_suffix.lower() + '.dds')
        cands += [base + '.dds', vp]
        for c in cands:
            if self.exists(c):
                return c
        return None


# ----------------------------------------------------------------------------
# Lexer
# ----------------------------------------------------------------------------

class Stmt:
    __slots__ = ('file', 'line', 'tokens', 'quoted', 'assign')

    def __init__(self, file, line, tokens, quoted, assign=None):
        self.file, self.line, self.tokens, self.quoted, self.assign = file, line, tokens, quoted, assign

    def __repr__(self):
        return '%s:%d %s' % (self.file, self.line, ' '.join(self.tokens))


def tokenize_line(s):
    """Split one line into tokens. Returns (tokens, quoted_flags). "..." groups (no escapes)."""
    toks, quoted = [], []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c in ' \t\r\n':
            i += 1
            continue
        if c == '"':
            j = s.find('"', i + 1)
            if j < 0:
                j = n
            toks.append(s[i + 1:j]); quoted.append(True)
            i = j + 1
            continue
        j = i
        while j < n and s[j] not in ' \t\r\n':
            if s[j] == '"':      # token with embedded quote e.g. a="b c" : read to closing quote
                k = s.find('"', j + 1)
                j = n if k < 0 else k + 1
                continue
            j += 1
        toks.append(s[i:j]); quoted.append(False)
        i = j
    return toks, quoted


def lex(text, fname='<string>'):
    """Text -> list of Stmt with comments removed.  rem = line comment (first token),
    beginRem ... endRem = block comment (nesting supported)."""
    out = []
    depth = 0
    for ln, raw in enumerate(text.splitlines(), 1):
        toks, quoted = tokenize_line(raw)
        if not toks:
            continue
        k = toks[0].lower()
        if k == 'beginrem':
            depth += 1
            continue
        if k == 'endrem':
            depth = max(0, depth - 1)
            continue
        if depth or k == 'rem':
            continue
        assign = None
        if len(toks) >= 3 and toks[-2] == '->':
            assign = toks[-1]
            toks, quoted = toks[:-2], quoted[:-2]
        out.append(Stmt(fname, ln, toks, quoted, assign))
    return out


# ----------------------------------------------------------------------------
# Parser: flat statements -> block tree
# ----------------------------------------------------------------------------

class If:
    __slots__ = ('branches', 'else_body', 'stmt')

    def __init__(self, stmt):
        self.stmt = stmt
        self.branches = []     # [(cond_tokens, body)]
        self.else_body = None


class While:
    __slots__ = ('cond', 'body', 'stmt')

    def __init__(self, stmt, cond):
        self.stmt, self.cond, self.body = stmt, cond, []


def parse(stmts, warn=None):
    root = []
    stack = [('root', root, None)]
    for st in stmts:
        k = st.tokens[0].lower()
        kind, body, node = stack[-1]
        if k == 'if':
            n = If(st); n.branches.append((st.tokens[1:], []))
            body.append(n)
            stack.append(('if', n.branches[-1][1], n))
        elif k == 'elseif':
            if kind != 'if':
                warn and warn('stray elseIf', st); continue
            node.branches.append((st.tokens[1:], []))
            stack[-1] = ('if', node.branches[-1][1], node)
        elif k == 'else':
            if kind != 'if':
                warn and warn('stray else', st); continue
            node.else_body = []
            stack[-1] = ('ifelse', node.else_body, node)
        elif k == 'endif':
            if kind not in ('if', 'ifelse'):
                warn and warn('stray endIf', st); continue
            stack.pop()
        elif k == 'while':
            n = While(st, st.tokens[1:])
            body.append(n)
            stack.append(('while', n.body, n))
        elif k == 'endwhile':
            if kind != 'while':
                warn and warn('stray endWhile', st); continue
            stack.pop()
        else:
            body.append(st)
    if len(stack) > 1 and warn:
        warn('unterminated block (%s)' % stack[-1][0], stack[-1][2].stmt if stack[-1][2] else None)
    return root


# ----------------------------------------------------------------------------
# World model (what a Rust importer would build)
# ----------------------------------------------------------------------------

def vec(s, n=None):
    try:
        v = [float(x) for x in s.split('/')]
    except ValueError:
        return None
    return v


class Template:
    def __init__(self, ttype, name, src):
        self.type, self.name, self.src = ttype, name, src
        self.props = []            # [(method, args)]
        self.children = []         # [{'template','position','rotation', 'props'}]
        self.geometry = None
        self.collision_mesh = None

    def get(self, method, default=None):
        m = method.lower()
        for k, a in reversed(self.props):
            if k == m:
                return a
        return default

    def to_dict(self):
        return {'type': self.type, 'name': self.name, 'src': self.src, 'geometry': self.geometry,
                'children': self.children, 'props': self.props}


class Geometry:
    def __init__(self, gtype, name, src_dir):
        self.type, self.name, self.dir = gtype, name, src_dir
        self.props = []

    def mesh_path(self):
        # RoadCompiled geometries have no per-template mesh: each road instance supplies its own
        # Levels/<L>/Roads/<name>_compiled.mesh via Object.geometry.loadMesh (CompiledRoads.con).
        ext = {'staticmesh': 'staticmesh', 'bundledmesh': 'bundledmesh', 'skinnedmesh': 'skinnedmesh'}.get(self.type.lower())
        return None if ext is None else '%s/meshes/%s.%s' % (self.dir, self.name.lower(), ext)


class Instance:
    def __init__(self, template, src):
        self.template, self.src = template, src
        self.position = None       # [x,y,z]  (Object.absolutePosition)
        self.rotation = None       # [yaw,pitch,roll] degrees (Object.rotation)
        self.transform = None      # 4x4 row-major, D3D row-vector (Object.absoluteTransformation)
        self.props = {}

    def to_dict(self):
        d = {'template': self.template, 'position': self.position}
        if self.rotation is not None:
            d['rotation'] = self.rotation
        if self.transform is not None:
            d['transform'] = self.transform
        if self.props:
            d['props'] = self.props
        return d


class World:
    """Receives engine commands.  Only the subset needed for a static importer is interpreted;
    everything else is stored generically per prefix so nothing is lost."""

    ENTITY_CREATORS = {
        'combatarea': ('create',), 'heightmapcluster': ('create', 'addheightmap'),
        'aistrategicarea': ('create', 'createfromcontrolpoint'), 'overgrowth': ('addmaterial', 'addtype'),
        'roadtemplate': ('createinstance',), 'hudbuilder': ('createobject',), 'kittemplate': ('create',),
        'weapontemplate': ('create',), 'aitemplate': ('create',), 'aitemplateplugin': ('create',),
        'animationsystem': ('createanimationsystem',), 'networkableinfo': ('createnewinfo',),
    }
    ENTITY_ALIAS = {'heightmap': 'heightmapcluster', 'overgrowthtype': 'overgrowth',
                    'roadtemplatetexture': 'roadtemplate'}

    def __init__(self):
        self.templates = {}        # lower name -> Template
        self.template_order = []
        self.geometries = {}
        self.instances = []
        self.active_template = None
        self.active_geometry = None
        self.cur_instance = None
        self.entities = collections.defaultdict(list)  # prefix -> [ {'create':[..], 'props':[..]} ]
        self.settings = collections.defaultdict(list)  # prefix -> [(method, args)]  (non-entity)
        self.cmd_counts = collections.Counter()
        self.unknown = collections.Counter()
        self.template_creates = collections.Counter()
        self.collision_templates = {}

    # -- ObjectTemplate / GeometryTemplate / Object -----------------------------
    def command(self, prefix, method, args, ctx):
        p, m = prefix.lower(), method.lower()
        self.cmd_counts[p + '.' + m] += 1
        if p == 'objecttemplate':
            return self._objecttemplate(m, args, ctx)
        if p == 'geometrytemplate':
            if m == 'create' and len(args) >= 2:
                g = Geometry(args[0], args[1], ctx.cur_dir)
                self.geometries[args[1].lower()] = g
                self.active_geometry = g
            elif m in ('active', 'activesafe') and args:
                self.active_geometry = self.geometries.get(args[-1].lower(), self.active_geometry)
            elif self.active_geometry is not None:
                self.active_geometry.props.append((m, args))
            return
        if p == 'collisionmanager' and m == 'createtemplate' and args:
            self.collision_templates[args[0].lower()] = ctx.cur_dir + '/meshes/' + args[0].lower() + '.collisionmesh'
            return
        if p == 'object':
            return self._object(m, args, ctx)
        # generic entities / settings
        ep = self.ENTITY_ALIAS.get(p, p)
        if m in self.ENTITY_CREATORS.get(ep, ()) and not (ep != p):
            self.entities[ep].append({'create': [m] + args, 'props': [], 'src': ctx.where()})
            return
        if ep in self.ENTITY_CREATORS and self.entities[ep]:
            self.entities[ep][-1]['props'].append(((p + '.' if ep != p else '') + m, args))
            return
        self.settings[p].append((m, args))

    def _objecttemplate(self, m, args, ctx):
        if m == 'create' and len(args) >= 2:
            t = Template(args[0], args[1], ctx.where())
            key = args[1].lower()
            self.template_creates[key] += 1
            if key not in self.templates:
                self.template_order.append(key)
            self.templates[key] = t
            self.active_template = t
            return
        if m == 'activesafe' and len(args) >= 2:
            key = args[1].lower()
            t = self.templates.get(key)
            if t is None:
                t = self.templates[key] = Template(args[0], args[1], ctx.where())
                self.template_order.append(key)
            self.active_template = t
            return
        if m == 'active' and args:
            self.active_template = self.templates.get(args[-1].lower(), self.active_template)
            return
        t = self.active_template
        if t is None:
            self.unknown['ObjectTemplate.%s (no active template)' % m] += 1
            return
        if m == 'addtemplate' and args:
            t.children.append({'template': args[0], 'position': None, 'rotation': None, 'props': []})
        elif m in ('setposition', 'setrotation') and args and t.children:
            t.children[-1]['position' if m == 'setposition' else 'rotation'] = vec(args[0])
        elif m == 'geometry' and args:
            t.geometry = args[0]
        elif m in ('collisionmesh', 'setcollisionmesh') and args:
            t.collision_mesh = args[0]
        else:
            if t.children and m.startswith('set') and m in ('setnetworkableinfo',):
                pass
            t.props.append((m, args))

    def _object(self, m, args, ctx):
        if m == 'create' and args:
            ctx.interp.ensure_template(args[0])
            inst = Instance(args[0], ctx.where())
            self.instances.append(inst)
            self.cur_instance = inst
            return
        inst = self.cur_instance
        if inst is None:
            self.unknown['Object.%s (no current object)' % m] += 1
            return
        if m == 'absoluteposition' and args:
            inst.position = vec(args[0])
        elif m == 'rotation' and args:
            inst.rotation = vec(args[0])
        elif m == 'absolutetransformation' and args:
            rows = re.findall(r'\[([^\]]*)\]', args[0])
            inst.transform = [vec(r) for r in rows]
            if len(inst.transform) == 4 and all(r and len(r) == 4 for r in inst.transform):
                inst.position = inst.transform[3][:3]
        elif m == 'geometry.loadmesh' and args:
            inst.props['mesh'] = args[0]
        else:
            inst.props[m] = args[0] if len(args) == 1 else args

    # -- queries --------------------------------------------------------------
    def instances_of_type(self, ttype):
        t = ttype.lower()
        return [i for i in self.instances if self.templates.get(i.template.lower()) is not None
                and self.templates[i.template.lower()].type.lower() == t]


# ----------------------------------------------------------------------------
# Interpreter
# ----------------------------------------------------------------------------

class Scope:
    def __init__(self, args=(), parent_consts=None):
        self.vars = {}
        for i in range(1, 9):
            self.vars['v_arg%d' % i] = args[i - 1] if i - 1 < len(args) else ''
        self.vars['v_result'] = ''
        self.consts = parent_consts if parent_consts is not None else {}


class Ctx:
    def __init__(self, interp, file):
        self.interp, self.file = interp, file
        self.cur_dir = posixpath.dirname(file)
        self.line = 0

    def where(self):
        return '%s:%d' % (self.file, self.line)


class ReturnSignal(Exception):
    pass


OPS = {'==': 'eq', 'equals': 'eq', '!=': 'ne', 'notequals': 'ne', '>': 'gt', 'greaterthan': 'gt',
       '>=': 'ge', 'greaterorequalthan': 'ge', '<': 'lt', 'lessthan': 'lt', '<=': 'le', 'lessorequalthan': 'le'}


class Interpreter:
    def __init__(self, vfs, world=None, lazy_templates=True, max_depth=64, verbose=False):
        self.vfs = vfs
        self.world = world or World()
        self.lazy = lazy_templates
        self.allow_multiple = True
        self.loaded = set()
        self.run_count = collections.Counter()
        self.missing_files = collections.Counter()
        self.warnings = []
        self.depth = 0
        self.max_depth = max_depth
        self.verbose = verbose
        self._tindex = None
        self._parsed = {}
        self.missing_templates = collections.Counter()
        self.lazy_loaded = []
        self.globals_consts = {}

    # -- file resolution --------------------------------------------------------
    def resolve(self, path, cur_dir):
        p = path.strip('"').replace('\\', '/')
        vp = vnorm(p) if p.startswith('/') else vnorm(cur_dir + '/' + p)
        for cand in (vp, vp + '.con'):
            if self.vfs.exists(cand):
                return cand
        return None

    def ensure_template(self, name):
        key = name.lower()
        if key in self.world.templates or not self.lazy:
            return
        if self._tindex is None:
            self._tindex = self.vfs.template_index()
        # <name>.con is the rule for everything except compiled road templates, which live in
        # objects/roads/splines/<name>_compiled.con (<name>.con there is the editor RoadTemplate).
        for cand in (key, key + '_compiled'):
            path = self._tindex.get(cand)
            if path is None:
                continue
            self.lazy_loaded.append(path)
            self.run_file(path, [], scope=None)
            if key in self.world.templates:
                return
        self.missing_templates[key] += 1

    # -- execution --------------------------------------------------------------
    def warn(self, msg, st=None):
        self.warnings.append('%s: %s' % (st if st is not None else '', msg))

    def run_file(self, vpath, args=(), scope=None):
        """Execute a file.  scope=None -> new scope (run); a Scope -> shared (include)."""
        vpath = vnorm(vpath)
        if not self.allow_multiple and vpath in self.loaded:
            return
        if self.depth >= self.max_depth:
            self.warn('max include depth at ' + vpath)
            return
        tree = self._parsed.get(vpath)
        if tree is None:
            try:
                text = self.vfs.read_text(vpath)
            except FileNotFoundError:
                self.missing_files[vpath] += 1
                return
            tree = self._parsed[vpath] = parse(lex(text, vpath), self.warn)
        self.loaded.add(vpath)
        self.run_count[vpath] += 1
        if scope is None:
            scope = Scope(args, self.globals_consts)
        elif args:
            for i, a in enumerate(args[:8], 1):
                scope.vars['v_arg%d' % i] = a
        ctx = Ctx(self, vpath)
        self.depth += 1
        try:
            self.exec_block(tree, scope, ctx)
        except ReturnSignal:
            pass
        finally:
            self.depth -= 1

    def subst(self, tok, quoted, scope):
        if quoted:
            return tok
        tl = tok.lower()
        if tl.startswith('v_'):
            return scope.vars.get(tl, tok)
        if tl.startswith('c_'):
            return scope.consts.get(tl, tok)
        return tok

    def eval_cond(self, toks, scope):
        if not toks:
            return False
        vals = [self.subst(t, False, scope) for t in toks]
        if len(vals) == 1:
            return vals[0] not in ('', '0')
        if len(vals) >= 3 and vals[1].lower() in OPS:
            a, op, b = vals[0], OPS[vals[1].lower()], ' '.join(vals[2:])
            try:
                fa, fb = float(a), float(b)
                return {'eq': fa == fb, 'ne': fa != fb, 'gt': fa > fb, 'ge': fa >= fb, 'lt': fa < fb, 'le': fa <= fb}[op]
            except ValueError:
                sa, sb = a.lower(), b.lower()
                return {'eq': sa == sb, 'ne': sa != sb, 'gt': sa > sb, 'ge': sa >= sb, 'lt': sa < sb, 'le': sa <= sb}[op]
        self.warn('unparsed condition ' + ' '.join(toks))
        return False

    def exec_block(self, body, scope, ctx):
        for node in body:
            if isinstance(node, If):
                ctx.line = node.stmt.line
                done = False
                for cond, blk in node.branches:
                    if self.eval_cond(cond, scope):
                        self.exec_block(blk, scope, ctx)
                        done = True
                        break
                if not done and node.else_body is not None:
                    self.exec_block(node.else_body, scope, ctx)
            elif isinstance(node, While):
                n = 0
                while self.eval_cond(node.cond, scope) and n < 100000:
                    self.exec_block(node.body, scope, ctx)
                    n += 1
            else:
                self.exec_stmt(node, scope, ctx)

    def exec_stmt(self, st, scope, ctx):
        ctx.line = st.line
        t0 = st.tokens[0]
        k = t0.lower()
        args = [self.subst(t, q, scope) for t, q in zip(st.tokens[1:], st.quoted[1:])]
        if k in ('run', 'include'):
            if not args:
                return
            path = self.resolve(st.tokens[1], ctx.cur_dir)
            if path is None:
                tgt = vnorm(args[0]) if args[0][:1] in '/\\' else vnorm(ctx.cur_dir + '/' + args[0])
                self.missing_files[tgt] += 1
                return
            if k == 'run':
                self.run_file(path, args[1:], scope=None)
            else:
                self.run_file(path, args[1:], scope=scope)
            return
        if k in ('var', 'const'):
            # var v_x [= value] | const c_x = value
            if len(st.tokens) >= 2:
                name = st.tokens[1].lower()
                val = ''
                if len(st.tokens) >= 4 and st.tokens[2] == '=':
                    val = ' '.join(args[2:])
                elif len(st.tokens) == 3:
                    val = args[1]
                (scope.consts if k == 'const' else scope.vars)[name] = val
            return
        if k == 'return':
            raise ReturnSignal()
        if k in ('echo', 'alias'):
            self.world.settings[k].append((k, args))
            return
        if k == 'console.allowmultiplefileload':
            self.allow_multiple = (args[:1] != ['0'])
            return
        if '.' in t0:
            prefix, method = t0.split('.', 1)
            self.world.command(prefix, method, args, ctx)
            if st.assign:
                scope.vars[st.assign.lower()] = ''
            return
        self.world.unknown[t0] += 1


# ----------------------------------------------------------------------------
# Level loading (static equivalent of GameLogic::loadLevel)
# ----------------------------------------------------------------------------

def load_level(root, mod, level, gamemode='gpm_cq', size=64, lazy=True):
    vfs = VFS(root, mod)
    vfs.mount_level(level)
    it = Interpreter(vfs, lazy_templates=lazy)
    L = 'levels/' + vfs.level.lower()
    # 1. Init.con with (empty) args -> game branch (editor passes v_arg1=BF2Editor)
    it.run_file(L + '/init.con', [])
    # 2. StaticObjects.con is run by the engine itself (not from Init.con), game branch
    it.run_file(L + '/staticobjects.con', [])
    # 2b. engine-run extras (exe string table): Triggerables.con (xpack elevators/doors etc.)
    if vfs.exists(L + '/triggerables.con'):
        it.run_file(L + '/triggerables.con', [])
    # 3. game-mode specific objects; placement blocks are inside 'if v_arg1 == host'
    gpo = '%s/gamemodes/%s/%s/gameplayobjects.con' % (L, gamemode.lower(), size)
    if vfs.exists(gpo):
        it.run_file(gpo, ['host'])
    else:
        it.warn('no ' + gpo)
    return vfs, it


def summarize_level(vfs, it):
    w = it.world
    out = collections.OrderedDict()
    out['level'] = vfs.level
    hm = w.entities.get('heightmapcluster', [])
    out['heightmapcluster'] = [e['create'] for e in hm[:1]] + [{'props': hm[0]['props']}] if hm else None
    out['heightmaps'] = [{'args': e['create'][1:], 'props': e['props']} for e in hm if e['create'][0] == 'addheightmap']
    out['terrain'] = w.settings.get('terrain')
    out['counts'] = {
        'templates': len(w.templates), 'geometries': len(w.geometries), 'instances': len(w.instances),
        'files_run': len(it.run_count), 'lazy_template_files': len(it.lazy_loaded),
        'missing_templates': sum(it.missing_templates.values()), 'missing_files': sum(it.missing_files.values()),
        'warnings': len(it.warnings)}
    return out


def level_json(vfs, it):
    """Everything a Rust level importer needs, as plain JSON."""
    w = it.world
    def tt(i):
        t = w.templates.get(i.template.lower())
        return t.type if t else None
    static, gameplay = [], {'ControlPoint': [], 'SpawnPoint': [], 'ObjectSpawner': [], 'other': []}
    for i in w.instances:
        d = i.to_dict()
        d['type'] = tt(i)
        d['src'] = i.src
        if '/gamemodes/' in i.src:
            t = w.templates.get(i.template.lower())
            if t is not None:
                d['template_props'] = t.props
                d['children'] = t.children
            gameplay.get(d['type'], gameplay['other']).append(d)
        else:
            static.append(d)
    meshes = {}
    for key in set(i.template.lower() for i in w.instances):
        t = w.templates.get(key)
        if t and t.geometry:
            g = w.geometries.get(t.geometry.lower())
            if g:
                meshes[key] = {'geometry_type': g.type, 'mesh': g.mesh_path(), 'children': t.children}
    return {'level': vfs.level, 'summary': summarize_level(vfs, it), 'static_objects': static,
            'gameplay': gameplay, 'template_meshes': meshes,
            'combat_areas': w.entities.get('combatarea', []),
            'settings': {k: v for k, v in w.settings.items() if k in (
                'lightmanager', 'skydome', 'renderer', 'terrain', 'gamelogic', 'hemimapmanager', 'undergrowth',
                'texturemanager', 'lightsettings', 'sound')},
            'missing_templates': dict(it.missing_templates), 'missing_files': dict(it.missing_files)}


# ----------------------------------------------------------------------------
# CLI / self tests
# ----------------------------------------------------------------------------

def _opt(argv, name, default):
    if name in argv:
        i = argv.index(name)
        v = argv[i + 1]
        del argv[i:i + 2]
        return v
    return default


def cmd_level(root, mod, argv):
    level = argv[0] if argv else 'Strike_at_Karkand'
    gm = argv[1] if len(argv) > 1 else 'gpm_cq'
    size = argv[2] if len(argv) > 2 else '64'
    vfs, it = load_level(root, mod, level, gm, size)
    w = it.world
    s = summarize_level(vfs, it)
    print('== level', vfs.level, gm, size)
    print('mounts:', [(os.path.basename(z), m) for z, m in vfs.mounts])
    print('counts:', s['counts'])
    for e in s['heightmaps'][:2]:
        print('heightmap', e['args'], e['props'])
    print('terrain settings:', w.settings.get('terrain'))
    types = collections.Counter()
    for i in w.instances:
        t = w.templates.get(i.template.lower())
        types[t.type if t else '<missing>'] += 1
    print('instances by template type:', dict(types))
    print('static objects (StaticObjects.con):', sum(1 for i in w.instances if i.src.split(':')[0].endswith('staticobjects.con')))
    print('control points:', len(w.instances_of_type('ControlPoint')), 'spawn points:', len(w.instances_of_type('SpawnPoint')),
          'object spawners:', len(w.instances_of_type('ObjectSpawner')))
    print('combat areas:', [(e['create'][1], sum(1 for p in e['props'] if p[0] == 'addareapoint')) for e in w.entities.get('combatarea', [])])
    print('roads (Object.geometry.loadMesh):', sum(1 for i in w.instances if 'mesh' in i.props))
    print('missing templates:', dict(it.missing_templates))
    print('missing files (first 10):', list(it.missing_files.items())[:10])
    print('warnings (first 5):', it.warnings[:5])
    print('unknown statements:', dict(w.unknown))
    js = _opt(argv, '--json', None)
    return vfs, it


def cmd_templates(root, mod):
    vfs = VFS(root, mod)
    it = Interpreter(vfs, lazy_templates=False)
    files = [k for k in vfs.index if k.startswith('objects/') and k.endswith('.con')]
    for f in sorted(files):
        it.run_file(f, [])
    w = it.world
    print('== templates: ran', len(files), 'object .con files (+includes: %d files total)' % len(it.run_count))
    print('templates:', len(w.templates), 'by type:', collections.Counter(t.type for t in w.templates.values()).most_common(15))
    print('geometries:', len(w.geometries), collections.Counter(g.type for g in w.geometries.values()).most_common())
    miss = [g.mesh_path() for g in w.geometries.values() if g.mesh_path() and not vfs.exists(g.mesh_path())]
    print('geometry mesh files resolved: %d / %d' % (sum(1 for g in w.geometries.values() if g.mesh_path()) - len(miss),
                                                     sum(1 for g in w.geometries.values() if g.mesh_path())), 'missing e.g.', miss[:3])
    print('children (addTemplate) total:', sum(len(t.children) for t in w.templates.values()))
    print('duplicate ObjectTemplate.create names:', sum(1 for v in w.template_creates.values() if v > 1))
    print('missing include/run targets:', sum(it.missing_files.values()), list(it.missing_files)[:5])
    print('warnings:', len(it.warnings), it.warnings[:3])
    print('top ObjectTemplate methods:', [(k, v) for k, v in w.cmd_counts.most_common(12)])
    return vfs, it


def cmd_survey(root):
    pref = collections.Counter()
    kw = collections.Counter()
    nfiles = 0
    for mod in ('bf2', 'xpack'):
        md = os.path.join(root, 'mods', mod)
        for dp, dn, fn in os.walk(md):
            for f in fn:
                p = os.path.join(dp, f)
                srcs = []
                if f.lower().endswith('.zip'):
                    z = zipfile.ZipFile(p)
                    srcs = [(i.filename, lambda i=i, z=z: z.read(i)) for i in z.infolist()
                            if i.filename.lower().endswith(('.con', '.tweak', '.inc', '.ai'))]
                elif f.lower().endswith(('.con', '.tweak', '.inc', '.ai')):
                    srcs = [(p, lambda p=p: open(p, 'rb').read())]
                for name, rd in srcs:
                    nfiles += 1
                    for st in lex(rd().decode('latin1'), name):
                        t = st.tokens[0]
                        if '.' in t:
                            pref[t.split('.')[0].lower()] += 1
                        else:
                            kw[t.lower()] += 1
    print('files', nfiles)
    for k, v in pref.most_common():
        print('%8d %s' % (v, k))
    print('keywords/other first tokens:', kw.most_common(20))


def cmd_test(root):
    ok = True
    def check(cond, msg):
        nonlocal ok
        print(('  PASS ' if cond else '  FAIL ') + msg)
        ok = ok and cond
    # lexer
    st = lex('rem x\nbeginRem\nfoo.bar 1\nendRem\nObjectTemplate.setName "a b" 1/2/3\n  var v_dist = 20\nx.y 1 -> v_r\n')
    check([s.tokens for s in st] == [['ObjectTemplate.setName', 'a b', '1/2/3'], ['var', 'v_dist', '=', '20'], ['x.y', '1']], 'lexer: rem/beginRem/quotes')
    check(st[2].assign == 'v_r', 'lexer: -> assignment')
    print('== Karkand gpm_cq 64')
    vfs, it = load_level(root, 'bf2', 'Strike_at_Karkand', 'gpm_cq', 64)
    w = it.world
    so = [i for i in w.instances if i.src.split(':')[0].endswith('staticobjects.con')]
    check(len(so) == 1336 - 1, 'StaticObjects.con: 1335 game-branch Object.create (1336 incl. editor-only DefaultEnvMap) -> %d' % len(so))
    check(sum(1 for i in so if i.rotation) == 840, 'Object.rotation lines in game branch: %d' % sum(1 for i in so if i.rotation))
    cps = w.instances_of_type('ControlPoint')
    check(len(cps) == 9, 'CQ64 control points = 9 -> %d' % len(cps))
    check(len(w.instances_of_type('ObjectSpawner')) == 42, 'CQ64 object (vehicle) spawners = 42 -> %d' % len(w.instances_of_type('ObjectSpawner')))
    check(len(w.instances_of_type('SpawnPoint')) == 48, 'CQ64 soldier spawn points = 48 -> %d' % len(w.instances_of_type('SpawnPoint')))
    ca = w.entities.get('combatarea', [])
    check(len(ca) == 2 and sum(1 for p in ca[1]['props'] if p[0] == 'addareapoint') > 100, 'combat areas: %s' % [(e['create'][1], len(e['props'])) for e in ca])
    hms = [e for e in w.entities['heightmapcluster'] if e['create'][0] == 'addheightmap']
    check(len(hms) == 9 and ('heightmap.setsize', ['513', '513']) in hms[0]['props'], 'Heightdata.con: 9 heightmaps, primary 513x513')
    check(any(m == 'load' for m, a in w.settings['terrain']), 'Terrain.con game branch (terrain.load terraindata.raw)')
    miss_real = {k: v for k, v in it.missing_templates.items()}
    check(not miss_real, 'all Object.create templates resolved by lazy lookup (%d template files lazily run): missing=%s' % (len(it.lazy_loaded), miss_real))
    geo_ok = sum(1 for i in so if (w.templates.get(i.template.lower()) and w.templates[i.template.lower()].geometry))
    check(geo_ok > 1200, 'static instances with geometry template: %d' % geo_ok)
    meshes = set()
    for i in so:
        t = w.templates.get(i.template.lower())
        if t and t.geometry and t.geometry.lower() in w.geometries:
            mp = w.geometries[t.geometry.lower()].mesh_path()
            if mp:
                meshes.add(mp)
    check(all(vfs.exists(m) for m in meshes), 'all %d referenced mesh files exist in client archives' % len(meshes))
    roads = [i for i in w.instances if 'mesh' in i.props]
    nroads = len(re.findall(r'loadmesh', vfs.read_text('levels/strike_at_karkand/compiledroads.con'), re.I))
    check(len(roads) == nroads and all(vfs.exists(i.props['mesh']) for i in roads), 'CompiledRoads.con: %d/%d road meshes, all resolvable' % (len(roads), nroads))
    print('== xpack Devils_Perch (parent-mod archives)')
    vfs2, it2 = load_level(root, 'xpack', 'Devils_Perch', 'gpm_cq', 64)
    check(len(it2.world.instances_of_type('ControlPoint')) > 0 and set(it2.missing_templates) <= {'xp1_mi_antenna_lights', 'xp1_light_cone_04'},
          'xpack level loads, CPs=%d, missing templates=%s (these 2 are dangling in retail data)' % (
        len(it2.world.instances_of_type('ControlPoint')), dict(it2.missing_templates)))
    print('== GreatWall (booster, level-local objects)')
    vfs3, it3 = load_level(root, 'bf2', 'GreatWall', 'gpm_cq', 32)
    check(not it3.missing_templates, 'GreatWall templates resolved (level-local objects via Init.con + overlay): %s' % dict(it3.missing_templates))
    check(vfs3.exists('objects/staticobjects/_asia/village/textures/greatwall_de.dds'), 'root overlay: level-local texture visible at objects/...')
    check(vfs3.find_texture('common\\textures\\sky\\Great_Wall_sky') is not None, 'root overlay: level-local sky texture resolves')
    print('== all Objects templates (bf2)')
    vfs4, it4 = cmd_templates(root, 'bf2')
    check(len(it4.world.templates) > 5000, 'templates parsed: %d' % len(it4.world.templates))
    print('RESULT:', 'ALL PASS' if ok else 'SOME FAILURES')
    return ok


def main(argv):
    root = _opt(argv, '--root', DEFAULT_ROOT)
    mod = _opt(argv, '--mod', 'bf2')
    js = _opt(argv, '--json', None)
    if not argv or argv[0] == 'test':
        return 0 if cmd_test(root) else 1
    if argv[0] == 'level':
        vfs, it = cmd_level(root, mod, argv[1:])
        if js:
            with open(js, 'w') as f:
                json.dump(level_json(vfs, it), f, indent=1)
            print('wrote', js)
    elif argv[0] == 'templates':
        cmd_templates(root, mod)
    elif argv[0] == 'survey':
        cmd_survey(root)
    else:
        print(__doc__)
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
