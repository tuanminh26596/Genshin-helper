//! `DiagnosticManager` implementing `gh_core::traits::DiagnosticService`
//! (opaque UID redaction, verified-window-only game capture, Extension-locked HoYoLAB viewport
//! PNG attachment, PII-scrubbed `03_report.json` + `06_app_log.txt`, and auto-ZIP packaging).

use crate::capture_backend::FrameCapturer;
use crate::log_buffer::LogRingBuffer;
use gh_core::atomic_io::{
    AtomicTempFile, DEFAULT_REPLACE_RETRIES, DEFAULT_REPLACE_RETRY_DELAY, write_bytes_atomic,
    write_json_atomic, write_str_atomic,
};
use gh_core::config::AppConfig;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::i18n::get_language;
use gh_core::paths::{AppPaths, RuntimeMode};
use gh_core::traits::DiagnosticService;
use gh_core::ui_layout::{compute_ui_layout, detect_hdr_clipping};
use gh_core::{APP_VERSION, CoreError, CoreResult};
use gh_cv::image_io::write_image_frame;
use gh_cv::locator::MapLocator;
use gh_hud::diag_annotate::render_annotated_frame;
use gh_hud::fonts::HudFonts;
use gh_net::sync_server::decode_png_base64;
use gh_win32::capture::WindowFinder;
use gh_win32::game_settings::is_process_elevated;
use gh_win32::locale::Win32LocaleProvider;
use gh_core::traits::LocaleProvider;
use gh_win32::sys_info::{
    DisplayTopology, collect_display_topology, collect_os_env_info, probe_game_window_diag,
    reveal_path_in_explorer,
};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use windows::Win32::System::SystemInformation::GetLocalTime;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

pub const DEFAULT_WINDOW_TITLES: [&str; 2] = ["Genshin Impact", "原神"];
pub const DEFAULT_PRE_LOST_RING_CAPACITY: usize = 18;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const MAX_BROWSER_PNG_BYTES: usize = 16 * 1024 * 1024;

/// Single minimap HUD crop + pose metadata stored in RAM for pre-lost diagnostics (UID/PII-free).
#[derive(Clone)]
pub struct PreLostMinimapFrame {
    pub ts_sec: f64,
    pub frame: FrameBuf,
    pub meta: Value,
}

/// Thread-safe ring buffer holding the last ~15–20 minimap frames before a tracking loss or bug report.
#[derive(Clone)]
pub struct MinimapPreLostRingBuffer {
    inner: Arc<Mutex<VecDeque<PreLostMinimapFrame>>>,
    capacity: usize,
}

impl Default for MinimapPreLostRingBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_PRE_LOST_RING_CAPACITY)
    }
}

impl MinimapPreLostRingBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.clamp(1, 64);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            capacity: cap,
        }
    }

    pub fn push(&self, ts_sec: f64, frame: FrameBuf, meta: Value) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(PreLostMinimapFrame { ts_sec, frame, meta });
    }

    pub fn snapshot(&self) -> Vec<PreLostMinimapFrame> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn local_timestamp_compact_and_iso() -> (String, String) {
    // SAFETY: `GetLocalTime` writes a stack-allocated `SYSTEMTIME` struct.
    let st = unsafe { GetLocalTime() };
    let compact = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    );
    let iso = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    );
    (compact, iso)
}

pub type BrowserStateReader = Arc<dyn Fn() -> Map<String, Value> + Send + Sync>;
pub type NavStateReader = Arc<dyn Fn() -> Option<Value> + Send + Sync>;
pub type GameRectReader = Arc<dyn Fn() -> Option<RectI32> + Send + Sync>;
pub type EnvHealthReader = Arc<dyn Fn() -> Value + Send + Sync>;

