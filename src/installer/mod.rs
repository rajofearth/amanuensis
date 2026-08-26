//! Installer ops layer: mode detection, install/update/uninstall pipelines,
//! registry + shortcut operations. No GPUI here — PART 2 (installer_ui.rs)
//! renders against this contract only.

pub mod ops;

pub use ops::{
    autostart_enabled, pick_folder, run_install, run_uninstall, run_update,
    set_autostart, set_start_menu_shortcut, start_menu_shortcut_exists,
};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::log;

/// Version this exe installs (what an Update upgrades TO).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Installed exe file name inside the install dir.
pub const EXE_NAME: &str = "amanuensis.exe";
/// Marker written next to the installed exe ({version, dir}).
pub const MARKER_FILE: &str = "install.json";

/// Must match WINDOW_TITLE in src/main.rs — how we find a running app window.
pub const APP_WINDOW_TITLE: &str = "amanuensis-window";
/// Must match the name in src/main.rs acquire_single_instance_lock().
pub const SINGLE_INSTANCE_MUTEX_NAME: &str = "Local\\amanuensis-single-instance";
/// HKCU uninstall entry for Add/Remove Programs.
pub const UNINSTALL_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Amanuensis";
/// HKCU autostart key.
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Run-key value name for Amanuensis.
pub const RUN_VALUE_NAME: &str = "Amanuensis";
/// Per-user Start Menu shortcut file name.
pub const SHORTCUT_FILE_NAME: &str = "Amanuensis.lnk";
/// Must match the dir in config.rs config_dir() — removed on full uninstall.
pub const CONFIG_DIR_NAME: &str = "amanuensis";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    /// Exe lives inside the install dir → normal dictation app.
    App,
    /// No install found → show the installer intro.
    Install,
    /// Existing install elsewhere → updater pre-filled from machine state.
    Update(InstalledInfo),
    /// Launched with --uninstall.
    Uninstall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledInfo {
    pub dir: PathBuf,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOptions {
    pub dir: PathBuf,
    pub start_menu: bool,
    pub autostart: bool,
}

/// Pipeline slots. `on_step(step, message)` is called once BEFORE each step
/// runs; `message` is the display line ("Copying files…"). Install/update pass
/// Step::label(enabled) so skipped toggles read as "Skipping …"; uninstall
/// passes removal phrasing on the same slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    CloseRunning,
    CopyFiles,
    StartMenu,
    AutoStart,
    Register,
    Launch,
}

/// Ordered steps for progress fraction computation in the UI.
pub const ALL_STEPS: [Step; 6] = [
    Step::CloseRunning,
    Step::CopyFiles,
    Step::StartMenu,
    Step::AutoStart,
    Step::Register,
    Step::Launch,
];

impl Step {
    pub fn label(&self, enabled: bool) -> &'static str {
        match self {
            Step::CloseRunning => "Closing Amanuensis…",
            Step::CopyFiles => "Copying files…",
            Step::StartMenu => {
                if enabled {
                    "Adding Start menu shortcut…"
                } else {
                    "Skipping Start menu shortcut"
                }
            }
            Step::AutoStart => {
                if enabled {
                    "Setting up auto-start…"
                } else {
                    "Skipping auto-start"
                }
            }
            Step::Register => "Finishing…",
            Step::Launch => "Launching Amanuensis…",
        }
    }
}

pub fn default_install_dir() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA");
    if local.is_none() {
        log!("installer", "LOCALAPPDATA not set; using relative fallback");
    }
    programs_dir(local.as_deref())
}

fn programs_dir(local_app_data: Option<&std::ffi::OsStr>) -> PathBuf {
    match local_app_data {
        Some(base) => PathBuf::from(base).join("Programs").join("Amanuensis"),
        None => PathBuf::from("Programs").join("Amanuensis"),
    }
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_FILE)
}

#[derive(Serialize, Deserialize)]
struct InstallMarker {
    version: String,
    dir: String,
}

/// Write the {version, dir} marker next to the installed exe.
pub fn write_marker(dir: &Path) -> Result<(), String> {
    let marker = InstallMarker {
        version: APP_VERSION.to_owned(),
        dir: dir.display().to_string(),
    };
    let json = serde_json::to_string_pretty(&marker)
        .map_err(|error| format!("serializing install marker: {error}"))?;
    std::fs::write(marker_path(dir), json)
        .map_err(|error| format!("writing {}: {error}", marker_path(dir).display()))
}

/// Installed version according to the marker file, if any.
pub fn installed_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(marker_path(dir)).ok()?;
    let marker: InstallMarker = serde_json::from_str(&text).ok()?;
    (!marker.version.is_empty()).then_some(marker.version)
}

pub fn detect_mode() -> LaunchMode {
    let uninstall_requested = std::env::args().any(|arg| arg == "--uninstall");
    let installed = query_installed();
    let running_from_install_dir =
        running_from_install_dir(installed.as_ref().map(|info| &info.dir));
    classify_mode(uninstall_requested, running_from_install_dir, installed)
}

/// Pure decision core of detect_mode (unit-tested).
fn classify_mode(
    uninstall_requested: bool,
    running_from_install_dir: bool,
    installed: Option<InstalledInfo>,
) -> LaunchMode {
    if uninstall_requested {
        LaunchMode::Uninstall
    } else if running_from_install_dir {
        LaunchMode::App
    } else {
        match installed {
            Some(info) => LaunchMode::Update(info),
            None => LaunchMode::Install,
        }
    }
}

