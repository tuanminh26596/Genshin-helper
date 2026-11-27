//! Reusable Win32 `WS_EX_LAYERED` window host, premultiplied BGRA DIBSection,
//! 2.0s `WDA_EXCLUDEFROMCAPTURE` refresh timer, and `WM_APP_CAPTURE_AFFINITY` (`0x8015`).

use crate::{HudError, HudResult};
use gh_core::frame::RectI32;
#[cfg(windows)]
use gh_platform::capture::{
    WindowFinder, ensure_default_desktop, ensure_dpi_awareness, BITBLT_ROP_SRCCOPY,
};
#[cfg(windows)]
use gh_platform::game_settings::refresh_exclude_from_capture;
#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::mem::size_of;
#[cfg(windows)]
use std::ptr::null_mut;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::{Arc, Condvar, Mutex};
#[cfg(windows)]
use std::thread::{self, JoinHandle};
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush,
    GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MonitorFromPoint, SelectObject,
};
#[cfg(windows)]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowRect, HTBOTTOM,
    HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT,
    HTTOPRIGHT, HWND_TOPMOST, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    KillTimer, LoadCursorW, MA_NOACTIVATE, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetCursor,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA,
    UnregisterClassW, UpdateLayeredWindow, WINDOW_EX_STYLE, WM_APP, WM_CANCELMODE,
    WM_CAPTURECHANGED, WM_DESTROY, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_SETCURSOR, WM_SIZING, WM_TIMER, WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_TOP, WMSZ_TOPLEFT,
    WMSZ_TOPRIGHT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
#[cfg(windows)]
use windows::core::PCWSTR;

#[cfg(target_os = "linux")]
pub use crate::layered_linux::{
    LayeredDibSurface, LayeredWindowCtx, LayeredWindowHost, query_monitor_bounds,
    query_monitor_work_area,
};

#[cfg(not(windows))]
const HTCLIENT: u32 = 1;
#[cfg(not(windows))]
const HTCAPTION: u32 = 2;
#[cfg(not(windows))]
const HTLEFT: u32 = 10;
#[cfg(not(windows))]
const HTRIGHT: u32 = 11;
#[cfg(not(windows))]
const HTTOP: u32 = 12;
#[cfg(not(windows))]
const HTTOPLEFT: u32 = 13;
#[cfg(not(windows))]
const HTTOPRIGHT: u32 = 14;
#[cfg(not(windows))]
const HTBOTTOM: u32 = 15;
#[cfg(not(windows))]
const HTBOTTOMLEFT: u32 = 16;
#[cfg(not(windows))]
const HTBOTTOMRIGHT: u32 = 17;
#[cfg(not(windows))]
const WMSZ_LEFT: u32 = 1;
#[cfg(not(windows))]
const WMSZ_TOP: u32 = 3;
#[cfg(not(windows))]
const WMSZ_TOPLEFT: u32 = 4;
#[cfg(not(windows))]
const WMSZ_TOPRIGHT: u32 = 5;
#[cfg(not(windows))]
const WMSZ_BOTTOMLEFT: u32 = 7;

pub use gh_core::{WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_APP_CAPTURE_AFFINITY};

pub const WDA_REFRESH_INTERVAL_MS: u64 = 2000;
pub const WDA_REFRESH_MS: u32 = 2000;
pub const CAPTURE_AFFINITY_MSG: u32 = gh_core::WM_APP_CAPTURE_AFFINITY;
#[cfg(windows)]
pub const CAPTURE_ROP_GUARD: u32 = BITBLT_ROP_SRCCOPY;
#[cfg(not(windows))]
pub const CAPTURE_ROP_GUARD: u32 = 0x00CC_0020;

#[cfg(windows)]
pub const EX_STYLE_CLICK_THROUGH: u32 = WS_EX_LAYERED.0
    | WS_EX_TRANSPARENT.0
    | WS_EX_TOPMOST.0
    | WS_EX_NOACTIVATE.0
    | WS_EX_TOOLWINDOW.0;
#[cfg(windows)]
pub const EX_STYLE_INTERACTIVE: u32 = EX_STYLE_CLICK_THROUGH & !WS_EX_TRANSPARENT.0;

pub const TIMER_ID_WDA_REFRESH: usize = 102;
#[cfg(windows)]
const WM_APP_HOST_CLOSE: u32 = WM_APP + 0x7F; // 0x807F, reserved away from 0x8001..=0x8015
pub const OFFSCREEN_TEST_OFFSET: (i32, i32) = (-20_000, -20_000);

#[cfg(windows)]
static INSTANCE_SEQ: AtomicUsize = AtomicUsize::new(1);

fn env_flag_truthy(val: Option<&str>) -> bool {
    val.map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Returns true when `GH_HUD_OFFSCREEN` asks overlays to shift offscreen during tests.
pub fn is_hud_offscreen_env() -> bool {
    env_flag_truthy(std::env::var("GH_HUD_OFFSCREEN").ok().as_deref())
}

/// Resolves the initial screen offset, shifting default `(0, 0)` overlays offscreen when
/// `GH_HUD_OFFSCREEN` is enabled for test runs while leaving explicit offsets untouched.
pub fn resolve_screen_offset(configured: (i32, i32), env_offscreen: bool) -> (i32, i32) {
    if configured != (0, 0) {
        configured
    } else if env_offscreen {
        OFFSCREEN_TEST_OFFSET
    } else {
        (0, 0)
    }
}

/// Converts straight RGBA8 pixels into top-down premultiplied BGRA8 bytes in `out_bgra`,
/// matching `src/nav_hud.py::rgba_to_premultiplied_bgra` (`(c * a + 127) // 255`).
pub fn rgba_to_premultiplied_bgra_into(rgba: &[u8], out_bgra: &mut [u8]) -> HudResult<()> {
    if !rgba.len().is_multiple_of(4) {
        return Err(HudError::InvalidBuffer(format!(
            "RGBA slice length {} is not a multiple of 4",
            rgba.len()
        )));
    }
    if out_bgra.len() != rgba.len() {
        return Err(HudError::InvalidBuffer(format!(
            "output BGRA length {} does not match input RGBA length {}",
            out_bgra.len(),
            rgba.len()
        )));
    }

    for (src, dst) in rgba.as_chunks::<4>().0.iter().zip(out_bgra.as_chunks_mut::<4>().0.iter_mut()) {
        let a = src[3] as u16;
        let r_pm = ((src[0] as u16 * a + 127) / 255) as u8;
        let g_pm = ((src[1] as u16 * a + 127) / 255) as u8;
        let b_pm = ((src[2] as u16 * a + 127) / 255) as u8;
        dst[0] = b_pm;
        dst[1] = g_pm;
        dst[2] = r_pm;
        dst[3] = src[3];
    }
    Ok(())
}

/// Allocates and returns top-down premultiplied BGRA8 bytes from straight RGBA8 pixels.
pub fn rgba_to_premultiplied_bgra(rgba: &[u8]) -> HudResult<Vec<u8>> {
    let mut out = vec![0u8; rgba.len()];
    rgba_to_premultiplied_bgra_into(rgba, &mut out)?;
    Ok(out)
}

/// Converts `tiny-skia` premultiplied RGBA8 pixels into top-down premultiplied BGRA8 bytes,
/// swizzling `R <-> B` and defensively clamping channels to `<= alpha`.
pub fn premultiplied_rgba_to_bgra_into(pm_rgba: &[u8], out_bgra: &mut [u8]) -> HudResult<()> {
    if !pm_rgba.len().is_multiple_of(4) {
        return Err(HudError::InvalidBuffer(format!(
            "premultiplied RGBA slice length {} is not a multiple of 4",
            pm_rgba.len()
        )));
    }
    if out_bgra.len() != pm_rgba.len() {
        return Err(HudError::InvalidBuffer(format!(
            "output BGRA length {} does not match input premultiplied RGBA length {}",
            out_bgra.len(),
            pm_rgba.len()
        )));
    }

    for (src, dst) in pm_rgba.as_chunks::<4>().0.iter().zip(out_bgra.as_chunks_mut::<4>().0.iter_mut()) {
        let a = src[3];
        dst[0] = src[2].min(a);
        dst[1] = src[1].min(a);
        dst[2] = src[0].min(a);
        dst[3] = a;
    }
    Ok(())
}

/// Allocates and returns top-down premultiplied BGRA8 bytes from `tiny-skia` premultiplied RGBA8 pixels.
pub fn premultiplied_rgba_to_bgra(pm_rgba: &[u8]) -> HudResult<Vec<u8>> {
    let mut out = vec![0u8; pm_rgba.len()];
    premultiplied_rgba_to_bgra_into(pm_rgba, &mut out)?;
    Ok(out)
}

/// Queries the Win32 monitor work area `(left, top, right, bottom)` containing `(point_x, point_y)`.
#[cfg(windows)]
pub fn query_monitor_work_area(point_x: i32, point_y: i32) -> (i32, i32, i32, i32) {
    // SAFETY: `MonitorFromPoint` and `GetMonitorInfoW` operate on stack structs with `cbSize` set.
    unsafe {
        let hmon = MonitorFromPoint(POINT { x: point_x, y: point_y }, MONITOR_DEFAULTTONEAREST);
        if !hmon.is_invalid() {
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                let rc = mi.rcWork;
                if rc.right > rc.left && rc.bottom > rc.top {
                    return (rc.left, rc.top, rc.right, rc.bottom);
                }
            }
        }
    }
    let (sw, sh) = WindowFinder::get_screen_dimensions();
    let w = if sw > 0 { sw } else { 1920 };
    let h = if sh > 0 { sh } else { 1080 };
    (0, 0, w, h)
}

