# Battlefield 2 mesh / skeleton / animation binary formats

This spec is verified against a retail install (patch 1.5 + SF + booster packs).
Every claim marked **[V]** was checked by the reference parsers in this directory (`bf2mesh.py`,
`bf2coll.py`, `bf2anim.py`) over every file of that type in the install. Claims marked **[S]** come from
the game's own HLSL shaders (`mods/bf2/Shaders_client.zip`). **[B]** means taken from the bf2-blender
add-on (marekzajac97/bf2-blender, `io_scene_bf2/core/bf2/*`) and consistent with the data but not
independently provable. **[?]** means unknown or inferred.

## 0. General conventions

* All values are little-endian. `u8/u16/u32` are unsigned, `i16/i32` signed, `f32` is IEEE float.
* `string` = `u32 length` + `length` bytes. There's no NUL terminator and no padding. The bytes are ASCII or latin-1.
  (`.ske` names are the exception: `u16` length, and the length includes a trailing NUL.)
* `vec3` = 3×f32. `mat4` = 16×f32 stored row by row as `m[row][col]` in **Direct3D row-vector convention**
  (`v' = v · M`, translation in row 3 = floats 12..14).
* Files never contain padding or alignment. Every shipped file that the game uses parses exactly to EOF **[V]**.
* Paths inside meshes (texture maps) are relative to the game's virtual file system, are case-insensitive, and
  mix `/` and `\`. `objects/...` resolves into `Objects_client.zip` (and `Booster_client.zip`), and `common/...` into
  `Common_client.zip`, as set up by `mods/<mod>/ClientArchives.con` (`fileManager.mountArchive Objects_client.zip Objects`).
  Mount xpack before bf2. 99.4 % of all 32 375 texture references resolve this way **[V]**. The rest are absolute DICE
  paths (`c:/dicecan/projects/bf2/bin/mods/xpack2/objects/...`: strip up to `/mods/<mod>/`), bare file names, a
  `Content\` prefix, a `@@ABSOLUTE@@` prefix (orphan files), or files that really are missing.

## 1. Coordinate system and conversion to glTF

BF2 is Direct3D-style and **left-handed**: **+X right, +Y up, +Z forward** **[V]**. Evidence:

* soldier skeleton: the eyes are at z = +0.08, and the `left_*` bones are at x < 0
* the LAV-25 gun-barrel part extends along +Z
* concrete barriers' vehicle collision is taller than the visual mesh
* Units are metres.

### Winding

* **Visible meshes:** `cross(v1 − v0, v2 − v0)` points along the vertex normal for 99.6 % of all triangles
  (7.32 M agree, 34 k disagree) **[V]**. So front faces are clockwise as seen by the viewer (D3D default), with culling on.
  Double-sided surfaces are stored as two triangles with opposite winding.
* **Collision meshes use the opposite winding:** `cross(v1 − v0, v2 − v0)` points *into* the solid
  (the cube test gives 24/24, and crates/barrels 77 %) **[V]**.

### Converting to glTF (right-handed, +Y up, asset front = +Z)

Mirror one axis. We recommend mirroring X, because it keeps BF2's +Z forward as glTF's +Z front and puts glTF's "right" at −X, as the glTF spec wants.

| item | BF2 value | glTF value (mirror X, S = diag(−1,1,1)) |
|---|---|---|
| position / normal / tangent | (x,y,z) | (−x, y, z) |
| visible-mesh triangle | (a,b,c) | (a,c,b) (reverse winding) |
| collision triangle | (a,b,c) | (a,b,c) (keep: the mirror turns it into outward CCW) |
| `.ske`/`.baf` quaternion `q_file=(x,y,z,w)` | | **(−x, y, z, w)** **[V, numerically]** |
| `.ske`/`.baf` translation | (x,y,z) | (−x, y, z) |
| any 4×4 matrix M (column-vector form) | M | S·M·S |
| skinnedmesh inverse-bind matrix | 16 floats as stored | the same 16 floats read as a column-major glTF matrix are the IBM in BF2 space; then apply S·IBM·S |
| UV | (u,v) | (u,v) (no flip) |
| tangent.w | `flip = 1 − 2·blendindices.byte2` | w = flip (see §2.7) |

**UVs:** D3D and glTF both put UV (0,0) at the top-left of the image, so don't flip V. The Blender add-on only flips V because Blender's UV origin is bottom-left.

**Mirroring Z instead (Bevy native: −Z forward, +X right):** use (x, y, −z) for positions and q → (x, y, −z, w) **[V, numerically]**, and apply the same winding rules.

## 2. Visible meshes: `.staticmesh`, `.bundledmesh`, `.skinnedmesh`

All three share one container layout. The **file extension** decides the type-specific parts (the header does not).

### 2.1 Layout

```
Header
  u32  u0              always 0
  u32  version         see version table
  u32  u1, u2, u3      always 0 (orphan gpu_* files: u1=69/819, u2=10)
  u8   u4              always 0 (bf2-blender: "BFP4F flag")