/// Where the current install lives, per registry InstallLocation falling back
/// to the default dir; None when neither the marker nor the uninstall key
/// exists anywhere (i.e. nothing is installed).
fn query_installed() -> Option<InstalledInfo> {
    let dir = install_location();
    let marker_version = installed_version(&dir);
    let has_key = ops::registry_key_exists(UNINSTALL_SUBKEY);
    if marker_version.is_none() && !has_key {
        return None;
    }
    let version = marker_version.or_else(|| ops::reg_read_string(UNINSTALL_SUBKEY, "DisplayVersion"));
    Some(InstalledInfo {
        dir,
        version: version.unwrap_or_default(),
    })
}

/// Install dir per the registry entry, or the default when absent/invalid.
pub fn install_location() -> PathBuf {
    ops::reg_read_string(UNINSTALL_SUBKEY, "InstallLocation")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(default_install_dir)
}

fn running_from_install_dir(install_dir: Option<&PathBuf>) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let dir = install_dir.cloned().unwrap_or_else(default_install_dir);
    same_file(&exe, &dir.join(EXE_NAME))
}

/// Canonicalized comparison; false when either side cannot be resolved.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "amanuensis-installer-test-{}-{unique}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn classify_prefers_uninstall_flag_over_everything() {
        let installed = Some(InstalledInfo {
            dir: PathBuf::from("C:\\Apps\\Amanuensis"),
            version: "1.0.0".to_owned(),
        });
        assert_eq!(
            classify_mode(true, true, installed.clone()),
            LaunchMode::Uninstall
        );
        assert_eq!(
            classify_mode(true, false, installed),
            LaunchMode::Uninstall
        );
    }

    #[test]
    fn running_inside_install_dir_is_app_mode() {
        assert_eq!(classify_mode(false, true, None), LaunchMode::App);
        assert_eq!(
            classify_mode(false, true, Some(InstalledInfo {
                dir: PathBuf::from("C:\\other"),
                version: "9.9".to_owned(),
            })),
            LaunchMode::App
        );
    }

    #[test]
    fn outside_exe_without_install_is_install_mode() {
        assert_eq!(classify_mode(false, false, None), LaunchMode::Install);
    }

    #[test]
    fn outside_exe_with_install_is_update_mode() {
        let info = InstalledInfo {
            dir: PathBuf::from("C:\\Apps\\Amanuensis"),
            version: "1.0.0".to_owned(),
        };
        assert_eq!(
            classify_mode(false, false, Some(info.clone())),
            LaunchMode::Update(info)
        );
    }

    #[test]
    fn programs_dir_nests_under_local_app_data() {
        assert_eq!(
            programs_dir(Some(std::ffi::OsStr::new("C:\\Users\\x\\AppData\\Local"))),
            PathBuf::from("C:\\Users\\x\\AppData\\Local\\Programs\\Amanuensis")
        );
        assert_eq!(
            programs_dir(None),
            PathBuf::from("Programs").join("Amanuensis")
        );
    }

    #[test]
    fn marker_round_trips_version_and_dir() {
        let dir = temp_dir("marker");
        write_marker(&dir).expect("write marker");
        assert_eq!(installed_version(&dir), Some(APP_VERSION.to_owned()));
        // Corrupt content reads as absent rather than panicking.
        std::fs::write(marker_path(&dir), "{not json").unwrap();
        assert_eq!(installed_version(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_marker_reads_as_absent() {
        let dir = temp_dir("missing-marker");
        assert_eq!(installed_version(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn step_labels_cover_both_toggle_states() {
        assert_eq!(Step::CloseRunning.label(true), "Closing Amanuensis…");
        assert_eq!(Step::CopyFiles.label(true), "Copying files…");
        assert_eq!(
            Step::StartMenu.label(true),
            "Adding Start menu shortcut…"
        );
        assert_eq!(
            Step::StartMenu.label(false),
            "Skipping Start menu shortcut"
        );
        assert_eq!(Step::AutoStart.label(true), "Setting up auto-start…");
        assert_eq!(Step::AutoStart.label(false), "Skipping auto-start");
        assert_eq!(Step::Register.label(true), "Finishing…");
        assert_eq!(Step::Launch.label(true), "Launching Amanuensis…");
        // Non-toggle steps ignore the enabled flag.
        assert_eq!(Step::CopyFiles.label(false), Step::CopyFiles.label(true));
    }

    #[test]
    fn all_steps_are_ordered_and_distinct() {
        let mut seen = ALL_STEPS.to_vec();
        seen.dedup();
        assert_eq!(seen.len(), ALL_STEPS.len());
        assert_eq!(ALL_STEPS[0], Step::CloseRunning);
        assert_eq!(*ALL_STEPS.last().unwrap(), Step::Launch);
    }

    #[test]
    fn same_file_compares_canonical_paths() {
        let dir = temp_dir("samefile");
        let a = dir.join("probe.txt");
        std::fs::write(&a, "x").unwrap();
        assert!(same_file(&a, &a));
        assert!(same_file(&a, &dir.join(".").join("probe.txt")));
        assert!(!same_file(&a, &dir.join("other.txt")));
        // Unresolvable paths compare unequal instead of panicking.
        assert!(!same_file(
            &dir.join("nope.txt"),
            &dir.join("also-nope.txt")
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
