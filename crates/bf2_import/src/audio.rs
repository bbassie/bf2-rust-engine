//! Sounds: BF2 ships voice-overs as Ogg Vorbis, which the game doesn't decode, so they are
//! converted to 16-bit PCM `.wav` here. `.wav` files are copied as they are.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bf2_formats::{Vfs, vfs::normalize};
use lewton::inside_ogg::OggStreamReader;

/// Copies (or converts) a sound from the game data; returns its path relative to `out`.
pub fn sound(vfs: &Vfs, reference: &str, out: &Path) -> Option<String> {
    let key = normalize(reference.trim_matches('"'));
    let target_rel = match key.strip_suffix(".ogg") {
        Some(stem) => format!("{stem}.wav"),
        None => key.clone(),
    };
    let target = out.join(&target_rel);
    if target.exists() {
        return Some(target_rel);
    }
    let data = vfs.read(&key).map_err(|e| log::debug!("sound {key}: {e}")).ok()?;
    let data = if key.ends_with(".ogg") {
        ogg_to_wav(&data).map_err(|e| log::warn!("sound {key}: {e:#}")).ok()?
    } else {
        data
    };
    std::fs::create_dir_all(target.parent()?).ok()?;
    std::fs::write(&target, data).ok()?;
    Some(target_rel)
}

/// Decodes Ogg Vorbis into a 16-bit PCM WAV file.
pub fn ogg_to_wav(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = OggStreamReader::new(std::io::Cursor::new(data)).context("reading ogg")?;
    let channels = reader.ident_hdr.audio_channels as u16;
    let rate = reader.ident_hdr.audio_sample_rate;
    if channels == 0 {
        bail!("no audio channels");
    }
    let mut samples: Vec<i16> = Vec::new();
    while let Some(packet) = reader.read_dec_packet_itl().context("decoding ogg")? {
        samples.extend(packet);
    }

    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}
