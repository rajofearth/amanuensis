use std::fmt;

use windows_sys::Win32::{
    Foundation::HWND,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
        SetForegroundWindow,
    },
};

#[derive(Clone, Copy)]
pub struct FocusTarget {
    hwnd: isize,
    title: [u16; 128],
    title_len: usize,
}

impl fmt::Display for FocusTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = String::from_utf16_lossy(&self.title[..self.title_len]);
        write!(f, "hwnd={:#x} title={:?})", self.hwnd, text)
    }
}

pub fn capture_foreground() -> Option<FocusTarget> {
    let hwnd = unsafe { GetForegroundWindow() } as isize;
    if hwnd == 0 {
        return None;
    }
    let mut target = FocusTarget { hwnd, title: [0; 128], title_len: 0 };
    let written = unsafe { GetWindowTextW(hwnd as HWND, target.title.as_mut_ptr(), 128) };
    target.title_len = written.max(0) as usize;
    Some(target)
}

/// Bring the captured window back to the foreground so synthesized keys land there.
/// Uses AttachThreadInput (no synthetic keys — an injected ALT tap would latch
/// the target's menu bar and swallow the subsequent paste).
pub fn restore_focus(target: &FocusTarget) -> Result<(), String> {
    if target.hwnd == 0 {
        return Err("no captured window".into());
    }
    if unsafe { IsWindow(target.hwnd as HWND) } == 0 {
        return Err(format!("captured window gone ({target})"));
    }
    let foreground = unsafe { GetForegroundWindow() } as isize;
    let foreground_thread =
        unsafe { GetWindowThreadProcessId(foreground as HWND, std::ptr::null_mut()) };
    let current_thread = unsafe { GetCurrentThreadId() };
    let attached = foreground_thread != 0
        && foreground_thread != current_thread
        && unsafe { AttachThreadInput(current_thread, foreground_thread, 1) } != 0;
    unsafe { SetForegroundWindow(target.hwnd as HWND) };
    if attached {
        unsafe { AttachThreadInput(current_thread, foreground_thread, 0) };
    }
    crate::logging::log(
        "focus",
        format!("restored {target} (attached={attached}, was hwnd={foreground:#x})"),
    );
    Ok(())
}
