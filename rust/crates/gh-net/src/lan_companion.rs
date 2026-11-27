//! Opt-in LAN Companion (`0.0.0.0:<port>`, default 8766): PIN auth with 429 rate-limit,
//! constant-time token checks, RFC 6455 WebSocket (15 Hz coalesce), pure-Rust ISO 18004 QR SVG.
//! Ported from `src/lan_companion.py`.

use crate::hd_assets::{DEFAULT_USER_AGENT, UndergroundOverlayProvider};
use crate::pin_guide::{HoYoLABLabelCatalog, MarkerUiAsset, PinGuideFetcher, PinIconCache};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    REFERRER_POLICY, UPGRADE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use gh_core::atomic_io;
use gh_core::multimap::{DEFAULT_TILE_BASE, MultimapDb};
use gh_core::traits::UndergroundOverlaySource;
use gh_core::{CoreError, CoreResult, DEFAULT_LAN_PORT};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
#[cfg(any(windows, test))]
use std::net::Ipv4Addr;
#[cfg(not(windows))]
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, oneshot, watch};

pub const MAX_HTTP_BODY_BYTES: usize = 32 * 1024;
pub const MAX_WS_PAYLOAD_BYTES: usize = 64 * 1024;
pub const SLOW_CLIENT_BUFFER_LIMIT_BYTES: usize = 256 * 1024;
const MAX_REMOTE_TILE_BYTES: usize = 1_000_000;
pub const PIN_MAX_FAILURES: usize = 5;
pub const PIN_WINDOW_SECS: f64 = 60.0;

const CSP_PIN_LOGIN: &str =
    "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'";
const CSP_COMPANION: &str = "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:";

// ---------------------------------------------------------------------------
// Pure-Rust Offline QR Code Generator (ISO/IEC 18004, Byte mode, ECC-L, V1-6)
// ---------------------------------------------------------------------------

struct QrSpec {
    version: usize,
    size: usize,
    max_bytes: usize,
    total_data_cw: usize,
    ec_cw_per_block: usize,
    num_blocks: usize,
    align_centers: &'static [usize],
}

const QR_SPECS_L: [QrSpec; 6] = [
    QrSpec {
        version: 1,
        size: 21,
        max_bytes: 17,
        total_data_cw: 19,
        ec_cw_per_block: 7,
        num_blocks: 1,
        align_centers: &[],
    },
    QrSpec {
        version: 2,
        size: 25,
        max_bytes: 32,
        total_data_cw: 34,
        ec_cw_per_block: 10,
        num_blocks: 1,
        align_centers: &[6, 18],
    },
    QrSpec {
        version: 3,
        size: 29,
        max_bytes: 53,
        total_data_cw: 55,
        ec_cw_per_block: 15,
        num_blocks: 1,
        align_centers: &[6, 22],
    },
    QrSpec {
        version: 4,
        size: 33,
        max_bytes: 78,
        total_data_cw: 80,
        ec_cw_per_block: 20,
        num_blocks: 1,
        align_centers: &[6, 26],
    },
    QrSpec {
        version: 5,
        size: 37,
        max_bytes: 106,
        total_data_cw: 108,
        ec_cw_per_block: 26,
        num_blocks: 1,
        align_centers: &[6, 30],
    },
    QrSpec {
        version: 6,
        size: 41,
        max_bytes: 134,
        total_data_cw: 136,
        ec_cw_per_block: 18,
        num_blocks: 2,
        align_centers: &[6, 34],
    },
];

const fn init_gf256() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if (x & 0x100) != 0 {
            x ^= 0x11D;
        }
        i += 1;
    }
    let mut j = 255;
    while j < 512 {
        exp[j] = exp[j - 255];
        j += 1;
    }
    (exp, log)
}

const GF_TABLES: ([u8; 512], [u8; 256]) = init_gf256();
const GF_EXP: &[u8; 512] = &GF_TABLES.0;
const GF_LOG: &[u8; 256] = &GF_TABLES.1;

#[inline]
fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    GF_EXP[(GF_LOG[a as usize] as usize) + (GF_LOG[b as usize] as usize)]
}

fn rs_generator_poly(degree: usize) -> Vec<u8> {
    let mut poly = vec![1u8];
    for &factor in GF_EXP.iter().take(degree) {
        let mut nxt = vec![0u8; poly.len() + 1];
        for (j, &coef) in poly.iter().enumerate() {
            nxt[j] ^= coef;
            nxt[j + 1] ^= gf_mul(coef, factor);
        }
        poly = nxt;
    }
    poly
}

fn rs_encode(data: &[u8], ec_len: usize) -> Vec<u8> {
    let poly_gen = rs_generator_poly(ec_len);
    let mut rem = Vec::with_capacity(data.len() + ec_len);
    rem.extend_from_slice(data);
    rem.resize(data.len() + ec_len, 0);
    for i in 0..data.len() {
        let coef = rem[i];
        if coef != 0 {
            for j in 1..poly_gen.len() {
                rem[i + j] ^= gf_mul(poly_gen[j], coef);
            }
        }
    }
    rem[data.len()..].to_vec()
}

#[inline]
fn mask_bit(mask_id: usize, r: usize, c: usize) -> bool {
    match mask_id {
        0 => (r + c).is_multiple_of(2),
        1 => r.is_multiple_of(2),
        2 => c.is_multiple_of(3),
        3 => (r + c).is_multiple_of(3),
        4 => ((r / 2) + (c / 3)).is_multiple_of(2),
        5 => ((r * c) % 2) + ((r * c) % 3) == 0,
        6 => (((r * c) % 2) + ((r * c) % 3)).is_multiple_of(2),
        _ => (((r + c) % 2) + ((r * c) % 3)).is_multiple_of(2),
    }
}

fn compute_format_bits_l(mask_id: usize) -> u16 {
    let data: u16 = (0b01 << 3) | ((mask_id as u16) & 0x7);
    let mut rem: u16 = data << 10;
    for bit in (10..=14).rev() {
        if ((rem >> bit) & 1) != 0 {
            rem ^= 0x537 << (bit - 10);
        }
    }
    ((data << 10) | rem) ^ 0x5412
}

fn place_format_bits(grid: &mut [Vec<bool>], mask_id: usize) {
    let n = grid.len();
    let fmt = compute_format_bits_l(mask_id);
    let bit = |idx: u32| -> bool { ((fmt >> idx) & 1) != 0 };

    let tl_cols = [0usize, 1, 2, 3, 4, 5, 7, 8];
    for (k, &c) in tl_cols.iter().enumerate() {
        grid[8][c] = bit(14 - k as u32);
    }
    let tl_rows = [7usize, 5, 4, 3, 2, 1, 0];
    for (k, &r) in tl_rows.iter().enumerate() {
        grid[r][8] = bit(6 - k as u32);
    }

    for k in 0..7usize {
        grid[n - 1 - k][8] = bit(14 - k as u32);
    }
    for k in 0..8usize {
        grid[8][n - 8 + k] = bit(7 - k as u32);
    }
}

fn qr_penalty(grid: &[Vec<bool>]) -> i32 {
    let n = grid.len();
    let mut penalty: i32 = 0;

    // Rule 1: 5+ consecutive same-color modules in rows and columns
    for row in grid.iter().take(n) {
        let mut run_len = 1i32;
        for c in 1..n {
            if row[c] == row[c - 1] {
                run_len += 1;
            } else {
                if run_len >= 5 {
                    penalty += 3 + (run_len - 5);
                }
                run_len = 1;
            }
        }
        if run_len >= 5 {
            penalty += 3 + (run_len - 5);
        }
    }

    for c in 0..n {
        let mut run_len = 1i32;
        for r in 1..n {
            if grid[r][c] == grid[r - 1][c] {
                run_len += 1;
            } else {
                if run_len >= 5 {
                    penalty += 3 + (run_len - 5);
                }
                run_len = 1;
            }
        }
        if run_len >= 5 {
            penalty += 3 + (run_len - 5);
        }
    }

    // Rule 2: 2x2 same-color blocks
    for r in 0..n.saturating_sub(1) {
        for c in 0..n.saturating_sub(1) {
            let v = grid[r][c];
            if v == grid[r][c + 1] && v == grid[r + 1][c] && v == grid[r + 1][c + 1] {
                penalty += 3;
            }
        }
    }

    // Rule 3: finder-like 1:1:3:1:1 patterns with 4 light modules on either side
    const PAT_A: [bool; 11] = [
        true, false, true, true, true, false, true, false, false, false, false,
    ];
    const PAT_B: [bool; 11] = [
        false, false, false, false, true, false, true, true, true, false, true,
    ];
    if n >= 11 {
        for row in grid.iter().take(n) {
            for c in 0..=(n - 11) {
                let seg = &row[c..c + 11];
                if seg == PAT_A || seg == PAT_B {
                    penalty += 40;
                }
            }
        }
        let mut col = Vec::with_capacity(n);
        for c in 0..n {
            col.clear();
            col.extend(grid.iter().map(|row| row[c]));
            for r in 0..=(n - 11) {
                let seg = &col[r..r + 11];
                if seg == PAT_A || seg == PAT_B {
                    penalty += 40;
                }
            }
        }
    }

    // Rule 4: dark module balance around 50%
    let dark: usize = grid
        .iter()
        .flat_map(|row| row.iter())
        .filter(|&&cell| cell)
        .count();
    let total = (n * n) as i64;
    let diff = (dark as i64 * 20 - total * 10).abs();
    let k = (diff / total) as i32;
    penalty += k * 10;
    penalty
}

