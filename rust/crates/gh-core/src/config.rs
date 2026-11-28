//! Backwards-compatible `config.json` schema matching `src/config.py`,
//! extended with optional overridable `hoyolab_sync` settings.

use crate::atomic_io;
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const VALID_LANGUAGES: [&str; 3] = ["auto", "vi", "en"];
pub const VALID_GUIDE_ZOOM_MODES: [&str; 3] = ["cycle", "compact", "large"];
pub const VALID_MMAP_OVERLAY_MODES: [&str; 3] = ["full", "minimal", "off"];
pub const VALID_AUTO_UPDATE_MODES: [&str; 3] = ["auto_download", "notify_only", "off"];
pub const VALID_APP_UPDATE_MODES: [&str; 3] = ["auto", "notify", "off"];
/// Current hotkey default profile schema version (`2` = left-hand `Alt`-only, zero `Shift` Sprint conflicts).
pub const CURRENT_HOTKEY_PROFILE_VERSION: u32 = 2;
pub const DEFAULT_HOTKEY_NAV_MARK: &str = "alt+f";
pub const DEFAULT_HOTKEY_NAV_UNDO: &str = "alt+t";
pub const DEFAULT_HOTKEY_NAV_GUIDE: &str = "alt+h";
/// Next community guide suggestion (N = next). `alt+g` is out: Genshin opens Tutorials on G even with Alt held.
pub const DEFAULT_HOTKEY_NAV_GUIDE_NEXT: &str = "alt+n";
pub const DEFAULT_HOTKEY_NAV_NEXT: &str = "alt+e";
pub const DEFAULT_HOTKEY_NAV_LAYOUT: &str = "alt+f7";
pub const DEFAULT_HOTKEY_NAV_TOGGLE: &str = "alt+x";
pub const DEFAULT_HOTKEY_NAV_UNLOCK: &str = "alt+f6";
pub const DEFAULT_HOTKEY_RADAR_TOGGLE: &str = "alt+`";
pub const DEFAULT_RADAR_ZOOM: f64 = -0.45;
pub const RADAR_ZOOM_MIN: f64 = -1.2;
pub const RADAR_ZOOM_MAX: f64 = 0.3;
pub const DEFAULT_RADAR_OPACITY: f64 = 0.94;
pub const RADAR_OPACITY_MIN: f64 = 0.45;
pub const RADAR_OPACITY_MAX: f64 = 1.0;

pub const DEFAULT_HOYOLAB_BASE_API: &str = "https://sg-public-api.hoyolab.com/common/map_user/ys_obc";
pub const DEFAULT_HOYOLAB_TILE_BASE: &str = "https://act-webstatic.hoyoverse.com/map_manage/map";
pub const DEFAULT_HOYOLAB_RPC_MAP_VERSION: &str = "4.5";
pub const DEFAULT_HOYOLAB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
pub const DEFAULT_PROBE_ID_RANGE: [i32; 2] = [1, 80];

pub const DEFAULT_INSTRUCTIONS_VI: &str = "Cấu hình phím tắt cho Genshin Helper. Hỗ trợ: mouse4, mouse5, middle, alt+f11, alt+f12, alt+q, f10, alt+mouse4, ctrl+q, v.v. Bạn có thể sửa trực tiếp tại đây hoặc click vào biểu tượng bánh răng trên Web Map.";

/// Overridable HoYoLAB map dataset sync & startup version-check settings (`src/hoyolab_sync.py`).
///
/// Kept optional in `config.json` via defaults so legacy configs and `extension/`
/// remain 100% compatible without changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoyolabSyncConfig {
    pub base_api: String,
    pub tile_base: String,
    pub rpc_map_version: String,
    pub user_agent: String,
    pub default_map_ids: Vec<i32>,
    /// Inclusive `[min_id, max_id]` probe range when discovering newly added HoYoLAB maps.
    pub probe_id_range: [i32; 2],
    /// Map search priority keyed by stringified `map_id` (lower = higher priority).
    pub map_priorities: BTreeMap<String, i32>,
    pub primary_landmark_labels: Vec<i32>,
    /// Non-blocking check comparing online `detail_v2.map_version` vs local `multimap_v4.json` on startup.
    pub auto_check_on_startup: bool,
    /// `"auto_download"` (background staged download + atomic swap) | `"notify_only"` | `"off"`.
    pub auto_update_mode: String,
}

impl Default for HoyolabSyncConfig {
    fn default() -> Self {
        let mut map_priorities = BTreeMap::new();
        for (mid, prio) in [(2, 1), (7, 4), (9, 3), (34, 5), (36, 6), (37, 7), (40, 2)] {
            map_priorities.insert(mid.to_string(), prio);
        }
        Self {
            base_api: DEFAULT_HOYOLAB_BASE_API.to_owned(),
            tile_base: DEFAULT_HOYOLAB_TILE_BASE.to_owned(),
            rpc_map_version: DEFAULT_HOYOLAB_RPC_MAP_VERSION.to_owned(),
            user_agent: DEFAULT_HOYOLAB_USER_AGENT.to_owned(),
            default_map_ids: vec![2, 7, 9, 34, 36, 37, 40],
            probe_id_range: DEFAULT_PROBE_ID_RANGE,
            map_priorities,
            primary_landmark_labels: vec![2, 3, 154],
            auto_check_on_startup: true,
            auto_update_mode: "auto_download".to_owned(),
        }
    }
}

