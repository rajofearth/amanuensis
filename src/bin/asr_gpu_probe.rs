use std::{fs, path::PathBuf, time::Instant};

use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OnlineRecognizer, OnlineRecognizerConfig,
    OnlineTransducerModelConfig,
};

const SAMPLE_RATE: i32 = 16000;
const FEATURE_DIM: i32 = 128;
const FEED_SAMPLES: usize = 480;
const DEFAULT_THREADS: i32 = 2;
const PROVIDERS: &[&str] = &["cpu", "dml", "vulkan", "cuda", "openvino", "coreml"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut threads = DEFAULT_THREADS;
    let mut positional: Vec<String> = Vec::new();
    for argument in &args {
        match argument.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: asr_gpu_probe <moonshine-dir> <nemotron-dir> <raw16k-f32le-file> [--threads=N]"
                );
                return;
            }
            other => {
                if let Some(value) = other.strip_prefix("--threads=") {
                    threads = value.parse().expect("--threads must be an integer");
                } else {
                    positional.push(other.to_string());
                }
            }
        }
    }
    if positional.len() != 3 {
        eprintln!(
            "usage: asr_gpu_probe <moonshine-dir> <nemotron-dir> <raw16k-f32le-file> [--threads=N]"
        );
        return;
    }
    let moonshine_dir = PathBuf::from(&positional[0]);
    let nemotron_dir = PathBuf::from(&positional[1]);
    let raw_path = PathBuf::from(&positional[2]);

    println!("== asr_gpu_probe ({threads} threads) ==");
    println!("moonshine_dir : {}", moonshine_dir.to_string_lossy());
    println!("nemotron_dir  : {}", nemotron_dir.to_string_lossy());
    println!("raw file      : {}", raw_path.to_string_lossy());

    let samples = read_raw(&raw_path);
    let audio_secs = samples.len() as f64 / SAMPLE_RATE as f64;
    println!("audio: {audio_secs:.1}s, {} samples\n", samples.len());

    for provider in PROVIDERS {
        probe_offline(&moonshine_dir, &samples, audio_secs, threads, provider);
        probe_online(&nemotron_dir, &samples, audio_secs, threads, provider);
        println!();
    }
}

fn probe_offline(dir: &PathBuf, samples: &[f32], audio_secs: f64, threads: i32, provider: &str) {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.num_threads = threads;
    config.model_config.moonshine.preprocessor = Some(path_string(&dir.join("preprocess.onnx")));
    config.model_config.moonshine.encoder = Some(path_string(&dir.join("encode.int8.onnx")));
    config.model_config.moonshine.uncached_decoder =
        Some(path_string(&dir.join("uncached_decode.int8.onnx")));
    config.model_config.moonshine.cached_decoder =
        Some(path_string(&dir.join("cached_decode.int8.onnx")));
    config.model_config.tokens = Some(path_string(&dir.join("tokens.txt")));
    config.model_config.provider = Some(provider.to_string());

    let Some(recognizer) = OfflineRecognizer::create(&config) else {
        println!("[moonshine offline] provider={provider} -> create()=None");
        return;
    };
    print!("[moonshine offline] provider={provider} -> create()=Some  | ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();

    let stream = recognizer.create_stream();
    let decode_started = Instant::now();
    stream.accept_waveform(SAMPLE_RATE, samples);
    recognizer.decode(&stream);
    let text = stream.get_result().map(|r| r.text).unwrap_or_default();
    let decode_secs = decode_started.elapsed().as_secs_f64();
    let realtime = if audio_secs > 0.0 {
        decode_secs / audio_secs
    } else {
        0.0
    };

    println!(
        "decode {:.2}s | {:.2}x realtime | resident {:.0} MiB",
        decode_secs,
        realtime,
        resident_mib()
    );
    println!("    transcript: {text}");
}

fn probe_online(dir: &PathBuf, samples: &[f32], audio_secs: f64, threads: i32, provider: &str) {
    let mut config = OnlineRecognizerConfig::default();
    config.feat_config.sample_rate = SAMPLE_RATE;
    config.feat_config.feature_dim = FEATURE_DIM;
    config.model_config.transducer = OnlineTransducerModelConfig {
        encoder: Some(path_string(&dir.join("encoder.int8.onnx"))),
        decoder: Some(path_string(&dir.join("decoder.int8.onnx"))),
        joiner: Some(path_string(&dir.join("joiner.int8.onnx"))),
    };
    config.model_config.tokens = Some(path_string(&dir.join("tokens.txt")));
    config.model_config.num_threads = threads;
    config.model_config.provider = Some(provider.to_string());
    config.decoding_method = Some("greedy_search".to_string());
    config.enable_endpoint = false;

    let Some(recognizer) = OnlineRecognizer::create(&config) else {
        println!("[nemotron online]  provider={provider} -> create()=None");
        return;
    };
    print!("[nemotron online]  provider={provider} -> create()=Some  | ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();

    let stream = recognizer.create_stream();
    let feed_started = Instant::now();
    for chunk in samples.chunks(FEED_SAMPLES) {
        stream.accept_waveform(SAMPLE_RATE, chunk);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
    }
    let feed_secs = feed_started.elapsed().as_secs_f64();
    let realtime = if audio_secs > 0.0 {
        feed_secs / audio_secs
    } else {
        0.0
    };

    stream.input_finished();
    while recognizer.is_ready(&stream) {
        recognizer.decode(&stream);
    }
    let text = recognizer
        .get_result(&stream)
        .map(|r| r.text)
        .unwrap_or_default();

    println!(
        "decode {:.2}s | {:.2}x realtime | resident {:.0} MiB",
        feed_secs,
        realtime,
        resident_mib()
    );
    println!("    transcript: {text}");
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn read_raw(path: &PathBuf) -> Vec<f32> {
    let bytes = fs::read(path).expect("read raw file");
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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