Geom table
  u32  geomCount
  repeat geomCount:  u32 lodCount

Vertex declaration
  u32  elementCount    (includes a terminator element)
  repeat elementCount:
    u16 flag           0 = used, 0xFF = unused (terminator)
    u16 offset         byte offset inside the vertex
    u16 type           D3DDECLTYPE (2 = FLOAT3, 1 = FLOAT2, 0 = FLOAT1, 4 = D3DCOLOR, 17 = UNUSED)
    u16 usage          low byte D3DDECLUSAGE, high byte = usage index (TEXCOORD n = n<<8 | 5)
  terminator is always {0xFF, 0, 17, 0}  [V]

u32  vertFormat        always 4 [V] (bytes per component; bf2-blender reads it as D3DPT_TRIANGLELIST=4)
u32  vertStride        bytes per vertex, == end of the last element [V]
u32  vertexCount
u8   vertexData[vertexCount * vertStride]
u32  indexCount
u16  indices[indexCount]

(staticmesh, bundledmesh only)
u32  alphaSortSets     always 8 in shipped files [V] (see 2.6)

for geom in geoms, for lod in geom.lods:            -- "LOD block"
  vec3 bmin, bmax      exact AABB of all vertices used by the LOD's materials [V, 100 %]
  if version <= 6:     vec3 (radiusAboutOrigin, halfDiagonalOfAABB, junk) [V]
  staticmesh:  u32 nodeCount ; mat4 nodes[nodeCount]
  bundledmesh: u32 partCount                       (no matrices)
  skinnedmesh: u32 rigCount ; repeat: u32 boneCount ; repeat: {u32 skeBoneIndex ; mat4 invBind}

for geom in geoms, for lod in geom.lods:            -- "material block"
  u32 materialCount
  repeat:
    (static/bundled only) u32 alphaMode           0 none, 1 alpha blend, 2 alpha test
    string fxFile                                 "StaticMesh.fx", "BundledMesh.fx", "SkinnedMesh.fx" (informational)
    string technique
    u32    mapCount ; string maps[mapCount]
    u32    vstart    first vertex of the material in the vertex buffer
    u32    istart    first index in the index buffer
    u32    inum      index count (one sort set)
    u32    vnum      vertex count
    u32    u4, u5    exporter heap garbage (pointer-like values), ignore, write 0 [V]
    (staticmesh version 11 only) vec3 mbmin, mbmax   exact AABB of the material's vertices [V, 100 %]
EOF
```

The LOD blocks for *all* geoms/LODs come first, then the material blocks for *all* geoms/LODs, both in
geom-major, LOD-minor order.

### 2.2 Versions found (2 030 files) [V]

| type | version | files | differences |
|---|---|---|---|
| staticmesh | 4 | 156 | LOD block has the extra `vec3` (radius triple); no material bounds |
| staticmesh | 10 | 17 | no extra vec3; no material bounds |
| staticmesh | 11 | 1206 | material bounds (24 bytes) after `u5` |
| staticmesh | 5 (head u1=819,u2=10) | 1 | orphan `gpu_xp1_stonewall_straight` (see 2.10) |
| bundledmesh | 6 | 265 | LOD block has the extra `vec3` |
| bundledmesh | 10 | 303 | none |
| bundledmesh | 6 (head u1=69,u2=10) | 2 | orphan `gpu_xpak_atv`, `gpu_xpak_jetski` (see 2.10) |
| skinnedmesh | 4 | 3 | LOD block has the extra `vec3` |
| skinnedmesh | 10, 11 | 4, 73 | identical layout (no material bounds in either) |

Rule: `version <= 6` → radius triple, and `staticmesh && version == 11` → material bounds. Nothing else changes.

### 2.3 Vertex declarations actually used [V]

```
562 bundled : POSITION f3, NORMAL f3, BLENDINDICES d3dcolor, TEXCOORD0 f2, TANGENT f3                 (stride 48)
  8 bundled : ... TEXCOORD0 f2, TEXCOORD1 f2, TANGENT f3   (AnimatedUV vehicles)                     (stride 56)
 80 skinned : POSITION f3, NORMAL f3, BLENDWEIGHT f1, BLENDINDICES d3dcolor, TEXCOORD0 f2, TANGENT f3 (stride 52)
static      : POSITION f3, NORMAL f3, BLENDINDICES d3dcolor, TEXCOORD0..N f2 (N = 0..4), TANGENT f3
              (1 set: 75 files, 2: 289, 3: 527, 4: 257, 5: 232)
