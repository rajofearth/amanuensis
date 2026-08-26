use std::sync::mpsc;
use std::time::Instant;

use amanuensis::asr::fetch::{
    EtaTracker, path_is_dir, progress_status, progress_summary, speed_summary,
};
use amanuensis::asr::{
    DownloadProgress, ModelSpec, cache_dir_for, is_model_cached, kind_by_id, repo_cache_dir_for,
    spec_by_id,
};
use amanuensis::log;
use amanuensis::setup_steps::{SetupStep, StepEvent, next_step};
use gpui::{
    App, Context, Div, IntoElement, Render, Stateful, Window, div, prelude::*, px, relative, rgb,
};

use crate::messages::UiMessage;
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SetupOrigin {
    FirstRun,
    Recovery,
    Settings,
    Respawn,
}

pub(crate) const RECOVERY_NOTICE: &str = "Cached model not found — download it below to continue.";
pub(crate) const BLOCKED_NOTICE: &str = "Model not downloaded — download it in setup first.";

pub(crate) struct OnboardingView {
    origin: SetupOrigin,
    model_id: &'static str,
    ram_gb: u32,
    cores: usize,
    status: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    busy: bool,
    downloading: bool,
    cancel_requested: bool,
    progress: Option<DownloadProgress>,
    eta: EtaTracker,
    step: Option<SetupStep>,
    start_queued: bool,
    tray_enabled: bool,
    mic_level: f32,
    mic_peak: f32,
    ui: mpsc::Sender<UiMessage>,
}

impl OnboardingView {
    pub(crate) fn new(
        origin: SetupOrigin,
        device: (u32, usize),
        model_id: &'static str,
        tray_enabled: bool,
        ui: mpsc::Sender<UiMessage>,
    ) -> Self {
        Self {
            origin,
            model_id,
            ram_gb: device.0,
            cores: device.1,
            status: None,
            error: None,
            notice: None,
            busy: false,
            downloading: false,
            cancel_requested: false,
            progress: None,
            eta: EtaTracker::new(),
            step: (origin == SetupOrigin::FirstRun).then_some(SetupStep::Welcome),
            start_queued: false,
            tray_enabled,
            mic_level: 0.0,
            mic_peak: 0.0,
            ui,
        }
    }

    fn toggle_tray(&mut self, cx: &mut Context<Self>) {
        self.tray_enabled = !self.tray_enabled;
        let _ = self.ui.send(UiMessage::TraySetEnabled(self.tray_enabled));
        cx.notify();
    }

    pub(crate) fn model_ready(&self) -> bool {
        is_model_cached(self.model_id)
    }

    pub(crate) fn model_dir_exists(&self) -> bool {
        kind_by_id(self.model_id)
            .and_then(repo_cache_dir_for)
            .is_some_and(|dir| path_is_dir(&dir))
    }

    pub(crate) fn step_active(&self) -> bool {
        self.step.is_some()
    }

    pub(crate) fn set_blocked_notice(&mut self, cx: &mut Context<Self>) {
        if self.notice.is_none() {
            self.notice = Some(BLOCKED_NOTICE.to_owned());
            cx.notify();
        }
    }

    pub(crate) fn queue_start(&mut self, cx: &mut Context<Self>) {
        self.start_queued = true;
        cx.notify();
    }

    fn nav(&mut self, event: StepEvent, cx: &mut Context<Self>) {
        if let Some(step) = self.step {
            self.step = next_step(step, event);
            cx.notify();
        }
    }

    pub(crate) fn mic_check_active(&self) -> bool {
        self.step == Some(SetupStep::MicCheck)
    }

