//! Vertical flipping of DDS textures without decoding them (DXT1/3/5 and uncompressed).
//!
//! BF2 terrain images have row 0 at the south edge; our format is north-up.

use anyhow::{Result, bail};

const HEADER: usize = 128;

#[derive(Clone, Copy)]
enum Format {
    Dxt1,
    Dxt3,
    Dxt5,
    Uncompressed { bytes_per_pixel: usize },
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
}

/// Returns a vertically flipped copy of a DDS file (all mip levels).
pub fn flip_vertical(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < HEADER || &data[..4] != b"DDS " {
        bail!("not a DDS file");
    }
    let height = u32_at(data, 12) as usize;
    let width = u32_at(data, 16) as usize;
    let mip_count = (u32_at(data, 28) as usize).max(1);
    let pf_flags = u32_at(data, 80);
    let four_cc = &data[84..88];
    let bit_count = u32_at(data, 88) as usize;
    let format = if pf_flags & 0x4 != 0 {
        match four_cc {
            b"DXT1" => Format::Dxt1,
            b"DXT3" => Format::Dxt3,
            b"DXT5" => Format::Dxt5,
            other => bail!("unsupported DDS fourCC {:?}", String::from_utf8_lossy(other)),
        }
    } else if bit_count % 8 == 0 && bit_count > 0 {
        Format::Uncompressed {
            bytes_per_pixel: bit_count / 8,
        }
    } else {
        bail!("unsupported DDS pixel format");
    };

    let mut out = data.to_vec();
    let mut offset = HEADER;
    let (mut w, mut h) = (width, height);
    for _ in 0..mip_count {
        let size = match format {
            Format::Uncompressed { bytes_per_pixel } => {
                let pitch = w * bytes_per_pixel;
                let size = pitch * h;
                if offset + size > data.len() {
                    break;
                }
                for row in 0..h {
                    let src = offset + row * pitch;
                    let dst = offset + (h - 1 - row) * pitch;
                    out[dst..dst + pitch].copy_from_slice(&data[src..src + pitch]);
                }
                size
            }
            _ => {
                let block_size = if matches!(format, Format::Dxt1) { 8 } else { 16 };
                let bw = w.div_ceil(4).max(1);
                let bh = h.div_ceil(4).max(1);
                let pitch = bw * block_size;
                let size = pitch * bh;
                if offset + size > data.len() {
                    break;
                }
                let rows = h.min(4);
                for by in 0..bh {
                    for bx in 0..bw {
                        let src = offset + by * pitch + bx * block_size;
                        let dst = offset + (bh - 1 - by) * pitch + bx * block_size;
                        let mut block = [0u8; 16];
                        block[..block_size].copy_from_slice(&data[src..src + block_size]);
                        flip_block(&mut block[..block_size], format, rows);
                        out[dst..dst + block_size].copy_from_slice(&block[..block_size]);
                    }
                }
                size
            }
        };
        offset += size;
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    Ok(out)
}

/// Converts an uncompressed 16-bit DDS into 32-bit BGRA: R5G6B5 (most BF2 terrain images),
/// and the A4R4G4B4 or A1R5G5B5 a few detail maps use (Operation Smoke Screen). Bevy's
/// loader and GPUs can't sample those. Returns `None` for any other format.
pub fn rgb565_to_bgra8(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < HEADER || &data[..4] != b"DDS " {
        return None;
    }
    let pf_flags = u32_at(data, 80);
    let bit_count = u32_at(data, 88);
    let (r_mask, g_mask, b_mask) = (u32_at(data, 92), u32_at(data, 96), u32_at(data, 100));
    // Alpha only counts with the "alpha pixels" flag.
    let a_mask = if pf_flags & 0x1 != 0 { u32_at(data, 104) } else { 0 };
    if pf_flags & 0x4 != 0 || bit_count != 16 || r_mask == 0 || g_mask == 0 || b_mask == 0 {
        return None;
    }
    // A channel's value scaled to 0..255 (255 without a mask).
    let channel = |v: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 255;
        }
        let max = mask >> mask.trailing_zeros();
        (((v & mask) >> mask.trailing_zeros()) * 255 / max) as u8
    };
    let mut out = data[..HEADER].to_vec();
    // Pixel format: RGB + alpha, 32 bits, A8R8G8B8 masks.
    out[80..84].copy_from_slice(&0x41u32.to_le_bytes());
    out[88..92].copy_from_slice(&32u32.to_le_bytes());
    out[92..96].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
    out[96..100].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
    out[100..104].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
    out[104..108].copy_from_slice(&0xFF00_0000u32.to_le_bytes());
    // Pitch flag and value describe the top level.
    let width = u32_at(data, 16);
    out[20..24].copy_from_slice(&(width * 4).to_le_bytes());
    for pixel in data[HEADER..].chunks_exact(2) {
        let v = u16::from_le_bytes([pixel[0], pixel[1]]) as u32;
        out.extend_from_slice(&[channel(v, b_mask), channel(v, g_mask), channel(v, r_mask), channel(v, a_mask)]);
    }
    Some(out)
}

