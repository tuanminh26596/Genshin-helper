//! Batch regression QA runner (`gh-sim qa`) against blessed frame-by-frame poses.
//!
//! Compares fresh synchronous replays of recorded sessions against human-approved
//! `<session>/blessed_poses.jsonl` files (verified by SHA-256 recorded in git-tracked
//! `tests/qa_baseline.json`). Metrics against legacy `meta.jsonl` are printed and
//! recorded for reference only — never used for pass/fail grading.

use crate::replay::{
    DEFAULT_DRIFT_THRESHOLD_W, ObservedFrame, ReplayOptions, ReplayReport, angle_diff_deg,
    percentile, round3, run_replay_with_poses,
};
use gh_core::atomic_io::{write_bytes_atomic, write_json_atomic};
use gh_net::update_check::sha256_file;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const QA_BASELINE_SCHEMA_VERSION: u32 = 1;
pub const QA_REPORT_SCHEMA_VERSION: u32 = 1;
pub const BLESSED_POSES_FILENAME: &str = "blessed_poses.jsonl";
pub const QA_REPORT_FILENAME: &str = "qa_report.json";
pub const HEADING_FLIP_THRESHOLD_DEG: f64 = 120.0;
pub const HEADING_SPIKE_STEP_THRESHOLD_DEG: f64 = 30.0;
pub const HEADING_SPIKE_RETURN_THRESHOLD_DEG: f64 = 15.0;
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
    pub only: Vec<String>,
    pub out_dir: Option<PathBuf>,
    pub bless: bool,
    pub overrides: QaThresholdOverrides,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHeadingStability {
    pub replay: HeadingStabilityPair,
    pub blessed: Option<HeadingStabilityPair>,
    pub live_meta: Option<HeadingStabilityPair>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadingStabilityGateOutcome {
    pub fail_reasons: Vec<String>,
    pub notes: Vec<String>,
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

/// Computes paired body & camera heading stability over observed replay/blessed frames.
pub fn compute_observed_heading_stability(poses: &[ObservedFrame]) -> HeadingStabilityPair {
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
    HeadingStabilityPair {
        body: compute_heading_stability(&body_samples),
        camera: compute_heading_stability(&camera_samples),
    }
}

/// Loads live `meta.jsonl` and computes body/camera heading stability using only records with
/// `state == "TRACKING"` and `mode == "minimap"`. Returns `None` if unavailable or missing fields.
pub fn load_live_meta_heading_stability(meta_path: &Path) -> Option<HeadingStabilityPair> {
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

    Some(HeadingStabilityPair {
        body: compute_heading_stability(&body_samples),
        camera: compute_heading_stability(&camera_samples),
    })
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
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:<20} {:>6}  {}",
        "Session",
        "Status",
        "Frames",
        "Codec(CS)",
        "vs Blessed (map/ov/p95/max)",
        "MMap(sync/acq)",
        "vs Meta[ref]",
        "Arrow (b/c flips)",
        "Time",
        "Reason"
    )
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
    format!(
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:<20} {:>5.1}s  {}",
        r.session_name,
        r.status,
        r.frames,
        codec_col,
        blessed_col,
        mmap_col,
        meta_col,
        arrow_col,
        r.wall_elapsed_s,
        r.reason
    )
}

