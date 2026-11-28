// Offline real-browser DOM & runtime fixture tests for multi-provider map runtime (Node >= 22, zero npm deps).
//
// Serves deterministic local HTML/JS shells via CDP Fetch interception on real provider hostnames
// (https://genshin-impact-map.appsample.com/, https://v3.yuanshen.site/, and
// https://act.hoyolab.com/ys/app/interactive-map/index.html) so window.location.hostname and
// localStorage behave identically to live pages while making ZERO network requests.
//
// Loads extension/coord_transform.js and extension/provider_runtime.js UNMODIFIED.
//
// Usage:
//   node tests/provider_dom_fixtures.mjs
// Optional env:
//   GH_CHROME=<chrome.exe>              Chromium executable override (else Playwright Chromium)
//   GH_PROVIDER_RUNTIME_PATH=<path.js>  Override provider_runtime.js (for seeded-fault proofs)
//   GH_COORD_TRANSFORM_PATH=<path.js>   Override coord_transform.js (for seeded-fault proofs)
// Exit code: 0 iff fail === 0, 1 on fail/error, 2 if Chromium not found.
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const FIXTURES_DIR = join(ROOT, 'tests', 'fixtures', 'providers');
const VECTORS_PATH = join(ROOT, 'tests', 'fixtures', 'multimap', 'transform_vectors.json');
const COORD_TRANSFORM_PATH = process.env.GH_COORD_TRANSFORM_PATH
  ? resolve(process.env.GH_COORD_TRANSFORM_PATH)
  : join(ROOT, 'extension', 'coord_transform.js');
const PROVIDER_RUNTIME_PATH = process.env.GH_PROVIDER_RUNTIME_PATH
  ? resolve(process.env.GH_PROVIDER_RUNTIME_PATH)
  : join(ROOT, 'extension', 'provider_runtime.js');

const APPSAMPLE_URL = 'https://genshin-impact-map.appsample.com/';
const KONGYING_URL = 'https://v3.yuanshen.site/';
const HOYOLAB_URL = 'https://act.hoyolab.com/ys/app/interactive-map/index.html';

const CDP_CALL_TIMEOUT_MS = 15000;
const HTTP_TIMEOUT_MS = 10000;
const WATCHDOG_MS = Number(process.env.GH_FIXTURE_WATCHDOG_MS) || 120000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let abortRun = null;

function findChrome() {
  if (process.env.GH_CHROME && existsSync(process.env.GH_CHROME)) return process.env.GH_CHROME;
  const base = join(process.env.LOCALAPPDATA || '', 'ms-playwright');
  if (!existsSync(base)) return null;
  const dirs = readdirSync(base).filter((d) => /^chromium-\d+$/.test(d)).sort().reverse();
  for (const d of dirs) {
    for (const sub of ['chrome-win64', 'chrome-win']) {
      const exe = join(base, d, sub, 'chrome.exe');
      if (existsSync(exe)) return exe;
    }
  }
  return null;
}

