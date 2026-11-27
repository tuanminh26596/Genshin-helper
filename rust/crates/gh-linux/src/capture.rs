//! X11-based screen capture for Linux, mirroring `gh-win32::capture`.
//!
//! Uses `x11rb` to find and capture the Genshin Impact window (running under
//! Proton/Wine inside the same X11 session). The captured region is converted
//! to `PixelFormat::Bgr8` to match what the CV pipeline expects.
//!
//! Zero `SendInput` / `XSendEvent` with synthetic events anywhere in this module.

use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing::warn;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

pub const DIB_POOL_SLOTS: usize = 4;

// ---------------------------------------------------------------------------
// Window class / process / title filters (mirrored from gh-win32::capture)
// ---------------------------------------------------------------------------

pub const NON_COVERING_CLASSES: [&str; 4] = [
    "desktop_window",
    "plasmashell",
    "krunner",
    "dde-desktop",
];

pub const BROWSER_PROCESS_NAMES: [&str; 15] = [
    "chrome",
    "msedge",
    "brave-browser",
    "firefox",
    "opera",
    "vivaldi-bin",
    "browser",
    "zen-browser",
    "arc",
    "floorp",
    "thorium",
    "waterfox",
    "coccoc",
    "chromium",
    "yandex-browser",
];

pub const BROWSER_TITLE_EXCLUDE: [&str; 7] = [
    "genshin impact",
    "code",
    "visual studio",
    "discord",
    "spotify",
    "powershell",
    "terminal",
];

pub const GAME_IGNORED_TITLE_SUBSTRINGS: [&str; 4] = [
    "helper",
    "hoyolab sync",
    "visual studio",
    "code",
];

pub const BROWSER_KEYWORDS: [&str; 4] = [
    "hoyolab",
    "interactive map",
    "bản đồ tương tác",
    "genshin helper",
];

// ---------------------------------------------------------------------------
// X11 connection singleton
// ---------------------------------------------------------------------------

static X11_CONN: OnceLock<Option<Arc<RustConnection>>> = OnceLock::new();

fn get_x11_conn() -> Option<Arc<RustConnection>> {
    X11_CONN
        .get_or_init(|| {
            match RustConnection::connect(None) {
                Ok((conn, _screen_num)) => Some(Arc::new(conn)),
                Err(e) => {
                    warn!("gh-linux: failed to connect to X11 display: {e}");
                    None
                }
            }
        })
        .clone()
}

fn default_screen(conn: &RustConnection) -> &Screen {
    let setup = conn.setup();
    &setup.roots[0]
}

// ---------------------------------------------------------------------------
// Hyprland IPC (Wayland native + XWayland + Gamescope window tracking)
// ---------------------------------------------------------------------------

static HYPR_ADDR_MAP: OnceLock<Mutex<HashMap<isize, String>>> = OnceLock::new();

fn hypr_addr_map() -> &'static Mutex<HashMap<isize, String>> {
    HYPR_ADDR_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn hypr_socket_path() -> Option<std::path::PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if sig.is_empty() {
        return None;
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let p = std::path::PathBuf::from(runtime)
            .join("hypr")
            .join(&sig)
            .join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let tmp_p = std::path::PathBuf::from("/tmp/hypr")
        .join(&sig)
        .join(".socket.sock");
    tmp_p.exists().then_some(tmp_p)
}

pub(crate) fn hypr_ipc_pub(cmd: &str) -> Option<String> {
    hypr_ipc(cmd)
}

fn hypr_ipc(cmd: &str) -> Option<String> {
    let path = hypr_socket_path()?;
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    Some(out)
}

fn hypr_addr_to_isize(addr: &str) -> Option<isize> {
    let hex = addr.trim().trim_start_matches("0x").trim_start_matches("0X");
    let val = u64::from_str_radix(hex, 16).ok()?;
    // Keep non-zero positive/negative handle; store mapping for reverse lookup
    let id = (val as isize) | (1isize << 62);
    if let Ok(mut map) = hypr_addr_map().lock() {
        map.insert(id, addr.trim().to_owned());
    }
    Some(id)
}