```

Parse the declaration generically: offsets always follow the order above, but read them from the table rather than hard-coding them.

### 2.4 Indices, materials, geoms

* **Index values are relative to the material's `vstart`** (`globalVertex = vstart + index`) **[V]**. Every index
  is below `vnum` (100 %). `vstart` is a u32, so a mesh can have more than 65 536 vertices even though indices are u16.
* Triangle list, 3 indices per face (`inum % 3 == 0` always).
* Unreferenced index ranges exist in 4 bundled meshes (13 104 indices, e.g. `tnk_type98`). They're leftover data; ignore them.
* **Geom meaning** (by convention, not stored):
  * vehicles and handheld weapons: G0 = 1st-person, G1 = 3rd-person, G2 = wreck
  * soldiers (skinned): G0 = 1P arms (bound to `1p_setup.ske`), G1 = 3P body (bound to `3p_setup.ske`) **[V]**
  * statics: G0 = intact, further geoms = destroyed variants
  * kits: one geom per kit
* LODs: LOD0 is the most detailed. There's no distance info in the file: switch distances come from the geometry template
  and engine constants (§2.12).

### 2.5 BLENDINDICES (`D3DCOLOR`, 4 bytes) [S][V]

The shaders use `D3DCOLORtoUBYTE4()`, which yields the bytes **in memory order** `b0 b1 b2 b3`:

| byte | staticmesh | bundledmesh | skinnedmesh |
|---|---|---|---|
| b0 | 0 (0/1 in 7 LODs, meaningless) | **geometry part index** (< `partCount`) | bone A (index into this material's rig) |
| b1 | 0 | 0 | bone B |
| b2 | **binormal flip** 0/1 | **binormal flip** 0/1 | **binormal flip** 0/1 |
| b3 | 0 | **animated-UV matrix index** 0..6 | 0 |

`binormal = normalize(cross(Tangent, Normal)) * (1 − 2*b2)` in BF2 space, from `RaShaderBM/SM/STM.fx`. Every b2 in the install is 0 or 1 **[V]**.

**Skinning weights:** `BLENDWEIGHT` (f1) is the weight of bone A. Bone B gets `1 − w` (`RaShaderSM.fx`). So there are at most 2
bones per vertex, and all weights are in [0, 1] **[V]**.

### 2.6 Alpha modes and pre-sorted index sets

* `alphaMode`: 0 opaque, 1 alpha blend, 2 alpha test. SkinnedMesh has no field; use the `Alpha_Test` technique token instead.
* For **alpha-blend (1)** materials of static/bundled meshes, the index buffer holds `alphaSortSets` (= 8) consecutive copies of
  the triangle list, each `inum` long, starting at `istart`. Each copy is depth-sorted for a different view direction
  **[V: index coverage adds up only with this rule]**. bf2-blender generates them with sorting planes whose normals are (0,0,−1)
  rotated by −(22.5° + i·45°) about Y. A renderer can use set 0 plus OIT or its own sorting.

### 2.7 Tangent space

* `TANGENT` is FLOAT3. There's no stored binormal: compute it as in 2.5.
* Tangents are **zero** in 25 556 static vertices (old files without normal maps), 2 342 bundled and 18 skinned vertices, and
  non-unit in others. Recompute them (MikkTSpace) when they're zero or the material has a normal map.
* For glTF after an axis mirror: `tangent.w = 1 − 2*b2` makes `cross(N', T')·w` equal the mirrored BF2 binormal (derivation:
  `S·cross(T,N) = cross(S·N, S·T)` for det(S) = −1). Whether BF2 normal-map green matches glTF green is **[?]**; see open questions.

### 2.8 Texture maps, techniques, UV channels

The `maps` list has no fixed slots. It often contains `Common\Textures\SpecularLUT_pow36.dds`, a shared lookup texture:
drop it before assigning slots **[V]**.

**StaticMesh.** The technique string is the concatenation of layers from `Base Detail Dirt Crack NDetail NCrack`, and the
maps appear **in that order**, followed by the optional LUT **[V]**. Suffixes are `_c, _de, _di, _cr, _deb, _crb`.
Techniques seen, as (count, number of maps including LUT):

```
BaseDetailNDetail (3521+775+90+50)   BaseDetailDirtNDetail (807+328)   Base (173+134+59+10)
BaseDetailCrackNDetailNCrack (168+4)  BaseDetailDirtCrackNDetailNCrack (112+48)   BaseDetail (82+26+11)
BaseDetailNDetailparallaxdetail (53)  BaseNDetail (15)  BaseDetailDirt (2)  alpha_one / alpha / alpha_cgfx / '' (rare junk)
```

* `parallaxdetail` suffix: parallax offset using the NDetail (detail normal) map's alpha as height, with a hard-coded scale of 0.0025 (`RaShaderSTM.fx`).
* `BaseNDetail` exists (the engine treats it like `BaseNBase` per bf2-blender).

UV channel per layer (per material). Evidence: shader permutation vertex requirements (`RaShaderSTM.mfx`), bf2-blender, and statistics:

| layer | TEXCOORD |
|---|---|
| Base | 0 |
| Detail, NDetail | 1 |
| Dirt | 2 |
| Crack, NCrack | 3 if the technique contains Dirt, else 2 |
| **Lightmap** | **the last TEXCOORD set of the vertex declaration** (when it isn't one of the layer channels above) |

Lightmap evidence **[V]**:

* In meshes with 3, 4 or 5 UV sets, the last set is the only non-overlapping one (rasterised overlap ratio median 1.00 to 1.01).
* It lies fully inside [0,1] in 955/1016 files.
* Meshes with only 1–2 sets have no lightmap UVs.
* The exceptions are wire fences, ladders and pagoda_01.
* Crack-only materials: their UV2 has the decal-like signature (75 % in [0,1], small area), which supports the crack = 2 rule.
* The 3ds Max exporter often writes all 5 sets even when layers are unused. Unused sets contain junk or copies.

Other StaticMesh rendering notes:

* Lightmap UVs are transformed per instance by `uv * LightMapOffset.xy + LightMapOffset.zw` (atlas placement from the
  level's `lightmaps/Objects/LightmapAtlas.tai`) **[S]**.
* Lightmap channels (per bf2-blender): G = sun, B = sky/ambient, R = point lights.
* Diffuse = Base × Detail (× Dirt), with Crack alpha-blended on top. Gloss comes from Detail alpha (not with alpha test).
* Alpha test uses Detail alpha (or Base alpha if there's no Detail).
* Meshes whose path contains `vegitation` (sic) use the leaf/trunk shaders, render two-sided and have no lightmap. The engine detects them by path (bf2-blender).

**BundledMesh.** The maps are `[Color _c] [Normal _b] SpecularLUT [Wreck _w]` (the most common pattern: c,b,LUT 750; c,LUT 522;
c,LUT,w 193) **[V]**. Assign slots by position after dropping the LUT, using the suffix to tell Normal from Wreck.
Only UV0 is used. The Wreck map multiplies Color (mostly geom 2).

The technique is a set of tokens, matched case-insensitively as substrings:

* `ColormapGloss`: gloss is Color alpha instead of Normal alpha.
* `Alpha_Test`: with ColormapGloss, black Color = transparent.
* `EnvMap`, `AnimatedUV`, `Cockpit` (static lighting), `NoHemiLight`, `Alpha`, `Alpha_One`.

Seen: `ColormapGloss`, `''`, `EnvMapColormapGloss`, `AlphaEnvMap`, `Alpha`, `AnimatedUVColormapGloss`, `Alpha_TestColormapGloss`,
`CockpitNoHemiLight`, `Alpha_One`, … The `alphaMode` field, not the technique, sets the blend state.

**SkinnedMesh.** The maps are `[Color _c] [Normal] SpecularLUT` (c,b,LUT 413; c,os,LUT 204) **[V]**. The technique is
*material name + tokens* (e.g. `dropbag_tangent`, `ske1:lod2_us_head_humanskin`), so match tokens as case-insensitive substrings:

* `tangent`: tangent-space normal map (`_b`). Without it, the normal map is **object-space** (`_b_os`).
* `Alpha_Test`, `humanskin`, `EnvMap`.

Gloss is Normal alpha. Object-space normal maps store BF2-space normals: mirror their X (or Z) channel when you convert
axes. glTF can't express them, so bake to tangent space or use a custom material.

**Animated UV** (tank tracks and wheels; `AnimatedUV` technique; TEXCOORD1 present in the 8 such meshes) **[S]**:

```
uv = mul(float3(TEXCOORD1, 1), uvMatrix[b3]) + TEXCOORD0
```

For b3 = 0, or for static display, `uv = TEXCOORD0 + TEXCOORD1`. bf2-blender additionally scales TEXCOORD1 by the texture aspect
ratio (`min(w,h)/w`, `min(w,h)/h`) for the rotating parts. TEXCOORD0 is the rotation centre.
b3: 1 L-wheel rotation, 2 L-wheel translation, 3 R-wheel rotation, 4 R-wheel translation, 5 R-track translation, 6 L-track
translation (bf2-blender `AnimUv`). The engine drives the 8 matrices.

### 2.9 Per-type LOD data semantics

* **StaticMesh nodes:** 1, 2 or 5 matrices per LOD. They're **unused**: vertices are already in object space. LOD1..n line up
  with LOD0 without applying them, even when node 0 is a non-identity translation (e.g. `fence_corrugated_3x12m_end` G0L1, `xp1_cliff_1`) **[V]**.
* **BundledMesh parts:** `partCount` equals `max(b0)+1` (1300 LODs) or is larger when some parts have no vertices (11) **[V]**.
  **Part vertices are in part-local space.** For example, the LAV-25's 8 wheel parts all have bounds ±0.49 around the origin **[V]**.
  Parts are placed by the ObjectTemplate hierarchy in `.con`/`.tweak`:
  * `ObjectTemplate.geometryPart N` selects part N.
  * The parent's `ObjectTemplate.addTemplate` / `setPosition x/y/z` / `setRotation yaw/pitch/roll` places the child.
  * The root template is part 0.
  * Parts move at runtime (turrets, wheels, and handheld weapon parts animated through the `meshN` skeleton bones).
* **SkinnedMesh rigs:** normally one rig per material (`rigs[i]` for `materials[i]`). b0/b1 index into `rig.bones`.
  * `skeBoneIndex` indexes the `.ske` node list.
  * `invBind` is the **inverse bind matrix in row-vector form**: `v_bone = v_mesh · invBind`.
  * **The bind pose equals the `.ske` rest pose** once the quaternion convention of §4 is used. Checks:
    1P mesh vs `1p_setup` exact; 3P mesh vs `3p_setup` max 0.014; flags vs `flag_setup` exact (`bf2anim.py --selftest`) **[V]**.
  * One legacy file (`antennaflag.skinnedmesh`, v4) has 1 rig for 2 materials. The second material's indices fit rig 0, so reuse the last rig.
  * Skinning (row-vector): `v' = Σ_i w_i · v · invBind_i · BoneWorld_i`.

