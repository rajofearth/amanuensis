use std::{
    error::Error,
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use cpal::{
    SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

const WINDOW_MS: f32 = 30.0;

pub fn spawn(sender: Sender<f32>) {
    thread::spawn(move || {
        if let Err(error) = run(sender) {
            eprintln!("audio capture failed: {error}");
        }
    });
}

fn run(sender: Sender<f32>) -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no default input device")?;
    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let window_samples = ((config.sample_rate.0 as f32 * WINDOW_MS / 1000.0).ceil() as usize).max(1);
    let stream = match format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, sender, window_samples, |s| s),
        SampleFormat::I16 => {
            build_stream::<i16>(&device, &config, sender, window_samples, |s| {
                s as f32 / i16::MAX as f32
            })
        }
        SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, sender, window_samples, |s| {
                (s as f32 - 32768.0) / 32768.0
            })
        }
        other => return Err(format!("unsupported input sample format: {other:?}").into()),
    }?;
    stream.play()?;
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    sender: Sender<f32>,
    window_samples: usize,
    to_f32: fn(T) -> f32,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + 'static,
{
    let channels = config.channels.max(1) as usize;
    let mut accumulator = Accumulator::new(window_samples);
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            for frame in data.chunks_exact(channels) {
                let mono = frame.iter().copied().map(to_f32).sum::<f32>() / channels as f32;
                if let Some(rms) = accumulator.push(mono) {
                    let _ = sender.send(rms);
                }
            }
        },
        |error| eprintln!("input stream error: {error}"),
        None,
    )
}

struct Accumulator {
    window_samples: usize,
    count: usize,
    sum_squares: f32,
}

impl Accumulator {
    fn new(window_samples: usize) -> Self {
        Self {
            window_samples,
            count: 0,
            sum_squares: 0.0,
        }
    }

    fn push(&mut self, sample: f32) -> Option<f32> {
        self.sum_squares += sample * sample;
        self.count += 1;
        if self.count >= self.window_samples {
            let rms = (self.sum_squares / self.count as f32).sqrt();
            self.count = 0;
            self.sum_squares = 0.0;
            Some(rms)
        } else {
            None
        }
    }
}
