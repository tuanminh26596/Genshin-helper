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
const I18N_PATH = process.env.GH_I18N_PATH
  ? resolve(process.env.GH_I18N_PATH)
  : join(ROOT, 'extension', 'i18n.js');
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
    window.__ghFitBoundsCalls = [];
    window.__ghDownloads = [];
    window.__ghMockRouteState = { ok: true, active: false };
    window.__ghMockRouteExportDoc = null;

    (function installBlobDownloadCapture() {
      const OrigBlob = window.Blob;
      const blobTexts = new WeakMap();
      const urlTexts = new Map();
      window.Blob = function (parts, opts) {
        const b = new OrigBlob(parts, opts);
        try {
          blobTexts.set(b, (parts || []).map((p) => String(p)).join(''));
        } catch (e) {}
        return b;
      };
      window.Blob.prototype = OrigBlob.prototype;
      const origCreateUrl = URL.createObjectURL.bind(URL);
      URL.createObjectURL = (blob) => {
        const u = origCreateUrl(blob);
        if (blob && blobTexts.has(blob)) urlTexts.set(u, blobTexts.get(blob));
        return u;
      };
      const origClick = HTMLAnchorElement.prototype.click;
      HTMLAnchorElement.prototype.click = function () {
        if (this.hasAttribute('download')) {
          const href = this.getAttribute('href') || '';
          window.__ghDownloads.push({
            filename: this.getAttribute('download') || '',
            href,
            content: urlTexts.has(href) ? urlTexts.get(href) : null
          });
          return;
        }
        return origClick.apply(this, arguments);
      };
    })();

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
      } else if (rec.path === '/route') {
        const act = rec.body && rec.body.action ? String(rec.body.action) : '';
        if (rec.method === 'GET' || act === 'status') {
          data = window.__ghMockRouteState || { ok: true, active: false };
        } else if (act === 'export') {
          data = window.__ghMockRouteExportDoc
            ? { ok: true, filename: window.__ghMockRouteExportDoc.filename || 'route.ghroute.json', doc: window.__ghMockRouteExportDoc.doc }
            : { ok: false, error: 'No active route' };
        } else if (act === 'clear') {
          window.__ghMockRouteState = { ok: true, active: false };
          data = window.__ghMockRouteState;
        } else if (act === 'reset' && window.__ghMockRouteState && window.__ghMockRouteState.active) {
          window.__ghMockRouteState = { ...window.__ghMockRouteState, active_section: 0, section_done: 0 };
          data = window.__ghMockRouteState;
        } else if (act === 'section' && window.__ghMockRouteState && window.__ghMockRouteState.active) {
          window.__ghMockRouteState = { ...window.__ghMockRouteState, active_section: Number(rec.body.section_index || 0) };
          data = window.__ghMockRouteState;
        }
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
        fitBounds: (bounds, opts) => {
          window.__ghFitBoundsCalls.push({ bounds, opts });
          if (Array.isArray(bounds) && bounds.length === 2) {
            center = {
              lat: (Number(bounds[0][0]) + Number(bounds[1][0])) * 0.5,
              lng: (Number(bounds[0][1]) + Number(bounds[1][1])) * 0.5
            };
          }
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
        },
        emit: (evStr) => {
          for (const ev of String(evStr).split(/\s+/)) {
            for (const fn of (listeners[ev] || []).slice()) fn();
          }
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
    localStorage.removeItem('gh_lang_cache');
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

      const MAP_SLUG_TO_ID = {
        teyvat: 2,
        enkanomiya: 7,
        chasm: 9,
        'sea-of-bygone-eras': 34,
        ancient_sacred_mountain: 36,
        temple_of_space: 37,
        frost_moon: 40
      };

      let state = {
        mapKey: fx.storeState.mapKey,
        types: [...fx.storeState.types],
        sid: fx.storeState.sid,
        user: fx.storeState.user,
        found: new Set(fx.storeState.found || []),
        overlay: { ...(fx.storeState.overlay || { mode: 'none' }) },
        selectedId: null,
        popupOpen: false,
        markersComplete: fx.storeState.markersComplete,
        markers: fx.markers
      };
      const subs = new Set();
      const notifySubs = () => { for (const fn of subs) fn(state); };
      const store = {
        getState: () => state,
        setState: (patch) => {
          const next = typeof patch === 'function' ? patch(state) : patch;
          state = { ...state, ...next };
          notifySubs();
        },
        subscribe: (fn) => { subs.add(fn); return () => subs.delete(fn); },
        __setTypes: (nextTypes) => {
          state = { ...state, types: [...nextTypes] };
          notifySubs();
        },
        __setMapKey: (nextKey) => {
          state = { ...state, mapKey: String(nextKey) };
          notifySubs();
        }
      };
      const indexMap = new Map(fx.markers.map((m) => [String(m.id), m]));
      const catalog = {
        allMarkers: fx.markers,
        officialMarkers: fx.markers,
        get all() { return this.allMarkers; },
        get: (id) => indexMap.get(String(id)) || null,
        markerIconUrl: (m) => fx.iconUrlTemplate.replace('{type}', String(m.type || '')),
        // Same rule as the live catalog.markerImageUrl: an image only when m.img and level >= imageMinLevel.
        markerImageUrl(m) {
          const { markerImageUrl: t, rules: r } = this.config;
          if (m.img && t && !(m.level < r.imageMinLevel)) return t.replace('{id}', m.id);
          return undefined;
        },
        config: {
          maps: { teyvat: { id: 2 } },
          markerImageUrl: fx.imageUrlTemplate,
          rules: { imageMinLevel: fx.imageMinLevel, commentMinLevel: fx.commentMinLevel },
          features: { comments: true }
        }
      };

      const overlays = [];
      const seenOv = new Set();
      for (const m of fx.markers) {
        if (m.layer && m.layer.floorId && m.layer.groupId) {
          const k = m.layer.groupId + ':' + m.layer.floorId;
          if (!seenOv.has(k)) {
            seenOv.add(k);
            overlays.push({
              id: Number(m.layer.floorId),
              groupId: Number(m.layer.groupId),
              name: 'B' + m.layer.floorId,
              url: 'https://game-cdn.appsample.com/gim/overlays/' + m.layer.floorId + '.png',
              bounds: {
                west: Number(m.position.x) - 0.02,
                south: Number(m.position.y) - 0.02,
                east: Number(m.position.x) + 0.02,
                north: Number(m.position.y) + 0.02
              }
            });
          }
        }
      }

      const overlayCalls = [];
      const adapter = {
        map,
        overlays: new Map(overlays.map((o) => [o.id, o])),
        activeOverlayIds: [],
        showOverlays: (ids) => {
          if (!Array.isArray(ids)) {
            throw new TypeError('AppSample adapter.showOverlays expects array of overlay IDs');
          }
          adapter.activeOverlayIds = ids.map(Number);
          overlayCalls.push([...adapter.activeOverlayIds]);
        }
      };

      const renderInfoWindow = (markerId) => {
        let infoWin = document.querySelector('[data-testid="info-window"]');
        if (!markerId) {
          if (infoWin) infoWin.remove();
          return;
        }
        if (!infoWin) {
          infoWin = document.createElement('div');
          infoWin.setAttribute('data-testid', 'info-window');
          const titleP = document.createElement('p');
          titleP.className = 'font-semibold';
          const footer = document.createElement('footer');
          infoWin.append(titleP, footer);
          document.body.appendChild(infoWin);
        }
        const m = indexMap.get(String(markerId));
        infoWin.setAttribute('data-marker-id', String(markerId));
        const titleP = infoWin.querySelector('p.font-semibold');
        if (titleP) titleP.textContent = (m && (m.title || (fx.messages && fx.messages.markers && fx.messages.markers[m.type]))) || String(markerId);
      };

      const mapSync = {
        adapter,
        deps: {
          currentMap: () => ({ id: MAP_SLUG_TO_ID[state.mapKey] || 2, overlays }),
          onMarkerClick: (id) => {
            const m = indexMap.get(String(id));
            if (!m) return;
            const nextOv = (m.layer && m.layer.floorId && m.layer.groupId)
              ? { mode: 'group', groupId: Number(m.layer.groupId), floorId: Number(m.layer.floorId) }
              : { mode: 'none' };
            state = { ...state, selectedId: String(id), popupOpen: true, overlay: nextOv };
            mapSync.applyOverlay();
            renderInfoWindow(String(id));
            notifySubs();
          }
        },
        applyOverlay: () => {
          const ov = state.overlay || { mode: 'none' };
          if (ov.mode === 'none') adapter.showOverlays([]);
          else if (ov.mode === 'group' && ov.floorId) adapter.showOverlays([Number(ov.floorId)]);
          else if (ov.mode === 'all') adapter.showOverlays(overlays.map((o) => o.id));
        },
        applyPopup: () => {
          renderInfoWindow(state.popupOpen ? state.selectedId : null);
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
        mapSync
      };
      container['__reactFiber$fixture'] = {
        memoizedProps: {},
        return: {
          memoizedProps: { value: appCtx },
          return: null
        }
      };
      window.__ghFixtureHandles = { map, store, catalog, adapter, mapSync, overlayCalls, renderInfoWindow };
    })();
  </script>
  <script src="/__gh/i18n.js"></script>
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
    localStorage.removeItem('gh_lang_cache');
    ${buildMockBridgeAndMapScript()}
    (function setupKongyingFixture() {
      localStorage.removeItem('marked_layers');
      const fx = ${JSON.stringify(fixture)};
      const container = document.querySelector('.leaflet-container');
      const map = createMockLeafletMap(container, fx.map);

      const mainStore = {
        selected_area: fx.mainStore.selected_area,
        selected_child_area: { ...fx.mainStore.selected_child_area },
        area_list: fx.mainStore.area_list,
        selected_item_list: fx.mainStore.selected_item_list
      };
      let refreshCount = 0;
      const vm = {
        map_tiles: { _map: map },
        BXGroup: null,
        teleport_group: null,
        mainStore,
        underground_show: Boolean(fx.vm.underground_show),
        underground_opacity: Boolean(fx.vm.underground_opacity),
        popup_window_show: false,
        handle_layer: { target: null },
        refresh_layergroup: () => { refreshCount += 1; },
        underground_opacity_switch: () => { vm.underground_opacity = !vm.underground_opacity; },
        popup_callback: (ev) => {
          const target = (ev && ev.target) || ev;
          if (!target || !target.options || !target.options.data) return;
          vm.handle_layer = { target };
          vm.popup_window_show = true;
          let pop = document.getElementById('popup_window');
          if (!pop) {
            pop = document.createElement('div');
            pop.id = 'popup_window';
            const content = document.createElement('div');
            content.className = 'content';
            const toggleBtn = document.createElement('div');
            toggleBtn.className = 'toggle_btn';
            pop.append(content, toggleBtn);
            document.body.appendChild(pop);
          }
          pop.style.display = 'block';
          pop.setAttribute('data-marker-id', String(target.options.data.id));
        }
      };

      const tpLayers = [];
      const bxLayers = [];
      for (const m of fx.markers) {
        const layerObj = {
          options: {
            data: m.data,
            icon: { options: { ...(m.icon_options || {}) } }
          },
          getLatLng: () => ({ lat: m.latlng.lat, lng: m.latlng.lng }),
          fire: (evName, payload) => {
            if (evName === 'click') {
              vm.popup_callback(payload || { target: layerObj, latlng: layerObj.getLatLng() });
            }
          }
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
      vm.BXGroup = bxGroup;
      vm.teleport_group = teleportGroup;
      map._layers = {
        1: teleportGroup,
        2: bxGroup
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
      window.__ghFixtureHandles = {
        map,
        vm,
        mainStore,
        getRefreshCount: () => refreshCount,
        setUndergroundShow: (flag) => { vm.underground_show = Boolean(flag); },
        setAreaCode: (code) => {
          const match = (mainStore.area_list || []).find((a) => a && a.code === code);
          if (match) mainStore.selected_child_area = { ...match };
        }
      };
    })();
  </script>
  <script src="/__gh/i18n.js"></script>
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

  for (const p of [I18N_PATH, COORD_TRANSFORM_PATH, PROVIDER_RUNTIME_PATH, VECTORS_PATH]) {
    if (!existsSync(p)) throw new Error(`Required file not found: ${p}`);
  }
  const appsampleFixture = JSON.parse(readFileSync(join(FIXTURES_DIR, 'appsample_state.json'), 'utf8'));
  const kongyingFixture = JSON.parse(readFileSync(join(FIXTURES_DIR, 'kongying_state.json'), 'utf8'));
  const vectorsDoc = JSON.parse(readFileSync(VECTORS_PATH, 'utf8'));
  const i18nSrc = readFileSync(I18N_PATH, 'utf8');
  const coordTransformSrc = readFileSync(COORD_TRANSFORM_PATH, 'utf8');
  const providerRuntimeSrc = readFileSync(PROVIDER_RUNTIME_PATH, 'utf8');

  const routeMap = new Map([
    [APPSAMPLE_URL, { contentType: 'text/html; charset=utf-8', body: buildAppSampleHtml(appsampleFixture) }],
    [KONGYING_URL, { contentType: 'text/html; charset=utf-8', body: buildKongyingHtml(kongyingFixture) }],
    [HOYOLAB_URL, { contentType: 'text/html; charset=utf-8', body: buildHoyolabHtml() }],
    ['/__gh/i18n.js', { contentType: 'application/javascript; charset=utf-8', body: i18nSrc }],
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

    // Guide content: AppSample pins with a catalog image must carry it as image_url, so the app's
    // Guide card can show it (pins without one stay empty).
    const asGuideRes = await page.eval(`(() => {
      window.__ghFixtureHandles.store.__setTypes(['o3', 'o5', 'o6', 'o111', 'o185']);
      const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
      const withImg = pins.find((p) => p.id === 'appsample:15130');
      const noImg = pins.find((p) => p.id === 'appsample:301');
      window.__ghFixtureHandles.store.__setTypes(['o3', 'o5', 'o6', 'o111']);
      return {
        withImg: withImg ? withImg.image_url : null,
        noImg: noImg ? noImg.image_url : null
      };
    })()`);
    record(
      'appsample_guide_image_from_catalog',
      asGuideRes.withImg === 'https://game-cdn.appsample.com/gim/item-imgs/15130.jpg' && asGuideRes.noImg === '',
      `15130=${JSON.stringify(asGuideRes.withImg)} 301=${JSON.stringify(asGuideRes.noImg)}`
    );

    // Community guide key: the app fetches the comments thread (summary + player screenshots)
    // only for markers whose popup shows comments, keyed by marker type and id.
    const asCommunityRes = await page.eval(`(() => {
      const h = window.__ghFixtureHandles;
      h.store.__setTypes(['o3', 'o5', 'o6', 'o111', 'o185']);
      const pick = () => {
        const pins = window.__GH_BRIDGE__.queryNearbyUncollectedPins();
        const p6 = pins.find((p) => p.id === 'appsample:301');
        const p3 = pins.find((p) => p.id === 'appsample:15130');
        return { p6: p6 && [p6.type_code, p6.community_guide], p3: p3 && [p3.type_code, p3.community_guide] };
      };
      const on = pick();
      h.catalog.config.features.comments = false;
      const off = pick();
      h.catalog.config.features.comments = true;
      h.store.__setTypes(['o3', 'o5', 'o6', 'o111']);
      return { on, off };
    })()`);
    record(
      'appsample_community_guide_key_follows_comment_rules',
      JSON.stringify(asCommunityRes.on) === JSON.stringify({ p6: ['o5', true], p3: ['o185', false] }) &&
        JSON.stringify(asCommunityRes.off) === JSON.stringify({ p6: ['o5', false], p3: ['o185', false] }),
      JSON.stringify(asCommunityRes)
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

    // (h1) AppSample LAN_FOCUS_PIN: isolation, transformed camera focus, underground overlay & popup
    const asFocusRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      const CT = window.GHCoordTransform;
      const tf = CT.getTransform('appsample', 2);
      const canon = CT.leafletToCanonical(tf, -0.24196, 0.61993);
      const baseViews = window.__ghSetViewCalls.length;

      // Ignored: foreign provider, un-namespaced id, wrong target_provider, wrong owner_tab_id
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: 'kongying:1288', map_id: 2, lat: canon.lat, lng: canon.lng });
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: '18596', map_id: 2, lat: canon.lat, lng: canon.lng });
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: 'appsample:18596', map_id: 2, lat: canon.lat, lng: canon.lng, target_provider: 'kongying' });
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: 'appsample:18596', map_id: 2, lat: canon.lat, lng: canon.lng, owner_tab_id: 'wrong_tab' });
      await new Promise((r) => setTimeout(r, 120));
      const ignoredOk = window.__ghSetViewCalls.length === baseViews && !document.querySelector('[data-testid="info-window"]');

      // Valid LAN_FOCUS_PIN for underground pin appsample:18596 (floorId=5, groupId=82)
      window.__ghSendServerEvent({
        type: 'LAN_FOCUS_PIN',
        pin_id: 'appsample:18596',
        map_id: 2,
        lat: canon.lat,
        lng: canon.lng,
        floor_id: '5',
        target_provider: 'appsample',
        owner_tab_id: window.__GH_BRIDGE__.tabId
      });
      await new Promise((r) => setTimeout(r, 150));

      const lastView = window.__ghSetViewCalls[window.__ghSetViewCalls.length - 1];
      const expLl = CT.canonicalToLeaflet(tf, canon.lat, canon.lng);
      const viewOk = Boolean(lastView && Math.abs(lastView.lat - expLl[0]) < 1e-5 && Math.abs(lastView.lng - expLl[1]) < 1e-5);
      const st = h.store.getState();
      const overlayOk = Boolean(st.overlay && st.overlay.mode === 'group' && st.overlay.groupId === 82 && st.overlay.floorId === 5 && h.adapter.activeOverlayIds.includes(5));
      const infoWin = document.querySelector('[data-testid="info-window"]');
      const popupOk = Boolean(infoWin && infoWin.getAttribute('data-marker-id') === '18596');
      const recenterVisible = Boolean(document.getElementById('gh-recenter-btn')?.classList.contains('is-visible'));

      // Reset state for subsequent tests
      h.renderInfoWindow(null);
      h.store.setState({ overlay: { mode: 'none' }, popupOpen: false, selectedId: null });
      h.mapSync.applyOverlay();
      window.__GH_BRIDGE__.recenter();

      return { ok: ignoredOk && viewOk && overlayOk && popupOk && recenterVisible, ignoredOk, viewOk, overlayOk, popupOk, recenterVisible, overlay: st.overlay };
    })()`);
    record('appsample_lan_focus_pin', asFocusRes.ok, JSON.stringify(asFocusRes));

    // (h2) AppSample LAN_SET_FLOOR: group/floor selection, surface reset, and provider routing guard
    const asFloorRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      window.__ghSendServerEvent({
        type: 'LAN_SET_FLOOR',
        floor_id: '10',
        target_provider: 'appsample',
        owner_tab_id: window.__GH_BRIDGE__.tabId
      });
      await new Promise((r) => setTimeout(r, 50));
      const ugSt = h.store.getState().overlay;
      const ugIds = [...h.adapter.activeOverlayIds];
      const ugOk = Boolean(ugSt && ugSt.mode === 'group' && ugSt.groupId === 84 && ugSt.floorId === 10 && ugIds.length === 1 && ugIds[0] === 10);

      // Surface reset
      window.__ghSendServerEvent({
        type: 'LAN_SET_FLOOR',
        floor_id: '0',
        target_provider: 'appsample',
        owner_tab_id: window.__GH_BRIDGE__.tabId
      });
      await new Promise((r) => setTimeout(r, 50));
      const surfSt = h.store.getState().overlay;
      const surfOk = Boolean(surfSt && surfSt.mode === 'none' && h.adapter.activeOverlayIds.length === 0);

      // Mismatched provider ignored
      window.__ghSendServerEvent({
        type: 'LAN_SET_FLOOR',
        floor_id: '5',
        target_provider: 'kongying'
      });
      await new Promise((r) => setTimeout(r, 50));
      const ignoredOk = h.store.getState().overlay.mode === 'none';

      return { ok: ugOk && surfOk && ignoredOk, ugOk, surfOk, ignoredOk, ugSt, surfSt };
    })()`);
    record('appsample_lan_set_floor', asFloorRes.ok, JSON.stringify(asFloorRes));

    // (h3) AppSample ROUTE_UPDATED: route bar + transformed SVG polyline & stops + map-switch clear/redraw
    const asRouteRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      const CT = window.GHCoordTransform;
      const tf = CT.getTransform('appsample', 2);
      const stop0 = { id: 'appsample:301', kind: 'collectible', lat: -2000, lng: 1500, collected: false };
      const stop1 = { id: 'appsample:16514', kind: 'collectible', lat: 5500, lng: 1500, collected: false };
      const exp0 = CT.canonicalToLeaflet(tf, stop0.lat, stop0.lng);
      const exp1 = CT.canonicalToLeaflet(tf, stop1.lat, stop1.lng);
      const baseLng1 = tf.base.ax * stop1.lng + tf.base.bx;

      window.__ghSendServerEvent({
        type: 'ROUTE_UPDATED',
        active: true,
        title: 'Inazuma Route',
        provider: 'hoyolab',
        map_id: 2,
        active_section: 0,
        section_count: 2,
        section_done: 0,
        section_total: 2,
        section_color: '#7EE8FF',
        active_edges: [[0, 1]],
        active_stops: [stop0, stop1]
      });
      await new Promise((r) => setTimeout(r, 60));

      const bar = document.getElementById('gh-route-bar');
      const svg = document.getElementById('gh-tsp-route-svg');
      const leg = svg ? svg.querySelector('.gh-route-leg') : null;
      const stops = svg ? svg.querySelectorAll('.gh-tsp-stop') : [];
      const fromParts = leg ? leg.getAttribute('data-leaflet-from').split(',').map(Number) : [];
      const toParts = leg ? leg.getAttribute('data-leaflet-to').split(',').map(Number) : [];
      const drawnCoordsOk = Boolean(
        bar && bar.textContent.includes('Inazuma Route') && bar.textContent.includes('S1/2') &&
        svg && leg && stops.length === 2 &&
        Math.abs(fromParts[0] - exp0[0]) < 1e-4 && Math.abs(fromParts[1] - exp0[1]) < 1e-4 &&
        Math.abs(toParts[0] - exp1[0]) < 1e-4 && Math.abs(toParts[1] - exp1[1]) < 1e-4 &&
        Math.abs(toParts[1] - baseLng1) > 0.15
      );

      // Switch to Enkanomiya (map 7) -> route SVG clears automatically
      h.store.__setMapKey('enkanomiya');
      await new Promise((r) => setTimeout(r, 50));
      const clearedOnSwitch = !document.getElementById('gh-tsp-route-svg');

      // Switch back to Teyvat (map 2) -> route SVG redraws
      h.store.__setMapKey('teyvat');
      await new Promise((r) => setTimeout(r, 50));
      const redrawnOnReturn = Boolean(document.getElementById('gh-tsp-route-svg')?.querySelector('.gh-route-leg'));

      // Route deactivation clears bar & SVG
      window.__ghSendServerEvent({ type: 'ROUTE_UPDATED', active: false });
      await new Promise((r) => setTimeout(r, 50));
      const clearedOnInactive = !document.getElementById('gh-route-bar') && !document.getElementById('gh-tsp-route-svg') && !document.body.classList.contains('gh-has-route-bar');

      return { ok: drawnCoordsOk && clearedOnSwitch && redrawnOnReturn && clearedOnInactive, drawnCoordsOk, clearedOnSwitch, redrawnOnReturn, clearedOnInactive };
    })()`);
    record('appsample_route_updated_and_map_switch', asRouteRes.ok, JSON.stringify(asRouteRes));

    // (h4) AppSample GH_CMD_ROUTE_IMPORT / GH_CMD_ROUTE_EXPORT / GH_CMD_ROUTE_CLEAR
    const asRouteCmdRes = await page.eval(`(async () => {
      window.__ghMockRouteState = {
        ok: true,
        active: true,
        title: 'Popup Imported',
        map_id: 2,
        active_section: 0,
        section_count: 1,
        section_done: 0,
        section_total: 2,
        section_color: '#52C4A0',
        active_edges: [[0, 1]],
        active_stops: [
          { id: 'appsample:301', kind: 'collectible', lat: -2000, lng: 1500, collected: false },
          { id: 'appsample:302', kind: 'collectible', lat: -1950, lng: 1550, collected: false }
        ]
      };
      const expectedDoc = { schema: 1, title: 'Popup Imported', map_id: 2, sections: [{ name: 'Sec 1', stops: ['301', '302'] }] };
      window.__ghMockRouteExportDoc = { filename: 'popup_imported.ghroute.json', doc: expectedDoc };
      const dlStart = window.__ghDownloads.length;
      const fitStart = window.__ghFitBoundsCalls.length;

      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_IMPORT', fit: true });
      await new Promise((r) => setTimeout(r, 100));
      const importOk = Boolean(
        document.getElementById('gh-route-bar')?.textContent.includes('Popup Imported') &&
        document.getElementById('gh-tsp-route-svg')?.querySelector('.gh-route-leg') &&
        window.__ghFitBoundsCalls.length > fitStart
      );

      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_EXPORT' });
      await new Promise((r) => setTimeout(r, 100));
      const dl = window.__ghDownloads[window.__ghDownloads.length - 1] || null;
      const exportOk = Boolean(
        window.__ghDownloads.length > dlStart &&
        dl && dl.filename === 'popup_imported.ghroute.json' &&
        JSON.parse(dl.content || '{}').title === 'Popup Imported'
      );

      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_CLEAR' });
      await new Promise((r) => setTimeout(r, 60));
      const clearOk = !document.getElementById('gh-route-bar') && !document.getElementById('gh-tsp-route-svg');
      window.__ghMockRouteState = { ok: true, active: false };

      return { ok: importOk && exportOk && clearOk, importOk, exportOk, clearOk };
    })()`);
    record('appsample_route_commands_import_export_clear', asRouteCmdRes.ok, JSON.stringify(asRouteCmdRes));

    // (h5) AppSample DIAGNOSTIC_CAPTURED modal: renders, tag appends, Save & Zip, Save Note
    const asDiagRes = await page.eval(`(async () => {
      const saveReqsBefore = window.__ghRequests.filter((r) => r.path === '/save_diagnostic_note').length;
      const openReqsBefore = window.__ghRequests.filter((r) => r.path === '/open_diagnostics').length;

      window.__ghSendServerEvent({
        type: 'DIAGNOSTIC_CAPTURED',
        folder_name: 'diag_as_001',
        zip_name: 'diag_as_001.zip',
        has_world_coords: true,
        summary: { app_version: '5.7.0', os_label: 'Windows 11' }
      });
      await new Promise((r) => setTimeout(r, 80));

      const modal = document.getElementById('gh-diag-note-modal');
      const modalOk = Boolean(modal && modal.getAttribute('data-folder') === 'diag_as_001' && document.getElementById('gh-note-title')?.textContent.includes('Diagnostic Capture Saved'));
      const firstTag = document.querySelector('#gh-note-tags button');
      if (firstTag) firstTag.click();
      const input = document.getElementById('gh-diag-note-input');
      if (input) input.value += 'extra test note';
      const tagOk = Boolean(input && input.value.includes('[Wrong Position]') && input.value.includes('extra test note'));

      document.getElementById('gh-note-save')?.click();
      await new Promise((r) => setTimeout(r, 80));
      const zipSaveReq = window.__ghRequests.slice(saveReqsBefore).find((r) => r.path === '/save_diagnostic_note');
      const zipOpenReq = window.__ghRequests.slice(openReqsBefore).find((r) => r.path === '/open_diagnostics');
      const zipFlowOk = Boolean(
        zipSaveReq?.body?.create_zip === true &&
        zipSaveReq?.body?.folder_name === 'diag_as_001' &&
        zipOpenReq?.body?.ensure_zip === true
      );

      document.getElementById('gh-note-save-only')?.click();
      await new Promise((r) => setTimeout(r, 80));
      const saveReqsAfter = window.__ghRequests.filter((r) => r.path === '/save_diagnostic_note');
      const noteOnlyReq = saveReqsAfter[saveReqsAfter.length - 1];
      const saveOnlyOk = Boolean(
        noteOnlyReq?.body?.create_zip === false &&
        String(noteOnlyReq?.body?.note || '').includes('[Wrong Position]') &&
        !document.getElementById('gh-diag-note-modal')
      );

      return { ok: modalOk && tagOk && zipFlowOk && saveOnlyOk, modalOk, tagOk, zipFlowOk, saveOnlyOk };
    })()`);
    record('appsample_diagnostic_captured_modal', asDiagRes.ok, JSON.stringify(asDiagRes));

    // (h6) AppSample LANGUAGE_UPDATED: live VI <-> EN re-render across all overlays
    const asLangRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      const CT = window.GHCoordTransform;
      const tf = CT.getTransform('appsample', 2);
      const canon = CT.leafletToCanonical(tf, 0.24676, 0.43245);

      // Show recenter btn (via unpinning LAN_FOCUS_PIN on surface pin 301), target lock pill, popup navigate btn, route bar, and diagnostic modal + toast
      window.__ghSendServerEvent({
        type: 'TARGET_LOCK_UPDATED',
        locked_target: { id: 'appsample:302', name: 'Anemoculus 302', map_id: 2, lat: canon.lat, lng: canon.lng }
      });
      window.__ghSendServerEvent({
        type: 'LAN_FOCUS_PIN',
        pin_id: 'appsample:301',
        map_id: 2,
        lat: canon.lat,
        lng: canon.lng,
        target_provider: 'appsample',
        owner_tab_id: window.__GH_BRIDGE__.tabId
      });
      window.__ghSendServerEvent({
        type: 'ROUTE_UPDATED',
        active: true,
        title: 'Lang Route',
        map_id: 2,
        active_section: 0,
        section_count: 1,
        section_done: 0,
        section_total: 1,
        active_edges: [],
        active_stops: [{ id: 'appsample:301', kind: 'collectible', lat: canon.lat, lng: canon.lng, collected: false }]
      });
      window.__ghSendServerEvent({
        type: 'DIAGNOSTIC_CAPTURED',
        folder_name: 'diag_lang_as',
        zip_name: 'diag_lang_as.zip',
        has_world_coords: true
      });
      await new Promise((r) => setTimeout(r, 150));

      // Switch EN -> VI
      window.__ghSendServerEvent({ type: 'LANGUAGE_UPDATED', language: 'vi' });
      await new Promise((r) => setTimeout(r, 60));
      const viSnap = {
        lang: window.__GH_BRIDGE__.getLanguage(),
        recenter: document.getElementById('gh-recenter-btn')?.textContent || '',
        lockCancel: document.querySelector('#gh-target-lock-pill button')?.textContent || '',
        navBtn: document.getElementById('gh-popup-navigate-btn')?.textContent || '',
        routeBar: document.getElementById('gh-route-bar')?.textContent || '',
        modalTitle: document.getElementById('gh-note-title')?.textContent || '',
        modalSaveOnly: document.getElementById('gh-note-save-only')?.textContent || '',
        modalTag: document.querySelector('#gh-note-tags button')?.textContent || '',
        toast: document.getElementById('gh-mini-toast')?.textContent || ''
      };
      const viOk = (
        viSnap.lang === 'vi' &&
        viSnap.recenter.includes('Bám lại vị trí') &&
        viSnap.lockCancel === 'Hủy' &&
        viSnap.navBtn.includes('Dẫn đường') &&
        viSnap.routeBar.includes('Xem tuyến') &&
        viSnap.modalTitle.includes('Chụp Chẩn Đoán Thành Công') &&
        viSnap.modalSaveOnly === 'Lưu ghi chú' &&
        viSnap.modalTag === '[Sai vị trí]' &&
        viSnap.toast.includes('Đã chụp chẩn đoán')
      );

      // Switch VI -> EN
      window.__ghSendServerEvent({ type: 'LANGUAGE_UPDATED', language: 'en' });
      await new Promise((r) => setTimeout(r, 60));
      const enSnap = {
        lang: window.__GH_BRIDGE__.getLanguage(),
        recenter: document.getElementById('gh-recenter-btn')?.textContent || '',
        lockCancel: document.querySelector('#gh-target-lock-pill button')?.textContent || '',
        navBtn: document.getElementById('gh-popup-navigate-btn')?.textContent || '',
        routeBar: document.getElementById('gh-route-bar')?.textContent || '',
        modalTitle: document.getElementById('gh-note-title')?.textContent || '',
        modalSaveOnly: document.getElementById('gh-note-save-only')?.textContent || '',
        modalTag: document.querySelector('#gh-note-tags button')?.textContent || '',
        toast: document.getElementById('gh-mini-toast')?.textContent || ''
      };
      const enOk = (
        enSnap.lang === 'en' &&
        enSnap.recenter.includes('Re-center') &&
        enSnap.lockCancel === 'Cancel' &&
        enSnap.navBtn.includes('Navigate') &&
        enSnap.routeBar.includes('View Route') &&
        enSnap.modalTitle.includes('Diagnostic Capture Saved') &&
        enSnap.modalSaveOnly === 'Save Note' &&
        enSnap.modalTag === '[Wrong Position]' &&
        enSnap.toast.includes('Diagnostic captured')
      );

      // Cleanup
      document.getElementById('gh-note-skip')?.click();
      window.__ghSendServerEvent({ type: 'ROUTE_UPDATED', active: false });
      window.__ghSendServerEvent({ type: 'TARGET_LOCK_UPDATED', locked_target: null });
      h.renderInfoWindow(null);
      window.__GH_BRIDGE__.recenter();

      return { ok: viOk && enOk, viOk, enOk, viSnap, enSnap };
    })()`);
    record('appsample_language_updated', asLangRes.ok, JSON.stringify({ viOk: asLangRes.viOk, enOk: asLangRes.enOk }));

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

    // (k1) Kongying LAN_FOCUS_PIN & LAN_SET_FLOOR
    const kyFocusFloorRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      const CT = window.GHCoordTransform;
      const tf = CT.getTransform('kongying', 2);
      const canon = CT.leafletToCanonical(tf, 1319.8804911037, -3606.7583012494);
      const baseViews = window.__ghSetViewCalls.length;

      // Foreign/mismatched ignored
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: 'appsample:301', map_id: 2, lat: canon.lat, lng: canon.lng });
      window.__ghSendServerEvent({ type: 'LAN_FOCUS_PIN', pin_id: 'kongying:1288', map_id: 2, lat: canon.lat, lng: canon.lng, target_provider: 'appsample' });
      await new Promise((r) => setTimeout(r, 80));
      const ignoredOk = window.__ghSetViewCalls.length === baseViews && !h.vm.popup_window_show;

      // Valid LAN_FOCUS_PIN for kongying:1288 -> swapped Leaflet setView + popup opened
      window.__ghSendServerEvent({
        type: 'LAN_FOCUS_PIN',
        pin_id: 'kongying:1288',
        map_id: 2,
        lat: canon.lat,
        lng: canon.lng,
        target_provider: 'kongying',
        owner_tab_id: window.__GH_BRIDGE__.tabId
      });
      await new Promise((r) => setTimeout(r, 120));
      const lastView = window.__ghSetViewCalls[window.__ghSetViewCalls.length - 1];
      const expLl = CT.canonicalToLeaflet(tf, canon.lat, canon.lng);
      const viewOk = Boolean(lastView && Math.abs(lastView.lat - expLl[0]) < 1e-3 && Math.abs(lastView.lng - expLl[1]) < 1e-3);
      const pop = document.getElementById('popup_window');
      const popupOk = Boolean(h.vm.popup_window_show && pop && pop.getAttribute('data-marker-id') === '1288');

      // LAN_SET_FLOOR: documented no-op when underground_show=false; binary toggle when underground_show=true
      h.setUndergroundShow(false);
      window.__ghSendServerEvent({ type: 'LAN_SET_FLOOR', floor_id: '10', target_provider: 'kongying', owner_tab_id: window.__GH_BRIDGE__.tabId });
      await new Promise((r) => setTimeout(r, 40));
      const noopWhenHidden = h.vm.underground_opacity === false;

      h.setUndergroundShow(true);
      window.__ghSendServerEvent({ type: 'LAN_SET_FLOOR', floor_id: '10', target_provider: 'kongying', owner_tab_id: window.__GH_BRIDGE__.tabId });
      await new Promise((r) => setTimeout(r, 40));
      const ugOn = h.vm.underground_opacity === true;

      window.__ghSendServerEvent({ type: 'LAN_SET_FLOOR', floor_id: '0', target_provider: 'kongying', owner_tab_id: window.__GH_BRIDGE__.tabId });
      await new Promise((r) => setTimeout(r, 40));
      const surfBack = h.vm.underground_opacity === false;

      return {
        ok: ignoredOk && viewOk && popupOk && noopWhenHidden && ugOn && surfBack,
        ignoredOk, viewOk, popupOk, noopWhenHidden, ugOn, surfBack
      };
    })()`);
    record('kongying_lan_focus_pin_and_set_floor', kyFocusFloorRes.ok, JSON.stringify(kyFocusFloorRes));

    // (k2) Kongying ROUTE_UPDATED (swapped axes + area switch), GH_CMD_ROUTE_*, LANGUAGE_UPDATED, DIAGNOSTIC_CAPTURED
    const kyParityRes = await page.eval(`(async () => {
      const h = window.__ghFixtureHandles;
      const CT = window.GHCoordTransform;
      const tf = CT.getTransform('kongying', 2);
      const exp0 = CT.canonicalToLeaflet(tf, -500, 1000);
      const exp1 = CT.canonicalToLeaflet(tf, -450, 1050);

      // ROUTE_UPDATED on Kongying map 2
      window.__ghSendServerEvent({
        type: 'ROUTE_UPDATED',
        active: true,
        title: 'KY Route',
        map_id: 2,
        active_section: 0,
        section_count: 2,
        section_done: 0,
        section_total: 2,
        active_edges: [[0, 1]],
        active_stops: [
          { id: 'kongying:1288', kind: 'collectible', lat: -500, lng: 1000, collected: false },
          { id: 'kongying:1193', kind: 'collectible', lat: -450, lng: 1050, collected: false }
        ]
      });
      await new Promise((r) => setTimeout(r, 60));
      const leg = document.querySelector('#gh-tsp-route-svg .gh-route-leg');
      const fromParts = leg ? leg.getAttribute('data-leaflet-from').split(',').map(Number) : [];
      const toParts = leg ? leg.getAttribute('data-leaflet-to').split(',').map(Number) : [];
      const routeSwapOk = Boolean(
        document.getElementById('gh-route-bar')?.textContent.includes('KY Route') &&
        leg &&
        Math.abs(fromParts[0] - exp0[0]) < 1e-3 && Math.abs(fromParts[1] - exp0[1]) < 1e-3 &&
        Math.abs(toParts[0] - exp1[0]) < 1e-3 && Math.abs(toParts[1] - exp1[1]) < 1e-3
      );

      // Area switch to Chasm UG (map 9) clears SVG; switching back to Mondstadt (map 2) redraws
      h.setAreaCode('A:LY:CENGYAN_UG');
      window.__GH_BRIDGE__.syncRouteOverlay();
      await new Promise((r) => setTimeout(r, 40));
      const clearedOnAreaSwitch = !document.getElementById('gh-tsp-route-svg');
      h.setAreaCode('A:MD:MENGDE');
      window.__GH_BRIDGE__.syncRouteOverlay();
      await new Promise((r) => setTimeout(r, 40));
      const redrawnOnAreaReturn = Boolean(document.querySelector('#gh-tsp-route-svg .gh-route-leg'));

      // LANGUAGE_UPDATED VI <-> EN while route bar, lock pill & recenter btn are visible
      window.__ghSendServerEvent({ type: 'TARGET_LOCK_UPDATED', locked_target: { id: 'kongying:1288', name: 'Chest', map_id: 2, lat: -500, lng: 1000 } });
      window.__ghSendServerEvent({ type: 'LANGUAGE_UPDATED', language: 'vi' });
      await new Promise((r) => setTimeout(r, 50));
      const langViOk = Boolean(
        document.getElementById('gh-route-bar')?.textContent.includes('Xem tuyến') &&
        document.getElementById('gh-recenter-btn')?.textContent.includes('Bám lại vị trí') &&
        document.querySelector('#gh-target-lock-pill button')?.textContent === 'Hủy'
      );
      window.__ghSendServerEvent({ type: 'LANGUAGE_UPDATED', language: 'en' });
      await new Promise((r) => setTimeout(r, 50));
      const langEnOk = Boolean(
        document.getElementById('gh-route-bar')?.textContent.includes('View Route') &&
        document.getElementById('gh-recenter-btn')?.textContent.includes('Re-center') &&
        document.querySelector('#gh-target-lock-pill button')?.textContent === 'Cancel'
      );

      // GH_CMD_ROUTE_EXPORT / GH_CMD_ROUTE_CLEAR / GH_CMD_ROUTE_IMPORT
      window.__ghMockRouteExportDoc = { filename: 'ky_route.ghroute.json', doc: { schema: 1, title: 'KY Route' } };
      const dlStart = window.__ghDownloads.length;
      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_EXPORT' });
      await new Promise((r) => setTimeout(r, 80));
      const exportOk = window.__ghDownloads.length > dlStart && window.__ghDownloads[window.__ghDownloads.length - 1].filename === 'ky_route.ghroute.json';

      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_CLEAR' });
      await new Promise((r) => setTimeout(r, 50));
      const clearOk = !document.getElementById('gh-route-bar') && !document.getElementById('gh-tsp-route-svg');

      window.__ghMockRouteState = {
        ok: true,
        active: true,
        title: 'KY Reimported',
        map_id: 2,
        active_section: 0,
        section_count: 1,
        section_done: 0,
        section_total: 1,
        active_edges: [],
        active_stops: [{ id: 'kongying:1288', kind: 'collectible', lat: -500, lng: 1000, collected: false }]
      };
      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_IMPORT', fit: true });
      await new Promise((r) => setTimeout(r, 80));
      const importOk = Boolean(document.getElementById('gh-route-bar')?.textContent.includes('KY Reimported'));
      window.__ghSendServerEvent({ type: 'GH_CMD_ROUTE_CLEAR' });
      window.__ghMockRouteState = { ok: true, active: false };

      // DIAGNOSTIC_CAPTURED modal + Escape close
      window.__ghSendServerEvent({ type: 'DIAGNOSTIC_CAPTURED', folder_name: 'diag_ky_001', has_world_coords: false, reason: 'Low landmarks' });
      await new Promise((r) => setTimeout(r, 80));
      const diagShown = Boolean(document.getElementById('gh-diag-note-modal')?.getAttribute('data-folder') === 'diag_ky_001');
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
      await new Promise((r) => setTimeout(r, 40));
      const diagEscClosed = !document.getElementById('gh-diag-note-modal');

      return {
        ok: routeSwapOk && clearedOnAreaSwitch && redrawnOnAreaReturn && langViOk && langEnOk && exportOk && clearOk && importOk && diagShown && diagEscClosed,
        routeSwapOk, clearedOnAreaSwitch, redrawnOnAreaReturn, langViOk, langEnOk, exportOk, clearOk, importOk, diagShown, diagEscClosed
      };
    })()`);
    record('kongying_language_route_commands_and_diagnostic', kyParityRes.ok, JSON.stringify(kyParityRes));

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