/// Encodes UTF-8 text into an ISO/IEC 18004 QR bit matrix (Versions 1-6, Byte mode, ECC-L).
pub fn generate_qr_matrix(data: &str) -> CoreResult<Vec<Vec<bool>>> {
    let payload = data.as_bytes();
    let Some(spec) = QR_SPECS_L.iter().find(|s| payload.len() <= s.max_bytes) else {
        return Err(CoreError::InvalidArgument(format!(
            "QR payload too long ({} bytes; max supported is {} bytes)",
            payload.len(),
            QR_SPECS_L[QR_SPECS_L.len() - 1].max_bytes
        )));
    };

    let version = spec.version;
    let n = spec.size;
    let total_data_cw = spec.total_data_cw;
    let ec_cw_per_block = spec.ec_cw_per_block;
    let num_blocks = spec.num_blocks;

    // 1. Byte-mode bit stream: mode (0100) + 8-bit count + payload bytes
    let mut bits: Vec<u8> = Vec::with_capacity(12 + payload.len() * 8 + 12);
    bits.extend_from_slice(&[0, 1, 0, 0]);
    let length = payload.len() as u8;
    for shift in (0..8).rev() {
        bits.push((length >> shift) & 1);
    }
    for &b in payload {
        for shift in (0..8).rev() {
            bits.push((b >> shift) & 1);
        }
    }

    let max_data_bits = total_data_cw * 8;
    let terminator = 4.min(max_data_bits.saturating_sub(bits.len()));
    bits.resize(bits.len() + terminator, 0);
    while !bits.len().is_multiple_of(8) {
        bits.push(0);
    }

    let mut data_cw: Vec<u8> = Vec::with_capacity(total_data_cw);
    let (byte_chunks, _) = bits.as_chunks::<8>();
    for chunk in byte_chunks {
        let mut val = 0u8;
        for &b in chunk {
            val = (val << 1) | b;
        }
        data_cw.push(val);
    }

    const PAD_BYTES: [u8; 2] = [0xEC, 0x11];
    let mut pad_idx = 0usize;
    while data_cw.len() < total_data_cw {
        data_cw.push(PAD_BYTES[pad_idx]);
        pad_idx ^= 1;
    }

    // 2. Reed-Solomon blocks & interleave
    let block_size = total_data_cw / num_blocks;
    let mut data_blocks: Vec<&[u8]> = Vec::with_capacity(num_blocks);
    let mut ec_blocks: Vec<Vec<u8>> = Vec::with_capacity(num_blocks);
    for b_idx in 0..num_blocks {
        let blk = &data_cw[b_idx * block_size..(b_idx + 1) * block_size];
        data_blocks.push(blk);
        ec_blocks.push(rs_encode(blk, ec_cw_per_block));
    }

    let mut interleaved: Vec<u8> = Vec::with_capacity(total_data_cw + num_blocks * ec_cw_per_block);
    for i in 0..block_size {
        for blk in &data_blocks {
            interleaved.push(blk[i]);
        }
    }
    for i in 0..ec_cw_per_block {
        for blk in &ec_blocks {
            interleaved.push(blk[i]);
        }
    }

    let mut codeword_bits: Vec<bool> = Vec::with_capacity(interleaved.len() * 8);
    for cw in interleaved {
        for shift in (0..8).rev() {
            codeword_bits.push(((cw >> shift) & 1) != 0);
        }
    }

    // 3. Allocate matrix & function-module mask
    let mut base = vec![vec![false; n]; n];
    let mut is_func = vec![vec![false; n]; n];

    let set_func = |base: &mut [Vec<bool>],
                    is_func: &mut [Vec<bool>],
                    r: isize,
                    c: isize,
                    val: bool| {
        if r >= 0 && (r as usize) < n && c >= 0 && (c as usize) < n {
            let ru = r as usize;
            let cu = c as usize;
            base[ru][cu] = val;
            is_func[ru][cu] = true;
        }
    };

    let draw_finder = |base: &mut [Vec<bool>], is_func: &mut [Vec<bool>], top: isize, left: isize| {
        for dr in -1..=7isize {
            for dc in -1..=7isize {
                let r = top + dr;
                let c = left + dc;
                let in_outer = (0..=6).contains(&dr) && (0..=6).contains(&dc);
                let on_border = dr == 0 || dr == 6 || dc == 0 || dc == 6;
                let in_center = (2..=4).contains(&dr) && (2..=4).contains(&dc);
                set_func(base, is_func, r, c, in_outer && (on_border || in_center));
            }
        }
    };

    draw_finder(&mut base, &mut is_func, 0, 0);
    draw_finder(&mut base, &mut is_func, 0, (n - 7) as isize);
    draw_finder(&mut base, &mut is_func, (n - 7) as isize, 0);

    // Alignment patterns
    for &ar in spec.align_centers {
        for &ac in spec.align_centers {
            if is_func[ar][ac] {
                continue;
            }
            for dr in -2..=2isize {
                for dc in -2..=2isize {
                    let dark = matches!(dr.abs().max(dc.abs()), 0 | 2);
                    set_func(
                        &mut base,
                        &mut is_func,
                        ar as isize + dr,
                        ac as isize + dc,
                        dark,
                    );
                }
            }
        }
    }

    // Timing patterns
    for i in 8..(n - 8) {
        if !is_func[6][i] {
            set_func(&mut base, &mut is_func, 6, i as isize, i.is_multiple_of(2));
        }
        if !is_func[i][6] {
            set_func(&mut base, &mut is_func, i as isize, 6, i.is_multiple_of(2));
        }
    }

    // Dark module (4 * version + 9, 8)
    set_func(&mut base, &mut is_func, (4 * version + 9) as isize, 8, true);

    // Reserve format info modules
    for c in [0usize, 1, 2, 3, 4, 5, 7, 8] {
        is_func[8][c] = true;
    }
    for r in [7usize, 5, 4, 3, 2, 1, 0] {
        is_func[r][8] = true;
    }
    for k in 0..7usize {
        is_func[n - 1 - k][8] = true;
    }
    for k in 0..8usize {
        is_func[8][n - 8 + k] = true;
    }

    // 4. 2-column zigzag bit placement
    let mut bit_idx = 0usize;
    let total_bits = codeword_bits.len();
    let mut col = (n - 1) as isize;
    let mut upward = true;
    while col > 0 {
        if col == 6 {
            col -= 1;
        }
        for step in 0..n {
            let r = if upward { n - 1 - step } else { step };
            for dc in [0isize, -1] {
                let c = (col + dc) as usize;
                if !is_func[r][c] {
                    if bit_idx < total_bits {
                        base[r][c] = codeword_bits[bit_idx];
                        bit_idx += 1;
                    } else {
                        base[r][c] = false;
                    }
                }
            }
        }
        upward = !upward;
        col -= 2;
    }

    // 5. Evaluate 8 masks and choose lowest penalty
    let mut best_penalty = i32::MAX;
    let mut best_grid: Option<Vec<Vec<bool>>> = None;
    for mask_id in 0..8usize {
        let mut cand = base.clone();
        for r in 0..n {
            for c in 0..n {
                if !is_func[r][c] && mask_bit(mask_id, r, c) {
                    cand[r][c] = !cand[r][c];
                }
            }
        }
        place_format_bits(&mut cand, mask_id);
        let score = qr_penalty(&cand);
        if score < best_penalty {
            best_penalty = score;
            best_grid = Some(cand);
        }
    }

    Ok(best_grid.expect("at least one mask evaluated"))
}

pub fn qr_matrix_to_svg(
    matrix: &[Vec<bool>],
    quiet_zone: usize,
    module_px: usize,
    fg: &str,
    bg: &str,
) -> String {
    let n = matrix.len();
    let total_modules = n + 2 * quiet_zone;
    let px = module_px.max(1);
    let size_px = total_modules * px;

    let mut path_d = String::new();
    for (r, row) in matrix.iter().enumerate() {
        let mut c = 0usize;
        while c < n {
            if row[c] {
                let start = c;
                while c < n && row[c] {
                    c += 1;
                }
                let run = c - start;
                use std::fmt::Write;
                let _ = write!(
                    path_d,
                    "M{} {}h{}v1h-{}z",
                    start + quiet_zone,
                    r + quiet_zone,
                    run,
                    run
                );
            } else {
                c += 1;
            }
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size_px}\" height=\"{size_px}\" \
viewBox=\"0 0 {total_modules} {total_modules}\" shape-rendering=\"crispEdges\" \
role=\"img\" aria-label=\"LAN Companion QR\">\
<rect width=\"{total_modules}\" height=\"{total_modules}\" fill=\"{bg}\"/>\
<path d=\"{path_d}\" fill=\"{fg}\"/></svg>"
    )
}

pub fn generate_qr_svg(data: &str) -> CoreResult<String> {
    let matrix = generate_qr_matrix(data)?;
    Ok(qr_matrix_to_svg(&matrix, 4, 6, "#111111", "#ffffff"))
}

// ---------------------------------------------------------------------------
// Primary LAN IPv4 Detection (offline routing-table probe, zero packets sent)
// ---------------------------------------------------------------------------

/// Resolves the LAN Companion TCP bind host, keeping production wildcard `"0.0.0.0"` unless
/// an explicit non-empty override (e.g. `GH_LAN_BIND_HOST="127.0.0.1"` in tests) is provided.
pub fn resolve_lan_bind_host(override_val: Option<&str>) -> String {
    match override_val.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => "0.0.0.0".to_owned(),
    }
}

fn is_usable_lan_ipv4(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !v4.is_multicast()
        }
        IpAddr::V6(_) => false,
    }
}

#[cfg(windows)]
fn load_windows_ipv4_rows() -> Vec<(u32, Ipv4Addr)> {
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetIpAddrTable, MIB_IPADDR_DELETED, MIB_IPADDR_DISCONNECTED, MIB_IPADDRROW_XP,
        MIB_IPADDRTABLE,
    };

    let mut size: u32 = 0;
    // SAFETY: `size = 0` queries the required byte length for `MIB_IPADDRTABLE`.
    unsafe {
        let _ = GetIpAddrTable(None, &mut size, false);
    }
    if size == 0 {
        return Vec::new();
    }

    for _ in 0..3 {
        let words = (size as usize)
            .div_ceil(std::mem::size_of::<u32>())
            .max(1);
        let mut buf: Vec<u32> = vec![0u32; words];
        let table_ptr = buf.as_mut_ptr().cast::<MIB_IPADDRTABLE>();
        // SAFETY: `buf` is 4-byte aligned (`align_of::<MIB_IPADDRTABLE>() == 4`) and sized for `size` bytes.
        let ret = unsafe { GetIpAddrTable(Some(table_ptr), &mut size, false) };
        if ret == ERROR_INSUFFICIENT_BUFFER.0 {
            continue;
        }
        if ret != NO_ERROR.0 {
            return Vec::new();
        }

        let buf_bytes = buf.len() * std::mem::size_of::<u32>();
        let header_offset = std::mem::offset_of!(MIB_IPADDRTABLE, table);
        if buf_bytes < header_offset {
            return Vec::new();
        }
        // SAFETY: `GetIpAddrTable` succeeded and wrote `dwNumEntries` into `table_ptr`.
        let num_entries = unsafe { (*table_ptr).dwNumEntries as usize };
        let Some(needed) = num_entries
            .checked_mul(std::mem::size_of::<MIB_IPADDRROW_XP>())
            .and_then(|arr| header_offset.checked_add(arr))
        else {
            return Vec::new();
        };
        if needed > buf_bytes || num_entries == 0 {
            return Vec::new();
        }

        // SAFETY: `header_offset + num_entries * size_of::<MIB_IPADDRROW_XP>() <= buf_bytes`.
        let rows = unsafe {
            std::slice::from_raw_parts((*table_ptr).table.as_ptr(), num_entries)
        };
        let mut out = Vec::with_capacity(num_entries);
        for row in rows {
            if (u32::from(row.wType) & (MIB_IPADDR_DELETED | MIB_IPADDR_DISCONNECTED)) != 0 {
                continue;
            }
            let ip = Ipv4Addr::from(row.dwAddr.to_ne_bytes());
            out.push((row.dwIndex, ip));
        }
        return out;
    }

    Vec::new()
}

#[cfg(windows)]
pub fn detect_lan_ipv4() -> String {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::IpHelper::GetBestInterface;

    const PROBE_DESTS: [[u8; 4]; 3] = [
        [10, 255, 255, 255],
        [192, 168, 1, 1],
        [172, 16, 0, 1],
    ];

    let rows = load_windows_ipv4_rows();
    if rows.is_empty() {
        return "127.0.0.1".to_owned();
    }

    for octets in PROBE_DESTS {
        let mut best_if: u32 = 0;
        let dest_ipaddr = u32::from_ne_bytes(octets);
        // SAFETY: Read-only IP Helper routing table lookup; opens zero sockets.
        let ret = unsafe { GetBestInterface(dest_ipaddr, &mut best_if) };
        if ret == NO_ERROR.0
            && let Some((_, ip)) = rows
                .iter()
                .find(|(idx, ip)| *idx == best_if && is_usable_lan_ipv4(IpAddr::V4(*ip)))
        {
            return ip.to_string();
        }
    }

    if let Some((_, ip)) = rows
        .iter()
        .find(|(_, ip)| is_usable_lan_ipv4(IpAddr::V4(*ip)) && ip.octets()[3] != 1)
    {
        return ip.to_string();
    }
    if let Some((_, ip)) = rows
        .iter()
        .find(|(_, ip)| is_usable_lan_ipv4(IpAddr::V4(*ip)))
    {
        return ip.to_string();
    }
    "127.0.0.1".to_owned()
}

#[cfg(not(windows))]
pub fn detect_lan_ipv4() -> String {
    const PROBE_TARGETS: [(&str, u16); 3] = [
        ("10.255.255.255", 1),
        ("192.168.1.1", 80),
        ("172.16.0.1", 80),
    ];
    for (target_ip, target_port) in PROBE_TARGETS {
        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0")
            && sock.connect((target_ip, target_port)).is_ok()
            && let Ok(local) = sock.local_addr()
            && is_usable_lan_ipv4(local.ip())
        {
            return local.ip().to_string();
        }
    }
    "127.0.0.1".to_owned()
}

// ---------------------------------------------------------------------------
// Credential & Constant-Time Auth Helpers
// ---------------------------------------------------------------------------

fn generate_pin() -> String {
    let val: u32 = rand::rng().random_range(0..1_000_000);
    format!("{val:06}")
}

fn encode_base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

fn generate_token() -> String {
    let mut raw = [0u8; 16];
    rand::rng().fill(&mut raw);
    encode_base64url_nopad(&raw)
}

