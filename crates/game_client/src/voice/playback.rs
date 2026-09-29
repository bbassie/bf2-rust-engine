//! Playing the voices we hear: one output stream of our own (cpal) that mixes every talker.
//!
//! Voice doesn't go through Bevy's audio: it is a live stream per talker, not a sound asset,
//! it needs the lowest latency the device gives, and it may go to a different device than
//! the game (a headset while the game plays on speakers). The stream's callback pulls a
//! 20 ms frame from each talker's jitter buffer (`game_voice::jitter`) whenever it needs
//! one, decodes it (or recovers or conceals a lost one), mixes, applies the volume (master
//! times voice) and resamples to the device's rate. Talkers muted in the scoreboard never
//! get here (see `voice::receive_voice`).

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use bevy::prelude::*;
use cpal::{
    FromSample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use game_voice::{
    FRAME_SAMPLES, SAMPLE_RATE,
    codec::Decoder,
    jitter::{JitterBuffer, JitterStats, Playout},
    resample::Resampler,
};

use super::capture::device_name;

/// Frames a talker stays in the mixer after it stopped talking (its decoder keeps its
/// state for the next burst): 10 s.
const FORGET_AFTER_FRAMES: u32 = 500;

struct Talker {
    jitter: JitterBuffer,
    decoder: Decoder,
    idle_frames: u32,
}

/// Every talker's buffer and decoder, shared with the output's audio thread.
pub struct Mixer {
    talkers: HashMap<u64, Talker>,
    /// Master volume times voice volume.
    gain: f32,
    resampler: Resampler,
    /// Mixed samples at the device's rate, not yet played.
    ready: VecDeque<f32>,
    block: Vec<f32>,
    frame: Vec<f32>,
    resampled: Vec<f32>,
}

impl Mixer {
    fn new(device_rate: u32) -> Self {
        Self {
            talkers: HashMap::new(),
            gain: 1.0,
            resampler: Resampler::new(SAMPLE_RATE, device_rate),
            ready: VecDeque::new(),
            block: vec![0.0; FRAME_SAMPLES],
            frame: vec![0.0; FRAME_SAMPLES],
            resampled: Vec::new(),
        }
    }

    /// A frame from `talker` arrived.
    pub fn push(&mut self, talker: u64, seq: u16, packet: Vec<u8>) {
        if !self.talkers.contains_key(&talker) {
            match Decoder::new() {
                Ok(decoder) => {
                    self.talkers.insert(talker, Talker { jitter: JitterBuffer::default(), decoder, idle_frames: 0 });
                }
                Err(err) => {
                    warn!("voice: {err}");
                    return;
                }
            }
        }
        if let Some(talker) = self.talkers.get_mut(&talker) {
            talker.idle_frames = 0;
            talker.jitter.push(seq, packet);
        }
    }

    /// Drops what `talker` has buffered (muted meanwhile).
    pub fn forget(&mut self, talker: u64) {
        self.talkers.remove(&talker);
    }

    /// What `talker`'s jitter buffer did so far.
    pub fn stats(&self, talker: u64) -> Option<JitterStats> {
        self.talkers.get(&talker).map(|t| t.jitter.stats)
    }

    /// Mixes the next 20 ms of every talker into `block`.
    fn mix_frame(&mut self) {
        self.block.fill(0.0);
        for talker in self.talkers.values_mut() {
            let result = match talker.jitter.next() {
                Playout::Packet(packet) => talker.decoder.decode(&packet, &mut self.frame),
                Playout::Recover(next) => talker.decoder.recover(&next, &mut self.frame),
                Playout::Conceal => talker.decoder.conceal(&mut self.frame),
                Playout::Silence => {
                    talker.idle_frames += 1;
                    continue;
                }
            };
            if result.is_ok() {
                for (out, sample) in self.block.iter_mut().zip(&self.frame) {
                    *out += sample;
                }
            }
        }
        self.talkers.retain(|_, t| t.idle_frames < FORGET_AFTER_FRAMES || t.jitter.is_active());
        let gain = self.gain;
        for sample in &mut self.block {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
        self.resampled.clear();
        self.resampler.process(&self.block, &mut self.resampled);
        self.ready.extend(self.resampled.iter().copied());
    }

    /// Fills `out` (interleaved, `channels` per frame) with the mix.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        let frames = out.len() / channels.max(1);
        while self.ready.len() < frames {
            self.mix_frame();
        }
        for frame in out.chunks_mut(channels.max(1)) {
            let sample = T::from_sample(self.ready.pop_front().unwrap_or(0.0));
            frame.fill(sample);
        }
    }
}

/// Our voice output stream.
pub struct Output {
    _stream: cpal::Stream,
    pub mixer: Arc<Mutex<Mixer>>,
    /// The device, for the logs.
    pub description: String,
}

impl Output {
    /// Opens the output named `device` (or the system's default).
    pub fn open(device: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match device {
            Some(name) => host
                .output_devices()
                .map_err(|err| err.to_string())?
                .find(|d| device_name(d).as_deref() == Some(name))
                .ok_or_else(|| format!("no audio output named {name}"))?,
            None => host.default_output_device().ok_or("no audio output")?,
        };
        let name = device_name(&device).unwrap_or_else(|| "speakers".into());
        let config = device.default_output_config().map_err(|err| format!("{name}: {err}"))?;
        let rate = config.sample_rate();
        let channels = usize::from(config.channels()).max(1);
        let mixer = Arc::new(Mutex::new(Mixer::new(rate)));
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::F64 => build::<f64>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::U16 => build::<u16>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::U32 => build::<u32>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::I8 => build::<i8>(&device, &config.config(), channels, mixer.clone()),
            SampleFormat::U8 => build::<u8>(&device, &config.config(), channels, mixer.clone()),
            other => return Err(format!("{name}: unsupported sample format {other}")),
        }
        .map_err(|err| format!("{name}: {err}"))?;
        stream.play().map_err(|err| format!("{name}: {err}"))?;
        Ok(Self { _stream: stream, mixer, description: format!("{name}, {rate} Hz") })
    }

    pub fn set_gain(&self, gain: f32) {
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.gain = gain;
        }
    }

    pub fn push(&self, talker: u64, seq: u16, packet: Vec<u8>) {
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.push(talker, seq, packet);
        }
    }

    pub fn forget(&self, talker: u64) {
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.forget(talker);
        }
    }

    pub fn stats(&self, talker: u64) -> Option<JitterStats> {
        self.mixer.lock().ok()?.stats(talker)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<Mixer>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| match mixer.lock() {
            Ok(mut mixer) => mixer.fill(out, channels),
            Err(_) => out.fill(T::from_sample(0.0f32)),
        },
        |err| warn!("voice: audio output: {err}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use game_voice::{BITRATE, codec::Encoder};

    use super::*;

    #[test]
    fn mixes_talkers_through_the_jitter_buffer() {
        let mut mixer = Mixer::new(44_100);
        let mut encoder = Encoder::new(BITRATE).unwrap();
        let tone: Vec<f32> = (0..FRAME_SAMPLES * 20)
            .map(|i| (i as f32 / SAMPLE_RATE as f32 * 300.0 * std::f32::consts::TAU).sin() * 0.3)
            .collect();
        for (seq, frame) in tone.chunks(FRAME_SAMPLES).enumerate() {
            mixer.push(7, seq as u16, encoder.encode(frame).unwrap());
        }
        let mut out = vec![0.0f32; 44_100 / 2 * 2];
        mixer.fill(&mut out, 2);
        let loud = out.iter().filter(|s| s.abs() > 0.05).count();
        assert!(loud > 5000, "only {loud} loud samples");
        // Stereo: both channels the same.
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
    }
}
