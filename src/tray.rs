#![allow(unsafe_op_in_unsafe_fn)]

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, IMAGE_ICON, KillTimer,
    LR_DEFAULTSIZE, LoadImageW, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_NCCREATE,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::messages::UiMessage;

const TRAY_CALLBACK: u32 = WM_APP + 1;
const MENU_TOGGLE: usize = 1;
const MENU_SETTINGS: usize = 2;
const MENU_QUIT: usize = 3;
const TRAY_ID: u32 = 1;

pub enum TrayCommand {
    SetEnabled(bool),
}

pub struct TrayController {
    commands: Sender<TrayCommand>,
}

impl TrayController {
    pub fn spawn(ui: Sender<UiMessage>, enabled: bool) -> Self {
        let (commands, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("amanuensis-tray".to_owned())
            .spawn(move || tray_thread(receiver, ui, enabled))
            .expect("failed to spawn tray thread");
        Self { commands }
    }

    pub fn sender(&self) -> Sender<TrayCommand> {
        self.commands.clone()
    }
}

struct TrayState {
    ui: Sender<UiMessage>,
    enabled: bool,
    icon: NOTIFYICONDATAW,
}

fn tray_thread(commands: Receiver<TrayCommand>, ui: Sender<UiMessage>, enabled: bool) {
    unsafe {
        let class_name = wide("AmanuensisTray");
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: GetModuleHandleW(std::ptr::null()),
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);

        let state = Box::new(TrayState {
            ui,
            enabled: false,
            icon: std::mem::zeroed(),
        });
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            state_ptr as *const _,
        );
        if hwnd.is_null() {
            drop(Box::from_raw(state_ptr));
            return;
        }

        let state = &mut *state_ptr;
        state.icon = make_icon(hwnd);
        set_enabled(state, enabled);
        SetTimer(hwnd, 1, 100, None);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            while let Ok(command) = commands.try_recv() {
                match command {
                    TrayCommand::SetEnabled(value) => set_enabled(state, value),
                }
            }
            let result = GetMessageW(&mut msg, hwnd, 0, 0);
            if result <= 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
    }
}

unsafe fn set_enabled(state: &mut TrayState, enabled: bool) {
    if enabled == state.enabled {
        return;
    }
    if enabled {
        let mut icon = state.icon;
        Shell_NotifyIconW(NIM_ADD, &mut icon);
    } else {
        let mut icon = state.icon;
        Shell_NotifyIconW(NIM_DELETE, &mut icon);
    }
    state.enabled = enabled;
}

unsafe fn make_icon(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
    icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    icon.hWnd = hwnd;
    icon.uID = TRAY_ID;
    icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    icon.uCallbackMessage = TRAY_CALLBACK;
    icon.hIcon = LoadImageW(
        GetModuleHandleW(std::ptr::null()),
        1 as *const u16,
        IMAGE_ICON,
        16,
        16,
        LR_DEFAULTSIZE,
    ) as _;
    let tip = wide("Amanuensis");
    let tip_len = tip.len().min(icon.szTip.len());
    icon.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);
    icon
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create =
            &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return 1;
    }

    let ptr = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
        as *mut TrayState;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = &mut *ptr;

    match message {
        TRAY_CALLBACK if lparam as u32 == WM_LBUTTONUP => {
            let _ = state.ui.send(UiMessage::OpenSettings);
            0
        }
        TRAY_CALLBACK if lparam as u32 == WM_RBUTTONUP => {
            show_menu(hwnd, state);
            0
        }
        WM_COMMAND => {
            match (wparam & 0xffff) as usize {
                MENU_TOGGLE => {
                    let _ = state.ui.send(UiMessage::TrayToggleRecording);
                }
                MENU_SETTINGS => {
                    let _ = state.ui.send(UiMessage::OpenSettings);
                }
                MENU_QUIT => {
                    let _ = state.ui.send(UiMessage::Quit);
                }
                _ => {}
            }
            0
        }
        WM_TIMER => 0,
        WM_DESTROY => {
            KillTimer(hwnd, 1);
            if state.enabled {
                let mut icon = state.icon;
                Shell_NotifyIconW(NIM_DELETE, &mut icon);
                state.enabled = false;
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn show_menu(hwnd: HWND, state: &TrayState) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let dictate = wide("Dictate");
    let settings = wide("Open settings");
    let quit = wide("Quit Amanuensis");
    AppendMenuW(menu, MF_STRING, MENU_TOGGLE, dictate.as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings.as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    PostMessageW(hwnd, 0x0000_0012, 0, 0);
    DestroyMenu(menu);
    let _ = state;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
