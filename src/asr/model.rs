use std::path::PathBuf;

use hf_hub::HFClientSync;

use super::Mode;

pub const REPO_OWNER: &str = "csukuangfj2";
const NEMOTRON_REPO: &str = "sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25";
const UNIFIED_REPO: &str = "sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-240ms";

const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Nemotron,
    Unified,
}

impl ModelKind {
    pub fn repo_name(self) -> &'static str {
        match self {
            Self::Nemotron => NEMOTRON_REPO,
            Self::Unified => UNIFIED_REPO,
        }
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

pub fn ensure_model(kind: ModelKind, on_file: &mut impl FnMut(&str)) -> Result<ModelPaths, String> {
    let client = HFClientSync::new().map_err(|error| format!("hf client init failed: {error}"))?;
    let repo = client.model(REPO_OWNER, kind.repo_name());
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
