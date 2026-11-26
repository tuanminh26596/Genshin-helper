//! Flicker-free GDI `BitBlt` capture (`src/capture.py`).
//!
//! DECISION: Always use plain `SRCCOPY` (`0x00CC_0020`), NEVER `CAPTUREBLT`, with a
//! thread-local / per-thread 4-slot LRU top-down 32-bit `CreateDIBSection` pool.
//! Zero `SendInput` anywhere in this module.

use crate::wgc_hdr::{
    MonitorHdrInfo, WgcHdrManager, WgcWindowCaptureOutcome, WgcWindowManager,
    probe_hdr_monitor_for_region,
};
use gh_core::frame::{FrameBuf, PixelFormat};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, Once, OnceLock, Weak};
use std::thread::ThreadId;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, ClientToScreen, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetWindowDC, HBITMAP, HDC,
    HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::StationsAndDesktops::{
    DESKTOP_CONTROL_FLAGS, GetThreadDesktop, GetUserObjectInformationW, HDESK, OpenDesktopW,
    SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{PROCESS_PER_MONITOR_DPI_AWARE, SetProcessDpiAwareness};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetClassNameW,
    GetClientRect, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowPlacement, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN, SW_RESTORE, SetForegroundWindow,
    SetProcessDPIAware, ShowWindow, WINDOWPLACEMENT,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

pub const BITBLT_ROP_SRCCOPY: u32 = SRCCOPY.0; // 0x00CC_0020 (never OR CAPTUREBLT)
pub const DIB_POOL_SLOTS: usize = 4;
const DESKTOP_ALL_ACCESS: u32 = 0x01FF;

/// Verbatim 10 non-covering window classes (`src/capture.py:252-263`).
pub const NON_COVERING_CLASSES: [&str; 10] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "ScreenClippingHost",
    "SnippingTool",
    "XamlExplorerHostIslandWindow",
    "MultitaskingViewFrame",
    "CEF-OSC-WIDGET",
    "NVOverlayWindow",
];

/// Verbatim 12 non-covering lowercase exe basenames (`src/capture.py:264-277`).
pub const NON_COVERING_PROCESSES: [&str; 12] = [
    "screenclippinghost.exe",
    "snippingtool.exe",
    "ms-screenclip.exe",
    "nvidia overlay.exe",
    "nvsphelper64.exe",
    "gamebar.exe",
    "sharex.exe",
    "lightshot.exe",
    "flameshot.exe",
    "greenshot.exe",
    "snagitcapture.exe",
    "picpick.exe",
];

/// Verbatim 15 browser lowercase exe basenames (`src/capture.py:278-294`).
pub const BROWSER_PROCESS_NAMES: [&str; 15] = [
    "chrome.exe",
    "msedge.exe",
    "brave.exe",
    "firefox.exe",
    "opera.exe",
    "vivaldi.exe",
    "browser.exe",
    "zen.exe",
    "arc.exe",
    "floorp.exe",
    "thorium.exe",
    "waterfox.exe",
    "coccoc.exe",
    "chromium.exe",
    "yandex.exe",
];

/// Verbatim 7 excluded browser title substrings (`src/capture.py:295-303`).
pub const BROWSER_TITLE_EXCLUDE: [&str; 7] = [
    "genshin impact",
    "code",
    "visual studio",
    "discord",
    "spotify",
    "powershell",
    "cmd.exe",
];

pub const GAME_IGNORED_TITLE_SUBSTRINGS: [&str; 6] = [
    "helper",
    "hoyolab sync",
    "visual studio",
    "code",
    "cmd.exe",
    "powershell",
];

pub const GAME_IGNORED_CLASSES: [&str; 2] =
    ["ConsoleWindowClass", "CASCADIA_HOSTING_WINDOW_CLASS"];

pub const BROWSER_KEYWORDS: [&str; 9] = [
    "hoyolab",
    "interactive map",
    "bản đồ tương tác",
    "genshin helper",
    "appsample",
    "yuanshen",
    "原神地图",
    "mapgenie",
    "gamedot",
];

pub const BROWSER_CLASSES: [&str; 2] = ["chrome_widgetwin_1", "mozillawindowclass"];

const BROWSER_FALLBACK_TITLE_NAMES: [&str; 6] =
    ["chrome", "edge", "firefox", "brave", "opera", "cốc cốc"];

static DPI_ONCE: Once = Once::new();

/// Ensures per-monitor DPI awareness is initialized once per process (`src/capture.py:126-141`).
pub fn ensure_dpi_awareness() {
    DPI_ONCE.call_once(|| {
        // SAFETY: Process-wide DPI awareness setup; fallback to legacy SetProcessDPIAware.
        unsafe {
            if SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE).is_err() {
                let _ = SetProcessDPIAware();
            }
        }
    });
}

static DEFAULT_DESKTOP_HANDLE: OnceLock<Option<isize>> = OnceLock::new();

thread_local! {
    static DESKTOP_ATTACHED: Cell<bool> = const { Cell::new(false) };
}

fn get_or_open_default_desktop() -> Option<HDESK> {
    let raw = *DEFAULT_DESKTOP_HANDLE.get_or_init(|| {
        let default_w: Vec<u16> = "Default\0".encode_utf16().collect();
        // SAFETY: Opening `"Default"` desktop once per process and retaining the single process-wide
        // handle avoids closing an active thread desktop on TLS teardown (`ERROR_BUSY` 170) and
        // eliminates per-thread `HDESK` kernel handle leaks.
        unsafe {
            OpenDesktopW(
                PCWSTR(default_w.as_ptr()),
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ALL_ACCESS,
            )
        }
        .ok()
        .filter(|h| !h.is_invalid())
        .map(|h| h.0 as isize)
    });
    raw.map(|ptr| HDESK(ptr as *mut c_void))
}

/// Ensures the calling thread is attached to the `"Default"` interactive desktop
/// (`src/capture.py:144-169`).
pub fn ensure_default_desktop() {
    DESKTOP_ATTACHED.with(|attached| {
        if attached.get() {
            return;
        }
        attached.set(true);

        // SAFETY: Querying current thread desktop object name.
        if let Ok(cur_desk) = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
            && !cur_desk.is_invalid()
        {
            let mut name_buf = [0u16; 128];
            let mut needed = 0u32;
            // SAFETY: `name_buf` is 256 bytes for `UOI_NAME`.
            let ok = unsafe {
                GetUserObjectInformationW(
                    HANDLE(cur_desk.0),
                    UOI_NAME,
                    Some(name_buf.as_mut_ptr().cast::<c_void>()),
                    (name_buf.len() * 2) as u32,
                    Some(&mut needed),
                )
            };
            if ok.is_ok() {
                let end = name_buf
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(name_buf.len());
                let name = String::from_utf16_lossy(&name_buf[..end]);
                if name.trim().eq_ignore_ascii_case("default") {
                    return;
                }
            }
        }

        if let Some(h_def) = get_or_open_default_desktop() {
            // SAFETY: Binding calling thread to the cached process-wide `"Default"` `HDESK`.
            unsafe {
                let _ = SetThreadDesktop(h_def);
            }
        }
    });
}

