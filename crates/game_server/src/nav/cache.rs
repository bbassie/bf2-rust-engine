//! `navgrid_<mode>_<size>.bin`: a built grid, so later loads of the level skip the build.
//!
//! Layout: magic, the geometry key (see [`super::build::geometry_key`]), then deflated:
//! cell size, origin (3 x f32), width, depth, cell count (u32), the column offsets, the
//! cells, the ladder count (u32) and the ladders (see [`LADDER_WORDS`]).

use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use anyhow::ensure;
use bevy::prelude::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

use super::{CellRef, NavCell, NavGrid, NavLadder, NavParams};

const MAGIC: &[u8; 8] = b"BF2NAVGR";

/// 4-byte words per ladder: both cells (x, z, index), foot, head and front (x, y, z), and
/// whether it goes down too.
const LADDER_WORDS: usize = 16;

fn ladder_words(ladder: &NavLadder) -> [u32; LADDER_WORDS] {
    let cell = |c: CellRef| [c.x, c.z, c.index];
    let vec = |v: Vec3| v.to_array().map(f32::to_bits);
    let mut words = [0; LADDER_WORDS];
    words[..3].copy_from_slice(&cell(ladder.bottom));
    words[3..6].copy_from_slice(&cell(ladder.top));
    words[6..9].copy_from_slice(&vec(ladder.foot));
    words[9..12].copy_from_slice(&vec(ladder.head));
    words[12..15].copy_from_slice(&vec(ladder.front));
    words[15] = ladder.down as u32;
    words
}

fn ladder_from_words(w: &[u32]) -> NavLadder {
    let cell = |i: usize| CellRef { x: w[i], z: w[i + 1], index: w[i + 2] };
    let vec = |i: usize| Vec3::new(f32::from_bits(w[i]), f32::from_bits(w[i + 1]), f32::from_bits(w[i + 2]));
    NavLadder {
        bottom: cell(0),
        top: cell(3),
        foot: vec(6),
        head: vec(9),
        front: vec(12),
        down: w[15] != 0,
    }
}

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
    z.write_all(&(grid.ladders.len() as u32).to_le_bytes())?;
    for ladder in &grid.ladders {
        z.write_all(bytemuck::cast_slice(&ladder_words(ladder)))?;
    }
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

/// A cached grid whatever geometry it was built from (tests with imported levels).
#[cfg(test)]
pub fn load_unchecked(path: &Path, params: NavParams) -> Option<NavGrid> {
    let bytes = fs::read(path).ok()?;
    (bytes.len() >= 16 && &bytes[..8] == MAGIC).then_some(())?;
    parse(&bytes[16..], params).ok()
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
    let ladders_at = HEADER + columns_len + cells_len;
    ensure!(data.len() >= ladders_at + 4, "truncated");
    let ladder_count = u32_at(ladders_at) as usize;
    ensure!(data.len() == ladders_at + 4 + ladder_count * LADDER_WORDS * 4, "wrong size");
    let columns: Vec<u32> = bytemuck::pod_collect_to_vec(&data[HEADER..HEADER + columns_len]);
    let cells: Vec<NavCell> = bytemuck::pod_collect_to_vec(&data[HEADER + columns_len..ladders_at]);
    let words: Vec<u32> = bytemuck::pod_collect_to_vec(&data[ladders_at + 4..]);
    let ladders: Vec<NavLadder> = words.chunks_exact(LADDER_WORDS).map(ladder_from_words).collect();
    ensure!(
        ladders.iter().all(|l| (l.bottom.index as usize) < count && (l.top.index as usize) < count),
        "bad ladder"
    );
    let mut ladder_ends: bevy::platform::collections::HashMap<u32, Vec<u16>> = default();
    for (i, ladder) in ladders.iter().enumerate() {
        ladder_ends.entry(ladder.bottom.index).or_default().push(i as u16);
        ladder_ends.entry(ladder.top.index).or_default().push(i as u16);
    }
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
        ladders,
        ladder_ends,
    })
}
