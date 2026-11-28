//! EXPERIMENTAL — measurement gate for the v5.5 M-Map route guidance redesign.
//!
//! Screen→world registration of the M-Map view that is *currently displayed*
//! (`world = scale · screen + t`), **without** requiring the player arrow on screen. The user
//! drags the map away from the character to look for the route region, so guidance must be
//! computed from what the map shows, never from the player position.
//!
//! Mirrors the waypoint/terrain cascade of [`MapLocator::locate_player`] with `arrow_pos = None`
//! and without the 1-waypoint rigid case (which needs the arrow). `locate_player` itself is not
//! touched. Accuracy is measured by `tests/e3_view_registration.rs` before any UI uses it.

use super::*;

/// Registered transform of the displayed M-Map view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewRegistration {
    pub map_id: i32,
    /// World units per screen pixel.
    pub scale: f64,
    pub tx: f64,
    pub ty: f64,
    pub inliers_count: usize,
    pub rmse: f64,
    /// `score_terrain_alignment` of the final transform (0..1, higher = better) — confidence cue.
    pub terrain_score: f64,
    /// `true` when no waypoint matched and the terrain-only fallback produced the transform.
    pub terrain_only: bool,
    pub zoom_ratio_t: Option<f64>,
    pub is_underground: bool,
    pub candidates_count: usize,
}

/// Prior for which map/area the displayed view belongs to.
///
/// The bottom-right region label names the *sub-map* when one is open (Enkanomiya, Sea of
/// Bygone Eras, ...), but on Teyvat it names the PLAYER's current region, not the dragged-to
/// view (observed in a 130 s live drag recording, 2026-11-24). An area prior is therefore only
/// valid while the player is in view; dragging needs [`ViewHint::RegionOcrMapOnly`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ViewHint {
    /// No prior: cascade tries Teyvat first, then isolated sub-maps.
    #[default]
    None,
    /// Region-label OCR as map AND area prior (only valid while the player is in view).
    RegionOcr,
    /// Region-label OCR as map prior only (safe while dragging away from the player).
    RegionOcrMapOnly,
    /// Externally known map id (labelled recordings / upstream state).
    Map(i32),
}

/// Search options for [`MapLocator::locate_view_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewOptions {
    /// Zero-waypoint terrain search + terrain head-to-head guard. Live tracking turns it off: on
    /// the 130 s drag recording those paths cost ~1 s per failing frame and produced "stuck"
    /// registrations, while the frame-to-frame tracker bridges waypoint-less views instead.
    pub allow_terrain_only: bool,
    /// Translation-vote matcher on Teyvat when the zoom slider gives the scale (see
    /// `match_by_translation_votes`).
    pub translation_votes: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            allow_terrain_only: true,
            translation_votes: false,
        }
    }
}

/// Translation votes: accept outright at this many inliers (with the margin below), otherwise
/// only replace a combinatorial match that has at least `VOTE_MIN_MARGIN` fewer inliers.
pub(super) const VOTE_ACCEPT_INLIERS: usize = 6;
const VOTE_MIN_INLIERS: usize = 4;
pub(super) const VOTE_MIN_MARGIN: usize = 2;
/// Vote bin and inlier tolerance, screen px @1440p.
const VOTE_CELL_PX_1440: f64 = 12.0;
const VOTE_TOL_PX_1440: f64 = 9.0;
/// Symmetric re-centering tolerance after snapping off the `12 px` grid-cell boundary.
/// Keeps the full `9.0 px` capture for jittered scenes while removing boundary quantization bias.
const VOTE_REFINE_TOL_PX_1440: f64 = 9.0;
const VOTE_TOP_BINS: usize = 16;

/// Minimum terrain alignment for a 3-waypoint match to be trusted (e3 report: every wrong
/// registration on the 51 labelled fixtures was a 3-inlier match with terrain score <= 0.20).
pub const VIEW_MIN_TERRAIN_3P: f64 = 0.25;
/// Minimum terrain alignment for a terrain-only (zero-waypoint) registration.
pub const VIEW_MIN_TERRAIN_ONLY: f64 = 0.30;

impl ViewRegistration {
    /// Confidence gate: >= 4 waypoint inliers, or 3 inliers backed by terrain alignment, or a
    /// strong terrain-only match. Guidance must not be drawn from a non-confident registration.
    pub fn is_confident(&self) -> bool {
        if self.terrain_only {
            return self.terrain_score >= VIEW_MIN_TERRAIN_ONLY;
        }
        self.inliers_count >= 4
            || (self.inliers_count == 3 && self.terrain_score >= VIEW_MIN_TERRAIN_3P)
    }

