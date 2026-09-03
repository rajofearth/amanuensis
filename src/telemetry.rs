use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::Serialize;
use serde_json::{Value, json};

use crate::backend_detect::detect_hardware;
use crate::log;

static CONSENT: AtomicBool = AtomicBool::new(false);

pub fn set_consent(enabled: bool) {
    CONSENT.store(enabled, Ordering::Relaxed);
}

pub fn consent() -> bool {
    CONSENT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn telemetry_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AM_TELEMETRY_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("amanuensis")
        .join("logs")
        .join("telemetry")
}

fn logs_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AM_TELEMETRY_DIR") {
        let p = PathBuf::from(dir);
        if let Some(parent) = p.parent().filter(|q| !q.as_os_str().is_empty()) {
            return parent.to_path_buf();
        }
        return p;
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("amanuensis")
        .join("logs")
}

fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Convert to YYYY-MM-DD using a simple day calculation.
    let days = secs / 86400;
    let mut y = 1970u64;
    let mut remaining = days;
    loop {
        let leap = is_leap(y);
        let year_days = if leap { 366 } else { 365 };
        if remaining < year_days {
            break;
        }
        remaining -= year_days;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days: &[u32] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u64;
    let mut rem = remaining;
    for &md in month_days {
        if rem < md as u64 {
            break;
        }
        rem -= md as u64;
        m += 1;
    }
    format!("{y:04}-{m:02}-{:02}", rem + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn utc_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

fn log_event(_name: &str, value: &Value) {
    if !CONSENT.load(Ordering::Relaxed) {
        return;
    }
    write_event(value);
}

fn write_event(value: &Value) {
    let dir = telemetry_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        log!("telemetry", "failed to create telemetry dir: {e}");
        return;
    }
    let date = now_utc();
    let path = dir.join(format!("{date}.jsonl"));
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()) {
                log!("telemetry", "write error: {e}");
            }
            let _ = file.flush();
        }
        Err(e) => {
            log!("telemetry", "failed to open {}: {e}", path.display());
        }
    }
}

#[allow(dead_code)]
fn iso_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    let date = {
        let mut y = 1970u64;
        let mut remaining = days;
        loop {
            let leap = is_leap(y);
            let year_days = if leap { 366 } else { 365 };
            if remaining < year_days {
                break;
            }
            remaining -= year_days;
            y += 1;
        }
        let leap = is_leap(y);
        let month_days: &[u32] = if leap {
            &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut m2 = 1u64;
        let mut rem = remaining;
        for &md in month_days {
            if rem < md as u64 {
                break;
            }
            rem -= md as u64;
            m2 += 1;
        }
        format!("{y:04}-{m2:02}-{:02}", rem + 1)
    };
    format!("{date}T{h:02}:{m:02}:{s:02}Z")
}

// ---------------------------------------------------------------------------
// Windows registry helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win_reg {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegOpenKeyExW, RegQueryValueExW,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(crate) fn query_string(key: HKEY, value_name: &str) -> Option<String> {
        unsafe {
            let name = wide(value_name);
            let mut value_type: u32 = 0;
            let mut size: u32 = 0;
            let status = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut value_type,
                std::ptr::null_mut(),
                &mut size,
            );
            if status != 0 || (value_type != REG_SZ && value_type != 0) || size < 2 {
                return None;
            }
            let mut buffer = vec![0u8; size as usize];
            let mut filled = size;
            if RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut value_type,
                buffer.as_mut_ptr(),
                &mut filled,
            ) != 0
            {
                return None;
            }
            let utf16_len = (filled as usize) / 2;
            let slice = std::slice::from_raw_parts(buffer.as_ptr() as *const u16, utf16_len);
            let end = slice.iter().position(|&u| u == 0).unwrap_or(slice.len());
            let text = String::from_utf16_lossy(&slice[..end]);
            Some(text.trim().to_owned())
        }
    }

    pub(crate) fn open_key(path: &str) -> Option<HKEY> {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            let wide_path = wide(path);
            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                wide_path.as_ptr(),
                0,
                KEY_READ,
                &mut key,
            ) == 0
            {
                Some(key)
            } else {
                None
            }
        }
    }
}

fn os_info() -> (String, String, String) {
    // Returns (os_name, os_version, os_build)
    #[cfg(windows)]
    {
        const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        if let Some(key) = win_reg::open_key(KEY) {
            let name =
                win_reg::query_string(key, "ProductName").unwrap_or_else(|| "Windows".to_owned());
            let version = win_reg::query_string(key, "DisplayVersion").unwrap_or_default();
            let build = win_reg::query_string(key, "CurrentBuild").unwrap_or_default();
            unsafe {
                windows_sys::Win32::System::Registry::RegCloseKey(key);
            }
            return (name, version, build);
        }
        ("Windows".to_owned(), String::new(), String::new())
    }
    #[cfg(not(windows))]
    {
        (
            std::env::consts::OS.to_owned(),
            String::new(),
            String::new(),
        )
    }
}

