//! `navgrid_<mode>_<size>.bin`: a built grid, so later loads of the level skip the build.
//!
//! Layout: magic, the geometry key (see [`super::build::geometry_key`]), then deflated:
//! cell size, origin (2 x f32), width, depth, cell count (u32), the column offsets (of the
//! level grid and the patches), the cells, the ladder count (u32) and the ladders (see
//! [`LADDER_WORDS`]), the level grid's cell count, the patch count and the patches (see
//! [`PATCH_WORDS`]), the portal count and the portals (from, to x, z, index).

use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use anyhow::ensure;
use bevy::prelude::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

use super::{CellRef, NavCell, NavGrid, NavLadder, NavParams, NavPatch, build::Frame};

const MAGIC: &[u8; 8] = b"BF2NAVG2";

/// 4-byte words per patch: frame center and axis, origin (x, z each), cell size, width,
/// depth, first column and first cell.
const PATCH_WORDS: usize = 11;

fn patch_words(p: &NavPatch) -> [u32; PATCH_WORDS] {
    let f = |v: f32| v.to_bits();
    [
        f(p.frame.center.x),
        f(p.frame.center.y),
        f(p.frame.axis.x),
        f(p.frame.axis.y),
        f(p.origin.x),
        f(p.origin.y),
        f(p.cell),
        p.width,
        p.depth,
        p.column_base,
        p.first_cell,
    ]
}

fn patch_from_words(w: &[u32]) -> NavPatch {
    let f = |i: usize| f32::from_bits(w[i]);
    NavPatch {
        frame: Frame {
            center: Vec2::new(f(0), f(1)),
            axis: Vec2::new(f(2), f(3)),
        },
        origin: Vec2::new(f(4), f(5)),
        cell: f(6),
        width: w[7],
        depth: w[8],
        column_base: w[9],
        first_cell: w[10],
    }
}

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
    for v in [grid.width, grid.depth, grid.cells.len() as u32, grid.columns.len() as u32] {
        z.write_all(&v.to_le_bytes())?;
    }
    z.write_all(bytemuck::cast_slice(&grid.columns))?;
    z.write_all(bytemuck::cast_slice(&grid.cells))?;
    z.write_all(&(grid.ladders.len() as u32).to_le_bytes())?;
    for ladder in &grid.ladders {
        z.write_all(bytemuck::cast_slice(&ladder_words(ladder)))?;
    }
    z.write_all(&grid.base_cells.to_le_bytes())?;
    z.write_all(&(grid.patches.len() as u32).to_le_bytes())?;
    for patch in &grid.patches {
        z.write_all(bytemuck::cast_slice(&patch_words(patch)))?;
    }
    let portals: Vec<[u32; 4]> = grid
        .portals
        .iter()
        .flat_map(|(&from, to)| to.iter().map(move |c| [from, c.x, c.z, c.index]))
        .collect();
    z.write_all(&(portals.len() as u32).to_le_bytes())?;
    z.write_all(bytemuck::cast_slice(&portals))?;
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

/// Reads the decompressed data front to back.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> anyhow::Result<&[u8]> {
        ensure!(self.data.len() - self.at >= n, "truncated");
        self.at += n;
        Ok(&self.data[self.at - n..self.at])
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> anyhow::Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn words(&mut self, n: usize) -> anyhow::Result<Vec<u32>> {
        Ok(bytemuck::pod_collect_to_vec(self.bytes(n * 4)?))
    }
}

fn parse(compressed: &[u8], mut params: NavParams) -> anyhow::Result<NavGrid> {
    let mut data = Vec::new();
    DeflateDecoder::new(compressed).read_to_end(&mut data)?;
    let mut r = Reader { data: &data, at: 0 };
    params.cell = r.f32()?;
    let origin = Vec2::new(r.f32()?, r.f32()?);
    let (width, depth, count, column_count) = (r.u32()?, r.u32()?, r.u32()? as usize, r.u32()? as usize);
    let base_columns = width as usize * depth as usize + 1;
    ensure!(column_count >= base_columns, "inconsistent columns");
    let columns: Vec<u32> = r.words(column_count)?;
    let cells_len = count * size_of::<NavCell>();
    let cells: Vec<NavCell> = bytemuck::pod_collect_to_vec(r.bytes(cells_len)?);
    let ladder_count = r.u32()? as usize;
    let words = r.words(ladder_count * LADDER_WORDS)?;
    let ladders: Vec<NavLadder> = words.chunks_exact(LADDER_WORDS).map(ladder_from_words).collect();
    let mut ladder_ends: bevy::platform::collections::HashMap<u32, Vec<u16>> = default();
    for (i, ladder) in ladders.iter().enumerate() {
        ladder_ends.entry(ladder.bottom.index).or_default().push(i as u16);
        ladder_ends.entry(ladder.top.index).or_default().push(i as u16);
    }
    let base_cells = r.u32()?;
    let patch_count = r.u32()? as usize;
    let words = r.words(patch_count * PATCH_WORDS)?;
    let patches: Vec<NavPatch> = words.chunks_exact(PATCH_WORDS).map(patch_from_words).collect();
    ensure!(patches.windows(2).all(|w| w[0].first_cell <= w[1].first_cell), "inconsistent patches");
    // A cell's `x`/`z` are columns of its own space: the level grid's `width`/`depth`, or
    // (`NavPatch::width`/`depth`) of whichever patch its index falls in (patches occupy
    // contiguous, increasing ranges of cells from `first_cell`; see `NavGrid::patch_of`). A
    // ladder or portal can end on a patch (an aircraft carrier's ladders and its doors onto
    // the level grid), so checking every cell against just the level grid's bounds would
    // either wrongly reject those or, checked too loosely, let a corrupt cache through with an
    // `x`/`z` that doesn't actually match its `index`.
    let space = |index: u32| match patches.iter().rposition(|p| index >= p.first_cell) {
        Some(i) => (patches[i].width, patches[i].depth),
        None => (width, depth),
    };
    let in_grid = |c: &CellRef| {
        (c.index as usize) < count && {
            let (w, d) = space(c.index);
            c.x < w && c.z < d
        }
    };
    ensure!(ladders.iter().all(|l| in_grid(&l.bottom) && in_grid(&l.top)), "bad ladder");
    let portal_count = r.u32()? as usize;
    let words = r.words(portal_count * 4)?;
    ensure!(r.at == data.len(), "wrong size");
    let mut portals: bevy::platform::collections::HashMap<u32, Vec<CellRef>> = default();
    for w in words.chunks_exact(4) {
        let target = CellRef { x: w[1], z: w[2], index: w[3] };
        ensure!((w[0] as usize) < count && in_grid(&target), "bad portal");
        portals.entry(w[0]).or_default().push(target);
    }
    ensure!(columns[base_columns - 1] == base_cells, "inconsistent columns");
    ensure!(columns.last() == Some(&(count as u32)), "inconsistent columns");
    for p in &patches {
        let end = p.column_base as usize + p.width as usize * p.depth as usize;
        ensure!(end < columns.len() && columns[p.column_base as usize] == p.first_cell, "bad patch");
    }
    ensure!(columns.windows(2).all(|w| w[0] <= w[1]), "inconsistent columns");
    Ok(NavGrid {
        params,
        origin,
        width,
        depth,
        columns,
        cells,
        ladders,
        ladder_ends,
        base_cells,
        patches,
        portals,
    })
}
