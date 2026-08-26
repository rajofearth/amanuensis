//! Scoped system-wide Escape hook, active only while a recording is running.
//!
//! The GPUI window is click-through and never has keyboard focus during pill
//! mode, so Escape pressed in another app never reaches `on_key_down`. While
//! recording we install a `WH_KEYBOARD_LL` hook that swallows Escape and feeds
//! the discard path; outside recording the hook is uninstalled entirely, so
//! Escape behaves normally everywhere.
//!
//! Threading model: one persistent pump thread owns the hook (low-level hooks
//! deliver events only to the installing thread's message loop). Install and
//! uninstall arrive as posted thread messages; the hook procedure itself stays
//! allocation-free — its fast path is one atomic load plus a virtual-key
//! compare, because it runs for every keystroke system-wide while installed.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

use crate::log;

const CMD_INSTALL: u32 = WM_APP + 1;
const CMD_UNINSTALL: u32 = WM_APP + 2;

/// Whether the hook may intercept Escape right now. Cleared before the
/// uninstall command is posted so interception stops instantly.
static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Transition flag: true between the swallowed press and its release, used to
/// ignore auto-repeat instead of firing repeated discards.
static ESCAPE_DOWN: AtomicBool = AtomicBool::new(false);
/// Hook handle; owned and touched only by the pump thread.
static INSTALLED_HOOK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Pump thread id, published before the hook is ever installed.
static PUMP_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// Escape presses flow here while recording; polled by the app's UI loop.
static DISCARD_TX: Mutex<Option<Sender<()>>> = Mutex::new(None);

/// Control half of the Escape hook. Clone it wherever recording lifecycle
/// code lives; dropping the last clone deactivates the hook (best effort).
#[derive(Clone)]
pub struct EscHook {
    _keepalive: Arc<KeepAlive>,
}

struct KeepAlive;

impl KeepAlive {
    fn deactivate(&self) {
        set_hook_active(false);
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.deactivate();
    }
}

impl EscHook {
    pub fn set_active(&self, active: bool) {
        set_hook_active(active);
    }
}

/// Receive half: drained each UI tick by the owner of the app's message loop.
pub struct EscEvents {
    rx: Receiver<()>,
}

impl EscEvents {
    /// Returns true once per pending "Escape pressed while recording" signal.
    pub fn try_recv_escape(&self) -> bool {
        self.rx.try_recv().is_ok()
    }
}

/// Spawns the hook pump thread. Call once for the lifetime of the process.
pub fn spawn() -> (EscHook, EscEvents) {
    let (tx, rx) = mpsc::channel::<()>();
    if let Ok(mut slot) = DISCARD_TX.lock() {
        *slot = Some(tx);
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_ok() {
        let builder = thread::Builder::new().name("esc-hook".into());
        if let Err(error) = builder.spawn(pump_thread) {
            log!("esc", "ERROR: failed to spawn hook pump thread: {error}");
        }
    }
    (
        EscHook {
            _keepalive: Arc::new(KeepAlive),
        },
        EscEvents { rx },
    )
}

fn set_hook_active(active: bool) {
    // Order matters: flip the gate first so the proc stops intercepting even
    // before the uninstall command lands on the pump thread.
    HOOK_ACTIVE.store(active, Ordering::Release);
    let target = PUMP_THREAD_ID.load(Ordering::Acquire);
    if target == 0 {
        // Pump thread has not published its id yet; it re-syncs from
        // HOOK_ACTIVE during startup, so losing this command is harmless.
        return;
    }
    let command = if active { CMD_INSTALL } else { CMD_UNINSTALL };
    let posted = unsafe { PostThreadMessageW(target, command, 0, 0) };
    if posted == 0 && active {
        log!("esc", "install command could not be posted to pump thread");
    }
}

fn pump_thread() {
    unsafe {
        PUMP_THREAD_ID.store(GetCurrentThreadId(), Ordering::Release);
        // Catch an activate() that raced ahead of the id publication above.
        if HOOK_ACTIVE.load(Ordering::Acquire) {
            install_hook();
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            match msg.message {
                CMD_INSTALL => install_hook(),
                CMD_UNINSTALL => uninstall_hook(),
                _ => {}
            }
        }
        uninstall_hook();
        PUMP_THREAD_ID.store(0, Ordering::Release);
    }
}

fn install_hook() {
    if !INSTALLED_HOOK.load(Ordering::Acquire).is_null() {
        return;
    }
    // hMod is null and the thread id is 0: the low-level hook lives in-process.
    let hook = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(escape_hook_proc), std::ptr::null_mut(), 0)
    };
    if hook.is_null() {
        log!("esc", "ERROR: SetWindowsHookExW failed");
        return;
    }
    INSTALLED_HOOK.store(hook, Ordering::Release);
    log!("esc", "Escape hook installed (recording)");
}

