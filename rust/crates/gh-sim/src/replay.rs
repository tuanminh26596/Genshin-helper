//! Synchronous deterministic replay driver executing production `gh_app::tracking_stack::build_tracking_stack`,
//! `gh_app::tracking_loop::run_tick_worker`, `prepare_tick_locate`, and `apply_tick_output` against
//! recorded frames (`minimap_rec`, full-frame lossless video sessions, or `diag_*` bundles).

use crate::capturer::{CaptureCoverageStats, ReplayCapturer};
use crate::compositor::{
    OfflineRadarAssets, composite_radar_onto_frame, save_bgra_png,
};
use crate::verified_gt::{SessionCheckpointGt, VerifiedGtFile};
use gh_app::capture_backend::FrameCapturer;
use gh_app::diagnostics::{DiagnosticManager, MinimapPreLostRingBuffer};
use gh_app::log_buffer::LogRingBuffer;
use gh_app::mmap_view::{LocatorRegistrar, MMapViewEngine};
use gh_app::state::{AppState, GameState, NavMarkConfirm, RouteState};
use gh_app::tracking_loop::{
    LoopRuntimeState, PostTickContext, TickMode, TickParams, apply_tick_output,
    prepare_tick_locate, run_tick_worker,
};
use gh_app::tracking_stack::{TrackingStackOptions, build_tracking_stack};
use gh_app::updater::AppUpdater;
use gh_core::atomic_io::write_json_atomic;
use gh_core::config::AppConfig;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::i18n::get_language;
use gh_core::map_bridge::PerMapAffineRegistry;
use gh_core::nav_controller::NavigationController;
use gh_core::paths::{AppPaths, RuntimeMode};
use gh_core::traits::OcrProvider;
use gh_cv::map_state::MapState;
use gh_cv::minimap_tracker::MinimapPose;
use gh_cv::tile_cache::LruTerrainTileCache;
use gh_hud::mmap_overlay::{MMapOverlayController, MMapOverlayMode};
use gh_hud::nav_hud::{NavHudConfig, NavMiniHud};
use gh_hud::radar_map::{HeadlessRadarRenderer, RadarAssetSource};
use gh_hud::toast::HudToastOverlay;
use gh_net::lan_companion::{LanCompanionConfig, LanCompanionServer};
use gh_net::pin_guide::{HoYoLABLabelCatalog, PinGuideFetcher, PinIconCache};
use gh_net::sync_server::{SyncServer, SyncServerConfig};
use gh_win32::audio::AudioFeedback;
use gh_win32::hotkey::UnifiedHotkeyManager;
use gh_win32::hotkey_conflicts::HotkeyConflictAdvisor;
use gh_win32::win_ocr::WindowsOcrService;
use image::ImageReader;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const REPORT_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_DRIFT_THRESHOLD_W: f64 = 15.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    MinimapRec,
    VideoSession,
    DiagnosticBundle,
}

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub source_dir: PathBuf,
    pub data_dir: PathBuf,
    pub gt_file: PathBuf,
    pub tiles_dir: Option<PathBuf>,
    pub ug_dir: Option<PathBuf>,
    pub paimon_cache_path: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub max_frames: Option<usize>,
    pub drift_threshold_w: f64,
    pub verify_mmap_view: bool,
    pub render_out: Option<PathBuf>,
    pub out_report: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OfflineAssetsReport {
    pub tiles_dir: String,
    pub ug_dir: String,
    pub paimon_cache_path: String,
    pub paimon_present_at_start: bool,
    pub tile_cache_disk_loads: u64,
    pub tile_cache_provider_loads: u64,
    pub tile_cache_lru_hits: u64,
    pub hd_provider_disk_hits: u64,
    pub hd_downloads: u64,
    pub relocator_ready: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceReport {
    pub frames_processed: usize,
    pub wall_elapsed_s: f64,
    pub effective_fps: f64,
    pub tick_ms_p50: f64,
    pub tick_ms_p90: f64,
    pub tick_ms_p99: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeCounts {
    pub minimap: usize,
    pub minimap_fast: usize,
    pub map_m: usize,
    pub map_m_paused: usize,
    pub idle_closed: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostTickTelemetry {
    pub map_m_synced_count: usize,
    pub pending_verify_scheduled: usize,
    pub pending_verify_corrected: usize,
    pub broadcast_minimap_events: usize,
    pub max_gate_reject_streak: usize,
    pub max_hdr_clip_streak: usize,
    pub mmap_view_frames: usize,
    pub mmap_view_acquired: usize,
    pub mmap_view_drawer_open: usize,
    pub rendered_previews: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriftStreak {
    pub frames: usize,
    pub start_frame: Option<usize>,
    pub end_frame: Option<usize>,
    pub max_delta_w: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegressionSliceSummary {
    pub compared_pos_frames: usize,
    pub median_pos_delta_w: Option<f64>,
    pub p95_pos_delta_w: Option<f64>,
    pub max_pos_delta_w: Option<f64>,
    pub frames_over_threshold: usize,
    pub pct_over_threshold: f64,
    pub longest_drift_streak: DriftStreak,
    pub median_heading_delta_deg: Option<f64>,
    pub p95_heading_delta_deg: Option<f64>,
}

/// Comparison against the recording's own `meta.jsonl` (baseline drift detection ONLY,
/// not proof of ground-truth accuracy).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegressionReport {
    pub baseline_source: String,
    pub drift_threshold_w: f64,
    pub compared_frames: usize,
    pub tracking_frames: usize,
    pub state_mismatches: usize,
    pub map_mismatches: usize,
    pub all_frames: RegressionSliceSummary,
    /// Excluding baseline pause/hold frames (`paimon_pause`, `relocating_hold`, `gate_hold`)
    /// so active tracking fidelity is visible separately from unrecorded pause/M-Map gaps.
    pub active_baseline_frames: RegressionSliceSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccuracyCheckpointOutcome {
    pub frame_idx: usize,
    pub ts: f64,
    pub passed: bool,
    pub false_lock: bool,
    pub expected_map_id: Option<i32>,
    pub actual_map_id: Option<i32>,
    pub pos_err_w: Option<f64>,
    pub heading_err_deg: Option<f64>,
    pub note: String,
}

/// Comparison strictly against human-verified ground truth in git-tracked `tests/verified_gt.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccuracyReport {
    pub has_verified_gt: bool,
    pub gt_file: String,
    pub note: String,
    pub checkpoints_evaluated: usize,
    pub checkpoints_passed: usize,
    pub false_lock_count: usize,
    pub median_pos_err_w: Option<f64>,
    pub max_pos_err_w: Option<f64>,
    pub details: Vec<AccuracyCheckpointOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayReport {
    pub schema_version: u32,
    pub session_name: String,
    pub session_kind: SessionKind,
    pub source_path: String,
    pub offline_assets: OfflineAssetsReport,
    pub capture_coverage: CaptureCoverageStats,
    pub performance: PerformanceReport,
    pub modes: ModeCounts,
    pub match_methods: BTreeMap<String, usize>,
    pub post_tick: PostTickTelemetry,
    pub regression: RegressionReport,
    pub accuracy: AccuracyReport,
}

#[derive(Debug, Clone)]
struct MetaRecord {
    idx: usize,
    ts: f64,
    frame_w: i32,
    frame_h: i32,
    cursor_client: Option<(f64, f64)>,
    capture_path: Option<String>,
    base_state: Option<String>,
    base_match: Option<String>,
    base_map_id: Option<i32>,
    base_world_x: Option<f64>,
    base_world_y: Option<f64>,
    base_heading: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedFrame {
    pub idx: usize,
    pub ts: f64,
    pub state: String,
    pub match_method: String,
    pub map_id: Option<i32>,
    pub world_x: Option<f64>,
    pub world_y: Option<f64>,
    pub heading: Option<f64>,
    #[serde(default)]
    pub body_heading: Option<f64>,
    #[serde(default)]
    pub camera_heading: Option<f64>,
    pub is_underground: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapMSyncRecord {
    pub frame_idx: usize,
    pub ts: f64,
    pub map_id: i32,
    pub payload: Value,
}

struct TempSandboxDir(PathBuf);

impl TempSandboxDir {
    fn new(prefix: &str) -> Result<Self, String> {
        let p = std::env::temp_dir().join(format!(
            "{prefix}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p)
            .map_err(|e| format!("failed to create temp sandbox {}: {e}", p.display()))?;
        Ok(Self(p))
    }
}

impl Drop for TempSandboxDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn detect_session_kind(source_dir: &Path) -> Result<SessionKind, String> {
    if !source_dir.is_dir() {
        return Err(format!(
            "replay source directory does not exist: {}",
            source_dir.display()
        ));
    }
    if source_dir.join("video.mkv").is_file() || source_dir.join("video.mp4").is_file() {
        return Ok(SessionKind::VideoSession);
    }
    if source_dir.join("frames").is_dir() && source_dir.join("meta.jsonl").is_file() {
        return Ok(SessionKind::MinimapRec);
    }
    if source_dir.join("01_game.png").is_file() && source_dir.join("03_report.json").is_file() {
        return Ok(SessionKind::DiagnosticBundle);
    }
    Err(format!(
        "unrecognized replay source layout in {} (expected video.mkv/mp4 + meta.jsonl, frames/ + meta.jsonl, or 01_game.png + 03_report.json)",
        source_dir.display()
    ))
}

fn load_meta_jsonl(path: &Path) -> Result<Vec<MetaRecord>, String> {
    let f = std::fs::File::open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for (line_no, line_res) in reader.lines().enumerate() {
        let line = line_res.map_err(|e| format!("read error at line {}: {e}", line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), line_no + 1))?;
        let idx = v
            .get("i")
            .or_else(|| v.get("frame_idx"))
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(out.len() + 1);
        let ts = v
            .get("ts")
            .and_then(Value::as_f64)
            .filter(|t| t.is_finite())
            .unwrap_or(out.len() as f64 / 20.0);
        let frame_w = v
            .get("frame_w")
            .and_then(Value::as_i64)
            .or_else(|| {
                v.get("client_rect")
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(2))
                    .and_then(Value::as_i64)
            })
            .unwrap_or(2560) as i32;
        let frame_h = v
            .get("frame_h")
            .and_then(Value::as_i64)
            .or_else(|| {
                v.get("client_rect")
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(3))
                    .and_then(Value::as_i64)
            })
            .unwrap_or(1440) as i32;
        let cursor_client = v
            .get("cursor_client_pos")
            .or_else(|| v.get("cursor"))
            .and_then(Value::as_array)
            .and_then(|a| {
                if a.len() == 2 {
                    let x = a[0].as_f64()?;
                    let y = a[1].as_f64()?;
                    if x.is_finite() && y.is_finite() {
                        return Some((x, y));
                    }
                }
                None
            });
        let capture_path = v
            .get("capture_path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let base_state = v.get("state").and_then(Value::as_str).map(str::to_owned);
        let base_match = v.get("match").and_then(Value::as_str).map(str::to_owned);
        let base_map_id = v.get("map_id").and_then(Value::as_i64).map(|n| n as i32);
        let base_world_x = v.get("world_x").and_then(Value::as_f64).filter(|f| f.is_finite());
        let base_world_y = v.get("world_y").and_then(Value::as_f64).filter(|f| f.is_finite());
        let base_heading = v
            .get("view")
            .or_else(|| v.get("camera"))
            .or_else(|| v.get("body"))
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite());
        out.push(MetaRecord {
            idx,
            ts,
            frame_w,
            frame_h,
            cursor_client,
            capture_path,
            base_state,
            base_match,
            base_map_id,
            base_world_x,
            base_world_y,
            base_heading,
        });
    }
    Ok(out)
}

fn load_png_as_bgra(path: &Path) -> Result<FrameBuf, String> {
    let dyn_img = ImageReader::open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("failed to decode {}: {e}", path.display()))?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = FrameBuf::new_zeroed(w, h, PixelFormat::Bgra8);
    for (src, dst) in rgba
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.data.as_chunks_mut::<4>().0.iter_mut())
    {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    Ok(out)
}

struct VideoPipeDecoder {
    child: Child,
    stdout: std::process::ChildStdout,
    w: u32,
    h: u32,
}

impl VideoPipeDecoder {
    fn spawn(ffmpeg_abs: &Path, video_path: &Path, w: u32, h: u32) -> Result<Self, String> {
        if !ffmpeg_abs.is_absolute() || !ffmpeg_abs.is_file() {
            return Err(format!(
                "--ffmpeg must be an existing absolute file path (never resolved from PATH): {}",
                ffmpeg_abs.display()
            ));
        }
        let mut child = Command::new(ffmpeg_abs)
            .args(["-v", "error", "-i"])
            .arg(video_path)
            .args(["-f", "rawvideo", "-pix_fmt", "bgr24", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn ffmpeg ({}): {e}", ffmpeg_abs.display()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg stdout pipe missing".to_owned())?;
        Ok(Self {
            child,
            stdout,
            w,
            h,
        })
    }

    fn read_next_frame(&mut self) -> Result<Option<FrameBuf>, String> {
        let mut buf = FrameBuf::new_zeroed(self.w, self.h, PixelFormat::Bgr8);
        let mut filled = 0usize;
        while filled < buf.data.len() {
            let n = self
                .stdout
                .read(&mut buf.data[filled..])
                .map_err(|e| format!("ffmpeg pipe read error: {e}"))?;
            if n == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(format!(
                    "truncated video frame: read {filled} of {} bytes",
                    buf.data.len()
                ));
            }
            filled += n;
        }
        Ok(Some(buf))
    }
}

impl Drop for VideoPipeDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn percentile(vals: &[f64], q: f64) -> Option<f64> {
    let mut clean: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    if clean.is_empty() {
        return None;
    }
    clean.sort_by(f64::total_cmp);
    let idx = ((clean.len() as f64 - 1.0) * q).round() as usize;
    Some(clean[idx.min(clean.len() - 1)])
}

pub(crate) fn angle_diff_deg(a: f64, b: f64) -> f64 {
    (((a - b + 180.0).rem_euclid(360.0)) - 180.0).abs()
}

pub(crate) fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn build_offline_replay_state(
    sandbox_root: &Path,
    data_dir: &Path,
    tiles_dir: PathBuf,
    ug_dir: PathBuf,
    paimon_cache_path: PathBuf,
    capturer: Arc<ReplayCapturer>,
    enable_mmap_view: bool,
) -> Result<(Arc<AppState>, Arc<LruTerrainTileCache>, AppConfig, bool), String> {
    let paths = AppPaths {
        mode: RuntimeMode::Installed,
        config_path: sandbox_root.join("config.json"),
        diagnostics_dir: sandbox_root.join("diagnostics"),
        data_dir: data_dir.to_path_buf(),
        tiles_dir: tiles_dir.clone(),
        ug_overlays_dir: ug_dir.clone(),
        cache_dir: sandbox_root.join("cache"),
        dev_data_dir: None,
    };

    let mut cfg = AppConfig {
        audio_enabled: false,
        audio_volume: 0,
        auto_check_app_updates: false,
        app_update_mode: "off".to_owned(),
        lan_companion_enabled: false,
        radar_enabled: false,
        ..Default::default()
    };
    cfg.hoyolab_sync.auto_check_on_startup = false;
    cfg.hoyolab_sync.auto_update_mode = "off".to_owned();

    // Shared production tracking stack builder — only explicit offline/sync options differ from live.
    let stack = build_tracking_stack(TrackingStackOptions {
        explicit_data_dir: Some(data_dir.to_path_buf()),
        tiles_dir,
        ug_overlays_dir: ug_dir,
        paimon_cache_path,
        global_index_path: Some(sandbox_root.join("teyvat_global_index_v3.npz")),
        offline: true,
        async_fetch: false,
        sync_relocator_warmup: true,
        ocr: WindowsOcrService::try_new()
            .ok()
            .map(|s| Arc::new(s) as Arc<dyn OcrProvider>),
    })
    .map_err(|e| format!("failed to build shared tracking stack: {e}"))?;

    let tile_cache = Arc::clone(&stack.tile_cache);
    let relocator_ready = stack.relocator_ready;

    let lan_companion = Arc::new(LanCompanionServer::new(LanCompanionConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        advertised_host: Some("127.0.0.1".to_owned()),
        ..Default::default()
    }));
    let server = SyncServer::new(SyncServerConfig {
        port: 0,
        config: cfg.clone(),
        config_path: Some(paths.config_path.clone()),
        diagnostics_dir: Some(paths.diagnostics_dir.clone()),
        lan_companion: Some(Arc::clone(&lan_companion)),
        ..Default::default()
    });

    let config_arc = Arc::new(RwLock::new(cfg.clone()));
    let capturer_dyn: Arc<dyn FrameCapturer> = capturer;
    let minimap_ring = MinimapPreLostRingBuffer::default();
    let srv_for_bs = server.clone();
    let diagnostics = Arc::new(
        DiagnosticManager::new(
            paths.clone(),
            Arc::clone(&capturer_dyn),
            Some(Arc::clone(&stack.locator)),
            Arc::clone(&config_arc),
            Arc::new(move || srv_for_bs.last_browser_state()),
            Arc::new(|| None),
            Arc::new(|| None),
        )
        .with_log_buffer(LogRingBuffer::default())
        .with_minimap_ring(minimap_ring.clone()),
    );

    let (stop_tx, _stop_rx) = watch::channel(false);
    let state = Arc::new(AppState {
        paths: paths.clone(),
        config_path: paths.config_path.clone(),
        config: config_arc,
        cli_port: None,
        cli_lan_port: None,
        disk_ports: Mutex::new((cfg.ws_port, cfg.lan_companion_port)),
        config_io_lock: Mutex::new(()),
        last_self_saved_mtime: Mutex::new(None),
        engine_active: AtomicBool::new(true),
        anchor_seq: AtomicU64::new(0),
        game_state: Mutex::new(GameState::default()),
        nav_state_snapshot: Mutex::new(None),
        nav_controller: Mutex::new(NavigationController::new().with_space_debounce_frames(4)),
        route_state: Mutex::new(RouteState::default()),
        nav_mark_confirm: Mutex::new(NavMarkConfirm::default()),
        tracker: Mutex::new(Some((stack.minimap_tracker, 0.0))),
        locator: Some(Arc::clone(&stack.locator)),
        state_machine: Mutex::new(stack.state_machine),
        capturer: capturer_dyn,
        server,
        lan_companion,
        nav_hud: Arc::new(NavMiniHud::new(NavHudConfig {
            screen_offset: gh_hud::layered::OFFSCREEN_TEST_OFFSET,
            ..Default::default()
        })),
        mmap_overlay: Arc::new(MMapOverlayController::new_with_offset(
            gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        )),
        mmap_view: if enable_mmap_view {
            Some(Arc::new(MMapViewEngine::new_sync(Arc::new(
                LocatorRegistrar::new(Arc::clone(&stack.locator)),
            ))))
        } else {
            None
        },
        radar_map: Mutex::new(None),
        radar_active: AtomicBool::new(false),
        radar_pins: Mutex::new(None),
        pin_icon_cache: PinIconCache::new(
            paths.cache_dir.join("lan_icons"),
            Arc::new(HoYoLABLabelCatalog::new()),
            Duration::from_secs(1),
        ),
        hud_toast: Arc::new(HudToastOverlay::with_screen_offset(
            gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        )),
        audio: Arc::new(AudioFeedback::new(false, 0)),
        tray: Mutex::new(None),
        hotkey_mgr: Arc::new(UnifiedHotkeyManager::new(false, None)),
        conflict_advisor: Arc::new(HotkeyConflictAdvisor::new()),
        guide_fetcher: PinGuideFetcher::new(Arc::new(get_language)),
        label_catalog: HoYoLABLabelCatalog::new(),
        maps_db: stack.maps_db,
        affine_registry: PerMapAffineRegistry::new(),
        hd_provider: stack.hd_provider,
        ug_provider: stack.ug_provider,
        minimap_recorder: Mutex::new(None),
        session_recorder: Mutex::new(None),
        minimap_ring,
        diagnostics,
        updater: Arc::new(AppUpdater::default()),
        stop_tx,
        start_instant: Instant::now(),
    });
    Ok((state, tile_cache, cfg, relocator_ready))
}

fn evaluate_session_accuracy(
    gt_file: &Path,
    session_name: &str,
    observed: &[ObservedFrame],
) -> Result<AccuracyReport, String> {
    let gt = VerifiedGtFile::load_or_default(gt_file)?;
    let Some(sess_gt) = gt.sessions.get(session_name) else {
        return Ok(AccuracyReport {
            has_verified_gt: false,
            gt_file: gt_file.display().to_string(),
            note: format!(
                "No human-verified ground truth entry for session '{session_name}' in {}; regression metrics only.",
                gt_file.display()
            ),
            ..Default::default()
        });
    };

    if sess_gt.checkpoints.is_empty() {
        return Ok(AccuracyReport {
            has_verified_gt: false,
            gt_file: gt_file.display().to_string(),
            note: format!(
                "Session '{session_name}' has 0 checkpoints in {}.",
                gt_file.display()
            ),
            ..Default::default()
        });
    }

    let by_idx: BTreeMap<usize, &ObservedFrame> = observed.iter().map(|o| (o.idx, o)).collect();
    let mut details = Vec::with_capacity(sess_gt.checkpoints.len());
    let mut pos_errs = Vec::new();
    let mut passed_cnt = 0usize;
    let mut false_locks = 0usize;

    for cp in &sess_gt.checkpoints {
        let obs = resolve_checkpoint_frame(cp, &by_idx, observed);
        let Some(obs) = obs else {
            details.push(AccuracyCheckpointOutcome {
                frame_idx: cp.frame_idx.unwrap_or(0),
                ts: cp.ts.unwrap_or(0.0),
                passed: false,
                false_lock: false,
                expected_map_id: cp.map_id,
                actual_map_id: None,
                pos_err_w: None,
                heading_err_deg: None,
                note: format!("checkpoint frame not present in replay ({})", cp.note),
            });
            continue;
        };

        let has_lock = obs.state == "TRACKING" && obs.world_x.is_some() && obs.world_y.is_some();
        if cp.expect_rejected {
            let fl = has_lock;
            if fl {
                false_locks += 1;
            } else {
                passed_cnt += 1;
            }
            details.push(AccuracyCheckpointOutcome {
                frame_idx: obs.idx,
                ts: obs.ts,
                passed: !fl,
                false_lock: fl,
                expected_map_id: None,
                actual_map_id: obs.map_id,
                pos_err_w: None,
                heading_err_deg: None,
                note: cp.note.clone(),
            });
            continue;
        }

        let pos_err = match (cp.ref_xy, obs.world_x, obs.world_y) {
            (Some([rx, ry]), Some(wx), Some(wy)) => Some((wx - rx).hypot(wy - ry)),
            _ => None,
        };
        if let Some(e) = pos_err {
            pos_errs.push(e);
        }
        let hdg_err = match (cp.heading_deg, obs.heading) {
            (Some(eh), Some(ah)) => Some(angle_diff_deg(ah, eh)),
            _ => None,
        };
        let map_ok = cp.map_id.is_none_or(|exp| obs.map_id == Some(exp));
        let pos_ok = cp.ref_xy.is_none() || pos_err.is_some_and(|e| e <= cp.tol_w);
        let hdg_ok = match (cp.heading_deg, cp.tol_deg) {
            (Some(_), Some(tol)) => hdg_err.is_some_and(|e| e <= tol),
            _ => true,
        };
        let ug_ok = cp.is_underground.is_none_or(|u| obs.is_underground == u);
        let passed = has_lock && map_ok && pos_ok && hdg_ok && ug_ok;
        let false_lock = has_lock && (!map_ok || pos_err.is_some_and(|e| e > cp.tol_w.max(120.0)));
        if false_lock {
            false_locks += 1;
        }
        if passed {
            passed_cnt += 1;
        }
        details.push(AccuracyCheckpointOutcome {
            frame_idx: obs.idx,
            ts: obs.ts,
            passed,
            false_lock,
            expected_map_id: cp.map_id,
            actual_map_id: obs.map_id,
            pos_err_w: pos_err.map(round3),
            heading_err_deg: hdg_err.map(round3),
            note: cp.note.clone(),
        });
    }

    Ok(AccuracyReport {
        has_verified_gt: true,
        gt_file: gt_file.display().to_string(),
        note: sess_gt.note.clone(),
        checkpoints_evaluated: details.len(),
        checkpoints_passed: passed_cnt,
        false_lock_count: false_locks,
        median_pos_err_w: percentile(&pos_errs, 0.50).map(round3),
        max_pos_err_w: pos_errs.iter().copied().reduce(f64::max).map(round3),
        details,
    })
}

fn resolve_checkpoint_frame<'a>(
    cp: &SessionCheckpointGt,
    by_idx: &BTreeMap<usize, &'a ObservedFrame>,
    observed: &'a [ObservedFrame],
) -> Option<&'a ObservedFrame> {
    if let Some(fi) = cp.frame_idx {
        return by_idx.get(&fi).copied();
    }
    if let Some(target_ts) = cp.ts {
        return observed.iter().min_by(|a, b| {
            (a.ts - target_ts)
                .abs()
                .total_cmp(&(b.ts - target_ts).abs())
        });
    }
    None
}

fn is_baseline_pause_match(m: Option<&str>) -> bool {
    matches!(
        m,
        Some("paimon_pause" | "relocating_hold" | "gate_hold" | "gate_rejected" | "loading_pause" | "paused")
    )
}

fn compute_slice_summary<'a>(
    pairs: impl Iterator<Item = (&'a MetaRecord, &'a ObservedFrame)>,
    drift_threshold_w: f64,
) -> RegressionSliceSummary {
    let mut pos_deltas = Vec::new();
    let mut hdg_deltas = Vec::new();
    let mut over_cnt = 0usize;

    let mut cur_streak_len = 0usize;
    let mut cur_start: Option<usize> = None;
    let mut cur_max = 0.0f64;
    let mut best = DriftStreak::default();

    for (rec, obs) in pairs {
        if let (Some(bx), Some(by), Some(ox), Some(oy)) =
            (rec.base_world_x, rec.base_world_y, obs.world_x, obs.world_y)
        {
            let d = (ox - bx).hypot(oy - by);
            pos_deltas.push(d);
            if d > drift_threshold_w {
                over_cnt += 1;
                if cur_streak_len == 0 {
                    cur_start = Some(obs.idx);
                    cur_max = d;
                } else {
                    cur_max = cur_max.max(d);
                }
                cur_streak_len += 1;
                if cur_streak_len > best.frames {
                    best = DriftStreak {
                        frames: cur_streak_len,
                        start_frame: cur_start,
                        end_frame: Some(obs.idx),
                        max_delta_w: Some(round3(cur_max)),
                    };
                }
            } else {
                cur_streak_len = 0;
                cur_start = None;
                cur_max = 0.0;
            }
        } else {
            cur_streak_len = 0;
            cur_start = None;
            cur_max = 0.0;
        }
        if let (Some(bh), Some(oh)) = (rec.base_heading, obs.heading) {
            hdg_deltas.push(angle_diff_deg(oh, bh));
        }
    }

    let compared_pos_frames = pos_deltas.len();
    let pct_over = if compared_pos_frames > 0 {
        round3((over_cnt as f64 / compared_pos_frames as f64) * 100.0)
    } else {
        0.0
    };

    RegressionSliceSummary {
        compared_pos_frames,
        median_pos_delta_w: percentile(&pos_deltas, 0.50).map(round3),
        p95_pos_delta_w: percentile(&pos_deltas, 0.95).map(round3),
        max_pos_delta_w: pos_deltas.iter().copied().reduce(f64::max).map(round3),
        frames_over_threshold: over_cnt,
        pct_over_threshold: pct_over,
        longest_drift_streak: best,
        median_heading_delta_deg: percentile(&hdg_deltas, 0.50).map(round3),
        p95_heading_delta_deg: percentile(&hdg_deltas, 0.95).map(round3),
    }
}

fn compute_regression(
    records: &[MetaRecord],
    observed: &[ObservedFrame],
    drift_threshold_w: f64,
) -> RegressionReport {
    let mut compared_frames = 0usize;
    let mut tracking_frames = 0usize;
    let mut state_mismatches = 0usize;
    let mut map_mismatches = 0usize;

    for (rec, obs) in records.iter().zip(observed.iter()) {
        if obs.state == "TRACKING" {
            tracking_frames += 1;
        }
        if let Some(ref b_state) = rec.base_state {
            compared_frames += 1;
            if b_state != &obs.state {
                state_mismatches += 1;
            }
        }
        if let (Some(bm), Some(om)) = (rec.base_map_id, obs.map_id)
            && bm != om
        {
            map_mismatches += 1;
        }
    }

    let all_frames = compute_slice_summary(records.iter().zip(observed.iter()), drift_threshold_w);
    let active_baseline_frames = compute_slice_summary(
        records
            .iter()
            .zip(observed.iter())
            .filter(|(rec, _)| !is_baseline_pause_match(rec.base_match.as_deref())),
        drift_threshold_w,
    );

    RegressionReport {
        baseline_source: "meta.jsonl".to_owned(),
        drift_threshold_w,
        compared_frames,
        tracking_frames,
        state_mismatches,
        map_mismatches,
        all_frames,
        active_baseline_frames,
    }
}

pub fn run_replay(opts: &ReplayOptions) -> Result<ReplayReport, String> {
    run_replay_with_poses(opts).map(|(report, _)| report)
}

pub fn run_replay_with_poses(
    opts: &ReplayOptions,
) -> Result<(ReplayReport, Vec<ObservedFrame>), String> {
    run_replay_full(opts).map(|(report, poses, _)| (report, poses))
}

pub fn run_replay_full(
    opts: &ReplayOptions,
) -> Result<(ReplayReport, Vec<ObservedFrame>, Vec<MapMSyncRecord>), String> {
    let kind = detect_session_kind(&opts.source_dir)?;
    let session_name = opts
        .source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session")
        .to_owned();

    let sandbox = TempSandboxDir::new("gh_sim_replay")?;
    let assets_cache = opts.source_dir.join("assets_cache");
    let tiles_dir = if assets_cache.join("tiles").is_dir() {
        assets_cache.join("tiles")
    } else if let Some(ref td) = opts.tiles_dir {
        td.clone()
    } else {
        let empty = sandbox.0.join("tiles");
        let _ = std::fs::create_dir_all(&empty);
        empty
    };
    let ug_dir = if assets_cache.join("ug_overlays").is_dir() {
        assets_cache.join("ug_overlays")
    } else if let Some(ref ud) = opts.ug_dir {
        ud.clone()
    } else {
        let empty = sandbox.0.join("ug_overlays");
        let _ = std::fs::create_dir_all(&empty);
        empty
    };
    let paimon_cache = if assets_cache.join("paimon_tpl.npz").is_file() {
        assets_cache.join("paimon_tpl.npz")
    } else if let Some(ref pc) = opts.paimon_cache_path {
        pc.clone()
    } else {
        sandbox.0.join("paimon_tpl.npz")
    };
    let paimon_present_at_start = paimon_cache.is_file();

    let enable_mmap_view = opts.verify_mmap_view || opts.render_out.is_some();
    let capturer = Arc::new(ReplayCapturer::new());
    let (state, tile_cache, cfg_snap, relocator_ready) = build_offline_replay_state(
        &sandbox.0,
        &opts.data_dir,
        tiles_dir.clone(),
        ug_dir.clone(),
        paimon_cache.clone(),
        Arc::clone(&capturer),
        enable_mmap_view,
    )?;

    let mut radar_renderer = if let Some(ref rdir) = opts.render_out {
        std::fs::create_dir_all(rdir)
            .map_err(|e| format!("failed to create render-out dir {}: {e}", rdir.display()))?;
        let assets: Arc<dyn RadarAssetSource> = Arc::new(OfflineRadarAssets {
            maps_db: Arc::clone(&state.maps_db),
            hd_provider: Arc::clone(&state.hd_provider),
            ug_provider: Arc::clone(&state.ug_provider),
        });
        Some(HeadlessRadarRenderer::new(assets))
    } else {
        None
    };

    let mut records: Vec<MetaRecord>;
    let mut video_decoder: Option<VideoPipeDecoder> = None;

    match kind {
        SessionKind::MinimapRec => {
            records = load_meta_jsonl(&opts.source_dir.join("meta.jsonl"))?;
        }
        SessionKind::VideoSession => {
            records = load_meta_jsonl(&opts.source_dir.join("meta.jsonl"))?;
            let video_path = if opts.source_dir.join("video.mkv").is_file() {
                opts.source_dir.join("video.mkv")
            } else {
                opts.source_dir.join("video.mp4")
            };
            let ffmpeg_abs = opts.ffmpeg_path.as_deref().ok_or_else(|| {
                "replaying a video session requires explicit absolute --ffmpeg <PATH>".to_owned()
            })?;
            let (vw, vh) = records
                .first()
                .map(|r| (r.frame_w.max(1) as u32, r.frame_h.max(1) as u32))
                .ok_or_else(|| "video session meta.jsonl has no records".to_owned())?;
            video_decoder = Some(VideoPipeDecoder::spawn(ffmpeg_abs, &video_path, vw, vh)?);
        }
        SessionKind::DiagnosticBundle => {
            let img = load_png_as_bgra(&opts.source_dir.join("01_game.png"))?;
            let (fw, fh) = (img.width as i32, img.height as i32);
            let rep_raw = std::fs::read_to_string(opts.source_dir.join("03_report.json"))
                .unwrap_or_default();
            let rep_val: Value = serde_json::from_str(&rep_raw).unwrap_or(Value::Null);
            let cv_tel = rep_val.get("cv_telemetry");
            records = vec![MetaRecord {
                idx: 1,
                ts: 1.0,
                frame_w: fw,
                frame_h: fh,
                cursor_client: None,
                capture_path: None,
                base_state: None,
                base_match: None,
                base_map_id: cv_tel
                    .and_then(|c| c.get("map_id"))
                    .and_then(Value::as_i64)
                    .map(|n| n as i32),
                base_world_x: cv_tel.and_then(|c| c.get("world_x")).and_then(Value::as_f64),
                base_world_y: cv_tel.and_then(|c| c.get("world_y")).and_then(Value::as_f64),
                base_heading: None,
            }];
        }
    }

    if let Some(limit) = opts.max_frames {
        records.truncate(limit);
    }

    let base_ts = records.first().map_or(0.0, |r| r.ts);
    let mut rt = LoopRuntimeState::new();
    let mut modes = ModeCounts::default();
    let mut match_methods: BTreeMap<String, usize> = BTreeMap::new();
    let mut post_tick = PostTickTelemetry::default();
    let mut tick_ms = Vec::with_capacity(records.len());
    let mut observed = Vec::with_capacity(records.len());
    let mut map_m_syncs: Vec<MapMSyncRecord> = Vec::new();
    let mut last_rendered_idx: Option<usize> = None;
    let wall_t0 = Instant::now();

    for rec in &records {
        let frame_opt = match kind {
            SessionKind::MinimapRec => {
                let p = opts
                    .source_dir
                    .join("frames")
                    .join(format!("{:06}.png", rec.idx));
                if !p.is_file() {
                    return Err(format!("missing recorded frame PNG: {}", p.display()));
                }
                Some(load_png_as_bgra(&p)?)
            }
            SessionKind::VideoSession => {
                let dec = video_decoder
                    .as_mut()
                    .ok_or_else(|| "video decoder not initialized".to_owned())?;
                dec.read_next_frame()?
            }
            SessionKind::DiagnosticBundle => {
                Some(load_png_as_bgra(&opts.source_dir.join("01_game.png"))?)
            }
        };

        let Some(frame) = frame_opt else {
            break;
        };

        let render_clone = if opts.render_out.is_some()
            && post_tick.rendered_previews < 8
            && last_rendered_idx.is_none_or(|prev| rec.idx.saturating_sub(prev) >= 100)
        {
            Some(frame.clone())
        } else {
            None
        };

        let is_hdr = rec
            .capture_path
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("wgc_hdr"));
        capturer.set_frame(Some(frame), 0, 0, rec.frame_w, rec.frame_h, is_hdr);

        let is_closed = state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            == MapState::Closed;

        let now_s = (rec.ts - base_ts).max(0.0);
        let params = TickParams {
            game_hwnd: 0,
            cl: 0,
            ct: 0,
            cw: rec.frame_w,
            ch: rec.frame_h,
            is_closed,
            do_auto_sync: true,
            do_minimap: true,
            fast_ok: false,
            guide_view: enable_mmap_view,
            guide_map: 2,
            want_window_capture: false,
            now_s: Some(now_s),
            cursor_client: rec.cursor_client,
            capture_session_frame: false,
        };

        let t0 = Instant::now();
        let out = run_tick_worker(&state, params);
        if let Some(ref vf) = out.view {
            post_tick.mmap_view_frames += 1;
            if vf.transform.is_some() {
                post_tick.mmap_view_acquired += 1;
            }
            if vf.drawer_open {
                post_tick.mmap_view_drawer_open += 1;
            }
        }
        if let (Some(rdir), Some(rr), Some(mut canvas), Some(pose)) = (
            opts.render_out.as_ref(),
            radar_renderer.as_mut(),
            render_clone,
            out.pose.as_ref(),
        ) && out.mode == TickMode::Minimap
            && let (Some(wx), Some(wy)) = (pose.world_x, pose.world_y)
            && composite_radar_onto_frame(
                rr,
                &mut canvas,
                pose.map_id.unwrap_or(2),
                wx,
                wy,
                pose.body_heading.or(pose.view_heading),
                pose.camera_heading.or(pose.view_heading),
                pose.is_underground,
                pose.floor_id,
                &pose.floor_name,
            )
        {
            let out_png = rdir.join(format!("frame_{:06}_radar.png", rec.idx));
            if save_bgra_png(&canvas, &out_png).is_ok() {
                post_tick.rendered_previews += 1;
                last_rendered_idx = Some(rec.idx);
            }
        }

        let mut obs = ObservedFrame {
            idx: rec.idx,
            ts: rec.ts,
            state: "IDLE".to_owned(),
            match_method: String::new(),
            map_id: None,
            world_x: None,
            world_y: None,
            heading: None,
            body_heading: None,
            camera_heading: None,
            is_underground: false,
        };

        match out.mode {
            TickMode::Minimap => {
                modes.minimap += 1;
                if let Some(ref pose) = out.pose {
                    apply_pose_to_obs(pose, &mut obs);
                }
            }
            TickMode::MinimapFast => {
                modes.minimap_fast += 1;
                if let Some(ref pose) = out.pose {
                    apply_pose_to_obs(pose, &mut obs);
                }
            }
            TickMode::MapM => {
                modes.map_m += 1;
                if let Some(ref sr) = out.step_res {
                    obs.state = format!("MAP_M_{:?}", sr.state);
                    obs.match_method = "map_m".to_owned();
                }
            }
            TickMode::MapMPaused => {
                modes.map_m_paused += 1;
                obs.state = "MAP_M_PAUSED".to_owned();
                obs.match_method = "map_m_paused".to_owned();
            }
            TickMode::IdleClosed => {
                modes.idle_closed += 1;
                obs.state = "IDLE_CLOSED".to_owned();
                obs.match_method = "idle_closed".to_owned();
            }
        }

        // Execute the exact shared production post-tick state machine (pending_verify,
        // gate_reject_streak, hdr_clip_streak, apply_map_m_anchor, nav_controller, etc.).
        let locate_res = if let Some(loc_kind) = prepare_tick_locate(&mut rt, &cfg_snap, &out)
            && let Some(ref loc) = state.locator
            && let Some(ref full_frame) = out.full_frame
        {
            let slider_hint = out.step_res.as_ref().and_then(|s| s.check.slider_res.as_ref());
            let loc_t0 = Instant::now();
            let want_dbg = tracing::enabled!(tracing::Level::INFO);
            let found = loc.locate_player_frame(full_frame.as_view(), want_dbg, slider_hint);
            if want_dbg {
                let loc_ms = loc_t0.elapsed().as_secs_f64() * 1000.0;
                match &found {
                    Some(r) => tracing::info!(
                        "[gh-sim locate] f={} kind={:?} ms={:.1} ok={} map={:?} target_map={:?} ocr={:?} inliers={:?} xy=({:?},{:?}) ug={} floor={:?}",
                        rec.idx,
                        loc_kind,
                        loc_ms,
                        r.has_world_coords,
                        r.map_id,
                        r.target_map_id,
                        r.region_text,
                        r.inliers_count,
                        r.world_x.map(|v| v.round()),
                        r.world_y.map(|v| v.round()),
                        r.is_underground,
                        r.floor_id,
                    ),
                    None => tracing::info!(
                        "[gh-sim locate] f={} kind={:?} ms={:.1} result=None",
                        rec.idx,
                        loc_kind,
                        loc_ms,
                    ),
                }
            }
            Some((loc_kind, found))
        } else {
            None
        };

        let post_ctx = PostTickContext {
            game_rect: RectI32 {
                x: 0,
                y: 0,
                width: rec.frame_w,
                height: rec.frame_h,
            },
            route_needs_mmap: false,
            overlay_mode: MMapOverlayMode::Off,
            session_rec_active: false,
            enable_ui_side_effects: false,
        };
        let mut noop_hide = || {};
        let summary = apply_tick_output(
            &state,
            &mut rt,
            &cfg_snap,
            &post_ctx,
            out,
            locate_res,
            &mut noop_hide,
        );

        if summary.map_m_synced {
            post_tick.map_m_synced_count += 1;
            obs.state = "MAP_M_SYNCED".to_owned();
            if let Some((mid, wx, wy, ug)) = summary.map_m_anchor {
                obs.map_id = Some(mid);
                obs.world_x = Some(wx);
                obs.world_y = Some(wy);
                obs.is_underground = ug;
            }
        }
        if let Some(payload) = summary.map_m_sync_payload {
            let map_id = summary
                .map_m_anchor
                .map(|(mid, _, _, _)| mid)
                .or_else(|| payload.get("map_id").and_then(Value::as_i64).map(|v| v as i32))
                .unwrap_or(2);
            map_m_syncs.push(MapMSyncRecord {
                frame_idx: rec.idx,
                ts: rec.ts,
                map_id,
                payload,
            });
        }
        if summary.pending_verify_scheduled {
            post_tick.pending_verify_scheduled += 1;
        }
        if summary.pending_verify_corrected {
            post_tick.pending_verify_corrected += 1;
        }
        if summary.broadcast_minimap_pos {
            post_tick.broadcast_minimap_events += 1;
        }
        post_tick.max_gate_reject_streak =
            post_tick.max_gate_reject_streak.max(summary.gate_reject_streak);
        post_tick.max_hdr_clip_streak =
            post_tick.max_hdr_clip_streak.max(summary.hdr_clip_streak);

        if !obs.match_method.is_empty() {
            *match_methods.entry(obs.match_method.clone()).or_insert(0) += 1;
        }

        tick_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        observed.push(obs);
    }

    let wall_elapsed_s = wall_t0.elapsed().as_secs_f64();
    let frames_processed = observed.len();
    let effective_fps = if wall_elapsed_s > 0.0 {
        frames_processed as f64 / wall_elapsed_s
    } else {
        0.0
    };

    let drift_thresh = if opts.drift_threshold_w.is_finite() && opts.drift_threshold_w > 0.0 {
        opts.drift_threshold_w
    } else {
        DEFAULT_DRIFT_THRESHOLD_W
    };
    let regression = compute_regression(&records[..frames_processed], &observed, drift_thresh);
    let accuracy = evaluate_session_accuracy(&opts.gt_file, &session_name, &observed)?;
    let tc_stats = tile_cache.stats();

    let report = ReplayReport {
        schema_version: REPORT_SCHEMA_VERSION,
        session_name,
        session_kind: kind,
        source_path: opts.source_dir.display().to_string(),
        offline_assets: OfflineAssetsReport {
            tiles_dir: tiles_dir.display().to_string(),
            ug_dir: ug_dir.display().to_string(),
            paimon_cache_path: paimon_cache.display().to_string(),
            paimon_present_at_start,
            tile_cache_disk_loads: tc_stats.disk_loads,
            tile_cache_provider_loads: tc_stats.provider_loads,
            tile_cache_lru_hits: tc_stats.hits,
            hd_provider_disk_hits: state.hd_provider.disk_hits(),
            hd_downloads: state.hd_provider.downloads(),
            relocator_ready,
        },
        capture_coverage: capturer.stats(),
        performance: PerformanceReport {
            frames_processed,
            wall_elapsed_s: round3(wall_elapsed_s),
            effective_fps: round3(effective_fps),
            tick_ms_p50: round3(percentile(&tick_ms, 0.50).unwrap_or(0.0)),
            tick_ms_p90: round3(percentile(&tick_ms, 0.90).unwrap_or(0.0)),
            tick_ms_p99: round3(percentile(&tick_ms, 0.99).unwrap_or(0.0)),
        },
        modes,
        match_methods,
        post_tick,
        regression,
        accuracy,
    };

    state.capturer.close();
    state.hd_provider.close();
    state.ug_provider.close();
    state.audio.close();
    state.guide_fetcher.stop();

    if let Some(ref out_path) = opts.out_report {
        let val = serde_json::to_value(&report)
            .map_err(|e| format!("failed to serialize report: {e}"))?;
        write_json_atomic(out_path, &val)
            .map_err(|e| format!("failed to write {}: {e}", out_path.display()))?;
    }

    Ok((report, observed, map_m_syncs))
}

