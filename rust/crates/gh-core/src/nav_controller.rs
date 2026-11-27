//! Navigation Sticky Target Controller & 1-second Collected Hold Guard (`src/nav_controller.py`).
//!
//! Ported from `src/nav_controller.py` plus pure-data normalization helpers from `main.py`.

use crate::route_doc::{RouteDocument, RouteStopKind};
use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

pub const ARRIVED_DISTANCE_M: f64 = 12.0;
pub const SPOT_CARD_DISTANCE_M: f64 = 20.0;
pub const PROXIMITY_TAKEOVER_M: f64 = 12.0;
pub const PROXIMITY_TAKEOVER_MARGIN_M: f64 = 8.0;
pub const CLOSER_RATIO: f64 = 0.75;
pub const CLOSER_MARGIN_M: f64 = 10.0;
pub const CLOSER_DWELL_S: f64 = 0.5;
pub const TELEPORT_JUMP_M: f64 = 50.0;
pub const LOCK_ON_ANGLE_DEG: f64 = 12.0;
pub const BEHIND_ANGLE_DEG: f64 = 135.0;
pub const NAV_MARK_CONFIRM_DIST_M: f64 = 25.0;

pub const HEADING_BONUS_MAX_M: f64 = 8.0;
pub const HEADING_ALIGN_FULL_DEG: f64 = 35.0;
pub const HEADING_ALIGN_ZERO_DEG: f64 = 105.0;

pub const DEFAULT_PROTECT_SECONDS: f64 = 8.0;
pub const ROLLBACK_PROTECT_SECONDS: f64 = 5.0;
pub const DEFAULT_HOLD_DURATION_SECONDS: f64 = 1.0;

pub const TAKEOVER_TARGET_AHEAD_DEG: f64 = 75.0;
pub const TAKEOVER_CAND_BEHIND_DEG: f64 = 110.0;
pub const COVERAGE_INSIDE_FACTOR: f64 = 0.97;

pub const GENERIC_PIN_NAMES: &[&str] = &[
    "map pin",
    "điểm đánh dấu",
    "pin",
    "mini-hud chỉ đường",
];

pub const TIER6_OCULUS_LABEL_IDS: &[i64] = &[
    5, 6, 194, 403, 508, 626, 694, 833, // Oculi (live)
    141, 357, 470, 554, 834, 791, 271, // Agate, Lumenspar, Plume, Carp, Wing, Memory, Voucher
    147, 210, 345, 395, 559, 575, // Legacy alias IDs
];

pub const TIER5_HIGH_VALUE_LABEL_IDS: &[i64] = &[
    8, 9, 212, 411, 509, 577, 703, 835, // Shrine of Depths
    806, // Kuuhenki Shrine
];

pub const TIER4_CHEST_LABEL_IDS: &[i64] = &[44, 45, 269, 236, 838, 879, 69, 75];

pub const TIER2_PUZZLE_LABEL_IDS: &[i64] = &[18, 148, 205, 64, 70, 71, 77, 547, 600];

pub const TIER6_KEYWORDS: &[&str] = &[
    "thần đồng",
    "oculus",
    "神瞳",
    "mã não",
    "crimson agate",
    "绯红玉髓",
    "lưu minh",
    "lumenspar",
    "流明晶石",
    "thương linh",
    "cá chép",
    "spirit carp",
    "苍灵鲤",
    "tịnh quang vũ",
    "净光翎",
    "giọt cánh",
    "khối ký ức",
    "phiếu quà",
];

pub const TIER5_KEYWORDS: &[&str] = &[
    "đền địa linh",
    "hòm địa linh",
    "shrine of depths",
    "地灵龛",
    "đền kuuhenki",
    "rương hiếm",
    "rương hoa lệ",
    "luxurious chest",
    "华丽宝箱",
];

pub const TIER4_KEYWORDS: &[&str] = &[
    "rương siêu cấp",
    "rương quý",
    "precious chest",
    "珍贵宝箱",
    "rương cao cấp",
    "rương tinh xảo",
    "exquisite chest",
    "精致宝箱",
    "rương kỳ dị",
    "remarkable chest",
    "奇馈宝箱",
    "rương snezhnaya",
    "rương báu",
];

pub const TIER3_KEYWORDS: &[&str] = &["rương thường", "common chest", "普通宝箱"];

pub const TIER2_KEYWORDS: &[&str] = &[
    "tiên linh",
    "seelie",
    "仙灵",
    "雷灵",
    "幻写灵",
    "khiêu chiến",
    "time trial",
    "限时挑战",
    "câu đố",
    "puzzle",
    "cơ quan",
    "huyền thoại",
    "bia nguyên tố",
    "challenge",
];

pub const TIER1_KEYWORDS: &[&str] = &[
    "ngọc thạch",
    "spincrystal",
    "旋曜玉帛",
    "ấn khóa",
    "ốc biển",
    "điểm ngắm cảnh",
    "viewpoint",
    "观景点",
    "đặc sản",
];

pub fn tier_bonus_m(tier: i32) -> f64 {
    match tier {
        6 => 8.0,
        5 => 6.0,
        4 => 4.5,
        3 => 3.5,
        2 => 2.0,
        1 => 1.0,
        _ => 0.0,
    }
}

pub fn is_generic_pin_name(name: &str) -> bool {
    let low = name.trim().to_lowercase();
    if low.is_empty() {
        return true;
    }
    if GENERIC_PIN_NAMES.contains(&low.as_str()) {
        return true;
    }
    if let Some(rest) = low.strip_prefix("pin ") {
        let stripped: String = rest
            .chars()
            .filter(|c| !matches!(c, ',' | '.' | '-'))
            .collect();
        return !stripped.is_empty() && stripped.chars().all(|c| c.is_numeric());
    }
    false
}

pub fn norm_floor_id(floor_id: Option<&str>) -> Option<String> {
    floor_id.and_then(|raw| {
        let cleaned = raw.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_owned())
        }
    })
}

fn value_to_trimmed_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_owned(),
        _ => v.to_string(),
    }
}

fn opt_i64(val: Option<&Value>) -> Option<i64> {
    let v = val?;
    match v {
        Value::Null | Value::Bool(_) => None,
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn opt_f64(val: Option<&Value>) -> Option<f64> {
    let v = val?;
    match v {
        Value::Null | Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpaceKey {
    pub map_id: i32,
    pub floor_tag: String,
}

impl SpaceKey {
    pub fn new(map_id: i32, floor_id: Option<&str>, is_underground: bool) -> Self {
        let floor_tag = match norm_floor_id(floor_id) {
            Some(fid) => {
                if is_underground {
                    format!("ug:{fid}")
                } else {
                    format!("sf:{fid}")
                }
            }
            None => {
                if is_underground {
                    "ug".to_owned()
                } else {
                    "surface".to_owned()
                }
            }
        };
        Self { map_id, floor_tag }
    }

    pub fn parse(&self) -> (i32, Option<&str>, bool) {
        if let Some(rest) = self.floor_tag.strip_prefix("ug:") {
            let trimmed = rest.trim();
            return (
                self.map_id,
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                },
                true,
            );
        }
        if self.floor_tag == "ug" {
            return (self.map_id, None, true);
        }
        if let Some(rest) = self.floor_tag.strip_prefix("sf:") {
            let trimmed = rest.trim();
            return (
                self.map_id,
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                },
                false,
            );
        }
        (self.map_id, None, false)
    }
}

pub fn is_floor_mismatch(
    target: &NavPin,
    player_floor_id: Option<&str>,
    player_is_underground: bool,
) -> bool {
    if target.is_underground != player_is_underground {
        return true;
    }
    let t_floor = norm_floor_id(target.floor_id.as_deref());
    let p_floor = norm_floor_id(player_floor_id);
    if let (Some(tf), Some(pf)) = (t_floor, p_floor)
        && tf != pf
    {
        return true;
    }
    false
}

pub fn is_same_floor_for_space(pin: &NavPin, space_key: &SpaceKey) -> bool {
    let (p_map, p_fid, p_ug) = space_key.parse();
    if pin.map_id != p_map {
        return false;
    }
    !is_floor_mismatch(pin, p_fid, p_ug)
}

/// Computes `(distance, bearing_cw_north_deg)` in HoYoLAB world CRS.
///
/// HoYoLAB world CRS:
/// - `lng` (`world_x`) increases East (Right)
/// - `lat` (`world_y`) increases South (Down); `-lat` is North (Up)
///
/// Bearing convention: `0° = North, 90° = East, 180° = South, 270° = West`.
pub fn compute_hoyolab_bearing(
    player_lat: f64,
    player_lng: f64,
    target_lat: f64,
    target_lng: f64,
) -> (f64, f64) {
    let dx = target_lng - player_lng;
    let dy = target_lat - player_lat;
    let dist = dx.hypot(dy);
    if !dist.is_finite() || dist <= 1e-9 {
        return (0.0, 0.0);
    }
    let mut bearing_deg = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
    if bearing_deg >= 360.0 || !bearing_deg.is_finite() {
        bearing_deg = 0.0;
    }
    (dist, bearing_deg)
}

/// Returns signed relative angle `delta_deg` in `[-180.0, 180.0]`:
/// `0°` = straight ahead, `+90°` = right (CW), `-90°` = left (CCW), `±180°` = behind.
pub fn normalize_angle_delta(bearing_deg: f64, view_heading_deg: f64) -> f64 {
    let diff = bearing_deg - view_heading_deg;
    let mut delta = (diff + 180.0).rem_euclid(360.0) - 180.0;
    if delta == -180.0 && diff.rem_euclid(360.0) == 180.0 {
        delta = 180.0;
    }
    delta
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NavPin {
    pub id: String,
    pub map_id: i32,
    pub lat: f64,
    pub lng: f64,
    pub name: String,
    pub category_id: Option<i64>,
    pub floor_id: Option<String>,
    pub floor_name: Option<String>,
    pub icon_url: Option<String>,
    pub priority: i32,
    #[serde(default, deserialize_with = "crate::deserialize_null_default")]
    pub is_underground: bool,
    pub category: String,
    pub image_url: String,
    pub description: String,
    pub cave_entrance: Option<Value>,
    pub label_id: Option<i64>,
    pub parent_label_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_section_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_section_total: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_section_name: Option<String>,
}

impl Default for NavPin {
    fn default() -> Self {
        Self {
            id: String::new(),
            map_id: 2,
            lat: 0.0,
            lng: 0.0,
            name: String::new(),
            category_id: None,
            floor_id: None,
            floor_name: None,
            icon_url: None,
            priority: 0,
            is_underground: false,
            category: String::new(),
            image_url: String::new(),
            description: String::new(),
            cave_entrance: None,
            label_id: None,
            parent_label_id: None,
            stop_kind: None,
            route_section_index: None,
            route_section_total: None,
            route_section_name: None,
        }
    }
}

impl NavPin {
    pub fn new(id: impl Into<String>, name: impl Into<String>, lat: f64, lng: f64) -> Self {
        let mut pin = Self {
            id: id.into(),
            name: name.into(),
            lat,
            lng,
            ..Default::default()
        };
        pin.normalize();
        pin
    }

    pub fn normalize(&mut self) {
        self.image_url = self.image_url.trim().to_owned();
        self.description = self.description.trim().to_owned();
        self.floor_id = norm_floor_id(self.floor_id.as_deref());
    }

    pub fn space_key(&self) -> SpaceKey {
        SpaceKey::new(self.map_id, self.floor_id.as_deref(), self.is_underground)
    }

    pub fn has_spot_content(&self) -> bool {
        !self.image_url.is_empty() || !self.description.is_empty()
    }

    pub fn floor_name_str(&self) -> &str {
        self.floor_name.as_deref().unwrap_or("")
    }

    /// Human label for HUD: explicit non-generic pin name, else HoYoLAB category name.
    pub fn display_name(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() && !is_generic_pin_name(name) {
            return name.to_owned();
        }
        let cat = self.category.trim();
        if !cat.is_empty() && !cat.chars().all(|c| c.is_numeric()) {
            return cat.to_owned();
        }
        name.to_owned()
    }

    pub fn from_value(data: &Value) -> Option<Self> {
        let obj = data.as_object()?;
        let lat = opt_f64(obj.get("lat").or_else(|| obj.get("y_pos"))).unwrap_or(0.0);
        let lng = opt_f64(obj.get("lng").or_else(|| obj.get("x_pos"))).unwrap_or(0.0);
        let id = obj
            .get("id")
            .map(value_to_trimmed_string)
            .unwrap_or_default();
        let name = obj
            .get("name")
            .map(value_to_trimmed_string)
            .unwrap_or_default();
        let map_id = opt_i64(obj.get("map_id")).unwrap_or(2) as i32;
        let floor_id_raw = obj.get("floor_id").map(value_to_trimmed_string);
        let floor_id = norm_floor_id(floor_id_raw.as_deref());
        let floor_name = obj
            .get("floor_name")
            .map(value_to_trimmed_string)
            .filter(|s| !s.is_empty());
        let is_underground = obj
            .get("is_underground")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let category = obj
            .get("category")
            .map(value_to_trimmed_string)
            .unwrap_or_default();
        let image_url = obj
            .get("image_url")
            .map(value_to_trimmed_string)
            .unwrap_or_default();
        let description = obj
            .get("description")
            .map(value_to_trimmed_string)
            .unwrap_or_default();
        let cave_entrance = obj
            .get("cave_entrance")
            .and_then(|v| v.as_object().cloned().map(Value::Object));
        let label_id = opt_i64(obj.get("label_id").or_else(|| obj.get("labelId")));
        let parent_label_id = opt_i64(
            obj.get("parent_label_id")
                .or_else(|| obj.get("parentLabelId")),
        );
        let category_id = opt_i64(obj.get("category_id"));
        let icon_url = obj
            .get("icon_url")
            .and_then(|v| v.as_str().map(str::to_owned));
        let priority = opt_i64(obj.get("priority")).unwrap_or(0) as i32;
        let stop_kind = obj
            .get("stop_kind")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let route_section_index = opt_i64(obj.get("route_section_index")).and_then(|v| usize::try_from(v).ok());
        let route_section_total = opt_i64(obj.get("route_section_total")).and_then(|v| usize::try_from(v).ok());
        let route_section_name = obj
            .get("route_section_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let mut pin = Self {
            id,
            map_id,
            lat,
            lng,
            name,
            category_id,
            floor_id,
            floor_name,
            icon_url,
            priority,
            is_underground,
            category,
            image_url,
            description,
            cave_entrance,
            label_id,
            parent_label_id,
            stop_kind,
            route_section_index,
            route_section_total,
            route_section_name,
        };
        pin.normalize();
        Some(pin)
    }

    /// Wire-compatible 14-key JSON object matching `NavPin.to_dict()` in Python.
    pub fn to_wire_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".to_owned(), json!(self.id));
        map.insert("name".to_owned(), json!(self.name));
        map.insert("lat".to_owned(), json!(self.lat));
        map.insert("lng".to_owned(), json!(self.lng));
        map.insert("map_id".to_owned(), json!(self.map_id));
        map.insert("floor_id".to_owned(), json!(self.floor_id));
        map.insert("floor_name".to_owned(), json!(self.floor_name_str()));
        map.insert("is_underground".to_owned(), json!(self.is_underground));
        map.insert("category".to_owned(), json!(self.category));
        map.insert("image_url".to_owned(), json!(self.image_url));
        map.insert("description".to_owned(), json!(self.description));
        let cave = match &self.cave_entrance {
            Some(Value::Object(obj)) => Value::Object(obj.clone()),
            _ => Value::Null,
        };
        map.insert("cave_entrance".to_owned(), cave);
        map.insert("label_id".to_owned(), json!(self.label_id));
        map.insert("parent_label_id".to_owned(), json!(self.parent_label_id));
        Value::Object(map)
    }
}

pub trait FloorResolver {
    fn infer_unique_underground_floor(
        &self,
        map_id: i32,
        lat: f64,
        lng: f64,
    ) -> Option<(String, String)>;

    fn resolve_floor_label(&self, map_id: i32, floor_id: &str) -> Option<String>;
}

/// Normalizes a raw extension/web-map pin payload into `NavPin` (`main.py::_normalize_raw_nav_pin`).
pub fn normalize_raw_nav_pin(
    raw: &Value,
    default_map_id: i32,
    floor_resolver: Option<&dyn FloorResolver>,
) -> Option<NavPin> {
    let obj = raw.as_object()?;
    let lat_val = opt_f64(obj.get("lat").or_else(|| obj.get("y_pos")))?;
    let lng_val = opt_f64(obj.get("lng").or_else(|| obj.get("x_pos")))?;
    if !lat_val.is_finite() || !lng_val.is_finite() {
        return None;
    }

    let raw_id = obj
        .get("id")
        .or_else(|| obj.get("point_id"))
        .map(value_to_trimmed_string)
        .unwrap_or_default();
    let pin_id = if raw_id.trim().is_empty() {
        format!("{lat_val:.2},{lng_val:.2}")
    } else {
        raw_id
    };

    let floor_id_raw = obj
        .get("floor_id")
        .or_else(|| obj.get("floorId"))
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                Some(value_to_trimmed_string(v))
            }
        });
    let mut floor_name = obj
        .get("floor_name")
        .or_else(|| obj.get("floorName"))
        .map(|v| {
            if v.is_null() {
                String::new()
            } else {
                value_to_trimmed_string(v).trim().to_owned()
            }
        })
        .unwrap_or_default();

    let is_ug = if let Some(v) = obj.get("is_underground").filter(|v| !v.is_null()) {
        match v {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            Value::String(s) => !s.is_empty(),
            _ => true,
        }
    } else if let Some(v) = obj.get("isUnderground").filter(|v| !v.is_null()) {
        match v {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            Value::String(s) => !s.is_empty(),
            _ => true,
        }
    } else {
        let fid_ug = floor_id_raw
            .as_deref()
            .is_some_and(|s| !matches!(s.trim(), "" | "0"));
        let fname_ug = !floor_name.is_empty()
            && !matches!(
                floor_name.to_ascii_uppercase().as_str(),
                "G" | "SURFACE" | "GROUND"
            );
        fid_ug || fname_ug
    };

    let map_id = opt_i64(obj.get("map_id").or_else(|| obj.get("mapId")))
        .map(|m| m as i32)
        .unwrap_or(default_map_id);

    let mut floor_id_str = floor_id_raw.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed == "0" {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });

    if is_ug && let Some(resolver) = floor_resolver {
        if floor_id_str.is_none()
            && let Some((fid, lbl)) = resolver.infer_unique_underground_floor(map_id, lat_val, lng_val)
        {
            floor_id_str = Some(fid);
            if floor_name.is_empty() || floor_name.chars().all(|c| c.is_numeric()) {
                floor_name = lbl;
            }
        }
        if let Some(ref fid) = floor_id_str
            && (floor_name.is_empty() || floor_name.chars().all(|c| c.is_numeric()))
            && let Some(resolved_lbl) = resolver.resolve_floor_label(map_id, fid)
            && !resolved_lbl.is_empty()
        {
            floor_name = resolved_lbl;
        }
    }

    let name = obj
        .get("name")
        .filter(|v| !v.is_null())
        .map(value_to_trimmed_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Pin {pin_id}"));

    let category = obj
        .get("category")
        .or_else(|| obj.get("label_id"))
        .or_else(|| obj.get("labelId"))
        .filter(|v| !v.is_null())
        .map(value_to_trimmed_string)
        .unwrap_or_default();

    let image_url = obj
        .get("image_url")
        .or_else(|| obj.get("imageUrl"))
        .filter(|v| !v.is_null())
        .map(value_to_trimmed_string)
        .unwrap_or_default();

    let description = obj
        .get("description")
        .or_else(|| obj.get("content"))
        .filter(|v| !v.is_null())
        .map(value_to_trimmed_string)
        .unwrap_or_default();

    let cave_entrance = obj
        .get("cave_entrance")
        .or_else(|| obj.get("caveEntrance"))
        .and_then(|v| v.as_object().cloned().map(Value::Object));

    let label_id = opt_i64(obj.get("label_id").or_else(|| obj.get("labelId")));
    let parent_label_id = opt_i64(
        obj.get("parent_label_id")
            .or_else(|| obj.get("parentLabelId")),
    );

    let mut pin = NavPin {
        id: pin_id,
        map_id,
        lat: lat_val,
        lng: lng_val,
        name,
        category_id: opt_i64(obj.get("category_id")),
        floor_id: floor_id_str,
        floor_name: if floor_name.is_empty() {
            None
        } else {
            Some(floor_name)
        },
        icon_url: obj
            .get("icon_url")
            .and_then(|v| v.as_str().map(str::to_owned)),
        priority: opt_i64(obj.get("priority")).unwrap_or(0) as i32,
        is_underground: is_ug,
        category,
        image_url,
        description,
        cave_entrance,
        label_id,
        parent_label_id,
        ..Default::default()
    };
    pin.normalize();
    Some(pin)
}