fn uninstall_hook() {
    let hook = INSTALLED_HOOK.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if hook.is_null() {
        return;
    }
    unsafe { UnhookWindowsHookEx(hook) };
    log!("esc", "Escape hook removed");
}

/// Pure classification of one low-level keyboard event.
fn escape_action(vk_code: u32, msg: u32, active: bool, already_down: bool) -> EscapeAction {
    if !active || vk_code != VK_ESCAPE as u32 {
        return EscapeAction::PassThrough;
    }
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if already_down {
                EscapeAction::Swallow
            } else {
                EscapeAction::TriggerDiscard
            }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if already_down {
                // We ate the press, so eat the matching release too.
                EscapeAction::Swallow
            } else {
                EscapeAction::PassThrough
            }
        }
        _ => EscapeAction::PassThrough,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeAction {
    PassThrough,
    Swallow,
    TriggerDiscard,
}

unsafe extern "system" fn escape_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Fast path: runs for EVERY keystroke system-wide while recording, so no
    // locks, allocation, or logging here.
    if code < 0 || !HOOK_ACTIVE.load(Ordering::Acquire) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let already_down = ESCAPE_DOWN.load(Ordering::Relaxed);
    let action = escape_action(info.vkCode, wparam as u32, true, already_down);
    match action {
        EscapeAction::PassThrough => unsafe {
            CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
        },
        EscapeAction::Swallow | EscapeAction::TriggerDiscard => {
            let is_down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            ESCAPE_DOWN.store(is_down, Ordering::Relaxed);
            if action == EscapeAction::TriggerDiscard {
                notify_discard();
            }
            // Swallow: neither the focused app nor later hooks see this event.
            1
        }
    }
}

fn notify_discard() {
    if let Ok(slot) = DISCARD_TX.lock() {
        if let Some(sender) = slot.as_ref() {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: u32 = VK_ESCAPE as u32;
    const KEY_A: u32 = 0x41;

    #[test]
    fn passes_everything_through_when_inactive() {
        assert_eq!(
            escape_action(ESC, WM_KEYDOWN, false, false),
            EscapeAction::PassThrough
        );
        assert_eq!(
            escape_action(ESC, WM_KEYUP, false, true),
            EscapeAction::PassThrough
        );
        assert_eq!(
            escape_action(KEY_A, WM_KEYDOWN, false, false),
            EscapeAction::PassThrough
        );
    }

    #[test]
    fn triggers_discard_on_fresh_press_only() {
        assert_eq!(
            escape_action(ESC, WM_KEYDOWN, true, false),
            EscapeAction::TriggerDiscard
        );
        assert_eq!(
            escape_action(ESC, WM_SYSKEYDOWN, true, false),
            EscapeAction::TriggerDiscard
        );
        // Auto-repeat of a press we already swallowed: swallow, don't re-fire.
        assert_eq!(
            escape_action(ESC, WM_KEYDOWN, true, true),
            EscapeAction::Swallow
        );
    }

    #[test]
    fn swallows_release_of_a_swallowed_press_only() {
        assert_eq!(
            escape_action(ESC, WM_KEYUP, true, true),
            EscapeAction::Swallow
        );
        assert_eq!(
            escape_action(ESC, WM_SYSKEYUP, true, true),
            EscapeAction::Swallow
        );
        // Release with no prior swallowed press passes through untouched.
        assert_eq!(
            escape_action(ESC, WM_KEYUP, true, false),
            EscapeAction::PassThrough
        );
    }

    #[test]
    fn other_keys_never_intercept_even_when_active() {
        assert_eq!(
            escape_action(KEY_A, WM_KEYDOWN, true, false),
            EscapeAction::PassThrough
        );
        assert_eq!(
            escape_action(KEY_A, WM_KEYUP, true, false),
            EscapeAction::PassThrough
        );
    }

    #[test]
    fn unknown_messages_pass_through() {
        assert_eq!(
            escape_action(ESC, WM_APP, true, false),
            EscapeAction::PassThrough
        );
    }
}