fn hypr_enumerate_clients() -> Option<Vec<WindowInfo>> {
    let raw = hypr_ipc("j/clients")?;
    let clients: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
    let mut out = Vec::with_capacity(clients.len());
    for c in clients {
        if c.get("mapped").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let addr = c.get("address").and_then(|v| v.as_str())?;
        let hwnd = hypr_addr_to_isize(addr)?;
        let title = c
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let class = c
            .get("class")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let at = c.get("at").and_then(|v| v.as_array());
        let size = c.get("size").and_then(|v| v.as_array());
        let (x, y) = at
            .and_then(|a| Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32)))
            .unwrap_or((0, 0));
        let (w, h) = size
            .and_then(|s| Some((s.first()?.as_i64()? as i32, s.get(1)?.as_i64()? as i32)))
            .unwrap_or((0, 0));
        out.push(WindowInfo {
            hwnd,
            title,
            class,
            rect: RectI32 {
                x,
                y,
                width: w,
                height: h,
            },
        });
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// WindowFinder (mirrors gh-win32::capture::WindowFinder)
// ---------------------------------------------------------------------------

/// X11-based window enumeration and focus management.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowFinder;

#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// X11 Window ID, cast to isize for API compatibility with Win32 HWND.
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub rect: RectI32,
}

impl WindowFinder {
    /// Returns the window ID with the given title substring (Genshin Impact).
    /// Supports Hyprland IPC (Wayland/XWayland/Gamescope) with X11 fallback.
    pub fn find_game_window(titles: &[&str]) -> Option<isize> {
        if let Some(wins) = hypr_enumerate_clients() {
            for win in wins {
                let title_lc = win.title.to_ascii_lowercase();
                let class_lc = win.class.to_ascii_lowercase();
                let matches_title = titles
                    .iter()
                    .any(|t| title_lc.contains(&t.to_ascii_lowercase()));
                let is_proton_genshin = class_lc.contains("genshinimpact.exe")
                    || class_lc.contains("yuanshen.exe");
                if matches_title || is_proton_genshin {
                    if GAME_IGNORED_TITLE_SUBSTRINGS
                        .iter()
                        .any(|s| title_lc.contains(s))
                    {
                        continue;
                    }
                    return Some(win.hwnd);
                }
            }
            return None;
        }

        let conn = get_x11_conn()?;
        let root = default_screen(&conn).root;
        let wins = enumerate_windows(&conn, root).ok()?;
        for win in wins {
            let title_lc = win.title.to_ascii_lowercase();
            let class_lc = win.class.to_ascii_lowercase();
            let matches_title = titles
                .iter()
                .any(|t| title_lc.contains(&t.to_ascii_lowercase()));
            let is_proton_genshin = class_lc.contains("genshinimpact.exe")
                || class_lc.contains("yuanshen.exe");
            if matches_title || is_proton_genshin {
                // Skip our own helper window
                if GAME_IGNORED_TITLE_SUBSTRINGS
                    .iter()
                    .any(|s| title_lc.contains(s))
                {
                    continue;
                }
                return Some(win.hwnd);
            }
        }
        None
    }

    /// Alias used by tracking_loop — searches even minimized windows.
    pub fn find_game_window_any_state(titles: &[&str]) -> Option<isize> {
        Self::find_game_window(titles)
    }

