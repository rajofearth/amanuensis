use std::sync::mpsc;
use std::time::{Duration, Instant};

use amanuensis::asr::fetch::{
    EtaTracker, path_is_dir, progress_status, progress_summary, speed_summary,
};
use amanuensis::asr::{
    DownloadProgress, ModelSpec, cache_dir_for, is_model_cached, kind_by_id, repo_cache_dir_for,
    spec_by_id,
};
use amanuensis::backend_detect::{self, BenchProgress, BenchResult, HardwareProfile};
use amanuensis::config::{self, AppConfig};
use amanuensis::log;
use amanuensis::setup_steps::{SetupStep, StepEvent, next_step};
use amanuensis::telemetry;
use gpui::{
    App, Context, Div, IntoElement, Render, Stateful, Window, div, prelude::*, px, relative, rgb,
};

use crate::brand_assets;
use crate::messages::UiMessage;
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SetupOrigin {
    FirstRun,
    Recovery,
    Settings,
    Respawn,
}

pub(crate) const RECOVERY_NOTICE: &str = "Cached model not found. Download it below to continue.";
pub(crate) const BLOCKED_NOTICE: &str = "Model not downloaded. Download it in setup first.";

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
    record_model: String,
    live_model: String,
    rewrite_styling: String,
    rewrite_structure: String,
    rewrite_context: String,
    telemetry_consent: bool,
    preload_engines: bool,
    mic_level: f32,
    mic_peak: f32,
    bench_run: bool,
    profile: HardwareProfile,
    bench_results: Vec<BenchResult>,
    bench_winner: Option<String>,
    bench_winner_result: Option<BenchResult>,
    bench_busy: bool,
    bench_skipped: bool,
    bench_started: bool,
    bench_running: Option<(String, i32)>,
    measuring_pulse: usize,
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
        let cfg = config::load().unwrap_or_default();
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
            record_model: cfg.record_model.clone(),
            live_model: cfg.live_model.clone(),
            rewrite_styling: cfg.rewrite_styling.clone(),
            rewrite_structure: cfg.rewrite_structure.clone(),
            rewrite_context: cfg.rewrite_context.clone(),
            telemetry_consent: cfg.telemetry_consent,
            preload_engines: cfg.preload_engines,
            mic_level: 0.0,
            mic_peak: 0.0,
            bench_run: false,
            profile: backend_detect::detect_hardware(),
            bench_results: Vec::new(),
            bench_winner: None,
            bench_winner_result: None,
            bench_busy: false,
            bench_skipped: false,
            bench_started: false,
            bench_running: None,
            measuring_pulse: 0,
            ui,
        }
    }

    fn toggle_tray(&mut self, cx: &mut Context<Self>) {
        self.tray_enabled = !self.tray_enabled;
        let _ = self.ui.send(UiMessage::TraySetEnabled(self.tray_enabled));
        cx.notify();
    }

    fn persist_config(&self) -> Result<(), String> {
        let mut cfg = config::load().unwrap_or_default();
        cfg.record_model = self.record_model.clone();
        cfg.live_model = self.live_model.clone();
        cfg.rewrite_styling = self.rewrite_styling.clone();
        cfg.rewrite_structure = self.rewrite_structure.clone();
        cfg.rewrite_context = self.rewrite_context.clone();
        cfg.telemetry_consent = self.telemetry_consent;
        cfg.preload_engines = self.preload_engines;
        cfg.tray_enabled = self.tray_enabled;
        config::save(&cfg)
    }

    fn set_engine(&mut self, slot: &'static str, id: &'static str, cx: &mut Context<Self>) {
        if slot == "record" {
            self.record_model = id.to_owned();
        } else {
            self.live_model = id.to_owned();
        }
        match self.persist_config() {
            Ok(()) => log!("app", "engine picked: {slot}={id}"),
            Err(error) => {
                log!("app", "engine pick save FAILED: {error}");
                self.error = Some(format!("could not save engine choice: {error}"));
            }
        }
        cx.notify();
    }

    fn set_preset(&mut self, slot: &'static str, value: &'static str, cx: &mut Context<Self>) {
        match slot {
            "styling" => self.rewrite_styling = value.to_owned(),
            "structure" => self.rewrite_structure = value.to_owned(),
            _ => self.rewrite_context = value.to_owned(),
        }
        match self.persist_config() {
            Ok(()) => log!("app", "rewrite preset picked: {slot}={value}"),
            Err(error) => {
                log!("app", "rewrite preset save FAILED: {error}");
                self.error = Some(format!("could not save rewrite choice: {error}"));
            }
        }
        cx.notify();
    }

    fn toggle_telemetry(&mut self, cx: &mut Context<Self>) {
        self.telemetry_consent = !self.telemetry_consent;
        let state = self.telemetry_consent;
        match self.persist_config() {
            Ok(()) => log!("app", "telemetry consent flipped: {state}"),
            Err(error) => {
                log!("app", "telemetry consent save FAILED: {error}");
                self.error = Some(format!("could not save telemetry choice: {error}"));
            }
        }
        telemetry::set_consent(state);
        telemetry::consent_event(state);
        cx.notify();
    }

    fn toggle_preload(&mut self, cx: &mut Context<Self>) {
        self.preload_engines = !self.preload_engines;
        match self.persist_config() {
            Ok(()) => log!("app", "preload flipped: {}", self.preload_engines),
            Err(error) => {
                log!("app", "preload save FAILED: {error}");
                self.error = Some(format!("could not save preload choice: {error}"));
            }
        }
        cx.notify();
    }

    fn export_logs_clicked(&mut self, cx: &mut Context<Self>) {
        match telemetry::export_logs() {
            Ok(path) => {
                log!("app", "logs exported: {}", path.display());
                self.status = Some(format!("Logs saved to {}", path.display()));
                self.error = None;
            }
            Err(error) => {
                log!("app", "log export FAILED: {error}");
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn recheck_backend(&mut self, cx: &mut Context<Self>) {
        if self.bench_run {
            log!("app", "backend recheck ignored: bench already running");
            return;
        }
        self.bench_run = true;
        log!("app", "backend recheck requested");
        let _ = self.ui.send(UiMessage::RecheckBackend);
        cx.notify();
    }

    pub(crate) fn on_bench_finished(&mut self, winner: Option<String>, cx: &mut Context<Self>) {
        self.bench_run = false;
        match winner {
            Some(provider) => {
                log!("app", "backend bench finished: winner={provider}");
                self.status = Some(format!("Backend test done. Fastest is {provider}."));
            }
            None => {
                log!("app", "backend bench finished: no winner recorded");
                self.status = Some("Backend test skipped or gave no result.".to_owned());
            }
        }
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
            let next = next_step(step, event);
            if next == Some(SetupStep::Measuring) {
                self.maybe_start_bench(cx);
            }
            self.step = next;
            cx.notify();
        }
    }

    fn maybe_start_bench(&mut self, cx: &mut Context<Self>) {
        if self.bench_started || self.bench_skipped {
            return;
        }
        self.bench_started = true;
        self.bench_busy = true;
        self.bench_winner = None;
        self.bench_winner_result = None;
        self.bench_results.clear();
        self.bench_running = None;
        self.measuring_pulse = 0;
        self.pulse_measuring(cx);
        log!("app", "onboarding bench started on background thread");
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let winner = backend_detect::run_backend_bench_with(&mut |progress| {
                let _ = ui.send(UiMessage::BenchProgress(progress));
            });
            let _ = ui.send(UiMessage::BenchProgress(BenchProgress::Finished { winner }));
        });
        cx.notify();
    }

    fn skip_bench(&mut self, cx: &mut Context<Self>) {
        if self.bench_skipped {
            log!("app", "bench skip ignored: already skipped");
            return;
        }
        self.bench_skipped = true;
        self.bench_busy = false;
        self.bench_running = None;
        let mut config = config::load().unwrap_or_default();
        config.backend_cache.asr.provider = "cpu".to_owned();
        config.backend_cache.asr.threads = 2;
        match config::save(&config) {
            Ok(()) => log!("app", "bench skipped; persisted cpu@2 fallback"),
            Err(error) => log!("app", "bench skip config save FAILED: {error}"),
        }
        if let Some(step) = self.step {
            self.step = next_step(step, StepEvent::NavNext);
        }
        cx.notify();
    }

    fn pulse_measuring(&mut self, cx: &mut Context<Self>) {
        if !self.bench_busy {
            return;
        }
        let interval = Duration::from_millis(350);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.measuring_pulse += 1;
                    this.pulse_measuring(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn on_bench_progress(&mut self, progress: BenchProgress, cx: &mut Context<Self>) {
        if self.bench_skipped {
            if matches!(progress, BenchProgress::Finished { .. }) {
                let mut config = config::load().unwrap_or_default();
                config.backend_cache.asr.provider = "cpu".to_owned();
                config.backend_cache.asr.threads = 2;
                match config::save(&config) {
                    Ok(()) => log!(
                        "app",
                        "bench finished after skip; cpu@2 fallback re-asserted"
                    ),
                    Err(error) => log!("app", "cpu@2 re-assert save FAILED: {error}"),
                }
            }
            log!("app", "bench progress ignored after skip: {progress:?}");
            return;
        }
        match progress {
            BenchProgress::Measuring {
                provider, threads, ..
            } => {
                self.bench_busy = true;
                self.bench_running = Some((provider.clone(), threads));
                log!("app", "bench measuring {provider} · {threads} threads");
            }
            BenchProgress::Measured(result) => {
                self.bench_running = None;
                self.bench_results.push(result);
                self.bench_results.sort_by(|a, b| {
                    a.rtf
                        .partial_cmp(&b.rtf)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                log!(
                    "app",
                    "bench result appended; live leaderboard has {} ranked results",
                    self.bench_results.len()
                );
            }
            BenchProgress::Finished { winner } => {
                self.bench_busy = false;
                self.bench_running = None;
                match winner {
                    Some(provider) => {
                        let config = config::load().unwrap_or_default();
                        let threads = config.backend_cache.asr.threads;
                        let threads = if threads > 0 { threads } else { 2 };
                        self.bench_winner = Some(bench_label(&provider, threads));
                        self.bench_winner_result = Some(BenchResult {
                            provider: provider.clone(),
                            threads,
                            is_gpu: provider == "cuda",
                            rtf: config.backend_cache.asr.win_rtf,
                        });
                        log!(
                            "app",
                            "onboarding bench finished: winner={provider} threads={threads}"
                        );
                    }
                    None => {
                        self.bench_winner = None;
                        self.bench_winner_result = None;
                        log!("app", "onboarding bench finished: no winner recorded");
                    }
                }
            }
        }
        cx.notify();
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
            self.status = Some("Cancelled. It will resume next time.".to_owned());
        } else {
            self.status = Some(
                "Cancelled. Progress is saved. The next Start picks up where it stopped."
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
        let config = AppConfig {
            model: self.model_id.to_owned(),
            tray_enabled: config::load().map_or(true, |existing| existing.tray_enabled),
            ..config::load().unwrap_or_default()
        };
        if let Err(error) = config::save(&config) {
            log!("app", "config save after model download FAILED: {error}");
        } else {
            log!(
                "app",
                "config saved after model download: model={}",
                config.model
            );
        }
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
        self.status = Some(format!(
            "Checking {} and preparing the download...",
            spec.display_name
        ));
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
                .child(brand_assets::horizontal(190.))
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
                            "One-time setup downloads the voice model (~{} MB), then a small cleanup model. After that, everything runs on your PC. Nothing leaves it.",
                            spec.size_mb
                        )),
                )
                .children((spec.min_ram_gb > self.ram_gb).then(|| {
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0xcc9933))
                        .child("Your PC has less memory than recommended. It may run slowly.")
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .id("consent-check")
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(0x505050))
                                .px(px(10.))
                                .py(px(4.))
                                .bg(if self.telemetry_consent {
                                    rgb(0x2a2a2a)
                                } else {
                                    rgb(0x171717)
                                })
                                .text_size(px(11.))
                                .text_color(if self.telemetry_consent {
                                    rgb(0xf2f2f2)
                                } else {
                                    rgb(0x707070)
                                })
                                .child(if self.telemetry_consent { "ON" } else { "OFF" })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_telemetry(cx))),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x909090))
                                .child("Share anonymous usage stats. Off unless you say yes."),
                        ),
                )
                .children(self.progress.as_ref().map(|progress| {
                    let eta = self.eta.estimate(progress.done, progress.total);
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xd8d8d8))
                        .child(progress_status(
                            &progress.file,
                            progress,
                            eta.as_deref(),
                        ))
                }))
                .children(self.progress.as_ref().map(progress_bar))
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
                    "A voice model on your PC turns your speech into text. Audio never leaves your machine",
                ))
                .child(numbered_row(
                    3,
                    "A cleanup model tidies the text using your presets, then pastes it where your cursor is. Very short clips skip this step",
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
                            .child("Say something. The bar should move."),
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
                                "Looks good. Continue",
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
                    .child(div().text_size(px(20.)).child("Setting up dictation"))
                    .children((self.progress.is_none()).then(|| {
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0xd8d8d8))
                            .child(
                                self.status
                                    .clone()
                                    .unwrap_or_else(|| "Preparing the download...".to_owned()),
                            )
                    }))
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
                            .child("You can cancel. It resumes where it left off."),
                    )
            }
            SetupStep::DetectHardware => {
                let profile = &self.profile;
                let graphics = bench_graphics_line(profile);
                base()
                    .child(div().text_size(px(26.)).child("A quick look at your setup"))
                    .child(
                        div()
                            .text_size(px(14.))
                            .child(detect_heading(profile)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .border_1()
                            .border_color(rgb(0x2e2e2e))
                            .px(px(14.))
                            .py(px(10.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(0x909090))
                                    .child("Graphics"),
                            )
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .child(graphics),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0x909090))
                            .child(
                                "Next I'll time a quick sample on each engine and pick the fastest one for your machine.",
                            ),
                    )
                    .child(tour_footer(
                        Some(action_button("tour-hardware-back", "Back", true).on_click(
                            cx.listener(|this, _, _, cx| this.nav(StepEvent::NavBack, cx)),
                        )),
                        primary_button(
                            "tour-hardware-next",
                            "Next",
                            cx.listener(|this, _, _, cx| this.nav(StepEvent::NavNext, cx)),
                        ),
                    ))
            }
            SetupStep::Measuring => {
                let candidates = backend_detect::candidates_for(&self.profile);
                let results = &self.bench_results;
                let expected = candidates.len();
                let fastest = results
                    .first()
                    .map(|result| result.rtf)
                    .filter(|rtf| rtf.is_finite() && *rtf > 0.0)
                    .unwrap_or(1.0);
                let waiting = self.bench_busy;
                let dots = ".".repeat((self.measuring_pulse % 3) + 1);
                let back = action_button("tour-measuring-back", "Back", true)
                    .on_click(cx.listener(|this, _, _, cx| this.nav(StepEvent::NavBack, cx)));
                base()
                    .child(div().text_size(px(26.)).child("Finding your fastest engine"))
                    .child(
                        div()
                            .text_size(px(14.))
                            .child(
                                "I'm timing a short sample on each engine. Every planned test is listed below; rows fill in as each engine finishes.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .border_1()
                            .border_color(rgb(0x2e2e2e))
                            .px(px(14.))
                            .py(px(12.))
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(div().size(px(22.)))
                                    .child(
                                        div()
                                            .w(px(150.))
                                            .text_size(px(10.))
                                            .text_color(rgb(0x606060))
                                            .child("Engine"),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(10.))
                                            .text_color(rgb(0x606060))
                                            .child("Speed"),
                                    ),
                            )
                            .children(candidates.iter().map(|candidate| {
                                let done = results
                                    .iter()
                                    .enumerate()
                                    .find(|(_, result)| {
                                        result.provider == candidate.provider
                                            && result.threads == candidate.threads
                                    })
                                    .map(|(index, result)| (index + 1, result));
                                let running = self.bench_running.as_ref().is_some_and(
                                    |(provider, threads)| {
                                        provider == &candidate.provider
                                            && *threads == candidate.threads
                                    },
                                );
                                match done {
                                    Some((rank, result)) => {
                                        let fraction = bench_bar_fraction(result, fastest);
                                        let winner_row = self
                                            .bench_winner_result
                                            .as_ref()
                                            .is_some_and(|winner| {
                                                winner.provider == result.provider
                                                    && winner.threads == result.threads
                                            });
                                        let fastest_row = result.rtf <= fastest;
                                        let fill = if fastest_row {
                                            rgb(0x4c9f6e)
                                        } else {
                                            rgb(0x6b7280)
                                        };
                                        let tag = if winner_row {
                                            "Selected".to_owned()
                                        } else if fastest_row {
                                            "Fastest".to_owned()
                                        } else if result.rtf.is_finite() && result.rtf > 0.0 {
                                            format!("{:.1}x slower", result.rtf / fastest)
                                        } else {
                                            String::new()
                                        };
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(10.))
                                            .child(
                                                div()
                                                    .size(px(22.))
                                                    .border_1()
                                                    .border_color(rgb(0x2e2e2e))
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(11.))
                                                    .text_color(if winner_row {
                                                        rgb(0x9fe0b8)
                                                    } else {
                                                        rgb(0x909090)
                                                    })
                                                    .child(format!("{rank}")),
                                            )
                                            .child(
                                                div()
                                                    .w(px(150.))
                                                    .truncate()
                                                    .text_size(px(13.))
                                                    .text_color(if winner_row {
                                                        rgb(0x9fe0b8)
                                                    } else {
                                                        rgb(0xe5e7eb)
                                                    })
                                                    .child(bench_label(
                                                        &result.provider,
                                                        result.threads,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .h(px(6.))
                                                    .bg(rgb(0x17191d))
                                                    .child(
                                                        div()
                                                            .h(px(6.))
                                                            .bg(fill)
                                                            .w(relative(fraction)),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .w(px(72.))
                                                    .justify_end()
                                                    .text_size(px(11.))
                                                    .text_color(if winner_row {
                                                        rgb(0x9fe0b8)
                                                    } else {
                                                        rgb(0x909090)
                                                    })
                                                    .child(tag),
                                            )
                                    }
                                    None if running => div()
                                        .flex()
                                        .items_center()
                                        .gap(px(10.))
                                        .bg(rgb(0x14161a))
                                        .child(div().size(px(22.)))
                                        .child(
                                            div()
                                                .w(px(150.))
                                                .truncate()
                                                .text_size(px(13.))
                                                .text_color(rgb(0xe5e7eb))
                                                .child(bench_label(
                                                    &candidate.provider,
                                                    candidate.threads,
                                                )),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .h(px(6.))
                                                .bg(rgb(0x17191d)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .w(px(72.))
                                                .justify_end()
                                                .text_size(px(11.))
                                                .text_color(rgb(0x7fb2e8))
                                                .child(format!("Running{dots}")),
                                        ),
                                    None => div()
                                        .flex()
                                        .items_center()
                                        .gap(px(10.))
                                        .child(div().size(px(22.)))
                                        .child(
                                            div()
                                                .w(px(150.))
                                                .truncate()
                                                .text_size(px(13.))
                                                .text_color(rgb(0x606060))
                                                .child(bench_label(
                                                    &candidate.provider,
                                                    candidate.threads,
                                                )),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .h(px(6.))
                                                .bg(rgb(0x17191d)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .w(px(72.))
                                                .justify_end()
                                                .text_size(px(11.))
                                                .text_color(rgb(0x606060))
                                                .child("Waiting"),
                                        ),
                                }
                            })),
                    )
                    .children(if waiting {
                        Some(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(0xd8d8d8))
                                        .child(format!("Measuring{dots}")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(11.))
                                        .text_color(rgb(0x606060))
                                        .child(if results.len() >= expected {
                                            "Almost done, just wrapping up.".to_owned()
                                        } else {
                                            "This usually takes a few seconds.".to_owned()
                                        }),
                                ),
                        )
                    } else {
                        None
                    })
                    .children((!waiting && self.bench_skipped).then(|| {
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0xd8d8d8))
                            .child("Using the CPU default.")
                    }))
                    .children(
                        (!waiting && !self.bench_skipped)
                            .then(|| {
                                self.bench_winner.as_ref().map(|winner| {
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(0x9fe0b8))
                                        .child(format!("Done. I'll use {winner}."))
                                })
                            })
                            .flatten(),
                    )
                    .children((!waiting && !self.bench_skipped && self.bench_winner.is_none()).then(|| {
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0xd8d8d8))
                            .child("I'll start you on the CPU default.")
                    }))
                    .children(waiting.then(|| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x909090))
                            .child("You can stop early and use the CPU default.")
                    }))
                    .child(if waiting {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(back)
                            .child(
                                action_button("tour-measuring-skip", "Use CPU default", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.skip_bench(cx))),
                            )
                    } else {
                        tour_footer(
                            Some(back),
                            primary_button(
                                "tour-measuring-next",
                                "Next",
                                cx.listener(|this, _, _, cx| this.nav(StepEvent::NavNext, cx)),
                            ),
                        )
                    })
            }
            SetupStep::Ready => base()
                .child(div().text_size(px(26.)).child("You are all set. Try it."))
                .child(
                    div()
                        .text_size(px(14.))
                        .child("Hold F9 and speak. Your words land in the last app you used."),
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
                        .child("F9 pressed. Recording begins once the models load")
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
                        "Found {} GB RAM and {} logical cores",
                        self.ram_gb, self.cores
                    )),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .gap(px(12.))
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
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .child("Windows tray icon"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(0x808080))
                                            .child(
                                                "Left-click opens settings; right-click shows actions.",
                                            ),
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
            .child({
                let cached = config::load().unwrap_or_default().backend_cache.asr;
                let profile = &self.profile;
                let description = if self.bench_run {
                    "Benchmarking backend…".to_owned()
                } else if !cached.provider.is_empty() {
                    format!(
                        "{} · {} threads · {:.2}x realtime",
                        cached.provider, cached.threads, cached.win_rtf
                    )
                } else {
                    "Not selected yet".to_owned()
                };
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
                            .child(div().text_size(px(13.)).child("Audio backend"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x808080))
                                    .child(description),
                            )
                            .child(div().text_size(px(10.)).text_color(rgb(0x606060)).child(
                                format!("{} · {}", profile.vendor.label(), profile.gpu_label),
                            )),
                    )
                    .child(
                        action_button(
                            "recheck-backend",
                            if self.bench_run {
                                "Benchmarking…"
                            } else {
                                "Re-check"
                            },
                            !self.bench_run,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.recheck_backend(cx))),
                    )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .border_1()
                    .border_color(rgb(0x2e2e2e))
                    .p(px(12.))
                    .child(div().text_size(px(13.)).child("Engines"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x808080))
                            .child("Record handles F9 clips and lands after you stop. Live shows text as you talk when set to Nemotron."),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(64.))
                                    .text_size(px(11.))
                                    .text_color(rgb(0x909090))
                                    .child("Record"),
                            )
                            .child(
                                choice_button(
                                    "engine-record-moonshine",
                                    "Moonshine",
                                    self.record_model == "moonshine",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_engine("record", "moonshine", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "engine-record-nemotron",
                                    "Nemotron",
                                    self.record_model == "nemotron",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_engine("record", "nemotron", cx)
                                })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(64.))
                                    .text_size(px(11.))
                                    .text_color(rgb(0x909090))
                                    .child("Live"),
                            )
                            .child(
                                choice_button(
                                    "engine-live-moonshine",
                                    "Moonshine",
                                    self.live_model == "moonshine",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_engine("live", "moonshine", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "engine-live-nemotron",
                                    "Nemotron",
                                    self.live_model == "nemotron",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_engine("live", "nemotron", cx)
                                })),
                            ),
                    )
                    .children((self.live_model == "moonshine").then(|| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0xcc9933))
                            .child(
                                "Live runs on Moonshine, so text only lands after you stop. You will not see partials while speaking.",
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .border_1()
                    .border_color(rgb(0x2e2e2e))
                    .p(px(12.))
                    .child(div().text_size(px(13.)).child("Rewrite presets"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x808080))
                            .child("Record clips get tidied before pasting. These presets set the tone. Very short clips skip this step."),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(64.))
                                    .text_size(px(11.))
                                    .text_color(rgb(0x909090))
                                    .child("Styling"),
                            )
                            .child(
                                choice_button(
                                    "preset-styling-casual",
                                    "Casual",
                                    self.rewrite_styling == "casual",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("styling", "casual", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "preset-styling-semi-casual",
                                    "Semi-casual",
                                    self.rewrite_styling == "semi-casual",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("styling", "semi-casual", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "preset-styling-semi-formal",
                                    "Semi-formal",
                                    self.rewrite_styling == "semi-formal",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("styling", "semi-formal", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "preset-styling-formal",
                                    "Formal",
                                    self.rewrite_styling == "formal",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("styling", "formal", cx)
                                })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(64.))
                                    .text_size(px(11.))
                                    .text_color(rgb(0x909090))
                                    .child("Structure"),
                            )
                            .child(
                                choice_button(
                                    "preset-structure-prose",
                                    "Prose",
                                    self.rewrite_structure == "prose",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("structure", "prose", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "preset-structure-lists",
                                    "Lists",
                                    self.rewrite_structure == "lists",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("structure", "lists", cx)
                                })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(64.))
                                    .text_size(px(11.))
                                    .text_color(rgb(0x909090))
                                    .child("Context"),
                            )
                            .child(
                                choice_button(
                                    "preset-context-general",
                                    "General",
                                    self.rewrite_context == "general",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("context", "general", cx)
                                })),
                            )
                            .child(
                                choice_button(
                                    "preset-context-email",
                                    "Email",
                                    self.rewrite_context == "email",
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_preset("context", "email", cx)
                                })),
                            ),
                    ),
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
                            .child(div().text_size(px(13.)).child("Usage stats"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x808080))
                                    .child("Anonymous telemetry. Off unless you say yes."),
                            ),
                    )
                    .child(
                        div()
                            .id("telemetry-toggle")
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(0x505050))
                            .px(px(10.))
                            .py(px(4.))
                            .bg(if self.telemetry_consent {
                                rgb(0x2a2a2a)
                            } else {
                                rgb(0x171717)
                            })
                            .text_size(px(11.))
                            .text_color(if self.telemetry_consent {
                                rgb(0xf2f2f2)
                            } else {
                                rgb(0x707070)
                            })
                            .child(if self.telemetry_consent { "ON" } else { "OFF" })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_telemetry(cx))),
                    ),
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
                            .child(div().text_size(px(13.)).child("Preload engines"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x808080))
                                    .child("Warm both engines at startup when memory allows."),
                            ),
                    )
                    .child(
                        div()
                            .id("preload-toggle")
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(0x505050))
                            .px(px(10.))
                            .py(px(4.))
                            .bg(if self.preload_engines {
                                rgb(0x2a2a2a)
                            } else {
                                rgb(0x171717)
                            })
                            .text_size(px(11.))
                            .text_color(if self.preload_engines {
                                rgb(0xf2f2f2)
                            } else {
                                rgb(0x707070)
                            })
                            .child(if self.preload_engines { "ON" } else { "OFF" })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_preload(cx))),
                    ),
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
                            .child(div().text_size(px(13.)).child("Logs"))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(0x808080))
                                    .child("Bundle logs and device info into a zip."),
                            ),
                    )
                    .child(
                        action_button("export-logs", "Export logs", true)
                            .on_click(cx.listener(|this, _, _, cx| this.export_logs_clicked(cx))),
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
                            .child(format!("~{} MB download", spec.size_mb))
                            .child(spec.wer_note),
                    )
                    .children((!cached_now && !self.downloading).then(|| {
                        div().text_size(px(11.)).text_color(rgb(0xcc9933)).child(
                            "Model not on disk. Download it before dictating.",
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
                            .child("F9 pressed. Recording begins once the models load")
                    })),
            )
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
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .children(back)
        .child(primary)
}

