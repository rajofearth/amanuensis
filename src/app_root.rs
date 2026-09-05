use std::{
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
};

use amanuensis::asr::rewrite::{s1_llama_cli_path, s1_model_path};
use amanuensis::asr::{
    Command, Event, ModelKind, ModelSelection, is_model_cached, kind_by_id, repo_cache_dir_for,
    spec_by_id,
};
use amanuensis::audio;
use amanuensis::backend_detect;
use amanuensis::config::{self, AppConfig};
use amanuensis::esc_hook::EscHook;
use amanuensis::log;
use amanuensis::pill_win32::PillCommand;
use amanuensis::pill_window as pw;
use gpui::{Context, Entity, KeyDownEvent, Window, div, prelude::*};

use crate::dictation_view::{Dictation, Phase};
use crate::download_runner::{remove_dir_all_retrying, spawn_model_download, spawn_s1_download};
use crate::messages::{DownloadMessage, HotkeyMessage, PasteResult, UiMessage};
use crate::tray::TrayCommand;

const LOW_SIGNAL_PEAK: f32 = 0.05;
const LOW_SIGNAL_GAIN: f32 = 8.0;
use crate::onboarding_view::{OnboardingView, SetupOrigin};

pub(crate) const WINDOW_TITLE: &str = "amanuensis-window";
/// True while the chained s1-mini fetch (started from a finished moonshine
/// engine download) is in flight. Process-wide because there is a single
/// `AppRoot`/download channel; a struct field would break `main.rs`'s
/// exhaustive `AppRoot` literal. Cleared on every terminal s1 message and on
/// `start_download`, so a stale s1 message can never be misclassified.
static S1_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Set when the chained s1-mini fetch fails: the engine is usable raw, so a
/// later `finish_onboarding` must proceed instead of re-chaining (otherwise
/// every finish would retry ~500 MB forever). Cleared by the next engine
/// `start_download`, which re-arms the normal download-path chain.
static S1_FAILED: AtomicBool = AtomicBool::new(false);
#[derive(Clone)]
pub(crate) enum Screen {
    Onboarding {
        view: Entity<OnboardingView>,
        origin: SetupOrigin,
    },
    Dictation,
}

pub(crate) struct AppRoot {
    pub(crate) screen: Screen,
    pub(crate) dictation: Option<Entity<Dictation>>,
    pub(crate) commands: Option<mpsc::Sender<Command>>,
    pub(crate) events: mpsc::Sender<Event>,
    pub(crate) results: mpsc::Sender<PasteResult>,
    pub(crate) downloads: mpsc::Sender<DownloadMessage>,
    pub(crate) ui: mpsc::Sender<UiMessage>,
    pub(crate) pill_cmd: mpsc::Sender<PillCommand>,
    pub(crate) tray_commands: mpsc::Sender<TrayCommand>,
    pub(crate) esc: EscHook,
    pub(crate) recorder: audio::Recorder,
    pub(crate) pending_start: bool,
    pub(crate) download_generation: u64,
    pub(crate) cancel_flag: Option<Arc<AtomicBool>>,
    pub(crate) worker_live: bool,
    pub(crate) hwnd: Option<isize>,
}

