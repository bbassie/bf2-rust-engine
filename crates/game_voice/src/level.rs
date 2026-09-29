//! Microphone level and voice activation.

/// Level of silence, in dBFS.
pub const SILENCE_DB: f32 = -96.0;

/// RMS level of `samples` in dBFS (0 = a full-scale square wave).
pub fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return SILENCE_DB;
    }
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if mean <= 1e-10 { SILENCE_DB } else { (10.0 * mean.log10()).max(SILENCE_DB) }
}

/// Multiplies by `gain` and clips softly, so a high input gain distorts gently instead of
/// wrapping.
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    for sample in samples {
        let x = *sample * gain;
        *sample = if x.abs() <= 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((x.abs() - 0.8) / 0.2).tanh()) };
    }
}

/// Voice activation: opens when a frame is louder than the threshold, stays open for a
/// moment after the voice drops (so words and short pauses aren't cut), closes after that.
#[derive(Clone, Debug)]
pub struct VoiceGate {
    /// Opening level in dBFS.
    pub threshold_db: f32,
    /// Frames it stays open after the last loud one.
    pub hold_frames: u32,
    open_for: u32,
}

impl VoiceGate {
    /// 400 ms of hold, at 20 ms frames.
    pub const HOLD_FRAMES: u32 = 20;

    pub fn new(threshold_db: f32) -> Self {
        Self { threshold_db, hold_frames: Self::HOLD_FRAMES, open_for: 0 }
    }

    /// Takes a frame's level; whether the gate is open for it.
    pub fn update(&mut self, level_db: f32) -> bool {
        if level_db >= self.threshold_db {
            self.open_for = self.hold_frames;
            true
        } else if self.open_for > 0 {
            self.open_for -= 1;
            true
        } else {
            false
        }
    }

    pub fn is_open(&self) -> bool {
        self.open_for > 0
    }

    pub fn close(&mut self) {
        self.open_for = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(rms_db(&[]), SILENCE_DB);
        assert_eq!(rms_db(&[0.0; 100]), SILENCE_DB);
        assert!((rms_db(&[1.0, -1.0]) - 0.0).abs() < 0.01);
        assert!((rms_db(&[0.1, -0.1]) + 20.0).abs() < 0.01);
    }

    #[test]
    fn gain_clips_softly() {
        let mut samples = [0.1, 0.5, -2.0];
        apply_gain(&mut samples, 2.0);
        assert!((samples[0] - 0.2).abs() < 1e-6);
        assert!(samples[1] > 0.8 && samples[1] < 1.0);
        assert!(samples[2] < -0.8 && samples[2] >= -1.0);
    }

    #[test]
    fn the_gate_holds_through_short_pauses() {
        let mut gate = VoiceGate::new(-40.0);
        assert!(!gate.update(-60.0), "quiet: closed");
        assert!(gate.update(-20.0), "speech opens it");
        for _ in 0..VoiceGate::HOLD_FRAMES {
            assert!(gate.update(-60.0), "held open through a pause");
        }
        assert!(!gate.update(-60.0), "then closes");
    }
}
