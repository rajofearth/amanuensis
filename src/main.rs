use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey},
};
use gpui::{
    App, Bounds, Context, Entity, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    relative, rgb, size,
};
use gpui_platform::application;
use raycast_dictation_clone::asr::{
    self, Command, Event, Mode, ModelKind, ModelSelection, ModelSpec, REGISTRY,
    ensure_model_by_spec, is_model_cached, kind_by_id, spec_by_id,
};
use raycast_dictation_clone::audio;
use raycast_dictation_clone::config::{self, AppConfig};
use raycast_dictation_clone::log;
use raycast_dictation_clone::logging;
use raycast_dictation_clone::paste::{MIN_AUDIO_SECS, clean_transcript, paste_text};
use raycast_dictation_clone::win_focus::{self, FocusTarget};

const BARS: usize = 26;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(50);

enum HotkeyMessage {
    ToggleRecording,
    DebugReload,
}

enum PasteResult {
    Pasted(usize),
    Failed(String),
}

enum DownloadMessage {
    Progress(String),
    Finished(Result<(ModelKind, ModelKind), String>),
}

enum UiMessage {
    OpenSettings,
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
        let _ = self.commands.send(Command::Start);
    }

    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        log!("app", "F9 toggle in phase {:?}", self.phase);
        match self.phase {
            Phase::Loading => self.pending_start = true,
            Phase::Idle => self.begin_recording(),
            Phase::Recording => {
                self.phase = Phase::Transcribing;
                self.transcribing_since = Some(Instant::now());
                let _ = self.commands.send(Command::Stop);
            }
            Phase::Transcribing => {}
        }
        cx.notify();
    }

    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Idle {
            return;
        }
        self.mode = match self.mode {
            Mode::Record => Mode::Live,
            Mode::Live => Mode::Record,
        };
        self.phase = Phase::Loading;
        self.status = format!("switching to {:?}", self.mode);
        let _ = self.commands.send(Command::SwitchMode(self.mode));
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
                    self.phase = Phase::Idle;
                    self.transcribing_since = None;
                    self.partial.clear();
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

    fn cancel_pending_start(&mut self, cx: &mut Context<Self>) {
        if self.pending_start {
            log!("app", "opening settings: cancelled queued start");
            self.pending_start = false;
            cx.notify();
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
                    .child(if self.phase == Phase::Transcribing {
                        let elapsed = self
                            .transcribing_since
                            .map_or(0, |started| started.elapsed().as_secs());
                        format!("{:?} (F9): transcribing… {elapsed}s", self.phase)
                    } else if self.phase == Phase::Loading || !self.status.is_empty() {
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
                    )
                    .children(
                        (self.phase != Phase::Recording && self.phase != Phase::Transcribing).then(
                            || {
                                div()
                                    .id("settings")
                                    .cursor_pointer()
                                    .rounded_sm()
                                    .px(px(8.))
                                    .py(px(2.))
                                    .bg(rgb(0x1c1c1c))
                                    .text_color(rgb(0x909090))
                                    .child("settings")
                                    .on_click(cx.listener(|this, _, _, _| this.open_settings()))
                            },
                        ),
                    ),
            )
            .children(
                (self.phase == Phase::Recording
                    && self.mode == Mode::Live
                    && !self.partial.is_empty())
                .then(|| {
                    div()
                        .max_w(px(720.))
                        .text_size(px(12.))
                        .text_color(rgb(0xcccccc))
                        .child(format!("partial: {}", self.partial))
                }),
            )
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

#[derive(Clone, Copy)]
enum Slot {
    Record,
    Live,
}

struct OnboardingView {
    record_id: &'static str,
    live_id: &'static str,
    ram_gb: u32,
    cores: usize,
    cached: Vec<bool>,
    status: Option<String>,
    error: Option<String>,
    downloading: bool,
    start_queued: bool,
    download: mpsc::Sender<DownloadMessage>,
}

impl OnboardingView {
    fn new(
        device: (u32, usize),
        download: mpsc::Sender<DownloadMessage>,
        record_id: &'static str,
        live_id: &'static str,
    ) -> Self {
        Self {
            record_id,
            live_id,
            ram_gb: device.0,
            cores: device.1,
            cached: REGISTRY
                .iter()
                .map(|spec| is_model_cached(spec.id))
                .collect(),
            status: None,
            error: None,
            downloading: false,
            start_queued: false,
            download,
        }
    }

    fn refresh_cached(&mut self) {
        for (index, spec) in REGISTRY.iter().enumerate() {
            self.cached[index] = is_model_cached(spec.id);
        }
    }

    fn assign(&mut self, slot: Slot, id: &'static str, cx: &mut Context<Self>) {
        match slot {
            Slot::Record => self.record_id = id,
            Slot::Live => self.live_id = id,
        }
        log!(
            "app",
            "model assigned: record={} live={}",
            self.record_id,
            self.live_id
        );
        cx.notify();
    }

    fn queue_start(&mut self, cx: &mut Context<Self>) {
        self.start_queued = true;
        cx.notify();
    }

    fn set_status(&mut self, text: String, cx: &mut Context<Self>) {
        self.status = Some(text);
        cx.notify();
    }

    fn download_failed(&mut self, error: String, cx: &mut Context<Self>) {
        log!("app", "download FAILED: {error}");
        self.downloading = false;
        self.error = Some(error);
        self.refresh_cached();
        cx.notify();
    }

    fn start_clicked(&mut self, cx: &mut Context<Self>) {
        if self.downloading {
            return;
        }
        let Some(record_spec) = spec_by_id(self.record_id) else {
            return;
        };
        let Some(live_spec) = spec_by_id(self.live_id) else {
            return;
        };
        self.downloading = true;
        self.error = None;
        self.status = Some(format!("checking {} …", record_spec.display_name));
        log!(
            "app",
            "download started: record={} live={}",
            record_spec.id,
            live_spec.id
        );
        let download = self.download.clone();
        thread::spawn(move || {
            let fetch = |spec, label| {
                let _ = download.send(DownloadMessage::Progress(format!("checking {label} …")));
                ensure_model_by_spec(spec, &mut |file| {
                    log!("app", "download progress: {file}");
                    let _ =
                        download.send(DownloadMessage::Progress(format!("downloading {file} …")));
                })
            };
            let record_result = fetch(record_spec, record_spec.display_name);
            let live_result = fetch(live_spec, live_spec.display_name);
            match (record_result, live_result) {
                (Ok(_), Ok(_)) => {
                    if let (Some(record), Some(live)) =
                        (kind_by_id(record_spec.id), kind_by_id(live_spec.id))
                    {
                        log!("app", "download finished for both models");
                        let _ = download.send(DownloadMessage::Finished(Ok((record, live))));
                    }
                }
                (Err(error), _) | (_, Err(error)) => {
                    let _ = download.send(DownloadMessage::Finished(Err(error)));
                }
            }
        });
        cx.notify();
    }

    fn model_row(
        &self,
        index: usize,
        spec: &ModelSpec,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected_record = self.record_id == spec.id;
        let selected_live = self.live_id == spec.id;
        let cached = self.cached[index];
        let record_id = spec.id;
        let live_id = spec.id;
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
                            .text_color(if cached { rgb(0x33cc66) } else { rgb(0x909090) })
                            .child(if cached { "cached" } else { "needs download" }),
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id(2 * index)
                            .cursor_pointer()
                            .rounded_sm()
                            .px(px(8.))
                            .py(px(2.))
                            .bg(if selected_record {
                                rgb(0x33cc66)
                            } else {
                                rgb(0x1c1c1c)
                            })
                            .text_color(if selected_record {
                                rgb(0x101010)
                            } else {
                                rgb(0xcccccc)
                            })
                            .child("use for record")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.assign(Slot::Record, record_id, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(2 * index + 1)
                            .cursor_pointer()
                            .rounded_sm()
                            .px(px(8.))
                            .py(px(2.))
                            .bg(if selected_live {
                                rgb(0x33cc66)
                            } else {
                                rgb(0x1c1c1c)
                            })
                            .text_color(if selected_live {
                                rgb(0x101010)
                            } else {
                                rgb(0xcccccc)
                            })
                            .child("use for live")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.assign(Slot::Live, live_id, cx)
                            })),
                    ),
            )
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut warning_specs: Vec<&ModelSpec> = Vec::new();
        for id in [self.record_id, self.live_id] {
            if let Some(spec) = spec_by_id(id)
                && !warning_specs.iter().any(|existing| existing.id == id)
                && spec.min_ram_gb > self.ram_gb
            {
                warning_specs.push(spec);
            }
        }
        let warnings: Vec<String> = warning_specs
            .iter()
            .map(|spec| {
                format!(
                    "{} recommends >= {} GB RAM (detected {} GB)",
                    spec.display_name, spec.min_ram_gb, self.ram_gb
                )
            })
            .collect();
        let mut rows = Vec::new();
        for (index, spec) in REGISTRY.iter().enumerate() {
            rows.push(self.model_row(index, spec, cx));
        }
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
            .children(rows)
            .children((!warnings.is_empty()).then(|| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0xcc9933))
                    .child(warnings.join("; "))
            }))
            .children(self.status.clone().map(|status| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0x33cc66))
                    .child(status)
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0xcc3333))
                    .child(format!("download failed: {error}"))
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
                    .bg(rgb(0x1c1c1c))
                    .border_1()
                    .border_color(rgb(0x404040))
                    .text_size(px(13.))
                    .child("Start dictating")
                    .on_click(cx.listener(|this, _, _, cx| this.start_clicked(cx))),
            )
    }
}