### 2.10 Orphan `gpu_*` files (xpack, not referenced by any `.con`)

Header `u1 = 69 / 819`, `u2 = 10`. Their LOD block has `f32, f32, u8` instead of the v≤6 vec3. The staticmesh one also has
`vec3 bmin, bmax, u32, u32, u8` after `u5`. These files have zero normals (967 vertices), so treat them as test exports.
The parser handles them.

### 2.11 Data quirks to sanitise [V]

* 7 NaN UVs, referenced by triangles, in `Vehicles/Land/USAPC_LAV25/Meshes/USAPC_LAV25.bundledMesh`.
* 19 zero normals in 3 old (v4) staticmeshes. Zero or non-unit tangents: see §2.7.
* Detail UVs tile far outside [0,1] (95 k static vertices with |uv| > 64). This is normal, so use wrap addressing.
* Some material `fxFile`s don't match the extension (40 bundled materials say `StaticMesh.fx`). The engine ignores this.

### 2.12 LOD switch and cull distances (engine code, BF2 1.5)

Read from the `RendDX9.dll` and `BF2.exe` 1.5 disassembly; the constants are the ones those binaries use.

* `GeometryTemplate.setSubGeometryLodDistance <geom> <lod> <m>` is the camera distance at which LOD `lod` of geom `geom`
  hands over to `lod + 1` (entries 0..lodCount-2; 386 of 722 static geometries set them, e.g. `billboard_highway_01`:
  10, 23, 90 m). When the mesh loads, LODs without an entry get a running default: it starts at **50 m**, an unset LOD
  takes the running value and adds 50, a set LOD adds its own distance to it. Unset entries below a set one are
  zero-filled (`mosque`: LOD0 would never show); the importer treats them as unset.
