//! Where our voice comes from: a microphone (cpal) or, for tests, a tone or WAV file
//! (`BF2_VOICE_TEST_INPUT`, see `game_voice::test_input`). Either way the result is 48 kHz
//! mono in 20 ms frames.
//!
//! The microphone stream is opened only while we transmit (push to talk held, voice
//! activation on, or the mic test running) and closed right after: no audio is captured
//! otherwise.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use bevy::prelude::*;
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use game_voice::{
    FRAME_SAMPLES, SAMPLE_RATE,
    resample::Resampler,
    test_input::{TestInput, TestSource},
};

/// Where samples come from.
enum Source {
    Mic {
        /// Keeps the stream running; dropped to stop it.
        _stream: cpal::Stream,
        /// Mono samples at the device's rate, filled by the audio thread.
        buffer: Arc<Mutex<Vec<f32>>>,
    },
    Test {
        source: TestSource,
        started: Instant,
    },
}

/// An open input: its samples, resampled to 48 kHz and cut into frames.
pub struct Capture {
    source: Source,
    resampler: Resampler,
    pending: Vec<f32>,
    scratch: Vec<f32>,
    /// What it is, for the logs and the settings page.
    pub description: String,
}

impl Capture {
    /// Opens the test input if one is set, else the microphone named `device` (or the
    /// system's default).
    pub fn open(device: Option<&str>, test: Option<&TestInput>) -> Result<Self, String> {
        if let Some(input) = test {
            let source = TestSource::new(input.clone());
            let rate = source.rate();
            return Ok(Self::new(
                Source::Test { source, started: Instant::now() },
                rate,
                format!("test input ({})", describe_test(input)),
            ));
        }
        let host = cpal::default_host();
        let device = match device {
            Some(name) => host
                .input_devices()
                .map_err(|err| err.to_string())?
                .find(|d| device_name(d).as_deref() == Some(name))
                .ok_or_else(|| format!("no microphone named {name}"))?,
            None => host.default_input_device().ok_or("no microphone")?,
        };
        let name = device_name(&device).unwrap_or_else(|| "microphone".into());
        let config = device.default_input_config().map_err(|err| format!("{name}: {err}"))?;
        let rate = config.sample_rate();
        let channels = usize::from(config.channels()).max(1);
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::F64 => build::<f64>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::U16 => build::<u16>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::U32 => build::<u32>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::I8 => build::<i8>(&device, &config.config(), channels, rate, buffer.clone()),
            SampleFormat::U8 => build::<u8>(&device, &config.config(), channels, rate, buffer.clone()),
            other => return Err(format!("{name}: unsupported sample format {other}")),
        }
        .map_err(|err| format!("{name}: {err}"))?;
        stream.play().map_err(|err| format!("{name}: {err}"))?;
        Ok(Self::new(Source::Mic { _stream: stream, buffer }, rate, format!("{name}, {rate} Hz")))
    }

    fn new(source: Source, rate: u32, description: String) -> Self {
        Self {
            source,
            resampler: Resampler::new(rate, SAMPLE_RATE),
            pending: Vec::new(),
            scratch: Vec::new(),
            description,
        }
    }

    /// Takes what was captured since the last call; returns the whole frames.
    pub fn frames(&mut self) -> Vec<Vec<f32>> {
        self.scratch.clear();
        match &mut self.source {
            Source::Mic { buffer, .. } => {
                if let Ok(mut buffer) = buffer.lock() {
                    self.scratch.append(&mut buffer);
                }
            }
            Source::Test { source, started } => source.take(started.elapsed().as_secs_f64(), &mut self.scratch),
        }
        self.resampler.process(&self.scratch, &mut self.pending);
        let whole = self.pending.len() / FRAME_SAMPLES;
        let frames = self.pending.chunks_exact(FRAME_SAMPLES).map(<[f32]>::to_vec).collect();
        self.pending.drain(..whole * FRAME_SAMPLES);
        frames
    }
}

fn describe_test(input: &TestInput) -> String {
    match input {
        TestInput::Tone(freq) => format!("{freq} Hz tone"),
        TestInput::Samples { rate, samples } => format!("{:.1} s at {rate} Hz", samples.len() as f32 / *rate as f32),
    }
}

/// A device's display name.
pub fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

/// The names of the input and output devices, for the settings.
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |devices: Result<_, _>| -> Vec<String> {
        match devices {
            Ok(devices) => {
                let mut names: Vec<String> = Iterator::filter_map(devices, |d: cpal::Device| device_name(&d)).collect();
                names.dedup();
                names
            }
            Err(err) => {
                warn!("voice: can't list audio devices: {err}");
                Vec::new()
            }
        }
    };
    (names(host.input_devices()), names(host.output_devices()))
}

/// An input stream of sample type `T`, mixed down to mono into `buffer` (at most a second is
/// kept, should nobody take it).
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let limit = rate as usize;
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let Ok(mut buffer) = buffer.lock() else {
                return;
            };
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                buffer.push(sum / frame.len() as f32);
            }
            if buffer.len() > limit {
                let excess = buffer.len() - limit;
                buffer.drain(..excess);
            }
        },
        |err| warn!("voice: microphone: {err}"),
        None,
    )
}
