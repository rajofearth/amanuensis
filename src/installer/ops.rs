//! Side-effectful installer operations: registry (HKCU only), Start Menu
//! shortcut, self-copy, close-running-instance, folder picker, and the
//! install/update/uninstall pipelines.

use std::io::ErrorKind;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE, HWND, LPARAM,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_SZ,
    REG_VALUE_TYPE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenMutexW, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    CREATE_NO_WINDOW, DETACHED_PROCESS, MUTEX_MODIFY_STATE, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

use super::{
    log, marker_path, write_marker, APP_WINDOW_TITLE, EXE_NAME, RUN_SUBKEY, RUN_VALUE_NAME,
    SHORTCUT_FILE_NAME, SINGLE_INSTANCE_MUTEX_NAME, UNINSTALL_SUBKEY,
};

const TAG: &str = "installer";

/// fs::copy retries while the previous instance's exe handle lingers.
const COPY_RETRIES: u32 = 8;
const COPY_BACKOFF: Duration = Duration::from_millis(250);
/// How long to wait for the app mutex to disappear after terminating.
const CLOSE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Spawned helpers must never flash a console window.
const HIDDEN_PROCESS: u32 = CREATE_NO_WINDOW | DETACHED_PROCESS;

// ---- Folder picker ----

/// Native folder dialog via a hidden PowerShell `Shell.Application`
/// BrowseForFolder call.
///
/// Why PowerShell instead of COM IFileDialog: windows-sys deliberately ships
/// raw types + GUIDs but NO COM vtable definitions, so calling
/// IFileOpenDialog directly would mean hand-writing ~30 function-pointer
/// vtables; ground rules forbid adding crates (`windows`, `com`). The hidden
/// process keeps GPUI's event loop untouched and `parent_hwnd` still owns the
/// dialog modally. Cancel/empty stdout → None.
pub fn pick_folder(parent_hwnd: isize) -> Option<PathBuf> {
    // 0x0051 = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX.
    let script = format!(
        "$f = (New-Object -ComObject Shell.Application).BrowseForFolder({}, 'Select folder', 0x0051); if ($f) {{ Write-Output $f.Self.Path }}",
        parent_hwnd
    );
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !chosen.is_empty() {
        Some(PathBuf::from(chosen))
    } else {
        None
    }
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn run_hidden_powershell(script: &str) -> Result<(), String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("spawning powershell: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "powershell failed ({}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

// ---- Start Menu shortcut ----

fn start_menu_lnk_path(appdata: &std::ffi::OsStr) -> PathBuf {
    PathBuf::from(appdata)
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join(SHORTCUT_FILE_NAME)
}

fn shortcut_lnk_path() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA not set")?;
    Ok(start_menu_lnk_path(&appdata))
}

pub fn start_menu_shortcut_exists() -> bool {
    shortcut_lnk_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Create (add=true) or remove (add=false) the per-user Start Menu shortcut
/// for `exe`. Removal ignores a missing file.
pub fn set_start_menu_shortcut(exe: &Path, add: bool) -> Result<(), String> {
    let lnk = shortcut_lnk_path()?;
    if !add {
        return match std::fs::remove_file(&lnk) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("removing {}: {error}", lnk.display())),
        };
    }
    // Creating .lnk files needs COM IShellLink, which windows-sys does not
    // bind (no vtable definitions); spawning WScript.Shell through hidden
    // PowerShell is the crate-free way to get a real shell link.
    let working_dir = exe.parent().unwrap_or(Path::new("."));
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; $s.WorkingDirectory = {}; $s.Description = 'Amanuensis'; $s.Save()",
        ps_quote(&lnk.display().to_string()),
        ps_quote(&exe.display().to_string()),
        ps_quote(&working_dir.display().to_string()),
    );
    run_hidden_powershell(&script)?;
    if lnk.exists() {
        Ok(())
    } else {
        Err(format!("shortcut did not appear at {}", lnk.display()))
    }
}

// ---- Autostart (HKCU Run key) ----

