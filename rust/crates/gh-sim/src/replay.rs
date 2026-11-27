//! Synchronous deterministic replay driver executing production `gh_app::tracking_stack::build_tracking_stack`,
//! `gh_app::tracking_loop::run_tick_worker`, `prepare_tick_locate`, and `apply_tick_output` against
//! recorded frames (`minimap_rec`, full-frame lossless video sessions, or `diag_*` bundles).

use crate::capturer::{CaptureCoverageStats, ReplayCapturer};
use crate::compositor::{
    OfflineRadarAssets, composite_radar_onto_frame, save_bgra_png,
};
use crate::timeline::{
    InputReplayReport, InputTimeline, dispatch_timeline_input, load_web_inputs_jsonl,
    record_nav_comparison,
};
use crate::verified_gt::{SessionCheckpointGt, VerifiedGtFile};
use gh_app::BUILD_GIT_COMMIT;
use gh_app::callbacks::AppServerCallbacks;
use gh_app::capture_backend::FrameCapturer;
use gh_app::diagnostics::{DiagnosticManager, MinimapPreLostRingBuffer};
use gh_app::log_buffer::LogRingBuffer;
use gh_app::mmap_view::{LocatorRegistrar, MMapViewEngine};
use gh_app::session_recorder::{NavFrameSnapshot, WEB_INPUTS_FILENAME};
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
use gh_core::provenance::{
    DATASET_FILENAMES, ReplayProvenance, SessionProvenance, compare_git_commits,
    fingerprint_data_dir,
};
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
pub const RECORDING_TICK_MODES: [&str; 6] = [
    "minimap",
    "minimap_fast",
    "map_m",
    "map_m_settling",
    "map_m_paused",
    "idle_closed",
];

/// Informational summary of live recording tick fidelity parsed from `meta.jsonl`
/// (`tick_seq` and per-mode `unrecorded_ticks`).
///
/// `None` on [`ReplayReport`] when the recording predates `tick_seq` (treated as unknown, never zero).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingFidelityReport {
    pub frames_with_tick_seq: usize,
    pub first_tick_seq: Option<u64>,
    pub last_tick_seq: Option<u64>,
    pub total_unrecorded_ticks: u64,
    pub unrecorded_by_mode: BTreeMap<String, u64>,
}

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
    #[serde(default)]
    pub provenance: ReplayProvenance,
    #[serde(default)]
    pub recording_fidelity: Option<RecordingFidelityReport>,
    #[serde(default)]
    pub input_replay: InputReplayReport,
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
    tick_seq: Option<u64>,
    unrecorded_ticks: Option<BTreeMap<String, u64>>,
    hotkeys: Vec<(f64, String)>,
    live_nav: Option<NavFrameSnapshot>,
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
        let tick_seq = v.get("tick_seq").and_then(Value::as_u64);
        let unrecorded_ticks = v.get("unrecorded_ticks").and_then(Value::as_object).map(|obj| {
            let mut counts = BTreeMap::new();
            for (k, val) in obj {
                if let Some(n) = val.as_u64() {
                    counts.insert(k.clone(), n);
                }
            }
            counts
        });
        let hotkeys: Vec<(f64, String)> = v
            .get("hotkeys")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let hk_ts = item.get("ts").and_then(Value::as_f64).filter(|t| t.is_finite())?;
                        let act = item.get("action").and_then(Value::as_str)?;
                        Some((hk_ts, act.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let live_nav = v
            .get("nav")
            .filter(|nv| !nv.is_null())
            .and_then(|nv| serde_json::from_value::<NavFrameSnapshot>(nv.clone()).ok());
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
            tick_seq,
            unrecorded_ticks,
            hotkeys,
            live_nav,
        });
    }
    Ok(out)
}

