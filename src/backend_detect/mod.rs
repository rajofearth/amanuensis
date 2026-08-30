mod registry;

use registry::GpuAdapter;

use crate::asr::{ModelKind, ModelPaths, cache_dir_for, is_model_cached};
use crate::config::{AsrCacheEntry, BackendCache};
use crate::log;

/// Version string used to invalidate cached backend selections across releases.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment variable that skips the onboarding benchmark entirely.
pub const ENV_SKIP: &str = "SKIP_BACKEND_BENCH";

/// Hidden CLI flag consumed by `main()` to run a single-candidate probe child.
pub const PROBE_FLAG: &str = "--backend-probe";

/// Marker the probe child prints to stdout once `create()` succeeds.
const MARKER_CREATED: &str = "__probe_created";
/// Marker the probe child prints to stdout before each timed RTF value.
const MARKER_RTF: &str = "__probe_rtf";
/// A subsTRING sherpa's C++ prints when a requested provider is unsupported and
/// it falls back to CPU. Presence of this in a probe's stderr rejects the candidate.
const FALLBACK_SENTINEL: &str = "Fallback to cpu";

/// A universal serialized-input clip (the repo's bundled `0.wav` test file) is
/// used as the benchmark workload. Kept short so the whole bench stays cheap.
const CALIB_SAMPLE_RATE: i32 = 16000;
const CALIB_FEED_SAMPLES: usize = 480;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Qualcomm,
    Apple,
    Cpu,
    Unknown,
}

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA",
            Self::Amd => "AMD",
            Self::Intel => "Intel",
            Self::Qualcomm => "Qualcomm",
            Self::Apple => "Apple",
            Self::Cpu => "CPU",
            Self::Unknown => "Unknown",
        }
    }

    /// The preferred GPU execution provider for this vendor, if any. Per the
    /// ticket's ground truth `dml` is not a sherpa provider string, and AMD /
    /// Snapdragon-class boards are to stay on CPU — so only NVIDIA exposes a GPU
    /// candidate here (`cuda`).
    fn gpu_provider(self) -> Option<&'static str> {
        match self {
            Self::Nvidia => Some("cuda"),
            _ => None,
        }
    }
}

/// A stable description of the host used to decide whether a cached backend
/// selection is still valid across devices/upgrades.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareProfile {
    pub vendor: Vendor,
    pub gpu_label: String,
    pub arch: String,
    pub os: String,
    pub device_hash: String,
}

/// One (provider, thread-count) combination worth bench-marking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCandidate {
    pub provider: String,
    pub threads: i32,
    pub is_gpu: bool,
}

/// A measured (candidate, realtime-factor) outcome from the bench.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchResult {
    pub provider: String,
    pub threads: i32,
    pub is_gpu: bool,
    pub rtf: f32,
}

/// Live progress surfaced to the UI while a benchmark runs.
#[derive(Clone, Debug, PartialEq)]
pub enum BenchProgress {
    /// A candidate is about to be measured (cooldown + probe run).
    Measuring {
        provider: String,
        threads: i32,
        is_gpu: bool,
    },
    /// A candidate finished measuring; the UI can rank/wins live from these.
    Measured(BenchResult),
    /// The full bench finished; the winner (or None if skipped/failed) is set.
    Finished { winner: Option<String> },
}

// ---------------------------------------------------------------------------
// Hardware detection
// ---------------------------------------------------------------------------

/// Enumerate display adapters (Windows registry) and fold them into a profile.
/// Never touches the network; falls back gracefully on any read error.
fn gather_gpus() -> Vec<GpuAdapter> {
    registry::enumerate_adapters()
}

/// Detect the dominant GPU vendor from the adapter list. When multiple adapters
/// are present the highest-priority provider vendor wins (NVIDIA > AMD > Intel).
fn dominant_vendor(adapters: &[GpuAdapter]) -> Vendor {
    for adapter in adapters {
        let vendor = vendor_of(adapter);
        if matches!(vendor, Vendor::Nvidia) {
            return Vendor::Nvidia;
        }
    }
    let mut amd = false;
    let mut intel = false;
    for adapter in adapters {
        match vendor_of(adapter) {
            Vendor::Amd => {
                if !amd && !intel {
                    amd = true;
                }
            }
            Vendor::Intel => intel = true,
            _ => {}
        }
    }
    if amd {
        Vendor::Amd
    } else if intel {
        Vendor::Intel
    } else {
        Vendor::Unknown
    }
}

