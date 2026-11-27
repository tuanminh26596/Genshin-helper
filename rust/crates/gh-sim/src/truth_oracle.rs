//! Informational replay accuracy oracle (`Truth` column in `gh-sim qa`).
//!
//! Evaluates REPLAY poses (`&[ObservedFrame]`) strictly against non-`meta.jsonl` references:
//! 1. Human-verified `verified_gt.json` checkpoints (`AccuracyReport::details`).
//! 2. High-confidence replay anchors (`MAP_M_SYNCED` with `inliers_count >= MIN_ANCHOR_INLIERS`
//!    or `TRACKING` frames with `match_method == "layer_switch_sift"`), propagated forward and
//!    backward within continuous same-map segments (severed by teleport/pause boundaries) plus
//!    low-inlier `MAP_M_SYNCED` contradiction checks.
//! 3. Physical plausibility between consecutive active `TRACKING` frames on the same `map_id`.
//!
//! Strictly informational: NEVER alters QA `passed`, `status`, thresholds, or exit codes, and
//! keeps `ObservedFrame` / `blessed_poses.jsonl` serialization completely untouched.

use crate::replay::{AccuracyReport, ObservedFrame, round3};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Minimum geometric landmark inlier count required for a `MAP_M_SYNCED` frame to qualify as a
/// high-confidence truth anchor.
///
/// `apply_tick_output` schedules `PendingVerify` whenever `inliers_count <= 3` because 2–3
/// landmark matches can false-lock onto ambiguous waypoint clusters (e.g. `sess_20261127_210311`
/// f233 with 2 inliers and f237 with 3 inliers on Map 40). Requiring at least 5 inliers restricts
/// M-Map anchors to well-constrained multi-landmark solves.
pub const MIN_ANCHOR_INLIERS: usize = 5;

/// Upper bound on sustained player traversal speed in HoYoLAB world units per second when
/// propagating a high-confidence anchor forward or backward across `|dt|` seconds.
///
/// Empirically measured across 12 clean Genshin QA sessions (~13,300 frames): sustained sprint /
/// glide / traversal speed peaks at 17.5 w/s over >= 3 s and 13.1 w/s over >= 5 s. Using 25.0 w/s
/// comfortably covers fast movement skills while keeping multi-second propagation tight enough
/// to expose 200+ w false locks.
pub const MAX_PLAYER_SPEED_W_PER_S: f64 = 25.0;

/// Additive world-unit margin added to `MAX_PLAYER_SPEED_W_PER_S * |dt|` for anchor consistency
/// and intra-M-Map non-panning sync agreement.
///
/// Matches `SessionCheckpointGt`'s default position tolerance (60.0 w) and `apply_tick_output`'s
/// `PendingVerify` correction threshold (60.0 w), absorbing normal M-Map/minimap registration
/// jitter (~5–20 w) and brief 1 s dash bursts (~30 w) without false positives.
pub const ANCHOR_MARGIN_W: f64 = 60.0;

/// Maximum single-step displacement (world units) allowed between two consecutive `TRACKING`
/// frames on the same `map_id` with no pause/teleport boundary between them.
///
/// Across 12 clean QA sessions, the largest legitimate active-tracking snap (re-acquiring after
/// `stationary_skip` via `annulus_template`) is 18.1 w. An 80.0 w ceiling gives > 4x headroom
/// over normal correction snaps while catching abrupt false-lock jumps.
pub const MAX_CONSECUTIVE_STEP_W: f64 = 80.0;

/// Maximum instantaneous speed (world units per second) allowed between two consecutive `TRACKING`
/// frames on the same `map_id` with no pause/teleport boundary between them.
///
/// Across 12 clean QA sessions, peak legitimate active-tracking snap speed is 201.5 w/s
/// (18.1 w over 90 ms) and long-hold recovery snap is 219.1 w/s (17.3 w over 79 ms). Setting
/// 500.0 w/s gives > 2x headroom (>= 25 w at the 50 ms dt floor) while flagging teleport-scale
/// unphysical jumps without a boundary.
pub const MAX_PLAUSIBLE_SPEED_W_PER_S: f64 = 500.0;

