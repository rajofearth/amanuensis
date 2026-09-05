use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use crate::config;

use super::model::{ModelPaths, ModelSpec, REPO_OWNER};

const HF_BASE: &str = "https://huggingface.co";

fn model_files(spec: &ModelSpec) -> &'static [&'static str] {
    match spec.id {
        "moonshine" => &MOONSHINE_FILES,
        _ => &NEMOTRON_FILES,
    }
}

pub(crate) static NEMOTRON_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

pub(crate) static MOONSHINE_FILES: [&str; 5] = [
    "preprocess.onnx",
    "encode.int8.onnx",
    "uncached_decode.int8.onnx",
    "cached_decode.int8.onnx",
    "tokens.txt",
];

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub bytes_per_sec: Option<f64>,
}

const SPEED_EMA_ALPHA: f64 = 0.25;

#[derive(Clone, Copy)]
struct SpeedSample {
    at: Instant,
    done: u64,
}

pub struct SpeedTracker {
    last: Option<SpeedSample>,
    ema_rate: Option<f64>,
    min_interval: std::time::Duration,
}

impl Default for SpeedTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeedTracker {
    pub fn new() -> Self {
        Self {
            last: None,
            ema_rate: None,
            min_interval: std::time::Duration::from_millis(100),
        }
    }

    pub fn push(&mut self, done: u64, at: Instant) -> Option<f64> {
        let Some(sample) = self.last else {
            self.last = Some(SpeedSample { at, done });
            return self.ema_rate;
        };
        let elapsed = at.duration_since(sample.at);
        if elapsed >= self.min_interval {
            let rate = done.saturating_sub(sample.done) as f64 / elapsed.as_secs_f64();
            self.ema_rate = Some(match self.ema_rate {
                Some(previous) => previous + SPEED_EMA_ALPHA * (rate - previous),
                None => rate,
            });
            self.last = Some(SpeedSample { at, done });
        }
        self.ema_rate
    }
}

pub(crate) struct Aggregate {
    total: u64,
    completed: u64,
    speed: SpeedTracker,
}

impl Aggregate {
    pub(crate) fn new(total: u64) -> Self {
        Self {
            total,
            completed: 0,
            speed: SpeedTracker::new(),
        }
    }

    pub(crate) fn finish_file(&mut self, bytes: u64) {
        self.completed += bytes;
    }

    pub(crate) fn current(&mut self, file: &str, streamed: u64) -> DownloadProgress {
        let done = self.completed + streamed;
        let bytes_per_sec = self.speed.push(done, Instant::now());
        DownloadProgress {
            file: file.to_owned(),
            done,
            total: self.total,
            bytes_per_sec,
        }
    }
}

fn models_root() -> Result<PathBuf, String> {
    let current = config::config_dir()?.join("models");
    if current.exists() {
        return Ok(current);
    }
    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA not set")?;
    let legacy = PathBuf::from(appdata).join("raycast-dictation/models");
    Ok(if legacy.exists() { legacy } else { current })
}

pub fn cached_model_dir(spec: &ModelSpec) -> PathBuf {
    models_root()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(spec.id)
}

fn file_url(spec: &ModelSpec, file: &str) -> String {
    format!("{HF_BASE}/{REPO_OWNER}/{}/resolve/main/{file}", spec.repo)
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .build()
    });
    &AGENT
}

pub(crate) fn head_content_length(url: &str) -> Option<u64> {
    let response = http_agent().head(url).call().ok()?;
    response.header("Content-Length")?.parse::<u64>().ok()
}

pub fn total_size(spec: &ModelSpec) -> Option<u64> {
    model_files(spec)
        .iter()
        .map(|file| head_content_length(&file_url(spec, file)))
        .collect::<Option<Vec<_>>>()
        .map(|sizes| sizes.iter().sum())
}

fn dir_complete(dir: &Path, files: &[&str]) -> bool {
    files.iter().all(|file| {
        std::fs::metadata(dir.join(file))
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false)
    })
}

pub fn is_spec_cached(spec: &ModelSpec) -> bool {
    dir_complete(&cached_model_dir(spec), model_files(spec))
}