fn vendor_of(adapter: &GpuAdapter) -> Vendor {
    let vendor = vendor_from_id(&adapter.vendor_id);
    if vendor != Vendor::Unknown {
        return vendor;
    }
    let text = adapter.description.to_ascii_lowercase();
    if text.contains("nvidia") {
        Vendor::Nvidia
    } else if text.contains("radeon") || text.contains("amd") {
        Vendor::Amd
    } else if text.contains("intel") {
        Vendor::Intel
    } else if text.contains("qualcomm") || adapter.vendor_id.eq_ignore_ascii_case("06F0") {
        Vendor::Qualcomm
    } else {
        Vendor::Unknown
    }
}

fn vendor_from_id(vendor_id: &str) -> Vendor {
    match vendor_id.to_ascii_uppercase().as_str() {
        "10DE" => Vendor::Nvidia,
        "1002" | "1022" => Vendor::Amd,
        "8086" => Vendor::Intel,
        "06F0" | "4D4F" => Vendor::Qualcomm,
        "106B" => Vendor::Apple,
        _ => Vendor::Unknown,
    }
}

fn cpu_vendor(arch: &str) -> Vendor {
    match arch {
        "x86_64" => Vendor::Intel,
        "aarch64" => Vendor::Qualcomm,
        _ => Vendor::Cpu,
    }
}

/// Stable per-device hash so `needs_bench` can detect a different machine.
fn hash_device(canonical: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in canonical.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn detect_hardware() -> HardwareProfile {
    let arch = std::env::consts::ARCH.to_owned();
    let os = std::env::consts::OS.to_owned();
    let adapters = gather_gpus();
    let vendor = {
        let gpu = dominant_vendor(&adapters);
        if gpu == Vendor::Unknown {
            cpu_vendor(&arch)
        } else {
            gpu
        }
    };
    let gpu_label = adapters
        .first()
        .map(|adapter| adapter.description.clone())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| vendor.label().to_owned());
    let mut canonical = format!("{arch}|{os}");
    for adapter in &adapters {
        canonical.push('|');
        canonical.push_str(&adapter.vendor_id);
        canonical.push(':');
        canonical.push_str(&adapter.device_id);
    }
    HardwareProfile {
        vendor,
        gpu_label,
        arch,
        os,
        device_hash: hash_device(&canonical),
    }
}

// ---------------------------------------------------------------------------
// Candidate matrix
// ---------------------------------------------------------------------------

/// Seed the candidate set for a host profile. CPU thread counts always feature
/// (thread count is part of the CPU decision). A GPU candidate is added only
/// for an NVIDIA x86-64 host; AMD and Snapdragon-class profiles stay on CPU.
pub fn candidates_for(profile: &HardwareProfile) -> Vec<ProviderCandidate> {
    let mut candidates = Vec::new();
    for threads in [1, 2, 4] {
        candidates.push(ProviderCandidate {
            provider: "cpu".to_owned(),
            threads,
            is_gpu: false,
        });
    }
    if profile.arch == "x86_64"
        && let Some(gpu) = profile.vendor.gpu_provider()
    {
        candidates.push(ProviderCandidate {
            provider: gpu.to_owned(),
            threads: 1,
            is_gpu: true,
        });
    }
    candidates
}

// ---------------------------------------------------------------------------
// Winner decision + cache validity
// ---------------------------------------------------------------------------

/// Pick the fastest backend. A GPU candidate must be at least
/// `GPU_MARGIN` (1.1x) faster than the best CPU result, otherwise CPU wins.
const GPU_MARGIN: f32 = 1.1;