#[inline]
fn ct_str_eq(a: &str, b: &str) -> bool {
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(d1), Some(d2)) = (h1, h2) {
                out.push(((d1 << 4) | d2) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        let tok = trimmed[7..].trim();
        if !tok.is_empty() {
            return Some(tok.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Same-Origin Mobile Companion HTML5 Viewer (Offline Bilingual VI/EN)
// ---------------------------------------------------------------------------

fn render_pin_login_html() -> &'static str {
    r#"<!DOCTYPE html>
<html lang="vi">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no"/>
<title>Genshin Helper LAN Companion</title>
<style>
  :root{--bg:#121318;--card:#1b1d26;--fg:#edf0f7;--muted:#9aa3b2;--accent:#bb86fc;--err:#ff6b6b;--border:#2b2f3d}
  *{box-sizing:border-box;font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif}
  body{margin:0;min-height:100dvh;display:flex;align-items:center;justify-content:center;background:var(--bg);color:var(--fg);padding:20px}
  .card{width:100%;max-width:360px;background:var(--card);border:1px solid var(--border);border-radius:16px;padding:24px;box-shadow:0 12px 36px rgba(0,0,0,.45)}
  h1{margin:0 0 6px;font-size:19px}
  p{margin:0 0 18px;color:var(--muted);font-size:13px;line-height:1.45}
  input{width:100%;padding:14px;font-size:24px;letter-spacing:8px;text-align:center;border-radius:10px;border:1px solid var(--border);background:#101218;color:var(--fg);outline:none}
  input:focus{border-color:var(--accent)}
  button{width:100%;margin-top:14px;padding:12px;border:0;border-radius:10px;background:var(--accent);color:#121318;font-weight:700;font-size:15px;cursor:pointer}
  .err{margin-top:10px;color:var(--err);font-size:13px;min-height:18px;text-align:center}
</style>
</head>
<body>
  <div class="card">
    <h1>Genshin Helper LAN</h1>
    <p>Nhập mã PIN 6 số hiển thị trên PC / Enter the 6-digit PIN shown on your PC.</p>
    <input id="pin" type="tel" inputmode="numeric" maxlength="6" placeholder="000000" autocomplete="one-time-code"/>
    <button id="go">Kết nối / Connect</button>
    <div class="err" id="err"></div>
  </div>
<script>
const pinEl = document.getElementById('pin');
const errEl = document.getElementById('err');
async function submitPin(){
  errEl.textContent = '';
  const pin = (pinEl.value || '').replace(/\D/g, '');
  if (pin.length !== 6) { errEl.textContent = 'Cần đủ 6 chữ số / Need 6 digits'; return; }
  try {
    const res = await fetch('/auth/pin', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({pin})
    });
    const data = await res.json();
    if (res.ok && data.companion_path) { location.href = data.companion_path; }
    else { errEl.textContent = data.error || 'Sai mã PIN / Invalid PIN'; }
  } catch (e) { errEl.textContent = 'Lỗi kết nối / Network error'; }
}
document.getElementById('go').addEventListener('click', submitPin);
pinEl.addEventListener('keydown', e => { if (e.key === 'Enter') submitPin(); });
</script>
</body>
</html>"#
}

fn render_companion_html(token: &str) -> String {
    let safe_token_json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_owned());
    include_str!("lan_companion_ui.html").replace("__GH_TOKEN_JSON__", &safe_token_json)
}

// ---------------------------------------------------------------------------
// Config, Info DTOs, and Server State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LanCompanionConfig {
    pub host: String,
    pub port: u16,
    pub max_hz: f64,
    pub max_clients: usize,
    pub advertised_host: Option<String>,
}

impl Default for LanCompanionConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_owned(),
            port: DEFAULT_LAN_PORT,
            max_hz: 15.0,
            max_clients: 8,
            advertised_host: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanConnectionInfo {
    pub is_running: bool,
    pub host: String,
    pub bind_host: String,
    pub port: u16,
    pub pin: String,
    pub token: String,
    pub url: String,
    pub ws_url: String,
    pub qr_svg: String,
    pub client_count: usize,
}

#[derive(Clone)]
pub struct LanMapAssets {
    pub maps_db: Arc<MultimapDb>,
    pub tile_cache_dir: PathBuf,
    pub ug_provider: Arc<UndergroundOverlayProvider>,
    pub guide_fetcher: PinGuideFetcher,
    /// Shared official HoYoLAB pin icon + marker UI sprite cache (same instance used by Native Radar).
    pub icon_cache: PinIconCache,
}

type ClientMsgCallback = Arc<dyn Fn(Value) + Send + Sync>;

struct WsClientHandle {
    tx: mpsc::Sender<Arc<String>>,
    queued_bytes: Arc<AtomicUsize>,
    close_notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
}

struct CoalescerState {
    latest_state: Option<Value>,
    pending_payload: Option<Value>,
    last_flush_mono: Instant,
    flush_task: Option<tokio::task::JoinHandle<()>>,
}

struct AuthCreds {
    pin: String,
    token: String,
}

struct ServerLifecycle {
    runtime_handle: Option<tokio::runtime::Handle>,
    shutdown_tx: Option<watch::Sender<bool>>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    bg_stop_tx: Option<oneshot::Sender<()>>,
    bg_thread: Option<std::thread::JoinHandle<()>>,
}

struct LanInner {
    bind_host: String,
    port: AtomicU16,
    max_clients: usize,
    min_broadcast_interval: Duration,
    advertised_host_override: Option<String>,

    is_running: AtomicBool,
    creds: RwLock<AuthCreds>,
    pin_failures: Mutex<HashMap<String, VecDeque<Instant>>>,
    clients: Mutex<HashMap<u64, WsClientHandle>>,
    next_client_id: AtomicU64,
    coalescer: Mutex<CoalescerState>,
    on_client_message: RwLock<Option<ClientMsgCallback>>,
    lifecycle: Mutex<ServerLifecycle>,

    map_assets: RwLock<Option<LanMapAssets>>,
    #[cfg(test)]
    tile_base_override: RwLock<Option<String>>,
    http_client: reqwest::Client,
    pins_snapshot: RwLock<Option<Arc<String>>>,
    pins_rev: AtomicU64,
    pins_rev_tx: watch::Sender<u64>,
    route_snapshot: RwLock<Option<Arc<String>>>,
}

#[derive(Clone)]
pub struct LanCompanionServer {
    inner: Arc<LanInner>,
}

impl Default for LanCompanionServer {
    fn default() -> Self {
        Self::new(LanCompanionConfig::default())
    }
}

impl LanCompanionServer {
    pub fn new(config: LanCompanionConfig) -> Self {
        let max_hz = if config.max_hz.is_finite() && config.max_hz >= 1.0 {
            config.max_hz
        } else {
            15.0
        };
        let min_interval = Duration::from_secs_f64(1.0 / max_hz);
        let init_last_flush = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        let (pins_rev_tx, _pins_rx) = watch::channel(0u64);
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_default();

        Self {
            inner: Arc::new(LanInner {
                bind_host: config.host,
                port: AtomicU16::new(config.port),
                max_clients: config.max_clients.max(1),
                min_broadcast_interval: min_interval,
                advertised_host_override: config.advertised_host,
                is_running: AtomicBool::new(false),
                creds: RwLock::new(AuthCreds {
                    pin: generate_pin(),
                    token: generate_token(),
                }),
                pin_failures: Mutex::new(HashMap::new()),
                clients: Mutex::new(HashMap::new()),
                next_client_id: AtomicU64::new(1),
                coalescer: Mutex::new(CoalescerState {
                    latest_state: None,
                    pending_payload: None,
                    last_flush_mono: init_last_flush,
                    flush_task: None,
                }),
                on_client_message: RwLock::new(None),
                lifecycle: Mutex::new(ServerLifecycle {
                    runtime_handle: None,
                    shutdown_tx: None,
                    server_task: None,
                    bg_stop_tx: None,
                    bg_thread: None,
                }),
                map_assets: RwLock::new(None),
                #[cfg(test)]
                tile_base_override: RwLock::new(None),
                http_client,
                pins_snapshot: RwLock::new(None),
                pins_rev: AtomicU64::new(0),
                pins_rev_tx,
                route_snapshot: RwLock::new(None),
            }),
        }
    }

    pub fn set_map_assets(&self, assets: LanMapAssets) {
        *self
            .inner
            .map_assets
            .write()
            .expect("map_assets lock poisoned") = Some(assets);
    }

    #[cfg(test)]
    pub fn set_tile_base_for_test(&self, base: impl Into<String>) {
        *self
            .inner
            .tile_base_override
            .write()
            .expect("tile_base_override lock poisoned") = Some(base.into());
    }

    pub fn update_pins_snapshot(&self, payload: Value) -> u64 {
        let rev = self.inner.pins_rev.fetch_add(1, Ordering::AcqRel) + 1;
        let mut obj = match payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        obj.insert("type".to_owned(), Value::String("PINS_SNAPSHOT".to_owned()));
        obj.insert("rev".to_owned(), json!(rev));
        if let Ok(encoded) = serde_json::to_string(&Value::Object(obj)) {
            let arc_str = Arc::new(encoded);
            *self
                .inner
                .pins_snapshot
                .write()
                .expect("pins_snapshot lock poisoned") = Some(arc_str);
            // Notify subscribers via watch channel instead of pushing large snapshots through
            // the 256 KiB per-client mpsc queue (which would evict healthy clients).
            let _ = self.inner.pins_rev_tx.send(rev);
        }
        rev
    }

    /// Returns the most recently cached `/lan_pins` JSON snapshot string (if any) without copying.
    pub fn latest_pins_snapshot_json(&self) -> Option<Arc<String>> {
        self.inner
            .pins_snapshot
            .read()
            .expect("pins_snapshot lock poisoned")
            .clone()
    }

    pub fn update_route_snapshot(&self, payload: Value) {
        let mut obj = match payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        obj.insert("type".to_owned(), Value::String("ROUTE_UPDATED".to_owned()));
        if let Ok(encoded) = serde_json::to_string(&Value::Object(obj)) {
            let arc_str = Arc::new(encoded);
            *self
                .inner
                .route_snapshot
                .write()
                .expect("route_snapshot lock poisoned") = Some(Arc::clone(&arc_str));
            if self.is_running() {
                flush_encoded_to_clients(&self.inner, arc_str);
            }
        }
    }

    pub fn send_event_to_clients(&self, payload: Value) {
        if self.is_running() {
            flush_to_clients(&self.inner, &payload);
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.is_running.load(Ordering::Acquire)
    }

    pub fn port(&self) -> u16 {
        self.inner.port.load(Ordering::Acquire)
    }

    pub fn set_port(&self, port: u16) {
        if !self.is_running() {
            self.inner.port.store(port, Ordering::Release);
        }
    }

    pub fn bind_host(&self) -> &str {
        &self.inner.bind_host
    }

    pub fn pin(&self) -> String {
        self.inner
            .creds
            .read()
            .expect("creds lock poisoned")
            .pin
            .clone()
    }

    pub fn token(&self) -> String {
        self.inner
            .creds
            .read()
            .expect("creds lock poisoned")
            .token
            .clone()
    }

    pub fn client_count(&self) -> usize {
        self.inner
            .clients
            .lock()
            .expect("clients lock poisoned")
            .len()
    }

    pub fn set_on_client_message(&self, cb: Option<ClientMsgCallback>) {
        *self
            .inner
            .on_client_message
            .write()
            .expect("callback lock poisoned") = cb;
    }

    pub fn rotate_credentials(&self) -> (String, String) {
        let new_pin = generate_pin();
        let new_tok = generate_token();
        {
            let mut creds = self.inner.creds.write().expect("creds lock poisoned");
            creds.pin = new_pin.clone();
            creds.token = new_tok.clone();
        }
        self.inner
            .pin_failures
            .lock()
            .expect("pin_failures lock poisoned")
            .clear();
        (new_pin, new_tok)
    }

    pub fn rotate_pin(&self) -> String {
        let new_pin = generate_pin();
        {
            let mut creds = self.inner.creds.write().expect("creds lock poisoned");
            creds.pin = new_pin.clone();
        }
        self.inner
            .pin_failures
            .lock()
            .expect("pin_failures lock poisoned")
            .clear();
        new_pin
    }

    pub fn get_advertised_host(&self) -> String {
        if let Some(ref ov) = self.inner.advertised_host_override
            && !ov.trim().is_empty()
        {
            return ov.clone();
        }
        if matches!(self.inner.bind_host.as_str(), "0.0.0.0" | "" | "::") {
            return detect_lan_ipv4();
        }
        self.inner.bind_host.clone()
    }

    pub fn get_companion_url(&self) -> String {
        let adv = self.get_advertised_host();
        let port = self.port();
        let tok = self.token();
        format!("http://{adv}:{port}/companion?token={tok}")
    }

    pub fn get_ws_url(&self) -> String {
        let adv = self.get_advertised_host();
        let port = self.port();
        let tok = self.token();
        format!("ws://{adv}:{port}/ws/{tok}")
    }

    pub fn get_qr_svg(&self) -> String {
        generate_qr_svg(&self.get_companion_url()).unwrap_or_default()
    }

    pub fn get_qr_svg_with(&self, quiet_zone: usize, module_px: usize) -> CoreResult<String> {
        let matrix = generate_qr_matrix(&self.get_companion_url())?;
        Ok(qr_matrix_to_svg(
            &matrix, quiet_zone, module_px, "#111111", "#ffffff",
        ))
    }

    pub fn get_connection_info(&self) -> LanConnectionInfo {
        let url = self.get_companion_url();
        let qr_svg = generate_qr_svg(&url).unwrap_or_default();
        LanConnectionInfo {
            is_running: self.is_running(),
            host: self.get_advertised_host(),
            bind_host: self.inner.bind_host.clone(),
            port: self.port(),
            pin: self.pin(),
            token: self.token(),
            url,
            ws_url: self.get_ws_url(),
            qr_svg,
            client_count: self.client_count(),
        }
    }

    pub async fn start(&self) -> CoreResult<LanConnectionInfo> {
        if self.is_running() {
            return Ok(self.get_connection_info());
        }

        let port = self.port();
        let listener = TcpListener::bind((self.inner.bind_host.as_str(), port)).await?;
        let actual_port = listener.local_addr()?.port();
        self.inner.port.store(actual_port, Ordering::Release);

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let rt_handle = tokio::runtime::Handle::current();

        let app = build_router(Arc::clone(&self.inner));
        let make_svc = app.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum::serve(listener, make_svc).with_graceful_shutdown(async move {
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
            lc.runtime_handle = Some(rt_handle);
            lc.shutdown_tx = Some(shutdown_tx);
            lc.server_task = Some(task);
        }
        self.inner.is_running.store(true, Ordering::Release);
        Ok(self.get_connection_info())
    }

    pub async fn stop(&self) {
        self.inner.is_running.store(false, Ordering::Release);

        {
            let mut coal = self.inner.coalescer.lock().expect("coalescer lock poisoned");
            if let Some(handle) = coal.flush_task.take() {
                handle.abort();
            }
            coal.pending_payload = None;
        }

        // Disconnect all WS clients immediately
        let handles: Vec<WsClientHandle> = {
            let mut clients = self.inner.clients.lock().expect("clients lock poisoned");
            clients.drain().map(|(_, h)| h).collect()
        };
        for h in handles {
            h.closed.store(true, Ordering::Release);
            h.close_notify.notify_waiters();
        }

        let (shutdown_tx, server_task) = {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            lc.runtime_handle = None;
            (lc.shutdown_tx.take(), lc.server_task.take())
        };
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(true);
        }
        if let Some(task) = server_task {
            let _ = task.await;
        }
    }

    /// Synchronous opt-in starter that runs an isolated Tokio runtime on a background thread.
    pub fn start_in_background(&self) -> CoreResult<LanConnectionInfo> {
        if self.is_running() {
            return Ok(self.get_connection_info());
        }

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<CoreResult<LanConnectionInfo>>(1);
        let (bg_stop_tx, bg_stop_rx) = oneshot::channel::<()>();
        let server_clone = self.clone();

        let join = std::thread::Builder::new()
            .name("GenshinHelper-LanCompanion".to_owned())
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
                    match server_clone.start().await {
                        Ok(info) => {
                            let _ = init_tx.send(Ok(info));
                        }
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    }
                    let _ = bg_stop_rx.await;
                    server_clone.stop().await;
                });
            })
            .map_err(CoreError::Io)?;

        {
            let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
            lc.bg_stop_tx = Some(bg_stop_tx);
            lc.bg_thread = Some(join);
        }

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(info)) => Ok(info),
            Ok(Err(e)) => {
                self.stop_background();
                Err(e)
            }
            Err(_) => {
                self.stop_background();
                Err(CoreError::Sync("Timed out starting LanCompanionServer".to_owned()))
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
        } else if self.is_running() {
            // If started via async `start()` without a bg thread, trigger shutdown synchronously
            self.inner.is_running.store(false, Ordering::Release);
            let handles: Vec<WsClientHandle> = {
                let mut clients = self.inner.clients.lock().expect("clients lock poisoned");
                clients.drain().map(|(_, h)| h).collect()
            };
            for h in handles {
                h.closed.store(true, Ordering::Release);
                h.close_notify.notify_waiters();
            }
            let shutdown_tx = {
                let mut lc = self.inner.lifecycle.lock().expect("lifecycle lock poisoned");
                lc.runtime_handle = None;
                lc.server_task.take();
                lc.shutdown_tx.take()
            };
            if let Some(tx) = shutdown_tx {
                let _ = tx.send(true);
            }
        }
    }

    pub fn latest_state(&self) -> Option<Value> {
        self.inner
            .coalescer
            .lock()
            .expect("coalescer lock poisoned")
            .latest_state
            .clone()
    }

    /// Thread-safely broadcasts live telemetry to all connected LAN companion clients
    /// with 15 Hz leading/trailing coalescing and >256 KiB slow-client eviction.
    pub fn broadcast_state(&self, payload: Value, force: bool) -> bool {
        if !self.is_running() {
            return false;
        }

        let mut stamped = match payload {
            Value::Object(map) => map,
            other => {
                let mut map = Map::new();
                map.insert("data".to_owned(), other);
                map
            }
        };
        stamped
            .entry("type".to_owned())
            .or_insert_with(|| Value::String("STATE".to_owned()));
        if !stamped.contains_key("ts") {
            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let ts = ((unix_ms as f64) / 1000.0 * 1000.0).round() / 1000.0;
            stamped.insert("ts".to_owned(), json!(ts));
        }
        let stamped_val = Value::Object(stamped);

        let rt_handle = {
            self.inner
                .lifecycle
                .lock()
                .expect("lifecycle lock poisoned")
                .runtime_handle
                .clone()
        };

        let now = Instant::now();
        let min_interval = self.inner.min_broadcast_interval;

        let mut coal = self.inner.coalescer.lock().expect("coalescer lock poisoned");
        let elapsed = now.saturating_duration_since(coal.last_flush_mono);
        coal.latest_state = Some(stamped_val.clone());

        if force || elapsed >= min_interval {
            if let Some(handle) = coal.flush_task.take() {
                handle.abort();
            }
            coal.pending_payload = None;
            coal.last_flush_mono = now;
            drop(coal);
            flush_to_clients(&self.inner, &stamped_val);
        } else {
            coal.pending_payload = Some(stamped_val);
            if coal.flush_task.is_none()
                && let Some(handle) = rt_handle
            {
                let delay = min_interval
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(1));
                let inner_clone = Arc::clone(&self.inner);
                coal.flush_task = Some(handle.spawn(async move {
                    tokio::time::sleep(delay).await;
                    let pending = {
                        let mut c = inner_clone.coalescer.lock().expect("coalescer lock poisoned");
                        c.flush_task = None;
                        let p = c.pending_payload.take();
                        if p.is_some() {
                            c.last_flush_mono = Instant::now();
                        }
                        p
                    };
                    if inner_clone.is_running.load(Ordering::Acquire)
                        && let Some(payload) = pending
                    {
                        flush_to_clients(&inner_clone, &payload);
                    }
                }));
            }
        }
        true
    }
}