    pub(crate) fn push_mic_levels(&mut self, levels: &[f32], cx: &mut Context<Self>) {
        let instant = levels.iter().copied().fold(0.0_f32, f32::max);
        if self.mic_peak > f32::EPSILON {
            self.mic_peak *= 0.995;
        }
        if instant > self.mic_peak {
            self.mic_peak = instant;
        }
        let target = if self.mic_peak > f32::EPSILON {
            (instant / self.mic_peak).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.mic_level = self.mic_level * 0.7 + target * 0.3;
        cx.notify();
    }

    pub(crate) fn download_progress(&mut self, progress: DownloadProgress, cx: &mut Context<Self>) {
        self.eta.push(progress.done, Instant::now());
        self.progress = Some(progress);
        cx.notify();
    }

    pub(crate) fn download_cancelled(&mut self, cx: &mut Context<Self>) {
        log!("app", "download cancelled");
        self.busy = false;
        self.downloading = false;
        self.cancel_requested = false;
        self.progress = None;
        self.eta.clear();
        if let Some(step) = self.step {
            self.step = next_step(step, StepEvent::DownloadCancelled);
            self.status = Some("Cancelled — it will resume next time.".to_owned());
        } else {
            self.status = Some(
                "Cancelled — progress saved; the next Start resumes from the same byte offset."
                    .to_owned(),
            );
        }
        cx.notify();
    }

    pub(crate) fn download_failed(&mut self, error: String, cx: &mut Context<Self>) {
        log!("app", "download FAILED: {error}");
        self.busy = false;
        self.downloading = false;
        self.cancel_requested = false;
        self.progress = None;
        self.eta.clear();
        if let Some(step) = self.step {
            self.step = next_step(step, StepEvent::DownloadFailed);
        }
        self.error = Some(error);
        cx.notify();
    }

    pub(crate) fn download_finished_step(&mut self, cx: &mut Context<Self>) {
        log!("app", "setup download finished");
        self.busy = false;
        self.downloading = false;
        self.cancel_requested = false;
        self.progress = None;
        self.eta.clear();
        if let Some(step) = self.step {
            self.step = next_step(step, StepEvent::DownloadFinished);
        }
        self.status = None;
        cx.notify();
    }

    pub(crate) fn delete_finished(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        self.busy = false;
        match result {
            Ok(()) => {
                log!(
                    "app",
                    "model deleted; any loaded recognizer was released first"
                );
                self.status = Some(
                    "Deleted. Use Download again or Start dictating to re-fetch it.".to_owned(),
                );
            }
            Err(error) => {
                log!("app", "delete FAILED: {error}");
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    pub(crate) fn start_clicked(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            log!("app", "start ignored: operation already in progress");
            return;
        }
        let Some(spec) = spec_by_id(self.model_id) else {
            return;
        };
        self.busy = true;
        self.downloading = true;
        self.cancel_requested = false;
        self.error = None;
        self.notice = None;
        self.progress = None;
        self.eta.clear();
        self.status = Some(format!("checking {} …", spec.display_name));
        log!("app", "download requested: {}", spec.id);
        if let Some(step) = self.step {
            self.step = next_step(step, StepEvent::StartDownload);
        }
        let _ = self.ui.send(UiMessage::StartDownload {
            captured_model: self.model_id,
            purge: false,
        });
        cx.notify();
    }

    pub(crate) fn reveal_clicked(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            log!("app", "reveal ignored: operation already in progress");
            return;
        }
        let Some(kind) = kind_by_id(self.model_id) else {
            return;
        };
        let Some(dir) = cache_dir_for(kind) else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let dir = match std::path::absolute(&dir) {
            Ok(absolute) => absolute,
            Err(error) => {
                log!(
                    "app",
                    "explorer launch FAILED: cannot absolutize {}: {error}",
                    dir.display()
                );
                self.error = Some(format!("cannot resolve model dir: {error}"));
                cx.notify();
                return;
            }
        };
        let Some(dir_str) = dir.to_str() else {
            self.error = Some("model dir path is not valid Unicode".to_owned());
            cx.notify();
            return;
        };
        log!("app", "revealing model dir: {dir_str}");
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg(dir_str)
            .spawn()
        {
            log!("app", "explorer launch FAILED: {error}");
            self.error = Some(format!("explorer launch failed: {error}"));
        }
        cx.notify();
    }

    pub(crate) fn delete_clicked(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            log!("app", "delete ignored: operation already in progress");
            return;
        }
        let Some(spec) = spec_by_id(self.model_id) else {
            return;
        };
        self.busy = true;
        self.error = None;
        self.status = Some(if self.model_ready() {
            "releasing model and deleting files …".to_owned()
        } else {
            "deleting model files …".to_owned()
        });
        log!("app", "delete requested: {}", spec.id);
        let _ = self.ui.send(UiMessage::DeleteRequest {
            captured_model: self.model_id,
        });
        cx.notify();
    }

    pub(crate) fn redownload_clicked(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            log!(
                "app",
                "download-again ignored: operation already in progress"
            );
            return;
        }
        let Some(spec) = spec_by_id(self.model_id) else {
            return;
        };
        let Some(kind) = kind_by_id(spec.id) else {
            return;
        };
        if repo_cache_dir_for(kind).is_none() {
            return;
        }
        self.busy = true;
        self.downloading = true;
        self.cancel_requested = false;
        self.error = None;
        self.notice = None;
        self.progress = None;
        self.eta.clear();
        self.status = Some(format!("re-downloading {} …", spec.display_name));
        log!("app", "download-again requested: {}", spec.id);
        let _ = self.ui.send(UiMessage::StartDownload {
            captured_model: self.model_id,
            purge: true,
        });
        cx.notify();
    }

