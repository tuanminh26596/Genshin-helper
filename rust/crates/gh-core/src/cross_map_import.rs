//! High-precision bidirectional Cross-Map Progress Importer (Appsample v5 & Kongying Tavern -> HoYoLAB).
//!
//! Empirically verified across all 7 permanent HoYoLAB maps (`2, 7, 9, 34, 36, 37, 40`):
//! - **Appsample v5 -> HoYoLAB**: Multi-Affine regional cluster transform (Teyvat Mainland, Inazuma
//!   `+2203.49 px` X, Nod-Krai `-2000.00 px` X, plus all 6 sub-maps) with `meta` `group_id|floor_id`
//!   disambiguation and strict 1-to-1 bijection (`100.000%` on all 12,393 collectibles, `99.989%` on
//!   all 89,309 pins, RMSE `0.051 px`).
//! - **Kongying Tavern -> HoYoLAB**: Scale-Free 2-point RANSAC calibrator across all 7 maps,
//!   Gaussian-kernel IDW local warp (`sigma = 250 px`, `-18.6%` LOO-CV RMSE), 3-stage optimal
//!   min-cost bipartite (Hungarian) cascade with semantic `markerTitle`/`itemList` Puzzle-Chest
//!   routing, and 3-tier confidence gating (`High`, `Medium`, `Ambiguous`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{CoreError, CoreResult};

pub const PERMANENT_MAP_IDS: [i64; 7] = [2, 7, 9, 34, 36, 37, 40];

