//! Global hotkey manager for Linux using evdev raw input.
//!
//! Monitors `/dev/input/event*` devices for key combinations. This approach
//! works independently of the display server (X11 or Wayland) since it reads
//! directly from the kernel input subsystem.
//!
//! Invariant: Read-only — strictly zero synthetic event injection.

use gh_core::protocol::format_hotkey_display_name;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use tracing::warn;

pub use crate::hotkey_conflicts::{hotkey_to_key_combo, lookup_mouse_alias};

pub type HotkeyCallback = Arc<dyn Fn() + Send + Sync>;
pub type ShouldSuppressFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Canonical 9 hotkey action names.
pub const HOTKEY_ACTION_NAMES: [&str; 9] = [
    "sync",
    "debug",
    "switch",
    "nav_mark",
    "nav_undo",
    "nav_guide",
    "nav_next",
    "nav_layout",
    "nav_toggle",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHotkey {
    pub is_mouse: bool,
    pub mouse_btn: Option<String>,
    pub modifiers: BTreeSet<&'static str>,
    pub main_key: String,
    pub clean_str: String,
}

pub fn parse_hotkey(hotkey: &str) -> ParsedHotkey {
    let raw = hotkey.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return ParsedHotkey {
            is_mouse: false,
            mouse_btn: None,
            modifiers: BTreeSet::new(),
            main_key: String::new(),
            clean_str: String::new(),
        };
    }

    let mut modifiers = BTreeSet::new();
    let mut main_key = String::new();
    let mut is_mouse = false;
    let mut mouse_btn: Option<String> = None;

    for tok in raw
        .split(['+', '-'])
        .flat_map(|p| p.split_whitespace())
        .filter(|p| !p.is_empty())
    {
        match tok {
            "alt" | "menu" => { modifiers.insert("alt"); }
            "ctrl" | "control" => { modifiers.insert("ctrl"); }
            "shift" => { modifiers.insert("shift"); }
            other => {
                if let Some(btn) = lookup_mouse_alias(other) {
                    is_mouse = true;
                    mouse_btn = Some(btn.to_owned());
                    main_key = btn.to_owned();
                } else {
                    main_key = other.to_owned();
                }
            }
        }
    }

    let mut parts: Vec<&str> = modifiers.iter().copied().collect();
    if !main_key.is_empty() {
        parts.push(&main_key);
    }
    let clean_str = parts.join("+");

    ParsedHotkey {
        is_mouse,
        mouse_btn,
        modifiers,
        main_key,
        clean_str,
    }
}

// ---------------------------------------------------------------------------
// UnifiedHotkeyManager
// ---------------------------------------------------------------------------

struct ActionEntry {
    parsed: ParsedHotkey,
    callback: HotkeyCallback,
    enabled: bool,
}

type ActionMap = HashMap<String, ActionEntry>;

pub struct UnifiedHotkeyManager {
    actions: Arc<RwLock<ActionMap>>,
    should_suppress: Arc<RwLock<Option<ShouldSuppressFn>>>,
    running: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for UnifiedHotkeyManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedHotkeyManager").finish()
    }
}

impl Default for UnifiedHotkeyManager {
    fn default() -> Self {
        Self::new(false, None)
    }
}

impl UnifiedHotkeyManager {
    pub fn new(_suppress_triggers: bool, should_suppress: Option<ShouldSuppressFn>) -> Self {
        Self {
            actions: Arc::new(RwLock::new(HashMap::new())),
            should_suppress: Arc::new(RwLock::new(should_suppress)),
            running: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }

    pub fn set_suppress_triggers(&self, _suppress: bool) {}

    /// Register a hotkey action.
    pub fn register(
        &self,
        action: &str,
        hotkey: &str,
        callback: HotkeyCallback,
        enabled: bool,
        _suppress_override: Option<bool>,
    ) {
        let parsed = parse_hotkey(hotkey);
        let mut map = self.actions.write().expect("lock poisoned");
        map.insert(
            action.to_owned(),
            ActionEntry {
                parsed,
                callback,
                enabled,
            },
        );
    }

    pub fn set_should_suppress(&self, callback: Option<ShouldSuppressFn>) {
        if let Ok(mut g) = self.should_suppress.write() {
            *g = callback;
        }
    }

    pub fn set_enabled(&self, action: &str, enabled: bool) -> bool {
        let mut map = self.actions.write().expect("lock poisoned");
        if let Some(entry) = map.get_mut(action) {
            entry.enabled = enabled;
            return true;
        }
        false
    }

    /// Start the evdev listener thread.
    pub fn start(&self) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return true;
        }