/// Minimum timestamp delta floor (seconds) used when computing consecutive-frame speed.
///
/// Matches the 20 Hz full minimap tracking tick period (`MINIMAP_FULL_STEP_S = 0.05 s`) so
/// duplicate or sub-50 ms frame timestamps cannot blow up `dist_w / dt_s`.
pub const STEP_DT_FLOOR_S: f64 = 0.05;

/// Maximum contiguous `gate_hold` / `relocating_hold` length (frames) directly after a pause or
/// M-Map boundary that is treated as a normal post-teleport relocation settling tail.
///
/// After a legitimate loading-screen teleport (`paimon_pause` / `MAP_M_Closed`), `minimap_tracker`
/// holds the pre-teleport coordinate for 1–5 frames (<= 0.43 s across all 12 clean QA sessions)
/// before `global_teyvat_relocate` / `layer_switch_sift` locks onto the destination. A ceiling of
/// 8 frames avoids false positives on normal post-teleport settling, whereas a prolonged hold
/// stuck on a bad anchor (e.g. 45 frames / 5.0 s in `sess_20261127_210311`) is checked against the
/// recovering high-confidence anchor.
pub const MAX_POST_BOUNDARY_RELOCATE_HOLD_FRAMES: usize = 8;

/// Side-channel per-frame locate metadata collected during replay.
///
/// Kept outside [`ObservedFrame`] so `blessed_poses.jsonl` schema and SHA-256 hashes stay untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayTruthMeta {
    /// M-Map `locate_player_frame` geometric inlier counts keyed by 1-based `ObservedFrame::idx`.
    #[serde(default)]
    pub map_m_inliers: BTreeMap<usize, usize>,
}

/// Informational accuracy oracle summary for a single replayed session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TruthSummary {
    /// Count of unique frame indices flagged by any oracle check.
    pub suspect_frames: usize,
    /// 1-based frame index with the largest recorded discrepancy (tie-broken by earliest frame).
    pub worst_frame: Option<usize>,
    /// Largest recorded discrepancy in world units (`0.0` when flagged without a distance metric).
    pub worst_distance_w: Option<f64>,
}

/// Detailed oracle output including the per-frame discrepancy map (`frame_idx -> max_error_w`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TruthOracleDetail {
    pub summary: TruthSummary,
    pub suspects: BTreeMap<usize, f64>,
}

#[inline]
fn valid_pos(f: &ObservedFrame) -> Option<(i32, f64, f64)> {
    let map_id = f.map_id?;
    let x = f.world_x.filter(|v| v.is_finite())?;
    let y = f.world_y.filter(|v| v.is_finite())?;
    Some((map_id, x, y))
}

#[inline]
fn is_pause_method(m: &str) -> bool {
    matches!(m, "paimon_pause" | "loading_pause" | "paused")
}

#[inline]
fn is_hold_method(m: &str) -> bool {
    matches!(
        m,
        "gate_hold" | "relocating_hold" | "gate_rejected" | "anchor_hold"
    )
}

#[inline]
fn is_hard_boundary_state(s: &str) -> bool {
    matches!(
        s,
        "IDLE" | "IDLE_CLOSED" | "UNLOCALIZED" | "LOST" | "RELOCATING"
    ) || s.starts_with("MAP_M_")
}

#[inline]
fn is_high_confidence_anchor(f: &ObservedFrame, meta: &ReplayTruthMeta) -> bool {
    if valid_pos(f).is_none() {
        return false;
    }
    if f.state == "MAP_M_SYNCED" {
        return meta.map_m_inliers.get(&f.idx).copied().unwrap_or(0) >= MIN_ANCHOR_INLIERS;
    }
    f.state == "TRACKING" && f.match_method == "layer_switch_sift"
}

fn record_suspect(suspects: &mut BTreeMap<usize, f64>, frame_idx: usize, err_w: f64) {
    let clean = if err_w.is_finite() && err_w >= 0.0 {
        err_w
    } else {
        0.0
    };
    suspects
        .entry(frame_idx)
        .and_modify(|prev| {
            if clean > *prev {
                *prev = clean;
            }
        })
        .or_insert(clean);
}

