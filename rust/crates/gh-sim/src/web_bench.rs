//! HoYoLAB web-map sync latency benchmark (`gh-sim web-scenario` and `gh-sim web-bench`).
//!
//! - `web-scenario` replays recorded sessions through the same production path `gh-sim qa` uses
//!   and extracts every M-Map `SYNC_POSITION_ONLY` payload in order.
//! - `web-bench` starts a standalone `gh-net::SyncServer` (refusing with exit 2 if the port is
//!   already bound, never sending `/shutdown` or taking over the port), waits for the browser
//!   extension to poll, warms up with the first scenario event, then measures round-trip
//!   `SYNC_POSITION_ONLY` feedback latency classified as `same_map` or `map_switch`.

use crate::qa::discover_sessions;
use crate::replay::{
    DEFAULT_DRIFT_THRESHOLD_W, ReplayOptions, percentile, round3, run_replay_full,
};
use gh_app::tracking_loop::TickPostSummary;
use gh_core::atomic_io::write_json_atomic;
use gh_core::traits::ServerCallbacks;
use gh_core::{CoreResult, DEFAULT_SYNC_HOST, DEFAULT_SYNC_PORT};
use gh_net::sync_server::{SyncServer, SyncServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub const WEB_SCENARIO_SCHEMA_VERSION: u32 = 1;
pub const WEB_BENCH_REPORT_SCHEMA_VERSION: u32 = 1;
pub const WEB_BENCH_REPORT_FILENAME: &str = "web_bench_report.json";
pub const DEFAULT_WEB_BENCH_PORT: u16 = DEFAULT_SYNC_PORT;
pub const DEFAULT_CONNECT_TIMEOUT_S: f64 = 60.0;
pub const DEFAULT_EVENT_TIMEOUT_S: f64 = 15.0;
pub const DEFAULT_GAP_S: f64 = 1.5;

const WARMUP_RETRY_INTERVAL: Duration = Duration::from_secs(6);
const POLL_CHECK_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct WebScenarioOptions {
    pub sessions_dir: PathBuf,
    pub data_dir: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub only: Vec<String>,
    pub out_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebScenarioEvent {
    pub session: String,
    pub frame_idx: usize,
    pub ts: f64,
    pub map_id: i32,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebScenarioFile {
    pub schema_version: u32,
    pub sessions: Vec<String>,
    pub events: Vec<WebScenarioEvent>,
}

/// Extracts ordered [`WebScenarioEvent`]s from synthetic or replayed `(frame_idx, ts, TickPostSummary)`
/// tuples for a single session.
pub fn extract_scenario_events_from_summaries(
    session: &str,
    ticks: &[(usize, f64, TickPostSummary)],
) -> Vec<WebScenarioEvent> {
    let mut out = Vec::new();
    for (frame_idx, ts, summary) in ticks {
        let Some(ref payload) = summary.map_m_sync_payload else {
            continue;
        };
        let map_id = summary
            .map_m_anchor
            .map(|(mid, _, _, _)| mid)
            .or_else(|| payload.get("map_id").and_then(Value::as_i64).map(|v| v as i32))
            .unwrap_or(2);
        out.push(WebScenarioEvent {
            session: session.to_owned(),
            frame_idx: *frame_idx,
            ts: *ts,
            map_id,
            payload: payload.clone(),
        });
    }
    out
}

/// Runs `gh-sim web-scenario`, replaying all discovered sessions and writing the extracted
/// `SYNC_POSITION_ONLY` scenario JSON atomically to `opts.out_path`.
pub fn run_web_scenario(opts: &WebScenarioOptions) -> Result<WebScenarioFile, String> {
    let session_dirs = discover_sessions(&opts.sessions_dir, &opts.only)?;
    let mut sessions = Vec::with_capacity(session_dirs.len());
    let mut events = Vec::new();

    for sdir in &session_dirs {
        let session_name = sdir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session")
            .to_owned();
        let replay_opts = ReplayOptions {
            source_dir: sdir.clone(),
            data_dir: opts.data_dir.clone(),
            gt_file: PathBuf::from("tests/verified_gt.json"),
            tiles_dir: None,
            ug_dir: None,
            paimon_cache_path: None,
            ffmpeg_path: opts.ffmpeg_path.clone(),
            max_frames: None,
            drift_threshold_w: DEFAULT_DRIFT_THRESHOLD_W,
            verify_mmap_view: false,
            render_out: None,
            out_report: None,
        };
        let (_report, _poses, map_m_syncs) = run_replay_full(&replay_opts)?;
        for rec in map_m_syncs {
            events.push(WebScenarioEvent {
                session: session_name.clone(),
                frame_idx: rec.frame_idx,
                ts: rec.ts,
                map_id: rec.map_id,
                payload: rec.payload,
            });
        }
        sessions.push(session_name);
    }

    let scenario = WebScenarioFile {
        schema_version: WEB_SCENARIO_SCHEMA_VERSION,
        sessions,
        events,
    };
    let val = serde_json::to_value(&scenario)
        .map_err(|e| format!("failed to serialize web scenario: {e}"))?;
    write_json_atomic(&opts.out_path, &val)
        .map_err(|e| format!("failed to write {}: {e}", opts.out_path.display()))?;

    println!(
        "scenario: {} events ({} sessions) -> {}",
        scenario.events.len(),
        scenario.sessions.len(),
        opts.out_path.display()
    );
    let _ = std::io::stdout().flush();

    Ok(scenario)
}

#[derive(Debug, Clone)]
pub struct WebBenchOptions {
    pub scenario_path: PathBuf,
    pub port: u16,
    pub connect_timeout_s: f64,
    pub event_timeout_s: f64,
    pub gap_s: f64,
    pub limit: Option<usize>,
    pub out_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebBenchClass {
    SameMap,
    MapSwitch,
}

impl WebBenchClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameMap => "same_map",
            Self::MapSwitch => "map_switch",
        }
    }
}

pub fn classify_transition(from_map: i32, to_map: i32) -> WebBenchClass {
    if from_map == to_map {
        WebBenchClass::SameMap
    } else {
        WebBenchClass::MapSwitch
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebBenchWarmupResult {
    pub session: String,
    pub frame_idx: usize,
    pub ts: f64,
    pub map_id: i32,
    pub seq: u64,
    pub status: String,
    pub latency_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebBenchEventResult {
    pub idx: usize,
    pub session: String,
    pub frame_idx: usize,
    pub ts: f64,
    pub seq: u64,
    pub from_map: i32,
    pub to_map: i32,
    pub class: WebBenchClass,
    pub status: String,
    pub ok: bool,
    pub latency_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebBenchClassSummary {
    pub count: usize,
    pub synced: usize,
    pub failures: usize,
    pub median_s: Option<f64>,
    pub max_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebBenchSummary {
    pub total: usize,
    pub synced: usize,
    pub failures: usize,
    pub same_map: WebBenchClassSummary,
    pub map_switch: WebBenchClassSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebBenchReport {
    pub schema_version: u32,
    pub scenario_path: String,
    pub port: u16,
    pub connect_timeout_s: f64,
    pub event_timeout_s: f64,
    pub gap_s: f64,
    pub limit: Option<usize>,
    pub passed: bool,
    pub warmup: WebBenchWarmupResult,
    pub events: Vec<WebBenchEventResult>,
    pub summary: WebBenchSummary,
}

pub fn summarize_web_bench_events(events: &[WebBenchEventResult]) -> WebBenchSummary {
    let summarize_class = |cls: WebBenchClass| -> WebBenchClassSummary {
        let mut count = 0usize;
        let mut synced = 0usize;
        let mut failures = 0usize;
        let mut latencies = Vec::new();
        for ev in events.iter().filter(|e| e.class == cls) {
            count += 1;
            if ev.ok {
                synced += 1;
                if let Some(lat) = ev.latency_s.filter(|v| v.is_finite()) {
                    latencies.push(lat);
                }
            } else {
                failures += 1;
            }
        }
        WebBenchClassSummary {
            count,
            synced,
            failures,
            median_s: percentile(&latencies, 0.50).map(round3),
            max_s: latencies.iter().copied().reduce(f64::max).map(round3),
        }
    };

    let same_map = summarize_class(WebBenchClass::SameMap);
    let map_switch = summarize_class(WebBenchClass::MapSwitch);
    let total = events.len();
    let synced = same_map.synced + map_switch.synced;
    let failures = same_map.failures + map_switch.failures;

    WebBenchSummary {
        total,
        synced,
        failures,
        same_map,
        map_switch,
    }
}

/// Checks whether `127.0.0.1:<port>` is free without connecting or sending `/shutdown`.
pub fn check_port_available(port: u16) -> Result<(), String> {
    if port == 0 {
        return Ok(());
    }
    match TcpListener::bind((DEFAULT_SYNC_HOST, port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(err) => Err(format!(
            "Port {DEFAULT_SYNC_HOST}:{port} is already in use ({err}). Please close GenshinHelper (or any process listening on port {port}) before running web-bench."
        )),
    }
}

pub fn load_web_scenario(path: &Path) -> Result<Vec<WebScenarioEvent>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read scenario {}: {e}", path.display()))?;
    let val: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse scenario {}: {e}", path.display()))?;
    let events: Vec<WebScenarioEvent> = if val.is_array() {
        serde_json::from_value(val)
            .map_err(|e| format!("invalid scenario array {}: {e}", path.display()))?
    } else {
        let parsed: WebScenarioFile = serde_json::from_value(val)
            .map_err(|e| format!("invalid scenario file {}: {e}", path.display()))?;
        parsed.events
    };
    if events.is_empty() {
        return Err(format!("scenario {} contains 0 events", path.display()));
    }
    Ok(events)
}

#[derive(Debug, Clone)]
struct FeedbackRecord {
    arrived_at: Instant,
    event_type: String,
    status: String,
    seq: Option<u64>,
}

fn parse_seq_field(val: Option<&Value>) -> Option<u64> {
    match val {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|v| u64::try_from(v).ok()))
            .or_else(|| n.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

struct FeedbackRecorder {
    records: Mutex<Vec<FeedbackRecord>>,
    notify: Notify,
}

impl FeedbackRecorder {
    fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            notify: Notify::new(),
        }
    }

    fn push(&self, rec: FeedbackRecord) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(rec);
        self.notify.notify_waiters();
    }

    fn find_matching_seq(&self, start_idx: usize, expected_seqs: &[u64]) -> Option<FeedbackRecord> {
        let guard = self.records.lock().unwrap_or_else(|e| e.into_inner());
        guard[start_idx..]
            .iter()
            .find(|r| {
                r.event_type == "SYNC_POSITION_ONLY"
                    && r.seq.is_some_and(|s| expected_seqs.contains(&s))
            })
            .cloned()
    }

    fn len(&self) -> usize {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    async fn wait_for_sync_feedback(
        &self,
        start_idx: usize,
        expected_seqs: &[u64],
        deadline: Instant,
    ) -> Option<FeedbackRecord> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(found) = self.find_matching_seq(start_idx, expected_seqs) {
                return Some(found);
            }

            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let rem = deadline.saturating_duration_since(now);
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(rem) => {
                    return self.find_matching_seq(start_idx, expected_seqs);
                }
            }
        }
    }
}

struct WebBenchCallbacks {
    recorder: Arc<FeedbackRecorder>,
}

impl ServerCallbacks for WebBenchCallbacks {
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
        let arrived_at = Instant::now();
        let event_type = payload
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let seq = parse_seq_field(payload.get("seq"));
        self.recorder.push(FeedbackRecord {
            arrived_at,
            event_type,
            status,
            seq,
        });
        Ok(json!({"shown": false}))
    }

    fn on_quick_switch(&self) -> CoreResult<String> {
        Ok("none".to_owned())
    }

    fn on_shutdown(&self) {}

    fn on_set_hotkeys(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": true}))
    }

    fn on_set_language(&self, lang: &str) -> CoreResult<Value> {
        Ok(json!({"ok": true, "language": lang}))
    }

    fn on_set_minimap_tracking(&self, enabled: bool) -> CoreResult<bool> {
        Ok(enabled)
    }

    fn on_set_nav_hud(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": true}))
    }

    fn on_set_audio(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": true}))
    }

    fn on_nearby_pins(&self, _payload: &Value) -> CoreResult<usize> {
        Ok(0)
    }

    fn on_nav_command(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": true}))
    }

    fn on_plan_tsp_route(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": false, "error": "not available in web-bench"}))
    }

    fn on_lan_companion(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": false}))
    }

    fn on_launch_borderless(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": false}))
    }

    fn on_cross_map_export(&self, _payload: &Value) -> CoreResult<Value> {
        Ok(json!({"ok": false}))
    }
}

fn fmt_opt_s(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}s"))
        .unwrap_or_else(|| "-".to_owned())
}

pub fn format_web_bench_table_and_summary(report: &WebBenchReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "warm-up: status={} map={} seq={} latency={:.3}s (excluded from metrics)",
        report.warmup.status, report.warmup.map_id, report.warmup.seq, report.warmup.latency_s
    ));
    lines.push(format!(
        "{:<4} {:>5}  {:<22} {:>6}  {:>4} -> {:<4}  {:<11} {:<10} {:>8}",
        "#", "Seq", "Session", "Frame", "From", "To", "Class", "Status", "Latency"
    ));
    lines.push("-".repeat(86));
    for ev in &report.events {
        lines.push(format!(
            "{:<4} {:>5}  {:<22} {:>6}  {:>4} -> {:<4}  {:<11} {:<10} {:>8}",
            ev.idx,
            ev.seq,
            ev.session,
            ev.frame_idx,
            ev.from_map,
            ev.to_map,
            ev.class.as_str(),
            ev.status,
            fmt_opt_s(ev.latency_s),
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "{:<12} {:>6} {:>9} {:>9} {:>9}",
        "class", "count", "median", "max", "failures"
    ));
    lines.push("-".repeat(49));
    for (name, s) in [
        ("same_map", &report.summary.same_map),
        ("map_switch", &report.summary.map_switch),
    ] {
        lines.push(format!(
            "{:<12} {:>6} {:>9} {:>9} {:>9}",
            name,
            s.count,
            fmt_opt_s(s.median_s),
            fmt_opt_s(s.max_s),
            s.failures
        ));
    }
    lines.join("\n")
}

