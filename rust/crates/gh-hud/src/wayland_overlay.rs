//! Native Wayland overlay window via `zwlr_layer_shell_v1`.
//!
//! Renders premultiplied BGRA8 surfaces (`wl_shm::Format::Argb8888`) at exact
//! `(x, y)` screen coordinates on `Layer::Overlay` with `ExclusiveZone(-1)`,
//! `KeyboardInteractivity::None`, and an empty `wl_region` input mask when
//! click-through (`!edit_mode`).

use crate::{HudError, HudResult};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::null_mut;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1,
};

const BTN_LEFT: u32 = 0x110;

#[derive(Debug, Clone)]
pub enum PointerEvent {
    Down { x: f64, y: f64 },
    Motion { x: f64, y: f64 },
    Up,
    Wheel { delta: i16 },
}

struct ShmSlot {
    width: i32,
    height: i32,
    size: usize,
    _fd: OwnedFd,
    ptr: *mut u8,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

impl ShmSlot {
    fn destroy(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        if !self.ptr.is_null() && self.size > 0 {
            // SAFETY: `self.ptr` was mapped via `mmap` with `self.size`.
            unsafe {
                libc::munmap(self.ptr.cast(), self.size);
            }
            self.ptr = null_mut();
            self.size = 0;
        }
    }
}

impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.destroy();
    }
}

struct WlState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    configured: bool,
    closed: bool,
    pointer_x: f64,
    pointer_y: f64,
    pointer_events: Vec<PointerEvent>,
    layer_surface_ref: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
}

// SAFETY: `WlState` is owned exclusively by a single `WaylandOverlay` on the host UI thread.
unsafe impl Send for WlState {}

