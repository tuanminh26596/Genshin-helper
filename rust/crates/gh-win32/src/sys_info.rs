//! Non-PII Windows hardware, OS build, display topology, DPI, and game-window diagnostics,
//! plus safe Explorer reveal helper for diagnostic bundles.
//!
//! Strictly collects generic technical telemetry only: NEVER collects usernames, machine names,
//! local user paths, monitor serials/EDIDs, GPU device IDs, or non-loopback network addresses.

use crate::capture::ensure_dpi_awareness;
use crate::runtime::is_desktop_sandbox_env;
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_MIRRORING_DRIVER, DISPLAY_DEVICEW,
    ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW,
    GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetForegroundWindow, GetSystemMetrics, GetWindowLongW, GetWindowRect, IsIconic,
    IsWindow, MONITORINFOF_PRIMARY, SM_CXSCREEN, SM_CYSCREEN, WS_CAPTION,
};
use windows::core::{BOOL, PCWSTR, w};

const WIN11_MIN_BUILD: u32 = 22_000;

struct HkeyGuard(HKEY);

impl Drop for HkeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is a valid opened Win32 registry key handle.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
            self.0 = HKEY::default();
        }
    }
}

/// Non-PII OS, hardware & generic GPU adapter summary.
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

/// Non-PII per-monitor geometry, DPI scale, and refresh rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub is_primary: bool,
    /// `[left, top, width, height]` in virtual screen pixels.
    pub bounds: [i32; 4],
    /// `[left, top, width, height]` work area excluding taskbar.
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

fn utf16_slice_to_trimmed_string(slice: &[u16]) -> String {
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end]).trim().to_owned()
}

fn read_hklm_string(subkey: PCWSTR, value_name: PCWSTR) -> Option<String> {
    let mut hkey = HKEY::default();
    // SAFETY: `subkey` is NUL-terminated static wide string; opened read-only.
    let open_res = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, Some(0), KEY_READ, &mut hkey) };
    if open_res.is_err() {
        return None;
    }
    let _guard = HkeyGuard(hkey);

    let mut reg_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0u32;
    // SAFETY: Query required byte length with `None` buffer.
    let size_res = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut reg_type),
            None,
            Some(&mut byte_len),
        )
    };
    if size_res.is_err() || byte_len == 0 || reg_type != REG_SZ {
        return None;
    }

    let mut buf = vec![0u16; (byte_len as usize).div_ceil(2)];
    let mut actual_bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` has capacity `actual_bytes`.
    let read_res = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut reg_type),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut actual_bytes),
        )
    };
    if read_res.is_err() {
        return None;
    }

    let u16_count = (actual_bytes as usize) / 2;
    let s = utf16_slice_to_trimmed_string(&buf[..u16_count.min(buf.len())]);
    if s.is_empty() { None } else { Some(s) }
}

fn read_hklm_u32(subkey: PCWSTR, value_name: PCWSTR) -> Option<u32> {
    let mut hkey = HKEY::default();
    // SAFETY: `subkey` is NUL-terminated static wide string; opened read-only.
    let open_res = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey, Some(0), KEY_READ, &mut hkey) };
    if open_res.is_err() {
        return None;
    }
    let _guard = HkeyGuard(hkey);

    let mut reg_type = REG_VALUE_TYPE::default();
    let mut val = 0u32;
    let mut byte_len = size_of::<u32>() as u32;
    // SAFETY: `val` is a 4-byte `u32` matching `REG_DWORD`.
    let read_res = unsafe {
        RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut reg_type),
            Some((&mut val as *mut u32).cast::<u8>()),
            Some(&mut byte_len),
        )
    };
    if read_res.is_ok() && reg_type == REG_DWORD && byte_len == size_of::<u32>() as u32 {
        Some(val)
    } else {
        None
    }
}

/// Pure helper selecting generic active, non-mirroring GPU adapter names with deduplication.
pub fn filter_gpu_adapter_names<I>(candidates: I) -> Vec<String>
where
    I: IntoIterator<Item = (String, u32)>,
{
    let mut out: Vec<String> = Vec::new();
    for (name, flags) in candidates {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_active = (flags & DISPLAY_DEVICE_ACTIVE.0) != 0;
        let is_mirror = (flags & DISPLAY_DEVICE_MIRRORING_DRIVER.0) != 0;
        if !is_active || is_mirror {
            continue;
        }
        if !out.iter().any(|existing| existing.eq_ignore_ascii_case(trimmed)) {
            out.push(trimmed.to_owned());
        }
    }
    out
}

fn collect_generic_gpu_adapters() -> Vec<String> {
    let mut raw: Vec<(String, u32)> = Vec::new();
    let mut dev_num = 0u32;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dd.cb` is initialized and `lpDevice = None` enumerates display adapters.
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), dev_num, &mut dd, 0) }.as_bool();
        if !ok {
            break;
        }
        let model = utf16_slice_to_trimmed_string(&dd.DeviceString);
        raw.push((model, dd.StateFlags.0));
        dev_num += 1;
        if dev_num >= 32 {
            break;
        }
    }
    filter_gpu_adapter_names(raw)
}

