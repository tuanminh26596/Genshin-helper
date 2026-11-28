//! System information for Linux diagnostics.
//!
//! Mirrors `gh-win32::sys_info` using /proc and /sys.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsEnvInfo {
    pub os_name: String,
    pub build_number: u32,
    pub ubr: u32,
    pub display_version: String,
    pub arch: String,
    pub logical_cores: usize,
    pub total_ram_gb: u32,
    pub gpu_adapters: Vec<String>,
}

impl OsEnvInfo {
    pub fn collect() -> Self {
        Self {
            os_name: read_os_name(),
            build_number: read_kernel_build_number(),
            ubr: 0,
            display_version: read_kernel_version(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cores: num_cpus(),
            total_ram_gb: read_total_ram_gb(),
            gpu_adapters: read_gpu_adapters(),
        }
    }
}

fn read_os_name() -> String {
    // Try /etc/os-release
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                return rest.trim_matches('"').to_owned();
            }
        }
    }
    "Linux".to_owned()
}

fn read_kernel_version() -> String {
    std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).map(|v| v.to_owned()))
        .unwrap_or_default()
}

fn read_kernel_build_number() -> u32 {
    let ver = read_kernel_version();
    ver.split('.')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn num_cpus() -> usize {
    // Parse /proc/cpuinfo for processor count
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(1)
}

fn read_total_ram_gb() -> u32 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|kb| (kb / 1024 / 1024) as u32)
        .unwrap_or(0)
}

fn read_gpu_adapters() -> Vec<String> {
    // Try lspci or /sys/class/drm
    let mut adapters = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let path = entry.path().join("device/vendor");
            if std::fs::read_to_string(&path).is_ok() {
                let device_path = entry.path().join("device/uevent");
                if let Ok(uevent) = std::fs::read_to_string(device_path) {
                    for line in uevent.lines() {
                        if let Some(driver) = line.strip_prefix("DRIVER=")
                            && !adapters.contains(&driver.to_owned())
                        {
                            adapters.push(driver.to_owned());
                        }
                    }
                }
            }
        }
    }
    if adapters.is_empty() {
        adapters.push("unknown".to_owned());
    }
    adapters
}

/// Open a path in the file manager (for diagnostics).
pub fn open_folder(path: &Path) {
    let _ = std::process::Command::new("xdg-open")
        .arg(path)
        .spawn();
}

/// Non-PII per-monitor geometry, DPI scale, and refresh rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub is_primary: bool,
    pub bounds: [i32; 4],
    pub work_area: [i32; 4],
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub scale_pct: u32,
    pub refresh_hz: u32,
}

/// Multi-monitor topology snapshot without device paths or monitor serials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTopology {
    pub monitor_count: usize,
    pub primary_screen: [i32; 2],
    pub monitors: Vec<MonitorInfo>,
}

/// Read-only game window state and borderless heuristic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameWindowDiag {
    pub found: bool,
    pub is_foreground: bool,
    pub is_minimized: bool,
    pub is_borderless_guess: bool,
    pub client_rect: Option<[i32; 4]>,
    pub window_rect: Option<[i32; 4]>,
    pub monitor_index: Option<usize>,
    pub monitor_scale_pct: Option<u32>,
}

pub fn collect_os_env_info() -> OsEnvInfo {
    OsEnvInfo::collect()
}

