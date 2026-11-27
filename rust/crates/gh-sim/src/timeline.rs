//! Deterministic input timeline (recorded hotkeys + web callbacks ordered by `ts`) for `gh-sim`.
//!
//! Designed so both recorded sessions (`meta.jsonl` hotkeys + `web_inputs.jsonl`) and future
//! scripted input injections share the exact same `TimelineInput` representation and dispatch path.

use gh_app::callbacks::AppServerCallbacks;
use gh_app::session_recorder::{NavFrameSnapshot, WebInputRecord};
use gh_app::state::AppState;
use gh_core::traits::ServerCallbacks;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// One replayable user or web input event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineInputKind {
    Hotkey { action: String },
    WebInput { callback: String, payload: Value },
}

/// Timestamped replay input ordered against frame timestamps (`ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineInput {
    pub ts: f64,
    #[serde(flatten)]
    pub kind: TimelineInputKind,
}

impl TimelineInput {
    pub fn hotkey(ts: f64, action: impl Into<String>) -> Self {
        Self {
            ts,
            kind: TimelineInputKind::Hotkey {
                action: action.into(),
            },
        }
    }

    pub fn web_input(ts: f64, callback: impl Into<String>, payload: Value) -> Self {
        Self {
            ts,
            kind: TimelineInputKind::WebInput {
                callback: callback.into(),
                payload,
            },
        }
    }
}

/// Sorted input timeline drained incrementally against frame timestamps.
///
/// Ordering rule: an input at timestamp `t` is dispatched after every frame with `frame_ts < t`
/// has been processed and before the first frame with `frame_ts >= t`.
#[derive(Debug, Clone, Default)]
pub struct InputTimeline {
    events: Vec<TimelineInput>,
    cursor: usize,
}

impl InputTimeline {
    /// Builds a timeline from recorded web inputs and recorded hotkeys.
    ///
    /// Events are stably sorted by `ts`; at identical timestamps, web inputs precede hotkeys so
    /// e.g. pins delivered at `t` are visible to a hotkey pressed at `t`.
    pub fn from_inputs(web_inputs: Vec<WebInputRecord>, hotkeys: Vec<(f64, String)>) -> Self {
        let mut events = Vec::with_capacity(web_inputs.len() + hotkeys.len());
        for w in web_inputs {
            if w.ts.is_finite() {
                events.push(TimelineInput::web_input(w.ts, w.callback, w.payload));
            }
        }
        for (ts, action) in hotkeys {
            if ts.is_finite() {
                events.push(TimelineInput::hotkey(ts, action));
            }
        }
        Self::from_events(events)
    }

    /// Builds a timeline directly from pre-constructed or script-loaded [`TimelineInput`] items.
    pub fn from_events(mut events: Vec<TimelineInput>) -> Self {
        events.retain(|e| e.ts.is_finite());
        events.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        Self { events, cursor: 0 }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns all pending events with `event.ts <= frame_ts` to dispatch immediately before
    /// processing the frame at `frame_ts`.
    pub fn drain_before_frame(&mut self, frame_ts: f64) -> Vec<TimelineInput> {
        let start = self.cursor;
        while self.cursor < self.events.len() && self.events[self.cursor].ts <= frame_ts {
            self.cursor += 1;
        }
        self.events[start..self.cursor].to_vec()
    }

    /// Returns any remaining events whose timestamps fall after the last replayed frame.
    pub fn drain_remaining(&mut self) -> Vec<TimelineInput> {
        let start = self.cursor;
        self.cursor = self.events.len();
        self.events[start..].to_vec()
    }
}

/// Summary of replayed hotkeys, delivered web inputs, and live-vs-replay navigation parity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReplayReport {
    pub hotkeys_recorded: usize,
    pub hotkeys_dispatched: usize,
    pub hotkeys_skipped: usize,
    pub hotkeys_dispatched_by_action: BTreeMap<String, usize>,
    pub hotkeys_skipped_by_action: BTreeMap<String, usize>,
    pub web_inputs_delivered: usize,
    pub web_inputs_by_callback: BTreeMap<String, usize>,
    pub web_inputs_skipped: usize,
    pub nav_frames_compared: usize,
    pub nav_mismatches: usize,
    /// Informational count of nav mismatches strictly before the first recorded web input
    /// in legacy recordings that lack an `initial_state` snapshot. Not counted in `nav_mismatches`.
    #[serde(default)]
    pub nav_carried_over_mismatches: usize,
    /// `true` when an `initial_state` snapshot was present in `web_inputs.jsonl` and applied
    /// before frame 1.
    #[serde(default)]
    pub initial_state_applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_nav_mismatch_idx: Option<usize>,
}

