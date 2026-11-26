//! Cross-map pin exporter & affine coordinate bridge (`src/map_bridge.py`).
//!
//! Pure-std + serde implementation providing:
//! - Declarative N:1 HoYoLAB `label_id` normalization via embedded `category_aliases.json`
//! - Full raw HoYoLAB API envelope parsing (`parse_hoyolab_points` / `parse_hoyolab_export`)
//! - Per-map independent axis-affine calibration (`fit_axis_affine`, `calibrate_from_shared_anchors`)
//! - Optimal 1-to-1 spatial + category + underground-layer matcher (`match_pins_one_to_one`)
//! - Target adapters for Kongying Tavern, Appsample, genshinmap.github.io (`GM_007~`), and Canonical JSON
//! - Unified `execute_cross_map_export` entrypoint for `gh-net::sync_server` and `gh-app::callbacks`

use crate::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const EMBEDDED_CATEGORY_ALIASES_JSON: &str =
    include_str!("../../../../assets/data/category_aliases.json");

pub const CANONICAL_SCHEMA_V1: &str = "genshin-helper.canonical-pins.v1";
pub const GM007_PREFIX: &str = "GM_007~";
pub const GM007_VERSION: &str = "GM_007";
pub const GM007_REQUIRED_KEYS: [&str; 5] =
    ["version", "options", "displayed", "completed", "editor"];

const MICRO_SCALE: f64 = 1_000_000.0;
const SURFACE_FLOOR_TOKENS: &[&str] = &["g", "ground", "surface", "teyvat"];
const EMPTY_FLOOR_TOKENS: &[&str] = &["", "0", "none", "null"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Canonical,
    Kongying,
    Appsample,
    Genshinmap,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Kongying => "kongying",
            Self::Appsample => "appsample",
            Self::Genshinmap => "genshinmap",
        }
    }
}

impl FromStr for ExportFormat {
    type Err = CoreError;

    fn from_str(s: &str) -> CoreResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "canonical" => Ok(Self::Canonical),
            "kongying" => Ok(Self::Kongying),
            "appsample" => Ok(Self::Appsample),
            "genshinmap" => Ok(Self::Genshinmap),
            other => Err(CoreError::InvalidArgument(format!(
                "Unsupported target_format: {other:?}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-Std Normalization, Timestamp, and Strict UTF-8 Base64 Primitives
// ---------------------------------------------------------------------------

pub fn normalize_text_key(text: &str) -> String {
    let lower = text.trim().to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut in_sep = false;
    for ch in lower.chars() {
        if ch.is_whitespace() || matches!(ch, '_' | '-' | '/') {
            if !in_sep && !out.is_empty() {
                out.push(' ');
                in_sep = true;
            }
        } else {
            out.push(ch);
            in_sep = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

pub fn normalize_floor_id(value: &Value) -> Option<String> {
    let raw = match value {
        Value::Null | Value::Bool(_) => return None,
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => value.to_string(),
    };
    let trimmed = raw.trim();
    if EMPTY_FLOOR_TOKENS.contains(&trimmed.to_ascii_lowercase().as_str()) {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

pub fn normalize_point_group(value: &Value) -> Option<i64> {
    let iv = coerce_strict_int(value, "point_group").ok()?;
    if iv > 0 { Some(iv) } else { None }
}

fn is_strict_json_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())),
        _ => None,
    }
}

pub fn coerce_strict_int(value: &Value, field_name: &str) -> CoreResult<i64> {
    match value {
        Value::Bool(_) => Err(CoreError::InvalidData(format!(
            "{field_name} must be an integer, got bool"
        ))),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            .ok_or_else(|| {
                CoreError::InvalidData(format!("{field_name} must be an integer, got {n}"))
            }),
        Value::String(s) => {
            let t = s.trim();
            let digits = t.strip_prefix('-').unwrap_or(t);
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                t.parse::<i64>().map_err(|_| {
                    CoreError::InvalidData(format!("{field_name} integer overflow: {t:?}"))
                })
            } else {
                Err(CoreError::InvalidData(format!(
                    "{field_name} must be an integer, got {t:?}"
                )))
            }
        }
        other => Err(CoreError::InvalidData(format!(
            "{field_name} must be an integer, got {other}"
        ))),
    }
}

fn coerce_f64(value: &Value, field_name: &str) -> CoreResult<f64> {
    let val = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match val {
        Some(f) if f.is_finite() => Ok(f),
        _ => Err(CoreError::InvalidData(format!(
            "{field_name} must be a finite number"
        ))),
    }
}

fn value_as_opt_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_owned())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.to_owned()),
        _ => None,
    }
}

fn is_json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n
            .as_i64()
            .map(|i| i != 0)
            .or_else(|| n.as_u64().map(|u| u != 0))
            .or_else(|| n.as_f64().map(|f| f != 0.0))
            .unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn is_leap_year(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's civil-to-days algorithm (days since 1970-01-01).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m_adj = i64::from(m) + if m > 2 { -3 } else { 9 };
    let doy = (153 * m_adj + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's days-to-civil algorithm (days since 1970-01-01 -> `(year, month, day)`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let y = y + i64::from(m <= 2);
    (y, m, d)
}

fn format_utc_parts(unix_sec: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_sec.div_euclid(86_400);
    let sod = unix_sec.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    (y, m, d, hh, mm, ss)
}

fn parse_two_digits(b: &[u8]) -> Option<u32> {
    if b.len() == 2 && b[0].is_ascii_digit() && b[1].is_ascii_digit() {
        Some(u32::from(b[0] - b'0') * 10 + u32::from(b[1] - b'0'))
    } else {
        None
    }
}

fn parse_four_digits(b: &[u8]) -> Option<i64> {
    if b.len() == 4 && b.iter().all(|c| c.is_ascii_digit()) {
        std::str::from_utf8(b).ok()?.parse::<i64>().ok()
    } else {
        None
    }
}

pub fn parse_timestamp_to_unix_sec(value: &Value) -> Option<i64> {
    match value {
        Value::Null | Value::Bool(_) => None,
        Value::Number(n) => {
            let mut num = n.as_f64()?;
            if !num.is_finite() || num <= 0.0 {
                return None;
            }
            if num >= 1e11 {
                num /= 1000.0;
            }
            Some(num.trunc() as i64)
        }
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            if t.chars().all(|c| c.is_ascii_digit()) {
                let num = t.parse::<f64>().ok()?;
                if !num.is_finite() || num <= 0.0 {
                    return None;
                }
                let sec = if num >= 1e11 { num / 1000.0 } else { num };
                return Some(sec.trunc() as i64);
            }
            let b = t.as_bytes();
            if b.len() < 10 {
                return None;
            }
            let sep = b[4];
            if !matches!(sep, b'-' | b'/') || b[7] != sep {
                return None;
            }
            let y = parse_four_digits(&b[0..4])?;
            let m = parse_two_digits(&b[5..7])?;
            let d = parse_two_digits(&b[8..10])?;
            if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
                return None;
            }
            if b.len() == 10 {
                if sep == b'-' {
                    return Some(days_from_civil(y, m, d) * 86_400);
                }
                return None;
            }
            if !matches!(b[10], b'T' | b't' | b' ') || b.len() < 19 {
                return None;
            }
            if b[13] != b':' || b[16] != b':' {
                return None;
            }
            let hh = parse_two_digits(&b[11..13])?;
            let mm = parse_two_digits(&b[14..16])?;
            let ss = parse_two_digits(&b[17..19])?;
            if hh > 23 || mm > 59 || ss > 59 {
                return None;
            }
            let mut idx = 19;
            if idx < b.len() && b[idx] == b'.' {
                idx += 1;
                let frac_start = idx;
                while idx < b.len() && b[idx].is_ascii_digit() {
                    idx += 1;
                }
                if idx == frac_start {
                    return None;
                }
            }
            let offset_sec: i64 = if idx == b.len() {
                0
            } else if b[idx] == b'Z' || b[idx] == b'z' {
                if idx + 1 != b.len() {
                    return None;
                }
                0
            } else if matches!(b[idx], b'+' | b'-') {
                let sign = if b[idx] == b'+' { 1i64 } else { -1i64 };
                let rest = &b[idx + 1..];
                let (oh, om) = if rest.len() == 5 && rest[2] == b':' {
                    (parse_two_digits(&rest[0..2])?, parse_two_digits(&rest[3..5])?)
                } else if rest.len() == 4 {
                    (parse_two_digits(&rest[0..2])?, parse_two_digits(&rest[2..4])?)
                } else {
                    return None;
                };
                if oh > 23 || om > 59 {
                    return None;
                }
                sign * (i64::from(oh) * 3600 + i64::from(om) * 60)
            } else {
                return None;
            };
            let local_sec = days_from_civil(y, m, d) * 86_400
                + i64::from(hh) * 3600
                + i64::from(mm) * 60
                + i64::from(ss);
            Some(local_sec - offset_sec)
        }
        _ => None,
    }
}

