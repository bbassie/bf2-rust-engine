//! `navgrid_<mode>_<size>.bin`: a built grid, so later loads of the level skip the build.
//!
//! Layout: magic, the geometry key (see [`super::build::geometry_key`]), then deflated:
//! cell size, origin (3 x f32), width, depth, cell count (u32), the column offsets and the
//! cells.

use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use anyhow::ensure;
use bevy::prelude::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

use super::{NavCell, NavGrid, NavParams};

const MAGIC: &[u8; 8] = b"BF2NAVGR";

pub fn save(path: &Path, key: u64, grid: &NavGrid) -> anyhow::Result<()> {
    // Written next to it and renamed, so a crash or a second process never leaves a torn file.
    let tmp = path.with_extension("bin.tmp");
    let mut out = std::io::BufWriter::new(fs::File::create(&tmp)?);
    out.write_all(MAGIC)?;
    out.write_all(&key.to_le_bytes())?;
    let mut z = DeflateEncoder::new(out, Compression::fast());
    for v in [grid.params.cell, grid.origin.x, grid.origin.y] {
        z.write_all(&v.to_le_bytes())?;
    }
    for v in [grid.width, grid.depth, grid.cells.len() as u32] {
        z.write_all(&v.to_le_bytes())?;
    }
    z.write_all(bytemuck::cast_slice(&grid.columns))?;
    z.write_all(bytemuck::cast_slice(&grid.cells))?;
    z.finish()?.flush()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The cached grid, if there is one for this key.
pub fn load(path: &Path, key: u64, params: NavParams) -> Option<NavGrid> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 16 || &bytes[..8] != MAGIC || bytes[8..16] != key.to_le_bytes() {
        return None;
    }
    parse(&bytes[16..], params)
        .map_err(|err| warn!("nav: ignoring {}: {err:#}", path.display()))
        .ok()
}

fn parse(compressed: &[u8], mut params: NavParams) -> anyhow::Result<NavGrid> {
    const HEADER: usize = 24;
    let mut data = Vec::new();
    DeflateDecoder::new(compressed).read_to_end(&mut data)?;
    ensure!(data.len() >= HEADER, "truncated");
    let f32_at = |i: usize| f32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    let u32_at = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
    params.cell = f32_at(0);
    let origin = Vec2::new(f32_at(4), f32_at(8));
    let (width, depth, count) = (u32_at(12), u32_at(16), u32_at(20) as usize);
    let columns_len = (width as usize * depth as usize + 1) * 4;
    let cells_len = count * size_of::<NavCell>();
    ensure!(data.len() == HEADER + columns_len + cells_len, "wrong size");
    let columns: Vec<u32> = bytemuck::pod_collect_to_vec(&data[HEADER..HEADER + columns_len]);
    let cells: Vec<NavCell> = bytemuck::pod_collect_to_vec(&data[HEADER + columns_len..]);
    ensure!(
        columns.last() == Some(&(count as u32)),
        "inconsistent columns"
    );
    Ok(NavGrid {
        params,
        origin,
        width,
        depth,
        columns,
        cells,
    })
}