/// Loads recorded web callback payloads from `<session>/web_inputs.jsonl` (returns empty if absent).
pub fn load_web_inputs_jsonl(path: &Path) -> Result<Vec<WebInputRecord>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let f = File::open(path).map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for (line_no, line_res) in reader.lines().enumerate() {
        let line = line_res
            .map_err(|e| format!("read error at {}:{}: {e}", path.display(), line_no + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: WebInputRecord = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON at {}:{}: {e}", path.display(), line_no + 1))?;
        out.push(rec);
    }
    Ok(out)
}

/// Dispatches one timeline input through production `gh_app::dispatch_hotkey_action` or
/// `AppServerCallbacks` and updates `report`.
pub fn dispatch_timeline_input(
    state: &Arc<AppState>,
    callbacks: &AppServerCallbacks,
    input: &TimelineInput,
    report: &mut InputReplayReport,
) {
    match &input.kind {
        TimelineInputKind::Hotkey { action } => {
            report.hotkeys_recorded += 1;
            if gh_app::is_replay_dispatchable_hotkey(action) {
                gh_app::dispatch_hotkey_action(state, action);
                report.hotkeys_dispatched += 1;
                *report
                    .hotkeys_dispatched_by_action
                    .entry(action.clone())
                    .or_insert(0) += 1;
            } else {
                report.hotkeys_skipped += 1;
                *report
                    .hotkeys_skipped_by_action
                    .entry(action.clone())
                    .or_insert(0) += 1;
            }
        }
        TimelineInputKind::WebInput { callback, payload } => {
            let res: Result<(), String> = match callback.as_str() {
                "on_nearby_pins" => callbacks.on_nearby_pins(payload).map(|_| ()).map_err(|e| e.to_string()),
                "on_nav_command" => callbacks.on_nav_command(payload).map(|_| ()).map_err(|e| e.to_string()),
                "on_route_command" => callbacks.on_route_command(payload).map(|_| ()).map_err(|e| e.to_string()),
                "on_plan_tsp_route" => callbacks.on_plan_tsp_route(payload).map(|_| ()).map_err(|e| e.to_string()),
                "on_set_nav_hud" => callbacks.on_set_nav_hud(payload).map(|_| ()).map_err(|e| e.to_string()),
                _ => {
                    report.web_inputs_skipped += 1;
                    return;
                }
            };
            if let Err(err) = res {
                tracing::debug!(
                    "[gh-sim timeline] callback '{}' returned error at ts={}: {err}",
                    callback,
                    input.ts
                );
            }
            report.web_inputs_delivered += 1;
            *report
                .web_inputs_by_callback
                .entry(callback.clone())
                .or_insert(0) += 1;
        }
    }
}

/// Compares replay's post-tick `NavFrameSnapshot` against live `meta.jsonl` `nav`
/// (target id or locked target id) and records counters on `report`.
pub fn record_nav_comparison(
    frame_idx: usize,
    live_nav: &NavFrameSnapshot,
    replay_nav: &NavFrameSnapshot,
    report: &mut InputReplayReport,
) {
    record_nav_comparison_with_carryover(
        frame_idx,
        0.0,
        None,
        false,
        live_nav,
        replay_nav,
        report,
    );
}

/// Like [`record_nav_comparison`], but for legacy recordings without an `initial_state` snapshot
/// (`!initial_state_applied`) routes mismatches strictly before `first_web_input_ts` into
/// `nav_carried_over_mismatches` instead of `nav_mismatches`.
pub fn record_nav_comparison_with_carryover(
    frame_idx: usize,
    frame_ts: f64,
    first_web_input_ts: Option<f64>,
    initial_state_applied: bool,
    live_nav: &NavFrameSnapshot,
    replay_nav: &NavFrameSnapshot,
    report: &mut InputReplayReport,
) {
    report.nav_frames_compared += 1;
    if replay_nav.target_id != live_nav.target_id
        || replay_nav.locked_target_id != live_nav.locked_target_id
    {
        let is_legacy_pre_web = !initial_state_applied
            && first_web_input_ts.is_some_and(|t0| frame_ts.is_finite() && frame_ts < t0);
        if is_legacy_pre_web {
            report.nav_carried_over_mismatches += 1;
        } else {
            report.nav_mismatches += 1;
            report.first_nav_mismatch_idx.get_or_insert(frame_idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timeline_dispatches_after_all_earlier_frames_and_before_first_ge_frame() {
        let web_inputs = vec![
            WebInputRecord {
                ts: 1.0,
                callback: "on_nearby_pins".to_owned(),
                payload: json!({"pins": []}),
            },
            WebInputRecord {
                ts: 0.5,
                callback: "on_set_nav_hud".to_owned(),
                payload: json!({"enabled": true}),
            },
        ];
        // Intentionally unsorted hotkeys, including:
        // - ts == frame 1 ts (1.0) -> after web_input at 1.0, before frame 1
        // - ts between frame 1 (1.0) and frame 2 (2.0) -> after frame 1, before frame 2
        // - ts slightly > frame 2 (2.05) even if attached to frame 2 -> after frame 2, before frame 3 (3.0)
        // - ts > last frame (3.5) -> drained after last frame
        let hotkeys = vec![
            (3.5, "nav_unlock".to_owned()),
            (2.05, "nav_guide".to_owned()),
            (1.0, "nav_next".to_owned()),
            (1.4, "nav_mark".to_owned()),
        ];

        let mut tl = InputTimeline::from_inputs(web_inputs, hotkeys);
        assert_eq!(tl.len(), 6);

        let mut trace: Vec<String> = Vec::new();
        let frame_timestamps = [1.0, 2.0, 3.0];
        for (idx, &fts) in frame_timestamps.iter().enumerate() {
            for ev in tl.drain_before_frame(fts) {
                match ev.kind {
                    TimelineInputKind::WebInput { callback, .. } => {
                        trace.push(format!("web:{callback}@{:.2}", ev.ts));
                    }
                    TimelineInputKind::Hotkey { action } => {
                        trace.push(format!("hk:{action}@{:.2}", ev.ts));
                    }
                }
            }
            trace.push(format!("frame:{}@{:.2}", idx + 1, fts));
        }
        for ev in tl.drain_remaining() {
            match ev.kind {
                TimelineInputKind::WebInput { callback, .. } => {
                    trace.push(format!("web:{callback}@{:.2}", ev.ts));
                }
                TimelineInputKind::Hotkey { action } => {
                    trace.push(format!("hk:{action}@{:.2}", ev.ts));
                }
            }
        }

        assert_eq!(
            trace,
            vec![
                "web:on_set_nav_hud@0.50",
                "web:on_nearby_pins@1.00",
                "hk:nav_next@1.00",
                "frame:1@1.00",
                "hk:nav_mark@1.40",
                "frame:2@2.00",
                "hk:nav_guide@2.05",
                "frame:3@3.00",
                "hk:nav_unlock@3.50",
            ]
        );
    }

    #[test]
    fn timeline_json_roundtrip_supports_scripted_injection() {
        let raw = json!([
            {"ts": 1.25, "kind": "web_input", "callback": "on_nearby_pins", "payload": {"map_id": 2, "pins": []}},
            {"ts": 2.50, "kind": "hotkey", "action": "nav_next"}
        ]);
        let parsed: Vec<TimelineInput> = serde_json::from_value(raw).unwrap();
        let mut tl = InputTimeline::from_events(parsed);
        let b1 = tl.drain_before_frame(2.0);
        assert_eq!(b1.len(), 1);
        assert!(matches!(b1[0].kind, TimelineInputKind::WebInput { .. }));
        let b2 = tl.drain_before_frame(3.0);
        assert_eq!(b2.len(), 1);
        assert!(matches!(b2[0].kind, TimelineInputKind::Hotkey { .. }));
    }

    #[test]
    fn allowlist_excludes_sync_switch_debug_and_unknown_actions() {
        for allowed in [
            "nav_next",
            "nav_unlock",
            "nav_guide",
            "nav_layout",
            "nav_toggle",
            "nav_mark",
            "nav_undo",
            "radar_toggle",
        ] {
            assert!(
                gh_app::is_replay_dispatchable_hotkey(allowed),
                "{allowed} should be replay-dispatchable"
            );
        }
        for skipped in ["sync", "switch", "debug", "unknown_action", ""] {
            assert!(
                !gh_app::is_replay_dispatchable_hotkey(skipped),
                "{skipped} must NOT be replay-dispatchable"
            );
        }
    }
}
