//! Linux `LayeredWindowHost` implementation for `gh-hud`.
//!
//! Runs a dedicated UI/event-loop thread per overlay (`toast`, `nav_hud`, `mmap_overlay`,
//! `radar_map`) with an `mpsc` message queue and software timers, matching the exact
//! lifecycle and API of the Win32 `LayeredWindowHost`.

use crate::HudResult;
use crate::layered::{
    LayeredHandler, LayeredWindowConfig, WDA_REFRESH_MS, is_hud_offscreen_env,
    resolve_screen_offset, rgba_to_premultiplied_bgra_into,
};
use crate::wayland_overlay::{PointerEvent, WaylandOverlay};
use gh_core::frame::RectI32;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static INSTANCE_SEQ: AtomicUsize = AtomicUsize::new(1);

const WM_APP_HOST_CLOSE: u32 = 0x8000 + 0x7F;
const TIMER_ID_WDA_REFRESH: usize = 102;

pub fn query_monitor_work_area(_point_x: i32, _point_y: i32) -> (i32, i32, i32, i32) {
    gh_platform::capture::get_screen_bounds()
}

pub fn query_monitor_bounds(_point_x: i32, _point_y: i32) -> (i32, i32, i32, i32) {
    gh_platform::capture::get_screen_bounds()
}

// ---------------------------------------------------------------------------
// LayeredDibSurface (software premultiplied BGRA backing buffer)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LayeredDibSurface {
    width: i32,
    height: i32,
    pos_x: i32,
    pos_y: i32,
    global_alpha: u8,
    bgra: Vec<u8>,
    realloc_count: usize,
    namespace: String,
    wl: Option<WaylandOverlay>,
    wl_init_attempted: bool,
}

impl std::fmt::Debug for LayeredDibSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayeredDibSurface")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("realloc_count", &self.realloc_count)
            .finish()
    }
}

impl LayeredDibSurface {
    pub fn new() -> Self {
        Self {
            global_alpha: 255,
            namespace: "genshin-helper-overlay".to_owned(),
            ..Default::default()
        }
    }

    pub fn with_namespace(namespace: impl Into<String>) -> Self {
        Self {
            global_alpha: 255,
            namespace: namespace.into(),
            ..Default::default()
        }
    }

    pub const fn is_ready(&self) -> bool {
        self.width > 0 && self.height > 0 && !self.bgra.is_empty()
    }

    pub const fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub const fn realloc_count(&self) -> usize {
        self.realloc_count
    }

    pub fn ensure_size(&mut self, width: i32, height: i32) -> HudResult<bool> {
        if width <= 0 || height <= 0 {
            return Ok(false);
        }
        if self.width == width && self.height == height && !self.bgra.is_empty() {
            return Ok(false);
        }
        let needed = (width as usize) * (height as usize) * 4;
        self.bgra.resize(needed, 0);
        self.width = width;
        self.height = height;
        self.realloc_count += 1;
        Ok(true)
    }

    pub fn upload_premultiplied_rgba(
        &mut self,
        width: i32,
        height: i32,
        rgba: &[u8],
    ) -> HudResult<()> {
        self.ensure_size(width, height)?;
        let n = (width as usize) * (height as usize) * 4;
        if rgba.len() < n {
            return Err(crate::HudError::InvalidBuffer("rgba buffer too small".into()));
        }
        for i in (0..n).step_by(4) {
            self.bgra[i] = rgba[i + 2];
            self.bgra[i + 1] = rgba[i + 1];
            self.bgra[i + 2] = rgba[i];
            self.bgra[i + 3] = rgba[i + 3];
        }
        Ok(())
    }

    pub fn upload_straight_rgba(&mut self, width: i32, height: i32, rgba: &[u8]) -> HudResult<()> {
        self.ensure_size(width, height)?;
        rgba_to_premultiplied_bgra_into(rgba, &mut self.bgra)
    }

    pub fn upload_premultiplied_bgra(
        &mut self,
        width: i32,
        height: i32,
        bgra: &[u8],
    ) -> HudResult<()> {
        self.ensure_size(width, height)?;
        let n = (width as usize) * (height as usize) * 4;
        if bgra.len() < n {
            return Err(crate::HudError::InvalidBuffer("bgra buffer too small".into()));
        }
        self.bgra[..n].copy_from_slice(&bgra[..n]);
        Ok(())
    }

