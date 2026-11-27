//! Seamless VS Code / T3 Code-style application updater & HoYoLAB map-dataset coordinator.
//!
//! Design & security invariants:
//! - Default `"auto"` mode: checks GitHub Releases ~3s after launch and every 6 hours while idle,
//!   silently pre-downloads & verifies the installer's SHA-256 digest in background, then offers
//!   instant 1-click **Restart to Update** (`/VERYSILENT`, ~2s, zero UAC when elevated) AND
//!   transparent **Install on Exit** (`/VERYSILENT /AUTOUPDATE=exit`) when the user quits.
//! - Game-safe: NEVER restarts the app while the user is playing Genshin Impact unless they
//!   explicitly click Restart/Install.
//! - Crash-loop & dev-safe: tracks install attempts in `%LOCALAPPDATA%\GenshinHelper\updates\updater_state.json`
//!   (stops automatic install retries after 2 failed attempts for the same version), and disables
//!   install-on-exit in `RuntimeMode::Dev` or `--no-tray` headless test runs.
//! - Fail-closed SHA-256 verification: refuses to download or launch any installer without a
//!   matching 64-char hex digest from `SHA256SUMS.txt`, and re-hashes the committed file on disk
//!   immediately before invoking `ShellExecuteW` (TOCTOU defense).

use crate::state::AppState;
use gh_core::atomic_io::write_json_atomic;
use gh_core::config::VALID_APP_UPDATE_MODES;
use gh_core::i18n::{get_language, t};
use gh_core::paths::RuntimeMode;
use gh_core::{APP_VERSION, CoreError, CoreResult};
use gh_hud::toast::ToastKind;
use gh_net::hoyolab_sync::{
    HoyolabSyncClient, StartupUpdateOutcome, check_and_apply_startup_update,
};
use gh_net::update_check::{
    DEFAULT_GITHUB_REPO, UpdateCheckClient, UpdateCheckConfig, UpdateCheckResult,
    VerifiedInstaller, cleanup_stale_update_files, is_newer_version, is_safe_installer_filename,
    verify_file_sha256,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

/// Maximum automatic install retries per target version before requiring manual action.
pub const MAX_AUTO_INSTALL_ATTEMPTS: u32 = 2;

/// Background periodic re-check interval while the app remains running in tray (6 hours).
pub const PERIODIC_UPDATE_CHECK_SECS: u64 = 6 * 3600;

const UPDATER_STATE_FILENAME: &str = "updater_state.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Verifying,
    ReadyToInstall,
    Installing,
    Error,
}

impl UpdatePhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Checking => "checking",
            Self::UpToDate => "up_to_date",
            Self::Available => "available",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::ReadyToInstall => "ready_to_install",
            Self::Installing => "installing",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTrigger {
    /// User clicked Restart/Install Now: silent install + automatic elevated app relaunch.
    RestartNow,
    /// App is shutting down normally: silent install in background without relaunching.
    OnExit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedUpdaterState {
    #[serde(default)]
    last_launched_version: String,
    #[serde(default)]
    pending_install_version: Option<String>,
    #[serde(default)]
    failed_attempts: BTreeMap<String, u32>,
}

impl PersistedUpdaterState {
    fn path_in(updates_dir: &Path) -> PathBuf {
        updates_dir.join(UPDATER_STATE_FILENAME)
    }

    fn load_from(updates_dir: &Path) -> Self {
        let p = Self::path_in(updates_dir);
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&raw).unwrap_or_default()
    }

    fn save_to(&self, updates_dir: &Path) {
        let _ = std::fs::create_dir_all(updates_dir);
        let _ = write_json_atomic(&Self::path_in(updates_dir), self);
    }
}

#[derive(Debug, Clone, Default)]
pub struct MapDataUpdateState {
    pub phase: String,
    pub updated_maps: Vec<i32>,
    pub new_map_ids: Vec<i32>,
    pub teyvat_changed: bool,
    pub restart_recommended: bool,
    pub last_error: Option<String>,
    pub checked_at: Option<u64>,
}

impl MapDataUpdateState {
    fn new() -> Self {
        Self {
            phase: "idle".to_owned(),
            ..Default::default()
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "phase": self.phase,
            "updated_maps": self.updated_maps,
            "new_map_ids": self.new_map_ids,
            "teyvat_changed": self.teyvat_changed,
            "restart_recommended": self.restart_recommended,
            "error": self.last_error,
            "checked_at": self.checked_at,
        })
    }
}

#[derive(Debug, Clone)]
struct UpdaterInner {
    phase: UpdatePhase,
    latest_check: Option<UpdateCheckResult>,
    downloaded_bytes: u64,
    total_bytes: u64,
    verified_installer: Option<VerifiedInstaller>,
    last_error: Option<String>,
    checked_at: Option<u64>,
    just_updated_from: Option<String>,
    notified_ready_version: Option<String>,
    failed_attempts: BTreeMap<String, u32>,
    map_data: MapDataUpdateState,
}

impl Default for UpdaterInner {
    fn default() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            latest_check: None,
            downloaded_bytes: 0,
            total_bytes: 0,
            verified_installer: None,
            last_error: None,
            checked_at: None,
            just_updated_from: None,
            notified_ready_version: None,
            failed_attempts: BTreeMap::new(),
            map_data: MapDataUpdateState::new(),
        }
    }
}

pub struct AppUpdater {
    inner: Mutex<UpdaterInner>,
    download_cancel: Arc<AtomicBool>,
    allow_install_on_exit: AtomicBool,
    client_config: UpdateCheckConfig,
    current_version: String,
}

impl Default for AppUpdater {
    fn default() -> Self {
        Self::new_from_env()
    }
}

impl AppUpdater {
    pub fn new_from_env() -> Self {
        let mut cfg = UpdateCheckConfig::default();
        if let Ok(base) = std::env::var("GH_UPDATE_API_BASE")
            && !base.trim().is_empty()
        {
            cfg.api_base_url = base.trim().to_owned();
        }
        if let Ok(repo) = std::env::var("GH_UPDATE_REPO")
            && !repo.trim().is_empty()
        {
            cfg.repo = repo.trim().to_owned();
        }
        Self::with_config(cfg, APP_VERSION.to_owned())
    }

    pub fn with_config(client_config: UpdateCheckConfig, current_version: String) -> Self {
        Self {
            inner: Mutex::new(UpdaterInner::default()),
            download_cancel: Arc::new(AtomicBool::new(false)),
            allow_install_on_exit: AtomicBool::new(false),
            client_config,
            current_version,
        }
    }

    /// Enables transparent install-on-exit (only enabled in real Installed tray sessions,
    /// never in Dev mode or `--no-tray` automated tests).
    pub fn set_allow_install_on_exit(&self, allow: bool) {
        self.allow_install_on_exit.store(allow, Ordering::Release);
    }

    pub fn allows_install_on_exit(&self) -> bool {
        self.allow_install_on_exit.load(Ordering::Acquire)
    }

