use std::{
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Instant,
};

use super::AsrBackend;
use super::model;
use super::nemotron::NemotronBackend;

#[derive(Debug)]
pub enum Command {
    Start,
    Stop,
    Chunk(Vec<f32>),
    #[allow(dead_code)]
    Shutdown,
    ReloadForDebug,
}

pub enum Event {
    LoadingProgress(String),
    Ready,
    Partial(String),
    Committed(String),
}

pub fn spawn_worker(events: Sender<Event>) -> Sender<Command> {
    let (commands, receiver) = channel::<Command>();
    thread::Builder::new()
        .name("asr-worker".to_owned())
        .spawn(move || run(receiver, events))
        .expect("failed to spawn asr worker thread");
    commands
}

fn run(commands: Receiver<Command>, events: Sender<Event>) {
    let _ = events.send(Event::LoadingProgress("fetching model".to_owned()));
    let paths = match model::ensure_model(&mut |file| {
        let _ = events.send(Event::LoadingProgress(format!("downloading {file}")));
    }) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("asr: {error}");
            let _ = events.send(Event::LoadingProgress("model fetch failed".to_owned()));
            return;
        }
    };

    let _ = events.send(Event::LoadingProgress("loading recognizer".to_owned()));
    let started = Instant::now();
    let Some(mut backend) = NemotronBackend::load(&paths) else {
        eprintln!("asr: OnlineRecognizer::create returned None");
        let _ = events.send(Event::LoadingProgress("load failed".to_owned()));
        return;
    };
    println!(
        "asr: cold load {:.2}s resident {:.0} MiB",
        started.elapsed().as_secs_f64(),
        resident_mib()
    );
    if events.send(Event::Ready).is_err() {
        return;
    }

    let mut active = false;
    let mut last_partial = String::new();
    loop {
        match commands.recv() {
            Ok(Command::Start) => {
                backend.start_session();
                last_partial.clear();
                active = true;
            }
            Ok(Command::Chunk(samples)) => {
                if !active {
                    continue;
                }
                backend.feed_audio(&samples);
                if let Some(partial) = backend.partial()
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
                    let _ = events.send(Event::Committed(text));
                }
            }
            Ok(Command::ReloadForDebug) => {
                let unload_started = Instant::now();
                drop(backend);
                println!(
                    "asr: unload {:.2}s resident {:.0} MiB",
                    unload_started.elapsed().as_secs_f64(),
                    resident_mib()
                );
                let reload_started = Instant::now();
                match NemotronBackend::load(&paths) {
                    Some(reloaded) => {
                        backend = reloaded;
                        println!(
                            "asr: warm reload {:.2}s resident {:.0} MiB",
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
            Ok(Command::Shutdown) | Err(_) => break,
        }
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
