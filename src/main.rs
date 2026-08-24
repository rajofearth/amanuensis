use std::{
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant},
};

use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey},
};
use gpui::{
    App, Bounds, Context, Div, Entity, MouseButton, Stateful, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, prelude::*, px,
    relative, rgb, size,
};
use gpui_platform::application;
use raycast_dictation_clone::asr::fetch::{
    EtaTracker, path_is_dir, progress_status, progress_summary,
};
use raycast_dictation_clone::asr::{
    self, Command, DownloadProgress, Event, Mode, ModelKind, ModelSelection, ModelSpec,
    cache_dir_for, ensure_model_by_spec, is_model_cached, kind_by_id, repo_cache_dir_for,
    spec_by_id,
};
use raycast_dictation_clone::audio;
use raycast_dictation_clone::config::{self, AppConfig};
use raycast_dictation_clone::log;
use raycast_dictation_clone::logging;
use raycast_dictation_clone::paste::{MIN_AUDIO_SECS, clean_transcript, paste_text};
use raycast_dictation_clone::pill_window as pw;
use raycast_dictation_clone::setup_steps::{SetupStep, StepEvent, next_step};
use raycast_dictation_clone::win_focus::{self, FocusTarget};

const BARS: usize = 26;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WINDOW_TITLE: &str = "raycast-dictation-window";
const FLASH_SECS: u64 = 1;

enum HotkeyMessage {
    ToggleRecording,
    DebugReload,
}

enum PasteResult {
    Pasted(usize),
    Failed(String),
}

enum DownloadMessage {
    Progress {
        generation: u64,
        progress: DownloadProgress,
    },
    Finished {
        generation: u64,
        result: Result<ModelKind, String>,
    },
    Cancelled {
        generation: u64,
    },
}

enum UiMessage {
    OpenSettings,
    ShowPill,
    HidePill,
    PanelClosed,
    StartDownload {
        captured_model: &'static str,
        purge: bool,
    },
    CancelDownload,
    FinishOnboarding {
        captured_model: &'static str,
    },
    DeleteRequest {
        captured_model: &'static str,
    },
    ResetSetup {
        captured_model: &'static str,
    },
    DeleteFinished(Result<(), String>),
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
    Flash,
}

struct Dictation {
    phase: Phase,
    mode: Mode,
    waveform: Waveform,
    commands: mpsc::Sender<Command>,
    results: mpsc::Sender<PasteResult>,
    ui: mpsc::Sender<UiMessage>,
    status: String,
    partial: String,
    committed: String,
    pending_start: bool,
    transcribing_since: Option<Instant>,
    focus: Option<FocusTarget>,
    recording_started: Option<Instant>,
    discard_requested: bool,
    flash_since: Option<Instant>,
    hovered: bool,
    drag_grab: Option<(i32, i32)>,
}

impl Dictation {
    fn begin_recording(&mut self) {
        self.focus = win_focus::capture_foreground();
        match &self.focus {
            Some(target) => log!("app", "recording starts, target window: {target}"),
            None => log!("app", "recording starts, no foreground window captured"),
        }
        self.phase = Phase::Recording;
        self.partial.clear();
        self.transcribing_since = None;
        self.flash_since = None;
        self.recording_started = Some(Instant::now());
        let _ = self.commands.send(Command::Start);
        let _ = self.ui.send(UiMessage::ShowPill);
    }