/// Flips the first `rows` pixel rows of one compressed 4x4 block.
fn flip_block(block: &mut [u8], format: Format, rows: usize) {
    let color = match format {
        Format::Dxt1 => 0,
        Format::Dxt3 => {
            // 4 rows of 16-bit explicit alpha.
            let mut alpha = [[0u8; 2]; 4];
            for (r, a) in alpha.iter_mut().enumerate() {
                *a = [block[r * 2], block[r * 2 + 1]];
            }
            for r in 0..rows {
                let src = alpha[rows - 1 - r];
                block[r * 2] = src[0];
                block[r * 2 + 1] = src[1];
            }
            8
        }
        Format::Dxt5 => {
            // 2 endpoints, then 16 3-bit indices = 4 rows of 12 bits.
            let mut bits = 0u64;
            for i in 0..6 {
                bits |= (block[2 + i] as u64) << (8 * i);
            }
            let row = |r: usize| (bits >> (12 * r)) & 0xFFF;
            let mut flipped = bits;
            for r in 0..rows {
                flipped &= !(0xFFF << (12 * r));
                flipped |= row(rows - 1 - r) << (12 * r);
            }
            for i in 0..6 {
                block[2 + i] = (flipped >> (8 * i)) as u8;
            }
            8
        }
        Format::Uncompressed { .. } => return,
    };
    // Color block: 2 endpoints, then one byte of 2-bit indices per row.
    let indices = color + 4;
    let original: Vec<u8> = block[indices..indices + 4].to_vec();
    for r in 0..rows {
        block[indices + r] = original[rows - 1 - r];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dxt1_header(width: u32, height: u32) -> Vec<u8> {
        let mut h = vec![0u8; HEADER];
        h[..4].copy_from_slice(b"DDS ");
        h[4..8].copy_from_slice(&124u32.to_le_bytes());
        h[12..16].copy_from_slice(&height.to_le_bytes());
        h[16..20].copy_from_slice(&width.to_le_bytes());
        h[28..32].copy_from_slice(&1u32.to_le_bytes());
        h[80..84].copy_from_slice(&4u32.to_le_bytes());
        h[84..88].copy_from_slice(b"DXT1");
        h
    }

    #[test]
    fn flips_dxt1_blocks_and_rows() {
        let mut data = dxt1_header(4, 8);
        data.extend_from_slice(&[1, 1, 1, 1, 0xA0, 0xA1, 0xA2, 0xA3]); // top block
        data.extend_from_slice(&[2, 2, 2, 2, 0xB0, 0xB1, 0xB2, 0xB3]); // bottom block
        let flipped = flip_vertical(&data).unwrap();
        assert_eq!(&flipped[HEADER..HEADER + 8], &[2, 2, 2, 2, 0xB3, 0xB2, 0xB1, 0xB0]);
        assert_eq!(&flipped[HEADER + 8..], &[1, 1, 1, 1, 0xA3, 0xA2, 0xA1, 0xA0]);
        // Flipping twice is the identity.
        assert_eq!(flip_vertical(&flipped).unwrap(), data);
    }

    #[test]
    fn widens_16_bit_formats() {
        let header = |flags: u32, masks: [u32; 4]| {
            let mut h = dxt1_header(2, 1);
            h[80..84].copy_from_slice(&flags.to_le_bytes());
            h[84..88].copy_from_slice(&[0; 4]);
            h[88..92].copy_from_slice(&16u32.to_le_bytes());
            for (i, mask) in masks.iter().enumerate() {
                h[92 + 4 * i..96 + 4 * i].copy_from_slice(&mask.to_le_bytes());
            }
            h
        };
        // R5G6B5: pure red, then white.
        let mut rgb565 = header(0x40, [0xF800, 0x07E0, 0x001F, 0]);
        rgb565.extend_from_slice(&[0x00, 0xF8, 0xFF, 0xFF]);
        let out = rgb565_to_bgra8(&rgb565).unwrap();
        assert_eq!(&out[HEADER..], &[0, 0, 255, 255, 255, 255, 255, 255]);
        // A4R4G4B4: half-transparent green, then opaque blue.
        let mut argb4444 = header(0x41, [0x0F00, 0x00F0, 0x000F, 0xF000]);
        argb4444.extend_from_slice(&[0xF0, 0x80, 0x0F, 0xF0]);
        let out = rgb565_to_bgra8(&argb4444).unwrap();
        assert_eq!(&out[HEADER..], &[0, 255, 0, 136, 255, 0, 0, 255]);
        assert_eq!(u32_at(&out, 88), 32);
        // Already 32-bit: left alone.
        let mut bgra = header(0x41, [0xFF0000, 0xFF00, 0xFF, 0xFF000000]);
        bgra[88..92].copy_from_slice(&32u32.to_le_bytes());
        assert!(rgb565_to_bgra8(&bgra).is_none());
    }
}
