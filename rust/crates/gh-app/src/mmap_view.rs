//! Live M-Map view tracking for route guidance: which part of which map the in-game M-Map shows.
//!
//! Wraps `gh_cv::view_tracker::ViewTracker` ("anchor + track", calibrated on the 130 s e5 drag
//! recording and optimized for 60 Hz live tracking with a zero-alloc 640×360 / 512×256 tiled-FFT
//! phase-correlation estimator):
//! * [`MMapViewEngine::step`] runs on every captured M-Map frame (tracking-loop worker thread):
//!   drawer check + frame-to-frame motion with the game cursor masked out;
//! * one registration worker (`gh-mmap-register`) runs `MapLocator::locate_view_with` on the
//!   newest *stable* frame whenever it is idle; its result is carried forward to the current frame
//!   inside the tracker, which decides whether it re-anchors, starts a candidate or is ignored.

use gh_core::frame::{FrameBuf, FrameView, PixelFormat};
use gh_cv::locator::{MapLocator, ViewHint, ViewOptions, ViewRegistration};
use gh_cv::ops::color::{frame_view_as_bgr, frame_view_to_bgr};
use gh_cv::view_tracker::{
    RegistrationVerdict, SliderScale, TrackSource, TrackerConfig, ViewTracker, ViewTransform,
    is_map_drawer_open,
};
use gh_cv::zoom_slider::{ZoomSliderDetector, ZoomSliderResult};
use ndarray::{ArrayView3, ShapeBuilder};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Registers one M-Map frame (the expensive "anchor" step).
pub trait ViewRegistrar: Send + Sync + 'static {
    fn register(&self, frame: &FrameBuf, slider: Option<&ZoomSliderResult>) -> Option<ViewRegistration>;
}

/// The region label is re-read at most this often while registrations keep succeeding.
const REGION_OCR_EVERY: Duration = Duration::from_millis(1500);
/// Telemetry summary cadence.
const STATS_WINDOW_SECS: f64 = 10.0;
/// Cap per-window latency samples so a 60 Hz stream (~600 samples / 10 s) never grows unbounded
/// if stats polling is paused.
const STATS_SAMPLE_CAP: usize = 1024;
const STATS_STEP_PREALLOC: usize = 640;
const STATS_REG_PREALLOC: usize = 128;

/// Frame-count horizons scaled from `TrackerConfig::default()` (30 fps baseline) to preserve the
/// same wall-clock timeouts at 60 Hz live tracking (20 s track, 3 s hidden, 8 s history).
fn live_60fps_config() -> TrackerConfig {
    let base = TrackerConfig::default();
    TrackerConfig {
        max_track_frames: base.max_track_frames * 2,
        hidden_max_frames: base.hidden_max_frames * 2,
        history_len: base.history_len * 2,
        ..base
    }
}

/// Zero-copy `(H, W, 3|4)` view for `Bgr8` or `Bgra8` frames (both `is_map_drawer_open` and
/// `ViewTracker::on_frame` read channels 0..3 as B, G, R).
fn frame_view_as_bgr_or_bgra(view: FrameView<'_>) -> Option<ArrayView3<'_, u8>> {
    if let Some(v) = frame_view_as_bgr(view) {
        return Some(v);
    }
    let (w, h) = (view.width as usize, view.height as usize);
    if view.format != PixelFormat::Bgra8 || w == 0 || h == 0 || view.stride < w * 4 {
        return None;
    }
    let data = view.data.get(..(h - 1) * view.stride + w * 4)?;
    ArrayView3::from_shape((h, w, 4).strides((view.stride, 4, 1)), data).ok()
}

/// `MapLocator` as the live registrar.
///
/// Live options from the e5 measurement: no terrain-only search (slow, produced stuck
/// registrations) and translation votes on Teyvat. The bottom-right region label is used as a
/// map prior only (on Teyvat it names the player's region, not the dragged-to view). It changes
/// only with the map, so its OCR result is reused for [`REGION_OCR_EVERY`]; a failed
/// registration drops it, so a map switch is picked up on the next one.
pub struct LocatorRegistrar {
    locator: Arc<MapLocator>,
    region: Mutex<Option<(Instant, Option<i32>)>>,
}

impl LocatorRegistrar {
    pub fn new(locator: Arc<MapLocator>) -> Self {
        Self {
            locator,
            region: Mutex::new(None),
        }
    }

    fn register_bgr(&self, bgr: ArrayView3<'_, u8>, slider: Option<&ZoomSliderResult>) -> Option<ViewRegistration> {
        let cached = {
            let g = self.region.lock().unwrap_or_else(|e| e.into_inner());
            g.filter(|(t, _)| t.elapsed() < REGION_OCR_EVERY).map(|(_, m)| m)
        };
        let map = cached.unwrap_or_else(|| {
            let m = self.locator.detect_region_name(bgr).0;
            *self.region.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), m));
            m
        });
        let opts = ViewOptions {
            allow_terrain_only: false,
            translation_votes: true,
        };
        let reg = self
            .locator
            .locate_view_with(bgr, slider, map.map_or(ViewHint::None, ViewHint::Map), opts);
        if reg.as_ref().is_none_or(|r| !r.is_confident()) {
            *self.region.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        reg
    }
}

