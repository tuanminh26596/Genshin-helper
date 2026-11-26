//! Localhost Sync Server (`127.0.0.1:<port>`) matching `src/server.py` 100%.
//!
//! Invariants:
//! - Lossless multi-tab `/poll?since=<seq>` backed by monotonic `_event_seq` + durable
//!   `_event_log` ring buffer (`maxlen = 128`) + separate per-type `_ephemeral_events`.
//! - Request body limit >= 32 MiB for `/cross_map_export`.
//! - `/shutdown` enforces `POST` + loopback (`403 Forbidden`, never `405`).
//! - CORS & Private Network Access + `Connection: close` on every HTTP response.

use crate::lan_companion::{LanCompanionConfig, LanCompanionServer};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, RawQuery, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, ORIGIN,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use gh_core::atomic_io::{write_json_atomic, write_str_atomic};
use gh_core::config::{AppConfig, VALID_MMAP_OVERLAY_MODES};
use gh_core::i18n::{resolve_language, set_language};
use gh_core::map_bridge::{execute_cross_map_export, execute_provider_calibration};
use gh_core::nav_controller::{
    NavAction, NavPin, NavigationController, normalize_raw_nav_pin,
};
use gh_core::protocol::{
    PollEnvelope, StatusResponse, format_hotkey_display_name,
};
use gh_core::route_doc::{parse_hoyolab_route_share_url, parse_route_auto};
use gh_core::traits::{ConflictAdvisor, DiagnosticService, ServerCallbacks};
use gh_core::tsp_route::{
    DEFAULT_TELEPORT_PENALTY, TspCoordInput, TspPinInput, TspPlanOptions, TspWaypoint,
    TspWaypointInput, load_waypoints_for_map, normalize_waypoints, plan_route,
};
use gh_core::{CoreError, CoreResult, DEFAULT_SYNC_HOST, DEFAULT_SYNC_PORT};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot, watch};

pub const EVENT_LOG_MAXLEN: usize = 128;
pub const EVENT_QUEUE_MAXLEN: usize = 64;
pub const POLL_TIMEOUT_SECS: u64 = 15;
pub const MAX_HTTP_BODY_BYTES: usize = 32 * 1024 * 1024;
pub const FEEDBACK_DEDUP_WINDOW_SECS: f64 = 3.0;
pub const FEEDBACK_SEEN_MAXLEN: usize = 24;
/// A `visible: true` report from a HoYoLAB tab counts only while fresh: tabs re-report every
/// ~5 s while open, so a crashed/closed tab can never pin feedback to the web forever.
pub const TAB_VISIBLE_TTL_SECS: f64 = 20.0;
/// Upper bound on tracked tab ids (oldest entries are pruned first).
pub const TAB_VISIBILITY_MAXLEN: usize = 16;

pub const HOTKEY_ACTIONS: [&str; 11] = [
    "sync",
    "debug",
    "switch",
    "nav_mark",
    "nav_undo",
    "nav_guide",
    "nav_next",
    "nav_layout",
    "nav_toggle",
    "nav_unlock",
    "radar_toggle",
];

const HEADER_ALLOW_PRIVATE_NETWORK: HeaderName =
    HeaderName::from_static("access-control-allow-private-network");

// ---------------------------------------------------------------------------
// Config & Shared State
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SyncServerConfig {
    pub host: String,
    pub port: u16,
    pub poll_timeout: Duration,
    pub config: AppConfig,
    pub config_path: Option<PathBuf>,
    pub diagnostics_dir: Option<PathBuf>,
    pub callbacks: Option<Arc<dyn ServerCallbacks>>,
    pub conflict_advisor: Option<Arc<dyn ConflictAdvisor>>,
    pub diagnostic_service: Option<Arc<dyn DiagnosticService>>,
    pub lan_companion: Option<Arc<LanCompanionServer>>,
}

impl Default for SyncServerConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_SYNC_HOST.to_owned(),
            port: DEFAULT_SYNC_PORT,
            poll_timeout: Duration::from_secs(POLL_TIMEOUT_SECS),
            config: AppConfig::default(),
            config_path: None,
            diagnostics_dir: None,
            callbacks: None,
            conflict_advisor: None,
            diagnostic_service: None,
            lan_companion: None,
        }
    }
}

struct PollState {
    event_seq: u64,
    event_log: VecDeque<(u64, Value)>,
    ephemeral_events: HashMap<String, (u64, Value)>,
    event_queue: VecDeque<Value>,
    active_waiters: usize,
    last_sync_info: Option<Value>,
    last_client_poll_ts: Option<Instant>,
    shutdown_requested: bool,
}

impl PollState {
    fn new() -> Self {
        Self {
            event_seq: 0,
            event_log: VecDeque::with_capacity(EVENT_LOG_MAXLEN),
            ephemeral_events: HashMap::new(),
            event_queue: VecDeque::with_capacity(EVENT_QUEUE_MAXLEN),
            active_waiters: 0,
            last_sync_info: None,
            last_client_poll_ts: None,
            shutdown_requested: false,
        }
    }

    fn next_event_since(&self, since_seq: u64) -> Option<Value> {
        let mut best_seq: Option<u64> = None;
        let mut best_payload: Option<&Value> = None;

        for (seq, payload) in &self.event_log {
            if *seq > since_seq {
                best_seq = Some(*seq);
                best_payload = Some(payload);
                break;
            }
        }

        for (seq, payload) in self.ephemeral_events.values() {
            if *seq > since_seq && best_seq.is_none_or(|b| *seq < b) {
                best_seq = Some(*seq);
                best_payload = Some(payload);
            }
        }

        best_payload.cloned()
    }
}

struct FeedbackSeenEntry {
    seq: i64,
    event_type: String,
    status: String,
    ts: Instant,
}

struct StandaloneNavState {
    controller: NavigationController,
    route_legs: Vec<Value>,
    guide_open: bool,
    edit_mode: bool,
}

struct ServerLifecycle {
    shutdown_tx: Option<watch::Sender<bool>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    bg_stop_tx: Option<oneshot::Sender<()>>,
    bg_thread: Option<std::thread::JoinHandle<()>>,
}

struct SyncInner {
    host: String,
    port: AtomicU16,
    poll_timeout: Duration,
    is_running: AtomicBool,
    engine_active: AtomicBool,
    skip_exit_update: AtomicBool,

    config: RwLock<AppConfig>,
    config_path: Option<PathBuf>,
    diagnostics_dir: Option<PathBuf>,

    callbacks: RwLock<Option<Arc<dyn ServerCallbacks>>>,
    conflict_advisor: RwLock<Option<Arc<dyn ConflictAdvisor>>>,
    diagnostic_service: RwLock<Option<Arc<dyn DiagnosticService>>>,
    lan_companion: Arc<LanCompanionServer>,

    poll_state: Mutex<PollState>,
    poll_notify: Notify,
    browser_state: Mutex<Map<String, Value>>,
    feedback_seen: Mutex<VecDeque<FeedbackSeenEntry>>,
    tab_visibility: Mutex<HashMap<String, TabVisibility>>,
    pin_owner: Mutex<Option<PinOwnerState>>,
    active_provider: Mutex<String>,
    nav_state: Mutex<StandaloneNavState>,
    lifecycle: Mutex<ServerLifecycle>,
}

const PIN_OWNER_TTL_SECS: f64 = 12.0;

/// Tracks the single authoritative HoYoLAB map tab across ALL connected browsers/profiles
/// (e.g. Chrome + Edge open simultaneously) so a background browser with different filters
/// cannot silently overwrite the active browser's `/nearby_pins` or `/lan_pins`.
#[derive(Debug, Clone)]
struct PinOwnerState {
    tab_id: String,
    last_seen: Instant,
    visible: bool,
    focused: bool,
}

impl SyncInner {
    fn claim_or_check_pin_owner(
        &self,
        tab_id: Option<&str>,
        visible: Option<bool>,
        focused: Option<bool>,
        user_initiated: bool,
        now: Instant,
    ) -> bool {
        let Some(tid) = tab_id.map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64) else {
            // Backward compatibility for callers/tests that omit `tab_id`.
            return true;
        };
        let mut guard = self.pin_owner.lock().expect("pin_owner lock poisoned");
        if let Some(ref mut cur) = *guard
            && cur.tab_id == tid
        {
            cur.last_seen = now;
            if let Some(v) = visible {
                cur.visible = v;
            }
            if let Some(f) = focused {
                cur.focused = f;
            }
            return true;
        }
        let owner_stale = guard
            .as_ref()
            .is_none_or(|o| now.saturating_duration_since(o.last_seen).as_secs_f64() > PIN_OWNER_TTL_SECS);
        let owner_hidden_and_new_visible =
            guard.as_ref().is_some_and(|o| !o.visible) && visible == Some(true);
        let new_focused = focused == Some(true);
        if owner_stale || user_initiated || new_focused || owner_hidden_and_new_visible {
            *guard = Some(PinOwnerState {
                tab_id: tid.to_owned(),
                last_seen: now,
                visible: visible.unwrap_or(true),
                focused: focused.unwrap_or(user_initiated),
            });
            true
        } else {
            false
        }
    }

    fn update_pin_owner_from_visibility(
        &self,
        tab_id: &str,
        visible: bool,
        focused: Option<bool>,
        interacted: bool,
        closing: bool,
        now: Instant,
    ) {
        let mut guard = self.pin_owner.lock().expect("pin_owner lock poisoned");
        if closing {
            if guard.as_ref().is_some_and(|o| o.tab_id == tab_id) {
                *guard = None;
            }
            return;
        }
        if interacted || focused == Some(true) {
            *guard = Some(PinOwnerState {
                tab_id: tab_id.to_owned(),
                last_seen: now,
                visible,
                focused: focused.unwrap_or(true),
            });
            return;
        }
        if let Some(ref mut cur) = *guard
            && cur.tab_id == tab_id
        {
            cur.visible = visible;
            cur.last_seen = now;
            if let Some(f) = focused {
                cur.focused = f;
            }
        }
    }

    /// Updates the active map provider if `raw_provider` differs from the current one.
    /// Returns `Some((new_provider, had_standalone_lock))` when switched.
    fn update_active_provider(&self, raw_provider: Option<&str>) -> Option<(String, bool)> {
        let norm = raw_provider
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty() && s.len() <= 32)?;
        let mut guard = self
            .active_provider
            .lock()
            .expect("active_provider lock poisoned");
        if *guard == norm {
            return None;
        }
        *guard = norm.clone();
        drop(guard);
        let had_lock = self
            .nav_state
            .lock()
            .expect("nav_state lock poisoned")
            .controller
            .clear_for_provider_switch();
        Some((norm, had_lock))
    }

    fn current_provider(&self) -> String {
        self.active_provider
            .lock()
            .expect("active_provider lock poisoned")
            .clone()
    }
}

async fn notify_provider_if_switched(inner: &Arc<SyncInner>, raw_provider: Option<&str>) {
    let Some((new_prov, had_lock)) = inner.update_active_provider(raw_provider) else {
        return;
    };
    if had_lock {
        let _ = broadcast_inner(
            inner,
            json!({"type": "TARGET_LOCK_UPDATED", "locked_target": Value::Null}),
            false,
            false,
        );
    }
    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let _ = tokio::task::spawn_blocking(move || cb.on_active_provider_change(&new_prov)).await;
    }
}

/// Last Page Visibility report (`document.hidden`) of one HoYoLAB map tab.
#[derive(Debug, Clone, Copy)]
struct TabVisibility {
    visible: bool,
    ts: Instant,
}

/// `true` when at least one tab reported `visible` within `ttl_s` before `now`.
fn any_tab_visible_at(tabs: &HashMap<String, TabVisibility>, now: Instant, ttl_s: f64) -> bool {
    tabs.values()
        .any(|t| t.visible && now.saturating_duration_since(t.ts).as_secs_f64() <= ttl_s)
}

#[derive(Clone)]
pub struct SyncServer {
    inner: Arc<SyncInner>,
}

impl Default for SyncServer {
    fn default() -> Self {
        Self::new(SyncServerConfig::default())
    }
}

impl SyncServer {
    pub fn new(cfg: SyncServerConfig) -> Self {
        let lan = cfg.lan_companion.unwrap_or_else(|| {
            let host = crate::lan_companion::resolve_lan_bind_host(
                std::env::var("GH_LAN_BIND_HOST").ok().as_deref(),
            );
            Arc::new(LanCompanionServer::new(LanCompanionConfig {
                host,
                port: cfg.config.lan_companion_port,
                ..Default::default()
            }))
        });

        Self {
            inner: Arc::new(SyncInner {
                host: cfg.host,
                port: AtomicU16::new(cfg.port),
                poll_timeout: cfg.poll_timeout,
                is_running: AtomicBool::new(false),
                engine_active: AtomicBool::new(true),
                skip_exit_update: AtomicBool::new(false),
                config: RwLock::new(cfg.config),
                config_path: cfg.config_path,
                diagnostics_dir: cfg.diagnostics_dir,
                callbacks: RwLock::new(cfg.callbacks),
                conflict_advisor: RwLock::new(cfg.conflict_advisor),
                diagnostic_service: RwLock::new(cfg.diagnostic_service),
                lan_companion: lan,
                poll_state: Mutex::new(PollState::new()),
                poll_notify: Notify::new(),
                browser_state: Mutex::new(Map::new()),
                feedback_seen: Mutex::new(VecDeque::with_capacity(FEEDBACK_SEEN_MAXLEN)),
                tab_visibility: Mutex::new(HashMap::new()),
                pin_owner: Mutex::new(None),
                active_provider: Mutex::new("hoyolab".to_owned()),
                nav_state: Mutex::new(StandaloneNavState {
                    controller: NavigationController::new(),
                    route_legs: Vec::new(),
                    guide_open: false,
                    edit_mode: false,
                }),
                lifecycle: Mutex::new(ServerLifecycle {
                    shutdown_tx: None,
                    server_task: None,
                    bg_stop_tx: None,
                    bg_thread: None,
                }),
            }),
        }
    }

    pub fn host(&self) -> &str {
        &self.inner.host
    }

    pub fn port(&self) -> u16 {
        self.inner.port.load(Ordering::Acquire)
    }

    pub fn is_running(&self) -> bool {
        self.inner.is_running.load(Ordering::Acquire)
    }

    pub fn active_provider(&self) -> String {
        self.inner.current_provider()
    }

    /// Returns `true` when `/shutdown` was initiated by a native Setup/Uninstaller process that
    /// requested skipping transparent install-on-exit (`skip_exit_update` or `uninstall_extension`).
    pub fn should_skip_exit_update(&self) -> bool {
        self.inner.skip_exit_update.load(Ordering::Acquire)
    }

    pub fn set_engine_active(&self, active: bool) {
        self.inner.engine_active.store(active, Ordering::Release);
    }

    pub fn lan_companion(&self) -> &Arc<LanCompanionServer> {
        &self.inner.lan_companion
    }

    pub fn set_callbacks(&self, cb: Option<Arc<dyn ServerCallbacks>>) {
        *self.inner.callbacks.write().expect("callbacks lock poisoned") = cb;
    }

    pub fn set_conflict_advisor(&self, adv: Option<Arc<dyn ConflictAdvisor>>) {
        *self
            .inner
            .conflict_advisor
            .write()
            .expect("advisor lock poisoned") = adv;
    }

    pub fn set_diagnostic_service(&self, svc: Option<Arc<dyn DiagnosticService>>) {
        *self
            .inner
            .diagnostic_service
            .write()
            .expect("diagnostic lock poisoned") = svc;
    }

    pub fn update_config(&self, cfg: AppConfig) {
        *self.inner.config.write().expect("config lock poisoned") = cfg;
    }

    pub fn latest_seq(&self) -> u64 {
        self.inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned")
            .event_seq
    }

    pub fn last_sync_info(&self) -> Option<Value> {
        self.inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned")
            .last_sync_info
            .clone()
    }

    pub fn update_last_sync_map_name(&self, map_id: i32, map_name: &str) {
        let mut st = self
            .inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned");
        if let Some(Value::Object(ref mut obj)) = st.last_sync_info
            && obj.get("map_id").and_then(Value::as_i64) == Some(map_id as i64)
        {
            obj.insert("map_name".to_owned(), Value::String(map_name.to_owned()));
        }
    }

    pub fn last_browser_state(&self) -> Map<String, Value> {
        self.inner
            .browser_state
            .lock()
            .expect("browser_state lock poisoned")
            .clone()
    }

    pub fn has_active_client(&self, max_age_s: f64) -> bool {
        let st = self
            .inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned");
        if st.active_waiters > 0 {
            return true;
        }
        match st.last_client_poll_ts {
            Some(ts) => ts.elapsed().as_secs_f64() <= max_age_s,
            None => false,
        }
    }

    /// Whether the user can currently see a HoYoLAB map tab (fresh `visible` report from any
    /// tab). Drives who owns sync feedback: the web page when visible, the in-game HUD otherwise.
    pub fn any_map_tab_visible(&self) -> bool {
        let tabs = self
            .inner
            .tab_visibility
            .lock()
            .expect("tab_visibility lock poisoned");
        any_tab_visible_at(&tabs, Instant::now(), TAB_VISIBLE_TTL_SECS)
    }

    pub fn broadcast(&self, message: Value, feedback_handled: bool, ephemeral: bool) -> u64 {
        broadcast_inner(&self.inner, message, feedback_handled, ephemeral)
    }

    pub fn broadcast_ephemeral(&self, message: Value) -> u64 {
        broadcast_inner(&self.inner, message, false, true)
    }

    pub async fn start(&self) -> CoreResult<u16> {
        if self.is_running() {
            return Ok(self.port());
        }

        let target_port = self.port();
        let listener = bind_with_takeover(&self.inner.host, target_port).await?;
        let actual_port = listener.local_addr()?.port();
        self.inner.port.store(actual_port, Ordering::Release);

        {
            let mut st = self
                .inner
                .poll_state
                .lock()
                .expect("poll_state lock poisoned");
            st.shutdown_requested = false;
        }

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let app = build_router(Arc::clone(&self.inner));
        let make_svc = app.into_make_service_with_connect_info::<PeerAddr>();
        let wrapped_listener = OptionsPreListener::new(listener, shutdown_rx.clone());
        let server = axum::serve(wrapped_listener, make_svc).with_graceful_shutdown(async move {
            while !*shutdown_rx.borrow() {
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        });

        let task = tokio::spawn(async move {
            let _ = server.await;
        });

        {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            lc.shutdown_tx = Some(shutdown_tx);
            lc.server_task = Some(task);
        }
        self.inner.is_running.store(true, Ordering::Release);
        Ok(actual_port)
    }

    pub async fn stop(&self) {
        self.inner.is_running.store(false, Ordering::Release);

        {
            let mut st = self
                .inner
                .poll_state
                .lock()
                .expect("poll_state lock poisoned");
            st.shutdown_requested = true;
            st.event_queue.clear();
            st.event_log.clear();
            st.ephemeral_events.clear();
        }
        self.inner.poll_notify.notify_waiters();

        let (shutdown_tx, server_task) = {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            (lc.shutdown_tx.take(), lc.server_task.take())
        };
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(true);
        }
        if let Some(task) = server_task {
            let _ = task.await;
        }
    }

