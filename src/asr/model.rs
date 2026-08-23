use std::path::PathBuf;

use hf_hub::HFClientSync;

use super::Mode;

pub const REPO_OWNER: &str = "csukuangfj2";
const NEMOTRON_REPO: &str = "sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25";
const UNIFIED_REPO: &str = "sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms";

pub struct ModelSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub repo: &'static str,
    pub size_mb: u32,
    pub min_ram_gb: u32,
    pub chunk_ms: u32,
    pub wer_note: &'static str,
}

pub const REGISTRY: [ModelSpec; 2] = [
    ModelSpec {
        id: "nemotron",
        display_name: "Nemotron Streaming (Live)",
        repo: NEMOTRON_REPO,
        size_mb: 632,
        min_ram_gb: 4,
        chunk_ms: 80,
        wer_note: "~7.2\u{2013}7.8% WER, tracks voice closely",
    },
    ModelSpec {
        id: "unified",
        display_name: "Parakeet Unified (Record)",
        repo: UNIFIED_REPO,
        size_mb: 632,
        min_ram_gb: 4,
        chunk_ms: 240,
        wer_note: "higher accuracy, slow on this device",
    },
];

pub fn spec_by_id(id: &str) -> Option<&'static ModelSpec> {
    REGISTRY.iter().find(|spec| spec.id == id)
}

pub fn kind_by_id(id: &str) -> Option<ModelKind> {
    match id {
        "nemotron" => Some(ModelKind::Nemotron),
        "unified" => Some(ModelKind::Unified),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Nemotron,
    Unified,
}

impl ModelKind {
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::Nemotron => &REGISTRY[0],
            Self::Unified => &REGISTRY[1],
        }
    }

    pub fn repo_name(self) -> &'static str {
        self.spec().repo
    }
}

impl From<Mode> for ModelKind {
    fn from(mode: Mode) -> Self {
        // Record used the unified (240ms-window) model, but its windowed decode
        // runs ~15x realtime on this SoC vs nemotron's ~2.5x. Nemotron handles
        // short record-mode utterances fine and gives legacy-parity latency.
        // See docs/PHASE11-NOTES.md for the offline-batch endgame.
        let _ = mode;
        Self::Nemotron
    }
}

#[derive(Clone)]
pub struct ModelPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

pub fn ensure_model_by_spec(
    spec: &ModelSpec,
    on_file: &mut impl FnMut(&str),
) -> Result<ModelPaths, String> {
    let client = HFClientSync::new().map_err(|error| format!("hf client init failed: {error}"))?;
    let repo = client.model(REPO_OWNER, spec.repo);
    let mut paths: Vec<PathBuf> = Vec::with_capacity(MODEL_FILES.len());
    for file in MODEL_FILES {
        let path = repo
            .download_file()
            .filename(file)
            .send()
            .map_err(|error| format!("downloading {file}: {error}"))?;
        on_file(file);
        paths.push(path);
    }
    Ok(ModelPaths {
        encoder: paths[0].clone(),
        decoder: paths[1].clone(),
        joiner: paths[2].clone(),
        tokens: paths[3].clone(),
    })
}

pub fn ensure_model(kind: ModelKind, on_file: &mut impl FnMut(&str)) -> Result<ModelPaths, String> {
    ensure_model_by_spec(kind.spec(), on_file)
}

pub fn is_model_cached(spec_id: &str) -> bool {
    let Some(spec) = spec_by_id(spec_id) else {
        return false;
    };
    let repo_dir = hf_hub::resolve_cache_dir().join(format!("models--{REPO_OWNER}--{}", spec.repo));
    let Ok(snapshots) = std::fs::read_dir(repo_dir.join("snapshots")) else {
        return false;
    };
    for revision in snapshots.flatten() {
        if MODEL_FILES
            .iter()
            .all(|file| revision.path().join(file).is_file())
        {
            return true;
        }
    }
    false
}
