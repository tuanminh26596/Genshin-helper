//! Cross-map progress importer service (`gh-net::cross_map_service`).
//!
//! Orchestrates:
//! 1. Cached HoYoLAB public static `point/list` + `label/tree` fetching across all 7 permanent maps.
//! 2. Dynamic Kongying Tavern guest OAuth bootstrap + cached areas/items/anchors + `list_byid`
//!    resolution for user-uploaded `Data_KYJG` backup IDs (zero hardcoded credentials).
//! 3. `POST /cross_map_import_preview` producing confidence-tiered per-map import plans.
//! 4. `GET/POST /cross_map_import_history` managing atomic 1-click Undo Rollback snapshots.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gh_core::atomic_io::write_bytes_atomic;
use gh_core::cross_map_import::{
    AppsampleMarkerLite, HoyolabPointLite, KongyingMarkerLite, MapImportPlan, PERMANENT_MAP_IDS,
    classify_kongying_item_name, kongying_area_code_to_hoyolab_map_id,
    match_appsample_map_to_hoyolab, match_kongying_map_to_hoyolab, parse_appsample_markers_v5,
    parse_hoyolab_points_lite,
};
use gh_core::{CoreError, CoreResult};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const HEADER_RPC_MAP_VERSION: HeaderName = HeaderName::from_static("x-rpc-map_version");
const HOYOLAB_STATIC_BASE: &str =
    "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc";
const KONGYING_WEB_ROOT: &str = "https://v3.yuanshen.site";
const KONGYING_API_BASE: &str = "https://cloud.yuanshen.site";

const HOYOLAB_CACHE_TTL_SECS: u64 = 12 * 3600;
const KONGYING_REF_CACHE_TTL_SECS: u64 = 24 * 3600;
const MAX_UNDO_SNAPSHOTS: usize = 10;

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn build_http_client() -> CoreResult<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0",
        ),
    );
    Client::builder()
        .default_headers(headers)
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| CoreError::Sync(format!("HTTP client init failed: {e}")))
}

fn file_is_fresh(path: &Path, ttl_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age.as_secs() < ttl_secs)
}

// ---------------------------------------------------------------------------
// HoYoLAB Public Static Point/Tree Cache
// ---------------------------------------------------------------------------

async fn fetch_hoyolab_map_points_cached(
    client: &Client,
    cache_dir: &Path,
    map_id: i64,
) -> CoreResult<Vec<HoyolabPointLite>> {
    let pt_cache = cache_dir.join(format!("crossmap_hoyo_pts_map{map_id}_v1.json"));
    let tree_cache = cache_dir.join(format!("crossmap_hoyo_tree_map{map_id}_v1.json"));

    let pt_val: Value = if file_is_fresh(&pt_cache, HOYOLAB_CACHE_TTL_SECS)
        && let Ok(raw) = std::fs::read_to_string(&pt_cache)
        && let Ok(v) = serde_json::from_str(&raw)
    {
        v
    } else {
        let url = format!(
            "{HOYOLAB_STATIC_BASE}/v2/map/point/list?map_id={map_id}&app_sn=ys_obc&lang=en-us"
        );
        let resp = client
            .get(&url)
            .header(HEADER_RPC_MAP_VERSION, HeaderValue::from_static("4.5"))
            .send()
            .await
            .map_err(|e| CoreError::Sync(format!("HoYoLAB point/list map={map_id}: {e}")))?;
        let text = resp
            .text()
            .await
            .map_err(|e| CoreError::Sync(format!("HoYoLAB point/list read map={map_id}: {e}")))?;
        let v: Value = serde_json::from_str(&text)?;
        let _ = std::fs::create_dir_all(cache_dir);
        let _ = write_bytes_atomic(&pt_cache, text.as_bytes());
        v
    };

    let tree_val: Option<Value> = if file_is_fresh(&tree_cache, HOYOLAB_CACHE_TTL_SECS)
        && let Ok(raw) = std::fs::read_to_string(&tree_cache)
        && let Ok(v) = serde_json::from_str(&raw)
    {
        Some(v)
    } else {
        let url = format!(
            "{HOYOLAB_STATIC_BASE}/v1/map/label/tree?map_id={map_id}&app_sn=ys_obc&lang=en-us"
        );
        match client
            .get(&url)
            .header(HEADER_RPC_MAP_VERSION, HeaderValue::from_static("4.5"))
            .send()
            .await
        {
            Ok(resp) => match resp.text().await {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        let _ = std::fs::create_dir_all(cache_dir);
                        let _ = write_bytes_atomic(&tree_cache, text.as_bytes());
                        Some(v)
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            },
            Err(_) => None,
        }
    };

    parse_hoyolab_points_lite(&pt_val, tree_val.as_ref())
}