fn run_key_value(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

pub fn autostart_enabled() -> bool {
    reg_read_string(RUN_SUBKEY, RUN_VALUE_NAME).is_some()
}

pub fn set_autostart(exe: &Path, enabled: bool) -> Result<(), String> {
    if enabled {
        reg_set_string(RUN_SUBKEY, RUN_VALUE_NAME, &run_key_value(exe))
    } else {
        reg_delete_value(RUN_SUBKEY, RUN_VALUE_NAME)
    }
}

// ---- Registry plumbing (all HKCU; no elevation involved) ----

pub(crate) fn registry_key_exists(subkey: &str) -> bool {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let name = wide(subkey);
        let opened =
            RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey);
        if opened == ERROR_SUCCESS {
            RegCloseKey(hkey);
            true
        } else {
            false
        }
    }
}

pub(crate) fn reg_read_string(subkey: &str, value_name: &str) -> Option<String> {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let key_name = wide(subkey);
        if RegOpenKeyExW(HKEY_CURRENT_USER, key_name.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return None;
        }
        let value = read_sz(hkey, value_name);
        RegCloseKey(hkey);
        value
    }
}

unsafe fn read_sz(hkey: HKEY, value_name: &str) -> Option<String> {
    unsafe {
        let name = wide(value_name);
        let mut value_type: REG_VALUE_TYPE = 0;
        let mut size: u32 = 0;
        let status = RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut size,
        );
        if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
            return None;
        }
        if value_type != REG_SZ || size == 0 {
            return None;
        }
        let mut buffer = vec![0_u8; size as usize];
        let mut filled = buffer.len() as u32;
        if RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buffer.as_mut_ptr(),
            &mut filled,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut words: Vec<u16> = buffer[..filled as usize]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        while words.last() == Some(&0) {
            words.pop();
        }
        String::from_utf16(&words).ok()
    }
}

fn reg_set_string(subkey: &str, value_name: &str, value: &str) -> Result<(), String> {
    let mut data = wide(value);
    reg_set(
        subkey,
        value_name,
        REG_SZ,
        data.as_mut_ptr().cast::<u8>(),
        (data.len() * 2) as u32,
    )
}

fn reg_set_dword(subkey: &str, value_name: &str, value: u32) -> Result<(), String> {
    let mut data = value.to_le_bytes();
    reg_set(
        subkey,
        value_name,
        REG_DWORD,
        data.as_mut_ptr(),
        data.len() as u32,
    )
}

fn reg_set(
    subkey: &str,
    value_name: &str,
    value_type: REG_VALUE_TYPE,
    data: *const u8,
    size: u32,
) -> Result<(), String> {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let key_name = wide(subkey);
        let status = RegOpenKeyExW(HKEY_CURRENT_USER, key_name.as_ptr(), 0, KEY_SET_VALUE, &mut hkey);
        if status == ERROR_FILE_NOT_FOUND {
            // Parent keys like ...\Uninstall may not exist yet on fresh
            // machines; RegSetValueExW does NOT create intermediate keys.
            create_key_chain(subkey)?;
            let status =
                RegOpenKeyExW(HKEY_CURRENT_USER, key_name.as_ptr(), 0, KEY_SET_VALUE, &mut hkey);
            if status != ERROR_SUCCESS {
                return Err(format!("opening {subkey}: win32 error {status}"));
            }
        } else if status != ERROR_SUCCESS {
            return Err(format!("opening {subkey}: win32 error {status}"));
        }
        let value_name_wide = wide(value_name);
        let status = RegSetValueExW(hkey, value_name_wide.as_ptr(), 0, value_type, data, size);
        RegCloseKey(hkey);
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("setting {subkey}\\{value_name}: win32 error {status}"))
        }
    }
}

/// Create every component of `subkey` under HKCU (RegCreateKeyExW per level).
fn create_key_chain(subkey: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{RegCreateKeyExW, KEY_WRITE};
    unsafe {
        let mut prefix = String::new();
        for component in subkey.split('\\') {
            if !prefix.is_empty() {
                prefix.push('\\');
            }
            prefix.push_str(component);
            let mut hkey: HKEY = std::ptr::null_mut();
            let name = wide(&prefix);
            let status = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                name.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_WRITE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            );
            if status != ERROR_SUCCESS {
                return Err(format!("creating {prefix}: win32 error {status}"));
            }
            RegCloseKey(hkey);
        }
    }
    Ok(())
}

