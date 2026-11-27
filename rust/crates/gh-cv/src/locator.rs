//! Multi-map landmark graph, Sobel terrain refinement (`WORLD_ANCHOR_Y_OFFSET = -8.5`),
//! edge compass rejection, and cave alpha-mask matching (`src/locator.py`).

use crate::heading360::wrap_angle_360;
use crate::ops::color::{bgr_px_to_gray, bgr_to_gray, bgr_to_hsv, frame_view_to_bgr};
use crate::ops::contours::{
    arc_length, approx_poly_dp, bounding_rect, contour_area, contour_moments, convex_hull,
    min_enclosing_triangle,
};
use crate::ops::filters::box_filter_f32;
use crate::ops::morph::{get_structuring_element, morphology_ex_u8};
use crate::ops::ncc::{match_template_u8_hwc, min_max_loc_f32};
use crate::ops::resize::{resize_u8, resize_u8_hwc};
use crate::ops::sobel::sobel_magnitude_u8_f32;
use crate::ops::{Interpolation, MorphOp, MorphShape, Point2f, Point2i, TemplateMatchMode};
use crate::zoom_slider::{ZoomSliderDetector, ZoomSliderResult};
use gh_core::assets::resolve_data_asset;
use gh_core::frame::{FrameBuf, FrameView, PixelFormat};
use gh_core::multimap::{MapMeta, MultimapDb, PointGroupEntry, WaypointEntry};
use gh_core::npz::{NpyArray, NpyData, NpzArchive};
use gh_core::paths::AppPaths;
use gh_core::traits::OcrProvider;
use ndarray::{Array2, Array3, ArrayView2, ArrayView3, s};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map as JsonMap, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

mod view_registration;
pub use view_registration::{ViewHint, ViewOptions, ViewRegistration};

pub const PRIMARY_LABELS: [i32; 3] = [2, 3, 154];
pub const WORLD_ANCHOR_Y_OFFSET: f64 = -8.5;
pub const WORLD_ANCHOR_MAP_IDS: [i32; 4] = [2, 9, 34, 36];
pub const ARROW_PIVOT_OFFSET_PX_1440: f64 = 2.78;
pub const DEFAULT_ICON_ANCHOR_OFFSET_PX_1440: (f64, f64) = (0.0, 5.0);

pub const DEFAULT_AREA_NAMES: &[(i32, &str)] = &[
    (1, "Mondstadt"),
    (2, "Liyue"),
    (3, "Inazuma"),
    (4, "Sumeru"),
    (8, "Fontaine"),
    (11, "Natlan"),
    (13, "Nod-Krai"),
    (16, "Snezhnaya"),
];

const SUBMAP_KEYWORDS: &[(i32, &[&str])] = &[
    (9, &["khu mo ngam", "vuc da sau", "mo ngam", "chasm"]),
    (7, &["enkanomiya"]),
    (34, &["ky nguyen cu", "bien ky", "remiuria", "remuria"]),
    (
        36,
        &[
            "thanh vien co",
            "nui thanh vien",
            "nui thanh",
            "vien co",
            "te dan tu lua",
        ],
    ),
    (37, &["bien lang vo chu", "bien lang"]),
    (
        40,
        &[
            "suong nguyet",
            "frost moon",
            "cao nguyen trang",
            "nguyen trang",
            "moon plateau",
            "bien trang dat dao",
            "bien trang",
            "ien trang",
            "trang dat",
            "trang det",
            "dat dao",
            "det dao",
            "vung toi cua mat trang",
            "vung toi",
            "toi cua",
            "cua mat trang",
            "cua mot trang",
            "mat trang",
            "mot trang",
            "dark side of the moon",
            "far side of the moon",
            "ungien",
            "nuur",
            "ibni",
            "belum",
            "va cham lu khach",
        ],
    ),
];

const AREA_KEYWORDS: &[(i32, &[&str])] = &[
    (
        8,
        &[
            "beryl",
            "belleau",
            "fontaine",
            "fontam",
            "fontain",
            "dai sanh",
            "dei sanh",
            "sanh font",
            "court",
            "merusea",
            "morte",
            "khu more",
            "erinnyes",
            "nostoi",
            "romaritime",
            "fleuve",
            "marcotte",
            "opera",
        ],
    ),
    (
        4,
        &[
            "sumeru",
            "lokapala",
            "ardravi",
            "ashavan",
            "avidya",
            "vanarana",
            "hadramaveth",
            "sa mac",
            "vissudha",
            "desert",
            "bien cat",
            "tru cot",
            "xoi mon",
            "gio lang",
        ],
    ),
    (
        2,
        &[
            "liyue",
            "tuyet van",
            "khanh van",
            "dao quang",
            "quy ly",
            "tram ngoc",
            "thuong coc",
            "man lam",
            "ngoai o li sa",
            "li sa",
            "chenyu",
            "quynh co",
            "song quynh",
        ],
    ),
    (
        1,
        &[
            "mondstadt",
            "phong khoi",
            "tuyet son",
            "long tich",
            "dragonspine",
            "phong tieu",
            "doi phong",
            "falcon",
        ],
    ),
    (
        3,
        &[
            "inazuma",
            "narukami",
            "kannazuka",
            "yashiori",
            "watatsumi",
            "seirai",
            "tsurumi",
        ],
    ),
    (
        11,
        &[
            "natlan",
            "luu tuyen",
            "to tro tan",
            "hat lua",
            "son dat",
            "coatepec",
            "tepeacac",
            "ven hoa",
            "long chao",
            "chao van hoa",
            "quahuacan",
            "tequemecan",
            "quenepa",
            "malipo",
            "luu huynh",
            "sulfur",
            "tollan",
            "atocpan",
            "ochkanatlan",
            "tieng vang",
        ],
    ),
    (
        13,
        &[
            "nod-krai",
            "nod krai",
            "lempe",
            "lempo",
            "hiisi",
            "paha",
            "tro phu",
            "nodkrai",
        ],
    ),
    (
        16,
        &[
            "snezhnaya",
            "dat bang",
            "bang vinh cuu",
            "vinh cuu",
            "bang vinh",
            "rankova",
            "zorya",
            "bach duong tuyet",
            "bach duong",
            "bech du",
            "lang mo",
            "chan bang",
            "long vu lua",
            "volkodlak",
            "dinh bang",
            "vuon vo uu",
            "novokitezhgrad",
            "novokjtezhgrad",
            "novokitezh",
            "belovod",
            "kitezh",
            "sai canh",
            "dinh sai canh",
            "xuyen sao",
        ],
    ),
];

pub const fn icon_anchor_offset_px_1440(label_id: i32) -> (f64, f64) {
    match label_id {
        2 => (0.0, 3.5),
        3 => (0.0, 5.0),
        154 => (2.5, 0.0),
        _ => DEFAULT_ICON_ANCHOR_OFFSET_PX_1440,
    }
}

pub fn icon_anchor_offset_px(label_id: i32, scale_ui: f64) -> (f64, f64) {
    let (ox, oy) = icon_anchor_offset_px_1440(label_id);
    (ox * scale_ui, oy * scale_ui)
}

const fn map_base_scale_cfg(map_id: i32) -> f64 {
    match map_id {
        2 => 1.12,
        7 => 0.49,
        9 => 0.75,
        34 => 0.60,
        36 => 0.80,
        37 => 0.60,
        40 => 0.62,
        _ => 0.75,
    }
}

#[derive(Debug, Clone)]
pub struct TerrainRaster {
    pub gray: Array2<u8>,
    /// `[tx0, ty0, z, orig_x, orig_y]`
    pub meta: [f64; 5],
}

#[derive(Debug, Clone, Default)]
pub struct LocatorPaths {
    pub db_path: Option<PathBuf>,
    pub terrain_npz_path: Option<PathBuf>,
    pub cave_masks_npz_path: Option<PathBuf>,
    pub ui_anchors_npz_path: Option<PathBuf>,
    pub legacy_underground_db_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerInfo {
    pub has_layers: bool,
    pub is_underground: bool,
    pub total_floors: usize,
    pub active_floor_idx: usize,
    pub surface_idx: usize,
    pub floor_name: String,
    pub slot_types: Vec<String>,
}

impl Default for LayerInfo {
    fn default() -> Self {
        Self {
            has_layers: false,
            is_underground: false,
            total_floors: 1,
            active_floor_idx: 0,
            surface_idx: 0,
            floor_name: "G".to_owned(),
            slot_types: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocateCandidateDebug {
    pub x: f64,
    pub y: f64,
    pub r: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocateInlierDebug {
    pub pixel_x: f64,
    pub pixel_y: f64,
    pub waypoint_id: i64,
    pub world_x: f64,
    pub world_y: f64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LocateResult {
    pub detected: bool,
    pub has_world_coords: bool,
    pub map_id: Option<i32>,
    pub map_name: Option<String>,
    pub area_id: Option<i32>,
    pub area_name: Option<String>,
    pub world_x: Option<f64>,
    pub world_y: Option<f64>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub pixel_x: f64,
    pub pixel_y: f64,
    pub heading: f64,
    pub scale: Option<f64>,
    pub inliers_count: Option<usize>,
    pub rmse: Option<f64>,
    pub is_underground: bool,
    pub in_game_is_underground: Option<bool>,
    pub floor_idx: usize,
    pub total_floors: usize,
    pub active_floor_idx: usize,
    pub surface_idx: usize,
    pub floor_name: String,
    pub floor_short: String,
    pub hoyolab_floor_name: Option<String>,
    pub group_id: Option<i32>,
    pub group_name: Option<String>,
    pub floor_id: Option<i32>,
    pub has_layers: bool,
    pub zoom_ratio_t: Option<f64>,
    // Debug-only fields
    pub candidates: Option<Vec<LocateCandidateDebug>>,
    pub inliers: Option<Vec<LocateInlierDebug>>,
    pub slider_res: Option<ZoomSliderResult>,
    pub region_text: Option<String>,
    pub target_map_id: Option<i32>,
    pub target_area_id: Option<i32>,
    pub slot_types: Option<Vec<String>>,
    pub reason: Option<String>,
    #[serde(skip)]
    pub debug_included: bool,
}

impl LocateResult {
    /// Emits wire-exact JSON matching `src/locator.py` (`REQUIRED_LOCATE_KEYS` + debug fields).
    pub fn to_json_value(&self) -> Value {
        let mut obj = JsonMap::new();
        obj.insert("detected".into(), json!(self.detected));
        obj.insert("has_world_coords".into(), json!(self.has_world_coords));

        if !self.detected {
            if let Some(reason) = &self.reason {
                obj.insert("reason".into(), json!(reason));
            }
            return Value::Object(obj);
        }

        if self.has_world_coords {
            obj.insert("map_id".into(), json!(self.map_id));
            obj.insert("map_name".into(), json!(self.map_name));
            obj.insert("area_id".into(), json!(self.area_id));
            obj.insert("area_name".into(), json!(self.area_name));
            obj.insert("world_x".into(), json!(self.world_x));
            obj.insert("world_y".into(), json!(self.world_y));
            obj.insert("lat".into(), json!(self.lat));
            obj.insert("lng".into(), json!(self.lng));
        }

        obj.insert("pixel_x".into(), json!(self.pixel_x));
        obj.insert("pixel_y".into(), json!(self.pixel_y));
        obj.insert("heading".into(), json!(self.heading));

        if self.has_world_coords {
            obj.insert("scale".into(), json!(self.scale));
            obj.insert("inliers_count".into(), json!(self.inliers_count));
            obj.insert("rmse".into(), json!(self.rmse));
        }

        obj.insert("is_underground".into(), json!(self.is_underground));
        if let Some(ig) = self.in_game_is_underground {
            obj.insert("in_game_is_underground".into(), json!(ig));
        }
        obj.insert("floor_idx".into(), json!(self.floor_idx));
        obj.insert("total_floors".into(), json!(self.total_floors));
        obj.insert("active_floor_idx".into(), json!(self.active_floor_idx));
        obj.insert("surface_idx".into(), json!(self.surface_idx));
        obj.insert("floor_name".into(), json!(self.floor_name));
        obj.insert("floor_short".into(), json!(self.floor_short));

        if self.has_world_coords {
            obj.insert("hoyolab_floor_name".into(), json!(self.hoyolab_floor_name));
            obj.insert("group_id".into(), json!(self.group_id));
            obj.insert("group_name".into(), json!(self.group_name));
            obj.insert("floor_id".into(), json!(self.floor_id));
        }

        obj.insert("has_layers".into(), json!(self.has_layers));
        obj.insert("zoom_ratio_t".into(), json!(self.zoom_ratio_t));

        if self.debug_included {
            if let Some(cands) = &self.candidates {
                obj.insert("candidates".into(), json!(cands));
            }
            if let Some(inliers) = &self.inliers {
                obj.insert("inliers".into(), json!(inliers));
            }
            obj.insert("slider_res".into(), json!(self.slider_res));
            if let Some(rt) = &self.region_text {
                obj.insert("region_text".into(), json!(rt));
            }
            if self.has_world_coords || self.inliers.is_some() || self.region_text.is_some() {
                obj.insert("target_map_id".into(), json!(self.target_map_id));
                obj.insert("target_area_id".into(), json!(self.target_area_id));
                if let Some(st) = &self.slot_types {
                    obj.insert("slot_types".into(), json!(st));
                }
            }
            if let Some(reason) = &self.reason {
                obj.insert("reason".into(), json!(reason));
            }
        }

        Value::Object(obj)
    }
}

impl Serialize for LocateResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json_value().serialize(serializer)
    }
}

type InlierPair<'a> = ((f64, f64, i32), &'a WaypointEntry);
type MatchResult<'a> = (Vec<InlierPair<'a>>, f64, [f64; 2], f64);
type Hyp2p<'a> = (f64, Vec<InlierPair<'a>>, f64, [f64; 2]);

#[derive(Debug, Clone)]
struct MapSpatialIndex {
    n_wps: usize,
    nb_id_sets: Vec<HashSet<usize>>,
    angles: Vec<f64>,
    dists: Vec<f64>,
    w1_dense: Vec<usize>,
    w2_dense: Vec<usize>,
    orig_order: Vec<usize>,
    grid: HashMap<(i32, i32), Vec<usize>>,
    grid_cell: f64,
}

#[derive(Debug, Clone)]
pub struct CaveBitMask {
    h: usize,
    w: usize,
    bits: Vec<u64>,
}

impl CaveBitMask {
    pub fn from_u8_slice(h: usize, w: usize, data: &[u8]) -> Self {
        let n = h.saturating_mul(w);
        let mut bits = vec![0u64; n.div_ceil(64)];
        for (i, &v) in data.iter().take(n).enumerate() {
            if v > 0 {
                bits[i >> 6] |= 1u64 << (i & 63);
            }
        }
        Self { h, w, bits }
    }

    #[inline]
    pub const fn dim(&self) -> (usize, usize) {
        (self.h, self.w)
    }

    #[inline]
    pub fn is_nonzero(&self, y: usize, x: usize) -> bool {
        if y >= self.h || x >= self.w {
            return false;
        }
        let idx = y * self.w + x;
        (self.bits[idx >> 6] & (1u64 << (idx & 63))) != 0
    }

    #[inline]
    pub fn any_nonzero_in_rect(&self, y0: usize, y1: usize, x0: usize, x1: usize) -> bool {
        let ye = y1.min(self.h);
        let xe = x1.min(self.w);
        if y0 >= ye || x0 >= xe {
            return false;
        }
        for y in y0..ye {
            for x in x0..xe {
                if self.is_nonzero(y, x) {
                    return true;
                }
            }
        }
        false
    }
}

#[derive(Debug, Clone)]
struct FloorResolveResult {
    is_hoyolab_underground: bool,
    group_id: Option<i32>,
    group_name: Option<String>,
    floor_id: Option<i32>,
    floor_short: String,
    floor_name: String,
    hoyolab_floor_name: String,
}

impl Default for FloorResolveResult {
    fn default() -> Self {
        Self {
            is_hoyolab_underground: false,
            group_id: None,
            group_name: None,
            floor_id: None,
            floor_short: "G".to_owned(),
            floor_name: "G".to_owned(),
            hoyolab_floor_name: "G".to_owned(),
        }
    }
}

#[derive(Debug, Default)]
struct TrackingState {
    last_known_pos: Option<(f64, f64)>,
    last_map_id: Option<i32>,
}

pub struct MapLocator {
    db: Arc<MultimapDb>,
    map_order: Vec<i32>,
    area_names_map: BTreeMap<i32, String>,
    area_bboxes: HashMap<(i32, Option<i32>), [f64; 4]>,
    edge_indices: HashMap<i32, MapSpatialIndex>,
    underground_groups: Vec<PointGroupEntry>,
    cave_masks: HashMap<String, CaveBitMask>,
    terrain_data: BTreeMap<i32, Arc<TerrainRaster>>,
    terrain_npz_path: Option<PathBuf>,
    lazy_terrain_map_ids: HashSet<i32>,
    lazy_terrain_slot: Mutex<Option<(i32, Arc<TerrainRaster>)>>,
    lazy_terrain_disabled: bool,
    ui_anchors: HashMap<String, Array3<u8>>,
    elev_tpl_cache: Mutex<HashMap<usize, [Array3<u8>; 4]>>,
    floor_label_index: Mutex<HashMap<(i32, String), String>>,
    ocr: Option<Arc<dyn OcrProvider>>,
    tracking: Mutex<TrackingState>,
}

fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

fn fold_latin_char(c: char) -> char {
    match c {
        'à' | 'á' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ằ' | 'ắ' | 'ẳ' | 'ẵ' | 'ặ' | 'â' | 'ầ' | 'ấ'
        | 'ẩ' | 'ẫ' | 'ậ' | 'ä' | 'å' | 'ā' | 'ą' | 'À' | 'Á' | 'Ả' | 'Ã' | 'Ạ' | 'Ă' | 'Ằ'
        | 'Ắ' | 'Ẳ' | 'Ẵ' | 'Ặ' | 'Â' | 'Ầ' | 'Ấ' | 'Ẩ' | 'Ẫ' | 'Ậ' | 'Ä' | 'Å' | 'Ā' | 'Ą' => {
            'a'
        }
        'è' | 'é' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ề' | 'ế' | 'ể' | 'ễ' | 'ệ' | 'ë' | 'ē' | 'ĕ'
        | 'ė' | 'ę' | 'ě' | 'È' | 'É' | 'Ẻ' | 'Ẽ' | 'Ẹ' | 'Ê' | 'Ề' | 'Ế' | 'Ể' | 'Ễ' | 'Ệ'
        | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => 'e',
        'ì' | 'í' | 'ỉ' | 'ĩ' | 'ị' | 'ï' | 'î' | 'ī' | 'ĭ' | 'į' | 'ı' | 'Ì' | 'Í' | 'Ỉ'
        | 'Ĩ' | 'Ị' | 'Ï' | 'Î' | 'Ī' | 'Ĭ' | 'Į' | 'İ' => 'i',
        'ò' | 'ó' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ồ' | 'ố' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ờ' | 'ớ'
        | 'ở' | 'ỡ' | 'ợ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'Ò' | 'Ó' | 'Ỏ' | 'Õ' | 'Ọ' | 'Ô'
        | 'Ồ' | 'Ố' | 'Ổ' | 'Ỗ' | 'Ộ' | 'Ơ' | 'Ờ' | 'Ớ' | 'Ở' | 'Ỡ' | 'Ợ' | 'Ö' | 'Ø' | 'Ō'
        | 'Ŏ' | 'Ő' => 'o',
        'ù' | 'ú' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ừ' | 'ứ' | 'ử' | 'ữ' | 'ự' | 'ü' | 'û' | 'ū'
        | 'ŭ' | 'ů' | 'ű' | 'ų' | 'Ù' | 'Ú' | 'Ủ' | 'Ũ' | 'Ụ' | 'Ư' | 'Ừ' | 'Ứ' | 'Ử' | 'Ữ'
        | 'Ự' | 'Ü' | 'Û' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => 'u',
        'ý' | 'ỳ' | 'ỷ' | 'ỹ' | 'ỵ' | 'ÿ' | 'ŷ' | 'Ý' | 'Ỳ' | 'Ỷ' | 'Ỹ' | 'Ỵ' | 'Ÿ' | 'Ŷ' => {
            'y'
        }
        'đ' | 'Đ' | 'ð' | 'Ð' => 'd',
        'ñ' | 'ń' | 'ņ' | 'ň' | 'Ñ' | 'Ń' | 'Ņ' | 'Ň' => 'n',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' | 'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => 'c',
        'ś' | 'ŝ' | 'ş' | 'š' | 'Ś' | 'Ŝ' | 'Ş' | 'Š' => 's',
        'ź' | 'ż' | 'ž' | 'Ź' | 'Ż' | 'Ž' => 'z',
        other => other.to_ascii_lowercase(),
    }
}

/// Normalizes OCR region strings matching `src/locator.py::_normalize_region_text`:
/// strips diacritics, preserves spaces (mapping non-alnum/whitespace to `' '`),
/// applies `"ciru" -> "cuu"`, `"cir" -> "cuu"`, `"cb" -> "co"`, and leaves `"me"` intact.
pub fn normalize_region_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        if is_combining_mark(c) {
            continue;
        }
        folded.push(fold_latin_char(c));
    }
    let replaced = folded
        .replace("ciru", "cuu")
        .replace("cir", "cuu")
        .replace("cb", "co");

    let mut out = String::with_capacity(replaced.len());
    for c in replaced.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_whitespace() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out
}

fn percentile_linear_u8(sorted_vals: &[u8], pct: f64) -> f64 {
    if sorted_vals.is_empty() {
        return 0.0;
    }
    let pos = (pct / 100.0) * (sorted_vals.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    sorted_vals[lo] as f64 * (1.0 - frac) + sorted_vals[hi] as f64 * frac
}

fn build_spatial_index(waypoints: &[WaypointEntry]) -> MapSpatialIndex {
    let n = waypoints.len();
    let grid_cell = 32.0f64;
    let inv_cell = 1.0 / grid_cell;
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let mut nb_id_sets: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    let mut raw_angles = Vec::new();
    let mut raw_dists = Vec::new();
    let mut raw_w1_dense = Vec::new();
    let mut raw_w2_dense = Vec::new();

    for (i, w1) in waypoints.iter().enumerate() {
        let gx = (w1.x_pos * inv_cell).floor() as i32;
        let gy = (w1.y_pos * inv_cell).floor() as i32;
        grid.entry((gx, gy)).or_default().push(i);

        let w1_major = w1.label_id == 2 || w1.label_id == 154;
        for (j, w2) in waypoints.iter().enumerate() {
            if i == j {
                continue;
            }
            let dw_x = w2.x_pos - w1.x_pos;
            let dw_y = w2.y_pos - w1.y_pos;
            let dist = dw_x.hypot(dw_y);
            let max_d = if w1_major && (w2.label_id == 2 || w2.label_id == 154) {
                3600.0
            } else {
                2500.0
            };
            if (35.0..=max_d).contains(&dist) {
                nb_id_sets[i].insert(j);
                raw_angles.push(dw_y.atan2(dw_x));
                raw_dists.push(dist);
                raw_w1_dense.push(i);
                raw_w2_dense.push(j);
            }
        }
    }

    let m = raw_angles.len();
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&a, &b| raw_angles[a].total_cmp(&raw_angles[b]).then_with(|| a.cmp(&b)));

    let mut angles = Vec::with_capacity(m);
    let mut dists = Vec::with_capacity(m);
    let mut w1_dense = Vec::with_capacity(m);
    let mut w2_dense = Vec::with_capacity(m);
    let mut orig_order = Vec::with_capacity(m);
    for idx in order {
        angles.push(raw_angles[idx]);
        dists.push(raw_dists[idx]);
        w1_dense.push(raw_w1_dense[idx]);
        w2_dense.push(raw_w2_dense[idx]);
        orig_order.push(idx);
    }

    MapSpatialIndex {
        n_wps: n,
        nb_id_sets,
        angles,
        dists,
        w1_dense,
        w2_dense,
        orig_order,
        grid,
        grid_cell,
    }
}

fn query_edge_index(
    edge_idx: &MapSpatialIndex,
    w1_allowed: &[bool],
    angle_s: f64,
    ang_tol: f64,
    min_w_dist: f64,
    max_w_dist: f64,
) -> Vec<usize> {
    let angles = &edge_idx.angles;
    if angles.is_empty() {
        return Vec::new();
    }
    let eps = 1e-9;
    let a_lo = angle_s - ang_tol - eps;
    let a_hi = angle_s + ang_tol + eps;
    let two_pi = 2.0 * std::f64::consts::PI;
    let mut slices: Vec<(usize, usize)> = Vec::new();

    let p_lo = (-std::f64::consts::PI).max(a_lo);
    let p_hi = std::f64::consts::PI.min(a_hi);
    if p_lo <= p_hi {
        let lo = angles.partition_point(|&a| a < p_lo);
        let hi = angles.partition_point(|&a| a <= p_hi);
        if hi > lo {
            slices.push((lo, hi));
        }
    }
    if a_lo < -std::f64::consts::PI {
        let w_lo = a_lo + two_pi;
        let lo = angles.partition_point(|&a| a < w_lo);
        if angles.len() > lo {
            slices.push((lo, angles.len()));
        }
    }
    if a_hi > std::f64::consts::PI {
        let w_hi = a_hi - two_pi;
        let hi = angles.partition_point(|&a| a <= w_hi);
        if hi > 0 {
            slices.push((0, hi));
        }
    }

    let mut hits = Vec::new();
    for (lo, hi) in slices {
        for idx in lo..hi {
            let d = edge_idx.dists[idx];
            if d >= min_w_dist && d <= max_w_dist && w1_allowed[edge_idx.w1_dense[idx]] {
                hits.push(idx);
            }
        }
    }
    if hits.len() > 1 {
        hits.sort_by_key(|&idx| edge_idx.orig_order[idx]);
        hits.dedup();
    }
    hits
}