// ---------- Pure WindowFinder Logic (Deterministic & Unit-Testable) ----------

pub fn is_ignored_game_window(title: &str, class_name: &str) -> bool {
    if GAME_IGNORED_CLASSES.contains(&class_name) {
        return true;
    }
    let tl = title.to_lowercase();
    GAME_IGNORED_TITLE_SUBSTRINGS
        .iter()
        .any(|sub| tl.contains(sub))
}

pub fn matches_game_window(title: &str, class_name: &str, titles: &[&str]) -> bool {
    if is_ignored_game_window(title, class_name) {
        return false;
    }
    if class_name == "UnityWndClass" {
        return true;
    }
    let trimmed = title.trim();
    let tl = trimmed.to_lowercase();
    if titles
        .iter()
        .any(|t| *t == trimmed || t.to_lowercase() == tl)
    {
        return true;
    }
    titles.iter().any(|t| tl.contains(&t.to_lowercase())) && !tl.contains("hoyoplay")
}

pub fn is_non_covering_class(class_name: &str) -> bool {
    NON_COVERING_CLASSES.contains(&class_name)
}

pub fn is_non_covering_process(proc_name: &str) -> bool {
    let lower = proc_name.trim().to_ascii_lowercase();
    NON_COVERING_PROCESSES.contains(&lower.as_str())
}

pub fn is_browser_title_excluded(title_lower: &str) -> bool {
    BROWSER_TITLE_EXCLUDE
        .iter()
        .any(|bad| title_lower.contains(bad))
}

pub fn is_browser_window_from_meta(
    proc_name: &str,
    title_lower: &str,
    allow_unverified_title: bool,
) -> bool {
    if is_browser_title_excluded(title_lower) {
        return false;
    }
    let proc_lower = proc_name.trim().to_ascii_lowercase();
    if !proc_lower.is_empty() {
        return BROWSER_PROCESS_NAMES.contains(&proc_lower.as_str());
    }
    if allow_unverified_title {
        return true;
    }
    BROWSER_FALLBACK_TITLE_NAMES
        .iter()
        .any(|b| title_lower.contains(b))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserMatch {
    Specific,
    Generic,
}

fn classify_browser_window(
    raw_title: &str,
    raw_class: &str,
    proc_name: &str,
    specific_only: bool,
) -> Option<BrowserMatch> {
    let title = raw_title.to_lowercase();
    if title.is_empty() {
        return None;
    }
    if BROWSER_KEYWORDS.iter().any(|kw| title.contains(kw))
        && is_browser_window_from_meta(proc_name, &title, true)
    {
        return Some(BrowserMatch::Specific);
    }
    if specific_only {
        return None;
    }
    let cls = raw_class.to_ascii_lowercase();
    if (BROWSER_CLASSES.iter().any(|bc| cls.contains(bc))
        || BROWSER_FALLBACK_TITLE_NAMES
            .iter()
            .any(|bn| title.contains(bn)))
        && is_browser_window_from_meta(proc_name, &title, false)
    {
        return Some(BrowserMatch::Generic);
    }
    None
}

/// Pure geometry check for foreground occlusion (`src/capture.py:344-362`).
/// `game_rect = (left, top, width, height)`, `fg_rect = Some((left, top, right, bottom))`.
pub fn foreground_covers_game(
    game_rect: (i32, i32, i32, i32),
    fg_rect: Option<(i32, i32, i32, i32)>,
    threshold: f64,
) -> bool {
    let Some((fl, ft, fr, fb)) = fg_rect else {
        return false;
    };
    let (gl, gt, gw, gh) = game_rect;
    if gw <= 0 || gh <= 0 {
        return false;
    }
    let gl64 = gl as i64;
    let gt64 = gt as i64;
    let gr64 = gl64 + (gw as i64);
    let gb64 = gt64 + (gh as i64);
    let ix = (gr64.min(fr as i64) - gl64.max(fl as i64)).max(0);
    let iy = (gb64.min(fb as i64) - gt64.max(ft as i64)).max(0);
    let area = ix.saturating_mul(iy) as f64;
    let game_area = (gw as i64).saturating_mul(gh as i64) as f64;
    (area / game_area) > threshold
}

// ---------- Win32 WindowFinder ----------

fn to_hwnd(raw: isize) -> Option<HWND> {
    if raw == 0 {
        None
    } else {
        Some(HWND(raw as *mut c_void))
    }
}

fn get_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is valid UTF-16 output storage.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
}

fn get_window_text_trimmed(hwnd: HWND) -> String {
    // SAFETY: `GetWindowTextLengthW` queries length for `hwnd`.
    let approx = unsafe { GetWindowTextLengthW(hwnd) };
    let cap = (approx.max(0) as usize).saturating_add(2).max(256);
    let mut buf = vec![0u16; cap];
    // SAFETY: `buf` holds at least 256 UTF-16 code units.
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
        .trim()
        .to_owned()
}

fn get_hwnd_process_basename(hwnd: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: Queries PID owning `hwnd`.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return String::new();
    }
    // SAFETY: Minimal read-only process handle closed immediately after querying image path.
    let Ok(h_proc) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };

    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has capacity `len`.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            h_proc,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: Close owned handle immediately.
    unsafe {
        let _ = CloseHandle(h_proc);
    }

    if ok.is_err() || len == 0 {
        return String::new();
    }
    let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    let normalized = full.replace('/', "\\");
    normalized
        .rsplit('\\')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn get_rect_min(hwnd: HWND, min_w: i32, min_h: i32) -> Option<(i32, i32, i32, i32)> {
    let mut cr = RECT::default();
    // SAFETY: Validating client rect and converting origin to screen coordinates.
    if unsafe { GetClientRect(hwnd, &mut cr).is_ok() } {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { ClientToScreen(hwnd, &mut pt).as_bool() } {
            let cw = cr.right - cr.left;
            let ch = cr.bottom - cr.top;
            if cw >= min_w && ch >= min_h {
                return Some((pt.x, pt.y, cw, ch));
            }
        }
    }

    let mut wr = RECT::default();
    // SAFETY: Fallback to full window rect.
    if unsafe { GetWindowRect(hwnd, &mut wr).is_ok() } {
        let ww = wr.right - wr.left;
        let wh = wr.bottom - wr.top;
        if ww >= min_w && wh >= min_h {
            return Some((wr.left, wr.top, ww, wh));
        }
    }
    None
}

fn get_rect_window_only(hwnd: HWND, min_w: i32, min_h: i32) -> Option<(i32, i32, i32, i32)> {
    let mut wr = RECT::default();
    // SAFETY: Reads outer window rect.
    if unsafe { GetWindowRect(hwnd, &mut wr).is_ok() } {
        let ww = wr.right - wr.left;
        let wh = wr.bottom - wr.top;
        if ww >= min_w && wh >= min_h {
            return Some((wr.left, wr.top, ww, wh));
        }
    }
    None
}

