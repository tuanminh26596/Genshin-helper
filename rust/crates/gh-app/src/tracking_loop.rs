//! 60 Hz heading / 20 Hz full minimap tracking loop, Map (M) FSM, HUD sync, LAN push, and HDR warning.

use crate::mmap_view::ViewFrame;
use crate::session_recorder::SessionFrameMeta;
use crate::state::{AppState, WINDOW_TITLES, json_f64, json_i64, json_truthy};
use gh_core::config::AppConfig;
use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::i18n::{get_language, t};
use gh_core::multimap::MultimapDb;
use gh_core::nav_controller::NavState;
use gh_core::ui_layout::compute_ui_layout;
use gh_cv::locator::{DEFAULT_AREA_NAMES, LocateResult, MapLocator};
use gh_cv::map_state::{
    CLOSE_PROBE_DEFAULT_THRESHOLD, MAX_RETRIES, MapScreenStateMachine, MapState, MapStepResult,
};
use gh_cv::minimap_tracker::{MinimapPose, TrackerState};
use gh_hud::layered::query_monitor_work_area;
use gh_hud::mmap_overlay::MMapOverlayMode;
use gh_hud::toast::ToastKind;
use gh_net::pin_guide::HoYoLABLabelCatalog;
use gh_win32::capture::WindowFinder;
use gh_win32::game_settings::check_uipi_status;
use gh_win32::runtime::HighResTimer;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const MINIMAP_TICK_S: f64 = 1.0 / 60.0;
/// Tick period while the M-Map is open with route or single-pin guidance (60 fps target).
pub const MMAP_GUIDE_TICK_S: f64 = 1.0 / 60.0;
/// With nothing to point at for this long, the M-Map overlay shows the text banner instead
/// (the first registration usually lands well within it after the map opens).
pub const MMAP_FALLBACK_DELAY: Duration = Duration::from_millis(700);
/// A longer gap between M-Map ticks restarts that grace period (map reopened, game refocused).
const MMAP_FALLBACK_GAP: Duration = Duration::from_millis(300);
pub const MINIMAP_FULL_STEP_S: f64 = 0.05;
pub const WINDOW_LOOKUP_CACHE_S: f64 = 0.25;
pub const HDR_CLIP_STREAK_WARN: usize = 8;