fn enqueue_single_client(
    tx: &mpsc::Sender<Arc<String>>,
    queued_bytes: &AtomicUsize,
    msg_arc: Arc<String>,
) {
    let msg_len = msg_arc.len();
    let queued = queued_bytes.load(Ordering::Acquire);
    if queued + msg_len <= SLOW_CLIENT_BUFFER_LIMIT_BYTES && tx.try_send(msg_arc).is_ok() {
        queued_bytes.fetch_add(msg_len, Ordering::AcqRel);
    }
}

fn flush_encoded_to_clients(inner: &Arc<LanInner>, msg_arc: Arc<String>) {
    let msg_len = msg_arc.len();

    let mut evicted: Vec<WsClientHandle> = Vec::new();
    {
        let mut clients = inner.clients.lock().expect("clients lock poisoned");
        if clients.is_empty() {
            return;
        }
        let ids: Vec<u64> = clients.keys().copied().collect();
        for id in ids {
            let should_drop = if let Some(handle) = clients.get(&id) {
                let queued = handle.queued_bytes.load(Ordering::Acquire);
                if queued + msg_len > SLOW_CLIENT_BUFFER_LIMIT_BYTES {
                    true
                } else {
                    match handle.tx.try_send(Arc::clone(&msg_arc)) {
                        Ok(()) => {
                            handle.queued_bytes.fetch_add(msg_len, Ordering::AcqRel);
                            false
                        }
                        Err(_) => true,
                    }
                }
            } else {
                false
            };
            if should_drop && let Some(removed) = clients.remove(&id) {
                evicted.push(removed);
            }
        }
    }

    for h in evicted {
        h.closed.store(true, Ordering::Release);
        h.close_notify.notify_waiters();
    }
}

fn flush_to_clients(inner: &Arc<LanInner>, payload: &Value) {
    let Ok(encoded) = serde_json::to_string(payload) else {
        return;
    };
    flush_encoded_to_clients(inner, Arc::new(encoded));
}

// ---------------------------------------------------------------------------
// Axum Router, Security Header Middleware & Handlers
// ---------------------------------------------------------------------------

fn build_router(inner: Arc<LanInner>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route("/", get(handle_root))
        .route("/auth/pin", post(handle_auth_pin))
        .route("/companion", get(handle_companion))
        .route("/qr.svg", get(handle_qr_svg))
        .route("/api/map_meta/{map_id}", get(handle_api_map_meta))
        .route("/api/tile/{map_id}/{z}/{tx}/{ty}", get(handle_api_tile))
        .route(
            "/api/ug_overlay/{map_id}/{group_id}/{floor_id}",
            get(handle_api_ug_overlay),
        )
        .route("/api/icon/{map_id}/{label_id}", get(handle_api_icon))
        .route("/api/marker_asset/{key}", get(handle_api_marker_asset))
        .route("/api/pin_guide/{pin_id}", get(handle_api_pin_guide))
        .route("/api/pin_guide_img/{pin_id}", get(handle_api_pin_guide_img))
        .route("/ws", get(handle_ws_no_path))
        .route("/ws/", get(handle_ws_no_path))
        .route("/ws/{*token}", get(handle_ws_with_path))
        .fallback(handle_not_found)
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .layer(middleware::from_fn(lan_security_headers_middleware))
        .with_state(inner)
}

async fn lan_security_headers_middleware(
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let headers = resp.headers_mut();
        if !headers.contains_key(CACHE_CONTROL) {
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        if !is_api {
            headers.insert(CONNECTION, HeaderValue::from_static("close"));
        }
    }
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

fn text_response(status: StatusCode, body: &'static str) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        "application/octet-stream"
    }
}

fn image_response(bytes: Vec<u8>, cache_control: &'static str) -> Response {
    let mime = sniff_image_mime(&bytes);
    let mut resp = (StatusCode::OK, bytes).into_response();
    let hdrs = resp.headers_mut();
    hdrs.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    hdrs.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    resp
}

fn verify_query_or_bearer(
    inner: &LanInner,
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    let q_tok = query.get("token").map(String::as_str).filter(|s| !s.is_empty());
    let bearer_tok = extract_bearer_token(headers);
    verify_provided_token(inner, q_tok.or(bearer_tok.as_deref()))
}

