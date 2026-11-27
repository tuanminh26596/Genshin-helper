//! GitHub Releases update checker & fail-closed SHA-256-verified installer downloader.
//!
//! Design invariants:
//! - Zero telemetry, zero background polling loops, zero silent auto-install: startup performs
//!   at most one opt-outable metadata check (`auto_check_app_updates`), and installer download /
//!   execution happens ONLY upon explicit user action.
//! - Zero external crypto dependencies: implements FIPS 180-4 SHA-256 directly in pure Rust
//!   with constant-time digest comparison (`subtle::ConstantTimeEq`).
//! - Fail-closed transport & integrity validation: enforces HTTPS on non-loopback hosts,
//!   allowlists GitHub release/asset hosts (`github.com`, `objects.githubusercontent.com`,
//!   `release-assets.githubusercontent.com`) across redirects, enforces size caps, rejects
//!   unsafe asset filenames, and refuses to commit or launch any installer without a matching
//!   `SHA256SUMS.txt` digest.

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::{Url, redirect};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::time::Duration;
use subtle::ConstantTimeEq;

/// Canonical GitHub repository slug (`owner/repo`) for official Genshin-Helper releases.
pub const DEFAULT_GITHUB_REPO: &str = "tuanminh26596/Genshin-helper";

/// Default GitHub REST API base URL.
pub const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";

/// Default HTTP timeout (in seconds) for update metadata checks.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Default HTTP timeout (in seconds) for streaming installer downloads.
pub const DEFAULT_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Maximum response size accepted for the GitHub release JSON payload (1 MiB).
pub const MAX_RELEASE_JSON_BYTES: usize = 1024 * 1024;

/// Maximum response size accepted for `SHA256SUMS.txt` (256 KiB).
pub const MAX_SHA256SUMS_BYTES: usize = 256 * 1024;

/// Maximum installer download size accepted (128 MiB safety ceiling).
pub const MAX_INSTALLER_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum character length preserved for release notes shown in UI.
pub const MAX_RELEASE_NOTES_CHARS: usize = 3500;

const HEADER_GITHUB_API_VERSION: HeaderName = HeaderName::from_static("x-github-api-version");
const INSTALLER_ASSET_PREFIX: &str = "GenshinHelper-Setup-";
const INSTALLER_ASSET_SUFFIX: &str = ".exe";
const CHECKSUM_ASSET_NAME: &str = "SHA256SUMS.txt";
const FILE_HASH_BUFFER_BYTES: usize = 64 * 1024;

static DOWNLOAD_PART_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Pure-Rust FIPS 180-4 SHA-256 (re-exported from gh-core)
// ---------------------------------------------------------------------------

pub use gh_core::sha256::{Sha256, sha256_bytes, sha256_file, sha256_hex};

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// SHA256SUMS.txt parsing & file verification
// ---------------------------------------------------------------------------

/// Parses standard GNU `sha256sum` manifests into a `filename -> lowercase_sha256` map.
///
/// Supports both `<64-hex>  <filename>` (text mode) and `<64-hex> *<filename>` (binary mode),
/// strips an optional UTF-8 BOM, ignores blank or `#` comment lines, normalizes digests to
/// lowercase hex, and silently skips lines with malformed hashes or missing filenames.
pub fn parse_sha256sums(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let clean = content.strip_prefix('\u{feff}').unwrap_or(content);

    for raw_line in clean.lines() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed_start = line.trim_start();
        if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
            continue;
        }

        let Some((digest_part, remainder)) = split_leading_sha256_token(trimmed_start) else {
            continue;
        };
        if !is_valid_sha256_hex(digest_part) {
            continue;
        }

        let filename_raw = if let Some(rest) = remainder.strip_prefix("  ") {
            rest
        } else if let Some(rest) = remainder.strip_prefix(" *") {
            rest
        } else if remainder.starts_with(' ') || remainder.starts_with('\t') {
            let trimmed_ws = remainder.trim_start_matches([' ', '\t']);
            trimmed_ws.strip_prefix('*').unwrap_or(trimmed_ws)
        } else {
            continue;
        };

        let filename = filename_raw.trim_end_matches([' ', '\t']);
        if filename.is_empty() {
            continue;
        }

        out.insert(filename.to_owned(), digest_part.to_ascii_lowercase());
    }

    out
}

fn split_leading_sha256_token(s: &str) -> Option<(&str, &str)> {
    let sep_idx = s.find([' ', '\t'])?;
    Some((&s[..sep_idx], &s[sep_idx..]))
}

/// Verifies that the file at `path` matches `expected_hex` (64-char ASCII hex SHA-256).
///
/// Uses constant-time digest comparison and returns `Err` if `expected_hex` is malformed or the
/// file cannot be read.
pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<bool, String> {
    let trimmed = expected_hex.trim();
    if !is_valid_sha256_hex(trimmed) {
        return Err(format!(
            "invalid expected SHA-256 hex digest (expected 64 hex chars, got {})",
            trimmed.len()
        ));
    }
    let expected_lower = trimmed.to_ascii_lowercase();
    let actual_lower =
        sha256_file(path).map_err(|e| format!("failed to hash {}: {e}", path.display()))?;
    let eq = actual_lower
        .as_bytes()
        .ct_eq(expected_lower.as_bytes());
    Ok(bool::from(eq))
}

// ---------------------------------------------------------------------------
// SemVer parsing & comparison
// ---------------------------------------------------------------------------

/// Parses a `MAJOR.MINOR.PATCH` version string (with optional leading `v`/`V`, pre-release,
/// or build metadata suffix) into `(major, minor, patch)`.
pub fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let (major, minor, patch, _) = parse_semver_parts(tag)?;
    Some((major, minor, patch))
}