pub(crate) fn mb_summary(done: u64, total: u64) -> String {
    if total == 0 {
        return "? MB".to_owned();
    }
    format!(
        "{}% · {}/{} MB",
        ((done * 100) / total).min(100),
        (done as f64 / 1e6).round() as u64,
        (total as f64 / 1e6).round() as u64
    )
}

pub fn speed_summary(bytes_per_sec: f64) -> String {
    let kb = (bytes_per_sec / 1e3).round();
    if kb < 1000.0 {
        format!("{} KB/s", kb as u64)
    } else {
        format!("{:.1} MB/s", bytes_per_sec / 1e6)
    }
}

pub fn progress_text(display_name: &str, progress: &DownloadProgress) -> String {
    format!(
        "Downloading {} — {} ({})",
        display_name,
        mb_summary(progress.done, progress.total),
        progress.file
    )
}

pub fn progress_status(
    display_name: &str,
    progress: &DownloadProgress,
    eta: Option<&str>,
) -> String {
    let speed_part = progress
        .bytes_per_sec
        .map(|bytes_per_sec| format!(" · {}", speed_summary(bytes_per_sec)))
        .unwrap_or_default();
    let eta_part = eta.map(|eta| format!(" · {eta}")).unwrap_or_default();
    format!(
        "Downloading {} — {}{speed_part}{eta_part} ({})",
        display_name,
        mb_summary(progress.done, progress.total),
        progress.file
    )
}

pub fn progress_summary(done: u64, total: u64, speed: Option<&str>, eta: Option<&str>) -> String {
    let speed_part = speed.map(|speed| format!(" · {speed}")).unwrap_or_default();
    let eta_part = eta.map(|eta| format!(" · {eta}")).unwrap_or_default();
    format!("{}{}{}", mb_summary(done, total), speed_part, eta_part)
}

pub fn path_is_dir(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug)]
pub struct EtaSample {
    pub at: Instant,
    pub done: u64,
}

pub struct EtaTracker {
    samples: std::collections::VecDeque<EtaSample>,
    min_interval: std::time::Duration,
    capacity: usize,
}