fn summarize_recording_fidelity(records: &[MetaRecord]) -> Option<RecordingFidelityReport> {
    let mut frames_with_tick_seq = 0usize;
    let mut first_tick_seq: Option<u64> = None;
    let mut last_tick_seq: Option<u64> = None;
    for rec in records {
        if let Some(seq) = rec.tick_seq {
            frames_with_tick_seq += 1;
            if first_tick_seq.is_none() {
                first_tick_seq = Some(seq);
            }
            last_tick_seq = Some(seq);
        }
    }
    if frames_with_tick_seq == 0 {
        return None;
    }

    let mut unrecorded_by_mode: BTreeMap<String, u64> = RECORDING_TICK_MODES
        .iter()
        .map(|&m| (m.to_owned(), 0u64))
        .collect();
    let mut total_unrecorded_ticks = 0u64;
    for rec in records {
        if let Some(ref map) = rec.unrecorded_ticks {
            for (mode, &count) in map {
                *unrecorded_by_mode.entry(mode.clone()).or_insert(0) += count;
                total_unrecorded_ticks += count;
            }
        }
    }
    Some(RecordingFidelityReport {
        frames_with_tick_seq,
        first_tick_seq,
        last_tick_seq,
        total_unrecorded_ticks,
        unrecorded_by_mode,
    })
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
    let offline_catalog = HoYoLABLabelCatalog::new_offline();
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
        radar_screen_offset: gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        pin_icon_cache: PinIconCache::new_offline(
            paths.cache_dir.join("lan_icons"),
            Arc::new(offline_catalog.clone()),
            Duration::from_secs(1),
        ),
        hud_toast: Arc::new(HudToastOverlay::with_screen_offset(
            gh_hud::layered::OFFSCREEN_TEST_OFFSET,
        )),
        audio: Arc::new(AudioFeedback::new(false, 0)),
        tray: Mutex::new(None),
        hotkey_mgr: Arc::new(UnifiedHotkeyManager::new(false, None)),
        conflict_advisor: Arc::new(HotkeyConflictAdvisor::new()),
        guide_fetcher: PinGuideFetcher::new_offline(Arc::new(get_language)),
        label_catalog: offline_catalog,
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

/// Reads the optional `provenance` object from `<source_dir>/info.json` (`None` for old sessions).
pub fn read_session_provenance(source_dir: &Path) -> Option<SessionProvenance> {
    let raw = std::fs::read_to_string(source_dir.join("info.json")).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let prov = val.get("provenance")?;
    if prov.is_null() {
        return None;
    }
    serde_json::from_value::<SessionProvenance>(prov.clone()).ok()
}

/// Resolves which dataset directory replay loads: `<source_dir>/assets_cache/data` when
/// the map database is present there, otherwise falls back to `--data-dir`.
pub fn resolve_replay_data_dir(
    source_dir: &Path,
    fallback_data_dir: &Path,
) -> (PathBuf, &'static str) {
    let session_data_dir = source_dir.join("assets_cache").join("data");
    if session_data_dir.join(DATASET_FILENAMES[0]).is_file() {
        (session_data_dir, "session")
    } else {
        (fallback_data_dir.to_path_buf(), "data_dir")
    }
}

/// Builds the replay report provenance block comparing recorded session metadata against the
/// dataset directory and build commit actually used during replay.
pub fn build_replay_provenance(
    recorded: Option<SessionProvenance>,
    replay_data_source: &str,
    effective_data_dir: &Path,
    current_commit: &str,
) -> ReplayProvenance {
    let replay_datasets = fingerprint_data_dir(effective_data_dir);
    let datasets_match = recorded
        .as_ref()
        .map(|rec| rec.datasets.matches(&replay_datasets));
    let same_commit = recorded
        .as_ref()
        .and_then(|rec| compare_git_commits(&rec.git_commit, current_commit));
    ReplayProvenance {
        recorded,
        replay_data_source: replay_data_source.to_owned(),
        replay_datasets,
        datasets_match,
        same_commit,
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

    let (effective_data_dir, replay_data_source) =
        resolve_replay_data_dir(&opts.source_dir, &opts.data_dir);
    let recorded_provenance = read_session_provenance(&opts.source_dir);
    let provenance = build_replay_provenance(
        recorded_provenance,
        replay_data_source,
        &effective_data_dir,
        BUILD_GIT_COMMIT,
    );

    let enable_mmap_view = opts.verify_mmap_view || opts.render_out.is_some();
    let capturer = Arc::new(ReplayCapturer::new());
    let (state, tile_cache, _cfg_init, relocator_ready) = build_offline_replay_state(
        &sandbox.0,
        &effective_data_dir,
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
                tick_seq: None,
                unrecorded_ticks: None,
                hotkeys: Vec::new(),
                live_nav: None,
            }];
        }
    }

    if let Some(limit) = opts.max_frames {
        records.truncate(limit);
    }

    let mut web_inputs = load_web_inputs_jsonl(&opts.source_dir.join(WEB_INPUTS_FILENAME))?;
    if opts.max_frames.is_some() {
        let cutoff_ts = records.last().map_or(f64::NEG_INFINITY, |r| r.ts);
        web_inputs.retain(|w| w.ts <= cutoff_ts);
    }
    let hotkeys: Vec<(f64, String)> = records
        .iter()
        .flat_map(|r| r.hotkeys.iter().cloned())
        .collect();
    let mut timeline = InputTimeline::from_inputs(web_inputs, hotkeys);
    let callbacks = AppServerCallbacks::new_offline_replay(Arc::clone(&state));
    let mut input_replay = InputReplayReport::default();

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

        for ev in timeline.drain_before_frame(rec.ts) {
            dispatch_timeline_input(&state, &callbacks, &ev, &mut input_replay);
        }
        let cfg_snap = state.config.read().expect("config lock poisoned").clone();

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
            session_rec_active: false,
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

        if let Some(ref live_nav) = rec.live_nav {
            let replay_nav = state.snapshot_nav_state();
            record_nav_comparison(rec.idx, live_nav, &replay_nav, &mut input_replay);
        }

        tick_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        observed.push(obs);
    }

    for ev in timeline.drain_remaining() {
        dispatch_timeline_input(&state, &callbacks, &ev, &mut input_replay);
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
    let recording_fidelity = summarize_recording_fidelity(&records[..frames_processed]);
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
        provenance,
        recording_fidelity,
        input_replay,
    };

    state.shutdown_radar();
    state.nav_hud.close();
    state.hud_toast.close();
    state.mmap_overlay.close();
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
        // Legacy session without provenance or assets_cache/data falls back to --data-dir
        assert_eq!(report.provenance.recorded, None);
        assert_eq!(report.provenance.replay_data_source, "data_dir");
        assert_eq!(report.provenance.datasets_match, None);
        assert_eq!(report.provenance.same_commit, None);
        assert_eq!(report.recording_fidelity, None);
        assert!(
            report
                .provenance
                .replay_datasets
                .get(DATASET_FILENAMES[0])
                .is_some()
        );
    }

    #[test]
    fn replay_uses_session_datasets_when_present_and_tracks_datasets_match() {
        let sandbox = TempSandboxDir::new("gh_sim_replay_prov").unwrap();
        let fallback_data_dir = sandbox.0.join("fallback_data");
        std::fs::create_dir_all(&fallback_data_dir).unwrap();
        write_json_atomic(
            &fallback_data_dir.join(DATASET_FILENAMES[0]),
            &json!({"version": "4.0.0", "maps": {}, "tag": "fallback"}),
        )
        .unwrap();

        let mk_session = |name: &str| {
            let sdir = sandbox.0.join(name);
            std::fs::create_dir_all(sdir.join("frames")).unwrap();
            let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
            for i in 1..=1usize {
                img.save(sdir.join("frames").join(format!("{i:06}.png")))
                    .unwrap();
            }
            std::fs::write(
                sdir.join("meta.jsonl"),
                "{\"i\":1,\"ts\":10.0,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n",
            )
            .unwrap();
            sdir
        };

        // 1. Session with assets_cache/data + matching provenance -> loads from "session", datasets_match = Some(true)
        let s_hermetic = mk_session("sess_hermetic");
        let sess_data = s_hermetic.join("assets_cache").join("data");
        std::fs::create_dir_all(&sess_data).unwrap();
        write_json_atomic(
            &sess_data.join(DATASET_FILENAMES[0]),
            &json!({"version": "4.0.0", "maps": {}, "tag": "session_override"}),
        )
        .unwrap();
        let sess_fp = fingerprint_data_dir(&sess_data);
        let fallback_fp = fingerprint_data_dir(&fallback_data_dir);
        assert_ne!(sess_fp, fallback_fp);
        write_json_atomic(
            &s_hermetic.join("info.json"),
            &json!({
                "schema_version": 1,
                "provenance": {
                    "app_version": gh_app::BUILD_APP_VERSION,
                    "git_commit": BUILD_GIT_COMMIT,
                    "data_source": "installed",
                    "datasets": sess_fp,
                }
            }),
        )
        .unwrap();

        let rep_hermetic = run_replay(&ReplayOptions {
            source_dir: s_hermetic,
            data_dir: fallback_data_dir.clone(),
            gt_file: sandbox.0.join("verified_gt.json"),
            tiles_dir: None,
            ug_dir: None,
            paimon_cache_path: None,
            ffmpeg_path: None,
            max_frames: None,
            drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            verify_mmap_view: false,
            render_out: None,
            out_report: None,
        })
        .unwrap();
        assert_eq!(rep_hermetic.provenance.replay_data_source, "session");
        assert_eq!(rep_hermetic.provenance.replay_datasets, sess_fp);
        assert_eq!(rep_hermetic.provenance.datasets_match, Some(true));
        if !BUILD_GIT_COMMIT.eq_ignore_ascii_case("unknown") {
            assert_eq!(rep_hermetic.provenance.same_commit, Some(true));
        }

        // 2. Session WITHOUT assets_cache/data whose recorded provenance had different datasets & commit ->
        // falls back to --data-dir, datasets_match = Some(false), same_commit = Some(false) (unless build commit unknown)
        let s_mismatch = mk_session("sess_mismatch");
        write_json_atomic(
            &s_mismatch.join("info.json"),
            &json!({
                "schema_version": 1,
                "provenance": {
                    "app_version": "5.7.0",
                    "git_commit": "0000000000000000000000000000000000000000",
                    "data_source": "installed",
                    "datasets": sess_fp,
                }
            }),
        )
        .unwrap();

        let rep_mismatch = run_replay(&ReplayOptions {
            source_dir: s_mismatch,
            data_dir: fallback_data_dir,
            gt_file: sandbox.0.join("verified_gt.json"),
            tiles_dir: None,
            ug_dir: None,
            paimon_cache_path: None,
            ffmpeg_path: None,
            max_frames: None,
            drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            verify_mmap_view: false,
            render_out: None,
            out_report: None,
        })
        .unwrap();
        assert_eq!(rep_mismatch.provenance.replay_data_source, "data_dir");
        assert_eq!(rep_mismatch.provenance.replay_datasets, fallback_fp);
        assert_eq!(rep_mismatch.provenance.datasets_match, Some(false));
        if !BUILD_GIT_COMMIT.eq_ignore_ascii_case("unknown") {
            assert_eq!(rep_mismatch.provenance.same_commit, Some(false));
        }
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
            tick_seq: None,
            unrecorded_ticks: None,
            hotkeys: Vec::new(),
            live_nav: None,
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
                data_dir: data_dir.clone(),
                data_source: "dev".to_owned(),
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
                false,
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
        let fid = rep
            .recording_fidelity
            .as_ref()
            .expect("recording_fidelity must be populated for newly recorded session");
        assert_eq!(fid.frames_with_tick_seq, 4);
        assert_eq!(fid.first_tick_seq, Some(1));
        assert_eq!(fid.last_tick_seq, Some(4));
        assert_eq!(fid.total_unrecorded_ticks, 0);
        assert_eq!(rep.input_replay.hotkeys_recorded, 4);
        assert_eq!(rep.input_replay.hotkeys_dispatched, 4);
        assert_eq!(rep.input_replay.hotkeys_skipped, 0);
        assert_eq!(
            rep.input_replay.hotkeys_dispatched_by_action.get("nav_next"),
            Some(&4)
        );
    }

    #[test]
    fn recording_fidelity_parses_tick_seq_and_unrecorded_ticks_and_treats_legacy_as_none() {
        let sandbox = TempSandboxDir::new("gh_sim_fidelity_meta").unwrap();

        // 1. Legacy meta without tick_seq -> None (unknown, never zero)
        let legacy_path = sandbox.0.join("meta_legacy.jsonl");
        std::fs::write(
            &legacy_path,
            concat!(
                "{\"i\":1,\"ts\":1.0,\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"minimap\"}\n",
                "{\"i\":2,\"ts\":1.05,\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"map_m\"}\n"
            ),
        )
        .unwrap();
        let legacy_recs = load_meta_jsonl(&legacy_path).unwrap();
        assert_eq!(summarize_recording_fidelity(&legacy_recs), None);

        // 2. New meta with tick_seq + unrecorded_ticks (mixing empty objects and sparse mode keys)
        let new_path = sandbox.0.join("meta_new.jsonl");
        std::fs::write(
            &new_path,
            concat!(
                "{\"i\":1,\"ts\":1.0,\"tick_seq\":1,\"unrecorded_ticks\":{},\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"minimap\"}\n",
                "{\"i\":2,\"ts\":1.05,\"tick_seq\":4,\"unrecorded_ticks\":{\"minimap_fast\":2},\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"minimap\"}\n",
                "{\"i\":3,\"ts\":1.10,\"tick_seq\":7,\"unrecorded_ticks\":{\"minimap\":1,\"minimap_fast\":1,\"map_m\":1},\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"map_m\"}\n",
                "{\"i\":4,\"ts\":1.125,\"tick_seq\":8,\"unrecorded_ticks\":{},\"frame_w\":2560,\"frame_h\":1440,\"mode\":\"map_m\"}\n"
            ),
        )
        .unwrap();
        let new_recs = load_meta_jsonl(&new_path).unwrap();
        let summary = summarize_recording_fidelity(&new_recs).expect("should parse fidelity");
        assert_eq!(summary.frames_with_tick_seq, 4);
        assert_eq!(summary.first_tick_seq, Some(1));
        assert_eq!(summary.last_tick_seq, Some(8));
        assert_eq!(summary.total_unrecorded_ticks, 5);
        assert_eq!(summary.unrecorded_by_mode.get("minimap"), Some(&1));
        assert_eq!(summary.unrecorded_by_mode.get("minimap_fast"), Some(&3));
        assert_eq!(summary.unrecorded_by_mode.get("map_m"), Some(&1));
        assert_eq!(summary.unrecorded_by_mode.get("map_m_settling"), Some(&0));
        assert_eq!(summary.unrecorded_by_mode.get("map_m_paused"), Some(&0));
        assert_eq!(summary.unrecorded_by_mode.get("idle_closed"), Some(&0));
    }

    #[test]
    fn e2e_replay_dispatches_hotkeys_and_web_inputs_with_nav_parity_and_seeded_fault() {
        use gh_app::session_recorder::{
            SessionCodec, SessionRecorder, SessionRecorderConfig, write_web_inputs_jsonl,
        };
        use gh_core::traits::ServerCallbacks;

        let sandbox = TempSandboxDir::new("gh_sim_e2e_input_replay").unwrap();
        let data_dir = sandbox.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let session_dir = sandbox.0.join("sess_synth_nav");
        std::fs::create_dir_all(session_dir.join("frames")).unwrap();
        let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
        for i in 1..=4usize {
            img.save(session_dir.join("frames").join(format!("{i:06}.png")))
                .unwrap();
        }

        // Build a live-equivalent recording state to generate ground-truth `meta.jsonl` (`nav` +
        // `hotkeys`) and sanitized `web_inputs.jsonl` through the real production callbacks,
        // shared `dispatch_hotkey_action`, `run_tick_worker`, and `apply_tick_output`.
        let rec_sandbox = sandbox.0.join("rec_state");
        std::fs::create_dir_all(&rec_sandbox).unwrap();
        let rec_capturer = Arc::new(ReplayCapturer::new());
        let (rec_state, _, _, _) = build_offline_replay_state(
            &rec_sandbox,
            &data_dir,
            rec_sandbox.join("tiles"),
            rec_sandbox.join("ug"),
            rec_sandbox.join("paimon.npz"),
            Arc::clone(&rec_capturer),
            false,
        )
        .unwrap();
        let srec = SessionRecorder::new(
            SessionRecorderConfig {
                seconds: 0.0,
                fps: 20.0,
                codec: SessionCodec::Libx264Rgb,
                ffmpeg_path: std::env::current_exe().unwrap(),
                out_root: rec_sandbox.join("out"),
                tiles_dir: rec_sandbox.join("tiles"),
                ug_overlays_dir: rec_sandbox.join("ug"),
                paimon_cache_path: rec_sandbox.join("paimon.npz"),
                data_dir: data_dir.clone(),
                data_source: "dev".to_owned(),
                maps_db: Arc::clone(&rec_state.maps_db),
                max_queue: 16,
            },
            None,
        )
        .unwrap();
        *rec_state.session_recorder.lock().unwrap() = Some(Arc::clone(&srec));

        let rec_cb = AppServerCallbacks::new_offline_replay(Arc::clone(&rec_state));
        let mut rec_rt = LoopRuntimeState::new();
        let mut meta_rows: Vec<Value> = Vec::new();
        let mut web_records = Vec::new();

        let step_live_frame = |idx: usize,
                               ts: f64,
                               hotkeys: Vec<Value>,
                               rec_rt: &mut LoopRuntimeState|
         -> Value {
            let png = load_png_as_bgra(&session_dir.join("frames").join(format!("{idx:06}.png")))
                .unwrap();
            rec_capturer.set_frame(Some(png), 0, 0, 2560, 1440, false);
            let cfg_snap = rec_state.config.read().unwrap().clone();
            let out = run_tick_worker(
                &rec_state,
                TickParams {
                    game_hwnd: 0,
                    cl: 0,
                    ct: 0,
                    cw: 2560,
                    ch: 1440,
                    is_closed: true,
                    do_auto_sync: true,
                    do_minimap: true,
                    fast_ok: false,
                    guide_view: false,
                    guide_map: 2,
                    want_window_capture: false,
                    now_s: Some((ts - 10.05).max(0.0)),
                    cursor_client: None,
                    capture_session_frame: false,
                    session_rec_active: false,
                },
            );
            let post_ctx = PostTickContext {
                game_rect: RectI32 {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
                route_needs_mmap: false,
                overlay_mode: MMapOverlayMode::Off,
                session_rec_active: false,
                enable_ui_side_effects: false,
            };
            let mut noop = || {};
            let _ = apply_tick_output(
                &rec_state,
                rec_rt,
                &cfg_snap,
                &post_ctx,
                out,
                None,
                &mut noop,
            );
            let nav_snap = rec_state.snapshot_nav_state();
            let mut row = json!({
                "i": idx,
                "ts": ts,
                "frame_w": 2560,
                "frame_h": 1440,
                "state": "UNLOCALIZED",
                "nav": nav_snap,
            });
            if !hotkeys.is_empty() {
                row["hotkeys"] = Value::Array(hotkeys);
            }
            row
        };

        // Before frame 1: deliver pins (with sensitive key stripped) + press nav_next (101 -> 102)
        assert_eq!(
            rec_cb
                .on_nearby_pins(&json!({
                    "map_id": 2,
                    "cookie": "drop_me",
                    "pins": [
                        {"id": "101", "lat": 100.0, "lng": 200.0, "name": "Chest A"},
                        {"id": "102", "lat": 120.0, "lng": 220.0, "name": "Chest B"}
                    ]
                }))
                .unwrap(),
            2
        );
        for mut w in srec.drain_pending_web_inputs() {
            assert!(w.payload.get("cookie").is_none());
            w.ts = 10.01;
            web_records.push(w);
        }
        gh_app::dispatch_hotkey_action(&rec_state, "nav_next");
        let r1 = step_live_frame(
            1,
            10.05,
            vec![json!({"ts": 10.02, "action": "nav_next"})],
            &mut rec_rt,
        );
        assert_eq!(r1["nav"]["target_id"], json!("102"));
        assert_eq!(r1["nav"]["user_locked_id"], json!("102"));
        assert_eq!(r1["nav"]["locked_target_id"], Value::Null);
        meta_rows.push(r1);

        // Before frame 2: lock single pin 103 via on_nav_command (auto_switch suppressed)
        let _ = rec_cb
            .on_nav_command(&json!({
                "action": "lock_target",
                "pin": {"id": "103", "lat": 50.0, "lng": 60.0, "map_id": 2, "name": "Oculus C"},
                "auto_switch": true,
                "token": "drop_token"
            }))
            .unwrap();
        for mut w in srec.drain_pending_web_inputs() {
            assert!(w.payload.get("token").is_none());
            w.ts = 10.07;
            web_records.push(w);
        }
        let r2 = step_live_frame(2, 10.10, vec![], &mut rec_rt);
        assert_eq!(r2["nav"]["target_id"], json!("103"));
        assert_eq!(r2["nav"]["locked_target_id"], json!("103"));
        meta_rows.push(r2);

        // Before frame 3: nav_unlock clears pin 103 lock (returns to 102); also include skipped `debug`
        gh_app::dispatch_hotkey_action(&rec_state, "nav_unlock");
        let r3 = step_live_frame(
            3,
            10.15,
            vec![
                json!({"ts": 10.12, "action": "nav_unlock"}),
                json!({"ts": 10.13, "action": "debug"}),
            ],
            &mut rec_rt,
        );
        assert_eq!(r3["nav"]["target_id"], json!("102"));
        assert_eq!(r3["nav"]["locked_target_id"], Value::Null);
        meta_rows.push(r3);

        // Before frame 4: nav_guide exercises offline PinGuideFetcher without making requests
        gh_app::dispatch_hotkey_action(&rec_state, "nav_guide");
        let r4 = step_live_frame(
            4,
            10.20,
            vec![json!({"ts": 10.17, "action": "nav_guide"})],
            &mut rec_rt,
        );
        assert_eq!(r4["nav"]["target_id"], json!("102"));
        meta_rows.push(r4);

        rec_state.shutdown_radar();
        rec_state.nav_hud.close();
        rec_state.hud_toast.close();
        rec_state.mmap_overlay.close();
        rec_state.capturer.close();
        rec_state.hd_provider.close();
        rec_state.ug_provider.close();
        rec_state.audio.close();
        rec_state.guide_fetcher.stop();

        let write_meta = |rows: &[Value]| {
            let mut s = String::new();
            for r in rows {
                s.push_str(&serde_json::to_string(r).unwrap());
                s.push('\n');
            }
            std::fs::write(session_dir.join("meta.jsonl"), s).unwrap();
        };
        write_meta(&meta_rows);
        write_web_inputs_jsonl(&session_dir.join(WEB_INPUTS_FILENAME), &web_records).unwrap();

        let run_case = || {
            run_replay(&ReplayOptions {
                source_dir: session_dir.clone(),
                data_dir: data_dir.clone(),
                gt_file: sandbox.0.join("verified_gt.json"),
                tiles_dir: None,
                ug_dir: None,
                paimon_cache_path: None,
                ffmpeg_path: None,
                max_frames: None,
                drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
                verify_mmap_view: false,
                render_out: None,
                out_report: None,
            })
            .unwrap()
        };

        // 1. Happy path: all replayable hotkeys dispatched, debug skipped, web inputs delivered, 0 nav mismatches
        let rep_ok = run_case();
        assert_eq!(rep_ok.offline_assets.hd_downloads, 0);
        assert_eq!(rep_ok.input_replay.hotkeys_recorded, 4);
        assert_eq!(rep_ok.input_replay.hotkeys_dispatched, 3);
        assert_eq!(rep_ok.input_replay.hotkeys_skipped, 1);
        assert_eq!(
            rep_ok.input_replay.hotkeys_dispatched_by_action.get("nav_next"),
            Some(&1)
        );
        assert_eq!(
            rep_ok.input_replay.hotkeys_dispatched_by_action.get("nav_unlock"),
            Some(&1)
        );
        assert_eq!(
            rep_ok.input_replay.hotkeys_dispatched_by_action.get("nav_guide"),
            Some(&1)
        );
        assert_eq!(
            rep_ok.input_replay.hotkeys_skipped_by_action.get("debug"),
            Some(&1)
        );
        assert_eq!(rep_ok.input_replay.web_inputs_delivered, 2);
        assert_eq!(
            rep_ok.input_replay.web_inputs_by_callback.get("on_nearby_pins"),
            Some(&1)
        );
        assert_eq!(
            rep_ok.input_replay.web_inputs_by_callback.get("on_nav_command"),
            Some(&1)
        );
        assert_eq!(rep_ok.input_replay.nav_frames_compared, 4);
        assert_eq!(rep_ok.input_replay.nav_mismatches, 0);

        // 2. Seeded fault A: remove web_inputs.jsonl -> replay has no pins/lock -> nav_mismatches > 0
        std::fs::remove_file(session_dir.join(WEB_INPUTS_FILENAME)).unwrap();
        let rep_no_web = run_case();
        assert_eq!(rep_no_web.input_replay.web_inputs_delivered, 0);
        assert_eq!(rep_no_web.input_replay.nav_frames_compared, 4);
        assert_eq!(rep_no_web.input_replay.nav_mismatches, 4);

        // 3. Seeded fault B: restore web_inputs.jsonl, drop `nav_unlock` hotkey -> frames 3 & 4 stay locked on 103
        write_web_inputs_jsonl(&session_dir.join(WEB_INPUTS_FILENAME), &web_records).unwrap();
        let mut rows_no_unlock = meta_rows.clone();
        rows_no_unlock[2]["hotkeys"] = json!([{"ts": 10.13, "action": "debug"}]);
        write_meta(&rows_no_unlock);
        let rep_no_unlock = run_case();
        assert_eq!(rep_no_unlock.input_replay.nav_frames_compared, 4);
        assert_eq!(rep_no_unlock.input_replay.nav_mismatches, 2);
        assert_eq!(rep_no_unlock.input_replay.first_nav_mismatch_idx, Some(3));

        // 4. Seeded fault C: restore `nav_unlock`, drop `nav_next` hotkey -> frame 1 targets 101 instead of 102
        let mut rows_no_next = meta_rows;
        rows_no_next[0].as_object_mut().unwrap().remove("hotkeys");
        write_meta(&rows_no_next);
        let rep_no_next = run_case();
        assert_eq!(rep_no_next.input_replay.nav_frames_compared, 4);
        assert!(
            rep_no_next.input_replay.nav_mismatches > 0,
            "dropping nav_next must produce nav mismatches"
        );
        assert_eq!(rep_no_next.input_replay.first_nav_mismatch_idx, Some(1));
    }
}