// ---------------------------------------------------------------------------
// Kongying Tavern Dynamic Guest Bootstrap & Cached Reference Catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KongyingItemMeta {
    area_code: String,
    map_id: i64,
    item_name: String,
    group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KongyingRefCache {
    fetched_at: u64,
    item_meta: HashMap<i64, KongyingItemMeta>,
    anchor_markers: Vec<KongyingMarkerLite>,
}

fn extract_script_srcs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(idx) = lower[pos..].find("<script") {
        let start = pos + idx;
        let Some(end_rel) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + end_rel + 1];
        let tag_lower = &lower[start..start + end_rel + 1];
        if let Some(src_idx) = tag_lower.find("src=") {
            let after = &tag[src_idx + 4..];
            let trimmed = after.trim_start();
            if let Some(q) = trimmed.bytes().next().filter(|&b| b == b'"' || b == b'\'') {
                let rest = &trimmed[1..];
                if let Some(close) = rest.find(q as char) {
                    out.push(rest[..close].to_owned());
                }
            }
        }
        pos = start + end_rel + 1;
    }
    out
}

fn extract_guest_basic_token(js: &str) -> Option<String> {
    let bytes = js.as_bytes();
    let needle = b"Basic ";
    let mut pos = 0;
    while pos + needle.len() < bytes.len() {
        let Some(rel) = js[pos..].find("Basic ") else {
            break;
        };
        let start = pos + rel;
        let mut end = start + needle.len();
        while end < bytes.len() {
            let b = bytes[end];
            if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' {
                end += 1;
            } else {
                break;
            }
        }
        let b64_len = end - (start + needle.len());
        if (8..=80).contains(&b64_len) {
            return Some(js[start..end].to_owned());
        }
        pos = end;
    }
    None
}

async fn post_kongying_json_retry(
    client: &Client,
    url: &str,
    auth_header: &str,
    body: Option<&Value>,
) -> CoreResult<Value> {
    let mut last_err = String::new();
    for attempt in 0..3 {
        let mut req = client.post(url).header(AUTHORIZATION, auth_header);
        if let Some(b) = body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = format!("JSON decode failed: {e}"),
            },
            Ok(resp) => {
                last_err = format!("HTTP {}", resp.status().as_u16());
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(600 * (attempt + 1) as u64)).await;
    }
    Err(CoreError::Sync(format!(
        "Kongying API request failed ({url}): {last_err}"
    )))
}

async fn obtain_kongying_guest_bearer(client: &Client) -> CoreResult<String> {
    let html = client
        .get(KONGYING_WEB_ROOT)
        .send()
        .await
        .map_err(|e| CoreError::Sync(format!("Kongying web root fetch failed: {e}")))?
        .text()
        .await
        .map_err(|e| CoreError::Sync(format!("Kongying web root read failed: {e}")))?;

    let mut guest_basic: Option<String> = None;
    for src in extract_script_srcs(&html) {
        if !src.ends_with(".js") {
            continue;
        }
        let full_url = if src.starts_with("http://") || src.starts_with("https://") {
            src
        } else {
            format!("{KONGYING_WEB_ROOT}/{}", src.trim_start_matches('/'))
        };
        if let Ok(resp) = client.get(&full_url).send().await
            && let Ok(js) = resp.text().await
            && let Some(tok) = extract_guest_basic_token(&js)
        {
            guest_basic = Some(tok);
            break;
        }
    }

    let basic = guest_basic.ok_or_else(|| {
        CoreError::Sync("Could not discover Kongying public guest OAuth bootstrap".into())
    })?;
    let token_url = format!("{KONGYING_API_BASE}/oauth/token?scope=all&grant_type=client_credentials");
    let tok_json = post_kongying_json_retry(client, &token_url, &basic, None).await?;
    let access = tok_json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Sync("Kongying OAuth response missing access_token".into()))?;
    Ok(format!("Bearer {access}"))
}