fn resolve_tile_base(inner: &LanInner) -> String {
    #[cfg(test)]
    if let Some(ref ov) = *inner
        .tile_base_override
        .read()
        .expect("tile_base_override lock poisoned")
        && !ov.trim().is_empty()
    {
        return ov.trim().trim_end_matches('/').to_owned();
    }
    #[cfg(not(test))]
    let _ = inner;
    DEFAULT_TILE_BASE.to_owned()
}

async fn handle_not_found() -> Response {
    text_response(StatusCode::NOT_FOUND, "Not Found")
}

async fn handle_health(State(inner): State<Arc<LanInner>>) -> Response {
    let clients = inner.clients.lock().expect("clients lock poisoned").len();
    json_response(StatusCode::OK, &json!({"ok": true, "clients": clients}))
}

async fn handle_root() -> Response {
    let mut resp = (StatusCode::OK, render_pin_login_html()).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP_PIN_LOGIN),
    );
    resp
}

fn check_pin_rate_limit(inner: &LanInner, peer_ip: &str) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs_f64(PIN_WINDOW_SECS);
    let mut map = inner
        .pin_failures
        .lock()
        .expect("pin_failures lock poisoned");
    let Some(bucket) = map.get_mut(peer_ip) else {
        return true;
    };
    while let Some(&front) = bucket.front() {
        if now.saturating_duration_since(front) > window {
            bucket.pop_front();
        } else {
            break;
        }
    }
    bucket.len() < PIN_MAX_FAILURES
}

fn record_pin_failure(inner: &LanInner, peer_ip: &str) {
    let now = Instant::now();
    let window = Duration::from_secs_f64(PIN_WINDOW_SECS);
    let mut map = inner
        .pin_failures
        .lock()
        .expect("pin_failures lock poisoned");
    let bucket = map
        .entry(peer_ip.to_owned())
        .or_insert_with(|| VecDeque::with_capacity(10));
    while let Some(&front) = bucket.front() {
        if now.saturating_duration_since(front) > window {
            bucket.pop_front();
        } else {
            break;
        }
    }
    if bucket.len() >= 10 {
        bucket.pop_front();
    }
    bucket.push_back(now);
}

async fn handle_auth_pin(
    State(inner): State<Arc<LanInner>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    let peer_ip = addr.ip().to_string();

    if !check_pin_rate_limit(&inner, &peer_ip) {
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({"ok": false, "error": "Too many attempts; retry in 60s"}),
        );
    }

    let req_obj: Map<String, Value> = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(map)) => map,
            _ => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({"ok": false, "error": "Invalid JSON"}),
                );
            }
        }
    };

    let candidate_pin = match req_obj.get("pin") {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };

    let (expected_pin, current_token) = {
        let creds = inner.creds.read().expect("creds lock poisoned");
        (creds.pin.clone(), creds.token.clone())
    };

    if !candidate_pin.is_empty() && ct_str_eq(&candidate_pin, &expected_pin) {
        return json_response(
            StatusCode::OK,
            &json!({
                "ok": true,
                "token": current_token,
                "companion_path": format!("/companion?token={current_token}"),
            }),
        );
    }

    record_pin_failure(&inner, &peer_ip);
    json_response(
        StatusCode::FORBIDDEN,
        &json!({"ok": false, "error": "Invalid PIN"}),
    )
}

fn verify_provided_token(inner: &LanInner, provided: Option<&str>) -> Result<String, StatusCode> {
    let Some(cand) = provided.filter(|s| !s.is_empty()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let expected = inner
        .creds
        .read()
        .expect("creds lock poisoned")
        .token
        .clone();
    if ct_str_eq(cand, &expected) {
        Ok(expected)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

async fn handle_companion(
    State(inner): State<Arc<LanInner>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let token = match verify_query_or_bearer(&inner, &query, &headers) {
        Ok(t) => t,
        Err(status) => return text_response(status, "Unauthorized"),
    };

    let html = render_companion_html(&token);
    let mut resp = (StatusCode::OK, html).into_response();
    let hdrs = resp.headers_mut();
    hdrs.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    hdrs.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP_COMPANION),
    );
    resp
}

async fn handle_qr_svg(
    State(inner): State<Arc<LanInner>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }

    let adv_host = if let Some(ref ov) = inner.advertised_host_override
        && !ov.trim().is_empty()
    {
        ov.clone()
    } else if matches!(inner.bind_host.as_str(), "0.0.0.0" | "" | "::") {
        detect_lan_ipv4()
    } else {
        inner.bind_host.clone()
    };
    let port = inner.port.load(Ordering::Acquire);
    let tok = inner.creds.read().expect("creds lock poisoned").token.clone();
    let url = format!("http://{adv_host}:{port}/companion?token={tok}");
    let svg = generate_qr_svg(&url).unwrap_or_default();

    let mut resp = (StatusCode::OK, svg).into_response();
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    resp
}

async fn handle_api_map_meta(
    State(inner): State<Arc<LanInner>>,
    AxumPath(map_id): AxumPath<i32>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Map assets unavailable");
    };
    let Some(meta) = assets.maps_db.map(map_id) else {
        return text_response(StatusCode::NOT_FOUND, "Map not found");
    };

    let lang = query.get("lang").map(String::as_str);
    let loc_name = HoYoLABLabelCatalog::get_map_name(map_id, lang, &meta.name);
    let mut floors = Vec::new();
    for g in &meta.point_groups {
        for fl in &g.floors {
            let Some(ref ov) = fl.overlay else {
                continue;
            };
            let b = ov.bounds();
            floors.push(json!({
                "group_id": g.id,
                "group_name": g.name,
                "floor_id": fl.id,
                "floor_name": fl.floor_name,
                "floor_short": fl.short_label(),
                "bounds": [b.0, b.1, b.2, b.3],
                "reflect_y": ov.reflect_y,
            }));
        }
    }

    json_response(
        StatusCode::OK,
        &json!({
            "ok": true,
            "map_id": map_id,
            "name": loc_name,
            "detail_v2": meta.detail_v2,
            "waypoints": meta.waypoints,
            "floors": floors,
        }),
    )
}

async fn handle_api_tile(
    State(inner): State<Arc<LanInner>>,
    AxumPath((map_id, z, tx, ty)): AxumPath<(i32, i32, i32, i32)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Map assets unavailable");
    };
    let Some(mdata) = assets.maps_db.map(map_id) else {
        return text_response(StatusCode::NOT_FOUND, "Map not found");
    };
    let Some(ref d2) = mdata.detail_v2 else {
        return text_response(StatusCode::NOT_FOUND, "Map detail unavailable");
    };
    if d2.map_version.trim().is_empty()
        || z < d2.min_zoom - 1
        || z > d2.max_zoom + 1
        || tx < 0
        || ty < 0
    {
        return text_response(StatusCode::NOT_FOUND, "Tile out of bounds");
    }
    let (max_tx, max_ty) = d2.max_tile_xy(z);
    if tx > max_tx || ty > max_ty {
        return text_response(StatusCode::NOT_FOUND, "Tile out of bounds");
    }

    let tile_dir = assets
        .tile_cache_dir
        .join(map_id.to_string())
        .join(z.to_string());
    let img_path = tile_dir.join(format!("{tx}_{ty}.webp"));
    let miss_path = tile_dir.join(format!("{tx}_{ty}.404"));

    if let Ok(cached) = tokio::fs::read(&img_path).await
        && cached.len() >= 12
        && sniff_image_mime(&cached).starts_with("image/")
    {
        return image_response(cached, "private, max-age=86400");
    }
    if tokio::fs::try_exists(&miss_path).await.unwrap_or(false) {
        return text_response(StatusCode::NOT_FOUND, "Tile missing");
    }

    let tile_base = resolve_tile_base(&inner);
    let url = d2.tile_url(&tile_base, map_id, z, tx, ty);
    let Ok(resp) = inner.http_client.get(&url).send().await else {
        return text_response(StatusCode::BAD_GATEWAY, "Upstream tile fetch failed");
    };
    let status = resp.status();
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::GONE
    ) {
        let miss_clone = miss_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(parent) = miss_clone.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = atomic_io::write_bytes_atomic(&miss_clone, b"");
        })
        .await;
        return text_response(StatusCode::NOT_FOUND, "Tile missing");
    }
    if !status.is_success() {
        return text_response(StatusCode::BAD_GATEWAY, "Upstream tile HTTP error");
    }
    let Ok(bytes) = resp.bytes().await else {
        return text_response(StatusCode::BAD_GATEWAY, "Upstream tile read failed");
    };
    if bytes.len() < 12
        || bytes.len() > MAX_REMOTE_TILE_BYTES
        || !sniff_image_mime(&bytes).starts_with("image/")
    {
        return text_response(StatusCode::BAD_GATEWAY, "Invalid tile image");
    }

    let raw = bytes.to_vec();
    let raw_save = raw.clone();
    let img_save = img_path.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(parent) = img_save.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = atomic_io::write_bytes_atomic(&img_save, &raw_save);
    })
    .await;

    image_response(raw, "private, max-age=86400")
}

async fn handle_api_ug_overlay(
    State(inner): State<Arc<LanInner>>,
    AxumPath((map_id, group_id, floor_id)): AxumPath<(i32, i32, i32)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Overlay assets unavailable");
    };
    let floor_spec = format!("{group_id}:{floor_id}");
    let bytes_opt = tokio::task::spawn_blocking(move || {
        assets
            .ug_provider
            .get_overlay_bytes(map_id, &floor_spec, false)
    })
    .await
    .ok()
    .flatten();

    match bytes_opt {
        Some(bytes) if bytes.len() >= 8 => image_response(bytes, "private, max-age=86400"),
        _ => text_response(StatusCode::NOT_FOUND, "Overlay not found"),
    }
}

async fn handle_api_icon(
    State(inner): State<Arc<LanInner>>,
    AxumPath((map_id, label_id)): AxumPath<(i32, i32)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Icon assets unavailable");
    };

    let bytes_opt = tokio::task::spawn_blocking(move || {
        assets
            .icon_cache
            .get_or_fetch_icon_bytes_sync(map_id, label_id)
    })
    .await
    .ok()
    .flatten();

    match bytes_opt {
        Some(raw) => image_response(raw, "private, max-age=86400"),
        None => text_response(StatusCode::NOT_FOUND, "Icon not found"),
    }
}

async fn handle_api_marker_asset(
    State(inner): State<Arc<LanInner>>,
    AxumPath(key): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    let Some(asset) = MarkerUiAsset::from_key(&key) else {
        return text_response(StatusCode::NOT_FOUND, "Unknown marker asset");
    };
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Icon assets unavailable");
    };

    let bytes_opt = tokio::task::spawn_blocking(move || {
        assets.icon_cache.get_or_fetch_ui_asset_bytes_sync(asset)
    })
    .await
    .ok()
    .flatten();

    match bytes_opt {
        Some(raw) => image_response(raw, "private, max-age=86400"),
        None => text_response(StatusCode::NOT_FOUND, "Marker asset unavailable"),
    }
}

async fn handle_api_pin_guide(
    State(inner): State<Arc<LanInner>>,
    AxumPath(pin_id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    if !PinGuideFetcher::is_fetchable_id(&pin_id) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"ok": false, "error": "Invalid pin_id"}),
        );
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Guide fetcher unavailable");
    };
    let map_id = query
        .get("map_id")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(2);
    let lang_opt = query.get("lang").cloned();
    let pid_owned = pin_id.clone();

    let guide = match tokio::task::spawn_blocking(move || {
        if let Some(cached) = assets.guide_fetcher.get_cached(&pid_owned, lang_opt.as_deref()) {
            return cached;
        }
        assets
            .guide_fetcher
            .fetch_sync(&pid_owned, map_id, lang_opt.as_deref())
    })
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({"ok": false}));
        }
    };

    json_response(
        StatusCode::OK,
        &json!({
            "ok": guide.ok,
            "pin_id": guide.pin_id,
            "description": guide.description,
            "has_image": guide.image_bytes.as_ref().is_some_and(|b| !b.is_empty()),
            "lang": guide.lang,
        }),
    )
}