fn reg_delete_value(subkey: &str, value_name: &str) -> Result<(), String> {
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let key_name = wide(subkey);
        let status = RegOpenKeyExW(HKEY_CURRENT_USER, key_name.as_ptr(), 0, KEY_SET_VALUE, &mut hkey);
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if status != ERROR_SUCCESS {
            return Err(format!("opening {subkey}: win32 error {status}"));
        }
        let name = wide(value_name);
        let status = RegDeleteValueW(hkey, name.as_ptr());
        RegCloseKey(hkey);
        match status {
            ERROR_SUCCESS => Ok(()),
            ERROR_FILE_NOT_FOUND => Ok(()),
            other => Err(format!("deleting {subkey}\\{value_name}: win32 error {other}")),
        }
    }
}

fn reg_delete_tree(subkey: &str) -> Result<(), String> {
    let name = wide(subkey);
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, name.as_ptr()) };
    match status {
        ERROR_SUCCESS => Ok(()),
        ERROR_FILE_NOT_FOUND => Ok(()),
        other => Err(format!("deleting key {subkey}: win32 error {other}")),
    }
}

fn uninstall_registry_values(exe: &Path, dir: &Path) -> Result<(), String> {
    reg_set_string(UNINSTALL_SUBKEY, "DisplayName", "Amanuensis")?;
    reg_set_string(UNINSTALL_SUBKEY, "DisplayVersion", super::APP_VERSION)?;
    reg_set_string(UNINSTALL_SUBKEY, "DisplayIcon", &exe.display().to_string())?;
    reg_set_string(UNINSTALL_SUBKEY, "InstallLocation", &dir.display().to_string())?;
    reg_set_string(UNINSTALL_SUBKEY, "UninstallString", &uninstall_string(exe))?;
    reg_set_dword(UNINSTALL_SUBKEY, "NoModify", 1)?;
    reg_set_dword(UNINSTALL_SUBKEY, "NoRepair", 1)
}

fn uninstall_string(exe: &Path) -> String {
    format!("\"{}\" --uninstall", exe.display())
}

// ---- Wide-string helper (repo idiom from main.rs / pill_win32.rs) ----

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- Close running instance ----

/// Terminate any running Amanuensis app instance so its exe can be replaced.
/// Detects it via the single-instance mutex (name pinned in main.rs), finds
/// its window by title, verifies the owning process is NOT this installer,
/// then terminates. No mutex → no-op.
fn close_running_instance() {
    if !mutex_is_held() {
        log!(TAG, "no running instance (mutex free)");
        return;
    }
    log!(TAG, "single-instance mutex held; closing running Amanuensis");
    for hwnd in find_app_windows() {
        terminate_window_process(hwnd);
    }
    wait_for_mutex_release();
    log!(TAG, "close-running-instance done");
}

fn mutex_is_held() -> bool {
    let name = wide(SINGLE_INSTANCE_MUTEX_NAME);
    unsafe {
        let existing = OpenMutexW(MUTEX_MODIFY_STATE, 0, name.as_ptr());
        if existing.is_null() {
            false
        } else {
            CloseHandle(existing);
            true
        }
    }
}

fn wait_for_mutex_release() {
    let deadline = Instant::now() + CLOSE_WAIT_TIMEOUT;
    while Instant::now() < deadline && mutex_is_held() {
        thread::sleep(CLOSE_POLL_INTERVAL);
    }
}

fn find_app_windows() -> Vec<HWND> {
    struct EnumCtx {
        title: Vec<u16>,
        found: *mut Vec<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let ctx = &mut *(lparam as *mut EnumCtx);
            let length = GetWindowTextLengthW(hwnd);
            if length <= 0 {
                return 1;
            }
            let mut buffer = vec![0_u16; (length + 1) as usize];
            GetWindowTextW(hwnd, buffer.as_mut_ptr(), length + 1);
            while buffer.last() == Some(&0) {
                buffer.pop();
            }
            if buffer != ctx.title {
                return 1;
            }
            (*ctx.found).push(hwnd);
            1
        }
    }

    let mut found: Vec<HWND> = Vec::new();
    let mut ctx = EnumCtx {
        title: wide(APP_WINDOW_TITLE),
        found: &mut found,
    };
    unsafe { EnumWindows(Some(enum_proc), &mut ctx as *mut EnumCtx as LPARAM) };
    found
}