pub fn collect_display_topology() -> DisplayTopology {
    let mut monitors = Vec::new();
    if let Some(raw) = crate::capture::hypr_ipc_pub("j/monitors")
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
    {
        for (idx, m) in arr.iter().enumerate() {
            let x = m.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let y = m.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let w = m.get("width").and_then(|v| v.as_i64()).unwrap_or(1920) as i32;
            let h = m.get("height").and_then(|v| v.as_i64()).unwrap_or(1080) as i32;
            let scale = m.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let scale_pct = (scale * 100.0).round() as u32;
            let dpi = (96.0 * scale).round() as u32;
            let refresh = m
                .get("refreshRate")
                .and_then(|v| v.as_f64())
                .unwrap_or(60.0)
                .round() as u32;
            let focused = m.get("focused").and_then(|v| v.as_bool()).unwrap_or(idx == 0);
            monitors.push(MonitorInfo {
                index: idx,
                is_primary: focused,
                bounds: [x, y, w, h],
                work_area: [x, y, w, h],
                dpi_x: dpi,
                dpi_y: dpi,
                scale_pct,
                refresh_hz: refresh.max(1),
            });
        }
    }
    if monitors.is_empty() {
        let (x, y, w, h) = crate::capture::get_screen_bounds();
        monitors.push(MonitorInfo {
            index: 0,
            is_primary: true,
            bounds: [x, y, w, h],
            work_area: [x, y, w, h],
            dpi_x: 96,
            dpi_y: 96,
            scale_pct: 100,
            refresh_hz: 60,
        });
    }
    let primary_screen = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .map_or([1920, 1080], |m| [m.bounds[2], m.bounds[3]]);
    DisplayTopology {
        monitor_count: monitors.len(),
        primary_screen,
        monitors,
    }
}

pub fn probe_game_window_diag(
    hwnd: Option<isize>,
    client_rect: Option<[i32; 4]>,
    topology: &DisplayTopology,
) -> GameWindowDiag {
    let Some(raw_hwnd) = hwnd.filter(|&h| h > 0) else {
        return GameWindowDiag {
            found: false,
            is_foreground: false,
            is_minimized: false,
            is_borderless_guess: false,
            client_rect,
            window_rect: None,
            monitor_index: None,
            monitor_scale_pct: None,
        };
    };
    let rect = crate::capture::WindowFinder::get_window_rect(raw_hwnd);
    let found = rect.is_some();
    let is_foreground = found && crate::capture::WindowFinder::is_foreground(raw_hwnd);
    let window_rect = rect.map(|r| [r.x, r.y, r.width, r.height]);
    let ref_rect = client_rect.or(window_rect);
    let mut monitor_index = None;
    let mut monitor_scale_pct = None;
    let mut fills_monitor = false;
    if let Some([rx, ry, rw, rh]) = ref_rect {
        let cx = rx + rw / 2;
        let cy = ry + rh / 2;
        if let Some(m) = topology.monitors.iter().find(|m| {
            let [mx, my, mw, mh] = m.bounds;
            cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
        }) {
            monitor_index = Some(m.index);
            monitor_scale_pct = Some(m.scale_pct);
            fills_monitor = (rx - m.bounds[0]).abs() <= 2
                && (ry - m.bounds[1]).abs() <= 2
                && (rw - m.bounds[2]).abs() <= 2
                && (rh - m.bounds[3]).abs() <= 2;
        }
    }
    GameWindowDiag {
        found,
        is_foreground,
        is_minimized: false,
        is_borderless_guess: found && fills_monitor,
        client_rect,
        window_rect,
        monitor_index,
        monitor_scale_pct,
    }
}

pub fn reveal_path_in_explorer(path: &Path) -> bool {
    if crate::runtime::is_desktop_sandbox_env() {
        return false;
    }
    let dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    std::process::Command::new("xdg-open")
        .arg(dir)
        .spawn()
        .is_ok()
}

/// Returns `(year, month_1_12, day_1_31, hour, minute, second, millisecond)` in local time.
pub fn local_time_parts() -> (i32, u32, u32, u32, u32, u32, u32) {
    let now = std::time::SystemTime::now();
    let dur = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let ms = dur.subsec_millis();
    // SAFETY: `tm` is zeroed and `localtime_r` is thread-safe.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers to `secs` and `tm`.
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    (
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
        ms,
    )
}

/// Returns `(total_mb, available_mb)` from `/proc/meminfo` (mirrors `gh_win32::sys_info`).
pub fn query_system_memory_mb() -> Option<(u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb: Option<u64> = None;
    let mut avail_kb: Option<u64> = None;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }
    Some((total_kb? / 1024, avail_kb? / 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_system_memory_mb_reports_valid_bounds() {
        let (total, avail) = query_system_memory_mb().expect("/proc/meminfo");
        assert!(total > 0, "total_mb must be positive: {total}");
        assert!(avail <= total, "avail_mb ({avail}) <= total_mb ({total})");
    }
}

