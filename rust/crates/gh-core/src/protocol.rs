//! Wire-compatible HTTP, WebSocket, and Chrome Native Messaging DTOs
//! matching 100% of `extension/` expectations without modifying `extension/`.

use crate::config::AppConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_NATIVE_MESSAGE_BYTES: usize = 1_048_576;

/// Formats a normalized hotkey string into a human-friendly display label
/// (e.g. `"alt+f11"` -> `"Alt + F11"`, `"alt+mouse4"` -> `"Alt + Mouse 4 (Chuột hông)"`).
pub fn format_hotkey_display_name(hotkey_str: &str) -> String {
    let lower = hotkey_str.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return String::new();
    }

    let tokens: Vec<&str> = lower
        .split(['+', '-'])
        .flat_map(|part| part.split_whitespace())
        .filter(|part| !part.is_empty())
        .collect();

    let mut has_ctrl = false;
    let mut has_alt = false;
    let mut has_shift = false;
    let mut mouse_btn: Option<&str> = None;
    let mut keys: Vec<String> = Vec::new();

    for token in &tokens {
        match *token {
            "ctrl" | "control" => {
                if !has_ctrl {
                    has_ctrl = true;
                    keys.push("Ctrl".to_owned());
                }
            }
            "alt" | "menu" => {
                if !has_alt {
                    has_alt = true;
                    keys.push("Alt".to_owned());
                }
            }
            "shift" => {
                if !has_shift {
                    has_shift = true;
                    keys.push("Shift".to_owned());
                }
            }
            "mouse4" | "mouse_4" | "xbutton1" | "xbtn1" | "mouse_back" | "btn4" => {
                mouse_btn = Some("mouse4");
            }
            "mouse5" | "mouse_5" | "xbutton2" | "xbtn2" | "mouse_forward" | "btn5" => {
                mouse_btn = Some("mouse5");
            }
            "middle" | "mouse_middle" | "mbutton" | "wheel" | "wheel_click" | "btn3" => {
                mouse_btn = Some("middle");
            }
            "right" | "mouse_right" | "rbutton" => {
                mouse_btn = Some("right");
            }
            other => keys.push(other.to_ascii_uppercase()),
        }
    }

    if let Some(btn) = mouse_btn {
        let mut mod_parts = Vec::new();
        if has_ctrl {
            mod_parts.push("Ctrl");
        }
        if has_alt {
            mod_parts.push("Alt");
        }
        if has_shift {
            mod_parts.push("Shift");
        }
        let main_name = match btn {
            "mouse4" => "Mouse 4 (Chuột hông)",
            "mouse5" => "Mouse 5 (Chuột hông)",
            "middle" => "Chuột giữa (Con lăn)",
            "right" => "Chuột phải",
            other => other,
        };
        mod_parts.push(main_name);
        return mod_parts.join(" + ");
    }

    if keys.is_empty() {
        lower.to_ascii_uppercase()
    } else {
        keys.join(" + ")
    }
}

/// Response schema for `GET /status` (`src/server.py` + `main.py` callback fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub version: String,
    pub script_version: String,
    pub queue_len: usize,
    pub latest_seq: u64,
    pub hotkey: String,
    pub display_name: String,
    pub hotkey_sync: String,
    pub display_name_sync: String,
    pub hotkey_debug: String,
    pub display_name_debug: String,
    pub hotkey_switch: String,
    pub display_name_switch: String,
    pub hotkey_nav_mark: String,
    pub display_name_nav_mark: String,
    pub hotkey_nav_undo: String,
    pub display_name_nav_undo: String,
    pub hotkey_nav_guide: String,
    pub display_name_nav_guide: String,
    pub hotkey_nav_guide_next: String,
    pub display_name_nav_guide_next: String,
    pub hotkey_nav_next: String,
    pub display_name_nav_next: String,
    pub hotkey_nav_layout: String,
    pub display_name_nav_layout: String,
    pub hotkey_nav_toggle: String,
    pub display_name_nav_toggle: String,
    pub hotkey_nav_unlock: String,
    pub display_name_nav_unlock: String,
    pub hotkey_radar_toggle: String,
    pub display_name_radar_toggle: String,
    pub hotkey_suppress_conflicts: bool,
    pub auto_sync_m: bool,
    pub single_monitor_mode: bool,
    pub monitor_count: usize,
    pub engine_active: bool,
    pub language: String,
    pub language_setting: String,
    pub minimap_tracking: bool,
    pub nav_hud_enabled: bool,
    pub nav_hud_position: String,
    pub nav_hud_custom_pos: Option<[f64; 2]>,
    pub nav_hud_edit_mode: bool,
    pub nav_hud_capture_visible: bool,
    pub nav_hud_guide_zoom_mode: String,
    pub mmap_overlay_mode: String,
    pub radar_enabled: bool,
    pub radar_rect: Option<[f64; 4]>,
    pub radar_zoom: f64,
    pub radar_opacity: f64,
    pub audio_enabled: bool,
    pub audio_volume: i32,
    pub lan_companion_running: bool,
    pub uipi_mismatch: bool,
    pub uipi_status: Value,
    pub last_sync: Option<Value>,
}