fn normal_window_area(hwnd: HWND) -> i64 {
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `wp.length` initialized to struct size.
    if unsafe { GetWindowPlacement(hwnd, &mut wp).is_ok() } {
        let r = wp.rcNormalPosition;
        let w = (r.right - r.left).max(0) as i64;
        let h = (r.bottom - r.top).max(0) as i64;
        return w.saturating_mul(h);
    }
    let mut wr = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut wr).is_ok() } {
        let w = (wr.right - wr.left).max(0) as i64;
        let h = (wr.bottom - wr.top).max(0) as i64;
        return w.saturating_mul(h);
    }
    0
}

unsafe extern "system" fn enum_windows_collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if lparam.0 != 0 {
        // SAFETY: `lparam` points to caller-owned `Vec<HWND>` for duration of `EnumWindows`.
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        list.push(hwnd);
    }
    BOOL(1)
}

fn enumerate_top_level_windows() -> Vec<HWND> {
    let mut list = Vec::with_capacity(128);
    let ptr = (&mut list as *mut Vec<HWND>) as isize;
    // SAFETY: `enum_windows_collect` synchronously pushes into `list` while `EnumWindows` runs.
    unsafe {
        let _ = EnumWindows(Some(enum_windows_collect), LPARAM(ptr));
    }
    list
}

/// Win32 window discovery, foreground occlusion, and focus management (`src/capture.py:172-667`).
/// Strictly read-only against the game window — zero `SendInput`.
pub struct WindowFinder;

impl WindowFinder {
    pub fn get_game_window(
        titles: &[&str],
        allow_screen_fallback: bool,
    ) -> Option<(isize, i32, i32, i32, i32)> {
        ensure_default_desktop();

        // Step 1: Foreground fast-path
        // SAFETY: Querying current foreground window.
        let fg = unsafe { GetForegroundWindow() };
        if !fg.0.is_null() && unsafe { IsWindowVisible(fg).as_bool() && !IsIconic(fg).as_bool() } {
            let cls = get_class_name(fg);
            let title = get_window_text_trimmed(fg);
            let title_lower = title.to_lowercase();
            if !is_ignored_game_window(&title, &cls)
                && (cls == "UnityWndClass"
                    || titles
                        .iter()
                        .any(|t| *t == title || t.to_lowercase() == title_lower))
                && let Some((l, t, w, h)) = get_rect_min(fg, 1000, 600)
            {
                return Some((fg.0 as isize, l, t, w, h));
            }
        }

        // Step 2: Enumerate visible non-minimized windows, choose largest area
        let mut best: Option<(i64, isize, i32, i32, i32, i32)> = None;
        for hwnd in enumerate_top_level_windows() {
            // SAFETY: Validating visibility and non-minimized state.
            if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
                continue;
            }
            let cls = get_class_name(hwnd);
            let title = get_window_text_trimmed(hwnd);
            if !matches_game_window(&title, &cls, titles) {
                continue;
            }
            if let Some((l, t, w, h)) = get_rect_min(hwnd, 1000, 600) {
                let area = (w as i64).saturating_mul(h as i64);
                if best.as_ref().is_none_or(|b| area > b.0) {
                    best = Some((area, hwnd.0 as isize, l, t, w, h));
                }
            }
        }
        if let Some((_, hwnd, l, t, w, h)) = best {
            return Some((hwnd, l, t, w, h));
        }