fn apply_pose_to_obs(pose: &MinimapPose, obs: &mut ObservedFrame) {
    obs.state = pose.state.as_str().to_owned();
    obs.match_method = pose.match_method.clone();
    obs.map_id = pose.map_id;
    obs.world_x = pose.world_x;
    obs.world_y = pose.world_y;
    obs.heading = pose.view_heading.or(pose.camera_heading).or(pose.body_heading);
    obs.body_heading = pose.body_heading;
    obs.camera_heading = pose.camera_heading;
    obs.is_underground = pose.is_underground;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::atomic_io::write_json_atomic;
    use serde_json::json;

    #[test]
    fn replay_runs_production_tick_worker_and_post_tick_synchronously() {
        let sandbox = TempSandboxDir::new("gh_sim_replay_unit").unwrap();
        let data_dir = sandbox.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let session_dir = sandbox.0.join("rec_unit");
        std::fs::create_dir_all(session_dir.join("frames")).unwrap();
        let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
        for i in 1..=3usize {
            img.save(session_dir.join("frames").join(format!("{i:06}.png")))
                .unwrap();
        }
        let meta = concat!(
            "{\"i\":1,\"ts\":100.0,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n",
            "{\"i\":2,\"ts\":100.05,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n",
            "{\"i\":3,\"ts\":100.10,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n"
        );
        std::fs::write(session_dir.join("meta.jsonl"), meta).unwrap();

        let gt_file = sandbox.0.join("verified_gt.json");
        let out_report = sandbox.0.join("report.json");

        let report = run_replay(&ReplayOptions {
            source_dir: session_dir,
            data_dir,
            gt_file,
            tiles_dir: None,
            ug_dir: None,
            paimon_cache_path: None,
            ffmpeg_path: None,
            max_frames: None,
            drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            verify_mmap_view: false,
            render_out: None,
            out_report: Some(out_report.clone()),
        })
        .unwrap();

        assert_eq!(report.performance.frames_processed, 3);
        assert_eq!(report.offline_assets.hd_downloads, 0);
        assert_eq!(report.capture_coverage.served.region_bgra, 3);
        assert_eq!(report.capture_coverage.unserved.close_or_slider_probe, 3);
        assert!(!report.accuracy.has_verified_gt);
        assert!(out_report.is_file());
    }

    #[test]
    fn drift_streak_and_pct_over_threshold_expose_sustained_offset_hidden_by_median() {
        let mk_rec = |i: usize, wx: f64, m: &str| MetaRecord {
            idx: i,
            ts: i as f64 * 0.05,
            frame_w: 2560,
            frame_h: 1440,
            cursor_client: None,
            capture_path: None,
            base_state: Some("TRACKING".to_owned()),
            base_match: Some(m.to_owned()),
            base_map_id: Some(2),
            base_world_x: Some(wx),
            base_world_y: Some(0.0),
            base_heading: Some(90.0),
        };
        let mk_obs = |i: usize, wx: f64| ObservedFrame {
            idx: i,
            ts: i as f64 * 0.05,
            state: "TRACKING".to_owned(),
            match_method: "hd_sobel_fast".to_owned(),
            map_id: Some(2),
            world_x: Some(wx),
            world_y: Some(0.0),
            heading: Some(90.0),
            body_heading: None,
            camera_heading: None,
            is_underground: false,
        };
        // 6 active frames with 0.5w delta, then 4 pause frames with 21.5w delta -> median stays 0.5w,
        // while frames_over_threshold=4 (40%) and longest_drift_streak=4 (frames 7..10) catch it!
        let mut recs = Vec::new();
        let mut obs = Vec::new();
        for i in 1..=6 {
            recs.push(mk_rec(i, 100.0, "hd_sobel_fast"));
            obs.push(mk_obs(i, 100.5));
        }
        for i in 7..=10 {
            recs.push(mk_rec(i, 121.5, "paimon_pause"));
            obs.push(mk_obs(i, 100.0));
        }

        let reg = compute_regression(&recs, &obs, 15.0);
        assert_eq!(reg.all_frames.median_pos_delta_w, Some(0.5));
        assert_eq!(reg.all_frames.frames_over_threshold, 4);
        assert_eq!(reg.all_frames.pct_over_threshold, 40.0);
        assert_eq!(reg.all_frames.longest_drift_streak.frames, 4);
        assert_eq!(reg.all_frames.longest_drift_streak.start_frame, Some(7));
        assert_eq!(reg.all_frames.longest_drift_streak.end_frame, Some(10));
        assert_eq!(reg.active_baseline_frames.compared_pos_frames, 6);
        assert_eq!(reg.active_baseline_frames.frames_over_threshold, 0);
    }

    #[test]
    #[ignore = "requires GH_FFMPEG absolute path to ffmpeg.exe"]
    fn session_recorder_and_replay_lossless_ffmpeg_roundtrip() {
        let ffmpeg_raw = std::env::var_os("GH_FFMPEG")
            .expect("GH_FFMPEG must be set to an existing absolute ffmpeg.exe path");
        let ffmpeg_abs = PathBuf::from(ffmpeg_raw);
        assert!(
            ffmpeg_abs.is_absolute() && ffmpeg_abs.is_file(),
            "GH_FFMPEG must be an existing absolute file: {}",
            ffmpeg_abs.display()
        );

        use gh_app::session_recorder::{
            SessionCodec, SessionFrameMeta, SessionRecorder, SessionRecorderConfig,
        };
        use gh_core::multimap::MultimapDb;

        let sandbox = TempSandboxDir::new("gh_sim_ffmpeg_rt").unwrap();
        let data_dir = sandbox.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let rec = SessionRecorder::new(
            SessionRecorderConfig {
                seconds: 0.2,
                fps: 20.0,
                codec: SessionCodec::Libx264Rgb,
                ffmpeg_path: ffmpeg_abs.clone(),
                out_root: sandbox.0.join("sessions"),
                tiles_dir: sandbox.0.join("src_tiles"),
                ug_overlays_dir: sandbox.0.join("src_ug"),
                paimon_cache_path: sandbox.0.join("paimon_tpl.npz"),
                maps_db: Arc::new(MultimapDb::default()),
                max_queue: 16,
            },
            None,
        )
        .unwrap();

        let (w, h) = (1280u32, 720u32);
        let mut expected_px = Vec::new();
        for i in 0..4usize {
            let mut bgr = FrameBuf::new_zeroed(w, h, PixelFormat::Bgr8);
            let pix = [(17 * i + 10) as u8, (31 * i + 20) as u8, (47 * i + 30) as u8];
            expected_px.push(pix);
            for chunk in bgr.data.as_chunks_mut::<3>().0 {
                *chunk = pix;
            }
            let mut extra = serde_json::Map::new();
            extra.insert("state".to_owned(), json!("UNLOCALIZED"));
            rec.record_hotkey_event(i as f64 * 0.05, "nav_next");
            rec.add_frame(
                &bgr,
                i as f64 * 0.05,
                SessionFrameMeta {
                    client_rect: RectI32 {
                        x: 0,
                        y: 0,
                        width: w as i32,
                        height: h as i32,
                    },
                    cursor_client_pos: Some((120.5 + i as f64, 240.0)),
                    capture_path: "gdi".to_owned(),
                    extra,
                },
            );
        }
        rec.join_timeout(Duration::from_secs(15));

        let sess_dir = rec.out_dir().to_path_buf();
        assert!(sess_dir.join("video.mkv").is_file());
        assert!(sess_dir.join("meta.jsonl").is_file());
        assert!(sess_dir.join("info.json").is_file());
        assert!(sess_dir.join("assets_cache").join("tiles").is_dir());
        assert!(sess_dir.join("assets_cache").join("ug_overlays").is_dir());

        let info: Value = serde_json::from_str(
            &std::fs::read_to_string(sess_dir.join("info.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(info["frames"], json!(4));
        assert_eq!(info["dropped_total"], json!(0));
        assert_eq!(info["write_errors"], json!(0));
        assert_eq!(info["ffmpeg_exit_code"], json!(0));
        assert_eq!(info["capture_paths_seen"], json!(["gdi"]));

        // Verify exact bit-lossless BGR decoding via VideoPipeDecoder
        let mut dec =
            VideoPipeDecoder::spawn(&ffmpeg_abs, &sess_dir.join("video.mkv"), w, h).unwrap();
        for (idx, exp) in expected_px.iter().enumerate() {
            let f = dec.read_next_frame().unwrap().expect("decoded frame");
            assert_eq!(
                &f.data[0..3],
                exp,
                "lossless RGB pixel mismatch at frame {idx}"
            );
        }
        assert!(dec.read_next_frame().unwrap().is_none());

        // Replay full-frame lossless video through production run_tick_worker + apply_tick_output
        let rep = run_replay(&ReplayOptions {
            source_dir: sess_dir,
            data_dir,
            gt_file: sandbox.0.join("verified_gt.json"),
            tiles_dir: None,
            ug_dir: None,
            paimon_cache_path: None,
            ffmpeg_path: Some(ffmpeg_abs),
            max_frames: None,
            drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            verify_mmap_view: false,
            render_out: None,
            out_report: None,
        })
        .unwrap();
        assert_eq!(rep.session_kind, SessionKind::VideoSession);
        assert_eq!(rep.performance.frames_processed, 4);
        assert_eq!(rep.capture_coverage.unserved.total(), 0);
        assert_eq!(rep.offline_assets.hd_downloads, 0);
    }
}
