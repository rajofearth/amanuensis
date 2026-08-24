use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use hound::{SampleFormat, WavReader};
use raycast_dictation_clone::asr::fetch;
use raycast_dictation_clone::asr::{AsrBackend, ModelKind, ModelPaths, NemotronBackend};

const SAMPLE_RATE: i32 = 16000;
const CHUNK_SAMPLES: usize = 480;
const HOLD_SECONDS: usize = 300;
const SAMPLE_INTERVAL_SECONDS: usize = 30;
const SPEECH_SECONDS: f64 = 6.0;
const GAP_SECONDS: f64 = 1.5;

fn main() {
    let mut kind = ModelKind::Nemotron;
    let mut reload_metrics = false;
    let mut hold_test = false;
    let mut hold_seconds = HOLD_SECONDS;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "nemotron" => kind = ModelKind::Nemotron,
            "--reload-metrics" => reload_metrics = true,
            "--hold-test" => hold_test = true,
            other => {
                if let Some(value) = other.strip_prefix("--hold-seconds=") {
                    hold_seconds = value.parse().expect("--hold-seconds must be an integer");
                } else {
                    panic!(
                        "unknown argument '{other}' (expected nemotron|--reload-metrics|--hold-test|--hold-seconds=N)"
                    );
                }
            }
        }
    }

    println!("== asr smoke ({kind:?}) ==");
    let (paths, wav_path, expected) = ensure_model_files(kind);

    if hold_test {
        run_hold_test(kind, &paths, &wav_path, hold_seconds);
        return;
    }
    if reload_metrics {
        run_reload_metrics(kind, &paths);
        return;
    }
    run_transcription_check(kind, &paths, &wav_path, &expected);
}