        if allow_screen_fallback {
            let (sw, sh) = Self::get_screen_dimensions();
            return Some((0, 0, 0, sw, sh));
        }
        None
    }

    /// Mouse cursor hotspot in screen coordinates (read-only; `None` if the call fails, e.g. on
    /// a secure desktop).
    pub fn cursor_screen_pos() -> Option<(i32, i32)> {
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: `pt` is a valid, writable POINT for the duration of the call.
        unsafe { GetCursorPos(&mut pt) }.ok()?;
        Some((pt.x, pt.y))
    }

    pub fn is_window_foreground(hwnd: isize) -> bool {
        let Some(target) = to_hwnd(hwnd) else {
            return false;
        };
        // SAFETY: Comparing foreground HWND against `target`.
        unsafe { GetForegroundWindow() == target }
    }

    pub fn foreground_covers_game(
        game_rect: (i32, i32, i32, i32),
        fg_rect: Option<(i32, i32, i32, i32)>,
        threshold: f64,
    ) -> bool {
        foreground_covers_game(game_rect, fg_rect, threshold)
    }

    pub fn is_game_visible_unfocused(
        hwnd: isize,
        game_rect: (i32, i32, i32, i32),
        own_hwnds: &[isize],
        keep_when_unfocused: bool,
    ) -> bool {
        let _ = keep_when_unfocused;
        let Some(target) = to_hwnd(hwnd) else {
            return false;
        };
        // SAFETY: Validating target game window state.
        if unsafe {
            !IsWindow(Some(target)).as_bool()
                || IsIconic(target).as_bool()
                || !IsWindowVisible(target).as_bool()
        } {
            return false;
        }

        // SAFETY: Checking foreground window and whether it is own app/HUD or non-covering overlay.
        let fg = unsafe { GetForegroundWindow() };
        if fg.0.is_null() || fg == target || own_hwnds.contains(&(fg.0 as isize)) {
            return true;
        }

        let fg_cls = get_class_name(fg);
        if is_non_covering_class(&fg_cls) {
            return true;
        }
        let fg_proc = get_hwnd_process_basename(fg);
        if is_non_covering_process(&fg_proc) {
            return true;
        }
        // SAFETY: Checking minimized foreground window.
        if unsafe { IsIconic(fg).as_bool() } {
            return true;
        }

        let mut wr = RECT::default();
        // SAFETY: Querying foreground window screen bounds.
        if unsafe { GetWindowRect(fg, &mut wr).is_err() } {
            return true;
        }
        !foreground_covers_game(game_rect, Some((wr.left, wr.top, wr.right, wr.bottom)), 0.10)
    }

    pub fn get_screen_dimensions() -> (i32, i32) {
        // SAFETY: Plain `GetSystemMetrics` screen dimension queries.
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }

    pub fn get_monitor_count() -> usize {
        // SAFETY: Plain `GetSystemMetrics(SM_CMONITORS)` query.
        let count = unsafe { GetSystemMetrics(SM_CMONITORS) };
        count.max(1) as usize
    }

    pub fn get_browser_window() -> Option<(isize, i32, i32, i32, i32)> {
        let mut specific: Option<(i64, isize, i32, i32, i32, i32)> = None;
        let mut generic: Option<(i64, isize, i32, i32, i32, i32)> = None;

        for hwnd in enumerate_top_level_windows() {
            // SAFETY: Enumerating visible non-minimized browser windows.
            if unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() } {
                continue;
            }
            let title = get_window_text_trimmed(hwnd);
            if title.is_empty() {
                continue;
            }
            let rect = match get_rect_min(hwnd, 1000, 600) {
                Some(r) if r.2 >= 500 && r.3 >= 400 => r,
                _ => match get_rect_window_only(hwnd, 500, 400) {
                    Some(r) => r,
                    None => continue,
                },
            };

            let cls = get_class_name(hwnd);
            let proc_name = get_hwnd_process_basename(hwnd);
            let area = (rect.2 as i64).saturating_mul(rect.3 as i64);

            match classify_browser_window(&title, &cls, &proc_name, false) {
                Some(BrowserMatch::Specific) if specific.as_ref().is_none_or(|b| area > b.0) => {
                    specific = Some((area, hwnd.0 as isize, rect.0, rect.1, rect.2, rect.3));
                }
                Some(BrowserMatch::Generic) if generic.as_ref().is_none_or(|b| area > b.0) => {
                    generic = Some((area, hwnd.0 as isize, rect.0, rect.1, rect.2, rect.3));
                }
                _ => {}
            }
        }

        if let Some((_, hwnd, l, t, w, h)) = specific {
            return Some((hwnd, l, t, w, h));
        }
        if let Some((_, hwnd, l, t, w, h)) = generic {
            return Some((hwnd, l, t, w, h));
        }
        None
    }

    pub fn find_game_window_any_state(titles: &[&str]) -> Option<isize> {
        if let Some((hwnd, _, _, _, _)) = Self::get_game_window(titles, false) {
            return Some(hwnd);
        }
        ensure_default_desktop();

        let mut best: Option<(i64, isize)> = None;
        for hwnd in enumerate_top_level_windows() {
            // SAFETY: Includes minimized windows (`IsIconic`) as long as `IsWindowVisible` holds.
            if unsafe { !IsWindowVisible(hwnd).as_bool() } {
                continue;
            }
            let cls = get_class_name(hwnd);
            let title = get_window_text_trimmed(hwnd);
            if !matches_game_window(&title, &cls, titles) {
                continue;
            }
            let area = normal_window_area(hwnd);
            if best.as_ref().is_none_or(|b| area > b.0) {
                best = Some((area, hwnd.0 as isize));
            }
        }
        best.map(|(_, hwnd)| hwnd)
    }

    pub fn find_browser_window_any_state(specific_only: bool) -> Option<isize> {
        if let Some((hwnd, _, _, _, _)) = Self::get_browser_window() {
            if !specific_only {
                return Some(hwnd);
            }
            if let Some(hw) = to_hwnd(hwnd) {
                let tl = get_window_text_trimmed(hw).to_lowercase();
                if BROWSER_KEYWORDS.iter().any(|kw| tl.contains(kw)) {
                    return Some(hwnd);
                }
            }
        }

        ensure_default_desktop();

        let mut specific: Option<(i64, isize)> = None;
        let mut generic: Option<(i64, isize)> = None;
        for hwnd in enumerate_top_level_windows() {
            // SAFETY: Includes minimized windows.
            if unsafe { !IsWindowVisible(hwnd).as_bool() } {
                continue;
            }
            let title = get_window_text_trimmed(hwnd);
            if title.is_empty() {
                continue;
            }
            let area = normal_window_area(hwnd);
            if area < (500 * 400) {
                continue;
            }
            let cls = get_class_name(hwnd);
            let proc_name = get_hwnd_process_basename(hwnd);

            match classify_browser_window(&title, &cls, &proc_name, specific_only) {
                Some(BrowserMatch::Specific) if specific.as_ref().is_none_or(|b| area > b.0) => {
                    specific = Some((area, hwnd.0 as isize));
                }
                Some(BrowserMatch::Generic) if generic.as_ref().is_none_or(|b| area > b.0) => {
                    generic = Some((area, hwnd.0 as isize));
                }
                _ => {}
            }
        }

        specific.or(generic).map(|(_, hwnd)| hwnd)
    }

    /// Focuses `hwnd` via `AllowSetForegroundWindow(ASFW_ANY)` and `AttachThreadInput`
    /// without injecting any synthetic keyboard/mouse events (zero `SendInput`).
    pub fn focus_window(hwnd: isize) -> bool {
        let Some(target) = to_hwnd(hwnd) else {
            return false;
        };
        // SAFETY: Validating target HWND.
        if unsafe { !IsWindow(Some(target)).as_bool() } {
            return false;
        }
        ensure_default_desktop();

        // SAFETY: Restore if minimized, allow foreground transfer, attempt direct focus.
        unsafe {
            if IsIconic(target).as_bool() {
                let _ = ShowWindow(target, SW_RESTORE);
            }
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = SetForegroundWindow(target);
            if GetForegroundWindow() == target {
                return true;
            }
        }

        // SAFETY: AttachThreadInput fallback with deterministic RAII/cleanup detach in reverse order.
        unsafe {
            let fg = GetForegroundWindow();
            let cur_tid = GetCurrentThreadId();
            let fg_tid = if fg.0.is_null() {
                0
            } else {
                GetWindowThreadProcessId(fg, None)
            };
            let target_tid = GetWindowThreadProcessId(target, None);

            let mut attached: Vec<(u32, u32)> = Vec::with_capacity(3);
            if fg_tid != 0 && fg_tid != cur_tid && AttachThreadInput(cur_tid, fg_tid, true).as_bool()
            {
                attached.push((cur_tid, fg_tid));
            }
            if target_tid != 0
                && target_tid != cur_tid
                && target_tid != fg_tid
                && AttachThreadInput(cur_tid, target_tid, true).as_bool()
            {
                attached.push((cur_tid, target_tid));
            }
            if fg_tid != 0
                && target_tid != 0
                && fg_tid != target_tid
                && AttachThreadInput(fg_tid, target_tid, true).as_bool()
            {
                attached.push((fg_tid, target_tid));
            }

            let _ = BringWindowToTop(target);
            let _ = SetForegroundWindow(target);

            for (a, b) in attached.into_iter().rev() {
                let _ = AttachThreadInput(a, b, false);
            }

            GetForegroundWindow() == target
        }
    }
}

// ---------- 4-Slot LRU GDI DIB Pool & ScreenCapturer ----------

struct DibSlot {
    dib: HBITMAP,
    bits: *mut u8,
    byte_len: usize,
}

struct GdiSession {
    max_slots: usize,
    srcdc: HDC,
    memdc: HDC,
    slots: Vec<((i32, i32), DibSlot)>,
    active_size: Option<(i32, i32)>,
    default_bitmap: Option<HGDIOBJ>,
    dib_alloc_count: usize,
    closed: bool,
}

// SAFETY: Each `GdiSession` is keyed by `ThreadId` and guarded by its own `Mutex<GdiSession>`;
// only `close()`/`Drop` on `ScreenCapturer` tears down sessions across threads.
unsafe impl Send for GdiSession {}

impl GdiSession {
    fn new(max_slots: usize) -> Option<Self> {
        let max_slots = max_slots.max(1);
        // SAFETY: Acquire full virtual desktop DC and compatible memory DC.
        let srcdc = unsafe { GetWindowDC(None) };
        if srcdc.is_invalid() {
            return None;
        }
        let memdc = unsafe { CreateCompatibleDC(Some(srcdc)) };
        if memdc.is_invalid() {
            unsafe {
                let _ = ReleaseDC(None, srcdc);
            }
            return None;
        }
        Some(Self {
            max_slots,
            srcdc,
            memdc,
            slots: Vec::with_capacity(max_slots),
            active_size: None,
            default_bitmap: None,
            dib_alloc_count: 0,
            closed: false,
        })
    }