impl AppRoot {
    pub(crate) fn hwnd_resolved(&mut self) -> Option<pw::HWND> {
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

    pub(crate) fn apply_pill_chrome(&mut self) {
        let Some(hwnd) = self.hwnd_resolved() else {
            return;
        };
        pw::remove_panel_wndproc(hwnd);
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

    pub(crate) fn apply_panel_chrome(&mut self) {
        let Some(hwnd) = self.hwnd_resolved() else {
            return;
        };
        pw::install_panel_wndproc(hwnd);
        let (style, ex) = pw::styles(hwnd);
        let style = pw::panel_style(style);
        let ex = (ex & !pw::EX_CLEAR_MASK)
            | windows_sys::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW;
        pw::set_styles(hwnd, style, ex);
        pw::demote_from_topmost(hwnd);
        pw::set_text(hwnd, &panel_title_utf16());
        let (frame_w, frame_h) = pw::panel_frame_size_for_client(
            pw::PANEL_WIDTH,
            pw::PANEL_HEIGHT,
            style,
            ex,
            pw::dpi(hwnd),
        );
        let (ax, ay, aw, ah) = pw::primary_work_area();
        let x = ax + (aw - frame_w) / 2;
        let y = ay + (ah - frame_h) / 2;
        pw::place(hwnd, x, y, frame_w, frame_h, true);
    }

    pub(crate) fn close_panel_to_pill(&mut self, cx: &mut Context<Self>) {
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
                    // Settings may have switched the record engine to
                    // moonshine while s1-mini was never fetched (cached
                    // engine → no download → no chain → rewrite silently
                    // raw). Kick the fetch off now, same rules as
                    // onboarding. Skipped while a download is in flight —
                    // its own terminal chains s1 — so the flight is never
                    // orphaned by a generation bump.
                    if self.cancel_flag.is_none()
                        && let Some(config) = config::load()
                        && let Some(kind) = kind_by_id(&config.record_model)
                    {
                        self.ensure_s1_chain(kind);
                    }
                } else {
                    log!("app", "onboarding panel closed; quitting app");
                    cx.quit();
                }
            }
            Screen::Dictation => self.hide_pill_window(),
        }
        cx.notify();
    }

    pub(crate) fn hide_pill_window(&mut self) {
        if let Some(hwnd) = self.hwnd_resolved() {
            self.apply_pill_chrome();
            pw::set_click_through(hwnd, true);
            log!("app", "pill idle: click-through");
        }
    }

    pub(crate) fn show_panel_window(&mut self) {
        match self.hwnd_resolved() {
            Some(_) => {
                self.apply_panel_chrome();
                if let Some(hwnd) = self.hwnd_resolved() {
                    pw::show_panel(hwnd);
                    log!("app", "panel shown");
                }
            }
            None => log!("app", "ERROR: panel window not found by title"),
        }
    }
}