pub fn normalize_timestamp_iso(value: &Value) -> Option<String> {
    let sec = parse_timestamp_to_unix_sec(value)?;
    let (y, m, d, hh, mm, ss) = format_utc_parts(sec);
    Some(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"))
}

pub fn format_kongying_time(unix_sec: i64) -> String {
    let (y, m, d, hh, mm, ss) = format_utc_parts(unix_sec);
    format!("{y:04}/{m:02}/{d:02} {hh:02}:{mm:02}:{ss:02}")
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(B64_TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_val(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn base64_decode_strict(s: &str) -> CoreResult<Vec<u8>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = trimmed.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(CoreError::InvalidData(
            "Invalid Base64 length (must be multiple of 4)".to_owned(),
        ));
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    let (chunks, _) = bytes.as_chunks::<4>();
    let num_chunks = chunks.len();
    for (ci, chunk) in chunks.iter().enumerate() {
        let is_last = ci + 1 == num_chunks;
        let a = b64_val(chunk[0]).ok_or_else(|| {
            CoreError::InvalidData("Invalid Base64 character in chunk[0]".to_owned())
        })?;
        let b = b64_val(chunk[1]).ok_or_else(|| {
            CoreError::InvalidData("Invalid Base64 character in chunk[1]".to_owned())
        })?;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        if (pad2 || pad3) && !is_last {
            return Err(CoreError::InvalidData(
                "Base64 padding '=' only allowed in final quartet".to_owned(),
            ));
        }
        if pad2 && !pad3 {
            return Err(CoreError::InvalidData(
                "Invalid Base64 padding '==' expected".to_owned(),
            ));
        }
        let c = if pad2 {
            0
        } else {
            b64_val(chunk[2]).ok_or_else(|| {
                CoreError::InvalidData("Invalid Base64 character in chunk[2]".to_owned())
            })?
        };
        let d = if pad3 {
            0
        } else {
            b64_val(chunk[3]).ok_or_else(|| {
                CoreError::InvalidData("Invalid Base64 character in chunk[3]".to_owned())
            })?
        };
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((triple >> 16) & 0xFF) as u8);
        if !pad2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if !pad3 {
            out.push((triple & 0xFF) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Core Data Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetId {
    Int(i64),
    Str(String),
}

impl TargetId {
    pub fn to_strict_int(&self, field: &str) -> CoreResult<i64> {
        match self {
            Self::Int(n) => Ok(*n),
            Self::Str(s) => coerce_strict_int(&Value::String(s.clone()), field),
        }
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(n) => write!(f, "{n}"),
            Self::Str(s) => f.write_str(s),
        }
    }
}

impl From<i64> for TargetId {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<&str> for TargetId {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}

impl From<String> for TargetId {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

fn default_map_id() -> i64 {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalPin {
    pub hoyolab_id: i64,
    #[serde(default = "default_map_id")]
    pub map_id: i64,
    pub label_id: i64,
    pub canonical_category: String,
    pub x_pos: f64,
    pub y_pos: f64,
    #[serde(default)]
    pub floor_id: Option<String>,
    #[serde(default)]
    pub point_group: Option<i64>,
    #[serde(default, deserialize_with = "crate::deserialize_null_default")]
    pub is_underground: bool,
    #[serde(default)]
    pub marked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_name: Option<String>,
}

impl CanonicalPin {
    pub fn new(
        hoyolab_id: i64,
        map_id: i64,
        label_id: i64,
        canonical_category: impl Into<String>,
        x_pos: f64,
        y_pos: f64,
    ) -> Self {
        Self {
            hoyolab_id,
            map_id,
            label_id,
            canonical_category: canonical_category.into(),
            x_pos,
            y_pos,
            floor_id: None,
            point_group: None,
            is_underground: false,
            marked_at: None,
            label_name: None,
        }
    }

    pub fn from_value(raw: &Value) -> CoreResult<Self> {
        let obj = raw
            .as_object()
            .ok_or_else(|| CoreError::InvalidData("CanonicalPin must be a JSON object".into()))?;
        let hoyolab_id = coerce_strict_int(
            obj.get("hoyolab_id")
                .ok_or_else(|| CoreError::InvalidData("Missing hoyolab_id".into()))?,
            "hoyolab_id",
        )?;
        let map_id = match obj.get("map_id") {
            Some(v) if !v.is_null() => coerce_strict_int(v, "map_id")?,
            _ => 2,
        };
        let label_id = coerce_strict_int(
            obj.get("label_id")
                .ok_or_else(|| CoreError::InvalidData("Missing label_id".into()))?,
            "label_id",
        )?;
        let canonical_category = obj
            .get("canonical_category")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::InvalidData("Missing canonical_category".into()))?
            .to_owned();
        let x_pos = coerce_f64(
            obj.get("x_pos")
                .ok_or_else(|| CoreError::InvalidData("Missing x_pos".into()))?,
            "x_pos",
        )?;
        let y_pos = coerce_f64(
            obj.get("y_pos")
                .ok_or_else(|| CoreError::InvalidData("Missing y_pos".into()))?,
            "y_pos",
        )?;
        let floor_id = obj.get("floor_id").and_then(normalize_floor_id);
        let point_group = obj.get("point_group").and_then(normalize_point_group);
        let is_underground = obj
            .get("is_underground")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let marked_at = obj.get("marked_at").and_then(normalize_timestamp_iso);
        let label_name = obj
            .get("label_name")
            .filter(|v| !v.is_null())
            .and_then(value_as_opt_string);

        Ok(Self {
            hoyolab_id,
            map_id,
            label_id,
            canonical_category,
            x_pos,
            y_pos,
            floor_id,
            point_group,
            is_underground,
            marked_at,
            label_name,
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetPin {
    pub target_id: TargetId,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub canonical_category: Option<String>,
    #[serde(default)]
    pub category_name: Option<String>,
    #[serde(default)]
    pub appsample_code: Option<String>,
    #[serde(default)]
    pub feature_key: Option<String>,
    #[serde(default)]
    pub map_id: Option<i64>,
    #[serde(default)]
    pub floor_id: Option<String>,
    #[serde(default)]
    pub point_group: Option<i64>,
    #[serde(default)]
    pub is_underground: Option<bool>,
    #[serde(default, deserialize_with = "crate::deserialize_null_default")]
    pub extra: BTreeMap<String, Value>,
}

impl TargetPin {
    pub fn new(target_id: impl Into<TargetId>, x: f64, y: f64) -> Self {
        Self {
            target_id: target_id.into(),
            x,
            y,
            canonical_category: None,
            category_name: None,
            appsample_code: None,
            feature_key: None,
            map_id: None,
            floor_id: None,
            point_group: None,
            is_underground: None,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HoyolabParseResult {
    pub map_id: i64,
    pub pins: Vec<CanonicalPin>,
    pub missing_from_point_list: Vec<i64>,
    pub unmapped_label_ids: Vec<i64>,
    pub total_marked: usize,
    pub point_index_size: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxisAffine {
    pub ax: f64,
    pub bx: f64,
    pub cy: f64,
    pub dy: f64,
    #[serde(default)]
    pub map_id: Option<i64>,
    #[serde(default)]
    pub rmse: f64,
    #[serde(default)]
    pub max_residual: f64,
    #[serde(default)]
    pub anchor_count: usize,
    #[serde(default = "default_fitted_source")]
    pub source: String,
}

fn default_fitted_source() -> String {
    "fitted".to_owned()
}

pub fn kongying_twt40_seed() -> AxisAffine {
    AxisAffine {
        ax: 4.0 / 3.0,
        bx: 1523.4,
        cy: 4.0 / 3.0,
        dy: -3921.6,
        map_id: Some(2),
        rmse: 0.0,
        max_residual: 0.0,
        anchor_count: 0,
        source: "seed_twt40".to_owned(),
    }
}

impl AxisAffine {
    pub fn transform(&self, x: f64, y: f64) -> (f64, f64) {
        (self.ax * x + self.bx, self.cy * y + self.dy)
    }

    pub fn inverse(&self) -> CoreResult<Self> {
        if self.ax.abs() < 1e-12 || self.cy.abs() < 1e-12 {
            return Err(CoreError::InvalidArgument(
                "Cannot invert degenerate AxisAffine with zero scale".to_owned(),
            ));
        }
        let inv_ax = 1.0 / self.ax;
        let inv_cy = 1.0 / self.cy;
        Ok(Self {
            ax: inv_ax,
            bx: -self.bx * inv_ax,
            cy: inv_cy,
            dy: -self.dy * inv_cy,
            map_id: self.map_id,
            rmse: self.rmse,
            max_residual: self.max_residual,
            anchor_count: self.anchor_count,
            source: format!("inverse:{}", self.source),
        })
    }

    pub fn has_y_flip(&self) -> bool {
        self.cy < 0.0
    }

    pub fn from_value(raw: &Value) -> CoreResult<Self> {
        let obj = raw
            .as_object()
            .ok_or_else(|| CoreError::InvalidData("AxisAffine must be a JSON object".into()))?;
        let ax = coerce_f64(
            obj.get("ax")
                .ok_or_else(|| CoreError::InvalidData("Missing ax".into()))?,
            "ax",
        )?;
        let bx = coerce_f64(
            obj.get("bx")
                .ok_or_else(|| CoreError::InvalidData("Missing bx".into()))?,
            "bx",
        )?;
        let cy = coerce_f64(
            obj.get("cy")
                .ok_or_else(|| CoreError::InvalidData("Missing cy".into()))?,
            "cy",
        )?;
        let dy = coerce_f64(
            obj.get("dy")
                .ok_or_else(|| CoreError::InvalidData("Missing dy".into()))?,
            "dy",
        )?;
        let map_id = match obj.get("map_id") {
            Some(v) if !v.is_null() => Some(coerce_strict_int(v, "map_id")?),
            _ => None,
        };
        let rmse = obj
            .get("rmse")
            .and_then(|v| coerce_f64(v, "rmse").ok())
            .unwrap_or(0.0);
        let max_residual = obj
            .get("max_residual")
            .and_then(|v| coerce_f64(v, "max_residual").ok())
            .unwrap_or(0.0);
        let anchor_count = obj
            .get("anchor_count")
            .and_then(|v| coerce_strict_int(v, "anchor_count").ok())
            .map(|n| n.max(0) as usize)
            .unwrap_or(0);
        let source = obj
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("loaded")
            .to_owned();
        Ok(Self {
            ax,
            bx,
            cy,
            dy,
            map_id,
            rmse,
            max_residual,
            anchor_count,
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    pub source: CanonicalPin,
    pub target: TargetPin,
    pub distance: f64,
    pub second_best_distance: Option<f64>,
    pub is_ambiguous: bool,
    pub matched_via: String,
    pub projected_x: f64,
    pub projected_y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnmatchedPin {
    pub source: CanonicalPin,
    pub reason: String,
    pub nearest_distance: Option<f64>,
    pub nearest_target_id: Option<TargetId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferSummary {
    pub total_source_pins: usize,
    pub matched_count: usize,
    pub confident_count: usize,
    pub ambiguous_count: usize,
    pub unmatched_count: usize,
    pub unmapped_categories: Vec<String>,
    pub missing_from_point_list_count: usize,
    pub match_rate_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferReport {
    pub matched: Vec<MatchResult>,
    pub ambiguous: Vec<MatchResult>,
    pub unmatched: Vec<UnmatchedPin>,
    pub unmapped_categories: Vec<String>,
    pub missing_from_point_list: Vec<i64>,
    pub match_rate_pct: f64,
}

impl TransferReport {
    pub fn confident_matches(&self) -> Vec<&MatchResult> {
        self.matched.iter().filter(|m| !m.is_ambiguous).collect()
    }

    pub fn total_source_pins(&self) -> usize {
        self.matched.len() + self.unmatched.len()
    }

    pub fn summary(&self) -> TransferSummary {
        TransferSummary {
            total_source_pins: self.total_source_pins(),
            matched_count: self.matched.len(),
            confident_count: self.confident_matches().len(),
            ambiguous_count: self.ambiguous.len(),
            unmatched_count: self.unmatched.len(),
            unmapped_categories: self.unmapped_categories.clone(),
            missing_from_point_list_count: self.missing_from_point_list.len(),
            match_rate_pct: (self.match_rate_pct * 100.0).round() / 100.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Declarative N:1 Category Alias Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CategorySpec {
    pub group: String,
    pub is_calibration_anchor: bool,
    pub primary_hoyolab_label_id: Option<i64>,
    pub hoyolab_label_ids: Vec<i64>,
    pub appsample_code: Option<String>,
    pub kongying_names: Vec<String>,
    pub genshinmap_keys: Vec<String>,
    pub name_aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CategoryRegistry {
    pub schema_version: i64,
    pub categories: BTreeMap<String, CategorySpec>,
    label_id_to_canonical: HashMap<i64, String>,
    appsample_to_canonical: HashMap<String, String>,
    name_to_canonical: HashMap<String, String>,
    genshinmap_key_to_canonical: HashMap<String, String>,
    genshinmap_suffix_pairs: Vec<(String, String)>,
    calibration_anchors: HashSet<String>,
    default_affines: HashMap<(String, i64), AxisAffine>,
}

static EMBEDDED_REGISTRY: OnceLock<CategoryRegistry> = OnceLock::new();

impl CategoryRegistry {
    pub fn embedded() -> &'static Self {
        EMBEDDED_REGISTRY.get_or_init(|| {
            Self::from_json_str(EMBEDDED_CATEGORY_ALIASES_JSON)
                .expect("embedded category_aliases.json must be valid")
        })
    }

    pub fn from_file(path: &Path) -> CoreResult<Self> {
        let raw = std::fs::read_to_string(path).map_err(CoreError::Io)?;
        Self::from_json_str(&raw)
    }

    pub fn from_json_str(raw: &str) -> CoreResult<Self> {
        let val: Value = serde_json::from_str(raw)?;
        Self::from_value(&val)
    }

    pub fn from_value(data: &Value) -> CoreResult<Self> {
        let obj = data.as_object().ok_or_else(|| {
            CoreError::InvalidData("category_aliases.json root must be an object".into())
        })?;
        let schema_version = obj
            .get("schema_version")
            .and_then(|v| coerce_strict_int(v, "schema_version").ok())
            .unwrap_or(1);

        let mut default_affines = HashMap::new();
        if let Some(raw_aff) = obj.get("default_map_affines").and_then(|v| v.as_object()) {
            for (target_name, by_map_val) in raw_aff {
                let Some(by_map) = by_map_val.as_object() else {
                    continue;
                };
                for (mid_key, spec_val) in by_map {
                    let Some(spec) = spec_val.as_object() else {
                        continue;
                    };
                    let Ok(mid) = mid_key.trim().parse::<i64>() else {
                        continue;
                    };
                    let ax = spec.get("ax").and_then(|v| coerce_f64(v, "ax").ok());
                    let bx = spec.get("bx").and_then(|v| coerce_f64(v, "bx").ok());
                    let cy = spec.get("cy").and_then(|v| coerce_f64(v, "cy").ok());
                    let dy = spec.get("dy").and_then(|v| coerce_f64(v, "dy").ok());
                    if let (Some(ax), Some(bx), Some(cy), Some(dy)) = (ax, bx, cy, dy) {
                        let source = spec
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("config_seed")
                            .to_owned();
                        default_affines.insert(
                            (target_name.trim().to_ascii_lowercase(), mid),
                            AxisAffine {
                                ax,
                                bx,
                                cy,
                                dy,
                                map_id: Some(mid),
                                rmse: 0.0,
                                max_residual: 0.0,
                                anchor_count: 0,
                                source,
                            },
                        );
                    }
                }
            }
        }

        let raw_categories = obj
            .get("categories")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                CoreError::InvalidData("category_aliases.json 'categories' must be an object".into())
            })?;

        let mut categories = BTreeMap::new();
        let mut label_id_to_canonical: HashMap<i64, String> = HashMap::new();
        let mut appsample_to_canonical = HashMap::new();
        let mut name_to_canonical = HashMap::new();
        let mut genshinmap_key_to_canonical = HashMap::new();
        let mut genshinmap_suffix_pairs = Vec::new();
        let mut calibration_anchors = HashSet::new();

        for (canonical, spec_val) in raw_categories {
            if !spec_val.is_object() {
                continue;
            }
            let spec: CategorySpec = serde_json::from_value(spec_val.clone())?;
            let canon_key = canonical.trim().to_owned();

            if spec.is_calibration_anchor {
                calibration_anchors.insert(canon_key.clone());
            }
            name_to_canonical.insert(normalize_text_key(&canon_key), canon_key.clone());

            for &lid in &spec.hoyolab_label_ids {
                if let Some(prev) = label_id_to_canonical.get(&lid)
                    && prev != &canon_key
                {
                    return Err(CoreError::InvalidData(format!(
                        "Duplicate HoYoLAB label_id {lid} mapped to both {prev:?} and {canon_key:?}"
                    )));
                }
                label_id_to_canonical.insert(lid, canon_key.clone());
            }

            if let Some(ref code) = spec.appsample_code {
                let c = code.trim().to_ascii_lowercase();
                if !c.is_empty() {
                    appsample_to_canonical.insert(c, canon_key.clone());
                }
            }

            for alias in spec.name_aliases.iter().chain(spec.kongying_names.iter()) {
                let norm = normalize_text_key(alias);
                if !norm.is_empty() {
                    name_to_canonical.insert(norm, canon_key.clone());
                }
            }

            for gm_key in &spec.genshinmap_keys {
                let gm_norm = gm_key.trim().to_ascii_lowercase();
                if !gm_norm.is_empty() {
                    genshinmap_key_to_canonical.insert(gm_norm.clone(), canon_key.clone());
                    genshinmap_suffix_pairs.push((gm_norm, canon_key.clone()));
                }
            }

            categories.insert(canon_key, spec);
        }

        genshinmap_suffix_pairs.sort_by_key(|a| Reverse(a.0.len()));

        Ok(Self {
            schema_version,
            categories,
            label_id_to_canonical,
            appsample_to_canonical,
            name_to_canonical,
            genshinmap_key_to_canonical,
            genshinmap_suffix_pairs,
            calibration_anchors,
            default_affines,
        })
    }

    pub fn is_known_category(&self, canonical_category: Option<&str>) -> bool {
        canonical_category.is_some_and(|c| !c.is_empty() && self.categories.contains_key(c))
    }

    pub fn is_calibration_anchor(&self, canonical_category: Option<&str>) -> bool {
        canonical_category
            .is_some_and(|c| !c.is_empty() && self.calibration_anchors.contains(c))
    }

    pub fn get_appsample_code(&self, canonical_category: &str) -> Option<&str> {
        self.categories
            .get(canonical_category)?
            .appsample_code
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    pub fn get_default_affine(&self, target: &str, map_id: i64) -> Option<AxisAffine> {
        let key = (target.trim().to_ascii_lowercase(), map_id);
        if let Some(aff) = self.default_affines.get(&key) {
            return Some(aff.clone());
        }
        if key.0 == "kongying" && key.1 == 2 {
            return Some(kongying_twt40_seed());
        }
        None
    }

    pub fn resolve_hoyolab_label(&self, label_id: i64, label_name: Option<&str>) -> Option<String> {
        if let Some(canon) = self.label_id_to_canonical.get(&label_id) {
            return Some(canon.clone());
        }
        if let Some(name) = label_name {
            let norm = normalize_text_key(name);
            if let Some(canon) = self.name_to_canonical.get(&norm) {
                return Some(canon.clone());
            }
        }
        None
    }

    pub fn resolve_target_category(
        &self,
        canonical_category: Option<&str>,
        appsample_code: Option<&str>,
        category_name: Option<&str>,
        feature_key: Option<&str>,
        hoyolab_label_id: Option<i64>,
    ) -> Option<String> {
        if let Some(raw_c) = canonical_category {
            let c = raw_c.trim();
            if !c.is_empty() {
                if self.categories.contains_key(c) {
                    return Some(c.to_owned());
                }
                let c_lower = c.to_ascii_lowercase();
                if let Some(found) = self.appsample_to_canonical.get(&c_lower) {
                    return Some(found.clone());
                }
                let c_norm = normalize_text_key(c);
                if let Some(found) = self.name_to_canonical.get(&c_norm) {
                    return Some(found.clone());
                }
            }
        }

        if let Some(code) = appsample_code {
            let code_norm = code.trim().to_ascii_lowercase();
            if let Some(found) = self.appsample_to_canonical.get(&code_norm) {
                return Some(found.clone());
            }
        }

        if let Some(lid) = hoyolab_label_id
            && let Some(resolved) = self.resolve_hoyolab_label(lid, None)
        {
            return Some(resolved);
        }

        if let Some(fk) = feature_key {
            let fk_raw = fk.trim();
            let fk_base = fk_raw.split('/').next().unwrap_or("").trim().to_ascii_lowercase();
            if !fk_base.is_empty() {
                if let Some(found) = self.genshinmap_key_to_canonical.get(&fk_base) {
                    return Some(found.clone());
                }
                for (suffix, canon) in &self.genshinmap_suffix_pairs {
                    if fk_base.ends_with(suffix.as_str()) {
                        return Some(canon.clone());
                    }
                }
            }
        }

        if let Some(cname) = category_name {
            let norm = normalize_text_key(cname);
            if let Some(found) = self.name_to_canonical.get(&norm) {
                return Some(found.clone());
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// HoYoLAB API Envelope Parser
// ---------------------------------------------------------------------------

fn extract_api_data<'a>(envelope: &'a Value, name: &str) -> CoreResult<&'a Value> {
    if let Some(obj) = envelope.as_object() {
        if let Some(rc) = obj.get("retcode") {
            let ok = match rc {
                Value::Null => true,
                Value::Number(n) => n.as_i64() == Some(0),
                Value::String(s) => s.trim() == "0",
                _ => false,
            };
            if !ok {
                let msg = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");
                return Err(CoreError::InvalidData(format!(
                    "HoYoLAB {name} API error: retcode={rc}, message={msg:?}"
                )));
            }
        }
        if let Some(data) = obj.get("data")
            && (data.is_object() || data.is_array())
        {
            return Ok(data);
        }
    }
    Ok(envelope)
}

fn extract_label_names(data_obj: &Value) -> HashMap<i64, String> {
    let mut names = HashMap::new();
    let Some(obj) = data_obj.as_object() else {
        return names;
    };

    fn walk_labels(items: Option<&Vec<Value>>, out: &mut HashMap<i64, String>) {
        let Some(arr) = items else {
            return;
        };
        for entry in arr {
            let Some(eobj) = entry.as_object() else {
                continue;
            };
            let raw_id = eobj
                .get("id")
                .filter(|v| !v.is_null())
                .or_else(|| eobj.get("label_id"));
            if let Some(rid) = raw_id
                && let Some(raw_name) = eobj.get("name").and_then(value_as_opt_string)
                && let Ok(lid) = coerce_strict_int(rid, "label_id")
            {
                out.insert(lid, raw_name);
            }
            if let Some(children) = eobj.get("children").and_then(|v| v.as_array()) {
                walk_labels(Some(children), out);
            }
        }
    }

    for key in ["label_list", "tree", "labels"] {
        walk_labels(obj.get(key).and_then(|v| v.as_array()), &mut names);
    }
    names
}

fn build_canonical_pin_from_hoyolab_point(
    point: &Map<String, Value>,
    default_map_id: i64,
    label_names: &HashMap<i64, String>,
    registry: &CategoryRegistry,
    marked_at: Option<&str>,
) -> CoreResult<(CanonicalPin, bool)> {
    let pid = coerce_strict_int(
        point
            .get("id")
            .ok_or_else(|| CoreError::InvalidData("Missing point.id".into()))?,
        "point.id",
    )?;
    let lid = coerce_strict_int(
        point
            .get("label_id")
            .ok_or_else(|| CoreError::InvalidData("Missing point.label_id".into()))?,
        "point.label_id",
    )?;
    let mid = match point.get("map_id") {
        Some(v) if !v.is_null() => coerce_strict_int(v, "point.map_id")?,
        _ => default_map_id,
    };
    let x_pos = coerce_f64(
        point
            .get("x_pos")
            .ok_or_else(|| CoreError::InvalidData("Missing point.x_pos".into()))?,
        "point.x_pos",
    )?;
    let y_pos = coerce_f64(
        point
            .get("y_pos")
            .ok_or_else(|| CoreError::InvalidData("Missing point.y_pos".into()))?,
        "point.y_pos",
    )?;

    let label_name = point
        .get("label_name")
        .and_then(value_as_opt_string)
        .or_else(|| label_names.get(&lid).cloned());

    let resolved = registry.resolve_hoyolab_label(lid, label_name.as_deref());
    let is_mapped = resolved.is_some();
    let canonical_category = resolved.unwrap_or_else(|| format!("unmapped_{lid}"));

    let (group_id, mut floor_id) = if let Some(pg_raw) = point.get("point_group") {
        if let Some(pg_obj) = pg_raw.as_object() {
            let gid_val = pg_obj
                .get("group_id")
                .filter(|v| is_json_truthy(v))
                .or_else(|| pg_obj.get("id"));
            (
                gid_val.and_then(normalize_point_group),
                pg_obj.get("floor_id").and_then(normalize_floor_id),
            )
        } else {
            let gid_val = if !pg_raw.is_null() {
                Some(pg_raw)
            } else {
                point.get("group_id")
            };
            (
                gid_val.and_then(normalize_point_group),
                point.get("floor_id").and_then(normalize_floor_id),
            )
        }
    } else {
        (
            point.get("group_id").and_then(normalize_point_group),
            point.get("floor_id").and_then(normalize_floor_id),
        )
    };

    if floor_id.is_none()
        && let Some(fid_val) = point.get("floor_id")
    {
        floor_id = normalize_floor_id(fid_val);
    }

    let is_underground = match point.get("is_underground") {
        Some(v) if !v.is_null() => v.as_bool().unwrap_or(false),
        _ => {
            let z_level = point
                .get("z_level")
                .and_then(|v| coerce_strict_int(v, "z_level").ok())
                .unwrap_or(0);
            let icon_sign = point
                .get("icon_sign")
                .and_then(|v| coerce_strict_int(v, "icon_sign").ok())
                .unwrap_or(0);
            floor_id.is_some()
                || group_id.is_some()
                || z_level != 0
                || matches!(icon_sign, 1..=3)
        }
    };

    let pin = CanonicalPin {
        hoyolab_id: pid,
        map_id: mid,
        label_id: lid,
        canonical_category,
        x_pos,
        y_pos,
        floor_id,
        point_group: group_id,
        is_underground,
        marked_at: marked_at.map(str::to_owned),
        label_name,
    };
    Ok((pin, is_mapped))
}

pub fn parse_hoyolab_points(
    point_list_envelope: &Value,
    map_id: i64,
    label_ids: Option<&[i64]>,
    registry: Option<&CategoryRegistry>,
) -> CoreResult<Vec<CanonicalPin>> {
    let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
    let data_obj = extract_api_data(point_list_envelope, "point/list")?;
    let label_names = extract_label_names(data_obj);

    let raw_points = if let Some(obj) = data_obj.as_object() {
        obj.get("point_list")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    } else if let Some(arr) = data_obj.as_array() {
        arr.as_slice()
    } else {
        return Err(CoreError::InvalidData(
            "point_list_envelope must contain data.point_list[] or a list".into(),
        ));
    };

    let allowed: Option<HashSet<i64>> = label_ids.map(|ids| ids.iter().copied().collect());
    let mut pins = Vec::new();
    for item in raw_points {
        let Some(pt) = item.as_object() else {
            continue;
        };
        if !pt.contains_key("id") || !pt.contains_key("label_id") {
            continue;
        }
        let lid = coerce_strict_int(&pt["label_id"], "label_id")?;
        if let Some(ref set) = allowed
            && !set.contains(&lid)
        {
            continue;
        }
        let (pin, _) =
            build_canonical_pin_from_hoyolab_point(pt, map_id, &label_names, reg, None)?;
        pins.push(pin);
    }
    Ok(pins)
}

pub fn parse_hoyolab_export(
    point_list_envelope: &Value,
    marked_list_envelope: &Value,
    map_id: i64,
    registry: Option<&CategoryRegistry>,
) -> CoreResult<HoyolabParseResult> {
    let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
    let pt_data = extract_api_data(point_list_envelope, "point/list")?;
    let mk_data = extract_api_data(marked_list_envelope, "mark_map_point_list")?;
    let label_names = extract_label_names(pt_data);

    let raw_points = if let Some(obj) = pt_data.as_object() {
        obj.get("point_list")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    } else if let Some(arr) = pt_data.as_array() {
        arr.as_slice()
    } else {
        return Err(CoreError::InvalidData(
            "point_list_envelope must contain data.point_list[] or a list".into(),
        ));
    };

    let raw_marked = if let Some(obj) = mk_data.as_object() {
        obj.get("list")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    } else if let Some(arr) = mk_data.as_array() {
        arr.as_slice()
    } else {
        return Err(CoreError::InvalidData(
            "marked_list_envelope must contain data.list[] or a list".into(),
        ));
    };

    let mut points_by_id: HashMap<i64, &Map<String, Value>> =
        HashMap::with_capacity(raw_points.len());
    for pt in raw_points {
        let Some(obj) = pt.as_object() else {
            continue;
        };
        let Some(id_val) = obj.get("id") else {
            continue;
        };
        let pid = coerce_strict_int(id_val, "point.id")?;
        points_by_id.insert(pid, obj);
    }

    let mut seen_marked: HashMap<i64, Option<String>> = HashMap::new();
    let mut marked_order: Vec<i64> = Vec::new();
    for entry in raw_marked {
        let (pid, ts_iso) = if let Some(obj) = entry.as_object() {
            let raw_pid = obj
                .get("point_id")
                .filter(|v| !v.is_null())
                .or_else(|| obj.get("id"));
            let Some(rp) = raw_pid else {
                continue;
            };
            let pid = coerce_strict_int(rp, "marked.point_id")?;
            let raw_ts = obj
                .get("marked_at")
                .filter(|v| is_json_truthy(v))
                .or_else(|| obj.get("mark_time").filter(|v| is_json_truthy(v)))
                .or_else(|| obj.get("updated_at").filter(|v| is_json_truthy(v)))
                .or_else(|| obj.get("created_at").filter(|v| is_json_truthy(v)));
            (pid, raw_ts.and_then(normalize_timestamp_iso))
        } else {
            (coerce_strict_int(entry, "marked.point_id")?, None)
        };

        if let Some(existing_ts) = seen_marked.get_mut(&pid) {
            if existing_ts.is_none() && ts_iso.is_some() {
                *existing_ts = ts_iso;
            }
        } else {
            seen_marked.insert(pid, ts_iso);
            marked_order.push(pid);
        }
    }

    let mut pins = Vec::new();
    let mut missing_from_point_list = Vec::new();
    let mut unmapped_label_set = BTreeSet::new();

    for pid in &marked_order {
        let Some(&pt) = points_by_id.get(pid) else {
            missing_from_point_list.push(*pid);
            continue;
        };
        let ts = seen_marked.get(pid).and_then(|o| o.as_deref());
        let (pin, is_mapped) =
            build_canonical_pin_from_hoyolab_point(pt, map_id, &label_names, reg, ts)?;
        if !is_mapped {
            unmapped_label_set.insert(pin.label_id);
        }
        pins.push(pin);
    }

    Ok(HoyolabParseResult {
        map_id,
        pins,
        missing_from_point_list,
        unmapped_label_ids: unmapped_label_set.into_iter().collect(),
        total_marked: marked_order.len(),
        point_index_size: points_by_id.len(),
    })
}

// ---------------------------------------------------------------------------
// Web-Mercator Helpers
// ---------------------------------------------------------------------------

pub fn web_mercator_to_pixel(lng: f64, lat: f64, zoom: i32) -> (f64, f64) {
    let world = 256.0 * 2f64.powi(zoom);
    let clamped_lat = lat.clamp(-85.05112878, 85.05112878);
    let rad = clamped_lat.to_radians();
    let px = (lng + 180.0) / 360.0 * world;
    let py = (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * world;
    (px, py)
}

pub fn pixel_to_web_mercator(px: f64, py: f64, zoom: i32) -> (f64, f64) {
    let world = 256.0 * 2f64.powi(zoom);
    let lng = (px / world) * 360.0 - 180.0;
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * (py / world);
    let lat = n.sinh().atan().to_degrees();
    (lng, lat)
}

// ---------------------------------------------------------------------------
// Per-Map Auto-Affine Calibrator
// ---------------------------------------------------------------------------

fn fit_1d_linear(xs: &[f64], ys: &[f64]) -> CoreResult<(f64, f64)> {
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|&x| (x - mx) * (x - mx)).sum();
    if sxx <= 1e-12 {
        return Err(CoreError::InvalidArgument(
            "Degenerate calibration anchors: zero variance along coordinate axis".into(),
        ));
    }
    let sxy: f64 = xs
        .iter()
        .zip(ys.iter())
        .map(|(&x, &y)| (x - mx) * (y - my))
        .sum();
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    Ok((slope, intercept))
}

fn median(values: &[f64]) -> f64 {
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let n = ordered.len();
    let mid = n / 2;
    if n % 2 == 1 {
        ordered[mid]
    } else {
        0.5 * (ordered[mid - 1] + ordered[mid])
    }
}

pub fn fit_axis_affine(
    src_points: &[(f64, f64)],
    dst_points: &[(f64, f64)],
    map_id: Option<i64>,
    robust: bool,
    outlier_sigma: f64,
    source: &str,
) -> CoreResult<AxisAffine> {
    if src_points.len() != dst_points.len() {
        return Err(CoreError::InvalidArgument(format!(
            "src_points ({}) and dst_points ({}) must have equal length",
            src_points.len(),
            dst_points.len()
        )));
    }
    let n = src_points.len();
    if n < 2 {
        return Err(CoreError::InvalidArgument(format!(
            "At least 2 anchor pairs are required to fit AxisAffine, got {n}"
        )));
    }

    let src_x: Vec<f64> = src_points.iter().map(|p| p.0).collect();
    let src_y: Vec<f64> = src_points.iter().map(|p| p.1).collect();
    let dst_x: Vec<f64> = dst_points.iter().map(|p| p.0).collect();
    let dst_y: Vec<f64> = dst_points.iter().map(|p| p.1).collect();
    if src_x.iter().any(|v| !v.is_finite())
        || src_y.iter().any(|v| !v.is_finite())
        || dst_x.iter().any(|v| !v.is_finite())
        || dst_y.iter().any(|v| !v.is_finite())
    {
        return Err(CoreError::InvalidArgument(
            "Anchor coordinates must be finite".into(),
        ));
    }

    let (mut ax, mut bx) = fit_1d_linear(&src_x, &dst_x)?;
    let (mut cy, mut dy) = fit_1d_linear(&src_y, &dst_y)?;

    let residuals: Vec<f64> = (0..n)
        .map(|i| (ax * src_x[i] + bx - dst_x[i]).hypot(cy * src_y[i] + dy - dst_y[i]))
        .collect();

    let mut inlier_indices: Vec<usize> = (0..n).collect();
    if robust && n >= 4 {
        let med = median(&residuals);
        let abs_devs: Vec<f64> = residuals.iter().map(|&r| (r - med).abs()).collect();
        let mad = median(&abs_devs);
        let threshold = 5.0_f64.max(med + outlier_sigma * 1.4826 * mad);
        let candidates: Vec<usize> = residuals
            .iter()
            .enumerate()
            .filter_map(|(i, &r)| if r <= threshold { Some(i) } else { None })
            .collect();
        if candidates.len() >= 3 && candidates.len() < n {
            let csx: Vec<f64> = candidates.iter().map(|&i| src_x[i]).collect();
            let cdx: Vec<f64> = candidates.iter().map(|&i| dst_x[i]).collect();
            let csy: Vec<f64> = candidates.iter().map(|&i| src_y[i]).collect();
            let cdy: Vec<f64> = candidates.iter().map(|&i| dst_y[i]).collect();
            if let (Ok((ax_r, bx_r)), Ok((cy_r, dy_r))) =
                (fit_1d_linear(&csx, &cdx), fit_1d_linear(&csy, &cdy))
            {
                ax = ax_r;
                bx = bx_r;
                cy = cy_r;
                dy = dy_r;
                inlier_indices = candidates;
            }
        }
    }

    let inlier_residuals: Vec<f64> = inlier_indices
        .iter()
        .map(|&i| (ax * src_x[i] + bx - dst_x[i]).hypot(cy * src_y[i] + dy - dst_y[i]))
        .collect();
    let rmse =
        (inlier_residuals.iter().map(|&r| r * r).sum::<f64>() / inlier_residuals.len() as f64)
            .sqrt();
    let max_res = inlier_residuals.iter().copied().fold(0.0_f64, f64::max);

    Ok(AxisAffine {
        ax,
        bx,
        cy,
        dy,
        map_id,
        rmse,
        max_residual: max_res,
        anchor_count: inlier_indices.len(),
        source: source.to_owned(),
    })
}

#[derive(Debug, Clone)]
pub struct PerMapAffineRegistry {
    transforms: HashMap<(String, i64), AxisAffine>,
    category_registry: CategoryRegistry,
}

impl Default for PerMapAffineRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PerMapAffineRegistry {
    pub fn new() -> Self {
        Self {
            transforms: HashMap::new(),
            category_registry: CategoryRegistry::embedded().clone(),
        }
    }

    pub fn with_registry(category_registry: CategoryRegistry) -> Self {
        Self {
            transforms: HashMap::new(),
            category_registry,
        }
    }

    pub fn set(&mut self, target: &str, map_id: i64, mut affine: AxisAffine) -> CoreResult<()> {
        if let Some(m) = affine.map_id
            && m != map_id
        {
            return Err(CoreError::InvalidArgument(format!(
                "Affine map_id={m} does not match registration map_id={map_id}"
            )));
        }
        affine.map_id = Some(map_id);
        self.transforms
            .insert((target.trim().to_ascii_lowercase(), map_id), affine);
        Ok(())
    }

    pub fn get(&self, target: &str, map_id: i64, allow_default_seed: bool) -> Option<AxisAffine> {
        let key = (target.trim().to_ascii_lowercase(), map_id);
        if let Some(aff) = self.transforms.get(&key) {
            return Some(aff.clone());
        }
        if allow_default_seed {
            return self.category_registry.get_default_affine(&key.0, key.1);
        }
        None
    }

    pub fn require(&self, target: &str, map_id: i64, allow_default_seed: bool) -> CoreResult<AxisAffine> {
        self.get(target, map_id, allow_default_seed).ok_or_else(|| {
            CoreError::NotFound(format!(
                "No AxisAffine calibrated for target={target:?}, map_id={map_id}. Sub-maps must be calibrated with fit_axis_affine() using shared anchors."
            ))
        })
    }
}

pub fn calibrate_from_shared_anchors(
    src_anchors: &[CanonicalPin],
    dst_anchors: &[TargetPin],
    map_id: i64,
    initial_seed: &AxisAffine,
    max_seed_dist: f64,
    registry: Option<&CategoryRegistry>,
) -> CoreResult<AxisAffine> {
    let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
    if let Some(m) = initial_seed.map_id
        && m != map_id
    {
        return Err(CoreError::InvalidArgument(format!(
            "Seed map_id={m} cannot be used to calibrate map_id={map_id}"
        )));
    }
    let anchor_src: Vec<CanonicalPin> = src_anchors
        .iter()
        .filter(|p| p.map_id == map_id && reg.is_calibration_anchor(Some(&p.canonical_category)))
        .cloned()
        .collect();
    if anchor_src.len() < 2 {
        return Err(CoreError::InvalidArgument(format!(
            "Need at least 2 landmark source anchors on map_id={map_id}, got {}",
            anchor_src.len()
        )));
    }
    let seed_for_map = AxisAffine {
        ax: initial_seed.ax,
        bx: initial_seed.bx,
        cy: initial_seed.cy,
        dy: initial_seed.dy,
        map_id: Some(map_id),
        rmse: initial_seed.rmse,
        max_residual: initial_seed.max_residual,
        anchor_count: initial_seed.anchor_count,
        source: initial_seed.source.clone(),
    };
    let report = match_pins_one_to_one(
        &anchor_src,
        dst_anchors,
        &seed_for_map,
        MatchOptions {
            max_dist: max_seed_dist,
            ambiguity_ratio: 1.25,
            ambiguity_abs_dist: 0.0,
            allow_name_fallback: true,
            use_optimal_assignment: true,
        },
        Some(reg),
        None,
    )?;
    if report.matched.len() < 2 {
        return Err(CoreError::InvalidArgument(format!(
            "Anchor refinement found only {} paired landmarks within max_seed_dist={max_seed_dist}",
            report.matched.len()
        )));
    }
    let src_pts: Vec<(f64, f64)> = report
        .matched
        .iter()
        .map(|m| (m.source.x_pos, m.source.y_pos))
        .collect();
    let dst_pts: Vec<(f64, f64)> = report
        .matched
        .iter()
        .map(|m| (m.target.x, m.target.y))
        .collect();
    fit_axis_affine(
        &src_pts,
        &dst_pts,
        Some(map_id),
        true,
        3.5,
        &format!("refined:{}", initial_seed.source),
    )
}

// ---------------------------------------------------------------------------
// Optimal 1-to-1 Bipartite Assignment (Connected-Component SSAP)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FlowEdge {
    to: usize,
    rev: usize,
    cap: i32,
    cost: i64,
}

fn solve_component_ssap(
    nu: usize,
    nv: usize,
    local_edges: &[(usize, usize, f64)],
) -> Vec<(usize, usize)> {
    if local_edges.is_empty() {
        return Vec::new();
    }
    if nu == 1 {
        let best_v = local_edges
            .iter()
            .min_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.1.cmp(&b.1)))
            .map(|e| e.1)
            .unwrap();
        return vec![(0, best_v)];
    }
    if nv == 1 {
        let best_u = local_edges
            .iter()
            .min_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.0.cmp(&b.0)))
            .map(|e| e.0)
            .unwrap();
        return vec![(best_u, 0)];
    }

    let src = 0usize;
    let snk = nu + nv + 1;
    let num_nodes = snk + 1;
    let mut adj: Vec<Vec<FlowEdge>> = vec![Vec::new(); num_nodes];

    let add_edge = |adj: &mut Vec<Vec<FlowEdge>>, u: usize, v: usize, cap: i32, cost: i64| -> usize {
        let fwd_idx = adj[u].len();
        let rev_idx = adj[v].len();
        adj[u].push(FlowEdge {
            to: v,
            rev: rev_idx,
            cap,
            cost,
        });
        adj[v].push(FlowEdge {
            to: u,
            rev: fwd_idx,
            cap: 0,
            cost: -cost,
        });
        fwd_idx
    };

    for i in 0..nu {
        add_edge(&mut adj, src, 1 + i, 1, 0);
    }
    for j in 0..nv {
        add_edge(&mut adj, 1 + nu + j, snk, 1, 0);
    }

    let mut uv_edge_refs: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(local_edges.len());
    for &(u_idx, v_idx, dist_val) in local_edges {
        let u_node = 1 + u_idx;
        let v_node = 1 + nu + v_idx;
        let int_cost = ((dist_val * MICRO_SCALE).round() as i64).max(0);
        let fwd_idx = add_edge(&mut adj, u_node, v_node, 1, int_cost);
        uv_edge_refs.push((u_idx, v_idx, u_node, fwd_idx));
    }

    let mut pot = vec![0i64; num_nodes];
    let inf: i64 = 1i64 << 62;

    loop {
        let mut dist = vec![inf; num_nodes];
        let mut parent_node = vec![usize::MAX; num_nodes];
        let mut parent_edge = vec![usize::MAX; num_nodes];
        dist[src] = 0;
        let mut pq: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();
        pq.push(Reverse((0, src)));

        while let Some(Reverse((d, u))) = pq.pop() {
            if d != dist[u] {
                continue;
            }
            let pot_u = pot[u];
            for (e_idx, edge) in adj[u].iter().enumerate() {
                if edge.cap <= 0 {
                    continue;
                }
                let v = edge.to;
                let nd = d + edge.cost + pot_u - pot[v];
                if nd < dist[v] {
                    dist[v] = nd;
                    parent_node[v] = u;
                    parent_edge[v] = e_idx;
                    pq.push(Reverse((nd, v)));
                }
            }
        }

        if dist[snk] == inf {
            break;
        }

        for node in 0..num_nodes {
            if dist[node] < inf {
                pot[node] += dist[node];
            }
        }

        let mut cur = snk;
        while cur != src {
            let prev = parent_node[cur];
            let e_idx = parent_edge[cur];
            let rev_idx = adj[prev][e_idx].rev;
            adj[prev][e_idx].cap -= 1;
            adj[cur][rev_idx].cap += 1;
            cur = prev;
        }
    }

    let mut matched_pairs = Vec::new();
    for (u_idx, v_idx, u_node, fwd_idx) in uv_edge_refs {
        if adj[u_node][fwd_idx].cap == 0 {
            matched_pairs.push((u_idx, v_idx));
        }
    }
    matched_pairs
}

pub fn solve_bipartite_one_to_one(
    edges: &[(usize, usize, f64)],
    use_optimal: bool,
) -> HashMap<usize, (usize, f64)> {
    if edges.is_empty() {
        return HashMap::new();
    }
    if !use_optimal {
        let mut sorted = edges.to_vec();
        sorted.sort_by(|a, b| {
            a.2.total_cmp(&b.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        let mut assigned_u = HashMap::new();
        let mut used_v = HashSet::new();
        for (u, v, d) in sorted {
            if !assigned_u.contains_key(&u) && used_v.insert(v) {
                assigned_u.insert(u, (v, d));
            }
        }
        return assigned_u;
    }

    let mut u_adj: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
    let mut v_adj: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut edge_dist: HashMap<(usize, usize), f64> = HashMap::new();

    for &(u, v, d) in edges {
        edge_dist
            .entry((u, v))
            .and_modify(|prev| {
                if d < *prev {
                    *prev = d;
                }
            })
            .or_insert(d);
    }

    for (&(u, v), &d) in &edge_dist {
        u_adj.entry(u).or_default().push((v, d));
        v_adj.entry(v).or_default().push(u);
    }

    let mut visited_u = HashSet::new();
    let mut visited_v = HashSet::new();
    let mut assignment = HashMap::new();

    let u_keys: Vec<usize> = u_adj.keys().copied().collect();
    for start_u in u_keys {
        if visited_u.contains(&start_u) {
            continue;
        }
        let mut comp_u = Vec::new();
        let mut comp_v = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back((true, start_u));
        visited_u.insert(start_u);

        while let Some((is_u, node)) = queue.pop_front() {
            if is_u {
                comp_u.push(node);
                if let Some(nbs) = u_adj.get(&node) {
                    for &(v_nb, _) in nbs {
                        if visited_v.insert(v_nb) {
                            queue.push_back((false, v_nb));
                        }
                    }
                }
            } else {
                comp_v.push(node);
                if let Some(nbs) = v_adj.get(&node) {
                    for &u_nb in nbs {
                        if visited_u.insert(u_nb) {
                            queue.push_back((true, u_nb));
                        }
                    }
                }
            }
        }

        comp_u.sort_unstable();
        comp_v.sort_unstable();
        let u_to_local: HashMap<usize, usize> =
            comp_u.iter().enumerate().map(|(i, &u)| (u, i)).collect();
        let v_to_local: HashMap<usize, usize> =
            comp_v.iter().enumerate().map(|(i, &v)| (v, i)).collect();

        let mut local_edges = Vec::new();
        for &u in &comp_u {
            let lu = u_to_local[&u];
            if let Some(nbs) = u_adj.get(&u) {
                for &(v, d) in nbs {
                    local_edges.push((lu, v_to_local[&v], d));
                }
            }
        }

        for (lu, lv) in solve_component_ssap(comp_u.len(), comp_v.len(), &local_edges) {
            let gu = comp_u[lu];
            let gv = comp_v[lv];
            assignment.insert(gu, (gv, edge_dist[&(gu, gv)]));
        }
    }

    assignment
}

// ---------------------------------------------------------------------------
// Spatial + Category + Underground Layer Matcher
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchOptions {
    pub max_dist: f64,
    pub ambiguity_ratio: f64,
    pub ambiguity_abs_dist: f64,
    pub allow_name_fallback: bool,
    pub use_optimal_assignment: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            max_dist: 15.0,
            ambiguity_ratio: 1.5,
            ambiguity_abs_dist: 6.0,
            allow_name_fallback: true,
            use_optimal_assignment: true,
        }
    }
}

fn layers_compatible(src: &CanonicalPin, dst: &TargetPin) -> bool {
    let src_floor = src
        .floor_id
        .as_deref()
        .and_then(|s| normalize_floor_id(&Value::String(s.to_owned())));
    let dst_floor = dst
        .floor_id
        .as_deref()
        .and_then(|s| normalize_floor_id(&Value::String(s.to_owned())));

    let dst_ug = dst.is_underground.or_else(|| {
        dst_floor
            .as_deref()
            .map(|f| !SURFACE_FLOOR_TOKENS.contains(&f.to_ascii_lowercase().as_str()))
    });

    if let Some(ug) = dst_ug
        && ug != src.is_underground
    {
        return false;
    }

    if let (Some(sf), Some(df)) = (&src_floor, &dst_floor)
        && !sf.eq_ignore_ascii_case(df)
    {
        return false;
    }

    if let (Some(spg), Some(dpg)) = (src.point_group, dst.point_group)
        && spg != dpg
    {
        return false;
    }

    true
}

fn resolve_source_bucket(
    pin: &CanonicalPin,
    registry: &CategoryRegistry,
) -> (Option<String>, Option<String>, Option<String>) {
    let cat = pin.canonical_category.trim();
    let label_name_norm = pin
        .label_name
        .as_deref()
        .map(normalize_text_key)
        .filter(|s| !s.is_empty());

    if !cat.is_empty() && !cat.starts_with("unmapped_") {
        if registry.is_known_category(Some(cat)) {
            return (Some(cat.to_owned()), label_name_norm, None);
        }
        if let Some(resolved) = registry.resolve_target_category(
            Some(cat),
            None,
            pin.label_name.as_deref(),
            None,
            None,
        ) {
            return (Some(resolved), label_name_norm, None);
        }
        return (Some(cat.to_owned()), label_name_norm, None);
    }

    if let Some(resolved) = registry.resolve_hoyolab_label(pin.label_id, pin.label_name.as_deref()) {
        return (Some(resolved), label_name_norm, None);
    }

    let unmapped_tag = if cat.is_empty() {
        format!("unmapped_{}", pin.label_id)
    } else {
        cat.to_owned()
    };
    (None, label_name_norm, Some(unmapped_tag))
}

fn resolve_target_bucket(
    pin: &TargetPin,
    registry: &CategoryRegistry,
) -> (Option<String>, Option<String>) {
    let name_norm = pin
        .category_name
        .as_deref()
        .map(normalize_text_key)
        .filter(|s| !s.is_empty());

    if let Some(resolved) = registry.resolve_target_category(
        pin.canonical_category.as_deref(),
        pin.appsample_code.as_deref(),
        pin.category_name.as_deref(),
        pin.feature_key.as_deref(),
        None,
    ) {
        return (Some(resolved), name_norm);
    }

    let raw_cat = pin.canonical_category.as_deref().unwrap_or("").trim();
    if !raw_cat.is_empty() && !raw_cat.starts_with("unmapped_") {
        return (Some(raw_cat.to_owned()), name_norm);
    }

    (None, name_norm)
}

pub fn match_pins_one_to_one(
    source_pins: &[CanonicalPin],
    target_pins: &[TargetPin],
    affine: &AxisAffine,
    options: MatchOptions,
    registry: Option<&CategoryRegistry>,
    missing_from_point_list: Option<&[i64]>,
) -> CoreResult<TransferReport> {
    if !options.max_dist.is_finite() || options.max_dist < 0.0 {
        return Err(CoreError::InvalidArgument(format!(
            "max_dist must be finite and non-negative, got {}",
            options.max_dist
        )));
    }
    if !options.ambiguity_ratio.is_finite() || options.ambiguity_ratio < 1.0 {
        return Err(CoreError::InvalidArgument(format!(
            "ambiguity_ratio must be >= 1.0, got {}",
            options.ambiguity_ratio
        )));
    }

    let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
    let missing_ids: Vec<i64> = missing_from_point_list.unwrap_or(&[]).to_vec();

    if let Some(aff_mid) = affine.map_id {
        for pin in source_pins {
            if pin.map_id != aff_mid {
                return Err(CoreError::InvalidArgument(format!(
                    "Map ID mismatch: source pin {} has map_id={}, but affine is calibrated for map_id={aff_mid}",
                    pin.hoyolab_id, pin.map_id
                )));
            }
        }
    }

    let mut target_by_category: HashMap<String, Vec<usize>> = HashMap::new();
    let mut target_by_name: HashMap<String, Vec<usize>> = HashMap::new();

    for (t_idx, t_pin) in target_pins.iter().enumerate() {
        if let (Some(aff_mid), Some(t_mid)) = (affine.map_id, t_pin.map_id)
            && t_mid != aff_mid
        {
            continue;
        }
        let (t_canon, t_name) = resolve_target_bucket(t_pin, reg);
        if let Some(canon) = t_canon {
            target_by_category.entry(canon).or_default().push(t_idx);
        }
        if let Some(name) = t_name {
            target_by_name.entry(name).or_default().push(t_idx);
        }
    }

    let mut bucket_sources: BTreeMap<(u8, String), Vec<usize>> = BTreeMap::new();
    let mut unmapped_categories_set: BTreeSet<String> = BTreeSet::new();
    let mut unmatched: Vec<UnmatchedPin> = Vec::new();
    let projected_coords: Vec<(f64, f64)> = source_pins
        .iter()
        .map(|pin| affine.transform(pin.x_pos, pin.y_pos))
        .collect();

    for (s_idx, s_pin) in source_pins.iter().enumerate() {
        let (s_canon, s_name, unmapped_tag) = resolve_source_bucket(s_pin, reg);
        if let Some(canon) = s_canon {
            bucket_sources.entry((0, canon)).or_default().push(s_idx);
        } else if options.allow_name_fallback
            && let Some(ref name) = s_name
            && target_by_name.contains_key(name)
        {
            bucket_sources
                .entry((1, name.clone()))
                .or_default()
                .push(s_idx);
        } else {
            if let Some(tag) = unmapped_tag {
                unmapped_categories_set.insert(tag);
            }
            unmatched.push(UnmatchedPin {
                source: s_pin.clone(),
                reason: "unmapped_category".to_owned(),
                nearest_distance: None,
                nearest_target_id: None,
            });
        }
    }

    let mut matched: Vec<MatchResult> = Vec::new();
    let mut used_target_indices: HashSet<usize> = HashSet::new();

    for ((mode_tag, bucket_key), s_indices) in bucket_sources {
        let mode = if mode_tag == 0 {
            "canonical_category"
        } else {
            "name_fallback"
        };
        let raw_t_indices = if mode_tag == 0 {
            target_by_category.get(&bucket_key)
        } else {
            target_by_name.get(&bucket_key)
        };
        let t_indices: Vec<usize> = raw_t_indices
            .map(|list| {
                list.iter()
                    .copied()
                    .filter(|ti| !used_target_indices.contains(ti))
                    .collect()
            })
            .unwrap_or_default();

        if t_indices.is_empty() {
            for s_idx in s_indices {
                unmatched.push(UnmatchedPin {
                    source: source_pins[s_idx].clone(),
                    reason: "no_target_in_category".to_owned(),
                    nearest_distance: None,
                    nearest_target_id: None,
                });
            }
            continue;
        }

        let mut edges: Vec<(usize, usize, f64)> = Vec::new();
        let mut compat_dists_by_source: HashMap<usize, Vec<(f64, usize)>> = HashMap::new();
        let mut had_layer_mismatch: HashMap<usize, bool> = HashMap::new();

        let use_spatial_grid = (s_indices.len() * t_indices.len()) > 25_000;
        let cell_size = (options.max_dist * options.ambiguity_ratio)
            .max(options.ambiguity_abs_dist)
            .max(1.0);
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        if use_spatial_grid {
            for &t_idx in &t_indices {
                let tp = &target_pins[t_idx];
                let cell = (
                    (tp.x / cell_size).floor() as i64,
                    (tp.y / cell_size).floor() as i64,
                );
                grid.entry(cell).or_default().push(t_idx);
            }
        }

        for &s_idx in &s_indices {
            let s_pin = &source_pins[s_idx];
            let (px, py) = projected_coords[s_idx];
            let mut compat_list: Vec<(f64, usize)> = Vec::new();
            let mut incompatible_count = 0usize;

            let cand_t_indices: Vec<usize> = if use_spatial_grid {
                let cx = (px / cell_size).floor() as i64;
                let cy = (py / cell_size).floor() as i64;
                let mut cands = Vec::new();
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        if let Some(bucket_cells) = grid.get(&(cx + dx, cy + dy)) {
                            cands.extend(bucket_cells.iter().copied());
                        }
                    }
                }
                cands
            } else {
                t_indices.clone()
            };

            for t_idx in cand_t_indices {
                let t_pin = &target_pins[t_idx];
                if !layers_compatible(s_pin, t_pin) {
                    incompatible_count += 1;
                    continue;
                }
                let d = (px - t_pin.x).hypot(py - t_pin.y);
                compat_list.push((d, t_idx));
                if d <= options.max_dist {
                    edges.push((s_idx, t_idx, d));
                }
            }

            compat_list.sort_by(|a, b| {
                a.0.total_cmp(&b.0).then_with(|| {
                    target_pins[a.1]
                        .target_id
                        .to_string()
                        .cmp(&target_pins[b.1].target_id.to_string())
                })
            });
            had_layer_mismatch.insert(s_idx, compat_list.is_empty() && incompatible_count > 0);
            compat_dists_by_source.insert(s_idx, compat_list);
        }

        let assignment = solve_bipartite_one_to_one(&edges, options.use_optimal_assignment);

        for s_idx in s_indices {
            let s_pin = &source_pins[s_idx];
            let compat_list = &compat_dists_by_source[&s_idx];
            if let Some(&(t_idx, d_assigned)) = assignment.get(&s_idx) {
                used_target_indices.insert(t_idx);
                let t_pin = &target_pins[t_idx];
                let (px, py) = projected_coords[s_idx];

                let second_best = compat_list
                    .iter()
                    .find(|(_, cand_ti)| *cand_ti != t_idx)
                    .map(|(cand_d, _)| *cand_d);

                let is_ambiguous = second_best.is_some_and(|d2| {
                    d2 <= d_assigned * options.ambiguity_ratio || d2 < options.ambiguity_abs_dist
                });

                matched.push(MatchResult {
                    source: s_pin.clone(),
                    target: t_pin.clone(),
                    distance: d_assigned,
                    second_best_distance: second_best,
                    is_ambiguous,
                    matched_via: mode.to_owned(),
                    projected_x: px,
                    projected_y: py,
                });
            } else if had_layer_mismatch.get(&s_idx).copied().unwrap_or(false) {
                unmatched.push(UnmatchedPin {
                    source: s_pin.clone(),
                    reason: "layer_mismatch".to_owned(),
                    nearest_distance: None,
                    nearest_target_id: None,
                });
            } else if compat_list.is_empty() {
                unmatched.push(UnmatchedPin {
                    source: s_pin.clone(),
                    reason: if use_spatial_grid {
                        "exceeds_max_dist".to_owned()
                    } else {
                        "no_target_in_category".to_owned()
                    },
                    nearest_distance: None,
                    nearest_target_id: None,
                });
            } else {
                let (best_d, best_ti) = compat_list[0];
                let reason = if best_d <= options.max_dist {
                    "capacity_exhausted"
                } else {
                    "exceeds_max_dist"
                };
                unmatched.push(UnmatchedPin {
                    source: s_pin.clone(),
                    reason: reason.to_owned(),
                    nearest_distance: Some(best_d),
                    nearest_target_id: Some(target_pins[best_ti].target_id.clone()),
                });
            }
        }
    }

    let ambiguous: Vec<MatchResult> = matched.iter().filter(|m| m.is_ambiguous).cloned().collect();
    let total_considered = source_pins.len() + missing_ids.len();
    let match_rate_pct = if total_considered > 0 {
        100.0 * (matched.len() as f64) / (total_considered as f64)
    } else {
        0.0
    };

    Ok(TransferReport {
        matched,
        ambiguous,
        unmatched,
        unmapped_categories: unmapped_categories_set.into_iter().collect(),
        missing_from_point_list: missing_ids,
        match_rate_pct,
    })
}

// ---------------------------------------------------------------------------
// Target Adapters (Kongying, Appsample, genshinmap.github.io, Canonical JSON)
// ---------------------------------------------------------------------------

fn select_matches(matches: &[MatchResult], include_ambiguous: bool) -> Vec<&MatchResult> {
    if include_ambiguous {
        matches.iter().collect()
    } else {
        matches.iter().filter(|m| !m.is_ambiguous).collect()
    }
}

fn is_valid_kongying_time(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let b = s.as_bytes();
    b.len() == 19
        && b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'/'
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[7] == b'/'
        && b[8..10].iter().all(|c| c.is_ascii_digit())
        && b[10] == b' '
        && b[11..13].iter().all(|c| c.is_ascii_digit())
        && b[13] == b':'
        && b[14..16].iter().all(|c| c.is_ascii_digit())
        && b[16] == b':'
        && b[17..19].iter().all(|c| c.is_ascii_digit())
}

pub struct KongyingAdapter;

impl KongyingAdapter {
    pub fn parse_markers(
        markers: &Value,
        map_id: i64,
        registry: Option<&CategoryRegistry>,
    ) -> CoreResult<Vec<TargetPin>> {
        let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
        let arr = markers.as_array().ok_or_else(|| {
            CoreError::InvalidData("Kongying markers must be a JSON array".into())
        })?;
        let mut out = Vec::new();
        for item in arr {
            let Some(m) = item.as_object() else {
                continue;
            };
            let Some(id_val) = m.get("id") else {
                continue;
            };
            let mid = coerce_strict_int(id_val, "marker.id")?;

            let (x_val, y_val) = match m.get("position") {
                Some(Value::String(s)) if s.contains(',') => {
                    let (px_s, py_s) = s.split_once(',').unwrap();
                    (
                        px_s.trim().parse::<f64>().map_err(|_| {
                            CoreError::InvalidData("Invalid Kongying position x".into())
                        })?,
                        py_s.trim().parse::<f64>().map_err(|_| {
                            CoreError::InvalidData("Invalid Kongying position y".into())
                        })?,
                    )
                }
                Some(Value::Array(pos_arr)) if pos_arr.len() >= 2 => (
                    coerce_f64(&pos_arr[0], "position[0]")?,
                    coerce_f64(&pos_arr[1], "position[1]")?,
                ),
                _ => {
                    let x = m
                        .get("x")
                        .ok_or_else(|| CoreError::InvalidData("Missing marker x".into()))?;
                    let y = m
                        .get("y")
                        .ok_or_else(|| CoreError::InvalidData("Missing marker y".into()))?;
                    (coerce_f64(x, "marker.x")?, coerce_f64(y, "marker.y")?)
                }
            };

            let mut cat_name = m
                .get("category_name")
                .and_then(value_as_opt_string)
                .or_else(|| m.get("item_name").and_then(value_as_opt_string))
                .or_else(|| m.get("markerTitle").and_then(value_as_opt_string));
            if cat_name.is_none()
                && let Some(items) = m.get("itemList").and_then(|v| v.as_array())
                && let Some(first) = items.first().and_then(|v| v.as_object())
            {
                cat_name = first
                    .get("name")
                    .and_then(value_as_opt_string)
                    .or_else(|| first.get("itemName").and_then(value_as_opt_string));
            }

            let canon = reg.resolve_target_category(
                m.get("canonical_category").and_then(|v| v.as_str()),
                None,
                cat_name.as_deref(),
                None,
                None,
            );

            let mut is_ug: Option<bool> = None;
            if let Some(v) = m.get("is_underground").filter(|v| !v.is_null()) {
                is_ug = v.as_bool();
            } else if let Some(v) = m.get("underground").filter(|v| !v.is_null()) {
                is_ug = v.as_bool();
            } else if let Some(hf_val) = m.get("hiddenFlag").filter(|v| !v.is_null())
                && let Ok(hf) = coerce_strict_int(hf_val, "hiddenFlag")
            {
                if hf == 2 {
                    is_ug = Some(true);
                } else if hf == 0 {
                    is_ug = Some(false);
                }
            }

            let mut floor_id = m.get("floor_id").and_then(normalize_floor_id);
            if floor_id.is_none()
                && let Some(extra_obj) = m.get("extra").and_then(|v| v.as_object())
                && let Some(ug_meta) = extra_obj.get("underground").and_then(|v| v.as_object())
            {
                if let Some(ug_flag) = ug_meta.get("is_underground").and_then(|v| v.as_bool()) {
                    is_ug = Some(ug_flag);
                }
                if let Some(rf) = ug_meta.get("floor_id").filter(|v| !v.is_null()) {
                    floor_id = normalize_floor_id(rf);
                } else if let Some(rl) = ug_meta.get("region_levels").filter(|v| !v.is_null()) {
                    if let Some(arr) = rl.as_array() {
                        if let Some(first) = arr.first() {
                            floor_id = normalize_floor_id(first);
                        }
                    } else {
                        floor_id = normalize_floor_id(rl);
                    }
                }
            }

            let marker_map_id = m
                .get("map_id")
                .and_then(|v| coerce_strict_int(v, "map_id").ok())
                .unwrap_or(map_id);
            let mut extra = BTreeMap::new();
            extra.insert(
                "markerTitle".to_owned(),
                m.get("markerTitle")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            );

            out.push(TargetPin {
                target_id: TargetId::Int(mid),
                x: x_val,
                y: y_val,
                canonical_category: canon,
                category_name: cat_name,
                appsample_code: None,
                feature_key: None,
                map_id: Some(marker_map_id),
                floor_id,
                point_group: None,
                is_underground: is_ug,
                extra,
            });
        }
        Ok(out)
    }

    pub fn export(
        report: &TransferReport,
        include_ambiguous: bool,
        updated_at_ms: Option<i64>,
        default_timestamp: Option<&Value>,
    ) -> CoreResult<Value> {
        let selected = select_matches(&report.matched, include_ambiguous);
        let now_ms = updated_at_ms.unwrap_or_else(now_unix_ms);
        let fallback_sec = default_timestamp
            .and_then(parse_timestamp_to_unix_sec)
            .unwrap_or_else(|| now_ms.div_euclid(1000));

        let mut ids_set = BTreeSet::new();
        let mut time_map: HashMap<i64, String> = HashMap::new();
        for m in selected {
            let tid = m.target.target_id.to_strict_int("kongying.target_id")?;
            ids_set.insert(tid);
            let sec = m
                .source
                .marked_at
                .as_deref()
                .and_then(|s| parse_timestamp_to_unix_sec(&Value::String(s.to_owned())))
                .unwrap_or(fallback_sec);
            time_map.insert(tid, format_kongying_time(sec));
        }

        let sorted_ids: Vec<i64> = ids_set.into_iter().collect();
        let mut ordered_times = Map::new();
        for tid in &sorted_ids {
            if let Some(ts) = time_map.get(tid) {
                ordered_times.insert(tid.to_string(), Value::String(ts.clone()));
            }
        }
        Ok(json!({
            "Data_KYJG": sorted_ids,
            "Time_KYJG": ordered_times,
            "updated_at": now_ms,
        }))
    }

    pub fn validate_schema(payload: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(obj) = payload.as_object() else {
            return vec!["Payload must be a JSON object".to_owned()];
        };
        match obj.get("Data_KYJG").and_then(|v| v.as_array()) {
            None => errors.push("Data_KYJG must be a flat list[int]".to_owned()),
            Some(arr) => {
                let mut ints = Vec::with_capacity(arr.len());
                let mut all_ints = true;
                for item in arr {
                    if let Some(i) = is_strict_json_int(item) {
                        ints.push(i);
                    } else {
                        all_ints = false;
                    }
                }
                if !all_ints {
                    errors.push("Data_KYJG elements must all be strict integers".to_owned());
                } else {
                    let is_strictly_sorted = ints.windows(2).all(|w| w[0] < w[1]);
                    if !is_strictly_sorted {
                        errors.push("Data_KYJG must be sorted and deduplicated".to_owned());
                    }
                }
            }
        }
        match obj.get("Time_KYJG").and_then(|v| v.as_object()) {
            None => errors.push("Time_KYJG must be a dict[str, str]".to_owned()),
            Some(tmap) => {
                for (k, v) in tmap {
                    let k_digits = k.strip_prefix('-').unwrap_or(k.as_str());
                    if k_digits.is_empty() || !k_digits.chars().all(|c| c.is_ascii_digit()) {
                        errors.push(format!("Time_KYJG key {k:?} must be a numeric string ID"));
                    }
                    match v.as_str() {
                        Some(s) if is_valid_kongying_time(s) => {}
                        _ => errors.push(format!(
                            "Time_KYJG[{k:?}] must match YYYY/MM/DD HH:mm:ss or '', got {v}"
                        )),
                    }
                }
            }
        }
        match obj.get("updated_at").and_then(is_strict_json_int) {
            Some(u) if u >= 0 => {}
            _ => errors.push("updated_at must be a non-negative integer epoch_ms".to_owned()),
        }
        errors
    }

    pub fn parse(raw: &Value) -> CoreResult<Value> {
        let data: Value = match raw {
            Value::String(s) => serde_json::from_str(s)?,
            other => other.clone(),
        };
        if !data.is_object() {
            return Err(CoreError::InvalidData(
                "Kongying archive must be a JSON object".into(),
            ));
        }
        let errors = Self::validate_schema(&data);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Kongying archive: {}",
                errors.join("; ")
            )));
        }
        let ids: BTreeSet<i64> = data["Data_KYJG"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| coerce_strict_int(v, "Data_KYJG[]"))
            .collect::<CoreResult<BTreeSet<i64>>>()?;
        let sorted_ids: Vec<i64> = ids.into_iter().collect();
        let mut tmap = Map::new();
        for (k, v) in data["Time_KYJG"].as_object().unwrap() {
            tmap.insert(k.clone(), Value::String(v.as_str().unwrap_or("").to_owned()));
        }
        let updated_at = is_strict_json_int(&data["updated_at"]).unwrap();
        Ok(json!({
            "Data_KYJG": sorted_ids,
            "Time_KYJG": tmap,
            "updated_at": updated_at,
        }))
    }

    pub fn to_json(payload: &Value, pretty: bool) -> CoreResult<String> {
        let errors = Self::validate_schema(payload);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Kongying archive payload: {}",
                errors.join("; ")
            )));
        }
        if pretty {
            serde_json::to_string_pretty(payload).map_err(CoreError::Json)
        } else {
            serde_json::to_string(payload).map_err(CoreError::Json)
        }
    }

    pub fn merge_existing(
        existing: &Value,
        exported: &Value,
        updated_at_ms: Option<i64>,
    ) -> CoreResult<Value> {
        let base = Self::parse(existing)?;
        let inc = Self::parse(exported)?;
        let mut merged_ids = BTreeSet::new();
        for v in base["Data_KYJG"].as_array().unwrap() {
            merged_ids.insert(v.as_i64().unwrap());
        }
        for v in inc["Data_KYJG"].as_array().unwrap() {
            merged_ids.insert(v.as_i64().unwrap());
        }
        let mut merged_times: HashMap<String, String> = HashMap::new();
        for (k, v) in base["Time_KYJG"].as_object().unwrap() {
            merged_times.insert(k.clone(), v.as_str().unwrap_or("").to_owned());
        }
        for (k, v) in inc["Time_KYJG"].as_object().unwrap() {
            if merged_times.get(k).is_none_or(|s| s.is_empty()) {
                merged_times.insert(k.clone(), v.as_str().unwrap_or("").to_owned());
            }
        }
        let out_ids: Vec<i64> = merged_ids.into_iter().collect();
        let mut out_times = Map::new();
        for id in &out_ids {
            let key = id.to_string();
            if let Some(ts) = merged_times.get(&key) {
                out_times.insert(key, Value::String(ts.clone()));
            }
        }
        let out_ms = updated_at_ms.unwrap_or_else(|| {
            base["updated_at"]
                .as_i64()
                .unwrap_or(0)
                .max(inc["updated_at"].as_i64().unwrap_or(0))
        });
        Ok(json!({
            "Data_KYJG": out_ids,
            "Time_KYJG": out_times,
            "updated_at": out_ms,
        }))
    }
}

pub struct AppsampleAdapter;

impl AppsampleAdapter {
    pub fn parse_markers(
        markers_or_columnar: &Value,
        map_id: i64,
        zoom: i32,
        delta_encoded: bool,
        registry: Option<&CategoryRegistry>,
    ) -> CoreResult<Vec<TargetPin>> {
        let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
        let mut out = Vec::new();

        if let Some(obj) = markers_or_columnar.as_object() {
            let raw_ids = obj
                .get("id")
                .filter(|v| is_json_truthy(v))
                .or_else(|| obj.get("ids").filter(|v| is_json_truthy(v)))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let lngs = obj
                .get("lng")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let lats = obj
                .get("lat")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let types = obj
                .get("type")
                .filter(|v| is_json_truthy(v))
                .or_else(|| obj.get("cat").filter(|v| is_json_truthy(v)))
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mids = obj.get("mid").and_then(|v| v.as_array());

            let count = raw_ids.len().min(lngs.len()).min(lats.len());
            let mut acc_id: i64 = 0;
            for idx in 0..count {
                let raw_val = coerce_strict_int(&raw_ids[idx], "appsample.id")?;
                acc_id = if delta_encoded {
                    acc_id + raw_val
                } else {
                    raw_val
                };
                if let Some(mids_arr) = mids
                    && idx < mids_arr.len()
                    && coerce_strict_int(&mids_arr[idx], "appsample.mid")? != map_id
                {
                    continue;
                }
                let code = types.get(idx).and_then(value_as_opt_string);
                let lng = coerce_f64(&lngs[idx], "appsample.lng")?;
                let lat = coerce_f64(&lats[idx], "appsample.lat")?;
                let (px, py) = web_mercator_to_pixel(lng, lat, zoom);
                let canon = reg.resolve_target_category(None, code.as_deref(), None, None, None);
                let mut extra = BTreeMap::new();
                extra.insert("lng".to_owned(), json!(lng));
                extra.insert("lat".to_owned(), json!(lat));
                out.push(TargetPin {
                    target_id: TargetId::Int(acc_id),
                    x: px,
                    y: py,
                    canonical_category: canon,
                    category_name: None,
                    appsample_code: code,
                    feature_key: None,
                    map_id: Some(map_id),
                    floor_id: None,
                    point_group: None,
                    is_underground: None,
                    extra,
                });
            }
            return Ok(out);
        }

        let rows = markers_or_columnar.as_array().ok_or_else(|| {
            CoreError::InvalidData("Appsample markers must be a columnar object or array".into())
        })?;
        for item in rows {
            let Some(row) = item.as_object() else {
                continue;
            };
            let Some(id_val) = row.get("id") else {
                continue;
            };
            if let Some(mid_v) = row.get("mid").filter(|v| !v.is_null())
                && coerce_strict_int(mid_v, "appsample.mid")? != map_id
            {
                continue;
            }
            let mid_val = coerce_strict_int(id_val, "appsample.id")?;
            let (px, py) = if row.contains_key("x")
                && row.contains_key("y")
                && !row.contains_key("lng")
            {
                (
                    coerce_f64(&row["x"], "appsample.x")?,
                    coerce_f64(&row["y"], "appsample.y")?,
                )
            } else {
                let lng = row
                    .get("lng")
                    .ok_or_else(|| CoreError::InvalidData("Missing appsample lng".into()))?;
                let lat = row
                    .get("lat")
                    .ok_or_else(|| CoreError::InvalidData("Missing appsample lat".into()))?;
                web_mercator_to_pixel(
                    coerce_f64(lng, "appsample.lng")?,
                    coerce_f64(lat, "appsample.lat")?,
                    zoom,
                )
            };
            let code_str = row
                .get("appsample_code")
                .filter(|v| is_json_truthy(v))
                .or_else(|| row.get("type").filter(|v| is_json_truthy(v)))
                .or_else(|| row.get("o").filter(|v| is_json_truthy(v)))
                .and_then(value_as_opt_string);
            let category_name = row.get("category_name").and_then(value_as_opt_string);
            let canon = reg.resolve_target_category(
                row.get("canonical_category").and_then(|v| v.as_str()),
                code_str.as_deref(),
                category_name.as_deref(),
                None,
                None,
            );
            let row_map_id = row
                .get("mid")
                .and_then(|v| coerce_strict_int(v, "appsample.mid").ok())
                .unwrap_or(map_id);
            out.push(TargetPin {
                target_id: TargetId::Int(mid_val),
                x: px,
                y: py,
                canonical_category: canon,
                category_name,
                appsample_code: code_str,
                feature_key: None,
                map_id: Some(row_map_id),
                floor_id: row.get("floor_id").and_then(normalize_floor_id),
                point_group: None,
                is_underground: row.get("is_underground").and_then(|v| v.as_bool()),
                extra: BTreeMap::new(),
            });
        }
        Ok(out)
    }

    pub fn export(report: &TransferReport, include_ambiguous: bool) -> CoreResult<Value> {
        let selected = select_matches(&report.matched, include_ambiguous);
        let mut ids = BTreeSet::new();
        for m in selected {
            ids.insert(m.target.target_id.to_strict_int("appsample.target_id")?);
        }
        Ok(json!({ "ids": ids.into_iter().collect::<Vec<i64>>() }))
    }

    pub fn validate_schema(payload: &Value) -> Vec<String> {
        let Some(obj) = payload.as_object() else {
            return vec!["Payload must be a JSON object".to_owned()];
        };
        let Some(ids) = obj.get("ids").and_then(|v| v.as_array()) else {
            return vec!["'ids' must be a list[int]".to_owned()];
        };
        let mut errors = Vec::new();
        let mut parsed = Vec::with_capacity(ids.len());
        let mut all_ints = true;
        for v in ids {
            if let Some(i) = is_strict_json_int(v) {
                parsed.push(i);
            } else {
                all_ints = false;
            }
        }
        if !all_ints {
            errors.push("All elements of 'ids' must be strict integers".to_owned());
        } else if !parsed.windows(2).all(|w| w[0] < w[1]) {
            errors.push("'ids' must be sorted and deduplicated".to_owned());
        }
        errors
    }

    pub fn parse(raw: &Value) -> CoreResult<Value> {
        let data: Value = match raw {
            Value::String(s) => serde_json::from_str(s)?,
            other => other.clone(),
        };
        if !data.is_object() {
            return Err(CoreError::InvalidData(
                "Appsample payload must be a JSON object".into(),
            ));
        }
        let errors = Self::validate_schema(&data);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Appsample payload: {}",
                errors.join("; ")
            )));
        }
        let ids: BTreeSet<i64> = data["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| coerce_strict_int(v, "ids[]"))
            .collect::<CoreResult<BTreeSet<i64>>>()?;
        Ok(json!({ "ids": ids.into_iter().collect::<Vec<i64>>() }))
    }

    pub fn to_json(payload: &Value, pretty: bool) -> CoreResult<String> {
        let errors = Self::validate_schema(payload);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Appsample payload: {}",
                errors.join("; ")
            )));
        }
        if pretty {
            serde_json::to_string_pretty(payload).map_err(CoreError::Json)
        } else {
            serde_json::to_string(payload).map_err(CoreError::Json)
        }
    }

    pub fn merge_existing(existing: &Value, exported: &Value) -> CoreResult<Value> {
        let base = Self::parse(existing)?;
        let inc = Self::parse(exported)?;
        let mut ids = BTreeSet::new();
        for v in base["ids"].as_array().unwrap() {
            ids.insert(v.as_i64().unwrap());
        }
        for v in inc["ids"].as_array().unwrap() {
            ids.insert(v.as_i64().unwrap());
        }
        Ok(json!({ "ids": ids.into_iter().collect::<Vec<i64>>() }))
    }
}

pub struct GenshinMapGithubAdapter;

impl GenshinMapGithubAdapter {
    pub fn parse_markers(
        features: &Value,
        map_id: i64,
        registry: Option<&CategoryRegistry>,
    ) -> CoreResult<Vec<TargetPin>> {
        let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
        let arr = features.as_array().ok_or_else(|| {
            CoreError::InvalidData("genshinmap features must be a JSON array".into())
        })?;
        let mut out = Vec::new();
        for item in arr {
            let Some(feat) = item.as_object() else {
                continue;
            };
            let mut feature_key = feat
                .get("featureKey")
                .filter(|v| is_json_truthy(v))
                .or_else(|| feat.get("feature_key").filter(|v| is_json_truthy(v)))
                .and_then(value_as_opt_string)
                .unwrap_or_default();
            let raw_id = feat
                .get("id")
                .filter(|v| is_json_truthy(v))
                .or_else(|| feat.get("sha1_id").filter(|v| is_json_truthy(v)))
                .or_else(|| feat.get("target_id").filter(|v| is_json_truthy(v)));
            let Some(raw_id_str) = raw_id.and_then(value_as_opt_string) else {
                continue;
            };
            let full_id = if raw_id_str.contains('/') {
                raw_id_str
            } else if !feature_key.is_empty() {
                format!("{feature_key}/{raw_id_str}")
            } else {
                raw_id_str
            };
            if feature_key.is_empty()
                && let Some((left, _)) = full_id.split_once('/')
            {
                feature_key = left.to_owned();
            }

            let (x_val, y_val) = if let (Some(xv), Some(yv)) = (feat.get("x"), feat.get("y")) {
                (coerce_f64(xv, "x")?, coerce_f64(yv, "y")?)
            } else if let Some(coords) = feat.get("coordinates").and_then(|v| v.as_array())
                && coords.len() >= 2
            {
                (
                    coerce_f64(&coords[0], "coordinates[0]")?,
                    coerce_f64(&coords[1], "coordinates[1]")?,
                )
            } else if let (Some(lng), Some(lat)) = (feat.get("lng"), feat.get("lat")) {
                (coerce_f64(lng, "lng")?, coerce_f64(lat, "lat")?)
            } else {
                continue;
            };

            let category_name = feat.get("category_name").and_then(value_as_opt_string);
            let canon = reg.resolve_target_category(
                feat.get("canonical_category").and_then(|v| v.as_str()),
                None,
                category_name.as_deref(),
                if feature_key.is_empty() {
                    None
                } else {
                    Some(feature_key.as_str())
                },
                None,
            );
            let feat_map_id = feat
                .get("map_id")
                .and_then(|v| coerce_strict_int(v, "map_id").ok())
                .unwrap_or(map_id);
            out.push(TargetPin {
                target_id: TargetId::Str(full_id),
                x: x_val,
                y: y_val,
                canonical_category: canon,
                category_name,
                appsample_code: None,
                feature_key: if feature_key.is_empty() {
                    None
                } else {
                    Some(feature_key)
                },
                map_id: Some(feat_map_id),
                floor_id: feat.get("floor_id").and_then(normalize_floor_id),
                point_group: None,
                is_underground: feat.get("is_underground").and_then(|v| v.as_bool()),
                extra: BTreeMap::new(),
            });
        }
        Ok(out)
    }

    fn default_preferences() -> Value {
        json!({
            "version": GM007_VERSION,
            "options": {},
            "displayed": {"features": {}, "routes": {}},
            "completed": {"features": {}},
            "editor": {"feature": {"data": []}, "route": {"data": []}},
        })
    }

    fn resolve_feature_compound_key(target: &TargetPin) -> CoreResult<String> {
        let tid = target.target_id.to_string();
        let tid_trim = tid.trim();
        if let Some((left, right)) = tid_trim.split_once('/')
            && !left.trim().is_empty()
            && !right.trim().is_empty()
        {
            return Ok(format!("{}/{}", left.trim(), right.trim()));
        }
        if let Some(ref fk) = target.feature_key
            && !fk.trim().is_empty()
            && !tid_trim.is_empty()
        {
            return Ok(format!("{}/{}", fk.trim(), tid_trim));
        }
        Err(CoreError::InvalidData(format!(
            "genshinmap target_id must be '<featureKey>/<sha1_id>' or include feature_key, got {tid_trim:?}"
        )))
    }

    pub fn build_payload(
        report: &TransferReport,
        include_ambiguous: bool,
        default_unix_sec: Option<i64>,
        base_preferences: Option<&Value>,
    ) -> CoreResult<Value> {
        let selected = select_matches(&report.matched, include_ambiguous);
        let fallback_sec = default_unix_sec.unwrap_or_else(now_unix_sec);

        let mut payload = match base_preferences {
            None | Some(Value::Null) => Self::default_preferences(),
            Some(base) if base.is_object() => base.clone(),
            Some(_) => {
                return Err(CoreError::InvalidData(
                    "base_preferences must be a JSON object".into(),
                ));
            }
        };
        let obj = payload.as_object_mut().unwrap();
        obj.insert(
            "version".to_owned(),
            Value::String(GM007_VERSION.to_owned()),
        );
        if !obj.contains_key("options") {
            obj.insert("options".to_owned(), json!({}));
        }
        if !obj.contains_key("displayed") {
            obj.insert("displayed".to_owned(), json!({"features": {}, "routes": {}}));
        }
        match obj.get("completed") {
            None => {
                obj.insert("completed".to_owned(), json!({"features": {}}));
            }
            Some(v) if v.is_object() => {}
            Some(_) => {
                return Err(CoreError::InvalidData(
                    "'completed' must be an object".into(),
                ));
            }
        }
        if !obj.contains_key("editor") {
            obj.insert(
                "editor".to_owned(),
                json!({"feature": {"data": []}, "route": {"data": []}}),
            );
        }
        let completed_obj = obj.get_mut("completed").unwrap().as_object_mut().unwrap();
        match completed_obj.get("features") {
            None => {
                completed_obj.insert("features".to_owned(), json!({}));
            }
            Some(v) if v.is_object() => {}
            Some(_) => {
                return Err(CoreError::InvalidData(
                    "'completed.features' must be a flat dict, not a list".into(),
                ));
            }
        }

        let mut features_map: BTreeMap<String, Value> = BTreeMap::new();
        for (k, v) in completed_obj["features"].as_object().unwrap() {
            features_map.insert(k.clone(), v.clone());
        }
        for m in selected {
            let compound = Self::resolve_feature_compound_key(&m.target)?;
            features_map.entry(compound).or_insert_with(|| {
                let sec = m
                    .source
                    .marked_at
                    .as_deref()
                    .and_then(|s| parse_timestamp_to_unix_sec(&Value::String(s.to_owned())))
                    .unwrap_or(fallback_sec);
                json!(sec)
            });
        }

        let mut sorted_features = Map::new();
        for (k, v) in features_map {
            sorted_features.insert(k, v);
        }
        completed_obj.insert("features".to_owned(), Value::Object(sorted_features));
        Ok(payload)
    }

    pub fn validate_schema(payload: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(obj) = payload.as_object() else {
            return vec!["Payload must be a JSON object".to_owned()];
        };
        for req_key in GM007_REQUIRED_KEYS {
            if !obj.contains_key(req_key) {
                errors.push(format!("Missing required key {req_key:?}"));
            }
        }
        if obj.get("version").and_then(|v| v.as_str()) != Some(GM007_VERSION) {
            errors.push(format!(
                "version must be {GM007_VERSION:?}, got {:?}",
                obj.get("version")
            ));
        }
        match obj.get("completed").and_then(|v| v.as_object()) {
            None => errors.push("'completed' must be an object".to_owned()),
            Some(completed) => match completed.get("features").and_then(|v| v.as_object()) {
                None => {
                    errors.push("'completed.features' must be a flat dict, not a list".to_owned());
                }
                Some(feats) => {
                    for (k, v) in feats {
                        if let Some((left, right)) = k.split_once('/') {
                            if left.trim().is_empty() || right.trim().is_empty() {
                                errors.push(format!(
                                    "completed.features key {k:?} has empty featureKey or id"
                                ));
                            }
                        } else {
                            errors.push(format!(
                                "completed.features key {k:?} must be '<featureKey>/<sha1_id>'"
                            ));
                        }
                        match is_strict_json_int(v) {
                            Some(ts) if ts >= 0 => {}
                            _ => errors.push(format!(
                                "completed.features[{k:?}] must be non-negative int unix_sec, got {v}"
                            )),
                        }
                    }
                }
            },
        }
        errors
    }

    pub fn encode(payload: &Value) -> CoreResult<String> {
        let errors = Self::validate_schema(payload);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid GM_007 payload: {}",
                errors.join("; ")
            )));
        }
        let raw_json = serde_json::to_string(payload)?;
        let framed = format!("{GM007_PREFIX}{raw_json}");
        Ok(base64_encode(framed.as_bytes()))
    }

    pub fn export(
        report: &TransferReport,
        include_ambiguous: bool,
        default_unix_sec: Option<i64>,
        base_preferences: Option<&Value>,
    ) -> CoreResult<String> {
        let payload = Self::build_payload(
            report,
            include_ambiguous,
            default_unix_sec,
            base_preferences,
        )?;
        Self::encode(&payload)
    }

    pub fn decode(encoded: &str) -> CoreResult<Value> {
        let trimmed = encoded.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidData(
                "Encoded GM_007 string must be a non-empty string".into(),
            ));
        }
        let decoded_bytes = base64_decode_strict(trimmed)?;
        let framed = String::from_utf8(decoded_bytes).map_err(|e| {
            CoreError::InvalidData(format!("Invalid UTF-8 Base64 GM_007 payload: {e}"))
        })?;
        let raw_json = framed.strip_prefix(GM007_PREFIX).ok_or_else(|| {
            CoreError::InvalidData(format!("Missing required {GM007_PREFIX:?} prefix"))
        })?;
        let payload: Value = serde_json::from_str(raw_json)?;
        let errors = Self::validate_schema(&payload);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid decoded GM_007 schema: {}",
                errors.join("; ")
            )));
        }
        Ok(payload)
    }

    pub fn merge_into_encoded(
        existing_encoded: &str,
        report: &TransferReport,
        include_ambiguous: bool,
        default_unix_sec: Option<i64>,
    ) -> CoreResult<String> {
        let existing_payload = Self::decode(existing_encoded)?;
        Self::export(
            report,
            include_ambiguous,
            default_unix_sec,
            Some(&existing_payload),
        )
    }
}