    fn evict_lru_slot(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        let (evict_key, evict_slot) = self.slots.remove(0);
        if self.active_size == Some(evict_key) {
            if let Some(default_bmp) = self.default_bitmap {
                // SAFETY: Deselect DIB from `memdc` BEFORE `DeleteObject`.
                unsafe {
                    SelectObject(self.memdc, default_bmp);
                }
            }
            self.active_size = None;
        }
        // SAFETY: `evict_slot.dib` is no longer selected into `memdc`.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(evict_slot.dib.0));
        }
    }

    fn ensure_dib(&mut self, width: i32, height: i32) -> Option<usize> {
        if self.closed || width <= 0 || height <= 0 {
            return None;
        }
        let key = (width, height);
        if let Some(pos) = self.slots.iter().position(|(k, _)| *k == key) {
            let entry = self.slots.remove(pos);
            let dib = entry.1.dib;
            self.slots.push(entry);
            if self.active_size != Some(key) {
                // SAFETY: Select cached DIB into `memdc`.
                let prev = unsafe { SelectObject(self.memdc, HGDIOBJ(dib.0)) };
                if prev.is_invalid() {
                    return None;
                }
                self.active_size = Some(key);
            }
            return Some(self.slots.len() - 1);
        }

        while self.slots.len() >= self.max_slots {
            self.evict_lru_slot();
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height => top-down DIB matching FrameBuf row order.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };

        let mut bits_ptr: *mut c_void = null_mut();
        // SAFETY: Creating 32-bit top-down DIBSection on `memdc`.
        let dib = unsafe {
            CreateDIBSection(
                Some(self.memdc),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                None,
                0,
            )
        }
        .ok()?;
        if dib.is_invalid() || bits_ptr.is_null() {
            if !dib.is_invalid() {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(dib.0));
                }
            }
            return None;
        }

        // SAFETY: Selecting newly created DIB into `memdc` and saving default bitmap once.
        let prev = unsafe { SelectObject(self.memdc, HGDIOBJ(dib.0)) };
        if prev.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(dib.0));
            }
            return None;
        }
        if self.default_bitmap.is_none() {
            self.default_bitmap = Some(prev);
        }

        let byte_len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        self.active_size = Some(key);
        self.dib_alloc_count += 1;
        self.slots.push((
            key,
            DibSlot {
                dib,
                bits: bits_ptr.cast::<u8>(),
                byte_len,
            },
        ));
        Some(self.slots.len() - 1)
    }

    fn bitblt_into_slot(&mut self, left: i32, top: i32, width: i32, height: i32) -> Option<usize> {
        let idx = self.ensure_dib(width, height)?;
        // SAFETY: Strictly plain `SRCCOPY` (`0x00CC_0020`), NEVER `CAPTUREBLT`.
        let ok = unsafe {
            BitBlt(
                self.memdc,
                0,
                0,
                width,
                height,
                Some(self.srcdc),
                left,
                top,
                SRCCOPY,
            )
        };
        if ok.is_err() {
            return None;
        }
        // SAFETY: Ensure DIB backing buffer reflects completed BitBlt before reading bytes.
        unsafe {
            let _ = GdiFlush();
        }
        Some(idx)
    }

    fn grab_bgr(&mut self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf> {
        let idx = self.bitblt_into_slot(left, top, width, height)?;
        let slot = &self.slots[idx].1;
        // SAFETY: `slot.bits` points to `slot.byte_len` valid DIB bytes owned by `self`.
        let bgra = unsafe { std::slice::from_raw_parts(slot.bits, slot.byte_len) };
        let w = width as u32;
        let h = height as u32;
        let mut out = FrameBuf::new_zeroed(w, h, PixelFormat::Bgr8);
        for (src_px, dst_px) in bgra
            .as_chunks::<4>()
            .0
            .iter()
            .zip(out.data.as_chunks_mut::<3>().0.iter_mut())
        {
            dst_px[0] = src_px[0];
            dst_px[1] = src_px[1];
            dst_px[2] = src_px[2];
        }
        Some(out)
    }

    fn grab_bgra(&mut self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf> {
        let idx = self.bitblt_into_slot(left, top, width, height)?;
        let slot = &self.slots[idx].1;
        // SAFETY: `slot.bits` points to `slot.byte_len` valid DIB bytes owned by `self`.
        let bgra = unsafe { std::slice::from_raw_parts(slot.bits, slot.byte_len) };
        let w = width as u32;
        let h = height as u32;
        let mut out = FrameBuf::new_zeroed(w, h, PixelFormat::Bgra8);
        out.data.copy_from_slice(bgra);
        Some(out)
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if !self.memdc.is_invalid()
            && let Some(default_bmp) = self.default_bitmap.take()
        {
            // SAFETY: Restore original 1x1 stock bitmap before deleting any DIBs.
            unsafe {
                SelectObject(self.memdc, default_bmp);
            }
        }
        self.active_size = None;
        for (_, slot) in self.slots.drain(..) {
            if !slot.dib.is_invalid() {
                // SAFETY: `slot.dib` is deselected and deleted once.
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(slot.dib.0));
                }
            }
        }
        if !self.memdc.is_invalid() {
            // SAFETY: Delete compatible memory DC.
            unsafe {
                let _ = DeleteDC(self.memdc);
            }
            self.memdc = HDC::default();
        }
        if !self.srcdc.is_invalid() {
            // SAFETY: Release screen window DC.
            unsafe {
                let _ = ReleaseDC(None, self.srcdc);
            }
            self.srcdc = HDC::default();
        }
    }
}

impl Drop for GdiSession {
    fn drop(&mut self) {
        self.close();
    }
}

type SessionRegistry = Mutex<HashMap<ThreadId, Arc<Mutex<GdiSession>>>>;

struct ThreadSessionCleanupGuard {
    tid: ThreadId,
    registries: Vec<Weak<SessionRegistry>>,
}

impl Drop for ThreadSessionCleanupGuard {
    fn drop(&mut self) {
        for weak in self.registries.drain(..) {
            let Some(sessions) = weak.upgrade() else {
                continue;
            };
            let removed = {
                let mut map = sessions.lock().unwrap_or_else(|e| e.into_inner());
                map.remove(&self.tid)
            };
            if let Some(sess_arc) = removed {
                let mut sess = sess_arc.lock().unwrap_or_else(|e| e.into_inner());
                sess.close();
            }
        }
    }
}

thread_local! {
    static CAPTURE_SESSION_CLEANUP: RefCell<Option<ThreadSessionCleanupGuard>> =
        const { RefCell::new(None) };
}

fn register_session_cleanup(tid: ThreadId, sessions: &Arc<SessionRegistry>) {
    let weak = Arc::downgrade(sessions);
    let _ = CAPTURE_SESSION_CLEANUP.try_with(|cell| {
        let mut slot = cell.borrow_mut();
        let guard = slot.get_or_insert_with(|| ThreadSessionCleanupGuard {
            tid,
            registries: Vec::new(),
        });
        guard.registries.retain(|w| w.strong_count() > 0);
        if !guard.registries.iter().any(|w| Weak::ptr_eq(w, &weak)) {
            guard.registries.push(weak);
        }
    });
}