pub fn pin_priority_tier(pin: &NavPin) -> i32 {
    let lid = pin.label_id;
    let parent_lid = pin.parent_label_id;
    let text = format!("{} {}", pin.category, pin.name).to_lowercase();

    if lid.is_some_and(|id| TIER6_OCULUS_LABEL_IDS.contains(&id))
        || TIER6_KEYWORDS.iter().any(|kw| text.contains(kw))
    {
        return 6;
    }
    if lid.is_some_and(|id| TIER5_HIGH_VALUE_LABEL_IDS.contains(&id))
        || (lid == Some(46) && matches!(parent_lid, None | Some(13)))
        || TIER5_KEYWORDS.iter().any(|kw| text.contains(kw))
    {
        return 5;
    }
    if lid.is_some_and(|id| TIER4_CHEST_LABEL_IDS.contains(&id))
        || TIER4_KEYWORDS.iter().any(|kw| text.contains(kw))
    {
        return 4;
    }
    if parent_lid == Some(13)
        || (lid == Some(17)
            && parent_lid.is_none()
            && !TIER2_KEYWORDS.iter().any(|kw| text.contains(kw)))
        || TIER3_KEYWORDS.iter().any(|kw| text.contains(kw))
        || ((text.contains("rương") || text.contains("chest") || text.contains("宝箱"))
            && !text.contains("rương giải đố"))
    {
        return 3;
    }
    if parent_lid == Some(186)
        || lid.is_some_and(|id| TIER2_PUZZLE_LABEL_IDS.contains(&id))
        || TIER2_KEYWORDS.iter().any(|kw| text.contains(kw))
    {
        return 2;
    }
    if matches!(parent_lid, Some(4 | 10 | 51))
        || TIER1_KEYWORDS.iter().any(|kw| text.contains(kw))
    {
        return 1;
    }
    0
}

pub fn camera_alignment_score(bearing_deg: f64, view_heading_deg: Option<f64>) -> f64 {
    let Some(heading) = view_heading_deg else {
        return 0.0;
    };
    if !heading.is_finite() || !bearing_deg.is_finite() {
        return 0.0;
    }
    let abs_delta = normalize_angle_delta(bearing_deg, heading).abs();
    if abs_delta <= HEADING_ALIGN_FULL_DEG {
        return 1.0;
    }
    if abs_delta >= HEADING_ALIGN_ZERO_DEG {
        return 0.0;
    }
    let frac = (abs_delta - HEADING_ALIGN_FULL_DEG)
        / (HEADING_ALIGN_ZERO_DEG - HEADING_ALIGN_FULL_DEG);
    0.5 * (1.0 + (PI * frac).cos())
}

