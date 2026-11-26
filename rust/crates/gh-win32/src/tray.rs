//! Win32 `Shell_NotifyIconW` system tray with embedded `.ico` and `TaskbarCreated` recovery (`src/tray.py`).

use gh_core::i18n::t_lang;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON, IDI_APPLICATION,
    LR_DEFAULTCOLOR, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow,
    SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_NCCREATE,
    WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_USER, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

pub const WM_TRAYICON: u32 = WM_USER + 20;
pub const WM_APP_UPDATE_STATE: u32 = WM_APP + 10;
pub const WM_APP_CLOSE: u32 = WM_APP + 11;

pub const ID_TOGGLE_ENGINE: u32 = 1001;
pub const ID_OPEN_MAP: u32 = 1002;
pub const ID_OPEN_DIAG: u32 = 1003;
pub const ID_OPEN_CONFIG: u32 = 1004;
pub const ID_QUIT: u32 = 1005;
pub const ID_LAUNCH_BORDERLESS: u32 = 1006;
pub const ID_SUPPORT: u32 = 1007;
pub const ID_CHECK_UPDATE: u32 = 1008;
pub const ID_STOP_SESSION_REC: u32 = 1009;

const ICON_CONNECTED_BYTES: &[u8] = include_bytes!("../../../../assets/icons/tray_connected.ico");
const ICON_IDLE_BYTES: &[u8] = include_bytes!("../../../../assets/icons/tray_default.ico");
const ICON_PAUSED_BYTES: &[u8] = include_bytes!("../../../../assets/icons/tray_paused.ico");
const ICON_OFFLINE_BYTES: &[u8] = include_bytes!("../../../../assets/icons/tray_disconnected.ico");

static TRAY_INSTANCE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayState {
    Connected,
    Idle,
    Paused,
    Offline,
}

impl TrayState {
    pub const ALL: [Self; 4] = [Self::Connected, Self::Idle, Self::Paused, Self::Offline];

    /// Parses a state string (`"connected" | "idle" | "paused" | "offline"`), normalizing
    /// unknown values to `TrayState::Idle` just like `src/tray.py`.
    pub fn from_str_lossy(raw: &str) -> Self {
        match raw {
            "connected" => Self::Connected,
            "paused" => Self::Paused,
            "offline" => Self::Offline,
            _ => Self::Idle,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Offline => "offline",
        }
    }

