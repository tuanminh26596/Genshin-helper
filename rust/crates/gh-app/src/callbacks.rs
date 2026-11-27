//! Concrete `gh_core::traits::ServerCallbacks` & `ConflictAdvisor` bridging `gh-net` to subsystems.

use crate::state::{AppState, get_hotkey_field, json_f64, json_i64, json_str_trimmed, json_truthy};
use gh_core::i18n::{get_language, t};
use gh_core::map_bridge::{
    CanonicalAnchor, execute_cross_map_export, execute_provider_calibration,
};
use gh_core::nav_controller::{NavAction, NavPin, normalize_raw_nav_pin};
use gh_core::traits::{ConflictAdvisor, ServerCallbacks};
use gh_core::tsp_route::{
    DEFAULT_TELEPORT_PENALTY, TspCoordInput, TspPinInput, TspPlanOptions, TspWaypoint,
    TspWaypointInput, normalize_waypoints, plan_route, waypoints_from_multimap,
};
use gh_core::{CoreError, CoreResult};
use gh_hud::toast::ToastKind;
use gh_net::pin_guide::{HoYoLABLabelCatalog, PinGuideFetcher};
use gh_net::sync_server::HOTKEY_ACTIONS;
use gh_win32::capture::WindowFinder;
use gh_win32::game_settings::launch_genshin_borderless;
use gh_win32::switcher::WindowSwitcher;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct AppServerCallbacks {
    pub state: Arc<AppState>,
    /// When `true` (offline replay), suppresses real desktop window focus changes (`auto_switch`).
    suppress_window_switch: bool,
}

impl AppServerCallbacks {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            suppress_window_switch: false,
        }
    }

    pub fn new_offline_replay(state: Arc<AppState>) -> Self {
        Self {
            state,
            suppress_window_switch: true,
        }
    }

    fn record_in_game_feedback(&self, event_type: &str, status: &str, seq: u64, fb: &InGameFeedback<'_>) {
        self.state.record_session_event_with("hud_feedback", || {
            json!({
                "event_type": event_type,
                "status": status,
                "seq": seq,
                "shown": fb.shown,
                "reason": fb.reason,
                "output": fb.output,
                "delivered": fb.delivered,
                "sound": fb.sound,
                "sound_played": fb.sound_played,
            })
        });
    }
}

/// Whether the in-game HUD toast + chime should answer this web feedback.
///
/// A position-only M-Map sync (`SYNC_POSITION_ONLY` / `synced`) is always confirmed in game:
/// the HoYoLAB page never beeps or toasts for it, so deferring to a visible map tab (for example
/// on a second monitor) left the user with no feedback at all. Other feedback (marks, errors)
/// keeps the visibility rule because the page plays its own beep for those.
fn app_owns_feedback(event_type: &str, status: &str, native_feedback_wanted: impl FnOnce() -> bool) -> bool {
    let position_only_sync =
        event_type.eq_ignore_ascii_case("SYNC_POSITION_ONLY") && status.eq_ignore_ascii_case("synced");
    position_only_sync || native_feedback_wanted()
}

/// What the app did in game for one web feedback. Recorded as a `hud_feedback` session event
/// next to the `web_feedback` it answers, so `gh-sim qa` can check from a live recording that
/// every confirmed M-Map sync got a toast and a chime.
struct InGameFeedback<'a> {
    shown: bool,
    /// Why nothing was shown (empty when `shown`).
    reason: &'a str,
    /// `toast`, `nav_flash`, or empty when nothing was shown.
    output: &'a str,
    /// The toast/flash was handed to its HUD window (`show_toast` posted it).
    delivered: bool,
    sound: &'a str,
    /// The chime was queued (false when the user muted sounds).
    sound_played: bool,
}

impl<'a> InGameFeedback<'a> {
    fn skipped(reason: &'a str) -> Self {
        Self {
            shown: false,
            reason,
            output: "",
            delivered: false,
            sound: "",
            sound_played: false,
        }
    }
}

impl ServerCallbacks for AppServerCallbacks {
    fn get_status_overrides(&self) -> Value {
        let cfg = self.state.config.read().expect("config lock poisoned").clone();
        let gs = self
            .state
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let engine_active = self.state.engine_active.load(Ordering::Acquire);
        let uipi_mismatch = gs.uipi_status.as_ref().is_some_and(|u| u.mismatch);
        let uipi_val = gs
            .uipi_status
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok())
            .unwrap_or(Value::Null);
        let sync_d = self.state.hotkey_mgr.get_display_name("sync");

        let locked_target_val = {
            let nav = self
                .state
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            nav.locked_target_pin()
                .map(NavPin::to_wire_json)
                .unwrap_or(Value::Null)
        };

