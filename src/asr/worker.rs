use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Instant,
};

use super::{AsrBackend, Mode, ModelKind, ModelPaths};
use super::model;
use super::nemotron::NemotronBackend;
use super::unified::UnifiedBackend;

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

pub enum Event {
    LoadingProgress(String),
    Ready,
    ModelReady(Mode),
    Partial(String),
    Committed { text: String, duration_secs: f32 },
}

const SESSION_SAMPLE_RATE: usize = 16_000;

pub fn spawn_worker(events: Sender<Event>) -> Sender<Command> {
    let (commands, receiver) = channel::<Command>();
    thread::Builder::new()
        .name("asr-worker".to_owned())
        .spawn(move || run(receiver, events))
        .expect("failed to spawn asr worker thread");
    commands
}

fn run(commands: Receiver<Command>, events: Sender<Event>) {
    let mut mode = Mode::Record;
    let Some(paths) = fetch_paths(ModelKind::from(mode), &events) else {
        return;
    };

    let _ = events.send(Event::LoadingProgress("loading recognizer".to_owned()));
    let started = Instant::now();
    let Some(mut backend) = load_backend(mode, &paths) else {
        eprintln!("asr: OnlineRecognizer::create returned None");
        let _ = events.send(Event::LoadingProgress("load failed".to_owned()));
        return;
    };
    println!(
        "asr: cold load {mode:?} {:.2}s resident {:.0} MiB",
        started.elapsed().as_secs_f64(),
        resident_mib()
    );
    if events.send(Event::Ready).is_err() {
        return;
    }

    let (mut record_paths, mut live_paths) = match mode {
        Mode::Record => (Some(paths), None),
        Mode::Live => (None, Some(paths)),
    };

    let mut active = false;
    let mut session_samples: usize = 0;
    let mut last_partial = String::new();
    loop {
        match commands.recv() {
            Ok(Command::Start) => {
                backend.start_session();
                session_samples = 0;
                last_partial.clear();
                active = true;
            }
            Ok(Command::Chunk(samples)) => {
                if !active {
                    continue;
                }
                backend.feed_audio(&samples);
                session_samples += samples.len();
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
                    let text = backend.finalize();
                    let _ = events.send(Event::Committed {
                        text,
                        duration_secs: session_samples as f32 / SESSION_SAMPLE_RATE as f32,
                    });
                }
            }
            Ok(Command::ReloadForDebug) => {
                let unload_started = Instant::now();
                drop(backend);
                println!(
                    "asr: unload {mode:?} {:.2}s resident {:.0} MiB",
                    unload_started.elapsed().as_secs_f64(),
                    resident_mib()
                );
                let cached = match mode {
                    Mode::Record => &mut record_paths,
                    Mode::Live => &mut live_paths,
                };
                let Some(paths) = cached.take() else {
                    eprintln!("asr: no cached model paths for reload");
                    return;
                };
                let reload_started = Instant::now();
                match load_backend(mode, &paths) {
                    Some(reloaded) => {
                        backend = reloaded;
                        *cached = Some(paths);
                        println!(
                            "asr: warm reload {mode:?} {:.2}s resident {:.0} MiB",
                            reload_started.elapsed().as_secs_f64(),
                            resident_mib()
                        );
                        let _ = events.send(Event::Ready);
                    }
                    None => {
                        eprintln!("asr: warm reload failed, create returned None");
                        return;
                    }
                }
            }
            Ok(Command::SwitchMode(target)) => {
                if target == mode {
                    continue;
                }
                if active {
                    active = false;
                    let text = backend.finalize();
                    let _ = events.send(Event::Committed {
                        text,
                        duration_secs: session_samples as f32 / SESSION_SAMPLE_RATE as f32,
                    });
                }
                let cached = match target {
                    Mode::Record => &mut record_paths,
                    Mode::Live => &mut live_paths,
                };
                let new_paths = match cached.take() {
                    Some(paths) => Some(paths),
                    None => fetch_paths(ModelKind::from(target), &events),
                };
                let Some(new_paths) = new_paths else {
                    continue;
                };
                let _ = events.send(Event::LoadingProgress(format!(
                    "loading {:?} recognizer",
                    target
                )));
                let load_started = Instant::now();
                let Some(new_backend) = load_backend(target, &new_paths) else {
                    eprintln!("asr: switch to {target:?} failed, create returned None");
                    return;
                };
                let unload_started = Instant::now();
                drop(std::mem::replace(&mut backend, new_backend));
                println!(
                    "asr: switch {mode:?} -> {target:?}: old unload {:.2}s new load {:.2}s resident {:.0} MiB",
                    unload_started.elapsed().as_secs_f64(),
                    load_started.elapsed().as_secs_f64(),
                    resident_mib()
                );
                mode = target;
                last_partial.clear();
                *cached = Some(new_paths);
                let _ = events.send(Event::ModelReady(mode));
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }
}

fn fetch_paths(kind: ModelKind, events: &Sender<Event>) -> Option<ModelPaths> {
    let _ = events.send(Event::LoadingProgress(format!("fetching {kind:?} model")));
    match model::ensure_model(kind, &mut |file| {
        let _ = events.send(Event::LoadingProgress(format!("downloading {file}")));
    }) {
        Ok(paths) => Some(paths),
        Err(error) => {
            eprintln!("asr: {error}");
            let _ = events.send(Event::LoadingProgress("model fetch failed".to_owned()));
            None
        }
    }
}

fn load_backend(mode: Mode, paths: &ModelPaths) -> Option<Box<dyn AsrBackend>> {
    match mode {
        Mode::Record => UnifiedBackend::load(paths).map(|backend| Box::new(backend) as _),
        Mode::Live => NemotronBackend::load(paths).map(|backend| Box::new(backend) as _),
    }
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