impl<'de> Deserialize<'de> for HoyolabSyncConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = Value::deserialize(deserializer)?;
        let Some(map) = val.as_object() else {
            return Err(serde::de::Error::custom(
                "hoyolab_sync config must be a JSON object",
            ));
        };
        Ok(Self::from_json_map(map))
    }
}

impl HoyolabSyncConfig {
    pub fn from_json_map(map: &Map<String, Value>) -> Self {
        let mut cfg = Self::default();
        cfg.base_api = parse_string_or(map.get("base_api"), &cfg.base_api);
        cfg.tile_base = parse_string_or(map.get("tile_base"), &cfg.tile_base);
        cfg.rpc_map_version = parse_string_or(map.get("rpc_map_version"), &cfg.rpc_map_version);
        cfg.user_agent = parse_string_or(map.get("user_agent"), &cfg.user_agent);
        if let Some(ids) = parse_i32_vec(map.get("default_map_ids"))
            && !ids.is_empty()
        {
            cfg.default_map_ids = ids;
        }
        if let Some(arr) = map.get("probe_id_range").and_then(Value::as_array)
            && arr.len() == 2
            && let (Some(a), Some(b)) = (parse_i32_val(&arr[0]), parse_i32_val(&arr[1]))
        {
            cfg.probe_id_range = [a, b];
        }
        if let Some(obj) = map.get("map_priorities").and_then(Value::as_object) {
            let mut parsed = BTreeMap::new();
            for (k, v) in obj {
                if let Some(p) = parse_i32_val(v) {
                    parsed.insert(k.clone(), p);
                }
            }
            if !parsed.is_empty() {
                cfg.map_priorities = parsed;
            }
        }
        if let Some(labels) = parse_i32_vec(map.get("primary_landmark_labels"))
            && !labels.is_empty()
        {
            cfg.primary_landmark_labels = labels;
        }
        cfg.auto_check_on_startup =
            parse_bool_or(map.get("auto_check_on_startup"), cfg.auto_check_on_startup);
        cfg.auto_update_mode =
            parse_string_or(map.get("auto_update_mode"), &cfg.auto_update_mode);
        cfg.normalize();
        cfg
    }

    pub fn normalize(&mut self) {
        if self.base_api.trim().is_empty() {
            self.base_api = DEFAULT_HOYOLAB_BASE_API.to_owned();
        } else {
            self.base_api = self.base_api.trim().trim_end_matches('/').to_owned();
        }
        if self.tile_base.trim().is_empty() {
            self.tile_base = DEFAULT_HOYOLAB_TILE_BASE.to_owned();
        } else {
            self.tile_base = self.tile_base.trim().trim_end_matches('/').to_owned();
        }
        if self.rpc_map_version.trim().is_empty() {
            self.rpc_map_version = DEFAULT_HOYOLAB_RPC_MAP_VERSION.to_owned();
        } else {
            self.rpc_map_version = self.rpc_map_version.trim().to_owned();
        }
        if self.user_agent.trim().is_empty() {
            self.user_agent = DEFAULT_HOYOLAB_USER_AGENT.to_owned();
        }
        if self.default_map_ids.is_empty() {
            self.default_map_ids = vec![2, 7, 9, 34, 36, 37, 40];
        }
        if self.primary_landmark_labels.is_empty() {
            self.primary_landmark_labels = vec![2, 3, 154];
        }

        let min_id = self.probe_id_range[0].clamp(1, 500);
        let max_id = self.probe_id_range[1].clamp(min_id, 500);
        self.probe_id_range = [min_id, max_id];

        let mode = self.auto_update_mode.trim().to_ascii_lowercase();
        self.auto_update_mode = if VALID_AUTO_UPDATE_MODES.contains(&mode.as_str()) {
            mode
        } else {
            "auto_download".to_owned()
        };
    }

    pub fn priority_for_map(&self, map_id: i32) -> i32 {
        self.map_priorities
            .get(&map_id.to_string())
            .copied()
            .unwrap_or(50 + map_id)
    }
}