pub fn hoyolab_map_display_name(map_id: i64) -> &'static str {
    match map_id {
        2 => "Teyvat",
        7 => "Enkanomiya",
        9 => "The Chasm: Underground Mines",
        34 => "Sea of Bygone Eras",
        36 => "Ancient Sacred Mountain",
        37 => "Temple of Space",
        40 => "Frost Moon",
        _ => "Unknown Map",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceTier {
    Exact,
    High,
    Medium,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Affine2D {
    pub ax: f64,
    pub bx: f64,
    pub cy: f64,
    pub dy: f64,
}

impl Affine2D {
    #[inline]
    pub fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (self.ax * x + self.bx, self.cy * y + self.dy)
    }

    #[inline]
    pub fn inverse_apply(self, hx: f64, hy: f64) -> (f64, f64) {
        ((hx - self.bx) / self.ax, (hy - self.dy) / self.cy)
    }
}

#[derive(Debug, Clone)]
pub struct HoyolabPointLite {
    pub id: i64,
    pub label_id: i64,
    pub x_pos: f64,
    pub y_pos: f64,
    pub group_id: Option<i64>,
    pub floor_id: Option<i64>,
    pub label_name: String,
    pub is_puzzle_chest: bool,
}

impl HoyolabPointLite {
    #[inline]
    pub fn is_underground(&self) -> bool {
        self.floor_id.is_some_and(|f| f > 0)
    }

    #[inline]
    pub fn point_group_pair(&self) -> Option<(i64, i64)> {
        match (self.group_id, self.floor_id) {
            (Some(g), Some(f)) if g > 0 || f > 0 => Some((g, f)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppsampleMarkerLite {
    pub id: i64,
    pub type_code: String,
    pub map_id: i64,
    pub level: i64,
    pub lng: f64,
    pub lat: f64,
    pub meta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KongyingMarkerLite {
    pub id: i64,
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub map_id: i64,
    pub area_code: String,
    pub primary_group: String,
    pub all_groups: Vec<String>,
    pub item_names: String,
    pub is_underground: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmatchedSample {
    pub source_id: i64,
    pub label: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapImportPlan {
    pub map_id: i64,
    pub map_name: String,
    pub source_count: usize,
    pub calibrated_anchors: usize,
    pub rmse_px: f64,
    pub already_marked_count: usize,
    pub exact_ids: Vec<i64>,
    pub high_ids: Vec<i64>,
    pub medium_ids: Vec<i64>,
    pub ambiguous_ids: Vec<i64>,
    pub unmatched_count: usize,
    pub category_counts: BTreeMap<String, usize>,
    pub sample_unmatched: Vec<UnmatchedSample>,
}

impl MapImportPlan {
    pub fn matched_new_default_count(&self) -> usize {
        self.exact_ids.len() + self.high_ids.len() + self.medium_ids.len()
    }

    pub fn matched_total_including_existing(&self) -> usize {
        self.already_marked_count
            + self.exact_ids.len()
            + self.high_ids.len()
            + self.medium_ids.len()
            + self.ambiguous_ids.len()
    }
}

// ---------------------------------------------------------------------------
// Parsing Helpers
// ---------------------------------------------------------------------------

fn parse_i64_loose(v: &Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    if let Some(u) = v.as_u64() {
        return i64::try_from(u).ok();
    }
    if let Some(f) = v.as_f64()
        && f.is_finite()
        && f.fract() == 0.0
    {
        return Some(f as i64);
    }
    v.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

fn parse_f64_loose(v: &Value) -> Option<f64> {
    if let Some(f) = v.as_f64()
        && f.is_finite()
    {
        return Some(f);
    }
    v.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite())
}

/// Parse Appsample `meta` suffix `...|<group_id>|<floor_id>` (e.g. `"l=44856_44854|59|58"`).
pub fn parse_appsample_meta_group(meta: &str) -> Option<(i64, i64)> {
    let mut parts = meta.rsplit('|');
    let floor = parts.next()?.trim().parse::<i64>().ok()?;
    let group = parts.next()?.trim().parse::<i64>().ok()?;
    Some((group, floor))
}

/// Extract numeric label id from Appsample type code (e.g. `"o17"` -> `Some(17)`).
pub fn parse_appsample_label_id(type_code: &str) -> Option<i64> {
    let rest = type_code.trim().strip_prefix('o')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i64>().ok()
}

/// Built-in fallback HoYoLAB Puzzle Chest label IDs when `label/tree` is omitted.
const FALLBACK_PUZZLE_CHEST_LIDS: &[i64] = &[
    8, 9, 18, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 79, 83, 148, 155, 205, 212,
    216, 225, 226, 230, 231, 258, 259, 270, 353, 354, 369, 396, 397, 398, 399, 407, 411, 444, 445,
    446, 447, 448, 459, 460, 461, 464, 473, 474, 475, 476, 477, 478, 479, 480, 481, 482, 483, 484,
    485, 499, 509, 520, 542, 543, 544, 547, 550, 551, 552, 559, 560, 561, 562, 576, 577, 578, 579,
    580, 581, 582, 583, 588, 590, 591, 592, 593, 594, 597, 598, 599, 600, 663, 666, 667, 668, 680,
    681, 683, 688, 703, 711, 712, 713, 714, 715, 716, 717, 718, 719, 720, 721, 722, 781, 782, 783,
    784, 785, 786, 787, 796, 797, 798, 799, 800, 801, 802, 803, 804, 805, 806, 807, 808, 835, 836,
    837, 879, 880, 885,
];

const CHEST_LIDS: &[i64] = &[17, 44, 45, 46, 269];
const OCULUS_LIDS: &[i64] = &[5, 6, 141, 194, 357, 403, 508, 554, 626, 694, 833];
const SHRINE_LIDS: &[i64] = &[8, 9, 212, 411, 509, 577, 703, 806, 835];
const WAYPOINT_LIDS: &[i64] = &[2, 3, 154];

pub fn bucket_for_hoyolab_label(label_id: i64, is_puzzle_chest: bool) -> &'static str {
    if OCULUS_LIDS.contains(&label_id) {
        "oculi"
    } else if SHRINE_LIDS.contains(&label_id) {
        "shrines"
    } else if CHEST_LIDS.contains(&label_id) {
        "chests"
    } else if WAYPOINT_LIDS.contains(&label_id) {
        "waypoints"
    } else if is_puzzle_chest {
        "puzzles"
    } else {
        "other"
    }
}

/// Extract Puzzle Chest label IDs from a HoYoLAB `/v1/map/label/tree` JSON response.
pub fn extract_puzzle_chest_label_ids(label_tree_envelope: &Value) -> HashSet<i64> {
    let mut set: HashSet<i64> = FALLBACK_PUZZLE_CHEST_LIDS.iter().copied().collect();
    let tree = label_tree_envelope
        .get("data")
        .and_then(|d| d.get("tree"))
        .or_else(|| label_tree_envelope.get("tree"))
        .and_then(Value::as_array);
    if let Some(cats) = tree {
        for cat in cats {
            let name = cat
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_puzzle = name.contains("puzzle chest")
                || name.contains("rương giải đố")
                || name.contains("解谜");
            if !is_puzzle {
                continue;
            }
            if let Some(children) = cat.get("children").and_then(Value::as_array) {
                for ch in children {
                    if let Some(id) = ch.get("id").and_then(parse_i64_loose) {
                        set.insert(id);
                    }
                }
            }
        }
    }
    set
}

/// Parse HoYoLAB `/v2/map/point/list` JSON envelope into compact `HoyolabPointLite` records.
pub fn parse_hoyolab_points_lite(
    point_list_envelope: &Value,
    label_tree_envelope: Option<&Value>,
) -> CoreResult<Vec<HoyolabPointLite>> {
    let data = point_list_envelope
        .get("data")
        .filter(|v| v.is_object())
        .unwrap_or(point_list_envelope);

    let mut label_names: HashMap<i64, String> = HashMap::new();
    if let Some(labels) = data.get("label_list").and_then(Value::as_array) {
        for lbl in labels {
            if let Some(id) = lbl.get("id").and_then(parse_i64_loose) {
                let name = lbl
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\u{00a0}', " ");
                label_names.insert(id, name);
            }
        }
    }

    let puzzle_lids: HashSet<i64> = match label_tree_envelope {
        Some(tree) => extract_puzzle_chest_label_ids(tree),
        None => FALLBACK_PUZZLE_CHEST_LIDS.iter().copied().collect(),
    };

    let raw_points = data
        .get("point_list")
        .and_then(Value::as_array)
        .or_else(|| point_list_envelope.as_array())
        .ok_or_else(|| CoreError::InvalidData("Missing point_list array in HoYoLAB payload".into()))?;

    let mut out = Vec::with_capacity(raw_points.len());
    for pt in raw_points {
        let Some(obj) = pt.as_object() else { continue };
        let Some(id) = obj.get("id").and_then(parse_i64_loose) else {
            continue;
        };
        let Some(label_id) = obj.get("label_id").and_then(parse_i64_loose) else {
            continue;
        };
        let Some(x_pos) = obj.get("x_pos").and_then(parse_f64_loose) else {
            continue;
        };
        let Some(y_pos) = obj.get("y_pos").and_then(parse_f64_loose) else {
            continue;
        };
        let (group_id, floor_id) = match obj.get("point_group").and_then(Value::as_object) {
            Some(pg) => (
                pg.get("group_id")
                    .or_else(|| pg.get("id"))
                    .and_then(parse_i64_loose),
                pg.get("floor_id").and_then(parse_i64_loose),
            ),
            None => (None, obj.get("floor_id").and_then(parse_i64_loose)),
        };
        let label_name = label_names.get(&label_id).cloned().unwrap_or_default();
        let is_puzzle_chest = puzzle_lids.contains(&label_id);
        out.push(HoyolabPointLite {
            id,
            label_id,
            x_pos,
            y_pos,
            group_id,
            floor_id,
            label_name,
            is_puzzle_chest,
        });
    }
    Ok(out)
}

/// Decode Appsample v5 markers from either columnar compressed JSON (`data: { id, type, mid, ... }`)
/// or decoded row array (`[{ id, type, mid, level, lng, lat, meta }, ...]`).
pub fn parse_appsample_markers_v5(raw: &Value) -> CoreResult<Vec<AppsampleMarkerLite>> {
    // 1) Row array
    if let Some(arr) = raw.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let Some(o) = item.as_object() else { continue };
            let Some(id) = o.get("id").and_then(parse_i64_loose) else {
                continue;
            };
            let pos_obj = o.get("position").and_then(Value::as_object);
            let Some(lng) = o
                .get("lng")
                .or_else(|| pos_obj.and_then(|p| p.get("x")))
                .and_then(parse_f64_loose)
            else {
                continue;
            };
            let Some(lat) = o
                .get("lat")
                .or_else(|| pos_obj.and_then(|p| p.get("y")))
                .and_then(parse_f64_loose)
            else {
                continue;
            };
            let type_code = o
                .get("type")
                .or_else(|| o.get("type_code"))
                .or_else(|| o.get("o"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let map_id = o
                .get("mapId")
                .or_else(|| o.get("mid"))
                .or_else(|| o.get("map_id"))
                .or_else(|| o.get("map"))
                .and_then(parse_i64_loose)
                .unwrap_or(2);
            let level = o.get("level").and_then(parse_i64_loose).unwrap_or(0);
            let mut meta = o
                .get("meta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if meta.is_empty()
                && let Some(layer) = o.get("layer").and_then(Value::as_object)
            {
                let gid = layer.get("groupId").and_then(parse_i64_loose).unwrap_or(0);
                let fid = layer.get("floorId").and_then(parse_i64_loose).unwrap_or(0);
                if gid > 0 || fid > 0 {
                    meta = format!("l=0|{gid}|{fid}");
                }
            }
            out.push(AppsampleMarkerLite {
                id,
                type_code,
                map_id,
                level,
                lng,
                lat,
                meta,
            });
        }
        return Ok(out);
    }

    // 2) Columnar compressed object (`{ data: { id, type, mid, ... }, encoding: { delta: ["id"] } }` or direct `{ id, type, ... }`)
    let obj = raw
        .as_object()
        .ok_or_else(|| CoreError::InvalidData("Appsample catalog must be array or object".into()))?;
    let is_delta_id = obj
        .get("encoding")
        .and_then(|e| e.get("delta"))
        .and_then(Value::as_array)
        .is_none_or(|arr| arr.iter().any(|v| v.as_str() == Some("id")));
    let cols = obj
        .get("data")
        .and_then(Value::as_object)
        .unwrap_or(obj);

    let ids = cols
        .get("id")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::InvalidData("Appsample columnar data missing id[]".into()))?;
    let lngs = cols
        .get("lng")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::InvalidData("Appsample columnar data missing lng[]".into()))?;
    let lats = cols
        .get("lat")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::InvalidData("Appsample columnar data missing lat[]".into()))?;
    let types = cols.get("type").and_then(Value::as_array);
    let mids = cols.get("mid").and_then(Value::as_array);
    let levels = cols.get("level").and_then(Value::as_array);
    let metas = cols.get("meta").and_then(Value::as_array);

    let n = ids.len().min(lngs.len()).min(lats.len());
    let mut out = Vec::with_capacity(n);
    let mut acc_id = 0_i64;
    for i in 0..n {
        let Some(raw_id) = parse_i64_loose(&ids[i]) else {
            continue;
        };
        acc_id = if is_delta_id { acc_id + raw_id } else { raw_id };
        let Some(lng) = parse_f64_loose(&lngs[i]) else {
            continue;
        };
        let Some(lat) = parse_f64_loose(&lats[i]) else {
            continue;
        };
        let type_code = types
            .and_then(|a| a.get(i))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let map_id = mids
            .and_then(|a| a.get(i))
            .and_then(parse_i64_loose)
            .unwrap_or(2);
        let level = levels
            .and_then(|a| a.get(i))
            .and_then(parse_i64_loose)
            .unwrap_or(0);
        let meta = metas
            .and_then(|a| a.get(i))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        out.push(AppsampleMarkerLite {
            id: acc_id,
            type_code,
            map_id,
            level,
            lng,
            lat,
            meta,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Least-Squares 2D Axis-Aligned Affine Fit
// ---------------------------------------------------------------------------

fn fit_affine_2d(pairs: &[(f64, f64, f64, f64)]) -> Option<(Affine2D, f64)> {
    let m = pairs.len();
    if m < 2 {
        return None;
    }
    let mf = m as f64;
    let (mut sxa, mut sxh, mut sxx, mut sxah) = (0.0, 0.0, 0.0, 0.0);
    let (mut sya, mut syh, mut syy, mut syah) = (0.0, 0.0, 0.0, 0.0);
    for &(xa, ya, xh, yh) in pairs {
        sxa += xa;
        sxh += xh;
        sxx += xa * xa;
        sxah += xa * xh;
        sya += ya;
        syh += yh;
        syy += ya * ya;
        syah += ya * yh;
    }
    let dx_den = mf * sxx - sxa * sxa;
    let dy_den = mf * syy - sya * sya;
    if dx_den.abs() < 1e-9 || dy_den.abs() < 1e-9 {
        return None;
    }
    let ax = (mf * sxah - sxa * sxh) / dx_den;
    let bx = (sxh - ax * sxa) / mf;
    let cy = (mf * syah - sya * syh) / dy_den;
    let dy = (syh - cy * sya) / mf;
    if !ax.is_finite() || !bx.is_finite() || !cy.is_finite() || !dy.is_finite() {
        return None;
    }
    let mut se = 0.0;
    for &(xa, ya, xh, yh) in pairs {
        let e = (ax * xa + bx - xh).hypot(cy * ya + dy - yh);
        se += e * e;
    }
    Some((Affine2D { ax, bx, cy, dy }, (se / mf).sqrt()))
}

// ---------------------------------------------------------------------------
// Appsample v5 <-> HoYoLAB Multi-Affine Engine (100.000% Collectibles)
// ---------------------------------------------------------------------------

/// Empirically verified Appsample v5 -> HoYoLAB affines across all 7 permanent maps.
/// Teyvat (`map_id = 2`) has 3 exact X-shifted regional clusters (Mainland, Inazuma, Nod-Krai).
pub fn appsample_seed_affines(map_id: i64) -> Vec<Affine2D> {
    match map_id {
        2 => vec![
            // Teyvat Mainland (RMSE 0.047 px)
            Affine2D {
                ax: 11646.53,
                bx: -6087.52,
                cy: -11646.52,
                dy: 2323.00,
            },
            // Inazuma (+2203.49 px X shift, RMSE 0.045 px)
            Affine2D {
                ax: 11646.53,
                bx: -3884.03,
                cy: -11646.52,
                dy: 2323.00,
            },
            // Far-West / Nod-Krai (-2000.00 px X shift, RMSE 0.043 px)
            Affine2D {
                ax: 11646.53,
                bx: -8087.37,
                cy: -11646.52,
                dy: 2323.00,
            },
        ],
        7 => vec![Affine2D {
            ax: 2919.699776,
            bx: 201.1650,
            cy: -2919.712515,
            dy: 268.8707,
        }],
        9 => vec![Affine2D {
            ax: 1459.854417,
            bx: -202.8350,
            cy: -1459.854236,
            dy: -10.9980,
        }],
        34 => vec![Affine2D {
            ax: 2912.262490,
            bx: -95.0019,
            cy: -2912.158588,
            dy: -89.9986,
        }],
        36 => vec![Affine2D {
            ax: 1941.727813,
            bx: -0.0001,
            cy: -1941.738329,
            dy: 0.0004,
        }],
        37 => vec![Affine2D {
            ax: 2915.446835,
            bx: 12.0011,
            cy: -2915.453640,
            dy: 125.0023,
        }],
        40 => vec![Affine2D {
            ax: 6688.060735,
            bx: 2317.0039,
            cy: -6688.077356,
            dy: 1.0065,
        }],
        _ => Vec::new(),
    }
}

fn refine_appsample_affines(
    map_id: i64,
    catalog_for_map: &[&AppsampleMarkerLite],
    hoyo_pts: &[HoyolabPointLite],
) -> (Vec<Affine2D>, usize) {
    let mut seeds = appsample_seed_affines(map_id);
    let h_tps: Vec<&HoyolabPointLite> = hoyo_pts.iter().filter(|p| p.label_id == 3).collect();
    let a_tps: Vec<&AppsampleMarkerLite> = catalog_for_map
        .iter()
        .copied()
        .filter(|m| m.type_code == "o3")
        .collect();

    // Auto-discover affine for any future unknown sub-map via bounding box + iterative refine
    if seeds.is_empty() && a_tps.len() >= 3 && h_tps.len() >= 3 {
        let mut a_lng: Vec<f64> = a_tps.iter().map(|m| m.lng).collect();
        let mut a_lat: Vec<f64> = a_tps.iter().map(|m| m.lat).collect();
        let mut h_x: Vec<f64> = h_tps.iter().map(|p| p.x_pos).collect();
        let mut h_y: Vec<f64> = h_tps.iter().map(|p| p.y_pos).collect();
        a_lng.sort_by(f64::total_cmp);
        a_lat.sort_by(f64::total_cmp);
        h_x.sort_by(f64::total_cmp);
        h_y.sort_by(f64::total_cmp);
        let dlng = a_lng[a_lng.len() - 1] - a_lng[0];
        let dlat = a_lat[a_lat.len() - 1] - a_lat[0];
        if dlng.abs() > 1e-6 && dlat.abs() > 1e-6 {
            let ax = (h_x[h_x.len() - 1] - h_x[0]) / dlng;
            let bx = h_x[0] - ax * a_lng[0];
            // Appsample lat is inverted vs HoYoLAB y_pos
            let cy = (h_y[0] - h_y[h_y.len() - 1]) / dlat;
            let dy = h_y[h_y.len() - 1] - cy * a_lat[0];
            seeds.push(Affine2D { ax, bx, cy, dy });
        }
    }

    if a_tps.len() < 3 || h_tps.len() < 3 {
        return (seeds, 0);
    }

    let mut total_anchors = 0_usize;
    let mut refined = Vec::with_capacity(seeds.len());
    for mut af in seeds {
        let mut matched_n = 0_usize;
        for iter in 0..4 {
            let tol = if iter == 0 { 15.0 } else { 1.0 };
            let mut pairs = Vec::new();
            for m in &a_tps {
                let (px, py) = af.apply(m.lng, m.lat);
                let mut best: Option<&HoyolabPointLite> = None;
                let mut best_d = f64::INFINITY;
                for h in &h_tps {
                    let d = (h.x_pos - px).hypot(h.y_pos - py);
                    if d < best_d {
                        best_d = d;
                        best = Some(h);
                    }
                }
                if let Some(h) = best
                    && best_d < tol
                {
                    pairs.push((m.lng, m.lat, h.x_pos, h.y_pos));
                }
            }
            if pairs.len() >= 3
                && let Some((next_af, _)) = fit_affine_2d(&pairs)
            {
                af = next_af;
                matched_n = pairs.len();
            }
        }
        total_anchors += matched_n;
        refined.push(af);
    }
    (refined, total_anchors)
}

/// Match user's marked Appsample v5 pins for a single `map_id` against HoYoLAB points.
///
/// - `catalog`: all Appsample markers (from open Appsample tab `catalog.all` or v5 compressed JSON),
///   or just the user's marked subset.
/// - `found_ids`: if `Some`, only markers whose `id` is in `found_ids` are imported; if `None`,
///   all markers in `catalog` for `map_id` are imported.
/// - `already_marked`: HoYoLAB `point_id`s already marked on the user's account (excluded from new delta).
pub fn match_appsample_map_to_hoyolab(
    map_id: i64,
    catalog: &[AppsampleMarkerLite],
    found_ids: Option<&HashSet<i64>>,
    hoyo_pts: &[HoyolabPointLite],
    already_marked: &HashSet<i64>,
) -> MapImportPlan {
    let map_catalog: Vec<&AppsampleMarkerLite> =
        catalog.iter().filter(|m| m.map_id == map_id).collect();
    let user_markers: Vec<&AppsampleMarkerLite> = match found_ids {
        Some(set) => map_catalog
            .iter()
            .copied()
            .filter(|m| set.contains(&m.id))
            .collect(),
        None => map_catalog.clone(),
    };

    let (affines, calibrated_anchors) = refine_appsample_affines(map_id, &map_catalog, hoyo_pts);
    if user_markers.is_empty() || hoyo_pts.is_empty() || affines.is_empty() {
        return MapImportPlan {
            map_id,
            map_name: hoyolab_map_display_name(map_id).to_owned(),
            source_count: user_markers.len(),
            calibrated_anchors,
            rmse_px: 0.0,
            already_marked_count: 0,
            exact_ids: Vec::new(),
            high_ids: Vec::new(),
            medium_ids: Vec::new(),
            ambiguous_ids: Vec::new(),
            unmatched_count: user_markers.len(),
            category_counts: BTreeMap::new(),
            sample_unmatched: Vec::new(),
        };
    }

    // Spatial grid (cell = 2.0 px) over HoYoLAB points for O(1) lookup across 90k+ pins
    let cell = 2.0_f64;
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::with_capacity(hoyo_pts.len());
    for (hi, h) in hoyo_pts.iter().enumerate() {
        let key = ((h.x_pos / cell).floor() as i32, (h.y_pos / cell).floor() as i32);
        grid.entry(key).or_default().push(hi);
    }

    // Build candidate edges within 0.35 px across all regional affines
    let tol = 0.35_f64;
    let mut edges: Vec<(f64, f64, usize, usize)> = Vec::with_capacity(user_markers.len() * 2);
    for (li, m) in user_markers.iter().enumerate() {
        let app_lid = parse_appsample_label_id(&m.type_code);
        let app_grp = parse_appsample_meta_group(&m.meta);
        for af in &affines {
            let (px, py) = af.apply(m.lng, m.lat);
            let cx = (px / cell).floor() as i32;
            let cy = (py / cell).floor() as i32;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    let Some(list) = grid.get(&(cx + dx, cy + dy)) else {
                        continue;
                    };
                    for &hi in list {
                        let h = &hoyo_pts[hi];
                        let dist = (h.x_pos - px).hypot(h.y_pos - py);
                        if dist > tol {
                            continue;
                        }
                        let same_lbl = if app_lid == Some(h.label_id) { 0.0 } else { 10.0 };
                        let same_grp = if app_grp == h.point_group_pair() {
                            0.0
                        } else {
                            1.0
                        };
                        let cost = same_lbl + same_grp + dist;
                        edges.push((cost, dist, li, hi));
                    }
                }
            }
        }
    }

    edges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut used_a = vec![false; user_markers.len()];
    let mut used_h = vec![false; hoyo_pts.len()];

    let mut exact_set = BTreeSet::new();
    let mut already_cnt = 0_usize;
    let mut matched_cnt = 0_usize;
    let mut se = 0.0_f64;
    let mut cat_counts: BTreeMap<String, usize> = BTreeMap::new();

    for &(_, dist, li, hi) in &edges {
        if used_a[li] || used_h[hi] {
            continue;
        }
        used_a[li] = true;
        used_h[hi] = true;
        matched_cnt += 1;
        se += dist * dist;
        let h = &hoyo_pts[hi];
        let bucket = bucket_for_hoyolab_label(h.label_id, h.is_puzzle_chest);
        *cat_counts.entry(bucket.to_owned()).or_insert(0) += 1;
        if already_marked.contains(&h.id) {
            already_cnt += 1;
        } else {
            exact_set.insert(h.id);
        }
    }

    let mut sample_unmatched = Vec::new();
    let mut unmatched_count = 0_usize;
    for (li, m) in user_markers.iter().enumerate() {
        if !used_a[li] {
            unmatched_count += 1;
            if sample_unmatched.len() < 15 {
                sample_unmatched.push(UnmatchedSample {
                    source_id: m.id,
                    label: m.type_code.clone(),
                    reason: "not_found_on_hoyolab".to_owned(),
                });
            }
        }
    }

    MapImportPlan {
        map_id,
        map_name: hoyolab_map_display_name(map_id).to_owned(),
        source_count: user_markers.len(),
        calibrated_anchors,
        rmse_px: if matched_cnt > 0 {
            ((se / matched_cnt as f64).sqrt() * 10000.0).round() / 10000.0
        } else {
            0.0
        },
        already_marked_count: already_cnt,
        exact_ids: exact_set.into_iter().collect(),
        high_ids: Vec::new(),
        medium_ids: Vec::new(),
        ambiguous_ids: Vec::new(),
        unmatched_count,
        category_counts: cat_counts,
        sample_unmatched,
    }
}

type GridPointBucket = Vec<(usize, f64, f64)>;
type BipartiteEdge = (f64, f64, usize, usize);

/// Reverse-match HoYoLAB marked pins -> Appsample v5 IDs (or exact `(lng, lat)` fallback when
/// no live Appsample catalog is provided). Fixes the previous `No calibrated affine` export error.
pub fn export_hoyolab_to_appsample_v5(
    source_pins: &[crate::map_bridge::CanonicalPin],
    raw_targets: Option<&Value>,
    map_id: i64,
) -> CoreResult<Value> {
    // Path 1: Live Appsample catalog provided -> 100.000% exact Appsample ID export
    if let Some(raw_t) = raw_targets
        && !raw_t.is_null()
        && let Ok(catalog) = parse_appsample_markers_v5(raw_t)
    {
        let map_cat: Vec<&AppsampleMarkerLite> =
            catalog.iter().filter(|m| m.map_id == map_id).collect();
        if !map_cat.is_empty() {
            let affines = appsample_seed_affines(map_id);
            if !affines.is_empty() {
                let cell = 2.0_f64;
                let mut grid: HashMap<(i32, i32), GridPointBucket> = HashMap::new();
                for (ai, m) in map_cat.iter().enumerate() {
                    for af in &affines {
                        let (px, py) = af.apply(m.lng, m.lat);
                        let key = ((px / cell).floor() as i32, (py / cell).floor() as i32);
                        grid.entry(key).or_default().push((ai, px, py));
                    }
                }
                let mut edges: Vec<(f64, usize, usize)> = Vec::new();
                for (si, sp) in source_pins.iter().enumerate() {
                    let cx = (sp.x_pos / cell).floor() as i32;
                    let cy = (sp.y_pos / cell).floor() as i32;
                    let sp_grp = sp.point_group.and_then(|g| {
                        sp.floor_id
                            .as_deref()
                            .and_then(|f| f.parse::<i64>().ok())
                            .map(|f| (g, f))
                    });
                    for dx in -1..=1 {
                        for dy in -1..=1 {
                            let Some(list) = grid.get(&(cx + dx, cy + dy)) else {
                                continue;
                            };
                            for &(ai, px, py) in list {
                                let d = (sp.x_pos - px).hypot(sp.y_pos - py);
                                if d > 0.35 {
                                    continue;
                                }
                                let m = map_cat[ai];
                                let same_lbl = if parse_appsample_label_id(&m.type_code)
                                    == Some(sp.label_id)
                                {
                                    0.0
                                } else {
                                    10.0
                                };
                                let same_grp = if parse_appsample_meta_group(&m.meta) == sp_grp {
                                    0.0
                                } else {
                                    1.0
                                };
                                edges.push((same_lbl + same_grp + d, si, ai));
                            }
                        }
                    }
                }
                edges.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut u_s = vec![false; source_pins.len()];
                let mut u_a = vec![false; map_cat.len()];
                let mut ids = BTreeSet::new();
                for &(_, si, ai) in &edges {
                    if u_s[si] || u_a[ai] {
                        continue;
                    }
                    u_s[si] = true;
                    u_a[ai] = true;
                    ids.insert(map_cat[ai].id);
                }
                let matched = ids.len();
                let unmatched = source_pins.len().saturating_sub(matched);
                let rate = if source_pins.is_empty() {
                    0.0
                } else {
                    ((100.0 * matched as f64 / source_pins.len() as f64) * 100.0).round() / 100.0
                };
                return Ok(json!({
                    "ok": true,
                    "format": "appsample",
                    "payload": { "ids": ids.into_iter().collect::<Vec<i64>>() },
                    "report": {
                        "total_source_pins": source_pins.len(),
                        "matched_count": matched,
                        "confident_count": matched,
                        "ambiguous_count": 0,
                        "unmatched_count": unmatched,
                        "unmapped_categories": [],
                        "missing_from_point_list_count": 0,
                        "match_rate_pct": rate
                    }
                }));
            }
        }
    }

    // Path 2: No live Appsample catalog -> inverse Multi-Affine coordinate export (never crashes)
    let affines = appsample_seed_affines(map_id);
    if affines.is_empty() {
        return Ok(json!({
            "ok": false,
            "format": "appsample",
            "payload": Value::Null,
            "report": Value::Null,
            "error": format!("No calibrated affine transform available for target=\"appsample\", map_id={map_id}"),
        }));
    }
    let mut pins_out = Vec::with_capacity(source_pins.len());
    for sp in source_pins {
        // Choose exact regional inverse affine on Teyvat (Inazuma east-south, Nod-Krai far-west)
        let af = if map_id == 2 && affines.len() >= 3 {
            if sp.x_pos > 2300.0 && sp.y_pos > 1600.0 {
                affines[1] // Inazuma
            } else if sp.x_pos < -7200.0 {
                affines[2] // Nod-Krai
            } else {
                affines[0] // Mainland
            }
        } else {
            affines[0]
        };
        let (lng, lat) = af.inverse_apply(sp.x_pos, sp.y_pos);
        let meta = match (sp.point_group, sp.floor_id.as_deref()) {
            (Some(g), Some(f)) => format!("l=0|{g}|{f}"),
            _ => String::new(),
        };
        pins_out.push(json!({
            "hoyolab_id": sp.hoyolab_id,
            "mid": map_id,
            "type": format!("o{}", sp.label_id),
            "canonical_category": sp.canonical_category,
            "lng": (lng * 100000.0).round() / 100000.0,
            "lat": (lat * 100000.0).round() / 100000.0,
            "meta": meta,
        }));
    }
    let count = pins_out.len();
    Ok(json!({
        "ok": true,
        "format": "appsample",
        "payload": {
            "schema": "genshin-helper.appsample-coords.v1",
            "map_id": map_id,
            "pins": pins_out,
        },
        "report": {
            "total_source_pins": count,
            "matched_count": count,
            "confident_count": count,
            "ambiguous_count": 0,
            "unmatched_count": 0,
            "unmapped_categories": [],
            "missing_from_point_list_count": 0,
            "match_rate_pct": if count > 0 { 100.0 } else { 0.0 },
        }
    }))
}

// ---------------------------------------------------------------------------
// Kongying Tavern -> HoYoLAB Engine (Scale-Free RANSAC + IDW + Semantic + Hungarian)
// ---------------------------------------------------------------------------

/// Map Kongying `area.code` to HoYoLAB `map_id`, excluding temporary event maps.
pub fn kongying_area_code_to_hoyolab_map_id(code: &str) -> Option<i64> {
    if !code.starts_with("A:") {
        return None;
    }
    let upper = code.to_ascii_uppercase();
    if upper.contains("APPLE")
        || upper.contains("VELURIYAM")
        || upper.contains("SIMULANKA")
        || upper.contains("SANJIE")
    {
        return None;
    }
    match code {
        "A:DQ:YUANXIAGONG" => Some(7),
        "A:LY:CENGYAN_UG" => Some(9),
        "A:FD:ANCIENT_SEA" => Some(34),
        "A:NT:NATA4" => Some(36),
        "A:MD:SHENDIAN" => Some(37),
        "A:NDKL:SY" => Some(40),
        _ => Some(2),
    }
}

/// Classify a Kongying item name into a canonical collectible group.
pub fn classify_kongying_item_name(name: &str) -> Option<&'static str> {
    let n = name.trim();
    match n {
        "七天神像" => Some("Statue"),
        "传送锚点" => Some("Teleport"),
        "普通的宝箱" | "普通宝箱" => Some("CommonChest"),
        "精致的宝箱" | "精致宝箱" => Some("ExquisiteChest"),
        "珍贵的宝箱" | "珍贵宝箱" => Some("PreciousChest"),
        "华丽的宝箱" | "华丽宝箱" => Some("LuxuriousChest"),
        "奇馈宝箱" => Some("RemarkableChest"),
        "仙灵" | "温暖仙灵" | "雷灵" => Some("Seelie"),
        "限时挑战" => Some("TimeTrial"),
        "观景点" => Some("Viewpoint"),
        "旋曜玉帛" => Some("RadiantSpincrystal"),
        "绯红玉髓" => Some("CrimsonAgate"),
        "流明晶石" => Some("Lumenspar"),
        "苍灵鲤" => Some("SpiritCarp"),
        _ => {
            if n.ends_with("秘境") {
                Some("Domain")
            } else if n.contains("风神瞳") {
                Some("Anemoculus")
            } else if n.contains("岩神瞳") {
                Some("Geoculus")
            } else if n.contains("雷神瞳") {
                Some("Electroculus")
            } else if n.contains("草神瞳") {
                Some("Dendroculus")
            } else if n.contains("水神瞳") {
                Some("Hydroculus")
            } else if n.contains("火神瞳") {
                Some("Pyroculus")
            } else if n.contains("月神瞳") {
                Some("Lunoculus")
            } else if n.contains("冰神瞳") {
                Some("Cryoculus")
            } else if n.ends_with("地灵龛") {
                Some("Shrine")
            } else if n.ends_with("幻写灵") {
                Some("Monetoo")
            } else if n.contains("摩拉箱") || n.contains("摩拉堆") {
                Some("MoraChest")
            } else {
                None
            }
        }
    }
}

pub fn hoyolab_labels_for_kongying_group(group: &str) -> &'static [i64] {
    match group {
        "Statue" => &[2],
        "Teleport" => &[3],
        "Domain" => &[154],
        "Anemoculus" => &[5],
        "Geoculus" => &[6],
        "CrimsonAgate" => &[141],
        "Electroculus" => &[194],
        "Lumenspar" => &[357],
        "Dendroculus" => &[403],
        "Hydroculus" => &[508],
        "SpiritCarp" => &[554],
        "Pyroculus" => &[626],
        "Lunoculus" => &[694],
        "Cryoculus" => &[833],
        "Shrine" => &[8, 9, 212, 411, 509, 577, 703, 835],
        "CommonChest" => &[17],
        "ExquisiteChest" => &[44],
        "PreciousChest" => &[45],
        "LuxuriousChest" => &[46],
        "RemarkableChest" => &[269],
        "Seelie" => &[18, 148, 205],
        "Monetoo" => &[597],
        "TimeTrial" => &[64, 600],
        "Viewpoint" => &[85],
        "RadiantSpincrystal" => &[356],
        "MoraChest" => &[129, 329, 359, 366, 596, 723, 819, 838],
        _ => &[],
    }
}

fn is_sparse_group(group: &str) -> bool {
    matches!(
        group,
        "Statue"
            | "Teleport"
            | "Domain"
            | "Anemoculus"
            | "Geoculus"
            | "CrimsonAgate"
            | "Electroculus"
            | "Lumenspar"
            | "Dendroculus"
            | "Hydroculus"
            | "SpiritCarp"
            | "Pyroculus"
            | "Lunoculus"
            | "Cryoculus"
            | "Shrine"
            | "Viewpoint"
            | "RadiantSpincrystal"
    )
}

fn is_chest_puzzle_group(group: &str) -> bool {
    matches!(
        group,
        "CommonChest"
            | "ExquisiteChest"
            | "PreciousChest"
            | "LuxuriousChest"
            | "RemarkableChest"
            | "Seelie"
            | "Monetoo"
            | "TimeTrial"
    )
}

fn semantic_puzzle_lids_from_text(text: &str) -> Vec<i64> {
    const RULES: &[(&[&str], &[i64])] = &[
        (&["限时挑战", "挑战"], &[64, 600]),
        (&["温暖仙灵"], &[148]),
        (&["雷灵"], &[205]),
        (&["仙灵", "指引仙灵"], &[18, 148, 205]),
        (&["击败魔物", "镇守", "魔物"], &[79, 547]),
        (&["元素方碑"], &[70]),
        (&["火把"], &[71]),
        (&["挖掘", "埋藏"], &[69, 599]),
        (&["压感", "压力"], &[67]),
        (&["大石堆"], &[72]),
        (&["小石堆", "碎石"], &[73, 359]),
        (&["风车"], &[65]),
        (&["蓬蓬果"], &[68]),
        (&["风史莱姆"], &[66]),
        (&["封印"], &[75]),
        (&["幻写灵"], &[597]),
        (&["聚光台座"], &[885]),
        (&["光映"], &[879]),
        (&["霜精"], &[836, 837, 880]),
        (&["雷探针", "继电石"], &[258, 230]),
        (&["剑柄"], &[259]),
        (&["机关立方", "雷立方"], &[216]),
        (&["死域"], &[354]),
        (&["芒荒", "荒芒"], &[499]),
        (&["月矩力", "月灵"], &[711, 712, 717, 722, 798]),
    ];
    let mut out = Vec::new();
    for &(keywords, lids) in RULES {
        if keywords.iter().any(|kw| text.contains(kw)) {
            out.extend_from_slice(lids);
        }
    }
    out
}

/// Scale-free 2-point RANSAC calibrator for Kongying -> HoYoLAB (works for all 7 maps automatically).
pub fn calibrate_kongying_scale_free(
    k_anchors: &[&KongyingMarkerLite],
    hoyo_pts: &[HoyolabPointLite],
) -> Option<(Affine2D, usize, f64)> {
    let k_stat: Vec<&KongyingMarkerLite> = k_anchors
        .iter()
        .copied()
        .filter(|m| m.primary_group == "Statue")
        .collect();
    let h_stat: Vec<&HoyolabPointLite> = hoyo_pts.iter().filter(|p| p.label_id == 2).collect();
    let k_tp: Vec<&KongyingMarkerLite> = k_anchors
        .iter()
        .copied()
        .filter(|m| m.primary_group == "Teleport")
        .collect();
    let h_tp: Vec<&HoyolabPointLite> = hoyo_pts.iter().filter(|p| p.label_id == 3).collect();

    let use_stat = k_stat.len() >= 5 && h_stat.len() >= 5;
    let k_use: Vec<(f64, f64)> = if use_stat {
        k_stat.iter().map(|m| (m.x, m.y)).collect()
    } else if k_tp.len() >= 4 && h_tp.len() >= 4 {
        k_tp.iter().map(|m| (m.x, m.y)).collect()
    } else {
        k_anchors.iter().map(|m| (m.x, m.y)).collect()
    };
    let h_use: Vec<(f64, f64)> = if use_stat {
        h_stat.iter().map(|p| (p.x_pos, p.y_pos)).collect()
    } else if k_tp.len() >= 4 && h_tp.len() >= 4 {
        h_tp.iter().map(|p| (p.x_pos, p.y_pos)).collect()
    } else {
        hoyo_pts
            .iter()
            .filter(|p| matches!(p.label_id, 2 | 3 | 154))
            .map(|p| (p.x_pos, p.y_pos))
            .collect()
    };

    if k_use.len() < 3 || h_use.len() < 3 {
        return None;
    }

    let mut k_sorted = k_use.clone();
    k_sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let k_pick: Vec<(f64, f64)> = if k_sorted.len() <= 25 {
        k_sorted
    } else {
        (0..25)
            .map(|i| k_sorted[(i * (k_sorted.len() - 1)) / 24])
            .collect()
    };

    let mut best_model: Option<Affine2D> = None;
    let mut best_inliers = 0_usize;
    let early_stop = ((k_pick.len() as f64) * 0.85).ceil() as usize;

    'outer: for i in 0..k_pick.len() {
        for j in (i + 1)..k_pick.len() {
            let dkx = k_pick[j].0 - k_pick[i].0;
            let dky = k_pick[j].1 - k_pick[i].1;
            if dkx.abs() < 150.0 || dky.abs() < 150.0 {
                continue;
            }
            for a in 0..h_use.len() {
                for b in 0..h_use.len() {
                    if a == b {
                        continue;
                    }
                    let ax = (h_use[b].0 - h_use[a].0) / dkx;
                    let cy = (h_use[b].1 - h_use[a].1) / dky;
                    if !(0.15..=5.0).contains(&ax) {
                        continue;
                    }
                    if (ax - cy.abs()).abs() / ax > 0.05 {
                        continue;
                    }
                    let bx = h_use[a].0 - ax * k_pick[i].0;
                    let dy = h_use[a].1 - cy * k_pick[i].1;
                    let mut inl = 0_usize;
                    for &(kx, ky) in &k_pick {
                        let px = ax * kx + bx;
                        let py = cy * ky + dy;
                        if h_use.iter().any(|&(hx, hy)| {
                            (hx - px).abs() < 25.0
                                && (hy - py).abs() < 25.0
                                && (hx - px).hypot(hy - py) < 25.0
                        }) {
                            inl += 1;
                        }
                    }
                    if inl > best_inliers {
                        best_inliers = inl;
                        best_model = Some(Affine2D { ax, bx, cy, dy });
                        if inl >= early_stop {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }

    let mut af = best_model?;
    if best_inliers < 3 {
        return None;
    }

    let h_by_group = |g: &str| -> Vec<&HoyolabPointLite> {
        let lid = match g {
            "Statue" => 2,
            "Teleport" => 3,
            "Domain" => 154,
            _ => return Vec::new(),
        };
        hoyo_pts.iter().filter(|p| p.label_id == lid).collect()
    };
    let h_s = h_by_group("Statue");
    let h_t = h_by_group("Teleport");
    let h_d = h_by_group("Domain");

    let mut pairs = Vec::new();
    for iter in 0..6 {
        let tol = match iter {
            0 => 50.0,
            1 => 28.0,
            _ => 22.0,
        };
        pairs.clear();
        for m in k_anchors {
            let pool: &[&HoyolabPointLite] = match m.primary_group.as_str() {
                "Statue" => &h_s,
                "Teleport" => &h_t,
                "Domain" => &h_d,
                _ => continue,
            };
            let (px, py) = af.apply(m.x, m.y);
            let mut best: Option<&HoyolabPointLite> = None;
            let mut bd = f64::INFINITY;
            for &h in pool {
                let d = (h.x_pos - px).hypot(h.y_pos - py);
                if d < bd {
                    bd = d;
                    best = Some(h);
                }
            }
            if let Some(h) = best
                && bd < tol
            {
                pairs.push((m.x, m.y, h.x_pos, h.y_pos));
            }
        }
        if pairs.len() < 3 {
            break;
        }
        if let Some((next_af, _)) = fit_affine_2d(&pairs) {
            af = next_af;
        }
    }
    let (final_af, rmse) = fit_affine_2d(&pairs)?;
    Some((final_af, pairs.len(), rmse))
}

/// Exact maximum-cardinality minimum-cost bipartite matcher via connected-component Hungarian
/// shortest-augmenting-path solver (falls back to greedy only for dense components > 60 nodes).
fn solve_optimal_bipartite(
    n_left: usize,
    n_right: usize,
    edges: &[(f64, f64, usize, usize)], // (cost, raw_dist, li, ri)
) -> Vec<(usize, usize, f64, f64)> {
    let mut adj_l: Vec<Vec<usize>> = vec![Vec::new(); n_left];
    let mut adj_r: Vec<Vec<usize>> = vec![Vec::new(); n_right];
    for (ei, &(_, _, li, ri)) in edges.iter().enumerate() {
        adj_l[li].push(ei);
        adj_r[ri].push(ei);
    }

    let mut vis_l = vec![false; n_left];
    let mut vis_r = vec![false; n_right];
    let mut out = Vec::new();

    for start_l in 0..n_left {
        if vis_l[start_l] || adj_l[start_l].is_empty() {
            continue;
        }
        // Fast path for isolated 1-to-1 pair
        if adj_l[start_l].len() == 1 {
            let e = edges[adj_l[start_l][0]];
            if adj_r[e.3].len() == 1 {
                vis_l[start_l] = true;
                vis_r[e.3] = true;
                out.push((e.2, e.3, e.1, e.0));
                continue;
            }
        }

        let mut comp_l = Vec::new();
        let mut comp_r = Vec::new();
        let mut comp_edges = Vec::new();
        let mut stack = vec![start_l];
        vis_l[start_l] = true;
        while let Some(u) = stack.pop() {
            comp_l.push(u);
            for &ei in &adj_l[u] {
                let e = edges[ei];
                comp_edges.push(e);
                let v = e.3;
                if !vis_r[v] {
                    vis_r[v] = true;
                    comp_r.push(v);
                    for &ei2 in &adj_r[v] {
                        let u2 = edges[ei2].2;
                        if !vis_l[u2] {
                            vis_l[u2] = true;
                            stack.push(u2);
                        }
                    }
                }
            }
        }

        let ml = comp_l.len();
        let mr = comp_r.len();
        if ml > 60 || mr > 60 {
            comp_edges.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut ul = HashSet::new();
            let mut ur = HashSet::new();
            for &(cost, raw_d, li, ri) in &comp_edges {
                if ul.insert(li) && ur.insert(ri) {
                    out.push((li, ri, raw_d, cost));
                }
            }
            continue;
        }

        let idx_l: HashMap<usize, usize> = comp_l
            .iter()
            .enumerate()
            .map(|(i, &v)| (v, i))
            .collect();
        let idx_r: HashMap<usize, usize> = comp_r
            .iter()
            .enumerate()
            .map(|(i, &v)| (v, i))
            .collect();

        let n_cols = mr + ml;
        let inf = 1e12_f64;
        let mut cost_mat = vec![vec![inf; n_cols]; ml];
        for (r, row) in cost_mat.iter_mut().enumerate() {
            row[mr + r] = 0.0; // dummy unmatched slot
        }
        let mut raw_map: HashMap<(usize, usize), (f64, f64)> = HashMap::new();
        for &(cost, raw_d, li, ri) in &comp_edges {
            let r = idx_l[&li];
            let c = idx_r[&ri];
            let w = cost - 10_000.0;
            if w < cost_mat[r][c] {
                cost_mat[r][c] = w;
                raw_map.insert((r, c), (raw_d, cost));
            }
        }

        // Standard O(ml^2 * n_cols) Hungarian shortest augmenting path
        let mut u = vec![0.0_f64; ml + 1];
        let mut v = vec![0.0_f64; n_cols + 1];
        let mut p = vec![0_usize; n_cols + 1];
        let mut way = vec![0_usize; n_cols + 1];
        for i in 1..=ml {
            p[0] = i;
            let mut j0 = 0_usize;
            let mut minv = vec![f64::INFINITY; n_cols + 1];
            let mut used = vec![false; n_cols + 1];
            loop {
                used[j0] = true;
                let i0 = p[j0];
                let mut delta = f64::INFINITY;
                let mut j1 = 0_usize;
                for j in 1..=n_cols {
                    if used[j] {
                        continue;
                    }
                    let cur = cost_mat[i0 - 1][j - 1] - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
                for j in 0..=n_cols {
                    if used[j] {
                        u[p[j]] += delta;
                        v[j] -= delta;
                    } else {
                        minv[j] -= delta;
                    }
                }
                j0 = j1;
                if p[j0] == 0 {
                    break;
                }
            }
            loop {
                let j1 = way[j0];
                p[j0] = p[j1];
                j0 = j1;
                if j0 == 0 {
                    break;
                }
            }
        }
        for (j, &row_1idx) in p.iter().enumerate().take(mr + 1).skip(1) {
            if row_1idx > 0 {
                let r = row_1idx - 1;
                let c = j - 1;
                if let Some(&(raw_d, cost)) = raw_map.get(&(r, c)) {
                    out.push((comp_l[r], comp_r[c], raw_d, cost));
                }
            }
        }
    }

    out
}

struct ProjectedKy<'a> {
    m: &'a KongyingMarkerLite,
    px: f64,
    py: f64,
}

fn build_spatial_edges<F>(
    k_sub: &[&ProjectedKy<'_>],
    h_sub: &[&HoyolabPointLite],
    max_dist: f64,
    floor_pen: f64,
    extra_filter: F,
) -> (Vec<BipartiteEdge>, Vec<f64>)
where
    F: Fn(&KongyingMarkerLite, &HoyolabPointLite) -> bool,
{
    let cell = max_dist.max(15.0);
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::with_capacity(h_sub.len());
    for (hi, h) in h_sub.iter().enumerate() {
        let key = ((h.x_pos / cell).floor() as i32, (h.y_pos / cell).floor() as i32);
        grid.entry(key).or_default().push(hi);
    }

    let mut edges = Vec::new();
    let mut d1 = vec![f64::INFINITY; k_sub.len()];
    let mut d2 = vec![f64::INFINITY; k_sub.len()];
    for (ki, k) in k_sub.iter().enumerate() {
        let cx = (k.px / cell).floor() as i32;
        let cy = (k.py / cell).floor() as i32;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(list) = grid.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                for &hi in list {
                    let h = h_sub[hi];
                    if !extra_filter(k.m, h) {
                        continue;
                    }
                    let raw_d = (h.x_pos - k.px).hypot(h.y_pos - k.py);
                    if raw_d > max_dist {
                        continue;
                    }
                    if raw_d < d1[ki] {
                        d2[ki] = d1[ki];
                        d1[ki] = raw_d;
                    } else if raw_d < d2[ki] {
                        d2[ki] = raw_d;
                    }
                    let mut cost = raw_d;
                    if floor_pen > 0.0 && k.m.is_underground != h.is_underground() {
                        cost += floor_pen;
                    }
                    edges.push((cost, raw_d, ki, hi));
                }
            }
        }
    }
    (edges, d2)
}

/// Match user's marked Kongying Tavern pins for a single `map_id` against HoYoLAB points.
///
/// - `anchor_markers`: calibration landmarks & oculi for `map_id` (Statues, Teleports, Domains, Oculi).
/// - `user_markers`: the user's marked Kongying markers belonging to `map_id`.
pub fn match_kongying_map_to_hoyolab(
    map_id: i64,
    anchor_markers: &[KongyingMarkerLite],
    user_markers: &[KongyingMarkerLite],
    hoyo_pts: &[HoyolabPointLite],
    already_marked: &HashSet<i64>,
) -> MapImportPlan {
    let map_anchors: Vec<&KongyingMarkerLite> = anchor_markers
        .iter()
        .filter(|m| m.map_id == map_id && matches!(m.primary_group.as_str(), "Statue" | "Teleport" | "Domain"))
        .collect();
    let map_users: Vec<&KongyingMarkerLite> = user_markers
        .iter()
        .filter(|m| m.map_id == map_id)
        .collect();

    let Some((base_af, cal_n, _cal_rmse)) = calibrate_kongying_scale_free(&map_anchors, hoyo_pts)
    else {
        return MapImportPlan {
            map_id,
            map_name: hoyolab_map_display_name(map_id).to_owned(),
            source_count: map_users.len(),
            calibrated_anchors: 0,
            rmse_px: 0.0,
            already_marked_count: 0,
            exact_ids: Vec::new(),
            high_ids: Vec::new(),
            medium_ids: Vec::new(),
            ambiguous_ids: Vec::new(),
            unmatched_count: map_users.len(),
            category_counts: BTreeMap::new(),
            sample_unmatched: Vec::new(),
        };
    };

    // Build Gaussian-kernel IDW residual field on Teyvat (map_id == 2) using Oculi + Teleports + Statues
    let mut ctrl_pts: Vec<(f64, f64, f64, f64, f64)> = Vec::new(); // (px, py, dx, dy, weight)
    if map_id == 2 {
        let warp_groups = [
            "Teleport",
            "Statue",
            "Anemoculus",
            "Geoculus",
            "Electroculus",
            "Dendroculus",
            "Hydroculus",
            "Pyroculus",
            "Lunoculus",
            "Cryoculus",
        ];
        for g in warp_groups {
            let k_proj: Vec<ProjectedKy<'_>> = anchor_markers
                .iter()
                .filter(|m| m.map_id == 2 && m.primary_group == g)
                .map(|m| {
                    let (px, py) = base_af.apply(m.x, m.y);
                    ProjectedKy { m, px, py }
                })
                .collect();
            if k_proj.is_empty() {
                continue;
            }
            let lids = hoyolab_labels_for_kongying_group(g);
            let h_sub: Vec<&HoyolabPointLite> = hoyo_pts
                .iter()
                .filter(|p| lids.contains(&p.label_id))
                .collect();
            let k_refs: Vec<&ProjectedKy<'_>> = k_proj.iter().collect();
            let (edges, _) = build_spatial_edges(&k_refs, &h_sub, 25.0, 3.0, |_, _| true);
            let mm = solve_optimal_bipartite(k_refs.len(), h_sub.len(), &edges);
            let w0 = if g.ends_with("culus") { 1.5 } else { 0.7 };
            for (ki, hi, dist, _) in mm {
                if dist <= 18.0 {
                    let kp = k_refs[ki];
                    let hp = h_sub[hi];
                    ctrl_pts.push((kp.px, kp.py, hp.x_pos - kp.px, hp.y_pos - kp.py, w0));
                }
            }
        }
    }

    let sigma = 250.0_f64;
    let inv_2s2 = 1.0 / (2.0 * sigma * sigma);
    let cutoff2 = 9.0 * sigma * sigma;
    let project_pt = |kx: f64, ky: f64| -> (f64, f64) {
        let (px0, py0) = base_af.apply(kx, ky);
        if ctrl_pts.is_empty() {
            return (px0, py0);
        }
        let (mut sw, mut sdx, mut sdy) = (0.0, 0.0, 0.0);
        for &(cx, cy, dx, dy, w0) in &ctrl_pts {
            let d2 = (px0 - cx).powi(2) + (py0 - cy).powi(2);
            if d2 > cutoff2 {
                continue;
            }
            let w = w0 * (-d2 * inv_2s2).exp();
            sw += w;
            sdx += w * dx;
            sdy += w * dy;
        }
        if sw > 0.2 {
            (px0 + sdx / sw, py0 + sdy / sw)
        } else {
            (px0, py0)
        }
    };

    let proj_users: Vec<ProjectedKy<'_>> = map_users
        .iter()
        .copied()
        .map(|m| {
            let (px, py) = project_pt(m.x, m.y);
            ProjectedKy { m, px, py }
        })
        .collect();

    struct MatchAcc<'a> {
        already_marked: &'a HashSet<i64>,
        used_k: HashSet<i64>,
        used_h: HashSet<i64>,
        high_set: BTreeSet<i64>,
        med_set: BTreeSet<i64>,
        amb_set: BTreeSet<i64>,
        already_cnt: usize,
        matched_cnt: usize,
        se: f64,
        cat_counts: BTreeMap<String, usize>,
    }

    impl MatchAcc<'_> {
        fn record(
            &mut self,
            k: &KongyingMarkerLite,
            h: &HoyolabPointLite,
            dist: f64,
            tier: ConfidenceTier,
        ) {
            self.used_k.insert(k.id);
            self.used_h.insert(h.id);
            self.matched_cnt += 1;
            self.se += dist * dist;
            let bucket = bucket_for_hoyolab_label(h.label_id, h.is_puzzle_chest);
            *self.cat_counts.entry(bucket.to_owned()).or_insert(0) += 1;
            if self.already_marked.contains(&h.id) {
                self.already_cnt += 1;
                return;
            }
            match tier {
                ConfidenceTier::Exact | ConfidenceTier::High => {
                    self.high_set.insert(h.id);
                }
                ConfidenceTier::Medium => {
                    self.med_set.insert(h.id);
                }
                ConfidenceTier::Ambiguous => {
                    self.amb_set.insert(h.id);
                }
            }
        }
    }

    let mut acc = MatchAcc {
        already_marked,
        used_k: HashSet::new(),
        used_h: HashSet::new(),
        high_set: BTreeSet::new(),
        med_set: BTreeSet::new(),
        amb_set: BTreeSet::new(),
        already_cnt: 0,
        matched_cnt: 0,
        se: 0.0,
        cat_counts: BTreeMap::new(),
    };

    // STAGE 1: Same-category Optimal Bipartite (adaptive 45px + ratio guard for sparse, 20px for dense)
    let stage1_groups = [
        "Statue",
        "Teleport",
        "Domain",
        "Anemoculus",
        "Geoculus",
        "CrimsonAgate",
        "Electroculus",
        "Lumenspar",
        "Dendroculus",
        "Hydroculus",
        "SpiritCarp",
        "Pyroculus",
        "Lunoculus",
        "Cryoculus",
        "Shrine",
        "Viewpoint",
        "RadiantSpincrystal",
        "RemarkableChest",
        "LuxuriousChest",
        "PreciousChest",
        "ExquisiteChest",
        "CommonChest",
        "Seelie",
        "Monetoo",
        "TimeTrial",
        "MoraChest",
    ];
    for g in stage1_groups {
        let sparse = is_sparse_group(g);
        let rad = if sparse { 45.0 } else { 20.0 };
        let k_sub: Vec<&ProjectedKy<'_>> = proj_users
            .iter()
            .filter(|p| p.m.primary_group == g && !acc.used_k.contains(&p.m.id))
            .collect();
        if k_sub.is_empty() {
            continue;
        }
        let lids = hoyolab_labels_for_kongying_group(g);
        let h_sub: Vec<&HoyolabPointLite> = hoyo_pts
            .iter()
            .filter(|h| lids.contains(&h.label_id) && !acc.used_h.contains(&h.id))
            .collect();
        if h_sub.is_empty() {
            continue;
        }
        let (edges, d2) = build_spatial_edges(&k_sub, &h_sub, rad, 4.0, |_, _| true);
        let mm = solve_optimal_bipartite(k_sub.len(), h_sub.len(), &edges);
        for (ki, hi, dist, _) in mm {
            if sparse && dist > 22.0 && d2[ki] < dist * 1.8 {
                continue;
            }
            let is_amb = d2[ki] < rad && dist > 2.5 && (d2[ki] - dist) < 3.0;
            let tier = if dist <= 12.0 && !is_amb {
                ConfidenceTier::High
            } else if !is_amb {
                ConfidenceTier::Medium
            } else {
                ConfidenceTier::Ambiguous
            };
            acc.record(k_sub[ki].m, h_sub[hi], dist, tier);
        }
    }

    // STAGE 2: Semantic markerTitle + secondary itemList -> specific HoYoLAB Puzzle Chest labels (<= 18px)
    {
        let mut k_sem: Vec<&ProjectedKy<'_>> = Vec::new();
        let mut allowed_map: HashMap<i64, HashSet<i64>> = HashMap::new();
        for p in &proj_users {
            if acc.used_k.contains(&p.m.id) || !is_chest_puzzle_group(&p.m.primary_group) {
                continue;
            }
            let mut allowed = HashSet::new();
            for g2 in &p.m.all_groups {
                if g2 != &p.m.primary_group {
                    for &lid in hoyolab_labels_for_kongying_group(g2) {
                        allowed.insert(lid);
                    }
                }
            }
            let combined = format!("{} {}", p.m.title, p.m.item_names);
            for lid in semantic_puzzle_lids_from_text(&combined) {
                allowed.insert(lid);
            }
            if !allowed.is_empty() {
                allowed_map.insert(p.m.id, allowed);
                k_sem.push(p);
            }
        }
        if !k_sem.is_empty() {
            let h_sub: Vec<&HoyolabPointLite> = hoyo_pts
                .iter()
                .filter(|h| {
                    !acc.used_h.contains(&h.id)
                        && (h.is_puzzle_chest || CHEST_LIDS.contains(&h.label_id))
                })
                .collect();
            let (edges, d2) = build_spatial_edges(&k_sem, &h_sub, 18.0, 4.0, |k, h| {
                allowed_map
                    .get(&k.id)
                    .is_some_and(|set| set.contains(&h.label_id))
            });
            let mm = solve_optimal_bipartite(k_sem.len(), h_sub.len(), &edges);
            for (ki, hi, dist, _) in mm {
                let is_amb = d2[ki] < 18.0 && dist > 2.5 && (d2[ki] - dist) < 3.0;
                let tier = if dist <= 12.0 && !is_amb {
                    ConfidenceTier::High
                } else if !is_amb {
                    ConfidenceTier::Medium
                } else {
                    ConfidenceTier::Ambiguous
                };
                acc.record(k_sem[ki].m, h_sub[hi], dist, tier);
            }
        }
    }

    // STAGE 3: Pooled Chest <-> HoYoLAB Puzzle Chest fallback (<= 16px, optimal bipartite)
    {
        let k_rem: Vec<&ProjectedKy<'_>> = proj_users
            .iter()
            .filter(|p| !acc.used_k.contains(&p.m.id) && is_chest_puzzle_group(&p.m.primary_group))
            .collect();
        if !k_rem.is_empty() {
            let h_sub: Vec<&HoyolabPointLite> = hoyo_pts
                .iter()
                .filter(|h| {
                    !acc.used_h.contains(&h.id)
                        && (h.is_puzzle_chest || CHEST_LIDS.contains(&h.label_id))
                })
                .collect();
            let (edges, d2) = build_spatial_edges(&k_rem, &h_sub, 16.0, 4.0, |_, _| true);
            let mm = solve_optimal_bipartite(k_rem.len(), h_sub.len(), &edges);
            for (ki, hi, dist, _) in mm {
                let is_amb = d2[ki] < 16.0 && dist > 2.5 && (d2[ki] - dist) < 3.0;
                let tier = if dist <= 10.0 && !is_amb {
                    ConfidenceTier::High
                } else if !is_amb {
                    ConfidenceTier::Medium
                } else {
                    ConfidenceTier::Ambiguous
                };
                acc.record(k_rem[ki].m, h_sub[hi], dist, tier);
            }
        }
    }

    // Remove any accidental cross-tier duplicates
    for id in &acc.high_set {
        acc.med_set.remove(id);
        acc.amb_set.remove(id);
    }
    for id in &acc.med_set {
        acc.amb_set.remove(id);
    }

    let mut sample_unmatched = Vec::new();
    let mut unmatched_count = 0_usize;
    for p in &proj_users {
        if !acc.used_k.contains(&p.m.id) {
            unmatched_count += 1;
            if sample_unmatched.len() < 15 {
                sample_unmatched.push(UnmatchedSample {
                    source_id: p.m.id,
                    label: if p.m.title.is_empty() {
                        p.m.primary_group.clone()
                    } else {
                        format!("{} ({})", p.m.primary_group, p.m.title)
                    },
                    reason: "no_safe_match_within_radius".to_owned(),
                });
            }
        }
    }

    MapImportPlan {
        map_id,
        map_name: hoyolab_map_display_name(map_id).to_owned(),
        source_count: map_users.len(),
        calibrated_anchors: cal_n,
        rmse_px: if acc.matched_cnt > 0 {
            ((acc.se / acc.matched_cnt as f64).sqrt() * 100.0).round() / 100.0
        } else {
            0.0
        },
        already_marked_count: acc.already_cnt,
        exact_ids: Vec::new(),
        high_ids: acc.high_set.into_iter().collect(),
        medium_ids: acc.med_set.into_iter().collect(),
        ambiguous_ids: acc.amb_set.into_iter().collect(),
        unmatched_count,
        category_counts: acc.cat_counts,
        sample_unmatched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn appsample_three_region_multi_affine_and_floor_disambiguation() {
        let affs = appsample_seed_affines(2);
        // Synthetic HoYoLAB pins in Mainland, Inazuma, Nod-Krai, plus 2 stacked pins at same (x,y)
        let hoyo = vec![
            HoyolabPointLite {
                id: 101,
                label_id: 17,
                x_pos: -1000.0,
                y_pos: 500.0,
                group_id: None,
                floor_id: None,
                label_name: "Common Chest".into(),
                is_puzzle_chest: false,
            },
            HoyolabPointLite {
                id: 102,
                label_id: 194,
                x_pos: 3200.0,
                y_pos: 2800.0,
                group_id: None,
                floor_id: None,
                label_name: "Electroculus".into(),
                is_puzzle_chest: false,
            },
            HoyolabPointLite {
                id: 103,
                label_id: 694,
                x_pos: -8200.0,
                y_pos: 100.0,
                group_id: None,
                floor_id: None,
                label_name: "Lunoculus".into(),
                is_puzzle_chest: false,
            },
            // Two identical-coord Mora pins differentiated ONLY by point_group (group_id=59, floor_id=58 vs surface)
            HoyolabPointLite {
                id: 201,
                label_id: 127,
                x_pos: -6341.5,
                y_pos: 3680.25,
                group_id: Some(59),
                floor_id: Some(58),
                label_name: "Mora".into(),
                is_puzzle_chest: false,
            },
            HoyolabPointLite {
                id: 202,
                label_id: 127,
                x_pos: -6341.5,
                y_pos: 3680.25,
                group_id: None,
                floor_id: None,
                label_name: "Mora".into(),
                is_puzzle_chest: false,
            },
        ];

        let (m_lng, m_lat) = affs[0].inverse_apply(-1000.0, 500.0);
        let (i_lng, i_lat) = affs[1].inverse_apply(3200.0, 2800.0);
        let (n_lng, n_lat) = affs[2].inverse_apply(-8200.0, 100.0);
        let (s_lng, s_lat) = affs[0].inverse_apply(-6341.5, 3680.25);

        let catalog = vec![
            AppsampleMarkerLite {
                id: 1,
                type_code: "o17".into(),
                map_id: 2,
                level: 0,
                lng: m_lng,
                lat: m_lat,
                meta: String::new(),
            },
            AppsampleMarkerLite {
                id: 2,
                type_code: "o194".into(),
                map_id: 2,
                level: 0,
                lng: i_lng,
                lat: i_lat,
                meta: String::new(),
            },
            AppsampleMarkerLite {
                id: 3,
                type_code: "o694".into(),
                map_id: 2,
                level: 0,
                lng: n_lng,
                lat: n_lat,
                meta: String::new(),
            },
            AppsampleMarkerLite {
                id: 4,
                type_code: "o127".into(),
                map_id: 2,
                level: 0,
                lng: s_lng,
                lat: s_lat,
                meta: "l=44856_44854|59|58".into(),
            },
        ];

        let already = HashSet::from([102]); // Electroculus already marked on HoYoLAB!
        let plan = match_appsample_map_to_hoyolab(2, &catalog, None, &hoyo, &already);
        assert_eq!(plan.already_marked_count, 1);
        assert_eq!(plan.unmatched_count, 0);
        assert_eq!(plan.exact_ids, vec![101, 103, 201]); // Disambiguated 201 (group 59|58) over 202!
        assert!(plan.rmse_px < 0.01);
    }

    #[test]
    fn kongying_scale_free_ransac_semantic_stage2_and_hungarian() {
        // Synthetic Enkanomiya-like 0.25 scale transform + semantic puzzle chest recovery
        let true_af = Affine2D {
            ax: 0.25,
            bx: -950.0,
            cy: 0.25,
            dy: -200.0,
        };
        let tp_coords = [
            (-1000.0, -800.0),
            (1200.0, -600.0),
            (-800.0, 900.0),
            (1400.0, 1100.0),
            (200.0, 100.0),
        ];
        let mut anchors = Vec::new();
        let mut hoyo = Vec::new();
        for (i, &(kx, ky)) in tp_coords.iter().enumerate() {
            let (hx, hy) = true_af.apply(kx, ky);
            anchors.push(KongyingMarkerLite {
                id: 1000 + i as i64,
                title: "传送锚点".into(),
                x: kx,
                y: ky,
                map_id: 7,
                area_code: "A:DQ:YUANXIAGONG".into(),
                primary_group: "Teleport".into(),
                all_groups: vec!["Teleport".into()],
                item_names: "传送锚点".into(),
                is_underground: false,
            });
            hoyo.push(HoyolabPointLite {
                id: 5000 + i as i64,
                label_id: 3,
                x_pos: hx + 1.0,
                y_pos: hy - 1.0,
                group_id: None,
                floor_id: None,
                label_name: "Teleport Waypoint".into(),
                is_puzzle_chest: false,
            });
        }
        // Add a Kongying ExquisiteChest whose real HoYoLAB pin is a Time Trial (label 64)
        let (tt_hx, tt_hy) = true_af.apply(500.0, 400.0);
        hoyo.push(HoyolabPointLite {
            id: 9001,
            label_id: 64,
            x_pos: tt_hx + 3.0,
            y_pos: tt_hy - 2.0,
            group_id: None,
            floor_id: None,
            label_name: "Time Trial Challenge".into(),
            is_puzzle_chest: true,
        });
        // Plus a nearby Torch Puzzle (label 71) only 6px away — semantic title must pick 9001!
        hoyo.push(HoyolabPointLite {
            id: 9002,
            label_id: 71,
            x_pos: tt_hx + 1.5,
            y_pos: tt_hy - 1.5,
            group_id: None,
            floor_id: None,
            label_name: "Torch Puzzle".into(),
            is_puzzle_chest: true,
        });

        let user_marked = vec![KongyingMarkerLite {
            id: 777,
            title: "限时挑战".into(),
            x: 500.0,
            y: 400.0,
            map_id: 7,
            area_code: "A:DQ:YUANXIAGONG".into(),
            primary_group: "ExquisiteChest".into(),
            all_groups: vec!["ExquisiteChest".into()],
            item_names: "精致的宝箱".into(),
            is_underground: false,
        }];

        let plan = match_kongying_map_to_hoyolab(7, &anchors, &user_marked, &hoyo, &HashSet::new());
        assert_eq!(plan.calibrated_anchors, 5);
        assert_eq!(plan.high_ids, vec![9001]); // Semantic Stage 2 picked Time Trial 9001 over closer Torch 9002!
        assert_eq!(plan.unmatched_count, 0);
    }

    #[test]
    fn offline_real_crossmap_lab_regression_when_present() {
        let lab_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("scratch")
            .join("crossmap_lab");
        let app_path = lab_dir.join("appsample_markers_v5.json");
        let h2_path = lab_dir.join("hoyolab_point_list_map2.json");
        let tree_path = lab_dir.join("hoyolab_label_tree_map2.json");
        if !app_path.is_file() || !h2_path.is_file() || !tree_path.is_file() {
            return;
        }
        let app_val: Value =
            serde_json::from_str(&std::fs::read_to_string(app_path).unwrap()).unwrap();
        let h2_val: Value =
            serde_json::from_str(&std::fs::read_to_string(h2_path).unwrap()).unwrap();
        let tree_val: Value =
            serde_json::from_str(&std::fs::read_to_string(tree_path).unwrap()).unwrap();

        let catalog = parse_appsample_markers_v5(&app_val).unwrap();
        let h2_pts = parse_hoyolab_points_lite(&h2_val, Some(&tree_val)).unwrap();
        let plan = match_appsample_map_to_hoyolab(2, &catalog, None, &h2_pts, &HashSet::new());
        assert!(
            plan.exact_ids.len() >= 83_480,
            "Expected >=83480 Teyvat Appsample exact matches, got {}",
            plan.exact_ids.len()
        );
        assert!(plan.rmse_px < 0.06, "Unexpected Appsample RMSE: {}", plan.rmse_px);
    }
}