/// Returns `true` if `latest` represents a strictly newer SemVer version than `current`.
///
/// Handles optional leading `v`/`V`, ignores `+build` metadata, and follows SemVer 2.0.0
/// precedence for pre-release suffixes (e.g. `5.6.0 > 5.6.0-rc.1`). Returns `false` if either
/// version string is not a valid `MAJOR.MINOR.PATCH` version.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    let Some((cur_maj, cur_min, cur_pat, cur_pre)) = parse_semver_parts(current) else {
        return false;
    };
    let Some((lat_maj, lat_min, lat_pat, lat_pre)) = parse_semver_parts(latest) else {
        return false;
    };

    match (lat_maj, lat_min, lat_pat).cmp(&(cur_maj, cur_min, cur_pat)) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (cur_pre, lat_pre) {
            (Some(_), None) => true,
            (None, Some(_)) | (None, None) => false,
            (Some(c), Some(l)) => compare_prerelease(l, c) == Ordering::Greater,
        },
    }
}

fn parse_semver_parts(tag: &str) -> Option<(u64, u64, u64, Option<&str>)> {
    let trimmed = tag.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let without_build = without_v
        .split_once('+')
        .map_or(without_v, |(before, _)| before);
    let (core, pre) = match without_build.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (without_build, None),
    };

    let mut parts = core.split('.');
    let major = parse_decimal_u64(parts.next()?)?;
    let minor = parse_decimal_u64(parts.next()?)?;
    let patch = parse_decimal_u64(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }

    Some((major, minor, patch, pre))
}

fn parse_decimal_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut a_it = a.split('.');
    let mut b_it = b.split('.');
    loop {
        match (a_it.next(), b_it.next()) {
            (Some(ia), Some(ib)) => {
                let a_num = parse_decimal_u128(ia);
                let b_num = parse_decimal_u128(ib);
                let ord = match (a_num, b_num) {
                    (Some(na), Some(nb)) => na.cmp(&nb),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => ia.cmp(ib),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

fn parse_decimal_u128(s: &str) -> Option<u128> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u128>().ok()
}

// ---------------------------------------------------------------------------
// Manual / Opt-In GitHub Releases Update Checker
// ---------------------------------------------------------------------------

/// Metadata for an individual GitHub Release downloadable asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

/// Result of a check against GitHub Releases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub release_url: String,
    #[serde(default)]
    pub release_name: String,
    #[serde(default)]
    pub release_notes: String,
    #[serde(default)]
    pub published_at: String,
    pub installer_asset: Option<ReleaseAsset>,
    pub expected_sha256: Option<String>,
    pub checksum_verified_present: bool,
}

/// Verified installer binary committed on disk after constant-time SHA-256 verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedInstaller {
    pub path: PathBuf,
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub reused_cached: bool,
}

/// Configuration for [`UpdateCheckClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckConfig {
    pub repo: String,
    pub api_base_url: String,
    pub timeout_secs: u64,
    pub user_agent: String,
}

impl Default for UpdateCheckConfig {
    fn default() -> Self {
        Self {
            repo: DEFAULT_GITHUB_REPO.to_owned(),
            api_base_url: DEFAULT_GITHUB_API_BASE.to_owned(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            user_agent: format!("GenshinHelper/{}", gh_core::APP_VERSION),
        }
    }
}

/// GitHub Releases update checker & SHA-256-verified installer downloader client.
///
/// Only performs network I/O when the caller explicitly invokes [`Self::check_latest`] or
/// [`Self::download_verified_installer`]. Never spawns background polling tasks on its own.
#[derive(Debug, Clone, Default)]
pub struct UpdateCheckClient {
    config: UpdateCheckConfig,
}

impl UpdateCheckClient {
    pub fn new(config: UpdateCheckConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &UpdateCheckConfig {
        &self.config
    }

    /// Queries `{api_base_url}/repos/{repo}/releases/latest` once and checks whether a newer
    /// release exists along with its installer asset, release notes, and published `SHA256SUMS.txt` digest.
    pub fn check_latest(&self, current_version: &str) -> Result<UpdateCheckResult, String> {
        let repo = validate_repo_slug(&self.config.repo)?;
        let (api_base, allow_loopback) = validate_api_base_url(&self.config.api_base_url)?;
        let timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let user_agent = self.config.user_agent.trim().to_owned();
        if user_agent.is_empty() {
            return Err("update checker user_agent must not be empty".to_owned());
        }

        let base_trimmed = api_base.as_str().trim_end_matches('/');
        let release_api_url = format!("{base_trimmed}/repos/{repo}/releases/latest");
        let api_host = api_base.host_str().unwrap_or_default().to_ascii_lowercase();

        let release_bytes = http_get_bytes(HttpGetRequest {
            url: &release_api_url,
            user_agent: &user_agent,
            accept: "application/vnd.github+json",
            include_github_api_version: true,
            timeout,
            limit: MAX_RELEASE_JSON_BYTES,
            allow_loopback,
            redirect_scope: RedirectScope::ApiHost(api_host),
        })?;

        let release: GitHubReleaseDto = serde_json::from_slice(&release_bytes)
            .map_err(|e| format!("failed to parse GitHub release JSON: {e}"))?;

        if release.draft {
            return Err("GitHub release payload is marked as draft".to_owned());
        }

        let latest_version = release.tag_name.trim().to_owned();
        if latest_version.is_empty() {
            return Err("GitHub release payload missing tag_name".to_owned());
        }

        let release_name = release
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&latest_version)
            .to_owned();
        let release_notes = release
            .body
            .as_deref()
            .map(sanitize_release_notes)
            .unwrap_or_default();
        let published_at = release
            .published_at
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned();

        let release_url = if release.html_url.trim().is_empty() {
            format!("https://github.com/{repo}/releases")
        } else {
            let trimmed_url = release.html_url.trim();
            if !is_allowed_release_page_url(trimmed_url, allow_loopback) {
                return Err(format!("untrusted GitHub release URL: {trimmed_url}"));
            }
            trimmed_url.to_owned()
        };

        let mut installer_asset: Option<ReleaseAsset> = None;
        let mut checksum_asset_url: Option<String> = None;

        for asset in release.assets {
            let name = asset.name.trim();
            let dl_url = asset.browser_download_url.trim();

            if installer_asset.is_none() && is_safe_installer_filename(name) {
                if !is_allowed_asset_url(dl_url, allow_loopback) {
                    return Err(format!("untrusted installer download URL: {dl_url}"));
                }
                installer_asset = Some(ReleaseAsset {
                    name: name.to_owned(),
                    download_url: dl_url.to_owned(),
                    size: asset.size,
                });
            } else if checksum_asset_url.is_none() && name == CHECKSUM_ASSET_NAME {
                if !is_allowed_asset_url(dl_url, allow_loopback) {
                    return Err(format!("untrusted checksum download URL: {dl_url}"));
                }
                checksum_asset_url = Some(dl_url.to_owned());
            }
        }

        let expected_sha256 = if let (Some(installer), Some(sums_url)) =
            (installer_asset.as_ref(), checksum_asset_url.as_deref())
        {
            let sums_bytes = http_get_bytes(HttpGetRequest {
                url: sums_url,
                user_agent: &user_agent,
                accept: "text/plain, application/octet-stream",
                include_github_api_version: false,
                timeout,
                limit: MAX_SHA256SUMS_BYTES,
                allow_loopback,
                redirect_scope: RedirectScope::ReleaseAsset,
            })?;
            let sums_text = String::from_utf8(sums_bytes)
                .map_err(|e| format!("SHA256SUMS.txt is not valid UTF-8: {e}"))?;
            let parsed = parse_sha256sums(&sums_text);
            lookup_asset_checksum(&parsed, &installer.name)
        } else {
            None
        };

        let checksum_verified_present = expected_sha256.is_some();
        let has_update = is_newer_version(current_version, &latest_version);

        Ok(UpdateCheckResult {
            current_version: current_version.trim().to_owned(),
            latest_version,
            has_update,
            release_url,
            release_name,
            release_notes,
            published_at,
            installer_asset,
            expected_sha256,
            checksum_verified_present,
        })
    }

