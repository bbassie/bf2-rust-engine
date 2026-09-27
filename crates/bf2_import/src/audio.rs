//! Sound files: BF2 ships voice-overs and ambience as Ogg Vorbis, which the game doesn't
//! decode, so they are converted to 16-bit PCM `.wav` here. `.wav` files are copied as they
//! are (BF2's are all 16-bit PCM).

use std::{
    path::Path,
    sync::atomic::{AtomicU32, Ordering},
};

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
    // Parallel imports may convert the same file: never leave a half-written one.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let temp = target.with_extension(format!("part{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&temp, data).ok()?;
    if std::fs::rename(&temp, &target).is_err() {
        std::fs::remove_file(&temp).ok();
    }
    target.exists().then_some(target_rel)
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
    Ok(pcm16_wav(channels, rate, &samples))
}

/// A 16-bit PCM WAV file of interleaved `samples`.
pub fn pcm16_wav(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
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
    wav
}

/// Channels, sample rate and interleaved samples of a 16-bit PCM WAV file.
pub fn read_pcm16_wav(data: &[u8]) -> Option<(u16, u32, Vec<i16>)> {
    if data.get(0..4)? != b"RIFF" || data.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut format, mut samples) = (None, None);
    let mut at = 12;
    while at + 8 <= data.len() {
        let size = u32::from_le_bytes(data[at + 4..at + 8].try_into().ok()?) as usize;
        let body = &data[at + 8..(at + 8 + size).min(data.len())];
        match &data[at..at + 4] {
            b"fmt " if body.len() >= 16 => {
                let word = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                format = Some((word(0), word(2), rate, word(14)));
            }
            b"data" => {
                samples = Some(body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>());
            }
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    match (format?, samples?) {
        ((1, channels, rate, 16), samples) if channels > 0 => Some((channels, rate, samples)),
        _ => None,
    }
}