        let mut out = json!({
            "hotkey": cfg.hotkey,
            "display_name": sync_d,
            "hotkey_sync": cfg.hotkey_sync,
            "display_name_sync": sync_d,
            "hotkey_debug": cfg.hotkey_debug,
            "display_name_debug": self.state.hotkey_mgr.get_display_name("debug"),
            "hotkey_switch": cfg.hotkey_switch,
            "display_name_switch": self.state.hotkey_mgr.get_display_name("switch"),
            "hotkey_nav_mark": cfg.hotkey_nav_mark,
            "display_name_nav_mark": self.state.hotkey_mgr.get_display_name("nav_mark"),
            "hotkey_nav_undo": cfg.hotkey_nav_undo,
            "display_name_nav_undo": self.state.hotkey_mgr.get_display_name("nav_undo"),
            "hotkey_nav_guide": cfg.hotkey_nav_guide,
            "display_name_nav_guide": self.state.hotkey_mgr.get_display_name("nav_guide"),
            "hotkey_nav_next": cfg.hotkey_nav_next,
            "display_name_nav_next": self.state.hotkey_mgr.get_display_name("nav_next"),
            "hotkey_nav_layout": cfg.hotkey_nav_layout,
            "display_name_nav_layout": self.state.hotkey_mgr.get_display_name("nav_layout"),
            "hotkey_nav_toggle": cfg.hotkey_nav_toggle,
            "display_name_nav_toggle": self.state.hotkey_mgr.get_display_name("nav_toggle"),
            "hotkey_nav_unlock": cfg.hotkey_nav_unlock,
            "display_name_nav_unlock": self.state.hotkey_mgr.get_display_name("nav_unlock"),
            "hotkey_radar_toggle": cfg.hotkey_radar_toggle,
            "display_name_radar_toggle": self.state.hotkey_mgr.get_display_name("radar_toggle"),
            "hotkey_suppress_conflicts": cfg.hotkey_suppress_conflicts,
            "locked_target": locked_target_val,
            "monitor_count": WindowFinder::get_monitor_count(),
            "engine_active": engine_active,
            "language": get_language(),
            "language_setting": cfg.language,
            "nav_hud_edit_mode": self.state.nav_hud.is_edit_mode(),
            "mmap_overlay_mode": cfg.mmap_overlay_mode,
            "radar_enabled": cfg.radar_enabled,
            "radar_rect": cfg.radar_rect,
            "radar_zoom": cfg.radar_zoom,
            "radar_opacity": cfg.radar_opacity,
            "lan_companion_running": self.state.lan_companion.is_running(),
            "uipi_mismatch": uipi_mismatch,
            "uipi_status": uipi_val,
        });
        // Insert update fields directly to stay under serde_json::json!'s default recursion limit.
        if let Value::Object(ref mut m) = out {
            m.insert("app_update_mode".into(), Value::String(cfg.app_update_mode.clone()));
            m.insert(
                "auto_check_app_updates".into(),
                Value::Bool(cfg.auto_check_app_updates),
            );
            m.insert(
                "app_update".into(),
                self.state.updater.status_summary_json(&cfg.app_update_mode),
            );
            m.insert(
                "active_map_provider".into(),
                Value::String(self.state.server.active_provider()),
            );
        }
        out
    }

    fn on_engine_state_change(&self, active: bool) -> CoreResult<bool> {
        Ok(self.state.set_engine_active(active))
    }

    fn on_auto_sync_change(&self, enabled: bool) -> CoreResult<bool> {
        let _ = self.state.mutate_and_save_config(|cfg| {
            cfg.auto_sync_m = enabled;
        });
        if !enabled {
            self.state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
        }
        Ok(enabled)
    }

    /// Deprecated: `single_monitor_mode` no longer drives behavior (the switch hotkey is always
    /// on and feedback ownership is auto-detected via `AppState::native_feedback_wanted`). The
    /// flag is still persisted so `/set_single_monitor` and old `config.json` files stay valid.
    fn on_single_monitor_change(&self, enabled: bool) -> CoreResult<bool> {
        let _ = self.state.mutate_and_save_config(|cfg| {
            cfg.single_monitor_mode = enabled;
        });
        Ok(enabled)
    }

    fn on_action_feedback(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let event_type = json_str_trimmed(req.get("event_type")).to_ascii_uppercase();
        let status = json_str_trimmed(req.get("status")).to_ascii_lowercase();
        let seq = req
            .get("seq")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| json_i64(Some(v)).and_then(|n| u64::try_from(n).ok()))
            })
            .unwrap_or(0);
        self.state.record_session_event_with("web_feedback", || {
            json!({
                "event_type": event_type,
                "status": status,
                "seq": seq,
            })
        });
        let action = json_str_trimmed(req.get("action")).to_ascii_lowercase();
        let pin_id = json_str_trimmed(req.get("pin_id"));
        let title_raw = json_str_trimmed(req.get("title"));
        let detail_raw = json_str_trimmed(req.get("detail"));

        if event_type == "NAV_MARK" {
            if matches!(status.as_str(), "error" | "no_nearby" | "not_found") {
                if action != "unmark" && !pin_id.is_empty() {
                    let restored = {
                        let mut nav = self
                            .state
                            .nav_controller
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        nav.rollback_collected(&pin_id).is_some()
                    };
                    if restored {
                        self.state.push_lan_snapshot(true);
                    }
                }
                let msg = if !detail_raw.is_empty() {
                    detail_raw
                } else if action == "unmark" {
                    t("hud.flash.web_unmark_failed", &[])
                } else {
                    t("hud.flash.web_failed", &[])
                };
                self.state
                    .nav_hud
                    .flash(&msg, gh_hud::nav_hud::HudPalette::Warning, 3.5);
                let sound_played = self.state.audio.play("warning", false);
                self.record_in_game_feedback(
                    &event_type,
                    &status,
                    seq,
                    &InGameFeedback {
                        shown: true,
                        reason: "",
                        output: "nav_flash",
                        delivered: true,
                        sound: "warning",
                        sound_played,
                    },
                );
                return Ok(json!(true));
            }
            self.record_in_game_feedback(&event_type, &status, seq, &InGameFeedback::skipped("page_handles"));
            return Ok(json!(false));
        }

        if !app_owns_feedback(&event_type, &status, || self.state.native_feedback_wanted()) {
            self.record_in_game_feedback(&event_type, &status, seq, &InGameFeedback::skipped("map_tab_visible"));
            return Ok(json!(false));
        }

        let rect = self
            .state
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rect;

        let (title_key, kind, duration_ms, sound) = match status.as_str() {
            "marked" => ("toast.marked.title", ToastKind::Success, 2400, "success"),
            "already_marked" => ("toast.already_marked.title", ToastKind::Info, 2200, "info"),
            "synced" => ("toast.synced.title", ToastKind::Info, 2000, "info"),
            "no_nearby" | "no_coords" | "error" => ("toast.no_nearby.title", ToastKind::Warning, 2500, "warning"),
            _ => {
                self.record_in_game_feedback(&event_type, &status, seq, &InGameFeedback::skipped("unknown_status"));
                return Ok(json!(false));
            }
        };
        let title = if title_raw.is_empty() {
            t(title_key, &[])
        } else {
            title_raw
        };
        let delivered = self
            .state
            .hud_toast
            .show_toast(&title, &detail_raw, kind, duration_ms, rect);
        let sound_played = self.state.audio.play(sound, false);
        self.record_in_game_feedback(
            &event_type,
            &status,
            seq,
            &InGameFeedback {
                shown: true,
                reason: "",
                output: "toast",
                delivered,
                sound,
                sound_played,
            },
        );
        Ok(json!(true))
    }

    fn on_quick_switch(&self) -> CoreResult<String> {
        let active_prov = self.state.server.active_provider();
        let server_cb = self.state.server.clone();
        let cb = move || {
            server_cb.broadcast(json!({"type": "FOCUS_MAP_TAB"}), false, false);
        };
        let target = WindowSwitcher::toggle_game_and_browser_for_provider(
            &crate::state::WINDOW_TITLES,
            Some(active_prov.as_str()),
            Some(&cb),
        );
        Ok(target.to_owned())
    }

    fn on_shutdown(&self) {
        let _ = self.state.stop_tx.send(true);
    }

    fn on_set_hotkeys(&self, payload: &Value) -> CoreResult<Value> {
        self.state.set_hotkeys(payload)
    }

    fn on_set_language(&self, lang: &str) -> CoreResult<Value> {
        self.state.set_language(lang)
    }

    fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool> {
        let _ = self.state.mutate_and_save_config(|cfg| {
            cfg.minimap_tracking = enabled;
        });
        if !enabled {
            self.state.pause_minimap_tracker("disabled");
            self.state.nav_hud.hide();
            self.state.hide_radar();
        }
        Ok(enabled)
    }

    fn on_set_nav_hud(&self, payload: &Value) -> CoreResult<Value> {
        self.state.record_web_input("on_set_nav_hud", payload);
        self.state.set_nav_hud(payload)
    }

    fn on_set_radar(&self, payload: &Value) -> CoreResult<Value> {
        self.state.set_radar(payload)
    }

    fn on_lan_pins(&self, payload: &Value) {
        self.state.on_lan_pins_received(payload);
    }

    fn on_set_audio(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let enabled_in = req.get("audio_enabled").or_else(|| req.get("enabled"));
        let vol_in = req.get("audio_volume").or_else(|| req.get("volume"));
        let test = json_truthy(req.get("test"), false);

        let (_, (audio_enabled, audio_volume), _) = self.state.mutate_and_save_config(|cfg| {
            if let Some(ev) = enabled_in {
                cfg.audio_enabled = json_truthy(Some(ev), cfg.audio_enabled);
            }
            if let Some(fv) = json_f64(vol_in)
                && fv.is_finite()
            {
                cfg.audio_volume = (fv.round() as i64).clamp(0, 100) as i32;
            }
            (cfg.audio_enabled, cfg.audio_volume)
        });

        self.state.audio.set_enabled(audio_enabled);
        self.state.audio.set_volume(audio_volume);
        if test {
            self.state.audio.play("success", true);
        }

        Ok(json!({
            "audio_enabled": audio_enabled,
            "audio_volume": audio_volume,
        }))
    }

    fn on_nearby_pins(&self, payload: &Value) -> CoreResult<usize> {
        self.state.record_web_input("on_nearby_pins", payload);
        let req = payload.as_object().cloned().unwrap_or_default();
        let map_id = json_i64(req.get("map_id")).unwrap_or(2) as i32;
        let raw_pins = req
            .get("pins")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let resolver = self.state.floor_resolver();
        let mut norm_pins: Vec<NavPin> = raw_pins
            .iter()
            .filter_map(|item| normalize_raw_nav_pin(item, map_id, Some(&resolver)))
            .collect();

        for pin in &mut norm_pins {
            if !PinGuideFetcher::is_fetchable_id(&pin.id)
                && (!pin.description.is_empty() || !pin.image_url.is_empty())
            {
                self.state.guide_fetcher.register_inline_guide(
                    &pin.id,
                    &pin.description,
                    &pin.image_url,
                    None,
                );
            }
            if let Some(cached) = self.state.guide_fetcher.get_cached(&pin.id, None) {
                if !cached.description.is_empty() {
                    pin.description = cached.description;
                }
                if !cached.image_url.is_empty() && pin.image_url.is_empty() {
                    pin.image_url = cached.image_url;
                }
            }
        }

        let filter_changed = json_truthy(req.get("filter_changed"), false);
        let coverage = if filter_changed {
            None
        } else {
            match (
                json_f64(req.get("lat")),
                json_f64(req.get("lng")),
                json_f64(req.get("coverage_radius")),
            ) {
                (Some(lat), Some(lng), Some(rad))
                    if lat.is_finite() && lng.is_finite() && rad.is_finite() && rad > 0.0 =>
                {
                    Some((lat, lng, rad))
                }
                _ => None,
            }
        };

        let count = norm_pins.len();
        {
            let mut nav = self
                .state
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            nav.set_pins(norm_pins, filter_changed, coverage);
        }
        self.state.push_lan_snapshot(false);
        Ok(count)
    }

    fn on_nav_command(&self, payload: &Value) -> CoreResult<Value> {
        self.state.record_web_input("on_nav_command", payload);
        let req = payload.as_object().cloned().unwrap_or_default();
        let act_str = json_str_trimmed(req.get("action")).to_ascii_lowercase();
        let pin_id = req
            .get("pin_id")
            .filter(|v| !v.is_null())
            .map(|v| json_str_trimmed(Some(v)))
            .filter(|s| !s.is_empty());

        if matches!(act_str.as_str(), "lock_target" | "lock_target_pin") {
            let pin_val = req
                .get("pin")
                .cloned()
                .unwrap_or_else(|| Value::Object(req.clone()));
            let map_id = json_i64(pin_val.get("map_id")).unwrap_or(2) as i32;
            let resolver = self.state.floor_resolver();
            let Some(mut pin) = normalize_raw_nav_pin(&pin_val, map_id, Some(&resolver)) else {
                return Err(CoreError::InvalidArgument(
                    "Invalid pin payload for lock_target".to_owned(),
                ));
            };
            if let Some(cached) = self.state.guide_fetcher.get_cached(&pin.id, None) {
                if !cached.description.is_empty() && pin.description.is_empty() {
                    pin.description = cached.description;
                }
                if !cached.image_url.is_empty() && pin.image_url.is_empty() {
                    pin.image_url = cached.image_url;
                }
            }
            let (locked_pin, summary) = {
                let mut nav = self
                    .state
                    .nav_controller
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let lp = nav.lock_target_pin(pin);
                let sum = serde_json::to_value(nav.summary()).unwrap_or(Value::Null);
                (lp, sum)
            };
            let disp = locked_pin.display_name();
            self.state.nav_flash(
                "hud.flash.locked_target",
                gh_hud::nav_hud::HudPalette::Info,
                2.8,
                &[("name", &disp)],
            );
            self.state.ensure_guide(Some(&locked_pin));
            self.state.broadcast_target_lock_state();
            self.state.push_lan_snapshot(true);
            let mut switched_to: Option<String> = None;
            if !self.suppress_window_switch && json_truthy(req.get("auto_switch"), false) {
                switched_to = self.on_quick_switch().ok();
            }
            return Ok(json!({
                "ok": true,
                "action": "lock_target",
                "target": locked_pin.to_wire_json(),
                "switched_to": switched_to,
                "summary": summary,
            }));
        }

        if matches!(act_str.as_str(), "unlock_target" | "clear_target_lock") {
            let (cleared, target_val, summary) = {
                let mut nav = self
                    .state
                    .nav_controller
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let cleared = nav.clear_target_lock();
                let tgt = nav.current_target().map(NavPin::to_wire_json);
                let sum = serde_json::to_value(nav.summary()).unwrap_or(Value::Null);
                (cleared, tgt, sum)
            };
            if cleared {
                self.state.nav_flash(
                    "hud.flash.unlocked_target",
                    gh_hud::nav_hud::HudPalette::Info,
                    2.0,
                    &[],
                );
            }
            self.state.broadcast_target_lock_state();
            self.state.push_lan_snapshot(true);
            return Ok(json!({
                "ok": true,
                "action": "unlock_target",
                "cleared": cleared,
                "target": target_val.unwrap_or(Value::Null),
                "summary": summary,
            }));
        }

        if matches!(act_str.as_str(), "select" | "select_target") {
            let pid = pin_id.as_deref().unwrap_or("");
            let (target_val, summary) = {
                let mut nav = self
                    .state
                    .nav_controller
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let tgt = nav.select_target(pid).map(NavPin::to_wire_json);
                let sum = serde_json::to_value(nav.summary()).unwrap_or(Value::Null);
                (tgt, sum)
            };
            self.state.push_lan_snapshot(true);
            return Ok(json!({
                "ok": true,
                "action": act_str,
                "target": target_val.unwrap_or(Value::Null),
                "summary": summary,
            }));
        }

        let Some(action) = NavAction::parse_command_alias(&act_str) else {
            return Err(CoreError::InvalidArgument(format!(
                "Unsupported nav action: {act_str:?}"
            )));
        };

        match action {
            NavAction::Guide => {
                let opened = self.state.nav_hud.toggle_guide();
                let (cur, sum) = {
                    let mut nav = self
                        .state
                        .nav_controller
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    (
                        nav.current_target().cloned(),
                        serde_json::to_value(nav.summary()).unwrap_or(Value::Null),
                    )
                };
                if opened {
                    self.state.ensure_guide(cur.as_ref());
                }
                Ok(json!({
                    "ok": true,
                    "action": "guide",
                    "guide_open": opened,
                    "target": cur.map(|p| p.to_wire_json()).unwrap_or(Value::Null),
                    "summary": sum,
                }))
            }
            NavAction::Layout => {
                let editing = self.state.nav_hud.toggle_edit_mode();
                let (cur, sum) = {
                    let mut nav = self
                        .state
                        .nav_controller
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    (
                        nav.current_target().map(NavPin::to_wire_json),
                        serde_json::to_value(nav.summary()).unwrap_or(Value::Null),
                    )
                };
                Ok(json!({
                    "ok": true,
                    "action": "layout",
                    "edit_mode": editing,
                    "target": cur.unwrap_or(Value::Null),
                    "summary": sum,
                }))
            }
            _ => {
                let last_pos = self
                    .state
                    .game_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_pos
                    .clone();
                let (target_val, summary) = {
                    let mut nav = self
                        .state
                        .nav_controller
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let tgt = match action {
                        NavAction::Next => nav.next_target().map(NavPin::to_wire_json),
                        NavAction::Prev => nav.prev_target().map(NavPin::to_wire_json),
                        NavAction::Skip => nav.skip_current().map(NavPin::to_wire_json),
                        NavAction::Blacklist => nav.blacklist_current().map(NavPin::to_wire_json),
                        NavAction::Mark => {
                            nav.mark_collected(pin_id.as_deref()).map(NavPin::to_wire_json)
                        }
                        NavAction::Undo => nav.undo().map(NavPin::to_wire_json),
                        NavAction::Refresh => {
                            let (plat, plng, pmid, pfid, pug) = if let Some(ref lp) = last_pos {
                                (
                                    json_f64(lp.get("lat")),
                                    json_f64(lp.get("lng")),
                                    json_i64(lp.get("map_id")).map(|i| i as i32),
                                    lp.get("floor_id")
                                        .filter(|v| !v.is_null())
                                        .map(|v| json_str_trimmed(Some(v)))
                                        .filter(|s| !s.is_empty()),
                                    lp.get("is_underground").and_then(Value::as_bool),
                                )
                            } else {
                                (None, None, None, None, None)
                            };
                            nav.refresh_nearest(plat, plng, pmid, pfid.as_deref(), pug)
                                .map(NavPin::to_wire_json)
                        }
                        NavAction::ClearRoute => {
                            nav.clear_route_queue();
                            self.state
                                .route_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .legs
                                .clear();
                            nav.current_target().map(NavPin::to_wire_json)
                        }
                        NavAction::Guide | NavAction::Layout => unreachable!(),
                    };
                    let sum = serde_json::to_value(nav.summary()).unwrap_or(Value::Null);
                    (tgt, sum)
                };
                self.state.push_lan_snapshot(true);
                Ok(json!({
                    "ok": true,
                    "action": action.as_str(),
                    "target": target_val.unwrap_or(Value::Null),
                    "summary": summary,
                }))
            }
        }
    }

    fn on_plan_tsp_route(&self, payload: &Value) -> CoreResult<Value> {
        self.state.record_web_input("on_plan_tsp_route", payload);
        let req = payload.as_object().cloned().unwrap_or_default();
        let map_id = json_i64(req.get("map_id")).unwrap_or(2) as i32;
        let teleport_penalty = json_f64(req.get("teleport_penalty"))
            .filter(|f| f.is_finite())
            .unwrap_or(DEFAULT_TELEPORT_PENALTY);

        let pins_val = req
            .get("pins")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let pin_inputs: Vec<TspPinInput> = if pins_val.is_array() {
            serde_json::from_value(pins_val)?
        } else {
            Vec::new()
        };

        let start_pos: Option<TspCoordInput> = match req.get("start_pos") {
            Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())?),
            _ => {
                let last_pos = self
                    .state
                    .game_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_pos
                    .clone();
                last_pos.and_then(|lp| {
                    match (json_f64(lp.get("lat")), json_f64(lp.get("lng"))) {
                        (Some(lat), Some(lng)) if lat.is_finite() && lng.is_finite() => {
                            Some(TspCoordInput::LatLng { lat, lng })
                        }
                        _ => None,
                    }
                })
            }
        };

        let waypoints: Vec<TspWaypoint> = match req.get("waypoints") {
            Some(v) if !v.is_null() => {
                let raw_wps: Vec<TspWaypointInput> = serde_json::from_value(v.clone())?;
                normalize_waypoints(&raw_wps)
            }
            _ => waypoints_from_multimap(&self.state.maps_db, map_id, true),
        };

        let plan = plan_route(
            &pin_inputs,
            start_pos,
            Some(&waypoints),
            TspPlanOptions {
                teleport_penalty,
                ..Default::default()
            },
        )?;
        let route_val = serde_json::to_value(&plan)?;

        let legs = route_val
            .get("legs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let resolver = self.state.floor_resolver();
        let ordered_nav_pins: Vec<NavPin> = route_val
            .get("ordered_pins")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| normalize_raw_nav_pin(p, map_id, Some(&resolver)))
                    .collect()
            })
            .unwrap_or_default();

        {
            let mut rs = self
                .state
                .route_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            rs.legs = legs;
            rs.last_route = Some(route_val.clone());
        }
        {
            let mut nav = self
                .state
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if ordered_nav_pins.is_empty() {
                nav.clear_route_queue();
            } else {
                nav.set_route_queue(ordered_nav_pins);
            }
        }
        self.state.push_lan_snapshot(true);

        Ok(route_val)
    }

    fn on_route_command(&self, payload: &Value) -> CoreResult<Value> {
        self.state.record_web_input("on_route_command", payload);
        self.state.handle_route_command(payload)
    }

    fn on_lan_companion(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let action = {
            let s = json_str_trimmed(req.get("action")).to_ascii_lowercase();
            if s.is_empty() { "status".to_owned() } else { s }
        };
        if !matches!(
            action.as_str(),
            "status" | "start" | "stop" | "rotate" | "rotate_pin" | "toggle"
        ) {
            return Err(CoreError::InvalidArgument(format!(
                "Unsupported lan_companion action: {action:?}"
            )));
        }

        let lan_port = self
            .state
            .config
            .read()
            .expect("config lock poisoned")
            .lan_companion_port;
        let lan = &self.state.lan_companion;
        let mut mutated = false;

        match action.as_str() {
            "status" => {}
            "start" => {
                if !lan.is_running() {
                    lan.set_port(lan_port);
                    lan.start_in_background()?;
                }
            }
            "stop" => {
                if lan.is_running() {
                    lan.stop_background();
                }
            }
            "rotate" | "rotate_pin" => {
                lan.rotate_credentials();
                mutated = true;
            }
            "toggle" => {
                if lan.is_running() {
                    lan.stop_background();
                } else {
                    lan.set_port(lan_port);
                    lan.start_in_background()?;
                }
            }
            _ => unreachable!(),
        }

        let running_now = lan.is_running();
        if matches!(action.as_str(), "start" | "stop" | "toggle") {
            mutated = true;
            let _ = self.state.mutate_and_save_config(|cfg| {
                cfg.lan_companion_enabled = running_now;
            });
        }
        if running_now && matches!(action.as_str(), "start" | "toggle") {
            self.state.push_lan_snapshot(true);
        }

        let info = lan.get_connection_info();
        if mutated {
            self.state.server.broadcast(
                json!({
                    "type": "LAN_COMPANION_UPDATED",
                    "running": info.is_running,
                    "url": info.url,
                    "pin": info.pin,
                }),
                false,
                false,
            );
        }

        let mut resp_map = match serde_json::to_value(&info) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        resp_map.insert("ok".to_owned(), Value::Bool(true));
        resp_map.insert("running".to_owned(), Value::Bool(info.is_running));
        Ok(Value::Object(resp_map))
    }

    fn on_launch_borderless(&self, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let dry_run = json_truthy(req.get("dry_run"), false);
        let force = json_truthy(req.get("force"), false);
        let width = json_i64(req.get("width")).and_then(|v| u32::try_from(v).ok());
        let height = json_i64(req.get("height")).and_then(|v| u32::try_from(v).ok());
        let exe_path = req
            .get("exe_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if !dry_run
            && !force
            && WindowFinder::get_game_window(&crate::state::WINDOW_TITLES, false).is_some()
        {
            let rect = self
                .state
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            let msg = t("borderless.already_running", &[]);
            self.state.hud_toast.show_toast(
                &t("app.name", &[]),
                &msg,
                ToastKind::Info,
                2800,
                rect,
            );
            return Ok(json!({
                "ok": false,
                "launched": false,
                "already_running": true,
                "message": msg,
            }));
        }

        let res = launch_genshin_borderless(width, height, exe_path, dry_run);
        let msg = if let Some(ref err) = res.error {
            err.clone()
        } else if dry_run {
            "Dry run".to_owned()
        } else {
            t("borderless.launched", &[])
        };

        if !dry_run {
            let rect = self
                .state
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.state.hud_toast.show_toast(
                &t("app.name", &[]),
                &msg,
                if res.ok {
                    ToastKind::Success
                } else {
                    ToastKind::Warning
                },
                3200,
                rect,
            );
        }

        Ok(json!({
            "ok": res.ok,
            "launched": res.ok && !dry_run,
            "dry_run": res.dry_run,
            "exe_path": res.exe_path,
            "cmd": res.cmd,
            "pid": res.pid,
            "error": res.error,
            "message": msg,
        }))
    }

    fn on_cross_map_export(&self, payload: &Value) -> CoreResult<Value> {
        execute_cross_map_export(payload, Some(&self.state.affine_registry), None)
    }

    fn on_app_update(&self, payload: &Value) -> CoreResult<Value> {
        self.state.handle_app_update_command(payload)
    }

    fn on_calibrate_provider(&self, payload: &Value) -> CoreResult<Value> {
        let map_id = payload
            .as_object()
            .and_then(|o| json_i64(o.get("map_id")))
            .unwrap_or(2) as i32;
        let canonical_anchors: Vec<CanonicalAnchor> = self
            .state
            .maps_db
            .maps
            .get(&map_id)
            .map(|m| {
                m.waypoints
                    .iter()
                    .filter(|w| matches!(w.label_id, 2 | 3 | 154))
                    .map(|w| CanonicalAnchor {
                        x: w.x_pos,
                        y: w.y_pos,
                        label_id: w.label_id,
                        area_id: w.area_id,
                    })
                    .collect()
            })
            .unwrap_or_default();
        execute_provider_calibration(payload, &canonical_anchors)
    }

    fn on_active_provider_change(&self, _provider_id: &str) {
        let had_lock = {
            let mut nav = self
                .state
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            nav.clear_for_provider_switch()
        };
        if had_lock {
            self.state.server.broadcast(
                json!({
                    "type": "TARGET_LOCK_UPDATED",
                    "locked_target": Value::Null,
                }),
                false,
                false,
            );
        }
        self.state.push_lan_snapshot(true);
    }
}