fn into_u8_2d(arr: NpyArray) -> Option<Array2<u8>> {
    let sh = arr.shape();
    if sh.len() != 2 || sh[0] == 0 || sh[1] == 0 {
        return None;
    }
    let (h, w) = (sh[0], sh[1]);
    let NpyData::U8(data) = arr.data else {
        return None;
    };
    Array2::from_shape_vec((h, w), data).ok()
}

fn load_u8_3d(archive: &NpzArchive, key: &str) -> Option<Array3<u8>> {
    let arr = archive.get(key)?;
    let sh = arr.shape();
    if sh.len() != 3 || sh[0] == 0 || sh[1] == 0 || sh[2] < 3 {
        return None;
    }
    let data = arr.as_u8().ok()?.to_vec();
    let full = Array3::from_shape_vec((sh[0], sh[1], sh[2]), data).ok()?;
    if sh[2] == 3 {
        Some(full)
    } else {
        Some(full.slice(s![.., .., 0..3]).to_owned())
    }
}

fn npy_to_terrain_meta(arr: &NpyArray) -> Option<[f64; 5]> {
    let vals: Vec<f64> = match &arr.data {
        NpyData::I64(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::I32(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::F32(v) => v.iter().map(|&x| x as f64).collect(),
        NpyData::U8(v) => v.iter().map(|&x| x as f64).collect(),
    };
    if vals.len() < 5 {
        return None;
    }
    Some([vals[0], vals[1], vals[2], vals[3], vals[4]])
}

fn load_single_terrain_raster_from_file(
    path: &std::path::Path,
    map_id: i32,
) -> Option<Arc<TerrainRaster>> {
    let gray_key = format!("map_{map_id}_gray");
    let meta_key = format!("map_{map_id}_meta");
    let gray_arr = NpzArchive::read_single_from_file(path, &gray_key).ok()?;
    let meta_arr = NpzArchive::read_single_from_file(path, &meta_key).ok()?;
    let gray = into_u8_2d(gray_arr)?;
    let meta = npy_to_terrain_meta(&meta_arr)?;
    Some(Arc::new(TerrainRaster { gray, meta }))
}

fn array_std_u8(arr: ArrayView2<'_, u8>) -> f64 {
    let n = arr.len();
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0u64;
    let mut sum_sq = 0u64;
    for &v in arr.iter() {
        let vu = v as u64;
        sum += vu;
        sum_sq += vu * vu;
    }
    let nf = n as f64;
    let mean = sum as f64 / nf;
    ((sum_sq as f64 / nf) - mean * mean).max(0.0).sqrt()
}

fn default_db_candidates() -> Vec<PathBuf> {
    let mut out = vec![resolve_data_asset("multimap_v4.json")];
    let paths = AppPaths::resolve();
    if let Some(root) = paths.dev_root() {
        out.push(root.join("data").join("multimap_v4.json"));
        out.push(root.join("data").join("waypoints_db.json"));
        out.push(root.join("scratch").join("waypoints.json"));
    }
    out
}

fn default_legacy_ug_path() -> Option<PathBuf> {
    AppPaths::resolve()
        .dev_root()
        .map(|root| root.join("data").join("underground_db.json"))
}

impl MapLocator {
    pub fn new(ocr: Option<Arc<dyn OcrProvider>>) -> Self {
        Self::with_paths(LocatorPaths::default(), ocr)
    }

    pub fn with_paths(paths: LocatorPaths, ocr: Option<Arc<dyn OcrProvider>>) -> Self {
        let mut area_names_map: BTreeMap<i32, String> = DEFAULT_AREA_NAMES
            .iter()
            .map(|&(id, name)| (id, (*name).to_owned()))
            .collect();

        // Optional legacy underground_db.json
        let mut legacy_ug_groups: Vec<PointGroupEntry> = Vec::new();
        let mut legacy_shorts: HashMap<(i32, i32), String> = HashMap::new();
        let ug_path = paths
            .legacy_underground_db_path
            .or_else(default_legacy_ug_path);
        if let Some(p) = ug_path
            && p.is_file()
            && let Ok(raw) = std::fs::read_to_string(&p)
            && let Ok(groups) = serde_json::from_str::<Vec<PointGroupEntry>>(&raw)
        {
            for g in &groups {
                for fl in &g.floors {
                    let fs = fl.floor_name_short.trim().to_ascii_uppercase();
                    if g.id != 0 && fl.id != 0 && !fs.is_empty() {
                        legacy_shorts.insert((g.id, fl.id), fs);
                    }
                }
            }
            legacy_ug_groups = groups;
        }

        // Cave alpha masks (bit-packed 1 bit/pixel)
        let mut cave_masks = HashMap::new();
        let masks_path = paths
            .cave_masks_npz_path
            .unwrap_or_else(|| resolve_data_asset("cave_masks_v4.npz"));
        if masks_path.is_file()
            && let Ok(archive) = NpzArchive::read_file(&masks_path)
        {
            for k in archive.keys() {
                if let Some(arr) = archive.get(k) {
                    let sh = arr.shape();
                    if sh.len() == 2
                        && sh[0] > 0
                        && sh[1] > 0
                        && let Ok(data) = arr.as_u8()
                    {
                        cave_masks
                            .insert(k.to_owned(), CaveBitMask::from_u8_slice(sh[0], sh[1], data));
                    }
                }
            }
        }

        // Terrain rasters: eagerly load only map 2 (Teyvat), index sub-maps lazily via NPZ keys.
        let mut terrain_data = BTreeMap::new();
        let mut lazy_terrain_map_ids = HashSet::new();
        let terrain_path = paths
            .terrain_npz_path
            .unwrap_or_else(|| resolve_data_asset("terrain_n2_v4.npz"));
        let terrain_npz_path = if terrain_path.is_file() {
            if let Ok(keys) = NpzArchive::list_keys_from_file(&terrain_path) {
                let key_set: HashSet<&str> = keys.iter().map(String::as_str).collect();
                for key in &keys {
                    if let Some(mid_str) =
                        key.strip_prefix("map_").and_then(|s| s.strip_suffix("_gray"))
                        && let Ok(mid) = mid_str.parse::<i32>()
                        && key_set.contains(format!("map_{mid}_meta").as_str())
                    {
                        lazy_terrain_map_ids.insert(mid);
                    }
                }
                if lazy_terrain_map_ids.contains(&2)
                    && let Some(raster) = load_single_terrain_raster_from_file(&terrain_path, 2)
                {
                    terrain_data.insert(2, raster);
                }
            }
            Some(terrain_path)
        } else {
            None
        };

        // Optional UI anchor templates
        let mut ui_anchors = HashMap::new();
        let anchors_path = paths
            .ui_anchors_npz_path
            .unwrap_or_else(|| resolve_data_asset("ui_anchors_v4.npz"));
        if anchors_path.is_file()
            && let Ok(archive) = NpzArchive::read_file(&anchors_path)
        {
            for k in ["tpl_ma", "tpl_mi", "tpl_la", "tpl_li"] {
                if let Some(arr) = load_u8_3d(&archive, k) {
                    ui_anchors.insert(k.to_owned(), arr);
                }
            }
        }

        // Multimap DB (or legacy list JSON fallback)
        let db_path = paths.db_path.unwrap_or_else(|| {
            let cands = default_db_candidates();
            cands
                .iter()
                .find(|p| p.is_file())
                .cloned()
                .unwrap_or_else(|| cands.into_iter().next().unwrap())
        });

        let mut db = MultimapDb::default();
        let mut map_order = Vec::new();
        let mut floor_label_index = HashMap::new();
        let mut all_pgs = Vec::new();

        if db_path.is_file()
            && let Ok(raw) = std::fs::read_to_string(&db_path)
            && let Ok(val) = serde_json::from_str::<Value>(&raw)
        {
            if let Some(obj) = val.as_object()
                && obj.contains_key("maps")
            {
                if let Ok(parsed) = MultimapDb::from_json_str(&raw) {
                    db = parsed;
                    if let Some(maps_obj) = obj.get("maps").and_then(Value::as_object) {
                        for k in maps_obj.keys() {
                            if let Ok(mid) = k.trim().parse::<i32>()
                                && db.maps.contains_key(&mid)
                            {
                                map_order.push(mid);
                            }
                        }
                    }
                }
            } else if val.is_array()
                && let Ok(wps) = serde_json::from_value::<Vec<WaypointEntry>>(val)
            {
                let mut m2 = MapMeta {
                    id: 2,
                    name: "Bản Đồ Tương Tác Teyvat".to_owned(),
                    priority: 1,
                    waypoints: wps,
                    ..MapMeta::default()
                };
                for w in &mut m2.waypoints {
                    if w.map_id == 0 {
                        w.map_id = 2;
                    }
                }
                db.maps.insert(2, m2);
                map_order.push(2);
            }
        }

        for &mid in db.maps.keys() {
            if !map_order.contains(&mid) {
                map_order.push(mid);
            }
        }

        let mut area_bboxes = HashMap::new();
        let mut edge_indices = HashMap::new();

        for (&mid, meta) in &mut db.maps {
            for ar in &meta.areas {
                let aname = ar.name.trim();
                if ar.id != 0
                    && !aname.is_empty()
                    && (mid == 2 || !area_names_map.contains_key(&ar.id))
                {
                    area_names_map
                        .entry(ar.id)
                        .or_insert_with(|| aname.to_owned());
                }
            }

            for g in &mut meta.point_groups {
                for (f_idx, fl) in g.floors.iter_mut().enumerate() {
                    let mut fs = fl.floor_name_short.trim().to_ascii_uppercase();
                    if fs.is_empty()
                        && let Some(ls) = legacy_shorts.get(&(g.id, fl.id))
                    {
                        fs = ls.clone();
                    }
                    fl.floor_name_short = fs.clone();
                    let fn_clean = fl.floor_name.trim();
                    if fl.id != 0 {
                        let lbl = if !fs.is_empty() {
                            fs
                        } else if !fn_clean.is_empty() {
                            fn_clean.to_owned()
                        } else {
                            format!("B{}", f_idx + 1)
                        };
                        floor_label_index.insert((mid, fl.id.to_string()), lbl);
                    }
                }
            }
            all_pgs.extend(meta.point_groups.clone());

            // Register bboxes
            if !meta.waypoints.is_empty() {
                let mut min_x = f64::INFINITY;
                let mut max_x = f64::NEG_INFINITY;
                let mut min_y = f64::INFINITY;
                let mut max_y = f64::NEG_INFINITY;
                let mut by_area: HashMap<i32, [f64; 4]> = HashMap::new();
                for w in &meta.waypoints {
                    min_x = min_x.min(w.x_pos);
                    max_x = max_x.max(w.x_pos);
                    min_y = min_y.min(w.y_pos);
                    max_y = max_y.max(w.y_pos);
                    if w.area_id != 0 {
                        by_area
                            .entry(w.area_id)
                            .and_modify(|bb| {
                                bb[0] = bb[0].min(w.x_pos);
                                bb[1] = bb[1].max(w.x_pos);
                                bb[2] = bb[2].min(w.y_pos);
                                bb[3] = bb[3].max(w.y_pos);
                            })
                            .or_insert([w.x_pos, w.x_pos, w.y_pos, w.y_pos]);
                    }
                }
                area_bboxes.insert((mid, None), [min_x, max_x, min_y, max_y]);
                for (aid, bb) in by_area {
                    area_bboxes.insert((mid, Some(aid)), bb);
                }
            }

            edge_indices.insert(mid, build_spatial_index(&meta.waypoints));
        }

        let underground_groups = if all_pgs.is_empty() {
            legacy_ug_groups
        } else {
            all_pgs
        };

        Self {
            db: Arc::new(db),
            map_order,
            area_names_map,
            area_bboxes,
            edge_indices,
            underground_groups,
            cave_masks,
            terrain_data,
            terrain_npz_path,
            lazy_terrain_map_ids,
            lazy_terrain_slot: Mutex::new(None),
            lazy_terrain_disabled: false,
            ui_anchors,
            elev_tpl_cache: Mutex::new(HashMap::new()),
            floor_label_index: Mutex::new(floor_label_index),
            ocr,
            tracking: Mutex::new(TrackingState::default()),
        }
    }

    pub fn multimap_db(&self) -> &MultimapDb {
        &self.db
    }

    pub fn multimap_db_arc(&self) -> Arc<MultimapDb> {
        Arc::clone(&self.db)
    }

    pub fn map_meta(&self, map_id: i32) -> Option<&MapMeta> {
        self.db.map(map_id)
    }

    pub fn terrain_data(&self) -> &BTreeMap<i32, Arc<TerrainRaster>> {
        &self.terrain_data
    }

    pub fn terrain_raster(&self, map_id: i32) -> Option<&Arc<TerrainRaster>> {
        self.terrain_data.get(&map_id)
    }

    pub fn has_terrain_for_map(&self, map_id: i32) -> bool {
        self.terrain_data.contains_key(&map_id)
            || (!self.lazy_terrain_disabled && self.lazy_terrain_map_ids.contains(&map_id))
    }

    pub fn terrain_map_count(&self) -> usize {
        if self.lazy_terrain_disabled {
            self.terrain_data.len()
        } else {
            let mut count = self.terrain_data.len();
            for mid in &self.lazy_terrain_map_ids {
                if !self.terrain_data.contains_key(mid) {
                    count += 1;
                }
            }
            count
        }
    }

    pub fn load_terrain_raster(&self, map_id: i32) -> Option<Arc<TerrainRaster>> {
        if let Some(raster) = self.terrain_data.get(&map_id) {
            return Some(Arc::clone(raster));
        }
        if self.lazy_terrain_disabled || !self.lazy_terrain_map_ids.contains(&map_id) {
            return None;
        }
        if let Ok(guard) = self.lazy_terrain_slot.lock()
            && let Some((cached_mid, ref raster)) = *guard
            && cached_mid == map_id
        {
            return Some(Arc::clone(raster));
        }
        let npz_path = self.terrain_npz_path.as_ref()?;
        if !npz_path.is_file() {
            return None;
        }
        let raster = load_single_terrain_raster_from_file(npz_path, map_id)?;
        if let Ok(mut guard) = self.lazy_terrain_slot.lock() {
            *guard = Some((map_id, Arc::clone(&raster)));
        }
        Some(raster)
    }

    pub fn set_terrain_data(&mut self, data: BTreeMap<i32, Arc<TerrainRaster>>) {
        self.terrain_data = data;
        self.lazy_terrain_disabled = self.terrain_data.is_empty();
        if let Ok(mut slot) = self.lazy_terrain_slot.lock() {
            *slot = None;
        }
    }

    pub fn area_names_map(&self) -> &BTreeMap<i32, String> {
        &self.area_names_map
    }

    pub fn cave_masks(&self) -> &HashMap<String, CaveBitMask> {
        &self.cave_masks
    }

    pub fn ui_anchors(&self) -> &HashMap<String, Array3<u8>> {
        &self.ui_anchors
    }

    pub fn has_ocr(&self) -> bool {
        self.ocr.is_some()
    }

    pub fn reset_tracking_state(&self) {
        if let Ok(mut guard) = self.tracking.lock() {
            guard.last_known_pos = None;
            guard.last_map_id = None;
        }
    }

    pub fn last_known_pos(&self) -> Option<(f64, f64)> {
        self.tracking.lock().ok().and_then(|g| g.last_known_pos)
    }

    pub fn last_map_id(&self) -> Option<i32> {
        self.tracking.lock().ok().and_then(|g| g.last_map_id)
    }

    pub fn normalize_region_text(text: &str) -> String {
        normalize_region_text(text)
    }

    pub fn detect_region_name(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
    ) -> (Option<i32>, Option<i32>, String) {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return (None, None, String::new());
        }
        let Some(ocr) = &self.ocr else {
            return (None, None, String::new());
        };
        let y0 = (h as f64 * 0.88) as usize;
        let y1 = (h as f64 * 0.96) as usize;
        let x0 = (w as f64 * 0.72) as usize;
        let x1 = (w as f64 * 0.96) as usize;
        if y1 <= y0 || x1 <= x0 {
            return (None, None, String::new());
        }
        let roi = frame_bgr.slice(s![y0..y1, x0..x1, 0..3]);
        let (rh, rw, _) = roi.dim();
        let mut buf = FrameBuf::new_zeroed(rw as u32, rh as u32, PixelFormat::Bgr8);
        for y in 0..rh {
            for x in 0..rw {
                let off = y * buf.stride + x * 3;
                buf.data[off] = roi[[y, x, 0]];
                buf.data[off + 1] = roi[[y, x, 1]];
                buf.data[off + 2] = roi[[y, x, 2]];
            }
        }
        let raw_text = match ocr.recognize_text(buf.as_view()) {
            Ok(s) => s,
            Err(_) => return (None, None, String::new()),
        };
        if raw_text.is_empty() {
            return (None, None, String::new());
        }
        let norm = normalize_region_text(&raw_text);
        for &(mid, kws) in SUBMAP_KEYWORDS {
            if kws.iter().any(|k| norm.contains(k)) {
                return (Some(mid), None, raw_text);
            }
        }
        for &(aid, kws) in AREA_KEYWORDS {
            if kws.iter().any(|k| norm.contains(k)) {
                return (Some(2), Some(aid), raw_text);
            }
        }
        (None, None, raw_text)
    }

    pub fn check_underground_alignment(
        &self,
        world_x: f64,
        world_y: f64,
        floor_name: Option<&str>,
        is_underground: bool,
    ) -> f64 {
        if !is_underground || self.underground_groups.is_empty() {
            return 0.0;
        }
        let clean_floor = floor_name.unwrap_or("").trim().to_ascii_uppercase();
        for g in &self.underground_groups {
            for fl in &g.floors {
                let Some(ov) = &fl.overlay else {
                    continue;
                };
                let (min_lng, max_lng, min_lat, max_lat) = ov.bounds();
                if (min_lng - 100.0..=max_lng + 100.0).contains(&world_x)
                    && (min_lat - 100.0..=max_lat + 100.0).contains(&world_y)
                {
                    let fs = fl.floor_name_short.trim().to_ascii_uppercase();
                    let fn_upper = fl.floor_name.to_ascii_uppercase();
                    if !clean_floor.is_empty()
                        && (fs == clean_floor || fn_upper.contains(&clean_floor))
                    {
                        return 800.0;
                    }
                    return 300.0;
                }
            }
        }
        0.0
    }

    pub fn detect_player_arrow(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        hsv_opt: Option<ArrayView3<'_, u8>>,
    ) -> Option<(f64, f64, f64)> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return None;
        }
        let hsv_owned;
        let hsv = match hsv_opt {
            Some(v) => v,
            None => {
                let bgr3 = if c > 3 {
                    frame_bgr.slice(s![.., .., 0..3])
                } else {
                    frame_bgr
                };
                hsv_owned = bgr_to_hsv(bgr3).ok()?;
                hsv_owned.view()
            }
        };

        let hsv_cow = hsv.as_standard_layout();
        let hsv_s = hsv_cow.as_slice()?;
        let su = h as f64 / 1440.0;
        let sf = su * su;
        let min_area = 550.0 * sf;
        let max_area = 1650.0 * sf;
        let min_dim = 16.0 * su;
        let max_dim = 56.0 * su;

        let contours = WP_SCRATCH.with(|cell| {
            let mut ws = cell.borrow_mut();
            let n_px = h * w;
            if ws.mask_buf.len() < n_px {
                ws.mask_buf.resize(n_px, 0);
            }
            let mask_buf = &mut ws.mask_buf[..n_px];
            let y_top = (h as f64 * 0.12) as usize;
            let y_bot = ((h as f64 * 0.92) as usize).min(h);
            mask_buf[..(y_top * w).min(n_px)].fill(0);
            if y_bot < h {
                mask_buf[y_bot * w..n_px].fill(0);
            }
            for y in y_top..y_bot {
                let row_px = y * w;
                let dst_row = &mut mask_buf[row_px..row_px + w];
                let src_row = &hsv_s[row_px * 3..(row_px + w) * 3];
                for (dst, px) in dst_row.iter_mut().zip(src_row.as_chunks::<3>().0) {
                    *dst = if (88..=99).contains(&px[0]) && px[1] >= 215 && px[2] >= 230 {
                        255
                    } else {
                        0
                    };
                }
            }
            let c1_y1 = ((h as f64 * 0.45) as usize).min(h);
            let c1_x1 = ((w as f64 * 0.18) as usize).min(w);
            for y in y_top..c1_y1 {
                mask_buf[y * w..y * w + c1_x1].fill(0);
            }
            let c2_y0 = ((h as f64 * 0.80) as usize).min(h);
            let c2_x0 = ((w as f64 * 0.82) as usize).min(w);
            for y in c2_y0..y_bot {
                mask_buf[y * w + c2_x0..y * w + w].fill(0);
            }
            extract_filtered_contours_external_slice(
                mask_buf,
                h,
                w,
                min_area,
                max_area,
                min_dim.floor() as i32,
                max_dim.ceil() as i32,
            )
        });
        if contours.is_empty() {
            return None;
        }
        let r_out = (75.0 * su).round_ties_even() as isize;
        let yel_thresh = ((45.0 * sf) as usize).max(25);

        let mut valid: Vec<(&Vec<Point2i>, f64, f64)> = Vec::new();
        let mut v_disc = Vec::with_capacity(2048);

        for cnt in &contours {
            let area = contour_area(cnt);
            if !(min_area..=max_area).contains(&area) {
                continue;
            }
            let rect = bounding_rect(cnt);
            let bw = rect.width as f64;
            let bh = rect.height as f64;
            if !(min_dim..=max_dim).contains(&bw) || !(min_dim..=max_dim).contains(&bh) {
                continue;
            }
            let aspect = bw.max(bh) / bw.min(bh).max(1.0);
            if aspect > 1.85 {
                continue;
            }
            let hull = convex_hull(cnt);
            let hull_area = contour_area(&hull);
            if hull_area <= 0.0 || (area / hull_area) < 0.55 {
                continue;
            }
            let mc = contour_moments(cnt);
            if mc.m00 <= 0.0 {
                continue;
            }
            let ccx = mc.m10 / mc.m00;
            let ccy = mc.m01 / mc.m00;

            let x0 = ((ccx as isize) - r_out).max(0) as usize;
            let x1 = ((ccx as isize) + r_out + 1).min(w as isize).max(0) as usize;
            let y0 = ((ccy as isize) - r_out).max(0) as usize;
            let y1 = ((ccy as isize) + r_out + 1).min(h as isize).max(0) as usize;

            if x1 > x0 && y1 > y0 {
                v_disc.clear();
                let mut dark_cnt = 0usize;
                let mut yel_ptr = 0usize;
                for y in y0..y1 {
                    let dy = y as f64 - ccy;
                    for x in x0..x1 {
                        let dx = x as f64 - ccx;
                        let dist = dx.hypot(dy) / su;
                        if (22.0..=38.0).contains(&dist) {
                            let v = hsv[[y, x, 2]];
                            if v < 65 {
                                dark_cnt += 1;
                            }
                            v_disc.push(v);
                        } else if (44.0..=74.0).contains(&dist) {
                            let hh = hsv[[y, x, 0]];
                            let ss = hsv[[y, x, 1]];
                            let vv = hsv[[y, x, 2]];
                            if (18..=34).contains(&hh) && ss >= 140 && vv >= 200 {
                                yel_ptr += 1;
                            }
                        }
                    }
                }
                let (dark_disc, v_p90) = if v_disc.is_empty() {
                    (0.0, 255.0)
                } else {
                    let dd = dark_cnt as f64 / v_disc.len() as f64;
                    v_disc.sort_unstable();
                    let p90 = percentile_linear_u8(&v_disc, 90.0);
                    (dd, p90)
                };
                if (dark_disc >= 0.70 && v_p90 <= 85.0)
                    || (dark_disc >= 0.55 && yel_ptr >= yel_thresh)
                {
                    continue;
                }
            }

            valid.push((cnt, ccx, ccy));
        }

        if valid.is_empty() {
            return None;
        }

        let center_x = w as f64 / 2.0;
        let center_y = h as f64 / 2.0;
        let &(best, mut cx, mut cy) = valid
            .iter()
            .min_by(|&&(_, ax, ay), &&(_, bx, by)| {
                let da = (ax - center_x).hypot(ay - center_y);
                let db = (bx - center_x).hypot(by - center_y);
                da.total_cmp(&db)
            })?;

        let peri = arc_length(best, true);
        let mut tri_pts: Option<[[f64; 2]; 3]> = None;
        for eps_f in [0.04, 0.05, 0.06, 0.03, 0.07, 0.08, 0.09, 0.10] {
            let ap = approx_poly_dp(best, eps_f * peri, true);
            if ap.len() == 3 {
                tri_pts = Some([
                    [ap[0].x as f64, ap[0].y as f64],
                    [ap[1].x as f64, ap[1].y as f64],
                    [ap[2].x as f64, ap[2].y as f64],
                ]);
                break;
            }
        }
        if tri_pts.is_none() {
            let pts_f: Vec<Point2f> = best
                .iter()
                .map(|p| Point2f {
                    x: p.x as f32,
                    y: p.y as f32,
                })
                .collect();
            if let Some((_, tri)) = min_enclosing_triangle(&pts_f) {
                tri_pts = Some([
                    [tri[0].x as f64, tri[0].y as f64],
                    [tri[1].x as f64, tri[1].y as f64],
                    [tri[2].x as f64, tri[2].y as f64],
                ]);
            }
        }

        if let Some(tp) = tri_pts {
            let mut best_i = 0usize;
            let mut best_val = f64::INFINITY;
            for i in 0..3 {
                let vd = (tp[i][0] - cx).hypot(tp[i][1] - cy);
                let j = (i + 1) % 3;
                let k = (i + 2) % 3;
                let ol = (tp[j][0] - tp[k][0]).hypot(tp[j][1] - tp[k][1]);
                let score = ol + vd;
                if score < best_val {
                    best_val = score;
                    best_i = i;
                }
            }
            let dx_n = tp[best_i][0] - cx;
            let dy_n = tp[best_i][1] - cy;
            let nd = dx_n.hypot(dy_n);
            if nd > 1e-6 {
                let ux = dx_n / nd;
                let uy = dy_n / nd;
                cx += ARROW_PIVOT_OFFSET_PX_1440 * su * ux;
                cy += ARROW_PIVOT_OFFSET_PX_1440 * su * uy;
                let angle = wrap_angle_360(ux.atan2(-uy).to_degrees());
                return Some((cx, cy, angle));
            }
        }

        let mut best_tip = best[0];
        let mut best_d2 = -1.0f64;
        for &p in best {
            let dx = p.x as f64 - cx;
            let dy = p.y as f64 - cy;
            let d2 = dx * dx + dy * dy;
            if d2 > best_d2 {
                best_d2 = d2;
                best_tip = p;
            }
        }
        let angle = wrap_angle_360((best_tip.x as f64 - cx).atan2(-(best_tip.y as f64 - cy)).to_degrees());
        Some((cx, cy, angle))
    }

    fn get_elev_templates(&self, scale_ui: f64) -> Option<[Array3<u8>; 4]> {
        let ma = self.ui_anchors.get("tpl_ma")?;
        let mi = self.ui_anchors.get("tpl_mi")?;
        let la = self.ui_anchors.get("tpl_la")?;
        let li = self.ui_anchors.get("tpl_li")?;
        let sz = ((24.0 * scale_ui).round_ties_even() as isize).max(12) as usize;
        if let Ok(guard) = self.elev_tpl_cache.lock()
            && let Some(hit) = guard.get(&sz)
        {
            return Some(hit.clone());
        }
        let interp = if sz < 48 {
            Interpolation::Area
        } else {
            Interpolation::Linear
        };
        let resize_one = |tpl: &Array3<u8>| -> Option<Array3<u8>> {
            if tpl.dim().0 == sz && tpl.dim().1 == sz {
                Some(tpl.clone())
            } else {
                resize_u8_hwc(tpl.view(), sz, sz, interp).ok()
            }
        };
        let entry = [
            resize_one(ma)?,
            resize_one(mi)?,
            resize_one(la)?,
            resize_one(li)?,
        ];
        if let Ok(mut guard) = self.elev_tpl_cache.lock() {
            guard.insert(sz, entry.clone());
        }
        Some(entry)
    }

    fn detect_layers_template(&self, frame_bgr: ArrayView3<'_, u8>) -> Option<LayerInfo> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return None;
        }
        let scale_ui = (h as f64 / 1440.0).min(w as f64 / 2560.0);
        if scale_ui <= 0.1 {
            return None;
        }
        let tpls = self.get_elev_templates(scale_ui)?;
        let sz_h = tpls[0].dim().0;
        let full_sz = ((48.0 * scale_ui).round_ties_even() as isize).max(20) as usize;
        let x_col = (w as f64 - 97.0 * scale_ui).round_ties_even() as isize;
        let half_strip = ((full_sz / 2 + 8) as isize)
            .max((40.0 * scale_ui).round_ties_even() as isize);
        let x0 = (x_col - half_strip).max(0) as usize;
        let x1 = (x_col + half_strip).min(w as isize).max(0) as usize;
        let y0 = (0.20 * h as f64) as usize;
        let y1 = (0.82 * h as f64) as usize;
        if y1 <= y0 || x1 <= x0 {
            return None;
        }
        let roi = frame_bgr.slice(s![y0..y1, x0..x1, 0..3]);
        let (rh, rw, _) = roi.dim();
        if rh < full_sz || rw < full_sz {
            return None;
        }

        let rh_h = (rh / 2).max(sz_h);
        let rw_h = (rw / 2).max(sz_h);
        let roi_h = resize_u8_hwc(roi, rw_h, rh_h, Interpolation::Area).ok()?;
        let sx_f = rw as f64 / rw_h as f64;
        let sy_f = rh as f64 / rh_h as f64;

        let tnames = ["MA", "MI", "LA", "LI"];
        let mut maps_res: Vec<Array2<f32>> = Vec::with_capacity(4);
        let mut raw_hits: Vec<(f64, usize, f64, f64, usize, usize)> = Vec::new();
        for (tidx, tpl) in tpls.iter().enumerate() {
            let res =
                match_template_u8_hwc(roi_h.view(), tpl.view(), TemplateMatchMode::CcoeffNormed)
                    .ok()?;
            let (mh, mw) = res.dim();
            for ry in 0..mh {
                for rx in 0..mw {
                    let sc = res[[ry, rx]] as f64;
                    if sc >= 0.74 {
                        let cx_g = x0 as f64 + (rx as f64 + sz_h as f64 / 2.0) * sx_f;
                        let cy_g = y0 as f64 + (ry as f64 + sz_h as f64 / 2.0) * sy_f;
                        raw_hits.push((sc, tidx, cx_g, cy_g, rx, ry));
                    }
                }
            }
            maps_res.push(res);
        }

        if raw_hits.is_empty() {
            return Some(LayerInfo::default());
        }

        #[derive(Clone)]
        struct ElevSlot {
            score: f64,
            tidx: usize,
            cx: f64,
            cy: f64,
            active_score: f64,
        }

        let mut enriched: Vec<ElevSlot> = raw_hits
            .into_iter()
            .map(|(sc, tidx, cx, cy, rx, ry)| {
                let active_score = (maps_res[0][[ry, rx]] as f64).max(maps_res[2][[ry, rx]] as f64);
                ElevSlot {
                    score: sc,
                    tidx,
                    cx,
                    cy,
                    active_score,
                }
            })
            .collect();
        enriched.sort_by(|a, b| b.score.total_cmp(&a.score));

        let min_dy = 28.0 * scale_ui;
        let min_dx = 18.0 * scale_ui;
        let mut slots: Vec<ElevSlot> = Vec::new();
        for h_item in enriched {
            if slots
                .iter()
                .any(|s| (h_item.cy - s.cy).abs() < min_dy && (h_item.cx - s.cx).abs() < min_dx)
            {
                continue;
            }
            slots.push(h_item);
        }

        if slots.len() < 2 {
            return Some(LayerInfo {
                slot_types: slots.iter().map(|s| tnames[s.tidx].to_owned()).collect(),
                ..LayerInfo::default()
            });
        }

        let mut xs_for_med: Vec<f64> = slots.iter().map(|s| s.cx).collect();
        xs_for_med.sort_by(|a, b| a.total_cmp(b));
        let med_x = if xs_for_med.len().is_multiple_of(2) {
            0.5 * (xs_for_med[xs_for_med.len() / 2 - 1] + xs_for_med[xs_for_med.len() / 2])
        } else {
            xs_for_med[xs_for_med.len() / 2]
        };
        let col_tol = 14.0 * scale_ui;
        slots.retain(|s| (s.cx - med_x).abs() <= col_tol);
        slots.sort_by(|a, b| a.cy.total_cmp(&b.cy));

        let has_active = slots
            .iter()
            .any(|s| s.tidx == 0 || s.tidx == 2 || s.active_score >= 0.74);
        if slots.len() < 2 || !has_active {
            return Some(LayerInfo {
                slot_types: slots.iter().map(|s| tnames[s.tidx].to_owned()).collect(),
                ..LayerInfo::default()
            });
        }

        let a_pos = slots
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let sa = a.active_score + if a.tidx == 0 || a.tidx == 2 { 0.25 } else { 0.0 };
                let sb = b.active_score + if b.tidx == 0 || b.tidx == 2 { 0.25 } else { 0.0 };
                sa.total_cmp(&sb)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        let max_gap = 135.0 * scale_ui;
        let mut start = a_pos;
        while start > 0 && (slots[start].cy - slots[start - 1].cy).abs() <= max_gap {
            start -= 1;
        }
        let mut end = a_pos + 1;
        while end < slots.len() && (slots[end].cy - slots[end - 1].cy).abs() <= max_gap {
            end += 1;
        }
        let cluster = &slots[start..end];
        let n = cluster.len();
        if n < 2 {
            return Some(LayerInfo {
                slot_types: cluster.iter().map(|s| tnames[s.tidx].to_owned()).collect(),
                ..LayerInfo::default()
            });
        }

        let active_idx = cluster
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let sa = a.active_score + if a.tidx == 0 || a.tidx == 2 { 1.0 } else { 0.0 };
                let sb = b.active_score + if b.tidx == 0 || b.tidx == 2 { 1.0 } else { 0.0 };
                sa.total_cmp(&sb)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        let is_master = |tidx: usize| tidx == 0 || tidx == 1; // MA or MI
        let surface_idx = if is_master(cluster[0].tidx) {
            0
        } else if is_master(cluster[n - 1].tidx) {
            n - 1
        } else if active_idx > 0 {
            0
        } else {
            n - 1
        };

        let is_underground = active_idx != surface_idx;
        let floor_name = if !is_underground {
            "G".to_owned()
        } else if surface_idx == 0 {
            format!("B{active_idx}")
        } else {
            format!("B{}", active_idx + 1)
        };

        Some(LayerInfo {
            has_layers: true,
            is_underground,
            total_floors: n,
            active_floor_idx: active_idx,
            surface_idx,
            floor_name,
            slot_types: cluster.iter().map(|s| tnames[s.tidx].to_owned()).collect(),
        })
    }

    pub fn detect_layers_universal(&self, frame_bgr: ArrayView3<'_, u8>) -> LayerInfo {
        let (h, w, c) = frame_bgr.dim();
        if h < 200 || w < 320 || c < 3 {
            return LayerInfo::default();
        }
        if let Some(tpl_res) = self.detect_layers_template(frame_bgr) {
            return tpl_res;
        }
        Self::detect_layers_structural(frame_bgr)
    }

    /// 100% IP-clean structural detector for Genshin's right-edge Map-M layer elevator pill.
    ///
    /// Operates on a `0.5x` canonical 1440p vertical strip (`~58x446 px`, `< 0.3 ms`) anchored to
    /// Unity's right-edge CanvasScaler Expand column (`x = w - 97 * min(h/1440, w/2560)`),
    /// immune to cyan snow/water terrain bleed (e.g. Snezhnaya / Fontaine) and brightness shifts:
    /// 1. Scores every row for inactive white-on-dark-capsule slots (`MI`/`LI`) and active cyan
    ///    slots (`LA` cyan diamond in dark capsule, or `MA` white mountain in glowing cyan halo).
    /// 2. Requires a valid active + inactive cluster bridged by continuous dark capsule pixels
    ///    centered vertically near `h / 2` (eliminates open-ocean / snow false positives).
    /// 3. Classifies surface (`M`) vs underground (`L`) on the top/bottom endpoints using radial
    ///    pure-white mass & mid-ring brightness (`MA/MI` >> `LA/LI` with zero overlap).
    fn detect_layers_structural(frame_bgr: ArrayView3<'_, u8>) -> LayerInfo {
        struct ElevMasks {
            c4: Vec<(isize, isize)>,
            c7: Vec<(isize, isize)>,
            c10: Vec<(isize, isize)>,
            c12: Vec<(isize, isize)>,
            r6_11: Vec<(isize, isize)>,
            r12_18: Vec<(isize, isize)>,
            r14_19: Vec<(isize, isize)>,
            r8_19: Vec<(isize, isize)>,
            q: [Vec<(isize, isize)>; 4],
            r19: Vec<(isize, isize)>,
        }

        static MASKS: OnceLock<ElevMasks> = OnceLock::new();
        let masks = MASKS.get_or_init(|| {
            let mut m = ElevMasks {
                c4: Vec::new(),
                c7: Vec::new(),
                c10: Vec::new(),
                c12: Vec::new(),
                r6_11: Vec::new(),
                r12_18: Vec::new(),
                r14_19: Vec::new(),
                r8_19: Vec::new(),
                q: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
                r19: Vec::new(),
            };
            for dy in -20isize..=20 {
                for dx in -20isize..=20 {
                    let r = ((dx * dx + dy * dy) as f64).sqrt();
                    if r <= 4.0 {
                        m.c4.push((dy, dx));
                    }
                    if r <= 7.0 {
                        m.c7.push((dy, dx));
                    }
                    if r <= 10.0 {
                        m.c10.push((dy, dx));
                    }
                    if r <= 12.0 {
                        m.c12.push((dy, dx));
                    }
                    if (6.0..=11.0).contains(&r) {
                        m.r6_11.push((dy, dx));
                    }
                    if (12.0..=18.0).contains(&r) {
                        m.r12_18.push((dy, dx));
                    }
                    if (14.0..=19.0).contains(&r) {
                        m.r14_19.push((dy, dx));
                    }
                    if (8.0..=19.0).contains(&r) {
                        m.r8_19.push((dy, dx));
                        let qi = match (dx >= 0, dy >= 0) {
                            (true, false) => 0,
                            (false, false) => 1,
                            (false, true) => 2,
                            (true, true) => 3,
                        };
                        m.q[qi].push((dy, dx));
                    }
                    if r <= 19.0 {
                        m.r19.push((dy, dx));
                    }
                }
            }
            m
        });

        let (h, w, _) = frame_bgr.dim();
        let scale_ui = (h as f64 / 1440.0).min(w as f64 / 2560.0);
        if scale_ui <= 0.1 {
            return LayerInfo::default();
        }

        let x_center = w as f64 - 97.0 * scale_ui;
        let strip_hw = 58.0 * scale_ui;
        let x0 = (x_center - strip_hw).round().max(0.0) as usize;
        let x1 = (x_center + strip_hw).round().min(w as f64) as usize;
        let y0 = (0.20 * h as f64).round() as usize;
        let y1 = (0.82 * h as f64).round().min(h as f64) as usize;
        if x1 <= x0 + 20 || y1 <= y0 + 80 {
            return LayerInfo::default();
        }

        let roi = frame_bgr.slice(s![y0..y1, x0..x1, 0..3]);
        let nw = (((x1 - x0) as f64) / (2.0 * scale_ui)).round().max(44.0) as usize;
        let nh = (((y1 - y0) as f64) / (2.0 * scale_ui)).round().max(60.0) as usize;
        if nw < 44 || nh < 60 {
            return LayerInfo::default();
        }

        let Ok(norm) = resize_u8_hwc(roi, nw, nh, Interpolation::Area) else {
            return LayerInfo::default();
        };
        let Ok(hsv) = bgr_to_hsv(norm.view()) else {
            return LayerInfo::default();
        };

        let n_px = nh * nw;
        let mut v_grid = vec![0u8; n_px];
        let mut s_grid = vec![0u8; n_px];
        let mut dark_cap = vec![0u8; n_px];
        let mut cyan = vec![0u8; n_px];
        let mut pure_w = vec![0u8; n_px];

        for y in 0..nh {
            let row = y * nw;
            for x in 0..nw {
                let h_val = hsv[[y, x, 0]];
                let s_val = hsv[[y, x, 1]];
                let v_val = hsv[[y, x, 2]];
                let idx = row + x;
                v_grid[idx] = v_val;
                s_grid[idx] = s_val;
                if (16..=138).contains(&v_val) && s_val <= 155 {
                    dark_cap[idx] = 1;
                }
                if (82..=116).contains(&h_val) && s_val >= 68 && v_val >= 85 {
                    cyan[idx] = 1;
                }
                if s_val <= 70 && v_val >= 148 {
                    pure_w[idx] = 1;
                }
            }
        }

        let mean_u8 = |grid: &[u8], cy: usize, cx: usize, offs: &[(isize, isize)]| -> f64 {
            let mut sum = 0u64;
            for &(dy, dx) in offs {
                let y = (cy as isize + dy) as usize;
                let x = (cx as isize + dx) as usize;
                sum += grid[y * nw + x] as u64;
            }
            sum as f64 / offs.len().max(1) as f64
        };

        let percentile_u8 =
            |grid: &[u8], cy: usize, cx: usize, offs: &[(isize, isize)], pct: f64, buf: &mut Vec<u8>| -> f64 {
                buf.clear();
                for &(dy, dx) in offs {
                    let y = (cy as isize + dy) as usize;
                    let x = (cx as isize + dx) as usize;
                    buf.push(grid[y * nw + x]);
                }
                if buf.is_empty() {
                    return 0.0;
                }
                buf.sort_unstable();
                let pos = ((buf.len() - 1) as f64 * pct).clamp(0.0, (buf.len() - 1) as f64);
                let lo = pos.floor() as usize;
                let hi = pos.ceil() as usize;
                let frac = pos - lo as f64;
                buf[lo] as f64 * (1.0 - frac) + buf[hi] as f64 * frac
            };

        let std_u8 = |grid: &[u8], cy: usize, cx: usize, offs: &[(isize, isize)]| -> f64 {
            let mut sum = 0.0f64;
            let mut sum2 = 0.0f64;
            for &(dy, dx) in offs {
                let y = (cy as isize + dy) as usize;
                let x = (cx as isize + dx) as usize;
                let val = grid[y * nw + x] as f64;
                sum += val;
                sum2 += val * val;
            }
            let n = offs.len().max(1) as f64;
            let m = sum / n;
            (sum2 / n - m * m).max(0.0).sqrt()
        };

        let scan_column = |xc: usize| -> Option<LayerInfo> {
            if xc < 20 || xc + 20 >= nw || nh <= 42 {
                return None;
            }
            let y_len = nh - 42;
            let mut inact_scores = vec![0.0f64; y_len];
            let mut act_scores = vec![0.0f64; y_len];
            let mut act_is_ma = vec![false; y_len];
            let mut v_buf = Vec::with_capacity(640);

            for i in 0..y_len {
                let cy = i + 21;

                // 1. Inactive slot (`MI` or `LI`): bright low-sat core on uniform dark capsule ring.
                let c4_v = mean_u8(&v_grid, cy, xc, &masks.c4);
                let c4_s = mean_u8(&s_grid, cy, xc, &masks.c4);
                let c4_cy = mean_u8(&cyan, cy, xc, &masks.c4);
                if c4_v >= 140.0 && c4_s <= 60.0 && c4_cy <= 0.15 {
                    let r12_dp = mean_u8(&dark_cap, cy, xc, &masks.r12_18);
                    let r12_cy = mean_u8(&cyan, cy, xc, &masks.r12_18);
                    if r12_dp >= 0.65 && r12_cy <= 0.15 {
                        let r12_v = percentile_u8(&v_grid, cy, xc, &masks.r12_18, 0.5, &mut v_buf);
                        let contrast = c4_v - r12_v;
                        if r12_v <= 128.0 && contrast >= 58.0 {
                            inact_scores[i] = r12_dp * (contrast / 110.0).min(1.0);
                        }
                    }
                }

                // 2. Active Case A (`LA`): vivid cyan diamond (r<=10) inside dark capsule (r=14..19).
                let c10_cy = mean_u8(&cyan, cy, xc, &masks.c10);
                let mut la_sc = 0.0f64;
                if c10_cy >= 0.48 {
                    let r14_dp = mean_u8(&dark_cap, cy, xc, &masks.r14_19);
                    let r14_cy = mean_u8(&cyan, cy, xc, &masks.r14_19);
                    if r14_dp >= 0.55 && r14_cy <= 0.40 {
                        let c7_v90 = percentile_u8(&v_grid, cy, xc, &masks.c7, 0.90, &mut v_buf);
                        let r14_v = percentile_u8(&v_grid, cy, xc, &masks.r14_19, 0.5, &mut v_buf);
                        if c7_v90 >= 148.0 && (c7_v90 - r14_v) >= 55.0 {
                            la_sc = c10_cy * r14_dp * ((c7_v90 - r14_v) / 110.0).min(1.0);
                        }
                    }
                }

                // 3. Active Case B (`MA`): bright pure-white mountain (r<=10) inside cyan halo (r=8..19).
                let c7_v = mean_u8(&v_grid, cy, xc, &masks.c7);
                let mut ma_sc = 0.0f64;
                if c7_v >= 148.0 {
                    let c10_pw = mean_u8(&pure_w, cy, xc, &masks.c10);
                    let r8_cy = mean_u8(&cyan, cy, xc, &masks.r8_19);
                    if c10_pw >= 0.20 && r8_cy >= 0.24 {
                        let q_ok = masks
                            .q
                            .iter()
                            .filter(|q| mean_u8(&cyan, cy, xc, q) >= 0.14)
                            .count();
                        if q_ok >= 3 {
                            let v_std = std_u8(&v_grid, cy, xc, &masks.r19);
                            if v_std >= 22.0 {
                                ma_sc = (c10_pw / 0.45).min(1.0)
                                    * (r8_cy / 0.45).min(1.0)
                                    * (v_std / 45.0).min(1.0);
                            }
                        }
                    }
                }

                if la_sc >= ma_sc && la_sc > 0.0 {
                    act_scores[i] = la_sc;
                    act_is_ma[i] = false;
                } else if ma_sc > 0.0 {
                    act_scores[i] = ma_sc;
                    act_is_ma[i] = true;
                }
            }

            let nms_peaks = |scores: &[f64], min_val: f64, min_dist: usize| -> Vec<(usize, f64, bool)> {
                let mut idxs: Vec<usize> = (0..scores.len()).filter(|&i| scores[i] >= min_val).collect();
                idxs.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
                let mut peaks: Vec<(usize, f64, bool)> = Vec::new();
                for i in idxs {
                    let cy = i + 21;
                    if peaks.iter().any(|&(py, _, _)| cy.abs_diff(py) < min_dist) {
                        continue;
                    }
                    peaks.push((cy, scores[i], act_is_ma[i]));
                }
                peaks
            };

            let act_peaks = nms_peaks(&act_scores, 0.18, 28);
            let inact_peaks = nms_peaks(&inact_scores, 0.30, 28);
            if act_peaks.is_empty() || inact_peaks.is_empty() {
                return None;
            }

            let bridge_ok = |y_a: usize, y_b: usize| -> bool {
                let gap = y_a.abs_diff(y_b);
                if !(31..=63).contains(&gap) {
                    return false;
                }
                let ym = (y_a + y_b) / 2;
                let ry0 = ym.saturating_sub(4);
                let ry1 = (ym + 5).min(nh);
                let rx0 = xc.saturating_sub(14);
                let rx1 = (xc + 15).min(nw);
                let mut sum = 0u64;
                let mut cnt = 0u64;
                for y in ry0..ry1 {
                    let row = y * nw;
                    for x in rx0..rx1 {
                        sum += dark_cap[row + x] as u64;
                        cnt += 1;
                    }
                }
                cnt > 0 && (sum as f64 / cnt as f64) >= 0.45
            };

            #[derive(Clone, Copy)]
            struct SlotCand {
                cy: usize,
                is_active: bool,
                score: f64,
                is_ma: bool,
            }

            let mut best_cl: Option<Vec<SlotCand>> = None;
            let mut best_sc = -1.0f64;

            for &(ay, asc, is_ma) in &act_peaks {
                let mut cands = Vec::with_capacity(inact_peaks.len() + 1);
                cands.push(SlotCand {
                    cy: ay,
                    is_active: true,
                    score: asc,
                    is_ma,
                });
                for &(iy, isc, _) in &inact_peaks {
                    if iy.abs_diff(ay) >= 29 {
                        cands.push(SlotCand {
                            cy: iy,
                            is_active: false,
                            score: isc,
                            is_ma: false,
                        });
                    }
                }
                cands.sort_by_key(|c| c.cy);
                let Some(a_idx) = cands.iter().position(|c| c.is_active) else {
                    continue;
                };
                let mut st = a_idx;
                while st > 0 && bridge_ok(cands[st - 1].cy, cands[st].cy) {
                    st -= 1;
                }
                let mut en = a_idx + 1;
                while en < cands.len() && bridge_ok(cands[en - 1].cy, cands[en].cy) {
                    en += 1;
                }
                let cl = &cands[st..en];
                if cl.len() < 2 {
                    continue;
                }
                let cy_mid_global =
                    y0 as f64 + 0.5 * (cl[0].cy + cl[cl.len() - 1].cy) as f64 * (2.0 * scale_ui);
                if (cy_mid_global - 0.5 * h as f64).abs() / scale_ui > 130.0 {
                    continue;
                }
                let sc = asc + cl.iter().filter(|c| !c.is_active).map(|c| c.score).sum::<f64>();
                if sc > best_sc {
                    best_sc = sc;
                    best_cl = Some(cl.to_vec());
                }
            }

            let cl = best_cl?;
            let n = cl.len();
            let active_idx = cl.iter().position(|c| c.is_active)?;

            let mountain_score = |slot: SlotCand| -> f64 {
                let pw12 = mean_u8(&pure_w, slot.cy, xc, &masks.c12);
                let mid_v = mean_u8(&v_grid, slot.cy, xc, &masks.r6_11) / 255.0;
                let bonus = if slot.is_active {
                    if slot.is_ma { 0.35 } else { -0.35 }
                } else {
                    0.0
                };
                0.65 * pw12 + 0.35 * mid_v + bonus
            };

            let surface_idx = if mountain_score(cl[0]) >= mountain_score(cl[n - 1]) {
                0
            } else {
                n - 1
            };
            let is_underground = active_idx != surface_idx;
            let floor_name = if !is_underground {
                "G".to_owned()
            } else if surface_idx == 0 {
                format!("B{active_idx}")
            } else {
                format!("B{}", active_idx + 1)
            };
            let slot_types = cl
                .iter()
                .enumerate()
                .map(|(idx, c)| match (idx == surface_idx, c.is_active) {
                    (true, true) => "MA".to_owned(),
                    (true, false) => "MI".to_owned(),
                    (false, true) => "LA".to_owned(),
                    (false, false) => "LI".to_owned(),
                })
                .collect();

            Some(LayerInfo {
                has_layers: true,
                is_underground,
                total_floors: n,
                active_floor_idx: active_idx,
                surface_idx,
                floor_name,
                slot_types,
            })
        };

        let xc_nom = (((x_center - x0 as f64) / (2.0 * scale_ui)).round() as isize)
            .clamp(20, nw as isize - 21) as usize;
        if let Some(info) = scan_column(xc_nom) {
            return info;
        }
        for delta in [-2isize, 2] {
            let xc_try = (xc_nom as isize + delta).clamp(20, nw as isize - 21) as usize;
            if xc_try != xc_nom
                && let Some(info) = scan_column(xc_try)
            {
                return info;
            }
        }
        LayerInfo::default()
    }

    pub fn detect_waypoint_candidates(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        arrow_pos: Option<(f64, f64)>,
        hsv_opt: Option<ArrayView3<'_, u8>>,
    ) -> Vec<(f64, f64, i32)> {
        self.detect_waypoint_candidates_with_gray(frame_bgr, arrow_pos, hsv_opt, None)
    }

    fn detect_waypoint_candidates_with_gray(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        arrow_pos: Option<(f64, f64)>,
        hsv_opt: Option<ArrayView3<'_, u8>>,
        gray_opt: Option<ArrayView2<'_, u8>>,
    ) -> Vec<(f64, f64, i32)> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return Vec::new();
        }
        let bgr3 = if c > 3 {
            frame_bgr.slice(s![.., .., 0..3])
        } else {
            frame_bgr
        };
        let hsv_owned;
        let hsv = match hsv_opt {
            Some(v) => v,
            None => {
                hsv_owned = bgr_to_hsv(bgr3).ok();
                match &hsv_owned {
                    Some(a) => a.view(),
                    None => return Vec::new(),
                }
            }
        };
        let gray_owned;
        let gray_frame = match gray_opt {
            Some(g) => g,
            None => {
                let Ok(g) = bgr_to_gray(bgr3) else {
                    return Vec::new();
                };
                gray_owned = g;
                gray_owned.view()
            }
        };

        let hsv_cow = hsv.as_standard_layout();
        let Some(hsv_s) = hsv_cow.as_slice() else {
            return Vec::new();
        };

        let scale_ui = w as f64 / 2560.0;
        let scale_factor = scale_ui * scale_ui;
        let min_area = 6.0f64.max(14.0 * scale_factor);
        let max_area = 110.0f64.max(1450.0 * scale_factor);
        let max_dim = 58.0 * scale_ui;
        let box_rad = ((18.0 * scale_ui).round_ties_even() as isize).max(12);
        let arrow_tol = 36.0 * scale_ui;
        let min_neutral_arrow = ((80.0 * scale_factor) as usize).max(16);

        let k_size = if w < 1600 { 2 } else { 3 };
        let n_px = h * w;
        WP_SCRATCH.with(|cell| {
            let mut ws = cell.borrow_mut();
            if ws.mask_buf.len() < n_px {
                ws.mask_buf.resize(n_px, 0);
            }
            if ws.tmp_buf.len() < n_px {
                ws.tmp_buf.resize(n_px, 0);
            }
            let ws_ref = &mut *ws;
            let mask_buf = &mut ws_ref.mask_buf[..n_px];
            let tmp_buf = &mut ws_ref.tmp_buf[..n_px];
            for (i, dst) in mask_buf.iter_mut().enumerate() {
                let p = i * 3;
                let hv = hsv_s[p];
                let sv = hsv_s[p + 1];
                let vv = hsv_s[p + 2];
                *dst = if (85..=126).contains(&hv) && sv >= 115 && vv >= 130 {
                    255
                } else {
                    0
                };
            }
            zero_ui_exclusion_rects(mask_buf, h, w);
            fast_open_rect_binary(mask_buf, tmp_buf, h, w, k_size);
            let contours = extract_filtered_contours_external_slice(
                mask_buf,
                h,
                w,
                min_area,
                max_area,
                1,
                max_dim.ceil() as i32,
            );

        let min_white_p1 = ((120.0 * scale_factor) as usize).max(15);
        let min_white_dom = ((65.0 * scale_factor) as usize).max(10);
        let min_dark_dom = ((100.0 * scale_factor) as usize).max(12);

        let mut candidates: Vec<(f64, f64, i32)> = Vec::new();
        for cnt in &contours {
            let area = contour_area(cnt);
            if !(min_area..=max_area).contains(&area) {
                continue;
            }
            let r = bounding_rect(cnt);
            if (r.width as f64) > max_dim || (r.height as f64) > max_dim {
                continue;
            }
            let mc = contour_moments(cnt);
            if mc.m00 <= 0.0 {
                continue;
            }
            let cx = mc.m10 / mc.m00;
            let cy = mc.m01 / mc.m00;
            let y0 = (cy as isize - box_rad).max(0) as usize;
            let y1 = (cy as isize + box_rad).min(h as isize).max(0) as usize;
            let x0 = (cx as isize - box_rad).max(0) as usize;
            let x1 = (cx as isize + box_rad).min(w as isize).max(0) as usize;

            let mut white_px = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    if gray_frame[[y, x]] > 200 {
                        white_px += 1;
                    }
                }
            }

            if let Some((ax, ay)) = arrow_pos
                && (cx - ax).hypot(cy - ay) < arrow_tol
            {
                let mut neutral_white = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        if gray_frame[[y, x]] > 198 && hsv[[y, x, 1]] < 65 {
                            neutral_white += 1;
                        }
                    }
                }
                if neutral_white < min_neutral_arrow {
                    continue;
                }
                candidates.push((cx, cy, white_px.max(neutral_white + 40) as i32));
            } else if white_px >= min_white_p1 {
                candidates.push((cx, cy, white_px as i32));
            } else if white_px >= min_white_dom {
                let mut dark_px = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        if gray_frame[[y, x]] < 65 {
                            dark_px += 1;
                        }
                    }
                }
                if dark_px >= min_dark_dom {
                    candidates.push((cx, cy, (white_px + dark_px) as i32));
                }
            }
        }

        let merge_dist = 34.0 * scale_ui;
        candidates.sort_by_key(|a| std::cmp::Reverse(a.2));
        let mut clean: Vec<(f64, f64, i32)> = Vec::new();
        for c_item in candidates {
            if !clean
                .iter()
                .any(|x| (c_item.0 - x.0).hypot(c_item.1 - x.1) < merge_dist)
            {
                clean.push(c_item);
            }
        }

        // Pass 2b: Dimmed / Anemo-teal / inactive-layer waypoints with ring contrast
        if clean.len() < 6 {
            for (i, dst) in mask_buf.iter_mut().enumerate() {
                let p = i * 3;
                let hv = hsv_s[p];
                let sv = hsv_s[p + 1];
                let vv = hsv_s[p + 2];
                *dst = if (82..=126).contains(&hv) && sv >= 78 && vv >= 105 {
                    255
                } else {
                    0
                };
            }
            zero_ui_exclusion_rects(mask_buf, h, w);
            fast_open_rect_binary(mask_buf, tmp_buf, h, w, 2);
            let cnts2 = extract_filtered_contours_external_slice(
                mask_buf,
                h,
                w,
                min_area,
                max_area,
                1,
                i32::MAX,
            );
            let outer_rad = ((20.0 * scale_ui).round_ties_even() as isize).max(14);
            let min_silver = ((75.0 * scale_factor) as usize).max(14);
            let mut cands2: Vec<(f64, f64, i32)> = Vec::new();
            for cnt in &cnts2 {
                let area = contour_area(cnt);
                if !(min_area..=max_area).contains(&area) {
                    continue;
                }
                let mc = contour_moments(cnt);
                if mc.m00 <= 0.0 {
                    continue;
                }
                let cx = mc.m10 / mc.m00;
                let cy = mc.m01 / mc.m00;
                if clean
                    .iter()
                    .any(|x| (cx - x.0).hypot(cy - x.1) < merge_dist)
                {
                    continue;
                }
                let y0 = (cy as isize - outer_rad).max(0) as usize;
                let y1 = (cy as isize + outer_rad).min(h as isize).max(0) as usize;
                let x0 = (cx as isize - outer_rad).max(0) as usize;
                let x1 = (cx as isize + outer_rad).min(w as isize).max(0) as usize;
                if y1 <= y0 || x1 <= x0 {
                    continue;
                }
                let ph = y1 - y0;
                let pw = x1 - x0;

                let mut silver_cnt = 0usize;
                let mut silver_sum = 0.0f64;
                let mut ring_bg_cnt = 0usize;
                let mut ring_bg_sum = 0.0f64;
                let mut neutral_arrow_cnt = 0usize;
                for py in 0..ph {
                    let on_ring_y = py < 3 || py + 3 >= ph;
                    for px_i in 0..pw {
                        let g = gray_frame[[y0 + py, x0 + px_i]];
                        let s_val = hsv[[y0 + py, x0 + px_i, 1]];
                        let is_silver = g > 158 && s_val < 68;
                        if is_silver {
                            silver_cnt += 1;
                            silver_sum += g as f64;
                        } else if on_ring_y || px_i < 3 || px_i + 3 >= pw {
                            ring_bg_cnt += 1;
                            ring_bg_sum += g as f64;
                        }
                        if g > 195 && s_val < 65 {
                            neutral_arrow_cnt += 1;
                        }
                    }
                }
                if silver_cnt < min_silver {
                    continue;
                }
                if let Some((ax, ay)) = arrow_pos
                    && (cx - ax).hypot(cy - ay) < arrow_tol
                    && neutral_arrow_cnt < min_neutral_arrow
                {
                    continue;
                }
                let ring_bg_mean = if ring_bg_cnt > 0 {
                    ring_bg_sum / ring_bg_cnt as f64
                } else {
                    255.0
                };
                let contrast = (silver_sum / silver_cnt as f64) - ring_bg_mean;
                if contrast > 28.0 {
                    cands2.push((cx, cy, silver_cnt as i32));
                }
            }
            cands2.sort_by_key(|a| std::cmp::Reverse(a.2));
            for c_item in cands2 {
                if !clean
                    .iter()
                    .any(|x| (c_item.0 - x.0).hypot(c_item.1 - x.1) < merge_dist)
                {
                    clean.push(c_item);
                }
            }
        }

        // Pass 2c: Locked/dark-backdrop neutral white teardrop pins
        if clean.len() < 5 {
            let gray_cow = gray_frame.as_standard_layout();
            if let Some(gray_s) = gray_cow.as_slice() {
                for (i, dst) in mask_buf.iter_mut().enumerate() {
                    *dst = if gray_s[i] > 205 && hsv_s[i * 3 + 1] < 50 {
                        255
                    } else {
                        0
                    };
                }
                zero_ui_exclusion_rects(mask_buf, h, w);
                let min_w_area = 16.0f64.max(22.0 * scale_factor);
                let max_w_area = 120.0f64.max(650.0 * scale_factor);
                let cnts_w = extract_filtered_contours_external_slice(
                    mask_buf,
                    h,
                    w,
                    min_w_area,
                    max_w_area,
                    1,
                    i32::MAX,
                );
                {
                let outer_rad = ((20.0 * scale_ui).round_ties_even() as isize).max(14);
                let mut cands3: Vec<(f64, f64, i32)> = Vec::new();
                for cnt in &cnts_w {
                    let wa = contour_area(cnt);
                    if !(min_w_area..=max_w_area).contains(&wa) {
                        continue;
                    }
                    let mc = contour_moments(cnt);
                    if mc.m00 <= 0.0 {
                        continue;
                    }
                    let wcx = mc.m10 / mc.m00;
                    let wcy = mc.m01 / mc.m00;
                    if clean
                        .iter()
                        .any(|x| (wcx - x.0).hypot(wcy - x.1) < merge_dist)
                    {
                        continue;
                    }
                    if let Some((ax, ay)) = arrow_pos
                        && (wcx - ax).hypot(wcy - ay) < arrow_tol
                    {
                        continue;
                    }
                    let y0 = (wcy as isize - outer_rad).max(0) as usize;
                    let y1 = (wcy as isize + outer_rad).min(h as isize).max(0) as usize;
                    let x0 = (wcx as isize - outer_rad).max(0) as usize;
                    let x1 = (wcx as isize + outer_rad).min(w as isize).max(0) as usize;
                    let ph = y1.saturating_sub(y0);
                    let pw = x1.saturating_sub(x0);
                    if ph >= 12 && pw >= 12 {
                        let mut b_sum = 0u64;
                        let mut b_cnt = 0usize;
                        // Replicates Python's border slice concatenation mean
                        for y in y0..y0 + 3 {
                            for x in x0..x1 {
                                b_sum += gray_frame[[y, x]] as u64;
                                b_cnt += 1;
                            }
                        }
                        for y in y1 - 3..y1 {
                            for x in x0..x1 {
                                b_sum += gray_frame[[y, x]] as u64;
                                b_cnt += 1;
                            }
                        }
                        for y in y0..y1 {
                            for x in x0..x0 + 3 {
                                b_sum += gray_frame[[y, x]] as u64;
                                b_cnt += 1;
                            }
                        }
                        for y in y0..y1 {
                            for x in x1 - 3..x1 {
                                b_sum += gray_frame[[y, x]] as u64;
                                b_cnt += 1;
                            }
                        }
                        if b_cnt > 0 && (b_sum as f64 / b_cnt as f64) < 145.0 {
                            cands3.push((wcx, wcy, wa as i32));
                        }
                    }
                }
                cands3.sort_by_key(|a| std::cmp::Reverse(a.2));
                for c_item in cands3 {
                    if !clean
                        .iter()
                        .any(|x| (c_item.0 - x.0).hypot(c_item.1 - x.1) < merge_dist)
                    {
                        clean.push(c_item);
                    }
                }
                }
            }
        }

        clean
        })
    }

    pub fn compensate_inliers_screen_anchors<'a>(
        inliers: &[((f64, f64, i32), &'a WaypointEntry)],
        scale_ui: f64,
    ) -> Vec<((f64, f64, i32), &'a WaypointEntry)> {
        inliers
            .iter()
            .map(|&(s, wp)| {
                let (ox, oy) = icon_anchor_offset_px(wp.label_id, scale_ui);
                ((s.0 + ox, s.1 + oy, s.2), wp)
            })
            .collect()
    }

    pub fn fit_isometric_lsq(
        inliers: &[((f64, f64, i32), &WaypointEntry)],
    ) -> Option<(f64, f64, f64, f64)> {
        let n = inliers.len();
        if n < 2 {
            return None;
        }
        let nf = n as f64;
        let msx = inliers.iter().map(|(s, _)| s.0).sum::<f64>() / nf;
        let msy = inliers.iter().map(|(s, _)| s.1).sum::<f64>() / nf;
        let mwx = inliers.iter().map(|(_, w)| w.x_pos).sum::<f64>() / nf;
        let mwy = inliers.iter().map(|(_, w)| w.y_pos).sum::<f64>() / nf;

        let mut denom = 0.0f64;
        let mut numer = 0.0f64;
        for &(s, w) in inliers {
            let dsx = s.0 - msx;
            let dsy = s.1 - msy;
            let dwx = w.x_pos - mwx;
            let dwy = w.y_pos - mwy;
            denom += dsx * dsx + dsy * dsy;
            numer += dsx * dwx + dsy * dwy;
        }
        if denom < 1e-6 {
            return None;
        }
        let scale = numer / denom;
        if scale <= 0.0 {
            return None;
        }
        let tx = mwx - scale * msx;
        let ty = mwy - scale * msy;
        let mut sq_err = 0.0f64;
        for &(s, w) in inliers {
            let ex = scale * s.0 + tx - w.x_pos;
            let ey = scale * s.1 + ty - w.y_pos;
            sq_err += ex * ex + ey * ey;
        }
        let rmse = (sq_err / nf).sqrt();
        Some((scale, tx, ty, rmse))
    }

    fn match_terrain_patch(
        &self,
        frame_gray: ArrayView2<'_, u8>,
        map_id: i32,
        scale: f64,
        tx: f64,
        ty: f64,
        search_pad: usize,
    ) -> Option<(Array2<f32>, f64, f64, usize, usize, f64)> {
        if scale <= 0.0 {
            return None;
        }
        let raster = self.load_terrain_raster(map_id)?;
        let [tx0, ty0, z, orig_x, orig_y] = raster.meta;
        let pow2z = 2.0f64.powf(z);
        let (ch_h, ch_w) = raster.gray.dim();
        let (fh, fw) = frame_gray.dim();
        if fh == 0 || fw == 0 {
            return None;
        }

        let rx0 = (0.16 * fw as f64) as usize;
        let rx1 = (0.84 * fw as f64) as usize;
        let ry0 = (0.14 * fh as f64) as usize;
        let ry1 = (0.86 * fh as f64) as usize;
        if rx1 <= rx0 || ry1 <= ry0 {
            return None;
        }
        let crop = frame_gray.slice(s![ry0..ry1, rx0..rx1]);
        let cw = (((rx1 - rx0) as f64 * scale * pow2z).round_ties_even() as usize).max(24);
        let ch = (((ry1 - ry0) as f64 * scale * pow2z).round_ties_even() as usize).max(24);
        if cw >= ch_w || ch >= ch_h {
            return None;
        }

        let q = resize_u8(crop, cw, ch, Interpolation::Area).ok()?;
        let x0_f = ((rx0 as f64 * scale + tx) + orig_x) * pow2z - tx0 * 256.0;
        let y0_f = ((ry0 as f64 * scale + ty) + orig_y) * pow2z - ty0 * 256.0;
        let x0 = x0_f.round_ties_even() as isize;
        let y0 = y0_f.round_ties_even() as isize;
        let pad = search_pad as isize;

        let rx_a = (x0 - pad).max(0) as usize;
        let ry_a = (y0 - pad).max(0) as usize;
        let rx_b = (x0 + cw as isize + pad).min(ch_w as isize).max(0) as usize;
        let ry_b = (y0 + ch as isize + pad).min(ch_h as isize).max(0) as usize;
        if rx_b.saturating_sub(rx_a) < cw || ry_b.saturating_sub(ry_a) < ch {
            return None;
        }

        let roi = raster.gray.slice(s![ry_a..ry_b, rx_a..rx_b]);
        let nz_cnt = roi.iter().filter(|&&v| v > 2).count();
        let nz_mean = nz_cnt as f64 / roi.len() as f64;
        if nz_mean < 0.40 || array_std_u8(roi) < 6.0 || array_std_u8(q.view()) < 6.0 {
            return None;
        }

        let qg = sobel_magnitude_u8_f32(q.view()).ok()?;
        let rg = sobel_magnitude_u8_f32(roi).ok()?;
        let res = fast_match_template_ccoeff_f32(rg.view(), qg.view())?;
        Some((res, x0_f, y0_f, rx_a, ry_a, pow2z))
    }

    pub fn score_terrain_alignment(
        &self,
        frame_gray: Option<ArrayView2<'_, u8>>,
        map_id: i32,
        scale: f64,
        tx: f64,
        ty: f64,
        search_pad: usize,
    ) -> f64 {
        let Some(fg) = frame_gray else {
            return 0.0;
        };
        let Some((res, _, _, _, _, _)) =
            self.match_terrain_patch(fg, map_id, scale, tx, ty, search_pad)
        else {
            return 0.0;
        };
        min_max_loc_f32(res.view()).map_or(0.0, |(_, mx, _, _)| mx as f64)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn refine_transform_with_terrain(
        &self,
        frame_gray: Option<ArrayView2<'_, u8>>,
        map_id: i32,
        scale: f64,
        tx: f64,
        ty: f64,
        search_pad: usize,
        is_underground: bool,
    ) -> (f64, f64, bool) {
        if is_underground {
            return (tx, ty, false);
        }
        let Some(fg) = frame_gray else {
            return (tx, ty, false);
        };
        let Some((res, x0_f, y0_f, rx_a, ry_a, pow2z)) =
            self.match_terrain_patch(fg, map_id, scale, tx, ty, search_pad)
        else {
            return (tx, ty, false);
        };
        let Ok((_, max_val_f, _, (mx, my))) = min_max_loc_f32(res.view()) else {
            return (tx, ty, false);
        };
        let max_val = max_val_f as f64;
        let (rh, rw) = res.dim();
        let mut res_sup = res.clone();
        let sy0 = my.saturating_sub(5);
        let sy1 = (my + 6).min(rh);
        let sx0 = mx.saturating_sub(5);
        let sx1 = (mx + 6).min(rw);
        res_sup.slice_mut(s![sy0..sy1, sx0..sx1]).fill(-1.0);
        let second_val = min_max_loc_f32(res_sup.view()).map_or(-1.0, |(_, m, _, _)| m as f64);
        if max_val < 0.30 || (max_val - second_val) < 0.06 {
            return (tx, ty, false);
        }

        let mut sx_off = 0.0f64;
        if mx > 0 && mx + 1 < rw {
            let v0 = res[[my, mx - 1]] as f64;
            let v1 = res[[my, mx]] as f64;
            let v2 = res[[my, mx + 1]] as f64;
            let denom_x = v0 - 2.0 * v1 + v2;
            if denom_x.abs() > 1e-6 {
                sx_off = (0.5 * (v0 - v2) / denom_x).clamp(-0.5, 0.5);
            }
        }
        let mut sy_off = 0.0f64;
        if my > 0 && my + 1 < rh {
            let v0 = res[[my - 1, mx]] as f64;
            let v1 = res[[my, mx]] as f64;
            let v2 = res[[my + 1, mx]] as f64;
            let denom_y = v0 - 2.0 * v1 + v2;
            if denom_y.abs() > 1e-6 {
                sy_off = (0.5 * (v0 - v2) / denom_y).clamp(-0.5, 0.5);
            }
        }

        let dwx = ((rx_a as f64 + mx as f64 + sx_off) - x0_f) / pow2z;
        let dwy = ((ry_a as f64 + my as f64 + sy_off) - y0_f) / pow2z;
        if dwx.abs() > 8.0 || dwy.abs() > 8.0 {
            return (tx, ty, false);
        }
        (tx + dwx, ty + dwy, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn match_single_map<'a>(
        &self,
        candidates: &[(f64, f64, i32)],
        map_id: i32,
        all_wps: &'a [WaypointEntry],
        allowed_wp_indices: &[usize],
        cand_limit: usize,
        frame_w: usize,
        base_scale: f64,
        target_area_id: Option<i32>,
        arrow_pos: Option<(f64, f64)>,
        allow_2point: bool,
        strict_scale: Option<f64>,
        frame_gray: Option<ArrayView2<'_, u8>>,
    ) -> Option<MatchResult<'a>> {
        let n_cands = candidates.len();
        if n_cands < 2 || allowed_wp_indices.len() < 2 {
            return None;
        }
        let edge_idx = self.edge_indices.get(&map_id)?;
        if edge_idx.angles.is_empty() {
            return None;
        }

        let mut w1_allowed = vec![false; edge_idx.n_wps];
        for &idx in allowed_wp_indices {
            if idx < edge_idx.n_wps {
                w1_allowed[idx] = true;
            }
        }

        if let Some(aid) = target_area_id {
            let mut area_valid = vec![false; edge_idx.n_wps];
            let mut seeds = Vec::new();
            for &idx in allowed_wp_indices {
                if all_wps[idx].area_id == aid {
                    area_valid[idx] = true;
                    seeds.push(idx);
                }
            }
            for idx in seeds {
                for &nb_idx in &edge_idx.nb_id_sets[idx] {
                    area_valid[nb_idx] = true;
                }
            }
            let filtered_count = allowed_wp_indices
                .iter()
                .filter(|&&idx| area_valid[idx])
                .count();
            if filtered_count >= 2 {
                for (i, slot) in w1_allowed.iter_mut().enumerate() {
                    *slot = *slot && area_valid[i];
                }
            }
        }

        let (expected_scale, min_scale, max_scale) = if let Some(ss) = strict_scale {
            (ss, ss * 0.85, ss * 1.15)
        } else if frame_w > 0 {
            let fw_ratio = 2560.0 / frame_w as f64;
            (
                base_scale * fw_ratio,
                (base_scale * 0.40 / 1.12) * fw_ratio,
                (base_scale * 2.10 / 1.12) * fw_ratio,
            )
        } else {
            (
                base_scale,
                base_scale * 0.40 / 1.12,
                base_scale * 2.10 / 1.12,
            )
        };

        let cand_lim = n_cands.min(cand_limit);
        let inv_cell = 1.0 / edge_idx.grid_cell;
        let two_pi = 2.0 * std::f64::consts::PI;

        type MatchTuple<'b> = (Vec<((f64, f64, i32), &'b WaypointEntry)>, f64, [f64; 2], f64);
        let mut best_match: Option<MatchTuple<'a>> = None;
        let mut best_score = -99999.0f64;

        struct Hyp3p<'b> {
            score: f64,
            inliers: Vec<((f64, f64, i32), &'b WaypointEntry)>,
            scale: f64,
            tx: f64,
            ty: f64,
            rmse: f64,
        }
        let mut hyps_3p: HashMap<[usize; 3], Hyp3p<'a>> = HashMap::new();

        for i in 0..cand_lim {
            let s1 = candidates[i];
            let (s1x, s1y, _) = s1;
            for (j, &s2) in candidates.iter().enumerate().take(cand_lim).skip(i + 1) {
                let ds_x = s2.0 - s1x;
                let ds_y = s2.1 - s1y;
                let dist_s = ds_x.hypot(ds_y);
                if dist_s < 55.0 {
                    continue;
                }
                let angle_s = ds_y.atan2(ds_x);
                let min_w_dist = min_scale * dist_s;
                let max_w_dist = max_scale * dist_s;

                let hits = query_edge_index(edge_idx, &w1_allowed, angle_s, 0.048, min_w_dist, max_w_dist);
                for hidx in hits {
                    let w1_idx = edge_idx.w1_dense[hidx];
                    let w2_idx = edge_idx.w2_dense[hidx];
                    let dist_w = edge_idx.dists[hidx];
                    let angle_w = edge_idx.angles[hidx];
                    let diff = ((angle_w - angle_s + std::f64::consts::PI).rem_euclid(two_pi)
                        - std::f64::consts::PI)
                        .abs();
                    if diff > 0.048 {
                        continue;
                    }
                    let scale = dist_w / dist_s;
                    let w1 = &all_wps[w1_idx];
                    let w2 = &all_wps[w2_idx];
                    let w1x = w1.x_pos;
                    let w1y = w1.y_pos;

                    let mut inliers = Vec::with_capacity(6);
                    inliers.push((s1, w1));
                    inliers.push((s2, w2));
                    let mut used = Vec::with_capacity(6);
                    used.push(w1_idx);
                    used.push(w2_idx);
                    let box_tol = 6.0f64.max(12.0 * scale);
                    let nb_ids = &edge_idx.nb_id_sets[w1_idx];

                    for (k, &sk) in candidates.iter().enumerate().take(cand_lim) {
                        if k == i || k == j {
                            continue;
                        }
                        let exp_wx = w1x + (sk.0 - s1x) * scale;
                        let exp_wy = w1y + (sk.1 - s1y) * scale;

                        let gx0 = ((exp_wx - box_tol) * inv_cell).floor() as i32;
                        let gx1 = ((exp_wx + box_tol) * inv_cell).floor() as i32;
                        let gy0 = ((exp_wy - box_tol) * inv_cell).floor() as i32;
                        let gy1 = ((exp_wy + box_tol) * inv_cell).floor() as i32;

                        let mut closest_d = 99999.0f64;
                        let mut closest_idx: Option<usize> = None;

                        for gx in gx0..=gx1 {
                            for gy in gy0..=gy1 {
                                let Some(bucket) = edge_idx.grid.get(&(gx, gy)) else {
                                    continue;
                                };
                                for &wc_idx in bucket {
                                    if used.contains(&wc_idx) || !nb_ids.contains(&wc_idx) {
                                        continue;
                                    }
                                    let wc = &all_wps[wc_idx];
                                    let dx = wc.x_pos - exp_wx;
                                    if dx >= box_tol || dx <= -box_tol {
                                        continue;
                                    }
                                    let dy = wc.y_pos - exp_wy;
                                    if dy >= box_tol || dy <= -box_tol {
                                        continue;
                                    }
                                    let d = dx.hypot(dy);
                                    if d < closest_d {
                                        closest_d = d;
                                        closest_idx = Some(wc_idx);
                                    }
                                }
                            }
                        }

                        if closest_d < box_tol
                            && let Some(cidx) = closest_idx
                        {
                            inliers.push((sk, &all_wps[cidx]));
                            used.push(cidx);
                        }
                    }

                    let n_inl = inliers.len();
                    if n_inl >= 3 {
                        let mut area_counts: HashMap<i32, usize> = HashMap::new();
                        let mut area_total = 0usize;
                        for &(_, wp) in &inliers {
                            if wp.area_id != 0 {
                                *area_counts.entry(wp.area_id).or_insert(0) += 1;
                                area_total += 1;
                            }
                        }
                        let dom_area = area_counts
                            .iter()
                            .max_by_key(|&(_, &c)| c)
                            .map(|(&aid, _)| aid);
                        if area_total > 0
                            && let Some(da) = dom_area
                        {
                            let dom_ratio = area_counts[&da] as f64 / area_total as f64;
                            if (n_inl <= 4 && dom_ratio < 1.0)
                                || (n_inl >= 5 && dom_ratio < 0.75)
                            {
                                continue;
                            }
                        }

                        let tx = inliers
                            .iter()
                            .map(|&(s, w)| w.x_pos - scale * s.0)
                            .sum::<f64>()
                            / n_inl as f64;
                        let ty = inliers
                            .iter()
                            .map(|&(s, w)| w.y_pos - scale * s.1)
                            .sum::<f64>()
                            / n_inl as f64;

                        let mut sum_sq_err = 0.0f64;
                        for &(s, w) in &inliers {
                            let ex = scale * s.0 + tx - w.x_pos;
                            let ey = scale * s.1 + ty - w.y_pos;
                            sum_sq_err += ex * ex + ey * ey;
                        }
                        let rmse = (sum_sq_err / n_inl as f64).sqrt();

                        let mut max_rmse = if n_inl >= 4 {
                            4.5
                        } else if strict_scale.is_some() {
                            3.5f64.max(4.2 * scale)
                        } else {
                            2.8
                        };
                        if target_area_id.is_some() && dom_area == target_area_id && n_inl == 3 {
                            max_rmse = max_rmse.max(4.0);
                        }
                        if rmse > max_rmse {
                            continue;
                        }
                        if strict_scale.is_none()
                            && n_inl == 3
                            && !(expected_scale * 0.60..=expected_scale * 1.70).contains(&scale)
                        {
                            continue;
                        }

                        let total_white: i32 = inliers.iter().map(|(s, _)| s.2).sum();
                        let scale_dev = (scale - expected_scale).abs() / expected_scale.max(1e-6);
                        let scale_penalty = scale_dev
                            * if strict_scale.is_some() { 320.0 } else { 40.0 };
                        let area_bonus = if target_area_id.is_some() {
                            if dom_area == target_area_id {
                                220.0
                            } else {
                                -150.0
                            }
                        } else {
                            0.0
                        };
                        let base_score = n_inl as f64 * 160.0 + (total_white as f64 * 0.08)
                            - (rmse * 5.0)
                            - scale_penalty
                            + area_bonus;

                        if n_inl == 3 && strict_scale.is_some() && frame_gray.is_some() {
                            let mut key = [used[0], used[1], used[2]];
                            key.sort_unstable();
                            let replace = hyps_3p
                                .get(&key)
                                .is_none_or(|prev| base_score > prev.score);
                            if replace {
                                hyps_3p.insert(
                                    key,
                                    Hyp3p {
                                        score: base_score,
                                        inliers,
                                        scale,
                                        tx,
                                        ty,
                                        rmse,
                                    },
                                );
                            }
                            continue;
                        }

                        if base_score > best_score {
                            best_score = base_score;
                            best_match = Some((inliers, scale, [tx, ty], rmse));

                            if (n_inl >= 5 && rmse <= 3.5 && scale_dev <= 0.16)
                                || (n_inl >= 4 && rmse <= 2.4 && scale_dev <= 0.14)
                                || (n_inl == 3
                                    && rmse <= 1.2
                                    && scale_dev <= 0.10
                                    && target_area_id.is_some()
                                    && dom_area == target_area_id)
                            {
                                return best_match;
                            }
                        }
                    }
                }
            }
        }

        if !hyps_3p.is_empty() && (best_match.is_none() || best_score < 700.0) {
            let mut top_3p: Vec<([usize; 3], Hyp3p<'a>)> = hyps_3p.into_iter().collect();
            top_3p.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));
            for (_, h3) in top_3p.into_iter().take(12) {
                let t_bonus =
                    self.score_terrain_alignment(frame_gray, map_id, h3.scale, h3.tx, h3.ty, 8)
                        * 450.0;
                let total_3 = h3.score + t_bonus;
                if total_3 > best_score {
                    best_score = total_3;
                    best_match = Some((h3.inliers, h3.scale, [h3.tx, h3.ty], h3.rmse));
                }
            }
        }

        // Ranked 2-point fallback
        if best_match.is_none()
            && candidates.len() >= 2
            && (target_area_id.is_some() || allow_2point)
        {
            let cand_lim2 = candidates.len().min(8);
            let max_dev_allowed = if strict_scale.is_some() { 0.18 } else { 0.35 };
            let mut hyps_2p: Vec<Hyp2p<'a>> = Vec::new();

            for i in 0..cand_lim2 {
                let s1 = candidates[i];
                for &s2 in candidates.iter().take(cand_lim2).skip(i + 1) {
                    let ds_x = s2.0 - s1.0;
                    let ds_y = s2.1 - s1.1;
                    let dist_s = ds_x.hypot(ds_y);
                    if dist_s < 75.0 {
                        continue;
                    }
                    let angle_s = ds_y.atan2(ds_x);
                    let hits = query_edge_index(
                        edge_idx,
                        &w1_allowed,
                        angle_s,
                        0.035,
                        min_scale * dist_s,
                        max_scale * dist_s,
                    );
                    for hidx in hits {
                        let w1 = &all_wps[edge_idx.w1_dense[hidx]];
                        let w2 = &all_wps[edge_idx.w2_dense[hidx]];
                        let dist_w = edge_idx.dists[hidx];
                        let angle_w = edge_idx.angles[hidx];
                        let diff = ((angle_w - angle_s + std::f64::consts::PI).rem_euclid(two_pi)
                            - std::f64::consts::PI)
                            .abs();
                        if diff > 0.035 {
                            continue;
                        }
                        if let Some(aid) = target_area_id
                            && w1.area_id != aid
                            && w2.area_id != aid
                        {
                            continue;
                        }
                        let scale = dist_w / dist_s;
                        let scale_dev = (scale - expected_scale).abs() / expected_scale.max(1e-6);
                        if scale_dev > max_dev_allowed {
                            continue;
                        }
                        let geom_score = 250.0 - scale_dev * 180.0 - diff * 500.0;
                        let tx = 0.5 * ((w1.x_pos - scale * s1.0) + (w2.x_pos - scale * s2.0));
                        let ty = 0.5 * ((w1.y_pos - scale * s1.1) + (w2.y_pos - scale * s2.1));
                        hyps_2p.push((geom_score, vec![(s1, w1), (s2, w2)], scale, [tx, ty]));
                    }
                }
            }

            if !hyps_2p.is_empty() {
                hyps_2p.sort_by(|a, b| b.0.total_cmp(&a.0));
                let last_pos = self.last_known_pos();
                let mut best_2p_final: Option<MatchTuple<'a>> = None;
                let mut best_2p_score = -999999.0f64;
                for (g_sc, inl_2p, sc_2p, t_2p) in hyps_2p.into_iter().take(10) {
                    let t_sc = if frame_gray.is_some() {
                        self.score_terrain_alignment(frame_gray, map_id, sc_2p, t_2p[0], t_2p[1], 8)
                    } else {
                        0.0
                    };
                    let mut prev_bonus = 0.0f64;
                    if let (Some(lp), Some(ap)) = (last_pos, arrow_pos) {
                        let pwx = sc_2p * ap.0 + t_2p[0];
                        let pwy = sc_2p * ap.1 + t_2p[1];
                        let dp = (pwx - lp.0).hypot(pwy - lp.1);
                        if dp <= 800.0 {
                            prev_bonus = (800.0 - dp) * 0.25;
                        }
                    }
                    let total_sc = g_sc + t_sc * 650.0 + prev_bonus;
                    if total_sc > best_2p_score {
                        best_2p_score = total_sc;
                        best_2p_final = Some((inl_2p, sc_2p, t_2p, 0.5));
                    }
                }
                if best_2p_final.is_some() {
                    return best_2p_final;
                }
            }
        }

        best_match
    }

    #[allow(clippy::too_many_arguments)]
    fn match_single_waypoint_rigid<'a>(
        &self,
        candidate_pt: (f64, f64),
        arrow_pos: (f64, f64),
        scale: f64,
        all_wps: &'a [WaypointEntry],
        allowed_wp_indices: &[usize],
        frame_shape: (usize, usize),
        target_area_id: Option<i32>,
        strict_scale: Option<f64>,
        frame_gray: Option<ArrayView2<'_, u8>>,
        map_id: i32,
    ) -> Option<MatchResult<'a>> {
        let (h, w) = frame_shape;
        let (cx, cy) = candidate_pt;
        let (px, py) = arrow_pos;
        let eff_scale = strict_scale.unwrap_or(scale);

        let area_pool: Vec<&'a WaypointEntry> = if let Some(aid) = target_area_id {
            let ap: Vec<&'a WaypointEntry> = allowed_wp_indices
                .iter()
                .map(|&i| &all_wps[i])
                .filter(|wp| wp.area_id == aid)
                .collect();
            if ap.is_empty() {
                allowed_wp_indices.iter().map(|&i| &all_wps[i]).collect()
            } else {
                ap
            }
        } else {
            allowed_wp_indices.iter().map(|&i| &all_wps[i]).collect()
        };

        let last_pos = self.last_known_pos();
        struct Hyp1p<'b> {
            wp: &'b WaypointEntry,
            tx: f64,
            ty: f64,
            unseen_count: usize,
            dist_prev: Option<f64>,
            terrain_score: f64,
            score: f64,
        }

        let mut valid_hypotheses = Vec::with_capacity(area_pool.len());
        let margin = 65.0 * eff_scale;
        for &wp in &area_pool {
            let tx = wp.x_pos - eff_scale * cx;
            let ty = wp.y_pos - eff_scale * cy;
            let view_min_x = tx + margin;
            let view_max_x = eff_scale * w as f64 + tx - margin;
            let view_min_y = ty + margin;
            let view_max_y = eff_scale * h as f64 + ty - margin;

            let mut unseen_count = 0usize;
            for &other_wp in &area_pool {
                if other_wp.id == wp.id {
                    continue;
                }
                if (view_min_x..=view_max_x).contains(&other_wp.x_pos)
                    && (view_min_y..=view_max_y).contains(&other_wp.y_pos)
                {
                    unseen_count += 1;
                }
            }

            let player_wx = eff_scale * px + tx;
            let player_wy = eff_scale * py + ty;
            let mut score = 1000.0 - (unseen_count as f64 * 250.0);
            let dist_prev = last_pos.map(|lp| {
                let d = (player_wx - lp.0).hypot(player_wy - lp.1);
                if d <= 600.0 {
                    score += (600.0 - d) * 2.0;
                } else {
                    score -= (d - 600.0) * 0.5;
                }
                d
            });
            valid_hypotheses.push(Hyp1p {
                wp,
                tx,
                ty,
                unseen_count,
                dist_prev,
                terrain_score: 0.0,
                score,
            });
        }

        if valid_hypotheses.is_empty() {
            return None;
        }
        valid_hypotheses.sort_by(|a, b| b.score.total_cmp(&a.score));

        if frame_gray.is_some() && self.has_terrain_for_map(map_id) {
            let mut eval_count = 0usize;
            for hyp in &mut valid_hypotheses {
                if hyp.unseen_count <= 1 {
                    let tsc =
                        self.score_terrain_alignment(frame_gray, map_id, eff_scale, hyp.tx, hyp.ty, 8);
                    hyp.terrain_score = tsc;
                    hyp.score += tsc * 700.0;
                    eval_count += 1;
                    if eval_count >= 12 {
                        break;
                    }
                }
            }
            valid_hypotheses.sort_by(|a, b| b.score.total_cmp(&a.score));
        }

        let best = &valid_hypotheses[0];
        let accepted = if (valid_hypotheses.len() == 1 && best.unseen_count <= 1)
            || (last_pos.is_some() && best.dist_prev.is_some_and(|d| d <= 500.0))
        {
            true
        } else if valid_hypotheses.len() > 1 {
            let second_best = &valid_hypotheses[1];
            let t_margin = best.terrain_score - second_best.terrain_score;
            (best.unseen_count == 0 && (best.score - second_best.score) >= 120.0)
                || (best.terrain_score >= 0.22 && t_margin >= 0.07 && best.unseen_count <= 1)
        } else {
            best.unseen_count == 0
        };

        if !accepted {
            return None;
        }
        let inliers = vec![((cx, cy, 500), best.wp)];
        Some((inliers, eff_scale, [best.tx, best.ty], 0.0))
    }

    fn match_zero_waypoint_terrain<'a>(
        &self,
        frame_gray: ArrayView2<'_, u8>,
        map_id: i32,
        scale: f64,
        target_area_id: Option<i32>,
    ) -> Option<MatchResult<'a>> {
        if scale <= 0.0 {
            return None;
        }
        let raster = self.load_terrain_raster(map_id)?;
        let [tx0, ty0, z, orig_x, orig_y] = raster.meta;
        let pow2z = 2.0f64.powf(z);
        let (ch_h, ch_w) = raster.gray.dim();
        let (fh, fw) = frame_gray.dim();
        if fh == 0 || fw == 0 {
            return None;
        }

        let rx0 = (0.16 * fw as f64) as usize;
        let rx1 = (0.84 * fw as f64) as usize;
        let ry0 = (0.14 * fh as f64) as usize;
        let ry1 = (0.86 * fh as f64) as usize;
        if rx1 <= rx0 || ry1 <= ry0 {
            return None;
        }
        let crop = frame_gray.slice(s![ry0..ry1, rx0..rx1]);
        if array_std_u8(crop) < 8.0 {
            return None;
        }

        let cw = (((rx1 - rx0) as f64 * scale * pow2z).round_ties_even() as usize).max(24);
        let ch = (((ry1 - ry0) as f64 * scale * pow2z).round_ties_even() as usize).max(24);
        if cw >= ch_w || ch >= ch_h {
            return None;
        }

        let last_pos = self.last_known_pos();
        // Whole-Teyvat coarse-to-fine fallback (computed on demand without keeping 117 MB cached)
        if target_area_id.is_none() && map_id == 2 && last_pos.is_none() {
            let ds = 0.5;
            let dw = ((ch_w as f64 * ds).round_ties_even() as usize).max(32);
            let dh = ((ch_h as f64 * ds).round_ties_even() as usize).max(32);
            let c_small = resize_u8(raster.gray.view(), dw, dh, Interpolation::Area).ok()?;
            let cw_s = ((cw as f64 * ds).round_ties_even() as usize).max(16);
            let ch_s = ((ch as f64 * ds).round_ties_even() as usize).max(16);
            let (sh, sw) = c_small.dim();
            if cw_s + 4 >= sw || ch_s + 4 >= sh {
                return None;
            }
            let res_c = {
                let rg_small = sobel_magnitude_u8_f32(c_small.view()).ok()?;
                let q_small = resize_u8(crop, cw_s, ch_s, Interpolation::Area).ok()?;
                let qg_small = sobel_magnitude_u8_f32(q_small.view()).ok()?;
                fast_match_template_ccoeff_f32(rg_small.view(), qg_small.view())?
            };
            let valid_mask = compute_coarse_valid_mask(c_small.view(), cw_s, ch_s)?;
            drop(c_small);

            let (rh_c, rw_c) = res_c.dim();
            let mut res_clean = Array2::<f32>::from_elem((rh_c, rw_c), -1.0f32);
            for oy in 0..rh_c {
                for ox in 0..rw_c {
                    if valid_mask[[oy, ox]] != 0 {
                        res_clean[[oy, ox]] = res_c[[oy, ox]];
                    }
                }
            }

            let (_, peak_c_f, _, peak_loc_c) = min_max_loc_f32(res_clean.view()).ok()?;
            let peak_c = peak_c_f as f64;
            if peak_c < 0.34 {
                return None;
            }
            let mut res_sup = res_clean;
            let sy0 = peak_loc_c.1.saturating_sub(20);
            let sy1 = (peak_loc_c.1 + 20).min(rh_c);
            let sx0 = peak_loc_c.0.saturating_sub(20);
            let sx1 = (peak_loc_c.0 + 20).min(rw_c);
            res_sup.slice_mut(s![sy0..sy1, sx0..sx1]).fill(-1.0);
            let second_c =
                min_max_loc_f32(res_sup.view()).map_or(-1.0, |(_, m, _, _)| m as f64);
            if (peak_c - second_c) < 0.10 {
                return None;
            }

            let bx = peak_loc_c.0 as f64 / ds;
            let by = peak_loc_c.1 as f64 / ds;
            let wx_rx0 = (bx + tx0 * 256.0) / pow2z - orig_x;
            let wy_ry0 = (by + ty0 * 256.0) / pow2z - orig_y;
            let mut tx = wx_rx0 - scale * rx0 as f64;
            let mut ty = wy_ry0 - scale * ry0 as f64;
            let (tx_ref, ty_ref, ok_ref) =
                self.refine_transform_with_terrain(Some(frame_gray), map_id, scale, tx, ty, 16, false);
            if ok_ref {
                tx = tx_ref;
                ty = ty_ref;
            }
            return Some((Vec::new(), scale, [tx, ty], 0.0));
        }

        let (wx_min, wx_max, wy_min, wy_max) = if target_area_id.is_none()
            && map_id == 2
            && let Some(lp) = last_pos
        {
            (lp.0 - 1600.0, lp.0 + 1600.0, lp.1 - 1600.0, lp.1 + 1600.0)
        } else {
            let bb = self
                .area_bboxes
                .get(&(map_id, target_area_id))
                .or_else(|| self.area_bboxes.get(&(map_id, None)))?;
            (bb[0] - 700.0, bb[1] + 700.0, bb[2] - 700.0, bb[3] + 700.0)
        };

        let cx0 = (((wx_min + orig_x) * pow2z - tx0 * 256.0).floor() as isize).max(0) as usize;
        let cy0 = (((wy_min + orig_y) * pow2z - ty0 * 256.0).floor() as isize).max(0) as usize;
        let cx1 = (((wx_max + orig_x) * pow2z - tx0 * 256.0).ceil() as isize)
            .min(ch_w as isize)
            .max(0) as usize;
        let cy1 = (((wy_max + orig_y) * pow2z - ty0 * 256.0).ceil() as isize)
            .min(ch_h as isize)
            .max(0) as usize;

        if cx1 <= cx0 + cw + 8 || cy1 <= cy0 + ch + 8 {
            return None;
        }
        let roi = raster.gray.slice(s![cy0..cy1, cx0..cx1]);
        let (roi_h, roi_w) = roi.dim();
        let mut use_coarse = roi_h.max(roi_w) > 360 && cw >= 48 && ch >= 48;
        let mut ds = if use_coarse { 0.5 } else { 1.0 };
        let mut roi_w_s = 0usize;
        let mut roi_h_s = 0usize;
        let mut cw_s = 0usize;
        let mut ch_s = 0usize;
        if use_coarse {
            roi_w_s = ((roi_w as f64 * ds).round_ties_even() as usize).max(16);
            roi_h_s = ((roi_h as f64 * ds).round_ties_even() as usize).max(16);
            cw_s = ((cw as f64 * ds).round_ties_even() as usize).max(16);
            ch_s = ((ch as f64 * ds).round_ties_even() as usize).max(16);
            if roi_w_s <= cw_s + 4 || roi_h_s <= ch_s + 4 {
                use_coarse = false;
                ds = 1.0;
            }
        }

        let (roi_work, q_work, kw, kh) = if use_coarse {
            (
                resize_u8(roi, roi_w_s, roi_h_s, Interpolation::Area).ok()?,
                resize_u8(crop, cw_s, ch_s, Interpolation::Area).ok()?,
                cw_s,
                ch_s,
            )
        } else {
            (
                roi.to_owned(),
                resize_u8(crop, cw, ch, Interpolation::Area).ok()?,
                cw,
                ch,
            )
        };

        let qg = sobel_magnitude_u8_f32(q_work.view()).ok()?;
        let rg = sobel_magnitude_u8_f32(roi_work.view()).ok()?;
        let res = fast_match_template_ccoeff_f32(rg.view(), qg.view())?;

        let nz = roi_work.mapv(|v| if v > 2 { 1.0f32 } else { 0.0f32 });
        let rf = roi_work.mapv(|v| v as f32);
        let rf2 = rf.mapv(|v| v * v);
        let nz_frac = box_filter_f32(nz.view(), kw, kh, true).ok()?;
        let mean_r = box_filter_f32(rf.view(), kw, kh, true).ok()?;
        let mean2_r = box_filter_f32(rf2.view(), kw, kh, true).ok()?;

        let hc = kh / 2;
        let wc = kw / 2;
        let (rh, rw) = res.dim();
        let mut res_clean = Array2::<f32>::from_elem((rh, rw), -1.0f32);
        for oy in 0..rh {
            for ox in 0..rw {
                let nzf = nz_frac[[hc + oy, wc + ox]];
                let m = mean_r[[hc + oy, wc + ox]];
                let m2 = mean2_r[[hc + oy, wc + ox]];
                let std_v = (m2 - m * m).max(0.0).sqrt();
                if nzf >= 0.85 && std_v >= 8.0 {
                    res_clean[[oy, ox]] = res[[oy, ox]];
                }
            }
        }

        let (_, peak_f, _, peak_loc) = min_max_loc_f32(res_clean.view()).ok()?;
        let peak = peak_f as f64;
        if peak < 0.24 {
            return None;
        }
        let mut res_sup = res_clean;
        let sup_r = ((40.0 * ds).round_ties_even() as usize).max(12);
        let sy0 = peak_loc.1.saturating_sub(sup_r);
        let sy1 = (peak_loc.1 + sup_r).min(rh);
        let sx0 = peak_loc.0.saturating_sub(sup_r);
        let sx1 = (peak_loc.0 + sup_r).min(rw);
        res_sup.slice_mut(s![sy0..sy1, sx0..sx1]).fill(-1.0);
        let second_peak = min_max_loc_f32(res_sup.view()).map_or(-1.0, |(_, m, _, _)| m as f64);
        if (peak - second_peak) < 0.07 {
            return None;
        }

        let best_cx = cx0 as f64 + peak_loc.0 as f64 / ds;
        let best_cy = cy0 as f64 + peak_loc.1 as f64 / ds;
        let wx_rx0 = (best_cx + tx0 * 256.0) / pow2z - orig_x;
        let wy_ry0 = (best_cy + ty0 * 256.0) / pow2z - orig_y;
        let mut tx = wx_rx0 - scale * rx0 as f64;
        let mut ty = wy_ry0 - scale * ry0 as f64;
        if use_coarse {
            let (tx_ref, ty_ref, ok_ref) =
                self.refine_transform_with_terrain(Some(frame_gray), map_id, scale, tx, ty, 12, false);
            if ok_ref {
                tx = tx_ref;
                ty = ty_ref;
            }
        }
        Some((Vec::new(), scale, [tx, ty], 0.0))
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_exact_hoyolab_floor(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        map_id: i32,
        world_x: f64,
        world_y: f64,
        scale: f64,
        tx: f64,
        ty: f64,
        layer_info: &LayerInfo,
        inliers: &[((f64, f64, i32), &WaypointEntry)],
        frame_v_u8: ArrayView2<'_, u8>,
    ) -> FloorResolveResult {
        if !layer_info.is_underground {
            return FloorResolveResult::default();
        }
        let groups: Vec<&PointGroupEntry> = if let Some(m) = self.db.map(map_id)
            && !m.point_groups.is_empty()
        {
            m.point_groups.iter().collect()
        } else {
            self.underground_groups
                .iter()
                .filter(|g| g.map_id == map_id || g.map_id == 0)
                .collect()
        };
        if groups.is_empty() {
            return FloorResolveResult::default();
        }

        let mut inlier_group_ids = HashSet::new();
        let mut inlier_floor_ids = HashSet::new();
        for &(_, w) in inliers {
            if let Some(pg) = &w.point_group {
                inlier_group_ids.insert(pg.group_id);
                if let Some(fid) = pg.floor_id {
                    inlier_floor_ids.insert(fid);
                }
            }
        }

        let mut cand_groups = Vec::new();
        for g in groups {
            let in_bbox = g.floors.iter().any(|fl| {
                fl.overlay.as_ref().is_some_and(|ov| {
                    let (min_lng, max_lng, min_lat, max_lat) = ov.bounds();
                    (min_lng - 40.0..=max_lng + 40.0).contains(&world_x)
                        && (min_lat - 40.0..=max_lat + 40.0).contains(&world_y)
                })
            });
            if in_bbox {
                cand_groups.push(g);
            }
        }
        if cand_groups.is_empty() {
            return FloorResolveResult::default();
        }

        let (h, w, _) = frame_bgr.dim();
        let total_floors = layer_info.total_floors.max(1);
        let active_idx = layer_info.active_floor_idx;
        let surface_idx = layer_info.surface_idx;
        let target_f_idx = if surface_idx == 0 {
            active_idx.saturating_sub(1)
        } else {
            active_idx
        };
        let target_short = layer_info.floor_name.trim().to_ascii_uppercase();

        let mut best_choice: Option<(f64, &PointGroupEntry, &gh_core::multimap::FloorEntry)> = None;
        for g in cand_groups {
            if g.floors.is_empty() {
                continue;
            }
            let expected_total = g.floors.len() + 1;
            let diff_total = expected_total.abs_diff(total_floors);
            let mut group_score = if diff_total == 0 {
                350.0
            } else {
                -120.0 * diff_total as f64
            };
            if inlier_group_ids.contains(&g.id) {
                group_score += 200.0;
            }

            for (f_idx, fl) in g.floors.iter().enumerate() {
                let Some(ov) = &fl.overlay else {
                    continue;
                };
                let (min_lng, max_lng, min_lat, max_lat) = ov.bounds();
                let mask_key = format!("{map_id}_{}_{}", g.id, fl.id);
                let alpha = self.cave_masks.get(&mask_key);

                let span_x = ov.r_x - ov.l_x;
                let span_y = ov.r_y - ov.l_y;
                let player_in_floor = if let Some(al) = alpha
                    && span_x != 0.0
                    && span_y != 0.0
                {
                    let (mh, mw) = al.dim();
                    let u_p = ((world_x - ov.l_x) / span_x * mw as f64).round_ties_even() as isize;
                    let v_p = ((world_y - ov.l_y) / span_y * mh as f64).round_ties_even() as isize;
                    if u_p >= 0 && u_p < mw as isize && v_p >= 0 && v_p < mh as isize {
                        let py0 = (v_p - 4).max(0) as usize;
                        let py1 = (v_p + 5).min(mh as isize) as usize;
                        let px0 = (u_p - 4).max(0) as usize;
                        let px1 = (u_p + 5).min(mw as isize) as usize;
                        al.any_nonzero_in_rect(py0, py1, px0, px1)
                    } else {
                        false
                    }
                } else {
                    (min_lng..=max_lng).contains(&world_x) && (min_lat..=max_lat).contains(&world_y)
                };

                if !player_in_floor {
                    continue;
                }

                let mut score = group_score + 220.0;
                if f_idx == target_f_idx {
                    score += 260.0;
                }
                if inlier_floor_ids.contains(&fl.id) {
                    score += 80.0;
                }
                let fs = fl.floor_name_short.trim().to_ascii_uppercase();
                if !fs.is_empty() && !target_short.is_empty() && fs == target_short {
                    score += 120.0;
                }

                if let Some(al) = alpha
                    && span_x != 0.0
                    && span_y != 0.0
                {
                    let (mh, mw) = al.dim();
                    let x0 = ((0.05 * w as f64) as isize)
                        .max(((min_lng - tx) / scale).round_ties_even() as isize)
                        .clamp(0, w as isize) as usize;
                    let x1 = ((0.93 * w as f64) as isize)
                        .min(((max_lng - tx) / scale).round_ties_even() as isize)
                        .clamp(0, w as isize) as usize;
                    let y0 = ((0.06 * h as f64) as isize)
                        .max(((min_lat - ty) / scale).round_ties_even() as isize)
                        .clamp(0, h as isize) as usize;
                    let y1 = ((0.92 * h as f64) as isize)
                        .min(((max_lat - ty) / scale).round_ties_even() as isize)
                        .clamp(0, h as isize) as usize;

                    if y1.saturating_sub(y0) >= 8 && x1.saturating_sub(x0) >= 8 {
                        let xs: Vec<usize> = (x0..x1)
                            .map(|x| {
                                (((x as f64 * scale + tx - ov.l_x) / span_x * mw as f64) as isize)
                                    .clamp(0, mw as isize - 1) as usize
                            })
                            .collect();
                        let mut in_sum = 0.0f64;
                        let mut in_cnt = 0usize;
                        let mut out_sum = 0.0f64;
                        let mut out_cnt = 0usize;
                        for y in y0..y1 {
                            let my = (((y as f64 * scale + ty - ov.l_y) / span_y * mh as f64)
                                as isize)
                                .clamp(0, mh as isize - 1) as usize;
                            for (dx, &mx) in xs.iter().enumerate() {
                                let pv = frame_v_u8[[y, x0 + dx]] as f64;
                                if al.is_nonzero(my, mx) {
                                    in_sum += pv;
                                    in_cnt += 1;
                                } else {
                                    out_sum += pv;
                                    out_cnt += 1;
                                }
                            }
                        }
                        if in_cnt >= 15 && out_cnt >= 15 {
                            let delta = (in_sum / in_cnt as f64) - (out_sum / out_cnt as f64);
                            score += 8.0 * delta;
                        }
                    }
                }

                if best_choice.is_none_or(|(bs, _, _)| score > bs) {
                    best_choice = Some((score, g, fl));
                }
            }
        }

        let Some((_, g_best, fl_best)) = best_choice else {
            return FloorResolveResult::default();
        };
        let fs_best = fl_best.floor_name_short.trim();
        let fn_best = fl_best.floor_name.trim();
        let floor_short = if !fs_best.is_empty() {
            fs_best.to_owned()
        } else if !fn_best.is_empty() {
            fn_best.to_owned()
        } else {
            target_short.clone()
        };
        let hoyolab_floor_name = if !fn_best.is_empty() {
            fn_best.to_owned()
        } else {
            floor_short.clone()
        };
        let group_name = if !g_best.name.trim().is_empty() {
            g_best.name.clone()
        } else if !fn_best.is_empty() {
            fn_best.to_owned()
        } else {
            format!("Group {}", g_best.id)
        };

        FloorResolveResult {
            is_hoyolab_underground: true,
            group_id: Some(g_best.id),
            group_name: Some(group_name),
            floor_id: Some(fl_best.id),
            floor_short: floor_short.clone(),
            floor_name: floor_short,
            hoyolab_floor_name,
        }
    }

    pub fn locate_player(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        debug: bool,
        slider_res_override: Option<&ZoomSliderResult>,
    ) -> Option<LocateResult> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 {
            return None;
        }
        let bgr3 = if c > 3 {
            frame_bgr.slice(s![.., .., 0..3])
        } else {
            frame_bgr
        };
        let n_px = h * w;
        LOCATOR_IMG_SCRATCH.with(|cell| {
            let mut im = cell.borrow_mut();
            if im.hsv_buf.len() < n_px * 3 {
                im.hsv_buf.resize(n_px * 3, 0);
            }
            if im.gray_buf.len() < n_px {
                im.gray_buf.resize(n_px, 0);
            }
            let im_ref = &mut *im;
            let hsv_s = &mut im_ref.hsv_buf[..n_px * 3];
            let gray_s = &mut im_ref.gray_buf[..n_px];
            fill_hsv_and_gray_into(frame_bgr, hsv_s, gray_s);
            let hsv = ArrayView3::from_shape((h, w, 3), hsv_s).ok()?;
            let frame_gray = ArrayView2::from_shape((h, w), gray_s).ok()?;

        let Some((px, py, angle)) = self.detect_player_arrow(bgr3, Some(hsv.view())) else {
            if debug {
                return Some(LocateResult {
                    detected: false,
                    has_world_coords: false,
                    map_id: None,
                    map_name: None,
                    area_id: None,
                    area_name: None,
                    world_x: None,
                    world_y: None,
                    lat: None,
                    lng: None,
                    pixel_x: 0.0,
                    pixel_y: 0.0,
                    heading: 0.0,
                    scale: None,
                    inliers_count: None,
                    rmse: None,
                    is_underground: false,
                    in_game_is_underground: None,
                    floor_idx: 0,
                    total_floors: 1,
                    active_floor_idx: 0,
                    surface_idx: 0,
                    floor_name: String::new(),
                    floor_short: String::new(),
                    hoyolab_floor_name: None,
                    group_id: None,
                    group_name: None,
                    floor_id: None,
                    has_layers: false,
                    zoom_ratio_t: None,
                    candidates: None,
                    inliers: None,
                    slider_res: None,
                    region_text: None,
                    target_map_id: None,
                    target_area_id: None,
                    slot_types: None,
                    reason: Some(
                        "Không tìm thấy mũi tên người chơi (Player Arrow) trên màn hình."
                            .to_owned(),
                    ),
                    debug_included: true,
                });
            }
            return None;
        };

        let layer_info = self.detect_layers_universal(bgr3);
        let owned_slider;
        let slider_res: Option<&ZoomSliderResult> = match slider_res_override {
            Some(s) => Some(s),
            None => {
                owned_slider = ZoomSliderDetector::detect_slider(bgr3);
                owned_slider.as_ref()
            }
        };
        let zoom_t = slider_res.map(|s| s.ratio_t);
        let known_scale_m2 = ZoomSliderDetector::get_map_scale(slider_res, 2, w);

        let candidates = self.detect_waypoint_candidates_with_gray(
            bgr3,
            Some((px, py)),
            Some(hsv.view()),
            Some(frame_gray.view()),
        );
        if self.db.maps.is_empty() {
            return Some(LocateResult {
                detected: true,
                has_world_coords: false,
                map_id: None,
                map_name: None,
                area_id: None,
                area_name: None,
                world_x: None,
                world_y: None,
                lat: None,
                lng: None,
                pixel_x: px,
                pixel_y: py,
                heading: angle,
                scale: None,
                inliers_count: None,
                rmse: None,
                is_underground: layer_info.is_underground,
                in_game_is_underground: Some(layer_info.is_underground),
                floor_idx: layer_info.active_floor_idx,
                total_floors: layer_info.total_floors,
                active_floor_idx: layer_info.active_floor_idx,
                surface_idx: layer_info.surface_idx,
                floor_name: layer_info.floor_name.clone(),
                floor_short: layer_info.floor_name,
                hoyolab_floor_name: None,
                group_id: None,
                group_name: None,
                floor_id: None,
                has_layers: layer_info.has_layers,
                zoom_ratio_t: zoom_t,
                candidates: if debug { Some(Vec::new()) } else { None },
                inliers: None,
                slider_res: if debug { slider_res.cloned() } else { None },
                region_text: None,
                target_map_id: None,
                target_area_id: None,
                slot_types: None,
                reason: if debug {
                    Some("Không có cơ sở dữ liệu bản đồ.".to_owned())
                } else {
                    None
                },
                debug_included: debug,
            });
        }

        let (target_map_id, target_area_id, region_text) = self.detect_region_name(bgr3);
        let is_ug_hint = layer_info.is_underground;

        let primary_indices = |wps: &[WaypointEntry]| -> Vec<usize> {
            let p: Vec<usize> = wps
                .iter()
                .enumerate()
                .filter_map(|(i, wp)| if wp.is_primary { Some(i) } else { None })
                .collect();
            if p.len() >= 2 {
                p
            } else {
                (0..wps.len()).collect()
            }
        };

        type CandidateMatch<'b> = (
            &'b MapMeta,
            Vec<((f64, f64, i32), &'b WaypointEntry)>,
            f64,
            [f64; 2],
            f64,
        );
        let mut best_candidate_match: Option<CandidateMatch<'_>> = None;
        let best_score = -99999.0f64;

        // Case 0: Single waypoint + strict slider scale
        if candidates.len() == 1 {
            let active_mid = target_map_id
                .filter(|mid| self.db.maps.contains_key(mid))
                .unwrap_or(2);
            if let Some(strict_s) = ZoomSliderDetector::get_map_scale(slider_res, active_mid, w)
                && let Some(mdata) = self.db.map(active_mid)
            {
                let prim_idx = primary_indices(&mdata.waypoints);
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_waypoint_rigid(
                    (candidates[0].0, candidates[0].1),
                    (px, py),
                    strict_s,
                    &mdata.waypoints,
                    &prim_idx,
                    (h, w),
                    target_area_id,
                    Some(strict_s),
                    Some(frame_gray.view()),
                    active_mid,
                ) {
                    best_candidate_match = Some((mdata, inl, sc, t_vec, rmse));
                }
            }
        }

        // Case A: OCR detected isolated sub-map
        if best_candidate_match.is_none()
            && candidates.len() >= 2
            && let Some(t_mid) = target_map_id
            && t_mid != 2
            && let Some(mdata) = self.db.map(t_mid)
        {
            let strict_sub = ZoomSliderDetector::get_map_scale(slider_res, t_mid, w);
            let base_s =
                strict_sub.unwrap_or_else(|| map_base_scale_cfg(t_mid) * (2560.0 / w as f64));
            let all_idx: Vec<usize> = (0..mdata.waypoints.len()).collect();
            let lims: &[usize] = if candidates.len() > 12 { &[12, 18] } else { &[12] };
            for &lim in lims {
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map(
                    &candidates,
                    t_mid,
                    &mdata.waypoints,
                    &all_idx,
                    lim,
                    w,
                    base_s,
                    None,
                    Some((px, py)),
                    true,
                    strict_sub,
                    Some(frame_gray.view()),
                ) {
                    best_candidate_match = Some((mdata, inl, sc, t_vec, rmse));
                    break;
                }
            }
        }

        // Case B: Map 2 (Teyvat)
        if best_candidate_match.is_none()
            && candidates.len() >= 2
            && (target_map_id == Some(2) || target_map_id.is_none())
            && let Some(m2) = self.db.map(2)
        {
            let strict_m2 = known_scale_m2;
            let base_m2 = strict_m2.unwrap_or(1.12);
            let prim_idx = primary_indices(&m2.waypoints);

            if zoom_t.is_some_and(|t| t <= 0.12) {
                let major_idx: Vec<usize> = prim_idx
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let lbl = m2.waypoints[i].label_id;
                        lbl == 2 || lbl == 154
                    })
                    .collect();
                if major_idx.len() >= 2
                    && let Some((inl, sc, t_vec, rmse)) = self.match_single_map(
                        &candidates,
                        2,
                        &m2.waypoints,
                        &major_idx,
                        26,
                        w,
                        base_m2,
                        target_area_id,
                        Some((px, py)),
                        target_area_id.is_some(),
                        strict_m2,
                        Some(frame_gray.view()),
                    )
                {
                    best_candidate_match = Some((m2, inl, sc, t_vec, rmse));
                }
            }

            if best_candidate_match.is_none() {
                let lims: &[usize] = if candidates.len() > 12 { &[12, 18] } else { &[12] };
                for &lim in lims {
                    if let Some((inl, sc, t_vec, rmse)) = self.match_single_map(
                        &candidates,
                        2,
                        &m2.waypoints,
                        &prim_idx,
                        lim,
                        w,
                        base_m2,
                        target_area_id,
                        Some((px, py)),
                        target_area_id.is_some(),
                        strict_m2,
                        Some(frame_gray.view()),
                    ) {
                        best_candidate_match = Some((m2, inl, sc, t_vec, rmse));
                        break;
                    }
                }
            }

            if best_candidate_match.is_none() && m2.waypoints.len() > prim_idx.len() {
                let all_idx: Vec<usize> = (0..m2.waypoints.len()).collect();
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map(
                    &candidates,
                    2,
                    &m2.waypoints,
                    &all_idx,
                    14,
                    w,
                    base_m2,
                    target_area_id,
                    Some((px, py)),
                    target_area_id.is_some(),
                    strict_m2,
                    Some(frame_gray.view()),
                ) {
                    best_candidate_match = Some((m2, inl, sc, t_vec, rmse));
                }
            }
        }

        // Case C: Fallback to isolated submaps without OCR hint
        if best_candidate_match.is_none()
            && candidates.len() >= 2
            && target_map_id.is_none()
            && target_area_id.is_none()
        {
            for &mid in &self.map_order {
                if mid == 2 {
                    continue;
                }
                let Some(mdata) = self.db.map(mid) else {
                    continue;
                };
                let strict_sub = ZoomSliderDetector::get_map_scale(slider_res, mid, w);
                let base_s = strict_sub.unwrap_or_else(|| map_base_scale_cfg(mid));
                let all_idx: Vec<usize> = (0..mdata.waypoints.len()).collect();
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map(
                    &candidates,
                    mid,
                    &mdata.waypoints,
                    &all_idx,
                    10,
                    w,
                    base_s,
                    None,
                    Some((px, py)),
                    false,
                    strict_sub,
                    Some(frame_gray.view()),
                ) {
                    let score = inl.len() as f64 * 150.0 - (rmse * 5.0);
                    if score > best_score {
                        best_candidate_match = Some((mdata, inl, sc, t_vec, rmse));
                        break;
                    }
                }
            }
        }

        // Surface Terrain Head-to-Head Guard
        if let Some((_mdata, ref _inl, _sc, _t_vec, _)) = best_candidate_match
            && !is_ug_hint
            && slider_res.is_some()
            && _inl.len() <= 4
            && self.has_terrain_for_map(_mdata.id)
        {
            let t_sc = self.score_terrain_alignment(
                Some(frame_gray.view()),
                _mdata.id,
                _sc,
                _t_vec[0],
                _t_vec[1],
                8,
            );
            if t_sc < 0.24 {
                let fb_mid = target_map_id
                    .filter(|mid| self.db.maps.contains_key(mid))
                    .unwrap_or(_mdata.id);
                if let Some(fb_scale) = ZoomSliderDetector::get_map_scale(slider_res, fb_mid, w)
                    && let Some(alt_0p) = self.match_zero_waypoint_terrain(
                        frame_gray.view(),
                        fb_mid,
                        fb_scale,
                        target_area_id,
                    )
                {
                    let alt_t = self.score_terrain_alignment(
                        Some(frame_gray.view()),
                        fb_mid,
                        alt_0p.1,
                        alt_0p.2[0],
                        alt_0p.2[1],
                        8,
                    );
                    if alt_t >= 0.40
                        && alt_t >= t_sc + 0.20
                        && let Some(fb_mdata) = self.db.map(fb_mid)
                    {
                        best_candidate_match =
                            Some((fb_mdata, alt_0p.0, alt_0p.1, alt_0p.2, alt_0p.3));
                    }
                }
            }
        }

        // Case D: Zero-waypoint terrain-only fallback
        if best_candidate_match.is_none() && slider_res.is_some() {
            let fallback_mid = target_map_id
                .filter(|mid| self.db.maps.contains_key(mid))
                .or_else(|| self.db.maps.contains_key(&2).then_some(2));
            if let Some(fb_mid) = fallback_mid
                && let Some(fb_scale) = ZoomSliderDetector::get_map_scale(slider_res, fb_mid, w)
                && let Some(res_0p) = self.match_zero_waypoint_terrain(
                    frame_gray.view(),
                    fb_mid,
                    fb_scale,
                    target_area_id,
                )
                && let Some(mdata) = self.db.map(fb_mid)
            {
                best_candidate_match = Some((mdata, res_0p.0, res_0p.1, res_0p.2, res_0p.3));
            }
        }

        if let Some((mdata, inliers, mut scale, mut t_vec, mut rmse)) = best_candidate_match {
            let mut tx = t_vec[0];
            let mut ty = t_vec[1];
            let scale_ui = h as f64 / 1440.0;

            if !inliers.is_empty() {
                let adj_inliers = Self::compensate_inliers_screen_anchors(&inliers, scale_ui);
                if adj_inliers.len() >= 2 {
                    if let Some((lsq_s, lsq_tx, lsq_ty, lsq_rmse)) =
                        Self::fit_isometric_lsq(&adj_inliers)
                        && (lsq_s - scale).abs() / scale.max(1e-6) <= 0.06
                    {
                        scale = lsq_s;
                        tx = lsq_tx;
                        ty = lsq_ty;
                        rmse = lsq_rmse;
                    } else {
                        let nf = adj_inliers.len() as f64;
                        tx = adj_inliers
                            .iter()
                            .map(|&(s, wp)| wp.x_pos - scale * s.0)
                            .sum::<f64>()
                            / nf;
                        ty = adj_inliers
                            .iter()
                            .map(|&(s, wp)| wp.y_pos - scale * s.1)
                            .sum::<f64>()
                            / nf;
                    }
                } else {
                    let (s0, w0) = adj_inliers[0];
                    tx = w0.x_pos - scale * s0.0;
                    ty = w0.y_pos - scale * s0.1;
                }

                if WORLD_ANCHOR_MAP_IDS.contains(&mdata.id) {
                    ty += WORLD_ANCHOR_Y_OFFSET;
                }

                let (tx_ref, ty_ref, terr_ok) = self.refine_transform_with_terrain(
                    Some(frame_gray.view()),
                    mdata.id,
                    scale,
                    tx,
                    ty,
                    12,
                    layer_info.is_underground,
                );
                if terr_ok {
                    tx = tx_ref;
                    ty = ty_ref;
                }
                let _ = &mut t_vec;
            }

            let world_x = scale * px + tx;
            let world_y = scale * py + ty;

            let mut area_id: Option<i32> = None;
            let mut area_name: Option<String> = None;
            if mdata.id == 2 {
                let mut counts: HashMap<i32, usize> = HashMap::new();
                for &(_, wp) in &inliers {
                    if wp.area_id != 0 {
                        *counts.entry(wp.area_id).or_insert(0) += 1;
                    }
                }
                if !counts.is_empty() {
                    area_id = counts
                        .into_iter()
                        .max_by_key(|&(aid, c)| (c, std::cmp::Reverse(aid)))
                        .map(|(a, _)| a);
                } else if target_area_id.is_some() {
                    area_id = target_area_id;
                } else if !mdata.waypoints.is_empty() {
                    area_id = mdata
                        .waypoints
                        .iter()
                        .min_by(|a, b| {
                            let da = (a.x_pos - world_x).powi(2) + (a.y_pos - world_y).powi(2);
                            let db = (b.x_pos - world_x).powi(2) + (b.y_pos - world_y).powi(2);
                            da.total_cmp(&db)
                        })
                        .map(|wp| wp.area_id)
                        .filter(|&aid| aid != 0);
                }
                if let Some(aid) = area_id {
                    area_name = Some(
                        self.area_names_map
                            .get(&aid)
                            .cloned()
                            .unwrap_or_else(|| format!("Area {aid}")),
                    );
                }
            }

            let frame_v_u8 = hsv.slice(s![.., .., 2]);
            let floor_resolved = self.resolve_exact_hoyolab_floor(
                bgr3,
                mdata.id,
                world_x,
                world_y,
                scale,
                tx,
                ty,
                &layer_info,
                &inliers,
                frame_v_u8,
            );

            if let Ok(mut guard) = self.tracking.lock() {
                guard.last_known_pos = Some((world_x, world_y));
                guard.last_map_id = Some(mdata.id);
            }

            let is_ug = if layer_info.is_underground {
                floor_resolved.is_hoyolab_underground
            } else {
                false
            };

            return Some(LocateResult {
                detected: true,
                has_world_coords: true,
                map_id: Some(mdata.id),
                map_name: Some(mdata.name.clone()),
                area_id,
                area_name,
                world_x: Some(world_x),
                world_y: Some(world_y),
                lat: Some(world_y),
                lng: Some(world_x),
                pixel_x: px,
                pixel_y: py,
                heading: angle,
                scale: Some(scale),
                inliers_count: Some(inliers.len()),
                rmse: Some(rmse),
                is_underground: is_ug,
                in_game_is_underground: Some(layer_info.is_underground),
                floor_idx: layer_info.active_floor_idx,
                total_floors: layer_info.total_floors,
                active_floor_idx: layer_info.active_floor_idx,
                surface_idx: layer_info.surface_idx,
                floor_name: floor_resolved.floor_name,
                floor_short: floor_resolved.floor_short,
                hoyolab_floor_name: Some(floor_resolved.hoyolab_floor_name),
                group_id: floor_resolved.group_id,
                group_name: floor_resolved.group_name,
                floor_id: floor_resolved.floor_id,
                has_layers: layer_info.has_layers,
                zoom_ratio_t: zoom_t,
                candidates: if debug {
                    Some(
                        candidates
                            .iter()
                            .map(|c_item| LocateCandidateDebug {
                                x: c_item.0,
                                y: c_item.1,
                                r: c_item.2 as f64,
                            })
                            .collect(),
                    )
                } else {
                    None
                },
                inliers: if debug {
                    Some(
                        inliers
                            .iter()
                            .map(|&(s, wp)| LocateInlierDebug {
                                pixel_x: s.0,
                                pixel_y: s.1,
                                waypoint_id: wp.id,
                                world_x: wp.x_pos,
                                world_y: wp.y_pos,
                                name: format!("WP_{}", wp.id),
                            })
                            .collect(),
                    )
                } else {
                    None
                },
                slider_res: if debug { slider_res.cloned() } else { None },
                region_text: if debug { Some(region_text) } else { None },
                target_map_id: if debug { target_map_id } else { None },
                target_area_id: if debug { target_area_id } else { None },
                slot_types: if debug {
                    Some(layer_info.slot_types)
                } else {
                    None
                },
                reason: if debug {
                    Some(format!(
                        "Định vị thành công Map {} ({}) qua {} mốc.",
                        mdata.id,
                        mdata.name,
                        inliers.len()
                    ))
                } else {
                    None
                },
                debug_included: debug,
            });
        }

        Some(LocateResult {
            detected: true,
            has_world_coords: false,
            map_id: None,
            map_name: None,
            area_id: None,
            area_name: None,
            world_x: None,
            world_y: None,
            lat: None,
            lng: None,
            pixel_x: px,
            pixel_y: py,
            heading: angle,
            scale: None,
            inliers_count: None,
            rmse: None,
            is_underground: layer_info.is_underground,
            in_game_is_underground: None,
            floor_idx: layer_info.active_floor_idx,
            total_floors: layer_info.total_floors,
            active_floor_idx: layer_info.active_floor_idx,
            surface_idx: layer_info.surface_idx,
            floor_name: layer_info.floor_name.clone(),
            floor_short: layer_info.floor_name,
            hoyolab_floor_name: None,
            group_id: None,
            group_name: None,
            floor_id: None,
            has_layers: layer_info.has_layers,
            zoom_ratio_t: zoom_t,
            candidates: if debug {
                Some(
                    candidates
                        .iter()
                        .map(|c_item| LocateCandidateDebug {
                            x: c_item.0,
                            y: c_item.1,
                            r: c_item.2 as f64,
                        })
                        .collect(),
                )
            } else {
                None
            },
            inliers: None,
            slider_res: if debug { slider_res.cloned() } else { None },
            region_text: if debug { Some(region_text) } else { None },
            target_map_id: if debug { target_map_id } else { None },
            target_area_id: if debug { target_area_id } else { None },
            slot_types: if debug {
                Some(layer_info.slot_types)
            } else {
                None
            },
            reason: if debug {
                Some(format!(
                    "Phát hiện {} waypoint nhưng không tìm thấy cấu hình mốc khớp với cơ sở dữ liệu.",
                    candidates.len()
                ))
            } else {
                None
            },
            debug_included: debug,
        })
        })
    }

    pub fn locate_player_frame(
        &self,
        frame: FrameView<'_>,
        debug: bool,
        slider_res_override: Option<&ZoomSliderResult>,
    ) -> Option<LocateResult> {
        let bgr = frame_view_to_bgr(frame).ok()?;
        self.locate_player(bgr.view(), debug, slider_res_override)
    }

    pub fn resolve_floor_label(&self, map_id: i32, floor_id: i32) -> String {
        if floor_id <= 0 {
            return String::new();
        }
        let fid_str = floor_id.to_string();
        if let Ok(guard) = self.floor_label_index.lock()
            && let Some(hit) = guard.get(&(map_id, fid_str.clone()))
        {
            return hit.clone();
        }
        if let Some(lbl) = self.db.resolve_floor_label(map_id, floor_id) {
            if let Ok(mut guard) = self.floor_label_index.lock() {
                guard.insert((map_id, fid_str), lbl.clone());
            }
            return lbl;
        }
        String::new()
    }

    pub fn resolve_floor_label_str(&self, map_id: i32, floor_id: &str) -> String {
        let trimmed = floor_id.trim();
        let Ok(fid) = trimmed.parse::<i32>() else {
            return String::new();
        };
        self.resolve_floor_label(map_id, fid)
    }

    pub fn infer_unique_underground_floor(
        &self,
        map_id: i32,
        lat: f64,
        lng: f64,
        margin: f64,
    ) -> Option<(String, String)> {
        self.db
            .infer_unique_underground_floor(map_id, lat, lng, margin)
            .map(|(fid, lbl)| (fid.to_string(), lbl))
    }
}