    /// Returns the focused window (Hyprland IPC on Wayland, X11 fallback).
    pub fn get_foreground_window() -> Option<isize> {
        if let Some(raw) = hypr_ipc("j/activewindow") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(addr) = v.get("address").and_then(|a| a.as_str()) {
                    return hypr_addr_to_isize(addr);
                }
            }
            return None;
        }
        let conn = get_x11_conn()?;
        let reply = conn
            .get_input_focus()
            .ok()?
            .reply()
            .ok()?;
        Some(reply.focus as isize)
    }

    /// Returns the client rectangle of the window in screen coordinates.
    pub fn get_screen_dimensions() -> (i32, i32) {
        let (_, _, w, h) = get_screen_bounds();
        (w, h)
    }

    pub fn get_monitor_count() -> usize {
        if let Some(raw) = hypr_ipc("j/monitors")
            && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
            && !arr.is_empty()
        {
            return arr.len();
        }
        1
    }

    pub fn get_window_rect(hwnd: isize) -> Option<RectI32> {
        if let Ok(map) = hypr_addr_map().lock()
            && let Some(addr) = map.get(&hwnd).cloned()
        {
            drop(map);
            if let Some(wins) = hypr_enumerate_clients()
                && let Some(w) = wins.into_iter().find(|w| {
                    hypr_addr_map()
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&w.hwnd).cloned())
                        .as_deref()
                        == Some(addr.as_str())
                })
            {
                return Some(w.rect);
            }
        }
        let conn = get_x11_conn()?;
        let wid = Window::from(hwnd as u32);
        let geom = conn.get_geometry(wid).ok()?.reply().ok()?;
        // Translate origin to root coordinates
        let translated = conn
            .translate_coordinates(wid, default_screen(&conn).root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some(RectI32 {
            x: translated.dst_x as i32,
            y: translated.dst_y as i32,
            width: geom.width as i32,
            height: geom.height as i32,
        })
    }

    /// Find browser window containing HoYoLAB map keywords.
    pub fn find_browser_window_any_state(_specific_only: bool) -> Option<isize> {
        let wins = if let Some(hypr_wins) = hypr_enumerate_clients() {
            hypr_wins
        } else {
            let conn = get_x11_conn()?;
            let root = default_screen(&conn).root;
            enumerate_windows(&conn, root).ok()?
        };

        for win in &wins {
            let title_lc = win.title.to_ascii_lowercase();
            if BROWSER_KEYWORDS.iter().any(|k| title_lc.contains(k)) {
                return Some(win.hwnd);
            }
        }
        // Fallback: any window whose class looks like a browser
        for win in &wins {
            let class_lc = win.class.to_ascii_lowercase();
            if BROWSER_PROCESS_NAMES.iter().any(|p| class_lc.contains(p)) {
                let title_lc = win.title.to_ascii_lowercase();
                if !BROWSER_TITLE_EXCLUDE.iter().any(|e| title_lc.contains(e)) {
                    return Some(win.hwnd);
                }
            }
        }
        None
    }

    /// Raise and focus a window (Hyprland dispatch on Wayland, X11 fallback).
    pub fn focus_window(hwnd: isize) -> bool {
        if let Ok(map) = hypr_addr_map().lock()
            && let Some(addr) = map.get(&hwnd).cloned()
        {
            drop(map);
            return hypr_ipc(&format!("dispatch focuswindow address:{addr}")).is_some();
        }
        let Some(conn) = get_x11_conn() else {
            return false;
        };
        let wid = Window::from(hwnd as u32);
        // Raise the window
        let _ = conn.configure_window(
            wid,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        );
        // Set input focus
        let _ = conn.set_input_focus(InputFocus::PARENT, wid, x11rb::CURRENT_TIME);
        let _ = conn.flush();
        true
    }

    /// Returns `true` if the given hwnd is currently the foreground window.
    pub fn is_foreground(hwnd: isize) -> bool {
        Self::get_foreground_window() == Some(hwnd)
    }

    pub fn is_window_foreground(hwnd: isize) -> bool {
        Self::is_foreground(hwnd)
    }

    pub fn cursor_screen_pos() -> Option<(i32, i32)> {
        get_cursor_pos()
    }

    pub fn get_game_window(
        titles: &[&str],
        allow_screen_fallback: bool,
    ) -> Option<(isize, i32, i32, i32, i32)> {
        if let Some(hwnd) = Self::find_game_window(titles)
            && let Some(r) = Self::get_window_rect(hwnd)
        {
            return Some((hwnd, r.x, r.y, r.width, r.height));
        }
        if allow_screen_fallback {
            let (x, y, w, h) = get_screen_bounds();
            return Some((0, x, y, w, h));
        }
        None
    }

    pub fn is_game_visible_unfocused(
        hwnd: isize,
        _game_rect: (i32, i32, i32, i32),
        _own_hwnds: &[isize],
        keep_when_unfocused: bool,
    ) -> bool {
        if !keep_when_unfocused {
            return false;
        }
        if let Ok(map) = hypr_addr_map().lock()
            && let Some(addr) = map.get(&hwnd).cloned()
        {
            drop(map);
            // Check if the client's workspace is active on any monitor
            if let (Some(clients_raw), Some(mons_raw)) = (hypr_ipc("j/clients"), hypr_ipc("j/monitors"))
                && let (Ok(clients), Ok(mons)) = (
                    serde_json::from_str::<Vec<serde_json::Value>>(&clients_raw),
                    serde_json::from_str::<Vec<serde_json::Value>>(&mons_raw),
                )
            {
                let active_ws: Vec<i64> = mons
                    .iter()
                    .filter_map(|m| m.get("activeWorkspace")?.get("id")?.as_i64())
                    .collect();
                if let Some(c) = clients
                    .iter()
                    .find(|c| c.get("address").and_then(|a| a.as_str()) == Some(addr.as_str()))
                {
                    let ws_id = c.get("workspace").and_then(|w| w.get("id")).and_then(|i| i.as_i64());
                    let hidden = c.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false);
                    return !hidden && ws_id.is_some_and(|id| active_ws.contains(&id));
                }
            }
        }
        Self::get_window_rect(hwnd).is_some()
    }
}

// ---------------------------------------------------------------------------
// Enumerate all X11 windows recursively
// ---------------------------------------------------------------------------

fn enumerate_windows(
    conn: &RustConnection,
    root: Window,
) -> Result<Vec<WindowInfo>, Box<dyn std::error::Error>> {
    let mut result = Vec::new();
    enumerate_recursive(conn, root, &mut result);
    Ok(result)
}

