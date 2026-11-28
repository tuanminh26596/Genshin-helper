//! 1.0s mtime hot-reload watcher for `config.json` and 2.0s tray state refresh (`main.py:2449-2590`).

use crate::state::{AppState, get_hotkey_field};
use gh_core::config::AppConfig;
use gh_core::i18n::set_language;
use gh_net::pin_guide::HoYoLABLabelCatalog;
use gh_net::sync_server::HOTKEY_ACTIONS;
use gh_platform::locale::Win32LocaleProvider;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::watch;

pub fn apply_reloaded_config(state: &Arc<AppState>, mut new_cfg: AppConfig) -> Vec<Value> {
    {
        let mut disk = state.disk_ports.lock().unwrap_or_else(|e| e.into_inner());
        disk.0 = new_cfg.ws_port;
        disk.1 = new_cfg.lan_companion_port;
    }
    if let Some(p) = state.cli_port
        && p > 0
    {
        new_cfg.ws_port = p;
    }
    if let Some(lp) = state.cli_lan_port
        && lp > 0
    {
        new_cfg.lan_companion_port = lp;
    }

    let old_cfg = state.config.read().expect("config lock poisoned").clone();
    if old_cfg == new_cfg {
        return Vec::new();
    }

    let mut broadcasts = Vec::new();

    if new_cfg.audio_enabled != old_cfg.audio_enabled
        || new_cfg.audio_volume != old_cfg.audio_volume
    {
        state.audio.set_enabled(new_cfg.audio_enabled);
        state.audio.set_volume(new_cfg.audio_volume);
        broadcasts.push(json!({
            "type": "AUDIO_UPDATED",
            "audio_enabled": new_cfg.audio_enabled,
            "audio_volume": new_cfg.audio_volume,
        }));
    }

    if new_cfg.hotkey_suppress_conflicts != old_cfg.hotkey_suppress_conflicts {
        state
            .hotkey_mgr
            .set_suppress_triggers(new_cfg.hotkey_suppress_conflicts);
    }

    let mut nav_labels_changed = false;
    for act in HOTKEY_ACTIONS {
        let old_hk = get_hotkey_field(&old_cfg, act);
        let new_hk = get_hotkey_field(&new_cfg, act);
        if new_hk != old_hk && !new_hk.is_empty() && state.hotkey_mgr.update_binding(act, &new_hk) {
            if act.starts_with("nav_") {
                nav_labels_changed = true;
            }
            broadcasts.push(json!({
                "type": "HOTKEY_UPDATED",
                "action": act,
                "hotkey": new_hk,
                "display_name": state.hotkey_mgr.get_display_name(act),
            }));
        }
    }
    if nav_labels_changed {
        state.refresh_nav_hud_labels();
    }

    if new_cfg.auto_sync_m != old_cfg.auto_sync_m {
        if !new_cfg.auto_sync_m {
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
        }
        broadcasts.push(json!({
            "type": "AUTO_SYNC_UPDATED",
            "auto_sync_m": new_cfg.auto_sync_m,
        }));
    }

    if new_cfg.language != old_cfg.language {
        let resolved = set_language(Some(&new_cfg.language), Some(&Win32LocaleProvider::new()));
        let offline = !new_cfg.hoyolab_sync.auto_check_on_startup
            || new_cfg
                .hoyolab_sync
                .auto_update_mode
                .eq_ignore_ascii_case("off");
        let cur_map_id = state
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pos
            .as_ref()
            .and_then(|v| v.get("map_id").and_then(Value::as_i64))
            .map(|i| i as i32)
            .unwrap_or(2);
        if !offline {
            state.label_catalog.prefetch(cur_map_id, Some(&resolved));
        }
        let map_name = HoYoLABLabelCatalog::get_map_name(cur_map_id, Some(&resolved), "");
        state
            .server
            .update_last_sync_map_name(cur_map_id, &map_name);
        state.nav_hud.set_language(Some(&resolved));

        let cur_target = {
            let mut nav = state
                .nav_controller
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cur_id = nav.current_target().map(|p| p.id.clone());
            if let Some(ref pid) = cur_id
                && let Some(pin) = nav.get_pin_mut(pid)
            {
                pin.description.clear();
            }
            nav.current_target().cloned()
        };
        if let Some(ref tgt) = cur_target
            && !offline
        {
            state.ensure_guide(Some(tgt));
        }

        if let Some(tray) = state
            .tray
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            tray.set_language(Some(&resolved));
        }
        state.refresh_tray_state();
        broadcasts.push(json!({
            "type": "LANGUAGE_UPDATED",
            "language": new_cfg.language,
            "resolved": resolved,
        }));
    }

    if new_cfg.minimap_tracking != old_cfg.minimap_tracking {
        if !new_cfg.minimap_tracking {
            state.pause_minimap_tracker("disabled");
            state.nav_hud.hide();
            state.hide_radar();
        }
        broadcasts.push(json!({
            "type": "MINIMAP_TRACKING_UPDATED",
            "enabled": new_cfg.minimap_tracking,
            "minimap_tracking": new_cfg.minimap_tracking,
        }));
    }

    let radar_changed = new_cfg.radar_enabled != old_cfg.radar_enabled
        || new_cfg.radar_rect != old_cfg.radar_rect
        || (new_cfg.radar_zoom - old_cfg.radar_zoom).abs() > 1e-6
        || (new_cfg.radar_opacity - old_cfg.radar_opacity).abs() > 1e-6;
    if radar_changed {
        state.apply_radar_enabled(new_cfg.radar_enabled);
        if new_cfg.radar_enabled
            && let Ok(guard) = state.radar_map.lock()
            && let Some(ref ctrl) = *guard
        {
            ctrl.set_layout_params(
                new_cfg.radar_rect,
                new_cfg.radar_zoom,
                new_cfg.radar_opacity,
            );
        }
        broadcasts.push(json!({
            "type": "RADAR_UPDATED",
            "enabled": new_cfg.radar_enabled,
            "radar_enabled": new_cfg.radar_enabled,
            "radar_rect": new_cfg.radar_rect,
            "radar_zoom": new_cfg.radar_zoom,
            "radar_opacity": new_cfg.radar_opacity,
        }));
    }

    let hud_changed = new_cfg.nav_hud_enabled != old_cfg.nav_hud_enabled
        || new_cfg.nav_hud_position != old_cfg.nav_hud_position
        || new_cfg.nav_hud_custom_pos != old_cfg.nav_hud_custom_pos
        || new_cfg.nav_hud_capture_visible != old_cfg.nav_hud_capture_visible
        || new_cfg.nav_hud_guide_zoom_mode != old_cfg.nav_hud_guide_zoom_mode
        || new_cfg.mmap_overlay_mode != old_cfg.mmap_overlay_mode;
    if hud_changed {
        if new_cfg.nav_hud_custom_pos != old_cfg.nav_hud_custom_pos {
            state
                .nav_hud
                .set_custom_position(new_cfg.nav_hud_custom_pos);
        }
        if new_cfg.nav_hud_position != old_cfg.nav_hud_position {
            state
                .nav_hud
                .set_position_mode(&new_cfg.nav_hud_position, new_cfg.nav_hud_custom_pos);
        }
        if new_cfg.nav_hud_capture_visible != old_cfg.nav_hud_capture_visible {
            let vis = new_cfg.nav_hud_capture_visible;
            state.nav_hud.set_capture_visible(vis);
            if state.is_radar_active()
                && let Ok(guard) = state.radar_map.lock()
                && let Some(ref ctrl) = *guard
            {
                ctrl.set_capture_visible(vis);
            }
            if !vis {
                state.mmap_overlay.set_capture_visible(false);
                state.capturer.release_window_capture();
            }
        }
        if new_cfg.nav_hud_guide_zoom_mode != old_cfg.nav_hud_guide_zoom_mode {
            state
                .nav_hud
                .set_guide_zoom_mode(&new_cfg.nav_hud_guide_zoom_mode);
        }
        if !new_cfg.nav_hud_enabled {
            state.nav_hud.hide();
        }
        if new_cfg.mmap_overlay_mode != old_cfg.mmap_overlay_mode
            && new_cfg.mmap_overlay_mode.eq_ignore_ascii_case("off")
        {
            state.mmap_overlay.hide();
        }
        broadcasts.push(json!({
            "type": "NAV_HUD_UPDATED",
            "enabled": new_cfg.nav_hud_enabled,
            "position": new_cfg.nav_hud_position,
            "custom_pos": new_cfg.nav_hud_custom_pos,
            "nav_hud_enabled": new_cfg.nav_hud_enabled,
            "nav_hud_position": new_cfg.nav_hud_position,
            "capture_visible": new_cfg.nav_hud_capture_visible,
            "nav_hud_capture_visible": new_cfg.nav_hud_capture_visible,
            "guide_zoom_mode": new_cfg.nav_hud_guide_zoom_mode,
            "nav_hud_guide_zoom_mode": new_cfg.nav_hud_guide_zoom_mode,
            "mmap_overlay_mode": new_cfg.mmap_overlay_mode,
        }));
    }

    *state.config.write().expect("config lock poisoned") = new_cfg.clone();
    state.server.update_config(new_cfg);

    for msg in &broadcasts {
        state.server.broadcast(msg.clone(), false, false);
    }

    broadcasts
}

