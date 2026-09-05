use std::{
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
    time::Duration,
};

use amanuensis::asr::rewrite::ensure_s1_assets;
use amanuensis::asr::{ModelKind, ModelSpec, ensure_model_by_spec, kind_by_id, repo_cache_dir_for};
use amanuensis::log;

use crate::messages::DownloadMessage;
pub(crate) fn remove_dir_all_retrying(dir: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            let mut last = error;
            for _ in 0..10 {
                thread::sleep(Duration::from_millis(250));
                match std::fs::remove_dir_all(dir) {
                    Ok(()) => return Ok(()),
                    Err(retry_error) => last = retry_error,
                }
            }
            Err(format!("removing {}: {last}", dir.display()))
        }
    }
}

pub(crate) fn spawn_model_download(
    spec: &'static ModelSpec,
    purge_first: bool,
    generation: u64,
    cancel: Arc<AtomicBool>,
    download: mpsc::Sender<DownloadMessage>,
) {
    thread::spawn(move || {
        if purge_first {
            let purged = kind_by_id(spec.id).and_then(repo_cache_dir_for);
            if let Some(repo_dir) = purged {
                if let Err(text) = remove_dir_all_retrying(&repo_dir) {
                    log!("app", "download FAILED: {text}");
                    let _ = download.send(DownloadMessage::Finished {
                        generation,
                        result: Err(text),
                    });
                    return;
                }
            }
        }
        run_download(spec, generation, cancel, download);
    });
}

/// s1-mini rewrite-asset fetch on the same download channel. Generation/cancel
/// semantics are identical to `spawn_model_download`: the caller reuses the
/// engine download's generation (so stale-drop keeps working) and stores a
/// fresh cancel flag before spawning. `ensure_s1_assets` already reports
/// `DownloadProgress` in the same shape as the engine fetch, so the existing
/// progress bar consumes it unchanged. `engine` is passed through into the
/// terminal `Finished` so the reader knows which record engine is ready.
pub(crate) fn spawn_s1_download(
    engine: ModelKind,
    generation: u64,
    cancel: Arc<AtomicBool>,
    download: mpsc::Sender<DownloadMessage>,
) {
    thread::spawn(move || {
        match ensure_s1_assets(
            &mut |progress| {
                log!(
                    "app",
                    "s1 download progress: {} · {} B / {} B",
                    progress.file,
                    progress.done,
                    progress.total
                );
                let _ = download.send(DownloadMessage::Progress {
                    generation,
                    progress,
                });
            },
            Some(&cancel),
        ) {
            Ok(true) => {
                log!("app", "s1-mini download finished");
                let _ = download.send(DownloadMessage::Finished {
                    generation,
                    result: Ok(engine),
                });
            }
            Ok(false) => {
                log!("app", "s1-mini download cancelled");
                let _ = download.send(DownloadMessage::Cancelled { generation });
            }
            Err(error) => {
                log!("app", "s1-mini download FAILED: {error}");
                let _ = download.send(DownloadMessage::Finished {
                    generation,
                    result: Err(error),
                });
            }
        }
    });
}

fn run_download(
    spec: &'static ModelSpec,
    generation: u64,
    cancel: Arc<AtomicBool>,
    download: mpsc::Sender<DownloadMessage>,
) {
    match ensure_model_by_spec(
        spec,
        &mut |progress| {
            log!(
                "app",
                "download progress: {} · {} B / {} B",
                progress.file,
                progress.done,
                progress.total
            );
            let _ = download.send(DownloadMessage::Progress {
                generation,
                progress,
            });
        },
        Some(&cancel),
    ) {
        Ok(Some(_)) => {
            if let Some(kind) = kind_by_id(spec.id) {
                log!("app", "download finished for {}", spec.id);
                let _ = download.send(DownloadMessage::Finished {
                    generation,
                    result: Ok(kind),
                });
            }
        }
        Ok(None) => {
            log!("app", "download cancelled for {}", spec.id);
            let _ = download.send(DownloadMessage::Cancelled { generation });
        }
        Err(error) => {
            let _ = download.send(DownloadMessage::Finished {
                generation,
                result: Err(error),
            });
        }
    }
}
