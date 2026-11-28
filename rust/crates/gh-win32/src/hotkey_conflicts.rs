//! Hotkey conflict analyzer, `RegisterHotKey` probe, and `NvidiaOverlayAdapter`
//! implementing `gh_core::traits::ConflictAdvisor` (`src/hotkey_conflicts.py`).

use gh_core::atomic_io::write_json_atomic;
use gh_core::paths::AppPaths;
use gh_core::traits::ConflictAdvisor;
use gh_core::{CoreError, CoreResult};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_VALUE_TYPE, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT as WIN_MOD_ALT, MOD_CONTROL as WIN_MOD_CONTROL,
    MOD_NOREPEAT as WIN_MOD_NOREPEAT, MOD_SHIFT as WIN_MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
};
use windows::core::{HRESULT, PCWSTR};

pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12;

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_NOREPEAT: u32 = 0x4000;

pub const ERROR_HOTKEY_ALREADY_REGISTERED_CODE: u32 = 1409;
const PROBE_HOTKEY_ID: i32 = 0x7A4F;

/// Verbatim 11 modifier VKs (`src/hotkey_conflicts.py:33-37`).
pub const MODIFIER_VKS: [u32; 11] = [
    0x10, 0x11, 0x12, // VK_SHIFT, VK_CONTROL, VK_MENU
    0xA0, 0xA1, // VK_LSHIFT, VK_RSHIFT
    0xA2, 0xA3, // VK_LCONTROL, VK_RCONTROL
    0xA4, 0xA5, // VK_LMENU, VK_RMENU
    0x5B, 0x5C, // VK_LWIN, VK_RWIN
];

/// Verbatim 21 mouse button aliases (`src/hotkey_conflicts.py:39-61`).
pub const MOUSE_BUTTON_MAP: [(&str, &str); 21] = [
    ("mouse4", "mouse4"),
    ("mouse_4", "mouse4"),
    ("xbutton1", "mouse4"),
    ("xbtn1", "mouse4"),
    ("mouse_back", "mouse4"),
    ("btn4", "mouse4"),
    ("mouse5", "mouse5"),
    ("mouse_5", "mouse5"),
    ("xbutton2", "mouse5"),
    ("xbtn2", "mouse5"),
    ("mouse_forward", "mouse5"),
    ("btn5", "mouse5"),
    ("middle", "middle"),
    ("mouse_middle", "middle"),
    ("mbutton", "middle"),
    ("wheel", "middle"),
    ("wheel_click", "middle"),
    ("btn3", "middle"),
    ("right", "right"),
    ("mouse_right", "right"),
    ("rbutton", "right"),
];

pub fn lookup_mouse_alias(token: &str) -> Option<&'static str> {
    MOUSE_BUTTON_MAP
        .iter()
        .find(|&&(alias, _)| alias == token)
        .map(|&(_, canonical)| canonical)
}

/// Verbatim `KEY_TO_VK` mapping (`src/hotkey_conflicts.py:63-130`).
/// Note: `"right"` is intentionally absent (mapped as mouse right-click); use `"arrowright"` -> `0x27`.
pub fn key_to_vk(key: &str) -> Option<u32> {
    let vk = match key {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "arrowright" => 0x27,
        "capslock" => 0x14,
        "pause" => 0x13,
        "printscreen" | "prtsc" => 0x2C,
        "`" | "grave" | "tilde" | "backquote" => 0xC0,
        "-" | "minus" => 0xBD,
        "=" | "equal" | "plus" => 0xBB,
        "[" | "bracketleft" => 0xDB,
        "]" | "bracketright" => 0xDD,
        "\\" | "backslash" => 0xDC,
        ";" | "semicolon" => 0xBA,
        "'" | "quote" => 0xDE,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        "/" | "slash" => 0xBF,
        "numpadmultiply" => 0x6A,
        "numpadadd" => 0x6B,
        "numpadsubtract" => 0x6D,
        "numpaddecimal" => 0x6E,
        "numpaddivide" => 0x6F,
        _ => {
            if key.len() == 1 {
                let b = key.as_bytes()[0];
                if b.is_ascii_lowercase() {
                    return Some((b - b'a' + 0x41) as u32);
                }
                if b.is_ascii_digit() {
                    return Some((b - b'0' + 0x30) as u32);
                }
            }
            if let Some(rest) = key.strip_prefix("numpad")
                && rest.len() == 1
            {
                let b = rest.as_bytes()[0];
                if b.is_ascii_digit() {
                    return Some((b - b'0' + 0x60) as u32);
                }
            }
            if let Some(rest) = key.strip_prefix('f')
                && !rest.is_empty()
                && !rest.starts_with('0')
                && rest.bytes().all(|b| b.is_ascii_digit())
                && let Ok(n) = rest.parse::<u32>()
                && (1..=24).contains(&n)
            {
                return Some(0x70 + (n - 1));
            }
            return None;
        }
    };
    Some(vk)
}

/// Verbatim `VK_TO_KEY` inverse mapping (`src/hotkey_conflicts.py:131-173`).
pub fn vk_to_key(vk: u32) -> Option<&'static str> {
    match vk {
        0x20 => Some("space"),
        0x09 => Some("tab"),
        0x0D => Some("enter"),
        0x1B => Some("esc"),
        0x08 => Some("backspace"),
        0x2D => Some("insert"),
        0x2E => Some("delete"),
        0x24 => Some("home"),
        0x23 => Some("end"),
        0x21 => Some("pageup"),
        0x22 => Some("pagedown"),
        0x26 => Some("up"),
        0x28 => Some("down"),
        0x25 => Some("left"),
        0x27 => Some("arrowright"),
        0x14 => Some("capslock"),
        0x13 => Some("pause"),
        0x2C => Some("printscreen"),
        0xC0 => Some("`"),
        0xBD => Some("-"),
        0xBB => Some("="),
        0xDB => Some("["),
        0xDD => Some("]"),
        0xDC => Some("\\"),
        0xBA => Some(";"),
        0xDE => Some("'"),
        0xBC => Some(","),
        0xBE => Some("."),
        0xBF => Some("/"),
        0x6A => Some("numpadmultiply"),
        0x6B => Some("numpadadd"),
        0x6D => Some("numpadsubtract"),
        0x6E => Some("numpaddecimal"),
        0x6F => Some("numpaddivide"),
        0x41..=0x5A => {
            const LETTERS: [&str; 26] = [
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
                "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
            ];
            Some(LETTERS[(vk - 0x41) as usize])
        }
        0x30..=0x39 => {
            const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
            Some(DIGITS[(vk - 0x30) as usize])
        }
        0x60..=0x69 => {
            const NUMPADS: [&str; 10] = [
                "numpad0", "numpad1", "numpad2", "numpad3", "numpad4", "numpad5", "numpad6",
                "numpad7", "numpad8", "numpad9",
            ];
            Some(NUMPADS[(vk - 0x60) as usize])
        }
        0x70..=0x87 => {
            const FKEYS: [&str; 24] = [
                "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13",
                "f14", "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
            ];
            Some(FKEYS[(vk - 0x70) as usize])
        }
        _ => None,
    }
}

/// Verbatim 22 Genshin Impact keys triggered even with Alt/Ctrl/Shift held (`src/hotkey_conflicts.py:335-358`).
pub const GENSHIN_ALWAYS_TRIGGERED_KEYS: [(&str, &str, &str); 22] = [
    ("g", "Giáo trình / Hướng dẫn", "Tutorials / Archive"),
    ("m", "Bản đồ (Map)", "World Map"),
    ("j", "Nhật ký Nhiệm vụ", "Quest Menu"),
    ("b", "Túi đồ", "Inventory"),
    ("c", "Nhân vật", "Character Menu"),
    ("l", "Thiết lập Đội hình", "Party Setup"),
    ("o", "Bạn bè / Co-op", "Friends / Co-op"),
    ("p", "Thoát Khiêu chiến / Bí cảnh", "Leave Challenge / Domain"),
    ("u", "Sổ Thám Hiểm / Bí Cảnh", "Adventurer Handbook / Domain"),
    ("y", "Thông báo / Thư", "Notices / Mail"),
    ("v", "Điều hướng Nhiệm vụ", "Navigate Objective"),
    ("z", "Đạo cụ nhỏ (Gadget)", "Quick Gadget"),
    ("f1", "Bằng Chứng Mạo Hiểm", "Adventurer Handbook"),
    ("f2", "Chế độ Nhiều Người Chơi", "Co-Op Mode"),
    ("f3", "Cầu Nguyện (Wish)", "Wish Menu"),
    ("f4", "Nhật Ký Hành Trình", "Battle Pass"),
    ("f5", "Sự Kiện", "Events Overview"),
    ("1", "Đổi nhân vật #1 (Alt+1 = Kỹ năng Nộ)", "Party Slot 1 (Alt+1 = Burst)"),
    ("2", "Đổi nhân vật #2 (Alt+2 = Kỹ năng Nộ)", "Party Slot 2 (Alt+2 = Burst)"),
    ("3", "Đổi nhân vật #3 (Alt+3 = Kỹ năng Nộ)", "Party Slot 3 (Alt+3 = Burst)"),
    ("4", "Đổi nhân vật #4 (Alt+4 = Kỹ năng Nộ)", "Party Slot 4 (Alt+4 = Burst)"),
    ("5", "Đổi nhân vật #5 (Alt+5 = Kỹ năng Nộ)", "Party Slot 5 (Alt+5 = Burst)"),
];