#[inline]
fn angle_diff_deg(a: Option<f64>, b: Option<f64>) -> f64 {
    match (a, b) {
        (None, None) => 0.0,
        (None, _) | (_, None) => 180.0,
        (Some(va), Some(vb)) => (((va - vb + 180.0).rem_euclid(360.0)) - 180.0).abs(),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MmBcastState {
    map_id: i32,
    area_id: Option<i32>,
    lat: f64,
    lng: f64,
    heading: Option<f64>,
    camera_heading: Option<f64>,
    is_underground: bool,
    floor_id: Option<i32>,
}

fn resolve_minimap_area(
    maps_db: &MultimapDb,
    locator: Option<&MapLocator>,
    map_id: i32,
    world_x: f64,
    world_y: f64,
) -> (Option<i32>, Option<String>) {
    if map_id != 2 {
        return (None, None);
    }
    let Some((aid, db_name)) = maps_db.nearest_area_for_pos(map_id, world_x, world_y) else {
        return (None, None);
    };
    let name = db_name
        .or_else(|| locator.and_then(|l| l.area_names_map().get(&aid).cloned()))
        .or_else(|| {
            DEFAULT_AREA_NAMES
                .iter()
                .find(|&&(id, _)| id == aid)
                .map(|&(_, n)| n.to_owned())
        });
    (Some(aid), name)
}

#[derive(Debug, Clone, PartialEq)]
struct HudSig {
    state: NavState,
    target_id: Option<String>,
    dist_tenth: Option<i64>,
    delta_tenth: Option<i64>,
    floor_mismatch: bool,
    route_target_name: String,
    route_step: Option<usize>,
    route_total: Option<usize>,
    route_target_map_id: Option<i32>,
    route_section_label: String,
    route_stop_kind: String,
    remaining_count: usize,
    show_spot_card: bool,
    rect: (i32, i32, i32, i32),
    position: String,
    lang: String,
    sync_label: String,
    guide_label: String,
    desc: String,
    image_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickMode {
    MinimapFast,
    Minimap,
    MapM,
    MapMPaused,
    IdleClosed,
}

pub const fn tick_mode_str(mode: TickMode) -> &'static str {
    match mode {
        TickMode::MinimapFast => "minimap_fast",
        TickMode::Minimap => "minimap",
        TickMode::MapM => "map_m",
        TickMode::MapMPaused => "map_m_paused",
        TickMode::IdleClosed => "idle_closed",
    }
}

/// Bucket name used for `unrecorded_ticks` in `meta.jsonl`: matches the existing `mode` strings,
/// with a dedicated `"map_m_settling"` key for M-Map ticks involving `MapState::Settling` before or
/// after the step.
pub const fn unrecorded_tick_bucket(
    mode: TickMode,
    map_pre_state: Option<MapState>,
    map_post_state: Option<MapState>,
) -> &'static str {
    if matches!(mode, TickMode::MapM)
        && (matches!(map_pre_state, Some(MapState::Settling))
            || matches!(map_post_state, Some(MapState::Settling)))
    {
        "map_m_settling"
    } else {
        tick_mode_str(mode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFrameDecision {
    Drop,
    KeepByGate,
    KeepForcedSettling,
}

impl SessionFrameDecision {
    pub const fn should_keep(self) -> bool {
        !matches!(self, Self::Drop)
    }

    pub const fn is_forced(self) -> bool {
        matches!(self, Self::KeepForcedSettling)
    }
}

/// Pure keep/drop decision for `SessionRecorder` frames on each tracking-loop tick:
/// - Recording inactive -> `Drop`.
/// - Inter-frame fps gate elapsed (`gate_ok`) -> `KeepByGate`.
/// - Gate not elapsed + `TickMode::MapM` with `MapState::Settling` before or after the step ->
///   `KeepForcedSettling` (keeps every open, settling, and sync transition tick by reusing the
///   already-captured M-Map frame).
/// - All other ticks -> `Drop`.
pub const fn decide_session_frame_keep(
    recording_active: bool,
    gate_ok: bool,
    mode: TickMode,
    map_pre_state: Option<MapState>,
    map_post_state: Option<MapState>,
) -> SessionFrameDecision {
    if !recording_active {
        return SessionFrameDecision::Drop;
    }
    if gate_ok {
        return SessionFrameDecision::KeepByGate;
    }
    if matches!(mode, TickMode::MapM)
        && (matches!(map_pre_state, Some(MapState::Settling))
            || matches!(map_post_state, Some(MapState::Settling)))
    {
        return SessionFrameDecision::KeepForcedSettling;
    }
    SessionFrameDecision::Drop
}

pub struct TickOutput {
    pub mode: TickMode,
    pub full_frame: Option<Arc<FrameBuf>>,
    /// M-Map FSM state immediately before `step_frame` ran (populated only when `step_map_frame` executes).
    pub map_pre_state: Option<MapState>,
    pub step_res: Option<MapStepResult>,
    pub pose: Option<MinimapPose>,
    /// M-Map view tracking result (route guidance only).
    pub view: Option<ViewFrame>,
    /// True when the M-Map frame was captured via overlay-blind per-window WGC (`CreateForWindow`),
    /// making it CV-safe to set `mmap_overlay` to `WDA_NONE` for OBS/ShadowPlay recording.
    pub overlay_capture_safe: bool,
    /// Monotonic timestamp (seconds) actually resolved inside `run_tick_worker` after capture.
    pub tick_ts: f64,
    /// Atomic full-client frame captured for `SessionRecorder` from the exact same capture as CV.
    pub session_frame: Option<Arc<FrameBuf>>,
    /// Capture path (`"gdi"`, `"wgc_hdr"`, `"wgc_window"`) of `session_frame`.
    pub session_capture_path: Option<&'static str>,
}

struct PendingVerify {
    ticks: usize,
    res: LocateResult,
}

/// Which `MapLocator::locate_player_frame` call `apply_tick_output` expects for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLocateKind {
    AutoSync,
    PendingVerify,
}

/// Persistent cross-tick state shared between live `run_tracking_loop` and deterministic `gh-sim`.
#[derive(Default)]
pub struct LoopRuntimeState {
    pending_verify: Option<PendingVerify>,
    nav_hud_visible: bool,
    last_hud_sig: Option<HudSig>,
    last_mm_bcast_ts: Option<f64>,
    last_mm_bcast_state: Option<MmBcastState>,
    last_pose_tracking: bool,
    gate_reject_streak: usize,
    /// `(since_s, last_s)` of the current stretch without a drawable M-Map guide scene.
    mmap_no_scene: Option<(f64, f64)>,
    last_nav_target_id: Option<String>,
    hdr_clip_streak: usize,
    hdr_warned: bool,
    last_wgc_win_status: String,
}

impl LoopRuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_pose_tracking(&self) -> bool {
        self.last_pose_tracking
    }

    pub fn has_pending_verify(&self) -> bool {
        self.pending_verify.is_some()
    }

    pub fn gate_reject_streak(&self) -> usize {
        self.gate_reject_streak
    }

    pub fn hdr_clip_streak(&self) -> usize {
        self.hdr_clip_streak
    }

    pub fn nav_hud_visible(&self) -> bool {
        self.nav_hud_visible
    }

    pub fn clear_pending_verify(&mut self) {
        self.pending_verify = None;
    }

    pub fn reset_bcast_and_verify(&mut self) {
        self.pending_verify = None;
        self.last_mm_bcast_state = None;
    }

    pub fn hide_nav_hud(&mut self, state: &AppState, emit_ui: bool) {
        if emit_ui && (self.nav_hud_visible || state.nav_hud.is_visible()) {
            state.nav_hud.hide();
        }
        self.nav_hud_visible = false;
        self.last_hud_sig = None;
    }
}

/// Environment context for [`apply_tick_output`].
#[derive(Debug, Clone, Copy)]
pub struct PostTickContext {
    pub game_rect: RectI32,
    pub route_needs_mmap: bool,
    pub overlay_mode: MMapOverlayMode,
    pub session_rec_active: bool,
    /// `true` in live `run_tracking_loop`; `false` in headless deterministic `gh-sim` replay
    /// so state transitions, `pending_verify`, `gate_reject_streak`, `hdr_clip_streak`,
    /// `apply_map_m_anchor`, `nav_controller`, and `nav_state_snapshot` all execute without
    /// spawning Win32 overlay windows or online guide fetches.
    pub enable_ui_side_effects: bool,
}

/// Structured post-tick outcomes returned by [`apply_tick_output`] for replay telemetry.
#[derive(Debug, Clone, Default)]
pub struct TickPostSummary {
    pub map_m_synced: bool,
    pub map_m_anchor: Option<(i32, f64, f64, bool)>,
    pub map_m_sync_payload: Option<Value>,
    pub pending_verify_scheduled: bool,
    pub pending_verify_corrected: bool,
    pub broadcast_minimap_pos: bool,
    pub nav_state: Option<String>,
    pub gate_reject_streak: usize,
    pub hdr_clip_streak: usize,
}

/// Builds the exact `SYNC_POSITION_ONLY` broadcast payload used by M-Map auto-sync and
/// pending-verify correction.
pub fn build_sync_position_only_payload(res: &LocateResult, lang: &str) -> Value {
    let mid = res.map_id.unwrap_or(2);
    let loc_map = HoYoLABLabelCatalog::get_map_name(
        mid,
        Some(lang),
        res.map_name.as_deref().unwrap_or(""),
    );
    let mut bcast = match res.to_json_value() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    bcast.insert(
        "type".to_owned(),
        Value::String("SYNC_POSITION_ONLY".to_owned()),
    );
    bcast.insert("map_name".to_owned(), Value::String(loc_map));
    Value::Object(bcast)
}

#[derive(Debug, Clone)]
pub struct TickParams {
    pub game_hwnd: isize,
    pub cl: i32,
    pub ct: i32,
    pub cw: i32,
    pub ch: i32,
    pub is_closed: bool,
    pub do_auto_sync: bool,
    pub do_minimap: bool,
    pub fast_ok: bool,
    /// Track the M-Map view for route guidance; `guide_map` = the route's map (slider-scale
    /// fallback until a view is registered).
    pub guide_view: bool,
    pub guide_map: i32,
    /// User wants HUDs visible in screen recordings AND M-Map route guidance is active.
    pub want_window_capture: bool,
    /// Explicit monotonic timestamp (seconds) for deterministic replay; `None` in live mode
    /// reads `state.start_instant.elapsed()` right after capture (preserving exact live timing).
    pub now_s: Option<f64>,
    /// Mouse hotspot in client pixels for M-Map cursor masking (`None` when outside or absent).
    pub cursor_client: Option<(f64, f64)>,
    /// When `true`, captures a single full-client frame and slices the HUD crop from that exact
    /// same buffer so `SessionRecorder` and CV share zero-skew pixels without an extra capture.
    pub capture_session_frame: bool,
    /// `true` whenever a `SessionRecorder` is active on this tick (even when `capture_session_frame`
    /// is `false` due to the 45 ms fps gate), allowing `step_map_frame` to reuse its already-captured
    /// full frame on Settling ticks without adding any capture to minimap or closed-probe ticks.
    pub session_rec_active: bool,
}

fn crop_top_left_bgra(full: &FrameBuf, w: usize, h: usize) -> Option<FrameBuf> {
    if full.format != PixelFormat::Bgra8 || w == 0 || h == 0 {
        return None;
    }
    let cw = w.min(full.width as usize);
    let ch = h.min(full.height as usize);
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut out = FrameBuf::new_zeroed(cw as u32, ch as u32, PixelFormat::Bgra8);
    let row_bytes = cw * 4;
    for y in 0..ch {
        let s_off = y * full.stride;
        let d_off = y * out.stride;
        out.data[d_off..d_off + row_bytes].copy_from_slice(&full.data[s_off..s_off + row_bytes]);
    }
    Some(out)
}

#[inline]
fn resolve_tick_ts(state: &AppState, now_override: Option<f64>) -> f64 {
    now_override.unwrap_or_else(|| state.start_instant.elapsed().as_secs_f64())
}

/// Captured M-Map frame → state machine step (+ view tracking when guiding).
fn step_map_frame(
    state: &AppState,
    frame: FrameBuf,
    p: &TickParams,
    overlay_capture_safe: bool,
) -> TickOutput {
    let tick_ts = resolve_tick_ts(state, p.now_s);
    let (pre_state, step) = {
        let mut sm = state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pre = sm.state();
        let res = sm.step_frame(Some(frame.as_view()));
        (pre, res)
    };
    let frame = Arc::new(frame);
    let view = match state.mmap_view.as_ref() {
        Some(engine) if p.guide_view && step.state != MapState::Closed => {
            Some(engine.step_at(
                Arc::clone(&frame),
                step.check.slider_res.as_ref(),
                p.cursor_client,
                p.guide_map,
                p.now_s,
            ))
        }
        _ => None,
    };
    let keep = decide_session_frame_keep(
        p.session_rec_active,
        p.capture_session_frame,
        TickMode::MapM,
        Some(pre_state),
        Some(step.state),
    )
    .should_keep();
    let (session_frame, session_capture_path) = if keep {
        let cap_path = if overlay_capture_safe {
            "wgc_window"
        } else if state.capturer.is_region_hdr(p.cl, p.ct, p.cw, p.ch) {
            "wgc_hdr"
        } else {
            "gdi"
        };
        (Some(Arc::clone(&frame)), Some(cap_path))
    } else {
        (None, None)
    };
    TickOutput {
        mode: TickMode::MapM,
        full_frame: Some(frame),
        map_pre_state: Some(pre_state),
        step_res: Some(step),
        pose: None,
        view,
        overlay_capture_safe,
        tick_ts,
        session_frame,
        session_capture_path,
    }
}

pub fn run_tick_worker(state: &AppState, p: TickParams) -> TickOutput {
    let TickParams {
        cl,
        ct,
        cw,
        ch,
        is_closed,
        do_auto_sync,
        do_minimap,
        fast_ok,
        capture_session_frame,
        ..
    } = p;
    if fast_ok && !capture_session_frame {
        let fast_rect_opt = {
            let mut tr_guard = state.tracker.lock().unwrap_or_else(|e| e.into_inner());
            tr_guard
                .as_mut()
                .and_then(|(tr, _)| tr.minimap_capture_rect(cw as u32, ch as u32, false).ok())
        };
        if let Some(rx_rect) = fast_rect_opt
            && rx_rect.width > 0
            && rx_rect.height > 0
        {
            let mm_fast = state.capturer.capture_region_bgra(
                cl + rx_rect.x,
                ct + rx_rect.y,
                rx_rect.width,
                rx_rect.height,
            );
            let now = resolve_tick_ts(state, p.now_s);
            let fast_pose = {
                let mut tr_guard = state.tracker.lock().unwrap_or_else(|e| e.into_inner());
                tr_guard.as_mut().and_then(|(tr, last_ts)| {
                    let ts = if now < *last_ts { *last_ts } else { now };
                    *last_ts = ts;
                    tr.step_headings_only(mm_fast.as_ref().map(|f| f.as_view()), Some(ts))
                })
            };
            if let Some(pose) = fast_pose {
                return TickOutput {
                    mode: TickMode::MinimapFast,
                    full_frame: None,
                    map_pre_state: None,
                    step_res: None,
                    pose: Some(pose),
                    view: None,
                    overlay_capture_safe: false,
                    tick_ts: now,
                    session_frame: None,
                    session_capture_path: None,
                };
            }
        }
    }

    if is_closed {
        let probe_hit = {
            let mut sm = state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if sm.has_anchor_templates() {
                let pr = MapScreenStateMachine::get_close_probe_rect(cl, ct, cw, ch);
                if pr.width > 0 && pr.height > 0 {
                    let roi = state
                        .capturer
                        .capture_region(pr.x, pr.y, pr.width, pr.height);
                    sm.is_close_x_probe_frame(
                        roi.as_ref().map(|f| f.as_view()),
                        ch as usize,
                        CLOSE_PROBE_DEFAULT_THRESHOLD,
                    )
                    .0
                } else {
                    false
                }
            } else {
                let pr = MapScreenStateMachine::get_slider_probe_rect(cl, ct, cw, ch);
                if pr.width > 0 && pr.height > 0 {
                    let roi = state
                        .capturer
                        .capture_region(pr.x, pr.y, pr.width, pr.height);
                    MapScreenStateMachine::is_slider_probe_frame(
                        roi.as_ref().map(|f| f.as_view()),
                        cw as usize,
                        ch as usize,
                    )
                    .0
                } else {
                    false
                }
            }
        };

        if probe_hit {
            let pause_ts = resolve_tick_ts(state, p.now_s);
            state.pause_minimap_tracker_at(pause_ts, "map_m_probe");
            if do_auto_sync {
                let (frame_opt, overlay_capture_safe) = if p.want_window_capture && p.game_hwnd != 0
                {
                    match state
                        .capturer
                        .capture_window_client_outcome(p.game_hwnd, cl, ct, cw, ch)
                    {
                        gh_win32::wgc_hdr::WgcWindowCaptureOutcome::Frame(wf) => (Some(wf), true),
                        gh_win32::wgc_hdr::WgcWindowCaptureOutcome::WarmingUp
                        | gh_win32::wgc_hdr::WgcWindowCaptureOutcome::Failed => {
                            (state.capturer.capture_region(cl, ct, cw, ch), false)
                        }
                    }
                } else {
                    (state.capturer.capture_region(cl, ct, cw, ch), false)
                };
                let Some(frame) = frame_opt else {
                    return TickOutput {
                        mode: TickMode::MapM,
                        full_frame: None,
                        map_pre_state: None,
                        step_res: None,
                        pose: None,
                        view: None,
                        overlay_capture_safe: false,
                        tick_ts: pause_ts,
                        session_frame: None,
                        session_capture_path: None,
                    };
                };
                return step_map_frame(state, frame, &p, overlay_capture_safe);
            }
            let (session_frame, session_capture_path) = if capture_session_frame {
                let sf = state.capturer.capture_region(cl, ct, cw, ch).map(Arc::new);
                let cp = sf.as_ref().map(|_| {
                    if state.capturer.is_region_hdr(cl, ct, cw, ch) {
                        "wgc_hdr"
                    } else {
                        "gdi"
                    }
                });
                (sf, cp)
            } else {
                (None, None)
            };
            return TickOutput {
                mode: TickMode::MapMPaused,
                full_frame: None,
                map_pre_state: None,
                step_res: None,
                pose: None,
                view: None,
                overlay_capture_safe: false,
                tick_ts: pause_ts,
                session_frame,
                session_capture_path,
            };
        }

        if do_minimap && let Ok(layout) = compute_ui_layout(cw as u32, ch as u32, false) {
            let mm = layout.minimap_rect;
            let pm = layout.paimon_maybe_rect;
            let cal_pad_x = (48.0 * layout.scale_ui).round() as i32;
            let cal_pad_y = (28.0 * layout.scale_ui).round() as i32;
            let hud_w = cw.min((mm.x + mm.width + cal_pad_x).max(pm.x + pm.width));
            let hud_h = ch.min((mm.y + mm.height + cal_pad_y).max(pm.y + pm.height));

            // Single-capture guarantee when SessionRecorder is active: capture the full client
            // frame once and slice `hud_bgra` from that exact buffer (zero timestamp/pixel skew).
            let (hud_bgra, session_frame, session_capture_path) = if capture_session_frame {
                let full_bgra = if cw > 0 && ch > 0 {
                    state.capturer.capture_region_bgra(cl, ct, cw, ch)
                } else {
                    None
                };
                let cap_path = full_bgra.as_ref().map(|_| {
                    if state.capturer.is_region_hdr(cl, ct, cw, ch) {
                        "wgc_hdr"
                    } else {
                        "gdi"
                    }
                });
                let hud_crop = if hud_w > 0 && hud_h > 0 {
                    full_bgra
                        .as_ref()
                        .and_then(|f| crop_top_left_bgra(f, hud_w as usize, hud_h as usize))
                } else {
                    None
                };
                (hud_crop, full_bgra.map(Arc::new), cap_path)
            } else {
                let hud = if hud_w > 0 && hud_h > 0 {
                    state.capturer.capture_region_bgra(cl, ct, hud_w, hud_h)
                } else {
                    None
                };
                (hud, None, None)
            };

            let rec_opt = state
                .minimap_recorder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            let now = resolve_tick_ts(state, p.now_s);
            let mut prev_ts_for_ring = 0.0_f64;
            let pose_opt = {
                let mut tr_guard = state.tracker.lock().unwrap_or_else(|e| e.into_inner());
                tr_guard.as_mut().and_then(|(tr, last_ts)| {
                    prev_ts_for_ring = *last_ts;
                    let ts = if now < *last_ts { *last_ts } else { now };
                    *last_ts = ts;
                    let pose = tr
                        .step_from_hud(
                            hud_bgra.as_ref().map(|f| f.as_view()),
                            cw as u32,
                            ch as u32,
                            Some(ts),
                            false,
                            None,
                        )
                        .ok()?;
                    if let Some(ref rec) = rec_opt
                        && rec.active()
                    {
                        let mut meta = Map::new();
                        meta.insert("state".to_owned(), json!(pose.state.as_str()));
                        meta.insert("match".to_owned(), json!(pose.match_method));
                        meta.insert("map_id".to_owned(), json!(pose.map_id));
                        meta.insert("world_x".to_owned(), json!(pose.world_x));
                        meta.insert("world_y".to_owned(), json!(pose.world_y));
                        meta.insert("span_world".to_owned(), json!(pose.span_world));
                        meta.insert("map_rotation_deg".to_owned(), json!(pose.map_rotation_deg));
                        meta.insert("underground".to_owned(), json!(pose.is_underground));
                        meta.insert("camera".to_owned(), json!(pose.camera_heading));
                        meta.insert("body".to_owned(), json!(pose.body_heading));
                        meta.insert("view".to_owned(), json!(pose.view_heading));
                        meta.insert("cal_dx_px".to_owned(), json!(pose.cal_dx_px));
                        meta.insert("cal_dy_px".to_owned(), json!(pose.cal_dy_px));
                        rec.add(hud_bgra.as_ref(), cw, ch, ts, Some(meta));
                    }
                    Some((pose, ts))
                })
            };

            if let Some((pose, ts)) = pose_opt {
                // Keep a 4 Hz (or immediate non-tracking) RAM ring of minimap crops for Alt+F12.
                if let Some(hud_frame) = hud_bgra
                    && (pose.state != TrackerState::Tracking
                        || (ts * 4.0).floor() != (prev_ts_for_ring * 4.0).floor())
                {
                    state.minimap_ring.push(
                        ts,
                        hud_frame,
                        json!({
                            "state": pose.state.as_str(),
                            "match_method": pose.match_method,
                            "map_id": pose.map_id,
                            "world_x": pose.world_x,
                            "world_y": pose.world_y,
                            "confidence": (pose.confidence * 1000.0).round() / 1000.0,
                            "is_underground": pose.is_underground,
                            "floor_name": pose.floor_name,
                            "stationary": pose.stationary,
                        }),
                    );
                }
                return TickOutput {
                    mode: TickMode::Minimap,
                    full_frame: None,
                    map_pre_state: None,
                    step_res: None,
                    pose: Some(pose),
                    view: None,
                    overlay_capture_safe: false,
                    tick_ts: ts,
                    session_frame,
                    session_capture_path,
                };
            }
        }

        let (session_frame, session_capture_path) = if capture_session_frame && cw > 0 && ch > 0 {
            let sf = state.capturer.capture_region(cl, ct, cw, ch).map(Arc::new);
            let cp = sf.as_ref().map(|_| {
                if state.capturer.is_region_hdr(cl, ct, cw, ch) {
                    "wgc_hdr"
                } else {
                    "gdi"
                }
            });
            (sf, cp)
        } else {
            (None, None)
        };
        let idle_ts = resolve_tick_ts(state, p.now_s);
        return TickOutput {
            mode: TickMode::IdleClosed,
            full_frame: None,
            map_pre_state: None,
            step_res: None,
            pose: None,
            view: None,
            overlay_capture_safe: false,
            tick_ts: idle_ts,
            session_frame,
            session_capture_path,
        };
    }

    let open_ts = resolve_tick_ts(state, p.now_s);
    state.pause_minimap_tracker_at(open_ts, "map_m_open");
    let (frame_opt, overlay_capture_safe) = if p.want_window_capture && p.game_hwnd != 0 {
        match state
            .capturer
            .capture_window_client_outcome(p.game_hwnd, cl, ct, cw, ch)
        {
            gh_win32::wgc_hdr::WgcWindowCaptureOutcome::Frame(wf) => (Some(wf), true),
            gh_win32::wgc_hdr::WgcWindowCaptureOutcome::WarmingUp
            | gh_win32::wgc_hdr::WgcWindowCaptureOutcome::Failed => {
                if state.mmap_overlay.capture_visible() {
                    // Zero-frame CV safety guard: switch overlay back to WDA_EXCLUDEFROMCAPTURE and
                    // skip this single transition frame rather than feeding overlay pixels into CV.
                    state.mmap_overlay.set_capture_visible(false);
                    (None, false)
                } else {
                    (state.capturer.capture_region(cl, ct, cw, ch), false)
                }
            }
        }
    } else if state.mmap_overlay.capture_visible() {
        state.mmap_overlay.set_capture_visible(false);
        (None, false)
    } else {
        (state.capturer.capture_region(cl, ct, cw, ch), false)
    };
    let Some(frame) = frame_opt else {
        return TickOutput {
            mode: TickMode::MapM,
            full_frame: None,
            map_pre_state: None,
            step_res: None,
            pose: None,
            view: None,
            overlay_capture_safe: false,
            tick_ts: open_ts,
            session_frame: None,
            session_capture_path: None,
        };
    };
    step_map_frame(state, frame, &p, overlay_capture_safe)
}

/// Decides whether `apply_tick_output` requires a `MapLocator::locate_player_frame` call for this tick.
///
/// Kept separate from [`apply_tick_output`] so live `run_tracking_loop` can execute the locate call
/// asynchronously on `TrackingWorker` without holding locks across `.await`, while `gh-sim` executes
/// it inline on the same thread.
pub fn prepare_tick_locate(
    rt: &mut LoopRuntimeState,
    cfg: &AppConfig,
    tick_out: &TickOutput,
) -> Option<PendingLocateKind> {
    if tick_out.mode != TickMode::MapM {
        return None;
    }
    let (Some(_), Some(step_res)) = (&tick_out.full_frame, &tick_out.step_res) else {
        return None;
    };
    if !cfg.auto_sync_m {
        return None;
    }
    if step_res.should_auto_sync {
        rt.pending_verify = None;
        return Some(PendingLocateKind::AutoSync);
    }
    if let Some(mut pv) = rt.pending_verify.take()
        && step_res.state == MapState::SyncedIdle
    {
        pv.ticks = pv.ticks.saturating_sub(1);
        if pv.ticks > 0 {
            rt.pending_verify = Some(pv);
        } else {
            // Re-stash so `apply_tick_output` can compare `v_res` against `pv.res`.
            rt.pending_verify = Some(pv);
            return Some(PendingLocateKind::PendingVerify);
        }
    }
    None
}

/// Shared post-tick state machine executed identically by both live `run_tracking_loop` and
/// deterministic `gh-sim` replay.
pub fn apply_tick_output(
    state: &Arc<AppState>,
    rt: &mut LoopRuntimeState,
    cfg_snap: &AppConfig,
    ctx: &PostTickContext,
    tick_out: TickOutput,
    locate_res: Option<(PendingLocateKind, Option<LocateResult>)>,
    on_hide_mmap_overlay: &mut dyn FnMut(),
) -> TickPostSummary {
    let mut summary = TickPostSummary::default();
    let game_rect = ctx.game_rect;
    let width = game_rect.width;
    let height = game_rect.height;
    let emit_ui = ctx.enable_ui_side_effects;

    match tick_out.mode {
        TickMode::Minimap | TickMode::MinimapFast => {
            rt.last_pose_tracking = tick_out
                .pose
                .as_ref()
                .is_some_and(|p| p.state == TrackerState::Tracking);
        }
        _ => {
            rt.last_pose_tracking = false;
        }
    }

    match tick_out.mode {
        TickMode::MapMPaused => {
            rt.hide_nav_hud(state, emit_ui);
            if emit_ui {
                state.hide_radar();
            }
            on_hide_mmap_overlay();
        }
        TickMode::MapM => {
            let cur_sm_state = state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .state();
            if cur_sm_state != MapState::Closed {
                rt.hide_nav_hud(state, emit_ui);
                if emit_ui {
                    state.hide_radar();
                }
            }
            if cur_sm_state == MapState::Closed || !ctx.route_needs_mmap {
                on_hide_mmap_overlay();
            }
            if let (Some(_full_frame), Some(_step_res)) = (&tick_out.full_frame, &tick_out.step_res) {
                match locate_res {
                    Some((PendingLocateKind::AutoSync, res_opt)) => match res_opt {
                        None => {
                            let mut sm = state
                                .state_machine
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if sm.retries() == 0 {
                                sm.request_retry();
                            }
                        }
                        Some(res) if res.has_world_coords => {
                            let dx_norm = (res.pixel_x - width as f64 * 0.5).abs()
                                / (width as f64 * 0.5).max(1.0);
                            let dy_norm = (res.pixel_y - height as f64 * 0.5).abs()
                                / (height as f64 * 0.5).max(1.0);
                            let inliers = res.inliers_count.unwrap_or(0);
                            if dx_norm > 0.65 || dy_norm > 0.65 {
                                tracing::info!(
                                    "[Auto-Sync M] Skipped: player arrow off-center ({dx_norm:.2}, {dy_norm:.2}) — user is panning."
                                );
                            } else if res.map_id == Some(2) && inliers < 2 {
                                let mut sm = state
                                    .state_machine
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                if sm.retries() < MAX_RETRIES {
                                    sm.request_retry();
                                }
                            } else {
                                let mid = res.map_id.unwrap_or(2);
                                let lang = get_language();
                                let payload = build_sync_position_only_payload(&res, &lang);
                                if emit_ui {
                                    let seq = state.server.broadcast(payload.clone(), false, false);
                                    state.record_session_event_with("sync_broadcast", || {
                                        json!({
                                            "seq": seq,
                                            "msg_type": "SYNC_POSITION_ONLY",
                                            "map_id": mid,
                                        })
                                    });
                                }
                                state.apply_map_m_anchor_at(&res, tick_out.tick_ts);
                                summary.map_m_synced = true;
                                summary.map_m_sync_payload = Some(payload);
                                if let (Some(wx), Some(wy)) = (res.world_x, res.world_y) {
                                    summary.map_m_anchor = Some((mid, wx, wy, res.is_underground));
                                }
                                if inliers <= 3 {
                                    rt.pending_verify = Some(PendingVerify { ticks: 4, res });
                                    summary.pending_verify_scheduled = true;
                                }
                            }
                        }
                        Some(_) => {
                            state
                                .state_machine
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .request_retry();
                        }
                    },
                    Some((PendingLocateKind::PendingVerify, v_res_opt)) => {
                        if let Some(pv) = rt.pending_verify.take()
                            && let Some(v_res) = v_res_opt
                            && v_res.has_world_coords
                            && let (Some(vwx), Some(vwy), Some(pwx), Some(pwy)) = (
                                v_res.world_x,
                                v_res.world_y,
                                pv.res.world_x,
                                pv.res.world_y,
                            )
                        {
                            let dx_norm = (v_res.pixel_x - width as f64 * 0.5).abs()
                                / (width as f64 * 0.5).max(1.0);
                            let dy_norm = (v_res.pixel_y - height as f64 * 0.5).abs()
                                / (height as f64 * 0.5).max(1.0);
                            if dx_norm <= 0.65 && dy_norm <= 0.65 {
                                let dist = (vwx - pwx).hypot(vwy - pwy);
                                let same_layer = v_res.is_underground == pv.res.is_underground
                                    && v_res.group_id == pv.res.group_id;
                                let v_inl = v_res.inliers_count.unwrap_or(0);
                                let p_inl = pv.res.inliers_count.unwrap_or(0);
                                if (dist > 60.0 && v_inl > p_inl)
                                    || (!same_layer && v_inl >= p_inl)
                                {
                                    let mid = v_res.map_id.unwrap_or(2);
                                    let lang = get_language();
                                    let payload = build_sync_position_only_payload(&v_res, &lang);
                                    if emit_ui {
                                        let seq = state.server.broadcast(
                                            payload.clone(),
                                            false,
                                            false,
                                        );
                                        state.record_session_event_with("sync_broadcast", || {
                                            json!({
                                                "seq": seq,
                                                "msg_type": "SYNC_POSITION_ONLY",
                                                "map_id": mid,
                                            })
                                        });
                                    }
                                    state.apply_map_m_anchor_at(&v_res, tick_out.tick_ts);
                                    summary.map_m_synced = true;
                                    summary.pending_verify_corrected = true;
                                    summary.map_m_anchor =
                                        Some((mid, vwx, vwy, v_res.is_underground));
                                    summary.map_m_sync_payload = Some(payload);
                                }
                            }
                        }
                    }
                    None => {}
                }

                if cur_sm_state != MapState::Closed && ctx.route_needs_mmap && emit_ui {
                    let lang = get_language();
                    let drawer_open = tick_out.view.is_some_and(|v| v.drawer_open);
                    let guide = tick_out
                        .view
                        .filter(|v| !v.drawer_open)
                        .and_then(|v| v.transform)
                        .and_then(|t| {
                            state.build_mmap_guide_scene(game_rect, t, ctx.overlay_mode, &lang)
                        });
                    let scene = if guide.is_some() {
                        rt.mmap_no_scene = None;
                        guide
                    } else {
                        let now = tick_out.tick_ts;
                        let since = match rt.mmap_no_scene {
                            Some((since, last))
                                if (now - last) < MMAP_FALLBACK_GAP.as_secs_f64() =>
                            {
                                since
                            }
                            _ => now,
                        };
                        rt.mmap_no_scene = Some((since, now));
                        if (now - since) >= MMAP_FALLBACK_DELAY.as_secs_f64() {
                            state.build_mmap_fallback_scene(
                                game_rect,
                                ctx.overlay_mode,
                                &lang,
                                drawer_open,
                            )
                        } else {
                            None
                        }
                    };
                    let want_mmap_cap_vis = !ctx.session_rec_active
                        && cfg_snap.nav_hud_capture_visible
                        && tick_out.overlay_capture_safe;
                    if cfg_snap.nav_hud_capture_visible {
                        let wgc_st = state.capturer.window_capture_status();
                        if wgc_st != rt.last_wgc_win_status {
                            if wgc_st.contains("failed") {
                                tracing::warn!(
                                    "[MMap-OBS] Per-window WGC capture unavailable ({wgc_st}); keeping M-Map overlay excluded from capture"
                                );
                            } else if wgc_st.starts_with("active(") {
                                tracing::info!(
                                    "[MMap-OBS] Per-window WGC capture ready ({wgc_st}); M-Map overlay visible to OBS/recorders"
                                );
                            }
                            rt.last_wgc_win_status = wgc_st;
                        }
                    } else if !rt.last_wgc_win_status.is_empty() {
                        rt.last_wgc_win_status.clear();
                    }
                    state.mmap_overlay.set_capture_visible(want_mmap_cap_vis);
                    match scene {
                        Some(sc) => state.mmap_overlay.update(sc),
                        None => state.mmap_overlay.hide(),
                    }
                    if let Some(line) = state.mmap_view.as_ref().and_then(|e| e.take_stats_line()) {
                        tracing::info!("{line}");
                    }
                }
            }
        }
        TickMode::Minimap | TickMode::MinimapFast => {
            on_hide_mmap_overlay();
            if let Some(ref pose) = tick_out.pose {
                if pose.hdr_clipped {
                    rt.hdr_clip_streak += 1;
                    if rt.hdr_clip_streak >= HDR_CLIP_STREAK_WARN && !rt.hdr_warned {
                        rt.hdr_warned = true;
                        tracing::warn!("[HDR] {}", t("hdr.warning.short", &[]));
                        if emit_ui {
                            state.hud_toast.show_toast(
                                &t("hdr.warning.title", &[]),
                                &t("hdr.warning.short", &[]),
                                ToastKind::Warning,
                                5500,
                                Some(game_rect),
                            );
                        }
                    }
                } else {
                    rt.hdr_clip_streak = 0;
                }

                let pose_valid = pose.state == TrackerState::Tracking
                    && pose.map_id.is_some()
                    && pose.lat.is_some()
                    && pose.lng.is_some();
                let should_bc_pos = pose.should_broadcast_web;
                let allow_heading_only = pose.stationary
                    && matches!(
                        pose.match_method.as_str(),
                        "stationary_skip" | "heading_fast"
                    );

                if pose_valid && (should_bc_pos || allow_heading_only) {
                    let prev_last = state
                        .game_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last_pos
                        .clone();

                    let heading_val = pose.body_heading.or(pose.view_heading);
                    let cam_val = pose.camera_heading.or(pose.view_heading);

                    let (eff_mid, eff_lat, eff_lng, eff_wx, eff_wy, eff_ug, eff_fid, eff_fname) =
                        if !should_bc_pos
                            && allow_heading_only
                            && let Some(ref pl) = prev_last
                            && let (Some(plat), Some(plng)) =
                                (json_f64(pl.get("lat")), json_f64(pl.get("lng")))
                        {
                            (
                                json_i64(pl.get("map_id"))
                                    .map(|i| i as i32)
                                    .unwrap_or_else(|| pose.map_id.unwrap_or(2)),
                                plat,
                                plng,
                                json_f64(pl.get("world_x")).unwrap_or(plng),
                                json_f64(pl.get("world_y")).unwrap_or(plat),
                                json_truthy(pl.get("is_underground"), pose.is_underground),
                                pl.get("floor_id")
                                    .and_then(|v| json_i64(Some(v)))
                                    .map(|i| i as i32)
                                    .or(pose.floor_id),
                                pl.get("floor_name")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| pose.floor_name.clone()),
                            )
                        } else {
                            let mid = pose.map_id.unwrap_or(2);
                            let lat = pose.lat.unwrap_or(0.0);
                            let lng = pose.lng.unwrap_or(0.0);
                            (
                                mid,
                                lat,
                                lng,
                                pose.world_x.unwrap_or(lng),
                                pose.world_y.unwrap_or(lat),
                                pose.is_underground,
                                pose.floor_id,
                                pose.floor_name.clone(),
                            )
                        };

                    let (eff_area_id, eff_area_name) = if !should_bc_pos
                        && allow_heading_only
                        && let Some(ref pl) = prev_last
                        && let Some(aid) = json_i64(pl.get("area_id")).map(|i| i as i32)
                    {
                        let aname = pl
                            .get("area_name")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .or_else(|| {
                                DEFAULT_AREA_NAMES
                                    .iter()
                                    .find(|&&(id, _)| id == aid)
                                    .map(|&(_, n)| n.to_owned())
                            });
                        (Some(aid), aname)
                    } else {
                        resolve_minimap_area(
                            &state.maps_db,
                            state.locator.as_deref(),
                            eff_mid,
                            eff_wx,
                            eff_wy,
                        )
                    };
                    let eff_group_id = if eff_ug {
                        eff_fid
                            .and_then(|fid| state.maps_db.resolve_floor_group_id(eff_mid, fid))
                            .or_else(|| {
                                if !should_bc_pos && allow_heading_only {
                                    prev_last
                                        .as_ref()
                                        .and_then(|pl| json_i64(pl.get("group_id")))
                                        .map(|i| i as i32)
                                } else {
                                    None
                                }
                            })
                    } else {
                        None
                    };

                    let pos_info = json!({
                        "map_id": eff_mid,
                        "area_id": eff_area_id,
                        "area_name": eff_area_name,
                        "lat": eff_lat,
                        "lng": eff_lng,
                        "world_x": eff_wx,
                        "world_y": eff_wy,
                        "heading": heading_val,
                        "camera_heading": cam_val,
                        "view_heading": pose.view_heading,
                        "is_underground": eff_ug,
                        "group_id": eff_group_id,
                        "floor_id": eff_fid,
                        "floor_name": eff_fname,
                    });
                    {
                        let mut gs = state.game_state.lock().unwrap_or_else(|e| e.into_inner());
                        gs.last_pos = Some(pos_info);
                    }

                    if state.has_active_route_doc() {
                        let floor_id_str = eff_fid.map(|f| f.to_string());
                        let adv = {
                            let mut nav = state
                                .nav_controller
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            nav.advance_route_on_position_on_floor(
                                eff_mid,
                                eff_lat,
                                eff_lng,
                                floor_id_str.as_deref(),
                            )
                        };
                        if adv.is_some() && emit_ui {
                            let st = state
                                .nav_controller
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .route_status_json();
                            state.push_route_state_update(&st);
                        }
                    }

                    let now_ts = tick_out.tick_ts;
                    if rt.last_mm_bcast_ts.is_none_or(|t0| (now_ts - t0) >= 0.05) {
                        let should_send = if !should_bc_pos && allow_heading_only {
                            rt.last_mm_bcast_state.as_ref().is_some_and(|prev| {
                                angle_diff_deg(heading_val, prev.heading) >= 2.0
                                    || angle_diff_deg(cam_val, prev.camera_heading) >= 2.0
                            })
                        } else if let Some(ref prev) = rt.last_mm_bcast_state {
                            prev.map_id != eff_mid
                                || prev.area_id != eff_area_id
                                || prev.is_underground != eff_ug
                                || prev.floor_id != eff_fid
                                || (eff_lat - prev.lat).hypot(eff_lng - prev.lng) >= 0.8
                                || angle_diff_deg(heading_val, prev.heading) >= 2.0
                                || angle_diff_deg(cam_val, prev.camera_heading) >= 2.0
                        } else {
                            true
                        };

                        if should_send {
                            rt.last_mm_bcast_ts = Some(now_ts);
                            rt.last_mm_bcast_state = Some(MmBcastState {
                                map_id: eff_mid,
                                area_id: eff_area_id,
                                lat: eff_lat,
                                lng: eff_lng,
                                heading: heading_val,
                                camera_heading: cam_val,
                                is_underground: eff_ug,
                                floor_id: eff_fid,
                            });
                            summary.broadcast_minimap_pos = true;
                            if emit_ui {
                                let conf_r = (pose.confidence * 1000.0).round() / 1000.0;
                                let lang = get_language();
                                let eff_map_name = HoYoLABLabelCatalog::get_map_name(
                                    eff_mid,
                                    Some(&lang),
                                    state.maps_db.map_name(eff_mid).unwrap_or(""),
                                );
                                state.server.broadcast_ephemeral(json!({
                                    "type": "MINIMAP_POSITION",
                                    "source": "minimap",
                                    "has_world_coords": true,
                                    "map_id": eff_mid,
                                    "map_name": eff_map_name,
                                    "area_id": eff_area_id,
                                    "area_name": eff_area_name,
                                    "world_x": eff_wx,
                                    "world_y": eff_wy,
                                    "lat": eff_lat,
                                    "lng": eff_lng,
                                    "heading": heading_val,
                                    "camera_heading": cam_val,
                                    "body_heading": pose.body_heading,
                                    "view_heading": pose.view_heading,
                                    "minimap_mode": pose.minimap_mode.as_str(),
                                    "zoom_regime": pose.zoom_regime.as_str(),
                                    "is_underground": eff_ug,
                                    "group_id": eff_group_id,
                                    "floor_id": eff_fid,
                                    "floor_short": eff_fname,
                                    "floor_name": eff_fname,
                                    "confidence": conf_r,
                                    "hdr_clipped": pose.hdr_clipped,
                                    "state": pose.state.as_str(),
                                }));
                            }
                        }
                    }

                    if emit_ui {
                        state.push_lan_snapshot(false);
                    }
                }

                // Navigation Mini-HUD + Native Floating Live Radar Map update
                let radar_active = state.is_radar_active();
                if !cfg_snap.nav_hud_enabled && !radar_active {
                    rt.hide_nav_hud(state, emit_ui);
                } else if matches!(
                    pose.match_method.as_str(),
                    "gate_rejected" | "gate_hold" | "paimon_pause" | "loading_pause" | "paused"
                ) {
                    rt.gate_reject_streak += 1;
                    if rt.gate_reject_streak >= 3 {
                        rt.hide_nav_hud(state, emit_ui);
                        if emit_ui {
                            state.hide_radar();
                        }
                    }
                } else {
                    rt.gate_reject_streak = 0;
                    let fallback_pos = state
                        .game_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last_pos
                        .clone();
                    let use_fallback = pose.lat.is_none()
                        && fallback_pos
                            .as_ref()
                            .and_then(|v| json_f64(v.get("lat")))
                            .is_some();

                    let eff_map_id = pose
                        .map_id
                        .or_else(|| {
                            fallback_pos
                                .as_ref()
                                .and_then(|v| json_i64(v.get("map_id")))
                                .map(|i| i as i32)
                        })
                        .unwrap_or(2);
                    let eff_lat = pose
                        .lat
                        .or_else(|| fallback_pos.as_ref().and_then(|v| json_f64(v.get("lat"))))
                        .unwrap_or(0.0);
                    let eff_lng = pose
                        .lng
                        .or_else(|| fallback_pos.as_ref().and_then(|v| json_f64(v.get("lng"))))
                        .unwrap_or(0.0);
                    let eff_heading = pose.view_heading.or(pose.camera_heading);
                    let eff_is_ug = if use_fallback {
                        fallback_pos
                            .as_ref()
                            .map(|v| json_truthy(v.get("is_underground"), pose.is_underground))
                            .unwrap_or(pose.is_underground)
                    } else {
                        pose.is_underground
                    };
                    let eff_floor_id = if use_fallback {
                        fallback_pos
                            .as_ref()
                            .and_then(|v| json_i64(v.get("floor_id")))
                            .map(|i| i as i32)
                            .or(pose.floor_id)
                    } else {
                        pose.floor_id
                    };
                    let mut eff_floor_name = if use_fallback {
                        fallback_pos
                            .as_ref()
                            .and_then(|v| v.get("floor_name").and_then(Value::as_str))
                            .unwrap_or(&pose.floor_name)
                            .trim()
                            .to_owned()
                    } else {
                        pose.floor_name.trim().to_owned()
                    };
                    if eff_is_ug
                        && (eff_floor_name.is_empty()
                            || eff_floor_name.eq_ignore_ascii_case("G")
                            || eff_floor_name.chars().all(|c| c.is_ascii_digit()))
                        && let Some(fid) = eff_floor_id
                        && let Some(ref loc) = state.locator
                    {
                        let resolved_lbl = loc.resolve_floor_label(eff_map_id, fid);
                        if !resolved_lbl.is_empty() {
                            eff_floor_name = resolved_lbl;
                        }
                    }

                    if cfg_snap.nav_hud_enabled {
                        let fid_str = eff_floor_id.map(|f| f.to_string());
                        let (mut guidance, nearby_snap) = {
                            let mut nav = state
                                .nav_controller
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let g = nav.compute_guidance(
                                eff_lat,
                                eff_lng,
                                eff_heading,
                                eff_map_id,
                                fid_str.as_deref(),
                                eff_is_ug,
                                pose.state.as_str(),
                                &eff_floor_name,
                            );
                            let nb = nav.nearby_snapshot(eff_lat, eff_lng, eff_map_id, 6);
                            (g, nb)
                        };
                        summary.nav_state = Some(guidance.state.as_str().to_owned());

                        {
                            let tgt = guidance.target.as_ref();
                            let snap = json!({
                                "state": guidance.state.as_str(),
                                "target_id": tgt.map(|t| t.id.as_str()),
                                "target_name": tgt.map(|t| t.display_name()),
                                "target_category": tgt.map(|t| t.category.as_str()),
                                "target_lat": tgt.map(|t| t.lat),
                                "target_lng": tgt.map(|t| t.lng),
                                "distance_m": guidance.distance_m,
                                "bearing_deg": guidance.bearing_deg,
                                "delta_deg": guidance.delta_deg,
                                "route_mode": guidance.route_mode.as_str(),
                                "floor_mismatch": guidance.floor_mismatch,
                                "remaining_count": guidance.remaining_count,
                                "player": {
                                    "lat": eff_lat,
                                    "lng": eff_lng,
                                    "map_id": eff_map_id,
                                    "floor_id": eff_floor_id,
                                    "floor_name": eff_floor_name,
                                    "is_underground": eff_is_ug,
                                },
                                "heading": {
                                    "view": pose.view_heading,
                                    "camera": pose.camera_heading,
                                    "body": pose.body_heading,
                                    "used": eff_heading,
                                },
                                "hud_visible": rt.nav_hud_visible,
                                "nearby": nearby_snap,
                            });
                            *state
                                .nav_state_snapshot
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(snap);
                        }

                        let cur_target_id = guidance.target.as_ref().map(|t| t.id.clone());
                        if cur_target_id != rt.last_nav_target_id {
                            rt.last_nav_target_id = cur_target_id;
                            if emit_ui {
                                state.ensure_guide(guidance.target.as_ref());
                            }
                        }

                        if emit_ui {
                            let cur_lang = get_language();
                            if guidance.state == NavState::Idle
                                && guidance.target.is_none()
                                && !guidance.is_route_mode
                            {
                                let idle_hint = if !state.server.has_active_client(5.0) {
                                    if cur_lang == "vi" {
                                        "Mở / F5 HoYoLAB Map để kết nối"
                                    } else {
                                        "Open / F5 HoYoLAB Map to connect"
                                    }
                                } else if cur_lang == "vi" {
                                    "Bật bộ lọc rương/thần đồng trên HoYoLAB"
                                } else {
                                    "Enable pin filters on HoYoLAB Map"
                                };
                                guidance.delta_deg = None;
                                guidance.route_target_name = idle_hint.to_owned();
                            }

                            let sync_label = state.hotkey_mgr.get_display_name("nav_mark");
                            let guide_label = state.hotkey_mgr.get_display_name("nav_guide");
                            update_nav_hud_if_changed(
                                state,
                                &mut rt.nav_hud_visible,
                                &mut rt.last_hud_sig,
                                guidance,
                                game_rect,
                                &cfg_snap.nav_hud_position,
                                cur_lang,
                                sync_label,
                                guide_label,
                            );
                        }
                    } else {
                        rt.hide_nav_hud(state, emit_ui);
                    }

                    if emit_ui && radar_active && (pose.lat.is_some() || use_fallback) {
                        state.push_radar_frame(
                            game_rect,
                            eff_map_id,
                            eff_lng,
                            eff_lat,
                            pose.body_heading.or(eff_heading),
                            pose.camera_heading.or(eff_heading),
                            eff_is_ug,
                            eff_floor_id,
                            eff_floor_name,
                            cfg_snap.nav_hud_capture_visible && !ctx.session_rec_active,
                        );
                    }
                }
            } else {
                rt.hide_nav_hud(state, emit_ui);
                if emit_ui {
                    state.hide_radar();
                }
            }
        }
        TickMode::IdleClosed => {
            on_hide_mmap_overlay();
            if emit_ui {
                state.hide_radar();
            }
            if cfg_snap.nav_hud_enabled && state.has_active_route_doc() {
                let guidance = {
                    let mut nav = state
                        .nav_controller
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    nav.compute_guidance(0.0, 0.0, None, 2, None, false, "UNLOCALIZED", "")
                };
                summary.nav_state = Some(guidance.state.as_str().to_owned());
                if emit_ui {
                    let cur_lang = get_language();
                    let sync_label = state.hotkey_mgr.get_display_name("nav_mark");
                    let guide_label = state.hotkey_mgr.get_display_name("nav_guide");
                    update_nav_hud_if_changed(
                        state,
                        &mut rt.nav_hud_visible,
                        &mut rt.last_hud_sig,
                        guidance,
                        game_rect,
                        &cfg_snap.nav_hud_position,
                        cur_lang,
                        sync_label,
                        guide_label,
                    );
                }
            } else {
                rt.hide_nav_hud(state, emit_ui);
            }
        }
    }

    summary.gate_reject_streak = rt.gate_reject_streak;
    summary.hdr_clip_streak = rt.hdr_clip_streak;
    summary
}

async fn sleep_or_shutdown(dur: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    if dur.is_zero() {
        tokio::task::yield_now().await;
        return *shutdown_rx.borrow();
    }
    tokio::select! {
        _ = tokio::time::sleep(dur) => *shutdown_rx.borrow(),
        _ = shutdown_rx.changed() => *shutdown_rx.borrow(),
    }
}

#[allow(clippy::too_many_arguments)]
fn update_nav_hud_if_changed(
    state: &AppState,
    nav_hud_visible: &mut bool,
    last_hud_sig: &mut Option<HudSig>,
    guidance: gh_core::nav_controller::NavGuidance,
    game_rect: RectI32,
    position: &str,
    cur_lang: String,
    sync_label: String,
    guide_label: String,
) {
    let sig = HudSig {
        state: guidance.state,
        target_id: guidance.target.as_ref().map(|t| t.id.clone()),
        dist_tenth: Some((guidance.distance_m * 10.0).round() as i64),
        delta_tenth: guidance.delta_deg.map(|d| (d * 10.0).round() as i64),
        floor_mismatch: guidance.floor_mismatch,
        route_target_name: guidance.route_target_name.clone(),
        route_step: guidance.route_step,
        route_total: guidance.route_total,
        route_target_map_id: guidance.route_target_map_id,
        route_section_label: guidance.route_section_label.clone(),
        route_stop_kind: guidance.route_stop_kind.clone(),
        remaining_count: guidance.remaining_count,
        show_spot_card: guidance.show_spot_card,
        rect: (game_rect.x, game_rect.y, game_rect.width, game_rect.height),
        position: position.to_owned(),
        lang: cur_lang.clone(),
        sync_label: sync_label.clone(),
        guide_label: guide_label.clone(),
        desc: guidance
            .target
            .as_ref()
            .map(|t| t.description.clone())
            .unwrap_or_default(),
        image_url: guidance
            .target
            .as_ref()
            .map(|t| t.image_url.clone())
            .unwrap_or_default(),
    };

    if last_hud_sig.as_ref() != Some(&sig) {
        let wa = query_monitor_work_area(
            game_rect.x + game_rect.width / 2,
            game_rect.y + game_rect.height / 2,
        );
        state.nav_hud.update(
            guidance,
            Some(game_rect),
            Some(wa),
            None,
            Some(position),
            Some(&cur_lang),
            Some(&sync_label),
            0.0,
            None,
            Some(&guide_label),
        );
        *nav_hud_visible = true;
        *last_hud_sig = Some(sig);
    }
}

enum TrackCmd {
    Tick {
        state: Arc<AppState>,
        params: TickParams,
        reply: tokio::sync::oneshot::Sender<TickOutput>,
    },
    LocatePlayer {
        locator: Arc<gh_cv::locator::MapLocator>,
        frame: Arc<FrameBuf>,
        slider: Option<gh_cv::zoom_slider::ZoomSliderResult>,
        reply: tokio::sync::oneshot::Sender<Option<LocateResult>>,
    },
    ReleaseScratch,
}

struct TrackingWorker {
    tx: Option<std::sync::mpsc::SyncSender<TrackCmd>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TrackingWorker {
    fn spawn() -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<TrackCmd>(2);
        let handle = std::thread::Builder::new()
            .name("gh-track-worker".to_owned())
            .spawn(move || {
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        TrackCmd::Tick {
                            state,
                            params,
                            reply,
                        } => {
                            let out = run_tick_worker(&state, params);
                            let _ = reply.send(out);
                        }
                        TrackCmd::LocatePlayer {
                            locator,
                            frame,
                            slider,
                            reply,
                        } => {
                            let res = locator.locate_player_frame(
                                frame.as_view(),
                                false,
                                slider.as_ref(),
                            );
                            gh_cv::locator::release_thread_scratch();
                            let _ = reply.send(res);
                        }
                        TrackCmd::ReleaseScratch => {
                            gh_cv::locator::release_thread_scratch();
                        }
                    }
                }
                gh_cv::locator::release_thread_scratch();
            })
            .ok()?;
        Some(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    async fn run_tick(&self, state: Arc<AppState>, params: TickParams) -> Option<TickOutput> {
        let tx = self.tx.as_ref()?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if tx
            .send(TrackCmd::Tick {
                state,
                params,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok()
    }

    async fn locate_player(
        &self,
        locator: Arc<gh_cv::locator::MapLocator>,
        frame: Arc<FrameBuf>,
        slider: Option<gh_cv::zoom_slider::ZoomSliderResult>,
    ) -> Option<LocateResult> {
        let tx = self.tx.as_ref()?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if tx
            .send(TrackCmd::LocatePlayer {
                locator,
                frame,
                slider,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok().flatten()
    }

    fn release_scratch(&self) {
        if let Some(ref tx) = self.tx {
            let _ = tx.try_send(TrackCmd::ReleaseScratch);
        }
    }
}

impl Drop for TrackingWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub async fn run_tracking_loop(state: Arc<AppState>, mut shutdown_rx: watch::Receiver<bool>) {
    tracing::info!(
        "Tracking loop running (v5.4 Hybrid Minimap + 3-Anchor Map M FSM). Waiting for Genshin Impact window..."
    );

    let worker = TrackingWorker::spawn();
    let mut hires_timer = HighResTimer::new();
    let mut window_attached = false;
    let mut last_res: Option<(i32, i32)> = None;
    let mut uipi_checked_hwnd: Option<isize> = None;
    let mut last_full_step_mono: Option<Instant> = None;
    let mut cached_win_info: Option<(isize, i32, i32, i32, i32)> = None;
    let mut cached_win_mono: Option<Instant> = None;
    let mut rt = LoopRuntimeState::new();

    let mut rate_win_start: Option<Instant> = None;
    let mut rate_minimap_full: usize = 0;
    let mut rate_minimap_fast: usize = 0;
    let mut rate_work_ms: Vec<f64> = Vec::new();

    let reset_rate_window = |start: &mut Option<Instant>,
                             full: &mut usize,
                             fast: &mut usize,
                             work: &mut Vec<f64>| {
        *start = None;
        *full = 0;
        *fast = 0;
        work.clear();
    };

    let mut hide_mmap_overlay = || {
        state.mmap_overlay.hide();
        state.mmap_overlay.set_capture_visible(false);
        state.capturer.release_window_capture();
        if let Some(engine) = state.mmap_view.as_ref() {
            engine.reset();
        }
        if let Some(ref w) = worker {
            w.release_scratch();
        }
    };

    while state.server.is_running() && !*shutdown_rx.borrow() {
        if !state.engine_active.load(Ordering::Acquire) {
            if window_attached {
                window_attached = false;
                last_res = None;
            }
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            rt.reset_bcast_and_verify();
            uipi_checked_hwnd = None;
            state.pause_minimap_tracker("standby");
            rt.hide_nav_hud(&state, true);
            state.hide_radar();
            hide_mmap_overlay();
            hires_timer.set(false);
            reset_rate_window(
                &mut rate_win_start,
                &mut rate_minimap_full,
                &mut rate_minimap_fast,
                &mut rate_work_ms,
            );
            if sleep_or_shutdown(Duration::from_millis(350), &mut shutdown_rx).await {
                break;
            }
            continue;
        }

        let tick_start = Instant::now();
        let win_info = if let (Some(cached), Some(ts)) = (cached_win_info, cached_win_mono)
            && tick_start.duration_since(ts).as_secs_f64() < WINDOW_LOOKUP_CACHE_S
        {
            Some(cached)
        } else {
            let found = WindowFinder::get_game_window(&WINDOW_TITLES, false);
            cached_win_info = found;
            cached_win_mono = Some(tick_start);
            found
        };

        let Some((hwnd, left, top, width, height)) = win_info else {
            if window_attached {
                tracing::warn!("Genshin Impact window lost. Waiting for window...");
                window_attached = false;
                {
                    let mut gs = state.game_state.lock().unwrap_or_else(|e| e.into_inner());
                    gs.hwnd = None;
                    gs.rect = None;
                    gs.uipi_status = None;
                }
                last_res = None;
                uipi_checked_hwnd = None;
                if let Some(tray) = state
                    .tray
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    tray.set_uipi_warning(false);
                }
            }
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            rt.reset_bcast_and_verify();
            state.pause_minimap_tracker("window_lost");
            rt.hide_nav_hud(&state, true);
            state.hide_radar();
            // Game closed (e.g. Alt+F4 with M-Map open): the M-Map overlay windows are
            // independent topmost windows and would otherwise linger over the desktop.
            hide_mmap_overlay();
            reset_rate_window(
                &mut rate_win_start,
                &mut rate_minimap_full,
                &mut rate_minimap_fast,
                &mut rate_work_ms,
            );
            if sleep_or_shutdown(Duration::from_millis(800), &mut shutdown_rx).await {
                break;
            }
            continue;
        };

        let game_rect = RectI32 {
            x: left,
            y: top,
            width,
            height,
        };
        {
            let mut gs = state.game_state.lock().unwrap_or_else(|e| e.into_inner());
            gs.hwnd = Some(hwnd);
            gs.rect = Some(game_rect);
        }

        if width < 800 || height < 500 {
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            rt.clear_pending_verify();
            state.pause_minimap_tracker("window_small");
            rt.hide_nav_hud(&state, true);
            state.hide_radar();
            hide_mmap_overlay();
            reset_rate_window(
                &mut rate_win_start,
                &mut rate_minimap_full,
                &mut rate_minimap_fast,
                &mut rate_work_ms,
            );
            if sleep_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await {
                break;
            }
            continue;
        }

        if !window_attached || last_res != Some((width, height)) {
            tracing::info!("Attached to Genshin Impact window ({width}x{height})");
            window_attached = true;
            last_res = Some((width, height));
        }

        if hwnd != 0 && uipi_checked_hwnd != Some(hwnd) {
            uipi_checked_hwnd = Some(hwnd);
            if let Ok(uipi) = tokio::task::spawn_blocking(move || check_uipi_status(Some(hwnd))).await
            {
                let mismatch = uipi.mismatch;
                {
                    let mut gs = state.game_state.lock().unwrap_or_else(|e| e.into_inner());
                    gs.uipi_status = Some(uipi.clone());
                }
                if let Some(tray) = state
                    .tray
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    tray.set_uipi_warning(mismatch);
                }
                if mismatch {
                    tracing::warn!("[UIPI] {}", uipi.reason);
                    state.hud_toast.show_toast(
                        &t("uipi.warning.title", &[]),
                        &t("uipi.warning.short", &[]),
                        ToastKind::Warning,
                        5500,
                        Some(game_rect),
                    );
                }
            }
        }

        let cfg_snap = state.config.read().expect("config lock poisoned").clone();
        let has_tracker = state
            .tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let minimap_active = has_tracker && cfg_snap.minimap_tracking;
        let overlay_mode = MMapOverlayMode::from_str_lossy(&cfg_snap.mmap_overlay_mode);
        let route_needs_mmap = overlay_mode != MMapOverlayMode::Off && state.has_mmap_guide_target();
        let want_mmap_probe = cfg_snap.auto_sync_m || route_needs_mmap;
        if !want_mmap_probe && !minimap_active {
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            rt.clear_pending_verify();
            state.pause_minimap_tracker("disabled");
            rt.hide_nav_hud(&state, true);
            state.hide_radar();
            hide_mmap_overlay();
            reset_rate_window(
                &mut rate_win_start,
                &mut rate_minimap_full,
                &mut rate_minimap_fast,
                &mut rate_work_ms,
            );
            if sleep_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await {
                break;
            }
            continue;
        }

        let is_fg = WindowFinder::is_window_foreground(hwnd);
        let sm_state = state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state();
        if !is_fg && sm_state != MapState::Settling && !rt.has_pending_verify() {
            state.pause_minimap_tracker("background");
            hires_timer.set(false);
            hide_mmap_overlay();
            reset_rate_window(
                &mut rate_win_start,
                &mut rate_minimap_full,
                &mut rate_minimap_fast,
                &mut rate_work_ms,
            );
            let mut hud_hwnds: Vec<isize> = state.nav_hud.hwnd().into_iter().collect();
            hud_hwnds.extend(state.mmap_overlay.hwnds());
            if state.is_radar_active()
                && let Ok(guard) = state.radar_map.lock()
                && let Some(ref ctrl) = *guard
                && let Some(rh) = ctrl.raw_hwnd()
            {
                hud_hwnds.push(rh);
            }
            let keep_hud = (rt.nav_hud_visible() || state.is_radar_active())
                && WindowFinder::is_game_visible_unfocused(
                    hwnd,
                    (left, top, width, height),
                    &hud_hwnds,
                    true,
                );
            if !keep_hud {
                rt.hide_nav_hud(&state, true);
                state.hide_radar();
            }
            if sleep_or_shutdown(Duration::from_millis(250), &mut shutdown_rx).await {
                break;
            }
            continue;
        }

        if !want_mmap_probe && sm_state != MapState::Closed {
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            rt.clear_pending_verify();
            hide_mmap_overlay();
        }

        let in_closed_state = state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            == MapState::Closed;
        let recorder_active = state
            .minimap_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|r| r.active());
        let session_rec_opt = state
            .session_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|r| r.active());
        let session_rec_active = session_rec_opt.is_some();
        let want_session_frame = session_rec_opt
            .as_ref()
            .is_some_and(|r| r.should_capture_at(state.start_instant.elapsed().as_secs_f64()));
        if session_rec_active {
            // Keep overlays strictly excluded from capture during clean session recording.
            state.mmap_overlay.set_capture_visible(false);
        }
        let heading_fast_ok = should_run_heading_fast(
            in_closed_state,
            minimap_active,
            rt.last_pose_tracking(),
            last_full_step_mono,
            tick_start,
            recorder_active || want_session_frame,
        );
        let do_auto_sync = want_mmap_probe;
        let guide_map = if route_needs_mmap {
            state.mmap_guide_map_id()
        } else {
            2
        };

        let cursor_client = if route_needs_mmap || session_rec_active {
            WindowFinder::cursor_screen_pos()
                .map(|(x, y)| (f64::from(x - left), f64::from(y - top)))
                .filter(|&(x, y)| x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height))
        } else {
            None
        };
        let tick_params = TickParams {
            game_hwnd: hwnd,
            cl: left,
            ct: top,
            cw: width,
            ch: height,
            is_closed: in_closed_state,
            do_auto_sync,
            do_minimap: minimap_active,
            fast_ok: heading_fast_ok,
            guide_view: route_needs_mmap,
            guide_map,
            want_window_capture: !session_rec_active
                && cfg_snap.nav_hud_capture_visible
                && route_needs_mmap
                && hwnd != 0,
            now_s: None,
            cursor_client,
            capture_session_frame: want_session_frame,
            session_rec_active,
        };
        let mut tick_out = if let Some(ref w) = worker {
            match w.run_tick(Arc::clone(&state), tick_params).await {
                Some(out) => out,
                None => continue,
            }
        } else {
            let st_for_worker = Arc::clone(&state);
            match tokio::task::spawn_blocking(move || run_tick_worker(&st_for_worker, tick_params))
                .await
            {
                Ok(out) => out,
                Err(_) => continue,
            }
        };

        // Stage the exact atomic frame captured inside `run_tick_worker` (zero second capture,
        // zero timestamp skew against the tracker pose) and enqueue it immediately AFTER
        // `apply_tick_output` so the recorded `nav` snapshot reflects the live navigation state
        // after this tick.
        let rec_tick_mode = tick_out.mode;
        let rec_map_pre_state = tick_out.map_pre_state;
        let rec_map_post_state = tick_out.step_res.as_ref().map(|s| s.state);
        let rec_tick_ts = tick_out.tick_ts;
        let pending_session_frame = if session_rec_opt.is_some() {
            let decision = decide_session_frame_keep(
                true,
                want_session_frame,
                rec_tick_mode,
                rec_map_pre_state,
                rec_map_post_state,
            );
            if decision.should_keep()
                && let Some(frame_arc) = tick_out.session_frame.take()
            {
                let mut extra = Map::new();
                extra.insert("mode".to_owned(), json!(tick_mode_str(rec_tick_mode)));
                if let Some(ref pose) = tick_out.pose {
                    extra.insert("state".to_owned(), json!(pose.state.as_str()));
                    extra.insert("match".to_owned(), json!(pose.match_method));
                    extra.insert("map_id".to_owned(), json!(pose.map_id));
                    extra.insert("world_x".to_owned(), json!(pose.world_x));
                    extra.insert("world_y".to_owned(), json!(pose.world_y));
                    extra.insert("underground".to_owned(), json!(pose.is_underground));
                    extra.insert("floor_id".to_owned(), json!(pose.floor_id));
                    extra.insert("camera".to_owned(), json!(pose.camera_heading));
                    extra.insert("body".to_owned(), json!(pose.body_heading));
                    extra.insert("view".to_owned(), json!(pose.view_heading));
                } else if let Some(ref sr) = tick_out.step_res {
                    extra.insert("state".to_owned(), json!(format!("MAP_M_{:?}", sr.state)));
                }
                let cap_path = tick_out.session_capture_path.unwrap_or("gdi").to_owned();
                Some((frame_arc, extra, cap_path, decision.is_forced()))
            } else {
                None
            }
        } else {
            None
        };

        match tick_out.mode {
            TickMode::Minimap => {
                last_full_step_mono = Some(Instant::now());
                if rate_win_start.is_none() {
                    rate_win_start = Some(tick_start);
                }
                rate_minimap_full += 1;
                rate_work_ms.push(tick_start.elapsed().as_secs_f64() * 1000.0);
            }
            TickMode::MinimapFast => {
                if rate_win_start.is_none() {
                    rate_win_start = Some(tick_start);
                }
                rate_minimap_fast += 1;
                rate_work_ms.push(tick_start.elapsed().as_secs_f64() * 1000.0);
            }
            _ => {
                reset_rate_window(
                    &mut rate_win_start,
                    &mut rate_minimap_full,
                    &mut rate_minimap_fast,
                    &mut rate_work_ms,
                );
            }
        }

        if let Some(win_t0) = rate_win_start {
            let elapsed_s = win_t0.elapsed().as_secs_f64();
            if elapsed_s >= 10.0 {
                let n_ticks = rate_minimap_full + rate_minimap_fast;
                if n_ticks > 0 && !rate_work_ms.is_empty() {
                    rate_work_ms.sort_by(f64::total_cmp);
                    let med = rate_work_ms[rate_work_ms.len() / 2];
                    let p90_idx = ((rate_work_ms.len() as f64) * 0.9)
                        .floor()
                        .min((rate_work_ms.len() - 1) as f64)
                        as usize;
                    let p90 = rate_work_ms[p90_idx];
                    tracing::info!(
                        "[Loop] minimap {:.1} Hz (heading fast {:.0}%, full {:.1} Hz), tick work med {med:.1} ms / p90 {p90:.1} ms",
                        n_ticks as f64 / elapsed_s,
                        (rate_minimap_fast as f64 / n_ticks as f64) * 100.0,
                        rate_minimap_full as f64 / elapsed_s,
                    );
                }
                reset_rate_window(
                    &mut rate_win_start,
                    &mut rate_minimap_full,
                    &mut rate_minimap_fast,
                    &mut rate_work_ms,
                );
            }
        }

        let locate_res = if let Some(kind) = prepare_tick_locate(&mut rt, &cfg_snap, &tick_out)
            && let Some(ref loc) = state.locator
            && let Some(ref frame_arc) = tick_out.full_frame
        {
            let slider_hint = tick_out
                .step_res
                .as_ref()
                .and_then(|s| s.check.slider_res.clone());
            let found = if let Some(ref w) = worker {
                w.locate_player(Arc::clone(loc), Arc::clone(frame_arc), slider_hint)
                    .await
            } else {
                let loc_clone = Arc::clone(loc);
                let frame_for_loc = Arc::clone(frame_arc);
                tokio::task::spawn_blocking(move || {
                    let res = loc_clone.locate_player_frame(
                        frame_for_loc.as_view(),
                        false,
                        slider_hint.as_ref(),
                    );
                    gh_cv::locator::release_thread_scratch();
                    res
                })
                .await
                .ok()
                .flatten()
            };
            Some((kind, found))
        } else {
            None
        };

        let post_ctx = PostTickContext {
            game_rect,
            route_needs_mmap,
            overlay_mode,
            session_rec_active,
            enable_ui_side_effects: true,
        };
        let _ = apply_tick_output(
            &state,
            &mut rt,
            &cfg_snap,
            &post_ctx,
            tick_out,
            locate_res,
            &mut hide_mmap_overlay,
        );

        if let Some(ref srec) = session_rec_opt {
            let mut kept = false;
            if let Some((frame_arc, mut extra, cap_path, forced)) = pending_session_frame {
                extra.insert(
                    "nav".to_owned(),
                    serde_json::to_value(state.snapshot_nav_state()).unwrap_or(Value::Null),
                );
                kept = srec.add_frame_arc(
                    &frame_arc,
                    rec_tick_ts,
                    SessionFrameMeta {
                        client_rect: game_rect,
                        cursor_client_pos: cursor_client,
                        capture_path: cap_path,
                        extra,
                    },
                    forced,
                );
            }
            if !kept {
                let bucket =
                    unrecorded_tick_bucket(rec_tick_mode, rec_map_pre_state, rec_map_post_state);
                srec.record_unrecorded_tick(bucket);
            }
        }

        let cur_state = state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state();
        let live_minimap = cur_state == MapState::Closed && minimap_active;
        let guiding_mmap =
            route_needs_mmap && cur_state != MapState::Closed && state.mmap_view.is_some();
        hires_timer.set(live_minimap || guiding_mmap);

        let sleep_dur = if guiding_mmap {
            let rem = MMAP_GUIDE_TICK_S - tick_start.elapsed().as_secs_f64();
            if rem > 0.001 {
                Duration::from_secs_f64(rem)
            } else {
                Duration::ZERO
            }
        } else if cur_state == MapState::Settling {
            Duration::from_millis(25)
        } else if live_minimap {
            let elapsed = tick_start.elapsed().as_secs_f64();
            let rem = MINIMAP_TICK_S - elapsed;
            if rem > 0.001 {
                Duration::from_secs_f64(rem)
            } else {
                Duration::ZERO
            }
        } else if cur_state == MapState::Closed {
            Duration::from_millis(75)
        } else if route_needs_mmap {
            Duration::from_millis(70)
        } else {
            Duration::from_millis(110)
        };

        if sleep_or_shutdown(sleep_dur, &mut shutdown_rx).await {
            break;
        }
    }

    hires_timer.set(false);
    state.mmap_overlay.hide();
    state.nav_hud.hide();
    state.hide_radar();
}

