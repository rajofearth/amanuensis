mod model;
mod nemotron;
mod unified;
pub mod worker;

pub use model::{ModelKind, ModelPaths, REPO_OWNER};
pub use nemotron::NemotronBackend;
pub use unified::UnifiedBackend;
pub use worker::{Command, Event, spawn_worker};

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
