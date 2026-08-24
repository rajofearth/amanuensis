use std::{
    io::Read,
    path::{Path, PathBuf},
};

use crate::config;

use super::model::{ModelPaths, ModelSpec, REPO_OWNER};

const HF_BASE: &str = "https://huggingface.co";
pub(crate) const MODEL_FILES: [&str; 4] = [
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub file: String,
    pub done: u64,
    pub total: u64,
}

struct Aggregate {
    total: u64,
    completed: u64,
}

impl Aggregate {
    fn new(total: u64) -> Self {
        Self {
            total,
            completed: 0,
        }
    }

    fn finish_file(&mut self, bytes: u64) {
        self.completed += bytes;
    }

    fn current(&self, file: &str, streamed: u64) -> DownloadProgress {
        DownloadProgress {
            file: file.to_owned(),
            done: self.completed + streamed,
            total: self.total,
        }
    }
}

fn models_root() -> Result<PathBuf, String> {
    Ok(config::config_dir()?.join("models"))
}

pub fn cached_model_dir(spec: &ModelSpec) -> PathBuf {
    models_root()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(spec.id)
}

fn file_url(spec: &ModelSpec, file: &str) -> String {
    format!("{HF_BASE}/{REPO_OWNER}/{}/resolve/main/{file}", spec.repo)
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .build()
    });
    &AGENT
}

fn head_content_length(url: &str) -> Option<u64> {
    let response = http_agent().head(url).call().ok()?;
    response.header("Content-Length")?.parse::<u64>().ok()
}

pub fn total_size(spec: &ModelSpec) -> Option<u64> {
    MODEL_FILES
        .iter()
        .map(|file| head_content_length(&file_url(spec, file)))
        .collect::<Option<Vec<_>>>()
        .map(|sizes| sizes.iter().sum())
}

fn dir_complete(dir: &Path, files: &[&str]) -> bool {
    files.iter().all(|file| {
        std::fs::metadata(dir.join(file))
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false)
    })
}

pub fn is_spec_cached(spec: &ModelSpec) -> bool {
    dir_complete(&cached_model_dir(spec), &MODEL_FILES)
}

pub(crate) fn mb_summary(done: u64, total: u64) -> String {
    if total == 0 {
        return "? MB".to_owned();
    }
    format!(
        "{}% · {}/{} MB",
        ((done * 100) / total).min(100),
        (done as f64 / 1e6).round() as u64,
        (total as f64 / 1e6).round() as u64
    )
}

pub fn progress_text(display_name: &str, progress: &DownloadProgress) -> String {
    format!(
        "Downloading {} — {} ({})",
        display_name,
        mb_summary(progress.done, progress.total),
        progress.file
    )
}

pub fn ensure_file(
    spec: &ModelSpec,
    file: &str,
    expected_size: Option<u64>,
    on_chunk: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let url = file_url(spec, file);
    let expected = match expected_size {
        Some(size) => Some(size),
        None => head_content_length(&url),
    };
    let dir = cached_model_dir(spec);
    std::fs::create_dir_all(&dir).map_err(|error| format!("creating model dir: {error}"))?;
    let final_path = dir.join(file);
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    if let Ok(meta) = std::fs::metadata(&final_path)
        && meta.is_file()
    {
        let accepted = match expected {
            Some(size) => meta.len() == size,
            None => meta.len() > 0,
        };
        if accepted {
            return Ok(());
        }
    }
    let part_path = PathBuf::from(format!("{}.part", final_path.display()));
    let mut open_len = 0_u64;
    if let Ok(meta) = std::fs::metadata(&part_path) {
        open_len = meta.len();
    }
    let resumed = if open_len > 0 {
        match http_agent()
            .get(&url)
            .set("Range", &format!("bytes={open_len}-"))
            .call()
        {
            Ok(response) if response.status() == 206 => Some((response, open_len)),
            _ => {
                let _ = std::fs::remove_file(&part_path);
                None
            }
        }
    } else {
        None
    };
    let (response, start_len) = match resumed {
        Some(pair) => pair,
        None => {
            let response = http_agent()
                .get(&url)
                .call()
                .map_err(|error| error.to_string())?;
            (response, 0)
        }
    };
    let mut out = if start_len > 0 {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .map_err(|error| format!("opening {}: {error}", part_path.display()))?
    } else {
        std::fs::File::create(&part_path)
            .map_err(|error| format!("creating {}: {error}", part_path.display()))?
    };
    let mut reader = response.into_reader();
    let mut buffer = [0_u8; 65536];
    let mut done = start_len;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                std::io::Write::write_all(&mut out, &buffer[..n])
                    .map_err(|error| format!("writing {}: {error}", part_path.display()))?;
                done += n as u64;
                on_chunk(done);
            }
            Err(error) => return Err(format!("streaming {file}: {error}")),
        }
    }
    std::io::Write::flush(&mut out).map_err(|error| format!("flushing {file}: {error}"))?;
    drop(out);
    if let Some(expected) = expected
        && done != expected
    {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!(
            "{file}: downloaded {done} bytes, expected {expected}"
        ));
    }
    std::fs::rename(&part_path, &final_path)
        .map_err(|error| format!("promoting {file}: {error}"))?;
    Ok(())
}

