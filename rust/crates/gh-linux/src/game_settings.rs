//! Game settings stub for Linux.
//!
//! On Windows this handles UIPI elevation, HoYoPlay registry paths, and
//! SetWindowDisplayAffinity. On Linux these are either not needed or trivially
//! replaced.

use serde::{Deserialize, Serialize};

pub use gh_core::ui_layout::{
    AspectLabel, HdrClippingReport, build_borderless_launch_args, detect_hdr_clipping,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UipiStatus {
    pub self_elevated: bool,
    pub game_elevated: Option<bool>,
    pub uipi_blocked: bool,
    pub needs_elevation: bool,
    pub mismatch: bool,
    pub reason: String,
}

/// On Linux there is no Windows UIPI; always returns a non-blocked status.
pub fn check_uipi_status(_hwnd: Option<isize>) -> UipiStatus {
    UipiStatus::default()
}

pub fn is_process_elevated() -> bool {
    false
}

pub fn relaunch_current_exe_elevated(_extra_args: &[&str]) -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BorderlessLaunchResult {
    pub ok: bool,
    pub dry_run: bool,
    pub exe_path: Option<String>,
    pub cmd: Vec<String>,
    pub pid: Option<u32>,
    pub error: Option<String>,
}

/// On Linux, "borderless" is handled by the window manager / Proton / gamescope.
pub fn launch_genshin_borderless(
    _width: Option<u32>,
    _height: Option<u32>,
    exe_path: Option<&str>,
    dry_run: bool,
) -> BorderlessLaunchResult {
    let resolved = exe_path
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| find_genshin_exe().map(|p| p.to_string_lossy().into_owned()));
    BorderlessLaunchResult {
        ok: resolved.is_some(),
        dry_run,
        exe_path: resolved,
        cmd: Vec::new(),
        pid: None,
        error: None,
    }
}

/// No-op on Linux — WDA_EXCLUDEFROMCAPTURE is a Windows DWM feature.
pub fn refresh_exclude_from_capture(_hwnd: isize) {}

/// Returns the game executable path by searching for Genshin Impact in common
/// Proton/Steam library locations.
pub fn find_genshin_exe() -> Option<std::path::PathBuf> {
    // Common Steam library paths on Linux
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        format!("{home}/.steam/steam/steamapps/common/Genshin Impact/GenshinImpact.exe"),
        format!("{home}/.local/share/Steam/steamapps/common/Genshin Impact/GenshinImpact.exe"),
        "/opt/games/GenshinImpact/GenshinImpact.exe".to_string(),
    ];
    for path in &candidates {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}