fn should_run_heading_fast(
    in_closed_state: bool,
    minimap_active: bool,
    last_pose_tracking: bool,
    last_full_step_mono: Option<Instant>,
    tick_start: Instant,
    recorder_active: bool,
) -> bool {
    in_closed_state
        && minimap_active
        && last_pose_tracking
        && last_full_step_mono.is_some_and(|t0| {
            tick_start.saturating_duration_since(t0).as_secs_f64() < MINIMAP_FULL_STEP_S
        })
        && !recorder_active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_fast_schedule_uses_completion_time_not_tick_start() {
        let base = Instant::now();
        let full_start = base;
        let completed_at = base + Duration::from_millis(80);
        let next_tick = completed_at + Duration::from_millis(16);

        // Stamping tick_start starves 60 Hz heading after any slow (>42ms) full step.
        assert!(!should_run_heading_fast(
            true,
            true,
            true,
            Some(full_start),
            next_tick,
            false,
        ));

        // Stamping completion time runs fast heading ticks inside the budget,
        // then yields to the next full step after MINIMAP_FULL_STEP_S elapses.
        assert!(should_run_heading_fast(
            true,
            true,
            true,
            Some(completed_at),
            next_tick,
            false,
        ));
        assert!(!should_run_heading_fast(
            true,
            true,
            true,
            Some(completed_at),
            completed_at + Duration::from_millis(55),
            false,
        ));
    }

    #[test]
    fn resolve_minimap_area_handles_teyvat_fallback_and_submaps() {
        let sample = r#"{
            "version": "4.0.0",
            "maps": {
                "2": {
                    "id": 2,
                    "name": "Teyvat",
                    "areas": [
                        {"id": 1, "name": "Mondstadt"}
                    ],
                    "waypoints": [
                        {"id": 1, "label_id": 2, "is_primary": true, "x_pos": 100.0, "y_pos": 100.0, "area_id": 1},
                        {"id": 2, "label_id": 2, "is_primary": true, "x_pos": -4000.0, "y_pos": 2500.0, "area_id": 11}
                    ]
                },
                "9": {
                    "id": 9,
                    "name": "The Chasm",
                    "areas": [{"id": 6, "name": "Chasm"}],
                    "waypoints": [
                        {"id": 3, "label_id": 2, "is_primary": true, "x_pos": 0.0, "y_pos": 0.0, "area_id": 6}
                    ]
                }
            }
        }"#;
        let db = MultimapDb::from_json_str(sample).expect("parse db");
        assert_eq!(
            resolve_minimap_area(&db, None, 2, 120.0, 90.0),
            (Some(1), Some("Mondstadt".to_owned()))
        );
        // Missing from `areas` table -> falls back to DEFAULT_AREA_NAMES ("Natlan").
        assert_eq!(
            resolve_minimap_area(&db, None, 2, -3950.0, 2480.0),
            (Some(11), Some("Natlan".to_owned()))
        );
        // Non-Teyvat submaps never emit Map-2 area switches.
        assert_eq!(resolve_minimap_area(&db, None, 9, 0.0, 0.0), (None, None));
        assert_eq!(
            resolve_minimap_area(&db, None, 2, f64::NAN, 0.0),
            (None, None)
        );
    }

    #[test]
    fn session_capture_uses_single_atomic_bgra_capture_and_matches_hud_crop() {
        use crate::capture_backend::FrameCapturer;
        use gh_cv::minimap_tracker::{HybridMinimapTracker, MinimapTrackerConfig};
        use gh_cv::tile_cache::{LruTerrainTileCache, TileCacheConfig};
        use gh_win32::wgc_hdr::WgcWindowCaptureOutcome;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;

        struct CountingCapturer {
            bgra_calls: AtomicUsize,
            bgra_rects: Mutex<Vec<(i32, i32, i32, i32)>>,
            full_w: u32,
            full_h: u32,
        }

        impl CountingCapturer {
            fn make_pattern(&self, left: i32, top: i32, width: u32, height: u32) -> FrameBuf {
                let mut fb = FrameBuf::new_zeroed(width, height, PixelFormat::Bgra8);
                for y in 0..height as usize {
                    let row = y * fb.stride;
                    for x in 0..width as usize {
                        let gx = (x as i32 + left) as u32;
                        let gy = (y as i32 + top) as u32;
                        let o = row + x * 4;
                        fb.data[o] = (gx & 0xFF) as u8;
                        fb.data[o + 1] = (gy & 0xFF) as u8;
                        fb.data[o + 2] = ((gx ^ gy) & 0xFF) as u8;
                        fb.data[o + 3] = 0xFF;
                    }
                }
                fb
            }
        }

        impl FrameCapturer for CountingCapturer {
            fn capture_region(&self, _l: i32, _t: i32, _w: i32, _h: i32) -> Option<FrameBuf> {
                // Return None for close/slider probes so we take the minimap branch cleanly.
                None
            }

            fn capture_region_bgra(&self, l: i32, t: i32, w: i32, h: i32) -> Option<FrameBuf> {
                self.bgra_calls.fetch_add(1, Ordering::Relaxed);
                self.bgra_rects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((l, t, w, h));
                if w <= 0 || h <= 0 || (w as u32) > self.full_w || (h as u32) > self.full_h {
                    return None;
                }
                Some(self.make_pattern(l, t, w as u32, h as u32))
            }

            fn capture_window_client_outcome(
                &self,
                _hwnd: isize,
                _l: i32,
                _t: i32,
                _w: i32,
                _h: i32,
            ) -> WgcWindowCaptureOutcome {
                WgcWindowCaptureOutcome::Failed
            }

            fn window_capture_status(&self) -> String {
                "test".to_owned()
            }

            fn release_window_capture(&self) {}
            fn close(&self) {}
        }

        let tmp = std::env::temp_dir().join(format!(
            "gh_single_cap_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");

        let cap = Arc::new(CountingCapturer {
            bgra_calls: AtomicUsize::new(0),
            bgra_rects: Mutex::new(Vec::new()),
            full_w: 1920,
            full_h: 1080,
        });
        let state = crate::state::make_test_state_with_capturer(&tmp, Arc::clone(&cap) as Arc<dyn FrameCapturer>);
        let cache = Arc::new(
            LruTerrainTileCache::from_config(TileCacheConfig {
                use_default_npz: false,
                disk_cache_dir: Some(tmp.join("tiles")),
                async_fetch: false,
                ..Default::default()
            })
            .expect("empty tile cache"),
        );
        let tracker = HybridMinimapTracker::new(cache, MinimapTrackerConfig::production_defaults())
            .expect("tracker init");
        *state.tracker.lock().unwrap_or_else(|e| e.into_inner()) = Some((tracker, 0.0));

        // 1) Session recording ON + explicit now_s: must execute ONE full-frame BGRA capture,
        // slice hud_bgra from that exact buffer, and honor now_s verbatim.
        let out_rec = run_tick_worker(
            &state,
            TickParams {
                game_hwnd: 0,
                cl: 0,
                ct: 0,
                cw: 1920,
                ch: 1080,
                is_closed: true,
                do_auto_sync: true,
                do_minimap: true,
                fast_ok: false,
                guide_view: false,
                guide_map: 2,
                want_window_capture: false,
                now_s: Some(12.34),
                cursor_client: None,
                capture_session_frame: true,
                session_rec_active: true,
            },
        );
        assert_eq!(out_rec.mode, TickMode::Minimap);
        assert!((out_rec.tick_ts - 12.34).abs() < 1e-9);
        assert_eq!(out_rec.session_capture_path, Some("gdi"));
        assert_eq!(
            cap.bgra_calls.load(Ordering::Relaxed),
            1,
            "capture_session_frame must capture once, not twice"
        );
        assert_eq!(
            *cap.bgra_rects.lock().unwrap_or_else(|e| e.into_inner()),
            vec![(0, 0, 1920, 1080)]
        );
        let full = out_rec
            .session_frame
            .as_ref()
            .expect("session_frame must be populated");
        assert_eq!((full.width, full.height), (1920, 1080));

        // Verify the HUD crop pushed to minimap_ring came bit-for-bit from the top-left of `full`.
        let ring = state.minimap_ring.snapshot();
        assert_eq!(ring.len(), 1);
        let hud = &ring[0].frame;
        assert!(hud.width < 1920 && hud.height < 1080);
        for y in 0..hud.height as usize {
            let h_off = y * hud.stride;
            let f_off = y * full.stride;
            let row_len = hud.width as usize * 4;
            assert_eq!(
                &hud.data[h_off..h_off + row_len],
                &full.data[f_off..f_off + row_len],
                "row {y} mismatch between atomic full frame and sliced hud_bgra"
            );
        }

        // 2) Session recording OFF + now_s: None: captures only HUD crop and resolves live clock.
        let out_live = run_tick_worker(
            &state,
            TickParams {
                game_hwnd: 0,
                cl: 0,
                ct: 0,
                cw: 1920,
                ch: 1080,
                is_closed: true,
                do_auto_sync: true,
                do_minimap: true,
                fast_ok: false,
                guide_view: false,
                guide_map: 2,
                want_window_capture: false,
                now_s: None,
                cursor_client: None,
                capture_session_frame: false,
                session_rec_active: false,
            },
        );
        assert_eq!(out_live.mode, TickMode::Minimap);
        assert!(out_live.session_frame.is_none());
        assert!(out_live.tick_ts >= 0.0);
        let rects = cap.bgra_rects.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[1], (0, 0, hud.width as i32, hud.height as i32));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn build_sync_position_only_payload_matches_legacy_inline_json() {
        let sample = LocateResult {
            detected: true,
            has_world_coords: true,
            map_id: Some(34),
            map_name: Some("Sea of Bygone Eras".to_owned()),
            area_id: Some(9),
            area_name: Some("Fontaine".to_owned()),
            world_x: Some(123.5),
            world_y: Some(-456.25),
            lat: Some(456.25),
            lng: Some(123.5),
            pixel_x: 1280.0,
            pixel_y: 720.0,
            heading: 90.0,
            scale: Some(1.25),
            inliers_count: Some(6),
            rmse: Some(1.1),
            is_underground: true,
            in_game_is_underground: Some(true),
            floor_idx: 1,
            total_floors: 2,
            active_floor_idx: 1,
            surface_idx: 0,
            floor_name: "Lower".to_owned(),
            floor_short: "sub".to_owned(),
            hoyolab_floor_name: Some("Lower Level".to_owned()),
            group_id: Some(101),
            group_name: Some("Cavern".to_owned()),
            floor_id: Some(202),
            has_layers: true,
            zoom_ratio_t: Some(0.45),
            candidates: None,
            inliers: None,
            slider_res: None,
            region_text: None,
            target_map_id: None,
            target_area_id: None,
            slot_types: None,
            reason: None,
            debug_included: false,
        };

        for lang in ["en", "vi"] {
            let mid = sample.map_id.unwrap_or(2);
            let loc_map = HoYoLABLabelCatalog::get_map_name(
                mid,
                Some(lang),
                sample.map_name.as_deref().unwrap_or(""),
            );
            let mut expected = match sample.to_json_value() {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            expected.insert(
                "type".to_owned(),
                Value::String("SYNC_POSITION_ONLY".to_owned()),
            );
            expected.insert("map_name".to_owned(), Value::String(loc_map.clone()));

            let actual = build_sync_position_only_payload(&sample, lang);
            assert_eq!(actual, Value::Object(expected));
            assert_eq!(actual["type"], "SYNC_POSITION_ONLY");
            assert_eq!(actual["map_name"], loc_map);
            assert_eq!(actual["map_id"], 34);
            assert_eq!(actual["has_world_coords"], true);
            assert_eq!(actual["lat"], 456.25);
            assert_eq!(actual["lng"], 123.5);
        }
    }

    #[test]
    fn decide_session_frame_keep_covers_gate_settling_minimap_idle_and_inactive() {
        // 1. Gate ok -> keep by gate across all modes when recording is active
        for mode in [
            TickMode::Minimap,
            TickMode::MinimapFast,
            TickMode::MapM,
            TickMode::MapMPaused,
            TickMode::IdleClosed,
        ] {
            let d = decide_session_frame_keep(true, true, mode, None, None);
            assert_eq!(d, SessionFrameDecision::KeepByGate, "mode={mode:?}");
            assert!(d.should_keep());
            assert!(!d.is_forced());
        }

        // 2. Gate not ok + pre or post state Settling (MapM tick) -> keep forced
        for (pre, post) in [
            (MapState::Closed, MapState::Settling),     // open tick
            (MapState::Settling, MapState::Settling),   // settling step
            (MapState::Settling, MapState::SyncedIdle), // sync tick
            (MapState::Settling, MapState::Closed),     // quick close from settling
            (MapState::SyncedIdle, MapState::Settling), // retry / scene-cut re-entering settling
            (MapState::Panning, MapState::Settling),
        ] {
            let d = decide_session_frame_keep(true, false, TickMode::MapM, Some(pre), Some(post));
            assert_eq!(
                d,
                SessionFrameDecision::KeepForcedSettling,
                "transition {pre:?} -> {post:?}"
            );
            assert!(d.should_keep());
            assert!(d.is_forced());
        }

        // 3. Gate not ok + minimap / closed / paused ticks -> drop (even if Settling were passed)
        for mode in [
            TickMode::Minimap,
            TickMode::MinimapFast,
            TickMode::MapMPaused,
            TickMode::IdleClosed,
        ] {
            let d = decide_session_frame_keep(true, false, mode, None, None);
            assert_eq!(d, SessionFrameDecision::Drop, "mode={mode:?}");
            assert!(!d.should_keep());
            let d_s = decide_session_frame_keep(
                true,
                false,
                mode,
                Some(MapState::Settling),
                Some(MapState::Settling),
            );
            assert_eq!(d_s, SessionFrameDecision::Drop, "non-MapM must not force");
        }

        // 4. Gate not ok + SyncedIdle / Panning / Closed with no Settling involved -> drop
        for (pre, post) in [
            (Some(MapState::SyncedIdle), Some(MapState::SyncedIdle)),
            (Some(MapState::SyncedIdle), Some(MapState::Panning)),
            (Some(MapState::Panning), Some(MapState::Panning)),
            (Some(MapState::Panning), Some(MapState::SyncedIdle)),
            (Some(MapState::SyncedIdle), Some(MapState::Closed)),
            (Some(MapState::Closed), Some(MapState::Closed)),
            (None, None),
        ] {
            let d = decide_session_frame_keep(true, false, TickMode::MapM, pre, post);
            assert_eq!(
                d,
                SessionFrameDecision::Drop,
                "non-settling MapM {pre:?} -> {post:?}"
            );
        }

        // 5. Recording inactive -> always drop (even if gate_ok or Settling)
        assert_eq!(
            decide_session_frame_keep(false, true, TickMode::Minimap, None, None),
            SessionFrameDecision::Drop
        );
        assert_eq!(
            decide_session_frame_keep(
                false,
                false,
                TickMode::MapM,
                Some(MapState::Settling),
                Some(MapState::SyncedIdle),
            ),
            SessionFrameDecision::Drop
        );
        assert_eq!(
            decide_session_frame_keep(
                false,
                true,
                TickMode::MapM,
                Some(MapState::Closed),
                Some(MapState::Settling),
            ),
            SessionFrameDecision::Drop
        );
    }

    #[test]
    fn unrecorded_tick_bucket_separates_settling_map_ticks() {
        assert_eq!(
            unrecorded_tick_bucket(TickMode::Minimap, None, None),
            "minimap"
        );
        assert_eq!(
            unrecorded_tick_bucket(TickMode::MinimapFast, None, None),
            "minimap_fast"
        );
        assert_eq!(
            unrecorded_tick_bucket(TickMode::MapMPaused, None, None),
            "map_m_paused"
        );
        assert_eq!(
            unrecorded_tick_bucket(TickMode::IdleClosed, None, None),
            "idle_closed"
        );
        assert_eq!(
            unrecorded_tick_bucket(
                TickMode::MapM,
                Some(MapState::SyncedIdle),
                Some(MapState::SyncedIdle)
            ),
            "map_m"
        );
        assert_eq!(
            unrecorded_tick_bucket(
                TickMode::MapM,
                Some(MapState::Closed),
                Some(MapState::Settling)
            ),
            "map_m_settling"
        );
        assert_eq!(
            unrecorded_tick_bucket(
                TickMode::MapM,
                Some(MapState::Settling),
                Some(MapState::SyncedIdle)
            ),
            "map_m_settling"
        );
    }

    #[test]
    fn step_map_frame_reuses_single_capture_for_settling_when_recording_and_skips_when_inactive() {
        use crate::capture_backend::FrameCapturer;
        use gh_win32::wgc_hdr::WgcWindowCaptureOutcome;
        use std::sync::atomic::AtomicUsize;

        struct FullCountingCapturer {
            region_calls: AtomicUsize,
        }

        impl FrameCapturer for FullCountingCapturer {
            fn capture_region(&self, _l: i32, _t: i32, w: i32, h: i32) -> Option<FrameBuf> {
                self.region_calls.fetch_add(1, Ordering::Relaxed);
                Some(FrameBuf::new_zeroed(w as u32, h as u32, PixelFormat::Bgr8))
            }

            fn capture_region_bgra(&self, _l: i32, _t: i32, _w: i32, _h: i32) -> Option<FrameBuf> {
                None
            }

            fn capture_window_client_outcome(
                &self,
                _hwnd: isize,
                _l: i32,
                _t: i32,
                _w: i32,
                _h: i32,
            ) -> WgcWindowCaptureOutcome {
                WgcWindowCaptureOutcome::Failed
            }

            fn window_capture_status(&self) -> String {
                "test".to_owned()
            }

            fn release_window_capture(&self) {}
            fn close(&self) {}
        }

        let tmp = std::env::temp_dir().join(format!(
            "gh_settling_reuse_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");

        let cap = Arc::new(FullCountingCapturer {
            region_calls: AtomicUsize::new(0),
        });
        let state = crate::state::make_test_state_with_capturer(
            &tmp,
            Arc::clone(&cap) as Arc<dyn FrameCapturer>,
        );

        // Force FSM into Settling (request_retry sets state = Settling)
        state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_retry();
        assert_eq!(
            state
                .state_machine
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .state(),
            MapState::Settling
        );

        // 1) Recording active + gate NOT ok (capture_session_frame = false):
        //    must reuse the single M-Map full capture without adding a second capture.
        let out_rec = run_tick_worker(
            &state,
            TickParams {
                game_hwnd: 0,
                cl: 0,
                ct: 0,
                cw: 640,
                ch: 360,
                is_closed: false,
                do_auto_sync: true,
                do_minimap: false,
                fast_ok: false,
                guide_view: false,
                guide_map: 2,
                want_window_capture: false,
                now_s: Some(5.0),
                cursor_client: None,
                capture_session_frame: false,
                session_rec_active: true,
            },
        );
        assert_eq!(out_rec.mode, TickMode::MapM);
        assert_eq!(out_rec.map_pre_state, Some(MapState::Settling));
        assert_eq!(cap.region_calls.load(Ordering::Relaxed), 1);
        let full_arc = out_rec.full_frame.as_ref().expect("full_frame present");
        let sess_arc = out_rec
            .session_frame
            .as_ref()
            .expect("session_frame must be populated for Settling tick");
        assert!(
            Arc::ptr_eq(full_arc, sess_arc),
            "session_frame must reuse the exact same Arc<FrameBuf>"
        );

        // 2) Recording inactive: session_frame must stay None even during Settling.
        state
            .state_machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_retry();
        let out_off = run_tick_worker(
            &state,
            TickParams {
                game_hwnd: 0,
                cl: 0,
                ct: 0,
                cw: 640,
                ch: 360,
                is_closed: false,
                do_auto_sync: true,
                do_minimap: false,
                fast_ok: false,
                guide_view: false,
                guide_map: 2,
                want_window_capture: false,
                now_s: Some(5.025),
                cursor_client: None,
                capture_session_frame: false,
                session_rec_active: false,
            },
        );
        assert_eq!(out_off.mode, TickMode::MapM);
        assert_eq!(out_off.map_pre_state, Some(MapState::Settling));
        assert!(out_off.session_frame.is_none());
        assert_eq!(cap.region_calls.load(Ordering::Relaxed), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
