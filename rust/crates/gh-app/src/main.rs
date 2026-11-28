//! Main desktop GUI binary (`GenshinHelper.exe`) wiring `gh-core`, `gh-win32`,
//! `gh-net`, `gh-cv`, and `gh-hud` (`main.py`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use gh_app::callbacks::{
    AppConflictAdvisor, AppServerCallbacks, prefetch_startup_labels_if_online,
};
use gh_app::capture_backend::FrameCapturer;
use gh_app::config_watcher::run_config_watcher;
use gh_app::diagnostics::{self, DiagnosticManager};
use gh_app::log_buffer::LogRingBuffer;
use gh_app::minimap_recorder::MinimapRecorder;
use gh_app::mmap_view;
use gh_app::session_recorder::{
    DEFAULT_SESSION_FPS, SessionCodec, SessionRecorder, SessionRecorderConfig,
    validate_ffmpeg_path,
};
use gh_app::state::{
    AppState, GameState, NavMarkConfirm, RouteState, WINDOW_TITLES, get_hotkey_field,
};
use gh_app::tracking_loop::run_tracking_loop;
use gh_app::tracking_stack::{TrackingStackOptions, build_tracking_stack};
use gh_app::updater::AppUpdater;
use gh_core::assets::{EnsureDataOptions, REQUIRED_ASSETS, ensure_data_ready};
use gh_core::config::AppConfig;
use gh_core::i18n::{get_language, set_language, t};
use gh_core::map_bridge::PerMapAffineRegistry;
use gh_core::nav_controller::NavigationController;
use gh_core::paths::{AppPaths, RuntimeMode};
use gh_core::traits::{ConflictAdvisor, DiagnosticService, OcrProvider, ServerCallbacks};
use gh_core::{APP_VERSION, CoreResult, DEFAULT_SYNC_HOST};
use gh_hud::mmap_overlay::MMapOverlayController;
use gh_hud::nav_hud::{GuideZoomMode, HudPalette, NavHudConfig, NavMiniHud, PositionMode};
use gh_hud::toast::HudToastOverlay;
use gh_net::hoyolab_sync::{
    HoyolabSyncClient, StartupUpdateOutcome, check_and_apply_startup_update,
};
use gh_net::lan_companion::{LanCompanionConfig, LanCompanionServer};
use gh_net::pin_guide::{HoYoLABLabelCatalog, LabelCatalogConfig, PinGuideFetcher, PinIconCache};
use gh_net::sync_server::{HOTKEY_ACTIONS, SyncServer, SyncServerConfig};
use gh_platform::audio::AudioFeedback;
use gh_platform::capture::{DIB_POOL_SLOTS, ScreenCapturer, WindowFinder};
use gh_platform::hotkey::UnifiedHotkeyManager;
use gh_platform::hotkey_conflicts::HotkeyConflictAdvisor;
use gh_platform::locale::Win32LocaleProvider;
use gh_platform::runtime::SingleInstanceMutex;
use gh_platform::tray::{SystemTrayApp, TrayCallbacks};
use gh_platform::win_ocr::WindowsOcrService;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
#[cfg(windows)]
use windows::Win32::UI::Shell::ShellExecuteW;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
#[cfg(windows)]
use windows::core::{PCWSTR, w};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "GenshinHelper",
    version = APP_VERSION,
    about = "Genshin Impact Exploration & HoYoLAB Auto-Sync Tool"
)]
pub struct CliArgs {
    /// Disable Windows System Tray icon
    #[arg(long)]
    pub no_tray: bool,

    /// Start in Standby mode (0% CPU until awakened)
    #[arg(long)]
    pub standby: bool,

    /// Start silently in background tray
    #[arg(long)]
    pub minimized: bool,

    /// Sync/hydrate HoYoLAB map datasets into the data directory and exit
    #[arg(long)]
    pub sync_data: bool,

    /// Losslessly record SECONDS of in-game minimap/HUD frames (30 fps)
    #[arg(long, value_name = "SECONDS", default_value_t = 0.0)]
    pub record_minimap: f64,

    /// Dev-only hidden lossless full-frame gameplay session recorder (requires explicit --ffmpeg).
    /// Pass without a number (`--record-session`) for unlimited manual-stop recording, or with
    /// SECONDS (`--record-session 90`) to auto-stop after real elapsed seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        num_args = 0..=1,
        default_missing_value = "0",
        hide = true
    )]
    pub record_session: Option<f64>,

    /// Target FPS for --record-session (default 20 fps matching minimap full-step cadence)
    #[arg(long, value_name = "FPS", default_value_t = DEFAULT_SESSION_FPS, hide = true)]
    pub record_fps: f64,

    /// Lossless RGB codec for --record-session: libx264rgb (default) or ffv1
    #[arg(long, value_name = "CODEC", default_value = "libx264rgb", hide = true)]
    pub record_codec: String,

    /// Explicit absolute path to ffmpeg.exe for --record-session (never resolved via PATH)
    #[arg(long, value_name = "ABS_PATH", hide = true)]
    pub ffmpeg: Option<PathBuf>,

    /// Dev-only toggle hotkey to start/stop --record-session (default alt+f8, off NVIDIA Alt+F9)
    #[arg(long, value_name = "COMBO", default_value = "alt+f8", hide = true)]
    pub record_hotkey: String,

    /// Override Sync Server bind port (`AppConfig.ws_port`)
    #[arg(long)]
    pub port: Option<u16>,

    /// Override LAN Companion port (`AppConfig.lan_companion_port`)
    #[arg(long)]
    pub lan_port: Option<u16>,

    /// Override explicit `config.json` path
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Skip acquiring the Win32 single-instance mutex
    #[arg(long)]
    pub no_mutex: bool,

    /// Disable automatic UAC elevation prompt on startup
    #[arg(long)]
    pub no_elevate: bool,
}