async fn handle_api_pin_guide_img(
    State(inner): State<Arc<LanInner>>,
    AxumPath(pin_id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = verify_query_or_bearer(&inner, &query, &headers) {
        return text_response(status, "Unauthorized");
    }
    if !PinGuideFetcher::is_fetchable_id(&pin_id) {
        return text_response(StatusCode::BAD_REQUEST, "Invalid pin_id");
    }
    let assets = inner
        .map_assets
        .read()
        .expect("map_assets lock poisoned")
        .clone();
    let Some(assets) = assets else {
        return text_response(StatusCode::NOT_FOUND, "Guide fetcher unavailable");
    };
    let map_id = query
        .get("map_id")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(2);
    let lang_opt = query.get("lang").cloned();
    let pid_owned = pin_id.clone();

    let bytes_opt = tokio::task::spawn_blocking(move || {
        let g = assets
            .guide_fetcher
            .get_cached(&pid_owned, lang_opt.as_deref())
            .unwrap_or_else(|| {
                assets
                    .guide_fetcher
                    .fetch_sync(&pid_owned, map_id, lang_opt.as_deref())
            });
        g.image_bytes.filter(|b| !b.is_empty())
    })
    .await
    .ok()
    .flatten();

    match bytes_opt {
        Some(bytes) => image_response(bytes, "private, max-age=3600"),
        None => text_response(StatusCode::NOT_FOUND, "Guide image unavailable"),
    }
}

async fn handle_ws_no_path(
    State(inner): State<Arc<LanInner>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    handle_ws_common(inner, addr, None, query, headers, ws).await
}

async fn handle_ws_with_path(
    State(inner): State<Arc<LanInner>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    AxumPath(raw_seg): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let trimmed = raw_seg.trim_matches('/');
    let path_tok = if trimmed.is_empty() {
        None
    } else {
        Some(percent_decode(trimmed))
    };
    handle_ws_common(inner, addr, path_tok, query, headers, ws).await
}

async fn handle_ws_common(
    inner: Arc<LanInner>,
    addr: SocketAddr,
    path_tok: Option<String>,
    query: HashMap<String, String>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let q_tok = query.get("token").map(String::as_str).filter(|s| !s.is_empty());
    let bearer_tok = extract_bearer_token(&headers);
    let provided = path_tok.as_deref().filter(|s| !s.is_empty()).or(q_tok).or(bearer_tok.as_deref());

    // Auth check MUST run before WebSocketUpgrade header validation (401/403 before 426).
    if let Err(status) = verify_provided_token(&inner, provided) {
        return text_response(status, "Unauthorized WebSocket");
    }

    let upgrade = match ws {
        Ok(u) => u,
        Err(_) => {
            let mut resp =
                text_response(StatusCode::UPGRADE_REQUIRED, "WebSocket upgrade required");
            resp.headers_mut()
                .insert(UPGRADE, HeaderValue::from_static("websocket"));
            return resp;
        }
    };

    let current_clients = inner.clients.lock().expect("clients lock poisoned").len();
    if current_clients >= inner.max_clients {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many companion clients",
        );
    }

    let peer_str = addr.to_string();
    upgrade
        .max_message_size(MAX_WS_PAYLOAD_BYTES)
        .max_write_buffer_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| handle_ws_connection(inner, socket, peer_str))
}

async fn ws_send_with_timeout(
    socket: &mut WebSocket,
    close_notify: &Notify,
    msg: Message,
) -> bool {
    tokio::select! {
        biased;
        _ = close_notify.notified() => false,
        res = tokio::time::timeout(Duration::from_secs(4), socket.send(msg)) => {
            matches!(res, Ok(Ok(())))
        }
    }
}