/// Queries total and currently available physical system RAM in MiB via `GlobalMemoryStatusEx`.
pub fn query_system_memory_mb() -> Option<(u64, u64)> {
    let mut mem = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `mem.dwLength` is initialized to `size_of::<MEMORYSTATUSEX>()`.
    if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
        const MIB: u64 = 1024 * 1024;
        Some((mem.ullTotalPhys / MIB, mem.ullAvailPhys / MIB))
    } else {
        None
    }
}

/// Collects non-PII Windows OS version/build, logical CPU count, rounded RAM GB, and generic GPU names.
pub fn collect_os_env_info() -> OsEnvInfo {
    let nt_key = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let build_number = read_hklm_string(nt_key, w!("CurrentBuildNumber"))
        .or_else(|| read_hklm_string(nt_key, w!("CurrentBuild")))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let ubr = read_hklm_u32(nt_key, w!("UBR")).unwrap_or(0);
    let display_version = read_hklm_string(nt_key, w!("DisplayVersion"))
        .or_else(|| read_hklm_string(nt_key, w!("ReleaseId")))
        .unwrap_or_default();

    let os_name = if build_number >= WIN11_MIN_BUILD {
        "Windows 11".to_owned()
    } else if build_number > 0 {
        "Windows 10".to_owned()
    } else {
        "Windows".to_owned()
    };

    let logical_cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);

    let mut mem = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `mem.dwLength` is initialized to `size_of::<MEMORYSTATUSEX>()`.
    let total_ram_gb = if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
        let gib = (mem.ullTotalPhys as f64) / (1024.0 * 1024.0 * 1024.0);
        gib.round().max(1.0) as u32
    } else {
        0
    };

    let gpu_adapters = collect_generic_gpu_adapters();

    OsEnvInfo {
        os_name,
        build_number,
        ubr,
        display_version,
        arch: std::env::consts::ARCH.to_owned(),
        logical_cores,
        total_ram_gb,
        gpu_adapters,
    }
}

struct EnumMonitorRow {
    is_primary: bool,
    bounds: [i32; 4],
    work_area: [i32; 4],
    dpi_x: u32,
    dpi_y: u32,
    refresh_hz: u32,
}