impl ViewRegistrar for LocatorRegistrar {
    fn register(&self, frame: &FrameBuf, slider: Option<&ZoomSliderResult>) -> Option<ViewRegistration> {
        let fv = frame.as_view();
        if let Some(bgr) = frame_view_as_bgr(fv) {
            return self.register_bgr(bgr, slider);
        }
        let bgr = frame_view_to_bgr(fv).ok()?;
        self.register_bgr(bgr.view(), slider)
    }
}

/// What the overlay may use for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ViewFrame {
    /// Confident screen→world transform of the displayed view (`None` = draw nothing).
    pub transform: Option<ViewTransform>,
    pub anchored: bool,
    /// The map drawer covers the right of the screen: hide the overlay.
    pub drawer_open: bool,
    pub step_ms: f64,
}

struct Job {
    epoch: u64,
    seq: u64,
    frame: Arc<FrameBuf>,
    slider: Option<ZoomSliderResult>,
}

enum WorkerCmd {
    Register(Job),
    TrimScratch,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    shown: u32,
    drawer: u32,
    registrations: u32,
    anchors: u32,
    reg_ms: Vec<f64>,
    step_ms: Vec<f64>,
}

impl Stats {
    fn with_capacity() -> Self {
        Self {
            reg_ms: Vec::with_capacity(STATS_REG_PREALLOC),
            step_ms: Vec::with_capacity(STATS_STEP_PREALLOC),
            ..Self::default()
        }
    }

    fn clear_reusing_buffers(&mut self) {
        self.since = None;
        self.frames = 0;
        self.shown = 0;
        self.drawer = 0;
        self.registrations = 0;
        self.anchors = 0;
        self.reg_ms.clear();
        self.step_ms.clear();
    }

    fn push_sample(buf: &mut Vec<f64>, count: u32, val: f64) {
        if buf.len() < STATS_SAMPLE_CAP {
            buf.push(val);
        } else {
            let idx = (count as usize).wrapping_sub(1) % STATS_SAMPLE_CAP;
            buf[idx] = val;
        }
    }
}

struct Shared {
    tracker: Mutex<ViewTracker>,
    busy: AtomicBool,
    /// Frames were fed since the last `reset` (makes `reset` free when idle; the tracking loop
    /// calls it on every non-M-Map tick).
    active: AtomicBool,
    /// Bumped by `reset`; results of jobs from an older epoch are dropped.
    epoch: AtomicU64,
    trim_pending: AtomicBool,
    stats: Mutex<Stats>,
}

pub struct MMapViewEngine {
    shared: Arc<Shared>,
    jobs: Mutex<Option<SyncSender<WorkerCmd>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// When `Some`, registrations execute synchronously inline inside `step()` (zero background
    /// thread) for deterministic offline replay in `gh-sim`. Live production leaves this `None`.
    inline_registrar: Option<Arc<dyn ViewRegistrar>>,
}

impl MMapViewEngine {
    pub fn new<R: ViewRegistrar>(registrar: Arc<R>) -> Self {
        Self::with_config(registrar, live_60fps_config())
    }