fn env_flag_truthy(val: Option<&str>) -> bool {
    val.map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn should_auto_elevate(args: &CliArgs, env_no_elevate: bool, is_elevated: bool) -> bool {
    !is_elevated && !args.no_elevate && !args.no_tray && !args.sync_data && !env_no_elevate
}

fn open_shell_target(target: &str) {
    #[cfg(windows)]
    {
        let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated for the duration of `ShellExecuteW`.
        unsafe {
            let _ = ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }
}

fn run_sync_data_mode(paths: &AppPaths, cfg: &AppConfig) -> CoreResult<()> {
    let target_dir = if paths.mode == RuntimeMode::Dev
        && let Some(ref dev_dir) = paths.dev_data_dir
    {
        dev_dir.clone()
    } else {
        paths.data_dir.clone()
    };
    let client = HoyolabSyncClient::new();
    let out = client.sync_to_directory_with_cfg(&target_dir, &cfg.hoyolab_sync, None, None)?;
    tracing::info!("[--sync-data] Hydrated map datasets into {}", out.display());
    Ok(())
}

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() {
    let log_buffer = LogRingBuffer::default();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(log_buffer.clone())
        .try_init();

    let args = CliArgs::parse();

    let session_rec_spec = if let Some(raw_secs) = args.record_session {
        let ffmpeg_abs = match validate_ffmpeg_path(args.ffmpeg.as_deref()) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!("[--record-session] {err}");
                std::process::exit(2);
            }
        };
        let codec = match SessionCodec::parse(&args.record_codec) {
            Ok(c) => c,
            Err(err) => {
                tracing::error!("[--record-session] {err}");
                std::process::exit(2);
            }
        };
        Some((raw_secs.max(0.0), ffmpeg_abs, codec))
    } else {
        None
    };

    let env_no_elevate = env_flag_truthy(std::env::var("GH_NO_ELEVATE").ok().as_deref());
    if should_auto_elevate(
        &args,
        env_no_elevate,
        gh_platform::game_settings::is_process_elevated(),
    ) && gh_platform::game_settings::relaunch_current_exe_elevated(&["--no-elevate"])
    {
        return;
    }

    let paths = AppPaths::resolve();
    let config_path = args.config.clone().unwrap_or_else(|| paths.config_path.clone());
    let mut cfg = AppConfig::load_or_create(&config_path);
    let disk_ports = (cfg.ws_port, cfg.lan_companion_port);
    let cli_port = args.port.filter(|&p| p > 0);
    let cli_lan_port = args.lan_port.filter(|&lp| lp > 0);
    if let Some(p) = cli_port {
        cfg.ws_port = p;
    }
    if let Some(lp) = cli_lan_port {
        cfg.lan_companion_port = lp;
    }

    set_language(Some(&cfg.language), Some(&Win32LocaleProvider::new()));

    if args.sync_data {
        if let Err(err) = run_sync_data_mode(&paths, &cfg) {
            tracing::error!("[--sync-data] Failed: {err}");
            std::process::exit(1);
        }
        return;
    }

    let _mutex_guard = if args.no_mutex {
        None
    } else {
        match SingleInstanceMutex::acquire_for_port(cfg.ws_port) {
            Ok(Some(g)) => Some(g),
            Ok(None) => {
                tracing::info!("Another GenshinHelper instance is already running. Exiting.");
                return;
            }
            Err(err) => {
                tracing::warn!("SingleInstanceMutex warning: {err}; continuing anyway.");
                None
            }
        }
    };

    let sync_client = HoyolabSyncClient::new();
    let offline_mode = !cfg.hoyolab_sync.auto_check_on_startup
        || cfg
            .hoyolab_sync
            .auto_update_mode
            .eq_ignore_ascii_case("off");
    let ensure_opts = EnsureDataOptions {
        cache_dir: Some(&paths.data_dir),
        fallback_bundle_dir: paths.dev_data_dir.as_deref(),
        required_assets: &REQUIRED_ASSETS,
        auto_sync: !offline_mode,
        sync_cfg: &cfg.hoyolab_sync,
    };
    match ensure_data_ready(&ensure_opts, Some(&sync_client)) {
        Ok(dir) => tracing::info!("[Startup] Map data ready: {}", dir.display()),
        Err(err) => {
            tracing::warn!("[Startup] Map data hydration skipped/incomplete ({err}); using available assets.");
        }
    }

    let capturer: Arc<dyn FrameCapturer> = Arc::new(ScreenCapturer::new(DIB_POOL_SLOTS));
    let ocr: Option<Arc<dyn OcrProvider>> = WindowsOcrService::try_new()
        .ok()
        .map(|s| Arc::new(s) as Arc<dyn OcrProvider>);
    let stack = build_tracking_stack(TrackingStackOptions::for_live(&paths, offline_mode, ocr))
        .expect("build_tracking_stack init");
    let locator = stack.locator;
    let maps_db = stack.maps_db;
    let hd_provider = stack.hd_provider;
    let ug_provider = stack.ug_provider;
    let minimap_tracker = stack.minimap_tracker;
    let state_machine = stack.state_machine;
    let loaded_data_dir = stack.data_dir;
    let loaded_data_source = stack.data_source;

    let lan_bind_host = gh_net::lan_companion::resolve_lan_bind_host(
        std::env::var("GH_LAN_BIND_HOST").ok().as_deref(),
    );
    let lan_companion = Arc::new(LanCompanionServer::new(LanCompanionConfig {
        host: lan_bind_host,
        port: cfg.lan_companion_port,
        ..Default::default()
    }));
    let server = SyncServer::new(SyncServerConfig {
        host: DEFAULT_SYNC_HOST.to_owned(),
        port: cfg.ws_port,
        config: cfg.clone(),
        config_path: Some(config_path.clone()),
        diagnostics_dir: Some(paths.diagnostics_dir.clone()),
        lan_companion: Some(Arc::clone(&lan_companion)),
        ..Default::default()
    });
    server.set_engine_active(!args.standby);

    let config_arc = Arc::new(RwLock::new(cfg.clone()));
    let audio = Arc::new(AudioFeedback::new(cfg.audio_enabled, cfg.audio_volume));
    let hud_toast = Arc::new(HudToastOverlay::new());
    if gh_hud::layered::is_hud_offscreen_env() {
        tracing::info!(
            "[Startup] HUD offscreen mode active (offset={:?})",
            gh_hud::layered::OFFSCREEN_TEST_OFFSET
        );
    }
    if gh_platform::runtime::is_desktop_sandbox_env() {
        tracing::info!(
            "[Startup] Desktop sandbox mode active (no window focus changes, no global input hooks)"
        );
    }
    let guide_fetcher = PinGuideFetcher::new(Arc::new(get_language));

    let label_hud_holder: Arc<Mutex<Option<Arc<NavMiniHud>>>> = Arc::new(Mutex::new(None));
    let label_hud_ref = Arc::clone(&label_hud_holder);
    let label_catalog = HoYoLABLabelCatalog::with_config(LabelCatalogConfig {
        on_loaded: Some(Arc::new(move |_, _| {
            if let Some(ref hud) = *label_hud_ref.lock().unwrap_or_else(|e| e.into_inner()) {
                hud.invalidate_render();
            }
        })),
        ..Default::default()
    });

    // Create NavMiniHud and UnifiedHotkeyManager before wrapping in AppState
    let state_slot: Arc<Mutex<Option<Arc<AppState>>>> = Arc::new(Mutex::new(None));

    let state_slot_suppress = Arc::clone(&state_slot);
    let hotkey_mgr = Arc::new(UnifiedHotkeyManager::new(
        cfg.hotkey_suppress_conflicts,
        Some(Arc::new(move |_act: &str| {
            let Some(st) = state_slot_suppress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            else {
                return false;
            };
            if !st
                .config
                .read()
                .expect("config lock poisoned")
                .hotkey_suppress_conflicts
            {
                return false;
            }
            let hwnd = st
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .hwnd;
            if let Some(hw) = hwnd
                && WindowFinder::is_window_foreground(hw)
            {
                return true;
            }
            if let Some(hud_hw) = st.nav_hud.hwnd()
                && WindowFinder::is_window_foreground(hud_hw)
            {
                return true;
            }
            false
        })),
    ));

    let state_slot_pos = Arc::clone(&state_slot);
    let catalog_for_hud = label_catalog.clone();
    let nav_hud = Arc::new(
        NavMiniHud::new(NavHudConfig {
            position_mode: PositionMode::from_str_lossy(&cfg.nav_hud_position),
            custom_pos: cfg.nav_hud_custom_pos,
            gamepad_mode: false,
            lang: Some(get_language()),
            hotkey_label: hotkey_mgr.get_display_name("nav_mark"),
            guide_hotkey_label: hotkey_mgr.get_display_name("nav_guide"),
            layout_hotkey_label: hotkey_mgr.get_display_name("nav_layout"),
            capture_visible: cfg.nav_hud_capture_visible && session_rec_spec.is_none(),
            guide_zoom_mode: GuideZoomMode::from_str_lossy(&cfg.nav_hud_guide_zoom_mode),
            screen_offset: (0, 0),
        })
        .with_position_callback(Arc::new(move |mode, pos| {
            if let Some(st) = state_slot_pos
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                st.on_hud_position_changed(mode, pos);
            }
        }))
        .with_label_resolver(Arc::new(move |mid, lid, lang| {
            catalog_for_hud.get_label(mid, lid, Some(lang))
        })),
    );
    *label_hud_holder.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&nav_hud));

    let srv_for_diag = server.clone();
    let nav_snap_reader_slot: Arc<Mutex<Option<Arc<AppState>>>> = Arc::clone(&state_slot);
    let game_rect_reader_slot: Arc<Mutex<Option<Arc<AppState>>>> = Arc::clone(&state_slot);
    let env_health_reader_slot: Arc<Mutex<Option<Arc<AppState>>>> = Arc::clone(&state_slot);
    let minimap_ring = diagnostics::MinimapPreLostRingBuffer::default();
    let diagnostics = Arc::new(
        DiagnosticManager::new(
            paths.clone(),
            Arc::clone(&capturer),
            Some(Arc::clone(&locator)),
            Arc::clone(&config_arc),
            Arc::new(move || srv_for_diag.last_browser_state()),
            Arc::new(move || {
                nav_snap_reader_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|st| {
                        st.nav_state_snapshot
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                    })
            }),
            Arc::new(move || {
                game_rect_reader_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|st| st.game_state.lock().unwrap_or_else(|e| e.into_inner()).rect)
            }),
        )
        .with_log_buffer(log_buffer)
        .with_minimap_ring(minimap_ring.clone())
        .with_env_health_reader(Arc::new(move || {
            env_health_reader_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|st| st.build_diag_env_health())
                .unwrap_or(serde_json::Value::Null)
        })),
    );

    let (stop_tx, shutdown_rx) = watch::channel(false);

    let mmap_view = Arc::new(mmap_view::MMapViewEngine::new(Arc::new(
        mmap_view::LocatorRegistrar::new(Arc::clone(&locator)),
    )));
    let state = Arc::new(AppState {
        paths: paths.clone(),
        config_path: config_path.clone(),
        config: config_arc,
        cli_port,
        cli_lan_port,
        disk_ports: Mutex::new(disk_ports),
        config_io_lock: Mutex::new(()),
        last_self_saved_mtime: Mutex::new(None),
        engine_active: AtomicBool::new(!args.standby),
        anchor_seq: AtomicU64::new(0),
        game_state: Mutex::new(GameState::default()),
        nav_state_snapshot: Mutex::new(None),
        nav_controller: Mutex::new(NavigationController::new().with_space_debounce_frames(4)),
        route_state: Mutex::new(RouteState::default()),
        nav_mark_confirm: Mutex::new(NavMarkConfirm::default()),
        tracker: Mutex::new(Some((minimap_tracker, 0.0))),
        locator: Some(locator),
        state_machine: Mutex::new(state_machine),
        capturer,
        server: server.clone(),
        lan_companion: Arc::clone(&lan_companion),
        nav_hud: Arc::clone(&nav_hud),
        mmap_overlay: Arc::new(MMapOverlayController::new()),
        mmap_view: Some(mmap_view),
        radar_map: Mutex::new(None),
        radar_active: AtomicBool::new(false),
        radar_pins: Mutex::new(None),
        radar_screen_offset: (0, 0),
        pin_icon_cache: PinIconCache::new(
            paths.cache_dir.join("lan_icons"),
            Arc::new(label_catalog.clone()),
            Duration::from_secs(6),
        ),
        hud_toast: Arc::clone(&hud_toast),
        audio: Arc::clone(&audio),
        tray: Mutex::new(None),
        hotkey_mgr: Arc::clone(&hotkey_mgr),
        conflict_advisor: Arc::new(HotkeyConflictAdvisor::new()),
        guide_fetcher,
        label_catalog,
        maps_db: Arc::clone(&maps_db),
        affine_registry: PerMapAffineRegistry::new(),
        hd_provider,
        ug_provider,
        minimap_recorder: Mutex::new(None),
        session_recorder: Mutex::new(None),
        minimap_ring,
        diagnostics: Arc::clone(&diagnostics),
        updater: Arc::new(AppUpdater::new_from_env()),
        stop_tx: stop_tx.clone(),
        start_instant: Instant::now(),
    });
    *state_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&state));

    lan_companion.set_map_assets(gh_net::lan_companion::LanMapAssets {
        maps_db: Arc::clone(&state.maps_db),
        tile_cache_dir: paths.tiles_dir.clone(),
        ug_provider: Arc::clone(&state.ug_provider),
        guide_fetcher: state.guide_fetcher.clone(),
        icon_cache: state.pin_icon_cache.clone(),
    });
    if cfg.radar_enabled {
        state.apply_radar_enabled(true);
    }
    let state_for_lan = Arc::clone(&state);
    lan_companion.set_on_client_message(Some(Arc::new(move |msg| {
        state_for_lan.handle_lan_client_message(msg);
    })));

    // Register all 9 hotkeys
    let active_init = !args.standby;
    for act in HOTKEY_ACTIONS {
        let st = Arc::clone(&state);
        let hk = get_hotkey_field(&cfg, act);
        let enabled = active_init;
        let act_owned = act.to_owned();
        hotkey_mgr.register(
            act,
            &hk,
            Arc::new(move || {
                if let Some(srec) = st
                    .session_recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    && srec.active()
                {
                    srec.record_hotkey_event(st.start_instant.elapsed().as_secs_f64(), &act_owned);
                }
                gh_app::dispatch_hotkey_action(&st, &act_owned);
            }),
            enabled,
            None,
        );
    }
    hotkey_mgr.start();
    state.refresh_nav_hud_labels();

    // Optional MinimapRecorder (--record-minimap)
    if args.record_minimap > 0.0 {
        let hud_for_rec = Arc::clone(&nav_hud);
        let rec = MinimapRecorder::new(
            args.record_minimap,
            &paths.minimap_rec_dir(),
            30.0,
            180,
            Some(Box::new(move |_out_dir, frames| {
                hud_for_rec.flash(
                    &format!("REC OK - {frames} frames"),
                    HudPalette::Success,
                    6.0,
                );
            })),
        );
        *state
            .minimap_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(rec);
    }

    // Optional hidden SessionRecorder (--record-session [SECONDS] --ffmpeg <ABS_PATH>)
    // Supports Alt+F8 (or --record-hotkey) toggle to start/stop multiple takes without restarting.
    let session_rec_toggle: Option<Arc<dyn Fn() + Send + Sync>> =
        if let Some((rec_secs, ffmpeg_abs, codec)) = session_rec_spec.clone() {
            let st_rec = Arc::clone(&state);
            let hud_rec = Arc::clone(&nav_hud);
            let paths_rec = paths.clone();
            let data_dir_rec = loaded_data_dir.clone();
            let data_source_rec = loaded_data_source.clone();
            let fps_rec = args.record_fps;
            let hk_label = args.record_hotkey.trim().to_ascii_uppercase();

            let spawn_take = Arc::new(move |seconds: f64| {
                let st_done = Arc::clone(&st_rec);
                let hud_done = Arc::clone(&hud_rec);
                let hk_done = hk_label.clone();
                match SessionRecorder::new(
                    SessionRecorderConfig {
                        seconds,
                        fps: fps_rec,
                        codec,
                        ffmpeg_path: ffmpeg_abs.clone(),
                        out_root: paths_rec.session_rec_dir(),
                        tiles_dir: paths_rec.tiles_dir.clone(),
                        ug_overlays_dir: paths_rec.ug_overlays_dir.clone(),
                        paimon_cache_path: paths_rec.paimon_cache_path(),
                        data_dir: data_dir_rec.clone(),
                        data_source: data_source_rec.clone(),
                        maps_db: Arc::clone(&st_rec.maps_db),
                        max_queue: 64,
                    },
                    Some(Box::new(move |out_dir, frames| {
                        if let Some(tray) = st_done
                            .tray
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                        {
                            tray.set_session_recording(false);
                        }
                        let mb = std::fs::metadata(out_dir.join("video.mkv"))
                            .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                            .unwrap_or(0.0);
                        let folder = out_dir
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("session");
                        hud_done.flash(
                            &format!("SESSION REC OK - {frames} frames"),
                            HudPalette::Success,
                            6.0,
                        );
                        let title = if get_language() == "en" {
                            format!("Session saved ({frames} frames, {mb:.1} MB)")
                        } else {
                            format!("Đã lưu phiên ghi ({frames} frame, {mb:.1} MB)")
                        };
                        let sub = format!("{folder} · {hk_done}");
                        st_done.hud_toast.show_toast(
                            &title,
                            &sub,
                            gh_hud::toast::ToastKind::Success,
                            6000,
                            None,
                        );
                    })),
                ) {
                    Ok(srec) => {
                        st_rec.attach_session_recorder(srec);
                        if let Some(tray) = st_rec
                            .tray
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                        {
                            tray.set_session_recording(true);
                        }
                        let (title, sub) = if get_language() == "en" {
                            ("REC started", format!("Press {hk_label} or Tray to stop"))
                        } else {
                            ("Đang ghi phiên gameplay", format!("Bấm {hk_label} hoặc Tray để dừng"))
                        };
                        st_rec.hud_toast.show_toast(
                            title,
                            &sub,
                            gh_hud::toast::ToastKind::Info,
                            4500,
                            None,
                        );
                    }
                    Err(err) => {
                        tracing::error!("[--record-session] Init failed: {err}");
                    }
                }
            });

            // `--record-session <SECONDS>` (> 0) starts immediately; bare `--record-session`
            // starts armed in standby so the user presses Alt+F8 when ready in-game.
            if rec_secs > 0.0 {
                spawn_take(rec_secs);
            }

            let st_tog = Arc::clone(&state);
            let spawn_for_tog = Arc::clone(&spawn_take);
            let toggle: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let active_srec = {
                    let mut guard = st_tog
                        .session_recorder
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if guard.as_ref().is_some_and(|r| r.active()) {
                        guard.take()
                    } else {
                        *guard = None;
                        None
                    }
                };
                if let Some(srec) = active_srec {
                    let title = if get_language() == "en" {
                        "Stopping recording..."
                    } else {
                        "Đang dừng & lưu phiên ghi..."
                    };
                    st_tog.hud_toast.show_toast(
                        title,
                        "ffmpeg",
                        gh_hud::toast::ToastKind::Info,
                        3000,
                        None,
                    );
                    let _ = std::thread::Builder::new()
                        .name("GH_StopSessionRec".to_owned())
                        .spawn(move || {
                            srec.finish();
                            srec.join_timeout(Duration::from_secs(15));
                        });
                } else {
                    spawn_for_tog(rec_secs);
                }
            });

            let hk_combo = args.record_hotkey.trim().to_owned();
            if !hk_combo.is_empty() {
                hotkey_mgr.register(
                    "dev_record_toggle",
                    &hk_combo,
                    Arc::clone(&toggle),
                    true,
                    None,
                );
            }
            Some(toggle)
        } else {
            None
        };
    let _ = &session_rec_toggle;

    // Opt-in LAN Companion startup
    if cfg.lan_companion_enabled {
        lan_companion.set_port(cfg.lan_companion_port);
        if let Err(err) = lan_companion.start_in_background() {
            tracing::error!("Failed to start opt-in LAN Companion: {err}");
        }
    }

    // Wire SyncServer traits & start HTTP server
    server.set_callbacks(Some(
        Arc::new(AppServerCallbacks::new(Arc::clone(&state))) as Arc<dyn ServerCallbacks>,
    ));
    server.set_conflict_advisor(Some(
        Arc::new(AppConflictAdvisor::new(Arc::clone(&state))) as Arc<dyn ConflictAdvisor>,
    ));
    server.set_diagnostic_service(Some(diagnostics as Arc<dyn DiagnosticService>));

    if let Err(err) = server.start().await {
        tracing::error!("Failed to start SyncServer: {err}");
        std::process::exit(1);
    }

    // Optional System Tray
    if !args.no_tray {
        let st_toggle = Arc::clone(&state);
        let st_diag = Arc::clone(&state);
        let st_cfg = Arc::clone(&state);
        let st_quit = Arc::clone(&state);
        let st_borderless = Arc::clone(&state);
        let st_update = Arc::clone(&state);
        let st_stop_rec = Arc::clone(&state);
        let tray = Arc::new(SystemTrayApp::new(
            TrayCallbacks {
                on_toggle_engine: Some(Arc::new(move || {
                    let next = !st_toggle.engine_active.load(Ordering::Acquire);
                    st_toggle.set_engine_active(next);
                    st_toggle.server.broadcast(
                        serde_json::json!({
                            "type": "ENGINE_STATE_UPDATED",
                            "engine_active": next,
                        }),
                        false,
                        false,
                    );
                })),
                on_open_map: Some(Arc::new(|| {
                    open_shell_target("https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2");
                })),
                on_open_diagnostics: Some(Arc::new(move || {
                    let _ = std::fs::create_dir_all(&st_diag.paths.diagnostics_dir);
                    open_shell_target(&st_diag.paths.diagnostics_dir.to_string_lossy());
                })),
                on_open_config_dir: Some(Arc::new(move || {
                    let dir = st_cfg.config_path.parent().unwrap_or(st_cfg.paths.config_dir());
                    let _ = std::fs::create_dir_all(dir);
                    open_shell_target(&dir.to_string_lossy());
                })),
                on_quit: Some(Arc::new(move || {
                    let _ = st_quit.stop_tx.send(true);
                })),
                on_launch_borderless: Some(Arc::new(move || {
                    if let Ok(cb) = AppServerCallbacks::new(Arc::clone(&st_borderless))
                        .on_launch_borderless(&serde_json::json!({}))
                    {
                        tracing::info!("[Tray] Borderless launch: {cb}");
                    }
                })),
                on_open_support: Some(Arc::new(|| {
                    let url = if get_language() == "vi" {
                        "https://github.com/tuanminh26596/Genshin-helper/blob/main/README.vi.md#%E1%BB%A7ng-h%E1%BB%99-t%C3%A1c-gi%E1%BA%A3-donate"
                    } else {
                        "https://ko-fi.com/minmories"
                    };
                    open_shell_target(url);
                })),
                on_check_update: Some(Arc::new(move || {
                    st_update.handle_tray_update_click();
                })),
                on_stop_session_rec: Some(Arc::new(move || {
                    let srec_opt = st_stop_rec
                        .session_recorder
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(srec) = srec_opt {
                        srec.finish();
                        srec.join_timeout(Duration::from_secs(15));
                    }
                    if let Some(tray) = st_stop_rec
                        .tray
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    {
                        tray.set_session_recording(false);
                    }
                })),
            },
            active_init,
            Some(get_language()),
        ));
        if tray.start() {
            let srec_active = state
                .session_recorder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|r| r.active());
            if srec_active {
                tray.set_session_recording(true);
            } else if session_rec_spec.is_some() {
                let hk_disp = args.record_hotkey.trim().to_ascii_uppercase();
                let (title, sub) = if get_language() == "en" {
                    ("Session recorder ready", format!("Press {hk_disp} to start / stop"))
                } else {
                    ("Sẵn sàng quay phiên", format!("Bấm {hk_disp} để bắt đầu / dừng"))
                };
                state.hud_toast.show_toast(
                    title,
                    &sub,
                    gh_hud::toast::ToastKind::Info,
                    5000,
                    None,
                );
            }
            *state.tray.lock().unwrap_or_else(|e| e.into_inner()) = Some(tray);
            state.refresh_tray_state();
        }
    }

    // Startup online checks (strictly gated on auto_check_on_startup && auto_update_mode != "off")
    let update_handle = state.spawn_app_update_scheduler(
        offline_mode,
        args.no_tray,
        shutdown_rx.clone(),
    );
    if !offline_mode {
        prefetch_startup_labels_if_online(&state);
        let st_map_up = Arc::clone(&state);
        let paths_up = paths.clone();
        let sync_up = cfg.hoyolab_sync.clone();
        let mut local_vers: BTreeMap<i32, String> = BTreeMap::new();
        for (&mid, meta) in &maps_db.maps {
            if let Some(ref d2) = meta.detail_v2
                && !d2.map_version.trim().is_empty()
            {
                local_vers.insert(mid, d2.map_version.clone());
            }
        }
        let _ = std::thread::Builder::new()
            .name("gh-startup-update".to_owned())
            .spawn(move || {
                st_map_up.updater.record_map_data_phase("checking", None);
                let runner = HoyolabSyncClient::new();
                match check_and_apply_startup_update(&runner, &paths_up, &sync_up, &local_vers) {
                    Ok(outcome) => {
                        match &outcome {
                            StartupUpdateOutcome::Disabled => {}
                            StartupUpdateOutcome::UpToDate(_) => {
                                tracing::info!("[Startup] HoYoLAB map dataset is up to date.");
                            }
                            StartupUpdateOutcome::UpdateAvailable(res) => {
                                tracing::info!(
                                    "[Startup] HoYoLAB map update available: {:?} (new={:?})",
                                    res.updated_maps.keys().collect::<Vec<_>>(),
                                    res.new_map_ids
                                );
                            }
                            StartupUpdateOutcome::Updated(res) => {
                                tracing::info!(
                                    "[Startup] HoYoLAB map dataset updated atomically: {:?}",
                                    res.updated_maps.keys().collect::<Vec<_>>()
                                );
                            }
                        }
                        st_map_up.handle_map_data_update_outcome(&outcome, false);
                    }
                    Err(e) => {
                        tracing::debug!("[Startup] HoYoLAB version check skipped: {e}");
                        st_map_up.updater.record_map_data_phase("idle", None);
                    }
                }
            });
    }

    if !args.minimized {
        tracing::info!(
            "GenshinHelper v{} ready on http://{}:{}",
            APP_VERSION,
            DEFAULT_SYNC_HOST,
            server.port()
        );
    }
    let _ = (t("app.name", &[]), &WINDOW_TITLES);

    // Spawn tracking loop & config hot-reload watcher
    let track_handle = tokio::spawn(run_tracking_loop(
        Arc::clone(&state),
        shutdown_rx.clone(),
    ));
    let watch_handle = tokio::spawn(run_config_watcher(
        Arc::clone(&state),
        shutdown_rx.clone(),
    ));

    // Wait for shutdown signal (/shutdown, tray quit, or Ctrl+C)
    let mut main_rx = shutdown_rx;
    loop {
        if *main_rx.borrow() {
            break;
        }
        tokio::select! {
            res = main_rx.changed() => {
                if res.is_err() || *main_rx.borrow() {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = stop_tx.send(true);
                break;
            }
        }
    }

    // Deterministic shutdown cleanup & Arc cycle breaking
    let rec_opt = state
        .minimap_recorder
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(rec) = rec_opt {
        rec.finish();
        rec.join_timeout(Duration::from_secs(10));
    }
    let srec_opt = state
        .session_recorder
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(srec) = srec_opt {
        srec.finish();
        srec.join_timeout(Duration::from_secs(15));
    }

    let _ = track_handle.await;
    let _ = watch_handle.await;
    let _ = update_handle.await;

    if let Some(tray) = state
        .tray
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        tray.close();
    }
    state.lan_companion.stop_background();
    state.shutdown_radar();
    state.mmap_overlay.close();
    state.nav_hud.close();
    state.guide_fetcher.stop();
    state.server.stop().await;

    // Break Arc reference cycles between SyncServer, NavMiniHud, and AppState
    state.server.set_callbacks(None);
    state.server.set_conflict_advisor(None);
    state.server.set_diagnostic_service(None);
    *state_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *label_hud_holder.lock().unwrap_or_else(|e| e.into_inner()) = None;

    state.hotkey_mgr.stop();
    state.audio.close();
    state.hud_toast.close();
    state.capturer.close();
    state.hd_provider.close();
    state.ug_provider.close();

    // Release single-instance mutex before optionally launching silent install-on-exit
    // so Inno Setup never encounters a live port, tray window, or mutex lock.
    // Skip install-on-exit when shutdown came from a Setup/Uninstaller process (`skip_exit_update`).
    drop(_mutex_guard);
    if !state.server.should_skip_exit_update() {
        let _ = state.apply_pending_update_on_exit(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults_and_all_override_flags() {
        let def = CliArgs::try_parse_from(["GenshinHelper"]).unwrap();
        assert!(!def.no_tray);
        assert!(!def.standby);
        assert!(!def.minimized);
        assert!(!def.sync_data);
        assert_eq!(def.record_minimap, 0.0);
        assert_eq!(def.port, None);
        assert_eq!(def.lan_port, None);
        assert_eq!(def.config, None);
        assert!(!def.no_mutex);
        assert!(!def.no_elevate);

        let full = CliArgs::try_parse_from([
            "GenshinHelper",
            "--no-tray",
            "--standby",
            "--minimized",
            "--sync-data",
            "--record-minimap",
            "15",
            "--port",
            "18765",
            "--lan-port",
            "18766",
            "--config",
            "custom_cfg.json",
            "--no-mutex",
            "--no-elevate",
        ])
        .unwrap();
        assert!(full.no_tray);
        assert!(full.standby);
        assert!(full.minimized);
        assert!(full.sync_data);
        assert_eq!(full.record_minimap, 15.0);
        assert_eq!(full.port, Some(18765));
        assert_eq!(full.lan_port, Some(18766));
        assert_eq!(full.config, Some(PathBuf::from("custom_cfg.json")));
        assert!(full.no_mutex);
        assert!(full.no_elevate);

        // Hidden dev session recorder flags parse cleanly yet stay hidden from user-facing --help
        let rec = CliArgs::try_parse_from([
            "GenshinHelper",
            "--record-session",
            "30",
            "--record-fps",
            "20",
            "--record-codec",
            "ffv1",
            "--ffmpeg",
            "C:\\tools\\ffmpeg.exe",
        ])
        .unwrap();
        assert_eq!(rec.record_session, Some(30.0));
        assert_eq!(rec.record_fps, 20.0);
        assert_eq!(rec.record_codec, "ffv1");
        assert_eq!(rec.ffmpeg, Some(PathBuf::from("C:\\tools\\ffmpeg.exe")));

        let rec_manual = CliArgs::try_parse_from([
            "GenshinHelper",
            "--record-session",
            "--ffmpeg",
            "C:\\tools\\ffmpeg.exe",
        ])
        .unwrap();
        assert_eq!(rec_manual.record_session, Some(0.0));

        let def_no_rec = CliArgs::try_parse_from(["GenshinHelper"]).unwrap();
        assert_eq!(def_no_rec.record_session, None);

        use clap::CommandFactory;
        let help = CliArgs::command().render_help().to_string();
        assert!(!help.contains("--record-session"), "hidden flag leaked into --help:\n{help}");
        assert!(!help.contains("--ffmpeg"), "hidden flag leaked into --help:\n{help}");
    }

    #[test]
    fn env_flag_truthy_and_should_auto_elevate_respect_guards() {
        assert!(!env_flag_truthy(None));
        assert!(!env_flag_truthy(Some("")));
        assert!(!env_flag_truthy(Some("0")));
        assert!(!env_flag_truthy(Some("false")));
        assert!(env_flag_truthy(Some("1")));
        assert!(env_flag_truthy(Some(" True ")));
        assert!(env_flag_truthy(Some("YES")));
        assert!(env_flag_truthy(Some("on")));

        let def = CliArgs::try_parse_from(["GenshinHelper"]).unwrap();
        assert!(should_auto_elevate(&def, false, false));
        assert!(!should_auto_elevate(&def, false, true));
        assert!(!should_auto_elevate(&def, true, false));

        let no_el = CliArgs::try_parse_from(["GenshinHelper", "--no-elevate"]).unwrap();
        assert!(!should_auto_elevate(&no_el, false, false));

        let no_tray = CliArgs::try_parse_from(["GenshinHelper", "--no-tray"]).unwrap();
        assert!(!should_auto_elevate(&no_tray, false, false));

        let sync = CliArgs::try_parse_from(["GenshinHelper", "--sync-data"]).unwrap();
        assert!(!should_auto_elevate(&sync, false, false));
    }
}