    pub const fn icon_filename(self) -> &'static str {
        match self {
            Self::Connected => "tray_connected.ico",
            Self::Idle => "tray_default.ico",
            Self::Paused => "tray_paused.ico",
            Self::Offline => "tray_disconnected.ico",
        }
    }

    pub const fn embedded_ico_bytes(self) -> &'static [u8] {
        match self {
            Self::Connected => ICON_CONNECTED_BYTES,
            Self::Idle => ICON_IDLE_BYTES,
            Self::Paused => ICON_PAUSED_BYTES,
            Self::Offline => ICON_OFFLINE_BYTES,
        }
    }

    pub const fn tooltip_key(self) -> &'static str {
        match self {
            Self::Connected => "tray.tooltip.connected",
            Self::Idle => "tray.tooltip.idle",
            Self::Paused => "tray.tooltip.paused",
            Self::Offline => "tray.tooltip.offline",
        }
    }

    pub const fn default_tooltip(self) -> &'static str {
        match self {
            Self::Connected => "Genshin Helper v5.0 — Đang kết nối Web Map",
            Self::Idle => "Genshin Helper v5.0 — Đang chạy (Chờ mở HoYoLAB Map)",
            Self::Paused => "Genshin Helper v5.0 — Tạm dừng (Standby 0% CPU)",
            Self::Offline => "Genshin Helper v5.0 — Offline",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuItem {
    pub cmd_id: u32,
    pub label: String,
    pub is_separator: bool,
}

/// Returns localized tooltip string for `state`, optionally appending UIPI Admin warning.
pub fn get_tooltip(state: TrayState, lang: Option<&str>, uipi_warning: bool) -> String {
    let key = state.tooltip_key();
    let looked = t_lang(key, lang, &[]);
    let base = if looked.is_empty() || looked == key {
        state.default_tooltip().to_owned()
    } else {
        looked
    };
    append_uipi_warning(base, lang, uipi_warning)
}

/// Convenience string-based wrapper for `get_tooltip` (unknown states normalize to `"idle"`).
pub fn get_tooltip_str(state: &str, lang: Option<&str>, uipi_warning: bool) -> String {
    get_tooltip(TrayState::from_str_lossy(state), lang, uipi_warning)
}

fn append_uipi_warning(base: String, lang: Option<&str>, uipi_warning: bool) -> String {
    if !uipi_warning {
        return base;
    }
    let warn_key = "uipi.warning.short";
    let warn = t_lang(warn_key, lang, &[]);
    if !warn.is_empty() && warn != warn_key {
        return format!("{base}\n⚠ {warn}");
    }
    base
}

/// Returns localized tray menu entries matching `src/tray.py::SystemTrayApp.build_menu_items`.
pub fn build_menu_items(engine_active: bool, lang: Option<&str>) -> Vec<TrayMenuItem> {
    build_menu_items_with_update(engine_active, lang, None)
}

/// Returns localized tray menu entries, dynamically highlighting an available or ready-to-install update.
pub fn build_menu_items_with_update(
    engine_active: bool,
    lang: Option<&str>,
    available_update: Option<&str>,
) -> Vec<TrayMenuItem> {
    build_menu_items_with_update_state(engine_active, lang, available_update, false)
}

/// Returns localized tray menu entries with distinct labels for available vs downloaded & verified updates.
pub fn build_menu_items_with_update_state(
    engine_active: bool,
    lang: Option<&str>,
    available_update: Option<&str>,
    ready_to_restart: bool,
) -> Vec<TrayMenuItem> {
    let toggle_label = if engine_active {
        t_lang("tray.menu.pause", lang, &[])
    } else {
        t_lang("tray.menu.resume", lang, &[])
    };
    let update_label = match available_update.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ver) => {
            let clean = ver.trim_start_matches(['v', 'V']);
            let key = if ready_to_restart {
                "tray.menu.update_ready"
            } else {
                "tray.menu.update_available"
            };
            t_lang(key, lang, &[("latest", clean)])
        }
        None => t_lang("tray.menu.check_update", lang, &[]),
    };

    vec![
        TrayMenuItem {
            cmd_id: ID_TOGGLE_ENGINE,
            label: toggle_label,
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: ID_OPEN_MAP,
            label: t_lang("tray.menu.open_map", lang, &[]),
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: ID_LAUNCH_BORDERLESS,
            label: t_lang("tray.menu.launch_borderless", lang, &[]),
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: 0,
            label: String::new(),
            is_separator: true,
        },
        TrayMenuItem {
            cmd_id: ID_OPEN_DIAG,
            label: t_lang("tray.menu.open_diag", lang, &[]),
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: ID_OPEN_CONFIG,
            label: t_lang("tray.menu.open_config", lang, &[]),
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: 0,
            label: String::new(),
            is_separator: true,
        },
        TrayMenuItem {
            cmd_id: ID_CHECK_UPDATE,
            label: update_label,
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: ID_SUPPORT,
            label: t_lang("tray.menu.support", lang, &[]),
            is_separator: false,
        },
        TrayMenuItem {
            cmd_id: 0,
            label: String::new(),
            is_separator: true,
        },
        TrayMenuItem {
            cmd_id: ID_QUIT,
            label: t_lang("tray.menu.quit", lang, &[]),
            is_separator: false,
        },
    ]
}