/// Runs `gh-sim qa` (verification mode or `--bless` mode) across discovered sessions.
pub fn run_qa(opts: &QaOptions) -> Result<QaReport, String> {
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
    let total_t0 = Instant::now();

    let header = format_qa_header();
    let sep = "-".repeat(header.len().min(172));
    println!("{header}");
    println!("{sep}");
    let _ = std::io::stdout().flush();

    let mut results = Vec::with_capacity(session_dirs.len());

    for sdir in &session_dirs {
        let session_name = sdir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session")
            .to_owned();
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

        let (report, poses) = run_replay_with_poses(&replay_opts)?;
        let meta_ref = meta_ref_from_report(&report);
        let replay_heading_stability = compute_observed_heading_stability(&poses);
        let live_meta_heading_stability =
            load_live_meta_heading_stability(&sdir.join("meta.jsonl"));
        let blessed_path = sdir.join(BLESSED_POSES_FILENAME);

        let result = if opts.bless {
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
            baseline.sessions.insert(session_name.clone(), entry);

            let self_cmp = compare_poses_against_blessed(&poses, &poses, eff_thresholds.pos_threshold_w);
            let mut reasons = Vec::new();
            if report.offline_assets.hd_downloads > 0 {
                reasons.push(format!(
                    "offline leak: hd_downloads={}",
                    report.offline_assets.hd_downloads
                ));
            }
            let passed = reasons.is_empty();
            QaSessionResult {
                session_name,
                passed,
                status: if passed {
                    "BLESSED".to_owned()
                } else {
                    "KHÔNG ĐẠT".to_owned()
                },
                reason: if passed {
                    "blessed ok".to_owned()
                } else {
                    reasons.join("; ")
                },
                reasons,
                notes: Vec::new(),
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
                },
            }
        } else {
            match baseline.sessions.get(&session_name) {
                None => QaSessionResult {
                    session_name,
                    passed: false,
                    status: "KHÔNG ĐẠT".to_owned(),
                    reason: "thiếu phiên trong tests/qa_baseline.json".to_owned(),
                    reasons: vec!["thiếu phiên trong tests/qa_baseline.json".to_owned()],
                    notes: Vec::new(),
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
                    },
                },
                Some(base_sess) => {
                    let mut reasons = Vec::new();
                    let mut notes = Vec::new();
                    let mut blessed_sha256_ok = false;
                    let mut vs_blessed = None;
                    let mut blessed_heading_stability = None;

                    if !blessed_path.is_file() {
                        reasons.push(format!("thiếu file {BLESSED_POSES_FILENAME}"));
                    } else {
                        match sha256_file(&blessed_path) {
                            Err(e) => reasons.push(format!("lỗi đọc sha256 blessed: {e}")),
                            Ok(actual_sha) => {
                                if !actual_sha.eq_ignore_ascii_case(&base_sess.blessed_poses_sha256)
                                {
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
                                    &eff_thresholds,
                                    &opts.overrides,
                                );
                                reasons.extend(eval_reasons);
                                if has_heading_fields {
                                    blessed_heading_stability =
                                        Some(compute_observed_heading_stability(&blessed_poses));
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
                        },
                    }
                }
            }
        };

        println!("{}", format_qa_row(&result));
        let _ = std::io::stdout().flush();
        results.push(result);
    }

    if opts.bless {
        baseline.save_atomic(&opts.baseline_path)?;
    }

    let total_wall_elapsed_s = round3(total_t0.elapsed().as_secs_f64());
    let passed_sessions = results.iter().filter(|r| r.passed).count();
    let total_sessions = results.len();
    let failed_sessions = total_sessions.saturating_sub(passed_sessions);
    let all_passed = failed_sessions == 0;

    println!("{sep}");
    println!(
        "Tổng: {passed_sessions}/{total_sessions} {} | thời gian: {total_wall_elapsed_s:.1}s | baseline: {}",
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
            only: vec![],
            out_dir: Some(out_dir.clone()),
            bless: true,
            overrides: QaThresholdOverrides::default(),
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
            only: vec![],
            out_dir: Some(out_dir.clone()),
            bless: false,
            overrides: QaThresholdOverrides::default(),
        })
        .unwrap();
        assert!(qa_rep.passed);
        assert_eq!(qa_rep.sessions[0].status, "ĐẠT");
        assert!(qa_rep.sessions[0].blessed_sha256_ok);

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
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides::default(),
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
            only: vec![],
            out_dir: None,
            bless: false,
            overrides: QaThresholdOverrides {
                max_map_mismatches: Some(999),
                ..Default::default()
            },
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
            },
        };
        let formatted = format_qa_row(&row_with_live);
        assert!(
            formatted.contains("b8/c0 live b8/c0"),
            "unexpected row: {formatted}"
        );
    }
}