unsafe extern "system" fn enum_monitor_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _lprc: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let list_ptr = lparam.0 as *mut Vec<EnumMonitorRow>;
    if list_ptr.is_null() {
        return BOOL(0);
    }

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info.monitorInfo.cbSize` is initialized to `size_of::<MONITORINFOEXW>()`.
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo) }.as_bool();
    if !ok {
        return BOOL(1);
    }

    let rc = info.monitorInfo.rcMonitor;
    let wa = info.monitorInfo.rcWork;
    let bounds = [
        rc.left,
        rc.top,
        (rc.right - rc.left).max(0),
        (rc.bottom - rc.top).max(0),
    ];
    let work_area = [
        wa.left,
        wa.top,
        (wa.right - wa.left).max(0),
        (wa.bottom - wa.top).max(0),
    ];
    let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: Querying effective DPI for valid `HMONITOR`.
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err()
        || dpi_x == 0
    {
        dpi_x = 96;
        dpi_y = 96;
    }

    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `info.szDevice` is NUL-terminated by `GetMonitorInfoW`; device name is used transiently
    // only to query refresh rate and is never stored or serialized.
    let refresh_hz = if unsafe {
        EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut devmode,
        )
    }
    .as_bool()
    {
        devmode.dmDisplayFrequency
    } else {
        0
    };

    // SAFETY: `list_ptr` points to a valid `Vec<EnumMonitorRow>` on the caller stack during `EnumDisplayMonitors`.
    let list = unsafe { &mut *list_ptr };
    list.push(EnumMonitorRow {
        is_primary,
        bounds,
        work_area,
        dpi_x,
        dpi_y,
        refresh_hz,
    });
    BOOL(1)
}

/// Pure helper computing rounded Windows display scaling percentage from DPI (`96 -> 100`).
pub fn dpi_to_scale_pct(dpi_x: u32) -> u32 {
    if dpi_x == 0 {
        return 100;
    }
    ((dpi_x as f64 / 96.0) * 100.0).round().max(50.0) as u32
}

/// Collects multi-monitor bounds, work areas, DPI scaling %, and refresh rates without serials/names.
pub fn collect_display_topology() -> DisplayTopology {
    ensure_dpi_awareness();

    let mut rows: Vec<EnumMonitorRow> = Vec::with_capacity(4);
    let ptr = (&mut rows as *mut Vec<EnumMonitorRow>) as isize;
    // SAFETY: `enum_monitor_cb` pushes into `rows` synchronously while `EnumDisplayMonitors` runs.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_monitor_cb), LPARAM(ptr));
    }

    // SAFETY: Plain `GetSystemMetrics` calls.
    let primary_screen = unsafe {
        [
            GetSystemMetrics(SM_CXSCREEN).max(0),
            GetSystemMetrics(SM_CYSCREEN).max(0),
        ]
    };

    let monitors: Vec<MonitorInfo> = rows
        .into_iter()
        .enumerate()
        .map(|(index, r)| MonitorInfo {
            index,
            is_primary: r.is_primary,
            bounds: r.bounds,
            work_area: r.work_area,
            dpi_x: r.dpi_x,
            dpi_y: r.dpi_y,
            scale_pct: dpi_to_scale_pct(r.dpi_x),
            refresh_hz: r.refresh_hz,
        })
        .collect();

    DisplayTopology {
        monitor_count: monitors.len().max(1),
        primary_screen,
        monitors,
    }
}

/// Pure heuristic checking whether a game window is borderless (no `WS_CAPTION` or client fills monitor)
/// and resolving which monitor contains the window center.
pub fn evaluate_game_window_diag(
    found: bool,
    is_foreground: bool,
    is_minimized: bool,
    window_style: Option<u32>,
    client_rect: Option<[i32; 4]>,
    window_rect: Option<[i32; 4]>,
    topology: &DisplayTopology,
) -> GameWindowDiag {
    let ref_rect = client_rect.or(window_rect);
    let mut monitor_index: Option<usize> = None;
    let mut monitor_scale_pct: Option<u32> = None;
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

    let no_caption = window_style.is_some_and(|st| (st & WS_CAPTION.0) == 0);
    let is_borderless_guess = found && !is_minimized && (no_caption || fills_monitor);

    GameWindowDiag {
        found,
        is_foreground,
        is_minimized,
        is_borderless_guess,
        client_rect,
        window_rect,
        monitor_index,
        monitor_scale_pct,
    }
}

/// Probes live Win32 state of the game window handle (strictly read-only).
pub fn probe_game_window_diag(
    hwnd: Option<isize>,
    client_rect: Option<[i32; 4]>,
    topology: &DisplayTopology,
) -> GameWindowDiag {
    let Some(raw_hwnd) = hwnd.filter(|&h| h > 0) else {
        return evaluate_game_window_diag(false, false, false, None, client_rect, None, topology);
    };
    let target = HWND(raw_hwnd as *mut c_void);
    // SAFETY: `IsWindow` validates `target`.
    if unsafe { !IsWindow(Some(target)).as_bool() } {
        return evaluate_game_window_diag(false, false, false, None, client_rect, None, topology);
    }

    // SAFETY: Read-only window metadata queries on validated `target` HWND.
    let (is_foreground, is_minimized, window_style, window_rect) = unsafe {
        let fg = GetForegroundWindow() == target;
        let min = IsIconic(target).as_bool();
        let style = GetWindowLongW(target, GWL_STYLE) as u32;
        let mut wr = RECT::default();
        let wrect = if GetWindowRect(target, &mut wr).is_ok() {
            Some([
                wr.left,
                wr.top,
                (wr.right - wr.left).max(0),
                (wr.bottom - wr.top).max(0),
            ])
        } else {
            None
        };
        (fg, min, Some(style), wrect)
    };

    evaluate_game_window_diag(
        true,
        is_foreground,
        is_minimized,
        window_style,
        client_rect,
        window_rect,
        topology,
    )
}

/// Opens Windows Explorer selecting a `.zip` / file (`/select,"<path>"`) or opening a directory.
/// Respects `GH_DESKTOP_SANDBOX` (returns `false` without launching UI in tests).
pub fn reveal_path_in_explorer(path: &Path) -> bool {
    if is_desktop_sandbox_env() {
        return false;
    }
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Strip Windows verbatim `\\?\` prefix because `explorer.exe /select,` fails on `\\?\` paths.
    let target_str = target.to_string_lossy();
    let clean_str = target_str.strip_prefix(r"\\?\").unwrap_or(&target_str);

    let mut cmd = Command::new("explorer.exe");
    if target.is_file() {
        let escaped = clean_str.replace('"', "");
        cmd.raw_arg(format!("/select,\"{escaped}\""));
    } else if target.is_dir() {
        cmd.arg(clean_str);
    } else {
        return false;
    }
    cmd.spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_env_and_display_topology_collect_valid_non_pii_telemetry() {
        let os = collect_os_env_info();
        assert!(!os.os_name.is_empty());
        assert!(os.logical_cores >= 1);
        assert!(os.total_ram_gb >= 1);
        assert_eq!(os.arch, std::env::consts::ARCH);

        let os_json = serde_json::to_string(&os).expect("serialize OsEnvInfo");
        let lower_os = os_json.to_ascii_lowercase();
        assert!(!lower_os.contains(r":\users\"));
        assert!(!lower_os.contains(r"\\.\display"));

        let topo = collect_display_topology();
        assert!(topo.monitor_count >= 1);
        assert!(!topo.monitors.is_empty());
        assert!(topo.primary_screen[0] > 0);
        assert!(topo.primary_screen[1] > 0);
        for m in &topo.monitors {
            assert!(m.scale_pct >= 50);
            assert!(m.bounds[2] > 0 && m.bounds[3] > 0);
        }

        let topo_json = serde_json::to_string(&topo).expect("serialize DisplayTopology");
        assert!(!topo_json.to_ascii_lowercase().contains("szdevice"));
        assert!(!topo_json.to_ascii_lowercase().contains(r"\\.\display"));
    }

    #[test]
    fn filter_gpu_adapters_keeps_active_non_mirror_and_deduplicates() {
        let adapters = filter_gpu_adapter_names([
            ("NVIDIA GeForce RTX 4070".to_owned(), DISPLAY_DEVICE_ACTIVE.0),
            ("NVIDIA GeForce RTX 4070".to_owned(), DISPLAY_DEVICE_ACTIVE.0),
            (
                "Microsoft Mirror Driver".to_owned(),
                DISPLAY_DEVICE_ACTIVE.0 | DISPLAY_DEVICE_MIRRORING_DRIVER.0,
            ),
            ("Inactive Adapter".to_owned(), 0),
            ("   ".to_owned(), DISPLAY_DEVICE_ACTIVE.0),
            ("AMD Radeon(TM) Graphics".to_owned(), DISPLAY_DEVICE_ACTIVE.0),
        ]);
        assert_eq!(
            adapters,
            vec![
                "NVIDIA GeForce RTX 4070".to_owned(),
                "AMD Radeon(TM) Graphics".to_owned()
            ]
        );
    }

    #[test]
    fn evaluate_game_window_diag_detects_borderless_and_monitor_scale() {
        let topo = DisplayTopology {
            monitor_count: 2,
            primary_screen: [2560, 1440],
            monitors: vec![
                MonitorInfo {
                    index: 0,
                    is_primary: true,
                    bounds: [0, 0, 2560, 1440],
                    work_area: [0, 0, 2560, 1392],
                    dpi_x: 120,
                    dpi_y: 120,
                    scale_pct: 125,
                    refresh_hz: 165,
                },
                MonitorInfo {
                    index: 1,
                    is_primary: false,
                    bounds: [2560, 0, 1920, 1080],
                    work_area: [2560, 0, 1920, 1040],
                    dpi_x: 96,
                    dpi_y: 96,
                    scale_pct: 100,
                    refresh_hz: 60,
                },
            ],
        };

        // Borderless fullscreen on monitor 0 (no WS_CAPTION)
        let borderless = evaluate_game_window_diag(
            true,
            true,
            false,
            Some(0x1000_0000),
            Some([0, 0, 2560, 1440]),
            Some([0, 0, 2560, 1440]),
            &topo,
        );
        assert!(borderless.is_borderless_guess);
        assert_eq!(borderless.monitor_index, Some(0));
        assert_eq!(borderless.monitor_scale_pct, Some(125));

        // Windowed with WS_CAPTION on monitor 1
        let windowed = evaluate_game_window_diag(
            true,
            false,
            false,
            Some(WS_CAPTION.0),
            Some([2700, 100, 1600, 900]),
            Some([2692, 69, 1616, 939]),
            &topo,
        );
        assert!(!windowed.is_borderless_guess);
        assert_eq!(windowed.monitor_index, Some(1));
        assert_eq!(windowed.monitor_scale_pct, Some(100));

        // Minimized -> not borderless guess
        let min_diag = evaluate_game_window_diag(
            true,
            false,
            true,
            Some(0),
            Some([0, 0, 2560, 1440]),
            None,
            &topo,
        );
        assert!(!min_diag.is_borderless_guess);
    }

    #[test]
    fn dpi_to_scale_pct_maps_standard_windows_scales() {
        assert_eq!(dpi_to_scale_pct(0), 100);
        assert_eq!(dpi_to_scale_pct(96), 100);
        assert_eq!(dpi_to_scale_pct(120), 125);
        assert_eq!(dpi_to_scale_pct(144), 150);
        assert_eq!(dpi_to_scale_pct(192), 200);
    }

    #[test]
    fn query_system_memory_mb_reports_valid_bounds() {
        let (total, avail) = query_system_memory_mb().expect("GlobalMemoryStatusEx");
        assert!(total >= 512, "unexpected total_mb={total}");
        assert!(avail <= total, "avail_mb={avail} > total_mb={total}");
    }
}