pub fn check_config_file_once(
    state: &Arc<AppState>,
    last_mtime: &mut Option<(SystemTime, u64)>,
) -> bool {
    let _io_guard = state
        .config_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(stamp) = crate::state::read_config_file_stamp(&state.config_path) else {
        return false;
    };
    if *last_mtime == Some(stamp) {
        return false;
    }
    *last_mtime = Some(stamp);
    if *state
        .last_self_saved_mtime
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        == Some(stamp)
    {
        return false;
    }

    match AppConfig::load_from_file(&state.config_path) {
        Ok(new_cfg) => {
            !apply_reloaded_config(state, new_cfg).is_empty()
        }
        Err(e) => {
            tracing::debug!("Config hot-reload parse skipped (retaining previous config): {e}");
            false
        }
    }
}

pub async fn run_config_watcher(
    state: Arc<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut last_mtime = crate::state::read_config_file_stamp(&state.config_path);
    let mut tick_count: u64 = 0;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
        if *shutdown_rx.borrow() {
            break;
        }
        tick_count = tick_count.wrapping_add(1);
        if tick_count.is_multiple_of(2) {
            state.refresh_tray_state();
        }
        let st_clone = Arc::clone(&state);
        if let Ok(next_mt) = tokio::task::spawn_blocking(move || {
            let mut mt = last_mtime;
            check_config_file_once(&st_clone, &mut mt);
            mt
        })
        .await
        {
            last_mtime = next_mt;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{make_test_state, make_test_state_with_cli_ports};
    use gh_core::nav_controller::NavPin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CW_TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn config_watcher_reloads_valid_changes_and_tolerates_malformed_json() {
        let seq = CW_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_cw_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);
        let mut last_mtime;

        // Write corrupt JSON -> retains previous config without panicking
        std::fs::write(&state.config_path, "{broken json").unwrap();
        last_mtime = None;
        assert!(!check_config_file_once(&state, &mut last_mtime));
        assert_eq!(state.config.read().unwrap().audio_volume, 50);

        // Write updated valid config -> applies delta
        let mut updated = state.config.read().unwrap().clone();
        updated.audio_volume = 80;
        updated.language = "vi".to_owned();
        updated.minimap_tracking = false;
        updated.save_to_file(&state.config_path).unwrap();
        last_mtime = None;
        assert!(check_config_file_once(&state, &mut last_mtime));
        assert_eq!(state.config.read().unwrap().audio_volume, 80);
        assert_eq!(state.config.read().unwrap().language, "vi");
        assert!(!state.config.read().unwrap().minimap_tracking);

        // Start watcher & stop cleanly via watch channel
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(run_config_watcher(Arc::clone(&state), rx));
        let _ = tx.send(true);
        handle.await.unwrap();

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cli_port_overrides_do_not_leak_on_save_and_survive_hot_reload() {
        let seq = CW_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_cw_ports_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state_with_cli_ports(&root, Some(19765), Some(19766));
        assert_eq!(state.config.read().unwrap().ws_port, 19765);
        assert_eq!(state.config.read().unwrap().lan_companion_port, 19766);

        // Saving via AppState::set_language must preserve disk ports on disk while keeping CLI overrides in memory.
        state.set_language("vi").unwrap();
        let disk_after_save = AppConfig::load_from_file(&state.config_path).unwrap();
        assert_eq!(disk_after_save.language, "vi");
        assert_eq!(disk_after_save.ws_port, gh_core::DEFAULT_SYNC_PORT);
        assert_eq!(disk_after_save.lan_companion_port, gh_core::DEFAULT_LAN_PORT);
        assert_eq!(state.config.read().unwrap().ws_port, 19765);
        assert_eq!(state.config.read().unwrap().lan_companion_port, 19766);

        // Self-saved mtime must suppress watcher reload even when last_mtime starts behind.
        let mut last_mtime = None;
        let seq_before = state.server.latest_seq();
        assert!(!check_config_file_once(&state, &mut last_mtime));
        assert_eq!(state.server.latest_seq(), seq_before);

        // Seed a navigation target with cached description to verify external language reload clears it.
        {
            let mut nav = state.nav_controller.lock().unwrap();
            let mut pin = NavPin::new("pin_1", "Target", 100.0, 200.0);
            pin.map_id = 2;
            pin.description = "Old description".to_owned();
            nav.set_pins(vec![pin], true, None);
            assert!(nav.current_target().is_some());
        }

        // Simulate external disk edit changing disk ports, audio_volume, and language.
        std::thread::sleep(Duration::from_millis(25));
        let mut ext_cfg = disk_after_save.clone();
        ext_cfg.ws_port = 18888;
        ext_cfg.lan_companion_port = 18889;
        ext_cfg.audio_volume = 88;
        ext_cfg.language = "en".to_owned();
        ext_cfg.save_to_file(&state.config_path).unwrap();

        assert!(check_config_file_once(&state, &mut last_mtime));
        {
            let cur = state.config.read().unwrap();
            assert_eq!(cur.audio_volume, 88);
            assert_eq!(cur.language, "en");
            // CLI overrides remain enforced in runtime config
            assert_eq!(cur.ws_port, 19765);
            assert_eq!(cur.lan_companion_port, 19766);
        }
        assert_eq!(*state.disk_ports.lock().unwrap(), (18888, 18889));
        {
            let mut nav = state.nav_controller.lock().unwrap();
            assert_eq!(nav.current_target().unwrap().description, "");
        }

        // Subsequent save_config preserves the updated disk ports (18888/18889), not CLI overrides.
        state.set_language("vi").unwrap();
        let disk_after_second_save = AppConfig::load_from_file(&state.config_path).unwrap();
        assert_eq!(disk_after_second_save.ws_port, 18888);
        assert_eq!(disk_after_second_save.lan_companion_port, 18889);
        assert_eq!(disk_after_second_save.language, "vi");

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn config_watcher_detects_same_mtime_different_length_external_edit() {
        let seq = CW_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_cw_stamp_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let state = make_test_state(&root);
        state.set_language("vi").unwrap();
        let saved_stamp = (*state.last_self_saved_mtime.lock().unwrap()).unwrap();
        let mut last_mtime = Some(saved_stamp);

        // External edit with a different file length ("vi" -> "auto") pinned to the exact same mtime.
        let mut ext_cfg = AppConfig::load_from_file(&state.config_path).unwrap();
        ext_cfg.language = "auto".to_owned();
        ext_cfg.save_to_file(&state.config_path).unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&state.config_path)
            .unwrap();
        f.set_modified(saved_stamp.0).unwrap();
        drop(f);

        let new_stamp = crate::state::read_config_file_stamp(&state.config_path).unwrap();
        assert_eq!(new_stamp.0, saved_stamp.0);
        assert_ne!(new_stamp.1, saved_stamp.1);

        assert!(check_config_file_once(&state, &mut last_mtime));
        assert_eq!(state.config.read().unwrap().language, "auto");

        state.nav_hud.close();
        state.hud_toast.close();
        state.guide_fetcher.stop();
        state.audio.close();
        state.capturer.close();
        let _ = std::fs::remove_dir_all(&root);
    }
}