pub struct AppConflictAdvisor {
    state: Arc<AppState>,
}

impl AppConflictAdvisor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    fn enrich_request(&self, request: &Value) -> Value {
        let mut obj = request.as_object().cloned().unwrap_or_default();
        let cfg = self.state.config.read().expect("config lock poisoned").clone();

        if !obj.contains_key("lang") {
            obj.insert("lang".to_owned(), Value::String(get_language()));
        }
        if !obj.contains_key("suppress_enabled") {
            obj.insert(
                "suppress_enabled".to_owned(),
                Value::Bool(cfg.hotkey_suppress_conflicts),
            );
        }
        let needs_bindings = obj
            .get("bindings")
            .and_then(Value::as_object)
            .is_none_or(|b| b.is_empty());
        if needs_bindings {
            let mut bindings = Map::new();
            for act in HOTKEY_ACTIONS {
                bindings.insert(act.to_owned(), Value::String(get_hotkey_field(&cfg, act)));
            }
            obj.insert("bindings".to_owned(), Value::Object(bindings));
        }
        Value::Object(obj)
    }
}

impl ConflictAdvisor for AppConflictAdvisor {
    fn check_conflicts(&self, request: &Value) -> CoreResult<Value> {
        let enriched = self.enrich_request(request);
        self.state.conflict_advisor.check_conflicts(&enriched)
    }