/// Queries the full Win32 monitor bounds (`rcMonitor`, including the taskbar strip covered by
/// Borderless Fullscreen games) containing `(point_x, point_y)`.
#[cfg(windows)]
pub fn query_monitor_bounds(point_x: i32, point_y: i32) -> (i32, i32, i32, i32) {
    // SAFETY: `MonitorFromPoint` and `GetMonitorInfoW` operate on stack structs with `cbSize` set.
    unsafe {
        let hmon = MonitorFromPoint(POINT { x: point_x, y: point_y }, MONITOR_DEFAULTTONEAREST);
        if !hmon.is_invalid() {
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                let rc = mi.rcMonitor;
                if rc.right > rc.left && rc.bottom > rc.top {
                    return (rc.left, rc.top, rc.right, rc.bottom);
                }
            }
        }
    }
    let (sw, sh) = WindowFinder::get_screen_dimensions();
    let w = if sw > 0 { sw } else { 1920 };
    let h = if sh > 0 { sh } else { 1080 };
    (0, 0, w, h)
}

/// UI-thread-owned top-down 32bpp `CreateDIBSection` surface for `UpdateLayeredWindow`.
#[cfg(windows)]
pub struct LayeredDibSurface {
    memdc: HDC,
    hbmp: HBITMAP,
    old_obj: HGDIOBJ,
    bits_ptr: *mut u8,
    width: i32,
    height: i32,
    realloc_count: usize,
}

#[cfg(windows)]
impl LayeredDibSurface {
    pub const fn new() -> Self {
        Self {
            memdc: HDC(null_mut()),
            hbmp: HBITMAP(null_mut()),
            old_obj: HGDIOBJ(null_mut()),
            bits_ptr: null_mut(),
            width: 0,
            height: 0,
            realloc_count: 0,
        }
    }

    pub const fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub const fn realloc_count(&self) -> usize {
        self.realloc_count
    }

    pub fn is_ready(&self) -> bool {
        !self.memdc.is_invalid() && !self.hbmp.is_invalid() && !self.bits_ptr.is_null()
    }

    /// Ensures the DIBSection matches `(width, height)`, reusing existing GDI handles when possible.
    /// Returns `Ok(true)` if a new DIBSection was allocated, `Ok(false)` if reused.
    pub fn ensure_size(&mut self, width: i32, height: i32) -> HudResult<bool> {
        if width <= 0 || height <= 0 {
            return Err(HudError::InvalidBuffer(format!(
                "invalid surface dimensions {width}x{height}"
            )));
        }
        if self.width == width && self.height == height && self.is_ready() {
            return Ok(false);
        }

        self.release();

        // SAFETY: Creating a memory DC and top-down 32-bit BI_RGB DIBSection on the owning thread.
        unsafe {
            let memdc = CreateCompatibleDC(None);
            if memdc.is_invalid() {
                return Err(HudError::Win32("CreateCompatibleDC failed".into()));
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = null_mut();
            let hbmp = match CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bmp) if !bmp.is_invalid() && !bits.is_null() => bmp,
                Ok(bmp) => {
                    if !bmp.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(bmp.0));
                    }
                    let _ = DeleteDC(memdc);
                    return Err(HudError::Win32(
                        "CreateDIBSection returned null bits pointer".into(),
                    ));
                }
                Err(err) => {
                    let _ = DeleteDC(memdc);
                    return Err(HudError::Win32(format!("CreateDIBSection failed: {err}")));
                }
            };

            let old_obj = SelectObject(memdc, HGDIOBJ(hbmp.0));
            if old_obj.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                let _ = DeleteDC(memdc);
                return Err(HudError::Win32(
                    "SelectObject failed for DIBSection".into(),
                ));
            }

            self.memdc = memdc;
            self.hbmp = hbmp;
            self.old_obj = old_obj;
            self.bits_ptr = bits.cast::<u8>();
            self.width = width;
            self.height = height;
            self.realloc_count += 1;
        }
        Ok(true)
    }

    fn dib_slice_mut(&mut self) -> HudResult<&mut [u8]> {
        if !self.is_ready() || self.width <= 0 || self.height <= 0 {
            return Err(HudError::InvalidBuffer(
                "DIBSection surface is not initialized".into(),
            ));
        }
        let byte_len = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| HudError::InvalidBuffer("surface byte size overflow".into()))?;
        // SAFETY: `bits_ptr` points to a valid `width * height * 4` DIBSection buffer owned by `self`.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.bits_ptr, byte_len) })
    }

    /// Uploads `tiny-skia` premultiplied RGBA8 pixels directly into the DIBSection.
    pub fn upload_premultiplied_rgba(
        &mut self,
        width: i32,
        height: i32,
        pm_rgba: &[u8],
    ) -> HudResult<()> {
        self.ensure_size(width, height)?;
        let dst = self.dib_slice_mut()?;
        premultiplied_rgba_to_bgra_into(pm_rgba, dst)?;
        // SAFETY: Flushes GDI pipeline after writing directly to DIBSection bits.
        unsafe {
            let _ = GdiFlush();
        }
        Ok(())
    }

    /// Uploads straight RGBA8 pixels, premultiplying and swizzling to BGRA8 into the DIBSection.
    pub fn upload_straight_rgba(&mut self, width: i32, height: i32, rgba: &[u8]) -> HudResult<()> {
        self.ensure_size(width, height)?;
        let dst = self.dib_slice_mut()?;
        rgba_to_premultiplied_bgra_into(rgba, dst)?;
        // SAFETY: Flushes GDI pipeline after writing directly to DIBSection bits.
        unsafe {
            let _ = GdiFlush();
        }
        Ok(())
    }

    /// Uploads already-premultiplied BGRA8 bytes directly into the DIBSection.
    pub fn upload_premultiplied_bgra(
        &mut self,
        width: i32,
        height: i32,
        bgra: &[u8],
    ) -> HudResult<()> {
        self.ensure_size(width, height)?;
        let dst = self.dib_slice_mut()?;
        if bgra.len() != dst.len() {
            return Err(HudError::InvalidBuffer(format!(
                "BGRA length {} does not match surface length {}",
                bgra.len(),
                dst.len()
            )));
        }
        dst.copy_from_slice(bgra);
        // SAFETY: Flushes GDI pipeline after writing directly to DIBSection bits.
        unsafe {
            let _ = GdiFlush();
        }
        Ok(())
    }

    /// Calls `UpdateLayeredWindow` with `AC_SRC_ALPHA` and `SourceConstantAlpha = global_alpha`.
    pub fn present(
        &self,
        hwnd: HWND,
        dst_pos: Option<(i32, i32)>,
        global_alpha: u8,
        show_topmost: bool,
    ) -> bool {
        if hwnd.0.is_null() || !self.is_ready() || self.width <= 0 || self.height <= 0 {
            return false;
        }
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let pt_dst_val = dst_pos.map(|(x, y)| POINT { x, y });
        let pt_dst_ptr = pt_dst_val
            .as_ref()
            .map(|p| p as *const POINT);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: global_alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        // SAFETY: `hwnd` and `self.memdc` are valid handles on the owning UI thread.
        let ok = unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                pt_dst_ptr,
                Some(&sz),
                Some(self.memdc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .is_ok()
        };

        if ok && show_topmost {
            if let Some((x, y)) = dst_pos {
                // SAFETY: Shows the layered window non-activating and topmost.
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x,
                        y,
                        self.width,
                        self.height,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            } else {
                // SAFETY: Shows the layered window topmost without changing position.
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            }
        }
        ok
    }

    /// Releases GDI objects cleanly, restoring `old_obj` before `DeleteObject`/`DeleteDC`.
    pub fn release(&mut self) {
        // SAFETY: Restoring the original bitmap before deleting our DIBSection and memory DC.
        unsafe {
            if !self.memdc.is_invalid() && !self.old_obj.is_invalid() {
                let _ = SelectObject(self.memdc, self.old_obj);
                self.old_obj = HGDIOBJ(null_mut());
            }
            if !self.hbmp.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.hbmp.0));
                self.hbmp = HBITMAP(null_mut());
            }
            if !self.memdc.is_invalid() {
                let _ = DeleteDC(self.memdc);
                self.memdc = HDC(null_mut());
            }
        }
        self.bits_ptr = null_mut();
        self.width = 0;
        self.height = 0;
    }
}

