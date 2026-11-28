//! Batch regression QA runner (`gh-sim qa`) against blessed frame-by-frame poses.
//!
//! Compares fresh synchronous replays of recorded sessions against human-approved
//! `<session>/blessed_poses.jsonl` files (verified by SHA-256 recorded in git-tracked
//! `tests/qa_baseline.json`). Metrics against legacy `meta.jsonl` are printed and
//! recorded for reference only — never used for pass/fail grading.

use crate::qa_cache::{
    SessionInputFingerprint, build_run_env_fingerprint, build_session_input_fingerprint,
    compute_session_engine_unchanged, default_qa_cache_dir, load_cached_scenario,
    load_cached_session, scenario_cache_key, session_cache_key, store_cached_scenario,
    store_cached_session,
};
use crate::replay::{
    DEFAULT_DRIFT_THRESHOLD_W, ObservedFrame, RecordingFidelityReport, ReplayFullOutput,
    ReplayOptions, ReplayReport, ReplaySharedContext, TempSandboxDir, angle_diff_deg, percentile,
    resolve_current_git_commit_and_root, resolve_replay_data_dir, round3,
    run_replay_with_context, run_replay_with_hook,
};
use crate::scenario::{
    DEFAULT_SCENARIOS_DIR, ScenarioFile, ScenarioResult, ScenarioSummary, format_scenario_line,
    load_scenarios_from_dir, run_scenario, run_scenario_with_context,
};
use crate::timeline::InputReplayReport;
use crate::truth_oracle::{TruthSummary, evaluate_truth_oracle, format_truth_col};
use crate::verified_gt::VerifiedGtFile;
use gh_app::BUILD_GIT_COMMIT;
use gh_app::tracking_stack::SharedTrackingAssets;
use gh_core::atomic_io::{write_bytes_atomic, write_json_atomic};
use gh_core::provenance::{DatasetFingerprint, ReplayProvenance, fingerprint_data_dir};
use gh_core::sha256::sha256_file;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const QA_BASELINE_SCHEMA_VERSION: u32 = 1;
pub const QA_REPORT_SCHEMA_VERSION: u32 = 1;
pub const BLESSED_POSES_FILENAME: &str = "blessed_poses.jsonl";
pub const QA_REPORT_FILENAME: &str = "qa_report.json";
pub const HEADING_FLIP_THRESHOLD_DEG: f64 = 120.0;
pub const HEADING_SPIKE_STEP_THRESHOLD_DEG: f64 = 30.0;
pub const HEADING_SPIKE_RETURN_THRESHOLD_DEG: f64 = 15.0;
pub const HEADING_MOVE_STEP_THRESHOLD_DEG: f64 = 4.0;
pub const HEADING_STUTTER_ENTRY_STEP_DEG: f64 = 5.0;
pub const HEADING_STUTTER_STALL_CAP_DEG: f64 = 3.0;
pub const HEADING_STUTTER_STALL_RATIO: f64 = 0.35;
pub const HEADING_STUTTER_RESUME_STEP_DEG: f64 = 10.0;
pub const HEADING_STUTTER_RESUME_JERK_DEG: f64 = 10.0;
pub const HEADING_STUTTER_MAX_STALL_STEPS: usize = 3;
pub const REBLESS_HEADING_STABILITY_NOTE: &str =
    "re-bless needed for heading stability (blessed_poses.jsonl lacks body_heading/camera_heading)";
pub const DEFAULT_QA_POLICY: &str = "Agent KHÔNG được chạy --bless, sửa baseline hay nới ngưỡng khi chưa có người dùng đồng ý. / Agents MUST NOT run --bless, modify this baseline, or relax thresholds without explicit user approval.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaThresholds {
    pub pos_threshold_w: f64,
    pub max_over_threshold_frames: usize,
    pub max_map_mismatches: usize,
    pub max_lost_pos_frames: usize,
    pub max_map_m_synced_drop: usize,
    pub max_mmap_view_drop: usize,
}