fn enumerate_recursive(conn: &RustConnection, win: Window, out: &mut Vec<WindowInfo>) {
    // Get children
    let Ok(cookie) = conn.query_tree(win) else {
        return;
    };
    let Ok(reply) = cookie.reply() else {
        return;
    };

    for child in reply.children {
        // Check if mapped (visible)
        if let Some(attrs) = conn.get_window_attributes(child).ok().and_then(|c| c.reply().ok()) {
            if attrs.map_state != MapState::UNMAPPED {
                let title = get_window_title(conn, child).unwrap_or_default();
                let class = get_window_class(conn, child).unwrap_or_default();
                if !title.is_empty() {
                    let rect = get_win_rect(conn, child);
                    out.push(WindowInfo {
                        hwnd: child as isize,
                        title,
                        class,
                        rect: rect.unwrap_or_default(),
                    });
                }
            }
        }
        enumerate_recursive(conn, child, out);
    }
}

fn get_window_title(conn: &RustConnection, win: Window) -> Option<String> {
    // Try _NET_WM_NAME (UTF-8) first
    let atom_net_wm_name = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let atom_utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;

    if let Some(reply) = conn
        .get_property(false, win, atom_net_wm_name, atom_utf8, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        if !reply.value.is_empty() {
            return String::from_utf8(reply.value).ok();
        }
    }

    // Fallback: WM_NAME (Latin-1)
    let atom_wm_name = AtomEnum::WM_NAME;
    if let Some(reply) = conn
        .get_property(false, win, atom_wm_name, AtomEnum::STRING, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).into_owned());
        }
    }
    None
}

fn get_window_class(conn: &RustConnection, win: Window) -> Option<String> {
    let reply = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    // WM_CLASS is two null-terminated strings; take the second (instance name)
    let raw = String::from_utf8_lossy(&reply.value).into_owned();
    let parts: Vec<&str> = raw.split('\0').collect();
    parts.get(1).map(|s| s.to_string()).or_else(|| parts.first().map(|s| s.to_string()))
}

