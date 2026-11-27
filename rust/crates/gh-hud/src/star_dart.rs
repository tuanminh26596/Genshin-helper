//! Shared bilaterally symmetrical **Tinh Tú (Star Dart)** renderer for all Teyvat Craft overlays.

use tiny_skia::{
    FillRule, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StarDartStyle {
    pub fill: [u8; 4],
    pub halo: [u8; 4],
    pub trim: [u8; 4],
    pub core: [u8; 4],
    pub halo_width_scale: f32,
}

impl StarDartStyle {
    pub const HUD: Self = Self {
        fill: [250, 246, 238, 255],
        halo: [20, 24, 34, 245],
        trim: [211, 188, 142, 235],
        core: [26, 30, 40, 255],
        halo_width_scale: 1.0,
    };

    pub const HUD_LOCK_ON: Self = Self {
        fill: [250, 246, 238, 255],
        halo: [20, 24, 34, 245],
        trim: [223, 196, 140, 250],
        core: [45, 212, 191, 255],
        halo_width_scale: 1.0,
    };

    pub const PLAYER: Self = Self {
        fill: [126, 232, 255, 255],
        halo: [20, 24, 34, 245],
        trim: [240, 252, 255, 235],
        core: [211, 188, 142, 255],
        halo_width_scale: 1.0,
    };

    pub const EDGE_GOLD: Self = Self {
        fill: [245, 217, 155, 250],
        halo: [20, 24, 34, 245],
        trim: [255, 243, 214, 235],
        core: [26, 30, 40, 255],
        halo_width_scale: 1.0,
    };

    pub const EDGE_CYAN: Self = Self {
        fill: [126, 232, 255, 245],
        halo: [20, 24, 34, 240],
        trim: [240, 252, 255, 230],
        core: [26, 30, 40, 255],
        halo_width_scale: 1.0,
    };
}

// Reference coordinate unit around center (16, 16) in viewBox 0..32:
// Tip reaches (0, -13), wings reach (±9.5, +9), notch sits at (0, +4).
const OUTER_VERTS: [(f32, f32); 4] = [(0.0, -13.0), (9.5, 9.0), (0.0, 4.0), (-9.5, 9.0)];
const TRIM_VERTS: [(f32, f32); 4] = [(0.0, -9.0), (6.2, 5.8), (0.0, 2.4), (-6.2, 5.8)];
const STAR_VERTS: [(f32, f32); 8] = [
    (0.0, -3.8),
    (1.1, -1.5),
    (3.4, -0.4),
    (1.1, 0.7),
    (0.0, 3.0),
    (-1.1, 0.7),
    (-3.4, -0.4),
    (-1.1, -1.5),
];

fn build_transformed_poly(
    pts: &[(f32, f32)],
    cx: f32,
    cy: f32,
    scale: f32,
    sin_a: f32,
    cos_a: f32,
) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for (idx, &(lx, ly)) in pts.iter().enumerate() {
        let sx = lx * scale;
        let sy = ly * scale;
        let px = cx + sx * cos_a - sy * sin_a;
        let py = cy + sx * sin_a + sy * cos_a;
        if idx == 0 {
            pb.move_to(px, py);
        } else {
            pb.line_to(px, py);
        }
    }
    pb.close();
    pb.finish()
}

fn solid_paint(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    p
}

/// Draws the harmonized **Tinh Tú (Star Dart)** pointer centered around `(cx, cy)`.
///
/// `angle_rad_cw_from_up` points straight up at `0.0` and clockwise with increasing radians.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_star_dart(
    pm: &mut Pixmap,
    cx: f32,
    cy: f32,
    radius: f32,
    angle_rad_cw_from_up: f32,
    style: StarDartStyle,
    transform: Transform,
    mask: Option<&Mask>,
) {
    if !radius.is_finite() || radius <= 0.5 {
        return;
    }
    let scale = radius / 13.0;
    let (sin_a, cos_a) = angle_rad_cw_from_up.sin_cos();

    if let Some(outer) = build_transformed_poly(&OUTER_VERTS, cx, cy, scale, sin_a, cos_a) {
        let halo_w = (2.4 * scale * style.halo_width_scale).max(1.2);
        let halo_stroke = Stroke {
            width: halo_w,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pm.stroke_path(&outer, &solid_paint(style.halo), &halo_stroke, transform, mask);
        pm.fill_path(&outer, &solid_paint(style.fill), FillRule::Winding, transform, mask);
    }

    if let Some(trim) = build_transformed_poly(&TRIM_VERTS, cx, cy, scale, sin_a, cos_a) {
        let trim_w = (1.15 * scale).max(0.75);
        let trim_stroke = Stroke {
            width: trim_w,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pm.stroke_path(&trim, &solid_paint(style.trim), &trim_stroke, transform, mask);
    }

    if let Some(star) = build_transformed_poly(&STAR_VERTS, cx, cy, scale, sin_a, cos_a) {
        let star_halo_w = (1.0 * scale).max(0.6);
        let star_stroke = Stroke {
            width: star_halo_w,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pm.stroke_path(&star, &solid_paint(style.halo), &star_stroke, transform, mask);
        pm.fill_path(&star, &solid_paint(style.core), FillRule::Winding, transform, mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_poly_symmetric(verts: &[(f32, f32)], name: &str) {
        for &(x, y) in verts {
            let has_mirror = verts
                .iter()
                .any(|&(mx, my)| (x + mx).abs() <= 1e-5 && (y - my).abs() <= 1e-5);
            assert!(
                has_mirror,
                "{name}: vertex ({x}, {y}) missing bilateral mirror (-x, y)"
            );
        }
    }

    #[test]
    fn star_dart_vertices_are_bilaterally_symmetric() {
        assert_poly_symmetric(&OUTER_VERTS, "OUTER_VERTS");
        assert_poly_symmetric(&TRIM_VERTS, "TRIM_VERTS");
        assert_poly_symmetric(&STAR_VERTS, "STAR_VERTS");
    }

    #[test]
    fn draw_star_dart_renders_non_empty_premultiplied_pixmap() {
        let mut pm = Pixmap::new(40, 40).expect("pixmap");
        draw_star_dart(
            &mut pm,
            20.0,
            20.0,
            13.0,
            0.35,
            StarDartStyle::PLAYER,
            Transform::identity(),
            None,
        );
        let lit = pm.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(lit > 40, "expected drawn dart pixels, got {lit}");
        for px in pm.pixels() {
            let a = px.alpha();
            assert!(px.red() <= a && px.green() <= a && px.blue() <= a);
        }
    }
}