impl Default for QaThresholds {
    fn default() -> Self {
        Self {
            pos_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            max_over_threshold_frames: 0,
            max_map_mismatches: 0,
            max_lost_pos_frames: 0,
            max_map_m_synced_drop: 0,
            max_mmap_view_drop: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QaThresholdOverrides {
    pub pos_threshold_w: Option<f64>,
    pub max_over_threshold_frames: Option<usize>,
    pub max_map_mismatches: Option<usize>,
    pub max_lost_pos_frames: Option<usize>,
    pub max_map_m_synced_drop: Option<usize>,
    pub max_mmap_view_drop: Option<usize>,
    pub min_mmap_view_acquired: Option<usize>,
    pub max_p95_pos_delta_w: Option<f64>,
    pub max_pos_delta_w: Option<f64>,
}

impl QaThresholdOverrides {
    /// Applies CLI overrides onto baseline thresholds.
    ///
    /// Rejects any override that loosens a baseline-backed threshold (exceeds baseline)
    /// or provides a non-finite (`NaN`, `±inf`) or negative `f64` value.
    pub fn apply_to(&self, base: &QaThresholds) -> Result<QaThresholds, String> {
        let mut errors = Vec::new();

        let pos_threshold_w = match self.pos_threshold_w {
            Some(v) if !v.is_finite() || v < 0.0 => {
                errors.push(format!(
                    "--pos-threshold-w={v} is invalid (must be finite and >= 0; baseline={})",
                    base.pos_threshold_w
                ));
                base.pos_threshold_w
            }
            Some(v) if v > base.pos_threshold_w => {
                errors.push(format!(
                    "--pos-threshold-w={v} loosens baseline ({})",
                    base.pos_threshold_w
                ));
                base.pos_threshold_w
            }
            Some(v) => v,
            None => base.pos_threshold_w,
        };

        let mut check_usize = |flag: &str, ov: Option<usize>, base_val: usize| -> usize {
            match ov {
                Some(v) if v > base_val => {
                    errors.push(format!("{flag}={v} loosens baseline ({base_val})"));
                    base_val
                }
                Some(v) => v,
                None => base_val,
            }
        };

        let max_over_threshold_frames = check_usize(
            "--max-over-threshold",
            self.max_over_threshold_frames,
            base.max_over_threshold_frames,
        );
        let max_map_mismatches = check_usize(
            "--max-map-mismatches",
            self.max_map_mismatches,
            base.max_map_mismatches,
        );
        let max_lost_pos_frames = check_usize(
            "--max-lost-pos-frames",
            self.max_lost_pos_frames,
            base.max_lost_pos_frames,
        );
        let max_map_m_synced_drop = check_usize(
            "--max-map-m-synced-drop",
            self.max_map_m_synced_drop,
            base.max_map_m_synced_drop,
        );
        let max_mmap_view_drop = check_usize(
            "--max-mmap-view-drop",
            self.max_mmap_view_drop,
            base.max_mmap_view_drop,
        );

        if let Some(v) = self.max_p95_pos_delta_w
            && (!v.is_finite() || v < 0.0)
        {
            errors.push(format!(
                "--max-p95-pos-delta-w={v} is invalid (must be finite and >= 0)"
            ));
        }
        if let Some(v) = self.max_pos_delta_w
            && (!v.is_finite() || v < 0.0)
        {
            errors.push(format!(
                "--max-pos-delta-w={v} is invalid (must be finite and >= 0)"
            ));
        }

        if !errors.is_empty() {
            return Err(format!(
                "invalid QA threshold override(s) (cannot loosen baseline without user approval): {}",
                errors.join("; ")
            ));
        }

        Ok(QaThresholds {
            pos_threshold_w,
            max_over_threshold_frames,
            max_map_mismatches,
            max_lost_pos_frames,
            max_map_m_synced_drop,
            max_mmap_view_drop,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaBaselineSession {
    pub blessed_poses_sha256: String,
    pub frames: usize,
    pub codec: String,
    pub color_safe: bool,
    pub tracking_frames: usize,
    pub map_m_synced_count: usize,
    pub mmap_view_frames: usize,
    pub mmap_view_acquired: usize,
    pub mmap_view_drawer_open: usize,
    pub meta_ref_p95_pos_delta_w: Option<f64>,
    pub meta_ref_max_pos_delta_w: Option<f64>,
    pub meta_ref_over_threshold: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaBaselineFile {
    pub schema_version: u32,
    pub policy: String,
    pub thresholds: QaThresholds,
    pub sessions: BTreeMap<String, QaBaselineSession>,
}

impl Default for QaBaselineFile {
    fn default() -> Self {
        Self {
            schema_version: QA_BASELINE_SCHEMA_VERSION,
            policy: DEFAULT_QA_POLICY.to_owned(),
            thresholds: QaThresholds::default(),
            sessions: BTreeMap::new(),
        }
    }
}

impl QaBaselineFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read QA baseline {}: {e}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| format!("failed to parse QA baseline {}: {e}", path.display()))
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), String> {
        let val = serde_json::to_value(self)
            .map_err(|e| format!("failed to serialize QA baseline: {e}"))?;
        write_json_atomic(path, &val)
            .map_err(|e| format!("failed to write QA baseline {}: {e}", path.display()))
    }
}

#[derive(Debug, Clone)]
pub struct QaOptions {
    pub sessions_dir: PathBuf,
    pub data_dir: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub baseline_path: PathBuf,
    pub gt_file: PathBuf,
    pub scenarios_dir: Option<PathBuf>,
    pub only: Vec<String>,
    pub out_dir: Option<PathBuf>,
    pub bless: bool,
    pub overrides: QaThresholdOverrides,
    /// Worker pool size (`None` = auto from CPU/RAM, `Some(1)` = original serial path).
    pub jobs: Option<usize>,
    /// Disable input-keyed disk result cache for this run.
    pub no_cache: bool,
    /// Optional override directory for the QA disk cache.
    pub cache_dir: Option<PathBuf>,
    /// Optional directory where per-session `<session>.jsonl` `ObservedFrame` poses are written.
    pub dump_poses_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BlessedComparison {
    pub blessed_frames: usize,
    pub actual_frames: usize,
    pub frame_count_mismatch: bool,
    pub compared_pos_frames: usize,
    pub map_mismatches: usize,
    pub first_map_mismatch_idx: Option<usize>,
    pub lost_pos_frames: usize,
    pub gained_pos_frames: usize,
    pub over_threshold_frames: usize,
    pub first_over_threshold_idx: Option<usize>,
    pub median_pos_delta_w: Option<f64>,
    pub p95_pos_delta_w: Option<f64>,
    pub max_pos_delta_w: Option<f64>,
    pub median_heading_delta_deg: Option<f64>,
    pub p95_heading_delta_deg: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MetaReferenceSummary {
    pub compared_pos_frames: usize,
    pub map_mismatches: usize,
    pub over_threshold_frames: usize,
    pub median_pos_delta_w: Option<f64>,
    pub p95_pos_delta_w: Option<f64>,
    pub max_pos_delta_w: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadingSample {
    pub idx: usize,
    pub tracking: bool,
    pub heading: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadingStability {
    pub steps: usize,
    pub flips: usize,
    pub spikes: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadingStabilityPair {
    pub body: HeadingStability,
    pub camera: HeadingStability,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct HeadingSmoothness {
    pub moving_pairs: usize,
    pub move_jerk_p50_deg: Option<f64>,
    pub move_jerk_p95_deg: Option<f64>,
    pub move_jerk_max_deg: Option<f64>,
    pub stutters: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct HeadingSmoothnessPair {
    pub body: HeadingSmoothness,
    pub camera: HeadingSmoothness,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionHeadingStability {
    pub replay: HeadingStabilityPair,
    pub blessed: Option<HeadingStabilityPair>,
    pub live_meta: Option<HeadingStabilityPair>,
    #[serde(default)]
    pub replay_smoothness: HeadingSmoothnessPair,
    #[serde(default)]
    pub blessed_smoothness: Option<HeadingSmoothnessPair>,
    #[serde(default)]
    pub live_meta_smoothness: Option<HeadingSmoothnessPair>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadingStabilityGateOutcome {
    pub fail_reasons: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MMapOpenLatency {
    pub open_ts: f64,
    pub map_id: Option<i32>,
    pub engine_synced_s: Option<f64>,
    pub broadcast_s: Option<f64>,
    pub hud_s: Option<f64>,
    /// Whether the game answered the web `synced` with a toast (`hud_feedback` event).
    /// `None` when the web never confirmed or the session predates `hud_feedback` events.
    #[serde(default)]
    pub in_game_feedback: Option<bool>,
    /// Reason the app gave for not showing anything (`hud_feedback.reason`), when it said one.
    #[serde(default)]
    pub in_game_skip_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MMapLatencySummary {
    pub count: usize,
    pub median_hud_s: Option<f64>,
    pub max_hud_s: Option<f64>,
    pub no_hud_count: usize,
    /// The recording carries `hud_feedback` events, so in-game output can be checked.
    #[serde(default)]
    pub in_game_audited: bool,
    /// Web-confirmed M-opens answered in game with a toast.
    #[serde(default)]
    pub in_game_ok: usize,
    /// Web-confirmed M-opens with no in-game toast: the user got no confirmation.
    #[serde(default)]
    pub in_game_missing: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LiveMMapLatency {
    pub available: bool,
    pub opens: Vec<MMapOpenLatency>,
    pub summary: MMapLatencySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaSessionResult {
    pub session_name: String,
    pub passed: bool,
    pub status: String,
    pub reason: String,
    pub reasons: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub frames: usize,
    pub codec: String,
    pub color_safe: bool,
    pub wall_elapsed_s: f64,
    pub effective_fps: f64,
    pub hd_downloads: u64,
    pub map_m_synced_count: usize,
    pub baseline_map_m_synced_count: Option<usize>,
    pub mmap_view_frames: usize,
    pub mmap_view_acquired: usize,
    pub baseline_mmap_view_acquired: Option<usize>,
    pub mmap_view_drawer_open: usize,
    pub blessed_sha256_ok: bool,
    pub vs_blessed: Option<BlessedComparison>,
    pub vs_meta_ref: MetaReferenceSummary,
    #[serde(default)]
    pub heading_stability: SessionHeadingStability,
    #[serde(default)]
    pub live_mmap_latency: LiveMMapLatency,
    #[serde(default)]
    pub provenance: Option<ReplayProvenance>,
    #[serde(default)]
    pub recording_fidelity: Option<RecordingFidelityReport>,
    #[serde(default)]
    pub input_replay: Option<InputReplayReport>,
    #[serde(default)]
    pub truth: Option<TruthSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaReport {
    pub schema_version: u32,
    pub mode: String,
    pub passed: bool,
    pub passed_sessions: usize,
    pub failed_sessions: usize,
    pub total_sessions: usize,
    pub total_wall_elapsed_s: f64,
    pub baseline_path: String,
    pub effective_thresholds: QaThresholds,
    pub cli_overrides: QaThresholdOverrides,
    pub sessions: Vec<QaSessionResult>,
    #[serde(default)]
    pub scenario_summary: ScenarioSummary,
    #[serde(default)]
    pub scenarios: Vec<ScenarioResult>,
}

/// Returns whether a recorded video codec preserves RGB channels without YUV chroma/range shift.
pub fn is_codec_color_safe(codec: &str) -> bool {
    let c = codec.trim().to_ascii_lowercase();
    matches!(
        c.as_str(),
        "libx264rgb" | "ffv1" | "utvideo" | "huffyuv" | "png" | "png_frames" | "rawvideo"
    )
}

/// Extracts `(codec, color_safe)` from `<session_dir>/info.json` (or PNG frames fallback).
pub fn read_session_codec_info(session_dir: &Path) -> (String, bool) {
    let info_path = session_dir.join("info.json");
    if let Ok(raw) = std::fs::read_to_string(&info_path)
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
        && let Some(codec) = val.get("codec").and_then(Value::as_str)
    {
        return (codec.to_owned(), is_codec_color_safe(codec));
    }
    if session_dir.join("frames").is_dir() {
        return ("png_frames".to_owned(), true);
    }
    ("unknown".to_owned(), false)
}

/// Discovers valid session directories under `sessions_dir`, sorted deterministically by name.
pub fn discover_sessions(sessions_dir: &Path, only: &[String]) -> Result<Vec<PathBuf>, String> {
    if !sessions_dir.is_dir() {
        return Err(format!(
            "sessions directory does not exist: {}",
            sessions_dir.display()
        ));
    }
    let mut dirs = Vec::new();
    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|e| format!("failed to read {}: {e}", sessions_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry error: {e}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let has_meta = path.join("meta.jsonl").is_file();
        let has_media = path.join("video.mkv").is_file()
            || path.join("video.mp4").is_file()
            || path.join("frames").is_dir();
        if has_meta && has_media {
            dirs.push(path);
        }
    }
    dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    if !only.is_empty() {
        let mut filtered = Vec::new();
        for name in only {
            let found = dirs
                .iter()
                .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(name.as_str()));
            match found {
                Some(p) => {
                    if !filtered.contains(p) {
                        filtered.push(p.clone());
                    }
                }
                None => {
                    return Err(format!(
                        "requested --only session '{}' not found under {}",
                        name,
                        sessions_dir.display()
                    ));
                }
            }
        }
        filtered.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        dirs = filtered;
    }

    if dirs.is_empty() {
        return Err(format!(
            "no valid sessions found under {}",
            sessions_dir.display()
        ));
    }
    Ok(dirs)
}

/// Sanitizes non-finite floats before JSONL serialization so every line stays valid JSON.
fn sanitize_pose(p: &ObservedFrame) -> ObservedFrame {
    ObservedFrame {
        idx: p.idx,
        ts: if p.ts.is_finite() { p.ts } else { 0.0 },
        state: p.state.clone(),
        match_method: p.match_method.clone(),
        map_id: p.map_id,
        world_x: p.world_x.filter(|v| v.is_finite()),
        world_y: p.world_y.filter(|v| v.is_finite()),
        heading: p.heading.filter(|v| v.is_finite()),
        body_heading: p.body_heading.filter(|v| v.is_finite()),
        camera_heading: p.camera_heading.filter(|v| v.is_finite()),
        is_underground: p.is_underground,
    }
}

/// Atomically writes `blessed_poses.jsonl` (1 compact JSON line per frame) and returns its SHA-256 hex.
pub fn write_blessed_poses_atomic(path: &Path, poses: &[ObservedFrame]) -> Result<String, String> {
    let mut buf = Vec::with_capacity(poses.len().saturating_mul(160));
    for pose in poses {
        let clean = sanitize_pose(pose);
        serde_json::to_writer(&mut buf, &clean)
            .map_err(|e| format!("failed to serialize blessed pose f={}: {e}", pose.idx))?;
        buf.push(b'\n');
    }
    write_bytes_atomic(path, &buf)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    sha256_file(path).map_err(|e| format!("failed to hash {}: {e}", path.display()))
}

/// Loads frame-by-frame poses from `<session>/blessed_poses.jsonl` and reports whether
/// the file contains the separate `body_heading`/`camera_heading` fields.
pub fn load_blessed_poses_with_heading_info(path: &Path) -> Result<(Vec<ObservedFrame>, bool), String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut has_heading_fields = false;
    for (line_no, line_res) in reader.lines().enumerate() {
        let line = line_res
            .map_err(|e| format!("read error {}:{}: {e}", path.display(), line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("parse error {}:{}: {e}", path.display(), line_no + 1))?;
        if val.get("body_heading").is_some() || val.get("camera_heading").is_some() {
            has_heading_fields = true;
        }
        let pose: ObservedFrame = serde_json::from_value(val)
            .map_err(|e| format!("parse error {}:{}: {e}", path.display(), line_no + 1))?;
        out.push(pose);
    }
    Ok((out, has_heading_fields))
}

/// Loads frame-by-frame poses from `<session>/blessed_poses.jsonl`.
pub fn load_blessed_poses(path: &Path) -> Result<Vec<ObservedFrame>, String> {
    load_blessed_poses_with_heading_info(path).map(|(poses, _)| poses)
}

/// Wraps an angle difference in degrees into `[-180.0, 180.0)`.
pub fn wrap_angle_deg(delta: f64) -> f64 {
    let mut w = (delta + 180.0).rem_euclid(360.0) - 180.0;
    if w >= 180.0 {
        w -= 360.0;
    }
    w
}

/// Pure function computing heading stability (`steps`, `flips`, `spikes`) for one heading kind
/// over a frame sequence.
pub fn compute_heading_stability(samples: &[HeadingSample]) -> HeadingStability {
    let valid_heading = |s: &HeadingSample| -> Option<f64> {
        if s.tracking {
            s.heading.filter(|h| h.is_finite())
        } else {
            None
        }
    };

    let mut steps = 0usize;
    let mut flips = 0usize;
    let mut spikes = 0usize;

    for pair in samples.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.idx.checked_add(1) != Some(b.idx) {
            continue;
        }
        let (Some(ha), Some(hb)) = (valid_heading(a), valid_heading(b)) else {
            continue;
        };
        steps += 1;
        let d = wrap_angle_deg(hb - ha);
        if d.abs() >= HEADING_FLIP_THRESHOLD_DEG {
            flips += 1;
        }
    }

    for tri in samples.windows(3) {
        let (a, b, c) = (&tri[0], &tri[1], &tri[2]);
        if a.idx.checked_add(1) != Some(b.idx) || b.idx.checked_add(1) != Some(c.idx) {
            continue;
        }
        let (Some(ha), Some(hb), Some(hc)) = (valid_heading(a), valid_heading(b), valid_heading(c))
        else {
            continue;
        };
        let d1 = wrap_angle_deg(hb - ha);
        let d2 = wrap_angle_deg(hc - hb);
        let opposite_sign = (d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0);
        if d1.abs() > HEADING_SPIKE_STEP_THRESHOLD_DEG
            && d2.abs() > HEADING_SPIKE_STEP_THRESHOLD_DEG
            && opposite_sign
            && wrap_angle_deg(hc - ha).abs() < HEADING_SPIKE_RETURN_THRESHOLD_DEG
        {
            spikes += 1;
        }
    }

    HeadingStability {
        steps,
        flips,
        spikes,
    }
}

fn collect_observed_heading_samples(poses: &[ObservedFrame]) -> (Vec<HeadingSample>, Vec<HeadingSample>) {
    let body_samples: Vec<HeadingSample> = poses
        .iter()
        .map(|p| HeadingSample {
            idx: p.idx,
            tracking: p.state == "TRACKING",
            heading: p.body_heading,
        })
        .collect();
    let camera_samples: Vec<HeadingSample> = poses
        .iter()
        .map(|p| HeadingSample {
            idx: p.idx,
            tracking: p.state == "TRACKING",
            heading: p.camera_heading,
        })
        .collect();
    (body_samples, camera_samples)
}

/// Computes paired body & camera heading stability over observed replay/blessed frames.
pub fn compute_observed_heading_stability(poses: &[ObservedFrame]) -> HeadingStabilityPair {
    let (body_samples, camera_samples) = collect_observed_heading_samples(poses);
    HeadingStabilityPair {
        body: compute_heading_stability(&body_samples),
        camera: compute_heading_stability(&camera_samples),
    }
}

/// Pure function computing informational per-step heading smoothness (`moving_pairs`, move-jerk
/// percentiles, and mid-sweep `stutters`) for one heading kind over a frame sequence.
pub fn compute_heading_smoothness(samples: &[HeadingSample]) -> HeadingSmoothness {
    let valid_heading = |s: &HeadingSample| -> Option<f64> {
        if s.tracking {
            s.heading.filter(|h| h.is_finite())
        } else {
            None
        }
    };

    let mut runs: Vec<Vec<f64>> = Vec::new();
    let mut cur_run: Vec<f64> = Vec::new();
    let mut prev_idx: Option<usize> = None;

    for s in samples {
        if let Some(h) = valid_heading(s) {
            if prev_idx.is_some_and(|p| p.checked_add(1) == Some(s.idx)) {
                cur_run.push(h);
            } else {
                if cur_run.len() >= 2 {
                    runs.push(std::mem::take(&mut cur_run));
                } else {
                    cur_run.clear();
                }
                cur_run.push(h);
            }
            prev_idx = Some(s.idx);
        } else {
            if cur_run.len() >= 2 {
                runs.push(std::mem::take(&mut cur_run));
            } else {
                cur_run.clear();
            }
            prev_idx = None;
        }
    }
    if cur_run.len() >= 2 {
        runs.push(cur_run);
    }

    let mut move_jerks = Vec::new();
    let mut stutters = 0usize;

    for run in &runs {
        let d: Vec<f64> = run.windows(2).map(|w| wrap_angle_deg(w[1] - w[0])).collect();
        for pair in d.windows(2) {
            if pair[0].abs() >= HEADING_MOVE_STEP_THRESHOLD_DEG
                || pair[1].abs() >= HEADING_MOVE_STEP_THRESHOLD_DEG
            {
                move_jerks.push((pair[1] - pair[0]).abs());
            }
        }

        let mut t = 0usize;
        while t + 2 < d.len() {
            let d_entry = d[t];
            if d_entry.abs() >= HEADING_STUTTER_ENTRY_STEP_DEG
                && d_entry.abs() < HEADING_FLIP_THRESHOLD_DEG
            {
                let dir = d_entry.signum();
                let stall_lim =
                    HEADING_STUTTER_STALL_CAP_DEG.min(d_entry.abs() * HEADING_STUTTER_STALL_RATIO);
                let mut s = 0usize;
                while s < HEADING_STUTTER_MAX_STALL_STEPS && t + 1 + s < d.len() {
                    if d[t + 1 + s].abs() <= stall_lim {
                        s += 1;
                    } else {
                        break;
                    }
                }
                if s >= 1 && t + 1 + s < d.len() {
                    let d_res = d[t + 1 + s];
                    if d_res * dir > 0.0
                        && d_res.abs() < HEADING_FLIP_THRESHOLD_DEG
                        && d_res.abs() >= HEADING_STUTTER_RESUME_STEP_DEG
                        && (d_res - d[t + s]).abs() >= HEADING_STUTTER_RESUME_JERK_DEG
                    {
                        stutters += 1;
                        t = t + 1 + s;
                        continue;
                    }
                }
            }
            t += 1;
        }
    }

    HeadingSmoothness {
        moving_pairs: move_jerks.len(),
        move_jerk_p50_deg: percentile(&move_jerks, 0.50).map(round3),
        move_jerk_p95_deg: percentile(&move_jerks, 0.95).map(round3),
        move_jerk_max_deg: move_jerks.iter().copied().reduce(f64::max).map(round3),
        stutters,
    }
}

/// Computes paired body & camera heading smoothness over observed replay/blessed frames.
pub fn compute_observed_heading_smoothness(poses: &[ObservedFrame]) -> HeadingSmoothnessPair {
    let (body_samples, camera_samples) = collect_observed_heading_samples(poses);
    HeadingSmoothnessPair {
        body: compute_heading_smoothness(&body_samples),
        camera: compute_heading_smoothness(&camera_samples),
    }
}

fn load_live_meta_heading_samples(
    meta_path: &Path,
) -> Option<(Vec<HeadingSample>, Vec<HeadingSample>)> {
    let file = std::fs::File::open(meta_path).ok()?;
    let reader = BufReader::new(file);
    let mut body_samples = Vec::new();
    let mut camera_samples = Vec::new();
    let mut has_heading_keys = false;

    for line_res in reader.lines() {
        let line = line_res.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(trimmed).ok()?;
        let body_val = v.get("body");
        let cam_val = v.get("camera");
        if body_val.is_some() || cam_val.is_some() {
            has_heading_keys = true;
        }
        let idx = v
            .get("i")
            .or_else(|| v.get("frame_idx"))
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(body_samples.len() + 1);
        let tracking = v.get("state").and_then(Value::as_str) == Some("TRACKING")
            && v.get("mode").and_then(Value::as_str) == Some("minimap");
        let body = body_val.and_then(Value::as_f64).filter(|f| f.is_finite());
        let camera = cam_val.and_then(Value::as_f64).filter(|f| f.is_finite());
        body_samples.push(HeadingSample {
            idx,
            tracking,
            heading: body,
        });
        camera_samples.push(HeadingSample {
            idx,
            tracking,
            heading: camera,
        });
    }

    if !has_heading_keys {
        return None;
    }

    Some((body_samples, camera_samples))
}

/// Loads live `meta.jsonl` and computes body/camera heading stability using only records with
/// `state == "TRACKING"` and `mode == "minimap"`. Returns `None` if unavailable or missing fields.
pub fn load_live_meta_heading_stability(meta_path: &Path) -> Option<HeadingStabilityPair> {
    let (body_samples, camera_samples) = load_live_meta_heading_samples(meta_path)?;
    Some(HeadingStabilityPair {
        body: compute_heading_stability(&body_samples),
        camera: compute_heading_stability(&camera_samples),
    })
}

/// Loads live `meta.jsonl` and computes body/camera heading smoothness using only records with
/// `state == "TRACKING"` and `mode == "minimap"`. Returns `None` if unavailable or missing fields.
pub fn load_live_meta_heading_smoothness(meta_path: &Path) -> Option<HeadingSmoothnessPair> {
    let (body_samples, camera_samples) = load_live_meta_heading_samples(meta_path)?;
    Some(HeadingSmoothnessPair {
        body: compute_heading_smoothness(&body_samples),
        camera: compute_heading_smoothness(&camera_samples),
    })
}

fn median_f64(vals: &[f64]) -> Option<f64> {
    let mut clean: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    if clean.is_empty() {
        return None;
    }
    clean.sort_by(f64::total_cmp);
    let n = clean.len();
    if n % 2 == 1 {
        Some(clean[n / 2])
    } else {
        Some((clean[n / 2 - 1] + clean[n / 2]) * 0.5)
    }
}

fn parse_event_seq(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

struct MetaFrameRow {
    ts: f64,
    mode: Option<String>,
    state: Option<String>,
    map_id: Option<i32>,
}

struct MetaAppEvent {
    ts: f64,
    kind: String,
    seq: u64,
    status: Option<String>,
    event_type: Option<String>,
    shown: Option<bool>,
    delivered: Option<bool>,
    reason: Option<String>,
}

struct MMapOpenSpan {
    open_ts: f64,
    end_frame_idx: usize,
    end_ts: f64,
    engine_synced_ts: Option<f64>,
}

/// Computes per-M-open live latency breakdown (`engine_synced_s`, `broadcast_s`, `hud_s`) and
/// summary from parsed `meta.jsonl` frame objects.
///
/// Informational only: returns `available: false` when no `events` array appears in `meta.jsonl`.
pub fn compute_live_mmap_latency(records: &[Value]) -> LiveMMapLatency {
    let mut frames: Vec<MetaFrameRow> = Vec::with_capacity(records.len());
    let mut events: Vec<MetaAppEvent> = Vec::new();
    let mut has_events = false;

    for v in records {
        if let Some(arr) = v.get("events").and_then(Value::as_array) {
            has_events = true;
            for ev in arr {
                let Some(ts) = ev.get("ts").and_then(Value::as_f64).filter(|f| f.is_finite())
                else {
                    continue;
                };
                let Some(kind) = ev.get("kind").and_then(Value::as_str) else {
                    continue;
                };
                events.push(MetaAppEvent {
                    ts,
                    kind: kind.to_owned(),
                    seq: parse_event_seq(ev.get("seq")),
                    status: ev.get("status").and_then(Value::as_str).map(str::to_owned),
                    event_type: ev.get("event_type").and_then(Value::as_str).map(str::to_owned),
                    shown: ev.get("shown").and_then(Value::as_bool),
                    delivered: ev.get("delivered").and_then(Value::as_bool),
                    reason: ev
                        .get("reason")
                        .and_then(Value::as_str)
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned),
                });
            }
        }
        let Some(ts) = v.get("ts").and_then(Value::as_f64).filter(|f| f.is_finite()) else {
            continue;
        };
        let mode = v.get("mode").and_then(Value::as_str).map(str::to_owned);
        let state = v.get("state").and_then(Value::as_str).map(str::to_owned);
        let map_id = v
            .get("map_id")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
        frames.push(MetaFrameRow {
            ts,
            mode,
            state,
            map_id,
        });
    }

    if !has_events {
        return LiveMMapLatency::default();
    }

    events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let in_game_audited = events.iter().any(|e| e.kind == "hud_feedback");

    let mut seen_non_map_m = false;
    let mut cur_open: Option<MMapOpenSpan> = None;
    let mut spans: Vec<MMapOpenSpan> = Vec::new();

    for (idx, frame) in frames.iter().enumerate() {
        let is_map_m = frame.mode.as_deref() == Some("map_m");
        if is_map_m {
            let is_synced_state = matches!(
                frame.state.as_deref(),
                Some("MAP_M_SyncedIdle") | Some("MAP_M_Panning")
            );
            if let Some(ref mut open) = cur_open {
                open.end_frame_idx = idx;
                open.end_ts = frame.ts;
                if open.engine_synced_ts.is_none() && is_synced_state {
                    open.engine_synced_ts = Some(frame.ts);
                }
            } else if seen_non_map_m {
                cur_open = Some(MMapOpenSpan {
                    open_ts: frame.ts,
                    end_frame_idx: idx,
                    end_ts: frame.ts,
                    engine_synced_ts: is_synced_state.then_some(frame.ts),
                });
            }
        } else {
            if let Some(open) = cur_open.take() {
                spans.push(open);
            }
            seen_non_map_m = true;
        }
    }
    if let Some(open) = cur_open.take() {
        spans.push(open);
    }

    let mut opens = Vec::with_capacity(spans.len());
    for span in spans {
        let broadcast_ev = events.iter().find(|e| {
            e.kind == "sync_broadcast" && e.ts >= span.open_ts && e.ts <= span.end_ts + 10.0
        });
        let hud_ev = broadcast_ev.and_then(|bc| {
            if bc.seq != 0
                && let Some(by_seq) = events.iter().find(|e| {
                    e.kind == "web_feedback"
                        && e.status
                            .as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case("synced"))
                        && e.seq == bc.seq
                })
            {
                return Some(by_seq);
            }
            events.iter().find(|e| {
                e.kind == "web_feedback"
                    && e.status
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("synced"))
                    && (bc.seq == 0 || e.seq == 0)
                    && e.ts >= bc.ts
                    && e.ts <= bc.ts + 15.0
            })
        });
        let map_id = frames[span.end_frame_idx + 1..]
            .iter()
            .take_while(|f| f.mode.as_deref() != Some("map_m"))
            .find(|f| {
                matches!(f.mode.as_deref(), Some("minimap") | Some("minimap_fast"))
                    && f.state.as_deref() == Some("TRACKING")
                    && f.map_id.is_some()
            })
            .and_then(|f| f.map_id);

        // The app records `hud_feedback` in the same call that handles the web feedback, so the
        // answer sits right after it with the same event type and seq.
        let in_game_ev = hud_ev.filter(|_| in_game_audited).map(|web| {
            events.iter().find(|e| {
                e.kind == "hud_feedback"
                    && e.seq == web.seq
                    && e.event_type.as_deref().map(str::to_ascii_uppercase)
                        == web.event_type.as_deref().map(str::to_ascii_uppercase)
                    && e.ts >= web.ts - 0.5
                    && e.ts <= web.ts + 2.0
            })
        });
        let in_game_feedback = in_game_ev.map(|answer| {
            answer.is_some_and(|e| e.shown == Some(true) && e.delivered != Some(false))
        });
        let in_game_skip_reason = in_game_ev
            .flatten()
            .filter(|e| e.shown != Some(true))
            .and_then(|e| e.reason.clone());

        opens.push(MMapOpenLatency {
            open_ts: span.open_ts,
            map_id,
            engine_synced_s: span.engine_synced_ts.map(|t| round3(t - span.open_ts)),
            broadcast_s: broadcast_ev.map(|e| round3(e.ts - span.open_ts)),
            hud_s: hud_ev.map(|e| round3(e.ts - span.open_ts)),
            in_game_feedback,
            in_game_skip_reason,
        });
    }

    let hud_vals: Vec<f64> = opens.iter().filter_map(|o| o.hud_s).collect();
    let count = opens.len();
    let no_hud_count = count.saturating_sub(hud_vals.len());
    let summary = MMapLatencySummary {
        count,
        median_hud_s: median_f64(&hud_vals).map(round3),
        max_hud_s: hud_vals.iter().copied().reduce(f64::max).map(round3),
        no_hud_count,
        in_game_audited,
        in_game_ok: opens.iter().filter(|o| o.in_game_feedback == Some(true)).count(),
        in_game_missing: opens.iter().filter(|o| o.in_game_feedback == Some(false)).count(),
    };

    LiveMMapLatency {
        available: true,
        opens,
        summary,
    }
}

/// Loads `meta.jsonl` and computes live M-Map HUD latency stats (informational only).
pub fn load_live_mmap_latency(meta_path: &Path) -> LiveMMapLatency {
    let Ok(file) = std::fs::File::open(meta_path) else {
        return LiveMMapLatency::default();
    };
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            return LiveMMapLatency::default();
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            return LiveMMapLatency::default();
        };
        records.push(v);
    }
    compute_live_mmap_latency(&records)
}

/// Evaluates heading stability of `replay` against `blessed` (when available).
///
/// - When `blessed` is `Some(b)`: fails if replay flips or spikes (body or camera) exceed the
///   blessed counts, with a reason naming kind, metric, actual, and blessed value.
/// - When `blessed` is `None`: does not fail; records a note that a re-bless is needed.
pub fn evaluate_heading_stability_gate(
    replay: &HeadingStabilityPair,
    blessed: Option<&HeadingStabilityPair>,
) -> HeadingStabilityGateOutcome {
    let Some(blessed) = blessed else {
        return HeadingStabilityGateOutcome {
            fail_reasons: Vec::new(),
            notes: vec![REBLESS_HEADING_STABILITY_NOTE.to_owned()],
        };
    };

    let mut fail_reasons = Vec::new();
    let checks = [
        ("body", "flips", replay.body.flips, blessed.body.flips),
        ("body", "spikes", replay.body.spikes, blessed.body.spikes),
        ("camera", "flips", replay.camera.flips, blessed.camera.flips),
        ("camera", "spikes", replay.camera.spikes, blessed.camera.spikes),
    ];
    for (kind, metric, actual, base) in checks {
        if actual > base {
            fail_reasons.push(format!(
                "heading stability {kind} {metric}: actual={actual} > blessed={base}"
            ));
        }
    }

    HeadingStabilityGateOutcome {
        fail_reasons,
        notes: Vec::new(),
    }
}

/// Frame-by-frame comparison of fresh `actual` poses against `blessed` poses.
pub fn compare_poses_against_blessed(
    blessed: &[ObservedFrame],
    actual: &[ObservedFrame],
    pos_threshold_w: f64,
) -> BlessedComparison {
    let mut frame_count_mismatch = blessed.len() != actual.len();
    let mut map_mismatches = 0usize;
    let mut first_map_mismatch_idx = None;
    let mut lost_pos_frames = 0usize;
    let mut gained_pos_frames = 0usize;
    let mut over_threshold_frames = 0usize;
    let mut first_over_threshold_idx = None;
    let mut deltas = Vec::with_capacity(blessed.len());
    let mut heading_deltas = Vec::with_capacity(blessed.len());

    for (b, a) in blessed.iter().zip(actual.iter()) {
        if b.idx != a.idx {
            frame_count_mismatch = true;
        }
        if let (Some(bm), Some(am)) = (b.map_id, a.map_id)
            && bm != am
        {
            map_mismatches += 1;
            first_map_mismatch_idx.get_or_insert(b.idx);
        }
        let b_pos = match (b.world_x, b.world_y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
            _ => None,
        };
        let a_pos = match (a.world_x, a.world_y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
            _ => None,
        };
        match (b_pos, a_pos) {
            (Some((bx, by)), Some((ax, ay))) => {
                let d = (ax - bx).hypot(ay - by);
                deltas.push(d);
                if d > pos_threshold_w {
                    over_threshold_frames += 1;
                    first_over_threshold_idx.get_or_insert(b.idx);
                }
            }
            (Some(_), None) => {
                lost_pos_frames += 1;
            }
            (None, Some(_)) => {
                gained_pos_frames += 1;
            }
            (None, None) => {}
        }
        if let (Some(bh), Some(ah)) = (b.heading, a.heading)
            && bh.is_finite()
            && ah.is_finite()
        {
            heading_deltas.push(angle_diff_deg(ah, bh));
        }
    }

    let max_pos_delta_w = deltas.iter().copied().reduce(f64::max).map(round3);
    BlessedComparison {
        blessed_frames: blessed.len(),
        actual_frames: actual.len(),
        frame_count_mismatch,
        compared_pos_frames: deltas.len(),
        map_mismatches,
        first_map_mismatch_idx,
        lost_pos_frames,
        gained_pos_frames,
        over_threshold_frames,
        first_over_threshold_idx,
        median_pos_delta_w: percentile(&deltas, 0.50).map(round3),
        p95_pos_delta_w: percentile(&deltas, 0.95).map(round3),
        max_pos_delta_w,
        median_heading_delta_deg: percentile(&heading_deltas, 0.50).map(round3),
        p95_heading_delta_deg: percentile(&heading_deltas, 0.95).map(round3),
    }
}

/// Evaluates comparison + post-tick telemetry against baseline & effective thresholds.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_session_against_baseline(
    base: &QaBaselineSession,
    cmp: &BlessedComparison,
    actual_map_m_synced: usize,
    actual_mmap_view_acquired: usize,
    hd_downloads: u64,
    eff: &QaThresholds,
    overrides: &QaThresholdOverrides,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if hd_downloads > 0 {
        reasons.push(format!("offline leak: hd_downloads={hd_downloads}"));
    }
    if cmp.frame_count_mismatch {
        reasons.push(format!(
            "số frame khác blessed ({} != {})",
            cmp.actual_frames, cmp.blessed_frames
        ));
    }
    if cmp.map_mismatches > eff.max_map_mismatches {
        let first = cmp
            .first_map_mismatch_idx
            .map(|f| format!(", đầu tiên f={f}"))
            .unwrap_or_default();
        reasons.push(format!(
            "lệch map_id {} frame (ngưỡng {}{})",
            cmp.map_mismatches, eff.max_map_mismatches, first
        ));
    }
    if cmp.over_threshold_frames > eff.max_over_threshold_frames {
        let max_s = cmp
            .max_pos_delta_w
            .map(|v| format!("{v:.2}w"))
            .unwrap_or_else(|| "-".to_owned());
        let first = cmp
            .first_over_threshold_idx
            .map(|f| format!(", đầu tiên f={f}"))
            .unwrap_or_default();
        reasons.push(format!(
            "vị trí lệch >{:.1}w: {} frame (max {max_s}, ngưỡng {}{})",
            eff.pos_threshold_w, cmp.over_threshold_frames, eff.max_over_threshold_frames, first
        ));
    }
    if cmp.lost_pos_frames > eff.max_lost_pos_frames {
        reasons.push(format!(
            "mất tracking {} frame so với blessed (ngưỡng {})",
            cmp.lost_pos_frames, eff.max_lost_pos_frames
        ));
    }
    let map_m_drop = base.map_m_synced_count.saturating_sub(actual_map_m_synced);
    if map_m_drop > eff.max_map_m_synced_drop {
        reasons.push(format!(
            "map_m_synced giảm {}→{} (giảm {}, ngưỡng {})",
            base.map_m_synced_count, actual_map_m_synced, map_m_drop, eff.max_map_m_synced_drop
        ));
    }
    let view_drop = base
        .mmap_view_acquired
        .saturating_sub(actual_mmap_view_acquired);
    if view_drop > eff.max_mmap_view_drop {
        reasons.push(format!(
            "mmap_view_acquired giảm {}→{} (giảm {}, ngưỡng {})",
            base.mmap_view_acquired, actual_mmap_view_acquired, view_drop, eff.max_mmap_view_drop
        ));
    }
    if let Some(min_req) = overrides.min_mmap_view_acquired
        && actual_mmap_view_acquired < min_req
    {
        reasons.push(format!(
            "mmap_view_acquired {actual_mmap_view_acquired} < ngưỡng CLI {min_req}"
        ));
    }
    if let Some(max_p95) = overrides.max_p95_pos_delta_w {
        let actual_p95 = cmp.p95_pos_delta_w.unwrap_or(0.0);
        if actual_p95 > max_p95 {
            reasons.push(format!(
                "p95 pos delta {actual_p95:.3}w > ngưỡng CLI {max_p95:.3}w"
            ));
        }
    }
    if let Some(max_lim) = overrides.max_pos_delta_w {
        let actual_max = cmp.max_pos_delta_w.unwrap_or(0.0);
        if actual_max > max_lim {
            reasons.push(format!(
                "max pos delta {actual_max:.3}w > ngưỡng CLI {max_lim:.3}w"
            ));
        }
    }
    reasons
}

fn meta_ref_from_report(report: &ReplayReport) -> MetaReferenceSummary {
    MetaReferenceSummary {
        compared_pos_frames: report.regression.all_frames.compared_pos_frames,
        map_mismatches: report.regression.map_mismatches,
        over_threshold_frames: report.regression.all_frames.frames_over_threshold,
        median_pos_delta_w: report.regression.all_frames.median_pos_delta_w,
        p95_pos_delta_w: report.regression.all_frames.p95_pos_delta_w,
        max_pos_delta_w: report.regression.all_frames.max_pos_delta_w,
    }
}

fn fmt_opt_w(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}w"))
        .unwrap_or_else(|| "-".to_owned())
}

pub fn format_qa_header() -> String {
    format!(
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:<20} {:<20} {:<13} {:<10} {:<10} {:<20} {:<24} {:<26} {:>6}  {}",
        "Session",
        "Status",
        "Frames",
        "Codec(CS)",
        "vs Blessed (map/ov/p95/max)",
        "MMap(sync/acq)",
        "vs Meta[ref]",
        "Arrow (b/c flips)",
        "Smooth(cSt/p95 bSt)",
        "HUD(p50/max)",
        "InGame",
        "Prov",
        "Unrec",
        "Nav",
        "Truth",
        "Time",
        "Reason"
    )
}

/// Informational navigation input replay column:
/// `hk <dispatched>/<recorded>`, appending ` web <n>` when web inputs exist and
/// ` nav <mismatch>/<compared>` when live `nav` exists; `n/a` when no hotkeys and no web inputs.
pub fn format_nav_col(input: Option<&InputReplayReport>) -> String {
    let Some(ir) = input else {
        return "n/a".to_owned();
    };
    if ir.hotkeys_recorded == 0
        && ir.web_inputs_delivered == 0
        && !ir.initial_state_applied
        && ir.nav_frames_compared == 0
    {
        return "n/a".to_owned();
    }
    let mut out = format!("hk {}/{}", ir.hotkeys_dispatched, ir.hotkeys_recorded);
    if ir.web_inputs_delivered > 0 {
        out.push_str(&format!(" web {}", ir.web_inputs_delivered));
    }
    if ir.nav_frames_compared > 0 {
        if ir.nav_carried_over_mismatches > 0 {
            out.push_str(&format!(
                " nav {}/{} (+{}pre)",
                ir.nav_mismatches, ir.nav_frames_compared, ir.nav_carried_over_mismatches
            ));
        } else {
            out.push_str(&format!(
                " nav {}/{}",
                ir.nav_mismatches, ir.nav_frames_compared
            ));
        }
    }
    out
}

/// Informational unrecorded-ticks column (`mm <minimap+fast> map <map_m+paused> set <settling>`,
/// appending `idl <n>` when `idle_closed > 0`), or `n/a` when `meta.jsonl` has no `tick_seq`.
pub fn format_unrec_col(fidelity: Option<&RecordingFidelityReport>) -> String {
    let Some(f) = fidelity else {
        return "n/a".to_owned();
    };
    let get = |k: &str| f.unrecorded_by_mode.get(k).copied().unwrap_or(0);
    let mm = get("minimap") + get("minimap_fast");
    let map = get("map_m") + get("map_m_paused");
    let set = get("map_m_settling");
    let idl = get("idle_closed");
    if idl > 0 {
        format!("mm {mm} map {map} set {set} idl {idl}")
    } else {
        format!("mm {mm} map {map} set {set}")
    }
}

/// Informational provenance column marker (`ok`, `data!`, `code=`, `data!code=`, `code!`,
/// `data!code!`, `code?`, `data!code?`, or `n/a` when the session has no provenance stamp).
pub fn format_prov_col(prov: Option<&ReplayProvenance>) -> String {
    let Some(p) = prov else {
        return "n/a".to_owned();
    };
    if p.recorded.is_none() {
        return "n/a".to_owned();
    }
    let data_diff = p.datasets_match == Some(false);
    let code_tag = match (p.same_commit, p.engine_code_unchanged) {
        (None, _) => "code?",
        (Some(true), _) => "",
        (Some(false), Some(true)) => "code=",
        (Some(false), Some(false)) | (Some(false), None) => "code!",
    };
    match (data_diff, code_tag) {
        (false, "") => "ok".to_owned(),
        (true, "") => "data!".to_owned(),
        (false, tag) => tag.to_owned(),
        (true, tag) => format!("data!{tag}"),
    }
}

fn short_commit(sha: &str) -> &str {
    let t = sha.trim();
    if t.len() >= 8 { &t[..8] } else { t }
}

/// Informational provenance notes (never affect pass/fail or thresholds).
pub fn provenance_notes(prov: Option<&ReplayProvenance>, current_commit: &str) -> Vec<String> {
    let Some(p) = prov else {
        return vec!["provenance unknown".to_owned()];
    };
    let Some(ref rec) = p.recorded else {
        return vec!["provenance unknown".to_owned()];
    };
    let effective_current = p.current_commit.as_deref().unwrap_or(current_commit);
    let mut notes = Vec::new();
    if p.datasets_match == Some(false) {
        notes.push(format!(
            "datasets differ (rec={}, replay={})",
            rec.data_source, p.replay_data_source
        ));
    }
    match (p.same_commit, p.engine_code_unchanged) {
        (Some(false), Some(true)) => {
            notes.push(format!(
                "commit differs (rec={}, cur={}, engine=)",
                short_commit(&rec.git_commit),
                short_commit(effective_current)
            ));
        }
        (Some(false), _) => {
            notes.push(format!(
                "commit differs (rec={}, cur={})",
                short_commit(&rec.git_commit),
                short_commit(effective_current)
            ));
        }
        (None, _) => {
            notes.push("commit unknown".to_owned());
        }
        (Some(true), _) => {}
    }
    notes
}

/// `ok/total` web-confirmed M-opens that got an in-game toast, `MISS` when any did not, `-` when
/// none were confirmed, `n/a` for recordings without `hud_feedback` events.
fn format_in_game_col(lat: &LiveMMapLatency) -> String {
    if !lat.available || !lat.summary.in_game_audited {
        return "n/a".to_owned();
    }
    let ok = lat.summary.in_game_ok;
    let total = ok + lat.summary.in_game_missing;
    if total == 0 {
        "-".to_owned()
    } else if lat.summary.in_game_missing == 0 {
        format!("{ok}/{total}")
    } else {
        format!("{ok}/{total} MISS")
    }
}

pub fn format_qa_row(r: &QaSessionResult) -> String {
    let cs = if r.color_safe { "Y" } else { "N" };
    let codec_col = format!("{}({cs})", r.codec);
    let blessed_col = match &r.vs_blessed {
        Some(c) => format!(
            "{}/{}/{}/{}",
            c.map_mismatches,
            c.over_threshold_frames,
            fmt_opt_w(c.p95_pos_delta_w),
            fmt_opt_w(c.max_pos_delta_w)
        ),
        None => "-".to_owned(),
    };
    let mmap_col = format!("{}/{}", r.map_m_synced_count, r.mmap_view_acquired);
    let meta_col = format!(
        "ov{}/{}",
        r.vs_meta_ref.over_threshold_frames,
        fmt_opt_w(r.vs_meta_ref.max_pos_delta_w)
    );
    let hs = &r.heading_stability;
    let arrow_col = match hs.live_meta {
        Some(live) => format!(
            "b{}/c{} live b{}/c{}",
            hs.replay.body.flips, hs.replay.camera.flips, live.body.flips, live.camera.flips
        ),
        None => format!(
            "b{}/c{} live -",
            hs.replay.body.flips, hs.replay.camera.flips
        ),
    };
    let smooth_col = {
        let rs = &hs.replay_smoothness;
        let p95_s = rs
            .camera
            .move_jerk_p95_deg
            .map(|v| format!("{v:.1}°"))
            .unwrap_or_else(|| "-".to_owned());
        format!("c{}/{} b{}", rs.camera.stutters, p95_s, rs.body.stutters)
    };
    let hud_col = match (
        r.live_mmap_latency.available,
        r.live_mmap_latency.summary.median_hud_s,
        r.live_mmap_latency.summary.max_hud_s,
    ) {
        (true, Some(med), Some(max)) => format!("{med:.1}/{max:.1}s"),
        _ => "n/a".to_owned(),
    };
    let in_game_col = format_in_game_col(&r.live_mmap_latency);
    let prov_col = format_prov_col(r.provenance.as_ref());
    let unrec_col = format_unrec_col(r.recording_fidelity.as_ref());
    let nav_col = format_nav_col(r.input_replay.as_ref());
    let truth_col = format_truth_col(r.truth.as_ref());
    format!(
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:<20} {:<20} {:<13} {:<10} {:<10} {:<20} {:<24} {:<26} {:>5.1}s  {}",
        r.session_name,
        r.status,
        r.frames,
        codec_col,
        blessed_col,
        mmap_col,
        meta_col,
        arrow_col,
        smooth_col,
        hud_col,
        in_game_col,
        prov_col,
        unrec_col,
        nav_col,
        truth_col,
        r.wall_elapsed_s,
        r.reason
    )
}

/// Computes bounded worker count from logical CPU count, available/total RAM (MiB), and work items.
pub fn compute_auto_jobs(
    logical_cpus: usize,
    avail_ram_mb: Option<u64>,
    total_ram_mb: Option<u64>,
    work_items: usize,
) -> usize {
    let cpus = logical_cpus.max(1);
    let cpu_bound = (cpus / 3).clamp(1, 12);
    let ram_budget_mb = match (avail_ram_mb, total_ram_mb) {
        (Some(avail), Some(total)) => avail.min((total * 3) / 4),
        (Some(avail), None) => avail,
        (None, Some(total)) => (total * 3) / 4,
        (None, None) => 8192,
    };
    let mem_bound = (ram_budget_mb.saturating_sub(2048) / 768).max(1) as usize;
    cpu_bound.min(mem_bound).clamp(1, work_items.max(1))
}

#[allow(clippy::too_many_arguments)]
fn finalize_session_result(
    sdir: &Path,
    session_name: String,
    codec: String,
    color_safe: bool,
    replay_out: ReplayFullOutput,
    opts: &QaOptions,
    eff_thresholds: &QaThresholds,
    baseline_session: Option<&QaBaselineSession>,
) -> Result<(QaSessionResult, Option<QaBaselineSession>), String> {
    let report = replay_out.report;
    let poses = replay_out.observed;
    if let Some(ref dump_dir) = opts.dump_poses_dir {
        std::fs::create_dir_all(dump_dir)
            .map_err(|e| format!("failed to create --dump-poses-dir {}: {e}", dump_dir.display()))?;
        let dump_path = dump_dir.join(format!("{session_name}.jsonl"));
        write_blessed_poses_atomic(&dump_path, &poses)?;
    }

    let session_truth = Some(evaluate_truth_oracle(
        &poses,
        &report.accuracy,
        &replay_out.truth_meta,
    ));
    let meta_ref = meta_ref_from_report(&report);
    let replay_heading_stability = compute_observed_heading_stability(&poses);
    let replay_heading_smoothness = compute_observed_heading_smoothness(&poses);
    let meta_path = sdir.join("meta.jsonl");
    let live_meta_samples = load_live_meta_heading_samples(&meta_path);
    let live_meta_heading_stability = live_meta_samples
        .as_ref()
        .map(|(b, c)| HeadingStabilityPair {
            body: compute_heading_stability(b),
            camera: compute_heading_stability(c),
        });
    let live_meta_heading_smoothness = live_meta_samples
        .as_ref()
        .map(|(b, c)| HeadingSmoothnessPair {
            body: compute_heading_smoothness(b),
            camera: compute_heading_smoothness(c),
        });
    let live_mmap_latency = load_live_mmap_latency(&meta_path);
    let session_prov = report.provenance.clone();
    let session_fidelity = report.recording_fidelity.clone();
    let session_input_replay = Some(report.input_replay.clone());
    let prov_cur_commit = session_prov
        .current_commit
        .as_deref()
        .unwrap_or(BUILD_GIT_COMMIT);
    let mut prov_notes = provenance_notes(Some(&session_prov), prov_cur_commit);
    if report.input_replay.nav_carried_over_mismatches > 0 {
        prov_notes.push(format!(
            "nav carried-over pre-web: {} frames (legacy recording without initial_state)",
            report.input_replay.nav_carried_over_mismatches
        ));
    }
    let blessed_path = sdir.join(BLESSED_POSES_FILENAME);

    if opts.bless {
        let sha256 = write_blessed_poses_atomic(&blessed_path, &poses)?;
        let tracking_frames = poses
            .iter()
            .filter(|p| {
                p.world_x.is_some_and(f64::is_finite) && p.world_y.is_some_and(f64::is_finite)
            })
            .count();
        let entry = QaBaselineSession {
            blessed_poses_sha256: sha256,
            frames: poses.len(),
            codec: codec.clone(),
            color_safe,
            tracking_frames,
            map_m_synced_count: report.post_tick.map_m_synced_count,
            mmap_view_frames: report.post_tick.mmap_view_frames,
            mmap_view_acquired: report.post_tick.mmap_view_acquired,
            mmap_view_drawer_open: report.post_tick.mmap_view_drawer_open,
            meta_ref_p95_pos_delta_w: meta_ref.p95_pos_delta_w,
            meta_ref_max_pos_delta_w: meta_ref.max_pos_delta_w,
            meta_ref_over_threshold: meta_ref.over_threshold_frames,
        };

        let self_cmp =
            compare_poses_against_blessed(&poses, &poses, eff_thresholds.pos_threshold_w);
        let mut reasons = Vec::new();
        if report.offline_assets.hd_downloads > 0 {
            reasons.push(format!(
                "offline leak: hd_downloads={}",
                report.offline_assets.hd_downloads
            ));
        }
        let passed = reasons.is_empty();
        let notes = prov_notes;
        let res = QaSessionResult {
            session_name,
            passed,
            status: if passed {
                "BLESSED".to_owned()
            } else {
                "KHÔNG ĐẠT".to_owned()
            },
            reason: if passed {
                if notes.is_empty() {
                    "blessed ok".to_owned()
                } else {
                    format!("blessed ok ({})", notes.join("; "))
                }
            } else {
                reasons.join("; ")
            },
            reasons,
            notes,
            frames: report.performance.frames_processed,
            codec,
            color_safe,
            wall_elapsed_s: report.performance.wall_elapsed_s,
            effective_fps: report.performance.effective_fps,
            hd_downloads: report.offline_assets.hd_downloads,
            map_m_synced_count: report.post_tick.map_m_synced_count,
            baseline_map_m_synced_count: Some(report.post_tick.map_m_synced_count),
            mmap_view_frames: report.post_tick.mmap_view_frames,
            mmap_view_acquired: report.post_tick.mmap_view_acquired,
            baseline_mmap_view_acquired: Some(report.post_tick.mmap_view_acquired),
            mmap_view_drawer_open: report.post_tick.mmap_view_drawer_open,
            blessed_sha256_ok: true,
            vs_blessed: Some(self_cmp),
            vs_meta_ref: meta_ref,
            heading_stability: SessionHeadingStability {
                replay: replay_heading_stability,
                blessed: Some(replay_heading_stability),
                live_meta: live_meta_heading_stability,
                replay_smoothness: replay_heading_smoothness,
                blessed_smoothness: Some(replay_heading_smoothness),
                live_meta_smoothness: live_meta_heading_smoothness,
            },
            live_mmap_latency,
            provenance: Some(session_prov),
            recording_fidelity: session_fidelity,
            input_replay: session_input_replay,
            truth: session_truth,
        };
        return Ok((res, Some(entry)));
    }

    let res = match baseline_session {
        None => QaSessionResult {
            session_name,
            passed: false,
            status: "KHÔNG ĐẠT".to_owned(),
            reason: "thiếu phiên trong tests/qa_baseline.json".to_owned(),
            reasons: vec!["thiếu phiên trong tests/qa_baseline.json".to_owned()],
            notes: prov_notes,
            frames: report.performance.frames_processed,
            codec,
            color_safe,
            wall_elapsed_s: report.performance.wall_elapsed_s,
            effective_fps: report.performance.effective_fps,
            hd_downloads: report.offline_assets.hd_downloads,
            map_m_synced_count: report.post_tick.map_m_synced_count,
            baseline_map_m_synced_count: None,
            mmap_view_frames: report.post_tick.mmap_view_frames,
            mmap_view_acquired: report.post_tick.mmap_view_acquired,
            baseline_mmap_view_acquired: None,
            mmap_view_drawer_open: report.post_tick.mmap_view_drawer_open,
            blessed_sha256_ok: false,
            vs_blessed: None,
            vs_meta_ref: meta_ref,
            heading_stability: SessionHeadingStability {
                replay: replay_heading_stability,
                blessed: None,
                live_meta: live_meta_heading_stability,
                replay_smoothness: replay_heading_smoothness,
                blessed_smoothness: None,
                live_meta_smoothness: live_meta_heading_smoothness,
            },
            live_mmap_latency,
            provenance: Some(session_prov),
            recording_fidelity: session_fidelity,
            input_replay: session_input_replay,
            truth: session_truth,
        },
        Some(base_sess) => {
            let mut reasons = Vec::new();
            let mut notes = Vec::new();
            let mut blessed_sha256_ok = false;
            let mut vs_blessed = None;
            let mut blessed_heading_stability = None;
            let mut blessed_heading_smoothness = None;

            if !blessed_path.is_file() {
                reasons.push(format!("thiếu file {BLESSED_POSES_FILENAME}"));
            } else {
                match sha256_file(&blessed_path) {
                    Err(e) => reasons.push(format!("lỗi đọc sha256 blessed: {e}")),
                    Ok(actual_sha) => {
                        if !actual_sha.eq_ignore_ascii_case(&base_sess.blessed_poses_sha256) {
                            reasons.push(format!(
                                "sha256 {BLESSED_POSES_FILENAME} không khớp baseline"
                            ));
                        } else {
                            blessed_sha256_ok = true;
                        }
                    }
                }
                match load_blessed_poses_with_heading_info(&blessed_path) {
                    Err(e) => reasons.push(format!("lỗi đọc {BLESSED_POSES_FILENAME}: {e}")),
                    Ok((blessed_poses, has_heading_fields)) => {
                        let cmp = compare_poses_against_blessed(
                            &blessed_poses,
                            &poses,
                            eff_thresholds.pos_threshold_w,
                        );
                        let eval_reasons = evaluate_session_against_baseline(
                            base_sess,
                            &cmp,
                            report.post_tick.map_m_synced_count,
                            report.post_tick.mmap_view_acquired,
                            report.offline_assets.hd_downloads,
                            eff_thresholds,
                            &opts.overrides,
                        );
                        reasons.extend(eval_reasons);
                        if has_heading_fields {
                            blessed_heading_stability =
                                Some(compute_observed_heading_stability(&blessed_poses));
                            blessed_heading_smoothness =
                                Some(compute_observed_heading_smoothness(&blessed_poses));
                        }
                        let hdg_gate = evaluate_heading_stability_gate(
                            &replay_heading_stability,
                            blessed_heading_stability.as_ref(),
                        );
                        reasons.extend(hdg_gate.fail_reasons);
                        notes.extend(hdg_gate.notes);
                        vs_blessed = Some(cmp);
                    }
                }
            }

            notes.extend(prov_notes);
            let passed = reasons.is_empty();
            QaSessionResult {
                session_name,
                passed,
                status: if passed {
                    "ĐẠT".to_owned()
                } else {
                    "KHÔNG ĐẠT".to_owned()
                },
                reason: if passed {
                    if notes.is_empty() {
                        "ok".to_owned()
                    } else {
                        format!("ok ({})", notes.join("; "))
                    }
                } else {
                    reasons.join("; ")
                },
                reasons,
                notes,
                frames: report.performance.frames_processed,
                codec,
                color_safe,
                wall_elapsed_s: report.performance.wall_elapsed_s,
                effective_fps: report.performance.effective_fps,
                hd_downloads: report.offline_assets.hd_downloads,
                map_m_synced_count: report.post_tick.map_m_synced_count,
                baseline_map_m_synced_count: Some(base_sess.map_m_synced_count),
                mmap_view_frames: report.post_tick.mmap_view_frames,
                mmap_view_acquired: report.post_tick.mmap_view_acquired,
                baseline_mmap_view_acquired: Some(base_sess.mmap_view_acquired),
                mmap_view_drawer_open: report.post_tick.mmap_view_drawer_open,
                blessed_sha256_ok,
                vs_blessed,
                vs_meta_ref: meta_ref,
                heading_stability: SessionHeadingStability {
                    replay: replay_heading_stability,
                    blessed: blessed_heading_stability,
                    live_meta: live_meta_heading_stability,
                    replay_smoothness: replay_heading_smoothness,
                    blessed_smoothness: blessed_heading_smoothness,
                    live_meta_smoothness: live_meta_heading_smoothness,
                },
                live_mmap_latency,
                provenance: Some(session_prov),
                recording_fidelity: session_fidelity,
                input_replay: session_input_replay,
                truth: session_truth,
            }
        }
    };
    Ok((res, None))
}

struct OutputCoordinator {
    sessions: Vec<Option<QaSessionResult>>,
    next_session: usize,
    scenarios: Vec<Option<ScenarioResult>>,
    next_scenario: usize,
    bless_mode: bool,
    blessed_entries: BTreeMap<String, QaBaselineSession>,
}

impl OutputCoordinator {
    fn new(session_count: usize, scenario_count: usize, bless_mode: bool) -> Self {
        Self {
            sessions: vec![None; session_count],
            next_session: 0,
            scenarios: vec![None; scenario_count],
            next_scenario: 0,
            bless_mode,
            blessed_entries: BTreeMap::new(),
        }
    }

    fn push_session(&mut self, idx: usize, res: QaSessionResult, entry: Option<QaBaselineSession>) {
        if let Some(e) = entry {
            self.blessed_entries.insert(res.session_name.clone(), e);
        }
        self.sessions[idx] = Some(res);
        self.flush();
    }

    fn push_scenario(&mut self, idx: usize, res: ScenarioResult) {
        self.scenarios[idx] = Some(res);
        self.flush();
    }

    fn flush(&mut self) {
        while self.next_session < self.sessions.len() {
            let Some(ref r) = self.sessions[self.next_session] else {
                break;
            };
            println!("{}", format_qa_row(r));
            let _ = std::io::stdout().flush();
            self.next_session += 1;
        }
        if !self.bless_mode && self.next_session == self.sessions.len() {
            while self.next_scenario < self.scenarios.len() {
                let Some(ref sr) = self.scenarios[self.next_scenario] else {
                    break;
                };
                println!("{}", format_scenario_line(sr));
                let _ = std::io::stdout().flush();
                self.next_scenario += 1;
            }
        }
    }
}

fn count_meta_lines(meta_path: &Path) -> usize {
    let Ok(f) = std::fs::File::open(meta_path) else {
        return 0;
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn session_video_mb(sdir: &Path) -> usize {
    for name in ["video.mkv", "video.mp4"] {
        let p = sdir.join(name);
        if let Ok(meta) = std::fs::metadata(&p) {
            return (meta.len() / (1024 * 1024)) as usize;
        }
    }
    0
}

type DatasetKey = (BTreeMap<String, Option<String>>, Option<String>);

fn dataset_key_for_dir(data_dir: &Path) -> (DatasetKey, DatasetFingerprint) {
    let fp = fingerprint_data_dir(data_dir);
    let ug_path = data_dir.join("underground_db.json");
    let ug_hash = if ug_path.is_file() {
        sha256_file(&ug_path).ok()
    } else {
        None
    };
    ((fp.0.clone(), ug_hash), fp)
}

enum QaWorkItem<'a> {
    Session {
        idx: usize,
        weight: usize,
        sdir: &'a PathBuf,
        session_name: String,
        codec: String,
        color_safe: bool,
        cache_key: Option<String>,
        ctx: Arc<ReplaySharedContext>,
    },
    Scenario {
        idx: usize,
        weight: usize,
        scenario: &'a ScenarioFile,
        sdir: PathBuf,
        cache_key: Option<String>,
        ctx: Arc<ReplaySharedContext>,
    },
}

impl QaWorkItem<'_> {
    fn weight(&self) -> usize {
        match self {
            Self::Session { weight, .. } | Self::Scenario { weight, .. } => *weight,
        }
    }

    fn sort_key(&self) -> (std::cmp::Reverse<usize>, u8, usize) {
        match self {
            Self::Session { weight, idx, .. } => (std::cmp::Reverse(*weight), 0, *idx),
            Self::Scenario { weight, idx, .. } => (std::cmp::Reverse(*weight), 1, *idx),
        }
    }
}

/// Runs `gh-sim qa` (verification mode or `--bless` mode) across discovered sessions.
pub fn run_qa(opts: &QaOptions) -> Result<QaReport, String> {
    if opts.jobs == Some(0) {
        return Err("--jobs must be >= 1".to_owned());
    }
    let session_dirs = discover_sessions(&opts.sessions_dir, &opts.only)?;

    let mut baseline = if opts.bless {
        if opts.baseline_path.is_file() {
            QaBaselineFile::load(&opts.baseline_path)?
        } else {
            QaBaselineFile::default()
        }
    } else {
        if !opts.baseline_path.is_file() {
            return Err(format!(
                "QA baseline file not found: {} (user approval required before running --bless)",
                opts.baseline_path.display()
            ));
        }
        QaBaselineFile::load(&opts.baseline_path)?
    };
    baseline.schema_version = QA_BASELINE_SCHEMA_VERSION;
    if baseline.policy.trim().is_empty() {
        baseline.policy = DEFAULT_QA_POLICY.to_owned();
    }

    let eff_thresholds = opts.overrides.apply_to(&baseline.thresholds)?;
    let loaded_scenarios = if opts.bless {
        Vec::new()
    } else {
        match opts.scenarios_dir.as_deref() {
            Some(dir) => load_scenarios_from_dir(dir, true)?,
            None => load_scenarios_from_dir(Path::new(DEFAULT_SCENARIOS_DIR), false)?,
        }
    };
    let session_dirs_by_name: BTreeMap<String, PathBuf> = session_dirs
        .iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|name| (name.to_owned(), p.clone()))
        })
        .collect();
    let selected_scenarios: Vec<&ScenarioFile> = if opts.bless {
        Vec::new()
    } else {
        loaded_scenarios
            .iter()
            .filter(|sc| session_dirs_by_name.contains_key(&sc.session))
            .collect()
    };

    let total_work = session_dirs.len() + selected_scenarios.len();
    let cache_enabled = !opts.no_cache && !opts.bless && opts.dump_poses_dir.is_none();
    let cache_dir = opts
        .cache_dir
        .clone()
        .unwrap_or_else(default_qa_cache_dir);

    let (current_commit, repo_root) = resolve_current_git_commit_and_root(BUILD_GIT_COMMIT);
    let env_fp = if cache_enabled {
        build_run_env_fingerprint(
            opts.ffmpeg_path.as_deref(),
            &opts.gt_file,
            &opts.baseline_path,
            &eff_thresholds,
            &opts.overrides,
            &current_commit,
        )
    } else {
        None
    };

    let total_t0 = Instant::now();
    let header = format_qa_header();
    let sep = "-".repeat(header.len().min(200));
    println!("{header}");
    println!("{sep}");
    let _ = std::io::stdout().flush();

    let mut cache_hits = 0usize;
    let mut effective_jobs = 1usize;
    let results: Vec<QaSessionResult>;
    let scenario_results: Vec<ScenarioResult>;

    if opts.jobs == Some(1) {
        // Explicit serial path (`--jobs 1`): matches original unshared execution.
        let mut ser_results = Vec::with_capacity(session_dirs.len());
        let mut ds_fp_cache = HashMap::new();
        let mut sess_fp_cache: HashMap<PathBuf, Option<SessionInputFingerprint>> = HashMap::new();

        for sdir in &session_dirs {
            let session_name = sdir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("session")
                .to_owned();
            let sess_fp = if env_fp.is_some() {
                let fp = build_session_input_fingerprint(sdir, &opts.data_dir, &mut ds_fp_cache);
                sess_fp_cache.insert(sdir.clone(), fp.clone());
                fp
            } else {
                None
            };
            let sess_key = match (&env_fp, &sess_fp) {
                (Some(env), Some(sfp)) => {
                    let eng = compute_session_engine_unchanged(
                        sdir,
                        &opts.data_dir,
                        &current_commit,
                        repo_root.as_deref(),
                    );
                    Some(session_cache_key(env, sfp, eng))
                }
                _ => None,
            };
            if let Some(ref key) = sess_key
                && let Some(cached) = load_cached_session(&cache_dir, key)
            {
                cache_hits += 1;
                println!("{}", format_qa_row(&cached));
                let _ = std::io::stdout().flush();
                ser_results.push(cached);
                continue;
            }

            let (codec, color_safe) = read_session_codec_info(sdir);
            let replay_opts = ReplayOptions {
                source_dir: sdir.clone(),
                data_dir: opts.data_dir.clone(),
                gt_file: opts.gt_file.clone(),
                tiles_dir: None,
                ug_dir: None,
                paimon_cache_path: None,
                ffmpeg_path: opts.ffmpeg_path.clone(),
                max_frames: None,
                drift_threshold_w: eff_thresholds.pos_threshold_w,
                verify_mmap_view: true,
                render_out: None,
                out_report: None,
            };
            let replay_out = run_replay_with_hook(&replay_opts, None)?;
            let base_entry = baseline.sessions.get(&session_name).cloned();
            let (result, blessed_entry) = finalize_session_result(
                sdir,
                session_name.clone(),
                codec,
                color_safe,
                replay_out,
                opts,
                &eff_thresholds,
                base_entry.as_ref(),
            )?;
            if let Some(entry) = blessed_entry {
                baseline.sessions.insert(session_name, entry);
            }
            if let Some(ref key) = sess_key {
                store_cached_session(&cache_dir, key, &result);
            }
            println!("{}", format_qa_row(&result));
            let _ = std::io::stdout().flush();
            ser_results.push(result);
        }

        if opts.bless {
            baseline.save_atomic(&opts.baseline_path)?;
        }

        let mut ser_scenarios = Vec::with_capacity(selected_scenarios.len());
        for sc in &selected_scenarios {
            let sdir = &session_dirs_by_name[&sc.session];
            let sess_fp = if env_fp.is_some() {
                if let Some(cached) = sess_fp_cache.get(sdir) {
                    cached.clone()
                } else {
                    let fp = build_session_input_fingerprint(sdir, &opts.data_dir, &mut ds_fp_cache);
                    sess_fp_cache.insert(sdir.clone(), fp.clone());
                    fp
                }
            } else {
                None
            };
            let sc_key = match (&env_fp, &sess_fp) {
                (Some(env), Some(sfp)) => scenario_cache_key(env, sfp, sc),
                _ => None,
            };
            if let Some(ref key) = sc_key
                && let Some(cached) = load_cached_scenario(&cache_dir, key)
            {
                cache_hits += 1;
                println!("{}", format_scenario_line(&cached));
                let _ = std::io::stdout().flush();
                ser_scenarios.push(cached);
                continue;
            }
            let sc_res = run_scenario(
                sc,
                sdir,
                &opts.data_dir,
                &opts.gt_file,
                opts.ffmpeg_path.as_deref(),
            )?;
            if let Some(ref key) = sc_key {
                store_cached_scenario(&cache_dir, key, &sc_res);
            }
            println!("{}", format_scenario_line(&sc_res));
            let _ = std::io::stdout().flush();
            ser_scenarios.push(sc_res);
        }
        results = ser_results;
        scenario_results = ser_scenarios;
    } else {
        // Parallel worker pool + shared read-only datasets + deterministic output coordinator.
        let mut coord = OutputCoordinator::new(
            session_dirs.len(),
            selected_scenarios.len(),
            opts.bless,
        );
        let mut ds_fp_cache = HashMap::new();
        let mut sess_fp_cache: HashMap<PathBuf, Option<SessionInputFingerprint>> = HashMap::new();
        let mut pending_sessions = Vec::new();
        let mut pending_scenarios = Vec::new();

        for (idx, sdir) in session_dirs.iter().enumerate() {
            let session_name = sdir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("session")
                .to_owned();
            let sess_fp = if env_fp.is_some() {
                let fp = build_session_input_fingerprint(sdir, &opts.data_dir, &mut ds_fp_cache);
                sess_fp_cache.insert(sdir.clone(), fp.clone());
                fp
            } else {
                None
            };
            let sess_key = match (&env_fp, &sess_fp) {
                (Some(env), Some(sfp)) => {
                    let eng = compute_session_engine_unchanged(
                        sdir,
                        &opts.data_dir,
                        &current_commit,
                        repo_root.as_deref(),
                    );
                    Some(session_cache_key(env, sfp, eng))
                }
                _ => None,
            };
            if let Some(ref key) = sess_key
                && let Some(cached) = load_cached_session(&cache_dir, key)
            {
                cache_hits += 1;
                coord.push_session(idx, cached, None);
                continue;
            }
            pending_sessions.push((idx, sdir, session_name, sess_key));
        }

        for (idx, &sc) in selected_scenarios.iter().enumerate() {
            let sdir = session_dirs_by_name[&sc.session].clone();
            let sess_fp = if env_fp.is_some() {
                if let Some(cached) = sess_fp_cache.get(&sdir) {
                    cached.clone()
                } else {
                    let fp = build_session_input_fingerprint(&sdir, &opts.data_dir, &mut ds_fp_cache);
                    sess_fp_cache.insert(sdir.clone(), fp.clone());
                    fp
                }
            } else {
                None
            };
            let sc_key = match (&env_fp, &sess_fp) {
                (Some(env), Some(sfp)) => scenario_cache_key(env, sfp, sc),
                _ => None,
            };
            if let Some(ref key) = sc_key
                && let Some(cached) = load_cached_scenario(&cache_dir, key)
            {
                cache_hits += 1;
                coord.push_scenario(idx, cached);
                continue;
            }
            pending_scenarios.push((idx, sc, sdir, sc_key));
        }

        let uncached_count = pending_sessions.len() + pending_scenarios.len();
        if uncached_count > 0 {
            let shared_sandbox = TempSandboxDir::new("gh_sim_qa_shared")?;
            let verified_gt = Arc::new(VerifiedGtFile::load_or_default(&opts.gt_file)?);
            let engine_unchanged_cache = Arc::new(Mutex::new(HashMap::new()));
            let mut ctx_by_key: BTreeMap<DatasetKey, Arc<ReplaySharedContext>> = BTreeMap::new();
            let mut ctx_for_dir = |sdir: &Path| -> Arc<ReplaySharedContext> {
                let (eff_data_dir, _) = resolve_replay_data_dir(sdir, &opts.data_dir);
                let (ds_key, ds_fp) = dataset_key_for_dir(&eff_data_dir);
                if let Some(existing) = ctx_by_key.get(&ds_key) {
                    return Arc::clone(existing);
                }
                let idx_num = ctx_by_key.len();
                let idx_dir = shared_sandbox.0.join(format!("idx_{idx_num}"));
                let _ = std::fs::create_dir_all(&idx_dir);
                let tracking_assets = Arc::new(SharedTrackingAssets::load_for_data_dir(
                    &eff_data_dir,
                    Some(idx_dir.join(gh_cv::global_relocator::GLOBAL_INDEX_FILENAME)),
                    true,
                ));
                let ctx = Arc::new(ReplaySharedContext {
                    tracking_assets,
                    verified_gt: Arc::clone(&verified_gt),
                    current_commit: current_commit.clone(),
                    repo_root: repo_root.clone(),
                    dataset_fingerprint: ds_fp,
                    engine_unchanged_cache: Arc::clone(&engine_unchanged_cache),
                    ffmpeg_threads: None,
                });
                ctx_by_key.insert(ds_key, Arc::clone(&ctx));
                ctx
            };

            let mut weight_cache: HashMap<PathBuf, (usize, usize)> = HashMap::new();
            let mut get_session_stats = |sdir: &Path| -> (usize, usize) {
                if let Some(&v) = weight_cache.get(sdir) {
                    return v;
                }
                let meta_lines = count_meta_lines(&sdir.join("meta.jsonl"));
                let vid_mb = session_video_mb(sdir);
                weight_cache.insert(sdir.to_path_buf(), (meta_lines, vid_mb));
                (meta_lines, vid_mb)
            };

            let mut work_items: Vec<QaWorkItem<'_>> = Vec::with_capacity(uncached_count);
            for (idx, sdir, session_name, cache_key) in pending_sessions {
                let (meta_lines, vid_mb) = get_session_stats(sdir);
                let weight = meta_lines.saturating_mul(10).saturating_add(vid_mb.saturating_mul(5));
                let (codec, color_safe) = read_session_codec_info(sdir);
                let ctx = ctx_for_dir(sdir);
                work_items.push(QaWorkItem::Session {
                    idx,
                    weight,
                    sdir,
                    session_name,
                    codec,
                    color_safe,
                    cache_key,
                    ctx,
                });
            }
            for (idx, sc, sdir, cache_key) in pending_scenarios {
                let (_, vid_mb) = get_session_stats(&sdir);
                let weight = sc
                    .last_needed_frame()
                    .saturating_mul(10)
                    .saturating_add(vid_mb);
                let ctx = ctx_for_dir(&sdir);
                work_items.push(QaWorkItem::Scenario {
                    idx,
                    weight,
                    scenario: sc,
                    sdir,
                    cache_key,
                    ctx,
                });
            }
            work_items.sort_by_key(|w| (w.sort_key(), w.weight()));

            effective_jobs = match opts.jobs {
                Some(n) => n.min(work_items.len().max(1)),
                None => {
                    let cpus = std::thread::available_parallelism()
                        .map(std::num::NonZeroUsize::get)
                        .unwrap_or(1);
                    let (total_mb, avail_mb) = gh_win32::sys_info::query_system_memory_mb()
                        .map(|(t, a)| (Some(t), Some(a)))
                        .unwrap_or((None, None));
                    compute_auto_jobs(cpus, avail_mb, total_mb, work_items.len())
                }
            };

            let queue = Mutex::new(VecDeque::from(work_items));
            let coord_mutex = Mutex::new(coord);
            let abort = AtomicBool::new(false);
            let first_err: Mutex<Option<String>> = Mutex::new(None);
            let baseline_sessions = &baseline.sessions;
            let cache_dir_ref = cache_enabled.then_some(cache_dir.as_path());

            std::thread::scope(|scope| {
                for _worker_id in 0..effective_jobs {
                    scope.spawn(|| {
                        loop {
                            if abort.load(Ordering::Acquire) {
                                break;
                            }
                            let next = {
                                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                                q.pop_front()
                            };
                            let Some(item) = next else {
                                break;
                            };
                            match item {
                                QaWorkItem::Session {
                                    idx,
                                    sdir,
                                    session_name,
                                    codec,
                                    color_safe,
                                    cache_key,
                                    ctx,
                                    ..
                                } => {
                                    let replay_opts = ReplayOptions {
                                        source_dir: sdir.clone(),
                                        data_dir: opts.data_dir.clone(),
                                        gt_file: opts.gt_file.clone(),
                                        tiles_dir: None,
                                        ug_dir: None,
                                        paimon_cache_path: None,
                                        ffmpeg_path: opts.ffmpeg_path.clone(),
                                        max_frames: None,
                                        drift_threshold_w: eff_thresholds.pos_threshold_w,
                                        verify_mmap_view: true,
                                        render_out: None,
                                        out_report: None,
                                    };
                                    let outcome =
                                        run_replay_with_context(&replay_opts, None, Some(&ctx))
                                            .and_then(|replay_out| {
                                                let base_sess =
                                                    baseline_sessions.get(&session_name);
                                                finalize_session_result(
                                                    sdir,
                                                    session_name,
                                                    codec,
                                                    color_safe,
                                                    replay_out,
                                                    opts,
                                                    &eff_thresholds,
                                                    base_sess,
                                                )
                                            });
                                    match outcome {
                                        Ok((res, blessed_entry)) => {
                                            if let (Some(cdir), Some(key)) =
                                                (cache_dir_ref, cache_key.as_deref())
                                            {
                                                store_cached_session(cdir, key, &res);
                                            }
                                            let mut c = coord_mutex
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            c.push_session(idx, res, blessed_entry);
                                        }
                                        Err(e) => {
                                            abort.store(true, Ordering::Release);
                                            let mut fe = first_err
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            if fe.is_none() {
                                                *fe = Some(e);
                                            }
                                            break;
                                        }
                                    }
                                }
                                QaWorkItem::Scenario {
                                    idx,
                                    scenario,
                                    sdir,
                                    cache_key,
                                    ctx,
                                    ..
                                } => {
                                    let outcome = run_scenario_with_context(
                                        scenario,
                                        &sdir,
                                        &opts.data_dir,
                                        &opts.gt_file,
                                        opts.ffmpeg_path.as_deref(),
                                        Some(&ctx),
                                    );
                                    match outcome {
                                        Ok(sc_res) => {
                                            if let (Some(cdir), Some(key)) =
                                                (cache_dir_ref, cache_key.as_deref())
                                            {
                                                store_cached_scenario(cdir, key, &sc_res);
                                            }
                                            let mut c = coord_mutex
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            c.push_scenario(idx, sc_res);
                                        }
                                        Err(e) => {
                                            abort.store(true, Ordering::Release);
                                            let mut fe = first_err
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            if fe.is_none() {
                                                *fe = Some(e);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
            });

            if let Some(err) = first_err.into_inner().unwrap_or(None) {
                return Err(err);
            }
            coord = coord_mutex.into_inner().unwrap_or_else(|e| e.into_inner());
        }

        if opts.bless {
            for (k, v) in coord.blessed_entries {
                baseline.sessions.insert(k, v);
            }
            baseline.save_atomic(&opts.baseline_path)?;
        }
        results = coord
            .sessions
            .into_iter()
            .map(|o| o.expect("session result populated"))
            .collect();
        scenario_results = coord
            .scenarios
            .into_iter()
            .map(|o| o.expect("scenario result populated"))
            .collect();
    }

    let passed_sessions = results.iter().filter(|r| r.passed).count();
    let total_sessions = results.len();
    let failed_sessions = total_sessions.saturating_sub(passed_sessions);
    let scenario_summary = ScenarioSummary::from_results(&scenario_results);
    let total_wall_elapsed_s = round3(total_t0.elapsed().as_secs_f64());
    let all_passed = failed_sessions == 0 && scenario_summary.fail == 0;

    println!("{sep}");
    if !opts.bless {
        println!("{}", scenario_summary.format_line());
    }
    if !results.is_empty() {
        let rb: usize = results.iter().map(|r| r.heading_stability.replay.body.flips).sum();
        let rc: usize = results.iter().map(|r| r.heading_stability.replay.camera.flips).sum();
        let rcs: usize = results
            .iter()
            .map(|r| r.heading_stability.replay_smoothness.camera.stutters)
            .sum();
        let rbs: usize = results
            .iter()
            .map(|r| r.heading_stability.replay_smoothness.body.stutters)
            .sum();
        let max_p95 = results
            .iter()
            .filter_map(|r| r.heading_stability.replay_smoothness.camera.move_jerk_p95_deg)
            .reduce(f64::max)
            .map(|v| format!("{v:.1}°"))
            .unwrap_or_else(|| "-".to_owned());

        let mut flip_notes = Vec::new();
        let mut stut_notes = Vec::new();
        if results.iter().any(|r| r.heading_stability.blessed.is_some()) {
            let bb: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.blessed.map(|b| b.body.flips))
                .sum();
            let bc: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.blessed.map(|b| b.camera.flips))
                .sum();
            flip_notes.push(format!("blessed b{bb}/c{bc}"));
        }
        if results.iter().any(|r| r.heading_stability.live_meta.is_some()) {
            let lb: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.live_meta.map(|l| l.body.flips))
                .sum();
            let lc: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.live_meta.map(|l| l.camera.flips))
                .sum();
            flip_notes.push(format!("live b{lb}/c{lc}"));
        }
        if results
            .iter()
            .any(|r| r.heading_stability.blessed_smoothness.is_some())
        {
            let bcs: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.blessed_smoothness.map(|b| b.camera.stutters))
                .sum();
            let bbs: usize = results
                .iter()
                .filter_map(|r| r.heading_stability.blessed_smoothness.map(|b| b.body.stutters))
                .sum();
            stut_notes.push(format!("blessed c{bcs}/b{bbs}"));
        }
        if results
            .iter()
            .any(|r| r.heading_stability.live_meta_smoothness.is_some())
        {
            let lcs: usize = results
                .iter()
                .filter_map(|r| {
                    r.heading_stability
                        .live_meta_smoothness
                        .map(|l| l.camera.stutters)
                })
                .sum();
            let lbs: usize = results
                .iter()
                .filter_map(|r| {
                    r.heading_stability
                        .live_meta_smoothness
                        .map(|l| l.body.stutters)
                })
                .sum();
            stut_notes.push(format!("live c{lcs}/b{lbs}"));
        }
        let flip_sfx = if flip_notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", flip_notes.join(", "))
        };
        let stut_sfx = if stut_notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", stut_notes.join(", "))
        };
        println!(
            "Heading: flips b{rb}/c{rc}{flip_sfx} | stutters c{rcs}/b{rbs}{stut_sfx} | cam move-jerk p95 max={max_p95}"
        );
    }
    println!(
        "Tổng: {passed_sessions}/{total_sessions} {} | thời gian: {total_wall_elapsed_s:.1}s | jobs: {effective_jobs} | cache: {cache_hits}/{total_work} | baseline: {}",
        if opts.bless { "BLESSED" } else { "ĐẠT" },
        opts.baseline_path.display()
    );

    let report = QaReport {
        schema_version: QA_REPORT_SCHEMA_VERSION,
        mode: if opts.bless {
            "bless".to_owned()
        } else {
            "qa".to_owned()
        },
        passed: all_passed,
        passed_sessions,
        failed_sessions,
        total_sessions,
        total_wall_elapsed_s,
        baseline_path: opts.baseline_path.display().to_string(),
        effective_thresholds: eff_thresholds,
        cli_overrides: opts.overrides.clone(),
        sessions: results,
        scenario_summary,
        scenarios: scenario_results,
    };

    if let Some(ref out_dir) = opts.out_dir {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| format!("failed to create --out dir {}: {e}", out_dir.display()))?;
        let report_path = out_dir.join(QA_REPORT_FILENAME);
        let val = serde_json::to_value(&report)
            .map_err(|e| format!("failed to serialize qa_report: {e}"))?;
        write_json_atomic(&report_path, &val)
            .map_err(|e| format!("failed to write {}: {e}", report_path.display()))?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn mk_pose(idx: usize, map_id: Option<i32>, x: Option<f64>, y: Option<f64>) -> ObservedFrame {
        ObservedFrame {
            idx,
            ts: idx as f64 * 0.05,
            state: "TRACKING".to_owned(),
            match_method: "hd_sobel_fast".to_owned(),
            map_id,
            world_x: x,
            world_y: y,
            heading: Some(90.0),
            body_heading: None,
            camera_heading: None,
            is_underground: false,
        }
    }

    fn mk_base(frames: usize, map_m_synced: usize, view_acq: usize) -> QaBaselineSession {
        QaBaselineSession {
            blessed_poses_sha256: "deadbeef".to_owned(),
            frames,
            codec: "libx264rgb".to_owned(),
            color_safe: true,
            tracking_frames: frames,
            map_m_synced_count: map_m_synced,
            mmap_view_frames: view_acq + 5,
            mmap_view_acquired: view_acq,
            mmap_view_drawer_open: 0,
            meta_ref_p95_pos_delta_w: Some(0.5),
            meta_ref_max_pos_delta_w: Some(1.2),
            meta_ref_over_threshold: 0,
        }
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "{prefix}_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn codec_color_safety_marks_libx265_false_and_libx264rgb_true() {
        assert!(!is_codec_color_safe("libx265"));
        assert!(!is_codec_color_safe("hevc"));
        assert!(!is_codec_color_safe("libx264"));
        assert!(is_codec_color_safe("libx264rgb"));
        assert!(is_codec_color_safe("ffv1"));
        assert!(is_codec_color_safe("png_frames"));
    }

    #[test]
    fn synthetic_pose_comparison_passes_within_thresholds() {
        let blessed = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(105.0), Some(205.0)),
            mk_pose(3, Some(40), Some(-50.0), Some(80.0)),
        ];
        let actual = vec![
            mk_pose(1, Some(2), Some(100.5), Some(200.2)),
            mk_pose(2, Some(2), Some(106.0), Some(205.0)),
            mk_pose(3, Some(40), Some(-50.0), Some(80.5)),
        ];
        let th = QaThresholds::default();
        let cmp = compare_poses_against_blessed(&blessed, &actual, th.pos_threshold_w);
        assert_eq!(cmp.map_mismatches, 0);
        assert_eq!(cmp.over_threshold_frames, 0);
        assert_eq!(cmp.lost_pos_frames, 0);
        assert!(cmp.max_pos_delta_w.unwrap() < 2.0);

        let base = mk_base(3, 5, 115);
        // mmap_view_acquired drops by 2 (within default tolerance of 2) -> PASS
        let reasons = evaluate_session_against_baseline(
            &base,
            &cmp,
            5,
            113,
            0,
            &th,
            &QaThresholdOverrides::default(),
        );
        assert!(reasons.is_empty(), "expected pass, got: {reasons:?}");
    }

    #[test]
    fn synthetic_pose_comparison_catches_map_mismatch() {
        let blessed = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(40), Some(105.0), Some(205.0)),
        ];
        let actual = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(105.0), Some(205.0)),
        ];
        let th = QaThresholds::default();
        let cmp = compare_poses_against_blessed(&blessed, &actual, th.pos_threshold_w);
        assert_eq!(cmp.map_mismatches, 1);
        assert_eq!(cmp.first_map_mismatch_idx, Some(2));

        let base = mk_base(2, 4, 100);
        let reasons = evaluate_session_against_baseline(
            &base,
            &cmp,
            4,
            100,
            0,
            &th,
            &QaThresholdOverrides::default(),
        );
        assert!(!reasons.is_empty());
        assert!(reasons[0].contains("map_id"), "got: {reasons:?}");
    }

    #[test]
    fn synthetic_pose_comparison_catches_position_drift_over_15w_and_drops() {
        let blessed = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(110.0), Some(210.0)),
        ];
        let actual = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(130.0), Some(210.0)), // 20w drift (> 15w)
        ];
        let th = QaThresholds::default();
        let cmp = compare_poses_against_blessed(&blessed, &actual, th.pos_threshold_w);
        assert_eq!(cmp.over_threshold_frames, 1);
        assert_eq!(cmp.first_over_threshold_idx, Some(2));
        assert_eq!(cmp.max_pos_delta_w, Some(20.0));

        let base = mk_base(2, 5, 115);
        // Also drop map_m_synced (5 -> 4) and mmap_view_acquired (115 -> 112, drop 3 > 2)
        let reasons = evaluate_session_against_baseline(
            &base,
            &cmp,
            4,
            112,
            0,
            &th,
            &QaThresholdOverrides::default(),
        );
        assert_eq!(reasons.len(), 3, "got: {reasons:?}");
        assert!(reasons[0].contains(">15.0w"));
        assert!(reasons[1].contains("map_m_synced"));
        assert!(reasons[2].contains("mmap_view_acquired"));
    }

    #[test]
    fn synthetic_pose_comparison_catches_lost_tracking_and_cli_tighter_override() {
        let blessed = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(110.0), Some(210.0)),
        ];
        let actual_lost = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, None, None, None),
        ];
        let th = QaThresholds::default();
        let cmp_lost = compare_poses_against_blessed(&blessed, &actual_lost, th.pos_threshold_w);
        assert_eq!(cmp_lost.lost_pos_frames, 1);
        let base = mk_base(2, 5, 115);
        let reasons_lost = evaluate_session_against_baseline(
            &base,
            &cmp_lost,
            5,
            115,
            0,
            &th,
            &QaThresholdOverrides::default(),
        );
        assert!(reasons_lost.iter().any(|r| r.contains("mất tracking")));

        // Identical poses still fail when CLI enforces tighter --min-mmap-view-acquired
        let cmp_same = compare_poses_against_blessed(&blessed, &blessed, th.pos_threshold_w);
        let strict = QaThresholdOverrides {
            min_mmap_view_acquired: Some(120),
            ..Default::default()
        };
        let reasons_strict =
            evaluate_session_against_baseline(&base, &cmp_same, 5, 115, 0, &th, &strict);
        assert_eq!(reasons_strict.len(), 1);
        assert!(reasons_strict[0].contains("ngưỡng CLI 120"));
    }

    #[test]
    fn bless_then_qa_cycle_passes_and_detects_tampering() {
        let tmp = TempDir::new("gh_sim_qa_cycle");
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let sessions_dir = tmp.0.join("qa_sessions");
        let sdir = sessions_dir.join("sess_unit_01");
        std::fs::create_dir_all(sdir.join("frames")).unwrap();
        let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
        for i in 1..=3usize {
            img.save(sdir.join("frames").join(format!("{i:06}.png")))
                .unwrap();
        }
        let meta_content = concat!(
            "{\"i\":1,\"ts\":10.0,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n",
            "{\"i\":2,\"ts\":10.05,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n",
            "{\"i\":3,\"ts\":10.10,\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}\n"
        );
        std::fs::write(sdir.join("meta.jsonl"), meta_content).unwrap();
        let info_content = serde_json::to_string_pretty(&json!({
            "codec": "libx265",
            "frames": 3,
            "width": 2560,
            "height": 1440
        }))
        .unwrap();
        std::fs::write(sdir.join("info.json"), &info_content).unwrap();

        let baseline_path = tmp.0.join("qa_baseline.json");
        let gt_file = tmp.0.join("verified_gt.json");
        let out_dir = tmp.0.join("out");

        // Step 1: --bless
        let bless_rep = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: None,
            only: vec![],
            out_dir: Some(out_dir.clone()),
            bless: true,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(bless_rep.passed);
        assert_eq!(bless_rep.total_sessions, 1);
        assert!(sdir.join(BLESSED_POSES_FILENAME).is_file());
        assert!(baseline_path.is_file());
        assert!(out_dir.join(QA_REPORT_FILENAME).is_file());
        // Original files untouched
        assert_eq!(
            std::fs::read_to_string(sdir.join("meta.jsonl")).unwrap(),
            meta_content
        );
        assert_eq!(
            std::fs::read_to_string(sdir.join("info.json")).unwrap(),
            info_content
        );

        let saved_base = QaBaselineFile::load(&baseline_path).unwrap();
        assert!(saved_base.policy.contains("--bless"));
        let entry = saved_base.sessions.get("sess_unit_01").unwrap();
        assert_eq!(entry.codec, "libx265");
        assert!(!entry.color_safe);
        assert_eq!(entry.frames, 3);

        // Step 2: qa passes cleanly
        let qa_rep = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: None,
            only: vec![],
            out_dir: Some(out_dir.clone()),
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(qa_rep.passed);
        assert_eq!(qa_rep.sessions[0].status, "ĐẠT");
        assert!(qa_rep.sessions[0].blessed_sha256_ok);
        assert_eq!(qa_rep.sessions[0].recording_fidelity, None);
        assert_eq!(
            format_unrec_col(qa_rep.sessions[0].recording_fidelity.as_ref()),
            "n/a"
        );

        // Step 3: tampering blessed_poses.jsonl triggers SHA-256 failure
        let mut raw = std::fs::read_to_string(sdir.join(BLESSED_POSES_FILENAME)).unwrap();
        raw.push('\n');
        std::fs::write(sdir.join(BLESSED_POSES_FILENAME), raw).unwrap();
        let tamper_rep = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(!tamper_rep.passed);
        assert_eq!(tamper_rep.sessions[0].status, "KHÔNG ĐẠT");
        assert!(!tamper_rep.sessions[0].blessed_sha256_ok);
    }

    #[test]
    fn apply_to_rejects_looser_and_allows_equal_or_tighter_for_all_baseline_fields() {
        let base = QaThresholds {
            pos_threshold_w: 15.0,
            max_over_threshold_frames: 3,
            max_map_mismatches: 2,
            max_lost_pos_frames: 4,
            max_map_m_synced_drop: 2,
            max_mmap_view_drop: 5,
        };

        // 1. pos_threshold_w (--pos-threshold-w)
        let err = QaThresholdOverrides {
            pos_threshold_w: Some(15.5),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--pos-threshold-w"), "got: {err}");
        assert!(err.contains("15.5"), "got: {err}");
        assert!(err.contains("15"), "got: {err}");

        let eq = QaThresholdOverrides {
            pos_threshold_w: Some(15.0),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap();
        assert_eq!(eq.pos_threshold_w, 15.0);

        let tight = QaThresholdOverrides {
            pos_threshold_w: Some(8.5),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap();
        assert_eq!(tight.pos_threshold_w, 8.5);
        assert_eq!(tight.max_mmap_view_drop, base.max_mmap_view_drop);

        // 2. max_over_threshold_frames (--max-over-threshold)
        let err = QaThresholdOverrides {
            max_over_threshold_frames: Some(4),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--max-over-threshold"), "got: {err}");
        assert!(err.contains('4'), "got: {err}");
        assert!(err.contains('3'), "got: {err}");

        assert_eq!(
            QaThresholdOverrides {
                max_over_threshold_frames: Some(3),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_over_threshold_frames,
            3
        );
        assert_eq!(
            QaThresholdOverrides {
                max_over_threshold_frames: Some(1),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_over_threshold_frames,
            1
        );

        // 3. max_map_mismatches (--max-map-mismatches)
        let err = QaThresholdOverrides {
            max_map_mismatches: Some(999),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--max-map-mismatches"), "got: {err}");
        assert!(err.contains("999"), "got: {err}");
        assert!(err.contains('2'), "got: {err}");

        assert_eq!(
            QaThresholdOverrides {
                max_map_mismatches: Some(2),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_map_mismatches,
            2
        );
        assert_eq!(
            QaThresholdOverrides {
                max_map_mismatches: Some(0),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_map_mismatches,
            0
        );

        // 4. max_lost_pos_frames (--max-lost-pos-frames)
        let err = QaThresholdOverrides {
            max_lost_pos_frames: Some(5),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--max-lost-pos-frames"), "got: {err}");
        assert!(err.contains('5'), "got: {err}");
        assert!(err.contains('4'), "got: {err}");

        assert_eq!(
            QaThresholdOverrides {
                max_lost_pos_frames: Some(4),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_lost_pos_frames,
            4
        );
        assert_eq!(
            QaThresholdOverrides {
                max_lost_pos_frames: Some(2),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_lost_pos_frames,
            2
        );

        // 5. max_map_m_synced_drop (--max-map-m-synced-drop)
        let err = QaThresholdOverrides {
            max_map_m_synced_drop: Some(3),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--max-map-m-synced-drop"), "got: {err}");
        assert!(err.contains('3'), "got: {err}");
        assert!(err.contains('2'), "got: {err}");

        assert_eq!(
            QaThresholdOverrides {
                max_map_m_synced_drop: Some(2),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_map_m_synced_drop,
            2
        );
        assert_eq!(
            QaThresholdOverrides {
                max_map_m_synced_drop: Some(0),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_map_m_synced_drop,
            0
        );

        // 6. max_mmap_view_drop (--max-mmap-view-drop)
        let err = QaThresholdOverrides {
            max_mmap_view_drop: Some(6),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap_err();
        assert!(err.contains("--max-mmap-view-drop"), "got: {err}");
        assert!(err.contains('6'), "got: {err}");
        assert!(err.contains('5'), "got: {err}");

        assert_eq!(
            QaThresholdOverrides {
                max_mmap_view_drop: Some(5),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_mmap_view_drop,
            5
        );
        assert_eq!(
            QaThresholdOverrides {
                max_mmap_view_drop: Some(1),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap()
            .max_mmap_view_drop,
            1
        );

        // Multiple offending flags are listed together in one error message
        let multi_err = QaThresholdOverrides {
            pos_threshold_w: Some(25.0),
            max_map_mismatches: Some(10),
            ..Default::default()
        }
        .apply_to(&QaThresholds::default())
        .unwrap_err();
        assert!(multi_err.contains("--pos-threshold-w"), "got: {multi_err}");
        assert!(
            multi_err.contains("--max-map-mismatches"),
            "got: {multi_err}"
        );
    }

    #[test]
    fn apply_to_rejects_non_finite_and_negative_f64_overrides() {
        let base = QaThresholds::default();
        let bad_values = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001, -10.0];

        for bad in bad_values {
            let err_pos = QaThresholdOverrides {
                pos_threshold_w: Some(bad),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap_err();
            assert!(
                err_pos.contains("--pos-threshold-w"),
                "bad={bad}: {err_pos}"
            );

            let err_p95 = QaThresholdOverrides {
                max_p95_pos_delta_w: Some(bad),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap_err();
            assert!(
                err_p95.contains("--max-p95-pos-delta-w"),
                "bad={bad}: {err_p95}"
            );

            let err_max = QaThresholdOverrides {
                max_pos_delta_w: Some(bad),
                ..Default::default()
            }
            .apply_to(&base)
            .unwrap_err();
            assert!(
                err_max.contains("--max-pos-delta-w"),
                "bad={bad}: {err_max}"
            );
        }

        // Zero is valid (strictest non-negative f64)
        let zero = QaThresholdOverrides {
            pos_threshold_w: Some(0.0),
            max_p95_pos_delta_w: Some(0.0),
            max_pos_delta_w: Some(0.0),
            ..Default::default()
        }
        .apply_to(&base)
        .unwrap();
        assert_eq!(zero.pos_threshold_w, 0.0);
    }

    #[test]
    fn apply_to_allows_cli_only_overrides_and_keeps_evaluation_unchanged() {
        let base = QaThresholds::default();
        let overrides = QaThresholdOverrides {
            min_mmap_view_acquired: Some(120),
            max_p95_pos_delta_w: Some(1.0),
            max_pos_delta_w: Some(2.0),
            ..Default::default()
        };
        let eff = overrides.apply_to(&base).unwrap();
        assert_eq!(eff, base);

        let blessed = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(110.0), Some(210.0)),
        ];
        let actual = vec![
            mk_pose(1, Some(2), Some(100.0), Some(200.0)),
            mk_pose(2, Some(2), Some(113.0), Some(210.0)), // 3.0w delta (<= 15w, > 2.0w max, > 1.0w p95)
        ];
        let cmp = compare_poses_against_blessed(&blessed, &actual, eff.pos_threshold_w);
        let sess_base = mk_base(2, 5, 115);
        let reasons =
            evaluate_session_against_baseline(&sess_base, &cmp, 5, 115, 0, &eff, &overrides);
        assert_eq!(reasons.len(), 3, "got: {reasons:?}");
        assert!(reasons[0].contains("mmap_view_acquired 115 < ngưỡng CLI 120"));
        assert!(reasons[1].contains("p95 pos delta"));
        assert!(reasons[2].contains("max pos delta"));
    }

    #[test]
    fn run_qa_rejects_loosening_override_before_replaying_any_session() {
        let tmp = TempDir::new("gh_sim_qa_reject_early");
        let sessions_dir = tmp.0.join("qa_sessions");
        let sdir = sessions_dir.join("sess_unplayable");
        std::fs::create_dir_all(&sdir).unwrap();
        // Video session with no ffmpeg_path: replay would fail with a missing-ffmpeg error if reached.
        std::fs::write(sdir.join("meta.jsonl"), "").unwrap();
        std::fs::write(sdir.join("video.mkv"), b"not-a-real-mkv").unwrap();

        let baseline_path = tmp.0.join("qa_baseline.json");
        QaBaselineFile::default().save_atomic(&baseline_path).unwrap();

        let err = run_qa(&QaOptions {
            sessions_dir,
            data_dir: tmp.0.join("missing_data"),
            ffmpeg_path: None,
            baseline_path,
            gt_file: tmp.0.join("verified_gt.json"),
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides {
                max_map_mismatches: Some(999),
                ..Default::default()
            },
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap_err();
        assert!(err.contains("--max-map-mismatches"), "got: {err}");
        assert!(err.contains("999"), "got: {err}");
        assert!(err.contains("(0)"), "got: {err}");
    }

    #[test]
    fn heading_stability_synthetic_flips_spikes_gaps_and_360_wrap() {
        let mk = |idx: usize, tracking: bool, heading: Option<f64>| HeadingSample {
            idx,
            tracking,
            heading,
        };
        let samples = vec![
            // Smooth 360/0 wrap: 350 -> 10 is +20 deg (step, NOT flip)
            mk(1, true, Some(350.0)),
            mk(2, true, Some(10.0)),
            // Single-frame spike: 10 -> 55 (+45) -> 12 (-43), |12 - 10| = 2 < 15 -> 1 spike, 0 flips
            mk(3, true, Some(55.0)),
            mk(4, true, Some(12.0)),
            // Large snap/flip: 12 -> 150 (+138 >= 120) -> 1 flip
            mk(5, true, Some(150.0)),
            // Non-tracking gap: must NOT pair (5,6) or (6,7) or bridge (5,7)
            mk(6, false, Some(10.0)),
            mk(7, true, Some(330.0)),
            // Idx gap (7 -> 9): must NOT pair (7,9) even though |wrap(150 - 330)| = 180 >= 120
            mk(9, true, Some(150.0)),
            // Smooth step + spike across 360/0 wrap: 355 -> 40 (+45) -> 358 (-42), |wrap(358-355)|=3 < 15
            mk(10, true, Some(355.0)), // 150 -> 355 is wrap(+205) = -155 -> flip #2
            mk(11, true, Some(40.0)),
            mk(12, true, Some(358.0)),
            // Non-finite heading breaks continuity
            mk(13, true, Some(f64::NAN)),
            mk(14, true, Some(180.0)),
        ];

        // Valid consecutive pairs:
        // (1,2): +20
        // (2,3): +45
        // (3,4): -43   [spike at idx 3]
        // (4,5): +138  [flip #1]
        // (9,10): -155 [flip #2]
        // (10,11): +45
        // (11,12): -42 [spike at idx 11]
        // Total: steps=7, flips=2, spikes=2
        let stab = compute_heading_stability(&samples);
        assert_eq!(
            stab,
            HeadingStability {
                steps: 7,
                flips: 2,
                spikes: 2,
            }
        );

        // Evidence sequence from live recording: body snaps 283 -> 157 (-126 deg = 1 flip)
        let evidence_body: Vec<HeadingSample> = [
            65.0, 51.0, 9.0, 338.0, 312.0, 283.0, 157.0, 133.0, 129.0, 128.0,
        ]
        .iter()
        .enumerate()
        .map(|(k, &h)| mk(k + 1, true, Some(h)))
        .collect();
        assert_eq!(
            compute_heading_stability(&evidence_body),
            HeadingStability {
                steps: 9,
                flips: 1,
                spikes: 0,
            }
        );
    }

    #[test]
    fn heading_smoothness_measures_sweeps_stutters_wrap_and_gaps() {
        let mk = |idx: usize, tracking: bool, heading: Option<f64>| HeadingSample {
            idx,
            tracking,
            heading,
        };

        // 1. Constant +15 deg/step sweep wrapping across 350 -> 5 -> 20: 0 stutters, 0.0 jerk
        let smooth_wrap: Vec<HeadingSample> = [320.0, 335.0, 350.0, 5.0, 20.0, 35.0]
            .iter()
            .enumerate()
            .map(|(k, &h)| mk(k + 1, true, Some(h)))
            .collect();
        let s_wrap = compute_heading_smoothness(&smooth_wrap);
        assert_eq!(s_wrap.moving_pairs, 4);
        assert_eq!(s_wrap.move_jerk_p50_deg, Some(0.0));
        assert_eq!(s_wrap.move_jerk_p95_deg, Some(0.0));
        assert_eq!(s_wrap.move_jerk_max_deg, Some(0.0));
        assert_eq!(s_wrap.stutters, 0);

        // 2. Two real stutter signatures separated by a gap (idx 6 -> 10):
        //    Run A deltas: +15.0, +1.5 (1-frame stall), +28.0 (catch-up) -> 1 stutter
        //    Run B deltas: -14.0, -1.8, 0.0 (2-frame stall), -32.0 (catch-up) -> 1 stutter
        let stalled = vec![
            mk(1, true, Some(100.0)),
            mk(2, true, Some(115.0)),
            mk(3, true, Some(116.5)),
            mk(4, true, Some(144.5)),
            // Gap breaks contiguous run: 144.5 -> 200.0 must NOT count as step or stutter
            mk(10, true, Some(200.0)),
            mk(11, true, Some(186.0)),
            mk(12, true, Some(184.2)),
            mk(13, true, Some(184.2)),
            mk(14, true, Some(152.2)),
            // Tracking lost mid-run breaks continuity
            mk(15, false, Some(300.0)),
            mk(16, true, Some(301.0)),
            mk(17, true, Some(330.0)),
        ];
        let s_stalled = compute_heading_smoothness(&stalled);
        assert_eq!(s_stalled.stutters, 2, "got {s_stalled:?}");
        assert_eq!(s_stalled.moving_pairs, 4, "got {s_stalled:?}");
        assert!(
            s_stalled.move_jerk_p95_deg.unwrap_or(0.0) >= 26.0,
            "expected high p95 jerk, got {s_stalled:?}"
        );
        assert_eq!(s_stalled.move_jerk_max_deg, Some(32.0));
    }

    #[test]
    fn heading_stability_gate_handles_worse_equal_better_and_missing_blessed_fields() {
        let blessed = HeadingStabilityPair {
            body: HeadingStability {
                steps: 100,
                flips: 3,
                spikes: 1,
            },
            camera: HeadingStability {
                steps: 100,
                flips: 1,
                spikes: 0,
            },
        };

        // 1. Equal -> pass, no notes
        let equal_out = evaluate_heading_stability_gate(&blessed, Some(&blessed));
        assert!(equal_out.fail_reasons.is_empty());
        assert!(equal_out.notes.is_empty());

        // 2. Better -> pass, no notes
        let better = HeadingStabilityPair {
            body: HeadingStability {
                steps: 100,
                flips: 0,
                spikes: 0,
            },
            camera: HeadingStability {
                steps: 100,
                flips: 0,
                spikes: 0,
            },
        };
        let better_out = evaluate_heading_stability_gate(&better, Some(&blessed));
        assert!(better_out.fail_reasons.is_empty());
        assert!(better_out.notes.is_empty());

        // 3. Worse on all four counters -> fail reasons name kind, metric, actual, blessed
        let worse = HeadingStabilityPair {
            body: HeadingStability {
                steps: 100,
                flips: 5,
                spikes: 2,
            },
            camera: HeadingStability {
                steps: 100,
                flips: 4,
                spikes: 3,
            },
        };
        let worse_out = evaluate_heading_stability_gate(&worse, Some(&blessed));
        assert_eq!(worse_out.fail_reasons.len(), 4, "got: {:?}", worse_out.fail_reasons);
        assert!(
            worse_out.fail_reasons[0].contains("body")
                && worse_out.fail_reasons[0].contains("flips")
                && worse_out.fail_reasons[0].contains('5')
                && worse_out.fail_reasons[0].contains('3'),
            "got: {}",
            worse_out.fail_reasons[0]
        );
        assert!(
            worse_out.fail_reasons[1].contains("body")
                && worse_out.fail_reasons[1].contains("spikes")
                && worse_out.fail_reasons[1].contains('2')
                && worse_out.fail_reasons[1].contains('1'),
            "got: {}",
            worse_out.fail_reasons[1]
        );
        assert!(
            worse_out.fail_reasons[2].contains("camera")
                && worse_out.fail_reasons[2].contains("flips")
                && worse_out.fail_reasons[2].contains('4')
                && worse_out.fail_reasons[2].contains('1'),
            "got: {}",
            worse_out.fail_reasons[2]
        );
        assert!(
            worse_out.fail_reasons[3].contains("camera")
                && worse_out.fail_reasons[3].contains("spikes")
                && worse_out.fail_reasons[3].contains('3')
                && worse_out.fail_reasons[3].contains('0'),
            "got: {}",
            worse_out.fail_reasons[3]
        );
        assert!(worse_out.notes.is_empty());

        // 4. Blessed lacks new fields -> pass with re-bless note
        let missing_out = evaluate_heading_stability_gate(&worse, None);
        assert!(missing_out.fail_reasons.is_empty());
        assert_eq!(missing_out.notes.len(), 1);
        assert!(
            missing_out.notes[0].contains("re-bless"),
            "got: {:?}",
            missing_out.notes
        );
    }

    #[test]
    fn old_format_blessed_jsonl_still_deserializes_and_flags_missing_heading_fields() {
        let tmp = TempDir::new("gh_sim_old_blessed");
        let old_path = tmp.0.join("blessed_poses.jsonl");
        let old_line = r#"{"idx":1,"ts":308.576,"state":"TRACKING","match_method":"hd_sobel_fast","map_id":2,"world_x":-4911.5,"world_y":-1306.4,"heading":269.33,"is_underground":false}"#;
        std::fs::write(&old_path, format!("{old_line}\n")).unwrap();

        let (poses, has_fields) = load_blessed_poses_with_heading_info(&old_path).unwrap();
        assert!(!has_fields);
        assert_eq!(poses.len(), 1);
        assert_eq!(poses[0].heading, Some(269.33));
        assert_eq!(poses[0].body_heading, None);
        assert_eq!(poses[0].camera_heading, None);

        // Writing via write_blessed_poses_atomic writes the new fields and marks has_fields = true.
        let new_path = tmp.0.join("blessed_new.jsonl");
        let mut p = poses[0].clone();
        p.body_heading = Some(123.5);
        p.camera_heading = Some(269.33);
        write_blessed_poses_atomic(&new_path, &[p]).unwrap();
        let (reloaded, has_new_fields) = load_blessed_poses_with_heading_info(&new_path).unwrap();
        assert!(has_new_fields);
        assert_eq!(reloaded[0].body_heading, Some(123.5));
        assert_eq!(reloaded[0].camera_heading, Some(269.33));
    }

    #[test]
    fn live_meta_heading_stability_requires_minimap_mode_and_tracking_state() {
        let tmp = TempDir::new("gh_sim_live_meta");
        let mp = tmp.0.join("meta.jsonl");
        let content = concat!(
            "{\"i\":1,\"mode\":\"minimap\",\"state\":\"TRACKING\",\"body\":10.0,\"camera\":20.0}\n",
            "{\"i\":2,\"mode\":\"minimap\",\"state\":\"TRACKING\",\"body\":150.0,\"camera\":25.0}\n",
            "{\"i\":3,\"mode\":\"map_m\",\"state\":\"TRACKING\",\"body\":0.0,\"camera\":200.0}\n",
            "{\"i\":4,\"mode\":\"minimap\",\"state\":\"TRACKING\",\"body\":300.0,\"camera\":30.0}\n"
        );
        std::fs::write(&mp, content).unwrap();

        let pair = load_live_meta_heading_stability(&mp).expect("should parse live meta");
        // Only pair (1,2) is valid: frame 3 has mode="map_m" so neither (2,3) nor (3,4) counts.
        assert_eq!(
            pair.body,
            HeadingStability {
                steps: 1,
                flips: 1,
                spikes: 0,
            }
        );
        assert_eq!(
            pair.camera,
            HeadingStability {
                steps: 1,
                flips: 0,
                spikes: 0,
            }
        );

        // Meta without body/camera keys returns None
        let mp_old = tmp.0.join("meta_old.jsonl");
        std::fs::write(&mp_old, "{\"i\":1,\"state\":\"TRACKING\"}\n").unwrap();
        assert_eq!(load_live_meta_heading_stability(&mp_old), None);
        assert_eq!(load_live_meta_heading_smoothness(&mp_old), None);

        let smooth_pair =
            load_live_meta_heading_smoothness(&mp).expect("should parse live meta smoothness");
        assert_eq!(smooth_pair.body.stutters, 0);
        assert_eq!(smooth_pair.camera.moving_pairs, 0);
    }

    #[test]
    fn qa_table_header_and_row_include_arrow_column() {
        let hdr = format_qa_header();
        assert!(hdr.contains("Arrow"), "header missing Arrow: {hdr}");

        let row_with_live = QaSessionResult {
            session_name: "sess_test".to_owned(),
            passed: true,
            status: "ĐẠT".to_owned(),
            reason: "ok".to_owned(),
            reasons: vec![],
            notes: vec![],
            frames: 100,
            codec: "libx264rgb".to_owned(),
            color_safe: true,
            wall_elapsed_s: 1.5,
            effective_fps: 66.7,
            hd_downloads: 0,
            map_m_synced_count: 2,
            baseline_map_m_synced_count: Some(2),
            mmap_view_frames: 10,
            mmap_view_acquired: 10,
            baseline_mmap_view_acquired: Some(10),
            mmap_view_drawer_open: 0,
            blessed_sha256_ok: true,
            vs_blessed: None,
            vs_meta_ref: MetaReferenceSummary::default(),
            heading_stability: SessionHeadingStability {
                replay: HeadingStabilityPair {
                    body: HeadingStability {
                        steps: 50,
                        flips: 8,
                        spikes: 1,
                    },
                    camera: HeadingStability {
                        steps: 50,
                        flips: 0,
                        spikes: 0,
                    },
                },
                blessed: None,
                live_meta: Some(HeadingStabilityPair {
                    body: HeadingStability {
                        steps: 50,
                        flips: 8,
                        spikes: 0,
                    },
                    camera: HeadingStability {
                        steps: 50,
                        flips: 0,
                        spikes: 0,
                    },
                }),
                replay_smoothness: HeadingSmoothnessPair {
                    body: HeadingSmoothness {
                        moving_pairs: 10,
                        move_jerk_p50_deg: Some(2.0),
                        move_jerk_p95_deg: Some(15.0),
                        move_jerk_max_deg: Some(25.0),
                        stutters: 2,
                    },
                    camera: HeadingSmoothness {
                        moving_pairs: 20,
                        move_jerk_p50_deg: Some(1.5),
                        move_jerk_p95_deg: Some(12.4),
                        move_jerk_max_deg: Some(28.0),
                        stutters: 1,
                    },
                },
                ..SessionHeadingStability::default()
            },
            live_mmap_latency: LiveMMapLatency::default(),
            provenance: None,
            recording_fidelity: None,
            input_replay: None,
            truth: None,
        };
        let formatted = format_qa_row(&row_with_live);
        assert!(hdr.contains("Smooth"), "header missing Smooth: {hdr}");
        assert!(
            formatted.contains("b8/c0 live b8/c0"),
            "unexpected row: {formatted}"
        );
        assert!(
            formatted.contains("c1/12.4° b2"),
            "missing Smooth column in row: {formatted}"
        );
        assert!(hdr.contains("HUD"), "header missing HUD: {hdr}");
        assert!(formatted.contains("n/a"), "unavailable latency should print n/a: {formatted}");

        let mut row_with_hud = row_with_live;
        row_with_hud.live_mmap_latency = LiveMMapLatency {
            available: true,
            opens: vec![],
            summary: MMapLatencySummary {
                count: 2,
                median_hud_s: Some(0.9),
                max_hud_s: Some(4.2),
                no_hud_count: 0,
                ..MMapLatencySummary::default()
            },
        };
        let formatted_hud = format_qa_row(&row_with_hud);
        assert!(
            formatted_hud.contains("0.9/4.2s"),
            "unexpected HUD column in row: {formatted_hud}"
        );
        assert!(hdr.contains("InGame"), "header missing InGame: {hdr}");
        assert!(hdr.contains("Prov"), "header missing Prov: {hdr}");

        let mut row_missing = row_with_hud;
        row_missing.live_mmap_latency.summary.in_game_audited = true;
        row_missing.live_mmap_latency.summary.in_game_ok = 1;
        row_missing.live_mmap_latency.summary.in_game_missing = 2;
        let formatted_missing = format_qa_row(&row_missing);
        assert!(
            formatted_missing.contains("1/3 MISS"),
            "missing in-game feedback must show: {formatted_missing}"
        );
        row_missing.live_mmap_latency.summary.in_game_ok = 3;
        row_missing.live_mmap_latency.summary.in_game_missing = 0;
        let formatted_ok = format_qa_row(&row_missing);
        assert!(
            formatted_ok.contains("3/3") && !formatted_ok.contains("MISS"),
            "unexpected InGame column: {formatted_ok}"
        );
    }

    #[test]
    fn live_in_game_feedback_audit_flags_web_synced_opens_without_a_toast() {
        let tmp = TempDir::new("gh_sim_in_game_fb");
        let mp = tmp.0.join("meta.jsonl");

        // 3 M-opens, each confirmed by the web (`synced`):
        // - #1 answered in game with a toast (ok).
        // - #2 the app skipped it because a map tab was visible (the 2nd-monitor regression).
        // - #3 no `hud_feedback` answer at all.
        let lines = vec![
            json!({"i": 1, "ts": 9.5, "mode": "minimap", "state": "TRACKING", "map_id": 2}),
            json!({"i": 2, "ts": 10.0, "mode": "map_m", "state": "MAP_M_SyncedIdle", "events": [
                {"ts": 10.3, "kind": "sync_broadcast", "seq": 10, "msg_type": "SYNC_POSITION_ONLY", "map_id": 2}
            ]}),
            json!({"i": 3, "ts": 11.0, "mode": "minimap", "state": "TRACKING", "map_id": 2, "events": [
                {"ts": 10.6, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 10},
                {"ts": 10.61, "kind": "hud_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 10,
                 "shown": true, "reason": "", "output": "toast", "delivered": true, "sound": "info", "sound_played": true}
            ]}),
            json!({"i": 4, "ts": 20.0, "mode": "map_m", "state": "MAP_M_SyncedIdle", "events": [
                {"ts": 20.3, "kind": "sync_broadcast", "seq": 20, "msg_type": "SYNC_POSITION_ONLY", "map_id": 2}
            ]}),
            json!({"i": 5, "ts": 21.0, "mode": "minimap", "state": "TRACKING", "map_id": 2, "events": [
                {"ts": 20.6, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 20},
                {"ts": 20.61, "kind": "hud_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 20,
                 "shown": false, "reason": "map_tab_visible", "output": "", "delivered": false, "sound": "", "sound_played": false}
            ]}),
            json!({"i": 6, "ts": 30.0, "mode": "map_m", "state": "MAP_M_SyncedIdle", "events": [
                {"ts": 30.3, "kind": "sync_broadcast", "seq": 30, "msg_type": "SYNC_POSITION_ONLY", "map_id": 2}
            ]}),
            json!({"i": 7, "ts": 31.0, "mode": "minimap", "state": "TRACKING", "map_id": 2, "events": [
                {"ts": 30.6, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 30},
                // An answer for another seq must not count for this open.
                {"ts": 30.61, "kind": "hud_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 31,
                 "shown": true, "output": "toast", "delivered": true}
            ]}),
        ];
        let mut content = String::new();
        for l in &lines {
            content.push_str(&serde_json::to_string(l).unwrap());
            content.push('\n');
        }
        std::fs::write(&mp, content).unwrap();

        let lat = load_live_mmap_latency(&mp);
        assert!(lat.available);
        assert_eq!(lat.opens.len(), 3);
        assert_eq!(lat.opens[0].in_game_feedback, Some(true));
        assert_eq!(lat.opens[0].in_game_skip_reason, None);
        assert_eq!(lat.opens[1].in_game_feedback, Some(false));
        assert_eq!(lat.opens[1].in_game_skip_reason.as_deref(), Some("map_tab_visible"));
        assert_eq!(lat.opens[2].in_game_feedback, Some(false));
        assert_eq!(lat.opens[2].in_game_skip_reason, None);
        assert!(lat.summary.in_game_audited);
        assert_eq!(lat.summary.in_game_ok, 1);
        assert_eq!(lat.summary.in_game_missing, 2);
        assert_eq!(format_in_game_col(&lat), "1/3 MISS");
    }

    #[test]
    fn live_mmap_latency_synthetic_seq_match_time_fallback_missing_feedback_and_unavailable() {
        let tmp = TempDir::new("gh_sim_mmap_lat");
        let mp = tmp.0.join("meta.jsonl");

        // 3 M-opens:
        // - Open #1 (ts=10.0..12.0): engine_synced at 10.25 (+0.25s), broadcast seq=10 at 10.30 (+0.30s),
        //   hud feedback matched by seq=10 at 10.90 (+0.90s), followed by minimap map_id=40.
        // - Open #2 (ts=30.0..32.0): engine_synced at 30.40 (+0.40s, MAP_M_Panning),
        //   broadcast seq=20 at 30.50 (+0.50s), feedback has seq=0 at 34.20 (+4.20s, matched via time fallback),
        //   followed by minimap map_id=34.
        // - Open #3 (ts=60.0..61.0): engine_synced at 60.20 (+0.20s), broadcast seq=30 at 60.25 (+0.25s),
        //   no matching feedback (only an unrelated status="marked" and another non-zero seq=99),
        //   followed by minimap map_id=2.
        let lines = vec![
            json!({"i": 1, "ts": 9.5, "mode": "minimap", "state": "TRACKING", "map_id": 2}),
            // Open #1
            json!({"i": 2, "ts": 10.0, "mode": "map_m", "state": "MAP_M_Settling"}),
            json!({"i": 3, "ts": 10.25, "mode": "map_m", "state": "MAP_M_SyncedIdle", "events": [
                {"ts": 10.30, "kind": "sync_broadcast", "seq": 10, "msg_type": "SYNC_POSITION_ONLY", "map_id": 40}
            ]}),
            json!({"i": 4, "ts": 12.0, "mode": "map_m", "state": "MAP_M_Closed", "events": [
                {"ts": 10.90, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 10}
            ]}),
            json!({"i": 5, "ts": 12.1, "mode": "minimap", "state": "RELOCALIZING", "map_id": null}),
            json!({"i": 6, "ts": 12.2, "mode": "minimap", "state": "TRACKING", "map_id": 40}),
            // Open #2
            json!({"i": 7, "ts": 30.0, "mode": "map_m", "state": "MAP_M_Settling"}),
            json!({"i": 8, "ts": 30.4, "mode": "map_m", "state": "MAP_M_Panning"}),
            json!({"i": 9, "ts": 32.0, "mode": "map_m", "state": "MAP_M_Closed", "events": [
                {"ts": 30.5, "kind": "sync_broadcast", "seq": 20, "msg_type": "SYNC_POSITION_ONLY", "map_id": 34}
            ]}),
            json!({"i": 10, "ts": 34.3, "mode": "minimap", "state": "TRACKING", "map_id": 34, "events": [
                {"ts": 34.2, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 0}
            ]}),
            // Open #3 (no matching feedback)
            json!({"i": 11, "ts": 60.0, "mode": "map_m", "state": "MAP_M_Settling"}),
            json!({"i": 12, "ts": 60.2, "mode": "map_m", "state": "MAP_M_SyncedIdle"}),
            json!({"i": 13, "ts": 61.0, "mode": "map_m", "state": "MAP_M_Closed", "events": [
                {"ts": 60.25, "kind": "sync_broadcast", "seq": 30, "msg_type": "SYNC_POSITION_ONLY", "map_id": 2}
            ]}),
            json!({"i": 14, "ts": 61.5, "mode": "minimap", "state": "TRACKING", "map_id": 2, "events": [
                {"ts": 61.1, "kind": "web_feedback", "event_type": "NAV_MARK", "status": "marked", "seq": 30},
                {"ts": 61.2, "kind": "web_feedback", "event_type": "SYNC_POSITION_ONLY", "status": "synced", "seq": 99}
            ]}),
        ];

        let mut content = String::new();
        for l in &lines {
            content.push_str(&serde_json::to_string(l).unwrap());
            content.push('\n');
        }
        std::fs::write(&mp, content).unwrap();

        let lat = load_live_mmap_latency(&mp);
        assert!(lat.available);
        assert_eq!(lat.opens.len(), 3);

        // Open #1 (seq match)
        assert_eq!(lat.opens[0].open_ts, 10.0);
        assert_eq!(lat.opens[0].map_id, Some(40));
        assert_eq!(lat.opens[0].engine_synced_s, Some(0.25));
        assert_eq!(lat.opens[0].broadcast_s, Some(0.3));
        assert_eq!(lat.opens[0].hud_s, Some(0.9));

        // Open #2 (time fallback with seq=0)
        assert_eq!(lat.opens[1].open_ts, 30.0);
        assert_eq!(lat.opens[1].map_id, Some(34));
        assert_eq!(lat.opens[1].engine_synced_s, Some(0.4));
        assert_eq!(lat.opens[1].broadcast_s, Some(0.5));
        assert_eq!(lat.opens[1].hud_s, Some(4.2));

        // Open #3 (no matching feedback)
        assert_eq!(lat.opens[2].open_ts, 60.0);
        assert_eq!(lat.opens[2].map_id, Some(2));
        assert_eq!(lat.opens[2].engine_synced_s, Some(0.2));
        assert_eq!(lat.opens[2].broadcast_s, Some(0.25));
        assert_eq!(lat.opens[2].hud_s, None);

        // Summary: count=3, hud_s=[0.9, 4.2] -> median=2.55, max=4.2, no_hud_count=1
        assert_eq!(lat.summary.count, 3);
        assert_eq!(lat.summary.median_hud_s, Some(2.55));
        assert_eq!(lat.summary.max_hud_s, Some(4.2));
        assert_eq!(lat.summary.no_hud_count, 1);

        // Recorded before `hud_feedback` existed: in-game output cannot be checked.
        assert!(!lat.summary.in_game_audited);
        assert!(lat.opens.iter().all(|o| o.in_game_feedback.is_none()));
        assert_eq!(format_in_game_col(&lat), "n/a");

        // Legacy meta with no `events` key reports unavailable
        let mp_legacy = tmp.0.join("meta_legacy.jsonl");
        std::fs::write(
            &mp_legacy,
            concat!(
                "{\"i\":1,\"ts\":1.0,\"mode\":\"minimap\",\"state\":\"TRACKING\",\"map_id\":2}\n",
                "{\"i\":2,\"ts\":2.0,\"mode\":\"map_m\",\"state\":\"MAP_M_SyncedIdle\"}\n"
            ),
        )
        .unwrap();
        let legacy = load_live_mmap_latency(&mp_legacy);
        assert!(!legacy.available);
        assert!(legacy.opens.is_empty());
        assert_eq!(legacy.summary.count, 0);
        assert_eq!(legacy.summary.median_hud_s, None);
        assert_eq!(legacy.summary.max_hud_s, None);
        assert_eq!(legacy.summary.no_hud_count, 0);
    }

    #[test]
    fn qa_prov_column_and_notes_cover_all_states_without_affecting_pass_fail() {
        use gh_core::provenance::{DatasetFingerprint, SessionProvenance};

        let mk_prov = |datasets_match: Option<bool>,
                       same_commit: Option<bool>,
                       engine_code_unchanged: Option<bool>| ReplayProvenance {
            recorded: Some(SessionProvenance {
                app_version: "5.7.0".to_owned(),
                git_commit: "1111111122222222333333334444444455555555".to_owned(),
                data_source: "installed".to_owned(),
                datasets: DatasetFingerprint::default(),
            }),
            replay_data_source: "data_dir".to_owned(),
            replay_datasets: DatasetFingerprint::default(),
            datasets_match,
            same_commit,
            engine_code_unchanged,
            current_commit: None,
        };

        // None / unrecorded -> n/a
        assert_eq!(format_prov_col(None), "n/a");
        assert_eq!(provenance_notes(None, "aaaa"), vec!["provenance unknown"]);
        let legacy_prov = ReplayProvenance {
            recorded: None,
            replay_data_source: "data_dir".to_owned(),
            replay_datasets: DatasetFingerprint::default(),
            datasets_match: None,
            same_commit: None,
            engine_code_unchanged: None,
            current_commit: None,
        };
        assert_eq!(format_prov_col(Some(&legacy_prov)), "n/a");
        assert_eq!(
            provenance_notes(Some(&legacy_prov), "aaaa"),
            vec!["provenance unknown"]
        );

        // Exact match -> ok, no notes
        let p_ok = mk_prov(Some(true), Some(true), Some(true));
        assert_eq!(format_prov_col(Some(&p_ok)), "ok");
        assert!(provenance_notes(Some(&p_ok), "1111111122222222333333334444444455555555").is_empty());

        // Data differs only -> data!
        let p_data = mk_prov(Some(false), Some(true), Some(true));
        assert_eq!(format_prov_col(Some(&p_data)), "data!");
        let n_data = provenance_notes(Some(&p_data), "1111111122222222333333334444444455555555");
        assert_eq!(n_data.len(), 1);
        assert!(n_data[0].contains("datasets differ"));

        // Commit differs only, engine unchanged -> code=
        let p_code_eq = mk_prov(Some(true), Some(false), Some(true));
        assert_eq!(format_prov_col(Some(&p_code_eq)), "code=");
        let n_code_eq = provenance_notes(Some(&p_code_eq), "abcdef1234567890");
        assert_eq!(n_code_eq.len(), 1);
        assert!(n_code_eq[0].contains("commit differs"));
        assert!(n_code_eq[0].contains("engine="));

        // Data differs + commit differs with engine unchanged -> data!code=
        let p_data_code_eq = mk_prov(Some(false), Some(false), Some(true));
        assert_eq!(format_prov_col(Some(&p_data_code_eq)), "data!code=");

        // Commit differs + engine changed -> code!
        let p_code = mk_prov(Some(true), Some(false), Some(false));
        assert_eq!(format_prov_col(Some(&p_code)), "code!");
        let n_code = provenance_notes(Some(&p_code), "abcdef1234567890");
        assert_eq!(n_code.len(), 1);
        assert!(n_code[0].contains("commit differs"));
        assert!(n_code[0].contains("11111111"));
        assert!(n_code[0].contains("abcdef12"));

        // Both differ -> data!code!
        let p_both = mk_prov(Some(false), Some(false), Some(false));
        assert_eq!(format_prov_col(Some(&p_both)), "data!code!");
        let n_both = provenance_notes(Some(&p_both), "abcdef1234567890");
        assert_eq!(n_both.len(), 2);
        assert!(n_both[0].contains("datasets differ"));
        assert!(n_both[1].contains("commit differs"));

        // Commit unknown -> code? / data!code?
        let p_code_unk = mk_prov(Some(true), None, None);
        assert_eq!(format_prov_col(Some(&p_code_unk)), "code?");
        assert_eq!(
            provenance_notes(Some(&p_code_unk), "unknown"),
            vec!["commit unknown"]
        );
        let p_data_code_unk = mk_prov(Some(false), None, None);
        assert_eq!(format_prov_col(Some(&p_data_code_unk)), "data!code?");

        // Informational-only end-to-end check: a blessed session with mismatched provenance STILL PASSES QA
        use gh_core::provenance::DATASET_FILENAMES;

        let tmp = TempDir::new("gh_sim_qa_prov_e2e");
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join(DATASET_FILENAMES[0]),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let sessions_dir = tmp.0.join("qa_sessions");
        let sdir = sessions_dir.join("sess_prov_diff");
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
        let mut bad_fp = DatasetFingerprint::default();
        bad_fp
            .0
            .insert(DATASET_FILENAMES[0].to_owned(), Some("f".repeat(64)));
        write_json_atomic(
            &sdir.join("info.json"),
            &json!({
                "codec": "png_frames",
                "frames": 1,
                "provenance": {
                    "app_version": "5.7.0",
                    "git_commit": "0".repeat(40),
                    "data_source": "installed",
                    "datasets": bad_fp
                }
            }),
        )
        .unwrap();

        let baseline_path = tmp.0.join("qa_baseline.json");
        let gt_file = tmp.0.join("verified_gt.json");
        let _ = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: true,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();

        let qa_rep = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(qa_rep.passed, "provenance mismatch must NEVER fail QA");
        let res = &qa_rep.sessions[0];
        assert_eq!(res.status, "ĐẠT");
        let prov_col = format_prov_col(res.provenance.as_ref());
        let (cur_commit, _) = crate::replay::resolve_current_git_commit_and_root(BUILD_GIT_COMMIT);
        if !cur_commit.eq_ignore_ascii_case("unknown") {
            assert_eq!(prov_col, "data!code!");
        } else {
            assert_eq!(prov_col, "data!code?");
        }
        assert!(res.notes.iter().any(|n| n.contains("datasets differ")));
        let row = format_qa_row(res);
        assert!(row.contains(&prov_col), "row missing Prov marker: {row}");
    }

    #[test]
    fn qa_unrec_column_formats_counts_and_na_without_affecting_pass_fail() {
        let hdr = format_qa_header();
        assert!(hdr.contains("Unrec"), "header missing Unrec: {hdr}");

        // 1. Legacy session without tick_seq -> "n/a"
        assert_eq!(format_unrec_col(None), "n/a");

        // 2. Session with minimap + minimap_fast + map_m + zero settling
        let mut by_mode = BTreeMap::new();
        by_mode.insert("minimap".to_owned(), 12);
        by_mode.insert("minimap_fast".to_owned(), 800);
        by_mode.insert("map_m".to_owned(), 3);
        by_mode.insert("map_m_paused".to_owned(), 0);
        by_mode.insert("map_m_settling".to_owned(), 0);
        by_mode.insert("idle_closed".to_owned(), 0);
        let fid = RecordingFidelityReport {
            frames_with_tick_seq: 400,
            first_tick_seq: Some(1),
            last_tick_seq: Some(1215),
            total_unrecorded_ticks: 815,
            unrecorded_by_mode: by_mode.clone(),
        };
        assert_eq!(format_unrec_col(Some(&fid)), "mm 812 map 3 set 0");

        // 3. Non-zero idle_closed appends `idl <n>`
        let mut by_mode_idl = by_mode;
        by_mode_idl.insert("idle_closed".to_owned(), 4);
        by_mode_idl.insert("map_m_settling".to_owned(), 1);
        let fid_idl = RecordingFidelityReport {
            frames_with_tick_seq: 400,
            first_tick_seq: Some(1),
            last_tick_seq: Some(1220),
            total_unrecorded_ticks: 820,
            unrecorded_by_mode: by_mode_idl,
        };
        assert_eq!(format_unrec_col(Some(&fid_idl)), "mm 812 map 3 set 1 idl 4");

        // 4. QA row formatting renders both `n/a` and formatted counts without affecting pass/fail
        let mk_row = |fidelity: Option<RecordingFidelityReport>| QaSessionResult {
            session_name: "sess_unrec_test".to_owned(),
            passed: true,
            status: "ĐẠT".to_owned(),
            reason: "ok".to_owned(),
            reasons: vec![],
            notes: vec![],
            frames: 400,
            codec: "libx264rgb".to_owned(),
            color_safe: true,
            wall_elapsed_s: 2.0,
            effective_fps: 200.0,
            hd_downloads: 0,
            map_m_synced_count: 2,
            baseline_map_m_synced_count: Some(2),
            mmap_view_frames: 10,
            mmap_view_acquired: 10,
            baseline_mmap_view_acquired: Some(10),
            mmap_view_drawer_open: 0,
            blessed_sha256_ok: true,
            vs_blessed: None,
            vs_meta_ref: MetaReferenceSummary::default(),
            heading_stability: SessionHeadingStability::default(),
            live_mmap_latency: LiveMMapLatency::default(),
            provenance: None,
            recording_fidelity: fidelity,
            input_replay: None,
            truth: None,
        };

        let row_legacy = format_qa_row(&mk_row(None));
        assert!(row_legacy.contains("n/a"), "legacy row missing n/a: {row_legacy}");

        let row_new = format_qa_row(&mk_row(Some(fid)));
        assert!(
            row_new.contains("mm 812 map 3 set 0"),
            "row missing formatted Unrec column: {row_new}"
        );

        let row_idl = format_qa_row(&mk_row(Some(fid_idl)));
        assert!(
            row_idl.contains("mm 812 map 3 set 1 idl 4"),
            "row missing idle Unrec column: {row_idl}"
        );
    }

    #[test]
    fn qa_nav_column_formats_hotkeys_web_nav_and_na_without_affecting_pass_fail() {
        let hdr = format_qa_header();
        assert!(hdr.contains("Nav"), "header missing Nav: {hdr}");

        // None or empty -> n/a
        assert_eq!(format_nav_col(None), "n/a");
        assert_eq!(format_nav_col(Some(&InputReplayReport::default())), "n/a");

        // Hotkeys only (e.g. 011306: 3/3 nav_guide; 011544: 0/1 debug)
        let hk_3_3 = InputReplayReport {
            hotkeys_recorded: 3,
            hotkeys_dispatched: 3,
            ..Default::default()
        };
        assert_eq!(format_nav_col(Some(&hk_3_3)), "hk 3/3");

        let hk_0_1 = InputReplayReport {
            hotkeys_recorded: 1,
            hotkeys_dispatched: 0,
            hotkeys_skipped: 1,
            ..Default::default()
        };
        assert_eq!(format_nav_col(Some(&hk_0_1)), "hk 0/1");

        // Hotkeys + web inputs + live nav comparison
        let full = InputReplayReport {
            hotkeys_recorded: 4,
            hotkeys_dispatched: 3,
            hotkeys_skipped: 1,
            web_inputs_delivered: 5,
            nav_frames_compared: 120,
            nav_mismatches: 2,
            ..Default::default()
        };
        assert_eq!(format_nav_col(Some(&full)), "hk 3/4 web 5 nav 2/120");

        // Web inputs only (no hotkeys), no live nav
        let web_only = InputReplayReport {
            web_inputs_delivered: 2,
            ..Default::default()
        };
        assert_eq!(format_nav_col(Some(&web_only)), "hk 0/0 web 2");

        // Legacy recording with carried-over pre-web nav mismatches surfaced separately
        let carried = InputReplayReport {
            web_inputs_delivered: 11,
            nav_frames_compared: 389,
            nav_mismatches: 16,
            nav_carried_over_mismatches: 72,
            ..Default::default()
        };
        assert_eq!(
            format_nav_col(Some(&carried)),
            "hk 0/0 web 11 nav 16/389 (+72pre)"
        );
    }

    #[test]
    fn qa_truth_column_formats_ok_suspect_and_na_without_affecting_pass_fail() {
        let hdr = format_qa_header();
        assert!(hdr.contains("Truth"), "header missing Truth: {hdr}");

        assert_eq!(format_truth_col(None), "n/a");
        assert_eq!(format_truth_col(Some(&TruthSummary::default())), "ok");
        let susp = TruthSummary {
            suspect_frames: 47,
            worst_frame: Some(233),
            worst_distance_w: Some(4664.6),
        };
        assert_eq!(format_truth_col(Some(&susp)), "susp 47f max 4665w@f233");

        let row = QaSessionResult {
            session_name: "sess_truth_test".to_owned(),
            passed: true,
            status: "ĐẠT".to_owned(),
            reason: "ok".to_owned(),
            reasons: vec![],
            notes: vec![],
            frames: 389,
            codec: "libx264rgb".to_owned(),
            color_safe: true,
            wall_elapsed_s: 3.0,
            effective_fps: 129.7,
            hd_downloads: 0,
            map_m_synced_count: 5,
            baseline_map_m_synced_count: Some(5),
            mmap_view_frames: 50,
            mmap_view_acquired: 50,
            baseline_mmap_view_acquired: Some(50),
            mmap_view_drawer_open: 0,
            blessed_sha256_ok: true,
            vs_blessed: None,
            vs_meta_ref: MetaReferenceSummary::default(),
            heading_stability: SessionHeadingStability::default(),
            live_mmap_latency: LiveMMapLatency::default(),
            provenance: None,
            recording_fidelity: None,
            input_replay: None,
            truth: Some(susp),
        };
        let rendered = format_qa_row(&row);
        assert!(
            rendered.contains("susp 47f max 4665w@f233"),
            "row missing Truth column: {rendered}"
        );
        assert!(row.passed, "Truth column must remain strictly informational");
    }

    fn setup_synthetic_qa_session(
        tmp: &TempDir,
        session_name: &str,
        num_frames: usize,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let sessions_dir = tmp.0.join("qa_sessions");
        let sdir = sessions_dir.join(session_name);
        std::fs::create_dir_all(sdir.join("frames")).unwrap();
        let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
        let mut meta = String::new();
        for i in 1..=num_frames {
            img.save(sdir.join("frames").join(format!("{i:06}.png")))
                .unwrap();
            let ts = 10.0 + (i - 1) as f64 * 0.05;
            meta.push_str(&format!(
                "{{\"i\":{i},\"ts\":{ts:.2},\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}}\n"
            ));
        }
        std::fs::write(sdir.join("meta.jsonl"), meta).unwrap();

        let baseline_path = tmp.0.join("qa_baseline.json");
        let gt_file = tmp.0.join("verified_gt.json");
        let bless = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: true,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(bless.passed);
        (sessions_dir, data_dir, baseline_path, gt_file)
    }

    #[test]
    fn qa_scenarios_pass_known_fail_and_only_filter() {
        use crate::scenario::ScenarioOutcome;

        let tmp = TempDir::new("gh_sim_qa_scenarios_ok");
        let (sessions_dir, data_dir, baseline_path, gt_file) =
            setup_synthetic_qa_session(&tmp, "sess_synth_sc", 5);
        let out_dir = tmp.0.join("out");
        let scenarios_dir = tmp.0.join("scenarios");
        std::fs::create_dir_all(&scenarios_dir).unwrap();

        let pins_input = json!({
            "frame": 1,
            "web_input": {
                "callback": "on_nearby_pins",
                "payload": {
                    "map_id": 2,
                    "pins": [
                        {"id": "101", "lat": 100.0, "lng": 200.0, "name": "Pin A"},
                        {"id": "102", "lat": 120.0, "lng": 220.0, "name": "Pin B"}
                    ]
                }
            }
        });

        // 1. Control scenario (pass) — stops at frame 2 out of 5!
        std::fs::write(
            scenarios_dir.join("01_pass.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "control_pass",
                "session": "sess_synth_sc",
                "description": "nav_next switches target from 101 to 102",
                "inputs": [
                    pins_input,
                    {"frame": 2, "hotkey": "nav_next"}
                ],
                "expect": [
                    {"frame": 1, "nav": {"target_id": null, "user_locked_id": null}},
                    {"frame": 2, "nav": {"target_id": "102", "user_locked_id": "102", "locked_target_id": null}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        // 2. Known-bug scenario (known-fail): expects a pin ID that is never delivered in this
        // synthetic session so known-fail reporting stays independent of real product bugs.
        std::fs::write(
            scenarios_dir.join("02_known_fail.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "synthetic_known_fail",
                "session": "sess_synth_sc",
                "description": "nav_unlock should clear user_locked_id from nav_next",
                "known_bug": "synthetic known-fail fixture (pin never delivered)",
                "inputs": [
                    pins_input,
                    {"frame": 2, "hotkey": "nav_next"}
                ],
                "expect": [
                    {"frame": 3, "within_frames": 1, "nav": {"target_id": "never_delivered_pin_999"}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        // 3. Scenario for another session — filtered out because only sess_synth_sc is selected
        std::fs::write(
            scenarios_dir.join("03_other_session.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "other_session_skip",
                "session": "sess_not_selected",
                "description": "ignored when session not in selection",
                "inputs": [],
                "expect": [
                    {"frame": 1, "nav": {"target_id": "nonexistent"}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        // Run QA with pass + known-fail -> MUST pass overall!
        let rep = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: Some(scenarios_dir),
            only: vec!["sess_synth_sc".to_owned()],
            out_dir: Some(out_dir.clone()),
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();

        assert!(rep.passed, "known-fail must NOT fail QA");
        assert_eq!(rep.scenarios.len(), 2);
        assert_eq!(rep.scenario_summary.pass, 1);
        assert_eq!(rep.scenario_summary.fail, 0);
        assert_eq!(rep.scenario_summary.known_fail, 1);
        assert_eq!(rep.scenario_summary.xpass, 0);
        assert_eq!(
            rep.scenario_summary.format_line(),
            "Scenarios: pass 1 | fail 0 | known-fail 1 | xpass 0"
        );
        assert_eq!(rep.scenarios[0].outcome, ScenarioOutcome::Pass);
        assert_eq!(rep.scenarios[0].frames_replayed, 2); // truncated to last needed frame (2 of 5)
        assert_eq!(rep.scenarios[1].outcome, ScenarioOutcome::KnownFail);
        assert_eq!(rep.scenarios[1].frames_replayed, 4); // frame 3 + within_frames 1 = 4 of 5
        assert_eq!(rep.scenarios[1].checks_passed, 0);
        assert_eq!(rep.scenarios[1].checks_total, 1);
        assert!(rep.scenarios[1].known_bug.is_some());

        // Verify scenario results were serialized into qa_report.json
        let saved_raw = std::fs::read_to_string(out_dir.join(QA_REPORT_FILENAME)).unwrap();
        let saved_val: Value = serde_json::from_str(&saved_raw).unwrap();
        assert_eq!(saved_val["scenario_summary"]["pass"], json!(1));
        assert_eq!(saved_val["scenario_summary"]["known_fail"], json!(1));
        assert_eq!(saved_val["scenarios"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn qa_scenarios_xpass_does_not_fail_qa() {
        use crate::scenario::ScenarioOutcome;

        let tmp = TempDir::new("gh_sim_qa_scenarios_xpass");
        let (sessions_dir, data_dir, baseline_path, gt_file) =
            setup_synthetic_qa_session(&tmp, "sess_synth_xpass", 4);
        let scenarios_dir = tmp.0.join("scenarios");
        std::fs::create_dir_all(&scenarios_dir).unwrap();

        // Xpass scenario: known_bug set, but expectations pass (explicit lock_target -> nav_unlock works)
        std::fs::write(
            scenarios_dir.join("01_xpass.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "unlock_after_lock_xpass",
                "session": "sess_synth_xpass",
                "description": "explicit lock_target cleared by nav_unlock",
                "known_bug": "stale known bug marker",
                "inputs": [
                    {
                        "frame": 1,
                        "web_input": {
                            "callback": "on_nearby_pins",
                            "payload": {
                                "map_id": 2,
                                "pins": [
                                    {"id": "101", "lat": 100.0, "lng": 200.0, "name": "Pin A"}
                                ]
                            }
                        }
                    },
                    {
                        "frame": 2,
                        "web_input": {
                            "callback": "on_nav_command",
                            "payload": {
                                "action": "lock_target",
                                "pin": {"id": "103", "lat": 50.0, "lng": 60.0, "map_id": 2, "name": "Pin C"}
                            }
                        }
                    },
                    {"frame": 3, "hotkey": "nav_unlock"}
                ],
                "expect": [
                    {"frame": 2, "nav": {"locked_target_id": "103"}},
                    {"frame": 3, "nav": {"locked_target_id": null}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let rep = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: Some(scenarios_dir),
            only: vec!["sess_synth_xpass".to_owned()],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();

        assert!(rep.passed, "xpass must NOT fail QA");
        assert_eq!(rep.scenario_summary.xpass, 1);
        assert_eq!(rep.scenarios[0].outcome, ScenarioOutcome::Xpass);
        assert_eq!(rep.scenarios[0].frames_replayed, 3);
    }

    #[test]
    fn qa_scenarios_seeded_fault_fails_qa_and_missing_dir_errors() {
        use crate::scenario::ScenarioOutcome;

        let tmp = TempDir::new("gh_sim_qa_scenarios_fail");
        let (sessions_dir, data_dir, baseline_path, gt_file) =
            setup_synthetic_qa_session(&tmp, "sess_synth_fail", 3);
        let scenarios_dir = tmp.0.join("scenarios");
        std::fs::create_dir_all(&scenarios_dir).unwrap();

        // Seeded fault: wrong expected target_id in a non-known-bug scenario MUST fail QA!
        std::fs::write(
            scenarios_dir.join("01_seeded_fail.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "seeded_wrong_target",
                "session": "sess_synth_fail",
                "description": "wrong expected target_id must fail qa",
                "inputs": [
                    {
                        "frame": 1,
                        "web_input": {
                            "callback": "on_nearby_pins",
                            "payload": {
                                "map_id": 2,
                                "pins": [
                                    {"id": "101", "lat": 100.0, "lng": 200.0, "name": "Pin A"},
                                    {"id": "102", "lat": 120.0, "lng": 220.0, "name": "Pin B"}
                                ]
                            }
                        }
                    },
                    {"frame": 2, "hotkey": "nav_next"}
                ],
                "expect": [
                    {"frame": 2, "nav": {"target_id": "wrong_pin_999"}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let fail_rep = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: Some(scenarios_dir),
            only: vec!["sess_synth_fail".to_owned()],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();

        assert!(!fail_rep.passed, "a non-known-bug scenario fail MUST fail QA");
        assert_eq!(fail_rep.failed_sessions, 0, "pose QA itself remains unaffected");
        assert_eq!(fail_rep.scenario_summary.fail, 1);
        assert_eq!(fail_rep.scenarios[0].outcome, ScenarioOutcome::Fail);
        let msg = fail_rep.scenarios[0].first_failure.as_deref().unwrap();
        assert!(msg.contains("wrong_pin_999"), "got: {msg}");
        assert!(msg.contains("102"), "got: {msg}");

        // Explicit non-existent --scenarios dir returns Err before replaying
        let err = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: Some(tmp.0.join("does_not_exist")),
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap_err();
        assert!(err.contains("scenarios directory does not exist"), "got: {err}");
    }

    #[test]
    fn compute_auto_jobs_respects_cpu_ram_and_work_caps() {
        assert_eq!(compute_auto_jobs(24, Some(48_000), Some(65_000), 28), 8);
        assert_eq!(compute_auto_jobs(16, Some(32_000), Some(32_000), 28), 5);
        assert_eq!(compute_auto_jobs(8, Some(16_000), Some(16_000), 28), 2);
        assert_eq!(compute_auto_jobs(4, Some(16_000), Some(16_000), 28), 1);
        assert_eq!(compute_auto_jobs(64, Some(128_000), Some(128_000), 28), 12);
        // Low RAM caps workers below cpu_bound
        assert_eq!(compute_auto_jobs(24, Some(4_096), Some(8_192), 28), 2);
        assert_eq!(compute_auto_jobs(24, Some(2_048), Some(4_096), 28), 1);
        // Few work items cap workers
        assert_eq!(compute_auto_jobs(24, Some(48_000), Some(65_000), 3), 3);
    }

    #[test]
    fn parallel_qa_matches_serial_report_order_and_poses() {
        let tmp = TempDir::new("gh_sim_qa_par_vs_ser");
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_json_atomic(
            &data_dir.join("multimap_v4.json"),
            &json!({"version": "4.0.0", "maps": {}}),
        )
        .unwrap();

        let sessions_dir = tmp.0.join("qa_sessions");
        let img = image::RgbaImage::from_pixel(434, 348, image::Rgba([20, 40, 60, 255]));
        for (name, frames) in [("sess_alpha", 3usize), ("sess_beta", 5usize), ("sess_gamma", 2usize)] {
            let sdir = sessions_dir.join(name);
            std::fs::create_dir_all(sdir.join("frames")).unwrap();
            let mut meta = String::new();
            for i in 1..=frames {
                img.save(sdir.join("frames").join(format!("{i:06}.png")))
                    .unwrap();
                let ts = 10.0 + (i - 1) as f64 * 0.05;
                meta.push_str(&format!(
                    "{{\"i\":{i},\"ts\":{ts:.2},\"frame_w\":2560,\"frame_h\":1440,\"state\":\"UNLOCALIZED\"}}\n"
                ));
            }
            std::fs::write(sdir.join("meta.jsonl"), meta).unwrap();
        }

        let scenarios_dir = tmp.0.join("scenarios");
        std::fs::create_dir_all(&scenarios_dir).unwrap();
        std::fs::write(
            scenarios_dir.join("01_beta.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "name": "beta_hud_toggle",
                "session": "sess_beta",
                "description": "hud toggle in parallel qa",
                "inputs": [{"frame": 2, "hotkey": "nav_toggle"}],
                "expect": [{"frame": 2, "ui": {"nav_hud_enabled": false}}]
            }))
            .unwrap(),
        )
        .unwrap();

        let baseline_path = tmp.0.join("qa_baseline.json");
        let gt_file = tmp.0.join("verified_gt.json");
        let bless = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: true,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(2),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap();
        assert!(bless.passed);

        let dump_ser = tmp.0.join("poses_ser");
        let dump_par = tmp.0.join("poses_par");
        let rep_ser = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: Some(scenarios_dir.clone()),
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(1),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: Some(dump_ser.clone()),
        })
        .unwrap();

        let rep_par = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: Some(scenarios_dir.clone()),
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(3),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: Some(dump_par.clone()),
        })
        .unwrap();

        assert!(rep_ser.passed && rep_par.passed);
        assert_eq!(rep_ser.sessions.len(), 3);
        assert_eq!(rep_par.sessions.len(), 3);
        assert_eq!(rep_ser.scenario_summary, rep_par.scenario_summary);
        for (a, b) in rep_ser.sessions.iter().zip(rep_par.sessions.iter()) {
            assert_eq!(a.session_name, b.session_name);
            assert_eq!(a.passed, b.passed);
            assert_eq!(a.status, b.status);
            assert_eq!(a.reason, b.reason);
            assert_eq!(a.vs_blessed, b.vs_blessed);
            assert_eq!(a.vs_meta_ref, b.vs_meta_ref);
            assert_eq!(a.heading_stability, b.heading_stability);
            assert_eq!(a.truth, b.truth);
            let p_ser = std::fs::read(dump_ser.join(format!("{}.jsonl", a.session_name))).unwrap();
            let p_par = std::fs::read(dump_par.join(format!("{}.jsonl", a.session_name))).unwrap();
            assert_eq!(p_ser, p_par, "pose diff on {}", a.session_name);
        }
        assert_eq!(rep_ser.scenarios[0].outcome, rep_par.scenarios[0].outcome);
        assert_eq!(rep_ser.scenarios[0].checks, rep_par.scenarios[0].checks);

        // Cache hit on subsequent run
        let cache_dir = tmp.0.join("cache");
        let rep_warm1 = run_qa(&QaOptions {
            sessions_dir: sessions_dir.clone(),
            data_dir: data_dir.clone(),
            ffmpeg_path: None,
            baseline_path: baseline_path.clone(),
            gt_file: gt_file.clone(),
            scenarios_dir: Some(scenarios_dir.clone()),
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(2),
            no_cache: false,
            cache_dir: Some(cache_dir.clone()),
            dump_poses_dir: None,
        })
        .unwrap();
        let rep_warm2 = run_qa(&QaOptions {
            sessions_dir,
            data_dir,
            ffmpeg_path: None,
            baseline_path,
            gt_file,
            scenarios_dir: Some(scenarios_dir),
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(2),
            no_cache: false,
            cache_dir: Some(cache_dir),
            dump_poses_dir: None,
        })
        .unwrap();
        assert_eq!(rep_warm1.sessions[0].reason, rep_warm2.sessions[0].reason);
        assert_eq!(rep_warm1.scenarios[0], rep_warm2.scenarios[0]);

        // --jobs 0 must error
        let err = run_qa(&QaOptions {
            sessions_dir: tmp.0.join("qa_sessions"),
            data_dir: tmp.0.join("data"),
            ffmpeg_path: None,
            baseline_path: tmp.0.join("qa_baseline.json"),
            gt_file: tmp.0.join("verified_gt.json"),
            scenarios_dir: None,
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
            jobs: Some(0),
            no_cache: true,
            cache_dir: None,
            dump_poses_dir: None,
        })
        .unwrap_err();
        assert!(err.contains("--jobs must be >= 1"), "got: {err}");
    }
}