/// Runs `gh-sim web-bench` asynchronously (used by both the CLI and in-process unit tests).
pub async fn run_web_bench_async(opts: &WebBenchOptions) -> Result<WebBenchReport, String> {
    if !opts.connect_timeout_s.is_finite() || opts.connect_timeout_s <= 0.0 {
        return Err(format!(
            "--connect-timeout-s must be finite and > 0 (got {})",
            opts.connect_timeout_s
        ));
    }
    if !opts.event_timeout_s.is_finite() || opts.event_timeout_s <= 0.0 {
        return Err(format!(
            "--event-timeout-s must be finite and > 0 (got {})",
            opts.event_timeout_s
        ));
    }
    if !opts.gap_s.is_finite() || opts.gap_s < 0.0 {
        return Err(format!(
            "--gap-s must be finite and >= 0 (got {})",
            opts.gap_s
        ));
    }

    // Refuse immediately BEFORE touching SyncServer::start() (which uses bind_with_takeover).
    check_port_available(opts.port)?;

    let scenario_events = load_web_scenario(&opts.scenario_path)?;
    let recorder = Arc::new(FeedbackRecorder::new());
    let callbacks: Arc<dyn ServerCallbacks> = Arc::new(WebBenchCallbacks {
        recorder: Arc::clone(&recorder),
    });
    let server = SyncServer::new(SyncServerConfig {
        host: DEFAULT_SYNC_HOST.to_owned(),
        port: opts.port,
        callbacks: Some(callbacks),
        ..Default::default()
    });

    let actual_port = server
        .start()
        .await
        .map_err(|e| format!("failed to start SyncServer on {}:{}: {e}", DEFAULT_SYNC_HOST, opts.port))?;

    let result = run_web_bench_on_server(opts, &server, &recorder, actual_port, &scenario_events).await;
    server.stop().await;
    result
}