    fn remap_external(&self, request: &Value) -> CoreResult<Value> {
        let enriched = self.enrich_request(request);
        let mut res = self.state.conflict_advisor.remap_external(&enriched)?;
        if res.get("ok").and_then(Value::as_bool) == Some(true) {
            let claim_action = json_str_trimmed(request.get("claim_action")).to_ascii_lowercase();
            let claim_hotkey = json_str_trimmed(request.get("claim_hotkey")).to_ascii_lowercase();
            if !claim_action.is_empty() && !claim_hotkey.is_empty() {
                let set_res = self.state.set_hotkeys(&json!({
                    "action": claim_action,
                    "hotkey": claim_hotkey,
                }))?;
                if let Some(entry) = set_res
                    .get("results")
                    .and_then(|r| r.get(&claim_action))
                    .and_then(Value::as_object)
                    && entry.get("ok").and_then(Value::as_bool) == Some(true)
                    && let Some(obj) = res.as_object_mut()
                {
                    obj.insert(
                        "claim_updated".to_owned(),
                        json!({
                            "action": claim_action,
                            "hotkey": entry.get("hotkey").and_then(Value::as_str).unwrap_or(&claim_hotkey),
                            "display_name": entry.get("display_name").and_then(Value::as_str).unwrap_or(""),
                        }),
                    );
                }
            }
        }
        Ok(res)
    }

