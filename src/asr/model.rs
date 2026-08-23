use std::path::PathBuf;

use hf_hub::HFClientSync;

pub const REPO_OWNER: &str = "csukuangfj2";
pub const REPO_NAME: &str = "sherpa-onnx-nemotron-speech-streaming-en-0.6b-80ms-int8-2026-04-25";

const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

#[derive(Clone)]
pub struct ModelPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

pub fn ensure_model(on_file: &mut impl FnMut(&str)) -> Result<ModelPaths, String> {
    let client = HFClientSync::new().map_err(|error| format!("hf client init failed: {error}"))?;
    let repo = client.model(REPO_OWNER, REPO_NAME);
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