    pub fn client(&self) -> UpdateCheckClient {
        UpdateCheckClient::new(self.client_config.clone())
    }

    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    pub fn phase(&self) -> UpdatePhase {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).phase
    }

    pub fn cancel_download(&self) {
        self.download_cancel.store(true, Ordering::Release);
    }

    /// Reconciles persisted updater state on startup:
    /// - Detects successful upgrade (`last_launched_version < current_version`), cleans old setup
    ///   files, and returns `Some(previous_version)` for the post-update celebration toast.
    /// - Detects failed/aborted previous install attempt (`pending_install_version > current_version`)
    ///   and increments its failure counter to guarantee zero crash-loops.
    pub fn reconcile_startup_state(&self, updates_dir: &Path) -> Option<String> {
        let mut persisted = PersistedUpdaterState::load_from(updates_dir);
        let cur = self.current_version.trim_start_matches(['v', 'V']).to_owned();
        let mut upgraded_from: Option<String> = None;

        if !persisted.last_launched_version.is_empty()
            && is_newer_version(&persisted.last_launched_version, &cur)
        {
            upgraded_from = Some(
                persisted
                    .last_launched_version
                    .trim_start_matches(['v', 'V'])
                    .to_owned(),
            );
            persisted.pending_install_version = None;
            persisted.failed_attempts.remove(&cur);
            persisted.last_launched_version = cur.clone();
            persisted.save_to(updates_dir);
            let _ = cleanup_stale_update_files(updates_dir, None);
        } else {
            let mut dirty = false;
            if let Some(pending) = persisted.pending_install_version.take() {
                let pend_clean = pending.trim_start_matches(['v', 'V']).to_owned();
                if is_newer_version(&cur, &pend_clean) {
                    let count = persisted.failed_attempts.entry(pend_clean).or_insert(0);
                    *count = count.saturating_add(1);
                }
                dirty = true;
            }
            if persisted.last_launched_version != cur {
                persisted.last_launched_version = cur.clone();
                dirty = true;
            }
            if dirty {
                persisted.save_to(updates_dir);
            }
            cleanup_non_newer_installers(updates_dir, &cur);
        }

        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.just_updated_from = upgraded_from.clone();
            g.failed_attempts = persisted.failed_attempts;
        }
        upgraded_from
    }

    fn record_install_attempt_on_disk(&self, updates_dir: &Path, target_version: &str) {
        let mut persisted = PersistedUpdaterState::load_from(updates_dir);
        let cur = self.current_version.trim_start_matches(['v', 'V']).to_owned();
        if persisted.last_launched_version.is_empty() {
            persisted.last_launched_version = cur;
        }
        persisted.pending_install_version = Some(
            target_version
                .trim_start_matches(['v', 'V'])
                .to_owned(),
        );
        persisted.save_to(updates_dir);
    }

    fn failed_attempts_for(&self, version: &str) -> u32 {
        let clean = version.trim_start_matches(['v', 'V']);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed_attempts
            .get(clean)
            .copied()
            .unwrap_or(0)
    }

    pub fn record_map_data_outcome(&self, outcome: &StartupUpdateOutcome) {
        let now = unix_now_secs();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.map_data.checked_at = Some(now);
        g.map_data.last_error = None;
        match outcome {
            StartupUpdateOutcome::Disabled => {
                g.map_data.phase = "disabled".to_owned();
                g.map_data.updated_maps.clear();
                g.map_data.new_map_ids.clear();
                g.map_data.teyvat_changed = false;
            }
            StartupUpdateOutcome::UpToDate(_) => {
                g.map_data.phase = "up_to_date".to_owned();
                g.map_data.updated_maps.clear();
                g.map_data.new_map_ids.clear();
                g.map_data.teyvat_changed = false;
            }
            StartupUpdateOutcome::UpdateAvailable(res) => {
                g.map_data.phase = "update_available".to_owned();
                g.map_data.updated_maps = res.updated_maps.keys().copied().collect();
                g.map_data.new_map_ids = res.new_map_ids.clone();
                g.map_data.teyvat_changed = res.teyvat_changed;
            }
            StartupUpdateOutcome::Updated(res) => {
                g.map_data.phase = "updated".to_owned();
                g.map_data.updated_maps = res.updated_maps.keys().copied().collect();
                g.map_data.new_map_ids = res.new_map_ids.clone();
                g.map_data.teyvat_changed = res.teyvat_changed;
                g.map_data.restart_recommended = true;
            }
        }
    }

    pub fn record_map_data_phase(&self, phase: &str, err: Option<String>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.map_data.phase = phase.to_owned();
        g.map_data.last_error = err;
        if phase == "error" {
            g.map_data.checked_at = Some(unix_now_secs());
        }
    }

    /// Lightweight summary included in `GET /status` overrides.
    pub fn status_summary_json(&self, app_update_mode: &str) -> Value {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let has_update = g.latest_check.as_ref().is_some_and(|r| r.has_update);
        let latest_version = g
            .latest_check
            .as_ref()
            .map(|r| r.latest_version.trim_start_matches(['v', 'V']).to_owned());
        let can_one_click = can_one_click_from_check(g.latest_check.as_ref());
        let progress_pct = compute_progress_pct(g.downloaded_bytes, g.total_bytes, g.phase);
        let attempts = latest_version
            .as_deref()
            .and_then(|v| g.failed_attempts.get(v).copied())
            .unwrap_or(0);
        let auto_blocked = attempts >= MAX_AUTO_INSTALL_ATTEMPTS;
        let will_install_on_exit = g.phase == UpdatePhase::ReadyToInstall
            && app_update_mode.eq_ignore_ascii_case("auto")
            && self.allows_install_on_exit()
            && !auto_blocked;

        json!({
            "phase": g.phase.as_str(),
            "has_update": has_update,
            "latest_version": latest_version,
            "can_one_click": can_one_click,
            "verified_ready": g.verified_installer.is_some(),
            "progress_pct": progress_pct,
            "app_update_mode": app_update_mode,
            "auto_check_app_updates": !app_update_mode.eq_ignore_ascii_case("off"),
            "will_install_on_exit": will_install_on_exit,
            "auto_install_blocked": auto_blocked,
            "just_updated_from": g.just_updated_from,
            "map_data_phase": g.map_data.phase,
            "map_restart_recommended": g.map_data.restart_recommended,
        })
    }

    /// Full wire snapshot returned by `GET/POST /app_update` and broadcast in `APP_UPDATE_UPDATED`.
    pub fn snapshot_json(&self, app_update_mode: &str) -> Value {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let check = g.latest_check.as_ref();
        let has_update = check.is_some_and(|r| r.has_update);
        let latest_version = check
            .map(|r| r.latest_version.trim_start_matches(['v', 'V']).to_owned())
            .unwrap_or_default();
        let release_url = check
            .map(|r| r.release_url.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("https://github.com/{}/releases", self.client_config.repo));
        let release_name = check.map(|r| r.release_name.clone()).unwrap_or_default();
        let release_notes = check.map(|r| r.release_notes.clone()).unwrap_or_default();
        let published_at = check.map(|r| r.published_at.clone()).unwrap_or_default();
        let installer_name = check
            .and_then(|r| r.installer_asset.as_ref())
            .map(|a| a.name.clone());
        let installer_size = check
            .and_then(|r| r.installer_asset.as_ref())
            .map(|a| a.size)
            .unwrap_or(0);
        let expected_sha256 = check.and_then(|r| r.expected_sha256.clone());
        let checksum_present = check.is_some_and(|r| r.checksum_verified_present);
        let can_one_click = can_one_click_from_check(check);
        let progress_pct = compute_progress_pct(g.downloaded_bytes, g.total_bytes, g.phase);
        let attempts = if latest_version.is_empty() {
            0
        } else {
            g.failed_attempts.get(&latest_version).copied().unwrap_or(0)
        };
        let auto_blocked = attempts >= MAX_AUTO_INSTALL_ATTEMPTS;
        let will_install_on_exit = g.phase == UpdatePhase::ReadyToInstall
            && app_update_mode.eq_ignore_ascii_case("auto")
            && self.allows_install_on_exit()
            && !auto_blocked;

        json!({
            "ok": true,
            "phase": g.phase.as_str(),
            "current_version": self.current_version,
            "latest_version": latest_version,
            "has_update": has_update,
            "release_name": release_name,
            "release_notes": release_notes,
            "published_at": published_at,
            "release_url": release_url,
            "installer_name": installer_name,
            "installer_size": installer_size,
            "expected_sha256": expected_sha256,
            "checksum_present": checksum_present,
            "can_one_click": can_one_click,
            "downloaded_bytes": g.downloaded_bytes,
            "total_bytes": g.total_bytes,
            "progress_pct": progress_pct,
            "verified_ready": g.verified_installer.is_some(),
            "will_install_on_exit": will_install_on_exit,
            "auto_install_blocked": auto_blocked,
            "just_updated_from": g.just_updated_from,
            "error": g.last_error,
            "checked_at": g.checked_at,
            "app_update_mode": app_update_mode,
            "auto_check_app_updates": !app_update_mode.eq_ignore_ascii_case("off"),
            "map_data": g.map_data.to_json(),
        })
    }

    pub fn release_page_url(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.latest_check
            .as_ref()
            .map(|r| r.release_url.clone())
            .filter(|u| u.starts_with("https://github.com/"))
            .unwrap_or_else(|| format!("https://github.com/{DEFAULT_GITHUB_REPO}/releases"))
    }
}

