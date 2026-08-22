mod audio;

use std::{
    sync::mpsc,
    thread,
    time::Duration,
};

use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey},
};
use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb,
    size,
};
use gpui_platform::application;

const BARS: usize = 26;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PLACEHOLDER_DELAY: Duration = Duration::from_secs(1);

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
    #[allow(dead_code)]
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
}

impl Dictation {
    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Idle => self.phase = Phase::Recording,
            Phase::Recording => {
                self.phase = Phase::Transcribing;
                cx.spawn(async move |dictation, cx| {
                    cx.background_executor().timer(PLACEHOLDER_DELAY).await;
                    dictation
                        .update(cx, |dictation, cx| {
                            if dictation.phase == Phase::Transcribing {
                                dictation.phase = Phase::Idle;
                                cx.notify();
                            }
                        })
                        .ok();
                })
                .detach();
            }
            Phase::Loading | Phase::Transcribing => {}
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
                    .child(format!("{:?} (F9)", self.phase))
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
                            .bg(rgb(0x33cc66))
                    })),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let manager =
            GlobalHotKeyManager::new().expect("failed to create global hotkey manager");
        manager
            .register(HotKey::new(None, Code::F9))
            .expect("failed to register F9");
        std::mem::forget(manager);

        let (hotkey_sender, hotkey_receiver) = mpsc::channel::<()>();
        thread::spawn(move || loop {
            match GlobalHotKeyEvent::receiver().try_recv() {
                Ok(event) => {
                    if event.state() == HotKeyState::Pressed {
                        let _ = hotkey_sender.send(());
                    }
                }
                Err(_) => thread::sleep(HOTKEY_POLL_INTERVAL),
            }
        });

        let bounds = Bounds::centered(None, size(px(800.), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let (sender, receiver) = mpsc::channel();
                audio::spawn(sender);
                let view = cx.new(|_| Dictation {
                    phase: Phase::Idle,
                    mode: Mode::Record,
                    waveform: Waveform {
                        bars: [0.0; BARS],
                        peak: 0.0,
                    },
                });
                window.spawn(cx, {
                    let view = view.clone();
                    async move |cx| {
                        let mut pending: Vec<f32> = Vec::new();
                        loop {
                            while let Ok(rms) = receiver.try_recv() {
                                pending.push(rms);
                            }
                            while hotkey_receiver.try_recv().is_ok() {
                                cx.update(|_, cx| {
                                    view.update(cx, |dictation, cx| {
                                        dictation.toggle_recording(cx)
                                    });
                                })
                                .ok();
                            }
                            cx.update(|_, cx| {
                                if view.read(cx).phase != Phase::Recording {
                                    pending.clear();
                                } else if !pending.is_empty() {
                                    view.update(cx, |dictation, cx| {
                                        dictation.waveform.extend(pending.drain(..));
                                        cx.notify();
                                    });
                                }
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