/// Persisted user configuration (`config.json`, 26 preserved legacy fields + `hoyolab_sync`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppConfig {
    pub hotkey: String,
    pub hotkey_sync: String,
    pub hotkey_debug: String,
    pub hotkey_switch: String,
    pub hotkey_nav_mark: String,
    pub hotkey_nav_undo: String,
    pub hotkey_nav_guide: String,
    pub hotkey_nav_guide_next: String,
    pub hotkey_nav_next: String,
    pub hotkey_nav_layout: String,
    pub hotkey_nav_toggle: String,
    pub hotkey_nav_unlock: String,
    pub hotkey_radar_toggle: String,
    /// Schema version for hotkey default migration (`2` = Shift-free left-hand defaults).
    pub hotkey_profile_version: u32,
    pub hotkey_suppress_conflicts: bool,
    pub audio_enabled: bool,
    pub audio_volume: i32,
    pub chest_search_radius: f64,
    pub ws_port: u16,
    pub auto_sync_m: bool,
    /// Deprecated since v5.5.0: no runtime effect (switch hotkey is always on; feedback owner is
    /// auto-detected). Kept for `config.json`, `/status` and `/set_single_monitor` compatibility.
    pub single_monitor_mode: bool,
    pub language: String,
    pub minimap_tracking: bool,
    pub nav_hud_enabled: bool,
    pub nav_hud_capture_visible: bool,
    pub nav_hud_position: String,
    pub nav_hud_custom_pos: Option<[f64; 2]>,
    pub nav_hud_guide_zoom_mode: String,
    pub mmap_overlay_mode: String,
    /// Opt-in Native Floating Live Radar Map (`false` by default = strict zero-overhead).
    pub radar_enabled: bool,
    /// Optional relative `[x_rel, y_rel, w_rel, h_rel]` inside the game client viewport.
    pub radar_rect: Option<[f64; 4]>,
    /// Radar tile zoom level (`-1.2..=0.3`, default `-0.45`).
    pub radar_zoom: f64,
    /// Radar window opacity (`0.45..=1.0`, default `0.94`).
    pub radar_opacity: f64,
    pub lan_companion_enabled: bool,
    pub lan_companion_port: u16,
    /// App update policy: `"auto"` (background check + SHA-256 pre-download + restart/exit install),
    /// `"notify"` (check & notify only), or `"off"` (manual check only).
    pub app_update_mode: String,
    /// Legacy/mirror boolean kept in sync with `app_update_mode != "off"`.
    pub auto_check_app_updates: bool,
    pub hoyolab_sync: HoyolabSyncConfig,
    pub instructions: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "alt+f11".to_owned(),
            hotkey_sync: "alt+f11".to_owned(),
            hotkey_debug: "alt+f12".to_owned(),
            hotkey_switch: "alt+q".to_owned(),
            hotkey_nav_mark: DEFAULT_HOTKEY_NAV_MARK.to_owned(),
            hotkey_nav_undo: DEFAULT_HOTKEY_NAV_UNDO.to_owned(),
            hotkey_nav_guide: DEFAULT_HOTKEY_NAV_GUIDE.to_owned(),
            hotkey_nav_guide_next: DEFAULT_HOTKEY_NAV_GUIDE_NEXT.to_owned(),
            hotkey_nav_next: DEFAULT_HOTKEY_NAV_NEXT.to_owned(),
            hotkey_nav_layout: DEFAULT_HOTKEY_NAV_LAYOUT.to_owned(),
            hotkey_nav_toggle: DEFAULT_HOTKEY_NAV_TOGGLE.to_owned(),
            hotkey_nav_unlock: DEFAULT_HOTKEY_NAV_UNLOCK.to_owned(),
            hotkey_radar_toggle: DEFAULT_HOTKEY_RADAR_TOGGLE.to_owned(),
            hotkey_profile_version: CURRENT_HOTKEY_PROFILE_VERSION,
            hotkey_suppress_conflicts: true,
            audio_enabled: true,
            audio_volume: 50,
            chest_search_radius: 30.0,
            ws_port: crate::DEFAULT_SYNC_PORT,
            auto_sync_m: true,
            single_monitor_mode: false,
            language: "auto".to_owned(),
            minimap_tracking: true,
            nav_hud_enabled: true,
            nav_hud_capture_visible: false,
            nav_hud_position: "under_minimap".to_owned(),
            nav_hud_custom_pos: None,
            nav_hud_guide_zoom_mode: "cycle".to_owned(),
            mmap_overlay_mode: "full".to_owned(),
            radar_enabled: false,
            radar_rect: None,
            radar_zoom: DEFAULT_RADAR_ZOOM,
            radar_opacity: DEFAULT_RADAR_OPACITY,
            lan_companion_enabled: false,
            lan_companion_port: crate::DEFAULT_LAN_PORT,
            app_update_mode: "auto".to_owned(),
            auto_check_app_updates: true,
            hoyolab_sync: HoyolabSyncConfig::default(),
            instructions: DEFAULT_INSTRUCTIONS_VI.to_owned(),
        }
    }
}

impl<'de> Deserialize<'de> for AppConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = Value::deserialize(deserializer)?;
        let Some(map) = val.as_object() else {
            return Err(serde::de::Error::custom(
                "config root must be a JSON object",
            ));
        };
        Ok(Self::from_json_map(map))
    }
}