fn terminate_window_process(hwnd: HWND) {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || pid == GetCurrentProcessId() {
            return;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            log!(TAG, "could not open pid {pid} to terminate");
            return;
        }
        if is_this_installer(process) {
            log!(TAG, "refusing to terminate own installer process");
            CloseHandle(process);
            return;
        }
        let terminated = TerminateProcess(process, 0);
        CloseHandle(process);
        log!(TAG, "terminate pid {pid}: {}", terminated != 0);
    }
}

/// True when the process image behind `process` resolves to THIS exe — the
/// safety rail that keeps the installer from killing itself when both share
/// the same window title.
fn is_this_installer(process: HANDLE) -> bool {
    let mut path = [0_u16; 1024];
    let mut size = path.len() as u32;
    unsafe {
        if QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            path.as_mut_ptr(),
            &mut size,
        ) == 0
        {
            return false;
        }
    }
    let image = String::from_utf16_lossy(&path[..size as usize]);
    match std::env::current_exe() {
        Ok(me) => image.eq_ignore_ascii_case(&me.display().to_string()),
        Err(_) => false,
    }
}

// ---- Self copy ----

fn copy_self_to_dir(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
    let source = std::env::current_exe().map_err(|error| format!("locating current exe: {error}"))?;
    let target = dir.join(EXE_NAME);
    let mut last_error: Option<std::io::Error> = None;
    for attempt in 0..COPY_RETRIES {
        match std::fs::copy(&source, &target) {
            Ok(_) => {
                if attempt > 0 {
                    log!(TAG, "self-copy succeeded on attempt {}", attempt + 1);
                }
                return Ok(target);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::PermissionDenied
                ) || matches!(error.raw_os_error(), Some(32 | 33)) =>
            {
                last_error = Some(error);
                thread::sleep(COPY_BACKOFF);
            }
            Err(error) => {
                return Err(format!("copying to {}: {error}", target.display()));
            }
        }
    }
    Err(format!(
        "{} still locked after {COPY_RETRIES} attempts: {}",
        target.display(),
        last_error.map(|e| e.to_string()).unwrap_or_default()
    ))
}

// ---- Launch installed exe ----

/// Launch via std Command: the child outlives us naturally (no handles to
/// leak, nothing kills it when the installer exits) and GUI-subsystem exes
/// need no console flags.
fn launch_detached(exe: &Path) -> Result<(), String> {
    Command::new(exe)
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .spawn()
        .map(|child| std::mem::forget(child))
        .map_err(|error| format!("launching {}: {error}", exe.display()))
}

// ---- Pipelines ----

fn announce(on_step: &mut dyn FnMut(super::Step, &str), step: super::Step, enabled: bool) {
    on_step(step, step.label(enabled));
}

/// Shared install/update sequence:
/// close running → self-copy → shortcut? → run key? → registry + marker → launch.
fn pipeline(opts: &super::InstallOptions, on_step: &mut dyn FnMut(super::Step, &str)) -> Result<(), String> {
    let target = opts.dir.join(EXE_NAME);

    announce(on_step, super::Step::CloseRunning, true);
    close_running_instance();

    announce(on_step, super::Step::CopyFiles, true);
    copy_self_to_dir(&opts.dir)?;

    announce(on_step, super::Step::StartMenu, opts.start_menu);
    if opts.start_menu {
        set_start_menu_shortcut(&target, true)?;
    }

    announce(on_step, super::Step::AutoStart, opts.autostart);
    if opts.autostart {
        set_autostart(&target, true)?;
    }

    announce(on_step, super::Step::Register, true);
    uninstall_registry_values(&target, &opts.dir)?;
    write_marker(&opts.dir)?;

    announce(on_step, super::Step::Launch, true);
    launch_detached(&target)?;

    Ok(())
}

pub fn run_install(
    opts: &super::InstallOptions,
    on_step: &mut dyn FnMut(super::Step, &str),
) -> Result<(), String> {
    log!(
        TAG,
        "installing v{} to {}",
        super::APP_VERSION,
        opts.dir.display()
    );
    pipeline(opts, on_step)
}

pub fn run_update(
    opts: &super::InstallOptions,
    installed: &super::InstalledInfo,
    on_step: &mut dyn FnMut(super::Step, &str),
) -> Result<(), String> {
    log!(
        TAG,
        "updating v{} -> v{} in {}",
        installed.version,
        super::APP_VERSION,
        opts.dir.display()
    );
    pipeline(opts, on_step)
}

