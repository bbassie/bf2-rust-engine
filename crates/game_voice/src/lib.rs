//! Voice chat's audio side, without an engine: everything between a microphone's samples and
//! a speaker's, except the devices and the network (the client's `voice` module has those).
//!
//! ```text
//!  mic (any rate, any channels)                                   speaker (any rate)
//!    │ downmix                                                            ▲
//!    ▼                                                                    │ Resampler
//!  Resampler ─▶ 48 kHz mono ─▶ gain, VoiceGate ─▶ Encoder ─▶ packets ─▶ JitterBuffer ─▶ Decoder
//!                               (level meter)      20 ms, 24 kbps        (reorder, FEC, PLC)
//! ```
//!
//! - [`codec`]: Opus (libopus 1.3.1 translated to Rust by c2rust, `unsafe-libopus`) in 20 ms
//!   frames of 48 kHz mono, VoIP mode, with in-band forward error correction.
//! - [`jitter`]: one buffer per talker: orders packets, waits a few frames before playing,
//!   recovers a lost frame from the next packet's FEC or conceals it, and notices when a
//!   talker stops.
//! - [`resample`]: windowed-sinc resampling between the devices' rates and 48 kHz.
//! - [`level`]: RMS level in dBFS and the voice activation gate.
//! - [`test_input`]: a tone or a WAV file in place of a microphone, for tests without one.

pub mod codec;
pub mod jitter;
pub mod level;
pub mod resample;
pub mod test_input;

/// The codec's sample rate: everything between the resamplers runs at this rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Length of one frame (one packet) in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Samples in one frame at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
/// Frames per second.
pub const FRAMES_PER_SECOND: u32 = 1000 / FRAME_MS;
/// Encoder bitrate in bits per second: clear speech, about 60 bytes a frame.
pub const BITRATE: i32 = 24_000;
