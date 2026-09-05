//! Installer GPUI front end (PART 2): one window rendering the Intro
//! (fresh install), Update, or Uninstall screen, then morphing into a
//! frameless draggable progress view while the ops pipeline runs on a worker
//! thread.
//!
//! Chrome morphing reuses the pill_window subclass approach with its OWN
//! wndproc static so App-mode panel state is never touched.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicIsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::brand_assets;
use amanuensis::installer::{
    self, ALL_STEPS, APP_VERSION, InstallOptions, InstalledInfo, LaunchMode, Step,
};
use amanuensis::log;
use amanuensis::pill_window as pw;
use gpui::{
    App, Bounds, Context, Div, Entity, IntoElement, Render, Stateful, TitlebarOptions, Window,
    WindowBounds, WindowKind, WindowOptions, div, prelude::*, px, relative, rgb, size,
};
use gpui_platform::application;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, GetWindowRect, HTCAPTION,
    SetWindowLongPtrW, WM_NCHITTEST, WNDPROC,
};

use crate::onboarding_view::{action_button, primary_button, text_button};

const TAG: &str = "installer";
/// Distinct from the app's "amanuensis-window" title on purpose: ops.rs
/// enumerates that title when terminating a running instance, and the
/// installer window must never match it.
/// Win32 window text for the installer window. Must equal the TitlebarOptions
/// title AND stay constant for the window's whole life: hwnd_resolved()
/// locates the window with FindWindowW, which matches this text. (Never
/// "amanuensis-window" — ops' close-running-instance logic enumerates that.)
const WINDOW_TITLE: &str = "Amanuensis Setup";
const WINDOW_WIDTH: f32 = 520.;
const WINDOW_HEIGHT: f32 = 400.;
/// Top strip of the frameless progress window routed to HTCAPTION for drag.
const DRAG_STRIP_PX: i32 = 44;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
/// How long the success state stays visible before the installer exits.
const SUCCESS_LINGER: Duration = Duration::from_millis(1200);
const UNINSTALL_SUCCESS_LINGER: Duration = Duration::from_millis(600);

const TAGLINE: &str =
    "Press F9 anywhere, speak, and your words become text. It all runs on your PC.";
const LAUNCHING_LINE: &str = "Launching Amanuensis…";

#[derive(Clone, Debug)]
enum Pipeline {
    Install,
    Update(InstalledInfo),
    Uninstall,
}

impl Pipeline {
    fn is_uninstall(&self) -> bool {
        matches!(self, Pipeline::Uninstall)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Choosing,
    Progress,
}

/// Events flowing from worker threads into the UI pump loop.
enum WorkerEvent {
    /// Fired by the ops layer BEFORE each step starts running.
    StepStarted {
        message: String,
    },
    FolderPicked(Option<PathBuf>),
    Finished(Result<(), String>),
}

struct InstallerApp {
    pipeline: Pipeline,
    stage: Stage,
    dir: PathBuf,
    start_menu: bool,
    autostart: bool,
    keep_data: bool,
    completed: Vec<String>,
    active: Option<String>,
    error: Option<String>,
    finished: bool,
    quit_deadline: Option<Instant>,
    hwnd: Option<isize>,
    sender: mpsc::Sender<WorkerEvent>,
    events: mpsc::Receiver<WorkerEvent>,
}

/// Entry point called from main() for non-App launch modes.
pub fn run(mode: LaunchMode) {
    if let LaunchMode::App = mode {
        log!(TAG, "BUG: installer UI asked to run in App mode; ignoring");
        return;
    }
    application().run(|cx| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        // Must equal WINDOW_TITLE — hwnd_resolved() finds this
                        // window via FindWindowW on that text.
                        title: Some(WINDOW_TITLE.to_owned().into()),
                        ..Default::default()
                    }),
                    focus: true,
                    show: true,
                    is_resizable: false,
                    kind: WindowKind::Normal,
                    ..Default::default()
                },
                |window, cx| {
                    let (sender, receiver) = mpsc::channel();
                    let app = cx.new(|_| InstallerApp::for_mode(mode, sender, receiver));
                    pump_worker_events(window, cx, app.clone());
                    app
                },
            )
            .unwrap();
        let _ = handle.update(cx, |_, window, cx| {
            let app = cx.entity();
            window.on_window_should_close(cx, move |_, cx| {
                app.update(cx, |installer, _| installer.request_close())
            });
        });
        log!(TAG, "installer window open");
    });
}