/// Returns `true` when `idx` belongs to a short (`<= MAX_POST_BOUNDARY_RELOCATE_HOLD_FRAMES`)
/// contiguous run of hold frames (`gate_hold` / `relocating_hold` / `gate_rejected` /
/// `anchor_hold`) immediately preceded by a session start, hard boundary, or pause boundary.
fn is_in_short_post_boundary_hold(observed: &[ObservedFrame], idx: usize) -> bool {
    let f = &observed[idx];
    if f.state != "TRACKING" || !is_hold_method(&f.match_method) {
        return false;
    }
    let mut start = idx;
    while start > 0 {
        let prev = &observed[start - 1];
        if prev.state == "TRACKING" && is_hold_method(&prev.match_method) {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = idx;
    while end + 1 < observed.len() {
        let next = &observed[end + 1];
        if next.state == "TRACKING" && is_hold_method(&next.match_method) {
            end += 1;
        } else {
            break;
        }
    }
    let hold_len = end - start + 1;
    if hold_len > MAX_POST_BOUNDARY_RELOCATE_HOLD_FRAMES {
        return false;
    }
    if start == 0 {
        return true;
    }
    let before = &observed[start - 1];
    is_hard_boundary_state(&before.state)
        || before.state != "TRACKING"
        || is_pause_method(&before.match_method)
}

/// Evaluates all three informational truth-oracle checks and returns both summary and per-frame
/// discrepancy details.
pub fn evaluate_truth_oracle_detailed(
    observed: &[ObservedFrame],
    accuracy: &AccuracyReport,
    meta: &ReplayTruthMeta,
) -> TruthOracleDetail {
    let mut suspects: BTreeMap<usize, f64> = BTreeMap::new();

    // -------------------------------------------------------------------------
    // Check (1): verified_gt.json checkpoints (when present in accuracy.details)
    // -------------------------------------------------------------------------
    for cp in &accuracy.details {
        if !cp.passed || cp.false_lock {
            let err = cp.pos_err_w.filter(|v| v.is_finite()).unwrap_or(0.0);
            record_suspect(&mut suspects, cp.frame_idx, err);
        }
    }

    // -------------------------------------------------------------------------
    // Check (2a): High-confidence anchor propagation (forward & backward)
    // -------------------------------------------------------------------------
    // Track which slice indices were flagged directly by a specific anchor so Check (2b) can tell
    // whether an M-Map open handed a false lock directly into the post-M-Map tracking segment.
    let mut flagged_by_anchor: BTreeMap<(usize, usize), f64> = BTreeMap::new();

    for (a_i, anchor) in observed.iter().enumerate() {
        if !is_high_confidence_anchor(anchor, meta) {
            continue;
        }
        let Some((a_map, ax, ay)) = valid_pos(anchor) else {
            continue;
        };
        let a_ts = if anchor.ts.is_finite() {
            anchor.ts
        } else {
            0.0
        };

        // Forward propagation from anchor
        for f in &observed[a_i + 1..] {
            if f.state != "TRACKING"
                || is_hard_boundary_state(&f.state)
                || is_pause_method(&f.match_method)
            {
                break;
            }
            let Some((f_map, fx, fy)) = valid_pos(f) else {
                continue;
            };
            if f_map != a_map {
                break;
            }
            let dt = if f.ts.is_finite() {
                (f.ts - a_ts).abs()
            } else {
                0.0
            };
            let allowed = MAX_PLAYER_SPEED_W_PER_S * dt + ANCHOR_MARGIN_W;
            let dist = (fx - ax).hypot(fy - ay);
            if dist > allowed {
                record_suspect(&mut suspects, f.idx, dist);
            }
        }

        // Backward propagation from anchor
        for j in (0..a_i).rev() {
            let f = &observed[j];
            if f.state != "TRACKING"
                || is_hard_boundary_state(&f.state)
                || is_pause_method(&f.match_method)
            {
                break;
            }
            // Do not cross a short post-boundary teleport relocation settling tail.
            if is_in_short_post_boundary_hold(observed, j) {
                break;
            }
            let Some((f_map, fx, fy)) = valid_pos(f) else {
                continue;
            };
            if f_map != a_map {
                break;
            }
            let dt = if f.ts.is_finite() {
                (a_ts - f.ts).abs()
            } else {
                0.0
            };
            let allowed = MAX_PLAYER_SPEED_W_PER_S * dt + ANCHOR_MARGIN_W;
            let dist = (fx - ax).hypot(fy - ay);
            if dist > allowed {
                record_suspect(&mut suspects, f.idx, dist);
                flagged_by_anchor.insert((j, a_i), dist);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Check (2b): Low-inlier MAP_M_SYNCED contradiction checks within/after an M-Map open
    // -------------------------------------------------------------------------
    let mut i = 0usize;
    while i < observed.len() {
        if !observed[i].state.starts_with("MAP_M_") {
            i += 1;
            continue;
        }
        let m_start = i;
        while i < observed.len() && observed[i].state.starts_with("MAP_M_") {
            i += 1;
        }
        let m_end = i - 1;

        let sync_indices: Vec<usize> = (m_start..=m_end)
            .filter(|&k| observed[k].state == "MAP_M_SYNCED" && valid_pos(&observed[k]).is_some())
            .collect();

        // (2b.1) Intra-M-Map non-panning contradiction between consecutive MAP_M_SYNCED frames.
        for pair in sync_indices.windows(2) {
            let (s1_i, s2_i) = (pair[0], pair[1]);
            let had_pan_between = (s1_i + 1..s2_i).any(|k| observed[k].state == "MAP_M_Panning");
            if had_pan_between {
                continue;
            }
            let (Some((m1, x1, y1)), Some((m2, x2, y2))) =
                (valid_pos(&observed[s1_i]), valid_pos(&observed[s2_i]))
            else {
                continue;
            };
            if m1 != m2 {
                continue;
            }
            let dist = (x2 - x1).hypot(y2 - y1);
            if dist <= ANCHOR_MARGIN_W {
                continue;
            }
            let inl1 = meta
                .map_m_inliers
                .get(&observed[s1_i].idx)
                .copied()
                .unwrap_or(0);
            let inl2 = meta
                .map_m_inliers
                .get(&observed[s2_i].idx)
                .copied()
                .unwrap_or(0);
            if inl1 < MIN_ANCHOR_INLIERS && inl2 >= MIN_ANCHOR_INLIERS {
                record_suspect(&mut suspects, observed[s1_i].idx, dist);
            } else if inl2 < MIN_ANCHOR_INLIERS && inl1 >= MIN_ANCHOR_INLIERS {
                record_suspect(&mut suspects, observed[s2_i].idx, dist);
            } else if inl1 < MIN_ANCHOR_INLIERS && inl2 < MIN_ANCHOR_INLIERS {
                // Both are low-inlier and disagree without panning: flag the weaker one
                // (or the earlier one on tie, matching PendingVerify replacement semantics).
                if inl2 < inl1 {
                    record_suspect(&mut suspects, observed[s2_i].idx, dist);
                } else {
                    record_suspect(&mut suspects, observed[s1_i].idx, dist);
                }
            }
        }

        // (2b.2) Low-inlier MAP_M_SYNCED contradicted by the first high-confidence tracking anchor
        // immediately after M-Map closes (with no panning after the sync, no pause/loading
        // boundary after M-Map close, and the very first post-M-Map frame already contradicted
        // by that anchor).
        let after_idx = m_end + 1;
        if after_idx < observed.len() && !sync_indices.is_empty() {
            let mut next_anchor: Option<usize> = None;
            for (offset, f) in observed[after_idx..].iter().enumerate() {
                let k = after_idx + offset;
                if f.state != "TRACKING"
                    || is_hard_boundary_state(&f.state)
                    || is_pause_method(&f.match_method)
                {
                    break;
                }
                if is_high_confidence_anchor(f, meta) {
                    next_anchor = Some(k);
                    break;
                }
            }
            if let Some(a_i) = next_anchor
                && flagged_by_anchor.contains_key(&(after_idx, a_i))
                && let Some((a_map, ax, ay)) = valid_pos(&observed[a_i])
            {
                let close_ts = if observed[m_end].ts.is_finite() {
                    observed[m_end].ts
                } else {
                    0.0
                };
                let a_ts = if observed[a_i].ts.is_finite() {
                    observed[a_i].ts
                } else {
                    close_ts
                };
                // Player cannot walk while fullscreen M-Map is open; walkable dt starts at close.
                let walk_dt = (a_ts - close_ts).max(0.0);
                let allowed = MAX_PLAYER_SPEED_W_PER_S * walk_dt + ANCHOR_MARGIN_W;

                for &s_i in &sync_indices {
                    let s_inl = meta
                        .map_m_inliers
                        .get(&observed[s_i].idx)
                        .copied()
                        .unwrap_or(0);
                    if s_inl >= MIN_ANCHOR_INLIERS {
                        continue;
                    }
                    let had_pan_after =
                        (s_i + 1..=m_end).any(|k| observed[k].state == "MAP_M_Panning");
                    if had_pan_after {
                        continue;
                    }
                    let Some((s_map, sx, sy)) = valid_pos(&observed[s_i]) else {
                        continue;
                    };
                    if s_map != a_map {
                        continue;
                    }
                    let dist = (sx - ax).hypot(sy - ay);
                    if dist > allowed {
                        record_suspect(&mut suspects, observed[s_i].idx, dist);
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Check (3): Physical plausibility between consecutive TRACKING frames
    // -------------------------------------------------------------------------
    for idx in 1..observed.len() {
        let prev = &observed[idx - 1];
        let curr = &observed[idx];
        if prev.state != "TRACKING" || curr.state != "TRACKING" {
            continue;
        }
        if is_pause_method(&prev.match_method) || is_pause_method(&curr.match_method) {
            continue;
        }
        // Legitimate post-teleport relocation jumps transition from a short post-boundary hold
        // (stale pre-teleport pose) to the newly acquired destination pose.
        if is_in_short_post_boundary_hold(observed, idx - 1) {
            continue;
        }
        let (Some((pm, px, py)), Some((cm, cx, cy))) = (valid_pos(prev), valid_pos(curr)) else {
            continue;
        };
        if pm != cm {
            continue;
        }
        let dist = (cx - px).hypot(cy - py);
        let raw_dt = if curr.ts.is_finite() && prev.ts.is_finite() {
            curr.ts - prev.ts
        } else {
            STEP_DT_FLOOR_S
        };
        let dt = raw_dt.max(STEP_DT_FLOOR_S);
        let speed = dist / dt;
        if dist > MAX_CONSECUTIVE_STEP_W || speed > MAX_PLAUSIBLE_SPEED_W_PER_S {
            // If `curr` is a high-confidence anchor recovering from a non-anchor frame, flag the
            // stale pre-recovery frame (`prev`) rather than the anchor itself.
            if is_high_confidence_anchor(curr, meta) && !is_high_confidence_anchor(prev, meta) {
                record_suspect(&mut suspects, prev.idx, dist);
            } else {
                record_suspect(&mut suspects, curr.idx, dist);
            }
        }
    }

    let suspect_frames = suspects.len();
    let (worst_frame, worst_distance_w) = if suspect_frames == 0 {
        (None, None)
    } else {
        let mut best_f = 0usize;
        let mut best_d = f64::NEG_INFINITY;
        for (&f_idx, &d) in &suspects {
            if d > best_d {
                best_d = d;
                best_f = f_idx;
            }
        }
        (Some(best_f), Some(round3(best_d.max(0.0))))
    };

    TruthOracleDetail {
        summary: TruthSummary {
            suspect_frames,
            worst_frame,
            worst_distance_w,
        },
        suspects,
    }
}

/// Evaluates the informational truth oracle and returns its compact [`TruthSummary`].
pub fn evaluate_truth_oracle(
    observed: &[ObservedFrame],
    accuracy: &AccuracyReport,
    meta: &ReplayTruthMeta,
) -> TruthSummary {
    evaluate_truth_oracle_detailed(observed, accuracy, meta).summary
}

/// Formats the `Truth` column for `gh-sim qa` (`"ok"` when 0 suspects, else
/// `"susp <N>f max <D>w@f<idx>"`; `"n/a"` when absent).
pub fn format_truth_col(truth: Option<&TruthSummary>) -> String {
    let Some(t) = truth else {
        return "n/a".to_owned();
    };
    if t.suspect_frames == 0 {
        return "ok".to_owned();
    }
    match (t.worst_distance_w, t.worst_frame) {
        (Some(d), Some(f)) => format!("susp {}f max {:.0}w@f{}", t.suspect_frames, d, f),
        (None, Some(f)) => format!("susp {}f max 0w@f{}", t.suspect_frames, f),
        _ => format!("susp {}f", t.suspect_frames),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::AccuracyCheckpointOutcome;

    fn mk_frame(
        idx: usize,
        ts: f64,
        state: &str,
        match_method: &str,
        map_id: Option<i32>,
        xy: Option<(f64, f64)>,
    ) -> ObservedFrame {
        ObservedFrame {
            idx,
            ts,
            state: state.to_owned(),
            match_method: match_method.to_owned(),
            map_id,
            world_x: xy.map(|(x, _)| x),
            world_y: xy.map(|(_, y)| y),
            heading: Some(90.0),
            body_heading: None,
            camera_heading: None,
            is_underground: false,
        }
    }

    #[test]
    fn clean_walking_and_gliding_track_has_zero_suspects() {
        // High-confidence M-Map anchor followed by smooth sprinting/gliding (~15 w/s) and a
        // layer_switch_sift anchor at the end.
        let mut frames = Vec::new();
        let mut meta = ReplayTruthMeta::default();
        frames.push(mk_frame(
            1,
            10.0,
            "MAP_M_SYNCED",
            "map_m",
            Some(2),
            Some((100.0, 200.0)),
        ));
        meta.map_m_inliers.insert(1, 10);

        for i in 2..=40usize {
            let dt = (i - 1) as f64 * 0.1;
            let method = if i == 40 {
                "layer_switch_sift"
            } else if i % 5 == 0 {
                "hd_sift"
            } else {
                "hd_sobel_fast"
            };
            // 15 w/s diagonal glide
            frames.push(mk_frame(
                i,
                10.0 + dt,
                "TRACKING",
                method,
                Some(2),
                Some((100.0 + 12.0 * dt, 200.0 + 9.0 * dt)),
            ));
        }

        let res = evaluate_truth_oracle(&frames, &AccuracyReport::default(), &meta);
        assert_eq!(res.suspect_frames, 0, "clean track must have 0 suspects: {res:?}");
        assert_eq!(res.worst_frame, None);
        assert_eq!(res.worst_distance_w, None);
        assert_eq!(format_truth_col(Some(&res)), "ok");
    }

    #[test]
    fn legitimate_teleport_across_boundaries_has_zero_suspects() {
        // 1. Track at (100, 200)
        // 2. Open M-Map, pan, close, loading screen (`paimon_pause`), 3-frame post-teleport
        //    `gate_hold`/`relocating_hold` holding old pose, then `global_teyvat_relocate` to
        //    (4500, -1200)
        // 3. Another loading_pause -> 2-frame `gate_hold` -> `layer_switch_sift` to (4200, -900)
        // 4. High-inlier MAP_M_SYNCED at (4200, -900)
        let mut meta = ReplayTruthMeta::default();
        let frames = vec![
            mk_frame(1, 1.0, "TRACKING", "hd_sobel_fast", Some(2), Some((100.0, 200.0))),
            mk_frame(2, 1.1, "TRACKING", "hd_sobel_fast", Some(2), Some((101.0, 200.5))),
            mk_frame(3, 1.2, "MAP_M_Settling", "map_m", None, None),
            mk_frame(4, 1.3, "MAP_M_SYNCED", "map_m", Some(2), Some((101.0, 200.5))),
            mk_frame(5, 1.5, "MAP_M_Panning", "map_m", None, None),
            mk_frame(6, 1.7, "MAP_M_Closed", "map_m", None, None),
            mk_frame(7, 1.8, "TRACKING", "paimon_pause", Some(2), Some((101.0, 200.5))),
            mk_frame(8, 2.5, "TRACKING", "paimon_pause", Some(2), Some((101.0, 200.5))),
            mk_frame(9, 2.6, "TRACKING", "gate_hold", Some(2), Some((101.0, 200.5))),
            mk_frame(10, 2.7, "TRACKING", "relocating_hold", Some(2), Some((101.0, 200.5))),
            mk_frame(11, 2.8, "TRACKING", "relocating_hold", Some(2), Some((101.0, 200.5))),
            mk_frame(12, 2.9, "TRACKING", "global_teyvat_relocate", Some(2), Some((4500.0, -1200.0))),
            mk_frame(13, 3.0, "TRACKING", "hd_sift", Some(2), Some((4500.5, -1200.0))),
            mk_frame(14, 3.5, "TRACKING", "loading_pause", Some(2), Some((4500.5, -1200.0))),
            mk_frame(15, 3.6, "TRACKING", "gate_hold", Some(2), Some((4500.5, -1200.0))),
            mk_frame(16, 3.7, "TRACKING", "gate_hold", Some(2), Some((4500.5, -1200.0))),
            mk_frame(17, 3.8, "TRACKING", "layer_switch_sift", Some(2), Some((4200.0, -900.0))),
            mk_frame(18, 3.9, "TRACKING", "hd_sobel_fast", Some(2), Some((4201.0, -900.0))),
            mk_frame(19, 4.0, "MAP_M_Settling", "map_m", None, None),
            mk_frame(20, 4.1, "MAP_M_SYNCED", "map_m", Some(2), Some((4201.0, -900.0))),
        ];
        meta.map_m_inliers.insert(4, 9);
        meta.map_m_inliers.insert(20, 8);

        let detail = evaluate_truth_oracle_detailed(&frames, &AccuracyReport::default(), &meta);
        assert_eq!(
            detail.summary.suspect_frames, 0,
            "legitimate teleports across boundaries must NOT be flagged: {:?}",
            detail.suspects
        );
    }

    #[test]
    fn false_lock_flagged_by_anchor_contradiction_and_unphysical_jump() {
        // Sub-case A: Mirrors a low-inlier M-Map false lock (f10 inliers=2 at (5014,-152),
        // f12 inliers=3 PendingVerify at (512,-164) with no panning, M-Map closes without
        // loading screen, 15 frames of stuck hold at (512,-164), then layer_switch_sift recovery
        // at (352,4.6)).
        let mut meta = ReplayTruthMeta::default();
        let mut frames = vec![
            mk_frame(9, 10.0, "MAP_M_Settling", "map_m", None, None),
            mk_frame(10, 10.1, "MAP_M_SYNCED", "map_m", Some(40), Some((5014.0, -152.0))),
            mk_frame(11, 10.3, "MAP_M_SyncedIdle", "map_m", None, None),
            mk_frame(12, 10.5, "MAP_M_SYNCED", "map_m", Some(40), Some((512.0, -164.0))),
            mk_frame(13, 10.7, "MAP_M_SyncedIdle", "map_m", None, None),
            mk_frame(14, 10.9, "MAP_M_Closed", "map_m", None, None),
        ];
        meta.map_m_inliers.insert(10, 2);
        meta.map_m_inliers.insert(12, 3);

        for k in 0..15usize {
            let idx = 15 + k;
            let method = if k == 0 { "gate_hold" } else { "relocating_hold" };
            frames.push(mk_frame(
                idx,
                11.0 + k as f64 * 0.1,
                "TRACKING",
                method,
                Some(40),
                Some((511.8, -163.6)),
            ));
        }
        // f30: recovery anchor via layer_switch_sift
        frames.push(mk_frame(
            30,
            12.5,
            "TRACKING",
            "layer_switch_sift",
            Some(40),
            Some((352.0, 4.6)),
        ));
        // f31: normal tracking near recovery
        frames.push(mk_frame(
            31,
            12.6,
            "TRACKING",
            "hd_sift",
            Some(40),
            Some((351.5, 4.5)),
        ));
        // f32: sudden unphysical jump (+300w in 0.1s) with NO boundary!
        frames.push(mk_frame(
            32,
            12.7,
            "TRACKING",
            "hd_sobel_fast",
            Some(40),
            Some((651.5, 4.5)),
        ));

        let detail = evaluate_truth_oracle_detailed(&frames, &AccuracyReport::default(), &meta);
        // f10, f12, f15..=29 (15 frames), and f32 must ALL be flagged; f30 & f31 must NOT!
        assert!(detail.suspects.contains_key(&10), "f10 false lock missing: {:?}", detail.suspects);
        assert!(detail.suspects.contains_key(&12), "f12 false lock missing: {:?}", detail.suspects);
        for f_idx in 15..=29 {
            assert!(
                detail.suspects.contains_key(&f_idx),
                "f{f_idx} stuck hold missing: {:?}",
                detail.suspects
            );
        }
        assert!(detail.suspects.contains_key(&32), "f32 unphysical jump missing: {:?}", detail.suspects);
        assert!(!detail.suspects.contains_key(&30), "f30 recovery anchor must NOT be flagged");
        assert!(!detail.suspects.contains_key(&31), "f31 clean tracking must NOT be flagged");
        assert_eq!(detail.summary.suspect_frames, 18);
        assert_eq!(detail.summary.worst_frame, Some(10));
        assert!(detail.summary.worst_distance_w.unwrap() > 4600.0);

        let col = format_truth_col(Some(&detail.summary));
        assert!(col.starts_with("susp 18f max "), "unexpected col: {col}");
        assert!(col.ends_with("w@f10"), "unexpected col: {col}");
    }

    #[test]
    fn verified_gt_failed_checkpoint_and_false_lock_are_flagged() {
        let frames = vec![
            mk_frame(1, 1.0, "TRACKING", "hd_sobel_fast", Some(2), Some((10.0, 20.0))),
            mk_frame(2, 1.1, "TRACKING", "hd_sobel_fast", Some(2), Some((11.0, 20.0))),
            mk_frame(3, 1.2, "TRACKING", "hd_sobel_fast", Some(2), Some((12.0, 20.0))),
        ];
        let acc = AccuracyReport {
            has_verified_gt: true,
            checkpoints_evaluated: 3,
            checkpoints_passed: 1,
            false_lock_count: 1,
            details: vec![
                AccuracyCheckpointOutcome {
                    frame_idx: 1,
                    ts: 1.0,
                    passed: true,
                    false_lock: false,
                    expected_map_id: Some(2),
                    actual_map_id: Some(2),
                    pos_err_w: Some(2.5),
                    heading_err_deg: None,
                    note: "good".to_owned(),
                },
                AccuracyCheckpointOutcome {
                    frame_idx: 2,
                    ts: 1.1,
                    passed: false,
                    false_lock: true,
                    expected_map_id: Some(2),
                    actual_map_id: Some(2),
                    pos_err_w: Some(412.3),
                    heading_err_deg: None,
                    note: "false lock".to_owned(),
                },
                AccuracyCheckpointOutcome {
                    frame_idx: 3,
                    ts: 1.2,
                    passed: false,
                    false_lock: false,
                    expected_map_id: Some(40),
                    actual_map_id: Some(2),
                    pos_err_w: None,
                    heading_err_deg: None,
                    note: "map mismatch".to_owned(),
                },
            ],
            ..Default::default()
        };

        let detail = evaluate_truth_oracle_detailed(&frames, &acc, &ReplayTruthMeta::default());
        assert_eq!(detail.summary.suspect_frames, 2);
        assert!(!detail.suspects.contains_key(&1));
        assert_eq!(detail.suspects.get(&2), Some(&412.3));
        assert_eq!(detail.suspects.get(&3), Some(&0.0));
        assert_eq!(detail.summary.worst_frame, Some(2));
        assert_eq!(detail.summary.worst_distance_w, Some(412.3));
        assert_eq!(format_truth_col(Some(&detail.summary)), "susp 2f max 412w@f2");
    }

    #[test]
    fn format_truth_col_formats_ok_suspect_and_na() {
        assert_eq!(format_truth_col(None), "n/a");
        assert_eq!(format_truth_col(Some(&TruthSummary::default())), "ok");
        assert_eq!(
            format_truth_col(Some(&TruthSummary {
                suspect_frames: 118,
                worst_frame: Some(249),
                worst_distance_w: Some(4630.4),
            })),
            "susp 118f max 4630w@f249"
        );
    }
}
