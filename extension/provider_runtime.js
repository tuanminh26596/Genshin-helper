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
  const SCRIPT_BUILD = 'multi-provider-v5';

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
    setFloor(ctx, req) {
      try {
        const wantUnderground = !(req && req.isSurface);
        if (!ctx || !ctx.vm || !ctx.vm.underground_show) {
          return { ok: false, supported: false, reason: 'underground_unavailable' };
        }
        if (Boolean(ctx.vm.underground_opacity) !== wantUnderground && typeof ctx.vm.underground_opacity_switch === 'function') {
          ctx.vm.underground_opacity_switch();
        }
        return { ok: true, mode: wantUnderground ? 'underground' : 'surface', granularity: 'toggle' };
      } catch (e) {
        return { ok: false, error: e && e.message ? e.message : 'setFloor failed' };
      }
    },
    syncUnderground(ctx, isUnderground) {
      this.setFloor(ctx, { isSurface: !isUnderground });
    },
    focusPin(ctx, rawId, ll) {
      const wantId = String(rawId || '').trim();
      let targetLayer = null;
      let bestDist = Infinity;
      kongyingEachMarkerLayer(ctx.map, (layer) => {
        const d = layer.options && layer.options.data;
        if (!d || d.id === undefined) return;
        if (wantId && String(d.id) === wantId) {
          targetLayer = layer;
          bestDist = -1;
          return;
        }
        if (!wantId && bestDist >= 0 && ll && typeof layer.getLatLng === 'function') {
          const lll = layer.getLatLng();
          if (lll && Number.isFinite(lll.lat) && Number.isFinite(lll.lng)) {
            const dist = Math.hypot(lll.lat - ll[0], lll.lng - ll[1]);
            if (dist < bestDist) {
              bestDist = dist;
              targetLayer = layer;
            }
          }
        }
      });
      if (!targetLayer) return { ok: false };
      const layerLatLng = typeof targetLayer.getLatLng === 'function'
        ? targetLayer.getLatLng()
        : (ll ? { lat: ll[0], lng: ll[1] } : null);
      if (typeof targetLayer.fire === 'function') {
        try {
          targetLayer.fire('click', { target: targetLayer, latlng: layerLatLng });
        } catch (e) {}
      }
      const opened = Boolean(ctx.vm && ctx.vm.handle_layer && ctx.vm.handle_layer.target === targetLayer);
      if (!opened) {
        if (ctx.vm && typeof ctx.vm.popup_callback === 'function') {
          try {
            ctx.vm.popup_callback({ target: targetLayer, latlng: layerLatLng });
          } catch (e) {}
        } else if (typeof targetLayer.openPopup === 'function') {
          try {
            targetLayer.openPopup();
          } catch (e) {}
        }
      }
      return { ok: true, layer: targetLayer };
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

  // Mirrors the AppSample page: comments (and with them the summary and screenshots) show for
  // markers whose level reaches config.rules.commentMinLevel while the comments feature is on.
  function appsampleHasCommunityGuide(ctx, m) {
    const cfg = (ctx && ctx.catalog && ctx.catalog.config) || {};
    if (cfg.features && cfg.features.comments === false) return false;
    const rules = cfg.rules || {};
    const minLevel = Number.isFinite(Number(rules.commentMinLevel)) ? Number(rules.commentMinLevel) : 4;
    const level = Number(m && m.level);
    return Number.isFinite(level) && level >= minLevel;
  }

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

  function appsampleGetOverlays(ctx) {
    try {
      const curMap = ctx && ctx.appCtx && ctx.appCtx.mapSync && ctx.appCtx.mapSync.deps &&
        typeof ctx.appCtx.mapSync.deps.currentMap === 'function'
        ? ctx.appCtx.mapSync.deps.currentMap()
        : null;
      if (curMap && Array.isArray(curMap.overlays)) return curMap.overlays;
      if (ctx && ctx.adapter && ctx.adapter.overlays instanceof Map) {
        return Array.from(ctx.adapter.overlays.values());
      }
    } catch (e) {}
    return [];
  }

  function appsampleResolveOverlayTarget(ctx, req) {
    const rawPinId = req && req.rawPinId ? String(req.rawPinId).trim() : '';
    if (rawPinId && ctx && ctx.catalog && typeof ctx.catalog.get === 'function') {
      const m = ctx.catalog.get(rawPinId);
      if (m && m.layer && Number(m.layer.floorId) > 0) {
        const fid = Number(m.layer.floorId);
        const gid = Number(m.layer.groupId || 0);
        if (gid > 0) return { groupId: gid, floorId: fid };
      }
    }

    const overlays = appsampleGetOverlays(ctx);
    const wantFloorId = req && req.floorId !== undefined && req.floorId !== null ? Number(req.floorId) : 0;
    const wantGroupId = req && req.groupId !== undefined && req.groupId !== null ? Number(req.groupId) : 0;
    if (Number.isFinite(wantFloorId) && wantFloorId > 0) {
      const ovMatch = (wantGroupId > 0
        ? overlays.find((ov) => ov && Number(ov.id) === wantFloorId && Number(ov.groupId) === wantGroupId)
        : null) || overlays.find((ov) => ov && Number(ov.id) === wantFloorId);
      if (ovMatch && Number(ovMatch.groupId) > 0) {
        return { groupId: Number(ovMatch.groupId), floorId: wantFloorId };
      }
      const all = (ctx && ctx.catalog && (ctx.catalog.allMarkers || ctx.catalog.officialMarkers)) || [];
      for (const m of all) {
        if (m && m.layer && Number(m.layer.floorId) === wantFloorId && Number(m.layer.groupId) > 0) {
          return { groupId: Number(m.layer.groupId), floorId: wantFloorId };
        }
      }
      if (Number.isFinite(wantGroupId) && wantGroupId > 0) {
        return { groupId: wantGroupId, floorId: wantFloorId };
      }
    }

    const pt = req && req.providerPoint;
    if (pt && Number.isFinite(pt.x) && Number.isFinite(pt.y)) {
      const floorHint = req && req.floorName ? String(req.floorName).trim().toLowerCase() : '';
      let fallbackMatch = null;
      for (const ov of overlays) {
        if (!ov || !ov.bounds) continue;
        const b = ov.bounds;
        if (pt.x >= b.west && pt.x <= b.east && pt.y >= b.south && pt.y <= b.north) {
          const oid = Number(ov.id);
          const gid = Number(ov.groupId);
          if (oid > 0 && gid > 0) {
            if (floorHint && String(ov.name || '').trim().toLowerCase() === floorHint) {
              return { groupId: gid, floorId: oid };
            }
            if (!fallbackMatch) fallbackMatch = { groupId: gid, floorId: oid };
          }
        }
      }
      if (fallbackMatch) return fallbackMatch;
    }
    return null;
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
        // Guide image: AppSample's own rule (m.img and level >= rules.imageMinLevel) lives in
        // catalog.markerImageUrl; ~5.6% of markers have one (game-cdn.appsample.com/gim/item-imgs).
        let imageUrl = '';
        try {
          if (ctx.catalog && typeof ctx.catalog.markerImageUrl === 'function') {
            imageUrl = String(ctx.catalog.markerImageUrl(m) || '');
          }
        } catch (e) {}
        // Community guide: AppSample's popup shows the comments thread (summary + player
        // screenshots) only for markers at or above rules.commentMinLevel; the app fetches it
        // on demand for the target pin, keyed by the marker type (`o17`) and id.
        const communityGuide = appsampleHasCommunityGuide(ctx, m);
        // Underground markers carry layer.{groupId, floorId}. m.level is the marker's zoom level
        // (4..6 on live surface markers), not a floor, so it must not mark a pin underground.
        const ugFloor = (m.layer && typeof m.layer === 'object' && Number(m.layer.floorId) > 0)
          ? Number(m.layer.floorId) : 0;
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
          image_url: imageUrl.startsWith('https://') ? imageUrl : '',
          type_code: typeCode,
          community_guide: communityGuide,
          marked: foundSet.has(rawId)
        });
      }
      return pins;
    },
    subscribe(ctx, onChange) {
      if (ctx.store && typeof ctx.store.subscribe === 'function') {
        const initSt = typeof ctx.store.getState === 'function' ? (ctx.store.getState() || {}) : {};
        let prevTypes = Array.isArray(initSt.types) ? initSt.types.join(',') : '';
        let prevFound = initSt.found instanceof Set ? initSt.found.size : (Array.isArray(initSt.found) ? initSt.found.length : 0);
        let prevMap = String(initSt.mapKey || '');
        return ctx.store.subscribe((state) => {
          const tSig = Array.isArray(state && state.types) ? state.types.join(',') : '';
          const fSize = state && state.found instanceof Set ? state.found.size : (Array.isArray(state && state.found) ? state.found.length : 0);
          const mKey = String((state && state.mapKey) || '');
          if (tSig !== prevTypes || fSize !== prevFound || mKey !== prevMap) {
            const mapChanged = mKey !== prevMap;
            const filterChanged = tSig !== prevTypes || mapChanged;
            prevTypes = tSig;
            prevFound = fSize;
            prevMap = mKey;
            onChange({ filterChanged, mapChanged });
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
    setFloor(ctx, req) {
      try {
        if (!ctx) return { ok: false };
        const applyOverlay = () => {
          if (ctx.appCtx && ctx.appCtx.mapSync && typeof ctx.appCtx.mapSync.applyOverlay === 'function') {
            ctx.appCtx.mapSync.applyOverlay();
            return true;
          }
          return false;
        };
        if (req && req.isSurface) {
          if (ctx.store && typeof ctx.store.setState === 'function') {
            ctx.store.setState({ overlay: { mode: 'none' } });
          }
          if (!applyOverlay() && ctx.adapter && typeof ctx.adapter.showOverlays === 'function') {
            ctx.adapter.showOverlays([]);
          }
          return { ok: true, mode: 'none' };
        }
        const target = appsampleResolveOverlayTarget(ctx, req);
        if (target && target.groupId > 0 && target.floorId > 0) {
          const nextOverlay = { mode: 'group', groupId: target.groupId, floorId: target.floorId };
          if (ctx.store && typeof ctx.store.setState === 'function') {
            ctx.store.setState({ overlay: nextOverlay });
          }
          if (!applyOverlay() && ctx.adapter && typeof ctx.adapter.showOverlays === 'function') {
            ctx.adapter.showOverlays([target.floorId]);
          }
          return { ok: true, mode: 'group', groupId: target.groupId, floorId: target.floorId };
        }
        if (ctx.store && typeof ctx.store.setState === 'function') {
          ctx.store.setState({ overlay: { mode: 'all' } });
        }
        if (!applyOverlay() && ctx.adapter && typeof ctx.adapter.showOverlays === 'function') {
          const fid = req && req.floorId !== undefined ? Number(req.floorId) : 0;
          ctx.adapter.showOverlays(Number.isFinite(fid) && fid > 0 ? [fid] : []);
        }
        return { ok: true, mode: 'all' };
      } catch (e) {
        return { ok: false, error: e && e.message ? e.message : 'setFloor failed' };
      }
    },
    syncUnderground(ctx, isUnderground, opts) {
      try {
        const st = ctx.store && typeof ctx.store.getState === 'function' ? ctx.store.getState() : {};
        const curMode = st.overlay && st.overlay.mode ? st.overlay.mode : 'none';
        if (isUnderground && curMode === 'none') {
          this.setFloor(ctx, { isSurface: false, providerPoint: opts && opts.providerPoint });
        } else if (!isUnderground && curMode !== 'none') {
          this.setFloor(ctx, { isSurface: true });
        }
      } catch (e) {}
    },
    focusPin(ctx, rawId) {
      const targetId = String(rawId || '').trim();
      if (!targetId) return { ok: false };
      const m = (ctx.catalog && typeof ctx.catalog.get === 'function') ? ctx.catalog.get(targetId) : null;
      if (ctx.appCtx && ctx.appCtx.mapSync && ctx.appCtx.mapSync.deps &&
        typeof ctx.appCtx.mapSync.deps.onMarkerClick === 'function') {
        try {
          ctx.appCtx.mapSync.deps.onMarkerClick(targetId);
          return { ok: true, marker: m };
        } catch (e) {}
      }
      if (m && m.layer && Number(m.layer.floorId) > 0) {
        this.setFloor(ctx, { isSurface: false, floorId: m.layer.floorId, groupId: m.layer.groupId, rawPinId: targetId });
      }
      if (ctx.store && typeof ctx.store.setState === 'function') {
        ctx.store.setState({ selectedId: targetId, popupOpen: true });
      }
      if (ctx.appCtx && ctx.appCtx.mapSync && typeof ctx.appCtx.mapSync.applyPopup === 'function') {
        ctx.appCtx.mapSync.applyPopup();
      } else if (ctx.adapter && typeof ctx.adapter.setMarkerState === 'function') {
        ctx.adapter.setMarkerState(targetId, { selected: true });
      }
      return { ok: Boolean(m || targetId), marker: m };
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
  let activeWebRoute = null;
  let routeInspecting = false;
  let tspSvgEl = null;
  let tspMoveHandler = null;
  let tspBoundMap = null;
  let playerBoundMap = null;
  let routeBarEl = null;
  let lastObservedMapId = null;
  let lastCalibratedKey = '';
  let calibrationInFlight = false;
  let prevArrowAngle = null;
  let prevFovAngle = null;
  let isActiveOwnerTab = false;
  let lastMapSwitchTs = 0;
  let lastToastSpec = null;
  let lastNoteModalFolder = '';
  let lastNoteModalTs = 0;
  let activeDiagModalRefresh = null;
  let activeDiagModalClose = null;
  let i18nUnsub = null;

  function isEventForThisProviderTab(ev) {
    if (!ev || typeof ev !== 'object') return true;
    if (ev.target_provider && String(ev.target_provider).toLowerCase() !== activeAdapter.id) return false;
    if (ev.owner_tab_id && String(ev.owner_tab_id) !== GH_TAB_ID) return false;
    return true;
  }

  function interpolateFallback(template, params) {
    if (!params || typeof params !== 'object') return String(template || '');
    return String(template || '').replace(/\{(\w+)\}/g, (match, name) => {
      if (Object.prototype.hasOwnProperty.call(params, name) && params[name] !== undefined && params[name] !== null) {
        return String(params[name]);
      }
      return match;
    });
  }

  function tr(key, params, fallback) {
    if (window.GHI18n && typeof window.GHI18n.t === 'function') {
      const val = window.GHI18n.t(key, params);
      if (val && val !== key) return val;
    }
    return fallback !== undefined ? interpolateFallback(fallback, params) : key;
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
      #gh-popup-navigate-btn.is-locked {
        border-color: rgba(238, 130, 125, 0.65); background: rgba(127, 29, 29, 0.85); color: #FDE8E8;
      }
      body.gh-has-route-bar #gh-mini-toast { top: 62px; }
      .gh-tsp-route-svg {
        position: absolute; left: 0; top: 0; pointer-events: none; overflow: visible; z-index: 450;
      }
      .gh-tsp-stop-label {
        font-family: 'JetBrains Mono', Consolas, monospace; font-size: 10px; font-weight: 700; fill: #F2ECE0;
      }
      #gh-route-bar {
        position: fixed; top: 12px; left: 50%; transform: translateX(-50%); z-index: 2147483000;
        display: flex; flex-wrap: wrap; align-items: center; justify-content: center; gap: 6px 10px;
        padding: 5px 5px 5px 14px; border-radius: 9px;
        background: rgba(26, 30, 40, 0.94); border: 1px solid rgba(211, 188, 142, 0.44);
        box-shadow: 0 10px 28px rgba(8, 10, 16, 0.65); color: #F2ECE0;
        font: 600 12px/1.25 -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Arial, sans-serif;
        font-variant-numeric: lining-nums tabular-nums;
        pointer-events: auto; width: max-content; max-width: calc(100vw - 24px); box-sizing: border-box;
      }
      #gh-route-bar * { box-sizing: border-box; }
      #gh-route-bar .gh-route-group { display: inline-flex; align-items: center; gap: 6px; min-width: 0; }
      #gh-route-bar .gh-route-sep { width: 1px; height: 18px; background: #232D42; flex-shrink: 0; }
      #gh-route-bar .gh-route-dot {
        width: 8px; height: 8px; border-radius: 50%; flex-shrink: 0; box-shadow: 0 0 0 3px #232D42;
      }
      #gh-route-bar .gh-route-title {
        max-width: 260px; min-width: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
        color: #F2ECE0; font-size: 14px; font-weight: 700; letter-spacing: -0.01em;
      }
      #gh-route-bar .gh-route-sec {
        display: inline-flex; align-items: baseline; gap: 5px; min-width: 74px; justify-content: center;
        font-size: 12px; white-space: nowrap; font-variant-numeric: lining-nums tabular-nums;
      }
      #gh-route-bar .gh-route-sec b { font-weight: 700; color: #E4CF9F; }
      #gh-route-bar .gh-route-sec span { font-weight: 500; color: #B0A89A; }
      #gh-route-bar .gh-route-btn {
        display: inline-flex; align-items: center; justify-content: center; gap: 6px;
        height: 28px; margin: 0; padding: 0 11px; border: 1px solid rgba(211, 188, 142, 0.36);
        border-radius: 7px; background: #222836; color: #F2ECE0;
        font: 600 12px/1 -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Arial, sans-serif;
        white-space: nowrap; cursor: pointer;
        transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease;
      }
      #gh-route-bar .gh-route-btn svg { width: 14px; height: 14px; flex-shrink: 0; }
      #gh-route-bar .gh-route-btn.is-icon { width: 28px; padding: 0; }
      #gh-route-bar .gh-route-btn:hover:not(:disabled) { border-color: rgba(211, 188, 142, 0.68); background: #2B3244; }
      #gh-route-bar .gh-route-btn:disabled { opacity: 0.38; cursor: default; }
      #gh-route-bar .gh-route-btn.is-primary {
        border-color: rgba(228, 207, 159, 0.68); background: linear-gradient(135deg, #D3BC8E, #B59963); color: #141822;
      }
      #gh-route-bar .gh-route-btn.is-follow {
        border-color: rgba(211, 188, 142, 0.6); background: rgba(211, 188, 142, 0.14); color: #E4CF9F;
      }
      #gh-route-bar .gh-route-btn.is-stop:hover:not(:disabled) {
        border-color: rgba(238, 130, 125, 0.65); background: rgba(238, 130, 125, 0.16); color: #FDE8E8;
      }
      #gh-diag-note-modal {
        position: fixed; inset: 0; width: 100vw; height: 100vh;
        background: rgba(8, 10, 16, 0.74); backdrop-filter: blur(10px);
        z-index: 2147483647; display: flex; align-items: center; justify-content: center;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Arial, sans-serif;
      }
      #gh-diag-note-modal * { box-sizing: border-box; font-variant-numeric: lining-nums tabular-nums; }
      ${activeAdapter.compactCss || ''}
    `;
    (document.head || document.documentElement).appendChild(style);
  }

  let toastTimer = null;
  function showMiniToast(text, durationMs = 2600, toastSpec = null) {
    let el = document.getElementById('gh-mini-toast');
    if (!el && document.body) {
      el = document.createElement('div');
      el.id = 'gh-mini-toast';
      document.body.appendChild(el);
    }
    if (!el) return;
    lastToastSpec = toastSpec || null;
    el.textContent = String(text || '');
    el.classList.add('is-visible');
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      lastToastSpec = null;
      el.classList.remove('is-visible');
    }, durationMs);
  }

  function showLocalizedToast(key, params = null, fallback = undefined, durationMs = 2600) {
    showMiniToast(tr(key, params, fallback), durationMs, { key, params, fallback });
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
    if (btn) {
      const labelText = tr('page.unpin.recenter_btn', null, 'Re-center');
      btn.textContent = `⌖ ${labelText}`;
      btn.title = tr('page.unpin.recenter_title', null, 'Resume auto-following player position');
      btn.setAttribute('aria-label', labelText);
      btn.classList.toggle('is-visible', !autoPanPinned && Boolean(lastCanonicalPos));
    }
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
      cancelBtn.className = 'gh-lock-pill-cancel';
      cancelBtn.addEventListener('click', () => {
        lockedTarget = null;
        renderTargetLockPill();
        syncPopupNavigateButtonState();
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
    const cancelBtn = pill.querySelector('.gh-lock-pill-cancel');
    if (cancelBtn) {
      cancelBtn.textContent = tr('page.nav.lock_pill_cancel', null, 'Cancel');
      cancelBtn.title = tr('page.nav.unlock_title', null, 'Clear the locked navigation target');
    }
    if (!lockedTarget) {
      pill.classList.remove('is-visible');
      return;
    }
    const label = pill.querySelector('.gh-lock-label');
    if (label) label.textContent = `🎯 ${lockedTarget.name || lockedTarget.id}`;
    pill.classList.add('is-visible');
  }

  function refreshLocalizedUi() {
    if (document.getElementById('gh-recenter-btn')) {
      ensureRecenterBtn();
    }
    if (document.getElementById('gh-target-lock-pill')) {
      renderTargetLockPill();
    }
    syncPopupNavigateButtonState();
    if (activeWebRoute && activeWebRoute.active) {
      renderRouteBar(activeWebRoute);
    }
    const toastEl = document.getElementById('gh-mini-toast');
    if (toastEl && toastEl.classList.contains('is-visible') && lastToastSpec) {
      toastEl.textContent = tr(lastToastSpec.key, lastToastSpec.params, lastToastSpec.fallback);
    }
    if (typeof activeDiagModalRefresh === 'function' && document.getElementById('gh-diag-note-modal')) {
      activeDiagModalRefresh();
    }
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
    if (playerBoundMap && playerMoveHandler && typeof playerBoundMap.off === 'function') {
      try { playerBoundMap.off('move zoom viewreset zoomend', playerMoveHandler); } catch (e) {}
    }
    playerBoundMap = null;
    playerMoveHandler = null;
    if (playerDomEl && playerDomEl.parentNode) playerDomEl.parentNode.removeChild(playerDomEl);
    playerDomEl = null;
    if (tspBoundMap && tspMoveHandler && typeof tspBoundMap.off === 'function') {
      try { tspBoundMap.off('move zoom viewreset zoomend', tspMoveHandler); } catch (e) {}
    }
    tspBoundMap = null;
    tspMoveHandler = null;
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
    lastObservedMapId = activeAdapter.getCurrentMapId(activeCtx);
    bindDragUnpin(activeCtx);
    if (typeof activeAdapter.subscribe === 'function') {
      storeUnsub = activeAdapter.subscribe(activeCtx, ({ filterChanged, mapChanged }) => {
        if (mapChanged) {
          syncRouteOverlayForMapChange(activeCtx);
        }
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
        if (activeWebRoute && activeWebRoute.active) {
          renderWebRouteOverlay(ctx, activeWebRoute);
        } else if (activeTspRoute) {
          drawRouteOverlaySvg(ctx, activeTspRoute, 'tsp');
        }
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
      playerBoundMap = map;
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

  function clearRouteOverlaySvg() {
    if (tspBoundMap && tspMoveHandler && typeof tspBoundMap.off === 'function') {
      try { tspBoundMap.off('move zoom viewreset zoomend', tspMoveHandler); } catch (e) {}
    }
    tspBoundMap = null;
    tspMoveHandler = null;
    if (tspSvgEl && tspSvgEl.parentNode) tspSvgEl.parentNode.removeChild(tspSvgEl);
    tspSvgEl = null;
  }

  function drawRouteOverlaySvg(ctx, routeData, kind = 'tsp') {
    clearRouteOverlaySvg();
    if (!ctx || !ctx.map || !routeData) return;

    const mapId = activeAdapter.getCurrentMapId(ctx);
    if (routeData.map_id !== undefined && routeData.map_id !== null && Number(routeData.map_id) !== mapId) {
      return;
    }
    const legs = Array.isArray(routeData.legs) ? routeData.legs : [];
    const stops = Array.isArray(routeData.ordered_pins) ? routeData.ordered_pins : [];
    if (legs.length === 0 && stops.length === 0) return;

    const tf = CT.getTransform(activeAdapter.id, mapId);
    if (!tf) return;
    const pane = typeof ctx.map.getPane === 'function' ? ctx.map.getPane('overlayPane') : null;
    if (!pane) return;

    const svgNS = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(svgNS, 'svg');
    svg.id = 'gh-tsp-route-svg';
    svg.setAttribute('class', 'gh-tsp-route-svg');
    svg.setAttribute('data-route-kind', kind);
    svg.setAttribute('data-map-id', String(mapId));
    svg.style.position = 'absolute';
    svg.style.left = '0';
    svg.style.top = '0';
    svg.style.overflow = 'visible';
    svg.style.pointerEvents = 'none';
    pane.appendChild(svg);
    tspSvgEl = svg;

    const routeSpec = routeData;
    const redraw = () => {
      if (!tspSvgEl || !activeCtx || !activeCtx.map) return;
      const liveMapId = activeAdapter.getCurrentMapId(activeCtx);
      if (routeSpec.map_id !== undefined && routeSpec.map_id !== null && Number(routeSpec.map_id) !== liveMapId) {
        clearRouteOverlaySvg();
        return;
      }
      const curTf = CT.getTransform(activeAdapter.id, liveMapId);
      if (!curTf) return;

      let minX = Infinity;
      let minY = Infinity;
      let maxX = -Infinity;
      let maxY = -Infinity;
      const trackPt = (pt) => {
        if (!pt || !Number.isFinite(pt.x) || !Number.isFinite(pt.y)) return;
        if (pt.x < minX) minX = pt.x;
        if (pt.y < minY) minY = pt.y;
        if (pt.x > maxX) maxX = pt.x;
        if (pt.y > maxY) maxY = pt.y;
      };

      const legGeom = [];
      for (const leg of legs) {
        if (!leg) continue;
        const a = CT.canonicalToLeaflet(curTf, Number(leg.from_lat), Number(leg.from_lng));
        const b = CT.canonicalToLeaflet(curTf, Number(leg.to_lat), Number(leg.to_lng));
        if (!a || !b) continue;
        const p1 = activeCtx.map.latLngToLayerPoint(a);
        const p2 = activeCtx.map.latLngToLayerPoint(b);
        trackPt(p1);
        trackPt(p2);
        legGeom.push({ leg, a, b, p1, p2 });
      }

      const stopGeom = [];
      for (let i = 0; i < stops.length; i++) {
        const s = stops[i];
        if (!s || typeof s.lat !== 'number' || typeof s.lng !== 'number') continue;
        const ll = CT.canonicalToLeaflet(curTf, Number(s.lat), Number(s.lng));
        if (!ll) continue;
        const pt = activeCtx.map.latLngToLayerPoint(ll);
        trackPt(pt);
        stopGeom.push({ idx: i, stop: s, ll, pt });
      }

      while (tspSvgEl.firstChild) tspSvgEl.removeChild(tspSvgEl.firstChild);
      if (!Number.isFinite(minX) || !Number.isFinite(minY)) return;

      const pad = 52;
      const vbX = Math.floor(minX - pad);
      const vbY = Math.floor(minY - pad);
      const vbW = Math.max(2, Math.ceil((maxX - minX) + pad * 2));
      const vbH = Math.max(2, Math.ceil((maxY - minY) + pad * 2));
      tspSvgEl.setAttribute('width', String(vbW));
      tspSvgEl.setAttribute('height', String(vbH));
      tspSvgEl.setAttribute('viewBox', `${vbX} ${vbY} ${vbW} ${vbH}`);
      tspSvgEl.style.transform = `translate3d(${vbX}px, ${vbY}px, 0)`;

      const walkColor = typeof routeSpec.color === 'string' && routeSpec.color ? routeSpec.color : '#38bdf8';
      for (const { leg, a, b, p1, p2 } of legGeom) {
        const dx = p2.x - p1.x;
        const dy = p2.y - p1.y;
        const segLen = Math.hypot(dx, dy);
        if (segLen < 0.5) continue;

        const isTeleport = Boolean(leg.is_teleport);
        let pathD;
        if (isTeleport) {
          const midX = (p1.x + p2.x) * 0.5;
          const midY = (p1.y + p2.y) * 0.5;
          const bow = Math.min(36, segLen * 0.14);
          const cx = midX + (-dy / segLen) * bow;
          const cy = midY + (dx / segLen) * bow;
          pathD = `M ${p1.x.toFixed(1)} ${p1.y.toFixed(1)} Q ${cx.toFixed(1)} ${cy.toFixed(1)} ${p2.x.toFixed(1)} ${p2.y.toFixed(1)}`;
        } else {
          pathD = `M ${p1.x.toFixed(1)} ${p1.y.toFixed(1)} L ${p2.x.toFixed(1)} ${p2.y.toFixed(1)}`;
        }

        const halo = document.createElementNS(svgNS, 'path');
        halo.setAttribute('d', pathD);
        halo.setAttribute('fill', 'none');
        halo.setAttribute('stroke', 'rgba(15, 23, 42, 0.78)');
        halo.setAttribute('stroke-width', isTeleport ? '5.2' : '6.5');
        halo.setAttribute('stroke-linecap', 'round');
        if (isTeleport) halo.setAttribute('stroke-dasharray', '8 6');
        tspSvgEl.appendChild(halo);

        const line = document.createElementNS(svgNS, 'path');
        line.setAttribute('class', 'gh-route-leg');
        line.setAttribute('d', pathD);
        line.setAttribute('fill', 'none');
        line.setAttribute('stroke', isTeleport ? '#f59e0b' : walkColor);
        line.setAttribute('stroke-width', isTeleport ? '2.6' : '3.2');
        line.setAttribute('stroke-linecap', 'round');
        line.setAttribute('data-leaflet-from', `${a[0].toFixed(6)},${a[1].toFixed(6)}`);
        line.setAttribute('data-leaflet-to', `${b[0].toFixed(6)},${b[1].toFixed(6)}`);
        line.setAttribute('data-layer-p1', `${p1.x.toFixed(1)},${p1.y.toFixed(1)}`);
        line.setAttribute('data-layer-p2', `${p2.x.toFixed(1)},${p2.y.toFixed(1)}`);
        if (isTeleport) line.setAttribute('stroke-dasharray', '8 6');
        tspSvgEl.appendChild(line);
      }

      for (const { idx, stop, ll, pt } of stopGeom) {
        const stepNum = stop.step || (idx + 1);
        const isFirst = stepNum === 1;
        const g = document.createElementNS(svgNS, 'g');
        g.setAttribute('class', 'gh-tsp-stop');
        g.setAttribute('data-step', String(stepNum));
        g.setAttribute('data-leaflet-lat', ll[0].toFixed(6));
        g.setAttribute('data-leaflet-lng', ll[1].toFixed(6));
        g.setAttribute('data-layer-pt', `${pt.x.toFixed(1)},${pt.y.toFixed(1)}`);

        const circle = document.createElementNS(svgNS, 'circle');
        circle.setAttribute('cx', pt.x.toFixed(1));
        circle.setAttribute('cy', pt.y.toFixed(1));
        circle.setAttribute('r', isFirst ? '11.5' : '10');
        circle.setAttribute('fill', isFirst ? '#064e3b' : '#0f172a');
        circle.setAttribute('stroke', isFirst ? '#22c55e' : walkColor);
        circle.setAttribute('stroke-width', '2');
        g.appendChild(circle);

        const label = document.createElementNS(svgNS, 'text');
        label.setAttribute('class', 'gh-tsp-stop-label');
        label.setAttribute('x', pt.x.toFixed(1));
        label.setAttribute('y', pt.y.toFixed(1));
        label.setAttribute('text-anchor', 'middle');
        label.setAttribute('dominant-baseline', 'central');
        label.textContent = String(stepNum);
        g.appendChild(label);
        tspSvgEl.appendChild(g);
      }
    };
    redraw();
    tspMoveHandler = redraw;
    tspBoundMap = ctx.map;
    ctx.map.on('move zoom viewreset zoomend', tspMoveHandler);
  }

  function drawTspOverlay(ctx, routeData) {
    activeTspRoute = routeData || null;
    if (!activeTspRoute) {
      if (activeWebRoute && activeWebRoute.active) {
        renderWebRouteOverlay(ctx, activeWebRoute);
      } else {
        clearRouteOverlaySvg();
      }
      return;
    }
    drawRouteOverlaySvg(ctx, activeTspRoute, 'tsp');
  }

  function renderWebRouteOverlay(ctx, routeStatus) {
    try {
      if (!routeStatus || !routeStatus.active) {
        if (activeTspRoute) drawRouteOverlaySvg(ctx, activeTspRoute, 'tsp');
        else clearRouteOverlaySvg();
        return;
      }
      const stops = Array.isArray(routeStatus.active_stops) ? routeStatus.active_stops : [];
      const edges = Array.isArray(routeStatus.active_edges) ? routeStatus.active_edges : null;
      let legs = [];
      if (edges) {
        for (const edge of edges) {
          if (!Array.isArray(edge) || edge.length < 2) continue;
          const a = stops[edge[0]];
          const b = stops[edge[1]];
          if (!a || !b || typeof a.lat !== 'number' || typeof b.lat !== 'number') continue;
          legs.push({
            from_lat: a.lat,
            from_lng: a.lng,
            to_lat: b.lat,
            to_lng: b.lng,
            is_teleport: a.kind === 'teleport'
          });
        }
      } else if (stops.length >= 2) {
        for (let i = 0; i < stops.length - 1; i++) {
          const a = stops[i];
          const b = stops[i + 1];
          if (!a || !b || typeof a.lat !== 'number' || typeof b.lat !== 'number') continue;
          legs.push({
            from_lat: a.lat,
            from_lng: a.lng,
            to_lat: b.lat,
            to_lng: b.lng,
            is_teleport: a.kind === 'teleport'
          });
        }
      }
      if (legs.length === 0 && Array.isArray(routeStatus.legs)) {
        legs = routeStatus.legs;
      }
      const orderedPins = stops
        .filter((s) => s && !s.collected && typeof s.lat === 'number' && typeof s.lng === 'number')
        .map((s, idx) => ({ step: idx + 1, lat: s.lat, lng: s.lng }));
      if (legs.length === 0 && orderedPins.length === 0) {
        clearRouteOverlaySvg();
        return;
      }
      drawRouteOverlaySvg(
        ctx,
        {
          map_id: routeStatus.map_id || 2,
          color: routeStatus.section_color || '#38bdf8',
          legs,
          ordered_pins: orderedPins
        },
        'web-route'
      );
    } catch (e) {}
  }

  function syncRouteOverlayForMapChange(ctx) {
    if (!ctx || !activeAdapter.isAlive(ctx)) return;
    const curMapId = activeAdapter.getCurrentMapId(ctx);
    if (lastObservedMapId === curMapId) return;
    lastObservedMapId = curMapId;
    if (activeWebRoute && activeWebRoute.active) {
      renderWebRouteOverlay(ctx, activeWebRoute);
    } else if (activeTspRoute) {
      drawRouteOverlaySvg(ctx, activeTspRoute, 'tsp');
    }
    ensureCalibrated(ctx);
  }

  function createSvgIcon(name) {
    const svgNS = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(svgNS, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '2');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(svgNS, 'path');
    path.setAttribute('d', name === 'chevron-left' ? 'M15 18l-6-6 6-6' : 'M9 6l6 6-6 6');
    svg.appendChild(path);
    return svg;
  }

  function triggerClientFileDownload(filename, contentStr, mimeType) {
    const blob = new Blob([contentStr], { type: mimeType });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = filename;
    link.style.display = 'none';
    (document.body || document.documentElement).appendChild(link);
    link.click();
    setTimeout(() => {
      try { link.remove(); } catch (e) {}
      try { URL.revokeObjectURL(url); } catch (e) {}
    }, 4000);
  }

  function sendRouteSectionSelect(targetIdx) {
    extRequest('/route', {
      method: 'POST',
      body: { action: 'section', section_index: targetIdx }
    }).then((resp) => {
      const res = resp && resp.ok ? resp.data : null;
      if (res && res.active) handleRouteUpdatedEvent(res, { fit: false });
    }).catch(() => {});
  }

  async function handleRouteExportCommand() {
    try {
      const resp = await extRequest('/route', {
        method: 'POST',
        body: { action: 'export' }
      });
      const res = resp && resp.ok ? resp.data : null;
      if (res && res.doc) {
        const fname = String(res.filename || 'route.ghroute.json');
        triggerClientFileDownload(fname, JSON.stringify(res.doc, null, 2), 'application/json');
        return { ok: true, filename: fname };
      }
      return { ok: false, error: (res && res.error) || (resp && resp.error) || 'No active route' };
    } catch (err) {
      return { ok: false, error: (err && err.message) ? err.message : 'Export failed' };
    }
  }

  function focusActiveRouteOnMap(routeStatus) {
    try {
      if (!routeStatus || !routeStatus.active) return false;
      const ctx = getLiveContext();
      if (!ctx || !ctx.map) return false;
      const curMapId = activeAdapter.getCurrentMapId(ctx);
      if (Number(routeStatus.map_id || 2) !== curMapId) return false;
      const tf = CT.getTransform(activeAdapter.id, curMapId);
      if (!tf) return false;

      const pts = [];
      const pushCanon = (pt) => {
        if (!pt || !Number.isFinite(Number(pt.lat)) || !Number.isFinite(Number(pt.lng))) return;
        const ll = CT.canonicalToLeaflet(tf, Number(pt.lat), Number(pt.lng));
        if (ll && Number.isFinite(ll[0]) && Number.isFinite(ll[1])) pts.push(ll);
      };
      pushCanon(routeStatus.anchor_teleport);
      const stops = Array.isArray(routeStatus.active_stops) ? routeStatus.active_stops : [];
      for (const s of stops) pushCanon(s);
      if (pts.length === 0) return false;

      routeInspecting = true;
      autoPanPinned = false;
      ensureRecenterBtn();

      if (pts.length >= 2 && typeof ctx.map.fitBounds === 'function') {
        let minLat = Infinity;
        let maxLat = -Infinity;
        let minLng = Infinity;
        let maxLng = -Infinity;
        for (const [la, ln] of pts) {
          if (la < minLat) minLat = la;
          if (la > maxLat) maxLat = la;
          if (ln < minLng) minLng = ln;
          if (ln > maxLng) maxLng = ln;
        }
        if ((maxLat - minLat) > 1e-6 || (maxLng - minLng) > 1e-6) {
          const fitOpts = { padding: [90, 90], animate: true };
          if (typeof ctx.map.getMaxZoom === 'function') {
            fitOpts.maxZoom = ctx.map.getMaxZoom();
          }
          ctx.map.fitBounds([[minLat, minLng], [maxLat, maxLng]], fitOpts);
          renderRouteBar(activeWebRoute);
          return true;
        }
      }

      const targetZoom = activeAdapter.ratioToZoom(ctx, 0.55);
      ctx.map.setView(pts[0], targetZoom, { animate: true });
      renderRouteBar(activeWebRoute);
      return true;
    } catch (e) {}
    return false;
  }

  function renderRouteBar(routeStatus) {
    if (!routeStatus || !routeStatus.active) {
      if (routeBarEl) {
        try { routeBarEl.remove(); } catch (e) {}
        routeBarEl = null;
      }
      try { if (document.body) document.body.classList.remove('gh-has-route-bar'); } catch (e) {}
      return;
    }
    injectStyles();
    if (!document.body && !document.documentElement) return;
    if (document.body) document.body.classList.add('gh-has-route-bar');
    if (!routeBarEl || !routeBarEl.isConnected) {
      const existing = document.getElementById('gh-route-bar');
      if (existing) existing.remove();
      routeBarEl = document.createElement('div');
      routeBarEl.id = 'gh-route-bar';
      routeBarEl.setAttribute('role', 'toolbar');
      (document.body || document.documentElement).appendChild(routeBarEl);
    }
    while (routeBarEl.firstChild) {
      routeBarEl.removeChild(routeBarEl.firstChild);
    }

    const dot = document.createElement('span');
    dot.className = 'gh-route-dot';
    dot.style.background = routeStatus.section_color || '#38bdf8';

    const titleEl = document.createElement('span');
    titleEl.className = 'gh-route-title';
    titleEl.textContent = String(routeStatus.title || 'Route');
    titleEl.title = titleEl.textContent;

    const secIdx = Number(routeStatus.active_section || 0);
    const secCount = Math.max(1, Number(routeStatus.section_count || 1));
    const secDone = Number(routeStatus.section_done || 0);
    const secTotal = Number(routeStatus.section_total || 0);

    const iconBtn = (iconName, title) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = 'gh-route-btn is-icon';
      b.title = title;
      b.setAttribute('aria-label', title);
      b.appendChild(createSvgIcon(iconName));
      return b;
    };
    const group = (...children) => {
      const g = document.createElement('span');
      g.className = 'gh-route-group';
      g.append(...children);
      return g;
    };
    const sep = () => {
      const s = document.createElement('span');
      s.className = 'gh-route-sep';
      s.setAttribute('aria-hidden', 'true');
      return s;
    };

    const prevBtn = iconBtn('chevron-left', tr('page.route.prev_sec_title', null, 'Previous section'));
    prevBtn.disabled = secCount <= 1;
    prevBtn.addEventListener('click', () => sendRouteSectionSelect(secIdx - 1));

    const secLabel = document.createElement('span');
    secLabel.className = 'gh-route-sec';
    const secMain = document.createElement('b');
    secMain.textContent = `S${secIdx + 1}/${secCount}`;
    const secProgress = document.createElement('span');
    secProgress.textContent = `· ${secDone}/${secTotal}`;
    secLabel.append(secMain, secProgress);

    const nextBtn = iconBtn('chevron-right', tr('page.route.next_sec_title', null, 'Next section'));
    nextBtn.disabled = secCount <= 1;
    nextBtn.addEventListener('click', () => sendRouteSectionSelect(secIdx + 1));

    const viewRouteBtn = document.createElement('button');
    viewRouteBtn.type = 'button';
    viewRouteBtn.className = 'gh-route-btn is-primary';
    viewRouteBtn.textContent = tr('page.route.focus_btn', null, 'View Route');
    viewRouteBtn.addEventListener('click', () => {
      focusActiveRouteOnMap(activeWebRoute);
    });

    const followBtn = document.createElement('button');
    followBtn.type = 'button';
    followBtn.className = 'gh-route-btn' + ((routeInspecting || !autoPanPinned) ? ' is-follow' : '');
    followBtn.textContent = tr('page.route.reattach_btn', null, 'To Player');
    followBtn.addEventListener('click', () => {
      routeInspecting = false;
      autoPanPinned = true;
      ensureRecenterBtn();
      const ctx = getLiveContext();
      if (ctx && lastCanonicalPos) {
        const tf = CT.getTransform(activeAdapter.id, lastCanonicalPos.map_id);
        const ll = tf ? CT.canonicalToLeaflet(tf, lastCanonicalPos.lat, lastCanonicalPos.lng) : null;
        if (ll) ctx.map.panTo(ll, { animate: true, duration: 0.25 });
      }
      renderRouteBar(activeWebRoute);
    });

    const expBtn = document.createElement('button');
    expBtn.type = 'button';
    expBtn.className = 'gh-route-btn';
    expBtn.textContent = tr('page.route.export_btn', null, 'Export');
    expBtn.addEventListener('click', () => { handleRouteExportCommand(); });

    const resetBtn = document.createElement('button');
    resetBtn.type = 'button';
    resetBtn.className = 'gh-route-btn';
    resetBtn.textContent = tr('page.route.reset_btn', null, 'Reset');
    resetBtn.title = tr('page.route.reset_title', null, 'Restart route from section 1');
    resetBtn.addEventListener('click', () => {
      extRequest('/route', { method: 'POST', body: { action: 'reset' } })
        .then((resp) => {
          const res = resp && resp.ok ? resp.data : null;
          if (res && res.active) {
            handleRouteUpdatedEvent(res);
            showLocalizedToast('page.route.reset_done', null, 'Route progress reset to section 1!');
          }
        }).catch(() => {});
    });

    const stopBtn = document.createElement('button');
    stopBtn.type = 'button';
    stopBtn.className = 'gh-route-btn is-stop';
    stopBtn.textContent = tr('page.route.stop_btn', null, 'Stop');
    stopBtn.addEventListener('click', () => {
      extRequest('/route', { method: 'POST', body: { action: 'clear' } })
        .then((resp) => {
          const res = resp && resp.ok ? resp.data : null;
          handleRouteUpdatedEvent(res || { active: false });
          showLocalizedToast('page.route.cleared', null, 'Web route stopped.');
        }).catch(() => {});
    });

    routeBarEl.append(
      group(dot, titleEl),
      sep(),
      group(prevBtn, secLabel, nextBtn),
      sep(),
      group(viewRouteBtn, followBtn, expBtn, resetBtn, stopBtn)
    );
  }

  function handleRouteUpdatedEvent(statusData, opts = {}) {
    activeWebRoute = (statusData && statusData.active) ? statusData : null;
    if (!activeWebRoute) {
      routeInspecting = false;
      renderRouteBar(null);
      if (activeTspRoute) drawRouteOverlaySvg(getLiveContext(), activeTspRoute, 'tsp');
      else clearRouteOverlaySvg();
      return;
    }
    renderRouteBar(activeWebRoute);
    renderWebRouteOverlay(getLiveContext(), activeWebRoute);
    if (opts && opts.fit) {
      focusActiveRouteOnMap(activeWebRoute);
    }
  }

  function syncInitialRouteState() {
    extRequest('/route', { method: 'GET' })
      .then((resp) => {
        const res = resp && resp.ok ? resp.data : null;
        if (res && res.active) {
          handleRouteUpdatedEvent(res, { fit: false });
        }
      })
      .catch(() => {});
  }

  function showDiagnosticNoteModal(folderName, hasWorldCoords = true, reason = '', meta = {}) {
    try {
      const fname = String(folderName || 'diagnostics').trim() || 'diagnostics';
      const zname = (meta && meta.zipName) ? String(meta.zipName).trim() : `${fname}.zip`;
      const summary = (meta && meta.summary && typeof meta.summary === 'object') ? meta.summary : {};
      const now = Date.now();
      if (lastNoteModalFolder === fname && (now - lastNoteModalTs) < 2000) {
        return;
      }
      lastNoteModalFolder = fname;
      lastNoteModalTs = now;

      injectStyles();
      if (typeof activeDiagModalClose === 'function') {
        activeDiagModalClose();
      }
      const existing = document.getElementById('gh-diag-note-modal');
      if (existing) existing.remove();

      const overlay = document.createElement('div');
      overlay.id = 'gh-diag-note-modal';
      overlay.className = 'gh-root';
      overlay.setAttribute('data-folder', fname);

      const badgeBg = hasWorldCoords ? 'rgba(82, 196, 160, 0.16)' : 'rgba(224, 171, 88, 0.16)';
      const badgeBorder = hasWorldCoords ? 'rgba(82, 196, 160, 0.45)' : 'rgba(224, 171, 88, 0.45)';
      const badgeColor = hasWorldCoords ? '#52C4A0' : '#E0AB58';
      let noteSavedOnce = false;

      overlay.innerHTML = `
        <div style="background: rgba(26, 30, 40, 0.98); border: 1px solid rgba(211, 188, 142, 0.55); border-radius: 14px; padding: 20px 24px; max-width: 540px; width: 92%; color: #F2ECE0; box-shadow: 0 24px 60px rgba(0,0,0,0.85); display: flex; flex-direction: column; gap: 11px;">
          <div style="display: flex; justify-content: space-between; align-items: flex-start; gap: 10px; border-bottom: 1px solid rgba(211, 188, 142, 0.22); padding-bottom: 10px;">
            <div>
              <strong id="gh-note-title" style="color: #E4CF9F; font-size: 15px; font-weight: 700;"></strong>
              <div id="gh-note-folder" style="font-family: 'JetBrains Mono', Consolas, monospace; font-size: 11.5px; color: #7EE8FF; margin-top: 2px;"></div>
            </div>
            <span id="gh-note-badge" style="background: ${badgeBg}; border: 1px solid ${badgeBorder}; color: ${badgeColor}; font-size: 11px; font-weight: 600; padding: 3px 8px; border-radius: 999px; white-space: nowrap;"></span>
          </div>
          <div id="gh-note-reason" style="font-size: 11.5px; color: #C2BAAB; display: none;"></div>
          <div id="gh-note-discord-hint" style="font-size: 11.5px; line-height: 1.45; color: #A7F3D0; background: rgba(82, 196, 160, 0.1); border: 1px solid rgba(82, 196, 160, 0.32); border-radius: 9px; padding: 8px 10px;"></div>
          <div id="gh-note-tags" style="display: flex; flex-wrap: wrap; gap: 6px;"></div>
          <textarea id="gh-diag-note-input" rows="3" style="width: 100%; background: #151923; border: 1px solid rgba(211, 188, 142, 0.4); border-radius: 10px; padding: 10px 12px; color: #F2ECE0; font-size: 12.5px; line-height: 1.45; resize: vertical; outline: none; font-family: inherit;"></textarea>
          <div style="display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center; gap: 8px;">
            <div style="display: flex; gap: 6px; flex-wrap: wrap;">
              <button id="gh-note-open-folder" type="button" style="background: rgba(126, 232, 255, 0.12); border: 1px solid rgba(126, 232, 255, 0.35); color: #7EE8FF; border-radius: 8px; padding: 6px 11px; font-size: 11.5px; font-weight: 600; cursor: pointer;"></button>
              <button id="gh-note-copy-discord" type="button" style="background: rgba(211, 188, 142, 0.14); border: 1px solid rgba(211, 188, 142, 0.38); color: #E4CF9F; border-radius: 8px; padding: 6px 11px; font-size: 11.5px; font-weight: 600; cursor: pointer;"></button>
            </div>
            <div style="display: flex; gap: 8px; flex-wrap: wrap;">
              <button id="gh-note-skip" type="button" style="background: #2B3244; border: 1px solid rgba(211, 188, 142, 0.25); color: #C2BAAB; border-radius: 8px; padding: 6px 12px; font-size: 12px; cursor: pointer;"></button>
              <button id="gh-note-save-only" type="button" style="background: rgba(126, 232, 255, 0.18); border: 1px solid rgba(126, 232, 255, 0.5); color: #F2ECE0; font-weight: 600; border-radius: 8px; padding: 6px 13px; font-size: 12px; cursor: pointer;"></button>
              <button id="gh-note-save" type="button" style="background: #D3BC8E; border: none; color: #1A1E28; font-weight: 700; border-radius: 8px; padding: 6px 14px; font-size: 12px; cursor: pointer;"></button>
            </div>
          </div>
          <div style="display: flex; justify-content: space-between; align-items: center; gap: 10px;">
            <span id="gh-note-shortcut" style="font-size: 11px; color: #B0A89A;"></span>
            <span id="gh-note-status" style="font-size: 11.5px; color: #52C4A0; text-align: right;"></span>
          </div>
          <div id="gh-note-error" style="color: #EE827D; font-size: 11.5px; min-height: 14px;"></div>
        </div>
      `;

      (document.body || document.documentElement).appendChild(overlay);
      const reasonEl = overlay.querySelector('#gh-note-reason');
      if (reason && reasonEl) {
        reasonEl.textContent = String(reason);
        reasonEl.style.display = 'block';
      }
      const textarea = overlay.querySelector('#gh-diag-note-input');
      const tagsWrap = overlay.querySelector('#gh-note-tags');
      const discordHintEl = overlay.querySelector('#gh-note-discord-hint');
      const openFolderBtn = overlay.querySelector('#gh-note-open-folder');
      const copyDiscordBtn = overlay.querySelector('#gh-note-copy-discord');
      const skipBtn = overlay.querySelector('#gh-note-skip');
      const saveOnlyBtn = overlay.querySelector('#gh-note-save-only');
      const saveBtn = overlay.querySelector('#gh-note-save');
      const statusEl = overlay.querySelector('#gh-note-status');
      const errEl = overlay.querySelector('#gh-note-error');

      const formatModalDiscordTemplate = () => {
        const appVer = summary.app_version || SCRIPT_VERSION;
        const osStr = summary.os_label || 'Windows';
        const resStr = summary.resolution_label || 'N/A';
        const dpiStr = summary.dpi_scale_pct ? `${summary.dpi_scale_pct}%` : 'N/A';
        const flags = [];
        if (summary.game_found === false) flags.push('no-game-window');
        if (summary.hdr_clipped) flags.push('HDR-clipped');
        if (summary.uipi_blocked) flags.push('UIPI-admin-game');
        if (summary.browser_captured) flags.push('map-viewport-ok');
        const envLine = `${osStr} | ${resStr} @ ${dpiStr}${flags.length ? ` (${flags.join(', ')})` : ''}`;
        const noteText = textarea.value.trim() || '-';
        return [
          `**[Beta Bug]** \`${zname}\``,
          `- **App:** \`v${appVer}\` (\`${SCRIPT_VERSION}\`)`,
          `- **Env:** ${envLine}`,
          `- **Details:** ${noteText}`,
          `_${tr('page.diag.modal.discord_footer', { zip: zname }, `Attach ${zname} (UID & personal data masked)`)}_`
        ].join('\n');
      };

      const copyTextCompat = async (text) => {
        try {
          if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
            await navigator.clipboard.writeText(text);
            return true;
          }
        } catch (e) {}
        try {
          const ta = document.createElement('textarea');
          ta.value = text;
          ta.style.cssText = 'position:fixed;opacity:0;left:-9999px;';
          document.body.appendChild(ta);
          ta.select();
          const ok = document.execCommand('copy');
          ta.remove();
          return Boolean(ok);
        } catch (e) {
          return false;
        }
      };

      const renderModalLabels = () => {
        const badgeEl = overlay.querySelector('#gh-note-badge');
        const titleEl = overlay.querySelector('#gh-note-title');
        const folderEl = overlay.querySelector('#gh-note-folder');
        const shortcutEl = overlay.querySelector('#gh-note-shortcut');
        if (titleEl) titleEl.textContent = tr('page.diag.modal.title', null, 'Diagnostic Capture Saved');
        if (badgeEl) {
          badgeEl.textContent = hasWorldCoords
            ? tr('page.diag.modal.badge_ok', null, 'World Coords Locked')
            : tr('page.diag.modal.badge_warn', null, 'Low Landmarks / Off-Screen');
        }
        if (folderEl) folderEl.textContent = tr('page.diag.modal.bundle_prefix', { folder: fname }, `Bundle: ${fname}`);
        if (discordHintEl) discordHintEl.textContent = tr('page.diag.modal.discord_hint', null, 'Screenshots & logs are saved to the folder.');
        if (shortcutEl) shortcutEl.textContent = tr('page.diag.modal.shortcut_hint', null, 'Ctrl + Enter: save note only • Esc: close');
        if (openFolderBtn) openFolderBtn.textContent = tr('page.diag.modal.open_folder', null, 'Open Folder');
        if (copyDiscordBtn) copyDiscordBtn.textContent = tr('page.diag.modal.copy_discord', null, 'Copy Discord Form');
        textarea.placeholder = tr('page.diag.modal.placeholder', null, 'Describe the issue here...');
        skipBtn.textContent = noteSavedOnce
          ? tr('page.diag.modal.close', null, 'Close')
          : tr('page.diag.modal.skip', null, 'Close (saved)');
        if (saveOnlyBtn && !saveOnlyBtn.disabled) {
          saveOnlyBtn.textContent = tr('page.diag.modal.save_only', null, 'Save Note');
        }
        if (!saveBtn.disabled) {
          saveBtn.textContent = tr('page.diag.modal.save', null, 'Save & Open .zip');
        }
        const tags = [
          tr('page.diag.tag.wrong_pos', null, '[Wrong Position]'),
          tr('page.diag.tag.wrong_floor', null, '[Wrong Floor]'),
          tr('page.diag.tag.no_cave_img', null, '[Cave Layer Missing]'),
          tr('page.diag.tag.panning_no_arrow', null, '[Dragging Map / No Arrow]'),
          tr('page.diag.tag.other', null, '[Other]')
        ];
        while (tagsWrap.firstChild) tagsWrap.removeChild(tagsWrap.firstChild);
        for (const tag of tags) {
          const btn = document.createElement('button');
          btn.type = 'button';
          btn.textContent = tag;
          btn.style.cssText = 'background: rgba(126, 232, 255, 0.12); border: 1px solid rgba(126, 232, 255, 0.35); color: #7EE8FF; border-radius: 999px; padding: 3px 9px; font-size: 11px; cursor: pointer;';
          btn.addEventListener('click', (e) => {
            e.stopPropagation();
            const cur = textarea.value;
            if (!cur.includes(tag)) {
              const sep = cur.trim().length > 0 && !cur.endsWith(' ') && !cur.endsWith('\n') ? ' ' : '';
              textarea.value = cur + sep + tag + ' ';
            }
            textarea.focus();
          });
          tagsWrap.appendChild(btn);
        }
      };

      activeDiagModalRefresh = renderModalLabels;
      renderModalLabels();

      const closeNoteModal = () => {
        document.removeEventListener('keydown', onModalKeyDown, true);
        if (activeDiagModalRefresh === renderModalLabels) activeDiagModalRefresh = null;
        if (activeDiagModalClose === closeNoteModal) activeDiagModalClose = null;
        overlay.remove();
      };
      activeDiagModalClose = closeNoteModal;

      if (openFolderBtn) {
        openFolderBtn.addEventListener('click', async () => {
          errEl.textContent = '';
          await copyTextCompat(formatModalDiscordTemplate());
          const resp = await extRequest('/open_diagnostics', {
            method: 'POST',
            body: { folder_name: fname }
          });
          const ok = Boolean(resp && resp.ok && (!resp.data || resp.data.ok !== false));
          if (ok) {
            statusEl.textContent = tr('page.diag.modal.folder_opened', null, 'Opened Diagnostics folder in Explorer.');
          } else {
            errEl.textContent = tr('page.diag.modal.folder_error', null, 'Could not open folder.');
          }
        });
      }

      if (copyDiscordBtn) {
        copyDiscordBtn.addEventListener('click', async () => {
          errEl.textContent = '';
          const ok = await copyTextCompat(formatModalDiscordTemplate());
          if (ok) {
            const msg = tr('page.diag.modal.copied_discord', null, 'Discord template copied.');
            statusEl.textContent = msg;
            showLocalizedToast('page.diag.modal.copied_discord', null, msg);
          }
        });
      }

      const submitNoteOnly = async () => {
        const noteText = textarea.value.trim();
        if (saveOnlyBtn) {
          saveOnlyBtn.disabled = true;
          saveOnlyBtn.textContent = tr('page.diag.modal.saving', null, 'Saving...');
        }
        saveBtn.disabled = true;
        errEl.textContent = '';
        try {
          const resp = await extRequest('/save_diagnostic_note', {
            method: 'POST',
            body: { folder_name: fname, note: noteText, create_zip: false }
          });
          if (saveOnlyBtn) {
            saveOnlyBtn.disabled = false;
            saveOnlyBtn.textContent = tr('page.diag.modal.save_only', null, 'Save Note');
          }
          saveBtn.disabled = false;
          const ok = Boolean(resp && resp.ok && (!resp.data || resp.data.ok !== false));
          if (ok) {
            noteSavedOnce = true;
            statusEl.textContent = tr('page.diag.modal.saved_only_inline', { folder: fname }, `Saved note to ${fname}/05_user_note.txt.`);
            showLocalizedToast('page.diag.modal.saved_toast', { folder: fname }, `Saved diagnostic note to [${fname}]/05_user_note.txt`);
            closeNoteModal();
          } else {
            errEl.textContent = (resp && resp.data && resp.data.error) || tr('page.diag.modal.save_error', null, 'Could not save note.');
          }
        } catch (err) {
          if (saveOnlyBtn) {
            saveOnlyBtn.disabled = false;
            saveOnlyBtn.textContent = tr('page.diag.modal.save_only', null, 'Save Note');
          }
          saveBtn.disabled = false;
          errEl.textContent = tr('page.diag.modal.offline_error', null, 'Cannot connect to desktop server.');
        }
      };

      const submitNoteAndZip = async () => {
        const noteText = textarea.value.trim();
        saveBtn.disabled = true;
        saveBtn.textContent = tr('page.diag.modal.saving', null, 'Saving...');
        if (saveOnlyBtn) saveOnlyBtn.disabled = true;
        errEl.textContent = '';
        try {
          const resp = await extRequest('/save_diagnostic_note', {
            method: 'POST',
            body: { folder_name: fname, note: noteText, create_zip: true }
          });
          saveBtn.disabled = false;
          saveBtn.textContent = tr('page.diag.modal.save', null, 'Save & Open .zip');
          if (saveOnlyBtn) saveOnlyBtn.disabled = false;
          const ok = Boolean(resp && resp.ok && (!resp.data || resp.data.ok !== false));
          if (ok) {
            noteSavedOnce = true;
            skipBtn.textContent = tr('page.diag.modal.close', null, 'Close');
            await copyTextCompat(formatModalDiscordTemplate());
            try {
              await extRequest('/open_diagnostics', {
                method: 'POST',
                body: { folder_name: fname, ensure_zip: true }
              });
            } catch (openErr) {}
            statusEl.textContent = tr('page.diag.modal.saved_inline', { zip: zname }, `Packaged ${zname}!`);
            showLocalizedToast('page.diag.modal.saved_toast', { folder: fname }, `Saved diagnostic note to [${fname}]/05_user_note.txt`);
          } else {
            errEl.textContent = (resp && resp.data && resp.data.error) || tr('page.diag.modal.save_error', null, 'Could not save note.');
          }
        } catch (err) {
          saveBtn.disabled = false;
          saveBtn.textContent = tr('page.diag.modal.save', null, 'Save & Open .zip');
          if (saveOnlyBtn) saveOnlyBtn.disabled = false;
          errEl.textContent = tr('page.diag.modal.offline_error', null, 'Cannot connect to desktop server.');
        }
      };

      const onModalKeyDown = (e) => {
        if (!document.body.contains(overlay)) {
          document.removeEventListener('keydown', onModalKeyDown, true);
          return;
        }
        if (e.key === 'Escape') {
          e.preventDefault();
          e.stopPropagation();
          closeNoteModal();
        } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
          e.preventDefault();
          e.stopPropagation();
          submitNoteOnly();
        }
      };

      document.addEventListener('keydown', onModalKeyDown, true);
      skipBtn.addEventListener('click', closeNoteModal);
      if (saveOnlyBtn) saveOnlyBtn.addEventListener('click', submitNoteOnly);
      saveBtn.addEventListener('click', submitNoteAndZip);
      overlay.addEventListener('click', (e) => { if (e.target === overlay) closeNoteModal(); });
      setTimeout(() => textarea.focus(), 40);
    } catch (err) {}
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

    syncRouteOverlayForMapChange(ctx);
    const curMapId = activeAdapter.getCurrentMapId(ctx);
    if (curMapId !== targetMapId) {
      // Never force full-page reload on a hidden secondary tab when another map tab is driving.
      const now = Date.now();
      const canSwitch = !document.hidden && (isManualSync || isActiveOwnerTab) && (now - lastMapSwitchTs > 6000);
      if (canSwitch) {
        lastMapSwitchTs = now;
        activeAdapter.switchMap(ctx, targetMapId);
        syncRouteOverlayForMapChange(ctx);
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
      const fwdPt = CT.forward(tf, lng, lat);
      activeAdapter.syncUnderground(ctx, data.is_underground, {
        providerPoint: fwdPt ? { x: fwdPt[0], y: fwdPt[1] } : null
      });
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

  function handleLanSetFloor(data) {
    if (!isEventForThisProviderTab(data)) return;
    const ctx = getLiveContext();
    if (!ctx || !ctx.map) return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    if (data && data.map_id !== undefined && data.map_id !== null && Number(data.map_id) !== mapId) return;

    const fidRaw = (data && data.floor_id !== undefined && data.floor_id !== null)
      ? String(data.floor_id).trim()
      : '';
    const isSurface = !fidRaw || fidRaw === '0' || fidRaw.toLowerCase() === 'surface';

    let providerPoint = null;
    const lat = data && data.lat !== undefined && data.lat !== null ? Number(data.lat) : NaN;
    const lng = data && data.lng !== undefined && data.lng !== null ? Number(data.lng) : NaN;
    if (Number.isFinite(lat) && Number.isFinite(lng)) {
      const tf = CT.getTransform(activeAdapter.id, mapId);
      const fwd = tf ? CT.forward(tf, lng, lat) : null;
      if (fwd) providerPoint = { x: fwd[0], y: fwd[1] };
    }

    const prefix = `${activeAdapter.id}:`;
    const fullPinId = (data && data.pin_id !== undefined && data.pin_id !== null) ? String(data.pin_id).trim() : '';
    const rawPinId = fullPinId.startsWith(prefix) ? fullPinId.slice(prefix.length) : '';

    if (typeof activeAdapter.setFloor === 'function') {
      activeAdapter.setFloor(ctx, {
        floorId: fidRaw,
        floorName: (data && data.floor_name) ? String(data.floor_name) : '',
        groupId: data && data.group_id !== undefined ? data.group_id : null,
        rawPinId,
        isSurface,
        providerPoint
      });
    } else if (typeof activeAdapter.syncUnderground === 'function') {
      activeAdapter.syncUnderground(ctx, !isSurface, { providerPoint });
    }
  }

  function handleLanFocusPin(data) {
    if (!data || typeof data !== 'object') return;
    if (!isEventForThisProviderTab(data)) return;

    const fullId = (data.pin_id !== undefined && data.pin_id !== null) ? String(data.pin_id).trim() : '';
    const prefix = `${activeAdapter.id}:`;
    if (fullId !== '' && !fullId.startsWith(prefix)) return;
    const rawId = fullId ? fullId.slice(prefix.length) : '';

    const ctx = getLiveContext();
    if (!ctx || !ctx.map) return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    if (data.map_id !== undefined && data.map_id !== null && Number(data.map_id) !== mapId) return;

    const fidStr = (data.floor_id !== undefined && data.floor_id !== null) ? String(data.floor_id).trim() : '';
    const hasFloor = Boolean(fidStr && fidStr !== '0' && fidStr.toLowerCase() !== 'surface');
    if (hasFloor) {
      handleLanSetFloor(data);
    }

    const tf = CT.getTransform(activeAdapter.id, mapId);
    const lat = data.lat !== undefined && data.lat !== null ? Number(data.lat) : NaN;
    const lng = data.lng !== undefined && data.lng !== null ? Number(data.lng) : NaN;
    const hasCoords = Number.isFinite(lat) && Number.isFinite(lng);
    const ll = (hasCoords && tf) ? CT.canonicalToLeaflet(tf, lat, lng) : null;
    if (ll) {
      autoPanPinned = false;
      ensureRecenterBtn();
      const targetZoom = Math.max(ctx.map.getZoom(), activeAdapter.ratioToZoom(ctx, 0.7));
      ctx.map.setView(ll, targetZoom, { animate: false });
    }

    if (rawId && typeof activeAdapter.focusPin === 'function') {
      setTimeout(() => {
        const curCtx = getLiveContext() || ctx;
        if (!curCtx || activeAdapter.getCurrentMapId(curCtx) !== mapId) return;
        activeAdapter.focusPin(curCtx, rawId, ll, data);
      }, hasFloor ? 80 : 25);
    }
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
      case 'LANGUAGE_UPDATED': {
        if (data.language && window.GHI18n && typeof window.GHI18n.setLang === 'function') {
          window.GHI18n.setLang(data.language);
        }
        refreshLocalizedUi();
        break;
      }
      case 'DIAGNOSTIC_CAPTURED': {
        const folderName = data.folder_name || 'diagnostics';
        const zipName = data.zip_name || `${folderName}.zip`;
        if (data.has_world_coords) {
          showLocalizedToast(
            'page.diag.captured_ok',
            { folder: zipName },
            `Diagnostic captured: [${zipName}]! Enter a bug note in the dialog.`
          );
        } else {
          showLocalizedToast(
            'page.diag.captured_warn',
            { folder: zipName, reason: data.reason || '' },
            `Diagnostic captured: [${zipName}]! ${data.reason || ''}`
          );
        }
        showDiagnosticNoteModal(folderName, Boolean(data.has_world_coords), data.reason || '', {
          zipName,
          zipSizeBytes: data.zip_size_bytes || 0,
          summary: (data.summary && typeof data.summary === 'object') ? data.summary : {}
        });
        break;
      }
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
      case 'LAN_FOCUS_PIN':
        handleLanFocusPin(data);
        break;
      case 'LAN_SET_FLOOR':
        handleLanSetFloor(data);
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
        syncPopupNavigateButtonState();
        break;
      }
      case 'ROUTE_UPDATED':
        handleRouteUpdatedEvent(data);
        break;
      case 'GH_CMD_ROUTE_IMPORT':
        extRequest('/route', { method: 'GET' }).then((resp) => {
          const res = resp && resp.ok ? resp.data : null;
          if (res && res.active) handleRouteUpdatedEvent(res, { fit: Boolean(data.fit) });
        }).catch(() => {});
        break;
      case 'GH_CMD_ROUTE_EXPORT':
        handleRouteExportCommand();
        break;
      case 'GH_CMD_ROUTE_CLEAR':
        handleRouteUpdatedEvent({ active: false });
        break;
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
          showLocalizedToast('page.tsp.no_pins', null, 'No visible pins in view');
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

  function isPinCurrentlyLocked(pinId) {
    if (!lockedTarget || lockedTarget.id === undefined || lockedTarget.id === null) return false;
    if (pinId === undefined || pinId === null) return false;
    return String(lockedTarget.id) === String(pinId);
  }

  function syncPopupNavigateButtonState(btn = null) {
    const targetBtn = btn || document.getElementById('gh-popup-navigate-btn');
    if (!targetBtn) return;
    const locked = isPinCurrentlyLocked(targetBtn.dataset.ghPinId);
    if (targetBtn.classList.contains('is-locked') !== locked) {
      targetBtn.classList.toggle('is-locked', locked);
    }
    const nextText = locked
      ? `✕ ${tr('page.nav.unlock_btn', null, 'Cancel Nav')}`
      : `🧭 ${tr('page.nav.navigate_btn', null, 'Navigate')}`;
    if (targetBtn.textContent !== nextText) {
      targetBtn.textContent = nextText;
    }
    const nextTitle = tr(locked ? 'page.nav.unlock_title' : 'page.nav.navigate_title', null, locked ? 'Clear target lock' : 'Navigate to this pin');
    if (targetBtn.title !== nextTitle) {
      targetBtn.title = nextTitle;
    }
  }

  function syncPopupNavigateButton() {
    const ctx = getLiveContext();
    if (!ctx || typeof activeAdapter.getOpenPopupPin !== 'function') return;
    const mapId = activeAdapter.getCurrentMapId(ctx);
    const tf = CT.getTransform(activeAdapter.id, mapId);
    if (!tf) return;
    const info = activeAdapter.getOpenPopupPin(ctx, tf);
    if (!info || !info.anchorEl || !info.pin) return;
    const existing = info.container.querySelector('#gh-popup-navigate-btn');
    if (existing) {
      existing.dataset.ghPinId = String(info.pin.id);
      existing._ghPin = info.pin;
      syncPopupNavigateButtonState(existing);
      return;
    }

    const btn = document.createElement('button');
    btn.id = 'gh-popup-navigate-btn';
    btn.type = 'button';
    btn.dataset.ghPinId = String(info.pin.id);
    btn._ghPin = info.pin;
    syncPopupNavigateButtonState(btn);
    btn.addEventListener('click', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      const curPin = btn._ghPin || info.pin;
      if (isPinCurrentlyLocked(btn.dataset.ghPinId)) {
        lockedTarget = null;
        renderTargetLockPill();
        syncPopupNavigateButtonState(btn);
        extRequest('/nav_command', {
          method: 'POST',
          body: { action: 'unlock_target', tab_id: GH_TAB_ID, provider_id: activeAdapter.id, user_initiated: true }
        });
        return;
      }
      lockedTarget = curPin;
      renderTargetLockPill();
      syncPopupNavigateButtonState(btn);
      extRequest('/nav_command', {
        method: 'POST',
        body: {
          action: 'lock_target',
          auto_switch: true,
          tab_id: GH_TAB_ID,
          provider_id: activeAdapter.id,
          user_initiated: true,
          pin: curPin
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
    syncRouteOverlayForMapChange(ctx);
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
    if (
      e.data.kind === 'GH_CMD_PLAN_TSP' ||
      e.data.kind === 'GH_CMD_CLEAR_TSP' ||
      e.data.kind === 'GH_CMD_ROUTE_EXPORT' ||
      e.data.kind === 'GH_CMD_ROUTE_IMPORT' ||
      e.data.kind === 'GH_CMD_ROUTE_CLEAR' ||
      e.data.kind === 'GH_CMD_REFRESH_PINS'
    ) {
      handleServerEvent({ ...e.data, type: e.data.kind });
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
      syncInitialRouteState();
    }
  }

  // =========================================================================
  // 7. Boot & Teardown
  // =========================================================================

  injectStyles();
  window.addEventListener('message', onWindowMessage);

  if (window.GHI18n && typeof window.GHI18n.onChange === 'function') {
    i18nUnsub = window.GHI18n.onChange(() => refreshLocalizedUi());
  }

  const onVis = () => {
    const nowVisible = !document.hidden;
    const focused = typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false;
    reportTabVisibility(nowVisible && focused, false);
  };
  const onFocus = () => {
    reportTabVisibility(true, false);
    const ctx = getLiveContext();
    if (ctx) {
      syncRouteOverlayForMapChange(ctx);
      pushNearbyPins(ctx, { force: true, userInitiated: true });
    }
  };
  const onHide = () => reportTabVisibility(false, true);
  const onRouteNavChange = () => {
    const ctx = getLiveContext();
    if (ctx) syncRouteOverlayForMapChange(ctx);
  };
  const onClick = () => {
    setTimeout(() => {
      syncPopupNavigateButton();
      const ctx = getLiveContext();
      if (ctx) {
        syncRouteOverlayForMapChange(ctx);
        pushNearbyPins(ctx, { force: false, userInitiated: true });
      }
    }, 180);
  };

  document.addEventListener('visibilitychange', onVis);
  window.addEventListener('focus', onFocus);
  window.addEventListener('pagehide', onHide);
  window.addEventListener('hashchange', onRouteNavChange);
  window.addEventListener('popstate', onRouteNavChange);
  document.addEventListener('click', onClick, true);

  const popupObserver = new MutationObserver(() => syncPopupNavigateButton());
  if (document.body) {
    popupObserver.observe(document.body, { childList: true, subtree: true });
  }

  const visTimer = setInterval(() => reportTabVisibility(false, false), 5000);
  const stateTimer = setInterval(() => {
    const ctx = getLiveContext();
    if (ctx) {
      syncRouteOverlayForMapChange(ctx);
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
      if (
        resp.data.language &&
        window.GHI18n &&
        typeof window.GHI18n.setLang === 'function' &&
        (typeof window.GHI18n.hasExplicitPreference !== 'function' || !window.GHI18n.hasExplicitPreference())
      ) {
        window.GHI18n.setLang(resp.data.language, { explicit: false });
      }
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
    syncInitialRouteState();
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
    getActiveRoute: () => activeWebRoute,
    syncRouteOverlay: () => syncRouteOverlayForMapChange(getLiveContext()),
    getLanguage: () => (window.GHI18n ? window.GHI18n.getLang() : 'en'),
    setLanguage: (lang) => {
      if (window.GHI18n && typeof window.GHI18n.setLang === 'function') {
        return window.GHI18n.setLang(lang);
      }
      return Promise.resolve(lang);
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
    window.removeEventListener('hashchange', onRouteNavChange);
    window.removeEventListener('popstate', onRouteNavChange);
    document.removeEventListener('click', onClick, true);
    if (typeof storeUnsub === 'function') {
      try { storeUnsub(); } catch (e) {}
    }
    if (typeof i18nUnsub === 'function') {
      try { i18nUnsub(); } catch (e) {}
      i18nUnsub = null;
    }
    if (typeof activeDiagModalClose === 'function') {
      try { activeDiagModalClose(); } catch (e) {}
    }
    if (routeBarEl && routeBarEl.parentNode) {
      routeBarEl.parentNode.removeChild(routeBarEl);
    }
    routeBarEl = null;
    try { if (document.body) document.body.classList.remove('gh-has-route-bar'); } catch (e) {}
    cleanupMapOverlays();
  };
})();