/// Encodes `tip` into a NUL-terminated `[u16; 128]` buffer (`NOTIFYICONDATAW.szTip`),
/// clamping to at most `127` UTF-16 code units without leaving an unpaired high surrogate.
pub fn encode_sz_tip(tip: &str) -> [u16; 128] {
    let mut buf = [0u16; 128];
    let mut len = 0usize;
    for unit in tip.encode_utf16() {
        if len >= 127 {
            break;
        }
        buf[len] = unit;
        len += 1;
    }
    if len == 127 && (0xD800..=0xDBFF).contains(&buf[126]) {
        buf[126] = 0;
    }
    buf
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcoFrameInfo {
    pub width: i32,
    pub height: i32,
    pub offset: usize,
    pub size: usize,
}

/// Parses an `.ico` `ICONDIR` (6 bytes) + `ICONDIRENTRY` table (`16 * count` bytes) directly
/// and returns the best frame slice for `desired_cx` x `desired_cy`.
///
/// Important: Vista+ `.ico` files with PNG payloads use 16-byte `ICONDIRENTRY` headers, so
/// raw `.ico` bytes MUST NOT be passed to `LookupIconIdFromDirectoryEx` (which expects 14-byte
/// `GRPICONDIRENTRY` resource entries).
pub fn parse_ico_best_frame(ico: &[u8], desired_cx: i32, desired_cy: i32) -> Option<(IcoFrameInfo, &[u8])> {
    if ico.len() < 6 {
        return None;
    }
    let reserved = u16::from_le_bytes([ico[0], ico[1]]);
    let id_type = u16::from_le_bytes([ico[2], ico[3]]);
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if reserved != 0 || id_type != 1 || count == 0 {
        return None;
    }
    let dir_end = 6usize.checked_add(count.checked_mul(16)?)?;
    if ico.len() < dir_end {
        return None;
    }

    let want_w = desired_cx.max(1);
    let want_h = desired_cy.max(1);
    let mut candidates: Vec<IcoFrameInfo> = Vec::with_capacity(count);
    for i in 0..count {
        let base = 6 + i * 16;
        let w = if ico[base] == 0 { 256 } else { i32::from(ico[base]) };
        let h = if ico[base + 1] == 0 { 256 } else { i32::from(ico[base + 1]) };
        let size = u32::from_le_bytes([ico[base + 8], ico[base + 9], ico[base + 10], ico[base + 11]])
            as usize;
        let offset = u32::from_le_bytes([
            ico[base + 12],
            ico[base + 13],
            ico[base + 14],
            ico[base + 15],
        ]) as usize;
        let end = offset.checked_add(size)?;
        if size > 0 && end <= ico.len() {
            candidates.push(IcoFrameInfo {
                width: w,
                height: h,
                offset,
                size,
            });
        }
    }
    if candidates.is_empty() {
        return None;
    }

    // Prefer exact match, else smallest frame >= desired, else largest available frame.
    let best = candidates
        .iter()
        .filter(|f| f.width >= want_w && f.height >= want_h)
        .min_by_key(|f| (f.width - want_w) + (f.height - want_h))
        .or_else(|| candidates.iter().max_by_key(|f| f.width.min(f.height)))?;

    let slice = &ico[best.offset..best.offset + best.size];
    Some((*best, slice))
}

/// Creates a Win32 `HICON` from an `.ico` byte slice using Rust `ICONDIR` parsing +
/// `CreateIconFromResourceEx` (`0x0003_0000`, `LR_DEFAULTCOLOR`).
pub fn create_icon_from_ico_bytes(ico: &[u8], cx: i32, cy: i32) -> Option<HICON> {
    let (_, frame_bytes) = parse_ico_best_frame(ico, cx, cy)?;
    // SAFETY: `frame_bytes` is a valid PNG/DIB icon image slice from the `.ico` directory.
    unsafe { CreateIconFromResourceEx(frame_bytes, true, 0x0003_0000, cx, cy, LR_DEFAULTCOLOR) }.ok()
}

pub type TrayCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default, Clone)]
pub struct TrayCallbacks {
    pub on_toggle_engine: Option<TrayCallback>,
    pub on_open_map: Option<TrayCallback>,
    pub on_open_diagnostics: Option<TrayCallback>,
    pub on_open_config_dir: Option<TrayCallback>,
    pub on_quit: Option<TrayCallback>,
    pub on_launch_borderless: Option<TrayCallback>,
    pub on_open_support: Option<TrayCallback>,
    pub on_check_update: Option<TrayCallback>,
    pub on_stop_session_rec: Option<TrayCallback>,
}

struct TrayMutableState {
    hwnd: isize,
    closed: bool,
    ready_done: bool,
    state: TrayState,
    last_applied_state: Option<TrayState>,
    engine_active: bool,
    session_rec_active: bool,
    lang: Option<String>,
    available_update: Option<String>,
    update_ready: bool,
    uipi_warning: bool,
    custom_tooltip: String,
    tooltip: String,
    nid_added: bool,
    icons: HashMap<TrayState, (isize, bool)>,
    taskbar_created_msg: u32,
}

impl TrayMutableState {
    fn compose_tooltip(&self) -> String {
        if !self.custom_tooltip.is_empty() {
            return append_uipi_warning(
                self.custom_tooltip.clone(),
                self.lang.as_deref(),
                self.uipi_warning,
            );
        }
        get_tooltip(self.state, self.lang.as_deref(), self.uipi_warning)
    }
}

struct TrayShared {
    inner: Mutex<TrayMutableState>,
    ready_cv: Condvar,
    callbacks: TrayCallbacks,
    class_name: String,
}

impl TrayShared {
    fn dispatch_cb(cb: Option<&TrayCallback>) {
        if let Some(func) = cb.cloned() {
            let _ = std::thread::Builder::new()
                .name("GH_TrayCallback".to_owned())
                .spawn(move || {
                    let _ = catch_unwind(AssertUnwindSafe(|| func()));
                });
        }
    }

