use std::{
    error::Error,
    io::Cursor,
    sync::mpsc::{self, Sender},
    thread,
};

use crate::log;
use cpal::{
    SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

const TARGET_SAMPLE_RATE: u32 = 16000;
const CHUNK_SAMPLES: usize = 480;

/// Gates whether microphone capture is active. Capture should only run while
/// the user is recording (or checking the mic in setup), never for the whole
/// lifetime of the app, so the OS "mic in use" indicator and the device are
/// only claimed during those windows.
#[derive(Clone)]
pub struct Recorder(mpsc::Sender<bool>);

impl Recorder {
    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.0.send(enabled);
    }
}

/// Spawns the microphone-capture thread. The returned [`Recorder`] gates
/// whether the device stream is actually open: while disabled the thread keeps
/// the device closed and produces no chunks.
pub fn spawn(sender: Sender<Vec<f32>>) -> Recorder {
    let (command_tx, command_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let mut stream: Option<cpal::Stream> = None;
        let mut was_enabled = false;
        while let Ok(enabled) = command_rx.recv() {
            match gate_action(was_enabled, enabled) {
                GateAction::Open => {
                    if stream.is_none() {
                        if let Err(error) = open_capture(sender.clone()) {
                            log!("audio", "capture open failed: {error}");
                        }
                    }
                }
                GateAction::Close => {
                    if let Some(active) = stream.take() {
                        drop(active);
                        log!("audio", "capture stopped");
                    }
                }
                GateAction::None => {}
            }
            was_enabled = enabled;
        }
    });
    Recorder(command_tx)
}

enum GateAction {
    /// Device is idle and capture was just turned on: open the stream.
    Open,
    /// Capture was just turned off: close the stream.
    Close,
    /// State unchanged; do nothing.
    None,
}

/// Decides what to do on one enable-flag update. Opens only on a rising edge
/// (so a device that fails to open isn't retried every frame), closes only on
/// a falling edge.
fn gate_action(was_enabled: bool, enabled: bool) -> GateAction {
    match (was_enabled, enabled) {
        (false, true) => GateAction::Open,
        (true, false) => GateAction::Close,
        _ => GateAction::None,
    }
}

fn open_capture(sender: Sender<Vec<f32>>) -> Result<cpal::Stream, Box<dyn Error>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no default input device")?;
    let supported = device.default_input_config()?;
    let config: StreamConfig = supported.into();
    let resample_from =
        (config.sample_rate.0 != TARGET_SAMPLE_RATE).then_some(config.sample_rate.0);
    let stream = open_stream(&device, &config, resample_from, sender)?;
    stream.play()?;
    log!("audio", "capture started");
    Ok(stream)
}

fn open_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    resample_from: Option<u32>,
    sender: Sender<Vec<f32>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    build_stream::<f32>(device, config, sender.clone(), resample_from, |sample| {
        sample
    })
    .or_else(|_| {
        build_stream::<i16>(device, config, sender.clone(), resample_from, |sample| {
            sample as f32 / i16::MAX as f32
        })
    })
    .or_else(|_| {
        build_stream::<u16>(device, config, sender, resample_from, |sample| {
            (sample as f32 - 32768.0) / 32768.0
        })
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    sender: Sender<Vec<f32>>,
    resample_from: Option<u32>,
    to_f32: fn(T) -> f32,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + Copy + 'static,
{
    let channels = config.channels.max(1) as usize;
    let mut pipeline = Pipeline::new(sender, resample_from);
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            for frame in data.chunks_exact(channels) {
                pipeline.push(frame.iter().copied().map(to_f32).sum::<f32>() / channels as f32);
            }
        },
        |error| eprintln!("input stream error: {error}"),
        None,
    )
}

struct Pipeline {
    resampler: Option<Resampler>,
    chunker: Chunker,
}

impl Pipeline {
    fn new(sender: Sender<Vec<f32>>, resample_from: Option<u32>) -> Self {
        Self {
            resampler: resample_from.map(Resampler::new),
            chunker: Chunker::new(sender),
        }
    }

    fn push(&mut self, sample: f32) {
        if let Some(resampler) = &mut self.resampler {
            resampler.push(sample, &mut |sample| self.chunker.push(sample));
        } else {
            self.chunker.push(sample);
        }
    }
}

struct Resampler {
    step: f64,
    position: f64,
    previous: f32,
    latest: f32,
    primed: bool,
}

impl Resampler {
    fn new(input_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / TARGET_SAMPLE_RATE as f64,
            position: 0.0,
            previous: 0.0,
            latest: 0.0,
            primed: false,
        }
    }

    fn push(&mut self, sample: f32, emit: &mut impl FnMut(f32)) {
        if !self.primed {
            self.previous = sample;
            self.latest = sample;
            self.primed = true;
            return;
        }
        self.previous = self.latest;
        self.latest = sample;
        while self.position < 1.0 {
            emit(self.previous + self.position as f32 * (self.latest - self.previous));
            self.position += self.step;
        }
        self.position -= 1.0;
    }
}

struct Chunker {
    buffer: Vec<f32>,
    sender: Sender<Vec<f32>>,
}

impl Chunker {
    fn new(sender: Sender<Vec<f32>>) -> Self {
        Self {
            buffer: Vec::new(),
            sender,
        }
    }

    fn push(&mut self, sample: f32) {
        self.buffer.push(sample);
        if self.buffer.len() == CHUNK_SAMPLES {
            let _ = self.sender.send(std::mem::take(&mut self.buffer));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GateAction, Resampler, gate_action};

    #[test]
    fn resampler_produces_target_rate() {
        const INPUT_RATE: u32 = 48_000;
        let input: Vec<f32> = (0..INPUT_RATE as usize)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / INPUT_RATE as f32).sin())
            .collect();
        let mut resampler = Resampler::new(INPUT_RATE);
        let mut output = Vec::new();
        for sample in input {
            resampler.push(sample, &mut |sample| output.push(sample));
        }
        assert!((output.len() as i64 - 16_000).abs() < 100);
    }

    #[test]
    fn gate_opens_only_on_rising_edge_and_closes_on_falling_edge() {
        use GateAction::{Close, None, Open};

        assert!(matches!(gate_action(false, true), Open));
        assert!(matches!(gate_action(false, false), None));
        assert!(matches!(gate_action(true, true), None));
        assert!(matches!(gate_action(true, false), Close));
    }
}

#[derive(Clone, Copy)]
pub enum Sound {
    Start,
    Cancel,
    Success,
    Failure,
}

pub fn play(sound: Sound) {
    thread::spawn(move || {
        let bytes = match sound {
            Sound::Start => include_bytes!("../assets/audio/yup-01.mp3").as_slice(),
            Sound::Cancel | Sound::Failure => {
                include_bytes!("../assets/audio/nope-03.mp3").as_slice()
            }
            Sound::Success => include_bytes!("../assets/audio/yup-01.mp3").as_slice(),
        };
        let Ok(stream) = rodio::OutputStreamBuilder::open_default_stream() else {
            return;
        };
        let Ok(source) = rodio::Decoder::try_from(Cursor::new(bytes)) else {
            return;
        };
        let sink = rodio::Sink::connect_new(stream.mixer());
        sink.append(source);
        sink.sleep_until_end();
    });
}
