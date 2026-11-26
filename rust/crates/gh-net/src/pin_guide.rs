//! Bilingual `(lang_tag, pin_id)` `PinGuideFetcher` & `HoYoLABLabelCatalog` (`src/pin_guide.py`).

use crate::hd_assets::run_blocking_http;
use gh_core::{CoreError, CoreResult, atomic_io};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

pub const API_BASE: &str = "https://sg-public-api.hoyolab.com/common/map_user/ys_obc";
pub const MAX_CACHE: usize = 64;
pub const MAX_CACHE_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_POINT_INFO_JSON_BYTES: usize = 512 * 1024;
pub const MAX_COMMENT_LIST_JSON_BYTES: usize = 512 * 1024;
pub const MAX_LABEL_TREE_JSON_BYTES: usize = 2 * 1024 * 1024;
pub const IMAGE_RESIZE_QS: &str = "x-oss-process=image/resize,w_960";
pub const LABEL_RETRY_BACKOFF_SECS: f64 = 60.0;
const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) GenshinHelper";

pub fn short_lang(lang: Option<&str>) -> &'static str {
    let normalized = lang.unwrap_or("vi").trim().to_ascii_lowercase();
    if normalized.starts_with("vi") {
        "vi"
    } else {
        "en"
    }
}

pub fn hoyolab_lang_tag(lang: Option<&str>) -> &'static str {
    if short_lang(lang) == "vi" {
        "vi-vn"
    } else {
        "en-us"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinGuide {
    pub pin_id: String,
    pub description: String,
    pub image_url: String,
    pub image_bytes: Option<Vec<u8>>,
    pub ok: bool,
    pub lang: String,
}

/// Strips HoYoLAB rich-text markup down to plain multi-line text.
pub fn clean_guide_text(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }

    // 1. Convert <br>, <br/>, <br /> and </p> into newlines, strip all other tags.
    let mut stripped = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(lt) = rest.find('<') {
        stripped.push_str(&rest[..lt]);
        let after_lt = &rest[lt + 1..];
        if let Some(gt) = after_lt.find('>') {
            let tag_inner = &after_lt[..gt];
            if is_break_tag(tag_inner) {
                stripped.push('\n');
            }
            rest = &after_lt[gt + 1..];
        } else {
            stripped.push('<');
            stripped.push_str(after_lt);
            rest = "";
            break;
        }
    }
    stripped.push_str(rest);

    // 2. Unescape HTML entities.
    let unescaped = unescape_html(&stripped);

    // 3. Normalize line whitespace and drop blank lines.
    let normalized = unescaped.replace('\r', "");
    let lines: Vec<String> = normalized
        .split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect();
    lines.join("\n").trim().to_owned()
}

fn is_break_tag(tag_inner: &str) -> bool {
    let trimmed = tag_inner.trim();
    if let Some(after_slash) = trimmed.strip_prefix('/') {
        return after_slash.trim().eq_ignore_ascii_case("p");
    }
    let without_trailing_slash = trimmed.strip_suffix('/').unwrap_or(trimmed).trim();
    without_trailing_slash.eq_ignore_ascii_case("br")
}

fn unescape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        if let Some(semi) = after_amp.find(';') {
            // Limit entity length so stray '&' doesn't swallow whole paragraphs.
            if semi > 0 && semi <= 12 {
                let entity = &after_amp[..semi];
                if let Some(decoded) = decode_html_entity(entity) {
                    out.push(decoded);
                    rest = &after_amp[semi + 1..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    out
}

fn decode_html_entity(entity: &str) -> Option<char> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return char::from_u32(code);
    }

    match entity {
        "nbsp" => Some('\u{00A0}'),
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "ndash" => Some('\u{2013}'),
        "mdash" => Some('\u{2014}'),
        "hellip" => Some('\u{2026}'),
        "bull" => Some('\u{2022}'),
        "times" => Some('\u{00D7}'),
        "deg" => Some('\u{00B0}'),
        "copy" => Some('\u{00A9}'),
        "reg" => Some('\u{00AE}'),
        "trade" => Some('\u{2122}'),
        _ => None,
    }
}

fn url_path_is_gif(url: &str) -> bool {
    let before_query_frag = url.split(['?', '#']).next().unwrap_or(url);
    let path = if let Some(scheme_pos) = before_query_frag.find("://") {
        let after_scheme = &before_query_frag[scheme_pos + 3..];
        match after_scheme.find('/') {
            Some(slash_pos) => &after_scheme[slash_pos..],
            None => "",
        }
    } else {
        before_query_frag
    };
    path.trim().to_ascii_lowercase().ends_with(".gif")
}

const ALLOWED_IMAGE_DOMAINS: &[&str] = &["hoyoverse.com", "hoyolab.com", "mihoyo.com"];

/// Additional HTTPS image hosts verified for third-party map providers (Kongying, AppSample,
/// MapGenie, GameDot) used strictly for extension-supplied inline guide images (SSRF-safe).
const ALLOWED_PROVIDER_IMAGE_DOMAINS: &[&str] = &[
    "hoyoverse.com",
    "hoyolab.com",
    "mihoyo.com",
    "assets.yuanshen.site",
    "tiles.yuanshen.site",
    "game-cdn.appsample.com",
    "media.mapgenie.io",
    "assets.gamedot.org",
];

fn host_matches_allowlist(host: &str, allowlist: &[&str]) -> bool {
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return false;
    }
    allowlist.iter().any(|d| {
        host == *d
            || host
                .strip_suffix(d)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn is_allowed_hoyolab_host(host: &str) -> bool {
    host_matches_allowlist(host, ALLOWED_IMAGE_DOMAINS)
}

fn extract_verified_https_host(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let rest = trimmed.strip_prefix("https://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(':')
        || authority.contains('\\')
    {
        return None;
    }
    Some(authority.to_ascii_lowercase())
}

/// Returns `true` iff `url` is a valid HTTPS URL pointing to an official HoYoLAB /
/// HoYoverse / miHoYo image host (SSRF-safe).
pub fn is_allowed_image_url(url: &str) -> bool {
    extract_verified_https_host(url).is_some_and(|h| is_allowed_hoyolab_host(&h))
}

/// Returns `true` iff `url` is a valid HTTPS URL pointing to an allowlisted official or
/// verified third-party map provider image CDN (SSRF-safe).
pub fn is_allowed_provider_image_url(url: &str) -> bool {
    extract_verified_https_host(url)
        .is_some_and(|h| host_matches_allowlist(&h, ALLOWED_PROVIDER_IMAGE_DOMAINS))
}

fn http_loopback_authority(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    if trimmed
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let rest = trimmed.strip_prefix("http://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
        return None;
    }
    let (host, port_opt) = if let Some(after_open) = authority.strip_prefix('[') {
        let close = after_open.find(']')?;
        let host = &after_open[..close];
        let tail = &after_open[close + 1..];
        let port_opt = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':')?)
        };
        (host, port_opt)
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if let Some(port) = port_opt
        && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let is_loopback =
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
    is_loopback.then_some(authority)
}

fn is_allowed_image_url_for_base(url: &str, api_base: &str) -> bool {
    if is_allowed_image_url(url) {
        return true;
    }
    let Some(base_auth) = http_loopback_authority(api_base) else {
        return false;
    };
    let Some(img_auth) = http_loopback_authority(url) else {
        return false;
    };
    img_auth.eq_ignore_ascii_case(base_auth)
}

pub fn resized_image_url(url: &str) -> String {
    if url.is_empty() || url.contains("x-oss-process") {
        return url.to_owned();
    }
    let Some(scheme_pos) = url.find("://") else {
        return url.to_owned();
    };
    let after_scheme = &url[scheme_pos + 3..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let netloc = &after_scheme[..authority_end];
    let host = netloc
        .split(':')
        .next()
        .unwrap_or(netloc)
        .to_ascii_lowercase();
    if !is_allowed_hoyolab_host(&host) {
        return url.to_owned();
    }
    let has_query = after_scheme
        .split('#')
        .next()
        .is_some_and(|before_frag| before_frag.contains('?'));
    let sep = if has_query { '&' } else { '?' };
    format!("{url}{sep}{IMAGE_RESIZE_QS}")
}

pub type JsonFetchHook = Arc<dyn Fn(&str) -> Result<Value, String> + Send + Sync>;
pub type BytesFetchHook = Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;
pub type GuideCallback = Box<dyn FnOnce(PinGuide) + Send>;

/// Extensible guide provider abstraction implemented by [`PinGuideFetcher`]
/// so future verified guide backends can plug in without modifying callers.
pub trait GuideProvider: Send + Sync {
    fn provider_name(&self) -> &'static str;
    fn is_fetchable(&self, pin_id: &str) -> bool;
    fn get_cached(&self, pin_id: &str, lang: Option<&str>) -> Option<PinGuide>;
    fn is_loading(&self, pin_id: &str, lang: Option<&str>) -> bool;
    fn fetch_guide(&self, pin_id: &str, map_id: i32, lang: Option<&str>) -> PinGuide;
    fn request_guide(
        &self,
        pin_id: &str,
        map_id: i32,
        on_done: GuideCallback,
        lang: Option<&str>,
    ) -> bool;
}

pub struct PinGuideConfig {
    pub lang_getter: Arc<dyn Fn() -> String + Send + Sync>,
    pub api_base: String,
    pub fetch_json: Option<JsonFetchHook>,
    pub fetch_bytes: Option<BytesFetchHook>,
    pub timeout: Duration,
}

impl Default for PinGuideConfig {
    fn default() -> Self {
        Self {
            lang_getter: Arc::new(|| "vi".to_owned()),
            api_base: API_BASE.to_owned(),
            fetch_json: None,
            fetch_bytes: None,
            timeout: Duration::from_secs(8),
        }
    }
}

enum GuideJobKind {
    Hoyolab { map_id: i32 },
    Inline { description: String, image_url: String },
}

type PendingJob = (String, String, GuideJobKind, GuideCallback);

#[derive(Default)]
struct PinGuideState {
    cache: VecDeque<((String, String), PinGuide)>,
    cache_image_bytes: usize,
    pending: Option<PendingJob>,
    inflight: Option<(String, String)>,
    thread_alive: bool,
    stopped: bool,
}

#[inline]
fn guide_image_bytes(guide: &PinGuide) -> usize {
    guide.image_bytes.as_ref().map_or(0, Vec::len)
}

struct PinGuideInner {
    lang_getter: Arc<dyn Fn() -> String + Send + Sync>,
    api_base: String,
    fetch_json: Option<JsonFetchHook>,
    fetch_bytes: Option<BytesFetchHook>,
    timeout: Duration,
    state: Mutex<PinGuideState>,
    cv: Condvar,
}

#[derive(Clone)]
pub struct PinGuideFetcher {
    inner: Arc<PinGuideInner>,
}

impl std::fmt::Debug for PinGuideFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinGuideFetcher")
            .field("api_base", &self.inner.api_base)
            .field("timeout", &self.inner.timeout)
            .finish()
    }
}

impl Default for PinGuideFetcher {
    fn default() -> Self {
        Self::with_config(PinGuideConfig::default())
    }
}

impl PinGuideFetcher {
    pub fn new(lang_getter: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Self::with_config(PinGuideConfig {
            lang_getter,
            ..Default::default()
        })
    }

    pub fn with_config(cfg: PinGuideConfig) -> Self {
        let trimmed = cfg.api_base.trim().trim_end_matches('/').to_owned();
        Self {
            inner: Arc::new(PinGuideInner {
                lang_getter: cfg.lang_getter,
                api_base: if trimmed.is_empty() {
                    API_BASE.to_owned()
                } else {
                    trimmed
                },
                fetch_json: cfg.fetch_json,
                fetch_bytes: cfg.fetch_bytes,
                timeout: cfg.timeout.max(Duration::from_millis(50)),
                state: Mutex::new(PinGuideState::default()),
                cv: Condvar::new(),
            }),
        }
    }

    pub fn is_fetchable_id(pin_id: &str) -> bool {
        let trimmed = pin_id.trim();
        !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit())
    }

    fn active_lang_tag(&self, lang: Option<&str>) -> &'static str {
        match lang {
            Some(l) => hoyolab_lang_tag(Some(l)),
            None => {
                let current = (self.inner.lang_getter)();
                hoyolab_lang_tag(Some(&current))
            }
        }
    }

    pub fn get_cached(&self, pin_id: &str, lang: Option<&str>) -> Option<PinGuide> {
        let key = (self.active_lang_tag(lang).to_owned(), pin_id.to_owned());
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let pos = st.cache.iter().position(|(k, _)| k == &key)?;
        let entry = st.cache.remove(pos)?;
        let out = entry.1.clone();
        st.cache.push_back(entry);
        Some(out)
    }

    pub fn is_loading(&self, pin_id: &str, lang: Option<&str>) -> bool {
        let key = (self.active_lang_tag(lang).to_owned(), pin_id.to_owned());
        let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.inflight.as_ref() == Some(&key)
            || st
                .pending
                .as_ref()
                .is_some_and(|(lt, pid, _, _)| lt == &key.0 && pid == &key.1)
    }

    pub fn request<F>(&self, pin_id: &str, map_id: i32, on_done: F, lang: Option<&str>) -> bool
    where
        F: FnOnce(PinGuide) + Send + 'static,
    {
        self.request_boxed(pin_id, map_id, Box::new(on_done), lang)
    }

    fn request_boxed(
        &self,
        pin_id: &str,
        map_id: i32,
        on_done: GuideCallback,
        lang: Option<&str>,
    ) -> bool {
        let pid = pin_id.trim().to_owned();
        if !Self::is_fetchable_id(&pid) {
            return false;
        }
        let lang_tag = self.active_lang_tag(lang).to_owned();
        let cache_key = (lang_tag.clone(), pid.clone());

        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.stopped {
            return false;
        }
        if st.cache.iter().any(|(k, _)| k == &cache_key) {
            return false;
        }
        if st.inflight.as_ref() == Some(&cache_key) {
            return false;
        }

        st.pending = Some((lang_tag, pid, GuideJobKind::Hoyolab { map_id }, on_done));
        if !st.thread_alive {
            st.thread_alive = true;
            let inner = Arc::clone(&self.inner);
            if std::thread::Builder::new()
                .name("GH_PinGuide".to_owned())
                .spawn(move || worker_loop(inner))
                .is_err()
            {
                st.thread_alive = false;
                st.pending = None;
                return false;
            }
        }
        self.inner.cv.notify_one();
        true
    }

    /// Registers a text-only inline guide supplied directly by a third-party map provider
    /// (e.g. Kongying / AppSample) into the LRU cache without hitting HoYoLAB HTTP APIs.
    pub fn register_inline_guide(
        &self,
        pin_id: &str,
        description: &str,
        image_url: &str,
        lang: Option<&str>,
    ) {
        let pid = pin_id.trim().to_owned();
        if pid.is_empty() {
            return;
        }
        let clean_desc = clean_guide_text(description);
        let safe_img = if is_allowed_provider_image_url(image_url) {
            image_url.trim().to_owned()
        } else {
            String::new()
        };
        if clean_desc.is_empty() && safe_img.is_empty() {
            return;
        }
        let lang_tag = self.active_lang_tag(lang).to_owned();
        let cache_key = (lang_tag.clone(), pid.clone());
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, existing)) = st.cache.iter().find(|(k, _)| k == &cache_key)
            && existing.image_bytes.as_ref().is_some_and(|b| !b.is_empty())
        {
            return;
        }
        if safe_img.is_empty() {
            let guide = PinGuide {
                pin_id: pid,
                description: clean_desc,
                image_url: String::new(),
                image_bytes: None,
                ok: true,
                lang: lang_tag,
            };
            insert_guide_into_cache(&mut st, cache_key, guide);
        }
    }

    /// Queues an SSRF-safe background image fetch for a third-party map provider pin whose
    /// description and allowlisted HTTPS `image_url` were supplied by the browser extension.
    pub fn request_inline_guide<F>(
        &self,
        pin_id: &str,
        description: &str,
        image_url: &str,
        on_done: F,
        lang: Option<&str>,
    ) -> bool
    where
        F: FnOnce(PinGuide) + Send + 'static,
    {
        let pid = pin_id.trim().to_owned();
        if pid.is_empty() {
            return false;
        }
        let safe_img = if is_allowed_provider_image_url(image_url) {
            image_url.trim().to_owned()
        } else {
            String::new()
        };
        let clean_desc = clean_guide_text(description);
        if safe_img.is_empty() {
            return false;
        }
        let lang_tag = self.active_lang_tag(lang).to_owned();
        let cache_key = (lang_tag.clone(), pid.clone());

        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.stopped {
            return false;
        }
        if let Some((_, existing)) = st.cache.iter().find(|(k, _)| k == &cache_key)
            && existing.image_bytes.as_ref().is_some_and(|b| !b.is_empty())
        {
            return false;
        }
        if st.inflight.as_ref() == Some(&cache_key) {
            return false;
        }

        st.pending = Some((
            lang_tag,
            pid,
            GuideJobKind::Inline {
                description: clean_desc,
                image_url: safe_img,
            },
            Box::new(on_done),
        ));
        if !st.thread_alive {
            st.thread_alive = true;
            let inner = Arc::clone(&self.inner);
            if std::thread::Builder::new()
                .name("GH_PinGuide".to_owned())
                .spawn(move || worker_loop(inner))
                .is_err()
            {
                st.thread_alive = false;
                st.pending = None;
                return false;
            }
        }
        self.inner.cv.notify_one();
        true
    }

    pub fn stop(&self) {
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.stopped = true;
        st.pending = None;
        self.inner.cv.notify_all();
    }

    pub fn fetch_sync(&self, pin_id: &str, map_id: i32, lang: Option<&str>) -> PinGuide {
        let lang_tag = self.active_lang_tag(lang).to_owned();
        fetch_sync_inner(&self.inner, pin_id, map_id, &lang_tag)
    }
}