impl InstallerApp {
    fn for_mode(
        mode: LaunchMode,
        sender: mpsc::Sender<WorkerEvent>,
        events: mpsc::Receiver<WorkerEvent>,
    ) -> Self {
        let (pipeline, dir, start_menu, autostart) = match mode {
            LaunchMode::Install => (
                Pipeline::Install,
                installer::default_install_dir(),
                true,
                false,
            ),
            LaunchMode::Update(info) => (
                // Toggles pre-filled from CURRENT machine state per spec.
                Pipeline::Update(info.clone()),
                info.dir.clone(),
                installer::start_menu_shortcut_exists(),
                installer::autostart_enabled(),
            ),
            LaunchMode::Uninstall => (
                Pipeline::Uninstall,
                installer::install_location(),
                false,
                false,
            ),
            LaunchMode::App => {
                log!(
                    TAG,
                    "BUG: App mode reached installer UI; defaulting to Install"
                );
                (
                    Pipeline::Install,
                    installer::default_install_dir(),
                    true,
                    false,
                )
            }
        };
        log!(
            TAG,
            "installer UI ready ({pipeline:?}) dir={}",
            dir.display()
        );
        Self {
            pipeline,
            stage: Stage::Choosing,
            dir,
            start_menu,
            autostart,
            keep_data: true,
            completed: Vec::new(),
            active: None,
            error: None,
            finished: false,
            quit_deadline: None,
            hwnd: None,
            sender,
            events,
        }
    }

    // ---- Actions ----

    fn options(&self) -> InstallOptions {
        InstallOptions {
            dir: self.dir.clone(),
            start_menu: self.start_menu,
            autostart: self.autostart,
        }
    }

    fn start_pipeline(&mut self, cx: &mut Context<Self>) {
        let Some(hwnd) = self.hwnd_resolved() else {
            log!(TAG, "ERROR: installer window not found; cannot start");
            self.error = Some("installer window not found".to_owned());
            cx.notify();
            return;
        };
        apply_progress_chrome(hwnd);
        self.stage = Stage::Progress;
        self.completed.clear();
        self.active = None;
        self.error = None;
        self.finished = false;
        self.quit_deadline = None;

        let sender = self.sender.clone();
        let pipeline = self.pipeline.clone();
        let opts = self.options();
        let keep_data = self.keep_data;
        thread::spawn(move || {
            let mut on_step = |step: Step, message: &str| {
                log!(TAG, "step {step:?}: {message}");
                let _ = sender.send(WorkerEvent::StepStarted {
                    message: message.to_owned(),
                });
            };
            let result = match &pipeline {
                Pipeline::Install => installer::run_install(&opts, &mut on_step),
                Pipeline::Update(installed) => {
                    installer::run_update(&opts, installed, &mut on_step)
                }
                Pipeline::Uninstall => installer::run_uninstall(keep_data, &mut on_step),
            };
            if let Err(error) = &result {
                log!(TAG, "pipeline FAILED: {error}");
            }
            let _ = sender.send(WorkerEvent::Finished(result));
        });
        cx.notify();
    }

    fn browse_clicked(&mut self, _cx: &mut Context<Self>) {
        let Some(hwnd) = self.hwnd_resolved() else {
            return;
        };
        // Raw HWND is !Send; hand the thread only the integer handle.
        let hwnd = hwnd as isize;
        let sender = self.sender.clone();
        thread::spawn(move || {
            let picked = installer::pick_folder(hwnd);
            let _ = sender.send(WorkerEvent::FolderPicked(picked));
        });
    }