/// Verbatim 13 Genshin Impact combat/movement keys (`src/hotkey_conflicts.py:361-375`).
pub const GENSHIN_COMBAT_KEYS: [(&str, &str, &str); 13] = [
    ("w", "Di chuyển Tiến", "Move Forward"),
    ("a", "Di chuyển Trái", "Move Left"),
    ("s", "Di chuyển Lùi", "Move Backward"),
    ("d", "Di chuyển Phải", "Move Right"),
    ("e", "Kỹ năng Nguyên Tố", "Elemental Skill"),
    ("q", "Kỹ năng Nộ", "Elemental Burst"),
    ("r", "Ngắm bắn", "Aim Mode"),
    ("f", "Tương tác / Nhặt đồ", "Interact / Pick Up"),
    ("t", "Tương tác Đặc biệt", "Special Interaction"),
    ("x", "Thả cánh bay", "Drop / Cancel Glide"),
    ("space", "Nhảy / Bung cánh", "Jump / Glide"),
    ("tab", "Bánh xe Phím tắt", "Shortcut Wheel"),
    ("esc", "Menu Paimon", "Paimon Menu"),
];

/// Safe suggestion candidates prioritized for left-hand `Alt`-only ergonomics (zero `Shift` Sprint / `Ctrl` Walk).
pub const SAFE_SUGGESTION_POOL: [&str; 24] = [
    "alt+f",
    "alt+t",
    "alt+e",
    "alt+h",
    "alt+x",
    "alt+`",
    "alt+f6",
    "alt+f7",
    "alt+f8",
    "alt+k",
    "alt+n",
    "alt+i",
    "alt+backspace",
    "alt+[",
    "alt+]",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "alt+f11",
    "alt+f12",
    "mouse4",
    "mouse5",
];

/// Verbatim 13 NVIDIA Overlay shortcut metadata rows (`src/hotkey_conflicts.py:459-473`).
pub const NVIDIA_SHORTCUT_META: [(&str, &str, &str, &str); 13] = [
    ("OpenIGO", "GFEOverlayHKeyV2", "Mở NVIDIA Overlay", "Open NVIDIA Overlay"),
    ("Screenshot", "ScreenshotHKey", "Chụp ảnh (NVIDIA)", "NVIDIA Screenshot"),
    ("PMOCOverlay", "PMOCOverlayHKey", "Bảng thông số FPS (NVIDIA)", "NVIDIA Statistics Overlay"),
    ("OpenFreestyle", "FSUIHKeyV1", "Bộ lọc game Freestyle", "NVIDIA Game Filter"),
    ("RecordToggle", "ManualHKey", "Bật/Tắt quay màn hình (NVIDIA)", "Toggle Video Recording"),
    ("OpenAnsel", "AnselUIHKey", "Chế độ ảnh Ansel", "NVIDIA Photo Mode"),
    ("DVRSave", "DVRHKey", "Lưu Instant Replay", "Save Instant Replay"),
    ("DVRToggle", "IRToggleHKey", "Bật/Tắt Instant Replay", "Toggle Instant Replay"),
    ("MicToggle", "MicToggleHKey", "Bật/Tắt Micro (NVIDIA)", "Toggle Microphone (NVIDIA)"),
    ("PMOCOverlayCycle", "PMOCOverlayCycleHKey", "Đổi chế độ thông số", "Cycle Statistics View"),
    ("ChatUIToggle", "ChatUIToggleHKey", "Khung Chat Broadcast", "Broadcast Chat UI"),
    ("ChatPTT", "ChatPTTHKey", "Nhấn để nói (NVIDIA)", "Push-To-Talk (NVIDIA)"),
    ("FreestylePresentCycle", "FSPresetHKey", "Đổi Preset Freestyle", "Cycle Filter Preset"),
];

/// Known third-party overlay default shortcut entry (`src/hotkey_conflicts.py:475-481`).
#[derive(Debug, Clone, Copy)]
pub struct KnownOverlayDefault {
    pub app_id: &'static str,
    pub app_name: &'static str,
    pub default_hotkey: &'static str,
    pub processes: &'static [&'static str],
    pub desc_vi: &'static str,
    pub desc_en: &'static str,
}

/// Verbatim 5 known overlay default hotkeys (`src/hotkey_conflicts.py:475-481`).
pub const KNOWN_OVERLAY_DEFAULTS: [KnownOverlayDefault; 5] = [
    KnownOverlayDefault {
        app_id: "amd_adrenalin",
        app_name: "AMD Software Overlay",
        default_hotkey: "alt+r",
        processes: &["amddow.exe", "radeonsoftware.exe"],
        desc_vi: "Bật/Tắt Overlay AMD",
        desc_en: "Toggle AMD Overlay",
    },
    KnownOverlayDefault {
        app_id: "amd_adrenalin",
        app_name: "AMD Sidebar Overlay",
        default_hotkey: "alt+z",
        processes: &["amddow.exe", "radeonsoftware.exe"],
        desc_vi: "Mở thanh bên AMD",
        desc_en: "Open AMD Sidebar",
    },
    KnownOverlayDefault {
        app_id: "amd_adrenalin",
        app_name: "AMD Performance Metrics",
        default_hotkey: "ctrl+shift+o",
        processes: &["amddow.exe", "radeonsoftware.exe"],
        desc_vi: "Bảng thông số AMD",
        desc_en: "AMD Metrics Overlay",
    },
    KnownOverlayDefault {
        app_id: "discord",
        app_name: "Discord Overlay",
        default_hotkey: "shift+`",
        processes: &["discord.exe"],
        desc_vi: "Mở Discord Game Overlay",
        desc_en: "Toggle Discord Overlay",
    },
    KnownOverlayDefault {
        app_id: "steam",
        app_name: "Steam Overlay",
        default_hotkey: "shift+tab",
        processes: &["steam.exe", "gameoverlayui.exe"],
        desc_vi: "Mở Steam Overlay",
        desc_en: "Open Steam Overlay",
    },
];

const NVSPCAPS_SUBKEY: &str = r"Software\NVIDIA Corporation\Global\ShadowPlay\NVSPCAPS";

// ---------- Pure Hotkey Parsing, Normalization & VK Conversion ----------

/// Parses a hotkey string into `(is_mouse, mouse_btn, modifiers, main_key)`.
pub fn parse_hotkey_parts(
    hotkey_str: &str,
) -> (bool, Option<&'static str>, BTreeSet<&'static str>, String) {
    let raw = hotkey_str.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return (false, None, BTreeSet::new(), String::new());
    }
    let mut modifiers = BTreeSet::new();
    let mut main_key = String::new();
    let mut is_mouse = false;
    let mut mouse_btn: Option<&'static str> = None;

    for tok in raw
        .split(['+', '-'])
        .flat_map(|p| p.split_whitespace())
        .filter(|p| !p.is_empty())
    {
        match tok {
            "ctrl" | "control" => {
                modifiers.insert("ctrl");
            }
            "alt" | "menu" => {
                modifiers.insert("alt");
            }
            "shift" => {
                modifiers.insert("shift");
            }
            other => {
                if let Some(btn) = lookup_mouse_alias(other) {
                    is_mouse = true;
                    mouse_btn = Some(btn);
                    main_key = btn.to_owned();
                } else {
                    main_key = other.to_owned();
                }
            }
        }
    }

    (is_mouse, mouse_btn, modifiers, main_key)
}

/// Returns canonical modifier order `ctrl+alt+shift+<key>`.
pub fn normalize_hotkey(hotkey_str: &str) -> String {
    let (is_mouse, mouse_btn, modifiers, main_key) = parse_hotkey_parts(hotkey_str);
    let mut parts: Vec<String> = Vec::with_capacity(4);
    for mod_name in ["ctrl", "alt", "shift"] {
        if modifiers.contains(mod_name) {
            parts.push(mod_name.to_owned());
        }
    }
    let key = match (is_mouse, mouse_btn) {
        (true, Some(btn)) => btn,
        _ => main_key.as_str(),
    };
    if !key.is_empty() {
        parts.push(key.to_owned());
    }
    parts.join("+")
}

pub fn is_mouse_hotkey(hotkey_str: &str) -> bool {
    parse_hotkey_parts(hotkey_str).0
}

fn title_case_ascii(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(s.len());
    out.extend(first.to_uppercase());
    for ch in chars {
        out.extend(ch.to_lowercase());
    }
    out
}

/// Formats a human-readable hotkey label matching `src/hotkey_conflicts.py:258-278`
/// (`"Mouse 4"`, `"Mouse 5"`, `"Middle"`, `"Right"`, `"Space"`).
pub fn format_display_name(hotkey_str: &str) -> String {
    let (is_mouse, mouse_btn, modifiers, main_key) = parse_hotkey_parts(hotkey_str);
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if modifiers.contains("ctrl") {
        parts.push("Ctrl".to_owned());
    }
    if modifiers.contains("alt") {
        parts.push("Alt".to_owned());
    }
    if modifiers.contains("shift") {
        parts.push("Shift".to_owned());
    }
    if is_mouse {
        let label = match mouse_btn.unwrap_or("") {
            "mouse4" => "Mouse 4".to_owned(),
            "mouse5" => "Mouse 5".to_owned(),
            "middle" => "Middle".to_owned(),
            "right" => "Right".to_owned(),
            other => other.to_ascii_uppercase(),
        };
        parts.push(label);
    } else if !main_key.is_empty() {
        let formatted = if main_key.len() <= 4 {
            main_key.to_ascii_uppercase()
        } else {
            title_case_ascii(&main_key)
        };
        parts.push(formatted);
    }
    if parts.is_empty() {
        hotkey_str.to_ascii_uppercase()
    } else {
        parts.join(" + ")
    }
}

