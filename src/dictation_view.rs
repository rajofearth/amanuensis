use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::messages::{HotkeyMessage, PasteResult};
use amanuensis::asr::{Command, Event, Mode};
use amanuensis::audio::{self, Sound};
use amanuensis::esc_hook::EscHook;
use amanuensis::log;
use amanuensis::paste::{MIN_AUDIO_SECS, clean_transcript, paste_text};
use amanuensis::pill_win32::{PillCommand, PillMode};
use amanuensis::win_focus::{self, FocusTarget};
use gpui::{Context, IntoElement, Render, Window, div, prelude::*};

pub(crate) const BARS: usize = 26;
const FLASH_SECS: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Loading,
    Idle,
    Recording,
    Transcribing,
    Flash,
}

pub(crate) struct Waveform {
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

pub(crate) struct Dictation {
    pub(crate) phase: Phase,
    mode: Mode,
    waveform: Waveform,
    pub(crate) commands: mpsc::Sender<Command>,
    results: mpsc::Sender<PasteResult>,
    pill_cmd: mpsc::Sender<PillCommand>,
    esc: EscHook,
    pending_start: bool,
    transcribing_since: Option<Instant>,
    focus: Option<FocusTarget>,
    recording_started: Option<Instant>,
    discard_requested: bool,
    flash_since: Option<Instant>,
    pub(crate) start_sent: bool,
    pub(crate) epoch: u64,
}

impl Dictation {
    pub(crate) fn new(
        commands: mpsc::Sender<Command>,
        results: mpsc::Sender<PasteResult>,
        pill_cmd: mpsc::Sender<PillCommand>,
        esc: EscHook,
        pending_start: bool,
    ) -> Self {
        Self {
            phase: Phase::Loading,
            mode: Mode::Record,
            waveform: Waveform {
                bars: [0.0; BARS],
                peak: 0.0,
            },
            commands,
            results,
            pill_cmd,
            esc,
            pending_start,
            transcribing_since: None,
            focus: None,
            recording_started: None,
            discard_requested: false,
            flash_since: None,
            start_sent: false,
            epoch: 0,
        }
    }

    fn begin_recording(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.focus = win_focus::capture_foreground();
        match &self.focus {
            Some(target) => log!("app", "recording starts, target window: {target}"),
            None => log!("app", "recording starts, no foreground window captured"),
        }
        self.phase = Phase::Recording;
        self.transcribing_since = None;
        self.flash_since = None;
        self.recording_started = Some(Instant::now());
        // Escape now cancels from anywhere; released on every exit path in
        // stop_recording, the sole funnel out of Phase::Recording.
        self.esc.set_active(true);
        self.start_sent = false;
        audio::play(Sound::Start);
        // Command::Start is withheld until the first captured chunk is
        // forwarded (see AppRoot::pump_audio) so the ASR session never
        // begins before any real audio exists.
        let _ = self.pill_cmd.send(PillCommand::Show(PillMode::Recording));
        let _ = self
            .pill_cmd
            .send(PillCommand::RecordingStarted(Instant::now()));
    }

    /// Sole exit path from Phase::Recording (F9 toggle, pill ✓ finish, and
    /// discard all land here), so releasing the Escape hook here covers them
    /// all. Stray discards after this point are no-ops: discard_clicked
    /// early-returns outside Recording.
    fn stop_recording(&mut self) {
        self.esc.set_active(false);
        self.phase = Phase::Transcribing;
        self.transcribing_since = Some(Instant::now());
        self.recording_started = None;
        let _ = self
            .pill_cmd
            .send(PillCommand::Show(PillMode::Transcribing));
        let _ = self
            .pill_cmd
            .send(PillCommand::TranscribingStarted(Instant::now()));
        let _ = self.commands.send(Command::Stop);
    }