    pub fn with_config<R: ViewRegistrar>(registrar: Arc<R>, cfg: TrackerConfig) -> Self {
        let shared = Arc::new(Shared {
            tracker: Mutex::new(ViewTracker::new(cfg)),
            busy: AtomicBool::new(false),
            active: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            trim_pending: AtomicBool::new(false),
            stats: Mutex::new(Stats::with_capacity()),
        });
        let (tx, rx) = sync_channel::<WorkerCmd>(2);
        let sh = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("gh-mmap-register".into())
            .spawn(move || registration_worker(&sh, registrar.as_ref(), &rx))
            .ok();
        Self {
            shared,
            jobs: Mutex::new(worker.as_ref().map(|_| tx)),
            worker: Mutex::new(worker),
            inline_registrar: None,
        }
    }

    /// Synchronous, zero-thread constructor for deterministic offline replay (`gh-sim`).
    /// Runs `ViewRegistrar::register` inline inside [`Self::step`] using the exact same
    /// registration & verdict logic as the live worker thread.
    pub fn new_sync<R: ViewRegistrar>(registrar: Arc<R>) -> Self {
        Self::with_config_sync(registrar, TrackerConfig::default())
    }

    pub fn with_config_sync<R: ViewRegistrar>(registrar: Arc<R>, cfg: TrackerConfig) -> Self {
        let shared = Arc::new(Shared {
            tracker: Mutex::new(ViewTracker::new(cfg)),
            busy: AtomicBool::new(false),
            active: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            trim_pending: AtomicBool::new(false),
            stats: Mutex::new(Stats::with_capacity()),
        });
        Self {
            shared,
            jobs: Mutex::new(None),
            worker: Mutex::new(None),
            inline_registrar: Some(registrar as Arc<dyn ViewRegistrar>),
        }
    }

    /// Feeds one captured M-Map frame (game-client pixels). `cursor` = mouse hotspot in the same
    /// pixels; `map_fallback` = map assumed for the slider scale until a view is registered.
    pub fn step(
        &self,
        frame: Arc<FrameBuf>,
        slider: Option<&ZoomSliderResult>,
        cursor: Option<(f64, f64)>,
        map_fallback: i32,
    ) -> ViewFrame {
        let t0 = Instant::now();
        self.shared.active.store(true, Ordering::Release);
        let fv = frame.as_view();
        let owned;
        let bgr = match frame_view_as_bgr_or_bgra(fv) {
            Some(v) => v,
            None => {
                let Ok(a) = frame_view_to_bgr(fv) else {
                    return ViewFrame::default();
                };
                owned = a;
                owned.view()
            }
        };
        let drawer_open = is_map_drawer_open(bgr);
        let slider = slider.filter(|s| s.found && s.ratio_t.is_finite());
        let (seq, stable, mut out) = {
            let mut tr = self.shared.tracker.lock().unwrap_or_else(|e| e.into_inner());
            let map_id = tr.output().transform.map_or(map_fallback, |t| t.map_id);
            let reading = ZoomSliderDetector::get_map_scale(slider, map_id, bgr.dim().1)
                .map(|scale| SliderScale { map_id, scale });
            let seq = tr.on_frame(bgr, reading, cursor);
            (seq, tr.frame_is_stable(), tr.output())
        };
        if stable
            && !self.shared.busy.load(Ordering::Acquire)
            && !self.shared.busy.swap(true, Ordering::AcqRel)
        {
            let job = Job {
                epoch: self.shared.epoch.load(Ordering::Acquire),
                seq,
                frame,
                slider: slider.cloned(),
            };
            if let Some(ref inline_reg) = self.inline_registrar {
                run_one_registration(&self.shared, inline_reg.as_ref(), job);
                out = self
                    .shared
                    .tracker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .output();
            } else {
                let sent = self
                    .jobs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|tx| match tx.try_send(WorkerCmd::Register(job)) {
                        Ok(()) => true,
                        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                    });
                if !sent {
                    self.shared.busy.store(false, Ordering::Release);
                }
            }
        }
        let step_ms = t0.elapsed().as_secs_f64() * 1000.0;
        {
            let mut st = self.shared.stats.lock().unwrap_or_else(|e| e.into_inner());
            st.since.get_or_insert_with(Instant::now);
            st.frames = st.frames.saturating_add(1);
            st.shown = st.shown.saturating_add(u32::from(out.transform.is_some() && !drawer_open));
            st.drawer = st.drawer.saturating_add(u32::from(drawer_open));
            let count = st.frames;
            Stats::push_sample(&mut st.step_ms, count, step_ms);
        }
        ViewFrame {
            transform: out.transform,
            anchored: out.source == Some(TrackSource::Anchored),
            drawer_open,
            step_ms,
        }
    }

    /// Forgets the view (M-Map closed, guidance off, game window lost). Pending registrations
    /// are discarded when they finish.
    pub fn reset(&self) {
        if !self.shared.active.swap(false, Ordering::AcqRel) {
            return;
        }
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.trim_pending.store(true, Ordering::Release);
        self.shared
            .tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset();
        self.clear_stats();
        if let Some(ref tx) = *self.jobs.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = tx.try_send(WorkerCmd::TrimScratch);
        }
    }

    /// One log line per ~10 s of M-Map guidance (`None` until then).
    pub fn take_stats_line(&self) -> Option<String> {
        let mut guard = self.shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *guard;
        let secs = st.since?.elapsed().as_secs_f64();
        if secs < STATS_WINDOW_SECS || st.frames == 0 {
            return None;
        }
        let pct = |v: &mut Vec<f64>, q: f64| {
            if v.is_empty() {
                return 0.0;
            }
            v.sort_by(f64::total_cmp);
            v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
        };
        let line = format!(
            "[M-Map view] {:.1} fps, drawn {:.0} %, drawer {:.0} %, step p50 {:.1} / p95 {:.1} ms, \
             registrations {} ({:.1}/s, p50 {:.0} ms), anchors {}",
            f64::from(st.frames) / secs,
            100.0 * f64::from(st.shown) / f64::from(st.frames),
            100.0 * f64::from(st.drawer) / f64::from(st.frames),
            pct(&mut st.step_ms, 0.5),
            pct(&mut st.step_ms, 0.95),
            st.registrations,
            f64::from(st.registrations) / secs,
            pct(&mut st.reg_ms, 0.5),
            st.anchors,
        );
        st.clear_reusing_buffers();
        Some(line)
    }

    /// Clears the stats window (guidance paused) so rates are not diluted by idle time.
    pub fn clear_stats(&self) {
        self.shared
            .stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear_reusing_buffers();
    }

    #[cfg(test)]
    fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::Acquire)
    }
}