    fn stop_recording(&mut self) {
        self.phase = Phase::Transcribing;
        self.transcribing_since = Some(Instant::now());
        self.recording_started = None;
        let _ = self.commands.send(Command::Stop);
    }

    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        log!("app", "F9 toggle in phase {:?}", self.phase);
        match self.phase {
            Phase::Loading => self.pending_start = true,
            Phase::Idle => self.begin_recording(),
            Phase::Recording => self.stop_recording(),
            Phase::Transcribing => self.pending_start = true,
            Phase::Flash => {
                log!("app", "F9 during done-flash: starting next recording");
                self.begin_recording();
            }
        }
        cx.notify();
    }

    fn discard_clicked(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Recording {
            return;
        }
        log!("app", "recording discarded by user");
        self.discard_requested = true;
        self.stop_recording();
        cx.notify();
    }

    fn done_clicked(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Recording {
            return;
        }
        log!("app", "recording finished by user");
        self.stop_recording();
        cx.notify();
    }

    fn tick_flash(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Flash {
            return;
        }
        let expired = self
            .flash_since
            .is_some_and(|since| since.elapsed().as_secs() >= FLASH_SECS);
        if !expired {
            return;
        }
        if self.pending_start {
            log!("app", "flash skipped: queued start consumed");
            self.pending_start = false;
            self.begin_recording();
        } else {
            self.phase = Phase::Idle;
            self.flash_since = None;
            self.committed.clear();
            let _ = self.ui.send(UiMessage::HidePill);
        }
        cx.notify();
    }

    fn handle_asr_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::LoadingProgress(message) => self.status = message,
            Event::Ready | Event::ModelReady(_) => {
                if let Event::ModelReady(mode) = event {
                    self.mode = mode;
                }
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
            Event::Committed {
                text,
                duration_secs,
            } => {
                if self.phase == Phase::Transcribing {
                    self.transcribing_since = None;
                    self.partial.clear();
                    if self.discard_requested {
                        log!(
                            "app",
                            "commit discarded by user ({} chars)",
                            text.chars().count()
                        );
                        self.discard_requested = false;
                        self.phase = Phase::Flash;
                        self.flash_since = Some(Instant::now());
                        cx.notify();
                        return;
                    }
                    self.phase = Phase::Flash;
                    self.flash_since = Some(Instant::now());
                    log!(
                        "app",
                        "committed {:.1}s audio, raw ({} chars): {text:?}",
                        duration_secs,
                        text.chars().count()
                    );
                    let cleaned = clean_transcript(&text);
                    if cleaned != text.trim() {
                        log!(
                            "app",
                            "cleanup: raw -> cleaned ({} chars): {cleaned:?}",
                            cleaned.chars().count()
                        );
                    }
                    if cleaned.is_empty() {
                        self.status = "empty".to_owned();
                        log!("app", "gate: dropped (empty after cleanup)");
                    } else if duration_secs < MIN_AUDIO_SECS {
                        self.status = format!("discarded (<{MIN_AUDIO_SECS}s)");
                        log!(
                            "app",
                            "gate: dropped ({duration_secs:.2}s < {MIN_AUDIO_SECS}s min)"
                        );
                    } else {
                        self.committed = cleaned.clone();
                        let results = self.results.clone();
                        // Follow the user's live focus: if they switched apps during
                        // recording, paste where they are now. Only rescue the focus
                        // when our own HUD is in the way.
                        let initial = self.focus;
                        let current = win_focus::capture_foreground();
                        let focus = match &current {
                            Some(window) if win_focus::is_own_window(window) => {
                                log!("app", "focus is on our HUD; restoring {initial:?}");
                                initial
                            }
                            Some(window) => {
                                if initial.as_ref() != Some(window) {
                                    log!(
                                        "app",
                                        "user moved to {window} since record start; pasting there"
                                    );
                                }
                                None
                            }
                            None => initial,
                        };
                        thread::spawn(move || {
                            let result = match paste_text(&cleaned, focus) {
                                Ok(chars) => PasteResult::Pasted(chars),
                                Err(error) => PasteResult::Failed(error),
                            };
                            let _ = results.send(result);
                        });
                    }
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

    fn handle_paste_result(&mut self, result: PasteResult, cx: &mut Context<Self>) {
        match result {
            PasteResult::Pasted(chars) => {
                log!("app", "paste ok: {chars} chars");
                self.status = format!("pasted {chars} chars");
            }
            PasteResult::Failed(error) => {
                log!("app", "paste FAILED: {error}");
                self.status = format!("paste failed: {error}");
            }
        }
        cx.notify();
    }

    fn open_settings(&self) {
        let _ = self.ui.send(UiMessage::OpenSettings);
    }

    fn drag_started(&mut self, cx: &mut Context<Self>) {
        if let (Some(hwnd), Some((cx_pos, cy_pos))) =
            (pw::find_by_title(&window_title_utf16()), pw::cursor_pos())
            && let Some((wx, wy)) = pw::window_rect(hwnd)
        {
            self.drag_grab = Some((cx_pos - wx, cy_pos - wy));
            log!("app", "pill drag started");
            cx.notify();
        }
    }

    fn drag_ended(&mut self, cx: &mut Context<Self>) {
        if self.drag_grab.take().is_some() {
            log!("app", "pill drag ended");
            cx.notify();
        }
    }

    fn cancel_pending_start(&mut self, cx: &mut Context<Self>) {
        if self.pending_start {
            log!("app", "opening settings: cancelled queued start");
            self.pending_start = false;
            cx.notify();
        }
    }
}

impl Render for Dictation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick_flash(cx);
        if matches!(
            self.phase,
            Phase::Recording | Phase::Transcribing | Phase::Flash
        ) {
            window.request_animation_frame();
        }
        if !matches!(
            self.phase,
            Phase::Recording | Phase::Transcribing | Phase::Flash
        ) {
            return div().id("pill-hidden").size_full();
        }
        let expanded = self.hovered && self.phase == Phase::Recording;
        let duration_text = match self.phase {
            Phase::Recording => self
                .recording_started
                .map(|started| {
                    let total_secs = started.elapsed().as_secs();
                    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
                })
                .unwrap_or_else(|| "00:00".to_owned()),
            Phase::Transcribing => "transcribing…".to_owned(),
            Phase::Flash => "done ✓".to_owned(),
            _ => String::new(),
        };
        let bar_levels: Vec<f32> = match self.phase {
            Phase::Transcribing | Phase::Flash => {
                let elapsed = self
                    .transcribing_since
                    .map_or(0.0_f64, |since| since.elapsed().as_secs_f64());
                (0..BARS)
                    .map(|index| {
                        let wave = (elapsed * 3.0 + index as f64 * 0.7).sin().abs();
                        ((0.2 + 0.5 * wave).clamp(0.08, 1.0)) as f32
                    })
                    .collect()
            }
            _ => self.waveform.levels().collect(),
        };
        let bar_color = if self.phase == Phase::Flash {
            rgb(0x606060)
        } else {
            rgb(0x33cc66)
        };
        div()
            .id("pill-root")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hovered = *hovered;
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, _| {
                    if this.phase == Phase::Recording {
                        log!("app", "settings ignored while recording");
                    } else {
                        this.open_settings();
                    }
                }),
            )
            .child(
                div()
                    .id("pill")
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded_full()
                    .bg(rgb(0x161616))
                    .border_1()
                    .border_color(rgb(0x333333))
                    .px(px(16.))
                    .py(px(10.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.drag_started(cx)),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.drag_ended(cx)),
                    )
                    .children(expanded.then(|| {
                        div()
                            .id("discard-recording")
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(rgb(0xcc3333))
                            .child("✕")
                            .on_click(cx.listener(|this, _, _, cx| this.discard_clicked(cx)))
                    }))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(2.))
                            .h(px(18.))
                            .w(px(110.))
                            .children(bar_levels.iter().map(|level| {
                                div()
                                    .flex_1()
                                    .h(relative(*level))
                                    .rounded_sm()
                                    .bg(bar_color)
                            })),
                    )
                    .child(
                        div()
                            .min_w(px(52.))
                            .text_size(px(13.))
                            .text_color(rgb(0xcccccc))
                            .child(duration_text),
                    )
                    .children(expanded.then(|| {
                        div()
                            .id("done-recording")
                            .cursor_pointer()
                            .text_size(px(14.))
                            .text_color(rgb(0x33cc66))
                            .child("✓")
                            .on_click(cx.listener(|this, _, _, cx| this.done_clicked(cx)))
                    })),
            )
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SetupOrigin {
    FirstRun,
    Recovery,
    Settings,
    Respawn,
}