    pub(crate) fn toggle_recording(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn discard_clicked(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Transcribing {
            // A Stop is already queued on the serial worker; just mark the
            // pending commit for drop instead of queueing a second Stop.
            log!("app", "recording discarded while transcribing");
            self.discard_requested = true;
            audio::play(Sound::Cancel);
            cx.notify();
            return;
        }
        if self.phase != Phase::Recording {
            return;
        }
        log!("app", "recording discarded by user");
        self.discard_requested = true;
        audio::play(Sound::Cancel);
        self.stop_recording();
        cx.notify();
    }

    pub(crate) fn done_clicked(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Recording {
            return;
        }
        log!("app", "recording finished by user");
        self.stop_recording();
        cx.notify();
    }

    fn enter_flash(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Flash;
        self.flash_since = Some(Instant::now());
        let _ = self.pill_cmd.send(PillCommand::FlashStarted);
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
            let _ = self.pill_cmd.send(PillCommand::Hide);
        }
        cx.notify();
    }

    pub(crate) fn handle_asr_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::LoadingProgress(message) => log!("asr", "{message}"),
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
                    log!("asr", "partial ({} chars)", text.chars().count());
                }
            }
            Event::Committed {
                text,
                duration_secs,
                epoch,
                rewritten,
            } => {
                if epoch != self.epoch {
                    log!("app", "stale commit dropped (epoch mismatch)");
                    return;
                }
                if self.phase == Phase::Transcribing {
                    self.transcribing_since = None;
                    if self.discard_requested {
                        log!(
                            "app",
                            "commit discarded by user ({} chars)",
                            text.chars().count()
                        );
                        self.discard_requested = false;
                        self.pending_start = false;
                        self.enter_flash(cx);
                        return;
                    }
                    self.enter_flash(cx);
                    log!(
                        "app",
                        "committed {:.1}s audio, raw ({} chars)",
                        duration_secs,
                        text.chars().count()
                    );
                    // s1-mini output is already styled: skip clean_transcript
                    // entirely (trim only) so curly quotes, em dashes, and
                    // truecasing survive. Raw ASR keeps full cleanup.
                    let cleaned = if rewritten {
                        text.trim().to_owned()
                    } else {
                        clean_transcript(&text)
                    };
                    if cleaned != text.trim() {
                        log!(
                            "app",
                            "cleanup: raw -> cleaned ({} chars)",
                            cleaned.chars().count()
                        );
                    }
                    if cleaned.is_empty() {
                        log!("app", "gate: dropped (empty after cleanup)");
                    } else if duration_secs < MIN_AUDIO_SECS {
                        log!(
                            "app",
                            "gate: dropped ({duration_secs:.2}s < {MIN_AUDIO_SECS}s min)"
                        );
                    } else {
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
                    if self.pending_start {
                        self.pending_start = false;
                        log!(
                            "app",
                            "queued start during transcribing: starting next recording"
                        );
                        self.begin_recording();
                    }
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn handle_hotkey(&mut self, message: HotkeyMessage, cx: &mut Context<Self>) {
        match message {
            HotkeyMessage::ToggleRecording => self.toggle_recording(cx),
            HotkeyMessage::DebugReload => {
                if self.phase == Phase::Idle {
                    let _ = self.commands.send(Command::ReloadForDebug);
                }
            }
        }
    }

    pub(crate) fn handle_paste_result(&mut self, result: PasteResult, cx: &mut Context<Self>) {
        match result {
            PasteResult::Pasted(chars) => {
                audio::play(Sound::Success);
                log!("app", "paste ok: {chars} chars");
            }
            PasteResult::Failed(error) => {
                audio::play(Sound::Failure);
                log!("app", "paste FAILED: {error}");
            }
        }
        cx.notify();
    }

    pub(crate) fn push_levels(&mut self, levels: Vec<f32>) {
        // Let the freshly-opened mic settle: cold-open transients (device
        // pop, the start blip bleeding back in) would pin the normalizer
        // peak and shrink every bar for seconds. Display-only; ASR chunks
        // are untouched.
        if self
            .recording_started
            .is_some_and(|since| since.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        self.waveform.extend(levels);
        let normalized: Vec<f32> = self.waveform.levels().collect();
        let _ = self.pill_cmd.send(PillCommand::Levels(normalized));
    }

    pub(crate) fn cancel_pending_start(&mut self, cx: &mut Context<Self>) {
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
        div().size_full()
    }
}