    /// Downloads `installer` into `dest_dir` with strict allowlist enforcement, streaming
    /// progress reporting, optional cancellation, and mandatory constant-time SHA-256 verification.
    ///
    /// Fails closed if `expected_sha256` is missing/malformed, if `installer.name` fails path
    /// traversal checks, if any redirect leaves the GitHub asset host allowlist, if the payload
    /// exceeds [`MAX_INSTALLER_BYTES`], or if the final digest does not match `expected_sha256`.
    /// Never leaves an unverified `.exe` or `.part` file on disk.
    pub fn download_verified_installer<F>(
        &self,
        installer: &ReleaseAsset,
        expected_sha256: &str,
        dest_dir: &Path,
        cancel: Option<Arc<AtomicBool>>,
        mut on_progress: F,
    ) -> Result<VerifiedInstaller, String>
    where
        F: FnMut(u64, u64) + Send + 'static,
    {
        let expected_trimmed = expected_sha256.trim();
        if !is_valid_sha256_hex(expected_trimmed) {
            return Err("missing or invalid expected SHA-256 digest (fail-closed)".to_owned());
        }
        let expected_lower = expected_trimmed.to_ascii_lowercase();

        let filename = installer.name.trim();
        if !is_safe_installer_filename(filename) {
            return Err(format!("unsafe installer filename rejected: {filename}"));
        }
        if installer.size > MAX_INSTALLER_BYTES {
            return Err(format!(
                "installer asset exceeds {MAX_INSTALLER_BYTES}-byte safety limit ({} bytes)",
                installer.size
            ));
        }

        let (_, allow_loopback) = validate_api_base_url(&self.config.api_base_url)?;
        let dl_url = installer.download_url.trim();
        if !is_allowed_asset_url(dl_url, allow_loopback) {
            return Err(format!("untrusted installer download URL: {dl_url}"));
        }

        let user_agent = self.config.user_agent.trim().to_owned();
        if user_agent.is_empty() {
            return Err("update checker user_agent must not be empty".to_owned());
        }

        std::fs::create_dir_all(dest_dir)
            .map_err(|e| format!("failed to create updates directory {}: {e}", dest_dir.display()))?;

        let final_path = dest_dir.join(filename);
        if final_path.is_file()
            && verify_file_sha256(&final_path, &expected_lower).unwrap_or(false)
        {
            let size_bytes = std::fs::metadata(&final_path)
                .map(|m| m.len())
                .unwrap_or(installer.size);
            on_progress(size_bytes, size_bytes);
            return Ok(VerifiedInstaller {
                path: final_path,
                filename: filename.to_owned(),
                size_bytes,
                sha256: expected_lower,
                reused_cached: true,
            });
        }

        let connect_timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let dl_timeout = Duration::from_secs(DEFAULT_DOWNLOAD_TIMEOUT_SECS);
        let url_owned = dl_url.to_owned();
        let dest_dir_owned = dest_dir.to_path_buf();
        let filename_owned = filename.to_owned();
        let hint_size = installer.size;

        run_blocking_result(move || {
            if cancel
                .as_ref()
                .is_some_and(|c| c.load(AtomicOrdering::Acquire))
            {
                return Err("download cancelled".to_owned());
            }

            let mut headers = HeaderMap::new();
            let ua_val = HeaderValue::from_str(&user_agent)
                .map_err(|e| format!("invalid user_agent header: {e}"))?;
            headers.insert(USER_AGENT, ua_val);
            headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));

            let policy = redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 8 {
                    return attempt.error("too many redirects");
                }
                let target = attempt.url();
                if is_allowed_asset_url(target.as_str(), allow_loopback) {
                    attempt.follow()
                } else {
                    attempt.error("redirect target blocked by host/scheme allowlist")
                }
            });

            let mut builder = reqwest::blocking::Client::builder()
                .connect_timeout(connect_timeout)
                .timeout(dl_timeout)
                .redirect(policy);
            if allow_loopback {
                builder = builder.no_proxy();
            }
            let client = builder
                .build()
                .map_err(|e| format!("failed to build download HTTP client: {e}"))?;