#[derive(Clone)]
enum Screen {
    Onboarding {
        view: Entity<OnboardingView>,
        from_hud: bool,
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
            Screen::Onboarding { view, from_hud } => match message {
                HotkeyMessage::ToggleRecording => {
                    if from_hud {
                        log!("app", "F9 ignored while settings open");
                    } else {
                        self.pending_start = true;
                        log!("app", "F9 during onboarding: queued start after setup");
                        view.update(cx, |onboarding, cx| onboarding.queue_start(cx));
                    }
                }
                HotkeyMessage::DebugReload => log!("app", "F10 ignored during onboarding"),
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

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Dictation) {
            return;
        }
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| dictation.cancel_pending_start(cx));
        }
        let config = config::load();
        let default_id = ModelKind::Nemotron.spec().id;
        let resolve =
            |id: &str| -> &'static str { spec_by_id(id).map_or(default_id, |spec| spec.id) };
        let record_id = config
            .as_ref()
            .map_or(default_id, |config| resolve(&config.record_model));
        let live_id = config
            .as_ref()
            .map_or(default_id, |config| resolve(&config.live_model));
        log!(
            "app",
            "opening settings from HUD: record={record_id} live={live_id}"
        );
        let device = detect_device();
        let view =
            cx.new(|_| OnboardingView::new(device, self.downloads.clone(), record_id, live_id));
        self.screen = Screen::Onboarding {
            view,
            from_hud: true,
        };
        cx.notify();
    }

    fn handle_download_message(&mut self, message: DownloadMessage, cx: &mut Context<Self>) {
        match message {
            DownloadMessage::Progress(text) => {
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| onboarding.set_status(text, cx));
                }
            }
            DownloadMessage::Finished(Ok((record, live))) => {
                self.finish_onboarding(record, live, cx)
            }
            DownloadMessage::Finished(Err(error)) => {
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| onboarding.download_failed(error, cx));
                }
            }
        }
    }

    fn finish_onboarding(&mut self, record: ModelKind, live: ModelKind, cx: &mut Context<Self>) {
        let config = AppConfig {
            record_model: record.spec().id.to_owned(),
            live_model: live.spec().id.to_owned(),
        };
        match config::save(&config) {
            Ok(()) => log!(
                "app",
                "config saved: record={} live={}",
                config.record_model,
                config.live_model
            ),
            Err(error) => log!("app", "config save FAILED: {error}"),
        }
        if matches!(self.screen, Screen::Onboarding { from_hud: true, .. }) {
            match self.commands.clone() {
                Some(commands) => {
                    let _ = commands.send(Command::SetModels { record, live });
                    log!(
                        "app",
                        "SetModels sent to worker: record={} live={}",
                        record.spec().id,
                        live.spec().id
                    );
                }
                None => log!("app", "ERROR: no worker channel for SetModels"),
            }
            self.screen = Screen::Dictation;
            cx.notify();
            return;
        }
        let selection = ModelSelection { record, live };
        let commands = asr::spawn_worker(self.events.clone(), selection);
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
    }
}

