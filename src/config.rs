use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::log;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AsrCacheEntry {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub threads: i32,
    #[serde(default)]
    pub win_rtf: f32,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub cached_at: String,
    #[serde(default)]
    pub device_hash: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NormalizerCacheEntry {
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub win_ms: u64,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub cached_at: String,
    #[serde(default)]
    pub device_hash: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendCache {
    #[serde(default)]
    pub asr: AsrCacheEntry,
    #[serde(default)]
    pub normalizer: NormalizerCacheEntry,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AppConfig {
    /// Legacy single-model setting, kept for backwards compatibility. Newer
    /// fields (`record_model` / `live_model`) win when present; otherwise the
    /// per-mode engines inherit from `model` so upgraded installs keep
    /// dictating on the engine they already used.
    pub model: String,
    pub record_model: String,
    pub live_model: String,
    pub rewrite_record: bool,
    #[serde(default)]
    pub telemetry_consent: bool,
    #[serde(default = "default_tray_enabled")]
    pub tray_enabled: bool,
    #[serde(default)]
    pub backend_cache: BackendCache,
}

fn default_record_model() -> String {
    "moonshine".to_owned()
}

fn default_live_model() -> String {
    "nemotron".to_owned()
}

fn default_rewrite_record() -> bool {
    true
}

fn default_tray_enabled() -> bool {
    true
}

/// Serde intermediate that records whether the per-mode fields were present in
/// the stored JSON, so `AppConfig` can resolve them against the legacy `model`.
#[derive(Default, Deserialize)]
struct AppConfigRaw {
    model: Option<String>,
    #[serde(default)]
    record_model: Option<String>,
    #[serde(default)]
    live_model: Option<String>,
    #[serde(default)]
    rewrite_record: Option<bool>,
    #[serde(default)]
    telemetry_consent: bool,
    #[serde(default = "default_tray_enabled")]
    tray_enabled: bool,
    #[serde(default)]
    backend_cache: BackendCache,
}

impl From<AppConfigRaw> for AppConfig {
    fn from(raw: AppConfigRaw) -> Self {
        let legacy_model = raw.model.clone();
        let record_model = raw
            .record_model
            .or_else(|| legacy_model.clone())
            .unwrap_or_else(default_record_model);
        let live_model = raw
            .live_model
            .or_else(|| legacy_model.clone())
            .unwrap_or_else(default_live_model);
        Self {
            model: legacy_model.unwrap_or_else(|| "nemotron".to_owned()),
            record_model,
            live_model,
            rewrite_record: raw.rewrite_record.unwrap_or_else(default_rewrite_record),
            telemetry_consent: raw.telemetry_consent,
            tray_enabled: raw.tray_enabled,
            backend_cache: raw.backend_cache,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfigRaw::default().into()
    }
}

pub fn config_dir() -> Result<PathBuf, String> {
    let base = PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA not set")?);
    Ok(base.join("amanuensis"))
}

fn legacy_config_dir() -> Result<PathBuf, String> {
    let base = PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA not set")?);
    Ok(base.join("raycast-dictation"))
}

fn config_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load() -> Option<AppConfig> {
    let current = config_path().ok()?;
    load_from(&current).or_else(|| {
        legacy_config_dir()
            .ok()
            .map(|dir| dir.join("config.json"))
            .and_then(|path| load_from(&path))
    })
}

pub fn save(config: &AppConfig) -> Result<(), String> {
    save_to(&config_path()?, config)
}

pub fn delete() -> Result<bool, String> {
    let path = config_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("removing {}: {error}", path.display())),
    }
}

pub fn set_tray_enabled(enabled: bool) -> Result<(), String> {
    let mut config = load().unwrap_or_default();
    config.tray_enabled = enabled;
    save(&config)
}

fn load_from(path: &Path) -> Option<AppConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<AppConfigRaw>(&text) {
        Ok(raw) => Some(raw.into()),
        Err(error) => {
            log!(
                "config",
                "config at {} unreadable or schema changed, re-running setup ({error})",
                path.display()
            );
            None
        }
    }
}

fn save_to(path: &Path, config: &AppConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("creating config dir: {error}"))?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|error| format!("serializing config: {error}"))?;
    std::fs::write(path, json).map_err(|error| format!("writing config: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "amanuensis-test-{}-{unique}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_path("roundtrip");
        let written = AppConfig {
            model: "nemotron".to_owned(),
            record_model: "moonshine".to_owned(),
            live_model: "nemotron".to_owned(),
            rewrite_record: true,
            telemetry_consent: false,
            tray_enabled: true,
            backend_cache: BackendCache {
                asr: AsrCacheEntry {
                    provider: "cpu".to_owned(),
                    threads: 4,
                    win_rtf: 0.84,
                    app_version: "1.0.3".to_owned(),
                    cached_at: "2026-08-29".to_owned(),
                    device_hash: "abcdef".to_owned(),
                },
                ..Default::default()
            },
        };
        save_to(&path, &written).expect("save");
        let loaded = load_from(&path).expect("load after save");
        assert_eq!(loaded, written);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_backend_cache_defaults_to_empty() {
        let path = temp_path("nocache");
        std::fs::write(&path, r#"{"model":"nemotron","tray_enabled":true}"#).unwrap();
        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded.backend_cache, BackendCache::default());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn partial_or_unknown_cache_fields_do_not_break_serde() {
        let path = temp_path("partialcache");
        std::fs::write(
            &path,
            r#"{
                "model":"nemotron",
                "tray_enabled":true,
                "backend_cache":{
                    "asr":{"provider":"cuda","threads":2},
                    "normalizer":{"backend":"cpu","future_field":"ignored"}
                }
            }"#,
        )
        .unwrap();
        let loaded = load_from(&path).expect("load with partial cache");
        assert_eq!(loaded.backend_cache.asr.provider, "cuda");
        assert_eq!(loaded.backend_cache.asr.threads, 2);
        assert_eq!(loaded.backend_cache.asr.app_version, "");
        assert_eq!(loaded.backend_cache.normalizer.backend, "cpu");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_missing_returns_none() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), None);
    }

    #[test]
    fn load_corrupt_returns_none() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_from(&path), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_model_inherits_into_both_modes() {
        // A pre-per-mode config only carries `model`. Upgrading must keep both
        // engines on that same model so behavior doesn't silently change.
        let path = temp_path("legacymodel");
        std::fs::write(&path, r#"{"model":"nemotron","tray_enabled":true}"#).unwrap();
        let loaded = load_from(&path).expect("load legacy");
        assert_eq!(loaded.record_model, "nemotron");
        assert_eq!(loaded.live_model, "nemotron");
        assert_eq!(loaded.rewrite_record, true);
        assert!(!loaded.telemetry_consent);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fresh_default_uses_new_default_profile() {
        // No `model` and no per-mode fields means a brand-new install, which
        // gets the moonshine record + nemotron live default profile.
        let config = AppConfig::default();
        assert_eq!(config.record_model, "moonshine");
        assert_eq!(config.live_model, "nemotron");
        assert_eq!(config.rewrite_record, true);
        assert!(!config.telemetry_consent);
    }

    #[test]
    fn per_mode_fields_override_legacy_model() {
        let path = temp_path("permode");
        std::fs::write(
            &path,
            r#"{
                "model":"nemotron",
                "record_model":"moonshine",
                "live_model":"nemotron",
                "rewrite_record":false,
                "telemetry_consent":true
            }"#,
        )
        .unwrap();
        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded.record_model, "moonshine");
        assert_eq!(loaded.live_model, "nemotron");
        assert!(!loaded.rewrite_record);
        assert!(loaded.telemetry_consent);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_old_two_slot_schema_now_loads_with_nemotron_engines() {
        // The old two-slot config (record_model/live_model, no `model`) is
        // superseded by our own per-mode fields, so it parses cleanly now.
        let path = temp_path("oldschema");
        std::fs::write(
            &path,
            r#"{"record_model":"nemotron","live_model":"nemotron"}"#,
        )
        .unwrap();
        let loaded = load_from(&path).expect("load old two-slot schema");
        assert_eq!(loaded.record_model, "nemotron");
        assert_eq!(loaded.live_model, "nemotron");
        let _ = std::fs::remove_file(path);
    }
}
