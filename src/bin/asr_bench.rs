use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use amanuensis::asr::fetch;
use amanuensis::asr::{AsrBackend, ModelKind, NemotronBackend};

const SAMPLE_RATE: usize = 16000;
const DEFAULT_FEED: usize = 480;

fn main() {
    let mut feed = DEFAULT_FEED;
    let mut files: Vec<PathBuf> = Vec::new();
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--feed-30ms" => feed = 480,
            "--feed-80ms" => feed = 1280,
            "--feed-240ms" => feed = 3840,
            "--help" | "-h" => {
                println!(
                    "usage: asr_bench [--feed-30ms|--feed-80ms|--feed-240ms] <raw16k-f32le files...>"
                );
                return;
            }
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        eprintln!("no raw files given");
        return;
    }

    println!(
        "== asr_bench (Nemotron, {feed}-sample chunks = {} ms) ==",
        feed * 1000 / SAMPLE_RATE
    );
    let kind = ModelKind::Nemotron;
    let spec = kind.spec();
    let dir = fetch::cached_model_dir(spec);
    let paths = amanuensis::asr::ModelPaths {
        encoder: dir.join("encoder.int8.onnx"),
        decoder: dir.join("decoder.int8.onnx"),
        joiner: dir.join("joiner.int8.onnx"),
        tokens: dir.join("tokens.txt"),
    };

    print!("loading backend once ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let load_started = Instant::now();
    let mut backend = NemotronBackend::load(&paths, None, 2).expect("backend load returned None");
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

        backend.start_session();
        let mut decode_nanos: u128 = 0;
        let mut chunk_min = Duration::MAX;
        let mut chunk_max = Duration::ZERO;
        let mut chunk_count = 0_usize;
        let mut last_partial = String::new();
        let mut advances = 0_usize;
        for chunk in samples.chunks(feed) {
            let started = Instant::now();
            backend.feed_audio(chunk);
            let elapsed = started.elapsed();
            decode_nanos += elapsed.as_nanos();
            chunk_min = chunk_min.min(elapsed);
            chunk_max = chunk_max.max(elapsed);
            chunk_count += 1;
            if let Some(partial) = backend.partial()
                && partial != last_partial
            {
                last_partial.clone_from(&partial);
                advances += 1;
            }
        }

        let finalize_started = Instant::now();
        let transcript = backend.finalize();
        let finalize_secs = finalize_started.elapsed().as_secs_f64();
        let decode_secs = decode_nanos as f64 / 1e9;
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
            "  decode {:.2}s wall | {:.2}x realtime | {} chunks x {feed} = {:.1} ms/chunk avg, min {:.1} max {:.1} | {advances} partial advances",
            decode_secs,
            realtime,
            chunk_count,
            decode_secs * 1000.0 / chunk_count as f64,
            chunk_min.as_secs_f64() * 1000.0,
            chunk_max.as_secs_f64() * 1000.0,
        );
        println!(
            "  finalize {:.2}s | resident {:.0} MiB",
            finalize_secs,
            resident_mib()
        );
        println!("  transcript: {transcript}");
    }
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