// ---------------------------------------------------------------------------
// Model cache info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    bytes: u64,
    cached: bool,
}

fn gather_model_info() -> Vec<ModelInfo> {
    let mut models = Vec::new();
    if let Some(base) = std::env::var_os("APPDATA").map(PathBuf::from) {
        let cache_root = base.join("amanuensis").join("models");
        for name in &["nemotron", "moonshine"] {
            let dir = cache_root.join(name);
            let cached = dir.exists();
            let bytes = if cached { dir_size(&dir) } else { 0 };
            models.push(ModelInfo {
                id: name.to_string(),
                bytes,
                cached,
            });
        }
    }
    models
}

fn dir_size(path: &PathBuf) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                total += fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            } else if p.is_dir() {
                total += dir_size(&p);
            }
        }
    }
    total
}

fn bytes_to_gb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn app_start() {
    log_event(
        "app_start",
        &json!({
            "event": "app_start",
            "ts": utc_timestamp(),
            "app_version": env!("CARGO_PKG_VERSION"),
            "build": env!("CARGO_PKG_VERSION"),
            "arch": std::env::consts::ARCH,
        }),
    );
}

pub fn device_profile() {
    let hw = detect_hardware();
    let (os_name, os_version, os_build) = os_info();

    let sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_memory(sysinfo::MemoryRefreshKind::everything()),
    );
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_owned())
        .unwrap_or_default();
    let cpu_threads = sys.cpus().len() as u32;
    let cpu_physical_cores =
        sysinfo::System::physical_core_count().unwrap_or(cpu_threads as usize) as u32;
    let ram_total_gb = bytes_to_gb(sys.total_memory());
    let ram_available_gb = bytes_to_gb(sys.available_memory());

    let models = gather_model_info();

    log_event(
        "device_profile",
        &json!({
            "event": "device_profile",
            "os_name": os_name,
            "os_version": os_version,
            "os_build": os_build,
            "arch": hw.arch,
            "cpu_brand": cpu_brand,
            "cpu_threads": cpu_threads,
            "cpu_physical_cores": cpu_physical_cores,
            "ram_total_gb": ram_total_gb,
            "ram_available_gb": ram_available_gb,
            "power_state": "unknown",
            "gpu_vendor": hw.vendor.label(),
            "gpu_name": hw.gpu_label,
            "gpu_vram_mb": 0,
            "gpu_driver": "unknown",
            "models": models,
        }),
    );
}

pub struct BackendSelected {
    pub kind: String,
    pub provider: String,
    pub backend: String,
    pub device: String,
    pub threads: i32,
    pub decided_by: String,
    pub bench_ref: Option<String>,
}

pub fn backend_selected(event: BackendSelected) {
    log_event(
        "backend_selected",
        &json!({
            "event": "backend_selected",
            "kind": event.kind,
            "provider": event.provider,
            "backend": event.backend,
            "device": event.device,
            "threads": event.threads,
            "decided_by": event.decided_by,
            "bench_ref": event.bench_ref,
        }),
    );
}

pub struct BackendBench {
    pub candidate: String,
    pub provider: String,
    pub device: String,
    pub rtf: f32,
    pub wall_ms: u64,
    pub margin: Option<f32>,
    pub accepted: bool,
}

pub fn backend_bench(event: BackendBench) {
    log_event(
        "backend_bench",
        &json!({
            "event": "backend_bench",
            "candidate": event.candidate,
            "provider": event.provider,
            "device": event.device,
            "rtf": event.rtf,
            "wall_ms": event.wall_ms,
            "margin": event.margin,
            "accepted": event.accepted,
            "ts": utc_timestamp(),
        }),
    );
}

pub struct SessionEnd {
    pub mode: String,
    pub audio_secs: f32,
    pub decode_wall_ms: u64,
    pub rtf: f32,
    pub finalize_ms: u64,
    pub partials_advanced: u32,
    pub peak_rss_mb: f64,
}

pub fn session_end(event: SessionEnd) {
    log_event(
        "session_end",
        &json!({
            "event": "session_end",
            "mode": event.mode,
            "audio_secs": event.audio_secs,
            "decode_wall_ms": event.decode_wall_ms,
            "rtf": event.rtf,
            "finalize_ms": event.finalize_ms,
            "partials_advanced": event.partials_advanced,
            "peak_rss_mb": event.peak_rss_mb,
        }),
    );
}

pub struct RewriteEnd {
    pub backend: String,
    pub device: String,
    pub wall_ms: u64,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub ok: bool,
}

