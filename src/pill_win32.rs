use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateFontIndirectW,
    CreateRoundRectRgn, CreateSolidBrush, DIB_RGB_COLORS, DeleteDC, DeleteObject, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, DrawTextW, FillRgn, FrameRgn, GetDC, GetDeviceCaps,
    InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BLENDFUNCTION, HBITMAP, HDC,
    HRGN, LOGFONTW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    GWLP_USERDATA, GetWindowLongPtrW, HTCAPTION, KillTimer, MSG, PostQuitMessage, RegisterClassW,
    SW_HIDE, SW_SHOWNOACTIVATE, SPI_GETWORKAREA, SendMessageW, SetTimer, SetWindowLongPtrW,
    ShowWindow, SystemParametersInfoW, TranslateMessage, UpdateLayeredWindow, WM_CREATE,
    WM_DESTROY, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::log;

const PILL_W: i32 = 340;
const PILL_H: i32 = 64;
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 33;
const BARS: usize = 26;
const INNER_PAD: i32 = 14;
const BAR_AREA_W: i32 = 120;
const BAR_H: i32 = 18;
const TEXT_W: i32 = 64;
const BTN_SIZE: i32 = 24;
const ROUND_RADIUS: i32 = 16;

const COLOR_BG: COLORREF = 0x00161616;
const COLOR_BORDER: COLORREF = 0x00333333;
const COLOR_BAR: COLORREF = 0x0033CC66;
const COLOR_BAR_FLASH: COLORREF = 0x00606060;
const COLOR_TEXT: COLORREF = 0x00CCCCCC;
const COLOR_DISCARD: COLORREF = 0x00CC3333;
const COLOR_FINISH: COLORREF = 0x0033CC66;
const COLOR_BTN_IDLE: COLORREF = 0x002A2A2A;
const COLOR_BTN_HOVER: COLORREF = 0x003A3A3A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillMode {
    Recording,
    Transcribing,
    Flash,
}

pub enum PillCommand {
    Show(PillMode),
    Hide,
    Levels(Vec<f32>),
    RecordingStarted(Instant),
    TranscribingStarted(Instant),
    FlashStarted,
}

pub enum PillButton {
    Discard,
    Finish,
    OpenSettings,
}

pub struct PillOverlay {
    cmd_tx: mpsc::Sender<PillCommand>,
    click_rx: mpsc::Receiver<PillButton>,
}

impl PillOverlay {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PillCommand>();
        let (click_tx, click_rx) = mpsc::channel::<PillButton>();
        thread::spawn(move || pill_thread(cmd_rx, click_tx));
        PillOverlay { cmd_tx, click_rx }
    }

    pub fn send(&self, cmd: PillCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn command_tx(&self) -> mpsc::Sender<PillCommand> {
        self.cmd_tx.clone()
    }

    pub fn try_recv_button(&self) -> Option<PillButton> {
        self.click_rx.try_recv().ok()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum HoverTarget {
    None,
    Discard,
    Finish,
    Body,
}

struct PillWindow {
    hwnd: HWND,
    hdc_mem: HDC,
    _hbmp: HBITMAP,
    pixels: *mut u8,
    width: i32,
    height: i32,
    dpi_scale: f64,
    mode: Option<PillMode>,
    levels: Vec<f32>,
    elapsed: f64,
    record_start: Option<Instant>,
    transcribe_start: Option<Instant>,
    flash_start: Option<Instant>,
    hover: HoverTarget,
    mouse_in: bool,
    cmd_rx: mpsc::Receiver<PillCommand>,
    click_tx: mpsc::Sender<PillButton>,
}

fn work_area() -> (i32, i32, i32, i32) {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            0,
        );
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }
}

fn dpi_scale(hwnd: HWND) -> f64 {
    unsafe {
        let hdc = GetDC(hwnd);
        let dpi = GetDeviceCaps(hdc, 88);
        ReleaseDC(hwnd, hdc);
        dpi as f64 / 96.0
    }
}

