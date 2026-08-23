use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use hound::{SampleFormat, WavReader};
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineTransducerModelConfig};
use sherpa_onnx_sys::FeatureConfig;

const REPO_OWNER: &str = "csukuangfj2";
const REPO_NAME: &str = "sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25";

const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

const SAMPLE_RATE: i32 = 16000;
const FEATURE_DIM: i32 = 128;

fn main() {
    println!("== nemotron streaming asr smoke test ==");

    let client = hf_hub::HFClientSync::new().expect("failed to initialize hf-hub blocking client");
    let repo = client.model(REPO_OWNER, REPO_NAME);

    let mut model_paths: Vec<PathBuf> = Vec::with_capacity(MODEL_FILES.len());
    for file in MODEL_FILES {
        print!("ensuring {file} ... ");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let path = repo
            .download_file()
            .filename(file)
            .send()
            .unwrap_or_else(|error| panic!("download {file} failed: {error}"));
        println!("{}", path.display());
        model_paths.push(path);
    }
    let wav_path = repo
        .download_file()
        .filename("test_wavs/0.wav")
        .send()
        .expect("download test_wavs/0.wav");
    let trans_path = repo
        .download_file()
        .filename("test_wavs/trans.txt")
        .send()
        .expect("download test_wavs/trans.txt");

    let (samples, native_rate) = read_wav_mono(&wav_path);
    let samples = resample_to_16k(&samples, native_rate);
    println!(
        "wav decoded: {} mono samples @ {native_rate} Hz -> {} ms at {SAMPLE_RATE} Hz",
        samples.len(),
        samples.len() * 1000 / SAMPLE_RATE as usize
    );

    let mut config = OnlineRecognizerConfig::default();
    config.feat_config = FeatureConfig {
        sample_rate: SAMPLE_RATE,
        feature_dim: FEATURE_DIM,
    };
    config.model_config.transducer = OnlineTransducerModelConfig {
        encoder: Some(path_string(&model_paths[0])),
        decoder: Some(path_string(&model_paths[1])),
        joiner: Some(path_string(&model_paths[2])),
    };
    config.model_config.tokens = Some(path_string(&model_paths[3]));
    config.decoding_method = Some("greedy_search".to_string());
    config.enable_endpoint = false;

    print!("loading recognizer ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let load_started = Instant::now();
    let recognizer =
        OnlineRecognizer::create(&config).expect("OnlineRecognizer::create returned None");
    let load_seconds = load_started.elapsed().as_secs_f64();
    println!("{load_seconds:.2}s resident {:.0} MiB", resident_mib());

    let decode_started = Instant::now();
    let stream = recognizer.create_stream();
    stream.accept_waveform(SAMPLE_RATE, &samples);
    while recognizer.is_ready(&stream) {
        recognizer.decode(&stream);
    }
    stream.input_finished();
    while recognizer.is_ready(&stream) {
        recognizer.decode(&stream);
    }
    let hypothesis = recognizer
        .get_result(&stream)
        .map(|result| result.text)
        .unwrap_or_default();
    println!(
        "decode took {:.2}s resident {:.0} MiB",
        decode_started.elapsed().as_secs_f64(),
        resident_mib()
    );
    println!("hypothesis: {hypothesis}");

    let expected = expected_for_zero(&trans_path);
    println!("expected:   {expected}");
    let got = normalize(&hypothesis);
    let want = normalize(&expected);
    if !want.is_empty() && (got.contains(&want) || want.contains(&got)) {
        println!("CHECKLIST A: MATCH -> default feature normalization behaves as NONE");
    } else {
        println!("CHECKLIST A: MISMATCH -> inspect normalization / feature_dim handling");
    }

    print!("unloading recognizer ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let unload_started = Instant::now();
    drop(recognizer);
    println!(
        "{:.2}s resident {:.0} MiB",
        unload_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    print!("warm reload ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let reload_started = Instant::now();
    let reloaded = OnlineRecognizer::create(&config).expect("warm reload returned None");
    println!(
        "{:.2}s resident {:.0} MiB",
        reload_started.elapsed().as_secs_f64(),
        resident_mib()
    );
    drop(reloaded);
}

fn expected_for_zero(trans_path: &Path) -> String {
    let contents = std::fs::read_to_string(trans_path).unwrap_or_default();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("0.wav") || trimmed.starts_with("0\t") || trimmed.starts_with("0 ") {
            return trimmed
                .split_once(['\t', ' '])
                .map(|(_, rest)| rest.trim().to_owned())
                .unwrap_or_else(|| trimmed.to_owned());
        }
    }
    contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn read_wav_mono(path: &Path) -> (Vec<f32>, u32) {
    let reader = WavReader::open(path).expect("open wav file");
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let mut samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Float => reader
            .into_samples::<f32>()
            .collect::<Result<Vec<f32>, _>>()
            .expect("read float wav samples"),
        SampleFormat::Int => reader
            .into_samples::<i16>()
            .collect::<Result<Vec<i16>, _>>()
            .expect("read int wav samples")
            .into_iter()
            .map(|sample| sample as f32 / 32768.0)
            .collect(),
    };
    if channels > 1 {
        samples = samples
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect();
    }
    (samples, spec.sample_rate)
}

fn resample_to_16k(samples: &[f32], from_rate: u32) -> Vec<f32> {
    if from_rate == SAMPLE_RATE as u32 {
        return samples.to_vec();
    }
    let step = from_rate as f64 / SAMPLE_RATE as f64;
    let out_len = ((samples.len() as f64 - 1.0) / step).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for index in 0..out_len {
        let position = index as f64 * step;
        let base = position.floor() as usize;
        let fraction = (position - base as f64) as f32;
        let current = samples[base];
        let next = samples.get(base + 1).copied().unwrap_or(current);
        out.push(current + fraction * (next - current));
    }
    out
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn resident_mib() -> f64 {
    use sysinfo::{ProcessesToUpdate, System, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return 0.0;
    };
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .map(|process| process.memory() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0)
}