fn can_one_click_from_check(check: Option<&UpdateCheckResult>) -> bool {
    check.is_some_and(|r| r.has_update && r.installer_asset.is_some() && r.checksum_verified_present)
}

/// Only allow transparent install-on-exit when running an elevated Installed tray session so
/// quitting the app can silently run Inno Setup without ever triggering a surprise UAC prompt.
fn should_allow_install_on_exit(no_tray: bool, mode: RuntimeMode, is_elevated: bool) -> bool {
    !no_tray && mode == RuntimeMode::Installed && is_elevated
}

fn compute_progress_pct(downloaded: u64, total: u64, phase: UpdatePhase) -> u8 {
    match phase {
        UpdatePhase::ReadyToInstall | UpdatePhase::Installing | UpdatePhase::Verifying => 100,
        UpdatePhase::Downloading => downloaded
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0)
            .min(100) as u8,
        _ => 0,
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keeps any cached setup `.exe` strictly newer than `current_version` while deleting stale setups & `.part.` files.
fn cleanup_non_newer_installers(updates_dir: &Path, current_version: &str) {
    let Ok(entries) = std::fs::read_dir(updates_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with('.') && name.contains(".part.") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if is_safe_installer_filename(name) {
            let extracted_ver = name
                .strip_prefix("GenshinHelper-Setup-")
                .and_then(|rest| rest.strip_suffix(".exe"))
                .unwrap_or("");
            if !is_newer_version(current_version, extracted_ver) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Builds Inno Setup windowless `/VERYSILENT` CLI parameters localized to the active app language.
///
/// - `InstallTrigger::RestartNow` passes `/AUTOUPDATE=1` so Inno Setup relaunches `GenshinHelper.exe`.
/// - `InstallTrigger::OnExit` passes `/AUTOUPDATE=exit` so Inno Setup upgrades silently and stays closed.
pub fn build_inno_update_args(lang: &str, trigger: InstallTrigger) -> String {
    let inno_lang = if lang.eq_ignore_ascii_case("vi") {
        "vietnamese"
    } else {
        "english"
    };
    let mode = match trigger {
        InstallTrigger::RestartNow => "1",
        InstallTrigger::OnExit => "exit",
    };
    format!(
        "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /AUTOUPDATE={mode} /LANG={inno_lang}"
    )
}

/// Validates a verified installer on disk (TOCTOU re-hash) and launches Inno Setup via `ShellExecuteW`.
fn spawn_verified_installer_exe(
    verified: &VerifiedInstaller,
    lang: &str,
    trigger: InstallTrigger,
) -> Result<(), String> {
    if !is_safe_installer_filename(&verified.filename) {
        return Err(format!("unsafe installer filename: {}", verified.filename));
    }
    if !verified.path.is_file() {
        return Err(format!(
            "verified installer missing on disk: {}",
            verified.path.display()
        ));
    }
    match verify_file_sha256(&verified.path, &verified.sha256) {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(&verified.path);
            return Err("SHA-256 re-verification failed before launch; removed file".to_owned());
        }
        Err(e) => return Err(format!("SHA-256 re-verification error: {e}")),
    }

    let args = build_inno_update_args(lang, trigger);
    let path_str = verified.path.to_string_lossy();
    let exe_w: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
    let args_w: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    let dir_w: Option<Vec<u16>> = verified.path.parent().map(|p| {
        p.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    });
    let verb = if gh_win32::game_settings::is_process_elevated() {
        w!("open")
    } else {
        w!("runas")
    };

    // SAFETY: All wide strings are NUL-terminated for the duration of `ShellExecuteW`.
    let res = unsafe {
        ShellExecuteW(
            None,
            verb,
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            dir_w.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            SW_SHOWNORMAL,
        )
    };
    if (res.0 as isize) <= 32 {
        return Err(format!(
            "Windows declined or failed to start installer (ShellExecute code {})",
            res.0 as isize
        ));
    }
    Ok(())
}

fn open_https_url_in_browser(url: &str) -> Result<(), String> {
    if !url.starts_with("https://github.com/") {
        return Err(format!("refused to open non-GitHub URL: {url}"));
    }
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated for the duration of `ShellExecuteW`.
    let res = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if (res.0 as isize) <= 32 {
        return Err(format!("failed to open browser URL (code {})", res.0 as isize));
    }
    Ok(())
}

impl AppState {
    fn current_app_update_mode(&self) -> String {
        self.config
            .read()
            .expect("config lock poisoned")
            .app_update_mode
            .clone()
    }

    pub fn broadcast_app_update_state(&self) {
        let mode = self.current_app_update_mode();
        let mut payload = self.updater.snapshot_json(&mode);
        if let Value::Object(ref mut map) = payload {
            map.insert("type".to_owned(), Value::String("APP_UPDATE_UPDATED".to_owned()));
        }
        self.server.broadcast(payload, false, false);
    }

    fn sync_tray_update_indicator(&self) {
        let (available_ver, ready) = {
            let g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ver = g
                .latest_check
                .as_ref()
                .filter(|r| r.has_update)
                .map(|r| r.latest_version.clone());
            let ready = g.phase == UpdatePhase::ReadyToInstall && g.verified_installer.is_some();
            (ver, ready)
        };
        if let Some(tray) = self
            .tray
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            tray.set_update_status(available_ver.as_deref(), ready);
        }
    }

    /// Reconciles post-update state & spawns the cancellable startup + 6-hour background update loop.
    pub fn spawn_app_update_scheduler(
        self: &Arc<Self>,
        offline_mode: bool,
        no_tray: bool,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let allow_exit_install = should_allow_install_on_exit(
            no_tray,
            self.paths.mode,
            gh_win32::game_settings::is_process_elevated(),
        );
        self.updater.set_allow_install_on_exit(allow_exit_install);

        let updates_dir = self.paths.updates_dir();
        let upgraded_from = self.updater.reconcile_startup_state(&updates_dir);

        let st = Arc::clone(self);
        tokio::spawn(async move {
            if let Some(prev_ver) = upgraded_from {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(1600)) => {}
                    _ = shutdown_rx.changed() => { return; }
                }
                if *shutdown_rx.borrow() {
                    return;
                }
                let rect = st
                    .game_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rect;
                let cur = st.updater.current_version().to_owned();
                tracing::info!("[AppUpdater] Completed upgrade v{prev_ver} -> v{cur}");
                st.hud_toast.show_toast(
                    &t("toast.update.completed_title", &[("current", &cur)]),
                    &t("toast.update.completed_detail", &[("prev", &prev_ver)]),
                    ToastKind::Success,
                    5500,
                    rect,
                );
                st.broadcast_app_update_state();
            }

            if offline_mode {
                return;
            }

            // Initial grace delay so startup hydration & HTTP bind complete smoothly.
            let mut next_wait = Duration::from_secs(3);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(next_wait) => {}
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
                if *shutdown_rx.borrow() {
                    break;
                }

                let mode = st.current_app_update_mode();
                let phase = st.updater.phase();
                if !mode.eq_ignore_ascii_case("off")
                    && !matches!(
                        phase,
                        UpdatePhase::Checking
                            | UpdatePhase::Downloading
                            | UpdatePhase::Verifying
                            | UpdatePhase::ReadyToInstall
                            | UpdatePhase::Installing
                    )
                {
                    let st_bg = Arc::clone(&st);
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = st_bg.run_app_update_check(false, true);
                    })
                    .await;
                }

                next_wait = Duration::from_secs(PERIODIC_UPDATE_CHECK_SECS);
            }
        })
    }

    /// Runs a GitHub Releases check synchronously and updates state, tray indicator, and broadcast.
    /// In `"auto"` mode, automatically pre-downloads & SHA-256-verifies eligible updates in background.
    pub fn run_app_update_check(
        self: &Arc<Self>,
        manual: bool,
        notify_if_available: bool,
    ) -> Result<UpdateCheckResult, String> {
        let rect = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rect;

        {
            let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(
                g.phase,
                UpdatePhase::Checking
                    | UpdatePhase::Downloading
                    | UpdatePhase::Verifying
                    | UpdatePhase::Installing
            ) {
                if let Some(ref existing) = g.latest_check {
                    return Ok(existing.clone());
                }
                return Err("update operation already in progress".to_owned());
            }
            g.phase = UpdatePhase::Checking;
            g.last_error = None;
        }
        self.broadcast_app_update_state();

        if manual {
            self.hud_toast.show_toast(
                &t("app.name", &[]),
                &t("toast.update.checking", &[]),
                ToastKind::Info,
                2200,
                rect,
            );
        }

        let client = self.updater.client();
        let cur_ver = self.updater.current_version().to_owned();
        match client.check_latest(&cur_ver) {
            Ok(res) => {
                let latest = res.latest_version.trim_start_matches(['v', 'V']).to_owned();
                let updates_dir = self.paths.updates_dir();
                let cached_verified = find_cached_verified_installer(&res, &updates_dir);
                let next_phase = if !res.has_update {
                    UpdatePhase::UpToDate
                } else if cached_verified.is_some() {
                    UpdatePhase::ReadyToInstall
                } else {
                    UpdatePhase::Available
                };

                {
                    let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.phase = next_phase;
                    g.total_bytes = res.installer_asset.as_ref().map_or(0, |a| a.size);
                    g.downloaded_bytes = cached_verified.as_ref().map_or(0, |v| v.size_bytes);
                    g.verified_installer = cached_verified;
                    g.latest_check = Some(res.clone());
                    g.last_error = None;
                    g.checked_at = Some(unix_now_secs());
                }

                self.sync_tray_update_indicator();
                self.broadcast_app_update_state();

                let mode = self.current_app_update_mode();
                let auto_blocked =
                    self.updater.failed_attempts_for(&latest) >= MAX_AUTO_INSTALL_ATTEMPTS;

                if res.has_update {
                    tracing::info!(
                        "[AppUpdater] Newer release available: v{latest} (sha256_present={}, mode={mode}, ready={})",
                        res.checksum_verified_present,
                        next_phase == UpdatePhase::ReadyToInstall
                    );

                    if next_phase == UpdatePhase::ReadyToInstall {
                        if manual || notify_if_available {
                            self.notify_update_ready_once(&latest, manual);
                        }
                    } else if !manual
                        && mode.eq_ignore_ascii_case("auto")
                        && can_one_click_from_check(Some(&res))
                        && !auto_blocked
                    {
                        // Seamless VS Code / T3 Code background pre-download & SHA-256 verification.
                        let _ = self.start_verified_download(false, false, notify_if_available);
                    } else if manual || notify_if_available {
                        let detail = if res.checksum_verified_present {
                            t("toast.update.available_detail", &[("current", &cur_ver)])
                        } else {
                            t("toast.update.available_detail_no_sha", &[("current", &cur_ver)])
                        };
                        self.hud_toast.show_toast(
                            &t("toast.update.available_title", &[("latest", &latest)]),
                            &detail,
                            ToastKind::Success,
                            5500,
                            rect,
                        );
                    }
                } else if manual {
                    self.hud_toast.show_toast(
                        &t("toast.update.up_to_date_title", &[("current", &cur_ver)]),
                        &t("toast.update.up_to_date_detail", &[]),
                        ToastKind::Info,
                        3200,
                        rect,
                    );
                }

                Ok(res)
            }
            Err(err) => {
                {
                    let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if manual {
                        g.phase = UpdatePhase::Error;
                        g.last_error = Some(err.clone());
                    } else {
                        g.phase = if g.latest_check.as_ref().is_some_and(|r| r.has_update) {
                            if g.verified_installer.is_some() {
                                UpdatePhase::ReadyToInstall
                            } else {
                                UpdatePhase::Available
                            }
                        } else {
                            UpdatePhase::Idle
                        };
                    }
                    g.checked_at = Some(unix_now_secs());
                }
                self.broadcast_app_update_state();

                if manual {
                    tracing::warn!("[AppUpdater] Manual check failed: {err}");
                    self.hud_toast.show_toast(
                        &t("toast.update.error_title", &[]),
                        &t("toast.update.error_detail", &[("error", &err)]),
                        ToastKind::Warning,
                        4200,
                        rect,
                    );
                } else {
                    tracing::debug!("[AppUpdater] Background check skipped/failed: {err}");
                }
                Err(err)
            }
        }
    }

    fn notify_update_ready_once(&self, latest: &str, force: bool) {
        let should_show = {
            let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !force && g.notified_ready_version.as_deref() == Some(latest) {
                false
            } else {
                g.notified_ready_version = Some(latest.to_owned());
                true
            }
        };
        if should_show {
            let rect = self
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.hud_toast.show_toast(
                &t("toast.update.ready_title", &[("latest", latest)]),
                &t("toast.update.ready_detail", &[]),
                ToastKind::Success,
                5500,
                rect,
            );
        }
    }

    /// Starts streaming download + SHA-256 verification on a background thread.
    /// - If `auto_install` is true, immediately launches the verified Inno Setup installer once hashed.
    /// - If `notify_when_ready` is true (background pre-download), shows a single ready toast on completion.
    pub fn start_verified_download(
        self: &Arc<Self>,
        auto_install: bool,
        show_toasts: bool,
        notify_when_ready: bool,
    ) -> CoreResult<Value> {
        let (installer, expected_sha256, latest_ver, already_ready) = {
            let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(
                g.phase,
                UpdatePhase::Downloading | UpdatePhase::Verifying | UpdatePhase::Installing
            ) {
                let mode = self.current_app_update_mode();
                return Ok(self.updater.snapshot_json(&mode));
            }
            let Some(check) = g.latest_check.clone() else {
                return Err(CoreError::InvalidArgument(
                    "No update check result yet; check for updates first.".to_owned(),
                ));
            };
            if !check.has_update {
                return Err(CoreError::InvalidArgument(
                    "Application is already up to date.".to_owned(),
                ));
            }
            let Some(asset) = check.installer_asset.clone() else {
                return Err(CoreError::InvalidArgument(
                    "Release does not include a Windows Setup installer asset.".to_owned(),
                ));
            };
            let Some(sha) = check.expected_sha256.clone() else {
                return Err(CoreError::InvalidArgument(
                    "Release is missing SHA256SUMS.txt digest; refusing unverified download (open GitHub Releases manually).".to_owned(),
                ));
            };

            let ready = g.verified_installer.clone().filter(|v| {
                v.sha256.eq_ignore_ascii_case(&sha)
                    && v.path.is_file()
                    && verify_file_sha256(&v.path, &sha).unwrap_or(false)
            });
            if ready.is_none() {
                g.phase = UpdatePhase::Downloading;
                g.downloaded_bytes = 0;
                g.total_bytes = asset.size;
                g.last_error = None;
                g.verified_installer = None;
            }
            (
                asset,
                sha,
                check.latest_version.trim_start_matches(['v', 'V']).to_owned(),
                ready,
            )
        };

        if already_ready.is_some() {
            if auto_install {
                return self.launch_verified_update_install(InstallTrigger::RestartNow, false);
            }
            let mode = self.current_app_update_mode();
            return Ok(self.updater.snapshot_json(&mode));
        }

        self.updater.download_cancel.store(false, Ordering::Release);
        self.broadcast_app_update_state();

        if show_toasts {
            let rect = self
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.hud_toast.show_toast(
                &t("toast.update.downloading_title", &[("latest", &latest_ver)]),
                &t("toast.update.downloading_detail", &[]),
                ToastKind::Info,
                4000,
                rect,
            );
        }

        let st = Arc::clone(self);
        let updates_dir = self.paths.updates_dir();
        let cancel_flag = Arc::clone(&self.updater.download_cancel);
        let client = self.updater.client();

        let _ = std::thread::Builder::new()
            .name("gh-app-update-dl".to_owned())
            .spawn(move || {
                let st_prog = Arc::clone(&st);
                let mut last_emit = Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
                let dl_res = client.download_verified_installer(
                    &installer,
                    &expected_sha256,
                    &updates_dir,
                    Some(Arc::clone(&cancel_flag)),
                    move |dl, tot| {
                        {
                            let mut g = st_prog
                                .updater
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            g.downloaded_bytes = dl;
                            if tot > 0 {
                                g.total_bytes = tot;
                            }
                            if tot > 0 && dl >= tot {
                                g.phase = UpdatePhase::Verifying;
                            }
                        }
                        if last_emit.elapsed() >= Duration::from_millis(180)
                            || (tot > 0 && dl >= tot)
                        {
                            last_emit = Instant::now();
                            st_prog.broadcast_app_update_state();
                        }
                    },
                );

                match dl_res {
                    Ok(verified) => {
                        let _ = cleanup_stale_update_files(&updates_dir, Some(&verified.filename));
                        tracing::info!(
                            "[AppUpdater] Verified installer ready: {} (sha256={}, cached={})",
                            verified.path.display(),
                            verified.sha256,
                            verified.reused_cached
                        );
                        {
                            let mut g = st
                                .updater
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            g.phase = UpdatePhase::ReadyToInstall;
                            g.downloaded_bytes = verified.size_bytes;
                            g.total_bytes = verified.size_bytes;
                            g.verified_installer = Some(verified);
                            g.last_error = None;
                        }
                        st.sync_tray_update_indicator();
                        st.broadcast_app_update_state();

                        if auto_install {
                            if let Err(err) =
                                st.launch_verified_update_install(InstallTrigger::RestartNow, false)
                            {
                                tracing::error!("[AppUpdater] Installer launch failed: {err}");
                            }
                        } else if notify_when_ready {
                            st.notify_update_ready_once(&latest_ver, false);
                        }
                    }
                    Err(err) => {
                        let was_cancelled = cancel_flag.load(Ordering::Acquire);
                        let is_background_silent = !auto_install && !show_toasts;
                        {
                            let mut g = st
                                .updater
                                .inner
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if was_cancelled || is_background_silent {
                                g.phase = UpdatePhase::Available;
                                g.downloaded_bytes = 0;
                                g.last_error = if was_cancelled { None } else { Some(err.clone()) };
                            } else {
                                g.phase = UpdatePhase::Error;
                                g.last_error = Some(err.clone());
                            }
                        }
                        st.sync_tray_update_indicator();
                        st.broadcast_app_update_state();
                        if !was_cancelled && show_toasts {
                            let rect = st
                                .game_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .rect;
                            st.hud_toast.show_toast(
                                &t("toast.update.error_title", &[]),
                                &t("toast.update.error_detail", &[("error", &err)]),
                                ToastKind::Warning,
                                4500,
                                rect,
                            );
                        } else if !was_cancelled {
                            tracing::debug!("[AppUpdater] Background pre-download failed: {err}");
                        }
                    }
                }
            });

        let mode = self.current_app_update_mode();
        Ok(self.updater.snapshot_json(&mode))
    }

    /// Re-verifies the downloaded installer SHA-256 on disk, persists an attempt marker, launches
    /// Inno Setup in windowless `/VERYSILENT` mode (unless `dry_run`), and schedules graceful app exit
    /// when triggered via `InstallTrigger::RestartNow`.
    pub fn launch_verified_update_install(
        &self,
        trigger: InstallTrigger,
        dry_run: bool,
    ) -> CoreResult<Value> {
        let (verified, latest_ver) = {
            let g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.phase == UpdatePhase::Installing {
                let mode = self.current_app_update_mode();
                return Ok(self.updater.snapshot_json(&mode));
            }
            let Some(v) = g.verified_installer.clone() else {
                return Err(CoreError::InvalidArgument(
                    "No SHA-256-verified installer is ready to install.".to_owned(),
                ));
            };
            let ver = g
                .latest_check
                .as_ref()
                .map(|r| r.latest_version.trim_start_matches(['v', 'V']).to_owned())
                .unwrap_or_default();
            (v, ver)
        };

        let updates_dir = self.paths.updates_dir();
        let lang = get_language();
        if dry_run {
            match verify_file_sha256(&verified.path, &verified.sha256) {
                Ok(true) => {}
                Ok(false) => {
                    let _ = std::fs::remove_file(&verified.path);
                    let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.phase = UpdatePhase::Error;
                    g.verified_installer = None;
                    g.last_error =
                        Some("SHA-256 re-verification failed before launch".to_owned());
                    return Err(CoreError::InvalidData(
                        "SHA-256 re-verification failed before launch".to_owned(),
                    ));
                }
                Err(e) => return Err(CoreError::InvalidData(e)),
            }
            if !latest_ver.is_empty() {
                self.updater
                    .record_install_attempt_on_disk(&updates_dir, &latest_ver);
            }
        } else {
            if !latest_ver.is_empty() {
                self.updater
                    .record_install_attempt_on_disk(&updates_dir, &latest_ver);
            }
            if let Err(err) = spawn_verified_installer_exe(&verified, &lang, trigger) {
                {
                    let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.phase = UpdatePhase::Error;
                    g.last_error = Some(err.clone());
                }
                self.broadcast_app_update_state();
                return Err(CoreError::Sync(err));
            }
        }

        {
            let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.phase = UpdatePhase::Installing;
            g.last_error = None;
        }
        self.broadcast_app_update_state();

        if trigger == InstallTrigger::RestartNow {
            let rect = self
                .game_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rect;
            self.hud_toast.show_toast(
                &t("toast.update.installing_title", &[("latest", &latest_ver)]),
                &t("toast.update.installing_detail", &[]),
                ToastKind::Success,
                4000,
                rect,
            );

            if !dry_run {
                let stop_tx = self.stop_tx.clone();
                let _ = std::thread::Builder::new()
                    .name("gh-update-shutdown".to_owned())
                    .spawn(move || {
                        // Allow HTTP 200 response & APP_UPDATE_UPDATED broadcast to flush before exit.
                        std::thread::sleep(Duration::from_millis(450));
                        let _ = stop_tx.send(true);
                    });
            }
        }

        let mode = self.current_app_update_mode();
        Ok(self.updater.snapshot_json(&mode))
    }

    /// Invoked during graceful shutdown in `main.rs` after HTTP server, tray, and mutex are released.
    /// Applies a pre-downloaded, SHA-256-verified update silently (`/AUTOUPDATE=exit`, no relaunch)
    /// when `app_update_mode == "auto"` in a real elevated Installed session and under the crash-loop cap.
    /// Non-elevated sessions skip automatic install-on-exit so quitting never pops a surprise UAC prompt.
    pub fn apply_pending_update_on_exit(&self, dry_run: bool) -> bool {
        if !dry_run
            && (!self.updater.allows_install_on_exit()
                || !gh_win32::game_settings::is_process_elevated())
        {
            return false;
        }
        if !self.current_app_update_mode().eq_ignore_ascii_case("auto") {
            return false;
        }
        let target_ver = {
            let g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.phase != UpdatePhase::ReadyToInstall || g.verified_installer.is_none() {
                return false;
            }
            let ver = g
                .latest_check
                .as_ref()
                .map(|r| r.latest_version.trim_start_matches(['v', 'V']).to_owned())
                .unwrap_or_default();
            if ver.is_empty() {
                return false;
            }
            let attempts = g.failed_attempts.get(&ver).copied().unwrap_or(0);
            if attempts >= MAX_AUTO_INSTALL_ATTEMPTS {
                tracing::warn!(
                    "[AppUpdater] Skipping auto install-on-exit for v{ver} (previous failed attempts={attempts})"
                );
                return false;
            }
            ver
        };

        tracing::info!(
            "[AppUpdater] Applying pre-verified update v{target_ver} silently on exit..."
        );
        match self.launch_verified_update_install(InstallTrigger::OnExit, dry_run) {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!("[AppUpdater] Install-on-exit skipped: {err}");
                false
            }
        }
    }

    /// Handles System Tray "Check for Updates / Restart to Update" clicks cleanly.
    pub fn handle_tray_update_click(self: &Arc<Self>) {
        let (phase, can_one_click, has_update, release_url) = {
            let g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            (
                g.phase,
                can_one_click_from_check(g.latest_check.as_ref()),
                g.latest_check.as_ref().is_some_and(|r| r.has_update),
                self.updater.release_page_url(),
            )
        };

        match phase {
            UpdatePhase::Checking
            | UpdatePhase::Downloading
            | UpdatePhase::Verifying
            | UpdatePhase::Installing => {}
            UpdatePhase::ReadyToInstall => {
                if let Err(err) =
                    self.launch_verified_update_install(InstallTrigger::RestartNow, false)
                {
                    tracing::warn!("[TrayUpdate] Install launch failed: {err}");
                }
            }
            UpdatePhase::Available if has_update && can_one_click => {
                if let Err(err) = self.start_verified_download(true, true, false) {
                    tracing::warn!("[TrayUpdate] Verified download failed to start: {err}");
                }
            }
            UpdatePhase::Available if has_update => {
                let _ = open_https_url_in_browser(&release_url);
            }
            _ => {
                let _ = self.run_app_update_check(true, true);
            }
        }
    }

    /// Triggers a manual HoYoLAB map dataset check + atomic download in background.
    pub fn start_manual_map_data_sync(self: &Arc<Self>) -> Value {
        let already_busy = {
            let mut g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(g.map_data.phase.as_str(), "checking" | "updating") {
                true
            } else {
                g.map_data.phase = "checking".to_owned();
                g.map_data.last_error = None;
                false
            }
        };
        if already_busy {
            let mode = self.current_app_update_mode();
            return self.updater.snapshot_json(&mode);
        }
        self.broadcast_app_update_state();

        let st = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("gh-manual-map-sync".to_owned())
            .spawn(move || {
                let mut sync_cfg = st
                    .config
                    .read()
                    .expect("config lock poisoned")
                    .hoyolab_sync
                    .clone();
                sync_cfg.auto_check_on_startup = true;
                sync_cfg.auto_update_mode = "auto_download".to_owned();

                let mut local_vers: BTreeMap<i32, String> = BTreeMap::new();
                for (&mid, meta) in &st.maps_db.maps {
                    if let Some(ref d2) = meta.detail_v2
                        && !d2.map_version.trim().is_empty()
                    {
                        local_vers.insert(mid, d2.map_version.clone());
                    }
                }

                let runner = HoyolabSyncClient::new();
                match check_and_apply_startup_update(&runner, &st.paths, &sync_cfg, &local_vers) {
                    Ok(outcome) => {
                        st.handle_map_data_update_outcome(&outcome, true);
                    }
                    Err(e) => {
                        tracing::warn!("[MapDataSync] Manual sync failed: {e}");
                        st.updater
                            .record_map_data_phase("error", Some(e.to_string()));
                        st.broadcast_app_update_state();
                    }
                }
            });

        let mode = self.current_app_update_mode();
        self.updater.snapshot_json(&mode)
    }

    /// Records a HoYoLAB map dataset outcome (startup or manual), broadcasts it, and shows a toast
    /// when new map data was downloaded or is available.
    pub fn handle_map_data_update_outcome(
        &self,
        outcome: &StartupUpdateOutcome,
        from_manual: bool,
    ) {
        self.updater.record_map_data_outcome(outcome);
        self.broadcast_app_update_state();

        let rect = self
            .game_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rect;

        match outcome {
            StartupUpdateOutcome::UpdateAvailable(res) => {
                let count = (res.updated_maps.len() + res.new_map_ids.len()).max(1);
                let count_s = count.to_string();
                self.hud_toast.show_toast(
                    &t("toast.map_update.available_title", &[]),
                    &t("toast.map_update.available_detail", &[("count", &count_s)]),
                    ToastKind::Info,
                    4800,
                    rect,
                );
            }
            StartupUpdateOutcome::Updated(res) => {
                let count = (res.updated_maps.len() + res.new_map_ids.len()).max(1);
                let count_s = count.to_string();
                self.hud_toast.show_toast(
                    &t("toast.map_update.updated_title", &[]),
                    &t("toast.map_update.updated_detail", &[("count", &count_s)]),
                    ToastKind::Success,
                    5000,
                    rect,
                );
            }
            StartupUpdateOutcome::UpToDate(_) if from_manual => {
                tracing::info!("[MapDataSync] HoYoLAB map dataset is already up to date.");
            }
            _ => {}
        }
    }

    /// Handles `GET/POST /app_update` actions from the Extension Popup and tests.
    pub fn handle_app_update_command(self: &Arc<Self>, payload: &Value) -> CoreResult<Value> {
        let req = payload.as_object().cloned().unwrap_or_default();
        let action = req
            .get("action")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "status".to_owned());

        match action.as_str() {
            "status" => {
                let mode = self.current_app_update_mode();
                Ok(self.updater.snapshot_json(&mode))
            }
            "check" => {
                let show_toast = req
                    .get("show_toast")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let _ = self.run_app_update_check(show_toast, false);
                let mode = self.current_app_update_mode();
                Ok(self.updater.snapshot_json(&mode))
            }
            "download" | "download_and_install" => {
                let auto_install = action == "download_and_install"
                    || req
                        .get("auto_install")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let show_toasts = req
                    .get("show_toast")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.start_verified_download(auto_install, show_toasts, false)
            }
            "install" => {
                let dry_run = req.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
                self.launch_verified_update_install(InstallTrigger::RestartNow, dry_run)
            }
            "cancel" => {
                self.updater.cancel_download();
                let mode = self.current_app_update_mode();
                Ok(self.updater.snapshot_json(&mode))
            }
            "open_release" => {
                let url = self.updater.release_page_url();
                open_https_url_in_browser(&url).map_err(CoreError::Sync)?;
                Ok(json!({ "ok": true, "url": url }))
            }
            "set_mode" | "set_auto_check" => {
                let next_mode = if let Some(m) = req
                    .get("mode")
                    .or_else(|| req.get("app_update_mode"))
                    .and_then(Value::as_str)
                {
                    let norm = m.trim().to_ascii_lowercase();
                    if VALID_APP_UPDATE_MODES.contains(&norm.as_str()) {
                        norm
                    } else {
                        return Err(CoreError::InvalidArgument(format!(
                            "Invalid app_update_mode: {m}"
                        )));
                    }
                } else {
                    let enabled = req
                        .get("enabled")
                        .or_else(|| req.get("auto_check_app_updates"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if enabled {
                        "auto".to_owned()
                    } else {
                        "off".to_owned()
                    }
                };

                let _ = self.mutate_and_save_config(|cfg| {
                    cfg.app_update_mode = next_mode.clone();
                    cfg.auto_check_app_updates = next_mode != "off";
                });
                self.broadcast_app_update_state();

                // If the user switched to `"auto"` while an eligible update is already known,
                // start silent pre-download right away so Restart-to-Update becomes instant.
                if next_mode == "auto" {
                    let can_predl = {
                        let g = self.updater.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.phase == UpdatePhase::Available
                            && can_one_click_from_check(g.latest_check.as_ref())
                    };
                    if can_predl {
                        let _ = self.start_verified_download(false, false, false);
                    }
                }
                Ok(self.updater.snapshot_json(&next_mode))
            }
            "sync_map_data" => Ok(self.start_manual_map_data_sync()),
            other => Err(CoreError::InvalidArgument(format!(
                "Unknown app_update action: {other}"
            ))),
        }
    }
}

