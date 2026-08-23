use std::fmt;

use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU},
        WindowsAndMessaging::{
            GetForegroundWindow, GetWindowTextW, IsWindow, SetForegroundWindow,
        },
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
/// Uses the ALT-key trick to bypass the foreground lock (background processes are
/// normally denied SetForegroundWindow).
pub fn restore_focus(target: &FocusTarget) -> Result<(), String> {
    if target.hwnd == 0 {
        return Err("no captured window".into());
    }
    if unsafe { IsWindow(target.hwnd as HWND) } == 0 {
        return Err(format!("captured window gone ({target})"));
    }
    send_alt_tap();
    let ok = unsafe { SetForegroundWindow(target.hwnd as HWND) };
    if ok == 0 {
        return Err(format!("SetForegroundWindow rejected for {target}"));
    }
    crate::logging::log("focus", format!("restored {target}"));
    Ok(())
}

fn send_alt_tap() {
    let mut down = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: unsafe { std::mem::zeroed() },
    };
    down.Anonymous.ki = KEYBDINPUT { wVk: VK_MENU, ..unsafe { std::mem::zeroed() } };
    let mut up_input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: unsafe { std::mem::zeroed() },
    };
    up_input.Anonymous.ki = KEYBDINPUT { wVk: VK_MENU, dwFlags: KEYEVENTF_KEYUP, ..unsafe { std::mem::zeroed() } };
    unsafe { SendInput(2, [down, up_input].as_ptr(), size_of::<INPUT>() as i32) };
}