impl Default for StatusResponse {
    fn default() -> Self {
        Self::from_config(&AppConfig::default())
    }
}

impl StatusResponse {
    pub fn from_config(cfg: &AppConfig) -> Self {
        let resolved_lang = match cfg.language.as_str() {
            "vi" => "vi",
            "en" => "en",
            _ => "en",
        };
        let sync_display = format_hotkey_display_name(&cfg.hotkey_sync);
        Self {
            status: "connected".to_owned(),
            version: crate::APP_VERSION.to_owned(),
            script_version: crate::SCRIPT_VERSION.to_owned(),
            queue_len: 0,
            latest_seq: 0,
            hotkey: cfg.hotkey.clone(),
            display_name: sync_display.clone(),
            hotkey_sync: cfg.hotkey_sync.clone(),
            display_name_sync: sync_display,
            hotkey_debug: cfg.hotkey_debug.clone(),
            display_name_debug: format_hotkey_display_name(&cfg.hotkey_debug),
            hotkey_switch: cfg.hotkey_switch.clone(),
            display_name_switch: format_hotkey_display_name(&cfg.hotkey_switch),
            hotkey_nav_mark: cfg.hotkey_nav_mark.clone(),
            display_name_nav_mark: format_hotkey_display_name(&cfg.hotkey_nav_mark),
            hotkey_nav_undo: cfg.hotkey_nav_undo.clone(),
            display_name_nav_undo: format_hotkey_display_name(&cfg.hotkey_nav_undo),
            hotkey_nav_guide: cfg.hotkey_nav_guide.clone(),
            display_name_nav_guide: format_hotkey_display_name(&cfg.hotkey_nav_guide),
            hotkey_nav_guide_next: cfg.hotkey_nav_guide_next.clone(),
            display_name_nav_guide_next: format_hotkey_display_name(&cfg.hotkey_nav_guide_next),
            hotkey_nav_next: cfg.hotkey_nav_next.clone(),
            display_name_nav_next: format_hotkey_display_name(&cfg.hotkey_nav_next),
            hotkey_nav_layout: cfg.hotkey_nav_layout.clone(),
            display_name_nav_layout: format_hotkey_display_name(&cfg.hotkey_nav_layout),
            hotkey_nav_toggle: cfg.hotkey_nav_toggle.clone(),
            display_name_nav_toggle: format_hotkey_display_name(&cfg.hotkey_nav_toggle),
            hotkey_nav_unlock: cfg.hotkey_nav_unlock.clone(),
            display_name_nav_unlock: format_hotkey_display_name(&cfg.hotkey_nav_unlock),
            hotkey_radar_toggle: cfg.hotkey_radar_toggle.clone(),
            display_name_radar_toggle: format_hotkey_display_name(&cfg.hotkey_radar_toggle),
            hotkey_suppress_conflicts: cfg.hotkey_suppress_conflicts,
            auto_sync_m: cfg.auto_sync_m,
            single_monitor_mode: cfg.single_monitor_mode,
            monitor_count: 1,
            engine_active: true,
            language: resolved_lang.to_owned(),
            language_setting: cfg.language.clone(),
            minimap_tracking: cfg.minimap_tracking,
            nav_hud_enabled: cfg.nav_hud_enabled,
            nav_hud_position: cfg.nav_hud_position.clone(),
            nav_hud_custom_pos: cfg.nav_hud_custom_pos,
            nav_hud_edit_mode: false,
            nav_hud_capture_visible: cfg.nav_hud_capture_visible,
            nav_hud_guide_zoom_mode: cfg.nav_hud_guide_zoom_mode.clone(),
            mmap_overlay_mode: cfg.mmap_overlay_mode.clone(),
            radar_enabled: cfg.radar_enabled,
            radar_rect: cfg.radar_rect,
            radar_zoom: cfg.radar_zoom,
            radar_opacity: cfg.radar_opacity,
            audio_enabled: cfg.audio_enabled,
            audio_volume: cfg.audio_volume,
            lan_companion_running: false,
            uipi_mismatch: false,
            uipi_status: Value::Null,
            last_sync: None,
        }
    }
}

