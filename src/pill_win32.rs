use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateFontIndirectW, CreateRoundRectRgn, CreateSolidBrush, DIB_RGB_COLORS, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FillRgn, FrameRgn, GetDC,
    GetDeviceCaps, HBITMAP, HDC, HRGN, InvalidateRect, LOGFONTW, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_DROPSHADOW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HTCAPTION, IDC_HAND, KillTimer, LoadCursorW,
    MSG, PostQuitMessage, RegisterClassW, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
    SendMessageW, SetCursor, SetTimer, SetWindowLongPtrW, ShowWindow, SystemParametersInfoW,
    TranslateMessage, UpdateLayeredWindow, WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::log;

// ---- Geometry (logical px, scaled by DPI at creation) ----
// One compact surface: controls overlay the waveform on hover.
const PILL_W: i32 = 120;
const PILL_H: i32 = 36;
const WAVE_H: i32 = 36;
const CONTROLS_X: i32 = 0;
const CONTROLS_Y: i32 = 0;
const CONTROLS_W: i32 = 120;
const CONTROLS_H: i32 = 36;
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 33;

const BARS: usize = 26; // level count arriving from the Waveform ring
const WAVE_BARS: usize = 31; // thin bars actually drawn (resampled from BARS)
const BAR_W: i32 = 2;
const BAR_MAX_H: i32 = 20;
const MIN_BAR_H: i32 = 1;
const INNER_PAD: i32 = 6;
const BTN_SIZE: i32 = 22;
const BTN_GAP: i32 = 4;
const PILL_RADIUS: i32 = 0;

// ---- Colors (COLORREF = 0x00BBGGRR) — monochrome, like the reference ----
const COLOR_BG: COLORREF = 0x00101010; // #101010
const COLOR_BORDER: COLORREF = 0x002E2E2E; // #2E2E2E
const COLOR_WAVE: COLORREF = 0x00D8D8D8; // bright Raycast-style waveform
const COLOR_WAVE_HOVER: COLORREF = 0x005A5A5A; // quieter while controls are overlaid
const COLOR_TEXT: COLORREF = 0x00F2F2F2; // #F2F2F2
const COLOR_BTN_IDLE: COLORREF = 0x001A1A1A; // #1A1A1A
const COLOR_BTN_HOVER: COLORREF = 0x00262626; // #262626

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

impl PillWindow {
    fn controls_rect(&self) -> RECT {
        RECT {
            left: self.s(CONTROLS_X),
            top: self.s(CONTROLS_Y),
            right: self.s(CONTROLS_X + CONTROLS_W),
            bottom: self.s(CONTROLS_Y + CONTROLS_H),
        }
    }

    fn discard_rect(&self) -> RECT {
        let bs = self.s(BTN_SIZE);
        let controls = self.controls_rect();
        RECT {
            left: controls.left + self.s(INNER_PAD),
            top: controls.top + (controls.bottom - controls.top - bs) / 2,
            right: controls.left + self.s(INNER_PAD) + bs,
            bottom: controls.top + (controls.bottom - controls.top - bs) / 2 + bs,
        }
    }

    fn finish_rect(&self) -> RECT {
        let bs = self.s(BTN_SIZE);
        let controls = self.controls_rect();
        let x = controls.right - self.s(INNER_PAD) - bs;
        RECT {
            left: x,
            top: controls.top + (controls.bottom - controls.top - bs) / 2,
            right: x + bs,
            bottom: controls.top + (controls.bottom - controls.top - bs) / 2 + bs,
        }
    }

    fn bar_area_x(&self) -> i32 {
        self.s(INNER_PAD)
    }

    fn text_x(&self) -> i32 {
        self.s(CONTROLS_X + INNER_PAD + BTN_SIZE + BTN_GAP)
    }

    fn text_width(&self) -> i32 {
        self.s(CONTROLS_W - 2 * (INNER_PAD + BTN_SIZE + BTN_GAP))
    }

    /// Logical -> device px.
    fn s(&self, v: i32) -> i32 {
        (v as f64 * self.dpi_scale) as i32
    }