pub fn pin_effective_distance(
    pin: &NavPin,
    dist_m: f64,
    bearing_deg: Option<f64>,
    view_heading_deg: Option<f64>,
) -> f64 {
    let heading_bonus = match (bearing_deg, view_heading_deg) {
        (Some(b), Some(h)) => {
            let align = camera_alignment_score(b, Some(h));
            (HEADING_BONUS_MAX_M * align).min(dist_m * 0.22)
        }
        _ => 0.0,
    };
    let tier_bonus = tier_bonus_m(pin_priority_tier(pin)).min(dist_m * 0.25);
    dist_m - heading_bonus - tier_bonus
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinRankKey {
    pub eff_dist: f64,
    pub raw_dist: f64,
    pub neg_tier: i32,
    pub id: String,
}

impl Eq for PinRankKey {}

impl Ord for PinRankKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.eff_dist
            .total_cmp(&other.eff_dist)
            .then_with(|| self.raw_dist.total_cmp(&other.raw_dist))
            .then_with(|| self.neg_tier.cmp(&other.neg_tier))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for PinRankKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub fn pin_rank_key(
    pin: &NavPin,
    player_lat: f64,
    player_lng: f64,
    view_heading_deg: Option<f64>,
) -> PinRankKey {
    let (dist, bearing) = compute_hoyolab_bearing(player_lat, player_lng, pin.lat, pin.lng);
    PinRankKey {
        eff_dist: pin_effective_distance(pin, dist, Some(bearing), view_heading_deg),
        raw_dist: dist,
        neg_tier: -pin_priority_tier(pin),
        id: pin.id.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavState {
    Tracking,
    Arrived,
    FloorMismatch,
    Lost,
    #[default]
    Idle,
}

impl NavState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracking => "tracking",
            Self::Arrived => "arrived",
            Self::FloorMismatch => "floor_mismatch",
            Self::Lost => "lost",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavRouteMode {
    Direct,
    CaveEntrance,
    Waypoint,
    #[default]
    None,
}

impl NavRouteMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::CaveEntrance => "cave_entrance",
            Self::Waypoint => "waypoint",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NavGuidance {
    pub target: Option<NavPin>,
    pub distance_m: f64,
    pub relative_bearing_deg: f64,
    pub floor_mismatch: bool,
    pub is_route_mode: bool,
    pub remaining_count: usize,
    pub state: NavState,
    pub target_distance_m: Option<f64>,
    pub bearing_deg: Option<f64>,
    pub delta_deg: Option<f64>,
    pub player_floor_id: Option<String>,
    pub player_is_underground: bool,
    pub target_floor_id: Option<String>,
    pub target_floor_name: String,
    pub target_is_underground: bool,
    pub route_mode: NavRouteMode,
    pub route_target_name: String,
    pub route_target_lat: Option<f64>,
    pub route_target_lng: Option<f64>,
    pub show_spot_card: bool,
    pub is_locked_on: bool,
    pub is_behind: bool,
    pub route_step: Option<usize>,
    pub route_total: Option<usize>,
    pub player_floor_name: String,
    #[serde(default)]
    pub route_target_map_id: Option<i32>,
    #[serde(default)]
    pub route_section_label: String,
    #[serde(default)]
    pub route_stop_kind: String,
    #[serde(default)]
    pub is_single_pin_locked: bool,
}

impl Default for NavGuidance {
    fn default() -> Self {
        Self {
            target: None,
            distance_m: 0.0,
            relative_bearing_deg: 0.0,
            floor_mismatch: false,
            is_route_mode: false,
            remaining_count: 0,
            state: NavState::Idle,
            target_distance_m: None,
            bearing_deg: None,
            delta_deg: None,
            player_floor_id: None,
            player_is_underground: false,
            target_floor_id: None,
            target_floor_name: String::new(),
            target_is_underground: false,
            route_mode: NavRouteMode::None,
            route_target_name: String::new(),
            route_target_lat: None,
            route_target_lng: None,
            show_spot_card: false,
            is_locked_on: false,
            is_behind: false,
            route_step: None,
            route_total: None,
            player_floor_name: String::new(),
            route_target_map_id: None,
            route_section_label: String::new(),
            route_stop_kind: String::new(),
            is_single_pin_locked: false,
        }
    }
}

impl NavGuidance {
    pub fn distance_opt_m(&self) -> Option<f64> {
        self.target_distance_m.map(|_| self.distance_m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavAction {
    Next,
    Prev,
    Skip,
    Undo,
    Refresh,
    ClearRoute,
    Blacklist,
    Mark,
    Guide,
    Layout,
}

impl NavAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Prev => "prev",
            Self::Skip => "skip",
            Self::Undo => "undo",
            Self::Refresh => "refresh",
            Self::ClearRoute => "clear_route",
            Self::Blacklist => "blacklist",
            Self::Mark => "mark",
            Self::Guide => "guide",
            Self::Layout => "layout",
        }
    }

    pub fn parse_server_action(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "next" => Some(Self::Next),
            "prev" => Some(Self::Prev),
            "skip" => Some(Self::Skip),
            "undo" => Some(Self::Undo),
            "refresh" => Some(Self::Refresh),
            "clear_route" => Some(Self::ClearRoute),
            "blacklist" => Some(Self::Blacklist),
            "mark" => Some(Self::Mark),
            "guide" => Some(Self::Guide),
            "layout" => Some(Self::Layout),
            _ => None,
        }
    }

    pub fn parse_command_alias(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "next" | "next_target" => Some(Self::Next),
            "prev" | "previous" | "prev_target" => Some(Self::Prev),
            "skip" | "skip_current" => Some(Self::Skip),
            "blacklist" | "blacklist_current" => Some(Self::Blacklist),
            "mark" | "collect" | "collected" | "mark_collected" => Some(Self::Mark),
            "undo" => Some(Self::Undo),
            "refresh" | "refresh_nearest" => Some(Self::Refresh),
            "clear_route" | "clear_route_queue" => Some(Self::ClearRoute),
            "guide" => Some(Self::Guide),
            "layout" => Some(Self::Layout),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NavigationSummary {
    pub total: usize,
    pub collected: usize,
    pub remaining: usize,
    pub session_skipped: usize,
    pub blacklisted: usize,
    pub route_active: bool,
    pub route_step: Option<usize>,
    pub route_total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NearbyPinSnapshot {
    pub id: String,
    pub name: String,
    pub category: String,
    pub priority_tier: i32,
    pub distance_m: f64,
    pub floor_id: Option<String>,
    pub underground: bool,
    pub status: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryAction {
    Collected,
    Skip,
    Blacklist,
}

impl HistoryAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Collected => "collected",
            Self::Skip => "skip",
            Self::Blacklist => "blacklist",
        }
    }
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    action: HistoryAction,
    pin_id: String,
    space_key: SpaceKey,
    prev_override_id: Option<String>,
    prev_route_index: Option<usize>,
}

type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct RouteAdvanceEvent {
    pub section_index: usize,
    pub passed_stop_id: Option<String>,
    pub passed_kind: Option<RouteStopKind>,
    pub snapped_from_section: Option<usize>,
    pub next_target_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionCompletedEvent {
    pub finished_section_index: usize,
    pub finished_section_name: String,
    pub next_section_index: Option<usize>,
    pub next_section_name: Option<String>,
    pub route_completed: bool,
}

pub struct NavigationController {
    pub space_debounce_frames: usize,
    pending_space: Option<SpaceKey>,
    pending_space_frames: usize,
    pub arrived_distance_m: f64,
    pub spot_card_distance_m: f64,

    pins_by_id: HashMap<String, NavPin>,
    pin_order: Vec<String>,
    waypoints: Vec<Value>,

    pub collected_ids: HashSet<String>,
    pub skipped_ids: HashSet<String>,
    pub blacklisted_ids: HashSet<String>,
    passed_waypoint_ids: HashSet<String>,

    selected_by_space: HashMap<SpaceKey, String>,
    orders: HashMap<SpaceKey, Vec<String>>,
    last_pos_by_space: HashMap<SpaceKey, (f64, f64)>,
    last_heading_by_space: HashMap<SpaceKey, f64>,
    active_space: Option<SpaceKey>,
    pinned_override_id: Option<String>,
    user_locked_id: Option<String>,
    target_lock_pin: Option<NavPin>,
    last_player_floor: Option<(i32, Option<String>)>,

    clock: ClockFn,
    closer_candidate: Option<String>,
    closer_since: f64,

    route_queue: Option<Vec<String>>,
    route_index: usize,
    active_route: Option<RouteDocument>,
    active_section_idx: usize,

    history: Vec<HistoryEntry>,
    selection_ready: bool,
    last_guidance: Option<NavGuidance>,

    history_pins: HashMap<String, NavPin>,
    protected_until: BTreeMap<String, f64>,
}

/// Serializable snapshot of `NavigationController` state captured at session recording start
/// so offline replay can restore carried-over pins, locks, route state, and exclusions before frame 1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InitialNavControllerState {
    pub pins: Vec<NavPin>,
    pub active_space: Option<SpaceKey>,
    pub selected_by_space: Vec<(SpaceKey, String)>,
    pub target_id: Option<String>,
    pub locked_target_pin: Option<NavPin>,
    pub user_locked_id: Option<String>,
    pub pinned_override_id: Option<String>,
    pub collected_ids: Vec<String>,
    pub skipped_ids: Vec<String>,
    pub blacklisted_ids: Vec<String>,
    pub passed_waypoint_ids: Vec<String>,
    pub active_route_doc: Option<RouteDocument>,
    pub active_section_idx: usize,
    pub route_queue: Option<Vec<String>>,
    pub route_index: usize,
    pub last_guidance: Option<NavGuidance>,
}

impl Default for NavigationController {
    fn default() -> Self {
        Self::new()
    }
}

impl NavigationController {
    pub fn new() -> Self {
        let start = Instant::now();
        let clock: ClockFn = Arc::new(move || start.elapsed().as_secs_f64());
        Self {
            space_debounce_frames: 0,
            pending_space: None,
            pending_space_frames: 0,
            arrived_distance_m: ARRIVED_DISTANCE_M,
            spot_card_distance_m: SPOT_CARD_DISTANCE_M,
            pins_by_id: HashMap::new(),
            pin_order: Vec::new(),
            waypoints: Vec::new(),
            collected_ids: HashSet::new(),
            skipped_ids: HashSet::new(),
            blacklisted_ids: HashSet::new(),
            passed_waypoint_ids: HashSet::new(),
            selected_by_space: HashMap::new(),
            orders: HashMap::new(),
            last_pos_by_space: HashMap::new(),
            last_heading_by_space: HashMap::new(),
            active_space: None,
            pinned_override_id: None,
            user_locked_id: None,
            target_lock_pin: None,
            last_player_floor: None,
            clock,
            closer_candidate: None,
            closer_since: 0.0,
            route_queue: None,
            route_index: 0,
            active_route: None,
            active_section_idx: 0,
            history: Vec::new(),
            selection_ready: true,
            last_guidance: None,
            history_pins: HashMap::new(),
            protected_until: BTreeMap::new(),
        }
    }

    pub fn with_clock(clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        let mut ctrl = Self::new();
        ctrl.set_clock(clock);
        ctrl
    }

    pub fn with_pins(pins: Vec<NavPin>) -> Self {
        let mut ctrl = Self::new();
        if !pins.is_empty() {
            ctrl.set_pins(pins, true, None);
        }
        ctrl
    }

    pub fn with_arrived_distance_m(mut self, arrived_distance_m: f64) -> Self {
        self.arrived_distance_m = if arrived_distance_m.is_finite() {
            arrived_distance_m.max(0.5)
        } else {
            ARRIVED_DISTANCE_M
        };
        self.spot_card_distance_m = self.spot_card_distance_m.max(self.arrived_distance_m);
        self
    }

    pub fn with_spot_card_distance_m(mut self, spot_card_distance_m: f64) -> Self {
        self.spot_card_distance_m = if spot_card_distance_m.is_finite() {
            spot_card_distance_m.max(self.arrived_distance_m)
        } else {
            SPOT_CARD_DISTANCE_M.max(self.arrived_distance_m)
        };
        self
    }

    pub fn with_space_debounce_frames(mut self, frames: usize) -> Self {
        self.space_debounce_frames = frames;
        self
    }

    pub fn with_collected_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.collected_ids = ids.into_iter().collect();
        self
    }

    pub fn with_blacklisted_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.blacklisted_ids = ids.into_iter().collect();
        self
    }

    pub fn with_waypoints(mut self, waypoints: Vec<Value>) -> Self {
        self.waypoints = waypoints;
        self
    }

    pub fn set_clock(&mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) {
        self.clock = Arc::new(clock);
    }

    /// Returns the current controller timestamp in seconds from its configured clock.
    pub fn now_s(&self) -> f64 {
        (self.clock)()
    }

    /// Captures a deterministic, serializable snapshot of carried-over navigation state.
    pub fn export_initial_state(&self) -> InitialNavControllerState {
        let mut selected_by_space: Vec<(SpaceKey, String)> = self
            .selected_by_space
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        selected_by_space.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let sorted_vec = |set: &HashSet<String>| -> Vec<String> {
            let mut v: Vec<String> = set.iter().cloned().collect();
            v.sort();
            v
        };

        let target_id = self
            .target_lock_pin
            .as_ref()
            .map(|p| p.id.clone())
            .or_else(|| {
                self.last_guidance
                    .as_ref()
                    .and_then(|g| g.target.as_ref().map(|t| t.id.clone()))
            })
            .or_else(|| {
                self.active_space
                    .as_ref()
                    .and_then(|sp| self.selected_by_space.get(sp).cloned())
            });

        InitialNavControllerState {
            pins: self.pins().into_iter().cloned().collect(),
            active_space: self.active_space.clone(),
            selected_by_space,
            target_id,
            locked_target_pin: self.target_lock_pin.clone(),
            user_locked_id: self.user_locked_id.clone(),
            pinned_override_id: self.pinned_override_id.clone(),
            collected_ids: sorted_vec(&self.collected_ids),
            skipped_ids: sorted_vec(&self.skipped_ids),
            blacklisted_ids: sorted_vec(&self.blacklisted_ids),
            passed_waypoint_ids: sorted_vec(&self.passed_waypoint_ids),
            active_route_doc: self.active_route.clone(),
            active_section_idx: self.active_section_idx,
            route_queue: self.route_queue.clone(),
            route_index: self.route_index,
            last_guidance: self.last_guidance.clone(),
        }
    }

    /// Restores a snapshot produced by [`Self::export_initial_state`] before replay frame 1.
    pub fn restore_initial_state(&mut self, st: InitialNavControllerState) {
        self.pins_by_id.clear();
        self.pin_order.clear();
        self.selected_by_space.clear();
        self.orders.clear();
        self.last_pos_by_space.clear();
        self.last_heading_by_space.clear();
        self.pending_space = None;
        self.pending_space_frames = 0;
        self.closer_candidate = None;
        self.closer_since = 0.0;
        self.history.clear();
        self.history_pins.clear();
        self.protected_until.clear();
        self.active_route = None;
        self.active_section_idx = 0;
        self.route_queue = None;
        self.route_index = 0;
        self.target_lock_pin = None;
        self.user_locked_id = None;
        self.pinned_override_id = None;
        self.active_space = None;
        self.last_guidance = None;

        self.collected_ids = st.collected_ids.into_iter().collect();
        self.skipped_ids = st.skipped_ids.into_iter().collect();
        self.blacklisted_ids = st.blacklisted_ids.into_iter().collect();
        self.passed_waypoint_ids = st.passed_waypoint_ids.into_iter().collect();

        if let Some(doc) = st.active_route_doc {
            let sec_idx = if doc.sections.is_empty() {
                None
            } else {
                Some(st.active_section_idx.min(doc.sections.len().saturating_sub(1)))
            };
            let _ = self.load_route_document(doc, sec_idx, &[]);
        }

        if !st.pins.is_empty() {
            self.set_pins(st.pins, false, None);
        }

        if let Some(rq) = st.route_queue {
            let clamped = st.route_index.min(rq.len());
            self.route_queue = Some(rq);
            self.route_index = clamped;
        }

        if let Some(mut locked) = st.locked_target_pin {
            locked.normalize();
            if !self.pins_by_id.contains_key(&locked.id) {
                self.pin_order.push(locked.id.clone());
                self.pins_by_id.insert(locked.id.clone(), locked.clone());
            }
            self.target_lock_pin = Some(locked);
        }

        for (sp, pid) in st.selected_by_space {
            self.selected_by_space.insert(sp, pid);
        }
        self.active_space = st.active_space;
        if let (Some(sp), Some(tid)) = (&self.active_space, &st.target_id)
            && !self.selected_by_space.contains_key(sp)
            && self.pins_by_id.contains_key(tid)
        {
            self.selected_by_space.insert(sp.clone(), tid.clone());
        }
        self.user_locked_id = st.user_locked_id;
        self.pinned_override_id = st.pinned_override_id;
        self.last_guidance = st.last_guidance;
        self.selection_ready = true;
    }

    pub fn pins(&self) -> Vec<&NavPin> {
        self.pin_order
            .iter()
            .filter_map(|pid| self.pins_by_id.get(pid))
            .collect()
    }

    pub fn last_guidance(&self) -> Option<&NavGuidance> {
        self.last_guidance.as_ref()
    }

    pub fn selection_ready(&self) -> bool {
        self.selection_ready
    }

    pub fn get_pin(&self, pin_id: &str) -> Option<&NavPin> {
        self.pins_by_id
            .get(pin_id)
            .or_else(|| self.history_pins.get(pin_id))
    }

    pub fn get_pin_mut(&mut self, pin_id: &str) -> Option<&mut NavPin> {
        self.pins_by_id.get_mut(pin_id)
    }

    pub fn peek_history(&self) -> Option<(&'static str, &str)> {
        self.history
            .last()
            .map(|entry| (entry.action.as_str(), entry.pin_id.as_str()))
    }

    pub fn protect_pin(&mut self, pin_id: &str, seconds: f64) {
        let now = (self.clock)();
        let dur = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        self.protected_until.insert(pin_id.to_owned(), now + dur);
    }

    fn is_protected(&mut self, pin_id: &str) -> bool {
        let Some(&until) = self.protected_until.get(pin_id) else {
            return false;
        };
        if (self.clock)() >= until {
            self.protected_until.remove(pin_id);
            return false;
        }
        true
    }

    fn retained_ids(&mut self) -> Vec<String> {
        let mut keep: BTreeSet<String> = self.selected_by_space.values().cloned().collect();
        let prot_keys: Vec<String> = self.protected_until.keys().cloned().collect();
        for pid in prot_keys {
            if self.is_protected(&pid) {
                keep.insert(pid);
            }
        }
        if let Some(ref ov) = self.pinned_override_id {
            keep.insert(ov.clone());
        }
        if let Some(ref rq) = self.route_queue {
            for pid in rq {
                keep.insert(pid.clone());
            }
        }
        if let Some(ref locked) = self.target_lock_pin {
            keep.insert(locked.id.clone());
        }
        keep.into_iter().collect()
    }

    pub fn collected_ids(&self) -> &HashSet<String> {
        &self.collected_ids
    }

    pub fn skipped_ids(&self) -> &HashSet<String> {
        &self.skipped_ids
    }

    pub fn blacklisted_ids(&self) -> &HashSet<String> {
        &self.blacklisted_ids
    }

    pub fn excluded_ids(&self) -> HashSet<String> {
        let mut out = self.collected_ids.clone();
        out.extend(self.skipped_ids.iter().cloned());
        out.extend(self.blacklisted_ids.iter().cloned());
        out
    }

    pub fn set_pins(
        &mut self,
        pins: Vec<NavPin>,
        reset_order: bool,
        coverage: Option<(f64, f64, f64)>,
    ) {
        let mut new_by_id: HashMap<String, NavPin> = HashMap::new();
        let mut new_order: Vec<String> = Vec::new();
        for mut pin in pins {
            pin.normalize();
            if !new_by_id.contains_key(&pin.id) {
                new_order.push(pin.id.clone());
            }
            new_by_id.insert(pin.id.clone(), pin);
        }

        let cov = coverage.and_then(|(c_lat, c_lng, c_rad)| {
            if c_lat.is_finite() && c_lng.is_finite() && c_rad.is_finite() && c_rad > 0.0 {
                Some((c_lat, c_lng, c_rad))
            } else {
                None
            }
        });

        let route_ids: HashSet<String> = self
            .route_queue
            .as_ref()
            .map(|rq| rq.iter().cloned().collect())
            .unwrap_or_default();

        let retained = self.retained_ids();
        for pid in retained {
            let is_route_stop = route_ids.contains(&pid);
            if let Some(incoming) = new_by_id.get_mut(&pid) {
                // Preserve RouteDocument metadata when a nearby filter pin shares an ID with an active route stop.
                if is_route_stop && let Some(old) = self.pins_by_id.get(&pid) {
                    if incoming.stop_kind.is_none() {
                        incoming.stop_kind = old.stop_kind.clone();
                    }
                    if incoming.route_section_index.is_none() {
                        incoming.route_section_index = old.route_section_index;
                    }
                    if incoming.route_section_total.is_none() {
                        incoming.route_section_total = old.route_section_total;
                    }
                    if incoming.route_section_name.is_none() {
                        incoming.route_section_name = old.route_section_name.clone();
                    }
                    if incoming.floor_id.is_none() {
                        incoming.floor_id = old.floor_id.clone();
                    }
                    if incoming.floor_name.is_none() {
                        incoming.floor_name = old.floor_name.clone();
                    }
                    if incoming.description.is_empty() {
                        incoming.description = old.description.clone();
                    }
                    if incoming.image_url.is_empty() {
                        incoming.image_url = old.image_url.clone();
                    }
                    if incoming.label_id.is_none() {
                        incoming.label_id = old.label_id;
                    }
                }
                continue;
            }
            let is_locked = self
                .target_lock_pin
                .as_ref()
                .is_some_and(|p| p.id == pid);
            let Some(old) = self.pins_by_id.get(&pid).cloned().or_else(|| {
                self.target_lock_pin
                    .as_ref()
                    .filter(|p| p.id == pid)
                    .cloned()
            }) else {
                continue;
            };
            if is_locked || is_route_stop || self.is_protected(&pid) {
                // Keep actively locked pin, active route stop, or protected pin until completed/cleared.
            } else if let Some((c_lat, c_lng, c_rad)) = cov {
                let inside = (old.lat - c_lat).hypot(old.lng - c_lng)
                    <= c_rad * COVERAGE_INSIDE_FACTOR;
                let active_map = self
                    .active_space
                    .as_ref()
                    .map(|sk| sk.map_id)
                    .unwrap_or(old.map_id);
                if inside && old.map_id == active_map {
                    continue;
                }
            } else {
                continue;
            }
            new_by_id.insert(pid.clone(), old);
            new_order.push(pid);
        }

        self.pins_by_id = new_by_id;
        self.pin_order = new_order;

        if reset_order {
            self.orders.clear();
        }
        self.selected_by_space
            .retain(|_, pid| self.pins_by_id.contains_key(pid));
        if let Some(ref ov) = self.pinned_override_id
            && !self.pins_by_id.contains_key(ov)
        {
            self.pinned_override_id = None;
        }
        if self.active_space.is_none()
            && let Some(first_id) = self.pin_order.first()
            && let Some(first_pin) = self.pins_by_id.get(first_id)
        {
            self.active_space = Some(first_pin.space_key());
        }
    }

    pub fn set_waypoints(&mut self, waypoints: Vec<Value>) {
        self.waypoints = waypoints;
    }

    pub fn set_route_queue(&mut self, ordered_pins: Vec<NavPin>) {
        if ordered_pins.is_empty() {
            self.clear_route_queue();
            return;
        }
        let mut queue_ids = Vec::with_capacity(ordered_pins.len());
        let mut seen = HashSet::with_capacity(ordered_pins.len());
        for mut pin in ordered_pins {
            pin.normalize();
            let pid = pin.id.clone();
            if !self.pins_by_id.contains_key(&pid) {
                self.pins_by_id.insert(pid.clone(), pin);
                self.pin_order.push(pid.clone());
            }
            if seen.insert(pid.clone()) {
                queue_ids.push(pid);
            }
        }
        self.install_route_queue(queue_ids);
    }

    pub fn set_route_queue_ids(&mut self, ordered_ids: &[String]) {
        if ordered_ids.is_empty() {
            self.clear_route_queue();
            return;
        }
        let mut queue_ids = Vec::with_capacity(ordered_ids.len());
        let mut seen = HashSet::with_capacity(ordered_ids.len());
        for pid in ordered_ids {
            if self.pins_by_id.contains_key(pid) && seen.insert(pid.clone()) {
                queue_ids.push(pid.clone());
            }
        }
        self.install_route_queue(queue_ids);
    }

    fn install_route_queue(&mut self, queue_ids: Vec<String>) {
        if queue_ids.is_empty() {
            self.clear_route_queue();
            return;
        }
        let first_space = self
            .pins_by_id
            .get(&queue_ids[0])
            .map(NavPin::space_key);
        self.route_queue = Some(queue_ids);
        self.route_index = 0;
        self.pinned_override_id = None;
        if self.active_space.is_none() {
            self.active_space = first_space;
        }
        self.sync_route_index_forward();
    }

    pub fn clear_route_queue(&mut self) {
        self.route_queue = None;
        self.route_index = 0;
    }

    /// Clears transient nearby pins, target lock, and route queue when the active
    /// browser tab switches between different map providers (e.g. HoYoLAB <-> AppSample
    /// or Kongying), preventing stale pin IDs from one provider from being marked on another.
    pub fn clear_for_provider_switch(&mut self) -> bool {
        let had_lock = self.target_lock_pin.is_some();
        self.pins_by_id.clear();
        self.pin_order.clear();
        self.selected_by_space.clear();
        self.orders.clear();
        self.pinned_override_id = None;
        self.target_lock_pin = None;
        self.user_locked_id = None;
        self.closer_candidate = None;
        self.route_queue = None;
        self.route_index = 0;
        self.last_guidance = None;
        had_lock
    }

    /// Locks navigation onto a single target pin until collected/skipped/blacklisted,
    /// cleared via [`Self::clear_target_lock`], or replaced by a loaded route.
    pub fn lock_target_pin(&mut self, mut pin: NavPin) -> NavPin {
        pin.normalize();
        let pid = pin.id.clone();
        if let Some(existing) = self.pins_by_id.get(&pid) {
            if pin.floor_id.is_none() {
                pin.floor_id = existing.floor_id.clone();
            }
            if pin.floor_name.is_none() {
                pin.floor_name = existing.floor_name.clone();
            }
            if pin.image_url.is_empty() {
                pin.image_url = existing.image_url.clone();
            }
            if pin.description.is_empty() {
                pin.description = existing.description.clone();
            }
            if pin.category.is_empty() {
                pin.category = existing.category.clone();
            }
            if pin.label_id.is_none() {
                pin.label_id = existing.label_id;
            }
            if pin.parent_label_id.is_none() {
                pin.parent_label_id = existing.parent_label_id;
            }
        } else {
            self.pin_order.push(pid.clone());
        }
        self.pins_by_id.insert(pid.clone(), pin.clone());
        self.skipped_ids.remove(&pid);
        self.blacklisted_ids.remove(&pid);
        self.target_lock_pin = Some(pin.clone());
        self.user_locked_id = Some(pid);
        self.closer_candidate = None;
        if self.active_space.is_none() {
            self.active_space = Some(pin.space_key());
        }
        pin
    }

    /// Clears the active single-pin target lock. Returns `true` if a lock was active.
    pub fn clear_target_lock(&mut self) -> bool {
        let Some(locked) = self.target_lock_pin.take() else {
            return false;
        };
        if self.user_locked_id.as_deref() == Some(locked.id.as_str()) {
            self.user_locked_id = None;
        }
        if self.pinned_override_id.as_deref() == Some(locked.id.as_str()) {
            self.pinned_override_id = None;
        }
        true
    }

    /// Clears any manual user lock (single-pin target lock, cycle/selection lock,
    /// or cross-floor override). Returns `true` if any user lock was active.
    pub fn clear_user_locks(&mut self) -> bool {
        let cleared_target = self.clear_target_lock();
        let cleared_user = self.user_locked_id.take().is_some();
        let cleared_override = self.pinned_override_id.take().is_some();
        let cleared = cleared_target || cleared_user || cleared_override;
        if cleared {
            self.closer_candidate = None;
        }
        cleared
    }

    /// Returns the currently locked single target pin, if any.
    pub fn locked_target_pin(&self) -> Option<&NavPin> {
        let locked = self.target_lock_pin.as_ref()?;
        self.pins_by_id.get(&locked.id).or(Some(locked))
    }

    /// Returns the user-locked (cycle/selection) pin ID, if any.
    pub fn user_locked_id(&self) -> Option<&str> {
        self.user_locked_id.as_deref()
    }

    /// Resets local progress for the active route document or route queue without touching HoYoLAB.
    pub fn reset_route_progress(&mut self) -> bool {
        if let Some(doc) = self.active_route.as_ref() {
            let stop_ids: Vec<String> = doc
                .sections
                .iter()
                .flat_map(|sec| sec.stops.iter().map(|s| s.id.clone()))
                .collect();
            for id in &stop_ids {
                self.collected_ids.remove(id);
                self.skipped_ids.remove(id);
                self.passed_waypoint_ids.remove(id);
            }
            self.clear_target_lock();
            self.active_section_idx = 0;
            self.install_active_route_section(0);
            return true;
        }
        if let Some(rq) = self.route_queue.clone() {
            for id in &rq {
                self.collected_ids.remove(id);
                self.skipped_ids.remove(id);
                self.passed_waypoint_ids.remove(id);
            }
            self.clear_target_lock();
            self.route_index = 0;
            let _ = self.sync_route_index_forward();
            return true;
        }
        false
    }

    pub fn is_route_stop_done(&self, pid: &str) -> bool {
        self.collected_ids.contains(pid)
            || self.skipped_ids.contains(pid)
            || self.blacklisted_ids.contains(pid)
            || (self.route_queue.is_some() && self.passed_waypoint_ids.contains(pid))
    }

    pub fn active_route_doc(&self) -> Option<(&RouteDocument, usize)> {
        let doc = self.active_route.as_ref()?;
        Some((doc, self.active_section_idx.min(doc.sections.len().saturating_sub(1))))
    }

    pub fn clear_route_document(&mut self) {
        self.active_route = None;
        self.active_section_idx = 0;
        self.passed_waypoint_ids.clear();
        self.clear_route_queue();
    }

    pub fn load_route_document(
        &mut self,
        doc: RouteDocument,
        section_idx: Option<usize>,
        initially_marked_ids: &[String],
    ) -> Option<usize> {
        self.clear_target_lock();
        for raw_id in initially_marked_ids {
            let trimmed = raw_id.trim();
            if !trimmed.is_empty() {
                self.collected_ids.insert(trimmed.to_owned());
            }
        }
        self.passed_waypoint_ids.clear();
        if doc.sections.is_empty() {
            self.clear_route_document();
            return None;
        }

        let excluded = self.excluded_ids();
        let chosen_idx = match section_idx {
            Some(i) if i < doc.sections.len() => i,
            _ => doc
                .sections
                .iter()
                .position(|sec| {
                    sec.stops
                        .iter()
                        .any(|s| s.kind == RouteStopKind::Collectible && !excluded.contains(&s.id))
                })
                .unwrap_or(0),
        };
        self.active_route = Some(doc);
        self.install_active_route_section(chosen_idx);
        Some(chosen_idx)
    }

    pub fn select_route_section(&mut self, section_idx: usize) -> bool {
        let Some(ref doc) = self.active_route else {
            return false;
        };
        if section_idx >= doc.sections.len() {
            return false;
        }
        self.install_active_route_section(section_idx);
        true
    }

    fn install_active_route_section(&mut self, sec_idx: usize) {
        let Some(doc) = self.active_route.as_ref() else {
            return;
        };
        if sec_idx >= doc.sections.len() {
            return;
        }
        let map_id = doc.map_id;
        let total_secs = doc.sections.len();
        let sec = &doc.sections[sec_idx];

        let mut queue_ids = Vec::with_capacity(sec.stops.len());
        let mut seen = HashSet::with_capacity(sec.stops.len());
        for s in &sec.stops {
            let mut pin = NavPin {
                id: s.id.clone(),
                map_id,
                lat: s.lat,
                lng: s.lng,
                name: s.name.clone(),
                floor_id: s.floor_id.clone(),
                floor_name: if s.floor_name.is_empty() {
                    None
                } else {
                    Some(s.floor_name.clone())
                },
                is_underground: s.is_underground,
                image_url: s.img_url.clone(),
                description: s.note.clone(),
                label_id: if s.label_id > 0 {
                    Some(s.label_id as i64)
                } else {
                    None
                },
                stop_kind: Some(s.kind.as_str().to_owned()),
                route_section_index: Some(sec_idx),
                route_section_total: Some(total_secs),
                route_section_name: Some(sec.name.clone()),
                ..Default::default()
            };
            pin.normalize();
            if let Some(existing) = self.pins_by_id.get(&pin.id) {
                if pin.floor_id.is_none() {
                    pin.floor_id = existing.floor_id.clone();
                }
                if pin.floor_name.is_none() {
                    pin.floor_name = existing.floor_name.clone();
                }
                if pin.image_url.is_empty() {
                    pin.image_url = existing.image_url.clone();
                }
                if pin.description.is_empty() {
                    pin.description = existing.description.clone();
                }
                if pin.category.is_empty() {
                    pin.category = existing.category.clone();
                }
            }
            let pid = pin.id.clone();
            if !self.pins_by_id.contains_key(&pid) {
                self.pin_order.push(pid.clone());
            }
            self.pins_by_id.insert(pid.clone(), pin);
            if seen.insert(pid.clone()) {
                queue_ids.push(pid);
            }
        }

        self.active_section_idx = sec_idx;
        if let Some(first_id) = queue_ids.first()
            && let Some(first_pin) = self.pins_by_id.get(first_id)
        {
            self.active_space = Some(first_pin.space_key());
        }
        self.user_locked_id = None;
        self.install_route_queue(queue_ids);
    }

    /// Evaluates continuous minimap coordinates against the active RouteDocument:
    /// 1. Snaps out-of-order to another uncompleted section when the player teleports directly there.
    /// 2. Auto-passes `teleport` (<= 55u) and `waypoint` (<= 22u, checking `floor_id` when both
    ///    player and waypoint floors are known) stops in order without manual marking.
    /// 3. Keeps `collectible` stops strictly manual (`Alt + K`).
    pub fn advance_route_on_position(
        &mut self,
        player_map_id: i32,
        player_lat: f64,
        player_lng: f64,
    ) -> Option<RouteAdvanceEvent> {
        let tracked_floor = self
            .last_player_floor
            .as_ref()
            .filter(|(mid, _)| *mid == player_map_id)
            .and_then(|(_, fid)| fid.clone());
        self.advance_route_on_position_on_floor(
            player_map_id,
            player_lat,
            player_lng,
            tracked_floor.as_deref(),
        )
    }

    /// Floor-aware variant of [`Self::advance_route_on_position`].
    pub fn advance_route_on_position_on_floor(
        &mut self,
        player_map_id: i32,
        player_lat: f64,
        player_lng: f64,
        player_floor_id: Option<&str>,
    ) -> Option<RouteAdvanceEvent> {
        if !player_lat.is_finite() || !player_lng.is_finite() || self.target_lock_pin.is_some() {
            return None;
        }
        let doc = self.active_route.as_ref()?;
        if doc.map_id != player_map_id || doc.sections.is_empty() {
            return None;
        }

        let cur_idx = self.active_section_idx.min(doc.sections.len() - 1);
        let cur_sec = &doc.sections[cur_idx];
        let min_dist_cur = {
            let mut best = f64::INFINITY;
            if let Some(ref anchor) = cur_sec.anchor_teleport {
                best = best.min((anchor.lat - player_lat).hypot(anchor.lng - player_lng));
            }
            for s in &cur_sec.stops {
                if !self.is_route_stop_done(&s.id) {
                    best = best.min((s.lat - player_lat).hypot(s.lng - player_lng));
                }
            }
            best
        };

        let mut snapped_from: Option<usize> = None;
        if min_dist_cur > 300.0 {
            let mut best_sec: Option<(usize, f64)> = None;
            for (idx, sec) in doc.sections.iter().enumerate() {
                if idx == cur_idx {
                    continue;
                }
                let has_uncollected = sec.stops.iter().any(|s| {
                    s.kind == RouteStopKind::Collectible && !self.excluded_ids().contains(&s.id)
                });
                if !has_uncollected {
                    continue;
                }
                let mut d_min = sec
                    .anchor_teleport
                    .as_ref()
                    .map(|a| (a.lat - player_lat).hypot(a.lng - player_lng))
                    .unwrap_or(f64::INFINITY);
                for s in &sec.stops {
                    if !self.excluded_ids().contains(&s.id) {
                        d_min = d_min.min((s.lat - player_lat).hypot(s.lng - player_lng));
                    }
                }
                if d_min <= 65.0 && best_sec.is_none_or(|(_, bd)| d_min < bd) {
                    best_sec = Some((idx, d_min));
                }
            }
            if let Some((next_idx, _)) = best_sec {
                self.install_active_route_section(next_idx);
                snapped_from = Some(cur_idx);
            }
        }

        let norm_p_floor = norm_floor_id(player_floor_id);
        let mut passed_stop_id: Option<String> = None;
        let mut passed_kind: Option<RouteStopKind> = None;
        for _ in 0..4 {
            let cur_pin = self.current_target().cloned();
            let Some(pin) = cur_pin else {
                break;
            };
            let threshold = match pin.stop_kind.as_deref() {
                Some("teleport") => 55.0,
                Some("waypoint") => 22.0,
                _ => break,
            };
            if pin.stop_kind.as_deref() == Some("waypoint")
                && let (Some(pf), Some(wf)) = (
                    norm_p_floor.as_deref(),
                    pin.floor_id.as_deref().map(str::trim).filter(|s| !s.is_empty()),
                )
                && pf != wf
            {
                break;
            }
            let dist = (pin.lat - player_lat).hypot(pin.lng - player_lng);
            if dist > threshold {
                break;
            }
            let kind = if pin.stop_kind.as_deref() == Some("teleport") {
                RouteStopKind::Teleport
            } else {
                RouteStopKind::Waypoint
            };
            self.passed_waypoint_ids.insert(pin.id.clone());
            passed_stop_id = Some(pin.id.clone());
            passed_kind = Some(kind);
            if let Some(ref rq) = self.route_queue
                && self.route_index < rq.len()
                && rq[self.route_index] == pin.id
            {
                self.route_index = rq.len().min(self.route_index + 1);
            }
            let _ = self.sync_route_index_forward();
        }

        if snapped_from.is_none() && passed_stop_id.is_none() {
            return None;
        }
        let next_name = self
            .current_target()
            .map(NavPin::display_name)
            .unwrap_or_default();
        Some(RouteAdvanceEvent {
            section_index: self.active_section_idx,
            passed_stop_id,
            passed_kind,
            snapped_from_section: snapped_from,
            next_target_name: next_name,
        })
    }

    pub fn check_and_advance_completed_section(&mut self) -> Option<SectionCompletedEvent> {
        let doc = self.active_route.as_ref()?;
        if doc.sections.is_empty() {
            return None;
        }
        let cur_idx = self.active_section_idx.min(doc.sections.len() - 1);
        let cur_sec = &doc.sections[cur_idx];
        let cur_collectibles = cur_sec.collectible_count();
        if cur_collectibles == 0 {
            return None;
        }
        let excluded = self.excluded_ids();
        let cur_remaining = cur_sec
            .stops
            .iter()
            .filter(|s| s.kind == RouteStopKind::Collectible && !excluded.contains(&s.id))
            .count();
        if cur_remaining > 0 {
            return None;
        }

        let finished_name = cur_sec.name.clone();
        let total = doc.sections.len();
        let mut next_idx_opt: Option<usize> = None;
        for offset in 1..total {
            let cand_idx = (cur_idx + offset) % total;
            let cand_sec = &doc.sections[cand_idx];
            if cand_sec
                .stops
                .iter()
                .any(|s| s.kind == RouteStopKind::Collectible && !excluded.contains(&s.id))
            {
                next_idx_opt = Some(cand_idx);
                break;
            }
        }

        if let Some(next_idx) = next_idx_opt {
            let next_name = doc.sections[next_idx].name.clone();
            self.install_active_route_section(next_idx);
            Some(SectionCompletedEvent {
                finished_section_index: cur_idx,
                finished_section_name: finished_name,
                next_section_index: Some(next_idx),
                next_section_name: Some(next_name),
                route_completed: false,
            })
        } else {
            Some(SectionCompletedEvent {
                finished_section_index: cur_idx,
                finished_section_name: finished_name,
                next_section_index: None,
                next_section_name: None,
                route_completed: true,
            })
        }
    }

    pub fn route_status_json(&self) -> Value {
        let Some(doc) = self.active_route.as_ref() else {
            return json!({ "active": false });
        };
        let sec_idx = self.active_section_idx.min(doc.sections.len().saturating_sub(1));
        let excluded = self.excluded_ids();
        let mut route_done = 0usize;
        let mut route_total = 0usize;
        let sections_summary: Vec<Value> = doc
            .sections
            .iter()
            .enumerate()
            .map(|(i, sec)| {
                let sec_total = sec.collectible_count();
                let sec_done = sec
                    .stops
                    .iter()
                    .filter(|s| s.kind == RouteStopKind::Collectible && excluded.contains(&s.id))
                    .count();
                route_done += sec_done;
                route_total += sec_total;
                json!({
                    "index": i,
                    "name": sec.name,
                    "color": sec.color,
                    "done": sec_done,
                    "total": sec_total,
                    "anchor_teleport": sec.anchor_teleport,
                })
            })
            .collect();

        let cur_sec = doc.sections.get(sec_idx);
        let section_done = cur_sec
            .map(|sec| {
                sec.stops
                    .iter()
                    .filter(|s| s.kind == RouteStopKind::Collectible && excluded.contains(&s.id))
                    .count()
            })
            .unwrap_or(0);
        let section_total = cur_sec.map(|s| s.collectible_count()).unwrap_or(0);
        let active_stops: Vec<Value> = cur_sec
            .map(|sec| {
                sec.stops
                    .iter()
                    .enumerate()
                    .map(|(idx, s)| {
                        json!({
                            "index": idx,
                            "id": s.id,
                            "kind": s.kind.as_str(),
                            "lat": s.lat,
                            "lng": s.lng,
                            "label_id": s.label_id,
                            "floor_id": s.floor_id,
                            "floor_name": s.floor_name,
                            "is_underground": s.is_underground,
                            "name": s.name,
                            "collected": self.is_route_stop_done(&s.id),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        json!({
            "active": true,
            "title": doc.title,
            "description": doc.description,
            "map_id": doc.map_id,
            "map_name": doc.map_name,
            "provider": doc.source.provider,
            "path_id": doc.source.path_id,
            "share_url": doc.source.share_url,
            "active_section": sec_idx,
            "section_count": doc.sections.len(),
            "section_name": cur_sec.map(|s| s.name.as_str()).unwrap_or(""),
            "section_color": cur_sec.map(|s| s.color.as_str()).unwrap_or("#38bdf8"),
            "section_done": section_done,
            "section_total": section_total,
            "route_done": route_done,
            "route_total": route_total,
            "anchor_teleport": cur_sec.and_then(|s| s.anchor_teleport.as_ref()),
            "active_edges": cur_sec.map(|s| &s.edges),
            "active_stops": active_stops,
            "sections_summary": sections_summary,
        })
    }

    pub fn is_route_mode(&self) -> bool {
        self.active_route.is_some() || self.route_queue.as_ref().is_some_and(|q| !q.is_empty())
    }

    pub fn route_progress(&self) -> Option<(usize, usize)> {
        let rq = self.route_queue.as_ref()?;
        if rq.is_empty() {
            return None;
        }
        let total = rq.len();
        if self.route_index >= total {
            Some((total, total))
        } else {
            Some((self.route_index + 1, total))
        }
    }

    fn eligible_for_space(&self, space_key: &SpaceKey) -> Vec<NavPin> {
        let excluded = self.excluded_ids();
        let mut same_space = Vec::new();
        for pid in &self.pin_order {
            if excluded.contains(pid) {
                continue;
            }
            if let Some(pin) = self.pins_by_id.get(pid)
                && is_same_floor_for_space(pin, space_key)
            {
                same_space.push(pin.clone());
            }
        }
        if !same_space.is_empty() {
            return same_space;
        }
        let mut same_map = Vec::new();
        for pid in &self.pin_order {
            if excluded.contains(pid) {
                continue;
            }
            if let Some(pin) = self.pins_by_id.get(pid)
                && pin.map_id == space_key.map_id
            {
                same_map.push(pin.clone());
            }
        }
        same_map
    }

    fn ordered_for_space(
        &mut self,
        space_key: &SpaceKey,
        player_pos: Option<(f64, f64)>,
        view_heading_deg: Option<f64>,
    ) -> Vec<NavPin> {
        let eligible = self.eligible_for_space(space_key);
        if eligible.is_empty() {
            self.orders.remove(space_key);
            return Vec::new();
        }

        let pos = player_pos.or_else(|| self.last_pos_by_space.get(space_key).copied());
        let heading =
            view_heading_deg.or_else(|| self.last_heading_by_space.get(space_key).copied());

        let by_id: HashMap<&str, &NavPin> =
            eligible.iter().map(|p| (p.id.as_str(), p)).collect();
        let mut order = self.orders.get(space_key).cloned().unwrap_or_default();

        if !order.iter().any(|pid| by_id.contains_key(pid.as_str())) {
            let mut sorted_pins = eligible.clone();
            if let Some((plat, plng)) = pos {
                sorted_pins.sort_by_key(|p| pin_rank_key(p, plat, plng, heading));
            }
            order = sorted_pins.iter().map(|p| p.id.clone()).collect();
            self.orders.insert(space_key.clone(), order.clone());
        }

        let mut ordered: Vec<NavPin> = order
            .iter()
            .filter_map(|pid| by_id.get(pid.as_str()).map(|p| (*p).clone()))
            .collect();
        let existing: HashSet<String> = ordered.iter().map(|p| p.id.clone()).collect();
        let mut extras: Vec<NavPin> = eligible
            .iter()
            .filter(|p| !existing.contains(&p.id))
            .cloned()
            .collect();
        if !extras.is_empty() {
            if let Some((plat, plng)) = pos {
                extras.sort_by_key(|p| pin_rank_key(p, plat, plng, heading));
            }
            ordered.extend(extras);
            let new_order: Vec<String> = ordered.iter().map(|p| p.id.clone()).collect();
            self.orders.insert(space_key.clone(), new_order);
        }
        ordered
    }

    fn sync_route_index_forward(&mut self) -> Option<NavPin> {
        let rq = self.route_queue.as_ref()?;
        if rq.is_empty() {
            return None;
        }
        let start = self.route_index;
        for (idx, pid) in rq.iter().enumerate().skip(start) {
            if !self.is_route_stop_done(pid)
                && let Some(pin) = self.pins_by_id.get(pid)
            {
                self.route_index = idx;
                return Some(pin.clone());
            }
        }
        self.route_index = rq.len();
        None
    }

    fn nearest_of(
        candidates: &[NavPin],
        pos: Option<(f64, f64)>,
        view_heading_deg: Option<f64>,
    ) -> Option<NavPin> {
        let (plat, plng) = pos?;
        candidates
            .iter()
            .min_by_key(|p| pin_rank_key(p, plat, plng, view_heading_deg))
            .cloned()
    }

    fn resolve_target(
        &mut self,
        space_key: &SpaceKey,
        player_pos: Option<(f64, f64)>,
        view_heading_deg: Option<f64>,
    ) -> Option<NavPin> {
        if let Some(locked_id) = self.target_lock_pin.as_ref().map(|p| p.id.clone()) {
            let locked = self
                .pins_by_id
                .get(&locked_id)
                .cloned()
                .or_else(|| self.target_lock_pin.clone())?;
            if locked.map_id == space_key.map_id {
                self.selected_by_space
                    .insert(space_key.clone(), locked.id.clone());
            }
            return Some(locked);
        }

        let excluded = self.excluded_ids();

        if self.route_queue.is_some() {
            let pin = self.sync_route_index_forward();
            if let Some(ref p) = pin {
                self.selected_by_space
                    .insert(space_key.clone(), p.id.clone());
            } else {
                self.selected_by_space.remove(space_key);
            }
            return pin;
        }

        if let Some(ref ov_id) = self.pinned_override_id.clone() {
            if let Some(override_pin) = self.pins_by_id.get(ov_id).cloned()
                && !excluded.contains(&override_pin.id)
                && override_pin.map_id == space_key.map_id
            {
                self.selected_by_space
                    .insert(space_key.clone(), override_pin.id.clone());
                return Some(override_pin);
            }
            self.pinned_override_id = None;
        }

        let heading =
            view_heading_deg.or_else(|| self.last_heading_by_space.get(space_key).copied());
        let candidates = self.ordered_for_space(space_key, player_pos, heading);
        if candidates.is_empty() {
            self.selected_by_space.remove(space_key);
            return None;
        }

        if let Some(selected_id) = self.selected_by_space.get(space_key)
            && let Some(found) = candidates.iter().find(|p| &p.id == selected_id)
        {
            return Some(found.clone());
        }

        let pos = player_pos.or_else(|| self.last_pos_by_space.get(space_key).copied());
        let chosen = Self::nearest_of(&candidates, pos, heading)
            .unwrap_or_else(|| candidates[0].clone());
        self.selected_by_space
            .insert(space_key.clone(), chosen.id.clone());
        Some(chosen)
    }

    pub fn current_target_peek(&self) -> Option<&NavPin> {
        if let Some(ref locked) = self.target_lock_pin {
            return self.pins_by_id.get(&locked.id).or(Some(locked));
        }
        if let Some(ref rq) = self.route_queue {
            for pid in rq.iter().skip(self.route_index) {
                if !self.is_route_stop_done(pid)
                    && let Some(pin) = self.pins_by_id.get(pid)
                {
                    return Some(pin);
                }
            }
            return None;
        }
        let excluded = self.excluded_ids();
        if let Some(ref ov_id) = self.pinned_override_id
            && !excluded.contains(ov_id)
            && let Some(pin) = self.pins_by_id.get(ov_id)
        {
            return Some(pin);
        }
        let active = self.active_space.as_ref()?;
        let pid = self.selected_by_space.get(active)?;
        if !excluded.contains(pid) {
            self.pins_by_id.get(pid)
        } else {
            None
        }
    }

    pub fn current_target(&mut self) -> Option<&NavPin> {
        if let Some(ref locked) = self.target_lock_pin {
            return self.pins_by_id.get(&locked.id).or(Some(locked));
        }
        if self.route_queue.is_some() {
            let mut hit_id: Option<String> = None;
            if let Some(ref rq) = self.route_queue
                && self.route_index < rq.len()
            {
                let pid = &rq[self.route_index];
                if !self.is_route_stop_done(pid) && self.pins_by_id.contains_key(pid) {
                    hit_id = Some(pid.clone());
                }
            }
            let target_id = if let Some(id) = hit_id {
                Some(id)
            } else {
                self.sync_route_index_forward().map(|p| p.id)
            };
            return target_id.and_then(|id| self.pins_by_id.get(&id));
        }

        let excluded = self.excluded_ids();
        if let Some(ref ov_id) = self.pinned_override_id
            && !excluded.contains(ov_id)
            && self.pins_by_id.contains_key(ov_id)
        {
            return self.pins_by_id.get(ov_id);
        }

        let active = self.active_space.clone()?;
        if let Some(pid) = self.selected_by_space.get(&active)
            && !excluded.contains(pid)
            && self.pins_by_id.contains_key(pid)
        {
            let pid_owned = pid.clone();
            return self.pins_by_id.get(&pid_owned);
        }
        let resolved_id = self.resolve_target(&active, None, None).map(|p| p.id)?;
        self.pins_by_id.get(&resolved_id)
    }

    pub fn summary(&self) -> NavigationSummary {
        let ids: HashSet<String> = if let Some(ref rq) = self.route_queue {
            rq.iter()
                .filter(|pid| self.pins_by_id.contains_key(*pid))
                .cloned()
                .collect()
        } else if let Some(ref active) = self.active_space {
            let map_id = active.map_id;
            self.pins_by_id
                .iter()
                .filter(|(_, p)| p.map_id == map_id)
                .map(|(pid, _)| pid.clone())
                .collect()
        } else {
            self.pins_by_id.keys().cloned().collect()
        };

        let collected = ids.intersection(&self.collected_ids).count();
        let skipped = ids.intersection(&self.skipped_ids).count();
        let blacklisted = ids.intersection(&self.blacklisted_ids).count();
        let excluded = self.excluded_ids();
        let remaining = ids.difference(&excluded).count();
        let rp = self.route_progress();
        NavigationSummary {
            total: ids.len(),
            collected,
            remaining,
            session_skipped: skipped,
            blacklisted,
            route_active: self.is_route_mode(),
            route_step: rp.map(|r| r.0),
            route_total: rp.map(|r| r.1),
        }
    }

    pub fn nearby_snapshot(
        &self,
        lat: f64,
        lng: f64,
        map_id: i32,
        limit: usize,
    ) -> Vec<NearbyPinSnapshot> {
        let mut rows: Vec<NearbyPinSnapshot> = self
            .pins()
            .into_iter()
            .filter(|pin| pin.map_id == map_id)
            .map(|pin| {
                let status = if self.collected_ids.contains(&pin.id) {
                    "collected"
                } else if self.skipped_ids.contains(&pin.id) {
                    "skipped"
                } else if self.blacklisted_ids.contains(&pin.id) {
                    "blacklisted"
                } else {
                    "eligible"
                };
                let dist = (pin.lng - lng).hypot(pin.lat - lat);
                NearbyPinSnapshot {
                    id: pin.id.clone(),
                    name: pin.display_name(),
                    category: pin.category.clone(),
                    priority_tier: pin_priority_tier(pin),
                    distance_m: (dist * 10.0).round() / 10.0,
                    floor_id: pin.floor_id.clone(),
                    underground: pin.is_underground,
                    status,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.distance_m.total_cmp(&b.distance_m).then_with(|| a.id.cmp(&b.id)));
        rows.truncate(limit);
        rows
    }

    pub fn select_target(&mut self, pin_id: &str) -> Option<&NavPin> {
        if !self.pins_by_id.contains_key(pin_id) || self.excluded_ids().contains(pin_id) {
            return None;
        }
        let pin = self.pins_by_id.get(pin_id)?.clone();
        if let Some(ref rq) = self.route_queue {
            let idx = rq.iter().position(|p| p == pin_id)?;
            self.route_index = idx;
            if let Some(ref active) = self.active_space {
                self.selected_by_space
                    .insert(active.clone(), pin_id.to_owned());
            }
            return self.pins_by_id.get(pin_id);
        }

        if self.active_space.is_none() {
            self.active_space = Some(pin.space_key());
        }
        let active = self.active_space.clone()?;
        self.selected_by_space.insert(active.clone(), pin.id.clone());
        self.user_locked_id = Some(pin.id.clone());
        if !is_same_floor_for_space(&pin, &active) {
            self.pinned_override_id = Some(pin.id.clone());
        } else {
            self.pinned_override_id = None;
        }
        self.pins_by_id.get(pin_id)
    }

    fn cycle(&mut self, offset: isize) -> Option<String> {
        if !self.selection_ready {
            return self.current_target().map(|p| p.id.clone());
        }

        if self.route_queue.is_some() {
            let rq = self.route_queue.as_ref()?;
            let total = rq.len();
            if total == 0 {
                return None;
            }
            if offset > 0 {
                for (idx, pid) in rq.iter().enumerate().skip(self.route_index + 1) {
                    if !self.is_route_stop_done(pid) && self.pins_by_id.contains_key(pid) {
                        let pid_owned = pid.clone();
                        self.route_index = idx;
                        if let Some(ref active) = self.active_space {
                            self.selected_by_space
                                .insert(active.clone(), pid_owned.clone());
                        }
                        return Some(pid_owned);
                    }
                }
                return self.current_target().map(|p| p.id.clone());
            }
            if offset < 0 {
                if self.route_index > 0 {
                    let top = (self.route_index - 1).min(total - 1);
                    for idx in (0..=top).rev() {
                        let pid = &rq[idx];
                        if !self.is_route_stop_done(pid) && self.pins_by_id.contains_key(pid) {
                            let pid_owned = pid.clone();
                            self.route_index = idx;
                            if let Some(ref active) = self.active_space {
                                self.selected_by_space
                                    .insert(active.clone(), pid_owned.clone());
                            }
                            return Some(pid_owned);
                        }
                    }
                }
                return self.current_target().map(|p| p.id.clone());
            }
            return self.current_target().map(|p| p.id.clone());
        }

        let active = self.active_space.clone()?;
        let current_id = self.current_target().map(|p| p.id.clone());
        let same_floor_cands = self.ordered_for_space(&active, None, None);
        let same_ids: HashSet<String> = same_floor_cands.iter().map(|p| p.id.clone()).collect();
        let excluded = self.excluded_ids();
        let map_id = active.map_id;
        let mut cross_floor_cands: Vec<NavPin> = self
            .pin_order
            .iter()
            .filter(|pid| !excluded.contains(*pid) && !same_ids.contains(*pid))
            .filter_map(|pid| self.pins_by_id.get(pid))
            .filter(|pin| pin.map_id == map_id)
            .cloned()
            .collect();
        let pos = self.last_pos_by_space.get(&active).copied();
        let heading = self.last_heading_by_space.get(&active).copied();
        if !cross_floor_cands.is_empty()
            && let Some((plat, plng)) = pos
        {
            cross_floor_cands.sort_by_key(|p| pin_rank_key(p, plat, plng, heading));
        }

        let mut candidates = same_floor_cands;
        candidates.extend(cross_floor_cands);
        if candidates.is_empty() {
            self.pinned_override_id = None;
            self.selected_by_space.remove(&active);
            return None;
        }

        let cur_idx = current_id
            .as_deref()
            .and_then(|cid| candidates.iter().position(|p| p.id == cid))
            .unwrap_or(0) as isize;
        let len = candidates.len() as isize;
        let next_idx = (cur_idx + offset).rem_euclid(len) as usize;
        let chosen = &candidates[next_idx];
        self.selected_by_space
            .insert(active.clone(), chosen.id.clone());
        self.pinned_override_id = if is_same_floor_for_space(chosen, &active) {
            None
        } else {
            Some(chosen.id.clone())
        };
        Some(chosen.id.clone())
    }

    pub fn next_target(&mut self) -> Option<&NavPin> {
        let pid = self.cycle(1);
        self.user_locked_id = pid.clone();
        pid.and_then(|id| self.pins_by_id.get(&id))
    }

    pub fn prev_target(&mut self) -> Option<&NavPin> {
        let pid = self.cycle(-1);
        self.user_locked_id = pid.clone();
        pid.and_then(|id| self.pins_by_id.get(&id))
    }

    fn proximity_takeover(
        &mut self,
        space_key: &SpaceKey,
        target: NavPin,
        player_pos: (f64, f64),
        view_heading_deg: Option<f64>,
    ) -> NavPin {
        if self.target_lock_pin.is_some()
            || self.route_queue.is_some()
            || self.pinned_override_id.is_some()
        {
            return target;
        }
        if self.user_locked_id.as_deref() == Some(target.id.as_str()) {
            return target;
        }
        if !is_same_floor_for_space(&target, space_key) {
            return target;
        }

        let (plat, plng) = player_pos;
        let heading =
            view_heading_deg.or_else(|| self.last_heading_by_space.get(space_key).copied());
        let (target_dist, target_bearing) =
            compute_hoyolab_bearing(plat, plng, target.lat, target.lng);
        let target_eff =
            pin_effective_distance(&target, target_dist, Some(target_bearing), heading);
        let target_ahead = heading.is_some_and(|h| {
            normalize_angle_delta(target_bearing, h).abs() <= TAKEOVER_TARGET_AHEAD_DEG
        });

        let mut closest_near: Option<NavPin> = None;
        let mut closest_near_key: Option<(f64, i32, String)> = None;
        let mut closest_near_dist = f64::INFINITY;
        let mut best_ranked: Option<NavPin> = None;
        let mut best_rank: Option<PinRankKey> = None;
        let mut cand_bearings: HashMap<String, f64> = HashMap::new();

        for pin in self.eligible_for_space(space_key) {
            if pin.id == target.id || !is_same_floor_for_space(&pin, space_key) {
                continue;
            }
            let (d, b) = compute_hoyolab_bearing(plat, plng, pin.lat, pin.lng);
            cand_bearings.insert(pin.id.clone(), b);
            let tier = pin_priority_tier(&pin);
            let near_key = (d, -tier, pin.id.clone());
            let near_better = match &closest_near_key {
                None => true,
                Some(prev) => {
                    near_key.0.total_cmp(&prev.0)
                        .then_with(|| near_key.1.cmp(&prev.1))
                        .then_with(|| near_key.2.cmp(&prev.2))
                        .is_lt()
                }
            };
            if near_better {
                closest_near_key = Some(near_key);
                closest_near_dist = d;
                closest_near = Some(pin.clone());
            }
            let eff_d = pin_effective_distance(&pin, d, Some(b), heading);
            let rk = PinRankKey {
                eff_dist: eff_d,
                raw_dist: d,
                neg_tier: -tier,
                id: pin.id.clone(),
            };
            if best_rank.as_ref().is_none_or(|prev| rk < *prev) {
                best_rank = Some(rk);
                best_ranked = Some(pin);
            }
        }

        if let Some(ref near) = closest_near
            && closest_near_dist <= PROXIMITY_TAKEOVER_M
            && target_dist > closest_near_dist + PROXIMITY_TAKEOVER_MARGIN_M
        {
            self.selected_by_space
                .insert(space_key.clone(), near.id.clone());
            self.closer_candidate = None;
            return near.clone();
        }

        let mut candidate: Option<NavPin> = None;
        if let Some(ref near) = closest_near {
            let near_behind = heading.is_some_and(|h| {
                cand_bearings
                    .get(&near.id)
                    .is_some_and(|&b| normalize_angle_delta(b, h).abs() > TAKEOVER_CAND_BEHIND_DEG)
            });
            let raw_closer = closest_near_dist <= CLOSER_RATIO * target_dist
                && (target_dist - closest_near_dist) >= CLOSER_MARGIN_M;
            if raw_closer && !(target_ahead && near_behind) {
                candidate = Some(near.clone());
            }
        }
        if candidate.is_none()
            && let (Some(best), Some(rank)) = (best_ranked, best_rank)
        {
            let best_eff = rank.eff_dist;
            let best_dist = rank.raw_dist;
            let best_behind = heading.is_some_and(|h| {
                cand_bearings
                    .get(&best.id)
                    .is_some_and(|&b| normalize_angle_delta(b, h).abs() > TAKEOVER_CAND_BEHIND_DEG)
            });
            let eff_closer = best_dist < target_dist
                && best_eff <= CLOSER_RATIO * target_eff
                && (target_eff - best_eff) >= CLOSER_MARGIN_M;
            if eff_closer && !(target_ahead && best_behind) {
                candidate = Some(best);
            }
        }

        if let Some(cand) = candidate {
            let now = (self.clock)();
            if self.closer_candidate.as_deref() != Some(cand.id.as_str()) {
                self.closer_candidate = Some(cand.id.clone());
                self.closer_since = now;
            } else if (now - self.closer_since) >= CLOSER_DWELL_S {
                self.selected_by_space
                    .insert(space_key.clone(), cand.id.clone());
                self.closer_candidate = None;
                return cand;
            }
            return target;
        }
        self.closer_candidate = None;
        target
    }

    fn relock_after_jump(&mut self, space_key: &SpaceKey) {
        if self.target_lock_pin.is_some()
            || self.route_queue.is_some()
            || self.pinned_override_id.is_some()
        {
            return;
        }
        if let Some(selected) = self.selected_by_space.get(space_key)
            && self.user_locked_id.as_deref() == Some(selected.as_str())
        {
            return;
        }
        self.selected_by_space.remove(space_key);
        self.orders.remove(space_key);
        self.closer_candidate = None;
    }

    fn advance_after_removal(
        &mut self,
        removed_id: &str,
        space_key: &SpaceKey,
        prefer_nearest: bool,
    ) -> Option<String> {
        if self
            .target_lock_pin
            .as_ref()
            .is_some_and(|p| p.id == removed_id)
        {
            self.target_lock_pin = None;
            if self.user_locked_id.as_deref() == Some(removed_id) {
                self.user_locked_id = None;
            }
        }
        if let Some(ref rq) = self.route_queue {
            if self.route_index < rq.len() && rq[self.route_index] == removed_id {
                self.route_index = rq.len().min(self.route_index + 1);
            }
            return self.resolve_target(space_key, None, None).map(|p| p.id);
        }

        if self.pinned_override_id.as_deref() == Some(removed_id) {
            self.pinned_override_id = None;
        }
        if self.user_locked_id.as_deref() == Some(removed_id) {
            self.user_locked_id = None;
        }

        if prefer_nearest {
            let eligible = self.eligible_for_space(space_key);
            let pos = self.last_pos_by_space.get(space_key).copied();
            let heading = self.last_heading_by_space.get(space_key).copied();
            if let Some(nearest) = Self::nearest_of(&eligible, pos, heading) {
                self.selected_by_space
                    .insert(space_key.clone(), nearest.id.clone());
                return self.resolve_target(space_key, None, None).map(|p| p.id);
            }
        }

        let order = self.orders.get(space_key).cloned().unwrap_or_default();
        let idx = order.iter().position(|id| id == removed_id);
        let eligible_ids: HashSet<String> = self
            .eligible_for_space(space_key)
            .into_iter()
            .map(|p| p.id)
            .collect();
        let rotated: Vec<String> = match idx {
            Some(i) => order[i + 1..]
                .iter()
                .chain(order[..=i].iter())
                .cloned()
                .collect(),
            None => order,
        };
        let next_id = rotated.into_iter().find(|pid| eligible_ids.contains(pid));
        match next_id {
            None => {
                self.selected_by_space.remove(space_key);
            }
            Some(nid) => {
                self.selected_by_space.insert(space_key.clone(), nid);
            }
        }
        self.resolve_target(space_key, None, None).map(|p| p.id)
    }

    pub fn skip_current(&mut self) -> Option<&NavPin> {
        if !self.selection_ready {
            return None;
        }
        let target = self.current_target()?.clone();
        let space_key = self
            .active_space
            .clone()
            .unwrap_or_else(|| target.space_key());
        let prev_route_idx = if self.route_queue.is_some() {
            Some(self.route_index)
        } else {
            None
        };
        self.skipped_ids.insert(target.id.clone());
        self.history_pins.insert(target.id.clone(), target.clone());
        self.history.push(HistoryEntry {
            action: HistoryAction::Skip,
            pin_id: target.id.clone(),
            space_key: space_key.clone(),
            prev_override_id: self.pinned_override_id.clone(),
            prev_route_index: prev_route_idx,
        });
        let next_id = self.advance_after_removal(&target.id, &space_key, false);
        next_id.and_then(|id| self.pins_by_id.get(&id))
    }

    pub fn blacklist_current(&mut self) -> Option<&NavPin> {
        if !self.selection_ready {
            return None;
        }
        let target = self.current_target()?.clone();
        let space_key = self
            .active_space
            .clone()
            .unwrap_or_else(|| target.space_key());
        let prev_route_idx = if self.route_queue.is_some() {
            Some(self.route_index)
        } else {
            None
        };
        self.blacklisted_ids.insert(target.id.clone());
        self.history_pins.insert(target.id.clone(), target.clone());
        self.history.push(HistoryEntry {
            action: HistoryAction::Blacklist,
            pin_id: target.id.clone(),
            space_key: space_key.clone(),
            prev_override_id: self.pinned_override_id.clone(),
            prev_route_index: prev_route_idx,
        });
        let next_id = self.advance_after_removal(&target.id, &space_key, false);
        next_id.and_then(|id| self.pins_by_id.get(&id))
    }

    pub fn mark_collected(&mut self, pin_id: Option<&str>) -> Option<&NavPin> {
        if !self.selection_ready {
            return None;
        }
        let target = match pin_id {
            Some(pid) => self.pins_by_id.get(pid)?.clone(),
            None => self.current_target()?.clone(),
        };
        let space_key = self
            .active_space
            .clone()
            .unwrap_or_else(|| target.space_key());
        let prev_route_idx = if self.route_queue.is_some() {
            Some(self.route_index)
        } else {
            None
        };
        let current_id = self.current_target().map(|p| p.id.clone());
        if pin_id.is_some()
            && let Some(ref cur_id) = current_id
            && cur_id != &target.id
        {
            if self
                .target_lock_pin
                .as_ref()
                .is_some_and(|p| p.id == target.id)
            {
                self.clear_target_lock();
            }
            self.collected_ids.insert(target.id.clone());
            self.history_pins.insert(target.id.clone(), target.clone());
            self.history.push(HistoryEntry {
                action: HistoryAction::Collected,
                pin_id: target.id,
                space_key,
                prev_override_id: self.pinned_override_id.clone(),
                prev_route_index: prev_route_idx,
            });
            let cur_owned = cur_id.clone();
            return self.pins_by_id.get(&cur_owned);
        }
        self.collected_ids.insert(target.id.clone());
        self.history_pins.insert(target.id.clone(), target.clone());
        self.history.push(HistoryEntry {
            action: HistoryAction::Collected,
            pin_id: target.id.clone(),
            space_key: space_key.clone(),
            prev_override_id: self.pinned_override_id.clone(),
            prev_route_index: prev_route_idx,
        });
        let next_id = self.advance_after_removal(&target.id, &space_key, true);
        next_id.and_then(|id| self.pins_by_id.get(&id))
    }

    pub fn undo(&mut self) -> Option<&NavPin> {
        let Some(entry) = self.history.pop() else {
            return self.current_target();
        };
        if !self.pins_by_id.contains_key(&entry.pin_id)
            && let Some(hist_pin) = self.history_pins.get(&entry.pin_id).cloned()
        {
            self.pins_by_id.insert(entry.pin_id.clone(), hist_pin);
            self.pin_order.push(entry.pin_id.clone());
        }
        self.protect_pin(&entry.pin_id, DEFAULT_PROTECT_SECONDS);
        match entry.action {
            HistoryAction::Skip => {
                self.skipped_ids.remove(&entry.pin_id);
            }
            HistoryAction::Blacklist => {
                self.blacklisted_ids.remove(&entry.pin_id);
            }
            HistoryAction::Collected => {
                self.collected_ids.remove(&entry.pin_id);
            }
        }
        self.active_space = Some(entry.space_key.clone());
        self.selected_by_space
            .insert(entry.space_key, entry.pin_id);
        self.pinned_override_id = entry.prev_override_id;

        if let (Some(rq), Some(prev_idx)) = (&self.route_queue, entry.prev_route_index)
            && !rq.is_empty()
        {
            self.route_index = prev_idx.min(rq.len() - 1);
        }
        self.current_target()
    }

    pub fn rollback_collected(&mut self, pin_id: &str) -> Option<&NavPin> {
        if !self.collected_ids.contains(pin_id) {
            return None;
        }
        if !self.pins_by_id.contains_key(pin_id)
            && let Some(hist_pin) = self.history_pins.get(pin_id).cloned()
        {
            self.pins_by_id.insert(pin_id.to_owned(), hist_pin);
            self.pin_order.push(pin_id.to_owned());
        }
        self.collected_ids.remove(pin_id);
        let mut matched_entry: Option<HistoryEntry> = None;
        for idx in (0..self.history.len()).rev() {
            if self.history[idx].action == HistoryAction::Collected
                && self.history[idx].pin_id == pin_id
            {
                matched_entry = Some(self.history.remove(idx));
                break;
            }
        }
        self.protect_pin(pin_id, ROLLBACK_PROTECT_SECONDS);
        let pin_space = self.pins_by_id.get(pin_id)?.space_key();
        let target_space = matched_entry
            .as_ref()
            .map(|e| e.space_key.clone())
            .unwrap_or_else(|| self.active_space.clone().unwrap_or(pin_space));
        self.active_space = Some(target_space.clone());
        self.selected_by_space
            .insert(target_space, pin_id.to_owned());
        if let Some(entry) = matched_entry {
            self.pinned_override_id = entry.prev_override_id;
            if let (Some(rq), Some(prev_idx)) = (&self.route_queue, entry.prev_route_index)
                && !rq.is_empty()
            {
                self.route_index = prev_idx.min(rq.len() - 1);
            }
        }
        self.pins_by_id.get(pin_id)
    }

    pub fn refresh_nearest(
        &mut self,
        player_lat: Option<f64>,
        player_lng: Option<f64>,
        player_map_id: Option<i32>,
        player_floor_id: Option<&str>,
        player_is_underground: Option<bool>,
    ) -> Option<&NavPin> {
        if let Some(mid) = player_map_id {
            self.active_space = Some(SpaceKey::new(
                mid,
                player_floor_id,
                player_is_underground.unwrap_or(false),
            ));
        }
        let active = self.active_space.clone()?;
        if let (Some(lat), Some(lng)) = (player_lat, player_lng) {
            self.last_pos_by_space.insert(active.clone(), (lat, lng));
        }
        self.pinned_override_id = None;
        if let Some(ref rq) = self.route_queue {
            let pos = self.last_pos_by_space.get(&active).copied();
            let excluded = self.excluded_ids();
            let mut best_idx: Option<usize> = None;
            let mut best_dist = f64::INFINITY;
            for (idx, pid) in rq.iter().enumerate() {
                if excluded.contains(pid) {
                    continue;
                }
                let Some(pin) = self.pins_by_id.get(pid) else {
                    continue;
                };
                let dist = match pos {
                    Some((plat, plng)) => (pin.lng - plng).hypot(pin.lat - plat),
                    None => idx as f64,
                };
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = Some(idx);
                }
            }
            if let Some(idx) = best_idx {
                self.route_index = idx;
                let pid = self.route_queue.as_ref()?[idx].clone();
                return self.pins_by_id.get(&pid);
            }
            return None;
        }

        self.orders.remove(&active);
        self.selected_by_space.remove(&active);
        let pos = self.last_pos_by_space.get(&active).copied();
        let heading = self.last_heading_by_space.get(&active).copied();
        let resolved_id = self.resolve_target(&active, pos, heading).map(|p| p.id)?;
        self.pins_by_id.get(&resolved_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compute_guidance(
        &mut self,
        player_lat: f64,
        player_lng: f64,
        view_heading_deg: Option<f64>,
        player_map_id: i32,
        player_floor_id: Option<&str>,
        player_is_underground: bool,
        tracking_state: &str,
        player_floor_name: &str,
    ) -> NavGuidance {
        let norm_state = if tracking_state.trim().is_empty() {
            "TRACKING".to_owned()
        } else {
            tracking_state.trim().to_ascii_uppercase()
        };
        let is_tracking = norm_state == "TRACKING";
        self.selection_ready = is_tracking;

        let norm_p_floor = norm_floor_id(player_floor_id);
        let norm_p_floor_name = player_floor_name.trim().to_owned();
        let mut space_key =
            SpaceKey::new(player_map_id, norm_p_floor.as_deref(), player_is_underground);

        if !is_tracking {
            let target = self.current_target().cloned();
            let rp = self.route_progress();
            let remaining = self.summary().remaining;
            let stop_kind = target
                .as_ref()
                .and_then(|t| t.stop_kind.clone())
                .unwrap_or_default();
            let sec_label = self
                .active_route
                .as_ref()
                .map(|_doc| format!("S{}", self.active_section_idx + 1))
                .unwrap_or_default();
            let target_mid = self
                .target_lock_pin
                .as_ref()
                .map(|p| p.map_id)
                .or_else(|| self.active_route.as_ref().map(|d| d.map_id))
                .filter(|&mid| mid != player_map_id);
            let is_single_pin_locked = self.target_lock_pin.is_some();
            let guidance = NavGuidance {
                state: NavState::Lost,
                target_floor_id: target.as_ref().and_then(|t| t.floor_id.clone()),
                target_floor_name: target
                    .as_ref()
                    .map(|t| t.floor_name_str().to_owned())
                    .unwrap_or_default(),
                target_is_underground: target.as_ref().is_some_and(|t| t.is_underground),
                target,
                distance_m: 0.0,
                relative_bearing_deg: 0.0,
                target_distance_m: None,
                bearing_deg: None,
                delta_deg: None,
                floor_mismatch: false,
                is_route_mode: self.is_route_mode(),
                player_floor_id: norm_p_floor,
                player_is_underground,
                route_mode: NavRouteMode::None,
                route_target_name: String::new(),
                route_target_lat: None,
                route_target_lng: None,
                show_spot_card: false,
                is_locked_on: false,
                is_behind: false,
                route_step: rp.map(|r| r.0),
                route_total: rp.map(|r| r.1),
                remaining_count: remaining,
                player_floor_name: norm_p_floor_name,
                route_target_map_id: target_mid,
                route_section_label: sec_label,
                route_stop_kind: stop_kind,
                is_single_pin_locked,
            };
            self.last_guidance = Some(guidance.clone());
            return guidance;
        }

        if let Some(target_lock) = self.locked_target_pin().cloned()
            && target_lock.map_id != player_map_id
        {
            let rp = self.route_progress();
            let remaining = self.summary().remaining;
            let guidance = NavGuidance {
                state: NavState::Idle,
                target_floor_id: target_lock.floor_id.clone(),
                target_floor_name: target_lock.floor_name_str().to_owned(),
                target_is_underground: target_lock.is_underground,
                route_target_name: target_lock.display_name(),
                route_target_lat: Some(target_lock.lat),
                route_target_lng: Some(target_lock.lng),
                route_target_map_id: Some(target_lock.map_id),
                target: Some(target_lock),
                distance_m: 0.0,
                relative_bearing_deg: 0.0,
                target_distance_m: None,
                bearing_deg: None,
                delta_deg: None,
                floor_mismatch: false,
                is_route_mode: self.is_route_mode(),
                player_floor_id: norm_p_floor,
                player_is_underground,
                route_mode: NavRouteMode::Waypoint,
                show_spot_card: false,
                is_locked_on: false,
                is_behind: false,
                route_step: rp.map(|r| r.0),
                route_total: rp.map(|r| r.1),
                remaining_count: remaining,
                player_floor_name: norm_p_floor_name,
                route_section_label: String::new(),
                route_stop_kind: String::new(),
                is_single_pin_locked: true,
            };
            self.last_guidance = Some(guidance.clone());
            return guidance;
        }

        if self.target_lock_pin.is_none()
            && let Some(ref doc) = self.active_route
            && doc.map_id != player_map_id
        {
            let rp = self.route_progress();
            let remaining = self.summary().remaining;
            let map_label = if doc.map_name.trim().is_empty() {
                format!("Map {}", doc.map_id)
            } else {
                doc.map_name.clone()
            };
            let sec_label = format!("S{}", self.active_section_idx + 1);
            let guidance = NavGuidance {
                state: NavState::Idle,
                target: None,
                distance_m: 0.0,
                relative_bearing_deg: 0.0,
                target_distance_m: None,
                bearing_deg: None,
                delta_deg: None,
                floor_mismatch: false,
                is_route_mode: true,
                player_floor_id: norm_p_floor,
                player_is_underground,
                target_floor_id: None,
                target_floor_name: String::new(),
                target_is_underground: false,
                route_mode: NavRouteMode::Waypoint,
                route_target_name: map_label,
                route_target_lat: None,
                route_target_lng: None,
                show_spot_card: false,
                is_locked_on: false,
                is_behind: false,
                route_step: rp.map(|r| r.0),
                route_total: rp.map(|r| r.1),
                remaining_count: remaining,
                player_floor_name: norm_p_floor_name,
                route_target_map_id: Some(doc.map_id),
                route_section_label: sec_label,
                route_stop_kind: String::new(),
                is_single_pin_locked: false,
            };
            self.last_guidance = Some(guidance.clone());
            return guidance;
        }

        let player_pos = (player_lat, player_lng);
        self.last_player_floor = Some((player_map_id, norm_p_floor.clone()));
        if self.space_debounce_frames > 0
            && let Some(cur) = self.active_space.clone()
            && space_key != cur
            && space_key.map_id == cur.map_id
        {
            if self.pending_space.as_ref() == Some(&space_key) {
                self.pending_space_frames += 1;
            } else {
                self.pending_space = Some(space_key.clone());
                self.pending_space_frames = 1;
            }
            if self.pending_space_frames < self.space_debounce_frames {
                space_key = cur;
            }
        } else {
            self.pending_space = None;
            self.pending_space_frames = 0;
        }

        if self.active_space.as_ref() != Some(&space_key) {
            self.orders.remove(&space_key);
        }
        self.active_space = Some(space_key.clone());

        if let Some((prev_lat, prev_lng)) = self.last_pos_by_space.get(&space_key).copied()
            && (player_pos.0 - prev_lat).hypot(player_pos.1 - prev_lng) > TELEPORT_JUMP_M
        {
            self.relock_after_jump(&space_key);
        }
        self.last_pos_by_space.insert(space_key.clone(), player_pos);

        let mut eff_heading: Option<f64> = None;
        if let Some(cand_h) = view_heading_deg.filter(|h| h.is_finite()) {
            eff_heading = Some(cand_h);
            self.last_heading_by_space.insert(space_key.clone(), cand_h);
        }
        if eff_heading.is_none() {
            eff_heading = self.last_heading_by_space.get(&space_key).copied();
        }

        let mut target = self.resolve_target(&space_key, Some(player_pos), eff_heading);
        if let Some(t) = target {
            target = Some(self.proximity_takeover(&space_key, t, player_pos, eff_heading));
        }

        let rp = self.route_progress();
        let remaining = self.summary().remaining;
        let sec_label = self
            .active_route
            .as_ref()
            .map(|_| format!("S{}", self.active_section_idx + 1))
            .unwrap_or_default();

        let Some(target) = target else {
            let guidance = NavGuidance {
                state: NavState::Idle,
                target: None,
                distance_m: 0.0,
                relative_bearing_deg: 0.0,
                target_distance_m: None,
                bearing_deg: None,
                delta_deg: None,
                floor_mismatch: false,
                is_route_mode: self.is_route_mode(),
                player_floor_id: norm_p_floor,
                player_is_underground,
                target_floor_id: None,
                target_floor_name: String::new(),
                target_is_underground: false,
                route_mode: NavRouteMode::None,
                route_target_name: String::new(),
                route_target_lat: None,
                route_target_lng: None,
                show_spot_card: false,
                is_locked_on: false,
                is_behind: false,
                route_step: rp.map(|r| r.0),
                route_total: rp.map(|r| r.1),
                remaining_count: remaining,
                player_floor_name: norm_p_floor_name,
                route_target_map_id: None,
                route_section_label: sec_label,
                route_stop_kind: String::new(),
                is_single_pin_locked: self.target_lock_pin.is_some(),
            };
            self.last_guidance = Some(guidance.clone());
            return guidance;
        };

        let (target_dist, direct_bearing) =
            compute_hoyolab_bearing(player_lat, player_lng, target.lat, target.lng);
        let floor_mismatch =
            is_floor_mismatch(&target, norm_p_floor.as_deref(), player_is_underground);

        let stop_kind = target.stop_kind.clone().unwrap_or_default();
        let route_mode = if stop_kind == "teleport" || stop_kind == "waypoint" {
            NavRouteMode::Waypoint
        } else {
            NavRouteMode::Direct
        };
        let route_name = target
            .route_section_name
            .clone()
            .unwrap_or_default();
        let dest_lat = target.lat;
        let dest_lng = target.lng;
        let active_dist = target_dist;
        let active_bearing = direct_bearing;

        let heading = view_heading_deg.filter(|h| h.is_finite()).unwrap_or(0.0);
        let delta_deg = normalize_angle_delta(active_bearing, heading);

        let state = if floor_mismatch {
            NavState::FloorMismatch
        } else if target_dist <= self.arrived_distance_m {
            NavState::Arrived
        } else {
            NavState::Tracking
        };

        let show_spot_card =
            target.has_spot_content() && active_dist.min(target_dist) <= self.spot_card_distance_m;
        let is_locked_on = delta_deg.abs() <= LOCK_ON_ANGLE_DEG;
        let is_behind = delta_deg.abs() > BEHIND_ANGLE_DEG;
        let is_single_pin_locked = self
            .target_lock_pin
            .as_ref()
            .is_some_and(|p| p.id == target.id);

        let guidance = NavGuidance {
            state,
            target_floor_id: target.floor_id.clone(),
            target_floor_name: target.floor_name_str().to_owned(),
            target_is_underground: target.is_underground,
            target: Some(target),
            distance_m: active_dist,
            relative_bearing_deg: delta_deg,
            target_distance_m: Some(target_dist),
            bearing_deg: Some(active_bearing),
            delta_deg: Some(delta_deg),
            floor_mismatch,
            is_route_mode: self.is_route_mode(),
            player_floor_id: norm_p_floor,
            player_is_underground,
            route_mode,
            route_target_name: route_name,
            route_target_lat: Some(dest_lat),
            route_target_lng: Some(dest_lng),
            show_spot_card,
            is_locked_on,
            is_behind,
            route_step: rp.map(|r| r.0),
            route_total: rp.map(|r| r.1),
            remaining_count: remaining,
            player_floor_name: norm_p_floor_name,
            route_target_map_id: None,
            route_section_label: sec_label,
            route_stop_kind: stop_kind,
            is_single_pin_locked,
        };
        self.last_guidance = Some(guidance.clone());
        guidance
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct HoldStatus {
    pub active: bool,
    pub progress: f64,
    pub confirmed: bool,
    pub cancelled: bool,
}

pub struct CollectedHoldController {
    pub duration_seconds: f64,
    started_at: Option<f64>,
    target_id: Option<String>,
    origin: Instant,
}

impl Default for CollectedHoldController {
    fn default() -> Self {
        Self {
            duration_seconds: DEFAULT_HOLD_DURATION_SECONDS,
            started_at: None,
            target_id: None,
            origin: Instant::now(),
        }
    }
}

impl CollectedHoldController {
    pub fn new(duration_seconds: f64) -> CoreResult<Self> {
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Err(CoreError::InvalidArgument(
                "Hold duration must be positive and finite".to_owned(),
            ));
        }
        Ok(Self {
            duration_seconds,
            started_at: None,
            target_id: None,
            origin: Instant::now(),
        })
    }

    pub fn is_active(&self) -> bool {
        self.started_at.is_some()
    }

    pub fn active_target_id(&self) -> Option<&str> {
        self.target_id.as_deref()
    }

    fn monotonic_now(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    pub fn begin(&mut self, target_id: Option<&str>, available: bool, now: Option<f64>) -> bool {
        let Some(tid) = target_id else {
            return false;
        };
        if !available {
            return false;
        }
        self.target_id = Some(tid.to_owned());
        self.started_at = Some(now.unwrap_or_else(|| self.monotonic_now()));
        true
    }

    pub fn cancel(&mut self) {
        self.started_at = None;
        self.target_id = None;
    }

    pub fn update(
        &mut self,
        key_down: bool,
        target_id: Option<&str>,
        available: bool,
        now: Option<f64>,
    ) -> HoldStatus {
        let Some(started) = self.started_at else {
            return HoldStatus::default();
        };
        if !key_down || !available || target_id != self.target_id.as_deref() {
            self.cancel();
            return HoldStatus {
                active: false,
                progress: 0.0,
                confirmed: false,
                cancelled: true,
            };
        }
        let current_time = now.unwrap_or_else(|| self.monotonic_now());
        let elapsed = current_time - started;
        let progress = (elapsed / self.duration_seconds).clamp(0.0, 1.0);
        if progress >= 1.0 {
            self.cancel();
            return HoldStatus {
                active: false,
                progress: 1.0,
                confirmed: true,
                cancelled: false,
            };
        }
        HoldStatus {
            active: true,
            progress,
            confirmed: false,
            cancelled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_pin(id: &str, name: &str, lat: f64, lng: f64) -> NavPin {
        NavPin::new(id, name, lat, lng)
    }

    #[test]
    fn crs_bearings_and_angle_deltas() {
        let cases = [
            ("North", -100.0, 0.0, 0.0),
            ("East", 0.0, 100.0, 90.0),
            ("South", 100.0, 0.0, 180.0),
            ("West", 0.0, -100.0, 270.0),
            ("NE", -100.0, 100.0, 45.0),
            ("SE", 100.0, 100.0, 135.0),
            ("SW", 100.0, -100.0, 225.0),
            ("NW", -100.0, -100.0, 315.0),
        ];
        for (label, tlat, tlng, expected) in cases {
            let (dist, bearing) = compute_hoyolab_bearing(0.0, 0.0, tlat, tlng);
            assert!(
                (bearing - expected).abs() < 1e-6,
                "{label}: expected {expected}, got {bearing}"
            );
            assert!(dist > 99.0);
        }

        assert!((normalize_angle_delta(0.0, 0.0) - 0.0).abs() < 1e-6);
        assert!((normalize_angle_delta(90.0, 0.0) - 90.0).abs() < 1e-6);
        assert!((normalize_angle_delta(270.0, 0.0) - (-90.0)).abs() < 1e-6);
        assert!((normalize_angle_delta(180.0, 0.0).abs() - 180.0).abs() < 1e-6);
        assert!((normalize_angle_delta(0.0, 270.0) - 90.0).abs() < 1e-6);
        assert!((normalize_angle_delta(355.0, 5.0) - (-10.0)).abs() < 1e-6);
        assert!((normalize_angle_delta(5.0, 355.0) - 10.0).abs() < 1e-6);

        for (label, tlat, tlng, expected) in &cases[..4] {
            let mut ctrl = NavigationController::with_pins(vec![make_pin(label, label, *tlat, *tlng)]);
            let g = ctrl.compute_guidance(0.0, 0.0, Some(*expected), 2, None, false, "TRACKING", "");
            assert_eq!(g.state, NavState::Tracking);
            assert!((g.bearing_deg.unwrap() - expected).abs() < 1e-6);
            assert!(g.delta_deg.unwrap().abs() < 1e-6);
            assert!(g.is_locked_on);
            assert!(!g.is_behind);
        }

        let mut ctrl_behind = NavigationController::with_pins(vec![make_pin("b", "B", 100.0, 0.0)]);
        let gb = ctrl_behind.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert!(gb.is_behind);
        assert!(!gb.is_locked_on);
    }

    #[test]
    fn smart_priority_tiers_and_camera_ranking() {
        let mut oculus = make_pin("oc", "Băng Thần Đồng", -33.0, 0.0);
        oculus.category = "Băng Thần Đồng".to_owned();
        oculus.label_id = Some(833);
        oculus.parent_label_id = Some(4);

        let mut shrine = make_pin("sh", "Đền Địa Linh", -30.0, 0.0);
        shrine.category = "Đền Địa Linh Snezhnaya".to_owned();
        shrine.label_id = Some(835);
        shrine.parent_label_id = Some(186);

        let mut luxurious = make_pin("lx", "Rương Hiếm", -30.0, 0.0);
        luxurious.category = "Rương Hiếm".to_owned();
        luxurious.label_id = Some(46);
        luxurious.parent_label_id = Some(13);

        let mut snezh_chest = make_pin("sc", "Rương Snezhnaya", -30.0, 0.0);
        snezh_chest.category = "Rương Snezhnaya".to_owned();
        snezh_chest.label_id = Some(838);
        snezh_chest.parent_label_id = Some(13);

        let mut common = make_pin("cc", "Rương Thường", -25.0, 0.0);
        common.category = "Rương Thường".to_owned();
        common.label_id = Some(17);
        common.parent_label_id = Some(13);

        let mut puzzle = make_pin("pz", "Câu đố nhỏ", -23.0, 0.0);
        puzzle.category = "Câu đố nhỏ".to_owned();
        puzzle.label_id = Some(77);
        puzzle.parent_label_id = Some(186);

        let mut trial = make_pin("tr", "Khiêu Chiến Hạn Giờ", -25.0, 0.0);
        trial.category = "Khiêu Chiến Hạn Giờ".to_owned();
        trial.label_id = Some(64);
        trial.parent_label_id = Some(186);

        let mut spincrystal = make_pin("sp", "Ngọc Thạch Âm Vang", -25.0, 0.0);
        spincrystal.category = "Ngọc Thạch Âm Vang".to_owned();
        spincrystal.label_id = Some(356);
        spincrystal.parent_label_id = Some(4);

        let mut enemy = make_pin("en", "Kẻ địch", -30.0, 0.0);
        enemy.category = "Fatui".to_owned();
        enemy.label_id = Some(999);
        enemy.parent_label_id = Some(50);

        assert_eq!(pin_priority_tier(&oculus), 6);
        assert_eq!(pin_priority_tier(&shrine), 5);
        assert_eq!(pin_priority_tier(&luxurious), 5);
        assert_eq!(pin_priority_tier(&snezh_chest), 4);
        assert_eq!(pin_priority_tier(&common), 3);
        assert_eq!(pin_priority_tier(&puzzle), 2);
        assert_eq!(pin_priority_tier(&trial), 2);
        assert_eq!(pin_priority_tier(&spincrystal), 1);
        assert_eq!(pin_priority_tier(&enemy), 0);

        let roundtrip = NavPin::from_value(&oculus.to_wire_json()).expect("roundtrip");
        assert_eq!(roundtrip.label_id, Some(833));
        assert_eq!(roundtrip.parent_label_id, Some(4));

        // 28m oculus beats 25m puzzle; 33m oculus loses to 25m puzzle
        let mut p_pz25 = puzzle.clone();
        p_pz25.id = "pz25".to_owned();
        p_pz25.lat = -25.0;
        let mut oc_28 = oculus.clone();
        oc_28.id = "oc28".to_owned();
        oc_28.lat = -28.0;
        assert_eq!(
            NavigationController::with_pins(vec![p_pz25.clone(), oc_28.clone()])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "oc28"
        );
        assert_eq!(
            NavigationController::with_pins(vec![p_pz25.clone(), oculus.clone()])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz25"
        );
        assert!(
            pin_effective_distance(&oc_28, 28.0, None, None)
                < pin_effective_distance(&p_pz25, 25.0, None, None)
        );

        // 20m puzzle beats 40m oculus
        let mut p_pz20 = puzzle.clone();
        p_pz20.id = "pz20".to_owned();
        p_pz20.lat = -20.0;
        let mut oc_40 = oculus.clone();
        oc_40.id = "oc40".to_owned();
        oc_40.lat = -40.0;
        assert_eq!(
            NavigationController::with_pins(vec![oc_40, p_pz20])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz20"
        );

        // 4m puzzle beats 12m oculus
        let mut p_pz4 = puzzle.clone();
        p_pz4.id = "pz4".to_owned();
        p_pz4.lat = -4.0;
        let mut oc_12 = oculus.clone();
        oc_12.id = "oc12".to_owned();
        oc_12.lat = -12.0;
        assert_eq!(
            NavigationController::with_pins(vec![oc_12, p_pz4])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz4"
        );

        // 24m common chest beats 23m puzzle; 25.5m puzzle beats 25m spincrystal
        let mut cc_24 = common.clone();
        cc_24.id = "cc24".to_owned();
        cc_24.lat = -24.0;
        assert_eq!(
            NavigationController::with_pins(vec![puzzle.clone(), cc_24])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "cc24"
        );
        let mut pz_255 = puzzle;
        pz_255.id = "pz255".to_owned();
        pz_255.lat = -25.5;
        assert_eq!(
            NavigationController::with_pins(vec![spincrystal, pz_255])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz255"
        );

        // Camera direction tie-break at equal 30m: ahead common chest beats behind oculus
        let mut ahead_cc = common;
        ahead_cc.id = "cc_ahead".to_owned();
        ahead_cc.lat = -30.0;
        let mut behind_oc = oculus;
        behind_oc.id = "oc_behind".to_owned();
        behind_oc.lat = 30.0;
        behind_oc.label_id = Some(2);
        assert_eq!(
            NavigationController::with_pins(vec![behind_oc.clone(), ahead_cc.clone()])
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "cc_ahead"
        );
        assert_eq!(
            NavigationController::with_pins(vec![behind_oc, ahead_cc])
                .compute_guidance(0.0, 0.0, Some(180.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "oc_behind"
        );
    }

    #[test]
    fn sticky_target_commands_and_undo() {
        let p1 = make_pin("p1", "Rương Thường 1", -30.0, 0.0);
        let p2 = make_pin("p2", "Rương Hiếm 2", -60.0, 0.0);
        let p3 = make_pin("p3", "Thần Đồng 3", -90.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![p1, p2, p3]);

        let g0 = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g0.target.as_ref().unwrap().id, "p1");
        assert_eq!(g0.state, NavState::Tracking);

        for walk_lat in [-35.0, -40.0, -45.0, -48.0] {
            let gm = ctrl.compute_guidance(walk_lat, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
            assert_eq!(gm.target.as_ref().unwrap().id, "p1");
        }

        // Proximity takeover at -85 (p3 is 5m away, p1 is 55m away)
        let g_take = ctrl.compute_guidance(-85.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_take.target.as_ref().unwrap().id, "p3");

        let refreshed = ctrl.refresh_nearest(None, None, None, None, None).unwrap();
        assert_eq!(refreshed.id, "p3");
        let g_ref = ctrl.compute_guidance(-85.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_ref.state, NavState::Arrived);

        assert_eq!(ctrl.next_target().unwrap().id, "p2");
        assert_eq!(ctrl.next_target().unwrap().id, "p1");
        assert_eq!(ctrl.prev_target().unwrap().id, "p2");

        let after_skip = ctrl.skip_current().unwrap().id.clone();
        assert!(ctrl.skipped_ids().contains("p2"));
        assert!(!ctrl.collected_ids().contains("p2"));
        assert_eq!(after_skip, "p1");

        let after_bl = ctrl.blacklist_current().unwrap().id.clone();
        assert!(ctrl.blacklisted_ids().contains("p1"));
        assert_eq!(after_bl, "p3");

        assert!(ctrl.mark_collected(None).is_none());
        assert!(ctrl.collected_ids().contains("p3"));
        let g_idle = ctrl.compute_guidance(-85.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_idle.state, NavState::Idle);
        assert!(g_idle.target.is_none());

        assert_eq!(ctrl.undo().unwrap().id, "p3");
        assert!(!ctrl.collected_ids().contains("p3"));
        assert_eq!(ctrl.undo().unwrap().id, "p1");
        assert!(!ctrl.blacklisted_ids().contains("p1"));
        assert_eq!(ctrl.undo().unwrap().id, "p2");
        assert!(!ctrl.skipped_ids().contains("p2"));

        let g_lost = ctrl.compute_guidance(-85.0, 0.0, Some(0.0), 2, None, false, "LOST", "");
        assert_eq!(g_lost.state, NavState::Lost);
        assert!(!g_lost.show_spot_card);
        assert_eq!(g_lost.target_distance_m, None);
    }

    #[test]
    fn feed_churn_coverage_and_undo_reinsert() {
        let mut near = make_pin("near", "Map Pin", -30.0, 0.0);
        near.category = "Rương Tinh Xảo".to_owned();
        let others: Vec<NavPin> = (0..30)
            .map(|i| {
                let angle = 2.0 * PI * (i as f64) / 30.0;
                let mut p = make_pin(
                    &format!("{}", 100 + i),
                    "Map Pin",
                    -angle.cos() * 120.0,
                    angle.sin() * 120.0,
                );
                p.category = "Rương Thường".to_owned();
                p
            })
            .collect();

        let mut ctrl = NavigationController::new();
        ctrl.set_clock(|| 0.0);
        let mut init_pins = vec![near.clone()];
        init_pins.extend(others[..24].iter().cloned());
        ctrl.set_pins(init_pins, true, Some((0.0, 0.0, 120.0)));
        assert_eq!(
            ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "near"
        );

        for step_lng in (15..=300).step_by(15) {
            let gw = ctrl.compute_guidance(0.0, step_lng as f64, Some(0.0), 2, None, false, "TRACKING", "");
            assert_eq!(gw.target.unwrap().id, "near");
        }
        ctrl.set_pins(others[..25].to_vec(), false, Some((0.0, 300.0, 200.0)));
        assert_eq!(
            ctrl.compute_guidance(0.0, 300.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "near"
        );

        // 40 subset churn feeds outside coverage
        let mut seed: u64 = 7;
        for _ in 0..40 {
            let mut pool = others.clone();
            for i in (1..pool.len()).rev() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let j = (seed as usize) % (i + 1);
                pool.swap(i, j);
            }
            ctrl.set_pins(pool[..25].to_vec(), false, Some((0.0, 300.0, 150.0)));
            let g3 = ctrl.compute_guidance(0.5, 300.5, Some(0.0), 2, None, false, "TRACKING", "");
            assert_eq!(g3.target.unwrap().id, "near");
        }

        // Now searched around (-30, 0) and near is missing -> dropped
        ctrl.set_pins(others[..25].to_vec(), false, Some((-30.0, 0.0, 200.0)));
        assert_ne!(
            ctrl.compute_guidance(-28.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "near"
        );

        // Legacy without coverage drops plain target
        let mut legacy = NavigationController::with_pins(vec![near]);
        legacy.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        legacy.set_pins(others[..3].to_vec(), true, None);
        assert_ne!(legacy.current_target().unwrap().id, "near");

        // Undo re-inserts & protects web-dropped pin + rollback_collected
        let p = make_pin("p", "P", -5.0, 0.0);
        let q = make_pin("q", "Q", -80.0, 0.0);
        let mut c_undo = NavigationController::new();
        c_undo.set_pins(vec![p, q.clone()], true, Some((0.0, 0.0, 200.0)));
        c_undo.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        c_undo.mark_collected(Some("p"));
        c_undo.set_pins(vec![q.clone()], false, Some((0.0, 0.0, 200.0)));
        assert!(c_undo.get_pin("p").is_some());
        assert_eq!(c_undo.peek_history(), Some(("collected", "p")));
        assert_eq!(c_undo.undo().unwrap().id, "p");
        c_undo.set_pins(vec![q], false, Some((0.0, 0.0, 200.0)));
        assert_eq!(
            c_undo
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "p"
        );

        let cc24 = make_pin("cc24", "CC", -24.0, 0.0);
        let pz = make_pin("pz", "PZ", -50.0, 0.0);
        let mut c_rb = NavigationController::new();
        c_rb.set_pins(vec![cc24, pz.clone()], true, Some((0.0, 0.0, 200.0)));
        assert_eq!(
            c_rb.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "cc24"
        );
        c_rb.mark_collected(Some("cc24"));
        assert_eq!(c_rb.current_target().unwrap().id, "pz");
        c_rb.set_pins(vec![pz], false, Some((0.0, 0.0, 200.0)));
        let restored = c_rb.rollback_collected("cc24").unwrap();
        assert_eq!(restored.id, "cc24");
        assert!(!c_rb.collected_ids().contains("cc24"));
        assert_eq!(c_rb.current_target().unwrap().id, "cc24");
    }

    #[test]
    fn takeover_dwell_teleport_and_debounce() {
        let puzzle = make_pin("pz", "Puzzle", -250.0, 0.0);
        let oculus = make_pin("oc", "Oculus", -5.0, 20.0);
        let far = make_pin("far", "Far", 900.0, 900.0);

        // Hand-pick resists takeover
        let mut hand = NavigationController::with_pins(vec![
            make_pin("pz", "Puzzle", -60.0, 0.0),
            make_pin("ch", "Chest", -5.0, 30.0),
        ]);
        hand.select_target("pz");
        hand.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(
            hand.compute_guidance(-5.0, 26.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz"
        );

        // 0.5s dwell clearly-closer re-lock
        let ticks = Arc::new(AtomicU64::new(0));
        let ticks_c = Arc::clone(&ticks);
        let mut ctrl = NavigationController::with_pins(vec![puzzle.clone(), oculus.clone()]);
        ctrl.set_clock(move || f64::from_bits(ticks_c.load(Ordering::Relaxed)));
        ctrl.select_target("pz");
        ctrl.user_locked_id = None;
        assert_eq!(
            ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz"
        );
        ticks.store(0.3_f64.to_bits(), Ordering::Relaxed);
        assert_eq!(
            ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz"
        );
        ticks.store(0.6_f64.to_bits(), Ordering::Relaxed);
        assert_eq!(
            ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "oc"
        );

        // 43m vs 50m does not flip
        let t2 = Arc::new(AtomicU64::new(0));
        let t2_c = Arc::clone(&t2);
        let mut c2 = NavigationController::with_pins(vec![
            make_pin("a", "A", -50.0, 0.0),
            make_pin("b", "B", 0.0, 43.0),
        ]);
        c2.set_clock(move || f64::from_bits(t2_c.load(Ordering::Relaxed)));
        c2.select_target("a");
        c2.user_locked_id = None;
        for k in 0..10 {
            t2.store((k as f64 * 0.5).to_bits(), Ordering::Relaxed);
            assert_eq!(
                c2.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                    .target
                    .unwrap()
                    .id,
                "a"
            );
        }

        // Behind >110 deg blocks dwell flip when target ahead <=75 deg
        let t_ahead = make_pin("ta", "Ahead", -30.0, 0.0);
        let t_behind = make_pin("tb", "Behind", 18.0, 0.0);
        let t3 = Arc::new(AtomicU64::new(0));
        let t3_c = Arc::clone(&t3);
        let mut c_behind = NavigationController::with_pins(vec![t_ahead, t_behind]);
        c_behind.set_clock(move || f64::from_bits(t3_c.load(Ordering::Relaxed)));
        c_behind.select_target("ta");
        c_behind.user_locked_id = None;
        assert_eq!(
            c_behind
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "ta"
        );
        t3.store(0.8_f64.to_bits(), Ordering::Relaxed);
        assert_eq!(
            c_behind
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "ta"
        );
        // Walking underfoot (<=12m) triggers immediate takeover
        assert_eq!(
            c_behind
                .compute_guidance(10.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "tb"
        );

        // Teleport > 50m drops auto-lock, keeps hand-pick
        let mut c4 = NavigationController::with_pins(vec![puzzle, oculus, far]);
        c4.set_clock(|| 0.0);
        assert_eq!(
            c4.compute_guidance(-240.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "pz"
        );
        assert_eq!(
            c4.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "oc"
        );
        c4.select_target("far");
        assert_eq!(
            c4.compute_guidance(-240.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "far"
        );

        // Space debounce frames = 4
        let sf = make_pin("sf", "Surface", -50.0, 0.0);
        let mut ug = make_pin("ug", "Cave", -20.0, 0.0);
        ug.floor_id = Some("7".to_owned());
        ug.is_underground = true;
        let mut deb = NavigationController::with_pins(vec![sf, ug]).with_space_debounce_frames(4);
        assert_eq!(
            deb.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "sf"
        );
        for _ in 0..3 {
            assert_eq!(
                deb.compute_guidance(0.0, 0.0, Some(0.0), 2, Some("7"), true, "TRACKING", "")
                    .target
                    .unwrap()
                    .id,
                "sf"
            );
            assert_eq!(
                deb.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                    .target
                    .unwrap()
                    .id,
                "sf"
            );
        }
        let mut last = NavGuidance::default();
        for _ in 0..4 {
            last = deb.compute_guidance(0.0, 0.0, Some(0.0), 2, Some("7"), true, "TRACKING", "");
        }
        assert_eq!(last.target.unwrap().id, "ug");
    }

    #[test]
    fn tsp_route_queue_step_progress_and_undo() {
        let p1 = make_pin("r1", "Route Stop 1", -100.0, 0.0);
        let p2 = make_pin("r2", "Route Stop 2", -200.0, 0.0);
        let p3 = make_pin("r3", "Route Stop 3", -5.0, 0.0);
        let mut ctrl = NavigationController::with_pins(vec![p1.clone(), p2.clone(), p3.clone()]);
        ctrl.set_route_queue(vec![p1, p2, p3]);

        let g1 = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g1.target.unwrap().id, "r1");
        assert_eq!(g1.route_step, Some(1));
        assert_eq!(g1.route_total, Some(3));
        assert!(g1.is_route_mode);

        assert_eq!(ctrl.mark_collected(None).unwrap().id, "r2");
        let g2 = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g2.target.unwrap().id, "r2");
        assert_eq!(g2.route_step, Some(2));

        assert_eq!(ctrl.mark_collected(None).unwrap().id, "r3");
        assert_eq!(ctrl.route_progress(), Some((3, 3)));

        assert_eq!(ctrl.undo().unwrap().id, "r2");
        assert_eq!(ctrl.route_progress(), Some((2, 3)));

        ctrl.clear_route_queue();
        assert!(!ctrl.is_route_mode());
        assert_eq!(ctrl.route_progress(), None);
    }

    #[test]
    fn floor_mismatch_normalization_and_spot_card() {
        let mut ug_pin = make_pin("ug1", "Rương Ngầm", -5.0, 0.0);
        ug_pin.floor_id = Some("F_B1".to_owned());
        ug_pin.floor_name = Some("B1".to_owned());
        ug_pin.is_underground = true;
        ug_pin.image_url = "https://example.invalid/spot1.jpg".to_owned();
        ug_pin.description = "Desc".to_owned();

        let mut ctrl = NavigationController::with_pins(vec![ug_pin]);
        let g_mis = ctrl.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_mis.state, NavState::FloorMismatch);
        assert!(g_mis.floor_mismatch);
        assert_eq!(g_mis.route_mode, NavRouteMode::Direct);
        assert!((g_mis.distance_m - 5.0).abs() < 1e-6);
        assert!(g_mis.show_spot_card);

        // Untagged underground pin matches floor_id="184"
        let sf = make_pin("sf1", "Surface", -40.0, 0.0);
        let mut ug_nt = make_pin("ug_nt", "Untagged UG", -12.0, 0.0);
        ug_nt.is_underground = true;
        let mut c_ut = NavigationController::with_pins(vec![sf.clone(), ug_nt]);
        let g_ut = c_ut.compute_guidance(0.0, 0.0, Some(0.0), 2, Some("184"), true, "TRACKING", "B1");
        assert_eq!(g_ut.target.unwrap().id, "ug_nt");
        assert!(!g_ut.floor_mismatch);

        // Cross-floor cycling
        let mut b1 = make_pin("b1_1", "B1 Pin", -15.0, 0.0);
        b1.floor_id = Some("F_B1".to_owned());
        b1.is_underground = true;
        let mut c_scope = NavigationController::with_pins(vec![sf, b1]);
        assert_eq!(
            c_scope
                .compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "")
                .target
                .unwrap()
                .id,
            "sf1"
        );
        assert_eq!(c_scope.next_target().unwrap().id, "b1_1");
        let gc = c_scope.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(gc.state, NavState::FloorMismatch);
        assert_eq!(c_scope.next_target().unwrap().id, "sf1");

        // Mock FloorResolver + normalize_raw_nav_pin
        struct MockResolver;
        impl FloorResolver for MockResolver {
            fn infer_unique_underground_floor(
                &self,
                map_id: i32,
                lat: f64,
                lng: f64,
            ) -> Option<(String, String)> {
                if map_id == 2 && (-20.0..=0.0).contains(&lat) && (10.0..=30.0).contains(&lng) {
                    Some(("184".to_owned(), "B1".to_owned()))
                } else {
                    None
                }
            }
            fn resolve_floor_label(&self, _map_id: i32, floor_id: &str) -> Option<String> {
                match floor_id {
                    "184" => Some("B1".to_owned()),
                    _ => None,
                }
            }
        }
        let nu = normalize_raw_nav_pin(
            &json!({"id": "nu1", "lat": -10.0, "lng": 20.0, "is_underground": true}),
            2,
            Some(&MockResolver),
        )
        .unwrap();
        assert_eq!(nu.floor_id.as_deref(), Some("184"));
        assert_eq!(nu.floor_name.as_deref(), Some("B1"));

        let na = normalize_raw_nav_pin(
            &json!({"id": "na1", "lat": 55.0, "lng": 60.0, "is_underground": true}),
            2,
            Some(&MockResolver),
        )
        .unwrap();
        assert_eq!(na.floor_id, None);
        assert!(na.is_underground);
    }

    #[test]
    fn collected_hold_controller_guard() {
        assert!(CollectedHoldController::new(0.0).is_err());
        assert!(CollectedHoldController::new(f64::NAN).is_err());

        let mut hold = CollectedHoldController::new(1.0).unwrap();
        assert!(!hold.begin(None, true, Some(10.0)));
        assert!(!hold.begin(Some("p1"), false, Some(10.0)));

        assert!(hold.begin(Some("p1"), true, Some(10.0)));
        let s1 = hold.update(true, Some("p1"), true, Some(10.4));
        assert!(s1.active && !s1.confirmed && !s1.cancelled && (s1.progress - 0.4).abs() < 1e-6);
        let s2 = hold.update(true, Some("p1"), true, Some(11.05));
        assert!(s2.confirmed && !s2.active && s2.progress == 1.0);
        assert!(!hold.is_active());

        assert!(hold.begin(Some("p1"), true, Some(20.0)));
        let sr = hold.update(false, Some("p1"), true, Some(20.5));
        assert!(sr.cancelled && !sr.confirmed && !hold.is_active());

        assert!(hold.begin(Some("p1"), true, Some(30.0)));
        let ss = hold.update(true, Some("p2"), true, Some(30.5));
        assert!(ss.cancelled && !ss.confirmed);

        assert!(hold.begin(Some("p1"), true, Some(40.0)));
        let sl = hold.update(true, Some("p1"), false, Some(40.5));
        assert!(sl.cancelled && !sl.confirmed);
    }

    #[test]
    fn stub_serde_and_action_aliases() {
        let pin: NavPin =
            serde_json::from_str(r#"{"id":"1","map_id":2,"lat":0.0,"lng":0.0}"#).unwrap();
        assert_eq!(pin.id, "1");
        assert_eq!(pin.map_id, 2);
        let wire = pin.to_wire_json();
        assert_eq!(wire.as_object().unwrap().len(), 14);
        assert_eq!(wire["floor_name"], "");

        for s in [
            "next",
            "prev",
            "skip",
            "undo",
            "refresh",
            "clear_route",
            "blacklist",
            "mark",
            "guide",
            "layout",
        ] {
            let act = NavAction::parse_server_action(s).expect(s);
            assert_eq!(act.as_str(), s);
            let round: NavAction = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(round, act);
        }
        assert_eq!(NavAction::parse_command_alias("previous"), Some(NavAction::Prev));
        assert_eq!(
            NavAction::parse_command_alias("mark_collected"),
            Some(NavAction::Mark)
        );
        assert_eq!(
            NavAction::parse_command_alias("clear_route_queue"),
            Some(NavAction::ClearRoute)
        );
        assert_eq!(NavAction::parse_server_action("next_target"), None);
    }

    #[test]
    fn nav_pin_deserialize_null_is_underground() {
        let pin: NavPin = serde_json::from_value(json!({
            "id": "p1",
            "map_id": 2,
            "lat": 1.0,
            "lng": 2.0,
            "is_underground": null
        }))
        .unwrap();
        assert!(!pin.is_underground);
    }

    #[test]
    fn route_document_session_auto_passes_waypoints_snaps_sections_and_reports_wrong_map() {
        use crate::route_doc::{RouteAnchor, RouteSection, RouteStop};

        let doc = RouteDocument {
            title: "Desert Scarabs".to_owned(),
            map_id: 2,
            map_name: "Teyvat".to_owned(),
            sections: vec![
                RouteSection {
                    index: 0,
                    name: "Section 1".to_owned(),
                    color: "#ffffff".to_owned(),
                    anchor_teleport: Some(RouteAnchor {
                        point_id: Some("tp1".to_owned()),
                        lat: 100.0,
                        lng: 100.0,
                        ..Default::default()
                    }),
                    stops: vec![
                        RouteStop {
                            id: "tp1".to_owned(),
                            kind: RouteStopKind::Teleport,
                            lat: 100.0,
                            lng: 100.0,
                            name: "Teleport".to_owned(),
                            ..Default::default()
                        },
                        RouteStop {
                            id: "cave1".to_owned(),
                            kind: RouteStopKind::Waypoint,
                            lat: 130.0,
                            lng: 100.0,
                            name: "Cave Entrance".to_owned(),
                            ..Default::default()
                        },
                        RouteStop {
                            id: "item1".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 150.0,
                            lng: 100.0,
                            name: "Scarab".to_owned(),
                            note: "On ceiling".to_owned(),
                            img_url: "https://webstatic.hoyoverse.com/s.png".to_owned(),
                            floor_id: Some("372".to_owned()),
                            floor_name: "B1".to_owned(),
                            is_underground: true,
                            ..Default::default()
                        },
                    ],
                    edges: vec![(0, 1), (1, 2)],
                },
                RouteSection {
                    index: 1,
                    name: "Section 2".to_owned(),
                    color: "#4ade80".to_owned(),
                    anchor_teleport: Some(RouteAnchor {
                        point_id: Some("tp2".to_owned()),
                        lat: 800.0,
                        lng: 800.0,
                        ..Default::default()
                    }),
                    stops: vec![RouteStop {
                        id: "item2".to_owned(),
                        kind: RouteStopKind::Collectible,
                        lat: 820.0,
                        lng: 820.0,
                        name: "Scarab 2".to_owned(),
                        ..Default::default()
                    }],
                    edges: Vec::new(),
                },
            ],
            ..Default::default()
        };

        let mut nav = NavigationController::new();
        let chosen = nav.load_route_document(doc, None, &[]);
        assert_eq!(chosen, Some(0));
        assert!(nav.is_route_mode());

        // Wrong map guidance when player is on Enkanomiya (map_id=7)
        let g_wrong = nav.compute_guidance(0.0, 0.0, Some(0.0), 7, None, false, "TRACKING", "");
        assert_eq!(g_wrong.state, NavState::Idle);
        assert_eq!(g_wrong.route_target_map_id, Some(2));
        assert_eq!(g_wrong.route_target_name, "Teyvat");

        // Land near Teleport tp1 (< 55u) -> auto-passes tp1 to cave1 WITHOUT dirtying collected_ids
        let ev_tp = nav
            .advance_route_on_position(2, 102.0, 100.0)
            .expect("teleport passed");
        assert_eq!(ev_tp.passed_stop_id.as_deref(), Some("tp1"));
        assert!(nav.collected_ids.is_empty());
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("cave1"));

        // Walk through cave1 (< 22u) -> advances cleanly to Scarab item1 with route image/notes
        let ev_cave = nav
            .advance_route_on_position(2, 125.0, 100.0)
            .expect("cave passed");
        assert_eq!(ev_cave.passed_stop_id.as_deref(), Some("cave1"));
        let g_item = nav.compute_guidance(148.0, 100.0, Some(0.0), 2, Some("372"), true, "TRACKING", "B1");
        assert_eq!(g_item.target.as_ref().map(|t| t.id.as_str()), Some("item1"));
        assert_eq!(g_item.target.as_ref().map(|t| t.description.as_str()), Some("On ceiling"));
        assert_eq!(g_item.route_section_label, "S1");

        // Collect item1 -> check_and_advance_completed_section advances to Section 2!
        nav.mark_collected(Some("item1"));
        let sec_done = nav
            .check_and_advance_completed_section()
            .expect("section 1 completed");
        assert_eq!(sec_done.finished_section_index, 0);
        assert_eq!(sec_done.next_section_index, Some(1));
        assert!(!sec_done.route_completed);
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("item2"));

        // Re-load from Section 1 and test out-of-order section snap when teleporting to Section 2
        assert!(nav.select_route_section(1));
        nav.rollback_collected("item1");
        assert!(nav.select_route_section(0));
        let snap_ev = nav
            .advance_route_on_position(2, 805.0, 805.0)
            .expect("out-of-order section snap");
        assert_eq!(snap_ev.snapped_from_section, Some(0));
        assert_eq!(snap_ev.section_index, 1);
    }

    #[test]
    fn single_pin_lock_survives_set_pins_and_takeover_releases_on_collect_and_handles_cross_map() {
        let mut far_pin = make_pin("lock_1", "Băng Thần Đồng Xa", -300.0, 0.0);
        far_pin.map_id = 2;
        let near_pin = make_pin("near_1", "Rương Gần", -3.0, 0.0);

        let mut nav = NavigationController::with_pins(vec![near_pin.clone()]);
        nav.skipped_ids.insert("lock_1".to_owned());
        nav.blacklisted_ids.insert("lock_1".to_owned());
        nav.collected_ids.insert("unrelated_done".to_owned());

        let locked = nav.lock_target_pin(far_pin);
        assert_eq!(locked.id, "lock_1");
        assert!(!nav.skipped_ids().contains("lock_1"));
        assert!(!nav.blacklisted_ids().contains("lock_1"));
        assert!(nav.collected_ids().contains("unrelated_done"));
        assert_eq!(nav.locked_target_pin().map(|p| p.id.as_str()), Some("lock_1"));
        assert_eq!(nav.current_target_peek().map(|p| p.id.as_str()), Some("lock_1"));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("lock_1"));

        // Survives periodic set_pins both with coverage covering the locked pin and without coverage
        nav.set_pins(vec![near_pin.clone()], true, Some((-300.0, 0.0, 500.0)));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("lock_1"));
        nav.set_pins(vec![near_pin], true, None);
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("lock_1"));

        // Resists proximity_takeover (near_1 is 3m away) and relock_after_jump (> 50m jump)
        let g1 = nav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g1.state, NavState::Tracking);
        assert_eq!(g1.target.as_ref().map(|p| p.id.as_str()), Some("lock_1"));
        assert!(g1.is_single_pin_locked);

        let g_jump = nav.compute_guidance(-2.0, 80.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_jump.target.as_ref().map(|p| p.id.as_str()), Some("lock_1"));
        assert!(g_jump.is_single_pin_locked);

        // Releases on mark_collected and falls back to near_1
        let after_collect = nav.mark_collected(None).expect("next target after collect");
        assert_eq!(after_collect.id, "near_1");
        assert!(nav.locked_target_pin().is_none());
        let g_after = nav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert!(!g_after.is_single_pin_locked);

        // Releases on skip_current and blacklist_current
        nav.lock_target_pin(make_pin("lock_skip", "Skip Lock", -120.0, 0.0));
        assert!(nav.locked_target_pin().is_some());
        let _ = nav.skip_current();
        assert!(nav.locked_target_pin().is_none());

        nav.lock_target_pin(make_pin("lock_bl", "BL Lock", -120.0, 0.0));
        assert!(nav.locked_target_pin().is_some());
        let _ = nav.blacklist_current();
        assert!(nav.locked_target_pin().is_none());

        // Cross-map single-pin lock reports Idle + Waypoint + route_target_map_id + is_single_pin_locked
        let mut enka_pin = make_pin("enka_1", "Rương Enkanomiya", 42.0, 84.0);
        enka_pin.map_id = 7;
        nav.lock_target_pin(enka_pin);
        let g_cross = nav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_cross.state, NavState::Idle);
        assert_eq!(g_cross.target.as_ref().map(|p| p.id.as_str()), Some("enka_1"));
        assert_eq!(g_cross.route_mode, NavRouteMode::Waypoint);
        assert_eq!(g_cross.route_target_map_id, Some(7));
        assert_eq!(g_cross.route_target_name, "Rương Enkanomiya");
        assert!(!g_cross.is_locked_on);
        assert!(g_cross.is_single_pin_locked);

        // clear_target_lock returns true once, then false
        assert!(nav.clear_target_lock());
        assert!(!nav.clear_target_lock());
        assert!(nav.locked_target_pin().is_none());
    }

    #[test]
    fn reset_route_progress_restores_all_stops_section_zero_and_waypoint_floor_guard() {
        use crate::route_doc::{RouteSection, RouteStop};

        let mut empty_nav = NavigationController::new();
        assert!(!empty_nav.reset_route_progress());

        let doc = RouteDocument {
            title: "Subterranean Run".to_owned(),
            map_id: 2,
            map_name: "Teyvat".to_owned(),
            sections: vec![
                RouteSection {
                    index: 0,
                    name: "Sec 1".to_owned(),
                    color: "#38bdf8".to_owned(),
                    anchor_teleport: None,
                    stops: vec![
                        RouteStop {
                            id: "wp_b1".to_owned(),
                            kind: RouteStopKind::Waypoint,
                            lat: 100.0,
                            lng: 100.0,
                            floor_id: Some("372".to_owned()),
                            floor_name: "B1".to_owned(),
                            is_underground: true,
                            name: "Tunnel Turn".to_owned(),
                            ..Default::default()
                        },
                        RouteStop {
                            id: "c1".to_owned(),
                            kind: RouteStopKind::Collectible,
                            lat: 110.0,
                            lng: 100.0,
                            name: "Scarab 1".to_owned(),
                            ..Default::default()
                        },
                    ],
                    edges: vec![(0, 1)],
                },
                RouteSection {
                    index: 1,
                    name: "Sec 2".to_owned(),
                    color: "#4ade80".to_owned(),
                    anchor_teleport: None,
                    stops: vec![RouteStop {
                        id: "c2".to_owned(),
                        kind: RouteStopKind::Collectible,
                        lat: 500.0,
                        lng: 500.0,
                        name: "Scarab 2".to_owned(),
                        ..Default::default()
                    }],
                    edges: Vec::new(),
                },
            ],
            ..Default::default()
        };

        let mut nav = NavigationController::new();
        nav.lock_target_pin(make_pin("pre_lock", "Pre Lock", 10.0, 10.0));
        assert!(nav.locked_target_pin().is_some());
        assert_eq!(nav.load_route_document(doc, None, &[]), Some(0));
        assert!(nav.locked_target_pin().is_none(), "load_route_document clears single-pin lock");

        // Player on different known floor "999" must NOT auto-pass waypoint on floor "372"
        let _ = nav.compute_guidance(100.0, 100.0, Some(0.0), 2, Some("999"), true, "TRACKING", "B2");
        assert!(nav.advance_route_on_position(2, 100.0, 100.0).is_none());
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("wp_b1"));

        // Player on matching floor "372" auto-passes waypoint, then stops at Collectible (manual only)
        let _ = nav.compute_guidance(100.0, 100.0, Some(0.0), 2, Some("372"), true, "TRACKING", "B1");
        let ev = nav
            .advance_route_on_position(2, 100.0, 100.0)
            .expect("matching floor auto-passes waypoint");
        assert_eq!(ev.passed_stop_id.as_deref(), Some("wp_b1"));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("c1"));
        assert!(nav.advance_route_on_position(2, 110.0, 100.0).is_none(), "collectible stays manual");

        // Collect c1, advance to section 1, skip c2, and lock an unrelated pin
        nav.mark_collected(Some("c1"));
        let _ = nav.check_and_advance_completed_section();
        assert_eq!(nav.active_route_doc().map(|(_, idx)| idx), Some(1));
        let _ = nav.skip_current();
        nav.lock_target_pin(make_pin("side_lock", "Side Lock", 15.0, 15.0));

        // Reset route progress restores all stops across all sections, section 0, and clears lock
        assert!(nav.reset_route_progress());
        assert!(nav.locked_target_pin().is_none());
        assert_eq!(nav.active_route_doc().map(|(_, idx)| idx), Some(0));
        assert!(!nav.collected_ids().contains("c1"));
        assert!(!nav.skipped_ids().contains("c2"));
        assert!(!nav.is_route_stop_done("wp_b1"));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("wp_b1"));

        // Plain route_queue reset without RouteDocument
        let mut qnav = NavigationController::new();
        qnav.set_route_queue(vec![
            make_pin("q1", "Q1", 10.0, 0.0),
            make_pin("q2", "Q2", 20.0, 0.0),
        ]);
        let _ = qnav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        let _ = qnav.mark_collected(None);
        assert_eq!(qnav.current_target().map(|p| p.id.as_str()), Some("q2"));
        assert!(qnav.reset_route_progress());
        assert_eq!(qnav.current_target().map(|p| p.id.as_str()), Some("q1"));
        assert_eq!(qnav.route_progress(), Some((1, 2)));
    }

    #[test]
    fn set_pins_filter_changed_drops_disabled_category_pin_outside_coverage() {
        let mut nav = NavigationController::new();
        // Initial push has a Snezhnaya Chest pin far away (320m > 260m coverage).
        let snezhnaya = make_pin("snez_1", "Snezhnaya Chest", 320.0, 0.0);
        nav.set_pins(vec![snezhnaya], false, Some((0.0, 0.0, 260.0)));
        let _ = nav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("snez_1"));

        // Normal proximity push (filter_changed = false, coverage = Some(260)) retains out-of-window selected pin.
        nav.set_pins(vec![], false, Some((0.0, 0.0, 260.0)));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("snez_1"));

        // Filter-toggle push (filter_changed = true -> reset_order = true, coverage = None) drops disabled category pin immediately.
        nav.set_pins(vec![], true, None);
        assert!(
            nav.current_target().is_none(),
            "filter_changed push must drop disabled-category pin even outside coverage radius"
        );

        // Explicit target lock still survives a filter-toggle push.
        nav.lock_target_pin(make_pin("locked_1", "Locked Chest", 400.0, 0.0));
        nav.set_pins(vec![], true, None);
        assert_eq!(
            nav.current_target().map(|p| p.id.as_str()),
            Some("locked_1")
        );
    }

    #[test]
    fn set_pins_with_nearby_coverage_preserves_active_route_stops_and_metadata() {
        use crate::route_doc::{RouteDocument, RouteSection, RouteStop, RouteStopKind};

        let doc = RouteDocument {
            title: "Nod-Krai Route".to_owned(),
            map_id: 2,
            sections: vec![RouteSection {
                index: 0,
                name: "Pillar of Embla".to_owned(),
                color: "#fde047".to_owned(),
                stops: vec![
                    RouteStop {
                        id: "rt_tp".to_owned(),
                        kind: RouteStopKind::Teleport,
                        lat: 100.0,
                        lng: 100.0,
                        label_id: 3,
                        name: "Embla Waypoint".to_owned(),
                        ..Default::default()
                    },
                    RouteStop {
                        id: "rt_wp".to_owned(),
                        kind: RouteStopKind::Waypoint,
                        lat: 130.0,
                        lng: 110.0,
                        floor_id: Some("372".to_owned()),
                        floor_name: "B1".to_owned(),
                        is_underground: true,
                        name: "Cave Entry".to_owned(),
                        ..Default::default()
                    },
                    RouteStop {
                        id: "rt_col".to_owned(),
                        kind: RouteStopKind::Collectible,
                        lat: 160.0,
                        lng: 120.0,
                        label_id: 17,
                        name: "Luxurious Chest".to_owned(),
                        note: "Behind ice wall".to_owned(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut nav = NavigationController::new();
        assert_eq!(nav.load_route_document(doc, None, &[]), Some(0));

        // 1. Empty `/nearby_pins` with coverage_radius covering all route stops (user did NOT enable sidebar filter)
        //    MUST NEVER evict active route stops or cause `NavState::Empty` ("No uncollected pins nearby").
        nav.set_pins(vec![], false, Some((120.0, 110.0, 260.0)));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("rt_tp"));
        let g = nav.compute_guidance(120.0, 110.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g.state, NavState::Tracking);
        assert_eq!(g.target.as_ref().map(|p| p.id.as_str()), Some("rt_tp"));

        // 2. Filter-changed `/nearby_pins` (`reset_order = true, coverage = None`) must also keep route stops intact.
        nav.set_pins(vec![], true, None);
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("rt_tp"));

        // 3. Incoming plain nearby pin overlapping an active route stop ID must preserve route metadata
        //    so teleport/waypoint auto-pass and section indicators keep working.
        let mut overlap = make_pin("rt_wp", "Cave Entry", 130.0, 110.0);
        overlap.description = String::new();
        nav.set_pins(vec![overlap], false, Some((120.0, 110.0, 260.0)));
        let wp_pin = nav.pins_by_id.get("rt_wp").expect("rt_wp retained");
        assert_eq!(wp_pin.stop_kind.as_deref(), Some("waypoint"));
        assert_eq!(wp_pin.route_section_index, Some(0));
        assert_eq!(wp_pin.route_section_total, Some(1));
        assert_eq!(wp_pin.route_section_name.as_deref(), Some("Pillar of Embla"));
        assert_eq!(wp_pin.floor_id.as_deref(), Some("372"));

        // Teleport auto-pass still advances cleanly to `rt_wp`, then `rt_col`.
        let ev_tp = nav
            .advance_route_on_position(2, 100.0, 100.0)
            .expect("teleport stop should auto-pass");
        assert_eq!(ev_tp.passed_stop_id.as_deref(), Some("rt_tp"));
        assert_eq!(nav.current_target().map(|p| p.id.as_str()), Some("rt_wp"));
    }

    #[test]
    fn clear_user_locks_releases_cycle_and_cross_floor_locks_and_restores_proximity_takeover() {
        let near = make_pin("near", "Near Chest", 0.0, 0.0);
        let far = make_pin("far", "Far Chest", -60.0, 0.0);
        let mut ug = make_pin("ug", "Cave Chest", -15.0, 0.0);
        ug.floor_id = Some("372".to_owned());
        ug.is_underground = true;

        let mut nav = NavigationController::with_pins(vec![near.clone(), far.clone(), ug]);
        let g0 = nav.compute_guidance(-5.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g0.target.as_ref().map(|p| p.id.as_str()), Some("near"));
        assert!(nav.user_locked_id().is_none());

        // Alt+E (next_target) locks onto the farther pin via user_locked_id (no target_lock_pin).
        let cycled = nav.next_target().expect("next target");
        assert_eq!(cycled.id, "far");
        assert_eq!(nav.user_locked_id(), Some("far"));
        assert!(nav.locked_target_pin().is_none());

        // Narrow clear_target_lock leaves cycle lock untouched (internal callers depend on this).
        assert!(!nav.clear_target_lock());
        assert_eq!(nav.user_locked_id(), Some("far"));

        // While locked, walking next to `near` does NOT trigger proximity_takeover.
        let g_locked = nav.compute_guidance(-1.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_locked.target.as_ref().map(|p| p.id.as_str()), Some("far"));

        // Cancel user locks -> clears user_locked_id and reports true once, then false.
        assert!(nav.clear_user_locks());
        assert!(!nav.clear_user_locks());
        assert!(nav.user_locked_id().is_none());

        // Moving next to `near` now triggers proximity_takeover.
        let g_after = nav.compute_guidance(0.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_after.target.as_ref().map(|p| p.id.as_str()), Some("near"));

        // Cross-floor cycle (`prev_target` wraps to `ug`) sets both user_locked_id and pinned_override_id.
        let cross = nav.prev_target().expect("cross-floor prev target");
        assert_eq!(cross.id, "ug");
        assert_eq!(nav.user_locked_id(), Some("ug"));
        assert_eq!(nav.pinned_override_id.as_deref(), Some("ug"));
        assert!(nav.clear_user_locks());
        assert!(nav.user_locked_id().is_none());
        assert!(nav.pinned_override_id.is_none());
        let g_surface = nav.compute_guidance(-2.0, 0.0, Some(0.0), 2, None, false, "TRACKING", "");
        assert_eq!(g_surface.target.as_ref().map(|p| p.id.as_str()), Some("near"));

        // Explicit single-pin lock is also cleared by clear_user_locks.
        nav.lock_target_pin(far);
        assert!(nav.locked_target_pin().is_some());
        assert!(nav.clear_user_locks());
        assert!(nav.locked_target_pin().is_none());
        assert!(nav.user_locked_id().is_none());
        assert!(!nav.clear_user_locks());
    }
}