* Selection: `lod = 1 + max{ i : dist[i] · globalStaticMeshLodDistanceScale · qualityScale < d / zoom }` (else 0), `d` the
  camera distance, `zoom = tan(fov₀/2) / tan(fov/2)` (1 unzoomed; fov₀ is the camera's first FOV). Highest geometry
  quality: both scales 1 (medium: quality scale 0.8; low: 0.5, distances ×2 and one LOD skipped via
  `forceStaticMeshSkipLod`). `GeometryTemplate.maxSkip3pLods` limits the skipping for geoms 1 and 2.
* Culling (the object manager in `BF2.exe`): cull radius `r = 0.8 · object radius · ObjectTemplate.cullRadiusScale`
  (default 1; 2 to 3.5 on long, flat statics such as walls, fences, sandbags, and on flags). With
  `D² = |camera − origin|² − r²` and `C² = zoom · max(10π · distanceCullConst² · r², minCullDistance²)`, the object is
  drawn fully up to `D² = 0.8 C²`, fades out until `1.2 C²`, and is hidden past the view distance plus `r`.
  `renderer.distanceCullConst` is 4 by default and 8 on high quality (`distanceCullConstPCO`, for soldiers and vehicles,
  5 and 10), `renderer.minCullDistance` 100 by default and 80 on high. So on high a static is drawn to about
  `max(35.9 · radius · cullRadiusScale, 80)` m.
* `ObjectTemplate.lodDistance High|Medium|Low` is only used by effects.
* What the importer makes of it (`bf2_import::lods`): LOD 1.. of the geoms a static object draws become
  `<mesh>_lod<N>.glb` (`_3p_lod<N>`, `_wreck_lod<N>`) listed in the template's parts as `lods` / `wreck_lods`
  with the distance each takes over at (the high-quality rule above), and the object's `draw_distance` is the
  distance from its origin where the cull fade is half done (`D² = C²`). Vegetation keeps neither (the tree code handles its distances). The client
  cross-fades between them with Bevy's `VisibilityRange` (±10 % around each distance), multiplies the switch
  distances by the zoom and the draw distances by its square root, and the view distance setting scales the draw
  distances.

## 3. Collision meshes: `.collisionmesh`

### 3.1 Layout (versions 8, 9, 10)

```
u32 u0 (0), u32 version
u32 partCount                                -- ObjectTemplate.collisionPart index (root = 0)
repeat part:
  u32 geomCount                              -- same meaning as the visible-mesh geom (vehicles: 0 = 1P usually empty, 1 = 3P, 2 = wreck)
  repeat geom:
    u32 colCount                             -- 0..5
    repeat col:
      (version >= 9) u32 colType             -- 0 projectile, 1 vehicle, 2 soldier, 3 AI, 4 unknown
      u32 faceCount
      repeat: u16 v0, v1, v2, material       -- 8 bytes
      u32 vertCount
      vec3 verts[vertCount]
      u16  vertMaterial[vertCount]           -- a material of some face using the vertex (98 % consistent)
      vec3 bmin, bmax                        -- AABB of verts
      u8   bspFlag                           -- ASCII '1' (0x31) = BSP follows, '0' (0x30) = none
      if bspFlag == '1':
        vec3 treeMin, treeMax
        u32  nodeCount ; node[nodeCount]     -- 16 bytes each, see 3.3
        u32  faceRefCount ; u16 faceRefs[]   -- face indices
        (version >= 10) u32 adjCount ; i32 faceAdj[adjCount]   -- adjCount = 3*faceCount
```

* **Version 8** has no `colType`. The type is implicit, equal to the col's list index (seen lists: `0`, `0,1`, `0,1,2`) **[B/?]**.
* **Version 9** adds `colType`. Types can skip: seen sequences include `0,2`, `0,1,3`, `0,1,3,4`, `2`, so always use the stored value.
* **Version 10** adds face adjacency, but **only when the BSP is present**. The single v10 col without BSP
  (`xp1_powerswitch_base` part 0 col 1, used in 4 SF maps) has no adjacency block. bf2-blender reads it unconditionally and would
  fail on this file **[V]**.
* `faceAdj[3*f + k]` = the face sharing edge k of face f, with edges `(v0,v1)`, `(v1,v2)`, `(v2,v0)`. −1 means an open edge
  (485 k entries checked, 0 mismatches) **[V]**. bf2-blender says only debug drawing uses it.

### 3.2 Types and materials

* **colType:**
  * 0 = projectile (detailed; used for bullets and hit detection)
  * 1 = vehicle (hull only)
  * 2 = soldier (includes interiors and stairs; building soldier meshes have more faces than vehicle meshes in 46/51 cases, and never fewer) **[V]**
  * 3 = AI (navmesh generation only)
  * 4 = unknown **[?]**: 192 cols, always a 1–15-face vertical "blade" in the object's mid-plane (fences, walls, poles, trees). bf2-blender rejects it.
* Face `material` is a local index. The `.con` maps it: `ObjectTemplate.mapMaterial <localIndex> <MaterialName> <?>` (e.g. `0 Metal_solid`),
  where the names refer to the global material table. Max local index seen: 63.
* Faces are usually sorted by material (not in 756 cols). There are 10 694 degenerate faces; tolerate them.

### 3.3 BSP node (16 bytes): an axis-aligned kd-tree [V]

```
f32 split
u32 flags     bits 0-1: axis (0 X, 1 Y, 2 Z)
              bit 2: child 0 is a leaf, leaf size = (flags >> 16) & 0xFF
              bit 3: child 1 is a leaf, leaf size = (flags >> 24) & 0xFF
u32 a         child 0: node index, or first faceRef if leaf
u32 b         child 1: node index, or first faceRef if leaf
```

* Child 0 holds faces with `coord[axis] <= split`, child 1 faces with `>= split`. Faces straddling the plane appear in both.
* Node 0 is the root. Every tree has exactly one root.
* Leaf ranges are always within `faceRefs`.

A reimplementation can ignore the BSP and build its own BVH, but it still has to parse the BSP to skip it.

### 3.4 Legacy versions (not supported, all unreferenced) [V]

* **v4:** `HMG_M134_Barrels/Minigun.collisionMesh`. The template uses `hmg_m134.collisionmesh` instead.
* **v7:** `aircontroltowerstatic.collisionmesh` (in bf2 and xpack). It's not placed in any level. Faces, verts and bounds are
  laid out like v8 (no colType), but it has a plane-based BSP with 32-byte per-face records.

## 4. Skeleton: `.ske` (version 2)

```
u32 version (2)
u32 nodeCount
repeat nodeCount:
  u16  nameLen            includes trailing NUL
  u8   name[nameLen]
  i16  parent             -1 = root; always < own index (parents precede children) [V]
  f32  qx, qy, qz, qw     quaternion, see below
  f32  px, py, pz         translation relative to parent
```

**Quaternion convention [V]:** the file stores the **conjugate** (inverse) of the local rotation. The true local rotation is
`r = (−qx, −qy, −qz, qw)`:

* column vectors: `p_parent = R(r)·p_local + t`, and `World = World_parent · Local`
* D3D row vectors: `Local = D3DXMatrixRotationQuaternion(r) · T(t)`, and `World = Local · World_parent`

Using `q` unconjugated gives bind-pose errors of about 1.25 (matrix units); the conjugate gives 0.00003 (median). This matches
bf2-blender, which calls `rot.invert()` on load and negates xyz in `.baf`. In glam:
`Mat4::from_rotation_translation(Quat::from_xyzw(-x,-y,-z,w), t)`. For glTF see §1.

Skeletons in the install: `1p_setup` (70 bones; roots `Camerabone`, `torus`), `3p_setup` (80), `flag_setup`/`flag_setup_wall`
(25), `parachute_setup` (32), `wind_cone` (8), `Oil_Pump_Setup` (5), `cloth_line_setup`, `antenna_setup`, `antennaflag_setup` (3).

The weapon-part bones `mesh1..mesh8` (3P) and `mesh1..mesh16` (1P) carry **uninitialised garbage** transforms (24 bones:
8 in 3P, 16 in 1P; e.g. denormals and 4e36). Treat them as identity: animations or code drive them. In `3p_setup`, `mesh9..mesh16` are kit attachment bones with valid transforms.

## 5. Animation: `.baf` (version 4)

```
u32 version (4)
u16 boneCount
u16 boneIds[boneCount]        indices into the .ske node list
u32 frameCount                2..999 seen (median 26)
u8  precision                 fraction-bit setting for positions (12..15 seen; 15: 5637 files, 14: 1713)
repeat boneCount:
  u16 dataSize                sum of the 7 streamSize values below [V, always]
  repeat 7 streams (qx, qy, qz, qw, px, py, pz):
    u16 streamSize            in 16-bit words, including 1 word per block header
    blocks until streamSize consumed:
      u8 head                 bit 7 = RLE, bits 0-6 = n frames (1..127)
      u8 blockWords           RLE: always 2; raw: always n+1 [V]
      RLE: u16 value           -> repeated n times
      raw: u16 value[n]
```

**Decoding.** Each stream expands to exactly `frameCount` values **[V]**. Values are **two's-complement int16**. The DICE files match
two's complement bit-exactly; bf2-blender's `word − 0xFFFF` variant matched 0/2113 samples **[V]**.

```
rot component = int16 / 32767.0                                  (precision fixed at 15)
pos component = int16 * 2^(15 − precision) / 32767.0             (range ±2^(15−precision))
```

Decoded quaternions have unit length to within 1e-4 **[V]**. They and the positions use **exactly the .ske convention** (stored
conjugate, parent-relative). A key equal to the rest pose decodes to the `.ske` values, and bone lengths match the skeleton within
1 % for 98 % of samples **[V]**.

Keys **replace** the local transform of the listed bones. Bones not in `boneIds` keep their rest (or other layer) transform.
Precision: about 3e-5 for rotation, and 3e-5·2^(15−p) m for position. Playback rate: 24 fps (bf2-blender: "BF2 hardcoded default") **[B]**.

Encoder (for writing): choose the largest precision whose range covers max|pos|, and truncate toward zero (DICE's data looks truncated **[?]**).
Emit RLE blocks for runs longer than 5, and split blocks at 126 frames (bf2-blender).

## 6. Level `.mesh` files = compiled roads

`Levels/<map>/client.zip: Roads/<name>_compiled.mesh`. The level's `server.zip:CompiledRoads.con` lists them:

```
object.create highway
object.geometry.loadMesh Levels\Strike_at_karkand\Roads\tarmac8_compiled.mesh
object.absoluteposition -99.9473/163.404/-311.777
```

The template's textures come from `Objects_client.zip: Roads/Splines/<template>_compiled.dat`. That file holds a primary texture
path, a newline, a secondary texture path and a newline (append `.dds`), then `f32 blendFactor (0.85)`, `f32 200`, `f32 300`
(fade distances **[?]**) and `u32 1`. The layout is:

```
u16 version (4 for every referenced file), u16 flags (1)
vec3 position              == object.absoluteposition (977/980) [V]
f32  radius                == max |local vertex| [V]
vec3 bmin, bmax            world-space AABB
u32  unknown               0..10 (not the template's SetPrio) [?]
u32  vertexCount
repeat: f32 x,y,z (local, add position), f32 u0,v0 (primary texture), f32 u1,v1 (secondary; shader samples tex1*0.1 in
        one path), f32 alpha (blend/fade, mostly 0..1, extrapolated values up to +-21 -> clamp)     -- 32 bytes, RoadCompiled.fx decl [S]
u32  indexCount ; u16 indices[]   (triangle list)
u32  patchCount ; repeat: u32 istart, u32 icount, f32 radius, vec3 centerWorld   -- culling spheres covering the index buffer
```

Use the same coordinate conversion as meshes. Roads are drawn as a decal over the terrain (blend by alpha).

* All **980 referenced** road meshes are v4 and parse 100 % **[V]**.
* 378 unreferenced leftovers:
  * v4: 142, parse
  * v2: 167, parse (56-byte vertex: pos3, side vector3, uv0, uv1, 4×f32 0; no patch table)
  * v0, v1, v3 and 5 files with garbage headers: 69, not decoded (unreferenced)

## 7. Empirical results

Commands: `python bf2mesh.py`, `python bf2mesh.py --roads`, `python bf2coll.py`, `python bf2anim.py`, `python bf2anim.py --selftest`.

### Visible meshes: parsed to EOF / passed validation / total

| archive | staticmesh | bundledmesh | skinnedmesh |
|---|---|---|---|
| mods\bf2\Objects_client.zip | 872/872/872 | 451/451/451 | 24/24/24 |
| mods\xpack\Objects_client.zip | 275/275/275 | 64/64/64 | 56/56/56 |
| mods\bf2\Booster_client.zip | 231/231/231 | 55/55/55 | – |
| mods\bf2\Common_client.zip | 2/2/2 | – | – |
| **total** | **1380 (100 %)** | **570 (100 %)** | **80 (100 %)** |

Validation checks:

* declaration consistency
* indices < material vnum
* index/vertex ranges in bounds
* LOD and material AABBs exact
* blend indices within rig / part count
* binormal flag ∈ {0,1}
* weights ∈ [0,1]
* winding vs normals

One warning: `antennaflag` has a rig shared by two materials.

### Collision meshes

| archive | OK / total | failures |
|---|---|---|
| mods\bf2\Objects_client.zip | 1183 / 1186 | v4 ×2, v7 ×1 (legacy, unreferenced) |
| mods\bf2\Objects_server.zip | 1183 / 1186 | byte-identical copies of the client files |
| mods\xpack\Objects_server.zip | 286 / 287 | v7 aircontroltowerstatic copy |
| mods\bf2\Booster_server.zip | 264 / 264 | |

That is 1733/1737 unique files (99.8 %), and 100 % of referenced files. Versions: v8 241, v9 112, v10 1380 (plus the legacy v4 ×2 and v7 ×2).

Checks:

* face indices < vertCount
* AABB
* BSP single root, children and leaf ranges in range
* adjacency length = 3·faces, and every adjacency edge is shared (0 mismatches)

The xpack `Objects_client.zip` contains no collision meshes, skeletons or animations; they're in xpack `Objects_server.zip`.

### Skeletons and animations

* `.ske`: 11/11 unique (bf2 9, xpack 2; bf2 client and server copies identical).
* `.baf`: 3470/3470 (bf2), 422/422 (xpack), 1/1 (booster), so 3893/3893 unique = 100 %.
* 1.96 M RLE blocks and 0.63 M raw blocks decoded. Block-size fields are always consistent.

### Aggregate data statistics

| mesh type | triangles agreeing with vertex normal | disagreeing |
|---|---|---|
| static | 5 344 419 | 19 891 |
| bundled | 1 555 677 | 12 101 |
| skinned | 424 255 | 2 055 |

Other anomalies are listed in §2.11.

## 8. Open questions

1. **Normal-map green channel / bitangent sign under glTF.** In BF2 space the binormal is `cross(T,N)·flip`, which per
   bf2-blender points toward decreasing V (image up). Check a normal-mapped vehicle visually after conversion, and negate
   `tangent.w` (or the green channel) if bumps look inverted.
2. **colType 4** semantics (AI or navmesh helper?). It's safe to ignore for physics.
3. **Version-8 collision colType** is assumed to equal the list index (bf2-blender doesn't support v8).
4. **Crack UV channel** in 5-set meshes for crack-without-dirt materials: the per-technique rule (UV2) is supported by statistics,
   but the engine code wasn't seen. 5 of 156 such materials have a constant UV2 and a varying UV3.
5. Lightmap UV when the last set coincides with a layer channel (e.g. ladder meshes with 3 sets and Dirt). Probably not lightmapped.
6. `.baf` fps (24 per bf2-blender) and the road header `unknown` u32 / `.dat` trailing floats are unverified.
7. How the engine uses `staticmesh` node matrices: data shows they're not needed for placement.
8. LOD selection (§2.12): the per-geom offset subtracted from the LOD distance (assumed 0), and which radius the object
   manager's cull uses (the importer takes the farthest bounding box corner from the origin).

## 9. Sources

* marekzajac97/bf2-blender (`io_scene_bf2/core/bf2/bf2_mesh/*.py`, `bf2_collmesh.py`, `bf2_skeleton.py`, `bf2_animation.py`,
  `core/mesh.py`, `core/material.py`, `core/utils.py`, `docs/BF2.md`), cloned 2026-09-26.
* The game's shaders: `mods/bf2/Shaders_client.zip` (`RaShaderBM.fx`, `RaShaderSM.fx`, `RaShaderSTM.fx`, `RaShaderSTM.mfx`, `RoadCompiled.fx`).
* Empirical analysis of every file in the install with the parsers in this directory.
