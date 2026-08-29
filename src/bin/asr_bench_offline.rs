use std::{fs, path::PathBuf, time::Instant};

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

const SAMPLE_RATE: usize = 16000;
const DEFAULT_THREADS: i32 = 2;

fn main() {
    let mut threads = DEFAULT_THREADS;
    let mut model_dir: Option<PathBuf> = None;
    let mut files: Vec<PathBuf> = Vec::new();
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: asr_bench_offline [--threads=N] <moonshine-model-dir> <raw16k-f32le files...>"
                );
                return;
            }
            other => {
                if let Some(value) = other.strip_prefix("--threads=") {
                    threads = value.parse().expect("--threads must be an integer");
                } else if model_dir.is_none() {
                    model_dir = Some(PathBuf::from(other));
                } else {
                    files.push(PathBuf::from(other));
                }
            }
        }
    }
    let Some(dir) = model_dir else {
        eprintln!(
            "usage: asr_bench_offline [--threads=N] <moonshine-model-dir> <raw16k-f32le files...>"
        );
        return;
    };
    if files.is_empty() {
        eprintln!("no raw files given");
        return;
    }

    println!("== asr_bench_offline (Moonshine, {threads} threads) ==",);

    print!("loading offline recognizer once ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let load_started = Instant::now();
    let recognizer = build_recognizer(&dir, threads);
    println!(
        "{:.2}s resident {:.0} MiB",
        load_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    for path in &files {
        let samples = read_raw(path);
        let audio_secs = samples.len() as f64 / SAMPLE_RATE as f64;
        let peak = samples
            .iter()
            .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));
        let rms = (samples.iter().map(|s| *s * s).sum::<f32>() / samples.len() as f32).sqrt();

        let stream = recognizer.create_stream();
        let decode_started = Instant::now();
        stream.accept_waveform(SAMPLE_RATE as i32, &samples);
        recognizer.decode(&stream);
        let text = stream.get_result().map(|r| r.text).unwrap_or_default();
        let decode_secs = decode_started.elapsed().as_secs_f64();
        let realtime = if audio_secs > 0.0 {
            decode_secs / audio_secs
        } else {
            0.0
        };

        println!(
            "\n[{}] {:.1}s audio, {} samples, peak {:.3} rms {:.3}",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            audio_secs,
            samples.len(),
            peak,
            rms
        );
        println!(
            "decode {:.2}s wall | {:.2}x realtime | 1 offline decode",
            decode_secs, realtime
        );
        println!("resident {:.0} MiB", resident_mib());
        println!("transcript: {text}");
    }
}

fn build_recognizer(dir: &PathBuf, threads: i32) -> OfflineRecognizer {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.num_threads = threads;
    config.model_config.moonshine.preprocessor = Some(path_string(&dir.join("preprocess.onnx")));
    config.model_config.moonshine.encoder = Some(path_string(&dir.join("encode.int8.onnx")));
    config.model_config.moonshine.uncached_decoder =
        Some(path_string(&dir.join("uncached_decode.int8.onnx")));
    config.model_config.moonshine.cached_decoder =
        Some(path_string(&dir.join("cached_decode.int8.onnx")));
    config.model_config.tokens = Some(path_string(&dir.join("tokens.txt")));
    config.model_config.provider = Some("cpu".into());
    OfflineRecognizer::create(&config).expect("offline create returned None")
}

fn path_string(path: &PathBuf) -> String {
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
