//! Compiled roads: `Levels/<map>/Roads/<name>_compiled.mesh` (version 4) and the road
//! template's `Roads/Splines/<template>_compiled.dat` texture list.
//!
//! Roads are meshes draped over the terrain and alpha-blended onto it.

use crate::reader::{ReadError, Reader};

#[derive(Clone, Copy, Debug)]
pub struct RoadVertex {
    /// Relative to [`CompiledRoad::position`].
    pub position: [f32; 3],
    /// Primary texture coordinates.
    pub uv0: [f32; 2],
    /// Secondary texture coordinates.
    pub uv1: [f32; 2],
    /// Blend towards the terrain at the road's edges (clamp to 0..1).
    pub alpha: f32,
}

#[derive(Clone, Debug)]
pub struct CompiledRoad {
    /// World position the vertices are relative to.
    pub position: [f32; 3],
    pub vertices: Vec<RoadVertex>,
    /// Triangle list.
    pub indices: Vec<u16>,
}

impl CompiledRoad {
    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let version = r.u16()?;
        let _flags = r.u16()?;
        if version != 4 {
            return Err(ReadError::Invalid(format!(
                "unsupported road mesh version {version}"
            )));
        }
        let position = r.vec3()?;
        let _radius = r.f32()?;
        r.skip(24)?; // world bounds
        let _unknown = r.u32()?;
        let vertex_count = r.count(32)?;
        let mut vertices = Vec::with_capacity(vertex_count);
        for _ in 0..vertex_count {
            vertices.push(RoadVertex {
                position: r.vec3()?,
                uv0: [r.f32()?, r.f32()?],
                uv1: [r.f32()?, r.f32()?],
                alpha: r.f32()?,
            });
        }
        let index_count = r.count(2)?;
        let indices: Vec<u16> = (0..index_count).map(|_| r.u16()).collect::<Result<_, _>>()?;
        if let Some(bad) = indices.iter().find(|&&i| i as usize >= vertices.len()) {
            return Err(ReadError::Invalid(format!("road index {bad} out of range")));
        }
        // Culling patches follow; not needed.
        Ok(Self {
            position,
            vertices,
            indices,
        })
    }
}

/// Textures of a road template, from `<template>_compiled.dat`.
#[derive(Clone, Debug)]
pub struct RoadTextures {
    /// VFS paths without extension.
    pub primary: String,
    pub secondary: String,
    /// `color = lerp(secondary, primary, blend)`.
    pub blend: f32,
}

impl RoadTextures {
    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let text_end = |from: usize| data[from..].iter().position(|&b| b == b'\n').map(|p| from + p);
        let first = text_end(0).ok_or_else(|| ReadError::Invalid("missing primary texture".into()))?;
        let second = text_end(first + 1).ok_or_else(|| ReadError::Invalid("missing secondary texture".into()))?;
        let latin1 = |bytes: &[u8]| bytes.iter().map(|&b| b as char).collect::<String>();
        let mut r = Reader::new(&data[second + 1..]);
        Ok(Self {
            primary: latin1(&data[..first]).trim().to_string(),
            secondary: latin1(&data[first + 1..second]).trim().to_string(),
            blend: r.f32().unwrap_or(0.85),
        })
    }
}
