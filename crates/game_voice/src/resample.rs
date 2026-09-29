//! Streaming sample rate conversion between the devices' rates (44.1, 48, 96 kHz, ...) and
//! the codec's 48 kHz: windowed-sinc interpolation, low-passed at the lower of the two
//! Nyquist frequencies so downsampling doesn't alias. 16 taps on each side, computed per
//! output sample: a few million multiply-adds a second, nothing for voice.

use std::f64::consts::PI;

/// Taps on each side of the interpolated point.
const HALF: usize = 16;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Low-pass cutoff relative to the input's Nyquist frequency (1 when upsampling).
    cutoff: f64,
    /// Input not yet consumed, starting `HALF` samples before `position`'s window.
    history: Vec<f32>,
    /// Where the next output sample lies in `history`.
    position: f64,
    passthrough: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let from = from.max(1);
        let to = to.max(1);
        Self {
            step: f64::from(from) / f64::from(to),
            cutoff: (f64::from(to) / f64::from(from)).min(1.0) * 0.97,
            // Leading silence, so the first output sample has a full window.
            history: vec![0.0; HALF],
            position: HALF as f64,
            passthrough: from == to,
        }
    }

    /// Converts `input`, appending to `out`. Keeps what it needs of `input` for next time.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.history.extend_from_slice(input);
        while self.position + (HALF as f64) < self.history.len() as f64 {
            out.push(self.sample_at(self.position));
            self.position += self.step;
        }
        // Drop what no future window reaches.
        let keep_from = (self.position.floor() as usize).saturating_sub(HALF);
        if keep_from > 0 {
            self.history.drain(..keep_from);
            self.position -= keep_from as f64;
        }
    }

    fn sample_at(&self, position: f64) -> f32 {
        let center = position.floor() as isize;
        let frac = position - center as f64;
        let mut sum = 0.0f64;
        let mut weights = 0.0f64;
        for k in (1 - HALF as isize)..=(HALF as isize) {
            let index = center + k;
            if index < 0 || index as usize >= self.history.len() {
                continue;
            }
            let x = k as f64 - frac;
            let weight = kernel(x, self.cutoff);
            sum += weight * f64::from(self.history[index as usize]);
            weights += weight;
        }
        // Normalized, so a constant stays constant whatever the fraction.
        if weights.abs() > 1e-9 { (sum / weights) as f32 } else { 0.0 }
    }
}

/// Low-pass sinc at `cutoff`, Blackman-windowed over `HALF` taps.
fn kernel(x: f64, cutoff: f64) -> f64 {
    let sinc = if x.abs() < 1e-9 { 1.0 } else { (PI * cutoff * x).sin() / (PI * cutoff * x) };
    let t = (x / HALF as f64).clamp(-1.0, 1.0);
    let window = 0.42 + 0.5 * (PI * t).cos() + 0.08 * (2.0 * PI * t).cos();
    cutoff * sinc * window
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, freq: f32, seconds: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| (i as f32 / rate as f32 * freq * std::f32::consts::TAU).sin() * 0.5)
            .collect()
    }

    /// Frequency by zero crossings.
    fn frequency(samples: &[f32], rate: u32) -> f32 {
        let crossings = samples.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f32 / (samples.len() as f32 / rate as f32)
    }

    fn convert(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = Resampler::new(from, to);
        let mut out = Vec::new();
        // In odd-sized pieces, like device callbacks.
        for chunk in input.chunks(441) {
            resampler.process(chunk, &mut out);
        }
        out
    }

    #[test]
    fn keeps_length_ratio_and_pitch() {
        for (from, to) in [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000), (16_000, 48_000), (48_000, 48_000)] {
            let input = tone(from, 440.0, 1.0);
            let out = convert(from, to, &input);
            // Short by the filter's latency (HALF input samples) at most.
            let expected = input.len() as f32 * to as f32 / from as f32;
            let latency = HALF as f32 * to as f32 / from as f32 + 2.0;
            assert!((out.len() as f32 - expected).abs() <= latency, "{from}->{to}: {} vs {expected}", out.len());
            let f = frequency(&out[200..], to);
            assert!((f - 440.0).abs() < 5.0, "{from}->{to}: {f} Hz");
            let peak = out[200..out.len() - 200].iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!((0.45..0.55).contains(&peak), "{from}->{to}: peak {peak}");
        }
    }

    #[test]
    fn downsampling_filters_what_the_new_rate_cant_hold() {
        // 30 kHz can't exist at 48 kHz: it must be filtered out, not folded down to 18 kHz.
        let input = tone(96_000, 30_000.0, 0.5);
        let out = convert(96_000, 48_000, &input);
        let rms = (out[100..].iter().map(|s| s * s).sum::<f32>() / out[100..].len() as f32).sqrt();
        assert!(rms < 0.02, "aliased energy {rms}");
    }
}
