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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QaSessionResult {
    pub session_name: String,
    pub passed: bool,
    pub status: String,
    pub reason: String,
    pub reasons: Vec<String>,
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

/// Loads frame-by-frame poses from `<session>/blessed_poses.jsonl`.
pub fn load_blessed_poses(path: &Path) -> Result<Vec<ObservedFrame>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (line_no, line_res) in reader.lines().enumerate() {
        let line = line_res
            .map_err(|e| format!("read error {}:{}: {e}", path.display(), line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let pose: ObservedFrame = serde_json::from_str(trimmed)
            .map_err(|e| format!("parse error {}:{}: {e}", path.display(), line_no + 1))?;
        out.push(pose);
    }
    Ok(out)
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
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:>6}  {}",
        "Session",
        "Status",
        "Frames",
        "Codec(CS)",
        "vs Blessed (map/ov/p95/max)",
        "MMap(sync/acq)",
        "vs Meta[ref]",
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
    format!(
        "{:<22} {:<10} {:>6} {:<13} {:<25} {:<14} {:<18} {:>5.1}s  {}",
        r.session_name,
        r.status,
        r.frames,
        codec_col,
        blessed_col,
        mmap_col,
        meta_col,
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
    let sep = "-".repeat(header.len().min(148));
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
            }
        } else {
            match baseline.sessions.get(&session_name) {
                None => QaSessionResult {
                    session_name,
                    passed: false,
                    status: "KHÔNG ĐẠT".to_owned(),
                    reason: "thiếu phiên trong tests/qa_baseline.json".to_owned(),
                    reasons: vec!["thiếu phiên trong tests/qa_baseline.json".to_owned()],
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
                },
                Some(base_sess) => {
                    let mut reasons = Vec::new();
                    let mut blessed_sha256_ok = false;
                    let mut vs_blessed = None;

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
                        match load_blessed_poses(&blessed_path) {
                            Err(e) => reasons.push(format!("lỗi đọc {BLESSED_POSES_FILENAME}: {e}")),
                            Ok(blessed_poses) => {
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
                            "ok".to_owned()
                        } else {
                            reasons.join("; ")
                        },
                        reasons,
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
}