/// Converts a keyboard hotkey into `(vk_code, modifiers)` or `None` if mouse/unknown.
pub fn hotkey_to_vk_combo(hotkey_str: &str) -> Option<(u32, BTreeSet<&'static str>)> {
    let (is_mouse, _, modifiers, main_key) = parse_hotkey_parts(hotkey_str);
    if is_mouse || main_key.is_empty() {
        return None;
    }
    let vk = key_to_vk(&main_key).or_else(|| {
        if main_key.len() == 1 && main_key.is_ascii() {
            Some(main_key.as_bytes()[0].to_ascii_uppercase() as u32)
        } else {
            None
        }
    })?;
    if MODIFIER_VKS.contains(&vk) {
        return None;
    }
    Some((vk, modifiers))
}

/// Converts a keyboard hotkey into ordered VK list (`[17?, 18?, 16?, main_vk]`).
pub fn hotkey_to_vk_sequence(hotkey_str: &str) -> Option<Vec<u32>> {
    let (vk, mods) = hotkey_to_vk_combo(hotkey_str)?;
    let mut seq = Vec::with_capacity(4);
    if mods.contains("ctrl") {
        seq.push(VK_CONTROL);
    }
    if mods.contains("alt") {
        seq.push(VK_MENU);
    }
    if mods.contains("shift") {
        seq.push(VK_SHIFT);
    }
    seq.push(vk);
    Some(seq)
}

/// Converts a Windows VK list (e.g. `[18, 90]`) into canonical hotkey string (`"alt+z"`).
pub fn vk_sequence_to_hotkey(vks: &[u32]) -> String {
    let mut mods = BTreeSet::new();
    let mut main_key = "";
    for &vk in vks {
        match vk {
            VK_CONTROL | 0xA2 | 0xA3 => {
                mods.insert("ctrl");
            }
            VK_MENU | 0xA4 | 0xA5 => {
                mods.insert("alt");
            }
            VK_SHIFT | 0xA0 | 0xA1 => {
                mods.insert("shift");
            }
            other => {
                if let Some(k) = vk_to_key(other) {
                    main_key = k;
                }
            }
        }
    }
    let mut parts: Vec<&str> = ["ctrl", "alt", "shift"]
        .into_iter()
        .filter(|m| mods.contains(m))
        .collect();
    if !main_key.is_empty() {
        parts.push(main_key);
    }
    parts.join("+")
}

// ---------- Win32 Probes (`RegisterHotKey` + Toolhelp32) ----------

/// Probes whether `hotkey_str` is currently registered system-wide via `RegisterHotKey`.
pub fn probe_system_hotkey_registered(hotkey_str: &str) -> bool {
    let Some((vk, mods)) = hotkey_to_vk_combo(hotkey_str) else {
        return false;
    };
    let mut flags = WIN_MOD_NOREPEAT;
    if mods.contains("alt") {
        flags |= WIN_MOD_ALT;
    }
    if mods.contains("ctrl") {
        flags |= WIN_MOD_CONTROL;
    }
    if mods.contains("shift") {
        flags |= WIN_MOD_SHIFT;
    }

    // SAFETY: Probing global hotkey availability on a temporary ID and immediately unregistering.
    match unsafe { RegisterHotKey(None, PROBE_HOTKEY_ID, HOT_KEY_MODIFIERS(flags.0), vk) } {
        Ok(()) => {
            unsafe {
                let _ = UnregisterHotKey(None, PROBE_HOTKEY_ID);
            }
            false
        }
        Err(err) => {
            // SAFETY: Checking Win32 last error for `ERROR_HOTKEY_ALREADY_REGISTERED`.
            let last = unsafe { GetLastError() };
            err.code() == HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED.0)
                || last == ERROR_HOTKEY_ALREADY_REGISTERED
        }
    }
}

/// Enumerates running process executable basenames (lowercased) via `CreateToolhelp32Snapshot`.
pub fn get_running_process_names() -> HashSet<String> {
    let mut out = HashSet::new();
    // SAFETY: Snapshot of current system processes, always closed before returning.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return out;
    };
    if snap == INVALID_HANDLE_VALUE || snap.is_invalid() {
        return out;
    }

    let mut pe = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `pe.dwSize` initialized to `size_of::<PROCESSENTRY32W>()`.
    if unsafe { Process32FirstW(snap, &mut pe).is_ok() } {
        loop {
            let end = pe
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(pe.szExeFile.len());
            let name = String::from_utf16_lossy(&pe.szExeFile[..end])
                .trim()
                .to_ascii_lowercase();
            if !name.is_empty() {
                out.insert(name);
            }
            // SAFETY: Advancing process snapshot iteration.
            if unsafe { Process32NextW(snap, &mut pe).is_err() } {
                break;
            }
        }
    }

    // SAFETY: Closing Toolhelp32 snapshot handle.
    unsafe {
        let _ = CloseHandle(snap);
    }
    out
}

// ---------- NvidiaOverlayAdapter ----------

struct HkeyGuard(HKEY);

impl Drop for HkeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Closing opened HKCU registry handle.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
            self.0 = HKEY::default();
        }
    }
}

#[derive(Debug, Clone)]
pub struct NvidiaShortcut {
    pub app_id: String,
    pub app_name: String,
    pub shortcut_id: String,
    pub reg_prefix: String,
    pub hotkey: String,
    pub display_name: String,
    pub vks: Vec<u32>,
    pub desc_vi: String,
    pub desc_en: String,
}

/// Reads, safely remaps (with automatic atomic backup), and restores NVIDIA Overlay shortcuts
/// in `ShareSettings.json` and `HKCU\...\NVSPCAPS`.
#[derive(Debug, Clone)]
pub struct NvidiaOverlayAdapter {
    pub share_settings_path: PathBuf,
    pub backup_path: PathBuf,
    pub write_registry: bool,
}

impl Default for NvidiaOverlayAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl NvidiaOverlayAdapter {
    pub fn new() -> Self {
        let local_app = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let share_settings_path = local_app
            .join("NVIDIA Corporation")
            .join("NVIDIA Overlay")
            .join("ShareSettings.json");
        let backup_path = AppPaths::resolve().external_hotkey_backups_path();
        Self {
            share_settings_path,
            backup_path,
            write_registry: true,
        }
    }

    /// Isolated constructor for unit tests (`write_registry = false` prevents touching HKCU).
    pub fn with_paths(
        share_settings_path: impl Into<PathBuf>,
        backup_path: impl Into<PathBuf>,
        write_registry: bool,
    ) -> Self {
        Self {
            share_settings_path: share_settings_path.into(),
            backup_path: backup_path.into(),
            write_registry,
        }
    }

    fn read_share_json(&self) -> Option<Map<String, Value>> {
        if !self.share_settings_path.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(&self.share_settings_path).ok()?;
        let val: Value = serde_json::from_str(&text).ok()?;
        val.as_object().cloned()
    }