impl GuideProvider for PinGuideFetcher {
    fn provider_name(&self) -> &'static str {
        "hoyolab"
    }

    fn is_fetchable(&self, pin_id: &str) -> bool {
        Self::is_fetchable_id(pin_id)
    }

    fn get_cached(&self, pin_id: &str, lang: Option<&str>) -> Option<PinGuide> {
        PinGuideFetcher::get_cached(self, pin_id, lang)
    }

    fn is_loading(&self, pin_id: &str, lang: Option<&str>) -> bool {
        PinGuideFetcher::is_loading(self, pin_id, lang)
    }

    fn fetch_guide(&self, pin_id: &str, map_id: i32, lang: Option<&str>) -> PinGuide {
        PinGuideFetcher::fetch_sync(self, pin_id, map_id, lang)
    }

    fn request_guide(
        &self,
        pin_id: &str,
        map_id: i32,
        on_done: GuideCallback,
        lang: Option<&str>,
    ) -> bool {
        self.request_boxed(pin_id, map_id, on_done, lang)
    }
}

impl Drop for PinGuideFetcher {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.stop();
        }
    }
}

fn point_info_url(api_base: &str, map_id: i32, encoded_pid: &str, lang_tag: &str) -> String {
    format!(
        "{api_base}/v1/map/point/info?map_id={map_id}&point_id={encoded_pid}&app_sn=ys_obc&lang={lang_tag}"
    )
}

fn fetch_json_with_limit(
    inner: &PinGuideInner,
    url: &str,
    limit: usize,
) -> Result<Value, String> {
    match &inner.fetch_json {
        Some(hook) => hook(url),
        None => http_get_json(url, inner.timeout, limit),
    }
}

fn extract_url_from_item(item: &Value) -> Option<&str> {
    if let Some(s) = item.as_str() {
        return Some(s.trim());
    }
    let obj = item.as_object()?;
    for key in ["url", "src", "img"] {
        if let Some(trimmed) = obj.get(key).and_then(Value::as_str).map(str::trim)
            && !trimmed.is_empty()
        {
            return Some(trimmed);
        }
    }
    None
}

fn extract_first_allowed_image_from_array(arr: &[Value], api_base: &str) -> String {
    for item in arr {
        if let Some(candidate) = extract_url_from_item(item)
            && is_allowed_image_url_for_base(candidate, api_base)
        {
            return candidate.to_owned();
        }
    }
    String::new()
}

