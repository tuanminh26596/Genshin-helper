//! Hotkey conflict detection (stub for Linux — no NVIDIA Overlay or similar remapping).
//!
//! Implements `gh_core::traits::ConflictAdvisor` with no-op stubs since Linux
//! doesn't have the same hotkey conflict ecosystem as Windows.

use gh_core::traits::ConflictAdvisor;
use gh_core::CoreResult;
use serde_json::Value;

/// Modifier virtual key codes for Linux (evdev).
pub const MODIFIER_VKS: &[&str] = &["alt", "ctrl", "shift", "super", "meta"];

/// Returns the mouse alias for a mouse button name, or `None`.
pub fn lookup_mouse_alias(name: &str) -> Option<&'static str> {
    match name {
        "mouse4" | "xbutton1" | "back" => Some("mouse4"),
        "mouse5" | "xbutton2" | "forward" => Some("mouse5"),
        "middle" | "mbtn" | "middlebutton" => Some("middle"),
        _ => None,
    }
}

/// Maps a hotkey string to a (modifiers, main_key) pair for display.
pub fn hotkey_to_key_combo(hotkey: &str) -> Option<(Vec<&'static str>, String)> {
    let parsed = crate::hotkey::parse_hotkey(hotkey);
    if parsed.main_key.is_empty() {
        return None;
    }
    Some((parsed.modifiers.iter().copied().collect(), parsed.main_key))
}

/// Linux hotkey conflict advisor — always reports no conflicts.
#[derive(Debug, Default)]
pub struct LinuxConflictAdvisor;

/// Alias for cross-platform `gh_platform::hotkey_conflicts::HotkeyConflictAdvisor` parity.
pub type HotkeyConflictAdvisor = LinuxConflictAdvisor;

impl LinuxConflictAdvisor {
    pub fn new() -> Self {
        Self
    }
}

impl ConflictAdvisor for LinuxConflictAdvisor {
    fn check_conflicts(&self, _request: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({ "conflicts": [], "ok": true }))
    }

    fn remap_external(&self, _request: &Value) -> CoreResult<Value> {
        // No external remapping on Linux
        Ok(serde_json::json!({ "ok": true, "skipped": true }))
    }

    fn restore_external(&self, _request: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({ "ok": true, "skipped": true }))
    }
}