impl Drop for MMapViewEngine {
    fn drop(&mut self) {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

fn run_one_registration(shared: &Shared, registrar: &dyn ViewRegistrar, job: Job) {
    let t0 = Instant::now();
    let reg = registrar.register(&job.frame, job.slider.as_ref());
    let reg_ms = t0.elapsed().as_secs_f64() * 1000.0;
    drop(job.frame);
    let verdict = reg.as_ref().and_then(|r| {
        if shared.epoch.load(Ordering::Acquire) != job.epoch {
            return None;
        }
        let mut tr = shared.tracker.lock().unwrap_or_else(|e| e.into_inner());
        // `reset` may have raced the check above; it bumps the epoch before resetting.
        (shared.epoch.load(Ordering::Acquire) == job.epoch).then(|| tr.on_registration(job.seq, r))
    });
    {
        let mut st = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.registrations = st.registrations.saturating_add(1);
        let count = st.registrations;
        Stats::push_sample(&mut st.reg_ms, count, reg_ms);
        st.anchors = st.anchors.saturating_add(u32::from(matches!(
            verdict,
            Some(
                RegistrationVerdict::Agrees
                    | RegistrationVerdict::Restored
                    | RegistrationVerdict::Acquired
                    | RegistrationVerdict::Switched
            )
        )));
    }
    if shared.trim_pending.swap(false, Ordering::AcqRel)
        || shared.epoch.load(Ordering::Acquire) != job.epoch
    {
        gh_cv::locator::release_thread_scratch();
    }
    shared.busy.store(false, Ordering::Release);
}

fn registration_worker(shared: &Shared, registrar: &dyn ViewRegistrar, rx: &Receiver<WorkerCmd>) {
    while let Ok(cmd) = rx.recv() {
        let job = match cmd {
            WorkerCmd::TrimScratch => {
                shared.trim_pending.store(false, Ordering::Release);
                gh_cv::locator::release_thread_scratch();
                continue;
            }
            WorkerCmd::Register(job) => job,
        };
        run_one_registration(shared, registrar, job);
    }
    gh_cv::locator::release_thread_scratch();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Textured synthetic "map" shifted by (ox, oy) px.
    fn frame(ox: f64, oy: f64) -> Arc<FrameBuf> {
        let (w, h) = (960u32, 540u32);
        let mut f = FrameBuf::new_zeroed(w, h, PixelFormat::Bgr8);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let (u, v) = (x as f64 - ox, y as f64 - oy);
                let val = 128.0
                    + 40.0 * (u * 0.013).sin() * (v * 0.017).cos()
                    + 30.0 * (u * 0.051 + v * 0.023).sin()
                    + 20.0 * ((u * 0.11).sin() * (v * 0.07).sin());
                let g = val.clamp(0.0, 255.0) as u8;
                let i = y * f.stride + x * 3;
                f.data[i..i + 3].copy_from_slice(&[g, g.wrapping_add(7), g.wrapping_sub(5)]);
            }
        }
        Arc::new(f)
    }