    /// Synchronous starter running an isolated Tokio runtime on a background thread.
    pub fn start_in_background(&self) -> CoreResult<u16> {
        if self.is_running() {
            return Ok(self.port());
        }

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<CoreResult<u16>>(1);
        let (bg_stop_tx, bg_stop_rx) = oneshot::channel::<()>();
        let srv_clone = self.clone();

        let join = std::thread::Builder::new()
            .name("GenshinHelper-SyncServer".to_owned())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = init_tx.send(Err(CoreError::Io(e)));
                        return;
                    }
                };
                rt.block_on(async move {
                    match srv_clone.start().await {
                        Ok(port) => {
                            let _ = init_tx.send(Ok(port));
                        }
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    }
                    let mut shutdown_rx_poll = {
                        // Wait until either bg_stop_rx triggers or /shutdown closes the server task
                        bg_stop_rx
                    };
                    let task_opt = {
                        let mut lc = srv_clone.inner.lifecycle.lock().expect("lifecycle lock poisoned");
                        lc.server_task.take()
                    };
                    if let Some(task) = task_opt {
                        tokio::select! {
                            _ = &mut shutdown_rx_poll => {
                                srv_clone.stop().await;
                            }
                            _ = task => {}
                        }
                    } else {
                        let _ = shutdown_rx_poll.await;
                        srv_clone.stop().await;
                    }
                });
            })
            .map_err(CoreError::Io)?;

        {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            lc.bg_stop_tx = Some(bg_stop_tx);
            lc.bg_thread = Some(join);
        }

        match init_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(port)) => Ok(port),
            Ok(Err(e)) => {
                self.stop_background();
                Err(e)
            }
            Err(_) => {
                self.stop_background();
                Err(CoreError::Sync("Timed out starting SyncServer".to_owned()))
            }
        }
    }

    pub fn stop_background(&self) {
        let (bg_stop_tx, bg_thread) = {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            (lc.bg_stop_tx.take(), lc.bg_thread.take())
        };
        if let Some(tx) = bg_stop_tx {
            let _ = tx.send(());
        }
        if let Some(thread) = bg_thread {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PeerAddr(SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, OptionsPreListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, OptionsPreListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

struct PrefixedStream {
    prefix: [u8; 8],
    prefix_len: usize,
    prefix_pos: usize,
    inner: TcpStream,
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.prefix_pos < self.prefix_len {
            let avail = &self.prefix[self.prefix_pos..self.prefix_len];
            let to_copy = avail.len().min(buf.remaining());
            buf.put_slice(&avail[..to_copy]);
            self.prefix_pos += to_copy;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

struct OptionsPreListener {
    local_addr: SocketAddr,
    rx: mpsc::Receiver<(PrefixedStream, SocketAddr)>,
}

impl OptionsPreListener {
    fn new(listener: TcpListener, mut shutdown_rx: watch::Receiver<bool>) -> Self {
        let local_addr = listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)));
        let (tx, rx) = mpsc::channel::<(PrefixedStream, SocketAddr)>(64);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((mut stream, addr)) = accepted else {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            continue;
                        };
                        let tx_conn = tx.clone();
                        tokio::spawn(async move {
                            let mut prefix = [0u8; 8];
                            let mut len = 0usize;
                            let total_deadline =
                                tokio::time::Instant::now() + Duration::from_millis(2000);
                            let mut is_options = false;

                            loop {
                                if len == 8 {
                                    is_options = prefix.eq_ignore_ascii_case(b"OPTIONS ");
                                    break;
                                }
                                let now = tokio::time::Instant::now();
                                if now >= total_deadline {
                                    break;
                                }
                                let read_budget =
                                    (total_deadline - now).min(Duration::from_millis(1000));
                                match tokio::time::timeout(
                                    read_budget,
                                    stream.read(&mut prefix[len..]),
                                )
                                .await
                                {
                                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                    Ok(Ok(n)) => {
                                        len += n;
                                        if !prefix[..len]
                                            .eq_ignore_ascii_case(&b"OPTIONS "[..len])
                                        {
                                            break;
                                        }
                                    }
                                }
                            }

                            if is_options {
                                handle_raw_options(stream, &prefix[..len]).await;
                            } else {
                                let prefixed = PrefixedStream {
                                    prefix,
                                    prefix_len: len,
                                    prefix_pos: 0,
                                    inner: stream,
                                };
                                let _ = tx_conn.send((prefixed, addr)).await;
                            }
                        });
                    }
                }
            }
        });

        Self { local_addr, rx }
    }
}

async fn handle_raw_options(mut stream: TcpStream, prefix: &[u8]) {
    let mut buf = Vec::with_capacity(1024);
    buf.extend_from_slice(prefix);
    let mut chunk = [0u8; 512];
    if !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while buf.len() < 32 * 1024 {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
        })
        .await;
    }

    const OPTIONS_RESP: &[u8] = b"HTTP/1.1 204 No Content\r\n\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
Access-Control-Allow-Headers: *\r\n\
Access-Control-Allow-Private-Network: true\r\n\
Connection: close\r\n\
Content-Length: 0\r\n\
\r\n";

    let _ = stream.write_all(OPTIONS_RESP).await;
    let _ = stream.shutdown().await;
}

impl axum::serve::Listener for OptionsPreListener {
    type Io = PrefixedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(pair) => pair,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

// ---------------------------------------------------------------------------
// Port Bind & WSAEADDRINUSE Takeover
// ---------------------------------------------------------------------------

async fn try_shutdown_existing(host: &str, port: u16) -> bool {
    let target_host = if host.trim().is_empty() || host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    };
    let req_future = async {
        let mut stream = TcpStream::connect((target_host, port)).await.ok()?;
        let req = format!(
            "POST /shutdown HTTP/1.1\r\nHost: {target_host}:{port}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.ok()?;
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some(code == 200)
    };

    matches!(
        tokio::time::timeout(Duration::from_millis(1500), req_future).await,
        Ok(Some(true))
    )
}

async fn bind_with_takeover(host: &str, port: u16) -> CoreResult<TcpListener> {
    let mut freed_existing = false;
    for attempt in 0..5usize {
        match TcpListener::bind((host, port)).await {
            Ok(listener) => return Ok(listener),
            Err(err) => {
                let is_addr_in_use =
                    err.kind() == ErrorKind::AddrInUse || err.raw_os_error() == Some(10048);
                if is_addr_in_use {
                    if !freed_existing && port != 0 {
                        freed_existing = true;
                        if try_shutdown_existing(host, port).await {
                            tokio::time::sleep(Duration::from_millis(450)).await;
                        }
                        continue;
                    }
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
                return Err(CoreError::Io(err));
            }
        }
    }
    unreachable!("loop always returns")
}

// ---------------------------------------------------------------------------
// Broadcast Engine
// ---------------------------------------------------------------------------

fn broadcast_inner(
    inner: &Arc<SyncInner>,
    message: Value,
    feedback_handled: bool,
    ephemeral: bool,
) -> u64 {
    if !inner.is_running.load(Ordering::Acquire) {
        return 0;
    }

    let mut stamped = match message {
        Value::Object(map) => map,
        other => {
            let mut m = Map::new();
            m.insert("data".to_owned(), other);
            m
        }
    };

    let seq = {
        let mut st = inner.poll_state.lock().expect("poll_state lock poisoned");
        st.event_seq += 1;
        let seq = st.event_seq;
        stamped.insert("_seq".to_owned(), json!(seq));

        let msg_type = stamped
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("UNKNOWN")
            .to_owned();

        let stamped_val = Value::Object(stamped);

        if ephemeral {
            st.ephemeral_events
                .insert(msg_type.clone(), (seq, stamped_val.clone()));
        } else {
            if st.event_log.len() >= EVENT_LOG_MAXLEN {
                st.event_log.pop_front();
            }
            st.event_log.push_back((seq, stamped_val.clone()));
        }

        if matches!(msg_type.as_str(), "SYNC_POSITION_ONLY" | "SYNC_AND_MARK")
            && stamped_val.get("has_world_coords").and_then(Value::as_bool) == Some(true)
        {
            let floor_str = stamped_val
                .get("hoyolab_floor_name")
                .or_else(|| stamped_val.get("floor_short"))
                .or_else(|| stamped_val.get("floor_name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("G");

            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();

            st.last_sync_info = Some(json!({
                "map_id": stamped_val.get("map_id").cloned().unwrap_or(Value::Null),
                "map_name": stamped_val.get("map_name").and_then(Value::as_str).unwrap_or(""),
                "area_name": stamped_val.get("area_name").and_then(Value::as_str).unwrap_or(""),
                "lat": stamped_val.get("lat").cloned().unwrap_or(Value::Null),
                "lng": stamped_val.get("lng").cloned().unwrap_or(Value::Null),
                "is_underground": stamped_val.get("is_underground").and_then(Value::as_bool).unwrap_or(false),
                "floor": floor_str,
                "ts": ts,
            }));
        }

        if feedback_handled && seq > 0 {
            let mut fb = inner
                .feedback_seen
                .lock()
                .expect("feedback_seen lock poisoned");
            if fb.len() >= FEEDBACK_SEEN_MAXLEN {
                fb.pop_front();
            }
            fb.push_back(FeedbackSeenEntry {
                seq: seq as i64,
                event_type: msg_type.clone(),
                status: "local_handled".to_owned(),
                ts: Instant::now(),
            });
        }

        if st.active_waiters == 0 {
            if ephemeral {
                let mut replaced = false;
                for existing in st.event_queue.iter_mut() {
                    if existing.get("type").and_then(Value::as_str) == Some(msg_type.as_str()) {
                        *existing = stamped_val.clone();
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    if st.event_queue.len() >= EVENT_QUEUE_MAXLEN {
                        st.event_queue.pop_front();
                    }
                    st.event_queue.push_back(stamped_val);
                }
            } else {
                if st.event_queue.len() >= EVENT_QUEUE_MAXLEN {
                    st.event_queue.pop_front();
                }
                st.event_queue.push_back(stamped_val);
            }
        }

        seq
    };

    inner.poll_notify.notify_waiters();
    seq
}

// ---------------------------------------------------------------------------
// Axum Router, CORS/PNA Middleware & Helpers
// ---------------------------------------------------------------------------

fn build_router(inner: Arc<SyncInner>) -> Router {
    Router::new()
        .route("/status", any(handle_status))
        .route("/set_engine_state", any(handle_set_engine_state))
        .route("/set_auto_sync", any(handle_set_auto_sync))
        .route("/set_single_monitor", any(handle_set_single_monitor))
        .route("/action_feedback", any(handle_action_feedback))
        .route("/tab_visibility", any(handle_tab_visibility))
        .route("/quick_switch", any(handle_quick_switch))
        .route("/shutdown", any(handle_shutdown))
        .route("/set_hotkey", any(handle_set_hotkey))
        .route("/set_hotkeys", any(handle_set_hotkey))
        .route("/check_hotkey_conflicts", any(handle_check_hotkey_conflicts))
        .route("/remap_external_hotkey", any(handle_remap_external_hotkey))
        .route("/restore_external_hotkey", any(handle_restore_external_hotkey))
        .route("/report_state", any(handle_report_state))
        .route("/browser_state", any(handle_browser_state))
        .route("/trigger_diagnostic", any(handle_trigger_diagnostic))
        .route("/save_diagnostic_note", any(handle_save_diagnostic_note))
        .route("/attach_diagnostic_browser", any(handle_attach_diagnostic_browser))
        .route("/open_diagnostics", any(handle_open_diagnostics))
        .route("/trigger_sync", any(handle_trigger_sync))
        .route("/set_language", any(handle_set_language))
        .route("/set_minimap_tracking", any(handle_set_minimap_tracking))
        .route("/set_nav_hud", any(handle_set_nav_hud))
        .route("/set_radar", any(handle_set_radar))
        .route("/set_audio", any(handle_set_audio))
        .route("/nearby_pins", any(handle_nearby_pins))
        .route("/nav_command", any(handle_nav_command))
        .route("/route", any(handle_route))
        .route("/plan_tsp_route", any(handle_plan_tsp_route))
        .route("/lan_companion", any(handle_lan_companion))
        .route("/lan_pins", any(handle_lan_pins))
        .route("/launch_borderless", any(handle_launch_borderless))
        .route("/cross_map_export", any(handle_cross_map_export))
        .route("/calibrate_provider", any(handle_calibrate_provider))
        .route("/cross_map_import_preview", any(handle_cross_map_import_preview))
        .route("/cross_map_import_history", any(handle_cross_map_import_history))
        .route("/app_update", any(handle_app_update))
        .route("/poll", any(handle_poll))
        .fallback(handle_not_found)
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .layer(middleware::from_fn(cors_pna_middleware))
        .with_state(inner)
}

fn attach_cors_pna_headers(resp: &mut Response) {
    let headers = resp.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    headers.insert(HEADER_ALLOW_PRIVATE_NETWORK, HeaderValue::from_static("true"));
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
}

async fn cors_pna_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        resp.headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        attach_cors_pna_headers(&mut resp);
        return resp;
    }

    let mut resp = next.run(req).await;
    attach_cors_pna_headers(&mut resp);
    resp
}

fn json_response(status: StatusCode, payload: &Value) -> Response {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

fn parse_lenient_json_map(bytes: &Bytes) -> Map<String, Value> {
    if bytes.is_empty() {
        return Map::new();
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn mark_client_poll(inner: &SyncInner) {
    let mut st = inner.poll_state.lock().expect("poll_state lock poisoned");
    st.last_client_poll_ts = Some(Instant::now());
}

fn json_truthy(v: Option<&Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn json_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn json_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            .or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn json_str_trimmed(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

fn save_standalone_config(inner: &SyncInner, cfg: &AppConfig) {
    if let Some(ref path) = inner.config_path {
        let _ = cfg.save_to_file(path);
    }
}

fn push_standalone_lan_snapshot(inner: &SyncInner, force: bool) {
    if !inner.lan_companion.is_running() {
        return;
    }
    let payload = {
        let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
        let cur_target = nav
            .controller
            .current_target()
            .map(NavPin::to_wire_json)
            .unwrap_or(Value::Null);
        let excluded = nav.controller.excluded_ids();
        let nearby: Vec<Value> = nav
            .controller
            .pins()
            .into_iter()
            .filter(|p| !excluded.contains(&p.id))
            .take(50)
            .map(NavPin::to_wire_json)
            .collect();
        json!({
            "target_pin": cur_target,
            "nearby_pins": nearby,
            "route_legs": nav.route_legs,
        })
    };
    inner.lan_companion.broadcast_state(payload, force);
}

async fn handle_not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        &json!({"ok": false, "error": "Not Found"}),
    )
}

// ---------------------------------------------------------------------------
// Route Handlers (27 logical routes / 28 URL paths)
// ---------------------------------------------------------------------------

async fn handle_status(State(inner): State<Arc<SyncInner>>) -> Response {
    mark_client_poll(&inner);

    let (queue_len, latest_seq, last_sync) = {
        let st = inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned");
        (st.event_queue.len(), st.event_seq, st.last_sync_info.clone())
    };
    let engine_active = inner.engine_active.load(Ordering::Acquire);
    let lan_running = inner.lan_companion.is_running();
    let edit_mode = inner
        .nav_state
        .lock()
        .expect("nav_state lock poisoned")
        .edit_mode;

    let mut status = {
        let cfg = inner.config.read().expect("config lock poisoned");
        StatusResponse::from_config(&cfg)
    };
    status.queue_len = queue_len;
    status.latest_seq = latest_seq;
    status.engine_active = engine_active;
    status.lan_companion_running = lan_running;
    status.nav_hud_edit_mode = edit_mode;
    status.last_sync = last_sync.clone();

    let resolved_lang = resolve_language(Some(&status.language_setting), None);
    status.language = resolved_lang.to_owned();

    let mut out_map = match serde_json::to_value(&status) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let overrides = tokio::task::spawn_blocking(move || cb.get_status_overrides())
            .await
            .unwrap_or(Value::Null);
        if let Value::Object(ov_map) = overrides {
            for (k, v) in ov_map {
                if (k == "queue_len" || k == "latest_seq")
                    || (k == "last_sync" && v.is_null() && last_sync.is_some())
                {
                    continue;
                }
                out_map.insert(k, v);
            }
        }
    }

    json_response(StatusCode::OK, &Value::Object(out_map))
}

async fn handle_set_engine_state(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let mut active = json_truthy(req.get("active"), true);

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt
        && let Ok(Ok(res)) =
            tokio::task::spawn_blocking(move || cb.on_engine_state_change(active)).await
    {
        active = res;
    }
    inner.engine_active.store(active, Ordering::Release);

    broadcast_inner(
        &inner,
        json!({"type": "ENGINE_STATE_UPDATED", "engine_active": active}),
        false,
        false,
    );
    json_response(StatusCode::OK, &json!({"ok": true, "engine_active": active}))
}

async fn handle_set_auto_sync(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    let req = parse_lenient_json_map(&body);
    let mut enabled = json_truthy(req.get("auto_sync_m"), true);

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        if let Ok(Ok(res)) =
            tokio::task::spawn_blocking(move || cb.on_auto_sync_change(enabled)).await
        {
            enabled = res;
        }
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        cfg.auto_sync_m = enabled;
        save_standalone_config(&inner, &cfg);
    }

    broadcast_inner(
        &inner,
        json!({"type": "AUTO_SYNC_UPDATED", "auto_sync_m": enabled}),
        false,
        false,
    );
    json_response(StatusCode::OK, &json!({"ok": true, "auto_sync_m": enabled}))
}

async fn handle_set_single_monitor(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    let req = parse_lenient_json_map(&body);
    let mut enabled = json_truthy(req.get("single_monitor_mode"), false);

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        if let Ok(Ok(res)) =
            tokio::task::spawn_blocking(move || cb.on_single_monitor_change(enabled)).await
        {
            enabled = res;
        }
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        cfg.single_monitor_mode = enabled;
        save_standalone_config(&inner, &cfg);
    }

    broadcast_inner(
        &inner,
        json!({"type": "SINGLE_MONITOR_UPDATED", "single_monitor_mode": enabled}),
        false,
        false,
    );
    json_response(
        StatusCode::OK,
        &json!({"ok": true, "single_monitor_mode": enabled}),
    )
}

/// `POST /tab_visibility {tab_id, visible}` — Page Visibility heartbeat from each HoYoLAB tab.
async fn handle_tab_visibility(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let tab_id = json_str_trimmed(req.get("tab_id"));
    if tab_id.is_empty() || tab_id.len() > 64 {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Missing or invalid tab_id"}),
        );
    }
    let visible = json_truthy(req.get("visible"), false);
    let focused = req.get("focused").and_then(Value::as_bool);
    let interacted = req.get("interacted").and_then(Value::as_bool).unwrap_or(false);
    let closing = req.get("closing").and_then(Value::as_bool).unwrap_or(false);
    let now = Instant::now();
    inner.update_pin_owner_from_visibility(&tab_id, visible, focused, interacted, closing, now);
    let any_visible = {
        let mut tabs = inner
            .tab_visibility
            .lock()
            .expect("tab_visibility lock poisoned");
        tabs.insert(tab_id, TabVisibility { visible, ts: now });
        tabs.retain(|_, t| now.saturating_duration_since(t.ts).as_secs_f64() <= TAB_VISIBLE_TTL_SECS * 6.0);
        while tabs.len() > TAB_VISIBILITY_MAXLEN {
            let Some(oldest) = tabs
                .iter()
                .min_by_key(|(_, t)| t.ts)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            tabs.remove(&oldest);
        }
        any_tab_visible_at(&tabs, now, TAB_VISIBLE_TTL_SECS)
    };
    json_response(
        StatusCode::OK,
        &json!({"ok": true, "visible": visible, "any_visible": any_visible}),
    )
}

async fn handle_action_feedback(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_map = parse_lenient_json_map(&body);
    let req_val = Value::Object(req_map.clone());

    let event_type = json_str_trimmed(req_map.get("event_type"));
    let status = json_str_trimmed(req_map.get("status"));
    let seq = json_i64(req_map.get("seq")).unwrap_or(0);
    let action = json_str_trimmed(req_map.get("action"));
    let pin_id = json_str_trimmed(req_map.get("pin_id"));

    let now = Instant::now();
    let mut deduped = false;
    if seq > 0 {
        let mut seen = inner
            .feedback_seen
            .lock()
            .expect("feedback_seen lock poisoned");
        for entry in seen.iter() {
            if now.saturating_duration_since(entry.ts).as_secs_f64() > FEEDBACK_DEDUP_WINDOW_SECS
                || entry.seq != seq
            {
                continue;
            }
            if entry.status == status
                || (event_type == "SYNC_AND_MARK" && entry.event_type == "SYNC_AND_MARK")
            {
                deduped = true;
                break;
            }
        }
        if !deduped {
            if seen.len() >= FEEDBACK_SEEN_MAXLEN {
                seen.pop_front();
            }
            seen.push_back(FeedbackSeenEntry {
                seq,
                event_type: event_type.clone(),
                status: status.clone(),
                ts: now,
            });
        }
    }

    let mut shown = false;
    if !deduped {
        let cb_opt = inner
            .callbacks
            .read()
            .expect("callbacks lock poisoned")
            .clone();
        if let Some(cb) = cb_opt {
            if let Ok(Ok(res)) =
                tokio::task::spawn_blocking(move || cb.on_action_feedback(&req_val)).await
            {
                shown = match res {
                    Value::Bool(b) => b,
                    Value::Object(m) => m.get("shown").and_then(Value::as_bool).unwrap_or(false),
                    _ => false,
                };
            }
        } else if event_type.eq_ignore_ascii_case("NAV_MARK") {
            let st_lower = status.to_ascii_lowercase();
            if matches!(st_lower.as_str(), "error" | "no_nearby" | "not_found") {
                let unmark = action.eq_ignore_ascii_case("unmark");
                if !unmark && !pin_id.is_empty() {
                    let restored = {
                        let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                        nav.controller.rollback_collected(&pin_id).is_some()
                    };
                    if restored {
                        push_standalone_lan_snapshot(&inner, true);
                    }
                }
                shown = true;
            }
        }
    }

    json_response(
        StatusCode::OK,
        &json!({"ok": true, "shown": shown, "deduped": deduped}),
    )
}

async fn handle_quick_switch(State(inner): State<Arc<SyncInner>>) -> Response {
    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let target = if let Some(cb) = cb_opt {
        tokio::task::spawn_blocking(move || cb.on_quick_switch())
            .await
            .ok()
            .and_then(|r| r.ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "none".to_owned())
    } else {
        "none".to_owned()
    };

    if target == "browser" {
        broadcast_inner(&inner, json!({"type": "FOCUS_MAP_TAB"}), false, false);
    }

    json_response(StatusCode::OK, &json!({"ok": true, "target": target}))
}

async fn handle_shutdown(
    State(inner): State<Arc<SyncInner>>,
    ConnectInfo(PeerAddr(addr)): ConnectInfo<PeerAddr>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method != Method::POST || !addr.ip().is_loopback() {
        return json_response(
            StatusCode::FORBIDDEN,
            &json!({"ok": false, "error": "Forbidden"}),
        );
    }

    // Only accept `uninstall_extension` / `skip_exit_update` from non-browser native loopback
    // callers (no Origin header, e.g. Inno Setup Setup/Uninstaller via WinHTTP) to prevent web CSRF.
    let is_native_caller = !headers.contains_key(ORIGIN);
    let body_json = if is_native_caller {
        serde_json::from_slice::<Value>(&body).ok()
    } else {
        None
    };
    let request_ext_uninstall = body_json
        .as_ref()
        .and_then(|v| v.get("uninstall_extension").and_then(Value::as_bool))
        .unwrap_or(false);
    let request_skip_exit_update = request_ext_uninstall
        || body_json
            .as_ref()
            .and_then(|v| v.get("skip_exit_update").and_then(Value::as_bool))
            .unwrap_or(false);

    if request_skip_exit_update {
        inner.skip_exit_update.store(true, Ordering::Release);
    }
    if request_ext_uninstall {
        broadcast_inner(
            &inner,
            json!({"type": "UNINSTALL_EXTENSION", "source": "uninstaller"}),
            false,
            false,
        );
    }

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let inner_clone = Arc::clone(&inner);
    let shutdown_delay_ms = if request_ext_uninstall { 180 } else { 10 };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(shutdown_delay_ms)).await;
        if let Some(cb) = cb_opt {
            let _ = tokio::task::spawn_blocking(move || cb.on_shutdown()).await;
        }
        inner_clone.is_running.store(false, Ordering::Release);
        {
            let mut st = inner_clone
                .poll_state
                .lock()
                .expect("poll_state lock poisoned");
            st.shutdown_requested = true;
        }
        inner_clone.poll_notify.notify_waiters();
        let tx = {
            inner_clone
                .lifecycle
                .lock()
                .expect("lifecycle lock poisoned")
                .shutdown_tx
                .take()
        };
        if let Some(shutdown_tx) = tx {
            let _ = shutdown_tx.send(true);
        }
    });

    let payload = if request_ext_uninstall {
        json!({
            "ok": true,
            "shutting_down": true,
            "uninstall_extension_sent": true,
        })
    } else {
        json!({"ok": true, "shutting_down": true})
    };
    json_response(StatusCode::OK, &payload)
}