        let actions = Arc::clone(&self.actions);
        let running = Arc::clone(&self.running);

        let handle = std::thread::spawn(move || {
            evdev_listener(actions, running);
        });
        *self.thread.lock().expect("lock poisoned") = Some(handle);
        true
    }

    pub fn close(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.lock().expect("lock poisoned").take() {
            let _ = t.join();
        }
    }

    pub fn stop(&self) {
        self.close();
    }

    pub fn get_display_name(&self, action: &str) -> String {
        let map = self.actions.read().expect("lock poisoned");
        if let Some(entry) = map.get(action) {
            format_hotkey_display_name(&entry.parsed.clean_str)
        } else {
            String::new()
        }
    }

    pub fn update_binding(&self, action: &str, hotkey: &str) -> bool {
        let parsed = parse_hotkey(hotkey);
        if parsed.main_key.is_empty() {
            return false;
        }
        if !parsed.is_mouse && key_name_to_evdev_code(&parsed.main_key).is_none() {
            return false;
        }
        let mut map = self.actions.write().expect("lock poisoned");
        if let Some(entry) = map.get_mut(action) {
            entry.parsed = parsed;
            return true;
        }
        false
    }
}

impl Drop for UnifiedHotkeyManager {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// evdev listener
// ---------------------------------------------------------------------------

fn evdev_listener(
    actions: Arc<RwLock<ActionMap>>,
    running: Arc<AtomicBool>,
) {
    use evdev::{Device, Key};
    use std::time::Duration;

    // Collect all keyboard and mouse devices
    let devices: Vec<Device> = evdev::enumerate()
        .filter_map(|(_, dev)| {
            let is_kbd_or_mouse = dev.supported_keys().is_some_and(|k| {
                k.contains(Key::KEY_A)
                    || k.contains(Key::BTN_SIDE)
                    || k.contains(Key::BTN_EXTRA)
                    || k.contains(Key::BTN_MIDDLE)
            });
            is_kbd_or_mouse.then_some(dev)
        })
        .collect();

    if devices.is_empty() {
        warn!(
            "gh-linux: no readable /dev/input/event* devices (add user to 'input' group for global Wayland hotkeys); falling back to X11/XWayland keymap polling"
        );
        x11_keymap_listener(actions, running);
        return;
    }

    // Track previously fired actions so holding a key doesn't re-trigger every 16ms
    let mut prev_matched: HashSet<String> = HashSet::new();

    while running.load(Ordering::SeqCst) {
        let mut pressed: HashSet<u16> = HashSet::new();
        for dev in &devices {
            if let Ok(keys) = dev.get_key_state() {
                for key in keys.iter() {
                    pressed.insert(key.code());
                }
            }
        }

        // Check if any registered hotkey matches the current pressed set
        {
            let map = actions.read().expect("lock poisoned");
            let mut now_matched: HashSet<String> = HashSet::new();
            for (name, entry) in map.iter() {
                if entry.enabled && hotkey_matches_pressed(&entry.parsed, &pressed) {
                    now_matched.insert(name.clone());
                    if !prev_matched.contains(name) {
                        // Read-only listener: `should_suppress` only gates Win32 key-swallowing,
                        // never whether the hotkey callback itself fires.
                        let cb = Arc::clone(&entry.callback);
                        std::thread::spawn(move || cb());
                    }
                }
            }
            prev_matched = now_matched;
        }

        std::thread::sleep(Duration::from_millis(16)); // ~60 Hz poll
    }
}

/// Map a parsed hotkey string to evdev key codes and check against `pressed`.
fn hotkey_matches_pressed(parsed: &ParsedHotkey, pressed: &HashSet<u16>) -> bool {
    use evdev::Key;

    if parsed.main_key.is_empty() {
        return false;
    }

    // Check modifiers
    let alt_ok = !parsed.modifiers.contains("alt")
        || pressed.contains(&Key::KEY_LEFTALT.code())
        || pressed.contains(&Key::KEY_RIGHTALT.code());
    let ctrl_ok = !parsed.modifiers.contains("ctrl")
        || pressed.contains(&Key::KEY_LEFTCTRL.code())
        || pressed.contains(&Key::KEY_RIGHTCTRL.code());
    let shift_ok = !parsed.modifiers.contains("shift")
        || pressed.contains(&Key::KEY_LEFTSHIFT.code())
        || pressed.contains(&Key::KEY_RIGHTSHIFT.code());

    if !alt_ok || !ctrl_ok || !shift_ok {
        return false;
    }

    if parsed.is_mouse {
        return match parsed.mouse_btn.as_deref() {
            Some("mouse4") => pressed.contains(&Key::BTN_SIDE.code()),
            Some("mouse5") => pressed.contains(&Key::BTN_EXTRA.code()),
            Some("middle") => pressed.contains(&Key::BTN_MIDDLE.code()),
            _ => false,
        };
    }

    // Map main key name to evdev code
    if let Some(code) = key_name_to_evdev_code(&parsed.main_key) {
        pressed.contains(&code)
    } else {
        false
    }
}

fn x11_keymap_listener(
    actions: Arc<RwLock<ActionMap>>,
    running: Arc<AtomicBool>,
) {
    use std::time::Duration;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;

    let Ok((conn, _)) = x11rb::rust_connection::RustConnection::connect(None) else {
        warn!("gh-linux: X11 fallback unavailable for hotkeys");
        return;
    };
    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let count = max_kc.saturating_sub(min_kc).saturating_add(1);
    let Ok(cookie) = conn.get_keyboard_mapping(min_kc, count) else {
        return;
    };
    let Ok(mapping) = cookie.reply() else {
        return;
    };
    let syms_per_kc = mapping.keysyms_per_keycode as usize;
    if syms_per_kc == 0 {
        return;
    }

    let mut sym_to_kcs: HashMap<u32, Vec<u8>> = HashMap::new();
    for i in 0..count as usize {
        let kc = min_kc.wrapping_add(i as u8);
        let base = i * syms_per_kc;
        if let Some(&sym) = mapping.keysyms.get(base)
            && sym != 0
        {
            sym_to_kcs.entry(sym).or_default().push(kc);
        }
    }

    let is_sym_down = |keys: &[u8; 32], sym: u32| -> bool {
        sym_to_kcs.get(&sym).is_some_and(|kcs| {
            kcs.iter().any(|&kc| {
                let byte = (kc / 8) as usize;
                let bit = kc % 8;
                byte < 32 && (keys[byte] & (1 << bit)) != 0
            })
        })
    };

    let mut prev_matched: HashSet<String> = HashSet::new();
    while running.load(Ordering::SeqCst) {
        if let Ok(c) = conn.query_keymap()
            && let Ok(reply) = c.reply()
        {
            let keys = &reply.keys;
            let map = actions.read().expect("lock poisoned");
            let mut now_matched: HashSet<String> = HashSet::new();
            for (name, entry) in map.iter() {
                if !entry.enabled || entry.parsed.main_key.is_empty() || entry.parsed.is_mouse {
                    continue;
                }
                let alt_ok = !entry.parsed.modifiers.contains("alt")
                    || is_sym_down(keys, 0xFFE9)
                    || is_sym_down(keys, 0xFFEA);
                let ctrl_ok = !entry.parsed.modifiers.contains("ctrl")
                    || is_sym_down(keys, 0xFFE3)
                    || is_sym_down(keys, 0xFFE4);
                let shift_ok = !entry.parsed.modifiers.contains("shift")
                    || is_sym_down(keys, 0xFFE1)
                    || is_sym_down(keys, 0xFFE2);
                let main_ok = key_name_to_x11_keysym(&entry.parsed.main_key)
                    .is_some_and(|sym| is_sym_down(keys, sym));
                if alt_ok && ctrl_ok && shift_ok && main_ok {
                    now_matched.insert(name.clone());
                    if !prev_matched.contains(name) {
                        // Read-only listener: `should_suppress` only gates Win32 key-swallowing,
                        // never whether the hotkey callback itself fires.
                        let cb = Arc::clone(&entry.callback);
                        std::thread::spawn(move || cb());
                    }
                }
            }
            prev_matched = now_matched;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn key_name_to_x11_keysym(name: &str) -> Option<u32> {
    if name.len() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_alphanumeric() {
            return Some(ch.to_ascii_lowercase() as u32);
        }
    }
    Some(match name {
        "f1" => 0xFFBE,
        "f2" => 0xFFBF,
        "f3" => 0xFFC0,
        "f4" => 0xFFC1,
        "f5" => 0xFFC2,
        "f6" => 0xFFC3,
        "f7" => 0xFFC4,
        "f8" => 0xFFC5,
        "f9" => 0xFFC6,
        "f10" => 0xFFC7,
        "f11" => 0xFFC8,
        "f12" => 0xFFC9,
        "`" | "grave" => 0x0060,
        "tab" => 0xFF09,
        "space" => 0x0020,
        "enter" | "return" => 0xFF0D,
        "escape" | "esc" => 0xFF1B,
        "backspace" => 0xFF08,
        "delete" => 0xFFFF,
        "insert" => 0xFF63,
        "home" => 0xFF50,
        "end" => 0xFF57,
        "pageup" => 0xFF55,
        "pagedown" => 0xFF56,
        "up" => 0xFF52,
        "down" => 0xFF54,
        "left" => 0xFF51,
        "right" => 0xFF53,
        _ => return None,
    })
}

fn key_name_to_evdev_code(name: &str) -> Option<u16> {
    use evdev::Key;
    Some(match name {
        "a" => Key::KEY_A.code(),
        "b" => Key::KEY_B.code(),
        "c" => Key::KEY_C.code(),
        "d" => Key::KEY_D.code(),
        "e" => Key::KEY_E.code(),
        "f" => Key::KEY_F.code(),
        "g" => Key::KEY_G.code(),
        "h" => Key::KEY_H.code(),
        "i" => Key::KEY_I.code(),
        "j" => Key::KEY_J.code(),
        "k" => Key::KEY_K.code(),
        "l" => Key::KEY_L.code(),
        "m" => Key::KEY_M.code(),
        "n" => Key::KEY_N.code(),
        "o" => Key::KEY_O.code(),
        "p" => Key::KEY_P.code(),
        "q" => Key::KEY_Q.code(),
        "r" => Key::KEY_R.code(),
        "s" => Key::KEY_S.code(),
        "t" => Key::KEY_T.code(),
        "u" => Key::KEY_U.code(),
        "v" => Key::KEY_V.code(),
        "w" => Key::KEY_W.code(),
        "x" => Key::KEY_X.code(),
        "y" => Key::KEY_Y.code(),
        "z" => Key::KEY_Z.code(),
        "f1" => Key::KEY_F1.code(),
        "f2" => Key::KEY_F2.code(),
        "f3" => Key::KEY_F3.code(),
        "f4" => Key::KEY_F4.code(),
        "f5" => Key::KEY_F5.code(),
        "f6" => Key::KEY_F6.code(),
        "f7" => Key::KEY_F7.code(),
        "f8" => Key::KEY_F8.code(),
        "f9" => Key::KEY_F9.code(),
        "f10" => Key::KEY_F10.code(),
        "f11" => Key::KEY_F11.code(),
        "f12" => Key::KEY_F12.code(),
        "`" | "grave" => Key::KEY_GRAVE.code(),
        "tab" => Key::KEY_TAB.code(),
        "space" => Key::KEY_SPACE.code(),
        "enter" | "return" => Key::KEY_ENTER.code(),
        "escape" | "esc" => Key::KEY_ESC.code(),
        "backspace" => Key::KEY_BACKSPACE.code(),
        "delete" => Key::KEY_DELETE.code(),
        "insert" => Key::KEY_INSERT.code(),
        "home" => Key::KEY_HOME.code(),
        "end" => Key::KEY_END.code(),
        "pageup" => Key::KEY_PAGEUP.code(),
        "pagedown" => Key::KEY_PAGEDOWN.code(),
        "up" => Key::KEY_UP.code(),
        "down" => Key::KEY_DOWN.code(),
        "left" => Key::KEY_LEFT.code(),
        "right" => Key::KEY_RIGHT.code(),
        "0" => Key::KEY_0.code(),
        "1" => Key::KEY_1.code(),
        "2" => Key::KEY_2.code(),
        "3" => Key::KEY_3.code(),
        "4" => Key::KEY_4.code(),
        "5" => Key::KEY_5.code(),
        "6" => Key::KEY_6.code(),
        "7" => Key::KEY_7.code(),
        "8" => Key::KEY_8.code(),
        "9" => Key::KEY_9.code(),
        _ => return None,
    })
}