fn selection_from_config(config: &AppConfig) -> ModelSelection {
    let resolve = |id: &str, fallback: ModelKind| {
        kind_by_id(id).unwrap_or_else(|| {
            log!(
                "app",
                "unknown model id '{id}' in config; using default instead"
            );
            fallback
        })
    };
    ModelSelection {
        record: resolve(&config.record_model, ModelKind::Nemotron),
        live: resolve(&config.live_model, ModelKind::Nemotron),
    }
}

fn detect_device() -> (u32, usize) {
    let system = sysinfo::System::new_all();
    let ram_gb = (system.total_memory() / (1024 * 1024 * 1024)) as u32;
    let cores = system.cpus().len();
    (ram_gb, cores)
}

fn main() {
    logging::init();
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
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
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

                let (dictation, commands) = match &loaded_config {
                    Some(config) => {
                        let selection = selection_from_config(config);
                        log!(
                            "app",
                            "config found: record={} live={}",
                            config.record_model,
                            config.live_model
                        );
                        let commands = asr::spawn_worker(event_sender.clone(), selection);
                        let dictation = cx.new(|_| {
                            build_dictation(
                                commands.clone(),
                                paste_sender.clone(),
                                ui_sender.clone(),
                                false,
                            )
                        });
                        (Some(dictation), Some(commands))
                    }
                    None => {
                        log!(
                            "app",
                            "no config; showing onboarding (device: {} GB RAM, {} logical cores)",
                            device.0,
                            device.1
                        );
                        (None, None)
                    }
                };

                let screen = if dictation.is_some() {
                    Screen::Dictation
                } else {
                    Screen::Onboarding {
                        view: cx.new(|_| {
                            OnboardingView::new(
                                device,
                                download_sender.clone(),
                                "nemotron",
                                "nemotron",
                            )
                        }),
                        from_hud: false,
                    }
                };

                let app = cx.new(|_| AppRoot {
                    screen,
                    dictation,
                    commands,
                    events: event_sender,
                    results: paste_sender,
                    downloads: download_sender,
                    ui: ui_sender,
                    pending_start: false,
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
                                        app.update(cx, |app, cx| match message {
                                            UiMessage::OpenSettings => app.open_settings(cx),
                                        });
                                    })
                                    .ok();
                                }
                                cx.update(|_, cx| {
                                    app.update(cx, |app, cx| {
                                        app.pump_audio(&mut pending_levels, &mut pending_chunks, cx)
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
        cx.activate(true);
    });
}

fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}
