pub mod fetch;
mod model;
mod nemotron;
pub mod worker;

pub use fetch::DownloadProgress;
pub use model::{
    ModelKind, ModelPaths, ModelSpec, REGISTRY, REPO_OWNER, cache_dir_for, ensure_model,
    ensure_model_by_spec, is_model_cached, kind_by_id, progress_text, repo_cache_dir_for,
    spec_by_id,
};
pub use nemotron::NemotronBackend;
pub use worker::{Command, Event, ModelSelection, spawn_worker};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Record,
    Live,
}

pub trait AsrBackend {
    fn start_session(&mut self);
    fn feed_audio(&mut self, samples: &[f32]);
    fn partial(&self) -> Option<String>;
    fn finalize(&mut self) -> String;
}

/// Thread count for the ASR recognizer. `ASR_THREADS` is the highest-priority
/// override; otherwise the caller-chosen `preferred` wins, defaulting to 2.
pub(crate) fn threads(preferred: i32) -> i32 {
    std::env::var("ASR_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(preferred)
}

/// Recognizer execution provider. `ASR_PROVIDER` is the highest-priority
/// override; otherwise the caller-chosen `preferred` provider wins.
pub(crate) fn provider(preferred: Option<&str>) -> Option<String> {
    std::env::var("ASR_PROVIDER")
        .ok()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .or_else(|| preferred.map(str::to_owned))
}