    pub(crate) fn render_step(
        &self,
        step: SetupStep,
        spec: &ModelSpec,
        cx: &mut Context<Self>,
    ) -> Div {
        let base = || {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(24.))
                .bg(rgb(0x0d0e10))
                .text_color(rgb(0xe5e7eb))
                .size_full()
        };
        match step {
            SetupStep::Welcome => base()
                .child(div().text_size(px(26.)).child("Welcome to Amanuensis"))
                .child(
                    div()
                        .text_size(px(14.))
                        .child(
                            "Press F9 anywhere, speak, and your words become text in whatever app you're using.",
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x909090))
                        .child(format!(
                            "One-time setup downloads a voice model (~{} MB). After that, everything runs on your PC — nothing leaves it.",
                            spec.size_mb
                        )),
                )
                .children((spec.min_ram_gb > self.ram_gb).then(|| {
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0xcc9933))
                        .child("Your PC has less memory than recommended — it may run slowly.")
                }))
                .children(self.error.clone().map(|error| {
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0xcc3333))
                        .child(format!("Something went wrong: {error}"))
                }))
                .child(tour_footer(
                    None,
                    primary_button(
                        "tour-welcome-next",
                        "Next",
                        cx.listener(|this, _, _, cx| this.nav(StepEvent::NavNext, cx)),
                    ),
                )),
            SetupStep::HowItWorks => base()
                .child(div().text_size(px(26.)).child("How it works"))
                .child(numbered_row(1, "F9 starts your microphone"))
                .child(numbered_row(
                    2,
                    "A voice model running on your PC transcribes as you speak — audio never leaves your machine",
                ))
                .child(numbered_row(
                    3,
                    "Filler words are cleaned up, then the text is pasted wherever your cursor is",
                ))
                .child(tour_footer(
                    Some(action_button("tour-how-back", "Back", true).on_click(cx.listener(
                        |this, _, _, cx| this.nav(StepEvent::NavBack, cx),
                    ))),
                    primary_button(
                        "tour-how-next",
                        "Next",
                        cx.listener(|this, _, _, cx| this.nav(StepEvent::NavNext, cx)),
                    ),
                )),
            SetupStep::Shortcuts => base()
                .child(div().text_size(px(26.)).child("Your shortcuts"))
                .child(shortcut_key_row("F9", "Start / stop dictation"))
                .child(shortcut_key_row("Esc", "Discard an active recording"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x909090))
                                .child("Pill hover"),
                        )
                        .child(div().text_size(px(14.)).child("✕ discard · ✓ finish")),
                )
                .child(div().text_size(px(13.)).child("Right-click the pill — open settings"))
                .child(
                    div()
                        .text_size(px(13.))
                        .child("Tray icon — left-click opens settings"),
                )
                .child(tour_footer(
                    Some(action_button("tour-shortcuts-back", "Back", true).on_click(
                        cx.listener(|this, _, _, cx| this.nav(StepEvent::NavBack, cx)),
                    )),
                    primary_button(
                        "tour-shortcuts-mic",
                        "Test my microphone",
                        cx.listener(|this, _, _, cx| this.nav(StepEvent::NavNext, cx)),
                    ),
                )),
            SetupStep::MicCheck => {
                let fraction = self.mic_level.clamp(0.0, 1.0);
                base()
                    .child(div().text_size(px(26.)).child("Mic check"))
                    .child(
                        div()
                            .text_size(px(14.))
                            .child("Say something — the bar should move."),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(12.))
                            .bg(rgb(0x17191d))
                            .child(
                                div()
                                    .h(px(12.))
                                    .bg(rgb(0xd8d8d8))
                                    .w(relative(fraction)),
                            ),
                    )
                    .children(self.status.clone().map(|status| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0xd8d8d8))
                            .child(status)
                    }))
                    .children(self.error.clone().map(|error| {
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0xcc3333))
                            .child(format!("Something went wrong: {error}"))
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(text_button(
                                "tour-mic-skip",
                                "Skip",
                                cx.listener(|this, _, _, cx| this.start_clicked(cx)),
                            ))
                            .child(primary_button(
                                "tour-mic-continue",
                                "Looks good — continue",
                                cx.listener(|this, _, _, cx| this.start_clicked(cx)),
                            )),
                    )
            }
            SetupStep::Downloading => {
                let eta = self
                    .progress
                    .as_ref()
                    .and_then(|progress| self.eta.estimate(progress.done, progress.total));
                let speed = self
                    .progress
                    .as_ref()
                    .and_then(|progress| progress.bytes_per_sec)
                    .map(speed_summary);
                base()
                    .child(div().text_size(px(20.)).child("Setting up your voice model…"))
                    .children(self.progress.as_ref().map(progress_bar))
                    .children(self.progress.as_ref().map(|progress| {
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0xd8d8d8))
                            .child(progress_summary(
                                progress.done,
                                progress.total,
                                speed.as_deref(),
                                eta.as_deref(),
                            ))
                    }))
                    .children((self.busy && self.downloading).then(|| {
                        action_button(
                            "cancel",
                            if self.cancel_requested {
                                "Cancelling…"
                            } else {
                                "Cancel"
                            },
                            !self.cancel_requested,
                        )
                        .on_click(cx.listener(|this, _, _, _| {
                            if this.cancel_requested {
                                log!("app", "cancel ignored: already requested");
                                return;
                            }
                            log!("app", "cancel requested");
                            let _ = this.ui.send(UiMessage::CancelDownload);
                        }))
                    }))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0x909090))
                            .child("You can cancel — it resumes where it left off."),
                    )
            }
            SetupStep::Ready => base()
                .child(div().text_size(px(26.)).child("You're all set — try it."))
                .child(
                    div()
                        .text_size(px(14.))
                        .child("Hold F9 and speak — your words will land in the last app you used."),
                )
                .children(self.notice.clone().map(|notice| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xcc9933))
                        .child(notice)
                }))
                .children(self.start_queued.then(|| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xcc9933))
                        .child("F9 pressed — recording will begin once models load")
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(primary_button(
                            "begin",
                            "Start using Amanuensis",
                            cx.listener(|this, _, _, _| {
                                let _ = this.ui.send(UiMessage::FinishOnboarding {
                                    captured_model: this.model_id,
                                });
                            }),
                        ))
                        .child(action_button("try-now", "Try it now (F9)", true).on_click(
                            cx.listener(|this, _, _, _| {
                                let _ = this.ui.send(UiMessage::QueueDictationStart);
                            }),
                        )),
                ),
        }
    }
}
impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(spec) = spec_by_id(self.model_id) else {
            return div()
                .size_full()
                .bg(rgb(0x0d0e10))
                .text_color(rgb(0xe5e7eb))
                .child("unknown model");
        };
        if let Some(step) = self.step {
            return self.render_step(step, spec, cx);
        }
        let cached_now = is_model_cached(spec.id);
        let dir_exists = self.model_dir_exists();
        let download_label = if cached_now {
            "Download again"
        } else {
            "Download"
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(24.))
            .bg(rgb(0x0d0e10))
            .text_color(rgb(0xe5e7eb))
            .size_full()
            .child(div().text_size(px(18.)).child("Amanuensis settings"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0x909090))
                    .child(format!(
                        "detected: {} GB RAM, {} logical cores",
                        self.ram_gb, self.cores
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_1()
                    .border_color(rgb(0x2e2e2e))
                    .px(px(12.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(13.)).child("Windows tray icon"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x808080))
                                    .child("Left-click opens settings; right-click shows actions."),
                            ),
                    )
                    .child(
                        div()
                            .id("tray-toggle")
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(0x505050))
                            .px(px(10.))
                            .py(px(4.))
                            .bg(if self.tray_enabled {
                                rgb(0x2a2a2a)
                            } else {
                                rgb(0x171717)
                            })
                            .text_size(px(11.))
                            .text_color(if self.tray_enabled {
                                rgb(0xf2f2f2)
                            } else {
                                rgb(0x707070)
                            })
                            .child(if self.tray_enabled { "ON" } else { "OFF" })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_tray(cx))),
                    ),
            )
            .children(
                ((self.origin == SetupOrigin::Recovery) && !self.downloading).then(|| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xcc9933))
                        .child(RECOVERY_NOTICE)
                }),
            )
            .children(
                self.notice
                    .clone()
                    .filter(|_| !cached_now && !self.downloading)
                    .map(|notice| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0xcc9933))
                            .child(notice)
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .border_1()
                    .border_color(rgb(0x2e2e2e))
                    .p(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .child(spec.display_name)
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(if cached_now {
                                        rgb(0xd8d8d8)
                                    } else {
                                        rgb(0x909090)
                                    })
                                    .child(if cached_now {
                                        "cached"
                                    } else {
                                        "not downloaded"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(12.))
                            .text_size(px(11.))
                            .text_color(rgb(0x909090))
                            .child(format!("{} MB · {} ms chunks", spec.size_mb, spec.chunk_ms))
                            .child(spec.wer_note),
                    )
                    .children((!cached_now && !self.downloading).then(|| {
                        div().text_size(px(11.)).text_color(rgb(0xcc9933)).child(
                            "Model not on disk — dictation won't work until you download it.",
                        )
                    })),
            )
            .children(
                (spec.min_ram_gb > self.ram_gb && !self.downloading).then(|| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xcc9933))
                        .child(format!(
                            "{} recommends >= {} GB RAM (detected {} GB)",
                            spec.display_name, spec.min_ram_gb, self.ram_gb
                        ))
                }),
            )
            .children(match self.progress.clone() {
                Some(progress) => {
                    let eta = self.eta.estimate(progress.done, progress.total);
                    Some(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0xd8d8d8))
                            .child(progress_status(
                                spec.display_name,
                                &progress,
                                eta.as_deref(),
                            )),
                    )
                }
                None => self.status.clone().map(|status| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xd8d8d8))
                        .child(status)
                }),
            })
            .children(self.progress.as_ref().map(progress_bar))
            .children(self.error.clone().map(|error| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0xcc3333))
                    .child(format!("failed: {error}"))
            }))
            .children(self.start_queued.then(|| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0xcc9933))
                    .child("F9 pressed — recording will begin once models load")
            }))
            .children((self.busy && self.downloading).then(|| {
                action_button(
                    "cancel",
                    if self.cancel_requested {
                        "Cancelling…"
                    } else {
                        "Cancel"
                    },
                    !self.cancel_requested,
                )
                .on_click(cx.listener(|this, _, _, _| {
                    if this.cancel_requested {
                        log!("app", "cancel ignored: already requested");
                        return;
                    }
                    log!("app", "cancel requested");
                    let _ = this.ui.send(UiMessage::CancelDownload);
                }))
            }))
            .children((!(self.busy && self.downloading)).then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .children(dir_exists.then(|| {
                        action_button("reveal", "Reveal in Explorer", !self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_clicked(cx)))
                    }))
                    .children(cached_now.then(|| {
                        action_button("delete", "Delete model", !self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.delete_clicked(cx)))
                    }))
                    .child(
                        action_button("redownload", download_label, !self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.redownload_clicked(cx))),
                    )
                    .children((self.origin == SetupOrigin::Settings).then(|| {
                        action_button("reset-setup", "Run setup again", !self.busy).on_click(
                            cx.listener(|this, _, _, _| {
                                if this.busy {
                                    log!(
                                        "app",
                                        "setup reset ignored: operation already in progress"
                                    );
                                    return;
                                }
                                let _ = this.ui.send(UiMessage::ResetSetup {
                                    captured_model: this.model_id,
                                });
                            }),
                        )
                    }))
            }))
    }
}

