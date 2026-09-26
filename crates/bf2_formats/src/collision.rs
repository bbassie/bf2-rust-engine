//! Collision meshes: `.collisionmesh` (versions 8, 9 and 10).
//!
//! Structure: parts (`ObjectTemplate.collisionPart`) → geoms → cols. Each col is a
//! triangle mesh for one purpose (projectiles, vehicles, soldiers, AI).

use crate::reader::{ReadError, Reader};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColType {
    /// Detailed mesh for bullets and hit detection.
    Projectile,
    /// Simplified hull for vehicle collisions.
    Vehicle,
    /// For soldiers, including interiors and stairs.
    Soldier,
    /// Navmesh generation only.
    Ai,
    Unknown(u32),
}

impl ColType {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Projectile,
            1 => Self::Vehicle,
            2 => Self::Soldier,
            3 => Self::Ai,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Col {
    pub col_type: ColType,
    /// `v0, v1, v2, local material index`. Winding points *into* the solid.
    pub faces: Vec<[u16; 4]>,
    pub vertices: Vec<[f32; 3]>,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

#[derive(Clone, Debug, Default)]
pub struct CollisionGeom {
    pub cols: Vec<Col>,
}

#[derive(Clone, Debug, Default)]
pub struct CollisionPart {
    pub geoms: Vec<CollisionGeom>,
}

#[derive(Clone, Debug)]
pub struct CollisionMesh {
    pub version: u32,
    pub parts: Vec<CollisionPart>,
}

impl CollisionMesh {
    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let _u0 = r.u32()?;
        let version = r.u32()?;
        if !(8..=10).contains(&version) {
            return Err(ReadError::Invalid(format!(
                "unsupported collision mesh version {version}"
            )));
        }
        let part_count = r.count(4)?;
        let mut parts = Vec::with_capacity(part_count);
        for _ in 0..part_count {
            let geom_count = r.count(4)?;
            let mut geoms = Vec::with_capacity(geom_count);
            for _ in 0..geom_count {
                let col_count = r.count(4)?;
                let mut cols = Vec::with_capacity(col_count);
                for index in 0..col_count {
                    let col_type = if version >= 9 {
                        ColType::from_u32(r.u32()?)
                    } else {
                        ColType::from_u32(index as u32)
                    };
                    let face_count = r.count(8)?;
                    let mut faces = Vec::with_capacity(face_count);
                    for _ in 0..face_count {
                        faces.push([r.u16()?, r.u16()?, r.u16()?, r.u16()?]);
                    }
                    let vert_count = r.count(12)?;
                    let vertices: Vec<[f32; 3]> =
                        (0..vert_count).map(|_| r.vec3()).collect::<Result<_, _>>()?;
                    r.skip(vert_count * 2)?; // per-vertex material
                    let bounds_min = r.vec3()?;
                    let bounds_max = r.vec3()?;
                    let has_bsp = r.u8()? == b'1';
                    if has_bsp {
                        r.skip(24)?; // tree bounds
                        let nodes = r.count(16)?;
                        r.skip(nodes * 16)?;
                        let refs = r.count(2)?;
                        r.skip(refs * 2)?;
                        if version >= 10 {
                            let adjacency = r.count(4)?;
                            r.skip(adjacency * 4)?;
                        }
                    }
                    if let Some(bad) = faces
                        .iter()
                        .find(|f| f[..3].iter().any(|&i| i as usize >= vertices.len()))
                    {
                        return Err(ReadError::Invalid(format!(
                            "face {bad:?} references a vertex out of range ({} vertices)",
                            vertices.len()
                        )));
                    }
                    cols.push(Col {
                        col_type,
                        faces,
                        vertices,
                        bounds_min,
                        bounds_max,
                    });
                }
                geoms.push(CollisionGeom { cols });
            }
            parts.push(CollisionPart { geoms });
        }
        if r.remaining() != 0 {
            return Err(ReadError::Invalid(format!(
                "{} trailing bytes after collision data",
                r.remaining()
            )));
        }
        Ok(Self { version, parts })
    }
}