fn compress_chain_simple_local(pts: &[Point2i]) -> Vec<Point2i> {
    if pts.len() <= 2 {
        return pts.to_vec();
    }
    let mut out = Vec::with_capacity(pts.len());
    let n = pts.len();
    for i in 0..n {
        let prev = pts[(i + n - 1) % n];
        let cur = pts[i];
        let next = pts[(i + 1) % n];
        let dx1 = (cur.x - prev.x).signum();
        let dy1 = (cur.y - prev.y).signum();
        let dx2 = (next.x - cur.x).signum();
        let dy2 = (next.y - cur.y).signum();
        if dx1 != dx2 || dy1 != dy2 {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(pts[0]);
    }
    out
}

fn zero_ui_exclusion_rects(buf: &mut [u8], h: usize, w: usize) {
    let rects = [
        (0usize, (h as f64 * 0.065) as usize, 0usize, w),
        (
            0usize,
            (h as f64 * 0.42) as usize,
            0usize,
            (w as f64 * 0.18) as usize,
        ),
        (
            (h as f64 * 0.36) as usize,
            (h as f64 * 0.62) as usize,
            0usize,
            (w as f64 * 0.05) as usize,
        ),
        (
            (h as f64 * 0.90) as usize,
            h,
            0usize,
            (w as f64 * 0.07) as usize,
        ),
        (
            (h as f64 * 0.90) as usize,
            h,
            (w as f64 * 0.37) as usize,
            (w as f64 * 0.63) as usize,
        ),
        (
            (h as f64 * 0.88) as usize,
            h,
            (w as f64 * 0.76) as usize,
            w,
        ),
        (
            (h as f64 * 0.20) as usize,
            (h as f64 * 0.80) as usize,
            (w as f64 * 0.93) as usize,
            w,
        ),
    ];
    for (y0, y1, x0, x1) in rects {
        let ya = y0.min(h);
        let yb = y1.min(h);
        let xa = x0.min(w);
        let xb = x1.min(w);
        if ya < yb && xa < xb {
            for y in ya..yb {
                buf[y * w + xa..y * w + xb].fill(0);
            }
        }
    }
}

/// Bit-exact separable binary `MorphOp::Open` for `{0, 255}` masks with `Rect(k, k)` (`k=2` or `k=3`).
fn fast_open_rect_binary(src: &mut [u8], tmp: &mut [u8], h: usize, w: usize, k: usize) {
    if h == 0 || w == 0 || k <= 1 {
        return;
    }
    if k == 3 {
        // 1. Erode horizontal (src -> tmp, OOB=255)
        if w == 1 {
            tmp.copy_from_slice(src);
        } else {
            for y in 0..h {
                let s = &src[y * w..(y + 1) * w];
                let d = &mut tmp[y * w..(y + 1) * w];
                d[0] = s[0] & s[1];
                for x in 1..w - 1 {
                    d[x] = s[x - 1] & s[x] & s[x + 1];
                }
                d[w - 1] = s[w - 2] & s[w - 1];
            }
        }
        // 2. Erode vertical (tmp -> src, OOB=255)
        if h == 1 {
            src.copy_from_slice(tmp);
        } else {
            for x in 0..w {
                src[x] = tmp[x] & tmp[w + x];
            }
            for y in 1..h - 1 {
                let r_prev = &tmp[(y - 1) * w..y * w];
                let r_cur = &tmp[y * w..(y + 1) * w];
                let r_next = &tmp[(y + 1) * w..(y + 2) * w];
                let dst = &mut src[y * w..(y + 1) * w];
                for x in 0..w {
                    dst[x] = r_prev[x] & r_cur[x] & r_next[x];
                }
            }
            let r_prev = &tmp[(h - 2) * w..(h - 1) * w];
            let r_cur = &tmp[(h - 1) * w..h * w];
            let dst = &mut src[(h - 1) * w..h * w];
            for x in 0..w {
                dst[x] = r_prev[x] & r_cur[x];
            }
        }
        // 3. Dilate horizontal (src -> tmp, OOB=0)
        if w == 1 {
            tmp.copy_from_slice(src);
        } else {
            for y in 0..h {
                let s = &src[y * w..(y + 1) * w];
                let d = &mut tmp[y * w..(y + 1) * w];
                d[0] = s[0] | s[1];
                for x in 1..w - 1 {
                    d[x] = s[x - 1] | s[x] | s[x + 1];
                }
                d[w - 1] = s[w - 2] | s[w - 1];
            }
        }
        // 4. Dilate vertical (tmp -> src, OOB=0)
        if h == 1 {
            src.copy_from_slice(tmp);
        } else {
            for x in 0..w {
                src[x] = tmp[x] | tmp[w + x];
            }
            for y in 1..h - 1 {
                let r_prev = &tmp[(y - 1) * w..y * w];
                let r_cur = &tmp[y * w..(y + 1) * w];
                let r_next = &tmp[(y + 1) * w..(y + 2) * w];
                let dst = &mut src[y * w..(y + 1) * w];
                for x in 0..w {
                    dst[x] = r_prev[x] | r_cur[x] | r_next[x];
                }
            }
            let r_prev = &tmp[(h - 2) * w..(h - 1) * w];
            let r_cur = &tmp[(h - 1) * w..h * w];
            let dst = &mut src[(h - 1) * w..h * w];
            for x in 0..w {
                dst[x] = r_prev[x] | r_cur[x];
            }
        }
        return;
    }
    if k == 2 {
        // Offsets (-1, -1), (0, -1), (-1, 0), (0, 0)
        for y in 0..h {
            let s = &src[y * w..(y + 1) * w];
            let d = &mut tmp[y * w..(y + 1) * w];
            d[0] = s[0];
            for x in 1..w {
                d[x] = s[x - 1] & s[x];
            }
        }
        src[0..w].copy_from_slice(&tmp[0..w]);
        for y in 1..h {
            let r_prev = &tmp[(y - 1) * w..y * w];
            let r_cur = &tmp[y * w..(y + 1) * w];
            let dst = &mut src[y * w..(y + 1) * w];
            for x in 0..w {
                dst[x] = r_prev[x] & r_cur[x];
            }
        }
        for y in 0..h {
            let s = &src[y * w..(y + 1) * w];
            let d = &mut tmp[y * w..(y + 1) * w];
            d[0] = s[0];
            for x in 1..w {
                d[x] = s[x - 1] | s[x];
            }
        }
        src[0..w].copy_from_slice(&tmp[0..w]);
        for y in 1..h {
            let r_prev = &tmp[(y - 1) * w..y * w];
            let r_cur = &tmp[y * w..(y + 1) * w];
            let dst = &mut src[y * w..(y + 1) * w];
            for x in 0..w {
                dst[x] = r_prev[x] | r_cur[x];
            }
        }
        return;
    }
    if let Ok(kernel) = get_structuring_element(MorphShape::Rect, k, k)
        && let Ok(arr) = Array2::from_shape_vec((h, w), src.to_vec())
        && let Ok(opened) = morphology_ex_u8(arr.view(), MorphOp::Open, kernel.view())
        && let Some(sl) = opened.as_slice()
    {
        src.copy_from_slice(sl);
    }
}

#[derive(Default)]
struct ImgScratch {
    hsv_buf: Vec<u8>,
    gray_buf: Vec<u8>,
}

const INV_30_F32_LOC: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut i = 1usize;
    while i < 256 {
        t[i] = 30.0 / (i as f32);
        i += 1;
    }
    t
};

