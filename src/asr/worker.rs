use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Instant,
};

use crate::log;

use super::model;
use super::moonshine::MoonshineBackend;
use super::nemotron::NemotronBackend;
use super::{AsrBackend, Mode, ModelKind, ModelPaths};

#[derive(Debug)]
pub enum Command {
    Start,
    Stop,
    Chunk(Vec<f32>),
    #[allow(dead_code)]
    Shutdown,
    ReloadForDebug,
    SwitchMode(Mode),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    /// ASR engine for record mode.
    pub record_model: ModelKind,
    /// ASR engine for live mode.
    pub live_model: ModelKind,
    /// sherpa provider string; `None` means the default (cpu). Env
    /// `ASR_PROVIDER` overrides this at recognizer load time.
    pub provider: Option<String>,
    /// Thread count for the recognizer. Env `ASR_THREADS` overrides this at
    /// recognizer load time.
    pub threads: i32,
    /// Whether record-mode transcripts run through the s1-mini rewrite pass.
    pub rewrite_record: bool,
    /// Thread count for the s1-mini rewrite subprocess.
    pub rewrite_threads: i32,
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            record_model: ModelKind::Moonshine,
            live_model: ModelKind::Nemotron,
            provider: None,
            threads: 2,
            rewrite_record: true,
            rewrite_threads: 2,
        }
    }
}

pub enum Event {
    LoadingProgress(String),
    Ready,
    ModelReady(Mode),
    Partial(String),
    Committed { text: String, duration_secs: f32 },
}

const SESSION_SAMPLE_RATE: usize = 16_000;

pub fn spawn_worker(events: Sender<Event>, selection: ModelSelection) -> Sender<Command> {
    let (commands, receiver) = channel::<Command>();
    thread::Builder::new()
        .name("asr-worker".to_owned())
        .spawn(move || run(receiver, events, selection))
        .expect("failed to spawn asr worker thread");
    commands
}