fn parse_point_info(env: &Value, api_base: &str) -> (String, String) {
    let info = env.get("data").and_then(|d| d.get("info"));
    let raw_content = info
        .and_then(|i| i.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let desc = clean_guide_text(raw_content);

    let mut img_url = info
        .and_then(|i| i.get("img"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| is_allowed_image_url_for_base(u, api_base))
        .unwrap_or_default()
        .to_owned();
    if img_url.is_empty()
        && let Some(arr) = info
            .and_then(|i| i.get("url_list"))
            .and_then(Value::as_array)
    {
        img_url = extract_first_allowed_image_from_array(arr, api_base);
    }

    (desc, img_url)
}

fn fetch_point_info(
    inner: &PinGuideInner,
    map_id: i32,
    encoded_pid: &str,
    lang_tag: &str,
) -> Option<(String, String)> {
    let url = point_info_url(&inner.api_base, map_id, encoded_pid, lang_tag);
    let env = fetch_json_with_limit(inner, &url, MAX_POINT_INFO_JSON_BYTES).ok()?;
    if !is_retcode_ok(&env) {
        return None;
    }
    Some(parse_point_info(&env, &inner.api_base))
}

fn fetch_sync_inner(
    inner: &PinGuideInner,
    pin_id: &str,
    map_id: i32,
    lang_tag: &str,
) -> PinGuide {
    let encoded_pid = url_encode_component(pin_id);
    let primary_url = point_info_url(&inner.api_base, map_id, &encoded_pid, lang_tag);

    let Ok(env) = fetch_json_with_limit(inner, &primary_url, MAX_POINT_INFO_JSON_BYTES) else {
        return PinGuide {
            pin_id: pin_id.to_owned(),
            description: String::new(),
            image_url: String::new(),
            image_bytes: None,
            ok: false,
            lang: lang_tag.to_owned(),
        };
    };

    if !is_retcode_ok(&env) {
        return PinGuide {
            pin_id: pin_id.to_owned(),
            description: String::new(),
            image_url: String::new(),
            image_bytes: None,
            ok: false,
            lang: lang_tag.to_owned(),
        };
    }

    let (mut desc, mut img_url) = parse_point_info(&env, &inner.api_base);
    let mut effective_lang = lang_tag;

    // Tier 2 (en-us point/info): backfill missing description and/or screenshot when requested
    // language lacks either field.
    if (desc.is_empty() || img_url.is_empty())
        && lang_tag != "en-us"
        && let Some((en_desc, en_img)) = fetch_point_info(inner, map_id, &encoded_pid, "en-us")
    {
        if desc.is_empty() && !en_desc.is_empty() {
            desc = en_desc;
            effective_lang = "en-us";
        }
        if img_url.is_empty() && !en_img.is_empty() {
            img_url = en_img;
            if desc.is_empty() {
                effective_lang = "en-us";
            }
        }
    }

    // Tier 3 (New regions such as Snezhnaya / Nod-Krai): when screenshot is still empty after
    // primary + en-us, borrow ONLY the language-neutral allowlisted image_url from official
    // HoYoLAB zh-cn / ja-jp point/info (never inject CJK text into VI/EN HUD descriptions).
    if img_url.is_empty() {
        for fallback_lang in ["zh-cn", "ja-jp"] {
            if fallback_lang == lang_tag {
                continue;
            }
            if let Some((_, fb_img)) = fetch_point_info(inner, map_id, &encoded_pid, fallback_lang)
                && !fb_img.is_empty()
            {
                img_url = fb_img;
                break;
            }
        }
    }

    let mut image_bytes = None;
    if !img_url.is_empty() && is_allowed_image_url_for_base(&img_url, &inner.api_base) {
        let resized = resized_image_url(&img_url);
        let img_total_timeout = inner
            .timeout
            .saturating_mul(3)
            .min(Duration::from_secs(25))
            .max(inner.timeout);
        let resized_timeout = if url_path_is_gif(&img_url) {
            inner.timeout
        } else {
            img_total_timeout
        };
        let fetch_img = |target: &str, total_timeout: Duration| -> Result<Vec<u8>, String> {
            let bytes = match &inner.fetch_bytes {
                Some(hook) => hook(target)?,
                None => http_get_bytes_with_timeouts(
                    target,
                    inner.timeout,
                    total_timeout,
                    MAX_IMAGE_BYTES,
                )?,
            };
            if bytes.is_empty() {
                return Err("empty image response".to_owned());
            }
            Ok(bytes)
        };
        if let Ok(bytes) = fetch_img(&resized, resized_timeout) {
            image_bytes = Some(bytes);
        } else if resized != img_url
            && let Ok(bytes) = fetch_img(&img_url, img_total_timeout)
        {
            image_bytes = Some(bytes);
        }
    }

    PinGuide {
        pin_id: pin_id.to_owned(),
        description: desc,
        image_url: img_url,
        image_bytes,
        ok: true,
        lang: effective_lang.to_owned(),
    }
}

fn insert_guide_into_cache(st: &mut PinGuideState, cache_key: (String, String), guide: PinGuide) {
    if let Some(pos) = st.cache.iter().position(|(k, _)| k == &cache_key)
        && let Some((_, old)) = st.cache.remove(pos)
    {
        st.cache_image_bytes = st
            .cache_image_bytes
            .saturating_sub(guide_image_bytes(&old));
    }
    st.cache_image_bytes = st
        .cache_image_bytes
        .saturating_add(guide_image_bytes(&guide));
    st.cache.push_back((cache_key, guide));
    while st.cache.len() > MAX_CACHE
        || (st.cache_image_bytes > MAX_CACHE_IMAGE_BYTES && st.cache.len() > 1)
    {
        if let Some((_, evicted)) = st.cache.pop_front() {
            st.cache_image_bytes = st
                .cache_image_bytes
                .saturating_sub(guide_image_bytes(&evicted));
        } else {
            break;
        }
    }
}

fn fetch_inline_image_inner(
    inner: &PinGuideInner,
    pin_id: &str,
    description: &str,
    image_url: &str,
    lang_tag: &str,
) -> PinGuide {
    let mut image_bytes = None;
    if is_allowed_provider_image_url(image_url) {
        let img_total_timeout = inner
            .timeout
            .saturating_mul(3)
            .min(Duration::from_secs(25))
            .max(inner.timeout);
        let bytes_res = match &inner.fetch_bytes {
            Some(hook) => hook(image_url),
            None => http_get_bytes_with_timeouts(
                image_url,
                inner.timeout,
                img_total_timeout,
                MAX_IMAGE_BYTES,
            ),
        };
        if let Ok(bytes) = bytes_res
            && !bytes.is_empty()
        {
            image_bytes = Some(bytes);
        }
    }
    PinGuide {
        pin_id: pin_id.to_owned(),
        description: description.to_owned(),
        image_url: image_url.to_owned(),
        image_bytes,
        ok: true,
        lang: lang_tag.to_owned(),
    }
}

fn worker_loop(inner: Arc<PinGuideInner>) {
    loop {
        let (lang_tag, pid, job_kind, cb) = {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            while st.pending.is_none() && !st.stopped {
                let (guard, _wait_res) = inner
                    .cv
                    .wait_timeout(st, WORKER_IDLE_TIMEOUT)
                    .unwrap_or_else(|e| e.into_inner());
                st = guard;
                if st.pending.is_none() && !st.stopped {
                    st.thread_alive = false;
                    return;
                }
            }
            if st.stopped {
                st.thread_alive = false;
                return;
            }
            let Some((lt, p, kind, callback)) = st.pending.take() else {
                continue;
            };
            st.inflight = Some((lt.clone(), p.clone()));
            (lt, p, kind, callback)
        };

        let guide = match job_kind {
            GuideJobKind::Hoyolab { map_id } => fetch_sync_inner(&inner, &pid, map_id, &lang_tag),
            GuideJobKind::Inline {
                description,
                image_url,
            } => fetch_inline_image_inner(&inner, &pid, &description, &image_url, &lang_tag),
        };
        let cache_key = (lang_tag, pid);

        let should_cache = guide.ok
            && (guide.image_url.is_empty()
                || guide.image_bytes.as_ref().is_some_and(|b| !b.is_empty()));

        {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.inflight = None;
            if should_cache {
                insert_guide_into_cache(&mut st, cache_key, guide.clone());
            }
        }

        cb(guide);
    }
}

pub type OnLabelsLoadedHook = Arc<dyn Fn(i32, &str) + Send + Sync>;

pub struct LabelCatalogConfig {
    pub api_base: String,
    pub fetch_json: Option<JsonFetchHook>,
    pub on_loaded: Option<OnLabelsLoadedHook>,
    pub timeout: Duration,
}

impl Default for LabelCatalogConfig {
    fn default() -> Self {
        Self {
            api_base: API_BASE.to_owned(),
            fetch_json: None,
            on_loaded: None,
            timeout: Duration::from_secs(8),
        }
    }
}

fn allowlisted_icon_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if is_allowed_image_url(trimmed) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

#[derive(Default)]
struct LabelCatalogState {
    labels: HashMap<(i32, String), BTreeMap<i32, String>>,
    icons: HashMap<i32, BTreeMap<i32, String>>,
    inflight: HashSet<(i32, String)>,
    retry_after: HashMap<(i32, String), Instant>,
}

struct LabelCatalogInner {
    api_base: String,
    fetch_json: Option<JsonFetchHook>,
    on_loaded: Option<OnLabelsLoadedHook>,
    timeout: Duration,
    state: Mutex<LabelCatalogState>,
}

#[derive(Clone)]
pub struct HoYoLABLabelCatalog {
    inner: Arc<LabelCatalogInner>,
}

impl std::fmt::Debug for HoYoLABLabelCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HoYoLABLabelCatalog")
            .field("api_base", &self.inner.api_base)
            .finish()
    }
}

impl Default for HoYoLABLabelCatalog {
    fn default() -> Self {
        Self::with_config(LabelCatalogConfig::default())
    }
}

