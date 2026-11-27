// Pure unit tests and Rust<->JS parity checks for multi-map coordinate transforms and
// provider URL detection (Node >= 22, zero npm deps).
//
// Loads extension/coord_transform.js and extension/providers_registry.js unmodified inside
// a node:vm sandbox and validates all Rust-generated vectors in
// tests/fixtures/multimap/transform_vectors.json.
//
// Usage:
//   node tests/multimap_unit.mjs
// Optional env overrides (for seeded-fault proofs):
//   GH_COORD_TRANSFORM_PATH=<path.js>
//   GH_PROVIDERS_REGISTRY_PATH=<path.js>
//   GH_TRANSFORM_VECTORS_PATH=<path.json>
// Exit code: 0 iff fail === 0, 1 on fail/error.
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const COORD_TRANSFORM_PATH = process.env.GH_COORD_TRANSFORM_PATH
  ? resolve(process.env.GH_COORD_TRANSFORM_PATH)
  : join(ROOT, 'extension', 'coord_transform.js');
const PROVIDERS_REGISTRY_PATH = process.env.GH_PROVIDERS_REGISTRY_PATH
  ? resolve(process.env.GH_PROVIDERS_REGISTRY_PATH)
  : join(ROOT, 'extension', 'providers_registry.js');
const TRANSFORM_VECTORS_PATH = process.env.GH_TRANSFORM_VECTORS_PATH
  ? resolve(process.env.GH_TRANSFORM_VECTORS_PATH)
  : join(ROOT, 'tests', 'fixtures', 'multimap', 'transform_vectors.json');

const REQUIRED_SEEDED_PAIRS = [
  'hoyolab:2',
  'kongying:2',
  'appsample:2',
  'appsample:7',
  'appsample:9',
  'appsample:34',
  'appsample:36',
  'appsample:37',
  'appsample:40',
];

function loadSandboxScripts(coordPath, registryPath) {
  const sandbox = { URL };
  vm.createContext(sandbox);

  const coordSrc = readFileSync(coordPath, 'utf8');
  vm.runInContext(coordSrc, sandbox, { filename: coordPath });

  const registrySrc = readFileSync(registryPath, 'utf8');
  vm.runInContext(registrySrc, sandbox, { filename: registryPath });

  if (!sandbox.GHCoordTransform) {
    throw new Error(`GHCoordTransform was not attached to globalThis by ${coordPath}`);
  }
  if (!sandbox.GHMapProviders) {
    throw new Error(`GHMapProviders was not attached to globalThis by ${registryPath}`);
  }
  return {
    coord: sandbox.GHCoordTransform,
    providers: sandbox.GHMapProviders,
  };
}

function approxEq(actual, expected, tol) {
  return Number.isFinite(actual) && Number.isFinite(expected) && Math.abs(actual - expected) <= tol;
}

