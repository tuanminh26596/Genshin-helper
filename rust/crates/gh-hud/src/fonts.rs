//! Direct `%WINDIR%\Fonts` loading via `fontdb::Database::load_font_file` +
//! `cosmic-text::FontSystem::new_with_locale_and_db` (zero full-system scan on standard Windows),
//! Vietnamese UTF-8 shaping (`Shaping::Advanced`), and premultiplied `tiny-skia` rasterization.

use cosmic_text::{
    Align, Attrs, Buffer, Color, Ellipsize, Family, FontSystem, Metrics, Shaping, Style,
    SwashCache, SwashContent, Weight, Wrap, fontdb,
};
#[cfg(windows)]
use gh_platform::locale::detect_windows_locale_name;
#[cfg(target_os = "linux")]
fn detect_windows_locale_name() -> Option<String> {
    gh_platform::locale::detect_linux_locale()
}
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tiny_skia::{PixmapMut, PremultipliedColorU8};

type FontBytesPool = OnceLock<Mutex<HashMap<PathBuf, Vec<Arc<Vec<u8>>>>>>;
static TARGET_FONT_BYTES_CACHE: FontBytesPool = OnceLock::new();

fn load_cached_target_font_bytes(fonts_dir: &Path) -> Vec<Arc<Vec<u8>>> {
    let cache = TARGET_FONT_BYTES_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(fonts_dir)
    {
        return hit.clone();
    }
    let mut loaded = Vec::new();
    for file_name in TARGET_FONT_FILES {
        let path = fonts_dir.join(file_name);
        if path.is_file()
            && let Ok(bytes) = std::fs::read(&path)
        {
            loaded.push(Arc::new(bytes));
        }
    }
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(fonts_dir.to_path_buf())
        .or_insert_with(|| loaded)
        .clone()
}

const TARGET_FONT_FILES: [&str; 8] = [
    "segoeui.ttf",
    "seguisb.ttf",
    "segoeuib.ttf",
    "arial.ttf",
    "arialbd.ttf",
    "tahoma.ttf",
    "tahomabd.ttf",
    "seguisym.ttf",
];

const MAX_SAFE_DRAW_COORD: f32 = 1.0e6;

#[inline]
fn is_safe_draw_coord(v: f32) -> bool {
    v.is_finite() && v.abs() <= MAX_SAFE_DRAW_COORD
}