impl HoYoLABLabelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(cfg: LabelCatalogConfig) -> Self {
        let trimmed = cfg.api_base.trim().trim_end_matches('/').to_owned();
        Self {
            inner: Arc::new(LabelCatalogInner {
                api_base: if trimmed.is_empty() {
                    API_BASE.to_owned()
                } else {
                    trimmed
                },
                fetch_json: cfg.fetch_json,
                on_loaded: cfg.on_loaded,
                timeout: cfg.timeout.max(Duration::from_millis(50)),
                state: Mutex::new(LabelCatalogState::default()),
            }),
        }
    }

    pub fn get_map_name(map_id: i32, lang: Option<&str>, fallback: &str) -> String {
        let short = short_lang(lang);
        let pair: Option<(&str, &str)> = match map_id {
            2 => Some(("Teyvat Interactive Map", "Bản Đồ Tương Tác Teyvat")),
            7 => Some(("Enkanomiya", "Enkanomiya")),
            9 => Some((
                "The Chasm: Underground Mines",
                "Vực Đá Sâu - Khu Mỏ Ngầm",
            )),
            34 => Some(("Sea of Bygone Eras", "Biển Kỷ Nguyên Cũ")),
            36 => Some(("Ancient Sacred Mountain", "Núi Thánh Cổ Đại")),
            37 => Some(("Temple of Space", "Đền Thờ Không Gian")),
            40 => Some(("Nod-Krai", "Nod-Krai")),
            _ => None,
        };

        if let Some((en, vi)) = pair {
            let localized = if short == "vi" { vi } else { en };
            if !localized.is_empty() {
                return localized.to_owned();
            }
            if !en.is_empty() {
                return en.to_owned();
            }
        }
        if !fallback.is_empty() {
            fallback.to_owned()
        } else {
            format!("Map {map_id}")
        }
    }

    pub fn fetch_sync(&self, map_id: i32, lang: Option<&str>) -> CoreResult<BTreeMap<i32, String>> {
        let short = short_lang(lang).to_owned();
        let lang_tag = hoyolab_lang_tag(Some(&short));
        let url = format!(
            "{}/v1/map/label/tree?map_id={map_id}&app_sn=ys_obc&lang={lang_tag}",
            self.inner.api_base
        );

        let env = match &self.inner.fetch_json {
            Some(hook) => hook(&url).map_err(CoreError::Sync)?,
            None => http_get_json(&url, self.inner.timeout, MAX_LABEL_TREE_JSON_BYTES)
                .map_err(CoreError::Sync)?,
        };

        if !is_retcode_ok(&env) {
            return Err(CoreError::Sync(format!(
                "HoYoLAB label/tree error retcode={:?}",
                env.get("retcode")
            )));
        }

        let mut parsed = BTreeMap::new();
        let mut parsed_icons = BTreeMap::new();
        if let Some(tree) = env
            .get("data")
            .and_then(|d| d.get("tree"))
            .and_then(Value::as_array)
        {
            walk_label_tree(tree, &mut parsed);
            walk_label_icons(tree, &mut parsed_icons);
        }

        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = (map_id, short);
        st.labels.insert(key.clone(), parsed.clone());
        let icon_entry = st.icons.entry(map_id).or_default();
        for (lid, icon_url) in parsed_icons {
            icon_entry.insert(lid, icon_url);
        }
        st.retry_after.remove(&key);
        Ok(parsed)
    }

    pub fn prefetch(&self, map_id: i32, lang: Option<&str>) {
        let short = short_lang(lang).to_owned();
        let key = (map_id, short.clone());
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.labels.contains_key(&key) || st.inflight.contains(&key) {
                return;
            }
            if let Some(until) = st.retry_after.get(&key)
                && Instant::now() < *until
            {
                return;
            }
            st.inflight.insert(key.clone());
        }

        let catalog = self.clone();
        let _ = std::thread::Builder::new()
            .name(format!("GH_Labels_{map_id}_{short}"))
            .spawn(move || {
                let ok = match catalog.fetch_sync(map_id, Some(&short)) {
                    Ok(_) => true,
                    Err(_) => {
                        let mut st = catalog.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                        st.retry_after.insert(
                            key.clone(),
                            Instant::now() + Duration::from_secs_f64(LABEL_RETRY_BACKOFF_SECS),
                        );
                        false
                    }
                };
                {
                    let mut st = catalog.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.inflight.remove(&key);
                }
                if ok && let Some(cb) = &catalog.inner.on_loaded {
                    cb(map_id, &short);
                }
            });
    }

    pub fn get_label(
        &self,
        map_id: i32,
        label_id: Option<i32>,
        lang: Option<&str>,
    ) -> Option<String> {
        let lid = label_id?;
        let short = short_lang(lang);
        let cached = {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.labels
                .get(&(map_id, short.to_owned()))
                .and_then(|m| m.get(&lid).cloned())
        };
        if let Some(name) = cached {
            return Some(name);
        }
        let is_loaded = {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.labels.contains_key(&(map_id, short.to_owned()))
        };
        if !is_loaded {
            self.prefetch(map_id, Some(short));
        }
        None
    }

    pub fn has_icons_for_map(&self, map_id: i32) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .icons
            .contains_key(&map_id)
    }

    pub fn get_icon_url(&self, map_id: i32, label_id: i32) -> Option<String> {
        let cached = {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.icons
                .get(&map_id)
                .and_then(|m| m.get(&label_id).cloned())
        };
        if let Some(url) = cached {
            return Some(url);
        }
        let is_loaded = {
            let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.icons.contains_key(&map_id)
        };
        if !is_loaded {
            self.prefetch(map_id, None);
        }
        None
    }
}

fn walk_label_tree(nodes: &[Value], out: &mut BTreeMap<i32, String>) {
    for item in nodes {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let raw_id = obj.get("id").and_then(|v| match v {
            Value::Number(n) => n.as_i64().and_then(|i| i32::try_from(i).ok()),
            Value::String(s) => s.trim().parse::<i32>().ok(),
            _ => None,
        });
        let raw_name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if let Some(lid) = raw_id
            && !raw_name.is_empty()
        {
            out.insert(lid, raw_name.to_owned());
        }
        if let Some(children) = obj.get("children").and_then(Value::as_array)
            && !children.is_empty()
        {
            walk_label_tree(children, out);
        }
    }
}

fn walk_label_icons(nodes: &[Value], out: &mut BTreeMap<i32, String>) {
    for item in nodes {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let raw_id = obj.get("id").and_then(|v| match v {
            Value::Number(n) => n.as_i64().and_then(|i| i32::try_from(i).ok()),
            Value::String(s) => s.trim().parse::<i32>().ok(),
            _ => None,
        });
        if let Some(lid) = raw_id
            && let Some(raw_icon) = obj.get("icon").and_then(Value::as_str)
            && let Some(safe_url) = allowlisted_icon_url(raw_icon)
        {
            out.insert(lid, safe_url);
        }
        if let Some(children) = obj.get("children").and_then(Value::as_array)
            && !children.is_empty()
        {
            walk_label_icons(children, out);
        }
    }
}

fn is_retcode_ok(env: &Value) -> bool {
    match env.get("retcode") {
        Some(Value::Number(n)) => n.as_i64() == Some(0),
        Some(Value::String(s)) => s.trim() == "0",
        _ => false,
    }
}

const MAX_PIN_ICON_BYTES: usize = 512 * 1024;

/// Official HoYoLAB Interactive Map marker UI sprite assets (fetched lazily at runtime from
/// HoYoLAB/HoYoVerse official static CDN into `%LOCALAPPDATA%\GenshinHelper\cache\lan_icons\`
/// — never bundled inside the repository or installer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkerUiAsset {
    BgNormal,
    BgSelected,
    SelectedFrame,
    UndergroundMark,
    UndergroundMarkSelected,
    WaterholeMark,
    WaterholeMarkSelected,
    ZLevelUpper,
    ZLevelMiddle,
    ZLevelLower,
}

impl MarkerUiAsset {
    pub const ALL: &'static [MarkerUiAsset] = &[
        Self::BgNormal,
        Self::BgSelected,
        Self::SelectedFrame,
        Self::UndergroundMark,
        Self::UndergroundMarkSelected,
        Self::WaterholeMark,
        Self::WaterholeMarkSelected,
        Self::ZLevelUpper,
        Self::ZLevelMiddle,
        Self::ZLevelLower,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::BgNormal => "marker_bg_normal",
            Self::BgSelected => "marker_bg_selected",
            Self::SelectedFrame => "marker_selected_frame",
            Self::UndergroundMark => "marker_ug_mark",
            Self::UndergroundMarkSelected => "marker_ug_mark_selected",
            Self::WaterholeMark => "marker_wh_mark",
            Self::WaterholeMarkSelected => "marker_wh_mark_selected",
            Self::ZLevelUpper => "marker_z_upper",
            Self::ZLevelMiddle => "marker_z_middle",
            Self::ZLevelLower => "marker_z_lower",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim() {
            "marker_bg_normal" => Some(Self::BgNormal),
            "marker_bg_selected" => Some(Self::BgSelected),
            "marker_selected_frame" => Some(Self::SelectedFrame),
            "marker_ug_mark" => Some(Self::UndergroundMark),
            "marker_ug_mark_selected" => Some(Self::UndergroundMarkSelected),
            "marker_wh_mark" => Some(Self::WaterholeMark),
            "marker_wh_mark_selected" => Some(Self::WaterholeMarkSelected),
            "marker_z_upper" => Some(Self::ZLevelUpper),
            "marker_z_middle" => Some(Self::ZLevelMiddle),
            "marker_z_lower" => Some(Self::ZLevelLower),
            _ => None,
        }
    }

    pub const fn url(self) -> &'static str {
        match self {
            Self::BgNormal => {
                "https://webstatic.hoyoverse.com/upload/static-resource/2022/01/27/bb4d956a2e14e03a9e7358b5e1f07691_6147235523146565799.png"
            }
            Self::BgSelected => {
                "https://webstatic.hoyoverse.com/upload/static-resource/2022/01/27/6cb7603c4fc3a043efe759713afbe456_543711165073740768.png"
            }
            Self::SelectedFrame => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/25/2fd02937dbad1352d1968a27e9d23e06_1503232752462054040.png"
            }
            Self::UndergroundMark => {
                "https://webstatic.hoyoverse.com/upload/static-resource/2022/12/29/7231b674fa743b10683c47aa029fcca2_794741109522805089.png"
            }
            Self::UndergroundMarkSelected => {
                "https://webstatic.hoyoverse.com/upload/static-resource/2022/12/29/983a7e71a36b724a4695a7d6049a2f6d_9173445168215857636.png"
            }
            Self::WaterholeMark => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/24/58ee86376c7a82c6dab2bf13156e49ed_4378193350715894241.png"
            }
            Self::WaterholeMarkSelected => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/24/2e0d1bbd60825dccd46a7d4d59bbc2e4_7268773892301601185.png"
            }
            Self::ZLevelUpper => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/25/702c6564136a8c156238f16df7e5cdb8_8183566117316619790.png"
            }
            Self::ZLevelMiddle => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/25/4b9e60d62f9aab681d468e1f8ed06288_6257723206515617428.png"
            }
            Self::ZLevelLower => {
                "https://fastcdn.hoyoverse.com/static-resource-v2/2023/07/25/407dc5416bbd774922d2cf378d13d9ae_9179121067510537665.png"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IconWorkerJob {
    Label(i32, i32),
    UiAsset(MarkerUiAsset),
}

#[derive(Default)]
struct PinIconState {
    closed: bool,
    inflight: HashSet<(i32, i32)>,
    missing: HashSet<(i32, i32)>,
    ui_inflight: HashSet<MarkerUiAsset>,
}

struct PinIconInner {
    disk_dir: PathBuf,
    catalog: Arc<HoYoLABLabelCatalog>,
    timeout: Duration,
    generation: AtomicU64,
    state: Mutex<PinIconState>,
    worker_tx: Mutex<Option<mpsc::Sender<IconWorkerJob>>>,
}

/// Lazy, zero-overhead-when-unused disk cache for HoYoLAB category pin icons
/// (`{disk_dir}/{map_id}_{label_id}.img`), shared by Native Live Radar Map & LAN Companion.
///
/// Spawns zero threads until [`Self::request_icon_async`] is invoked, and only downloads URLs
/// resolved from [`HoYoLABLabelCatalog`] that pass the HoYoLAB/HoYoVerse SSRF allowlist.
#[derive(Clone)]
pub struct PinIconCache {
    inner: Arc<PinIconInner>,
}

impl std::fmt::Debug for PinIconCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinIconCache")
            .field("disk_dir", &self.inner.disk_dir)
            .field("generation", &self.generation())
            .finish()
    }
}