impl AppConfig {
    /// Parses a JSON object map with per-field tolerance matching `src/config.py`.
    pub fn from_json_map(map: &Map<String, Value>) -> Self {
        let mut cfg = Self::default();

        let resolved_sync = parse_hotkey_sync(map);
        cfg.hotkey = resolved_sync.clone();
        cfg.hotkey_sync = resolved_sync;

        cfg.hotkey_debug = parse_string_or(map.get("hotkey_debug"), &cfg.hotkey_debug);
        cfg.hotkey_switch = parse_string_or(map.get("hotkey_switch"), &cfg.hotkey_switch);
        cfg.hotkey_nav_mark = parse_string_or(map.get("hotkey_nav_mark"), &cfg.hotkey_nav_mark);
        cfg.hotkey_nav_undo = parse_string_or(map.get("hotkey_nav_undo"), &cfg.hotkey_nav_undo);
        cfg.hotkey_nav_guide = parse_string_or(map.get("hotkey_nav_guide"), &cfg.hotkey_nav_guide);
        cfg.hotkey_nav_guide_next =
            parse_string_or(map.get("hotkey_nav_guide_next"), &cfg.hotkey_nav_guide_next);
        cfg.hotkey_nav_next = parse_string_or(map.get("hotkey_nav_next"), &cfg.hotkey_nav_next);
        cfg.hotkey_nav_layout =
            parse_string_or(map.get("hotkey_nav_layout"), &cfg.hotkey_nav_layout);
        cfg.hotkey_nav_toggle =
            parse_string_or(map.get("hotkey_nav_toggle"), &cfg.hotkey_nav_toggle);
        cfg.hotkey_nav_unlock =
            parse_string_or(map.get("hotkey_nav_unlock"), &cfg.hotkey_nav_unlock);
        cfg.hotkey_radar_toggle =
            parse_string_or(map.get("hotkey_radar_toggle"), &cfg.hotkey_radar_toggle);

        // Migrate unmodified legacy v1 hotkey defaults (which used `Shift` = Genshin Sprint
        // and right-hand `K`/`N`) to the v2 left-hand `Alt`-only defaults, while preserving
        // any custom user bindings untouched.
        let loaded_profile_ver = map
            .get("hotkey_profile_version")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        if loaded_profile_ver < CURRENT_HOTKEY_PROFILE_VERSION {
            if normalize_hotkey(&cfg.hotkey_nav_mark) == "alt+k" {
                cfg.hotkey_nav_mark = DEFAULT_HOTKEY_NAV_MARK.to_owned();
            }
            if normalize_hotkey(&cfg.hotkey_nav_undo) == "alt+shift+k" {
                cfg.hotkey_nav_undo = DEFAULT_HOTKEY_NAV_UNDO.to_owned();
            }
            if normalize_hotkey(&cfg.hotkey_nav_next) == "alt+n" {
                cfg.hotkey_nav_next = DEFAULT_HOTKEY_NAV_NEXT.to_owned();
            }
            if normalize_hotkey(&cfg.hotkey_nav_layout) == "alt+shift+h" {
                cfg.hotkey_nav_layout = DEFAULT_HOTKEY_NAV_LAYOUT.to_owned();
            }
            if normalize_hotkey(&cfg.hotkey_radar_toggle) == "alt+shift+x" {
                cfg.hotkey_radar_toggle = DEFAULT_HOTKEY_RADAR_TOGGLE.to_owned();
            }
        }
        cfg.hotkey_profile_version = CURRENT_HOTKEY_PROFILE_VERSION;

        cfg.hotkey_suppress_conflicts = parse_bool_or(
            map.get("hotkey_suppress_conflicts"),
            cfg.hotkey_suppress_conflicts,
        );
        cfg.audio_enabled = parse_bool_or(map.get("audio_enabled"), cfg.audio_enabled);
        cfg.audio_volume = parse_volume(map.get("audio_volume"), cfg.audio_volume);
        cfg.chest_search_radius =
            parse_positive_f64(map.get("chest_search_radius"), cfg.chest_search_radius);
        cfg.ws_port = parse_port(map.get("ws_port"), cfg.ws_port);
        cfg.auto_sync_m = parse_bool_or(map.get("auto_sync_m"), cfg.auto_sync_m);
        cfg.single_monitor_mode =
            parse_bool_or(map.get("single_monitor_mode"), cfg.single_monitor_mode);
        cfg.language = parse_string_or(map.get("language"), &cfg.language);
        cfg.minimap_tracking = parse_bool_or(map.get("minimap_tracking"), cfg.minimap_tracking);
        cfg.nav_hud_enabled = parse_bool_or(map.get("nav_hud_enabled"), cfg.nav_hud_enabled);
        cfg.nav_hud_capture_visible = parse_bool_or(
            map.get("nav_hud_capture_visible"),
            cfg.nav_hud_capture_visible,
        );
        cfg.nav_hud_position = parse_string_or(map.get("nav_hud_position"), &cfg.nav_hud_position);
        cfg.nav_hud_custom_pos = parse_custom_pos(map.get("nav_hud_custom_pos"));
        cfg.nav_hud_guide_zoom_mode = parse_string_or(
            map.get("nav_hud_guide_zoom_mode"),
            &cfg.nav_hud_guide_zoom_mode,
        );
        cfg.mmap_overlay_mode = parse_string_or(
            map.get("mmap_overlay_mode"),
            &cfg.mmap_overlay_mode,
        );
        cfg.radar_enabled = parse_bool_or(map.get("radar_enabled"), cfg.radar_enabled);
        cfg.radar_rect = parse_radar_rect(map.get("radar_rect"));
        cfg.radar_zoom = parse_f64_or(map.get("radar_zoom"), cfg.radar_zoom);
        cfg.radar_opacity = parse_f64_or(map.get("radar_opacity"), cfg.radar_opacity);
        cfg.lan_companion_enabled =
            parse_bool_or(map.get("lan_companion_enabled"), cfg.lan_companion_enabled);
        cfg.lan_companion_port = parse_port(map.get("lan_companion_port"), cfg.lan_companion_port);
        if let Some(raw_mode) = map.get("app_update_mode").and_then(Value::as_str) {
            cfg.app_update_mode = raw_mode.to_owned();
            cfg.auto_check_app_updates = !raw_mode.trim().eq_ignore_ascii_case("off");
        } else {
            cfg.auto_check_app_updates =
                parse_bool_or(map.get("auto_check_app_updates"), cfg.auto_check_app_updates);
            cfg.app_update_mode = if cfg.auto_check_app_updates {
                "auto".to_owned()
            } else {
                "off".to_owned()
            };
        }

        if let Some(sync_obj) = map.get("hoyolab_sync").and_then(Value::as_object) {
            cfg.hoyolab_sync = HoyolabSyncConfig::from_json_map(sync_obj);
        }

        cfg.instructions = parse_string_or(map.get("instructions"), &cfg.instructions);

        cfg.normalize();
        cfg
    }