    fn retry_clicked(&mut self, cx: &mut Context<Self>) {
        log!(TAG, "retry requested");
        self.start_pipeline(cx);
    }

    /// Window X button / Alt+F4. Blocked while the pipeline runs; otherwise
    /// exits directly (repo idiom for Quit, see main.rs UiMessage::Quit).
    fn request_close(&mut self) -> bool {
        if self.stage == Stage::Progress && !self.finished {
            log!(TAG, "close blocked while pipeline is running");
            return false;
        }
        log!(TAG, "close requested; exiting");
        std::process::exit(0);
    }

    // ---- Event pump ----

    fn handle_event(&mut self, event: WorkerEvent, cx: &mut Context<Self>) {
        match event {
            WorkerEvent::StepStarted { message } => {
                // on_step fires BEFORE each step, so the previously active
                // step is complete by now.
                if let Some(previous) = self.active.take() {
                    self.completed.push(previous);
                }
                self.active = Some(message);
                cx.notify();
            }
            WorkerEvent::FolderPicked(Some(dir)) => {
                log!(TAG, "picked install dir {}", dir.display());
                self.dir = dir;
                cx.notify();
            }
            WorkerEvent::FolderPicked(None) => {}
            WorkerEvent::Finished(Ok(())) => {
                if let Some(active) = self.active.take() {
                    self.completed.push(active);
                }
                self.finished = true;
                if !self.pipeline.is_uninstall() {
                    // The ops layer already launched the installed exe.
                    self.active = Some(LAUNCHING_LINE.to_owned());
                }
                self.quit_deadline = Some(
                    Instant::now()
                        + if self.pipeline.is_uninstall() {
                            UNINSTALL_SUCCESS_LINGER
                        } else {
                            SUCCESS_LINGER
                        },
                );
                log!(TAG, "pipeline finished OK");
                cx.notify();
            }
            WorkerEvent::Finished(Err(error)) => {
                self.finished = true;
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    /// Checked every pump tick; returns true once the success linger elapsed.
    fn poll_quit(&mut self) -> bool {
        match self.quit_deadline {
            Some(deadline) if Instant::now() >= deadline => {
                self.quit_deadline = None;
                true
            }
            _ => false,
        }
    }

    fn hwnd_resolved(&mut self) -> Option<pw::HWND> {
        if self.hwnd.is_none() {
            match pw::find_by_title(&window_title_utf16()) {
                Some(hwnd) if pw::process_owns_window(hwnd) => self.hwnd = Some(hwnd as isize),
                _ => log!(TAG, "ERROR: installer window not found by title"),
            }
        }
        self.hwnd.map(|value| value as pw::HWND)
    }

    // ---- Rendering ----

    fn render_choosing(&self, cx: &mut Context<Self>) -> Div {
        match &self.pipeline {
            Pipeline::Uninstall => self.render_uninstall_confirm(cx),
            Pipeline::Update(_) | Pipeline::Install => self.render_intro_update(cx),
        }
    }

    /// Intro (fresh install) and Update screens share layout; differences:
    /// path editability, version arrow, button label, pre-filled toggles.
    fn render_intro_update(&self, cx: &mut Context<Self>) -> Div {
        let updating = matches!(self.pipeline, Pipeline::Update(_));
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(24.))
            .bg(rgb(0x0d0e10))
            .text_color(rgb(0xe5e7eb))
            .size_full()
            .child(brand_assets::horizontal(190.))
            .child(div().text_size(px(26.)).child("Amanuensis"))
            .child(div().text_size(px(14.)).child(TAGLINE))
            .children(updating.then(|| {
                let installed = match &self.pipeline {
                    Pipeline::Update(info) => info.version.as_str(),
                    _ => "",
                };
                div()
                    .text_size(px(13.))
                    .text_color(rgb(0x909090))
                    .child(version_arrow(installed, APP_VERSION))
            }))
            .child(
                div()
                    .id("installer-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .gap(px(14.))
                    .child(self.render_path_row(cx))
                    .child(toggle_row(
                        "start-menu-toggle",
                        "Start menu shortcut",
                        "Adds Amanuensis to your all-apps list.",
                        self.start_menu,
                        cx.listener(|this, _, _, cx| {
                            this.start_menu = !this.start_menu;
                            cx.notify();
                        }),
                    ))
                    .child(toggle_row(
                        "autostart-toggle",
                        "Start with Windows",
                        "Launches Amanuensis when you sign in.",
                        self.autostart,
                        cx.listener(|this, _, _, cx| {
                            this.autostart = !this.autostart;
                            cx.notify();
                        }),
                    )),
            )
            .child(footer(
                text_button(
                    "cancel",
                    "Cancel",
                    cx.listener(|_, _, _, cx| {
                        log!(TAG, "cancelled from intro");
                        cx.quit();
                    }),
                ),
                primary_button(
                    if updating {
                        "update-btn"
                    } else {
                        "install-btn"
                    },
                    if updating { "Update" } else { "Install" },
                    cx.listener(|this, _, _, cx| this.start_pipeline(cx)),
                ),
            ))
    }

    fn render_path_row(&self, cx: &mut Context<Self>) -> Div {
        let browsable = matches!(self.pipeline, Pipeline::Install);
        div()
            .flex()
            .items_center()
            .justify_between()
            .border_1()
            .border_color(rgb(0x2e2e2e))
            .px(px(12.))
            .py(px(8.))
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(13.))
                    .text_color(rgb(0xd8d8d8))
                    .child(self.dir.display().to_string()),
            )
            .children(browsable.then(|| {
                text_button(
                    "browse",
                    "Browse…",
                    cx.listener(|this, _, _, cx| this.browse_clicked(cx)),
                )
            }))
    }

    fn render_uninstall_confirm(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(24.))
            .bg(rgb(0x0d0e10))
            .text_color(rgb(0xe5e7eb))
            .size_full()
            .child(div().text_size(px(26.)).child("Uninstall Amanuensis?"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0x909090))
                            .child("Amanuensis will be removed from this PC."),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0x808080))
                            .child(format!("Installed at {}", self.dir.display())),
                    ),
            )
            .child(
                div()
                    .id("installer-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .gap(px(14.))
                    .child(
                        div()
                            .id("keep-data")
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .border_1()
                            .border_color(rgb(0x2e2e2e))
                            .px(px(12.))
                            .py(px(8.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(14.))
                                    .border_1()
                                    .border_color(rgb(0x505050))
                                    .bg(if self.keep_data {
                                        rgb(0x2a2a2a)
                                    } else {
                                        rgb(0x171717)
                                    })
                                    .text_size(px(11.))
                                    .text_color(rgb(0xf2f2f2))
                                    .child(if self.keep_data { "✓" } else { "" }),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .child("Keep my models and settings"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.keep_data = !this.keep_data;
                                cx.notify();
                            })),
                    ),
            )
            .child(footer(
                text_button(
                    "cancel",
                    "Cancel",
                    cx.listener(|_, _, _, cx| {
                        log!(TAG, "cancelled from uninstall confirm");
                        cx.quit();
                    }),
                ),
                primary_button(
                    "remove",
                    "Remove",
                    cx.listener(|this, _, _, cx| this.start_pipeline(cx)),
                ),
            ))
    }

    fn render_progress(&self, cx: &mut Context<Self>) -> Div {
        let fraction =
            progress_fraction(self.completed.len(), self.finished && self.error.is_none());
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .px(px(24.))
            .pt(px(DRAG_STRIP_PX as f32))
            .pb(px(24.))
            .bg(rgb(0x0d0e10))
            .text_color(rgb(0xe5e7eb))
            .size_full()
            .child(
                div()
                    .flex()
                    .justify_center()
                    .child(div().text_size(px(22.)).child("Amanuensis")),
            )
            .child(progress_bar(fraction))
            .children(self.completed.iter().map(status_done_row))
            .children(self.active.clone().map(|label| {
                div()
                    .text_size(px(13.))
                    .text_color(rgb(0xe5e7eb))
                    .child(label)
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .text_size(px(13.))
                    .text_color(rgb(0xcc3333))
                    .child(format!("Something went wrong: {error}"))
            }))
            .children(self.error.is_some().then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        action_button("retry", "Retry", true)
                            .on_click(cx.listener(|this, _, _, cx| this.retry_clicked(cx))),
                    )
                    .child(text_button(
                        "close",
                        "Close",
                        cx.listener(|_, _, _, cx| {
                            log!(TAG, "closed after failure");
                            cx.quit();
                        }),
                    ))
            }))
            .child(div().flex_1())
    }
}

