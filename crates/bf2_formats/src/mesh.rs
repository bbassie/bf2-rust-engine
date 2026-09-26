//! Visible meshes: `.staticmesh`, `.bundledmesh`, `.skinnedmesh`.
//!
//! All three share one container: a D3D vertex declaration, one vertex and one index buffer,
//! then per geom/LOD blocks and per geom/LOD material lists. Coordinates are BF2's
//! left-handed space (+X right, +Y up, +Z forward); converting is the caller's job.

use crate::reader::{ReadError, Reader};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshKind {
    Static,
    Bundled,
    Skinned,
}

impl MeshKind {
    /// From a file name's extension.
    pub fn from_path(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".staticmesh") {
            Some(Self::Static)
        } else if lower.ends_with(".bundledmesh") {
            Some(Self::Bundled)
        } else if lower.ends_with(".skinnedmesh") {
            Some(Self::Skinned)
        } else {
            None
        }
    }
}

/// `D3DDECLUSAGE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Position,
    BlendWeight,
    BlendIndices,
    Normal,
    TexCoord,
    Tangent,
    Other(u8),
}

impl Usage {
    fn from_d3d(v: u8) -> Self {
        match v {
            0 => Self::Position,
            1 => Self::BlendWeight,
            2 => Self::BlendIndices,
            3 => Self::Normal,
            5 => Self::TexCoord,
            6 => Self::Tangent,
            other => Self::Other(other),
        }
    }
}

/// `D3DDECLTYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclType {
    Float1,
    Float2,
    Float3,
    Float4,
    D3dColor,
    Other(u16),
}

impl DeclType {
    fn from_d3d(v: u16) -> Self {
        match v {
            0 => Self::Float1,
            1 => Self::Float2,
            2 => Self::Float3,
            3 => Self::Float4,
            4 => Self::D3dColor,
            other => Self::Other(other),
        }
    }