const RECIP_U32_SHIFT32_LOC: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut i = 1usize;
    while i < 256 {
        t[i] = (1u64 << 32).div_ceil(i as u64);
        i += 1;
    }
    t
};

fn fill_hsv_and_gray_into(src: ArrayView3<'_, u8>, hsv_out: &mut [u8], gray_out: &mut [u8]) {
    let (h, w, c) = src.dim();
    #[inline(always)]
    fn proc_px(b: u8, g: u8, r: u8) -> (u8, u8, u8, u8) {
        let gray = bgr_px_to_gray(b, g, r);
        let vmax = b.max(g).max(r);
        let vmin = b.min(g).min(r);
        let diff = vmax - vmin;
        let s = if vmax == 0 {
            0u8
        } else {
            let num = 255u64 * diff as u64 + (vmax as u64 >> 1);
            ((num * RECIP_U32_SHIFT32_LOC[vmax as usize]) >> 32).min(255) as u8
        };
        let h_val = if diff == 0 {
            0u8
        } else {
            let inv = INV_30_F32_LOC[diff as usize];
            let (base, delta) = if vmax == r {
                (180.0f32, (g as i32 - b as i32) as f32)
            } else if vmax == g {
                (240.0f32, (b as i32 - r as i32) as f32)
            } else {
                (300.0f32, (r as i32 - g as i32) as f32)
            };
            let h_shifted = (base + delta * inv + 0.5) as i32;
            (if h_shifted >= 180 {
                h_shifted - 180
            } else {
                h_shifted
            }) as u8
        };
        (h_val, s, vmax, gray)
    }

    if let Some(s) = src.as_slice() {
        for ((dst_h, dst_g), px) in hsv_out
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(gray_out.iter_mut())
            .zip(s.chunks_exact(c))
        {
            let (hv, sv, vv, gv) = proc_px(px[0], px[1], px[2]);
            dst_h[0] = hv;
            dst_h[1] = sv;
            dst_h[2] = vv;
            *dst_g = gv;
        }
    } else {
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let (hv, sv, vv, gv) = proc_px(src[[y, x, 0]], src[[y, x, 1]], src[[y, x, 2]]);
                let ho = (row + x) * 3;
                hsv_out[ho] = hv;
                hsv_out[ho + 1] = sv;
                hsv_out[ho + 2] = vv;
                gray_out[row + x] = gv;
            }
        }
    }
}