impl Default for EtaTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl EtaTracker {
    pub fn new() -> Self {
        Self {
            samples: std::collections::VecDeque::new(),
            min_interval: std::time::Duration::from_millis(250),
            capacity: 32,
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn push(&mut self, done: u64, at: Instant) {
        if let Some(last) = self.samples.back()
            && at.duration_since(last.at) < self.min_interval
        {
            return;
        }
        self.samples.push_back(EtaSample { at, done });
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    pub fn estimate(&self, done: u64, total: u64) -> Option<String> {
        let snapshot: Vec<EtaSample> = self.samples.iter().copied().collect();
        eta_left(&snapshot, done, total)
    }
}

pub fn eta_left(samples: &[EtaSample], done: u64, total: u64) -> Option<String> {
    let last = *samples.last()?;
    let window: Vec<EtaSample> = samples
        .iter()
        .copied()
        .filter(|sample| last.at.duration_since(sample.at).as_secs_f64() <= 10.0)
        .collect();
    let first = *window.first()?;
    let span = last.at.duration_since(first.at).as_secs_f64();
    if window.len() < 2 || span < 2.0 {
        return None;
    }
    let delta = last.done.saturating_sub(first.done);
    if delta == 0 {
        return None;
    }
    let rate = delta as f64 / span;
    let remaining = total.saturating_sub(done) as f64 / rate;
    Some(format_seconds(remaining))
}

fn format_seconds(seconds: f64) -> String {
    let whole = seconds.round() as u64;
    if whole < 60 {
        format!("~{whole}s left")
    } else {
        format!("~{}m {}s left", whole / 60, whole % 60)
    }
}

pub fn generation_is_current(message_generation: u64, current_generation: u64) -> bool {
    message_generation == current_generation
}

/// Stream `url` to `final_path` with resume (`.part` + `Range`), byte-exact
/// verification when `expected_size` is known, and per-write `on_chunk(done)`
/// callbacks. `Ok(true)` = ready, `Ok(false)` = cancelled. Shared by model
/// files (`ensure_file`) and s1 assets.
pub(crate) fn download_url_to(
    url: &str,
    file_label: &str,
    final_path: &Path,
    expected_size: Option<u64>,
    cancel: Option<&AtomicBool>,
    on_chunk: &mut dyn FnMut(u64),
) -> Result<bool, String> {
    if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Ok(false);
    }
    let expected = match expected_size {
        Some(size) => Some(size),
        None => head_content_length(url),
    };
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    if let Ok(meta) = std::fs::metadata(final_path)
        && meta.is_file()
    {
        let accepted = match expected {
            Some(size) => meta.len() == size,
            None => meta.len() > 0,
        };
        if accepted {
            return Ok(true);
        }
    }
    let part_path = PathBuf::from(format!("{}.part", final_path.display()));
    let mut open_len = 0_u64;
    if let Ok(meta) = std::fs::metadata(&part_path) {
        open_len = meta.len();
    }
    let resumed = if open_len > 0 {
        match http_agent()
            .get(url)
            .set("Range", &format!("bytes={open_len}-"))
            .call()
        {
            Ok(response) if response.status() == 206 => Some((response, open_len)),
            _ => {
                let _ = std::fs::remove_file(&part_path);
                None
            }
        }
    } else {
        None
    };
    let (response, start_len) = match resumed {
        Some(pair) => pair,
        None => {
            let response = http_agent()
                .get(url)
                .call()
                .map_err(|error| error.to_string())?;
            (response, 0)
        }
    };
    let mut out = if start_len > 0 {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .map_err(|error| format!("opening {}: {error}", part_path.display()))?
    } else {
        std::fs::File::create(&part_path)
            .map_err(|error| format!("creating {}: {error}", part_path.display()))?
    };
    let mut reader = response.into_reader();
    let mut buffer = [0_u8; 65536];
    let mut done = start_len;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                    return Ok(false);
                }
                std::io::Write::write_all(&mut out, &buffer[..n])
                    .map_err(|error| format!("writing {}: {error}", part_path.display()))?;
                done += n as u64;
                on_chunk(done);
            }
            Err(error) => return Err(format!("streaming {file_label}: {error}")),
        }
    }
    std::io::Write::flush(&mut out).map_err(|error| format!("flushing {file_label}: {error}"))?;
    drop(out);
    if let Some(expected) = expected
        && done != expected
    {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!(
            "{file_label}: downloaded {done} bytes, expected {expected}"
        ));
    }
    std::fs::rename(&part_path, final_path)
        .map_err(|error| format!("promoting {file_label}: {error}"))?;
    Ok(true)
}

pub fn ensure_file(
    spec: &ModelSpec,
    file: &str,
    expected_size: Option<u64>,
    cancel: Option<&AtomicBool>,
    on_chunk: &mut dyn FnMut(u64),
) -> Result<bool, String> {
    let url = file_url(spec, file);
    let final_path = cached_model_dir(spec).join(file);
    download_url_to(&url, file, &final_path, expected_size, cancel, on_chunk)
}