    /// Normalizes, clamps, and validates fields identically to `src/config.py`.
    pub fn normalize(&mut self) {
        let primary = normalize_hotkey(&self.hotkey);
        let sync = normalize_hotkey(&self.hotkey_sync);
        let resolved_sync = if !primary.is_empty() {
            primary
        } else if !sync.is_empty() {
            sync
        } else {
            "alt+f11".to_owned()
        };
        self.hotkey = resolved_sync.clone();
        self.hotkey_sync = resolved_sync;

        self.hotkey_debug = non_empty_or(&self.hotkey_debug, "alt+f12");
        self.hotkey_switch = non_empty_or(&self.hotkey_switch, "alt+q");
        self.hotkey_nav_mark = non_empty_or(&self.hotkey_nav_mark, DEFAULT_HOTKEY_NAV_MARK);
        self.hotkey_nav_undo = non_empty_or(&self.hotkey_nav_undo, DEFAULT_HOTKEY_NAV_UNDO);
        self.hotkey_nav_guide = non_empty_or(&self.hotkey_nav_guide, DEFAULT_HOTKEY_NAV_GUIDE);
        self.hotkey_nav_guide_next =
            non_empty_or(&self.hotkey_nav_guide_next, DEFAULT_HOTKEY_NAV_GUIDE_NEXT);
        self.hotkey_nav_next = non_empty_or(&self.hotkey_nav_next, DEFAULT_HOTKEY_NAV_NEXT);
        self.hotkey_nav_layout = non_empty_or(&self.hotkey_nav_layout, DEFAULT_HOTKEY_NAV_LAYOUT);
        self.hotkey_nav_toggle = non_empty_or(&self.hotkey_nav_toggle, DEFAULT_HOTKEY_NAV_TOGGLE);
        self.hotkey_nav_unlock = non_empty_or(&self.hotkey_nav_unlock, DEFAULT_HOTKEY_NAV_UNLOCK);
        self.hotkey_radar_toggle =
            non_empty_or(&self.hotkey_radar_toggle, DEFAULT_HOTKEY_RADAR_TOGGLE);
        if self.hotkey_profile_version < CURRENT_HOTKEY_PROFILE_VERSION {
            self.hotkey_profile_version = CURRENT_HOTKEY_PROFILE_VERSION;
        }

        self.audio_volume = self.audio_volume.clamp(0, 100);
        if !self.chest_search_radius.is_finite() || self.chest_search_radius <= 0.0 {
            self.chest_search_radius = 30.0;
        }
        if self.ws_port == 0 {
            self.ws_port = crate::DEFAULT_SYNC_PORT;
        }
        if self.lan_companion_port == 0 {
            self.lan_companion_port = crate::DEFAULT_LAN_PORT;
        }

        let lang = self.language.trim().to_ascii_lowercase();
        self.language = if VALID_LANGUAGES.contains(&lang.as_str()) {
            lang
        } else {
            "auto".to_owned()
        };

        let pos = self.nav_hud_position.trim().to_ascii_lowercase();
        self.nav_hud_position = if pos.is_empty() {
            "under_minimap".to_owned()
        } else {
            pos
        };

        self.nav_hud_custom_pos = self.nav_hud_custom_pos.and_then(|[fx, fy]| {
            if fx.is_finite()
                && fy.is_finite()
                && (0.0..=1.0).contains(&fx)
                && (0.0..=1.0).contains(&fy)
            {
                Some([
                    (fx * 10_000.0).round() / 10_000.0,
                    (fy * 10_000.0).round() / 10_000.0,
                ])
            } else {
                None
            }
        });

        let gzm = self.nav_hud_guide_zoom_mode.trim().to_ascii_lowercase();
        self.nav_hud_guide_zoom_mode = if VALID_GUIDE_ZOOM_MODES.contains(&gzm.as_str()) {
            gzm
        } else {
            "cycle".to_owned()
        };

        let mmo = self.mmap_overlay_mode.trim().to_ascii_lowercase();
        self.mmap_overlay_mode = if VALID_MMAP_OVERLAY_MODES.contains(&mmo.as_str()) {
            mmo
        } else {
            "full".to_owned()
        };

        self.radar_rect = self.radar_rect.and_then(|[x, y, w, h]| {
            if x.is_finite()
                && y.is_finite()
                && w.is_finite()
                && h.is_finite()
                && (0.0..=0.98).contains(&x)
                && (0.0..=0.98).contains(&y)
                && (0.05..=0.85).contains(&w)
                && (0.05..=0.85).contains(&h)
            {
                Some([
                    (x * 10_000.0).round() / 10_000.0,
                    (y * 10_000.0).round() / 10_000.0,
                    (w * 10_000.0).round() / 10_000.0,
                    (h * 10_000.0).round() / 10_000.0,
                ])
            } else {
                None
            }
        });
        self.radar_zoom = if self.radar_zoom.is_finite() {
            ((self.radar_zoom.clamp(RADAR_ZOOM_MIN, RADAR_ZOOM_MAX)) * 100.0).round() / 100.0
        } else {
            DEFAULT_RADAR_ZOOM
        };
        self.radar_opacity = if self.radar_opacity.is_finite() {
            ((self.radar_opacity.clamp(RADAR_OPACITY_MIN, RADAR_OPACITY_MAX)) * 100.0).round()
                / 100.0
        } else {
            DEFAULT_RADAR_OPACITY
        };

        if self.instructions.trim().is_empty() {
            self.instructions = DEFAULT_INSTRUCTIONS_VI.to_owned();
        }

        let app_upd = self.app_update_mode.trim().to_ascii_lowercase();
        self.app_update_mode = if VALID_APP_UPDATE_MODES.contains(&app_upd.as_str()) {
            app_upd
        } else if self.auto_check_app_updates {
            "auto".to_owned()
        } else {
            "off".to_owned()
        };
        self.auto_check_app_updates = self.app_update_mode != "off";

        self.hoyolab_sync.normalize();
    }