fn parse_query_params(raw_query: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(qs) = raw_query else {
        return out;
    };
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let k_dec = percent_decode_form(k);
        let v_dec = percent_decode_form(v);
        out.insert(k_dec, v_dec);
    }
    out
}

fn percent_decode_form(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(d1), Some(d2)) = (h1, h2) {
                out.push(((d1 << 4) | d2) as u8);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn apply_standalone_hotkey(cfg: &mut AppConfig, action: &str, new_key: &str) -> (bool, String) {
    let norm = new_key.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return (false, String::new());
    }
    match action {
        "sync" => {
            cfg.hotkey = norm.clone();
            cfg.hotkey_sync = norm.clone();
        }
        "debug" => cfg.hotkey_debug = norm.clone(),
        "switch" => cfg.hotkey_switch = norm.clone(),
        "nav_mark" => cfg.hotkey_nav_mark = norm.clone(),
        "nav_undo" => cfg.hotkey_nav_undo = norm.clone(),
        "nav_guide" => cfg.hotkey_nav_guide = norm.clone(),
        "nav_next" => cfg.hotkey_nav_next = norm.clone(),
        "nav_layout" => cfg.hotkey_nav_layout = norm.clone(),
        "nav_toggle" => cfg.hotkey_nav_toggle = norm.clone(),
        "nav_unlock" => cfg.hotkey_nav_unlock = norm.clone(),
        "radar_toggle" => cfg.hotkey_radar_toggle = norm.clone(),
        _ => return (false, String::new()),
    }
    (true, format_hotkey_display_name(&norm))
}

async fn handle_set_hotkey(
    State(inner): State<Arc<SyncInner>>,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    let mut req_map = parse_lenient_json_map(&body);
    if req_map.is_empty() {
        for (k, v) in parse_query_params(raw_query.as_deref()) {
            req_map.insert(k, Value::String(v.trim().to_owned()));
        }
    }

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();

    let mut results = Map::new();
    if let Some(cb) = cb_opt {
        let req_val = Value::Object(req_map.clone());
        if let Ok(Ok(cb_res)) =
            tokio::task::spawn_blocking(move || cb.on_set_hotkeys(&req_val)).await
            && let Some(res_obj) = cb_res
                .get("results")
                .and_then(Value::as_object)
                .or_else(|| cb_res.as_object())
        {
            for (act, entry) in res_obj {
                if entry.is_object() {
                    results.insert(act.clone(), entry.clone());
                    if entry.get("ok").and_then(Value::as_bool) == Some(true) {
                        broadcast_inner(
                            &inner,
                            json!({
                                "type": "HOTKEY_UPDATED",
                                "action": act,
                                "hotkey": entry.get("hotkey").and_then(Value::as_str).unwrap_or(""),
                                "display_name": entry.get("display_name").and_then(Value::as_str).unwrap_or(""),
                            }),
                            false,
                            false,
                        );
                    }
                }
            }
        }
    } else {
        let has_batch = HOTKEY_ACTIONS
            .iter()
            .any(|act| req_map.contains_key(&format!("hotkey_{act}")));
        let mut updated_any = false;
        let mut cfg = inner.config.write().expect("config lock poisoned");

        if has_batch {
            for act in HOTKEY_ACTIONS {
                let kname = format!("hotkey_{act}");
                let val_str = json_str_trimmed(req_map.get(&kname)).to_ascii_lowercase();
                if val_str.is_empty() {
                    continue;
                }
                let (ok, dname) = apply_standalone_hotkey(&mut cfg, act, &val_str);
                results.insert(
                    act.to_owned(),
                    json!({"ok": ok, "hotkey": val_str, "display_name": dname}),
                );
                if ok {
                    updated_any = true;
                    broadcast_inner(
                        &inner,
                        json!({
                            "type": "HOTKEY_UPDATED",
                            "action": act,
                            "hotkey": val_str,
                            "display_name": dname,
                        }),
                        false,
                        false,
                    );
                }
            }
        } else if req_map.contains_key("action") && req_map.contains_key("hotkey") {
            let act = json_str_trimmed(req_map.get("action")).to_ascii_lowercase();
            let new_k = json_str_trimmed(req_map.get("hotkey")).to_ascii_lowercase();
            let (ok, dname) = apply_standalone_hotkey(&mut cfg, &act, &new_k);
            results.insert(
                act.clone(),
                json!({"ok": ok, "hotkey": new_k, "display_name": dname}),
            );
            if ok {
                updated_any = true;
                broadcast_inner(
                    &inner,
                    json!({
                        "type": "HOTKEY_UPDATED",
                        "action": act,
                        "hotkey": new_k,
                        "display_name": dname,
                    }),
                    false,
                    false,
                );
            }
        } else if req_map.contains_key("hotkey") {
            let new_k = json_str_trimmed(req_map.get("hotkey")).to_ascii_lowercase();
            let (ok, dname) = apply_standalone_hotkey(&mut cfg, "sync", &new_k);
            results.insert(
                "sync".to_owned(),
                json!({"ok": ok, "hotkey": new_k, "display_name": dname}),
            );
            if ok {
                updated_any = true;
                broadcast_inner(
                    &inner,
                    json!({
                        "type": "HOTKEY_UPDATED",
                        "action": "sync",
                        "hotkey": new_k,
                        "display_name": dname,
                    }),
                    false,
                    false,
                );
            }
        }

        if updated_any {
            save_standalone_config(&inner, &cfg);
        }
    }

    json_response(StatusCode::OK, &json!({"ok": true, "results": results}))
}

async fn handle_check_hotkey_conflicts(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_map = parse_lenient_json_map(&body);
    let req_val = Value::Object(req_map.clone());

    let adv_opt = inner
        .conflict_advisor
        .read()
        .expect("advisor lock poisoned")
        .clone();
    let payload = if let Some(adv) = adv_opt {
        match tokio::task::spawn_blocking(move || adv.check_conflicts(&req_val)).await {
            Ok(Ok(Value::Object(m))) => Value::Object(m),
            Ok(Ok(_)) => json!({"ok": false, "error": "Invalid conflict result"}),
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    } else {
        let single_hk = json_str_trimmed(req_map.get("hotkey"));
        if !single_hk.is_empty() {
            json!({
                "ok": true,
                "single": {
                    "hotkey": single_hk,
                    "severity": "none",
                    "conflicts": [],
                },
                "active_backups": [],
            })
        } else {
            let mut actions = Map::new();
            for act in HOTKEY_ACTIONS {
                actions.insert(
                    act.to_owned(),
                    json!({"severity": "none", "conflicts": []}),
                );
            }
            json!({
                "ok": true,
                "actions": actions,
                "active_backups": [],
            })
        }
    };

    json_response(StatusCode::OK, &payload)
}

async fn handle_remap_external_hotkey(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_val = Value::Object(parse_lenient_json_map(&body));

    let adv_opt = inner
        .conflict_advisor
        .read()
        .expect("advisor lock poisoned")
        .clone();
    let payload = if let Some(adv) = adv_opt {
        match tokio::task::spawn_blocking(move || adv.remap_external(&req_val)).await {
            Ok(Ok(Value::Object(m))) => Value::Object(m),
            Ok(Ok(_)) => json!({"ok": false, "error": "Invalid remap result"}),
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    } else {
        json!({"ok": false, "error": "External hotkey remap not configured"})
    };

    let ok = payload.get("ok").and_then(Value::as_bool) == Some(true);
    if ok && let Some(cu) = payload.get("claim_updated").and_then(Value::as_object) {
        broadcast_inner(
            &inner,
            json!({
                "type": "HOTKEY_UPDATED",
                "action": cu.get("action").and_then(Value::as_str).unwrap_or(""),
                "hotkey": cu.get("hotkey").and_then(Value::as_str).unwrap_or(""),
                "display_name": cu.get("display_name").and_then(Value::as_str).unwrap_or(""),
            }),
            false,
            false,
        );
    }

    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(status, &payload)
}

async fn handle_restore_external_hotkey(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_val = Value::Object(parse_lenient_json_map(&body));

    let adv_opt = inner
        .conflict_advisor
        .read()
        .expect("advisor lock poisoned")
        .clone();
    let payload = if let Some(adv) = adv_opt {
        match tokio::task::spawn_blocking(move || adv.restore_external(&req_val)).await {
            Ok(Ok(Value::Object(m))) => Value::Object(m),
            Ok(Ok(_)) => json!({"ok": false, "error": "Invalid restore result"}),
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    } else {
        json!({"ok": false, "error": "External hotkey restore not configured"})
    };

    let status = if payload.get("ok").and_then(Value::as_bool) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(status, &payload)
}

async fn handle_report_state(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_map = parse_lenient_json_map(&body);
    let tab_id = req_map
        .get("tab_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let visible = req_map.get("tab_visible").and_then(Value::as_bool);
    let focused = req_map.get("tab_focused").and_then(Value::as_bool);
    if !inner.claim_or_check_pin_owner(tab_id, visible, focused, false, Instant::now()) {
        return json_response(StatusCode::OK, &json!({"ignored_secondary_client": true}));
    }
    let provider_id = req_map
        .get("provider_id")
        .or_else(|| req_map.get("provider"))
        .and_then(Value::as_str);
    notify_provider_if_switched(&inner, provider_id).await;
    if !req_map.is_empty() {
        let mut st = inner
            .browser_state
            .lock()
            .expect("browser_state lock poisoned");
        for (k, v) in req_map {
            st.insert(k, v);
        }
    }
    json_response(StatusCode::OK, &json!({}))
}

async fn handle_browser_state(State(inner): State<Arc<SyncInner>>) -> Response {
    let st = inner
        .browser_state
        .lock()
        .expect("browser_state lock poisoned")
        .clone();
    json_response(StatusCode::OK, &Value::Object(st))
}

async fn handle_trigger_diagnostic(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    let req = parse_lenient_json_map(&body);
    let req_val = Value::Object(req);

    let svc_opt = inner
        .diagnostic_service
        .read()
        .expect("diagnostic lock poisoned")
        .clone();
    let bundle_res = if let Some(svc) = svc_opt {
        match tokio::task::spawn_blocking(move || svc.trigger_diagnostic_ex(&req_val)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => json!({"success": false, "error": e.to_string()}),
            Err(e) => json!({"success": false, "error": e.to_string()}),
        }
    } else {
        json!({"success": false, "error": "DiagnosticService not configured"})
    };

    json_response(StatusCode::OK, &json!({"ok": true, "bundle": bundle_res}))
}

const MAX_DIAG_BROWSER_PNG_BYTES: usize = 16 * 1024 * 1024;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub fn decode_png_base64(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim();
    let b64_part = if let Some((prefix, rest)) = trimmed.split_once(',') {
        if prefix.to_ascii_lowercase().starts_with("data:image/png;base64") {
            rest.trim()
        } else {
            return None;
        }
    } else {
        trimmed
    };
    if b64_part.is_empty() {
        return None;
    }

    let mut clean: Vec<u8> = Vec::with_capacity(b64_part.len());
    for &b in b64_part.as_bytes() {
        if !b.is_ascii_whitespace() {
            clean.push(b);
        }
    }
    if clean.is_empty() || !clean.len().is_multiple_of(4) {
        return None;
    }

    let max_decoded = (clean.len() / 4) * 3;
    if max_decoded > MAX_DIAG_BROWSER_PNG_BYTES + 4 {
        return None;
    }

    let mut out = Vec::with_capacity(max_decoded);
    let mut i = 0usize;
    while i < clean.len() {
        let mut vals = [0u32; 4];
        let mut pads = 0usize;
        for j in 0..4 {
            let ch = clean[i + j];
            if ch == b'=' {
                if i + j < clean.len() - 2 {
                    return None;
                }
                pads += 1;
                vals[j] = 0;
            } else {
                if pads > 0 {
                    return None;
                }
                vals[j] = match ch {
                    b'A'..=b'Z' => (ch - b'A') as u32,
                    b'a'..=b'z' => (ch - b'a' + 26) as u32,
                    b'0'..=b'9' => (ch - b'0' + 52) as u32,
                    b'+' | b'-' => 62,
                    b'/' | b'_' => 63,
                    _ => return None,
                };
            }
        }
        let triple = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push(((triple >> 16) & 0xFF) as u8);
        if pads < 2 {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if pads == 0 {
            out.push((triple & 0xFF) as u8);
        }
        i += 4;
    }

    if out.len() > MAX_DIAG_BROWSER_PNG_BYTES || !out.starts_with(&PNG_MAGIC) {
        return None;
    }
    Some(out)
}

async fn handle_attach_diagnostic_browser(
    State(inner): State<Arc<SyncInner>>,
    ConnectInfo(PeerAddr(addr)): ConnectInfo<PeerAddr>,
    method: Method,
    body: Bytes,
) -> Response {
    if method != Method::POST || !addr.ip().is_loopback() {
        return json_response(
            StatusCode::FORBIDDEN,
            &json!({"ok": false, "error": "Forbidden"}),
        );
    }

    let req = parse_lenient_json_map(&body);
    let folder_name = json_str_trimmed(req.get("folder_name"));
    if !is_valid_diag_folder_name(&folder_name) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Invalid diagnostic folder name"}),
        );
    }

    let raw_b64 = req
        .get("png_base64")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(png_bytes) = decode_png_base64(raw_b64) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Invalid PNG base64 payload"}),
        );
    };

    let svc_opt = inner
        .diagnostic_service
        .read()
        .expect("diagnostic lock poisoned")
        .clone();
    let Some(svc) = svc_opt else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"ok": false, "error": "DiagnosticService not configured"}),
        );
    };

    let res = tokio::task::spawn_blocking(move || {
        svc.attach_diagnostic_browser(&folder_name, &png_bytes)
    })
    .await;

    match res {
        Ok(Ok(val)) => json_response(StatusCode::OK, &val),
        Ok(Err(CoreError::NotFound(msg))) => {
            json_response(StatusCode::NOT_FOUND, &json!({"ok": false, "not_found": true, "error": msg}))
        }
        Ok(Err(CoreError::InvalidArgument(msg))) => {
            json_response(StatusCode::BAD_REQUEST, &json!({"ok": false, "error": msg}))
        }
        Ok(Err(e)) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"ok": false, "error": e.to_string()}),
        ),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"ok": false, "error": e.to_string()}),
        ),
    }
}

async fn handle_open_diagnostics(
    State(inner): State<Arc<SyncInner>>,
    ConnectInfo(PeerAddr(addr)): ConnectInfo<PeerAddr>,
    method: Method,
    body: Bytes,
) -> Response {
    if method != Method::POST || !addr.ip().is_loopback() {
        return json_response(
            StatusCode::FORBIDDEN,
            &json!({"ok": false, "error": "Forbidden"}),
        );
    }

    let req = parse_lenient_json_map(&body);
    let folder_name = json_str_trimmed(req.get("folder_name"));
    let ensure_zip = req
        .get("ensure_zip")
        .or_else(|| req.get("create_zip"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let folder_opt = if folder_name.is_empty() {
        None
    } else {
        if !is_valid_diag_folder_name(&folder_name) {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"ok": false, "error": "Invalid diagnostic folder name"}),
            );
        }
        Some(folder_name)
    };

    let svc_opt = inner
        .diagnostic_service
        .read()
        .expect("diagnostic lock poisoned")
        .clone();
    let Some(svc) = svc_opt else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"ok": false, "error": "DiagnosticService not configured"}),
        );
    };

    let res = tokio::task::spawn_blocking(move || {
        svc.open_diagnostics_folder_ex(folder_opt.as_deref(), ensure_zip)
    })
    .await;

    match res {
        Ok(Ok(val)) => json_response(StatusCode::OK, &val),
        Ok(Err(CoreError::NotFound(msg))) => {
            json_response(StatusCode::NOT_FOUND, &json!({"ok": false, "not_found": true, "error": msg}))
        }
        Ok(Err(CoreError::InvalidArgument(msg))) => {
            json_response(StatusCode::BAD_REQUEST, &json!({"ok": false, "error": msg}))
        }
        Ok(Err(e)) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"ok": false, "error": e.to_string()}),
        ),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"ok": false, "error": e.to_string()}),
        ),
    }
}

fn is_valid_diag_folder_name(name: &str) -> bool {
    if name.is_empty()
        || !name.starts_with("diag_")
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
    {
        return false;
    }
    Path::new(name).file_name() == Some(OsStr::new(name))
}