impl AppRoot {
    pub(crate) fn handle_asr_event(&mut self, event: Event, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| dictation.handle_asr_event(event, cx));
        }
    }

    pub(crate) fn handle_hotkey(&mut self, message: HotkeyMessage, cx: &mut Context<Self>) {
        match self.screen.clone() {
            Screen::Dictation => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| dictation.handle_hotkey(message, cx));
                }
            }
            Screen::Onboarding { view, origin } => match message {
                HotkeyMessage::ToggleRecording => match origin {
                    SetupOrigin::Settings | SetupOrigin::Respawn => {
                        if self.dictation.is_some() {
                            log!("app", "F9 while settings open: returning to dictation");
                            self.close_panel_to_pill(cx);
                            if let Some(dictation) = self.dictation.clone() {
                                dictation
                                    .update(cx, |dictation, cx| dictation.toggle_recording(cx));
                            }
                        }
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

    fn handle_escape(&mut self, cx: &mut Context<Self>) {
        match self.screen.clone() {
            Screen::Dictation => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| dictation.discard_clicked(cx));
                }
            }
            Screen::Onboarding { origin, .. } if origin == SetupOrigin::Settings => {
                self.close_panel_to_pill(cx);
            }
            Screen::Onboarding { .. } => {}
        }
    }

    pub(crate) fn handle_paste_result(&mut self, result: PasteResult, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| {
                dictation.handle_paste_result(result, cx)
            });
        }
    }

    pub(crate) fn pump_audio(
        &mut self,
        levels: &mut Vec<f32>,
        chunks: &mut Vec<Vec<f32>>,
        cx: &mut Context<Self>,
    ) {
        // Claim the mic only while the user is actually using it: during a
        // recording or the setup mic check. When neither is active the device
        // stays closed so the OS never shows it as in use.
        let (phase, mic_check_active) = match &self.screen {
            Screen::Dictation => (self.dictation.as_ref().map(|d| d.read(cx).phase), false),
            Screen::Onboarding { view, .. } => (None, view.read(cx).mic_check_active()),
        };
        self.recorder
            .set_enabled(capture_wanted(phase, mic_check_active));
        match self.screen.clone() {
            Screen::Dictation => {
                if let Some(dictation) = self.dictation.clone() {
                    if phase == Some(Phase::Recording) {
                        let drained_levels = std::mem::take(levels);
                        let drained_chunks = std::mem::take(chunks);
                        dictation.update(cx, |dictation, cx| {
                            dictation.push_levels(drained_levels);
                            // Open the ASR session right before forwarding the
                            // first captured chunk, never before real audio
                            // exists (the worker drops chunks until Start).
                            if !drained_chunks.is_empty() && !dictation.start_sent {
                                let _ = dictation.commands.send(Command::Start {
                                    epoch: dictation.epoch,
                                });
                                dictation.start_sent = true;
                            }
                            for chunk in drained_chunks {
                                let _ = dictation
                                    .commands
                                    .send(Command::Chunk(prepare_asr_chunk(chunk)));
                            }
                            cx.notify();
                        });
                    } else {
                        levels.clear();
                        chunks.clear();
                    }
                } else {
                    levels.clear();
                    chunks.clear();
                }
            }
            Screen::Onboarding { view, .. } => {
                if mic_check_active && !levels.is_empty() {
                    let drained_levels = std::mem::take(levels);
                    view.update(cx, |onboarding, cx| {
                        onboarding.push_mic_levels(&drained_levels, cx)
                    });
                }
                chunks.clear();
            }
        }
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Dictation) {
            return;
        }
        if let Some(dictation) = self.dictation.clone() {
            dictation.update(cx, |dictation, cx| dictation.cancel_pending_start(cx));
        }
        let default_id = ModelKind::Moonshine.spec().id;
        let model_id = config::load().map_or(default_id, |config| {
            spec_by_id(&config.record_model).map_or(default_id, |spec| spec.id)
        });
        log!("app", "opening settings: record model={model_id}");
        let device = detect_device();
        let view = cx.new(|_| {
            OnboardingView::new(
                SetupOrigin::Settings,
                device,
                model_id,
                config::load().map_or(true, |config| config.tray_enabled),
                self.ui.clone(),
            )
        });
        self.screen = Screen::Onboarding {
            view,
            origin: SetupOrigin::Settings,
        };
        self.show_panel_window();
        cx.notify();
    }

    pub(crate) fn start_download(&mut self, captured_model: &'static str, purge: bool) {
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
        // A fresh engine download invalidates any chained s1 state; stale s1
        // messages are dropped by the generation check below. It also clears
        // a past s1 failure, re-arming the normal download-path chain.
        S1_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
        S1_FAILED.store(false, std::sync::atomic::Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        log!(
            "app",
            "download started: {} (generation {generation}, purge={purge})",
            spec.id
        );
        spawn_model_download(spec, purge, generation, cancel, self.downloads.clone());
    }

    /// Start the chained s1-mini fetch on `generation`: mark in-flight,
    /// install a fresh cancel flag, and spawn. Progress/terminal messages
    /// flow through `handle_download_message`, so readiness flips only on
    /// the s1 terminal (`engine_ready`) — the same rules as onboarding.
    fn start_s1_fetch(&mut self, engine: ModelKind, generation: u64) {
        S1_IN_FLIGHT.store(true, std::sync::atomic::Ordering::SeqCst);
        log!(
            "app",
            "engine {} cached but s1-mini assets missing; starting s1 fetch (generation {generation})",
            engine.spec().id
        );
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        spawn_s1_download(engine, generation, cancel, self.downloads.clone());
    }

    /// Chain the s1 fetch for a record engine that needs it but has no
    /// download in flight (settings switch onto a cached moonshine, or a
    /// direct Finish with s1 missing): fresh generation, same
    /// progress/ready rules as the onboarding chain. Returns true when the
    /// caller must stay unready (fetch started, or one already running).
    /// Never triggers when the engine itself is uncached (its own download
    /// chains s1 on Finish) and never retries a failed s1 (see `S1_FAILED`).
    fn ensure_s1_chain(&mut self, record: ModelKind) -> bool {
        if S1_IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        if S1_FAILED.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        if !Self::s1_chain_needed(record) {
            return false;
        }
        if !is_model_cached(record.spec().id) {
            return false;
        }
        self.download_generation += 1;
        let generation = self.download_generation;
        self.start_s1_fetch(record, generation);
        true
    }

    /// True when a moonshine record engine must chain the s1-mini fetch
    /// before flipping ready: rewrite enabled in the live config and at
    /// least one s1 asset missing. Nemotron-only flows never trigger s1. A
    /// missing config (first run) means pipeline defaults, where rewrite is
    /// on.
    fn s1_chain_needed(model: ModelKind) -> bool {
        if model != ModelKind::Moonshine {
            return false;
        }
        if !config::load().map(|c| c.rewrite_record).unwrap_or(true) {
            return false;
        }
        s1_model_path().is_none() || s1_llama_cli_path().is_none()
    }

    /// Engine (+ s1, when chained) present: advance the step flow or finish
    /// onboarding, consuming any queued pending-start.
    pub(crate) fn engine_ready(&mut self, model: ModelKind, cx: &mut Context<Self>) {
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

    pub(crate) fn handle_download_message(
        &mut self,
        message: DownloadMessage,
        cx: &mut Context<Self>,
    ) {
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
                        if S1_IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst) {
                            // S1 phase complete: engine + s1-mini both present.
                            S1_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
                            log!(
                                "app",
                                "s1-mini assets ready; engine + rewrite ready to record"
                            );
                            self.engine_ready(model, cx);
                        } else if Self::s1_chain_needed(model) {
                            // Engine done but s1-mini missing: stay unready and
                            // chain the s1 fetch on the same generation so the
                            // progress bar keeps moving. Readiness flips only
                            // when the s1 terminal message arrives below.
                            self.start_s1_fetch(model, generation);
                        } else {
                            self.engine_ready(model, cx);
                        }
                    }
                    Err(error) => {
                        if S1_IN_FLIGHT.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            // S1 failed: never block recording on it. The
                            // engine is cached; rewrite just stays unavailable.
                            // Record the failure so the `finish_onboarding`
                            // below proceeds raw instead of re-chaining.
                            S1_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
                            log!(
                                "app",
                                "ERROR: s1-mini download FAILED: {error}; continuing engine-ready with rewrite unavailable"
                            );
                            self.engine_ready(ModelKind::Moonshine, cx);
                        } else if let Screen::Onboarding { view, .. } = self.screen.clone() {
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
                if S1_IN_FLIGHT.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    log!(
                        "app",
                        "s1-mini download cancelled; engine cached, staying in setup"
                    );
                }
                if let Screen::Onboarding { view, .. } = self.screen.clone() {
                    view.update(cx, |onboarding, cx| onboarding.download_cancelled(cx));
                }
            }
        }
    }

    pub(crate) fn handle_ui_message(&mut self, message: UiMessage, cx: &mut Context<Self>) {
        match message {
            UiMessage::OpenSettings => self.open_settings(cx),
            UiMessage::TrayToggleRecording => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| {
                        dictation.toggle_recording(cx);
                    });
                }
            }
            UiMessage::TraySetEnabled(enabled) => {
                if let Err(error) = config::set_tray_enabled(enabled) {
                    log!("app", "tray preference save FAILED: {error}");
                }
                let _ = self.tray_commands.send(TrayCommand::SetEnabled(enabled));
            }
            UiMessage::Quit => std::process::exit(0),
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
            UiMessage::RecheckBackend => {
                let ui = self.ui.clone();
                thread::spawn(move || {
                    let winner = backend_detect::run_backend_bench();
                    let _ = ui.send(UiMessage::BackendBenchFinished(winner));
                });
            }
            UiMessage::BenchProgress(progress) => match self.screen.clone() {
                Screen::Onboarding { view, .. } => {
                    view.update(cx, |onboarding, cx| {
                        onboarding.on_bench_progress(progress, cx)
                    });
                }
                Screen::Dictation => {
                    log!(
                        "app",
                        "backend bench progress dropped while dictating: {progress:?}"
                    );
                }
            },
            UiMessage::BackendBenchFinished(winner) => match self.screen.clone() {
                Screen::Onboarding { view, .. } => {
                    view.update(cx, |onboarding, cx| {
                        onboarding.on_bench_finished(winner, cx)
                    });
                }
                Screen::Dictation => {
                    log!(
                        "app",
                        "backend bench finished while dictating: winner={:?}",
                        winner
                    );
                }
            },
            UiMessage::ResetSetup { captured_model } => self.reset_setup(captured_model, cx),
            UiMessage::PillDiscard => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| dictation.discard_clicked(cx));
                }
            }
            UiMessage::PillFinish => {
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| dictation.done_clicked(cx));
                }
            }
            UiMessage::QueueDictationStart => {
                let Screen::Onboarding { view, origin } = self.screen.clone() else {
                    return;
                };
                if !matches!(origin, SetupOrigin::FirstRun | SetupOrigin::Recovery) {
                    return;
                }
                if view.read(cx).model_ready() || self.worker_live {
                    self.pending_start = true;
                    log!(
                        "app",
                        "try-it clicked during setup: queued start after models load"
                    );
                    view.update(cx, |onboarding, cx| onboarding.queue_start(cx));
                } else {
                    log!(
                        "app",
                        "try-it blocked: model not fully cached and no worker loaded"
                    );
                    view.update(cx, |onboarding, cx| onboarding.set_blocked_notice(cx));
                }
            }
        }
    }

    pub(crate) fn delete_model(&mut self, captured_model: &'static str) {
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

    pub(crate) fn reset_setup(&mut self, captured_model: &'static str, cx: &mut Context<Self>) {
        match config::delete() {
            Ok(true) => log!("app", "setup reset: config deleted"),
            Ok(false) => log!("app", "setup reset: no config file present"),
            Err(error) => log!("app", "setup reset FAILED: {error}"),
        }
        let default_id = ModelKind::Nemotron.spec().id;
        let model_id = spec_by_id(captured_model).map_or(default_id, |spec| spec.id);
        log!("app", "re-entering setup with model={model_id}");
        let device = detect_device();
        let view = cx.new(|_| {
            OnboardingView::new(
                SetupOrigin::Respawn,
                device,
                model_id,
                config::load().map_or(true, |config| config.tray_enabled),
                self.ui.clone(),
            )
        });
        self.screen = Screen::Onboarding {
            view,
            origin: SetupOrigin::Respawn,
        };
        cx.notify();
    }

    pub(crate) fn finish_onboarding(&mut self, model: ModelKind, cx: &mut Context<Self>) {
        let existing = config::load();
        let existing_defaults = existing.clone().unwrap_or_default();
        let config = AppConfig {
            record_model: model.spec().id.to_owned(),
            live_model: existing
                .as_ref()
                .map(|c| c.live_model.clone())
                .unwrap_or_else(|| ModelKind::Nemotron.spec().id.to_owned()),
            model: model.spec().id.to_owned(),
            tray_enabled: existing.map_or(true, |c| c.tray_enabled),
            ..existing_defaults
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
        let selection = selection_from_config(&config);
        if self.ensure_s1_chain(selection.record_model) {
            // Record engine wants the rewrite pass but s1-mini is missing
            // (settings switch onto a cached moonshine, or a direct Finish
            // with no download in flight): stay unready and fetch it now.
            // The s1 terminal flips readiness via `engine_ready`.
            return;
        }
        if self.worker_live {
            // The old SetModel path is gone and the worker bakes its
            // selection in at spawn, so a newly picked engine needs a fresh
            // worker. Spawn first, then retire the old one, so the UI is
            // never left without a worker when the new engine is cached.
            let record_id = selection.record_model.spec().id;
            if is_model_cached(record_id) {
                log!(
                    "app",
                    "worker live; switching to newly picked engine {record_id}"
                );
                if let Some(dictation) = self.dictation.as_ref() {
                    let phase = dictation.read(cx).phase;
                    if matches!(phase, Phase::Recording | Phase::Transcribing) {
                        log!(
                            "app",
                            "WARNING: engine switch drops in-flight audio in phase {phase:?}; old worker's late Committed will be ignored"
                        );
                    }
                }
                let fresh = amanuensis::asr::spawn_worker(self.events.clone(), selection.clone());
                if let Some(old) = self.commands.replace(fresh.clone()) {
                    let _ = old.send(Command::Shutdown);
                }
                if let Some(dictation) = self.dictation.clone() {
                    dictation.update(cx, |dictation, cx| {
                        dictation.commands = fresh.clone();
                        cx.notify();
                    });
                }
            } else {
                log!(
                    "app",
                    "worker live; new engine {record_id} not cached, keeping existing worker"
                );
            }
            self.pending_start = false;
            self.screen = Screen::Dictation;
            self.hide_pill_window();
            cx.notify();
            return;
        }
        let cache = config::load().map(|c| c.backend_cache).unwrap_or_default();
        let profile = backend_detect::detect_hardware();
        if is_model_cached("nemotron") && backend_detect::needs_bench(&cache.asr, &profile) {
            log!("app", "post-onboarding backend bench scheduled");
            std::thread::spawn(backend_detect::run_backend_bench);
        }
        log!("app", "worker spawned with selection {selection:?}");
        let commands = amanuensis::asr::spawn_worker(self.events.clone(), selection);
        self.worker_live = true;
        let pending_start = std::mem::replace(&mut self.pending_start, false);
        let dictation = cx.new(|_| {
            Dictation::new(
                commands.clone(),
                self.results.clone(),
                self.pill_cmd.clone(),
                self.esc.clone(),
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
        let content = match self.screen.clone() {
            Screen::Onboarding { view, .. } => div().size_full().child(view),
            Screen::Dictation => match self.dictation.clone() {
                Some(dictation) => div().size_full().child(dictation),
                None => div().size_full(),
            },
        };
        content.on_key_down(_cx.listener(|this, event: &KeyDownEvent, _, cx| {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                this.handle_escape(cx);
            }
        }))
    }
}
pub(crate) fn window_title_utf16() -> Vec<u16> {
    WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

pub(crate) fn panel_title_utf16() -> Vec<u16> {
    "Amanuensis Settings"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

pub(crate) fn selection_from_config(config: &AppConfig) -> ModelSelection {
    let cached = &config.backend_cache.asr;
    let provider = (!cached.provider.is_empty()).then(|| cached.provider.clone());
    let threads = if cached.threads > 0 {
        cached.threads
    } else {
        2
    };
    let record = match kind_by_id(&config.record_model) {
        Some(kind) => kind,
        None => {
            log!(
                "app",
                "ERROR: unknown record_model '{}'; falling back to legacy model '{}'",
                config.record_model,
                config.model
            );
            match kind_by_id(&config.model) {
                Some(kind) => kind,
                None => {
                    log!(
                        "app",
                        "ERROR: unknown legacy model '{}'; using default moonshine",
                        config.model
                    );
                    ModelKind::Moonshine
                }
            }
        }
    };
    let live = match kind_by_id(&config.live_model) {
        Some(kind) => kind,
        None => {
            log!(
                "app",
                "ERROR: unknown live_model '{}'; falling back to legacy model '{}'",
                config.live_model,
                config.model
            );
            match kind_by_id(&config.model) {
                Some(kind) => kind,
                None => {
                    log!(
                        "app",
                        "ERROR: unknown legacy model '{}'; using default nemotron",
                        config.model
                    );
                    ModelKind::Nemotron
                }
            }
        }
    };
    let selection = ModelSelection {
        record_model: record,
        live_model: live,
        provider,
        threads,
        rewrite_record: config.rewrite_record,
        rewrite_threads: cached.threads.max(2),
    };
    amanuensis::telemetry::set_consent(config.telemetry_consent);
    selection
}

pub(crate) fn detect_device() -> (u32, usize) {
    let system = sysinfo::System::new_all();
    let ram_gb = (system.total_memory() / (1024 * 1024 * 1024)) as u32;
    let cores = system.cpus().len();
    (ram_gb, cores)
}

pub(crate) fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Whether the microphone should be captured this frame. `phase` is the
/// dictation phase, or `None` while in onboarding (no dictation yet).
fn capture_wanted(phase: Option<Phase>, mic_check_active: bool) -> bool {
    match phase {
        Some(Phase::Recording) => true,
        Some(_) => false,
        None => mic_check_active,
    }
}

fn prepare_asr_chunk(mut samples: Vec<f32>) -> Vec<f32> {
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak >= LOW_SIGNAL_PEAK {
        return samples;
    }
    for sample in &mut samples {
        *sample = (*sample * LOW_SIGNAL_GAIN).clamp(-1.0, 1.0);
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::{Phase, capture_wanted, prepare_asr_chunk};

    #[test]
    fn quiet_asr_chunks_are_boosted_without_clipping() {
        let boosted = prepare_asr_chunk(vec![0.01, -0.02]);
        assert_eq!(boosted, vec![0.08, -0.16]);
    }

    #[test]
    fn normal_asr_chunks_are_unchanged() {
        let samples = vec![0.05, -0.1];
        assert_eq!(prepare_asr_chunk(samples.clone()), samples);
    }

    #[test]
    fn mic_captured_only_while_recording_or_checking() {
        assert!(capture_wanted(Some(Phase::Recording), false));
        for phase in [
            Phase::Loading,
            Phase::Idle,
            Phase::Transcribing,
            Phase::Flash,
        ] {
            assert!(
                !capture_wanted(Some(phase), false),
                "{phase:?} must not capture"
            );
        }
        assert!(capture_wanted(None, true));
        assert!(!capture_wanted(None, false));
    }
}