/// Flicker-free multi-thread GDI `ScreenCapturer` with automatic DXGI/WGC HDR tonemapping
/// (`src/capture.py:841-917` + `P0.2` native WGC `R16G16B16A16Float` -> SDR `Bgra8`/`Bgr8`).
pub struct ScreenCapturer {
    max_dib_slots: usize,
    sessions: Arc<SessionRegistry>,
    wgc_hdr: WgcHdrManager,
    wgc_window: WgcWindowManager,
    hdr_wgc_enabled: bool,
}

impl Default for ScreenCapturer {
    fn default() -> Self {
        Self::new(DIB_POOL_SLOTS)
    }
}

impl ScreenCapturer {
    pub const BITBLT_ROP: u32 = BITBLT_ROP_SRCCOPY;

    pub fn new(max_dib_slots: usize) -> Self {
        ensure_dpi_awareness();
        Self {
            max_dib_slots: max_dib_slots.max(1),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            wgc_hdr: WgcHdrManager::new(),
            wgc_window: WgcWindowManager::new(),
            hdr_wgc_enabled: true,
        }
    }

    /// Enables or disables automatic DXGI HDR detection + WGC `R16G16B16A16Float` tonemapping.
    /// Enabled by default; disabling forces the plain GDI `SRCCOPY` capture path.
    pub fn with_hdr_wgc(mut self, enabled: bool) -> Self {
        self.hdr_wgc_enabled = enabled;
        self
    }