pub struct CanonicalJsonAdapter;

impl CanonicalJsonAdapter {
    pub fn export_pins(
        pins: &[CanonicalPin],
        map_id: Option<i64>,
        exported_at: Option<&Value>,
        missing_from_point_list: &[i64],
        unmapped_label_ids_override: Option<&[i64]>,
    ) -> Value {
        let eff_map_id = map_id.unwrap_or_else(|| pins.first().map(|p| p.map_id).unwrap_or(2));
        let unmapped_labels: Vec<i64> = match unmapped_label_ids_override {
            Some(ids) => ids.to_vec(),
            None => pins
                .iter()
                .filter(|p| p.canonical_category.starts_with("unmapped_"))
                .map(|p| p.label_id)
                .collect::<BTreeSet<i64>>()
                .into_iter()
                .collect(),
        };
        let ts_iso = exported_at
            .and_then(normalize_timestamp_iso)
            .unwrap_or_else(|| {
                let (y, m, d, hh, mm, ss) = format_utc_parts(now_unix_sec());
                format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
            });
        let mut sorted_pins = pins.to_vec();
        sorted_pins.sort_by_key(|p| (p.map_id, p.hoyolab_id));

        json!({
            "schema": CANONICAL_SCHEMA_V1,
            "exported_at": ts_iso,
            "map_id": eff_map_id,
            "stats": {
                "total_pins": sorted_pins.len(),
                "missing_from_point_list_count": missing_from_point_list.len(),
                "unmapped_label_ids": unmapped_labels,
            },
            "missing_from_point_list": missing_from_point_list,
            "pins": sorted_pins.iter().map(CanonicalPin::to_value).collect::<Vec<Value>>(),
        })
    }