            let mut resp = client
                .get(&url_owned)
                .headers(headers)
                .send()
                .map_err(|e| format!("installer download request failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("installer download HTTP status {}", resp.status()));
            }

            let content_len = resp.content_length();
            if let Some(cl) = content_len
                && cl > MAX_INSTALLER_BYTES
            {
                return Err(format!(
                    "installer Content-Length ({cl} bytes) exceeds {MAX_INSTALLER_BYTES}-byte safety limit"
                ));
            }
            let total_expected = content_len.unwrap_or(hint_size);

            let seq = DOWNLOAD_PART_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let part_name = format!(".{filename_owned}.part.{}.{seq}", std::process::id());
            let part_path = dest_dir_owned.join(part_name);
            let mut guard = PartFileGuard::new(part_path.clone());

            let mut downloaded: u64 = 0;
            let mut hasher = Sha256::new();
            {
                let mut out_file = File::create(&part_path)
                    .map_err(|e| format!("failed to create temp installer {}: {e}", part_path.display()))?;
                let mut buf = [0u8; FILE_HASH_BUFFER_BYTES];
                on_progress(0, total_expected);

                loop {
                    if cancel
                        .as_ref()
                        .is_some_and(|c| c.load(AtomicOrdering::Acquire))
                    {
                        return Err("download cancelled".to_owned());
                    }
                    let n = match resp.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => return Err(format!("installer download read failed: {err}")),
                    };
                    downloaded = downloaded.saturating_add(n as u64);
                    if downloaded > MAX_INSTALLER_BYTES {
                        return Err(format!(
                            "installer stream exceeded {MAX_INSTALLER_BYTES}-byte safety limit"
                        ));
                    }
                    out_file
                        .write_all(&buf[..n])
                        .map_err(|e| format!("failed to write installer chunk: {e}"))?;
                    hasher.update(&buf[..n]);
                    on_progress(downloaded, total_expected.max(downloaded));
                }

                out_file
                    .flush()
                    .map_err(|e| format!("failed to flush downloaded installer: {e}"))?;
                out_file
                    .sync_all()
                    .map_err(|e| format!("failed to sync downloaded installer: {e}"))?;
            }

            if downloaded == 0 {
                return Err("downloaded installer file is empty".to_owned());
            }

            let stream_hex = hasher.finalize_hex();
            if !bool::from(stream_hex.as_bytes().ct_eq(expected_lower.as_bytes())) {
                return Err(format!(
                    "SHA-256 verification failed (expected {expected_lower}, got {stream_hex})"
                ));
            }

            // Defense-in-depth: re-hash flushed bytes on disk before atomic promotion.
            match verify_file_sha256(&part_path, &expected_lower) {
                Ok(true) => {}
                Ok(false) => {
                    return Err("on-disk SHA-256 verification failed after download".to_owned());
                }
                Err(e) => return Err(e),
            }

            let final_path = dest_dir_owned.join(&filename_owned);
            if final_path.exists() {
                let _ = std::fs::remove_file(&final_path);
            }
            std::fs::rename(&part_path, &final_path).map_err(|e| {
                format!(
                    "failed to commit verified installer to {}: {e}",
                    final_path.display()
                )
            })?;
            guard.disarm();

            Ok(VerifiedInstaller {
                path: final_path,
                filename: filename_owned,
                size_bytes: downloaded,
                sha256: expected_lower,
                reused_cached: false,
            })
        })
    }
}

/// Removes stale `GenshinHelper-Setup-*.exe` and leftover `*.part.*` files under `dest_dir`,
/// optionally keeping `keep_filename`. Returns the number of files deleted.
pub fn cleanup_stale_update_files(dest_dir: &Path, keep_filename: Option<&str>) -> usize {
    let Ok(entries) = std::fs::read_dir(dest_dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if keep_filename.is_some_and(|keep| name.eq_ignore_ascii_case(keep)) {
            continue;
        }
        let is_part = name.starts_with('.') && name.contains(".part.");
        let is_old_setup = is_safe_installer_filename(name);
        if (is_part || is_old_setup) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

struct PartFileGuard {
    path: PathBuf,
    armed: bool,
}

impl PartFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseDto {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GitHubAssetDto>,
}

#[derive(Debug, Deserialize)]
struct GitHubAssetDto {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone)]
enum RedirectScope {
    ApiHost(String),
    ReleaseAsset,
}

fn sanitize_release_notes(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim();
    if trimmed.chars().count() <= MAX_RELEASE_NOTES_CHARS {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(MAX_RELEASE_NOTES_CHARS).collect();
    out.push('…');
    out
}

fn is_installer_asset_name(name: &str) -> bool {
    name.starts_with(INSTALLER_ASSET_PREFIX)
        && name.len() > INSTALLER_ASSET_PREFIX.len() + INSTALLER_ASSET_SUFFIX.len()
        && name
            .get(name.len() - INSTALLER_ASSET_SUFFIX.len()..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(INSTALLER_ASSET_SUFFIX))
}

/// Validates that `name` is a flat `GenshinHelper-Setup-*.exe` leaf filename free of path traversal.
pub fn is_safe_installer_filename(name: &str) -> bool {
    is_installer_asset_name(name)
        && !name.contains(['/', '\\', ':', '\0'])
        && !name.contains("..")
        && !name.chars().any(char::is_control)
}

fn lookup_asset_checksum(
    parsed: &BTreeMap<String, String>,
    installer_name: &str,
) -> Option<String> {
    if let Some(exact) = parsed.get(installer_name) {
        return Some(exact.clone());
    }
    parsed
        .iter()
        .find(|(entry_name, _)| {
            entry_name
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|leaf| leaf == installer_name)
        })
        .map(|(_, digest)| digest.clone())
}

fn validate_repo_slug(raw: &str) -> Result<&str, String> {
    let slug = raw.trim();
    let Some((owner, name)) = slug.split_once('/') else {
        return Err(format!("invalid GitHub repository slug: {slug}"));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || owner == "."
        || owner == ".."
        || name == "."
        || name == ".."
        || !is_safe_repo_segment(owner)
        || !is_safe_repo_segment(name)
    {
        return Err(format!("invalid GitHub repository slug: {slug}"));
    }
    Ok(slug)
}

fn is_safe_repo_segment(seg: &str) -> bool {
    seg.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_loopback_host_str(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) || host.eq_ignore_ascii_case("localhost")
}

fn validate_api_base_url(raw: &str) -> Result<(Url, bool), String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("invalid api_base_url: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("api_base_url must not contain embedded credentials".to_owned());
    }
    let Some(host) = url.host_str() else {
        return Err("api_base_url missing host".to_owned());
    };
    let is_loopback = is_loopback_host_str(host);
    match url.scheme() {
        "https" => Ok((url, is_loopback)),
        "http" if is_loopback => Ok((url, true)),
        other => Err(format!(
            "api_base_url must use https for non-loopback hosts (got {other}://{host})"
        )),
    }
}

/// Validates whether `raw` is a trusted GitHub release/asset download URL.
///
/// Accepts HTTPS URLs hosted on `github.com`, `objects.githubusercontent.com`, or
/// `release-assets.githubusercontent.com`. Loopback (`127.0.0.1` / `[::1]` / `localhost`)
/// HTTP/HTTPS URLs are accepted only when `allow_loopback` is `true` (offline mock tests).
pub fn is_allowed_asset_url(raw: &str, allow_loopback: bool) -> bool {
    let Ok(url) = Url::parse(raw.trim()) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if allow_loopback && is_loopback_host_str(host) {
        return matches!(url.scheme(), "http" | "https");
    }
    url.scheme() == "https"
        && (host.eq_ignore_ascii_case("github.com")
            || host.eq_ignore_ascii_case("objects.githubusercontent.com")
            || host.eq_ignore_ascii_case("release-assets.githubusercontent.com"))
}

fn is_allowed_release_page_url(raw: &str, allow_loopback: bool) -> bool {
    let Ok(url) = Url::parse(raw.trim()) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if allow_loopback && is_loopback_host_str(host) {
        return matches!(url.scheme(), "http" | "https");
    }
    url.scheme() == "https" && host.eq_ignore_ascii_case("github.com")
}

struct HttpGetRequest<'a> {
    url: &'a str,
    user_agent: &'a str,
    accept: &'static str,
    include_github_api_version: bool,
    timeout: Duration,
    limit: usize,
    allow_loopback: bool,
    redirect_scope: RedirectScope,
}

