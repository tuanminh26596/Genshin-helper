//! Native Wayland screen region capture via `zwlr_screencopy_manager_v1`.
//!
//! Works on wlroots-based Wayland compositors (Hyprland, Sway, River, Wayfire, Labwc)
//! with zero user prompts and sub-millisecond region-only SHM copy (`capture_output_region`).

use gh_core::frame::{FrameBuf, PixelFormat};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::null_mut;
use std::sync::{Mutex, OnceLock};
use tracing::{debug, warn};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

#[derive(Debug, Clone)]
struct OutputInfo {
    global_name: u32,
    output: wl_output::WlOutput,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy)]
struct BufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct WaylandState {
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    outputs: Vec<OutputInfo>,
    // Per-capture transient state
    buffer_spec: Option<BufferSpec>,
    y_invert: bool,
    copy_state: CopyState,
    pending_buffer: Option<wl_buffer::WlBuffer>,
    pending_pool: Option<wl_shm_pool::WlShmPool>,
    shm_ptr: *mut u8,
    shm_len: usize,
    shm_fd: Option<OwnedFd>,
}

// SAFETY: WaylandState is only ever accessed behind a Mutex on a single thread at a time.
unsafe impl Send for WaylandState {}

impl WaylandState {
    fn new() -> Self {
        Self {
            shm: None,
            screencopy: None,
            outputs: Vec::new(),
            buffer_spec: None,
            y_invert: false,
            copy_state: CopyState::Pending,
            pending_buffer: None,
            pending_pool: None,
            shm_ptr: null_mut(),
            shm_len: 0,
            shm_fd: None,
        }
    }

    fn cleanup_capture(&mut self) {
        if let Some(buf) = self.pending_buffer.take() {
            buf.destroy();
        }
        if let Some(pool) = self.pending_pool.take() {
            pool.destroy();
        }
        if !self.shm_ptr.is_null() && self.shm_len > 0 {
            // SAFETY: shm_ptr was mapped with mmap of size shm_len.
            unsafe {
                libc::munmap(self.shm_ptr.cast(), self.shm_len);
            }
            self.shm_ptr = null_mut();
            self.shm_len = 0;
        }
        self.shm_fd = None;
        self.buffer_spec = None;
        self.y_invert = false;
        self.copy_state = CopyState::Pending;
    }
}