impl WlState {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            seat: None,
            pointer: None,
            configured: false,
            closed: false,
            pointer_x: 0.0,
            pointer_y: 0.0,
            pointer_events: Vec::new(),
            layer_surface_ref: None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        ));
                }
                "wl_shm" => {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(
                        registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        ),
                    );
                }
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
                state.pointer_events.push(PointerEvent::Motion {
                    x: surface_x,
                    y: surface_y,
                });
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(btn_state),
                ..
            } if button == BTN_LEFT => match btn_state {
                wl_pointer::ButtonState::Pressed => {
                    state.pointer_events.push(PointerEvent::Down {
                        x: state.pointer_x,
                        y: state.pointer_y,
                    });
                }
                wl_pointer::ButtonState::Released => {
                    state.pointer_events.push(PointerEvent::Up);
                }
                _ => {}
            },
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } => {
                // Wayland vertical axis: negative = scroll up (positive Win32 wheel delta)
                let delta = (-value * 12.0).round() as i16;
                if delta != 0 {
                    state.pointer_events.push(PointerEvent::Wheel { delta });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for WlState {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer_surface.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(WlState: ignore wl_compositor::WlCompositor);
delegate_noop!(WlState: ignore wl_surface::WlSurface);
delegate_noop!(WlState: ignore wl_region::WlRegion);
delegate_noop!(WlState: ignore wl_shm::WlShm);
delegate_noop!(WlState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WlState: ignore wl_buffer::WlBuffer);
delegate_noop!(WlState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

fn create_memfd(size: usize) -> std::io::Result<OwnedFd> {
    let name = CString::new("gh-hud-overlay").expect("valid cstr");
    // SAFETY: Standard Linux `memfd_create` + `ftruncate`.
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Newly created valid fd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

pub struct WaylandOverlay {
    conn: Connection,
    queue: EventQueue<WlState>,
    state: WlState,
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    shm_slot: Option<ShmSlot>,
    last_rect: Option<(i32, i32, i32, i32)>,
    last_click_through: Option<bool>,
    mapped: bool,
}

impl WaylandOverlay {
    pub fn try_new(namespace: &str) -> Option<Self> {
        std::env::var_os("WAYLAND_DISPLAY")?;
        let conn = Connection::connect_to_env().ok()?;
        let display = conn.display();
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = WlState::new();
        let _registry = display.get_registry(&qh, ());

        if queue.roundtrip(&mut state).is_err() {
            return None;
        }
        // Second roundtrip to receive wl_seat capabilities
        let _ = queue.roundtrip(&mut state);

        let compositor = state.compositor.as_ref()?.clone();
        let layer_shell = state.layer_shell.as_ref()?.clone();
        let _shm = state.shm.as_ref()?;

        let surface = compositor.create_surface(&qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            namespace.to_owned(),
            &qh,
            (),
        );
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left,
        );
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(
            zwlr_layer_surface_v1::KeyboardInteractivity::None,
        );
        layer_surface.set_size(1, 1);

        // Start click-through by default
        let empty_region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&empty_region));
        empty_region.destroy();

        surface.commit();
        if queue.roundtrip(&mut state).is_err() {
            layer_surface.destroy();
            surface.destroy();
            return None;
        }
        state.layer_surface_ref = Some(layer_surface.clone());

        Some(Self {
            conn,
            queue,
            state,
            surface,
            layer_surface,
            shm_slot: None,
            last_rect: None,
            last_click_through: Some(true),
            mapped: false,
        })
    }

    pub fn set_click_through(&mut self, click_through: bool) {
        if self.last_click_through == Some(click_through) {
            return;
        }
        if click_through {
            if let Some(ref compositor) = self.state.compositor {
                let qh = self.queue.handle();
                let empty = compositor.create_region(&qh, ());
                self.surface.set_input_region(Some(&empty));
                empty.destroy();
            }
        } else {
            self.surface.set_input_region(None);
        }
        self.last_click_through = Some(click_through);
        self.surface.commit();
        let _ = self.conn.flush();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        bgra: &[u8],
        global_alpha: u8,
        click_through: bool,
    ) -> HudResult<()> {
        if width <= 0 || height <= 0 {
            return Ok(());
        }
        let needed = (width as usize) * (height as usize) * 4;
        if bgra.len() < needed {
            return Err(HudError::InvalidBuffer("bgra buffer too small".into()));
        }

        self.set_click_through(click_through);

        if self.last_rect != Some((x, y, width, height)) {
            self.layer_surface.set_size(width as u32, height as u32);
            self.layer_surface.set_margin(y.max(0), 0, 0, x.max(0));
            self.last_rect = Some((x, y, width, height));
        }

        // Per zwlr_layer_shell_v1 protocol, a layer surface must be configured
        // (via an unbuffered commit + Configure + ack_configure) before any buffer
        // is attached — both on first show and after hide() unmapped it.
        if !self.state.configured {
            self.surface.commit();
            for _ in 0..3 {
                if self.queue.roundtrip(&mut self.state).is_err() {
                    return Err(HudError::InvalidBuffer(
                        "wayland roundtrip failed during configure".into(),
                    ));
                }
                if self.state.configured || self.state.closed {
                    break;
                }
            }
            if !self.state.configured || self.state.closed {
                return Ok(());
            }
        }

        let need_new_slot = self
            .shm_slot
            .as_ref()
            .is_none_or(|s| s.width != width || s.height != height);
        if need_new_slot {
            self.shm_slot = None;
            let shm = self
                .state
                .shm
                .as_ref()
                .ok_or_else(|| HudError::InvalidBuffer("missing wl_shm".into()))?;
            let fd = create_memfd(needed).map_err(|e| {
                HudError::InvalidBuffer(format!("memfd_create failed: {e}"))
            })?;
            // SAFETY: Valid memfd of `needed` bytes.
            let ptr = unsafe {
                libc::mmap(
                    null_mut(),
                    needed,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(HudError::InvalidBuffer("mmap failed".into()));
            }
            let qh = self.queue.handle();
            let pool = shm.create_pool(fd.as_fd(), needed as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                width,
                height,
                width * 4,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            );
            self.shm_slot = Some(ShmSlot {
                width,
                height,
                size: needed,
                _fd: fd,
                ptr: ptr.cast(),
                pool,
                buffer,
            });
        }

        if let Some(ref slot) = self.shm_slot {
            // SAFETY: `slot.ptr` is mapped for `slot.size >= needed` bytes.
            let dst = unsafe { std::slice::from_raw_parts_mut(slot.ptr, needed) };
            if global_alpha == 255 {
                dst.copy_from_slice(&bgra[..needed]);
            } else {
                let ga = global_alpha as u16;
                for (d, &s) in dst.iter_mut().zip(&bgra[..needed]) {
                    *d = ((s as u16 * ga + 127) / 255) as u8;
                }
            }
            self.surface.attach(Some(&slot.buffer), 0, 0);
            self.surface.damage_buffer(0, 0, width, height);
            self.surface.commit();
            self.mapped = true;
            let _ = self.conn.flush();
        }

        let _ = self.queue.dispatch_pending(&mut self.state);
        Ok(())
    }

    pub fn hide(&mut self) {
        if self.mapped {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
            let _ = self.conn.flush();
            self.mapped = false;
            self.state.configured = false;
        }
    }

    pub fn pump_events(&mut self) -> Vec<PointerEvent> {
        let _ = self.conn.flush();
        if let Some(guard) = self.queue.prepare_read() {
            let fd = self.conn.as_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: Valid `pollfd` on stack, 0ms non-blocking poll.
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
        std::mem::take(&mut self.state.pointer_events)
    }
}

impl Drop for WaylandOverlay {
    fn drop(&mut self) {
        self.hide();
        self.shm_slot = None;
        self.layer_surface.destroy();
        self.surface.destroy();
        let _ = self.conn.flush();
    }
}