fn status_done_row(label: &String) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(0x6fbf73))
                .child("✓"),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(0x909090))
                .child(label.to_owned()),
        )
}

/// Settings-page style toggle row (mirrors the tray toggle in
/// onboarding_view.rs).
fn toggle_row(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .border_1()
        .border_color(rgb(0x2e2e2e))
        .px(px(12.))
        .py(px(8.))
        .gap(px(8.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(13.)).child(title))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x808080))
                        .child(description),
                ),
        )
        .child(
            toggle_chip(id, enabled)
                .on_click(move |event, window, app| on_click(event, window, app)),
        )
}

fn toggle_chip(id: &'static str, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .border_1()
        .border_color(rgb(0x505050))
        .px(px(10.))
        .py(px(4.))
        .bg(if enabled {
            rgb(0x2a2a2a)
        } else {
            rgb(0x171717)
        })
        .text_size(px(11.))
        .text_color(if enabled {
            rgb(0xf2f2f2)
        } else {
            rgb(0x707070)
        })
        .child(if enabled { "ON" } else { "OFF" })
}

fn footer(cancel: Stateful<Div>, primary: Stateful<Div>) -> Div {
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.))
        .child(cancel)
        .child(primary)
}

fn progress_bar(fraction: f32) -> Div {
    div()
        .w_full()
        .h(px(3.))
        .bg(rgb(0x17191d))
        .child(div().h(px(3.)).bg(rgb(0xd8d8d8)).w(relative(fraction)))
}

