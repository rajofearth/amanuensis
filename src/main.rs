mod asr;
mod audio;

use std::{sync::mpsc, thread, time::Duration};

use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey},
};
use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb,
    size,
};
use gpui_platform::application;

use asr::{Command, Event};

const BARS: usize = 26;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(50);

enum HotkeyMessage {
    ToggleRecording,
    DebugReload,
}

struct Waveform {
    bars: [f32; BARS],
    peak: f32,
}

impl Waveform {
    fn push(&mut self, rms: f32) {
        self.bars.copy_within(1.., 0);
        *self.bars.last_mut().unwrap() = rms;
        if rms > self.peak {
            self.peak = rms;
        } else {
            self.peak *= 0.995;
        }
    }

    fn extend(&mut self, values: impl IntoIterator<Item = f32>) {
        for value in values {
            self.push(value);
        }
    }

    fn levels(&self) -> impl Iterator<Item = f32> + '_ {
        let scale = if self.peak > f32::EPSILON {
            1.0 / self.peak
        } else {
            0.0
        };
        self.bars.iter().map(move |value| value * scale)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Loading,
    Idle,
    Recording,
    Transcribing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Record,
    Live,
}

struct Dictation {
    phase: Phase,
    mode: Mode,
    waveform: Waveform,
    commands: mpsc::Sender<Command>,
    status: String,
    partial: String,
    committed: String,
    pending_start: bool,
}

impl Dictation {
    fn begin_recording(&mut self) {
        self.phase = Phase::Recording;
        self.partial.clear();
        let _ = self.commands.send(Command::Start);
    }

    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Loading => self.pending_start = true,
            Phase::Idle => self.begin_recording(),
            Phase::Recording => {
                self.phase = Phase::Transcribing;
                let _ = self.commands.send(Command::Stop);
            }
            Phase::Transcribing => {}
        }
        cx.notify();
    }

    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            Mode::Record => Mode::Live,
            Mode::Live => Mode::Record,
        };
        cx.notify();
    }

    fn handle_asr_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::LoadingProgress(message) => self.status = message,
            Event::Ready => {
                if self.phase == Phase::Loading && self.pending_start {
                    self.pending_start = false;
                    self.begin_recording();
                } else if self.phase == Phase::Loading {
                    self.phase = Phase::Idle;
                }
            }
            Event::Partial(text) => {
                if self.phase == Phase::Recording {
                    self.partial = text;
                }
            }
            Event::Committed(text) => {
                if self.phase == Phase::Transcribing {
                    self.phase = Phase::Idle;
                    self.committed = text;
                    self.partial.clear();
                }
            }
        }
        cx.notify();
    }

    fn handle_hotkey(&mut self, message: HotkeyMessage, cx: &mut Context<Self>) {
        match message {
            HotkeyMessage::ToggleRecording => self.toggle_recording(cx),
            HotkeyMessage::DebugReload => {
                if self.phase == Phase::Idle {
                    let _ = self.commands.send(Command::ReloadForDebug);
                }
            }
        }
    }
}