fn save_diagnostic_note_standalone(
    diag_dir: Option<&Path>,
    folder_name: &str,
    note: &str,
) -> Value {
    let Some(root) = diag_dir else {
        return json!({
            "ok": false,
            "not_found": true,
            "error": format!("Diagnostic folder not found: {folder_name}"),
        });
    };

    let bundle_dir = root.join(folder_name);
    if !bundle_dir.is_dir() {
        return json!({
            "ok": false,
            "not_found": true,
            "error": format!("Diagnostic folder not found: {folder_name}"),
        });
    }

    let note_clean = note.trim();
    let note_path = bundle_dir.join("05_user_note.txt");
    let content = if note_clean.is_empty() {
        String::new()
    } else {
        format!("{note_clean}\n")
    };
    if let Err(e) = write_str_atomic(&note_path, &content) {
        return json!({"ok": false, "error": format!("Failed to write user note: {e}")});
    }

    let report_path = bundle_dir.join("03_report.json");
    if report_path.is_file()
        && let Ok(raw) = std::fs::read_to_string(&report_path)
        && let Ok(Value::Object(mut report_obj)) = serde_json::from_str::<Value>(&raw)
    {
        report_obj.insert("user_note".to_owned(), Value::String(note_clean.to_owned()));
        let _ = write_json_atomic(&report_path, &Value::Object(report_obj));
    }

    json!({
        "ok": true,
        "folder_name": folder_name,
        "note_path": note_path.to_string_lossy().into_owned(),
        "user_note": note_clean,
    })
}

async fn handle_save_diagnostic_note(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    let req = parse_lenient_json_map(&body);
    let folder_name = json_str_trimmed(req.get("folder_name"));
    let note = req
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let create_zip = req
        .get("create_zip")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if !is_valid_diag_folder_name(&folder_name) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Invalid diagnostic folder name"}),
        );
    }

    let svc_opt = inner
        .diagnostic_service
        .read()
        .expect("diagnostic lock poisoned")
        .clone();
    let note_res = if let Some(svc) = svc_opt {
        let fname = folder_name.clone();
        let n = note.clone();
        match tokio::task::spawn_blocking(move || svc.save_diagnostic_note_ex(&fname, &n, create_zip)).await {
            Ok(Ok(v)) => v,
            Ok(Err(CoreError::NotFound(msg))) => {
                json!({"ok": false, "not_found": true, "error": msg})
            }
            Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    } else {
        let diag_dir = inner.diagnostics_dir.clone();
        let fname = folder_name.clone();
        tokio::task::spawn_blocking(move || {
            save_diagnostic_note_standalone(diag_dir.as_deref(), &fname, &note)
        })
        .await
        .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}))
    };

    let status = if note_res.get("ok").and_then(Value::as_bool) == Some(true) {
        StatusCode::OK
    } else if note_res.get("not_found").and_then(Value::as_bool) == Some(true) {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };

    json_response(status, &note_res)
}

async fn handle_trigger_sync(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    let mut payload = Map::new();
    payload.insert("type".to_owned(), Value::String("SYNC_AND_MARK".to_owned()));
    for (k, v) in parse_lenient_json_map(&body) {
        payload.insert(k, v);
    }
    let val = Value::Object(payload);
    broadcast_inner(&inner, val.clone(), false, false);
    json_response(StatusCode::OK, &json!({"ok": true, "broadcasted": val}))
}

async fn handle_set_language(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let raw_lang = {
        let s = json_str_trimmed(req.get("language")).to_ascii_lowercase();
        if s.is_empty() { "auto".to_owned() } else { s }
    };

    if !matches!(raw_lang.as_str(), "vi" | "en" | "auto") {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": format!("Invalid language {raw_lang:?}; expected 'vi', 'en', or 'auto'")}),
        );
    }

    let mut language = raw_lang.clone();
    let mut resolved = resolve_language(Some(&language), None).to_owned();

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let lang_in = raw_lang.clone();
        if let Ok(Ok(cb_res)) =
            tokio::task::spawn_blocking(move || cb.on_set_language(&lang_in)).await
        {
            match cb_res {
                Value::Object(m) => {
                    if let Some(l) = m.get("language").and_then(Value::as_str)
                        && !l.trim().is_empty()
                    {
                        language = l.trim().to_ascii_lowercase();
                    }
                    if let Some(r) = m.get("resolved").and_then(Value::as_str)
                        && !r.trim().is_empty()
                    {
                        resolved = r.trim().to_ascii_lowercase();
                    } else {
                        resolved = resolve_language(Some(&language), None).to_owned();
                    }
                }
                Value::String(s) if !s.trim().is_empty() => {
                    language = s.trim().to_ascii_lowercase();
                    resolved = resolve_language(Some(&language), None).to_owned();
                }
                _ => {}
            }
        }
    } else {
        resolved = set_language(Some(&language), None);
        let mut cfg = inner.config.write().expect("config lock poisoned");
        cfg.language = language.clone();
        save_standalone_config(&inner, &cfg);
    }

    broadcast_inner(
        &inner,
        json!({
            "type": "LANGUAGE_UPDATED",
            "language": language,
            "resolved": resolved,
        }),
        false,
        false,
    );

    json_response(
        StatusCode::OK,
        &json!({"ok": true, "language": language, "resolved": resolved}),
    )
}

async fn handle_set_minimap_tracking(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let raw_val = req
        .get("enabled")
        .or_else(|| req.get("minimap_tracking"));
    let mut enabled = json_truthy(raw_val, true);

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        if let Ok(Ok(res)) =
            tokio::task::spawn_blocking(move || cb.on_set_minimap_tracking(enabled)).await
        {
            enabled = res;
        }
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        cfg.minimap_tracking = enabled;
        save_standalone_config(&inner, &cfg);
    }

    broadcast_inner(
        &inner,
        json!({
            "type": "MINIMAP_TRACKING_UPDATED",
            "enabled": enabled,
            "minimap_tracking": enabled,
        }),
        false,
        false,
    );
    json_response(
        StatusCode::OK,
        &json!({"ok": true, "enabled": enabled, "minimap_tracking": enabled}),
    )
}

fn clamp_custom_pos_098(val: Option<&Value>) -> Option<[f64; 2]> {
    let arr = val?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let fx = json_f64(Some(&arr[0]))?;
    let fy = json_f64(Some(&arr[1]))?;
    if !fx.is_finite() || !fy.is_finite() {
        return None;
    }
    let cx = ((fx.clamp(0.0, 0.98) * 10_000.0).round() / 10_000.0).clamp(0.0, 0.98);
    let cy = ((fy.clamp(0.0, 0.98) * 10_000.0).round() / 10_000.0).clamp(0.0, 0.98);
    Some([
        if cx == 0.0 { 0.0 } else { cx },
        if cy == 0.0 { 0.0 } else { cy },
    ])
}

async fn handle_set_nav_hud(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let req_val = Value::Object(req.clone());

    let mut final_enabled: bool;
    let mut final_position: String;
    let mut extra = Map::new();

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let en_in = req
            .get("enabled")
            .or_else(|| req.get("nav_hud_enabled"))
            .map(|v| json_truthy(Some(v), true));
        let pos_in = req
            .get("position")
            .or_else(|| req.get("nav_hud_position"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());

        final_enabled = en_in.unwrap_or(true);
        final_position = pos_in.unwrap_or_else(|| "under_minimap".to_owned());

        if let Ok(Ok(Value::Object(cb_map))) =
            tokio::task::spawn_blocking(move || cb.on_set_nav_hud(&req_val)).await
        {
            if let Some(v) = cb_map.get("enabled").or_else(|| cb_map.get("nav_hud_enabled")) {
                final_enabled = json_truthy(Some(v), final_enabled);
            }
            if let Some(p) = cb_map
                .get("position")
                .or_else(|| cb_map.get("nav_hud_position"))
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
            {
                final_position = p;
            }
            for k in [
                "custom_pos",
                "edit_mode",
                "capture_visible",
                "nav_hud_capture_visible",
                "guide_zoom_mode",
                "nav_hud_guide_zoom_mode",
                "mmap_overlay_mode",
            ] {
                if let Some(v) = cb_map.get(k) {
                    if k == "custom_pos" {
                        let clamped = clamp_custom_pos_098(Some(v));
                        extra.insert(
                            k.to_owned(),
                            clamped.map_or(Value::Null, |p| json!(p)),
                        );
                    } else {
                        extra.insert(k.to_owned(), v.clone());
                    }
                }
            }
        }
        if req.contains_key("custom_pos") && !extra.contains_key("custom_pos") {
            let clamped = clamp_custom_pos_098(req.get("custom_pos"));
            extra.insert(
                "custom_pos".to_owned(),
                clamped.map_or(Value::Null, |p| json!(p)),
            );
        }
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");

        if let Some(v) = req.get("enabled").or_else(|| req.get("nav_hud_enabled"))
            && !v.is_null()
        {
            cfg.nav_hud_enabled = json_truthy(Some(v), true);
        }
        if req.contains_key("custom_pos") {
            cfg.nav_hud_custom_pos = clamp_custom_pos_098(req.get("custom_pos"));
        }
        if let Some(pos_raw) = req
            .get("position")
            .or_else(|| req.get("nav_hud_position"))
            .and_then(Value::as_str)
        {
            let norm = pos_raw.trim().to_ascii_lowercase();
            if !norm.is_empty() {
                if norm == "custom" && cfg.nav_hud_custom_pos.is_none() {
                    let fallback_pos = if cfg.nav_hud_position != "custom" {
                        cfg.nav_hud_position.clone()
                    } else {
                        "under_minimap".to_owned()
                    };
                    cfg.nav_hud_position = fallback_pos;
                    nav.edit_mode = true;
                } else {
                    cfg.nav_hud_position = norm;
                }
            }
        }
        if let Some(edit_req) = req.get("edit_mode").filter(|v| !v.is_null()) {
            if edit_req
                .as_str()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("toggle"))
            {
                nav.edit_mode = !nav.edit_mode;
            } else {
                nav.edit_mode = json_truthy(Some(edit_req), false);
            }
        }
        if let Some(cap_req) = req
            .get("capture_visible")
            .or_else(|| req.get("nav_hud_capture_visible"))
            .filter(|v| !v.is_null())
        {
            cfg.nav_hud_capture_visible = json_truthy(Some(cap_req), false);
        }
        if let Some(zoom_req) = req
            .get("guide_zoom_mode")
            .or_else(|| req.get("nav_hud_guide_zoom_mode"))
            .and_then(Value::as_str)
        {
            let norm = zoom_req.trim().to_ascii_lowercase();
            if matches!(norm.as_str(), "cycle" | "compact" | "large") {
                cfg.nav_hud_guide_zoom_mode = norm;
            }
        }
        if let Some(ov_req) = req.get("mmap_overlay_mode").and_then(Value::as_str) {
            let norm = ov_req.trim().to_ascii_lowercase();
            if VALID_MMAP_OVERLAY_MODES.contains(&norm.as_str()) {
                cfg.mmap_overlay_mode = norm;
            }
        }

        save_standalone_config(&inner, &cfg);

        final_enabled = cfg.nav_hud_enabled;
        final_position = cfg.nav_hud_position.clone();
        extra.insert(
            "custom_pos".to_owned(),
            cfg.nav_hud_custom_pos.map_or(Value::Null, |p| json!(p)),
        );
        extra.insert("edit_mode".to_owned(), Value::Bool(nav.edit_mode));
        extra.insert(
            "capture_visible".to_owned(),
            Value::Bool(cfg.nav_hud_capture_visible),
        );
        extra.insert(
            "nav_hud_capture_visible".to_owned(),
            Value::Bool(cfg.nav_hud_capture_visible),
        );
        extra.insert(
            "guide_zoom_mode".to_owned(),
            Value::String(cfg.nav_hud_guide_zoom_mode.clone()),
        );
        extra.insert(
            "nav_hud_guide_zoom_mode".to_owned(),
            Value::String(cfg.nav_hud_guide_zoom_mode.clone()),
        );
        extra.insert(
            "mmap_overlay_mode".to_owned(),
            Value::String(cfg.mmap_overlay_mode.clone()),
        );
    }

    let mut bcast = Map::new();
    bcast.insert("type".to_owned(), Value::String("NAV_HUD_UPDATED".to_owned()));
    bcast.insert("enabled".to_owned(), Value::Bool(final_enabled));
    bcast.insert("position".to_owned(), Value::String(final_position.clone()));
    bcast.insert("nav_hud_enabled".to_owned(), Value::Bool(final_enabled));
    bcast.insert(
        "nav_hud_position".to_owned(),
        Value::String(final_position.clone()),
    );
    for (k, v) in &extra {
        bcast.insert(k.clone(), v.clone());
    }
    broadcast_inner(&inner, Value::Object(bcast), false, false);

    let mut resp_map = Map::new();
    resp_map.insert("ok".to_owned(), Value::Bool(true));
    resp_map.insert("enabled".to_owned(), Value::Bool(final_enabled));
    resp_map.insert("position".to_owned(), Value::String(final_position));
    for (k, v) in extra {
        resp_map.insert(k, v);
    }
    json_response(StatusCode::OK, &Value::Object(resp_map))
}

async fn handle_set_radar(State(inner): State<Arc<SyncInner>>, body: Bytes) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let req_val = Value::Object(req.clone());

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();

    let (enabled, radar_rect, radar_zoom, radar_opacity) = if let Some(cb) = cb_opt {
        let mut en = req
            .get("enabled")
            .or_else(|| req.get("radar_enabled"))
            .map(|v| json_truthy(Some(v), false))
            .unwrap_or(false);
        let mut rect = None;
        let mut zoom = gh_core::config::DEFAULT_RADAR_ZOOM;
        let mut opacity = gh_core::config::DEFAULT_RADAR_OPACITY;
        if let Ok(Ok(Value::Object(cb_map))) =
            tokio::task::spawn_blocking(move || cb.on_set_radar(&req_val)).await
        {
            if let Some(v) = cb_map.get("radar_enabled").or_else(|| cb_map.get("enabled")) {
                en = json_truthy(Some(v), en);
            }
            if let Some(r) = cb_map.get("radar_rect").and_then(Value::as_array)
                && r.len() == 4
                && let (Some(x), Some(y), Some(w), Some(h)) =
                    (r[0].as_f64(), r[1].as_f64(), r[2].as_f64(), r[3].as_f64())
            {
                rect = Some([x, y, w, h]);
            }
            if let Some(z) = cb_map.get("radar_zoom").and_then(Value::as_f64) {
                zoom = z;
            }
            if let Some(op) = cb_map.get("radar_opacity").and_then(Value::as_f64) {
                opacity = op;
            }
        }
        (en, rect, zoom, opacity)
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        if let Some(v) = req.get("enabled").or_else(|| req.get("radar_enabled"))
            && !v.is_null()
        {
            cfg.radar_enabled = json_truthy(Some(v), false);
        }
        if let Some(z) = req.get("radar_zoom").and_then(Value::as_f64) {
            cfg.radar_zoom = z;
        }
        if let Some(op) = req.get("radar_opacity").and_then(Value::as_f64) {
            cfg.radar_opacity = op;
        }
        if let Some(r) = req.get("radar_rect") {
            if r.is_null() {
                cfg.radar_rect = None;
            } else if let Some(arr) = r.as_array()
                && arr.len() == 4
                && let (Some(x), Some(y), Some(w), Some(h)) = (
                    arr[0].as_f64(),
                    arr[1].as_f64(),
                    arr[2].as_f64(),
                    arr[3].as_f64(),
                )
            {
                cfg.radar_rect = Some([x, y, w, h]);
            }
        }
        cfg.normalize();
        save_standalone_config(&inner, &cfg);
        (
            cfg.radar_enabled,
            cfg.radar_rect,
            cfg.radar_zoom,
            cfg.radar_opacity,
        )
    };

    let bcast = json!({
        "type": "RADAR_UPDATED",
        "enabled": enabled,
        "radar_enabled": enabled,
        "radar_rect": radar_rect,
        "radar_zoom": radar_zoom,
        "radar_opacity": radar_opacity,
    });
    broadcast_inner(&inner, bcast, false, false);

    json_response(
        StatusCode::OK,
        &json!({
            "ok": true,
            "enabled": enabled,
            "radar_enabled": enabled,
            "radar_rect": radar_rect,
            "radar_zoom": radar_zoom,
            "radar_opacity": radar_opacity,
        }),
    )
}

async fn handle_set_audio(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let enabled_in = req.get("audio_enabled").or_else(|| req.get("enabled"));
    let vol_in = req.get("audio_volume").or_else(|| req.get("volume"));
    let tested = json_truthy(req.get("test"), false);

    let mut audio_enabled = enabled_in.is_none_or(|v| json_truthy(Some(v), true));
    let mut audio_volume = match json_f64(vol_in) {
        Some(f) if f.is_finite() => (f.round() as i64).clamp(0, 100) as i32,
        _ => 50,
    };

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let req_val = Value::Object(req);
        if let Ok(Ok(Value::Object(cb_map))) =
            tokio::task::spawn_blocking(move || cb.on_set_audio(&req_val)).await
        {
            if let Some(v) = cb_map.get("audio_enabled") {
                audio_enabled = json_truthy(Some(v), audio_enabled);
            }
            if let Some(v) = json_f64(cb_map.get("audio_volume"))
                && v.is_finite()
            {
                audio_volume = (v.round() as i64).clamp(0, 100) as i32;
            }
        }
    } else {
        let mut cfg = inner.config.write().expect("config lock poisoned");
        if enabled_in.is_some() {
            cfg.audio_enabled = audio_enabled;
        } else {
            audio_enabled = cfg.audio_enabled;
        }
        if vol_in.is_some() {
            cfg.audio_volume = audio_volume;
        } else {
            audio_volume = cfg.audio_volume;
        }
        save_standalone_config(&inner, &cfg);
    }

    broadcast_inner(
        &inner,
        json!({
            "type": "AUDIO_UPDATED",
            "audio_enabled": audio_enabled,
            "audio_volume": audio_volume,
        }),
        false,
        false,
    );

    json_response(
        StatusCode::OK,
        &json!({
            "ok": true,
            "tested": tested,
            "audio_enabled": audio_enabled,
            "audio_volume": audio_volume,
        }),
    )
}