pub fn rewrite_end(event: RewriteEnd) {
    log_event(
        "rewrite_end",
        &json!({
            "event": "rewrite_end",
            "backend": event.backend,
            "device": event.device,
            "wall_ms": event.wall_ms,
            "in_tokens": event.in_tokens,
            "out_tokens": event.out_tokens,
            "ok": event.ok,
        }),
    );
}

pub struct ModelDownload {
    pub model: String,
    pub bytes: u64,
    pub seconds: f64,
    pub source: String,
}

pub fn model_download(event: ModelDownload) {
    log_event(
        "model_download",
        &json!({
            "event": "model_download",
            "model": event.model,
            "bytes": event.bytes,
            "seconds": event.seconds,
            "source": event.source,
        }),
    );
}

pub struct ErrorEvent {
    pub kind: String,
    pub backend: String,
    pub message: String,
    pub stderr_tail: String,
}

pub fn error(event: ErrorEvent) {
    log_event(
        "error",
        &json!({
            "event": "error",
            "kind": event.kind,
            "backend": event.backend,
            "message": event.message,
            "stderr_tail": event.stderr_tail,
        }),
    );
}

pub struct Fallback {
    pub kind: String,
    pub provider: String,
    pub reason: String,
    pub from: String,
    pub to: String,
}

pub fn fallback(event: Fallback) {
    log_event(
        "fallback",
        &json!({
            "event": "fallback",
            "kind": event.kind,
            "provider": event.provider,
            "reason": event.reason,
            "from": event.from,
            "to": event.to,
        }),
    );
}

pub fn consent_event(state: bool) {
    write_event(&json!({
        "event": "consent",
        "state": state,
        "ts": utc_timestamp(),
    }));
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

fn device_profile_json() -> Value {
    let hw = detect_hardware();
    let (os_name, os_version, os_build) = os_info();
    let sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_memory(sysinfo::MemoryRefreshKind::everything()),
    );
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_owned())
        .unwrap_or_default();
    let cpu_threads = sys.cpus().len() as u32;
    let cpu_physical_cores =
        sysinfo::System::physical_core_count().unwrap_or(cpu_threads as usize) as u32;
    let ram_total_gb = bytes_to_gb(sys.total_memory());
    let ram_available_gb = bytes_to_gb(sys.available_memory());
    let models = gather_model_info();

    json!({
        "os_name": os_name,
        "os_version": os_version,
        "os_build": os_build,
        "arch": hw.arch,
        "cpu_brand": cpu_brand,
        "cpu_threads": cpu_threads,
        "cpu_physical_cores": cpu_physical_cores,
        "ram_total_gb": ram_total_gb,
        "ram_available_gb": ram_available_gb,
        "power_state": "unknown",
        "gpu_vendor": hw.vendor.label(),
        "gpu_name": hw.gpu_label,
        "gpu_vram_mb": 0,
        "gpu_driver": "unknown",
        "models": models,
    })
}

fn config_summary_json() -> Value {
    let config = crate::config::load().unwrap_or_default();
    let provider = {
        let hw = detect_hardware();
        hw.vendor.label().to_owned()
    };
    json!({
        "model": config.record_model,
        "threads": config.backend_cache.asr.threads,
        "provider": provider,
        "rewrite_enabled": config.rewrite_record,
    })
}