fn http_get_bytes(req: HttpGetRequest<'_>) -> Result<Vec<u8>, String> {
    let url_owned = req.url.to_owned();
    let ua_owned = req.user_agent.to_owned();
    let accept = req.accept;
    let include_github_api_version = req.include_github_api_version;
    let timeout = req.timeout;
    let limit = req.limit;
    let allow_loopback = req.allow_loopback;
    let redirect_scope = req.redirect_scope;

    run_blocking_result(move || {
        let mut headers = HeaderMap::new();
        let ua_val = HeaderValue::from_str(&ua_owned)
            .map_err(|e| format!("invalid user_agent header: {e}"))?;
        headers.insert(USER_AGENT, ua_val);
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        if include_github_api_version {
            headers.insert(
                HEADER_GITHUB_API_VERSION,
                HeaderValue::from_static("2022-11-28"),
            );
        }

        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            let target = attempt.url();
            let allowed = match &redirect_scope {
                RedirectScope::ReleaseAsset => is_allowed_asset_url(target.as_str(), allow_loopback),
                RedirectScope::ApiHost(expected_host) => {
                    let Some(host) = target.host_str() else {
                        return attempt.error("redirect target missing host");
                    };
                    if !target.username().is_empty() || target.password().is_some() {
                        false
                    } else if allow_loopback && is_loopback_host_str(host) {
                        matches!(target.scheme(), "http" | "https")
                    } else {
                        target.scheme() == "https"
                            && (host.eq_ignore_ascii_case(expected_host)
                                || host.eq_ignore_ascii_case("api.github.com"))
                    }
                }
            };
            if allowed {
                attempt.follow()
            } else {
                attempt.error("redirect target blocked by host/scheme allowlist")
            }
        });

        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .redirect(policy);
        if allow_loopback {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;

        let resp = client
            .get(&url_owned)
            .headers(headers)
            .send()
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP status {}", resp.status()));
        }

        let mut buf = Vec::new();
        let mut limited = resp.take((limit + 1) as u64);
        limited
            .read_to_end(&mut buf)
            .map_err(|e| format!("failed to read HTTP body: {e}"))?;
        if buf.len() > limit {
            return Err(format!("HTTP response exceeded {limit}-byte safety limit"));
        }
        Ok(buf)
    })
}