    pub fn from_json_str(raw: &str) -> CoreResult<Self> {
        let map: Map<String, Value> = serde_json::from_str(raw)?;
        Ok(Self::from_json_map(&map))
    }

    pub fn to_json_pretty(&self) -> CoreResult<String> {
        atomic_io::to_json_pretty_4(self)
    }

    pub fn save_to_file(&self, path: &Path) -> CoreResult<()> {
        atomic_io::write_json_atomic(path, self)
    }

    pub fn load_from_file(path: &Path) -> CoreResult<Self> {
        let raw = std::fs::read_to_string(path).map_err(CoreError::Io)?;
        Self::from_json_str(&raw)
    }

    /// Loads config from `path` if present; falls back to defaults without clobbering
    /// a corrupt user file, or creates a default file when missing.
    pub fn load_or_create(path: &Path) -> Self {
        if path.is_file() {
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(map) = serde_json::from_str::<Map<String, Value>>(&raw)
            {
                let prev_ver = map
                    .get("hotkey_profile_version")
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32;
                let cfg = Self::from_json_map(&map);
                if prev_ver < CURRENT_HOTKEY_PROFILE_VERSION {
                    let _ = cfg.save_to_file(path);
                }
                return cfg;
            }
            Self::default()
        } else {
            let cfg = Self::default();
            let _ = cfg.save_to_file(path);
            cfg
        }
    }
}

fn normalize_hotkey(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn non_empty_or(raw: &str, fallback: &str) -> String {
    let cleaned = normalize_hotkey(raw);
    if cleaned.is_empty() {
        fallback.to_owned()
    } else {
        cleaned
    }
}

fn parse_hotkey_sync(map: &Map<String, Value>) -> String {
    if let Some(s) = map.get("hotkey").and_then(Value::as_str) {
        let cleaned = normalize_hotkey(s);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    if let Some(s) = map.get("hotkey_sync").and_then(Value::as_str) {
        let cleaned = normalize_hotkey(s);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    "alt+f11".to_owned()
}

fn parse_string_or(val: Option<&Value>, default: &str) -> String {
    match val.and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => default.to_owned(),
    }
}

fn parse_bool_or(val: Option<&Value>, default: bool) -> bool {
    match val {
        Some(Value::Bool(b)) => *b,
        _ => default,
    }
}

fn parse_f64_or(val: Option<&Value>, default: f64) -> f64 {
    match val {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(default),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .unwrap_or(default),
        _ => default,
    }
}

fn parse_radar_rect(val: Option<&Value>) -> Option<[f64; 4]> {
    let arr = val?.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let x = parse_f64_or(Some(&arr[0]), f64::NAN);
    let y = parse_f64_or(Some(&arr[1]), f64::NAN);
    let w = parse_f64_or(Some(&arr[2]), f64::NAN);
    let h = parse_f64_or(Some(&arr[3]), f64::NAN);
    if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() {
        Some([x, y, w, h])
    } else {
        None
    }
}

fn parse_i32_val(val: &Value) -> Option<i32> {
    match val {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i32::try_from(i).ok()
            } else if let Some(u) = n.as_u64() {
                i32::try_from(u).ok()
            } else {
                None
            }
        }
        Value::String(s) => s.trim().parse::<i32>().ok(),
        _ => None,
    }
}

fn parse_i32_vec(val: Option<&Value>) -> Option<Vec<i32>> {
    let arr = val?.as_array()?;
    let out: Vec<i32> = arr.iter().filter_map(parse_i32_val).collect();
    Some(out)
}