    fn load_icons(&self) {
        // SAFETY: Querying small system icon metrics.
        let cx = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let cy = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
        // SAFETY: Shared stock icon handle (`IDI_APPLICATION`) does not need `DestroyIcon`.
        let fallback = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();

        let mut loaded_map = HashMap::with_capacity(TrayState::ALL.len());
        for state in TrayState::ALL {
            if let Some(hicon) = create_icon_from_ico_bytes(state.embedded_ico_bytes(), cx, cy) {
                loaded_map.insert(state, (hicon.0 as isize, true));
            } else {
                loaded_map.insert(state, (fallback.0 as isize, false));
            }
        }

        let mut guard = self.inner.lock().expect("tray state lock poisoned");
        guard.icons = loaded_map;
    }

    fn destroy_icons(&self) {
        let icons = {
            let mut guard = self.inner.lock().expect("tray state lock poisoned");
            std::mem::take(&mut guard.icons)
        };
        for (raw_hicon, owned) in icons.into_values() {
            if owned && raw_hicon != 0 {
                // SAFETY: Icon was created via `CreateIconFromResourceEx` and owned by this tray.
                let _ = unsafe { DestroyIcon(HICON(raw_hicon as *mut std::ffi::c_void)) };
            }
        }
    }

    fn apply_notify_icon(&self, hwnd: HWND, action: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
        let (tip, raw_hicon) = {
            let guard = self.inner.lock().expect("tray state lock poisoned");
            let tip = if guard.tooltip.is_empty() {
                guard.compose_tooltip()
            } else {
                guard.tooltip.clone()
            };
            let icon_raw = guard
                .icons
                .get(&guard.state)
                .or_else(|| guard.icons.get(&TrayState::Idle))
                .map(|&(h, _)| h)
                .unwrap_or(0);
            (tip, icon_raw)
        };

        let hicon = if raw_hicon != 0 {
            HICON(raw_hicon as *mut std::ffi::c_void)
        } else {
            // SAFETY: Fallback stock application icon.
            unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default()
        };

        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 0,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: hicon,
            szTip: encode_sz_tip(&tip),
            ..Default::default()
        };