    fn read_registry_vk(&self, reg_prefix: &str) -> Option<Vec<u32>> {
        if !self.write_registry || reg_prefix.is_empty() {
            return None;
        }
        let subkey_w: Vec<u16> = NVSPCAPS_SUBKEY
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut hkey = HKEY::default();
        // SAFETY: Opening HKCU `NVSPCAPS` read-only.
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_w.as_ptr()),
                Some(0),
                KEY_READ,
                &mut hkey,
            )
            .is_err()
        } {
            return None;
        }
        let _guard = HkeyGuard(hkey);

        let count_bytes = Self::query_reg_binary(hkey, &format!("{reg_prefix}Count"))?;
        if count_bytes.len() < 4 {
            return None;
        }
        let count = u32::from_le_bytes(count_bytes[..4].try_into().ok()?);
        if count == 0 {
            return Some(Vec::new());
        }
        let mut vks = Vec::new();
        for idx in 0..count.min(6) {
            let bytes = Self::query_reg_binary(hkey, &format!("{reg_prefix}{idx}"))?;
            if bytes.len() >= 4 {
                vks.push(u32::from_le_bytes(bytes[..4].try_into().ok()?));
            }
        }
        Some(vks)
    }

    fn query_reg_binary(hkey: HKEY, name: &str) -> Option<Vec<u8>> {
        let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ty = REG_VALUE_TYPE::default();
        let mut len = 0u32;
        // SAFETY: Querying size of `REG_BINARY` value.
        if unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(name_w.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut len),
            )
            .is_err()
        } || ty != REG_BINARY
            || len == 0
        {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let mut actual = len;
        // SAFETY: `buf` is `actual` bytes.
        if unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(name_w.as_ptr()),
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr()),
                Some(&mut actual),
            )
            .is_err()
        } {
            return None;
        }
        buf.truncate(actual as usize);
        Some(buf)
    }

    fn write_registry_vk(&self, reg_prefix: &str, vks: &[u32]) -> bool {
        if !self.write_registry || reg_prefix.is_empty() {
            return false;
        }
        let subkey_w: Vec<u16> = NVSPCAPS_SUBKEY
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut hkey = HKEY::default();
        // SAFETY: Opening HKCU `NVSPCAPS` for read/write only when `self.write_registry` is true.
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_w.as_ptr()),
                Some(0),
                KEY_READ | KEY_WRITE,
                &mut hkey,
            )
            .is_err()
        } {
            return false;
        }
        let _guard = HkeyGuard(hkey);

        let count_name: Vec<u16> = format!("{reg_prefix}Count")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let count_le = (vks.len() as u32).to_le_bytes();
        // SAFETY: Writing 4-byte LE `REG_BINARY` count.
        if unsafe {
            RegSetValueExW(
                hkey,
                PCWSTR(count_name.as_ptr()),
                Some(0),
                REG_BINARY,
                Some(&count_le),
            )
            .is_err()
        } {
            return false;
        }

        for (idx, &vk) in vks.iter().enumerate() {
            let val_name: Vec<u16> = format!("{reg_prefix}{idx}")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let vk_le = vk.to_le_bytes();
            // SAFETY: Writing 4-byte LE `REG_BINARY` VK entry.
            if unsafe {
                RegSetValueExW(
                    hkey,
                    PCWSTR(val_name.as_ptr()),
                    Some(0),
                    REG_BINARY,
                    Some(&vk_le),
                )
                .is_err()
            } {
                return false;
            }
        }

        for extra_idx in vks.len()..8 {
            let val_name: Vec<u16> = format!("{reg_prefix}{extra_idx}")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: Best-effort cleanup of stale trailing VK entries.
            unsafe {
                let _ = RegDeleteValueW(hkey, PCWSTR(val_name.as_ptr()));
            }
        }
        true
    }

    pub fn read_shortcuts(&self) -> Vec<NvidiaShortcut> {
        let mut by_id: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        if let Some(data) = self.read_share_json()
            && let Some(raw_sc) = data
                .get("settings")
                .and_then(|s| s.get("shortcuts"))
                .and_then(|sc| sc.as_object())
        {
            for (sid, val) in raw_sc {
                if let Some(arr) = val.as_array() {
                    let vks: Vec<u32> = arr
                        .iter()
                        .filter_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                        .collect();
                    if !vks.is_empty() {
                        by_id.insert(sid.clone(), vks);
                    }
                }
            }
        }

        if self.write_registry {
            for &(sid, reg_prefix, _, _) in &NVIDIA_SHORTCUT_META {
                if !by_id.contains_key(sid)
                    && let Some(reg_vks) = self.read_registry_vk(reg_prefix)
                    && !reg_vks.is_empty()
                {
                    by_id.insert(sid.to_owned(), reg_vks);
                }
            }
        }

        let mut items = Vec::new();
        for (sid, vks) in by_id {
            let hk = vk_sequence_to_hotkey(&vks);
            if hk.is_empty() {
                continue;
            }
            let (reg_prefix, desc_vi, desc_en) = NVIDIA_SHORTCUT_META
                .iter()
                .find(|&&(id, _, _, _)| id == sid)
                .map(|&(_, rp, vi, en)| (rp, vi, en))
                .unwrap_or(("", sid.as_str(), sid.as_str()));
            items.push(NvidiaShortcut {
                app_id: "nvidia_overlay".to_owned(),
                app_name: "NVIDIA Overlay".to_owned(),
                shortcut_id: sid.clone(),
                reg_prefix: reg_prefix.to_owned(),
                display_name: format_display_name(&hk),
                hotkey: hk,
                vks,
                desc_vi: desc_vi.to_owned(),
                desc_en: desc_en.to_owned(),
            });
        }
        items
    }

    fn load_backups(&self) -> Map<String, Value> {
        if self.backup_path.is_file()
            && let Ok(text) = std::fs::read_to_string(&self.backup_path)
            && let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text)
            && obj.get("items").is_some_and(|v| v.is_object())
        {
            return obj;
        }
        let mut root = Map::new();
        root.insert("version".to_owned(), json!(1));
        root.insert("items".to_owned(), Value::Object(Map::new()));
        root
    }

    fn save_backups(&self, store: &Map<String, Value>) -> CoreResult<()> {
        if let Some(parent) = Path::new(&self.backup_path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        write_json_atomic(&self.backup_path, store)
    }

    pub fn list_backups(&self) -> Vec<Value> {
        let store = self.load_backups();
        let Some(items) = store.get("items").and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, entry_val) in items {
            let Some(entry) = entry_val.as_object() else {
                continue;
            };
            let orig_hk = entry
                .get("original_hotkey")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_hk = entry
                .get("current_hotkey")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let shortcut_id = entry
                .get("shortcut_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            out.push(json!({
                "key": key,
                "app_id": entry.get("app_id").and_then(|v| v.as_str()).unwrap_or("nvidia_overlay"),
                "app_name": entry.get("app_name").and_then(|v| v.as_str()).unwrap_or("NVIDIA Overlay"),
                "shortcut_id": shortcut_id,
                "desc_vi": entry.get("desc_vi").and_then(|v| v.as_str()).unwrap_or(shortcut_id),
                "desc_en": entry.get("desc_en").and_then(|v| v.as_str()).unwrap_or(shortcut_id),
                "original_hotkey": orig_hk,
                "original_display": format_display_name(orig_hk),
                "current_hotkey": new_hk,
                "current_display": format_display_name(new_hk),
            }));
        }
        out
    }

    pub fn remap_shortcut(&self, shortcut_id: &str, new_hotkey: &str) -> Value {
        let Some(vks) = hotkey_to_vk_sequence(new_hotkey) else {
            return json!({
                "ok": false,
                "error": format!("Unsupported keyboard hotkey: {new_hotkey:?}"),
            });
        };
        let norm_new = normalize_hotkey(new_hotkey);

        let mut data = self.read_share_json();
        if data.is_none() && !self.write_registry {
            return json!({
                "ok": false,
                "error": "NVIDIA Overlay ShareSettings.json not found.",
            });
        }

        let (reg_prefix, desc_vi, desc_en) = NVIDIA_SHORTCUT_META
            .iter()
            .find(|&&(id, _, _, _)| id == shortcut_id)
            .map(|&(_, rp, vi, en)| (rp, vi, en))
            .unwrap_or(("", shortcut_id, shortcut_id));

        let mut old_vks: Option<Vec<u32>> = data.as_ref().and_then(|root| {
            root.get("settings")
                .and_then(|s| s.get("shortcuts"))
                .and_then(|sc| sc.get(shortcut_id))
                .and_then(|arr| arr.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                        .collect()
                })
        });
        if old_vks.is_none() && !reg_prefix.is_empty() && self.write_registry {
            old_vks = self.read_registry_vk(reg_prefix);
        }
        let Some(old_vks) = old_vks.filter(|v| !v.is_empty()) else {
            return json!({
                "ok": false,
                "error": format!("NVIDIA shortcut {shortcut_id:?} not found."),
            });
        };

        let mut backup_store = self.load_backups();
        let backup_key = format!("nvidia_overlay:{shortcut_id}");
        let items = backup_store
            .entry("items")
            .or_insert_with(|| Value::Object(Map::new()));
        if !items.is_object() {
            *items = Value::Object(Map::new());
        }
        let items_obj = items.as_object_mut().expect("items is object");

        let original_hotkey = if let Some(existing) =
            items_obj.get_mut(&backup_key).and_then(|v| v.as_object_mut())
        {
            existing.insert("current_vks".to_owned(), json!(vks));
            existing.insert("current_hotkey".to_owned(), json!(norm_new));
            existing
                .get("original_hotkey")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        } else {
            let old_hk = vk_sequence_to_hotkey(&old_vks);
            items_obj.insert(
                backup_key.clone(),
                json!({
                    "app_id": "nvidia_overlay",
                    "app_name": "NVIDIA Overlay",
                    "shortcut_id": shortcut_id,
                    "desc_vi": desc_vi,
                    "desc_en": desc_en,
                    "original_vks": old_vks,
                    "original_hotkey": old_hk,
                    "current_vks": vks,
                    "current_hotkey": norm_new,
                }),
            );
            old_hk
        };
        if let Err(err) = self.save_backups(&backup_store) {
            return json!({
                "ok": false,
                "error": format!("Failed to persist hotkey backup: {err}"),
            });
        }

        let mut json_written = false;
        if let Some(ref mut root) = data {
            let settings = root
                .entry("settings")
                .or_insert_with(|| Value::Object(Map::new()));
            if !settings.is_object() {
                *settings = Value::Object(Map::new());
            }
            let shortcuts = settings
                .as_object_mut()
                .expect("settings object")
                .entry("shortcuts")
                .or_insert_with(|| Value::Object(Map::new()));
            if !shortcuts.is_object() {
                *shortcuts = Value::Object(Map::new());
            }
            shortcuts
                .as_object_mut()
                .expect("shortcuts object")
                .insert(shortcut_id.to_owned(), json!(vks));

            if let Err(err) = write_json_atomic(&self.share_settings_path, root) {
                return json!({
                    "ok": false,
                    "error": format!("Failed to write ShareSettings.json: {err}"),
                });
            }
            json_written = true;
        }

        let reg_written = if !reg_prefix.is_empty() {
            self.write_registry_vk(reg_prefix, &vks)
        } else {
            false
        };
        if !json_written && !reg_written {
            return json!({
                "ok": false,
                "error": "Failed to write NVIDIA Overlay configuration.",
            });
        }

        json!({
            "ok": true,
            "app_id": "nvidia_overlay",
            "shortcut_id": shortcut_id,
            "new_hotkey": norm_new,
            "new_display": format_display_name(&norm_new),
            "original_hotkey": original_hotkey,
            "original_display": format_display_name(&original_hotkey),
            "restart_required": true,
        })
    }

    pub fn restore_shortcut(&self, shortcut_id: &str) -> Value {
        let mut backup_store = self.load_backups();
        let backup_key = format!("nvidia_overlay:{shortcut_id}");
        let entry = backup_store
            .get("items")
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(&backup_key))
            .and_then(|v| v.as_object())
            .cloned();

        let Some(entry) = entry else {
            return json!({
                "ok": false,
                "error": format!("No backup recorded for {shortcut_id:?}."),
            });
        };
        let Some(orig_arr) = entry.get("original_vks").and_then(|v| v.as_array()) else {
            return json!({
                "ok": false,
                "error": format!("No backup recorded for {shortcut_id:?}."),
            });
        };
        let orig_vks: Vec<u32> = orig_arr
            .iter()
            .filter_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
            .collect();
        let orig_hk = entry
            .get("original_hotkey")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| vk_sequence_to_hotkey(&orig_vks));

        let reg_prefix = NVIDIA_SHORTCUT_META
            .iter()
            .find(|&&(id, _, _, _)| id == shortcut_id)
            .map(|&(_, rp, _, _)| rp)
            .unwrap_or("");

        let mut json_written = false;
        if let Some(mut root) = self.read_share_json() {
            let settings = root
                .entry("settings")
                .or_insert_with(|| Value::Object(Map::new()));
            if !settings.is_object() {
                *settings = Value::Object(Map::new());
            }
            let shortcuts = settings
                .as_object_mut()
                .expect("settings object")
                .entry("shortcuts")
                .or_insert_with(|| Value::Object(Map::new()));
            if !shortcuts.is_object() {
                *shortcuts = Value::Object(Map::new());
            }
            shortcuts
                .as_object_mut()
                .expect("shortcuts object")
                .insert(shortcut_id.to_owned(), json!(orig_vks));

            if let Err(err) = write_json_atomic(&self.share_settings_path, &root) {
                return json!({
                    "ok": false,
                    "error": format!("Failed to restore ShareSettings.json: {err}"),
                });
            }
            json_written = true;
        }

        let reg_written = if !reg_prefix.is_empty() {
            self.write_registry_vk(reg_prefix, &orig_vks)
        } else {
            false
        };
        if !json_written && !reg_written {
            return json!({
                "ok": false,
                "error": "Failed to restore NVIDIA Overlay configuration.",
            });
        }

        if let Some(items_obj) = backup_store.get_mut("items").and_then(|v| v.as_object_mut()) {
            items_obj.remove(&backup_key);
        }
        if let Err(err) = self.save_backups(&backup_store) {
            return json!({
                "ok": false,
                "error": format!("Failed to save backup store: {err}"),
            });
        }

        json!({
            "ok": true,
            "app_id": "nvidia_overlay",
            "shortcut_id": shortcut_id,
            "restored_hotkey": orig_hk,
            "restored_display": format_display_name(&orig_hk),
            "restart_required": true,
        })
    }
}

