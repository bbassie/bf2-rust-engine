//! Opus encoding and decoding of 20 ms frames of 48 kHz mono.
//!
//! The codec is libopus 1.3.1 itself, translated to Rust by c2rust (`unsafe-libopus`): the
//! reference encoder and decoder, checked against the IETF test vectors bit for bit, without
//! a C compiler or CMake (the `audiopus`/`opus` crates need a prebuilt or CMake-built libopus,
//! which Windows machines usually lack). Its API is libopus's raw C API, so this module wraps
//! it: every `unsafe` call is here, with the pointer invariants in one place.
//!
//! Settings: VoIP application, voice signal, [`crate::BITRATE`] VBR, in-band FEC for 10 %
//! expected loss (each packet carries a coarse copy of the previous frame, see
//! [`Decoder::recover`]), complexity 8.

use std::ptr::NonNull;

use unsafe_libopus::{
    OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
    OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE,
    OpusDecoder, OpusEncoder, opus_decode_float, opus_decoder_create, opus_decoder_destroy, opus_encode_float,
    opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy,
};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

/// Largest packet the encoder writes (and the network accepts): 20 ms at 24 kbps is about
/// 60 bytes, VBR peaks stay far below this.
pub const MAX_PACKET_BYTES: usize = 256;

/// What went wrong in the codec (a libopus error code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecError(pub i32);

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.0 {
            -1 => "bad argument",
            -2 => "buffer too small",
            -3 => "internal error",
            -4 => "invalid packet",
            -5 => "unimplemented",
            -6 => "invalid state",
            -7 => "allocation failed",
            _ => "unknown error",
        };
        write!(f, "opus: {reason} ({})", self.0)
    }
}

impl std::error::Error for CodecError {}

fn check(code: i32) -> Result<i32, CodecError> {
    if code < 0 { Err(CodecError(code)) } else { Ok(code) }
}

/// An Opus encoder for one microphone.
pub struct Encoder {
    state: NonNull<OpusEncoder>,
}

