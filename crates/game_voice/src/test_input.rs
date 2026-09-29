//! A stand-in for a microphone, for tests on machines without one (and for scenarios, which
//! can't speak): a tone, or a WAV file played in a loop.
//!
//! The client takes it from `BF2_VOICE_TEST_INPUT`: `tone`, `tone:<Hz>` or a path to a WAV
//! file (PCM 8/16/24/32 bit or 32-bit float, any rate, any channels; mixed down to mono).

use std::path::Path;

use crate::SAMPLE_RATE;

/// Where test samples come from.
#[derive(Clone, Debug)]
pub enum TestInput {
    /// A sine at this frequency, at -12 dBFS.
    Tone(f32),
    /// Mono samples at a rate, looped.
    Samples { rate: u32, samples: Vec<f32> },
}

impl TestInput {
    /// `tone`, `tone:<Hz>` or a WAV file's path.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("tone") {
            return Ok(TestInput::Tone(440.0));
        }
        if let Some(freq) = spec.strip_prefix("tone:") {
            let freq: f32 = freq.parse().map_err(|_| format!("bad tone frequency `{freq}`"))?;
            if !(20.0..=20_000.0).contains(&freq) {
                return Err(format!("tone frequency {freq} Hz out of range"));
            }
            return Ok(TestInput::Tone(freq));
        }
        let (rate, samples) = read_wav(Path::new(spec))?;
        if samples.is_empty() {
            return Err(format!("{spec}: no samples"));
        }
        Ok(TestInput::Samples { rate, samples })
    }

    /// The rate [`TestSource::take`] produces samples at.
    pub fn rate(&self) -> u32 {
        match self {
            TestInput::Tone(_) => SAMPLE_RATE,
            TestInput::Samples { rate, .. } => *rate,
        }
    }
}

/// Produces a [`TestInput`]'s samples as a microphone would: as many as the elapsed time
/// calls for.
#[derive(Clone, Debug)]
pub struct TestSource {
    input: TestInput,
    /// Samples produced so far.
    produced: u64,
}

impl TestSource {
    pub fn new(input: TestInput) -> Self {
        Self { input, produced: 0 }
    }

    pub fn rate(&self) -> u32 {
        self.input.rate()
    }

    /// The samples due by `elapsed` seconds since the source started.
    pub fn take(&mut self, elapsed: f64, out: &mut Vec<f32>) {
        let due = (elapsed.max(0.0) * f64::from(self.rate())) as u64;
        // Never more than a second at once (after a stall).
        let from = self.produced.max(due.saturating_sub(u64::from(self.rate())));
        for i in from..due {
            out.push(match &self.input {
                TestInput::Tone(freq) => {
                    let t = i as f64 / f64::from(SAMPLE_RATE);
                    ((t * f64::from(*freq) * std::f64::consts::TAU).sin() * 0.25) as f32
                }
                TestInput::Samples { samples, .. } => samples[(i % samples.len() as u64) as usize],
            });
        }
        self.produced = due.max(self.produced);
    }
}

/// Reads a WAV file: its rate and its samples mixed down to mono, -1..1.
pub fn read_wav(path: &Path) -> Result<(u32, Vec<f32>), String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    parse_wav(&bytes).map_err(|err| format!("{}: {err}", path.display()))
}

pub fn parse_wav(bytes: &[u8]) -> Result<(u32, Vec<f32>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut format = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = body.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " if end - body >= 16 => {
                let mut tag = u16_at(body);
                let channels = u16_at(body + 2).max(1) as usize;
                let rate = u32_at(body + 4);
                let bits = u16_at(body + 14);
                // WAVE_FORMAT_EXTENSIBLE: the real format is in the sub-format GUID.
                if tag == 0xFFFE && end - body >= 26 {
                    tag = u16_at(body + 24);
                }
                format = Some((tag, channels, rate, bits));
            }
            b"data" => {
                let Some((tag, channels, rate, bits)) = format else {
                    return Err("data before fmt".into());
                };
                if rate == 0 {
                    return Err("sample rate 0".into());
                }
                let data = &bytes[body..end];
                let sample: Box<dyn Fn(&[u8]) -> f32> = match (tag, bits) {
                    (1, 8) => Box::new(|b| (f32::from(b[0]) - 128.0) / 128.0),
                    (1, 16) => Box::new(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0),
                    (1, 24) => Box::new(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0),
                    (1, 32) => Box::new(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0),
                    (3, 32) => Box::new(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    _ => return Err(format!("unsupported format {tag} with {bits} bits")),
                };
                let width = usize::from(bits / 8);
                let frame = width * channels;
                let samples = data
                    .chunks_exact(frame)
                    .map(|f| f.chunks_exact(width).map(&sample).sum::<f32>() / channels as f32)
                    .map(|s| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 })
                    .collect();
                return Ok((rate, samples));
            }
            _ => {}
        }
        // Chunks are padded to even sizes.
        at = body.saturating_add(size + (size & 1));
    }
    Err("no data chunk".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * 2).to_le_bytes());
        out.extend((channels * 2).to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn reads_stereo_pcm16_as_mono() {
        let bytes = wav(22_050, 2, &[16384, 0, -16384, -16384]);
        let (rate, samples) = parse_wav(&bytes).unwrap();
        assert_eq!(rate, 22_050);
        assert_eq!(samples, vec![0.25, -0.5]);
    }

    #[test]
    fn refuses_what_isnt_a_wav() {
        assert!(parse_wav(b"hello world, not a wav").is_err());
        assert!(TestInput::parse("tone:5").is_err());
        assert!(matches!(TestInput::parse("tone:300"), Ok(TestInput::Tone(f)) if f == 300.0));
    }

    #[test]
    fn a_source_produces_samples_in_real_time() {
        let mut source = TestSource::new(TestInput::Tone(440.0));
        let mut out = Vec::new();
        source.take(0.5, &mut out);
        assert_eq!(out.len(), 24_000);
        source.take(0.5, &mut out);
        assert_eq!(out.len(), 24_000, "nothing new without time passing");
        source.take(0.52, &mut out);
        assert_eq!(out.len(), 24_960);
        let mut looped = TestSource::new(TestInput::Samples { rate: 8000, samples: vec![0.1, 0.2] });
        let mut out = Vec::new();
        looped.take(0.001, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.1, 0.2, 0.1, 0.2, 0.1, 0.2]);
    }
}