    pub fn screen_to_world(&self, x: f64, y: f64) -> (f64, f64) {
        (self.scale * x + self.tx, self.scale * y + self.ty)
    }

    pub fn world_to_screen(&self, wx: f64, wy: f64) -> (f64, f64) {
        let s = if self.scale.abs() < 1e-12 { 1e-12 } else { self.scale };
        ((wx - self.tx) / s, (wy - self.ty) / s)
    }
}

impl MapLocator {
    /// Registers the displayed M-Map view using the given map/area prior ([`ViewHint`]).
    pub fn locate_view(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        slider_res_override: Option<&ZoomSliderResult>,
        hint: ViewHint,
    ) -> Option<ViewRegistration> {
        self.locate_view_opts(frame_bgr, slider_res_override, hint, true)
    }

    /// Like [`Self::locate_view`] with only [`ViewOptions::allow_terrain_only`] chosen.
    pub fn locate_view_opts(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        slider_res_override: Option<&ZoomSliderResult>,
        hint: ViewHint,
        allow_terrain_only: bool,
    ) -> Option<ViewRegistration> {
        let opts = ViewOptions {
            allow_terrain_only,
            ..ViewOptions::default()
        };
        self.locate_view_with(frame_bgr, slider_res_override, hint, opts)
    }

    /// Registers the displayed M-Map view with explicit [`ViewOptions`].
    pub fn locate_view_with(
        &self,
        frame_bgr: ArrayView3<'_, u8>,
        slider_res_override: Option<&ZoomSliderResult>,
        hint: ViewHint,
        opts: ViewOptions,
    ) -> Option<ViewRegistration> {
        let (h, w, c) = frame_bgr.dim();
        if h == 0 || w == 0 || c < 3 || self.db.maps.is_empty() {
            return None;
        }
        let bgr3 = if c > 3 {
            frame_bgr.slice(s![.., .., 0..3])
        } else {
            frame_bgr
        };
        let n_px = h * w;
        LOCATOR_IMG_SCRATCH.with(|cell| {
            let mut im = cell.borrow_mut();
            if im.hsv_buf.len() < n_px * 3 {
                im.hsv_buf.resize(n_px * 3, 0);
            }
            if im.gray_buf.len() < n_px {
                im.gray_buf.resize(n_px, 0);
            }
            let im_ref = &mut *im;
            let hsv_s = &mut im_ref.hsv_buf[..n_px * 3];
            let gray_s = &mut im_ref.gray_buf[..n_px];
            fill_hsv_and_gray_into(frame_bgr, hsv_s, gray_s);
            let hsv = ArrayView3::from_shape((h, w, 3), &*hsv_s).ok()?;
            let frame_gray = ArrayView2::from_shape((h, w), &*gray_s).ok()?;
            self.locate_view_inner(bgr3, hsv, frame_gray, slider_res_override, hint, opts)
        })
    }

