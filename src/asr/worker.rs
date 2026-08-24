use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Instant,
};

use crate::log;

use super::model;
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
    SetModel(ModelKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    pub model: ModelKind,
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            model: ModelKind::Nemotron,
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

fn run(commands: Receiver<Command>, events: Sender<Event>, mut selection: ModelSelection) {
    let mut mode = Mode::Record;
    let Some(paths) = fetch_paths(selection.model, &events) else {
        return;
    };

    let _ = events.send(Event::LoadingProgress("loading recognizer".to_owned()));
    let started = Instant::now();
    let Some(mut backend) = load_backend(&paths) else {
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
    if events.send(Event::Ready).is_err() {
        return;
    }

    let mut cached_paths: Option<ModelPaths> = Some(paths);

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
                    let text = backend.finalize();
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
                match load_backend(&paths) {
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
                log!(
                    "asr",
                    "switch to {target:?}: single resident model, keeping recognizer"
                );
                mode = target;
                let _ = events.send(Event::ModelReady(mode));
            }
            Ok(Command::SetModel(kind)) => {
                if kind == selection.model {
                    log!("asr", "set model: already {kind:?}, nothing to do");
                    continue;
                }
                log!("asr", "set model: {:?} -> {:?}", selection.model, kind);
                selection.model = kind;
                cached_paths = None;
                match load_and_swap(
                    mode,
                    kind,
                    &mut active,
                    &mut backend,
                    &mut last_partial,
                    &mut cached_paths,
                    session_samples,
                    &events,
                ) {
                    SwapOutcome::Swapped | SwapOutcome::FetchFailed => {}
                    SwapOutcome::Fatal => return,
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
        Some(paths) => Some(paths),
        None => fetch_paths(kind, events),
    };
    let Some(new_paths) = new_paths else {
        return SwapOutcome::FetchFailed;
    };
    let _ = events.send(Event::LoadingProgress(format!(
        "loading {kind:?} recognizer"
    )));
    let load_started = Instant::now();
    let Some(new_backend) = load_backend(&new_paths) else {
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
    let _ = events.send(Event::LoadingProgress(format!("fetching {kind:?} model")));
    match model::ensure_model(kind, &mut |file| {
        let _ = events.send(Event::LoadingProgress(format!("downloading {file}")));
    }) {
        Ok(paths) => Some(paths),
        Err(error) => {
            log!("asr", "ERROR: model fetch failed: {error}");
            let _ = events.send(Event::LoadingProgress("model fetch failed".to_owned()));
            None
        }
    }
}

fn load_backend(paths: &ModelPaths) -> Option<Box<dyn AsrBackend>> {
    NemotronBackend::load(paths).map(|backend| Box::new(backend) as _)
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