fn find_cached_verified_installer(
    res: &UpdateCheckResult,
    updates_dir: &Path,
) -> Option<VerifiedInstaller> {
    if !res.has_update {
        return None;
    }
    let asset = res.installer_asset.as_ref()?;
    let expected = res.expected_sha256.as_deref()?;
    if !is_safe_installer_filename(&asset.name) {
        return None;
    }
    let path = updates_dir.join(&asset.name);
    if path.is_file() && verify_file_sha256(&path, expected).unwrap_or(false) {
        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(asset.size);
        return Some(VerifiedInstaller {
            path,
            filename: asset.name.clone(),
            size_bytes,
            sha256: expected.to_ascii_lowercase(),
            reused_cached: true,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::make_test_state_with_db;
    use gh_net::update_check::sha256_hex;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(label: &str) -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gh_app_updater_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_inno_update_args_distinguishes_restart_vs_exit_and_language() {
        let vi_restart = build_inno_update_args("vi", InstallTrigger::RestartNow);
        assert!(vi_restart.contains("/VERYSILENT"));
        assert!(vi_restart.contains("/AUTOUPDATE=1"));
        assert!(vi_restart.contains("/LANG=vietnamese"));

        let en_exit = build_inno_update_args("en", InstallTrigger::OnExit);
        assert!(en_exit.contains("/VERYSILENT"));
        assert!(en_exit.contains("/AUTOUPDATE=exit"));
        assert!(en_exit.contains("/LANG=english"));
    }

    #[test]
    fn startup_reconciliation_detects_upgrade_and_enforces_crash_loop_guard() {
        let root = temp_root("reconcile");
        let updates_dir = root.join("updates");
        std::fs::create_dir_all(&updates_dir).unwrap();

        // 1. Simulate previous v5.7.0 launching an upgrade to v5.8.0 that succeeded
        PersistedUpdaterState {
            last_launched_version: "5.7.0".to_owned(),
            pending_install_version: Some("5.8.0".to_owned()),
            failed_attempts: BTreeMap::from([("5.8.0".to_owned(), 1)]),
        }
        .save_to(&updates_dir);
        std::fs::write(updates_dir.join("GenshinHelper-Setup-v5.8.0.exe"), b"old").unwrap();

        let up_new = AppUpdater::with_config(UpdateCheckConfig::default(), "5.8.0".to_owned());
        let prev = up_new.reconcile_startup_state(&updates_dir);
        assert_eq!(prev.as_deref(), Some("5.7.0"));
        assert!(!updates_dir.join("GenshinHelper-Setup-v5.8.0.exe").exists());
        assert_eq!(up_new.failed_attempts_for("5.8.0"), 0);

        // 2. Simulate two failed/aborted install attempts for v5.9.0 while staying on v5.8.0
        for expected_count in 1..=2 {
            up_new.record_install_attempt_on_disk(&updates_dir, "5.9.0");
            let up_retry =
                AppUpdater::with_config(UpdateCheckConfig::default(), "5.8.0".to_owned());
            assert_eq!(up_retry.reconcile_startup_state(&updates_dir), None);
            assert_eq!(up_retry.failed_attempts_for("5.9.0"), expected_count);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_mode_predownloads_in_background_and_supports_install_on_exit_and_mode_switching() {
        let payload = b"MZ-synthetic-inno-installer-payload-v5.8.0-verified".to_vec();
        let good_sha = sha256_hex(&payload);
        let setup_name = "GenshinHelper-Setup-v5.8.0.exe";

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock github");
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let release_json = json!({
            "tag_name": "v5.8.0",
            "name": "Genshin-Helper v5.8.0",
            "body": "- Seamless VS Code-style auto-download\n- Instant Restart to Update",
            "published_at": "2026-11-26T00:00:00Z",
            "html_url": "https://github.com/tuanminh26596/Genshin-helper/releases/tag/v5.8.0",
            "assets": [
                {
                    "name": setup_name,
                    "browser_download_url": format!("{base}/dl/{setup_name}"),
                    "size": payload.len() as u64
                },
                {
                    "name": "SHA256SUMS.txt",
                    "browser_download_url": format!("{base}/dl/SHA256SUMS.txt"),
                    "size": 100u64
                }
            ]
        })
        .to_string();
        let sums_body = format!("{good_sha}  {setup_name}\n");
        let payload_srv = payload.clone();

        let srv = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
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
                if path == "/repos/tuanminh26596/Genshin-helper/releases/latest" {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{release_json}",
                        release_json.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else if path == "/dl/SHA256SUMS.txt" {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sums_body}",
                        sums_body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else if path == format!("/dl/{setup_name}") {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload_srv.len()
                    );
                    let _ = stream.write_all(hdr.as_bytes());
                    let _ = stream.write_all(&payload_srv);
                }
                let _ = stream.flush();
            }
        });

        let root = temp_root("auto_predl");
        let mut state_arc = make_test_state_with_db(&root, None, None, None);
        {
            let st_mut = Arc::get_mut(&mut state_arc).expect("unique Arc in test");
            st_mut.updater = Arc::new(AppUpdater::with_config(
                UpdateCheckConfig {
                    api_base_url: base,
                    timeout_secs: 15,
                    ..Default::default()
                },
                "5.7.0".to_owned(),
            ));
            st_mut.updater.set_allow_install_on_exit(true);
        }

        // 1. Background check in default "auto" mode triggers automatic SHA-256 pre-download
        let res = state_arc
            .run_app_update_check(false, false)
            .expect("background check ok");
        assert!(res.has_update);

        let deadline = Instant::now() + Duration::from_secs(15);
        while state_arc.updater.phase() != UpdatePhase::ReadyToInstall
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state_arc.updater.phase(), UpdatePhase::ReadyToInstall);

        let snap = state_arc
            .handle_app_update_command(&json!({"action": "status"}))
            .unwrap();
        assert_eq!(snap["phase"], "ready_to_install");
        assert_eq!(snap["verified_ready"], true);
        assert_eq!(snap["will_install_on_exit"], true);

        // 2. Switching mode to "notify" disables install-on-exit while keeping verified file ready
        let mode_notify = state_arc
            .handle_app_update_command(&json!({"action": "set_mode", "mode": "notify"}))
            .unwrap();
        assert_eq!(mode_notify["app_update_mode"], "notify");
        assert_eq!(mode_notify["will_install_on_exit"], false);
        assert!(!state_arc.apply_pending_update_on_exit(true));

        // 3. Switching back to "auto" enables dry-run install-on-exit & records pending attempt
        let _ = state_arc
            .handle_app_update_command(&json!({"action": "set_mode", "mode": "auto"}))
            .unwrap();
        assert!(state_arc.apply_pending_update_on_exit(true));
        assert_eq!(state_arc.updater.phase(), UpdatePhase::Installing);

        let persisted = PersistedUpdaterState::load_from(&state_arc.paths.updates_dir());
        assert_eq!(persisted.pending_install_version.as_deref(), Some("5.8.0"));

        srv.join().expect("mock server done");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_allow_install_on_exit_requires_installed_tray_and_elevation() {
        assert!(should_allow_install_on_exit(false, RuntimeMode::Installed, true));
        assert!(!should_allow_install_on_exit(false, RuntimeMode::Installed, false));
        assert!(!should_allow_install_on_exit(true, RuntimeMode::Installed, true));
        assert!(!should_allow_install_on_exit(false, RuntimeMode::Dev, true));
    }
}
