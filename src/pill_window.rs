use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::WindowsAndMessaging::{
        FindWindowW, GetCursorPos, GetWindowRect, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, SystemParametersInfoW,
    },
};

pub const PILL_WIDTH: i32 = 340;
pub const PILL_HEIGHT: i32 = 64;
pub const PANEL_WIDTH: i32 = 800;
pub const PANEL_HEIGHT: i32 = 600;

pub fn find_by_title(title_utf16: &[u16]) -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title_utf16.as_ptr()) };
    (!hwnd.is_null()).then_some(hwnd)
}

pub fn set_bounds(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
}

pub fn move_to(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE,
        );
    }
}

pub fn show_no_activate(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn hide(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn primary_work_area() -> (i32, i32, i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            0,
        );
    }
    (
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point) };
    if ok != 0 {
        Some((point.x, point.y))
    } else {
        None
    }
}

pub fn window_rect(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
    if ok != 0 {
        Some((rect.left, rect.top))
    } else {
        None
    }
}