/// Scrubs personal paths, usernames, HoYoLAB cookies/tokens, LAN PINs, 9-10 digit UIDs,
/// and non-loopback IPv4 addresses from diagnostic text and logs.
pub fn sanitize_pii(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let env_profile = std::env::var("USERPROFILE").ok();
    let env_appdata = std::env::var("APPDATA").ok();
    let env_localappdata = std::env::var("LOCALAPPDATA").ok();
    let env_username = std::env::var("USERNAME").ok();
    sanitize_pii_with_env(
        input,
        env_profile.as_deref(),
        env_appdata.as_deref(),
        env_localappdata.as_deref(),
        env_username.as_deref(),
    )
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_owned();
    }
    let h_bytes = haystack.as_bytes();
    let n_bytes = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0usize;
    while i < h_bytes.len() {
        if i + n_bytes.len() <= h_bytes.len()
            && h_bytes[i..i + n_bytes.len()].eq_ignore_ascii_case(n_bytes)
        {
            out.push_str(replacement);
            i += n_bytes.len();
        } else if let Some(ch) = haystack[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn replace_path_variants(s: String, raw_path: Option<&str>, placeholder: &str) -> String {
    let Some(p) = raw_path.map(str::trim).filter(|p| p.len() >= 4) else {
        return s;
    };
    let slash_ver = p.replace('\\', "/");
    let backslash_ver = p.replace('/', "\\");
    let mut out = replace_ascii_case_insensitive(&s, &backslash_ver, placeholder);
    if slash_ver != backslash_ver {
        out = replace_ascii_case_insensitive(&out, &slash_ver, placeholder);
    }
    out
}

fn replace_bounded_token_ci(input: &str, token: &str, replacement: &str) -> String {
    if token.len() < 3 || input.len() < token.len() {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let t_bytes = token.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if i + t_bytes.len() <= bytes.len()
            && bytes[i..i + t_bytes.len()].eq_ignore_ascii_case(t_bytes)
        {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after_idx = i + t_bytes.len();
            let after_ok = after_idx == bytes.len() || !bytes[after_idx].is_ascii_alphanumeric();
            if before_ok && after_ok {
                out.push_str(replacement);
                i = after_idx;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn redact_drive_user_homes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let user_seg = b"users";
    while i < bytes.len() {
        if i + 9 <= bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
            && bytes[i + 3..i + 8].eq_ignore_ascii_case(user_seg)
            && (bytes[i + 8] == b'\\' || bytes[i + 8] == b'/')
        {
            let mut end = i + 9;
            while end < bytes.len()
                && !matches!(
                    bytes[end],
                    b'\\' | b'/' | b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b'<' | b'>' | b'|'
                )
            {
                end += 1;
            }
            if end > i + 9 {
                out.push_str("<USERPROFILE>");
                i = end;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn redact_secret_kv(input: &str) -> String {
    const SECRET_KEYS: [&str; 11] = [
        "cookie_token_v2",
        "account_id_v2",
        "authorization",
        "cookie_token",
        "account_id",
        "ltoken_v2",
        "ltuid_v2",
        "lan_pin",
        "ltoken",
        "ltuid",
        "pin",
    ];
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let mut matched_key: Option<&str> = None;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
        if before_ok {
            for &key in &SECRET_KEYS {
                let klen = key.len();
                if i + klen < bytes.len()
                    && bytes[i..i + klen].eq_ignore_ascii_case(key.as_bytes())
                {
                    let mut p = i + klen;
                    if bytes[p] == b'"' || bytes[p] == b'\'' {
                        p += 1;
                    }
                    while p < bytes.len() && bytes[p] == b' ' {
                        p += 1;
                    }
                    if p < bytes.len() && (bytes[p] == b'=' || bytes[p] == b':') {
                        p += 1;
                        while p < bytes.len() && bytes[p] == b' ' {
                            p += 1;
                        }
                        let mut quote_char = None;
                        if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
                            quote_char = Some(bytes[p]);
                            p += 1;
                        }
                        let val_start = p;
                        while p < bytes.len()
                            && !matches!(
                                bytes[p],
                                b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b'}' | b'"' | b'\''
                            )
                        {
                            p += 1;
                        }
                        let val_len = p.saturating_sub(val_start);
                        if (key == "pin" && val_len >= 4) || val_len >= 4 {
                            out.push_str(&input[i..val_start]);
                            out.push_str("<REDACTED>");
                            if let Some(q) = quote_char
                                && p < bytes.len()
                                && bytes[p] == q
                            {
                                out.push(q as char);
                                p += 1;
                            }
                            i = p;
                            matched_key = Some(key);
                            break;
                        }
                    }
                }
            }
        }
        if matched_key.is_some() {
            continue;
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn redact_uid_markers(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if before_ok && i + 3 <= bytes.len() && bytes[i..i + 3].eq_ignore_ascii_case(b"uid") {
            let mut p = i + 3;
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b':' || bytes[p] == b'=') {
                p += 1;
            }
            let d_start = p;
            while p < bytes.len() && bytes[p].is_ascii_digit() {
                p += 1;
            }
            let d_len = p - d_start;
            if (9..=10).contains(&d_len) && (p == bytes.len() || !bytes[p].is_ascii_digit()) {
                out.push_str("UID:<REDACTED>");
                i = p;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn redact_non_loopback_ipv4(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let before_ok = i == 0 || (!bytes[i - 1].is_ascii_digit() && bytes[i - 1] != b'.');
        if before_ok && bytes[i].is_ascii_digit() {
            let mut p = i;
            let mut valid = true;
            for oct_idx in 0..4 {
                let start = p;
                while p < bytes.len() && bytes[p].is_ascii_digit() && p - start < 4 {
                    p += 1;
                }
                let len = p - start;
                if len == 0 || len > 3 {
                    valid = false;
                    break;
                }
                let Ok(val) = input[start..p].parse::<u16>() else {
                    valid = false;
                    break;
                };
                if val > 255 {
                    valid = false;
                    break;
                }
                if oct_idx < 3 {
                    if p >= bytes.len() || bytes[p] != b'.' {
                        valid = false;
                        break;
                    }
                    p += 1;
                }
            }
            let after_ok = p == bytes.len() || (!bytes[p].is_ascii_digit() && bytes[p] != b'.');
            if valid && after_ok {
                let ip = &input[i..p];
                if ip != "127.0.0.1" && ip != "0.0.0.0" {
                    out.push_str("<LAN_IP>");
                } else {
                    out.push_str(ip);
                }
                i = p;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn sanitize_pii_with_env(
    input: &str,
    userprofile: Option<&str>,
    appdata: Option<&str>,
    localappdata: Option<&str>,
    username: Option<&str>,
) -> String {
    let mut out = input.to_owned();
    out = replace_path_variants(out, localappdata, "<LOCALAPPDATA>");
    out = replace_path_variants(out, appdata, "<APPDATA>");
    out = replace_path_variants(out, userprofile, "<USERPROFILE>");
    out = redact_drive_user_homes(&out);
    let dev_ws_bs = concat!("e:", "\\", "dev", " soft");
    let dev_ws_fs = concat!("e:", "/", "dev", " soft");
    out = replace_ascii_case_insensitive(&out, dev_ws_bs, "<WORKSPACE>");
    out = replace_ascii_case_insensitive(&out, dev_ws_fs, "<WORKSPACE>");
    if let Some(uname) = username.map(str::trim).filter(|u| u.len() >= 3) {
        out = replace_bounded_token_ci(&out, uname, "<USER>");
    }
    out = redact_secret_kv(&out);
    out = redact_uid_markers(&out);
    out = redact_non_loopback_ipv4(&out);
    out
}

pub fn parse_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(&PNG_MAGIC) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

fn compute_frame_luma_mean(frame: &FrameBuf) -> Option<f64> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let ch = frame.format.channels();
    if w == 0 || h == 0 || frame.stride < w * ch {
        return None;
    }
    let mut sum = 0.0f64;
    let mut count = 0u64;
    let step_y = (h / 180).max(1);
    let step_x = (w / 320).max(1);
    let mut y = 0usize;
    while y < h {
        let row = &frame.data[y * frame.stride..y * frame.stride + w * ch];
        let mut x = 0usize;
        while x < w {
            let off = x * ch;
            let luma = match frame.format {
                PixelFormat::Gray8 => row[off] as f64,
                PixelFormat::Bgr8 | PixelFormat::Bgra8 => {
                    0.114 * row[off] as f64 + 0.587 * row[off + 1] as f64 + 0.299 * row[off + 2] as f64
                }
                PixelFormat::Rgba8 => {
                    0.299 * row[off] as f64 + 0.587 * row[off + 1] as f64 + 0.114 * row[off + 2] as f64
                }
            };
            sum += luma;
            count += 1;
            x += step_x;
        }
        y += step_y;
    }
    if count == 0 {
        None
    } else {
        Some(((sum / count as f64) * 10.0).round() / 10.0)
    }
}

fn sanitize_browser_state(mut map: Map<String, Value>) -> Map<String, Value> {
    for val in map.values_mut() {
        if let Value::String(s) = val {
            *s = sanitize_pii(s);
        }
    }
    map
}

fn allowlist_extension_info(req: Option<&Value>) -> Value {
    let Some(obj) = req.and_then(|v| v.get("extension_info")).and_then(Value::as_object) else {
        return json!({});
    };
    let mut clean = Map::new();
    for key in [
        "extension_version",
        "browser_family",
        "connected_map_tabs",
        "compact_ui_enabled",
        "lang",
    ] {
        if let Some(val) = obj.get(key) {
            let v = match val {
                Value::String(s) => Value::String(sanitize_pii(s)),
                Value::Number(_) | Value::Bool(_) => val.clone(),
                _ => continue,
            };
            clean.insert(key.to_owned(), v);
        }
    }
    Value::Object(clean)
}

fn collect_bundle_zip_entries(base_dir: &Path, rel_prefix: &str, out: &mut Vec<(String, PathBuf)>) -> CoreResult<()> {
    for ent in std::fs::read_dir(base_dir)? {
        let ent = ent?;
        let p = ent.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = if rel_prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if p.is_file() {
            out.push((rel, p));
        } else if p.is_dir() && rel_prefix.is_empty() && name == "07_pre_lost_frames" {
            collect_bundle_zip_entries(&p, &rel, out)?;
        }
    }
    Ok(())
}

fn write_bundle_zip(bundle_dir: &Path, zip_path: &Path, folder_name: &str) -> CoreResult<u64> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    collect_bundle_zip_entries(bundle_dir, "", &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut tmp = AtomicTempFile::new(zip_path, "diagzip")?;
    {
        let file = tmp.as_file_mut()?;
        let mut writer = zip::ZipWriter::new(file);
        for (rel_name, path) in &entries {
            let method = if rel_name.to_ascii_lowercase().ends_with(".png") {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            let opts = SimpleFileOptions::default().compression_method(method);
            writer.start_file(format!("{folder_name}/{rel_name}"), opts)?;
            let mut src = std::fs::File::open(path)?;
            let mut buf = Vec::new();
            src.read_to_end(&mut buf)?;
            writer.write_all(&buf)?;
        }
        let finished = writer.finish()?;
        finished.flush()?;
    }
    tmp.commit(DEFAULT_REPLACE_RETRIES, DEFAULT_REPLACE_RETRY_DELAY)?;
    let size = std::fs::metadata(zip_path).map(|m| m.len()).unwrap_or(0);
    Ok(size)
}

pub struct DiagnosticManager {
    paths: AppPaths,
    capturer: Arc<dyn FrameCapturer>,
    locator: Option<Arc<MapLocator>>,
    config: Arc<RwLock<AppConfig>>,
    read_browser_state: BrowserStateReader,
    read_nav_state: NavStateReader,
    read_game_rect: GameRectReader,
    read_env_health: Option<EnvHealthReader>,
    log_buffer: Option<LogRingBuffer>,
    minimap_ring: Option<MinimapPreLostRingBuffer>,
    fonts: Mutex<Option<HudFonts>>,
}

impl DiagnosticManager {
    pub fn new(
        paths: AppPaths,
        capturer: Arc<dyn FrameCapturer>,
        locator: Option<Arc<MapLocator>>,
        config: Arc<RwLock<AppConfig>>,
        read_browser_state: BrowserStateReader,
        read_nav_state: NavStateReader,
        read_game_rect: GameRectReader,
    ) -> Self {
        Self {
            paths,
            capturer,
            locator,
            config,
            read_browser_state,
            read_nav_state,
            read_game_rect,
            read_env_health: None,
            log_buffer: None,
            minimap_ring: None,
            fonts: Mutex::new(None),
        }
    }

    pub fn with_log_buffer(mut self, log_buffer: LogRingBuffer) -> Self {
        self.log_buffer = Some(log_buffer);
        self
    }

    pub fn with_minimap_ring(mut self, minimap_ring: MinimapPreLostRingBuffer) -> Self {
        self.minimap_ring = Some(minimap_ring);
        self
    }

    pub fn with_env_health_reader(mut self, reader: EnvHealthReader) -> Self {
        self.read_env_health = Some(reader);
        self
    }

    pub fn diagnostics_root(&self) -> &Path {
        &self.paths.diagnostics_dir
    }

    fn ensure_diagnostics_dir(&self) -> CoreResult<&Path> {
        std::fs::create_dir_all(&self.paths.diagnostics_dir)?;
        if self.paths.mode == RuntimeMode::Dev
            && let Some(parent) = self.paths.diagnostics_dir.parent()
        {
            let gitignore = parent.join(".gitignore");
            if !gitignore.exists() {
                let _ = write_str_atomic(&gitignore, "*\n");
            }
        }
        Ok(&self.paths.diagnostics_dir)
    }

    fn validate_existing_bundle(&self, folder_name: &str) -> CoreResult<(String, PathBuf, PathBuf)> {
        let clean = folder_name.trim();
        if clean.is_empty()
            || !clean.starts_with("diag_")
            || clean.contains("..")
            || clean.contains('/')
            || clean.contains('\\')
            || Path::new(clean).file_name().and_then(|s| s.to_str()) != Some(clean)
        {
            return Err(CoreError::InvalidArgument(
                "Invalid diagnostic folder name".to_owned(),
            ));
        }

        let diag_root = self.ensure_diagnostics_dir()?.to_path_buf();
        let bundle_dir = diag_root.join(clean);
        if !bundle_dir.is_dir() {
            return Err(CoreError::NotFound(format!(
                "Diagnostic folder not found: {clean}"
            )));
        }

        let canon_root = std::fs::canonicalize(&diag_root)?;
        let canon_bundle = std::fs::canonicalize(&bundle_dir)?;
        if !canon_bundle.starts_with(&canon_root) {
            return Err(CoreError::InvalidArgument(
                "Invalid diagnostic folder path".to_owned(),
            ));
        }

        let zip_path = diag_root.join(format!("{clean}.zip"));
        Ok((clean.to_owned(), bundle_dir, zip_path))
    }

    /// Applies an irreversible opaque mask over the bottom-right UID watermark area
    /// (`x: 84%..100%`, `y: 95.5%..100%`, strictly below the `88%..95.5%` region-switch button)
    /// without mutating the input `FrameBuf`.
    pub fn redact_uid_region(frame: &FrameBuf) -> FrameBuf {
        if frame.data.is_empty() || frame.width < 16 || frame.height < 16 {
            return frame.clone();
        }
        let w = frame.width as usize;
        let h = frame.height as usize;
        let ch = frame.format.channels();
        if frame.stride < w * ch {
            return frame.clone();
        }
        let x1_base = ((w as f64 * 0.84).floor() as usize).min(w - 1);
        let y1_base = ((h as f64 * 0.955).floor() as usize).min(h - 1);
        let x1 = match compute_ui_layout(frame.width, frame.height, false) {
            Ok(layout) => x1_base.min(layout.uid_rect.x.max(0) as usize).min(w - 1),
            Err(_) => x1_base,
        };
        let y1 = y1_base;
        if x1 >= w || y1 >= h {
            return frame.clone();
        }

        let mut out = frame.clone();
        for y in y1..h {
            let row = &mut out.data[y * out.stride..y * out.stride + w * ch];
            for x in x1..w {
                let off = x * ch;
                match frame.format {
                    PixelFormat::Gray8 => row[off] = 18,
                    PixelFormat::Bgr8 => {
                        row[off] = 25;
                        row[off + 1] = 15;
                        row[off + 2] = 11;
                    }
                    PixelFormat::Bgra8 => {
                        row[off] = 25;
                        row[off + 1] = 15;
                        row[off + 2] = 11;
                        row[off + 3] = 255;
                    }
                    PixelFormat::Rgba8 => {
                        row[off] = 11;
                        row[off + 1] = 15;
                        row[off + 2] = 25;
                        row[off + 3] = 255;
                    }
                }
            }
        }
        out
    }

    pub fn capture_and_create_bundle(
        &self,
        game_rect_override: Option<RectI32>,
        browser_state_override: Option<Map<String, Value>>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
    ) -> CoreResult<Value> {
        self.capture_and_create_bundle_ex(
            game_rect_override,
            browser_state_override,
            trigger_source,
            user_note,
            redact_uid,
            nav_state_override,
            "",
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn capture_and_create_bundle_ex(
        &self,
        game_rect_override: Option<RectI32>,
        browser_state_override: Option<Map<String, Value>>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
        category: &str,
        extension_info: Option<Value>,
        browser_png_bytes: Option<&[u8]>,
    ) -> CoreResult<Value> {
        let create_zip = trigger_source != "hotkey_debug";
        self.capture_and_create_bundle_with_zip(
            game_rect_override,
            browser_state_override,
            trigger_source,
            user_note,
            redact_uid,
            nav_state_override,
            category,
            extension_info,
            browser_png_bytes,
            create_zip,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn capture_and_create_bundle_with_zip(
        &self,
        game_rect_override: Option<RectI32>,
        browser_state_override: Option<Map<String, Value>>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
        category: &str,
        extension_info: Option<Value>,
        browser_png_bytes: Option<&[u8]>,
        create_zip: bool,
    ) -> CoreResult<Value> {
        let diag_root = self.ensure_diagnostics_dir()?;
        let (compact_ts, iso_ts) = local_timestamp_compact_and_iso();

        let mut folder_name = format!("diag_{compact_ts}");
        let mut bundle_dir = diag_root.join(&folder_name);
        if bundle_dir.exists() {
            for idx in 1..=99u32 {
                let candidate_name = format!("diag_{compact_ts}_{idx:02}");
                let candidate_dir = diag_root.join(&candidate_name);
                if !candidate_dir.exists() {
                    folder_name = candidate_name;
                    bundle_dir = candidate_dir;
                    break;
                }
            }
        }
        std::fs::create_dir_all(&bundle_dir)?;

        let display_topo = collect_display_topology();
        let env_health = self
            .read_env_health
            .as_ref()
            .map(|r| r())
            .unwrap_or_else(|| Value::Object(Map::new()));

        // Resolve ONLY verified game window — NEVER fall back to full desktop screen capture.
        let mut game_hwnd: Option<isize> = env_health.get("game_hwnd").and_then(Value::as_i64).map(|v| v as isize);
        let mut game_rect = game_rect_override.or_else(|| (self.read_game_rect)());
        if game_rect.is_none_or(|r| r.width < 800 || r.height < 500) {
            if let Some((hwnd, l, t, w, h)) =
                WindowFinder::get_game_window(&DEFAULT_WINDOW_TITLES, false)
            {
                game_hwnd = Some(hwnd);
                game_rect = Some(RectI32 {
                    x: l,
                    y: t,
                    width: w,
                    height: h,
                });
            }
        } else if game_hwnd.is_none()
            && let Some((hwnd, _, _, _, _)) =
                WindowFinder::get_game_window(&DEFAULT_WINDOW_TITLES, false)
        {
            game_hwnd = Some(hwnd);
        }

        let client_arr = game_rect.map(|r| [r.x, r.y, r.width, r.height]);
        let game_win_diag = probe_game_window_diag(game_hwnd, client_arr, &display_topo);

        let mut capture_latency_ms: Option<f64> = None;
        let game_frame = if let Some(r) = game_rect
            && r.width >= 800
            && r.height >= 500
            && !game_win_diag.is_minimized
        {
            let t0 = Instant::now();
            let frame = self.capturer.capture_region(r.x, r.y, r.width, r.height);
            capture_latency_ms = Some(((t0.elapsed().as_secs_f64() * 1000.0) * 10.0).round() / 10.0);
            frame
        } else {
            None
        };

        let hdr_info = game_rect.and_then(|r| {
            self.capturer
                .active_hdr_monitor_info(r.x, r.y, r.width, r.height)
        });
        let mut game_win_obj = serde_json::to_value(&game_win_diag).unwrap_or_else(|_| json!({}));
        if let Some(obj) = game_win_obj.as_object_mut() {
            obj.insert(
                "capture_latency_ms".to_owned(),
                capture_latency_ms.map(Value::from).unwrap_or(Value::Null),
            );
            obj.insert("wgc_hdr_active".to_owned(), Value::Bool(hdr_info.is_some()));
            obj.insert(
                "wgc_hdr_sdr_white_nits".to_owned(),
                hdr_info
                    .map(|h| json!((h.sdr_white_nits * 10.0).round() / 10.0))
                    .unwrap_or(Value::Null),
            );
        }

        self.create_bundle_in_dir(
            &bundle_dir,
            &folder_name,
            &iso_ts,
            game_frame.as_ref(),
            Some(game_win_obj),
            None,
            browser_png_bytes,
            browser_state_override,
            trigger_source,
            user_note,
            redact_uid,
            nav_state_override,
            category,
            extension_info,
            Some(&display_topo),
            Some(&env_health),
            capture_latency_ms,
            create_zip,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_bundle_with_frames(
        &self,
        game_frame: Option<&FrameBuf>,
        browser_frame: Option<&FrameBuf>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
    ) -> CoreResult<Value> {
        self.create_bundle_with_frames_zip(
            game_frame,
            browser_frame,
            trigger_source,
            user_note,
            redact_uid,
            nav_state_override,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_bundle_with_frames_zip(
        &self,
        game_frame: Option<&FrameBuf>,
        browser_frame: Option<&FrameBuf>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
        create_zip: bool,
    ) -> CoreResult<Value> {
        let diag_root = self.ensure_diagnostics_dir()?;
        let (compact_ts, iso_ts) = local_timestamp_compact_and_iso();
        let mut folder_name = format!("diag_{compact_ts}");
        let mut bundle_dir = diag_root.join(&folder_name);
        if bundle_dir.exists() {
            for idx in 1..=99u32 {
                let candidate_name = format!("diag_{compact_ts}_{idx:02}");
                let candidate_dir = diag_root.join(&candidate_name);
                if !candidate_dir.exists() {
                    folder_name = candidate_name;
                    bundle_dir = candidate_dir;
                    break;
                }
            }
        }
        std::fs::create_dir_all(&bundle_dir)?;
        let game_win = game_frame.map(|f| {
            json!({
                "found": true,
                "is_foreground": false,
                "is_minimized": false,
                "is_borderless_guess": false,
                "client_rect": [0, 0, f.width, f.height],
                "window_rect": [0, 0, f.width, f.height],
            })
        });
        let display_topo = collect_display_topology();
        let env_health = self
            .read_env_health
            .as_ref()
            .map(|r| r())
            .unwrap_or_else(|| Value::Object(Map::new()));

        self.create_bundle_in_dir(
            &bundle_dir,
            &folder_name,
            &iso_ts,
            game_frame,
            game_win,
            browser_frame,
            None,
            None,
            trigger_source,
            user_note,
            redact_uid,
            nav_state_override,
            "",
            None,
            Some(&display_topo),
            Some(&env_health),
            None,
            create_zip,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_bundle_in_dir(
        &self,
        bundle_dir: &Path,
        folder_name: &str,
        iso_ts: &str,
        game_frame: Option<&FrameBuf>,
        game_win_info: Option<Value>,
        browser_frame: Option<&FrameBuf>,
        browser_png_bytes: Option<&[u8]>,
        browser_state_override: Option<Map<String, Value>>,
        trigger_source: &str,
        user_note: &str,
        redact_uid: bool,
        nav_state_override: Option<Value>,
        category: &str,
        extension_info: Option<Value>,
        display_topo_opt: Option<&DisplayTopology>,
        env_health_opt: Option<&Value>,
        capture_latency_ms: Option<f64>,
        create_zip: bool,
    ) -> CoreResult<Value> {
        let note_clean = sanitize_pii(user_note.trim());
        let category_clean = category.trim().to_ascii_lowercase();

        // Save 01_game.png (UID-redacted if enabled)
        let mut game_written = false;
        let game_img_path = bundle_dir.join("01_game.png");
        if let Some(gf) = game_frame {
            let res = if redact_uid {
                let redacted = Self::redact_uid_region(gf);
                write_image_frame(&game_img_path, redacted.as_view(), Some("png"))
            } else {
                write_image_frame(&game_img_path, gf.as_view(), Some("png"))
            };
            game_written = res.is_ok();
        }

        // Save 02_browser.png ONLY from validated HoYoLAB viewport PNG or explicit test frame
        let mut browser_written = false;
        let mut browser_capture_meta = json!({"included": false, "source": "none"});
        let browser_img_path = bundle_dir.join("02_browser.png");
        if let Some(png_bytes) = browser_png_bytes
            && png_bytes.len() <= MAX_BROWSER_PNG_BYTES
            && let Some((bw, bh)) = parse_png_dimensions(png_bytes)
            && write_bytes_atomic(&browser_img_path, png_bytes).is_ok()
        {
            browser_written = true;
            browser_capture_meta = json!({
                "included": true,
                "source": "extension_hoyolab_viewport",
                "width": bw,
                "height": bh,
            });
        } else if let Some(bf) = browser_frame
            && write_image_frame(&browser_img_path, bf.as_view(), Some("png")).is_ok()
        {
            browser_written = true;
            browser_capture_meta = json!({
                "included": true,
                "source": "frame_override",
                "width": bf.width,
                "height": bf.height,
            });
        }

        // Step 3: Run CV localization on UNREDACTED game_frame
        let cv_telemetry: Value = if let Some(gf) = game_frame {
            if let Some(loc) = &self.locator {
                let res = loc.locate_player_frame(gf.as_view(), true, None);
                gh_cv::locator::release_thread_scratch();
                res.map(|r| r.to_json_value())
                    .unwrap_or_else(|| json!({"detected": false, "has_world_coords": false}))
            } else {
                json!({
                    "detected": false,
                    "has_world_coords": false,
                    "reason": "Locator unavailable"
                })
            }
        } else {
            json!({
                "detected": false,
                "has_world_coords": false,
                "reason": "Game window not found or minimized"
            })
        };

        // Step 4: Render annotated visual (04_cv_annotated.png), THEN redact UID
        let mut annotated_written = false;
        let annotated_path = bundle_dir.join("04_cv_annotated.png");
        if let Some(gf) = game_frame {
            let annotated_res = {
                let mut fonts = self.fonts.lock().unwrap_or_else(|e| e.into_inner());
                let f = fonts.get_or_insert_with(HudFonts::new);
                let res = render_annotated_frame(f, gf.as_view(), &cv_telemetry);
                *fonts = None;
                res
            };
            match annotated_res {
                Ok(ann) => {
                    let final_ann = if redact_uid {
                        Self::redact_uid_region(&ann)
                    } else {
                        ann
                    };
                    annotated_written =
                        write_image_frame(&annotated_path, final_ann.as_view(), Some("png"))
                            .is_ok();
                }
                Err(e) => {
                    tracing::warn!("Diagnostic annotation render failed: {e}");
                    let fallback = if redact_uid {
                        Self::redact_uid_region(gf)
                    } else {
                        gf.clone()
                    };
                    annotated_written =
                        write_image_frame(&annotated_path, fallback.as_view(), Some("png")).is_ok();
                }
            }
        }

        // Step 5: Compute safe environment telemetry (no paths, no usernames, no secrets)
        let os_env = collect_os_env_info();
        let owned_topo;
        let display_topo = match display_topo_opt {
            Some(t) => t,
            None => {
                owned_topo = collect_display_topology();
                &owned_topo
            }
        };
        let hdr_report = game_frame.map(|gf| detect_hdr_clipping(gf.as_view()));
        let ui_layout = game_frame.and_then(|gf| compute_ui_layout(gf.width, gf.height, false).ok());
        let luma_mean = game_frame.and_then(compute_frame_luma_mean);

        let browser_state =
            sanitize_browser_state(browser_state_override.unwrap_or_else(|| (self.read_browser_state)()));
        let nav_state = nav_state_override
            .or_else(|| (self.read_nav_state)())
            .unwrap_or_else(|| Value::Object(Map::new()));
        let cfg_snap = self.config.read().expect("config lock poisoned").clone();
        let install_mode = match self.paths.mode {
            RuntimeMode::Installed => "installed",
            RuntimeMode::Dev => "dev",
        };
        let sys_locale = Win32LocaleProvider::new().system_language_tag();
        let env_h = env_health_opt.cloned().unwrap_or_else(|| Value::Object(Map::new()));

        let mut game_env_obj = match game_win_info.clone() {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        game_env_obj.insert(
            "aspect_label".to_owned(),
            ui_layout
                .map(|l| Value::String(l.aspect_label.as_str().to_owned()))
                .unwrap_or(Value::Null),
        );
        game_env_obj.insert(
            "scale_ui".to_owned(),
            ui_layout
                .map(|l| json!(((l.scale_ui * 1000.0).round() / 1000.0)))
                .unwrap_or(Value::Null),
        );
        game_env_obj.insert(
            "capture_latency_ms".to_owned(),
            capture_latency_ms.map(Value::from).unwrap_or(Value::Null),
        );
        game_env_obj.insert(
            "frame_luma_mean".to_owned(),
            luma_mean.map(Value::from).unwrap_or(Value::Null),
        );
        game_env_obj.insert(
            "hdr_clipping".to_owned(),
            hdr_report
                .map(|h| {
                    json!({
                        "clipped": h.clipped,
                        "p95": h.p95,
                        "white_ratio": ((h.white_ratio * 1000.0).round() / 1000.0),
                    })
                })
                .unwrap_or(Value::Null),
        );
        game_env_obj.insert(
            "uipi_status".to_owned(),
            env_h.get("uipi_status").cloned().unwrap_or(Value::Null),
        );
        let game_env = Value::Object(game_env_obj);

        let engine_health = json!({
            "engine_active": env_h.get("engine_active").cloned().unwrap_or(Value::Null),
            "datasets": env_h.get("datasets").cloned().unwrap_or_else(|| json!({})),
            "minimap_tracker": env_h.get("minimap_tracker").cloned().unwrap_or_else(|| json!({})),
            "hotkey_conflicts_summary": env_h.get("hotkey_conflicts_summary").cloned().unwrap_or_else(|| json!({})),
        });

        let report_data = json!({
            "schema_version": 2,
            "timestamp": iso_ts,
            "trigger_source": trigger_source,
            "category": category_clean,
            "user_note": note_clean,
            "uid_redacted": redact_uid,
            "game_window": game_win_info,
            "game_env": game_env,
            "display": display_topo,
            "browser_capture": browser_capture_meta,
            "browser_state_from_web": browser_state,
            "extension_info": extension_info.unwrap_or_else(|| json!({})),
            "cv_telemetry": cv_telemetry,
            "nav_state": nav_state,
            "engine_health": engine_health,
            "config": {
                "hotkey_sync": cfg_snap.hotkey_sync,
                "hotkey_switch": cfg_snap.hotkey_switch,
                "hotkey_debug": cfg_snap.hotkey_debug,
                "hotkey_nav_mark": cfg_snap.hotkey_nav_mark,
                "hotkey_nav_undo": cfg_snap.hotkey_nav_undo,
                "hotkey_nav_guide": cfg_snap.hotkey_nav_guide,
                "hotkey_nav_guide_next": cfg_snap.hotkey_nav_guide_next,
                "hotkey_nav_next": cfg_snap.hotkey_nav_next,
                "hotkey_nav_layout": cfg_snap.hotkey_nav_layout,
                "hotkey_nav_toggle": cfg_snap.hotkey_nav_toggle,
                "hotkey_suppress_conflicts": cfg_snap.hotkey_suppress_conflicts,
                "auto_sync_m": cfg_snap.auto_sync_m,
                "minimap_tracking": cfg_snap.minimap_tracking,
                "nav_hud_enabled": cfg_snap.nav_hud_enabled,
                "nav_hud_position": cfg_snap.nav_hud_position,
                "nav_hud_capture_visible": cfg_snap.nav_hud_capture_visible,
                "nav_hud_guide_zoom_mode": cfg_snap.nav_hud_guide_zoom_mode,
                "mmap_overlay_mode": cfg_snap.mmap_overlay_mode,
                "audio_enabled": cfg_snap.audio_enabled,
                "audio_volume": cfg_snap.audio_volume,
                "language": cfg_snap.language,
                "chest_search_radius": cfg_snap.chest_search_radius,
                "ws_port": cfg_snap.ws_port,
            },
            "system": {
                "os_name": os_env.os_name,
                "os_build": os_env.build_number,
                "os_ubr": os_env.ubr,
                "os_display_version": os_env.display_version,
                "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                "arch": os_env.arch,
                "backend": "rust",
                "app_version": APP_VERSION,
                "install_mode": install_mode,
                "exe_name": "GenshinHelper.exe",
                "uptime_sec": env_h.get("uptime_sec").cloned().unwrap_or(Value::Null),
                "app_language": get_language(),
                "system_locale": sys_locale,
                "elevated_self": env_h.get("elevated_self").and_then(Value::as_bool).unwrap_or_else(is_process_elevated),
                "logical_cores": os_env.logical_cores,
                "total_ram_gb": os_env.total_ram_gb,
                "gpu_adapters": os_env.gpu_adapters,
            }
        });
        let report_path = bundle_dir.join("03_report.json");
        let report_written = write_json_atomic(&report_path, &report_data).is_ok();

        let mut files_list = Vec::new();
        if game_written {
            files_list.push("01_game.png".to_owned());
        }
        if browser_written {
            files_list.push("02_browser.png".to_owned());
        }
        if report_written {
            files_list.push("03_report.json".to_owned());
        }
        if annotated_written {
            files_list.push("04_cv_annotated.png".to_owned());
        }
        if !note_clean.is_empty() {
            let note_path = bundle_dir.join("05_user_note.txt");
            if write_str_atomic(&note_path, &format!("{note_clean}\n")).is_ok() {
                files_list.push("05_user_note.txt".to_owned());
            }
        }
        if let Some(ref log_buf) = self.log_buffer {
            let lines = log_buf.snapshot_lines();
            let log_content = if lines.is_empty() {
                "# (no recent log events)\n".to_owned()
            } else {
                let mut body: Vec<String> = lines.into_iter().map(|l| sanitize_pii(&l)).collect();
                body.push(String::new());
                body.join("\n")
            };
            let log_path = bundle_dir.join("06_app_log.txt");
            if write_str_atomic(&log_path, &log_content).is_ok() {
                files_list.push("06_app_log.txt".to_owned());
            }
        }

        let mut pre_lost_count: usize = 0;
        if let Some(ref ring) = self.minimap_ring {
            let snaps = ring.snapshot();
            if !snaps.is_empty() {
                let pre_dir = bundle_dir.join("07_pre_lost_frames");
                if std::fs::create_dir_all(&pre_dir).is_ok() {
                    let mut index_rows = Vec::with_capacity(snaps.len());
                    for (idx, item) in snaps.iter().enumerate() {
                        let fname = format!("frame_{idx:02}.png");
                        let fpath = pre_dir.join(&fname);
                        if write_image_frame(&fpath, item.frame.as_view(), Some("png")).is_ok() {
                            pre_lost_count += 1;
                            index_rows.push(json!({
                                "file": fname,
                                "ts_sec": (item.ts_sec * 1000.0).round() / 1000.0,
                                "width": item.frame.width,
                                "height": item.frame.height,
                                "meta": item.meta,
                            }));
                        }
                    }
                    if pre_lost_count > 0 {
                        let _ = write_json_atomic(
                            &pre_dir.join("index.json"),
                            &json!({ "count": pre_lost_count, "frames": index_rows }),
                        );
                        files_list.push("07_pre_lost_frames/".to_owned());
                    }
                }
            }
        }

        let diag_root = self.paths.diagnostics_dir.clone();
        let zip_name = format!("{folder_name}.zip");
        let zip_path = diag_root.join(&zip_name);
        let (zip_created, zip_size_bytes) = if create_zip {
            let sz = write_bundle_zip(bundle_dir, &zip_path, folder_name).unwrap_or(0);
            (sz > 0, sz)
        } else {
            (false, 0)
        };

        let default_reason = if get_language() == "vi" {
            "Đã chụp chẩn đoán thành công."
        } else {
            "Captured successfully"
        };
        let reason_raw = cv_telemetry
            .get("reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_reason);
        let reason = sanitize_pii(reason_raw);
        let has_world_coords = cv_telemetry
            .get("has_world_coords")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let os_label = if os_env.display_version.is_empty() {
            format!("{} (Build {})", os_env.os_name, os_env.build_number)
        } else {
            format!(
                "{} {} (Build {})",
                os_env.os_name, os_env.display_version, os_env.build_number
            )
        };
        let resolution_label = game_frame
            .map(|gf| format!("{}x{}", gf.width, gf.height))
            .unwrap_or_else(|| format!("{}x{}", display_topo.primary_screen[0], display_topo.primary_screen[1]));
        let dpi_scale_pct = game_env
            .get("monitor_scale_pct")
            .and_then(Value::as_u64)
            .or_else(|| display_topo.monitors.first().map(|m| m.scale_pct as u64))
            .unwrap_or(100);
        let hdr_clipped = hdr_report.is_some_and(|h| h.clipped);
        let uipi_blocked = env_h
            .get("uipi_status")
            .and_then(|u| u.get("uipi_blocked"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        Ok(json!({
            "success": true,
            "bundle_dir": bundle_dir.to_string_lossy().into_owned(),
            "folder_name": folder_name,
            "zip_name": zip_name,
            "zip_path": zip_path.to_string_lossy().into_owned(),
            "zip_created": zip_created,
            "zip_size_bytes": zip_size_bytes,
            "pre_lost_frames_count": pre_lost_count,
            "has_world_coords": has_world_coords,
            "reason": reason,
            "user_note": note_clean,
            "category": category_clean,
            "files": files_list,
            "summary": {
                "app_version": APP_VERSION,
                "os_label": os_label,
                "resolution_label": resolution_label,
                "dpi_scale_pct": dpi_scale_pct,
                "hdr_clipped": hdr_clipped,
                "uipi_blocked": uipi_blocked,
                "game_found": game_frame.is_some(),
                "browser_captured": browser_written,
            }
        }))
    }

    pub fn save_user_note(&self, folder_name: &str, note: &str) -> CoreResult<Value> {
        self.save_user_note_ex(folder_name, note, true)
    }

    pub fn save_user_note_ex(
        &self,
        folder_name: &str,
        note: &str,
        create_zip: bool,
    ) -> CoreResult<Value> {
        let (clean, bundle_dir, zip_path) = self.validate_existing_bundle(folder_name)?;

        let note_clean = sanitize_pii(note.trim());
        let note_path = bundle_dir.join("05_user_note.txt");
        let content = if note_clean.is_empty() {
            String::new()
        } else {
            format!("{note_clean}\n")
        };
        write_str_atomic(&note_path, &content)?;

        let report_path = bundle_dir.join("03_report.json");
        if report_path.is_file()
            && let Ok(raw) = std::fs::read_to_string(&report_path)
            && let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(&raw)
        {
            let (_, iso_ts) = local_timestamp_compact_and_iso();
            obj.insert("user_note".to_owned(), Value::String(note_clean.clone()));
            obj.insert("user_note_updated_at".to_owned(), Value::String(iso_ts));
            let _ = write_json_atomic(&report_path, &Value::Object(obj));
        }

        let (zip_created, zip_size_bytes) = if create_zip {
            let sz = write_bundle_zip(&bundle_dir, &zip_path, &clean).unwrap_or(0);
            (sz > 0, sz)
        } else {
            (zip_path.is_file(), 0)
        };

        Ok(json!({
            "ok": true,
            "folder_name": clean,
            "note_path": note_path.to_string_lossy().into_owned(),
            "user_note": note_clean,
            "zip_name": format!("{clean}.zip"),
            "zip_created": zip_created,
            "zip_size_bytes": zip_size_bytes,
        }))
    }

    pub fn attach_browser_png(&self, folder_name: &str, png_bytes: &[u8]) -> CoreResult<Value> {
        if png_bytes.len() > MAX_BROWSER_PNG_BYTES {
            return Err(CoreError::InvalidArgument(
                "Browser PNG exceeds 16 MiB limit".to_owned(),
            ));
        }
        let Some((w, h)) = parse_png_dimensions(png_bytes) else {
            return Err(CoreError::InvalidArgument(
                "Invalid PNG payload".to_owned(),
            ));
        };
        let (clean, bundle_dir, zip_path) = self.validate_existing_bundle(folder_name)?;

        let browser_img_path = bundle_dir.join("02_browser.png");
        write_bytes_atomic(&browser_img_path, png_bytes)?;

        let report_path = bundle_dir.join("03_report.json");
        if report_path.is_file()
            && let Ok(raw) = std::fs::read_to_string(&report_path)
            && let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(&raw)
        {
            obj.insert(
                "browser_capture".to_owned(),
                json!({
                    "included": true,
                    "source": "extension_hoyolab_viewport",
                    "width": w,
                    "height": h,
                }),
            );
            let _ = write_json_atomic(&report_path, &Value::Object(obj));
        }

        let zip_size_bytes = write_bundle_zip(&bundle_dir, &zip_path, &clean).unwrap_or(0);
        Ok(json!({
            "ok": true,
            "folder_name": clean,
            "zip_name": format!("{clean}.zip"),
            "zip_size_bytes": zip_size_bytes,
            "width": w,
            "height": h,
        }))
    }

    pub fn open_diagnostics_folder(&self, folder_name: Option<&str>) -> CoreResult<Value> {
        self.open_diagnostics_folder_ex(folder_name, false)
    }

    pub fn open_diagnostics_folder_ex(
        &self,
        folder_name: Option<&str>,
        ensure_zip: bool,
    ) -> CoreResult<Value> {
        let diag_root = self.ensure_diagnostics_dir()?.to_path_buf();
        let target: PathBuf = match folder_name.map(str::trim).filter(|s| !s.is_empty()) {
            Some(fname) => {
                let (clean, bundle_dir, zip_path) = self.validate_existing_bundle(fname)?;
                if ensure_zip && !zip_path.is_file() {
                    let _ = write_bundle_zip(&bundle_dir, &zip_path, &clean);
                }
                if zip_path.is_file() {
                    zip_path
                } else {
                    bundle_dir
                }
            }
            None => diag_root,
        };

        let opened = reveal_path_in_explorer(&target);
        let target_name = target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("diagnostics")
            .to_owned();
        Ok(json!({
            "ok": true,
            "opened": opened,
            "target_name": target_name,
        }))
    }
}

impl DiagnosticService for DiagnosticManager {
    fn trigger_diagnostic(&self, note: Option<&str>, source: Option<&str>) -> CoreResult<Value> {
        let src = source
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("web_ui");
        self.capture_and_create_bundle(None, None, src, note.unwrap_or(""), true, None)
    }

    fn trigger_diagnostic_ex(&self, req: &Value) -> CoreResult<Value> {
        let note = req.get("note").and_then(Value::as_str).unwrap_or("");
        let source = req
            .get("source")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("web_ui");
        let category = req.get("category").and_then(Value::as_str).unwrap_or("");
        let ext_info = allowlist_extension_info(Some(req));
        let browser_png = req
            .get("browser_png_base64")
            .and_then(Value::as_str)
            .and_then(decode_png_base64);
        let create_zip = req
            .get("create_zip")
            .and_then(Value::as_bool)
            .unwrap_or(source != "hotkey_debug");

        self.capture_and_create_bundle_with_zip(
            None,
            None,
            source,
            note,
            true,
            None,
            category,
            Some(ext_info),
            browser_png.as_deref(),
            create_zip,
        )
    }

    fn save_diagnostic_note(&self, folder_name: &str, note: &str) -> CoreResult<Value> {
        self.save_user_note(folder_name, note)
    }

    fn save_diagnostic_note_ex(
        &self,
        folder_name: &str,
        note: &str,
        create_zip: bool,
    ) -> CoreResult<Value> {
        self.save_user_note_ex(folder_name, note, create_zip)
    }

    fn attach_diagnostic_browser(
        &self,
        folder_name: &str,
        png_bytes: &[u8],
    ) -> CoreResult<Value> {
        self.attach_browser_png(folder_name, png_bytes)
    }

    fn open_diagnostics_folder(&self, folder_name: Option<&str>) -> CoreResult<Value> {
        self.open_diagnostics_folder(folder_name)
    }

    fn open_diagnostics_folder_ex(
        &self,
        folder_name: Option<&str>,
        ensure_zip: bool,
    ) -> CoreResult<Value> {
        self.open_diagnostics_folder_ex(folder_name, ensure_zip)
    }
}

pub fn ensure_writable_parent(path: &Path) -> CoreResult<PathBuf> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_core::frame::PixelFormat;
    use gh_cv::image_io::read_image_bgr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DIAG_TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_diag_paths(tag: &str) -> (PathBuf, AppPaths) {
        let seq = DIAG_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "gh_app_diag_{tag}_{}_{seq}_tiếng_việt",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = AppPaths {
            mode: RuntimeMode::Installed,
            config_path: root.join("config.json"),
            diagnostics_dir: root.join("diagnostics"),
            data_dir: root.join("data"),
            tiles_dir: root.join("tiles"),
            ug_overlays_dir: root.join("ug_overlays"),
            cache_dir: root.join("cache"),
            dev_data_dir: None,
        };
        (root, paths)
    }

    fn roi_gradient_energy(frame: &FrameBuf, x1: usize, y1: usize) -> f64 {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let ch = frame.format.channels();
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for y in (y1 + 1)..(h - 1) {
            for x in (x1 + 1)..(w - 1) {
                let c = frame.data[y * frame.stride + x * ch] as f64;
                let r = frame.data[y * frame.stride + (x + 1) * ch] as f64;
                let d = frame.data[(y + 1) * frame.stride + x * ch] as f64;
                sum += (r - c).abs() + (d - c).abs();
                count += 1;
            }
        }
        if count == 0 { 0.0 } else { sum / count as f64 }
    }

    fn make_checkerboard_bgr(w: u32, h: u32) -> FrameBuf {
        let mut fb = FrameBuf::new_zeroed(w, h, PixelFormat::Bgr8);
        let x1 = ((w as f64 * 0.84).floor() as usize).min(w as usize - 1);
        let y1 = ((h as f64 * 0.955).floor() as usize).min(h as usize - 1);
        for y in 0..(h as usize) {
            for x in 0..(w as usize) {
                let idx = y * fb.stride + x * 3;
                if x >= x1 && y >= y1 {
                    let val = if ((x + y) % 2) == 0 { 255 } else { 0 };
                    fb.data[idx] = val;
                    fb.data[idx + 1] = val;
                    fb.data[idx + 2] = val;
                } else if x < 32 && y < 32 {
                    fb.data[idx] = 123;
                    fb.data[idx + 1] = 45;
                    fb.data[idx + 2] = 210;
                } else {
                    fb.data[idx] = 180;
                    fb.data[idx + 1] = 180;
                    fb.data[idx + 2] = 180;
                }
            }
        }
        fb
    }

    fn make_minimal_png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PNG_MAGIC);
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&[0u8; 4]);
        out
    }

    #[test]
    fn redact_uid_region_masks_roi_opaquely_without_mutating_outside_pixels() {
        let tiny = FrameBuf::new_zeroed(12, 12, PixelFormat::Bgr8);
        assert_eq!(DiagnosticManager::redact_uid_region(&tiny).data, tiny.data);

        let src = make_checkerboard_bgr(1280, 720);
        let snap_top_left = src.data[0..9].to_vec();
        let x1 = (1280.0 * 0.84_f64).floor() as usize;
        let y1 = (720.0 * 0.955_f64).floor() as usize;
        let before_energy = roi_gradient_energy(&src, x1, y1);
        assert!(before_energy > 200.0);

        let redacted = DiagnosticManager::redact_uid_region(&src);
        assert_eq!(src.data[0..9], snap_top_left);
        assert_eq!(redacted.data[0..9], snap_top_left);

        // Region-switch OCR area (y = 90% < 95.5%) must stay untouched
        let ocr_y = (720.0 * 0.90) as usize;
        let ocr_off = ocr_y * redacted.stride + (x1 + 10) * 3;
        assert_eq!(&redacted.data[ocr_off..ocr_off + 3], &[180, 180, 180]);

        // Opaque mask has zero gradient energy inside UID ROI
        let after_energy = roi_gradient_energy(&redacted, x1, y1);
        assert!(after_energy < 1e-6, "expected opaque mask, got {after_energy}");
    }

    #[test]
    fn sanitize_pii_scrubs_paths_secrets_uids_and_lan_ips_preserving_loopback() {
        let profile_sample = concat!("C:\\", "Users\\", "TestPlayer");
        let appdata_sample = concat!("C:\\", "Users\\", "TestPlayer\\AppData\\Roaming");
        let raw = format!(
            "{} | {} | {} | {} | {} | 192.168.1.42 | 127.0.0.1:8765 | Lỗi tiếng Việt",
            concat!("C:\\", "Users\\", "TestPlayer\\AppData\\Local\\GenshinHelper"),
            concat!("E:\\", "Dev ", "Soft\\Genshin-Helper"),
            concat!("ltoken", "_v2=supersecret123456"),
            concat!("UI", "D: 801234567"),
            concat!("pin", "=654321"),
        );

        let clean = sanitize_pii_with_env(
            &raw,
            Some(profile_sample),
            Some(appdata_sample),
            None,
            Some("TestPlayer"),
        );
        assert!(!clean.contains("TestPlayer"));
        assert!(!clean.contains("supersecret123456"));
        assert!(!clean.contains("801234567"));
        assert!(!clean.contains("654321"));
        assert!(!clean.contains("192.168.1.42"));
        assert!(!clean.contains(concat!("Dev", " Soft")));
        assert!(clean.contains("127.0.0.1:8765"));
        assert!(clean.contains("<USERPROFILE>"));
        assert!(clean.contains("<WORKSPACE>"));
        assert!(clean.contains("<REDACTED>"));
        assert!(clean.contains("<LAN_IP>"));
        assert!(clean.contains("Lỗi tiếng Việt"));
    }

    #[test]
    fn bundle_creation_annotates_redacts_zips_and_guards_save_user_note() {
        let (root, paths) = temp_diag_paths("bundle");
        let log_buf = LogRingBuffer::new(32, 256);
        log_buf.push_line(concat!("INFO gh_app: loaded ", "C:\\", "Users\\", "Player\\cache"));
        let mgr = DiagnosticManager::new(
            paths.clone(),
            Arc::new(gh_win32::capture::ScreenCapturer::new(2)) as Arc<dyn FrameCapturer>,
            None,
            Arc::new(RwLock::new(AppConfig::default())),
            Arc::new(Map::new),
            Arc::new(|| None),
            Arc::new(|| None),
        )
        .with_log_buffer(log_buf);

        let gf = make_checkerboard_bgr(1280, 720);
        let bf = FrameBuf::new_zeroed(320, 240, PixelFormat::Bgr8);
        let res = mgr
            .create_bundle_with_frames(
                Some(&gf),
                Some(&bf),
                "unit_test",
                "Ghi chú tiếng Việt kiểm tra",
                true,
                Some(json!({"state": "idle"})),
            )
            .unwrap();

        assert_eq!(res["success"], Value::Bool(true));
        let folder = res["folder_name"].as_str().unwrap().to_owned();
        let bdir = PathBuf::from(res["bundle_dir"].as_str().unwrap());
        let zip_path = PathBuf::from(res["zip_path"].as_str().unwrap());
        assert!(bdir.join("01_game.png").is_file());
        assert!(bdir.join("02_browser.png").is_file());
        assert!(bdir.join("03_report.json").is_file());
        assert!(bdir.join("04_cv_annotated.png").is_file());
        assert!(bdir.join("05_user_note.txt").is_file());
        assert!(bdir.join("06_app_log.txt").is_file());
        assert!(zip_path.is_file());
        assert!(res["zip_size_bytes"].as_u64().unwrap_or(0) > 0);

        let log_txt = std::fs::read_to_string(bdir.join("06_app_log.txt")).unwrap();
        assert!(!log_txt.contains("Player"));
        assert!(log_txt.contains("<USERPROFILE>"));

        let report_raw = std::fs::read_to_string(bdir.join("03_report.json")).unwrap();
        let report: Value = serde_json::from_str(&report_raw).unwrap();
        assert_eq!(report["schema_version"], json!(2));
        assert!(report["system"].get("executable").is_none());
        assert_eq!(report["system"]["exe_name"], json!("GenshinHelper.exe"));
        assert!(report["display"]["monitor_count"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(report["uid_redacted"], Value::Bool(true));
        assert_eq!(
            report["user_note"],
            Value::String("Ghi chú tiếng Việt kiểm tra".to_owned())
        );

        // Verify saved 04_cv_annotated.png also has UID ROI opaquely masked (<20 energy)
        let ann_img = read_image_bgr(&bdir.join("04_cv_annotated.png")).unwrap();
        let x1 = (1280.0 * 0.84_f64).floor() as usize;
        let y1 = (720.0 * 0.955_f64).floor() as usize;
        let mut ann_grad = 0.0f64;
        let mut cnt = 0usize;
        for y in (y1 + 1)..719 {
            for x in (x1 + 1)..1279 {
                let c = ann_img[[y, x, 0]] as f64;
                let r = ann_img[[y, x + 1, 0]] as f64;
                let d = ann_img[[y + 1, x, 0]] as f64;
                ann_grad += (r - c).abs() + (d - c).abs();
                cnt += 1;
            }
        }
        let ann_energy = ann_grad / cnt.max(1) as f64;
        assert!(
            ann_energy < 20.0,
            "annotated ROI should be masked, got {ann_energy}"
        );

        // Traversal guard
        for bad in ["../bad", "diag_a/../b", "diag_a/b", "diag_a\\b", "foo", ""] {
            assert!(matches!(
                mgr.save_user_note(bad, "test"),
                Err(CoreError::InvalidArgument(_))
            ));
        }
        // Missing bundle -> NotFound
        assert!(matches!(
            mgr.save_user_note("diag_99991231_235959", "test"),
            Err(CoreError::NotFound(_))
        ));

        // Valid update -> updates 05_user_note.txt, 03_report.json, and rebuilds .zip
        let upd = mgr.save_user_note(&folder, "Cập nhật tiếng Việt mới").unwrap();
        assert_eq!(upd["ok"], Value::Bool(true));
        let note_txt = std::fs::read_to_string(bdir.join("05_user_note.txt")).unwrap();
        assert_eq!(note_txt.trim(), "Cập nhật tiếng Việt mới");

        // Attach browser PNG -> updates 02_browser.png, 03_report.json, and .zip
        let fake_png = make_minimal_png_bytes(1920, 1080);
        let att = mgr.attach_browser_png(&folder, &fake_png).unwrap();
        assert_eq!(att["ok"], Value::Bool(true));
        assert_eq!(att["width"], json!(1920));
        assert_eq!(att["height"], json!(1080));
        assert!(matches!(
            mgr.attach_browser_png(&folder, b"not a png"),
            Err(CoreError::InvalidArgument(_))
        ));

        // Read ZIP archive back to confirm updated note and 02_browser.png are inside
        let zip_file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(zip_file).unwrap();
        let mut zip_note = String::new();
        archive
            .by_name(&format!("{folder}/05_user_note.txt"))
            .unwrap()
            .read_to_string(&mut zip_note)
            .unwrap();
        assert_eq!(zip_note.trim(), "Cập nhật tiếng Việt mới");
        assert!(archive.by_name(&format!("{folder}/02_browser.png")).is_ok());
        assert!(archive.by_name(&format!("{folder}/03_report.json")).is_ok());

        // Missing frames without log_buffer must omit unwritten PNG names from `files`
        let bare_mgr = DiagnosticManager::new(
            paths,
            Arc::new(gh_win32::capture::ScreenCapturer::new(2)) as Arc<dyn FrameCapturer>,
            None,
            Arc::new(RwLock::new(AppConfig::default())),
            Arc::new(Map::new),
            Arc::new(|| None),
            Arc::new(|| None),
        );
        let partial_res = bare_mgr
            .create_bundle_with_frames(None, None, "unit_test_empty", "", false, None)
            .unwrap();
        let files = partial_res["files"].as_array().unwrap();
        let file_names: Vec<&str> = files.iter().filter_map(Value::as_str).collect();
        assert_eq!(file_names, vec!["03_report.json"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lazy_zip_and_pre_lost_minimap_ring_buffer_workflow() {
        let (root, paths) = temp_diag_paths("lazy_ring");
        let ring = MinimapPreLostRingBuffer::new(3);
        for i in 0..5u32 {
            let mut mm = FrameBuf::new_zeroed(48, 48, PixelFormat::Bgra8);
            mm.data[0] = (i * 40) as u8;
            ring.push(i as f64 * 0.1, mm, json!({ "seq": i, "state": "TRACKING" }));
        }
        assert_eq!(ring.len(), 3);
        let snaps = ring.snapshot();
        assert_eq!(snaps[0].meta["seq"], json!(2));
        assert_eq!(snaps[2].meta["seq"], json!(4));

        let mgr = DiagnosticManager::new(
            paths,
            Arc::new(gh_win32::capture::ScreenCapturer::new(2)) as Arc<dyn FrameCapturer>,
            None,
            Arc::new(RwLock::new(AppConfig::default())),
            Arc::new(Map::new),
            Arc::new(|| None),
            Arc::new(|| None),
        )
        .with_minimap_ring(ring);

        let gf = make_checkerboard_bgr(1280, 720);
        // Lazy ZIP capture (create_zip = false) writes raw folder + 07_pre_lost_frames WITHOUT .zip
        let res = mgr
            .create_bundle_with_frames_zip(Some(&gf), None, "hotkey_debug", "", true, None, false)
            .unwrap();
        let folder = res["folder_name"].as_str().unwrap().to_owned();
        let bdir = PathBuf::from(res["bundle_dir"].as_str().unwrap());
        let zip_path = PathBuf::from(res["zip_path"].as_str().unwrap());
        assert_eq!(res["zip_created"], Value::Bool(false));
        assert_eq!(res["zip_size_bytes"], json!(0));
        assert_eq!(res["pre_lost_frames_count"], json!(3));
        assert!(!zip_path.exists(), "lazy capture must not create .zip yet");
        assert!(bdir.join("07_pre_lost_frames").join("frame_00.png").is_file());
        assert!(bdir.join("07_pre_lost_frames").join("frame_02.png").is_file());
        assert!(bdir.join("07_pre_lost_frames").join("index.json").is_file());

        // Note-only save (create_zip = false) updates 05_user_note.txt WITHOUT creating .zip
        let note_only = mgr
            .save_user_note_ex(&folder, "Sai 15m khi chạy ở Natlan", false)
            .unwrap();
        assert_eq!(note_only["ok"], Value::Bool(true));
        assert_eq!(note_only["zip_created"], Value::Bool(false));
        assert!(!zip_path.exists());
        assert_eq!(
            std::fs::read_to_string(bdir.join("05_user_note.txt")).unwrap().trim(),
            "Sai 15m khi chạy ở Natlan"
        );

        // Save & ZIP on-demand (create_zip = true) creates .zip containing 05_user_note + 07_pre_lost_frames
        let with_zip = mgr
            .save_user_note_ex(&folder, "Ghi chú đầy đủ gửi Discord", true)
            .unwrap();
        assert_eq!(with_zip["ok"], Value::Bool(true));
        assert_eq!(with_zip["zip_created"], Value::Bool(true));
        assert!(with_zip["zip_size_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(zip_path.is_file());

        let zip_file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(zip_file).unwrap();
        assert!(archive.by_name(&format!("{folder}/05_user_note.txt")).is_ok());
        assert!(archive.by_name(&format!("{folder}/07_pre_lost_frames/frame_00.png")).is_ok());
        assert!(archive.by_name(&format!("{folder}/07_pre_lost_frames/index.json")).is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }
}