impl PinIconCache {
    pub fn new(disk_dir: PathBuf, catalog: Arc<HoYoLABLabelCatalog>, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(PinIconInner {
                disk_dir,
                catalog,
                timeout: timeout.max(Duration::from_millis(50)),
                generation: AtomicU64::new(0),
                state: Mutex::new(PinIconState::default()),
                worker_tx: Mutex::new(None),
            }),
        }
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(AtomicOrdering::Acquire)
    }

    pub fn disk_dir(&self) -> &Path {
        &self.inner.disk_dir
    }

    pub fn icon_path(&self, map_id: i32, label_id: i32) -> PathBuf {
        self.inner
            .disk_dir
            .join(format!("{map_id}_{label_id}.img"))
    }

    pub fn ui_asset_path(&self, asset: MarkerUiAsset) -> PathBuf {
        self.inner
            .disk_dir
            .join(format!("ui_{}.img", asset.key()))
    }

    /// Reads cached icon bytes from disk (`>= 8` bytes, valid image header). Never blocks on network.
    pub fn get_cached_bytes(&self, map_id: i32, label_id: i32) -> Option<Vec<u8>> {
        if label_id <= 0 {
            return None;
        }
        let path = self.icon_path(map_id, label_id);
        let bytes = std::fs::read(&path).ok()?;
        if is_valid_icon_bytes(&bytes) {
            Some(bytes)
        } else {
            None
        }
    }

    /// Reads cached official HoYoLAB marker UI sprite bytes from disk. Never blocks on network.
    pub fn get_ui_asset_bytes(&self, asset: MarkerUiAsset) -> Option<Vec<u8>> {
        let path = self.ui_asset_path(asset);
        let bytes = std::fs::read(&path).ok()?;
        if is_valid_icon_bytes(&bytes) {
            Some(bytes)
        } else {
            None
        }
    }

    /// Returns cached icon bytes or synchronously fetches, validates, and persists them into the
    /// single shared disk cache (`{disk_dir}/{map_id}_{label_id}.img`), bumping `generation` so
    /// Native Live Radar and LAN Companion share one unified asset pool.
    pub fn get_or_fetch_icon_bytes_sync(&self, map_id: i32, label_id: i32) -> Option<Vec<u8>> {
        if label_id <= 0 {
            return None;
        }
        if let Some(cached) = self.get_cached_bytes(map_id, label_id) {
            return Some(cached);
        }
        self.inner.fetch_and_save_one_sync(map_id, label_id)
    }

    /// Returns cached HoYoLAB marker UI sprite bytes or synchronously fetches, validates, and
    /// persists them into the shared cache (`ui_{key}.img`), bumping `generation`.
    pub fn get_or_fetch_ui_asset_bytes_sync(&self, asset: MarkerUiAsset) -> Option<Vec<u8>> {
        if let Some(cached) = self.get_ui_asset_bytes(asset) {
            return Some(cached);
        }
        self.inner.fetch_and_save_ui_asset_sync(asset)
    }

    fn ensure_worker_and_send(&self, job: IconWorkerJob) -> bool {
        let mut tx_guard = self.inner.worker_tx.lock().unwrap_or_else(|e| e.into_inner());
        if tx_guard.is_none() {
            let (tx, rx) = mpsc::channel::<IconWorkerJob>();
            let inner = Arc::clone(&self.inner);
            let spawned = std::thread::Builder::new()
                .name("gh-pin-icon-worker".to_owned())
                .spawn(move || {
                    for req in rx {
                        let is_closed = {
                            let st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                            st.closed
                        };
                        match req {
                            IconWorkerJob::Label(mid, lid) => {
                                if is_closed {
                                    let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                    st.inflight.remove(&(mid, lid));
                                    continue;
                                }
                                inner.fetch_and_save_one(mid, lid);
                            }
                            IconWorkerJob::UiAsset(asset) => {
                                if is_closed {
                                    let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                                    st.ui_inflight.remove(&asset);
                                    continue;
                                }
                                inner.fetch_and_save_ui_asset(asset);
                            }
                        }
                    }
                });
            if spawned.is_ok() {
                *tx_guard = Some(tx);
            } else {
                return false;
            }
        }
        tx_guard
            .as_ref()
            .is_some_and(|tx| tx.send(job).is_ok())
    }

    /// Enqueues an async background download for `(map_id, label_id)` if not cached/inflight/missing.
    pub fn request_icon_async(&self, map_id: i32, label_id: i32) {
        if label_id <= 0 {
            return;
        }
        if self.icon_path(map_id, label_id).is_file() {
            return;
        }
        let key = (map_id, label_id);
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed || st.inflight.contains(&key) || st.missing.contains(&key) {
                return;
            }
            st.inflight.insert(key);
        }

        if !self.ensure_worker_and_send(IconWorkerJob::Label(map_id, label_id)) {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.inflight.remove(&key);
        }
    }

    /// Enqueues an async background download for an official HoYoLAB marker UI sprite.
    pub fn request_ui_asset_async(&self, asset: MarkerUiAsset) {
        if self.ui_asset_path(asset).is_file() {
            return;
        }
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed || st.ui_inflight.contains(&asset) {
                return;
            }
            st.ui_inflight.insert(asset);
        }

        if !self.ensure_worker_and_send(IconWorkerJob::UiAsset(asset)) {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.ui_inflight.remove(&asset);
        }
    }

    /// Stops the lazy icon worker thread and clears transient state (zero overhead when Radar is OFF).
    pub fn stop(&self) {
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            st.inflight.clear();
            st.ui_inflight.clear();
        }
        let mut tx_guard = self.inner.worker_tx.lock().unwrap_or_else(|e| e.into_inner());
        *tx_guard = None;
    }

    /// Re-arms the icon cache after a previous `stop()` (still spawns zero threads until needed).
    pub fn reopen(&self) {
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = false;
    }

    #[cfg(test)]
    fn has_active_worker(&self) -> bool {
        self.inner
            .worker_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

impl PinIconInner {
    fn fetch_and_save_one_sync(&self, map_id: i32, label_id: i32) -> Option<Vec<u8>> {
        // Ensure label/tree is hydrated synchronously before deciding an icon is missing;
        // otherwise cold startup races permanently poison `missing`.
        let mut url_opt = self.catalog.get_icon_url(map_id, label_id);
        if url_opt.is_none() && !self.catalog.has_icons_for_map(map_id) {
            let _ = self.catalog.fetch_sync(map_id, None);
            url_opt = self.catalog.get_icon_url(map_id, label_id);
        }
        let Some(url) = url_opt else {
            // Only mark permanently missing if the map's catalog actually loaded.
            if self.catalog.has_icons_for_map(map_id) {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.missing.insert((map_id, label_id));
            }
            return None;
        };
        if !is_allowed_image_url(&url) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.missing.insert((map_id, label_id));
            return None;
        }
        let Ok(bytes) = http_get_bytes(&url, self.timeout, MAX_PIN_ICON_BYTES) else {
            return None;
        };
        if !is_valid_icon_bytes(&bytes) {
            return None;
        }
        let _ = std::fs::create_dir_all(&self.disk_dir);
        let path = self.disk_dir.join(format!("{map_id}_{label_id}.img"));
        if atomic_io::write_bytes_atomic(&path, &bytes).is_ok() {
            self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        }
        Some(bytes)
    }

    fn fetch_and_save_one(&self, map_id: i32, label_id: i32) {
        struct Guard<'a> {
            state: &'a Mutex<PinIconState>,
            key: (i32, i32),
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.inflight.remove(&self.key);
            }
        }
        let _guard = Guard {
            state: &self.state,
            key: (map_id, label_id),
        };
        let _ = self.fetch_and_save_one_sync(map_id, label_id);
    }

    fn fetch_and_save_ui_asset_sync(&self, asset: MarkerUiAsset) -> Option<Vec<u8>> {
        let url = asset.url();
        if !is_allowed_image_url(url) {
            return None;
        }
        let Ok(bytes) = http_get_bytes(url, self.timeout, MAX_PIN_ICON_BYTES) else {
            return None;
        };
        if !is_valid_icon_bytes(&bytes) {
            return None;
        }
        let _ = std::fs::create_dir_all(&self.disk_dir);
        let path = self.disk_dir.join(format!("ui_{}.img", asset.key()));
        if atomic_io::write_bytes_atomic(&path, &bytes).is_ok() {
            self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        }
        Some(bytes)
    }

    fn fetch_and_save_ui_asset(&self, asset: MarkerUiAsset) {
        struct UiGuard<'a> {
            state: &'a Mutex<PinIconState>,
            asset: MarkerUiAsset,
        }
        impl Drop for UiGuard<'_> {
            fn drop(&mut self) {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.ui_inflight.remove(&self.asset);
            }
        }
        let _guard = UiGuard {
            state: &self.state,
            asset,
        };
        let _ = self.fetch_and_save_ui_asset_sync(asset);
    }
}

fn is_valid_icon_bytes(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || bytes.len() > MAX_PIN_ICON_BYTES {
        return false;
    }
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF8")
}

fn url_encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn http_get_bytes(url: &str, timeout: Duration, limit: usize) -> Result<Vec<u8>, String> {
    http_get_bytes_with_timeouts(url, timeout, timeout, limit)
}