fn raw_kongying_marker_to_lite(
    m: &Value,
    item_meta: &HashMap<i64, KongyingItemMeta>,
) -> Option<KongyingMarkerLite> {
    const PRIORITY: &[&str] = &[
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

    let obj = m.as_object()?;
    let id = obj.get("id")?.as_i64()?;
    let pos_str = obj.get("position")?.as_str()?;
    let mut coords = pos_str.split(',');
    let x = coords.next()?.trim().parse::<f64>().ok()?;
    let y = coords.next()?.trim().parse::<f64>().ok()?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    let item_list = obj.get("itemList").and_then(Value::as_array)?;
    let mut metas: Vec<&KongyingItemMeta> = Vec::new();
    for entry in item_list {
        if let Some(iid) = entry.get("itemId").and_then(Value::as_i64)
            && let Some(meta) = item_meta.get(&iid)
        {
            metas.push(meta);
        }
    }
    let first = *metas.first()?;

    let mut group_set: BTreeSet<&str> = BTreeSet::new();
    let mut names = Vec::new();
    for meta in &metas {
        if !meta.item_name.is_empty() {
            names.push(meta.item_name.as_str());
        }
        if let Some(g) = meta.group.as_deref() {
            group_set.insert(g);
        }
    }
    let primary_group = PRIORITY
        .iter()
        .copied()
        .find(|g| group_set.contains(g))
        .or_else(|| group_set.iter().next().copied())
        .unwrap_or("Other")
        .to_owned();

    let is_underground = obj
        .get("extra")
        .and_then(|e| e.get("underground"))
        .and_then(|u| u.get("is_underground"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Some(KongyingMarkerLite {
        id,
        title: obj
            .get("markerTitle")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        x,
        y,
        map_id: first.map_id,
        area_code: first.area_code.clone(),
        primary_group,
        all_groups: group_set.into_iter().map(str::to_owned).collect(),
        item_names: names.join(" "),
        is_underground,
    })
}

async fn load_or_fetch_kongying_ref(
    client: &Client,
    cache_dir: &Path,
    bearer: &str,
) -> CoreResult<KongyingRefCache> {
    let ref_path = cache_dir.join("crossmap_kongying_ref_v1.json");
    if file_is_fresh(&ref_path, KONGYING_REF_CACHE_TTL_SECS)
        && let Ok(raw) = std::fs::read_to_string(&ref_path)
        && let Ok(cached) = serde_json::from_str::<KongyingRefCache>(&raw)
        && !cached.anchor_markers.is_empty()
    {
        return Ok(cached);
    }

    // 1. Areas
    let areas_url = format!("{KONGYING_API_BASE}/api/area/get/list");
    let areas_val = post_kongying_json_retry(
        client,
        &areas_url,
        bearer,
        Some(&json!({"parentId": -1, "isTraverse": true})),
    )
    .await?;
    let mut area_map: HashMap<i64, (String, i64)> = HashMap::new();
    if let Some(arr) = areas_val.get("data").and_then(Value::as_array) {
        for a in arr {
            if let (Some(id), Some(code)) = (
                a.get("id").and_then(Value::as_i64),
                a.get("code").and_then(Value::as_str),
            ) && let Some(mid) = kongying_area_code_to_hoyolab_map_id(code)
            {
                area_map.insert(id, (code.to_owned(), mid));
            }
        }
    }

    // 2. Items (paginated, size 500)
    let items_url = format!("{KONGYING_API_BASE}/api/item/get/list");
    let mut item_meta: HashMap<i64, KongyingItemMeta> = HashMap::new();
    let mut anchor_item_ids: Vec<i64> = Vec::new();
    for page in 1..=15 {
        let r = post_kongying_json_retry(
            client,
            &items_url,
            bearer,
            Some(&json!({"current": page, "size": 500})),
        )
        .await?;
        let Some(rec) = r
            .get("data")
            .and_then(|d| d.get("record"))
            .and_then(Value::as_array)
        else {
            break;
        };
        let rec_len = rec.len();
        for it in rec {
            let Some(iid) = it.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let Some(aid) = it.get("areaId").and_then(Value::as_i64) else {
                continue;
            };
            let Some((code, mid)) = area_map.get(&aid) else {
                continue;
            };
            let name = it
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let grp = classify_kongying_item_name(&name).map(str::to_owned);
            if let Some(g) = grp.as_deref()
                && matches!(
                    g,
                    "Statue"
                        | "Teleport"
                        | "Domain"
                        | "Anemoculus"
                        | "Geoculus"
                        | "Electroculus"
                        | "Dendroculus"
                        | "Hydroculus"
                        | "Pyroculus"
                        | "Lunoculus"
                        | "Cryoculus"
                )
            {
                anchor_item_ids.push(iid);
            }
            item_meta.insert(
                iid,
                KongyingItemMeta {
                    area_code: code.clone(),
                    map_id: *mid,
                    item_name: name,
                    group: grp,
                },
            );
        }
        let total = r
            .get("data")
            .and_then(|d| d.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if rec_len < 500 || (page * 500) >= total {
            break;
        }
    }

    // 3. Calibration anchors (Statues + Teleports + Domains + Oculi across all 7 permanent maps)
    let byinfo_url = format!("{KONGYING_API_BASE}/api/marker/get/list_byinfo");
    let mut anchors_by_id: HashMap<i64, KongyingMarkerLite> = HashMap::new();
    for chunk in anchor_item_ids.chunks(25) {
        let r = post_kongying_json_retry(
            client,
            &byinfo_url,
            bearer,
            Some(&json!({"itemIdList": chunk})),
        )
        .await?;
        if let Some(list) = r.get("data").and_then(Value::as_array) {
            for m in list {
                if let Some(lite) = raw_kongying_marker_to_lite(m, &item_meta) {
                    anchors_by_id.insert(lite.id, lite);
                }
            }
        }
    }

    let cache = KongyingRefCache {
        fetched_at: now_unix_secs(),
        item_meta,
        anchor_markers: anchors_by_id.into_values().collect(),
    };
    let _ = std::fs::create_dir_all(cache_dir);
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = write_bytes_atomic(&ref_path, &bytes);
    }
    Ok(cache)
}

async fn resolve_kongying_user_markers(
    client: &Client,
    cache_dir: &Path,
    user_ids: &[i64],
) -> CoreResult<(Vec<KongyingMarkerLite>, Vec<KongyingMarkerLite>)> {
    let bearer = obtain_kongying_guest_bearer(client).await?;
    let ref_cache = load_or_fetch_kongying_ref(client, cache_dir, &bearer).await?;

    let byid_url = format!("{KONGYING_API_BASE}/api/marker/get/list_byid");
    let mut resolved: HashMap<i64, KongyingMarkerLite> = HashMap::with_capacity(user_ids.len());
    for chunk in user_ids.chunks(400) {
        let r = post_kongying_json_retry(
            client,
            &byid_url,
            &bearer,
            Some(&json!({"markerIdList": chunk})),
        )
        .await?;
        if let Some(list) = r.get("data").and_then(Value::as_array) {
            for m in list {
                if let Some(lite) = raw_kongying_marker_to_lite(m, &ref_cache.item_meta) {
                    resolved.insert(lite.id, lite);
                }
            }
        }
    }
    Ok((ref_cache.anchor_markers, resolved.into_values().collect()))
}

// ---------------------------------------------------------------------------
// Request Helpers & Unified Preview Orchestrator
// ---------------------------------------------------------------------------

fn parse_i64_array_loose(v: &Value) -> Vec<i64> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    for item in arr {
        if let Some(i) = item.as_i64() {
            seen.insert(i);
        } else if let Some(s) = item.as_str()
            && let Ok(i) = s.trim().parse::<i64>()
        {
            seen.insert(i);
        }
    }
    seen.into_iter().collect()
}

fn parse_already_marked_by_map(req: &Map<String, Value>) -> HashMap<i64, HashSet<i64>> {
    let mut out: HashMap<i64, HashSet<i64>> = HashMap::new();
    let Some(obj) = req
        .get("already_marked_by_map")
        .or_else(|| req.get("hoyolab_marked_by_map"))
        .and_then(Value::as_object)
    else {
        return out;
    };
    for (k, v) in obj {
        let Ok(mid) = k.trim().parse::<i64>() else {
            continue;
        };
        let ids: HashSet<i64> = parse_i64_array_loose(v).into_iter().collect();
        out.insert(mid, ids);
    }
    out
}

fn parse_inline_hoyolab_points_by_map(
    req: &Map<String, Value>,
) -> HashMap<i64, Vec<HoyolabPointLite>> {
    let mut out = HashMap::new();
    let Some(obj) = req.get("hoyolab_points_by_map").and_then(Value::as_object) else {
        return out;
    };
    let trees = req.get("hoyolab_trees_by_map").and_then(Value::as_object);
    for (k, v) in obj {
        let Ok(mid) = k.trim().parse::<i64>() else {
            continue;
        };
        let tree = trees.and_then(|t| t.get(k));
        if let Ok(pts) = parse_hoyolab_points_lite(v, tree) {
            out.insert(mid, pts);
        }
    }
    out
}

fn extract_kongying_ids(req: &Map<String, Value>) -> Vec<i64> {
    if let Some(v) = req.get("kongying_ids") {
        let ids = parse_i64_array_loose(v);
        if !ids.is_empty() {
            return ids;
        }
    }
    if let Some(payload) = req.get("kongying_payload").or_else(|| req.get("payload")) {
        if let Some(arr) = payload.as_array() {
            return parse_i64_array_loose(&Value::Array(arr.clone()));
        }
        if let Some(obj) = payload.as_object() {
            if let Some(v) = obj.get("Data_KYJG") {
                return parse_i64_array_loose(v);
            }
            if let Some(v) = obj.get("marked_layers") {
                return parse_i64_array_loose(v);
            }
            // Also support Kongying web gist export: { files: { Data_KYJG: { content: "[...]" } } }
            if let Some(content) = obj
                .get("files")
                .and_then(|f| f.get("Data_KYJG"))
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
                && let Ok(parsed) = serde_json::from_str::<Value>(content)
            {
                return parse_i64_array_loose(&parsed);
            }
        }
    }
    Vec::new()
}

pub async fn execute_cross_map_import_preview(
    req: &Map<String, Value>,
    cache_dir: &Path,
) -> CoreResult<Value> {
    let source = req
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(source.as_str(), "appsample" | "kongying") {
        return Err(CoreError::InvalidArgument(format!(
            "Unsupported import source: {source:?} (expected \"appsample\" or \"kongying\")"
        )));
    }

    let filter_map_id = req.get("map_id").and_then(Value::as_i64);
    let already_by_map = parse_already_marked_by_map(req);
    let inline_hoyo = parse_inline_hoyolab_points_by_map(req);
    let client = build_http_client()?;

    let mut map_plans: Vec<MapImportPlan> = Vec::new();

    if source == "appsample" {
        let raw_catalog = req
            .get("catalog")
            .or_else(|| req.get("markers"))
            .or_else(|| req.get("appsample_payload"))
            .ok_or_else(|| {
                CoreError::InvalidArgument(
                    "Appsample import requires 'markers' or 'catalog' payload".into(),
                )
            })?;
        let catalog: Vec<AppsampleMarkerLite> = parse_appsample_markers_v5(raw_catalog)?;
        let found_set: Option<HashSet<i64>> = req
            .get("found_ids")
            .or_else(|| req.get("ids"))
            .map(|v| parse_i64_array_loose(v).into_iter().collect());

        // Determine which permanent map IDs have marked Appsample pins
        let mut touched_maps: BTreeSet<i64> = BTreeSet::new();
        for m in &catalog {
            if !PERMANENT_MAP_IDS.contains(&m.map_id) {
                continue;
            }
            if let Some(target_mid) = filter_map_id
                && m.map_id != target_mid
            {
                continue;
            }
            if found_set.as_ref().is_none_or(|s| s.contains(&m.id)) {
                touched_maps.insert(m.map_id);
            }
        }

        for mid in touched_maps {
            let hoyo_pts = if let Some(pts) = inline_hoyo.get(&mid) {
                pts.clone()
            } else {
                fetch_hoyolab_map_points_cached(&client, cache_dir, mid).await?
            };
            let empty_already = HashSet::new();
            let already = already_by_map.get(&mid).unwrap_or(&empty_already);
            let plan = match_appsample_map_to_hoyolab(
                mid,
                &catalog,
                found_set.as_ref(),
                &hoyo_pts,
                already,
            );
            map_plans.push(plan);
        }
    } else {
        // Kongying Tavern
        let (anchors, user_markers): (Vec<KongyingMarkerLite>, Vec<KongyingMarkerLite>) =
            if let (Some(a_val), Some(u_val)) = (
                req.get("kongying_anchors"),
                req.get("kongying_user_markers"),
            ) {
                (
                    serde_json::from_value(a_val.clone())?,
                    serde_json::from_value(u_val.clone())?,
                )
            } else {
                let ids = extract_kongying_ids(req);
                if ids.is_empty() {
                    return Err(CoreError::InvalidArgument(
                        "Kongying import requires non-empty Data_KYJG / kongying_ids".into(),
                    ));
                }
                resolve_kongying_user_markers(&client, cache_dir, &ids).await?
            };

        let mut touched_maps: BTreeSet<i64> = BTreeSet::new();
        for m in &user_markers {
            if !PERMANENT_MAP_IDS.contains(&m.map_id) {
                continue;
            }
            if let Some(target_mid) = filter_map_id
                && m.map_id != target_mid
            {
                continue;
            }
            touched_maps.insert(m.map_id);
        }

        for mid in touched_maps {
            let hoyo_pts = if let Some(pts) = inline_hoyo.get(&mid) {
                pts.clone()
            } else {
                fetch_hoyolab_map_points_cached(&client, cache_dir, mid).await?
            };
            let empty_already = HashSet::new();
            let already = already_by_map.get(&mid).unwrap_or(&empty_already);
            let plan = match_kongying_map_to_hoyolab(
                mid,
                &anchors,
                &user_markers,
                &hoyo_pts,
                already,
            );
            map_plans.push(plan);
        }
    }

    let mut tot_src = 0_usize;
    let mut tot_already = 0_usize;
    let mut tot_exact = 0_usize;
    let mut tot_high = 0_usize;
    let mut tot_med = 0_usize;
    let mut tot_amb = 0_usize;
    let mut tot_unm = 0_usize;
    let mut tot_cats: BTreeMap<String, usize> = BTreeMap::new();
    for p in &map_plans {
        tot_src += p.source_count;
        tot_already += p.already_marked_count;
        tot_exact += p.exact_ids.len();
        tot_high += p.high_ids.len();
        tot_med += p.medium_ids.len();
        tot_amb += p.ambiguous_ids.len();
        tot_unm += p.unmatched_count;
        for (k, v) in &p.category_counts {
            *tot_cats.entry(k.clone()).or_insert(0) += v;
        }
    }

    Ok(json!({
        "ok": true,
        "source": source,
        "maps": map_plans,
        "totals": {
            "source_pins": tot_src,
            "already_marked": tot_already,
            "exact": tot_exact,
            "high": tot_high,
            "medium": tot_med,
            "ambiguous": tot_amb,
            "unmatched": tot_unm,
            "ready_new_default": tot_exact + tot_high + tot_med,
            "category_counts": tot_cats,
        }
    }))
}

// ---------------------------------------------------------------------------
// 1-Click Undo Rollback Snapshot Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportUndoSnapshot {
    pub id: String,
    pub created_at: u64,
    pub source: String,
    pub total_applied: usize,
    pub maps: BTreeMap<String, Vec<i64>>,
    #[serde(default)]
    pub reverted: bool,
    #[serde(default)]
    pub reverted_at: Option<u64>,
}

fn history_file_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("crossmap_import_history.json")
}