/// Event kinds broadcast over lossless `/poll?since=<seq>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BroadcastEventType {
    // Durable events stored in `_event_log` ring buffer (maxlen = 128)
    SyncPositionOnly,
    SyncAndMark,
    EngineStateUpdated,
    AutoSyncUpdated,
    SingleMonitorUpdated,
    HotkeyUpdated,
    LanguageUpdated,
    MinimapTrackingUpdated,
    NavHudUpdated,
    RadarUpdated,
    AudioUpdated,
    TspRouteUpdated,
    RouteUpdated,
    DiagnosticCaptured,
    FocusMapTab,
    NavMarkPin,
    LanCompanionUpdated,
    UipiStatusUpdated,
    AppUpdateUpdated,
    // Ephemeral coalesced event (never evicts `_event_log`)
    MinimapPosition,
    // Control frames
    Heartbeat,
    Shutdown,
}

/// Enveloped event returned by `/poll?since=<seq>` with monotonic `_seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollEnvelope {
    #[serde(rename = "type")]
    pub event_type: BroadcastEventType,
    #[serde(rename = "_seq", skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_version: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl PollEnvelope {
    pub fn heartbeat(seq: u64) -> Self {
        Self {
            event_type: BroadcastEventType::Heartbeat,
            seq: Some(seq),
            script_version: Some(crate::SCRIPT_VERSION.to_owned()),
            extra: serde_json::Map::new(),
        }
    }

    pub fn shutdown() -> Self {
        Self {
            event_type: BroadcastEventType::Shutdown,
            seq: None,
            script_version: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// Request payload for Chrome Native Messaging (`com.genshinhelper.host`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum NativeRequest {
    Ping,
    StartServer,
    StopServer {
        #[serde(default = "default_stop_mode")]
        mode: String,
    },
}

fn default_stop_mode() -> String {
    "standby".to_owned()
}

/// Response payload for Chrome Native Messaging (`GenshinHelperHost.exe`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeResponse {
    pub ok: bool,
    pub host_version: String,
    pub server_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActionFeedbackRequest {
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub pin_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_heartbeat_serializes_exact_wire_keys() {
        let hb = PollEnvelope::heartbeat(42);
        let v = serde_json::to_value(&hb).expect("serialize heartbeat");
        assert_eq!(v["type"], "HEARTBEAT");
        assert_eq!(v["_seq"], 42);
        assert_eq!(v["script_version"], crate::SCRIPT_VERSION);
    }

    #[test]
    fn native_messaging_request_roundtrip() {
        let ping: NativeRequest = serde_json::from_str(r#"{"cmd":"ping"}"#).expect("parse ping");
        assert_eq!(ping, NativeRequest::Ping);

        let stop: NativeRequest =
            serde_json::from_str(r#"{"cmd":"stop_server","mode":"shutdown"}"#).expect("parse stop");
        assert_eq!(
            stop,
            NativeRequest::StopServer {
                mode: "shutdown".to_owned()
            }
        );
    }

    #[test]
    fn status_response_default_and_from_config_expose_all_56_keys() {
        let status = StatusResponse::default();
        let val = serde_json::to_value(&status).expect("serialize StatusResponse");
        let obj = val.as_object().expect("json object");
        assert_eq!(obj.len(), 56);
        assert_eq!(obj["hotkey_nav_guide_next"], "alt+n");
        assert_eq!(obj["display_name_nav_guide_next"], "Alt + N");
        assert_eq!(obj["radar_enabled"], false);
        assert_eq!(obj["hotkey_radar_toggle"], "alt+`");
        assert_eq!(obj["hotkey_nav_unlock"], "alt+f6");
        assert_eq!(obj["display_name_nav_unlock"], "Alt + F6");
        assert!(obj["radar_rect"].is_null());
        assert_eq!(obj["mmap_overlay_mode"], "full");
        assert_eq!(obj["status"], "connected");
        assert_eq!(obj["version"], crate::APP_VERSION);
        assert_eq!(obj["script_version"], crate::SCRIPT_VERSION);
        assert_eq!(obj["language"], "en");
        assert_eq!(obj["language_setting"], "auto");
        assert_eq!(obj["monitor_count"], 1);
        assert_eq!(obj["engine_active"], true);
        assert!(obj["uipi_status"].is_null());
        assert!(obj["last_sync"].is_null());

        let cfg = AppConfig {
            language: "vi".to_owned(),
            hotkey: "alt+mouse4".to_owned(),
            hotkey_sync: "alt+mouse4".to_owned(),
            ..Default::default()
        };
        let custom = StatusResponse::from_config(&cfg);
        assert_eq!(custom.language, "vi");
        assert_eq!(custom.language_setting, "vi");
        assert_eq!(custom.display_name_sync, "Alt + Mouse 4 (Chuột hông)");
    }

    #[test]
    fn action_feedback_request_accepts_empty_json() {
        let req: ActionFeedbackRequest =
            serde_json::from_str("{}").expect("empty ActionFeedbackRequest");
        assert_eq!(req.event_type, "");
        assert_eq!(req.status, "");
        assert_eq!(req.title, None);
    }

    #[test]
    fn format_hotkey_display_name_matches_all_21_mouse_aliases_and_edge_cases() {
        for alias in [
            "mouse4",
            "mouse_4",
            "xbutton1",
            "xbtn1",
            "mouse_back",
            "btn4",
        ] {
            assert_eq!(
                format_hotkey_display_name(alias),
                "Mouse 4 (Chuột hông)",
                "alias={alias}"
            );
            assert_eq!(
                format_hotkey_display_name(&format!("alt+{alias}")),
                "Alt + Mouse 4 (Chuột hông)",
                "alt+alias={alias}"
            );
        }

        for alias in [
            "mouse5",
            "mouse_5",
            "xbutton2",
            "xbtn2",
            "mouse_forward",
            "btn5",
        ] {
            assert_eq!(
                format_hotkey_display_name(alias),
                "Mouse 5 (Chuột hông)",
                "alias={alias}"
            );
        }

        for alias in [
            "middle",
            "mouse_middle",
            "mbutton",
            "wheel",
            "wheel_click",
            "btn3",
        ] {
            assert_eq!(
                format_hotkey_display_name(alias),
                "Chuột giữa (Con lăn)",
                "alias={alias}"
            );
        }

        for alias in ["right", "mouse_right", "rbutton"] {
            assert_eq!(
                format_hotkey_display_name(alias),
                "Chuột phải",
                "alias={alias}"
            );
        }

        // x1 / x2 are not in Python MOUSE_BUTTON_MAP, so they format as keys
        assert_eq!(format_hotkey_display_name("x1"), "X1");
        assert_eq!(format_hotkey_display_name("x2"), "X2");
        assert_eq!(format_hotkey_display_name("alt+x1"), "Alt + X1");

        // Empty / whitespace returns "" matching UnifiedHotkeyManager.format_display_name
        assert_eq!(format_hotkey_display_name(""), "");
        assert_eq!(format_hotkey_display_name("   "), "");
    }
}