    pub fn export_parse_result(
        result: &HoyolabParseResult,
        map_id: Option<i64>,
        exported_at: Option<&Value>,
    ) -> Value {
        Self::export_pins(
            &result.pins,
            Some(map_id.unwrap_or(result.map_id)),
            exported_at,
            &result.missing_from_point_list,
            Some(&result.unmapped_label_ids),
        )
    }

    pub fn validate_schema(payload: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(obj) = payload.as_object() else {
            return vec!["Payload must be a JSON object".to_owned()];
        };
        if obj.get("schema").and_then(|v| v.as_str()) != Some(CANONICAL_SCHEMA_V1) {
            errors.push(format!(
                "schema must be {CANONICAL_SCHEMA_V1:?}, got {:?}",
                obj.get("schema")
            ));
        }
        if !obj.get("pins").is_some_and(|v| v.is_array()) {
            errors.push("'pins' must be a list".to_owned());
        }
        if obj.get("map_id").and_then(is_strict_json_int).is_none() {
            errors.push("'map_id' must be an integer".to_owned());
        }
        errors
    }

    pub fn to_json(payload: &Value, pretty: bool) -> CoreResult<String> {
        let errors = Self::validate_schema(payload);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Canonical JSON payload: {}",
                errors.join("; ")
            )));
        }
        if pretty {
            serde_json::to_string_pretty(payload).map_err(CoreError::Json)
        } else {
            serde_json::to_string(payload).map_err(CoreError::Json)
        }
    }

    pub fn parse(raw: &Value) -> CoreResult<(Vec<CanonicalPin>, Value)> {
        let data: Value = match raw {
            Value::String(s) => serde_json::from_str(s)?,
            other => other.clone(),
        };
        let errors = Self::validate_schema(&data);
        if !errors.is_empty() {
            return Err(CoreError::InvalidData(format!(
                "Invalid Canonical JSON: {}",
                errors.join("; ")
            )));
        }
        let pins = data["pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(CanonicalPin::from_value)
            .collect::<CoreResult<Vec<CanonicalPin>>>()?;
        let missing: Vec<i64> = data
            .get("missing_from_point_list")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|x| coerce_strict_int(x, "missing_from_point_list[]"))
                    .collect::<CoreResult<Vec<i64>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let meta = json!({
            "schema": data["schema"],
            "exported_at": data.get("exported_at").cloned().unwrap_or(Value::Null),
            "map_id": is_strict_json_int(&data["map_id"]).unwrap(),
            "missing_from_point_list": missing,
            "stats": data.get("stats").cloned().unwrap_or_else(|| json!({})),
        });
        Ok((pins, meta))
    }
}