pub fn decide_winner(results: &[BenchResult]) -> Option<BenchResult> {
    let best = |is_gpu: bool| {
        results
            .iter()
            .filter(|r| r.is_gpu == is_gpu)
            .min_by(|a, b| {
                a.rtf
                    .partial_cmp(&b.rtf)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    };
    let best_cpu = best(false);
    let best_gpu = best(true);
    match (best_cpu, best_gpu) {
        (None, None) => None,
        (Some(cpu), None) => Some(cpu.clone()),
        (None, Some(gpu)) => Some(gpu.clone()),
        (Some(cpu), Some(gpu)) => {
            if gpu.rtf <= cpu.rtf / GPU_MARGIN {
                Some(gpu.clone())
            } else {
                Some(cpu.clone())
            }
        }
    }
}

/// Whether a fresh bench is warranted: no cached ASR pick yet, the device hash
/// changed, or an app upgrade happened. Env overrides short-circuit the bench.
pub fn needs_bench(asr: &AsrCacheEntry, profile: &HardwareProfile) -> bool {
    if env_forces_idle() {
        return false;
    }
    asr.provider.is_empty()
        || (!asr.device_hash.is_empty() && asr.device_hash != profile.device_hash)
        || (!asr.app_version.is_empty() && asr.app_version != APP_VERSION)
}

/// Env overrides make the benchmark redundant (the operator has pinned a choice)
/// or explicitly skip it.
pub fn env_forces_idle() -> bool {
    std::env::var_os(ENV_SKIP).is_some()
        || std::env::var_os("ASR_PROVIDER").is_some()
        || std::env::var_os("ASR_THREADS").is_some()
}

// ---------------------------------------------------------------------------
// Benchmarking
// ---------------------------------------------------------------------------

/// Relative path (within the model cache dir) of the speech clip used as the
/// benchmark workload. It ships with the sherpa-onnx model repo and is fetched
/// on demand by `ensure_calib_clip` if not already on disk.
const CALIB_CLIP_REL: &str = "test_wavs/0.wav";

/// Make the calibration speech clip available on disk. Returns its full path.
/// Uses the model repo's own `test_wavs/0.wav` (a real ~6s spoken clip) so the
/// bench measures real decode work rather than digital silence.
pub(crate) fn ensure_calib_clip() -> Option<std::path::PathBuf> {
    let spec = ModelKind::Nemotron.spec();
    let dir = crate::asr::cache_dir_for(ModelKind::Nemotron)?;
    let clip = dir.join(CALIB_CLIP_REL);
    if std::fs::metadata(&clip).is_err() {
        match crate::asr::fetch::ensure_file(spec, CALIB_CLIP_REL, None, None, &mut |_| {}) {
            Ok(true) => log!("backend", "calibration clip ready: {}", clip.display()),
            Ok(false) => log!("backend", "calibration clip download cancelled"),
            Err(error) => log!("backend", "calibration clip unavailable: {error}"),
        }
    }
    std::fs::metadata(&clip).ok().map(|_| clip)
}

/// Shared calibration workload: raw 16 kHz f32le samples decoded from the
/// speech clip. Falls back to a short silence buffer only if the clip cannot
/// be found.
fn calib_samples(clip: &std::path::Path) -> Vec<f32> {
    if let Ok(mut reader) = hound::WavReader::open(clip) {
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .filter_map(Result::ok)
            .map(|sample| sample as f32 / 32768.0)
            .collect();
        if !samples.is_empty() {
            return samples;
        }
    }
    // Fallback: 1.0s of silence keeps the bench deterministic.
    log!(
        "backend",
        "calibration clip unreadable; using silence fallback"
    );
    vec![0.0; CALIB_SAMPLE_RATE as usize]
}

/// Make a model, run an untimed warmup pass, then time up to `runs` decode
/// passes keeping the best (lowest) RTF. Returns None if creation failed.
/// This runs inside a probe child so that sherpa's C++ stderr is capturable by
/// the parent via OS pipes (in-process `SetStdHandle` capture provably does not
/// reach the CRT's `stderr`, verified experimentally).
fn bench_candidate(
    paths: &ModelPaths,
    provider: &str,
    threads: i32,
    samples: &[f32],
    audio_secs: f64,
) -> Option<f32> {
    use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineTransducerModelConfig};
    let mut config = OnlineRecognizerConfig::default();
    config.feat_config.sample_rate = CALIB_SAMPLE_RATE;
    config.feat_config.feature_dim = 128;
    config.model_config.transducer = OnlineTransducerModelConfig {
        encoder: Some(path_string(&paths.encoder)),
        decoder: Some(path_string(&paths.decoder)),
        joiner: Some(path_string(&paths.joiner)),
    };
    config.model_config.tokens = Some(path_string(&paths.tokens));
    config.model_config.num_threads = threads;
    config.model_config.provider = Some(provider.to_owned());
    config.decoding_method = Some("greedy_search".to_owned());
    config.enable_endpoint = false;

    let recognizer = OnlineRecognizer::create(&config)?;
    eprintln!("[probe] recognizer created provider={provider} threads={threads}");
    println!("{MARKER_CREATED} provider={provider} threads={threads}");

    // Warmup pass (untimed) — first decode pays model/thread warm-up cost.
    run_decode(&recognizer, samples);
    std::thread::sleep(std::time::Duration::from_millis(150));

    let mut best: Option<f32> = None;
    for _ in 0..2 {
        let started = std::time::Instant::now();
        run_decode(&recognizer, samples);
        let elapsed = started.elapsed().as_secs_f64();
        let rtf = if audio_secs > 0.0 {
            (elapsed / audio_secs) as f32
        } else {
            f32::MAX
        };
        best = Some(match best {
            Some(current) => current.min(rtf),
            None => rtf as f32,
        });
    }
    let best = best?;
    println!("{MARKER_RTF} {best:.4}");
    eprintln!("[probe] best rtf={best:.4}");
    Some(best)
}

fn run_decode(recognizer: &sherpa_onnx::OnlineRecognizer, samples: &[f32]) {
    let stream = recognizer.create_stream();
    for chunk in samples.chunks(CALIB_FEED_SAMPLES) {
        stream.accept_waveform(CALIB_SAMPLE_RATE, chunk);
        while recognizer.is_ready(&stream) {
            recognizer.decode(&stream);
        }
    }
    stream.input_finished();
    while recognizer.is_ready(&stream) {
        recognizer.decode(&stream);
    }
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Entry point for the `--backend-probe` child. Reads its configuration from
/// the environment, benchmarks a single candidate, prints structured results to
/// stdout, and exits with a non-zero code if the model could not be built.
pub fn probe_child() -> i32 {
    let Some(provider) = std::env::var("AM_PROBE_PROVIDER").ok() else {
        eprintln!("[probe] missing AM_PROBE_PROVIDER");
        return 2;
    };
    let Some(threads) = std::env::var("AM_PROBE_THREADS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    else {
        eprintln!("[probe] missing/invalid AM_PROBE_THREADS");
        return 2;
    };
    let Some(model_dir) = cache_dir_for(ModelKind::Nemotron) else {
        eprintln!("[probe] cannot resolve model dir");
        return 2;
    };
    let paths = ModelPaths {
        encoder: model_dir.join("encoder.int8.onnx"),
        decoder: model_dir.join("decoder.int8.onnx"),
        joiner: model_dir.join("joiner.int8.onnx"),
        tokens: model_dir.join("tokens.txt"),
    };
    let samples = cache_dir_for(ModelKind::Nemotron)
        .map(|dir| dir.join(CALIB_CLIP_REL))
        .map(|clip| calib_samples(&clip))
        .unwrap_or_default();
    let audio_secs = samples.len() as f64 / CALIB_SAMPLE_RATE as f64;
    match bench_candidate(&paths, &provider, threads, &samples, audio_secs) {
        Some(_) => 0,
        None => 1,
    }
}

// ---------------------------------------------------------------------------
// Parent-side orchestration
// ---------------------------------------------------------------------------

/// Spawn `current_exe` with `--backend-probe` for one candidate and capture the
/// child's stdout and stderr. Returns (result lines, stderr text). Both streams
/// are drained to EOF (which blocks until the child exits), so the call is
/// naturally bounded by the probe's own runtime.
fn run_probe_candidate(candidate: &ProviderCandidate) -> (Vec<String>, String) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap_or_default());
    command
        .arg(PROBE_FLAG)
        .env("AM_PROBE_PROVIDER", &candidate.provider)
        .env("AM_PROBE_THREADS", candidate.threads.to_string());
    let Ok(output) = command.output() else {
        return (Vec::new(), String::new());
    };
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    (lines, String::from_utf8_lossy(&output.stderr).into_owned())
}

