//! Shared runtime state and cross-subsystem actions for `gh-app`.

use crate::diagnostics::{DiagnosticManager, MinimapPreLostRingBuffer};
use crate::minimap_recorder::MinimapRecorder;
use crate::updater::AppUpdater;
use crate::mmap_guide::{
    FallbackInput, GuideInput, SINGLE_PIN_MMAP_DISTANCE_M, build_single_pin_route_doc,
    compute_fallback_scene, compute_guide_scene, section_area_name,
};
use crate::mmap_view::MMapViewEngine;
use gh_core::config::{AppConfig, VALID_MMAP_OVERLAY_MODES};
use gh_core::frame::RectI32;
use gh_core::i18n::{get_language, set_language, t};
use gh_core::map_bridge::PerMapAffineRegistry;
use gh_core::multimap::MultimapDb;
use gh_core::nav_controller::{FloorResolver, NavPin, NavigationController};
use gh_core::paths::AppPaths;
use gh_core::protocol::format_hotkey_display_name;
use gh_core::route_doc::{
    RouteStopKind, delete_saved_route, list_saved_routes, load_saved_route,
    parse_hoyolab_route_share_url, parse_route_auto, save_route_to_library,
};
use gh_core::traits::ConflictAdvisor;
use gh_core::{CoreError, CoreResult, DEFAULT_SYNC_HOST};
use gh_cv::locator::{LocateResult, MapLocator};
use gh_cv::map_state::MapScreenStateMachine;
use gh_cv::minimap_tracker::HybridMinimapTracker;
use gh_cv::view_tracker::ViewTransform;
use gh_hud::mmap_overlay::{
    MMapGuideScene, MMapOverlayController, MMapOverlayMode, parse_hex_rgba,
};
use gh_hud::nav_hud::{HudPalette, NavMiniHud, PositionMode};
use gh_hud::radar_map::{
    DEFAULT_RADAR_ROUTE_COLOR, RadarAssetSource, RadarMapController, RadarPin, RadarRouteLeg,
    RadarScene, RadarTargetHint, parse_radar_pins_payload,
};
use gh_hud::toast::{HudToastOverlay, ToastKind};
use gh_net::hd_assets::{HoyolabHdTileProvider, UndergroundOverlayProvider};
use gh_net::lan_companion::LanCompanionServer;
use gh_net::pin_guide::{HoYoLABLabelCatalog, PinGuide, PinGuideFetcher, PinIconCache};
use gh_net::sync_server::{HOTKEY_ACTIONS, SyncServer};
use gh_win32::audio::AudioFeedback;
use gh_win32::capture::{ScreenCapturer, WindowFinder};
use gh_win32::game_settings::UipiStatus;
use gh_win32::hotkey::UnifiedHotkeyManager;
use gh_win32::hotkey_conflicts::HotkeyConflictAdvisor;
use gh_win32::locale::Win32LocaleProvider;
use gh_win32::switcher::WindowSwitcher;
use gh_win32::tray::{SystemTrayApp, TrayState};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime};
use tokio::sync::watch;

pub const WINDOW_TITLES: [&str; 2] = ["Genshin Impact", "原神"];
pub const NAV_MARK_CONFIRM_DIST_M: f64 = 25.0;

#[derive(Debug, Clone, Default)]
pub struct GameState {
    pub hwnd: Option<isize>,
    pub rect: Option<RectI32>,
    pub last_pos: Option<Value>,
    pub uipi_status: Option<UipiStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteState {
    pub legs: Vec<Value>,
    pub last_route: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct NavMarkConfirm {
    pub pin_id: Option<String>,
    pub until: Option<Instant>,
}

pub struct AppFloorResolver {
    pub locator: Option<Arc<MapLocator>>,
    pub maps_db: Arc<MultimapDb>,
}

impl FloorResolver for AppFloorResolver {
    fn infer_unique_underground_floor(
        &self,
        map_id: i32,
        lat: f64,
        lng: f64,
    ) -> Option<(String, String)> {
        if let Some(loc) = &self.locator {
            loc.infer_unique_underground_floor(map_id, lat, lng, 15.0)
        } else {
            self.maps_db
                .infer_unique_underground_floor(map_id, lat, lng, 15.0)
                .map(|(fid, lbl)| (fid.to_string(), lbl))
        }
    }

    fn resolve_floor_label(&self, map_id: i32, floor_id: &str) -> Option<String> {
        let lbl = if let Some(loc) = &self.locator {
            loc.resolve_floor_label_str(map_id, floor_id)
        } else {
            let fid: i32 = floor_id.trim().parse().ok()?;
            self.maps_db.resolve_floor_label(map_id, fid)?
        };
        if lbl.is_empty() { None } else { Some(lbl) }
    }
}

pub fn json_truthy(v: Option<&Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn json_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

pub fn json_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn json_str_trimmed(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub fn get_hotkey_field(cfg: &AppConfig, action: &str) -> String {
    match action {
        "sync" => cfg.hotkey_sync.clone(),
        "debug" => cfg.hotkey_debug.clone(),
        "switch" => cfg.hotkey_switch.clone(),
        "nav_mark" => cfg.hotkey_nav_mark.clone(),
        "nav_undo" => cfg.hotkey_nav_undo.clone(),
        "nav_guide" => cfg.hotkey_nav_guide.clone(),
        "nav_next" => cfg.hotkey_nav_next.clone(),
        "nav_layout" => cfg.hotkey_nav_layout.clone(),
        "nav_toggle" => cfg.hotkey_nav_toggle.clone(),
        "nav_unlock" => cfg.hotkey_nav_unlock.clone(),
        "radar_toggle" => cfg.hotkey_radar_toggle.clone(),
        _ => String::new(),
    }
}

pub fn set_hotkey_field(cfg: &mut AppConfig, action: &str, new_hk: &str) -> bool {
    let norm = new_hk.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return false;
    }
    match action {
        "sync" => {
            cfg.hotkey = norm.clone();
            cfg.hotkey_sync = norm;
        }
        "debug" => cfg.hotkey_debug = norm,
        "switch" => cfg.hotkey_switch = norm,
        "nav_mark" => cfg.hotkey_nav_mark = norm,
        "nav_undo" => cfg.hotkey_nav_undo = norm,
        "nav_guide" => cfg.hotkey_nav_guide = norm,
        "nav_next" => cfg.hotkey_nav_next = norm,
        "nav_layout" => cfg.hotkey_nav_layout = norm,
        "nav_toggle" => cfg.hotkey_nav_toggle = norm,
        "nav_unlock" => cfg.hotkey_nav_unlock = norm,
        "radar_toggle" => cfg.hotkey_radar_toggle = norm,
        _ => return false,
    }
    true
}

pub fn read_config_file_stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((mtime, meta.len()))
}

pub struct AppState {
    pub paths: AppPaths,
    pub config_path: PathBuf,
    pub config: Arc<RwLock<AppConfig>>,
    pub cli_port: Option<u16>,
    pub cli_lan_port: Option<u16>,
    pub disk_ports: Mutex<(u16, u16)>,
    pub config_io_lock: Mutex<()>,
    pub last_self_saved_mtime: Mutex<Option<(SystemTime, u64)>>,
    pub engine_active: AtomicBool,
    pub anchor_seq: AtomicU64,
    pub game_state: Mutex<GameState>,
    pub nav_state_snapshot: Mutex<Option<Value>>,
    pub nav_controller: Mutex<NavigationController>,
    pub route_state: Mutex<RouteState>,
    pub nav_mark_confirm: Mutex<NavMarkConfirm>,
    pub tracker: Mutex<Option<(HybridMinimapTracker, f64)>>,
    pub locator: Option<Arc<MapLocator>>,
    pub state_machine: Mutex<MapScreenStateMachine>,
    pub capturer: Arc<ScreenCapturer>,
    pub server: SyncServer,
    pub lan_companion: Arc<LanCompanionServer>,
    pub nav_hud: Arc<NavMiniHud>,
    pub mmap_overlay: Arc<MMapOverlayController>,
    /// Live M-Map view tracking for route guidance (`None` without a map locator).
    pub mmap_view: Option<Arc<MMapViewEngine>>,
    /// Opt-in Native Floating Live Radar Map (`None` when disabled = strict zero-overhead).
    pub radar_map: Mutex<Option<RadarMapController>>,
    pub radar_active: AtomicBool,
    pub radar_pins: Mutex<Option<(i32, Arc<Vec<RadarPin>>)>>,
    pub pin_icon_cache: PinIconCache,
    pub hud_toast: Arc<HudToastOverlay>,
    pub audio: Arc<AudioFeedback>,
    pub tray: Mutex<Option<Arc<SystemTrayApp>>>,
    pub hotkey_mgr: Arc<UnifiedHotkeyManager>,
    pub conflict_advisor: Arc<HotkeyConflictAdvisor>,
    pub guide_fetcher: PinGuideFetcher,
    pub label_catalog: HoYoLABLabelCatalog,
    pub maps_db: Arc<MultimapDb>,
    pub affine_registry: PerMapAffineRegistry,
    pub hd_provider: Arc<HoyolabHdTileProvider>,
    pub ug_provider: Arc<UndergroundOverlayProvider>,
    pub minimap_recorder: Mutex<Option<Arc<MinimapRecorder>>>,
    pub minimap_ring: MinimapPreLostRingBuffer,
    pub diagnostics: Arc<DiagnosticManager>,
    pub updater: Arc<AppUpdater>,
    pub stop_tx: watch::Sender<bool>,
    pub start_instant: Instant,
}

impl AppState {
    pub fn floor_resolver(&self) -> AppFloorResolver {
        AppFloorResolver {
            locator: self.locator.clone(),
            maps_db: Arc::clone(&self.maps_db),
        }
    }

    pub fn next_tracker_ts(&self) -> f64 {
        let now = self.start_instant.elapsed().as_secs_f64();
        let mut guard = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, ref mut last_ts)) = *guard {
            let next = if now < *last_ts { *last_ts } else { now };
            *last_ts = next;
            next
        } else {
            now
        }
    }