pub fn export_logs() -> Result<PathBuf, String> {
    let base = logs_dir();
    let telemetry = telemetry_dir();
    let tmp = std::env::temp_dir().join("amanuensis-export-staging");

    // Clean and recreate staging dir
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("creating staging dir: {e}"))?;

    // Copy human log
    let human_log = base.join("amanuensis.log");
    if human_log.exists() {
        let _ = fs::copy(&human_log, tmp.join("amanuensis.log"));
    }

    // Copy telemetry JSONL files
    if telemetry.exists() {
        if let Ok(rd) = fs::read_dir(&telemetry) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    if let Some(name) = path.file_name() {
                        let _ = fs::copy(&path, tmp.join(name));
                    }
                }
            }
        }
    }

    // Write device-info.json
    let device_json = serde_json::to_string_pretty(&device_profile_json())
        .map_err(|e| format!("serializing device info: {e}"))?;
    fs::write(tmp.join("device-info.json"), device_json)
        .map_err(|e| format!("writing device-info.json: {e}"))?;

    // Write config-summary.json
    let config_json = serde_json::to_string_pretty(&config_summary_json())
        .map_err(|e| format!("serializing config summary: {e}"))?;
    fs::write(tmp.join("config-summary.json"), config_json)
        .map_err(|e| format!("writing config-summary.json: {e}"))?;

    // Create zip using PowerShell Compress-Archive
    let version = env!("CARGO_PKG_VERSION");
    let timestamp = utc_timestamp();
    let zip_name = format!("telemetry-manuensis-{version}-{timestamp}.zip");
    let zip_path = base.join(&zip_name);

    let source = tmp.join("*");
    let output = std::process::Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("Compress-Archive -Path $args[0] -DestinationPath $args[1] -Force")
        .arg(source)
        .arg(&zip_path)
        .output()
        .map_err(|e| format!("running powershell: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Compress-Archive failed: {stderr}"));
    }

    // Clean up staging
    let _ = fs::remove_dir_all(&tmp);

    // Open Explorer to the logs folder
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", zip_path.to_string_lossy()))
        .spawn();

    Ok(zip_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    // The telemetry dir env var and consent atomic are process-global, but
    // `cargo test` runs `#[test]` fns in parallel threads. Serialize every test
    // that touches them so one test's cleanup can't race another's writes.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup_telemetry_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("amanuensis-telemetry-test-{id}"));
        let telemetry = tmp.join("logs").join("telemetry");
        // Remove stale dir from prior failed runs (COUNTER resets per process).
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&telemetry).unwrap();
        // SAFETY: tests run single-threaded by default; env vars are process-global.
        unsafe { std::env::set_var("AM_TELEMETRY_DIR", &telemetry) };
        tmp
    }

    fn cleanup(tmp: &PathBuf) {
        let _ = fs::remove_dir_all(tmp);
        // SAFETY: tests run single-threaded by default; env vars are process-global.
        unsafe {
            let _ = std::env::remove_var("AM_TELEMETRY_DIR");
        }
    }

    #[test]
    fn consent_default_off() {
        let _lock = LOCK.lock().unwrap();
        let tmp = setup_telemetry_dir();
        set_consent(false);
        app_start();
        let telemetry = tmp.join("logs").join("telemetry");
        let count = if telemetry.exists() {
            fs::read_dir(&telemetry).unwrap().count()
        } else {
            0
        };
        assert_eq!(count, 0);
        cleanup(&tmp);
    }

    #[test]
    fn consent_gate_events_written() {
        let _lock = LOCK.lock().unwrap();
        let tmp = setup_telemetry_dir();
        set_consent(true);
        app_start();
        let telemetry = tmp.join("logs").join("telemetry");
        assert!(telemetry.exists());
        let files: Vec<_> = fs::read_dir(&telemetry)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!files.is_empty());
        let content = fs::read_to_string(files[0].path()).unwrap();
        let parsed: Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event"], "app_start");
        assert_eq!(parsed["app_version"], env!("CARGO_PKG_VERSION"));
        set_consent(false);
        cleanup(&tmp);
    }

    #[test]
    fn jsonl_line_is_valid_json() {
        let _lock = LOCK.lock().unwrap();
        let tmp = setup_telemetry_dir();
        set_consent(true);
        backend_bench(BackendBench {
            candidate: "cpu-4".to_owned(),
            provider: "cpu".to_owned(),
            device: "test".to_owned(),
            rtf: 0.5,
            wall_ms: 1234,
            margin: Some(1.1),
            accepted: true,
        });
        let telemetry = tmp.join("logs").join("telemetry");
        let files: Vec<_> = fs::read_dir(&telemetry)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let content = fs::read_to_string(files[0].path()).unwrap();
        let parsed: Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["rtf"], 0.5);
        assert_eq!(parsed["accepted"], true);
        set_consent(false);
        cleanup(&tmp);
    }

    #[test]
    fn multiple_events_append_to_same_file() {
        let _lock = LOCK.lock().unwrap();
        let tmp = setup_telemetry_dir();
        set_consent(true);
        app_start();
        device_profile();
        let telemetry = tmp.join("logs").join("telemetry");
        let files: Vec<_> = fs::read_dir(&telemetry)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
        let content = fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["event"], "app_start");
        assert_eq!(second["event"], "device_profile");
        set_consent(false);
        cleanup(&tmp);
    }

    #[test]
    fn device_profile_contains_required_fields() {
        let _lock = LOCK.lock().unwrap();
        let tmp = setup_telemetry_dir();
        set_consent(true);
        device_profile();
        let telemetry = tmp.join("logs").join("telemetry");
        let files: Vec<_> = fs::read_dir(&telemetry)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let content = fs::read_to_string(files[0].path()).unwrap();
        let parsed: Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event"], "device_profile");
        assert!(parsed["os_name"].is_string());
        assert!(parsed["arch"].is_string());
        assert!(parsed["cpu_brand"].is_string());
        assert!(parsed["ram_total_gb"].is_number());
        assert!(parsed["models"].is_array());
        set_consent(false);
        cleanup(&tmp);
    }
}