// ---------------------------------------------------------------------------
// Unified `/cross_map_export` Entrypoint
// ---------------------------------------------------------------------------

const TARGET_PIN_FIELDS: [&str; 12] = [
    "target_id",
    "x",
    "y",
    "canonical_category",
    "category_name",
    "appsample_code",
    "feature_key",
    "map_id",
    "floor_id",
    "point_group",
    "is_underground",
    "extra",
];

fn has_non_empty_str_field(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

fn try_parse_pre_normalized_target_array(
    arr: &[Value],
    format: ExportFormat,
) -> Option<Vec<TargetPin>> {
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = item.as_object()?;
        if !obj.contains_key("target_id") || !obj.contains_key("x") || !obj.contains_key("y") {
            return None;
        }
        if !obj
            .keys()
            .all(|k| TARGET_PIN_FIELDS.contains(&k.as_str()))
        {
            return None;
        }
        if format == ExportFormat::Genshinmap {
            let has_target_pin_marker = has_non_empty_str_field(obj, "canonical_category")
                || has_non_empty_str_field(obj, "appsample_code")
                || obj.get("point_group").is_some_and(|v| !v.is_null())
                || obj
                    .get("extra")
                    .and_then(|v| v.as_object())
                    .is_some_and(|m| !m.is_empty());
            if !has_target_pin_marker {
                return None;
            }
        }
        let pin = serde_json::from_value::<TargetPin>(item.clone()).ok()?;
        if !pin.x.is_finite() || !pin.y.is_finite() {
            return None;
        }
        out.push(pin);
    }
    Some(out)
}

pub fn execute_cross_map_export(
    req: &Value,
    affine_registry: Option<&PerMapAffineRegistry>,
    registry: Option<&CategoryRegistry>,
) -> CoreResult<Value> {
    let reg = registry.unwrap_or_else(|| CategoryRegistry::embedded());
    let obj = req
        .as_object()
        .ok_or_else(|| CoreError::InvalidArgument("Request body must be a JSON object".into()))?;

    let format_raw = obj
        .get("target_format")
        .filter(|v| is_json_truthy(v))
        .or_else(|| obj.get("format").filter(|v| is_json_truthy(v)))
        .and_then(|v| v.as_str())
        .unwrap_or("canonical");
    let format: ExportFormat = format_raw.parse()?;
    let format_str = format.as_str();

    let map_id = obj
        .get("map_id")
        .filter(|v| !v.is_null())
        .and_then(|v| coerce_strict_int(v, "map_id").ok())
        .unwrap_or(2);

    let point_list = obj.get("point_list").filter(|v| !v.is_null());
    let marked_list = obj.get("marked_list").filter(|v| !v.is_null());
    let raw_pins = obj.get("pins").and_then(|v| v.as_array());

    let mut parsed_export: Option<HoyolabParseResult> = None;
    let mut missing_ids: Vec<i64> = Vec::new();
    let source_pins: Vec<CanonicalPin> = match (point_list, marked_list) {
        (Some(pl), Some(ml)) => {
            let res = parse_hoyolab_export(pl, ml, map_id, Some(reg))?;
            missing_ids = res.missing_from_point_list.clone();
            let pins = res.pins.clone();
            parsed_export = Some(res);
            pins
        }
        (Some(pl), None) => parse_hoyolab_points(pl, map_id, None, Some(reg))?,
        (None, _) => {
            if let Some(arr) = raw_pins {
                arr.iter()
                    .filter(|v| v.is_object())
                    .map(CanonicalPin::from_value)
                    .collect::<CoreResult<Vec<CanonicalPin>>>()?
            } else {
                Vec::new()
            }
        }
    };

    if format == ExportFormat::Canonical {
        let payload = match &parsed_export {
            Some(res) => CanonicalJsonAdapter::export_parse_result(res, Some(map_id), None),
            None => CanonicalJsonAdapter::export_pins(&source_pins, Some(map_id), None, &[], None),
        };
        let unmapped_cats: Vec<String> = source_pins
            .iter()
            .filter(|p| p.canonical_category.starts_with("unmapped_"))
            .map(|p| p.canonical_category.clone())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        let total_considered = source_pins.len() + missing_ids.len();
        let rate = if total_considered > 0 {
            ((100.0 * (source_pins.len() as f64) / (total_considered as f64)) * 100.0).round()
                / 100.0
        } else {
            0.0
        };
        let report = TransferSummary {
            total_source_pins: source_pins.len(),
            matched_count: source_pins.len(),
            confident_count: source_pins.len(),
            ambiguous_count: 0,
            unmatched_count: 0,
            unmapped_categories: unmapped_cats,
            missing_from_point_list_count: missing_ids.len(),
            match_rate_pct: rate,
        };
        return Ok(json!({
            "ok": true,
            "format": format_str,
            "payload": payload,
            "report": report,
        }));
    }

    let empty_targets = Value::Array(Vec::new());
    let raw_targets = obj
        .get("target_pins")
        .filter(|v| !v.is_null())
        .or_else(|| obj.get("target_markers").filter(|v| is_json_truthy(v)))
        .unwrap_or(&empty_targets);

    let parsed_targets: Vec<TargetPin> = if let Some(arr) = raw_targets.as_array()
        && let Some(pins) = try_parse_pre_normalized_target_array(arr, format)
    {
        pins
    } else {
        match format {
            ExportFormat::Kongying => KongyingAdapter::parse_markers(raw_targets, map_id, Some(reg))?,
            ExportFormat::Appsample => {
                AppsampleAdapter::parse_markers(raw_targets, map_id, 13, true, Some(reg))?
            }
            ExportFormat::Genshinmap => {
                GenshinMapGithubAdapter::parse_markers(raw_targets, map_id, Some(reg))?
            }
            ExportFormat::Canonical => unreachable!(),
        }
    };

    let affine: Option<AxisAffine> = if let Some(aff_val) = obj.get("affine").filter(|v| v.is_object()) {
        let mut aff = AxisAffine::from_value(aff_val)?;
        if aff.map_id.is_none() {
            aff.map_id = Some(map_id);
        }
        Some(aff)
    } else if let (Some(src_a), Some(dst_a)) = (
        obj.get("src_anchors").and_then(|v| v.as_array()),
        obj.get("dst_anchors").and_then(|v| v.as_array()),
    ) {
        let parse_pts = |arr: &[Value]| -> CoreResult<Vec<(f64, f64)>> {
            arr.iter()
                .map(|v| {
                    let pair = v.as_array().ok_or_else(|| {
                        CoreError::InvalidArgument("Anchor point must be [x, y]".into())
                    })?;
                    if pair.len() < 2 {
                        return Err(CoreError::InvalidArgument(
                            "Anchor point must have 2 coordinates".into(),
                        ));
                    }
                    Ok((
                        coerce_f64(&pair[0], "anchor.x")?,
                        coerce_f64(&pair[1], "anchor.y")?,
                    ))
                })
                .collect()
        };
        let src_pts = parse_pts(src_a)?;
        let dst_pts = parse_pts(dst_a)?;
        Some(fit_axis_affine(
            &src_pts,
            &dst_pts,
            Some(map_id),
            true,
            3.5,
            "fitted",
        )?)
    } else {
        let from_reg = match affine_registry {
            Some(ar) => ar.get(format_str, map_id, true),
            None => PerMapAffineRegistry::with_registry(reg.clone()).get(format_str, map_id, true),
        };
        if from_reg.is_some() {
            from_reg
        } else if obj
            .get("use_identity_affine")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Some(AxisAffine {
                ax: 1.0,
                bx: 0.0,
                cy: 1.0,
                dy: 0.0,
                map_id: Some(map_id),
                rmse: 0.0,
                max_residual: 0.0,
                anchor_count: 0,
                source: "identity".to_owned(),
            })
        } else {
            None
        }
    };

    let Some(affine) = affine else {
        // Appsample v5 has calibrated multi-affine regional seeds (Teyvat Mainland/Inazuma/Nod-Krai
        // + all 6 permanent sub-maps), so export succeeds cleanly even without an explicit affine.
        if format == ExportFormat::Appsample {
            return crate::cross_map_import::export_hoyolab_to_appsample_v5(
                &source_pins,
                if raw_targets.is_null() {
                    None
                } else {
                    Some(raw_targets)
                },
                map_id,
            );
        }
        return Ok(json!({
            "ok": false,
            "format": format_str,
            "payload": Value::Null,
            "report": Value::Null,
            "error": format!("No calibrated affine transform available for target={format_str:?}, map_id={map_id}"),
        }));
    };

    let max_dist = obj
        .get("max_dist")
        .and_then(|v| coerce_f64(v, "max_dist").ok())
        .unwrap_or(15.0);
    let include_ambiguous = obj
        .get("include_ambiguous")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let transfer = match_pins_one_to_one(
        &source_pins,
        &parsed_targets,
        &affine,
        MatchOptions {
            max_dist,
            ..Default::default()
        },
        Some(reg),
        Some(&missing_ids),
    )?;

    let existing_payload = obj.get("existing_payload").filter(|v| !v.is_null());
    let has_non_empty_existing = existing_payload.is_some_and(|v| match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(m) => !m.is_empty(),
        _ => false,
    });

    let exported_out: Value = match format {
        ExportFormat::Kongying => {
            let exp = KongyingAdapter::export(&transfer, include_ambiguous, None, None)?;
            if has_non_empty_existing {
                KongyingAdapter::merge_existing(existing_payload.unwrap(), &exp, None)?
            } else {
                exp
            }
        }
        ExportFormat::Appsample => {
            let exp = AppsampleAdapter::export(&transfer, include_ambiguous)?;
            if has_non_empty_existing {
                AppsampleAdapter::merge_existing(existing_payload.unwrap(), &exp)?
            } else {
                exp
            }
        }
        ExportFormat::Genshinmap => {
            if let Some(Value::String(s)) = existing_payload
                && !s.trim().is_empty()
            {
                Value::String(GenshinMapGithubAdapter::merge_into_encoded(
                    s,
                    &transfer,
                    include_ambiguous,
                    None,
                )?)
            } else {
                Value::String(GenshinMapGithubAdapter::export(
                    &transfer,
                    include_ambiguous,
                    None,
                    None,
                )?)
            }
        }
        ExportFormat::Canonical => unreachable!(),
    };

    Ok(json!({
        "ok": true,
        "format": format_str,
        "payload": exported_out,
        "report": transfer.summary(),
    }))
}