        // SAFETY: `nid` is initialized with `cbSize = size_of::<NOTIFYICONDATAW>()` and valid fields.
        let ok = unsafe { Shell_NotifyIconW(action, &nid) }.as_bool();
        if ok && action == NIM_ADD {
            let mut guard = self.inner.lock().expect("tray state lock poisoned");
            guard.nid_added = true;
        }
    }

    fn show_context_menu(&self, hwnd: HWND) {
        let (active, session_rec_active, lang, available_update, update_ready) = {
            let guard = self.inner.lock().expect("tray state lock poisoned");
            (
                guard.engine_active,
                guard.session_rec_active,
                guard.lang.clone(),
                guard.available_update.clone(),
                guard.update_ready,
            )
        };

        // SAFETY: Creating and populating a transient Win32 popup menu on the tray UI thread.
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };

        let mut items = Vec::new();
        if session_rec_active {
            let stop_label = if lang.as_deref() == Some("en") {
                "⏹ Stop Session Recording"
            } else {
                "⏹ Dừng ghi phiên (Stop Recording)"
            };
            items.push(TrayMenuItem {
                cmd_id: ID_STOP_SESSION_REC,
                label: stop_label.to_owned(),
                is_separator: false,
            });
            items.push(TrayMenuItem {
                cmd_id: 0,
                label: String::new(),
                is_separator: true,
            });
        }
        items.extend(build_menu_items_with_update_state(
            active,
            lang.as_deref(),
            available_update.as_deref(),
            update_ready,
        ));

        for item in items {
            if item.is_separator {
                let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
            } else {
                let mut wide: Vec<u16> = item.label.encode_utf16().collect();
                wide.push(0);
                let _ = unsafe {
                    AppendMenuW(menu, MF_STRING, item.cmd_id as usize, PCWSTR(wide.as_ptr()))
                };
            }
        }

        let mut pos = POINT::default();
        // SAFETY: Querying cursor screen coordinates and displaying popup menu.
        unsafe {
            let _ = GetCursorPos(&mut pos);
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                pos.x,
                pos.y,
                None,
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: `lparam` points to `CREATESTRUCTW` during `WM_NCCREATE`.
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        if !cs.lpCreateParams.is_null() {
            // SAFETY: Storing non-owning `*const TrayShared` kept alive on `_ui_thread_main` stack.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: Reading pointer stored in `GWLP_USERDATA` on `WM_NCCREATE`.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const TrayShared;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `TrayShared` stays alive for the whole lifetime of the message loop.
    let shared = unsafe { &*ptr };

    let taskbar_msg = shared
        .inner
        .lock()
        .expect("tray state lock poisoned")
        .taskbar_created_msg;

    if taskbar_msg != 0 && msg == taskbar_msg {
        shared.apply_notify_icon(hwnd, NIM_ADD);
        return LRESULT(0);
    }

    match msg {
        WM_APP_UPDATE_STATE => {
            let nid_added = shared
                .inner
                .lock()
                .expect("tray state lock poisoned")
                .nid_added;
            let action = if nid_added { NIM_MODIFY } else { NIM_ADD };
            shared.apply_notify_icon(hwnd, action);
            LRESULT(0)
        }
        WM_TRAYICON => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                shared.show_context_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK {
                TrayShared::dispatch_cb(shared.callbacks.on_open_map.as_ref());
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = (wparam.0 & 0xFFFF) as u32;
            match cmd_id {
                ID_TOGGLE_ENGINE => {
                    TrayShared::dispatch_cb(shared.callbacks.on_toggle_engine.as_ref());
                }
                ID_OPEN_MAP => {
                    TrayShared::dispatch_cb(shared.callbacks.on_open_map.as_ref());
                }
                ID_LAUNCH_BORDERLESS => {
                    TrayShared::dispatch_cb(shared.callbacks.on_launch_borderless.as_ref());
                }
                ID_OPEN_DIAG => {
                    TrayShared::dispatch_cb(shared.callbacks.on_open_diagnostics.as_ref());
                }
                ID_OPEN_CONFIG => {
                    TrayShared::dispatch_cb(shared.callbacks.on_open_config_dir.as_ref());
                }
                ID_CHECK_UPDATE => {
                    TrayShared::dispatch_cb(shared.callbacks.on_check_update.as_ref());
                }
                ID_SUPPORT => {
                    TrayShared::dispatch_cb(shared.callbacks.on_open_support.as_ref());
                }
                ID_STOP_SESSION_REC => {
                    TrayShared::dispatch_cb(shared.callbacks.on_stop_session_rec.as_ref());
                }
                ID_QUIT => {
                    TrayShared::dispatch_cb(shared.callbacks.on_quit.as_ref());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                TrayShared::dispatch_cb(shared.callbacks.on_quit.as_ref());
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            TrayShared::dispatch_cb(shared.callbacks.on_quit.as_ref());
            LRESULT(0)
        }
        WM_APP_CLOSE => {
            let was_added = {
                let mut guard = shared.inner.lock().expect("tray state lock poisoned");
                let prev = guard.nid_added;
                guard.nid_added = false;
                prev
            };
            if was_added {
                let nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 0,
                    ..Default::default()
                };
                // SAFETY: Removing our tray icon before destroying the window.
                let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
            }
            // SAFETY: Destroying our hidden tray window on its owning UI thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            shared.destroy_icons();
            // SAFETY: Clearing `GWLP_USERDATA` and terminating the `GH_SystemTray` message loop.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Pure Win32 `Shell_NotifyIconW` system tray controller running on a dedicated `"GH_SystemTray"` thread.
pub struct SystemTrayApp {
    shared: Arc<TrayShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SystemTrayApp {
    pub fn new(callbacks: TrayCallbacks, initial_active: bool, lang: Option<String>) -> Self {
        let state = if initial_active {
            TrayState::Idle
        } else {
            TrayState::Paused
        };
        let tooltip = get_tooltip(state, lang.as_deref(), false);
        let seq = TRAY_INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let class_name = format!("GenshinHelperTrayWnd_{seq}");

        Self {
            shared: Arc::new(TrayShared {
                inner: Mutex::new(TrayMutableState {
                    hwnd: 0,
                    closed: false,
                    ready_done: false,
                    state,
                    last_applied_state: None,
                    engine_active: initial_active,
                    session_rec_active: false,
                    lang,
                    available_update: None,
                    update_ready: false,
                    uipi_warning: false,
                    custom_tooltip: String::new(),
                    tooltip,
                    nid_added: false,
                    icons: HashMap::new(),
                    taskbar_created_msg: 0,
                }),
                ready_cv: Condvar::new(),
                callbacks,
                class_name,
            }),
            thread: Mutex::new(None),
        }
    }

    fn ui_thread_main(shared: Arc<TrayShared>) {
        // SAFETY: Registering Explorer's `TaskbarCreated` broadcast message for tray icon recovery.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.taskbar_created_msg = taskbar_created;
        }

        let Ok(hmod) = (unsafe { GetModuleHandleW(None) }) else {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.ready_done = true;
            shared.ready_cv.notify_all();
            return;
        };
        let hinst: HINSTANCE = hmod.into();

        let mut class_w: Vec<u16> = shared.class_name.encode_utf16().collect();
        class_w.push(0);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class_w.as_ptr()),
            ..Default::default()
        };

        // SAFETY: Registering unique per-instance window class on UI thread.
        if unsafe { RegisterClassW(&wc) } == 0 {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.ready_done = true;
            shared.ready_cv.notify_all();
            return;
        }

        // SAFETY: Creating hidden 0x0 top-level window so shell broadcast messages (`TaskbarCreated`)
        // are received cleanly. `shared` Arc remains alive across the message loop.
        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_w.as_ptr()),
                w!("GenshinHelperTray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                Some(Arc::as_ptr(&shared) as *const std::ffi::c_void),
            )
        } {
            Ok(h) => h,
            Err(_) => {
                let _ = unsafe { UnregisterClassW(PCWSTR(class_w.as_ptr()), Some(hinst)) };
                let mut guard = shared.inner.lock().expect("tray state lock poisoned");
                guard.ready_done = true;
                shared.ready_cv.notify_all();
                return;
            }
        };

        let should_close_immediately = {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.hwnd = hwnd.0 as isize;
            guard.closed
        };

        if should_close_immediately {
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = unsafe { UnregisterClassW(PCWSTR(class_w.as_ptr()), Some(hinst)) };
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.hwnd = 0;
            guard.ready_done = true;
            shared.ready_cv.notify_all();
            return;
        }

        shared.load_icons();
        shared.apply_notify_icon(hwnd, NIM_ADD);

        {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.ready_done = true;
            shared.ready_cv.notify_all();
        }

        let mut msg = MSG::default();
        // SAFETY: Standard Win32 message loop for the hidden tray window.
        loop {
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
            if ret <= 0 {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        shared.destroy_icons();
        {
            let mut guard = shared.inner.lock().expect("tray state lock poisoned");
            guard.hwnd = 0;
        }
        // SAFETY: Window is destroyed; unregister class cleanly.
        let _ = unsafe { UnregisterClassW(PCWSTR(class_w.as_ptr()), Some(hinst)) };
    }

    pub fn start(&self) -> bool {
        let mut thread_guard = self.thread.lock().expect("tray thread lock poisoned");
        {
            let guard = self.shared.inner.lock().expect("tray state lock poisoned");
            if guard.closed {
                return false;
            }
        }
        if let Some(h) = thread_guard.as_ref()
            && !h.is_finished()
        {
            return true;
        }

        {
            let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
            guard.ready_done = false;
        }

        let worker_shared = Arc::clone(&self.shared);
        let Ok(handle) = std::thread::Builder::new()
            .name("GH_SystemTray".to_owned())
            .spawn(move || Self::ui_thread_main(worker_shared))
        else {
            return false;
        };
        *thread_guard = Some(handle);
        drop(thread_guard);

        let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !guard.ready_done {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next_guard, wait_res) = self
                .shared
                .ready_cv
                .wait_timeout(guard, remaining)
                .expect("tray state lock poisoned");
            guard = next_guard;
            if wait_res.timed_out() {
                break;
            }
        }
        guard.hwnd != 0
    }

    pub fn set_state(&self, state: TrayState, tooltip: &str, engine_active: Option<bool>) {
        let hwnd = {
            let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
            guard.engine_active = engine_active.unwrap_or(state != TrayState::Paused);
            guard.custom_tooltip = tooltip.to_owned();
            guard.state = state;
            let tip = guard.compose_tooltip();
            if guard.last_applied_state == Some(state)
                && guard.tooltip == tip
                && guard.hwnd != 0
            {
                return;
            }
            guard.tooltip = tip;
            guard.last_applied_state = Some(state);
            guard.hwnd
        };
        if hwnd != 0 {
            let hw = HWND(hwnd as *mut std::ffi::c_void);
            // SAFETY: Posting custom `WM_APP_UPDATE_STATE` to our hidden tray window.
            let _ = unsafe { PostMessageW(Some(hw), WM_APP_UPDATE_STATE, WPARAM(0), LPARAM(0)) };
        }
    }

    pub fn set_state_str(&self, state: &str, tooltip: &str, engine_active: Option<bool>) {
        self.set_state(TrayState::from_str_lossy(state), tooltip, engine_active);
    }

    pub fn set_uipi_warning(&self, mismatch: bool) {
        let hwnd = {
            let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
            if guard.uipi_warning == mismatch {
                return;
            }
            guard.uipi_warning = mismatch;
            guard.tooltip = guard.compose_tooltip();
            guard.hwnd
        };
        if hwnd != 0 {
            let hw = HWND(hwnd as *mut std::ffi::c_void);
            // SAFETY: Posting custom `WM_APP_UPDATE_STATE` to our hidden tray window.
            let _ = unsafe { PostMessageW(Some(hw), WM_APP_UPDATE_STATE, WPARAM(0), LPARAM(0)) };
        }
    }

    pub fn set_available_update(&self, version: Option<&str>) {
        self.set_update_status(version, false);
    }

    pub fn set_update_status(&self, version: Option<&str>, ready_to_restart: bool) {
        let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
        guard.available_update = version
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches(['v', 'V']).to_owned());
        guard.update_ready = ready_to_restart && guard.available_update.is_some();
    }

    pub fn set_session_recording(&self, active: bool) {
        let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
        guard.session_rec_active = active;
    }

    pub fn set_language(&self, lang: Option<&str>) {
        let hwnd = {
            let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
            guard.lang = lang.map(str::to_owned);
            let tip = guard.compose_tooltip();
            if guard.tooltip == tip && guard.hwnd != 0 {
                return;
            }
            guard.tooltip = tip;
            guard.hwnd
        };
        if hwnd != 0 {
            let hw = HWND(hwnd as *mut std::ffi::c_void);
            // SAFETY: Posting custom `WM_APP_UPDATE_STATE` to our hidden tray window.
            let _ = unsafe { PostMessageW(Some(hw), WM_APP_UPDATE_STATE, WPARAM(0), LPARAM(0)) };
        }
    }

    pub fn close(&self) {
        let hwnd = {
            let mut guard = self.shared.inner.lock().expect("tray state lock poisoned");
            guard.closed = true;
            guard.hwnd
        };
        if hwnd != 0 {
            let hw = HWND(hwnd as *mut std::ffi::c_void);
            // SAFETY: Requesting clean icon deletion and window destruction on the UI thread.
            let _ = unsafe { PostMessageW(Some(hw), WM_APP_CLOSE, WPARAM(0), LPARAM(0)) };
        }
        let handle = {
            let mut thread_guard = self.thread.lock().expect("tray thread lock poisoned");
            thread_guard.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Drop for SystemTrayApp {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_items_match_contract_in_vi_and_en() {
        for lang in [Some("vi"), Some("en")] {
            let active_items = build_menu_items(true, lang);
            let paused_items = build_menu_items(false, lang);

            assert_eq!(active_items.len(), 11);
            assert_eq!(paused_items.len(), 11);

            assert_eq!(active_items[0].cmd_id, ID_TOGGLE_ENGINE);
            assert_ne!(active_items[0].label, paused_items[0].label);
            assert!(!active_items[0].label.is_empty());

            let seps: Vec<usize> = active_items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| it.is_separator.then_some(i))
                .collect();
            assert_eq!(seps, vec![3, 6, 9]);

            assert!(active_items.iter().any(|i| i.cmd_id == ID_CHECK_UPDATE));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_SUPPORT));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_QUIT));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_OPEN_MAP));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_LAUNCH_BORDERLESS));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_OPEN_DIAG));
            assert!(active_items.iter().any(|i| i.cmd_id == ID_OPEN_CONFIG));

            let upd_items = build_menu_items_with_update(true, lang, Some("v5.8.0"));
            let upd_entry = upd_items
                .iter()
                .find(|i| i.cmd_id == ID_CHECK_UPDATE)
                .expect("update item");
            assert!(upd_entry.label.contains("v5.8.0"));
            assert_ne!(
                upd_entry.label,
                active_items
                    .iter()
                    .find(|i| i.cmd_id == ID_CHECK_UPDATE)
                    .unwrap()
                    .label
            );

            let ready_items =
                build_menu_items_with_update_state(true, lang, Some("v5.8.0"), true);
            let ready_entry = ready_items
                .iter()
                .find(|i| i.cmd_id == ID_CHECK_UPDATE)
                .expect("ready update item");
            assert!(ready_entry.label.contains("v5.8.0"));
            assert_ne!(ready_entry.label, upd_entry.label);
        }
    }

    #[test]
    fn tooltip_composition_handles_states_unknown_and_uipi_warning() {
        for state in TrayState::ALL {
            let vi = get_tooltip(state, Some("vi"), false);
            let en = get_tooltip(state, Some("en"), false);
            assert!(!vi.is_empty());
            assert!(!en.is_empty());
            assert_ne!(vi, state.tooltip_key());
            assert_ne!(en, state.tooltip_key());
        }

        // Unknown string normalizes to Idle
        assert_eq!(
            get_tooltip_str("unknown_state", Some("en"), false),
            get_tooltip(TrayState::Idle, Some("en"), false)
        );

        let warned = get_tooltip(TrayState::Connected, Some("en"), true);
        assert!(warned.contains("\n⚠ "));
        assert!(warned.len() > get_tooltip(TrayState::Connected, Some("en"), false).len());
    }

    #[test]
    fn encode_sz_tip_truncates_safely_without_splitting_surrogate_pairs() {
        let short = encode_sz_tip("Genshin Helper");
        assert_eq!(short[14], 0);

        // 126 BMP chars + 1 supplementary emoji (2 UTF-16 code units: high + low surrogate).
        // Index 126 would be the high surrogate, so it must be dropped to avoid invalid UTF-16.
        let mut s = "a".repeat(126);
        s.push('🚀');
        let encoded = encode_sz_tip(&s);
        assert_eq!(encoded[125], b'a' as u16);
        assert_eq!(encoded[126], 0);
        assert_eq!(encoded[127], 0);

        // 200 ASCII chars -> exactly 127 chars + NUL
        let long_ascii = "x".repeat(200);
        let enc2 = encode_sz_tip(&long_ascii);
        assert_eq!(enc2[126], b'x' as u16);
        assert_eq!(enc2[127], 0);
    }

    #[test]
    fn parse_ico_best_frame_and_create_icon_succeed_for_all_embedded_icons() {
        for state in TrayState::ALL {
            let ico = state.embedded_ico_bytes();
            for size in [16, 24, 32, 48] {
                let (info, frame) = parse_ico_best_frame(ico, size, size)
                    .expect("embedded .ico must parse cleanly");
                assert_eq!(info.width, size);
                assert_eq!(info.height, size);
                assert!(frame.starts_with(b"\x89PNG\r\n\x1a\n"));

                let hicon = create_icon_from_ico_bytes(ico, size, size)
                    .expect("CreateIconFromResourceEx must succeed");
                assert!(!hicon.0.is_null());
                // SAFETY: Cleaning up test HICON.
                unsafe { DestroyIcon(hicon).unwrap() };
            }
        }
    }

    #[test]
    fn tray_lifecycle_start_mutate_and_close_cleanly() {
        let tray = SystemTrayApp::new(TrayCallbacks::default(), true, Some("en".to_owned()));
        let started = tray.start();
        assert!(started, "SystemTrayApp::start() must create tray window cleanly");

        tray.set_state(TrayState::Connected, "", Some(true));
        tray.set_uipi_warning(true);
        tray.set_language(Some("vi"));
        tray.set_state_str("paused", "", None);
        tray.close();

        // Idempotent close & cannot restart after closed
        tray.close();
        assert!(!tray.start());
    }

    #[test]
    fn tray_handles_wm_queryendsession_and_wm_close_cleanly() {
        use std::sync::atomic::AtomicBool;
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

        let quit_called = Arc::new(AtomicBool::new(false));
        let quit_flag = Arc::clone(&quit_called);
        let tray = SystemTrayApp::new(
            TrayCallbacks {
                on_quit: Some(Arc::new(move || {
                    quit_flag.store(true, Ordering::SeqCst);
                })),
                ..Default::default()
            },
            true,
            Some("en".to_owned()),
        );
        assert!(tray.start());

        let raw_hwnd = tray
            .shared
            .inner
            .lock()
            .expect("tray state lock")
            .hwnd;
        assert_ne!(raw_hwnd, 0);
        let hw = HWND(raw_hwnd as *mut std::ffi::c_void);

        // SAFETY: Sending WM_QUERYENDSESSION and WM_CLOSE synchronously to our live tray window.
        let qes = unsafe { SendMessageW(hw, WM_QUERYENDSESSION, Some(WPARAM(0)), Some(LPARAM(0))) };
        assert_eq!(qes.0, 1);

        let _ = unsafe { SendMessageW(hw, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0))) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !quit_called.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert!(
            quit_called.load(Ordering::SeqCst),
            "WM_CLOSE must dispatch on_quit callback"
        );
        tray.close();
    }
}