// The state is plain memory owned by this value; libopus keeps no thread-local state.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: i32) -> Result<Self, CodecError> {
        let mut error = 0;
        // SAFETY: valid arguments; the result is checked for null and the error code.
        let state = unsafe { opus_encoder_create(SAMPLE_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut error) };
        let state = NonNull::new(state).ok_or(CodecError(error))?;
        let encoder = Self { state };
        if error != OPUS_OK {
            return Err(CodecError(error));
        }
        let ptr = encoder.state.as_ptr();
        // SAFETY: `ptr` is a live encoder; each request takes one i32.
        unsafe {
            check(opus_encoder_ctl!(ptr, OPUS_SET_BITRATE_REQUEST, bitrate))?;
            check(opus_encoder_ctl!(ptr, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE))?;
            check(opus_encoder_ctl!(ptr, OPUS_SET_INBAND_FEC_REQUEST, 1))?;
            check(opus_encoder_ctl!(ptr, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10))?;
            check(opus_encoder_ctl!(ptr, OPUS_SET_COMPLEXITY_REQUEST, 8))?;
        }
        Ok(encoder)
    }

    /// Encodes one frame of [`FRAME_SAMPLES`] samples (-1..1) into a packet.
    pub fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, CodecError> {
        assert_eq!(frame.len(), FRAME_SAMPLES, "one 20 ms frame at a time");
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        // SAFETY: `frame` holds FRAME_SAMPLES mono samples, `packet` MAX_PACKET_BYTES bytes.
        let written = check(unsafe {
            opus_encode_float(
                self.state.as_ptr(),
                frame.as_ptr(),
                FRAME_SAMPLES as i32,
                packet.as_mut_ptr(),
                MAX_PACKET_BYTES as i32,
            )
        })?;
        packet.truncate(written as usize);
        Ok(packet)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_encoder_create`, destroyed once.
        unsafe { opus_encoder_destroy(self.state.as_ptr()) }
    }
}

/// An Opus decoder for one talker.
pub struct Decoder {
    state: NonNull<OpusDecoder>,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, CodecError> {
        let mut error = 0;
        // SAFETY: valid arguments; the result is checked.
        let state = unsafe { opus_decoder_create(SAMPLE_RATE as i32, 1, &mut error) };
        let state = NonNull::new(state).ok_or(CodecError(error))?;
        let decoder = Self { state };
        if error != OPUS_OK {
            return Err(CodecError(error));
        }
        Ok(decoder)
    }

    /// Decodes a packet into `out` ([`FRAME_SAMPLES`] samples). A packet that isn't valid
    /// Opus is an error and leaves `out` silent.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<(), CodecError> {
        self.run(Some(packet), false, out)
    }

    /// Recovers the frame before `next` from the coarse copy `next` carries (in-band FEC),
    /// for a lost packet whose successor already arrived. Without FEC data in `next` this
    /// conceals the frame instead.
    pub fn recover(&mut self, next: &[u8], out: &mut [f32]) -> Result<(), CodecError> {
        self.run(Some(next), true, out)
    }

    /// Conceals a lost frame (packet loss concealment): continues the voice for a moment,
    /// fading out.
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<(), CodecError> {
        self.run(None, false, out)
    }

    fn run(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<(), CodecError> {
        assert_eq!(out.len(), FRAME_SAMPLES, "one 20 ms frame at a time");
        if packet.is_some_and(|p| p.is_empty() || p.len() > MAX_PACKET_BYTES) {
            out.fill(0.0);
            return Err(CodecError(-4));
        }
        let (data, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        // SAFETY: `data` is null (concealment) or `len` readable bytes; `out` holds
        // FRAME_SAMPLES samples, the most one mono frame of 20 ms decodes to at 48 kHz.
        let decoded = unsafe {
            opus_decode_float(self.state.as_ptr(), data, len, out.as_mut_ptr(), FRAME_SAMPLES as i32, fec as i32)
        };
        match check(decoded) {
            Ok(samples) => {
                // A packet of shorter frames (10 ms from another encoder) leaves a tail.
                out[samples as usize..].fill(0.0);
                Ok(())
            }
            Err(err) => {
                out.fill(0.0);
                Err(err)
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create`, destroyed once.
        unsafe { opus_decoder_destroy(self.state.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BITRATE, FRAMES_PER_SECOND};

    /// A voice-like test signal: a 220 Hz tone with harmonics, slowly swelling.
    fn signal(frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let envelope = 0.6 + 0.4 * (t * 3.0).sin();
                let f = 220.0 * std::f32::consts::TAU * t;
                envelope * (0.35 * f.sin() + 0.15 * (2.0 * f).sin() + 0.08 * (3.0 * f).sin())
            })
            .collect()
    }

    /// The lag of `b` behind `a` in samples where they correlate best, and how well.
    fn best_lag(a: &[f32], b: &[f32], max_lag: usize) -> (usize, f32) {
        let mut best = (0, f32::MIN);
        for lag in 0..max_lag {
            let n = a.len() - max_lag;
            let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
            for i in 0..n {
                ab += a[i] * b[i + lag];
                aa += a[i] * a[i];
                bb += b[i + lag] * b[i + lag];
            }
            let correlation = ab / (aa.sqrt() * bb.sqrt()).max(1e-9);
            if correlation > best.1 {
                best = (lag, correlation);
            }
        }
        best
    }

    #[test]
    fn round_trip_keeps_the_voice() {
        let mut encoder = Encoder::new(BITRATE).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let input = signal(50);
        let mut output = Vec::new();
        let mut bytes = 0;
        for frame in input.chunks(FRAME_SAMPLES) {
            let packet = encoder.encode(frame).unwrap();
            assert!(!packet.is_empty() && packet.len() <= MAX_PACKET_BYTES);
            bytes += packet.len();
            let mut out = vec![0.0; FRAME_SAMPLES];
            decoder.decode(&packet, &mut out).unwrap();
            output.extend(out);
        }
        // About the bitrate asked for (VBR: within a factor of two).
        let kbps = bytes as f32 * 8.0 / (50.0 / FRAMES_PER_SECOND as f32) / 1000.0;
        assert!((12.0..=48.0).contains(&kbps), "{kbps} kbps");
        // Skip the first frames while the encoder settles, then the output must follow the
        // input closely once aligned by the codec's delay.
        let skip = 10 * FRAME_SAMPLES;
        let (lag, correlation) = best_lag(&input[skip..], &output[skip..], 800);
        // (The signal is periodic, so the lag found is the codec delay modulo the period.)
        assert!(correlation > 0.9, "correlation {correlation} at lag {lag}");
        let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32;
        let ratio = energy(&output[skip..]) / energy(&input[skip..]);
        assert!((0.6..1.4).contains(&ratio), "energy ratio {ratio}");
    }

    #[test]
    fn lost_frames_are_recovered_or_concealed() {
        let mut encoder = Encoder::new(BITRATE).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets: Vec<Vec<u8>> = signal(30).chunks(FRAME_SAMPLES).map(|f| encoder.encode(f).unwrap()).collect();
        let mut out = vec![0.0; FRAME_SAMPLES];
        let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32;
        for packet in &packets[..20] {
            decoder.decode(packet, &mut out).unwrap();
        }
        // Packet 20 lost, 21 arrived: its FEC brings 20 back, with the voice still there.
        decoder.recover(&packets[21], &mut out).unwrap();
        assert!(energy(&out) > 0.005, "recovered frame is silent: {}", energy(&out));
        decoder.decode(&packets[21], &mut out).unwrap();
        // 22 and 23 lost with nothing after: concealed, fading rather than cutting off.
        decoder.conceal(&mut out).unwrap();
        assert!(energy(&out) > 0.0005, "concealed frame is silent");
        decoder.conceal(&mut out).unwrap();
        decoder.decode(&packets[24], &mut out).unwrap();
    }

    #[test]
    fn garbage_is_refused_not_crashed_on() {
        let mut decoder = Decoder::new().unwrap();
        let mut out = vec![1.0; FRAME_SAMPLES];
        assert!(decoder.decode(&[], &mut out).is_err());
        assert!(decoder.decode(&[0u8; MAX_PACKET_BYTES + 1], &mut out).is_err());
        // Random bytes: either an error or some decoded noise, never a crash.
        let mut seed = 0x1234_5678u32;
        for _ in 0..200 {
            let len = 1 + (seed % 120) as usize;
            let packet: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let _ = decoder.decode(&packet, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }
}