const RECOVERY_NOTICE: &str = "Cached model not found — download it below to continue.";
const BLOCKED_NOTICE: &str = "Model not downloaded — download it in setup first.";

struct OnboardingView {
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
    ui: mpsc::Sender<UiMessage>,
}

impl OnboardingView {
    fn new(
        origin: SetupOrigin,
        device: (u32, usize),
        model_id: &'static str,
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
            ui,
        }
    }

    fn model_ready(&self) -> bool {
        is_model_cached(self.model_id)
    }

    fn model_dir_exists(&self) -> bool {
        kind_by_id(self.model_id)
            .and_then(repo_cache_dir_for)
            .is_some_and(|dir| path_is_dir(&dir))
    }

    fn step_active(&self) -> bool {
        self.step.is_some()
    }

    fn set_blocked_notice(&mut self, cx: &mut Context<Self>) {
        if self.notice.is_none() {
            self.notice = Some(BLOCKED_NOTICE.to_owned());
            cx.notify();
        }
    }

    fn queue_start(&mut self, cx: &mut Context<Self>) {
        self.start_queued = true;
        cx.notify();
    }

    fn download_progress(&mut self, progress: DownloadProgress, cx: &mut Context<Self>) {
        self.eta.push(progress.done, Instant::now());
        self.progress = Some(progress);
        cx.notify();
    }

    fn download_cancelled(&mut self, cx: &mut Context<Self>) {
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

    fn download_failed(&mut self, error: String, cx: &mut Context<Self>) {
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

    fn download_finished_step(&mut self, cx: &mut Context<Self>) {
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

    fn delete_finished(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
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

    fn start_clicked(&mut self, cx: &mut Context<Self>) {
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

    fn reveal_clicked(&mut self, cx: &mut Context<Self>) {
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

    fn delete_clicked(&mut self, cx: &mut Context<Self>) {
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

    fn redownload_clicked(&mut self, cx: &mut Context<Self>) {
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

    fn render_step(&self, step: SetupStep, spec: &ModelSpec, cx: &mut Context<Self>) -> Div {
        let base = || {
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(24.))
                .bg(rgb(0x101010))
                .text_color(rgb(0xcccccc))
                .size_full()
        };
        match step {
            SetupStep::Welcome => base()
                .child(div().text_size(px(26.)).child("Dictation"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(14.))
                        .child("Hold")
                        .child(keycap("F9"))
                        .child(", speak, and your words appear in any app."),
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
                .child(primary_button(
                    "get-started",
                    if self.error.is_some() {
                        "Try again"
                    } else {
                        "Get started"
                    },
                    cx.listener(|this, _, _, cx| this.start_clicked(cx)),
                )),
            SetupStep::Downloading => {
                let eta = self
                    .progress
                    .as_ref()
                    .and_then(|progress| self.eta.estimate(progress.done, progress.total));
                base()
                    .child(div().text_size(px(20.)).child("Setting up your voice model…"))
                    .children(self.progress.as_ref().map(progress_bar))
                    .children(self.progress.as_ref().map(|progress| {
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0x33cc66))
                            .child(progress_summary(
                                progress.done,
                                progress.total,
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
                .child(div().text_size(px(24.)).child("You're all set."))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(14.))
                        .child("Hold")
                        .child(keycap("F9"))
                        .child("and speak — text appears wherever your cursor is."),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x909090))
                        .child("Fine-tune anything later in settings."),
                )
                .child(primary_button(
                    "begin",
                    "Start dictating",
                    cx.listener(|this, _, _, _| {
                        let _ = this.ui.send(UiMessage::FinishOnboarding {
                            captured_model: this.model_id,
                        });
                    }),
                )),
        }
    }
}

fn remove_dir_all_retrying(dir: &std::path::Path) -> Result<(), String> {
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

fn spawn_model_download(
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

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(spec) = spec_by_id(self.model_id) else {
            return div()
                .size_full()
                .bg(rgb(0x101010))
                .text_color(rgb(0xcccccc))
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
            .bg(rgb(0x101010))
            .text_color(rgb(0xcccccc))
            .size_full()
            .child(div().text_size(px(18.)).child("Dictation setup"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0x909090))
                    .child(format!(
                        "detected: {} GB RAM, {} logical cores",
                        self.ram_gb, self.cores
                    )),
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
                    .border_color(rgb(0x404040))
                    .rounded_sm()
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
                                        rgb(0x33cc66)
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
                            .text_color(rgb(0x33cc66))
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
                        .text_color(rgb(0x33cc66))
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
            .child(
                div()
                    .id("start")
                    .cursor_pointer()
                    .rounded_sm()
                    .px(px(16.))
                    .py(px(6.))
                    .bg(if self.busy {
                        rgb(0x141414)
                    } else {
                        rgb(0x1c1c1c)
                    })
                    .border_1()
                    .border_color(rgb(0x404040))
                    .text_size(px(13.))
                    .text_color(if self.busy {
                        rgb(0x606060)
                    } else {
                        rgb(0xcccccc)
                    })
                    .child("Start dictating")
                    .on_click(cx.listener(|this, _, _, cx| this.start_clicked(cx))),
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

fn keycap(label: &'static str) -> Div {
    div()
        .rounded_sm()
        .border_1()
        .border_color(rgb(0x404040))
        .bg(rgb(0x1c1c1c))
        .px(px(10.))
        .py(px(4.))
        .text_size(px(14.))
        .child(label)
}

fn primary_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .rounded_sm()
        .px(px(16.))
        .py(px(8.))
        .bg(rgb(0x33cc66))
        .text_color(rgb(0x101010))
        .text_size(px(14.))
        .child(label)
        .on_click(move |event, window, app| on_click(event, window, app))
}

fn progress_bar(progress: &DownloadProgress) -> Div {
    let fraction = if progress.total > 0 {
        (progress.done as f32 / progress.total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    div()
        .w_full()
        .h(px(3.))
        .rounded_sm()
        .bg(rgb(0x1e1e1e))
        .child(
            div()
                .h(px(3.))
                .rounded_sm()
                .bg(rgb(0x33cc66))
                .w(relative(fraction)),
        )
}

fn action_button(id: &'static str, label: &'static str, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .rounded_sm()
        .px(px(8.))
        .py(px(2.))
        .border_1()
        .border_color(rgb(0x404040))
        .text_size(px(13.))
        .bg(if enabled {
            rgb(0x1c1c1c)
        } else {
            rgb(0x141414)
        })
        .text_color(if enabled {
            rgb(0xcccccc)
        } else {
            rgb(0x606060)
        })
        .child(label)
}

#[derive(Clone)]
enum Screen {
    Onboarding {
        view: Entity<OnboardingView>,
        origin: SetupOrigin,
    },
    Dictation,
}

struct AppRoot {
    screen: Screen,
    dictation: Option<Entity<Dictation>>,
    commands: Option<mpsc::Sender<Command>>,
    events: mpsc::Sender<Event>,
    results: mpsc::Sender<PasteResult>,
    downloads: mpsc::Sender<DownloadMessage>,
    ui: mpsc::Sender<UiMessage>,
    pending_start: bool,
    download_generation: u64,
    cancel_flag: Option<Arc<AtomicBool>>,
    worker_live: bool,
    hwnd: Option<isize>,
}

impl AppRoot {
    fn hwnd_resolved(&mut self) -> Option<pw::HWND> {
        if self.hwnd.is_none() {
            match pw::find_by_title(&window_title_utf16()) {
                Some(hwnd) if pw::process_owns_window(hwnd) => {
                    self.hwnd = Some(hwnd as isize);
                }
                Some(_) => {
                    log!(
                        "app",
                        "ERROR: window with our title belongs to another process; not touching it"
                    );
                }
                None => {
                    log!("app", "ERROR: main window not found by title");
                }
            }
        }
        self.hwnd.map(|value| value as pw::HWND)
    }

    fn apply_pill_chrome(&mut self) {
        let Some(hwnd) = self.hwnd_resolved() else {
            return;
        };
        let (style, ex) = pw::styles(hwnd);
        let style = pw::pill_style(style);
        let ex = (ex & !pw::EX_CLEAR_MASK) | pw::EX_PILL;
        pw::set_styles(hwnd, style, ex);
        pw::set_text(hwnd, &window_title_utf16());
        let (frame_w, frame_h) =
            pw::frame_size_for_client(pw::PILL_WIDTH, pw::PILL_HEIGHT, style, ex);
        let (wx, wy, ww, wh) = pw::primary_work_area();
        let x = wx + (ww - frame_w) / 2;
        let y = wy + wh - frame_h - 12;
        pw::place(hwnd, x, y, frame_w, frame_h, true);
    }

    fn apply_panel_chrome(&mut self) {
        let Some(hwnd) = self.hwnd_resolved() else {
            return;
        };
        let (style, ex) = pw::styles(hwnd);
        let style = pw::panel_style(style);
        let ex = ex & !pw::EX_CLEAR_MASK;
        pw::set_styles(hwnd, style, ex);
        pw::set_text(hwnd, &panel_title_utf16());
        let (frame_w, frame_h) =
            pw::frame_size_for_client(pw::PANEL_WIDTH, pw::PANEL_HEIGHT, style, ex);
        let (ax, ay, aw, ah) = pw::primary_work_area();
        let x = ax + (aw - frame_w) / 2;
        let y = ay + (ah - frame_h) / 2;
        pw::place(hwnd, x, y, frame_w, frame_h, true);
    }

    fn close_panel_to_pill(&mut self, cx: &mut Context<Self>) {
        log!("app", "panel closed; returning to idle pill");
        match self.screen.clone() {
            Screen::Onboarding { origin, .. } => {
                if matches!(origin, SetupOrigin::Settings)
                    && let Some(dictation) = self.dictation.clone()
                {
                    dictation.update(cx, |dictation, cx| dictation.cancel_pending_start(cx));
                }
                if self.dictation.is_some() {
                    self.screen = Screen::Dictation;
                    self.hide_pill_window();
                } else {
                    log!("app", "onboarding not completed; hiding until restart");
                    self.hide_pill_window();
                }
            }
            Screen::Dictation => self.hide_pill_window(),
        }
        cx.notify();
    }

    fn show_pill_window(&mut self) {
        match self.hwnd_resolved() {
            Some(hwnd) => {
                self.apply_pill_chrome();
                pw::show_no_activate(hwnd);
                let rect = pw::window_rect_full(hwnd)
                    .map(|(l, t, r, b)| format!("({l},{t})-({r},{b})"))
                    .unwrap_or_else(|| "unavailable".to_owned());
                let (wax, way, waw, wah) = pw::primary_work_area();
                log!(
                    "app",
                    "pill shown: rect={rect} work=({},{},{},{}) dpi={} client={}x{}",
                    wax,
                    way,
                    waw,
                    wah,
                    pw::dpi(hwnd),
                    pw::PILL_WIDTH,
                    pw::PILL_HEIGHT
                );
            }
            None => log!("app", "ERROR: pill window not found by title"),
        }
    }

    fn hide_pill_window(&mut self) {
        if let Some(hwnd) = self.hwnd_resolved() {
            pw::hide(hwnd);
            log!("app", "pill hidden");
        }
    }

    fn show_panel_window(&mut self) {
        match self.hwnd_resolved() {
            Some(_) => {
                self.apply_panel_chrome();
                if let Some(hwnd) = self.hwnd_resolved() {
                    pw::show_no_activate(hwnd);
                    log!("app", "panel shown");
                }
            }
            None => log!("app", "ERROR: panel window not found by title"),
        }
    }
}

impl AppRoot {
    fn handle_asr_event(&mut self, event: Event, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| dictation.handle_asr_event(event, cx));
        }
    }

    fn handle_hotkey(&mut self, message: HotkeyMessage, cx: &mut Context<Self>) {
        match self.screen.clone() {
            Screen::Dictation => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| dictation.handle_hotkey(message, cx));
                }
            }
            Screen::Onboarding { view, origin } => match message {
                HotkeyMessage::ToggleRecording => match origin {
                    SetupOrigin::Settings | SetupOrigin::Respawn => {
                        log!("app", "F9 ignored while settings open");
                    }
                    SetupOrigin::FirstRun | SetupOrigin::Recovery => {
                        if view.read(cx).model_ready() || self.worker_live {
                            self.pending_start = true;
                            log!("app", "F9 during setup: queued start after models load");
                            view.update(cx, |onboarding, cx| onboarding.queue_start(cx));
                        } else {
                            log!(
                                "app",
                                "F9 blocked: model not fully cached and no worker loaded"
                            );
                            view.update(cx, |onboarding, cx| onboarding.set_blocked_notice(cx));
                        }
                    }
                },
                HotkeyMessage::DebugReload => log!("app", "F10 ignored during setup"),
            },
        }
    }

    fn handle_paste_result(&mut self, result: PasteResult, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| {
                dictation.handle_paste_result(result, cx)
            });
        }
    }

    fn pump_audio(
        &mut self,
        levels: &mut Vec<f32>,
        chunks: &mut Vec<Vec<f32>>,
        cx: &mut Context<Self>,
    ) {
        if let (Screen::Dictation, Some(dictation)) = (self.screen.clone(), self.dictation.clone())
        {
            let drained_levels = std::mem::take(levels);
            let drained_chunks = std::mem::take(chunks);
            dictation.update(cx, |dictation, cx| {
                if dictation.phase == Phase::Recording {
                    dictation.waveform.extend(drained_levels);
                    for chunk in drained_chunks {
                        let _ = dictation.commands.send(Command::Chunk(chunk));
                    }
                }
                cx.notify();
            });
        } else {
            levels.clear();
            chunks.clear();
        }
    }

    fn pump_pill(&mut self, _cx: &mut Context<Self>) {
        let Some(dictation) = self.dictation.clone() else {
            return;
        };
        let grab = dictation.read(_cx).drag_grab;
        if let (Some(hwnd), Some((gx, gy)), Some((cx_pos, cy_pos))) = (
            pw::find_by_title(&window_title_utf16()),
            grab,
            pw::cursor_pos(),
        ) {
            pw::move_to(hwnd, cx_pos - gx, cy_pos - gy);
        }
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Dictation) {
            return;
        }
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| dictation.cancel_pending_start(cx));
        }
        let default_id = ModelKind::Nemotron.spec().id;
        let model_id = config::load().map_or(default_id, |config| {
            spec_by_id(&config.model).map_or(default_id, |spec| spec.id)
        });
        log!("app", "opening settings: model={model_id}");
        let device = detect_device();
        let view = cx
            .new(|_| OnboardingView::new(SetupOrigin::Settings, device, model_id, self.ui.clone()));
        self.screen = Screen::Onboarding {
            view,
            origin: SetupOrigin::Settings,
        };
        self.show_panel_window();
        cx.notify();
    }

    fn start_download(&mut self, captured_model: &'static str, purge: bool) {
        let Some(spec) = spec_by_id(captured_model) else {
            log!("app", "download ignored: unknown model '{captured_model}'");
            return;
        };
        if purge && self.worker_live {
            match self.commands.clone() {
                Some(commands) => {
                    let _ = commands.send(Command::Shutdown);
                    log!("app", "releasing loaded model before re-download purge");
                    self.worker_live = false;
                }
                None => {
                    log!(
                        "app",
                        "ERROR: worker_live with no worker channel before purge"
                    );
                    self.worker_live = false;
                }
            }
        }
        self.download_generation += 1;
        let generation = self.download_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        log!(
            "app",
            "download started: {} (generation {generation}, purge={purge})",
            spec.id
        );
        spawn_model_download(spec, purge, generation, cancel, self.downloads.clone());
    }

    fn handle_download_message(&mut self, message: DownloadMessage, cx: &mut Context<Self>) {
        match message {
            DownloadMessage::Progress {
                generation,
                progress,
            } => {
                if generation != self.download_generation {
                    log!(
                        "app",
                        "stale download progress dropped (generation {generation})"
                    );
                    return;
                }
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| {
                        onboarding.download_progress(progress, cx)
                    });
                }
            }
            DownloadMessage::Finished { generation, result } => {
                if generation != self.download_generation {
                    log!(
                        "app",
                        "stale download finish dropped (generation {generation})"
                    );
                    return;
                }
                self.cancel_flag = None;
                match result {
                    Ok(model) => {
                        let step_flow_view = match self.screen.clone() {
                            Screen::Onboarding { view, .. } => Some(view),
                            Screen::Dictation => None,
                        };
                        let in_step_flow = step_flow_view
                            .as_ref()
                            .is_some_and(|view| view.read(cx).step_active());
                        if in_step_flow && let Some(view) = step_flow_view {
                            view.update(cx, |onboarding, cx| onboarding.download_finished_step(cx));
                        } else {
                            self.finish_onboarding(model, cx);
                        }
                    }
                    Err(error) => {
                        if let Screen::Onboarding { view, .. } = self.screen.clone() {
                            view.update(cx, |onboarding, cx| onboarding.download_failed(error, cx));
                        }
                    }
                }
            }
            DownloadMessage::Cancelled { generation } => {
                if generation != self.download_generation {
                    log!("app", "stale cancel dropped (generation {generation})");
                    return;
                }
                self.cancel_flag = None;
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| onboarding.download_cancelled(cx));
                }
            }
        }
    }

    fn handle_ui_message(&mut self, message: UiMessage, cx: &mut Context<Self>) {
        match message {
            UiMessage::OpenSettings => self.open_settings(cx),
            UiMessage::ShowPill => self.show_pill_window(),
            UiMessage::HidePill => self.hide_pill_window(),
            UiMessage::PanelClosed => self.close_panel_to_pill(cx),
            UiMessage::StartDownload {
                captured_model,
                purge,
            } => self.start_download(captured_model, purge),
            UiMessage::CancelDownload => {
                if let Some(flag) = &self.cancel_flag {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    log!("app", "cancel ignored: no download in flight");
                }
            }
            UiMessage::FinishOnboarding { captured_model } => {
                let kind = kind_by_id(captured_model).unwrap_or(ModelKind::Nemotron);
                self.finish_onboarding(kind, cx);
            }
            UiMessage::DeleteRequest { captured_model } => self.delete_model(captured_model),
            UiMessage::DeleteFinished(result) => {
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| onboarding.delete_finished(result, cx));
                }
            }
            UiMessage::ResetSetup { captured_model } => self.reset_setup(captured_model, cx),
        }
    }

    fn delete_model(&mut self, captured_model: &'static str) {
        let kind = kind_by_id(captured_model).unwrap_or(ModelKind::Nemotron);
        let Some(dir) = repo_cache_dir_for(kind) else {
            return;
        };
        if self.worker_live {
            match self.commands.clone() {
                Some(commands) => {
                    let _ = commands.send(Command::Shutdown);
                    log!("app", "releasing loaded model before delete");
                    self.worker_live = false;
                }
                None => {
                    log!(
                        "app",
                        "ERROR: worker_live with no worker channel before delete"
                    );
                    self.worker_live = false;
                }
            }
        }
        let ui = self.ui.clone();
        thread::spawn(move || {
            let result = remove_dir_all_retrying(&dir);
            let _ = ui.send(UiMessage::DeleteFinished(result));
        });
    }

    fn reset_setup(&mut self, captured_model: &'static str, cx: &mut Context<Self>) {
        match config::delete() {
            Ok(true) => log!("app", "setup reset: config deleted"),
            Ok(false) => log!("app", "setup reset: no config file present"),
            Err(error) => log!("app", "setup reset FAILED: {error}"),
        }
        let default_id = ModelKind::Nemotron.spec().id;
        let model_id = spec_by_id(captured_model).map_or(default_id, |spec| spec.id);
        log!("app", "re-entering setup with model={model_id}");
        let device = detect_device();
        let view = cx
            .new(|_| OnboardingView::new(SetupOrigin::Respawn, device, model_id, self.ui.clone()));
        self.screen = Screen::Onboarding {
            view,
            origin: SetupOrigin::Respawn,
        };
        cx.notify();
    }

    fn finish_onboarding(&mut self, model: ModelKind, cx: &mut Context<Self>) {
        let config = AppConfig {
            model: model.spec().id.to_owned(),
        };
        match config::save(&config) {
            Ok(()) => log!("app", "config saved: model={}", config.model),
            Err(error) => log!("app", "config save FAILED: {error}"),
        }
        if self.worker_live {
            match self.commands.clone() {
                Some(commands) => {
                    let _ = commands.send(Command::SetModel(model));
                    log!("app", "SetModel sent to live worker: {}", model.spec().id);
                }
                None => {
                    log!(
                        "app",
                        "ERROR: worker_live with no worker channel; cannot send SetModel"
                    );
                }
            }
            self.pending_start = false;
            self.screen = Screen::Dictation;
            self.hide_pill_window();
            cx.notify();
            return;
        }
        let selection = ModelSelection { model };
        let commands = asr::spawn_worker(self.events.clone(), selection);
        self.worker_live = true;
        log!("app", "worker spawned with selection {selection:?}");
        let pending_start = std::mem::replace(&mut self.pending_start, false);
        let dictation = cx.new(|_| {
            build_dictation(
                commands.clone(),
                self.results.clone(),
                self.ui.clone(),
                pending_start,
            )
        });
        self.dictation = Some(dictation);
        self.commands = Some(commands);
        self.screen = Screen::Dictation;
        self.hide_pill_window();
        cx.notify();
    }
}