fn version_arrow(installed: &str, this: &str) -> String {
    if installed.is_empty() || installed == this {
        format!("v{this}")
    } else {
        format!("v{installed} → v{this}")
    }
}

fn progress_fraction(completed: usize, finished_ok: bool) -> f32 {
    if finished_ok {
        return 1.0;
    }
    (completed as f32 / ALL_STEPS.len() as f32).clamp(0.0, 1.0)
}

fn window_title_utf16() -> Vec<u16> {
    WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

// ---- Frameless draggable progress chrome (pill_window subclass approach) ----

static PROGRESS_PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);

/// Swap the choosing-screen chrome for the frameless progress chrome: no
/// caption/sysmenu, same client size re-centered on the primary display.
pub fn apply_progress_chrome(hwnd: HWND) {
    install_progress_wndproc(hwnd);
    let (style, ex) = pw::styles(hwnd);
    let style = pw::pill_style(style);
    let ex = ex & !pw::EX_CLEAR_MASK;
    pw::set_styles(hwnd, style, ex);
    let (frame_w, frame_h) =
        pw::frame_size_for_client(WINDOW_WIDTH as i32, WINDOW_HEIGHT as i32, style, ex);
    let (ax, ay, aw, ah) = pw::primary_work_area();
    pw::place(
        hwnd,
        ax + (aw - frame_w) / 2,
        ay + (ah - frame_h) / 2,
        frame_w,
        frame_h,
        true,
    );
}