fn parse_port(val: Option<&Value>, default: u16) -> u16 {
    match val {
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                if (1..=65535).contains(&u) {
                    return u as u16;
                }
            } else if let Some(i) = n.as_i64() {
                if (1..=65535).contains(&i) {
                    return i as u16;
                }
            } else if let Some(f) = n.as_f64()
                && f.is_finite()
                && f.fract() == 0.0
                && (1.0..=65535.0).contains(&f)
            {
                return f as u16;
            }
            default
        }
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(p) if (1..=65535).contains(&p) => p as u16,
            _ => default,
        },
        _ => default,
    }
}

fn parse_volume(val: Option<&Value>, default: i32) -> i32 {
    match val {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.clamp(0, 100) as i32
            } else if let Some(u) = n.as_u64() {
                u.min(100) as i32
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    f.round().clamp(0.0, 100.0) as i32
                } else {
                    default
                }
            } else {
                default
            }
        }
        Some(Value::String(s)) => match s.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => f.round().clamp(0.0, 100.0) as i32,
            _ => default,
        },
        _ => default,
    }
}

fn parse_positive_f64(val: Option<&Value>, default: f64) -> f64 {
    let parsed = match val {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(f) if f.is_finite() && f > 0.0 => f,
        _ => default,
    }
}