fn detect_heading(profile: &HardwareProfile) -> String {
    if matches!(profile.vendor, backend_detect::Vendor::Nvidia) {
        format!(
            "I found an {} graphics card. I'll check whether it can make dictation faster.",
            profile.vendor.label()
        )
    } else {
        "I'll tune the engine to make the most of this CPU.".to_owned()
    }
}

fn bench_graphics_line(profile: &HardwareProfile) -> String {
    if matches!(profile.vendor, backend_detect::Vendor::Nvidia) {
        if profile.gpu_label.trim().is_empty() {
            profile.vendor.label().to_owned()
        } else {
            profile.gpu_label.clone()
        }
    } else {
        "CPU only".to_owned()
    }
}

fn bench_label(provider: &str, threads: i32) -> String {
    match provider {
        "cuda" => "NVIDIA (GPU)".to_owned(),
        _ => {
            let cores = if threads == 1 {
                "1 core".to_owned()
            } else {
                format!("{threads} cores")
            };
            format!("CPU · {cores}")
        }
    }
}

fn bench_bar_fraction(result: &BenchResult, fastest_rtf: f32) -> f32 {
    if result.rtf.is_finite() && result.rtf > 0.0 && fastest_rtf.is_finite() && fastest_rtf > 0.0 {
        (fastest_rtf / result.rtf).min(1.0)
    } else {
        0.0
    }
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

pub(crate) fn choice_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(8.))
        .py(px(2.))
        .border_1()
        .border_color(if selected {
            rgb(0x6b7280)
        } else {
            rgb(0x2e2e2e)
        })
        .text_size(px(12.))
        .bg(if selected {
            rgb(0x2a2a2a)
        } else {
            rgb(0x17191d)
        })
        .text_color(if selected {
            rgb(0xf2f2f2)
        } else {
            rgb(0xe5e7eb)
        })
        .child(label)
}