async fn handle_nearby_pins(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let tab_id = req
        .get("tab_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let visible = req.get("tab_visible").and_then(Value::as_bool);
    let focused = req.get("tab_focused").and_then(Value::as_bool);
    let user_initiated = req
        .get("user_initiated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !inner.claim_or_check_pin_owner(tab_id, visible, focused, user_initiated, Instant::now()) {
        return json_response(
            StatusCode::OK,
            &json!({"ok": true, "count": 0, "ignored_secondary_client": true}),
        );
    }
    let provider_id = req
        .get("provider_id")
        .or_else(|| req.get("provider"))
        .and_then(Value::as_str);
    notify_provider_if_switched(&inner, provider_id).await;
    let map_id = json_i64(req.get("map_id")).unwrap_or(2) as i32;
    let raw_pins = req
        .get("pins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let count = if let Some(cb) = cb_opt {
        let req_val = Value::Object(req);
        tokio::task::spawn_blocking(move || cb.on_nearby_pins(&req_val))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(raw_pins.len())
    } else {
        let norm_pins: Vec<NavPin> = raw_pins
            .iter()
            .filter_map(|item| normalize_raw_nav_pin(item, map_id, None))
            .collect();
        let filter_changed = json_truthy(req.get("filter_changed"), false);
        let coverage = if filter_changed {
            None
        } else {
            match (
                json_f64(req.get("lat")),
                json_f64(req.get("lng")),
                json_f64(req.get("coverage_radius")),
            ) {
                (Some(lat), Some(lng), Some(rad))
                    if lat.is_finite() && lng.is_finite() && rad.is_finite() && rad > 0.0 =>
                {
                    Some((lat, lng, rad))
                }
                _ => None,
            }
        };
        let n = norm_pins.len();
        {
            let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
            nav.controller.set_pins(norm_pins, filter_changed, coverage);
        }
        push_standalone_lan_snapshot(&inner, false);
        n
    };

    json_response(StatusCode::OK, &json!({"ok": true, "count": count}))
}

async fn handle_nav_command(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    if let Some(tab_id) = req.get("tab_id").and_then(Value::as_str) {
        // User-initiated Navigate / lock_target from a browser tab promotes that tab as pin owner.
        let _ = inner.claim_or_check_pin_owner(Some(tab_id), Some(true), Some(true), true, Instant::now());
    }
    let action_str = json_str_trimmed(req.get("action")).to_ascii_lowercase();
    let is_lock_action = matches!(
        action_str.as_str(),
        "lock_target" | "lock_target_pin" | "unlock_target" | "clear_target_lock"
    );
    let parsed_action = NavAction::parse_server_action(&action_str);
    if !is_lock_action && parsed_action.is_none() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "ok": false,
                "action": action_str,
                "error": format!("Unsupported nav action: {action_str:?}"),
            }),
        );
    }

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let res_payload = if let Some(cb) = cb_opt {
        let req_val = Value::Object(req);
        match tokio::task::spawn_blocking(move || cb.on_nav_command(&req_val)).await {
            Ok(Ok(Value::Object(mut m))) => {
                m.entry("ok".to_owned()).or_insert(Value::Bool(true));
                m.entry("action".to_owned())
                    .or_insert_with(|| Value::String(action_str));
                Value::Object(m)
            }
            Ok(Ok(other)) => json!({
                "ok": json_truthy(Some(&other), true),
                "action": action_str,
            }),
            Ok(Err(e)) => json!({"ok": false, "action": action_str, "error": e.to_string()}),
            Err(e) => json!({"ok": false, "action": action_str, "error": e.to_string()}),
        }
    } else if is_lock_action {
        let (payload, should_push_lan) = {
            let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
            if matches!(action_str.as_str(), "unlock_target" | "clear_target_lock") {
                let cleared = nav.controller.clear_target_lock();
                let cur = nav
                    .controller
                    .current_target()
                    .map(NavPin::to_wire_json)
                    .unwrap_or(Value::Null);
                let sum = serde_json::to_value(nav.controller.summary()).unwrap_or(Value::Null);
                (
                    json!({
                        "ok": true,
                        "action": "unlock_target",
                        "cleared": cleared,
                        "target": cur,
                        "summary": sum,
                    }),
                    true,
                )
            } else {
                let pin_val = req.get("pin").cloned().unwrap_or_else(|| Value::Object(req.clone()));
                let map_id = json_i64(pin_val.get("map_id")).unwrap_or(2) as i32;
                if let Some(pin) = normalize_raw_nav_pin(&pin_val, map_id, None) {
                    let locked = nav.controller.lock_target_pin(pin);
                    let sum = serde_json::to_value(nav.controller.summary()).unwrap_or(Value::Null);
                    (
                        json!({
                            "ok": true,
                            "action": "lock_target",
                            "target": locked.to_wire_json(),
                            "summary": sum,
                        }),
                        true,
                    )
                } else {
                    (
                        json!({
                            "ok": false,
                            "action": "lock_target",
                            "error": "Invalid pin payload",
                        }),
                        false,
                    )
                }
            }
        };
        if should_push_lan {
            push_standalone_lan_snapshot(&inner, true);
        }
        payload
    } else {
        let action = parsed_action.expect("validated above");
        let pin_id = req
            .get("pin_id")
            .filter(|v| !v.is_null())
            .map(|v| json_str_trimmed(Some(v)))
            .filter(|s| !s.is_empty());

        let (payload, should_push_lan) = {
            let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
            match action {
                NavAction::Guide => {
                    nav.guide_open = !nav.guide_open;
                    let cur = nav
                        .controller
                        .current_target()
                        .map(NavPin::to_wire_json)
                        .unwrap_or(Value::Null);
                    let sum = serde_json::to_value(nav.controller.summary()).unwrap_or(Value::Null);
                    (
                        json!({
                            "ok": true,
                            "action": "guide",
                            "guide_open": nav.guide_open,
                            "target": cur,
                            "summary": sum,
                        }),
                        false,
                    )
                }
                NavAction::Layout => {
                    nav.edit_mode = !nav.edit_mode;
                    let cur = nav
                        .controller
                        .current_target()
                        .map(NavPin::to_wire_json)
                        .unwrap_or(Value::Null);
                    let sum = serde_json::to_value(nav.controller.summary()).unwrap_or(Value::Null);
                    (
                        json!({
                            "ok": true,
                            "action": "layout",
                            "edit_mode": nav.edit_mode,
                            "target": cur,
                            "summary": sum,
                        }),
                        false,
                    )
                }
                _ => {
                    let target_val = match action {
                        NavAction::Next => nav.controller.next_target().map(NavPin::to_wire_json),
                        NavAction::Prev => nav.controller.prev_target().map(NavPin::to_wire_json),
                        NavAction::Skip => nav.controller.skip_current().map(NavPin::to_wire_json),
                        NavAction::Blacklist => {
                            nav.controller.blacklist_current().map(NavPin::to_wire_json)
                        }
                        NavAction::Mark => nav
                            .controller
                            .mark_collected(pin_id.as_deref())
                            .map(NavPin::to_wire_json),
                        NavAction::Undo => nav.controller.undo().map(NavPin::to_wire_json),
                        NavAction::Refresh => nav
                            .controller
                            .refresh_nearest(None, None, None, None, None)
                            .map(NavPin::to_wire_json),
                        NavAction::ClearRoute => {
                            nav.controller.clear_route_queue();
                            nav.route_legs.clear();
                            nav.controller.current_target().map(NavPin::to_wire_json)
                        }
                        NavAction::Guide | NavAction::Layout => unreachable!(),
                    };
                    let sum = serde_json::to_value(nav.controller.summary()).unwrap_or(Value::Null);
                    (
                        json!({
                            "ok": true,
                            "action": action.as_str(),
                            "target": target_val.unwrap_or(Value::Null),
                            "summary": sum,
                        }),
                        true,
                    )
                }
            }
        };

        if should_push_lan {
            push_standalone_lan_snapshot(&inner, true);
        }
        payload
    };

    json_response(StatusCode::OK, &res_payload)
}

async fn handle_plan_tsp_route(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let map_id = json_i64(req.get("map_id")).unwrap_or(2) as i32;
    let teleport_penalty = json_f64(req.get("teleport_penalty"))
        .filter(|f| f.is_finite())
        .unwrap_or(DEFAULT_TELEPORT_PENALTY);

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();

    let route_res: CoreResult<Value> = if let Some(cb) = cb_opt {
        let req_val = Value::Object(req.clone());
        match tokio::task::spawn_blocking(move || cb.on_plan_tsp_route(&req_val)).await {
            Ok(r) => r,
            Err(e) => Err(CoreError::Sync(e.to_string())),
        }
    } else {
        let req_clone = req.clone();
        tokio::task::spawn_blocking(move || {
            let pins_val = req_clone
                .get("pins")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let pin_inputs: Vec<TspPinInput> = if pins_val.is_array() {
                serde_json::from_value(pins_val)?
            } else {
                Vec::new()
            };

            let start_pos: Option<TspCoordInput> = match req_clone.get("start_pos") {
                Some(v) if !v.is_null() => Some(serde_json::from_value(v.clone())?),
                _ => None,
            };

            let waypoints: Vec<TspWaypoint> = match req_clone.get("waypoints") {
                Some(v) if !v.is_null() => {
                    let raw_wps: Vec<TspWaypointInput> = serde_json::from_value(v.clone())?;
                    normalize_waypoints(&raw_wps)
                }
                _ => load_waypoints_for_map(map_id, None).unwrap_or_default(),
            };

            let plan = plan_route(
                &pin_inputs,
                start_pos,
                Some(&waypoints),
                TspPlanOptions {
                    teleport_penalty,
                    ..Default::default()
                },
            )?;
            serde_json::to_value(plan).map_err(CoreError::Json)
        })
        .await
        .unwrap_or_else(|e| Err(CoreError::Sync(e.to_string())))
    };

    let route_val = match route_res {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"ok": false, "error": e.to_string()}),
            );
        }
    };

    let route_map = match route_val {
        Value::Object(m) => m,
        _ => {
            let mut m = Map::new();
            m.insert("ordered_pins".to_owned(), Value::Array(Vec::new()));
            m.insert("legs".to_owned(), Value::Array(Vec::new()));
            m.insert("teleport_hints".to_owned(), Value::Array(Vec::new()));
            m
        }
    };

    if inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .is_none()
    {
        let legs = route_map
            .get("legs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let ordered_nav_pins: Vec<NavPin> = route_map
            .get("ordered_pins")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| normalize_raw_nav_pin(p, map_id, None))
                    .collect()
            })
            .unwrap_or_default();
        {
            let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
            nav.route_legs = legs;
            if ordered_nav_pins.is_empty() {
                nav.controller.clear_route_queue();
            } else {
                nav.controller.set_route_queue(ordered_nav_pins);
            }
        }
        push_standalone_lan_snapshot(&inner, true);
    }

    let mut bcast = Map::new();
    bcast.insert(
        "type".to_owned(),
        Value::String("TSP_ROUTE_UPDATED".to_owned()),
    );
    bcast.insert("map_id".to_owned(), json!(map_id));
    for (k, v) in &route_map {
        bcast.insert(k.clone(), v.clone());
    }
    broadcast_inner(&inner, Value::Object(bcast), false, false);

    let mut resp_map = Map::new();
    resp_map.insert("ok".to_owned(), Value::Bool(true));
    resp_map.insert("map_id".to_owned(), json!(map_id));
    for (k, v) in route_map {
        resp_map.insert(k, v);
    }
    json_response(StatusCode::OK, &Value::Object(resp_map))
}

async fn handle_app_update(
    State(inner): State<Arc<SyncInner>>,
    method: Method,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let mut req = if method == Method::GET {
        Map::new()
    } else {
        parse_lenient_json_map(&body)
    };
    let action = req
        .get("action")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "status".to_owned());
    req.insert("action".to_owned(), Value::String(action.clone()));
    let req_val = Value::Object(req.clone());

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();

    if let Some(cb) = cb_opt {
        let res = match tokio::task::spawn_blocking(move || cb.on_app_update(&req_val)).await {
            Ok(r) => r,
            Err(e) => Err(CoreError::Sync(e.to_string())),
        };
        return match res {
            Ok(val) => {
                let mut map = match val {
                    Value::Object(m) => m,
                    other => {
                        let mut m = Map::new();
                        m.insert("data".to_owned(), other);
                        m
                    }
                };
                map.entry("ok".to_owned()).or_insert(Value::Bool(true));
                json_response(StatusCode::OK, &Value::Object(map))
            }
            Err(e) => json_response(
                StatusCode::BAD_REQUEST,
                &json!({"ok": false, "error": e.to_string()}),
            ),
        };
    }

    match action.as_str() {
        "status" => {
            let (mode, auto_check) = {
                let cfg = inner.config.read().expect("config lock poisoned");
                (cfg.app_update_mode.clone(), cfg.auto_check_app_updates)
            };
            json_response(
                StatusCode::OK,
                &json!({
                    "ok": true,
                    "phase": "idle",
                    "current_version": gh_core::APP_VERSION,
                    "has_update": false,
                    "app_update_mode": mode,
                    "auto_check_app_updates": auto_check,
                    "map_data": { "phase": "idle" },
                }),
            )
        }
        "set_mode" | "set_auto_check" => {
            let next_mode = if let Some(m) = req
                .get("mode")
                .or_else(|| req.get("app_update_mode"))
                .and_then(Value::as_str)
            {
                let norm = m.trim().to_ascii_lowercase();
                if matches!(norm.as_str(), "auto" | "notify" | "off") {
                    norm
                } else {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        &json!({"ok": false, "error": format!("Invalid app_update_mode: {m}")}),
                    );
                }
            } else {
                let enabled = json_truthy(
                    req.get("enabled").or_else(|| req.get("auto_check_app_updates")),
                    true,
                );
                if enabled { "auto".to_owned() } else { "off".to_owned() }
            };
            let enabled = next_mode != "off";
            let cfg_clone = {
                let mut cfg = inner.config.write().expect("config lock poisoned");
                cfg.app_update_mode = next_mode.clone();
                cfg.auto_check_app_updates = enabled;
                cfg.clone()
            };
            save_standalone_config(&inner, &cfg_clone);
            json_response(
                StatusCode::OK,
                &json!({"ok": true, "app_update_mode": next_mode, "auto_check_app_updates": enabled}),
            )
        }
        _ => json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Update service unavailable in standalone mode"}),
        ),
    }
}

fn broadcast_standalone_route_updated(inner: &Arc<SyncInner>, status_json: &Value) {
    let mut bcast = status_json.as_object().cloned().unwrap_or_default();
    bcast.insert("type".to_owned(), Value::String("ROUTE_UPDATED".to_owned()));
    broadcast_inner(inner, Value::Object(bcast), false, false);
}

async fn handle_route(
    State(inner): State<Arc<SyncInner>>,
    method: Method,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let mut req = if method == Method::GET {
        Map::new()
    } else {
        parse_lenient_json_map(&body)
    };
    let action = req
        .get("action")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if method == Method::GET {
                "status".to_owned()
            } else if req.contains_key("sections")
                || req.contains_key("data")
                || req.contains_key("format")
                || req.contains_key("raw")
                || req.contains_key("document")
            {
                "load".to_owned()
            } else {
                "status".to_owned()
            }
        });
    req.insert("action".to_owned(), Value::String(action.clone()));
    let req_val = Value::Object(req.clone());

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();

    if let Some(cb) = cb_opt {
        let res = match tokio::task::spawn_blocking(move || cb.on_route_command(&req_val)).await {
            Ok(r) => r,
            Err(e) => Err(CoreError::Sync(e.to_string())),
        };
        return match res {
            Ok(val) => {
                let mut map = match val {
                    Value::Object(m) => m,
                    other => {
                        let mut m = Map::new();
                        m.insert("data".to_owned(), other);
                        m
                    }
                };
                map.entry("ok".to_owned()).or_insert(Value::Bool(true));
                json_response(StatusCode::OK, &Value::Object(map))
            }
            Err(e) => json_response(
                StatusCode::BAD_REQUEST,
                &json!({"ok": false, "error": e.to_string()}),
            ),
        };
    }

    // Standalone fallback (headless tests without AppState callbacks)
    let standalone_res: CoreResult<Value> = match action.as_str() {
        "status" => {
            let nav = inner.nav_state.lock().expect("nav_state lock poisoned");
            Ok(nav.controller.route_status_json())
        }
        "load" | "import" => {
            match parse_route_auto(&req_val, None) {
                Ok(doc) => {
                    let sec_idx = req
                        .get("section_index")
                        .and_then(Value::as_u64)
                        .map(|u| u as usize);
                    let marked: Vec<String> = req
                        .get("marked_ids")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let status_json = {
                        let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                        nav.controller.load_route_document(doc, sec_idx, &marked);
                        nav.controller.route_status_json()
                    };
                    push_standalone_lan_snapshot(&inner, true);
                    inner.lan_companion.update_route_snapshot(status_json.clone());
                    broadcast_standalone_route_updated(&inner, &status_json);
                    Ok(status_json)
                }
                Err(e) => Err(e),
            }
        }
        "section" => {
            let idx = req
                .get("section_index")
                .and_then(Value::as_u64)
                .map(|u| u as usize)
                .unwrap_or(0);
            let (ok_sel, status_json) = {
                let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                let ok_sel = nav.controller.select_route_section(idx);
                (ok_sel, nav.controller.route_status_json())
            };
            if !ok_sel {
                Err(CoreError::InvalidArgument(format!(
                    "Invalid section_index: {idx}"
                )))
            } else {
                push_standalone_lan_snapshot(&inner, true);
                inner.lan_companion.update_route_snapshot(status_json.clone());
                broadcast_standalone_route_updated(&inner, &status_json);
                Ok(status_json)
            }
        }
        "reset" => {
            let (was_reset, status_json) = {
                let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                let ok = nav.controller.reset_route_progress();
                (ok, nav.controller.route_status_json())
            };
            if !was_reset {
                Err(CoreError::InvalidArgument("No active route".to_owned()))
            } else {
                push_standalone_lan_snapshot(&inner, true);
                inner.lan_companion.update_route_snapshot(status_json.clone());
                broadcast_standalone_route_updated(&inner, &status_json);
                Ok(status_json)
            }
        }
        "clear" => {
            let status_json = {
                let mut nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                nav.controller.clear_route_document();
                nav.route_legs.clear();
                nav.controller.route_status_json()
            };
            push_standalone_lan_snapshot(&inner, true);
            inner.lan_companion.update_route_snapshot(status_json.clone());
            broadcast_standalone_route_updated(&inner, &status_json);
            Ok(status_json)
        }
        "export" => {
            let exported = {
                let nav = inner.nav_state.lock().expect("nav_state lock poisoned");
                nav.controller
                    .active_route_doc()
                    .map(|(doc, _)| (doc.suggested_filename(), doc.to_value()))
            };
            match exported {
                Some((fname, Ok(doc_val))) => Ok(json!({
                    "filename": fname,
                    "doc": doc_val,
                })),
                Some((_, Err(e))) => Err(e),
                None => Err(CoreError::InvalidArgument("No active route".to_owned())),
            }
        }
        "parse_share_url" => {
            let url = json_str_trimmed(req.get("url"));
            if let Some((map_id, share_id)) = parse_hoyolab_route_share_url(&url) {
                Ok(json!({
                    "map_id": map_id,
                    "share_id": share_id,
                    "url": url,
                }))
            } else {
                Err(CoreError::InvalidArgument(
                    "Invalid HoYoLAB interactive-map share URL".to_owned(),
                ))
            }
        }
        other => Err(CoreError::InvalidArgument(format!(
            "Unsupported route action: {other}"
        ))),
    };

    match standalone_res {
        Ok(val) => {
            let mut map = match val {
                Value::Object(m) => m,
                other => {
                    let mut m = Map::new();
                    m.insert("data".to_owned(), other);
                    m
                }
            };
            map.insert("ok".to_owned(), Value::Bool(true));
            json_response(StatusCode::OK, &Value::Object(map))
        }
        Err(e) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": e.to_string()}),
        ),
    }
}

async fn handle_lan_companion(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let action = {
        let s = json_str_trimmed(req.get("action")).to_ascii_lowercase();
        if s.is_empty() { "status".to_owned() } else { s }
    };

    if !matches!(
        action.as_str(),
        "status" | "start" | "stop" | "rotate" | "rotate_pin" | "toggle"
    ) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "ok": false,
                "running": false,
                "url": "",
                "pin": "",
                "qr_svg": "",
                "error": format!("Unsupported lan_companion action: {action:?}"),
            }),
        );
    }

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let req_val = Value::Object(req);
        let res = tokio::task::spawn_blocking(move || cb.on_lan_companion(&req_val)).await;
        match res {
            Ok(Ok(Value::Object(cb_map))) => {
                let mut out = Map::new();
                let ok_val = cb_map
                    .get("ok")
                    .is_none_or(|v| json_truthy(Some(v), true));
                let running_val = cb_map
                    .get("running")
                    .or_else(|| cb_map.get("is_running"))
                    .is_some_and(|v| json_truthy(Some(v), false));
                let url_val = cb_map
                    .get("url")
                    .or_else(|| cb_map.get("companion_url"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let pin_val = cb_map
                    .get("pin")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let qr_val = cb_map
                    .get("qr_svg")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();

                out.insert("ok".to_owned(), Value::Bool(ok_val));
                out.insert("running".to_owned(), Value::Bool(running_val));
                out.insert("url".to_owned(), Value::String(url_val));
                out.insert("pin".to_owned(), Value::String(pin_val));
                out.insert("qr_svg".to_owned(), Value::String(qr_val));
                for (k, v) in cb_map {
                    out.entry(k).or_insert(v);
                }
                return json_response(StatusCode::OK, &Value::Object(out));
            }
            Ok(Ok(_)) => {
                return json_response(
                    StatusCode::OK,
                    &json!({"ok": false, "running": false, "url": "", "pin": "", "qr_svg": ""}),
                );
            }
            Ok(Err(e)) => {
                return json_response(
                    StatusCode::OK,
                    &json!({"ok": false, "running": inner.lan_companion.is_running(), "url": "", "pin": "", "qr_svg": "", "error": e.to_string()}),
                );
            }
            Err(e) => {
                return json_response(
                    StatusCode::OK,
                    &json!({"ok": false, "running": inner.lan_companion.is_running(), "url": "", "pin": "", "qr_svg": "", "error": e.to_string()}),
                );
            }
        }
    }

    // Standalone LAN Companion management inside gh-net
    let lan = Arc::clone(&inner.lan_companion);
    let configured_port = inner
        .config
        .read()
        .expect("config lock poisoned")
        .lan_companion_port;

    let mut mutated = false;
    let op_res: CoreResult<()> = match action.as_str() {
        "status" => Ok(()),
        "start" => {
            if !lan.is_running() {
                lan.set_port(configured_port);
                lan.start().await.map(|_| ())
            } else {
                Ok(())
            }
        }
        "stop" => {
            if lan.is_running() {
                lan.stop().await;
            }
            Ok(())
        }
        "rotate" | "rotate_pin" => {
            lan.rotate_credentials();
            mutated = true;
            Ok(())
        }
        "toggle" => {
            if lan.is_running() {
                lan.stop().await;
                Ok(())
            } else {
                lan.set_port(configured_port);
                lan.start().await.map(|_| ())
            }
        }
        _ => unreachable!(),
    };

    if let Err(e) = op_res {
        return json_response(
            StatusCode::OK,
            &json!({
                "ok": false,
                "running": lan.is_running(),
                "url": "",
                "pin": "",
                "qr_svg": "",
                "error": e.to_string(),
            }),
        );
    }

    let running_now = lan.is_running();
    if matches!(action.as_str(), "start" | "stop" | "toggle") {
        mutated = true;
        let mut cfg = inner.config.write().expect("config lock poisoned");
        cfg.lan_companion_enabled = running_now;
        save_standalone_config(&inner, &cfg);
    }

    if running_now && matches!(action.as_str(), "start" | "toggle") {
        push_standalone_lan_snapshot(&inner, true);
    }

    let info = lan.get_connection_info();
    if mutated {
        broadcast_inner(
            &inner,
            json!({
                "type": "LAN_COMPANION_UPDATED",
                "running": info.is_running,
                "url": info.url,
                "pin": info.pin,
            }),
            false,
            false,
        );
    }

    let mut resp_map = match serde_json::to_value(&info) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    resp_map.insert("ok".to_owned(), Value::Bool(true));
    resp_map.insert("running".to_owned(), Value::Bool(info.is_running));
    json_response(StatusCode::OK, &Value::Object(resp_map))
}