    fn locate_view_inner(
        &self,
        bgr3: ArrayView3<'_, u8>,
        hsv: ArrayView3<'_, u8>,
        frame_gray: ArrayView2<'_, u8>,
        slider_res_override: Option<&ZoomSliderResult>,
        hint: ViewHint,
        opts: ViewOptions,
    ) -> Option<ViewRegistration> {
        let allow_terrain_only = opts.allow_terrain_only;
        let (h, w, _) = bgr3.dim();
        let layer_info = self.detect_layers_universal(bgr3);
        let owned_slider;
        let slider_res: Option<&ZoomSliderResult> = match slider_res_override {
            Some(s) => Some(s),
            None => {
                owned_slider = ZoomSliderDetector::detect_slider(bgr3);
                owned_slider.as_ref()
            }
        };
        let zoom_t = slider_res.map(|s| s.ratio_t);
        let known_scale_m2 = ZoomSliderDetector::get_map_scale(slider_res, 2, w);

        let candidates =
            self.detect_waypoint_candidates_with_gray(bgr3, None, Some(hsv), Some(frame_gray));
        let (target_map_id, target_area_id) = match hint {
            ViewHint::None => (None, None),
            ViewHint::RegionOcr => {
                let (m, a, _) = self.detect_region_name(bgr3);
                (m, a)
            }
            ViewHint::RegionOcrMapOnly => (self.detect_region_name(bgr3).0, None),
            ViewHint::Map(mid) => (self.db.maps.contains_key(&mid).then_some(mid), None),
        };
        let is_ug_hint = layer_info.is_underground;

        let primary_indices = |wps: &[WaypointEntry]| -> Vec<usize> {
            let p: Vec<usize> = wps
                .iter()
                .enumerate()
                .filter_map(|(i, wp)| wp.is_primary.then_some(i))
                .collect();
            if p.len() >= 2 { p } else { (0..wps.len()).collect() }
        };

        type CandidateMatch<'b> = (&'b MapMeta, Vec<InlierPair<'b>>, f64, [f64; 2], f64);
        let mut best: Option<CandidateMatch<'_>> = None;
        let mut terrain_only = false;

        // Case A: region hint names an isolated sub-map.
        if candidates.len() >= 2
            && let Some(t_mid) = target_map_id
            && t_mid != 2
            && let Some(mdata) = self.db.map(t_mid)
        {
            let strict_sub = ZoomSliderDetector::get_map_scale(slider_res, t_mid, w);
            let base_s =
                strict_sub.unwrap_or_else(|| map_base_scale_cfg(t_mid) * (2560.0 / w as f64));
            let all_idx: Vec<usize> = (0..mdata.waypoints.len()).collect();
            let lims: &[usize] = if candidates.len() > 12 { &[12, 18] } else { &[12] };
            for &lim in lims {
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map_with_ug(
                    &candidates,
                    t_mid,
                    &mdata.waypoints,
                    &all_idx,
                    lim,
                    w,
                    base_s,
                    None,
                    None,
                    true,
                    strict_sub,
                    Some(frame_gray),
                    is_ug_hint,
                ) {
                    best = Some((mdata, inl, sc, t_vec, rmse));
                    break;
                }
            }
        }

        // Case B: Teyvat (map 2).
        if best.is_none()
            && candidates.len() >= 2
            && (target_map_id == Some(2) || target_map_id.is_none())
            && let Some(m2) = self.db.map(2)
        {
            let strict_m2 = known_scale_m2;
            let base_m2 = strict_m2.unwrap_or(1.12);
            let prim_idx = primary_indices(&m2.waypoints);

            // Case B0: translation votes at the slider scale.
            let mut votes: Option<MatchResult<'_>> = None;
            if opts.translation_votes
                && let Some(s) = strict_m2
            {
                votes = self.match_by_translation_votes(&candidates, &m2.waypoints, &prim_idx, s, h);
                if let Some(v) = votes.as_ref()
                    && v.0.len() >= VOTE_ACCEPT_INLIERS
                {
                    best = Some((m2, v.0.clone(), v.1, v.2, v.3));
                }
            }

            if best.is_none() && zoom_t.is_some_and(|t| t <= 0.12) {
                let major_idx: Vec<usize> = prim_idx
                    .iter()
                    .copied()
                    .filter(|&i| matches!(m2.waypoints[i].label_id, 2 | 154))
                    .collect();
                if major_idx.len() >= 2
                    && let Some((inl, sc, t_vec, rmse)) = self.match_single_map_with_ug(
                        &candidates,
                        2,
                        &m2.waypoints,
                        &major_idx,
                        26,
                        w,
                        base_m2,
                        target_area_id,
                        None,
                        target_area_id.is_some(),
                        strict_m2,
                        Some(frame_gray),
                        is_ug_hint,
                    )
                {
                    best = Some((m2, inl, sc, t_vec, rmse));
                }
            }

            if best.is_none() {
                let lims: &[usize] = if candidates.len() > 12 { &[12, 18] } else { &[12] };
                for &lim in lims {
                    if let Some((inl, sc, t_vec, rmse)) = self.match_single_map_with_ug(
                        &candidates,
                        2,
                        &m2.waypoints,
                        &prim_idx,
                        lim,
                        w,
                        base_m2,
                        target_area_id,
                        None,
                        target_area_id.is_some(),
                        strict_m2,
                        Some(frame_gray),
                        is_ug_hint,
                    ) {
                        best = Some((m2, inl, sc, t_vec, rmse));
                        break;
                    }
                }
            }

            if best.is_none() && m2.waypoints.len() > prim_idx.len() {
                let all_idx: Vec<usize> = (0..m2.waypoints.len()).collect();
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map_with_ug(
                    &candidates,
                    2,
                    &m2.waypoints,
                    &all_idx,
                    14,
                    w,
                    base_m2,
                    target_area_id,
                    None,
                    target_area_id.is_some(),
                    strict_m2,
                    Some(frame_gray),
                    is_ug_hint,
                ) {
                    best = Some((m2, inl, sc, t_vec, rmse));
                }
            }

            if let Some(v) = votes
                && best.as_ref().is_none_or(|b| b.1.len() + VOTE_MIN_MARGIN <= v.0.len())
            {
                best = Some((m2, v.0, v.1, v.2, v.3));
            }
        }

        // Case C: isolated sub-maps without a region hint.
        if best.is_none()
            && candidates.len() >= 2
            && target_map_id.is_none()
            && target_area_id.is_none()
        {
            for &mid in &self.shared.map_order {
                if mid == 2 {
                    continue;
                }
                let Some(mdata) = self.db.map(mid) else {
                    continue;
                };
                let strict_sub = ZoomSliderDetector::get_map_scale(slider_res, mid, w);
                let base_s = strict_sub.unwrap_or_else(|| map_base_scale_cfg(mid));
                let all_idx: Vec<usize> = (0..mdata.waypoints.len()).collect();
                if let Some((inl, sc, t_vec, rmse)) = self.match_single_map_with_ug(
                    &candidates,
                    mid,
                    &mdata.waypoints,
                    &all_idx,
                    10,
                    w,
                    base_s,
                    None,
                    None,
                    false,
                    strict_sub,
                    Some(frame_gray),
                    is_ug_hint,
                ) {
                    best = Some((mdata, inl, sc, t_vec, rmse));
                    break;
                }
            }
        }

        // Surface terrain head-to-head guard for weak (<= 4 inlier) waypoint matches.
        if allow_terrain_only
            && let Some((mdata, ref inl, sc, t_vec, _)) = best
            && !is_ug_hint
            && slider_res.is_some()
            && inl.len() <= 4
        {
            let fb_mid = target_map_id
                .filter(|mid| self.db.maps.contains_key(mid))
                .unwrap_or(mdata.id);
            if self.has_terrain_for_map(fb_mid) {
                let t_sc = if self.has_terrain_for_map(mdata.id) {
                    self.score_terrain_alignment(
                        Some(frame_gray),
                        mdata.id,
                        sc,
                        t_vec[0],
                        t_vec[1],
                        8,
                    )
                } else {
                    0.0
                };
                if t_sc < WEAK_MATCH_TERRAIN_CHECK_THRESH
                    && let Some(fb_scale) = ZoomSliderDetector::get_map_scale(slider_res, fb_mid, w)
                    && let Some(alt) =
                        self.match_zero_waypoint_terrain(frame_gray, fb_mid, fb_scale, target_area_id)
                {
                    let alt_t = self.score_terrain_alignment(
                        Some(frame_gray),
                        fb_mid,
                        alt.1,
                        alt.2[0],
                        alt.2[1],
                        8,
                    );
                    if should_terrain_guard_override(
                        inl.len(),
                        mdata.id,
                        sc,
                        t_vec,
                        t_sc,
                        fb_mid,
                        alt.1,
                        alt.2,
                        alt_t,
                        w,
                        h,
                    ) && let Some(fb_mdata) = self.db.map(fb_mid)
                    {
                        terrain_only = alt.0.is_empty();
                        best = Some((fb_mdata, alt.0, alt.1, alt.2, alt.3));
                    }
                }
            }
        }

        // Case D: zero-waypoint terrain-only fallback (needs the zoom slider scale).
        if allow_terrain_only && best.is_none() && slider_res.is_some() {
            let fallback_mid = target_map_id
                .filter(|mid| self.db.maps.contains_key(mid))
                .or_else(|| self.db.maps.contains_key(&2).then_some(2));
            if let Some(fb_mid) = fallback_mid
                && let Some(fb_scale) = ZoomSliderDetector::get_map_scale(slider_res, fb_mid, w)
                && let Some(res) =
                    self.match_zero_waypoint_terrain(frame_gray, fb_mid, fb_scale, target_area_id)
                && let Some(mdata) = self.db.map(fb_mid)
            {
                terrain_only = true;
                best = Some((mdata, res.0, res.1, res.2, res.3));
            }
        }

        let (mdata, inliers, mut scale, t_vec, mut rmse) = best?;
        let mut tx = t_vec[0];
        let mut ty = t_vec[1];
        if !inliers.is_empty() {
            let scale_ui = h as f64 / 1440.0;
            let adj = Self::compensate_inliers_screen_anchors(&inliers, scale_ui);
            if adj.len() >= 2 {
                if let Some((lsq_s, lsq_tx, lsq_ty, lsq_rmse)) = Self::fit_isometric_lsq(&adj)
                    && (lsq_s - scale).abs() / scale.max(1e-6) <= 0.06
                {
                    scale = lsq_s;
                    tx = lsq_tx;
                    ty = lsq_ty;
                    rmse = lsq_rmse;
                } else {
                    let nf = adj.len() as f64;
                    tx = adj.iter().map(|&(s, wp)| wp.x_pos - scale * s.0).sum::<f64>() / nf;
                    ty = adj.iter().map(|&(s, wp)| wp.y_pos - scale * s.1).sum::<f64>() / nf;
                }
            } else {
                let (s0, w0) = adj[0];
                tx = w0.x_pos - scale * s0.0;
                ty = w0.y_pos - scale * s0.1;
            }
            if WORLD_ANCHOR_MAP_IDS.contains(&mdata.id) {
                ty += WORLD_ANCHOR_Y_OFFSET;
            }
            let (tx_ref, ty_ref, terr_ok) = self.refine_transform_with_terrain(
                Some(frame_gray),
                mdata.id,
                scale,
                tx,
                ty,
                12,
                layer_info.is_underground,
            );
            if terr_ok {
                tx = tx_ref;
                ty = ty_ref;
            }
        }

        let terrain_score = self.score_terrain_alignment(Some(frame_gray), mdata.id, scale, tx, ty, 8);
        Some(ViewRegistration {
            map_id: mdata.id,
            scale,
            tx,
            ty,
            inliers_count: inliers.len(),
            rmse,
            terrain_score,
            terrain_only,
            zoom_ratio_t: zoom_t,
            is_underground: layer_info.is_underground,
            candidates_count: candidates.len(),
        })
    }
}

