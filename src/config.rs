use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub record_model: String,
    pub live_model: String,
}

pub fn config_dir() -> Result<PathBuf, String> {
    let base = PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA not set")?);
    Ok(base.join("raycast-dictation"))
}

fn config_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("config.json"))
}

pub fn load() -> Option<AppConfig> {
    load_from(&config_path().ok()?)
}

pub fn save(config: &AppConfig) -> Result<(), String> {
    save_to(&config_path()?, config)
}

fn load_from(path: &Path) -> Option<AppConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
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
            "raycast-dictation-test-{}-{unique}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_path("roundtrip");
        let written = AppConfig {
            record_model: "nemotron".to_owned(),
            live_model: "unified".to_owned(),
        };
        save_to(&path, &written).expect("save");
        let loaded = load_from(&path).expect("load after save");
        assert_eq!(loaded, written);
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
}