    fn restore_external(&self, request: &Value) -> CoreResult<Value> {
        self.state.conflict_advisor.restore_external(request)
    }
}

pub fn prefetch_startup_labels_if_online(state: &AppState) {
    let cfg = state.config.read().expect("config lock poisoned").clone();
    if cfg.hoyolab_sync.auto_check_on_startup
        && !cfg.hoyolab_sync.auto_update_mode.eq_ignore_ascii_case("off")
    {
        let lang = get_language();
        state.label_catalog.prefetch(2, Some(&lang));
    }
}

pub fn refresh_map_name_for_sync(map_id: i32, lang: &str, fallback: &str) -> String {
    HoYoLABLabelCatalog::get_map_name(map_id, Some(lang), fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticManager;
    use crate::state::make_test_state;
    use gh_core::frame::{FrameBuf, PixelFormat};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CB_TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (u16, Value) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = TcpStream::connect(addr).unwrap();
        let payload = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw);
        let (head, body_str) = text.split_once("\r\n\r\n").unwrap();
        let status_line = head.lines().next().unwrap();
        let code: u16 = status_line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let val: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
        (code, val)
    }

    #[tokio::test]
    async fn headless_sync_server_with_app_callbacks_handles_core_routes_and_clamps() {
        let seq = CB_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir().join(format!(
            "gh_app_cb_test_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).unwrap();

        let state = make_test_state(&tmp_root);
        let cb = Arc::new(AppServerCallbacks::new(Arc::clone(&state)));
        let adv = Arc::new(AppConflictAdvisor::new(Arc::clone(&state)));
        state.server.set_callbacks(Some(cb));
        state.server.set_conflict_advisor(Some(adv));
        state
            .server
            .set_diagnostic_service(Some(Arc::clone(&state.diagnostics)
                as Arc<dyn gh_core::traits::DiagnosticService>));

        let port = state.server.start().await.unwrap();

        let (code, status) = tokio::task::spawn_blocking(move || {
            http_request(port, "GET", "/status", None)
        })
        .await
        .unwrap();
        assert_eq!(code, 200);
        assert_eq!(status["version"], json!(gh_core::APP_VERSION));
        assert!(status.get("hotkey_sync").is_some());

        // Language switch
        let (code, lang_res) = tokio::task::spawn_blocking(move || {
            http_request(port, "POST", "/set_language", Some(r#"{"language":"vi"}"#))
        })
        .await
        .unwrap();
        assert_eq!(code, 200);
        assert_eq!(lang_res["resolved"], json!("vi"));

        // Nav HUD custom_pos clamped to [0.0, 0.98]
        let (code, hud_res) = tokio::task::spawn_blocking(move || {
            http_request(
                port,
                "POST",
                "/set_nav_hud",
                Some(r#"{"enabled":true,"position":"custom","custom_pos":[1.5,-0.25]}"#),
            )
        })
        .await
        .unwrap();
        assert_eq!(code, 200);
        assert_eq!(hud_res["custom_pos"], json!([0.98, 0.0]));

        // Nearby pins + nav command + TSP route
        let (code, np_res) = tokio::task::spawn_blocking(move || {
            http_request(
                port,
                "POST",
                "/nearby_pins",
                Some(r#"{"map_id":2,"pins":[{"id":"101","lat":100.0,"lng":200.0,"name":"Chest A"},{"id":"102","lat":110.0,"lng":210.0,"name":"Chest B"}]}"#),
            )
        })
        .await
        .unwrap();
        assert_eq!(code, 200);
        assert_eq!(np_res["count"], json!(2));

        let (code, tsp_res) = tokio::task::spawn_blocking(move || {
            http_request(
                port,
                "POST",
                "/plan_tsp_route",
                Some(r#"{"map_id":2,"pins":[{"id":101,"lat":100.0,"lng":200.0},{"id":102,"lat":110.0,"lng":210.0}],"start_pos":{"lat":90.0,"lng":190.0}}"#),
            )
        })
        .await
        .unwrap();
        assert_eq!(code, 200);
        assert_eq!(tsp_res["pin_count"], json!(2));

        // Diagnostic bundle + traversal guard + valid UTF-8 save_diagnostic_note
        let gf = FrameBuf::new_zeroed(64, 64, PixelFormat::Bgr8);
        let bf = FrameBuf::new_zeroed(64, 64, PixelFormat::Bgr8);
        let diag_res = state
            .diagnostics
            .create_bundle_with_frames(Some(&gf), Some(&bf), "test", "", true, None)
            .unwrap();
        let fname = diag_res["folder_name"].as_str().unwrap().to_owned();

        let (c_bad, _) = tokio::task::spawn_blocking(move || {
            http_request(
                port,
                "POST",
                "/save_diagnostic_note",
                Some(r#"{"folder_name":"../bad","note":"x"}"#),
            )
        })
        .await
        .unwrap();
        assert_eq!(c_bad, 400);

        let (c_miss, _) = tokio::task::spawn_blocking(move || {
            http_request(
                port,
                "POST",
                "/save_diagnostic_note",
                Some(r#"{"folder_name":"diag_99991231_235959","note":"x"}"#),
            )
        })
        .await
        .unwrap();
        assert_eq!(c_miss, 404);

        let body_ok = json!({"folder_name": fname, "note": "Lỗi bản đồ tiếng Việt"}).to_string();
        let (c_ok, ok_res) = tokio::task::spawn_blocking(move || {
            http_request(port, "POST", "/save_diagnostic_note", Some(&body_ok))
        })
        .await
        .unwrap();
        assert_eq!(c_ok, 200);
        assert_eq!(ok_res["user_note"], json!("Lỗi bản đồ tiếng Việt"));

        // Shutdown via POST
        let (c_shut, _) = tokio::task::spawn_blocking(move || {
            http_request(port, "POST", "/shutdown", Some("{}"))
        })
        .await
        .unwrap();
        assert_eq!(c_shut, 200);
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        state.server.stop().await;
        state.server.set_callbacks(None);
        state.server.set_conflict_advisor(None);
        state.server.set_diagnostic_service(None);
        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn nav_mark_error_feedback_flashes_action_specific_web_error_and_preserves_unmark() {
        let seq = CB_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir().join(format!(
            "gh_app_cb_nav_fb_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).unwrap();

        let state = make_test_state(&tmp_root);
        let cb = AppServerCallbacks::new(Arc::clone(&state));
        state
            .nav_hud
            .update(Default::default(), None, None, None, None, None, None, 0.0, None, None);

        {
            let mut nav = state.nav_controller.lock().unwrap();
            let pin = NavPin::new("101", "Test Chest", 10.0, 20.0);
            nav.set_pins(vec![pin], true, None);
            nav.mark_collected(Some("101"));
            assert!(nav.collected_ids.contains("101"));
        }

        // Unmark failure must NOT roll back into collected, and must flash web_unmark_failed.
        let handled_unmark = cb
            .on_action_feedback(&json!({
                "event_type": "NAV_MARK",
                "action": "unmark",
                "status": "error",
                "pin_id": "101",
                "detail": ""
            }))
            .unwrap();
        assert_eq!(handled_unmark, json!(true));

        let want_unmark = t("hud.flash.web_unmark_failed", &[]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if state.nav_hud.last_render_meta().subtitle == want_unmark {
                break;
            }
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            state.nav_hud.wait_for_render_meta(rem, |m| m.subtitle == want_unmark);
        }
        let meta_unmark = state.nav_hud.last_render_meta();
        assert_eq!(meta_unmark.subtitle, want_unmark);
        assert_eq!(meta_unmark.palette, gh_hud::nav_hud::HudPalette::Warning);

        // Mark failure rolls back collected pin and flashes web_failed.
        let handled_mark = cb
            .on_action_feedback(&json!({
                "event_type": "NAV_MARK",
                "action": "mark",
                "status": "not_found",
                "pin_id": "101",
                "detail": ""
            }))
            .unwrap();
        assert_eq!(handled_mark, json!(true));
        assert!(
            !state
                .nav_controller
                .lock()
                .unwrap()
                .collected_ids
                .contains("101")
        );

        let want_mark = t("hud.flash.web_failed", &[]);
        let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline2 {
            if state.nav_hud.last_render_meta().subtitle == want_mark {
                break;
            }
            let rem = deadline2.saturating_duration_since(std::time::Instant::now());
            state.nav_hud.wait_for_render_meta(rem, |m| m.subtitle == want_mark);
        }
        let meta_mark = state.nav_hud.last_render_meta();
        assert_eq!(meta_mark.subtitle, want_mark);
        assert_eq!(meta_mark.palette, gh_hud::nav_hud::HudPalette::Warning);

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn m_map_position_sync_is_always_confirmed_in_game_even_with_a_visible_map_tab() {
        // A visible map tab on a second monitor makes native_feedback_wanted() false.
        assert!(app_owns_feedback("SYNC_POSITION_ONLY", "synced", || false));
        assert!(app_owns_feedback("sync_position_only", "SYNCED", || false));
        // Marks and errors keep deferring to a visible page (it beeps itself).
        assert!(!app_owns_feedback("SYNC_AND_MARK", "marked", || false));
        assert!(!app_owns_feedback("SYNC_POSITION_ONLY", "error", || false));
        assert!(!app_owns_feedback("SYNC_AND_MARK", "synced", || false));
        // Nobody can see a map tab: the app answers everything, as before.
        assert!(app_owns_feedback("SYNC_AND_MARK", "marked", || true));
        assert!(app_owns_feedback("SYNC_POSITION_ONLY", "error", || true));
    }

    #[test]
    fn on_action_feedback_records_web_feedback_event_only_when_recorder_active() {
        let seq_id = CB_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir().join(format!(
            "gh_app_cb_fb_ev_{}_{seq_id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).unwrap();

        let state = make_test_state(&tmp_root);
        let cb = AppServerCallbacks::new(Arc::clone(&state));
        let rec = crate::session_recorder::SessionRecorder::new(
            crate::session_recorder::SessionRecorderConfig {
                seconds: 0.0,
                fps: crate::session_recorder::DEFAULT_SESSION_FPS,
                codec: crate::session_recorder::SessionCodec::Libx264Rgb,
                ffmpeg_path: std::env::current_exe().unwrap(),
                out_root: tmp_root.join("out"),
                tiles_dir: tmp_root.join("tiles"),
                ug_overlays_dir: tmp_root.join("ug"),
                paimon_cache_path: tmp_root.join("paimon.npz"),
                data_dir: tmp_root.join("data"),
                data_source: "dev".to_owned(),
                maps_db: Arc::clone(&state.maps_db),
                max_queue: 16,
            },
            None,
        )
        .unwrap();

        // Active recorder receives events for every feedback, including missing seq -> 0
        *state.session_recorder.lock().unwrap() = Some(Arc::clone(&rec));
        let _ = cb
            .on_action_feedback(&json!({
                "event_type": "sync_position_only",
                "status": "Synced",
                "seq": 42
            }))
            .unwrap();
        let _ = cb
            .on_action_feedback(&json!({
                "event_type": "NAV_MARK",
                "status": "marked"
            }))
            .unwrap();

        // Every web feedback is followed by one `hud_feedback` saying what the game showed.
        let evs = rec.take_pending_events_for_test();
        assert_eq!(evs.len(), 4, "events: {evs:?}");
        assert_eq!(evs[0]["kind"], "web_feedback");
        assert_eq!(evs[0]["event_type"], "SYNC_POSITION_ONLY");
        assert_eq!(evs[0]["status"], "synced");
        assert_eq!(evs[0]["seq"], 42);
        assert!(evs[0]["ts"].as_f64().is_some());

        // A confirmed M-Map sync always gets a toast + info chime in game, whatever the tab state.
        assert_eq!(evs[1]["kind"], "hud_feedback");
        assert_eq!(evs[1]["event_type"], "SYNC_POSITION_ONLY");
        assert_eq!(evs[1]["seq"], 42);
        assert_eq!(evs[1]["shown"], true);
        assert_eq!(evs[1]["output"], "toast");
        assert_eq!(evs[1]["sound"], "info");

        assert_eq!(evs[2]["kind"], "web_feedback");
        assert_eq!(evs[2]["event_type"], "NAV_MARK");
        assert_eq!(evs[2]["status"], "marked");
        assert_eq!(evs[2]["seq"], 0);

        // A successful NAV_MARK is the page's to confirm.
        assert_eq!(evs[3]["kind"], "hud_feedback");
        assert_eq!(evs[3]["event_type"], "NAV_MARK");
        assert_eq!(evs[3]["shown"], false);
        assert_eq!(evs[3]["reason"], "page_handles");

        // Finished recorder records nothing
        rec.finish();
        let _ = cb
            .on_action_feedback(&json!({
                "event_type": "SYNC_POSITION_ONLY",
                "status": "synced",
                "seq": 43
            }))
            .unwrap();
        assert!(rec.take_pending_events_for_test().is_empty());

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn web_input_callbacks_record_sanitized_deduplicated_payloads_when_active() {
        let seq_id = CB_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_root = std::env::temp_dir().join(format!(
            "gh_app_cb_web_rec_{}_{seq_id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).unwrap();

        let state = make_test_state(&tmp_root);
        let cb = AppServerCallbacks::new_offline_replay(Arc::clone(&state));
        let rec = crate::session_recorder::SessionRecorder::new(
            crate::session_recorder::SessionRecorderConfig {
                seconds: 0.0,
                fps: crate::session_recorder::DEFAULT_SESSION_FPS,
                codec: crate::session_recorder::SessionCodec::Libx264Rgb,
                ffmpeg_path: std::env::current_exe().unwrap(),
                out_root: tmp_root.join("out"),
                tiles_dir: tmp_root.join("tiles"),
                ug_overlays_dir: tmp_root.join("ug"),
                paimon_cache_path: tmp_root.join("paimon.npz"),
                data_dir: tmp_root.join("data"),
                data_source: "dev".to_owned(),
                maps_db: Arc::clone(&state.maps_db),
                max_queue: 16,
            },
            None,
        )
        .unwrap();
        *state.session_recorder.lock().unwrap() = Some(Arc::clone(&rec));

        // 1. on_nearby_pins (with sensitive key + duplicate)
        let pins_payload = json!({
            "map_id": 2,
            "cookie": "ltoken_v2=secret",
            "pins": [{"id": "101", "lat": 10.0, "lng": 20.0, "name": "Chest A"}]
        });
        assert_eq!(cb.on_nearby_pins(&pins_payload).unwrap(), 1);
        assert_eq!(cb.on_nearby_pins(&pins_payload).unwrap(), 1);

        // 2. on_nav_command (lock_target with auto_switch suppressed in offline replay)
        let _ = cb
            .on_nav_command(&json!({
                "action": "lock_target",
                "pin": {"id": "101", "lat": 10.0, "lng": 20.0, "name": "Chest A"},
                "auto_switch": true,
                "authorization": "Bearer secret"
            }))
            .unwrap();

        // 3. on_route_command
        let _ = cb.on_route_command(&json!({"action": "status"})).unwrap();

        // 4. on_plan_tsp_route
        let _ = cb
            .on_plan_tsp_route(&json!({
                "map_id": 2,
                "pins": [{"id": 101, "lat": 10.0, "lng": 20.0}],
                "start_pos": {"lat": 0.0, "lng": 0.0}
            }))
            .unwrap();

        // 5. on_set_nav_hud
        let _ = cb
            .on_set_nav_hud(&json!({"enabled": true, "position": "under_minimap"}))
            .unwrap();

        let (recorded, skipped, by_cb) = rec.web_input_stats();
        assert_eq!(recorded, 5);
        assert_eq!(skipped, 1);
        assert_eq!(by_cb.get("on_nearby_pins"), Some(&1));

        let items = rec.drain_pending_web_inputs();
        assert_eq!(items.len(), 5);
        let cbs: Vec<&str> = items.iter().map(|i| i.callback.as_str()).collect();
        assert_eq!(
            cbs,
            vec![
                "on_nearby_pins",
                "on_nav_command",
                "on_route_command",
                "on_plan_tsp_route",
                "on_set_nav_hud",
            ]
        );
        assert!(items[0].payload.get("cookie").is_none());
        assert!(items[1].payload.get("authorization").is_none());

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[allow(dead_code)]
    fn _ensure_diag_import(_: &DiagnosticManager) {}
}