fn http_get_bytes_with_timeouts(
    url: &str,
    connect_timeout: Duration,
    total_timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let url_owned = url.to_owned();
    let res = run_blocking_http(move || {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, image/*"));
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(total_timeout)
            .build()
            .ok()?;
        let resp = client.get(&url_owned).headers(headers).send().ok()?;
        if !resp.status().is_success() {
            return Some(Err(format!("HTTP {}", resp.status())));
        }
        let mut buf = Vec::new();
        let mut limited = resp.take((limit + 1) as u64);
        if let Err(e) = limited.read_to_end(&mut buf) {
            return Some(Err(format!("read failed: {e}")));
        }
        if buf.len() > limit {
            return Some(Err("response too large".to_owned()));
        }
        Some(Ok(buf))
    });
    match res {
        Some(Ok(bytes)) => Ok(bytes),
        Some(Err(e)) => Err(e),
        None => Err("HTTP request failed".to_owned()),
    }
}

fn http_get_json(url: &str, timeout: Duration, limit: usize) -> Result<Value, String> {
    let bytes = http_get_bytes(url, timeout, limit)?;
    serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("invalid JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::Query;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[test]
    fn clean_guide_text_and_resized_image_url_match_python() {
        assert_eq!(
            clean_guide_text("Đi <b>qua</b> cầu<br/>rẽ trái&nbsp;sau đá"),
            "Đi qua cầu\nrẽ trái sau đá"
        );
        assert_eq!(
            clean_guide_text("<p>Sau thác nước</p><p>Nhặt rương&amp;mở khóa&#33;</p>"),
            "Sau thác nước\nNhặt rương&mở khóa!"
        );
        assert_eq!(
            clean_guide_text("  &#x41; &lt;tag&gt; &quot;ok&quot; &apos;hi&apos;\r\n\r\n  line2  "),
            "A <tag> \"ok\" 'hi'\nline2"
        );
        assert_eq!(clean_guide_text(""), "");

        assert_eq!(
            resized_image_url("https://upload-os-bbs.hoyolab.com/a.png"),
            "https://upload-os-bbs.hoyolab.com/a.png?x-oss-process=image/resize,w_960"
        );
        assert_eq!(
            resized_image_url("https://act-webstatic.hoyoverse.com/a.png?v=1"),
            "https://act-webstatic.hoyoverse.com/a.png?v=1&x-oss-process=image/resize,w_960"
        );
        assert_eq!(
            resized_image_url("https://cdn.mihoyo.com/a.png?x-oss-process=image/resize,w_480"),
            "https://cdn.mihoyo.com/a.png?x-oss-process=image/resize,w_480"
        );
        assert_eq!(
            resized_image_url("https://example.com/a.png"),
            "https://example.com/a.png"
        );
    }

    #[test]
    fn verify_nav_ux_v54_guide_fetcher_contract() {
        let seen_urls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_urls_json = Arc::clone(&seen_urls);
        let seen_img_urls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_img_urls_hook = Arc::clone(&seen_img_urls);

        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            lang_getter: Arc::new(|| "vi".to_owned()),
            fetch_json: Some(Arc::new(move |url| {
                seen_urls_json.lock().unwrap().push(url.to_owned());
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "content": "<p>Sau thác nước</p>",
                            "img": "https://upload-os-bbs.hoyoverse.com/guide.png"
                        }
                    }
                }))
            })),
            fetch_bytes: Some(Arc::new(move |url| {
                seen_img_urls_hook.lock().unwrap().push(url.to_owned());
                Ok(b"PNG_DATA".to_vec())
            })),
            ..Default::default()
        });

        // Rejects non-digit IDs
        assert!(!PinGuideFetcher::is_fetchable_id("pin_1.0_2.0"));
        assert!(!fetcher.request("pin_1.0_2.0", 2, |_| {}, None));

        // Async request + wait
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(fetcher.request(
            "107596",
            2,
            move |g| {
                let _ = tx.send(g);
            },
            None
        ));
        let guide = rx.recv_timeout(Duration::from_secs(3)).expect("guide");
        assert!(guide.ok);
        assert_eq!(guide.pin_id, "107596");
        assert_eq!(guide.description, "Sau thác nước");
        assert_eq!(guide.image_bytes.as_deref(), Some(b"PNG_DATA".as_slice()));
        assert_eq!(guide.lang, "vi-vn");

        let api_url = &seen_urls.lock().unwrap()[0];
        assert!(api_url.contains("point_id=107596"));
        assert!(api_url.contains("lang=vi-vn"));
        assert!(seen_img_urls.lock().unwrap()[0].contains("x-oss-process=image/resize,w_960"));

        // Second request for same (lang_tag, pin_id) hits cache -> returns false
        assert!(!fetcher.request("107596", 2, |_| {}, None));
        assert!(fetcher.get_cached("107596", None).is_some());
    }

    #[test]
    fn bilingual_cache_isolation_and_label_catalog() {
        let active_lang = Arc::new(Mutex::new("vi".to_owned()));
        let active_lang_getter = Arc::clone(&active_lang);

        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            lang_getter: Arc::new(move || active_lang_getter.lock().unwrap().clone()),
            fetch_json: Some(Arc::new(|url| {
                let is_en = url.contains("lang=en-us");
                let text = if is_en {
                    "English community tip"
                } else {
                    "Gợi ý tiếng Việt"
                };
                Ok(json!({
                    "retcode": 0,
                    "data": { "info": { "content": text, "img": "" } }
                }))
            })),
            fetch_bytes: Some(Arc::new(|_| Ok(Vec::new()))),
            ..Default::default()
        });

        let (tx1, rx1) = std::sync::mpsc::channel();
        assert!(fetcher.request(
            "12345",
            2,
            move |g| {
                let _ = tx1.send(g);
            },
            None
        ));
        let g_vi = rx1.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(g_vi.description, "Gợi ý tiếng Việt");
        assert_eq!(g_vi.lang, "vi-vn");

        // Switch active language to "en" -> cached VI entry must NOT satisfy EN lookup
        *active_lang.lock().unwrap() = "en".to_owned();
        assert!(fetcher.get_cached("12345", None).is_none());

        let (tx2, rx2) = std::sync::mpsc::channel();
        assert!(fetcher.request(
            "12345",
            2,
            move |g| {
                let _ = tx2.send(g);
            },
            None
        ));
        let g_en = rx2.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(g_en.description, "English community tip");
        assert_eq!(g_en.lang, "en-us");

        // Both remain independently cached
        assert_eq!(
            fetcher.get_cached("12345", Some("vi")).unwrap().description,
            "Gợi ý tiếng Việt"
        );
        assert_eq!(
            fetcher.get_cached("12345", Some("en")).unwrap().description,
            "English community tip"
        );

        // HoYoLABLabelCatalog bilingual lookup + map names
        let loaded_events = Arc::new(Mutex::new(Vec::<(i32, String)>::new()));
        let loaded_hook = Arc::clone(&loaded_events);
        let catalog = HoYoLABLabelCatalog::with_config(LabelCatalogConfig {
            fetch_json: Some(Arc::new(|url| {
                let is_vi = url.contains("lang=vi-vn");
                let name18 = if is_vi {
                    "Khiêu Chiến Hạn Giờ"
                } else {
                    "Time Trial Challenge"
                };
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "tree": [{
                            "id": 1,
                            "name": "Root",
                            "children": [{ "id": 18, "name": name18 }]
                        }]
                    }
                }))
            })),
            on_loaded: Some(Arc::new(move |mid, short| {
                loaded_hook.lock().unwrap().push((mid, short.to_owned()));
            })),
            ..Default::default()
        });

        catalog.fetch_sync(2, Some("vi")).unwrap();
        catalog.fetch_sync(2, Some("en")).unwrap();
        assert_eq!(
            catalog.get_label(2, Some(18), Some("vi")).as_deref(),
            Some("Khiêu Chiến Hạn Giờ")
        );
        assert_eq!(
            catalog.get_label(2, Some(18), Some("en")).as_deref(),
            Some("Time Trial Challenge")
        );
        assert_eq!(
            HoYoLABLabelCatalog::get_map_name(2, Some("en"), ""),
            "Teyvat Interactive Map"
        );
        assert_eq!(
            HoYoLABLabelCatalog::get_map_name(2, Some("vi"), ""),
            "Bản Đồ Tương Tác Teyvat"
        );
        assert_eq!(
            HoYoLABLabelCatalog::get_map_name(999, Some("en"), "Fallback"),
            "Fallback"
        );
        assert_eq!(
            HoYoLABLabelCatalog::get_map_name(999, Some("en"), ""),
            "Map 999"
        );

        // Async prefetch triggers on_loaded callback
        catalog.prefetch(7, Some("en"));
        for _ in 0..40 {
            if !loaded_events.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            loaded_events
                .lock()
                .unwrap()
                .contains(&(7, "en".to_owned()))
        );
    }

    #[test]
    fn lru_eviction_at_max_cache_64() {
        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_json: Some(Arc::new(|url| {
                Ok(json!({
                    "retcode": 0,
                    "data": { "info": { "content": format!("tip:{url}"), "img": "" } }
                }))
            })),
            ..Default::default()
        });

        for i in 1..=65u32 {
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(fetcher.request(
                &i.to_string(),
                2,
                move |g| {
                    let _ = tx.send(g);
                },
                Some("vi")
            ));
            let g = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(g.ok);
        }

        // Pin "1" was oldest and should be evicted, while "2".."65" remain cached
        assert!(fetcher.get_cached("1", Some("vi")).is_none());
        assert!(fetcher.get_cached("2", Some("vi")).is_some());
        assert!(fetcher.get_cached("65", Some("vi")).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_http_pin_guide_and_label_catalog_inside_tokio() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_srv = Arc::clone(&calls);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = Router::new()
            .route(
                "/v1/map/point/info",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let c = Arc::clone(&calls_srv);
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        let pid = q.get("point_id").cloned().unwrap_or_default();
                        axum::Json(json!({
                            "retcode": 0,
                            "data": {
                                "info": {
                                    "content": format!("Pin <b>{pid}</b><br/>ready"),
                                    "url_list": [{ "url": format!("http://{addr}/img.png") }]
                                }
                            }
                        }))
                    }
                }),
            )
            .route(
                "/v1/map/label/tree",
                get(|| async {
                    axum::Json(json!({
                        "retcode": "0",
                        "data": { "tree": [{ "id": 1, "name": "Cat", "children": [{ "id": 2, "name": "Teleport" }] }] }
                    }))
                }),
            )
            .route(
                "/img.png",
                get(|| async { ([(axum::http::header::CONTENT_TYPE, "image/png")], b"FAKEPNG".to_vec()).into_response() }),
            );

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        let base = format!("http://{addr}");
        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            api_base: base.clone(),
            ..Default::default()
        });
        let guide = fetcher.fetch_sync("42", 2, Some("en"));
        assert!(guide.ok);
        assert_eq!(guide.description, "Pin 42\nready");
        assert_eq!(guide.image_bytes.as_deref(), Some(b"FAKEPNG".as_slice()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let catalog = HoYoLABLabelCatalog::with_config(LabelCatalogConfig {
            api_base: base,
            ..Default::default()
        });
        let labels = catalog.fetch_sync(2, Some("en")).unwrap();
        assert_eq!(labels.get(&2).map(String::as_str), Some("Teleport"));

        let _ = tx.send(());
    }

    #[test]
    fn label_catalog_extracts_allowlisted_icons_and_rejects_untrusted_hosts() {
        let catalog = HoYoLABLabelCatalog::with_config(LabelCatalogConfig {
            fetch_json: Some(Arc::new(|_| {
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "tree": [{
                            "id": 1,
                            "name": "Teleports",
                            "icon": "https://act-webstatic.hoyoverse.com/map_manage/tp.png",
                            "children": [
                                {
                                    "id": 2,
                                    "name": "Statue",
                                    "icon": "https://upload-os-bbs.hoyolab.com/statue.webp?v=2"
                                },
                                {
                                    "id": 6,
                                    "name": "Anemoculus",
                                    "icon": "https://act.hoyoverse.com/map_manage/anemo.png"
                                },
                                {
                                    "id": 7,
                                    "name": "LegacyStatic",
                                    "icon": "https://upload-static.hoyoverse.com/map_manage/conch.png"
                                },
                                {
                                    "id": 8,
                                    "name": "UploadStaticNoDash",
                                    "icon": "https://uploadstatic.hoyoverse.com/map_manage/chest.png"
                                },
                                {
                                    "id": 3,
                                    "name": "UntrustedHttp",
                                    "icon": "http://act-webstatic.hoyoverse.com/bad.png"
                                },
                                {
                                    "id": 4,
                                    "name": "UntrustedHost",
                                    "icon": "https://evil.example.com/pwn.png"
                                },
                                {
                                    "id": 5,
                                    "name": "UserInfoBypass",
                                    "icon": "https://act.hoyolab.com@evil.example.com/pwn.png"
                                }
                            ]
                        }]
                    }
                }))
            })),
            ..Default::default()
        });

        catalog.fetch_sync(2, Some("vi")).unwrap();
        assert_eq!(
            catalog.get_icon_url(2, 1).as_deref(),
            Some("https://act-webstatic.hoyoverse.com/map_manage/tp.png")
        );
        assert_eq!(
            catalog.get_icon_url(2, 2).as_deref(),
            Some("https://upload-os-bbs.hoyolab.com/statue.webp?v=2")
        );
        assert_eq!(
            catalog.get_icon_url(2, 6).as_deref(),
            Some("https://act.hoyoverse.com/map_manage/anemo.png")
        );
        assert_eq!(
            catalog.get_icon_url(2, 7).as_deref(),
            Some("https://upload-static.hoyoverse.com/map_manage/conch.png")
        );
        assert_eq!(
            catalog.get_icon_url(2, 8).as_deref(),
            Some("https://uploadstatic.hoyoverse.com/map_manage/chest.png")
        );
        assert!(catalog.get_icon_url(2, 3).is_none());
        assert!(catalog.get_icon_url(2, 4).is_none());
        assert!(catalog.get_icon_url(2, 5).is_none());
    }

    #[test]
    fn gif_fallback_on_empty_or_failed_resize_and_retry_when_image_fails() {
        assert_eq!(MAX_IMAGE_BYTES, 16 * 1024 * 1024);
        assert!(url_path_is_gif("https://upload-os-bbs.hoyolab.com/clip.GIF?v=1#top"));
        assert!(!url_path_is_gif("https://example.gif/clip.png"));
        assert!(!url_path_is_gif("https://example.gif"));

        // 1. Resized OSS URL returning empty body or error falls back to raw GIF URL.
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_hook = Arc::clone(&calls);
        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_json: Some(Arc::new(|_| {
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "content": "Animated guide",
                            "img": "https://upload-os-bbs.hoyoverse.com/demo.gif"
                        }
                    }
                }))
            })),
            fetch_bytes: Some(Arc::new(move |url| {
                calls_hook.lock().unwrap().push(url.to_owned());
                if url.contains("x-oss-process") {
                    Ok(Vec::new()) // empty body must fall back to original URL
                } else {
                    Ok(b"GIF89a_FALLBACK".to_vec())
                }
            })),
            ..Default::default()
        });

        let g = fetcher.fetch_sync("777", 2, Some("en"));
        assert!(g.ok);
        assert_eq!(g.image_bytes.as_deref(), Some(b"GIF89a_FALLBACK".as_slice()));
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].contains("x-oss-process"));
        assert_eq!(recorded[1], "https://upload-os-bbs.hoyoverse.com/demo.gif");

        // 2. Failed image download with non-empty image_url must NOT be cached so request() retries.
        let attempt = Arc::new(AtomicUsize::new(0));
        let attempt_hook = Arc::clone(&attempt);
        let retry_fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_json: Some(Arc::new(|_| {
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "content": "",
                            "img": "https://upload-os-bbs.hoyoverse.com/retry.gif"
                        }
                    }
                }))
            })),
            fetch_bytes: Some(Arc::new(move |_| {
                let n = attempt_hook.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err("HTTP 504".to_owned())
                } else {
                    Ok(b"GIF89a_OK".to_vec())
                }
            })),
            ..Default::default()
        });

        let (tx1, rx1) = std::sync::mpsc::channel();
        assert!(retry_fetcher.request("888", 2, move |g| { let _ = tx1.send(g); }, Some("vi")));
        let first = rx1.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(first.ok);
        assert!(first.image_bytes.is_none());
        assert!(retry_fetcher.get_cached("888", Some("vi")).is_none());

        let (tx2, rx2) = std::sync::mpsc::channel();
        assert!(retry_fetcher.request("888", 2, move |g| { let _ = tx2.send(g); }, Some("vi")));
        let second = rx2.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(second.ok);
        assert_eq!(second.image_bytes.as_deref(), Some(b"GIF89a_OK".as_slice()));
        assert!(retry_fetcher.get_cached("888", Some("vi")).is_some());
    }

    #[test]
    fn byte_cap_evicts_oldest_large_guide_images() {
        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_json: Some(Arc::new(|_| {
                Ok(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "content": "big guide",
                            "img": "https://upload-os-bbs.hoyoverse.com/big.png"
                        }
                    }
                }))
            })),
            fetch_bytes: Some(Arc::new(|_| Ok(vec![0xAB; 6 * 1024 * 1024]))),
            ..Default::default()
        });

        for pid in ["101", "102", "103"] {
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(fetcher.request(pid, 2, move |g| { let _ = tx.send(g); }, Some("vi")));
            let g = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(g.ok);
        }

        // 3 x 6 MiB = 18 MiB > MAX_CACHE_IMAGE_BYTES (16 MiB), so "101" must be evicted.
        assert!(fetcher.get_cached("101", Some("vi")).is_none());
        assert!(fetcher.get_cached("102", Some("vi")).is_some());
        assert!(fetcher.get_cached("103", Some("vi")).is_some());
        let st = fetcher.inner.state.lock().unwrap();
        assert!(st.cache_image_bytes <= MAX_CACHE_IMAGE_BYTES);
    }

    struct MockHttpServer {
        addr: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockHttpServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(std::net::SocketAddr, &str) -> (u16, &'static str, Vec<u8>) + Send + 'static,
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let addr = listener.local_addr().expect("local_addr");
            let stop = Arc::new(AtomicBool::new(false));
            let stop_srv = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop_srv.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut req_buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        req_buf.extend_from_slice(&chunk[..n]);
                        if req_buf.windows(4).any(|w| w == b"\r\n\r\n") || req_buf.len() >= 16384 {
                            break;
                        }
                    }
                    if req_buf.is_empty() {
                        continue;
                    }
                    let req_text = String::from_utf8_lossy(&req_buf);
                    let target = req_text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, content_type, body) = handler(addr, target);
                    let status_text = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            });
            Self {
                addr,
                stop,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.addr);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn json_response(body: Value) -> (u16, &'static str, Vec<u8>) {
        (200, "application/json", body.to_string().into_bytes())
    }

    #[test]
    fn is_allowed_image_url_enforces_hoyolab_hosts_and_blocks_ssrf() {
        assert!(is_allowed_image_url(
            "https://upload-os-bbs.hoyolab.com/a.png"
        ));
        assert!(is_allowed_image_url(
            "https://upload-os-bbs.hoyoverse.com/b.webp?v=1"
        ));
        assert!(is_allowed_image_url(
            "https://act-webstatic.hoyoverse.com/c.png"
        ));
        assert!(is_allowed_image_url("https://cdn.mihoyo.com/d.jpg"));
        assert!(is_allowed_image_url("https://hoyolab.com/e.png"));

        assert!(!is_allowed_image_url(""));
        assert!(!is_allowed_image_url(
            "http://upload-os-bbs.hoyolab.com/a.png"
        ));
        assert!(!is_allowed_image_url("https://evil.example.com/a.png"));
        assert!(!is_allowed_image_url("https://evil-hoyolab.com/a.png"));
        assert!(!is_allowed_image_url("https://hoyolab.com.evil.com/a.png"));
        assert!(!is_allowed_image_url(
            "https://upload-os-bbs.hoyolab.com@evil.example.com/a.png"
        ));
        assert!(!is_allowed_image_url(
            "https://upload-os-bbs.hoyolab.com:8443/a.png"
        ));
        assert!(!is_allowed_image_url(
            "https://upload-os-bbs.hoyolab.com\\@evil.example.com/a.png"
        ));
        assert!(!is_allowed_image_url("https://127.0.0.1/a.png"));
        assert!(!is_allowed_image_url(
            "https://upload-os-bbs.hoyolab.com/a.png\r\nX-Inject: 1"
        ));

        let loopback_base = "http://127.0.0.1:41234";
        assert!(is_allowed_image_url_for_base(
            "http://127.0.0.1:41234/img.png",
            loopback_base
        ));
        assert!(!is_allowed_image_url_for_base(
            "http://127.0.0.1:9999/img.png",
            loopback_base
        ));
        assert!(!is_allowed_image_url_for_base(
            "http://127.0.0.1:41234/img.png",
            API_BASE
        ));
        assert!(!is_allowed_image_url_for_base(
            "http://evil.example.com/img.png",
            loopback_base
        ));
        assert!(!is_allowed_image_url_for_base(
            "https://evil.example.com/img.png",
            loopback_base
        ));
    }

    #[test]
    fn mock_http_vi_empty_falls_back_to_en_point_info_and_preserves_lang_cache() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_srv = Arc::clone(&seen);
        let server = MockHttpServer::start(move |addr, target| {
            seen_srv.lock().unwrap().push(target.to_owned());
            if target.starts_with("/v1/map/point/info") && target.contains("lang=vi-vn") {
                json_response(json!({
                    "retcode": 0,
                    "data": { "info": { "content": "  <br/>  ", "img": "" } }
                }))
            } else if target.starts_with("/v1/map/point/info") && target.contains("lang=en-us") {
                json_response(json!({
                    "retcode": 0,
                    "data": {
                        "info": {
                            "content": "<p>Climb the stone pillar</p>",
                            "img": format!("http://{addr}/en_guide.png")
                        }
                    }
                }))
            } else if target == "/en_guide.png" {
                (200, "image/png", b"EN_GUIDE_PNG".to_vec())
            } else {
                (404, "text/plain", b"not found".to_vec())
            }
        });

        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            api_base: server.base_url(),
            ..Default::default()
        });

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(fetcher.request(
            "2001",
            2,
            move |g| {
                let _ = tx.send(g);
            },
            Some("vi")
        ));
        let guide = rx.recv_timeout(Duration::from_secs(5)).expect("guide");
        assert!(guide.ok);
        assert_eq!(guide.pin_id, "2001");
        assert_eq!(guide.description, "Climb the stone pillar");
        assert_eq!(guide.image_url, format!("{}/en_guide.png", server.base_url()));
        assert_eq!(
            guide.image_bytes.as_deref(),
            Some(b"EN_GUIDE_PNG".as_slice())
        );
        assert_eq!(guide.lang, "en-us");

        let hits = seen.lock().unwrap().clone();
        assert_eq!(hits.len(), 3);
        assert!(hits[0].starts_with("/v1/map/point/info") && hits[0].contains("lang=vi-vn"));
        assert!(hits[1].starts_with("/v1/map/point/info") && hits[1].contains("lang=en-us"));
        assert_eq!(hits[2], "/en_guide.png");

        // Cache key remains keyed by requested (lang_tag, pin_id) so switching Popup language refreshes.
        assert!(fetcher.get_cached("2001", Some("vi")).is_some());
        assert!(fetcher.get_cached("2001", Some("en")).is_none());
    }

    #[test]
    fn mock_http_empty_image_falls_back_to_zh_ja_image_only_and_rejects_disallowed_hosts() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_srv = Arc::clone(&seen);
        let server = MockHttpServer::start(move |addr, target| {
            seen_srv.lock().unwrap().push(target.to_owned());
            if target.starts_with("/v1/map/point/info") && target.contains("point_id=3001") {
                if target.contains("lang=zh-cn") {
                    json_response(json!({
                        "retcode": 0,
                        "data": {
                            "info": {
                                "content": "中文说明不应进入越语HUD",
                                "img": format!("http://{addr}/snez_zh.png")
                            }
                        }
                    }))
                } else {
                    json_response(json!({
                        "retcode": 0,
                        "data": { "info": { "content": "", "img": "", "url_list": [] } }
                    }))
                }
            } else if target.starts_with("/v1/map/point/info") && target.contains("point_id=3002") {
                if target.contains("lang=vi-vn") {
                    json_response(json!({
                        "retcode": 0,
                        "data": { "info": { "content": "<p>Mở cơ quan bên trái</p>", "img": "" } }
                    }))
                } else if target.contains("lang=zh-cn") {
                    json_response(json!({
                        "retcode": 0,
                        "data": {
                            "info": {
                                "content": "恶意链接",
                                "img": "https://evil.example.com/steal.png",
                                "url_list": ["https://upload-os-bbs.hoyolab.com@evil.example.com/bypass.png"]
                            }
                        }
                    }))
                } else if target.contains("lang=ja-jp") {
                    json_response(json!({
                        "retcode": 0,
                        "data": {
                            "info": {
                                "content": "日本語説明",
                                "img": format!("http://{addr}/snez_ja.png")
                            }
                        }
                    }))
                } else {
                    json_response(json!({
                        "retcode": 0,
                        "data": { "info": { "content": "", "img": "" } }
                    }))
                }
            } else if target == "/snez_zh.png" {
                (200, "image/png", b"SNEZ_ZH_IMG".to_vec())
            } else if target == "/snez_ja.png" {
                (200, "image/png", b"SNEZ_JA_IMG".to_vec())
            } else {
                (404, "text/plain", b"not found".to_vec())
            }
        });

        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            api_base: server.base_url(),
            ..Default::default()
        });

        // Tier 1 (vi) empty -> Tier 2 (en) empty -> Tier 3 (zh-cn) image-only (no CJK text!).
        let g1 = fetcher.fetch_sync("3001", 2, Some("vi"));
        assert!(g1.ok);
        assert!(g1.description.is_empty(), "must never inject zh-cn text into VI HUD");
        assert_eq!(g1.image_url, format!("{}/snez_zh.png", server.base_url()));
        assert_eq!(g1.image_bytes.as_deref(), Some(b"SNEZ_ZH_IMG".as_slice()));
        assert_eq!(g1.lang, "vi-vn");
        {
            let hits = seen.lock().unwrap();
            assert_eq!(hits.len(), 4);
            assert!(hits.iter().all(|h| !h.contains("/v1/map/comment/list")));
            assert!(hits[0].contains("lang=vi-vn"));
            assert!(hits[1].contains("lang=en-us"));
            assert!(hits[2].contains("lang=zh-cn"));
            assert_eq!(hits[3], "/snez_zh.png");
        }

        // VI text preserved, zh-cn evil hosts rejected -> ja-jp allowlisted screenshot borrowed.
        seen.lock().unwrap().clear();
        let g2 = fetcher.fetch_sync("3002", 2, Some("vi"));
        assert!(g2.ok);
        assert_eq!(g2.description, "Mở cơ quan bên trái");
        assert_eq!(g2.image_url, format!("{}/snez_ja.png", server.base_url()));
        assert_eq!(g2.image_bytes.as_deref(), Some(b"SNEZ_JA_IMG".as_slice()));
        assert_eq!(g2.lang, "vi-vn");
        {
            let hits = seen.lock().unwrap();
            assert_eq!(hits.len(), 5);
            assert!(hits[2].contains("lang=zh-cn"));
            assert!(hits[3].contains("lang=ja-jp"));
            assert_eq!(hits[4], "/snez_ja.png");
        }
    }

    #[test]
    fn third_party_provider_inline_guide_ssrf_allowlist_and_async_fetch() {
        assert!(is_allowed_provider_image_url(
            "https://assets.yuanshen.site/images/560.jpg"
        ));
        assert!(is_allowed_provider_image_url(
            "https://game-cdn.appsample.com/gim/images/123.jpg"
        ));
        assert!(!is_allowed_provider_image_url(
            "http://assets.yuanshen.site/images/560.jpg"
        ));
        assert!(!is_allowed_provider_image_url(
            "https://127.0.0.1/evil.png"
        ));
        assert!(!is_allowed_provider_image_url(
            "https://evil.example.com/assets.yuanshen.site/a.png"
        ));

        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_bytes: Some(Arc::new(|url| {
                assert_eq!(url, "https://assets.yuanshen.site/images/560.jpg");
                Ok(b"KY_IMG_OK".to_vec())
            })),
            ..Default::default()
        });
        assert!(!PinGuideFetcher::is_fetchable_id("kongying:560"));
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(fetcher.request_inline_guide(
            "kongying:560",
            "<b>Under the bridge</b>",
            "https://assets.yuanshen.site/images/560.jpg",
            move |g| {
                let _ = tx.send(g);
            },
            Some("en"),
        ));
        let g = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(g.ok);
        assert_eq!(g.description, "Under the bridge");
        assert_eq!(g.image_bytes.as_deref(), Some(b"KY_IMG_OK".as_slice()));
        assert!(fetcher.get_cached("kongying:560", Some("en")).is_some());
        fetcher.stop();
    }

    #[test]
    fn guide_provider_trait_delegates_cleanly() {
        let fetcher = PinGuideFetcher::with_config(PinGuideConfig {
            fetch_json: Some(Arc::new(|url| {
                if url.contains("/v1/map/point/info") && url.contains("lang=en-us") {
                    Ok(json!({
                        "retcode": 0,
                        "data": { "info": { "content": "<b>Provider</b> tip", "img": "" } }
                    }))
                } else {
                    Ok(json!({
                        "retcode": 0,
                        "data": { "info": { "content": "", "img": "" } }
                    }))
                }
            })),
            ..Default::default()
        });

        let provider: &dyn GuideProvider = &fetcher;
        assert_eq!(provider.provider_name(), "hoyolab");
        assert!(provider.is_fetchable("4001"));
        assert!(!provider.is_fetchable("bad_pin"));
        assert!(provider.get_cached("4001", Some("vi")).is_none());

        let sync_guide = provider.fetch_guide("4001", 2, Some("vi"));
        assert!(sync_guide.ok);
        assert_eq!(sync_guide.description, "Provider tip");
        assert_eq!(sync_guide.lang, "en-us");

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(provider.request_guide(
            "4001",
            2,
            Box::new(move |g| {
                let _ = tx.send(g);
            }),
            Some("vi")
        ));
        let async_guide = rx.recv_timeout(Duration::from_secs(3)).expect("async guide");
        assert!(async_guide.ok);
        assert_eq!(async_guide.description, "Provider tip");
        assert!(provider.get_cached("4001", Some("vi")).is_some());
        assert!(!provider.is_loading("4001", Some("vi")));
    }

    #[test]
    fn pin_icon_cache_lazy_zero_threads_and_disk_hit_plus_ssrf_guard() {
        let dir = std::env::temp_dir().join(format!(
            "gh_pin_icon_cache_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let catalog = Arc::new(HoYoLABLabelCatalog::new());
        let cache = PinIconCache::new(dir.clone(), catalog, Duration::from_secs(2));

        // Zero threads spawned until async request is made.
        assert!(!cache.has_active_worker());
        assert!(cache.get_cached_bytes(2, 42).is_none());

        // Valid PNG magic on disk returns immediately with zero worker threads.
        let png_stub = b"\x89PNG\r\n\x1a\nFAKE_ICON_BYTES".to_vec();
        std::fs::write(cache.icon_path(2, 42), &png_stub).unwrap();
        assert_eq!(cache.get_cached_bytes(2, 42).as_deref(), Some(png_stub.as_slice()));
        assert!(!cache.has_active_worker());

        // Invalid / corrupt payload is rejected cleanly.
        std::fs::write(cache.icon_path(2, 43), b"<html>nope</html>").unwrap();
        assert!(cache.get_cached_bytes(2, 43).is_none());

        // Official HoYoLAB MarkerUiAsset catalog: every URL passes strict SSRF allowlist & round-trips keys.
        assert_eq!(MarkerUiAsset::ALL.len(), 10);
        for &asset in MarkerUiAsset::ALL {
            assert_eq!(MarkerUiAsset::from_key(asset.key()), Some(asset));
            assert!(
                is_allowed_image_url(asset.url()),
                "official asset {} must pass image URL allowlist",
                asset.key()
            );
        }
        assert_eq!(MarkerUiAsset::from_key("../evil"), None);
        assert!(cache.get_ui_asset_bytes(MarkerUiAsset::BgNormal).is_none());
        std::fs::write(cache.ui_asset_path(MarkerUiAsset::BgNormal), &png_stub).unwrap();
        assert_eq!(
            cache.get_ui_asset_bytes(MarkerUiAsset::BgNormal).as_deref(),
            Some(png_stub.as_slice())
        );
        assert!(!cache.has_active_worker());

        cache.stop();
        assert!(!cache.has_active_worker());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