    fn buttons_visible(&self) -> bool {
        self.mode == Some(PillMode::Recording) && self.mouse_in
    }
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

fn hit_test(pill: &PillWindow, x: i32, y: i32) -> HoverTarget {
    if !pill.buttons_visible() {
        return HoverTarget::None;
    }
    let inside = |r: &RECT| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
    if inside(&pill.discard_rect()) {
        return HoverTarget::Discard;
    }
    if inside(&pill.finish_rect()) {
        return HoverTarget::Finish;
    }
    HoverTarget::Body
}

/// Animated bar levels for modes without live mic data.
fn synthesized_levels(mode: PillMode, elapsed: f64) -> Vec<f32> {
    match mode {
        PillMode::Flash => vec![0.0; BARS],
        _ => (0..BARS)
            .map(|index| {
                let wave = (elapsed * 4.0 + index as f64 * 0.7).sin().abs();
                (0.20 + 0.60 * wave).clamp(0.05, 1.0) as f32
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

/// Per-pixel alpha pass: opaque inside the rounded rectangle, transparent
/// outside. GDI writes leave the alpha channel zero, which AC_SRC_ALPHA would
/// render as a fully transparent window — this pass is what makes it visible.
fn apply_alpha(
    pixels: *mut u8,
    w: i32,
    h: i32,
    wave_rect: &RECT,
    controls_rect: &RECT,
    corner_radius: i32,
) {
    if pixels.is_null() {
        return;
    }
    let stride = w as usize * 4;
    let radius = corner_radius.max(0) as f64;
    for y in 0..h {
        let row = unsafe { pixels.add(y as usize * stride) };
        for x in 0..w {
            let inside = point_in_round_rect(x, y, wave_rect, radius)
                || point_in_round_rect(x, y, controls_rect, radius);
            unsafe {
                *row.add(x as usize * 4 + 3) = if inside { 0xFF } else { 0x00 };
            }
        }
    }
}

fn point_in_round_rect(x: i32, y: i32, rect: &RECT, radius: f64) -> bool {
    if x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom {
        return false;
    }
    let dx = (x - rect.left).min(rect.right - 1 - x) as f64;
    let dy = (y - rect.top).min(rect.bottom - 1 - y) as f64;
    if dx >= radius || dy >= radius {
        true
    } else {
        dx * dx + dy * dy <= radius * radius
    }
}

fn fill_round(hdc: HDC, r: &RECT, radius: i32, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let rgn = CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
        FillRgn(hdc, rgn, brush);
        DeleteObject(brush as _);
        DeleteObject(rgn as _);
    }
}

fn draw_button(
    pill: &PillWindow,
    hdc: HDC,
    r: &RECT,
    glyph: &[u16],
    glyph_color: COLORREF,
    hovered: bool,
) {
    let bg = if hovered {
        COLOR_BTN_HOVER
    } else {
        COLOR_BTN_IDLE
    };
    let radius = pill.s(PILL_RADIUS * 2);
    fill_round(hdc, r, radius, bg);
    unsafe {
        // 1px border, like the original window's action buttons.
        let brush = CreateSolidBrush(COLOR_BORDER);
        let rgn: HRGN =
            CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, radius, radius);
        FrameRgn(hdc, rgn, brush, 1, 1);
        DeleteObject(brush as _);
        DeleteObject(rgn as _);
        SetTextColor(hdc, glyph_color);
        DrawTextW(
            hdc,
            glyph.as_ptr(),
            (glyph.len() - 1) as i32,
            r as *const RECT as *mut RECT,
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
        let hdc = pill.hdc_mem;

        let wave_rect = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: pill.s(WAVE_H),
        };
        let controls_rect = pill.controls_rect();
        let corner = pill.s(PILL_RADIUS * 2); // GDI ellipse size = 2x visual radius

        // Clear the backing store before redrawing the compact panel.
        std::ptr::write_bytes(pill.pixels, 0, (w * h * 4) as usize);
        fill_round(hdc, &wave_rect, corner, COLOR_BG);
        let border_brush = CreateSolidBrush(COLOR_BORDER);
        let outline: HRGN = CreateRoundRectRgn(0, 0, w + 1, h + 1, corner, corner);
        FrameRgn(hdc, outline, border_brush, 1, 1);
        DeleteObject(border_brush as _);
        DeleteObject(outline as _);

        SetBkMode(hdc, 1);

        let segoe: Vec<u16> = "Segoe UI"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let font = CreateFontIndirectW(&LOGFONTW {
            lfHeight: (-(14.0 * pill.dpi_scale)) as i32,
            lfWeight: 600,
            lfFaceName: {
                let mut name = [0u16; 32];
                let copy_len = segoe.len().min(31);
                name[..copy_len].copy_from_slice(&segoe[..copy_len]);
                name
            },
            ..std::mem::zeroed()
        });
        let old_font = SelectObject(hdc, font as _);

        // Bars: live mic levels while recording, self-animated while transcribing.
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

        // Waveform: thin bars resampled from the mic levels, symmetric
        // envelope — tallest in the middle, tapering to dots at the edges.
        let bar_max_h = pill.s(BAR_MAX_H);
        let bar_top = h / 2;
        let bar_x0 = pill.bar_area_x();
        let area_w = w - pill.s(INNER_PAD) - bar_x0;
        let bar_w = pill.s(BAR_W);
        let count = WAVE_BARS as i32;
        let n_in = levels.len();
        if n_in > 0 && area_w > bar_w {
            let gap = ((area_w - count * bar_w) / (count - 1)).max(1);
            let total_w = count * bar_w + (count - 1) * gap;
            let start_x = bar_x0 + (area_w - total_w) / 2;
            let center = (WAVE_BARS as f32 - 1.0) / 2.0;
            for i in 0..count {
                // Linear resample of the input levels down/up to WAVE_BARS.
                let t = i as f32 * (n_in as f32 - 1.0) / (count as f32 - 1.0);
                let i0 = (t.floor() as usize).min(n_in - 1);
                let i1 = (i0 + 1).min(n_in - 1);
                let frac = t - i0 as f32;
                let level = (levels[i0] * (1.0 - frac) + levels[i1] * frac).clamp(0.0, 1.0);
                // Parabolic envelope peaked at the center.
                let env = (1.0 - ((i as f32 - center) / (center + 0.5)).abs().powi(2)).max(0.0);
                let env = 0.82 * env;
                let raw_height = bar_max_h as f32 * env * level;
                if raw_height < pill.s(MIN_BAR_H) as f32 {
                    continue;
                }
                let bar_h = raw_height as i32;
                let bar_x = start_x + i * (bar_w + gap);
                let r = RECT {
                    left: bar_x,
                    top: bar_top - bar_h / 2,
                    right: bar_x + bar_w,
                    bottom: bar_top - bar_h / 2 + bar_h,
                };
                let wave_color = if pill.buttons_visible() {
                    COLOR_WAVE_HOVER
                } else {
                    COLOR_WAVE
                };
                fill_round(hdc, &r, bar_w, wave_color);
            }
        }

        // Hover controls sit over the dim waveform.
        if pill.buttons_visible() {
            let cross: Vec<u16> = "\u{2715}"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let check: Vec<u16> = "\u{2713}"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            draw_button(
                pill,
                hdc,
                &pill.discard_rect(),
                &cross,
                COLOR_TEXT,
                pill.hover == HoverTarget::Discard,
            );
            draw_button(
                pill,
                hdc,
                &pill.finish_rect(),
                &check,
                COLOR_TEXT,
                pill.hover == HoverTarget::Finish,
            );
        }

        // Right slot: timer / dots / done.
        SetTextColor(hdc, COLOR_TEXT);
        let right_text: String = match pill.mode {
            Some(PillMode::Recording) if pill.buttons_visible() => {
                let total = pill.elapsed as u64;
                format!("{}:{:02}", total / 60, total % 60)
            }
            None => String::new(),
            _ => String::new(),
        };
        if !right_text.is_empty() {
            let utf16: Vec<u16> = right_text
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut text_rect = RECT {
                left: pill.text_x(),
                top: controls_rect.top,
                right: pill.text_x() + pill.text_width(),
                bottom: controls_rect.bottom,
            };
            DrawTextW(
                hdc,
                utf16.as_ptr(),
                utf16.len() as i32 - 1,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }

        SelectObject(hdc, old_font);
        DeleteObject(font as _);

        apply_alpha(
            pill.pixels,
            w,
            h,
            &wave_rect,
            &controls_rect,
            pill.s(PILL_RADIUS),
        );

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

fn client_cursor_pos(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
        (pt.x, pt.y)
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
            WM_SETCURSOR => {
                if (lparam & 0xFFFF) as u16 == 1
                /* HTCLIENT */
                {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PillWindow;
                    if !ptr.is_null() {
                        let pill = &*ptr;
                        let (x, y) = client_cursor_pos(hwnd);
                        if matches!(
                            hit_test(pill, x, y),
                            HoverTarget::Discard | HoverTarget::Finish
                        ) {
                            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
                            return 1;
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
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
                let new_hover = hit_test(pill, x, y);
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
                match hit_test(pill, x, y) {
                    HoverTarget::Discard => {
                        let _ = (*ptr).click_tx.send(PillButton::Discard);
                    }
                    HoverTarget::Finish => {
                        let _ = (*ptr).click_tx.send(PillButton::Finish);
                    }
                    HoverTarget::Body => {
                        ReleaseCapture();
                        SendMessageW(
                            hwnd,
                            0x00A1, /* WM_NCLBUTTONDOWN */
                            HTCAPTION as WPARAM,
                            0,
                        );
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
        let class_name: Vec<u16> = "AmanuensisPill"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
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
