//! Headless HUD / Live Radar / M-Map visual compositor for `gh-sim`.
//!
//! Composites production `gh-hud` premultiplied RGBA overlays directly onto recorded 1440p
//! `FrameBuf` frames with Porter-Duff source-over alpha blending (zero Win32 HWND, zero network).

use gh_core::frame::{FrameBuf, PixelFormat, RectI32};
use gh_core::multimap::MultimapDb;
use gh_core::traits::UndergroundOverlaySource;
use gh_hud::radar_map::{
    HeadlessRadarRenderer, RadarAssetSource, RadarScene, compute_default_radar_rect,
};
use gh_net::hd_assets::{HoyolabHdTileProvider, UndergroundOverlayProvider};
use image::{ImageBuffer, Rgba};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Offline synchronous [`RadarAssetSource`] backed strictly by a session's local `assets_cache/`.
pub struct OfflineRadarAssets {
    pub maps_db: Arc<MultimapDb>,
    pub hd_provider: Arc<HoyolabHdTileProvider>,
    pub ug_provider: Arc<UndergroundOverlayProvider>,
}

impl RadarAssetSource for OfflineRadarAssets {
    fn maps_db(&self) -> Arc<MultimapDb> {
        Arc::clone(&self.maps_db)
    }

    fn tile_generation(&self) -> u64 {
        self.hd_provider.tile_generation()
    }

    fn icon_generation(&self) -> u64 {
        0
    }

    fn get_color_tile_bytes(&self, map_id: i32, zoom: i32, tx: i32, ty: i32) -> Option<Vec<u8>> {
        self.hd_provider
            .get_color_tile_bytes(map_id, zoom, tx, ty, Some(false))
    }

    fn prefetch_color_tiles_async(&self, _map_id: i32, _tiles: &[(i32, i32, i32)]) {
        // Offline replay: never trigger background downloads.
    }

    fn get_ug_overlay_bytes(&self, map_id: i32, _group_id: i32, floor_id: i32) -> Option<Vec<u8>> {
        self.ug_provider
            .get_overlay_bytes(map_id, &floor_id.to_string(), false)
    }

    fn get_icon_bytes_or_request(&self, _map_id: i32, _label_id: i32) -> Option<Vec<u8>> {
        None
    }
}

/// Alpha-blends a premultiplied RGBA8 overlay buffer onto a `Bgra8` or `Bgr8` target frame in-place.
pub fn blend_premul_rgba_onto_bgra(
    dst: &mut FrameBuf,
    src_w: u32,
    src_h: u32,
    premul_rgba: &[u8],
    dst_x: i32,
    dst_y: i32,
    opacity: f32,
) {
    let bpp = match dst.format {
        PixelFormat::Bgra8 => 4usize,
        PixelFormat::Bgr8 => 3usize,
        _ => return,
    };
    if src_w == 0 || src_h == 0 {
        return;
    }
    let expected = (src_w as usize)
        .saturating_mul(src_h as usize)
        .saturating_mul(4);
    if premul_rgba.len() < expected {
        return;
    }
    let op = opacity.clamp(0.0, 1.0);
    let dw = dst.width as i32;
    let dh = dst.height as i32;

    for sy in 0..src_h as i32 {
        let ty = dst_y + sy;
        if ty < 0 || ty >= dh {
            continue;
        }
        let dst_row = ty as usize * dst.stride;
        let src_row = sy as usize * src_w as usize * 4;
        for sx in 0..src_w as i32 {
            let tx = dst_x + sx;
            if tx < 0 || tx >= dw {
                continue;
            }
            let so = src_row + sx as usize * 4;
            let sa_raw = premul_rgba[so + 3];
            if sa_raw == 0 {
                continue;
            }
            let sr = premul_rgba[so] as f32 * op;
            let sg = premul_rgba[so + 1] as f32 * op;
            let sb = premul_rgba[so + 2] as f32 * op;
            let inv_a = 1.0 - (sa_raw as f32 / 255.0) * op;

            let doff = dst_row + tx as usize * bpp;
            let db = dst.data[doff] as f32;
            let dg = dst.data[doff + 1] as f32;
            let dr = dst.data[doff + 2] as f32;

            dst.data[doff] = (sb + db * inv_a).clamp(0.0, 255.0).round() as u8;
            dst.data[doff + 1] = (sg + dg * inv_a).clamp(0.0, 255.0).round() as u8;
            dst.data[doff + 2] = (sr + dr * inv_a).clamp(0.0, 255.0).round() as u8;
            if bpp == 4 {
                dst.data[doff + 3] = 255;
            }
        }
    }
}