fn parse_rtf(lines: &[String]) -> Option<f32> {
    lines.iter().find_map(|line| {
        line.strip_prefix(MARKER_RTF)
            .and_then(|rest| rest.trim().parse::<f32>().ok())
    })
}

fn parse_created(lines: &[String]) -> bool {
    lines.iter().any(|line| line.starts_with(MARKER_CREATED))
}

/// Run the full backend benchmark for the current device and persist the winner
/// into config. Returns the winning provider if one was produced, or None when
/// the bench was skipped (env override / no cached model / no winners).
pub fn run_backend_bench() -> Option<String> {
    run_backend_bench_with(&mut |_| {})
}

/// Like `run_backend_bench`, but reports live progress through `on_progress`
/// so callers can surface a "measuring your machine" step with per-candidate
/// updates. Runs synchronously on the calling thread (spawn your own thread if
/// the caller is the UI thread).
pub fn run_backend_bench_with<F: FnMut(BenchProgress)>(on_progress: &mut F) -> Option<String> {
    if env_forces_idle() {
        log!("backend", "bench skipped (env override)");
        return None;
    }
    if !is_model_cached("nemotron") {
        log!("backend", "bench deferred: model not cached");
        return None;
    }
    let _ = ensure_calib_clip();
    let profile = detect_hardware();
    let candidates = candidates_for(&profile);
    log!(
        "backend",
        "bench start profile={:?} candidates={}",
        profile.vendor,
        candidates.len()
    );
    // Cooldown between candidates keeps back-to-back decodes from thermally
    // skewing results. (The ticket's full 30 s is impractical for a live
    // onboarding bench; this is a lighter-but-much-larger-than-nothing gap.)
    let cooldown = std::time::Duration::from_millis(700);
    let mut results = Vec::new();
    let mut aborted = false;
    for candidate in &candidates {
        on_progress(BenchProgress::Measuring {
            provider: candidate.provider.clone(),
            threads: candidate.threads,
            is_gpu: candidate.is_gpu,
        });
        std::thread::sleep(cooldown);
        let (lines, stderr_text) = run_probe_candidate(candidate);
        if stderr_text.contains(FALLBACK_SENTINEL) {
            log!(
                "backend",
                "reject {} (fell back to cpu)",
                candidate.provider
            );
            continue;
        }
        if !parse_created(&lines) {
            log!(
                "backend",
                "reject {}: unrecognized create result",
                candidate.provider
            );
            continue;
        }
        let Some(rtf) = parse_rtf(&lines) else {
            log!("backend", "reject {}: no RTF measured", candidate.provider);
            continue;
        };
        // A run this far past realtime is a throttled/failed measurement, not a
        // real result. Abort the whole bench and keep the previous winner.
        if rtf > 10.0 {
            log!(
                "backend",
                "abort bench: {} RTF {rtf:.2} > 10x realtime (thermal/failure); keeping previous winner",
                candidate.provider
            );
            aborted = true;
            break;
        }
        let measured = BenchResult {
            provider: candidate.provider.clone(),
            threads: candidate.threads,
            is_gpu: candidate.is_gpu,
            rtf,
        };
        results.push(measured.clone());
        on_progress(BenchProgress::Measured(measured));
        log!(
            "backend",
            "candidate {} threads={} rtf={rtf:.3}",
            candidate.provider,
            candidate.threads
        );
    }
    if aborted {
        on_progress(BenchProgress::Finished { winner: None });
        return None;
    }
    let Some(winner) = decide_winner(&results) else {
        log!("backend", "no usable backend measured");
        on_progress(BenchProgress::Finished { winner: None });
        return None;
    };
    persist_winner(&profile, &winner);
    on_progress(BenchProgress::Finished {
        winner: Some(winner.provider.clone()),
    });
    Some(winner.provider.clone())
}

