#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_root;
mod brand_assets;
mod dictation_view;
mod download_runner;
mod installer_ui;
mod messages;
mod onboarding_view;
mod tray;

use std::{sync::mpsc, thread, time::Duration};

use amanuensis::asr::{self, Event, is_model_cached};
use amanuensis::audio;
use amanuensis::backend_detect;
use amanuensis::config;
use amanuensis::esc_hook;
use amanuensis::installer::{self, LaunchMode};
use amanuensis::log;
use amanuensis::logging;
use amanuensis::pill_win32::PillButton;
use amanuensis::pill_win32::PillOverlay;
use amanuensis::pill_window as pw;
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey},
};
use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, px, size,
};
use gpui_platform::application;

use crate::app_root::{AppRoot, Screen, detect_device, rms_level, selection_from_config};
use crate::dictation_view::Dictation;
use crate::messages::{DownloadMessage, HotkeyMessage, PasteResult, UiMessage};
use crate::onboarding_view::{OnboardingView, SetupOrigin};
use crate::tray::TrayController;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const HOTKEY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WINDOW_TITLE: &str = "amanuensis-window";

fn main() {
    logging::init();
    if std::env::args().any(|argument| argument == backend_detect::PROBE_FLAG) {
        let code = amanuensis::backend_detect::probe_child();
        std::process::exit(code);
    }
    match installer::detect_mode() {
        LaunchMode::App => {}
        mode => {
            log!("installer", "installer launch mode detected ({mode:?})");
            installer_ui::run(mode);
            return;
        }
    }
    if !acquire_single_instance_lock() {
        log!("app", "another instance is running; exiting");
        return;
    }
    log!("app", "starting Amanuensis");
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
                let recorder = audio::spawn(audio_sender);

                let (event_sender, event_receiver) = mpsc::channel::<Event>();
                let (paste_sender, paste_receiver) = mpsc::channel::<PasteResult>();
                let (download_sender, download_receiver) = mpsc::channel::<DownloadMessage>();
                let (ui_sender, ui_receiver) = mpsc::channel::<UiMessage>();
                let pill = PillOverlay::spawn();
                let (esc, esc_events) = esc_hook::spawn();
                let loop_ui = ui_sender.clone();

                let loaded_config = config::load().or_else(|| {
                    // Older builds could finish downloading before saving setup
                    // state. A complete default model is enough to safely
                    // recover that install without replaying onboarding.
                    let model = asr::ModelKind::Nemotron;
                    if !is_model_cached(model.spec().id) {
                        return None;
                    }
                    let recovered = config::AppConfig {
                        model: model.spec().id.to_owned(),
                        tray_enabled: true,
                        ..Default::default()
                    };
                    match config::save(&recovered) {
                        Ok(()) => {
                            log!("app", "recovered config from cached model: {}", recovered.model);
                            Some(recovered)
                        }
                        Err(error) => {
                            log!("app", "cached-model config recovery FAILED: {error}");
                            None
                        }
                    }
                });
                let tray_enabled = loaded_config
                    .as_ref()
                    .map_or(true, |config| config.tray_enabled);
                let tray = TrayController::spawn(ui_sender.clone(), tray_enabled);
                let device = detect_device();

                // Optional background ASR backend benchmark. Runs only once the
                // model is cached and the cached pick is stale; the winner is
                // persisted for the next launch. Never blocks the pending-start
                // path: the UI/worker are spawned regardless, on this thread.
                if is_model_cached("nemotron") {
                    let cached_asr = loaded_config
                        .as_ref()
                        .map(|config| config.backend_cache.asr.clone())
                        .unwrap_or_default();
                    let profile = backend_detect::detect_hardware();
                    if backend_detect::needs_bench(&cached_asr, &profile) {
                        log!("app", "background backend bench scheduled");
                        std::thread::spawn(backend_detect::run_backend_bench);
                    }
                }

                let (dictation, commands, screen) = match loaded_config {
                    Some(config) => {
                        let selection = selection_from_config(&config);
                        let model_id = selection.model.spec().id;
                        if is_model_cached(model_id) {
                            log!("app", "config found: model={}", config.model);
                            let commands = asr::spawn_worker(event_sender.clone(), selection);
                            let dictation = cx.new(|_| {
                                Dictation::new(
                                    commands.clone(),
                                    paste_sender.clone(),
                                    pill.command_tx(),
                                    esc.clone(),
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
                                    tray_enabled,
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
                                tray_enabled,
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
                    pill_cmd: pill.command_tx(),
                    tray_commands: tray.sender(),
                    esc: esc.clone(),
                    recorder,
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
                                while let Some(button) = pill.try_recv_button() {
                                    let message = match button {
                                        PillButton::Discard => UiMessage::PillDiscard,
                                        PillButton::Finish => UiMessage::PillFinish,
                                        PillButton::OpenSettings => UiMessage::OpenSettings,
                                    };
                                    let _ = loop_ui.send(message);
                                }
                                // Escape pressed anywhere while recording:
                                // reuse the exact pill ✕ discard path.
                                while esc_events.try_recv_escape() {
                                    let _ = loop_ui.send(UiMessage::PillDiscard);
                                }
                                cx.update(|_, cx| {
                                    app.update(cx, |app, cx| {
                                        app.pump_audio(
                                            &mut pending_levels,
                                            &mut pending_chunks,
                                            cx,
                                        );
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
                app.apply_pill_chrome();
                pw::set_click_through(hwnd, true);
                log!("app", "window idle at startup (invisible click-through chrome)");
            }
        });
    });
}

fn acquire_single_instance_lock() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\amanuensis-single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle.is_null() {
        log!("app", "single-instance mutex creation failed");
        return false;
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        log!("app", "another Amanuensis instance is already running");
        return false;
    }
    true
}