#[inline]
fn sanitize_max_width(max_width: Option<f32>) -> Option<f32> {
    max_width
        .filter(|w| w.is_finite())
        .map(|w| w.clamp(0.0, MAX_SAFE_DRAW_COORD))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontWeightRole {
    #[default]
    Regular,
    Semibold,
    Bold,
}

impl FontWeightRole {
    pub const fn to_cosmic_weight(self) -> Weight {
        match self {
            Self::Regular => Weight::NORMAL,
            Self::Semibold => Weight::SEMIBOLD,
            Self::Bold => Weight::BOLD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl TextAlign {
    const fn to_cosmic(self) -> Align {
        match self {
            Self::Left => Align::Left,
            Self::Center => Align::Center,
            Self::Right => Align::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextEffect {
    #[default]
    None,
    Shadow {
        offset_x: i32,
        offset_y: i32,
        color: [u8; 4],
    },
    Outline {
        radius_px: i32,
        color: [u8; 4],
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub family: &'static str,
    pub weight: FontWeightRole,
    pub size_px: f32,
    pub line_height_px: f32,
    pub color: [u8; 4],
}

impl TextStyle {
    pub fn new(size_px: f32, weight: FontWeightRole, color: [u8; 4]) -> Self {
        let size = size_px.max(1.0);
        Self {
            family: "Segoe UI",
            weight,
            size_px: size,
            line_height_px: (size * 1.25).round().max(1.0),
            color,
        }
    }

    pub fn with_line_height(mut self, line_height_px: f32) -> Self {
        self.line_height_px = line_height_px.max(1.0);
        self
    }

    pub fn with_family(mut self, family: &'static str) -> Self {
        self.family = family;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextMetricsBox {
    pub width: f32,
    pub height: f32,
    pub visual_top_offset: f32,
    pub visual_height: f32,
    pub line_count: usize,
}

pub struct HudFonts {
    font_system: FontSystem,
    swash_cache: SwashCache,
    loaded_files: usize,
    used_system_fallback: bool,
    sans_serif_family: String,
    loaded_families: HashSet<String>,
}

impl HudFonts {
    /// Loads targeted `%WINDIR%\Fonts` TTFs via `fontdb::Database::load_font_file`,
    /// falling back to `load_system_fonts()` only if no target font loaded.
    pub fn new() -> Self {
        let windir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let fonts_dir = windir.join("Fonts");

        let mut db = fontdb::Database::new();
        let cached_fonts = load_cached_target_font_bytes(&fonts_dir);
        let mut loaded_files = 0usize;
        for bytes in cached_fonts {
            let src_arc: Arc<dyn AsRef<[u8]> + Sync + Send> = bytes;
            let ids = db.load_font_source(fontdb::Source::Binary(src_arc));
            if !ids.is_empty() {
                loaded_files += 1;
            }
        }

        let mut used_system_fallback = false;
        if db.is_empty() {
            db.load_system_fonts();
            used_system_fallback = true;
        }

        let mut loaded_families = HashSet::new();
        for face in db.faces() {
            for (family_name, _) in &face.families {
                loaded_families.insert(family_name.clone());
            }
        }

        let preferred_families = [
            "Segoe UI",
            "Arial",
            "Tahoma",
            "Open Sans",
            "Adwaita Sans",
            "Noto Sans",
            "Inter",
            "Ubuntu",
            "Cantarell",
            "DejaVu Sans",
            "Liberation Sans",
        ];
        let sans_serif_family = preferred_families
            .iter()
            .find(|&&f| loaded_families.contains(f))
            .map(|&f| f.to_owned())
            .or_else(|| loaded_families.iter().min().cloned())
            .unwrap_or_else(|| "Segoe UI".to_owned());
        db.set_sans_serif_family(&sans_serif_family);

        let locale = detect_windows_locale_name().unwrap_or_else(|| "en-US".to_owned());
        let font_system = FontSystem::new_with_locale_and_db(locale, db);
        let swash_cache = SwashCache::new();

        Self {
            font_system,
            swash_cache,
            loaded_files,
            used_system_fallback,
            sans_serif_family,
            loaded_families,
        }
    }

    pub const fn loaded_files(&self) -> usize {
        self.loaded_files
    }

    pub const fn used_system_fallback(&self) -> bool {
        self.used_system_fallback
    }

    pub fn sans_serif_family(&self) -> &str {
        &self.sans_serif_family
    }

    fn build_attrs<'a>(&'a self, style: &'a TextStyle) -> Attrs<'a> {
        let family = if !style.family.is_empty() && self.loaded_families.contains(style.family) {
            Family::Name(style.family)
        } else {
            Family::SansSerif
        };
        Attrs::new()
            .family(family)
            .weight(style.weight.to_cosmic_weight())
            .style(Style::Normal)
            .color(Color::rgba(
                style.color[0],
                style.color[1],
                style.color[2],
                style.color[3],
            ))
    }

    fn shape_buffer(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
        align: Align,
        wrap: Wrap,
        ellipsize: Ellipsize,
    ) -> Buffer {
        let size_px = if style.size_px.is_finite() {
            style.size_px.max(1.0)
        } else {
            1.0
        };
        let line_height_px = if style.line_height_px.is_finite() {
            style.line_height_px.max(1.0)
        } else {
            (size_px * 1.25).round().max(1.0)
        };
        let metrics = Metrics::new(size_px, line_height_px);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_wrap(wrap);
        buffer.set_ellipsize(ellipsize);
        buffer.set_size(sanitize_max_width(max_width), None);
        let attrs = self.build_attrs(&style);
        buffer.set_text(text, &attrs, Shaping::Advanced, Some(align));
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }

    fn inspect_buffer_metrics(&mut self, buffer: &Buffer) -> TextMetricsBox {
        let mut width = 0.0f32;
        let mut height = 0.0f32;
        let mut line_count = 0usize;
        let mut ink_min_y: Option<i32> = None;
        let mut ink_max_y: Option<i32> = None;

        for run in buffer.layout_runs() {
            if !is_safe_draw_coord(run.line_y) {
                continue;
            }
            line_count += 1;
            width = width.max(run.line_w);
            height = height.max(run.line_top + run.line_height);
            for glyph in run.glyphs {
                if !is_safe_draw_coord(glyph.x) || !is_safe_draw_coord(run.line_y + glyph.y) {
                    continue;
                }
                let pg = glyph.physical((0.0, run.line_y), 1.0);
                if let Some(img) = self
                    .swash_cache
                    .get_image(&mut self.font_system, pg.cache_key)
                    && img.placement.width > 0
                    && img.placement.height > 0
                {
                    let top = pg.y - img.placement.top;
                    let bottom = top + img.placement.height as i32;
                    ink_min_y = Some(ink_min_y.map_or(top, |cur| cur.min(top)));
                    ink_max_y = Some(ink_max_y.map_or(bottom, |cur| cur.max(bottom)));
                }
            }
        }

        let (visual_top_offset, visual_height) = match (ink_min_y, ink_max_y) {
            (Some(min_y), Some(max_y)) => (min_y as f32, (max_y - min_y).max(0) as f32),
            _ => (0.0, 0.0),
        };

        TextMetricsBox {
            width,
            height,
            visual_top_offset,
            visual_height,
            line_count,
        }
    }

    /// Measures single- or multi-line text using `Shaping::Advanced` and `SwashCache` tight ink bounds.
    pub fn measure_text(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
        wrap: Wrap,
        ellipsize: Ellipsize,
    ) -> TextMetricsBox {
        if text.is_empty() {
            return TextMetricsBox::default();
        }
        let buffer = self.shape_buffer(text, style, max_width, Align::Left, wrap, ellipsize);
        self.inspect_buffer_metrics(&buffer)
    }

    fn measure_single_line_width(&mut self, text: &str, style: TextStyle) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let buffer = self.shape_buffer(
            text,
            style,
            None,
            Align::Left,
            Wrap::None,
            Ellipsize::None,
        );
        buffer
            .layout_runs()
            .fold(0.0f32, |acc, run| acc.max(run.line_w))
    }

    /// Normalizes whitespace and truncates `text` on UTF-8 char boundaries with `"..."`
    /// so its shaped width fits within `max_width` (matching Python `_truncate_line`).
    pub fn truncate_line(&mut self, text: &str, style: TextStyle, max_width: f32) -> String {
        let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return String::new();
        }
        if self.measure_single_line_width(&clean, style) <= max_width {
            return clean;
        }

        let boundaries: Vec<usize> = clean
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(clean.len()))
            .collect();
        let ellipsis = "...";
        let mut lo = 1usize;
        let mut hi = boundaries.len().saturating_sub(1);
        let mut best = ellipsis.to_owned();

        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let end = boundaries[mid];
            let cand = format!("{}{}", clean[..end].trim_end(), ellipsis);
            if self.measure_single_line_width(&cand, style) <= max_width {
                best = cand;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        best
    }

    /// Greedy word wrap across `\n`-separated paragraphs with a global `max_lines` cap,
    /// ellipsizing the final allowed line if any text overflows (`src/nav_hud.py::_wrap_lines`).
    pub fn wrap_lines(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: f32,
        max_lines: usize,
    ) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }

        let normalized = text.replace('\r', "");
        let paragraphs: Vec<String> = normalized
            .split('\n')
            .filter(|p| !p.trim().is_empty())
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();

        let mut out: Vec<String> = Vec::new();
        for (p_idx, para) in paragraphs.iter().enumerate() {
            let mut remaining = para.clone();
            while !remaining.is_empty() && out.len() < max_lines {
                if out.len() == max_lines - 1 {
                    let mut rest_parts = vec![remaining];
                    rest_parts.extend(paragraphs[p_idx + 1..].iter().cloned());
                    let rest_all = rest_parts.join(" ");
                    out.push(self.truncate_line(&rest_all, style, max_width));
                    return out;
                }

                if self.measure_single_line_width(&remaining, style) <= max_width {
                    out.push(remaining);
                    break;
                }

                let words: Vec<&str> = remaining.split(' ').collect();
                let mut line = String::new();
                let mut idx = 0usize;
                while idx < words.len() {
                    let cand = if line.is_empty() {
                        words[idx].to_owned()
                    } else {
                        format!("{line} {}", words[idx])
                    };
                    if self.measure_single_line_width(&cand, style) <= max_width {
                        line = cand;
                        idx += 1;
                    } else {
                        break;
                    }
                }

                if line.is_empty() {
                    // Single token wider than max_width -> split strictly at char boundaries.
                    let boundaries: Vec<usize> = remaining
                        .char_indices()
                        .map(|(i, _)| i)
                        .chain(std::iter::once(remaining.len()))
                        .collect();
                    let mut lo = 1usize;
                    let mut hi = boundaries.len().saturating_sub(1);
                    let mut best_end = boundaries.get(1).copied().unwrap_or(remaining.len());
                    while lo <= hi {
                        let mid = lo + (hi - lo) / 2;
                        let end = boundaries[mid];
                        if self.measure_single_line_width(&remaining[..end], style) <= max_width {
                            best_end = end;
                            lo = mid + 1;
                        } else {
                            hi = mid - 1;
                        }
                    }
                    line = remaining[..best_end].to_owned();
                    remaining = remaining[best_end..].trim().to_owned();
                } else {
                    remaining = words[idx..].join(" ").trim().to_owned();
                }

                out.push(line);
            }
            if out.len() >= max_lines {
                break;
            }
        }

        out
    }

    fn rasterize_buffer_pass(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        buffer: &Buffer,
        origin_x: f32,
        origin_y: f32,
        color: [u8; 4],
    ) {
        if color[3] == 0 || !is_safe_draw_coord(origin_x) || !is_safe_draw_coord(origin_y) {
            return;
        }
        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;
        if pw <= 0 || ph <= 0 {
            return;
        }
        let pixels = pixmap.pixels_mut();

        for run in buffer.layout_runs() {
            let line_origin_y = origin_y + run.line_y;
            if !is_safe_draw_coord(line_origin_y) {
                continue;
            }
            for glyph in run.glyphs {
                let gx = origin_x + glyph.x;
                let gy = line_origin_y + glyph.y;
                if !is_safe_draw_coord(gx) || !is_safe_draw_coord(gy) {
                    continue;
                }
                let pg = glyph.physical((origin_x, line_origin_y), 1.0);
                let Some(img) = self
                    .swash_cache
                    .get_image(&mut self.font_system, pg.cache_key)
                else {
                    continue;
                };
                let gw = img.placement.width as i32;
                let gh = img.placement.height as i32;
                if gw <= 0 || gh <= 0 {
                    continue;
                }
                let gx0 = pg.x + img.placement.left;
                let gy0 = pg.y - img.placement.top;

                match img.content {
                    SwashContent::Mask => {
                        for row in 0..gh {
                            let dy = gy0 + row;
                            if dy < 0 || dy >= ph {
                                continue;
                            }
                            let row_off = (dy as usize) * (pw as usize);
                            let src_row = (row as usize) * (gw as usize);
                            for col in 0..gw {
                                let dx = gx0 + col;
                                if dx < 0 || dx >= pw {
                                    continue;
                                }
                                let cov = img.data[src_row + col as usize];
                                if cov == 0 {
                                    continue;
                                }
                                let src_a = ((color[3] as u16 * cov as u16 + 127) / 255) as u8;
                                if src_a == 0 {
                                    continue;
                                }
                                blend_source_over_pixel(
                                    &mut pixels[row_off + dx as usize],
                                    color[0],
                                    color[1],
                                    color[2],
                                    src_a,
                                );
                            }
                        }
                    }
                    SwashContent::Color => {
                        for row in 0..gh {
                            let dy = gy0 + row;
                            if dy < 0 || dy >= ph {
                                continue;
                            }
                            let row_off = (dy as usize) * (pw as usize);
                            let src_row = (row as usize) * (gw as usize) * 4;
                            for col in 0..gw {
                                let dx = gx0 + col;
                                if dx < 0 || dx >= pw {
                                    continue;
                                }
                                let idx = src_row + (col as usize) * 4;
                                let ca = img.data[idx + 3];
                                let src_a = ((color[3] as u16 * ca as u16 + 127) / 255) as u8;
                                if src_a == 0 {
                                    continue;
                                }
                                blend_source_over_pixel(
                                    &mut pixels[row_off + dx as usize],
                                    img.data[idx],
                                    img.data[idx + 1],
                                    img.data[idx + 2],
                                    src_a,
                                );
                            }
                        }
                    }
                    SwashContent::SubpixelMask => {
                        for row in 0..gh {
                            let dy = gy0 + row;
                            if dy < 0 || dy >= ph {
                                continue;
                            }
                            let row_off = (dy as usize) * (pw as usize);
                            let src_row = (row as usize) * (gw as usize) * 3;
                            for col in 0..gw {
                                let dx = gx0 + col;
                                if dx < 0 || dx >= pw {
                                    continue;
                                }
                                let idx = src_row + (col as usize) * 3;
                                let cov = ((img.data[idx] as u16
                                    + img.data[idx + 1] as u16
                                    + img.data[idx + 2] as u16)
                                    / 3) as u8;
                                if cov == 0 {
                                    continue;
                                }
                                let src_a = ((color[3] as u16 * cov as u16 + 127) / 255) as u8;
                                if src_a == 0 {
                                    continue;
                                }
                                blend_source_over_pixel(
                                    &mut pixels[row_off + dx as usize],
                                    color[0],
                                    color[1],
                                    color[2],
                                    src_a,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Renders shaped text onto `pixmap` using premultiplied `SourceOver` compositing
    /// and optional shadow/outline halo effects.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        x: f32,
        y: f32,
        max_width: Option<f32>,
        align: TextAlign,
        wrap: Wrap,
        ellipsize: Ellipsize,
        style: TextStyle,
        effect: TextEffect,
    ) -> TextMetricsBox {
        if text.is_empty() || !is_safe_draw_coord(x) || !is_safe_draw_coord(y) {
            return TextMetricsBox::default();
        }
        let max_width = sanitize_max_width(max_width);
        let cosmic_align = if max_width.is_some() {
            align.to_cosmic()
        } else {
            Align::Left
        };
        let buffer = self.shape_buffer(text, style, max_width, cosmic_align, wrap, ellipsize);
        let metrics = self.inspect_buffer_metrics(&buffer);

        let draw_x = if max_width.is_none() {
            match align {
                TextAlign::Left => x,
                TextAlign::Center => x - metrics.width * 0.5,
                TextAlign::Right => x - metrics.width,
            }
        } else {
            x
        };
        if !is_safe_draw_coord(draw_x) {
            return metrics;
        }

        match effect {
            TextEffect::None => {}
            TextEffect::Shadow {
                offset_x,
                offset_y,
                color,
            } => {
                self.rasterize_buffer_pass(
                    pixmap,
                    &buffer,
                    draw_x + offset_x as f32,
                    y + offset_y as f32,
                    color,
                );
            }
            TextEffect::Outline { radius_px, color } => {
                let r = radius_px.clamp(0, 6);
                let max_d2 = r * r + 1;
                for dy in -r..=r {
                    for dx in -r..=r {
                        if (dx == 0 && dy == 0) || (dx * dx + dy * dy > max_d2) {
                            continue;
                        }
                        self.rasterize_buffer_pass(
                            pixmap,
                            &buffer,
                            draw_x + dx as f32,
                            y + dy as f32,
                            color,
                        );
                    }
                }
            }
        }

        self.rasterize_buffer_pass(pixmap, &buffer, draw_x, y, style.color);
        metrics
    }

    /// Renders pre-wrapped lines onto `pixmap` spaced by `style.line_height_px`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_lines(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        lines: &[String],
        x: f32,
        y: f32,
        max_width: Option<f32>,
        align: TextAlign,
        style: TextStyle,
        effect: TextEffect,
    ) -> TextMetricsBox {
        if lines.is_empty() || !is_safe_draw_coord(x) || !is_safe_draw_coord(y) {
            return TextMetricsBox::default();
        }
        let max_width = sanitize_max_width(max_width);
        let mut combined = TextMetricsBox::default();
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let ly = y + (i as f32) * style.line_height_px;
            if !is_safe_draw_coord(ly) {
                continue;
            }
            let m = self.draw_text(
                pixmap,
                line,
                x,
                ly,
                max_width,
                align,
                Wrap::None,
                Ellipsize::None,
                style,
                effect,
            );
            combined.width = combined.width.max(m.width);
            combined.line_count += 1;
        }
        combined.height = (lines.len() as f32) * style.line_height_px;
        combined
    }
}

impl Default for HudFonts {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn blend_source_over_pixel(
    dst: &mut PremultipliedColorU8,
    src_r: u8,
    src_g: u8,
    src_b: u8,
    src_a: u8,
) {
    let sa = src_a as u16;
    let sr = (src_r as u16 * sa + 127) / 255;
    let sg = (src_g as u16 * sa + 127) / 255;
    let sb = (src_b as u16 * sa + 127) / 255;

    let inv_a = 255 - sa;
    let out_a = (sa + (dst.alpha() as u16 * inv_a + 127) / 255).min(255) as u8;
    let out_r = (sr + (dst.red() as u16 * inv_a + 127) / 255)
        .min(out_a as u16) as u8;
    let out_g = (sg + (dst.green() as u16 * inv_a + 127) / 255)
        .min(out_a as u16) as u8;
    let out_b = (sb + (dst.blue() as u16 * inv_a + 127) / 255)
        .min(out_a as u16) as u8;

    if let Some(pm) = PremultipliedColorU8::from_rgba(out_r, out_g, out_b, out_a) {
        *dst = pm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Pixmap;

    fn assert_premultiplied_invariant(pixmap: &Pixmap) {
        for px in pixmap.pixels() {
            let a = px.alpha();
            assert!(
                px.red() <= a && px.green() <= a && px.blue() <= a,
                "premultiplied invariant violated: rgba=({}, {}, {}, {})",
                px.red(),
                px.green(),
                px.blue(),
                a
            );
        }
    }

    fn ink_bounds_and_count(pixmap: &Pixmap) -> (usize, Option<(u32, u32, u32, u32)>) {
        let w = pixmap.width();
        let mut count = 0usize;
        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;

        for (idx, px) in pixmap.pixels().iter().enumerate() {
            if px.alpha() > 0 {
                count += 1;
                let x = (idx as u32) % w;
                let y = (idx as u32) / w;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let bounds = (count > 0).then_some((min_x, min_y, max_x, max_y));
        (count, bounds)
    }

    #[cfg(windows)]
    #[test]
    fn targeted_windows_font_load_avoids_system_scan() {
        let fonts = HudFonts::new();
        assert!(
            !fonts.used_system_fallback(),
            "expected targeted %WINDIR%\\Fonts load without system scan"
        );
        assert!(
            fonts.loaded_files() >= 5,
            "expected at least 5 target fonts loaded, got {}",
            fonts.loaded_files()
        );
        assert_eq!(fonts.sans_serif_family(), "Segoe UI");
    }

    #[test]
    fn vietnamese_diacritics_and_symbols_render_premultiplied() {
        let mut fonts = HudFonts::new();
        let text = "Đã ghim: Rương Hiếm • Khác tầng! (Ngầm (B1) → Mặt đất (G)) ă â đ ê ô ơ ư Ă Â Đ Ê Ô Ơ Ư á à ả ã ạ ứ ừ ử ữ ự … ↓ ↑ ↕";
        let style = TextStyle::new(15.0, FontWeightRole::Semibold, [248, 250, 252, 255]);
        let metrics = fonts.measure_text(text, style, None, Wrap::None, Ellipsize::None);
        assert!(metrics.width > 300.0);
        assert!(metrics.visual_height > 8.0);

        let mut pixmap = Pixmap::new(1000, 48).expect("pixmap");
        let mut pm_mut = pixmap.as_mut();
        fonts.draw_text(
            &mut pm_mut,
            text,
            8.0,
            8.0,
            None,
            TextAlign::Left,
            Wrap::None,
            Ellipsize::None,
            style,
            TextEffect::None,
        );
        assert_premultiplied_invariant(&pixmap);
        let (ink_vn, _) = ink_bounds_and_count(&pixmap);
        assert!(ink_vn > 400, "expected substantial inked pixels, got {ink_vn}");

        // Compare against ASCII-only subset to verify diacritics & symbols actually add glyphs.
        let mut ascii_pm = Pixmap::new(1000, 48).expect("ascii pixmap");
        let mut ascii_mut = ascii_pm.as_mut();
        fonts.draw_text(
            &mut ascii_mut,
            "Da ghim: Ruong Hiem",
            8.0,
            8.0,
            None,
            TextAlign::Left,
            Wrap::None,
            Ellipsize::None,
            style,
            TextEffect::None,
        );
        let (ink_ascii, _) = ink_bounds_and_count(&ascii_pm);
        assert!(ink_vn > ink_ascii * 2);
    }

    #[test]
    fn weight_roles_differentiate_regular_semibold_and_bold() {
        let mut fonts = HudFonts::new();
        let word = "Genshin";

        let mut render_weight = |weight: FontWeightRole| -> Vec<u8> {
            let mut pm = Pixmap::new(160, 40).expect("pixmap");
            let mut pm_mut = pm.as_mut();
            let style = TextStyle::new(18.0, weight, [255, 255, 255, 255]);
            fonts.draw_text(
                &mut pm_mut,
                word,
                8.0,
                6.0,
                None,
                TextAlign::Left,
                Wrap::None,
                Ellipsize::None,
                style,
                TextEffect::None,
            );
            pm.data().to_vec()
        };

        let reg = render_weight(FontWeightRole::Regular);
        let semi = render_weight(FontWeightRole::Semibold);
        let bold = render_weight(FontWeightRole::Bold);
        assert_ne!(reg, semi, "Regular and Semibold bitmaps should differ");
        assert_ne!(semi, bold, "Semibold and Bold bitmaps should differ");
        assert_ne!(reg, bold, "Regular and Bold bitmaps should differ");
    }

    #[test]
    fn truncate_and_multipara_wrap_respect_limits_and_utf8() {
        let mut fonts = HudFonts::new();
        let style = TextStyle::new(12.0, FontWeightRole::Regular, [255, 255, 255, 255])
            .with_line_height(16.0);

        let long_vn = "Khu vực: Đồng bằng Phong Khởi — Rương Hiếm trên mái nhà phía Đông Bắc gần tượng Thất Thiên";
        let truncated = fonts.truncate_line(long_vn, style, 160.0);
        assert!(truncated.ends_with("..."));
        let trunc_w = fonts
            .measure_text(&truncated, style, None, Wrap::None, Ellipsize::None)
            .width;
        assert!(
            trunc_w <= 160.5,
            "truncated width {trunc_w} exceeded max_width 160"
        );

        let multi = "Đoạn 1: Leo lên vách đá phía Tây.\nĐoạn 2: Dùng nguyên tố Phong kích hoạt.\nĐoạn 3: Nhảy xuống tầng ngầm B1.\nĐoạn 4: Mở rương hiếm và thu thập Thần Đồng.";
        let wrapped = fonts.wrap_lines(multi, style, 180.0, 5);
        assert!(
            wrapped.len() <= 5 && wrapped.len() >= 4,
            "unexpected wrapped line count: {}",
            wrapped.len()
        );
        for line in &wrapped {
            let lw = fonts
                .measure_text(line, style, None, Wrap::None, Ellipsize::None)
                .width;
            assert!(lw <= 180.5, "line '{line}' exceeded 180px: {lw}");
        }
        assert!(wrapped.last().is_some_and(|l| l.ends_with("...")));
    }

    #[test]
    fn outline_and_shadow_expand_ink_bounds() {
        let mut fonts = HudFonts::new();
        let style = TextStyle::new(16.0, FontWeightRole::Bold, [255, 255, 255, 255]);

        let mut plain_pm = Pixmap::new(180, 48).expect("plain");
        let mut plain_mut = plain_pm.as_mut();
        fonts.draw_text(
            &mut plain_mut,
            "Teyvat",
            12.0,
            10.0,
            None,
            TextAlign::Left,
            Wrap::None,
            Ellipsize::None,
            style,
            TextEffect::None,
        );

        let mut out_pm = Pixmap::new(180, 48).expect("outline");
        let mut out_mut = out_pm.as_mut();
        fonts.draw_text(
            &mut out_mut,
            "Teyvat",
            12.0,
            10.0,
            None,
            TextAlign::Left,
            Wrap::None,
            Ellipsize::None,
            style,
            TextEffect::Outline {
                radius_px: 1,
                color: [0, 0, 0, 220],
            },
        );
        assert_premultiplied_invariant(&out_pm);

        let (plain_cnt, plain_box) = ink_bounds_and_count(&plain_pm);
        let (out_cnt, out_box) = ink_bounds_and_count(&out_pm);
        assert!(out_cnt > plain_cnt);
        let (px0, py0, px1, py1) = plain_box.expect("plain ink");
        let (ox0, oy0, ox1, oy1) = out_box.expect("outline ink");
        assert!(ox0 < px0 && oy0 < py0 && ox1 > px1 && oy1 > py1);
    }

    #[test]
    fn outlier_and_non_finite_coordinates_do_not_panic() {
        let mut fonts = HudFonts::new();
        let style = TextStyle::new(14.0, FontWeightRole::Regular, [255, 255, 255, 255]);
        let mut pm = Pixmap::new(64, 64).expect("pixmap");
        let mut pm_mut = pm.as_mut();

        for (x, y, max_w) in [
            (1.0e10_f32, 100.0_f32, None),
            (-1.0e10_f32, 100.0_f32, Some(120.0)),
            (10.0, f32::INFINITY, None),
            (10.0, f32::NEG_INFINITY, Some(f32::INFINITY)),
            (f32::NAN, 10.0, Some(f32::NAN)),
            (10.0, 10.0, Some(f32::NAN)),
            (10.0, 10.0, Some(f32::INFINITY)),
        ] {
            let m = fonts.draw_text(
                &mut pm_mut,
                "Xin chào Teyvat",
                x,
                y,
                max_w,
                TextAlign::Left,
                Wrap::None,
                Ellipsize::None,
                style,
                TextEffect::Shadow {
                    offset_x: 1,
                    offset_y: 1,
                    color: [0, 0, 0, 200],
                },
            );
            assert!(m.width.is_finite() && m.height.is_finite());

            let lines = vec!["Dòng 1".to_owned(), "Dòng 2".to_owned()];
            let ml = fonts.draw_text_lines(
                &mut pm_mut,
                &lines,
                x,
                y,
                max_w,
                TextAlign::Left,
                style,
                TextEffect::None,
            );
            assert!(ml.width.is_finite() && ml.height.is_finite());
        }
    }
}