    /// Returns active HDR monitor metadata if the requested region lies wholly on an HDR monitor.
    pub fn active_hdr_monitor_info(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<MonitorHdrInfo> {
        if !self.hdr_wgc_enabled {
            return None;
        }
        probe_hdr_monitor_for_region(left, top, width, height)
    }

    /// Returns true when the requested screen region will be captured via WGC HDR tonemapping.
    pub fn is_region_hdr(&self, left: i32, top: i32, width: i32, height: i32) -> bool {
        self.active_hdr_monitor_info(left, top, width, height).is_some()
    }

    fn get_or_create_session(&self, tid: ThreadId) -> Option<Arc<Mutex<GdiSession>>> {
        ensure_default_desktop();
        let existing = {
            let map = self.sessions.lock().ok()?;
            map.get(&tid).cloned()
        };
        if let Some(existing) = existing {
            let needs_recreate = existing.lock().map(|g| g.closed).unwrap_or(true);
            if !needs_recreate {
                return Some(existing);
            }
        }
        let session = GdiSession::new(self.max_dib_slots)?;
        let arc = Arc::new(Mutex::new(session));
        {
            let mut map = self.sessions.lock().ok()?;
            map.insert(tid, Arc::clone(&arc));
        }
        register_session_cleanup(tid, &self.sessions);
        Some(arc)
    }

    fn capture_with_recovery(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
        bgra: bool,
    ) -> Option<FrameBuf> {
        if width <= 0 || height <= 0 {
            return None;
        }
        if self.hdr_wgc_enabled
            && let Some(frame) = self
                .wgc_hdr
                .try_capture_hdr_region(left, top, width, height, bgra)
        {
            return Some(frame);
        }
        let tid = std::thread::current().id();
        let session_arc = self.get_or_create_session(tid)?;
        let captured = match session_arc.lock() {
            Ok(mut session) => {
                if bgra {
                    session.grab_bgra(left, top, width, height)
                } else {
                    session.grab_bgr(left, top, width, height)
                }
            }
            Err(_) => None,
        };
        if captured.is_some() {
            return captured;
        }

        // Recovery: drop broken thread session so next capture starts fresh.
        if let Ok(mut map) = self.sessions.lock() {
            map.remove(&tid);
        }
        if let Ok(mut session) = session_arc.lock() {
            session.close();
        }
        None
    }

    pub fn capture_region(&self, left: i32, top: i32, width: i32, height: i32) -> Option<FrameBuf> {
        self.capture_with_recovery(left, top, width, height, false)
    }

    pub fn capture_region_bgra(
        &self,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        self.capture_with_recovery(left, top, width, height, true)
    }

    /// Captures the `(width, height)` client area directly from `hwnd` via borderless WGC
    /// (`CreateForWindow`), excluding all topmost overlays (`WS_EX_LAYERED` HUDs / M-Map route
    /// overlay even when set to `WDA_NONE` for OBS/ShadowPlay recording).
    /// Non-blocking while DWM delivers frame 1 (`WarmingUp`).
    pub fn capture_window_client_outcome(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> WgcWindowCaptureOutcome {
        self.wgc_window
            .try_capture_window_client_outcome(hwnd, left, top, width, height, false)
    }

    /// Convenience wrapper returning `Some(frame)` only when an overlay-free WGC window frame is ready.
    pub fn capture_window_client(
        &self,
        hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        self.wgc_window
            .try_capture_window_client(hwnd, left, top, width, height, false)
    }

    /// Returns a diagnostic status string for the per-window WGC capture manager.
    pub fn window_capture_status(&self) -> String {
        self.wgc_window.status_summary()
    }

    /// Releases any active per-window WGC capture session (zero idle GPU/DWM overhead when M-Map
    /// is closed or capture-visible overlay mode is off).
    pub fn release_window_capture(&self) {
        self.wgc_window.close();
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn active_thread_dib_alloc_count(&self) -> usize {
        let tid = std::thread::current().id();
        let Ok(map) = self.sessions.lock() else {
            return 0;
        };
        let Some(sess) = map.get(&tid) else {
            return 0;
        };
        sess.lock().map(|g| g.dib_alloc_count).unwrap_or(0)
    }

    pub fn active_thread_slot_sizes(&self) -> Vec<(i32, i32)> {
        let tid = std::thread::current().id();
        let Ok(map) = self.sessions.lock() else {
            return Vec::new();
        };
        let Some(sess) = map.get(&tid) else {
            return Vec::new();
        };
        sess.lock()
            .map(|g| g.slots.iter().map(|(k, _)| *k).collect())
            .unwrap_or_default()
    }

    pub fn close(&self) {
        self.wgc_hdr.close();
        self.wgc_window.close();
        let drained: Vec<Arc<Mutex<GdiSession>>> = match self.sessions.lock() {
            Ok(mut map) => map.drain().map(|(_, v)| v).collect(),
            Err(_) => Vec::new(),
        };
        for sess in drained {
            if let Ok(mut guard) = sess.lock() {
                guard.close();
            }
        }
    }
}

impl Drop for ScreenCapturer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
    };

    #[test]
    fn bitblt_rop_never_uses_captureblt() {
        assert_eq!(BITBLT_ROP_SRCCOPY, 0x00CC_0020);
        assert_eq!(ScreenCapturer::BITBLT_ROP, 0x00CC_0020);
        assert_eq!(BITBLT_ROP_SRCCOPY & 0x4000_0000, 0);
        assert_eq!(DIB_POOL_SLOTS, 4);
    }

    #[test]
    fn screen_capturer_bgr_bgra_independence_negative_coords_and_multithread() {
        // Test GDI session lifecycle deterministically even on HDR-enabled dev monitors.
        let cap = Arc::new(ScreenCapturer::new(DIB_POOL_SLOTS).with_hdr_wgc(false));

        let mut f1 = cap
            .capture_region(0, 0, 320, 180)
            .expect("capture 320x180 bgr");
        assert_eq!(f1.width, 320);
        assert_eq!(f1.height, 180);
        assert_eq!(f1.format, PixelFormat::Bgr8);
        assert_eq!(f1.data.len(), 320 * 180 * 3);

        let f2 = cap
            .capture_region(0, 0, 320, 180)
            .expect("second capture 320x180 bgr");
        let f2_snapshot = f2.data.clone();
        f1.data.fill(0);
        assert_eq!(f2.data, f2_snapshot, "mutating f1 must not mutate f2");

        // Dynamic resize
        let f3 = cap
            .capture_region(0, 0, 640, 360)
            .expect("capture 640x360 bgr");
        assert_eq!(f3.width, 640);
        assert_eq!(f3.height, 360);
        assert_eq!(f3.data.len(), 640 * 360 * 3);

        // Invalid dimensions return None
        assert!(cap.capture_region(0, 0, 0, 100).is_none());
        assert!(cap.capture_region(0, 0, 100, -5).is_none());
        assert!(cap.capture_region_bgra(0, 0, 0, 10).is_none());
        assert!(cap.capture_region_bgra(0, 0, 10, -1).is_none());

        // Negative virtual desktop coordinates succeed cleanly
        let neg = cap
            .capture_region(-8, 0, 64, 64)
            .expect("negative virtual desktop coords");
        assert_eq!(neg.width, 64);
        assert_eq!(neg.height, 64);

        // Worker thread creates a second GDI session; join() waits for OS thread exit & TLS drop,
        // and close() clears the remaining main-thread session.
        let cap_worker = Arc::clone(&cap);
        std::thread::spawn(move || {
            let wf = cap_worker
                .capture_region_bgra(0, 0, 96, 96)
                .expect("worker thread capture");
            assert_eq!(wf.format, PixelFormat::Bgra8);
            assert_eq!(wf.data.len(), 96 * 96 * 4);
            assert!(cap_worker.session_count() >= 2);
        })
        .join()
        .expect("worker thread");
        assert_eq!(cap.session_count(), 1);
        cap.close();
        assert_eq!(cap.session_count(), 0);
    }

    #[test]
    fn dead_worker_threads_release_gdi_sessions_and_handles() {
        let warmup = Arc::new(ScreenCapturer::new(DIB_POOL_SLOTS).with_hdr_wgc(false));
        let warmup_worker = Arc::clone(&warmup);
        std::thread::spawn(move || {
            let _ = warmup_worker.capture_region(0, 0, 64, 64);
        })
        .join()
        .expect("warmup worker thread");
        warmup.close();

        // SAFETY: Querying process GDI handle count after one-time warmup.
        let proc_h = unsafe { GetCurrentProcess() };
        let gdi_before = unsafe { GetGuiResources(proc_h, GR_GDIOBJECTS) };

        let cap = Arc::new(ScreenCapturer::new(DIB_POOL_SLOTS).with_hdr_wgc(false));
        for _ in 0..8 {
            let cap_worker = Arc::clone(&cap);
            std::thread::spawn(move || {
                let f = cap_worker
                    .capture_region(0, 0, 160, 90)
                    .expect("worker capture 160x90");
                assert_eq!(f.width, 160);
                assert_eq!(f.height, 90);
                assert_eq!(cap_worker.session_count(), 1);
            })
            .join()
            .expect("worker thread");
        }
        assert_eq!(cap.session_count(), 0);

        let gdi_after = unsafe { GetGuiResources(proc_h, GR_GDIOBJECTS) };
        assert_eq!(
            gdi_after, gdi_before,
            "Dead worker threads leaked GDI objects: before={gdi_before}, after={gdi_after}"
        );
    }

    #[test]
    fn dib_pool_4_slot_lru_eviction_and_zero_gdi_leak() {
        // Warm up once before measuring baseline handles (explicit GDI pool test).
        let warmup = ScreenCapturer::new(DIB_POOL_SLOTS).with_hdr_wgc(false);
        let _ = warmup.capture_region(0, 0, 64, 64);
        warmup.close();

        // SAFETY: Querying process GDI & kernel handle counts on current process.
        let proc_h = unsafe { GetCurrentProcess() };
        let gdi_before = unsafe { GetGuiResources(proc_h, GR_GDIOBJECTS) };
        let mut kern_before = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(proc_h, &mut kern_before);
        }

        {
            let cap = ScreenCapturer::new(4).with_hdr_wgc(false);
            // Alternate 3 sizes 15x across BGR and BGRA => exactly 3 DIB allocations.
            for idx in 0..15 {
                let _ = cap.capture_region(0, 0, 218, 218).expect("218x218");
                let _ = cap.capture_region_bgra(0, 0, 96, 96).expect("96x96");
                if idx % 2 == 0 {
                    let _ = cap.capture_region(0, 0, 640, 360).expect("640x360");
                } else {
                    let _ = cap.capture_region_bgra(0, 0, 640, 360).expect("640x360 bgra");
                }
            }
            assert_eq!(cap.active_thread_dib_alloc_count(), 3);
            assert_eq!(cap.active_thread_slot_sizes().len(), 3);

            // BGRA buffer independence on (640, 360) keeps (96, 96) older than (640, 360)
            let mut b1 = cap.capture_region_bgra(0, 0, 640, 360).expect("b1");
            let b2 = cap.capture_region_bgra(0, 0, 640, 360).expect("b2");
            let b2_copy = b2.data.clone();
            b1.data.fill(0xAA);
            assert_eq!(b2.data, b2_copy);

            // Touch (218, 218) so (96, 96) becomes LRU
            let _ = cap.capture_region(0, 0, 218, 218).expect("touch 218");
            // 4th distinct size fits without eviction
            let _ = cap.capture_region(0, 0, 320, 180).expect("320x180");
            assert_eq!(cap.active_thread_dib_alloc_count(), 4);
            assert_eq!(cap.active_thread_slot_sizes().len(), 4);

            // 5th distinct size evicts LRU (96, 96), keeping (218, 218)
            let _ = cap.capture_region(0, 0, 160, 120).expect("160x120");
            assert_eq!(cap.active_thread_dib_alloc_count(), 5);
            let slots = cap.active_thread_slot_sizes();
            assert_eq!(slots.len(), 4);
            assert!(!slots.contains(&(96, 96)), "LRU (96, 96) should be evicted");
            assert!(slots.contains(&(218, 218)), "(218, 218) should remain cached");
            cap.close();
        }

        // max_slots = 1 evicts the currently active selected DIB cleanly
        {
            let cap1 = ScreenCapturer::new(1).with_hdr_wgc(false);
            let _ = cap1.capture_region(0, 0, 100, 100).expect("100x100");
            let _ = cap1.capture_region(0, 0, 120, 120).expect("120x120");
            assert_eq!(cap1.active_thread_dib_alloc_count(), 2);
            assert_eq!(cap1.active_thread_slot_sizes(), vec![(120, 120)]);
            cap1.close();
        }

        let gdi_after = unsafe { GetGuiResources(proc_h, GR_GDIOBJECTS) };
        let mut kern_after = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(proc_h, &mut kern_after);
        }
        assert_eq!(
            gdi_after, gdi_before,
            "GDI objects leaked: before={gdi_before}, after={gdi_after}"
        );
        assert!(
            kern_after <= kern_before + 2,
            "Kernel handle leak: before={kern_before}, after={kern_after}"
        );
    }

    #[test]
    fn window_finder_constants_occlusion_and_win32_smoke() {
        assert_eq!(NON_COVERING_CLASSES.len(), 10);
        assert_eq!(NON_COVERING_PROCESSES.len(), 12);
        assert_eq!(BROWSER_PROCESS_NAMES.len(), 15);
        assert_eq!(BROWSER_TITLE_EXCLUDE.len(), 7);

        for cls in [
            "Shell_TrayWnd",
            "ScreenClippingHost",
            "SnippingTool",
            "CEF-OSC-WIDGET",
            "NVOverlayWindow",
        ] {
            assert!(is_non_covering_class(cls), "missing class {cls}");
        }
        for exe in ["screenclippinghost.exe", "nvidia overlay.exe", "gamebar.exe"] {
            assert!(is_non_covering_process(exe), "missing exe {exe}");
        }

        // Geometry parity from tests/verify_nav_ux_v54.py:429-444
        assert!(!foreground_covers_game(
            (0, 0, 1920, 1080),
            Some((1912, -8, 3848, 1048)),
            0.10
        ));
        assert!(foreground_covers_game(
            (0, 0, 1920, 1080),
            Some((-8, -8, 1928, 1048)),
            0.10
        ));
        assert!(!foreground_covers_game(
            (0, 0, 1920, 1080),
            Some((1600, 700, 1900, 1050)),
            0.10
        ));
        assert!(!foreground_covers_game((0, 0, 1920, 1080), None, 0.10));
        // Extreme / inverted bounds must not overflow i32 subtraction
        assert!(!foreground_covers_game(
            (0, 0, 1920, 1080),
            Some((i32::MAX, 0, i32::MIN, 1080)),
            0.10
        ));

        // Vietnamese Unicode case-folding parity with Python str.lower()
        assert_eq!(
            classify_browser_window("Bản Đồ Tương Tác Teyvat - Google Chrome", "Chrome_WidgetWin_1", "chrome.exe", false),
            Some(BrowserMatch::Specific)
        );
        assert_eq!(
            classify_browser_window("Bản Đồ Tương Tác", "", "chrome.exe", true),
            Some(BrowserMatch::Specific)
        );
        assert_eq!(
            classify_browser_window("CỐC CỐC", "", "", false),
            Some(BrowserMatch::Generic)
        );
        assert!(matches_game_window("ĐỈNH CAO TEVYAT", "", &["Đỉnh cao tevyat"]));

        // Browser title exclusion & verification
        assert!(!is_browser_window_from_meta(
            "chrome.exe",
            "genshin impact wiki",
            true
        ));
        assert!(is_browser_window_from_meta(
            "brave.exe",
            "hoyolab interactive map",
            true
        ));
        assert!(!is_browser_window_from_meta(
            "notepad.exe",
            "hoyolab interactive map",
            true
        ));

        // Live Win32 non-invasive checks
        let (sw, sh) = WindowFinder::get_screen_dimensions();
        assert!(sw > 0 && sh > 0);
        assert!(WindowFinder::get_monitor_count() >= 1);
        assert!(!WindowFinder::is_window_foreground(0));
        assert!(!WindowFinder::focus_window(0));
        assert!(!WindowFinder::is_game_visible_unfocused(
            0,
            (0, 0, 1920, 1080),
            &[],
            false
        ));
        assert_eq!(
            WindowFinder::get_game_window(&["__DefinitelyNoSuchGameWindow__"], false),
            None
        );
        assert_eq!(
            WindowFinder::find_game_window_any_state(&["__DefinitelyNoSuchGameWindow__"]),
            None
        );
        let fb = WindowFinder::get_game_window(&["__DefinitelyNoSuchGameWindow__"], true);
        assert_eq!(fb, Some((0, 0, 0, sw, sh)));
    }

    #[test]
    fn ensure_default_desktop_multithread_zero_handle_leak() {
        let cap = Arc::new(ScreenCapturer::new(DIB_POOL_SLOTS));
        let run_wave = |threads: usize| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let cap_worker = Arc::clone(&cap);
                    std::thread::spawn(move || {
                        ensure_default_desktop();
                        let frame = cap_worker
                            .capture_region(0, 0, 32, 32)
                            .expect("wave capture 32x32");
                        assert_eq!(frame.width, 32);
                        assert_eq!(frame.height, 32);
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("wave worker");
            }
            cap.close();
        };

        // Wave 1 warms up process-wide cached "Default" HDESK + thread pools
        run_wave(12);

        // SAFETY: Querying current process kernel handle count after Wave 1 completes.
        let proc_h = unsafe { GetCurrentProcess() };
        let mut handles_after_wave1 = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(proc_h, &mut handles_after_wave1);
        }

        // Wave 2 spawns 12 fresh threads; must leak 0 kernel handles.
        run_wave(12);

        let mut handles_after_wave2 = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(proc_h, &mut handles_after_wave2);
        }
        let delta = handles_after_wave2.saturating_sub(handles_after_wave1);
        assert_eq!(
            delta, 0,
            "Wave 2 leaked {delta} kernel handles (wave1={handles_after_wave1}, wave2={handles_after_wave2})"
        );
    }

    #[test]
    fn screen_capturer_auto_hdr_probe_and_capture_smoke() {
        let cap = ScreenCapturer::new(DIB_POOL_SLOTS);
        assert!(cap.active_hdr_monitor_info(0, 0, 0, 100).is_none());
        assert!(!cap.is_region_hdr(0, 0, -10, 100));

        let bgr = cap.capture_region(0, 0, 96, 64).expect("auto-hdr bgr");
        assert_eq!((bgr.width, bgr.height, bgr.format), (96, 64, PixelFormat::Bgr8));
        assert_eq!(bgr.data.len(), 96 * 64 * 3);

        let bgra = cap.capture_region_bgra(0, 0, 96, 64).expect("auto-hdr bgra");
        assert_eq!((bgra.width, bgra.height, bgra.format), (96, 64, PixelFormat::Bgra8));
        assert_eq!(bgra.data.len(), 96 * 64 * 4);
        cap.close();

        let gdi_only = ScreenCapturer::new(DIB_POOL_SLOTS).with_hdr_wgc(false);
        assert!(!gdi_only.is_region_hdr(0, 0, 96, 64));
        gdi_only.close();
    }
}