/// Uninstall sequence reusing the same Step slots (message text differs):
/// close running → remove shortcut → remove run key → remove uninstall key →
/// delete files (+ optional config/models) via the classic self-delete trick.
pub fn run_uninstall(
    keep_data: bool,
    on_step: &mut dyn FnMut(super::Step, &str),
) -> Result<(), String> {
    let dir = super::install_location();
    log!(
        TAG,
        "uninstalling v{} from {} (keep_data: {keep_data})",
        installed_version_or_unknown(&dir),
        dir.display()
    );

    on_step(super::Step::CloseRunning, "Closing Amanuensis…");
    close_running_instance();

    on_step(super::Step::StartMenu, "Removing Start menu shortcut…");
    set_start_menu_shortcut(&dir.join(EXE_NAME), false)?;

    on_step(super::Step::AutoStart, "Removing auto-start entry…");
    set_autostart(Path::new(""), false)?;

    on_step(super::Step::Register, "Removing uninstall registration…");
    reg_delete_tree(UNINSTALL_SUBKEY)?;

    on_step(super::Step::CopyFiles, "Deleting files…");
    delete_install_data(keep_data, &dir)?;

    Ok(())
}

fn installed_version_or_unknown(dir: &Path) -> String {
    super::installed_version(dir).unwrap_or_else(|| "unknown".to_owned())
}

fn delete_install_data(keep_data: bool, dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(marker_path(dir));
    if !keep_data {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let data_dir = PathBuf::from(appdata).join(super::CONFIG_DIR_NAME);
            match std::fs::remove_dir_all(&data_dir) {
                Ok(()) => log!(TAG, "removed data dir {}", data_dir.display()),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("removing {}: {error}", data_dir.display()));
                }
            }
        }
    }
    spawn_self_delete(dir)
}

fn self_delete_command(dir: &Path) -> String {
    format!(
        "ping -n 2 127.0.0.1 > NUL & rd /s /q \"{}\"",
        dir.display()
    )
}

/// The uninstaller IS the installed exe, so it cannot delete its own dir
/// while running. Hand the deletion to an outliving detached cmd that waits
/// two pings first; we exit right after returning (classic trick, spec-approved).
fn spawn_self_delete(dir: &Path) -> Result<(), String> {
    let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    Command::new(comspec)
        .arg("/c")
        .arg(self_delete_command(dir))
        .creation_flags(HIDDEN_PROCESS)
        .spawn()
        .map(|child| std::mem::forget(child))
        .map_err(|error| format!("spawning cleanup cmd: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_quote_wraps_in_single_quotes_and_doubles_embedded_ones() {
        assert_eq!(ps_quote("C:\\Apps"), "'C:\\Apps'");
        assert_eq!(ps_quote("it's here"), "'it''s here'");
    }

    #[test]
    fn run_key_value_quotes_the_exe_path() {
        assert_eq!(
            run_key_value(Path::new("C:\\Apps\\Amanuensis\\amanuensis.exe")),
            "\"C:\\Apps\\Amanuensis\\amanuensis.exe\""
        );
    }

    #[test]
    fn uninstall_string_quotes_exe_and_appends_flag() {
        assert_eq!(
            uninstall_string(Path::new("C:\\Apps\\amanuensis.exe")),
            "\"C:\\Apps\\amanuensis.exe\" --uninstall"
        );
    }

    #[test]
    fn start_menu_lnk_lands_in_programs_folder() {
        assert_eq!(
            start_menu_lnk_path(std::ffi::OsStr::new("C:\\Users\\x\\AppData\\Roaming")),
            PathBuf::from(
                "C:\\Users\\x\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Amanuensis.lnk"
            )
        );
    }

    #[test]
    fn self_delete_command_pings_then_removes_dir_quoted() {
        let command = self_delete_command(Path::new("C:\\Programs\\Amanuensis"));
        assert!(command.starts_with("ping -n 2 127.0.0.1 > NUL & rd /s /q \""));
        assert!(command.ends_with("\\Programs\\Amanuensis\""));
    }

    #[test]
    fn wide_appends_nul_terminator() {
        assert_eq!(wide("ab"), vec![b'a' as u16, b'b' as u16, 0]);
    }
}