fn parse_custom_pos(val: Option<&Value>) -> Option<[f64; 2]> {
    let arr = val?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let parse_coord = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    let fx = parse_coord(&arr[0])?;
    let fy = parse_coord(&arr[1])?;
    if fx.is_finite() && fy.is_finite() && (0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)
    {
        Some([
            (fx * 10_000.0).round() / 10_000.0,
            (fy * 10_000.0).round() / 10_000.0,
        ])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_core_config_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn defaults_match_python_invariants() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.hotkey, "alt+f11");
        assert_eq!(cfg.hotkey_sync, "alt+f11");
        assert_eq!(cfg.hotkey_nav_mark, "alt+f");
        assert_eq!(cfg.hotkey_nav_undo, "alt+t");
        assert_eq!(cfg.hotkey_nav_next, "alt+e");
        assert_eq!(cfg.hotkey_nav_layout, "alt+f7");
        assert_eq!(cfg.hotkey_nav_unlock, "alt+f6");
        assert_eq!(cfg.hotkey_radar_toggle, "alt+`");
        assert_eq!(cfg.hotkey_profile_version, CURRENT_HOTKEY_PROFILE_VERSION);
        assert_eq!(cfg.ws_port, 8765);
        assert_eq!(cfg.lan_companion_port, 8766);
        assert!(!cfg.lan_companion_enabled);
        assert!(!cfg.nav_hud_capture_visible);
        assert_eq!(cfg.mmap_overlay_mode, "full");
        assert!(cfg.hotkey_suppress_conflicts);
        assert_eq!(cfg.hoyolab_sync.rpc_map_version, "4.5");
        assert_eq!(cfg.hoyolab_sync.probe_id_range, [1, 80]);
        assert_eq!(cfg.hoyolab_sync.auto_update_mode, "auto_download");
        assert_eq!(cfg.app_update_mode, "auto");
        assert!(cfg.auto_check_app_updates);
        assert_eq!(cfg.hoyolab_sync.priority_for_map(40), 2);
        assert_eq!(cfg.hoyolab_sync.priority_for_map(99), 149);
    }

    #[test]
    fn legacy_v1_hotkeys_migrate_to_shift_free_left_hand_preserving_custom() {
        let legacy_json = r#"{
            "hotkey_nav_mark": "alt+k",
            "hotkey_nav_undo": "alt+shift+k",
            "hotkey_nav_next": "mouse5",
            "hotkey_nav_layout": "alt+shift+h",
            "hotkey_radar_toggle": "alt+shift+x"
        }"#;
        let cfg = AppConfig::from_json_str(legacy_json).expect("legacy json");
        assert_eq!(cfg.hotkey_profile_version, CURRENT_HOTKEY_PROFILE_VERSION);
        assert_eq!(cfg.hotkey_nav_mark, "alt+f");
        assert_eq!(cfg.hotkey_nav_undo, "alt+t");
        assert_eq!(cfg.hotkey_nav_next, "mouse5", "custom binding must be preserved");
        assert_eq!(cfg.hotkey_nav_layout, "alt+f7");
        assert_eq!(cfg.hotkey_radar_toggle, "alt+`");
        assert_eq!(cfg.hotkey_nav_unlock, "alt+f6");

        // Once at v2, explicitly re-binding `alt+k` stays `alt+k`.
        let v2_json = r#"{
            "hotkey_profile_version": 2,
            "hotkey_nav_mark": "alt+k"
        }"#;
        let cfg2 = AppConfig::from_json_str(v2_json).expect("v2 json");
        assert_eq!(cfg2.hotkey_nav_mark, "alt+k");
    }

    #[test]
    fn json_roundtrip_and_normalization() {
        let raw = r#"{
            "hotkey": "ALT+F10",
            "audio_volume": 150,
            "language": "INVALID",
            "nav_hud_custom_pos": [0.123456, 0.5],
            "nav_hud_guide_zoom_mode": "LARGE"
        }"#;
        let cfg = AppConfig::from_json_str(raw).expect("valid config json");
        assert_eq!(cfg.hotkey, "alt+f10");
        assert_eq!(cfg.hotkey_sync, "alt+f10");
        assert_eq!(cfg.audio_volume, 100);
        assert_eq!(cfg.language, "auto");
        assert_eq!(cfg.nav_hud_custom_pos, Some([0.1235, 0.5]));
        assert_eq!(cfg.nav_hud_guide_zoom_mode, "large");
        // Missing `hoyolab_sync` block falls back cleanly to defaults
        assert_eq!(cfg.hoyolab_sync, HoyolabSyncConfig::default());

        let serialized = cfg.to_json_pretty().expect("serialize config");
        assert!(serialized.contains("\n    \"hotkey\": \"alt+f10\""));
        assert!(!serialized.contains("\\u"));
        let reparsed = AppConfig::from_json_str(&serialized).expect("reparse config");
        assert_eq!(cfg, reparsed);
    }

    #[test]
    fn hoyolab_sync_partial_override_and_validation() {
        let raw = r#"{
            "hoyolab_sync": {
                "base_api": "https://example.com/api/",
                "rpc_map_version": "6.0",
                "probe_id_range": [50, 10],
                "auto_update_mode": "bogus"
            }
        }"#;
        let cfg = AppConfig::from_json_str(raw).expect("valid json");
        assert_eq!(cfg.hoyolab_sync.base_api, "https://example.com/api");
        assert_eq!(cfg.hoyolab_sync.rpc_map_version, "6.0");
        assert_eq!(cfg.hoyolab_sync.probe_id_range, [50, 50]);
        assert_eq!(cfg.hoyolab_sync.auto_update_mode, "auto_download");
        assert_eq!(
            cfg.hoyolab_sync.default_map_ids,
            vec![2, 7, 9, 34, 36, 37, 40]
        );
    }

    #[test]
    fn out_of_range_custom_pos_is_cleared() {
        let raw = r#"{"nav_hud_custom_pos": [1.5, 0.2]}"#;
        let cfg = AppConfig::from_json_str(raw).expect("valid json");
        assert_eq!(cfg.nav_hud_custom_pos, None);
    }

    #[test]
    fn tolerant_per_field_fallbacks_preserve_other_fields() {
        for bad_port in ["0", "-1", "70000", "\"bad\"", "null"] {
            let raw = format!(
                r#"{{"lan_companion_port": {bad_port}, "ws_port": {bad_port}, "language": "vi", "audio_enabled": false}}"#
            );
            let cfg = AppConfig::from_json_str(&raw).expect("per-field tolerance");
            assert_eq!(cfg.lan_companion_port, 8766);
            assert_eq!(cfg.ws_port, 8765);
            assert_eq!(cfg.language, "vi");
            assert!(!cfg.audio_enabled);
        }

        let raw = r#"{
            "hotkey_sync": "ALT+F9",
            "audio_volume": 75.6,
            "chest_search_radius": -10,
            "nav_hud_custom_pos": ["x", 0.5],
            "hoyolab_sync": null,
            "minimap_tracking": null
        }"#;
        let cfg = AppConfig::from_json_str(raw).expect("tolerant parse");
        assert_eq!(cfg.hotkey, "alt+f9");
        assert_eq!(cfg.hotkey_sync, "alt+f9");
        assert_eq!(cfg.audio_volume, 76);
        assert_eq!(cfg.chest_search_radius, 30.0);
        assert_eq!(cfg.nav_hud_custom_pos, None);
        assert_eq!(cfg.hoyolab_sync, HoyolabSyncConfig::default());
        assert!(cfg.minimap_tracking);

        let neg_vol = AppConfig::from_json_str(r#"{"audio_volume": -5}"#).expect("neg vol");
        assert_eq!(neg_vol.audio_volume, 0);
        let bad_vol = AppConfig::from_json_str(r#"{"audio_volume": "bad"}"#).expect("bad vol");
        assert_eq!(bad_vol.audio_volume, 50);
    }

    #[test]
    fn invalid_json_syntax_still_returns_error() {
        let err = AppConfig::from_json_str("{not valid json").expect_err("must error");
        assert!(matches!(err, CoreError::Json(_)));
    }

    #[test]
    fn load_or_create_creates_missing_and_preserves_corrupt_file() {
        let base = make_temp_dir("load_or_create");
        let cfg_path = base.join("config.json");

        let created = AppConfig::load_or_create(&cfg_path);
        assert_eq!(created, AppConfig::default());
        assert!(cfg_path.is_file());
        let saved_text = std::fs::read_to_string(&cfg_path).expect("read saved config");
        assert!(saved_text.contains("\n    \"hotkey\": \"alt+f11\""));
        assert!(saved_text.contains("Cấu hình phím tắt"));

        // Corrupt user file must NOT be overwritten
        std::fs::write(&cfg_path, "{corrupt user edit").expect("write corrupt");
        let loaded = AppConfig::load_or_create(&cfg_path);
        assert_eq!(loaded, AppConfig::default());
        assert_eq!(
            std::fs::read_to_string(&cfg_path).expect("read corrupt"),
            "{corrupt user edit"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
