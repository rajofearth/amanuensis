mod model;
mod nemotron;
pub mod worker;

pub use model::{
    ModelKind, ModelPaths, ModelSpec, REGISTRY, REPO_OWNER, cache_dir_for, ensure_model,
    ensure_model_by_spec, is_model_cached, kind_by_id, repo_cache_dir_for, spec_by_id,
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

pub(crate) fn threads() -> i32 {
    std::env::var("ASR_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2)
}