    pub fn size(self) -> usize {
        match self {
            Self::Float1 | Self::D3dColor => 4,
            Self::Float2 => 8,
            Self::Float3 => 12,
            Self::Float4 => 16,
            Self::Other(_) => 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VertexElement {
    pub offset: u16,
    pub ty: DeclType,
    pub usage: Usage,
    pub usage_index: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlphaMode {
    #[default]
    Opaque,
    Blend,
    Test,
}

#[derive(Clone, Debug)]
pub struct Material {
    /// Static and bundled meshes only; skinned meshes use the technique string.
    pub alpha_mode: AlphaMode,
    pub fx_file: String,
    pub technique: String,
    /// Texture paths in the VFS, often without extension and with mixed slashes.
    pub maps: Vec<String>,
    /// First vertex; indices are relative to it.
    pub vstart: u32,
    pub istart: u32,
    /// Index count of one sort set.
    pub inum: u32,
    pub vnum: u32,
}

impl Material {
    /// Texture maps without the shared specular lookup texture.
    pub fn texture_maps(&self) -> Vec<&str> {
        self.maps
            .iter()
            .map(String::as_str)
            .filter(|m| !m.to_ascii_lowercase().contains("specularlut"))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct RigBone {
    /// Index into the `.ske` node list.
    pub ske_index: u32,
    /// Inverse bind matrix, D3D row-vector form.
    pub inverse_bind: [f32; 16],
}

#[derive(Clone, Debug, Default)]
pub struct Lod {
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    /// Staticmesh node matrices (unused by the engine).
    pub nodes: Vec<[f32; 16]>,
    /// Bundledmesh part count.
    pub part_count: u32,
    /// Skinnedmesh rigs, normally one per material.
    pub rigs: Vec<Vec<RigBone>>,
    pub materials: Vec<Material>,
}

#[derive(Clone, Debug, Default)]
pub struct Geom {
    pub lods: Vec<Lod>,
}

#[derive(Clone, Debug)]
pub struct VisMesh {
    pub kind: MeshKind,
    pub version: u32,
    pub elements: Vec<VertexElement>,
    pub stride: usize,
    pub vertex_count: usize,
    pub vertex_data: Vec<u8>,
    pub indices: Vec<u16>,
    pub geoms: Vec<Geom>,
}

impl VisMesh {
    pub fn parse(data: &[u8], kind: MeshKind) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let _u0 = r.u32()?;
        let version = r.u32()?;
        let u1 = r.u32()?;
        let _u2 = r.u32()?;
        let _u3 = r.u32()?;
        let _u4 = r.u8()?;
        if u1 != 0 {
            return Err(ReadError::Invalid(format!(
                "unsupported mesh header variant (u1 = {u1}); only test exports use it"
            )));
        }

        let geom_count = r.count(4)?;
        let lod_counts: Vec<usize> = (0..geom_count).map(|_| r.count(1)).collect::<Result<_, _>>()?;

        let element_count = r.count(8)?;
        let mut elements = Vec::with_capacity(element_count);
        for _ in 0..element_count {
            let flag = r.u16()?;
            let offset = r.u16()?;
            let ty = r.u16()?;
            let usage = r.u16()?;
            if flag == 0xFF || ty == 17 {
                continue;
            }
            elements.push(VertexElement {
                offset,
                ty: DeclType::from_d3d(ty),
                usage: Usage::from_d3d((usage & 0xFF) as u8),
                usage_index: (usage >> 8) as u8,
            });
        }

        let _vert_format = r.u32()?;
        let stride = r.u32()? as usize;
        if stride == 0 {
            return Err(ReadError::Invalid("vertex stride is 0".into()));
        }
        let vertex_count = r.count(stride)?;
        let vertex_data = r.bytes(vertex_count * stride)?.to_vec();
        let index_count = r.count(2)?;
        let indices = (0..index_count).map(|_| r.u16()).collect::<Result<_, _>>()?;

        if kind != MeshKind::Skinned {
            let _alpha_sort_sets = r.u32()?;
        }

        let mut geoms: Vec<Geom> = lod_counts
            .iter()
            .map(|&n| Geom {
                lods: vec![Lod::default(); n],
            })
            .collect();

        for geom in &mut geoms {
            for lod in &mut geom.lods {
                lod.bounds_min = r.vec3()?;
                lod.bounds_max = r.vec3()?;
                if version <= 6 {
                    let _radius = r.vec3()?;
                }
                match kind {
                    MeshKind::Static => {
                        let n = r.count(64)?;
                        lod.nodes = (0..n).map(|_| r.mat4()).collect::<Result<_, _>>()?;
                    }
                    MeshKind::Bundled => lod.part_count = r.u32()?,
                    MeshKind::Skinned => {
                        let rig_count = r.count(4)?;
                        for _ in 0..rig_count {
                            let bone_count = r.count(68)?;
                            let mut bones = Vec::with_capacity(bone_count);
                            for _ in 0..bone_count {
                                bones.push(RigBone {
                                    ske_index: r.u32()?,
                                    inverse_bind: r.mat4()?,
                                });
                            }
                            lod.rigs.push(bones);
                        }
                    }
                }
            }
        }

        for geom in &mut geoms {
            for lod in &mut geom.lods {
                let material_count = r.count(4)?;
                for _ in 0..material_count {
                    let alpha_mode = if kind == MeshKind::Skinned {
                        AlphaMode::Opaque
                    } else {
                        match r.u32()? {
                            1 => AlphaMode::Blend,
                            2 => AlphaMode::Test,
                            _ => AlphaMode::Opaque,
                        }
                    };
                    let fx_file = r.string()?;
                    let technique = r.string()?;
                    let map_count = r.count(4)?;
                    let maps = (0..map_count).map(|_| r.string()).collect::<Result<_, _>>()?;
                    let vstart = r.u32()?;
                    let istart = r.u32()?;
                    let inum = r.u32()?;
                    let vnum = r.u32()?;
                    let _u4 = r.u32()?;
                    let _u5 = r.u32()?;
                    if kind == MeshKind::Static && version == 11 {
                        let _bmin = r.vec3()?;
                        let _bmax = r.vec3()?;
                    }
                    let alpha_mode = if kind == MeshKind::Skinned
                        && technique.to_ascii_lowercase().contains("alpha_test")
                    {
                        AlphaMode::Test
                    } else {
                        alpha_mode
                    };
                    lod.materials.push(Material {
                        alpha_mode,
                        fx_file,
                        technique,
                        maps,
                        vstart,
                        istart,
                        inum,
                        vnum,
                    });
                }
            }
        }

        if r.remaining() != 0 {
            return Err(ReadError::Invalid(format!(
                "{} trailing bytes after mesh data",
                r.remaining()
            )));
        }

        Ok(Self {
            kind,
            version,
            elements,
            stride,
            vertex_count,
            vertex_data,
            indices,
            geoms,
        })
    }

    pub fn element(&self, usage: Usage, index: u8) -> Option<VertexElement> {
        self.elements
            .iter()
            .copied()
            .find(|e| e.usage == usage && e.usage_index == index)
    }

    /// Number of `TEXCOORD` sets in the declaration.
    pub fn texcoord_sets(&self) -> u8 {
        self.elements
            .iter()
            .filter(|e| e.usage == Usage::TexCoord)
            .map(|e| e.usage_index + 1)
            .max()
            .unwrap_or(0)
    }

    fn element_bytes(&self, vertex: usize, element: VertexElement) -> &[u8] {
        let start = vertex * self.stride + element.offset as usize;
        &self.vertex_data[start..start + element.ty.size()]
    }

    /// Reads a float element of one vertex (up to 4 components; missing ones are 0).
    pub fn read_floats(&self, vertex: usize, element: VertexElement) -> [f32; 4] {
        let bytes = self.element_bytes(vertex, element);
        let mut out = [0.0; 4];
        for (i, chunk) in bytes.chunks_exact(4).take(4).enumerate() {
            out[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        out
    }

    /// Reads a `D3DCOLOR` element as bytes in memory order.
    pub fn read_bytes4(&self, vertex: usize, element: VertexElement) -> [u8; 4] {
        let b = self.element_bytes(vertex, element);
        [b[0], b[1], b[2], b[3]]
    }

    /// All values of a float attribute.
    pub fn attribute<const N: usize>(&self, usage: Usage, index: u8) -> Option<Vec<[f32; N]>> {
        let element = self.element(usage, index)?;
        Some(
            (0..self.vertex_count)
                .map(|v| {
                    let f = self.read_floats(v, element);
                    std::array::from_fn(|i| f[i])
                })
                .collect(),
        )
    }

    /// Blend indices of all vertices.
    pub fn blend_indices(&self) -> Option<Vec<[u8; 4]>> {
        let element = self.element(Usage::BlendIndices, 0)?;
        Some(
            (0..self.vertex_count)
                .map(|v| self.read_bytes4(v, element))
                .collect(),
        )
    }

    /// Triangles of a material as absolute vertex indices (first sort set only).
    pub fn material_triangles(&self, material: &Material) -> Vec<[u32; 3]> {
        let start = material.istart as usize;
        let end = (start + material.inum as usize).min(self.indices.len());
        self.indices[start.min(end)..end]
            .chunks_exact(3)
            .map(|t| {
                [
                    material.vstart + t[0] as u32,
                    material.vstart + t[1] as u32,
                    material.vstart + t[2] as u32,
                ]
            })
            .collect()
    }
}
