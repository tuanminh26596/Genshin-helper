//! Synchronous deterministic replay driver executing production `gh_app::tracking_stack::build_tracking_stack`,
//! `gh_app::tracking_loop::run_tick_worker`, `prepare_tick_locate`, and `apply_tick_output` against
//! recorded frames (`minimap_rec`, full-frame lossless video sessions, or `diag_*` bundles).

use crate::capturer::{CaptureCoverageStats, ReplayCapturer};
use crate::compositor::{
    OfflineRadarAssets, composite_radar_onto_frame, save_bgra_png,
};
use crate::timeline::{
    InputReplayReport, InputTimeline, TimelineInput, TimelineInputKind, dispatch_timeline_input,
    load_web_inputs_jsonl, record_nav_comparison_with_carryover,
};
use crate::truth_oracle::{ReplayTruthMeta, TruthSummary, evaluate_truth_oracle};
use crate::verified_gt::{SessionCheckpointGt, VerifiedGtFile};
use gh_app::BUILD_GIT_COMMIT;
use gh_app::callbacks::AppServerCallbacks;
use gh_app::capture_backend::FrameCapturer;
use gh_app::diagnostics::{DiagnosticManager, MinimapPreLostRingBuffer};
use gh_app::log_buffer::LogRingBuffer;
use gh_app::mmap_view::{LocatorRegistrar, MMapViewEngine};
use gh_app::session_recorder::{INITIAL_STATE_CALLBACK, NavFrameSnapshot, WEB_INPUTS_FILENAME};
use gh_app::state::{AppState, GameState, NavMarkConfirm, RouteState, UiFrameSnapshot};
use gh_app::tracking_loop::{
    LoopRuntimeState, PostTickContext, TickMode, TickParams, apply_tick_output,
    prepare_tick_locate, run_tick_worker,
};
use gh_app::tracking_stack::{
    SharedTrackingAssets, TrackingStackOptions, build_tracking_stack,
};
use gh_app::updater::AppUpdater;
use gh_core::atomic_io::write_json_atomic;
use gh_core::config::AppConfig;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::i18n::get_language;
use gh_core::map_bridge::PerMapAffineRegistry;
use gh_core::nav_controller::NavigationController;
use gh_core::paths::{AppPaths, RuntimeMode};
use gh_core::provenance::{
    DATASET_FILENAMES, DatasetFingerprint, ENGINE_PROVENANCE_PATHS, ReplayProvenance,
    SessionProvenance, compare_git_commits, fingerprint_data_dir,
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
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
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
    #[serde(default)]
    pub truth: TruthSummary,
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

pub(crate) struct TempSandboxDir(pub(crate) PathBuf);

static SANDBOX_SEQ: AtomicU64 = AtomicU64::new(0);

impl TempSandboxDir {
    pub(crate) fn new(prefix: &str) -> Result<Self, String> {
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "{prefix}_{}_{seq}_{}",
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
    frame_rx: Option<Receiver<Result<Option<FrameBuf>, String>>>,
    recycle_tx: Option<SyncSender<FrameBuf>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoPipeDecoder {
    fn spawn(
        ffmpeg_abs: &Path,
        video_path: &Path,
        w: u32,
        h: u32,
        ffmpeg_threads: Option<usize>,
    ) -> Result<Self, String> {
        if !ffmpeg_abs.is_absolute() || !ffmpeg_abs.is_file() {
            return Err(format!(
                "--ffmpeg must be an existing absolute file path (never resolved from PATH): {}",
                ffmpeg_abs.display()
            ));
        }
        let mut cmd = Command::new(ffmpeg_abs);
        cmd.args(["-v", "error"]);
        if let Some(threads) = ffmpeg_threads.filter(|&n| n > 0) {
            cmd.args(["-threads", &threads.to_string()]);
        }
        let mut child = cmd
            .args(["-i"])
            .arg(video_path)
            .args(["-f", "rawvideo", "-pix_fmt", "bgr24", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn ffmpeg ({}): {e}", ffmpeg_abs.display()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg stdout pipe missing".to_owned())?;

        let (frame_tx, frame_rx) = sync_channel::<Result<Option<FrameBuf>, String>>(1);
        let (recycle_tx, recycle_rx) = sync_channel::<FrameBuf>(2);
        for _ in 0..2 {
            let _ = recycle_tx.send(FrameBuf::new_zeroed(w, h, PixelFormat::Bgr8));
        }

        let reader_thread = std::thread::Builder::new()
            .name("gh-sim-ffmpeg-read".to_owned())
            .spawn(move || {
                while let Ok(mut buf) = recycle_rx.recv() {
                    let mut filled = 0usize;
                    let res = loop {
                        if filled >= buf.data.len() {
                            break Ok(Some(buf));
                        }
                        match stdout.read(&mut buf.data[filled..]) {
                            Ok(0) => {
                                if filled == 0 {
                                    break Ok(None);
                                }
                                break Err(format!(
                                    "truncated video frame: read {filled} of {} bytes",
                                    buf.data.len()
                                ));
                            }
                            Ok(n) => filled += n,
                            Err(e) => break Err(format!("ffmpeg pipe read error: {e}")),
                        }
                    };
                    let done = !matches!(&res, Ok(Some(_)));
                    if frame_tx.send(res).is_err() || done {
                        break;
                    }
                }
            })
            .map_err(|e| format!("failed to spawn ffmpeg reader thread: {e}"))?;

        Ok(Self {
            child,
            frame_rx: Some(frame_rx),
            recycle_tx: Some(recycle_tx),
            reader_thread: Some(reader_thread),
        })
    }

    fn read_next_frame(&mut self) -> Result<Option<FrameBuf>, String> {
        let Some(ref rx) = self.frame_rx else {
            return Ok(None);
        };
        match rx.recv() {
            Ok(res) => res,
            Err(_) => Ok(None),
        }
    }

    fn recycle_frame(&self, buf: FrameBuf) {
        if let Some(ref tx) = self.recycle_tx {
            let _ = tx.try_send(buf);
        }
    }
}

impl Drop for VideoPipeDecoder {
    fn drop(&mut self) {
        self.recycle_tx.take();
        self.frame_rx.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
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

struct OfflineReplayState {
    state: Arc<AppState>,
    tile_cache: Arc<LruTerrainTileCache>,
    relocator_ready: bool,
    sim_clock_bits: Arc<AtomicU64>,
}

#[allow(clippy::too_many_arguments)]
fn build_offline_replay_state(
    sandbox_root: &Path,
    data_dir: &Path,
    tiles_dir: PathBuf,
    ug_dir: PathBuf,
    paimon_cache_path: PathBuf,
    capturer: Arc<ReplayCapturer>,
    enable_mmap_view: bool,
    shared_assets: Option<Arc<SharedTrackingAssets>>,
) -> Result<OfflineReplayState, String> {
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
        shared_assets,
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

    let sim_clock_bits = Arc::new(AtomicU64::new(0.0f64.to_bits()));
    let sim_clock_reader = Arc::clone(&sim_clock_bits);
    let nav_ctrl = NavigationController::with_clock(move || {
        f64::from_bits(sim_clock_reader.load(Ordering::Acquire))
    })
    .with_space_debounce_frames(4);

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
        nav_controller: Mutex::new(nav_ctrl),
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
    Ok(OfflineReplayState {
        state,
        tile_cache,
        relocator_ready,
        sim_clock_bits,
    })
}

fn evaluate_session_accuracy_with_gt(
    gt_file: &Path,
    gt: &VerifiedGtFile,
    session_name: &str,
    observed: &[ObservedFrame],
) -> AccuracyReport {
    let Some(sess_gt) = gt.sessions.get(session_name) else {
        return AccuracyReport {
            has_verified_gt: false,
            gt_file: gt_file.display().to_string(),
            note: format!(
                "No human-verified ground truth entry for session '{session_name}' in {}; regression metrics only.",
                gt_file.display()
            ),
            ..Default::default()
        };
    };

    if sess_gt.checkpoints.is_empty() {
        return AccuracyReport {
            has_verified_gt: false,
            gt_file: gt_file.display().to_string(),
            note: format!(
                "Session '{session_name}' has 0 checkpoints in {}.",
                gt_file.display()
            ),
            ..Default::default()
        };
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

    AccuracyReport {
        has_verified_gt: true,
        gt_file: gt_file.display().to_string(),
        note: sess_gt.note.clone(),
        checkpoints_evaluated: details.len(),
        checkpoints_passed: passed_cnt,
        false_lock_count: false_locks,
        median_pos_err_w: percentile(&pos_errs, 0.50).map(round3),
        max_pos_err_w: pos_errs.iter().copied().reduce(f64::max).map(round3),
        details,
    }
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

fn find_git_repo_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn is_git_hex_sha(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 7 && t.len() <= 40 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn query_git_head_in_repo(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?;
    let trimmed = sha.trim();
    if is_git_hex_sha(trimmed) {
        Some(trimmed.to_ascii_lowercase())
    } else {
        None
    }
}

/// Resolves the active git HEAD commit SHA and repository root at runtime, falling back to
/// `build_commit` (compiled `BUILD_GIT_COMMIT`) when not inside a git checkout.
pub fn resolve_current_git_commit_and_root(build_commit: &str) -> (String, Option<PathBuf>) {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    let repo_root = candidates
        .into_iter()
        .find_map(|start| find_git_repo_root(&start));
    if let Some(ref root) = repo_root
        && let Some(runtime_sha) = query_git_head_in_repo(root)
    {
        return (runtime_sha, Some(root.clone()));
    }
    (build_commit.trim().to_owned(), repo_root)
}

/// Returns `Some(true)` if `ENGINE_PROVENANCE_PATHS` have zero git diff between `recorded_commit`
/// and `current_commit`, `Some(false)` if any engine file differs, or `None` when either commit is
/// missing/unresolvable in `repo_root`.
pub fn check_engine_paths_unchanged(
    repo_root: &Path,
    recorded_commit: &str,
    current_commit: &str,
) -> Option<bool> {
    let rec = recorded_commit.trim();
    let cur = current_commit.trim();
    if !is_git_hex_sha(rec) || !is_git_hex_sha(cur) {
        return None;
    }
    if rec.eq_ignore_ascii_case(cur) {
        return Some(true);
    }
    for rev in [rec, cur] {
        let obj = format!("{rev}^{{commit}}");
        let ok = Command::new("git")
            .args(["cat-file", "-e", &obj])
            .current_dir(repo_root)
            .output()
            .ok()
            .is_some_and(|o| o.status.success());
        if !ok {
            return None;
        }
    }

    let mut cmd = Command::new("git");
    cmd.args(["diff", "--name-only", rec, cur, "--"])
        .args(ENGINE_PROVENANCE_PATHS)
        .current_dir(repo_root);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let changed = String::from_utf8_lossy(&out.stdout);
    Some(changed.trim().is_empty())
}

/// Builds the replay report provenance block comparing recorded session metadata against the
/// dataset directory and build commit actually used during replay.
pub fn build_replay_provenance(
    recorded: Option<SessionProvenance>,
    replay_data_source: &str,
    effective_data_dir: &Path,
    current_commit: &str,
) -> ReplayProvenance {
    build_replay_provenance_with_repo(
        recorded,
        replay_data_source,
        effective_data_dir,
        current_commit,
        None,
    )
}

/// Like [`build_replay_provenance`], additionally checking `ENGINE_PROVENANCE_PATHS` via git in
/// `repo_root` when `same_commit == Some(false)`.
pub fn build_replay_provenance_with_repo(
    recorded: Option<SessionProvenance>,
    replay_data_source: &str,
    effective_data_dir: &Path,
    current_commit: &str,
    repo_root: Option<&Path>,
) -> ReplayProvenance {
    let replay_datasets = fingerprint_data_dir(effective_data_dir);
    let datasets_match = recorded
        .as_ref()
        .map(|rec| rec.datasets.matches(&replay_datasets));
    let same_commit = recorded
        .as_ref()
        .and_then(|rec| compare_git_commits(&rec.git_commit, current_commit));
    let engine_code_unchanged = match same_commit {
        Some(true) => Some(true),
        Some(false) => recorded.as_ref().and_then(|rec| {
            repo_root.and_then(|root| {
                check_engine_paths_unchanged(root, &rec.git_commit, current_commit)
            })
        }),
        None => None,
    };
    let effective_cur = if current_commit.trim().is_empty()
        || current_commit.trim().eq_ignore_ascii_case("unknown")
    {
        None
    } else {
        Some(current_commit.trim().to_owned())
    };
    ReplayProvenance {
        recorded,
        replay_data_source: replay_data_source.to_owned(),
        replay_datasets,
        datasets_match,
        same_commit,
        engine_code_unchanged,
        current_commit: effective_cur,
    }
}

/// Optional scripted-scenario hook layered on top of a standard offline replay.
#[derive(Debug, Clone, Default)]
pub struct ReplayScenarioHook {
    /// Additional scripted inputs keyed by 1-based frame index (`rec.idx`), dispatched right before
    /// that frame is processed (merged with the session's recorded inputs).
    pub extra_inputs: Vec<(usize, TimelineInputKind)>,
    /// Stop replaying after processing this 1-based frame index (`rec.idx`).
    pub stop_at_frame: Option<usize>,
    /// Capture `state.snapshot_nav_state()` after every processed frame.
    pub capture_nav_snapshots: bool,
    /// Capture `state.snapshot_ui_state()` after every processed frame.
    pub capture_ui_snapshots: bool,
}

/// Complete output of [`run_replay_with_hook`], including optional per-frame nav/UI snapshots.
#[derive(Debug, Clone)]
pub struct ReplayFullOutput {
    pub report: ReplayReport,
    pub observed: Vec<ObservedFrame>,
    pub map_m_syncs: Vec<MapMSyncRecord>,
    pub nav_snapshots: BTreeMap<usize, NavFrameSnapshot>,
    pub ui_snapshots: BTreeMap<usize, UiFrameSnapshot>,
    pub truth_meta: ReplayTruthMeta,
}

struct ReplayStateCleanupGuard(Arc<AppState>);

impl Drop for ReplayStateCleanupGuard {
    fn drop(&mut self) {
        self.0.shutdown_radar();
        self.0.nav_hud.close();
        self.0.hud_toast.close();
        self.0.mmap_overlay.close();
        self.0.capturer.close();
        self.0.hd_provider.close();
        self.0.ug_provider.close();
        self.0.audio.close();
        self.0.guide_fetcher.stop();
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
    run_replay_with_hook(opts, None).map(|o| (o.report, o.observed, o.map_m_syncs))
}

/// Preloaded read-only datasets, relocator index, ground truth, and git provenance shared
/// across parallel replay sessions in `gh-sim qa`.
#[derive(Clone)]
pub struct ReplaySharedContext {
    pub tracking_assets: Arc<SharedTrackingAssets>,
    pub verified_gt: Arc<VerifiedGtFile>,
    pub current_commit: String,
    pub repo_root: Option<PathBuf>,
    pub dataset_fingerprint: DatasetFingerprint,
    pub engine_unchanged_cache: Arc<Mutex<HashMap<String, Option<bool>>>>,
    pub ffmpeg_threads: Option<usize>,
}

pub fn run_replay_with_hook(
    opts: &ReplayOptions,
    hook: Option<&ReplayScenarioHook>,
) -> Result<ReplayFullOutput, String> {
    run_replay_with_context(opts, hook, None)
}

pub fn run_replay_with_context(
    opts: &ReplayOptions,
    hook: Option<&ReplayScenarioHook>,
    ctx: Option<&ReplaySharedContext>,
) -> Result<ReplayFullOutput, String> {
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
    let provenance = if let Some(c) = ctx {
        let datasets_match = recorded_provenance
            .as_ref()
            .map(|rec| rec.datasets.matches(&c.dataset_fingerprint));
        let same_commit = recorded_provenance
            .as_ref()
            .and_then(|rec| compare_git_commits(&rec.git_commit, &c.current_commit));
        let engine_code_unchanged = match same_commit {
            Some(true) => Some(true),
            Some(false) => recorded_provenance.as_ref().and_then(|rec| {
                let root = c.repo_root.as_deref()?;
                {
                    let guard = c
                        .engine_unchanged_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(&cached) = guard.get(&rec.git_commit) {
                        return cached;
                    }
                }
                let val = check_engine_paths_unchanged(root, &rec.git_commit, &c.current_commit);
                c.engine_unchanged_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(rec.git_commit.clone(), val);
                val
            }),
            None => None,
        };
        let effective_cur = if c.current_commit.trim().is_empty()
            || c.current_commit.trim().eq_ignore_ascii_case("unknown")
        {
            None
        } else {
            Some(c.current_commit.trim().to_owned())
        };
        ReplayProvenance {
            recorded: recorded_provenance,
            replay_data_source: replay_data_source.to_owned(),
            replay_datasets: c.dataset_fingerprint.clone(),
            datasets_match,
            same_commit,
            engine_code_unchanged,
            current_commit: effective_cur,
        }
    } else {
        let (current_commit, repo_root) = resolve_current_git_commit_and_root(BUILD_GIT_COMMIT);
        build_replay_provenance_with_repo(
            recorded_provenance,
            replay_data_source,
            &effective_data_dir,
            &current_commit,
            repo_root.as_deref(),
        )
    };

    let enable_mmap_view = opts.verify_mmap_view || opts.render_out.is_some();
    let capturer = Arc::new(ReplayCapturer::new());
    let OfflineReplayState {
        state,
        tile_cache,
        relocator_ready,
        sim_clock_bits,
    } = build_offline_replay_state(
        &sandbox.0,
        &effective_data_dir,
        tiles_dir.clone(),
        ug_dir.clone(),
        paimon_cache.clone(),
        Arc::clone(&capturer),
        enable_mmap_view,
        ctx.map(|c| Arc::clone(&c.tracking_assets)),
    )?;
    let _state_cleanup = ReplayStateCleanupGuard(Arc::clone(&state));

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
            let ff_threads = ctx.and_then(|c| c.ffmpeg_threads);
            video_decoder = Some(VideoPipeDecoder::spawn(
                ffmpeg_abs,
                &video_path,
                vw,
                vh,
                ff_threads,
            )?);
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
    if let Some(stop_idx) = hook.and_then(|h| h.stop_at_frame)
        && let Some(cut) = records.iter().position(|r| r.idx > stop_idx)
    {
        records.truncate(cut);
    }

    let loaded_web_inputs = load_web_inputs_jsonl(&opts.source_dir.join(WEB_INPUTS_FILENAME))?;
    let mut initial_state_record = None;
    let mut web_inputs = Vec::with_capacity(loaded_web_inputs.len());
    for w in loaded_web_inputs {
        if w.callback == INITIAL_STATE_CALLBACK {
            initial_state_record = Some(w);
        } else {
            web_inputs.push(w);
        }
    }
    let first_web_input_ts: Option<f64> = web_inputs
        .iter()
        .filter(|w| w.ts.is_finite())
        .map(|w| w.ts)
        .reduce(f64::min);
    if opts.max_frames.is_some() || hook.and_then(|h| h.stop_at_frame).is_some() {
        let cutoff_ts = records.last().map_or(f64::NEG_INFINITY, |r| r.ts);
        web_inputs.retain(|w| w.ts <= cutoff_ts);
    }
    let hotkeys: Vec<(f64, String)> = records
        .iter()
        .flat_map(|r| r.hotkeys.iter().cloned())
        .collect();

    let base_ts = records.first().map_or(0.0, |r| r.ts);
    let clock_origin = [
        records.first().map(|r| r.ts),
        initial_state_record.as_ref().map(|w| w.ts),
        first_web_input_ts,
        hotkeys
            .iter()
            .filter(|(ts, _)| ts.is_finite())
            .map(|(ts, _)| *ts)
            .reduce(f64::min),
    ]
    .into_iter()
    .flatten()
    .filter(|t| t.is_finite())
    .reduce(f64::min)
    .unwrap_or(0.0);
    let set_sim_clock = |ts: f64| {
        let s = if ts.is_finite() {
            (ts - clock_origin).max(0.0)
        } else {
            0.0
        };
        sim_clock_bits.store(s.to_bits(), Ordering::Release);
    };

    let mut timeline = if let Some(extra) = hook
        .map(|h| h.extra_inputs.as_slice())
        .filter(|e| !e.is_empty())
    {
        let mut events = Vec::with_capacity(web_inputs.len() + hotkeys.len() + extra.len());
        for w in web_inputs {
            if w.ts.is_finite() {
                events.push(TimelineInput::web_input(w.ts, w.callback, w.payload));
            }
        }
        for (ts, action) in hotkeys {
            if ts.is_finite() {
                events.push(TimelineInput::hotkey(ts, action));
            }
        }
        let ts_by_idx: BTreeMap<usize, f64> = records.iter().map(|r| (r.idx, r.ts)).collect();
        for (frame_idx, kind) in extra {
            let Some(&frame_ts) = ts_by_idx.get(frame_idx) else {
                return Err(format!(
                    "scenario input references frame {frame_idx} which does not exist in session '{session_name}'"
                ));
            };
            events.push(TimelineInput {
                ts: frame_ts,
                kind: kind.clone(),
            });
        }
        InputTimeline::from_events(events)
    } else {
        InputTimeline::from_inputs(web_inputs, hotkeys)
    };
    let callbacks = AppServerCallbacks::new_offline_replay(Arc::clone(&state));
    let mut input_replay = InputReplayReport::default();
    if let Some(ref init_rec) = initial_state_record {
        set_sim_clock(init_rec.ts);
        if state.apply_initial_nav_state(&init_rec.payload) {
            input_replay.initial_state_applied = true;
        }
    }
    let capture_nav_snapshots = hook.is_some_and(|h| h.capture_nav_snapshots);
    let capture_ui_snapshots =
        hook.is_some_and(|h| h.capture_ui_snapshots || h.capture_nav_snapshots);
    let mut nav_snapshots: BTreeMap<usize, NavFrameSnapshot> = BTreeMap::new();
    let mut ui_snapshots: BTreeMap<usize, UiFrameSnapshot> = BTreeMap::new();
    let mut rt = LoopRuntimeState::new();
    let mut modes = ModeCounts::default();
    let mut match_methods: BTreeMap<String, usize> = BTreeMap::new();
    let mut post_tick = PostTickTelemetry::default();
    let mut tick_ms = Vec::with_capacity(records.len());
    let mut observed = Vec::with_capacity(records.len());
    let mut map_m_syncs: Vec<MapMSyncRecord> = Vec::new();
    let mut truth_meta = ReplayTruthMeta::default();
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
            set_sim_clock(ev.ts);
            dispatch_timeline_input(&state, &callbacks, &ev, &mut input_replay);
        }
        set_sim_clock(rec.ts);
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
        let locate_inliers = locate_res
            .as_ref()
            .and_then(|(_, found)| found.as_ref())
            .and_then(|r| r.inliers_count);
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
            if let Some(inliers) = locate_inliers {
                truth_meta.map_m_inliers.insert(rec.idx, inliers);
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

        if rec.live_nav.is_some() || capture_nav_snapshots {
            let replay_nav = state.snapshot_nav_state();
            if let Some(ref live_nav) = rec.live_nav {
                record_nav_comparison_with_carryover(
                    rec.idx,
                    rec.ts,
                    first_web_input_ts,
                    input_replay.initial_state_applied,
                    live_nav,
                    &replay_nav,
                    &mut input_replay,
                );
            }
            if capture_nav_snapshots {
                nav_snapshots.insert(rec.idx, replay_nav);
            }
        }
        if capture_ui_snapshots {
            ui_snapshots.insert(rec.idx, state.snapshot_ui_state());
        }

        tick_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        observed.push(obs);
        if let Some(used) = capturer.take_frame()
            && let Some(ref dec) = video_decoder
        {
            dec.recycle_frame(used);
        }
    }

    for ev in timeline.drain_remaining() {
        set_sim_clock(ev.ts);
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
    let accuracy = match ctx {
        Some(c) => evaluate_session_accuracy_with_gt(
            &opts.gt_file,
            &c.verified_gt,
            &session_name,
            &observed,
        ),
        None => {
            let gt = VerifiedGtFile::load_or_default(&opts.gt_file)?;
            evaluate_session_accuracy_with_gt(&opts.gt_file, &gt, &session_name, &observed)
        }
    };
    let truth = evaluate_truth_oracle(&observed, &accuracy, &truth_meta);
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
        truth,
    };

    drop(_state_cleanup);

    if let Some(ref out_path) = opts.out_report {
        let val = serde_json::to_value(&report)
            .map_err(|e| format!("failed to serialize report: {e}"))?;
        write_json_atomic(out_path, &val)
            .map_err(|e| format!("failed to write {}: {e}", out_path.display()))?;
    }

    Ok(ReplayFullOutput {
        report,
        observed,
        map_m_syncs,
        nav_snapshots,
        ui_snapshots,
        truth_meta,
    })
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
        let mut dec = VideoPipeDecoder::spawn(
            &ffmpeg_abs,
            &sess_dir.join("video.mkv"),
            w,
            h,
            None,
        )
        .unwrap();
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
        let OfflineReplayState {
            state: rec_state, ..
        } = build_offline_replay_state(
            &rec_sandbox,
            &data_dir,
            rec_sandbox.join("tiles"),
            rec_sandbox.join("ug"),
            rec_sandbox.join("paimon.npz"),
            Arc::clone(&rec_capturer),
            false,
            None,
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

    #[test]
    fn provenance_engine_path_comparison_detects_non_engine_vs_engine_commits() {
        let sandbox = TempSandboxDir::new("gh_sim_prov_git").unwrap();
        let repo = sandbox.0.join("repo");
        let data_dir = sandbox.0.join("data");
        std::fs::create_dir_all(repo.join("rust/crates/gh-cv/src")).unwrap();
        std::fs::create_dir_all(repo.join("rust/crates/gh-app/src")).unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join(DATASET_FILENAMES[0]),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();
        let fp = fingerprint_data_dir(&data_dir);

        let git_ok = |args: &[&str]| -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };

        git_ok(&["init"]);
        git_ok(&["config", "user.email", "test@example.com"]);
        git_ok(&["config", "user.name", "Test"]);

        std::fs::write(repo.join("rust/crates/gh-cv/src/lib.rs"), "// cv v1\n").unwrap();
        std::fs::write(
            repo.join("rust/crates/gh-app/src/tracking_loop.rs"),
            "// loop v1\n",
        )
        .unwrap();
        std::fs::write(repo.join("docs/notes.md"), "v1\n").unwrap();
        git_ok(&["add", "."]);
        git_ok(&["commit", "-m", "base"]);
        let c_base = git_ok(&["rev-parse", "HEAD"]);

        // Non-engine commit: only docs/notes.md changed -> engine_code_unchanged = Some(true)
        std::fs::write(repo.join("docs/notes.md"), "v2\n").unwrap();
        git_ok(&["add", "docs/notes.md"]);
        git_ok(&["commit", "-m", "docs only"]);
        let c_docs = git_ok(&["rev-parse", "HEAD"]);

        // Engine commit in gh-cv -> engine_code_unchanged = Some(false)
        std::fs::write(repo.join("rust/crates/gh-cv/src/lib.rs"), "// cv v2\n").unwrap();
        git_ok(&["add", "rust/crates/gh-cv/src/lib.rs"]);
        git_ok(&["commit", "-m", "cv change"]);
        let c_cv = git_ok(&["rev-parse", "HEAD"]);

        // Engine commit in gh-app tracking_loop -> engine_code_unchanged = Some(false) vs c_cv
        std::fs::write(
            repo.join("rust/crates/gh-app/src/tracking_loop.rs"),
            "// loop v2\n",
        )
        .unwrap();
        git_ok(&["add", "rust/crates/gh-app/src/tracking_loop.rs"]);
        git_ok(&["commit", "-m", "tracking_loop change"]);
        let c_loop = git_ok(&["rev-parse", "HEAD"]);

        let mk_rec = |sha: &str| SessionProvenance {
            app_version: "5.7.0".to_owned(),
            git_commit: sha.to_owned(),
            data_source: "installed".to_owned(),
            datasets: fp.clone(),
        };

        // 1. Same commit -> same_commit=Some(true), engine_code_unchanged=Some(true)
        let p_same = build_replay_provenance_with_repo(
            Some(mk_rec(&c_docs)),
            "data_dir",
            &data_dir,
            &c_docs,
            Some(&repo),
        );
        assert_eq!(p_same.same_commit, Some(true));
        assert_eq!(p_same.engine_code_unchanged, Some(true));

        // 2. Non-engine change only -> same_commit=Some(false), engine_code_unchanged=Some(true) -> code=
        let p_non_engine = build_replay_provenance_with_repo(
            Some(mk_rec(&c_base)),
            "data_dir",
            &data_dir,
            &c_docs,
            Some(&repo),
        );
        assert_eq!(p_non_engine.same_commit, Some(false));
        assert_eq!(p_non_engine.engine_code_unchanged, Some(true));

        // 3. gh-cv engine change -> same_commit=Some(false), engine_code_unchanged=Some(false) -> code!
        let p_cv = build_replay_provenance_with_repo(
            Some(mk_rec(&c_docs)),
            "data_dir",
            &data_dir,
            &c_cv,
            Some(&repo),
        );
        assert_eq!(p_cv.same_commit, Some(false));
        assert_eq!(p_cv.engine_code_unchanged, Some(false));

        // 4. gh-app tracking_loop change -> same_commit=Some(false), engine_code_unchanged=Some(false) -> code!
        let p_loop = build_replay_provenance_with_repo(
            Some(mk_rec(&c_cv)),
            "data_dir",
            &data_dir,
            &c_loop,
            Some(&repo),
        );
        assert_eq!(p_loop.same_commit, Some(false));
        assert_eq!(p_loop.engine_code_unchanged, Some(false));

        // 5. Missing commit SHA -> falls back to None (code!)
        let p_missing = build_replay_provenance_with_repo(
            Some(mk_rec(&"0".repeat(40))),
            "data_dir",
            &data_dir,
            &c_loop,
            Some(&repo),
        );
        assert_eq!(p_missing.same_commit, Some(false));
        assert_eq!(p_missing.engine_code_unchanged, None);
    }

    #[test]
    fn e1_initial_state_restores_carried_over_nav_before_frame_1_and_legacy_separates_pre_web() {
        use gh_app::session_recorder::{SessionCodec, SessionRecorder, SessionRecorderConfig};
        use gh_core::traits::ServerCallbacks;
        use serde_json::json;

        let tmp = TempSandboxDir::new("gh_sim_e1_init_state").unwrap();
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("multimap_v4.json"), r#"{"maps":{}}"#).unwrap();

        let gt_path = tmp.0.join("verified_gt.json");
        std::fs::write(&gt_path, r#"{"schema_version":1,"sessions":{}}"#).unwrap();

        let make_session_dir = |name: &str| -> PathBuf {
            let dir = tmp.0.join(name);
            let fdir = dir.join("frames");
            std::fs::create_dir_all(&fdir).unwrap();
            let frame = FrameBuf::new_zeroed(2560, 1440, PixelFormat::Bgra8);
            for i in 1..=4usize {
                save_bgra_png(&frame, &fdir.join(format!("{i:06}.png"))).unwrap();
            }
            dir
        };

        // Build live state with carried-over nav target "23075" BEFORE recording starts.
        let rec_sandbox = tmp.0.join("rec_sb");
        std::fs::create_dir_all(rec_sandbox.join("tiles")).unwrap();
        std::fs::create_dir_all(rec_sandbox.join("ug")).unwrap();
        let rec_capturer = Arc::new(ReplayCapturer::new());
        let OfflineReplayState {
            state: rec_state, ..
        } = build_offline_replay_state(
            &rec_sandbox,
            &data_dir,
            rec_sandbox.join("tiles"),
            rec_sandbox.join("ug"),
            rec_sandbox.join("paimon.npz"),
            Arc::clone(&rec_capturer),
            false,
            None,
        )
        .unwrap();
        let rec_cb = AppServerCallbacks::new_offline_replay(Arc::clone(&rec_state));
        let _ = rec_cb
            .on_nearby_pins(&json!({
                "map_id": 7,
                "pins": [
                    {"id": "23075", "lat": 100.0, "lng": 200.0, "name": "Carried Pin"}
                ]
            }))
            .unwrap();
        // Select target "23075" via lock_target or update so snapshot_nav_state reports "23075"
        {
            let mut nav = rec_state.nav_controller.lock().unwrap();
            let _ = nav.select_target("23075");
        }
        assert_eq!(rec_state.snapshot_nav_state().target_id.as_deref(), Some("23075"));

        // Attach SessionRecorder -> automatically captures sanitized initial_state record!
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
        rec_state.attach_session_recorder(Arc::clone(&srec));
        let mut init_records = srec.drain_pending_web_inputs();
        assert_eq!(init_records.len(), 1);
        assert_eq!(init_records[0].callback, INITIAL_STATE_CALLBACK);
        init_records[0].ts = 100.0;

        // Also record a later web input at ts=110.0 (between frame 2 and frame 3) clearing pins via filter_changed=true.
        let _ = rec_cb
            .on_nearby_pins(&json!({
                "map_id": 7,
                "filter_changed": true,
                "pins": []
            }))
            .unwrap();
        let mut later_web = srec.drain_pending_web_inputs();
        assert_eq!(later_web.len(), 1);
        later_web[0].ts = 110.0;

        // Frames 1..2 (ts 101.0, 102.0): carried-over target "23075" before first web input at 110.0.
        // Frame 3 (ts 111.0): after filter_changed=true empty pins, target becomes null.
        // Frame 4 (ts 112.0): intentionally inject a live target "99999" (mismatch after first web input).
        let meta_with_init = [
            json!({"i": 1, "ts": 101.0, "frame_w": 2560, "frame_h": 1440, "state": "UNLOCALIZED", "nav": {"target_id": "23075", "locked_target_id": null, "user_locked_id": "23075", "state": "idle"}}),
            json!({"i": 2, "ts": 102.0, "frame_w": 2560, "frame_h": 1440, "state": "UNLOCALIZED", "nav": {"target_id": "23075", "locked_target_id": null, "user_locked_id": "23075", "state": "idle"}}),
            json!({"i": 3, "ts": 111.0, "frame_w": 2560, "frame_h": 1440, "state": "UNLOCALIZED", "nav": {"target_id": null, "locked_target_id": null, "user_locked_id": null, "state": "idle"}}),
            json!({"i": 4, "ts": 112.0, "frame_w": 2560, "frame_h": 1440, "state": "UNLOCALIZED", "nav": {"target_id": null, "locked_target_id": null, "user_locked_id": null, "state": "idle"}}),
        ];

        // (a) Session WITH initial_state -> restores "23075" before frame 1 with 0 nav mismatches!
        let sess_with_init = make_session_dir("sess_with_init");
        std::fs::write(
            sess_with_init.join("meta.jsonl"),
            meta_with_init
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let all_web: Vec<_> = init_records.into_iter().chain(later_web.iter().cloned()).collect();
        std::fs::write(
            sess_with_init.join(WEB_INPUTS_FILENAME),
            all_web
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();

        let rep_init = run_replay(&ReplayOptions {
            source_dir: sess_with_init,
            data_dir: data_dir.clone(),
            gt_file: gt_path.clone(),
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
        assert!(rep_init.input_replay.initial_state_applied);
        assert_eq!(rep_init.input_replay.nav_frames_compared, 4);
        assert_eq!(rep_init.input_replay.nav_mismatches, 0);
        assert_eq!(rep_init.input_replay.nav_carried_over_mismatches, 0);

        // (b) Legacy session WITHOUT initial_state -> frames 1..2 (ts < 110.0) counted in
        // nav_carried_over_mismatches (2), frame 4 post-web mismatch counted in nav_mismatches (1).
        let sess_legacy = make_session_dir("sess_legacy");
        let meta_legacy = [
            meta_with_init[0].clone(),
            meta_with_init[1].clone(),
            meta_with_init[2].clone(),
            json!({"i": 4, "ts": 112.0, "frame_w": 2560, "frame_h": 1440, "state": "UNLOCALIZED", "nav": {"target_id": "99999", "locked_target_id": null, "user_locked_id": null, "state": "idle"}}),
        ];
        std::fs::write(
            sess_legacy.join("meta.jsonl"),
            meta_legacy
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        std::fs::write(
            sess_legacy.join(WEB_INPUTS_FILENAME),
            later_web
                .iter()
                .map(|r| serde_json::to_string(r).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();

        let rep_legacy = run_replay(&ReplayOptions {
            source_dir: sess_legacy,
            data_dir,
            gt_file: gt_path,
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
        assert!(!rep_legacy.input_replay.initial_state_applied);
        assert_eq!(rep_legacy.input_replay.nav_frames_compared, 4);
        assert_eq!(rep_legacy.input_replay.nav_carried_over_mismatches, 2);
        assert_eq!(rep_legacy.input_replay.nav_mismatches, 1);
        assert_eq!(rep_legacy.input_replay.first_nav_mismatch_idx, Some(4));
    }

    #[test]
    fn e3_replay_nav_uses_simulated_clock_not_wall_clock() {
        use gh_core::nav_controller::{CLOSER_DWELL_S, NavPin};

        let tmp = TempSandboxDir::new("gh_sim_e3_sim_clock").unwrap();
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("multimap_v4.json"), r#"{"maps":{}}"#).unwrap();
        std::fs::create_dir_all(tmp.0.join("tiles")).unwrap();
        std::fs::create_dir_all(tmp.0.join("ug")).unwrap();

        let capturer = Arc::new(ReplayCapturer::new());
        let OfflineReplayState {
            state,
            sim_clock_bits,
            ..
        } = build_offline_replay_state(
            &tmp.0,
            &data_dir,
            tmp.0.join("tiles"),
            tmp.0.join("ug"),
            tmp.0.join("paimon.npz"),
            capturer,
            false,
            None,
        )
        .unwrap();

        let set_sim_s = |s: f64| sim_clock_bits.store(s.to_bits(), Ordering::Release);

        // 1. Proximity takeover (`CLOSER_DWELL_S = 0.5s`): two updates separated by 0.6s in
        // simulated time executed back-to-back in < 5ms wall time MUST switch target.
        let t_wall = Instant::now();
        {
            let mut nav = state.nav_controller.lock().unwrap();
            nav.set_pins(
                vec![
                    NavPin {
                        id: "pin_a".to_owned(),
                        map_id: 2,
                        lat: 0.0,
                        lng: 100.0,
                        name: "Pin A".to_owned(),
                        ..Default::default()
                    },
                    NavPin {
                        id: "pin_b".to_owned(),
                        map_id: 2,
                        lat: 0.0,
                        lng: 163.0,
                        name: "Pin B".to_owned(),
                        ..Default::default()
                    },
                ],
                true,
                None,
            );
            set_sim_s(1.0);
            let g0 = nav.compute_guidance(0.0, 100.0, None, 2, None, false, "TRACKING", "G");
            assert_eq!(g0.target.as_ref().map(|p| p.id.as_str()), Some("pin_a"));

            // Move player 45m (< TELEPORT_JUMP_M=50m) to (0, 145): 45m from pin_a, 18m (> PROXIMITY_TAKEOVER_M=12m) from pin_b.
            set_sim_s(1.1);
            let g1 = nav.compute_guidance(0.0, 145.0, None, 2, None, false, "TRACKING", "G");
            assert_eq!(
                g1.target.as_ref().map(|p| p.id.as_str()),
                Some("pin_a"),
                "dwell just started at t=1.1s, must not switch yet"
            );

            // Advance simulated clock past CLOSER_DWELL_S (0.5s) while wall elapsed is < 50ms.
            set_sim_s(1.1 + CLOSER_DWELL_S + 0.15);
            let g2 = nav.compute_guidance(0.0, 145.0, None, 2, None, false, "TRACKING", "G");
            assert!(
                t_wall.elapsed().as_secs_f64() < CLOSER_DWELL_S,
                "sanity: test must complete faster than wall-clock dwell"
            );
            assert_eq!(
                g2.target.as_ref().map(|p| p.id.as_str()),
                Some("pin_b"),
                "simulated clock advanced past CLOSER_DWELL_S so proximity takeover must fire"
            );
        }

        // 2. NavMarkConfirm 3.0s expiry: two nav_mark presses separated by 4.0s simulated time
        // (and < 50ms wall time) on a far target (> 25m) must NOT confirm on the 2nd press.
        {
            let mut nav = state.nav_controller.lock().unwrap();
            nav.set_pins(
                vec![NavPin {
                    id: "far_pin".to_owned(),
                    map_id: 2,
                    lat: 0.0,
                    lng: 500.0,
                    name: "Far Pin".to_owned(),
                    ..Default::default()
                }],
                true,
                None,
            );
            set_sim_s(10.0);
            let _ = nav.compute_guidance(0.0, 0.0, None, 2, None, false, "TRACKING", "G");
        }
        set_sim_s(10.0);
        gh_app::dispatch_hotkey_action(&state, "nav_mark");
        assert!(
            !state.nav_controller.lock().unwrap().collected_ids.contains("far_pin"),
            "first far press arms 3s confirmation"
        );
        // Advance simulated clock +4.0s (> 3.0s confirm window) in < 50ms wall time -> window expired.
        set_sim_s(14.0);
        gh_app::dispatch_hotkey_action(&state, "nav_mark");
        assert!(
            !state.nav_controller.lock().unwrap().collected_ids.contains("far_pin"),
            "4.0s simulated gap exceeds 3.0s confirm window; wall-clock bug would falsely mark collected"
        );
        // Press again +1.0s later (< 3.0s window from 14.0s) -> must confirm and mark collected!
        set_sim_s(15.0);
        gh_app::dispatch_hotkey_action(&state, "nav_mark");
        assert!(
            state.nav_controller.lock().unwrap().collected_ids.contains("far_pin"),
            "1.0s simulated gap within 3.0s confirm window must confirm collection"
        );
    }
}