    pub fn present(
        &mut self,
        _hwnd: isize,
        x: i32,
        y: i32,
        global_alpha: u8,
        click_through: bool,
    ) -> HudResult<()> {
        self.pos_x = x;
        self.pos_y = y;
        self.global_alpha = global_alpha;
        if x <= -10_000 || y <= -10_000 || self.width <= 0 || self.height <= 0 {
            if let Some(ref mut wl) = self.wl {
                wl.hide();
            }
            return Ok(());
        }
        if !self.wl_init_attempted {
            self.wl_init_attempted = true;
            let ns = if self.namespace.is_empty() {
                "genshin-helper-overlay"
            } else {
                &self.namespace
            };
            self.wl = WaylandOverlay::try_new(ns);
        }
        if let Some(ref mut wl) = self.wl {
            wl.present(
                x,
                y,
                self.width,
                self.height,
                &self.bgra,
                global_alpha,
                click_through,
            )?;
        }
        Ok(())
    }

    pub fn hide_overlay(&mut self) {
        if let Some(ref mut wl) = self.wl {
            wl.hide();
        }
    }

    pub fn set_click_through(&mut self, click_through: bool) {
        if let Some(ref mut wl) = self.wl {
            wl.set_click_through(click_through);
        }
    }

    pub fn pump_events(&mut self) -> Vec<PointerEvent> {
        if let Some(ref mut wl) = self.wl {
            wl.pump_events()
        } else {
            Vec::new()
        }
    }

    pub fn release(&mut self) {
        self.hide_overlay();
        self.bgra.clear();
        self.bgra.shrink_to_fit();
        self.width = 0;
        self.height = 0;
    }
}

// ---------------------------------------------------------------------------
// Host state & message loop
// ---------------------------------------------------------------------------

enum HostMsg {
    App { msg: u32, wparam: usize, lparam: isize },
    Close,
}

struct HostMutable {
    hwnd: isize,
    closed: bool,
    ready_done: bool,
    visible: bool,
    edit_mode: bool,
    screen_offset: (i32, i32),
    surface_size: (i32, i32),
    surface_realloc_count: usize,
    tx: Option<Sender<HostMsg>>,
}

struct HostShared {
    state: Mutex<HostMutable>,
    ready_cv: Condvar,
    capture_visible: AtomicBool,
    wda_refresh_count: AtomicUsize,
    config: LayeredWindowConfig,
}

pub struct LayeredWindowCtx<'a> {
    hwnd: isize,
    shared: &'a Arc<HostShared>,
    surface: &'a mut LayeredDibSurface,
    visible: &'a mut bool,
    dragging: &'a mut bool,
    edit_mode: &'a mut bool,
    click_through: &'a mut bool,
    timers: &'a RefCell<HashMap<usize, (Duration, Instant)>>,
    should_quit: &'a Cell<bool>,
}

impl<'a> LayeredWindowCtx<'a> {
    pub fn raw_hwnd(&self) -> isize {
        self.hwnd
    }

    pub const fn is_visible(&self) -> bool {
        *self.visible
    }

    pub const fn is_dragging(&self) -> bool {
        *self.dragging
    }

    pub const fn is_edit_mode(&self) -> bool {
        *self.edit_mode
    }

    pub const fn is_click_through(&self) -> bool {
        *self.click_through
    }

    pub fn surface_size(&self) -> (i32, i32) {
        self.surface.size()
    }

    pub fn surface_realloc_count(&self) -> usize {
        self.surface.realloc_count()
    }

    pub fn screen_offset(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset
    }

    fn sync_shared_flags(&self) {
        let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
        guard.visible = *self.visible;
        guard.edit_mode = *self.edit_mode;
        guard.surface_size = self.surface.size();
        guard.surface_realloc_count = self.surface.realloc_count();
    }

    pub fn set_edit_mode(&mut self, enabled: bool) {
        *self.edit_mode = enabled;
        *self.click_through = !enabled;
        self.surface.set_click_through(!enabled);
        self.sync_shared_flags();
    }