// ---------------------------------------------------------------------------
// Live Multi-Provider Piecewise Coordinate Transform & Auto-Calibration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpatialBbox {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl SpatialBbox {
    pub fn from_points(pts: &[(f64, f64)]) -> Option<Self> {
        let first = *pts.first()?;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.0, first.0, first.1, first.1);
        for &(x, y) in pts.iter().skip(1) {
            if x < min_x {
                min_x = x;
            }
            if x > max_x {
                max_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
        Some(Self {
            min_x,
            max_x,
            min_y,
            max_y,
        })
    }

    pub fn contains(&self, x: f64, y: f64, pad_x: f64, pad_y: f64) -> bool {
        x >= self.min_x - pad_x
            && x <= self.max_x + pad_x
            && y >= self.min_y - pad_y
            && y <= self.max_y + pad_y
    }

    pub fn distance_sq(&self, x: f64, y: f64) -> f64 {
        let dx = if x < self.min_x {
            self.min_x - x
        } else if x > self.max_x {
            x - self.max_x
        } else {
            0.0
        };
        let dy = if y < self.min_y {
            self.min_y - y
        } else if y > self.max_y {
            y - self.max_y
        } else {
            0.0
        };
        dx * dx + dy * dy
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneAffine {
    pub id: String,
    pub canonical_bbox: SpatialBbox,
    pub provider_bbox: SpatialBbox,
    pub affine: AxisAffine,
}

/// Bidirectional transform between Canonical Internal CRS (HoYoLAB World Coordinates)
/// and an external map provider's coordinate space, supporting per-region piecewise
/// shifts (e.g. AppSample shifting Inazuma and Easybreeze Resort closer to the mainland)
/// and swapped Leaflet axes (e.g. Kongying v3 mapping `lat = +X East, lng = +Y South`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiecewiseMapTransform {
    pub provider: String,
    pub map_id: i64,
    #[serde(default)]
    pub swap_lat_lng: bool,
    pub base: AxisAffine,
    #[serde(default)]
    pub zones: Vec<ZoneAffine>,
    #[serde(default)]
    pub matched_anchors: usize,
    #[serde(default)]
    pub total_canonical_anchors: usize,
    #[serde(default)]
    pub rmse_canonical: f64,
    #[serde(default)]
    pub source: String,
}

impl PiecewiseMapTransform {
    /// Projects Canonical `(cx, cy)` (`lng = cx`, `lat = cy`) -> Provider `(px, py)`.
    pub fn forward(&self, cx: f64, cy: f64) -> (f64, f64) {
        let pad = 350.0;
        let mut best_zone: Option<(&ZoneAffine, f64)> = None;
        for z in &self.zones {
            if z.canonical_bbox.contains(cx, cy, pad, pad) {
                let d2 = z.canonical_bbox.distance_sq(cx, cy);
                if best_zone.is_none_or(|(_, bd)| d2 < bd) {
                    best_zone = Some((z, d2));
                }
            }
        }
        if let Some((zone, _)) = best_zone {
            return zone.affine.transform(cx, cy);
        }
        self.base.transform(cx, cy)
    }

    /// Projects Provider `(px, py)` -> Canonical `(cx, cy)`.
    pub fn inverse(&self, px: f64, py: f64) -> CoreResult<(f64, f64)> {
        let pad_x = 350.0 * self.base.ax.abs();
        let pad_y = 350.0 * self.base.cy.abs();
        let mut best_zone: Option<(&ZoneAffine, f64)> = None;
        for z in &self.zones {
            if z.provider_bbox.contains(px, py, pad_x, pad_y) {
                let d2 = z.provider_bbox.distance_sq(px, py);
                if best_zone.is_none_or(|(_, bd)| d2 < bd) {
                    best_zone = Some((z, d2));
                }
            }
        }
        if let Some((zone, _)) = best_zone {
            return Ok(zone.affine.inverse()?.transform(px, py));
        }
        Ok(self.base.inverse()?.transform(px, py))
    }

    /// Converts Canonical `(cx, cy)` into Leaflet `(lat, lng)` respecting `swap_lat_lng`.
    pub fn to_leaflet_latlng(&self, cx: f64, cy: f64) -> (f64, f64) {
        let (px, py) = self.forward(cx, cy);
        if self.swap_lat_lng {
            (px, py)
        } else {
            (py, px)
        }
    }

    /// Converts Leaflet `(lat, lng)` into Canonical `(cx, cy)` respecting `swap_lat_lng`.
    pub fn from_leaflet_latlng(&self, lat: f64, lng: f64) -> CoreResult<(f64, f64)> {
        let (px, py) = if self.swap_lat_lng {
            (lat, lng)
        } else {
            (lng, lat)
        };
        self.inverse(px, py)
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalAnchor {
    pub x: f64,
    pub y: f64,
    pub label_id: i32,
    pub area_id: i32,
}

#[derive(Debug, Clone)]
pub struct ProviderAnchor {
    pub x: f64,
    pub y: f64,
    pub is_statue: bool,
}

fn make_seed_affine(
    ax: f64,
    bx: f64,
    cy: f64,
    dy: f64,
    map_id: i64,
    source: &str,
) -> AxisAffine {
    AxisAffine {
        ax,
        bx,
        cy,
        dy,
        map_id: Some(map_id),
        rmse: 0.0,
        max_residual: 0.0,
        anchor_count: 0,
        source: source.to_owned(),
    }
}

/// Returns empirically verified default [`PiecewiseMapTransform`] seeds for live
/// multi-provider sync (used as zero-latency initial fallback before runtime
/// auto-calibration runs against the page's live waypoints).
pub fn provider_default_transform(provider: &str, map_id: i64) -> Option<PiecewiseMapTransform> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "hoyolab" | "miyoushe" => Some(PiecewiseMapTransform {
            provider: provider.trim().to_ascii_lowercase(),
            map_id,
            swap_lat_lng: false,
            base: make_seed_affine(1.0, 0.0, 1.0, 0.0, map_id, "identity"),
            zones: Vec::new(),
            matched_anchors: 0,
            total_canonical_anchors: 0,
            rmse_canonical: 0.0,
            source: "identity".to_owned(),
        }),
        "kongying" => match map_id {
            2 => Some(PiecewiseMapTransform {
                provider: "kongying".to_owned(),
                map_id: 2,
                swap_lat_lng: true,
                base: make_seed_affine(
                    1.335301544,
                    1527.748294,
                    1.335165651,
                    -3931.249166,
                    2,
                    "seed_kongying_twt702",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_kongying_twt702".to_owned(),
            }),
            _ => None,
        },
        "appsample" => match map_id {
            2 => {
                let sx = 8.58624949353e-5;
                let sy = -8.58625216451e-5;
                let base = make_seed_affine(
                    sx,
                    0.522689857989,
                    sy,
                    0.199458672905,
                    2,
                    "seed_appsample_v72_m2",
                );
                let inazuma = ZoneAffine {
                    id: "area_3".to_owned(),
                    canonical_bbox: SpatialBbox {
                        min_x: -169.0,
                        max_x: 3840.0,
                        min_y: 4087.0,
                        max_y: 9019.2,
                    },
                    provider_bbox: SpatialBbox {
                        min_x: sx * -169.0 + 0.3334857058,
                        max_x: sx * 3840.0 + 0.3334857058,
                        min_y: sy * 9019.2 + 0.1994622811,
                        max_y: sy * 4087.0 + 0.1994622811,
                    },
                    affine: make_seed_affine(
                        sx,
                        0.3334857058,
                        sy,
                        0.1994622811,
                        2,
                        "seed_appsample_v72_inazuma",
                    ),
                };
                let easybreeze = ZoneAffine {
                    id: "area_11_west".to_owned(),
                    canonical_bbox: SpatialBbox {
                        min_x: -15949.9,
                        max_x: -14339.4,
                        min_y: 695.5,
                        max_y: 2662.2,
                    },
                    provider_bbox: SpatialBbox {
                        min_x: sx * -15949.9 + 0.6943898695,
                        max_x: sx * -14339.4 + 0.6943898695,
                        min_y: sy * 2662.2 + 0.1994580854,
                        max_y: sy * 695.5 + 0.1994580854,
                    },
                    affine: make_seed_affine(
                        sx,
                        0.6943898695,
                        sy,
                        0.1994580854,
                        2,
                        "seed_appsample_v72_easybreeze",
                    ),
                };
                Some(PiecewiseMapTransform {
                    provider: "appsample".to_owned(),
                    map_id: 2,
                    swap_lat_lng: false,
                    base,
                    zones: vec![inazuma, easybreeze],
                    matched_anchors: 0,
                    total_canonical_anchors: 0,
                    rmse_canonical: 0.0,
                    source: "seed_appsample_v72".to_owned(),
                })
            }
            7 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 7,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0003425009681,
                    -0.06889919302,
                    -0.0003424994737,
                    0.09208809008,
                    7,
                    "seed_appsample_v72_m7",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            9 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 9,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0006849998113,
                    0.1389419667,
                    -0.0006849998961,
                    -0.007533606738,
                    9,
                    "seed_appsample_v72_m9",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            34 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 34,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0003433756412,
                    0.03262133342,
                    -0.000343387891,
                    -0.03090441433,
                    34,
                    "seed_appsample_v72_m34",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            36 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 36,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0005150052408,
                    0.0,
                    -0.0005150024515,
                    0.0,
                    36,
                    "seed_appsample_v72_m36",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            37 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 37,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0003430005951,
                    -0.00411639969,
                    -0.0003429997946,
                    0.04287577188,
                    37,
                    "seed_appsample_v72_m37",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            40 => Some(PiecewiseMapTransform {
                provider: "appsample".to_owned(),
                map_id: 40,
                swap_lat_lng: false,
                base: make_seed_affine(
                    0.0001495201733,
                    -0.3464388249,
                    -0.0001495198016,
                    0.0001504959339,
                    40,
                    "seed_appsample_v72_m40",
                ),
                zones: Vec::new(),
                matched_anchors: 0,
                total_canonical_anchors: 0,
                rmse_canonical: 0.0,
                source: "seed_appsample_v72".to_owned(),
            }),
            _ => None,
        },
        _ => None,
    }
}

/// Zero-seed robust multi-cluster RANSAC + per-area piecewise zone discovery between
/// canonical HoYoLAB landmarks (`multimap_v4.json`) and live provider waypoints.
pub fn calibrate_piecewise_from_anchors(
    provider: &str,
    map_id: i64,
    canonical_anchors: &[CanonicalAnchor],
    provider_anchors: &[ProviderAnchor],
    expected_y_flip: Option<bool>,
    swap_lat_lng: bool,
    seed_override: Option<&PiecewiseMapTransform>,
) -> CoreResult<PiecewiseMapTransform> {
    let prov = provider.trim().to_ascii_lowercase();
    if canonical_anchors.len() < 3 || provider_anchors.len() < 3 {
        if let Some(seed) = seed_override
            .cloned()
            .or_else(|| provider_default_transform(&prov, map_id))
        {
            return Ok(seed);
        }
        return Err(CoreError::InvalidArgument(format!(
            "Need at least 3 canonical ({}) and provider ({}) anchors for {prov} map {map_id}",
            canonical_anchors.len(),
            provider_anchors.len()
        )));
    }

    // 1. Evaluate seed hypothesis first if available (handles single-area partial loads cleanly).
    let mut best_hyp: Option<(usize, f64, f64, f64, f64)> = None;
    let eval_hyp = |sx: f64, bx: f64, sy: f64, dy: f64, thresh: f64| -> usize {
        if !sx.is_finite() || !sy.is_finite() || sx.abs() < 1e-10 || sy.abs() < 1e-10 {
            return 0;
        }
        let inv_sx = 1.0 / sx.abs();
        let inv_sy = 1.0 / sy.abs();
        let mut count = 0usize;
        for p in provider_anchors {
            let mut min_err = f64::INFINITY;
            for c in canonical_anchors {
                let ex = (p.x - (sx * c.x + bx)) * inv_sx;
                let ey = (p.y - (sy * c.y + dy)) * inv_sy;
                let err = ex.hypot(ey);
                if err < min_err {
                    min_err = err;
                }
            }
            if min_err < thresh {
                count += 1;
            }
        }
        count
    };

    if let Some(seed) = seed_override
        .cloned()
        .or_else(|| provider_default_transform(&prov, map_id))
    {
        let inl = eval_hyp(seed.base.ax, seed.base.bx, seed.base.cy, seed.base.dy, 60.0);
        if inl >= 4 {
            best_hyp = Some((inl, seed.base.ax, seed.base.bx, seed.base.cy, seed.base.dy));
        }
    }

    // 2. Pair-voting RANSAC on Statues (label_id == 2) first, falling back to full anchors
    //    when fewer than 4 Statues exist on the map (e.g. Chasm / Enkanomiya / partial area).
    let need_ransac = best_hyp.is_none_or(|(inl, ..)| inl * 2 < provider_anchors.len().min(canonical_anchors.len()));
    if need_ransac {
        let can_statues: Vec<&CanonicalAnchor> = canonical_anchors
            .iter()
            .filter(|a| a.label_id == 2)
            .collect();
        let prv_statues: Vec<&ProviderAnchor> = provider_anchors
            .iter()
            .filter(|a| a.is_statue)
            .collect();

        type Point2Pair = (Vec<(f64, f64)>, Vec<(f64, f64)>);
        let (src_sub, dst_sub): Point2Pair =
            if can_statues.len() >= 4 && prv_statues.len() >= 4 && can_statues.len() <= 80 && prv_statues.len() <= 80 {
                (
                    can_statues.iter().map(|a| (a.x, a.y)).collect(),
                    prv_statues.iter().map(|a| (a.x, a.y)).collect(),
                )
            } else {
                // Subsample deterministically to <= 45 points so O(n^2 * m^2) finishes in < 5ms.
                let step_c = (canonical_anchors.len() / 40).max(1);
                let step_p = (provider_anchors.len() / 40).max(1);
                (
                    canonical_anchors
                        .iter()
                        .step_by(step_c)
                        .take(45)
                        .map(|a| (a.x, a.y))
                        .collect(),
                    provider_anchors
                        .iter()
                        .step_by(step_p)
                        .take(45)
                        .map(|a| (a.x, a.y))
                        .collect(),
                )
            };

        let target_early = (dst_sub.len().min(src_sub.len()) * 85) / 100;
        let span_x = src_sub
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max)
            - src_sub.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let span_y = src_sub
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max)
            - src_sub.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let min_dx = (span_x * 0.12).max(120.0);
        let min_dy = (span_y * 0.12).max(120.0);

        'outer: for i in 0..src_sub.len() {
            for j in (i + 1)..src_sub.len() {
                let dx_c = src_sub[j].0 - src_sub[i].0;
                let dy_c = src_sub[j].1 - src_sub[i].1;
                if dx_c.abs() < min_dx || dy_c.abs() < min_dy {
                    continue;
                }
                for a in 0..dst_sub.len() {
                    for b in 0..dst_sub.len() {
                        if a == b {
                            continue;
                        }
                        let sx = (dst_sub[b].0 - dst_sub[a].0) / dx_c;
                        let sy = (dst_sub[b].1 - dst_sub[a].1) / dy_c;
                        if sx <= 1e-10 {
                            continue;
                        }
                        if let Some(flip) = expected_y_flip {
                            if flip && sy >= -1e-10 {
                                continue;
                            }
                            if !flip && sy <= 1e-10 {
                                continue;
                            }
                        } else if sy.abs() <= 1e-10 {
                            continue;
                        }
                        if ((sx.abs() / sy.abs()) - 1.0).abs() > 0.06 {
                            continue;
                        }
                        let bx = dst_sub[a].0 - sx * src_sub[i].0;
                        let dy = dst_sub[a].1 - sy * src_sub[i].1;
                        let inv_sx = 1.0 / sx.abs();
                        let inv_sy = 1.0 / sy.abs();
                        let mut inl = 0usize;
                        for &(px, py) in &dst_sub {
                            let mut ok = false;
                            for &(cx, cy) in &src_sub {
                                let err = ((px - (sx * cx + bx)) * inv_sx)
                                    .hypot((py - (sy * cy + dy)) * inv_sy);
                                if err < 45.0 {
                                    ok = true;
                                    break;
                                }
                            }
                            if ok {
                                inl += 1;
                            }
                        }
                        if best_hyp.is_none_or(|(b_inl, ..)| inl > b_inl) {
                            best_hyp = Some((inl, sx, bx, sy, dy));
                            if inl >= target_early.max(4) {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
    }

    let Some((_, mut sx, mut bx, mut sy, mut dy)) = best_hyp else {
        if let Some(seed) = seed_override
            .cloned()
            .or_else(|| provider_default_transform(&prov, map_id))
        {
            return Ok(seed);
        }
        return Err(CoreError::NotFound(format!(
            "Auto-calibration found no consistent affine for {prov} map {map_id}"
        )));
    };

    // 3. Iterative ICP refinement on all anchors with progressive outlier rejection.
    const ICP_THRESHOLDS: [f64; 6] = [65.0, 35.0, 15.0, 6.0, 3.0, 3.0];
    let mut matched_can = Vec::new();
    let mut prev_matched = 0usize;
    for &thresh in &ICP_THRESHOLDS {
        let inv_sx = 1.0 / sx.abs();
        let inv_sy = 1.0 / sy.abs();
        let mut round_can = Vec::new();
        let mut round_prv = Vec::new();
        for c in canonical_anchors {
            let pred_x = sx * c.x + bx;
            let pred_y = sy * c.y + dy;
            let mut best_j = None;
            let mut best_err = f64::INFINITY;
            for (j, p) in provider_anchors.iter().enumerate() {
                let err = ((p.x - pred_x) * inv_sx).hypot((p.y - pred_y) * inv_sy);
                if err < best_err {
                    best_err = err;
                    best_j = Some(j);
                }
            }
            if let Some(j) = best_j
                && best_err < thresh
            {
                round_can.push((c.x, c.y));
                round_prv.push((provider_anchors[j].x, provider_anchors[j].y));
            }
        }
        // Keep the tighter pass only if enough consistent anchors survive (protects noisier maps).
        if round_can.len() < 3 || (thresh <= 6.0 && prev_matched >= 10 && round_can.len() * 2 < prev_matched) {
            break;
        }
        prev_matched = round_can.len();
        matched_can = round_can;
        let cx_v: Vec<f64> = matched_can.iter().map(|p| p.0).collect();
        let cy_v: Vec<f64> = matched_can.iter().map(|p| p.1).collect();
        let px_v: Vec<f64> = round_prv.iter().map(|p| p.0).collect();
        let py_v: Vec<f64> = round_prv.iter().map(|p| p.1).collect();
        if let (Ok((ax_n, bx_n)), Ok((cy_n, dy_n))) =
            (fit_1d_linear(&cx_v, &px_v), fit_1d_linear(&cy_v, &py_v))
            && ax_n.is_finite()
            && cy_n.is_finite()
            && ax_n.abs() > 1e-10
            && cy_n.abs() > 1e-10
        {
            sx = ax_n;
            bx = bx_n;
            sy = cy_n;
            dy = dy_n;
        }
    }

    if matched_can.len() < 3 {
        if let Some(seed) = seed_override
            .cloned()
            .or_else(|| provider_default_transform(&prov, map_id))
        {
            return Ok(seed);
        }
        return Err(CoreError::NotFound(format!(
            "Calibration matched too few anchors ({}) for {prov} map {map_id}",
            matched_can.len()
        )));
    }

    // 4. Piecewise zone discovery for unmatched area clusters (e.g. AppSample Inazuma & Easybreeze).
    let inv_sx = 1.0 / sx.abs();
    let inv_sy = 1.0 / sy.abs();
    let mut unmatched_by_area: BTreeMap<i32, Vec<&CanonicalAnchor>> = BTreeMap::new();
    let mut total_sq_err = 0.0;
    let mut total_matched = 0usize;

    for c in canonical_anchors {
        let pred_x = sx * c.x + bx;
        let pred_y = sy * c.y + dy;
        let mut best_err = f64::INFINITY;
        for p in provider_anchors {
            let err = ((p.x - pred_x) * inv_sx).hypot((p.y - pred_y) * inv_sy);
            if err < best_err {
                best_err = err;
            }
        }
        if best_err < 12.0 {
            total_sq_err += best_err * best_err;
            total_matched += 1;
        } else if best_err >= 35.0 {
            unmatched_by_area.entry(c.area_id).or_default().push(c);
        }
    }

    let mut zones = Vec::new();
    for (area_id, cluster) in unmatched_by_area {
        if cluster.len() < 4 {
            continue;
        }
        // Translation voting at fixed base scale (sx, sy).
        let mut best_shift: Option<(usize, f64, f64)> = None;
        for c0 in cluster.iter().take(6) {
            for p0 in provider_anchors {
                let cand_bx = p0.x - sx * c0.x;
                let cand_dy = p0.y - sy * c0.y;
                let mut inl = 0usize;
                for c in &cluster {
                    let px = sx * c.x + cand_bx;
                    let py = sy * c.y + cand_dy;
                    let ok = provider_anchors
                        .iter()
                        .any(|p| ((p.x - px) * inv_sx).hypot((p.y - py) * inv_sy) < 20.0);
                    if ok {
                        inl += 1;
                    }
                }
                if best_shift.is_none_or(|(bi, ..)| inl > bi) {
                    best_shift = Some((inl, cand_bx, cand_dy));
                }
            }
        }
        let Some((inl, z_bx, z_dy)) = best_shift else {
            continue;
        };
        if inl < 4 || inl * 10 < cluster.len() * 7 {
            continue;
        }
        // Refine zone offset via mean residual and verify sub-15u RMSE.
        let mut c_pts = Vec::new();
        let mut p_pts = Vec::new();
        let mut z_sq = 0.0;
        let mut z_max = 0.0_f64;
        for c in &cluster {
            let px = sx * c.x + z_bx;
            let py = sy * c.y + z_dy;
            if let Some((best_p, err)) = provider_anchors
                .iter()
                .map(|p| (p, ((p.x - px) * inv_sx).hypot((p.y - py) * inv_sy)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                && err < 20.0
            {
                c_pts.push((c.x, c.y));
                p_pts.push((best_p.x, best_p.y));
                z_sq += err * err;
                z_max = z_max.max(err);
            }
        }
        if c_pts.len() < 4 {
            continue;
        }
        let z_rmse = (z_sq / c_pts.len() as f64).sqrt();
        if z_rmse > 15.0 {
            continue;
        }
        // Compute exact mean offset for the zone at shared scale (or refit if well-spread).
        let mean_bx = p_pts
            .iter()
            .zip(c_pts.iter())
            .map(|(p, c)| p.0 - sx * c.0)
            .sum::<f64>()
            / c_pts.len() as f64;
        let mean_dy = p_pts
            .iter()
            .zip(c_pts.iter())
            .map(|(p, c)| p.1 - sy * c.1)
            .sum::<f64>()
            / c_pts.len() as f64;
        let (Some(cbbox), Some(pbbox)) =
            (SpatialBbox::from_points(&c_pts), SpatialBbox::from_points(&p_pts))
        else {
            continue;
        };
        total_sq_err += z_sq;
        total_matched += c_pts.len();
        zones.push(ZoneAffine {
            id: format!("area_{area_id}"),
            canonical_bbox: cbbox,
            provider_bbox: pbbox,
            affine: AxisAffine {
                ax: sx,
                bx: mean_bx,
                cy: sy,
                dy: mean_dy,
                map_id: Some(map_id),
                rmse: z_rmse,
                max_residual: z_max,
                anchor_count: c_pts.len(),
                source: "auto_calibrated_zone".to_owned(),
            },
        });
    }

    let overall_rmse = if total_matched > 0 {
        (total_sq_err / total_matched as f64).sqrt()
    } else {
        0.0
    };

    Ok(PiecewiseMapTransform {
        provider: prov,
        map_id,
        swap_lat_lng,
        base: AxisAffine {
            ax: sx,
            bx,
            cy: sy,
            dy,
            map_id: Some(map_id),
            rmse: overall_rmse,
            max_residual: 0.0,
            anchor_count: matched_can.len(),
            source: "auto_calibrated".to_owned(),
        },
        zones,
        matched_anchors: total_matched,
        total_canonical_anchors: canonical_anchors.len(),
        rmse_canonical: overall_rmse,
        source: "auto_calibrated".to_owned(),
    })
}

/// Unified `POST /calibrate_provider` entrypoint invoked by `gh-net::sync_server`
/// and `gh-app::callbacks`.
pub fn execute_provider_calibration(
    payload: &Value,
    canonical_anchors: &[CanonicalAnchor],
) -> CoreResult<Value> {
    let obj = payload
        .as_object()
        .ok_or_else(|| CoreError::InvalidData("Calibration payload must be a JSON object".into()))?;
    let provider = obj
        .get("provider")
        .or_else(|| obj.get("provider_id"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::InvalidArgument("Missing provider".into()))?;
    let map_id = obj
        .get("map_id")
        .and_then(|v| coerce_strict_int(v, "map_id").ok())
        .unwrap_or(2);
    let swap_lat_lng = obj
        .get("swap_lat_lng")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| provider == "kongying");
    let expected_y_flip = obj.get("expected_y_flip").and_then(Value::as_bool).or(
        match provider.as_str() {
            "appsample" | "mapgenie" => Some(true),
            "kongying" | "hoyolab" | "miyoushe" => Some(false),
            _ => None,
        },
    );

    let mut provider_anchors = Vec::new();
    if let Some(arr) = obj.get("anchors").and_then(Value::as_array) {
        for item in arr {
            let Some(m) = item.as_object() else {
                continue;
            };
            let x_v = m.get("x").or_else(|| m.get("lng"));
            let y_v = m.get("y").or_else(|| m.get("lat"));
            let (Some(xv), Some(yv)) = (x_v, y_v) else {
                continue;
            };
            let (Ok(x), Ok(y)) = (coerce_f64(xv, "anchor.x"), coerce_f64(yv, "anchor.y")) else {
                continue;
            };
            let is_statue = m
                .get("is_statue")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    m.get("category")
                        .or_else(|| m.get("type"))
                        .and_then(Value::as_str)
                        .is_some_and(|t| {
                            let low = t.to_ascii_lowercase();
                            low == "o2"
                                || low.contains("statue")
                                || low.contains("七天神像")
                        })
                });
            provider_anchors.push(ProviderAnchor { x, y, is_statue });
        }
    }

    let transform = calibrate_piecewise_from_anchors(
        &provider,
        map_id,
        canonical_anchors,
        &provider_anchors,
        expected_y_flip,
        swap_lat_lng,
        None,
    )?;

    Ok(json!({
        "ok": true,
        "provider": provider,
        "map_id": map_id,
        "transform": transform,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn piecewise_auto_calibration_discovers_shifted_island_zones() {
        // Synthetic Teyvat mainland (area 1) + shifted Inazuma island (area 3)
        // mimicking AppSample v7.2's -2203.6u horizontal island shift.
        let sx = 8.58625e-5_f64;
        let sy = -8.58625e-5_f64;
        let bx_main = 0.52269_f64;
        let dy_main = 0.19946_f64;
        let bx_inazuma = 0.33349_f64;

        let mut canonical = Vec::new();
        let mut provider = Vec::new();
        for i in 0..16 {
            let cx = -8000.0 + (i as f64) * 650.0;
            let cy = -4000.0 + ((i % 4) as f64) * 1800.0;
            let is_statue = i % 2 == 0;
            canonical.push(CanonicalAnchor {
                x: cx,
                y: cy,
                label_id: if is_statue { 2 } else { 3 },
                area_id: 1,
            });
            provider.push(ProviderAnchor {
                x: sx * cx + bx_main,
                y: sy * cy + dy_main,
                is_statue,
            });
        }
        for i in 0..8 {
            let cx = 1200.0 + (i as f64) * 320.0;
            let cy = 5000.0 + ((i % 3) as f64) * 1100.0;
            let is_statue = i % 2 == 0;
            canonical.push(CanonicalAnchor {
                x: cx,
                y: cy,
                label_id: if is_statue { 2 } else { 3 },
                area_id: 3,
            });
            provider.push(ProviderAnchor {
                x: sx * cx + bx_inazuma,
                y: sy * cy + dy_main,
                is_statue,
            });
        }

        let tf = calibrate_piecewise_from_anchors(
            "appsample",
            2,
            &canonical,
            &provider,
            Some(true),
            false,
            None,
        )
        .expect("piecewise calibration must succeed");
        assert_eq!(tf.matched_anchors, 24);
        assert!(tf.rmse_canonical < 0.05, "rmse={}", tf.rmse_canonical);
        assert_eq!(tf.zones.len(), 1);
        assert_eq!(tf.zones[0].id, "area_3");

        // Verify round-trip forward <-> inverse on both mainland and shifted Inazuma.
        for &(cx, cy) in &[(-2500.0, 500.0), (2000.0, 6200.0)] {
            let (lat, lng) = tf.to_leaflet_latlng(cx, cy);
            let (rcx, rcy) = tf.from_leaflet_latlng(lat, lng).unwrap();
            assert!((rcx - cx).abs() < 1e-4 && (rcy - cy).abs() < 1e-4);
        }
    }

    #[test]
    fn kongying_seed_swaps_leaflet_lat_lng_axes() {
        let tf = provider_default_transform("kongying", 2).expect("kongying map 2 seed");
        assert!(tf.swap_lat_lng);
        let (lat, lng) = tf.to_leaflet_latlng(1000.0, -500.0);
        // lat corresponds to +X East, lng corresponds to +Y South
        assert!((lat - (1.335301544 * 1000.0 + 1527.748294)).abs() < 1e-4);
        assert!((lng - (1.335165651 * -500.0 - 3931.249166)).abs() < 1e-4);
        let (cx, cy) = tf.from_leaflet_latlng(lat, lng).unwrap();
        assert!((cx - 1000.0).abs() < 1e-4 && (cy - (-500.0)).abs() < 1e-4);
    }

    #[test]
    fn category_aliases_n_to_1_and_duplicate_rejection() {
        let reg = CategoryRegistry::embedded();
        for lid in [8, 9, 212, 411, 509, 577, 703, 835] {
            assert_eq!(
                reg.resolve_hoyolab_label(lid, None).as_deref(),
                Some("shrine_of_depths")
            );
        }
        assert_eq!(reg.get_appsample_code("shrine_of_depths"), Some("o8"));
        assert_eq!(
            reg.resolve_target_category(None, Some("o8"), None, None, None)
                .as_deref(),
            Some("shrine_of_depths")
        );
        assert_eq!(
            reg.resolve_target_category(None, None, Some("地灵龛"), None, None)
                .as_deref(),
            Some("shrine_of_depths")
        );
        assert_eq!(
            reg.resolve_target_category(
                None,
                None,
                None,
                Some("FontaineShrineOfDepths/deadbeef"),
                None
            )
            .as_deref(),
            Some("shrine_of_depths")
        );

        assert_eq!(reg.resolve_hoyolab_label(18, None).as_deref(), Some("seelie"));
        assert_eq!(reg.resolve_hoyolab_label(148, None).as_deref(), Some("seelie"));
        assert_eq!(reg.get_appsample_code("seelie"), Some("o18"));
        assert_eq!(
            reg.resolve_hoyolab_label(205, None).as_deref(),
            Some("electro_seelie")
        );
        assert_eq!(
            reg.resolve_target_category(None, None, None, Some("InazumaElectroSeelie/abc123"), None)
                .as_deref(),
            Some("electro_seelie")
        );
        assert_eq!(
            reg.resolve_target_category(None, None, None, Some("DragonspineSeelie/abc123"), None)
                .as_deref(),
            Some("seelie")
        );

        for (lid, expected) in [
            (17, "common_chest"),
            (44, "exquisite_chest"),
            (45, "precious_chest"),
            (46, "luxurious_chest"),
            (269, "remarkable_chest"),
            (64, "time_trial_challenge"),
            (5, "anemoculus"),
            (6, "geoculus"),
            (141, "crimson_agate"),
            (194, "electroculus"),
            (357, "lumenspar"),
            (403, "dendroculus"),
            (508, "hydroculus"),
            (554, "spirit_carp"),
            (626, "pyroculus"),
            (694, "lunoculus"),
            (833, "cryoculus"),
            (85, "viewpoint"),
            (356, "radiant_spincrystal"),
            (597, "monetoo"),
        ] {
            assert_eq!(reg.resolve_hoyolab_label(lid, None).as_deref(), Some(expected));
        }

        for anchor_lid in [2, 3, 154] {
            let cat = reg.resolve_hoyolab_label(anchor_lid, None).unwrap();
            assert!(reg.is_calibration_anchor(Some(&cat)));
        }

        let dup = json!({
            "categories": {
                "cat_a": {"hoyolab_label_ids": [99]},
                "cat_b": {"hoyolab_label_ids": [99]},
            }
        });
        assert!(matches!(
            CategoryRegistry::from_value(&dup),
            Err(CoreError::InvalidData(_))
        ));
    }

    #[test]
    fn hoyolab_envelope_parser_and_missing_tracking() {
        let point_list = json!({
            "retcode": 0,
            "message": "OK",
            "data": {
                "label_list": [
                    {"id": 17, "name": "Common Chest"},
                    {"id": 577, "name": "Natlan Shrine of Depths"},
                    {"id": 148, "name": "Warming Seelie"},
                    {"id": 9991, "name": "Mysterious Totem"},
                    {"id": 9992, "name": "Shrine of Depths"}
                ],
                "point_list": [
                    {"id": 1001, "label_id": 17, "x_pos": 120.5, "y_pos": -340.25, "z_level": 0, "icon_sign": 0, "point_group": null},
                    {"id": 1002, "label_id": 577, "x_pos": -500.0, "y_pos": 820.0, "z_level": 0, "icon_sign": 1, "point_group": {"group_id": 248, "floor_id": 372}},
                    {"id": 1003, "label_id": 148, "x_pos": "42.25", "y_pos": "19.75"},
                    {"id": 1004, "label_id": 9991, "x_pos": 10.0, "y_pos": 20.0},
                    {"id": 1005, "label_id": 9992, "x_pos": 30.0, "y_pos": 40.0}
                ]
            }
        });
        let marked_list = json!({
            "retcode": 0,
            "message": "OK",
            "data": {
                "list": [
                    {"point_id": 1001, "created_at": "2026-10-01 12:30:00"},
                    {"point_id": "1001", "created_at": "2026-10-02 00:00:00"},
                    {"point_id": 1002, "updated_at": 1760000000},
                    {"point_id": 1003, "marked_at": "2026/10/05 08:15:30"},
                    {"point_id": 1004},
                    {"point_id": 1005},
                    {"point_id": 777777},
                    {"point_id": 888888}
                ]
            }
        });

        let parsed = parse_hoyolab_export(&point_list, &marked_list, 2, None).unwrap();
        assert_eq!(parsed.map_id, 2);
        assert_eq!(parsed.point_index_size, 5);
        assert_eq!(parsed.total_marked, 7);
        assert_eq!(parsed.missing_from_point_list, vec![777777, 888888]);
        assert_eq!(parsed.unmapped_label_ids, vec![9991]);
        assert_eq!(parsed.pins.len(), 5);

        let by_id: HashMap<i64, &CanonicalPin> =
            parsed.pins.iter().map(|p| (p.hoyolab_id, p)).collect();
        assert_eq!(by_id[&1001].canonical_category, "common_chest");
        assert!(!by_id[&1001].is_underground);
        assert_eq!(by_id[&1001].marked_at.as_deref(), Some("2026-10-01T12:30:00Z"));
        assert_eq!(by_id[&1002].canonical_category, "shrine_of_depths");
        assert_eq!(by_id[&1002].point_group, Some(248));
        assert_eq!(by_id[&1002].floor_id.as_deref(), Some("372"));
        assert!(by_id[&1002].is_underground);
        assert!(by_id[&1002].marked_at.is_some());
        assert_eq!(by_id[&1003].canonical_category, "seelie");
        assert_eq!(by_id[&1003].x_pos, 42.25);
        assert_eq!(by_id[&1003].y_pos, 19.75);
        assert_eq!(by_id[&1004].canonical_category, "unmapped_9991");
        assert_eq!(by_id[&1004].label_name.as_deref(), Some("Mysterious Totem"));
        assert_eq!(by_id[&1005].canonical_category, "shrine_of_depths");

        assert!(parse_hoyolab_export(&json!({"retcode": -100}), &marked_list, 2, None).is_err());

        // Real multimap_v4.json Enkanomiya compatibility check when present
        let mm_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("data")
            .join("multimap_v4.json");
        if mm_path.is_file() {
            let mm: Value =
                serde_json::from_str(&std::fs::read_to_string(mm_path).unwrap()).unwrap();
            let enk_wps = mm["maps"]["7"]["waypoints"].as_array().unwrap();
            let mk_list: Vec<Value> = enk_wps[..5]
                .iter()
                .map(|w| json!({"point_id": w["id"]}))
                .collect();
            let res = parse_hoyolab_export(
                &json!({"retcode": 0, "data": {"point_list": enk_wps}}),
                &json!({"retcode": 0, "data": {"list": mk_list}}),
                7,
                None,
            )
            .unwrap();
            assert_eq!(res.pins.len(), 5);
            assert!(res.pins.iter().all(|p| p.map_id == 7));
        }
    }

    #[test]
    fn per_map_affine_y_flip_outlier_trim_and_isolation() {
        let seed = kongying_twt40_seed();
        assert_eq!(seed.map_id, Some(2));
        let (kx, ky) = seed.transform(300.0, 600.0);
        assert!((kx - (400.0 + 1523.4)).abs() < 1e-6);
        assert!((ky - (800.0 - 3921.6)).abs() < 1e-6);

        let true_teyvat = AxisAffine {
            ax: 4.0 / 3.0,
            bx: 1531.0,
            cy: 4.0 / 3.0,
            dy: -3915.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "truth".to_owned(),
        };
        let src_anchors = vec![
            CanonicalPin::new(1, 2, 3, "teleport_waypoint", -2000.0, -1500.0),
            CanonicalPin::new(2, 2, 3, "teleport_waypoint", 1200.0, -800.0),
            CanonicalPin::new(3, 2, 2, "statue_of_the_seven", -500.0, 1400.0),
            CanonicalPin::new(4, 2, 3, "teleport_waypoint", 2400.0, 900.0),
        ];
        let dst_anchors: Vec<TargetPin> = src_anchors
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (x, y) = true_teyvat.transform(p.x_pos, p.y_pos);
                let mut t = TargetPin::new(100 + i as i64, x, y);
                t.canonical_category = Some(p.canonical_category.clone());
                t.map_id = Some(2);
                t
            })
            .collect();

        let refined = calibrate_from_shared_anchors(
            &src_anchors,
            &dst_anchors,
            2,
            &seed,
            100.0,
            None,
        )
        .unwrap();
        assert_eq!(refined.map_id, Some(2));
        assert!((refined.ax - true_teyvat.ax).abs() < 1e-6);
        assert!((refined.bx - true_teyvat.bx).abs() < 1e-6);
        assert!((refined.cy - true_teyvat.cy).abs() < 1e-6);
        assert!((refined.dy - true_teyvat.dy).abs() < 1e-6);

        let enk_src = [(-400.0, -300.0), (500.0, -100.0), (-200.0, 600.0), (700.0, 400.0)];
        let enk_true = AxisAffine {
            ax: 1.25,
            bx: 240.0,
            cy: -1.75,
            dy: 1180.0,
            map_id: Some(7),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "truth".to_owned(),
        };
        let enk_dst: Vec<(f64, f64)> = enk_src.iter().map(|&(x, y)| enk_true.transform(x, y)).collect();
        let enk_fit = fit_axis_affine(&enk_src, &enk_dst, Some(7), true, 3.5, "fitted").unwrap();
        assert!(enk_fit.has_y_flip());
        assert!((enk_fit.ax - 1.25).abs() < 1e-9);
        assert!((enk_fit.cy - (-1.75)).abs() < 1e-9);

        let inv = enk_fit.inverse().unwrap();
        let (tx, ty) = enk_fit.transform(123.4, -567.8);
        let (rx, ry) = inv.transform(tx, ty);
        assert!((rx - 123.4).abs() < 1e-7);
        assert!((ry - (-567.8)).abs() < 1e-7);

        // Outlier trimming
        let rob_src = [
            (0.0, 0.0),
            (100.0, 50.0),
            (200.0, -100.0),
            (-150.0, 200.0),
            (300.0, 250.0),
            (50.0, -250.0),
        ];
        let mut rob_dst: Vec<(f64, f64)> =
            rob_src.iter().map(|&(x, y)| enk_true.transform(x, y)).collect();
        rob_dst[2].0 += 600.0;
        rob_dst[2].1 -= 450.0;
        let rob_fit = fit_axis_affine(&rob_src, &rob_dst, Some(7), true, 3.5, "fitted").unwrap();
        assert_eq!(rob_fit.anchor_count, 5);
        assert!((rob_fit.ax - 1.25).abs() < 1e-6);

        let mut aff_reg = PerMapAffineRegistry::new();
        assert!(aff_reg.get("kongying", 2, true).is_some());
        assert!(aff_reg.get("kongying", 7, true).is_none());
        assert!(aff_reg.require("kongying", 9, true).is_err());
        aff_reg.set("kongying", 7, enk_fit).unwrap();
        assert!(aff_reg.require("kongying", 7, true).unwrap().has_y_flip());

        // Cross-map guard
        assert!(
            match_pins_one_to_one(
                &[CanonicalPin::new(1, 7, 44, "common_chest", 0.0, 0.0)],
                &[],
                &seed,
                MatchOptions::default(),
                None,
                None,
            )
            .is_err()
        );

        for (lng, lat) in [(-120.5, 35.2), (0.0, 0.0), (45.125, -22.75)] {
            let (px, py) = web_mercator_to_pixel(lng, lat, 13);
            let (lng2, lat2) = pixel_to_web_mercator(px, py, 13);
            assert!((lng - lng2).abs() < 1e-7);
            assert!((lat - lat2).abs() < 1e-7);
        }
    }

    #[test]
    fn strict_max_dist_category_bucketing_and_name_fallback() {
        let ident = AxisAffine {
            ax: 1.0,
            bx: 0.0,
            cy: 1.0,
            dy: 0.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "ident".to_owned(),
        };
        let mut p6 = CanonicalPin::new(6, 2, 9991, "unmapped_9991", 900.0, 900.0);
        p6.label_name = Some("Mysterious Totem".to_owned());
        let mut p7 = CanonicalPin::new(7, 2, 9999, "unmapped_9999", 1100.0, 1100.0);
        p7.label_name = Some("Ghost Relic".to_owned());

        let sources = vec![
            CanonicalPin::new(1, 2, 44, "common_chest", 100.0, 100.0),
            CanonicalPin::new(2, 2, 44, "common_chest", 5100.0, 100.0),
            CanonicalPin::new(3, 2, 44, "common_chest", 300.0, 300.0),
            CanonicalPin::new(4, 2, 577, "shrine_of_depths", 500.0, 500.0),
            CanonicalPin::new(5, 2, 148, "seelie", 700.0, 700.0),
            p6,
            p7,
        ];
        let mut t1 = TargetPin::new(101, 102.0, 100.0);
        t1.canonical_category = Some("common_chest".to_owned());
        t1.map_id = Some(2);
        let mut t2 = TargetPin::new(102, 300.0, 300.0);
        t2.canonical_category = Some("exquisite_chest".to_owned());
        t2.map_id = Some(2);
        let mut t3 = TargetPin::new(103, 501.0, 499.0);
        t3.appsample_code = Some("o8".to_owned());
        t3.map_id = Some(2);
        let mut t4 = TargetPin::new(104, 699.5, 700.5);
        t4.appsample_code = Some("o18".to_owned());
        t4.map_id = Some(2);
        let mut t5 = TargetPin::new(105, 901.0, 900.0);
        t5.category_name = Some("Mysterious Totem".to_owned());
        t5.map_id = Some(2);

        let report = match_pins_one_to_one(
            &sources,
            &[t1, t2, t3, t4, t5],
            &ident,
            MatchOptions::default(),
            None,
            Some(&[888888]),
        )
        .unwrap();

        let matched_map: HashMap<i64, &MatchResult> =
            report.matched.iter().map(|m| (m.source.hoyolab_id, m)).collect();
        let unmatched_map: HashMap<i64, &UnmatchedPin> = report
            .unmatched
            .iter()
            .map(|u| (u.source.hoyolab_id, u))
            .collect();

        assert_eq!(matched_map[&1].target.target_id, TargetId::Int(101));
        assert_eq!(matched_map[&4].target.target_id, TargetId::Int(103));
        assert_eq!(matched_map[&5].target.target_id, TargetId::Int(104));
        assert_eq!(matched_map[&6].target.target_id, TargetId::Int(105));
        assert_eq!(matched_map[&6].matched_via, "name_fallback");
        assert_eq!(unmatched_map[&2].reason, "exceeds_max_dist");
        assert_eq!(unmatched_map[&3].reason, "exceeds_max_dist");
        assert_eq!(unmatched_map[&7].reason, "unmapped_category");
        assert_eq!(report.unmapped_categories, vec!["unmapped_9999"]);
        assert!((report.match_rate_pct - 50.0).abs() < 1e-6);
    }

    #[test]
    fn one_to_one_ssap_brute_force_ambiguity_and_spatial_grid() {
        let ident = AxisAffine {
            ax: 1.0,
            bx: 0.0,
            cy: 1.0,
            dy: 0.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "ident".to_owned(),
        };

        // Augmenting-path test
        let aug_src = vec![
            CanonicalPin::new(30, 2, 44, "common_chest", 0.0, 0.0),
            CanonicalPin::new(31, 2, 44, "common_chest", 6.0, 0.0),
        ];
        let mut td1 = TargetPin::new(401, 5.0, 0.0);
        td1.canonical_category = Some("common_chest".to_owned());
        let mut td2 = TargetPin::new(402, 13.0, 0.0);
        td2.canonical_category = Some("common_chest".to_owned());
        let rep_opt = match_pins_one_to_one(
            &aug_src,
            &[td1, td2],
            &ident,
            MatchOptions {
                max_dist: 10.0,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(rep_opt.matched.len(), 2);

        // Brute-force SSAP check across 25 random 5x5 bipartite graphs
        let mut rng = 20261121u64;
        let mut next_u64 = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            rng
        };
        for _ in 0..25 {
            let mut edges = Vec::new();
            let mut edge_mat = [[None; 5]; 5];
            for (u, row) in edge_mat.iter_mut().enumerate() {
                for (v, cell) in row.iter_mut().enumerate() {
                    let r = ((next_u64() >> 11) as f64) / ((1u64 << 53) as f64);
                    if r < 0.55 {
                        let w = (0.1 + r * 26.0 * 1000.0).round() / 1000.0;
                        edges.push((u, v, w));
                        *cell = Some(w);
                    }
                }
            }
            let got = solve_bipartite_one_to_one(&edges, true);
            let used_v: HashSet<usize> = got.values().map(|&(v, _)| v).collect();
            assert_eq!(used_v.len(), got.len());
            let got_card = got.len();
            let got_cost: f64 = got.values().map(|&(_, d)| d).sum();

            // Brute-force over all injective partial assignments on 5x5 (6^5 = 7776)
            let mut best_card = 0usize;
            let mut best_cost = f64::INFINITY;
            for mask in 0..(6usize.pow(5)) {
                let mut m = mask;
                let mut card = 0usize;
                let mut cost = 0.0;
                let mut seen_v = [false; 5];
                let mut valid = true;
                for row in &edge_mat {
                    let choice = m % 6;
                    m /= 6;
                    if choice < 5 {
                        if seen_v[choice] {
                            valid = false;
                            break;
                        }
                        let Some(w) = row[choice] else {
                            valid = false;
                            break;
                        };
                        seen_v[choice] = true;
                        card += 1;
                        cost += w;
                    }
                }
                if valid && (card > best_card || (card == best_card && cost < best_cost)) {
                    best_card = card;
                    best_cost = cost;
                }
            }
            assert_eq!(got_card, best_card);
            assert!((got_cost - best_cost).abs() < 1e-5);
        }

        // Spatial grid equivalence (>25,000 pairs)
        let grid_src: Vec<CanonicalPin> = (0..200)
            .map(|i| {
                CanonicalPin::new(
                    1000 + i,
                    2,
                    44,
                    "common_chest",
                    (i * 40) as f64,
                    ((i % 7) * 50) as f64,
                )
            })
            .collect();
        let mut grid_dst: Vec<TargetPin> = (0..195)
            .map(|i| {
                let mut t = TargetPin::new(2000 + i, (i * 40) as f64 + 1.5, ((i % 7) * 50) as f64 - 0.5);
                t.canonical_category = Some("common_chest".to_owned());
                t
            })
            .collect();
        for _ in 0..5 {
            let mut t = TargetPin::new(9999, 999999.0, 999999.0);
            t.canonical_category = Some("common_chest".to_owned());
            grid_dst.push(t);
        }
        let grid_rep = match_pins_one_to_one(
            &grid_src,
            &grid_dst,
            &ident,
            MatchOptions::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(grid_rep.matched.len(), 195);
        assert_eq!(grid_rep.unmatched.len(), 5);
        assert!(grid_rep.unmatched.iter().all(|u| u.reason == "exceeds_max_dist"));
    }

    #[test]
    fn underground_layer_separation() {
        let ident = AxisAffine {
            ax: 1.0,
            bx: 0.0,
            cy: 1.0,
            dy: 0.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "ident".to_owned(),
        };
        let mut s2 = CanonicalPin::new(2, 2, 44, "common_chest", 500.0, -500.0);
        s2.floor_id = Some("372".to_owned());
        s2.point_group = Some(248);
        s2.is_underground = true;
        let mut s3 = CanonicalPin::new(3, 2, 44, "common_chest", 500.0, -500.0);
        s3.floor_id = Some("373".to_owned());
        s3.point_group = Some(248);
        s3.is_underground = true;
        let stacked_src = vec![
            CanonicalPin::new(1, 2, 44, "common_chest", 500.0, -500.0),
            s2.clone(),
            s3,
        ];

        let mut d1 = TargetPin::new(901, 500.0, -500.0);
        d1.canonical_category = Some("common_chest".to_owned());
        d1.floor_id = Some("373".to_owned());
        d1.point_group = Some(248);
        d1.is_underground = Some(true);
        let mut d2 = TargetPin::new(902, 500.0, -500.0);
        d2.canonical_category = Some("common_chest".to_owned());
        d2.is_underground = Some(false);
        let mut d3 = TargetPin::new(903, 500.0, -500.0);
        d3.canonical_category = Some("common_chest".to_owned());
        d3.floor_id = Some("372".to_owned());
        d3.point_group = Some(248);
        d3.is_underground = Some(true);

        let rep = match_pins_one_to_one(
            &stacked_src,
            &[d1, d2, d3],
            &ident,
            MatchOptions::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(rep.matched.len(), 3);
        assert_eq!(rep.ambiguous.len(), 0);
        let pairs: HashMap<i64, TargetId> = rep
            .matched
            .iter()
            .map(|m| (m.source.hoyolab_id, m.target.target_id.clone()))
            .collect();
        assert_eq!(pairs[&1], TargetId::Int(902));
        assert_eq!(pairs[&2], TargetId::Int(903));
        assert_eq!(pairs[&3], TargetId::Int(901));

        let mut surf = TargetPin::new(910, 500.0, -500.0);
        surf.canonical_category = Some("common_chest".to_owned());
        surf.is_underground = Some(false);
        let mis = match_pins_one_to_one(&[s2], &[surf], &ident, MatchOptions::default(), None, None)
            .unwrap();
        assert_eq!(mis.unmatched[0].reason, "layer_mismatch");
    }

    #[test]
    fn adapters_roundtrip_and_utf8_non_ascii_gm007_base64() {
        let mut p11 = CanonicalPin::new(11, 2, 44, "common_chest", 100.0, 200.0);
        p11.marked_at = Some("2026-10-15T14:22:10Z".to_owned());
        let mut p12 = CanonicalPin::new(12, 2, 577, "shrine_of_depths", 400.0, -100.0);
        p12.marked_at = Some("2026-10-16T09:05:00Z".to_owned());
        let mut p13 = CanonicalPin::new(13, 2, 148, "seelie", 402.0, -100.0);
        p13.marked_at = Some("2026-10-16T09:06:00Z".to_owned());
        let src_pins = vec![p11, p12, p13];

        let seed = kongying_twt40_seed();
        let ky_raw = json!([
            {
                "id": 50020,
                "position": format!("{},{}", seed.transform(100.0, 200.0).0, seed.transform(100.0, 200.0).1),
                "itemList": [{"name": "普通宝箱"}],
                "hiddenFlag": 0
            },
            {
                "id": 50005,
                "position": [seed.transform(400.0, -100.0).0, seed.transform(400.0, -100.0).1],
                "markerTitle": "地灵龛",
                "hiddenFlag": 0
            },
            {
                "id": 50010,
                "position": [seed.transform(402.0, -100.0).0, seed.transform(402.0, -100.0).1],
                "itemList": [{"name": "温暖仙灵"}],
                "extra": {"underground": {"is_underground": false}}
            }
        ]);
        let ky_targets = KongyingAdapter::parse_markers(&ky_raw, 2, None).unwrap();
        let ky_rep =
            match_pins_one_to_one(&src_pins, &ky_targets, &seed, MatchOptions::default(), None, None)
                .unwrap();
        assert_eq!(ky_rep.matched.len(), 3);
        let ky_payload = KongyingAdapter::export(&ky_rep, true, Some(1761000000123), None).unwrap();
        assert!(KongyingAdapter::validate_schema(&ky_payload).is_empty());
        assert_eq!(ky_payload["Data_KYJG"], json!([50005, 50010, 50020]));
        assert_eq!(ky_payload["Time_KYJG"]["50020"], "2026/10/15 14:22:10");

        let merged_ky = KongyingAdapter::merge_existing(
            &json!({
                "Data_KYJG": [10001, 50020],
                "Time_KYJG": {"10001": "2025/01/01 00:00:00", "50020": "2025/05/05 05:05:05"},
                "updated_at": 1750000000000i64
            }),
            &ky_payload,
            None,
        )
        .unwrap();
        assert_eq!(merged_ky["Data_KYJG"], json!([10001, 50005, 50010, 50020]));
        assert_eq!(merged_ky["Time_KYJG"]["50020"], "2025/05/05 05:05:05");

        // Appsample columnar delta-encoded
        let app_aff = AxisAffine {
            ax: 12.5,
            bx: 1048576.0,
            cy: -12.5,
            dy: 1048576.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "app".to_owned(),
        };
        let ll: Vec<(f64, f64)> = src_pins
            .iter()
            .map(|p| {
                let (px, py) = app_aff.transform(p.x_pos, p.y_pos);
                pixel_to_web_mercator(px, py, 13)
            })
            .collect();
        let col_app = json!({
            "id": [7000, 15, 20, 5],
            "lng": [ll[0].0, ll[1].0, ll[2].0, 0.0],
            "lat": [ll[0].1, ll[1].1, ll[2].1, 0.0],
            "type": ["o17", "o8", "o18", "o17"],
            "mid": [2, 2, 2, 7]
        });
        let app_targets = AppsampleAdapter::parse_markers(&col_app, 2, 13, true, None).unwrap();
        let app_rep =
            match_pins_one_to_one(&src_pins, &app_targets, &app_aff, MatchOptions::default(), None, None)
                .unwrap();
        let app_out = AppsampleAdapter::export(&app_rep, true).unwrap();
        assert_eq!(app_out, json!({"ids": [7000, 7015, 7035]}));

        // GenshinMap GM_007~ UTF-8 non-ASCII Base64 roundtrip
        let gm_aff = AxisAffine {
            ax: 0.05,
            bx: 10.0,
            cy: -0.05,
            dy: -20.0,
            map_id: Some(2),
            rmse: 0.0,
            max_residual: 0.0,
            anchor_count: 0,
            source: "gm".to_owned(),
        };
        let gm_raw = json!([
            {"featureKey": "MondstadtCommonChest", "id": "a".repeat(40), "x": gm_aff.transform(100.0, 200.0).0, "y": gm_aff.transform(100.0, 200.0).1},
            {"featureKey": "NatlanShrineOfDepths", "id": "b".repeat(40), "x": gm_aff.transform(400.0, -100.0).0, "y": gm_aff.transform(400.0, -100.0).1},
            {"featureKey": "DragonspineSeelie", "id": "c".repeat(40), "x": gm_aff.transform(402.0, -100.0).0, "y": gm_aff.transform(402.0, -100.0).1},
        ]);
        let gm_targets = GenshinMapGithubAdapter::parse_markers(&gm_raw, 2, None).unwrap();
        let gm_rep = match_pins_one_to_one(
            &src_pins,
            &gm_targets,
            &gm_aff,
            MatchOptions {
                max_dist: 5.0,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        let existing_gm_obj = json!({
            "version": "GM_007",
            "options": {"locale": "vi-VN", "note": "Rương ở vách đá 璃月 🧭"},
            "displayed": {"features": {"MondstadtCommonChest": true}, "routes": {}},
            "completed": {"features": {format!("LiyueGeoculus/{}", "f".repeat(40)): 1700000001}},
            "editor": {"feature": {"data": [{"title": "Điểm đánh dấu tiếng Việt"}]}, "route": {"data": []}}
        });
        let existing_gm_b64 = GenshinMapGithubAdapter::encode(&existing_gm_obj).unwrap();
        let merged_gm_b64 =
            GenshinMapGithubAdapter::merge_into_encoded(&existing_gm_b64, &gm_rep, true, None)
                .unwrap();
        let merged_gm = GenshinMapGithubAdapter::decode(&merged_gm_b64).unwrap();
        assert_eq!(merged_gm["options"]["note"], "Rương ở vách đá 璃月 🧭");
        assert_eq!(
            merged_gm["editor"]["feature"]["data"][0]["title"],
            "Điểm đánh dấu tiếng Việt"
        );
        assert_eq!(
            merged_gm["completed"]["features"]
                .as_object()
                .unwrap()
                .len(),
            4
        );

        // Canonical JSON roundtrip
        let canon = CanonicalJsonAdapter::export_pins(
            &src_pins,
            Some(2),
            Some(&json!("2026-11-21T10:30:00Z")),
            &[55555],
            None,
        );
        let canon_str = CanonicalJsonAdapter::to_json(&canon, false).unwrap();
        let (parsed_pins, meta) =
            CanonicalJsonAdapter::parse(&Value::String(canon_str)).unwrap();
        assert_eq!(meta["schema"], CANONICAL_SCHEMA_V1);
        assert_eq!(parsed_pins.len(), 3);
        assert_eq!(meta["missing_from_point_list"], json!([55555]));
    }

    #[test]
    fn execute_cross_map_export_unified_entrypoint() {
        let req_canon = json!({
            "target_format": "canonical",
            "map_id": 2,
            "point_list": {"retcode": 0, "data": {"point_list": [{"id": 11, "label_id": 44, "x_pos": 10.0, "y_pos": 20.0}]}},
            "marked_list": {"retcode": 0, "data": {"list": [{"point_id": 11}, {"point_id": 999}]}}
        });
        let res_c = execute_cross_map_export(&req_canon, None, None).unwrap();
        assert_eq!(res_c["ok"], true);
        assert_eq!(res_c["format"], "canonical");
        assert_eq!(res_c["report"]["total_source_pins"], 1);
        assert_eq!(res_c["report"]["missing_from_point_list_count"], 1);

        // Uncalibrated sub-map returns ok=false without crashing
        let req_submap = json!({
            "format": "kongying",
            "map_id": 7,
            "pins": [],
            "target_markers": []
        });
        let res_sub = execute_cross_map_export(&req_submap, None, None).unwrap();
        assert_eq!(res_sub["ok"], false);
        assert!(res_sub["error"].as_str().unwrap().contains("map_id=7"));

        // Invalid format returns Err(InvalidArgument)
        let req_bad = json!({"target_format": "invalid_fmt"});
        assert!(matches!(
            execute_cross_map_export(&req_bad, None, None),
            Err(CoreError::InvalidArgument(_))
        ));

        // Kongying with target_markers + existing_payload merge + pre-normalized TargetPin support
        let req_ky = json!({
            "target_format": "kongying",
            "map_id": 7,
            "use_identity_affine": true,
            "pins": [
                {"hoyolab_id": 1, "map_id": 7, "label_id": 44, "canonical_category": "common_chest", "x_pos": 100.0, "y_pos": 200.0}
            ],
            "target_markers": [
                {"target_id": 500, "x": 101.0, "y": 200.0, "canonical_category": "common_chest", "map_id": 7}
            ],
            "existing_payload": {
                "Data_KYJG": [123],
                "Time_KYJG": {"123": "2026/01/01 00:00:00"},
                "updated_at": 1000
            }
        });
        let res_ky = execute_cross_map_export(&req_ky, None, None).unwrap();
        assert_eq!(res_ky["ok"], true);
        assert_eq!(res_ky["payload"]["Data_KYJG"], json!([123, 500]));
    }

    #[test]
    fn python_falsy_or_fallback_and_raw_genshinmap_regression() {
        // 1) parse_hoyolab_export: empty marked_at/mark_time falls through to updated_at,
        //    and point_group {"group_id": "", "id": 12} falls through to id=12.
        let pt_env = json!({
            "retcode": 0,
            "data": {
                "point_list": [
                    {
                        "id": 1001,
                        "label_id": 44,
                        "x_pos": 10.0,
                        "y_pos": 20.0,
                        "point_group": {"group_id": "", "id": 12, "floor_id": "201"}
                    }
                ]
            }
        });
        let mk_env = json!({
            "retcode": 0,
            "data": {
                "list": [
                    {"point_id": 1001, "marked_at": "", "mark_time": "", "updated_at": 1_760_000_000}
                ]
            }
        });
        let parsed = parse_hoyolab_export(&pt_env, &mk_env, 2, None).unwrap();
        assert_eq!(parsed.pins.len(), 1);
        assert_eq!(
            parsed.pins[0].marked_at.as_deref(),
            Some("2025-10-09T08:53:20Z")
        );
        assert_eq!(parsed.pins[0].point_group, Some(12));

        // 2) AppsampleAdapter columnar: empty id[] falls through to ids, empty type[] falls through to cat.
        let (lng, lat) = pixel_to_web_mercator(100.0, 200.0, 13);
        let col = json!({
            "id": [],
            "ids": [7000],
            "lng": [lng],
            "lat": [lat],
            "type": [],
            "cat": ["o17"]
        });
        let app_col = AppsampleAdapter::parse_markers(&col, 2, 13, true, None).unwrap();
        assert_eq!(app_col.len(), 1);
        assert_eq!(app_col[0].target_id, TargetId::Int(7000));
        assert_eq!(
            app_col[0].canonical_category.as_deref(),
            Some("common_chest")
        );

        // 3) AppsampleAdapter row: empty appsample_code="" falls through to type / o.
        let rows = json!([
            {"id": 7000, "x": 100.0, "y": 200.0, "appsample_code": "", "type": "o17"},
            {"id": 7001, "x": 105.0, "y": 205.0, "appsample_code": "", "type": "", "o": "o8"}
        ]);
        let app_rows = AppsampleAdapter::parse_markers(&rows, 2, 13, true, None).unwrap();
        assert_eq!(app_rows.len(), 2);
        assert_eq!(
            app_rows[0].canonical_category.as_deref(),
            Some("common_chest")
        );
        assert_eq!(
            app_rows[1].canonical_category.as_deref(),
            Some("shrine_of_depths")
        );

        // 4) GenshinMapGithubAdapter::parse_markers: empty id="" falls through to sha1_id,
        //    empty featureKey="" falls through to feature_key.
        let gm_feats = json!([
            {
                "featureKey": "DragonspineSeelie",
                "id": "",
                "sha1_id": "cccccccccccccccccccccccccccccccccccccccc",
                "x": 1.0,
                "y": 2.0
            },
            {
                "featureKey": "",
                "feature_key": "MondstadtCommonChest",
                "id": "",
                "sha1_id": "",
                "target_id": "dddddddddddddddddddddddddddddddddddddddd",
                "x": 3.0,
                "y": 4.0
            }
        ]);
        let gm_parsed = GenshinMapGithubAdapter::parse_markers(&gm_feats, 2, None).unwrap();
        assert_eq!(gm_parsed.len(), 2);
        assert_eq!(
            gm_parsed[0].target_id,
            TargetId::Str("DragonspineSeelie/cccccccccccccccccccccccccccccccccccccccc".to_owned())
        );
        assert_eq!(
            gm_parsed[1].target_id,
            TargetId::Str("MondstadtCommonChest/dddddddddddddddddddddddddddddddddddddddd".to_owned())
        );
        assert_eq!(
            gm_parsed[1].canonical_category.as_deref(),
            Some("common_chest")
        );

        // 5) execute_cross_map_export: raw GenshinMap marker with target_id + featureKey + x + y
        //    must route through GenshinMapGithubAdapter::parse_markers (not pre-normalized TargetPin),
        //    and empty target_format="" falls through to format / canonical.
        let req_gm_raw = json!({
            "target_format": "genshinmap",
            "map_id": 2,
            "use_identity_affine": true,
            "pins": [
                {"hoyolab_id": 1, "map_id": 2, "label_id": 44, "canonical_category": "common_chest", "x_pos": 15.0, "y_pos": -30.0}
            ],
            "target_markers": [
                {
                    "featureKey": "MondstadtCommonChest",
                    "target_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "x": 15.0,
                    "y": -30.0
                }
            ]
        });
        let res_gm = execute_cross_map_export(&req_gm_raw, None, None).unwrap();
        assert_eq!(res_gm["ok"], true);
        assert_eq!(res_gm["report"]["matched_count"], 1);
        let decoded = GenshinMapGithubAdapter::decode(res_gm["payload"].as_str().unwrap()).unwrap();
        assert!(
            decoded["completed"]["features"]
                .as_object()
                .unwrap()
                .contains_key("MondstadtCommonChest/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );

        let req_empty_fmt = json!({
            "target_format": "",
            "format": "",
            "pins": []
        });
        let res_fmt = execute_cross_map_export(&req_empty_fmt, None, None).unwrap();
        assert_eq!(res_fmt["ok"], true);
        assert_eq!(res_fmt["format"], "canonical");
    }

    #[test]
    fn closed_world_target_pin_routing_and_adversarial_regression() {
        // Structural guard: TARGET_PIN_FIELDS matches TargetPin's serialized fields exactly
        let serialized_tp = serde_json::to_value(TargetPin::new(1, 0.0, 0.0)).unwrap();
        let tp_obj = serialized_tp.as_object().unwrap();
        assert_eq!(tp_obj.len(), TARGET_PIN_FIELDS.len());
        assert!(tp_obj.keys().all(|k| TARGET_PIN_FIELDS.contains(&k.as_str())));

        // 1) Raw GenshinMap compound target_id only (no featureKey / canonical_category)
        let req_gm_compound = json!({
            "target_format": "genshinmap",
            "map_id": 2,
            "use_identity_affine": true,
            "pins": [
                {"hoyolab_id": 1, "map_id": 2, "label_id": 44, "canonical_category": "common_chest", "x_pos": 15.0, "y_pos": -30.0}
            ],
            "target_markers": [
                {"target_id": "MondstadtCommonChest/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "x": 15.0, "y": -30.0}
            ]
        });
        let res_gm_compound = execute_cross_map_export(&req_gm_compound, None, None).unwrap();
        assert_eq!(res_gm_compound["ok"], true);
        assert_eq!(res_gm_compound["report"]["matched_count"], 1);
        let decoded_compound =
            GenshinMapGithubAdapter::decode(res_gm_compound["payload"].as_str().unwrap()).unwrap();
        assert!(
            decoded_compound["completed"]["features"]
                .as_object()
                .unwrap()
                .contains_key("MondstadtCommonChest/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );

        // 2) Raw GenshinMap snake_case feature_key + target_id (no canonical_category)
        let req_gm_snake = json!({
            "target_format": "genshinmap",
            "map_id": 2,
            "use_identity_affine": true,
            "pins": [
                {"hoyolab_id": 1, "map_id": 2, "label_id": 44, "canonical_category": "common_chest", "x_pos": 15.0, "y_pos": -30.0}
            ],
            "target_markers": [
                {"feature_key": "MondstadtCommonChest", "target_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "x": 15.0, "y": -30.0}
            ]
        });
        let res_gm_snake = execute_cross_map_export(&req_gm_snake, None, None).unwrap();
        assert_eq!(res_gm_snake["ok"], true);
        assert_eq!(res_gm_snake["report"]["matched_count"], 1);
        let decoded_snake =
            GenshinMapGithubAdapter::decode(res_gm_snake["payload"].as_str().unwrap()).unwrap();
        assert!(
            decoded_snake["completed"]["features"]
                .as_object()
                .unwrap()
                .contains_key("MondstadtCommonChest/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );

        // 3) Raw Kongying marker carrying id + target_id + item_name
        let req_ky_raw = json!({
            "target_format": "kongying",
            "map_id": 2,
            "affine": {"ax": 1.0, "bx": 0.0, "cy": 1.0, "dy": 0.0},
            "pins": [
                {"hoyolab_id": 1, "map_id": 2, "label_id": 44, "canonical_category": "common_chest", "x_pos": 100.0, "y_pos": 200.0}
            ],
            "target_markers": [
                {"id": 500, "target_id": 500, "x": 100.0, "y": 200.0, "item_name": "Common Chest"}
            ]
        });
        let res_ky_raw = execute_cross_map_export(&req_ky_raw, None, None).unwrap();
        assert_eq!(res_ky_raw["ok"], true);
        assert_eq!(res_ky_raw["report"]["matched_count"], 1);
        assert_eq!(res_ky_raw["payload"]["Data_KYJG"], json!([500]));

        // 4) Raw Appsample marker carrying id + target_id + type
        let req_app_raw = json!({
            "target_format": "appsample",
            "map_id": 2,
            "affine": {"ax": 1.0, "bx": 0.0, "cy": 1.0, "dy": 0.0},
            "pins": [
                {"hoyolab_id": 1, "map_id": 2, "label_id": 17, "canonical_category": "common_chest", "x_pos": 100.0, "y_pos": 200.0}
            ],
            "target_markers": [
                {"id": 7000, "target_id": 7000, "x": 100.0, "y": 200.0, "type": "o17"}
            ]
        });
        let res_app_raw = execute_cross_map_export(&req_app_raw, None, None).unwrap();
        assert_eq!(res_app_raw["ok"], true);
        assert_eq!(res_app_raw["report"]["matched_count"], 1);
        assert_eq!(res_app_raw["payload"]["ids"], json!([7000]));

        // 5) Pre-normalized TargetPin JSON (minimal & full serialized) across all 3 target formats,
        //    including target_id: 0 for Kongying & Appsample and explicit null fields
        for fmt in ["kongying", "appsample", "genshinmap"] {
            let minimal_tid = if fmt == "genshinmap" {
                json!("MondstadtCommonChest/cccccccccccccccccccccccccccccccccccccccc")
            } else {
                json!(0)
            };
            let mut full_pin = if fmt == "genshinmap" {
                TargetPin::new(
                    "MondstadtCommonChest/dddddddddddddddddddddddddddddddddddddddd",
                    15.0,
                    -30.0,
                )
            } else {
                TargetPin::new(0, 15.0, -30.0)
            };
            full_pin.canonical_category = Some("common_chest".to_owned());
            full_pin.map_id = Some(2);

            for target_json in [
                json!({
                    "target_id": minimal_tid,
                    "x": 15.0,
                    "y": -30.0,
                    "canonical_category": "common_chest",
                    "map_id": 2,
                    "is_underground": null,
                    "extra": null
                }),
                serde_json::to_value(&full_pin).unwrap(),
            ] {
                let req = json!({
                    "target_format": fmt,
                    "map_id": 2,
                    "affine": {"ax": 1.0, "bx": 0.0, "cy": 1.0, "dy": 0.0},
                    "pins": [
                        {"hoyolab_id": 1, "map_id": 2, "label_id": 44, "canonical_category": "common_chest", "x_pos": 15.0, "y_pos": -30.0}
                    ],
                    "target_pins": [target_json]
                });
                let res = execute_cross_map_export(&req, None, None).unwrap();
                assert_eq!(res["ok"], true, "failed for format={fmt}");
                assert_eq!(res["report"]["matched_count"], 1, "matched_count for format={fmt}");
                if fmt == "kongying" {
                    assert_eq!(res["payload"]["Data_KYJG"], json!([0]));
                } else if fmt == "appsample" {
                    assert_eq!(res["payload"]["ids"], json!([0]));
                }
            }
        }

        // Falsy target_markers (e.g. "") falls back to [] instead of erroring
        let req_falsy_tm = json!({
            "target_format": "kongying",
            "map_id": 2,
            "use_identity_affine": true,
            "pins": [],
            "target_markers": ""
        });
        let res_falsy_tm = execute_cross_map_export(&req_falsy_tm, None, None).unwrap();
        assert_eq!(res_falsy_tm["ok"], true);
        assert_eq!(res_falsy_tm["payload"]["Data_KYJG"], json!([]));
    }

    #[test]
    fn null_safe_serde_and_genshinmap_preferences_validation_parity() {
        // Direct serde null safety on CanonicalPin::is_underground and TargetPin::extra
        let cp: CanonicalPin = serde_json::from_value(json!({
            "hoyolab_id": 1,
            "label_id": 44,
            "canonical_category": "common_chest",
            "x_pos": 1.0,
            "y_pos": 2.0,
            "is_underground": null
        }))
        .unwrap();
        assert!(!cp.is_underground);

        let tp: TargetPin = serde_json::from_value(json!({
            "target_id": 42,
            "x": 1.0,
            "y": 2.0,
            "is_underground": null,
            "extra": null
        }))
        .unwrap();
        assert_eq!(tp.is_underground, None);
        assert!(tp.extra.is_empty());

        // GenshinMapGithubAdapter::build_payload / export validation parity
        let empty_report = TransferReport {
            matched: Vec::new(),
            ambiguous: Vec::new(),
            unmatched: Vec::new(),
            unmapped_categories: Vec::new(),
            missing_from_point_list: Vec::new(),
            match_rate_pct: 0.0,
        };

        let bad_completed = json!({"completed": "not_an_object"});
        assert!(matches!(
            GenshinMapGithubAdapter::export(&empty_report, true, Some(1000), Some(&bad_completed)),
            Err(CoreError::InvalidData(_))
        ));

        let bad_features_list = json!({"completed": {"features": ["bad"]}});
        assert!(matches!(
            GenshinMapGithubAdapter::export(&empty_report, true, Some(1000), Some(&bad_features_list)),
            Err(CoreError::InvalidData(_))
        ));

        let bad_features_entry = json!({"completed": {"features": {"MondstadtCommonChest/abc": "bad"}}});
        assert!(matches!(
            GenshinMapGithubAdapter::export(&empty_report, true, Some(1000), Some(&bad_features_entry)),
            Err(CoreError::InvalidData(_))
        ));

        let bad_features_neg = json!({"completed": {"features": {"MondstadtCommonChest/abc": -1}}});
        assert!(matches!(
            GenshinMapGithubAdapter::export(&empty_report, true, Some(1000), Some(&bad_features_neg)),
            Err(CoreError::InvalidData(_))
        ));
    }
}