async fn run_web_bench_on_server(
    opts: &WebBenchOptions,
    server: &SyncServer,
    recorder: &FeedbackRecorder,
    actual_port: u16,
    scenario_events: &[WebScenarioEvent],
) -> Result<WebBenchReport, String> {
    let connect_deadline = Instant::now() + Duration::from_secs_f64(opts.connect_timeout_s);

    // Wait until the extension has actively parked in /poll.
    while server.active_poll_waiters() == 0 {
        if Instant::now() >= connect_deadline {
            return Err(format!(
                "timed out after {:.1}s waiting for browser extension to poll http://{DEFAULT_SYNC_HOST}:{actual_port}/poll",
                opts.connect_timeout_s
            ));
        }
        tokio::time::sleep(POLL_CHECK_INTERVAL).await;
    }

    // Warm-up: broadcast the first scenario event and wait up to connect-timeout for `synced`.
    // Retry periodically if the HoYoLAB page is still mounting its Leaflet map on cold start.
    let warmup_ev = &scenario_events[0];
    let warmup_start_idx = recorder.len();
    let mut warmup_seqs: Vec<(u64, Instant)> = Vec::new();
    let warmup_result = loop {
        let sent_at = Instant::now();
        let seq = server.broadcast(warmup_ev.payload.clone(), false, false);
        warmup_seqs.push((seq, sent_at));
        let seq_ids: Vec<u64> = warmup_seqs.iter().map(|(s, _)| *s).collect();

        let retry_deadline = (Instant::now() + WARMUP_RETRY_INTERVAL).min(connect_deadline);
        if let Some(fb) = recorder
            .wait_for_sync_feedback(warmup_start_idx, &seq_ids, retry_deadline)
            .await
        {
            let matched_seq = fb.seq.unwrap_or(seq);
            let matched_sent_at = warmup_seqs
                .iter()
                .find(|(s, _)| *s == matched_seq)
                .map(|(_, t)| *t)
                .unwrap_or(sent_at);
            if fb.status != "synced" {
                return Err(format!(
                    "warm-up event (seq={matched_seq}, map={}) returned status '{}' instead of 'synced'",
                    warmup_ev.map_id, fb.status
                ));
            }
            break WebBenchWarmupResult {
                session: warmup_ev.session.clone(),
                frame_idx: warmup_ev.frame_idx,
                ts: warmup_ev.ts,
                map_id: warmup_ev.map_id,
                seq: matched_seq,
                status: fb.status,
                latency_s: round3(
                    fb.arrived_at
                        .saturating_duration_since(matched_sent_at)
                        .as_secs_f64(),
                ),
            };
        }

        if Instant::now() >= connect_deadline {
            return Err(format!(
                "timed out after {:.1}s waiting for warm-up SYNC_POSITION_ONLY 'synced' feedback (map={})",
                opts.connect_timeout_s, warmup_ev.map_id
            ));
        }
    };

    let measured_slice = &scenario_events[1..];
    let measured_count = opts
        .limit
        .map_or(measured_slice.len(), |lim| lim.min(measured_slice.len()));
    let measured_events = &measured_slice[..measured_count];

    if !measured_events.is_empty() && opts.gap_s > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(opts.gap_s)).await;
    }

    let mut results = Vec::with_capacity(measured_events.len());
    let mut prev_map_id = warmup_ev.map_id;
    let event_timeout = Duration::from_secs_f64(opts.event_timeout_s);

    for (i, ev) in measured_events.iter().enumerate() {
        let from_map = prev_map_id;
        let to_map = ev.map_id;
        prev_map_id = to_map;
        let class = classify_transition(from_map, to_map);

        let start_idx = recorder.len();
        let sent_at = Instant::now();
        let seq = server.broadcast(ev.payload.clone(), false, false);
        let deadline = sent_at + event_timeout;

        let (status, ok, latency_s) = match recorder
            .wait_for_sync_feedback(start_idx, &[seq], deadline)
            .await
        {
            Some(fb) => {
                let lat = round3(
                    fb.arrived_at
                        .saturating_duration_since(sent_at)
                        .as_secs_f64(),
                );
                let ok = fb.status == "synced";
                (fb.status, ok, Some(lat))
            }
            None => ("timeout".to_owned(), false, None),
        };

        results.push(WebBenchEventResult {
            idx: i + 1,
            session: ev.session.clone(),
            frame_idx: ev.frame_idx,
            ts: ev.ts,
            seq,
            from_map,
            to_map,
            class,
            status,
            ok,
            latency_s,
        });

        if i + 1 < measured_events.len() && opts.gap_s > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(opts.gap_s)).await;
        }
    }

    let summary = summarize_web_bench_events(&results);
    let passed = summary.failures == 0;
    let report = WebBenchReport {
        schema_version: WEB_BENCH_REPORT_SCHEMA_VERSION,
        scenario_path: opts.scenario_path.display().to_string(),
        port: actual_port,
        connect_timeout_s: opts.connect_timeout_s,
        event_timeout_s: opts.event_timeout_s,
        gap_s: opts.gap_s,
        limit: opts.limit,
        passed,
        warmup: warmup_result,
        events: results,
        summary,
    };

    let formatted = format_web_bench_table_and_summary(&report);
    println!("{formatted}");
    let _ = std::io::stdout().flush();

    if let Some(ref out_dir) = opts.out_dir {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| format!("failed to create out dir {}: {e}", out_dir.display()))?;
        let report_path = out_dir.join(WEB_BENCH_REPORT_FILENAME);
        let val = serde_json::to_value(&report)
            .map_err(|e| format!("failed to serialize web bench report: {e}"))?;
        write_json_atomic(&report_path, &val)
            .map_err(|e| format!("failed to write {}: {e}", report_path.display()))?;
    }

    Ok(report)
}