function runSuite() {
  const counts = { pass: 0, fail: 0, 'known-fail': 0, xpass: 0 };

  function record(id, ok, detail, knownBug = null) {
    let status;
    if (ok && !knownBug) status = 'pass';
    else if (!ok && !knownBug) status = 'fail';
    else if (!ok && knownBug) status = 'known-fail';
    else status = 'xpass';

    counts[status]++;
    const suffix = knownBug
      ? ok
        ? ` [known_bug resolved: ${knownBug}]`
        : ` [known_bug: ${knownBug}]`
      : '';
    if (status !== 'pass') {
      console.log(`  [${status}] ${id}: ${detail}${suffix}`);
    }
  }

  const { coord, providers } = loadSandboxScripts(COORD_TRANSFORM_PATH, PROVIDERS_REGISTRY_PATH);
  const fixture = JSON.parse(readFileSync(TRANSFORM_VECTORS_PATH, 'utf8'));

  // 1. Schema and coverage check on transform_vectors.json
  const tol = fixture && fixture.tolerances ? fixture.tolerances : {};
  const forwardAbs = Number(tol.forward_abs);
  const leafletAbs = Number(tol.leaflet_abs);
  const roundtripAbs = Number(tol.roundtrip_canonical_abs);
  const vectors = Array.isArray(fixture && fixture.vectors) ? fixture.vectors : [];

  record(
    'fixture_schema',
    fixture &&
      fixture.schema_version === 1 &&
      Number.isFinite(forwardAbs) &&
      forwardAbs > 0 &&
      Number.isFinite(leafletAbs) &&
      leafletAbs > 0 &&
      Number.isFinite(roundtripAbs) &&
      roundtripAbs > 0 &&
      vectors.length > 0,
    `schema_version=${fixture && fixture.schema_version}, vectors=${vectors.length}`
  );

  const presentPairs = new Set(vectors.map((v) => `${v.provider}:${v.map_id}`));
  const missingPairs = REQUIRED_SEEDED_PAIRS.filter((k) => !presentPairs.has(k));
  record(
    'fixture_seeded_pair_coverage',
    missingPairs.length === 0,
    missingPairs.length === 0 ? `covered ${REQUIRED_SEEDED_PAIRS.length} seeded pairs` : `missing pairs: ${missingPairs.join(', ')}`
  );

  // 2. Verify every vector against JS coord_transform
  for (const v of vectors) {
    const errors = [];
    const tf = coord.getTransform(v.provider, v.map_id);
    if (!tf) {
      errors.push(`getTransform(${v.provider}, ${v.map_id}) returned null`);
    } else {
      if (!coord.isValidTransform(tf)) {
        errors.push('isValidTransform(tf) returned false');
      }
      if (Boolean(tf.swap_lat_lng) !== Boolean(v.swap_lat_lng)) {
        errors.push(`swap_lat_lng mismatch: got ${tf.swap_lat_lng}, expected ${v.swap_lat_lng}`);
      }

      const cx = v.canonical.x;
      const cy = v.canonical.y;
      const fwd = coord.forward(tf, cx, cy);
      if (!Array.isArray(fwd) || fwd.length !== 2) {
        errors.push(`forward(${cx}, ${cy}) returned ${JSON.stringify(fwd)}`);
      } else {
        if (!approxEq(fwd[0], v.expected_provider.x, forwardAbs)) {
          errors.push(`forward.x mismatch: got ${fwd[0]}, expected ${v.expected_provider.x} (diff=${Math.abs(fwd[0] - v.expected_provider.x)})`);
        }
        if (!approxEq(fwd[1], v.expected_provider.y, forwardAbs)) {
          errors.push(`forward.y mismatch: got ${fwd[1]}, expected ${v.expected_provider.y} (diff=${Math.abs(fwd[1] - v.expected_provider.y)})`);
        }

        // Verify expected_zone selection matches affine coefficients
        if (v.expected_zone === 'base') {
          const basePx = tf.base.ax * cx + tf.base.bx;
          const basePy = tf.base.cy * cy + tf.base.dy;
          if (!approxEq(fwd[0], basePx, forwardAbs) || !approxEq(fwd[1], basePy, forwardAbs)) {
            errors.push(`expected_zone=base did not match base affine: got (${fwd[0]}, ${fwd[1]}), base=(${basePx}, ${basePy})`);
          }
        } else {
          const zone = (tf.zones || []).find((z) => z.id === v.expected_zone);
          if (!zone) {
            errors.push(`expected_zone="${v.expected_zone}" not found in tf.zones`);
          } else {
            const zPx = zone.affine.ax * cx + zone.affine.bx;
            const zPy = zone.affine.cy * cy + zone.affine.dy;
            if (!approxEq(fwd[0], zPx, forwardAbs) || !approxEq(fwd[1], zPy, forwardAbs)) {
              errors.push(`expected_zone=${v.expected_zone} did not match zone affine: got (${fwd[0]}, ${fwd[1]}), zone=(${zPx}, ${zPy})`);
            }
          }
        }

        // Inverse round-trip
        const inv = coord.inverse(tf, fwd[0], fwd[1]);
        if (!Array.isArray(inv) || inv.length !== 2) {
          errors.push(`inverse(${fwd[0]}, ${fwd[1]}) returned ${JSON.stringify(inv)}`);
        } else {
          if (!approxEq(inv[0], cx, roundtripAbs) || !approxEq(inv[1], cy, roundtripAbs)) {
            errors.push(`inverse roundtrip mismatch: got (${inv[0]}, ${inv[1]}), expected (${cx}, ${cy})`);
          }
        }
      }

      // canonicalToLeaflet(tf, canonicalLat=cy, canonicalLng=cx) -> [lat, lng]
      const leaflet = coord.canonicalToLeaflet(tf, cy, cx);
      if (!Array.isArray(leaflet) || leaflet.length !== 2) {
        errors.push(`canonicalToLeaflet(${cy}, ${cx}) returned ${JSON.stringify(leaflet)}`);
      } else {
        if (!approxEq(leaflet[0], v.expected_leaflet.lat, leafletAbs)) {
          errors.push(`leaflet.lat mismatch: got ${leaflet[0]}, expected ${v.expected_leaflet.lat}`);
        }
        if (!approxEq(leaflet[1], v.expected_leaflet.lng, leafletAbs)) {
          errors.push(`leaflet.lng mismatch: got ${leaflet[1]}, expected ${v.expected_leaflet.lng}`);
        }

        // leafletToCanonical round-trip
        const back = coord.leafletToCanonical(tf, leaflet[0], leaflet[1]);
        if (!back || typeof back !== 'object') {
          errors.push(`leafletToCanonical(${leaflet[0]}, ${leaflet[1]}) returned ${JSON.stringify(back)}`);
        } else {
          if (!approxEq(back.lng, cx, roundtripAbs) || !approxEq(back.lat, cy, roundtripAbs)) {
            errors.push(`leafletToCanonical roundtrip mismatch: got {lat:${back.lat}, lng:${back.lng}}, expected {lat:${cy}, lng:${cx}}`);
          }
        }
      }
    }

    record(`vector:${v.id}`, errors.length === 0, errors.length === 0 ? 'ok' : errors.join('; '));
  }

  // 3. Forward-only base fallback on non-invertible ocean fold-over sides just outside 350u pad
  {
    const tf = coord.getTransform('appsample', 2);
    const fallbackCases = [
      { id: 'appsample_m2_inazuma_outside_pad_west_fallback', cx: -569.0, cy: 6000.0 },
      { id: 'appsample_m2_easybreeze_outside_pad_east_fallback', cx: -13939.4, cy: 1500.0 },
    ];
    for (const c of fallbackCases) {
      const fwd = tf ? coord.forward(tf, c.cx, c.cy) : null;
      const basePx = tf ? tf.base.ax * c.cx + tf.base.bx : NaN;
      const basePy = tf ? tf.base.cy * c.cy + tf.base.dy : NaN;
      const ok = Boolean(
        fwd && approxEq(fwd[0], basePx, forwardAbs) && approxEq(fwd[1], basePy, forwardAbs)
      );
      record(
        c.id,
        ok,
        ok ? 'fell back to base' : `got ${JSON.stringify(fwd)}, expected base (${basePx}, ${basePy})`
      );
    }
  }

  // 4. miyoushe identity transform
  {
    const errors = [];
    for (const mapId of [2, 9]) {
      const tf = coord.getTransform('miyoushe', mapId);
      if (!tf) {
        errors.push(`getTransform('miyoushe', ${mapId}) returned null`);
        continue;
      }
      if (tf.swap_lat_lng !== false) {
        errors.push(`miyoushe:${mapId} swap_lat_lng=${tf.swap_lat_lng}, expected false`);
      }
      const cx = -1234.5;
      const cy = 678.25;
      const fwd = coord.forward(tf, cx, cy);
      if (!fwd || !approxEq(fwd[0], cx, 1e-12) || !approxEq(fwd[1], cy, 1e-12)) {
        errors.push(`miyoushe:${mapId} forward mismatch: ${JSON.stringify(fwd)}`);
      }
      const ll = coord.canonicalToLeaflet(tf, cy, cx);
      if (!ll || !approxEq(ll[0], cy, 1e-12) || !approxEq(ll[1], cx, 1e-12)) {
        errors.push(`miyoushe:${mapId} canonicalToLeaflet mismatch: ${JSON.stringify(ll)}`);
      }
      const inv = coord.inverse(tf, cx, cy);
      if (!inv || !approxEq(inv[0], cx, 1e-12) || !approxEq(inv[1], cy, 1e-12)) {
        errors.push(`miyoushe:${mapId} inverse mismatch: ${JSON.stringify(inv)}`);
      }
      const back = coord.leafletToCanonical(tf, cy, cx);
      if (!back || !approxEq(back.lat, cy, 1e-12) || !approxEq(back.lng, cx, 1e-12)) {
        errors.push(`miyoushe:${mapId} leafletToCanonical mismatch: ${JSON.stringify(back)}`);
      }
    }
    record('miyoushe_identity', errors.length === 0, errors.length === 0 ? 'ok' : errors.join('; '));
  }

  // 5. isValidTransform rejects degenerate transforms and setTransform rejects invalid ones
  {
    const invalidCases = [
      { id: 'invalid_null', tf: null },
      { id: 'invalid_undefined', tf: undefined },
      { id: 'invalid_empty_object', tf: {} },
      { id: 'invalid_missing_base', tf: { provider: 'appsample', map_id: 2, zones: [] } },
      { id: 'invalid_ax_zero', tf: { base: { ax: 0, bx: 1, cy: 1, dy: 0 } } },
      { id: 'invalid_cy_zero', tf: { base: { ax: 1, bx: 0, cy: 0, dy: 1 } } },
      { id: 'invalid_ax_tiny_1e-13', tf: { base: { ax: 1e-13, bx: 0, cy: 1, dy: 0 } } },
      { id: 'invalid_cy_tiny_neg_1e-13', tf: { base: { ax: 1, bx: 0, cy: -1e-13, dy: 0 } } },
      { id: 'invalid_ax_nan', tf: { base: { ax: Number.NaN, bx: 0, cy: 1, dy: 0 } } },
      { id: 'invalid_bx_nan', tf: { base: { ax: 1, bx: Number.NaN, cy: 1, dy: 0 } } },
      { id: 'invalid_cy_nan', tf: { base: { ax: 1, bx: 0, cy: Number.NaN, dy: 0 } } },
      { id: 'invalid_dy_nan', tf: { base: { ax: 1, bx: 0, cy: 1, dy: Number.NaN } } },
      { id: 'invalid_ax_infinity', tf: { base: { ax: Number.POSITIVE_INFINITY, bx: 0, cy: 1, dy: 0 } } },
      { id: 'invalid_dy_neg_infinity', tf: { base: { ax: 1, bx: 0, cy: 1, dy: Number.NEGATIVE_INFINITY } } },
    ];

    const beforeAppsample2 = coord.getTransform('appsample', 2);
    for (const c of invalidCases) {
      const validResult = coord.isValidTransform(c.tf);
      const setResult = coord.setTransform('appsample', 2, c.tf);
      const afterAppsample2 = coord.getTransform('appsample', 2);
      const preserved = afterAppsample2 === beforeAppsample2;
      const ok = validResult === false && setResult === false && preserved;
      record(
        `transform_validation:${c.id}`,
        ok,
        ok
          ? 'rejected'
          : `isValidTransform=${validResult}, setTransform=${setResult}, preserved=${preserved}`
      );
    }

    // Positive setTransform check on an isolated test map id
    const customTf = {
      provider: 'appsample',
      map_id: 9999,
      swap_lat_lng: false,
      base: { ax: 0.5, bx: 10.0, cy: -0.5, dy: -20.0 },
      zones: [],
      source: 'unit_test_custom',
    };
    const validCustom = coord.isValidTransform(customTf);
    const setCustom = coord.setTransform('appsample', 9999, customTf);
    const gotCustom = coord.getTransform('appsample', 9999);
    const fwdCustom = gotCustom ? coord.forward(gotCustom, 4.0, 8.0) : null;
    const okCustom = Boolean(
      validCustom &&
        setCustom &&
        gotCustom === customTf &&
        fwdCustom &&
        approxEq(fwdCustom[0], 12.0, 1e-12) &&
        approxEq(fwdCustom[1], -24.0, 1e-12)
    );
    record(
      'transform_validation:valid_setTransform',
      okCustom,
      okCustom ? 'accepted and applied' : `valid=${validCustom}, set=${setCustom}, fwd=${JSON.stringify(fwdCustom)}`
    );
  }

  // 6. detectProviderFromUrl coverage
  {
    const urlCases = [
      // Valid HoYoLAB URLs
      {
        id: 'url_hoyolab_index',
        url: 'https://act.hoyolab.com/ys/app/interactive-map/index.html',
        expected: 'hoyolab',
      },
      {
        id: 'url_hoyolab_hash_map2',
        url: 'https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2',
        expected: 'hoyolab',
      },
      {
        id: 'url_hoyolab_lang_query',
        url: 'https://act.hoyolab.com/ys/app/interactive-map/index.html?lang=vi-vn#/map/2',
        expected: 'hoyolab',
      },
      // Valid Kongying URLs
      {
        id: 'url_kongying_v3',
        url: 'https://v3.yuanshen.site/',
        expected: 'kongying',
      },
      {
        id: 'url_kongying_v3_hash',
        url: 'https://v3.yuanshen.site/#/map/2',
        expected: 'kongying',
      },
      {
        id: 'url_kongying_apex',
        url: 'https://yuanshen.site/',
        expected: 'kongying',
      },
      // Valid AppSample URLs
      {
        id: 'url_appsample_root',
        url: 'https://genshin-impact-map.appsample.com/',
        expected: 'appsample',
      },
      {
        id: 'url_appsample_query',
        url: 'https://genshin-impact-map.appsample.com/?map=teyvat',
        expected: 'appsample',
      },
      // Non-map HoYoLAB URLs -> null
      {
        id: 'url_hoyolab_non_map_home',
        url: 'https://www.hoyolab.com/home',
        expected: null,
      },
      {
        id: 'url_hoyolab_non_map_signin',
        url: 'https://act.hoyolab.com/ys/event/signin-sea-v3/index.html',
        expected: null,
      },
      {
        id: 'url_hoyolab_non_map_records',
        url: 'https://act.hoyolab.com/app/community-game-records-sea/index.html',
        expected: null,
      },
      // Unrelated URLs -> null
      {
        id: 'url_unrelated_example',
        url: 'https://example.com/',
        expected: null,
      },
      {
        id: 'url_unrelated_hoyoverse',
        url: 'https://genshin.hoyoverse.com/en/home',
        expected: null,
      },
      {
        id: 'url_unrelated_query_mention',
        url: 'https://google.com/search?q=act.hoyolab.com/ys/app/interactive-map/',
        expected: null,
      },
      // Spoofed hostnames -> null
      {
        id: 'url_spoof_kongying_prefix',
        url: 'https://evil-v3.yuanshen.site/',
        expected: null,
      },
      {
        id: 'url_spoof_kongying_suffix',
        url: 'https://v3.yuanshen.site.evil.com/',
        expected: null,
      },
      {
        id: 'url_spoof_appsample_prefix',
        url: 'https://evil-genshin-impact-map.appsample.com/',
        expected: null,
      },
      {
        id: 'url_spoof_appsample_suffix',
        url: 'https://genshin-impact-map.appsample.com.evil.com/',
        expected: null,
      },
      {
        id: 'url_spoof_hoyolab_prefix',
        url: 'https://evil-hoyolab.com/ys/app/interactive-map/index.html',
        expected: null,
        knownBug:
          "extension/providers_registry.js uses u.hostname.includes('hoyolab.com') instead of exact/subdomain match",
      },
      {
        id: 'url_spoof_hoyolab_suffix',
        url: 'https://hoyolab.com.evil.com/ys/app/interactive-map/index.html',
        expected: null,
        knownBug:
          "extension/providers_registry.js uses u.hostname.includes('hoyolab.com') instead of exact/subdomain match",
      },
      // Empty / invalid strings / non-strings -> null
      {
        id: 'url_empty_string',
        url: '',
        expected: null,
      },
      {
        id: 'url_invalid_text',
        url: 'not-a-valid-url',
        expected: null,
      },
      {
        id: 'url_javascript_scheme',
        url: 'javascript:alert(1)',
        expected: null,
      },
      {
        id: 'url_null',
        url: null,
        expected: null,
      },
      {
        id: 'url_undefined',
        url: undefined,
        expected: null,
      },
      {
        id: 'url_number',
        url: 12345,
        expected: null,
      },
    ];

    for (const c of urlCases) {
      const actual = providers.detectProviderFromUrl(c.url);
      const ok = actual === c.expected;
      record(
        c.id,
        ok,
        ok ? `detected=${JSON.stringify(actual)}` : `got ${JSON.stringify(actual)}, expected ${JSON.stringify(c.expected)}`,
        c.knownBug || null
      );
    }
  }

  console.log(
    `Multimap unit: pass ${counts.pass} | fail ${counts.fail} | known-fail ${counts['known-fail']} | xpass ${counts.xpass}`
  );
  return counts.fail === 0 ? 0 : 1;
}

try {
  process.exit(runSuite());
} catch (err) {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
}