#[derive(Default)]
struct WpScratch {
    mask_buf: Vec<u8>,
    tmp_buf: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
struct CcStat {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    area: i32,
    sx: usize,
    sy: usize,
}

#[derive(Default)]
struct ContourScratch {
    labels: Vec<i32>,
    parent: Vec<usize>,
    stats: Vec<CcStat>,
    ext_bg: Vec<u8>,
    stk: Vec<(u32, u32)>,
}

thread_local! {
    static LOCATOR_IMG_SCRATCH: RefCell<ImgScratch> = RefCell::new(ImgScratch::default());
    static WP_SCRATCH: RefCell<WpScratch> = RefCell::new(WpScratch::default());
    static CONTOUR_SCRATCH: RefCell<ContourScratch> = RefCell::new(ContourScratch::default());
}

/// Drops retained thread-local full-frame scratch buffers when M-Map is closed or blocking
/// locator calls complete (re-allocated lazily on next full-frame locate).
pub fn release_thread_scratch() {
    let _ = LOCATOR_IMG_SCRATCH.try_with(|cell| {
        if let Ok(mut im) = cell.try_borrow_mut() {
            *im = ImgScratch::default();
        }
    });
    let _ = WP_SCRATCH.try_with(|cell| {
        if let Ok(mut ws) = cell.try_borrow_mut() {
            *ws = WpScratch::default();
        }
    });
    let _ = CONTOUR_SCRATCH.try_with(|cell| {
        if let Ok(mut cs) = cell.try_borrow_mut() {
            *cs = ContourScratch::default();
        }
    });
}

fn compute_coarse_valid_mask(
    c_small: ArrayView2<'_, u8>,
    cw_s: usize,
    ch_s: usize,
) -> Option<Array2<u8>> {
    let (sh, sw) = c_small.dim();
    if cw_s == 0 || ch_s == 0 || cw_s > sw || ch_s > sh {
        return None;
    }
    let rh = sh - ch_s + 1;
    let rw = sw - cw_s + 1;
    let stride = sw + 1;
    let total = (sh + 1) * stride;
    let mut int_nz = vec![0u64; total];
    let mut int_v = vec![0u64; total];
    let mut int_v2 = vec![0u64; total];

    for y in 0..sh {
        let mut row_nz = 0u64;
        let mut row_v = 0u64;
        let mut row_v2 = 0u64;
        let prev_row = y * stride;
        let cur_row = (y + 1) * stride;
        for x in 0..sw {
            let v = c_small[[y, x]] as u64;
            if v > 2 {
                row_nz += 1;
            }
            row_v += v;
            row_v2 += v * v;
            int_nz[cur_row + x + 1] = int_nz[prev_row + x + 1] + row_nz;
            int_v[cur_row + x + 1] = int_v[prev_row + x + 1] + row_v;
            int_v2[cur_row + x + 1] = int_v2[prev_row + x + 1] + row_v2;
        }
    }

    let area = (cw_s * ch_s) as f64;
    let mut valid = Array2::<u8>::zeros((rh, rw));
    for oy in 0..rh {
        let r0 = oy * stride;
        let r1 = (oy + ch_s) * stride;
        for ox in 0..rw {
            let c0 = ox;
            let c1 = ox + cw_s;
            let s_nz = (int_nz[r1 + c1] + int_nz[r0 + c0]) - (int_nz[r0 + c1] + int_nz[r1 + c0]);
            let s_v = (int_v[r1 + c1] + int_v[r0 + c0]) - (int_v[r0 + c1] + int_v[r1 + c0]);
            let s_v2 = (int_v2[r1 + c1] + int_v2[r0 + c0]) - (int_v2[r0 + c1] + int_v2[r1 + c0]);
            let nzf = (s_nz as f64) / area;
            let m = (s_v as f64) / area;
            let m2 = (s_v2 as f64) / area;
            let std_v = (m2 - m * m).max(0.0).sqrt();
            if nzf >= 0.85 && std_v >= 8.0 {
                valid[[oy, ox]] = 1;
            }
        }
    }
    Some(valid)
}

fn extract_filtered_contours_external_slice(
    m: &[u8],
    h: usize,
    w: usize,
    min_contour_area: f64,
    max_contour_area: f64,
    min_dim: i32,
    max_dim: i32,
) -> Vec<Vec<Point2i>> {
    if h == 0 || w == 0 || m.len() < h * w {
        return Vec::new();
    }

    #[inline]
    fn find_root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    #[inline]
    fn find_root_const(parent: &[usize], mut x: usize) -> usize {
        while parent[x] != x {
            x = parent[x];
        }
        x
    }

    CONTOUR_SCRATCH.with(|cell| {
    let mut cs = cell.borrow_mut();
    let n_px = h * w;
    if cs.labels.len() < n_px {
        cs.labels.resize(n_px, 0);
    }
    let cs_ref = &mut *cs;
    let labels = &mut cs_ref.labels[..n_px];
    labels.fill(0);
    let parent = &mut cs_ref.parent;
    parent.clear();
    parent.push(0usize);
    let stats = &mut cs_ref.stats;
    stats.clear();
    stats.push(CcStat::default());

    for y in 0..h {
        let row_off = y * w;
        let mut x = 0usize;
        while x < w {
            while x + 8 <= w && m[row_off + x..row_off + x + 8] == [0u8; 8] {
                x += 8;
            }
            while x < w && m[row_off + x] == 0 {
                x += 1;
            }
            if x >= w {
                break;
            }
            let x0 = x;
            while x < w && m[row_off + x] != 0 {
                x += 1;
            }
            let x1 = x;

            let r = stats.len();
            parent.push(r);
            stats.push(CcStat {
                min_x: x0 as i32,
                min_y: y as i32,
                max_x: (x1 - 1) as i32,
                max_y: y as i32,
                area: (x1 - x0) as i32,
                sx: x0,
                sy: y,
            });
            labels[row_off + x0..row_off + x1].fill(r as i32);

            if y > 0 {
                let prev_off = (y - 1) * w;
                let mut px = x0.saturating_sub(1);
                let px_end = (x1 + 1).min(w);
                while px < px_end {
                    let p = labels[prev_off + px] as usize;
                    if p != 0 {
                        let mut r1 = find_root(parent, r);
                        let mut r2 = find_root(parent, p);
                        if r1 != r2 {
                            if r2 < r1 {
                                std::mem::swap(&mut r1, &mut r2);
                            }
                            parent[r2] = r1;
                            stats[r1].min_x = stats[r1].min_x.min(stats[r2].min_x);
                            stats[r1].max_x = stats[r1].max_x.max(stats[r2].max_x);
                            stats[r1].min_y = stats[r1].min_y.min(stats[r2].min_y);
                            stats[r1].max_y = stats[r1].max_y.max(stats[r2].max_y);
                            stats[r1].area += stats[r2].area;
                        }
                        while px + 1 < px_end && labels[prev_off + px + 1] as usize == p {
                            px += 1;
                        }
                    }
                    px += 1;
                }
            }
        }
    }

    let roots: Vec<usize> = (1..stats.len()).filter(|&r| parent[r] == r).collect();
    if roots.is_empty() {
        return Vec::new();
    }

    let mut survivors: Vec<usize> = Vec::with_capacity(32);
    let mut min_cand_bw = i32::MAX;
    let mut min_cand_bh = i32::MAX;
    for &root_id in &roots {
        let st = stats[root_id];
        let bw = st.max_x - st.min_x + 1;
        let bh = st.max_y - st.min_y + 1;
        if bw < min_dim || bw > max_dim || bh < min_dim || bh > max_dim {
            continue;
        }
        if (bw as f64 * bh as f64) < min_contour_area {
            continue;
        }
        let max_px_upper = max_contour_area * 2.0 + 2.0 * (bw + bh) as f64 + 64.0;
        if (st.area as f64) > max_px_upper {
            continue;
        }
        min_cand_bw = min_cand_bw.min(bw);
        min_cand_bh = min_cand_bh.min(bh);
        survivors.push(root_id);
    }
    if survivors.is_empty() {
        return Vec::new();
    }

    let enclosers: Vec<usize> = if roots.len() > 1 {
        roots
            .iter()
            .copied()
            .filter(|&id| {
                let st = stats[id];
                (st.max_x - st.min_x + 1) >= min_cand_bw + 2
                    && (st.max_y - st.min_y + 1) >= min_cand_bh + 2
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut ext_bg_ready = false;

    const CW8: [(isize, isize); 8] = [
        (-1, 0),
        (-1, -1),
        (0, -1),
        (1, -1),
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
    ];

    let mut contours = Vec::with_capacity(survivors.len());

    for &root_id in &survivors {
        let st = stats[root_id];
        let bw = st.max_x - st.min_x + 1;
        let bh = st.max_y - st.min_y + 1;

        if !enclosers.is_empty()
            && enclosers.iter().any(|&other_id| {
                if other_id == root_id {
                    return false;
                }
                let other = stats[other_id];
                other.min_x < st.min_x
                    && other.min_y < st.min_y
                    && other.max_x > st.max_x
                    && other.max_y > st.max_y
            })
        {
            if !ext_bg_ready {
                if cs_ref.ext_bg.len() < n_px {
                    cs_ref.ext_bg.resize(n_px, 0);
                }
                let bg = &mut cs_ref.ext_bg[..n_px];
                bg.fill(0);
                let stk = &mut cs_ref.stk;
                stk.clear();
                for x in 0..w {
                    if labels[x] == 0 {
                        stk.push((x as u32, 0));
                    }
                    let bot = (h - 1) * w + x;
                    if labels[bot] == 0 {
                        stk.push((x as u32, (h - 1) as u32));
                    }
                }
                for y in 0..h {
                    if labels[y * w] == 0 {
                        stk.push((0, y as u32));
                    }
                    if labels[y * w + (w - 1)] == 0 {
                        stk.push(((w - 1) as u32, y as u32));
                    }
                }
                while let Some((qx, qy)) = stk.pop() {
                    let uy = qy as usize;
                    let row = uy * w;
                    let ux = qx as usize;
                    if bg[row + ux] != 0 || labels[row + ux] != 0 {
                        continue;
                    }
                    let mut lx = ux;
                    while lx > 0 && labels[row + lx - 1] == 0 {
                        lx -= 1;
                    }
                    let mut rx = ux;
                    while rx + 1 < w && labels[row + rx + 1] == 0 {
                        rx += 1;
                    }
                    bg[row + lx..=row + rx].fill(1);
                    for ny in [
                        uy.checked_sub(1),
                        if uy + 1 < h { Some(uy + 1) } else { None },
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let nrow = ny * w;
                        let mut x = lx;
                        while x <= rx {
                            while x <= rx && (labels[nrow + x] != 0 || bg[nrow + x] != 0) {
                                x += 1;
                            }
                            if x > rx {
                                break;
                            }
                            stk.push((x as u32, ny as u32));
                            while x <= rx && labels[nrow + x] == 0 && bg[nrow + x] == 0 {
                                x += 1;
                            }
                        }
                    }
                }
                ext_bg_ready = true;
            }
            if st.sx > 0 && cs_ref.ext_bg[st.sy * w + (st.sx - 1)] == 0 {
                continue;
            }
        }

        let sx = st.sx as isize;
        let sy = st.sy as isize;
        let mut pts = Vec::with_capacity((bw + bh) as usize * 2);
        pts.push(Point2i {
            x: st.sx as i32,
            y: st.sy as i32,
        });

        let mut cx = sx;
        let mut cy = sy;
        let mut start_dir = 0usize;
        let mut first_next: Option<(isize, isize)> = None;
        let max_steps = ((bw as usize * bh as usize) * 4).max(64);
        for _ in 0..max_steps {
            let mut found = None;
            for step in 0..8 {
                let d = (start_dir + step) % 8;
                let nx = cx + CW8[d].0;
                let ny = cy + CW8[d].1;
                if nx >= st.min_x as isize
                    && nx <= st.max_x as isize
                    && ny >= st.min_y as isize
                    && ny <= st.max_y as isize
                {
                    let nl = labels[ny as usize * w + nx as usize] as usize;
                    if nl != 0 && find_root_const(parent, nl) == root_id {
                        found = Some((nx, ny, d));
                        break;
                    }
                }
            }
            let Some((nx, ny, hit_dir)) = found else {
                break;
            };
            if let Some((fnx, fny)) = first_next {
                if cx == sx && cy == sy && nx == fnx && ny == fny {
                    break;
                }
            } else {
                first_next = Some((nx, ny));
            }
            if !(nx == sx && ny == sy) || pts.len() == 1 {
                if nx == sx && ny == sy && first_next.is_some() {
                    // Wait for Jacob's stopping check.
                } else {
                    pts.push(Point2i {
                        x: nx as i32,
                        y: ny as i32,
                    });
                }
            }
            cx = nx;
            cy = ny;
            start_dir = (hit_dir + 5) % 8;
        }
        if pts.len() > 1 && pts.last() == pts.first() {
            pts.pop();
        }
        let final_pts = compress_chain_simple_local(&pts);
        if !final_pts.is_empty() {
            contours.push(final_pts);
        }
    }

    contours
    })
}

thread_local! {
    static LOCATOR_FFT_PLANNER: RefCell<FftPlanner<f32>> = RefCell::new(FftPlanner::new());
}

#[inline]
fn next_smooth_len_235(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let mut cur = n;
    loop {
        let mut x = cur;
        for p in [2usize, 3, 5] {
            while x.is_multiple_of(p) {
                x /= p;
            }
        }
        if x == 1 {
            return cur;
        }
        cur += 1;
    }
}

fn transpose_c32_blocked(
    src: &[Complex<f32>],
    dst: &mut [Complex<f32>],
    rows: usize,
    cols: usize,
) {
    const BLK: usize = 16;
    for r0 in (0..rows).step_by(BLK) {
        let r1 = (r0 + BLK).min(rows);
        for c0 in (0..cols).step_by(BLK) {
            let c1 = (c0 + BLK).min(cols);
            for r in r0..r1 {
                let srow = r * cols;
                for c in c0..c1 {
                    dst[c * rows + r] = src[srow + c];
                }
            }
        }
    }
}

/// Fast `CcoeffNormed` 1-channel template matcher using a thread-local cached `rustfft::FftPlanner<f32>`,
/// complex-packed dual-real forward 2D FFT, cache-blocked transposes for contiguous 1D column FFTs,
/// and `f64` integral images for exact variance normalization.
fn fast_match_template_ccoeff_f32(
    img: ArrayView2<'_, f32>,
    tpl: ArrayView2<'_, f32>,
) -> Option<Array2<f32>> {
    let (ih, iw) = img.dim();
    let (kh, kw) = tpl.dim();
    if ih == 0 || iw == 0 || kh == 0 || kw == 0 || kh > ih || kw > iw {
        return None;
    }
    let oh = ih - kh + 1;
    let ow = iw - kw + 1;
    let out_cells = oh * ow;
    let n_tpl = (kh * kw) as f64;

    let mut sum_i_all = 0.0f64;
    for &v in img.iter() {
        sum_i_all += v as f64;
    }
    let mean_shift = sum_i_all / ((ih * iw) as f64);

    let mut t_mean = 0.0f64;
    for &v in tpl.iter() {
        t_mean += v as f64;
    }
    t_mean /= n_tpl;

    let mut t_vec = Vec::with_capacity(kh * kw);
    let mut t_sq_sum = 0.0f64;
    for y in 0..kh {
        for x in 0..kw {
            let tv = tpl[[y, x]] as f64 - t_mean;
            t_sq_sum += tv * tv;
            t_vec.push(tv);
        }
    }
    if t_sq_sum <= 1e-12 {
        return Some(Array2::<f32>::zeros((oh, ow)));
    }

    if out_cells <= 256 || (out_cells * kh * kw) <= 250_000 {
        let mut out = Array2::<f32>::zeros((oh, ow));
        for oy in 0..oh {
            for ox in 0..ow {
                let mut sum_i = 0.0f64;
                let mut sum_i2 = 0.0f64;
                let mut dot = 0.0f64;
                for ky in 0..kh {
                    let trow = &t_vec[ky * kw..(ky + 1) * kw];
                    for kx in 0..kw {
                        let iv = img[[oy + ky, ox + kx]] as f64 - mean_shift;
                        let tv = trow[kx];
                        sum_i += iv;
                        sum_i2 += iv * iv;
                        dot += iv * tv;
                    }
                }
                let var_i = (sum_i2 - (sum_i * sum_i) / n_tpl).max(0.0);
                let denom = (var_i * t_sq_sum).sqrt();
                let val = if denom <= 1e-12 || var_i <= 1e-12 * n_tpl {
                    0.0
                } else {
                    (dot / denom).clamp(-1.0, 1.0)
                };
                out[[oy, ox]] = if val.is_finite() { val as f32 } else { 0.0 };
            }
        }
        return Some(out);
    }

    let fh = next_smooth_len_235(ih);
    let fw = next_smooth_len_235(iw);

    let stride_int = iw + 1;
    let mut int_i = vec![0.0f64; (ih + 1) * stride_int];
    let mut int_i2 = vec![0.0f64; (ih + 1) * stride_int];
    let mut z = vec![Complex::new(0.0f32, 0.0f32); fh * fw];
    for y in 0..ih {
        let mut row_s1 = 0.0f64;
        let mut row_s2 = 0.0f64;
        let zrow = y * fw;
        for x in 0..iw {
            let v = img[[y, x]] as f64 - mean_shift;
            z[zrow + x].re = v as f32;
            row_s1 += v;
            row_s2 += v * v;
            let idx = (y + 1) * stride_int + (x + 1);
            int_i[idx] = int_i[y * stride_int + (x + 1)] + row_s1;
            int_i2[idx] = int_i2[y * stride_int + (x + 1)] + row_s2;
        }
    }
    for y in 0..kh {
        let zrow = y * fw;
        let trow = y * kw;
        for x in 0..kw {
            z[zrow + x].im = t_vec[trow + x] as f32;
        }
    }

    let (row_fwd, col_fwd, row_inv, col_inv) = LOCATOR_FFT_PLANNER.with(|cell| {
        let mut p = cell.borrow_mut();
        (
            p.plan_fft_forward(fw),
            p.plan_fft_forward(fh),
            p.plan_fft_inverse(fw),
            p.plan_fft_inverse(fh),
        )
    });

    let scratch_len = row_fwd
        .get_inplace_scratch_len()
        .max(col_fwd.get_inplace_scratch_len())
        .max(row_inv.get_inplace_scratch_len())
        .max(col_inv.get_inplace_scratch_len());
    let mut scratch = vec![Complex::new(0.0f32, 0.0f32); scratch_len];
    let mut tmp = vec![Complex::new(0.0f32, 0.0f32); fh * fw];

    for r in 0..fh {
        row_fwd.process_with_scratch(&mut z[r * fw..(r + 1) * fw], &mut scratch);
    }
    transpose_c32_blocked(&z, &mut tmp, fh, fw);
    for c in 0..fw {
        col_fwd.process_with_scratch(&mut tmp[c * fh..(c + 1) * fh], &mut scratch);
    }
    transpose_c32_blocked(&tmp, &mut z, fw, fh);

    for r in 0..fh {
        let r_neg = if r == 0 { 0 } else { fh - r };
        let zrow = r * fw;
        let nrow = r_neg * fw;
        for c in 0..fw {
            let c_neg = if c == 0 { 0 } else { fw - c };
            let f = z[zrow + c];
            let f_neg = z[nrow + c_neg].conj();
            let f_img = Complex::new(0.5 * (f.re + f_neg.re), 0.5 * (f.im + f_neg.im));
            let f_kern_conj = Complex::new(0.5 * (f.im - f_neg.im), 0.5 * (f.re - f_neg.re));
            tmp[zrow + c] = f_img * f_kern_conj;
        }
    }

    for r in 0..fh {
        row_inv.process_with_scratch(&mut tmp[r * fw..(r + 1) * fw], &mut scratch);
    }
    transpose_c32_blocked(&tmp, &mut z, fh, fw);
    for c in 0..fw {
        col_inv.process_with_scratch(&mut z[c * fh..(c + 1) * fh], &mut scratch);
    }

    let norm = 1.0 / ((fh * fw) as f64);
    let mut out = Array2::<f32>::zeros((oh, ow));
    for oy in 0..oh {
        let y0 = oy;
        let y1 = oy + kh;
        for ox in 0..ow {
            let x0 = ox;
            let x1 = ox + kw;
            let s1 = int_i[y1 * stride_int + x1]
                - int_i[y0 * stride_int + x1]
                - int_i[y1 * stride_int + x0]
                + int_i[y0 * stride_int + x0];
            let s2 = int_i2[y1 * stride_int + x1]
                - int_i2[y0 * stride_int + x1]
                - int_i2[y1 * stride_int + x0]
                + int_i2[y0 * stride_int + x0];
            // Read directly from transposed `z[ox * fh + oy]` to save one transpose!
            let dot = z[ox * fh + oy].re as f64 * norm;
            let var_i = (s2 - (s1 * s1) / n_tpl).max(0.0);
            let denom = (var_i * t_sq_sum).sqrt();
            let val = if denom <= 1e-12 || var_i <= 1e-12 * n_tpl {
                0.0
            } else {
                (dot / denom).clamp(-1.0, 1.0)
            };
            out[[oy, ox]] = if val.is_finite() { val as f32 } else { 0.0 };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::CoreError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_cv_locator_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    struct ErrOcrProvider;
    impl OcrProvider for ErrOcrProvider {
        fn recognize_text(&self, _frame: FrameView<'_>) -> gh_core::CoreResult<String> {
            Err(CoreError::InvalidArgument("crop too small".into()))
        }
    }

    struct FixedOcrProvider(&'static str);
    impl OcrProvider for FixedOcrProvider {
        fn recognize_text(&self, _frame: FrameView<'_>) -> gh_core::CoreResult<String> {
            Ok(self.0.to_owned())
        }
    }

    #[test]
    fn normalize_region_text_preserves_spaces_and_avoids_me_corruption() {
        assert_eq!(
            normalize_region_text("Nüi Thånh Vién cb"),
            "nui thanh vien co"
        );
        assert_eq!(
            normalize_region_text("Bech Du'0'ng Tuyét"),
            "bech du 0 ng tuyet"
        );
        assert_eq!(
            normalize_region_text("åt Bäng Vinh Ciru"),
            "at bang vinh cuu"
        );
        assert_eq!(
            normalize_region_text("I Dei Sånh Fontame"),
            "i dei sanh fontame"
        );
        assert_eq!(
            normalize_region_text("Bién KY Nguyen Cü"),
            "bien ky nguyen cu"
        );
        assert_eq!(normalize_region_text("au - Khu MÖ Ngam"), "au   khu mo ngam");
        assert_eq!(normalize_region_text("Sumeru"), "sumeru");
        assert_eq!(normalize_region_text("Remuria"), "remuria");
        assert_eq!(normalize_region_text("Khu Morte"), "khu morte");
    }

    #[test]
    fn ocr_error_fallback_and_region_detection_work() {
        let missing_dir = make_temp_dir("ocr");
        let paths = LocatorPaths {
            db_path: Some(missing_dir.join("missing.json")),
            terrain_npz_path: Some(missing_dir.join("missing_t.npz")),
            cave_masks_npz_path: Some(missing_dir.join("missing_c.npz")),
            ui_anchors_npz_path: Some(missing_dir.join("missing_u.npz")),
            legacy_underground_db_path: Some(missing_dir.join("missing_ug.json")),
        };
        let frame = Array3::<u8>::zeros((1440, 2560, 3));

        let loc_err = MapLocator::with_paths(paths.clone(), Some(Arc::new(ErrOcrProvider)));
        assert_eq!(
            loc_err.detect_region_name(frame.view()),
            (None, None, String::new())
        );

        let loc_ok =
            MapLocator::with_paths(paths.clone(), Some(Arc::new(FixedOcrProvider("Nüi Thånh Vién cb"))));
        let (mid, aid, raw) = loc_ok.detect_region_name(frame.view());
        assert_eq!(mid, Some(36));
        assert_eq!(aid, None);
        assert_eq!(raw, "Nüi Thånh Vién cb");

        // Map 40 (Frost Moon) sub-regions including real Windows OCR diacritic confusions
        for sample in [
            "Biển Trăng Dạt Dào",
            "3ién Träng Det Däo",
            "Vùng Tối Của Mặt Trăng",
            "Tbi Cüa Möt Träng",
            "Frost Moon",
        ] {
            let loc_m40 =
                MapLocator::with_paths(paths.clone(), Some(Arc::new(FixedOcrProvider(sample))));
            let (m40_id, m40_aid, _) = loc_m40.detect_region_name(frame.view());
            assert_eq!(m40_id, Some(40), "expected Map 40 for OCR sample {sample:?}");
            assert_eq!(m40_aid, None);
        }

        let _ = std::fs::remove_dir_all(&missing_dir);
    }

    #[test]
    fn icon_anchor_scaling_matches_python() {
        for (h, su) in [(1080.0, 0.75), (1440.0, 1.0), (2160.0, 1.5)] {
            let s = h / 1440.0;
            let (ox_tp, oy_tp) = icon_anchor_offset_px(3, s);
            let (ox_st, oy_st) = icon_anchor_offset_px(2, s);
            let (ox_dm, oy_dm) = icon_anchor_offset_px(154, s);
            assert!(ox_tp.abs() < 1e-9 && (oy_tp - 5.0 * su).abs() < 1e-9);
            assert!(ox_st.abs() < 1e-9 && (oy_st - 3.5 * su).abs() < 1e-9);
            assert!((ox_dm - 2.5 * su).abs() < 1e-9 && oy_dm.abs() < 1e-9);
        }
    }

    #[test]
    fn missing_assets_legacy_array_and_synthetic_negatives() {
        let base = make_temp_dir("legacy");
        let legacy_json = base.join("waypoints_db.json");
        std::fs::write(
            &legacy_json,
            r#"[
                {"id": 1001, "label_id": 3, "is_primary": true, "x_pos": 10.0, "y_pos": 20.0, "area_id": 1},
                {"id": 1002, "label_id": 2, "is_primary": true, "x_pos": 210.0, "y_pos": 20.0, "area_id": 1}
            ]"#,
        )
        .unwrap();

        let loc = MapLocator::with_paths(
            LocatorPaths {
                db_path: Some(legacy_json),
                terrain_npz_path: Some(base.join("absent_t.npz")),
                cave_masks_npz_path: Some(base.join("absent_c.npz")),
                ui_anchors_npz_path: Some(base.join("absent_u.npz")),
                legacy_underground_db_path: Some(base.join("absent_ug.json")),
            },
            None,
        );
        assert!(loc.map_meta(2).is_some());
        assert_eq!(loc.map_meta(2).unwrap().waypoints.len(), 2);
        assert!(loc.terrain_data().is_empty());
        assert!(loc.ui_anchors().is_empty());

        for fill in [0u8, 255u8] {
            let synth = Array3::<u8>::from_elem((720, 1280, 3), fill);
            assert!(loc.locate_player(synth.view(), false, None).is_none());
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn floor_label_and_unique_inference_work() {
        let base = make_temp_dir("floor");
        let mm_path = base.join("multimap.json");
        std::fs::write(
            &mm_path,
            r#"{
                "version": "4.0.0",
                "maps": {
                    "2": {
                        "id": 2,
                        "name": "Teyvat",
                        "point_groups": [{
                            "id": 90,
                            "map_id": 2,
                            "name": "Fontaine Cave",
                            "floors": [
                                {
                                    "id": 78,
                                    "floor_name": "B1",
                                    "floor_short": "b1",
                                    "index": 0,
                                    "overlay": {"url": "https://x/1.png", "l_x": -5000.0, "l_y": -1550.0, "r_x": -4850.0, "r_y": -1400.0}
                                }
                            ]
                        }]
                    }
                }
            }"#,
        )
        .unwrap();

        let loc = MapLocator::with_paths(
            LocatorPaths {
                db_path: Some(mm_path),
                terrain_npz_path: Some(base.join("absent_t.npz")),
                cave_masks_npz_path: Some(base.join("absent_c.npz")),
                ui_anchors_npz_path: Some(base.join("absent_u.npz")),
                legacy_underground_db_path: Some(base.join("absent_ug.json")),
            },
            None,
        );
        assert_eq!(loc.resolve_floor_label(2, 78), "B1");
        assert_eq!(loc.resolve_floor_label(2, 0), "");
        assert_eq!(
            loc.infer_unique_underground_floor(2, -1478.0, -4942.0, 15.0),
            Some(("78".to_owned(), "B1".to_owned()))
        );
        assert_eq!(
            loc.infer_unique_underground_floor(2, 0.0, 0.0, 15.0),
            None
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn coarse_valid_mask_matches_box_filter_f32() {
        let mut c_small = Array2::<u8>::zeros((36, 44));
        for y in 0..36 {
            for x in 0..44 {
                c_small[[y, x]] = if y < 6 || x < 4 {
                    0
                } else {
                    (((y * 17 + x * 31) % 210) + 20) as u8
                };
            }
        }
        let cw_s = 12usize;
        let ch_s = 10usize;
        let mask = compute_coarse_valid_mask(c_small.view(), cw_s, ch_s).unwrap();

        let nz = c_small.mapv(|v| if v > 2 { 1.0f32 } else { 0.0f32 });
        let cf = c_small.mapv(|v| v as f32);
        let cf2 = cf.mapv(|v| v * v);
        let nz_frac = box_filter_f32(nz.view(), cw_s, ch_s, true).unwrap();
        let mean_c = box_filter_f32(cf.view(), cw_s, ch_s, true).unwrap();
        let mean2_c = box_filter_f32(cf2.view(), cw_s, ch_s, true).unwrap();
        let hc_s = ch_s / 2;
        let wc_s = cw_s / 2;
        let (rh, rw) = mask.dim();
        assert_eq!((rh, rw), (36 - ch_s + 1, 44 - cw_s + 1));
        for oy in 0..rh {
            for ox in 0..rw {
                let nzf = nz_frac[[hc_s + oy, wc_s + ox]];
                let m = mean_c[[hc_s + oy, wc_s + ox]];
                let m2 = mean2_c[[hc_s + oy, wc_s + ox]];
                let std_v = (m2 - m * m).max(0.0).sqrt();
                let expected = u8::from(nzf >= 0.85 && std_v >= 8.0);
                assert_eq!(mask[[oy, ox]], expected, "mismatch at ({oy}, {ox})");
            }
        }
    }

    #[test]
    fn release_thread_scratch_resets_capacities_to_zero() {
        LOCATOR_IMG_SCRATCH.with(|cell| {
            let mut im = cell.borrow_mut();
            im.hsv_buf.resize(4096, 1);
            im.gray_buf.resize(1024, 2);
        });
        WP_SCRATCH.with(|cell| {
            let mut ws = cell.borrow_mut();
            ws.mask_buf.resize(2048, 3);
            ws.tmp_buf.resize(2048, 4);
        });
        CONTOUR_SCRATCH.with(|cell| {
            let mut cs = cell.borrow_mut();
            cs.labels.resize(1024, 5);
            cs.parent.resize(128, 6);
            cs.ext_bg.resize(1024, 7);
        });

        release_thread_scratch();

        LOCATOR_IMG_SCRATCH.with(|cell| {
            let im = cell.borrow();
            assert_eq!(im.hsv_buf.capacity(), 0);
            assert_eq!(im.gray_buf.capacity(), 0);
        });
        WP_SCRATCH.with(|cell| {
            let ws = cell.borrow();
            assert_eq!(ws.mask_buf.capacity(), 0);
            assert_eq!(ws.tmp_buf.capacity(), 0);
        });
        CONTOUR_SCRATCH.with(|cell| {
            let cs = cell.borrow();
            assert_eq!(cs.labels.capacity(), 0);
            assert_eq!(cs.parent.capacity(), 0);
            assert_eq!(cs.ext_bg.capacity(), 0);
        });
    }

    #[test]
    fn detect_layers_universal_rejects_cyan_snow_and_locks_surface_and_underground() {
        let mut loc = MapLocator::with_paths(
            LocatorPaths {
                db_path: Some(PathBuf::from("nonexistent_db.json")),
                terrain_npz_path: Some(PathBuf::from("nonexistent_terrain.npz")),
                cave_masks_npz_path: Some(PathBuf::from("nonexistent_cave.npz")),
                ui_anchors_npz_path: Some(PathBuf::from("nonexistent_ui.npz")),
                legacy_underground_db_path: None,
            },
            None,
        );
        let _ = &mut loc;
        assert!(loc.ui_anchors().is_empty());

        let paint_disk = |img: &mut Array3<u8>, cx: i32, cy: i32, r: i32, bgr: [u8; 3]| {
            let (h, w, _) = img.dim();
            let y0 = (cy - r).max(0) as usize;
            let y1 = (cy + r).min(h as i32 - 1) as usize;
            let x0 = (cx - r).max(0) as usize;
            let x1 = (cx + r).min(w as i32 - 1) as usize;
            let r2 = r * r;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let dx = x as i32 - cx;
                    let dy = y as i32 - cy;
                    if dx * dx + dy * dy <= r2 {
                        img[[y, x, 0]] = bgr[0];
                        img[[y, x, 1]] = bgr[1];
                        img[[y, x, 2]] = bgr[2];
                    }
                }
            }
        };

        let paint_capsule = |img: &mut Array3<u8>, cx: i32, y_top: i32, y_bot: i32| {
            let dark = [72u8, 66, 54]; // dark navy-slate pill
            for y in y_top..=y_bot {
                for x in (cx - 38)..=(cx + 38) {
                    img[[y as usize, x as usize, 0]] = dark[0];
                    img[[y as usize, x as usize, 1]] = dark[1];
                    img[[y as usize, x as usize, 2]] = dark[2];
                }
            }
            paint_disk(img, cx, y_top, 38, dark);
            paint_disk(img, cx, y_bot, 38, dark);
        };

        // Case 1: Pure Snezhnaya cyan snow / ocean background with NO elevator pill -> false!
        let mut snow = Array3::<u8>::zeros((1440, 2560, 3));
        for y in 0..1440 {
            for x in 0..2560 {
                snow[[y, x, 0]] = 235;
                snow[[y, x, 1]] = 205;
                snow[[y, x, 2]] = 110;
            }
        }
        // Add decoy cyan ice blobs at x=2360 (where old contour detector false-triggered)
        paint_disk(&mut snow, 2362, 745, 24, [245, 220, 40]);
        let r_none = loc.detect_layers_universal(snow.view());
        assert!(!r_none.has_layers, "open cyan snow must not trigger false layers: {r_none:?}");

        // Case 2: 3-floor Snezhnaya surface-active elevator (`[MA, LI, LI]` at x = 2560 - 97 = 2463)
        let mut surf_img = snow.clone();
        let xc = 2560 - 97;
        paint_capsule(&mut surf_img, xc, 634, 804);
        // Slot 0 (MA): cyan halo r=40 + big pure-white mountain core r=19
        paint_disk(&mut surf_img, xc, 634, 40, [220, 195, 40]);
        paint_disk(&mut surf_img, xc, 634, 19, [248, 248, 248]);
        // Slot 1 & 2 (LI): compact white diamond r=10 on dark capsule
        paint_disk(&mut surf_img, xc, 722, 10, [230, 230, 230]);
        paint_disk(&mut surf_img, xc, 804, 10, [230, 230, 230]);
        let r_surf = loc.detect_layers_universal(surf_img.view());
        assert!(r_surf.has_layers, "expected 3-floor surface elevator: {r_surf:?}");
        assert_eq!(r_surf.total_floors, 3);
        assert_eq!(r_surf.active_floor_idx, 0);
        assert_eq!(r_surf.surface_idx, 0);
        assert!(!r_surf.is_underground);
        assert_eq!(r_surf.floor_name, "G");

        // Case 3: 3-floor underground-active elevator (`[MI, LA, LI]`)
        let mut ug_img = snow;
        paint_capsule(&mut ug_img, xc, 624, 812);
        // Slot 0 (MI): large pure-white mountain r=19 on dark capsule
        paint_disk(&mut ug_img, xc, 624, 19, [245, 245, 245]);
        // Slot 1 (LA): vivid cyan diamond r=20 inside dark capsule
        paint_disk(&mut ug_img, xc, 722, 20, [245, 215, 25]);
        // Slot 2 (LI): compact white diamond r=10
        paint_disk(&mut ug_img, xc, 812, 10, [230, 230, 230]);
        let r_ug = loc.detect_layers_universal(ug_img.view());
        assert!(r_ug.has_layers, "expected 3-floor underground elevator: {r_ug:?}");
        assert_eq!(r_ug.total_floors, 3);
        assert_eq!(r_ug.active_floor_idx, 1);
        assert_eq!(r_ug.surface_idx, 0);
        assert!(r_ug.is_underground);
        assert_eq!(r_ug.floor_name, "B1");
    }
}