pub fn load_import_history(cache_dir: &Path) -> Vec<ImportUndoSnapshot> {
    let p = history_file_path(cache_dir);
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<ImportUndoSnapshot>>(&raw).unwrap_or_default()
}

fn save_import_history(cache_dir: &Path, items: &[ImportUndoSnapshot]) -> CoreResult<()> {
    let _ = std::fs::create_dir_all(cache_dir);
    let bytes = serde_json::to_vec_pretty(items)?;
    write_bytes_atomic(&history_file_path(cache_dir), &bytes)?;
    Ok(())
}

pub fn handle_import_history_command(
    req: Option<&Map<String, Value>>,
    cache_dir: &Path,
) -> CoreResult<Value> {
    let Some(obj) = req else {
        let snapshots = load_import_history(cache_dir);
        return Ok(json!({ "ok": true, "snapshots": snapshots }));
    };
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list")
        .trim()
        .to_ascii_lowercase();

    match action.as_str() {
        "list" | "" => {
            let snapshots = load_import_history(cache_dir);
            Ok(json!({ "ok": true, "snapshots": snapshots }))
        }
        "save" => {
            let snap_val = obj
                .get("snapshot")
                .ok_or_else(|| CoreError::InvalidArgument("Missing snapshot object".into()))?;
            let mut snap: ImportUndoSnapshot = serde_json::from_value(snap_val.clone())?;
            if snap.id.trim().is_empty() {
                snap.id = format!("imp_{}", now_unix_secs());
            }
            if snap.created_at == 0 {
                snap.created_at = now_unix_secs();
            }
            let mut list = load_import_history(cache_dir);
            list.retain(|s| s.id != snap.id);
            list.insert(0, snap.clone());
            list.truncate(MAX_UNDO_SNAPSHOTS);
            save_import_history(cache_dir, &list)?;
            Ok(json!({ "ok": true, "snapshot": snap, "snapshots": list }))
        }
        "mark_reverted" => {
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let mut list = load_import_history(cache_dir);
            let mut found = false;
            for s in &mut list {
                if s.id == id {
                    s.reverted = true;
                    s.reverted_at = Some(now_unix_secs());
                    found = true;
                }
            }
            save_import_history(cache_dir, &list)?;
            Ok(json!({ "ok": true, "updated": found, "snapshots": list }))
        }
        "delete" => {
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let mut list = load_import_history(cache_dir);
            let before = list.len();
            list.retain(|s| s.id != id);
            save_import_history(cache_dir, &list)?;
            Ok(json!({ "ok": true, "deleted": list.len() < before, "snapshots": list }))
        }
        other => Err(CoreError::InvalidArgument(format!(
            "Unsupported import history action: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::cross_map_import::appsample_seed_affines;

    #[test]
    fn script_src_and_guest_token_extractors_work() {
        let html = r#"<!doctype html><script type="module" src="/assets/index.abc12345.js"></script>"#;
        assert_eq!(extract_script_srcs(html), vec!["/assets/index.abc12345.js"]);

        let sample_js = "const h = 'Basic dGVzdF9ndWVzdF90b2tlbg==';";
        assert_eq!(
            extract_guest_basic_token(sample_js).as_deref(),
            Some("Basic dGVzdF9ndWVzdF90b2tlbg==")
        );
    }

    #[tokio::test]
    async fn offline_preview_and_undo_history_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "gh_crossmap_service_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let af = appsample_seed_affines(2)[0];
        let (lng, lat) = af.inverse_apply(-500.0, 200.0);
        let req_val = json!({
            "source": "appsample",
            "found_ids": [9001],
            "markers": [
                {"id": 9001, "type": "o17", "mid": 2, "level": 0, "lng": lng, "lat": lat, "meta": ""}
            ],
            "hoyolab_points_by_map": {
                "2": {
                    "data": {
                        "point_list": [
                            {"id": 77001, "label_id": 17, "x_pos": -500.0, "y_pos": 200.0}
                        ]
                    }
                }
            },
            "already_marked_by_map": { "2": [] }
        });
        let preview =
            execute_cross_map_import_preview(req_val.as_object().unwrap(), &tmp)
                .await
                .unwrap();
        assert_eq!(preview["ok"], true);
        assert_eq!(preview["totals"]["exact"], 1);
        assert_eq!(preview["maps"][0]["exact_ids"], json!([77001]));

        // Undo snapshot store roundtrip
        let save_req = json!({
            "action": "save",
            "snapshot": {
                "id": "imp_test_1",
                "created_at": 1700000000,
                "source": "appsample",
                "total_applied": 1,
                "maps": { "2": [77001] }
            }
        });
        let saved = handle_import_history_command(save_req.as_object(), &tmp).unwrap();
        assert_eq!(saved["snapshots"].as_array().unwrap().len(), 1);

        let rev_req = json!({ "action": "mark_reverted", "id": "imp_test_1" });
        let rev = handle_import_history_command(rev_req.as_object(), &tmp).unwrap();
        assert_eq!(rev["snapshots"][0]["reverted"], true);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