fn ensure_model_files(kind: ModelKind) -> (ModelPaths, PathBuf, String) {
    let spec = kind.spec();
    fetch::ensure_file(spec, "test_wavs/0.wav", None, &mut |_| {})
        .expect("download test_wavs/0.wav");
    fetch::ensure_file(spec, "test_wavs/trans.txt", None, &mut |_| {})
        .expect("download test_wavs/trans.txt");

    let dir = fetch::cached_model_dir(spec);
    let wav_path = dir.join("test_wavs").join("0.wav");
    let trans_path = dir.join("test_wavs").join("trans.txt");
    let expected = expected_for_zero(&trans_path);

    (
        ModelPaths {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        wav_path,
        expected,
    )
}

fn make_backend(_kind: ModelKind, paths: &ModelPaths) -> Option<Box<dyn AsrBackend>> {
    NemotronBackend::load(paths).map(|backend| Box::new(backend) as _)
}

fn feed_chunk_samples(_kind: ModelKind) -> usize {
    if let Ok(value) = std::env::var("ASR_SMOKE_FEED_SAMPLES") {
        value
            .parse()
            .expect("ASR_SMOKE_FEED_SAMPLES must be an integer")
    } else {
        CHUNK_SAMPLES
    }
}

fn run_transcription_check(kind: ModelKind, paths: &ModelPaths, wav_path: &Path, expected: &str) {
    let raw_mode = std::env::var("ASR_SMOKE_RAW").is_ok();
    let mut samples = match std::env::var("ASR_SMOKE_RAW") {
        Ok(path) => {
            let bytes = std::fs::read(&path).expect("read ASR_SMOKE_RAW file");
            println!(
                "raw input: {} ({} samples @ 16000 Hz)",
                path,
                bytes.len() / 4
            );
            bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect::<Vec<f32>>()
        }
        Err(_) => {
            let (loaded, native_rate) = read_wav_mono(wav_path);
            if std::env::var("ASR_SMOKE_NN_RESAMPLE").is_ok() {
                nearest_resample_to_16k(&loaded, native_rate)
            } else {
                resample_to_16k(&loaded, native_rate)
            }
        }
    };
    if let Ok(value) = std::env::var("ASR_SMOKE_GAIN") {
        let gain: f32 = value.parse().expect("ASR_SMOKE_GAIN must be a number");
        for sample in samples.iter_mut() {
            *sample *= gain;
        }
    }
    if let Ok(secs) = std::env::var("ASR_SMOKE_PAD_LEAD_SECS") {
        let pad: f64 = secs
            .parse()
            .expect("ASR_SMOKE_PAD_LEAD_SECS must be a number");
        let mut padded = vec![0.0_f32; (pad * SAMPLE_RATE as f64) as usize];
        padded.extend_from_slice(&samples);
        samples = padded;
    }
    if let Ok(secs) = std::env::var("ASR_SMOKE_TRUNCATE_SECS") {
        let limit: f64 = secs
            .parse()
            .expect("ASR_SMOKE_TRUNCATE_SECS must be a number");
        samples.truncate((limit * SAMPLE_RATE as f64) as usize);
    }
    let peak = samples
        .iter()
        .fold(0.0_f32, |max, sample| max.max(sample.abs()));
    let rms =
        (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt();
    println!("input level: peak {peak:.4} rms {rms:.4}");
    if !raw_mode {
        println!(
            "wav decoded: {} mono samples -> {} ms at {SAMPLE_RATE} Hz",
            samples.len(),
            samples.len() * 1000 / SAMPLE_RATE as usize
        );
    }

    print!("loading {:?} backend ... ", kind);
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let load_started = Instant::now();
    let mut backend = make_backend(kind, paths).expect("backend load returned None");
    println!(
        "{:.2}s resident {:.0} MiB",
        load_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    let decode_started = Instant::now();
    let feed = feed_chunk_samples(kind);
    backend.start_session();
    let mut last_partial = String::new();
    let mut partial_advances = 0_usize;
    for chunk in samples.chunks(feed) {
        backend.feed_audio(chunk);
        if let Some(partial) = backend.partial()
            && partial != last_partial
        {
            last_partial.clone_from(&partial);
            partial_advances += 1;
        }
    }
    let transcript = backend.finalize();
    println!(
        "decode took {:.2}s across {} x {}-sample feeds ({partial_advances} partial advances) resident {:.0} MiB",
        decode_started.elapsed().as_secs_f64(),
        samples.len().div_ceil(feed),
        feed,
        resident_mib()
    );
    println!("transcript: {transcript}");

    if raw_mode {
        println!("RAW MODE ({kind:?}): transcript above — inspect manually");
        return;
    }
    println!("expected:   {expected}");
    let got = normalize(&transcript);
    let want = normalize(&expected);
    if !want.is_empty() && (got.contains(&want) || want.contains(&got)) {
        println!(
            "CHECKLIST A ({kind:?}): MATCH -> encoder-metadata normalization (per_feature) honored"
        );
    } else {
        println!(
            "CHECKLIST A ({kind:?}): MISMATCH -> inspect normalization / feature_dim handling"
        );
    }
}

fn run_reload_metrics(kind: ModelKind, paths: &ModelPaths) {
    print!("cold load ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let cold_started = Instant::now();
    let backend = make_backend(kind, paths).expect("cold load returned None");
    println!(
        "{:.2}s resident {:.0} MiB",
        cold_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    print!("unload ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let unload_started = Instant::now();
    drop(backend);
    println!(
        "{:.2}s resident {:.0} MiB",
        unload_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    print!("warm reload ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let warm_started = Instant::now();
    let reloaded = make_backend(kind, paths).expect("warm reload returned None");
    println!(
        "{:.2}s resident {:.0} MiB",
        warm_started.elapsed().as_secs_f64(),
        resident_mib()
    );
    drop(reloaded);
    println!("post-drop resident {:.0} MiB", resident_mib());
}

fn run_hold_test(kind: ModelKind, paths: &ModelPaths, wav_path: &Path, hold_seconds: usize) {
    let (source, native_rate) = read_wav_mono(wav_path);
    let source = resample_to_16k(&source, native_rate);
    let total_samples = hold_seconds * SAMPLE_RATE as usize;
    let dictation = dictation_audio(&source, total_samples);
    println!(
        "hold audio: {} samples = {} s ({}s speech / {}s gap rhythm from test wav)",
        dictation.len(),
        dictation.len() / SAMPLE_RATE as usize,
        SPEECH_SECONDS,
        GAP_SECONDS
    );

    print!("loading {kind:?} backend ... ");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let load_started = Instant::now();
    let mut backend = make_backend(kind, paths).expect("backend load returned None");
    println!(
        "{:.2}s resident {:.0} MiB",
        load_started.elapsed().as_secs_f64(),
        resident_mib()
    );

    backend.start_session();
    let started = Instant::now();
    let feed = feed_chunk_samples(kind);
    let mut decode_nanos = 0_u128;
    let mut last_partial = String::new();
    let mut last_advance_sample = 0_usize;
    let mut advances = 0_usize;
    let mut next_mark = SAMPLE_INTERVAL_SECONDS * SAMPLE_RATE as usize;
    let mut baseline_rss: Option<f64> = None;
    let mut chunks_done = 0_usize;
    let mut mark_nanos = 0_u128;
    let mut mark_chunks = 0_usize;

    for (index, chunk) in dictation.chunks(feed).enumerate() {
        let fed = index * feed;
        let decode_started = Instant::now();
        backend.feed_audio(chunk);
        if let Some(partial) = backend.partial()
            && partial != last_partial
        {
            last_partial.clone_from(&partial);
            advances += 1;
            last_advance_sample = fed;
        }
        decode_nanos += decode_started.elapsed().as_nanos();
        chunks_done += 1;

        if fed + chunk.len() >= next_mark {
            let rss = resident_mib();
            baseline_rss.get_or_insert(rss);
            let window_chunks = (chunks_done - mark_chunks).max(1);
            let window_ms_per_chunk =
                (decode_nanos - mark_nanos) as f64 / 1e6 / window_chunks as f64;
            println!(
                "{:>4}s audio | wall {:>6.1}s | cumulative decode {:>6.2}s | window {:>5.1} ms/chunk | rss {:>7.1} MiB | partial {:>4} chars advanced {}x last @{}s",
                next_mark / SAMPLE_RATE as usize,
                started.elapsed().as_secs_f64(),
                decode_nanos as f64 / 1e9,
                window_ms_per_chunk,
                rss,
                last_partial.chars().count(),
                advances,
                last_advance_sample / SAMPLE_RATE as usize,
            );
            mark_nanos = decode_nanos;
            mark_chunks = chunks_done;
            next_mark += SAMPLE_INTERVAL_SECONDS * SAMPLE_RATE as usize;
        }
    }

    let finalize_started = Instant::now();
    let text = backend.finalize();
    let skip = text.chars().count().saturating_sub(120);
    let tail: String = text.chars().skip(skip).collect();
    println!(
        "finalize took {:.2}s | final text {} chars | partials advanced {}x, last advance @{}s of {}s",
        finalize_started.elapsed().as_secs_f64(),
        text.chars().count(),
        advances,
        last_advance_sample / SAMPLE_RATE as usize,
        hold_seconds
    );
    println!("tail: ...{tail}");

    let final_rss = resident_mib();
    let baseline = baseline_rss.unwrap_or(final_rss);
    let growth = final_rss - baseline;
    println!(
        "rss trajectory: first-sample {baseline:.0} MiB -> final {final_rss:.0} MiB (growth {growth:.0} MiB)"
    );
    if text.trim().is_empty() {
        println!("HOLD VERDICT ({kind:?}): FAIL -> empty final text after {hold_seconds}s hold");
    } else if advances == 0 {
        println!("HOLD VERDICT ({kind:?}): SUSPECT -> partials never advanced during the hold");
    } else if growth > 512.0 {
        println!(
            "HOLD VERDICT ({kind:?}): UNGROUNDED GROWTH (+{growth:.0} MiB) -> add ~20s endpoint-flush threshold in record mode"
        );
    } else {
        println!(
            "HOLD VERDICT ({kind:?}): BOUNDED -> state growth benign over {hold_seconds}s, no endpoint-flush needed"
        );
    }
}

fn dictation_audio(source: &[f32], total_samples: usize) -> Vec<f32> {
    let speech_len = (SPEECH_SECONDS * SAMPLE_RATE as f64) as usize;
    let gap_len = (GAP_SECONDS * SAMPLE_RATE as f64) as usize;
    let mut out = Vec::with_capacity(total_samples);
    let mut cursor = 0_usize;
    while out.len() < total_samples {
        let take = speech_len.min(total_samples - out.len());
        for offset in 0..take {
            out.push(source[(cursor + offset) % source.len()]);
        }
        cursor = (cursor + take) % source.len();
        let silence = gap_len.min(total_samples - out.len());
        out.resize(out.len() + silence, 0.0);
    }
    out
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

fn nearest_resample_to_16k(samples: &[f32], from_rate: u32) -> Vec<f32> {
    if from_rate == SAMPLE_RATE as u32 {
        return samples.to_vec();
    }
    let step = from_rate as f64 / SAMPLE_RATE as f64;
    let out_len = ((samples.len() as f64 - 1.0) / step).floor() as usize;
    (0..out_len)
        .map(|index| samples[(index as f64 * step) as usize])
        .collect()
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