fn tour_footer(back: Option<Stateful<Div>>, primary: Stateful<Div>) -> Div {
    div().flex().items_center().gap(px(8.)).children(back).child(primary)
}

fn numbered_row(index: usize, text: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(0x909090))
                .child(format!("{index}.")),
        )
        .child(div().flex_1().text_size(px(14.)).child(text))
}

fn shortcut_key_row(key: &'static str, description: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(keycap(key))
        .child(div().text_size(px(14.)).child(description))
}

pub(crate) fn text_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(4.))
        .py(px(8.))
        .text_size(px(13.))
        .text_color(rgb(0x909090))
        .child(label)
        .on_click(move |event, window, app| on_click(event, window, app))
}

pub(crate) fn keycap(label: &'static str) -> Div {
    div()
        .border_1()
        .border_color(rgb(0x2e2e2e))
        .bg(rgb(0x17191d))
        .px(px(10.))
        .py(px(4.))
        .text_size(px(14.))
        .child(label)
}

pub(crate) fn primary_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(16.))
        .py(px(8.))
        .bg(rgb(0xd8d8d8))
        .text_color(rgb(0x0d0e10))
        .text_size(px(14.))
        .child(label)
        .on_click(move |event, window, app| on_click(event, window, app))
}

pub(crate) fn progress_bar(progress: &DownloadProgress) -> Div {
    let fraction = if progress.total > 0 {
        (progress.done as f32 / progress.total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    div()
        .w_full()
        .h(px(3.))
        .bg(rgb(0x17191d))
        .child(div().h(px(3.)).bg(rgb(0xd8d8d8)).w(relative(fraction)))
}

pub(crate) fn action_button(id: &'static str, label: &'static str, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(8.))
        .py(px(2.))
        .border_1()
        .border_color(rgb(0x2e2e2e))
        .text_size(px(13.))
        .bg(if enabled {
            rgb(0x17191d)
        } else {
            rgb(0x141414)
        })
        .text_color(if enabled {
            rgb(0xe5e7eb)
        } else {
            rgb(0x606060)
        })
        .child(label)
}