impl Render for AppRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen.clone() {
            Screen::Onboarding { view, .. } => div().size_full().child(view),
            Screen::Dictation => match self.dictation.clone() {
                Some(dictation) => div().size_full().child(dictation),
                None => div().size_full(),
            },
        }
    }
}

fn window_title_utf16() -> Vec<u16> {
    WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn panel_title_utf16() -> Vec<u16> {
    "Dictation Setup"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn build_dictation(
    commands: mpsc::Sender<Command>,
    results: mpsc::Sender<PasteResult>,
    ui: mpsc::Sender<UiMessage>,
    pending_start: bool,
) -> Dictation {
    Dictation {
        phase: Phase::Loading,
        mode: Mode::Record,
        waveform: Waveform {
            bars: [0.0; BARS],
            peak: 0.0,
        },
        commands,
        results,
        ui,
        status: "fetching model".to_owned(),
        partial: String::new(),
        committed: String::new(),
        pending_start,
        transcribing_since: None,
        focus: None,
        recording_started: None,
        discard_requested: false,
        flash_since: None,
        hovered: false,
        drag_grab: None,
    }
}

fn selection_from_config(config: &AppConfig) -> ModelSelection {
    let fallback = ModelKind::Nemotron;
    let model = kind_by_id(&config.model).unwrap_or_else(|| {
        log!(
            "app",
            "unknown model id '{}' in config; using default instead",
            config.model
        );
        fallback
    });
    ModelSelection { model }
}

fn detect_device() -> (u32, usize) {
    let system = sysinfo::System::new_all();
    let ram_gb = (system.total_memory() / (1024 * 1024 * 1024)) as u32;
    let cores = system.cpus().len();
    (ram_gb, cores)
}

fn main() {
    logging::init();
    if !acquire_single_instance_lock() {
        log!("app", "another instance is running; exiting");
        return;
    }
    log!("app", "starting raycast-dictation-clone");
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
        let handle = cx
            .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(WINDOW_TITLE.to_owned().into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                focus: false,
                show: true,
                is_resizable: false,
                kind: WindowKind::PopUp,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                let (audio_sender, audio_receiver) = mpsc::channel::<Vec<f32>>();
                audio::spawn(audio_sender);

                let (event_sender, event_receiver) = mpsc::channel::<Event>();
                let (paste_sender, paste_receiver) = mpsc::channel::<PasteResult>();
                let (download_sender, download_receiver) = mpsc::channel::<DownloadMessage>();
                let (ui_sender, ui_receiver) = mpsc::channel::<UiMessage>();

                let loaded_config = config::load();
                let device = detect_device();

                let (dictation, commands, screen) = match loaded_config {
                    Some(config) => {
                        let selection = selection_from_config(&config);
                        let model_id = selection.model.spec().id;
                        if is_model_cached(model_id) {
                            log!("app", "config found: model={}", config.model);
                            let commands = asr::spawn_worker(event_sender.clone(), selection);
                            let dictation = cx.new(|_| {
                                build_dictation(
                                    commands.clone(),
                                    paste_sender.clone(),
                                    ui_sender.clone(),
                                    false,
                                )
                            });
                            (Some(dictation), Some(commands), Screen::Dictation)
                        } else {
                            log!(
                                "app",
                                "config found but cached model '{model_id}' missing; entering recovery setup"
                            );
                            let view = cx.new(|_| {
                                OnboardingView::new(
                                    SetupOrigin::Recovery,
                                    device,
                                    model_id,
                                    ui_sender.clone(),
                                )
                            });
                            (
                                None,
                                None,
                                Screen::Onboarding {
                                    view,
                                    origin: SetupOrigin::Recovery,
                                },
                            )
                        }
                    }
                    None => {
                        log!(
                            "app",
                            "no config; showing first-run setup (device: {} GB RAM, {} logical cores)",
                            device.0,
                            device.1
                        );
                        let view = cx.new(|_| {
                            OnboardingView::new(
                                SetupOrigin::FirstRun,
                                device,
                                "nemotron",
                                ui_sender.clone(),
                            )
                        });
                        (
                            None,
                            None,
                            Screen::Onboarding {
                                view,
                                origin: SetupOrigin::FirstRun,
                            },
                        )
                    }
                };

                let app = cx.new(|_| AppRoot {
                    worker_live: dictation.is_some(),
                    screen,
                    dictation,
                    commands,
                    events: event_sender,
                    results: paste_sender,
                    downloads: download_sender,
                    ui: ui_sender,
                    pending_start: false,
                    download_generation: 0,
                    cancel_flag: None,
                    hwnd: None,
                });

                window
                    .spawn(cx, {
                        let app = app.clone();
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
                                        app.update(cx, |app, cx| app.handle_asr_event(event, cx));
                                    })
                                    .ok();
                                }
                                while let Ok(message) = hotkey_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        app.update(cx, |app, cx| app.handle_hotkey(message, cx));
                                    })
                                    .ok();
                                }
                                while let Ok(result) = paste_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        app.update(cx, |app, cx| {
                                            app.handle_paste_result(result, cx)
                                        });
                                    })
                                    .ok();
                                }
                                while let Ok(message) = download_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        app.update(cx, |app, cx| {
                                            app.handle_download_message(message, cx)
                                        });
                                    })
                                    .ok();
                                }
                                while let Ok(message) = ui_receiver.try_recv() {
                                    cx.update(|_, cx| {
                                        app.update(cx, |app, cx| {
                                            app.handle_ui_message(message, cx)
                                        });
                                    })
                                    .ok();
                                }
                                cx.update(|_, cx| {
                                    app.update(cx, |app, cx| {
                                        app.pump_audio(
                                            &mut pending_levels,
                                            &mut pending_chunks,
                                            cx,
                                        );
                                        app.pump_pill(cx);
                                    });
                                })
                                .ok();
                                cx.background_executor().timer(POLL_INTERVAL).await;
                            }
                        }
                    })
                    .detach();
                app
            },
        )
        .unwrap();
        let _ = handle.update(cx, |app, window, cx| {
            let close_ui = app.ui.clone();
            window.on_window_should_close(cx, move |_, _| {
                log!("app", "panel close requested");
                let _ = close_ui.send(UiMessage::PanelClosed);
                false
            });
            if matches!(app.screen, Screen::Onboarding { .. }) {
                app.show_panel_window();
            } else if let Some(hwnd) = app.hwnd_resolved() {
                pw::hide(hwnd);
                log!("app", "window hidden at startup (idle pill)");
            }
        });
    });
}

fn acquire_single_instance_lock() -> bool {
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\raycast-dictation-single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle.is_null() {
        log!("app", "single-instance mutex creation failed");
        return false;
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    error == ERROR_ALREADY_EXISTS
}

fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}