fn hit_test(x: i32, y: i32, w: i32, h: i32, scale: f64, any_hover: bool) -> HoverTarget {
    if !any_hover {
        return HoverTarget::None;
    }
    let bs = (BTN_SIZE as f64 * scale) as i32;
    let ip = (INNER_PAD as f64 * scale) as i32;
    let center_y = h / 2;
    let btn_top = center_y - bs / 2;

    let discard_x = ip;
    if x >= discard_x && x <= discard_x + bs && y >= btn_top && y <= btn_top + bs {
        return HoverTarget::Discard;
    }

    let finish_x = w - ip - bs;
    if x >= finish_x && x <= finish_x + bs && y >= btn_top && y <= btn_top + bs {
        return HoverTarget::Finish;
    }

    HoverTarget::Body
}

fn is_recording(pill: &PillWindow) -> bool {
    matches!(pill.mode, Some(PillMode::Recording))
}

/// Animated bar levels for modes that self-animate (no live mic data).
fn synthesized_levels(mode: PillMode, elapsed: f64) -> Vec<f32> {
    match mode {
        PillMode::Flash => vec![0.06; BARS],
        _ => (0..BARS)
            .map(|index| {
                let wave = (elapsed * 3.0 + index as f64 * 0.7).sin().abs();
                (0.2 + 0.5 * wave).clamp(0.08, 1.0) as f32
            })
            .collect(),
    }
}

fn drain_commands(pill: &mut PillWindow) {
    while let Ok(cmd) = pill.cmd_rx.try_recv() {
        match cmd {
            PillCommand::Show(mode) => {
                pill.mode = Some(mode);
                match mode {
                    PillMode::Transcribing => {
                        pill.transcribe_start.get_or_insert_with(Instant::now);
                    }
                    PillMode::Flash => {
                        pill.flash_start.get_or_insert_with(Instant::now);
                    }
                    PillMode::Recording => {}
                }
                show(pill.hwnd);
            }
            PillCommand::Hide => {
                pill.mode = None;
                hide(pill.hwnd);
            }
            PillCommand::Levels(levels) => {
                pill.levels = levels;
            }
            PillCommand::RecordingStarted(instant) => {
                pill.record_start = Some(instant);
            }
            PillCommand::TranscribingStarted(instant) => {
                pill.transcribe_start = Some(instant);
            }
            PillCommand::FlashStarted => {
                pill.mode = Some(PillMode::Flash);
                pill.flash_start = Some(Instant::now());
                show(pill.hwnd);
            }
        }
    }
}

fn show(hwnd: HWND) {
    unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
}

fn hide(hwnd: HWND) {
    unsafe { ShowWindow(hwnd, SW_HIDE) };
}

fn update_elapsed(pill: &mut PillWindow) {
    match pill.mode {
        Some(PillMode::Recording) => {
            if let Some(start) = pill.record_start {
                pill.elapsed = start.elapsed().as_secs_f64();
            }
        }
        Some(PillMode::Transcribing) => {
            if let Some(start) = pill.transcribe_start {
                pill.elapsed = start.elapsed().as_secs_f64();
            }
        }
        Some(PillMode::Flash) => {
            if let Some(start) = pill.flash_start {
                let elapsed = start.elapsed().as_secs_f64();
                if elapsed >= 0.8 {
                    pill.mode = None;
                    hide(pill.hwnd);
                    return;
                }
                pill.elapsed = elapsed;
            }
        }
        None => {}
    }
}

/// Per-pixel alpha pass: opaque inside the rounded capsule, transparent outside.
/// GDI writes leave the alpha channel garbage/zero, which AC_SRC_ALPHA would
/// render as a fully transparent window — this is what makes the pill visible.
fn apply_alpha(pixels: *mut u8, w: i32, h: i32, scale: f64) {
    if pixels.is_null() {
        return;
    }
    let stride = w as usize * 4;
    let radius = (ROUND_RADIUS as f64 * scale) as i32;
    for y in 0..h {
        let row = unsafe { pixels.add(y as usize * stride) };
        let dy = y.min(h - 1 - y);
        for x in 0..w {
            let dx = x.min(w - 1 - x);
            let inside = if dx >= radius || dy >= radius {
                true
            } else {
                let ddx = (radius - dx) as f64;
                let ddy = (radius - dy) as f64;
                ddx * ddx + ddy * ddy <= (radius * radius) as f64
            };
            unsafe {
                *row.add(x as usize * 4 + 3) = if inside { 0xFF } else { 0x00 };
            }
        }
    }
}