/// Synchronous CLI entry point for `gh-sim web-bench`.
pub fn run_web_bench(opts: &WebBenchOptions) -> Result<WebBenchReport, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create Tokio runtime: {e}"))?;
    rt.block_on(run_web_bench_async(opts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gh_sim_web_bench_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_payload(map_id: i32, lat: f64, lng: f64) -> Value {
        json!({
            "type": "SYNC_POSITION_ONLY",
            "detected": true,
            "has_world_coords": true,
            "map_id": map_id,
            "map_name": format!("Map {map_id}"),
            "lat": lat,
            "lng": lng,
            "world_x": lng,
            "world_y": -lat,
        })
    }

    #[test]
    fn web_scenario_extracts_ordered_events_with_map_ids_from_synthetic_summaries() {
        let s1 = TickPostSummary {
            map_m_synced: true,
            map_m_anchor: Some((40, 10.0, 20.0, false)),
            map_m_sync_payload: Some(sample_payload(40, -20.0, 10.0)),
            ..Default::default()
        };

        let s2 = TickPostSummary::default(); // non-sync tick must be skipped

        let s3 = TickPostSummary {
            map_m_synced: true,
            map_m_anchor: None, // fallback to payload["map_id"]
            map_m_sync_payload: Some(sample_payload(34, 50.0, -30.0)),
            ..Default::default()
        };

        let s4 = TickPostSummary {
            map_m_synced: true,
            pending_verify_corrected: true,
            map_m_anchor: Some((2, 100.0, 200.0, true)),
            map_m_sync_payload: Some(sample_payload(2, -200.0, 100.0)),
            ..Default::default()
        };

        let ticks = vec![
            (10usize, 1.25f64, s1),
            (11usize, 1.50f64, s2),
            (25usize, 4.00f64, s3),
            (29usize, 4.40f64, s4),
        ];

        let events = extract_scenario_events_from_summaries("sess_synth", &ticks);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].session, "sess_synth");
        assert_eq!(events[0].frame_idx, 10);
        assert_eq!(events[0].ts, 1.25);
        assert_eq!(events[0].map_id, 40);
        assert_eq!(events[0].payload["type"], "SYNC_POSITION_ONLY");

        assert_eq!(events[1].frame_idx, 25);
        assert_eq!(events[1].map_id, 34);

        assert_eq!(events[2].frame_idx, 29);
        assert_eq!(events[2].map_id, 2);
    }

    #[test]
    fn web_bench_classifies_transitions_and_summarizes_classes() {
        assert_eq!(classify_transition(2, 2), WebBenchClass::SameMap);
        assert_eq!(classify_transition(2, 40), WebBenchClass::MapSwitch);
        assert_eq!(classify_transition(40, 34), WebBenchClass::MapSwitch);

        let events = vec![
            WebBenchEventResult {
                idx: 1,
                session: "s".to_owned(),
                frame_idx: 1,
                ts: 1.0,
                seq: 2,
                from_map: 2,
                to_map: 2,
                class: WebBenchClass::SameMap,
                status: "synced".to_owned(),
                ok: true,
                latency_s: Some(0.10),
            },
            WebBenchEventResult {
                idx: 2,
                session: "s".to_owned(),
                frame_idx: 2,
                ts: 2.0,
                seq: 3,
                from_map: 2,
                to_map: 2,
                class: WebBenchClass::SameMap,
                status: "synced".to_owned(),
                ok: true,
                latency_s: Some(0.20),
            },
            WebBenchEventResult {
                idx: 3,
                session: "s".to_owned(),
                frame_idx: 3,
                ts: 3.0,
                seq: 4,
                from_map: 2,
                to_map: 2,
                class: WebBenchClass::SameMap,
                status: "synced".to_owned(),
                ok: true,
                latency_s: Some(0.30),
            },
            WebBenchEventResult {
                idx: 4,
                session: "s".to_owned(),
                frame_idx: 4,
                ts: 4.0,
                seq: 5,
                from_map: 2,
                to_map: 40,
                class: WebBenchClass::MapSwitch,
                status: "synced".to_owned(),
                ok: true,
                latency_s: Some(1.50),
            },
            WebBenchEventResult {
                idx: 5,
                session: "s".to_owned(),
                frame_idx: 5,
                ts: 5.0,
                seq: 6,
                from_map: 40,
                to_map: 34,
                class: WebBenchClass::MapSwitch,
                status: "timeout".to_owned(),
                ok: false,
                latency_s: None,
            },
        ];

        let summary = summarize_web_bench_events(&events);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.synced, 4);
        assert_eq!(summary.failures, 1);

        assert_eq!(summary.same_map.count, 3);
        assert_eq!(summary.same_map.synced, 3);
        assert_eq!(summary.same_map.failures, 0);
        assert_eq!(summary.same_map.median_s, Some(0.2));
        assert_eq!(summary.same_map.max_s, Some(0.3));

        assert_eq!(summary.map_switch.count, 2);
        assert_eq!(summary.map_switch.synced, 1);
        assert_eq!(summary.map_switch.failures, 1);
        assert_eq!(summary.map_switch.median_s, Some(1.5));
        assert_eq!(summary.map_switch.max_s, Some(1.5));
    }

    #[test]
    fn web_bench_refuses_busy_port_without_sending_shutdown() {
        let tmp = unique_tmp_dir("busy_port");
        let scenario_path = tmp.join("scenario.json");
        let scenario = WebScenarioFile {
            schema_version: WEB_SCENARIO_SCHEMA_VERSION,
            sessions: vec!["sess_busy".to_owned()],
            events: vec![WebScenarioEvent {
                session: "sess_busy".to_owned(),
                frame_idx: 1,
                ts: 1.0,
                map_id: 2,
                payload: sample_payload(2, 0.0, 0.0),
            }],
        };
        write_json_atomic(
            &scenario_path,
            &serde_json::to_value(&scenario).expect("serialize scenario"),
        )
        .expect("write scenario");

        let listener = TcpListener::bind((DEFAULT_SYNC_HOST, 0)).expect("bind busy port");
        let busy_port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");

        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop_bg = Arc::clone(&stop);
        let accepted_bg = Arc::clone(&accepted);
        let watcher = std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((_stream, _addr)) => {
                        accepted_bg.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = run_web_bench(&WebBenchOptions {
            scenario_path,
            port: busy_port,
            connect_timeout_s: 2.0,
            event_timeout_s: 1.0,
            gap_s: 0.0,
            limit: None,
            out_dir: None,
        })
        .expect_err("busy port must fail with setup error");

        stop.store(true, Ordering::Relaxed);
        let _ = watcher.join();

        assert!(
            err.contains("GenshinHelper"),
            "error should instruct user to close GenshinHelper, got: {err}"
        );
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            0,
            "busy-port refusal must never connect or send /shutdown to existing listener"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Value, String> {
        let mut stream = TcpStream::connect((DEFAULT_SYNC_HOST, port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let body_bytes = body.unwrap_or("").as_bytes();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_bytes.len()
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("write hdr: {e}"))?;
        if !body_bytes.is_empty() {
            stream
                .write_all(body_bytes)
                .await
                .map_err(|e| format!("write body: {e}"))?;
        }
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        let (_, json_body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("malformed HTTP response: {text}"))?;
        serde_json::from_str(json_body).map_err(|e| format!("parse json '{json_body}': {e}"))
    }

    #[tokio::test]
    async fn web_bench_matches_seq_handles_non_synced_and_timeout_without_browser() {
        let tmp = unique_tmp_dir("seq_and_status");
        let scenario_path = tmp.join("scenario.json");
        let out_dir = tmp.join("out");

        // Event 0: warm-up (map 2)
        // Event 1: map 2 -> 2 (same_map) -> client sends wrong seq & wrong event_type first, then matching `synced`
        // Event 2: map 2 -> 40 (map_switch) -> client sends non-synced status `"error"`
        // Event 3: map 40 -> 40 (same_map) -> client drops it -> timeout
        let scenario = WebScenarioFile {
            schema_version: WEB_SCENARIO_SCHEMA_VERSION,
            sessions: vec!["sess_mock".to_owned()],
            events: vec![
                WebScenarioEvent {
                    session: "sess_mock".to_owned(),
                    frame_idx: 1,
                    ts: 1.0,
                    map_id: 2,
                    payload: sample_payload(2, 10.0, 20.0),
                },
                WebScenarioEvent {
                    session: "sess_mock".to_owned(),
                    frame_idx: 2,
                    ts: 2.0,
                    map_id: 2,
                    payload: sample_payload(2, 11.0, 21.0),
                },
                WebScenarioEvent {
                    session: "sess_mock".to_owned(),
                    frame_idx: 3,
                    ts: 3.0,
                    map_id: 40,
                    payload: sample_payload(40, 30.0, 40.0),
                },
                WebScenarioEvent {
                    session: "sess_mock".to_owned(),
                    frame_idx: 4,
                    ts: 4.0,
                    map_id: 40,
                    payload: sample_payload(40, 31.0, 41.0),
                },
            ],
        };
        write_json_atomic(
            &scenario_path,
            &serde_json::to_value(&scenario).expect("serialize"),
        )
        .expect("write scenario");

        let recorder = Arc::new(FeedbackRecorder::new());
        let callbacks: Arc<dyn ServerCallbacks> = Arc::new(WebBenchCallbacks {
            recorder: Arc::clone(&recorder),
        });
        let server = SyncServer::new(SyncServerConfig {
            host: DEFAULT_SYNC_HOST.to_owned(),
            port: 0,
            callbacks: Some(callbacks),
            ..Default::default()
        });
        let port = server.start().await.expect("start ephemeral sync server");

        let client_task = tokio::spawn(async move {
            let mut since: u64 = 0;
            let mut seen_events = 0usize;
            while seen_events < 4 {
                let ev = match http_request(port, "GET", &format!("/poll?since={since}"), None).await
                {
                    Ok(v) => v,
                    Err(_) => break,
                };
                if ev.get("type").and_then(Value::as_str) == Some("SHUTDOWN") {
                    break;
                }
                if ev.get("type").and_then(Value::as_str) == Some("HEARTBEAT") {
                    continue;
                }
                let seq = ev.get("_seq").and_then(Value::as_u64).unwrap_or(0);
                if seq > since {
                    since = seq;
                }
                seen_events += 1;

                match seen_events {
                    1 => {
                        // Warm-up: respond synced
                        let body = json!({
                            "event_type": "SYNC_POSITION_ONLY",
                            "status": "synced",
                            "seq": seq,
                        })
                        .to_string();
                        let _ = http_request(port, "POST", "/action_feedback", Some(&body)).await;
                    }
                    2 => {
                        // Measured #1: send noise (wrong seq, wrong event_type) before matching synced
                        let wrong_seq = json!({
                            "event_type": "SYNC_POSITION_ONLY",
                            "status": "error",
                            "seq": seq + 99,
                        })
                        .to_string();
                        let _ =
                            http_request(port, "POST", "/action_feedback", Some(&wrong_seq)).await;

                        let wrong_type = json!({
                            "event_type": "SYNC_AND_MARK",
                            "status": "error",
                            "seq": seq,
                        })
                        .to_string();
                        let _ =
                            http_request(port, "POST", "/action_feedback", Some(&wrong_type)).await;

                        tokio::time::sleep(Duration::from_millis(25)).await;
                        let good = json!({
                            "event_type": "SYNC_POSITION_ONLY",
                            "status": "synced",
                            "seq": seq,
                        })
                        .to_string();
                        let _ = http_request(port, "POST", "/action_feedback", Some(&good)).await;
                    }
                    3 => {
                        // Measured #2: non-synced status
                        let bad = json!({
                            "event_type": "SYNC_POSITION_ONLY",
                            "status": "error",
                            "seq": seq,
                        })
                        .to_string();
                        let _ = http_request(port, "POST", "/action_feedback", Some(&bad)).await;
                    }
                    4 => {
                        // Measured #3: send nothing -> expect timeout
                    }
                    _ => {}
                }
            }
        });

        let opts = WebBenchOptions {
            scenario_path,
            port,
            connect_timeout_s: 5.0,
            event_timeout_s: 0.35,
            gap_s: 0.0,
            limit: None,
            out_dir: Some(out_dir.clone()),
        };

        let report =
            run_web_bench_on_server(&opts, &server, &recorder, port, &scenario.events)
                .await
                .expect("web-bench run completes with structured failures");

        server.stop().await;
        let _ = client_task.await;

        assert!(!report.passed, "non-synced + timeout must mark passed=false");
        assert_eq!(report.warmup.map_id, 2);
        assert_eq!(report.warmup.status, "synced");
        assert_eq!(report.events.len(), 3);

        // Event 1: same_map, ignored wrong seq & wrong event_type, matched synced
        assert_eq!(report.events[0].from_map, 2);
        assert_eq!(report.events[0].to_map, 2);
        assert_eq!(report.events[0].class, WebBenchClass::SameMap);
        assert_eq!(report.events[0].status, "synced");
        assert!(report.events[0].ok);
        assert!(report.events[0].latency_s.is_some());

        // Event 2: map_switch, non-synced status "error"
        assert_eq!(report.events[1].from_map, 2);
        assert_eq!(report.events[1].to_map, 40);
        assert_eq!(report.events[1].class, WebBenchClass::MapSwitch);
        assert_eq!(report.events[1].status, "error");
        assert!(!report.events[1].ok);
        assert!(report.events[1].latency_s.is_some());

        // Event 3: same_map, timeout
        assert_eq!(report.events[2].from_map, 40);
        assert_eq!(report.events[2].to_map, 40);
        assert_eq!(report.events[2].class, WebBenchClass::SameMap);
        assert_eq!(report.events[2].status, "timeout");
        assert!(!report.events[2].ok);
        assert!(report.events[2].latency_s.is_none());

        assert_eq!(report.summary.same_map.count, 2);
        assert_eq!(report.summary.same_map.synced, 1);
        assert_eq!(report.summary.same_map.failures, 1);
        assert_eq!(report.summary.map_switch.count, 1);
        assert_eq!(report.summary.map_switch.synced, 0);
        assert_eq!(report.summary.map_switch.failures, 1);

        let rendered = format_web_bench_table_and_summary(&report);
        assert!(rendered.contains("same_map"));
        assert!(rendered.contains("map_switch"));

        let saved_path = out_dir.join(WEB_BENCH_REPORT_FILENAME);
        assert!(saved_path.is_file(), "web_bench_report.json must be written");
        let saved: WebBenchReport =
            serde_json::from_str(&std::fs::read_to_string(&saved_path).expect("read report"))
                .expect("parse report");
        assert_eq!(saved.events.len(), 3);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
