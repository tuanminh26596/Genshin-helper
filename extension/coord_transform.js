// Bidirectional Piecewise Affine Coordinate Transform (Canonical HoYoLAB CRS <-> External Map Providers).
// Mirrors rust/crates/gh-core/src/map_bridge.rs::PiecewiseMapTransform 1:1.
// Supports per-region piecewise island shifts (e.g. AppSample Inazuma & Easybreeze Resort),
// swapped Leaflet lat/lng axes (e.g. Kongying v3 where lat = +X East, lng = +Y South),
// and live updates from desktop POST /calibrate_provider.
(function (root) {
  'use strict';

  const ZONE_PAD_CANONICAL = 350.0;

  function makeSeedAffine(ax, bx, cy, dy, mapId, source) {
    return { ax, bx, cy, dy, map_id: mapId, rmse: 0, max_residual: 0, anchor_count: 0, source };
  }

  function bboxContains(bbox, x, y, padX, padY) {
    if (!bbox) return false;
    return (
      x >= bbox.min_x - padX &&
      x <= bbox.max_x + padX &&
      y >= bbox.min_y - padY &&
      y <= bbox.max_y + padY
    );
  }

  function bboxDistanceSq(bbox, x, y) {
    const dx = x < bbox.min_x ? bbox.min_x - x : (x > bbox.max_x ? x - bbox.max_x : 0);
    const dy = y < bbox.min_y ? bbox.min_y - y : (y > bbox.max_y ? y - bbox.max_y : 0);
    return dx * dx + dy * dy;
  }

  function affineForward(aff, x, y) {
    return [aff.ax * x + aff.bx, aff.cy * y + aff.dy];
  }

  function affineInverse(aff, px, py) {
    if (!aff || Math.abs(aff.ax) < 1e-12 || Math.abs(aff.cy) < 1e-12) return null;
    return [(px - aff.bx) / aff.ax, (py - aff.dy) / aff.cy];
  }

  function buildDefaultSeeds() {
    const asSx = 8.58624949353e-5;
    const asSy = -8.58625216451e-5;
    return {
      'kongying:2': {
        provider: 'kongying',
        map_id: 2,
        swap_lat_lng: true,
        base: makeSeedAffine(1.335301544, 1527.748294, 1.335165651, -3931.249166, 2, 'seed_kongying_twt702'),
        zones: [],
        source: 'seed_kongying_twt702'
      },
      'appsample:2': {
        provider: 'appsample',
        map_id: 2,
        swap_lat_lng: false,
        base: makeSeedAffine(asSx, 0.522689857989, asSy, 0.199458672905, 2, 'seed_appsample_v72_m2'),
        zones: [
          {
            id: 'area_3',
            canonical_bbox: { min_x: -169.0, max_x: 3840.0, min_y: 4087.0, max_y: 9019.2 },
            provider_bbox: {
              min_x: asSx * -169.0 + 0.3334857058,
              max_x: asSx * 3840.0 + 0.3334857058,
              min_y: asSy * 9019.2 + 0.1994622811,
              max_y: asSy * 4087.0 + 0.1994622811
            },
            affine: makeSeedAffine(asSx, 0.3334857058, asSy, 0.1994622811, 2, 'seed_appsample_v72_inazuma')
          },
          {
            id: 'area_11_west',
            canonical_bbox: { min_x: -15949.9, max_x: -14339.4, min_y: 695.5, max_y: 2662.2 },
            provider_bbox: {
              min_x: asSx * -15949.9 + 0.6943898695,
              max_x: asSx * -14339.4 + 0.6943898695,
              min_y: asSy * 2662.2 + 0.1994580854,
              max_y: asSy * 695.5 + 0.1994580854
            },
            affine: makeSeedAffine(asSx, 0.6943898695, asSy, 0.1994580854, 2, 'seed_appsample_v72_easybreeze')
          }
        ],
        source: 'seed_appsample_v72'
      },
      'appsample:7': {
        provider: 'appsample',
        map_id: 7,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0003425009681, -0.06889919302, -0.0003424994737, 0.09208809008, 7, 'seed_appsample_v72_m7'),
        zones: [],
        source: 'seed_appsample_v72'
      },
      'appsample:9': {
        provider: 'appsample',
        map_id: 9,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0006849998113, 0.1389419667, -0.0006849998961, -0.007533606738, 9, 'seed_appsample_v72_m9'),
        zones: [],
        source: 'seed_appsample_v72'
      },
      'appsample:34': {
        provider: 'appsample',
        map_id: 34,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0003433756412, 0.03262133342, -0.000343387891, -0.03090441433, 34, 'seed_appsample_v72_m34'),
        zones: [],
        source: 'seed_appsample_v72'
      },
      'appsample:36': {
        provider: 'appsample',
        map_id: 36,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0005150052408, 0.0, -0.0005150024515, 0.0, 36, 'seed_appsample_v72_m36'),
        zones: [],
        source: 'seed_appsample_v72'
      },
      'appsample:37': {
        provider: 'appsample',
        map_id: 37,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0003430005951, -0.00411639969, -0.0003429997946, 0.04287577188, 37, 'seed_appsample_v72_m37'),
        zones: [],
        source: 'seed_appsample_v72'
      },
      'appsample:40': {
        provider: 'appsample',
        map_id: 40,
        swap_lat_lng: false,
        base: makeSeedAffine(0.0001495201733, -0.3464388249, -0.0001495198016, 0.0001504959339, 40, 'seed_appsample_v72_m40'),
        zones: [],
        source: 'seed_appsample_v72'
      }
    };
  }

  const runtimeCache = new Map();
  const defaultSeeds = buildDefaultSeeds();

  function keyOf(provider, mapId) {
    return `${String(provider || '').trim().toLowerCase()}:${Number(mapId)}`;
  }

  function isValidTransform(tf) {
    return Boolean(
      tf &&
      tf.base &&
      Number.isFinite(tf.base.ax) &&
      Number.isFinite(tf.base.bx) &&
      Number.isFinite(tf.base.cy) &&
      Number.isFinite(tf.base.dy) &&
      Math.abs(tf.base.ax) > 1e-12 &&
      Math.abs(tf.base.cy) > 1e-12
    );
  }

  function setTransform(provider, mapId, tf) {
    if (!isValidTransform(tf)) return false;
    runtimeCache.set(keyOf(provider, mapId), tf);
    return true;
  }

  function getTransform(provider, mapId) {
    const p = String(provider || '').trim().toLowerCase();
    const mid = Number(mapId);
    if (p === 'hoyolab' || p === 'miyoushe') {
      return {
        provider: p,
        map_id: mid,
        swap_lat_lng: false,
        base: makeSeedAffine(1, 0, 1, 0, mid, 'identity'),
        zones: [],
        source: 'identity'
      };
    }
    const k = keyOf(p, mid);
    if (runtimeCache.has(k)) return runtimeCache.get(k);
    return defaultSeeds[k] || null;
  }

  function forward(tf, cx, cy) {
    if (!isValidTransform(tf) || !Number.isFinite(cx) || !Number.isFinite(cy)) return null;
    const zones = Array.isArray(tf.zones) ? tf.zones : [];
    let bestZone = null;
    let bestD2 = Infinity;
    for (const z of zones) {
      if (bboxContains(z.canonical_bbox, cx, cy, ZONE_PAD_CANONICAL, ZONE_PAD_CANONICAL)) {
        const d2 = bboxDistanceSq(z.canonical_bbox, cx, cy);
        if (d2 < bestD2) {
          bestD2 = d2;
          bestZone = z;
        }
      }
    }
    return affineForward(bestZone ? bestZone.affine : tf.base, cx, cy);
  }

  function inverse(tf, px, py) {
    if (!isValidTransform(tf) || !Number.isFinite(px) || !Number.isFinite(py)) return null;
    const zones = Array.isArray(tf.zones) ? tf.zones : [];
    const padX = ZONE_PAD_CANONICAL * Math.abs(tf.base.ax);
    const padY = ZONE_PAD_CANONICAL * Math.abs(tf.base.cy);
    let bestZone = null;
    let bestD2 = Infinity;
    for (const z of zones) {
      if (bboxContains(z.provider_bbox, px, py, padX, padY)) {
        const d2 = bboxDistanceSq(z.provider_bbox, px, py);
        if (d2 < bestD2) {
          bestD2 = d2;
          bestZone = z;
        }
      }
    }
    return affineInverse(bestZone ? bestZone.affine : tf.base, px, py);
  }

  // Canonical (lat = cy South, lng = cx East) -> Provider Leaflet [lat, lng]
  function canonicalToLeaflet(tf, canonicalLat, canonicalLng) {
    const p = forward(tf, canonicalLng, canonicalLat);
    if (!p) return null;
    return tf.swap_lat_lng ? [p[0], p[1]] : [p[1], p[0]];
  }

  // Provider Leaflet (lat, lng) -> Canonical { lat: cy, lng: cx }
  function leafletToCanonical(tf, leafletLat, leafletLng) {
    const px = tf && tf.swap_lat_lng ? leafletLat : leafletLng;
    const py = tf && tf.swap_lat_lng ? leafletLng : leafletLat;
    const c = inverse(tf, px, py);
    if (!c) return null;
    return { lat: c[1], lng: c[0] };
  }

  const api = {
    getTransform,
    setTransform,
    isValidTransform,
    forward,
    inverse,
    canonicalToLeaflet,
    leafletToCanonical
  };

  if (root && typeof root === 'object') {
    root.GHCoordTransform = api;
  }
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : (typeof window !== 'undefined' ? window : this));
