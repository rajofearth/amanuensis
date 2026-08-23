use std::{error::Error, sync::mpsc::Sender, thread, time::Duration};

use cpal::{
    SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

const TARGET_SAMPLE_RATE: u32 = 16000;
const CHUNK_SAMPLES: usize = 480;

pub fn spawn(sender: Sender<Vec<f32>>) {
    thread::spawn(move || {
        if let Err(error) = run(sender) {
            eprintln!("audio capture failed: {error}");
        }
    });
}

fn run(sender: Sender<Vec<f32>>) -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no default input device")?;
    let supported = device.default_input_config()?;

    let preferred = StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(TARGET_SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };
    let stream =
        open_stream(&device, &preferred, None, sender.clone()).or_else(|preferred_error| {
            let native_rate = supported.sample_rate().0;
            let resample_from = (native_rate != TARGET_SAMPLE_RATE).then_some(native_rate);
            let config: StreamConfig = supported.into();
            open_stream(&device, &config, resample_from, sender).map_err(|error| {
                eprintln!("native-rate stream also failed: {error}");
                preferred_error
            })
        })?;
    stream.play()?;
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn open_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    resample_from: Option<u32>,
    sender: Sender<Vec<f32>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    build_stream::<f32>(device, config, sender.clone(), resample_from, |s| s)
        .or_else(|_| {
            build_stream::<i16>(device, config, sender.clone(), resample_from, |s| {
                s as f32 / i16::MAX as f32
            })
        })
        .or_else(|_| {
            build_stream::<u16>(device, config, sender, resample_from, |s| {
                (s as f32 - 32768.0) / 32768.0
            })
        })
        .map_err(|error| {
            eprintln!(
                "failed to open stream at {} Hz ({} ch): {error}",
                config.sample_rate.0, config.channels
            );
            error
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
        move |data: &[T], _| pipeline.ingest(data, channels, to_f32),
        |error| eprintln!("input stream error: {error}"),
        None,
    )
}

struct Pipeline {
    resampler: Option<Resampler>,
    chunker: Chunker,
    scratch: Vec<f32>,
}

impl Pipeline {
    fn new(sender: Sender<Vec<f32>>, resample_from: Option<u32>) -> Self {
        Self {
            resampler: resample_from.map(Resampler::new),
            chunker: Chunker::new(sender),
            scratch: Vec::new(),
        }
    }

    fn ingest<T: Copy>(&mut self, data: &[T], channels: usize, to_f32: fn(T) -> f32) {
        self.scratch.clear();
        for frame in data.chunks_exact(channels) {
            self.scratch
                .push(frame.iter().copied().map(to_f32).sum::<f32>() / channels as f32);
        }
        match self.resampler.as_mut() {
            Some(resampler) => {
                let chunker = &mut self.chunker;
                resampler.push(&self.scratch, &mut |sample| chunker.push(sample));
            }
            None => {
                for &sample in &self.scratch {
                    self.chunker.push(sample);
                }
            }
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

    fn push(&mut self, samples: &[f32], emit: &mut impl FnMut(f32)) {
        for &sample in samples {
            if !self.primed {
                self.previous = sample;
                self.latest = sample;
                self.primed = true;
                continue;
            }
            self.previous = self.latest;
            self.latest = sample;
            while self.position < 1.0 {
                emit(self.previous + (self.position as f32) * (self.latest - self.previous));
                self.position += self.step;
            }
            self.position -= 1.0;
        }
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