fn install_progress_wndproc(hwnd: HWND) {
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        if current == progress_subclass_proc as *const () as isize {
            return;
        }
        PROGRESS_PREV_WNDPROC.store(current, Ordering::SeqCst);
        SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            progress_subclass_proc as *const () as isize,
        );
    }
}

unsafe extern "system" fn progress_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        WM_NCHITTEST => unsafe {
            if hit_in_drag_strip(hwnd, lparam) {
                HTCAPTION as isize
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        },
        _ => match PROGRESS_PREV_WNDPROC.load(Ordering::SeqCst) {
            0 => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            prev => unsafe {
                CallWindowProcW(
                    std::mem::transmute::<isize, WNDPROC>(prev),
                    hwnd,
                    msg,
                    wparam,
                    lparam,
                )
            },
        },
    }
}

fn hit_in_drag_strip(hwnd: HWND, lparam: isize) -> bool {
    let x = signed_loword(lparam);
    let y = signed_hiword(lparam);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }
    x >= rect.left && x < rect.right && y >= rect.top && y - rect.top <= DRAG_STRIP_PX
}

fn signed_loword(lparam: isize) -> i32 {
    ((lparam as usize) & 0xFFFF) as u16 as i16 as i32
}

fn signed_hiword(lparam: isize) -> i32 {
    (((lparam as usize) >> 16) & 0xFFFF) as u16 as i16 as i32
}

// ---- Background event pump (main.rs window.spawn idiom) ----

fn pump_worker_events(window: &mut Window, cx: &mut App, app: Entity<InstallerApp>) {
    window
        .spawn(cx, async move |cx| {
            loop {
                cx.update(|_, cx| {
                    app.update(cx, |installer, cx| {
                        while let Ok(event) = installer.events.try_recv() {
                            installer.handle_event(event, cx);
                        }
                    });
                })
                .ok();
                let should_quit = cx
                    .update(|_, cx| app.update(cx, |installer, _| installer.poll_quit()))
                    .ok()
                    .unwrap_or(false);
                if should_quit {
                    log!(TAG, "success linger elapsed; quitting");
                    cx.update(|_, cx| cx.quit()).ok();
                    break;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
}

impl Render for InstallerApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.stage {
            Stage::Choosing => self.render_choosing(cx),
            Stage::Progress => self.render_progress(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_arrow_shows_both_versions_when_different() {
        assert_eq!(version_arrow("1.0.0", "1.0.1"), "v1.0.0 \u{2192} v1.0.1");
    }

    #[test]
    fn version_arrow_collapses_equal_or_unknown_installed_version() {
        assert_eq!(version_arrow("1.0.1", "1.0.1"), "v1.0.1");
        assert_eq!(version_arrow("", "1.0.1"), "v1.0.1");
    }

    #[test]
    fn progress_fraction_tracks_completed_over_all_steps() {
        let total = ALL_STEPS.len();
        assert_eq!(progress_fraction(0, false), 0.0);
        assert_eq!(
            progress_fraction(total - 1, false),
            (total - 1) as f32 / total as f32
        );
        assert_eq!(progress_fraction(3, true), 1.0);
    }

    #[test]
    fn progress_fraction_clamps_out_of_range_counts() {
        assert_eq!(progress_fraction(99, false), 1.0);
    }

    #[test]
    fn signed_words_decode_screen_coordinates_with_negatives() {
        // Cursor at (-4, -9): multi-monitor setups left of / above origin.
        let x = -4_i32;
        let y = -9_i32;
        let lparam = ((y as u16 as isize) << 16) | (x as u16 as isize);
        assert_eq!(signed_loword(lparam), x);
        assert_eq!(signed_hiword(lparam), y);
    }
}