    pub fn pause_minimap_tracker(&self, reason: &str) {
        let now = self.start_instant.elapsed().as_secs_f64();
        let mut guard = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ref mut tracker, ref mut last_ts)) = *guard {
            let ts = if now < *last_ts { *last_ts } else { now };
            *last_ts = ts;
            let _ = tracker.pause(Some(ts), reason, 0.0);
        }
    }

    fn save_config_locked(&self, cfg: &AppConfig) -> CoreResult<()> {
        self.server.update_config(cfg.clone());
        let mut disk_cfg = cfg.clone();
        {
            let mut disk = self.disk_ports.lock().unwrap_or_else(|e| e.into_inner());
            if self.cli_port.is_some() {
                disk_cfg.ws_port = disk.0;
            } else {
                disk.0 = disk_cfg.ws_port;
            }
            if self.cli_lan_port.is_some() {
                disk_cfg.lan_companion_port = disk.1;
            } else {
                disk.1 = disk_cfg.lan_companion_port;
            }
        }
        disk_cfg.save_to_file(&self.config_path)?;
        if let Some(stamp) = read_config_file_stamp(&self.config_path) {
            *self
                .last_self_saved_mtime
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(stamp);
        }
        Ok(())
    }

    pub fn save_config(&self, cfg: &AppConfig) -> CoreResult<()> {
        let _io_guard = self
            .config_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.save_config_locked(cfg)
    }

    pub fn mutate_and_save_config<R>(
        &self,
        mutator: impl FnOnce(&mut AppConfig) -> R,
    ) -> (AppConfig, R, CoreResult<()>) {
        let _io_guard = self
            .config_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cfg_clone, out) = {
            let mut cfg = self.config.write().expect("config lock poisoned");
            let out = mutator(&mut cfg);
            (cfg.clone(), out)
        };
        let save_res = self.save_config_locked(&cfg_clone);
        (cfg_clone, out, save_res)
    }

    pub fn refresh_tray_state(&self) {
        let tray_opt = self
            .tray
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(tray) = tray_opt else {
            return;
        };
        let active = self.engine_active.load(Ordering::Acquire);
        if !active {
            tray.set_state(TrayState::Paused, "", Some(false));
        } else if self.server.has_active_client(10.0) {
            tray.set_state(TrayState::Connected, "", Some(true));
        } else {
            tray.set_state(TrayState::Idle, "", Some(true));
        }
    }

    pub fn build_lan_state_payload(&self) -> Value {
        let last_pos = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pos
            .clone();
        let legs = self
            .route_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .legs
            .clone();

        let mut payload = Map::new();
        if let Some(Value::Object(pos_obj)) = last_pos {
            if let (Some(lat), Some(lng)) = (
                json_f64(pos_obj.get("lat")),
                json_f64(pos_obj.get("lng")),
            ) {
                payload.insert("lat".to_owned(), json!(lat));
                payload.insert("lng".to_owned(), json!(lng));
            }
            if let Some(mid) = json_i64(pos_obj.get("map_id")) {
                payload.insert("map_id".to_owned(), json!(mid));
            }
            if let Some(h) = json_f64(pos_obj.get("heading")) {
                payload.insert("heading".to_owned(), json!(h));
            }
            if let Some(ch) = json_f64(pos_obj.get("camera_heading")) {
                payload.insert("camera_heading".to_owned(), json!(ch));
            }
            let is_ug = json_truthy(pos_obj.get("is_underground"), false);
            payload.insert("is_underground".to_owned(), Value::Bool(is_ug));
            if let Some(fid) = pos_obj.get("floor_id")
                && !fid.is_null()
            {
                payload.insert("floor_id".to_owned(), fid.clone());
            }
            let fname = pos_obj
                .get("floor_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(if is_ug { "B1" } else { "G" });
            payload.insert("floor_name".to_owned(), Value::String(fname.to_owned()));
        }

        {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            let cur_target = nav
                .current_target()
                .map(NavPin::to_wire_json)
                .unwrap_or(Value::Null);
            let target_locked = nav.locked_target_pin().is_some();
            payload.insert("target_pin".to_owned(), cur_target);
            payload.insert("target_locked".to_owned(), Value::Bool(target_locked));
            let excluded = nav.excluded_ids();
            let nearby: Vec<Value> = nav
                .pins()
                .into_iter()
                .filter(|p| !excluded.contains(&p.id))
                .take(25)
                .map(|p| {
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "category": p.category,
                        "lat": p.lat,
                        "lng": p.lng,
                        "map_id": p.map_id,
                        "label_id": p.label_id,
                        "floor_id": p.floor_id,
                        "floor_name": p.floor_name_str(),
                        "is_underground": p.is_underground,
                    })
                })
                .collect();
            payload.insert("nearby_pins".to_owned(), Value::Array(nearby));
        }

        payload.insert(
            "anchor_seq".to_owned(),
            json!(self.anchor_seq.load(Ordering::Acquire)),
        );
        payload.insert("route_legs".to_owned(), Value::Array(legs));
        Value::Object(payload)
    }

    pub fn push_lan_snapshot(&self, force: bool) {
        if !self.lan_companion.is_running() {
            return;
        }
        let payload = self.build_lan_state_payload();
        self.lan_companion.broadcast_state(payload, force);
    }

    pub fn handle_lan_client_message(&self, msg: Value) {
        let Some(obj) = msg.as_object() else {
            return;
        };
        let msg_type = json_str_trimmed(obj.get("type")).to_ascii_uppercase();
        match msg_type.as_str() {
            "MARK_PIN" => {
                let pid = json_str_trimmed(obj.get("pin_id"));
                if pid.is_empty() {
                    return;
                }
                let action = if json_str_trimmed(obj.get("action")).eq_ignore_ascii_case("unmark") {
                    "unmark"
                } else {
                    "mark"
                };
                let map_id = json_i64(obj.get("map_id")).unwrap_or(2) as i32;
                let lat = json_f64(obj.get("lat")).filter(|v| v.is_finite());
                let lng = json_f64(obj.get("lng")).filter(|v| v.is_finite());
                let name = {
                    let s = json_str_trimmed(obj.get("name"));
                    if s.is_empty() {
                        format!("Pin {pid}")
                    } else {
                        s
                    }
                };
                let floor_id = {
                    let s = json_str_trimmed(obj.get("floor_id"));
                    if s.is_empty() { None } else { Some(s) }
                };
                let is_ug = json_truthy(obj.get("is_underground"), floor_id.is_some());

                let resolved_pin = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    let existing = nav.get_pin(&pid).cloned();
                    let pin = existing.unwrap_or_else(|| NavPin {
                        id: pid.clone(),
                        map_id,
                        lat: lat.unwrap_or(0.0),
                        lng: lng.unwrap_or(0.0),
                        name,
                        floor_id,
                        is_underground: is_ug,
                        ..Default::default()
                    });
                    if nav.get_pin(&pid).is_none() {
                        let mut merged: Vec<NavPin> = nav.pins().into_iter().cloned().collect();
                        merged.push(pin.clone());
                        nav.set_pins(merged, false, None);
                    }
                    if action == "mark" {
                        nav.mark_collected(Some(&pid));
                    } else {
                        nav.rollback_collected(&pid);
                        nav.protect_pin(&pid, 15.0);
                    }
                    pin
                };

                let sent = self.broadcast_nav_mark(&resolved_pin, action);
                self.push_lan_snapshot(true);
                if action == "mark" {
                    self.maybe_notify_route_section_completion();
                }
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": sent || action == "mark" || action == "unmark",
                    "action": action,
                    "pin_id": pid,
                }));
            }
            "SELECT_TARGET" => {
                let pid = json_str_trimmed(obj.get("pin_id"));
                if pid.is_empty() {
                    return;
                }
                let map_id = json_i64(obj.get("map_id")).unwrap_or(2) as i32;
                let lat = json_f64(obj.get("lat")).filter(|v| v.is_finite());
                let lng = json_f64(obj.get("lng")).filter(|v| v.is_finite());
                let name = {
                    let s = json_str_trimmed(obj.get("name"));
                    if s.is_empty() {
                        format!("Pin {pid}")
                    } else {
                        s
                    }
                };
                let floor_id = {
                    let s = json_str_trimmed(obj.get("floor_id"));
                    if s.is_empty() { None } else { Some(s) }
                };
                let is_ug = json_truthy(obj.get("is_underground"), floor_id.is_some());

                let selected = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    if nav.get_pin(&pid).is_none()
                        && let (Some(plat), Some(plng)) = (lat, lng)
                    {
                        let mut merged: Vec<NavPin> = nav.pins().into_iter().cloned().collect();
                        merged.push(NavPin {
                            id: pid.clone(),
                            map_id,
                            lat: plat,
                            lng: plng,
                            name,
                            floor_id,
                            is_underground: is_ug,
                            ..Default::default()
                        });
                        nav.set_pins(merged, false, None);
                    }
                    nav.protect_pin(&pid, 15.0);
                    nav.select_target(&pid).is_some()
                };
                self.push_lan_snapshot(true);
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": selected,
                    "action": "select_target",
                    "pin_id": pid,
                }));
            }
            "UNDO_LAST" => {
                self.handle_nav_undo_hotkey();
            }
            "PLAN_TSP" => {
                self.server
                    .broadcast(json!({"type": "GH_CMD_PLAN_TSP"}), false, true);
            }
            "SET_FLOOR_PC" => {
                self.server.broadcast(
                    json!({
                        "type": "LAN_SET_FLOOR",
                        "floor_id": obj.get("floor_id").cloned().unwrap_or(Value::Null),
                        "floor_name": json_str_trimmed(obj.get("floor_name")),
                    }),
                    false,
                    true,
                );
            }
            "SELECT_ROUTE_SECTION" => {
                let idx = json_i64(obj.get("section_index")).unwrap_or(0).max(0) as usize;
                let ok = self
                    .handle_route_command(&json!({
                        "action": "section",
                        "section_index": idx,
                    }))
                    .is_ok();
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": ok,
                    "action": "select_route_section",
                    "section_index": idx,
                }));
            }
            "LOCK_TARGET" => {
                let pid = json_str_trimmed(obj.get("pin_id"));
                let map_id = json_i64(obj.get("map_id")).unwrap_or(2) as i32;
                let lat = json_f64(obj.get("lat")).filter(|v| v.is_finite());
                let lng = json_f64(obj.get("lng")).filter(|v| v.is_finite());
                if pid.is_empty() || lat.is_none() || lng.is_none() {
                    return;
                }
                let name = {
                    let s = json_str_trimmed(obj.get("name"));
                    if s.is_empty() { format!("Pin {pid}") } else { s }
                };
                let floor_id = {
                    let s = json_str_trimmed(obj.get("floor_id"));
                    if s.is_empty() { None } else { Some(s) }
                };
                let is_ug = json_truthy(obj.get("is_underground"), floor_id.is_some());
                let label_id = json_i64(obj.get("label_id")).filter(|&v| v > 0);
                let mut pin = NavPin {
                    id: pid.clone(),
                    map_id,
                    lat: lat.unwrap_or(0.0),
                    lng: lng.unwrap_or(0.0),
                    name: name.clone(),
                    floor_id,
                    is_underground: is_ug,
                    label_id,
                    ..Default::default()
                };
                if let Some(cached) = self.guide_fetcher.get_cached(&pin.id, None) {
                    if !cached.description.is_empty() {
                        pin.description = cached.description;
                    }
                    if !cached.image_url.is_empty() {
                        pin.image_url = cached.image_url;
                    }
                }
                let locked = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    nav.lock_target_pin(pin)
                };
                let disp = locked.display_name();
                self.nav_flash("hud.flash.locked_target", HudPalette::Info, 2.8, &[("name", &disp)]);
                self.broadcast_target_lock_state();
                self.push_lan_snapshot(true);
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": true,
                    "action": "lock_target",
                    "pin_id": pid,
                }));
            }
            "UNLOCK_TARGET" => {
                let cleared = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    nav.clear_target_lock()
                };
                if cleared {
                    self.nav_flash("hud.flash.unlocked_target", HudPalette::Info, 2.0, &[]);
                    self.audio.play("info", false);
                }
                self.broadcast_target_lock_state();
                self.push_lan_snapshot(true);
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": true,
                    "action": "unlock_target",
                    "cleared": cleared,
                }));
            }
            "RESET_ROUTE" => {
                let ok = self
                    .handle_route_command(&json!({ "action": "reset" }))
                    .is_ok();
                self.lan_companion.send_event_to_clients(json!({
                    "type": "ACTION_ACK",
                    "ok": ok,
                    "action": "reset_route",
                }));
            }
            "REQUEST_SYNC" => {
                self.push_lan_snapshot(true);
            }
            _ => {}
        }
    }

    pub fn has_active_route_doc(&self) -> bool {
        self.nav_controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_route_doc()
            .is_some()
    }

    /// True when M-Map overlay guidance should track the M-Map view: either an active route
    /// document is loaded, OR a single locked pin is on another map / far away (> 180 m).
    pub fn has_mmap_guide_target(&self) -> bool {
        let locked = {
            let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            if nav.active_route_doc().is_some() {
                return true;
            }
            nav.locked_target_pin().cloned()
        };
        let Some(pin) = locked else {
            return false;
        };
        let last_pos = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pos
            .clone();
        let Some(lp) = last_pos else {
            return true;
        };
        let p_mid = json_i64(lp.get("map_id")).map(|v| v as i32);
        if p_mid.is_some_and(|mid| mid != pin.map_id) {
            return true;
        }
        match (json_f64(lp.get("lat")), json_f64(lp.get("lng"))) {
            (Some(plat), Some(plng)) if plat.is_finite() && plng.is_finite() => {
                (pin.lat - plat).hypot(pin.lng - plng) > SINGLE_PIN_MMAP_DISTANCE_M
            }
            _ => true,
        }
    }

    pub fn mmap_guide_map_id(&self) -> i32 {
        let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pin) = nav.locked_target_pin() {
            return pin.map_id;
        }
        nav.active_route_doc().map_or(2, |(d, _)| d.map_id)
    }

    pub fn push_route_state_update(&self, status_json: &Value) {
        self.lan_companion.update_route_snapshot(status_json.clone());
        self.push_lan_snapshot(true);
        let mut bcast = status_json.as_object().cloned().unwrap_or_default();
        bcast.insert("type".to_owned(), Value::String("ROUTE_UPDATED".to_owned()));
        self.server.broadcast(Value::Object(bcast), false, false);
    }

    pub fn notify_route_loaded_in_game(&self, status_json: &Value) {
        let title = status_json
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Route");
        let secs = status_json
            .get("section_count")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .to_string();
        let game_rect = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rect;
        let toast_title = t("toast.route.loaded_title", &[("title", title)]);
        let toast_detail = t("toast.route.loaded_detail", &[("sections", &secs)]);
        self.hud_toast
            .show_toast(&toast_title, &toast_detail, ToastKind::Info, 4200, game_rect);
        self.nav_flash(
            "toast.route.loaded_title",
            HudPalette::Info,
            3.2,
            &[("title", title)],
        );
    }

    pub fn maybe_notify_route_section_completion(&self) {
        let evt = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            nav.check_and_advance_completed_section()
        };
        let Some(evt) = evt else {
            return;
        };
        let st = self
            .nav_controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route_status_json();
        self.push_route_state_update(&st);
        if let Some(next_idx) = evt.next_section_index {
            let sec_num = format!("S{}", next_idx + 1);
            let done_num = format!("S{}", evt.finished_section_index + 1);
            self.nav_flash(
                "hud.route.section_done",
                HudPalette::Success,
                3.4,
                &[("done", &done_num), ("next", &sec_num)],
            );
        } else {
            let title = st.get("title").and_then(Value::as_str).unwrap_or("");
            self.nav_flash("hud.route.all_done", HudPalette::Success, 4.0, &[("title", title)]);
        }
    }

    pub fn handle_route_command(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let action = json_str_trimmed(req.get("action")).to_ascii_lowercase();
        let action = if action.is_empty() {
            if req.contains_key("sections")
                || req.contains_key("data")
                || req.contains_key("format")
                || req.contains_key("raw")
                || req.contains_key("document")
            {
                "load".to_owned()
            } else {
                "status".to_owned()
            }
        } else {
            action
        };

        match action.as_str() {
            "status" => {
                let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                Ok(nav.route_status_json())
            }
            "load" | "import" => {
                let doc = parse_route_auto(payload, Some(&self.maps_db))?;
                let sec_idx = json_i64(req.get("section_index"))
                    .filter(|&v| v >= 0)
                    .map(|v| v as usize);
                let mut marked: Vec<String> = req
                    .get("marked_ids")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();

                {
                    let mut rs = self.route_state.lock().unwrap_or_else(|e| e.into_inner());
                    rs.legs.clear();
                    rs.last_route = Some(payload.clone());
                }
                let st = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    marked.extend(nav.collected_ids.iter().cloned());
                    nav.load_route_document(doc, sec_idx, &marked);
                    nav.route_status_json()
                };
                self.push_route_state_update(&st);
                self.notify_route_loaded_in_game(&st);
                Ok(st)
            }
            "section" => {
                let idx = json_i64(req.get("section_index")).unwrap_or(0).max(0) as usize;
                let (ok_sel, st) = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    let ok_sel = nav.select_route_section(idx);
                    (ok_sel, nav.route_status_json())
                };
                if !ok_sel {
                    return Err(CoreError::InvalidArgument(format!(
                        "Invalid section_index: {idx}"
                    )));
                }
                self.push_route_state_update(&st);
                Ok(st)
            }
            "reset" => {
                let (was_reset, st) = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    let ok = nav.reset_route_progress();
                    (ok, nav.route_status_json())
                };
                if !was_reset {
                    return Err(CoreError::InvalidArgument("No active route".to_owned()));
                }
                self.push_route_state_update(&st);
                self.nav_flash("hud.flash.route_reset", HudPalette::Info, 2.8, &[]);
                Ok(st)
            }
            "clear" => {
                self.mmap_overlay.hide();
                {
                    let mut rs = self.route_state.lock().unwrap_or_else(|e| e.into_inner());
                    rs.legs.clear();
                    rs.last_route = None;
                }
                let st = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    nav.clear_route_document();
                    nav.route_status_json()
                };
                self.push_route_state_update(&st);
                Ok(st)
            }
            "export" => {
                let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                let Some((doc, _)) = nav.active_route_doc() else {
                    return Err(CoreError::InvalidArgument("No active route".to_owned()));
                };
                Ok(json!({
                    "filename": doc.suggested_filename(),
                    "doc": doc.to_value()?,
                }))
            }
            "save" => {
                let doc = {
                    let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    nav.active_route_doc().map(|(d, _)| d.clone())
                }
                .ok_or_else(|| CoreError::InvalidArgument("No active route".to_owned()))?;
                let saved_path = save_route_to_library(&self.paths.routes_dir(), &doc)?;
                Ok(json!({
                    "saved": true,
                    "filename": doc.suggested_filename(),
                    "path": saved_path.to_string_lossy(),
                }))
            }
            "list_saved" => {
                let routes = list_saved_routes(&self.paths.routes_dir())?;
                Ok(json!({ "routes": routes }))
            }
            "load_saved" => {
                let fname = json_str_trimmed(req.get("filename"));
                if fname.is_empty() {
                    return Err(CoreError::InvalidArgument("Missing filename".to_owned()));
                }
                let doc = load_saved_route(&self.paths.routes_dir(), &fname, Some(&self.maps_db))?;
                let st = {
                    let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
                    let marked: Vec<String> = nav.collected_ids.iter().cloned().collect();
                    nav.load_route_document(doc, None, &marked);
                    nav.route_status_json()
                };
                self.push_route_state_update(&st);
                self.notify_route_loaded_in_game(&st);
                Ok(st)
            }
            "delete_saved" => {
                let fname = json_str_trimmed(req.get("filename"));
                if fname.is_empty() {
                    return Err(CoreError::InvalidArgument("Missing filename".to_owned()));
                }
                let deleted = delete_saved_route(&self.paths.routes_dir(), &fname)?;
                Ok(json!({ "deleted": deleted, "filename": fname }))
            }
            "parse_share_url" => {
                let url = json_str_trimmed(req.get("url"));
                if let Some((map_id, share_id)) = parse_hoyolab_route_share_url(&url) {
                    Ok(json!({
                        "map_id": map_id,
                        "share_id": share_id,
                        "url": url,
                    }))
                } else {
                    Err(CoreError::InvalidArgument(
                        "Invalid HoYoLAB interactive-map share URL".to_owned(),
                    ))
                }
            }
            other => Err(CoreError::InvalidArgument(format!(
                "Unsupported route action: {other}"
            ))),
        }
    }

    /// Route-guidance (or locked single-pin) scene for the registered M-Map `view`.
    pub fn build_mmap_guide_scene(
        &self,
        game_rect: RectI32,
        view: ViewTransform,
        mode: MMapOverlayMode,
        lang: &str,
    ) -> Option<MMapGuideScene> {
        if mode == MMapOverlayMode::Off {
            return None;
        }
        let (doc, active, done) = {
            let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pin) = nav.locked_target_pin().cloned() {
                let mname = self.localized_map_name(pin.map_id, "", lang);
                let sdoc = build_single_pin_route_doc(&self.maps_db, &pin, &mname);
                let sdone = vec![vec![false; sdoc.sections.first().map_or(0, |s| s.stops.len())]];
                (sdoc, 0usize, sdone)
            } else {
                let (doc, active) = nav.active_route_doc()?;
                let done: Vec<Vec<bool>> = doc
                    .sections
                    .iter()
                    .map(|sec| sec.stops.iter().map(|st| nav.is_route_stop_done(&st.id)).collect())
                    .collect();
                (doc.clone(), active, done)
            }
        };
        let route_map_name = self.localized_map_name(doc.map_id, &doc.map_name, lang);
        let view_map_name = self.localized_map_name(view.map_id, "", lang);
        compute_guide_scene(&GuideInput {
            doc: &doc,
            active,
            done: &done,
            view,
            game_rect,
            mode,
            lang,
            route_map_name: &route_map_name,
            view_map_name: &view_map_name,
        })
    }

    /// Text-only M-Map banner (route/locked pin, section, area / map to go to) for when no view is
    /// registered or the map drawer is open.
    pub fn build_mmap_fallback_scene(
        &self,
        game_rect: RectI32,
        mode: MMapOverlayMode,
        lang: &str,
        drawer_open: bool,
    ) -> Option<MMapGuideScene> {
        if mode == MMapOverlayMode::Off {
            return None;
        }
        let (doc, active) = {
            let nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pin) = nav.locked_target_pin().cloned() {
                let mname = self.localized_map_name(pin.map_id, "", lang);
                (build_single_pin_route_doc(&self.maps_db, &pin, &mname), 0usize)
            } else {
                let (doc, active) = nav.active_route_doc()?;
                (doc.clone(), active)
            }
        };
        let route_map_name = self.localized_map_name(doc.map_id, &doc.map_name, lang);
        let region = section_area_name(&self.maps_db, &doc, active, lang);
        compute_fallback_scene(&FallbackInput {
            doc: &doc,
            active,
            game_rect,
            mode,
            lang,
            route_map_name: &route_map_name,
            region_name: region.as_deref(),
            drawer_open,
        })
    }

    fn localized_map_name(&self, map_id: i32, raw: &str, lang: &str) -> String {
        let raw = if raw.trim().is_empty() {
            self.maps_db.map_name(map_id).unwrap_or("")
        } else {
            raw
        };
        HoYoLABLabelCatalog::get_map_name(map_id, Some(lang), raw)
    }

    pub fn apply_map_m_anchor(&self, res: &LocateResult) {
        if !res.has_world_coords {
            return;
        }
        let Some(world_x) = res.world_x else {
            return;
        };
        let Some(world_y) = res.world_y else {
            return;
        };
        let map_id = res.map_id.unwrap_or(2);
        let is_ug = res.is_underground;
        let floor_id = res.floor_id;
        let floor_name = if !res.floor_short.is_empty() {
            res.floor_short.clone()
        } else if let Some(ref hf) = res.hoyolab_floor_name
            && !hf.is_empty()
        {
            hf.clone()
        } else if !res.floor_name.is_empty() {
            res.floor_name.clone()
        } else if is_ug {
            "B1".to_owned()
        } else {
            "G".to_owned()
        };

        let pos_info = json!({
            "map_id": map_id,
            "area_id": res.area_id,
            "area_name": res.area_name,
            "lat": world_y,
            "lng": world_x,
            "world_x": world_x,
            "world_y": world_y,
            "heading": res.heading,
            "camera_heading": res.heading,
            "view_heading": res.heading,
            "is_underground": is_ug,
            "group_id": res.group_id,
            "floor_id": floor_id,
            "floor_name": floor_name,
        });
        {
            let mut gs = self.game_state.lock().unwrap_or_else(|e| e.into_inner());
            gs.last_pos = Some(pos_info);
        }

        let now = self.start_instant.elapsed().as_secs_f64();
        {
            let mut guard = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((ref mut tracker, ref mut last_ts)) = *guard {
                let ts = if now < *last_ts { *last_ts } else { now };
                *last_ts = ts;
                let _ = tracker.set_map_m_anchor(
                    map_id,
                    world_x,
                    world_y,
                    is_ug,
                    floor_id,
                    &floor_name,
                    Some(ts),
                );
            }
        }

        self.anchor_seq.fetch_add(1, Ordering::AcqRel);
        if self.radar_active.load(Ordering::Acquire) {
            self.prefetch_radar_tiles_around(map_id, world_x, world_y);
        }
        self.push_lan_snapshot(true);
    }

    pub fn refresh_nav_hud_labels(&self) {
        let mark = self.hotkey_mgr.get_display_name("nav_mark");
        let guide = self.hotkey_mgr.get_display_name("nav_guide");
        let layout = self.hotkey_mgr.get_display_name("nav_layout");
        self.nav_hud
            .set_hotkey_labels(Some(&mark), Some(&guide), Some(&layout));
    }

    pub fn nav_flash(&self, key: &str, kind: HudPalette, duration_s: f64, args: &[(&str, &str)]) {
        self.nav_hud.flash(&t(key, args), kind, duration_s);
    }

    pub fn apply_guide(self: &Arc<Self>, guide: &PinGuide) {
        let cur_id = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pin) = nav.get_pin_mut(&guide.pin_id) {
                if !guide.description.is_empty() {
                    pin.description = guide.description.clone();
                }
                if !guide.image_url.is_empty() && pin.image_url.is_empty() {
                    pin.image_url = guide.image_url.clone();
                }
            }
            nav.current_target().map(|p| p.id.clone())
        };
        let img_decoded = if let Some(ref bytes) = guide.image_bytes
            && !guide.image_url.is_empty()
        {
            self.nav_hud.put_guide_image(&guide.image_url, bytes)
        } else {
            false
        };
        if cur_id.as_deref() == Some(guide.pin_id.as_str()) {
            let status = if guide.ok && (!guide.description.is_empty() || img_decoded) {
                "ready"
            } else if !guide.ok || (!guide.image_url.is_empty() && !img_decoded) {
                "error"
            } else {
                "empty"
            };
            self.nav_hud.set_guide_status(status);
        }
    }

    pub fn ensure_guide(self: &Arc<Self>, pin: Option<&NavPin>) {
        let Some(pin) = pin else {
            self.nav_hud.set_guide_status("");
            return;
        };
        if let Some(cached) = self.guide_fetcher.get_cached(&pin.id, None) {
            self.apply_guide(&cached);
            return;
        }
        let state_weak = Arc::downgrade(self);
        let queued = self.guide_fetcher.request(
            &pin.id,
            pin.map_id,
            move |guide| {
                if let Some(st) = state_weak.upgrade() {
                    st.apply_guide(&guide);
                }
            },
            None,
        ) || (!pin.image_url.is_empty()
            && self.guide_fetcher.request_inline_guide(
                &pin.id,
                &pin.description,
                &pin.image_url,
                {
                    let state_weak = Arc::downgrade(self);
                    move |guide| {
                        if let Some(st) = state_weak.upgrade() {
                            st.apply_guide(&guide);
                        }
                    }
                },
                None,
            ));
        if queued {
            self.nav_hud.set_guide_status("loading");
        } else if !self.guide_fetcher.is_loading(&pin.id, None) {
            self.nav_hud.set_guide_status(if pin.has_spot_content() {
                ""
            } else {
                "empty"
            });
        }
    }

    pub fn broadcast_nav_mark(&self, pin: &NavPin, action: &str) -> bool {
        if !self.server.has_active_client(10.0) {
            return false;
        }
        self.server.broadcast(
            json!({
                "type": "NAV_MARK_PIN",
                "action": action,
                "pin_id": pin.id,
                "lat": pin.lat,
                "lng": pin.lng,
                "map_id": pin.map_id,
                "floor_id": pin.floor_id,
                "is_underground": pin.is_underground,
                "name": pin.display_name(),
            }),
            false,
            false,
        );
        true
    }

    pub fn broadcast_target_lock_state(&self) {
        let locked_val = self
            .nav_controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .locked_target_pin()
            .map(NavPin::to_wire_json);
        self.server.broadcast(
            json!({
                "type": "TARGET_LOCK_UPDATED",
                "locked_target": locked_val,
            }),
            false,
            false,
        );
    }

    pub fn set_engine_active(&self, active: bool) -> bool {
        self.engine_active.store(active, Ordering::Release);
        self.server.set_engine_active(active);

        self.hotkey_mgr.set_enabled("sync", active);
        self.hotkey_mgr.set_enabled("debug", active);
        self.hotkey_mgr.set_enabled("switch", active);
        for act in [
            "nav_mark",
            "nav_undo",
            "nav_guide",
            "nav_next",
            "nav_layout",
            "nav_toggle",
            "nav_unlock",
            "radar_toggle",
        ] {
            self.hotkey_mgr.set_enabled(act, active);
        }

        if !active {
            self.nav_hud.set_edit_mode(false);
            self.nav_hud.set_guide_open(false, None);
            self.set_radar_edit_mode(false);
            self.hide_radar();
            self.state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            self.pause_minimap_tracker("standby");
            self.nav_hud.hide();
        }
        self.refresh_tray_state();
        active
    }

    pub fn set_hotkeys(&self, payload: &Value) -> CoreResult<Value> {
        let req_map = payload.as_object().cloned().unwrap_or_default();
        let has_batch = HOTKEY_ACTIONS
            .iter()
            .any(|act| req_map.contains_key(&format!("hotkey_{act}")));

        let mut results = Map::new();
        let mut all_ok = true;
        let mut nav_changed = false;
        let mut applied: Vec<(String, String)> = Vec::new();

        // Keep `config_io_lock` outermost and NEVER hold `self.config.write()` across
        // `hotkey_mgr.update_binding` -> `join_hook_thread`, because `gh-kb-hook` invokes
        // `should_suppress` -> `config.read()`, which would AB-BA deadlock.
        let _io_guard = self
            .config_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        {
            let mut apply_one = |act: &str, raw_key: &str| {
                let norm = raw_key.trim().to_ascii_lowercase();
                if norm.is_empty() || !HOTKEY_ACTIONS.contains(&act) {
                    all_ok = false;
                    results.insert(
                        act.to_owned(),
                        json!({"ok": false, "hotkey": norm, "display_name": ""}),
                    );
                    return;
                }
                let ok = self.hotkey_mgr.update_binding(act, &norm);
                let dname = if ok {
                    applied.push((act.to_owned(), norm.clone()));
                    if act.starts_with("nav_") {
                        nav_changed = true;
                    }
                    self.hotkey_mgr.get_display_name(act)
                } else {
                    all_ok = false;
                    format_hotkey_display_name(&norm)
                };
                results.insert(
                    act.to_owned(),
                    json!({"ok": ok, "hotkey": norm, "display_name": dname}),
                );
            };

            if has_batch {
                for act in HOTKEY_ACTIONS {
                    let kname = format!("hotkey_{act}");
                    let val_str = json_str_trimmed(req_map.get(&kname));
                    if !val_str.is_empty() {
                        apply_one(act, &val_str);
                    }
                }
            } else if req_map.contains_key("action") && req_map.contains_key("hotkey") {
                let act = json_str_trimmed(req_map.get("action")).to_ascii_lowercase();
                let new_k = json_str_trimmed(req_map.get("hotkey"));
                apply_one(&act, &new_k);
            } else if req_map.contains_key("hotkey") {
                let new_k = json_str_trimmed(req_map.get("hotkey"));
                apply_one("sync", &new_k);
            }
        }

        if nav_changed {
            self.refresh_nav_hud_labels();
        }

        if !applied.is_empty() {
            let cfg_clone = {
                let mut cfg = self.config.write().expect("config lock poisoned");
                for (act, norm) in &applied {
                    set_hotkey_field(&mut cfg, act, norm);
                }
                cfg.clone()
            };
            if let Err(err) = self.save_config_locked(&cfg_clone) {
                tracing::warn!(
                    "Failed to save config after set_hotkeys ({}): {err}",
                    self.config_path.display()
                );
            }
        }

        Ok(json!({
            "ok": all_ok,
            "results": results,
        }))
    }

    pub fn set_language(self: &Arc<Self>, lang: &str) -> CoreResult<Value> {
        let lower = lang.trim().to_ascii_lowercase();
        let raw = if matches!(lower.as_str(), "vi" | "en" | "auto") {
            lower
        } else {
            "auto".to_owned()
        };

        let (_cfg_clone, (resolved, offline_gate), _) = self.mutate_and_save_config(|cfg| {
            let resolved = set_language(Some(&raw), Some(&Win32LocaleProvider::new()));
            cfg.language = raw.clone();
            let offline = !cfg.hoyolab_sync.auto_check_on_startup
                || cfg.hoyolab_sync.auto_update_mode.eq_ignore_ascii_case("off");
            (resolved, offline)
        });

        let cur_map_id = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pos
            .as_ref()
            .and_then(|v| json_i64(v.get("map_id")))
            .map(|i| i as i32)
            .unwrap_or(2);

        if !offline_gate {
            self.label_catalog.prefetch(cur_map_id, Some(&resolved));
        }
        let localized_map = HoYoLABLabelCatalog::get_map_name(cur_map_id, Some(&resolved), "");
        self.server
            .update_last_sync_map_name(cur_map_id, &localized_map);
        self.nav_hud.set_language(Some(&resolved));

        let cur_target = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            let cur_id = nav.current_target().map(|p| p.id.clone());
            if let Some(ref pid) = cur_id
                && let Some(pin) = nav.get_pin_mut(pid)
            {
                pin.description.clear();
            }
            nav.current_target().cloned()
        };
        if let Some(ref tgt) = cur_target
            && !offline_gate
        {
            self.ensure_guide(Some(tgt));
        }

        if let Some(tray) = self
            .tray
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            tray.set_language(Some(&resolved));
        }
        self.refresh_tray_state();

        Ok(json!({
            "language": raw,
            "resolved": resolved,
        }))
    }

    pub fn set_nav_hud(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let (cfg_clone, edit_mode, _) = self.mutate_and_save_config(|cfg| {
            if let Some(v) = req.get("enabled").or_else(|| req.get("nav_hud_enabled"))
                && !v.is_null()
            {
                cfg.nav_hud_enabled = json_truthy(Some(v), true);
            }

            if let Some(cp_val) = req.get("custom_pos") {
                let parsed = if let Some(arr) = cp_val.as_array()
                    && arr.len() == 2
                    && let (Some(fx), Some(fy)) = (json_f64(Some(&arr[0])), json_f64(Some(&arr[1])))
                    && fx.is_finite()
                    && fy.is_finite()
                {
                    NavMiniHud::norm_custom_pos(Some([fx, fy]))
                } else {
                    None
                };
                cfg.nav_hud_custom_pos = parsed;
                self.nav_hud.set_custom_position(parsed);
            }

            if let Some(pos_raw) = req
                .get("position")
                .or_else(|| req.get("nav_hud_position"))
                .and_then(Value::as_str)
            {
                let norm = pos_raw.trim().to_ascii_lowercase();
                if !norm.is_empty() {
                    if norm == "custom" && cfg.nav_hud_custom_pos.is_none() {
                        if cfg.nav_hud_position == "custom" {
                            cfg.nav_hud_position = "under_minimap".to_owned();
                        }
                        self.nav_hud
                            .set_position_mode(&cfg.nav_hud_position, cfg.nav_hud_custom_pos);
                        self.nav_hud.set_edit_mode(true);
                    } else {
                        let parsed = NavMiniHud::norm_position_mode(Some(&norm));
                        cfg.nav_hud_position = parsed.as_str().to_owned();
                        self.nav_hud
                            .set_position_mode(&cfg.nav_hud_position, cfg.nav_hud_custom_pos);
                    }
                }
            }

            if let Some(edit_req) = req.get("edit_mode").filter(|v| !v.is_null()) {
                if edit_req
                    .as_str()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("toggle"))
                {
                    let editing = self.nav_hud.toggle_edit_mode();
                    self.set_radar_edit_mode(editing);
                } else {
                    let want = json_truthy(Some(edit_req), false);
                    self.nav_hud.set_edit_mode(want);
                    self.set_radar_edit_mode(want);
                }
            }

            if let Some(cap_req) = req
                .get("capture_visible")
                .or_else(|| req.get("nav_hud_capture_visible"))
                .filter(|v| !v.is_null())
            {
                let vis = json_truthy(Some(cap_req), false);
                cfg.nav_hud_capture_visible = vis;
                self.nav_hud.set_capture_visible(vis);
                if self.radar_active.load(Ordering::Acquire)
                    && let Ok(guard) = self.radar_map.lock()
                    && let Some(ref ctrl) = *guard
                {
                    ctrl.set_capture_visible(vis);
                }
                if !vis {
                    self.mmap_overlay.set_capture_visible(false);
                    self.capturer.release_window_capture();
                }
            }

            if let Some(zoom_req) = req
                .get("guide_zoom_mode")
                .or_else(|| req.get("nav_hud_guide_zoom_mode"))
                .and_then(Value::as_str)
            {
                let norm = NavMiniHud::norm_guide_zoom_mode(Some(zoom_req));
                cfg.nav_hud_guide_zoom_mode = norm.as_str().to_owned();
                self.nav_hud
                    .set_guide_zoom_mode(&cfg.nav_hud_guide_zoom_mode);
            }
            if let Some(ov_req) = req.get("mmap_overlay_mode").and_then(Value::as_str) {
                let norm = ov_req.trim().to_ascii_lowercase();
                if VALID_MMAP_OVERLAY_MODES.contains(&norm.as_str()) {
                    cfg.mmap_overlay_mode = norm;
                    if cfg.mmap_overlay_mode == "off" {
                        self.mmap_overlay.hide();
                    }
                }
            }

            let edit_mode = self.nav_hud.is_edit_mode();
            if !cfg.nav_hud_enabled && !edit_mode {
                self.nav_hud.hide();
            }
            edit_mode
        });

        Ok(json!({
            "enabled": cfg_clone.nav_hud_enabled,
            "nav_hud_enabled": cfg_clone.nav_hud_enabled,
            "position": cfg_clone.nav_hud_position,
            "nav_hud_position": cfg_clone.nav_hud_position,
            "custom_pos": cfg_clone.nav_hud_custom_pos,
            "edit_mode": edit_mode,
            "capture_visible": cfg_clone.nav_hud_capture_visible,
            "nav_hud_capture_visible": cfg_clone.nav_hud_capture_visible,
            "guide_zoom_mode": cfg_clone.nav_hud_guide_zoom_mode,
            "nav_hud_guide_zoom_mode": cfg_clone.nav_hud_guide_zoom_mode,
            "mmap_overlay_mode": cfg_clone.mmap_overlay_mode,
        }))
    }

    /// Who owns sync/mark feedback for the event happening now: `true` = in-game HUD toast +
    /// chime, `false` = the HoYoLAB page (its own toast/beep/animation). Replaces the old manual
    /// "Single-Monitor Mode" toggle: the in-game HUD speaks up whenever the user cannot see a map
    /// tab (no fresh `visible` report), and always on a single monitor where the browser sits
    /// behind the game even if Chromium's occlusion tracking misreports it.
    pub fn native_feedback_wanted(&self) -> bool {
        WindowFinder::get_monitor_count() <= 1 || !self.server.any_map_tab_visible()
    }

    pub fn handle_hotkey_action(&self) {
        let native_fb = self.native_feedback_wanted();

        let (mut game_hwnd, mut rect) = {
            let gs = self.game_state.lock().unwrap_or_else(|e| e.into_inner());
            (gs.hwnd, gs.rect)
        };
        if rect.is_none_or(|r| r.width < 1000 || r.height < 600)
            && let Some((hwnd_found, l, t, w, h)) =
                WindowFinder::get_game_window(&WINDOW_TITLES, true)
        {
            let r = RectI32 {
                x: l,
                y: t,
                width: w,
                height: h,
            };
            rect = Some(r);
            if hwnd_found != 0 {
                game_hwnd = Some(hwnd_found);
            }
            let mut gs = self.game_state.lock().unwrap_or_else(|e| e.into_inner());
            gs.rect = Some(r);
            if hwnd_found != 0 {
                gs.hwnd = Some(hwnd_found);
            }
        }

        if native_fb && !self.server.has_active_client(10.0) {
            self.hud_toast.show_toast(
                &t("toast.map_not_ready.title", &[]),
                &t("toast.map_not_ready.detail", &[]),
                ToastKind::Error,
                2800,
                rect,
            );
            self.audio.play("error", false);
            return;
        }

        let Some(r) = rect else {
            if native_fb {
                self.hud_toast.show_toast(
                    &t("toast.not_detected.title", &[]),
                    &t("toast.not_detected.detail", &[]),
                    ToastKind::Warning,
                    2500,
                    None,
                );
                self.audio.play("warning", false);
            }
            self.server.broadcast(
                json!({"type": "SYNC_AND_MARK", "detected": false, "native_feedback": native_fb}),
                native_fb,
                false,
            );
            return;
        };

        let full_frame_opt = if self.mmap_overlay.capture_visible()
            && let Some(hwnd) = game_hwnd
            && hwnd != 0
        {
            self.capturer
                .capture_window_client(hwnd, r.x, r.y, r.width, r.height)
                .or_else(|| self.capturer.capture_region(r.x, r.y, r.width, r.height))
        } else {
            self.capturer.capture_region(r.x, r.y, r.width, r.height)
        };
        let Some(full_frame) = full_frame_opt else {
            if native_fb {
                self.hud_toast.show_toast(
                    &t("toast.not_detected.title", &[]),
                    &t("borderless.tip", &[]),
                    ToastKind::Warning,
                    2500,
                    Some(r),
                );
                self.audio.play("warning", false);
            }
            self.server.broadcast(
                json!({"type": "SYNC_AND_MARK", "detected": false, "native_feedback": native_fb}),
                native_fb,
                false,
            );
            return;
        };

        let res_opt = self
            .locator
            .as_ref()
            .and_then(|loc| {
                let res = loc.locate_player_frame(full_frame.as_view(), false, None);
                gh_cv::locator::release_thread_scratch();
                res
            });

        if let Ok(bgr) = gh_cv::ops::color::frame_view_to_bgr(full_frame.as_view()) {
            self.state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_synced(Some(bgr.view()));
        }

        if let Some(res) = res_opt
            && res.detected
        {
            let lang = get_language();
            let mid = res.map_id.unwrap_or(2);
            let fallback = res.map_name.as_deref().unwrap_or("");
            let loc_map_name = HoYoLABLabelCatalog::get_map_name(mid, Some(&lang), fallback);
            let mut payload = match res.to_json_value() {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            payload.insert("type".to_owned(), Value::String("SYNC_AND_MARK".to_owned()));
            payload.insert("map_name".to_owned(), Value::String(loc_map_name));
            payload.insert("native_feedback".to_owned(), Value::Bool(native_fb));
            self.server
                .broadcast(Value::Object(payload), false, false);
            if res.has_world_coords {
                self.apply_map_m_anchor(&res);
            }
        } else {
            if native_fb {
                self.hud_toast.show_toast(
                    &t("toast.not_detected.title", &[]),
                    &t("toast.not_detected.detail", &[]),
                    ToastKind::Warning,
                    2500,
                    Some(r),
                );
                self.audio.play("warning", false);
            }
            self.server.broadcast(
                json!({"type": "SYNC_AND_MARK", "detected": false, "native_feedback": native_fb}),
                native_fb,
                false,
            );
        }
    }

    pub fn handle_debug_action(&self, trigger_source: &str, user_note: &str, broadcast: bool) -> Value {
        let create_zip = trigger_source != "hotkey_debug";
        let bundle = self
            .diagnostics
            .capture_and_create_bundle_with_zip(
                None,
                None,
                trigger_source,
                user_note,
                true,
                None,
                "",
                None,
                None,
                create_zip,
            )
            .unwrap_or_else(|e| json!({"success": false, "error": e.to_string()}));
        if broadcast {
            self.server.broadcast(
                json!({
                    "type": "DIAGNOSTIC_CAPTURED",
                    "folder_name": bundle.get("folder_name").and_then(Value::as_str).unwrap_or(""),
                    "zip_name": bundle.get("zip_name").and_then(Value::as_str).unwrap_or(""),
                    "zip_created": bundle.get("zip_created").and_then(Value::as_bool).unwrap_or(false),
                    "zip_size_bytes": bundle.get("zip_size_bytes").and_then(Value::as_u64).unwrap_or(0),
                    "bundle_dir": bundle.get("bundle_dir").and_then(Value::as_str).unwrap_or(""),
                    "has_world_coords": bundle.get("has_world_coords").and_then(Value::as_bool).unwrap_or(false),
                    "reason": bundle.get("reason").and_then(Value::as_str).unwrap_or(""),
                    "summary": bundle.get("summary").cloned().unwrap_or(Value::Null),
                }),
                false,
                false,
            );
        }
        bundle
    }

    pub fn build_diag_env_health(&self) -> Value {
        let cfg_opt = self.config.try_read().ok().map(|c| c.clone());
        let (game_hwnd, uipi_val) = self
            .game_state
            .try_lock()
            .ok()
            .map(|gs| {
                (
                    gs.hwnd,
                    gs.uipi_status
                        .as_ref()
                        .and_then(|u| serde_json::to_value(u).ok())
                        .unwrap_or(Value::Null),
                )
            })
            .unwrap_or((None, Value::Null));

        let (paimon_bootstrapped, global_relocator_ready, tracker_json) = self
            .tracker
            .try_lock()
            .ok()
            .and_then(|g| {
                g.as_ref().map(|(tr, _)| {
                    let (dx, dy) = tr.calibrated_offset_px();
                    let paimon = tr.has_paimon_tpl();
                    let reloc = tr.global_relocator_ready();
                    let tj = json!({
                        "state": tr.state().as_str(),
                        "mode": tr.minimap_mode().as_str(),
                        "zoom_regime": tr.zoom_regime().as_str(),
                        "calibrated_offset_px": [dx, dy],
                        "matcher_calls": tr.matcher_calls(),
                        "stationary_skips": tr.stationary_skips(),
                        "last_match_ms": (tr.last_match_ms() * 100.0).round() / 100.0,
                    });
                    (paimon, reloc, tj)
                })
            })
            .unwrap_or((false, false, json!({"state": "unavailable"})));

        let (terrain_count, cave_count, ui_anchors_count, ocr_available) = self
            .locator
            .as_ref()
            .map(|l| {
                (
                    l.terrain_map_count(),
                    l.cave_masks().len(),
                    l.ui_anchors().len(),
                    l.has_ocr(),
                )
            })
            .unwrap_or((0, 0, 0, false));

        let conflicts_summary = if let Some(ref cfg) = cfg_opt {
            let mut bindings = BTreeMap::new();
            for act in HOTKEY_ACTIONS {
                bindings.insert(act.to_owned(), get_hotkey_field(cfg, act));
            }
            let req = json!({
                "bindings": bindings,
                "lang": get_language(),
                "suppress_enabled": cfg.hotkey_suppress_conflicts,
            });
            let analysis = self
                .conflict_advisor
                .check_conflicts(&req)
                .unwrap_or(Value::Null);
            let mut internal_count = 0usize;
            let mut genshin_count = 0usize;
            let mut external_count = 0usize;
            if let Some(actions) = analysis.get("actions").and_then(Value::as_object) {
                for act_val in actions.values() {
                    if let Some(list) = act_val.get("conflicts").and_then(Value::as_array) {
                        for c in list {
                            match c.get("kind").and_then(Value::as_str).unwrap_or("") {
                                "internal" => internal_count += 1,
                                "genshin" => genshin_count += 1,
                                _ => external_count += 1,
                            }
                        }
                    }
                }
            }
            json!({
                "internal_count": internal_count,
                "genshin_count": genshin_count,
                "external_count": external_count,
            })
        } else {
            json!({})
        };

        json!({
            "uptime_sec": self.start_instant.elapsed().as_secs(),
            "elevated_self": gh_win32::game_settings::is_process_elevated(),
            "engine_active": self.engine_active.load(Ordering::Acquire),
            "game_hwnd": game_hwnd,
            "uipi_status": uipi_val,
            "datasets": {
                "multimap_maps_count": self.maps_db.maps.len(),
                "terrain_maps_count": terrain_count,
                "cave_masks_count": cave_count,
                "ui_anchors_count": ui_anchors_count,
                "paimon_bootstrapped": paimon_bootstrapped,
                "global_relocator_ready": global_relocator_ready,
                "ocr_available": ocr_available,
                "hd_tile_downloads": self.hd_provider.downloads(),
            },
            "minimap_tracker": tracker_json,
            "wgc_window_capture": self.capturer.window_capture_status(),
            "mmap_overlay_capture_visible": self.mmap_overlay.capture_visible(),
            "hotkey_conflicts_summary": conflicts_summary,
        })
    }

    pub fn handle_switch_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let server_cb = self.server.clone();
        let cb = move || {
            server_cb.broadcast(json!({"type": "FOCUS_MAP_TAB"}), false, false);
        };
        let target = WindowSwitcher::toggle_game_and_browser(&WINDOW_TITLES, Some(&cb));
        if target == "no_browser" {
            let rect = self
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.hud_toast.show_toast(
                &t("toast.map_not_ready.title", &[]),
                &t("toast.map_not_ready.detail", &[]),
                ToastKind::Warning,
                2400,
                rect,
            );
            self.audio.play("warning", false);
        }
    }

    pub fn handle_nav_mark_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let (target, dist, ready) = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            let tgt = nav.current_target().cloned();
            let d = nav.last_guidance().and_then(|g| {
                if g.target.as_ref().map(|t| &t.id) == tgt.as_ref().map(|t| &t.id) {
                    g.target_distance_m
                } else {
                    None
                }
            });
            (tgt, d, nav.selection_ready())
        };
        let Some(target) = target else {
            self.nav_flash("hud.flash.no_target", HudPalette::Warning, 2.6, &[]);
            self.audio.play("warning", false);
            return;
        };
        if !ready {
            self.nav_flash("hud.flash.no_target", HudPalette::Warning, 2.6, &[]);
            self.audio.play("warning", false);
            return;
        }

        let now = Instant::now();
        let confirmed = {
            let conf = self
                .nav_mark_confirm
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            conf.pin_id.as_deref() == Some(target.id.as_str())
                && conf.until.is_some_and(|u| now < u)
        };
        if let Some(d) = dist
            && d > NAV_MARK_CONFIRM_DIST_M
            && !confirmed
        {
            let mut conf = self
                .nav_mark_confirm
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            conf.pin_id = Some(target.id.clone());
            conf.until = Some(now + std::time::Duration::from_secs(3));
            let meters = (d.round() as i64).to_string();
            let hk = self.hotkey_mgr.get_display_name("nav_mark");
            self.nav_flash(
                "hud.flash.far_confirm",
                HudPalette::Warning,
                3.0,
                &[("meters", &meters), ("hotkey", &hk)],
            );
            return;
        }

        {
            let mut conf = self
                .nav_mark_confirm
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            conf.pin_id = None;
            conf.until = None;
        }

        let (marked, lock_changed) = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            let had_lock = nav.locked_target_pin().is_some();
            nav.mark_collected(Some(&target.id));
            let has_lock = nav.locked_target_pin().is_some();
            (nav.collected_ids.contains(&target.id), had_lock != has_lock)
        };
        if !marked {
            self.nav_flash("hud.flash.no_target", HudPalette::Warning, 2.6, &[]);
            return;
        }
        if lock_changed {
            self.broadcast_target_lock_state();
        }

        let name = target.display_name().to_owned();
        if self.broadcast_nav_mark(&target, "mark") {
            let undo_hk = self.hotkey_mgr.get_display_name("nav_undo");
            self.nav_flash(
                "hud.flash.marked",
                HudPalette::Success,
                3.2,
                &[("name", &name), ("undo", &undo_hk)],
            );
        } else {
            self.nav_flash(
                "hud.flash.marked_local",
                HudPalette::Success,
                3.2,
                &[("name", &name)],
            );
        }
        self.audio.play("success", false);
        self.push_lan_snapshot(true);
        self.maybe_notify_route_section_completion();
    }

    pub fn handle_nav_undo_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let (last_action, pin_id, pin_opt) = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            match nav.peek_history() {
                None => (None, String::new(), None),
                Some((act, pid)) => {
                    let act_owned = act.to_owned();
                    let pid_owned = pid.to_owned();
                    let pin = nav.get_pin(&pid_owned).cloned();
                    nav.undo();
                    (Some(act_owned), pid_owned, pin)
                }
            }
        };
        let Some(action) = last_action else {
            self.nav_flash("hud.flash.nothing_to_undo", HudPalette::Info, 2.6, &[]);
            return;
        };
        let mut web_synced = true;
        if action == "collected" {
            web_synced = pin_opt
                .as_ref()
                .is_some_and(|pin| self.broadcast_nav_mark(pin, "unmark"));
        }
        let display = pin_opt
            .as_ref()
            .map(|p| p.display_name().to_owned())
            .unwrap_or(pin_id);
        if action == "collected" && !web_synced {
            self.nav_flash("hud.flash.web_unmark_failed", HudPalette::Warning, 3.5, &[]);
            self.audio.play("warning", false);
        } else {
            self.nav_flash(
                "hud.flash.undone",
                HudPalette::Info,
                2.6,
                &[("name", &display)],
            );
            self.audio.play("info", false);
        }
        self.push_lan_snapshot(true);
    }

    pub fn handle_nav_guide_hotkey(self: &Arc<Self>) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let opened = self.nav_hud.toggle_guide();
        if opened {
            let cur = self
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current_target()
                .cloned();
            if cur.is_none() {
                self.nav_flash("hud.flash.no_target", HudPalette::Warning, 2.6, &[]);
            }
            self.ensure_guide(cur.as_ref());
        }
    }

    pub fn handle_nav_next_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let (nxt_name, lock_changed) = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            let had_lock = nav.locked_target_pin().is_some();
            let nxt = nav.next_target().map(|p| p.display_name().to_owned());
            let has_lock = nav.locked_target_pin().is_some();
            (nxt, had_lock != has_lock)
        };
        if lock_changed {
            self.broadcast_target_lock_state();
            self.push_lan_snapshot(true);
        }
        if let Some(name) = nxt_name {
            self.nav_flash("hud.flash.next", HudPalette::Info, 2.0, &[("name", &name)]);
        } else {
            self.nav_flash("hud.flash.no_target", HudPalette::Warning, 2.6, &[]);
        }
    }

    pub fn handle_nav_unlock_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let cleared = {
            let mut nav = self.nav_controller.lock().unwrap_or_else(|e| e.into_inner());
            nav.clear_target_lock()
        };
        if cleared {
            self.nav_flash("hud.flash.unlocked_target", HudPalette::Info, 2.2, &[]);
            self.audio.play("info", false);
            self.broadcast_target_lock_state();
            self.push_lan_snapshot(true);
        }
    }

    pub fn handle_nav_layout_hotkey(&self) {
        let editing = self.nav_hud.toggle_edit_mode();
        self.set_radar_edit_mode(editing);
        if !editing {
            self.nav_flash("hud.edit.saved", HudPalette::Success, 2.0, &[]);
        }
    }

    pub fn is_radar_active(&self) -> bool {
        self.radar_active.load(Ordering::Acquire)
    }

    pub fn set_radar_edit_mode(&self, enabled: bool) {
        if !self.radar_active.load(Ordering::Acquire) {
            return;
        }
        if let Ok(guard) = self.radar_map.lock()
            && let Some(ref ctrl) = *guard
        {
            ctrl.set_edit_mode(enabled);
        }
    }

    pub fn hide_radar(&self) {
        if !self.radar_active.load(Ordering::Acquire) {
            return;
        }
        if let Ok(guard) = self.radar_map.lock()
            && let Some(ref ctrl) = *guard
        {
            ctrl.hide();
        }
    }

    pub fn shutdown_radar(&self) {
        self.radar_active.store(false, Ordering::Release);
        let ctrl_opt = self
            .radar_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(ctrl) = ctrl_opt {
            ctrl.close();
        }
        self.pin_icon_cache.stop();
        *self.radar_pins.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Enables or completely tears down the Native Floating Live Radar Map.
    /// Turning OFF destroys the Win32 window, UI/decoder threads, icon worker, and all RGBA caches
    /// so zero CPU, threads, HWND, or RAM remain allocated.
    pub fn apply_radar_enabled(self: &Arc<Self>, enabled: bool) {
        if !enabled {
            self.shutdown_radar();
            return;
        }
        let mut slot = self.radar_map.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            self.radar_active.store(true, Ordering::Release);
            return;
        }
        let cfg = self.config.read().expect("config lock poisoned").clone();
        self.pin_icon_cache.reopen();
        let assets: Arc<dyn RadarAssetSource> = Arc::new(AppRadarAssetSource {
            hd_provider: Arc::clone(&self.hd_provider),
            ug_provider: Arc::clone(&self.ug_provider),
            icon_cache: self.pin_icon_cache.clone(),
            maps_db: Arc::clone(&self.maps_db),
        });
        let st_weak = Arc::downgrade(self);
        let on_layout_saved: gh_hud::radar_map::RadarLayoutCallback =
            Arc::new(move |rel_rect, zoom, opacity| {
                if let Some(st) = st_weak.upgrade() {
                    let (cfg_saved, _, _) = st.mutate_and_save_config(|c| {
                        c.radar_rect = rel_rect;
                        c.radar_zoom = zoom;
                        c.radar_opacity = opacity;
                        c.normalize();
                    });
                    st.server.broadcast(
                        json!({
                            "type": "RADAR_UPDATED",
                            "enabled": cfg_saved.radar_enabled,
                            "radar_enabled": cfg_saved.radar_enabled,
                            "radar_rect": cfg_saved.radar_rect,
                            "radar_zoom": cfg_saved.radar_zoom,
                            "radar_opacity": cfg_saved.radar_opacity,
                        }),
                        false,
                        false,
                    );
                }
            });
        #[cfg(test)]
        let radar_offset = gh_hud::layered::OFFSCREEN_TEST_OFFSET;
        #[cfg(not(test))]
        let radar_offset = (0, 0);
        let ctrl = RadarMapController::new_with_offset(
            assets,
            cfg.radar_rect,
            cfg.radar_zoom,
            cfg.radar_opacity,
            cfg.nav_hud_capture_visible,
            Some(on_layout_saved),
            radar_offset,
        );
        if self.nav_hud.is_edit_mode() {
            ctrl.set_edit_mode(true);
        }
        *slot = Some(ctrl);
        drop(slot);
        self.radar_active.store(true, Ordering::Release);

        // Immediately hydrate pins from any already-cached `/lan_pins` snapshot & warm coarse layer.
        if let Some(raw_json) = self.lan_companion.latest_pins_snapshot_json()
            && let Ok(val) = serde_json::from_str::<Value>(&raw_json)
        {
            self.on_lan_pins_received(&val);
        }
        let cur_mid = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pos
            .as_ref()
            .and_then(|v| json_i64(v.get("map_id")))
            .map(|i| i as i32)
            .unwrap_or(2);
        self.hd_provider.prefetch_coarse_overview_async(cur_mid);
    }

    pub fn set_radar(self: &Arc<Self>, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let (cfg_clone, _, _) = self.mutate_and_save_config(|cfg| {
            if let Some(v) = req.get("enabled").or_else(|| req.get("radar_enabled"))
                && !v.is_null()
            {
                cfg.radar_enabled = json_truthy(Some(v), cfg.radar_enabled);
            }
            if let Some(z) = json_f64(req.get("radar_zoom")) {
                cfg.radar_zoom = z;
            }
            if let Some(op) = json_f64(req.get("radar_opacity")) {
                cfg.radar_opacity = op;
            }
            if let Some(r) = req.get("radar_rect") {
                if r.is_null() {
                    cfg.radar_rect = None;
                } else if let Some(arr) = r.as_array()
                    && arr.len() == 4
                    && let (Some(x), Some(y), Some(w), Some(h)) = (
                        arr[0].as_f64(),
                        arr[1].as_f64(),
                        arr[2].as_f64(),
                        arr[3].as_f64(),
                    )
                {
                    cfg.radar_rect = Some([x, y, w, h]);
                }
            }
            cfg.normalize();
        });

        self.apply_radar_enabled(cfg_clone.radar_enabled);
        if cfg_clone.radar_enabled
            && let Ok(guard) = self.radar_map.lock()
            && let Some(ref ctrl) = *guard
        {
            ctrl.set_layout_params(
                cfg_clone.radar_rect,
                cfg_clone.radar_zoom,
                cfg_clone.radar_opacity,
            );
        }

        Ok(json!({
            "enabled": cfg_clone.radar_enabled,
            "radar_enabled": cfg_clone.radar_enabled,
            "radar_rect": cfg_clone.radar_rect,
            "radar_zoom": cfg_clone.radar_zoom,
            "radar_opacity": cfg_clone.radar_opacity,
        }))
    }

    pub fn handle_radar_toggle_hotkey(self: &Arc<Self>) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let next_enabled = !self
            .config
            .read()
            .expect("config lock poisoned")
            .radar_enabled;
        let Ok(res) = self.set_radar(&json!({"enabled": next_enabled})) else {
            return;
        };
        let final_enabled = res
            .get("radar_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(next_enabled);

        let mut bcast = match res {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        bcast.insert(
            "type".to_owned(),
            Value::String("RADAR_UPDATED".to_owned()),
        );
        self.server.broadcast(Value::Object(bcast), false, false);

        let rect = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rect;
        let toast_key = if final_enabled {
            "toast.radar_shown"
        } else {
            "toast.radar_hidden"
        };
        self.hud_toast.show_toast(
            &t(toast_key, &[]),
            &self.hotkey_mgr.get_display_name("radar_toggle"),
            ToastKind::Info,
            1500,
            rect,
        );
        self.audio.play("info", false);
    }

    pub fn on_lan_pins_received(&self, payload: &Value) {
        if !self.radar_active.load(Ordering::Acquire) {
            return;
        }
        let (map_id, pins) = parse_radar_pins_payload(payload);
        *self.radar_pins.lock().unwrap_or_else(|e| e.into_inner()) = Some((map_id, Arc::new(pins)));
    }

    pub fn prefetch_radar_tiles_around(&self, map_id: i32, world_x: f64, world_y: f64) {
        if !self.radar_active.load(Ordering::Acquire) {
            return;
        }
        let Some(d2) = self
            .maps_db
            .map(map_id)
            .and_then(|m| m.detail_v2.as_ref())
        else {
            return;
        };
        let zoom = self
            .config
            .read()
            .expect("config lock poisoned")
            .radar_zoom;
        let tz = (zoom.ceil() as i32).clamp(d2.min_zoom, d2.max_zoom);
        let tile_world = 256.0 / 2f64.powi(tz);
        let (tx0, tx1, ty0, ty1) = d2.tile_range_for_world_rect(
            world_x - tile_world,
            world_x + tile_world,
            world_y - tile_world,
            world_y + tile_world,
            tz,
        );
        let mut tiles = Vec::with_capacity(9);
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                tiles.push((tz, tx, ty));
            }
        }
        self.hd_provider.prefetch_color_tiles_async(map_id, &tiles);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push_radar_frame(
        &self,
        game_rect: RectI32,
        map_id: i32,
        world_x: f64,
        world_y: f64,
        body_heading: Option<f64>,
        camera_heading: Option<f64>,
        is_underground: bool,
        floor_id: Option<i32>,
        floor_short: String,
        capture_visible: bool,
    ) {
        if !self.radar_active.load(Ordering::Acquire) {
            return;
        }
        let filter_pins = {
            let g = self.radar_pins.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref() {
                Some((mid, list)) if *mid == map_id => Arc::clone(list),
                _ => Arc::new(Vec::new()),
            }
        };
        let overlay = {
            let mut nav = self
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            build_radar_route_overlay(&mut nav, map_id, &filter_pins)
        };
        let pins = overlay.pins;
        let excluded_ids = overlay.excluded_ids;
        let target = overlay.target;
        let route_anchor = overlay.route_anchor;
        let route_legs = match overlay.route_legs {
            Some(legs) => Arc::new(legs),
            None => {
                let rs = self.route_state.lock().unwrap_or_else(|e| e.into_inner());
                let mut legs_out = Vec::with_capacity(rs.legs.len());
                for (idx, leg) in rs.legs.iter().enumerate() {
                    if let (Some(fl), Some(fg), Some(tl), Some(tg)) = (
                        json_f64(leg.get("from_lat")),
                        json_f64(leg.get("from_lng")),
                        json_f64(leg.get("to_lat")),
                        json_f64(leg.get("to_lng")),
                    ) {
                        legs_out.push(RadarRouteLeg {
                            from_lat: fl,
                            from_lng: fg,
                            to_lat: tl,
                            to_lng: tg,
                            done: json_truthy(leg.get("done"), false),
                            step_index: idx,
                            color: DEFAULT_RADAR_ROUTE_COLOR,
                            emphasized: true,
                        });
                    }
                }
                Arc::new(legs_out)
            }
        };
        let layout_hk = self.hotkey_mgr.get_display_name("nav_layout");
        let edit_hint = t("radar.edit.hint", &[("hotkey", &layout_hk)]);

        if let Ok(guard) = self.radar_map.lock()
            && let Some(ref ctrl) = *guard
        {
            ctrl.update(RadarScene {
                game_rect,
                map_id,
                world_x,
                world_y,
                vx: 0.0,
                vy: 0.0,
                body_heading,
                camera_heading,
                is_underground,
                group_id: None,
                floor_id,
                floor_short,
                pins,
                excluded_ids,
                target,
                route_anchor,
                route_legs,
                capture_visible,
                edit_hint,
            });
        }
    }

    pub fn handle_nav_toggle_hotkey(&self) {
        if !self.engine_active.load(Ordering::Acquire) {
            return;
        }
        let next_enabled = !self
            .config
            .read()
            .expect("config lock poisoned")
            .nav_hud_enabled;
        let Ok(res) = self.set_nav_hud(&json!({"enabled": next_enabled})) else {
            return;
        };
        let final_enabled = res
            .get("nav_hud_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(next_enabled);

        let mut bcast = match res {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        bcast.insert(
            "type".to_owned(),
            Value::String("NAV_HUD_UPDATED".to_owned()),
        );
        self.server.broadcast(Value::Object(bcast), false, false);

        if final_enabled {
            let has_guidance = self
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_guidance()
                .is_some();
            if has_guidance && self.nav_hud.hwnd().is_some() {
                self.nav_hud.invalidate_render();
            }
            self.nav_flash("toast.hud_shown", HudPalette::Info, 1.8, &[]);
            self.audio.play("info", false);
        } else {
            let rect = self
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.hud_toast.show_toast(
                &t("toast.hud_hidden", &[]),
                &self.hotkey_mgr.get_display_name("nav_toggle"),
                ToastKind::Info,
                1400,
                rect,
            );
            self.audio.play("info", false);
        }
    }

    pub fn on_hud_position_changed(&self, mode: PositionMode, custom_pos: Option<[f64; 2]>) {
        let (cfg_clone, _, _) = self.mutate_and_save_config(|cfg| {
            cfg.nav_hud_position = mode.as_str().to_owned();
            cfg.nav_hud_custom_pos = custom_pos;
        });
        let enabled = cfg_clone.nav_hud_enabled;
        let pos_str = cfg_clone.nav_hud_position;
        self.server.broadcast(
            json!({
                "type": "NAV_HUD_UPDATED",
                "enabled": enabled,
                "position": pos_str,
                "custom_pos": custom_pos,
                "nav_hud_enabled": enabled,
                "nav_hud_position": pos_str,
            }),
            false,
            false,
        );
    }

    pub fn current_sync_url(&self) -> String {
        format!("http://{}:{}", DEFAULT_SYNC_HOST, self.server.port())
    }
}

#[cfg(test)]
pub fn make_test_state(root: &std::path::Path) -> Arc<AppState> {
    make_test_state_with_cli_ports(root, None, None)
}

#[cfg(test)]
pub fn make_test_state_with_cli_ports(
    root: &std::path::Path,
    cli_port: Option<u16>,
    cli_lan_port: Option<u16>,
) -> Arc<AppState> {
    make_test_state_with_db(root, cli_port, cli_lan_port, None)
}

#[cfg(test)]
pub fn make_test_state_with_db(
    root: &std::path::Path,
    cli_port: Option<u16>,
    cli_lan_port: Option<u16>,
    custom_maps_db: Option<Arc<MultimapDb>>,
) -> Arc<AppState> {
    use gh_core::paths::RuntimeMode;
    use gh_net::sync_server::SyncServerConfig;
    use std::time::Duration;

    let mut cfg = AppConfig::default();
    cfg.hoyolab_sync.auto_check_on_startup = false;
    cfg.hoyolab_sync.auto_update_mode = "off".to_owned();
    let config_path = root.join("config.json");
    let _ = cfg.save_to_file(&config_path);
    let disk_ports = (cfg.ws_port, cfg.lan_companion_port);
    if let Some(p) = cli_port
        && p > 0
    {
        cfg.ws_port = p;
    }
    if let Some(lp) = cli_lan_port
        && lp > 0
    {
        cfg.lan_companion_port = lp;
    }

    let paths = AppPaths {
        mode: RuntimeMode::Installed,
        config_path: config_path.clone(),
        diagnostics_dir: root.join("diagnostics"),
        data_dir: root.join("data"),
        tiles_dir: root.join("tiles"),
        ug_overlays_dir: root.join("ug_overlays"),
        cache_dir: root.join("cache"),
        dev_data_dir: None,
    };

    let config = Arc::new(RwLock::new(cfg.clone()));
    let capturer = Arc::new(ScreenCapturer::new(2));
    let maps_db = custom_maps_db.unwrap_or_else(|| Arc::new(MultimapDb::default()));
    let hd_provider = Arc::new(
        HoyolabHdTileProvider::new(
            Some(paths.tiles_dir.clone()),
            Some(Arc::clone(&maps_db)),
            Duration::from_secs(1),
            true,
            true,
            1,
        )
        .unwrap(),
    );
    let ug_provider = Arc::new(
        UndergroundOverlayProvider::new(
            Some(paths.ug_overlays_dir.clone()),
            Some(Arc::clone(&maps_db)),
            Duration::from_secs(1),
            true,
            true,
            1,
        )
        .unwrap(),
    );

    let lan_companion = Arc::new(LanCompanionServer::new(gh_net::lan_companion::LanCompanionConfig {
        host: "127.0.0.1".to_owned(),
        port: cfg.lan_companion_port,
        advertised_host: Some("127.0.0.1".to_owned()),
        ..Default::default()
    }));
    let server = SyncServer::new(SyncServerConfig {
        port: 0,
        config: cfg.clone(),
        config_path: Some(config_path.clone()),
        diagnostics_dir: Some(paths.diagnostics_dir.clone()),
        lan_companion: Some(Arc::clone(&lan_companion)),
        ..Default::default()
    });

    let game_state = Mutex::new(GameState::default());
    let nav_state_snapshot = Mutex::new(None);

    let srv_for_bs = server.clone();
    let test_log_buf = crate::log_buffer::LogRingBuffer::default();
    let test_mm_ring = MinimapPreLostRingBuffer::default();
    let diagnostics = Arc::new(
        DiagnosticManager::new(
            paths.clone(),
            Arc::clone(&capturer),
            None,
            Arc::clone(&config),
            Arc::new(move || srv_for_bs.last_browser_state()),
            Arc::new(|| None),
            Arc::new(|| None),
        )
        .with_log_buffer(test_log_buf)
        .with_minimap_ring(test_mm_ring.clone()),
    );

    let test_icons_dir = paths.cache_dir.join("lan_icons");
    let (stop_tx, _stop_rx) = watch::channel(false);

    Arc::new(AppState {
        paths,
        config_path,
        config,
        cli_port: cli_port.filter(|&p| p > 0),
        cli_lan_port: cli_lan_port.filter(|&lp| lp > 0),
        disk_ports: Mutex::new(disk_ports),
        config_io_lock: Mutex::new(()),
        last_self_saved_mtime: Mutex::new(None),
        engine_active: AtomicBool::new(true),
        anchor_seq: AtomicU64::new(0),
        game_state,
        nav_state_snapshot,
        nav_controller: Mutex::new(NavigationController::new().with_space_debounce_frames(4)),
        route_state: Mutex::new(RouteState::default()),
        nav_mark_confirm: Mutex::new(NavMarkConfirm::default()),
        tracker: Mutex::new(None),
        locator: None,
        state_machine: Mutex::new(MapScreenStateMachine::with_anchors_path(None)),
        capturer,
        server,
        lan_companion,
        nav_hud: Arc::new(NavMiniHud::new(gh_hud::nav_hud::NavHudConfig {
            screen_offset: gh_hud::layered::OFFSCREEN_TEST_OFFSET,
            ..Default::default()
        })),
        mmap_overlay: Arc::new(MMapOverlayController::new_with_offset(
            gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        )),
        mmap_view: None,
        radar_map: Mutex::new(None),
        radar_active: AtomicBool::new(false),
        radar_pins: Mutex::new(None),
        pin_icon_cache: PinIconCache::new(
            test_icons_dir,
            Arc::new(HoYoLABLabelCatalog::new()),
            std::time::Duration::from_secs(2),
        ),
        hud_toast: Arc::new(HudToastOverlay::with_screen_offset(
            gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        )),
        audio: Arc::new(AudioFeedback::new(false, 50)),
        tray: Mutex::new(None),
        hotkey_mgr: Arc::new(UnifiedHotkeyManager::new(false, None)),
        conflict_advisor: Arc::new(HotkeyConflictAdvisor::new()),
        guide_fetcher: PinGuideFetcher::new(Arc::new(get_language)),
        label_catalog: HoYoLABLabelCatalog::new(),
        maps_db,
        affine_registry: PerMapAffineRegistry::new(),
        hd_provider,
        ug_provider,
        minimap_recorder: Mutex::new(None),
        minimap_ring: test_mm_ring,
        diagnostics,
        updater: Arc::new(AppUpdater::default()),
        stop_tx,
        start_instant: Instant::now(),
    })
}

pub struct AppRadarAssetSource {
    pub hd_provider: Arc<HoyolabHdTileProvider>,
    pub ug_provider: Arc<UndergroundOverlayProvider>,
    pub icon_cache: PinIconCache,
    pub maps_db: Arc<MultimapDb>,
}

impl RadarAssetSource for AppRadarAssetSource {
    fn maps_db(&self) -> Arc<MultimapDb> {
        Arc::clone(&self.maps_db)
    }

    fn tile_generation(&self) -> u64 {
        self.hd_provider.tile_generation()
    }

    fn icon_generation(&self) -> u64 {
        self.icon_cache.generation()
    }

    fn get_color_tile_bytes(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> Option<Vec<u8>> {
        self.hd_provider
            .get_color_tile_bytes(map_id, zoom, tx, ty, Some(true))
    }

    fn prefetch_color_tiles_async(&self, map_id: i32, tiles: &[(i32, i32, i32)]) {
        self.hd_provider.prefetch_color_tiles_async(map_id, tiles);
    }

    fn get_ug_overlay_bytes(&self, map_id: i32, group_id: i32, floor_id: i32) -> Option<Vec<u8>> {
        use gh_core::traits::UndergroundOverlaySource;
        let spec = format!("{group_id}:{floor_id}");
        self.ug_provider.get_overlay_bytes(map_id, &spec, true)
    }

    fn get_icon_bytes_or_request(&self, map_id: i32, label_id: i32) -> Option<Vec<u8>> {
        if let Some(bytes) = self.icon_cache.get_cached_bytes(map_id, label_id) {
            return Some(bytes);
        }
        self.icon_cache.request_icon_async(map_id, label_id);
        None
    }

    fn get_marker_ui_asset_or_request(&self, key: &str) -> Option<Vec<u8>> {
        let asset = gh_net::pin_guide::MarkerUiAsset::from_key(key)?;
        if let Some(bytes) = self.icon_cache.get_ui_asset_bytes(asset) {
            return Some(bytes);
        }
        self.icon_cache.request_ui_asset_async(asset);
        None
    }
}

pub(crate) struct RadarRouteOverlayData {
    pub pins: Arc<Vec<RadarPin>>,
    pub excluded_ids: Arc<HashSet<String>>,
    pub target: Option<RadarTargetHint>,
    pub route_anchor: Option<RadarTargetHint>,
    pub route_legs: Option<Vec<RadarRouteLeg>>,
}

/// Single-source-of-truth Radar overlay projection built directly from `NavigationController`
/// (shared with Mini-HUD and M-Map Overlay):
/// - Preserves existing sidebar filter pins (`filter_pins`) intact (Mode A).
/// - When an active `RouteDocument` matches `map_id`, emits section-colored `RadarRouteLeg`s
///   (identical edges & colors to M-Map), merges missing uncollected `Collectible` stops into
///   the pin list, and surfaces the active section's suggested `anchor_teleport` waypoint highlight.
pub(crate) fn build_radar_route_overlay(
    nav: &mut NavigationController,
    map_id: i32,
    filter_pins: &Arc<Vec<RadarPin>>,
) -> RadarRouteOverlayData {
    let excluded = Arc::new(nav.excluded_ids());
    let locked_id = nav.locked_target_pin().map(|p| p.id.clone());
    let active_accent = nav
        .active_route_doc()
        .filter(|(doc, _)| doc.map_id == map_id)
        .and_then(|(doc, sec_idx)| doc.sections.get(sec_idx))
        .map(|sec| parse_hex_rgba(&sec.color, 235));

    let target = nav.current_target().map(|p| RadarTargetHint {
        id: p.id.clone(),
        lat: p.lat,
        lng: p.lng,
        is_locked: locked_id.as_deref() == Some(p.id.as_str()),
        is_underground: p.is_underground,
        floor_id: p.floor_id.clone(),
        is_route_stop: p.stop_kind.is_some(),
        accent: p.stop_kind.as_ref().and(active_accent),
    });

    let Some((doc, active_sec_idx)) = nav
        .active_route_doc()
        .filter(|(doc, _)| doc.map_id == map_id)
    else {
        return RadarRouteOverlayData {
            pins: Arc::clone(filter_pins),
            excluded_ids: excluded,
            target,
            route_anchor: None,
            route_legs: None,
        };
    };

    let mut legs: Vec<RadarRouteLeg> = Vec::new();
    let mut seen_ids: HashSet<&str> = filter_pins.iter().map(|p| p.id.as_str()).collect();
    let mut extra_pins: Vec<RadarPin> = Vec::new();
    let mut step_counter = 0usize;

    for (si, sec) in doc.sections.iter().enumerate() {
        let emphasized = si == active_sec_idx;
        let color = parse_hex_rgba(&sec.color, if emphasized { 235 } else { 125 });

        if emphasized
            && let Some(ref anchor) = sec.anchor_teleport
            && let Some(first) = sec.stops.first()
            && (anchor.lat - first.lat).hypot(anchor.lng - first.lng) > 12.0
        {
            legs.push(RadarRouteLeg {
                from_lat: anchor.lat,
                from_lng: anchor.lng,
                to_lat: first.lat,
                to_lng: first.lng,
                done: nav.is_route_stop_done(&first.id),
                step_index: step_counter,
                color,
                emphasized: true,
            });
            step_counter += 1;
        }

        if sec.edges.is_empty() {
            for pair in sec.stops.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                legs.push(RadarRouteLeg {
                    from_lat: a.lat,
                    from_lng: a.lng,
                    to_lat: b.lat,
                    to_lng: b.lng,
                    done: nav.is_route_stop_done(&a.id) && nav.is_route_stop_done(&b.id),
                    step_index: step_counter,
                    color,
                    emphasized,
                });
                step_counter += 1;
            }
        } else {
            for &(ai, bi) in &sec.edges {
                if let (Some(a), Some(b)) = (sec.stops.get(ai), sec.stops.get(bi)) {
                    legs.push(RadarRouteLeg {
                        from_lat: a.lat,
                        from_lng: a.lng,
                        to_lat: b.lat,
                        to_lng: b.lng,
                        done: nav.is_route_stop_done(&a.id) && nav.is_route_stop_done(&b.id),
                        step_index: step_counter,
                        color,
                        emphasized,
                    });
                    step_counter += 1;
                }
            }
        }

        for stop in &sec.stops {
            if stop.kind != RouteStopKind::Collectible
                || nav.is_route_stop_done(&stop.id)
                || !seen_ids.insert(stop.id.as_str())
            {
                continue;
            }
            extra_pins.push(RadarPin {
                id: stop.id.clone(),
                lat: stop.lat,
                lng: stop.lng,
                label_id: stop.label_id,
                floor_id: stop.floor_id.clone(),
                marked: false,
                is_underground: stop.is_underground,
                icon_sign: if stop.is_underground { 1 } else { 0 },
                z_level: 0,
            });
        }
    }

    let route_anchor = doc.sections.get(active_sec_idx).and_then(|sec| {
        let has_remaining = sec
            .stops
            .iter()
            .any(|s| s.kind == RouteStopKind::Collectible && !nav.is_route_stop_done(&s.id));
        if !has_remaining {
            return None;
        }
        let anchor = sec.anchor_teleport.as_ref()?;
        let already_targeted = target
            .as_ref()
            .is_some_and(|t| (t.lat - anchor.lat).hypot(t.lng - anchor.lng) <= 14.0);
        if already_targeted {
            return None;
        }
        Some(RadarTargetHint {
            id: anchor
                .point_id
                .clone()
                .unwrap_or_else(|| "__route_anchor__".to_owned()),
            lat: anchor.lat,
            lng: anchor.lng,
            is_locked: false,
            is_underground: anchor.is_underground,
            floor_id: anchor.floor_id.clone(),
            is_route_stop: true,
            accent: active_accent,
        })
    });

    let merged_pins = if extra_pins.is_empty() {
        Arc::clone(filter_pins)
    } else {
        let mut combined = Vec::with_capacity(filter_pins.len() + extra_pins.len());
        combined.extend(filter_pins.iter().cloned());
        combined.extend(extra_pins);
        Arc::new(combined)
    };

    RadarRouteOverlayData {
        pins: merged_pins,
        excluded_ids: excluded,
        target,
        route_anchor,
        route_legs: Some(legs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static STATE_TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn set_hotkeys_rebinding_kb_to_mouse_under_read_contention_does_not_deadlock() {
        let seq = STATE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_state_hk_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);
        let state_weak = Arc::downgrade(&state);
        state.hotkey_mgr.set_should_suppress(Some(Arc::new(move |_| {
            state_weak
                .upgrade()
                .map(|st| st.config.read().expect("config lock poisoned").hotkey_suppress_conflicts)
                .unwrap_or(false)
        })));

        {
            let cfg = state.config.read().unwrap().clone();
            for act in HOTKEY_ACTIONS {
                let hk = get_hotkey_field(&cfg, act);
                state
                    .hotkey_mgr
                    .register(act, &hk, Arc::new(|| {}), true, None);
            }
        }
        assert!(state.hotkey_mgr.start());

        let stop_reader = Arc::new(AtomicBool::new(false));
        let reader_ready = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop_reader);
        let reader_ready_tx = Arc::clone(&reader_ready);
        let reader_state = Arc::clone(&state);
        let reader_handle = std::thread::spawn(move || {
            let mut reads = 0usize;
            while !reader_stop.load(Ordering::Acquire) {
                let _suppress = reader_state
                    .config
                    .read()
                    .expect("config lock poisoned")
                    .hotkey_suppress_conflicts;
                let _disp = reader_state.hotkey_mgr.get_display_name("sync");
                reads += 1;
                if reads == 1 {
                    reader_ready_tx.store(true, Ordering::Release);
                }
                if reads.is_multiple_of(64) {
                    std::thread::yield_now();
                }
            }
            reads
        });

        let wait_start = Instant::now();
        while !reader_ready.load(Ordering::Acquire) {
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "reader_handle did not enter read loop within 5s"
            );
            std::thread::yield_now();
        }

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker_handle = std::thread::spawn(move || {
            // Transition all 9 actions from keyboard -> mouse4 so `need_kb` becomes false
            // and `UnifiedHotkeyManager::update_binding` runs `join_hook_thread` on `gh-kb-hook`.
            let mut to_mouse = Map::new();
            for act in HOTKEY_ACTIONS {
                to_mouse.insert(format!("hotkey_{act}"), json!("mouse4"));
            }
            let r1 = worker_state.set_hotkeys(&Value::Object(to_mouse)).unwrap();
            assert_eq!(r1["ok"], json!(true));

            // Transition back to keyboard (`f9`) and then back to mouse (`mouse5`) under contention.
            let r2 = worker_state
                .set_hotkeys(&json!({"action": "sync", "hotkey": "f9"}))
                .unwrap();
            assert_eq!(r2["ok"], json!(true));
            assert_eq!(r2["results"]["sync"]["hotkey"], json!("f9"));

            let r3 = worker_state
                .set_hotkeys(&json!({"action": "sync", "hotkey": "mouse5"}))
                .unwrap();
            assert_eq!(r3["ok"], json!(true));
            assert_eq!(r3["results"]["sync"]["hotkey"], json!("mouse5"));

            // Partial batch failure: valid `nav_mark` updates, invalid `debug` stays at prior binding.
            let r4 = worker_state
                .set_hotkeys(&json!({
                    "hotkey_nav_mark": "f10",
                    "hotkey_debug": "not_a_valid_key_xyz",
                }))
                .unwrap();
            assert_eq!(r4["ok"], json!(false));
            assert_eq!(r4["results"]["nav_mark"]["ok"], json!(true));
            assert_eq!(r4["results"]["nav_mark"]["hotkey"], json!("f10"));
            assert_eq!(r4["results"]["debug"]["ok"], json!(false));

            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("set_hotkeys deadlocked during kb <-> mouse rebinding under config.read contention");
        worker_handle.join().unwrap();

        stop_reader.store(true, Ordering::Release);
        let read_iters = reader_handle.join().unwrap();
        assert!(read_iters > 0);

        {
            let cfg = state.config.read().unwrap();
            assert_eq!(cfg.hotkey_sync, "mouse5");
            assert_eq!(cfg.hotkey, "mouse5");
            assert_eq!(cfg.hotkey_nav_mark, "f10");
            assert_eq!(cfg.hotkey_debug, "mouse4");
        }
        let disk_cfg = AppConfig::load_from_file(&state.config_path).unwrap();
        assert_eq!(disk_cfg.hotkey_sync, "mouse5");
        assert_eq!(disk_cfg.hotkey_nav_mark, "f10");
        assert_eq!(disk_cfg.hotkey_debug, "mouse4");

        state.hotkey_mgr.stop();
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn http_get_json(port: u16, path: &str) -> Value {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect test sync_server");
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.expect("write GET");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read response");
        let sep = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP header separator");
        serde_json::from_slice(&raw[sep + 4..]).expect("JSON response body")
    }

    #[tokio::test]
    async fn handle_nav_undo_hotkey_warns_when_web_offline_and_broadcasts_unmark_when_online() {
        let seq = STATE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_state_undo_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);
        state
            .nav_hud
            .update(Default::default(), None, None, None, None, None, None, 0.0, None, None);

        {
            let mut nav = state.nav_controller.lock().unwrap();
            nav.set_pins(vec![NavPin::new("201", "Test Chest", 15.0, 25.0)], true, None);
            nav.mark_collected(Some("201"));
            assert!(nav.collected_ids.contains("201"));
        }

        // Offline web map -> local undo succeeds, HUD flashes web_unmark_failed warning.
        state.handle_nav_undo_hotkey();
        assert!(
            !state
                .nav_controller
                .lock()
                .unwrap()
                .collected_ids
                .contains("201")
        );
        let want_offline = t("hud.flash.web_unmark_failed", &[]);
        let dl1 = Instant::now() + Duration::from_secs(2);
        while Instant::now() < dl1 {
            if state.nav_hud.last_render_meta().subtitle == want_offline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.nav_hud.last_render_meta().subtitle, want_offline);
        assert_eq!(state.nav_hud.last_render_meta().palette, HudPalette::Warning);

        // Online web map -> broadcasts NAV_MARK_PIN { action: "unmark" } and flashes undone.
        let port = state.server.start().await.unwrap();
        let hb_resp = http_get_json(port, "/poll?since=-1").await;
        let start_seq = hb_resp["_seq"].as_u64().unwrap_or(0);

        {
            let mut nav = state.nav_controller.lock().unwrap();
            nav.mark_collected(Some("201"));
            assert!(nav.collected_ids.contains("201"));
        }
        state.handle_nav_undo_hotkey();
        assert!(
            !state
                .nav_controller
                .lock()
                .unwrap()
                .collected_ids
                .contains("201")
        );

        let ev = http_get_json(port, &format!("/poll?since={start_seq}")).await;
        assert_eq!(ev["type"], json!("NAV_MARK_PIN"));
        assert_eq!(ev["action"], json!("unmark"));
        assert_eq!(ev["pin_id"], json!("201"));

        let want_online = t("hud.flash.undone", &[("name", "Test Chest")]);
        let dl2 = Instant::now() + Duration::from_secs(2);
        while Instant::now() < dl2 {
            if state.nav_hud.last_render_meta().subtitle == want_online {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.nav_hud.last_render_meta().subtitle, want_online);

        state.server.stop().await;
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn handle_lan_client_message_handles_mark_select_target_and_floor_sync() {
        let seq = STATE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_state_lan_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);
        let port = state.server.start().await.unwrap();
        let hb = http_get_json(port, "/poll?since=-1").await;
        let seq0 = hb["_seq"].as_u64().unwrap_or(0);

        {
            let mut gs = state.game_state.lock().unwrap();
            gs.last_pos = Some(json!({
                "map_id": 2,
                "lat": 100.0,
                "lng": 200.0,
                "floor_id": "372",
                "floor_name": "B1",
                "is_underground": true
            }));
        }
        let payload = state.build_lan_state_payload();
        assert_eq!(payload["floor_id"], json!("372"));
        assert!(payload.get("mirror").is_none());

        // SELECT_TARGET outside nearby window merges & locks target
        state.handle_lan_client_message(json!({
            "type": "SELECT_TARGET",
            "pin_id": "555",
            "map_id": 2,
            "lat": 300.0,
            "lng": 400.0,
            "name": "Distant Luxurious Chest",
            "floor_id": "372",
            "is_underground": true
        }));
        {
            let mut nav = state.nav_controller.lock().unwrap();
            assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("555"));
        }

        // MARK_PIN mark & unmark broadcasts NAV_MARK_PIN and toggles collected_ids
        state.handle_lan_client_message(json!({
            "type": "MARK_PIN",
            "action": "mark",
            "pin_id": "555",
            "map_id": 2,
            "lat": 300.0,
            "lng": 400.0,
            "name": "Distant Luxurious Chest"
        }));
        assert!(state.nav_controller.lock().unwrap().collected_ids.contains("555"));

        state.handle_lan_client_message(json!({
            "type": "MARK_PIN",
            "action": "unmark",
            "pin_id": "555",
            "map_id": 2,
            "lat": 300.0,
            "lng": 400.0,
            "name": "Distant Luxurious Chest"
        }));
        assert!(!state.nav_controller.lock().unwrap().collected_ids.contains("555"));

        // SET_FLOOR_PC relays LAN_SET_FLOOR
        state.handle_lan_client_message(json!({
            "type": "SET_FLOOR_PC",
            "floor_id": "372",
            "floor_name": "B1"
        }));

        let mut saw_mark = false;
        let mut saw_unmark = false;
        let mut saw_set_floor = false;
        let mut cur_seq = seq0;
        for _ in 0..8 {
            let ev = http_get_json(port, &format!("/poll?since={cur_seq}")).await;
            let next_s = ev["_seq"].as_u64().unwrap_or(cur_seq);
            if next_s > cur_seq {
                cur_seq = next_s;
            }
            match ev["type"].as_str() {
                Some("NAV_MARK_PIN") => {
                    if ev["action"] == "mark" {
                        saw_mark = true;
                    } else if ev["action"] == "unmark" {
                        saw_unmark = true;
                    }
                }
                Some("LAN_SET_FLOOR") if ev["floor_id"] == json!("372") => {
                    saw_set_floor = true;
                }
                _ => {}
            }
            if saw_mark && saw_unmark && saw_set_floor {
                break;
            }
        }
        assert!(saw_mark && saw_unmark && saw_set_floor);

        state.server.stop().await;
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn build_mmap_guide_scene_follows_the_registered_view_not_the_player() {
        use gh_hud::mmap_overlay::MMapGuideStage;

        let root = std::env::temp_dir().join(format!(
            "gh_state_mmap_guide_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let state = make_test_state(&root);
        let route_payload = json!({
            "format": "genshin-helper-route",
            "version": 1,
            "title": "Nod-Krai Farm",
            "map_id": 2,
            "sections": [{
                "index": 0,
                "name": "S1",
                "anchor_teleport": { "lat": -3301.0, "lng": -10804.0, "label_id": 3 },
                "stops": [
                    { "id": "s1", "kind": "collectible", "lat": -3250.0, "lng": -10760.0, "label_id": 17 },
                    { "id": "s2", "kind": "collectible", "lat": -3150.0, "lng": -10600.0, "label_id": 17 }
                ]
            }]
        });
        let _ = state
            .handle_route_command(&route_payload)
            .expect("load test route");
        let rect = RectI32 { x: 0, y: 0, width: 2560, height: 1440 };
        let centred = |cx: f64, cy: f64, scale: f64, map_id: i32| ViewTransform {
            map_id,
            scale,
            tx: cx - scale * 1280.0,
            ty: cy - scale * 720.0,
        };
        // The player position is irrelevant: put it right at the target.
        state.game_state.lock().unwrap().last_pos =
            Some(json!({ "map_id": 2, "lat": -3301.0, "lng": -10804.0 }));

        // View far away on Teyvat -> edge arrow toward the route.
        let s1 = state
            .build_mmap_guide_scene(rect, centred(0.0, 0.0, 1.9, 2), MMapOverlayMode::Full, "vi")
            .expect("route arrow");
        assert_eq!(s1.stage, MMapGuideStage::Route);
        assert!(s1.arrow.expect("arrow").label.contains("Nod-Krai Farm"));

        // Zoomed into the section -> teleport ring + card.
        let s2 = state
            .build_mmap_guide_scene(rect, centred(-10700.0, -3220.0, 0.5, 2), MMapOverlayMode::Full, "vi")
            .expect("teleport");
        assert_eq!(s2.stage, MMapGuideStage::Teleport);
        assert_eq!(s2.teleport.expect("teleport").title, "Dịch chuyển tại đây");

        // Viewing Enkanomiya -> wrong-map banner naming Teyvat's localised name.
        let s3 = state
            .build_mmap_guide_scene(rect, centred(0.0, 0.0, 1.0, 7), MMapOverlayMode::Full, "vi")
            .expect("wrong map");
        assert_eq!(s3.stage, MMapGuideStage::WrongMap);

        assert!(
            state
                .build_mmap_guide_scene(rect, centred(0.0, 0.0, 1.0, 2), MMapOverlayMode::Off, "vi")
                .is_none()
        );

        // No registered view -> text banner; drawer open -> which map to pick.
        let fb = state
            .build_mmap_fallback_scene(rect, MMapOverlayMode::Full, "vi", false)
            .expect("fallback");
        assert_eq!(fb.stage, MMapGuideStage::Locating);
        let (title, _) = fb.banner.expect("banner");
        assert!(title.contains("Nod-Krai Farm") && title.contains("S1/1"), "{title}");
        let (_, sub) = state
            .build_mmap_fallback_scene(rect, MMapOverlayMode::Full, "vi", true)
            .and_then(|s| s.banner)
            .expect("drawer banner");
        assert!(sub.starts_with("Chọn bản đồ:"), "{sub}");

        state.mmap_overlay.close();
        state.server.stop().await;
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn target_lock_can_be_cleared_via_hotkey_and_lan_message() {
        let seq = STATE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_state_unlock_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);

        // 1. Lock via LAN message, then clear via in-game hotkey (`nav_unlock`).
        state.handle_lan_client_message(json!({
            "type": "LOCK_TARGET",
            "pin_id": "pin_9001",
            "map_id": 2,
            "lat": -100.0,
            "lng": 200.0,
            "name": "Exquisite Chest",
        }));
        assert_eq!(
            state
                .nav_controller
                .lock()
                .unwrap()
                .locked_target_pin()
                .map(|p| p.id.as_str()),
            Some("pin_9001")
        );
        state.handle_nav_unlock_hotkey();
        assert!(state.nav_controller.lock().unwrap().locked_target_pin().is_none());

        // 2. Lock again, then clear via LAN `UNLOCK_TARGET` message.
        state.handle_lan_client_message(json!({
            "type": "LOCK_TARGET",
            "pin_id": "pin_9002",
            "map_id": 2,
            "lat": -50.0,
            "lng": 75.0,
            "name": "Precious Chest",
        }));
        assert!(state.nav_controller.lock().unwrap().locked_target_pin().is_some());
        state.handle_lan_client_message(json!({ "type": "UNLOCK_TARGET" }));
        assert!(state.nav_controller.lock().unwrap().locked_target_pin().is_none());

        state.mmap_overlay.close();
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_radar_enforces_default_off_and_strict_zero_overhead_teardown() {
        let seq = STATE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_state_radar_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);

        // 1. Default OFF: zero threads/HWND/pins allocated.
        assert!(!state.config.read().unwrap().radar_enabled);
        assert_eq!(state.config.read().unwrap().hotkey_radar_toggle, "alt+`");
        assert!(!state.is_radar_active());
        assert!(state.radar_map.lock().unwrap().is_none());
        assert!(state.radar_pins.lock().unwrap().is_none());

        let sample_pins = json!({
            "map_id": 2,
            "names": ["Chest"],
            "pins": [["7001", -12.0, 34.0, 17, null, 0, 0, 0]]
        });
        // While OFF, incoming `/lan_pins` must be a zero-cost no-op.
        state.on_lan_pins_received(&sample_pins);
        assert!(state.radar_pins.lock().unwrap().is_none());

        // 2. Turn ON via `set_radar`: spawns controller + hydrates pins.
        let r_on = state
            .set_radar(&json!({"enabled": true, "radar_zoom": -0.3, "radar_opacity": 0.88}))
            .expect("enable radar");
        assert_eq!(r_on["radar_enabled"], json!(true));
        assert!(state.is_radar_active());
        assert!(state.radar_map.lock().unwrap().is_some());
        state.on_lan_pins_received(&sample_pins);
        {
            let pins_guard = state.radar_pins.lock().unwrap();
            let (mid, list) = pins_guard.as_ref().expect("radar pins cached while ON");
            assert_eq!(*mid, 2);
            assert_eq!(list.len(), 1);
        }

        // 3. Turn OFF via `set_radar`: tears down controller, threads, HWND, and pin cache.
        let r_off = state
            .set_radar(&json!({"enabled": false}))
            .expect("disable radar");
        assert_eq!(r_off["radar_enabled"], json!(false));
        assert!(!state.is_radar_active());
        assert!(state.radar_map.lock().unwrap().is_none());
        assert!(state.radar_pins.lock().unwrap().is_none());

        // Incoming `/lan_pins` is again ignored while OFF.
        state.on_lan_pins_received(&sample_pins);
        assert!(state.radar_pins.lock().unwrap().is_none());

        state.shutdown_radar();
        state.mmap_overlay.close();
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn radar_route_overlay_merges_filter_pins_with_route_legs_and_waypoint_anchor() {
        use gh_core::route_doc::{RouteAnchor, RouteDocument, RouteSection, RouteStop};

        let mut nav = NavigationController::new();
        let doc = RouteDocument {
            title: "Unified Radar Route".to_owned(),
            map_id: 2,
            sections: vec![
                RouteSection {
                    index: 0,
                    name: "Section 1".to_owned(),
                    color: "#fde047".to_owned(),
                    anchor_teleport: Some(RouteAnchor {
                        point_id: Some("wp_anchor_1".to_owned()),
                        lat: 10.0,
                        lng: 10.0,
                        label_id: 3,
                        ..Default::default()
                    }),
                    stops: vec![
                        RouteStop {
                            id: "route_pin_1".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 50.0,
                            lng: 20.0,
                            label_id: 17,
                            name: "Chest 1".to_owned(),
                            ..Default::default()
                        },
                        RouteStop {
                            id: "route_pin_2".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 90.0,
                            lng: 40.0,
                            label_id: 17,
                            name: "Chest 2".to_owned(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                RouteSection {
                    index: 1,
                    name: "Section 2".to_owned(),
                    color: "#4ade80".to_owned(),
                    stops: vec![
                        RouteStop {
                            id: "sec2_a".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 300.0,
                            lng: 300.0,
                            label_id: 44,
                            name: "Oculus A".to_owned(),
                            ..Default::default()
                        },
                        RouteStop {
                            id: "sec2_b".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 350.0,
                            lng: 320.0,
                            label_id: 44,
                            name: "Oculus B".to_owned(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(nav.load_route_document(doc, Some(0), &[]), Some(0));

        // Sidebar filter has 1 unrelated pin + 1 overlapping route pin ("route_pin_1").
        let filter_pins = Arc::new(vec![
            RadarPin {
                id: "filter_only_99".to_owned(),
                lat: 12.0,
                lng: 15.0,
                label_id: 18,
                floor_id: None,
                marked: false,
                is_underground: false,
                icon_sign: 0,
                z_level: 0,
            },
            RadarPin {
                id: "route_pin_1".to_owned(),
                lat: 50.0,
                lng: 20.0,
                label_id: 17,
                floor_id: None,
                marked: false,
                is_underground: false,
                icon_sign: 0,
                z_level: 0,
            },
        ]);

        let ov = build_radar_route_overlay(&mut nav, 2, &filter_pins);

        // Mode A: keeps both filter pins + appends missing route collectibles without duplicating route_pin_1.
        let ids: Vec<&str> = ov.pins.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["filter_only_99", "route_pin_1", "route_pin_2", "sec2_a", "sec2_b"]
        );

        // Active target is route_pin_1 with route styling; anchor_teleport is surfaced separately for highlighting.
        let tgt = ov.target.expect("active route target");
        assert_eq!(tgt.id, "route_pin_1");
        assert!(tgt.is_route_stop);
        assert_eq!(tgt.accent, Some([0xfd, 0xe0, 0x47, 235]));

        let anc = ov.route_anchor.expect("suggested anchor teleport highlight");
        assert_eq!(anc.id, "wp_anchor_1");
        assert!(anc.is_route_stop);

        // Legs include anchor->pin1 (active), pin1->pin2 (active), and sec2_a->sec2_b (secondary).
        let legs = ov.route_legs.expect("route legs from active RouteDocument");
        assert_eq!(legs.len(), 3);
        assert!(legs[0].emphasized && !legs[0].done);
        assert!(legs[1].emphasized && !legs[1].done);
        assert!(!legs[2].emphasized && !legs[2].done);
        assert_eq!(legs[0].color, [0xfd, 0xe0, 0x47, 235]);
        assert_eq!(legs[2].color, [0x4a, 0xde, 0x80, 125]);

        // Different map_id returns filter pins unchanged and None legs.
        let other = build_radar_route_overlay(&mut nav, 9, &filter_pins);
        assert_eq!(other.pins.len(), 2);
        assert!(other.route_anchor.is_none());
        assert!(other.route_legs.is_none());
    }
}