impl Render for Dictation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .bg(rgb(0x101010))
            .border_1()
            .border_color(rgb(0x404040))
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .text_size(px(11.))
                    .text_color(rgb(0x909090))
                    .child(if self.phase == Phase::Loading || !self.status.is_empty() {
                        format!("{:?} (F9): {}", self.phase, self.status)
                    } else {
                        format!("{:?} (F9)", self.phase)
                    })
                    .child(
                        div()
                            .id("mode")
                            .cursor_pointer()
                            .rounded_sm()
                            .px(px(8.))
                            .py(px(2.))
                            .bg(rgb(0x1c1c1c))
                            .text_color(if self.mode == Mode::Live {
                                rgb(0xcc3333)
                            } else {
                                rgb(0x33cc66)
                            })
                            .child(format!("{:?}", self.mode))
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_mode(cx))),
                    ),
            )
            .children((self.phase == Phase::Recording && !self.partial.is_empty()).then(|| {
                div()
                    .max_w(px(720.))
                    .text_size(px(12.))
                    .text_color(rgb(0xcccccc))
                    .child(format!("partial: {}", self.partial))
            }))
            .children((!self.committed.is_empty()).then(|| {
                div()
                    .max_w(px(720.))
                    .text_size(px(12.))
                    .text_color(rgb(0x33cc66))
                    .child(format!("committed: {}", self.committed))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .h(relative(0.7))
                    .w_full()
                    .px(px(24.))
                    .children(self.waveform.levels().map(|level| {
                        div()
                            .flex_1()
                            .h(relative(level.clamp(0.02, 1.0)))
                            .rounded_sm()
                            .bg(if self.phase == Phase::Recording {
                                rgb(0x33cc66)
                            } else {
                                rgb(0x404040)
                            })
                    })),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let manager = GlobalHotKeyManager::new().expect("failed to create global hotkey manager");
        let toggle_key = HotKey::new(None, Code::F9);
        let debug_key = HotKey::new(None, Code::F10);
        manager.register(toggle_key).expect("failed to register F9");
        manager.register(debug_key).expect("failed to register F10");
        std::mem::forget(manager);

        let (hotkey_sender, hotkey_receiver) = mpsc::channel::<HotkeyMessage>();
        thread::spawn(move || {
            loop {
                match GlobalHotKeyEvent::receiver().try_recv() {
                    Ok(event) => {
                        if event.state() == HotKeyState::Pressed {
                            let message = if event.id == toggle_key.id() {
                                Some(HotkeyMessage::ToggleRecording)
                            } else if event.id == debug_key.id() {
                                Some(HotkeyMessage::DebugReload)
                            } else {
                                None
                            };
                            if let Some(message) = message {
                                let _ = hotkey_sender.send(message);
                            }
                        }
                    }
                    Err(_) => thread::sleep(HOTKEY_POLL_INTERVAL),
                }
            }
        });

        let bounds = Bounds::centered(None, size(px(800.), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let (audio_sender, audio_receiver) = mpsc::channel::<Vec<f32>>();
                audio::spawn(audio_sender);

                let (event_sender, event_receiver) = mpsc::channel::<Event>();
                let command_sender = asr::spawn_worker(event_sender);

                let view = cx.new(|_| Dictation {
                    phase: Phase::Loading,
                    mode: Mode::Record,
                    waveform: Waveform {
                        bars: [0.0; BARS],
                        peak: 0.0,
                    },
                    commands: command_sender,
                    status: "fetching model".to_owned(),
                    partial: String::new(),
                    committed: String::new(),
                    pending_start: false,
                });
                window
                    .spawn(cx, {
                        let view = view.clone();
                        async move |cx| {
                            let mut pending_levels: Vec<f32> = Vec::new();
                            let mut pending_chunks: Vec<Vec<f32>> = Vec::new();
                            loop {
                                while let Ok(chunk) = audio_receiver.try_recv() {
                                    pending_levels.push(rms_level(&chunk));
                                    pending_chunks.push(chunk);
                                }
                                while let Ok(event) = event_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        view.update(cx, |dictation, cx| {
                                            dictation.handle_asr_event(event, cx)
                                        });
                                    })
                                    .ok();
                                }
                                while let Ok(message) = hotkey_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        view.update(cx, |dictation, cx| {
                                            dictation.handle_hotkey(message, cx)
                                        });
                                    })
                                    .ok();
                                }
                                cx.update(|_, cx| {
                                    view.update(cx, |dictation, cx| {
                                        if dictation.phase == Phase::Recording {
                                            dictation.waveform.extend(pending_levels.drain(..));
                                            for chunk in pending_chunks.drain(..) {
                                                let _ =
                                                    dictation.commands.send(Command::Chunk(chunk));
                                            }
                                        } else {
                                            pending_levels.clear();
                                            pending_chunks.clear();
                                        }
                                        cx.notify();
                                    });
                                })
                                .ok();
                                cx.background_executor().timer(POLL_INTERVAL).await;
                            }
                        }
                    })
                    .detach();
                view
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}