async fn handle_ws_connection(inner: Arc<LanInner>, mut socket: WebSocket, _peer: String) {
    let client_id = inner.next_client_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel::<Arc<String>>(64);
    let tx_self = tx.clone();
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let close_notify = Arc::new(Notify::new());
    let closed = Arc::new(AtomicBool::new(false));

    let over_limit = {
        let mut clients = inner.clients.lock().expect("clients lock poisoned");
        if clients.len() >= inner.max_clients {
            true
        } else {
            clients.insert(
                client_id,
                WsClientHandle {
                    tx,
                    queued_bytes: Arc::clone(&queued_bytes),
                    close_notify: Arc::clone(&close_notify),
                    closed: Arc::clone(&closed),
                },
            );
            false
        }
    };
    if over_limit {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    // Immediately push latest known state upon WS connect (preserved first-frame contract)
    let initial_state = {
        inner
            .coalescer
            .lock()
            .expect("coalescer lock poisoned")
            .latest_state
            .clone()
    };
    if let Some(state_val) = initial_state
        && let Ok(text) = serde_json::to_string(&state_val)
    {
        let init_ok = ws_send_with_timeout(&mut socket, &close_notify, Message::text(text)).await;
        if !init_ok {
            inner
                .clients
                .lock()
                .expect("clients lock poisoned")
                .remove(&client_id);
            return;
        }
    }

    let mut pins_rx = inner.pins_rev_tx.subscribe();
    let _ = pins_rx.borrow_and_update();

    loop {
        if !inner.is_running.load(Ordering::Acquire) || closed.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            biased;
            _ = close_notify.notified() => {
                break;
            }
            outbound = rx.recv() => {
                let Some(msg_arc) = outbound else {
                    break;
                };
                queued_bytes.fetch_sub(msg_arc.len(), Ordering::AcqRel);
                if closed.load(Ordering::Acquire) {
                    break;
                }
                if !ws_send_with_timeout(&mut socket, &close_notify, Message::text((*msg_arc).clone())).await {
                    break;
                }
            }
            pins_ev = pins_rx.changed() => {
                if pins_ev.is_ok() {
                    let snap = inner
                        .pins_snapshot
                        .read()
                        .expect("pins_snapshot lock poisoned")
                        .clone();
                    if let Some(s_arc) = snap
                        && !ws_send_with_timeout(&mut socket, &close_notify, Message::text((*s_arc).clone())).await
                    {
                        break;
                    }
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    // Axum/Tungstenite auto-responds with Pong on Ping; do NOT duplicate Pong.
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Binary(_))) => {}
                    Some(Ok(Message::Text(txt))) => {
                        if txt.len() <= MAX_WS_PAYLOAD_BYTES
                            && let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(txt.as_str())
                        {
                            let msg_type = obj
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim()
                                .to_ascii_uppercase();

                            if msg_type == "HELLO" {
                                let r_snap = inner
                                    .route_snapshot
                                    .read()
                                    .expect("route_snapshot lock poisoned")
                                    .clone();
                                if let Some(r_arc) = r_snap
                                    && !ws_send_with_timeout(&mut socket, &close_notify, Message::text((*r_arc).clone())).await
                                {
                                    break;
                                }
                                let snap = inner
                                    .pins_snapshot
                                    .read()
                                    .expect("pins_snapshot lock poisoned")
                                    .clone();
                                let _ = pins_rx.borrow_and_update();
                                if let Some(s_arc) = snap
                                    && !ws_send_with_timeout(&mut socket, &close_notify, Message::text((*s_arc).clone())).await
                                {
                                    break;
                                }
                            } else if msg_type == "PING" {
                                enqueue_single_client(
                                    &tx_self,
                                    &queued_bytes,
                                    Arc::new(r#"{"type":"PONG"}"#.to_owned()),
                                );
                            }

                            let cb = inner
                                .on_client_message
                                .read()
                                .expect("callback lock poisoned")
                                .clone();
                            if let Some(callback) = cb {
                                callback(Value::Object(obj));
                            }
                        }
                    }
                }
            }
        }
    }

    inner
        .clients
        .lock()
        .expect("clients lock poisoned")
        .remove(&client_id);
    if !closed.load(Ordering::Acquire) {
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            socket.send(Message::Close(None)),
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Unit & Localhost Integration Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    #[test]
    fn opt_in_invariant_does_not_bind_socket() {
        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            ..Default::default()
        });
        assert!(!srv.is_running());
        assert_eq!(srv.pin().len(), 6);
        assert!(srv.pin().chars().all(|c| c.is_ascii_digit()));
        assert_eq!(srv.token().len(), 22);
        assert!(!srv.broadcast_state(json!({"lat": 1.0, "lng": 2.0}), true));
        let info = srv.get_connection_info();
        assert!(!info.is_running);
        assert_eq!(info.client_count, 0);
    }

    #[test]
    fn qr_encoder_v1_through_v6_and_svg_allowlist() {
        let payloads = [
            ("v1", "http://10.0.0.1", 21usize),
            ("v2", "http://192.168.1.50:8766/ab", 25usize),
            ("v3", "http://192.168.1.50:8766/companion?token=abcdefgh", 29usize),
            ("v4", "http://192.168.100.255:8766/companion?token=abcdefghijklmnopqrstuv", 33usize),
            ("v5", &"a".repeat(90), 37usize),
            ("v6", &"b".repeat(134), 41usize),
        ];

        for (label, text, expected_dim) in payloads {
            let mat = generate_qr_matrix(text).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(mat.len(), expected_dim, "{label} row count");
            assert!(mat.iter().all(|r| r.len() == expected_dim), "{label} col count");
            // Top-left 7x7 finder outer ring dark
            for i in 0..7 {
                assert!(mat[0][i], "{label} finder top");
                assert!(mat[6][i], "{label} finder bottom");
                assert!(mat[i][0], "{label} finder left");
                assert!(mat[i][6], "{label} finder right");
            }
            // Dark module at (4*version + 9, 8)
            let version = (expected_dim - 17) / 4;
            assert!(mat[4 * version + 9][8], "{label} dark module");

            let svg = generate_qr_svg(text).expect("svg");
            assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
            assert!(svg.contains("<rect "));
            assert!(svg.contains("<path d=\"M"));
            assert!(svg.contains("shape-rendering=\"crispEdges\""));
            assert!(svg.contains("aria-label=\"LAN Companion QR\""));
            assert!(!svg.contains("<script"));
        }

        let too_long = "x".repeat(135);
        assert!(generate_qr_matrix(&too_long).is_err());
    }

    #[test]
    fn detect_lan_ipv4_returns_valid_ipv4() {
        let ip = detect_lan_ipv4();
        let parsed: Ipv4Addr = ip.parse().expect("must be valid IPv4");
        assert!(!parsed.is_unspecified());
        assert!(!parsed.is_multicast());
        assert!(!parsed.is_link_local());

        // Byte-order invariant matches Win32 IPAddr / in_addr layout.
        let raw_ip = u32::from_ne_bytes([192, 168, 1, 23]);
        assert_eq!(Ipv4Addr::from(raw_ip.to_ne_bytes()), Ipv4Addr::new(192, 168, 1, 23));

        assert!(!is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2))));
        assert!(!is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(!is_usable_lan_ipv4(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
        assert!(is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 23))));
        assert!(is_usable_lan_ipv4(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));

        assert_eq!(resolve_lan_bind_host(None), "0.0.0.0");
        assert_eq!(resolve_lan_bind_host(Some("")), "0.0.0.0");
        assert_eq!(resolve_lan_bind_host(Some("   ")), "0.0.0.0");
        assert_eq!(resolve_lan_bind_host(Some(" 127.0.0.1 ")), "127.0.0.1");
    }

    fn assert_lan_headers(headers: &reqwest::header::HeaderMap) {
        assert_eq!(headers.get("cache-control").and_then(|v| v.to_str().ok()), Some("no-store"));
        assert_eq!(
            headers.get("x-content-type-options").and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            headers.get("referrer-policy").and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        assert_eq!(
            headers.get("connection").and_then(|v| v.to_str().ok()),
            Some("close")
        );
    }

    async fn raw_ws_connect(
        port: u16,
        path: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<BufReader<TcpStream>, (u16, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("tcp connect");
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await.expect("write ws req");

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.expect("status line");
        let status_code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let mut hdr_block = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("hdr line");
            if line.trim().is_empty() {
                break;
            }
            hdr_block.push_str(&line);
        }

        if status_code == 101 {
            Ok(reader)
        } else {
            Err((status_code, hdr_block))
        }
    }

    async fn raw_ws_send_masked(stream: &mut BufReader<TcpStream>, opcode: u8, payload: &[u8]) {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.push(0x80 | (opcode & 0x0F));
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | (len as u8));
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        frame.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i & 3]);
        }
        stream.get_mut().write_all(&frame).await.expect("write ws frame");
    }

    async fn raw_ws_recv_frame(stream: &mut BufReader<TcpStream>) -> Option<(u8, Vec<u8>)> {
        let mut head = [0u8; 2];
        if stream.read_exact(&mut head).await.is_err() {
            return None;
        }
        let opcode = head[0] & 0x0F;
        let masked = (head[1] & 0x80) != 0;
        let mut len = (head[1] & 0x7F) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).await.ok()?;
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).await.ok()?;
            len = u64::from_be_bytes(ext) as usize;
        }
        let mut mask = [0u8; 4];
        if masked {
            stream.read_exact(&mut mask).await.ok()?;
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut payload).await.ok()?;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Some((opcode, payload))
    }

    #[tokio::test]
    async fn lan_http_auth_rate_limit_precedence_and_ws_lifecycle() {
        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            max_hz: 15.0,
            max_clients: 2,
            advertised_host: Some("127.0.0.1".to_owned()),
        });
        let info = srv.start().await.expect("start lan");
        assert!(info.is_running);
        assert_ne!(info.port, 0);
        let base = format!("http://127.0.0.1:{}", info.port);
        let client = reqwest::Client::new();

        // 1. /health
        let r_health = client.get(format!("{base}/health")).send().await.expect("health");
        assert_eq!(r_health.status(), 200);
        assert_lan_headers(r_health.headers());
        let j_health: Value = r_health.json().await.expect("json");
        assert_eq!(j_health, json!({"ok": true, "clients": 0}));

        // 2. GET / strict CSP + no token leaked
        let r_root = client.get(format!("{base}/")).send().await.expect("root");
        assert_eq!(r_root.status(), 200);
        assert_lan_headers(r_root.headers());
        let csp = r_root
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("default-src 'self'"));
        let root_body = r_root.text().await.expect("root text");
        assert!(!root_body.contains(&info.token));

        // 3. /companion & /qr.svg & /ws auth checks + query precedence over header
        for path in ["/companion", "/qr.svg", "/ws"] {
            let r_miss = client.get(format!("{base}{path}")).send().await.expect("miss");
            assert_eq!(r_miss.status(), 401, "{path} missing");
            assert_lan_headers(r_miss.headers());

            let r_wrong = client
                .get(format!("{base}{path}?token=wrong_tok"))
                .send()
                .await
                .expect("wrong");
            assert_eq!(r_wrong.status(), 403, "{path} wrong");
            assert_lan_headers(r_wrong.headers());
        }

        let r_prec = client
            .get(format!("{base}/companion?token=wrong_tok"))
            .header(AUTHORIZATION, format!("Bearer {}", info.token))
            .send()
            .await
            .expect("prec");
        assert_eq!(r_prec.status(), 403);

        // Plain HTTP GET /ws/<valid> -> 426 Upgrade Required + Upgrade: websocket
        let r_plain_ws = client
            .get(format!("{base}/ws/{}", info.token))
            .send()
            .await
            .expect("plain ws");
        assert_eq!(r_plain_ws.status(), 426);
        assert_lan_headers(r_plain_ws.headers());
        assert_eq!(
            r_plain_ws.headers().get("upgrade").and_then(|v| v.to_str().ok()),
            Some("websocket")
        );

        // Valid /companion via query & header
        let r_comp = client
            .get(format!("{base}/companion?token={}", info.token))
            .send()
            .await
            .expect("comp");
        assert_eq!(r_comp.status(), 200);
        assert_lan_headers(r_comp.headers());
        assert!(
            r_comp
                .headers()
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("ws:")
        );
        let comp_html = r_comp.text().await.expect("comp html");
        assert!(comp_html.contains("Theo dõi"));
        assert!(comp_html.contains("Follow"));

        // Valid /qr.svg via header
        let r_qr = client
            .get(format!("{base}/qr.svg"))
            .header(AUTHORIZATION, format!("Bearer {}", info.token))
            .send()
            .await
            .expect("qr");
        assert_eq!(r_qr.status(), 200);
        assert_lan_headers(r_qr.headers());
        assert!(
            r_qr.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("image/svg+xml")
        );
        assert!(r_qr.text().await.expect("svg").contains("<svg "));

        // 4. POST /auth/pin: malformed JSON -> 400 (no penalty), 5 wrong -> 403, 6th even valid -> 429
        let r_bad_json = client
            .post(format!("{base}/auth/pin"))
            .header("Content-Type", "application/json")
            .body("{not_json")
            .send()
            .await
            .expect("bad json");
        assert_eq!(r_bad_json.status(), 400);
        assert_lan_headers(r_bad_json.headers());

        let wrong_pin = if info.pin == "000000" { "999999" } else { "000000" };
        for _ in 0..5 {
            let r_w = client
                .post(format!("{base}/auth/pin"))
                .json(&json!({"pin": wrong_pin}))
                .send()
                .await
                .expect("wrong pin");
            assert_eq!(r_w.status(), 403);
        }
        let r_locked = client
            .post(format!("{base}/auth/pin"))
            .json(&json!({"pin": info.pin}))
            .send()
            .await
            .expect("locked");
        assert_eq!(r_locked.status(), 429);
        assert_lan_headers(r_locked.headers());

        // Rotate clears failure rate limit and issues new pin + token
        let (new_pin, new_tok) = srv.rotate_credentials();
        let r_ok = client
            .post(format!("{base}/auth/pin"))
            .json(&json!({"pin": new_pin}))
            .send()
            .await
            .expect("pin ok");
        assert_eq!(r_ok.status(), 200);
        let j_ok: Value = r_ok.json().await.expect("pin json");
        assert_eq!(j_ok["ok"], true);
        assert_eq!(j_ok["token"], new_tok);
        assert_eq!(j_ok["companion_path"], format!("/companion?token={new_tok}"));

        // 5. WebSocket: wrong token -> 403, valid -> 101 + immediate state + Ping->Pong + client msg + max_clients 503
        let ws_err = raw_ws_connect(info.port, "/ws/wrong_tok", &[]).await;
        assert_eq!(ws_err.unwrap_err().0, 403);

        // Seed initial state before connecting
        assert!(srv.broadcast_state(json!({"lat": 12.5, "lng": -34.5}), true));

        let received_count = Arc::new(AtomicUsize::new(0));
        let received_notify = Arc::new(Notify::new());
        let received_clone = Arc::clone(&received_count);
        let received_notify_cb = Arc::clone(&received_notify);
        srv.set_on_client_message(Some(Arc::new(move |val| {
            if val.get("hello").and_then(|v| v.as_str()) == Some("world") {
                received_clone.fetch_add(1, Ordering::AcqRel);
                received_notify_cb.notify_waiters();
            }
        })));

        let mut ws1 = raw_ws_connect(info.port, &format!("/ws/{new_tok}"), &[])
            .await
            .expect("ws1 connect");
        let (op0, p0) = raw_ws_recv_frame(&mut ws1).await.expect("initial state");
        assert_eq!(op0, 0x1);
        let init_json: Value = serde_json::from_slice(&p0).expect("state json");
        assert_eq!(init_json["type"], "STATE");
        assert_eq!(init_json["lat"], 12.5);

        // Ping -> single Pong
        raw_ws_send_masked(&mut ws1, 0x9, b"ping42").await;
        let (op_pong, p_pong) = raw_ws_recv_frame(&mut ws1).await.expect("pong");
        assert_eq!(op_pong, 0xA);
        assert_eq!(p_pong, b"ping42");

        // Send JSON object frame -> triggers on_client_message
        let recv_fut = received_notify.notified();
        raw_ws_send_masked(&mut ws1, 0x1, br#"{"hello":"world"}"#).await;
        if received_count.load(Ordering::Acquire) < 1 {
            tokio::time::timeout(Duration::from_secs(10), recv_fut)
                .await
                .expect("on_client_message timeout");
        }
        assert_eq!(received_count.load(Ordering::Acquire), 1);

        // 2nd client allowed (max_clients = 2), 3rd client rejected with 503
        let _ws2 = raw_ws_connect(info.port, &format!("/ws/{new_tok}"), &[])
            .await
            .expect("ws2 connect");
        let ws3_err = raw_ws_connect(info.port, &format!("/ws/{new_tok}"), &[]).await;
        assert_eq!(ws3_err.unwrap_err().0, 503);

        // Coalescer: 5 rapid updates collapse cleanly and deliver latest value
        for i in 1..=5 {
            assert!(srv.broadcast_state(json!({"lat": i as f64, "lng": 0.0}), false));
        }
        let mut last_lat = None;
        let coalesce_dl = Instant::now() + Duration::from_secs(10);
        while last_lat != Some(5.0) {
            let rem = coalesce_dl.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "timed out waiting for coalesced lat=5.0 (last={last_lat:?})");
            let Some((op, data)) = tokio::time::timeout(rem, raw_ws_recv_frame(&mut ws1))
                .await
                .expect("coalesced state timeout")
            else {
                break;
            };
            if op == 0x1
                && let Ok(v) = serde_json::from_slice::<Value>(&data)
            {
                last_lat = v.get("lat").and_then(|x| x.as_f64());
            }
        }
        assert_eq!(last_lat, Some(5.0));

        srv.stop().await;
        assert!(!srv.is_running());
    }

    #[tokio::test]
    async fn slow_client_never_blocks_fast_client_and_over_256kib_evicts() {
        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            max_hz: 15.0,
            max_clients: 4,
            advertised_host: Some("127.0.0.1".to_owned()),
        });
        let info = srv.start().await.expect("start");

        // Unit check: stalled handle with >256 KiB pending is evicted immediately on flush_to_clients
        let (tx_slow, _rx_slow) = mpsc::channel::<Arc<String>>(64);
        let (tx_fast, mut rx_fast) = mpsc::channel::<Arc<String>>(64);
        let slow_notify = Arc::new(Notify::new());
        let fast_notify = Arc::new(Notify::new());
        let slow_closed = Arc::new(AtomicBool::new(false));
        let fast_closed = Arc::new(AtomicBool::new(false));
        {
            let mut clients = srv.inner.clients.lock().expect("clients");
            clients.insert(
                9001,
                WsClientHandle {
                    tx: tx_slow,
                    queued_bytes: Arc::new(AtomicUsize::new(260_000)),
                    close_notify: Arc::clone(&slow_notify),
                    closed: Arc::clone(&slow_closed),
                },
            );
            clients.insert(
                9002,
                WsClientHandle {
                    tx: tx_fast,
                    queued_bytes: Arc::new(AtomicUsize::new(0)),
                    close_notify: Arc::clone(&fast_notify),
                    closed: Arc::clone(&fast_closed),
                },
            );
        }
        let pad_20k = "X".repeat(20 * 1024);
        assert!(srv.broadcast_state(json!({"evict_test": true, "pad": pad_20k}), true));
        assert!(slow_closed.load(Ordering::Acquire));
        assert!(!fast_closed.load(Ordering::Acquire));
        assert_eq!(srv.client_count(), 1, "slow >256 KiB handle must be evicted");
        let delivered = rx_fast.try_recv().expect("healthy handle receives frame");
        assert!(delivered.contains("evict_test"));
        srv.inner.clients.lock().expect("clients").clear();

        // Live WS check: unreading slow WS client never blocks fast WS client
        let _slow = raw_ws_connect(info.port, &format!("/ws/{}", info.token), &[])
            .await
            .expect("slow ws");
        let mut fast = raw_ws_connect(info.port, &format!("/ws/{}", info.token), &[])
            .await
            .expect("fast ws");

        // Drain the guaranteed initial STATE snapshot before sending seq=0..8
        let (op_init, init_data) =
            tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut fast))
                .await
                .expect("initial state timeout")
                .expect("initial state frame");
        assert_eq!(op_init, 0x1);
        let init_v: Value = serde_json::from_slice(&init_data).expect("init json");
        assert_eq!(init_v["evict_test"], true);

        let big_pad = "Z".repeat(70 * 1024);
        for seq in 0..8 {
            assert!(srv.broadcast_state(json!({"seq": seq, "pad": big_pad}), true));
            let (op, data) =
                tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut fast))
                    .await
                    .expect("fast recv timeout")
                    .expect("fast recv frame");
            assert_eq!(op, 0x1);
            let v: Value = serde_json::from_slice(&data).expect("json");
            assert_eq!(v["seq"], seq);
        }

        srv.broadcast_state(json!({"alive": true}), true);
        let (op, data) =
            tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut fast))
                .await
                .expect("alive timeout")
                .expect("alive frame");
        assert_eq!(op, 0x1);
        let v: Value = serde_json::from_slice(&data).expect("json");
        assert_eq!(v["alive"], true);

        srv.stop().await;
    }

    #[test]
    fn start_in_background_and_stop_background_work_synchronously() {
        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            advertised_host: Some("127.0.0.1".to_owned()),
            ..Default::default()
        });
        let info = srv.start_in_background().expect("bg start");
        assert!(info.is_running);
        assert_ne!(info.port, 0);
        assert!(srv.is_running());

        let resp = reqwest::blocking::get(format!("http://127.0.0.1:{}/health", info.port))
            .expect("blocking health");
        assert_eq!(resp.status(), 200);

        srv.stop_background();
        assert!(!srv.is_running());
    }

    #[tokio::test]
    async fn hd_map_api_endpoints_ws_route_and_pins_snapshot() {
        use axum::Router as MockRouter;
        use axum::routing::get as mock_get;
        use gh_core::multimap::{DetailV2Meta, MapMeta, WaypointEntry};
        use std::collections::BTreeMap;

        let seq = std::process::id();
        let tmp_root = std::env::temp_dir().join(format!("gh_lan_hd_test_{seq}"));
        let _ = std::fs::remove_dir_all(&tmp_root);
        let tiles_dir = tmp_root.join("tiles");
        let icons_dir = tmp_root.join("icons");
        let ug_dir = tmp_root.join("ug");
        std::fs::create_dir_all(&tiles_dir).unwrap();

        // Upstream mock server serving 1 WebP tile
        let fake_webp: Vec<u8> = b"RIFF\x18\x00\x00\x00WEBPVP8 \x0c\x00\x00\x00\x00\x00\x00\x00".to_vec();
        let fake_webp_srv = fake_webp.clone();
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_app = MockRouter::new().route(
            "/2/ver1/1_2_N1.webp",
            mock_get(move || {
                let body = fake_webp_srv.clone();
                async move { ([(CONTENT_TYPE, "image/webp")], body).into_response() }
            }),
        );
        let (up_tx, up_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(upstream_listener, upstream_app)
                .with_graceful_shutdown(async {
                    let _ = up_rx.await;
                })
                .await;
        });

        let mut maps = BTreeMap::new();
        maps.insert(
            2,
            MapMeta {
                id: 2,
                name: "Teyvat".to_owned(),
                detail_v2: Some(DetailV2Meta {
                    map_version: "ver1".to_owned(),
                    total_size: [4096.0, 4096.0],
                    origin: [2048.0, 2048.0],
                    min_zoom: -2,
                    max_zoom: 0,
                    ..Default::default()
                }),
                waypoints: vec![WaypointEntry {
                    id: 1,
                    is_primary: true,
                    x_pos: 10.0,
                    y_pos: 20.0,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let maps_db = Arc::new(MultimapDb {
            maps,
            ..Default::default()
        });
        let ug_provider = Arc::new(
            UndergroundOverlayProvider::new(
                Some(ug_dir),
                Some(Arc::clone(&maps_db)),
                Duration::from_secs(1),
                true,
                true,
                1,
            )
            .unwrap(),
        );
        let assets = LanMapAssets {
            maps_db,
            tile_cache_dir: tiles_dir.clone(),
            ug_provider,
            guide_fetcher: PinGuideFetcher::new(Arc::new(|| "en".to_owned())),
            icon_cache: PinIconCache::new(
                icons_dir.clone(),
                Arc::new(HoYoLABLabelCatalog::new()),
                Duration::from_secs(2),
            ),
        };

        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            max_clients: 4,
            advertised_host: Some("127.0.0.1".to_owned()),
            ..Default::default()
        });
        srv.set_map_assets(assets);
        srv.set_tile_base_for_test(format!("http://{upstream_addr}"));
        let info = srv.start().await.expect("start lan");
        let base = format!("http://127.0.0.1:{}", info.port);
        let cli = reqwest::Client::new();

        // 1. Auth enforcement on /api/map_meta & /api/tile
        assert_eq!(
            cli.get(format!("{base}/api/map_meta/2")).send().await.unwrap().status(),
            401
        );
        assert_eq!(
            cli.get(format!("{base}/api/tile/2/-1/1/2?token=bad")).send().await.unwrap().status(),
            403
        );

        let meta_res: Value = cli
            .get(format!("{base}/api/map_meta/2?token={}&lang=vi", info.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(meta_res["ok"], true);
        assert_eq!(meta_res["waypoints"].as_array().unwrap().len(), 1);

        // 2. /api/tile fetches upstream, caches on disk, sets private max-age=86400
        let r_tile = cli
            .get(format!("{base}/api/tile/2/-1/1/2?token={}", info.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r_tile.status(), 200);
        assert_eq!(
            r_tile.headers().get("cache-control").and_then(|v| v.to_str().ok()),
            Some("private, max-age=86400")
        );
        assert_eq!(r_tile.bytes().await.unwrap().as_ref(), fake_webp.as_slice());
        assert!(tiles_dir.join("2").join("-1").join("1_2.webp").is_file());

        // 2b. /api/marker_asset/{key} serves cached official HoYoLAB sprites and rejects unknown keys
        let fake_png = b"\x89PNG\r\n\x1a\nFAKE_MARKER_BG".to_vec();
        std::fs::create_dir_all(&icons_dir).unwrap();
        std::fs::write(icons_dir.join("ui_marker_bg_normal.img"), &fake_png).unwrap();
        let r_ma = cli
            .get(format!("{base}/api/marker_asset/marker_bg_normal?token={}", info.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r_ma.status(), 200);
        assert_eq!(r_ma.bytes().await.unwrap().as_ref(), fake_png.as_slice());
        let r_bad_ma = cli
            .get(format!("{base}/api/marker_asset/not_a_real_key?token={}", info.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r_bad_ma.status(), 404);

        // 3. Seed state, route snapshot, and pins snapshot -> HELLO receives ROUTE_UPDATED & PINS_SNAPSHOT
        assert!(srv.broadcast_state(json!({"lat": 5.0, "lng": 6.0}), true));
        srv.update_route_snapshot(json!({"active": true, "title": "Test Route", "active_section": 0, "section_count": 2}));
        let rev = srv.update_pins_snapshot(json!({
            "map_id": 2,
            "names": ["Chest"],
            "categories": [{"id": 17, "name": "Chest", "count": 1}],
            "pins": [["999", 5.5, 6.5, 17, null, 0, 0, 0]]
        }));
        assert_eq!(rev, 1);

        let seen_actions = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_notify = Arc::new(Notify::new());
        let seen_clone = Arc::clone(&seen_actions);
        let seen_notify_cb = Arc::clone(&seen_notify);
        srv.set_on_client_message(Some(Arc::new(move |val| {
            if let Some(t) = val.get("type").and_then(Value::as_str) {
                seen_clone.lock().unwrap().push(t.to_owned());
                seen_notify_cb.notify_waiters();
            }
        })));

        let mut ws = raw_ws_connect(info.port, &format!("/ws/{}", info.token), &[])
            .await
            .expect("ws connect");
        let (op_init, p_init) = raw_ws_recv_frame(&mut ws).await.unwrap();
        assert_eq!(op_init, 0x1);
        assert_eq!(serde_json::from_slice::<Value>(&p_init).unwrap()["type"], "STATE");

        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"HELLO"}"#).await;
        let mut saw_route = false;
        let mut saw_pins = false;
        for _ in 0..4 {
            if let Ok(Some((op, data))) = tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut ws)).await {
                assert_eq!(op, 0x1);
                let v: Value = serde_json::from_slice(&data).unwrap();
                if v["type"] == "ROUTE_UPDATED" {
                    saw_route = true;
                } else if v["type"] == "PINS_SNAPSHOT" {
                    saw_pins = true;
                }
                if saw_route && saw_pins {
                    break;
                }
            }
        }
        assert!(saw_route && saw_pins, "expected ROUTE_UPDATED and PINS_SNAPSHOT after HELLO");

        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"SELECT_ROUTE_SECTION","section_index":1}"#).await;
        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"MARK_PIN","action":"mark","pin_id":"999"}"#).await;
        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"SELECT_TARGET","pin_id":"999","lat":5.5,"lng":6.5}"#).await;
        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"UNDO_LAST"}"#).await;
        let act_dl = Instant::now() + Duration::from_secs(10);
        loop {
            let fut = seen_notify.notified();
            if seen_actions.lock().unwrap().len() >= 4 {
                break;
            }
            let rem = act_dl.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "timed out waiting for 4 WS client actions");
            let _ = tokio::time::timeout(rem, fut).await;
        }
        let actions = seen_actions.lock().unwrap().clone();
        assert!(actions.contains(&"SELECT_ROUTE_SECTION".to_owned()));
        assert!(actions.contains(&"MARK_PIN".to_owned()));
        assert!(actions.contains(&"SELECT_TARGET".to_owned()));
        assert!(actions.contains(&"UNDO_LAST".to_owned()));

        srv.stop().await;
        let _ = up_tx.send(());
        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[tokio::test]
    async fn large_pins_snapshot_and_ping_pong_work_without_eviction() {
        let srv = LanCompanionServer::new(LanCompanionConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            max_clients: 4,
            advertised_host: Some("127.0.0.1".to_owned()),
            ..Default::default()
        });
        let info = srv.start().await.expect("start lan");

        let mut ws = raw_ws_connect(info.port, &format!("/ws/{}", info.token), &[])
            .await
            .expect("ws connect");

        // JSON PING -> JSON PONG
        raw_ws_send_masked(&mut ws, 0x1, br#"{"type":"PING"}"#).await;
        let (op_pong, p_pong) = tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut ws))
            .await
            .expect("pong timeout")
            .expect("pong frame");
        assert_eq!(op_pong, 0x1);
        assert_eq!(serde_json::from_slice::<Value>(&p_pong).unwrap()["type"], "PONG");

        // >300 KiB PINS_SNAPSHOT delivered cleanly via watch channel without evicting client
        let huge_pins: Vec<Value> = (0..10500)
            .map(|i| json!([format!("p{i}"), 10.0, 20.0, 17, null, 0, 0, 0]))
            .collect();
        let rev = srv.update_pins_snapshot(json!({
            "map_id": 2,
            "names": ["Common Chest With Long Description Padding Text"],
            "categories": [{"id": 17, "name": "Chest", "count": 5500}],
            "pins": huge_pins,
        }));
        assert_eq!(rev, 1);

        let (op_snap, p_snap) = tokio::time::timeout(Duration::from_secs(10), raw_ws_recv_frame(&mut ws))
            .await
            .expect("snapshot timeout")
            .expect("snapshot frame");
        assert_eq!(op_snap, 0x1);
        assert!(p_snap.len() > 300_000, "expected >300 KiB snapshot, got {}", p_snap.len());
        let snap_v: Value = serde_json::from_slice(&p_snap).unwrap();
        assert_eq!(snap_v["type"], "PINS_SNAPSHOT");
        assert_eq!(srv.client_count(), 1);

        srv.stop().await;
    }

    #[test]
    fn sniff_image_mime_recognizes_gif_and_sets_content_type() {
        assert_eq!(sniff_image_mime(b"GIF87a\x01\x00"), "image/gif");
        assert_eq!(sniff_image_mime(b"GIF89a\x01\x00"), "image/gif");

        let resp = image_response(b"GIF89a\x01\x00".to_vec(), "private, max-age=3600");
        assert_eq!(
            resp.headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/gif")
        );
    }

    #[test]
    fn companion_ui_includes_visible_pin_sheet_css_and_unified_route_pin_pool() {
        let html = render_companion_html("tok123");
        assert!(
            html.contains(".sheet{") && html.contains("z-index:30"),
            "pin sheet overlay CSS must float visibly above the map canvas"
        );
        assert!(
            html.contains("function getEffectivePins()"),
            "LAN Companion must merge activeRoute stops into the official HoYoLAB marker & hit-test pool"
        );
    }
}