async fn handle_lan_pins(State(inner): State<Arc<SyncInner>>, body: Bytes) -> Response {
    mark_client_poll(&inner);
    let Ok(val @ Value::Object(_)) = serde_json::from_slice::<Value>(&body) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Invalid JSON object"}),
        );
    };
    if let Value::Object(ref obj) = val {
        let tab_id = obj
            .get("tab_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let visible = obj.get("tab_visible").and_then(Value::as_bool);
        let focused = obj.get("tab_focused").and_then(Value::as_bool);
        let user_initiated = obj
            .get("user_initiated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !inner.claim_or_check_pin_owner(tab_id, visible, focused, user_initiated, Instant::now()) {
            return json_response(
                StatusCode::OK,
                &json!({"ok": true, "rev": 0, "ignored_secondary_client": true}),
            );
        }
        let provider_id = obj
            .get("provider_id")
            .or_else(|| obj.get("provider"))
            .and_then(Value::as_str);
        notify_provider_if_switched(&inner, provider_id).await;
    }
    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    if let Some(cb) = cb_opt {
        let val_cb = val.clone();
        let _ = tokio::task::spawn_blocking(move || cb.on_lan_pins(&val_cb)).await;
    }
    let rev = inner.lan_companion.update_pins_snapshot(val);
    json_response(StatusCode::OK, &json!({"ok": true, "rev": rev}))
}

async fn handle_launch_borderless(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req_val = Value::Object(parse_lenient_json_map(&body));

    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let launch_res = if let Some(cb) = cb_opt {
        match tokio::task::spawn_blocking(move || cb.on_launch_borderless(&req_val)).await {
            Ok(Ok(Value::Object(mut m))) => {
                let ok_val = m.get("ok").is_some_and(|v| json_truthy(Some(v), false));
                let dry_run = m
                    .get("dry_run")
                    .is_some_and(|v| json_truthy(Some(v), false));
                let launched_val = m
                    .get("launched")
                    .map_or(ok_val && !dry_run, |v| json_truthy(Some(v), false));
                let msg_val = m
                    .get("message")
                    .or_else(|| m.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if launched_val {
                            "Launched".to_owned()
                        } else {
                            String::new()
                        }
                    });
                m.insert("ok".to_owned(), Value::Bool(ok_val));
                m.insert("launched".to_owned(), Value::Bool(launched_val));
                m.insert("message".to_owned(), Value::String(msg_val));
                Value::Object(m)
            }
            Ok(Ok(other)) => {
                let ok_val = json_truthy(Some(&other), false);
                json!({"ok": ok_val, "launched": ok_val, "message": ""})
            }
            Ok(Err(e)) => json!({"ok": false, "launched": false, "message": e.to_string()}),
            Err(e) => json!({"ok": false, "launched": false, "message": e.to_string()}),
        }
    } else {
        json!({
            "ok": false,
            "launched": false,
            "message": "Borderless launcher not configured",
        })
    };

    json_response(StatusCode::OK, &launch_res)
}

async fn handle_cross_map_export(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let target_format = {
        let raw = req
            .get("target_format")
            .filter(|v| json_truthy(Some(v), false))
            .or_else(|| req.get("format").filter(|v| json_truthy(Some(v), false)));
        let s = json_str_trimmed(raw).to_ascii_lowercase();
        if s.is_empty() { "canonical".to_owned() } else { s }
    };

    if !matches!(
        target_format.as_str(),
        "kongying" | "appsample" | "genshinmap" | "canonical"
    ) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "ok": false,
                "format": target_format,
                "payload": Value::Null,
                "report": Value::Null,
                "error": format!("Unsupported target_format: {target_format:?}"),
            }),
        );
    }

    let req_val = Value::Object(req);
    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let res: CoreResult<Value> = if let Some(cb) = cb_opt {
        tokio::task::spawn_blocking(move || cb.on_cross_map_export(&req_val))
            .await
            .unwrap_or_else(|e| Err(CoreError::Sync(e.to_string())))
    } else {
        tokio::task::spawn_blocking(move || execute_cross_map_export(&req_val, None, None))
            .await
            .unwrap_or_else(|e| Err(CoreError::Sync(e.to_string())))
    };

    match res {
        Ok(Value::Object(m)) => {
            let mut out = Map::new();
            let ok_val = m.get("ok").is_none_or(|v| json_truthy(Some(v), true));
            let fmt_val = m
                .get("format")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&target_format)
                .to_owned();
            out.insert("ok".to_owned(), Value::Bool(ok_val));
            out.insert("format".to_owned(), Value::String(fmt_val));
            out.insert(
                "payload".to_owned(),
                m.get("payload").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "report".to_owned(),
                m.get("report").cloned().unwrap_or(Value::Null),
            );
            for (k, v) in m {
                out.entry(k).or_insert(v);
            }
            json_response(StatusCode::OK, &Value::Object(out))
        }
        Ok(other) => json_response(
            StatusCode::OK,
            &json!({
                "ok": true,
                "format": target_format,
                "payload": other,
                "report": Value::Null,
            }),
        ),
        Err(e) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "ok": false,
                "format": target_format,
                "payload": Value::Null,
                "report": Value::Null,
                "error": e.to_string(),
            }),
        ),
    }
}

async fn handle_calibrate_provider(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let req_val = Value::Object(req);
    let cb_opt = inner
        .callbacks
        .read()
        .expect("callbacks lock poisoned")
        .clone();
    let res: CoreResult<Value> = if let Some(cb) = cb_opt {
        tokio::task::spawn_blocking(move || cb.on_calibrate_provider(&req_val))
            .await
            .unwrap_or_else(|e| Err(CoreError::Sync(e.to_string())))
    } else {
        tokio::task::spawn_blocking(move || execute_provider_calibration(&req_val, &[]))
            .await
            .unwrap_or_else(|e| Err(CoreError::Sync(e.to_string())))
    };
    match res {
        Ok(val) => {
            let status = if val.get("ok").and_then(Value::as_bool) == Some(false) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            json_response(status, &val)
        }
        Err(e) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({
                "ok": false,
                "error": e.to_string(),
            }),
        ),
    }
}

async fn handle_cross_map_import_preview(
    State(inner): State<Arc<SyncInner>>,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let req = parse_lenient_json_map(&body);
    let cache_dir = gh_core::paths::AppPaths::resolve().cache_dir;
    match crate::cross_map_service::execute_cross_map_import_preview(&req, &cache_dir).await {
        Ok(val) => json_response(StatusCode::OK, &val),
        Err(CoreError::InvalidArgument(msg)) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "ok": false, "error": msg }),
        ),
        Err(err) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "ok": false, "error": err.to_string() }),
        ),
    }
}

async fn handle_cross_map_import_history(
    State(inner): State<Arc<SyncInner>>,
    method: Method,
    body: Bytes,
) -> Response {
    mark_client_poll(&inner);
    let cache_dir = gh_core::paths::AppPaths::resolve().cache_dir;
    let res = if method == Method::GET {
        crate::cross_map_service::handle_import_history_command(None, &cache_dir)
    } else {
        let req = parse_lenient_json_map(&body);
        crate::cross_map_service::handle_import_history_command(Some(&req), &cache_dir)
    };
    match res {
        Ok(val) => json_response(StatusCode::OK, &val),
        Err(CoreError::InvalidArgument(msg)) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({ "ok": false, "error": msg }),
        ),
        Err(err) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "ok": false, "error": err.to_string() }),
        ),
    }
}

struct WaiterGuard {
    inner: Arc<SyncInner>,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut st = self
            .inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned");
        st.active_waiters = st.active_waiters.saturating_sub(1);
    }
}