    /// Registers every frame at a fixed transform (world = 2·screen + (1000, 500)) — i.e. it
    /// "sees" the view as it was at the first frame; the tracker carries it forward.
    struct FakeRegistrar {
        calls: AtomicUsize,
        delay_ms: u64,
    }

    impl ViewRegistrar for FakeRegistrar {
        fn register(&self, _frame: &FrameBuf, _slider: Option<&ZoomSliderResult>) -> Option<ViewRegistration> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            std::thread::sleep(Duration::from_millis(self.delay_ms));
            Some(ViewRegistration {
                map_id: 2,
                scale: 2.0,
                tx: 1000.0,
                ty: 500.0,
                inliers_count: 12,
                rmse: 1.0,
                terrain_score: 0.5,
                terrain_only: false,
                zoom_ratio_t: Some(0.5),
                is_underground: false,
                candidates_count: 12,
            })
        }
    }

    fn wait_idle(e: &MMapViewEngine) {
        let t0 = Instant::now();
        while e.is_busy() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn worker_registration_acquires_the_view_and_reset_drops_it() {
        let reg = Arc::new(FakeRegistrar { calls: AtomicUsize::new(0), delay_ms: 5 });
        let e = MMapViewEngine::new(Arc::clone(&reg));
        // First frame: no motion history yet → not stable → no registration started.
        let v = e.step(frame(0.0, 0.0), None, None, 2);
        assert!(v.transform.is_none() && !v.drawer_open);
        assert_eq!(reg.calls.load(Ordering::Acquire), 0);
        // Static frames: stable → the worker registers; a 12-inlier match acquires.
        for _ in 0..3 {
            e.step(frame(0.0, 0.0), None, None, 2);
            wait_idle(&e);
        }
        let v = e.step(frame(0.0, 0.0), None, None, 2);
        let t = v.transform.expect("acquired");
        assert_eq!(t.map_id, 2);
        assert!((t.tx - 1000.0).abs() < 1.0 && (t.ty - 500.0).abs() < 1.0, "{t:?}");
        assert!(reg.calls.load(Ordering::Acquire) >= 1);

        e.reset();
        let v = e.step(frame(0.0, 0.0), None, None, 2);
        assert!(v.transform.is_none(), "reset forgets the view");
    }

    #[test]
    fn a_registration_finishing_after_reset_is_discarded() {
        let reg = Arc::new(FakeRegistrar { calls: AtomicUsize::new(0), delay_ms: 150 });
        let e = MMapViewEngine::new(Arc::clone(&reg));
        e.step(frame(0.0, 0.0), None, None, 2);
        e.step(frame(0.0, 0.0), None, None, 2);
        assert!(e.is_busy(), "registration started");
        e.reset();
        wait_idle(&e);
        let v = e.step(frame(0.0, 0.0), None, None, 2);
        assert!(v.transform.is_none(), "stale-epoch result must not anchor the new session");
    }

    #[test]
    fn stats_line_waits_for_a_window_and_drawer_frames_are_flagged() {
        let reg = Arc::new(FakeRegistrar { calls: AtomicUsize::new(0), delay_ms: 0 });
        let e = MMapViewEngine::new(reg);
        let mut f = (*frame(0.0, 0.0)).clone();
        let (w, h) = (f.width as usize, f.height as usize);
        for y in (h * 10 / 100)..(h * 90 / 100) {
            for x in (w * 67 / 100)..w {
                let i = y * f.stride + x * 3;
                f.data[i..i + 3].copy_from_slice(&[61, 48, 37]);
            }
        }
        let v = e.step(Arc::new(f), None, None, 2);
        assert!(v.drawer_open);
        assert!(e.take_stats_line().is_none(), "no line before 10 s");
        e.shared.stats.lock().expect("stats").since = Some(Instant::now() - Duration::from_secs(11));
        let line = e.take_stats_line().expect("line");
        assert!(line.contains("drawer 100 %"), "{line}");
    }

    #[test]
    fn live_60fps_config_scales_frame_horizons_and_bgra_is_zero_copy() {
        let base = TrackerConfig::default();
        let live = live_60fps_config();
        assert_eq!(live.max_track_frames, base.max_track_frames * 2);
        assert_eq!(live.hidden_max_frames, base.hidden_max_frames * 2);
        assert_eq!(live.history_len, base.history_len * 2);
        assert_eq!(live.min_peak.to_bits(), base.min_peak.to_bits());
        assert_eq!(live.agree_px_1440.to_bits(), base.agree_px_1440.to_bits());

        // Build a BGRA8 frame from the synthetic BGR frame and verify zero-copy view + tracking.
        let make_bgra = |ox: f64, oy: f64| {
            let bgr = frame(ox, oy);
            let mut bgra = FrameBuf::new_zeroed(bgr.width, bgr.height, PixelFormat::Bgra8);
            for y in 0..bgr.height as usize {
                for x in 0..bgr.width as usize {
                    let s = y * bgr.stride + x * 3;
                    let d = y * bgra.stride + x * 4;
                    bgra.data[d..d + 3].copy_from_slice(&bgr.data[s..s + 3]);
                    bgra.data[d + 3] = 255;
                }
            }
            Arc::new(bgra)
        };
        let f0 = make_bgra(0.0, 0.0);
        let view = frame_view_as_bgr_or_bgra(f0.as_view()).expect("zero-copy bgra view");
        assert_eq!(view.dim(), (f0.height as usize, f0.width as usize, 4));
        assert_eq!(view.as_ptr(), f0.data.as_ptr());

        let reg = Arc::new(FakeRegistrar { calls: AtomicUsize::new(0), delay_ms: 0 });
        let e = MMapViewEngine::new(reg);
        for _ in 0..3 {
            e.step(Arc::clone(&f0), None, None, 2);
            wait_idle(&e);
        }
        let v = e.step(make_bgra(20.0, 0.0), None, None, 2);
        let t = v.transform.expect("tracked bgra");
        // Content shifted +20 px at scale 2.0 → tx shifts by -40 world units from 1000 to ~960.
        assert!((t.tx - 960.0).abs() < 6.0, "expected tx near 960, got {t:?}");

        // Stats sample buffer remains bounded and reuses capacity across resets.
        let mut st = Stats::with_capacity();
        let cap_before = st.step_ms.capacity();
        for i in 1..=(STATS_SAMPLE_CAP as u32 + 200) {
            Stats::push_sample(&mut st.step_ms, i, f64::from(i));
        }
        assert_eq!(st.step_ms.len(), STATS_SAMPLE_CAP);
        st.clear_reusing_buffers();
        assert!(st.step_ms.is_empty());
        assert!(st.step_ms.capacity() >= cap_before);
    }

    #[test]
    fn sync_registration_acquires_view_inline_without_worker_thread() {
        let reg = Arc::new(FakeRegistrar { calls: AtomicUsize::new(0), delay_ms: 0 });
        let e = MMapViewEngine::new_sync(Arc::clone(&reg));
        assert!(
            e.worker.lock().unwrap().is_none(),
            "sync mode must not spawn a background worker thread"
        );
        let f = frame(0.0, 0.0);
        let mut acquired = None;
        for _ in 0..4 {
            let v = e.step(Arc::clone(&f), None, None, 2);
            if v.transform.is_some() {
                acquired = Some(v);
                break;
            }
        }
        let v = acquired.expect("sync mode must acquire view deterministically without sleeping");
        assert!(v.anchored);
        assert!(reg.calls.load(Ordering::Relaxed) >= 1);
        assert!(!e.is_busy());
        e.reset();
        let after = e.step(frame(40.0, 0.0), None, None, 2);
        assert!(after.transform.is_none());
    }
}
