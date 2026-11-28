//! Acyclic cross-crate boundary traits implemented by `gh-win32`, `gh-net`, `gh-cv`, and `gh-app`.
//!
//! Defining these traits in `gh-core` eliminates all 7 cross-module dependency cycles
//! present in the original Python backend.

use crate::config::HoyolabSyncConfig;
use crate::frame::FrameView;
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Result of comparing online HoYoLAB `detail_v2.map_version` values against local `multimap_v4.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MapVersionCheckResult {
    /// Map IDs whose online `map_version` differs from local cache: `map_id -> (local, online)`.
    pub updated_maps: BTreeMap<i32, (String, String)>,
    /// Newly discovered active map IDs not present in local `multimap_v4.json`.
    pub new_map_ids: Vec<i32>,
    /// True when Map 2 (Teyvat) version changed, requiring global relocator index rebuild.
    pub teyvat_changed: bool,
}

impl MapVersionCheckResult {
    pub fn has_updates(&self) -> bool {
        !self.updated_maps.is_empty() || !self.new_map_ids.is_empty()
    }
}

/// OCR abstraction implemented by `gh-win32::win_ocr` (in-process WinRT `OcrEngine`)
/// and consumed by `gh-cv::locator` without creating a `gh-cv -> gh-win32` dependency.
pub trait OcrProvider: Send + Sync {
    fn recognize_text(&self, frame: FrameView<'_>) -> CoreResult<String>;
}

/// Non-blocking HD terrain tile provider implemented by `gh-net::hd_assets`
/// and consumed by `gh-cv::tile_cache` / `gh-cv::minimap_tracker`.
pub trait TileProvider: Send + Sync {
    fn request_tile(
        &self,
        map_id: i32,
        z: i32,
        x: i32,
        y: i32,
        async_fetch: bool,
    ) -> Option<Vec<u8>>;

    /// Monotonic count of tiles newly persisted to disk by background/async fetches.
    fn downloads(&self) -> u64 {
        0
    }
}

/// Non-blocking underground floor overlay source implemented by `gh-net::hd_assets`
/// and consumed by `gh-cv::tile_cache` / `gh-cv::minimap_tracker`.
pub trait UndergroundOverlaySource: Send + Sync {
    fn get_overlay_bytes(&self, map_id: i32, floor_id: &str, async_fetch: bool) -> Option<Vec<u8>>;
}

/// Hotkey conflict analyzer and external remapper implemented by `gh-win32::hotkey_conflicts`
/// and invoked by `gh-net::sync_server` without creating a `gh-net -> gh-win32` dependency.
pub trait ConflictAdvisor: Send + Sync {
    fn check_conflicts(&self, request: &Value) -> CoreResult<Value>;
    fn remap_external(&self, request: &Value) -> CoreResult<Value>;
    fn restore_external(&self, request: &Value) -> CoreResult<Value>;
}

/// Diagnostic bundle & user note service implemented by `gh-app::diagnostics`.
pub trait DiagnosticService: Send + Sync {
    fn trigger_diagnostic(&self, note: Option<&str>, source: Option<&str>) -> CoreResult<Value>;
    fn save_diagnostic_note(&self, folder_name: &str, note: &str) -> CoreResult<Value>;

    fn save_diagnostic_note_ex(
        &self,
        folder_name: &str,
        note: &str,
        create_zip: bool,
    ) -> CoreResult<Value> {
        let _ = create_zip;
        self.save_diagnostic_note(folder_name, note)
    }

    fn trigger_diagnostic_ex(&self, req: &Value) -> CoreResult<Value> {
        let note = req.get("note").and_then(Value::as_str);
        let source = req.get("source").and_then(Value::as_str);
        self.trigger_diagnostic(note, source)
    }

    fn attach_diagnostic_browser(
        &self,
        _folder_name: &str,
        _png_bytes: &[u8],
    ) -> CoreResult<Value> {
        Err(CoreError::Unimplemented(
            "attach_diagnostic_browser not implemented",
        ))
    }

    fn open_diagnostics_folder(&self, _folder_name: Option<&str>) -> CoreResult<Value> {
        Err(CoreError::Unimplemented(
            "open_diagnostics_folder not implemented",
        ))
    }

    fn open_diagnostics_folder_ex(
        &self,
        folder_name: Option<&str>,
        ensure_zip: bool,
    ) -> CoreResult<Value> {
        let _ = ensure_zip;
        self.open_diagnostics_folder(folder_name)
    }
}

/// OS locale probe implemented by `gh-win32::locale` and consumed by `gh-core::i18n`.
pub trait LocaleProvider: Send + Sync {
    fn system_language_tag(&self) -> String;
}

/// First-run & auto-update HoYoLAB dataset hydration runner implemented by `gh-net::hoyolab_sync`
/// and orchestrated by `gh-core::assets`.
pub trait DatasetSyncRunner: Send + Sync {
    fn check_online_map_versions(
        &self,
        sync_cfg: &HoyolabSyncConfig,
        local_versions: &BTreeMap<i32, String>,
    ) -> CoreResult<MapVersionCheckResult>;

    fn hydrate_into(&self, staging_dir: &Path, sync_cfg: &HoyolabSyncConfig) -> CoreResult<()>;
}

/// Application callback bridge consumed by `gh-net::sync_server` and wired in `gh-app::callbacks`.
pub trait ServerCallbacks: Send + Sync {
    fn get_status_overrides(&self) -> Value;
    fn on_engine_state_change(&self, active: bool) -> CoreResult<bool>;
    fn on_auto_sync_change(&self, enabled: bool) -> CoreResult<bool>;
    fn on_single_monitor_change(&self, enabled: bool) -> CoreResult<bool>;
    fn on_action_feedback(&self, payload: &Value) -> CoreResult<Value>;
    fn on_quick_switch(&self) -> CoreResult<String>;
    fn on_shutdown(&self);
    fn on_set_hotkeys(&self, payload: &Value) -> CoreResult<Value>;
    fn on_set_language(&self, lang: &str) -> CoreResult<Value>;
    fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool>;
    fn on_set_nav_hud(&self, payload: &Value) -> CoreResult<Value>;
    fn on_set_audio(&self, payload: &Value) -> CoreResult<Value>;
    fn on_nearby_pins(&self, payload: &Value) -> CoreResult<usize>;
    fn on_nav_command(&self, payload: &Value) -> CoreResult<Value>;
    fn on_plan_tsp_route(&self, payload: &Value) -> CoreResult<Value>;
    fn on_lan_companion(&self, payload: &Value) -> CoreResult<Value>;
    fn on_launch_borderless(&self, payload: &Value) -> CoreResult<Value>;
    fn on_cross_map_export(&self, payload: &Value) -> CoreResult<Value>;
    fn on_route_command(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({"ok": true}))
    }
    fn on_set_radar(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({"ok": true}))
    }
    fn on_lan_pins(&self, _payload: &Value) {}
    fn on_app_update(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({"ok": true}))
    }
    fn on_calibrate_provider(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(serde_json::json!({"ok": false, "error": "Provider calibration not configured"}))
    }
    fn on_active_provider_change(&self, _provider_id: &str) {}
}