async fn handle_poll(
    State(inner): State<Arc<SyncInner>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    mark_client_poll(&inner);

    let since_seq: Option<i64> = query
        .get("since")
        .and_then(|s| s.trim().parse::<i64>().ok());

    if let Some(since) = since_seq
        && since < 0
    {
        let current_seq = inner
            .poll_state
            .lock()
            .expect("poll_state lock poisoned")
            .event_seq;
        let hb = serde_json::to_value(PollEnvelope::heartbeat(current_seq))
            .unwrap_or_else(|_| json!({"type": "HEARTBEAT", "_seq": current_seq}));
        return json_response(StatusCode::OK, &hb);
    }

    let timeout = inner.poll_timeout;
    let deadline = tokio::time::Instant::now() + timeout;

    let mut legacy_start_seq: Option<u64> = None;

    loop {
        let notified = inner.poll_notify.notified();
        tokio::pin!(notified);

        let immediate: Option<Value> = {
            let mut st = inner
                .poll_state
                .lock()
                .expect("poll_state lock poisoned");
            if st.shutdown_requested {
                Some(
                    serde_json::to_value(PollEnvelope::shutdown())
                        .unwrap_or_else(|_| json!({"type": "SHUTDOWN"})),
                )
            } else if let Some(since) = since_seq {
                st.next_event_since(since as u64)
            } else if let Some(ev) = st.event_queue.pop_front() {
                Some(ev)
            } else if let Some(start_seq) = legacy_start_seq
                && st.event_seq > start_seq
            {
                st.next_event_since(start_seq)
            } else {
                if legacy_start_seq.is_none() {
                    legacy_start_seq = Some(st.event_seq);
                }
                None
            }
        };

        if let Some(ev) = immediate {
            return json_response(StatusCode::OK, &ev);
        }

        // Register waiter BEFORE releasing lock / awaiting notification to guarantee zero lost wakeups.
        let _guard = {
            let mut st = inner
                .poll_state
                .lock()
                .expect("poll_state lock poisoned");
            if st.shutdown_requested {
                return json_response(
                    StatusCode::OK,
                    &serde_json::to_value(PollEnvelope::shutdown())
                        .unwrap_or_else(|_| json!({"type": "SHUTDOWN"})),
                );
            }
            if let Some(since) = since_seq
                && let Some(ev) = st.next_event_since(since as u64)
            {
                return json_response(StatusCode::OK, &ev);
            }
            if since_seq.is_none() {
                if let Some(ev) = st.event_queue.pop_front() {
                    return json_response(StatusCode::OK, &ev);
                }
                if let Some(start_seq) = legacy_start_seq
                    && st.event_seq > start_seq
                    && let Some(ev) = st.next_event_since(start_seq)
                {
                    return json_response(StatusCode::OK, &ev);
                }
            }
            st.active_waiters += 1;
            notified.as_mut().enable();
            WaiterGuard {
                inner: Arc::clone(&inner),
            }
        };

        let sleep = tokio::time::sleep_until(deadline);
        tokio::select! {
            _ = &mut notified => {
                // Loop back and inspect poll_state (drops _guard first)
            }
            _ = sleep => {
                drop(_guard);
                let current_seq = inner
                    .poll_state
                    .lock()
                    .expect("poll_state lock poisoned")
                    .event_seq;
                let hb = serde_json::to_value(PollEnvelope::heartbeat(current_seq))
                    .unwrap_or_else(|_| json!({"type": "HEARTBEAT", "_seq": current_seq}));
                return json_response(StatusCode::OK, &hb);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit & Localhost Integration Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn make_temp_dir(label: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("gh_net_sync_{label}_{}_{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    fn assert_cors_pna(headers: &reqwest::header::HeaderMap) {
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert_eq!(
            headers
                .get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok()),
            Some("GET, POST, OPTIONS")
        );
        assert_eq!(
            headers
                .get("access-control-allow-headers")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert_eq!(
            headers
                .get("access-control-allow-private-network")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            headers.get("connection").and_then(|v| v.to_str().ok()),
            Some("close")
        );
    }

    #[tokio::test]
    async fn calibrate_provider_and_multi_provider_switch_clears_stale_lock() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(120),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // 1. /calibrate_provider returns verified seed transform for appsample (including Inazuma & Easybreeze zones)
        let cal_resp = cli
            .post(format!("{base}/calibrate_provider"))
            .json(&json!({"provider": "appsample", "map_id": 2, "anchors": []}))
            .send()
            .await
            .expect("calibrate_provider");
        assert_eq!(cal_resp.status(), StatusCode::OK);
        let cal_json: Value = cal_resp.json().await.expect("cal json");
        assert_eq!(cal_json["ok"], true);
        assert_eq!(cal_json["provider"], "appsample");
        assert_eq!(cal_json["transform"]["zones"].as_array().map(Vec::len), Some(2));

        // 2. Lock a pin on HoYoLAB, then switch active tab to Kongying -> stale lock is cleared
        assert_eq!(srv.active_provider(), "hoyolab");
        let lock_resp = cli
            .post(format!("{base}/nav_command"))
            .json(&json!({
                "action": "lock_target",
                "pin": {"id": "1001", "name": "HoYo Pin", "lat": 10.0, "lng": 20.0, "map_id": 2}
            }))
            .send()
            .await
            .expect("lock_target");
        assert_eq!(lock_resp.status(), StatusCode::OK);

        let ky_resp = cli
            .post(format!("{base}/nearby_pins"))
            .json(&json!({
                "tab_id": "tab_ky",
                "provider_id": "kongying",
                "tab_visible": true,
                "tab_focused": true,
                "user_initiated": true,
                "map_id": 2,
                "pins": [{"id": "kongying:560", "name": "普通宝箱", "lat": 12.0, "lng": 22.0, "map_id": 2}]
            }))
            .send()
            .await
            .expect("ky nearby_pins");
        assert_eq!(ky_resp.status(), StatusCode::OK);
        assert_eq!(srv.active_provider(), "kongying");

        srv.stop().await;
    }

    #[tokio::test]
    async fn all_28_paths_cors_pna_and_boundaries() {
        let tmp = make_temp_dir("routes");
        let cfg_path = tmp.join("config.json");
        let diag_dir = tmp.join("diagnostics");
        let valid_bundle = diag_dir.join("diag_20261122_010203");
        std::fs::create_dir_all(&valid_bundle).expect("bundle dir");
        std::fs::write(valid_bundle.join("03_report.json"), r#"{"ok":true}"#).expect("report");

        let lan_probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("lan probe");
        let lan_port = lan_probe.local_addr().expect("lan probe addr").port();
        drop(lan_probe);

        let app_cfg = AppConfig {
            lan_companion_port: lan_port,
            ..Default::default()
        };
        let lan = Arc::new(LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: lan_port,
            advertised_host: Some("127.0.0.1".to_owned()),
            ..Default::default()
        }));

        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(120),
            config: app_cfg,
            config_path: Some(cfg_path.clone()),
            diagnostics_dir: Some(diag_dir),
            lan_companion: Some(lan),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        assert_ne!(port, 0);
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // 1. OPTIONS preflight -> 204 Content-Length: 0
        for route in ["/status", "/poll", "/cross_map_export", "/shutdown"] {
            let resp = cli
                .request(reqwest::Method::OPTIONS, format!("{base}{route}"))
                .send()
                .await
                .expect("options");
            assert_eq!(resp.status(), 204);
            assert_cors_pna(resp.headers());
            assert_eq!(
                resp.headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok()),
                Some("0")
            );
            assert!(resp.bytes().await.expect("body").is_empty());
        }

        // 2. 404 still has CORS/PNA + Connection: close
        let r_404 = cli.get(format!("{base}/no_such_route")).send().await.expect("404");
        assert_eq!(r_404.status(), 404);
        assert_cors_pna(r_404.headers());

        // 3. /shutdown rejects GET/PUT/DELETE with 403 (never 405)
        for m in [
            reqwest::Method::GET,
            reqwest::Method::PUT,
            reqwest::Method::DELETE,
        ] {
            let r_shut = cli
                .request(m, format!("{base}/shutdown"))
                .send()
                .await
                .expect("shut reject");
            assert_eq!(r_shut.status(), 403);
            assert_cors_pna(r_shut.headers());
            let j: Value = r_shut.json().await.expect("json");
            assert_eq!(j, json!({"ok": false, "error": "Forbidden"}));
        }

        // 4. /status returns all 54 keys (including Radar + nav_unlock keys)
        let r_st = cli.get(format!("{base}/status")).send().await.expect("status");
        assert_eq!(r_st.status(), 200);
        assert_cors_pna(r_st.headers());
        let st_json: Value = r_st.json().await.expect("status json");
        assert_eq!(st_json.as_object().expect("obj").len(), 54);
        assert_eq!(st_json["version"], "5.7.0");
        assert_eq!(st_json["script_version"], "v5.7.0");
        assert_eq!(st_json["radar_enabled"], false);
        assert_eq!(st_json["hotkey_radar_toggle"], "alt+`");
        assert_eq!(st_json["hotkey_nav_unlock"], "alt+f6");

        let r_radar = cli
            .post(format!("{base}/set_radar"))
            .json(&json!({"enabled": true, "radar_zoom": -0.35, "radar_opacity": 0.9}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_radar["ok"], true);
        assert_eq!(r_radar["radar_enabled"], true);

        // 5. Toggles & /set_hotkey + /set_hotkeys alias + query fallback
        assert_eq!(
            cli.post(format!("{base}/set_engine_state"))
                .json(&json!({"active": false}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({"ok": true, "engine_active": false})
        );
        assert_eq!(
            cli.post(format!("{base}/set_auto_sync"))
                .json(&json!({"auto_sync_m": false}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({"ok": true, "auto_sync_m": false})
        );
        assert_eq!(
            cli.post(format!("{base}/set_single_monitor"))
                .json(&json!({"single_monitor_mode": true}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({"ok": true, "single_monitor_mode": true})
        );
        assert_eq!(
            cli.post(format!("{base}/set_minimap_tracking"))
                .json(&json!({"enabled": false}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({"ok": true, "enabled": false, "minimap_tracking": false})
        );

        let r_hk = cli
            .post(format!("{base}/set_hotkey"))
            .json(&json!({"action": "debug", "hotkey": "alt+f10"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_hk["results"]["debug"]["ok"], true);
        assert_eq!(r_hk["results"]["debug"]["hotkey"], "alt+f10");

        let r_hks = cli
            .post(format!("{base}/set_hotkeys"))
            .json(&json!({"hotkey_sync": "alt+f8", "hotkey_nav_toggle": "alt+y"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_hks["results"]["sync"]["ok"], true);
        assert_eq!(r_hks["results"]["nav_toggle"]["ok"], true);

        let r_hk_q = cli
            .get(format!("{base}/set_hotkey?action=switch&hotkey=alt%2Bp"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_hk_q["results"]["switch"]["hotkey"], "alt+p");

        // 6. /set_language, /set_audio, /set_nav_hud custom_pos [0.0, 0.98] clamp
        assert_eq!(
            cli.post(format!("{base}/set_language"))
                .json(&json!({"language": "fr"}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            cli.post(format!("{base}/set_language"))
                .json(&json!({"language": "vi"}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({"ok": true, "language": "vi", "resolved": "vi"})
        );
        assert_eq!(
            cli.post(format!("{base}/set_audio"))
                .json(&json!({"audio_enabled": false, "audio_volume": 999}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["audio_volume"],
            100
        );
        let r_hud_clamp = cli
            .post(format!("{base}/set_nav_hud"))
            .json(&json!({"custom_pos": [9.5, -1.0], "edit_mode": "toggle"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_hud_clamp["custom_pos"], json!([0.98, 0.0]));
        assert_eq!(r_hud_clamp["edit_mode"], true);
        let r_hud_bad = cli
            .post(format!("{base}/set_nav_hud"))
            .json(&json!({"custom_pos": "bad"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert!(r_hud_bad["custom_pos"].is_null());
        let r_ov = cli
            .post(format!("{base}/set_nav_hud"))
            .json(&json!({"mmap_overlay_mode": "minimal"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_ov["mmap_overlay_mode"], "minimal");
        let _ = cli
            .post(format!("{base}/set_nav_hud"))
            .json(&json!({"mmap_overlay_mode": "full"}))
            .send()
            .await
            .unwrap();

        // 7. /report_state (malformed -> 200 {}) + /browser_state
        assert_eq!(
            cli.post(format!("{base}/report_state"))
                .body("{not_json")
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            json!({})
        );
        cli.post(format!("{base}/report_state"))
            .json(&json!({"zoom": 1.5}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            cli.get(format!("{base}/browser_state"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["zoom"],
            1.5
        );

        // 8. /save_diagnostic_note traversal 400, missing 404, valid 200
        for bad in ["../bad", "diag_a/b", "diag_a\\b", "foo", ""] {
            let r = cli
                .post(format!("{base}/save_diagnostic_note"))
                .json(&json!({"folder_name": bad, "note": "x"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "bad={bad}");
        }
        let r_missing = cli
            .post(format!("{base}/save_diagnostic_note"))
            .json(&json!({"folder_name": "diag_999999", "note": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_missing.status(), 404);
        assert_eq!(
            r_missing.json::<Value>().await.unwrap()["not_found"],
            true
        );
        let r_note_ok = cli
            .post(format!("{base}/save_diagnostic_note"))
            .json(&json!({"folder_name": "diag_20261122_010203", "note": "Ghi chú tiếng Việt"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_note_ok.status(), 200);
        assert_eq!(
            std::fs::read_to_string(valid_bundle.join("05_user_note.txt")).unwrap(),
            "Ghi chú tiếng Việt\n"
        );

        // 9. /check_hotkey_conflicts, /remap_external_hotkey, /restore_external_hotkey, /trigger_diagnostic, /launch_borderless
        assert_eq!(
            cli.post(format!("{base}/check_hotkey_conflicts"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            cli.post(format!("{base}/remap_external_hotkey"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            cli.post(format!("{base}/restore_external_hotkey"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            cli.post(format!("{base}/trigger_diagnostic"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            cli.post(format!("{base}/launch_borderless"))
                .json(&json!({"dry_run": true}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["launched"],
            false
        );
        assert_eq!(
            cli.post(format!("{base}/quick_switch"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["target"],
            "none"
        );

        // 10. /nearby_pins, /nav_command, /plan_tsp_route, /action_feedback dedup & NAV_MARK rollback
        let pins = json!([
            {"id": "p1", "name": "Pin 1", "lat": 10.0, "lng": 20.0, "map_id": 2},
            {"id": "p2", "name": "Pin 2", "lat": 50.0, "lng": 60.0, "map_id": 2}
        ]);
        assert_eq!(
            cli.post(format!("{base}/nearby_pins"))
                .json(&json!({"map_id": 2, "pins": pins}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["count"],
            2
        );
        assert_eq!(
            cli.post(format!("{base}/nav_command"))
                .json(&json!({"action": "bogus"}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let r_mark = cli
            .post(format!("{base}/nav_command"))
            .json(&json!({"action": "mark", "pin_id": "p1"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_mark["summary"]["collected"], 1);

        let fb1 = cli
            .post(format!("{base}/action_feedback"))
            .json(&json!({"event_type": "NAV_MARK", "status": "error", "seq": 9001, "action": "mark", "pin_id": "p1"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(fb1["deduped"], false);
        let fb2 = cli
            .post(format!("{base}/action_feedback"))
            .json(&json!({"event_type": "NAV_MARK", "status": "error", "seq": 9001, "action": "mark", "pin_id": "p1"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(fb2["deduped"], true);

        let r_ref = cli
            .post(format!("{base}/nav_command"))
            .json(&json!({"action": "refresh"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_ref["summary"]["collected"], 0);

        let r_tsp = cli
            .post(format!("{base}/plan_tsp_route"))
            .json(&json!({"map_id": 2, "start_pos": [0.0, 0.0], "pins": pins}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_tsp["ok"], true);
        assert_eq!(r_tsp["ordered_pins"].as_array().unwrap().len(), 2);

        // 10b. /route REST lifecycle (load -> GET -> section switch -> export -> clear + ROUTE_UPDATED event)
        let hb0: Value = cli
            .get(format!("{base}/poll?since=-1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let seq_before_route = hb0["_seq"].as_u64().unwrap();

        let sample_route = json!({
            "action": "load",
            "format": "genshin-helper-route",
            "version": 1,
            "title": "Test Route",
            "map_id": 2,
            "sections": [
                {
                    "index": 0,
                    "name": "S1",
                    "color": "#38bdf8",
                    "stops": [
                        {"id": "rs1", "kind": "collectible", "lat": 100.0, "lng": 200.0}
                    ],
                    "edges": []
                },
                {
                    "index": 1,
                    "name": "S2",
                    "color": "#4ade80",
                    "stops": [
                        {"id": "rs2", "kind": "collectible", "lat": 300.0, "lng": 400.0}
                    ],
                    "edges": []
                }
            ]
        });
        let r_route_load = cli
            .post(format!("{base}/route"))
            .json(&sample_route)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_route_load["ok"], true);
        assert_eq!(r_route_load["active"], true);
        assert_eq!(r_route_load["section_count"], 2);
        assert_eq!(r_route_load["active_section"], 0);

        let r_route_get = cli
            .get(format!("{base}/route"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_route_get["active"], true);
        assert_eq!(r_route_get["title"], "Test Route");

        let r_route_sec = cli
            .post(format!("{base}/route"))
            .json(&json!({"action": "section", "section_index": 1}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_route_sec["active_section"], 1);

        let r_route_exp = cli
            .post(format!("{base}/route"))
            .json(&json!({"action": "export"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_route_exp["doc"]["sections"].as_array().unwrap().len(), 2);

        let r_route_clr = cli
            .post(format!("{base}/route"))
            .json(&json!({"action": "clear"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_route_clr["active"], false);

        // Verify ROUTE_UPDATED appeared in durable event stream after seq_before_route
        let poll_route = cli
            .get(format!("{base}/poll?since={seq_before_route}"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(poll_route["type"], "ROUTE_UPDATED");

        // 11. /lan_companion supports status|start|stop|rotate|rotate_pin|toggle (400 on invalid)
        assert_eq!(
            cli.post(format!("{base}/lan_companion"))
                .json(&json!({"action": "bad"}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let mut r_tog = cli
            .post(format!("{base}/lan_companion"))
            .json(&json!({"action": "toggle"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_tog["ok"], true);
        assert_eq!(r_tog["running"], true);
        assert_eq!(r_tog["bind_host"], "127.0.0.1");
        let old_pin = r_tog["pin"].as_str().unwrap().to_owned();
        let r_rot = cli
            .post(format!("{base}/lan_companion"))
            .json(&json!({"action": "rotate"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_rot["ok"], true);
        assert_ne!(r_rot["pin"].as_str().unwrap(), old_pin);
        r_tog = cli
            .post(format!("{base}/lan_companion"))
            .json(&json!({"action": "stop"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(r_tog["running"], false);

        // 12. >2.5 MiB /cross_map_export succeeds (DefaultBodyLimit = 32 MiB)
        let mut big_points = Vec::with_capacity(20_000);
        for i in 0..20_000 {
            big_points.push(json!({
                "id": i + 1,
                "label_id": 16,
                "x_pos": (i as f64) * 0.5,
                "y_pos": (i as f64) * -0.25,
                "name": "Common Chest Large Payload Entry Padding Text 1234567890"
            }));
        }
        let big_body = serde_json::to_vec(&json!({
            "target_format": "canonical",
            "map_id": 2,
            "point_list": {"data": {"point_list": big_points}},
        }))
        .unwrap();
        assert!(big_body.len() > 2 * 1024 * 1024);
        let r_big = cli
            .post(format!("{base}/cross_map_export"))
            .header("Content-Type", "application/json")
            .body(big_body)
            .send()
            .await
            .unwrap();
        assert_eq!(r_big.status(), 200);
        let j_big: Value = r_big.json().await.unwrap();
        assert_eq!(j_big["ok"], true);
        assert_eq!(j_big["report"]["total_source_pins"], 20_000);

        // Config persisted
        assert!(cfg_path.is_file());

        srv.stop().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn lossless_multitab_poll_ephemeral_coalesce_and_shutdown_wakeup() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(150),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // since=-1 returns instant HEARTBEAT even while other pollers wait
        let hb: Value = cli
            .get(format!("{base}/poll?since=-1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hb["type"], "HEARTBEAT");
        let seq0 = hb["_seq"].as_u64().unwrap();

        // Spawn 2 concurrent pollers at seq0
        let cli1 = cli.clone();
        let cli2 = cli.clone();
        let u1 = format!("{base}/poll?since={seq0}");
        let u2 = format!("{base}/poll?since={seq0}");
        let f1 = tokio::spawn(async move { cli1.get(u1).send().await.unwrap().json::<Value>().await.unwrap() });
        let f2 = tokio::spawn(async move { cli2.get(u2).send().await.unwrap().json::<Value>().await.unwrap() });

        tokio::time::sleep(Duration::from_millis(30)).await;

        // since=-1 still returns immediately
        let hb_fast: Value = cli
            .get(format!("{base}/poll?since=-1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hb_fast["type"], "HEARTBEAT");

        // Trigger sync -> wakes both pollers with identical _seq and updates last_sync
        cli.post(format!("{base}/trigger_sync"))
            .json(&json!({
                "map_id": 2,
                "map_name": "Teyvat",
                "area_name": "Mondstadt",
                "lat": 100.0,
                "lng": -200.0,
                "has_world_coords": true,
            }))
            .send()
            .await
            .unwrap();

        let ev1 = f1.await.unwrap();
        let ev2 = f2.await.unwrap();
        assert_eq!(ev1["type"], "SYNC_AND_MARK");
        assert_eq!(ev1["_seq"], ev2["_seq"]);
        let sync_seq = ev1["_seq"].as_u64().unwrap();

        // Sequential 3rd tab catching up from seq0 gets identical event
        let ev3: Value = cli
            .get(format!("{base}/poll?since={seq0}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev3["_seq"], sync_seq);

        // Check /status.last_sync updated
        let st: Value = cli.get(format!("{base}/status")).send().await.unwrap().json().await.unwrap();
        assert_eq!(st["last_sync"]["area_name"], "Mondstadt");

        // Ephemeral MINIMAP_POSITION never evicts durable events from _event_log
        let seq_before_eph = srv.latest_seq();
        for i in 0..200 {
            srv.broadcast_ephemeral(json!({"type": "MINIMAP_POSITION", "idx": i}));
        }
        // Durable SYNC_AND_MARK at sync_seq is still reachable via seq0!
        let ev_durable: Value = cli
            .get(format!("{base}/poll?since={seq0}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev_durable["_seq"], sync_seq);
        // Only latest ephemeral (idx=199) exists after seq_before_eph
        let ev_eph: Value = cli
            .get(format!("{base}/poll?since={seq_before_eph}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev_eph["type"], "MINIMAP_POSITION");
        assert_eq!(ev_eph["idx"], 199);

        // Timeout returns HEARTBEAT
        let cur_seq = srv.latest_seq();
        let hb_to: Value = cli
            .get(format!("{base}/poll?since={cur_seq}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(hb_to["type"], "HEARTBEAT");

        // Legacy cursorless /poll pops _event_queue (first the coalesced MINIMAP_POSITION, then AUTO_SYNC_UPDATED)
        srv.broadcast(json!({"type": "AUTO_SYNC_UPDATED", "auto_sync_m": true}), false, false);
        let ev_leg_eph: Value = cli
            .get(format!("{base}/poll?since=not_an_int"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev_leg_eph["type"], "MINIMAP_POSITION");
        assert_eq!(ev_leg_eph["idx"], 199);

        let ev_leg: Value = cli
            .get(format!("{base}/poll?since=not_an_int"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev_leg["type"], "AUTO_SYNC_UPDATED");

        // feedback_handled=true pre-seeds _feedback_seen dedup
        let bseq = srv.broadcast(json!({"type": "SYNC_AND_MARK"}), true, false);
        let fb_pre: Value = cli
            .post(format!("{base}/action_feedback"))
            .json(&json!({"event_type": "SYNC_AND_MARK", "status": "marked", "seq": bseq}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fb_pre["deduped"], true);

        // /shutdown wakes waiting pollers with SHUTDOWN
        let wait_seq = srv.latest_seq();
        let cli_w = cli.clone();
        let u_w = format!("{base}/poll?since={wait_seq}");
        let fw = tokio::spawn(async move { cli_w.get(u_w).send().await.unwrap().json::<Value>().await.unwrap() });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let r_shut = cli
            .post(format!("{base}/shutdown"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_shut.status(), 200);
        let ev_shut = fw.await.unwrap();
        assert_eq!(ev_shut["type"], "SHUTDOWN");
    }

    #[tokio::test]
    async fn shutdown_uninstall_extension_requires_non_browser_loopback_and_notifies_poll() {
        let cli = reqwest::Client::new();

        // 1. Native loopback caller (no Origin header) triggers UNINSTALL_EXTENSION before SHUTDOWN.
        let srv1 = SyncServer::new(SyncServerConfig {
            port: 0,
            ..Default::default()
        });
        let port1 = srv1.start().await.expect("start srv1");
        let base1 = format!("http://127.0.0.1:{port1}");
        let wait_seq1 = srv1.latest_seq();
        let cli_w1 = cli.clone();
        let u_w1 = format!("{base1}/poll?since={wait_seq1}");
        let fw1 = tokio::spawn(async move {
            cli_w1.get(u_w1).send().await.unwrap().json::<Value>().await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let resp1: Value = cli
            .post(format!("{base1}/shutdown"))
            .json(&json!({"uninstall_extension": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp1["ok"], true);
        assert_eq!(resp1["uninstall_extension_sent"], true);
        assert!(srv1.should_skip_exit_update());
        let ev1 = fw1.await.unwrap();
        assert_eq!(ev1["type"], "UNINSTALL_EXTENSION");

        // 2. Browser-originated request with Origin header must NOT trigger UNINSTALL_EXTENSION or skip_exit_update.
        let srv2 = SyncServer::new(SyncServerConfig {
            port: 0,
            ..Default::default()
        });
        let port2 = srv2.start().await.expect("start srv2");
        let base2 = format!("http://127.0.0.1:{port2}");
        let wait_seq2 = srv2.latest_seq();
        let cli_w2 = cli.clone();
        let u_w2 = format!("{base2}/poll?since={wait_seq2}");
        let fw2 = tokio::spawn(async move {
            cli_w2.get(u_w2).send().await.unwrap().json::<Value>().await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let resp2: Value = cli
            .post(format!("{base2}/shutdown"))
            .header("Origin", "https://evil.example")
            .json(&json!({"uninstall_extension": true, "skip_exit_update": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp2["ok"], true);
        assert!(resp2.get("uninstall_extension_sent").is_none());
        assert!(!srv2.should_skip_exit_update());
        let ev2 = fw2.await.unwrap();
        assert_eq!(ev2["type"], "SHUTDOWN");
    }

    struct MockQuickSwitchCb;
    impl ServerCallbacks for MockQuickSwitchCb {
        fn get_status_overrides(&self) -> Value {
            json!({"monitor_count": 2})
        }
        fn on_engine_state_change(&self, active: bool) -> CoreResult<bool> {
            Ok(active)
        }
        fn on_auto_sync_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_single_monitor_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_action_feedback(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(Value::Bool(true))
        }
        fn on_quick_switch(&self) -> CoreResult<String> {
            Ok("browser".to_owned())
        }
        fn on_shutdown(&self) {}
        fn on_set_hotkeys(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_set_language(&self, lang: &str) -> CoreResult<Value> {
            Ok(json!({"language": lang, "resolved": lang}))
        }
        fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_set_nav_hud(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"enabled": true, "position": "under_minimap"}))
        }
        fn on_set_audio(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"audio_enabled": true, "audio_volume": 50}))
        }
        fn on_nearby_pins(&self, _payload: &Value) -> CoreResult<usize> {
            Ok(0)
        }
        fn on_nav_command(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": true}))
        }
        fn on_plan_tsp_route(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ordered_pins": [], "legs": [], "teleport_hints": []}))
        }
        fn on_lan_companion(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": true, "running": false}))
        }
        fn on_launch_borderless(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": false, "launched": false, "message": "mock"}))
        }
        fn on_cross_map_export(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": true, "format": "canonical", "payload": {}, "report": {}}))
        }
    }

    #[tokio::test]
    async fn quick_switch_to_browser_emits_focus_map_tab_and_status_overrides_apply() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(200),
            callbacks: Some(Arc::new(MockQuickSwitchCb)),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        let st: Value = cli.get(format!("{base}/status")).send().await.unwrap().json().await.unwrap();
        assert_eq!(st["monitor_count"], 2);

        let seq0 = srv.latest_seq();
        let qs: Value = cli
            .post(format!("{base}/quick_switch"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(qs, json!({"ok": true, "target": "browser"}));

        let ev: Value = cli
            .get(format!("{base}/poll?since={seq0}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev["type"], "FOCUS_MAP_TAB");

        srv.stop().await;
    }

    #[test]
    fn tab_visibility_ttl_expires_stale_visible_reports() {
        let now = Instant::now();
        let mut tabs = HashMap::new();
        assert!(!any_tab_visible_at(&tabs, now, TAB_VISIBLE_TTL_SECS));
        tabs.insert("hidden".to_owned(), TabVisibility { visible: false, ts: now });
        assert!(!any_tab_visible_at(&tabs, now, TAB_VISIBLE_TTL_SECS));
        tabs.insert("fresh".to_owned(), TabVisibility { visible: true, ts: now });
        assert!(any_tab_visible_at(&tabs, now, TAB_VISIBLE_TTL_SECS));
        let later = now + Duration::from_secs_f64(TAB_VISIBLE_TTL_SECS + 1.0);
        assert!(
            !any_tab_visible_at(&tabs, later, TAB_VISIBLE_TTL_SECS),
            "a tab that stopped reporting must not keep feedback on the web"
        );
    }

    #[tokio::test]
    async fn tab_visibility_endpoint_tracks_each_tab_independently() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(120),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();
        let post = |body: Value| {
            let cli = cli.clone();
            let url = format!("{base}/tab_visibility");
            async move {
                let resp = cli.post(url).json(&body).send().await.expect("post");
                let status = resp.status();
                (status, resp.json::<Value>().await.expect("json"))
            }
        };

        assert!(!srv.any_map_tab_visible());
        let (st, _) = post(json!({"visible": true})).await;
        assert_eq!(st, 400, "tab_id is required");

        let (st, body) = post(json!({"tab_id": "a", "visible": true})).await;
        assert_eq!(st, 200);
        assert_eq!(body["any_visible"], true);
        assert!(srv.any_map_tab_visible());

        let (_, body) = post(json!({"tab_id": "b", "visible": false})).await;
        assert_eq!(body["any_visible"], true, "tab a is still visible");

        let (_, body) = post(json!({"tab_id": "a", "visible": false})).await;
        assert_eq!(body["any_visible"], false);
        assert!(!srv.any_map_tab_visible());

        srv.stop().await;
    }

    #[tokio::test]
    async fn port_takeover_on_addr_in_use_succeeds() {
        // Reserve a free ephemeral port, then drop listener to get a free port number
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("probe");
        let shared_port = probe.local_addr().unwrap().port();
        drop(probe);

        let srv_a = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: shared_port,
            poll_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        assert_eq!(srv_a.start().await.expect("start A"), shared_port);

        let srv_b = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: shared_port,
            poll_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        assert_eq!(srv_b.start().await.expect("takeover B"), shared_port);

        let st: Value = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{shared_port}/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(st["status"], "connected");

        srv_b.stop().await;
    }

    fn parse_raw_http_response(raw: &[u8]) -> (u16, HashMap<String, String>, Vec<u8>) {
        let split_pos = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("raw HTTP response header terminator");
        let head_str = std::str::from_utf8(&raw[..split_pos]).expect("utf8 headers");
        let mut lines = head_str.split("\r\n");
        let status_line = lines.next().expect("status line");
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status code");
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
        }
        (code, headers, raw[split_pos + 4..].to_vec())
    }

    fn assert_raw_cors_pna(headers: &HashMap<String, String>) {
        assert_eq!(
            headers.get("access-control-allow-origin").map(String::as_str),
            Some("*")
        );
        assert_eq!(
            headers.get("access-control-allow-methods").map(String::as_str),
            Some("GET, POST, OPTIONS")
        );
        assert_eq!(
            headers.get("access-control-allow-headers").map(String::as_str),
            Some("*")
        );
        assert_eq!(
            headers
                .get("access-control-allow-private-network")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
    }

    #[tokio::test]
    async fn options_prelistener_handles_fragmented_delayed_options_and_fragmented_get_post() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");

        // (a) Fragmented TCP OPTIONS: "OPT" + flush + 5ms + "IONS /status..."
        {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect frag options");
            stream.set_nodelay(true).expect("nodelay");
            stream.write_all(b"OPT").await.expect("write OPT");
            stream.flush().await.expect("flush OPT");
            tokio::time::sleep(Duration::from_millis(5)).await;
            stream
                .write_all(b"IONS /status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .expect("write rest options");
            stream.flush().await.expect("flush rest options");

            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.expect("read frag options");
            let (code, headers, body) = parse_raw_http_response(&raw);
            assert_eq!(code, 204);
            assert_eq!(headers.get("content-length").map(String::as_str), Some("0"));
            assert_raw_cors_pna(&headers);
            assert!(body.is_empty());
        }

        // (b) Delayed TCP OPTIONS: connect + 300ms + "OPTIONS /status..."
        {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect delayed options");
            stream.set_nodelay(true).expect("nodelay");
            tokio::time::sleep(Duration::from_millis(300)).await;
            stream
                .write_all(b"OPTIONS /status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .expect("write delayed options");
            stream.flush().await.expect("flush delayed options");

            let mut raw = Vec::new();
            stream
                .read_to_end(&mut raw)
                .await
                .expect("read delayed options");
            let (code, headers, body) = parse_raw_http_response(&raw);
            assert_eq!(code, 204);
            assert_eq!(headers.get("content-length").map(String::as_str), Some("0"));
            assert_raw_cors_pna(&headers);
            assert!(body.is_empty());
        }

        // (c) Fragmented GET via PrefixedStream
        {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect frag get");
            stream.set_nodelay(true).expect("nodelay");
            stream.write_all(b"G").await.expect("write G");
            stream.flush().await.expect("flush G");
            tokio::time::sleep(Duration::from_millis(5)).await;
            stream
                .write_all(b"ET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .expect("write rest get");
            stream.flush().await.expect("flush rest get");

            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.expect("read frag get");
            let (code, headers, body) = parse_raw_http_response(&raw);
            assert_eq!(code, 200);
            assert_raw_cors_pna(&headers);
            let parsed: Value = serde_json::from_slice(&body).expect("status json");
            assert_eq!(parsed["status"], "connected");
        }

        // (d) Fragmented POST via PrefixedStream
        {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect frag post");
            stream.set_nodelay(true).expect("nodelay");
            stream.write_all(b"P").await.expect("write P");
            stream.flush().await.expect("flush P");
            tokio::time::sleep(Duration::from_millis(5)).await;
            stream
                .write_all(
                    b"OST /report_state HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"frag\":123}",
                )
                .await
                .expect("write rest post");
            stream.flush().await.expect("flush rest post");

            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.expect("read frag post");
            let (code, headers, body) = parse_raw_http_response(&raw);
            assert_eq!(code, 200);
            assert_raw_cors_pna(&headers);
            assert_eq!(body, b"{}");
        }

        let bstate: Value = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/browser_state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(bstate["frag"], 123);

        srv.stop().await;
    }

    struct MockFeedbackCb {
        calls: AtomicUsize,
        last_payload: Mutex<Option<Value>>,
    }

    impl ServerCallbacks for MockFeedbackCb {
        fn get_status_overrides(&self) -> Value {
            json!({})
        }
        fn on_engine_state_change(&self, active: bool) -> CoreResult<bool> {
            Ok(active)
        }
        fn on_auto_sync_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_single_monitor_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_action_feedback(&self, payload: &Value) -> CoreResult<Value> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            *self.last_payload.lock().expect("last_payload lock") = Some(payload.clone());
            Ok(json!({"shown": true}))
        }
        fn on_quick_switch(&self) -> CoreResult<String> {
            Ok("none".to_owned())
        }
        fn on_shutdown(&self) {}
        fn on_set_hotkeys(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_set_language(&self, lang: &str) -> CoreResult<Value> {
            Ok(json!({"language": lang, "resolved": lang}))
        }
        fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_set_nav_hud(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_set_audio(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_nearby_pins(&self, _payload: &Value) -> CoreResult<usize> {
            Ok(0)
        }
        fn on_nav_command(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": true}))
        }
        fn on_plan_tsp_route(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_lan_companion(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_launch_borderless(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": false, "launched": false}))
        }
        fn on_cross_map_export(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn action_feedback_permissive_fields_preserve_numeric_pin_id_and_string_seq() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // Standalone mode: numeric pin_id + string seq must roll back collected pin "100001" and dedup
        let pins = json!([
            {"id": 100001, "name": "Numeric Pin", "lat": 12.0, "lng": 34.0, "map_id": 2}
        ]);
        cli.post(format!("{base}/nearby_pins"))
            .json(&json!({"map_id": 2, "pins": pins}))
            .send()
            .await
            .unwrap();
        let r_mark: Value = cli
            .post(format!("{base}/nav_command"))
            .json(&json!({"action": "mark", "pin_id": "100001"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_mark["summary"]["collected"], 1);

        let payload_num = json!({
            "event_type": "NAV_MARK",
            "status": "error",
            "seq": "9001",
            "action": "mark",
            "pin_id": 100001
        });
        let fb1: Value = cli
            .post(format!("{base}/action_feedback"))
            .json(&payload_num)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fb1["ok"], true);
        assert_eq!(fb1["deduped"], false);
        assert_eq!(fb1["shown"], true);

        let r_ref: Value = cli
            .post(format!("{base}/nav_command"))
            .json(&json!({"action": "refresh"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_ref["summary"]["collected"], 0);

        let fb2: Value = cli
            .post(format!("{base}/action_feedback"))
            .json(&payload_num)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fb2["deduped"], true);

        // Callback mode: numeric pin_id + string seq invokes on_action_feedback once with intact fields
        let cb = Arc::new(MockFeedbackCb {
            calls: AtomicUsize::new(0),
            last_payload: Mutex::new(None),
        });
        srv.set_callbacks(Some(Arc::clone(&cb) as Arc<dyn ServerCallbacks>));

        let cb_payload = json!({
            "event_type": "NAV_MARK",
            "status": "ok",
            "seq": "9002",
            "action": "mark",
            "pin_id": 200002
        });
        let cfb1: Value = cli
            .post(format!("{base}/action_feedback"))
            .json(&cb_payload)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfb1["deduped"], false);
        assert_eq!(cfb1["shown"], true);
        assert_eq!(cb.calls.load(Ordering::Acquire), 1);
        let seen_payload = cb
            .last_payload
            .lock()
            .expect("last_payload")
            .clone()
            .expect("recorded payload");
        assert_eq!(seen_payload["pin_id"], 200002);
        assert_eq!(seen_payload["seq"], "9002");
        assert_eq!(seen_payload["event_type"], "NAV_MARK");

        let cfb2: Value = cli
            .post(format!("{base}/action_feedback"))
            .json(&cb_payload)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfb2["deduped"], true);
        assert_eq!(cb.calls.load(Ordering::Acquire), 1);

        srv.stop().await;
    }

    struct MockConflictAdvisor;
    impl ConflictAdvisor for MockConflictAdvisor {
        fn check_conflicts(&self, req: &Value) -> CoreResult<Value> {
            Ok(json!({
                "ok": true,
                "checked": req.get("hotkey").cloned().unwrap_or(Value::Null),
                "severity": "high",
                "conflicts": [{"process": "nvcontainer.exe", "hotkey": "alt+z"}]
            }))
        }

        fn remap_external(&self, req: &Value) -> CoreResult<Value> {
            if req.get("fail").and_then(Value::as_bool) == Some(true) {
                return Err(CoreError::Sync("mock remap error".to_owned()));
            }
            Ok(json!({
                "ok": true,
                "remapped": true,
                "claim_updated": {
                    "action": "nav_mark",
                    "hotkey": "alt+m",
                    "display_name": "Alt+M"
                }
            }))
        }

        fn restore_external(&self, req: &Value) -> CoreResult<Value> {
            Ok(json!({
                "ok": true,
                "restored": req.get("app").cloned().unwrap_or(json!("nvidia"))
            }))
        }
    }

    struct MockDiagnosticService;
    impl DiagnosticService for MockDiagnosticService {
        fn trigger_diagnostic(&self, note: Option<&str>, source: Option<&str>) -> CoreResult<Value> {
            Ok(json!({
                "success": true,
                "folder_name": "diag_20261122_120000",
                "note": note,
                "source": source
            }))
        }

        fn save_diagnostic_note(&self, folder_name: &str, note: &str) -> CoreResult<Value> {
            if folder_name == "diag_missing" {
                return Err(CoreError::NotFound(format!("missing {folder_name}")));
            }
            Ok(json!({
                "ok": true,
                "folder_name": folder_name,
                "user_note": note.trim()
            }))
        }

        fn trigger_diagnostic_ex(&self, req: &Value) -> CoreResult<Value> {
            let note = req.get("note").and_then(Value::as_str);
            let source = req.get("source").and_then(Value::as_str);
            let category = req.get("category").and_then(Value::as_str);
            Ok(json!({
                "success": true,
                "folder_name": "diag_20261122_120000",
                "note": note,
                "source": source,
                "category": category
            }))
        }

        fn attach_diagnostic_browser(
            &self,
            folder_name: &str,
            png_bytes: &[u8],
        ) -> CoreResult<Value> {
            if folder_name == "diag_missing" {
                return Err(CoreError::NotFound(format!("missing {folder_name}")));
            }
            Ok(json!({
                "ok": true,
                "folder_name": folder_name,
                "bytes": png_bytes.len()
            }))
        }

        fn open_diagnostics_folder(&self, folder_name: Option<&str>) -> CoreResult<Value> {
            Ok(json!({
                "ok": true,
                "opened": false,
                "target": folder_name
            }))
        }
    }

    struct MockBorderlessCallbacks;
    impl ServerCallbacks for MockBorderlessCallbacks {
        fn get_status_overrides(&self) -> Value {
            json!({})
        }
        fn on_engine_state_change(&self, active: bool) -> CoreResult<bool> {
            Ok(active)
        }
        fn on_auto_sync_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_single_monitor_change(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_action_feedback(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"shown": false}))
        }
        fn on_quick_switch(&self) -> CoreResult<String> {
            Ok("none".to_owned())
        }
        fn on_shutdown(&self) {}
        fn on_set_hotkeys(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_set_language(&self, lang: &str) -> CoreResult<Value> {
            Ok(json!({"language": lang, "resolved": lang}))
        }
        fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool> {
            Ok(enabled)
        }
        fn on_set_nav_hud(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_set_audio(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_nearby_pins(&self, _payload: &Value) -> CoreResult<usize> {
            Ok(0)
        }
        fn on_nav_command(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({"ok": true}))
        }
        fn on_plan_tsp_route(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_lan_companion(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
        fn on_launch_borderless(&self, payload: &Value) -> CoreResult<Value> {
            let dry_run = payload.get("dry_run").and_then(Value::as_bool) == Some(true);
            Ok(json!({
                "ok": true,
                "dry_run": dry_run,
                "message": if dry_run { "Dry run ok" } else { "Borderless applied" }
            }))
        }
        fn on_cross_map_export(&self, _payload: &Value) -> CoreResult<Value> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn trait_delegation_conflict_advisor_diagnostic_service_and_callbacks() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(200),
            callbacks: Some(Arc::new(MockBorderlessCallbacks)),
            conflict_advisor: Some(Arc::new(MockConflictAdvisor)),
            diagnostic_service: Some(Arc::new(MockDiagnosticService)),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // /check_hotkey_conflicts
        let r_chk: Value = cli
            .post(format!("{base}/check_hotkey_conflicts"))
            .json(&json!({"hotkey": "alt+z"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_chk["ok"], true);
        assert_eq!(r_chk["severity"], "high");
        assert_eq!(r_chk["checked"], "alt+z");

        // /remap_external_hotkey (claim_updated -> HOTKEY_UPDATED broadcast)
        let seq0 = srv.latest_seq();
        let r_remap: Value = cli
            .post(format!("{base}/remap_external_hotkey"))
            .json(&json!({"action": "nav_mark"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_remap["ok"], true);
        assert_eq!(r_remap["remapped"], true);

        let ev: Value = cli
            .get(format!("{base}/poll?since={seq0}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ev["type"], "HOTKEY_UPDATED");
        assert_eq!(ev["action"], "nav_mark");
        assert_eq!(ev["hotkey"], "alt+m");
        assert_eq!(ev["display_name"], "Alt+M");

        let r_remap_err = cli
            .post(format!("{base}/remap_external_hotkey"))
            .json(&json!({"fail": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_remap_err.status(), 400);

        // /restore_external_hotkey
        let r_rest: Value = cli
            .post(format!("{base}/restore_external_hotkey"))
            .json(&json!({"app": "nvidia"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_rest["ok"], true);
        assert_eq!(r_rest["restored"], "nvidia");

        // /trigger_diagnostic
        let r_diag: Value = cli
            .post(format!("{base}/trigger_diagnostic"))
            .json(&json!({"note": "test note", "source": "popup", "category": "wrong_floor"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_diag["ok"], true);
        assert_eq!(r_diag["bundle"]["folder_name"], "diag_20261122_120000");
        assert_eq!(r_diag["bundle"]["note"], "test note");
        assert_eq!(r_diag["bundle"]["source"], "popup");
        assert_eq!(r_diag["bundle"]["category"], "wrong_floor");

        // /attach_diagnostic_browser: method guard 403, traversal 400, bad base64/magic 400, valid PNG 200, missing 404
        let r_att_get = cli
            .get(format!("{base}/attach_diagnostic_browser"))
            .send()
            .await
            .unwrap();
        assert_eq!(r_att_get.status(), 403);

        let r_att_trav = cli
            .post(format!("{base}/attach_diagnostic_browser"))
            .json(&json!({"folder_name": "../bad", "png_base64": "iVBORw0KGgo="}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_att_trav.status(), 400);

        let r_att_non_png = cli
            .post(format!("{base}/attach_diagnostic_browser"))
            .json(&json!({"folder_name": "diag_20261122_120000", "png_base64": "SGVsbG8="}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_att_non_png.status(), 400);

        let r_att_ok = cli
            .post(format!("{base}/attach_diagnostic_browser"))
            .json(&json!({
                "folder_name": "diag_20261122_120000",
                "png_base64": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r_att_ok.status(), 200);
        let j_att_ok: Value = r_att_ok.json().await.unwrap();
        assert_eq!(j_att_ok["ok"], true);
        assert_eq!(j_att_ok["bytes"], 16);

        let r_att_404 = cli
            .post(format!("{base}/attach_diagnostic_browser"))
            .json(&json!({
                "folder_name": "diag_missing",
                "png_base64": "iVBORw0KGgo="
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r_att_404.status(), 404);

        // /open_diagnostics: method guard 403, traversal 400, valid 200
        assert_eq!(
            cli.get(format!("{base}/open_diagnostics")).send().await.unwrap().status(),
            403
        );
        assert_eq!(
            cli.post(format!("{base}/open_diagnostics"))
                .json(&json!({"folder_name": "../bad"}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let r_open_ok = cli
            .post(format!("{base}/open_diagnostics"))
            .json(&json!({"folder_name": "diag_20261122_120000"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_open_ok.status(), 200);
        assert_eq!(r_open_ok.json::<Value>().await.unwrap()["target"], "diag_20261122_120000");

        // /save_diagnostic_note (200 valid + 404 NotFound)
        let r_note_ok = cli
            .post(format!("{base}/save_diagnostic_note"))
            .json(&json!({"folder_name": "diag_20261122_120000", "note": "Ghi chú mock"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_note_ok.status(), 200);
        let j_note_ok: Value = r_note_ok.json().await.unwrap();
        assert_eq!(j_note_ok["user_note"], "Ghi chú mock");

        let r_note_404 = cli
            .post(format!("{base}/save_diagnostic_note"))
            .json(&json!({"folder_name": "diag_missing", "note": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r_note_404.status(), 404);

        // /launch_borderless (dry_run vs live)
        let r_dry: Value = cli
            .post(format!("{base}/launch_borderless"))
            .json(&json!({"dry_run": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_dry["ok"], true);
        assert_eq!(r_dry["launched"], false);
        assert_eq!(r_dry["message"], "Dry run ok");

        let r_live: Value = cli
            .post(format!("{base}/launch_borderless"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r_live["ok"], true);
        assert_eq!(r_live["launched"], true);
        assert_eq!(r_live["message"], "Borderless applied");

        srv.stop().await;
    }

    #[tokio::test]
    async fn cross_browser_pin_owner_blocks_background_browser_from_overwriting_active_browser() {
        let srv = SyncServer::new(SyncServerConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            poll_timeout: Duration::from_millis(120),
            ..Default::default()
        });
        let port = srv.start().await.expect("start");
        let base = format!("http://127.0.0.1:{port}");
        let cli = reqwest::Client::new();

        // Browser A (Chrome, Oculi ON) initially claims ownership.
        let r1: Value = cli
            .post(format!("{base}/nearby_pins"))
            .json(&json!({
                "tab_id": "chrome_tab_a",
                "tab_visible": true,
                "tab_focused": true,
                "user_initiated": true,
                "map_id": 2,
                "lat": 10.0,
                "lng": 20.0,
                "pins": [{"id": "oculus_1", "name": "Oculus", "lat": 10.0, "lng": 20.0}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r1["count"], 1);

        // User switches to Browser B (Edge, Oculi OFF, Chest ON) and clicks filter.
        let r2: Value = cli
            .post(format!("{base}/nearby_pins"))
            .json(&json!({
                "tab_id": "edge_tab_b",
                "tab_visible": true,
                "tab_focused": true,
                "user_initiated": true,
                "map_id": 2,
                "lat": 10.0,
                "lng": 20.0,
                "pins": [{"id": "chest_1", "name": "Chest", "lat": 11.0, "lng": 21.0}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r2["count"], 1);
        assert!(r2.get("ignored_secondary_client").is_none());

        // Passive background push from Browser A (still has Oculi ON!) MUST be ignored!
        let r3: Value = cli
            .post(format!("{base}/nearby_pins"))
            .json(&json!({
                "tab_id": "chrome_tab_a",
                "tab_visible": true,
                "tab_focused": false,
                "user_initiated": false,
                "map_id": 2,
                "lat": 10.0,
                "lng": 20.0,
                "pins": [{"id": "oculus_1", "name": "Oculus", "lat": 10.0, "lng": 20.0}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r3["ignored_secondary_client"], true);

        let r4: Value = cli
            .post(format!("{base}/lan_pins"))
            .json(&json!({
                "tab_id": "chrome_tab_a",
                "tab_visible": true,
                "tab_focused": false,
                "user_initiated": false,
                "map_id": 2,
                "names": ["Oculus"],
                "pins": [["oculus_1", 10.0, 20.0, 5, null, 0, 0, 0]]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r4["ignored_secondary_client"], true);

        // Active browser B closes -> releases ownership so Browser A can cleanly take over.
        let _ = cli
            .post(format!("{base}/tab_visibility"))
            .json(&json!({"tab_id": "edge_tab_b", "visible": false, "closing": true}))
            .send()
            .await
            .unwrap();
        let r5: Value = cli
            .post(format!("{base}/nearby_pins"))
            .json(&json!({
                "tab_id": "chrome_tab_a",
                "tab_visible": true,
                "tab_focused": false,
                "user_initiated": false,
                "map_id": 2,
                "lat": 10.0,
                "lng": 20.0,
                "pins": [{"id": "oculus_1", "name": "Oculus", "lat": 10.0, "lng": 20.0}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r5["count"], 1);
        assert!(r5.get("ignored_secondary_client").is_none());

        srv.stop().await;
    }
}