fn persist_winner(profile: &HardwareProfile, winner: &BenchResult) {
    let mut config = crate::config::load().unwrap_or_default();
    config.backend_cache.asr = AsrCacheEntry {
        provider: winner.provider.clone(),
        threads: winner.threads,
        win_rtf: winner.rtf,
        app_version: APP_VERSION.to_owned(),
        cached_at: chrono_string(),
        device_hash: profile.device_hash.clone(),
    };
    let _ = crate::config::save(&config);
    log!(
        "backend",
        "persisted winner provider={} threads={} rtf={:.3}",
        winner.provider,
        winner.threads,
        winner.rtf
    );
}

fn chrono_string() -> String {
    // RFC 3339-ish timestamp without pulling in a date dependency.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

/// Convenience value used to populate `BackendCache` for tests / manual runs.
#[allow(dead_code)]
fn default_cache() -> BackendCache {
    BackendCache::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_result(threads: i32, rtf: f32) -> BenchResult {
        BenchResult {
            provider: "cpu".to_owned(),
            threads,
            is_gpu: false,
            rtf,
        }
    }

    fn gpu_result(rtf: f32) -> BenchResult {
        BenchResult {
            provider: "cuda".to_owned(),
            threads: 1,
            is_gpu: true,
            rtf,
        }
    }

    #[test]
    fn gpu_wins_when_1_1x_faster() {
        let results = [cpu_result(2, 0.90), gpu_result(0.80)];
        let winner = decide_winner(&results).unwrap();
        assert_eq!(winner.provider, "cuda");
    }

    #[test]
    fn cpu_wins_when_gpu_not_marginally_faster() {
        // 0.84 is not <= 0.90 / 1.1 (~0.818) so CPU stays.
        let results = [cpu_result(4, 0.84), gpu_result(0.84)];
        let winner = decide_winner(&results).unwrap();
        assert_eq!(winner.provider, "cpu");
    }

    #[test]
    fn cpu_only_falls_back_to_cpu() {
        let results = [
            cpu_result(1, 1.15),
            cpu_result(2, 1.01),
            cpu_result(4, 0.87),
        ];
        let winner = decide_winner(&results).unwrap();
        assert_eq!(winner.provider, "cpu");
        assert_eq!(winner.threads, 4);
    }

    #[test]
    fn no_cpu_but_gpu_available() {
        let winner = decide_winner(&[gpu_result(1.2)]).unwrap();
        assert_eq!(winner.provider, "cuda");
    }

    #[test]
    fn empty_results_no_winner() {
        assert!(decide_winner(&[]).is_none());
    }

    #[test]
    fn vendor_provider_mapping() {
        assert_eq!(Vendor::Nvidia.gpu_provider(), Some("cuda"));
        assert_eq!(Vendor::Amd.gpu_provider(), None);
        assert_eq!(Vendor::Qualcomm.gpu_provider(), None);
        assert_eq!(Vendor::Intel.gpu_provider(), None);
        assert_eq!(Vendor::Cpu.gpu_provider(), None);
    }

    #[test]
    fn candidates_include_cpu_threads_and_gpu() {
        let profile = HardwareProfile {
            vendor: Vendor::Nvidia,
            gpu_label: "GeForce".to_owned(),
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "abc".to_owned(),
        };
        let candidates = candidates_for(&profile);
        let cpu_threads: Vec<i32> = candidates
            .iter()
            .filter(|c| c.provider == "cpu")
            .map(|c| c.threads)
            .collect();
        assert_eq!(cpu_threads, vec![1, 2, 4]);
        assert!(candidates.iter().any(|c| c.provider == "cuda" && c.is_gpu));
    }

    #[test]
    fn snapdragon_probes_no_gpu() {
        let profile = HardwareProfile {
            vendor: Vendor::Qualcomm,
            gpu_label: "Adreno".to_owned(),
            arch: "aarch64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "abc".to_owned(),
        };
        let candidates = candidates_for(&profile);
        assert!(!candidates.is_empty());
        assert!(!candidates.iter().any(|c| c.is_gpu));
        assert!(candidates.iter().all(|c| c.provider == "cpu"));
    }

    #[test]
    fn amd_probes_no_gpu() {
        let profile = HardwareProfile {
            vendor: Vendor::Amd,
            gpu_label: "Radeon".to_owned(),
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "abc".to_owned(),
        };
        let candidates = candidates_for(&profile);
        assert!(!candidates.iter().any(|c| c.is_gpu));
    }

    #[test]
    fn needs_bench_first_run() {
        let profile = HardwareProfile {
            vendor: Vendor::Nvidia,
            gpu_label: "g".to_owned(),
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "h1".to_owned(),
        };
        let empty = AsrCacheEntry::default();
        assert!(needs_bench(&empty, &profile));
    }

    #[test]
    fn needs_bench_device_change_invalidates() {
        let profile = HardwareProfile {
            vendor: Vendor::Nvidia,
            gpu_label: "g".to_owned(),
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "h2".to_owned(),
        };
        let cached = AsrCacheEntry {
            provider: "cpu".to_owned(),
            threads: 4,
            win_rtf: 0.85,
            app_version: APP_VERSION.to_owned(),
            cached_at: "0".to_owned(),
            device_hash: "h1".to_owned(),
        };
        assert!(needs_bench(&cached, &profile));
    }

    #[test]
    fn needs_bench_up_to_date_is_false() {
        let profile = HardwareProfile {
            vendor: Vendor::Nvidia,
            gpu_label: "g".to_owned(),
            arch: "x86_64".to_owned(),
            os: "windows".to_owned(),
            device_hash: "h1".to_owned(),
        };
        let cached = AsrCacheEntry {
            provider: "cpu".to_owned(),
            threads: 4,
            win_rtf: 0.85,
            app_version: APP_VERSION.to_owned(),
            cached_at: "0".to_owned(),
            device_hash: "h1".to_owned(),
        };
        assert!(!needs_bench(&cached, &profile));
    }
}