pub fn ensure_model(
    spec: &ModelSpec,
    on_progress: &mut dyn FnMut(DownloadProgress),
    cancel: Option<&AtomicBool>,
) -> Result<Option<ModelPaths>, String> {
    if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Ok(None);
    }
    let dir = cached_model_dir(spec);
    std::fs::create_dir_all(&dir).map_err(|error| format!("creating model dir: {error}"))?;
    let files = model_files(spec);
    let known_sizes: Vec<Option<u64>> = files
        .iter()
        .map(|file| head_content_length(&file_url(spec, file)))
        .collect();
    let total: u64 = known_sizes
        .iter()
        .fold(0, |sum, size| sum + size.unwrap_or(0));
    let mut aggregate = Aggregate::new(total);
    let download_started = Instant::now();
    for (index, file) in files.iter().enumerate() {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Ok(None);
        }
        let expected = known_sizes[index];
        let file_name = (*file).to_owned();
        let completed = ensure_file(spec, file, expected, cancel, &mut |streamed| {
            on_progress(aggregate.current(&file_name, streamed));
        })?;
        if !completed {
            return Ok(None);
        }
        let finished = expected.unwrap_or_else(|| {
            std::fs::metadata(dir.join(file))
                .map(|meta| meta.len())
                .unwrap_or(0)
        });
        aggregate.finish_file(finished);
    }
    crate::telemetry::model_download(crate::telemetry::ModelDownload {
        model: spec.id.to_owned(),
        bytes: total,
        seconds: download_started.elapsed().as_secs_f64(),
        source: "huggingface".into(),
    });
    let paths = match spec.id {
        "moonshine" => ModelPaths::Moonshine {
            preprocessor: dir.join(MOONSHINE_FILES[0]),
            encoder: dir.join(MOONSHINE_FILES[1]),
            uncached_decoder: dir.join(MOONSHINE_FILES[2]),
            cached_decoder: dir.join(MOONSHINE_FILES[3]),
            tokens: dir.join(MOONSHINE_FILES[4]),
        },
        _ => ModelPaths::Nemotron {
            encoder: dir.join(NEMOTRON_FILES[0]),
            decoder: dir.join(NEMOTRON_FILES[1]),
            joiner: dir.join(NEMOTRON_FILES[2]),
            tokens: dir.join(NEMOTRON_FILES[3]),
        },
    };
    Ok(Some(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "amanuensis-fetch-test-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn file_url_has_expected_shape() {
        let spec = &super::super::model::REGISTRY[0];
        assert_eq!(
            file_url(spec, "encoder.int8.onnx"),
            format!(
                "https://huggingface.co/{REPO_OWNER}/{}/resolve/main/encoder.int8.onnx",
                spec.repo
            )
        );
    }

    #[test]
    fn aggregate_math_skip_adds_full_partial_adds_streamed() {
        let mut aggregate = Aggregate::new(700_000_000);
        aggregate.finish_file(462_000_000);
        let mid = aggregate.current("decoder.int8.onnx", 30_000_000);
        assert_eq!(mid.done, 492_000_000);
        assert_eq!(mid.total, 700_000_000);
        aggregate.finish_file(238_000_000);
        let next = aggregate.current("joiner.int8.onnx", 0);
        assert_eq!(next.done, 700_000_000);
    }

    #[test]
    fn mb_summary_handles_unknown_total_and_rounding() {
        assert_eq!(mb_summary(238_000_000, 0), "? MB");
        assert_eq!(mb_summary(238_000_000, 700_000_000), "34% · 238/700 MB");
        assert_eq!(mb_summary(700_500_000, 700_000_000), "100% · 701/700 MB");
        assert_eq!(mb_summary(0, 700_000_000), "0% · 0/700 MB");
    }

    #[test]
    fn speed_summary_formats_kb_below_one_mb_and_mb_above() {
        assert_eq!(speed_summary(0.0), "0 KB/s");
        assert_eq!(speed_summary(512_000.0), "512 KB/s");
        assert_eq!(speed_summary(999_400.0), "999 KB/s");
        assert_eq!(speed_summary(999_500.0), "1.0 MB/s");
        assert_eq!(speed_summary(2_400_000.0), "2.4 MB/s");
        assert_eq!(speed_summary(26_500_000.0), "26.5 MB/s");
    }

    #[test]
    fn speed_tracker_emas_rate_clamps_rapid_samples_and_hides_first() {
        let t0 = Instant::now();
        let mut tracker = SpeedTracker::new();
        assert_eq!(tracker.push(0, t0), None);
        assert_eq!(
            tracker.push(50_000_000, t0 + std::time::Duration::from_millis(20)),
            None
        );
        let first = tracker.push(1_000_000, t0 + std::time::Duration::from_secs(1));
        assert_eq!(first, Some(1_000_000.0));
        assert_eq!(
            tracker.push(1_400_000, t0 + std::time::Duration::from_millis(1_050)),
            Some(1_000_000.0)
        );
        let second = tracker.push(3_000_000, t0 + std::time::Duration::from_secs(2));
        assert_eq!(second, Some(1_250_000.0));
    }

    #[test]
    fn aggregate_payload_starts_with_unknown_speed() {
        let mut aggregate = Aggregate::new(700_000_000);
        assert_eq!(
            aggregate.current("encoder.int8.onnx", 10_000).bytes_per_sec,
            None
        );
        assert_eq!(
            aggregate.current("encoder.int8.onnx", 20_000).bytes_per_sec,
            None
        );
    }

    #[test]
    fn dir_complete_rejects_truncated_files() {
        let dir = temp_dir("complete");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), b"hello").unwrap();
        std::fs::write(dir.join("b.bin"), b"world").unwrap();
        assert!(dir_complete(&dir, &["a.bin", "b.bin"]));
        std::fs::write(dir.join("b.bin"), b"").unwrap();
        assert!(!dir_complete(&dir, &["a.bin", "b.bin"]));
        assert!(!dir_complete(&dir, &["a.bin", "missing.bin"]));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn sample(seconds_ago: f64, done: u64) -> EtaSample {
        EtaSample {
            at: Instant::now() - std::time::Duration::from_secs_f64(seconds_ago),
            done,
        }
    }

    #[test]
    fn eta_qualifies_after_two_second_window() {
        let samples = [
            sample(6.0, 100_000_000),
            sample(3.0, 200_000_000),
            sample(0.0, 300_000_000),
        ];
        assert_eq!(
            eta_left(&samples, 300_000_000, 600_000_000),
            Some("~9s left".to_owned())
        );
    }

    #[test]
    fn eta_none_until_window_spans_two_seconds() {
        let samples = [sample(1.5, 100), sample(0.0, 200)];
        assert_eq!(eta_left(&samples, 200, 600), None);
        assert_eq!(eta_left(&[sample(0.0, 100)], 100, 600), None);
        assert_eq!(eta_left(&[], 0, 600), None);
    }

    #[test]
    fn eta_evicts_stale_samples() {
        let samples = [sample(60.0, 10_000_000), sample(0.5, 11_000_000)];
        assert_eq!(eta_left(&samples, 11_000_000, 700_000_000), None);
    }

    #[test]
    fn eta_formats_minutes_and_seconds() {
        let samples = [sample(4.0, 0), sample(0.0, 4_000_000)];
        let eta = eta_left(&samples, 4_000_000, 1_000_000_000).unwrap();
        assert_eq!(eta, "~16m 36s left");
        let samples = [sample(4.0, 0), sample(0.0, 400_000_000)];
        let eta = eta_left(&samples, 400_000_000, 1_000_000_000).unwrap();
        assert_eq!(eta, "~6s left");
    }

    #[test]
    fn eta_none_when_no_progress_in_window() {
        let samples = [sample(6.0, 500), sample(0.0, 500)];
        assert_eq!(eta_left(&samples, 500, 700_000_000), None);
    }

    #[test]
    fn generation_guard_matches_only_current() {
        assert!(generation_is_current(7, 7));
        assert!(!generation_is_current(6, 7));
    }

    #[test]
    fn eta_tracker_throttles_progress_flood_so_window_qualifies() {
        let t0 = Instant::now();
        let mut tracker = EtaTracker::new();
        tracker.push(0, t0);
        for step in 1..=200_u64 {
            tracker.push(step * 100_000, t0 + std::time::Duration::from_millis(step));
        }
        assert_eq!(tracker.samples.len(), 1);
        assert_eq!(tracker.estimate(200 * 100_000, 700_000_000), None);
        tracker.push(300_000_000, t0 + std::time::Duration::from_secs(3));
        let estimate = tracker
            .estimate(300_000_000, 700_000_000)
            .expect("window spans 3s after throttle");
        assert!(estimate.ends_with("left"));
    }

    #[test]
    fn eta_tracker_clear_resets_samples() {
        let mut tracker = EtaTracker::new();
        tracker.push(100, Instant::now());
        assert!(!tracker.samples.is_empty());
        tracker.clear();
        assert!(tracker.samples.is_empty());
        assert_eq!(tracker.estimate(100, 600), None);
    }

    #[test]
    fn path_is_dir_distinguishes_dir_file_and_missing() {
        let dir = temp_dir("isdir");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(path_is_dir(&dir));
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(!path_is_dir(&file));
        assert!(!path_is_dir(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