// ---------- HotkeyConflictAdvisor ----------

pub type ProbeFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;
pub type ProcFn = Arc<dyn Fn() -> HashSet<String> + Send + Sync>;

#[derive(Debug, Clone, Copy, Default)]
pub struct AnalyzeOptions<'a> {
    pub action: Option<&'a str>,
    pub current_bindings: Option<&'a BTreeMap<String, String>>,
    pub lang: &'a str,
    pub nv_shortcuts: Option<&'a [NvidiaShortcut]>,
    pub running_procs: Option<&'a HashSet<String>>,
    pub suppress_enabled: bool,
}

/// 3-layer conflict engine implementing `gh_core::traits::ConflictAdvisor`
/// (`src/hotkey_conflicts.py:742-1138`).
pub struct HotkeyConflictAdvisor {
    pub nvidia: NvidiaOverlayAdapter,
    probe_fn: ProbeFn,
    proc_fn: ProcFn,
}

impl Default for HotkeyConflictAdvisor {
    fn default() -> Self {
        Self::new()
    }
}

impl HotkeyConflictAdvisor {
    pub fn new() -> Self {
        Self {
            nvidia: NvidiaOverlayAdapter::new(),
            probe_fn: Arc::new(probe_system_hotkey_registered),
            proc_fn: Arc::new(get_running_process_names),
        }
    }

    pub fn with_dependencies(
        nvidia: NvidiaOverlayAdapter,
        probe_fn: ProbeFn,
        proc_fn: ProcFn,
    ) -> Self {
        Self {
            nvidia,
            probe_fn,
            proc_fn,
        }
    }