pub fn ensure_model(
    spec: &ModelSpec,
    on_progress: &mut dyn FnMut(DownloadProgress),
) -> Result<ModelPaths, String> {
    let dir = cached_model_dir(spec);
    std::fs::create_dir_all(&dir).map_err(|error| format!("creating model dir: {error}"))?;
    let known_sizes: Vec<Option<u64>> = MODEL_FILES
        .iter()
        .map(|file| head_content_length(&file_url(spec, file)))
        .collect();
    let total: u64 = known_sizes
        .iter()
        .fold(0, |sum, size| sum + size.unwrap_or(0));
    let mut aggregate = Aggregate::new(total);
    for (index, file) in MODEL_FILES.iter().enumerate() {
        let expected = known_sizes[index];
        let file_name = (*file).to_owned();
        ensure_file(spec, file, expected, &mut |streamed| {
            on_progress(aggregate.current(&file_name, streamed));
        })?;
        let finished = expected.unwrap_or_else(|| {
            std::fs::metadata(dir.join(file))
                .map(|meta| meta.len())
                .unwrap_or(0)
        });
        aggregate.finish_file(finished);
    }
    Ok(ModelPaths {
        encoder: dir.join(MODEL_FILES[0]),
        decoder: dir.join(MODEL_FILES[1]),
        joiner: dir.join(MODEL_FILES[2]),
        tokens: dir.join(MODEL_FILES[3]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "raycast-dictation-fetch-test-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn file_url_has_expected_shape() {
        let spec = &super::super::model::REGISTRY[0];
        assert_eq!(
            file_url(spec, "encoder.int8.onnx"),
            format!(
                "https://huggingface.co/{REPO_OWNER}/{}/resolve/main/encoder.int8.onnx",
                spec.repo
            )
        );
    }

    #[test]
    fn aggregate_math_skip_adds_full_partial_adds_streamed() {
        let mut aggregate = Aggregate::new(700_000_000);
        aggregate.finish_file(462_000_000);
        let mid = aggregate.current("decoder.int8.onnx", 30_000_000);
        assert_eq!(mid.done, 492_000_000);
        assert_eq!(mid.total, 700_000_000);
        aggregate.finish_file(238_000_000);
        let next = aggregate.current("joiner.int8.onnx", 0);
        assert_eq!(next.done, 700_000_000);
    }

    #[test]
    fn mb_summary_handles_unknown_total_and_rounding() {
        assert_eq!(mb_summary(238_000_000, 0), "? MB");
        assert_eq!(mb_summary(238_000_000, 700_000_000), "34% · 238/700 MB");
        assert_eq!(mb_summary(700_500_000, 700_000_000), "100% · 701/700 MB");
        assert_eq!(mb_summary(0, 700_000_000), "0% · 0/700 MB");
    }

    #[test]
    fn dir_complete_rejects_truncated_files() {
        let dir = temp_dir("complete");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), b"hello").unwrap();
        std::fs::write(dir.join("b.bin"), b"world").unwrap();
        assert!(dir_complete(&dir, &["a.bin", "b.bin"]));
        std::fs::write(dir.join("b.bin"), b"").unwrap();
        assert!(!dir_complete(&dir, &["a.bin", "b.bin"]));
        assert!(!dir_complete(&dir, &["a.bin", "missing.bin"]));
        let _ = std::fs::remove_dir_all(dir);
    }
}
