use super::fetch;
pub const REPO_OWNER: &str = "csukuangfj2";
const NEMOTRON_REPO: &str = "sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25";

pub struct ModelSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub repo: &'static str,
    pub size_mb: u32,
    pub min_ram_gb: u32,
    pub chunk_ms: u32,
    pub wer_note: &'static str,
}

pub const REGISTRY: [ModelSpec; 1] = [ModelSpec {
    id: "nemotron",
    display_name: "Nemotron Streaming",
    repo: NEMOTRON_REPO,
    size_mb: 632,
    min_ram_gb: 4,
    chunk_ms: 80,
    wer_note: "~7.2\u{2013}7.8% WER, tracks voice closely",
}];

pub fn spec_by_id(id: &str) -> Option<&'static ModelSpec> {
    REGISTRY.iter().find(|spec| spec.id == id)
}

pub fn kind_by_id(id: &str) -> Option<ModelKind> {
    match id {
        "nemotron" => Some(ModelKind::Nemotron),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Nemotron,
}

impl ModelKind {
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::Nemotron => &REGISTRY[0],
        }
    }
}

#[derive(Clone)]
pub struct ModelPaths {
    pub encoder: std::path::PathBuf,
    pub decoder: std::path::PathBuf,
    pub joiner: std::path::PathBuf,
    pub tokens: std::path::PathBuf,
}

pub use fetch::{DownloadProgress, cached_model_dir, progress_text};

pub fn cache_dir_for(kind: ModelKind) -> Option<std::path::PathBuf> {
    Some(cached_model_dir(kind.spec()))
}

pub fn repo_cache_dir_for(kind: ModelKind) -> Option<std::path::PathBuf> {
    Some(cached_model_dir(kind.spec()))
}

pub fn ensure_model_by_spec(
    spec: &ModelSpec,
    on_progress: &mut dyn FnMut(DownloadProgress),
) -> Result<ModelPaths, String> {
    fetch::ensure_model(spec, on_progress)
}

pub fn ensure_model(
    kind: ModelKind,
    on_progress: &mut dyn FnMut(DownloadProgress),
) -> Result<ModelPaths, String> {
    ensure_model_by_spec(kind.spec(), on_progress)
}

pub fn is_model_cached(spec_id: &str) -> bool {
    spec_by_id(spec_id).is_some_and(fetch::is_spec_cached)
}