class Cdp {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl);
    this.id = 0;
    this.pending = new Map();
    this.handlers = [];
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id && this.pending.has(m.id)) {
        const p = this.pending.get(m.id);
        this.pending.delete(m.id);
        clearTimeout(p.timer);
        p.resolve(m);
      } else if (m.method) {
        for (const h of this.handlers) h(m);
      }
    };
    const failAll = (why) => {
      for (const [id, p] of this.pending) {
        clearTimeout(p.timer);
        p.reject(new Error(`CDP ${p.method} #${id}: ${why}`));
      }
      this.pending.clear();
    };
    this.ws.onclose = () => failAll('socket closed');
    this.ready = new Promise((r, j) => {
      const timer = setTimeout(() => j(new Error(`CDP socket open timeout (${CDP_CALL_TIMEOUT_MS} ms)`)), CDP_CALL_TIMEOUT_MS);
      this.ws.onopen = () => { clearTimeout(timer); r(); };
      this.ws.onerror = () => { clearTimeout(timer); j(new Error('CDP socket error')); };
    });
  }
  send(method, params = {}, timeoutMs = CDP_CALL_TIMEOUT_MS) {
    return new Promise((resolveSend, rejectSend) => {
      const id = ++this.id;
      const timer = setTimeout(() => {
        if (!this.pending.delete(id)) return;
        rejectSend(new Error(`CDP ${method} #${id} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolveSend, reject: rejectSend, timer, method });
      try {
        this.ws.send(JSON.stringify({ id, method, params }));
      } catch (e) {
        this.pending.delete(id);
        clearTimeout(timer);
        rejectSend(e);
      }
    });
  }
  async eval(expression) {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (r.result && r.result.exceptionDetails) {
      const desc = r.result.exceptionDetails.exception
        ? r.result.exceptionDetails.exception.description
        : r.result.exceptionDetails.text;
      throw new Error(desc || 'Runtime.evaluate exception');
    }
    return r.result && r.result.result ? r.result.result.value : undefined;
  }
  waitForEvent(method, timeoutMs = 15000) {
    return new Promise((resolveWait, rejectWait) => {
      const timer = setTimeout(() => {
        this.handlers = this.handlers.filter((h) => h !== listener);
        rejectWait(new Error(`Timeout waiting for CDP event ${method}`));
      }, timeoutMs);
      const listener = (m) => {
        if (m.method === method) {
          clearTimeout(timer);
          this.handlers = this.handlers.filter((h) => h !== listener);
          resolveWait(m);
        }
      };
      this.handlers.push(listener);
    });
  }
  close() {
    if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) {
      this.ws.close();
    }
  }
}

function buildMockBridgeAndMapScript() {
  return `
    window.__ghRequests = [];
    window.__ghSetViewCalls = [];
    window.__ghPanToCalls = [];

    window.addEventListener('message', (e) => {
      if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-bg') return;
      if (e.data.kind !== 'API_REQUEST') return;
      const rec = { path: e.data.path, method: e.data.method || 'GET', body: e.data.body };
      window.__ghRequests.push(rec);
      let data = { ok: true };
      if (rec.path === '/status') {
        data = { ok: true, language: 'en', engine_active: true, auto_sync: true, auto_sync_m: true, minimap_tracking: true, latest_seq: 0 };
      } else if (rec.path === '/nearby_pins' || rec.path === '/tab_visibility' || rec.path === '/report_state') {
        data = { ok: true, is_owner: true };
      } else if (rec.path === '/calibrate_provider') {
        data = { ok: true, transform: null };
      }
      window.postMessage({
        __gh_ext: true,
        dir: 'to-page',
        kind: 'API_RESPONSE',
        reqId: e.data.reqId,
        response: { ok: true, status: 200, data }
      }, window.location.origin);
    });

    function createMockLeafletMap(container, meta) {
      let center = { lat: Number(meta.center.lat), lng: Number(meta.center.lng) };
      let zoom = Number(meta.zoom);
      const minZoom = Number(meta.minZoom);
      const maxZoom = Number(meta.maxZoom);
      const panes = {};
      const listeners = {};
      const ensurePane = (name) => {
        if (!panes[name]) {
          const el = document.createElement('div');
          el.className = 'leaflet-pane leaflet-' + name;
          el.style.position = 'absolute';
          container.appendChild(el);
          panes[name] = el;
        }
        return panes[name];
      };
      ensurePane('overlayPane');
      ensurePane('markerPane');
      const map = {
        _container: container,
        _layers: {},
        getContainer: () => container,
        getCenter: () => ({ lat: center.lat, lng: center.lng }),
        getZoom: () => zoom,
        getMinZoom: () => minZoom,
        getMaxZoom: () => maxZoom,
        getPane: (name) => panes[name] || null,
        createPane: (name) => ensurePane(name),
        setView: (ll, z, opts) => {
          const lat = Array.isArray(ll) ? Number(ll[0]) : Number(ll.lat);
          const lng = Array.isArray(ll) ? Number(ll[1]) : Number(ll.lng);
          center = { lat, lng };
          if (Number.isFinite(z)) zoom = z;
          window.__ghSetViewCalls.push({ lat, lng, zoom, opts });
          return map;
        },
        panTo: (ll, opts) => {
          const lat = Array.isArray(ll) ? Number(ll[0]) : Number(ll.lat);
          const lng = Array.isArray(ll) ? Number(ll[1]) : Number(ll.lng);
          center = { lat, lng };
          window.__ghPanToCalls.push({ lat, lng, opts });
          return map;
        },
        latLngToLayerPoint: (ll) => {
          const lat = Array.isArray(ll) ? Number(ll[0]) : Number(ll.lat);
          const lng = Array.isArray(ll) ? Number(ll[1]) : Number(ll.lng);
          return { x: 400 + (lng - center.lng) * 100, y: 300 - (lat - center.lat) * 100 };
        },
        latLngToContainerPoint: (ll) => {
          const lat = Array.isArray(ll) ? Number(ll[0]) : Number(ll.lat);
          const lng = Array.isArray(ll) ? Number(ll[1]) : Number(ll.lng);
          return { x: 400 + (lng - center.lng) * 100, y: 300 - (lat - center.lat) * 100 };
        },
        getBounds: () => ({ contains: () => true }),
        on: (evStr, fn) => {
          for (const ev of String(evStr).split(/\s+/)) {
            if (!listeners[ev]) listeners[ev] = [];
            listeners[ev].push(fn);
          }
          return map;
        },
        off: (evStr, fn) => {
          for (const ev of String(evStr).split(/\s+/)) {
            if (listeners[ev]) listeners[ev] = listeners[ev].filter((f) => f !== fn);
          }
          return map;
        }
      };
      return map;
    }

    function sendServerEvent(data) {
      window.postMessage({
        __gh_ext: true,
        dir: 'to-page',
        kind: 'SERVER_EVENT',
        data
      }, window.location.origin);
    }
    window.__ghSendServerEvent = sendServerEvent;
  `;
}

function buildAppSampleHtml(fixture) {
  return `<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>AppSample Offline Shell</title></head>
<body>
  <div class="leaflet-container" style="position:relative;width:800px;height:600px;"></div>
  <script>
    ${buildMockBridgeAndMapScript()}
    (function setupAppSampleFixture() {
      const fx = ${JSON.stringify(fixture)};
      const container = document.querySelector('.leaflet-container');
      const map = createMockLeafletMap(container, fx.map);

      // Minimal Leaflet L.marker + L.divIcon (matching live AppSample hasWindowL=true)
      window.L = {
        divIcon: (opts) => ({ options: opts }),
        marker: (latlng, opts = {}) => {
          let curLatLng = Array.isArray(latlng) ? { lat: Number(latlng[0]), lng: Number(latlng[1]) } : { lat: Number(latlng.lat), lng: Number(latlng.lng) };
          const el = document.createElement('div');
          el.className = (opts.icon && opts.icon.options && opts.icon.options.className) || 'leaflet-marker-icon';
          if (opts.icon && opts.icon.options && opts.icon.options.html) {
            el.innerHTML = opts.icon.options.html;
          }
          const markerObj = {
            options: opts,
            _latlng: curLatLng,
            addTo: (m) => {
              const pane = (opts.pane && m.getPane(opts.pane)) || m.getPane('markerPane') || container;
              pane.appendChild(el);
              window.__ghLastLeafletMarker = markerObj;
              return markerObj;
            },
            setLatLng: (next) => {
              curLatLng = Array.isArray(next) ? { lat: Number(next[0]), lng: Number(next[1]) } : { lat: Number(next.lat), lng: Number(next.lng) };
              markerObj._latlng = curLatLng;
              return markerObj;
            },
            getLatLng: () => curLatLng,
            getElement: () => el,
            remove: () => { if (el.parentNode) el.parentNode.removeChild(el); }
          };
          return markerObj;
        }
      };

      const state = {
        mapKey: fx.storeState.mapKey,
        types: [...fx.storeState.types],
        sid: fx.storeState.sid,
        user: fx.storeState.user,
        found: new Set(fx.storeState.found || []),
        overlay: { ...(fx.storeState.overlay || { mode: 'none' }) },
        markersComplete: fx.storeState.markersComplete,
        markers: fx.markers
      };
      const subs = new Set();
      const store = {
        getState: () => state,
        subscribe: (fn) => { subs.add(fn); return () => subs.delete(fn); },
        __setTypes: (nextTypes) => {
          state.types = [...nextTypes];
          for (const fn of subs) fn(state);
        }
      };
      const indexMap = new Map(fx.markers.map((m) => [String(m.id), m]));
      const catalog = {
        allMarkers: fx.markers,
        officialMarkers: fx.markers,
        get all() { return this.allMarkers; },
        get: (id) => indexMap.get(String(id)) || null,
        markerIconUrl: (m) => fx.iconUrlTemplate.replace('{type}', String(m.type || '')),
        config: { maps: { teyvat: { id: 2 } } }
      };
      const overlayCalls = [];
      const adapter = {
        map,
        showOverlays: (flag) => {
          overlayCalls.push(Boolean(flag));
          state.overlay = { mode: flag ? 'underground' : 'none' };
        }
      };
      const progress = {
        toggleFound: async (id) => {
          if (state.found.has(id)) state.found.delete(id);
          else state.found.add(id);
        }
      };
      const appCtx = {
        store,
        catalog,
        progress,
        data: { mapKey: fx.storeState.mapKey, messages: fx.messages },
        mapSync: { adapter }
      };
      container['__reactFiber$fixture'] = {
        memoizedProps: {},
        return: {
          memoizedProps: { value: appCtx },
          return: null
        }
      };
      window.__ghFixtureHandles = { map, store, catalog, overlayCalls };
    })();
  </script>
  <script src="/__gh/coord_transform.js"></script>
  <script src="/__gh/provider_runtime.js"></script>
</body>
</html>`;
}

function buildKongyingHtml(fixture) {
  return `<!DOCTYPE html>
<html lang="zh-CN">
<head><meta charset="utf-8"><title>Kongying v3 Offline Shell</title></head>
<body>
  <div id="q-app">
    <div class="leaflet-container" style="position:relative;width:800px;height:600px;"></div>
  </div>
  <script>
    ${buildMockBridgeAndMapScript()}
    (function setupKongyingFixture() {
      localStorage.removeItem('marked_layers');
      const fx = ${JSON.stringify(fixture)};
      const container = document.querySelector('.leaflet-container');
      const map = createMockLeafletMap(container, fx.map);

      const tpLayers = [];
      const bxLayers = [];
      for (const m of fx.markers) {
        const layerObj = {
          options: {
            data: m.data,
            icon: { options: { ...(m.icon_options || {}) } }
          },
          getLatLng: () => ({ lat: m.latlng.lat, lng: m.latlng.lng })
        };
        if (m.icon_options && m.icon_options.teleport) tpLayers.push(layerObj);
        else bxLayers.push(layerObj);
      }
      const makeGroup = (arr) => ({
        _map: map,
        getLayers: () => arr,
        eachLayer: (fn) => { for (const x of arr) fn(x); }
      });
      const teleportGroup = makeGroup(tpLayers);
      const bxGroup = makeGroup(bxLayers);
      map._layers = {
        1: teleportGroup,
        2: bxGroup
      };

      const mainStore = {
        selected_area: fx.mainStore.selected_area,
        selected_child_area: { ...fx.mainStore.selected_child_area },
        area_list: fx.mainStore.area_list,
        selected_item_list: fx.mainStore.selected_item_list
      };
      let refreshCount = 0;
      const vm = {
        map_tiles: { _map: map },
        BXGroup: bxGroup,
        teleport_group: teleportGroup,
        mainStore,
        underground_show: Boolean(fx.vm.underground_show),
        underground_opacity: Boolean(fx.vm.underground_opacity),
        refresh_layergroup: () => { refreshCount += 1; },
        underground_opacity_switch: () => { vm.underground_opacity = !vm.underground_opacity; }
      };
      const qapp = document.querySelector('#q-app');
      qapp.__vue_app__ = {
        config: {
          globalProperties: {
            $router: {
              currentRoute: {
                value: {
                  matched: [{ instances: { default: vm } }]
                }
              }
            }
          }
        }
      };
      window.__ghFixtureHandles = { map, vm, mainStore, getRefreshCount: () => refreshCount };
    })();
  </script>
  <script src="/__gh/coord_transform.js"></script>
  <script src="/__gh/provider_runtime.js"></script>
</body>
</html>`;
}

function buildHoyolabHtml() {
  return `<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>HoYoLAB Isolation Shell</title></head>
<body>
  <div class="leaflet-container" style="position:relative;width:800px;height:600px;"></div>
  <script>
    window.__gh_page_main_mock_sentinel = { intact: true, owner: 'page_main' };
    window.__gh_booted_build = 'mock-hoyolab-build';
  </script>
  <script src="/__gh/coord_transform.js"></script>
  <script src="/__gh/provider_runtime.js"></script>
</body>
</html>`;
}

async function main() {
  const chrome = findChrome();
  if (!chrome) {
    console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)');
    return 2;
  }

  for (const p of [COORD_TRANSFORM_PATH, PROVIDER_RUNTIME_PATH, VECTORS_PATH]) {
    if (!existsSync(p)) throw new Error(`Required file not found: ${p}`);
  }
  const appsampleFixture = JSON.parse(readFileSync(join(FIXTURES_DIR, 'appsample_state.json'), 'utf8'));
  const kongyingFixture = JSON.parse(readFileSync(join(FIXTURES_DIR, 'kongying_state.json'), 'utf8'));
  const vectorsDoc = JSON.parse(readFileSync(VECTORS_PATH, 'utf8'));
  const coordTransformSrc = readFileSync(COORD_TRANSFORM_PATH, 'utf8');
  const providerRuntimeSrc = readFileSync(PROVIDER_RUNTIME_PATH, 'utf8');

  const routeMap = new Map([
    [APPSAMPLE_URL, { contentType: 'text/html; charset=utf-8', body: buildAppSampleHtml(appsampleFixture) }],
    [KONGYING_URL, { contentType: 'text/html; charset=utf-8', body: buildKongyingHtml(kongyingFixture) }],
    [HOYOLAB_URL, { contentType: 'text/html; charset=utf-8', body: buildHoyolabHtml() }],
    ['/__gh/coord_transform.js', { contentType: 'application/javascript; charset=utf-8', body: coordTransformSrc }],
    ['/__gh/provider_runtime.js', { contentType: 'application/javascript; charset=utf-8', body: providerRuntimeSrc }]
  ]);

  const profile = mkdtempSync(join(tmpdir(), 'gh_provider_fixtures_'));
  const proc = spawn(chrome, [
    '--headless=new',
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--window-size=1440,900',
    'about:blank',
  ], { stdio: 'ignore' });
  const procExited = new Promise((res) => proc.once('exit', res));
  const sessions = [];

  const killAndClean = async () => {
    for (const c of sessions) c.close();
    if (process.platform === 'win32' && proc.pid) {
      spawnSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' });
    } else if (!proc.killed && proc.exitCode === null) {
      proc.kill();
    }
    await Promise.race([procExited, sleep(2000)]);
    await sleep(250);
    if (existsSync(profile)) {
      try {
        rmSync(profile, { recursive: true, force: true, maxRetries: 15, retryDelay: 250 });
      } catch (e) {
        console.warn(`[WARN] could not remove temp profile ${profile}: ${e && e.code ? e.code : e}`);
      }
    }
  };

  abortRun = () => {
    for (const c of sessions) c.close();
    if (process.platform === 'win32' && proc.pid) {
      spawnSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' });
    } else if (!proc.killed && proc.exitCode === null) {
      proc.kill();
    }
    try {
      rmSync(profile, { recursive: true, force: true });
    } catch (e) {
      console.warn(`[WARN] abort cleanup failed for ${profile}: ${e && e.code ? e.code : e}`);
    }
  };

  const counts = { pass: 0, fail: 0, 'known-fail': 0, xpass: 0 };
  function record(id, ok, detail, knownBug = null) {
    const hasKnown = Boolean(knownBug);
    let status;
    if (ok && !hasKnown) status = 'pass';
    else if (!ok && !hasKnown) status = 'fail';
    else if (!ok && hasKnown) status = 'known-fail';
    else status = 'xpass';
    counts[status]++;
    const suffix = hasKnown ? ` [known_bug${ok ? ' resolved' : ''}: ${knownBug}]` : '';
    console.log(`  [${status}] ${id}: ${detail}${suffix}`);
  }

  try {
    let port = null;
    for (let i = 0; i < 150 && !port; i++) {
      await sleep(100);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
    }
    if (!port) throw new Error('Chromium did not expose a DevTools port');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method, signal: AbortSignal.timeout(HTTP_TIMEOUT_MS) }).then((r) => r.json());

    const pageTarget = await http('/json/new?about:blank', 'PUT');
    const page = new Cdp(pageTarget.webSocketDebuggerUrl);
    sessions.push(page);
    await page.ready;
    await page.send('Runtime.enable');
    await page.send('Page.enable');

    // Intercept ALL http/https traffic so the entire test runs 100% offline.
    page.handlers.push((m) => {
      if (m.method !== 'Fetch.requestPaused') return;
      const reqUrl = m.params.request.url;
      let match = routeMap.get(reqUrl);
      if (!match && URL.canParse(reqUrl)) {
        match = routeMap.get(new URL(reqUrl).pathname);
      }
      const onInterceptErr = (err) => {
        if (page.ws.readyState === WebSocket.OPEN) {
          console.warn('[WARN] Fetch intercept reply failed:', err && err.message ? err.message : err);
        }
      };
      if (match) {
        page.send('Fetch.fulfillRequest', {
          requestId: m.params.requestId,
          responseCode: 200,
          responseHeaders: [
            { name: 'Content-Type', value: match.contentType },
            { name: 'Cache-Control', value: 'no-store' }
          ],
          body: Buffer.from(match.body, 'utf8').toString('base64')
        }).catch(onInterceptErr);
      } else {
        page.send('Fetch.failRequest', {
          requestId: m.params.requestId,
          errorReason: 'BlockedByClient'
        }).catch(onInterceptErr);
      }
    });
    await page.send('Fetch.enable', { patterns: [{ urlPattern: 'http://*' }, { urlPattern: 'https://*' }] });

    const navigateAndLoad = async (url) => {
      const loaded = page.waitForEvent('Page.loadEventFired', 15000);
      await page.send('Page.navigate', { url });
      await loaded;
      await sleep(150);
    };

    // =========================================================================
    // 1. AppSample Offline Fixture Suite (a, b, c, e, f, g)
    // =========================================================================
    await navigateAndLoad(APPSAMPLE_URL);

    // (a) Provider registration & SDK contract
    const regRes = await page.eval(`(() => {
      const fn = window.__GH_REGISTER_MAP_PROVIDER__;
      const map = window.__GH_MAP_PROVIDERS__;
      if (typeof fn !== 'function' || !(map instanceof Map)) {
        return { ok: false, reason: 'missing __GH_REGISTER_MAP_PROVIDER__ or __GH_MAP_PROVIDERS__' };
      }
      const hasBuiltins = map.has('kongying') && map.has('appsample');
      const rejectNull = fn(null) === false;
      const rejectMissingId = fn({ matchesLocation: () => false }) === false;
      const rejectMissingMatch = fn({ id: 'bad_custom' }) === false;
      const acceptValid = fn({ id: 'CustomTest', matchesLocation: () => false }) === true && map.has('customtest');
      map.delete('customtest');
      return {
        ok: hasBuiltins && rejectNull && rejectMissingId && rejectMissingMatch && acceptValid,
        hasBuiltins, rejectNull, rejectMissingId, rejectMissingMatch, acceptValid
      };
    })()`);
    record('provider_sdk_registration', regRes.ok, JSON.stringify(regRes));

    // (b) AppSample runtime boot, context, initial API posts, SYNC_POSITION_ONLY & LIVE_POSITION
    const asBootRes = await page.eval(`(async () => {
      const b = window.__GH_BRIDGE__;
      if (!b) return { ok: false, reason: 'missing __GH_BRIDGE__' };
      const ctx = b.getContext();
      const mapId = b.getCurrentMapId();
      await new Promise((r) => setTimeout(r, 120));
      const reqPaths = window.__ghRequests.map((r) => r.path);
      const hasCal = reqPaths.includes('/calibrate_provider');
      const hasNearby = reqPaths.includes('/nearby_pins');
      const hasLan = reqPaths.includes('/lan_pins');
      const calReq = window.__ghRequests.find((r) => r.path === '/calibrate_provider');
      const calAnchorCount = calReq?.body?.anchors?.length || 0;
      const calSwap = calReq?.body?.swap_lat_lng;

      // Send SYNC_POSITION_ONLY (Mondstadt mainland vector: cx=1500, cy=-2000 -> lat=0.3711837162, lng=0.6514836004)
      window.__ghSendServerEvent({
        type: 'SYNC_POSITION_ONLY',
        map_id: 2,
        lat: -2000,
        lng: 1500,
        heading: 90,
        camera_heading: 135,
        zoom_ratio_t: 0.5,
        has_world_coords: true
      });
      await new Promise((r) => setTimeout(r, 80));
      const setView = window.__ghSetViewCalls[window.__ghSetViewCalls.length - 1] || null;
      const markerAfterSync = window.__ghLastLeafletMarker ? window.__ghLastLeafletMarker.getLatLng() : null;

      // Send LIVE_POSITION (slight step) -> triggers panTo & updates arrow/cone CSS vars
      window.__ghSendServerEvent({
        type: 'LIVE_POSITION',
        map_id: 2,
        lat: -1990,
        lng: 1510,
        heading: 180,
        camera_heading: 225,
        is_underground: false
      });
      await new Promise((r) => setTimeout(r, 80));
      const panTo = window.__ghPanToCalls[window.__ghPanToCalls.length - 1] || null;
      const wrap = document.querySelector('.gh-player-marker-wrap');
      const cssDir = wrap ? wrap.style.getPropertyValue('--gh-dir').trim() : null;
      const cssRot = wrap ? wrap.style.getPropertyValue('--gh-rot').trim() : null;
      const hasFov = wrap ? wrap.classList.contains('gh-has-fov') : false;

      return {
        ok: b.providerId === 'appsample' && Boolean(ctx) && mapId === 2 &&
            hasCal && calAnchorCount === 8 && calSwap === false &&
            hasNearby && hasLan &&
            Boolean(setView) && Boolean(markerAfterSync) && Boolean(panTo) &&
            parseFloat(cssDir) === 180 && parseFloat(cssRot) === 225 && hasFov,
        providerId: b.providerId,
        mapId,
        hasCal,
        calAnchorCount,
        calSwap,
        hasNearby,
        hasLan,
        setView,
        markerAfterSync,
        panTo,
        cssDir,
        cssRot,
        hasFov
      };
    })()`);
    record('appsample_boot_sync_and_bridge', asBootRes.ok, `provider=${asBootRes.providerId} mapId=${asBootRes.mapId} anchors=${asBootRes.calAnchorCount} dir=${asBootRes.cssDir} rot=${asBootRes.cssRot}`);

    // (c) Coordinate transforms against tests/fixtures/multimap/transform_vectors.json (all providers & zones)
    const vecRes = await page.eval(`(() => {
      const doc = ${JSON.stringify(vectorsDoc)};
      const CT = window.GHCoordTransform;
      if (!CT) return { ok: false, errors: ['window.GHCoordTransform missing'] };
      const tolF = doc.tolerances.forward_abs;
      const tolL = doc.tolerances.leaflet_abs;
      const tolR = doc.tolerances.roundtrip_canonical_abs;
      const errors = [];
      let checked = 0;
      for (const v of doc.vectors) {
        const tf = CT.getTransform(v.provider, v.map_id);
        if (!tf) { errors.push(v.id + ': missing transform'); continue; }
        if (Boolean(tf.swap_lat_lng) !== Boolean(v.swap_lat_lng)) {
          errors.push(v.id + ': swap_lat_lng mismatch');
        }
        const fwd = CT.forward(tf, v.canonical.x, v.canonical.y);
        if (!fwd || Math.abs(fwd[0] - v.expected_provider.x) > tolF || Math.abs(fwd[1] - v.expected_provider.y) > tolF) {
          errors.push(v.id + ': forward mismatch got=' + JSON.stringify(fwd));
        }
        const ll = CT.canonicalToLeaflet(tf, v.canonical.y, v.canonical.x);
        if (!ll || Math.abs(ll[0] - v.expected_leaflet.lat) > tolL || Math.abs(ll[1] - v.expected_leaflet.lng) > tolL) {
          errors.push(v.id + ': leaflet mismatch got=' + JSON.stringify(ll));
        }
        const rt = CT.leafletToCanonical(tf, ll[0], ll[1]);
        if (!rt || Math.abs(rt.lng - v.canonical.x) > tolR || Math.abs(rt.lat - v.canonical.y) > tolR) {
          errors.push(v.id + ': roundtrip mismatch got=' + JSON.stringify(rt));
        }
        checked++;
      }
      return { ok: errors.length === 0 && checked === doc.vectors.length, checked, errors: errors.slice(0, 5) };
    })()`);
    record('coord_transform_vectors_parity', vecRes.ok, `checked=${vecRes.checked}/${vectorsDoc.vectors.length}${vecRes.errors.length ? ' errors=' + vecRes.errors.join('; ') : ''}`);

    // (c2 + f) AppSample visible pins: namespacing, landmark exclusion, inverse coordinates across Mainland + Inazuma + Easybreeze
    const asPinsRes = await page.eval(`(() => {
      // Enable o111 alongside default types to test Inazuma + Easybreeze + underground pins
      window.__ghFixtureHandles.store.__setTypes(['o3', 'o5', 'o6', 'o111']);
      const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
      const ids = pins.map((p) => p.id);
      const allNamespaced = pins.length > 0 && pins.every((p) => p.id.startsWith('appsample:'));
      const noLandmarks = !ids.includes('appsample:2608') && !ids.includes('appsample:2587');
      const hasMainland = pins.find((p) => p.id === 'appsample:301');
      const hasInazuma = pins.find((p) => p.id === 'appsample:16514');
      const hasEasybreeze = pins.find((p) => p.id === 'appsample:87284');
      // Verify inverse pin coordinates match GHCoordTransform.leafletToCanonical
      const tf = window.GHCoordTransform.getTransform('appsample', 2);
      const expIna = window.GHCoordTransform.leafletToCanonical(tf, -0.22243, 0.59061);
      const expEasy = window.GHCoordTransform.leafletToCanonical(tf, 0.054, -0.63953);
      const inaMatch = Boolean(hasInazuma && Math.abs(hasInazuma.lat - expIna.lat) < 1e-6 && Math.abs(hasInazuma.lng - expIna.lng) < 1e-6);
      const easyMatch = Boolean(hasEasybreeze && Math.abs(hasEasybreeze.lat - expEasy.lat) < 1e-6 && Math.abs(hasEasybreeze.lng - expEasy.lng) < 1e-6);
      // Also check honest surface vs underground classification
      const surfacePin301 = pins.find((p) => p.id === 'appsample:301');
      const ugPin20413 = pins.find((p) => p.id === 'appsample:20413');
      return {
        ok: allNamespaced && noLandmarks && Boolean(hasMainland) && inaMatch && easyMatch,
        count: pins.length,
        allNamespaced,
        noLandmarks,
        inaMatch,
        easyMatch,
        surfacePin301Ug: surfacePin301 ? { is_underground: surfacePin301.is_underground, floor_id: surfacePin301.floor_id } : null,
        ugPin20413Ug: ugPin20413 ? { is_underground: ugPin20413.is_underground, floor_id: ugPin20413.floor_id } : null
      };
    })()`);
    record('appsample_pin_namespacing_and_inverse_coords', asPinsRes.ok, `count=${asPinsRes.count} namespaced=${asPinsRes.allNamespaced} noLandmarks=${asPinsRes.noLandmarks} inazuma=${asPinsRes.inaMatch} easybreeze=${asPinsRes.easyMatch}`);

    // Honest live-schema check: surface AppSample pins have zoom level=6 and NO layer; underground pins have layer.{groupId,floorId}
    const asUgAccurate = Boolean(
      asPinsRes.surfacePin301Ug && asPinsRes.surfacePin301Ug.is_underground === false &&
      asPinsRes.ugPin20413Ug && asPinsRes.ugPin20413Ug.is_underground === true && asPinsRes.ugPin20413Ug.floor_id === '10'
    );
    record(
      'appsample_live_surface_vs_underground_classification',
      asUgAccurate,
      `surface301=${JSON.stringify(asPinsRes.surfacePin301Ug)} ug20413=${JSON.stringify(asPinsRes.ugPin20413Ug)} (expected floor_id="10")`
    );

    // (e) Target lock clearing on AppSample
    const asLockRes = await page.eval(`(async () => {
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'appsample:301', name: 'Anemoculus', map_id: 2, lat: -550, lng: -1050 }
      });
      await new Promise((r) => setTimeout(r, 50));
      const pill = document.getElementById('gh-target-lock-pill');
      const visibleOwn = Boolean(pill && pill.classList.contains('is-visible'));

      // Foreign provider pin (hoyolab:123) clears pill
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'hoyolab:123', name: 'HoYo Pin', map_id: 2, lat: 0, lng: 0 }
      });
      await new Promise((r) => setTimeout(r, 50));
      const clearedOnHoyolab = Boolean(pill && !pill.classList.contains('is-visible'));

      // Re-lock own pin, then foreign kongying:456 clears pill
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'appsample:301', name: 'Anemoculus', map_id: 2, lat: -550, lng: -1050 }
      });
      await new Promise((r) => setTimeout(r, 50));
      const relocked = Boolean(pill && pill.classList.contains('is-visible'));
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'kongying:456', name: 'KY Pin', map_id: 2, lat: 0, lng: 0 }
      });
      await new Promise((r) => setTimeout(r, 50));
      const clearedOnKongying = Boolean(pill && !pill.classList.contains('is-visible'));

      // Re-lock own pin, then null clears pill
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'appsample:301', name: 'Anemoculus', map_id: 2, lat: -550, lng: -1050 }
      });
      await new Promise((r) => setTimeout(r, 50));
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: null
      });
      await new Promise((r) => setTimeout(r, 50));
      const clearedOnNull = Boolean(pill && !pill.classList.contains('is-visible'));

      return {
        ok: visibleOwn && clearedOnHoyolab && relocked && clearedOnKongying && clearedOnNull,
        visibleOwn, clearedOnHoyolab, relocked, clearedOnKongying, clearedOnNull
      };
    })()`);
    record('appsample_target_lock_clearing', asLockRes.ok, JSON.stringify(asLockRes));

    // (f + g) AppSample routing isolation + guest mark error & toast
    const asMarkRes = await page.eval(`(async () => {
      const beforeFeedback = window.__ghRequests.filter((r) => r.path === '/action_feedback').length;
      const beforeTotal = window.__ghRequests.length;
      // Ignored: pure-digit HoYoLAB id, foreign kongying id, mismatched target_provider, mismatched owner_tab_id, GH_CMD_EXPORT_PINS
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: '123456', action: 'mark', _seq: 10 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'kongying:1288', action: 'mark', _seq: 11 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'appsample:301', action: 'mark', target_provider: 'kongying', _seq: 12 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'appsample:301', action: 'mark', owner_tab_id: 'other_tab_xyz', _seq: 13 });
      window.__ghSendServerEvent({ type: 'GH_CMD_EXPORT_PINS', _seq: 14 });
      await new Promise((r) => setTimeout(r, 120));
      const afterIgnoredFeedback = window.__ghRequests.filter((r) => r.path === '/action_feedback').length;
      const afterIgnoredExport = window.__ghRequests.slice(beforeTotal).filter((r) => r.path.includes('export')).length;

      // Valid guest mark on AppSample -> expects status='error', sign-in detail, and visible #gh-mini-toast
      window.__ghSendServerEvent({
        type: 'NAV_MARK_PIN',
        pin_id: 'appsample:301',
        action: 'mark',
        target_provider: 'appsample',
        owner_tab_id: window.__GH_BRIDGE__.tabId,
        _seq: 15
      });
      await new Promise((r) => setTimeout(r, 150));
      const feedbacks = window.__ghRequests.filter((r) => r.path === '/action_feedback');
      const lastFb = feedbacks[feedbacks.length - 1] || null;
      const toastEl = document.getElementById('gh-mini-toast');
      const toastVisible = Boolean(toastEl && toastEl.classList.contains('is-visible'));
      const toastText = toastEl ? toastEl.textContent : '';
      const ignoredAll = (afterIgnoredFeedback === beforeFeedback) && afterIgnoredExport === 0;
      const guestErrorOk = Boolean(
        lastFb && lastFb.body &&
        lastFb.body.status === 'error' &&
        lastFb.body.pin_id === 'appsample:301' &&
        String(lastFb.body.detail || '').includes('Sign in on AppSample to mark/unmark pins') &&
        toastVisible && toastText.includes('Sign in on AppSample to mark/unmark pins')
      );
      return {
        ok: ignoredAll && guestErrorOk,
        ignoredAll,
        guestErrorOk,
        feedbackDetail: lastFb?.body?.detail || null,
        toastVisible,
        toastText
      };
    })()`);
    record('appsample_routing_isolation_and_guest_mark_error', asMarkRes.ok, `ignoredAll=${asMarkRes.ignoredAll} detail="${asMarkRes.feedbackDetail}" toastVisible=${asMarkRes.toastVisible}`);

    // =========================================================================
    // 2. Kongying v3 Offline Fixture Suite (b, c, e, f, g)
    // =========================================================================
    await navigateAndLoad(KONGYING_URL);

    const kyBootRes = await page.eval(`(async () => {
      const b = window.__GH_BRIDGE__;
      if (!b) return { ok: false, reason: 'missing __GH_BRIDGE__' };
      const ctx = b.getContext();
      const mapId = b.getCurrentMapId();
      await new Promise((r) => setTimeout(r, 120));
      const reqPaths = window.__ghRequests.map((r) => r.path);
      const calReq = window.__ghRequests.find((r) => r.path === '/calibrate_provider');
      const calAnchorCount = calReq?.body?.anchors?.length || 0;
      const calSwap = calReq?.body?.swap_lat_lng;

      // SYNC_POSITION_ONLY at canonical (1000, -500) -> expected Leaflet [2863.049838, -4598.8319915] (swap_lat_lng=true)
      window.__ghSendServerEvent({
        type: 'SYNC_POSITION_ONLY',
        map_id: 2,
        lat: -500,
        lng: 1000,
        heading: 45,
        camera_heading: 60,
        zoom_ratio_t: 0.5,
        has_world_coords: true
      });
      await new Promise((r) => setTimeout(r, 80));
      const setView = window.__ghSetViewCalls[window.__ghSetViewCalls.length - 1] || null;
      const domMarker = document.getElementById('gh-player-marker-dom');
      const wrap = domMarker ? domMarker.querySelector('.gh-player-marker-wrap') : null;
      const cssDir = wrap ? wrap.style.getPropertyValue('--gh-dir').trim() : null;
      const cssRot = wrap ? wrap.style.getPropertyValue('--gh-rot').trim() : null;

      // LIVE_POSITION panTo
      window.__ghSendServerEvent({
        type: 'LIVE_POSITION',
        map_id: 2,
        lat: -490,
        lng: 1010,
        heading: 120,
        camera_heading: 150
      });
      await new Promise((r) => setTimeout(r, 80));
      const panTo = window.__ghPanToCalls[window.__ghPanToCalls.length - 1] || null;
      const cssDirLive = wrap ? wrap.style.getPropertyValue('--gh-dir').trim() : null;
      const cssRotLive = wrap ? wrap.style.getPropertyValue('--gh-rot').trim() : null;
      const setViewCoordsOk = Boolean(setView && Math.abs(setView.lat - 2863.049838) < 1e-4 && Math.abs(setView.lng - (-4598.8319915)) < 1e-4);

      return {
        ok: b.providerId === 'kongying' && Boolean(ctx) && mapId === 2 &&
            calAnchorCount === 6 && calSwap === true &&
            reqPaths.includes('/nearby_pins') && reqPaths.includes('/lan_pins') &&
            setViewCoordsOk && Boolean(domMarker) && Boolean(panTo) &&
            parseFloat(cssDir) === 45 && parseFloat(cssRot) === 60 &&
            parseFloat(cssDirLive) === 120 && parseFloat(cssRotLive) === 150,
        providerId: b.providerId,
        mapId,
        calAnchorCount,
        calSwap,
        setView,
        setViewCoordsOk,
        hasDomMarker: Boolean(domMarker),
        hasPanTo: Boolean(panTo),
        cssDir,
        cssRot,
        cssDirLive,
        cssRotLive
      };
    })()`);
    record('kongying_boot_sync_and_dom_marker', kyBootRes.ok, `provider=${kyBootRes.providerId} mapId=${kyBootRes.mapId} anchors=${kyBootRes.calAnchorCount} swap=${kyBootRes.calSwap} setViewOk=${kyBootRes.setViewCoordsOk}`);

    // Kongying visible pins: namespacing, landmark exclusion, inverse coords
    const kyPinsRes = await page.eval(`(() => {
      const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
      const ids = pins.map((p) => p.id);
      const allNamespaced = pins.length === 6 && pins.every((p) => p.id.startsWith('kongying:'));
      const noLandmarks = !ids.includes('kongying:6278') && !ids.includes('kongying:6283');
      const p1288 = pins.find((p) => p.id === 'kongying:1288');
      const tf = window.GHCoordTransform.getTransform('kongying', 2);
      const exp = window.GHCoordTransform.leafletToCanonical(tf, 1319.8804911037, -3606.7583012494);
      const invMatch = Boolean(p1288 && Math.abs(p1288.lat - exp.lat) < 1e-6 && Math.abs(p1288.lng - exp.lng) < 1e-6);
      return { ok: allNamespaced && noLandmarks && invMatch, count: pins.length, allNamespaced, noLandmarks, invMatch };
    })()`);
    record('kongying_pin_namespacing_and_inverse_coords', kyPinsRes.ok, JSON.stringify(kyPinsRes));

    // Kongying target lock clearing + routing isolation + mark/unmark in localStorage
    const kyMarkAndLockRes = await page.eval(`(async () => {
      // Target lock clearing
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'kongying:1288', name: 'Chest', map_id: 2, lat: 243, lng: -155 }
      });
      await new Promise((r) => setTimeout(r, 40));
      const pill = document.getElementById('gh-target-lock-pill');
      const lockedOwn = Boolean(pill && pill.classList.contains('is-visible'));
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'appsample:301', name: 'Foreign', map_id: 2, lat: 0, lng: 0 }
      });
      await new Promise((r) => setTimeout(r, 40));
      const clearedForeign = Boolean(pill && !pill.classList.contains('is-visible'));

      // Ignored foreign/mismatched events
      const beforeFb = window.__ghRequests.filter((r) => r.path === '/action_feedback').length;
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: '99999', action: 'mark', _seq: 20 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'appsample:301', action: 'mark', _seq: 21 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'kongying:1288', action: 'mark', target_provider: 'appsample', _seq: 22 });
      window.__ghSendServerEvent({ type: 'NAV_MARK_PIN', pin_id: 'kongying:1288', action: 'mark', owner_tab_id: 'wrong_tab', _seq: 23 });
      window.__ghSendServerEvent({ type: 'GH_CMD_EXPORT_PINS', _seq: 24 });
      await new Promise((r) => setTimeout(r, 120));
      const ignoredOk = window.__ghRequests.filter((r) => r.path === '/action_feedback').length === beforeFb &&
        localStorage.getItem('marked_layers') === null;

      // Mark kongying:1288
      window.__ghSendServerEvent({
        type: 'NAV_MARK_PIN',
        pin_id: 'kongying:1288',
        action: 'mark',
        target_provider: 'kongying',
        owner_tab_id: window.__GH_BRIDGE__.tabId,
        _seq: 25
      });
      await new Promise((r) => setTimeout(r, 140));
      const afterMarkRaw = localStorage.getItem('marked_layers');
      const afterMarkList = JSON.parse(afterMarkRaw || '[]').map(String);
      const fbMark = window.__ghRequests.filter((r) => r.path === '/action_feedback').slice(-1)[0] || null;

      // Unmark kongying:1288
      window.__ghSendServerEvent({
        type: 'NAV_MARK_PIN',
        pin_id: 'kongying:1288',
        action: 'unmark',
        target_provider: 'kongying',
        owner_tab_id: window.__GH_BRIDGE__.tabId,
        _seq: 26
      });
      await new Promise((r) => setTimeout(r, 140));
      const afterUnmarkRaw = localStorage.getItem('marked_layers');
      const afterUnmarkList = JSON.parse(afterUnmarkRaw || '[]').map(String);
      const fbUnmark = window.__ghRequests.filter((r) => r.path === '/action_feedback').slice(-1)[0] || null;
      const refreshCount = window.__ghFixtureHandles.getRefreshCount();

      const markOk = afterMarkList.includes('1288') && fbMark?.body?.status === 'ok' && fbMark?.body?.action === 'mark';
      const unmarkOk = !afterUnmarkList.includes('1288') && fbUnmark?.body?.status === 'ok' && fbUnmark?.body?.action === 'unmark';
      return {
        ok: lockedOwn && clearedForeign && ignoredOk && markOk && unmarkOk && refreshCount >= 2,
        lockedOwn, clearedForeign, ignoredOk, markOk, unmarkOk, refreshCount,
        afterMarkList, afterUnmarkList
      };
    })()`);
    record('kongying_target_lock_isolation_and_mark_unmark', kyMarkAndLockRes.ok, `locked=${kyMarkAndLockRes.lockedOwn} cleared=${kyMarkAndLockRes.clearedForeign} ignored=${kyMarkAndLockRes.ignoredOk} mark=${kyMarkAndLockRes.markOk} unmark=${kyMarkAndLockRes.unmarkOk}`);

    // =========================================================================
    // 3. HoYoLAB Isolation Suite (d)
    // =========================================================================
    await navigateAndLoad(HOYOLAB_URL);

    const hlRes = await page.eval(`(() => {
      return {
        ok: typeof window.__GH_REGISTER_MAP_PROVIDER__ === 'undefined' &&
            typeof window.__GH_BRIDGE__ === 'undefined' &&
            document.getElementById('gh-provider-styles') === null &&
            Boolean(window.__gh_page_main_mock_sentinel && window.__gh_page_main_mock_sentinel.intact === true) &&
            window.__gh_booted_build === 'mock-hoyolab-build',
        hasRegister: typeof window.__GH_REGISTER_MAP_PROVIDER__,
        hasBridge: typeof window.__GH_BRIDGE__,
        hasStyles: Boolean(document.getElementById('gh-provider-styles')),
        sentinelIntact: Boolean(window.__gh_page_main_mock_sentinel && window.__gh_page_main_mock_sentinel.intact === true),
        bootedBuild: window.__gh_booted_build
      };
    })()`);
    record('hoyolab_isolation_early_exit', hlRes.ok, JSON.stringify(hlRes));
  } finally {
    await killAndClean();
  }

  console.log(`Provider fixtures: pass ${counts.pass} | fail ${counts.fail} | known-fail ${counts['known-fail']} | xpass ${counts.xpass}`);
  return counts.fail === 0 ? 0 : 1;
}

const watchdog = setTimeout(() => {
  console.error(`[ERROR] provider DOM fixtures exceeded ${WATCHDOG_MS} ms watchdog; aborting`);
  if (abortRun) abortRun();
  process.exit(1);
}, WATCHDOG_MS);
void watchdog;

main().then((code) => process.exit(code), (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
});
