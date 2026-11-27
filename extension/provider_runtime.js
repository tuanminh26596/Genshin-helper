// Multi-Provider Map Runtime & Open Provider SDK for Genshin-Helper (MAIN world).
// Runs on non-HoYoLAB supported maps (Kongying Tavern v3, AppSample, and any
// adapter registered via window.__GH_REGISTER_MAP_PROVIDER__).
// HoYoLAB Interactive Map continues to be handled by page_main.js untouched.
(function () {
  'use strict';

  const hostLower = String(window.location.hostname || '').toLowerCase();
  if (hostLower.includes('hoyolab.com') && window.location.pathname.includes('/interactive-map/')) {
    return;
  }

  // Idempotent hot-reload guard
  if (typeof window.__gh_provider_teardown === 'function') {
    try { window.__gh_provider_teardown(); } catch (e) {}
  }

  const CT = window.GHCoordTransform;
  const GH_TAB_ID = `gh_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
  const SCRIPT_VERSION = '5.7.0';
  const SCRIPT_BUILD = 'multi-provider-v1';

  // =========================================================================
  // 1. Open Provider Registry & Built-in Tier 1 Adapters
  // =========================================================================

  const registeredAdapters = new Map();

  function registerProviderAdapter(adapter) {
    if (!adapter || typeof adapter.id !== 'string' || typeof adapter.matchesLocation !== 'function') {
      return false;
    }
    registeredAdapters.set(adapter.id.toLowerCase(), adapter);
    return true;
  }

  window.__GH_REGISTER_MAP_PROVIDER__ = registerProviderAdapter;
  window.__GH_MAP_PROVIDERS__ = registeredAdapters;

  // --- Built-in Adapter A: Kongying Tavern v3 (https://v3.yuanshen.site/) ---
  const KONGYING_AREA_CODE_TO_MAP_ID = {
    'A:DQ:YUANXIAGONG': 7,
    'A:DQ:SANJIE': 7,
    'A:LY:CENGYAN_UG': 9,
    'A:FD:ANCIENT_SEA': 34,
    'A:NT:NATA4': 36,
    'A:MD:SHENDIAN': 37,
    'A:NDKL:SY': 40
  };

  const KONGYING_MAP_ID_TO_NAME_HINT = {
    2: ['蒙德', '璃月', '稻妻', '须弥', '枫丹', '纳塔', '挪德卡莱', '至冬'],
    7: ['渊下宫'],
    9: ['地下矿区'],
    34: ['旧日之海'],
    36: ['远古圣山'],
    37: ['空之神殿'],
    40: ['霜月']
  };

  const KONGYING_LANDMARK_RE = /^(传送锚点|七天神像|秘境|征讨领域|浪船锚点|奖励秘境)$/;

  function kongyingReadMarkedSet() {
    try {
      const raw = localStorage.getItem('marked_layers');
      if (!raw) return new Set();
      const arr = JSON.parse(raw);
      return new Set(Array.isArray(arr) ? arr.map((x) => String(x)) : []);
    } catch (e) {
      return new Set();
    }
  }

  function kongyingWriteMarkedSet(idSet) {
    try {
      const nums = Array.from(idSet).map((s) => {
        const n = Number(s);
        return Number.isFinite(n) ? n : s;
      });
      localStorage.setItem('marked_layers', JSON.stringify(nums));
      return true;
    } catch (e) {
      return false;
    }
  }

  function kongyingEachMarkerLayer(map, fn) {
    if (!map || !map._layers) return;
    const seen = new Set();
    const visit = (layer) => {
      if (!layer || seen.has(layer)) return;
      seen.add(layer);
      if (layer.options && layer.options.data && layer.options.data.id !== undefined) {
        fn(layer);
        return;
      }
      if (typeof layer.getLayers === 'function') {
        try {
          for (const child of layer.getLayers()) visit(child);
        } catch (e) {}
      } else if (typeof layer.eachLayer === 'function') {
        try {
          layer.eachLayer((child) => visit(child));
        } catch (e) {}
      }
    };
    for (const l of Object.values(map._layers)) visit(l);
  }

  const KongyingV3Adapter = {
    id: 'kongying',
    name: 'Kongying Tavern',
    capabilities: {
      markInGuest: true,
      layeredFloors: 'toggle',
      compactUi: true,
      pinGuideInline: true
    },
    matchesLocation(loc) {
      const h = String((loc && loc.hostname) || '').toLowerCase();
      return h === 'v3.yuanshen.site' || h === 'yuanshen.site';
    },
    findContext() {
      const qapp = document.querySelector('#q-app');
      const router = qapp && qapp.__vue_app__ && qapp.__vue_app__.config &&
        qapp.__vue_app__.config.globalProperties && qapp.__vue_app__.config.globalProperties.$router;
      const matched = router && router.currentRoute && router.currentRoute.value && router.currentRoute.value.matched;
      let vm = null;
      if (Array.isArray(matched)) {
        for (const r of matched) {
          const def = r && r.instances && r.instances.default;
          if (def && (def.map_tiles || def.BXGroup || def.mainStore)) {
            vm = def;
            break;
          }
        }
      }
      if (!vm) return null;
      const map = (vm.map_tiles && vm.map_tiles._map) ||
        (vm.BXGroup && vm.BXGroup._map) ||
        (vm.teleport_group && vm.teleport_group._map) || null;
      if (!map || typeof map.setView !== 'function') return null;
      return { vm, map, mainStore: vm.mainStore || null, providerId: 'kongying' };
    },
    isAlive(ctx) {
      return Boolean(ctx && ctx.map && ctx.map._container && ctx.map._container.isConnected);
    },
    getCurrentMapId(ctx) {
      const child = ctx && ctx.mainStore && ctx.mainStore.selected_child_area;
      const code = child && typeof child.code === 'string' ? child.code.trim() : '';
      if (code && KONGYING_AREA_CODE_TO_MAP_ID[code]) {
        return KONGYING_AREA_CODE_TO_MAP_ID[code];
      }
      return 2;
    },
    switchMap(ctx, targetMapId) {
      const cur = this.getCurrentMapId(ctx);
      if (cur === Number(targetMapId)) return true;
      const hints = KONGYING_MAP_ID_TO_NAME_HINT[Number(targetMapId)];
      if (!hints || !ctx || !ctx.mainStore || !Array.isArray(ctx.mainStore.area_list)) return false;
      const targetArea = ctx.mainStore.area_list.find((a) =>
        a && typeof a.name === 'string' && hints.some((h) => a.name.includes(h))
      );
      if (!targetArea) return false;
      try {
        ctx.mainStore.selected_child_area = targetArea;
        return true;
      } catch (e) {
        return false;
      }
    },
    ratioToZoom(ctx, zoomRatio) {
      const minZ = typeof ctx.map.getMinZoom === 'function' ? ctx.map.getMinZoom() : -4;
      const maxZ = typeof ctx.map.getMaxZoom === 'function' ? ctx.map.getMaxZoom() : 2;
      // Avoid -4 where maxBounds clamps map center on wide screens
      const floorZ = Math.max(minZ, -2.5);
      const t = Number.isFinite(zoomRatio) ? Math.max(0, Math.min(1, zoomRatio)) : 0.45;
      return Math.max(floorZ, Math.min(maxZ, floorZ + (maxZ - floorZ) * t));
    },
    collectCalibrationAnchors(ctx) {
      const anchors = [];
      kongyingEachMarkerLayer(ctx.map, (layer) => {
        const d = layer.options && layer.options.data;
        if (!d || typeof d.position !== 'string') return;
        const title = String(d.markerTitle || '').trim();
        const isTp = Boolean(layer.options && layer.options.icon && layer.options.icon.options && layer.options.icon.options.teleport) ||
          KONGYING_LANDMARK_RE.test(title);
        if (!isTp) return;
        const parts = d.position.split(',');
        const x = Number(parts[0]);
        const y = Number(parts[1]);
        if (!Number.isFinite(x) || !Number.isFinite(y)) return;
        anchors.push({
          x,
          y,
          is_statue: title.includes('七天神像'),
          category: title
        });
      });
      return anchors;
    },
    getFilterSignature(ctx) {
      const sel = (ctx.mainStore && Array.isArray(ctx.mainStore.selected_item_list))
        ? ctx.mainStore.selected_item_list.map((i) => i && i.id).filter((id) => id !== undefined).sort((a, b) => a - b).join(',')
        : '';
      const markedCount = kongyingReadMarkedSet().size;
      const ug = ctx.vm && ctx.vm.underground_opacity ? 1 : 0;
      return `ky:${this.getCurrentMapId(ctx)}:${sel}:${markedCount}:${ug}`;
    },
    collectVisiblePins(ctx, tf) {
      const mapId = this.getCurrentMapId(ctx);
      const markedSet = kongyingReadMarkedSet();
      const pins = [];
      const seenIds = new Set();
      kongyingEachMarkerLayer(ctx.map, (layer) => {
        const d = layer.options && layer.options.data;
        if (!d || d.id === undefined) return;
        const rawId = String(d.id);
        if (seenIds.has(rawId)) return;
        const title = String(d.markerTitle || '').trim();
        const isLandmark = Boolean(layer.options && layer.options.icon && layer.options.icon.options && layer.options.icon.options.teleport) ||
          KONGYING_LANDMARK_RE.test(title);
        if (isLandmark) return;
        let lat = NaN;
        let lng = NaN;
        if (typeof layer.getLatLng === 'function') {
          const ll = layer.getLatLng();
          if (ll) { lat = Number(ll.lat); lng = Number(ll.lng); }
        }
        if ((!Number.isFinite(lat) || !Number.isFinite(lng)) && typeof d.position === 'string') {
          const parts = d.position.split(',');
          lat = Number(parts[0]);
          lng = Number(parts[1]);
        }
        if (!Number.isFinite(lat) || !Number.isFinite(lng)) return;
        const canon = CT.leafletToCanonical(tf, lat, lng);
        if (!canon) return;
        seenIds.add(rawId);
        let extra = d.extra;
        if (typeof extra === 'string') {
          try { extra = JSON.parse(extra); } catch (e) { extra = {}; }
        }
        const isUg = Boolean((extra && extra.underground && extra.underground.is_underground) || Number(d.hiddenFlag) === 2);
        const rawImg = typeof d.picture === 'string' ? d.picture.trim() : '';
        pins.push({
          id: `kongying:${rawId}`,
          rawId,
          map_id: mapId,
          lat: canon.lat,
          lng: canon.lng,
          name: title || `Pin ${rawId}`,
          category: title,
          is_underground: isUg,
          floor_id: null,
          floor_name: isUg ? 'B1' : '',
          description: typeof d.content === 'string' ? d.content.trim() : '',
          image_url: rawImg.startsWith('https://') ? rawImg : '',
          marked: markedSet.has(rawId)
        });
      });
      return pins;
    },
    async markPin(ctx, rawId, action) {
      const targetId = String(rawId || '').trim();
      if (!targetId) return { ok: false, error: 'Missing pin id' };
      const wantMarked = action !== 'unmark';
      const markedSet = kongyingReadMarkedSet();
      const currentlyMarked = markedSet.has(targetId);
      if (currentlyMarked === wantMarked) {
        return { ok: true, already: true };
      }
      if (wantMarked) {
        markedSet.add(targetId);
      } else {
        markedSet.delete(targetId);
      }
      if (!kongyingWriteMarkedSet(markedSet)) {
        return { ok: false, error: 'Failed to persist marked_layers' };
      }
      try {
        if (ctx.vm && typeof ctx.vm.refresh_layergroup === 'function') {
          ctx.vm.refresh_layergroup();
        }
      } catch (e) {}
      return { ok: true };
    },
    syncUnderground(ctx, isUnderground) {
      try {
        if (!ctx || !ctx.vm || !ctx.vm.underground_show) return;
        if (Boolean(ctx.vm.underground_opacity) !== Boolean(isUnderground) && typeof ctx.vm.underground_opacity_switch === 'function') {
          ctx.vm.underground_opacity_switch();
        }
      } catch (e) {}
    },
    getOpenPopupPin(ctx, tf) {
      const popupEl = document.querySelector('#popup_window');
      if (!popupEl || popupEl.style.display === 'none' || !popupEl.offsetParent) return null;
      const layer = ctx.vm && ctx.vm.handle_layer && ctx.vm.handle_layer.target;
      const d = layer && layer.options && layer.options.data;
      if (!d || d.id === undefined) return null;
      const rawId = String(d.id);
      const ll = typeof layer.getLatLng === 'function' ? layer.getLatLng() : null;
      if (!ll) return null;
      const canon = CT.leafletToCanonical(tf, ll.lat, ll.lng);
      if (!canon) return null;
      return {
        container: popupEl,
        anchorEl: popupEl.querySelector('.toggle_btn') || popupEl.querySelector('.content') || popupEl,
        pin: {
          id: `kongying:${rawId}`,
          map_id: this.getCurrentMapId(ctx),
          lat: canon.lat,
          lng: canon.lng,
          name: String(d.markerTitle || `Pin ${rawId}`),
          category: String(d.markerTitle || ''),
          description: typeof d.content === 'string' ? d.content.trim() : '',
          image_url: typeof d.picture === 'string' && d.picture.startsWith('https://') ? d.picture.trim() : '',
          is_underground: Number(d.hiddenFlag) === 2
        }
      };
    },
    compactCss: [
      'body.gh-compact-ui .extra_btn { opacity: 0.25; transition: opacity 0.2s; }',
      'body.gh-compact-ui .extra_btn:hover { opacity: 1; }',
      'body.gh-compact-ui footer.footer { display: none !important; }'
    ].join('\n')
  };

  // --- Built-in Adapter B: AppSample (https://genshin-impact-map.appsample.com/) ---
  const APPSAMPLE_KEY_TO_MAP_ID = {
    'teyvat': 2,
    'enkanomiya': 7,
    'chasm': 9,
    'sea-of-bygone-eras': 34,
    'ancient_sacred_mountain': 36,
    'temple_of_space': 37,
    'frost_moon': 40
  };
  const APPSAMPLE_MAP_ID_TO_PATH = {
    2: '/',
    7: '/enkanomiya/',
    9: '/chasm/',
    34: '/sea-of-bygone-eras/',
    36: '/ancient_sacred_mountain/',
    37: '/temple_of_space/',
    40: '/frost_moon/'
  };
  const APPSAMPLE_LANDMARK_TYPES = new Set(['o2', 'o3', 'o154', 'entrance']);
  const APPSAMPLE_LANG_PREFIX_RE = /^\/(zh|ko|ja|ru|pt|es|id|fr|de)(?:\/|$)/i;

  function appsampleParseLabelId(typeCode) {
    if (typeof typeCode !== 'string') return null;
    const m = typeCode.trim().match(/^o(\d+)$/i);
    return m ? Number(m[1]) : null;
  }

  function appsampleCategoryLabel(ctx, typeCode) {
    try {
      const msg = ctx.data && ctx.data.messages;
      if (msg && msg.markers && msg.markers[typeCode]) {
        return String(msg.markers[typeCode]);
      }
      const cfg = ctx.catalog && ctx.catalog.config;
      const types = cfg && (cfg.types || cfg.categories);
      if (types && types[typeCode] && types[typeCode].name) return String(types[typeCode].name);
    } catch (e) {}
    return String(typeCode || '');
  }

  const AppSampleAdapter = {
    id: 'appsample',
    name: 'AppSample Map',
    capabilities: {
      markInGuest: false,
      layeredFloors: 'toggle',
      compactUi: true,
      pinGuideInline: true
    },
    matchesLocation(loc) {
      return String((loc && loc.hostname) || '').toLowerCase() === 'genshin-impact-map.appsample.com';
    },
    findContext() {
      const lc = document.querySelector('.leaflet-container');
      if (!lc) return null;
      const fk = Object.keys(lc).find((k) => k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$'));
      if (!fk) return null;
      let node = lc[fk];
      let depth = 0;
      let appCtx = null;
      while (node && depth < 20) {
        const val = node.memoizedProps && node.memoizedProps.value;
        if (val && typeof val === 'object' && val.mapSync && val.store && val.catalog) {
          appCtx = val;
          break;
        }
        node = node.return;
        depth += 1;
      }
      if (!appCtx || !appCtx.mapSync || !appCtx.mapSync.adapter || !appCtx.mapSync.adapter.map) return null;
      return {
        appCtx,
        adapter: appCtx.mapSync.adapter,
        map: appCtx.mapSync.adapter.map,
        store: appCtx.store,
        catalog: appCtx.catalog,
        progress: appCtx.progress,
        data: appCtx.data,
        providerId: 'appsample'
      };
    },
    isAlive(ctx) {
      return Boolean(ctx && ctx.map && ctx.map._container && ctx.map._container.isConnected);
    },
    getCurrentMapId(ctx) {
      try {
        const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : null;
        const key = (st && st.mapKey) || (ctx.data && ctx.data.mapKey) || '';
        if (key && APPSAMPLE_KEY_TO_MAP_ID[key] !== undefined) {
          return APPSAMPLE_KEY_TO_MAP_ID[key];
        }
      } catch (e) {}
      const path = String(window.location.pathname || '').toLowerCase();
      for (const [slug, mid] of Object.entries(APPSAMPLE_KEY_TO_MAP_ID)) {
        if (slug !== 'teyvat' && path.includes(`/${slug}`)) return mid;
      }
      return 2;
    },
    switchMap(ctx, targetMapId) {
      const cur = this.getCurrentMapId(ctx);
      if (cur === Number(targetMapId)) return true;
      const slugPath = APPSAMPLE_MAP_ID_TO_PATH[Number(targetMapId)];
      if (!slugPath) return false;
      const langMatch = String(window.location.pathname || '').match(APPSAMPLE_LANG_PREFIX_RE);
      const langPrefix = langMatch ? `/${langMatch[1].toLowerCase()}` : '';
      const targetUrl = `${window.location.origin}${langPrefix}${slugPath}`;
      try {
        window.location.assign(targetUrl);
        return true;
      } catch (e) {
        return false;
      }
    },
    ratioToZoom(ctx, zoomRatio) {
      const minZ = typeof ctx.map.getMinZoom === 'function' ? ctx.map.getMinZoom() : 10;
      const maxZ = typeof ctx.map.getMaxZoom === 'function' ? ctx.map.getMaxZoom() : 16;
      const t = Number.isFinite(zoomRatio) ? Math.max(0, Math.min(1, zoomRatio)) : 0.45;
      return Math.max(minZ, Math.min(maxZ, minZ + (maxZ - minZ) * (0.2 + 0.7 * t)));
    },
    collectCalibrationAnchors(ctx, mapId) {
      const all = (ctx.catalog && (ctx.catalog.allMarkers || ctx.catalog.officialMarkers)) || [];
      const out = [];
      for (const m of all) {
        if (!m || Number(m.mapId) !== Number(mapId)) continue;
        if (m.type !== 'o2' && m.type !== 'o3') continue;
        const x = m.position && Number(m.position.x);
        const y = m.position && Number(m.position.y);
        if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
        out.push({ x, y, is_statue: m.type === 'o2', type: m.type });
      }
      return out;
    },
    getFilterSignature(ctx) {
      const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : {};
      const types = Array.isArray(st.types) ? [...st.types].sort().join(',') : '';
      const foundSize = st.found instanceof Set ? st.found.size : (Array.isArray(st.found) ? st.found.length : 0);
      const sid = Number(st.sid || 0);
      return `as:${this.getCurrentMapId(ctx)}:${types}:${foundSize}:${sid}`;
    },
    collectVisiblePins(ctx, tf) {
      const mapId = this.getCurrentMapId(ctx);
      const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : {};
      const activeTypes = new Set(Array.isArray(st.types) ? st.types : []);
      if (activeTypes.size === 0) return [];
      const foundSet = st.found instanceof Set ? st.found : new Set(Array.isArray(st.found) ? st.found : []);
      const sourceList = (Array.isArray(st.markers) && st.markers.length > 0)
        ? st.markers
        : ((ctx.catalog && ctx.catalog.allMarkers) || []);
      const pins = [];
      for (const m of sourceList) {
        if (!m || m.id === undefined) continue;
        if (m.mapId !== undefined && Number(m.mapId) !== mapId) continue;
        const typeCode = String(m.type || '');
        if (!activeTypes.has(typeCode) || APPSAMPLE_LANDMARK_TYPES.has(typeCode)) continue;
        const x = m.position && Number(m.position.x);
        const y = m.position && Number(m.position.y);
        if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
        const canon = CT.leafletToCanonical(tf, y, x);
        if (!canon) continue;
        const rawId = String(m.id);
        const lid = appsampleParseLabelId(typeCode);
        const catName = appsampleCategoryLabel(ctx, typeCode);
        let iconUrl = '';
        try {
          if (ctx.catalog && typeof ctx.catalog.markerIconUrl === 'function') {
            iconUrl = String(ctx.catalog.markerIconUrl(m) || '');
          }
        } catch (e) {}
        const ugFloor = (m.sid && Number(m.sid) > 0) ? Number(m.sid)
          : ((m.level && Number(m.level) > 0) ? Number(m.level) : 0);
        pins.push({
          id: `appsample:${rawId}`,
          rawId,
          map_id: mapId,
          lat: canon.lat,
          lng: canon.lng,
          name: String(m.title || m.name || catName || `Pin ${rawId}`),
          category: catName || typeCode,
          label_id: lid,
          icon_url: iconUrl.startsWith('https://') ? iconUrl : '',
          is_underground: ugFloor > 0,
          floor_id: ugFloor > 0 ? String(ugFloor) : null,
          floor_name: ugFloor > 0 ? 'B1' : '',
          description: typeof m.description === 'string' ? m.description.trim() : '',
          image_url: '',
          marked: foundSet.has(rawId)
        });
      }
      return pins;
    },
    subscribe(ctx, onChange) {
      if (ctx.store && typeof ctx.store.subscribe === 'function') {
        let prevTypes = '';
        let prevFound = -1;
        let prevMap = '';
        return ctx.store.subscribe((state) => {
          const tSig = Array.isArray(state && state.types) ? state.types.join(',') : '';
          const fSize = state && state.found instanceof Set ? state.found.size : 0;
          const mKey = String((state && state.mapKey) || '');
          if (tSig !== prevTypes || fSize !== prevFound || mKey !== prevMap) {
            const filterChanged = tSig !== prevTypes || mKey !== prevMap;
            prevTypes = tSig;
            prevFound = fSize;
            prevMap = mKey;
            onChange({ filterChanged, mapChanged: false });
          }
        });
      }
      return null;
    },
    async markPin(ctx, rawId, action) {
      const targetId = String(rawId || '').trim();
      if (!targetId) return { ok: false, error: 'Missing pin id' };
      const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : {};
      if (!st.user) {
        return {
          ok: false,
          authRequired: true,
          error: 'Sign in on AppSample to mark/unmark pins'
        };
      }
      const wantMarked = action !== 'unmark';
      const alreadyFound = st.found instanceof Set ? st.found.has(targetId) : false;
      if (alreadyFound === wantMarked) {
        return { ok: true, already: true };
      }
      if (ctx.progress && typeof ctx.progress.toggleFound === 'function') {
        await ctx.progress.toggleFound(targetId);
        return { ok: true };
      }
      return { ok: false, error: 'AppSample progress controller unavailable' };
    },
    syncUnderground(ctx, isUnderground) {
      try {
        if (!ctx || !ctx.adapter || typeof ctx.adapter.showOverlays !== 'function') return;
        const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : {};
        const curMode = st.overlay && st.overlay.mode ? st.overlay.mode : 'none';
        if (isUnderground && curMode === 'none') {
          ctx.adapter.showOverlays(true);
        } else if (!isUnderground && curMode !== 'none') {
          ctx.adapter.showOverlays(false);
        }
      } catch (e) {}
    },
    getOpenPopupPin(ctx, tf) {
      const infoWin = document.querySelector('[data-testid="info-window"][data-marker-id]');
      if (!infoWin) return null;
      const rawId = infoWin.getAttribute('data-marker-id');
      if (!rawId) return null;
      const m = (ctx.catalog && typeof ctx.catalog.get === 'function') ? ctx.catalog.get(rawId) : null;
      if (!m || !m.position) return null;
      const canon = CT.leafletToCanonical(tf, Number(m.position.y), Number(m.position.x));
      if (!canon) return null;
      const lid = appsampleParseLabelId(m.type);
      const titleEl = infoWin.querySelector('p.font-semibold, h3, h4');
      const title = (titleEl && titleEl.textContent && titleEl.textContent.trim()) ||
        String(m.title || appsampleCategoryLabel(ctx, m.type) || `Pin ${rawId}`);
      return {
        container: infoWin,
        anchorEl: infoWin.querySelector('footer') || infoWin,
        pin: {
          id: `appsample:${rawId}`,
          map_id: this.getCurrentMapId(ctx),
          lat: canon.lat,
          lng: canon.lng,
          name: title,
          category: appsampleCategoryLabel(ctx, m.type),
          label_id: lid,
          is_underground: Boolean(m.sid && Number(m.sid) > 0)
        }
      };
    },
    compactCss: [
      'body.gh-compact-ui header, body.gh-compact-ui [data-testid="top-nav"] { display: none !important; }',
      'body.gh-compact-ui [class*="nitro-ad"], body.gh-compact-ui [id*="ad-"], body.gh-compact-ui [class*="ad-container"] { display: none !important; }'
    ].join('\n')
  };

  registerProviderAdapter(KongyingV3Adapter);
  registerProviderAdapter(AppSampleAdapter);

  function resolveActiveAdapter() {
    for (const adapter of registeredAdapters.values()) {
      try {
        if (adapter.matchesLocation(window.location)) return adapter;
      } catch (e) {}
    }
    return null;
  }

  const activeAdapter = resolveActiveAdapter();
  if (!activeAdapter) {
    return;
  }

  // =========================================================================
  // 2. Shared Runtime State, IPC Bridge, Styles & Overlay Helpers
  // =========================================================================

  let extReqSeq = 0;
  const pendingExtRequests = new Map();
  let autoSyncEnabled = true;
  let engineActive = true;
  let autoPanPinned = true;
  let lastPlayerState = null;
  let lastCanonicalPos = null;
  let lastFilterSig = '';
  let lastNearbyPushTs = 0;
  let lastNearbyPos = null;
  let lastLanPinsFp = '';
  let lastLanPinsTs = 0;
  let lockedTarget = null;
  let compactUiEnabled = false;
  let navMarkChain = Promise.resolve();
  let playerMarker = null;
  let playerDomEl = null;
  let playerMoveHandler = null;
  let boundDragContainer = null;
  let storeUnsub = null;
  let activeCtx = null;
  let activeTspRoute = null;
  let tspSvgEl = null;
  let tspMoveHandler = null;
  let lastCalibratedKey = '';
  let calibrationInFlight = false;
  let prevArrowAngle = null;
  let prevFovAngle = null;
  let isActiveOwnerTab = false;
  let lastMapSwitchTs = 0;

  function isEventForThisProviderTab(ev) {
    if (!ev || typeof ev !== 'object') return true;
    if (ev.target_provider && String(ev.target_provider).toLowerCase() !== activeAdapter.id) return false;
    if (ev.owner_tab_id && String(ev.owner_tab_id) !== GH_TAB_ID) return false;
    return true;
  }

  function tr(key, params, fallback) {
    if (window.GHI18n && typeof window.GHI18n.t === 'function') {
      const val = window.GHI18n.t(key, params);
      if (val && val !== key) return val;
    }
    return fallback !== undefined ? fallback : key;
  }

  function unwrapAngle(prev, next) {
    if (!Number.isFinite(next)) return prev || 0;
    if (prev === null || prev === undefined || !Number.isFinite(prev)) return next;
    let delta = ((next - prev) % 360 + 540) % 360 - 180;
    return prev + delta;
  }

  function extRequest(path, opts = {}) {
    return new Promise((resolve) => {
      const reqId = `pr_${Date.now()}_${++extReqSeq}`;
      const timer = setTimeout(() => {
        pendingExtRequests.delete(reqId);
        resolve({ ok: false, status: 0, data: null, error: 'timeout' });
      }, opts.timeoutMs || 10000);
      pendingExtRequests.set(reqId, { resolve, timer });
      window.postMessage(
        {
          __gh_ext: true,
          dir: 'to-bg',
          kind: 'API_REQUEST',
          reqId,
          path,
          method: opts.method || 'GET',
          body: opts.body
        },
        window.location.origin
      );
    });
  }

  function injectStyles() {
    if (document.getElementById('gh-provider-styles')) return;
    const style = document.createElement('style');
    style.id = 'gh-provider-styles';
    style.textContent = `
      .gh-player-marker-wrap {
        position: relative; width: 44px; height: 44px; pointer-events: none;
        transform-origin: 22px 22px; will-change: transform;
      }
      .gh-player-pulse {
        position: absolute; left: 50%; top: 50%; width: 22px; height: 22px;
        margin: -11px 0 0 -11px; border-radius: 50%;
        background: rgba(126, 232, 255, 0.24);
        animation: ghProvPulse 2.1s ease-out infinite;
      }
      @keyframes ghProvPulse {
        0% { transform: scale(0.75); opacity: 0.9; }
        100% { transform: scale(2.15); opacity: 0; }
      }
      .gh-player-fov-svg {
        position: absolute; left: -18px; top: -18px; width: 80px; height: 80px;
        opacity: 0; pointer-events: none;
        transform: rotate(calc(var(--gh-rot, 0) * 1deg));
        transition: opacity 0.18s ease;
      }
      .gh-player-marker-wrap.gh-has-fov .gh-player-fov-svg { opacity: 0.85; }
      .gh-player-arrow-svg {
        position: absolute; left: 6px; top: 6px; width: 32px; height: 32px;
        transform: rotate(calc(var(--gh-dir, 0) * 1deg));
        filter: drop-shadow(0 2px 5px rgba(20, 24, 34, 0.88));
      }
      #gh-recenter-btn {
        position: fixed; right: 20px; bottom: 88px; z-index: 9998;
        display: none; align-items: center; gap: 6px;
        padding: 8px 14px; border-radius: 999px; border: 1px solid rgba(211,188,142,0.52);
        background: rgba(26, 30, 40, 0.94); color: #F2ECE0; font: 600 12px/1 system-ui, sans-serif;
        cursor: pointer; box-shadow: inset 0 0 0 1px rgba(211,188,142,0.14), 0 6px 20px rgba(8,10,16,0.52);
      }
      #gh-recenter-btn.is-visible { display: inline-flex; }
      #gh-target-lock-pill {
        position: fixed; left: 50%; bottom: 20px; transform: translateX(-50%); z-index: 9998;
        display: none; align-items: center; gap: 8px; padding: 7px 12px;
        border-radius: 999px; border: 1px solid rgba(211,188,142,0.55);
        background: rgba(26, 30, 40, 0.94); color: #F2ECE0; font: 600 12px/1.2 system-ui, sans-serif;
        box-shadow: inset 0 0 0 1px rgba(211,188,142,0.14), 0 6px 22px rgba(8,10,16,0.55);
      }
      #gh-target-lock-pill.is-visible { display: inline-flex; }
      #gh-target-lock-pill button {
        border: 0; border-radius: 999px; padding: 3px 8px; cursor: pointer;
        background: rgba(238, 130, 125, 0.88); color: #141822; font: 600 11px/1 system-ui, sans-serif;
      }
      #gh-mini-toast {
        position: fixed; top: 16px; left: 50%; transform: translateX(-50%); z-index: 10000;
        padding: 8px 14px; border-radius: 10px; border: 1px solid rgba(211,188,142,0.48);
        background: rgba(26, 30, 40, 0.94); color: #F2ECE0; font: 600 12px/1.35 system-ui, sans-serif;
        box-shadow: inset 0 0 0 1px rgba(211,188,142,0.14), 0 8px 24px rgba(8,10,16,0.55); opacity: 0; pointer-events: none;
        transition: opacity 0.18s ease;
      }
      #gh-mini-toast.is-visible { opacity: 1; }
      #gh-popup-navigate-btn {
        margin-left: 6px; padding: 5px 10px; border-radius: 8px;
        border: 1px solid rgba(211,188,142,0.58); background: rgba(43, 50, 68, 0.94);
        color: #DFC48C; font: 600 12px/1.2 system-ui, sans-serif; cursor: pointer;
      }
      #gh-popup-navigate-btn:hover { background: rgba(52, 60, 82, 0.98); }
      ${activeAdapter.compactCss || ''}
    `;
    (document.head || document.documentElement).appendChild(style);
  }

  let toastTimer = null;
  function showMiniToast(text, durationMs = 2600) {
    let el = document.getElementById('gh-mini-toast');
    if (!el) {
      el = document.createElement('div');
      el.id = 'gh-mini-toast';
      document.body.appendChild(el);
    }
    el.textContent = String(text || '');
    el.classList.add('is-visible');
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('is-visible'), durationMs);
  }

  function applyCompactUi(enabled) {
    compactUiEnabled = Boolean(enabled);
    if (document.body) {
      document.body.classList.toggle('gh-compact-ui', compactUiEnabled);
    }
  }

  function ensureRecenterBtn() {
    let btn = document.getElementById('gh-recenter-btn');
    if (!btn && document.body) {
      btn = document.createElement('button');
      btn.id = 'gh-recenter-btn';
      btn.type = 'button';
      btn.textContent = `⌖ ${tr('page.unpin.recenter_btn', null, 'Re-center')}`;
      btn.addEventListener('click', () => {
        autoPanPinned = true;
        btn.classList.remove('is-visible');
        const ctx = getLiveContext();
        if (ctx && lastCanonicalPos) {
          const tf = CT.getTransform(activeAdapter.id, lastCanonicalPos.map_id);
          const ll = tf ? CT.canonicalToLeaflet(tf, lastCanonicalPos.lat, lastCanonicalPos.lng) : null;
          if (ll) ctx.map.panTo(ll, { animate: true, duration: 0.25 });
        }
      });
      document.body.appendChild(btn);
    }
    if (btn) btn.classList.toggle('is-visible', !autoPanPinned && Boolean(lastCanonicalPos));
  }

  function renderTargetLockPill() {
    let pill = document.getElementById('gh-target-lock-pill');
    if (!pill && document.body) {
      pill = document.createElement('div');
      pill.id = 'gh-target-lock-pill';
      const label = document.createElement('span');
      label.className = 'gh-lock-label';
      label.style.cursor = 'pointer';
      label.addEventListener('click', () => {
        const ctx = getLiveContext();
        if (!ctx || !lockedTarget) return;
        const tf = CT.getTransform(activeAdapter.id, lockedTarget.map_id || activeAdapter.getCurrentMapId(ctx));
        const ll = tf ? CT.canonicalToLeaflet(tf, Number(lockedTarget.lat), Number(lockedTarget.lng)) : null;
        if (ll) ctx.map.setView(ll, Math.max(ctx.map.getZoom(), activeAdapter.ratioToZoom(ctx, 0.7)), { animate: true });
      });
      const cancelBtn = document.createElement('button');
      cancelBtn.type = 'button';
      cancelBtn.textContent = tr('page.nav.lock_pill_cancel', null, '✕');
      cancelBtn.addEventListener('click', () => {
        lockedTarget = null;
        renderTargetLockPill();
        extRequest('/nav_command', {
          method: 'POST',
          body: { action: 'unlock_target', tab_id: GH_TAB_ID, provider_id: activeAdapter.id, user_initiated: true }
        });
      });
      pill.appendChild(label);
      pill.appendChild(cancelBtn);
      document.body.appendChild(pill);
    }
    if (!pill) return;
    if (!lockedTarget) {
      pill.classList.remove('is-visible');
      return;
    }
    const label = pill.querySelector('.gh-lock-label');
    if (label) label.textContent = `🎯 ${lockedTarget.name || lockedTarget.id}`;
    pill.classList.add('is-visible');
  }

  // =========================================================================
  // 3. Context Lifecycle, Live Calibration & Player Marker Rendering
  // =========================================================================

  function bindDragUnpin(ctx) {
    const container = ctx && ctx.map && typeof ctx.map.getContainer === 'function' ? ctx.map.getContainer() : null;
    if (!container || container === boundDragContainer) return;
    boundDragContainer = container;
    let downTs = 0;
    let downX = 0;
    let downY = 0;
    const onDown = (ev) => {
      const pt = ev.touches ? ev.touches[0] : ev;
      if (!pt) return;
      downTs = Date.now();
      downX = pt.clientX;
      downY = pt.clientY;
    };
    const onMove = (ev) => {
      if (!downTs || !autoPanPinned) return;
      const pt = ev.touches ? ev.touches[0] : ev;
      if (!pt) return;
      const heldMs = Date.now() - downTs;
      const distPx = Math.hypot(pt.clientX - downX, pt.clientY - downY);
      if (heldMs > 100 && distPx > 20) {
        autoPanPinned = false;
        ensureRecenterBtn();
      }
    };
    const onUp = () => { downTs = 0; };
    container.addEventListener('mousedown', onDown, { passive: true });
    container.addEventListener('mousemove', onMove, { passive: true });
    window.addEventListener('mouseup', onUp, { passive: true });
    container.addEventListener('touchstart', onDown, { passive: true });
    container.addEventListener('touchmove', onMove, { passive: true });
    window.addEventListener('touchend', onUp, { passive: true });
  }

  function cleanupMapOverlays() {
    try {
      if (playerMarker && typeof playerMarker.remove === 'function') playerMarker.remove();
    } catch (e) {}
    playerMarker = null;
    if (playerDomEl && playerDomEl.parentNode) playerDomEl.parentNode.removeChild(playerDomEl);
    playerDomEl = null;
    if (tspSvgEl && tspSvgEl.parentNode) tspSvgEl.parentNode.removeChild(tspSvgEl);
    tspSvgEl = null;
    boundDragContainer = null;
  }

  function getLiveContext() {
    if (activeCtx && activeAdapter.isAlive(activeCtx)) {
      return activeCtx;
    }
    // Map recreated by SPA navigation -> tear down stale overlays & rebind
    if (activeCtx) {
      cleanupMapOverlays();
      if (typeof storeUnsub === 'function') {
        try { storeUnsub(); } catch (e) {}
        storeUnsub = null;
      }
    }
    const next = activeAdapter.findContext();
    if (!next) {
      activeCtx = null;
      return null;
    }
    activeCtx = next;
    bindDragUnpin(activeCtx);
    if (typeof activeAdapter.subscribe === 'function') {
      storeUnsub = activeAdapter.subscribe(activeCtx, ({ filterChanged }) => {
        pushNearbyPins(activeCtx, { force: true, filterChanged: Boolean(filterChanged), userInitiated: true });
      });
    }
    ensureCalibrated(activeCtx);
    return activeCtx;
  }

  async function ensureCalibrated(ctx, force = false) {
    if (!ctx || calibrationInFlight) return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    const anchors = activeAdapter.collectCalibrationAnchors(ctx, mapId);
    const key = `${activeAdapter.id}:${mapId}:${anchors.length}`;
    if (!force && key === lastCalibratedKey) return;
    if (anchors.length < 3) {
      // Rely on verified default seed until map loads anchors
      return;
    }
    calibrationInFlight = true;
    try {
      const resp = await extRequest('/calibrate_provider', {
        method: 'POST',
        body: {
          provider: activeAdapter.id,
          map_id: mapId,
          swap_lat_lng: activeAdapter.id === 'kongying',
          anchors: anchors.slice(0, 1200)
        }
      });
      if (resp && resp.ok && resp.data && resp.data.ok && resp.data.transform) {
        CT.setTransform(activeAdapter.id, mapId, resp.data.transform);
        lastCalibratedKey = key;
        pushNearbyPins(ctx, { force: true, filterChanged: false, userInitiated: false });
      }
    } catch (e) {
    } finally {
      calibrationInFlight = false;
    }
  }

  function buildPlayerMarkerHtml() {
    return `
      <div class="gh-player-marker-wrap">
        <div class="gh-player-pulse"></div>
        <svg class="gh-player-fov-svg" viewBox="0 0 80 80" aria-hidden="true">
          <defs>
            <radialGradient id="ghProvFovGrad" cx="50%" cy="50%" r="50%">
              <stop offset="0%" stop-color="#38bdf8" stop-opacity="0.52"/>
              <stop offset="100%" stop-color="#38bdf8" stop-opacity="0"/>
            </radialGradient>
          </defs>
          <path d="M40 40 L16 8 A38 38 0 0 1 64 8 Z" fill="url(#ghProvFovGrad)"/>
          <path d="M16 8 A38 38 0 0 1 64 8" fill="none" stroke="#D3BC8E" stroke-opacity="0.72" stroke-width="1.5"/>
        </svg>
        <svg class="gh-player-arrow-svg" viewBox="0 0 32 32" aria-hidden="true">
          <path d="M16 3 L25.5 25 L16 20 L6.5 25 Z" fill="#7EE8FF" stroke="#141822" stroke-width="2.4" stroke-linejoin="round" paint-order="stroke fill"/>
          <path d="M16 7.0 L22.2 21.8 L16 18.4 L9.8 21.8 Z" fill="none" stroke="#F0FCFF" stroke-width="1.15" stroke-linejoin="round"/>
          <path d="M16 12.2 L17.1 14.5 L19.4 15.6 L17.1 16.7 L16 19.0 L14.9 16.7 L12.6 15.6 L14.9 14.5 Z" fill="#D3BC8E" stroke="#141822" stroke-width="1.0" stroke-linejoin="round" paint-order="stroke fill"/>
        </svg>
      </div>
    `;
  }

  function updatePlayerMarker(ctx, leafletLatLng, headingDeg, cameraHeadingDeg) {
    if (!ctx || !ctx.map || !leafletLatLng) return;
    const map = ctx.map;
    let pane = typeof map.getPane === 'function' ? map.getPane('ghPlayerPane') : null;
    if (!pane && typeof map.createPane === 'function') {
      pane = map.createPane('ghPlayerPane');
      if (pane) {
        pane.style.zIndex = '690';
        pane.style.pointerEvents = 'none';
      }
    }

    const dir = Number.isFinite(headingDeg) ? unwrapAngle(prevArrowAngle, headingDeg) : (prevArrowAngle || 0);
    prevArrowAngle = dir;
    const hasFov = Number.isFinite(cameraHeadingDeg);
    const rot = hasFov ? unwrapAngle(prevFovAngle, cameraHeadingDeg) : (prevFovAngle || 0);
    if (hasFov) prevFovAngle = rot;

    const applyVars = (rootEl) => {
      const wrap = rootEl && rootEl.querySelector ? (rootEl.classList.contains('gh-player-marker-wrap') ? rootEl : rootEl.querySelector('.gh-player-marker-wrap')) : null;
      if (!wrap) return;
      wrap.style.setProperty('--gh-dir', dir.toFixed(1));
      wrap.style.setProperty('--gh-rot', rot.toFixed(1));
      wrap.classList.toggle('gh-has-fov', hasFov);
    };

    if (window.L && typeof window.L.marker === 'function' && typeof window.L.divIcon === 'function') {
      if (!playerMarker) {
        playerMarker = window.L.marker(leafletLatLng, {
          icon: window.L.divIcon({
            className: 'gh-player-leaflet-icon',
            html: buildPlayerMarkerHtml(),
            iconSize: [44, 44],
            iconAnchor: [22, 22]
          }),
          interactive: false,
          zIndexOffset: 9000,
          pane: pane ? 'ghPlayerPane' : undefined
        }).addTo(map);
      } else {
        playerMarker.setLatLng(leafletLatLng);
      }
      applyVars(typeof playerMarker.getElement === 'function' ? playerMarker.getElement() : null);
      return;
    }

    // Fallback DOM marker repositioned via map.latLngToLayerPoint (for bundled Leaflet like Kongying v3)
    const hostPane = pane || (typeof map.getPane === 'function' ? map.getPane('overlayPane') : null) || map._container;
    if (!playerDomEl && hostPane) {
      playerDomEl = document.createElement('div');
      playerDomEl.id = 'gh-player-marker-dom';
      playerDomEl.style.position = 'absolute';
      playerDomEl.style.left = '0';
      playerDomEl.style.top = '0';
      playerDomEl.style.pointerEvents = 'none';
      playerDomEl.style.zIndex = '690';
      playerDomEl.innerHTML = buildPlayerMarkerHtml();
      hostPane.appendChild(playerDomEl);
      playerMoveHandler = () => {
        if (!playerDomEl || !lastPlayerState || !activeCtx || !activeCtx.map) return;
        const pt = activeCtx.map.latLngToLayerPoint(lastPlayerState.ll);
        playerDomEl.style.transform = `translate3d(${(pt.x - 22).toFixed(2)}px, ${(pt.y - 22).toFixed(2)}px, 0)`;
      };
      map.on('move zoom viewreset zoomend', playerMoveHandler);
    }
    lastPlayerState = { ll: leafletLatLng };
    if (playerDomEl && typeof map.latLngToLayerPoint === 'function') {
      const pt = map.latLngToLayerPoint(leafletLatLng);
      playerDomEl.style.transform = `translate3d(${(pt.x - 22).toFixed(2)}px, ${(pt.y - 22).toFixed(2)}px, 0)`;
      applyVars(playerDomEl);
    }
  }

  // =========================================================================
  // 4. Nearby Pins Feed, LAN Snapshot & TSP Route Overlay
  // =========================================================================

  function pushNearbyPins(ctx, opts = {}) {
    if (!ctx || !activeAdapter.isAlive(ctx)) return false;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    const tf = CT.getTransform(activeAdapter.id, mapId);
    if (!tf) return false;

    const now = Date.now();
    const force = Boolean(opts.force);
    const sig = activeAdapter.getFilterSignature(ctx);
    const filterChanged = Boolean(opts.filterChanged) || (lastFilterSig !== '' && sig !== lastFilterSig);
    lastFilterSig = sig;

    let refLat = lastCanonicalPos && lastCanonicalPos.map_id === mapId ? lastCanonicalPos.lat : null;
    let refLng = lastCanonicalPos && lastCanonicalPos.map_id === mapId ? lastCanonicalPos.lng : null;
    if (refLat === null || refLng === null) {
      const c = ctx.map.getCenter();
      const canonC = CT.leafletToCanonical(tf, c.lat, c.lng);
      if (!canonC) return false;
      refLat = canonC.lat;
      refLng = canonC.lng;
    }

    if (!force && !filterChanged) {
      const moved = lastNearbyPos ? Math.hypot(refLat - lastNearbyPos.lat, refLng - lastNearbyPos.lng) : Infinity;
      const minInterval = moved <= 15 ? 1500 : 300;
      if (now - lastNearbyPushTs < minInterval) return false;
    }
    lastNearbyPushTs = now;
    lastNearbyPos = { lat: refLat, lng: refLng };

    const allVisible = activeAdapter.collectVisiblePins(ctx, tf);
    const uncollected = allVisible.filter((p) => !p.marked);
    const radius = 260;
    const nearby = uncollected
      .map((p) => ({ ...p, distance: Math.hypot(p.lat - refLat, p.lng - refLng) }))
      .filter((p) => p.distance <= radius)
      .sort((a, b) => a.distance - b.distance)
      .slice(0, 40);

    const userInitiated = Boolean(opts.userInitiated);
    extRequest('/nearby_pins', {
      method: 'POST',
      body: {
        tab_id: GH_TAB_ID,
        provider_id: activeAdapter.id,
        tab_visible: !document.hidden,
        tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
        user_initiated: userInitiated,
        filter_changed: filterChanged,
        lat: refLat,
        lng: refLng,
        map_id: mapId,
        coverage_radius: radius,
        pins: nearby
      }
    }).then((resp) => {
      if (resp && resp.ok && resp.data && typeof resp.data.is_owner === 'boolean') {
        isActiveOwnerTab = resp.data.is_owner;
      }
    }).catch(() => {});

    pushLanPinsSnapshot(mapId, allVisible, force || filterChanged, userInitiated);
    return true;
  }

  function pushLanPinsSnapshot(mapId, visiblePins, force, userInitiated) {
    const now = Date.now();
    if (!force && now - lastLanPinsTs < 2500) return;
    const limited = visiblePins.slice(0, 2000);
    const names = [];
    const nameIdx = new Map();
    const catMap = new Map();
    const packed = [];
    let markedCount = 0;

    for (const p of limited) {
      if (p.marked) markedCount += 1;
      const nm = p.name || p.id;
      let ni = nameIdx.get(nm);
      if (ni === undefined) {
        ni = names.length;
        names.push(nm);
        nameIdx.set(nm, ni);
      }
      const lid = Number.isFinite(p.label_id) ? p.label_id : 0;
      if (!catMap.has(lid)) {
        catMap.set(lid, { id: lid, name: p.category || nm, count: 0 });
      }
      catMap.get(lid).count += 1;
      packed.push([
        p.id,
        Number(p.lat.toFixed(2)),
        Number(p.lng.toFixed(2)),
        lid,
        ni,
        p.marked ? 1 : 0,
        p.is_underground ? 1 : 0,
        p.floor_id || ''
      ]);
    }
    const fp = `${activeAdapter.id}:${mapId}:${packed.length}:${markedCount}`;
    if (!force && fp === lastLanPinsFp) return;
    lastLanPinsFp = fp;
    lastLanPinsTs = now;

    extRequest('/lan_pins', {
      method: 'POST',
      body: {
        tab_id: GH_TAB_ID,
        provider_id: activeAdapter.id,
        tab_visible: !document.hidden,
        tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
        user_initiated: Boolean(userInitiated),
        map_id: mapId,
        names,
        categories: Array.from(catMap.values()).sort((a, b) => b.count - a.count),
        pins: packed
      }
    }).catch(() => {});
  }

  function drawTspOverlay(ctx, routeData) {
    activeTspRoute = routeData || null;
    if (tspSvgEl && tspSvgEl.parentNode) tspSvgEl.parentNode.removeChild(tspSvgEl);
    tspSvgEl = null;
    if (!ctx || !ctx.map || !routeData || !Array.isArray(routeData.legs) || routeData.legs.length === 0) return;

    const mapId = activeAdapter.getCurrentMapId(ctx);
    const tf = CT.getTransform(activeAdapter.id, mapId);
    if (!tf) return;
    const pane = typeof ctx.map.getPane === 'function' ? ctx.map.getPane('overlayPane') : null;
    if (!pane) return;

    const svgNS = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(svgNS, 'svg');
    svg.id = 'gh-tsp-route-svg';
    svg.style.position = 'absolute';
    svg.style.left = '0';
    svg.style.top = '0';
    svg.style.overflow = 'visible';
    svg.style.pointerEvents = 'none';
    pane.appendChild(svg);
    tspSvgEl = svg;

    const redraw = () => {
      if (!tspSvgEl || !activeCtx || !activeCtx.map || !activeTspRoute) return;
      while (tspSvgEl.firstChild) tspSvgEl.removeChild(tspSvgEl.firstChild);
      const curTf = CT.getTransform(activeAdapter.id, activeAdapter.getCurrentMapId(activeCtx));
      if (!curTf) return;
      for (const leg of activeTspRoute.legs) {
        const a = CT.canonicalToLeaflet(curTf, Number(leg.from_lat), Number(leg.from_lng));
        const b = CT.canonicalToLeaflet(curTf, Number(leg.to_lat), Number(leg.to_lng));
        if (!a || !b) continue;
        const p1 = activeCtx.map.latLngToLayerPoint(a);
        const p2 = activeCtx.map.latLngToLayerPoint(b);
        const line = document.createElementNS(svgNS, 'line');
        line.setAttribute('x1', p1.x.toFixed(1));
        line.setAttribute('y1', p1.y.toFixed(1));
        line.setAttribute('x2', p2.x.toFixed(1));
        line.setAttribute('y2', p2.y.toFixed(1));
        line.setAttribute('stroke', leg.is_teleport ? '#f59e0b' : '#38bdf8');
        line.setAttribute('stroke-width', '3');
        if (leg.is_teleport) line.setAttribute('stroke-dasharray', '7 5');
        tspSvgEl.appendChild(line);
      }
    };
    redraw();
    if (!tspMoveHandler) {
      tspMoveHandler = redraw;
      ctx.map.on('move zoom viewreset zoomend', tspMoveHandler);
    }
  }

  // =========================================================================
  // 5. Inbound Desktop Event Handling
  // =========================================================================

  function handlePositionEvent(data, { isManualSync = false, autoMark = false } = {}) {
    if (!data || !engineActive) return;
    if (!isManualSync && !autoSyncEnabled) return;
    const lat = Number(data.lat);
    const lng = Number(data.lng);
    if (!Number.isFinite(lat) || !Number.isFinite(lng)) return;
    const targetMapId = Number(data.map_id || 2);
    lastCanonicalPos = { lat, lng, map_id: targetMapId };

    const ctx = getLiveContext();
    if (!ctx) return;

    const curMapId = activeAdapter.getCurrentMapId(ctx);
    if (curMapId !== targetMapId) {
      // Never force full-page reload on a hidden secondary tab when another map tab is driving.
      const now = Date.now();
      const canSwitch = !document.hidden && (isManualSync || isActiveOwnerTab) && (now - lastMapSwitchTs > 6000);
      if (canSwitch) {
        lastMapSwitchTs = now;
        activeAdapter.switchMap(ctx, targetMapId);
      }
      return;
    }

    ensureCalibrated(ctx);
    const tf = CT.getTransform(activeAdapter.id, targetMapId);
    if (!tf) return;
    const ll = CT.canonicalToLeaflet(tf, lat, lng);
    if (!ll) return;

    const heading = Number.isFinite(Number(data.heading)) ? Number(data.heading) : null;
    const camHeading = Number.isFinite(Number(data.camera_heading)) ? Number(data.camera_heading) : null;
    updatePlayerMarker(ctx, ll, heading, camHeading);

    if (typeof data.is_underground === 'boolean' && typeof activeAdapter.syncUnderground === 'function') {
      activeAdapter.syncUnderground(ctx, data.is_underground);
    }

    if (isManualSync) {
      autoPanPinned = true;
      ensureRecenterBtn();
      const targetZoom = activeAdapter.ratioToZoom(ctx, Number(data.zoom_ratio_t));
      ctx.map.setView(ll, targetZoom, { animate: true, duration: 0.25 });
    } else if (autoPanPinned) {
      ctx.map.panTo(ll, { animate: false });
    }

    pushNearbyPins(ctx, { force: isManualSync, userInitiated: isManualSync });

    if (autoMark) {
      // Find nearest uncollected pin within 60 canonical units and mark it
      const visible = activeAdapter.collectVisiblePins(ctx, tf).filter((p) => !p.marked);
      let best = null;
      let bestD = 60;
      for (const p of visible) {
        const d = Math.hypot(p.lat - lat, p.lng - lng);
        if (d < bestD) { bestD = d; best = p; }
      }
      if (best) {
        handleNavMarkPin({ pin_id: best.id, action: 'mark' });
      }
    }
  }

  function handleNavMarkPin(data) {
    const fullId = String((data && data.pin_id) || '').trim();
    const action = (data && data.action === 'unmark') ? 'unmark' : 'mark';
    const seq = (data && typeof data._seq === 'number') ? data._seq : 0;
    const prefix = `${activeAdapter.id}:`;
    // Strictly require this provider's namespace prefix AND active owner/provider routing so
    // pure-digit HoYoLAB pins or another provider's pins are NEVER touched or rolled back.
    if (!fullId.startsWith(prefix) || !isEventForThisProviderTab(data)) {
      return;
    }
    const rawId = fullId.slice(prefix.length);

    navMarkChain = navMarkChain.then(async () => {
      const ctx = getLiveContext();
      if (!ctx) {
        extRequest('/action_feedback', {
          method: 'POST',
          body: { event_type: 'NAV_MARK', status: 'error', action, pin_id: fullId, seq, detail: 'Map not ready' }
        });
        return;
      }
      try {
        const res = await activeAdapter.markPin(ctx, rawId, action);
        if (res && res.ok) {
          extRequest('/action_feedback', {
            method: 'POST',
            body: { event_type: 'NAV_MARK', status: 'ok', action, pin_id: fullId, seq }
          });
          pushNearbyPins(ctx, { force: true, userInitiated: true });
        } else {
          const msg = (res && res.error) || 'Failed to mark pin';
          showMiniToast(msg, 3200);
          extRequest('/action_feedback', {
            method: 'POST',
            body: { event_type: 'NAV_MARK', status: 'error', action, pin_id: fullId, seq, detail: msg }
          });
        }
      } catch (err) {
        const msg = (err && err.message) ? err.message : 'Mark error';
        extRequest('/action_feedback', {
          method: 'POST',
          body: { event_type: 'NAV_MARK', status: 'error', action, pin_id: fullId, seq, detail: msg }
        });
      }
    });
  }

  function handleServerEvent(data) {
    if (!data || typeof data !== 'object') return;
    switch (data.type) {
      case 'ENGINE_STATE_UPDATED':
        if (typeof data.engine_active === 'boolean') engineActive = data.engine_active;
        break;
      case 'AUTO_SYNC_UPDATED':
        if (typeof data.auto_sync === 'boolean') autoSyncEnabled = data.auto_sync;
        break;
      case 'MINIMAP_POSITION':
      case 'LIVE_POSITION':
        handlePositionEvent(data, { isManualSync: false });
        break;
      case 'SYNC_POSITION_ONLY':
        handlePositionEvent(data, { isManualSync: true });
        break;
      case 'SYNC_AND_MARK':
        handlePositionEvent(data, { isManualSync: true, autoMark: isEventForThisProviderTab(data) });
        break;
      case 'NAV_MARK_PIN':
        handleNavMarkPin(data);
        break;
      case 'TARGET_LOCK_UPDATED': {
        const nextLock = data.locked_target || null;
        const lockId = nextLock && nextLock.id !== undefined && nextLock.id !== null ? String(nextLock.id) : '';
        if (nextLock && lockId.startsWith(`${activeAdapter.id}:`)) {
          lockedTarget = nextLock;
        } else {
          lockedTarget = null;
        }
        renderTargetLockPill();
        break;
      }
      case 'TSP_ROUTE_UPDATED': {
        const ctx = getLiveContext();
        if (data.target_provider && String(data.target_provider).toLowerCase() !== activeAdapter.id) {
          drawTspOverlay(ctx, null);
        } else {
          drawTspOverlay(ctx, data.route || data);
        }
        break;
      }
      case 'GH_CMD_REFRESH_PINS': {
        const ctx = getLiveContext();
        if (ctx) pushNearbyPins(ctx, { force: true, filterChanged: true, userInitiated: true });
        break;
      }
      case 'GH_CMD_PLAN_TSP': {
        const ctx = getLiveContext();
        if (!ctx) break;
        const mapId = activeAdapter.getCurrentMapId(ctx);
        const tf = CT.getTransform(activeAdapter.id, mapId);
        if (!tf) break;
        const bounds = ctx.map.getBounds();
        const pins = activeAdapter.collectVisiblePins(ctx, tf)
          .filter((p) => !p.marked)
          .filter((p) => {
            const ll = CT.canonicalToLeaflet(tf, p.lat, p.lng);
            return ll && bounds.contains(ll);
          })
          .slice(0, 60);
        if (pins.length === 0) {
          showMiniToast(tr('page.tsp.no_pins', null, 'No visible pins in view'));
          break;
        }
        extRequest('/plan_tsp_route', {
          method: 'POST',
          body: { map_id: mapId, pins }
        }).then((resp) => {
          if (resp && resp.ok && resp.data) drawTspOverlay(ctx, resp.data.route || resp.data);
        });
        break;
      }
      case 'GH_CMD_CLEAR_TSP':
        drawTspOverlay(getLiveContext(), null);
        break;
      default:
        break;
    }
  }

  // =========================================================================
  // 6. Popup "Navigate" Button Observer & Tab Visibility / State Heartbeat
  // =========================================================================

  function syncPopupNavigateButton() {
    const ctx = getLiveContext();
    if (!ctx || typeof activeAdapter.getOpenPopupPin !== 'function') return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    const tf = CT.getTransform(activeAdapter.id, mapId);
    if (!tf) return;
    const info = activeAdapter.getOpenPopupPin(ctx, tf);
    if (!info || !info.anchorEl || !info.pin) return;
    if (info.container.querySelector('#gh-popup-navigate-btn')) return;

    const btn = document.createElement('button');
    btn.id = 'gh-popup-navigate-btn';
    btn.type = 'button';
    btn.textContent = `🧭 ${tr('page.nav.navigate_btn', null, 'Navigate')}`;
    btn.addEventListener('click', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      lockedTarget = info.pin;
      renderTargetLockPill();
      extRequest('/nav_command', {
        method: 'POST',
        body: {
          action: 'lock_target',
          auto_switch: true,
          tab_id: GH_TAB_ID,
          provider_id: activeAdapter.id,
          user_initiated: true,
          pin: info.pin
        }
      });
    });
    info.anchorEl.appendChild(btn);
  }

  function reportTabVisibility(interacted = false, closing = false) {
    extRequest('/tab_visibility', {
      method: 'POST',
      body: {
        tab_id: GH_TAB_ID,
        provider_id: activeAdapter.id,
        visible: !document.hidden,
        focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
        interacted: Boolean(interacted),
        closing: Boolean(closing)
      }
    }).then((resp) => {
      if (resp && resp.ok && resp.data && typeof resp.data.is_owner === 'boolean') {
        isActiveOwnerTab = resp.data.is_owner;
      }
    }).catch(() => {});
  }

  function reportState() {
    const ctx = getLiveContext();
    if (!ctx) return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    const center = ctx.map.getCenter();
    const tf = CT.getTransform(activeAdapter.id, mapId);
    const canonC = tf ? CT.leafletToCanonical(tf, center.lat, center.lng) : null;
    extRequest('/report_state', {
      method: 'POST',
      body: {
        tab_id: GH_TAB_ID,
        provider_id: activeAdapter.id,
        tab_visible: !document.hidden,
        tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
        map_id: mapId,
        center: canonC ? [canonC.lat, canonC.lng] : [center.lat, center.lng],
        zoom: ctx.map.getZoom(),
        calibrated: Boolean(tf && tf.source === 'auto_calibrated'),
        scriptVersion: SCRIPT_VERSION,
        scriptBuild: SCRIPT_BUILD,
        runtimeMode: 'extension'
      }
    }).then((resp) => {
      if (resp && resp.ok && resp.data && typeof resp.data.is_owner === 'boolean') {
        isActiveOwnerTab = resp.data.is_owner;
      }
    }).catch(() => {});
  }

  function onWindowMessage(e) {
    if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-page') return;
    if (e.data.kind === 'API_RESPONSE' && e.data.reqId) {
      const entry = pendingExtRequests.get(e.data.reqId);
      if (entry) {
        clearTimeout(entry.timer);
        pendingExtRequests.delete(e.data.reqId);
        entry.resolve(e.data.response);
      }
      return;
    }
    if (e.data.kind === 'SERVER_EVENT' && e.data.data) {
      handleServerEvent(e.data.data);
      return;
    }
    if (e.data.kind === 'COMPACT_UI_SYNC' && typeof e.data.enabled === 'boolean') {
      applyCompactUi(e.data.enabled);
      return;
    }
    if (e.data.kind === 'CONNECTION_STATUS' && e.data.connected) {
      if (typeof e.data.engineActive === 'boolean') engineActive = e.data.engineActive;
      const ctx = getLiveContext();
      if (ctx) {
        ensureCalibrated(ctx, true);
        pushNearbyPins(ctx, { force: true, userInitiated: false });
      }
    }
  }

  // =========================================================================
  // 7. Boot & Teardown
  // =========================================================================

  injectStyles();
  window.addEventListener('message', onWindowMessage);

  const onVis = () => {
    const nowVisible = !document.hidden;
    const focused = typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false;
    reportTabVisibility(nowVisible && focused, false);
  };
  const onFocus = () => {
    reportTabVisibility(true, false);
    const ctx = getLiveContext();
    if (ctx) pushNearbyPins(ctx, { force: true, userInitiated: true });
  };
  const onHide = () => reportTabVisibility(false, true);
  const onClick = () => {
    setTimeout(() => {
      syncPopupNavigateButton();
      const ctx = getLiveContext();
      if (ctx) pushNearbyPins(ctx, { force: false, userInitiated: true });
    }, 180);
  };

  document.addEventListener('visibilitychange', onVis);
  window.addEventListener('focus', onFocus);
  window.addEventListener('pagehide', onHide);
  document.addEventListener('click', onClick, true);

  const popupObserver = new MutationObserver(() => syncPopupNavigateButton());
  if (document.body) {
    popupObserver.observe(document.body, { childList: true, subtree: true });
  }

  const visTimer = setInterval(() => reportTabVisibility(false, false), 5000);
  const stateTimer = setInterval(() => {
    const ctx = getLiveContext();
    if (ctx) {
      ensureCalibrated(ctx);
      reportState();
      pushNearbyPins(ctx, { force: false, userInitiated: false });
    }
  }, 4000);

  // Hydrate initial desktop status
  extRequest('/status').then((resp) => {
    if (resp && resp.ok && resp.data) {
      if (typeof resp.data.engine_active === 'boolean') engineActive = resp.data.engine_active;
      if (typeof resp.data.auto_sync === 'boolean') autoSyncEnabled = resp.data.auto_sync;
      if (resp.data.locked_target && String(resp.data.locked_target.id || '').startsWith(`${activeAdapter.id}:`)) {
        lockedTarget = resp.data.locked_target;
        renderTargetLockPill();
      }
    }
    reportTabVisibility(false, false);
    const ctx = getLiveContext();
    if (ctx) {
      ensureCalibrated(ctx, true);
      pushNearbyPins(ctx, { force: true, userInitiated: false });
    }
  });

  // Expose diagnostic & E2E verification bridge (mirrors HoYoLAB window.__GH_BRIDGE__)
  window.__GH_BRIDGE__ = {
    tabId: GH_TAB_ID,
    providerId: activeAdapter.id,
    providerName: activeAdapter.name,
    capabilities: activeAdapter.capabilities,
    isActiveOwner: () => isActiveOwnerTab,
    getContext: () => getLiveContext(),
    getCurrentMapId: () => {
      const ctx = getLiveContext();
      return ctx ? activeAdapter.getCurrentMapId(ctx) : null;
    },
    getTransform: () => {
      const ctx = getLiveContext();
      return ctx ? CT.getTransform(activeAdapter.id, activeAdapter.getCurrentMapId(ctx)) : null;
    },
    calibrateNow: () => ensureCalibrated(getLiveContext(), true),
    queryNearbyUncollectedPins: () => {
      const ctx = getLiveContext();
      if (!ctx) return [];
      const tf = CT.getTransform(activeAdapter.id, activeAdapter.getCurrentMapId(ctx));
      return tf ? activeAdapter.collectVisiblePins(ctx, tf).filter((p) => !p.marked) : [];
    },
    handleServerEvent,
    setCompactUI: applyCompactUi,
    toggleCompactUI: () => applyCompactUi(!compactUiEnabled),
    recenter: () => {
      autoPanPinned = true;
      ensureRecenterBtn();
      const ctx = getLiveContext();
      if (ctx && lastCanonicalPos) {
        const tf = CT.getTransform(activeAdapter.id, lastCanonicalPos.map_id);
        const ll = tf ? CT.canonicalToLeaflet(tf, lastCanonicalPos.lat, lastCanonicalPos.lng) : null;
        if (ll) ctx.map.panTo(ll, { animate: true });
      }
    },
    unwrapAngle
  };

  window.__gh_provider_teardown = () => {
    clearInterval(visTimer);
    clearInterval(stateTimer);
    popupObserver.disconnect();
    window.removeEventListener('message', onWindowMessage);
    document.removeEventListener('visibilitychange', onVis);
    window.removeEventListener('focus', onFocus);
    window.removeEventListener('pagehide', onHide);
    document.removeEventListener('click', onClick, true);
    if (typeof storeUnsub === 'function') {
      try { storeUnsub(); } catch (e) {}
    }
    cleanupMapOverlays();
  };
})();