    fn norm_lang(lang: &str) -> &'static str {
        if lang.trim().to_ascii_lowercase().starts_with("en") {
            "en"
        } else {
            "vi"
        }
    }

    fn detect_genshin_conflict(
        &self,
        norm_hk: &str,
        lang: &str,
        suppress_enabled: bool,
    ) -> Option<Value> {
        let (is_mouse, _, mods, main_key) = parse_hotkey_parts(norm_hk);
        if is_mouse || main_key.is_empty() {
            return None;
        }
        let is_en = lang == "en";

        if let Some(&(_, vi_name, en_name)) = GENSHIN_ALWAYS_TRIGGERED_KEYS
            .iter()
            .find(|&&(k, _, _)| k == main_key)
        {
            let feat = if is_en { en_name } else { vi_name };
            if !mods.is_empty() && suppress_enabled {
                let mod_txt = mods
                    .iter()
                    .map(|m| title_case_ascii(m))
                    .collect::<Vec<_>>()
                    .join("+");
                let key_upper = main_key.to_ascii_uppercase();
                let msg = if is_en {
                    format!(
                        "Trigger-key suppression intercepts {key_upper} ({feat}) while the game is focused so it won't open the in-game menu."
                    )
                } else {
                    format!(
                        "Đã tự chặn phím {key_upper} ({feat}) khi chơi game (tổ hợp {mod_txt}+{key_upper}) nên sẽ không làm bật menu game."
                    )
                };
                return Some(json!({
                    "kind": "genshin",
                    "severity": "info",
                    "suppressed": true,
                    "app_id": "genshin_impact",
                    "app_name": "Genshin Impact",
                    "shortcut_id": main_key,
                    "can_remap": false,
                    "message": msg,
                }));
            }

            let key_upper = main_key.to_ascii_uppercase();
            let mods_slash = mods
                .iter()
                .map(|m| title_case_ascii(m))
                .collect::<Vec<_>>()
                .join("/");
            let mods_plus = mods
                .iter()
                .map(|m| title_case_ascii(m))
                .collect::<Vec<_>>()
                .join("+");
            let msg = match (!mods.is_empty(), is_en) {
                (true, true) => {
                    format!("Genshin Impact reads {key_upper} ({feat}) even when {mods_slash} is held.")
                }
                (false, true) => format!("Genshin Impact opens {key_upper} ({feat})."),
                (true, false) => format!(
                    "Genshin vẫn nhận phím {key_upper} ({feat}) kể cả khi giữ {mods_plus}!"
                ),
                (false, false) => {
                    format!("Trùng phím mặc định của Genshin: {key_upper} ({feat}).")
                }
            };
            return Some(json!({
                "kind": "genshin",
                "severity": "warn",
                "app_id": "genshin_impact",
                "app_name": "Genshin Impact",
                "shortcut_id": main_key,
                "can_remap": false,
                "message": msg,
            }));
        }

        if let Some(&(_, vi_name, en_name)) = GENSHIN_COMBAT_KEYS
            .iter()
            .find(|&&(k, _, _)| k == main_key)
            && !mods.contains("alt")
        {
            let feat = if is_en { en_name } else { vi_name };
            let key_upper = main_key.to_ascii_uppercase();
            let msg = if is_en {
                format!("Conflicts with Genshin Impact combat/movement key {key_upper} ({feat}).")
            } else {
                format!("Trùng phím điều khiển trong Genshin: {key_upper} ({feat}).")
            };
            return Some(json!({
                "kind": "genshin",
                "severity": "warn",
                "app_id": "genshin_impact",
                "app_name": "Genshin Impact",
                "shortcut_id": main_key,
                "can_remap": false,
                "message": msg,
            }));
        }

        None
    }

    fn detect_external_conflicts(
        &self,
        norm_hk: &str,
        lang: &str,
        nv_shortcuts: &[NvidiaShortcut],
        running_procs: &HashSet<String>,
    ) -> Vec<Value> {
        if is_mouse_hotkey(norm_hk) {
            return Vec::new();
        }
        let is_en = lang == "en";
        let mut out = Vec::new();

        for item in nv_shortcuts {
            if normalize_hotkey(&item.hotkey) != norm_hk {
                continue;
            }
            let label = if is_en {
                &item.desc_en
            } else {
                &item.desc_vi
            };
            let msg = if is_en {
                format!(
                    "Used by NVIDIA Overlay ({label}). You can switch keys or remap NVIDIA's shortcut."
                )
            } else {
                format!(
                    "Đang trùng NVIDIA Overlay ({label}). Bạn có thể đổi phím khác hoặc chiếm & đổi phím NVIDIA."
                )
            };
            out.push(json!({
                "kind": "external_config",
                "severity": "error",
                "app_id": "nvidia_overlay",
                "app_name": "NVIDIA Overlay",
                "shortcut_id": item.shortcut_id,
                "shortcut_label": label,
                "can_remap": true,
                "current_external_hotkey": item.hotkey,
                "current_external_display": item.display_name,
                "message": msg,
            }));
        }

        let nv_running = running_procs.contains("nvidia overlay.exe")
            || running_procs.contains("nvsphelper64.exe")
            || running_procs.contains("nvcontainer.exe");
        let probed_taken = (self.probe_fn)(norm_hk);
        if probed_taken && out.is_empty() {
            let mut matched_known = false;
            for entry in &KNOWN_OVERLAY_DEFAULTS {
                if normalize_hotkey(entry.default_hotkey) != norm_hk {
                    continue;
                }
                if entry.processes.iter().any(|p| running_procs.contains(*p)) {
                    matched_known = true;
                    let desc = if is_en { entry.desc_en } else { entry.desc_vi };
                    let msg = if is_en {
                        format!("Active hotkey held by {} ({desc}).", entry.app_name)
                    } else {
                        format!(
                            "Phím này đang bị {} ({desc}) đăng ký toàn cục.",
                            entry.app_name
                        )
                    };
                    out.push(json!({
                        "kind": "external_live",
                        "severity": "error",
                        "app_id": entry.app_id,
                        "app_name": entry.app_name,
                        "shortcut_id": entry.default_hotkey,
                        "can_remap": false,
                        "message": msg,
                    }));
                    break;
                }
            }
            if !matched_known {
                let owner = if nv_running {
                    "NVIDIA Overlay / Windows"
                } else if is_en {
                    "another app / Windows"
                } else {
                    "ứng dụng khác / Windows"
                };
                let msg = if is_en {
                    format!(
                        "Already registered system-wide by {owner} (may conflict unless taken over)."
                    )
                } else {
                    format!("Phím này đang bị {owner} đăng ký toàn cục trên Windows.")
                };
                out.push(json!({
                    "kind": "external_live",
                    "severity": "error",
                    "app_id": "system",
                    "app_name": owner,
                    "shortcut_id": "",
                    "can_remap": false,
                    "message": msg,
                }));
            }
        }
        out
    }

    fn is_hotkey_clean(
        &self,
        cand_hk: &str,
        used_norms: &HashSet<String>,
        nv_norms: &HashSet<String>,
    ) -> bool {
        let norm = normalize_hotkey(cand_hk);
        if norm.is_empty() || used_norms.contains(&norm) || nv_norms.contains(&norm) {
            return false;
        }
        if self.detect_genshin_conflict(&norm, "vi", false).is_some() {
            return false;
        }
        if (self.probe_fn)(&norm) {
            return false;
        }
        true
    }

    pub fn suggest_safe_hotkeys(
        &self,
        action: Option<&str>,
        current_bindings: Option<&BTreeMap<String, String>>,
        count: usize,
        nv_shortcuts: Option<&[NvidiaShortcut]>,
    ) -> Vec<Value> {
        let mut used_norms = HashSet::new();
        if let Some(bindings) = current_bindings {
            for (act_name, hk) in bindings {
                if action == Some(act_name.as_str()) {
                    continue;
                }
                if !hk.is_empty() {
                    used_norms.insert(normalize_hotkey(hk));
                }
            }
        }

        let owned_nv;
        let nv_list = match nv_shortcuts {
            Some(list) => list,
            None => {
                owned_nv = self.nvidia.read_shortcuts();
                &owned_nv
            }
        };
        let nv_norms: HashSet<String> = nv_list
            .iter()
            .filter(|it| !it.hotkey.is_empty())
            .map(|it| normalize_hotkey(&it.hotkey))
            .collect();

        let mut suggestions: Vec<(String, Value)> = Vec::new();
        for &cand in &SAFE_SUGGESTION_POOL {
            if suggestions.len() >= count {
                break;
            }
            if self.is_hotkey_clean(cand, &used_norms, &nv_norms) {
                let norm = normalize_hotkey(cand);
                let item = json!({
                    "hotkey": norm,
                    "display_name": format_display_name(&norm),
                });
                suggestions.push((norm, item));
            }
        }
        if suggestions.len() < count {
            for &cand in &SAFE_SUGGESTION_POOL {
                if suggestions.len() >= count {
                    break;
                }
                let norm = normalize_hotkey(cand);
                if suggestions.iter().any(|(h, _)| h == &norm) {
                    continue;
                }
                if self.is_hotkey_clean(cand, &used_norms, &nv_norms) {
                    let item = json!({
                        "hotkey": norm,
                        "display_name": format_display_name(&norm),
                    });
                    suggestions.push((norm, item));
                }
            }
        }

        suggestions.into_iter().map(|(_, v)| v).collect()
    }

    pub fn analyze_hotkey(
        &self,
        hotkey_str: &str,
        action: Option<&str>,
        current_bindings: Option<&BTreeMap<String, String>>,
        lang: &str,
        suppress_enabled: bool,
    ) -> Value {
        self.analyze_hotkey_with(
            hotkey_str,
            AnalyzeOptions {
                action,
                current_bindings,
                lang,
                nv_shortcuts: None,
                running_procs: None,
                suppress_enabled,
            },
        )
    }

    pub fn analyze_hotkey_with(&self, hotkey_str: &str, opts: AnalyzeOptions<'_>) -> Value {
        let norm_lang = Self::norm_lang(if opts.lang.is_empty() { "vi" } else { opts.lang });
        let norm_hk = normalize_hotkey(hotkey_str);

        let owned_nv;
        let nv_list = match opts.nv_shortcuts {
            Some(list) => list,
            None => {
                owned_nv = self.nvidia.read_shortcuts();
                &owned_nv
            }
        };
        let owned_procs;
        let procs = match opts.running_procs {
            Some(p) => p,
            None => {
                owned_procs = (self.proc_fn)();
                &owned_procs
            }
        };

        let mut conflicts: Vec<Value> = Vec::new();
        if let Some(bindings) = opts.current_bindings {
            for (other_act, other_hk) in bindings {
                if opts.action == Some(other_act.as_str()) {
                    continue;
                }
                if !other_hk.is_empty() && normalize_hotkey(other_hk) == norm_hk {
                    let msg = if norm_lang == "en" {
                        format!("Already assigned to [{other_act}] in Genshin Helper.")
                    } else {
                        format!("Trùng với chức năng [{other_act}] trong Genshin Helper.")
                    };
                    conflicts.push(json!({
                        "kind": "internal",
                        "severity": "error",
                        "app_id": "genshin_helper",
                        "app_name": "Genshin Helper",
                        "shortcut_id": other_act,
                        "can_remap": false,
                        "message": msg,
                    }));
                }
            }
        }

        if let Some(g_conf) =
            self.detect_genshin_conflict(&norm_hk, norm_lang, opts.suppress_enabled)
        {
            conflicts.push(g_conf);
        }

        conflicts.extend(self.detect_external_conflicts(&norm_hk, norm_lang, nv_list, procs));

        let severity = if conflicts
            .iter()
            .any(|c| c.get("severity").and_then(|s| s.as_str()) == Some("error"))
        {
            "error"
        } else if conflicts
            .iter()
            .any(|c| c.get("severity").and_then(|s| s.as_str()) == Some("warn"))
        {
            "warn"
        } else if !conflicts.is_empty() {
            "info"
        } else {
            "ok"
        };

        let suggestions = if severity == "warn" || severity == "error" {
            self.suggest_safe_hotkeys(opts.action, opts.current_bindings, 3, Some(nv_list))
        } else {
            Vec::new()
        };

        json!({
            "hotkey": norm_hk,
            "display_name": format_display_name(&norm_hk),
            "ok": severity != "warn" && severity != "error",
            "severity": severity,
            "conflicts": conflicts,
            "suggestions": suggestions,
        })
    }

    pub fn analyze_all(
        &self,
        bindings: &BTreeMap<String, String>,
        lang: &str,
        suppress_enabled: bool,
    ) -> Value {
        let nv_list = self.nvidia.read_shortcuts();
        let procs = (self.proc_fn)();
        let mut results = Map::new();
        for (act, hk) in bindings {
            if hk.is_empty() {
                continue;
            }
            results.insert(
                act.clone(),
                self.analyze_hotkey_with(
                    hk,
                    AnalyzeOptions {
                        action: Some(act),
                        current_bindings: Some(bindings),
                        lang,
                        nv_shortcuts: Some(&nv_list),
                        running_procs: Some(&procs),
                        suppress_enabled,
                    },
                ),
            );
        }
        json!({
            "ok": true,
            "actions": results,
            "active_backups": self.nvidia.list_backups(),
            "nvidia_detected": !nv_list.is_empty(),
        })
    }

    pub fn remap_external_hotkey(
        &self,
        app_id: &str,
        shortcut_id: &str,
        new_hotkey: &str,
        current_bindings: Option<&BTreeMap<String, String>>,
        lang: &str,
    ) -> Value {
        let norm_lang = Self::norm_lang(lang);
        let is_en = norm_lang == "en";
        if app_id != "nvidia_overlay" {
            let msg = if is_en {
                format!("Automatic remapping is not supported for {app_id:?}.")
            } else {
                format!("Chưa hỗ trợ tự đổi phím trực tiếp cho {app_id:?}.")
            };
            return json!({ "ok": false, "error": msg });
        }

        let norm_new = normalize_hotkey(new_hotkey);
        if hotkey_to_vk_sequence(&norm_new).is_none() {
            let msg = if is_en {
                "Please enter a valid keyboard combination (not mouse)."
            } else {
                "Vui lòng chọn tổ hợp phím bàn phím hợp lệ cho app kia."
            };
            return json!({ "ok": false, "error": msg });
        }

        if let Some(bindings) = current_bindings {
            for (act, hk) in bindings {
                if !hk.is_empty() && normalize_hotkey(hk) == norm_new {
                    let disp = format_display_name(&norm_new);
                    let msg = if is_en {
                        format!("Replacement hotkey [{disp}] is already used by [{act}].")
                    } else {
                        format!("Phím mới [{disp}] đang trùng với [{act}] của app!")
                    };
                    return json!({ "ok": false, "error": msg });
                }
            }
        }

        for existing in self.nvidia.read_shortcuts() {
            if existing.shortcut_id != shortcut_id
                && normalize_hotkey(&existing.hotkey) == norm_new
            {
                let disp = format_display_name(&norm_new);
                let msg = if is_en {
                    format!(
                        "NVIDIA Overlay already uses [{disp}] for {}.",
                        existing.desc_en
                    )
                } else {
                    format!(
                        "NVIDIA Overlay đang dùng [{disp}] cho {}!",
                        existing.desc_vi
                    )
                };
                return json!({ "ok": false, "error": msg });
            }
        }

        if (self.probe_fn)(&norm_new) {
            let disp = format_display_name(&norm_new);
            let msg = if is_en {
                format!("[{disp}] is currently held by another Windows hotkey.")
            } else {
                format!("Phím [{disp}] đang bị một phần mềm khác trên Windows giữ.")
            };
            return json!({ "ok": false, "error": msg });
        }

        let mut res = self.nvidia.remap_shortcut(shortcut_id, &norm_new);
        if res.get("ok").and_then(|v| v.as_bool()) == Some(true)
            && let Some(obj) = res.as_object_mut()
        {
            obj.insert(
                "active_backups".to_owned(),
                Value::Array(self.nvidia.list_backups()),
            );
        }
        res
    }

    pub fn restore_external_hotkey(&self, app_id: &str, shortcut_id: &str) -> Value {
        if app_id != "nvidia_overlay" {
            return json!({
                "ok": false,
                "error": format!("Unsupported app_id {app_id:?}"),
            });
        }
        let mut res = self.nvidia.restore_shortcut(shortcut_id);
        if res.get("ok").and_then(|v| v.as_bool()) == Some(true)
            && let Some(obj) = res.as_object_mut()
        {
            obj.insert(
                "active_backups".to_owned(),
                Value::Array(self.nvidia.list_backups()),
            );
        }
        res
    }
}