#[cfg(windows)]
impl Default for LayeredDibSurface {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Drop for LayeredDibSurface {
    fn drop(&mut self) {
        self.release();
    }
}

/// Configuration for `LayeredWindowHost`.
#[derive(Debug, Clone)]
pub struct LayeredWindowConfig {
    pub class_prefix: &'static str,
    pub window_title: &'static str,
    pub thread_name: &'static str,
    pub screen_offset: (i32, i32),
    pub enable_wda: bool,
    pub initial_capture_visible: bool,
    pub wda_refresh_ms: u32,
    /// Opt-in resize borders + scroll-wheel callbacks while `edit_mode == true` (default `false`).
    pub resizable: bool,
    /// When `true`, clamp direct drag/resize to full monitor bounds (`rcMonitor`) instead of
    /// desktop work area (`rcWork`), so overlays on Borderless Fullscreen games can cover the
    /// bottom taskbar strip (e.g. bottom-right UID watermark).
    pub clamp_to_monitor: bool,
    pub min_size: (i32, i32),
    pub max_size: (i32, i32),
    pub resize_border_px: i32,
}

impl Default for LayeredWindowConfig {
    fn default() -> Self {
        Self {
            class_prefix: "GenshinHelperLayered",
            window_title: "GenshinHelperHUD",
            thread_name: "GH_LayeredHud",
            screen_offset: (0, 0),
            enable_wda: true,
            initial_capture_visible: false,
            wda_refresh_ms: WDA_REFRESH_MS,
            resizable: false,
            clamp_to_monitor: false,
            min_size: (220, 200),
            max_size: (720, 560),
            resize_border_px: 10,
        }
    }
}

/// Pure hit-test resolver for resizable layered windows during edit mode.
pub fn compute_resize_hit_test(screen_x: i32, screen_y: i32, rect: RectI32, border_px: i32) -> u32 {
    let b = border_px.max(4);
    let left = screen_x < rect.x + b;
    let right = screen_x >= rect.x + rect.width - b;
    let top = screen_y < rect.y + b;
    let bottom = screen_y >= rect.y + rect.height - b;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCAPTION,
    }
}

/// Clamps an interactive `WM_SIZING` rectangle to `[min_size, max_size]`, anchoring the opposite edge.
pub fn clamp_sizing_rect(
    edge: usize,
    rect: &mut RectI32,
    min_size: (i32, i32),
    max_size: (i32, i32),
) {
    let min_w = min_size.0.max(32);
    let min_h = min_size.1.max(32);
    let max_w = max_size.0.max(min_w);
    let max_h = max_size.1.max(min_h);

    let clamped_w = rect.width.clamp(min_w, max_w);
    if clamped_w != rect.width {
        if matches!(
            edge as u32,
            WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT
        ) {
            rect.x += rect.width - clamped_w;
        }
        rect.width = clamped_w;
    }
    let clamped_h = rect.height.clamp(min_h, max_h);
    if clamped_h != rect.height {
        if matches!(
            edge as u32,
            WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT
        ) {
            rect.y += rect.height - clamped_h;
        }
        rect.height = clamped_h;
    }
}

/// Pure direct-manipulation drag/resize geometry for resizable layered windows.
/// Computes the updated rectangle directly from `(start_rect, dx, dy)` so horizontal-only
/// drags NEVER alter `y`/`height` and vertical-only drags NEVER alter `x`/`width`, while
/// clamping smoothly to `[min_size, max_size]` and the monitor work area without triggering
/// Windows DWM Snap.
pub fn apply_direct_drag_resize(
    hit: u32,
    start_rect: RectI32,
    dx: i32,
    dy: i32,
    min_size: (i32, i32),
    max_size: (i32, i32),
    work_area: Option<(i32, i32, i32, i32)>,
) -> RectI32 {
    let min_w = min_size.0.max(32);
    let min_h = min_size.1.max(32);
    let max_w = max_size.0.max(min_w);
    let max_h = max_size.1.max(min_h);

    if hit == HTCAPTION {
        let mut x = start_rect.x.saturating_add(dx);
        let mut y = start_rect.y.saturating_add(dy);
        let w = start_rect.width.clamp(min_w, max_w);
        let h = start_rect.height.clamp(min_h, max_h);
        if let Some((wl, wt, wr, wb)) = work_area
            && wr > wl
            && wb > wt
        {
            x = x.clamp(wl, (wr - w).max(wl));
            y = y.clamp(wt, (wb - h).max(wt));
        }
        return RectI32 {
            x,
            y,
            width: w,
            height: h,
        };
    }

    let mut left = start_rect.x;
    let mut top = start_rect.y;
    let mut right = start_rect.x.saturating_add(start_rect.width);
    let mut bottom = start_rect.y.saturating_add(start_rect.height);

    let moves_left = matches!(hit, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT);
    let moves_right = matches!(hit, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT);
    let moves_top = matches!(hit, HTTOP | HTTOPLEFT | HTTOPRIGHT);
    let moves_bottom = matches!(hit, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT);

    if moves_left {
        let mut new_left = left.saturating_add(dx);
        if let Some((wl, _, _, _)) = work_area {
            new_left = new_left.max(wl);
        }
        let max_left = right.saturating_sub(min_w);
        let min_left = right.saturating_sub(max_w);
        left = new_left.clamp(min_left, max_left);
    } else if moves_right {
        let mut new_right = right.saturating_add(dx);
        if let Some((_, _, wr, _)) = work_area {
            new_right = new_right.min(wr);
        }
        let min_right = left.saturating_add(min_w);
        let max_right = left.saturating_add(max_w);
        right = new_right.clamp(min_right, max_right);
    }

    if moves_top {
        let mut new_top = top.saturating_add(dy);
        if let Some((_, wt, _, _)) = work_area {
            new_top = new_top.max(wt);
        }
        let max_top = bottom.saturating_sub(min_h);
        let min_top = bottom.saturating_sub(max_h);
        top = new_top.clamp(min_top, max_top);
    } else if moves_bottom {
        let mut new_bottom = bottom.saturating_add(dy);
        if let Some((_, _, _, wb)) = work_area {
            new_bottom = new_bottom.min(wb);
        }
        let min_bottom = top.saturating_add(min_h);
        let max_bottom = top.saturating_add(max_h);
        bottom = new_bottom.clamp(min_bottom, max_bottom);
    }

    RectI32 {
        x: left,
        y: top,
        width: (right - left).max(min_w),
        height: (bottom - top).max(min_h),
    }
}