fn run_blocking_result<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        let handle = std::thread::Builder::new()
            .name("gh-update-check".to_owned())
            .spawn(f)
            .map_err(|e| format!("failed to spawn update-check thread: {e}"))?;
        handle
            .join()
            .map_err(|_| "update-check thread panicked".to_owned())?
    } else {
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_file_path(label: &str) -> std::path::PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh_net_update_check_{label}_{}_{seq}.tmp",
            std::process::id()
        ))
    }

    fn spawn_mock_http_server(
        routes: Vec<(String, u16, &'static str, String)>,
        expected_requests: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock server");
        let addr = listener.local_addr().expect("mock server local_addr");
        let base_url = format!("http://{addr}");

        let handle = std::thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept mock connection");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                let mut req_buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            req_buf.extend_from_slice(&chunk[..n]);
                            if req_buf.windows(4).any(|w| w == b"\r\n\r\n") || req_buf.len() > 16384
                            {
                                break;
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }

                let req_str = String::from_utf8_lossy(&req_buf);
                let path = req_str
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let (status, content_type, body) = routes
                    .iter()
                    .find(|(route_path, _, _, _)| route_path == path)
                    .map(|(_, st, ct, b)| (*st, *ct, b.as_str()))
                    .unwrap_or((404, "text/plain", "not found"));

                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        (base_url, handle)
    }

    #[test]
    fn sha256_nist_fips_180_4_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            ),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );

        // Verify streaming chunked updates match single-shot across block boundaries (55..66, 130).
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 128, 130] {
            let payload: Vec<u8> = (0..len).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
            let direct = sha256_bytes(&payload);

            let mut streamed = Sha256::new();
            for chunk in payload.chunks(7) {
                streamed.update(chunk);
            }
            assert_eq!(streamed.finalize(), direct, "chunked mismatch at len={len}");
        }
    }

    #[test]
    fn parse_sha256sums_supports_text_binary_bom_and_rejects_invalid() {
        let h1 = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        let h2 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let content = format!(
            "\u{feff}# Release checksums\r\n\
             \r\n\
             {h1}  GenshinHelper-Setup-v5.7.0.exe\r\n\
             {h2} *Genshin Helper Portable.zip\n\
             deadbeef  too-short.exe\n\
             zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  non-hex.exe\n\
             {h2}no-separator.exe\n\
             {h2}  \n"
        );

        let parsed = parse_sha256sums(&content);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get("GenshinHelper-Setup-v5.7.0.exe").map(String::as_str),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            parsed.get("Genshin Helper Portable.zip").map(String::as_str),
            Some(h2)
        );
    }

    #[test]
    fn verify_file_sha256_matches_mismatches_and_validates_hex() {
        let path = temp_file_path("verify");
        std::fs::write(&path, b"abc").expect("write temp file");

        let abc_lower = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let abc_upper = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        let wrong_hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        assert_eq!(sha256_file(&path).unwrap(), abc_lower);
        assert_eq!(verify_file_sha256(&path, abc_lower), Ok(true));
        assert_eq!(verify_file_sha256(&path, abc_upper), Ok(true));
        assert_eq!(verify_file_sha256(&path, wrong_hex), Ok(false));
        assert!(verify_file_sha256(&path, "not-a-valid-sha256").is_err());

        let _ = std::fs::remove_file(&path);
        assert!(verify_file_sha256(&path, abc_lower).is_err());
    }

    #[test]
    fn parse_semver_and_is_newer_version_behavior() {
        assert_eq!(parse_semver("v5.6.0"), Some((5, 6, 0)));
        assert_eq!(parse_semver("5.6.0"), Some((5, 6, 0)));
        assert_eq!(parse_semver("V5.6.1-rc.1+build.42"), Some((5, 6, 1)));
        assert_eq!(parse_semver("5.6"), None);
        assert_eq!(parse_semver("5.6.0.1"), None);
        assert_eq!(parse_semver("v5.6.0-"), None);
        assert_eq!(parse_semver("not-semver"), None);

        assert!(is_newer_version("5.6.0", "v5.6.1"));
        assert!(is_newer_version("v5.6.0", "5.7.0"));
        assert!(is_newer_version("5.6.0", "v6.0.0"));
        assert!(is_newer_version("5.6.0-rc.1", "v5.6.0"));
        assert!(is_newer_version("5.6.0-beta.2", "5.6.0-beta.10"));

        assert!(!is_newer_version("5.6.0", "v5.6.0"));
        assert!(!is_newer_version("5.7.0", "v5.6.9"));
        assert!(!is_newer_version("5.6.0", "v5.6.0-rc.1"));
        assert!(!is_newer_version("5.6.0-beta.10", "5.6.0-beta.2"));
        assert!(!is_newer_version("invalid", "5.7.0"));
        assert!(!is_newer_version("5.6.0", "invalid"));
    }

    #[test]
    fn url_allowlist_enforces_https_and_github_hosts() {
        assert!(is_allowed_asset_url(
            "https://github.com/tuanminh26596/Genshin-helper/releases/download/v5.7.0/GenshinHelper-Setup-v5.7.0.exe",
            false
        ));
        assert!(is_allowed_asset_url(
            "https://objects.githubusercontent.com/github-production-release-asset-123/asset.exe",
            false
        ));
        assert!(is_allowed_asset_url(
            "https://release-assets.githubusercontent.com/github-production-release-asset-123/asset.exe",
            false
        ));

        assert!(!is_allowed_asset_url(
            "http://github.com/tuanminh26596/Genshin-helper/releases/download/v5.7.0/asset.exe",
            false
        ));
        assert!(!is_allowed_asset_url(
            "https://github.com.evil.example/asset.exe",
            false
        ));
        assert!(!is_allowed_asset_url(
            "https://user:pass@github.com/asset.exe",
            false
        ));
        assert!(!is_allowed_asset_url(
            "http://127.0.0.1:8765/GenshinHelper-Setup-v5.7.0.exe",
            false
        ));
        assert!(is_allowed_asset_url(
            "http://127.0.0.1:8765/GenshinHelper-Setup-v5.7.0.exe",
            true
        ));
    }

    #[test]
    fn check_latest_with_loopback_mock_server_extracts_installer_and_sha256() {
        let expected_hash = "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1";
        let setup_name = "GenshinHelper-Setup-v5.7.0.exe";
        let (base_url, handle) = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock server");
            let addr = listener.local_addr().expect("mock server local_addr");
            let base = format!("http://{addr}");
            let setup_dl = format!("{base}/download/{setup_name}");
            let sums_dl = format!("{base}/download/SHA256SUMS.txt");
            let release_json = serde_json::json!({
                "tag_name": "v5.7.0",
                "html_url": "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.7.0",
                "assets": [
                    {
                        "name": setup_name,
                        "browser_download_url": setup_dl,
                        "size": 7_450_123u64
                    },
                    {
                        "name": "SHA256SUMS.txt",
                        "browser_download_url": sums_dl,
                        "size": 100u64
                    }
                ]
            })
            .to_string();
            let sums_body = format!("{expected_hash}  {setup_name}\r\n");

            let h = std::thread::spawn(move || {
                for _ in 0..4 {
                    let (mut stream, _) = listener.accept().expect("accept mock connection");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut req_buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                req_buf.extend_from_slice(&chunk[..n]);
                                if req_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let req_str = String::from_utf8_lossy(&req_buf);
                    let path = req_str
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (ct, body) = if path == "/repos/tuanminh26596/Genshin-helper/releases/latest"
                    {
                        ("application/json", release_json.as_str())
                    } else if path == "/download/SHA256SUMS.txt" {
                        ("text/plain", sums_body.as_str())
                    } else {
                        ("text/plain", "")
                    };
                    let status = if body.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            });
            (base, h)
        };

        let client = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: base_url.clone(),
            timeout_secs: 5,
            ..Default::default()
        });

        let res = client
            .check_latest("5.6.0")
            .expect("check_latest should succeed");
        assert_eq!(res.current_version, "5.6.0");
        assert_eq!(res.latest_version, "v5.7.0");
        assert!(res.has_update);
        assert_eq!(
            res.release_url,
            "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.7.0"
        );
        assert_eq!(
            res.installer_asset,
            Some(ReleaseAsset {
                name: setup_name.to_owned(),
                download_url: format!("{base_url}/download/{setup_name}"),
                size: 7_450_123,
            })
        );
        assert_eq!(res.expected_sha256.as_deref(), Some(expected_hash));
        assert!(res.checksum_verified_present);

        // Up-to-date check against the same mock server.
        let up_to_date = client
            .check_latest("v5.7.0")
            .expect("up-to-date check should succeed");
        assert!(!up_to_date.has_update);
        assert!(up_to_date.checksum_verified_present);

        handle.join().expect("mock server thread finished cleanly");
    }

    #[test]
    fn check_latest_handles_missing_sha256sums_and_rejects_untrusted_urls() {
        // 1. Non-HTTPS non-loopback api_base_url rejected before any network call.
        let insecure = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: "http://api.github.com".to_owned(),
            ..Default::default()
        });
        assert!(insecure.check_latest("5.6.0").is_err());

        // 2. Release without SHA256SUMS.txt succeeds with checksum_verified_present = false.
        let no_sums_json = serde_json::json!({
            "tag_name": "v5.7.0",
            "html_url": "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.7.0",
            "assets": [
                {
                    "name": "GenshinHelper-Setup-v5.7.0.exe",
                    "browser_download_url": "https://github.com/tuanminh26596/Genshin-helper/releases/download/v5.7.0/GenshinHelper-Setup-v5.7.0.exe",
                    "size": 12345u64
                }
            ]
        })
        .to_string();

        let (base_ok, handle_ok) = spawn_mock_http_server(
            vec![(
                "/repos/tuanminh26596/Genshin-helper/releases/latest".to_owned(),
                200,
                "application/json",
                no_sums_json,
            )],
            1,
        );
        let ok_client = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: base_ok,
            timeout_secs: 5,
            ..Default::default()
        });
        let res = ok_client
            .check_latest("5.6.0")
            .expect("release without SHA256SUMS.txt should succeed");
        assert!(res.installer_asset.is_some());
        assert_eq!(res.expected_sha256, None);
        assert!(!res.checksum_verified_present);
        handle_ok.join().expect("mock ok server finished");

        // 3. Untrusted installer download host fails closed.
        let evil_json = serde_json::json!({
            "tag_name": "v5.7.0",
            "html_url": "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.7.0",
            "assets": [
                {
                    "name": "GenshinHelper-Setup-v5.7.0.exe",
                    "browser_download_url": "https://evil.example.com/GenshinHelper-Setup-v5.7.0.exe",
                    "size": 12345u64
                }
            ]
        })
        .to_string();
        let (base_evil, handle_evil) = spawn_mock_http_server(
            vec![(
                "/repos/tuanminh26596/Genshin-helper/releases/latest".to_owned(),
                200,
                "application/json",
                evil_json,
            )],
            1,
        );
        let evil_client = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: base_evil,
            timeout_secs: 5,
            ..Default::default()
        });
        let err = evil_client
            .check_latest("5.6.0")
            .expect_err("untrusted asset URL must be rejected");
        assert!(err.contains("untrusted installer download URL"));
        handle_evil.join().expect("mock evil server finished");
    }

    #[test]
    fn check_latest_extracts_release_notes_and_published_at() {
        let setup_name = "GenshinHelper-Setup-v5.8.0.exe";
        let release_json = serde_json::json!({
            "tag_name": "v5.8.0",
            "name": "Genshin-Helper v5.8.0 Public",
            "body": "## Highlights\r\n- 1-Click verified updater\r\n- Faster tracking\r\n",
            "published_at": "2026-11-26T00:00:00Z",
            "html_url": "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.8.0",
            "assets": [
                {
                    "name": setup_name,
                    "browser_download_url": format!("https://github.com/tuanminh26596/Genshin-helper/releases/download/v5.8.0/{setup_name}"),
                    "size": 1024u64
                }
            ]
        })
        .to_string();

        let (base_url, handle) = spawn_mock_http_server(
            vec![(
                "/repos/tuanminh26596/Genshin-helper/releases/latest".to_owned(),
                200,
                "application/json",
                release_json,
            )],
            1,
        );
        let client = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: base_url,
            timeout_secs: 5,
            ..Default::default()
        });
        let res = client.check_latest("5.7.0").expect("check_latest ok");
        assert!(res.has_update);
        assert_eq!(res.release_name, "Genshin-Helper v5.8.0 Public");
        assert_eq!(
            res.release_notes,
            "## Highlights\n- 1-Click verified updater\n- Faster tracking"
        );
        assert_eq!(res.published_at, "2026-11-26T00:00:00Z");
        handle.join().expect("mock server done");
    }

    #[test]
    fn download_verified_installer_follows_redirect_verifies_sha256_caches_and_rejects_tamper() {
        use std::sync::Mutex;

        let payload: Vec<u8> = (0..131_072usize)
            .map(|i| ((i.wrapping_mul(29) ^ (i >> 3)) & 0xff) as u8)
            .collect();
        let good_hash = sha256_hex(&payload);
        let wrong_hash = "0".repeat(64);
        let setup_name = "GenshinHelper-Setup-v5.8.0.exe";

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock dl server");
        let addr = listener.local_addr().expect("mock dl local_addr");
        let base = format!("http://{addr}");
        let payload_srv = payload.clone();

        // Expect 4 HTTP requests:
        // 1. /redirect -> 302 Location: /cdn/setup.exe
        // 2. /cdn/setup.exe -> 200 payload (verified OK)
        // 3. /cdn/setup.exe -> 200 payload (mismatch test)
        // 4. /evil-redirect -> 302 Location: https://evil.example.com/a.exe (blocked)
        let srv = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept dl conn");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut req_buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            req_buf.extend_from_slice(&chunk[..n]);
                            if req_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let req_str = String::from_utf8_lossy(&req_buf);
                let path = req_str
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                if path == "/redirect" {
                    let resp = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/cdn/setup.exe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else if path == "/evil-redirect" {
                    let resp = "HTTP/1.1 302 Found\r\nLocation: https://evil.example.com/GenshinHelper-Setup-v5.8.0.exe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes());
                } else if path == "/cdn/setup.exe" {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload_srv.len()
                    );
                    let _ = stream.write_all(hdr.as_bytes());
                    let _ = stream.write_all(&payload_srv);
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                let _ = stream.flush();
            }
        });

        let dest_dir = temp_file_path("dl_dir");
        let _ = std::fs::remove_dir_all(&dest_dir);
        let client = UpdateCheckClient::new(UpdateCheckConfig {
            api_base_url: base.clone(),
            timeout_secs: 5,
            ..Default::default()
        });
        let asset = ReleaseAsset {
            name: setup_name.to_owned(),
            download_url: format!("{base}/redirect"),
            size: payload.len() as u64,
        };

        // 1. Happy path across 302 redirect with progress ticks
        let progress_log = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let progress_cb = Arc::clone(&progress_log);
        let verified = client
            .download_verified_installer(&asset, &good_hash, &dest_dir, None, move |dl, tot| {
                progress_cb.lock().unwrap().push((dl, tot));
            })
            .expect("verified download should succeed");
        assert!(!verified.reused_cached);
        assert_eq!(verified.size_bytes, payload.len() as u64);
        assert_eq!(verified.sha256, good_hash);
        assert!(verified.path.is_file());
        let ticks = progress_log.lock().unwrap();
        assert!(ticks.len() >= 2);
        assert_eq!(ticks.last().unwrap().0, payload.len() as u64);
        drop(ticks);

        // 2. Cache hit skips network completely when verified file already exists
        let cached = client
            .download_verified_installer(&asset, &good_hash, &dest_dir, None, |_, _| {})
            .expect("cached verified file should reuse cleanly");
        assert!(cached.reused_cached);
        assert_eq!(cached.path, verified.path);

        // 3. Clean file, test SHA-256 mismatch leaves zero installer or .part files
        let _ = std::fs::remove_file(&verified.path);
        let direct_asset = ReleaseAsset {
            name: setup_name.to_owned(),
            download_url: format!("{base}/cdn/setup.exe"),
            size: payload.len() as u64,
        };
        let err = client
            .download_verified_installer(&direct_asset, &wrong_hash, &dest_dir, None, |_, _| {})
            .expect_err("wrong sha256 must fail closed");
        assert!(err.contains("SHA-256 verification failed"));
        assert!(!dest_dir.join(setup_name).exists());
        assert_eq!(std::fs::read_dir(&dest_dir).unwrap().count(), 0);

        // 4. Untrusted redirect target blocked fail-closed
        let evil_redir_asset = ReleaseAsset {
            name: setup_name.to_owned(),
            download_url: format!("{base}/evil-redirect"),
            size: payload.len() as u64,
        };
        let redir_err = client
            .download_verified_installer(&evil_redir_asset, &good_hash, &dest_dir, None, |_, _| {})
            .expect_err("untrusted redirect must fail closed");
        assert!(redir_err.contains("download request failed") || redir_err.contains("allowlist"));

        // 5. Pre-cancelled download & unsafe filenames rejected before network
        let cancelled = Arc::new(AtomicBool::new(true));
        let cancel_err = client
            .download_verified_installer(&direct_asset, &good_hash, &dest_dir, Some(cancelled), |_, _| {})
            .expect_err("cancelled flag must abort download");
        assert!(cancel_err.contains("cancelled"));

        let bad_name_asset = ReleaseAsset {
            name: "GenshinHelper-Setup-..\\evil.exe".to_owned(),
            download_url: format!("{base}/cdn/setup.exe"),
            size: 100,
        };
        assert!(
            client
                .download_verified_installer(&bad_name_asset, &good_hash, &dest_dir, None, |_, _| {})
                .is_err()
        );

        // 6. Cleanup removes stale setup + .part files while preserving keep_filename and unrelated files
        std::fs::write(dest_dir.join("GenshinHelper-Setup-v5.6.0.exe"), b"old").unwrap();
        std::fs::write(dest_dir.join("GenshinHelper-Setup-v5.8.0.exe"), b"keep").unwrap();
        std::fs::write(dest_dir.join(".GenshinHelper-Setup-v5.8.0.exe.part.1.0"), b"part").unwrap();
        std::fs::write(dest_dir.join("notes.txt"), b"keep-other").unwrap();
        let removed = cleanup_stale_update_files(&dest_dir, Some("GenshinHelper-Setup-v5.8.0.exe"));
        assert_eq!(removed, 2);
        assert!(!dest_dir.join("GenshinHelper-Setup-v5.6.0.exe").exists());
        assert!(dest_dir.join("GenshinHelper-Setup-v5.8.0.exe").is_file());
        assert!(dest_dir.join("notes.txt").is_file());

        let _ = std::fs::remove_dir_all(&dest_dir);
        srv.join().expect("mock dl server done");
    }
}