fn get_win_rect(conn: &RustConnection, win: Window) -> Option<RectI32> {
    let geom = conn.get_geometry(win).ok()?.reply().ok()?;
    let root = default_screen(conn).root;
    let translated = conn
        .translate_coordinates(win, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some(RectI32 {
        x: translated.dst_x as i32,
        y: translated.dst_y as i32,
        width: geom.width as i32,
        height: geom.height as i32,
    })
}

// ---------------------------------------------------------------------------
// ScreenCapturer (mirrors gh-win32::capture::ScreenCapturer)
// ---------------------------------------------------------------------------

/// X11-based screen region capturer using `XGetImage` (via x11rb `get_image`).
///
/// For Proton/Wine game windows on X11, this correctly captures the composited
/// window content since Wine renders via XCB/GLX which is composited by the
/// window manager into the X11 framebuffer.
pub struct ScreenCapturer {
    _pool_slots: usize,
}

impl ScreenCapturer {
    pub fn new(pool_slots: usize) -> Self {
        // Initialize X11 connection eagerly
        let _ = get_x11_conn();
        Self { _pool_slots: pool_slots }
    }

    pub fn with_hdr_wgc(self, _enabled: bool) -> Self {
        // HDR WGC is Windows-only; on Linux this is a no-op
        self
    }

    pub fn close(&self) {
        // No explicit cleanup needed for X11
    }

    /// Captures a screen region from absolute screen coordinates.
    /// Uses native Wayland `zwlr_screencopy_manager_v1` when available,
    /// falling back to X11 `get_image`. Returns `FrameBuf` with `PixelFormat::Bgr8`.
    pub fn capture_region(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        if let Some(frame) =
            crate::wayland_capture::capture_region(x, y, width, height, PixelFormat::Bgr8)
        {
            return Some(frame);
        }
        let conn = get_x11_conn()?;
        let root = default_screen(&conn).root;

        let img = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                x as i16,
                y as i16,
                width as u16,
                height as u16,
                !0u32,
            )
            .ok()?
            .reply()
            .ok()?;

        let w = width as u32;
        let h = height as u32;
        let stride_bgr = w as usize * 3;
        let mut data = vec![0u8; stride_bgr * h as usize];

        // X11 ZPixmap is BGRA (4 bytes per pixel on 24/32-bit display)
        let src = &img.data;
        let bytes_per_pixel = src.len() / (w as usize * h as usize).max(1);
        let bytes_per_pixel = bytes_per_pixel.max(3);

        for row in 0..h as usize {
            for col in 0..w as usize {
                let src_off = (row * w as usize + col) * bytes_per_pixel;
                let dst_off = row * stride_bgr + col * 3;
                if src_off + 2 < src.len() && dst_off + 2 < data.len() {
                    // X11 ZPixmap: B G R [A]
                    data[dst_off] = src[src_off];
                    data[dst_off + 1] = src[src_off + 1];
                    data[dst_off + 2] = src[src_off + 2];
                }
            }
        }

        Some(FrameBuf {
            width: w,
            height: h,
            stride: stride_bgr,
            format: PixelFormat::Bgr8,
            data,
        })
    }

    /// Captures a screen region as BGRA.
    pub fn capture_region_bgra(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        if let Some(frame) =
            crate::wayland_capture::capture_region(x, y, width, height, PixelFormat::Bgra8)
        {
            return Some(frame);
        }
        let conn = get_x11_conn()?;
        let root = default_screen(&conn).root;

        let img = conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                x as i16,
                y as i16,
                width as u16,
                height as u16,
                !0u32,
            )
            .ok()?
            .reply()
            .ok()?;

        let w = width as u32;
        let h = height as u32;
        let stride = w as usize * 4;
        let mut data = vec![0u8; stride * h as usize];

        let src = &img.data;
        let bytes_per_pixel = src.len() / (w as usize * h as usize).max(1);
        let bytes_per_pixel = bytes_per_pixel.max(4);

        for row in 0..h as usize {
            for col in 0..w as usize {
                let src_off = (row * w as usize + col) * bytes_per_pixel;
                let dst_off = row * stride + col * 4;
                if src_off + 3 < src.len() && dst_off + 3 < data.len() {
                    data[dst_off] = src[src_off];
                    data[dst_off + 1] = src[src_off + 1];
                    data[dst_off + 2] = src[src_off + 2];
                    data[dst_off + 3] = src[src_off + 3];
                }
            }
        }

        Some(FrameBuf {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Bgra8,
            data,
        })
    }

    /// Returns `None` on Linux (no HDR detection without DXGI).
    pub fn active_hdr_monitor_info(
        &self,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> Option<crate::wgc_hdr::MonitorHdrInfo> {
        None
    }

    /// Always false on Linux.
    pub fn is_region_hdr(&self, _x: i32, _y: i32, _w: i32, _h: i32) -> bool {
        false
    }

    pub fn capture_window_client(
        &self,
        _hwnd: isize,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Option<FrameBuf> {
        self.capture_region(left, top, width, height)
    }

    pub fn window_capture_status(&self) -> String {
        if crate::wayland_capture::is_available() {
            "wayland-screencopy".to_owned()
        } else {
            "x11-getimage".to_owned()
        }
    }

    pub fn release_window_capture(&self) {}

    pub fn capture_window_client_outcome(
        &self,
        _hwnd: isize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> crate::wgc_hdr::WgcWindowCaptureOutcome<FrameBuf> {
        match self.capture_region(x, y, w, h) {
            Some(frame) => crate::wgc_hdr::WgcWindowCaptureOutcome::Frame(frame),
            None => crate::wgc_hdr::WgcWindowCaptureOutcome::Failed,
        }
    }
}

// ---------------------------------------------------------------------------
// DPI awareness (no-op on Linux)
// ---------------------------------------------------------------------------

pub fn ensure_dpi_awareness() {
    // Linux uses logical pixels natively; no special setup needed.
}

pub fn ensure_default_desktop() -> bool {
    true
}

/// Queries the current cursor position in root screen coordinates.
pub fn get_cursor_pos() -> Option<(i32, i32)> {
    if let Some(raw) = hypr_ipc("j/cursorpos")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        && let (Some(x), Some(y)) = (
            v.get("x").and_then(|n| n.as_i64()),
            v.get("y").and_then(|n| n.as_i64()),
        )
    {
        return Some((x as i32, y as i32));
    }
    let conn = get_x11_conn()?;
    let root = default_screen(&conn).root;
    let reply = conn.query_pointer(root).ok()?.reply().ok()?;
    Some((reply.root_x as i32, reply.root_y as i32))
}

/// Returns `(x, y, width, height)` of the primary screen.
pub fn get_screen_bounds() -> (i32, i32, i32, i32) {
    if let Some(bounds) = crate::wayland_capture::screen_bounds() {
        return bounds;
    }
    if let Some(conn) = get_x11_conn() {
        let scr = default_screen(&conn);
        return (0, 0, scr.width_in_pixels as i32, scr.height_in_pixels as i32);
    }
    (0, 0, 1920, 1080)
}