/// Trait implemented by UI-thread overlay controllers (`toast`, `nav_hud`, `radar_map`) hosted in `LayeredWindowHost`.
pub trait LayeredHandler: Send + 'static {
    fn on_app_message(
        &mut self,
        _ctx: &mut LayeredWindowCtx<'_>,
        _msg: u32,
        _wparam: usize,
        _lparam: isize,
    ) -> Option<isize> {
        None
    }

    fn on_timer(&mut self, _ctx: &mut LayeredWindowCtx<'_>, _timer_id: usize) {}

    fn on_drag_start(&mut self, _ctx: &mut LayeredWindowCtx<'_>) {}

    fn on_moving(&mut self, _ctx: &mut LayeredWindowCtx<'_>, _window_rect: RectI32) {}

    fn on_sizing(&mut self, _ctx: &mut LayeredWindowCtx<'_>, _window_rect: RectI32) {}

    fn on_drag_end(&mut self, _ctx: &mut LayeredWindowCtx<'_>, _window_rect: RectI32) {}

    fn on_wheel(&mut self, _ctx: &mut LayeredWindowCtx<'_>, _delta: i16, _ctrl_held: bool) {}

    fn on_destroy(&mut self, _ctx: &mut LayeredWindowCtx<'_>) {}
}

#[cfg(windows)]
struct HostMutable {
    hwnd: isize,
    closed: bool,
    ready_done: bool,
    visible: bool,
    edit_mode: bool,
    screen_offset: (i32, i32),
    surface_size: (i32, i32),
    surface_realloc_count: usize,
    state_seq: u64,
}

#[cfg(windows)]
struct HostShared {
    state: Mutex<HostMutable>,
    ready_cv: Condvar,
    state_cv: Condvar,
    capture_visible: AtomicBool,
    wda_refresh_count: AtomicUsize,
    config: LayeredWindowConfig,
    class_name: String,
}

/// Safe UI-thread context passed to `LayeredHandler` callbacks inside `wndproc`.
#[cfg(windows)]
pub struct LayeredWindowCtx<'a> {
    hwnd: HWND,
    shared: &'a Arc<HostShared>,
    surface: &'a mut LayeredDibSurface,
    visible: &'a mut bool,
    dragging: &'a mut bool,
    edit_mode: &'a mut bool,
    click_through: &'a mut bool,
}

#[cfg(windows)]
impl<'a> LayeredWindowCtx<'a> {
    pub const fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn raw_hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub const fn is_visible(&self) -> bool {
        *self.visible
    }

    pub const fn is_dragging(&self) -> bool {
        *self.dragging
    }

    pub const fn is_edit_mode(&self) -> bool {
        *self.edit_mode
    }

    pub const fn is_click_through(&self) -> bool {
        *self.click_through
    }

    pub fn surface_size(&self) -> (i32, i32) {
        self.surface.size()
    }

    pub fn surface_realloc_count(&self) -> usize {
        self.surface.realloc_count()
    }

    pub fn screen_offset(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset
    }

    fn sync_shared_flags(&self) {
        let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
        guard.visible = *self.visible;
        guard.edit_mode = *self.edit_mode;
        guard.surface_size = self.surface.size();
        guard.surface_realloc_count = self.surface.realloc_count();
        guard.state_seq = guard.state_seq.wrapping_add(1);
        drop(guard);
        self.shared.state_cv.notify_all();
    }

    /// Toggles edit mode and updates `WS_EX_TRANSPARENT` + `SWP_FRAMECHANGED`.
    pub fn set_edit_mode(&mut self, enabled: bool) {
        *self.edit_mode = enabled;
        self.apply_click_through(!enabled);
        self.sync_shared_flags();
    }

    /// Updates `WDA_NONE` / `WDA_EXCLUDEFROMCAPTURE` immediately from the UI thread.
    pub fn set_capture_visible(&mut self, visible: bool) {
        self.shared.capture_visible.store(visible, Ordering::Release);
        if self.shared.config.enable_wda
            && !self.hwnd.0.is_null()
            && refresh_exclude_from_capture(self.hwnd.0 as isize, !visible)
        {
            self.shared.wda_refresh_count.fetch_add(1, Ordering::AcqRel);
            let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
            guard.state_seq = guard.state_seq.wrapping_add(1);
            drop(guard);
            self.shared.state_cv.notify_all();
        }
    }

    fn apply_click_through(&mut self, click_through: bool) {
        if *self.click_through == click_through || self.hwnd.0.is_null() {
            return;
        }
        let style = if click_through {
            EX_STYLE_CLICK_THROUGH
        } else {
            EX_STYLE_INTERACTIVE
        };
        // SAFETY: `self.hwnd` is a valid layered window owned by this UI thread.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style as isize);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        *self.click_through = click_through;
    }

    /// Uploads premultiplied RGBA8 pixels and presents the layered window.
    #[allow(clippy::too_many_arguments)]
    pub fn present_premultiplied_rgba(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        pm_rgba: &[u8],
        global_alpha: u8,
        keep_position: bool,
    ) -> HudResult<()> {
        self.surface.upload_premultiplied_rgba(w, h, pm_rgba)?;
        self.present_uploaded(x, y, global_alpha, keep_position)
    }

    /// Uploads straight RGBA8 pixels and presents the layered window.
    #[allow(clippy::too_many_arguments)]
    pub fn present_straight_rgba(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        rgba: &[u8],
        global_alpha: u8,
        keep_position: bool,
    ) -> HudResult<()> {
        self.surface.upload_straight_rgba(w, h, rgba)?;
        self.present_uploaded(x, y, global_alpha, keep_position)
    }

    fn present_uploaded(
        &mut self,
        x: i32,
        y: i32,
        global_alpha: u8,
        keep_position: bool,
    ) -> HudResult<()> {
        let dst_pos = if keep_position && *self.visible {
            None
        } else {
            let (ox, oy) = self.screen_offset();
            Some((x + ox, y + oy))
        };
        let show_topmost = !*self.visible;
        if self.surface.present(self.hwnd, dst_pos, global_alpha, show_topmost) {
            *self.visible = true;
            self.sync_shared_flags();
            Ok(())
        } else {
            Err(HudError::Win32("UpdateLayeredWindow failed".into()))
        }
    }

    /// Updates `SourceConstantAlpha` without re-uploading pixels (smooth toast fade-out).
    pub fn present_alpha(&mut self, global_alpha: u8) -> bool {
        self.surface.present(self.hwnd, None, global_alpha, false)
    }

    /// Moves the layered window smoothly without re-uploading pixels (instant direct drag).
    pub fn present_position(&mut self, x: i32, y: i32, global_alpha: u8) -> bool {
        let (ox, oy) = self.screen_offset();
        self.surface
            .present(self.hwnd, Some((x + ox, y + oy)), global_alpha, false)
    }

    /// Hides the layered window without destroying its DIBSection.
    pub fn hide(&mut self) {
        if !self.hwnd.0.is_null() {
            // SAFETY: Valid HWND on owning UI thread.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        *self.visible = false;
        self.sync_shared_flags();
    }

    /// Hides the layered window AND releases its GDI `DIBSection` backing memory.
    pub fn hide_and_release_surface(&mut self) {
        if !self.hwnd.0.is_null() {
            // SAFETY: Valid HWND on owning UI thread.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.surface.release();
        *self.visible = false;
        self.sync_shared_flags();
    }

    pub fn set_timer(&self, id: usize, interval_ms: u32) {
        if !self.hwnd.0.is_null() {
            // SAFETY: Valid HWND on owning UI thread.
            unsafe {
                let _ = SetTimer(Some(self.hwnd), id, interval_ms.max(1), None);
            }
        }
    }

    pub fn kill_timer(&self, id: usize) {
        if !self.hwnd.0.is_null() {
            // SAFETY: Valid HWND on owning UI thread.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), id);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg(windows)]
struct DirectDragState {
    hit: u32,
    start_cursor: (i32, i32),
    start_rect: RectI32,
    last_rect: RectI32,
    work_area: (i32, i32, i32, i32),
}

#[cfg(windows)]
struct WindowInner {
    shared: Arc<HostShared>,
    surface: LayeredDibSurface,
    visible: bool,
    dragging: bool,
    direct_drag: Option<DirectDragState>,
    edit_mode: bool,
    click_through: bool,
    wda_timer_active: bool,
    destroyed: bool,
}

#[cfg(windows)]
impl WindowInner {
    fn ctx(&mut self, hwnd: HWND) -> LayeredWindowCtx<'_> {
        LayeredWindowCtx {
            hwnd,
            shared: &self.shared,
            surface: &mut self.surface,
            visible: &mut self.visible,
            dragging: &mut self.dragging,
            edit_mode: &mut self.edit_mode,
            click_through: &mut self.click_through,
        }
    }

    fn apply_wda(&self, hwnd: HWND) {
        if !self.shared.config.enable_wda {
            return;
        }
        let exclude = !self.shared.capture_visible.load(Ordering::Acquire);
        if refresh_exclude_from_capture(hwnd.0 as isize, exclude) {
            self.shared.wda_refresh_count.fetch_add(1, Ordering::AcqRel);
            let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
            guard.state_seq = guard.state_seq.wrapping_add(1);
            drop(guard);
            self.shared.state_cv.notify_all();
        }
    }
}

#[cfg(windows)]
struct WindowState<H: LayeredHandler> {
    inner: WindowInner,
    handler: H,
}

#[cfg(windows)]
struct CreateParams<H: LayeredHandler> {
    state: Option<Box<WindowState<H>>>,
}

#[cfg(windows)]
unsafe extern "system" fn layered_wnd_proc<H: LayeredHandler>(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: `lparam` points to `CREATESTRUCTW` passed by `CreateWindowExW`.
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        if !cs.lpCreateParams.is_null() {
            // SAFETY: `lpCreateParams` is a valid `&mut CreateParams<H>` for the duration of
            // `CreateWindowExW`; `.take()` transfers `WindowState<H>` ownership to `GWLP_USERDATA`.
            let params = unsafe { &mut *(cs.lpCreateParams as *mut CreateParams<H>) };
            if let Some(state_box) = params.state.take() {
                let state_ptr = Box::into_raw(state_box);
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
                }
            }
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: Reading `WindowState<H>` pointer set during `WM_NCCREATE`.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState<H>;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    if msg == WM_NCDESTROY {
        // SAFETY: Last message for this window; reclaim boxed `WindowState<H>` and clear pointer.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let mut boxed = Box::from_raw(ptr);
            boxed.inner.surface.release();
            drop(boxed);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    }

    // SAFETY: `ptr` remains live until `WM_NCDESTROY`.
    let state = unsafe { &mut *ptr };

    match msg {
        WM_APP_CAPTURE_AFFINITY => {
            state.inner.apply_wda(hwnd);
            LRESULT(0)
        }
        WM_APP_HOST_CLOSE => {
            if state.inner.wda_timer_active {
                // SAFETY: Valid HWND on owning UI thread.
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_ID_WDA_REFRESH);
                }
                state.inner.wda_timer_active = false;
            }
            // SAFETY: Destroys our own window on the owning UI thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ID_WDA_REFRESH {
                state.inner.apply_wda(hwnd);
                if state.inner.visible {
                    // SAFETY: Re-asserts topmost z-order without activating or moving.
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                }
                return LRESULT(0);
            }
            let mut ctx = state.inner.ctx(hwnd);
            state.handler.on_timer(&mut ctx, wparam.0);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            if state.inner.edit_mode {
                if state.inner.shared.config.resizable {
                    // Use direct pointer capture (`WM_LBUTTONDOWN` + `SetCapture`) instead of
                    // `DefWindowProcW`'s modal sizing/moving loop so Windows 11 DWM edge-Snap
                    // never stretches the orthogonal axis or fights `UpdateLayeredWindow`.
                    return LRESULT(HTCLIENT as isize);
                }
                LRESULT(HTCAPTION as isize)
            } else {
                // SAFETY: Standard Win32 default hit-testing when click-through.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_SETCURSOR => {
            if state.inner.edit_mode {
                let cursor_id = if state.inner.shared.config.resizable {
                    let hit = if let Some(drag) = state.inner.direct_drag {
                        drag.hit
                    } else {
                        let mut pt = POINT::default();
                        let mut rc = RECT::default();
                        // SAFETY: Valid HWND on owning UI thread.
                        if unsafe { GetCursorPos(&mut pt).is_ok() && GetWindowRect(hwnd, &mut rc).is_ok() } {
                            let rect = RectI32 {
                                x: rc.left,
                                y: rc.top,
                                width: rc.right - rc.left,
                                height: rc.bottom - rc.top,
                            };
                            compute_resize_hit_test(
                                pt.x,
                                pt.y,
                                rect,
                                state.inner.shared.config.resize_border_px,
                            )
                        } else {
                            HTCAPTION
                        }
                    };
                    match hit {
                        HTTOPLEFT | HTBOTTOMRIGHT => IDC_SIZENWSE,
                        HTTOPRIGHT | HTBOTTOMLEFT => IDC_SIZENESW,
                        HTLEFT | HTRIGHT => IDC_SIZEWE,
                        HTTOP | HTBOTTOM => IDC_SIZENS,
                        _ => IDC_SIZEALL,
                    }
                } else {
                    IDC_SIZEALL
                };
                // SAFETY: Sets drag/resize cursor during edit mode.
                unsafe {
                    if let Ok(cur) = LoadCursorW(None, cursor_id) {
                        let _ = SetCursor(Some(cur));
                    }
                }
                LRESULT(1)
            } else {
                // SAFETY: Standard Win32 cursor fallback.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_LBUTTONDOWN => {
            if state.inner.edit_mode && state.inner.shared.config.resizable {
                let mut pt = POINT::default();
                let mut rc = RECT::default();
                // SAFETY: Valid HWND on owning UI thread.
                if unsafe { GetCursorPos(&mut pt).is_ok() && GetWindowRect(hwnd, &mut rc).is_ok() } {
                    let (ox, oy) = state
                        .inner
                        .shared
                        .state
                        .lock()
                        .expect("layered host lock poisoned")
                        .screen_offset;
                    let start_rect = RectI32 {
                        x: rc.left - ox,
                        y: rc.top - oy,
                        width: rc.right - rc.left,
                        height: rc.bottom - rc.top,
                    };
                    let screen_rect = RectI32 {
                        x: rc.left,
                        y: rc.top,
                        width: rc.right - rc.left,
                        height: rc.bottom - rc.top,
                    };
                    let hit = compute_resize_hit_test(
                        pt.x,
                        pt.y,
                        screen_rect,
                        state.inner.shared.config.resize_border_px,
                    );
                    let cx = start_rect.x + start_rect.width / 2;
                    let cy = start_rect.y + start_rect.height / 2;
                    let wa = if state.inner.shared.config.clamp_to_monitor {
                        query_monitor_bounds(cx, cy)
                    } else {
                        query_monitor_work_area(cx, cy)
                    };
                    state.inner.direct_drag = Some(DirectDragState {
                        hit,
                        start_cursor: (pt.x, pt.y),
                        start_rect,
                        last_rect: start_rect,
                        work_area: wa,
                    });
                    state.inner.dragging = true;
                    // SAFETY: Captures mouse for smooth direct drag/resize until release.
                    unsafe {
                        let _ = SetCapture(hwnd);
                    }
                    let mut ctx = state.inner.ctx(hwnd);
                    state.handler.on_drag_start(&mut ctx);
                }
                LRESULT(0)
            } else {
                // SAFETY: Standard Win32 default handling outside resizable edit mode.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_MOUSEMOVE => {
            if let Some(mut drag) = state.inner.direct_drag {
                if !state.inner.edit_mode {
                    state.inner.direct_drag = None;
                    state.inner.dragging = false;
                    // SAFETY: Releases capture if edit mode ended mid-drag.
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    return LRESULT(0);
                }
                let mut pt = POINT::default();
                // SAFETY: Plain Win32 read-only cursor query.
                if unsafe { GetCursorPos(&mut pt).is_ok() } {
                    let dx = pt.x - drag.start_cursor.0;
                    let dy = pt.y - drag.start_cursor.1;
                    let next_rect = apply_direct_drag_resize(
                        drag.hit,
                        drag.start_rect,
                        dx,
                        dy,
                        state.inner.shared.config.min_size,
                        state.inner.shared.config.max_size,
                        Some(drag.work_area),
                    );
                    if next_rect != drag.last_rect {
                        drag.last_rect = next_rect;
                        state.inner.direct_drag = Some(drag);
                        let mut ctx = state.inner.ctx(hwnd);
                        if drag.hit == HTCAPTION {
                            state.handler.on_moving(&mut ctx, next_rect);
                        } else {
                            state.handler.on_sizing(&mut ctx, next_rect);
                        }
                    }
                }
                LRESULT(0)
            } else {
                // SAFETY: Standard Win32 mouse move handling when not dragging.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED | WM_CANCELMODE => {
            if let Some(drag) = state.inner.direct_drag.take() {
                state.inner.dragging = false;
                if msg != WM_CAPTURECHANGED {
                    // SAFETY: Releases our own mouse capture.
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                let mut ctx = state.inner.ctx(hwnd);
                state.handler.on_drag_end(&mut ctx, drag.last_rect);
                LRESULT(0)
            } else {
                // SAFETY: Standard Win32 default handling.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_SIZING => {
            if state.inner.edit_mode && state.inner.shared.config.resizable && lparam.0 != 0 {
                // SAFETY: `lparam` points to a mutable `RECT` owned by Win32 during `WM_SIZING`.
                let rc = unsafe { &mut *(lparam.0 as *mut RECT) };
                let mut rect = RectI32 {
                    x: rc.left,
                    y: rc.top,
                    width: rc.right - rc.left,
                    height: rc.bottom - rc.top,
                };
                clamp_sizing_rect(
                    wparam.0,
                    &mut rect,
                    state.inner.shared.config.min_size,
                    state.inner.shared.config.max_size,
                );
                rc.left = rect.x;
                rc.top = rect.y;
                rc.right = rect.x + rect.width;
                rc.bottom = rect.y + rect.height;
                let mut ctx = state.inner.ctx(hwnd);
                state.handler.on_sizing(&mut ctx, rect);
                LRESULT(1)
            } else {
                // SAFETY: Default Win32 sizing handling when not resizable.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_MOUSEWHEEL => {
            if state.inner.edit_mode && state.inner.shared.config.resizable {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let ctrl_held = (wparam.0 & 0x0008) != 0; // MK_CONTROL
                let mut ctx = state.inner.ctx(hwnd);
                state.handler.on_wheel(&mut ctx, delta, ctrl_held);
                LRESULT(0)
            } else {
                // SAFETY: Default wheel handling outside edit mode.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_ENTERSIZEMOVE => {
            state.inner.dragging = true;
            let mut ctx = state.inner.ctx(hwnd);
            state.handler.on_drag_start(&mut ctx);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            state.inner.dragging = false;
            let mut rc = RECT::default();
            // SAFETY: Valid HWND on owning UI thread.
            let ok = unsafe { GetWindowRect(hwnd, &mut rc).is_ok() };
            let rect = if ok {
                RectI32 {
                    x: rc.left,
                    y: rc.top,
                    width: rc.right - rc.left,
                    height: rc.bottom - rc.top,
                }
            } else {
                RectI32::default()
            };
            let mut ctx = state.inner.ctx(hwnd);
            state.handler.on_drag_end(&mut ctx, rect);
            LRESULT(0)
        }
        WM_DESTROY => {
            if state.inner.wda_timer_active {
                // SAFETY: Valid HWND on owning UI thread.
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_ID_WDA_REFRESH);
                }
                state.inner.wda_timer_active = false;
            }
            if !state.inner.destroyed {
                state.inner.destroyed = true;
                let mut ctx = state.inner.ctx(hwnd);
                state.handler.on_destroy(&mut ctx);
            }
            state.inner.surface.release();
            state.inner.visible = false;
            {
                let mut guard = state
                    .inner
                    .shared
                    .state
                    .lock()
                    .expect("layered host lock poisoned");
                guard.visible = false;
                guard.surface_size = (0, 0);
            }
            // SAFETY: Terminates the dedicated UI thread message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ if (WM_APP..=0xBFFF).contains(&msg) => {
            let mut ctx = state.inner.ctx(hwnd);
            if let Some(res) = state
                .handler
                .on_app_message(&mut ctx, msg, wparam.0, lparam.0)
            {
                LRESULT(res)
            } else {
                // SAFETY: Default handling for unhandled app messages.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        // SAFETY: Standard Win32 default window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Thread-safe Win32 layered window host with lazy UI-thread startup and clean shutdown.
#[cfg(windows)]
pub struct LayeredWindowHost {
    shared: Arc<HostShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[cfg(windows)]
impl LayeredWindowHost {
    /// Creates a new `LayeredWindowHost` WITHOUT spawning any OS thread or HWND yet.
    pub fn new(config: LayeredWindowConfig) -> Self {
        let seq = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let class_name = format!("{}_{seq}", config.class_prefix);
        let env_offscreen = is_hud_offscreen_env();
        let initial_offset = resolve_screen_offset(config.screen_offset, env_offscreen);
        let initial_cap = config.initial_capture_visible;

        Self {
            shared: Arc::new(HostShared {
                state: Mutex::new(HostMutable {
                    hwnd: 0,
                    closed: false,
                    ready_done: false,
                    visible: false,
                    edit_mode: false,
                    screen_offset: initial_offset,
                    surface_size: (0, 0),
                    surface_realloc_count: 0,
                    state_seq: 0,
                }),
                ready_cv: Condvar::new(),
                state_cv: Condvar::new(),
                capture_visible: AtomicBool::new(initial_cap),
                wda_refresh_count: AtomicUsize::new(0),
                config,
                class_name,
            }),
            thread: Mutex::new(None),
        }
    }

    /// Lazily spawns the UI thread and creates the `WS_EX_LAYERED` window if not yet running.
    pub fn ensure_started<H, F>(&self, make_handler: F) -> Option<isize>
    where
        H: LayeredHandler,
        F: FnOnce() -> H + Send + 'static,
    {
        {
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.closed {
                return None;
            }
            if guard.hwnd != 0 {
                return Some(guard.hwnd);
            }
        }

        let mut thread_guard = self.thread.lock().expect("layered thread lock poisoned");
        {
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.closed {
                return None;
            }
            if guard.hwnd != 0 {
                return Some(guard.hwnd);
            }
        }

        let need_spawn = match thread_guard.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if need_spawn {
            if let Some(old) = thread_guard.take() {
                let _ = old.join();
            }
            {
                let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
                guard.ready_done = false;
            }
            let shared = Arc::clone(&self.shared);
            let thread_name = self.shared.config.thread_name.to_owned();
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || Self::ui_thread_main(shared, make_handler))
                .ok()?;
            *thread_guard = Some(handle);
        }
        drop(thread_guard);

        let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
        let wait_res = self
            .shared
            .ready_cv
            .wait_timeout_while(guard, Duration::from_secs(2), |st| !st.ready_done)
            .expect("layered ready wait failed");
        guard = wait_res.0;
        if guard.hwnd != 0 {
            Some(guard.hwnd)
        } else {
            None
        }
    }

    fn ui_thread_main<H, F>(shared: Arc<HostShared>, make_handler: F)
    where
        H: LayeredHandler,
        F: FnOnce() -> H,
    {
        ensure_dpi_awareness();
        ensure_default_desktop();

        let Ok(hmod) = (unsafe { GetModuleHandleW(None) }) else {
            let mut guard = shared.state.lock().expect("layered host lock poisoned");
            guard.ready_done = true;
            shared.ready_cv.notify_all();
            return;
        };
        let hinst: HINSTANCE = hmod.into();

        let class_w: Vec<u16> = shared
            .class_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let title_w: Vec<u16> = shared
            .config
            .window_title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let wc = WNDCLASSW {
            lpfnWndProc: Some(layered_wnd_proc::<H>),
            hInstance: hinst,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            ..Default::default()
        };

        // SAFETY: Registering unique per-instance window class on UI thread.
        let atom = unsafe { RegisterClassW(&wc) };
        if atom == 0 {
            let mut guard = shared.state.lock().expect("layered host lock poisoned");
            guard.ready_done = true;
            shared.ready_cv.notify_all();
            return;
        }

        let state_box = Box::new(WindowState {
            inner: WindowInner {
                shared: Arc::clone(&shared),
                surface: LayeredDibSurface::new(),
                visible: false,
                dragging: false,
                direct_drag: None,
                edit_mode: false,
                click_through: true,
                wda_timer_active: false,
                destroyed: false,
            },
            handler: make_handler(),
        });
        let params_ptr = Box::into_raw(Box::new(CreateParams {
            state: Some(state_box),
        }));

        let (init_x, init_y) = shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset;

        // SAFETY: `CreateWindowExW` invokes `WM_NCCREATE` synchronously with `params_ptr`,
        // which `.take()`s `WindowState<H>` on success; `params_ptr` is reclaimed below.
        let hwnd_res = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(EX_STYLE_CLICK_THROUGH),
                PCWSTR(class_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                WS_POPUP,
                init_x,
                init_y,
                10,
                10,
                None,
                None,
                Some(hinst),
                Some(params_ptr.cast::<c_void>()),
            )
        };
        // SAFETY: `CreateWindowExW` has finished synchronously dispatching `WM_NCCREATE`;
        // reclaim `CreateParams` (freeing `WindowState<H>` only if `WM_NCCREATE` never took it).
        drop(unsafe { Box::from_raw(params_ptr) });

        let hwnd = match hwnd_res {
            Ok(h) if !h.0.is_null() => h,
            _ => {
                // SAFETY: Class was registered on this thread and window creation failed.
                unsafe {
                    let _ = UnregisterClassW(PCWSTR(class_w.as_ptr()), Some(hinst));
                }
                let mut guard = shared.state.lock().expect("layered host lock poisoned");
                guard.ready_done = true;
                shared.ready_cv.notify_all();
                return;
            }
        };

        if shared.config.enable_wda {
            let exclude = !shared.capture_visible.load(Ordering::Acquire);
            if refresh_exclude_from_capture(hwnd.0 as isize, exclude) {
                shared.wda_refresh_count.fetch_add(1, Ordering::AcqRel);
            }
            if shared.config.wda_refresh_ms > 0 {
                // SAFETY: `hwnd` is a valid window owned by this UI thread.
                unsafe {
                    let _ = SetTimer(
                        Some(hwnd),
                        TIMER_ID_WDA_REFRESH,
                        shared.config.wda_refresh_ms,
                        None,
                    );
                    // Mark active directly inside `WindowState` if pointer is still stored.
                    let cur = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState<H>;
                    if !cur.is_null() {
                        (*cur).inner.wda_timer_active = true;
                    }
                }
            }
        }

        let closed_during_startup = {
            let mut guard = shared.state.lock().expect("layered host lock poisoned");
            if guard.closed {
                guard.hwnd = 0;
                guard.ready_done = true;
                shared.ready_cv.notify_all();
                true
            } else {
                guard.hwnd = hwnd.0 as isize;
                guard.ready_done = true;
                shared.ready_cv.notify_all();
                false
            }
        };

        if closed_during_startup {
            // SAFETY: `close()` raced before `hwnd` was published; destroy the window on its
            // owning UI thread so `WM_DESTROY` -> `PostQuitMessage(0)` drains the message loop.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }

        let mut msg = MSG::default();
        // SAFETY: Standard Win32 message loop on UI thread until `PostQuitMessage`.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnregisterClassW(PCWSTR(class_w.as_ptr()), Some(hinst));
        }

        let mut guard = shared.state.lock().expect("layered host lock poisoned");
        guard.hwnd = 0;
        guard.visible = false;
        guard.state_seq = guard.state_seq.wrapping_add(1);
        drop(guard);
        shared.state_cv.notify_all();
    }

    /// Waits on UI-thread state notifications until `pred()` returns `true` or `timeout` elapses.
    pub fn wait_until_state<F>(&self, timeout: Duration, mut pred: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let seq = self
                .shared
                .state
                .lock()
                .expect("layered host lock poisoned")
                .state_seq;
            if pred() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.state_seq != seq {
                continue;
            }
            let _ = self
                .shared
                .state_cv
                .wait_timeout(guard, remaining)
                .expect("layered host condvar poisoned");
        }
    }

    pub fn post_message(&self, msg: u32, wparam: usize, lparam: isize) -> bool {
        let hwnd = {
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.closed || guard.hwnd == 0 {
                return false;
            }
            guard.hwnd
        };
        // SAFETY: `PostMessageW` safely queues onto the UI thread's message loop.
        unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), msg, WPARAM(wparam), LPARAM(lparam)).is_ok() }
    }

    /// Updates `capture_visible` and posts `WM_APP_CAPTURE_AFFINITY` (`0x8015`) to the window.
    pub fn set_capture_visible(&self, visible: bool) -> bool {
        self.shared.capture_visible.store(visible, Ordering::Release);
        let hwnd = self.raw_hwnd().unwrap_or(0);
        if hwnd != 0 {
            self.post_message(WM_APP_CAPTURE_AFFINITY, 0, 0)
        } else {
            true
        }
    }

    pub fn capture_visible(&self) -> bool {
        self.shared.capture_visible.load(Ordering::Acquire)
    }

    pub fn wda_refresh_count(&self) -> usize {
        self.shared.wda_refresh_count.load(Ordering::Acquire)
    }

    pub fn raw_hwnd(&self) -> Option<isize> {
        let hwnd = self
            .shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .hwnd;
        (hwnd != 0).then_some(hwnd)
    }

    pub fn is_visible(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .visible
    }

    pub fn is_edit_mode(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .edit_mode
    }

    pub fn is_thread_running(&self) -> bool {
        let guard = self.thread.lock().expect("layered thread lock poisoned");
        guard.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn set_screen_offset(&self, offset: (i32, i32)) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset = offset;
    }

    pub fn screen_offset(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset
    }

    pub fn surface_size(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .surface_size
    }

    pub fn surface_realloc_count(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .surface_realloc_count
    }

    /// Cleanly tears down the layered window, releases all GDI surfaces, and joins the UI thread.
    pub fn close(&self) {
        let hwnd = {
            let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
            guard.closed = true;
            guard.hwnd
        };
        if hwnd != 0 {
            // SAFETY: Requests window destruction on the owning UI thread.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_APP_HOST_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        let handle = self
            .thread
            .lock()
            .expect("layered thread lock poisoned")
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
        guard.hwnd = 0;
        guard.visible = false;
    }
}

#[cfg(windows)]
impl Drop for LayeredWindowHost {
    fn drop(&mut self) {
        self.close();
    }
}

// Compile-time check against forbidden SendInput or CAPTUREBLT constants.
const _: () = {
    assert!(CAPTURE_AFFINITY_MSG == 0x8015);
    assert!(WDA_EXCLUDEFROMCAPTURE == 0x11);
    assert!(WDA_NONE == 0x00);
    assert!(CAPTURE_ROP_GUARD == 0x00CC_0020);
};

#[cfg(all(test, windows))]
pub(crate) fn assert_hwnd_offscreen_from_virtual_desktop(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    let hw = HWND(hwnd as *mut c_void);
    let mut rc = RECT::default();
    // SAFETY: Valid live top-level HWND owned by test; querying window bounds on Windows.
    let ok = unsafe { GetWindowRect(hw, &mut rc).is_ok() };
    assert!(ok, "GetWindowRect failed for hwnd={hwnd}");

    // SAFETY: Reading Win32 virtual screen metrics.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    assert!(vw > 0 && vh > 0, "virtual screen dims must be positive: {vw}x{vh}");
    let no_overlap =
        rc.right <= vx || rc.left >= vx + vw || rc.bottom <= vy || rc.top >= vy + vh;
    assert!(
        no_overlap,
        "HWND {hwnd} rect ({},{})..({},{}) overlaps virtual desktop ({},{})..({},{})",
        rc.left,
        rc.top,
        rc.right,
        rc.bottom,
        vx,
        vy,
        vx + vw,
        vy + vh
    );
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use gh_win32::game_settings::get_window_display_affinity;
    use std::sync::mpsc;
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources,
    };

    #[test]
    fn resolve_screen_offset_respects_explicit_and_env_guard() {
        assert_eq!(resolve_screen_offset((0, 0), false), (0, 0));
        assert_eq!(
            resolve_screen_offset((0, 0), true),
            OFFSCREEN_TEST_OFFSET
        );
        assert_eq!(resolve_screen_offset((10, 20), true), (10, 20));
        assert_eq!(
            resolve_screen_offset((-500, -500), false),
            (-500, -500)
        );
        assert!(!env_flag_truthy(None));
        assert!(!env_flag_truthy(Some("")));
        assert!(!env_flag_truthy(Some("0")));
        assert!(env_flag_truthy(Some("1")));
        assert!(env_flag_truthy(Some(" True ")));
        assert!(env_flag_truthy(Some("yes")));
        assert!(env_flag_truthy(Some("ON")));
    }

    #[test]
    fn premultiplied_bgra_conversion_matches_python_and_invariants() {
        let mut rgba = Vec::new();
        for a in [0u8, 1, 64, 127, 128, 200, 254, 255] {
            rgba.extend_from_slice(&[255, 128, 37, a]);
        }
        let bgra = rgba_to_premultiplied_bgra(&rgba).expect("straight RGBA->BGRA");
        assert_eq!(bgra.len(), rgba.len());

        for (src, dst) in rgba.as_chunks::<4>().0.iter().zip(bgra.as_chunks::<4>().0.iter()) {
            let a = src[3];
            assert_eq!(dst[3], a);
            assert!(dst[0] <= a && dst[1] <= a && dst[2] <= a);
            let exp_r = ((src[0] as u16 * a as u16 + 127) / 255) as u8;
            let exp_g = ((src[1] as u16 * a as u16 + 127) / 255) as u8;
            let exp_b = ((src[2] as u16 * a as u16 + 127) / 255) as u8;
            assert_eq!(dst, &[exp_b, exp_g, exp_r, a]);
        }

        // Premultiplied RGBA (`tiny-skia`) swizzle & defensive clamp
        let pm_in = [200u8, 100, 50, 120, 40, 80, 10, 200];
        let pm_bgra = premultiplied_rgba_to_bgra(&pm_in).expect("pm RGBA->BGRA");
        assert_eq!(pm_bgra, vec![50, 100, 120, 120, 10, 80, 40, 200]);
        for px in pm_bgra.as_chunks::<4>().0 {
            assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
        }
    }

    #[test]
    fn monitor_work_area_returns_valid_non_empty_rect() {
        let (l, t, r, b) = query_monitor_work_area(100, 100);
        assert!(r > l, "expected right ({r}) > left ({l})");
        assert!(b > t, "expected bottom ({b}) > top ({t})");
        let (ml, mt, mr, mb) = query_monitor_bounds(100, 100);
        assert!(mr > ml && mb > mt);
        assert!(ml <= l && mt <= t && mr >= r && mb >= b);
    }

    const MSG_TEST_PRESENT: u32 = WM_APP + 1;
    const MSG_TEST_ALPHA: u32 = WM_APP + 2;
    const MSG_TEST_EDIT: u32 = WM_APP + 3;
    const MSG_TEST_HIDE: u32 = WM_APP + 4;

    struct TestHandler {
        ack_tx: mpsc::Sender<&'static str>,
    }

    impl LayeredHandler for TestHandler {
        fn on_app_message(
            &mut self,
            ctx: &mut LayeredWindowCtx<'_>,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> Option<isize> {
            match msg {
                MSG_TEST_PRESENT => {
                    let w = wparam as i32;
                    let h = lparam as i32;
                    let mut pm = vec![0u8; (w * h * 4) as usize];
                    for px in pm.as_chunks_mut::<4>().0 {
                        px.copy_from_slice(&[34, 197, 94, 220]);
                    }
                    let _ = ctx.present_premultiplied_rgba(10, 20, w, h, &pm, 255, false);
                    let _ = self.ack_tx.send("present");
                    Some(0)
                }
                MSG_TEST_ALPHA => {
                    let _ = ctx.present_alpha((wparam & 0xFF) as u8);
                    let _ = self.ack_tx.send("alpha");
                    Some(0)
                }
                MSG_TEST_EDIT => {
                    ctx.set_edit_mode(wparam != 0);
                    let _ = self.ack_tx.send("edit");
                    Some(0)
                }
                MSG_TEST_HIDE => {
                    ctx.hide();
                    let _ = self.ack_tx.send("hide");
                    Some(0)
                }
                _ => None,
            }
        }
    }

    fn gui_counts() -> (u32, u32) {
        // SAFETY: Process pseudo-handle query for GDI/USER object leak assertions.
        unsafe {
            let proc = GetCurrentProcess();
            (
                GetGuiResources(proc, GR_GDIOBJECTS),
                GetGuiResources(proc, GR_USEROBJECTS),
            )
        }
    }

    #[test]
    fn layered_host_offscreen_wda_dib_reuse_edit_mode_and_zero_leaks() {
        let host = LayeredWindowHost::new(LayeredWindowConfig {
            class_prefix: "GhHudLayeredTest",
            window_title: "GhHudLayeredTest",
            thread_name: "GH_LayeredTest",
            screen_offset: (-20000, -20000),
            enable_wda: true,
            initial_capture_visible: false,
            wda_refresh_ms: 80,
            ..LayeredWindowConfig::default()
        });

        // Lazy startup invariant: no thread or HWND until `ensure_started`.
        assert!(!host.is_thread_running());
        assert_eq!(host.raw_hwnd(), None);

        // Warm up baseline after process-wide lazy Win32 initialization.
        let (tx, rx) = mpsc::channel();
        let hwnd = host
            .ensure_started(move || TestHandler { ack_tx: tx })
            .expect("off-screen layered window must start");
        assert!(host.is_thread_running());
        assert_eq!(host.raw_hwnd(), Some(hwnd));

        // Verify EX_STYLE matches click-through topmost non-activating toolwindow.
        let ex_style =
            unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
        assert_eq!(ex_style, EX_STYLE_CLICK_THROUGH);

        // Verify initial WDA_EXCLUDEFROMCAPTURE (0x11) and 0x8015 toggle to WDA_NONE (0x00).
        assert_eq!(
            get_window_display_affinity(hwnd),
            Some(WDA_EXCLUDEFROMCAPTURE)
        );
        let init_refreshes = host.wda_refresh_count();
        assert!(init_refreshes >= 1);

        assert!(host.set_capture_visible(true));
        assert!(
            host.wait_until_state(Duration::from_secs(15), || {
                host.wda_refresh_count() > init_refreshes
                    && get_window_display_affinity(hwnd) == Some(WDA_NONE)
            }),
            "timed out waiting for WDA_NONE"
        );
        assert_eq!(get_window_display_affinity(hwnd), Some(WDA_NONE));
        assert!(host.wda_refresh_count() > init_refreshes);

        let after_none = host.wda_refresh_count();
        assert!(host.set_capture_visible(false));
        assert!(
            host.wait_until_state(Duration::from_secs(15), || {
                host.wda_refresh_count() > after_none
                    && get_window_display_affinity(hwnd) == Some(WDA_EXCLUDEFROMCAPTURE)
            }),
            "timed out waiting for WDA_EXCLUDEFROMCAPTURE"
        );
        assert_eq!(
            get_window_display_affinity(hwnd),
            Some(WDA_EXCLUDEFROMCAPTURE)
        );

        // Wait for timer-driven WDA refresh (80ms period).
        let before_timer = host.wda_refresh_count();
        assert!(
            host.wait_until_state(Duration::from_secs(15), || {
                host.wda_refresh_count() > before_timer
            }),
            "timed out waiting for timer-driven WDA refresh"
        );
        assert!(host.wda_refresh_count() > before_timer);

        // Snapshot resource count after initial window/DIB creation & present.
        assert!(host.post_message(MSG_TEST_PRESENT, 128, 48));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("present ack"),
            "present"
        );
        let (gdi_base, user_base) = gui_counts();

        // Presenting same size reuses DIBSection (no reallocation!).
        let reallocs_after_first = host.surface_realloc_count();
        assert_eq!(reallocs_after_first, 1);
        assert!(host.is_visible());
        assert_eq!(host.surface_size(), (128, 48));
        assert_hwnd_offscreen_from_virtual_desktop(hwnd);

        assert!(host.post_message(MSG_TEST_PRESENT, 128, 48));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("reuse ack"),
            "present"
        );
        assert_eq!(host.surface_realloc_count(), reallocs_after_first);

        // Alpha update without pixel re-upload.
        assert!(host.post_message(MSG_TEST_ALPHA, 128, 0));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("alpha ack"),
            "alpha"
        );
        assert_eq!(host.surface_realloc_count(), reallocs_after_first);

        // Resizing reallocates DIBSection cleanly.
        assert!(host.post_message(MSG_TEST_PRESENT, 240, 80));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("resize ack"),
            "present"
        );
        assert_eq!(host.surface_size(), (240, 80));
        assert_hwnd_offscreen_from_virtual_desktop(hwnd);
        assert_eq!(host.surface_realloc_count(), reallocs_after_first + 1);

        // Edit mode clears WS_EX_TRANSPARENT; disabling restores it.
        assert!(host.post_message(MSG_TEST_EDIT, 1, 0));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("edit on ack"),
            "edit"
        );
        let style_edit =
            unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
        assert_eq!(style_edit & WS_EX_TRANSPARENT.0, 0);
        assert!(host.is_edit_mode());

        assert!(host.post_message(MSG_TEST_EDIT, 0, 0));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("edit off ack"),
            "edit"
        );
        let style_ct =
            unsafe { GetWindowLongPtrW(HWND(hwnd as *mut c_void), GWL_EXSTYLE) } as u32;
        assert_ne!(style_ct & WS_EX_TRANSPARENT.0, 0);
        assert!(!host.is_edit_mode());

        // Hide & close cleanly.
        assert!(host.post_message(MSG_TEST_HIDE, 0, 0));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).expect("hide ack"),
            "hide"
        );
        assert!(!host.is_visible());

        host.close();
        assert_eq!(host.raw_hwnd(), None);
        assert!(!host.is_visible());
        assert!(!host.is_thread_running());

        let (gdi_after, user_after) = gui_counts();
        assert!(
            gdi_after <= gdi_base,
            "GDI handle leak: before={gdi_base}, after={gdi_after}"
        );
        assert!(
            user_after <= user_base,
            "USER handle leak: before={user_base}, after={user_after}"
        );
    }

    #[test]
    fn resize_hit_test_and_sizing_clamp_behave_accurately() {
        let rect = RectI32 {
            x: 100,
            y: 200,
            width: 400,
            height: 300,
        };
        assert_eq!(compute_resize_hit_test(103, 203, rect, 10), HTTOPLEFT);
        assert_eq!(compute_resize_hit_test(496, 204, rect, 10), HTTOPRIGHT);
        assert_eq!(compute_resize_hit_test(102, 496, rect, 10), HTBOTTOMLEFT);
        assert_eq!(compute_resize_hit_test(495, 495, rect, 10), HTBOTTOMRIGHT);
        assert_eq!(compute_resize_hit_test(102, 350, rect, 10), HTLEFT);
        assert_eq!(compute_resize_hit_test(497, 350, rect, 10), HTRIGHT);
        assert_eq!(compute_resize_hit_test(300, 202, rect, 10), HTTOP);
        assert_eq!(compute_resize_hit_test(300, 496, rect, 10), HTBOTTOM);
        assert_eq!(compute_resize_hit_test(300, 350, rect, 10), HTCAPTION);

        let mut too_small_left = RectI32 {
            x: 350,
            y: 200,
            width: 150,
            height: 280,
        };
        clamp_sizing_rect(WMSZ_LEFT as usize, &mut too_small_left, (220, 200), (720, 560));
        assert_eq!(too_small_left.width, 220);
        assert_eq!(too_small_left.x + too_small_left.width, 500);

        let mut too_big_bottom_right = RectI32 {
            x: 100,
            y: 200,
            width: 900,
            height: 800,
        };
        clamp_sizing_rect(
            windows::Win32::UI::WindowsAndMessaging::WMSZ_BOTTOMRIGHT as usize,
            &mut too_big_bottom_right,
            (220, 200),
            (720, 560),
        );
        assert_eq!(too_big_bottom_right.x, 100);
        assert_eq!(too_big_bottom_right.y, 200);
        assert_eq!((too_big_bottom_right.width, too_big_bottom_right.height), (720, 560));
    }

    #[test]
    fn apply_direct_drag_resize_never_stretches_orthogonal_axis_at_screen_edges() {
        let wa = (0, 0, 1920, 1080);
        let start = RectI32 {
            x: 1450,
            y: 750,
            width: 460,
            height: 320,
        };
        // Dragging HTRIGHT far past the right screen edge must clamp width to monitor edge
        // and NEVER alter `y` or `height`.
        let r_right = apply_direct_drag_resize(
            HTRIGHT,
            start,
            500,
            300,
            (220, 200),
            (720, 560),
            Some(wa),
        );
        assert_eq!(r_right.x, 1450);
        assert_eq!(r_right.y, 750);
        assert_eq!(r_right.height, 320);
        assert_eq!(r_right.x + r_right.width, 1920);

        // Dragging HTBOTTOM far past bottom screen edge must NEVER alter `x` or `width`.
        let r_bottom = apply_direct_drag_resize(
            HTBOTTOM,
            start,
            -400,
            400,
            (220, 200),
            (720, 560),
            Some(wa),
        );
        assert_eq!(r_bottom.x, 1450);
        assert_eq!(r_bottom.width, 460);
        assert_eq!(r_bottom.y, 750);
        assert_eq!(r_bottom.y + r_bottom.height, 1080);

        // Pure move (HTCAPTION) clamps inside monitor work area and keeps size exact.
        let r_move = apply_direct_drag_resize(
            HTCAPTION,
            start,
            -2000,
            -1000,
            (220, 200),
            (720, 560),
            Some(wa),
        );
        assert_eq!(r_move, RectI32 { x: 0, y: 0, width: 460, height: 320 });
    }
}