impl Drop for WaylandState {
    fn drop(&mut self) {
        self.cleanup_capture();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    let v = version.min(3);
                    state.screencopy = Some(
                        registry.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                            name,
                            v,
                            qh,
                            (),
                        ),
                    );
                }
                "wl_output" => {
                    let v = version.min(4);
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, v, qh, name);
                    state.outputs.push(OutputInfo {
                        global_name: name,
                        output,
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        scale: 1,
                    });
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.global_name != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(out) = state.outputs.iter_mut().find(|o| o.global_name == *global_name) {
            match event {
                wl_output::Event::Geometry { x, y, .. } => {
                    out.x = x;
                    out.y = y;
                }
                wl_output::Event::Mode {
                    flags,
                    width,
                    height,
                    ..
                } => {
                    if let wayland_client::WEnum::Value(f) = flags
                        && f.contains(wl_output::Mode::Current)
                    {
                        out.width = width;
                        out.height = height;
                    }
                }
                wl_output::Event::Scale { factor } => {
                    out.scale = factor.max(1);
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(WaylandState: ignore wl_shm::WlShm);
delegate_noop!(WaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandState: ignore wl_buffer::WlBuffer);
delegate_noop!(WaylandState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let wayland_client::WEnum::Value(fmt) = format else {
                    state.copy_state = CopyState::Failed;
                    return;
                };
                state.buffer_spec = Some(BufferSpec {
                    format: fmt,
                    width,
                    height,
                    stride,
                });

                // Allocate SHM buffer and start copy immediately (v1/v2 compatibility)
                let Some(shm) = state.shm.as_ref() else {
                    state.copy_state = CopyState::Failed;
                    return;
                };
                let size = (stride as usize) * (height as usize);
                if size == 0 {
                    state.copy_state = CopyState::Failed;
                    return;
                }
                let Ok(fd) = create_memfd(size) else {
                    state.copy_state = CopyState::Failed;
                    return;
                };
                // SAFETY: Valid memfd of `size` bytes.
                let ptr = unsafe {
                    libc::mmap(
                        null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd.as_raw_fd(),
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    state.copy_state = CopyState::Failed;
                    return;
                }

                use std::os::fd::AsFd;
                let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
                let buffer =
                    pool.create_buffer(0, width as i32, height as i32, stride as i32, fmt, qh, ());
                frame.copy(&buffer);

                state.shm_ptr = ptr.cast();
                state.shm_len = size;
                state.shm_fd = Some(fd);
                state.pending_pool = Some(pool);
                state.pending_buffer = Some(buffer);
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let wayland_client::WEnum::Value(f) = flags {
                    state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.copy_state = CopyState::Ready;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.copy_state = CopyState::Failed;
            }
            _ => {}
        }
    }
}

fn create_memfd(size: usize) -> std::io::Result<OwnedFd> {
    let name = CString::new("gh-screencopy").expect("valid cstr");
    // SAFETY: Standard Linux memfd_create + ftruncate.
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

struct WaylandCapturerInner {
    conn: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
}

static WAYLAND_CAPTURER: OnceLock<Option<Mutex<WaylandCapturerInner>>> = OnceLock::new();

fn get_wayland_capturer() -> Option<&'static Mutex<WaylandCapturerInner>> {
    WAYLAND_CAPTURER
        .get_or_init(|| {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() {
                return None;
            }
            let conn = match Connection::connect_to_env() {
                Ok(c) => c,
                Err(e) => {
                    debug!("gh-linux: Wayland connection unavailable: {e}");
                    return None;
                }
            };
            let display = conn.display();
            let mut queue = conn.new_event_queue();
            let qh = queue.handle();
            let mut state = WaylandState::new();
            let _registry = display.get_registry(&qh, ());

            // Roundtrip twice: first for globals, second for wl_output geometry/mode events
            if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
                return None;
            }

            if state.screencopy.is_none() || state.shm.is_none() || state.outputs.is_empty() {
                debug!(
                    "gh-linux: Wayland compositor lacks zwlr_screencopy_manager_v1 or outputs"
                );
                return None;
            }

            Some(Mutex::new(WaylandCapturerInner { conn, queue, state }))
        })
        .as_ref()
}

pub fn is_available() -> bool {
    get_wayland_capturer().is_some()
}

pub fn screen_bounds() -> Option<(i32, i32, i32, i32)> {
    let lock = get_wayland_capturer()?;
    let guard = lock.lock().ok()?;
    let out = guard.state.outputs.first()?;
    Some((out.x, out.y, out.width, out.height))
}

/// Captures an absolute-coordinate screen region `(x, y, width, height)` using
/// `zwlr_screencopy_manager_v1::capture_output_region`, returning `PixelFormat::Bgr8`
/// or `PixelFormat::Bgra8`.
pub fn capture_region(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    format: PixelFormat,
) -> Option<FrameBuf> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let lock = get_wayland_capturer()?;
    let mut guard = lock.lock().ok()?;
    let WaylandCapturerInner { conn: _, queue, state } = &mut *guard;

    let screencopy = state.screencopy.as_ref()?.clone();
    // Pick output containing (x, y), or fallback to first output
    let output = state
        .outputs
        .iter()
        .find(|o| x >= o.x && x < o.x + o.width && y >= o.y && y < o.y + o.height)
        .or_else(|| state.outputs.first())?
        .clone();

    let rel_x = x - output.x;
    let rel_y = y - output.y;
    let x0 = rel_x.max(0);
    let y0 = rel_y.max(0);
    let x1 = (rel_x + width).min(output.width);
    let y1 = (rel_y + height).min(output.height);
    let cw = x1 - x0;
    let ch = y1 - y0;
    if cw <= 0 || ch <= 0 {
        return None;
    }

    state.cleanup_capture();
    let qh = queue.handle();
    let frame = screencopy.capture_output_region(
        0,
        &output.output,
        x0,
        y0,
        cw,
        ch,
        &qh,
        (),
    );

    // Pump events until Ready or Failed
    while state.copy_state == CopyState::Pending {
        if queue.blocking_dispatch(state).is_err() {
            state.cleanup_capture();
            frame.destroy();
            return None;
        }
    }

    let result = if state.copy_state == CopyState::Ready
        && let Some(spec) = state.buffer_spec
        && !state.shm_ptr.is_null()
    {
        // SAFETY: shm_ptr is mapped for shm_len bytes and Ready guarantees compositor finished writing.
        let raw = unsafe { std::slice::from_raw_parts(state.shm_ptr, state.shm_len) };
        convert_shm_to_frame(raw, spec, state.y_invert, format)
    } else {
        None
    };

    state.cleanup_capture();
    frame.destroy();
    result
}

fn convert_shm_to_frame(
    raw: &[u8],
    spec: BufferSpec,
    y_invert: bool,
    target_format: PixelFormat,
) -> Option<FrameBuf> {
    let w = spec.width as usize;
    let h = spec.height as usize;
    let src_stride = spec.stride as usize;
    if raw.len() < src_stride * h {
        return None;
    }

    let swap_rb = matches!(
        spec.format,
        wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888
    );

    let out_channels = match target_format {
        PixelFormat::Bgr8 => 3,
        PixelFormat::Bgra8 => 4,
        _ => return None,
    };
    let dst_stride = w * out_channels;
    let mut data = vec![0u8; dst_stride * h];

    for row in 0..h {
        let src_row = if y_invert { h - 1 - row } else { row };
        let s_row = &raw[src_row * src_stride..src_row * src_stride + w * 4];
        let d_row = &mut data[row * dst_stride..row * dst_stride + dst_stride];
        for col in 0..w {
            let s = &s_row[col * 4..col * 4 + 4];
            let (b, g, r, a) = if swap_rb {
                (s[2], s[1], s[0], s[3])
            } else {
                (s[0], s[1], s[2], s[3])
            };
            let d = &mut d_row[col * out_channels..col * out_channels + out_channels];
            d[0] = b;
            d[1] = g;
            d[2] = r;
            if out_channels == 4 {
                d[3] = a;
            }
        }
    }

    Some(FrameBuf {
        width: spec.width,
        height: spec.height,
        stride: dst_stride,
        format: target_format,
        data,
    })
}