    pub fn set_capture_visible(&mut self, visible: bool) {
        self.shared.capture_visible.store(visible, Ordering::Release);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn present_premultiplied_rgba(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        rgba: &[u8],
        global_alpha: u8,
        keep_position: bool,
    ) -> HudResult<()> {
        self.surface.upload_premultiplied_rgba(width, height, rgba)?;
        let (px, py) = if keep_position {
            (self.surface.pos_x, self.surface.pos_y)
        } else {
            (x, y)
        };
        let ct = *self.click_through;
        self.surface.present(self.hwnd, px, py, global_alpha, ct)?;
        *self.visible = true;
        self.sync_shared_flags();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn present_straight_rgba(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        rgba: &[u8],
        global_alpha: u8,
        keep_position: bool,
    ) -> HudResult<()> {
        self.surface.upload_straight_rgba(width, height, rgba)?;
        let (px, py) = if keep_position {
            (self.surface.pos_x, self.surface.pos_y)
        } else {
            (x, y)
        };
        let ct = *self.click_through;
        self.surface.present(self.hwnd, px, py, global_alpha, ct)?;
        *self.visible = true;
        self.sync_shared_flags();
        Ok(())
    }

    pub fn present_alpha(&mut self, global_alpha: u8) -> bool {
        let (px, py) = (self.surface.pos_x, self.surface.pos_y);
        let ct = *self.click_through;
        let ok = self
            .surface
            .present(self.hwnd, px, py, global_alpha, ct)
            .is_ok();
        *self.visible = true;
        self.sync_shared_flags();
        ok
    }

    pub fn present_position(&mut self, x: i32, y: i32, global_alpha: u8) -> bool {
        let (ox, oy) = self.screen_offset();
        let ct = *self.click_through;
        let ok = self
            .surface
            .present(self.hwnd, x + ox, y + oy, global_alpha, ct)
            .is_ok();
        *self.visible = true;
        self.sync_shared_flags();
        ok
    }

    pub fn hide(&mut self) {
        *self.visible = false;
        self.surface.hide_overlay();
        self.sync_shared_flags();
    }

    pub fn hide_and_release_surface(&mut self) {
        *self.visible = false;
        self.surface.release();
        self.sync_shared_flags();
    }

    pub fn set_timer(&self, id: usize, interval_ms: u32) {
        let dur = Duration::from_millis(interval_ms.max(10) as u64);
        self.timers.borrow_mut().insert(id, (dur, Instant::now() + dur));
    }

    pub fn kill_timer(&self, id: usize) {
        self.timers.borrow_mut().remove(&id);
    }

    pub fn request_close(&self) {
        self.should_quit.set(true);
    }
}

pub struct LayeredWindowHost {
    shared: Arc<HostShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LayeredWindowHost {
    pub fn new(config: LayeredWindowConfig) -> Self {
        let env_offscreen = is_hud_offscreen_env();
        let initial_offset = resolve_screen_offset(config.screen_offset, env_offscreen);
        let initial_cap = config.initial_capture_visible;

        Self {
            shared: Arc::new(HostShared {
                state: Mutex::new(HostMutable {
                    hwnd: 0,
                    closed: false,
                    ready_done: false,
                    visible: false,
                    edit_mode: false,
                    screen_offset: initial_offset,
                    surface_size: (0, 0),
                    surface_realloc_count: 0,
                    tx: None,
                }),
                ready_cv: Condvar::new(),
                capture_visible: AtomicBool::new(initial_cap),
                wda_refresh_count: AtomicUsize::new(0),
                config,
            }),
            thread: Mutex::new(None),
        }
    }

    pub fn ensure_started<H, F>(&self, make_handler: F) -> Option<isize>
    where
        H: LayeredHandler,
        F: FnOnce() -> H + Send + 'static,
    {
        {
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.closed {
                return None;
            }
            if guard.hwnd != 0 {
                return Some(guard.hwnd);
            }
        }

        let mut thread_guard = self.thread.lock().expect("layered thread lock poisoned");
        {
            let guard = self.shared.state.lock().expect("layered host lock poisoned");
            if guard.closed {
                return None;
            }
            if guard.hwnd != 0 {
                return Some(guard.hwnd);
            }
        }

        let shared = Arc::clone(&self.shared);
        let thread_name = self.shared.config.thread_name.to_owned();
        let (tx, rx) = mpsc::channel::<HostMsg>();
        let pseudo_hwnd = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed) as isize;

        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut handler = make_handler();
                let mut surface = LayeredDibSurface::with_namespace(format!(
                    "genshin-helper-{}",
                    shared.config.thread_name
                ));
                let mut visible = false;
                let mut dragging = false;
                let mut drag_anchor = (0.0f64, 0.0f64);
                let mut edit_mode = false;
                let mut click_through = true;
                let timers = RefCell::new(HashMap::<usize, (Duration, Instant)>::new());
                let should_quit = Cell::new(false);

                if shared.config.enable_wda {
                    let dur = Duration::from_millis(
                        shared.config.wda_refresh_ms.max(WDA_REFRESH_MS) as u64,
                    );
                    timers
                        .borrow_mut()
                        .insert(TIMER_ID_WDA_REFRESH, (dur, Instant::now() + dur));
                }

                {
                    let mut guard = shared.state.lock().expect("layered host lock poisoned");
                    guard.hwnd = pseudo_hwnd;
                    guard.tx = Some(tx);
                    guard.ready_done = true;
                    shared.ready_cv.notify_all();
                }

                while !should_quit.get() {
                    let now = Instant::now();
                    let next_wake = timers
                        .borrow()
                        .values()
                        .map(|(_, due)| *due)
                        .min()
                        .unwrap_or_else(|| now + Duration::from_millis(50));
                    let wait_dur = next_wake
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(50));

                    match rx.recv_timeout(wait_dur) {
                        Ok(HostMsg::Close) => {
                            should_quit.set(true);
                        }
                        Ok(HostMsg::App { msg, wparam, lparam }) => {
                            if msg == WM_APP_HOST_CLOSE {
                                should_quit.set(true);
                            } else {
                                let mut ctx = LayeredWindowCtx {
                                    hwnd: pseudo_hwnd,
                                    shared: &shared,
                                    surface: &mut surface,
                                    visible: &mut visible,
                                    dragging: &mut dragging,
                                    edit_mode: &mut edit_mode,
                                    click_through: &mut click_through,
                                    timers: &timers,
                                    should_quit: &should_quit,
                                };
                                let _ = handler.on_app_message(&mut ctx, msg, wparam, lparam);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            should_quit.set(true);
                        }
                    }

                    // Fire due timers
                    let now = Instant::now();
                    let due_ids: Vec<usize> = {
                        let mut t = timers.borrow_mut();
                        let ids: Vec<usize> = t
                            .iter()
                            .filter_map(|(&id, &(_, due))| (now >= due).then_some(id))
                            .collect();
                        for &id in &ids {
                            if let Some((dur, due)) = t.get_mut(&id) {
                                *due = now + *dur;
                            }
                        }
                        ids
                    };
                    for id in due_ids {
                        if id == TIMER_ID_WDA_REFRESH {
                            shared.wda_refresh_count.fetch_add(1, Ordering::Relaxed);
                        } else {
                            let mut ctx = LayeredWindowCtx {
                                hwnd: pseudo_hwnd,
                                shared: &shared,
                                surface: &mut surface,
                                visible: &mut visible,
                                dragging: &mut dragging,
                                edit_mode: &mut edit_mode,
                                click_through: &mut click_through,
                                timers: &timers,
                                should_quit: &should_quit,
                            };
                            handler.on_timer(&mut ctx, id);
                        }
                    }

                    // Pump Wayland overlay events (Configure / Closed / Edit-mode pointer)
                    let ptr_events = surface.pump_events();
                    if !ptr_events.is_empty() {
                        for ev in ptr_events {
                            match ev {
                                PointerEvent::Down { x, y } if edit_mode => {
                                    dragging = true;
                                    drag_anchor = (x, y);
                                    let mut ctx = LayeredWindowCtx {
                                        hwnd: pseudo_hwnd,
                                        shared: &shared,
                                        surface: &mut surface,
                                        visible: &mut visible,
                                        dragging: &mut dragging,
                                        edit_mode: &mut edit_mode,
                                        click_through: &mut click_through,
                                        timers: &timers,
                                        should_quit: &should_quit,
                                    };
                                    handler.on_drag_start(&mut ctx);
                                }
                                PointerEvent::Motion { x, y } if dragging && edit_mode => {
                                    let dx = (x - drag_anchor.0).round() as i32;
                                    let dy = (y - drag_anchor.1).round() as i32;
                                    if dx != 0 || dy != 0 {
                                        let new_x = (surface.pos_x + dx).max(0);
                                        let new_y = (surface.pos_y + dy).max(0);
                                        let rect = RectI32 {
                                            x: new_x,
                                            y: new_y,
                                            width: surface.width,
                                            height: surface.height,
                                        };
                                        let ga = surface.global_alpha;
                                        let _ = surface.present(pseudo_hwnd, new_x, new_y, ga, false);
                                        let mut ctx = LayeredWindowCtx {
                                            hwnd: pseudo_hwnd,
                                            shared: &shared,
                                            surface: &mut surface,
                                            visible: &mut visible,
                                            dragging: &mut dragging,
                                            edit_mode: &mut edit_mode,
                                            click_through: &mut click_through,
                                            timers: &timers,
                                            should_quit: &should_quit,
                                        };
                                        handler.on_moving(&mut ctx, rect);
                                    }
                                }
                                PointerEvent::Up if dragging => {
                                    dragging = false;
                                    let rect = RectI32 {
                                        x: surface.pos_x,
                                        y: surface.pos_y,
                                        width: surface.width,
                                        height: surface.height,
                                    };
                                    let mut ctx = LayeredWindowCtx {
                                        hwnd: pseudo_hwnd,
                                        shared: &shared,
                                        surface: &mut surface,
                                        visible: &mut visible,
                                        dragging: &mut dragging,
                                        edit_mode: &mut edit_mode,
                                        click_through: &mut click_through,
                                        timers: &timers,
                                        should_quit: &should_quit,
                                    };
                                    handler.on_drag_end(&mut ctx, rect);
                                }
                                PointerEvent::Wheel { delta } if edit_mode => {
                                    let mut ctx = LayeredWindowCtx {
                                        hwnd: pseudo_hwnd,
                                        shared: &shared,
                                        surface: &mut surface,
                                        visible: &mut visible,
                                        dragging: &mut dragging,
                                        edit_mode: &mut edit_mode,
                                        click_through: &mut click_through,
                                        timers: &timers,
                                        should_quit: &should_quit,
                                    };
                                    handler.on_wheel(&mut ctx, delta, false);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let mut ctx = LayeredWindowCtx {
                    hwnd: pseudo_hwnd,
                    shared: &shared,
                    surface: &mut surface,
                    visible: &mut visible,
                    dragging: &mut dragging,
                    edit_mode: &mut edit_mode,
                    click_through: &mut click_through,
                    timers: &timers,
                    should_quit: &should_quit,
                };
                handler.on_destroy(&mut ctx);
                surface.release();

                let mut guard = shared.state.lock().expect("layered host lock poisoned");
                guard.hwnd = 0;
                guard.tx = None;
                guard.visible = false;
            })
            .ok();

        if let Some(h) = handle {
            *thread_guard = Some(h);
            let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
            while !guard.ready_done && !guard.closed {
                guard = self.shared.ready_cv.wait(guard).expect("layered cv poisoned");
            }
            if guard.hwnd != 0 {
                return Some(guard.hwnd);
            }
        }
        None
    }

    pub fn post_message(&self, msg: u32, wparam: usize, lparam: isize) -> bool {
        let guard = self.shared.state.lock().expect("layered host lock poisoned");
        if let Some(tx) = &guard.tx {
            tx.send(HostMsg::App { msg, wparam, lparam }).is_ok()
        } else {
            false
        }
    }

    pub fn set_capture_visible(&self, visible: bool) -> bool {
        self.shared.capture_visible.store(visible, Ordering::Release);
        true
    }

    pub fn capture_visible(&self) -> bool {
        self.shared.capture_visible.load(Ordering::Acquire)
    }

    pub fn wda_refresh_count(&self) -> usize {
        self.shared.wda_refresh_count.load(Ordering::Relaxed)
    }

    pub fn raw_hwnd(&self) -> Option<isize> {
        let guard = self.shared.state.lock().expect("layered host lock poisoned");
        (guard.hwnd != 0).then_some(guard.hwnd)
    }

    pub fn is_visible(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .visible
    }

    pub fn is_edit_mode(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .edit_mode
    }

    pub fn is_thread_running(&self) -> bool {
        self.thread
            .lock()
            .expect("layered thread lock poisoned")
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    pub fn set_screen_offset(&self, offset: (i32, i32)) {
        let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
        guard.screen_offset = offset;
    }

    pub fn screen_offset(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .screen_offset
    }

    pub fn surface_size(&self) -> (i32, i32) {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .surface_size
    }

    pub fn surface_realloc_count(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("layered host lock poisoned")
            .surface_realloc_count
    }

    pub fn close(&self) {
        let tx = {
            let mut guard = self.shared.state.lock().expect("layered host lock poisoned");
            guard.closed = true;
            self.shared.ready_cv.notify_all();
            guard.tx.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(HostMsg::Close);
        }
        if let Some(h) = self
            .thread
            .lock()
            .expect("layered thread lock poisoned")
            .take()
        {
            let _ = h.join();
        }
    }
}

impl Drop for LayeredWindowHost {
    fn drop(&mut self) {
        self.close();
    }
}