fn run(commands: Receiver<Command>, events: Sender<Event>, selection: ModelSelection) {
    let mut mode = Mode::Record;
    let wanted = model_for_mode(selection.record_model, selection.live_model, mode);
    let Some(paths) = fetch_paths(wanted, &events) else {
        return;
    };

    let _ = events.send(Event::LoadingProgress("loading recognizer".to_owned()));
    let started = Instant::now();
    let Some(mut backend) = load_backend(&paths, selection.provider.as_deref(), selection.threads)
    else {
        log!("asr", "ERROR: OnlineRecognizer::create returned None");
        let _ = events.send(Event::LoadingProgress("load failed".to_owned()));
        return;
    };
    log!(
        "asr",
        "cold load {mode:?} in {:.2}s, resident {:.0} MiB",
        started.elapsed().as_secs_f64(),
        resident_mib()
    );
    crate::telemetry::backend_selected(crate::telemetry::BackendSelected {
        kind: if mode == Mode::Record {
            "asr_record".into()
        } else {
            "asr_live".into()
        },
        provider: selection.provider.as_deref().unwrap_or("cpu").to_string(),
        backend: wanted.spec().id.to_owned(),
        device: "cpu".into(),
        threads: selection.threads,
        decided_by: "cache".into(),
        bench_ref: None,
    });
    if events.send(Event::Ready).is_err() {
        return;
    }

    let mut cached_paths: Option<ModelPaths> = Some(paths);
    let mut loaded_kind = wanted;

    let mut active = false;
    let mut session_samples: usize = 0;
    let mut session_feeds: usize = 0;
    let mut decode_nanos: u128 = 0;
    let mut sum_sq: f64 = 0.0;
    let mut peak: f32 = 0.0;
    let mut last_partial = String::new();
    let mut dump_samples: Vec<f32> = Vec::new();
    let dump_path = std::env::var_os("ASR_DUMP_AUDIO").map(std::path::PathBuf::from);
    if let Some(path) = &dump_path {
        log!("asr", "audio dump enabled -> {}", path.display());
    }
    loop {
        match commands.recv() {
            Ok(Command::Start) => {
                backend.start_session();
                session_samples = 0;
                session_feeds = 0;
                decode_nanos = 0;
                sum_sq = 0.0;
                peak = 0.0;
                last_partial.clear();
                active = true;
                log!("asr", "session start ({mode:?})");
            }
            Ok(Command::Chunk(samples)) => {
                if !active {
                    continue;
                }
                let started = Instant::now();
                backend.feed_audio(&samples);
                decode_nanos += started.elapsed().as_nanos();
                session_feeds += 1;
                session_samples += samples.len();
                if dump_path.is_some() {
                    dump_samples.extend_from_slice(&samples);
                }
                for &sample in &samples {
                    sum_sq += (sample as f64) * (sample as f64);
                    if sample.abs() > peak {
                        peak = sample.abs();
                    }
                }
                if mode == Mode::Live
                    && let Some(partial) = backend.partial()
                    && partial != last_partial
                {
                    last_partial.clone_from(&partial);
                    let _ = events.send(Event::Partial(partial));
                }
            }
            Ok(Command::Stop) => {
                if active {
                    active = false;
                    let finalize_started = Instant::now();
                    let mut text = backend.finalize();
                    let duration_secs = session_samples as f32 / SESSION_SAMPLE_RATE as f32;
                    let audio_secs = session_samples as f64 / SESSION_SAMPLE_RATE as f64;
                    let decode_secs = decode_nanos as f64 / 1e9;
                    let rms = if session_samples > 0 {
                        (sum_sq / session_samples as f64).sqrt()
                    } else {
                        0.0
                    };
                    log!(
                        "asr",
                        "session end: {:.2}s audio, {} feeds, rms {:.4} peak {:.3}, decode wall {:.2}s ({:.1}x realtime), finalize {:.2}s",
                        audio_secs,
                        session_feeds,
                        rms,
                        peak,
                        decode_secs,
                        if audio_secs > 0.0 {
                            decode_secs / audio_secs
                        } else {
                            0.0
                        },
                        finalize_started.elapsed().as_secs_f64()
                    );
                    if mode == Mode::Record && selection.rewrite_record && !text.trim().is_empty() {
                        let rewrite_started = Instant::now();
                        let options = super::rewrite::RewriteOptions {
                            threads: selection.rewrite_threads,
                            gpu_layers: 0,
                        };
                        match super::rewrite::rewrite(&text, &options) {
                            Some(result) if result.ok && !result.text.is_empty() => {
                                text = result.text;
                                log!(
                                    "asr",
                                    "rewrite done in {:.2}s (wall {})",
                                    rewrite_started.elapsed().as_secs_f64(),
                                    result.wall_ms
                                );
                                crate::telemetry::rewrite_end(crate::telemetry::RewriteEnd {
                                    backend: "s1-mini".into(),
                                    device: "cpu".into(),
                                    wall_ms: rewrite_started.elapsed().as_millis() as u64,
                                    in_tokens: result.in_tokens,
                                    out_tokens: result.out_tokens,
                                    ok: result.ok,
                                });
                            }
                            Some(_) => {
                                log!("asr", "rewrite returned nothing usable; keeping ASR text");
                            }
                            None => {
                                log!(
                                    "asr",
                                    "rewrite unavailable (model/executable missing); keeping ASR text"
                                );
                            }
                        }
                    }
                    crate::telemetry::session_end(crate::telemetry::SessionEnd {
                        mode: if mode == Mode::Record {
                            "record".into()
                        } else {
                            "live".into()
                        },
                        audio_secs: audio_secs as f32,
                        decode_wall_ms: (decode_nanos / 1_000_000) as u64,
                        rtf: if audio_secs > 0.0 {
                            decode_secs / audio_secs as f64
                        } else {
                            0.0
                        } as f32,
                        finalize_ms: finalize_started.elapsed().as_millis() as u64,
                        partials_advanced: 0,
                        peak_rss_mb: resident_mib(),
                    });
                    if let Some(path) = &dump_path {
                        let bytes: Vec<u8> = dump_samples
                            .iter()
                            .flat_map(|sample| sample.to_le_bytes())
                            .collect();
                        match std::fs::write(path, &bytes) {
                            Ok(()) => log!(
                                "asr",
                                "audio dump written: {} ({} samples, f32le 16k)",
                                path.display(),
                                dump_samples.len()
                            ),
                            Err(error) => {
                                log!("asr", "ERROR: audio dump write failed: {error}");
                            }
                        }
                        dump_samples.clear();
                    }
                    let _ = events.send(Event::Committed {
                        text,
                        duration_secs,
                    });
                } else {
                    // Never leave the UI in Transcribing: a Stop with no live
                    // session (mic yielded nothing, Start never sent) must
                    // still answer or the pill spins forever and cancel dies.
                    log!(
                        "asr",
                        "stop with no active session; answering empty (mic may have yielded no audio)"
                    );
                    let _ = events.send(Event::Committed {
                        text: String::new(),
                        duration_secs: 0.0,
                    });
                }
            }
            Ok(Command::ReloadForDebug) => {
                let unload_started = Instant::now();
                drop(backend);
                log!(
                    "asr",
                    "unload {mode:?} in {:.2}s, resident {:.0} MiB",
                    unload_started.elapsed().as_secs_f64(),
                    resident_mib()
                );
                let Some(paths) = cached_paths.take() else {
                    log!("asr", "ERROR: no cached model paths for reload");
                    return;
                };
                let reload_started = Instant::now();
                match load_backend(&paths, selection.provider.as_deref(), selection.threads) {
                    Some(reloaded) => {
                        backend = reloaded;
                        cached_paths = Some(paths);
                        log!(
                            "asr",
                            "warm reload {mode:?} in {:.2}s, resident {:.0} MiB",
                            reload_started.elapsed().as_secs_f64(),
                            resident_mib()
                        );
                        let _ = events.send(Event::Ready);
                    }
                    None => {
                        log!("asr", "ERROR: warm reload failed, create returned None");
                        return;
                    }
                }
            }
            Ok(Command::SwitchMode(target)) => {
                let wanted = model_for_mode(selection.record_model, selection.live_model, target);
                if wanted == loaded_kind {
                    log!(
                        "asr",
                        "switch to {target:?}: model {wanted:?} already resident, keeping recognizer"
                    );
                    mode = target;
                    let _ = events.send(Event::ModelReady(mode));
                } else {
                    log!(
                        "asr",
                        "switch to {target:?}: swapping {loaded_kind:?} -> {wanted:?}"
                    );
                    match load_and_swap(
                        target,
                        wanted,
                        &mut active,
                        &mut backend,
                        &mut last_partial,
                        &mut cached_paths,
                        session_samples,
                        &events,
                        selection.provider.as_deref(),
                        selection.threads,
                    ) {
                        SwapOutcome::Swapped | SwapOutcome::FetchFailed => {}
                        SwapOutcome::Fatal => return,
                    }
                    loaded_kind = wanted;
                    mode = target;
                }
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }
}

enum SwapOutcome {
    Swapped,
    FetchFailed,
    Fatal,
}

fn load_and_swap(
    target: Mode,
    kind: ModelKind,
    active: &mut bool,
    backend: &mut Box<dyn AsrBackend>,
    last_partial: &mut String,
    cached: &mut Option<ModelPaths>,
    session_samples: usize,
    events: &Sender<Event>,
    provider: Option<&str>,
    threads: i32,
) -> SwapOutcome {
    if *active {
        *active = false;
        let text = backend.finalize();
        let _ = events.send(Event::Committed {
            text,
            duration_secs: session_samples as f32 / SESSION_SAMPLE_RATE as f32,
        });
    }
    let new_paths = match cached.take() {
        Some(paths) if paths_matches_kind(&paths, kind) => Some(paths),
        _ => fetch_paths(kind, events),
    };
    let Some(new_paths) = new_paths else {
        return SwapOutcome::FetchFailed;
    };
    let _ = events.send(Event::LoadingProgress(format!(
        "loading {kind:?} recognizer"
    )));
    let load_started = Instant::now();
    let Some(new_backend) = load_backend(&new_paths, provider, threads) else {
        log!(
            "asr",
            "ERROR: swap to {kind:?} failed, create returned None"
        );
        return SwapOutcome::Fatal;
    };
    let unload_started = Instant::now();
    drop(std::mem::replace(backend, new_backend));
    log!(
        "asr",
        "swap to {kind:?} for {target:?}: old unload {:.2}s, new load {:.2}s, resident {:.0} MiB",
        unload_started.elapsed().as_secs_f64(),
        load_started.elapsed().as_secs_f64(),
        resident_mib()
    );
    last_partial.clear();
    *cached = Some(new_paths);
    let _ = events.send(Event::ModelReady(target));
    SwapOutcome::Swapped
}

fn fetch_paths(kind: ModelKind, events: &Sender<Event>) -> Option<ModelPaths> {
    let _ = events.send(Event::LoadingProgress(format!(
        "checking {} model",
        kind.spec().display_name
    )));
    match model::ensure_model(
        kind,
        &mut |progress| {
            let _ = events.send(Event::LoadingProgress(model::progress_text(
                kind.spec().display_name,
                &progress,
            )));
        },
        None,
    ) {
        Ok(Some(paths)) => Some(paths),
        Ok(None) => None,
        Err(error) => {
            log!("asr", "ERROR: model fetch failed: {error}");
            let _ = events.send(Event::LoadingProgress("model fetch failed".to_owned()));
            None
        }
    }
}

fn load_backend(
    paths: &ModelPaths,
    provider: Option<&str>,
    threads: i32,
) -> Option<Box<dyn AsrBackend>> {
    match paths {
        ModelPaths::Nemotron { .. } => {
            NemotronBackend::load(paths, provider, threads).map(|backend| Box::new(backend) as _)
        }
        ModelPaths::Moonshine { .. } => {
            MoonshineBackend::load(paths, provider, threads).map(|backend| Box::new(backend) as _)
        }
    }
}

/// The ASR engine wanted for a given mode.
fn model_for_mode(record: ModelKind, live: ModelKind, mode: Mode) -> ModelKind {
    match mode {
        Mode::Record => record,
        Mode::Live => live,
    }
}

fn paths_matches_kind(paths: &ModelPaths, kind: ModelKind) -> bool {
    matches!(
        (paths, kind),
        (ModelPaths::Nemotron { .. }, ModelKind::Nemotron)
            | (ModelPaths::Moonshine { .. }, ModelKind::Moonshine)
    )
}

fn resident_mib() -> f64 {
    use sysinfo::{ProcessesToUpdate, System, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return 0.0;
    };
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .map(|process| process.memory() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0)
}