impl MapLocator {
    /// Translation-only registration at a known scale. On Teyvat the zoom slider predicts the
    /// registered scale within ±0.5 % (e5 recording, 1194 strong registrations), so the view is
    /// fixed by a 2-D translation: every (candidate, waypoint) pair votes for the translation it
    /// implies and the best-supported bins are verified by counting inliers. Unlike
    /// `match_single_map` it neither caps the candidates (12/18 there) nor requires 3–4 point
    /// matches to share one area, which lost zoomed-out views spanning area borders. Returns
    /// raw-candidate inlier pairs like the combinatorial matcher (anchor compensation and LSQ
    /// happen afterwards), or `None` without a clear winner.
    pub(super) fn match_by_translation_votes<'a>(
        &self,
        candidates: &[(f64, f64, i32)],
        wps: &'a [WaypointEntry],
        idx: &[usize],
        scale: f64,
        frame_h: usize,
    ) -> Option<MatchResult<'a>> {
        if candidates.len() < VOTE_MIN_INLIERS || idx.len() < VOTE_MIN_INLIERS || scale.is_nan() || scale <= 0.0 {
            return None;
        }
        let ui = frame_h as f64 / 1440.0;
        let cell = scale * VOTE_CELL_PX_1440 * ui;
        let tol = scale * VOTE_TOL_PX_1440 * ui;
        let offs: Vec<(f64, f64)> = idx.iter().map(|&i| icon_anchor_offset_px(wps[i].label_id, ui)).collect();
        let mut bins: HashMap<(i64, i64), (u32, f64, f64)> = HashMap::new();
        for c in candidates {
            for (k, &i) in idx.iter().enumerate() {
                let (ox, oy) = offs[k];
                let tx = wps[i].x_pos - scale * (c.0 + ox);
                let ty = wps[i].y_pos - scale * (c.1 + oy);
                let e = bins
                    .entry(((tx / cell).floor() as i64, (ty / cell).floor() as i64))
                    .or_insert((0, 0.0, 0.0));
                e.0 += 1;
                e.1 += tx;
                e.2 += ty;
            }
        }
        let mut top: Vec<_> =
            bins.into_iter().filter(|(_, b)| b.0 >= 2).collect();
        top.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        top.truncate(VOTE_TOP_BINS);

        // Verify: unique nearest-waypoint inliers within `max_d` for a translation center.
        let collect_inliers = |center: [f64; 2], max_d: f64| -> (Vec<InlierPair<'a>>, f64) {
            let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
            for (ci, c) in candidates.iter().enumerate() {
                let mut bestd = max_d;
                let mut bestk = None;
                for (k, &i) in idx.iter().enumerate() {
                    let (ox, oy) = offs[k];
                    let d = (wps[i].x_pos - (scale * (c.0 + ox) + center[0]))
                        .hypot(wps[i].y_pos - (scale * (c.1 + oy) + center[1]));
                    if d < bestd {
                        bestd = d;
                        bestk = Some(k);
                    }
                }
                if let Some(k) = bestk {
                    pairs.push((bestd, ci, k));
                }
            }
            pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut used = vec![false; idx.len()];
            let mut inl: Vec<InlierPair<'a>> = Vec::new();
            let mut sq = 0.0;
            for (d, ci, k) in pairs {
                if !used[k] {
                    used[k] = true;
                    inl.push((candidates[ci], &wps[idx[k]]));
                    sq += (d / scale) * (d / scale);
                }
            }
            let rmse = if inl.is_empty() {
                f64::INFINITY
            } else {
                (sq / inl.len() as f64).sqrt()
            };
            (inl, rmse)
        };

        let mut hyps: Vec<(Vec<InlierPair<'a>>, [f64; 2], f64)> = Vec::new();
        for &(_, (n, sx, sy)) in &top {
            let t = [sx / f64::from(n), sy / f64::from(n)];
            let (inl, rmse) = collect_inliers(t, tol);
            if !inl.is_empty() {
                hyps.push((inl, t, rmse));
            }
        }
        hyps.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.2.total_cmp(&b.2)));
        let best = hyps.first()?;
        if best.0.len() < VOTE_MIN_INLIERS {
            return None;
        }
        // Runner-up = best hypothesis at a clearly different translation.
        let runner = hyps
            .iter()
            .skip(1)
            .find(|h| (h.1[0] - best.1[0]).hypot(h.1[1] - best.1[1]) > 3.0 * cell)
            .map_or(0, |h| h.0.len());
        if best.0.len() < runner + VOTE_MIN_MARGIN {
            return None;
        }
        let (inl, t, rmse) = hyps.swap_remove(0);

        // Re-center around the component-wise median translation so a cluster straddling a 12 px
        // vote-cell boundary is not pulled toward the quantized bin center (which otherwise drops
        // valid inliers across the boundary and admits off-center outliers).
        let median_f64 = |vals: &mut [f64]| -> f64 {
            vals.sort_by(|a, b| a.total_cmp(b));
            let m = vals.len();
            if m % 2 == 1 {
                vals[m / 2]
            } else {
                0.5 * (vals[m / 2 - 1] + vals[m / 2])
            }
        };
        let mut txs = Vec::with_capacity(inl.len());
        let mut tys = Vec::with_capacity(inl.len());
        for &(c, wp) in &inl {
            let (ox, oy) = icon_anchor_offset_px(wp.label_id, ui);
            txs.push(wp.x_pos - scale * (c.0 + ox));
            tys.push(wp.y_pos - scale * (c.1 + oy));
        }
        let t_med = [median_f64(&mut txs), median_f64(&mut tys)];

        let refine_tol = scale * VOTE_REFINE_TOL_PX_1440 * ui;
        let (ref_inl, _) = collect_inliers(t_med, refine_tol);
        if ref_inl.len() < VOTE_MIN_INLIERS {
            return Some((inl, scale, t, rmse));
        }

        let nf = ref_inl.len() as f64;
        let mut sum_tx = 0.0;
        let mut sum_ty = 0.0;
        let mut ref_ts = Vec::with_capacity(ref_inl.len());
        for &(c, wp) in &ref_inl {
            let (ox, oy) = icon_anchor_offset_px(wp.label_id, ui);
            let tx_i = wp.x_pos - scale * (c.0 + ox);
            let ty_i = wp.y_pos - scale * (c.1 + oy);
            sum_tx += tx_i;
            sum_ty += ty_i;
            ref_ts.push((tx_i, ty_i));
        }
        let t_ref = [sum_tx / nf, sum_ty / nf];
        let sq: f64 = ref_ts
            .iter()
            .map(|&(tx_i, ty_i)| {
                let dx = (tx_i - t_ref[0]) / scale;
                let dy = (ty_i - t_ref[1]) / scale;
                dx * dx + dy * dy
            })
            .sum();
        Some((ref_inl, scale, t_ref, (sq / nf).sqrt()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(inliers: usize, terrain: f64, terrain_only: bool) -> ViewRegistration {
        ViewRegistration {
            map_id: 2,
            scale: 0.75,
            tx: -1200.0,
            ty: 340.0,
            inliers_count: inliers,
            rmse: 2.0,
            terrain_score: terrain,
            terrain_only,
            zoom_ratio_t: Some(0.5),
            is_underground: false,
            candidates_count: 10,
        }
    }

    #[test]
    fn confidence_gate_rejects_weak_three_point_and_terrain_only_matches() {
        assert!(reg(4, 0.0, false).is_confident());
        assert!(reg(3, VIEW_MIN_TERRAIN_3P, false).is_confident());
        assert!(!reg(3, 0.20, false).is_confident(), "every wrong e3 match was 3-point, terrain <= 0.20");
        assert!(!reg(2, 0.9, false).is_confident());
        assert!(reg(0, VIEW_MIN_TERRAIN_ONLY, true).is_confident());
        assert!(!reg(0, 0.18, true).is_confident());
    }

    #[test]
    fn screen_world_round_trip() {
        let r = reg(5, 0.4, false);
        let (wx, wy) = r.screen_to_world(1280.0, 720.0);
        let (sx, sy) = r.world_to_screen(wx, wy);
        assert!((sx - 1280.0).abs() < 1e-9 && (sy - 720.0).abs() < 1e-9);
    }

    #[test]
    fn translation_votes_recenters_across_cell_boundary_and_prunes_outlier() {
        let absent = PathBuf::from("__absent_unit_test_asset__");
        let loc = MapLocator::with_paths(
            LocatorPaths {
                db_path: Some(absent.clone()),
                terrain_npz_path: Some(absent.clone()),
                cave_masks_npz_path: Some(absent.clone()),
                ui_anchors_npz_path: Some(absent.clone()),
                legacy_underground_db_path: Some(absent),
            },
            None,
        );

        // Cluster straddling the 12 px cell boundary at tx=12.0 (scale=1.0, frame_h=1440):
        // Bin (0,0) collects 6 votes [2.2, 2.4, 11.2, 11.4, 11.6, 11.8] (mean 8.433), whose raw
        // 9 px window keeps the 6.0-6.2 px outliers (2.2, 2.4) while dropping valid point 19.2.
        // Re-centering at median t_med=11.5 recovers 19.2 across the boundary and prunes 2.2 & 2.4.
        let txs = [2.2, 2.4, 11.2, 11.4, 11.6, 11.8, 12.4, 12.6, 19.2];
        let mut candidates = Vec::with_capacity(txs.len());
        let mut wps = Vec::with_capacity(txs.len());
        for (i, &tx_i) in txs.iter().enumerate() {
            let sx = 200.0 + (i as f64) * 180.0;
            let sy = 300.0 + ((i % 3) as f64) * 200.0;
            candidates.push((sx, sy, 100));
            wps.push(WaypointEntry {
                id: (i + 1) as i64,
                label_id: 3,
                is_primary: true,
                x_pos: sx + tx_i,
                y_pos: sy + 5.0 + 6.0,
                area_id: 1,
                map_id: 2,
                z_level: 0,
                icon_sign: 0,
                point_group: None,
            });
        }
        let idx: Vec<usize> = (0..wps.len()).collect();
        let (inl, sc, t_ref, _) = loc
            .match_by_translation_votes(&candidates, &wps, &idx, 1.0, 1440)
            .expect("expected translation-vote match");
        assert!((sc - 1.0).abs() < 1e-12);

        let mut got_ids: Vec<i64> = inl.iter().map(|(_, wp)| wp.id).collect();
        got_ids.sort_unstable();
        assert_eq!(got_ids, vec![3, 4, 5, 6, 7, 8, 9], "got ids={got_ids:?}");
        assert!(
            (t_ref[0] - 12.8857).abs() < 0.05 && (t_ref[1] - 6.0).abs() < 1e-6,
            "unexpected refined mean translation: {t_ref:?}"
        );
    }
}