fn extract_bindings(request: &Value) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(obj) = request.get("bindings").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k.clone(), s.to_owned());
            }
        }
    }
    map
}

impl ConflictAdvisor for HotkeyConflictAdvisor {
    fn check_conflicts(&self, request: &Value) -> CoreResult<Value> {
        let lang = request
            .get("lang")
            .and_then(|v| v.as_str())
            .unwrap_or("vi");
        let suppress_enabled = request
            .get("suppress_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let bindings = extract_bindings(request);

        let single_hk = request
            .get("hotkey")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(hk) = single_hk {
            let action = request
                .get("action")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let single =
                self.analyze_hotkey(hk, action, Some(&bindings), lang, suppress_enabled);
            let nv_list = self.nvidia.read_shortcuts();
            return Ok(json!({
                "ok": true,
                "single": single,
                "active_backups": self.nvidia.list_backups(),
                "nvidia_detected": !nv_list.is_empty(),
            }));
        }

        Ok(self.analyze_all(&bindings, lang, suppress_enabled))
    }

    fn remap_external(&self, request: &Value) -> CoreResult<Value> {
        let app_id = request
            .get("app_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let shortcut_id = request
            .get("shortcut_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let non_empty_str = |key: &str| {
            request
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let new_hotkey = non_empty_str("new_external_hotkey")
            .or_else(|| non_empty_str("new_hotkey"))
            .unwrap_or("");

        if app_id.is_empty() || shortcut_id.is_empty() || new_hotkey.is_empty() {
            return Err(CoreError::InvalidArgument(
                "app_id, shortcut_id, and new_external_hotkey are required".to_owned(),
            ));
        }

        let lang = request
            .get("lang")
            .and_then(|v| v.as_str())
            .unwrap_or("vi");
        let mut bindings = extract_bindings(request);
        if let (Some(claim_act), Some(claim_hk)) = (
            request
                .get("claim_action")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            request
                .get("claim_hotkey")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            bindings.insert(claim_act.to_owned(), claim_hk.to_owned());
        }

        Ok(self.remap_external_hotkey(app_id, shortcut_id, new_hotkey, Some(&bindings), lang))
    }

    fn restore_external(&self, request: &Value) -> CoreResult<Value> {
        let app_id = request
            .get("app_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let shortcut_id = request
            .get("shortcut_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if app_id.is_empty() || shortcut_id.is_empty() {
            return Err(CoreError::InvalidArgument(
                "app_id and shortcut_id are required".to_owned(),
            ));
        }
        Ok(self.restore_external_hotkey(app_id, shortcut_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(tag: &str) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_win32_hc_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_isolated_advisor(
        dir: &Path,
        taken: &[&str],
        procs: &[&str],
    ) -> (HotkeyConflictAdvisor, PathBuf, PathBuf) {
        let share_path = dir.join("ShareSettings.json");
        let backup_path = dir.join("external_hotkey_backups.json");

        let share_payload = json!({
            "settings": {
                "unrelated_flag": true,
                "shortcuts": {
                    "OpenIGO": [18, 90],
                    "Screenshot": [18, 120]
                }
            },
            "other_root_key": "keep_me"
        });
        write_json_atomic(&share_path, &share_payload).expect("write ShareSettings.json");

        let adapter = NvidiaOverlayAdapter::with_paths(&share_path, &backup_path, false);
        let taken_set: HashSet<String> = taken.iter().map(|s| normalize_hotkey(s)).collect();
        let proc_set: HashSet<String> = procs.iter().map(|s| s.to_ascii_lowercase()).collect();

        let advisor = HotkeyConflictAdvisor::with_dependencies(
            adapter,
            Arc::new(move |hk| taken_set.contains(&normalize_hotkey(hk))),
            Arc::new(move || proc_set.clone()),
        );
        (advisor, share_path, backup_path)
    }

    #[test]
    fn tables_normalization_and_vk_roundtrips_match_python() {
        assert_eq!(MODIFIER_VKS.len(), 11);
        assert_eq!(MOUSE_BUTTON_MAP.len(), 21);
        assert_eq!(GENSHIN_ALWAYS_TRIGGERED_KEYS.len(), 22);
        assert_eq!(GENSHIN_COMBAT_KEYS.len(), 13);
        assert_eq!(SAFE_SUGGESTION_POOL.len(), 24);
        assert_eq!(NVIDIA_SHORTCUT_META.len(), 13);
        assert_eq!(KNOWN_OVERLAY_DEFAULTS.len(), 5);

        assert_eq!(
            hotkey_to_vk_sequence("alt+shift+p"),
            Some(vec![VK_MENU, VK_SHIFT, 80])
        );
        assert_eq!(vk_sequence_to_hotkey(&[18, 16, 80]), "alt+shift+p");
        assert_eq!(normalize_hotkey("Shift+Alt+Ctrl+K"), "ctrl+alt+shift+k");

        // Mouse "right" vs keyboard "arrowright" disambiguation
        assert!(is_mouse_hotkey("alt+right"));
        assert_eq!(hotkey_to_vk_sequence("alt+right"), None);
        assert!(!is_mouse_hotkey("alt+arrowright"));
        assert_eq!(
            hotkey_to_vk_sequence("alt+arrowright"),
            Some(vec![VK_MENU, 0x27])
        );
        assert_eq!(vk_sequence_to_hotkey(&[18, 0x27]), "alt+arrowright");

        // hotkey_conflicts format_display_name differs intentionally from gh_core::protocol
        assert_eq!(format_display_name("alt+k"), "Alt + K");
        assert_eq!(format_display_name("mouse4"), "Mouse 4");
        assert_eq!(format_display_name("middle"), "Middle");
        assert_eq!(format_display_name("right"), "Right");
        assert_eq!(format_display_name("space"), "Space");

        // Leading-zero & malformed F-keys rejected
        assert_eq!(key_to_vk("f1"), Some(0x70));
        assert_eq!(key_to_vk("f24"), Some(0x87));
        assert_eq!(key_to_vk("f01"), None);
        assert_eq!(key_to_vk("f+1"), None);
        assert_eq!(key_to_vk("f0"), None);
        assert_eq!(key_to_vk("f25"), None);
    }

    #[test]
    fn advisor_layers_nvidia_remap_and_restore_work_isolated() {
        let dir = make_temp_dir("advisor");
        let (advisor, share_path, _backup_path) =
            make_isolated_advisor(&dir, &["ctrl+shift+f1"], &["nvidia overlay.exe"]);

        let mut bindings = BTreeMap::new();
        bindings.insert("sync".to_owned(), "alt+f11".to_owned());
        bindings.insert("nav_mark".to_owned(), "alt+k".to_owned());
        bindings.insert("nav_undo".to_owned(), "alt+shift+k".to_owned());

        // Safe defaults
        let batch = advisor.analyze_all(&bindings, "en", false);
        assert_eq!(batch["ok"], true);
        assert_eq!(batch["nvidia_detected"], true);
        assert_eq!(batch["actions"]["nav_mark"]["ok"], true);
        assert_eq!(batch["actions"]["nav_mark"]["severity"], "ok");

        // Genshin always-triggered key with suppress_enabled=false vs true
        let g_warn = advisor.analyze_hotkey("alt+g", Some("nav_mark"), Some(&bindings), "en", false);
        assert_eq!(g_warn["ok"], false);
        assert_eq!(g_warn["severity"], "warn");
        assert_eq!(g_warn["conflicts"][0]["kind"], "genshin");

        let g_info = advisor.analyze_hotkey("alt+shift+g", Some("nav_mark"), Some(&bindings), "en", true);
        assert_eq!(g_info["ok"], true);
        assert_eq!(g_info["severity"], "info");
        assert_eq!(g_info["conflicts"][0]["kind"], "genshin");
        assert_eq!(g_info["conflicts"][0]["suppressed"], true);
        assert_eq!(g_info["suggestions"].as_array().expect("arr").len(), 0);

        // Internal duplicate
        let dup = advisor.analyze_hotkey("alt+k", Some("sync"), Some(&bindings), "en", false);
        assert_eq!(dup["severity"], "error");
        assert_eq!(dup["conflicts"][0]["kind"], "internal");
        assert_eq!(dup["conflicts"][0]["shortcut_id"], "nav_mark");

        // Live global probe (ctrl+shift+f1 hits both Genshin F1 and external_live)
        let live = advisor.analyze_hotkey("ctrl+shift+f1", Some("sync"), Some(&bindings), "en", false);
        assert_eq!(live["severity"], "error");
        let live_conflicts = live["conflicts"].as_array().expect("live conflicts");
        assert!(live_conflicts.iter().any(|c| {
            c["kind"] == "external_live" && c["app_name"] == "NVIDIA Overlay / Windows"
        }));

        // NVIDIA config conflict on alt+z (alongside Genshin Z Gadget)
        let nv = advisor.analyze_hotkey("alt+z", Some("nav_mark"), Some(&bindings), "en", false);
        assert_eq!(nv["severity"], "error");
        let nv_ext = nv["conflicts"]
            .as_array()
            .expect("nv conflicts")
            .iter()
            .find(|c| c["kind"] == "external_config")
            .expect("external_config conflict for OpenIGO");
        assert_eq!(nv_ext["can_remap"], true);
        assert_eq!(nv_ext["shortcut_id"], "OpenIGO");

        // Collision guards on remap
        assert_eq!(
            advisor.remap_external_hotkey("nvidia_overlay", "OpenIGO", "alt+k", Some(&bindings), "en")["ok"],
            false
        );
        assert_eq!(
            advisor.remap_external_hotkey("nvidia_overlay", "OpenIGO", "alt+f9", Some(&bindings), "en")["ok"],
            false
        );
        assert_eq!(
            advisor.remap_external_hotkey("nvidia_overlay", "OpenIGO", "ctrl+shift+f1", Some(&bindings), "en")["ok"],
            false
        );

        // Valid remap writes [18, 16, 80], preserves unrelated keys, records 1 backup
        let remapped =
            advisor.remap_external_hotkey("nvidia_overlay", "OpenIGO", "alt+shift+p", Some(&bindings), "en");
        assert_eq!(remapped["ok"], true);
        assert_eq!(remapped["new_hotkey"], "alt+shift+p");
        assert_eq!(remapped["original_hotkey"], "alt+z");
        assert_eq!(remapped["active_backups"].as_array().expect("backups").len(), 1);

        let disk_json: Value =
            serde_json::from_str(&std::fs::read_to_string(&share_path).expect("read share"))
                .expect("parse share");
        assert_eq!(disk_json["other_root_key"], "keep_me");
        assert_eq!(disk_json["settings"]["unrelated_flag"], true);
        assert_eq!(disk_json["settings"]["shortcuts"]["OpenIGO"], json!([18, 16, 80]));

        // alt+z no longer has NVIDIA external_config conflict (and is ok:true with suppression enabled)
        let after_remap =
            advisor.analyze_hotkey("alt+z", Some("nav_mark"), Some(&bindings), "en", true);
        assert_eq!(after_remap["ok"], true);
        assert!(
            !after_remap["conflicts"]
                .as_array()
                .expect("conflicts")
                .iter()
                .any(|c| c["kind"] == "external_config")
        );

        // 1-click restore brings back [18, 90] and clears backup
        let restored = advisor.restore_external_hotkey("nvidia_overlay", "OpenIGO");
        assert_eq!(restored["ok"], true);
        assert_eq!(restored["restored_hotkey"], "alt+z");
        assert_eq!(advisor.nvidia.list_backups().len(), 0);
        let disk_after: Value =
            serde_json::from_str(&std::fs::read_to_string(&share_path).expect("read share"))
                .expect("parse share");
        assert_eq!(disk_after["settings"]["shortcuts"]["OpenIGO"], json!([18, 90]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_overlays_nav_undo_suggestions_and_trait_contract() {
        let dir = make_temp_dir("trait_overlay");
        let (advisor, _, _) = make_isolated_advisor(
            &dir,
            &["alt+r", "shift+`", "shift+tab"],
            &["amddow.exe", "discord.exe", "steam.exe"],
        );

        let amd = advisor.analyze_hotkey("alt+r", None, None, "en", false);
        assert_eq!(amd["conflicts"][0]["app_id"], "amd_adrenalin");
        let discord = advisor.analyze_hotkey("shift+`", None, None, "en", false);
        assert_eq!(discord["conflicts"][0]["app_id"], "discord");
        let steam = advisor.analyze_hotkey("shift+tab", None, None, "en", false);
        assert!(
            steam["conflicts"]
                .as_array()
                .expect("steam conflicts")
                .iter()
                .any(|c| c["app_id"] == "steam")
        );

        // nav_undo suggestions avoid Shift (Sprint) and Ctrl (Walk) in Genshin
        let undo_sugg = advisor.suggest_safe_hotkeys(Some("nav_undo"), None, 3, None);
        assert_eq!(undo_sugg.len(), 3);
        for item in &undo_sugg {
            let hk = item["hotkey"].as_str().expect("hotkey");
            assert!(
                !hk.contains("shift") && !hk.contains("ctrl"),
                "nav_undo suggestion should prefer Shift/Ctrl-free left-hand keys: {hk}"
            );
        }

        // ConflictAdvisor trait single & batch & remap & restore
        let trait_obj: &dyn ConflictAdvisor = &advisor;
        let single_res = trait_obj
            .check_conflicts(&json!({
                "hotkey": "alt+z",
                "action": "nav_mark",
                "bindings": { "nav_mark": "alt+k" },
                "lang": "en",
                "suppress_enabled": false
            }))
            .expect("check single");
        assert_eq!(single_res["ok"], true);
        assert_eq!(single_res["single"]["severity"], "error");

        let batch_res = trait_obj
            .check_conflicts(&json!({
                "bindings": { "nav_mark": "alt+k" },
                "lang": "vi"
            }))
            .expect("check batch");
        assert_eq!(batch_res["ok"], true);
        assert!(batch_res["actions"]["nav_mark"].is_object());

        let remap_fail = trait_obj
            .remap_external(&json!({
                "app_id": "discord",
                "shortcut_id": "x",
                "new_hotkey": "alt+shift+p"
            }))
            .expect("unsupported app");
        assert_eq!(remap_fail["ok"], false);

        let remap_ok = trait_obj
            .remap_external(&json!({
                "app_id": "nvidia_overlay",
                "shortcut_id": "OpenIGO",
                "new_external_hotkey": "alt+shift+p",
                "claim_action": "nav_mark",
                "claim_hotkey": "alt+z",
                "bindings": { "nav_mark": "alt+k" }
            }))
            .expect("remap ok");
        assert_eq!(remap_ok["ok"], true);

        let restore_ok = trait_obj
            .restore_external(&json!({
                "app_id": "nvidia_overlay",
                "shortcut_id": "OpenIGO"
            }))
            .expect("restore ok");
        assert_eq!(restore_ok["ok"], true);

        let restore_missing = trait_obj
            .restore_external(&json!({
                "app_id": "nvidia_overlay",
                "shortcut_id": "OpenIGO"
            }))
            .expect("restore missing");
        assert_eq!(restore_missing["ok"], false);

        // Null or empty new_external_hotkey must fall back to new_hotkey (src/server.py:654)
        let remap_null = trait_obj
            .remap_external(&json!({
                "app_id": "nvidia_overlay",
                "shortcut_id": "OpenIGO",
                "new_external_hotkey": null,
                "new_hotkey": "alt+shift+p"
            }))
            .expect("remap null fallback");
        assert_eq!(remap_null["ok"], true);
        assert_eq!(remap_null["new_hotkey"], "alt+shift+p");

        let remap_empty = trait_obj
            .remap_external(&json!({
                "app_id": "nvidia_overlay",
                "shortcut_id": "OpenIGO",
                "new_external_hotkey": "",
                "new_hotkey": "ctrl+shift+f6"
            }))
            .expect("remap empty fallback");
        assert_eq!(remap_empty["ok"], true);
        assert_eq!(remap_empty["new_hotkey"], "ctrl+shift+f6");

        let _ = trait_obj.restore_external(&json!({
            "app_id": "nvidia_overlay",
            "shortcut_id": "OpenIGO"
        }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_process_snapshot_and_hotkey_probe_smoke() {
        let procs = get_running_process_names();
        assert!(!procs.is_empty(), "process snapshot should be non-empty");
        // Verify probe returns a deterministic bool without panicking
        let _ = probe_system_hotkey_registered("ctrl+alt+shift+f24");
    }

    #[test]
    fn default_left_hand_shift_free_hotkeys_have_zero_genshin_or_overlay_conflicts() {
        let tmp = std::env::temp_dir();
        let advisor = HotkeyConflictAdvisor::with_dependencies(
            NvidiaOverlayAdapter::with_paths(
                tmp.join("gh_test_radar_nv_share_nonexistent.xml"),
                tmp.join("gh_test_radar_nv_backup_nonexistent.json"),
                false,
            ),
            Arc::new(|_| false),
            Arc::new(HashSet::new),
        );
        let res = advisor
            .check_conflicts(&json!({
                "bindings": {
                    "sync": "alt+f11",
                    "debug": "alt+f12",
                    "switch": "alt+q",
                    "nav_mark": "alt+f",
                    "nav_undo": "alt+t",
                    "nav_guide": "alt+h",
                    "nav_guide_next": "alt+n",
                    "nav_next": "alt+e",
                    "nav_layout": "alt+f7",
                    "nav_toggle": "alt+x",
                    "nav_unlock": "alt+f6",
                    "radar_toggle": "alt+`"
                },
                "lang": "vi",
                "suppress_conflicts": false
            }))
            .expect("conflict check");
        for act in [
            "sync",
            "debug",
            "switch",
            "nav_mark",
            "nav_undo",
            "nav_guide",
            "nav_guide_next",
            "nav_next",
            "nav_layout",
            "nav_toggle",
            "nav_unlock",
            "radar_toggle",
        ] {
            assert_eq!(res["actions"][act]["ok"], true, "action={act}");
            assert_eq!(
                res["actions"][act]["conflicts"].as_array().map(Vec::len),
                Some(0),
                "action={act} conflicts={:?}",
                res["actions"][act]["conflicts"]
            );
        }
    }
}