/// Saves a `Bgra8` or `Bgr8` `FrameBuf` as a standard RGBA PNG file.
pub fn save_bgra_png(frame: &FrameBuf, path: &Path) -> Result<(), String> {
    let bpp = match frame.format {
        PixelFormat::Bgra8 => 4usize,
        PixelFormat::Bgr8 => 3usize,
        other => return Err(format!("expected Bgra8 or Bgr8 frame, got {other:?}")),
    };
    let w = frame.width;
    let h = frame.height;
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(w, h);
    for y in 0..h as usize {
        let row = y * frame.stride;
        for x in 0..w as usize {
            let o = row + x * bpp;
            img.put_pixel(
                x as u32,
                y as u32,
                Rgba([frame.data[o + 2], frame.data[o + 1], frame.data[o], 255]),
            );
        }
    }
    img.save(path)
        .map_err(|e| format!("failed to write composite PNG {}: {e}", path.display()))
}

/// Composites a live-equivalent bottom-right Radar Map directly onto `bgra_frame`.
#[allow(clippy::too_many_arguments)]
pub fn composite_radar_onto_frame(
    renderer: &mut HeadlessRadarRenderer,
    bgra_frame: &mut FrameBuf,
    map_id: i32,
    world_x: f64,
    world_y: f64,
    body_heading: Option<f64>,
    camera_heading: Option<f64>,
    is_underground: bool,
    floor_id: Option<i32>,
    floor_short: &str,
) -> bool {
    let game_rect = RectI32 {
        x: 0,
        y: 0,
        width: bgra_frame.width as i32,
        height: bgra_frame.height as i32,
    };
    let rect = compute_default_radar_rect(game_rect);
    let scene = RadarScene {
        game_rect,
        map_id,
        world_x,
        world_y,
        vx: 0.0,
        vy: 0.0,
        body_heading,
        camera_heading,
        is_underground,
        group_id: None,
        floor_id,
        floor_short: floor_short.to_owned(),
        pins: Arc::new(Vec::new()),
        excluded_ids: Arc::new(HashSet::new()),
        target: None,
        route_anchor: None,
        route_legs: Arc::new(Vec::new()),
        capture_visible: false,
        edit_hint: String::new(),
    };
    let Some((rw, rh, rgba)) =
        renderer.render_scene_rgba(&scene, rect.width as u32, rect.height as u32, 0.0, false)
    else {
        return false;
    };
    blend_premul_rgba_onto_bgra(bgra_frame, rw, rh, rgba, rect.x, rect.y, 0.94);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premul_rgba_blends_cleanly_onto_bgra_with_bounds_clipping() {
        let mut dst = FrameBuf::new_zeroed(4, 4, PixelFormat::Bgra8);
        for px in dst.data.as_chunks_mut::<4>().0 {
            *px = [100, 100, 100, 255];
        }
        // 2x2 opaque red premultiplied RGBA
        let mut overlay = Vec::new();
        for _ in 0..4 {
            overlay.extend_from_slice(&[200, 0, 0, 255]);
        }
        blend_premul_rgba_onto_bgra(&mut dst, 2, 2, &overlay, 3, 3, 0.5);
        // Pixel (3,3) is blended: R = 200*0.5 + 100*0.5 = 150, G/B = 50
        let o = 3 * dst.stride + 3 * 4;
        assert_eq!(&dst.data[o..o + 4], &[50, 50, 150, 255]);
        // Pixel (0,0) untouched
        assert_eq!(&dst.data[0..4], &[100, 100, 100, 255]);
    }

    #[test]
    fn premul_rgba_blends_cleanly_onto_bgr8() {
        let mut dst = FrameBuf::new_zeroed(2, 2, PixelFormat::Bgr8);
        for px in dst.data.as_chunks_mut::<3>().0 {
            *px = [100, 100, 100];
        }
        // 1x1 opaque blue premultiplied RGBA
        let overlay = [0u8, 0, 200, 255];
        blend_premul_rgba_onto_bgra(&mut dst, 1, 1, &overlay, 1, 0, 0.5);
        // Pixel (1,0) in BGR8: B = 200*0.5 + 100*0.5 = 150, G/R = 50
        let o = 3;
        assert_eq!(&dst.data[o..o + 3], &[150, 50, 50]);
        assert_eq!(&dst.data[0..3], &[100, 100, 100]);
    }
}