fn draw_button_glyph(hdc: HDC, x: i32, top: i32, size: i32, glyph: &[u16], color: COLORREF) {
    unsafe {
        SetTextColor(hdc, color);
        let mut rect = RECT {
            left: x,
            top,
            right: x + size,
            bottom: top + size,
        };
        DrawTextW(
            hdc,
            glyph.as_ptr(),
            (glyph.len() - 1) as i32,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn render_pill(pill: &PillWindow) {
    if pill.mode.is_none() || pill.hdc_mem.is_null() || pill.pixels.is_null() {
        return;
    }
    unsafe {
        let w = pill.width;
        let h = pill.height;
        let scale = pill.dpi_scale;
        let hdc = pill.hdc_mem;

        let bg_brush = CreateSolidBrush(COLOR_BG);
        let bg_rect = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        windows_sys::Win32::Graphics::Gdi::FillRect(hdc, &bg_rect, bg_brush);
        DeleteObject(bg_brush as _);

        let ip = (INNER_PAD as f64 * scale) as i32;
        let bs = (BTN_SIZE as f64 * scale) as i32;
        let center_y = h / 2;

        // Border follows the rounded outline.
        let border_brush = CreateSolidBrush(COLOR_BORDER);
        let outline: HRGN =
            CreateRoundRectRgn(0, 0, w + 1, h + 1, ROUND_RADIUS * 2, ROUND_RADIUS * 2);
        FrameRgn(hdc, outline, border_brush, 1, 1);
        DeleteObject(border_brush as _);
        DeleteObject(outline as _);

        SetBkMode(hdc, 1);

        let segoe: Vec<u16> = "Segoe UI".encode_utf16().chain(std::iter::once(0)).collect();
        let font_size = (-12.0 * scale * 20.0 / 96.0) as i32;
        let font = CreateFontIndirectW(&LOGFONTW {
            lfHeight: font_size,
            lfWeight: 400,
            lfFaceName: {
                let mut name = [0u16; 32];
                let copy_len = segoe.len().min(31);
                name[..copy_len].copy_from_slice(&segoe[..copy_len]);
                name
            },
            ..std::mem::zeroed()
        });
        let old_font = SelectObject(hdc, font as _);

        // Buttons appear once the cursor is over the pill (recording) or hover.
        let expanded = pill.hover != HoverTarget::None || is_recording(pill);
        if expanded && pill.hover != HoverTarget::None {
            let discard_x = ip;
            let finish_x = w - ip - bs;
            let btn_top = center_y - bs / 2;

            let btn_color = match pill.hover {
                HoverTarget::Discard | HoverTarget::Finish => COLOR_BTN_HOVER,
                _ => COLOR_BTN_IDLE,
            };

            for bx in [discard_x, finish_x] {
                let btn_brush = CreateSolidBrush(btn_color);
                let btn_rgn =
                    CreateRoundRectRgn(bx, btn_top, bx + bs, btn_top + bs, 6 * scale as i32 + 1, 6 * scale as i32 + 1);
                FillRgn(hdc, btn_rgn, btn_brush);
                DeleteObject(btn_brush as _);
                DeleteObject(btn_rgn as _);
            }

            let cross: Vec<u16> = "\u{2715}".encode_utf16().chain(std::iter::once(0)).collect();
            draw_button_glyph(hdc, discard_x, btn_top, bs, &cross, COLOR_DISCARD);
            let check: Vec<u16> = "\u{2713}".encode_utf16().chain(std::iter::once(0)).collect();
            draw_button_glyph(hdc, finish_x, btn_top, bs, &check, COLOR_FINISH);
        }

        // Bars: live mic levels while recording, self-animated otherwise.
        let synth;
        let levels: &[f32] = match pill.mode {
            Some(PillMode::Recording) | None => &pill.levels,
            Some(PillMode::Transcribing) => {
                synth = synthesized_levels(PillMode::Transcribing, pill.elapsed);
                &synth
            }
            Some(PillMode::Flash) => {
                synth = synthesized_levels(PillMode::Flash, pill.elapsed);
                &synth
            }
        };

        let shift_left = expanded && pill.hover != HoverTarget::None;
        let bar_area_x = ip + if shift_left { bs + (4.0 * scale) as i32 } else { 0 };
        let bar_w = (((BAR_AREA_W as f64 * scale) as i32) - (BARS as i32 - 1) * 2) / BARS as i32;
        let bar_gap = 2;
        let bar_max_h = (BAR_H as f64 * scale) as i32;
        let bar_top = center_y - bar_max_h / 2;

        let bar_color: COLORREF = match pill.mode {
            Some(PillMode::Flash) => COLOR_BAR_FLASH,
            _ => COLOR_BAR,
        };
        for (index, level) in levels.iter().take(BARS).enumerate() {
            let level = level.clamp(0.0, 1.0);
            let bar_h = ((bar_max_h as f32 * level) as i32).max(2);
            let bar_x = bar_area_x + index as i32 * (bar_w + bar_gap);
            let bar_y = bar_top + bar_max_h - bar_h;
            let bar_brush = CreateSolidBrush(bar_color);
            let bar_rgn = CreateRoundRectRgn(bar_x, bar_y, bar_x + bar_w, bar_y + bar_h, 2, 2);
            FillRgn(hdc, bar_rgn, bar_brush);
            DeleteObject(bar_brush as _);
            DeleteObject(bar_rgn as _);
        }

        // Status text pinned to the right edge.
        SetTextColor(hdc, COLOR_TEXT);
        let text_area_w = (TEXT_W as f64 * scale) as i32;
        let text_x = w - ip - text_area_w;
        let duration_str = match pill.mode {
            Some(PillMode::Recording) => {
                let total_secs = pill.elapsed as u64;
                format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
            }
            Some(PillMode::Transcribing) => "transcribing\u{2026}".to_owned(),
            Some(PillMode::Flash) => "done \u{2713}".to_owned(),
            None => String::new(),
        };
        let utf16: Vec<u16> = duration_str.encode_utf16().chain(std::iter::once(0)).collect();
        let mut text_rect = RECT {
            left: text_x,
            top: 0,
            right: text_x + text_area_w,
            bottom: h,
        };
        DrawTextW(
            hdc,
            utf16.as_ptr(),
            utf16.len() as i32 - 1,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        SelectObject(hdc, old_font);
        DeleteObject(font as _);

        apply_alpha(pill.pixels, w, h, scale);

        let src = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        let blend = BLENDFUNCTION {
            AlphaFormat: AC_SRC_ALPHA as u8,
            BlendFlags: 0,
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: 255,
        };
        let hdc_screen = GetDC(pill.hwnd);
        UpdateLayeredWindow(
            pill.hwnd,
            hdc_screen,
            std::ptr::null(),
            &size,
            hdc,
            &src,
            0,
            &blend,
            0x00000002, // ULW_ALPHA
        );
        ReleaseDC(pill.hwnd, hdc_screen);
    }
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let rect: RECT = std::mem::zeroed();
        InvalidateRect(hwnd, &rect, 0);
    }
}

unsafe extern "system" fn pill_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER_ID);
                PostQuitMessage(0);
                0
            }
            WM_TIMER => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                if ptr.is_null() {
                    return 0;
                }
                let pill = &mut *ptr;
                drain_commands(pill);
                update_elapsed(pill);
                if pill.mode.is_some() {
                    render_pill(pill);
                }
                0
            }
            WM_MOUSEMOVE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                if ptr.is_null() {
                    return 0;
                }
                let pill = &mut *ptr;
                if !pill.mouse_in {
                    let mut tme: TRACKMOUSEEVENT = std::mem::zeroed();
                    tme.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
                    tme.dwFlags = TME_LEAVE;
                    tme.hwndTrack = hwnd;
                    TrackMouseEvent(&mut tme);
                    pill.mouse_in = true;
                }
                let x = (lparam & 0xFFFF) as i16 as i32;
                let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
                let has_buttons = is_recording(pill);
                let new_hover = hit_test(x, y, pill.width, pill.height, pill.dpi_scale, has_buttons);
                let changed = new_hover != pill.hover;
                pill.hover = new_hover;
                if changed {
                    invalidate(hwnd);
                }
                0
            }
            WM_MOUSELEAVE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                if !ptr.is_null() {
                    let pill = &mut *ptr;
                    pill.mouse_in = false;
                    pill.hover = HoverTarget::None;
                    invalidate(hwnd);
                }
                0
            }
            WM_LBUTTONDOWN => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                if ptr.is_null() {
                    return 0;
                }
                let pill = &*ptr;
                let x = (lparam & 0xFFFF) as i16 as i32;
                let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
                let target = hit_test(
                    x,
                    y,
                    pill.width,
                    pill.height,
                    pill.dpi_scale,
                    is_recording(pill),
                );
                match target {
                    HoverTarget::Discard => {
                        let _ = (*ptr).click_tx.send(PillButton::Discard);
                    }
                    HoverTarget::Finish => {
                        let _ = (*ptr).click_tx.send(PillButton::Finish);
                    }
                    HoverTarget::Body => {
                        ReleaseCapture();
                        SendMessageW(hwnd, 0x00A1 /* WM_NCLBUTTONDOWN */, HTCAPTION as WPARAM, 0);
                    }
                    HoverTarget::None => {}
                }
                0
            }
            WM_RBUTTONUP => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                if !ptr.is_null() {
                    let _ = (*ptr).click_tx.send(PillButton::OpenSettings);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn create_backing(hwnd: HWND, w: i32, h: i32) -> (HDC, HBITMAP, *mut u8) {
    unsafe {
        let hdc_screen = GetDC(hwnd);
        let hdc_mem = CreateCompatibleDC(hdc_screen);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut pixels: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = windows_sys::Win32::Graphics::Gdi::CreateDIBSection(
            hdc_screen,
            &bmi,
            DIB_RGB_COLORS,
            &mut pixels,
            std::ptr::null_mut(),
            0,
        );
        ReleaseDC(hwnd, hdc_screen);
        if hbmp.is_null() || pixels.is_null() {
            return (hdc_mem, std::ptr::null_mut(), std::ptr::null_mut());
        }
        SelectObject(hdc_mem, hbmp as _);
        (hdc_mem, hbmp, pixels as *mut u8)
    }
}

fn pill_thread(cmd_rx: mpsc::Receiver<PillCommand>, click_tx: mpsc::Sender<PillButton>) {
    let hwnd = unsafe {
        let class_name: Vec<u16> = "RaycastDictationPill"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(pill_wnd_proc),
            hInstance: GetModuleHandleW(std::ptr::null()),
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wnd_class);

        let ex_style: u32 = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let style: u32 = WS_POPUP;

        let pre_dpi_hdc = GetDC(std::ptr::null_mut());
        let pre_dpi = GetDeviceCaps(pre_dpi_hdc, 88);
        ReleaseDC(std::ptr::null_mut(), pre_dpi_hdc);
        let scale = pre_dpi as f64 / 96.0;
        let w = (PILL_W as f64 * scale) as i32;
        let h = (PILL_H as f64 * scale) as i32;

        // Bottom-center of the work area, docked above the taskbar.
        let (sx, sy, work_w, work_h) = work_area();
        let x = sx + (work_w - w) / 2;
        let y = sy + work_h - h - (12.0 * scale) as i32;

        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            std::ptr::null(),
            style,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };

    if hwnd.is_null() {
        log!("pill", "ERROR: CreateWindowExW failed");
        return;
    }

    let scale = dpi_scale(hwnd);
    let w = (PILL_W as f64 * scale) as i32;
    let h = (PILL_H as f64 * scale) as i32;
    let (hdc_mem, hbmp, pixels) = create_backing(hwnd, w, h);

    let pill = PillWindow {
        hwnd,
        hdc_mem,
        _hbmp: hbmp,
        pixels,
        width: w,
        height: h,
        dpi_scale: scale,
        mode: None,
        levels: vec![0.0; BARS],
        elapsed: 0.0,
        record_start: None,
        transcribe_start: None,
        flash_start: None,
        hover: HoverTarget::None,
        mouse_in: false,
        cmd_rx,
        click_tx,
    };

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(pill)) as isize);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
        DeleteDC(hdc_mem);
        DeleteObject(hbmp as _);
    }
}
