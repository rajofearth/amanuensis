mod audio;

use std::{sync::mpsc, time::Duration};

use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, relative, rgb,
    size,
};
use gpui_platform::application;

const BARS: usize = 26;
const POLL_INTERVAL: Duration = Duration::from_millis(16);

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

impl Render for Waveform {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x101010))
            .border_1()
            .border_color(rgb(0x404040))
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .h_full()
                    .w_full()
                    .px(px(24.))
                    .children(self.levels().map(|level| {
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
        let bounds = Bounds::centered(None, size(px(800.), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let (sender, receiver) = mpsc::channel();
                audio::spawn(sender);
                let view = cx.new(|_| Waveform {
                    bars: [0.0; BARS],
                    peak: 0.0,
                });
                window.spawn(cx, {
                    let view = view.clone();
                    async move |cx| {
                    let mut pending: Vec<f32> = Vec::new();
                    loop {
                        while let Ok(rms) = receiver.try_recv() {
                            pending.push(rms);
                        }
                        if !pending.is_empty() {
                            cx.update(|_, cx| {
                                view.update(cx, |waveform, cx| {
                                    waveform.extend(pending.drain(..));
                                    cx.notify();
                                })
                            })
                            .ok();
                        }
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
