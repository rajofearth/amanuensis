use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    System::Threading::GetCurrentProcessId,
    UI::{
        HiDpi::GetDpiForWindow,
        WindowsAndMessaging::{
            AdjustWindowRectEx, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetCursorPos, GetWindowLongW,
            GetWindowRect, GetWindowThreadProcessId, PostMessageW, SPI_GETWORKAREA, SW_HIDE,
            SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_NOZORDER, SetWindowLongW, SetWindowPos, SetWindowTextW, ShowWindow,
            SystemParametersInfoW, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
        },
    },
};

pub use windows_sys::Win32::Foundation::HWND;

pub const PILL_WIDTH: i32 = 340;
pub const PILL_HEIGHT: i32 = 64;
pub const PANEL_WIDTH: i32 = 800;
pub const PANEL_HEIGHT: i32 = 600;

pub const CHROME_CLEAR_MASK: u32 =
    WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_THICKFRAME;
pub const EX_CLEAR_MASK: u32 = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
pub const EX_PILL: u32 = WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;

pub fn dpi(hwnd: HWND) -> u32 {
    unsafe { GetDpiForWindow(hwnd) }
}

pub fn window_rect_full(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
    if ok != 0 {
        Some((rect.left, rect.top, rect.right, rect.bottom))
    } else {
        None
    }
}

pub fn pill_style(style: u32) -> u32 {
    (style & !CHROME_CLEAR_MASK) | WS_POPUP
}

pub fn panel_style(style: u32) -> u32 {
    (style & !(CHROME_CLEAR_MASK | WS_POPUP)) | (WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX)
}

pub fn find_by_title(title_utf16: &[u16]) -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title_utf16.as_ptr()) };
    (!hwnd.is_null()).then_some(hwnd)
}

pub fn process_owns_window(hwnd: HWND) -> bool {
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    pid == unsafe { GetCurrentProcessId() }
}

pub fn styles(hwnd: HWND) -> (u32, u32) {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE);
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        (style as u32, ex_style as u32)
    }
}

pub fn set_styles(hwnd: HWND, style: u32, ex_style: u32) {
    unsafe {
        SetWindowLongW(hwnd, GWL_STYLE, style as i32);
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style as i32);
    }
}

pub fn frame_size_for_client(
    client_width: i32,
    client_height: i32,
    style: u32,
    ex_style: u32,
) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    unsafe {
        AdjustWindowRectEx(&mut rect, style, 0, ex_style);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

pub fn set_text(hwnd: HWND, title_utf16: &[u16]) {
    unsafe {
        SetWindowTextW(hwnd, title_utf16.as_ptr());
    }
}

pub fn place(hwnd: HWND, x: i32, y: i32, width: i32, height: i32, frame_changed: bool) {
    let mut flags = SWP_NOACTIVATE | SWP_NOZORDER;
    if frame_changed {
        flags |= SWP_FRAMECHANGED;
    }
    unsafe {
        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, width, height, flags);
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
        use windows_sys::Win32::Graphics::Gdi::{
            RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
        };
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_FRAME | RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        PostMessageW(hwnd, WM_GPUI_FORCE_UPDATE_WINDOW, 0, 0);
    }
}

const WM_GPUI_FORCE_UPDATE_WINDOW: u32 = 0x0400 + 5;

pub fn hide(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn set_click_through(hwnd: HWND, enabled: bool) {
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let updated = if enabled {
            ex | WS_EX_TRANSPARENT
        } else {
            ex & !WS_EX_TRANSPARENT
        };
        if updated != ex {
            SetWindowLongW(hwnd, GWL_EXSTYLE, updated as i32);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
        PostMessageW(hwnd, WM_GPUI_FORCE_UPDATE_WINDOW, 0, 0);
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
