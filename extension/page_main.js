(function () {
    'use strict';

    window.__gh_runtime_mode = 'extension';

    if (!window.location.hostname.includes('hoyolab.com') || !window.location.pathname.includes('/interactive-map/')) {
        return;
    }

    const SCRIPT_VERSION = 'v5.7.0';
    const SCRIPT_BUILD = '5.7.0-r7';

    function bootGenshinHelper() {
    if (window.__gh_booted_build === SCRIPT_BUILD) return;
    if (typeof window.__gh_teardown === 'function') {
        try { window.__gh_teardown(); } catch (e) {}
    }
    _cleanupOrphanPlayerMarkers(null);
    window.__gh_booted_version = SCRIPT_BUILD;
    window.__gh_booted_build = SCRIPT_BUILD;

    const pendingExtRequests = new Map();
    let extReqCounter = 0;
    let initialStatusSynced = false;
    let warnedVersionMismatch = false;

    function extRequest(path, options = {}) {
        const method = (options.method || 'GET').toUpperCase();
        const body = options.body !== undefined ? options.body : undefined;
        const defaultTimeout = path.startsWith('/trigger_diagnostic') ? 25000 : 10000;
        const timeoutMs = typeof options.timeoutMs === 'number' ? options.timeoutMs : defaultTimeout;
        const reqId = `gh_ext_${Date.now()}_${++extReqCounter}`;

        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                pendingExtRequests.delete(reqId);
                reject(new Error(`Extension bridge timeout (${path})`));
            }, timeoutMs);

            pendingExtRequests.set(reqId, { resolve, reject, timer });

            window.postMessage({
                __gh_ext: true,
                dir: 'to-bg',
                kind: 'API_REQUEST',
                reqId,
                path,
                method,
                body
            }, window.location.origin);
        });
    }

    const apiRequest = extRequest;

    let autoPanEnabled = true;
    let autoSwitchLayerEnabled = true;
    let autoSyncMapMEnabled = true;
    // Set per SYNC_* event by the app: true = the in-game HUD already gives toast + chime
    // (user cannot see this tab), so the page stays quiet to avoid double feedback.
    let nativeFeedbackActive = false;
    let compactUiEnabled = false;
    let pendingVisibleRefresh = false;
    let lastSyncedView = null;
    let isConnected = false;
    let engineActive = true;
    let miniToastTimer = null;
    let currentHotkeySyncDisplay = 'Alt+F11';
    let currentHotkeyDebugDisplay = 'Alt+F12';
    let currentHotkeySwitchDisplay = 'Alt + Q';
    let currentHotkeySyncRaw = 'alt+f11';
    let currentHotkeyDebugRaw = 'alt+f12';
    let currentHotkeySwitchRaw = 'alt+q';
    let playerMarkerLayer = null;
    let playerMarkerMapRef = null;
    let playerMarkerWrapEl = null;
    let playerDomMarkerEl = null;
    let playerDomWrapEl = null;
    let playerDomMapRef = null;
    let playerLastState = null;
    let playerUnwrappedDir = null;
    let playerUnwrappedRot = null;
    let dragBoundContainerEl = null;
    let dragListenersCleanup = null;
    let layerTimers = [];
    let minimapAuxTimers = [];
    let lastMinimapAreaSwitchId = null;
    let lastMinimapAreaSwitchTs = 0;
    let lastMinimapLayerSig = null;
    let lastMinimapLayerSyncTs = 0;
    let lastNoteModalFolder = '';
    let lastNoteModalTs = 0;
    let lastEventSeq = -1;
    let lastNearbyPushTs = 0;
    let lastNearbyPushLat = null;
    let lastNearbyPushLng = null;
    let lastNearbyPushMapId = null;
    let lastNearbyPushFloorId = null;
    let activeTspRoute = null;
    let tspSvgEl = null;
    let tspMapRef = null;
    let tspRedrawRaf = null;
    let activeWebRoute = null;
    let routeInspecting = false;
    let ghRouteBarEl = null;
    let activeLockedPin = null;
    let ghLockPillEl = null;
    const hoyolabRouteMetaCache = new Map();
    const hoyolabRoutePayloadCache = new Map();
    const hoyolabRouteByTitleCache = new Map();
    let lastSyncedRouteSig = '';
    let userClearedRouteSig = '';
    let pendingRouteLoad = null;
    let routeLoadRetryTimer = null;
    let routeLoadSeq = 0;
    let routeDomClickHandler = null;
    let routeDetailObserver = null;
    let routeDetailTimers = [];
    let routeHashChangeHandler = null;
    let lastObservedRouteDomFp = '';
    let cachedMarkerIndexSrc = null;
    let cachedMarkerIndexLen = -1;
    let cachedMarkerByMarker = null;
    let cachedMarkerById = null;
    let cachedFloorGroupListSrc = null;
    let cachedFloorGroupListLen = -1;
    let cachedFloorLabelMap = null;
    let exportInProgress = false;
    let lastLanPinsTs = 0;
    let lastLanPinsFp = '';
    const cachedMarkedIdsByMap = new Map();
    const cachedMarkedFetchedAt = new Map();
    const markedFetchInFlight = new Map();
    const recentMarkOverrideByMap = new Map(); // Map<number, Map<string, { marked: boolean, until: number }>>

    function _setLocalMarkOverride(mapId, pinId, marked, ttlMs = 8000) {
        if (pinId === null || pinId === undefined || String(pinId).trim() === '') return;
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
            ? parseInt(mapId, 10)
            : getCurrentWebMapId();
        const pid = String(pinId).trim();
        const isMarked = Boolean(marked);
        let overrides = recentMarkOverrideByMap.get(mid);
        if (!overrides) {
            overrides = new Map();
            recentMarkOverrideByMap.set(mid, overrides);
        }
        overrides.set(pid, { marked: isMarked, until: Date.now() + ttlMs });
        let cached = cachedMarkedIdsByMap.get(mid);
        if (!cached && isMarked) {
            cached = new Set();
            cachedMarkedIdsByMap.set(mid, cached);
        }
        if (cached) {
            if (isMarked) cached.add(pid);
            else cached.delete(pid);
        }
    }

    function _getLocalMarkOverride(mapId, pinId) {
        if (pinId === null || pinId === undefined || String(pinId).trim() === '') return null;
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
            ? parseInt(mapId, 10)
            : getCurrentWebMapId();
        const overrides = recentMarkOverrideByMap.get(mid);
        if (!overrides) return null;
        const pid = String(pinId).trim();
        const entry = overrides.get(pid);
        if (!entry) return null;
        if (Date.now() > entry.until) {
            overrides.delete(pid);
            return null;
        }
        return Boolean(entry.marked);
    }

    function _applyLocalOverridesToSet(mapId, idSet) {
        if (!(idSet instanceof Set)) return;
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
            ? parseInt(mapId, 10)
            : getCurrentWebMapId();
        const overrides = recentMarkOverrideByMap.get(mid);
        if (!overrides) return;
        const now = Date.now();
        for (const [pid, entry] of overrides.entries()) {
            if (!entry || now > entry.until) {
                overrides.delete(pid);
                continue;
            }
            if (entry.marked) idSet.add(pid);
            else idSet.delete(pid);
        }
    }
    // HoYoLAB label (category) names per map+lang: point items carry only label_id, so the
    // in-game Mini-HUD would otherwise show a generic "Map Pin" for every target.
    const labelNamesByKey = new Map();
    const labelParentByKey = new Map();
    const labelFetchInFlight = new Map();
    const hookedMarkStores = new WeakSet();
    const hookedPopupMaps = new WeakSet();
    let navMarkChain = Promise.resolve();
    // Nearby feed geometry: the controller needs the searched radius to tell "out of window"
    // from "collected on web" (prevents the in-game arrow from flipping between pins).
    const NEARBY_QUERY_RADIUS = 260;
    const NEARBY_QUERY_LIMIT = 40;
    let popupNavObserver = null;
    let popupNavDebounceTimer = null;
    let lastOpenedPopupRef = null;
    let lastEnabledCatSig = null;
    let filterRefreshTimer = null;
    try {
        const savedSync = localStorage.getItem('gh_hotkey_sync_display') || localStorage.getItem('gh_hotkey_display');
        if (savedSync) currentHotkeySyncDisplay = savedSync;
        const savedDebug = localStorage.getItem('gh_hotkey_debug_display');
        if (savedDebug) currentHotkeyDebugDisplay = savedDebug;
        const savedSwitch = localStorage.getItem('gh_hotkey_switch_display');
        if (savedSwitch) currentHotkeySwitchDisplay = savedSwitch;
        const savedSyncRaw = localStorage.getItem('gh_hotkey_sync_raw');
        if (savedSyncRaw) currentHotkeySyncRaw = savedSyncRaw;
        const savedDebugRaw = localStorage.getItem('gh_hotkey_debug_raw');
        if (savedDebugRaw) currentHotkeyDebugRaw = savedDebugRaw;
        const savedSwitchRaw = localStorage.getItem('gh_hotkey_switch_raw');
        if (savedSwitchRaw) currentHotkeySwitchRaw = savedSwitchRaw;
        const savedAutoM = localStorage.getItem('gh_auto_sync_m');
        if (savedAutoM === '0' || savedAutoM === 'false') autoSyncMapMEnabled = false;
        const savedCompact = localStorage.getItem('gh_compact_ui');
        if (savedCompact === '1' || savedCompact === 'true') compactUiEnabled = true;
    } catch(e) {}

    function tr(key, params) {
        if (window.GHI18n && typeof window.GHI18n.t === 'function') {
            return window.GHI18n.t(key, params);
        }
        return key;
    }

    console.log(`%c[Genshin Helper ${SCRIPT_BUILD}] Loaded (i18n ${window.GHI18n ? 'ok' : 'MISSING'}) (In-Place Dual Marker + Drag Auto-Unpin + Safe Compact UI)`, 'color: #38bdf8; font-size: 16px; font-weight: bold;');

    function setAutoSyncUI(enabled) {
        autoSyncMapMEnabled = !!enabled;
        try { localStorage.setItem('gh_auto_sync_m', autoSyncMapMEnabled ? '1' : '0'); } catch(e) {}
    }

    let compactTagTimer = null;

    function isProtectedSpaNode(el) {
        if (!el || el.nodeType !== 1) return true;
        if (el.matches && el.matches('.gt-modal, .gt-popup-root, .map-loading-mask, .leaflet-container, #gh-mini-toast, #gh-recenter-btn, #gh-diag-modal-overlay')) {
            return true;
        }
        if (el.closest && el.closest('.gt-modal, .gt-popup-root, .map-loading-mask')) {
            return true;
        }
        if (el.querySelector && el.querySelector('.leaflet-container')) {
            return true;
        }
        return false;
    }

    function clearCompactDomTags() {
        try {
            document.querySelectorAll('.gh-compact-sidebar').forEach((el) => el.classList.remove('gh-compact-sidebar'));
            document.querySelectorAll('.gh-compact-hide').forEach((el) => el.classList.remove('gh-compact-hide'));
        } catch (e) {}
    }

    function applyCompactDomTags() {
        clearCompactDomTags();
        if (!compactUiEnabled || !document.body) return;
        try {
            const vw = window.innerWidth || 1280;
            const vh = window.innerHeight || 720;

            // 1. Identify left filter sidebar container regardless of HoYoLAB class hashes
            const explicitSidebars = document.querySelectorAll(
                '.filter-panel, [class*="filter-panel"], [class*="map-filter"], [class*="sidebar"]'
            );
            let taggedSidebar = false;
            for (const el of explicitSidebars) {
                if (isProtectedSpaNode(el)) continue;
                const r = el.getBoundingClientRect();
                if (r.width >= 200 && r.width <= 460 && r.height >= vh * 0.45 && r.left <= 80) {
                    el.classList.add('gh-compact-sidebar');
                    taggedSidebar = true;
                    break;
                }
            }
            if (!taggedSidebar) {
                const candidates = document.querySelectorAll('aside, section, div');
                for (const el of candidates) {
                    if (isProtectedSpaNode(el)) continue;
                    const style = window.getComputedStyle(el);
                    if (style.position !== 'fixed' && style.position !== 'absolute') continue;
                    const r = el.getBoundingClientRect();
                    if (r.width >= 240 && r.width <= 440 && r.height >= vh * 0.55 && r.left >= -20 && r.left <= 60 && r.top <= 120) {
                        const txt = (el.innerText || '').slice(0, 600);
                        if (/Điểm Dịch Chuyển|Thần Đồng|Rương|Thám Hiểm|Bộ lọc|Teleport|Oculi|Chest|Pins|Filters|Markers/i.test(txt)) {
                            el.classList.add('gh-compact-sidebar');
                            break;
                        }
                    }
                }
            }

            // 2. Hide bottom-right HoYoLAB App QR promo box & floating download cards
            const allBoxes = document.querySelectorAll('div, section, a');
            for (const el of allBoxes) {
                if (isProtectedSpaNode(el)) continue;
                const r = el.getBoundingClientRect();
                if (r.width >= 60 && r.width <= 260 && r.height >= 60 && r.height <= 260 && r.right >= vw - 48 && r.bottom >= vh - 48) {
                    const txt = (el.innerText || '').trim();
                    if (/HoYoLAB\s*App|Quét mã|Scan\s*QR|Download/i.test(txt)) {
                        el.classList.add('gh-compact-hide');
                    }
                }
            }
        } catch (e) {}
    }

    function setCompactUI(enabled, opts = {}) {
        const next = Boolean(enabled);
        const changed = compactUiEnabled !== next;
        compactUiEnabled = next;
        try { localStorage.setItem('gh_compact_ui', compactUiEnabled ? '1' : '0'); } catch(e) {}
        injectThemeStyles();
        if (document.body) {
            document.body.classList.toggle('gh-compact-ui', compactUiEnabled);
        }
        if (compactTagTimer) {
            clearTimeout(compactTagTimer);
            compactTagTimer = null;
        }
        if (compactUiEnabled) {
            applyCompactDomTags();
            compactTagTimer = setTimeout(applyCompactDomTags, 600);
        } else {
            clearCompactDomTags();
        }
        if (opts.syncStorage !== false) {
            try {
                window.postMessage({
                    __gh_ext: true,
                    dir: 'to-isolated',
                    kind: 'COMPACT_UI_SET',
                    enabled: compactUiEnabled
                }, window.location.origin);
            } catch (e) {}
        }
        if (changed && !opts.silent) {
            setLog(tr(compactUiEnabled ? 'page.compact.enabled' : 'page.compact.disabled'), '#a855f7');
        }
        return compactUiEnabled;
    }

    // Small stroked icons (24-unit grid, currentColor) for the in-page controls.
    const GH_ICON_PATHS = {
        'chevron-left': ['M15 18l-6-6 6-6'],
        'chevron-right': ['M9 6l6 6-6 6'],
        crosshair: ['M12 3v3', 'M12 18v3', 'M3 12h3', 'M18 12h3', 'M12 5.5a6.5 6.5 0 1 0 0 13a6.5 6.5 0 1 0 0-13z', 'M12 10.5a1.5 1.5 0 1 0 0 3a1.5 1.5 0 1 0 0-3z'],
    };

    function ghIcon(name) {
        const ns = 'http://www.w3.org/2000/svg';
        const svg = document.createElementNS(ns, 'svg');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '2');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        svg.setAttribute('aria-hidden', 'true');
        for (const d of GH_ICON_PATHS[name] || []) {
            const path = document.createElementNS(ns, 'path');
            path.setAttribute('d', d);
            svg.appendChild(path);
        }
        return svg;
    }

    function updateRecenterButtonUI() {
        const existing = document.getElementById('gh-recenter-btn');
        if (autoPanEnabled) {
            if (existing) existing.classList.remove('gh-recenter-visible');
            return;
        }
        injectThemeStyles();
        let btn = existing;
        if (!btn) {
            btn = document.createElement('button');
            btn.id = 'gh-recenter-btn';
            btn.type = 'button';
            btn.className = 'gh-root';
            btn.addEventListener('click', (e) => {
                e.preventDefault();
                e.stopPropagation();
                recenterToPlayer();
            });
            (document.body || document.documentElement).appendChild(btn);
        }
        let label = btn.querySelector('.gh-recenter-label');
        if (!label) {
            while (btn.firstChild) btn.removeChild(btn.firstChild);
            label = document.createElement('span');
            label.className = 'gh-recenter-label';
            btn.append(ghIcon('crosshair'), label);
        }
        label.textContent = tr('page.unpin.recenter_btn');
        btn.title = tr('page.unpin.recenter_title');
        btn.setAttribute('aria-label', label.textContent);
        btn.classList.add('gh-recenter-visible');
    }

    function setAutoPanPinned(pinned, opts = {}) {
        const next = Boolean(pinned);
        const changed = autoPanEnabled !== next;
        autoPanEnabled = next;
        updateRecenterButtonUI();
        if (changed && activeWebRoute && activeWebRoute.active) {
            // "Về nhân vật" is highlighted only while the map is not following the player.
            renderRouteBar(activeWebRoute);
        }
        if (changed && !next && !opts.silent) {
            showMiniToast(tr('page.unpin.toast'), '#fbbf24');
        } else if (changed && next && opts.notifyResumed) {
            showMiniToast(tr('page.unpin.resumed_toast'), '#38bdf8');
        }
        return autoPanEnabled;
    }

    function recenterToPlayer() {
        setAutoPanPinned(true, { notifyResumed: true });
        try {
            const ctx = findMapContext();
            if (ctx && ctx.map && playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                const targetMapId = Number(playerLastState.map_id || 2);
                const savedZoom = (typeof ctx.map.getZoom === 'function') ? ctx.map.getZoom() : null;
                let areaChanged = false;
                if (
                    targetMapId === 2 &&
                    targetMapId === getCurrentWebMapId() &&
                    ((playerLastState.area_id !== undefined && playerLastState.area_id !== null) || playerLastState.area_name)
                ) {
                    const areaRes = ensureAreaSwitch(
                        ctx,
                        playerLastState.area_id,
                        playerLastState.area_name,
                        Boolean(playerLastState.is_underground && autoSwitchLayerEnabled)
                    );
                    if (areaRes === 'changed') {
                        areaChanged = true;
                        lastMinimapAreaSwitchId = (playerLastState.area_id !== undefined && playerLastState.area_id !== null)
                            ? Number(playerLastState.area_id)
                            : String(playerLastState.area_name).trim().toLowerCase();
                        lastMinimapAreaSwitchTs = Date.now();
                    }
                }
                const allowAnim = !document.hidden && !areaChanged;
                if (areaChanged && savedZoom !== null) {
                    ctx.map.setView([playerLastState.lat, playerLastState.lng], savedZoom, { animate: false });
                } else {
                    ctx.map.panTo([playerLastState.lat, playerLastState.lng], {
                        animate: allowAnim,
                        duration: allowAnim ? 0.28 : 0
                    });
                }
                if (areaChanged) {
                    schedulePostAreaOrLayerRefresh(ctx, playerLastState, targetMapId, savedZoom, true);
                }
            }
        } catch (e) {}
    }

    function bindDragUnpinListeners(ctx) {
        try {
            const container = (ctx && ctx.map && typeof ctx.map.getContainer === 'function')
                ? ctx.map.getContainer()
                : document.querySelector('.leaflet-container');
            if (!container || container === dragBoundContainerEl) return;

            if (typeof dragListenersCleanup === 'function') {
                dragListenersCleanup();
                dragListenersCleanup = null;
            }

            let pointerActive = false;
            let dragStartTs = 0;
            let dragStartX = 0;
            let dragStartY = 0;

            const readCoords = (evt) => {
                if (evt.touches && evt.touches.length > 0) {
                    return { x: evt.touches[0].clientX, y: evt.touches[0].clientY };
                }
                return { x: evt.clientX, y: evt.clientY };
            };

            const onPointerDown = (evt) => {
                if (evt.type === 'mousedown' && evt.button !== 0) return;
                const pos = readCoords(evt);
                pointerActive = true;
                dragStartTs = Date.now();
                dragStartX = pos.x;
                dragStartY = pos.y;
            };

            const onPointerMove = (evt) => {
                if (!pointerActive || !autoPanEnabled) return;
                const pos = readCoords(evt);
                const heldMs = Date.now() - dragStartTs;
                const distPx = Math.hypot(pos.x - dragStartX, pos.y - dragStartY);
                if (heldMs > 100 && distPx > 20) {
                    pointerActive = false;
                    setAutoPanPinned(false);
                }
            };

            const onPointerEnd = () => {
                pointerActive = false;
            };

            container.addEventListener('mousedown', onPointerDown, { passive: true });
            container.addEventListener('mousemove', onPointerMove, { passive: true });
            container.addEventListener('touchstart', onPointerDown, { passive: true });
            container.addEventListener('touchmove', onPointerMove, { passive: true });
            window.addEventListener('mouseup', onPointerEnd, { passive: true });
            window.addEventListener('touchend', onPointerEnd, { passive: true });
            window.addEventListener('touchcancel', onPointerEnd, { passive: true });

            dragBoundContainerEl = container;
            dragListenersCleanup = () => {
                container.removeEventListener('mousedown', onPointerDown);
                container.removeEventListener('mousemove', onPointerMove);
                container.removeEventListener('touchstart', onPointerDown);
                container.removeEventListener('touchmove', onPointerMove);
                window.removeEventListener('mouseup', onPointerEnd);
                window.removeEventListener('touchend', onPointerEnd);
                window.removeEventListener('touchcancel', onPointerEnd);
            };
        } catch (e) {}
    }

    if (window.GHI18n && typeof window.GHI18n.onChange === 'function') {
        window.GHI18n.onChange(() => {
            if (!autoPanEnabled) {
                updateRecenterButtonUI();
            }
            if (activeWebRoute && activeWebRoute.active) {
                renderRouteBar(activeWebRoute);
            }
        });
    }

    function clearLayerTimers() {
        for (const tid of layerTimers) clearTimeout(tid);
        layerTimers = [];
    }

    function scheduleLayerTimer(fn, delay) {
        const tid = setTimeout(() => {
            layerTimers = layerTimers.filter(t => t !== tid);
            fn();
        }, delay);
        layerTimers.push(tid);
        return tid;
    }

    function clearMinimapAuxTimers() {
        for (const tid of minimapAuxTimers) clearTimeout(tid);
        minimapAuxTimers = [];
    }

    function scheduleMinimapAuxTimer(fn, delay) {
        const tid = setTimeout(() => {
            minimapAuxTimers = minimapAuxTimers.filter(t => t !== tid);
            fn();
        }, delay);
        minimapAuxTimers.push(tid);
        return tid;
    }

    // =========================================================================
    // 1. Lightweight Auto-Dismiss Mini Toast & Theme Styles
    // =========================================================================
    function showMiniToast(msg, color) {
        try {
            if (!msg) return;
            injectThemeStyles();

            let toast = document.getElementById('gh-mini-toast');
            if (!toast) {
                toast = document.createElement('div');
                toast.id = 'gh-mini-toast';
                toast.className = 'gh-root';
                toast.setAttribute('aria-live', 'polite');

                const dot = document.createElement('span');
                dot.id = 'gh-mini-toast-dot';
                const text = document.createElement('span');
                text.id = 'gh-mini-toast-text';

                toast.appendChild(dot);
                toast.appendChild(text);
                (document.body || document.documentElement).appendChild(toast);
            }

            const dotEl = toast.querySelector('#gh-mini-toast-dot');
            const textEl = toast.querySelector('#gh-mini-toast-text');
            const dotColor = color || '#38bdf8';

            if (dotEl) {
                dotEl.style.background = dotColor;
                dotEl.style.boxShadow = `0 0 8px ${dotColor}`;
            }
            if (textEl) {
                textEl.textContent = String(msg);
            }

            toast.classList.add('gh-toast-visible');
            if (miniToastTimer) {
                clearTimeout(miniToastTimer);
            }
            miniToastTimer = setTimeout(() => {
                miniToastTimer = null;
                const activeToast = document.getElementById('gh-mini-toast');
                if (activeToast) {
                    activeToast.classList.remove('gh-toast-visible');
                }
            }, 2400);
        } catch (e) {}
    }

    function setLog(msg, color) {
        console.log('[Genshin Helper v5.7.0]', msg);
        if (document.hidden) {
            return;
        }
        showMiniToast(msg, color);
    }

    function updateHotkeyDisplay(displayName, action = 'sync', rawKey = null) {
        if (!displayName) return;
        const act = (action || 'sync').toLowerCase();
        if (act === 'debug') {
            currentHotkeyDebugDisplay = displayName;
            if (rawKey) currentHotkeyDebugRaw = rawKey;
            try {
                localStorage.setItem('gh_hotkey_debug_display', displayName);
                if (rawKey) localStorage.setItem('gh_hotkey_debug_raw', rawKey);
            } catch(e) {}
        } else if (act === 'switch') {
            currentHotkeySwitchDisplay = displayName;
            if (rawKey) currentHotkeySwitchRaw = rawKey;
            try {
                localStorage.setItem('gh_hotkey_switch_display', displayName);
                if (rawKey) localStorage.setItem('gh_hotkey_switch_raw', rawKey);
            } catch(e) {}
        } else {
            currentHotkeySyncDisplay = displayName;
            if (rawKey) currentHotkeySyncRaw = rawKey;
            try {
                localStorage.setItem('gh_hotkey_sync_display', displayName);
                localStorage.setItem('gh_hotkey_display', displayName);
                if (rawKey) localStorage.setItem('gh_hotkey_sync_raw', rawKey);
            } catch(e) {}
        }
    }

    function injectThemeStyles() {
        if (document.getElementById('gh-theme-styles')) return;

        const style = document.createElement('style');
        style.id = 'gh-theme-styles';
        style.textContent = `
            /* HoYoLAB sets Corbel/Tahoma on every element; the extension's own UI keeps the
               DESIGN.md system stack (Segoe UI on Windows, full Vietnamese coverage). */
            .gh-root, .gh-root *,
            #gh-route-bar, #gh-route-bar *,
            #gh-recenter-btn, #gh-recenter-btn *,
            #gh-mini-toast, #gh-mini-toast * {
                font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Arial, sans-serif !important;
            }
            .gh-root, .gh-root * {
                box-sizing: border-box;
                -webkit-font-smoothing: antialiased;
                -moz-osx-font-smoothing: grayscale;
                text-rendering: optimizeLegibility;
                font-variant-numeric: lining-nums tabular-nums !important;
                -webkit-font-feature-settings: "lnum" 1, "tnum" 1 !important;
                font-feature-settings: "lnum" 1, "tnum" 1 !important;
            }

            .gh-mono {
                font-family: 'JetBrains Mono', 'SF Mono', Consolas, Monaco, monospace !important;
                font-variant-numeric: tabular-nums lining-nums !important;
                letter-spacing: -0.01em;
            }

            /* --- Lightweight Auto-Dismiss Toast --- */
            #gh-mini-toast {
                position: fixed;
                top: 16px;
                left: 50%;
                transform: translateX(-50%) translateY(-8px);
                z-index: 2147483646;
                display: inline-flex;
                align-items: center;
                gap: 8px;
                max-width: min(480px, calc(100vw - 32px));
                padding: 7px 14px;
                border-radius: 999px;
                background: rgba(15, 23, 42, 0.90);
                backdrop-filter: blur(12px);
                border: 1px solid rgba(56, 189, 248, 0.35);
                box-shadow: 0 10px 26px rgba(2, 6, 23, 0.55);
                color: #ECEFF4;
                font-size: 12px;
                font-weight: 500;
                line-height: 1.3;
                pointer-events: none;
                opacity: 0;
                transition: opacity 0.22s ease, transform 0.22s ease;
            }
            #gh-mini-toast.gh-toast-visible {
                opacity: 1;
                transform: translateX(-50%) translateY(0);
            }
            body.gh-has-route-bar #gh-mini-toast {
                top: 62px;
            }
            #gh-mini-toast-dot {
                width: 8px;
                height: 8px;
                border-radius: 50%;
                flex-shrink: 0;
                background: #38bdf8;
            }
            #gh-mini-toast-text {
                white-space: nowrap;
                overflow: hidden;
                text-overflow: ellipsis;
            }

            /* --- Floating Re-center Button (when user drags map to unpin) --- */
            #gh-recenter-btn {
                position: fixed;
                bottom: 24px;
                left: 50%;
                transform: translateX(-50%) translateY(10px);
                z-index: 2147483645;
                display: inline-flex;
                align-items: center;
                gap: 8px;
                height: 36px;
                padding: 0 16px 0 13px;
                border-radius: 999px;
                background: rgba(11, 15, 25, 0.94);
                border: 1px solid rgba(56, 189, 248, 0.5);
                box-shadow: 0 10px 28px rgba(2, 6, 23, 0.6);
                color: #BAE6FD;
                font-size: 12px;
                font-weight: 600;
                line-height: 1;
                letter-spacing: 0;
                white-space: nowrap;
                cursor: pointer;
                opacity: 0;
                pointer-events: none;
                transition: opacity 0.2s cubic-bezier(0.16, 1, 0.3, 1), transform 0.2s cubic-bezier(0.16, 1, 0.3, 1), background-color 0.15s ease, border-color 0.15s ease;
            }
            #gh-recenter-btn svg {
                width: 16px;
                height: 16px;
                flex-shrink: 0;
                color: #38bdf8;
            }
            #gh-recenter-btn.gh-recenter-visible {
                opacity: 1;
                pointer-events: auto;
                transform: translateX(-50%) translateY(0);
            }
            #gh-recenter-btn:hover {
                background: rgba(3, 105, 161, 0.55);
                border-color: #38bdf8;
                color: #ECEFF4;
            }
            #gh-recenter-btn:hover svg {
                color: #ECEFF4;
            }
            #gh-recenter-btn:focus-visible {
                outline: 2px solid #38bdf8;
                outline-offset: 3px;
            }
            #gh-recenter-btn:active {
                transform: translateX(-50%) translateY(1px);
            }

            /* --- Live In-Place Dual-Rotation Player Marker (--gh-dir & --gh-rot) --- */
            @keyframes gh-player-pulse {
                0% { transform: translate(-50%, -50%) scale(0.72); opacity: 0.85; }
                75% { transform: translate(-50%, -50%) scale(1.65); opacity: 0; }
                100% { transform: translate(-50%, -50%) scale(1.65); opacity: 0; }
            }
            .gh-player-marker-wrap {
                position: relative;
                width: 44px;
                height: 44px;
                pointer-events: none;
                --gh-dir: 0deg;
                --gh-rot: 0deg;
            }
            /* High-contrast base disc: keeps the arrow readable on top of busy, colourful pin icons */
            .gh-player-marker-wrap::before {
                content: '';
                position: absolute;
                left: 50%;
                top: 50%;
                width: 28px;
                height: 28px;
                transform: translate(-50%, -50%);
                border-radius: 50%;
                background: rgba(15, 23, 42, 0.82);
                border: 2px solid #ffffff;
                box-shadow: 0 0 0 3px rgba(56, 189, 248, 0.55), 0 2px 10px rgba(0, 0, 0, 0.55);
                pointer-events: none;
            }
            .gh-player-pulse-ring {
                position: absolute;
                left: 50%;
                top: 50%;
                width: 34px;
                height: 34px;
                border-radius: 50%;
                border: 2px solid #38bdf8;
                background: rgba(56, 189, 248, 0.22);
                animation: gh-player-pulse 1.8s cubic-bezier(0.2, 0.6, 0.3, 1) infinite;
            }
            .gh-player-fov-svg {
                position: absolute;
                left: 50%;
                top: 50%;
                width: 72px;
                height: 72px;
                transform-origin: 50% 50%;
                transform: translate(-50%, -50%) rotate(var(--gh-rot, 0deg));
                opacity: 0;
                transition: transform 90ms linear, opacity 160ms ease;
                will-change: transform;
                pointer-events: none;
            }
            .gh-player-marker-wrap.gh-has-fov .gh-player-fov-svg {
                opacity: 1;
            }
            .gh-player-arrow-svg {
                position: absolute;
                left: 50%;
                top: 50%;
                width: 30px;
                height: 30px;
                transform-origin: 50% 50%;
                transform: translate(-50%, -50%) rotate(var(--gh-dir, 0deg));
                transition: transform 90ms linear;
                will-change: transform;
                filter: drop-shadow(0 2px 6px rgba(2, 132, 199, 0.85));
                pointer-events: none;
            }

            /* --- TSP Smart Route SVG Overlay (overlayPane) --- */
            .gh-tsp-route-svg {
                position: absolute;
                left: 0;
                top: 0;
                pointer-events: none;
                overflow: visible;
                z-index: 450;
            }
            .gh-tsp-chevron-text {
                font-family: 'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', Arial, sans-serif;
                font-size: 11px;
                font-weight: 700;
                fill: #e0f2fe;
                stroke: #0f172a;
                stroke-width: 2.2px;
                paint-order: stroke;
                letter-spacing: 2px;
            }
            .gh-tsp-stop-label {
                font-family: 'JetBrains Mono', 'SF Mono', Consolas, Monaco, monospace;
                font-size: 10px;
                font-weight: 700;
                fill: #f8fafc;
            }

            /* --- Floating Web Route Companion Pill --- */
            #gh-route-bar {
                position: fixed;
                top: 12px;
                left: 50%;
                transform: translateX(-50%);
                z-index: 2147483000;
                display: flex;
                flex-wrap: wrap;
                align-items: center;
                justify-content: center;
                gap: 6px 10px;
                padding: 5px 5px 5px 14px;
                border-radius: 9px;
                background: rgba(11, 15, 25, 0.94);
                border: 1px solid rgba(56, 189, 248, 0.32);
                box-shadow: 0 10px 28px rgba(2, 6, 23, 0.6);
                color: #ECEFF4;
                font-size: 12px;
                font-weight: 600;
                line-height: 1.25;
                letter-spacing: 0;
                pointer-events: auto;
                width: max-content;
                max-width: calc(100vw - 24px);
                box-sizing: border-box;
            }
            #gh-route-bar * {
                box-sizing: border-box;
            }
            #gh-route-bar .gh-route-group {
                display: inline-flex;
                align-items: center;
                gap: 6px;
                min-width: 0;
            }
            #gh-route-bar .gh-route-sep {
                width: 1px;
                height: 18px;
                background: #232D42;
                flex-shrink: 0;
            }
            #gh-route-bar .gh-route-dot {
                width: 8px;
                height: 8px;
                border-radius: 50%;
                flex-shrink: 0;
                box-shadow: 0 0 0 3px #232D42;
            }
            #gh-route-bar .gh-route-title {
                max-width: 260px;
                min-width: 0;
                white-space: nowrap;
                overflow: hidden;
                text-overflow: ellipsis;
                color: #ECEFF4;
                font-size: 14px;
                font-weight: 700;
                letter-spacing: -0.01em;
            }
            #gh-route-bar .gh-route-sec {
                display: inline-flex;
                align-items: baseline;
                gap: 5px;
                min-width: 74px;
                justify-content: center;
                font-size: 12px;
                white-space: nowrap;
                font-variant-numeric: lining-nums tabular-nums;
            }
            #gh-route-bar .gh-route-sec b {
                font-weight: 700;
                color: #BAE6FD;
            }
            #gh-route-bar .gh-route-sec span {
                font-weight: 500;
                color: #8B9BB5;
            }
            #gh-route-bar .gh-route-btn {
                display: inline-flex;
                align-items: center;
                justify-content: center;
                gap: 6px;
                height: 28px;
                margin: 0;
                padding: 0 11px;
                border: 1px solid #32405D;
                border-radius: 7px;
                background: #121826;
                color: #ECEFF4;
                font-size: 12px;
                font-weight: 600;
                line-height: 1;
                white-space: nowrap;
                cursor: pointer;
                transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease;
            }
            #gh-route-bar .gh-route-btn svg {
                width: 14px;
                height: 14px;
                flex-shrink: 0;
            }
            #gh-route-bar .gh-route-btn.is-icon {
                width: 28px;
                padding: 0;
            }
            #gh-route-bar .gh-route-btn:hover:not(:disabled) {
                border-color: rgba(56, 189, 248, 0.6);
                background: #182134;
                color: #ECEFF4;
            }
            #gh-route-bar .gh-route-btn:focus-visible {
                outline: 2px solid #38bdf8;
                outline-offset: 2px;
            }
            #gh-route-bar .gh-route-btn:active:not(:disabled) {
                transform: translateY(1px);
            }
            #gh-route-bar .gh-route-btn:disabled {
                opacity: 0.38;
                cursor: default;
            }
            #gh-route-bar .gh-route-btn.is-primary {
                border-color: rgba(56, 189, 248, 0.55);
                background: rgba(3, 105, 161, 0.42);
                color: #BAE6FD;
            }
            #gh-route-bar .gh-route-btn.is-primary:hover:not(:disabled) {
                background: rgba(3, 105, 161, 0.7);
                border-color: #38bdf8;
            }
            #gh-route-bar .gh-route-btn.is-follow {
                border-color: rgba(217, 164, 65, 0.6);
                background: rgba(217, 164, 65, 0.12);
                color: #f3d28b;
            }
            #gh-route-bar .gh-route-btn.is-follow:hover:not(:disabled) {
                border-color: #d9a441;
                background: rgba(217, 164, 65, 0.22);
                color: #F3D28B;
            }
            #gh-route-bar .gh-route-btn.is-stop:hover:not(:disabled) {
                border-color: rgba(248, 113, 113, 0.65);
                background: rgba(248, 113, 113, 0.14);
                color: #fecaca;
            }
            #gh-popup-navigate-btn {
                display: inline-flex;
                align-items: center;
                justify-content: center;
                gap: 5px;
                margin-left: 8px;
                padding: 5px 11px;
                border-radius: 999px;
                border: 1px solid rgba(251, 191, 36, 0.65);
                background: linear-gradient(135deg, rgba(217, 119, 6, 0.92), rgba(180, 83, 9, 0.92));
                color: #fffbeb;
                font-size: 12px;
                font-weight: 600;
                line-height: 1.2;
                cursor: pointer;
                white-space: nowrap;
                box-shadow: 0 2px 10px rgba(217, 119, 6, 0.35);
                transition: transform 0.12s ease, filter 0.15s ease;
            }
            #gh-popup-navigate-btn:hover:not(:disabled) {
                filter: brightness(1.12);
                transform: translateY(-1px);
            }
            #gh-popup-navigate-btn:disabled {
                opacity: 0.6;
                cursor: wait;
            }
            #gh-popup-navigate-btn.is-locked {
                border-color: rgba(248, 113, 113, 0.72);
                background: linear-gradient(135deg, rgba(185, 28, 28, 0.92), rgba(153, 27, 27, 0.92));
                color: #fef2f2;
                box-shadow: 0 2px 10px rgba(220, 38, 38, 0.35);
            }
            #gh-target-lock-pill {
                position: fixed;
                bottom: 22px;
                left: 50%;
                transform: translateX(-50%);
                z-index: 999990;
                display: inline-flex;
                align-items: center;
                gap: 8px;
                max-width: min(440px, 88vw);
                padding: 6px 10px 6px 12px;
                border-radius: 999px;
                border: 1px solid rgba(251, 191, 36, 0.65);
                background: rgba(15, 23, 42, 0.92);
                color: #fef3c7;
                font-size: 12px;
                font-weight: 600;
                box-shadow: 0 6px 24px rgba(0, 0, 0, 0.45);
                backdrop-filter: blur(8px);
                pointer-events: auto;
            }
            #gh-target-lock-pill .gh-lock-pill-name {
                overflow: hidden;
                text-overflow: ellipsis;
                white-space: nowrap;
                cursor: pointer;
            }
            #gh-target-lock-pill .gh-lock-pill-cancel {
                flex-shrink: 0;
                padding: 3px 9px;
                border-radius: 999px;
                border: 1px solid rgba(248, 113, 113, 0.65);
                background: rgba(220, 38, 38, 0.22);
                color: #fecaca;
                font-size: 11px;
                font-weight: 600;
                cursor: pointer;
                transition: background 0.15s ease;
            }
            #gh-target-lock-pill .gh-lock-pill-cancel:hover {
                background: rgba(220, 38, 38, 0.45);
                color: #ffffff;
            }

            /* --- Safe Compact HoYoLAB Web UI (opt-in via body.gh-compact-ui) --- */
            body.gh-compact-ui .gh-compact-hide,
            body.gh-compact-ui .mhy-header,
            body.gh-compact-ui .hoyolab-header,
            body.gh-compact-ui .mhy-hoyolab-app-header,
            body.gh-compact-ui [class*="mhy-header"],
            body.gh-compact-ui [class*="hoyolab-header"],
            body.gh-compact-ui [class*="bbs-header"],
            body.gh-compact-ui .map-notice-bar,
            body.gh-compact-ui [class*="notice-bar"],
            body.gh-compact-ui .bbs-qr,
            body.gh-compact-ui [class*="bbs-qr"],
            body.gh-compact-ui [class*="qr-code"],
            body.gh-compact-ui [class*="qrcode"],
            body.gh-compact-ui [class*="download-qr"],
            body.gh-compact-ui .mihoyo-cookie-tips,
            body.gh-compact-ui .strong-tip-closable,
            body.gh-compact-ui .driver-popover {
                display: none !important;
            }
            body.gh-compact-ui .filter-panel,
            body.gh-compact-ui .gh-compact-sidebar {
                transform: translateX(calc(-100% + 26px)) !important;
                opacity: 0.35 !important;
                transition: transform 0.22s ease, opacity 0.22s ease !important;
            }
            body.gh-compact-ui .filter-panel:hover,
            body.gh-compact-ui .filter-panel:focus-within,
            body.gh-compact-ui .gh-compact-sidebar:hover,
            body.gh-compact-ui .gh-compact-sidebar:focus-within {
                transform: translateX(0) !important;
                opacity: 1 !important;
            }

            /* --- CRITICAL SPA SAFETY GUARD (never hide/remove .gt-modal, .gt-popup-root, or .map-loading-mask) --- */
            .gt-modal[data-modal-visible-count="0"] {
                pointer-events: none !important;
            }
            .map-loading-mask {
                pointer-events: none !important;
            }
        `;
        (document.head || document.documentElement).appendChild(style);
    }

    function setStatus(connected, active = engineActive) {
        isConnected = Boolean(connected);
        if (typeof active === 'boolean') {
            engineActive = active;
        }
    }

    // =========================================================================
    // 2. Deep Map Context & Native MarkerList Resolver
    // =========================================================================
    function findMapContext() {
        let map = null;
        let ciLayer = null;
        let markerList = null;
        let vm = null;

        const container = document.querySelector('.leaflet-container');
        if (container) {
            if (container._leaflet_map) map = container._leaflet_map;
            let el = container;
            while (el) {
                if (el.__vue__) {
                    const v = el.__vue__;
                    if (v.map) map = v.map;
                    if (v.ciLayer) ciLayer = v.ciLayer;
                    if (Array.isArray(v.markerList) && v.markerList.length > 0) markerList = v.markerList;
                    if (v.map && (v.ciLayer || v.markerList)) {
                        vm = v;
                        break;
                    }
                }
                el = el.parentElement;
            }
        }

        if (!markerList || !map) {
            const all = document.querySelectorAll('*');
            for (let i = 0; i < all.length; i++) {
                const v = all[i].__vue__;
                if (v) {
                    if (!map && v.map && v.map._layers) map = v.map;
                    if (!ciLayer && v.ciLayer) ciLayer = v.ciLayer;
                    if (!markerList && Array.isArray(v.markerList) && v.markerList.length > 0) markerList = v.markerList;
                    if (map && markerList) {
                        vm = v;
                        break;
                    }
                }
            }
        }

        if (!map && window.map && window.map._layers) map = window.map;

        return { map, ciLayer, markerList, vm };
    }

    function getAllMarkers(ctx) {
        const { map, ciLayer, markerList } = ctx;
        const markers = [];

        if (Array.isArray(markerList) && markerList.length > 0 && map) {
            for (const item of markerList) {
                const m = item.marker;
                let latlng = null;
                if (m && typeof m.getLatLng === 'function') {
                    latlng = m.getLatLng();
                } else if (item.y_pos !== undefined && item.x_pos !== undefined) {
                    latlng = { lat: item.y_pos, lng: item.x_pos };
                }

                if (latlng && typeof map.latLngToContainerPoint === 'function') {
                    const pt = map.latLngToContainerPoint(latlng);
                    markers.push({
                        marker: m,
                        id: item.id,
                        labelId: item.label_id,
                        latlng: latlng,
                        point: pt,
                        rawItem: item,
                        source: 'markerList'
                    });
                }
            }
        }

        if (markers.length === 0 && ciLayer) {
            const rbushTree = ciLayer._markers || ciLayer._latlngMarkers;
            if (rbushTree) {
                let list = null;
                if (typeof rbushTree.all === 'function') list = rbushTree.all();
                else if (Array.isArray(rbushTree.data)) list = rbushTree.data;
                else if (rbushTree.data && Array.isArray(rbushTree.data.children)) list = rbushTree.data.children;

                if (list) {
                    for (const item of list) {
                        const m = item.data || item;
                        if (m && typeof m.getLatLng === 'function') {
                            const latlng = m.getLatLng();
                            const pt = map ? map.latLngToContainerPoint(latlng) : { x: (item.minX + item.maxX) / 2, y: (item.minY + item.maxY) / 2 };
                            markers.push({ marker: m, latlng, point: pt, source: 'ciLayer' });
                        }
                    }
                }
            }
        }

        return markers;
    }

    // =========================================================================
    // =========================================================================
    // 3. Universal Layered Exploration Manager (Caves & High-Altitude Floors)
    // =========================================================================
    function resolveGisVm(ctx = null) {
        if (ctx && ctx.vm && ctx.vm.$store) return ctx.vm;
        const direct = document.querySelector('.mhy-game-gis')?.__vue__;
        if (direct && direct.$store) return direct;
        const found = findMapContext();
        if (found && found.vm && found.vm.$store) return found.vm;
        const appVm = document.querySelector('#app')?.__vue__;
        if (appVm && appVm.$store) return appVm;
        return (found && found.vm) ? found.vm : (direct || null);
    }

    function _hasFilterConfig(vm) {
        if (!vm || typeof vm !== 'object') return false;
        if (Array.isArray(vm.flattenedMarkerConfig) && vm.flattenedMarkerConfig.length > 0) return true;
        if (Array.isArray(vm.filteredMarkerTypesData) && vm.filteredMarkerTypesData.length > 0) return true;
        return false;
    }

    // HoYoLAB stores `flattenedMarkerConfig`, `filteredMarkerTypesData`, `getFilteredLayers`,
    // and `layerVisible` on the child `filterPanel` Vue component (`gisVm.$refs.filterPanel` /
    // `.filter-panel.__vue__`), NOT directly on `.mhy-game-gis.__vue__`.
    function resolveFilterPanelVm(ctx = null) {
        try {
            const gisVm = resolveGisVm(ctx);
            if (gisVm) {
                if (gisVm.$refs && gisVm.$refs.filterPanel && _hasFilterConfig(gisVm.$refs.filterPanel)) {
                    return gisVm.$refs.filterPanel;
                }
                if (_hasFilterConfig(gisVm)) {
                    return gisVm;
                }
                if (Array.isArray(gisVm.$children)) {
                    for (let i = 0; i < gisVm.$children.length; i++) {
                        const ch = gisVm.$children[i];
                        if (_hasFilterConfig(ch)) return ch;
                        if (ch && Array.isArray(ch.$children)) {
                            for (let j = 0; j < ch.$children.length; j++) {
                                if (_hasFilterConfig(ch.$children[j])) return ch.$children[j];
                            }
                        }
                    }
                }
            }
            const fpEl = document.querySelector('.filter-panel');
            if (fpEl && fpEl.__vue__ && _hasFilterConfig(fpEl.__vue__)) {
                return fpEl.__vue__;
            }
        } catch (e) {}
        return null;
    }

    function _getFilterPanelFlatConfig(fpVm) {
        if (!fpVm || typeof fpVm !== 'object') return null;
        // HoYoLAB's `toggleLayer()`, `toggleGroup()`, `toggleLayerById()`, and `initShownTypesLayers()`
        // mutate `entry.layerVisible` on `filteredMarkerTypesData` children (area-cloned entries),
        // NOT on `flattenedMarkerConfig` (which reads the stale `markerConfig` prop).
        // Always read `filteredMarkerTypesData` FIRST, falling back to `flattenedMarkerConfig` only
        // when `filteredMarkerTypesData` is empty.
        if (Array.isArray(fpVm.filteredMarkerTypesData) && fpVm.filteredMarkerTypesData.length > 0) {
            const flat = [];
            for (let i = 0; i < fpVm.filteredMarkerTypesData.length; i++) {
                const grp = fpVm.filteredMarkerTypesData[i];
                if (grp && Array.isArray(grp.children)) {
                    for (let j = 0; j < grp.children.length; j++) {
                        if (grp.children[j]) flat.push(grp.children[j]);
                    }
                }
            }
            if (flat.length > 0) return flat;
        }
        if (Array.isArray(fpVm.flattenedMarkerConfig) && fpVm.flattenedMarkerConfig.length > 0) {
            return fpVm.flattenedMarkerConfig;
        }
        return null;
    }

    function _collectSelectedMarkerIds(fpVm) {
        const ids = new Set();
        try {
            if (fpVm && Array.isArray(fpVm.selectedMarkerTypes)) {
                for (let i = 0; i < fpVm.selectedMarkerTypes.length; i++) {
                    const it = fpVm.selectedMarkerTypes[i];
                    if (it && it.id !== undefined && it.id !== null) ids.add(String(it.id));
                }
            }
            const rawRoute = fpVm && fpVm.$route && fpVm.$route.query && fpVm.$route.query.shown_types;
            const rawSearch = (typeof window.location === 'object' && window.location.search)
                ? (new URLSearchParams(window.location.search)).get('shown_types')
                : null;
            const shownStr = (typeof rawRoute === 'string' && rawRoute.trim())
                ? rawRoute
                : (typeof rawSearch === 'string' ? rawSearch : '');
            if (shownStr) {
                const parts = shownStr.split(',');
                for (let i = 0; i < parts.length; i++) {
                    const p = parts[i].trim();
                    if (p) ids.add(p);
                }
            }
        } catch (e) {}
        return ids;
    }

    function _isFilterEntryVisible(entry, selectedIds) {
        if (!entry || typeof entry !== 'object') return false;
        if (typeof entry.layerVisible === 'boolean') return entry.layerVisible === true;
        if (selectedIds && selectedIds.size > 0 && entry.id !== undefined && entry.id !== null) {
            return selectedIds.has(String(entry.id));
        }
        return false;
    }

    function _getEnabledCategorySignature(ctx, mapId) {
        try {
            const fpVm = resolveFilterPanelVm(ctx);
            const flatCfg = _getFilterPanelFlatConfig(fpVm);
            if (!flatCfg) return null;
            const selectedIds = _collectSelectedMarkerIds(fpVm);
            const ids = [];
            for (let i = 0; i < flatCfg.length; i++) {
                const entry = flatCfg[i];
                if (_isFilterEntryVisible(entry, selectedIds)) {
                    ids.push(String(entry.id ?? i));
                }
            }
            if (fpVm && fpVm.selfMarkerIdMap && typeof fpVm.selfMarkerIdMap === 'object') {
                const vals = fpVm.selfMarkerIdMap instanceof Map
                    ? Array.from(fpVm.selfMarkerIdMap.values())
                    : Object.values(fpVm.selfMarkerIdMap);
                for (let i = 0; i < vals.length; i++) {
                    const entry = vals[i];
                    if (entry && entry.layerVisible === true) {
                        ids.push(`self:${entry.id ?? i}`);
                    }
                }
            }
            ids.sort();
            const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
                ? parseInt(mapId, 10)
                : getCurrentWebMapId();
            return `${mid}|${ids.join(',')}`;
        } catch (e) {
            return null;
        }
    }

    function ensureAreaSwitch(ctx, areaId, areaName, isUnderground = false) {
        if (!areaId && !areaName) return 'none';
        try {
            const gisVm = resolveGisVm(ctx);
            const store = gisVm && gisVm.$store ? gisVm.$store : null;
            const targetCleanName = (areaName || '').trim().toLowerCase();

            // 1. Primary Strategy: Direct Vuex Store commit + marker reload (no camera fitBounds, works even when dropdown is folded)
            if (store && store.state && Array.isArray(store.state.areaList) && store.state.areaList.length > 0) {
                const map2Areas = store.state.areaList.filter(a => a && Number(a.map_id) === 2);
                const pool = map2Areas.length > 0 ? map2Areas : store.state.areaList;
                let targetArea = (areaId !== undefined && areaId !== null)
                    ? pool.find(a => Number(a.id) === Number(areaId))
                    : null;
                if (!targetArea && targetCleanName) {
                    targetArea = pool.find(a => a && a.name && a.name.toLowerCase().includes(targetCleanName)) || null;
                }
                if (targetArea) {
                    const curArea = store.state.areaInfo;
                    if (curArea && Number(curArea.id) === Number(targetArea.id)) {
                        return 'same';
                    }
                    store.commit('updateAreaInfo', targetArea);
                    if (!isUnderground) {
                        if (typeof gisVm.toggleAreaLayer === 'function') {
                            gisVm.toggleAreaLayer();
                        } else if (gisVm.$refs && gisVm.$refs.filterPanel && typeof gisVm.$refs.filterPanel.toggleAreaLayer === 'function') {
                            gisVm.$refs.filterPanel.toggleAreaLayer();
                        }
                    }
                    setLog(tr('page.area.switched', { name: targetArea.name }), '#38bdf8');
                    return 'changed';
                }
            }

            // 2. Fallback DOM LI click (uses textContent so display:none items still match)
            const lis = Array.from(document.querySelectorAll('.area-list__dropdown li, .area-list li'));
            const activeLi = lis.find(li => li.classList && li.classList.contains('active'));
            const currentActiveText = activeLi ? (activeLi.querySelector('.area')?.textContent || '').trim().toLowerCase() : '';
            if (targetCleanName && currentActiveText && currentActiveText.includes(targetCleanName)) {
                return 'same';
            }
            let targetLi = null;
            if (targetCleanName) {
                targetLi = lis.find(li => {
                    const txt = (li.querySelector('.area')?.textContent || '').trim().toLowerCase();
                    return txt && (txt === targetCleanName || txt.includes(targetCleanName) || targetCleanName.includes(txt));
                });
            }
            if (targetLi && !isUnderground) {
                targetLi.click();
                setLog(tr('page.area.switched', { name: areaName || tr('page.area.fallback', { id: areaId }) }), '#38bdf8');
                return 'changed';
            }
        } catch (e) {
            console.error('[Genshin Helper] Error in ensureAreaSwitch:', e);
        }
        return 'none';
    }

    function findMatchingPointGroup(targetLat, targetLng, targetFloorName, totalFloors = null, activeFloorIdx = null) {
        if (targetLat === null || targetLng === null) return null;
        const gisEl = document.querySelector('.mhy-game-gis');
        const vm = gisEl?.__vue__;
        const list = (vm && Array.isArray(vm.pointGroupList) && vm.pointGroupList.length > 0)
            ? vm.pointGroupList
            : (vm && vm.$store && Array.isArray(vm.$store.state.pointGroupList) ? vm.$store.state.pointGroupList : []);

        if (!list || list.length === 0) return null;

        const cleanTargetFloor = (targetFloorName || '').trim().toUpperCase();
        const candidates = [];

        for (const g of list) {
            if (!g.floors || g.floors.length === 0) continue;

            let insideGroup = false;
            let bestFloorInGroup = null;
            let bestFloorScore = -9999;

            // Elevator has 1 surface floor (G) + g.floors.length underground floors
            const expectedTotal = g.floors.length + 1;
            let floorCountScore = 0;
            if (totalFloors) {
                const floorDiff = Math.abs(expectedTotal - totalFloors);
                if (floorDiff === 0) {
                    floorCountScore = 2000; // Perfect match on total elevator floors
                } else {
                    floorCountScore = -floorDiff * 500; // Heavy penalty for mismatch
                }
            }

            for (let fIdx = 0; fIdx < g.floors.length; fIdx++) {
                const f = g.floors[fIdx];
                if (!f.overlay) continue;

                const ov = f.overlay;
                const minLat = Math.min(ov.l_y, ov.r_y);
                const maxLat = Math.max(ov.l_y, ov.r_y);
                const minLng = Math.min(ov.l_x, ov.r_x);
                const maxLng = Math.max(ov.l_x, ov.r_x);

                // Margin tolerance 60m
                if (targetLng >= minLng - 60 && targetLng <= maxLng + 60 &&
                    targetLat >= minLat - 60 && targetLat <= maxLat + 60) {

                    insideGroup = true;
                    let fScore = 0;
                    const s = (f.floor_name_short || '').trim().toUpperCase();
                    const n = (f.floor_name || '').trim().toUpperCase();

                    // Floor naming matching
                    if (cleanTargetFloor) {
                        if (s === cleanTargetFloor) {
                            fScore += 500;
                        } else if (n.includes(cleanTargetFloor)) {
                            fScore += 300;
                        } else if (cleanTargetFloor === 'B1' && (fIdx === 0 || s === '')) {
                            // First underground floor represents B1 even if short name is empty
                            fScore += 400;
                        }
                    }

                    // Active floor index matching (in-game activeFloorIdx: 0=Surface, 1=Underground1, 2=Underground2...)
                    if (activeFloorIdx !== null && activeFloorIdx > 0 && fIdx === (activeFloorIdx - 1)) {
                        fScore += 400;
                    }

                    // Distance to center of this floor overlay
                    const centerLat = (minLat + maxLat) / 2;
                    const centerLng = (minLng + maxLng) / 2;
                    const distSq = (targetLat - centerLat) ** 2 + (targetLng - centerLng) ** 2;
                    const distScore = Math.max(0, 100 - Math.sqrt(distSq) / 10);
                    fScore += distScore;

                    if (fScore > bestFloorScore) {
                        bestFloorScore = fScore;
                        bestFloorInGroup = f;
                    }
                }
            }

            if (insideGroup && bestFloorInGroup) {
                candidates.push({
                    group: g,
                    defaultFloor: bestFloorInGroup,
                    score: floorCountScore + bestFloorScore
                });
            }
        }

        if (candidates.length > 0) {
            candidates.sort((a, b) => b.score - a.score);
            return { group: candidates[0].group, defaultFloor: candidates[0].defaultFloor, gisVm: vm };
        }
        return null;
    }

    function computeTargetLeafletZoom(map, data) {
        const minZ = (map && typeof map.getMinZoom === 'function') ? map.getMinZoom() : -3;
        const maxZ = (map && typeof map.getMaxZoom === 'function') ? map.getMaxZoom() : 2;
        let z = 0.8;
        if (data && typeof data.zoom_ratio_t === 'number' && !isNaN(data.zoom_ratio_t)) {
            const t = Math.max(0, Math.min(1, data.zoom_ratio_t));
            z = -1.5 + 3.5 * t;
        }
        return Math.max(minZ, Math.min(maxZ, z));
    }

    function unwrapAngle(prev, next) {
        if (typeof next !== 'number' || !Number.isFinite(next)) {
            return (typeof prev === 'number' && Number.isFinite(prev)) ? prev : 0;
        }
        if (typeof prev !== 'number' || !Number.isFinite(prev)) {
            return ((next % 360) + 360) % 360;
        }
        const delta = ((next - prev) % 360 + 540) % 360 - 180;
        return prev + delta;
    }

    function _buildPlayerMarkerMarkup() {
        return (
            '<div class="gh-player-marker-wrap">' +
                '<div class="gh-player-pulse-ring"></div>' +
                '<svg class="gh-player-fov-svg" viewBox="0 0 64 64" aria-hidden="true">' +
                    '<defs>' +
                        '<radialGradient id="gh-fov-grad" cx="50%" cy="50%" r="50%">' +
                            '<stop offset="0%" stop-color="#38bdf8" stop-opacity="0.46"/>' +
                            '<stop offset="68%" stop-color="#38bdf8" stop-opacity="0.16"/>' +
                            '<stop offset="100%" stop-color="#38bdf8" stop-opacity="0"/>' +
                        '</radialGradient>' +
                    '</defs>' +
                    '<path d="M32 32 L15.94 9.06 A28 28 0 0 1 48.06 9.06 Z" fill="url(#gh-fov-grad)"/>' +
                '</svg>' +
                '<svg class="gh-player-arrow-svg" viewBox="0 0 32 32" aria-hidden="true">' +
                    '<path d="M16 4.5 L25 24 L16 19.5 L7 24 Z" fill="#38bdf8" stroke="#ffffff" stroke-width="2" stroke-linejoin="round"/>' +
                '</svg>' +
            '</div>'
        );
    }

    function _createPlayerMarkerWrapNode() {
        const SVG_NS = 'http://www.w3.org/2000/svg';
        const wrap = document.createElement('div');
        wrap.className = 'gh-player-marker-wrap';

        const pulse = document.createElement('div');
        pulse.className = 'gh-player-pulse-ring';
        wrap.appendChild(pulse);

        const fovSvg = document.createElementNS(SVG_NS, 'svg');
        fovSvg.setAttribute('class', 'gh-player-fov-svg');
        fovSvg.setAttribute('viewBox', '0 0 64 64');
        fovSvg.setAttribute('aria-hidden', 'true');

        const defs = document.createElementNS(SVG_NS, 'defs');
        const grad = document.createElementNS(SVG_NS, 'radialGradient');
        grad.setAttribute('id', 'gh-fov-grad-dom');
        grad.setAttribute('cx', '50%');
        grad.setAttribute('cy', '50%');
        grad.setAttribute('r', '50%');

        const stops = [
            ['0%', '0.46'],
            ['68%', '0.16'],
            ['100%', '0']
        ];
        for (const [offset, opacity] of stops) {
            const stop = document.createElementNS(SVG_NS, 'stop');
            stop.setAttribute('offset', offset);
            stop.setAttribute('stop-color', '#38bdf8');
            stop.setAttribute('stop-opacity', opacity);
            grad.appendChild(stop);
        }
        defs.appendChild(grad);
        fovSvg.appendChild(defs);

        const fovPath = document.createElementNS(SVG_NS, 'path');
        fovPath.setAttribute('d', 'M32 32 L15.94 9.06 A28 28 0 0 1 48.06 9.06 Z');
        fovPath.setAttribute('fill', 'url(#gh-fov-grad-dom)');
        fovSvg.appendChild(fovPath);
        wrap.appendChild(fovSvg);

        const arrowSvg = document.createElementNS(SVG_NS, 'svg');
        arrowSvg.setAttribute('class', 'gh-player-arrow-svg');
        arrowSvg.setAttribute('viewBox', '0 0 32 32');
        arrowSvg.setAttribute('aria-hidden', 'true');

        const arrowPath = document.createElementNS(SVG_NS, 'path');
        arrowPath.setAttribute('d', 'M16 4.5 L25 24 L16 19.5 L7 24 Z');
        arrowPath.setAttribute('fill', '#38bdf8');
        arrowPath.setAttribute('stroke', '#ffffff');
        arrowPath.setAttribute('stroke-width', '2');
        arrowPath.setAttribute('stroke-linejoin', 'round');
        arrowSvg.appendChild(arrowPath);
        wrap.appendChild(arrowSvg);

        return wrap;
    }

    function _applyPlayerMarkerProps(wrapEl, data, unwrappedDir, hasCameraHeading, unwrappedRot) {
        if (!wrapEl) return;
        wrapEl.style.setProperty('--gh-dir', `${unwrappedDir.toFixed(2)}deg`);
        if (hasCameraHeading) {
            wrapEl.style.setProperty('--gh-rot', `${unwrappedRot.toFixed(2)}deg`);
            wrapEl.classList.add('gh-has-fov');
        } else {
            wrapEl.classList.remove('gh-has-fov');
        }
        const latStr = (data && typeof data.lat === 'number') ? data.lat.toFixed(1) : '?';
        const lngStr = (data && typeof data.lng === 'number') ? data.lng.toFixed(1) : '?';
        const floorTag = (data && data.is_underground && (data.floor_short || data.floor_name))
            ? ` • ${data.floor_short || data.floor_name}`
            : '';
        wrapEl.setAttribute('title', tr('page.marker.title', { lat: latStr, lng: lngStr, floor: floorTag }));
    }

    function _repositionDomPlayerMarker() {
        try {
            if (!playerDomMarkerEl || !playerDomMapRef || !playerLastState) return;
            if (typeof playerDomMapRef.latLngToLayerPoint === 'function') {
                const pt = playerDomMapRef.latLngToLayerPoint([playerLastState.lat, playerLastState.lng]);
                playerDomMarkerEl.style.transform = `translate3d(${(pt.x - 22).toFixed(2)}px, ${(pt.y - 22).toFixed(2)}px, 0)`;
            }
        } catch(e) {}
    }

    // HoYoLAB draws every pin into a single <canvas class="leaflet-canvas-icon-layer"> inside
    // markerPane with `z-index: 9999`, while Leaflet gives our marker `z-index = y + offset`
    // (≈ 5000) in the same pane — so the player arrow always sank under the pin icons. A
    // dedicated pane above markerPane (600) and tooltipPane (650) but below popupPane (700)
    // keeps the arrow on top of every pin without covering pin popups.
    const PLAYER_PANE_NAME = 'ghPlayerPane';
    const PLAYER_PANE_Z = 690;

    function _ensurePlayerPane(map) {
        try {
            if (!map || typeof map.getPane !== 'function') return null;
            let pane = map.getPane(PLAYER_PANE_NAME);
            if (!pane && typeof map.createPane === 'function') {
                pane = map.createPane(PLAYER_PANE_NAME);
            }
            if (pane) {
                pane.style.zIndex = String(PLAYER_PANE_Z);
                pane.style.pointerEvents = 'none';
            }
            return pane || null;
        } catch (e) {
            return null;
        }
    }

    function _cleanupOrphanPlayerMarkers(keepEl = null) {
        try {
            const nodes = document.querySelectorAll('#gh-player-marker-dom, .gh-player-leaflet-icon, .gh-player-marker-wrap');
            for (let i = 0; i < nodes.length; i++) {
                const node = nodes[i];
                if (!node || !node.isConnected) continue;
                let host = node;
                if (node.classList && node.classList.contains('gh-player-marker-wrap')) {
                    host = node.closest('#gh-player-marker-dom, .gh-player-leaflet-icon') || node;
                }
                if (keepEl && (host === keepEl || (typeof keepEl.contains === 'function' && keepEl.contains(host)))) {
                    continue;
                }
                try { host.remove(); } catch (e) {}
            }
        } catch (e) {}
    }

    function updatePlayerMarker(ctx, data) {
        try {
            if (!ctx || !ctx.map || !data || typeof data.lat !== 'number' || typeof data.lng !== 'number') return;
            injectThemeStyles();
            bindDragUnpinListeners(ctx);

            const rawDir = (typeof data.heading === 'number' && Number.isFinite(data.heading)) ? data.heading : null;
            if (rawDir !== null) {
                playerUnwrappedDir = unwrapAngle(playerUnwrappedDir, rawDir);
            } else if (playerUnwrappedDir === null) {
                playerUnwrappedDir = 0;
            }

            const rawCam = (typeof data.camera_heading === 'number' && Number.isFinite(data.camera_heading))
                ? data.camera_heading
                : null;
            const hasCameraHeading = rawCam !== null;
            if (hasCameraHeading) {
                playerUnwrappedRot = unwrapAngle(playerUnwrappedRot, rawCam);
            }

            const resolvedMarkerMapId = data.map_id ? parseInt(data.map_id, 10) : getCurrentWebMapId();
            const samePrevMap = Boolean(playerLastState && Number(playerLastState.map_id || 2) === resolvedMarkerMapId);
            playerLastState = {
                lat: data.lat,
                lng: data.lng,
                heading: rawDir !== null ? rawDir : (playerLastState ? playerLastState.heading : 0),
                camera_heading: hasCameraHeading ? rawCam : null,
                is_underground: Boolean(data.is_underground),
                floor_id: data.floor_id !== undefined ? data.floor_id : (samePrevMap ? playerLastState.floor_id : null),
                floor_short: data.floor_short || data.floor_name || null,
                map_id: resolvedMarkerMapId,
                area_id: (data.area_id !== undefined && data.area_id !== null)
                    ? Number(data.area_id)
                    : (samePrevMap ? playerLastState.area_id : null),
                area_name: data.area_name || (samePrevMap ? playerLastState.area_name : null)
            };

            // Primary path: native Leaflet L.marker created ONCE, updated in-place via setLatLng + CSS custom properties
            if (window.L && typeof window.L.marker === 'function' && typeof window.L.divIcon === 'function') {
                if (playerDomMarkerEl) {
                    if (playerDomMapRef && typeof playerDomMapRef.off === 'function') {
                        playerDomMapRef.off('move zoom viewreset zoomend', _repositionDomPlayerMarker);
                    }
                    playerDomMarkerEl.remove();
                    playerDomMarkerEl = null;
                    playerDomWrapEl = null;
                    playerDomMapRef = null;
                }
                const playerPane = _ensurePlayerPane(ctx.map);
                if (playerMarkerLayer && (playerMarkerMapRef !== ctx.map || (playerPane && playerMarkerLayer.options.pane !== PLAYER_PANE_NAME))) {
                    try { playerMarkerLayer.remove(); } catch(e) {}
                    playerMarkerLayer = null;
                    playerMarkerWrapEl = null;
                }
                const createdLayer = !playerMarkerLayer;
                if (!playerMarkerLayer) {
                    const icon = window.L.divIcon({
                        className: 'gh-player-leaflet-icon',
                        html: _buildPlayerMarkerMarkup(),
                        iconSize: [44, 44],
                        iconAnchor: [22, 22]
                    });
                    const markerOpts = {
                        icon: icon,
                        interactive: false,
                        zIndexOffset: 9000
                    };
                    if (playerPane) markerOpts.pane = PLAYER_PANE_NAME;
                    playerMarkerLayer = window.L.marker([data.lat, data.lng], markerOpts).addTo(ctx.map);
                    playerMarkerMapRef = ctx.map;
                    const el = typeof playerMarkerLayer.getElement === 'function' ? playerMarkerLayer.getElement() : null;
                    playerMarkerWrapEl = el ? el.querySelector('.gh-player-marker-wrap') : null;
                } else {
                    playerMarkerLayer.setLatLng([data.lat, data.lng]);
                    if (!playerMarkerWrapEl || !playerMarkerWrapEl.isConnected) {
                        const el = typeof playerMarkerLayer.getElement === 'function' ? playerMarkerLayer.getElement() : null;
                        playerMarkerWrapEl = el ? el.querySelector('.gh-player-marker-wrap') : null;
                    }
                }
                const activeEl = typeof playerMarkerLayer.getElement === 'function' ? playerMarkerLayer.getElement() : null;
                if (createdLayer || document.getElementsByClassName('gh-player-marker-wrap').length > 1) {
                    _cleanupOrphanPlayerMarkers(activeEl);
                }
                _applyPlayerMarkerProps(
                    playerMarkerWrapEl,
                    data,
                    playerUnwrappedDir,
                    hasCameraHeading,
                    playerUnwrappedRot || 0
                );
                return;
            }

            // Fallback path for bundled HoYoLAB Leaflet (created ONCE in the top player pane, updated in-place via translate3d + CSS vars)
            if (playerMarkerLayer) {
                try { playerMarkerLayer.remove(); } catch (e) {}
                playerMarkerLayer = null;
                playerMarkerMapRef = null;
                playerMarkerWrapEl = null;
            }
            const pane = _ensurePlayerPane(ctx.map)
                || ((typeof ctx.map.getPanes === 'function' && ctx.map.getPanes() && ctx.map.getPanes().markerPane)
                    ? ctx.map.getPanes().markerPane
                    : (typeof ctx.map.getContainer === 'function' ? ctx.map.getContainer() : null));
            if (!pane) return;

            const createdDom = (!playerDomMarkerEl || !playerDomMarkerEl.isConnected || playerDomMapRef !== ctx.map || playerDomMarkerEl.parentNode !== pane);
            if (createdDom) {
                if (playerDomMapRef && playerDomMapRef !== ctx.map && typeof playerDomMapRef.off === 'function') {
                    playerDomMapRef.off('move zoom viewreset zoomend', _repositionDomPlayerMarker);
                }
                if (playerDomMarkerEl) playerDomMarkerEl.remove();
                playerDomMarkerEl = document.createElement('div');
                playerDomMarkerEl.id = 'gh-player-marker-dom';
                playerDomMarkerEl.style.cssText = 'position:absolute;left:0;top:0;width:44px;height:44px;z-index:9000;pointer-events:none;will-change:transform;';
                playerDomWrapEl = _createPlayerMarkerWrapNode();
                playerDomMarkerEl.appendChild(playerDomWrapEl);
                pane.appendChild(playerDomMarkerEl);
                if (playerDomMapRef !== ctx.map && typeof ctx.map.on === 'function') {
                    playerDomMapRef = ctx.map;
                    ctx.map.on('move zoom viewreset zoomend', _repositionDomPlayerMarker);
                } else {
                    playerDomMapRef = ctx.map;
                }
            }
            if (createdDom || document.getElementsByClassName('gh-player-marker-wrap').length > 1) {
                _cleanupOrphanPlayerMarkers(playerDomMarkerEl);
            }
            _applyPlayerMarkerProps(
                playerDomWrapEl,
                data,
                playerUnwrappedDir,
                hasCameraHeading,
                playerUnwrappedRot || 0
            );
            _repositionDomPlayerMarker();
        } catch (e) {
            console.error('[Genshin Helper] Error in updatePlayerMarker:', e);
        }
    }

    function selectElevatorFloorExact(data, targetFloor = null) {
        try {
            const elevator = document.querySelector('#underground-elevator, .underground-elevator');
            if (!elevator || elevator.offsetWidth === 0) return false;
            const domFloors = Array.from(elevator.querySelectorAll('.underground-elevator__floor'));
            if (domFloors.length === 0) return false;

            const elevatorVm = elevator.__vue__;
            const vmFloors = elevatorVm && Array.isArray(elevatorVm.floors) ? elevatorVm.floors : [];
            const targetFloorId = (targetFloor && targetFloor.id !== undefined) ? targetFloor.id : (data ? data.floor_id : null);

            let chosen = null;
            if (targetFloorId !== null && targetFloorId !== undefined && vmFloors.length === domFloors.length) {
                const vmIdx = vmFloors.findIndex(ef => ef && Number(ef.id) === Number(targetFloorId));
                if (vmIdx >= 0 && domFloors[vmIdx]) {
                    chosen = domFloors[vmIdx];
                }
            }

            if (!chosen) {
                const shortWant = ((targetFloor && (targetFloor.floor_name_short || targetFloor.floor_short)) || (data && (data.floor_short || data.floor_name)) || '').trim().toUpperCase();
                if (shortWant) {
                    chosen = domFloors.find(f => (f.innerText || f.textContent || '').trim().toUpperCase() === shortWant);
                }
            }

            if (!chosen && data && data.active_floor_idx !== undefined && data.active_floor_idx >= 0 && data.active_floor_idx < domFloors.length) {
                chosen = domFloors[data.active_floor_idx];
            }

            if (chosen) {
                dispatchFullClick(chosen);
                return true;
            }
        } catch(e) {}
        return false;
    }

    function forceRefreshCaveCanvas(gisVm) {
        try {
            const layer = gisVm && gisVm.canvasLayeredImageLayer;
            if (!layer) return false;
            if (typeof layer.setBackground === 'function') {
                layer.setBackground('rgba(0, 0, 0, 0.5)');
            }
            if (typeof layer._reset === 'function') {
                layer._reset();
            } else if (typeof layer.redraw === 'function') {
                layer.redraw();
            }
            if (layer._imageLookup && typeof layer._imageLookup === 'object') {
                Object.values(layer._imageLookup).forEach(entry => {
                    const img = Array.isArray(entry) ? entry[0] : null;
                    if (img && !img.complete && !img.__gh_onload_bound) {
                        img.__gh_onload_bound = true;
                        img.addEventListener('load', () => {
                            try {
                                if (typeof layer.setBackground === 'function') layer.setBackground('rgba(0, 0, 0, 0.5)');
                                if (typeof layer._reset === 'function') layer._reset();
                            } catch (e) {}
                        }, { once: true });
                    }
                });
            }
            return true;
        } catch (e) {
            return false;
        }
    }

    function triggerElevatorGToFloorKickstart(gisVm, resolvedFloor, afterDone) {
        try {
            const el = document.querySelector('#underground-elevator, .underground-elevator');
            const vm = el ? el.__vue__ : null;
            if (vm && Array.isArray(vm.floors) && typeof vm.onSwitchFloor === 'function') {
                const gIdx = vm.floors.findIndex(f => f && (f.floor_index === -1 || (f.floor_name_short || '').trim().toUpperCase() === 'G'));
                let tIdx = resolvedFloor
                    ? vm.floors.findIndex(f => f && Number(f.id) === Number(resolvedFloor.id))
                    : -1;
                if (tIdx < 0 && resolvedFloor) {
                    const wantShort = (resolvedFloor.floor_name_short || '').trim().toUpperCase();
                    if (wantShort) {
                        tIdx = vm.floors.findIndex(f => f && (f.floor_name_short || '').trim().toUpperCase() === wantShort);
                    }
                }
                if (gIdx >= 0 && tIdx >= 0 && gIdx !== tIdx) {
                    vm.onSwitchFloor(gIdx, vm.floors[gIdx]);
                    scheduleLayerTimer(() => {
                        vm.onSwitchFloor(tIdx, vm.floors[tIdx]);
                        scheduleLayerTimer(() => {
                            forceRefreshCaveCanvas(gisVm);
                            if (typeof afterDone === 'function') afterDone();
                        }, 220);
                    }, 560);
                    return true;
                }
            }
            const domFloors = el ? Array.from(el.querySelectorAll('.underground-elevator__floor')) : [];
            const domG = domFloors.find(f => (f.innerText || f.textContent || '').trim().toUpperCase() === 'G');
            if (domG) {
                dispatchFullClick(domG);
                scheduleLayerTimer(() => {
                    selectElevatorFloorExact({}, resolvedFloor);
                    scheduleLayerTimer(() => {
                        forceRefreshCaveCanvas(gisVm);
                        if (typeof afterDone === 'function') afterDone();
                    }, 220);
                }, 560);
                return true;
            }
        } catch (e) {}
        return false;
    }

    function handleLayeredExploration(ctx, data, targetZoom = null, areaChanged = false) {
        try {
            clearLayerTimers();
            const isUnderground = Boolean(data && data.is_underground);
            const gisVm = resolveGisVm(ctx);
            const finalZoom = (targetZoom !== null && targetZoom !== undefined)
                ? targetZoom
                : computeTargetLeafletZoom(ctx ? ctx.map : null, data);

            const applyViewAndMarker = (animate = false) => {
                if (ctx && ctx.map && data && data.lat !== undefined && data.lng !== undefined) {
                    if (autoPanEnabled) {
                        const allowAnim = Boolean(animate) && !document.hidden;
                        ctx.map.setView([data.lat, data.lng], finalZoom, {
                            animate: allowAnim,
                            duration: allowAnim ? 0.25 : 0
                        });
                        lastSyncedView = {
                            lat: data.lat,
                            lng: data.lng,
                            zoom: finalZoom,
                            mapId: data.map_id ? parseInt(data.map_id, 10) : 2
                        };
                        if (document.hidden) {
                            pendingVisibleRefresh = true;
                        }
                    }
                    updatePlayerMarker(ctx, data);
                }
            };

            const activeInfo = gisVm && gisVm.$store && gisVm.$store.state ? gisVm.$store.state.layeredExplorationInfo : null;
            const hasCanvasLayer = Boolean(gisVm && gisVm.canvasLayeredImageLayer);
            const elevatorEl = document.querySelector('#underground-elevator, .underground-elevator');
            const inExploration = Boolean(activeInfo || hasCanvasLayer || (elevatorEl && elevatorEl.offsetWidth > 0));

            // Case 1: Surface
            if (!isUnderground) {
                if (inExploration) {
                    if (ctx && ctx.map && typeof ctx.map.setMaxBounds === 'function') {
                        ctx.map.setMaxBounds(null);
                    }
                    if (ctx && ctx.map && gisVm && gisVm.setMaxBoundsFn && typeof ctx.map.off === 'function') {
                        ctx.map.off('zoomend', gisVm.setMaxBoundsFn);
                    }
                    if (gisVm && typeof gisVm.onQuitExploreAPointGroup === 'function') {
                        setLog(tr('page.layer.exit_surface'), '#38bdf8');
                        Promise.race([
                            Promise.resolve(gisVm.onQuitExploreAPointGroup()).catch(err => console.error('[Genshin Helper] onQuitExploreAPointGroup error:', err)),
                            new Promise(r => setTimeout(r, 1500))
                        ]).finally(() => {
                            applyViewAndMarker(false);
                            scheduleLayerTimer(() => applyViewAndMarker(false), 480);
                        });
                    } else {
                        exitUndergroundExploration();
                        scheduleLayerTimer(() => applyViewAndMarker(false), 240);
                    }
                } else if (areaChanged && ctx && ctx.map && data && data.lat !== undefined && data.lng !== undefined) {
                    const guardDrift = () => {
                        try {
                            if (!ctx || !ctx.map || typeof ctx.map.getCenter !== 'function') return;
                            const c = ctx.map.getCenter();
                            if (c && Math.hypot(c.lat - data.lat, c.lng - data.lng) > 15) {
                                applyViewAndMarker(false);
                            }
                        } catch (e) {}
                    };
                    scheduleLayerTimer(guardDrift, 220);
                    scheduleLayerTimer(guardDrift, 520);
                }
                return;
            }

            // Case 2: Underground / Multilayer (prefer exact group_id + floor_id from backend)
            const list = (gisVm && Array.isArray(gisVm.pointGroupList) && gisVm.pointGroupList.length > 0)
                ? gisVm.pointGroupList
                : (gisVm && gisVm.$store && Array.isArray(gisVm.$store.state.pointGroupList) ? gisVm.$store.state.pointGroupList : []);

            if (list.length === 0) {
                if (!data.__gh_pg_retried) {
                    data.__gh_pg_retried = true;
                    scheduleLayerTimer(() => handleLayeredExploration(ctx, data, targetZoom), 450);
                }
                return;
            }

            let group = null;
            let targetFloor = null;
            if (data.group_id !== undefined && data.group_id !== null) {
                group = list.find(g => Number(g.id) === Number(data.group_id)) || null;
            }
            if (!group && data.lat !== undefined && data.lng !== undefined) {
                const match = findMatchingPointGroup(data.lat, data.lng, data.floor_short || data.floor_name, data.total_floors, data.active_floor_idx);
                if (match && match.group) {
                    group = match.group;
                    targetFloor = match.defaultFloor || null;
                }
            }

            if (group && Array.isArray(group.floors) && group.floors.length > 0) {
                if (data.floor_id !== undefined && data.floor_id !== null) {
                    targetFloor = group.floors.find(f => Number(f.id) === Number(data.floor_id)) || targetFloor;
                }
                if (!targetFloor) {
                    const cleanFloor = (data.floor_short || data.floor_name || '').trim().toUpperCase();
                    if (cleanFloor) {
                        targetFloor = group.floors.find((f, fIdx) => {
                            const s = (f.floor_name_short || '').trim().toUpperCase();
                            const n = (f.floor_name || '').trim().toUpperCase();
                            return s === cleanFloor || n.includes(cleanFloor) || (cleanFloor === 'B1' && fIdx === 0);
                        }) || null;
                    }
                }
                if (!targetFloor && data.active_floor_idx !== undefined && data.active_floor_idx > 0) {
                    targetFloor = group.floors[data.active_floor_idx - 1] || null;
                }
                if (!targetFloor) targetFloor = group.floors[0];

                let floorIndex = targetFloor
                    ? group.floors.findIndex(f => Number(f.id) === Number(targetFloor.id))
                    : -1;
                if (floorIndex < 0 && targetFloor) floorIndex = group.floors.indexOf(targetFloor);
                floorIndex = Math.max(0, floorIndex);
                const resolvedFloor = group.floors[floorIndex] || targetFloor;
                const label = resolvedFloor.floor_name || resolvedFloor.floor_name_short || group.name || tr('page.layer.default_underground');

                const elevatorVm = elevatorEl ? elevatorEl.__vue__ : null;
                const elevatorFloors = elevatorVm?.floors || [];
                const isSameGroup = Boolean(
                    (activeInfo && Number(activeInfo.id) === Number(group.id)) ||
                    (Array.isArray(elevatorFloors) && elevatorFloors.length > 0 &&
                        group.floors.some(gf => elevatorFloors.some(ef => ef && Number(ef.id) === Number(gf.id))))
                );

                // Already inside the same group AND cave image overlay is mounted -> switch floor cleanly
                if (isSameGroup && hasCanvasLayer) {
                    const alreadyOnFloor = Boolean(
                        activeInfo && (
                            Number(activeInfo.activeFloorId) === Number(resolvedFloor.id) ||
                            Number(activeInfo.activeIndex) === floorIndex
                        ) && (!elevatorVm || Number(elevatorVm.activeFloorId) === Number(resolvedFloor.id))
                    );
                    if (!alreadyOnFloor) {
                        const elevTIdx = (elevatorVm && Array.isArray(elevatorVm.floors))
                            ? elevatorVm.floors.findIndex(f => f && Number(f.id) === Number(resolvedFloor.id))
                            : -1;
                        if (elevTIdx >= 0 && typeof elevatorVm.onSwitchFloor === 'function') {
                            elevatorVm.onSwitchFloor(elevTIdx, elevatorVm.floors[elevTIdx]);
                            setLog(tr('page.layer.switch_floor', { label }), '#22c55e');
                            scheduleLayerTimer(() => { applyViewAndMarker(false); forceRefreshCaveCanvas(gisVm); }, 180);
                            scheduleLayerTimer(() => { applyViewAndMarker(false); forceRefreshCaveCanvas(gisVm); }, 600);
                            return;
                        }
                        if (gisVm && typeof gisVm.onSwitchExploreFloor === 'function') {
                            const floorPayload = Object.assign({}, resolvedFloor, {
                                name: resolvedFloor.floor_name || resolvedFloor.floor_name_short || ''
                            });
                            Promise.resolve(gisVm.onSwitchExploreFloor(floorPayload))
                                .catch(() => selectElevatorFloorExact(data, resolvedFloor))
                                .finally(() => {
                                    applyViewAndMarker(false);
                                    forceRefreshCaveCanvas(gisVm);
                                    scheduleLayerTimer(() => {
                                        applyViewAndMarker(false);
                                        forceRefreshCaveCanvas(gisVm);
                                    }, 600);
                                });
                            setLog(tr('page.layer.switch_floor', { label }), '#22c55e');
                            return;
                        }
                        selectElevatorFloorExact(data, resolvedFloor);
                    }
                    forceRefreshCaveCanvas(gisVm);
                    const hasTiles = Boolean(
                        gisVm.canvasLayeredImageLayer &&
                        gisVm.canvasLayeredImageLayer._latlngImageOverlays &&
                        gisVm.canvasLayeredImageLayer._latlngImageOverlays.total > 0
                    );
                    if (!hasTiles) {
                        triggerElevatorGToFloorKickstart(gisVm, resolvedFloor, () => applyViewAndMarker(false));
                    } else {
                        scheduleLayerTimer(() => {
                            applyViewAndMarker(false);
                            forceRefreshCaveCanvas(gisVm);
                        }, 160);
                    }
                    return;
                }

                // IMPORTANT: onEnterExploreAPointGroup(group, floorIndex) takes INTEGER floorIndex (0, 1, ...)
                const doEnter = () => {
                    if (gisVm && typeof gisVm.onEnterExploreAPointGroup === 'function') {
                        setLog(tr('page.layer.enter_explore', { label }), '#22c55e');
                        Promise.resolve(gisVm.onEnterExploreAPointGroup(group, floorIndex))
                            .catch(err => console.error('[Genshin Helper] onEnterExploreAPointGroup error:', err))
                            .finally(() => {
                                applyViewAndMarker(false);
                                forceRefreshCaveCanvas(gisVm);
                                scheduleLayerTimer(() => {
                                    applyViewAndMarker(false);
                                    forceRefreshCaveCanvas(gisVm);
                                    const activeNow = gisVm && gisVm.$store && gisVm.$store.state ? gisVm.$store.state.layeredExplorationInfo : null;
                                    if ((!activeNow || Number(activeNow.id) !== Number(group.id)) && !doEnter._retried) {
                                        doEnter._retried = true;
                                        applyViewAndMarker(false);
                                        doEnter();
                                        return;
                                    }
                                    const layerOk = Boolean(
                                        gisVm.canvasLayeredImageLayer &&
                                        gisVm.canvasLayeredImageLayer._latlngImageOverlays &&
                                        gisVm.canvasLayeredImageLayer._latlngImageOverlays.total > 0
                                    );
                                    const elevNow = document.querySelector('#underground-elevator, .underground-elevator')?.__vue__;
                                    const elevOk = !elevNow || Number(elevNow.activeFloorId) === Number(resolvedFloor.id);
                                    if (!layerOk || !elevOk) {
                                        triggerElevatorGToFloorKickstart(gisVm, resolvedFloor, () => applyViewAndMarker(false));
                                    }
                                }, 580);
                            });
                    }
                };

                if (inExploration) {
                    if (ctx && ctx.map && typeof ctx.map.setMaxBounds === 'function') {
                        ctx.map.setMaxBounds(null);
                    }
                    if (ctx && ctx.map && gisVm && gisVm.setMaxBoundsFn && typeof ctx.map.off === 'function') {
                        ctx.map.off('zoomend', gisVm.setMaxBoundsFn);
                    }
                    if (gisVm && typeof gisVm.onQuitExploreAPointGroup === 'function') {
                        Promise.race([
                            Promise.resolve(gisVm.onQuitExploreAPointGroup()).catch(() => {}),
                            new Promise(r => setTimeout(r, 1500))
                        ]).finally(() => {
                            scheduleLayerTimer(() => {
                                applyViewAndMarker(false);
                                doEnter();
                            }, 180);
                        });
                        return;
                    }
                    exitUndergroundExploration();
                    scheduleLayerTimer(() => {
                        applyViewAndMarker(false);
                        doEnter();
                    }, 240);
                    return;
                }

                doEnter();
                return;
            }

            if (elevatorEl && elevatorEl.offsetWidth > 0) {
                selectElevatorFloorExact(data, null);
            }
        } catch (e) {
            console.error('[Genshin Helper] Error in handleLayeredExploration:', e);
        }
    }

    // =========================================================================
    // 3.1 Universal Underground Floor Switcher (Index & Name 1:1 Matching + Kickstart)
    // =========================================================================
    function dispatchFullClick(el) {
        if (!el) return;
        try {
            const opts = { bubbles: true, cancelable: true, view: window };
            el.dispatchEvent(new MouseEvent('mouseenter', opts));
            el.dispatchEvent(new MouseEvent('mousedown', opts));
            el.dispatchEvent(new MouseEvent('mouseup', opts));
            el.dispatchEvent(new MouseEvent('click', opts));
        } catch (e) {
            try { el.click(); } catch(err) {}
        }
    }

    function switchUndergroundFloor(floorIdx, totalFloors, floorName, forceKickstart = false) {
        try {
            const elevator = document.querySelector('#underground-elevator, .underground-elevator');
            if (elevator && elevator.offsetWidth > 0) {
                const floors = Array.from(elevator.querySelectorAll('.underground-elevator__floor'));
                if (floors.length > 0) {
                    let target = null;
                    if (floorName) {
                        const cleanName = floorName.toUpperCase().trim();
                        target = floors.find(f => (f.innerText || f.textContent || '').trim().toUpperCase() === cleanName);
                    }
                    if (!target && floorIdx !== undefined && floorIdx >= 0 && floorIdx < floors.length) {
                        target = floors[floorIdx];
                    }
                    if (!target) {
                        target = floors.find(f => (f.innerText || f.textContent || '').trim().toUpperCase() !== 'G') || floors[floors.length - 1];
                    }

                    if (target) {
                        const txt = (target.innerText || target.textContent || '').trim();
                        const floorG = floors.find(f => (f.innerText || f.textContent || '').trim().toUpperCase() === 'G');

                        // Kickstart toggle: Clicking G then clicking Target Floor forces HoYoLAB Leaflet to mount tiles
                        const floorDisplay = txt || tr('page.layer.floor_fallback', { idx: floorIdx });
                        if (forceKickstart && floorG && floorG !== target) {
                            dispatchFullClick(floorG);
                            setTimeout(() => {
                                dispatchFullClick(target);
                                setLog(tr('page.layer.activated_floor', { label: floorDisplay }), '#22c55e');
                            }, 130);
                        } else {
                            dispatchFullClick(target);
                            setLog(tr('page.layer.selected_floor', { label: floorDisplay }), '#22c55e');
                        }
                        return true;
                    }
                }
            }
        } catch (e) {
            console.error('[Genshin Helper] Error in switchUndergroundFloor:', e);
        }
        return false;
    }

    function ensureFloorSwitch(floorIdx, totalFloors, floorName, ctx = null, targetLat = null, targetLng = null, maxAttempts = 12) {
        let count = 0;
        let kickstarted = false;
        const timer = setInterval(() => {
            count++;
            // On attempt 2, trigger Kickstart Toggle (G -> Target Floor) to unlock Leaflet tiles
            const shouldKickstart = (!kickstarted && count >= 2);
            const ok = switchUndergroundFloor(floorIdx, totalFloors, floorName, shouldKickstart);
            if (shouldKickstart && ok) {
                kickstarted = true;
            }
            if ((ok && count >= 4) || count >= maxAttempts) {
                clearInterval(timer);
                if (ctx && ctx.map && targetLat !== null && targetLng !== null && autoPanEnabled) {
                    setTimeout(() => {
                        const curZoom = ctx.map.getZoom();
                        const finalZoom = (curZoom < -0.5 || curZoom > 2.0) ? 0.8 : curZoom;
                        const allowAnim = !document.hidden;
                        ctx.map.setView([targetLat, targetLng], finalZoom, { animate: allowAnim, duration: allowAnim ? 0.5 : 0 });
                    }, 250);
                }
            }
        }, 180);
    }

    function exitUndergroundExploration() {
        try {
            const elevator = document.querySelector('#underground-elevator, .underground-elevator');
            const vm = elevator ? elevator.__vue__ : null;
            if (vm && typeof vm.onClickExit === 'function') {
                vm.onClickExit();
                setLog(tr('page.layer.exit_surface'), '#38bdf8');
                return true;
            }
            const exitBtn = document.querySelector('.underground-elevator__exit, [class*="underground-elevator__exit"], .underground-elevator [class*="exit"], #underground-elevator [class*="exit"]');
            if (exitBtn) {
                exitBtn.click();
                setLog(tr('page.layer.exit_surface'), '#38bdf8');
                return true;
            }
        } catch (e) {}
        return false;
    }

    // =========================================================================
    // 4. Extension Bridge Events & Live State Sync
    // =========================================================================
    function warnServerVersionMismatch(data) {
        if (data && data.script_version && data.script_version !== SCRIPT_VERSION && !warnedVersionMismatch) {
            warnedVersionMismatch = true;
            console.warn(`[Genshin Helper] Phiên bản Desktop (${data.script_version}) khác Extension (${SCRIPT_VERSION}).`);
        }
    }

    const onWindowMessage = (e) => {
        if (window.__gh_booted_build !== SCRIPT_BUILD) return;
        if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-page') {
            return;
        }
        const msg = e.data;

        if (msg.reqId && (msg.kind === 'API_RESPONSE' || Object.prototype.hasOwnProperty.call(msg, 'response'))) {
            const entry = pendingExtRequests.get(msg.reqId);
            if (!entry) return;
            pendingExtRequests.delete(msg.reqId);
            clearTimeout(entry.timer);
            const resp = msg.response;
            if (resp && resp.ok) {
                entry.resolve(resp.data);
            } else {
                const err = new Error((resp && resp.error) || `HTTP ${(resp && resp.status) || 0}`);
                err.status = (resp && resp.status) || 0;
                err.data = (resp && resp.data) || null;
                entry.reject(err);
            }
            return;
        }

        if (msg.kind === 'CONNECTION_STATUS') {
            const wasConnected = isConnected;
            const nextEngine = typeof msg.engineActive === 'boolean' ? msg.engineActive : engineActive;
            setStatus(Boolean(msg.connected), nextEngine);
            if (msg.connected && (!wasConnected || !initialStatusSynced)) {
                fetchInitialStatus();
                if (!wasConnected) {
                    syncInitialRouteState();
                }
            }
            return;
        }

        if (msg.kind === 'BRIDGE_READY') {
            if (window.GHI18n && typeof window.GHI18n.isReady === 'function' && !window.GHI18n.isReady()) {
                try {
                    window.postMessage({ __gh_ext: true, dir: 'to-isolated', kind: 'I18N_REQUEST' }, window.location.origin);
                } catch (err) {}
            }
            if (!initialStatusSynced) {
                fetchInitialStatus();
            }
            return;
        }

        if (msg.kind === 'COMPACT_UI_SYNC' && typeof msg.enabled === 'boolean') {
            setCompactUI(msg.enabled, { syncStorage: false });
            return;
        }

        const directCmd = msg.kind || msg.type;
        if (
            directCmd === 'GH_CMD_PLAN_TSP' ||
            directCmd === 'GH_CMD_CLEAR_TSP' ||
            directCmd === 'GH_CMD_EXPORT_PINS' ||
            directCmd === 'GH_CMD_ROUTE_EXPORT' ||
            directCmd === 'GH_CMD_ROUTE_IMPORT' ||
            directCmd === 'GH_CMD_ROUTE_CLEAR'
        ) {
            handleLocalCommand({ ...msg, type: directCmd });
            return;
        }

        if (msg.kind === 'SERVER_EVENT' && msg.data) {
            const data = msg.data;
            if (
                data._local_cmd === true ||
                data.type === 'GH_CMD_PLAN_TSP' ||
                data.type === 'GH_CMD_CLEAR_TSP' ||
                data.type === 'GH_CMD_EXPORT_PINS' ||
                data.type === 'GH_CMD_ROUTE_EXPORT' ||
                data.type === 'GH_CMD_ROUTE_IMPORT' ||
                data.type === 'GH_CMD_ROUTE_CLEAR'
            ) {
                handleLocalCommand(data);
                return;
            }
            setStatus(true);
            warnServerVersionMismatch(data);
            if (typeof data._seq === 'number') {
                lastEventSeq = data._seq;
            }
            if (data.type && data.type !== 'HEARTBEAT') {
                try {
                    handleServerMessage(data);
                } catch (err) {
                    console.error('[Genshin Helper] handleServerMessage error:', err);
                }
            }
        }
    };
    window.addEventListener('message', onWindowMessage);

    function sendActionFeedback(eventType, status, title = '', detail = '', seq = 0) {
        apiRequest('/action_feedback', {
            method: 'POST',
            body: {
                event_type: eventType,
                status: status,
                title: title || '',
                detail: detail || '',
                seq: typeof seq === 'number' ? seq : 0
            }
        }).catch(() => {});
    }

    const onVisibilityChange = () => {
        if (document.hidden || !pendingVisibleRefresh) return;
        pendingVisibleRefresh = false;
        try {
            const ctx = findMapContext();
            if (ctx && ctx.map) {
                if (typeof ctx.map.invalidateSize === 'function') {
                    ctx.map.invalidateSize({ animate: false });
                }
                if (!routeInspecting && lastSyncedView && getCurrentWebMapId() === Number(lastSyncedView.mapId || 2)) {
                    ctx.map.setView([lastSyncedView.lat, lastSyncedView.lng], lastSyncedView.zoom, { animate: false });
                }
                _repositionDomPlayerMarker();
                _scheduleTspRouteRedraw();
                forceRefreshCaveCanvas(resolveGisVm(ctx));
            }
        } catch (e) {}
    };
    document.addEventListener('visibilitychange', onVisibilityChange);

    // Page Visibility heartbeat: lets the app decide per sync event whether the user can see
    // this tab (page gives feedback) or not (in-game HUD toast + chime). Replaces the manual
    // "Single-Monitor Mode" toggle. Visible reports expire app-side after ~20 s.
    const GH_TAB_ID = `tab_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
    function reportTabVisibility(forceHidden = false, opts = {}) {
        const isVisible = !forceHidden && !document.hidden;
        const isFocused = isVisible && typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false;
        apiRequest('/tab_visibility', {
            method: 'POST',
            body: {
                tab_id: GH_TAB_ID,
                visible: isVisible,
                focused: isFocused,
                interacted: Boolean(opts && opts.interacted),
                closing: Boolean(opts && opts.closing)
            }
        }).catch(() => {});
    }
    const onVisibilityReport = () => {
        reportTabVisibility(false, { interacted: !document.hidden });
        if (!document.hidden && typeof scheduleFilterPinRefresh === 'function') {
            scheduleFilterPinRefresh(true);
        }
    };
    const onWindowFocusReport = () => {
        reportTabVisibility(false, { interacted: true });
        if (typeof scheduleFilterPinRefresh === 'function') {
            scheduleFilterPinRefresh(true);
        }
    };
    const onPageHideReport = () => reportTabVisibility(true, { closing: true });
    document.addEventListener('visibilitychange', onVisibilityReport);
    window.addEventListener('focus', onWindowFocusReport);
    window.addEventListener('pagehide', onPageHideReport);
    const tabVisibilityTimer = setInterval(() => reportTabVisibility(false), 5000);
    reportTabVisibility(false);

    function reportState() {
        try {
            const ctx = findMapContext();
            if (!ctx.map) return;
            _ensureMarkRedrawHooks(ctx);
            _reconcileMarkedVisuals(ctx, { skipSelectedPopup: true });
            const center = ctx.map.getCenter();
            const zoom = ctx.map.getZoom();
            const markersCount = (ctx.markerList && ctx.markerList.length) || (ctx.ciLayer && ctx.ciLayer._latlngMarkers && ctx.ciLayer._latlngMarkers.all ? ctx.ciLayer._latlngMarkers.all().length : 0);
            const gisVm = resolveGisVm(ctx);
            const st = gisVm && gisVm.$store && gisVm.$store.state ? gisVm.$store.state : {};
            const areaInfo = st.areaInfo || {};
            const layerInfo = st.layeredExplorationInfo || null;
            apiRequest('/report_state', {
                method: 'POST',
                body: {
                    tab_id: GH_TAB_ID,
                    tab_visible: !document.hidden,
                    tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
                    center: [center.lat, center.lng],
                    zoom: zoom,
                    markersCount: markersCount,
                    activeAreaId: areaInfo.id !== undefined ? areaInfo.id : null,
                    activeAreaName: areaInfo.name || null,
                    layeredGroupId: layerInfo && layerInfo.id !== undefined ? layerInfo.id : null,
                    layeredFloorId: layerInfo && layerInfo.activeFloorId !== undefined ? layerInfo.activeFloorId : null,
                    scriptVersion: SCRIPT_VERSION,
                    scriptBuild: SCRIPT_BUILD,
                    runtimeMode: 'extension'
                }
            }).catch(() => {});
        } catch (e) {}
    }

    const reportStateTimer = setInterval(reportState, 5000);

    function fetchInitialStatus() {
        apiRequest('/status', { method: 'GET' })
            .then(data => {
                if (data) {
                    initialStatusSynced = true;
                    const activeState = typeof data.engine_active === 'boolean' ? data.engine_active : engineActive;
                    setStatus(true, activeState);
                    warnServerVersionMismatch(data);
                    if (typeof data.latest_seq === 'number' && (lastEventSeq < 0 || data.latest_seq < lastEventSeq)) {
                        lastEventSeq = data.latest_seq;
                    }
                    if (data.display_name_sync) updateHotkeyDisplay(data.display_name_sync, 'sync', data.hotkey_sync || null);
                    else if (data.display_name) updateHotkeyDisplay(data.display_name, 'sync', data.hotkey || null);
                    if (data.display_name_debug) updateHotkeyDisplay(data.display_name_debug, 'debug', data.hotkey_debug || null);
                    if (data.display_name_switch) updateHotkeyDisplay(data.display_name_switch, 'switch', data.hotkey_switch || null);
                    if (typeof data.auto_sync_m === 'boolean') setAutoSyncUI(data.auto_sync_m);
                    if (data.language && window.GHI18n && !window.GHI18n.hasExplicitPreference()) {
                        // App language is the default until the user picks VI | EN.
                        window.GHI18n.setLang(data.language, { explicit: false });
                    }
                    if (!activeWebRoute) {
                        syncInitialRouteState();
                    }
                    setActiveLockedPin(data.locked_target || null);
                }
            })
            .catch(() => {
                setStatus(false);
            });
    }

    function showDiagnosticNoteModal(folderName, hasWorldCoords = true, reason = '', meta = {}) {
        try {
            const fname = (folderName || 'diagnostics').trim();
            const zname = (meta && meta.zipName) ? String(meta.zipName).trim() : `${fname}.zip`;
            const summary = (meta && meta.summary && typeof meta.summary === 'object') ? meta.summary : {};
            const now = Date.now();
            if (lastNoteModalFolder === fname && (now - lastNoteModalTs) < 2000) {
                return;
            }
            lastNoteModalFolder = fname;
            lastNoteModalTs = now;

            injectThemeStyles();
            const existing = document.getElementById('gh-diag-note-modal');
            if (existing) existing.remove();

            const overlay = document.createElement('div');
            overlay.id = 'gh-diag-note-modal';
            overlay.className = 'gh-root';
            overlay.setAttribute('data-folder', fname);
            overlay.style.cssText = `
                position: fixed; top: 0; left: 0; width: 100vw; height: 100vh;
                background: rgba(0, 0, 0, 0.72); backdrop-filter: blur(10px);
                z-index: 2147483647; display: flex; align-items: center; justify-content: center;
            `;

            const badgeBg = hasWorldCoords ? 'rgba(34, 197, 94, 0.16)' : 'rgba(245, 158, 11, 0.16)';
            const badgeBorder = hasWorldCoords ? 'rgba(34, 197, 94, 0.45)' : 'rgba(245, 158, 11, 0.45)';
            const badgeColor = hasWorldCoords ? '#4ade80' : '#fbbf24';
            let noteSavedOnce = false;

            overlay.innerHTML = `
                <div style="background: rgba(15, 23, 42, 0.98); border: 1px solid rgba(245, 158, 11, 0.55); border-radius: 16px; padding: 20px 24px; max-width: 540px; width: 92%; color: #fff; box-shadow: 0 24px 60px rgba(0,0,0,0.85); display: flex; flex-direction: column; gap: 11px;">
                    <div style="display: flex; justify-content: space-between; align-items: flex-start; gap: 10px; border-bottom: 1px solid rgba(51, 65, 85, 0.6); padding-bottom: 10px;">
                        <div>
                            <strong id="gh-note-title" style="color: #fbbf24; font-size: 15px; font-weight: 700;"></strong>
                            <div id="gh-note-folder" class="gh-mono" style="font-size: 11.5px; color: #7dd3fc; margin-top: 2px;"></div>
                        </div>
                        <span id="gh-note-badge" style="background: ${badgeBg}; border: 1px solid ${badgeBorder}; color: ${badgeColor}; font-size: 11px; font-weight: 600; padding: 3px 8px; border-radius: 999px; white-space: nowrap;"></span>
                    </div>
                    <div id="gh-note-reason" style="font-size: 11.5px; color: #cbd5e1; display: none;"></div>
                    <div id="gh-note-discord-hint" style="font-size: 11.5px; line-height: 1.45; color: #a7f3d0; background: rgba(16, 185, 129, 0.1); border: 1px solid rgba(16, 185, 129, 0.32); border-radius: 9px; padding: 8px 10px;"></div>
                    <div id="gh-note-tags" style="display: flex; flex-wrap: wrap; gap: 6px;"></div>
                    <textarea id="gh-diag-note-input" rows="3" style="width: 100%; box-sizing: border-box; background: rgba(2, 6, 23, 0.8); border: 1px solid rgba(56, 189, 248, 0.4); border-radius: 10px; padding: 10px 12px; color: #f8fafc; font-size: 12.5px; line-height: 1.45; resize: vertical; outline: none; font-family: inherit;"></textarea>
                    <div style="display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center; gap: 8px;">
                        <div style="display: flex; gap: 6px; flex-wrap: wrap;">
                            <button id="gh-note-open-folder" type="button" style="background: rgba(56, 189, 248, 0.14); border: 1px solid rgba(56, 189, 248, 0.38); color: #7dd3fc; border-radius: 8px; padding: 6px 11px; font-size: 11.5px; font-weight: 600; cursor: pointer;"></button>
                            <button id="gh-note-copy-discord" type="button" style="background: rgba(245, 158, 11, 0.14); border: 1px solid rgba(245, 158, 11, 0.38); color: #fcd34d; border-radius: 8px; padding: 6px 11px; font-size: 11.5px; font-weight: 600; cursor: pointer;"></button>
                        </div>
                        <div style="display: flex; gap: 8px; flex-wrap: wrap;">
                            <button id="gh-note-skip" type="button" style="background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.16); color: #94a3b8; border-radius: 8px; padding: 6px 12px; font-size: 12px; cursor: pointer;"></button>
                            <button id="gh-note-save-only" type="button" style="background: rgba(56, 189, 248, 0.18); border: 1px solid rgba(56, 189, 248, 0.55); color: #e0f2fe; font-weight: 600; border-radius: 8px; padding: 6px 13px; font-size: 12px; cursor: pointer;"></button>
                            <button id="gh-note-save" type="button" style="background: #d97706; border: none; color: #fff; font-weight: 600; border-radius: 8px; padding: 6px 14px; font-size: 12px; cursor: pointer;"></button>
                        </div>
                    </div>
                    <div style="display: flex; justify-content: space-between; align-items: center; gap: 10px;">
                        <span id="gh-note-shortcut" style="font-size: 11px; color: #64748b;"></span>
                        <span id="gh-note-status" style="font-size: 11.5px; color: #4ade80; text-align: right;"></span>
                    </div>
                    <div id="gh-note-error" style="color: #ef4444; font-size: 11.5px; min-height: 14px;"></div>
                </div>
            `;

            document.body.appendChild(overlay);
            const reasonEl = overlay.querySelector('#gh-note-reason');
            if (reason) {
                reasonEl.textContent = reason;
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
                const appVer = summary.app_version || '5.7.0';
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
                    `_${tr('page.diag.modal.discord_footer', { zip: zname })}_`
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
                if (titleEl) titleEl.textContent = tr('page.diag.modal.title');
                if (badgeEl) badgeEl.textContent = hasWorldCoords ? tr('page.diag.modal.badge_ok') : tr('page.diag.modal.badge_warn');
                if (folderEl) folderEl.textContent = tr('page.diag.modal.bundle_prefix', { folder: fname });
                if (discordHintEl) discordHintEl.textContent = tr('page.diag.modal.discord_hint');
                if (shortcutEl) shortcutEl.textContent = tr('page.diag.modal.shortcut_hint');
                if (openFolderBtn) openFolderBtn.textContent = tr('page.diag.modal.open_folder');
                if (copyDiscordBtn) copyDiscordBtn.textContent = tr('page.diag.modal.copy_discord');
                textarea.placeholder = tr('page.diag.modal.placeholder');
                skipBtn.textContent = noteSavedOnce ? tr('page.diag.modal.close') : tr('page.diag.modal.skip');
                if (saveOnlyBtn && !saveOnlyBtn.disabled) {
                    saveOnlyBtn.textContent = tr('page.diag.modal.save_only');
                }
                if (!saveBtn.disabled) {
                    saveBtn.textContent = tr('page.diag.modal.save');
                }
                const tagKeys = [
                    'page.diag.tag.wrong_pos',
                    'page.diag.tag.wrong_floor',
                    'page.diag.tag.no_cave_img',
                    'page.diag.tag.panning_no_arrow',
                    'page.diag.tag.other',
                ];
                tagsWrap.innerHTML = '';
                tagKeys.forEach((tKey) => {
                    const tag = tr(tKey);
                    const btn = document.createElement('button');
                    btn.type = 'button';
                    btn.textContent = tag;
                    btn.style.cssText = 'background: rgba(56, 189, 248, 0.12); border: 1px solid rgba(56, 189, 248, 0.35); color: #7dd3fc; border-radius: 999px; padding: 3px 9px; font-size: 11px; cursor: pointer;';
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
                });
            };

            renderModalLabels();
            let unsubI18n = null;
            if (window.GHI18n && typeof window.GHI18n.onChange === 'function') {
                unsubI18n = window.GHI18n.onChange(() => {
                    if (document.body.contains(overlay)) {
                        renderModalLabels();
                    }
                });
                if (typeof window.GHI18n.isReady === 'function' && !window.GHI18n.isReady()) {
                    try {
                        window.postMessage({ __gh_ext: true, dir: 'to-isolated', kind: 'I18N_REQUEST' }, window.location.origin);
                    } catch (err) {}
                }
            }

            const closeNoteModal = () => {
                document.removeEventListener('keydown', onModalKeyDown, true);
                if (typeof unsubI18n === 'function') {
                    try { unsubI18n(); } catch (err) {}
                    unsubI18n = null;
                }
                overlay.remove();
            };

            if (openFolderBtn) {
                openFolderBtn.addEventListener('click', async () => {
                    errEl.textContent = '';
                    const md = formatModalDiscordTemplate();
                    await copyTextCompat(md);
                    apiRequest('/open_diagnostics', {
                        method: 'POST',
                        body: { folder_name: fname }
                    })
                    .then(res => {
                        if (res && res.ok) {
                            statusEl.textContent = tr('page.diag.modal.folder_opened');
                        } else {
                            errEl.textContent = tr('page.diag.modal.folder_error');
                        }
                    })
                    .catch(() => {
                        errEl.textContent = tr('page.diag.modal.folder_error');
                    });
                });
            }

            if (copyDiscordBtn) {
                copyDiscordBtn.addEventListener('click', async () => {
                    errEl.textContent = '';
                    const md = formatModalDiscordTemplate();
                    const ok = await copyTextCompat(md);
                    if (ok) {
                        statusEl.textContent = tr('page.diag.modal.copied_discord');
                        setLog(tr('page.diag.modal.copied_discord'), '#22c55e');
                    }
                });
            }

            const submitNoteOnly = async () => {
                const noteText = textarea.value.trim();
                if (saveOnlyBtn) {
                    saveOnlyBtn.disabled = true;
                    saveOnlyBtn.textContent = tr('page.diag.modal.saving');
                }
                saveBtn.disabled = true;
                errEl.textContent = '';

                try {
                    const res = await apiRequest('/save_diagnostic_note', {
                        method: 'POST',
                        body: { folder_name: fname, note: noteText, create_zip: false }
                    });
                    if (saveOnlyBtn) {
                        saveOnlyBtn.disabled = false;
                        saveOnlyBtn.textContent = tr('page.diag.modal.save_only');
                    }
                    saveBtn.disabled = false;
                    if (res && res.ok) {
                        noteSavedOnce = true;
                        statusEl.textContent = tr('page.diag.modal.saved_only_inline', { folder: fname });
                        setLog(tr('page.diag.modal.saved_toast', { folder: fname }), '#22c55e');
                        closeNoteModal();
                    } else {
                        errEl.textContent = (res && res.error) ? res.error : tr('page.diag.modal.save_error');
                    }
                } catch (err) {
                    if (saveOnlyBtn) {
                        saveOnlyBtn.disabled = false;
                        saveOnlyBtn.textContent = tr('page.diag.modal.save_only');
                    }
                    saveBtn.disabled = false;
                    errEl.textContent = (err && err.data && err.data.error)
                        ? err.data.error
                        : tr('page.diag.modal.offline_error');
                }
            };

            const submitNoteAndZip = async () => {
                const noteText = textarea.value.trim();
                saveBtn.disabled = true;
                saveBtn.textContent = tr('page.diag.modal.saving');
                if (saveOnlyBtn) saveOnlyBtn.disabled = true;
                errEl.textContent = '';

                try {
                    const res = await apiRequest('/save_diagnostic_note', {
                        method: 'POST',
                        body: { folder_name: fname, note: noteText, create_zip: true }
                    });
                    saveBtn.disabled = false;
                    saveBtn.textContent = tr('page.diag.modal.save');
                    if (saveOnlyBtn) saveOnlyBtn.disabled = false;
                    if (res && res.ok) {
                        noteSavedOnce = true;
                        skipBtn.textContent = tr('page.diag.modal.close');
                        const md = formatModalDiscordTemplate();
                        await copyTextCompat(md);
                        try {
                            await apiRequest('/open_diagnostics', {
                                method: 'POST',
                                body: { folder_name: fname, ensure_zip: true }
                            });
                        } catch (openErr) {}
                        statusEl.textContent = tr('page.diag.modal.saved_inline', { zip: zname });
                        setLog(tr('page.diag.modal.saved_toast', { folder: fname }), '#22c55e');
                    } else {
                        errEl.textContent = (res && res.error) ? res.error : tr('page.diag.modal.save_error');
                    }
                } catch (err) {
                    saveBtn.disabled = false;
                    saveBtn.textContent = tr('page.diag.modal.save');
                    if (saveOnlyBtn) saveOnlyBtn.disabled = false;
                    errEl.textContent = (err && err.data && err.data.error)
                        ? err.data.error
                        : tr('page.diag.modal.offline_error');
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
        } catch (err) {
            console.error('[Genshin Helper] showDiagnosticNoteModal error:', err);
        }
    }

    function handleServerMessage(data) {
        if (data.type === 'HOTKEY_UPDATED') {
            const act = data.action || 'sync';
            if (data.display_name) {
                updateHotkeyDisplay(data.display_name, act, data.hotkey || null);
                setLog(tr('page.hotkey.updated', { action: act.toUpperCase(), name: data.display_name }), '#38bdf8');
            }
            return;
        }

        if (data.type === 'LANGUAGE_UPDATED') {
            if (data.language && window.GHI18n && typeof window.GHI18n.setLang === 'function') {
                window.GHI18n.setLang(data.language);
            }
            renderTargetLockPill();
            syncPopupNavigateButtonState();
            return;
        }

        if (data.type === 'AUTO_SYNC_UPDATED') {
            if (typeof data.auto_sync_m === 'boolean') {
                setAutoSyncUI(data.auto_sync_m);
                setLog(tr(data.auto_sync_m ? 'page.autosync.on' : 'page.autosync.off'), '#38bdf8');
            }
            return;
        }

        if (data.type === 'SINGLE_MONITOR_UPDATED') {
            // Deprecated toggle: feedback owner is now auto-detected by the app (tab visibility).
            return;
        }

        if (data.type === 'ENGINE_STATE_UPDATED') {
            if (typeof data.engine_active === 'boolean') {
                setStatus(true, data.engine_active);
                setLog(
                    tr(data.engine_active ? 'page.engine.resumed' : 'page.engine.standby'),
                    data.engine_active ? '#22c55e' : '#fbbf24'
                );
            }
            return;
        }

        if (data.type === 'DIAGNOSTIC_CAPTURED') {
            playBeep();
            const folderName = data.folder_name || 'diagnostics';
            const zipName = data.zip_name || `${folderName}.zip`;
            if (data.has_world_coords) {
                setLog(tr('page.diag.captured_ok', { folder: zipName }), '#22c55e');
            } else {
                setLog(tr('page.diag.captured_warn', { folder: zipName, reason: data.reason || '' }), '#fbbf24');
            }
            showDiagnosticNoteModal(folderName, Boolean(data.has_world_coords), data.reason || '', {
                zipName,
                zipSizeBytes: data.zip_size_bytes || 0,
                summary: (data.summary && typeof data.summary === 'object') ? data.summary : {}
            });
            return;
        }

        if (data.type === 'ROUTE_UPDATED') {
            handleRouteUpdatedEvent(data);
            return;
        }

        if (data.type === 'TARGET_LOCK_UPDATED') {
            setActiveLockedPin(data.locked_target || null);
            return;
        }

        if (data.type === 'TSP_ROUTE_UPDATED') {
            const route = Object.prototype.hasOwnProperty.call(data, 'route') ? data.route : data;
            const hasRoute = Boolean(
                route &&
                typeof route === 'object' &&
                !data.cleared &&
                ((Array.isArray(route.legs) && route.legs.length > 0) ||
                 (Array.isArray(route.ordered_pins) && route.ordered_pins.length > 0))
            );
            if (hasRoute) {
                renderTspRouteOnMap(route);
            } else {
                clearTspRouteFromMap();
            }
            return;
        }

        if (data.type === 'MINIMAP_POSITION' || data.type === 'LIVE_POSITION') {
            handleContinuousMinimapUpdate(data);
            return;
        }

        if (data.type === 'NAV_MARK_PIN') {
            handleNavMarkPin(data);
            return;
        }

        if (data.type === 'LAN_FOCUS_PIN') {
            handleLanFocusPin(data);
            return;
        }

        if (data.type === 'LAN_SET_FLOOR') {
            handleLanSetFloor(data);
            return;
        }

        if (data.type === 'SYNC_POSITION_ONLY' || data.type === 'SYNC_AND_MARK') {
            nativeFeedbackActive = data.native_feedback === true;
            const markAfterSync = (data.type === 'SYNC_AND_MARK');
            const eventSeq = typeof data._seq === 'number' ? data._seq : 0;
            if (data.detected === false) {
                if (markAfterSync) {
                    setLog(tr('page.sync.not_detected'), '#f59e0b');
                    sendActionFeedback(
                        'SYNC_AND_MARK',
                        'error',
                        tr('page.sync.not_detected_title'),
                        tr('page.sync.not_detected_detail'),
                        eventSeq
                    );
                }
                return;
            }
            if (!markAfterSync && (data.source === 'minimap' || data.continuous === true || data.silent === true)) {
                handleContinuousMinimapUpdate(data);
                return;
            }
            if (!markAfterSync && routeInspecting && !maybeSmartReattach(data)) {
                const curCtx = findMapContext();
                if (curCtx && curCtx.map && typeof data.lat === 'number' && typeof data.lng === 'number' && Number(data.map_id || 2) === getCurrentWebMapId()) {
                    updatePlayerMarker(curCtx, data);
                }
                return;
            }
            if (routeInspecting) {
                routeInspecting = false;
                if (activeWebRoute) renderRouteBar(activeWebRoute);
            }
            // Explicit Map (M) sync or Sync & Mark always auto-re-pins autoPanEnabled
            setAutoPanPinned(true, { silent: true });
            performAutoSyncAndMark(data, markAfterSync, eventSeq);
        }
    }

    function getWebLayerState(ctx) {
        const gisVm = resolveGisVm(ctx);
        const activeInfo = gisVm && gisVm.$store && gisVm.$store.state ? gisVm.$store.state.layeredExplorationInfo : null;
        const hasCanvasLayer = Boolean(gisVm && gisVm.canvasLayeredImageLayer);
        const elevatorEl = document.querySelector('#underground-elevator, .underground-elevator');
        const inLayer = Boolean(activeInfo || hasCanvasLayer || (elevatorEl && elevatorEl.offsetWidth > 0));
        const rawActiveFloorId = (activeInfo && activeInfo.activeFloorId !== undefined && activeInfo.activeFloorId !== null)
            ? Number(activeInfo.activeFloorId)
            : NaN;
        return {
            gisVm,
            activeInfo,
            inLayer,
            activeFloorId: Number.isFinite(rawActiveFloorId) ? rawActiveFloorId : null
        };
    }

    function buildLayerSyncSig(data) {
        if (!data || !data.is_underground) return 'surface';
        const fid = (data.floor_id !== undefined && data.floor_id !== null && String(data.floor_id).trim() !== '')
            ? String(data.floor_id).trim()
            : ((data.floor_short || data.floor_name || '').trim().toUpperCase());
        return `ug:${fid}`;
    }

    function schedulePostAreaOrLayerRefresh(ctx, fallbackData, targetMapId, savedZoom, guardDriftNeeded) {
        clearMinimapAuxTimers();
        if (guardDriftNeeded) {
            const guardDrift = () => {
                try {
                    if (!autoPanEnabled || routeInspecting) return;
                    if (getCurrentWebMapId() !== targetMapId) return;
                    const curCtx = findMapContext() || ctx;
                    if (!curCtx || !curCtx.map || typeof curCtx.map.getCenter !== 'function') return;
                    const latest = (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number'
                        && Number(playerLastState.map_id || 2) === targetMapId)
                        ? playerLastState
                        : fallbackData;
                    const c = curCtx.map.getCenter();
                    if (c && Math.hypot(c.lat - latest.lat, c.lng - latest.lng) > 15) {
                        const z = (typeof savedZoom === 'number' && Number.isFinite(savedZoom))
                            ? savedZoom
                            : curCtx.map.getZoom();
                        curCtx.map.setView([latest.lat, latest.lng], z, { animate: false });
                        lastSyncedView = {
                            lat: latest.lat,
                            lng: latest.lng,
                            zoom: z,
                            mapId: targetMapId
                        };
                        if (document.hidden) {
                            pendingVisibleRefresh = true;
                        }
                        updatePlayerMarker(curCtx, latest);
                    }
                } catch (e) {}
            };
            scheduleMinimapAuxTimer(guardDrift, 220);
            scheduleMinimapAuxTimer(guardDrift, 520);
        }

        const refreshNearby = () => {
            try {
                if (getCurrentWebMapId() !== targetMapId) return;
                const latest = (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number'
                    && Number(playerLastState.map_id || 2) === targetMapId)
                    ? playerLastState
                    : fallbackData;
                const floorArg = latest.floor_id !== undefined ? latest.floor_id : (latest.floor_short || null);
                pushNearbyPinsToServer(latest.lat, latest.lng, targetMapId, floorArg, { force: true });
            } catch (e) {}
        };
        scheduleMinimapAuxTimer(refreshNearby, 750);
        scheduleMinimapAuxTimer(refreshNearby, 1600);
    }

    function handleContinuousMinimapUpdate(data) {
        try {
            if (!data || typeof data.lat !== 'number' || typeof data.lng !== 'number') return;
            const ctx = findMapContext();
            if (!ctx || !ctx.map) return;
            _ensureMarkRedrawHooks(ctx);
            bindDragUnpinListeners(ctx);
            const targetMapId = data.map_id ? parseInt(data.map_id, 10) : 2;
            if (targetMapId !== getCurrentWebMapId()) return;

            if (routeInspecting && maybeSmartReattach(data)) {
                routeInspecting = false;
                setAutoPanPinned(true, { silent: true });
                if (activeWebRoute) renderRouteBar(activeWebRoute);
            }

            const followingPlayer = Boolean(autoPanEnabled && !routeInspecting);
            const curCenter = (typeof ctx.map.getCenter === 'function') ? ctx.map.getCenter() : null;
            const distFromCur = curCenter ? Math.hypot(curCenter.lat - data.lat, curCenter.lng - data.lng) : Infinity;
            const preSwitchZoom = (typeof ctx.map.getZoom === 'function') ? ctx.map.getZoom() : null;

            let areaChanged = false;
            let layerTransitioned = false;

            if (followingPlayer) {
                const now = Date.now();
                const layerState = getWebLayerState(ctx);
                const wantUnderground = Boolean(data.is_underground);
                const parsedFloorId = (data.floor_id !== undefined && data.floor_id !== null && String(data.floor_id).trim() !== '')
                    ? Number(data.floor_id)
                    : NaN;
                const targetFloorId = Number.isFinite(parsedFloorId) ? parsedFloorId : null;
                const layerMismatch = autoSwitchLayerEnabled && (
                    wantUnderground
                        ? (!layerState.inLayer || (targetFloorId !== null && layerState.activeFloorId !== null && layerState.activeFloorId !== targetFloorId))
                        : layerState.inLayer
                );
                const layerSig = buildLayerSyncSig(data);
                const shouldTriggerLayerTransition = Boolean(
                    layerMismatch &&
                    ((layerSig !== lastMinimapLayerSig) || ((now - lastMinimapLayerSyncTs) >= 2500))
                );

                // Auto switch region/area on Map 2 so HoYoLAB loads pins for the player's current nation
                if (targetMapId === 2 && ((data.area_id !== undefined && data.area_id !== null) || data.area_name)) {
                    const targetAreaKey = (data.area_id !== undefined && data.area_id !== null)
                        ? Number(data.area_id)
                        : String(data.area_name).trim().toLowerCase();
                    const isNewTargetArea = (lastMinimapAreaSwitchId !== targetAreaKey);
                    const areaCooldownOk = (isNewTargetArea && (distFromCur > 250 || (now - lastMinimapAreaSwitchTs) >= 800))
                        || ((now - lastMinimapAreaSwitchTs) >= 1200);
                    if (areaCooldownOk) {
                        const skipToggleForCave = Boolean(wantUnderground && autoSwitchLayerEnabled);
                        const areaRes = ensureAreaSwitch(ctx, data.area_id, data.area_name, skipToggleForCave);
                        if (areaRes === 'changed') {
                            areaChanged = true;
                            lastMinimapAreaSwitchId = targetAreaKey;
                            lastMinimapAreaSwitchTs = now;
                        }
                    }
                }

                // Clear stale cave maxBounds when on surface or starting a layer transition
                if (!wantUnderground || shouldTriggerLayerTransition) {
                    if (typeof ctx.map.setMaxBounds === 'function') {
                        ctx.map.setMaxBounds(null);
                    }
                    if (layerState.gisVm && layerState.gisVm.setMaxBoundsFn && typeof ctx.map.off === 'function') {
                        ctx.map.off('zoomend', layerState.gisVm.setMaxBoundsFn);
                    }
                }

                const targetZoom = (typeof preSwitchZoom === 'number' && Number.isFinite(preSwitchZoom))
                    ? preSwitchZoom
                    : ctx.map.getZoom();
                if (distFromCur > 250 || document.hidden || areaChanged || shouldTriggerLayerTransition) {
                    ctx.map.setView([data.lat, data.lng], targetZoom, { animate: false });
                } else if (distFromCur > 4) {
                    ctx.map.panTo([data.lat, data.lng], { animate: true, duration: 0.16 });
                }
                lastSyncedView = {
                    lat: data.lat,
                    lng: data.lng,
                    zoom: targetZoom,
                    mapId: targetMapId
                };
                if (document.hidden) {
                    pendingVisibleRefresh = true;
                }

                // Sync underground <-> surface exploration mode when web layer state disagrees
                if (shouldTriggerLayerTransition) {
                    lastMinimapLayerSig = layerSig;
                    lastMinimapLayerSyncTs = now;
                    layerTransitioned = true;
                    handleLayeredExploration(ctx, data, targetZoom, areaChanged);
                }
            }

            updatePlayerMarker(ctx, data);
            pushNearbyPinsToServer(
                data.lat,
                data.lng,
                targetMapId,
                data.floor_id !== undefined ? data.floor_id : (data.floor_short || null),
                (areaChanged || layerTransitioned) ? { force: true } : {}
            );

            if (areaChanged || layerTransitioned) {
                schedulePostAreaOrLayerRefresh(ctx, data, targetMapId, preSwitchZoom, areaChanged && !layerTransitioned);
            }
        } catch (e) {}
    }

    // =========================================================================
    // 5. Core Auto-Sync & Mark (Multi-Map & World Navigation)
    // =========================================================================
    function getCurrentWebMapId() {
        const m = window.location.hash.match(/#\/map\/(\d+)/);
        if (m) return parseInt(m[1]);
        return 2; // Default to Teyvat
    }

    function waitForNewMapReady(timeoutMs = 4000) {
        return new Promise((resolve) => {
            const start = Date.now();
            const check = () => {
                const ctx = findMapContext();
                if (ctx && ctx.map) {
                    resolve(ctx);
                } else if (Date.now() - start < timeoutMs) {
                    setTimeout(check, 100);
                } else {
                    resolve(findMapContext());
                }
            };
            setTimeout(check, 250);
        });
    }

    function performAutoSyncAndMark(data, markAfterSync = false, eventSeq = 0) {
        try {
            const targetMapId = data.map_id ? parseInt(data.map_id) : 2;
            const currentMapId = getCurrentWebMapId();

            if (data.has_world_coords && targetMapId !== currentMapId) {
                const mapName = data.map_name || `Map ${targetMapId}`;
                setLog(tr('page.sync.switching_map', { name: mapName }), '#38bdf8');
                if (markAfterSync && !nativeFeedbackActive) playBeep();

                // Switch route via Vue Router or Hash
                try {
                    const vueApp = document.querySelector('#app')?.__vue__;
                    if (vueApp && vueApp.$router) {
                        vueApp.$router.push(`/map/${targetMapId}`);
                    }
                } catch (e) {}

                if (window.location.hash.includes('#/map/')) {
                    window.location.hash = window.location.hash.replace(/#\/map\/\d+/, `#/map/${targetMapId}`);
                } else {
                    window.location.hash = `#/map/${targetMapId}`;
                }
                window.dispatchEvent(new HashChangeEvent('hashchange'));

                // Fallback: If SPA router fails to load target map within 700ms, reload to force HoYoLAB to mount target map
                setTimeout(() => {
                    if (getCurrentWebMapId() !== targetMapId) {
                        const url = new URL(window.location.href);
                        url.hash = `#/map/${targetMapId}`;
                        window.location.href = url.toString();
                        window.location.reload();
                    }
                }, 700);

                waitForNewMapReady(4000).then((newCtx) => {
                    setLog(tr('page.sync.map_opened', { name: mapName }), '#22c55e');
                    executeSyncAndMarkOnContext(newCtx, data, markAfterSync, eventSeq);
                });
                return;
            }

            const ctx = findMapContext();
            executeSyncAndMarkOnContext(ctx, data, markAfterSync, eventSeq);
        } catch (e) {
            console.error('[Genshin Helper] Error in performAutoSyncAndMark:', e);
        }
    }

    function executeSyncAndMarkOnContext(ctx, data, markAfterSync = false, eventSeq = 0) {
        try {
            if (!ctx || !ctx.map) {
                setLog(tr('page.sync.map_not_found'), '#ef4444');
                if (markAfterSync) {
                    sendActionFeedback(
                        'SYNC_AND_MARK',
                        'error',
                        tr('page.sync.map_not_ready_title'),
                        tr('page.sync.map_not_ready_detail'),
                        eventSeq
                    );
                }
                return;
            }

            _ensureMarkRedrawHooks(ctx);
            bindDragUnpinListeners(ctx);

            // Step 1: Check if game sent real World Coordinates (lat, lng)
            const hasWorld = (data && data.has_world_coords && data.lat !== undefined && data.lng !== undefined);
            const targetLat = hasWorld ? data.lat : null;
            const targetLng = hasWorld ? data.lng : null;
            const targetZoom = computeTargetLeafletZoom(ctx.map, data);

            if (hasWorld) {
                const mapName = data.map_name ? ` (${data.map_name})` : '';
                if (markAfterSync) {
                    setLog(
                        tr('page.sync.panning_to', {
                            map: mapName,
                            lat: targetLat.toFixed(1),
                            lng: targetLng.toFixed(1)
                        }),
                        '#38bdf8'
                    );
                }

                // Auto switch region/area on Map 2 (commits areaInfo for both Surface & Caves so sidebar & markers match)
                const areaSwitchRes = (!data.map_id || parseInt(data.map_id) === 2)
                    ? ensureAreaSwitch(ctx, data.area_id, data.area_name, Boolean(data.is_underground && autoSwitchLayerEnabled))
                    : 'none';
                const areaChanged = (areaSwitchRes === 'changed');
                if (areaChanged) {
                    lastMinimapAreaSwitchId = (data.area_id !== undefined && data.area_id !== null)
                        ? Number(data.area_id)
                        : String(data.area_name || '').trim().toLowerCase();
                    lastMinimapAreaSwitchTs = Date.now();
                }

                const gisVmNow = resolveGisVm(ctx);
                const elevatorNow = document.querySelector('#underground-elevator, .underground-elevator');
                const inLayerNow = Boolean(
                    (gisVmNow && gisVmNow.$store && gisVmNow.$store.state && gisVmNow.$store.state.layeredExplorationInfo) ||
                    (gisVmNow && gisVmNow.canvasLayeredImageLayer) ||
                    (elevatorNow && elevatorNow.offsetWidth > 0)
                );
                const needsLayerTransition = Boolean(autoSwitchLayerEnabled && (data.is_underground || inLayerNow));

                // Unconditionally clear stale cave maxBounds when landing on surface or transitioning layers
                if (!data.is_underground || needsLayerTransition) {
                    if (typeof ctx.map.setMaxBounds === 'function') {
                        ctx.map.setMaxBounds(null);
                    }
                    if (gisVmNow && gisVmNow.setMaxBoundsFn && typeof ctx.map.off === 'function') {
                        ctx.map.off('zoomend', gisVmNow.setMaxBoundsFn);
                    }
                }

                // Instant snap on teleports / zoom jumps / layer transitions / hidden tab / 1-monitor mode; smooth short pan only for visible nearby moves
                if (autoPanEnabled) {
                    const curCenter = (typeof ctx.map.getCenter === 'function') ? ctx.map.getCenter() : null;
                    const distFromCur = curCenter ? Math.hypot(curCenter.lat - targetLat, curCenter.lng - targetLng) : Infinity;
                    const currentZoom = ctx.map.getZoom();
                    const mustSnap = document.hidden || needsLayerTransition || distFromCur > 250 || Math.abs(currentZoom - targetZoom) > 0.25;
                    if (mustSnap) {
                        ctx.map.setView([targetLat, targetLng], targetZoom, { animate: false });
                    } else {
                        ctx.map.panTo([targetLat, targetLng], { animate: true, duration: 0.22 });
                    }
                    lastSyncedView = {
                        lat: targetLat,
                        lng: targetLng,
                        zoom: targetZoom,
                        mapId: data.map_id ? parseInt(data.map_id, 10) : 2
                    };
                    if (document.hidden) {
                        pendingVisibleRefresh = true;
                    }
                }
                updatePlayerMarker(ctx, data);
                const resolvedSyncMapId = data.map_id ? parseInt(data.map_id, 10) : getCurrentWebMapId();
                const resolvedSyncFloorId = data.floor_id !== undefined ? data.floor_id : (data.floor_short || null);
                pushNearbyPinsToServer(targetLat, targetLng, resolvedSyncMapId, resolvedSyncFloorId, { force: true });

                // Handle automatic cave & multi-floor exploration (or returning to surface)
                if (autoSwitchLayerEnabled) {
                    clearMinimapAuxTimers();
                    lastMinimapLayerSig = buildLayerSyncSig(data);
                    lastMinimapLayerSyncTs = Date.now();
                    handleLayeredExploration(ctx, data, targetZoom, areaChanged);
                }
                if (areaChanged || needsLayerTransition) {
                    setTimeout(() => {
                        pushNearbyPinsToServer(targetLat, targetLng, resolvedSyncMapId, resolvedSyncFloorId, { force: true });
                    }, 750);
                }
            } else {
                setLog(tr('page.sync.low_landmarks'), '#f59e0b');
                if (autoSwitchLayerEnabled) {
                    handleLayeredExploration(ctx, data, targetZoom);
                }
            }

            // Auto-sync on Map (M) only updates position/marker/layer — never opens popups or clicks "Ghim"
            if (!markAfterSync) {
                closeOpenPopup();
                if (hasWorld) {
                    const floorDesc = data.is_underground
                        ? ` [${data.hoyolab_floor_name || data.floor_short || data.floor_name || tr('page.layer.layered_label')}]`
                        : ` [${tr('page.layer.surface_label')}]`;
                    setLog(tr('page.sync.synced_pos', { floor: floorDesc }), '#22c55e');
                    const floorLabel = data.is_underground
                        ? (data.hoyolab_floor_name || data.floor_short || data.floor_name || tr('page.layer.layered_label'))
                        : (data.map_name || tr('page.layer.surface_label'));
                    const coordsDetail = (data.area_name ? `${data.area_name} • ` : '') + `(${targetLat.toFixed(0)}, ${targetLng.toFixed(0)})`;
                    sendActionFeedback('SYNC_POSITION_ONLY', 'synced', floorLabel, coordsDetail, eventSeq);
                }
                return;
            }

            // Step 2 (manual hotkey SYNC_AND_MARK): Wait for map animation to settle before querying collectibles
            const waitTime = hasWorld ? ((document.hidden) ? 140 : 650) : 50;
            setTimeout(() => {
                let closest = null;
                let minDistance = Infinity;

                if (hasWorld) {
                    const radius = 60;
                    const indivMarkers = _collectIndividualMarkers(ctx);
                    for (let i = 0; i < indivMarkers.length; i++) {
                        const m = indivMarkers[i];
                        if (!m || _isNavLandmarkPin(null, m)) continue;
                        const latlng = (typeof m.getLatLng === 'function') ? m.getLatLng() : null;
                        if (!latlng || typeof latlng.lat !== 'number' || typeof latlng.lng !== 'number') continue;
                        const d = Math.hypot(latlng.lat - targetLat, latlng.lng - targetLng);
                        if (d < minDistance && d <= radius) {
                            minDistance = d;
                            const pt = (ctx.map && typeof ctx.map.latLngToContainerPoint === 'function') ? ctx.map.latLngToContainerPoint(latlng) : null;
                            closest = { marker: m, latlng, point: pt, rawItem: null };
                        }
                    }

                    const tree = ctx.ciLayer && ctx.ciLayer._latlngMarkers;
                    if (!closest && tree && typeof tree.search === 'function') {
                        // Leaflet RBush tree stores minX/maxX as Lng, minY/maxY as Lat
                        const treeHits = tree.search({
                            minX: targetLng - radius,
                            minY: targetLat - radius,
                            maxX: targetLng + radius,
                            maxY: targetLat + radius
                        });
                        function isNavLandmark(it, markerObj) {
                            if (!it && !markerObj) return false;
                            const name = ((it && it.name) || (it && it.rawItem && it.rawItem.name) || (markerObj && markerObj.options && markerObj.options.title) || '').toLowerCase();
                            if (name.includes('dịch chuyển') || name.includes('dich chuyen') || name.includes('teleport') || name.includes('thần tượng') || name.includes('than tuong') || name.includes('statue')) return true;
                            const lid = (it && (it.label_id || (it.rawItem && it.rawItem.label_id))) || (markerObj && markerObj.label_id);
                            if (lid === 2 || lid === 3 || lid === 154) return true;
                            return false;
                        }

                        for (const item of treeHits) {
                            const m = item.data || item;
                            if (isNavLandmark(item, m)) continue;
                            const latlng = (m && typeof m.getLatLng === 'function') ? m.getLatLng() : { lat: (item.minY + item.maxY) / 2, lng: (item.minX + item.maxX) / 2 };
                            const d = Math.hypot(latlng.lat - targetLat, latlng.lng - targetLng);
                            if (d < minDistance) {
                                minDistance = d;
                                const pt = (ctx.map && typeof ctx.map.latLngToContainerPoint === 'function') ? ctx.map.latLngToContainerPoint(latlng) : { x: (item.minX + item.maxX) / 2, y: (item.minY + item.maxY) / 2 };
                                closest = { marker: m, latlng, point: pt, rawItem: item };
                            }
                        }
                    }

                    // Direct fast bounding-box fallback on markerList
                    if (!closest && Array.isArray(ctx.markerList) && ctx.markerList.length > 0) {
                        for (let i = 0; i < ctx.markerList.length; i++) {
                            const item = ctx.markerList[i];
                            const m = item.marker;
                            const name = (item.name || (m && m.options && m.options.title) || '').toLowerCase();
                            const lid = item.label_id || item.labelId;
                            if (lid === 2 || lid === 3 || lid === 154 || name.includes('dịch chuyển') || name.includes('teleport') || name.includes('thần tượng')) continue;

                            const y = (item.y_pos !== undefined) ? item.y_pos : (item.marker && item.marker.getLatLng ? item.marker.getLatLng().lat : null);
                            const x = (item.x_pos !== undefined) ? item.x_pos : (item.marker && item.marker.getLatLng ? item.marker.getLatLng().lng : null);
                            if (y === null || x === null) continue;
                            const dy = y - targetLat;
                            if (dy > radius || dy < -radius) continue;
                            const dx = x - targetLng;
                            if (dx > radius || dx < -radius) continue;
                            const d = Math.hypot(dx, dy);
                            if (d < minDistance) {
                                minDistance = d;
                                const latlng = { lat: y, lng: x };
                                const pt = (ctx.map && typeof ctx.map.latLngToContainerPoint === 'function') ? ctx.map.latLngToContainerPoint(latlng) : null;
                                closest = { marker: item.marker, latlng, point: pt, rawItem: item };
                            }
                        }
                    }

                    // Proximity check: Only open popup if player is close (< 60m)
                    if (!closest || minDistance > radius) {
                        closeOpenPopup();
                        setLog(tr('page.sync.no_nearby_toast'), '#22c55e');
                        sendActionFeedback('SYNC_AND_MARK', 'no_nearby', tr('page.sync.no_nearby_title'), '', eventSeq);
                        return;
                    }

                    setLog(tr('page.sync.nearest_opening', { dist: minDistance.toFixed(1) }), '#10b981');
                } else {
                    // Screen center fallback
                    const markers = getAllMarkers(ctx);
                    if (markers.length === 0) {
                        setLog(tr('page.sync.no_markers'), '#ef4444');
                        sendActionFeedback(
                            'SYNC_AND_MARK',
                            'no_nearby',
                            tr('page.sync.low_landmarks_title'),
                            tr('page.sync.low_landmarks_detail'),
                            eventSeq
                        );
                        return;
                    }
                    const mapEl = document.querySelector('.leaflet-container') || document.body;
                    const rect = mapEl.getBoundingClientRect();
                    const centerX = rect.left + rect.width / 2;
                    const centerY = rect.top + rect.height / 2;
                    for (const m of markers) {
                        const dist = Math.hypot(m.point.x - centerX, m.point.y - centerY);
                        if (dist < minDistance) {
                            minDistance = dist;
                            closest = m;
                        }
                    }
                }

                if (!closest) {
                    setLog(tr('page.sync.no_match'), '#ef4444');
                    sendActionFeedback(
                        'SYNC_AND_MARK',
                        'no_nearby',
                        tr('page.sync.low_landmarks_title'),
                        tr('page.sync.low_landmarks_detail'),
                        eventSeq
                    );
                    return;
                }

                // Open Marker Popup
                if (closest.marker) {
                    if (typeof closest.marker.openPopup === 'function') closest.marker.openPopup();
                    if (typeof closest.marker.fire === 'function') closest.marker.fire('click');
                }
                if (ctx.ciLayer && typeof ctx.ciLayer._executeListeners === 'function') {
                    ctx.ciLayer._executeListeners({ type: 'click', containerPoint: closest.point });
                }
                if (closest.domElement) {
                    closest.domElement.click();
                }

                // Wait for popup and click "Ghim" (wall-clock deadline immune to hidden-tab timer throttling)
                let layerActivated = false;
                let resolved = false;
                const deadline = Date.now() + 2400;
                let checkInterval = null;

                const pollPopupTick = () => {
                    if (resolved) {
                        if (checkInterval) clearInterval(checkInterval);
                        return;
                    }

                    // If underground exploration button is inside popup, only click if elevator is not already open
                    if (autoSwitchLayerEnabled && data.is_underground && !layerActivated) {
                        const elevator = document.querySelector('#underground-elevator, .underground-elevator');
                        const isElevatorOpen = !!elevator && elevator.offsetWidth > 0;
                        if (!isElevatorOpen) {
                            const layerBtn = findLayerButton();
                            if (layerBtn) {
                                layerActivated = true;
                                setLog(tr('page.layer.activating'), '#38bdf8');
                                layerBtn.click();
                                ensureFloorSwitch(data.active_floor_idx, data.total_floors, data.floor_name);
                            }
                        } else {
                            layerActivated = true;
                            ensureFloorSwitch(data.active_floor_idx, data.total_floors, data.floor_name);
                        }
                    }

                    const ghimBtn = findGhimButton();
                    if (ghimBtn) {
                        resolved = true;
                        if (checkInterval) clearInterval(checkInterval);
                        clickGhimAndNotify(ghimBtn, eventSeq, hasWorld ? minDistance : null, ctx, closest && closest.marker);
                        return;
                    }

                    const alreadyMarked = findAlreadyMarkedButton();
                    if (alreadyMarked) {
                        resolved = true;
                        if (checkInterval) clearInterval(checkInterval);
                        const itemTitle = getPopupTitle();
                        if (!nativeFeedbackActive) playBeep();
                        setLog(tr('page.sync.already_marked_toast'), '#fbbf24');
                        sendActionFeedback('SYNC_AND_MARK', 'already_marked', tr('page.sync.already_marked_title'), itemTitle, eventSeq);
                        return;
                    }

                    if (Date.now() >= deadline) {
                        resolved = true;
                        if (checkInterval) clearInterval(checkInterval);
                        setLog(tr('page.sync.no_mark_btn'), '#f59e0b');
                        sendActionFeedback('SYNC_AND_MARK', 'no_nearby', tr('page.sync.no_mark_btn'), '', eventSeq);
                    }
                };

                checkInterval = setInterval(pollPopupTick, 100);
                setTimeout(pollPopupTick, 30);

            }, waitTime);

        } catch (err) {
            console.error('[Genshin Helper Fatal Error]', err);
            setLog(tr('page.sync.error_toast', { error: err.message }), '#ef4444');
            if (markAfterSync) {
                sendActionFeedback('SYNC_AND_MARK', 'error', tr('page.sync.error_title'), err.message || '', eventSeq);
            }
        }
    }

    // =========================================================================
    // 5.1 In-game Mark / Unmark of the exact Mini-HUD target (no Alt+Tab)
    // =========================================================================
    function _sendNavMarkFeedback(action, status, title, seq, pinId = null) {
        apiRequest('/action_feedback', {
            method: 'POST',
            body: {
                event_type: 'NAV_MARK',
                action: action,
                status: status,
                title: title || '',
                detail: '',
                pin_id: (pinId !== null && pinId !== undefined) ? String(pinId) : '',
                seq: typeof seq === 'number' ? seq : 0
            }
        }).catch(() => {});
    }

    function _redrawCiLayer(ciLayer) {
        if (!ciLayer) return;
        try {
            if (typeof ciLayer.redraw === 'function') {
                ciLayer.redraw();
            } else if (typeof ciLayer._redraw === 'function') {
                ciLayer._redraw(true);
            } else if (typeof ciLayer.throttleRedraw === 'function') {
                ciLayer.throttleRedraw();
            }
        } catch (e) {}
    }

    function _getMarkedRecordFromStore(ctx, pinId) {
        if (pinId === null || pinId === undefined || pinId === '') return null;
        const want = String(pinId);
        try {
            const gisVm = resolveGisVm(ctx);
            const list = gisVm && gisVm.$store && gisVm.$store.state && gisVm.$store.state.markPointList;
            if (!Array.isArray(list)) return null;
            for (let i = 0; i < list.length; i++) {
                const row = list[i];
                if (row && String(row.point_id) === want) {
                    return row.id || true;
                }
            }
        } catch (e) {}
        return null;
    }

    function _syncMarkerVisualState(ctx, marker, isMarked, recordId = true, resetSelected = false) {
        if (!marker) return;
        try {
            if (!marker._map && ctx && ctx.map) {
                marker._map = ctx.map;
            }
            const gisVm = resolveGisVm(ctx);
            const markedOp = (
                gisVm && gisVm.$store && gisVm.$store.getters &&
                typeof gisVm.$store.getters.markedMarkerOpacity === 'number'
            ) ? gisVm.$store.getters.markedMarkerOpacity : 0.3;
            const desiredOp = isMarked ? markedOp : 1;
            const applyOpacity = (op) => {
                if (typeof marker.setOpacity === 'function') {
                    marker.setOpacity(op);
                } else if (marker.options) {
                    marker.options.opacity = op;
                }
            };

            if (typeof marker.setMarkedState === 'function') {
                marker.setMarkedState(isMarked ? 'marked' : 'default', isMarked ? (recordId || true) : false, true);
            }
            marker.marked = isMarked ? (recordId || true) : false;
            if (!marker.options || marker.options.opacity !== desiredOp) {
                applyOpacity(desiredOp);
            }
            if (resetSelected && typeof marker.setState === 'function') {
                marker.setState('default', undefined, false);
                if (!marker.options || marker.options.opacity !== desiredOp) {
                    applyOpacity(desiredOp);
                }
            }
        } catch (e) {}
    }

    function _extractStoreMarkMap(state) {
        const out = new Map();
        const list = state && state.markPointList;
        if (Array.isArray(list)) {
            for (let i = 0; i < list.length; i++) {
                const row = list[i];
                if (row && row.point_id !== undefined && row.point_id !== null) {
                    out.set(String(row.point_id), row.id || true);
                }
            }
        }
        return out;
    }

    function _collectAllCandidateMarkers(ctx) {
        const out = [];
        const seen = new Set();
        const add = (m) => {
            if (!m || typeof m !== 'object' || seen.has(m)) return;
            seen.add(m);
            out.push(m);
        };
        const indiv = _collectIndividualMarkers(ctx);
        for (let i = 0; i < indiv.length; i++) add(indiv[i]);
        if (ctx && Array.isArray(ctx.markerList)) {
            for (let i = 0; i < ctx.markerList.length; i++) {
                const it = ctx.markerList[i];
                if (it && it.marker) add(it.marker);
            }
        }
        if (ctx && ctx.ciLayer && ctx.ciLayer.contextMenuMarker) {
            add(ctx.ciLayer.contextMenuMarker);
        }
        return out;
    }

    function _reconcileMarkedVisuals(ctx, opts = {}) {
        try {
            const c = ctx || findMapContext();
            if (!c || !c.ciLayer) return 0;
            const curMid = getCurrentWebMapId();
            const gisVm = resolveGisVm(c);
            const store = gisVm && gisVm.$store;
            const hasLiveStore = Boolean(store && store.state && Array.isArray(store.state.markPointList));
            const markedMap = _extractStoreMarkMap(store && store.state);
            if (!hasLiveStore) {
                const cachedIds = cachedMarkedIdsByMap.get(curMid);
                if (cachedIds) {
                    for (const cid of cachedIds) {
                        const key = String(cid);
                        if (!markedMap.has(key)) markedMap.set(key, true);
                    }
                }
            }
            const unmarkedIds = new Set();
            if (opts && opts.unmarkedIds instanceof Set) {
                for (const id of opts.unmarkedIds) unmarkedIds.add(String(id));
            }
            const overrides = recentMarkOverrideByMap.get(curMid);
            if (overrides) {
                const now = Date.now();
                for (const [pid, entry] of overrides.entries()) {
                    if (!entry || now > entry.until) {
                        overrides.delete(pid);
                        continue;
                    }
                    if (entry.marked) {
                        if (!markedMap.has(pid)) markedMap.set(pid, true);
                        unmarkedIds.delete(pid);
                    } else {
                        markedMap.delete(pid);
                        unmarkedIds.add(pid);
                    }
                }
            }
            for (const uid of unmarkedIds) {
                markedMap.delete(uid);
            }
            const skipSelectedPopup = opts.skipSelectedPopup !== false;
            const keepSelected = Boolean(opts.keepSelected);
            const popupOpen = Boolean(findPopupContainer());
            const selectedMarker = c.ciLayer.contextMenuMarker || null;
            const pool = _collectAllCandidateMarkers(c);
            let fixed = 0;

            for (let i = 0; i < pool.length; i++) {
                const m = pool[i];
                const pidRaw = m.pointId ?? m.id ?? (m.attrs && m.attrs.marker && m.attrs.marker.id);
                if (pidRaw === undefined || pidRaw === null) continue;
                const pid = String(pidRaw);
                if (markedMap.has(pid)) {
                    if (skipSelectedPopup && popupOpen && m === selectedMarker) continue;
                    const op = m.options && typeof m.options.opacity === 'number' ? m.options.opacity : 1;
                    if (!m.marked || op >= 0.95) {
                        _syncMarkerVisualState(c, m, true, markedMap.get(pid), !keepSelected);
                        fixed += 1;
                    }
                } else if (unmarkedIds.has(pid)) {
                    const op = m.options && typeof m.options.opacity === 'number' ? m.options.opacity : 1;
                    if (m.marked || op < 0.95) {
                        _syncMarkerVisualState(c, m, false, false, true);
                        fixed += 1;
                    }
                }
            }

            if (fixed > 0 || opts.forceRedraw) {
                _redrawCiLayer(c.ciLayer);
            }
            return fixed;
        } catch (e) {
            return 0;
        }
    }

    function _ensureMarkRedrawHooks(ctx) {
        try {
            const c = ctx || findMapContext();
            if (!c) return;
            const gisVm = resolveGisVm(c);
            const store = gisVm && gisVm.$store;
            if (store && typeof store.subscribe === 'function' && !hookedMarkStores.has(store)) {
                hookedMarkStores.add(store);
                let prevMap = _extractStoreMarkMap(store.state);
                store.subscribe((mutation, state) => {
                    if (!mutation || mutation.type !== 'updateMarkPointList') return;
                    const mid = getCurrentWebMapId();
                    const nextMap = _extractStoreMarkMap(state);

                    const changed = new Map();
                    const unmarkedSet = new Set();
                    for (const [pid, recId] of nextMap.entries()) {
                        if (!prevMap.has(pid)) changed.set(pid, { marked: true, recId });
                    }
                    for (const pid of prevMap.keys()) {
                        if (!nextMap.has(pid)) {
                            changed.set(pid, { marked: false, recId: false });
                            unmarkedSet.add(pid);
                        }
                    }
                    prevMap = nextMap;

                    if (changed.size >= 1 && changed.size <= 50) {
                        for (const [pid, info] of changed.entries()) {
                            _setLocalMarkOverride(mid, pid, info.marked, 4000);
                        }
                    }
                    const nextSet = new Set(nextMap.keys());
                    _applyLocalOverridesToSet(mid, nextSet);
                    cachedMarkedIdsByMap.set(mid, nextSet);
                    cachedMarkedFetchedAt.set(mid, Date.now());

                    if (changed.size >= 1 && changed.size <= 50) {
                        const curCtx = findMapContext();
                        const ci = curCtx && curCtx.ciLayer;
                        const pool = _collectAllCandidateMarkers(curCtx);
                        for (let i = 0; i < pool.length; i++) {
                            const m = pool[i];
                            const pid = m.pointId ?? m.id ?? (m.attrs && m.attrs.marker && m.attrs.marker.id);
                            if (pid === undefined || pid === null) continue;
                            const info = changed.get(String(pid));
                            if (info) {
                                _syncMarkerVisualState(curCtx, m, info.marked, info.recId, false);
                            }
                        }
                        _redrawCiLayer(ci);
                        // Outlast HoYoLAB async popupopen y.getInfo() overwrite and 500ms destroy cleanup
                        for (const delayMs of [150, 650, 1300]) {
                            setTimeout(() => {
                                _reconcileMarkedVisuals(findMapContext(), {
                                    skipSelectedPopup: true,
                                    keepSelected: true,
                                    unmarkedIds: unmarkedSet,
                                    forceRedraw: delayMs === 150
                                });
                            }, delayMs);
                        }
                        if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                            pushNearbyPinsToServer(
                                playerLastState.lat,
                                playerLastState.lng,
                                mid,
                                playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                                { force: true }
                            );
                        }
                    }
                });
            }

            if (c.map && typeof c.map.on === 'function' && !hookedPopupMaps.has(c.map)) {
                hookedPopupMaps.add(c.map);
                c.map.on('popupopen', (ev) => {
                    const popupRef = ev && ev.popup ? ev.popup : null;
                    lastOpenedPopupRef = popupRef;
                    for (const delayMs of [50, 200, 500, 1100, 2000]) {
                        setTimeout(() => {
                            try {
                                injectPopupNavigateButton(popupRef);
                            } catch (e) {}
                        }, delayMs);
                    }
                });
                c.map.on('popupclose', (ev) => {
                    if (!ev || !ev.popup || ev.popup === lastOpenedPopupRef) {
                        lastOpenedPopupRef = null;
                    }
                    try {
                        const curCtx = findMapContext();
                        const ci = curCtx && curCtx.ciLayer;
                        if (!ci) return;
                        const cm = ci.contextMenuMarker;
                        if (cm) {
                            const pid = cm.pointId ?? cm.id ?? (cm.attrs && cm.attrs.marker && cm.attrs.marker.id);
                            const storeRec = _getMarkedRecordFromStore(curCtx, pid);
                            const curGis = resolveGisVm(curCtx);
                            const hasLiveStore = Boolean(curGis && curGis.$store && curGis.$store.state && Array.isArray(curGis.$store.state.markPointList));
                            const localOv = _getLocalMarkOverride(getCurrentWebMapId(), pid);
                            const isMarked = (localOv !== null)
                                ? localOv
                                : (hasLiveStore ? Boolean(storeRec) : Boolean(storeRec || cm.marked));
                            _syncMarkerVisualState(curCtx, cm, isMarked, isMarked ? (storeRec || cm.marked || true) : false, true);
                            ci.contextMenuMarker = null;
                        }
                        _reconcileMarkedVisuals(curCtx, { skipSelectedPopup: false, forceRedraw: true });
                        for (const delayMs of [120, 650]) {
                            setTimeout(() => {
                                _reconcileMarkedVisuals(findMapContext(), {
                                    skipSelectedPopup: false,
                                    forceRedraw: delayMs === 120
                                });
                            }, delayMs);
                        }
                    } catch (e) {}
                });
            }
        } catch (e) {}
    }

    function _findPopupVueComponent(popupRoot) {
        if (!popupRoot) return null;
        const hasPinFields = (vm) => {
            if (!vm || typeof vm !== 'object') return false;
            return Boolean(
                vm.marker || vm.info || vm.point || vm.pointInfo || vm.config ||
                (vm.$props && (vm.$props.marker || vm.$props.info || vm.$props.config))
            );
        };
        const tryNode = (el) => {
            let vm = el && el.__vue__ ? el.__vue__ : null;
            for (let d = 0; vm && d < 6; d++) {
                if (hasPinFields(vm)) return vm;
                vm = vm.$parent || null;
            }
            return null;
        };
        try {
            const inner = (typeof popupRoot.querySelector === 'function')
                ? popupRoot.querySelector('.map-popup, .expand-popup')
                : null;
            const vmInner = tryNode(inner);
            if (vmInner) return vmInner;
            const vmSelf = tryNode(popupRoot);
            if (vmSelf) return vmSelf;
            if (typeof popupRoot.querySelectorAll === 'function') {
                const nodes = popupRoot.querySelectorAll('div, section, article');
                const lim = Math.min(nodes.length, 25);
                for (let i = 0; i < lim; i++) {
                    const found = tryNode(nodes[i]);
                    if (found) return found;
                }
            }
        } catch (e) {}
        return null;
    }

    function _extractPopupPinPayload(popupRef) {
        const ctx = findMapContext();
        const mid = getCurrentWebMapId();
        const ref = popupRef || lastOpenedPopupRef;
        let marker = (ref && ref._source) || (ctx && ctx.ciLayer && ctx.ciLayer.contextMenuMarker) || null;
        let latlng = (marker && typeof marker.getLatLng === 'function')
            ? marker.getLatLng()
            : ((ref && typeof ref.getLatLng === 'function') ? ref.getLatLng() : null);

        // Inspect open popup Vue component (`.map-popup.__vue__` / `.expand-popup.__vue__`)
        // so MutationObserver re-injections and async popup loads always recover full metadata.
        let vuePinId = null;
        let vueMarkerObj = null;
        let vueConfigObj = null;
        let vueMapId = null;
        try {
            const popEl = findPopupContainer();
            let vm = _findPopupVueComponent(popEl);
            for (let depth = 0; vm && depth < 5; depth++) {
                if (!vueConfigObj && vm.config && typeof vm.config === 'object') vueConfigObj = vm.config;
                if (!vueConfigObj && vm.$props && vm.$props.config && typeof vm.$props.config === 'object') vueConfigObj = vm.$props.config;
                if (vueMapId === null && vm.mapId !== undefined && vm.mapId !== null && !isNaN(Number(vm.mapId))) {
                    vueMapId = parseInt(vm.mapId, 10);
                }
                const cands = [
                    vm.marker, vm.info, vm.point, vm.pointInfo, vm.data,
                    vm.$props && vm.$props.marker, vm.$props && vm.$props.info
                ];
                for (const obj of cands) {
                    if (!obj || typeof obj !== 'object') continue;
                    const yVal = Number(obj.y_pos ?? obj.lat);
                    const xVal = Number(obj.x_pos ?? obj.lng);
                    if (Number.isFinite(yVal) && Number.isFinite(xVal)) {
                        if (!latlng) latlng = { lat: yVal, lng: xVal };
                        if (!vueMarkerObj) vueMarkerObj = obj;
                        if (vuePinId === null && obj.id !== undefined && obj.id !== null) vuePinId = String(obj.id);
                        break;
                    }
                }
                if (latlng && vueMarkerObj && vueConfigObj) break;
                vm = vm.$parent || null;
            }
        } catch (e) {}

        if (!marker && ctx && latlng && typeof latlng.lat === 'number' && typeof latlng.lng === 'number') {
            const found = _findMarkerForPin(ctx, vuePinId, latlng.lat, latlng.lng);
            if (found && found.marker) {
                marker = found.marker;
                latlng = found.latlng || latlng;
            }
        }
        if (!latlng || !Number.isFinite(latlng.lat) || !Number.isFinite(latlng.lng)) {
            return null;
        }
        const resolvedMid = (vueMapId !== null && Number.isFinite(vueMapId)) ? vueMapId : mid;
        const lookup = ctx ? _getMarkerMetaLookup(ctx) : null;
        const meta = (marker || vueMarkerObj) ? _resolveRichPinItem(vueMarkerObj, marker, lookup) : null;
        const pinId = (meta && meta.id !== null && meta.id !== undefined)
            ? String(meta.id)
            : (vuePinId || `web_${resolvedMid}_${Math.round(latlng.lat)}_${Math.round(latlng.lng)}`);
        const cfgName = (vueConfigObj && vueConfigObj.name) ? String(vueConfigObj.name).trim() : '';
        const labelId = (meta && meta.labelId !== null && meta.labelId !== undefined)
            ? Number(meta.labelId)
            : ((vueConfigObj && vueConfigObj.id !== undefined && vueConfigObj.id !== null && Number.isFinite(Number(vueConfigObj.id)))
                ? Number(vueConfigObj.id)
                : null);
        const parentLabelId = (meta && meta.parentLabelId !== null && meta.parentLabelId !== undefined)
            ? Number(meta.parentLabelId)
            : ((vueConfigObj && vueConfigObj.parent_id !== undefined && vueConfigObj.parent_id !== null && Number.isFinite(Number(vueConfigObj.parent_id)))
                ? Number(vueConfigObj.parent_id)
                : null);
        const name = (meta && meta.name) ? meta.name : (getPopupTitle() || cfgName || tr('page.sync.default_pin_title'));
        return {
            id: pinId,
            name,
            category: (meta && meta.labelName) ? meta.labelName : cfgName,
            lat: Number(latlng.lat),
            lng: Number(latlng.lng),
            map_id: resolvedMid,
            label_id: labelId,
            parent_label_id: parentLabelId,
            floor_id: (meta && meta.floorId !== null && meta.floorId !== undefined) ? String(meta.floorId) : null,
            floor_name: (meta && meta.floorName) ? meta.floorName : '',
            is_underground: Boolean(meta && meta.isUnderground),
            image_url: (meta && meta.imageUrl) ? meta.imageUrl : '',
            description: (meta && meta.description) ? meta.description : ''
        };
    }

    function _isPinCurrentlyLocked(pinId) {
        if (!activeLockedPin || pinId === undefined || pinId === null) return false;
        return String(activeLockedPin.id) === String(pinId);
    }

    function syncPopupNavigateButtonState(btn = null) {
        const targetBtn = btn || document.getElementById('gh-popup-navigate-btn');
        if (!targetBtn) return;
        const locked = _isPinCurrentlyLocked(targetBtn.dataset.ghPinId);
        targetBtn.classList.toggle('is-locked', locked);
        targetBtn.textContent = locked
            ? `✕ ${tr('page.nav.unlock_btn')}`
            : `🧭 ${tr('page.nav.navigate_btn')}`;
        targetBtn.title = tr(locked ? 'page.nav.unlock_title' : 'page.nav.navigate_title');
    }

    async function sendUnlockTargetCommand() {
        try {
            const res = await apiRequest('/nav_command', {
                method: 'POST',
                body: {
                    action: 'unlock_target',
                    tab_id: GH_TAB_ID,
                    user_initiated: true
                }
            });
            setActiveLockedPin(null);
            if (res && res.ok) {
                setLog(tr('page.nav.unlocked'), '#94a3b8');
                return true;
            }
        } catch (e) {}
        return false;
    }

    function renderTargetLockPill() {
        try {
            if (!activeLockedPin || !activeLockedPin.id) {
                if (ghLockPillEl) {
                    ghLockPillEl.remove();
                    ghLockPillEl = null;
                }
                return;
            }
            injectThemeStyles();
            if (!ghLockPillEl || !document.body.contains(ghLockPillEl)) {
                ghLockPillEl = document.createElement('div');
                ghLockPillEl.id = 'gh-target-lock-pill';
                ghLockPillEl.className = 'gh-root';
                document.body.appendChild(ghLockPillEl);
            }
            ghLockPillEl.textContent = '';

            const nameEl = document.createElement('span');
            nameEl.className = 'gh-lock-pill-name';
            const displayName = String(activeLockedPin.name || `Pin #${activeLockedPin.id}`);
            nameEl.textContent = `🧭 ${displayName}`;
            nameEl.title = displayName;
            nameEl.addEventListener('click', () => {
                try {
                    const ctx = findMapContext();
                    const lat = Number(activeLockedPin.lat);
                    const lng = Number(activeLockedPin.lng);
                    const targetMapId = Number(activeLockedPin.map_id || 2);
                    if (ctx && ctx.map && Number.isFinite(lat) && Number.isFinite(lng) && targetMapId === getCurrentWebMapId()) {
                        setAutoPanPinned(false, { silent: true });
                        ctx.map.setView([lat, lng], Math.max(ctx.map.getZoom(), -0.5), { animate: true });
                    }
                } catch (e) {}
            });

            const cancelBtn = document.createElement('button');
            cancelBtn.type = 'button';
            cancelBtn.className = 'gh-lock-pill-cancel';
            cancelBtn.textContent = `✕ ${tr('page.nav.lock_pill_cancel')}`;
            cancelBtn.title = tr('page.nav.unlock_title');
            cancelBtn.addEventListener('click', async (evt) => {
                evt.preventDefault();
                evt.stopPropagation();
                cancelBtn.disabled = true;
                try {
                    await sendUnlockTargetCommand();
                } finally {
                    cancelBtn.disabled = false;
                }
            });

            ghLockPillEl.append(nameEl, cancelBtn);
        } catch (e) {}
    }

    function setActiveLockedPin(pinOrNull) {
        if (pinOrNull && typeof pinOrNull === 'object' && pinOrNull.id !== undefined && pinOrNull.id !== null && String(pinOrNull.id).trim() !== '') {
            activeLockedPin = {
                id: String(pinOrNull.id),
                name: String(pinOrNull.name || pinOrNull.category || `Pin #${pinOrNull.id}`),
                lat: Number(pinOrNull.lat),
                lng: Number(pinOrNull.lng),
                map_id: Number(pinOrNull.map_id || getCurrentWebMapId())
            };
        } else {
            activeLockedPin = null;
        }
        renderTargetLockPill();
        syncPopupNavigateButtonState();
    }

    function injectPopupNavigateButton(popupRef) {
        const popup = findPopupContainer();
        if (!popup) return;
        const anchorBtn = findGhimButton() || findAlreadyMarkedButton();
        const footerContainer = (anchorBtn && anchorBtn.parentElement)
            ? anchorBtn.parentElement
            : popup.querySelector('.map-popup__footer, .expand-popup__footer, [class*="popup__footer"]');
        if (!footerContainer) return;
        const pinPayload = _extractPopupPinPayload(popupRef);
        if (!pinPayload) return;

        const existing = footerContainer.querySelector('#gh-popup-navigate-btn');
        if (existing) {
            existing.dataset.ghPinId = pinPayload.id;
            syncPopupNavigateButtonState(existing);
            return;
        }

        injectThemeStyles();
        const navBtn = document.createElement('button');
        navBtn.id = 'gh-popup-navigate-btn';
        navBtn.type = 'button';
        navBtn.className = 'gh-root';
        navBtn.dataset.ghPinId = pinPayload.id;
        syncPopupNavigateButtonState(navBtn);
        navBtn.addEventListener('click', async (evt) => {
            evt.preventDefault();
            evt.stopPropagation();
            navBtn.disabled = true;
            try {
                const freshPin = _extractPopupPinPayload(lastOpenedPopupRef || popupRef) || pinPayload;
                navBtn.dataset.ghPinId = freshPin.id;
                if (_isPinCurrentlyLocked(freshPin.id)) {
                    await sendUnlockTargetCommand();
                    return;
                }
                const res = await apiRequest('/nav_command', {
                    method: 'POST',
                    body: {
                        action: 'lock_target',
                        pin: freshPin,
                        auto_switch: true,
                        tab_id: GH_TAB_ID,
                        user_initiated: true
                    }
                });
                if (res && res.ok) {
                    setActiveLockedPin(res.target || freshPin);
                    setLog(tr('page.nav.navigate_locked', { name: freshPin.name }), '#fbbf24');
                } else {
                    setLog(tr('page.nav.navigate_err'), '#f59e0b');
                }
            } catch (e) {
                setLog(tr('page.nav.navigate_err'), '#f59e0b');
            } finally {
                navBtn.disabled = false;
                syncPopupNavigateButtonState(navBtn);
            }
        });
        if (anchorBtn && anchorBtn.parentElement === footerContainer) {
            footerContainer.insertBefore(navBtn, anchorBtn.nextSibling);
        } else {
            footerContainer.appendChild(navBtn);
        }
    }

    function installPopupNavigateObserver() {
        try {
            if (popupNavObserver || typeof MutationObserver !== 'function') return;
            // Observe document.body so the observer survives map switches (Teyvat <-> Enkanomiya
            // <-> Chasm) that tear down and recreate `.leaflet-popup-pane`.
            const root = document.body || document.documentElement;
            if (!root) return;
            popupNavObserver = new MutationObserver(() => {
                // Fast O(1) bailout when no map popup is open or our button is already mounted.
                const hasPopup = document.querySelector('.map-popup, .expand-popup, .leaflet-popup:not(.leaflet-popup-pane)');
                if (!hasPopup) return;
                if (hasPopup.querySelector('#gh-popup-navigate-btn')) return;
                if (popupNavDebounceTimer) clearTimeout(popupNavDebounceTimer);
                popupNavDebounceTimer = setTimeout(() => {
                    popupNavDebounceTimer = null;
                    try {
                        if (findPopupContainer()) {
                            injectPopupNavigateButton(lastOpenedPopupRef);
                        }
                    } catch (e) {}
                }, 40);
            });
            popupNavObserver.observe(root, { childList: true, subtree: true });
        } catch (e) {}
    }

    function _findMarkerForPin(ctx, pinId, lat, lng) {
        const lookup = _getMarkerMetaLookup(ctx);
        const wantId = (pinId !== null && pinId !== undefined) ? String(pinId) : null;
        const radius = 6;
        let best = null;
        let bestDist = Infinity;
        const consider = (item, m, ll) => {
            if (!ll || typeof ll.lat !== 'number' || typeof ll.lng !== 'number') return false;
            const meta = _resolveRichPinItem(item, m, lookup);
            const d = Math.hypot(ll.lat - lat, ll.lng - lng);
            if (wantId !== null && meta.id !== null && meta.id !== undefined && String(meta.id) === wantId) {
                best = { marker: m, latlng: ll, rawItem: item };
                bestDist = -1;
                return true;
            }
            if (bestDist >= 0 && d < bestDist && d <= 2.5) {
                bestDist = d;
                best = { marker: m, latlng: ll, rawItem: item };
            }
            return false;
        };
        // Same source as the nearby feed: individual markers, including pins hidden in a cluster
        for (const m of _collectIndividualMarkers(ctx)) {
            const ll = (m && typeof m.getLatLng === 'function') ? m.getLatLng() : null;
            if (!ll || Math.abs(ll.lat - lat) > radius || Math.abs(ll.lng - lng) > radius) continue;
            if (consider(null, m, ll)) break;
        }
        if (!best) {
            for (const m of _collectAllCandidateMarkers(ctx)) {
                const ll = (m && typeof m.getLatLng === 'function') ? m.getLatLng() : null;
                if (!ll || Math.abs(ll.lat - lat) > radius || Math.abs(ll.lng - lng) > radius) continue;
                if (consider(null, m, ll)) break;
            }
        }
        if (best && ctx.map && typeof ctx.map.latLngToContainerPoint === 'function') {
            best.point = ctx.map.latLngToContainerPoint(best.latlng);
        }
        return best;
    }

    function _waitUntil(cond, timeoutMs) {
        return new Promise((resolve) => {
            const end = Date.now() + timeoutMs;
            const step = () => {
                let ok = false;
                try { ok = !!cond(); } catch (e) { ok = false; }
                if (ok || Date.now() >= end) { resolve(ok); return; }
                setTimeout(step, 50);
            };
            step();
        });
    }

    async function _openPinPopup(ctx, found) {
        const map = ctx.map;
        const marker = found.marker;
        let restoreView = null;
        if (marker && !marker._map && typeof map.setView === 'function') {
            // Pin is inside a cluster at this zoom: bring it out (view is restored afterwards)
            restoreView = { center: map.getCenter(), zoom: map.getZoom() };
            const maxZ = typeof map.getMaxZoom === 'function' ? map.getMaxZoom() : 4;
            const zoom = Math.max(map.getZoom(), (Number.isFinite(maxZ) ? maxZ : 4) - 1);
            map.setView(found.latlng, zoom, { animate: false });
            await _waitUntil(() => !!marker._map, 1500);
        }
        const point = map.latLngToContainerPoint(found.latlng);
        const evt = {
            type: 'click',
            latlng: found.latlng,
            containerPoint: point,
            layerPoint: typeof map.containerPointToLayerPoint === 'function' ? map.containerPointToLayerPoint(point) : point,
            originalEvent: new MouseEvent('click'),
            target: map
        };
        let opened = false;
        if (marker && ctx.ciLayer && typeof ctx.ciLayer.onMarkerClick === 'function') {
            try { ctx.ciLayer.onMarkerClick(evt, [{ data: marker }]); opened = true; } catch (e) { opened = false; }
        }
        if (!opened && marker && typeof marker.openPopup === 'function') {
            try { marker.openPopup(); opened = true; } catch (e) { opened = false; }
        }
        if (!opened && ctx.ciLayer && typeof ctx.ciLayer._executeListeners === 'function') {
            try { ctx.ciLayer._executeListeners(evt); } catch (e) { /* fall through to the poll timeout */ }
        }
        return restoreView;
    }

    async function handleNavMarkPin(data) {
        const task = navMarkChain.then(async () => {
            const action = (data && data.action === 'unmark') ? 'unmark' : 'mark';
            const seq = typeof data._seq === 'number' ? data._seq : 0;
            const pinId = (data && data.pin_id !== undefined && data.pin_id !== null) ? String(data.pin_id) : '';
            const name = (data && data.name) || tr('page.sync.default_pin_title');
            let restoreView = null;
            let ctx = null;
            try {
                const lat = Number(data.lat);
                const lng = Number(data.lng);
                ctx = findMapContext();
                const targetMapId = data.map_id ? parseInt(data.map_id, 10) : 2;
                if (!ctx || !ctx.map || !Number.isFinite(lat) || !Number.isFinite(lng) || targetMapId !== getCurrentWebMapId()) {
                    _sendNavMarkFeedback(action, 'error', name, seq, pinId);
                    return;
                }
                _ensureMarkRedrawHooks(ctx);
                const found = _findMarkerForPin(ctx, data.pin_id, lat, lng);
                if (!found) {
                    setLog(tr('page.nav.pin_not_found', { name }), '#f59e0b');
                    _sendNavMarkFeedback(action, 'not_found', name, seq, pinId);
                    return;
                }
                const markerPid = found.marker && (found.marker.pointId ?? found.marker.id);
                const targetPid = pinId || (markerPid !== undefined && markerPid !== null ? String(markerPid) : '');
                const storeRec = _getMarkedRecordFromStore(ctx, targetPid);
                if (action === 'mark' && ((found.marker && found.marker.marked) || storeRec)) {
                    _syncMarkerVisualState(ctx, found.marker, true, storeRec || (found.marker && found.marker.marked) || true, true);
                    _redrawCiLayer(ctx.ciLayer);
                    _sendNavMarkFeedback(action, 'already_marked', name, seq, targetPid);
                    if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                        pushNearbyPinsToServer(
                            playerLastState.lat,
                            playerLastState.lng,
                            targetMapId,
                            playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                            { force: true }
                        );
                    }
                    return;
                }

                if (findPopupContainer()) {
                    closeOpenPopup();
                    await _waitUntil(() => !findPopupContainer(), 600);
                }
                restoreView = await _openPinPopup(ctx, found);

                await _waitUntil(() => Boolean(findGhimButton() || findAlreadyMarkedButton()), 2600);
                const markBtn = findGhimButton();
                const unmarkBtn = findAlreadyMarkedButton();

                let status = null;
                let actionBtn = null;
                if (action === 'mark') {
                    if (markBtn) {
                        actionBtn = markBtn;
                    } else if (unmarkBtn) {
                        status = 'already_marked';
                    }
                } else {
                    if (unmarkBtn) {
                        actionBtn = unmarkBtn;
                    } else if (markBtn) {
                        status = 'already_unmarked';
                    }
                }

                if (actionBtn) {
                    const hadStoreBefore = Boolean(_getMarkedRecordFromStore(ctx, targetPid));
                    const hadMarkerBefore = Boolean(found.marker && found.marker.marked);
                    actionBtn.click();
                    const confirmed = await _waitUntil(() => {
                        const storeHas = Boolean(_getMarkedRecordFromStore(ctx, targetPid));
                        const markerHas = Boolean(found.marker && found.marker.marked);
                        if (action === 'mark') {
                            return Boolean(findAlreadyMarkedButton()) || (!hadStoreBefore && storeHas) || (!hadMarkerBefore && markerHas) || (markerHas && storeHas);
                        }
                        return Boolean(findGhimButton()) || (hadStoreBefore && !storeHas) || (hadMarkerBefore && !markerHas && !storeHas);
                    }, 3500);
                    if (confirmed) {
                        status = (action === 'mark') ? 'marked' : 'unmarked';
                    } else {
                        setLog(tr('page.sync.no_mark_btn'), '#f59e0b');
                        status = 'error';
                    }
                } else if (!status) {
                    setLog(tr('page.sync.no_mark_btn'), '#f59e0b');
                    status = 'error';
                }

                if (status === 'marked' || status === 'already_marked') {
                    _setLocalMarkOverride(targetMapId, targetPid, true, 8000);
                    const rec = _getMarkedRecordFromStore(ctx, targetPid) || (found.marker && found.marker.marked) || true;
                    _syncMarkerVisualState(ctx, found.marker, true, rec, true);
                    if (ctx.ciLayer && ctx.ciLayer.contextMenuMarker === found.marker) {
                        ctx.ciLayer.contextMenuMarker = null;
                    }
                    _redrawCiLayer(ctx.ciLayer);
                } else if (status === 'unmarked' || status === 'already_unmarked') {
                    _setLocalMarkOverride(targetMapId, targetPid, false, 8000);
                    _syncMarkerVisualState(ctx, found.marker, false, false, true);
                    if (ctx.ciLayer && ctx.ciLayer.contextMenuMarker === found.marker) {
                        ctx.ciLayer.contextMenuMarker = null;
                    }
                    _redrawCiLayer(ctx.ciLayer);
                }

                if (status === 'marked' || status === 'unmarked') {
                    setLog(tr(action === 'mark' ? 'page.nav.marked' : 'page.nav.unmarked', { name }), '#22c55e');
                }
                _sendNavMarkFeedback(action, status, name, seq, targetPid || pinId);

                closeOpenPopup();
                await _waitUntil(() => !findPopupContainer(), 400);
                if (restoreView && ctx.map && typeof ctx.map.setView === 'function') {
                    try { ctx.map.setView(restoreView.center, restoreView.zoom, { animate: false }); } catch (e) { /* ignore */ }
                    restoreView = null;
                }
                _redrawCiLayer(ctx.ciLayer);
                const unmarkedSet = (action === 'unmark' && targetPid) ? new Set([targetPid]) : null;
                for (const delayMs of [150, 700]) {
                    setTimeout(() => {
                        const curCtx = findMapContext();
                        if (found && found.marker && (status === 'marked' || status === 'unmarked')) {
                            const isM = status === 'marked';
                            _syncMarkerVisualState(curCtx || ctx, found.marker, isM, isM ? ((found.marker && found.marker.marked) || true) : false, true);
                        }
                        _reconcileMarkedVisuals(curCtx || ctx, {
                            skipSelectedPopup: false,
                            unmarkedIds: unmarkedSet,
                            forceRedraw: delayMs === 150
                        });
                    }, delayMs);
                }

                const mid = getCurrentWebMapId();
                _refreshMarkedPinIds(mid, true).finally(() => {
                    pushLanPinsSnapshot(findMapContext() || ctx, mid, true);
                    if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                        pushNearbyPinsToServer(
                            playerLastState.lat,
                            playerLastState.lng,
                            mid,
                            playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                            { force: true }
                        );
                    }
                });
            } catch (e) {
                console.error('[Genshin Helper] handleNavMarkPin error:', e);
                if (restoreView && ctx && ctx.map && typeof ctx.map.setView === 'function') {
                    try { ctx.map.setView(restoreView.center, restoreView.zoom, { animate: false }); } catch (err) { /* ignore */ }
                }
                _sendNavMarkFeedback(action, 'error', name, seq, pinId);
            }
        });
        navMarkChain = task.catch(() => {});
        return task;
    }

    // =========================================================================
    // 6. Popup Helpers
    // =========================================================================
    function getText(el) {
        if (!el) return '';
        try { return (el.innerText || el.textContent || '').trim(); } catch (e) { return ''; }
    }

    function _isVisibleEl(el) {
        if (!el) return false;
        if (el.offsetParent !== null) return true;
        try {
            const r = el.getBoundingClientRect();
            return r.width > 0 && r.height > 0;
        } catch (e) {
            return false;
        }
    }

    function findPopupContainer() {
        // Check inner popup selectors in strict priority order: a comma-joined querySelector
        // returns `.leaflet-popup` first in DOM tree order (which lacks `.__vue__`).
        const selectors = [
            '.expand-popup',
            '.map-popup',
            '.leaflet-popup-content',
            '.leaflet-popup:not(.leaflet-popup-pane)',
            '[class*="map-popup"]'
        ];
        for (let i = 0; i < selectors.length; i++) {
            const els = document.querySelectorAll(selectors[i]);
            for (let j = 0; j < els.length; j++) {
                if (_isVisibleEl(els[j])) return els[j];
            }
        }
        const dlg = document.querySelector('.el-dialog');
        return _isVisibleEl(dlg) ? dlg : null;
    }

    function closeOpenPopup() {
        try {
            const explicitBtns = document.querySelectorAll('.leaflet-popup-close-button, .map-popup__close, .map-popup-custom__close, .map-entrance-popup__close');
            explicitBtns.forEach(el => {
                if (el && el.offsetWidth > 0 && el.offsetHeight > 0 && typeof el.click === 'function') {
                    el.click();
                }
            });
            const ctx = findMapContext();
            if (ctx && ctx.map && typeof ctx.map.closePopup === 'function') {
                ctx.map.closePopup();
            }
        } catch(e) {}
    }

    function isButtonDisabled(el) {
        if (!el) return true;
        if (el.disabled === true) return true;
        if (typeof el.getAttribute === 'function' && el.getAttribute('aria-disabled') === 'true') return true;
        if (el.classList && (el.classList.contains('is-disabled') || el.classList.contains('disabled'))) return true;
        return false;
    }

    function normalizeBtnText(el) {
        return getText(el).replace(/\s+/g, ' ').trim().toLowerCase();
    }

    function findGhimButton() {
        try {
            const popup = findPopupContainer() || document;
            const candidates = Array.from(popup.querySelectorAll('.map-popup__btn, button, [role="button"]'));
            return candidates.find(el => {
                if (el.offsetWidth === 0 || el.offsetHeight === 0 || isButtonDisabled(el)) return false;
                const norm = normalizeBtnText(el);
                if (!norm) return false;
                if (
                    norm.includes('nhiều') ||
                    norm.includes('trong game') ||
                    norm.includes('mới') ||
                    norm.includes('đã') ||
                    norm.includes('bỏ') ||
                    norm.includes('hủy') ||
                    norm.includes('huỷ') ||
                    norm.includes('unmark') ||
                    norm.includes('marked')
                ) {
                    return false;
                }
                return norm === 'ghim' || norm === 'mark';
            });
        } catch (e) { return null; }
    }

    function findLayerButton() {
        try {
            const candidates = Array.from(document.querySelectorAll('.map-popup__entrance, [class*="map-popup__entrance"], .map-popup__special-btns > div, .map-popup__special-btns span'));
            return candidates.find(el => {
                if (el.offsetWidth === 0 || el.offsetHeight === 0) return false;
                const txt = getText(el);
                return (txt === 'Thám Hiểm Phân Tầng' || txt.includes('Thám Hiểm Phân Tầng') || txt.includes('Phân Tầng') || txt.includes('Underground')) && !txt.includes('Lối Vào');
            });
        } catch (e) { return null; }
    }

    function findAlreadyMarkedButton() {
        try {
            const popup = findPopupContainer() || document;
            const candidates = Array.from(popup.querySelectorAll('.map-popup__btn, button, [role="button"]'));
            return candidates.find(el => {
                if (el.offsetWidth === 0 || el.offsetHeight === 0 || isButtonDisabled(el)) return false;
                const norm = normalizeBtnText(el);
                return (
                    norm === 'bỏ ghim' ||
                    norm === 'hủy ghim' ||
                    norm === 'huỷ ghim' ||
                    norm === 'đã ghim' ||
                    norm === 'unmark' ||
                    norm === 'marked'
                );
            });
        } catch (e) { return null; }
    }

    function getPopupTitle() {
        let itemTitle = tr('page.sync.default_pin_title');
        const popup = findPopupContainer();
        if (popup) {
            const titleEl = popup.querySelector('.leaflet-popup-content h3, [class*="title"], h3, h4');
            if (titleEl) {
                const txt = getText(titleEl);
                if (txt) itemTitle = txt;
            }
        }
        return itemTitle;
    }

    function clickGhimAndNotify(btn, seq = 0, distM = null, ctx = null, marker = null) {
        const itemTitle = getPopupTitle();
        btn.click();
        if (!nativeFeedbackActive) playBeep();
        setLog(tr('page.sync.marked_toast', { title: itemTitle }), '#22c55e');
        const distStr = (typeof distM === 'number' && isFinite(distM))
            ? tr('page.sync.marked_dist', { dist: distM.toFixed(1) })
            : '';
        sendActionFeedback('SYNC_AND_MARK', 'marked', tr('page.sync.marked_title', { title: itemTitle }), distStr, seq);
        _waitUntil(() => Boolean(findAlreadyMarkedButton()) || Boolean(marker && marker.marked), 3000)
            .then((ok) => {
                const curCtx = ctx || findMapContext();
                const targetMarker = marker || (curCtx && curCtx.ciLayer && curCtx.ciLayer.contextMenuMarker) || null;
                if (ok && targetMarker) {
                    _syncMarkerVisualState(curCtx, targetMarker, true, targetMarker.marked || true, true);
                }
                _reconcileMarkedVisuals(curCtx, { skipSelectedPopup: false, forceRedraw: true });
                for (const delayMs of [150, 700]) {
                    setTimeout(() => {
                        const latestCtx = findMapContext();
                        if (targetMarker) {
                            _syncMarkerVisualState(latestCtx, targetMarker, true, targetMarker.marked || true, true);
                        }
                        _reconcileMarkedVisuals(latestCtx, { skipSelectedPopup: false, forceRedraw: delayMs === 150 });
                    }, delayMs);
                }
                const mid = (playerLastState && playerLastState.map_id) || getCurrentWebMapId();
                return _refreshMarkedPinIds(mid, true).finally(() => {
                    pushLanPinsSnapshot(findMapContext() || ctx, mid, true);
                    if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                        pushNearbyPinsToServer(
                            playerLastState.lat,
                            playerLastState.lng,
                            mid,
                            playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                            { force: true }
                        );
                    }
                });
            })
            .catch(() => {});
    }

    function playBeep() {
        try {
            const ctx = new (window.AudioContext || window.webkitAudioContext)();
            const osc = ctx.createOscillator();
            const gain = ctx.createGain();
            osc.type = 'sine';
            osc.frequency.setValueAtTime(659.25, ctx.currentTime);
            osc.frequency.setValueAtTime(880, ctx.currentTime + 0.08);
            gain.gain.setValueAtTime(0.12, ctx.currentTime);
            gain.gain.exponentialRampToValueAtTime(0.01, ctx.currentTime + 0.25);
            osc.connect(gain);
            gain.connect(ctx.destination);
            osc.start();
            osc.stop(ctx.currentTime + 0.25);
        } catch (e) {}
    }

    // =========================================================================
    // 7. Wave 4 Integration Bridge (window.__GH_BRIDGE__)
    // =========================================================================
    const NAV_EXCLUDED_PARENT_LABELS = new Set([1, 426]);

    function _markerLabelConfig(markerObj) {
        const cfg = markerObj && markerObj.attrs && markerObj.attrs.config;
        return (cfg && typeof cfg === 'object') ? cfg : null;
    }

    function _isNavLandmarkPin(item, markerObj) {
        if (!item && !markerObj) return false;
        const cfg = _markerLabelConfig(markerObj);
        const parentId = Number(
            (cfg && cfg.parent_id) ?? (markerObj && markerObj.parentLabelId) ?? NaN
        );
        if (Number.isFinite(parentId) && NAV_EXCLUDED_PARENT_LABELS.has(parentId)) {
            return true;
        }
        const name = (
            (item && item.name) ||
            (item && item.rawItem && item.rawItem.name) ||
            (markerObj && markerObj.options && markerObj.options.title) ||
            ''
        ).toLowerCase();
        if (
            name.includes('dịch chuyển') ||
            name.includes('dich chuyen') ||
            name.includes('teleport') ||
            name.includes('thần tượng') ||
            name.includes('than tuong') ||
            name.includes('statue')
        ) {
            return true;
        }
        const lid = Number(
            (item && (item.label_id || item.labelId || (item.rawItem && item.rawItem.label_id))) ||
            (markerObj && (markerObj.label_id || (markerObj.options && markerObj.options.label_id))) ||
            0
        );
        return lid === 2 || lid === 3 || lid === 154;
    }

    function _extractMarkedIdsFromEnvelope(env) {
        const ids = new Set();
        const addItem = (entry) => {
            if (entry === null || entry === undefined) return;
            if (typeof entry === 'number' || typeof entry === 'string') {
                const s = String(entry).trim();
                if (s) ids.add(s);
                return;
            }
            if (typeof entry === 'object') {
                const rawId = entry.point_id ?? entry.map_point_id ?? entry.id ?? entry.pointId;
                if (rawId !== undefined && rawId !== null && String(rawId).trim() !== '') {
                    ids.add(String(rawId).trim());
                }
            }
        };
        try {
            const root = (env && env.data && typeof env.data === 'object') ? env.data : env;
            if (!root || typeof root !== 'object') return ids;
            const lists = [
                root.list,
                root.map_point_list,
                root.marked_list,
                root.point_list,
                root.point_ids,
                root.marked_ids,
                root.ids,
                root.data
            ];
            for (const candidate of lists) {
                if (Array.isArray(candidate)) {
                    for (const item of candidate) addItem(item);
                } else if (candidate && typeof candidate === 'object') {
                    for (const [k, v] of Object.entries(candidate)) {
                        if (Array.isArray(v)) {
                            for (const item of v) addItem(item);
                        } else if (v === true || v === 1) {
                            addItem(k);
                        }
                    }
                }
            }
        } catch (e) {}
        return ids;
    }

    function _refreshMarkedPinIds(mapId, force = false) {
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
            ? parseInt(mapId, 10)
            : getCurrentWebMapId();
        const now = Date.now();
        const lastTs = cachedMarkedFetchedAt.get(mid) || 0;
        if (!force && cachedMarkedIdsByMap.has(mid) && (now - lastTs) < 45000) {
            return Promise.resolve(cachedMarkedIdsByMap.get(mid));
        }
        if (markedFetchInFlight.has(mid)) {
            return markedFetchInFlight.get(mid);
        }
        const apiLang = (window.GHI18n && window.GHI18n.getLang() === 'vi') ? 'vi-vn' : 'en-us';
        const url = `https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/mark_map_point_list?map_id=${encodeURIComponent(mid)}&app_sn=ys_obc&lang=${encodeURIComponent(apiLang)}`;
        const p = _fetchHoyolabOriginJson(url)
            .then((env) => {
                if (env && (env.retcode === 0 || env.retcode === undefined)) {
                    const prevSize = cachedMarkedIdsByMap.has(mid) ? cachedMarkedIdsByMap.get(mid).size : -1;
                    const ids = _extractMarkedIdsFromEnvelope(env);
                    _applyLocalOverridesToSet(mid, ids);
                    cachedMarkedIdsByMap.set(mid, ids);
                    cachedMarkedFetchedAt.set(mid, Date.now());
                    if (ids.size !== prevSize && playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                        pushNearbyPinsToServer(
                            playerLastState.lat,
                            playerLastState.lng,
                            mid,
                            playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                            { force: true }
                        );
                    }
                    return ids;
                }
                return cachedMarkedIdsByMap.get(mid) || new Set();
            })
            .catch(() => cachedMarkedIdsByMap.get(mid) || new Set())
            .finally(() => {
                markedFetchInFlight.delete(mid);
            });
        markedFetchInFlight.set(mid, p);
        return p;
    }

    function _collectMarkedPinIds(ctx) {
        const markedSet = new Set();
        const curMapId = getCurrentWebMapId();
        const cached = cachedMarkedIdsByMap.get(curMapId);
        if (cached) {
            for (const id of cached) markedSet.add(id);
        }
        const ingest = (src) => {
            if (!src) return;
            if (Array.isArray(src)) {
                for (const item of src) {
                    if (typeof item === 'string' || typeof item === 'number') {
                        markedSet.add(String(item));
                    } else if (item && typeof item === 'object') {
                        const id = item.point_id ?? item.map_point_id ?? item.id;
                        if (id !== undefined && id !== null) markedSet.add(String(id));
                    }
                }
            } else if (src instanceof Set) {
                for (const id of src) markedSet.add(String(id));
            } else if (src instanceof Map) {
                for (const [k, v] of src.entries()) {
                    if (v) markedSet.add(String(k));
                }
            } else if (typeof src === 'object') {
                for (const [k, v] of Object.entries(src)) {
                    if (v === true || v === 1 || (typeof v === 'object' && v !== null && (v.is_marked || v.marked || v.point_id))) {
                        markedSet.add(String(k));
                    }
                }
            }
        };
        try {
            const gisVm = resolveGisVm(ctx);
            const st = gisVm && gisVm.$store && gisVm.$store.state ? gisVm.$store.state : null;
            if (st && typeof st === 'object') {
                const scanObj = (obj, depth) => {
                    if (!obj || typeof obj !== 'object' || depth > 2) return;
                    for (const [k, v] of Object.entries(obj)) {
                        if (!v) continue;
                        if (/mark|sign|collect/i.test(k)) {
                            ingest(v);
                        } else if (depth < 2 && typeof v === 'object' && !Array.isArray(v) && !(v instanceof Set) && !(v instanceof Map)) {
                            scanObj(v, depth + 1);
                        }
                    }
                };
                scanObj(st, 0);
            }
        } catch (e) {}
        _applyLocalOverridesToSet(curMapId, markedSet);
        return markedSet;
    }

    function _isPinMarked(item, markerObj, markedSet) {
        const raw = (item && item.rawItem) || item || {};
        const opts = (markerObj && markerObj.options) || {};
        const iconOpts = (opts.icon && opts.icon.options) || {};
        const id = raw.id ?? raw.point_id ?? opts.id ?? opts.point_id ?? (markerObj && (markerObj.pointId ?? markerObj.id));
        if (id !== undefined && id !== null) {
            const ov = _getLocalMarkOverride(getCurrentWebMapId(), id);
            if (ov !== null) {
                return ov;
            }
        }
        if (
            Boolean(raw.is_marked) ||
            Boolean(raw.marked) ||
            Boolean(raw.is_sign) ||
            Boolean(raw.sign) ||
            Boolean(raw.collected) ||
            Boolean(raw.checked) ||
            Number(raw.mark_status) === 1 ||
            Number(raw.status) === 1 ||
            Boolean(opts.isMarked) ||
            Boolean(opts.is_marked) ||
            Boolean(opts.marked) ||
            Boolean(markerObj && (markerObj.marked || markerObj.isMarked || markerObj.is_marked))
        ) {
            return true;
        }
        // Visual dimming check: HoYoLAB sets reduced opacity (< 0.95) or marked class on collected markers
        if (typeof opts.opacity === 'number' && opts.opacity < 0.95) {
            return true;
        }
        if (typeof raw.opacity === 'number' && raw.opacity < 0.95) {
            return true;
        }
        const clsStr = String(iconOpts.className || (markerObj && markerObj._icon && markerObj._icon.className) || '');
        if (/marked|signed|checked|collected/i.test(clsStr)) {
            return true;
        }
        if (markerObj && markerObj._icon && markerObj._icon.style && markerObj._icon.style.opacity) {
            const domOp = parseFloat(markerObj._icon.style.opacity);
            if (Number.isFinite(domOp) && domOp < 0.95) {
                return true;
            }
        }
        if (id !== undefined && id !== null && markedSet.has(String(id))) {
            return true;
        }
        return false;
    }

    function _getMarkerMetaLookup(ctx) {
        const list = ctx && Array.isArray(ctx.markerList) ? ctx.markerList : null;
        if (!list || list.length === 0) {
            return { byMarker: null, byId: null };
        }
        if (cachedMarkerIndexSrc === list && cachedMarkerIndexLen === list.length && cachedMarkerByMarker && cachedMarkerById) {
            return { byMarker: cachedMarkerByMarker, byId: cachedMarkerById };
        }
        const byMarker = new WeakMap();
        const byId = new Map();
        for (let i = 0; i < list.length; i++) {
            const it = list[i];
            if (!it || typeof it !== 'object') continue;
            if (it.marker && typeof it.marker === 'object') {
                byMarker.set(it.marker, it);
            }
            const rawId = it.id ?? it.point_id;
            if (rawId !== undefined && rawId !== null) {
                byId.set(String(rawId), it);
            }
        }
        cachedMarkerIndexSrc = list;
        cachedMarkerIndexLen = list.length;
        cachedMarkerByMarker = byMarker;
        cachedMarkerById = byId;
        return { byMarker, byId };
    }

    function _resolveFloorNameFromId(floorId) {
        if (floorId === null || floorId === undefined) return '';
        const fidStr = String(floorId).trim();
        if (!fidStr || fidStr === '0') return '';
        const gisEl = document.querySelector('.mhy-game-gis');
        const vm = gisEl && gisEl.__vue__;
        const list = (vm && Array.isArray(vm.pointGroupList) && vm.pointGroupList.length > 0)
            ? vm.pointGroupList
            : (vm && vm.$store && vm.$store.state && Array.isArray(vm.$store.state.pointGroupList)
                ? vm.$store.state.pointGroupList
                : null);
        if (!list || list.length === 0) return '';
        if (cachedFloorGroupListSrc !== list || cachedFloorGroupListLen !== list.length || !cachedFloorLabelMap) {
            const fmap = new Map();
            for (const g of list) {
                const floors = g && Array.isArray(g.floors) ? g.floors : [];
                for (let idx = 0; idx < floors.length; idx++) {
                    const fl = floors[idx];
                    if (!fl || fl.id === undefined || fl.id === null) continue;
                    const lbl = String(fl.floor_name_short || fl.floor_short || fl.floor_name || `B${idx + 1}`).trim();
                    if (lbl) fmap.set(String(fl.id).trim(), lbl);
                }
            }
            cachedFloorGroupListSrc = list;
            cachedFloorGroupListLen = list.length;
            cachedFloorLabelMap = fmap;
        }
        return cachedFloorLabelMap.get(fidStr) || '';
    }

    function _labelCacheKey(mapId) {
        const lang = (window.GHI18n && window.GHI18n.getLang() === 'vi') ? 'vi-vn' : 'en-us';
        return `${mapId}|${lang}`;
    }

    function _ensureLabelNames(mapId) {
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
            ? parseInt(mapId, 10)
            : getCurrentWebMapId();
        const key = _labelCacheKey(mid);
        if (labelNamesByKey.has(key)) return Promise.resolve(labelNamesByKey.get(key));
        if (labelFetchInFlight.has(key)) return labelFetchInFlight.get(key);
        const apiLang = key.split('|')[1];
        const url = `https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/label/tree?map_id=${encodeURIComponent(mid)}&app_sn=ys_obc&lang=${encodeURIComponent(apiLang)}`;
        const p = _fetchHoyolabOriginJson(url)
            .then((env) => {
                const names = new Map();
                const parents = new Map();
                const walk = (arr, parentId = null) => {
                    if (!Array.isArray(arr)) return;
                    for (const l of arr) {
                        if (!l || typeof l !== 'object') continue;
                        if (l.id !== undefined && l.id !== null) {
                            if (l.name) {
                                names.set(String(l.id), String(l.name).trim());
                            }
                            if (parentId !== null && parentId !== undefined) {
                                parents.set(String(l.id), Number(parentId));
                            }
                        }
                        walk(l.children, (l.id !== undefined && l.id !== null) ? l.id : parentId);
                    }
                };
                walk(env && env.data && env.data.tree, null);
                if (names.size > 0) {
                    labelNamesByKey.set(key, names);
                    labelParentByKey.set(key, parents);
                    // Re-push so the in-game HUD swaps generic titles for real category names
                    if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                        pushNearbyPinsToServer(
                            playerLastState.lat,
                            playerLastState.lng,
                            mid,
                            playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                            { force: true }
                        );
                    }
                }
                return names;
            })
            .catch(() => new Map())
            .finally(() => { labelFetchInFlight.delete(key); });
        labelFetchInFlight.set(key, p);
        return p;
    }

    function _labelNameFor(labelId, mapId = null) {
        if (labelId === null || labelId === undefined) return '';
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId))) ? parseInt(mapId, 10) : getCurrentWebMapId();
        const names = labelNamesByKey.get(_labelCacheKey(mid));
        if (!names) {
            _ensureLabelNames(mid);
            return '';
        }
        return names.get(String(labelId)) || '';
    }

    function _labelParentFor(labelId, mapId = null) {
        if (labelId === null || labelId === undefined) return null;
        const mid = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId))) ? parseInt(mapId, 10) : getCurrentWebMapId();
        const parents = labelParentByKey.get(_labelCacheKey(mid));
        if (!parents) {
            _ensureLabelNames(mid);
            return null;
        }
        const pid = parents.get(String(labelId));
        return (pid !== undefined && pid !== null && Number.isFinite(Number(pid))) ? Number(pid) : null;
    }

    function _resolveRichPinItem(item, markerObj, lookup) {
        const attrMarker = (markerObj && markerObj.attrs && markerObj.attrs.marker && typeof markerObj.attrs.marker === 'object')
            ? markerObj.attrs.marker
            : null;
        const baseRaw = (item && item.rawItem) || (item && Object.keys(item).length ? item : null) || attrMarker || {};
        const cfg = _markerLabelConfig(markerObj);
        const opts = (markerObj && markerObj.options) || {};
        let enriched = null;
        if (lookup) {
            if (lookup.byMarker && markerObj && typeof markerObj === 'object') {
                enriched = lookup.byMarker.get(markerObj) || null;
            }
            if (!enriched && lookup.byId) {
                const candId = baseRaw.id ?? baseRaw.point_id ?? opts.id ?? opts.point_id ?? (markerObj && (markerObj.pointId ?? markerObj.id));
                if (candId !== undefined && candId !== null) {
                    enriched = lookup.byId.get(String(candId)) || null;
                }
            }
        }
        const raw = enriched ? Object.assign({}, baseRaw, enriched) : baseRaw;
        const id = raw.id ?? raw.point_id ?? opts.id ?? opts.point_id ?? (markerObj && (markerObj.pointId ?? markerObj.id)) ?? null;
        const labelId = raw.label_id ?? raw.labelId ?? opts.label_id ?? opts.labelId ?? (markerObj && markerObj.label_id) ?? (cfg && cfg.id) ?? null;
        const pg = (raw.point_group && typeof raw.point_group === 'object') ? raw.point_group : null;
        const floorId = raw.floor_id ?? raw.floorId ?? (pg && pg.floor_id) ?? opts.floor_id ?? opts.floorId ?? (markerObj && markerObj.floor_id) ?? null;
        let floorName = String(raw.floor_name || raw.floor_name_short || opts.floor_name || '').trim();
        if (!floorName && floorId !== null && floorId !== undefined) {
            floorName = _resolveFloorNameFromId(floorId);
        }
        const iconSign = Number(raw.icon_sign ?? opts.icon_sign ?? 0) || 0;
        const zLevel = Number(raw.z_level ?? opts.z_level ?? 0) || 0;
        const isUnderground = (typeof raw.is_underground === 'boolean')
            ? raw.is_underground
            : ((typeof opts.is_underground === 'boolean')
                ? opts.is_underground
                : Boolean(
                    (floorId !== null && floorId !== undefined && String(floorId).trim() !== '' && String(floorId) !== '0') ||
                    (pg && (pg.group_id || pg.floor_id)) ||
                    zLevel !== 0 ||
                    [1, 2, 3].includes(iconSign)
                ));
        const infoObj = (raw.info && typeof raw.info === 'object') ? raw.info : null;
        const imageUrl = String(
            raw.image_url || raw.imageUrl || raw.picture || raw.img ||
            (infoObj && (infoObj.img || infoObj.picture || infoObj.image_url)) ||
            (Array.isArray(raw.picture_list) && raw.picture_list[0]) ||
            opts.image_url || opts.picture || opts.img || ''
        ).trim();
        const description = String(
            raw.description || raw.content || raw.desc ||
            (infoObj && (infoObj.content || infoObj.description || infoObj.desc)) ||
            opts.description || opts.content || opts.desc || ''
        ).trim();
        const rawParentLid = raw.parent_label_id ?? raw.parentLabelId ?? (cfg && cfg.parent_id) ?? (markerObj && markerObj.parentLabelId) ?? _labelParentFor(labelId);
        const parentLabelId = (rawParentLid !== null && rawParentLid !== undefined && rawParentLid !== '' && Number.isFinite(Number(rawParentLid)))
            ? Number(rawParentLid)
            : null;
        const labelName = String(raw.label_name || raw.labelName || (cfg && cfg.name) || _labelNameFor(labelId) || '').trim();
        const name = String(raw.name || raw.title || opts.title || opts.name || labelName || tr('page.sync.default_pin_title')).trim();
        return {
            raw,
            id,
            name,
            labelName,
            labelId,
            parentLabelId,
            floorId,
            floorName,
            isUnderground,
            iconSign,
            zLevel,
            imageUrl,
            description
        };
    }

    // Individual pin markers of the categories currently enabled on the web map.
    // Primary authoritative source: HoYoLAB `filterPanel` Vue component (`gisVm.$refs.filterPanel`
    // / `.filter-panel.__vue__`) `flattenedMarkerConfig` strictly filtered by `layerVisible === true`,
    // so unchecked categories (e.g. Snezhnaya Chest) never leak stale markers from
    // `ciLayer._latlngMarkers`. Falls back to `ciLayer._latlngMarkers` only when `filterPanel`
    // is not yet mounted.
    function _collectIndividualMarkers(ctx) {
        const out = [];
        const seen = new Set();
        const addMarker = (m) => {
            if (!m || seen.has(m)) return;
            if (m._childCount !== undefined && typeof m.getAllChildMarkers === 'function') {
                for (const child of m.getAllChildMarkers()) addMarker(child);
                return;
            }
            seen.add(m);
            out.push(m);
        };
        const collectEntryLayers = (fpVm, entry, selectedIds) => {
            if (!_isFilterEntryVisible(entry, selectedIds)) return;
            let layers = null;
            if (fpVm && typeof fpVm.getFilteredLayers === 'function') {
                try { layers = fpVm.getFilteredLayers(entry); } catch (e) { layers = null; }
            }
            if (!Array.isArray(layers) || layers.length === 0) {
                const lg = typeof entry.getLayerGroup === 'function' ? entry.getLayerGroup() : null;
                if (lg && lg._topClusterLevel && typeof lg._topClusterLevel.getAllChildMarkers === 'function') {
                    layers = lg._topClusterLevel.getAllChildMarkers();
                } else if (lg && typeof lg.getLayers === 'function') {
                    layers = lg.getLayers();
                } else if (typeof entry.getGroup2 === 'function') {
                    const g2 = entry.getGroup2();
                    if (Array.isArray(g2)) layers = g2;
                    else if (g2 && typeof g2.getLayers === 'function') layers = g2.getLayers();
                } else if (Array.isArray(entry.group2)) {
                    layers = entry.group2;
                }
            }
            if (Array.isArray(layers)) {
                for (let j = 0; j < layers.length; j++) addMarker(layers[j]);
            } else if (typeof entry.getLatLng === 'function') {
                addMarker(entry);
            }
        };

        try {
            const fpVm = resolveFilterPanelVm(ctx);
            const flatCfg = _getFilterPanelFlatConfig(fpVm);
            if (flatCfg && flatCfg.length > 0) {
                const selectedIds = _collectSelectedMarkerIds(fpVm);
                for (let i = 0; i < flatCfg.length; i++) {
                    collectEntryLayers(fpVm, flatCfg[i], selectedIds);
                }
                if (fpVm && fpVm.selfMarkerIdMap && typeof fpVm.selfMarkerIdMap === 'object') {
                    const selfVals = fpVm.selfMarkerIdMap instanceof Map
                        ? Array.from(fpVm.selfMarkerIdMap.values())
                        : Object.values(fpVm.selfMarkerIdMap);
                    for (let i = 0; i < selfVals.length; i++) {
                        collectEntryLayers(fpVm, selfVals[i], null);
                    }
                }
                return out;
            }
        } catch (e) {}

        const tree = ctx && ctx.ciLayer && ctx.ciLayer._latlngMarkers;
        if (!tree || typeof tree.all !== 'function') return out;
        const entries = tree.all();
        const groups = new Set();
        for (const entry of entries) {
            const m = entry && (entry.data || entry);
            if (!m) continue;
            const g = m._group || (m.__parent && m.__parent._group) || null;
            if (g && g._topClusterLevel && typeof g._topClusterLevel.getAllChildMarkers === 'function') {
                groups.add(g);
                continue;
            }
            addMarker(m);
        }
        for (const g of groups) {
            for (const child of g._topClusterLevel.getAllChildMarkers()) addMarker(child);
        }
        return out;
    }

    function queryNearbyUncollectedPins(lat, lng, radius = 150, limit = 25) {
        try {
            if (typeof lat !== 'number' || !Number.isFinite(lat) || typeof lng !== 'number' || !Number.isFinite(lng)) {
                return [];
            }
            const r = (typeof radius === 'number' && radius > 0) ? radius : 150;
            const maxItems = (typeof limit === 'number' && limit > 0) ? Math.floor(limit) : 25;
            const ctx = findMapContext();
            if (!ctx) return [];

            const markedSet = _collectMarkedPinIds(ctx);
            const lookup = _getMarkerMetaLookup(ctx);
            const results = [];
            const seenKeys = new Set();

            const pushPin = (item, markerObj, pinLat, pinLng) => {
                if (typeof pinLat !== 'number' || typeof pinLng !== 'number') return;
                if (!Number.isFinite(pinLat) || !Number.isFinite(pinLng)) return;

                const meta = _resolveRichPinItem(item, markerObj, lookup);
                if (_isNavLandmarkPin(meta.raw, markerObj) || _isNavLandmarkPin(item, markerObj)) return;
                if (_isPinMarked(meta.raw, markerObj, markedSet) || _isPinMarked(item, markerObj, markedSet)) return;

                const dLat = pinLat - lat;
                const dLng = pinLng - lng;
                const dist = Math.hypot(dLat, dLng);
                if (dist > r) return;

                const dedupKey = meta.id !== null ? `id:${meta.id}` : `${pinLat.toFixed(2)},${pinLng.toFixed(2)}`;
                if (seenKeys.has(dedupKey)) return;
                seenKeys.add(dedupKey);

                // Compass bearing in degrees (HoYoLAB CRS: -lat = North, +lng = East; 0° = North clockwise)
                const bearingRad = Math.atan2(dLng, -dLat);
                const bearingDeg = ((bearingRad * 180 / Math.PI) + 360) % 360;

                results.push({
                    id: meta.id,
                    labelId: meta.labelId,
                    label_id: meta.labelId,
                    parentLabelId: meta.parentLabelId,
                    parent_label_id: meta.parentLabelId,
                    name: meta.name,
                    label_name: meta.labelName || '',
                    lat: pinLat,
                    lng: pinLng,
                    distance: Number(dist.toFixed(2)),
                    bearing: Number(bearingDeg.toFixed(1)),
                    floorId: meta.floorId,
                    floor_id: meta.floorId,
                    floorName: meta.floorName || null,
                    floor_name: meta.floorName,
                    is_underground: meta.isUnderground,
                    image_url: meta.imageUrl,
                    description: meta.description
                });
            };

            // HoYoLAB clusters pins (MarkerCluster): the icon-layer tree holds CLUSTERS at low zoom,
            // whose centroid is not a real pin. Read the individual markers from the cluster group.
            const individual = _collectIndividualMarkers(ctx);
            for (const m of individual) {
                const ll = (m && typeof m.getLatLng === 'function') ? m.getLatLng() : null;
                if (!ll) continue;
                if (Math.abs(ll.lat - lat) > r || Math.abs(ll.lng - lng) > r) continue;
                pushPin(null, m, ll.lat, ll.lng);
            }

            results.sort((a, b) => a.distance - b.distance);
            return results.slice(0, maxItems);
        } catch (e) {
            return [];
        }
    }

    function pushNearbyPinsToServer(lat, lng, mapId = null, floorId = null, opts = {}) {
        try {
            if (typeof lat !== 'number' || !Number.isFinite(lat) || typeof lng !== 'number' || !Number.isFinite(lng)) {
                return false;
            }
            const resolvedMapId = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
                ? parseInt(mapId, 10)
                : getCurrentWebMapId();
            const resolvedFloorId = (floorId !== undefined && floorId !== null && String(floorId).trim() !== '')
                ? String(floorId)
                : null;
            const force = Boolean(opts && opts.force);
            const now = Date.now();
            const elapsed = now - lastNearbyPushTs;
            const moved = (lastNearbyPushLat !== null && lastNearbyPushLng !== null)
                ? Math.hypot(lat - lastNearbyPushLat, lng - lastNearbyPushLng)
                : Infinity;
            const contextChanged = (resolvedMapId !== lastNearbyPushMapId) || (resolvedFloorId !== lastNearbyPushFloorId);

            if (!force) {
                if (elapsed < 300 && !contextChanged) {
                    return false;
                }
                if (elapsed < 1500 && moved <= 15 && !contextChanged) {
                    return false;
                }
            }

            lastNearbyPushTs = now;
            lastNearbyPushLat = lat;
            lastNearbyPushLng = lng;
            lastNearbyPushMapId = resolvedMapId;
            lastNearbyPushFloorId = resolvedFloorId;

            // Keep authoritative HoYoLAB mark_map_point_list cache fresh (auto-re-pushes if marked set updates)
            _refreshMarkedPinIds(resolvedMapId, false);

            _ensureLabelNames(resolvedMapId);
            const curCtxForSig = findMapContext();
            const curCatSig = _getEnabledCategorySignature(curCtxForSig, resolvedMapId);
            let autoFilterChanged = false;
            if (curCatSig !== null && curCatSig !== lastEnabledCatSig) {
                lastEnabledCatSig = curCatSig;
                autoFilterChanged = true;
            }
            const filterChanged = Boolean((opts && opts.filterChanged) || autoFilterChanged);

            const bridge = window.__GH_BRIDGE__;
            const nearbyRaw = (bridge && typeof bridge.queryNearbyUncollectedPins === 'function')
                ? bridge.queryNearbyUncollectedPins(lat, lng, NEARBY_QUERY_RADIUS, NEARBY_QUERY_LIMIT)
                : queryNearbyUncollectedPins(lat, lng, NEARBY_QUERY_RADIUS, NEARBY_QUERY_LIMIT);

            const nearbyList = Array.isArray(nearbyRaw) ? nearbyRaw.slice(0, NEARBY_QUERY_LIMIT) : [];
            // Radius actually covered: a full list stops at its farthest pin, not at the query radius
            let coverageRadius = NEARBY_QUERY_RADIUS;
            if (nearbyList.length >= NEARBY_QUERY_LIMIT) {
                const last = nearbyList[nearbyList.length - 1];
                const lastDist = (last && typeof last.distance === 'number')
                    ? last.distance
                    : Math.hypot(Number(last.lat) - lat, Number(last.lng) - lng);
                if (Number.isFinite(lastDist)) coverageRadius = Math.min(NEARBY_QUERY_RADIUS, lastDist);
            }

            const pins = nearbyList.map((p) => ({
                // Position-based fallback id (no list index) so the id stays stable across pushes
                id: p.id !== null && p.id !== undefined ? p.id : `pin_${Number(p.lat).toFixed(1)}_${Number(p.lng).toFixed(1)}`,
                name: p.name || p.label_name || tr('page.sync.default_pin_title'),
                category: p.label_name || '',
                lat: p.lat,
                lng: p.lng,
                map_id: resolvedMapId,
                label_id: p.label_id ?? p.labelId ?? null,
                parent_label_id: p.parent_label_id ?? p.parentLabelId ?? null,
                floor_id: p.floor_id ?? p.floorId ?? null,
                floor_name: p.floor_name ?? p.floorName ?? '',
                is_underground: Boolean(p.is_underground),
                image_url: p.image_url || '',
                description: p.description || ''
            }));

            const userInitiated = Boolean(opts && (opts.userInitiated || opts.force));
            extRequest('/nearby_pins', {
                method: 'POST',
                body: {
                    tab_id: GH_TAB_ID,
                    tab_visible: !document.hidden,
                    tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
                    user_initiated: userInitiated,
                    filter_changed: filterChanged,
                    lat,
                    lng,
                    map_id: resolvedMapId,
                    floor_id: resolvedFloorId,
                    coverage_radius: Number(coverageRadius.toFixed(2)),
                    pins
                }
            }).catch(() => {});
            pushLanPinsSnapshot(curCtxForSig, resolvedMapId, force || filterChanged, userInitiated);
            return true;
        } catch (e) {
            return false;
        }
    }

    function handleLanSetFloor(data) {
        try {
            const ctx = findMapContext();
            if (!ctx || !ctx.map) return;
            const gisVm = resolveGisVm(ctx);
            const fidRaw = (data && data.floor_id !== undefined && data.floor_id !== null)
                ? String(data.floor_id).trim()
                : '';
            const mid = getCurrentWebMapId();
            if (!fidRaw || fidRaw === '0' || fidRaw.toLowerCase() === 'surface') {
                clearMinimapAuxTimers();
                lastMinimapLayerSig = 'surface';
                lastMinimapLayerSyncTs = Date.now();
                handleLayeredExploration(ctx, {
                    is_underground: false,
                    map_id: mid,
                    lat: data ? data.lat : undefined,
                    lng: data ? data.lng : undefined
                }, null);
                return;
            }
            const list = (gisVm && Array.isArray(gisVm.pointGroupList) && gisVm.pointGroupList.length > 0)
                ? gisVm.pointGroupList
                : (gisVm && gisVm.$store && Array.isArray(gisVm.$store.state.pointGroupList) ? gisVm.$store.state.pointGroupList : []);
            const group = list.find((g) => g && Array.isArray(g.floors) && g.floors.some((f) => f && String(f.id) === fidRaw)) || null;
            const lanLayerData = {
                is_underground: true,
                map_id: mid,
                group_id: group ? group.id : null,
                floor_id: fidRaw,
                floor_name: (data && data.floor_name) ? data.floor_name : '',
                lat: data ? data.lat : undefined,
                lng: data ? data.lng : undefined
            };
            clearMinimapAuxTimers();
            lastMinimapLayerSig = buildLayerSyncSig(lanLayerData);
            lastMinimapLayerSyncTs = Date.now();
            handleLayeredExploration(ctx, lanLayerData, null);
            if (!group) {
                selectElevatorFloorExact({ floor_id: fidRaw, floor_name: (data && data.floor_name) || '' }, null);
            }
        } catch (e) {
            console.error('[Genshin Helper] handleLanSetFloor error:', e);
        }
    }

    function handleLanFocusPin(data) {
        try {
            if (!data || typeof data !== 'object') return;
            const ctx = findMapContext();
            if (!ctx || !ctx.map) return;
            const hasFloor = data.floor_id !== undefined && data.floor_id !== null && String(data.floor_id).trim() !== '' && String(data.floor_id) !== '0';
            if (hasFloor) {
                handleLanSetFloor({ floor_id: data.floor_id, lat: data.lat, lng: data.lng });
            }
            const hasCoords = typeof data.lat === 'number' && Number.isFinite(data.lat) && typeof data.lng === 'number' && Number.isFinite(data.lng);
            if (hasCoords) {
                setAutoPanPinned(false);
                ctx.map.setView([data.lat, data.lng], Math.max(ctx.map.getZoom(), 0), { animate: false });
            }
            if (data.pin_id !== undefined && data.pin_id !== null && hasCoords) {
                setTimeout(() => {
                    const curCtx = findMapContext() || ctx;
                    const found = _findMarkerForPin(curCtx, data.pin_id, data.lat, data.lng);
                    if (found) {
                        _openPinPopup(curCtx, found);
                    }
                }, hasFloor ? 220 : 40);
            }
        } catch (e) {
            console.error('[Genshin Helper] handleLanFocusPin error:', e);
        }
    }

    function pushLanPinsSnapshot(ctx, mapId = null, force = false, userInitiated = false) {
        try {
            const curCtx = ctx || findMapContext();
            if (!curCtx || !curCtx.map) return false;
            const now = Date.now();
            if (!force && (now - lastLanPinsTs) < 2500) return false;

            const resolvedMapId = (mapId !== null && mapId !== undefined && !isNaN(Number(mapId)))
                ? parseInt(mapId, 10)
                : getCurrentWebMapId();
            _ensureLabelNames(resolvedMapId);

            const fpVm = resolveFilterPanelVm(curCtx);
            const hasAuthoritativeFlatCfg = Boolean(fpVm && _getFilterPanelFlatConfig(fpVm));
            let markers = _collectIndividualMarkers(curCtx);
            if (!hasAuthoritativeFlatCfg && markers.length === 0) {
                markers = _collectAllCandidateMarkers(curCtx);
            }
            if (!hasAuthoritativeFlatCfg && (!markers || markers.length === 0)) return false;

            const markedSet = _collectMarkedPinIds(curCtx);
            const lookup = _getMarkerMetaLookup(curCtx);
            const seenIds = new Set();
            const entries = [];

            for (let i = 0; i < markers.length; i++) {
                const m = markers[i];
                const ll = (m && typeof m.getLatLng === 'function') ? m.getLatLng() : null;
                if (!ll || !Number.isFinite(ll.lat) || !Number.isFinite(ll.lng)) continue;
                const meta = _resolveRichPinItem(null, m, lookup);
                if (!meta || meta.id === null || meta.id === undefined) continue;
                if (_isNavLandmarkPin(meta.raw, m)) continue;
                const pid = String(meta.id);
                if (!pid || seenIds.has(pid)) continue;
                seenIds.add(pid);

                const lid = Number(meta.labelId || 0);
                const lname = String(meta.labelName || _labelNameFor(lid) || meta.name || lid || '').trim();
                const fid = (meta.floorId !== null && meta.floorId !== undefined && String(meta.floorId).trim() !== '' && String(meta.floorId) !== '0')
                    ? String(meta.floorId)
                    : null;
                const marked = _isPinMarked(meta.raw, m, markedSet) ? 1 : 0;
                entries.push({
                    id: pid,
                    lat: Number(ll.lat.toFixed(2)),
                    lng: Number(ll.lng.toFixed(2)),
                    labelId: Number.isFinite(lid) ? lid : 0,
                    labelName: lname,
                    floorId: fid,
                    marked,
                    name: String(meta.name || lname || ''),
                    isUg: meta.isUnderground ? 1 : 0,
                    iconSign: Number(meta.iconSign || 0) || 0,
                    zLevel: Number(meta.zLevel || 0) || 0
                });
            }

            if (entries.length > 2000) {
                const hasPlayer = playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number';
                const center = hasPlayer
                    ? playerLastState
                    : ((typeof curCtx.map.getCenter === 'function' && curCtx.map.getCenter()) || { lat: 0, lng: 0 });
                entries.sort((a, b) =>
                    Math.hypot(a.lat - center.lat, a.lng - center.lng) -
                    Math.hypot(b.lat - center.lat, b.lng - center.lng)
                );
                entries.length = 2000;
            }

            let markedCount = 0;
            const names = [];
            const nameIdxMap = new Map();
            const catMap = new Map();
            const pins = new Array(entries.length);

            for (let i = 0; i < entries.length; i++) {
                const e = entries[i];
                if (e.marked) markedCount++;
                let nIdx = nameIdxMap.get(e.name);
                if (nIdx === undefined) {
                    nIdx = names.length;
                    names.push(e.name);
                    nameIdxMap.set(e.name, nIdx);
                }
                const c = catMap.get(e.labelId);
                if (c) {
                    c.count++;
                } else {
                    catMap.set(e.labelId, { id: e.labelId, name: e.labelName || String(e.labelId), count: 1 });
                }
                pins[i] = [e.id, e.lat, e.lng, e.labelId, e.floorId, e.marked, nIdx, e.isUg, e.iconSign, e.zLevel];
            }

            const firstId = entries.length > 0 ? entries[0].id : '';
            const lastId = entries.length > 0 ? entries[entries.length - 1].id : '';
            const fp = `${resolvedMapId}:${entries.length}:${markedCount}:${firstId}:${lastId}`;
            if (!force && fp === lastLanPinsFp) {
                lastLanPinsTs = now;
                return false;
            }

            lastLanPinsFp = fp;
            lastLanPinsTs = now;
            const categories = Array.from(catMap.values()).sort((a, b) => b.count - a.count || a.id - b.id);
            extRequest('/lan_pins', {
                method: 'POST',
                body: {
                    tab_id: GH_TAB_ID,
                    tab_visible: !document.hidden,
                    tab_focused: typeof document.hasFocus === 'function' ? Boolean(document.hasFocus()) : false,
                    user_initiated: Boolean(userInitiated || force),
                    map_id: resolvedMapId,
                    names,
                    categories,
                    pins
                }
            }).catch(() => {});
            return true;
        } catch (e) {
            return false;
        }
    }

    // =========================================================================
    // 7.1 TSP Smart Route SVG Overlay (<textPath> ► arrows) & Planner (F7)
    // =========================================================================
    function clearTspRouteFromMap(opts = {}) {
        try {
            if (tspRedrawRaf !== null) {
                cancelAnimationFrame(tspRedrawRaf);
                tspRedrawRaf = null;
            }
            if (tspMapRef && typeof tspMapRef.off === 'function') {
                tspMapRef.off('move zoom viewreset zoomend', _scheduleTspRouteRedraw);
            }
            if (tspSvgEl) {
                tspSvgEl.remove();
                tspSvgEl = null;
            }
            const stray = document.getElementById('gh-tsp-route-svg');
            if (stray) stray.remove();
            const hadRoute = Boolean(activeTspRoute);
            activeTspRoute = null;
            tspMapRef = null;
            if (opts && opts.notifyToast && hadRoute) {
                setLog(tr('page.tsp.cleared'), '#94a3b8');
            }
            return true;
        } catch (e) {
            return false;
        }
    }

    function _scheduleTspRouteRedraw() {
        if (!activeTspRoute) return;
        if (tspRedrawRaf !== null) return;
        tspRedrawRaf = requestAnimationFrame(() => {
            tspRedrawRaf = null;
            _drawTspRouteSvg();
        });
    }

    function _extractLegCoords(leg) {
        if (!leg || typeof leg !== 'object') return null;
        let fLat = leg.from_lat;
        let fLng = leg.from_lng;
        let tLat = leg.to_lat;
        let tLng = leg.to_lng;
        if ((fLat === undefined || fLng === undefined) && Array.isArray(leg.points) && leg.points.length >= 2) {
            fLat = leg.points[0][0];
            fLng = leg.points[0][1];
            tLat = leg.points[1][0];
            tLng = leg.points[1][1];
        }
        if (
            typeof fLat !== 'number' || !Number.isFinite(fLat) ||
            typeof fLng !== 'number' || !Number.isFinite(fLng) ||
            typeof tLat !== 'number' || !Number.isFinite(tLat) ||
            typeof tLng !== 'number' || !Number.isFinite(tLng)
        ) {
            return null;
        }
        return { fLat, fLng, tLat, tLng };
    }

    function _drawTspRouteSvg() {
        try {
            if (!activeTspRoute) return false;
            const ctx = findMapContext();
            if (!ctx || !ctx.map || typeof ctx.map.latLngToLayerPoint !== 'function') return false;
            const routeMapId = activeTspRoute.map_id !== undefined && activeTspRoute.map_id !== null
                ? parseInt(activeTspRoute.map_id, 10)
                : null;
            if (routeMapId !== null && !isNaN(routeMapId) && routeMapId !== getCurrentWebMapId()) {
                if (tspSvgEl) tspSvgEl.style.display = 'none';
                return false;
            }

            injectThemeStyles();
            const map = ctx.map;
            const panes = (typeof map.getPanes === 'function') ? map.getPanes() : null;
            const overlayPane = (panes && panes.overlayPane)
                ? panes.overlayPane
                : (typeof map.getContainer === 'function' ? map.getContainer() : null);
            if (!overlayPane) return false;

            const legs = Array.isArray(activeTspRoute.legs) ? activeTspRoute.legs : [];
            const stops = Array.isArray(activeTspRoute.ordered_pins) ? activeTspRoute.ordered_pins : [];
            if (legs.length === 0 && stops.length === 0) {
                clearTspRouteFromMap();
                return false;
            }

            const SVG_NS = 'http://www.w3.org/2000/svg';
            const XLINK_NS = 'http://www.w3.org/1999/xlink';

            if (!tspSvgEl || !tspSvgEl.isConnected || tspSvgEl.parentNode !== overlayPane) {
                if (tspSvgEl) tspSvgEl.remove();
                const old = document.getElementById('gh-tsp-route-svg');
                if (old) old.remove();
                tspSvgEl = document.createElementNS(SVG_NS, 'svg');
                tspSvgEl.setAttribute('id', 'gh-tsp-route-svg');
                tspSvgEl.setAttribute('class', 'gh-tsp-route-svg');
                tspSvgEl.setAttribute('aria-hidden', 'true');
                overlayPane.appendChild(tspSvgEl);
            }
            tspSvgEl.style.display = '';

            if (tspMapRef !== map) {
                if (tspMapRef && typeof tspMapRef.off === 'function') {
                    tspMapRef.off('move zoom viewreset zoomend', _scheduleTspRouteRedraw);
                }
                tspMapRef = map;
                if (typeof map.on === 'function') {
                    map.on('move zoom viewreset zoomend', _scheduleTspRouteRedraw);
                }
            }

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
            for (let i = 0; i < legs.length; i++) {
                const coords = _extractLegCoords(legs[i]);
                if (!coords) continue;
                const p1 = map.latLngToLayerPoint([coords.fLat, coords.fLng]);
                const p2 = map.latLngToLayerPoint([coords.tLat, coords.tLng]);
                trackPt(p1);
                trackPt(p2);
                legGeom.push({ idx: i, leg: legs[i], p1, p2 });
            }

            const stopGeom = [];
            for (let i = 0; i < stops.length; i++) {
                const s = stops[i];
                if (!s || typeof s.lat !== 'number' || typeof s.lng !== 'number') continue;
                const pt = map.latLngToLayerPoint([s.lat, s.lng]);
                trackPt(pt);
                stopGeom.push({ idx: i, stop: s, pt });
            }

            if (!Number.isFinite(minX) || !Number.isFinite(minY)) {
                return false;
            }

            const pad = 52;
            const vbX = Math.floor(minX - pad);
            const vbY = Math.floor(minY - pad);
            const vbW = Math.max(2, Math.ceil((maxX - minX) + pad * 2));
            const vbH = Math.max(2, Math.ceil((maxY - minY) + pad * 2));

            tspSvgEl.setAttribute('width', String(vbW));
            tspSvgEl.setAttribute('height', String(vbH));
            tspSvgEl.setAttribute('viewBox', `${vbX} ${vbY} ${vbW} ${vbH}`);
            tspSvgEl.style.transform = `translate3d(${vbX}px, ${vbY}px, 0)`;

            while (tspSvgEl.firstChild) {
                tspSvgEl.removeChild(tspSvgEl.firstChild);
            }

            const defs = document.createElementNS(SVG_NS, 'defs');
            const legsGroup = document.createElementNS(SVG_NS, 'g');
            const arrowsGroup = document.createElementNS(SVG_NS, 'g');
            const stopsGroup = document.createElementNS(SVG_NS, 'g');

            for (const entry of legGeom) {
                const { idx, leg, p1, p2 } = entry;
                const dx = p2.x - p1.x;
                const dy = p2.y - p1.y;
                const segLen = Math.hypot(dx, dy);
                if (segLen < 1.5) continue;

                const isTeleport = Boolean(leg.is_teleport);
                let pathD;
                if (isTeleport) {
                    const midX = (p1.x + p2.x) * 0.5;
                    const midY = (p1.y + p2.y) * 0.5;
                    const bow = Math.min(36, segLen * 0.14);
                    const nx = -dy / segLen;
                    const ny = dx / segLen;
                    const cx = midX + nx * bow;
                    const cy = midY + ny * bow;
                    pathD = `M ${p1.x.toFixed(1)} ${p1.y.toFixed(1)} Q ${cx.toFixed(1)} ${cy.toFixed(1)} ${p2.x.toFixed(1)} ${p2.y.toFixed(1)}`;
                } else {
                    pathD = `M ${p1.x.toFixed(1)} ${p1.y.toFixed(1)} L ${p2.x.toFixed(1)} ${p2.y.toFixed(1)}`;
                }

                const halo = document.createElementNS(SVG_NS, 'path');
                halo.setAttribute('d', pathD);
                halo.setAttribute('fill', 'none');
                halo.setAttribute('stroke', 'rgba(15, 23, 42, 0.78)');
                halo.setAttribute('stroke-width', isTeleport ? '5.2' : '6.5');
                halo.setAttribute('stroke-linecap', 'round');
                if (isTeleport) halo.setAttribute('stroke-dasharray', '8 6');
                legsGroup.appendChild(halo);

                const line = document.createElementNS(SVG_NS, 'path');
                line.setAttribute('d', pathD);
                line.setAttribute('fill', 'none');
                line.setAttribute('stroke', isTeleport ? '#f59e0b' : '#38bdf8');
                line.setAttribute('stroke-width', isTeleport ? '2.6' : '3.2');
                line.setAttribute('stroke-linecap', 'round');
                if (isTeleport) {
                    line.setAttribute('stroke-dasharray', '8 6');
                }
                legsGroup.appendChild(line);

                if (isTeleport) {
                    const wpRing = document.createElementNS(SVG_NS, 'circle');
                    wpRing.setAttribute('cx', p2.x.toFixed(1));
                    wpRing.setAttribute('cy', p2.y.toFixed(1));
                    wpRing.setAttribute('r', '7');
                    wpRing.setAttribute('fill', 'rgba(245, 158, 11, 0.25)');
                    wpRing.setAttribute('stroke', '#f59e0b');
                    wpRing.setAttribute('stroke-width', '2');
                    legsGroup.appendChild(wpRing);
                } else if (segLen >= 26) {
                    const pathId = `gh-tsp-leg-path-${idx}`;
                    const defPath = document.createElementNS(SVG_NS, 'path');
                    defPath.setAttribute('id', pathId);
                    defPath.setAttribute('d', pathD);
                    defs.appendChild(defPath);

                    const repeatCount = Math.max(1, Math.min(12, Math.floor(segLen / 40)));
                    const chevronStr = '  ►  '.repeat(repeatCount);

                    const textEl = document.createElementNS(SVG_NS, 'text');
                    textEl.setAttribute('class', 'gh-tsp-chevron-text');
                    textEl.setAttribute('dy', '3.6');

                    const textPathEl = document.createElementNS(SVG_NS, 'textPath');
                    textPathEl.setAttribute('href', `#${pathId}`);
                    textPathEl.setAttributeNS(XLINK_NS, 'xlink:href', `#${pathId}`);
                    textPathEl.setAttribute('startOffset', '50%');
                    textPathEl.setAttribute('text-anchor', 'middle');
                    textPathEl.textContent = chevronStr;

                    textEl.appendChild(textPathEl);
                    arrowsGroup.appendChild(textEl);
                }
            }

            for (const entry of stopGeom) {
                const { idx, stop, pt } = entry;
                const stepNum = stop.step || (idx + 1);
                const isFirst = stepNum === 1;

                const badge = document.createElementNS(SVG_NS, 'g');
                badge.setAttribute('class', 'gh-tsp-stop');

                const circle = document.createElementNS(SVG_NS, 'circle');
                circle.setAttribute('cx', pt.x.toFixed(1));
                circle.setAttribute('cy', pt.y.toFixed(1));
                circle.setAttribute('r', isFirst ? '11.5' : '10');
                circle.setAttribute('fill', isFirst ? '#064e3b' : '#0f172a');
                circle.setAttribute('stroke', isFirst ? '#22c55e' : '#38bdf8');
                circle.setAttribute('stroke-width', '2');
                badge.appendChild(circle);

                const label = document.createElementNS(SVG_NS, 'text');
                label.setAttribute('class', 'gh-tsp-stop-label');
                label.setAttribute('x', pt.x.toFixed(1));
                label.setAttribute('y', pt.y.toFixed(1));
                label.setAttribute('text-anchor', 'middle');
                label.setAttribute('dominant-baseline', 'central');
                label.textContent = String(stepNum);
                badge.appendChild(label);

                stopsGroup.appendChild(badge);
            }

            tspSvgEl.appendChild(defs);
            tspSvgEl.appendChild(legsGroup);
            tspSvgEl.appendChild(arrowsGroup);
            tspSvgEl.appendChild(stopsGroup);
            return true;
        } catch (e) {
            console.error('[Genshin Helper] Error rendering TSP route SVG:', e);
            return false;
        }
    }

    function renderTspRouteOnMap(routeData) {
        if (!routeData || typeof routeData !== 'object') {
            clearTspRouteFromMap();
            return false;
        }
        const legs = Array.isArray(routeData.legs) ? routeData.legs : [];
        const stops = Array.isArray(routeData.ordered_pins) ? routeData.ordered_pins : [];
        if (legs.length === 0 && stops.length === 0) {
            clearTspRouteFromMap();
            return false;
        }
        activeTspRoute = routeData;
        return _drawTspRouteSvg();
    }

    function _collectVisibleUncollectedPins(ctx, startLat, startLng, maxPins = 60) {
        const markedSet = _collectMarkedPinIds(ctx);
        const lookup = _getMarkerMetaLookup(ctx);
        const candidates = [];
        const seenKeys = new Set();

        let minLat = -Infinity;
        let maxLat = Infinity;
        let minLng = -Infinity;
        let maxLng = Infinity;
        if (ctx.map && typeof ctx.map.getBounds === 'function') {
            try {
                const b = ctx.map.getBounds();
                if (b && typeof b.getSouthWest === 'function' && typeof b.getNorthEast === 'function') {
                    const sw = b.getSouthWest();
                    const ne = b.getNorthEast();
                    minLat = Math.min(sw.lat, ne.lat);
                    maxLat = Math.max(sw.lat, ne.lat);
                    minLng = Math.min(sw.lng, ne.lng);
                    maxLng = Math.max(sw.lng, ne.lng);
                }
            } catch (e) {}
        }

        const pushCandidate = (item, markerObj, pinLat, pinLng) => {
            if (typeof pinLat !== 'number' || !Number.isFinite(pinLat) || typeof pinLng !== 'number' || !Number.isFinite(pinLng)) return;
            if (pinLat < minLat || pinLat > maxLat || pinLng < minLng || pinLng > maxLng) return;

            const meta = _resolveRichPinItem(item, markerObj, lookup);
            if (_isNavLandmarkPin(meta.raw, markerObj) || _isNavLandmarkPin(item, markerObj)) return;
            if (_isPinMarked(meta.raw, markerObj, markedSet) || _isPinMarked(item, markerObj, markedSet)) return;

            const dedupKey = meta.id !== null ? `id:${meta.id}` : `${pinLat.toFixed(2)},${pinLng.toFixed(2)}`;
            if (seenKeys.has(dedupKey)) return;
            seenKeys.add(dedupKey);

            candidates.push({
                id: meta.id !== null ? meta.id : `pin_${candidates.length}`,
                name: meta.name,
                lat: pinLat,
                lng: pinLng,
                label_id: meta.labelId,
                floor_id: meta.floorId,
                is_underground: meta.isUnderground,
                image_url: meta.imageUrl,
                description: meta.description,
                _dist: Math.hypot(pinLat - startLat, pinLng - startLng)
            });
        };

        const tree = ctx.ciLayer && ctx.ciLayer._latlngMarkers;
        if (tree && typeof tree.search === 'function' && Number.isFinite(minLat) && Number.isFinite(minLng)) {
            const hits = tree.search({ minX: minLng, minY: minLat, maxX: maxLng, maxY: maxLat });
            for (const entry of hits) {
                const m = entry.data || entry;
                const ll = (m && typeof m.getLatLng === 'function')
                    ? m.getLatLng()
                    : { lat: (entry.minY + entry.maxY) / 2, lng: (entry.minX + entry.maxX) / 2 };
                pushCandidate(entry, m, ll.lat, ll.lng);
            }
        }

        if (candidates.length === 0 && Array.isArray(ctx.markerList) && ctx.markerList.length > 0) {
            for (let i = 0; i < ctx.markerList.length; i++) {
                const item = ctx.markerList[i];
                const m = item.marker;
                const y = (item.y_pos !== undefined) ? item.y_pos : (m && typeof m.getLatLng === 'function' ? m.getLatLng().lat : null);
                const x = (item.x_pos !== undefined) ? item.x_pos : (m && typeof m.getLatLng === 'function' ? m.getLatLng().lng : null);
                if (y === null || x === null) continue;
                pushCandidate(item, m, y, x);
            }
        }

        candidates.sort((a, b) => a._dist - b._dist);
        const trimmed = candidates.slice(0, maxPins).map(({ _dist, ...rest }) => rest);

        if (trimmed.length < 2) {
            const fallback = queryNearbyUncollectedPins(startLat, startLng, 1500, maxPins);
            if (fallback.length > trimmed.length) {
                return fallback.map((p, idx) => ({
                    id: p.id !== null && p.id !== undefined ? p.id : `pin_${idx}`,
                    name: p.name,
                    lat: p.lat,
                    lng: p.lng,
                    label_id: p.label_id ?? p.labelId ?? null,
                    floor_id: p.floor_id ?? p.floorId ?? null,
                    is_underground: Boolean(p.is_underground),
                    image_url: p.image_url || '',
                    description: p.description || ''
                }));
            }
        }
        return trimmed;
    }

    async function requestPlanTspRoute(opts = {}) {
        try {
            const ctx = findMapContext();
            if (!ctx || !ctx.map) {
                setLog(tr('page.sync.map_not_found'), '#ef4444');
                return { ok: false, error: 'Map not ready' };
            }
            const currentMapId = getCurrentWebMapId();
            const center = (typeof ctx.map.getCenter === 'function') ? ctx.map.getCenter() : { lat: 0, lng: 0 };
            const usePlayer = Boolean(
                playerLastState &&
                typeof playerLastState.lat === 'number' &&
                typeof playerLastState.lng === 'number' &&
                Number(playerLastState.map_id || currentMapId) === Number(currentMapId)
            );
            const startLat = usePlayer ? playerLastState.lat : center.lat;
            const startLng = usePlayer ? playerLastState.lng : center.lng;

            const maxPins = (opts && typeof opts.maxPins === 'number' && opts.maxPins > 0) ? Math.min(60, Math.floor(opts.maxPins)) : 60;
            const pins = _collectVisibleUncollectedPins(ctx, startLat, startLng, maxPins);
            if (pins.length === 0) {
                setLog(tr('page.tsp.no_pins'), '#f59e0b');
                return { ok: false, empty: true, error: 'No visible uncollected pins' };
            }

            setLog(tr('page.tsp.planning'), '#38bdf8');
            const res = await extRequest('/plan_tsp_route', {
                method: 'POST',
                body: {
                    map_id: currentMapId,
                    start_pos: [startLat, startLng],
                    pins
                }
            });

            const routeData = (res && res.route && typeof res.route === 'object') ? res.route : res;
            if (!routeData || typeof routeData !== 'object' || (!Array.isArray(routeData.legs) && !Array.isArray(routeData.ordered_pins))) {
                const errMsg = (res && res.error) ? String(res.error) : 'Invalid route response';
                setLog(tr('page.tsp.error', { error: errMsg }), '#ef4444');
                return { ok: false, error: errMsg };
            }
            if (routeData.map_id === undefined) {
                routeData.map_id = currentMapId;
            }

            renderTspRouteOnMap(routeData);
            const stopCount = typeof routeData.pin_count === 'number'
                ? routeData.pin_count
                : (Array.isArray(routeData.ordered_pins) ? routeData.ordered_pins.length : pins.length);
            const tpCount = typeof routeData.teleport_count === 'number'
                ? routeData.teleport_count
                : (Array.isArray(routeData.teleport_hints) ? routeData.teleport_hints.length : 0);
            setLog(tr('page.tsp.planned', { count: stopCount, teleports: tpCount }), '#22c55e');
            return { ok: true, route: routeData };
        } catch (err) {
            const errMsg = (err && err.message) ? err.message : 'Request failed';
            setLog(tr('page.tsp.error', { error: errMsg }), '#ef4444');
            return { ok: false, error: errMsg };
        }
    }

    // =========================================================================
    // 7.2 1-Click HoYoLAB Cross-Map Pin Exporter (F8)
    // =========================================================================
    function _triggerClientFileDownload(filename, contentStr, mimeType) {
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

    async function _fetchHoyolabOriginJson(url) {
        const res = await fetch(url, {
            method: 'GET',
            credentials: 'include',
            headers: {
                'x-rpc-map_version': '4.5'
            }
        });
        if (!res.ok) {
            throw new Error(`HTTP ${res.status}`);
        }
        return res.json();
    }

    async function exportMarkedPins(targetFormat = 'canonical') {
        if (exportInProgress) {
            return { ok: false, busy: true };
        }
        const allowedFormats = ['kongying', 'appsample', 'genshinmap', 'canonical'];
        const normalizedFmt = String(targetFormat || 'canonical').trim().toLowerCase();
        const fmt = allowedFormats.includes(normalizedFmt) ? normalizedFmt : 'canonical';
        const mapId = getCurrentWebMapId();
        const apiLang = (window.GHI18n && window.GHI18n.getLang() === 'vi') ? 'vi-vn' : 'en-us';
        const apiBase = 'https://sg-public-api.hoyolab.com/common/map_user/ys_obc';

        exportInProgress = true;
        setLog(tr('page.export.fetching', { format: fmt }), '#38bdf8');
        try {
            // Fetch FULL point/list WITHOUT narrow label_ids filter so hidden categories are never dropped
            const pointListUrl = `${apiBase}/v2/map/point/list?map_id=${encodeURIComponent(mapId)}&app_sn=ys_obc&lang=${encodeURIComponent(apiLang)}`;
            const markedListUrl = `${apiBase}/v1/map/point/mark_map_point_list?map_id=${encodeURIComponent(mapId)}&app_sn=ys_obc&lang=${encodeURIComponent(apiLang)}`;

            const [pointListEnvelope, markedListEnvelope] = await Promise.all([
                _fetchHoyolabOriginJson(pointListUrl),
                _fetchHoyolabOriginJson(markedListUrl)
            ]);

            if (markedListEnvelope && typeof markedListEnvelope.retcode === 'number' && markedListEnvelope.retcode !== 0) {
                if (markedListEnvelope.retcode === -100 || markedListEnvelope.retcode === 10001) {
                    setLog(tr('page.export.login_required'), '#f59e0b');
                    return { ok: false, error: 'HoYoLAB login required' };
                }
                throw new Error(markedListEnvelope.message || `HoYoLAB retcode ${markedListEnvelope.retcode}`);
            }

            const res = await extRequest('/cross_map_export', {
                method: 'POST',
                timeoutMs: 30000,
                body: {
                    point_list: pointListEnvelope,
                    marked_list: markedListEnvelope,
                    map_id: mapId,
                    target_format: fmt
                }
            });

            if (!res || res.ok === false) {
                throw new Error((res && res.error) ? String(res.error) : 'Export failed');
            }

            const rawPayload = (res.content !== undefined)
                ? res.content
                : (res.exported !== undefined
                    ? res.exported
                    : (res.payload !== undefined
                        ? res.payload
                        : (res.output !== undefined ? res.output : res.data)));

            const isTextFormat = (
                fmt === 'genshinmap' ||
                (typeof rawPayload === 'string' && rawPayload.startsWith('GM_007~')) ||
                (typeof res.filename === 'string' && res.filename.toLowerCase().endsWith('.txt'))
            );
            const ext = isTextFormat ? 'txt' : 'json';
            const mimeType = isTextFormat ? 'text/plain;charset=utf-8' : 'application/json;charset=utf-8';
            const filename = (typeof res.filename === 'string' && res.filename.trim())
                ? res.filename.trim()
                : `genshin-helper-export-${fmt}-map${mapId}.${ext}`;

            const fileContent = (typeof rawPayload === 'string')
                ? rawPayload
                : JSON.stringify(rawPayload !== undefined ? rawPayload : res, null, 2);

            _triggerClientFileDownload(filename, fileContent, mimeType);

            const stats = (res.stats && typeof res.stats === 'object')
                ? res.stats
                : ((res.summary && typeof res.summary === 'object')
                    ? res.summary
                    : ((res.report && typeof res.report === 'object') ? res.report : res));

            const matched = Number(
                stats.matched_count ?? stats.matched ?? stats.total_pins ?? stats.exported_count ??
                (rawPayload && Array.isArray(rawPayload.pins) ? rawPayload.pins.length : 0)
            ) || 0;
            const unmatched = Number(stats.unmatched_count ?? stats.unmatched ?? 0) || 0;
            const missing = Number(
                stats.missing_from_point_list_count ??
                (Array.isArray(stats.missing_from_point_list) ? stats.missing_from_point_list.length : null) ??
                (Array.isArray(res.missing_from_point_list) ? res.missing_from_point_list.length : 0)
            ) || 0;

            setLog(tr('page.export.done', { format: fmt, matched, unmatched, missing }), '#22c55e');
            return { ok: true, filename, matched, unmatched, missing };
        } catch (err) {
            const errMsg = (err && err.message) ? err.message : 'Export failed';
            setLog(tr('page.export.error', { error: errMsg }), '#ef4444');
            return { ok: false, error: errMsg };
        } finally {
            exportInProgress = false;
        }
    }

    // =========================================================================
    // 7.3 Web-Native Route Companion (HoYoLAB Capture, Guard & .ghroute.json)
    // =========================================================================
    function maybeSmartReattach(pos) {
        try {
            if (!routeInspecting || !activeWebRoute || !activeWebRoute.active || !pos) return false;
            if (typeof pos.lat !== 'number' || typeof pos.lng !== 'number') return false;
            const rMid = Number(activeWebRoute.map_id || 2);
            const pMid = Number(pos.map_id || 2);
            if (rMid !== pMid) return false;

            const anchor = activeWebRoute.anchor_teleport;
            if (anchor && typeof anchor.lat === 'number' && typeof anchor.lng === 'number') {
                if (Math.hypot(pos.lat - anchor.lat, pos.lng - anchor.lng) < 250) {
                    showMiniToast(tr('page.route.reattached'), '#38bdf8');
                    return true;
                }
            }
            const stops = Array.isArray(activeWebRoute.active_stops) ? activeWebRoute.active_stops : [];
            const nextStop = stops.find((s) => s && !s.collected) || stops[0];
            if (nextStop && typeof nextStop.lat === 'number' && typeof nextStop.lng === 'number') {
                if (Math.hypot(pos.lat - nextStop.lat, pos.lng - nextStop.lng) < 250) {
                    showMiniToast(tr('page.route.reattached'), '#38bdf8');
                    return true;
                }
            }
        } catch (e) {}
        return false;
    }

    function sendRouteSectionSelect(nextIdx) {
        if (!activeWebRoute || !activeWebRoute.active) return;
        const count = Math.max(1, Number(activeWebRoute.section_count || 1));
        const targetIdx = ((nextIdx % count) + count) % count;
        extRequest('/route', {
            method: 'POST',
            body: { action: 'section', section_index: targetIdx }
        })
            .then((res) => {
                if (res && res.active) handleRouteUpdatedEvent(res, { fit: false });
            })
            .catch(() => {});
    }

    async function handleRouteExportCommand() {
        try {
            const res = await extRequest('/route', {
                method: 'POST',
                body: { action: 'export' }
            });
            if (res && res.doc) {
                const fname = String(res.filename || 'route.ghroute.json');
                _triggerClientFileDownload(fname, JSON.stringify(res.doc, null, 2), 'application/json');
                return { ok: true, filename: fname };
            }
            return { ok: false, error: (res && res.error) || 'No active route' };
        } catch (err) {
            return { ok: false, error: (err && err.message) ? err.message : 'Export failed' };
        }
    }

    function renderWebRouteOverlay(routeStatus) {
        try {
            if (!routeStatus || !routeStatus.active) {
                clearTspRouteFromMap();
                return;
            }
            const nativeDrawerOpen = Boolean(document.querySelector('.route-detail'));
            if (nativeDrawerOpen && routeStatus.provider === 'hoyolab') {
                clearTspRouteFromMap();
                return;
            }
            const stops = Array.isArray(routeStatus.active_stops) ? routeStatus.active_stops : [];
            const edges = Array.isArray(routeStatus.active_edges) ? routeStatus.active_edges : [];
            const legs = [];
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
            const orderedPins = stops
                .filter((s) => s && !s.collected && typeof s.lat === 'number' && typeof s.lng === 'number')
                .map((s, idx) => ({ step: idx + 1, lat: s.lat, lng: s.lng }));
            if (legs.length === 0 && orderedPins.length === 0) {
                clearTspRouteFromMap();
                return;
            }
            renderTspRouteOnMap({
                map_id: routeStatus.map_id || 2,
                legs,
                ordered_pins: orderedPins
            });
        } catch (e) {}
    }

    function renderRouteBar(routeStatus) {
        if (!routeStatus || !routeStatus.active) {
            if (ghRouteBarEl) {
                try { ghRouteBarEl.remove(); } catch (e) {}
                ghRouteBarEl = null;
            }
            try { document.body && document.body.classList.remove('gh-has-route-bar'); } catch (e) {}
            return;
        }
        injectThemeStyles();
        try { document.body && document.body.classList.add('gh-has-route-bar'); } catch (e) {}
        if (!ghRouteBarEl || !ghRouteBarEl.isConnected) {
            const existing = document.getElementById('gh-route-bar');
            if (existing) existing.remove();
            ghRouteBarEl = document.createElement('div');
            ghRouteBarEl.id = 'gh-route-bar';
            ghRouteBarEl.setAttribute('role', 'toolbar');
            (document.body || document.documentElement).appendChild(ghRouteBarEl);
        }
        while (ghRouteBarEl.firstChild) {
            ghRouteBarEl.removeChild(ghRouteBarEl.firstChild);
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

        const iconBtn = (icon, title) => {
            const b = document.createElement('button');
            b.type = 'button';
            b.className = 'gh-route-btn is-icon';
            b.title = title;
            b.setAttribute('aria-label', title);
            b.appendChild(ghIcon(icon));
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

        const prevBtn = iconBtn('chevron-left', tr('page.route.prev_sec_title'));
        prevBtn.disabled = secCount <= 1;
        prevBtn.addEventListener('click', () => sendRouteSectionSelect(secIdx - 1));

        // "S1/7" (section) + "0/7" (collected in this section)
        const secLabel = document.createElement('span');
        secLabel.className = 'gh-route-sec';
        const secMain = document.createElement('b');
        secMain.textContent = `S${secIdx + 1}/${secCount}`;
        const secProgress = document.createElement('span');
        secProgress.textContent = `· ${secDone}/${secTotal}`;
        secLabel.append(secMain, secProgress);

        const nextBtn = iconBtn('chevron-right', tr('page.route.next_sec_title'));
        nextBtn.disabled = secCount <= 1;
        nextBtn.addEventListener('click', () => sendRouteSectionSelect(secIdx + 1));

        const viewRouteBtn = document.createElement('button');
        viewRouteBtn.type = 'button';
        viewRouteBtn.className = 'gh-route-btn is-primary';
        viewRouteBtn.textContent = tr('page.route.focus_btn');
        viewRouteBtn.addEventListener('click', () => {
            focusActiveRouteOnMap(activeWebRoute);
        });

        const followBtn = document.createElement('button');
        followBtn.type = 'button';
        followBtn.className = 'gh-route-btn' + ((routeInspecting || !autoPanEnabled) ? ' is-follow' : '');
        followBtn.textContent = tr('page.route.reattach_btn');
        followBtn.addEventListener('click', () => {
            routeInspecting = false;
            recenterToPlayer();
            renderRouteBar(activeWebRoute);
        });

        const expBtn = document.createElement('button');
        expBtn.type = 'button';
        expBtn.className = 'gh-route-btn';
        expBtn.textContent = tr('page.route.export_btn');
        expBtn.addEventListener('click', () => { handleRouteExportCommand(); });

        const resetBtn = document.createElement('button');
        resetBtn.type = 'button';
        resetBtn.className = 'gh-route-btn';
        resetBtn.textContent = tr('page.route.reset_btn');
        resetBtn.title = tr('page.route.reset_title');
        resetBtn.addEventListener('click', () => {
            extRequest('/route', { method: 'POST', body: { action: 'reset' } })
                .then((res) => {
                    if (res && res.active) {
                        handleRouteUpdatedEvent(res);
                        setLog(tr('page.route.reset_done'), '#38bdf8');
                    }
                })
                .catch(() => {});
        });

        const stopBtn = document.createElement('button');
        stopBtn.type = 'button';
        stopBtn.className = 'gh-route-btn is-stop';
        stopBtn.textContent = tr('page.route.stop_btn');
        stopBtn.addEventListener('click', () => {
            userClearedRouteSig = lastSyncedRouteSig || '__manual_stop__';
            _clearPendingRouteLoad();
            extRequest('/route', { method: 'POST', body: { action: 'clear' } })
                .then((res) => {
                    handleRouteUpdatedEvent(res || { active: false });
                    setLog(tr('page.route.cleared'), '#94a3b8');
                })
                .catch(() => {});
        });

        ghRouteBarEl.append(
            group(dot, titleEl),
            sep(),
            group(prevBtn, secLabel, nextBtn),
            sep(),
            group(viewRouteBtn, followBtn, expBtn, resetBtn, stopBtn)
        );
    }

    function focusActiveRouteOnMap(routeStatus) {
        try {
            if (!routeStatus || !routeStatus.active) return false;
            const ctx = findMapContext();
            if (!ctx || !ctx.map || Number(routeStatus.map_id || 2) !== getCurrentWebMapId()) return false;
            const anchor = routeStatus.anchor_teleport;
            const stops = Array.isArray(routeStatus.active_stops) ? routeStatus.active_stops : [];
            const firstStop = stops.find((s) => s && !s.collected) || stops[0] || null;
            const focus = (anchor && typeof anchor.lat === 'number') ? anchor : firstStop;

            const pts = [];
            if (anchor && typeof anchor.lat === 'number' && typeof anchor.lng === 'number' && isFinite(anchor.lat) && isFinite(anchor.lng)) {
                pts.push([anchor.lat, anchor.lng]);
            }
            for (const s of stops) {
                if (s && typeof s.lat === 'number' && typeof s.lng === 'number' && isFinite(s.lat) && isFinite(s.lng)) {
                    pts.push([s.lat, s.lng]);
                }
            }

            routeInspecting = true;
            setAutoPanPinned(false, { silent: true });

            if (pts.length >= 2 && typeof ctx.map.fitBounds === 'function') {
                let minLat = Infinity, maxLat = -Infinity, minLng = Infinity, maxLng = -Infinity;
                for (const [la, ln] of pts) {
                    if (la < minLat) minLat = la;
                    if (la > maxLat) maxLat = la;
                    if (ln < minLng) minLng = ln;
                    if (ln > maxLng) maxLng = ln;
                }
                if ((maxLat - minLat) > 40 || (maxLng - minLng) > 40) {
                    ctx.map.fitBounds([[minLat, minLng], [maxLat, maxLng]], { padding: [90, 90], maxZoom: 0.1, animate: true });
                    renderRouteBar(activeWebRoute);
                    return true;
                }
            }

            if (focus && typeof focus.lat === 'number' && typeof focus.lng === 'number') {
                const targetZoom = Math.min(Math.max(ctx.map.getZoom(), -1.2), 0.0);
                ctx.map.setView([focus.lat, focus.lng], targetZoom, { animate: true });
                renderRouteBar(activeWebRoute);
                return true;
            }
        } catch (e) {}
        return false;
    }

    function handleRouteUpdatedEvent(statusData, opts = {}) {
        activeWebRoute = (statusData && statusData.active) ? statusData : null;
        if (!activeWebRoute) {
            routeInspecting = false;
            renderRouteBar(null);
            clearTspRouteFromMap();
            return;
        }
        renderRouteBar(activeWebRoute);
        renderWebRouteOverlay(activeWebRoute);
        if (opts && opts.fit) {
            focusActiveRouteOnMap(activeWebRoute);
        }
    }

    function _normalizeRouteTitleKey(raw) {
        return String(raw || '').replace(/\s+/g, ' ').trim().toLowerCase();
    }

    function _extractShareIdFromUrl(urlStr) {
        const s = typeof urlStr === 'string' ? urlStr : String(urlStr || '');
        if (!s) return '';
        const m = s.match(/[?&#](?:share_id|path_id|obj_id)=([^&#]+)/i);
        if (!m || !m[1]) return '';
        try { return decodeURIComponent(m[1]).trim(); } catch (e) { return String(m[1]).trim(); }
    }

    function _isValidHoyolabPathObj(obj) {
        if (!obj || typeof obj !== 'object' || !Array.isArray(obj.sections) || obj.sections.length === 0) {
            return false;
        }
        return obj.sections.some((s) =>
            s && typeof s === 'object' && (
                (Array.isArray(s.system_points) && s.system_points.length > 0) ||
                (Array.isArray(s.custom_points) && s.custom_points.length > 0)
            )
        );
    }

    function _extractHoyolabPathObj(jsonData) {
        if (!jsonData || typeof jsonData !== 'object') return null;
        const d = (jsonData.data && typeof jsonData.data === 'object') ? jsonData.data : jsonData;
        if (!d || typeof d !== 'object') return null;
        if (_isValidHoyolabPathObj(d.path)) return d.path;
        if (_isValidHoyolabPathObj(d)) return d;
        if (_isValidHoyolabPathObj(d.route)) return d.route;
        if (_isValidHoyolabPathObj(d.info)) return d.info;
        return null;
    }

    function _computeRouteSig(pathObj, mapId) {
        if (!pathObj || !Array.isArray(pathObj.sections)) return '';
        const pid = String(pathObj.obj_id || pathObj.path_id || pathObj.id || '').trim();
        const parts = [`m:${Number(mapId || 2)}`, `id:${pid}`, `n:${pathObj.sections.length}`];
        for (let i = 0; i < pathObj.sections.length && i < 12; i++) {
            const sec = pathObj.sections[i];
            if (!sec || typeof sec !== 'object') continue;
            const pts = [];
            for (const key of ['system_points', 'custom_points']) {
                const arr = sec[key];
                if (!Array.isArray(arr)) continue;
                for (let j = 0; j < arr.length && pts.length < 4; j++) {
                    const p = arr[j];
                    if (!p || typeof p !== 'object') continue;
                    const idPart = p.id ?? p.point_id ?? '';
                    const xPart = Math.round(Number(p.x_pos ?? p.lng ?? 0));
                    const yPart = Math.round(Number(p.y_pos ?? p.lat ?? 0));
                    pts.push(`${idPart}:${xPart},${yPart}`);
                }
            }
            parts.push(`s${i}[${pts.join(';')}]`);
        }
        return parts.join('|');
    }

    function _setBoundedCache(map, key, value, maxSize = 32) {
        if (!key) return;
        const k = String(key).trim();
        if (!k) return;
        if (map.has(k)) map.delete(k);
        map.set(k, value);
        while (map.size > maxSize) {
            const oldest = map.keys().next().value;
            if (oldest === undefined) break;
            map.delete(oldest);
        }
    }

    function _clearPendingRouteLoad() {
        pendingRouteLoad = null;
        if (routeLoadRetryTimer) {
            clearTimeout(routeLoadRetryTimer);
            routeLoadRetryTimer = null;
        }
    }

    function _clearRouteDetailTimers() {
        for (const tid of routeDetailTimers) {
            try { clearTimeout(tid); } catch (e) {}
        }
        routeDetailTimers = [];
    }

    function _readRouteDetailDomHeader(detailEl) {
        if (!detailEl) return '';
        const header = detailEl.querySelector('.route-detail__row-header') ||
            detailEl.querySelector('[class*="route-detail__title"], [class*="route-detail__name"], [class*="row-header"]');
        return header ? String(header.textContent || '').trim() : '';
    }

    function _readRouteDetailDomFp(detailEl) {
        if (!detailEl) return '';
        const hdr = _readRouteDetailDomHeader(detailEl);
        const secs = detailEl.querySelectorAll('.route-detail__section').length;
        const sid = _extractShareIdFromUrl(window.location ? window.location.href : '');
        const txtSample = String(detailEl.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 120);
        return `${sid}|${hdr}|${secs}|${txtSample}`;
    }

    function _dispatchRouteLoadAttempt(loadJob) {
        if (!loadJob || loadJob.seq !== routeLoadSeq) return;
        const retryDelays = [450, 1300, 2600];
        extRequest('/route', { method: 'POST', body: loadJob.body })
            .then((res) => {
                if (loadJob.seq !== routeLoadSeq) return;
                if (res && res.active) {
                    if (pendingRouteLoad && pendingRouteLoad.seq === loadJob.seq) {
                        _clearPendingRouteLoad();
                    }
                    lastSyncedRouteSig = loadJob.sig;
                    userClearedRouteSig = '';
                    handleRouteUpdatedEvent(res, { fit: false });
                    const capturedTitle = String(res.title || 'Route');
                    const capturedSecs = Number(res.section_count || 1);
                    setLog(
                        tr('page.route.captured', {
                            title: capturedTitle,
                            sections: capturedSecs
                        }),
                        '#38bdf8'
                    );
                    showMiniToast(
                        tr('page.route.captured_hint', {
                            title: capturedTitle,
                            sections: capturedSecs
                        }),
                        '#38bdf8'
                    );
                    return;
                }
                throw new Error('Route load returned inactive status');
            })
            .catch(() => {
                if (loadJob.seq !== routeLoadSeq) return;
                const attemptIdx = loadJob.attempts;
                loadJob.attempts += 1;
                if (attemptIdx < retryDelays.length && (loadJob.fromApi || document.querySelector('.route-detail'))) {
                    if (routeLoadRetryTimer) clearTimeout(routeLoadRetryTimer);
                    routeLoadRetryTimer = setTimeout(() => {
                        routeLoadRetryTimer = null;
                        _dispatchRouteLoadAttempt(loadJob);
                    }, retryDelays[attemptIdx]);
                }
            });
    }

    function handleHoyolabApiResponse(urlStr, jsonData, opts = {}) {
        try {
            if (!jsonData || typeof jsonData !== 'object') return false;
            const d = jsonData.data;
            if (!d || typeof d !== 'object') return false;
            const urlNorm = typeof urlStr === 'string' ? urlStr : String(urlStr || '');
            const lowUrl = urlNorm.toLowerCase();

            if (Array.isArray(d.list)) {
                let touchedList = false;
                for (const item of d.list) {
                    if (!item || typeof item !== 'object') continue;
                    const itemIds = Array.from(new Set(
                        [item.path_id, item.obj_id, item.id]
                            .map((v) => String(v || '').trim())
                            .filter(Boolean)
                    ));
                    const itemPid = itemIds[0] || '';
                    const itemTitles = Array.from(new Set(
                        [item.path_name, item.name, item.post_title]
                            .map((v) => String(v || '').trim())
                            .filter(Boolean)
                    ));
                    const itemTitle = itemTitles[0] || '';
                    if (itemPid && (lowUrl.includes('get_recommend_path') || item.path_id || item.path_name)) {
                        touchedList = true;
                        const metaRec = {
                            path_id: itemPid,
                            title: itemTitle,
                            titles: itemTitles,
                            description: String(item.content || ''),
                            share_url: String(item.url || item.post_link || ''),
                            map_id: Number(item.map_id || getCurrentWebMapId())
                        };
                        for (const idKey of itemIds) {
                            hoyolabRouteMetaCache.set(idKey, metaRec);
                        }
                    }
                    const embeddedPath = _extractHoyolabPathObj(item);
                    if (embeddedPath) {
                        touchedList = true;
                        const wrapped = { data: { path: embeddedPath } };
                        const embPid = itemPid || String(embeddedPath.obj_id || embeddedPath.path_id || '');
                        const cacheEntry = {
                            raw: wrapped,
                            path_id: embPid,
                            title: itemTitle,
                            map_id: Number(item.map_id || getCurrentWebMapId())
                        };
                        for (const idKey of Array.from(new Set([...itemIds, embPid].filter(Boolean)))) {
                            _setBoundedCache(hoyolabRoutePayloadCache, idKey, cacheEntry);
                        }
                        for (const t of itemTitles) {
                            const normTitle = _normalizeRouteTitleKey(t);
                            if (normTitle) {
                                _setBoundedCache(hoyolabRouteByTitleCache, normTitle, cacheEntry);
                            }
                        }
                    }
                }
                if (touchedList) return true;
            }

            const pathObj = _extractHoyolabPathObj(jsonData);
            if (!pathObj) return false;

            const isLiveApi = !urlNorm.startsWith('vue://') && !urlNorm.startsWith('cache://');
            const curMapId = getCurrentWebMapId();
            const candidateIds = Array.from(new Set(
                [pathObj.obj_id, pathObj.path_id, pathObj.id, _extractShareIdFromUrl(urlNorm), _extractShareIdFromUrl(window.location ? window.location.href : '')]
                    .map((v) => String(v || '').trim())
                    .filter(Boolean)
            ));
            const pid = String(pathObj.obj_id || pathObj.path_id || pathObj.id || '').trim();
            const urlShareId = _extractShareIdFromUrl(urlNorm) || _extractShareIdFromUrl(window.location ? window.location.href : '');
            let cached = null;
            for (const idKey of candidateIds) {
                if (hoyolabRouteMetaCache.has(idKey)) {
                    cached = hoyolabRouteMetaCache.get(idKey);
                    break;
                }
            }
            const detailEl = document.querySelector('.route-detail');
            const domTitle = detailEl ? _readRouteDetailDomHeader(detailEl) : '';
            const metaTitle = (cached && cached.title) || pathObj.post_title || pathObj.name || pathObj.path_name || '';
            // Only fall back to the DOM header when API/meta title is empty (for vue:// or untitled
            // responses), avoiding grabbing the previous route's title before Vue re-renders.
            const title = metaTitle || (!isLiveApi ? domTitle : '') || domTitle || '';

            const cacheEntry = { raw: jsonData, path_id: pid || urlShareId, title, map_id: curMapId };
            for (const idKey of candidateIds) {
                _setBoundedCache(hoyolabRoutePayloadCache, idKey, cacheEntry);
            }
            const cacheTitles = Array.from(new Set(
                [
                    title,
                    cached && cached.title,
                    ...((cached && Array.isArray(cached.titles)) ? cached.titles : []),
                    pathObj.post_title,
                    pathObj.name,
                    pathObj.path_name,
                    !isLiveApi ? domTitle : ''
                ].map((v) => String(v || '').trim()).filter(Boolean)
            ));
            for (const ct of cacheTitles) {
                const titleKey = _normalizeRouteTitleKey(ct);
                if (titleKey) _setBoundedCache(hoyolabRouteByTitleCache, titleKey, cacheEntry);
            }

            const sig = _computeRouteSig(pathObj, curMapId);
            const force = Boolean(opts.force || isLiveApi);
            if (!force && sig && sig === userClearedRouteSig) {
                return false;
            }
            if (!force && sig && sig === lastSyncedRouteSig && activeWebRoute && activeWebRoute.active) {
                return false;
            }
            if (force || (sig && sig !== userClearedRouteSig)) {
                userClearedRouteSig = '';
            }

            const ctx = findMapContext();
            const markedSet = cachedMarkedIdsByMap.get(curMapId) || (ctx ? _collectMarkedPinIds(ctx) : new Set());
            const markedIds = Array.from(markedSet);

            routeInspecting = true;
            setAutoPanPinned(false, { silent: true });
            const reqBody = {
                action: 'load',
                raw: jsonData,
                path_id: pid || undefined,
                title: title || undefined,
                description: (cached && cached.description) || undefined,
                share_url: (cached && cached.share_url) || pathObj.post_link || (window.location ? window.location.href : undefined),
                map_id: curMapId,
                marked_ids: markedIds
            };
            routeLoadSeq += 1;
            const loadJob = {
                seq: routeLoadSeq,
                sig,
                body: reqBody,
                attempts: 0,
                fromApi: isLiveApi
            };
            pendingRouteLoad = loadJob;
            if (routeLoadRetryTimer) {
                clearTimeout(routeLoadRetryTimer);
                routeLoadRetryTimer = null;
            }
            _dispatchRouteLoadAttempt(loadJob);
            return true;
        } catch (e) {
            return false;
        }
    }

    function _normalizeHookUrl(raw) {
        if (typeof raw === 'string') return raw;
        if (raw && typeof raw.url === 'string') return raw.url;
        if (raw && typeof raw.href === 'string') return raw.href;
        try { return raw ? String(raw) : ''; } catch (e) { return ''; }
    }

    function _isRouteHookUrl(urlStr) {
        const u = _normalizeHookUrl(urlStr);
        if (!u) return false;
        const low = u.toLowerCase();
        if (
            low.includes('point_list') ||
            low.includes('/point/list') ||
            low.includes('label_list') ||
            low.includes('/label/tree') ||
            low.includes('mark_map_point') ||
            low.includes('/map/anchor') ||
            low.includes('/map/info') ||
            low.includes('/point/info')
        ) {
            return false;
        }
        if (low.includes('get_recommend_path') || low.includes('get_share_map_spots')) {
            return true;
        }
        return low.includes('/map_user/') && (
            low.includes('path') ||
            low.includes('route') ||
            low.includes('spot') ||
            low.includes('share') ||
            low.includes('line') ||
            low.includes('plan')
        );
    }

    function installHoYoLabRouteHooks() {
        window.__ghOnHoyolabRouteApi = handleHoyolabApiResponse;
        const earlyBuf = window.__ghRouteHookBuffer;
        if (earlyBuf && typeof earlyBuf === 'object') {
            if (Array.isArray(earlyBuf.recommends)) {
                for (const item of earlyBuf.recommends) {
                    if (item && item.data) {
                        handleHoyolabApiResponse(item.url || 'get_recommend_path', item.data);
                    }
                }
            }
            if (earlyBuf.routesById && typeof earlyBuf.routesById === 'object') {
                for (const [k, item] of Object.entries(earlyBuf.routesById)) {
                    if (!item || !item.data) continue;
                    const po = _extractHoyolabPathObj(item.data);
                    if (!po) continue;
                    const pid = String(po.obj_id || po.path_id || k || '');
                    const meta = pid ? hoyolabRouteMetaCache.get(pid) : null;
                    const title = (meta && meta.title) || po.post_title || po.name || po.path_name || '';
                    const cacheEntry = { raw: item.data, path_id: pid || k, title, map_id: getCurrentWebMapId() };
                    _setBoundedCache(hoyolabRoutePayloadCache, k, cacheEntry);
                    if (pid && pid !== k) _setBoundedCache(hoyolabRoutePayloadCache, pid, cacheEntry);
                    const tk = _normalizeRouteTitleKey(title);
                    if (tk) _setBoundedCache(hoyolabRouteByTitleCache, tk, cacheEntry);
                }
            }
            if (earlyBuf.lastRoute && earlyBuf.lastRoute.data) {
                handleHoyolabApiResponse(earlyBuf.lastRoute.url || 'get_share_map_spots', earlyBuf.lastRoute.data, { force: true });
            }
        }

        if (window.__ghRouteHookVer === 3) return;
        window.__ghRouteHookInstalled = true;
        window.__ghRouteHookVer = 3;

        if (typeof window.fetch === 'function') {
            const origFetch = window.fetch;
            window.fetch = function (...args) {
                const urlStr = _normalizeHookUrl(args[0]);
                const p = origFetch.apply(this, args);
                if (_isRouteHookUrl(urlStr)) {
                    p.then((res) => {
                        if (!res || !res.ok || typeof res.clone !== 'function') return;
                        res.clone().json()
                            .then((j) => {
                                if (typeof window.__ghOnHoyolabRouteApi === 'function') {
                                    window.__ghOnHoyolabRouteApi(urlStr, j);
                                }
                            })
                            .catch(() => {});
                    }).catch(() => {});
                }
                return p;
            };
        }

        if (window.XMLHttpRequest && window.XMLHttpRequest.prototype) {
            const origOpen = window.XMLHttpRequest.prototype.open;
            const origSend = window.XMLHttpRequest.prototype.send;
            window.XMLHttpRequest.prototype.open = function (method, url, ...rest) {
                this.__ghReqUrl = _normalizeHookUrl(url);
                return origOpen.call(this, method, url, ...rest);
            };
            window.XMLHttpRequest.prototype.send = function (...args) {
                const u = this.__ghReqUrl || '';
                if (_isRouteHookUrl(u)) {
                    this.addEventListener('load', () => {
                        try {
                            if (this.status >= 200 && this.status < 300) {
                                let payload = null;
                                if (this.responseType === 'json') {
                                    payload = this.response;
                                } else if (!this.responseType || this.responseType === 'text') {
                                    payload = (typeof this.responseText === 'string' && this.responseText)
                                        ? JSON.parse(this.responseText)
                                        : null;
                                } else if (this.responseType === 'arraybuffer' && this.response && typeof TextDecoder === 'function') {
                                    const txt = new TextDecoder('utf-8').decode(new Uint8Array(this.response));
                                    payload = txt ? JSON.parse(txt) : null;
                                }
                                if (payload && typeof window.__ghOnHoyolabRouteApi === 'function') {
                                    window.__ghOnHoyolabRouteApi(u, payload);
                                }
                            }
                        } catch (e) {}
                    }, { once: true });
                }
                return origSend.apply(this, args);
            };
        }
    }

    function _extractOpenRouteFromVue(detailEl) {
        const queue = [];
        const enqueued = new Set();
        const pushObj = (obj, depth) => {
            if (!obj || typeof obj !== 'object' || enqueued.has(obj)) return;
            if (typeof Node !== 'undefined' && obj instanceof Node) return;
            if (obj === window || obj === document) return;
            enqueued.add(obj);
            queue.push({ obj, depth });
        };
        const enqueueVm = (vm, baseDepth) => {
            if (!vm || typeof vm !== 'object') return;
            try { if (vm._data && typeof vm._data === 'object') pushObj(vm._data, baseDepth); } catch (e) {}
            try { if (vm.$data && typeof vm.$data === 'object') pushObj(vm.$data, baseDepth); } catch (e) {}
            try { if (vm.$props && typeof vm.$props === 'object') pushObj(vm.$props, baseDepth); } catch (e) {}
            try { if (vm.$store && vm.$store.state && typeof vm.$store.state === 'object') pushObj(vm.$store.state, baseDepth + 1); } catch (e) {}
            try { if (vm.$store && vm.$store.getters && typeof vm.$store.getters === 'object') pushObj(vm.$store.getters, baseDepth + 1); } catch (e) {}
            try {
                if (vm._computedWatchers && typeof vm._computedWatchers === 'object') {
                    const cwKeys = Object.keys(vm._computedWatchers);
                    for (let i = 0; i < cwKeys.length && i < 32; i++) {
                        const w = vm._computedWatchers[cwKeys[i]];
                        if (w && w.value && typeof w.value === 'object') pushObj(w.value, baseDepth + 1);
                    }
                }
            } catch (e) {}
            pushObj(vm, baseDepth + 1);
            try { if (vm.$parent && typeof vm.$parent === 'object') pushObj(vm.$parent, baseDepth + 1); } catch (e) {}
            try {
                if (Array.isArray(vm.$children)) {
                    for (let i = 0; i < vm.$children.length && i < 20; i++) {
                        pushObj(vm.$children[i], baseDepth + 1);
                    }
                }
            } catch (e) {}
        };
        const enqueueVue3 = (comp, baseDepth) => {
            if (!comp || typeof comp !== 'object') return;
            try { if (comp.props && typeof comp.props === 'object') pushObj(comp.props, baseDepth); } catch (e) {}
            try { if (comp.setupState && typeof comp.setupState === 'object') pushObj(comp.setupState, baseDepth); } catch (e) {}
            try { if (comp.data && typeof comp.data === 'object') pushObj(comp.data, baseDepth); } catch (e) {}
            try { if (comp.ctx && typeof comp.ctx === 'object') pushObj(comp.ctx, baseDepth + 1); } catch (e) {}
            try { if (comp.proxy && typeof comp.proxy === 'object') pushObj(comp.proxy, baseDepth + 1); } catch (e) {}
            try { if (comp.parent && typeof comp.parent === 'object') pushObj(comp.parent, baseDepth + 1); } catch (e) {}
        };
        const inspectEl = (el, baseDepth) => {
            if (!el) return;
            if (el.__vue__) enqueueVm(el.__vue__, baseDepth);
            if (el.__vueParentComponent) enqueueVue3(el.__vueParentComponent, baseDepth);
        };

        // 1. Start at `.route-detail`, walk up its ancestors and inspect inner elements first so
        //    the currently mounted route component wins over unrelated map state.
        let anc = detailEl;
        for (let up = 0; anc && up < 6; up++) {
            inspectEl(anc, up === 0 ? 0 : 1);
            anc = anc.parentElement;
        }
        if (detailEl && typeof detailEl.querySelectorAll === 'function') {
            const inner = detailEl.querySelectorAll('*');
            let picked = 0;
            for (let i = 0; i < inner.length && picked < 24; i++) {
                if (inner[i].__vue__ || inner[i].__vueParentComponent) {
                    inspectEl(inner[i], 1);
                    picked += 1;
                }
            }
        }
        // 2. Fallback to root GIS / app Vuex stores.
        const mapCtx = findMapContext();
        if (mapCtx && mapCtx.vm) enqueueVm(mapCtx.vm, 1);
        inspectEl(document.querySelector('.mhy-game-gis'), 1);
        inspectEl(document.querySelector('#root'), 2);
        inspectEl(document.querySelector('#app'), 2);

        const skipKeys = new Set([
            'map', 'ciLayer', 'markerList', 'pointList', 'allPoints', 'labelList',
            'canvasLayeredImageLayer', 'leaflet', '_layers', '_map', '$el', '$root',
            '$vnode', '_vnode', '$options', '$listeners', '$attrs', '$slots', '$scopedSlots'
        ]);
        let scanned = 0;
        while (queue.length > 0 && scanned < 360) {
            const { obj, depth } = queue.shift();
            scanned += 1;
            if (_isValidHoyolabPathObj(obj)) {
                return obj;
            }
            if (depth >= 6) continue;
            if (obj instanceof Map) {
                let mCount = 0;
                for (const val of obj.values()) {
                    if (mCount++ >= 40) break;
                    if (val && typeof val === 'object') {
                        if (_isValidHoyolabPathObj(val)) return val;
                        pushObj(val, depth + 1);
                    }
                }
                continue;
            }
            if (Array.isArray(obj)) {
                if (obj.length > 40) continue;
                for (let i = 0; i < obj.length; i++) {
                    const el = obj[i];
                    if (el && typeof el === 'object') {
                        if (_isValidHoyolabPathObj(el)) return el;
                        pushObj(el, depth + 1);
                    }
                }
                continue;
            }
            // Unwrap Vue 3 RefImpl / ComputedRefImpl where `value` is a prototype getter or `_value` is underscored.
            if (obj.__v_isRef || Object.prototype.hasOwnProperty.call(obj, '_value')) {
                let refVal = null;
                try { refVal = (obj.value !== undefined) ? obj.value : obj._value; } catch (e) {}
                if (refVal && typeof refVal === 'object') {
                    if (_isValidHoyolabPathObj(refVal)) return refVal;
                    pushObj(refVal, depth + 1);
                }
            }
            let keys;
            try { keys = Object.keys(obj); } catch (e) { continue; }
            for (let i = 0; i < keys.length && i < 60; i++) {
                const k = keys[i];
                if (skipKeys.has(k) || k.startsWith('__')) continue;
                if (k.startsWith('$') && k !== '$data' && k !== '$props' && k !== '$store' && k !== '$parent') continue;
                if (k.startsWith('_') && k !== '_data' && k !== '_value') continue;
                let child;
                try { child = obj[k]; } catch (e) { continue; }
                if (!child || typeof child !== 'object') continue;
                if (_isValidHoyolabPathObj(child)) {
                    return child;
                }
                pushObj(child, depth + 1);
            }
        }
        return null;
    }

    function _lookupCachedRouteForOpenDom(detailEl) {
        const hrefSid = _extractShareIdFromUrl(window.location ? window.location.href : '');
        if (hrefSid && hoyolabRoutePayloadCache.has(hrefSid)) {
            return hoyolabRoutePayloadCache.get(hrefSid);
        }
        const domTitle = _readRouteDetailDomHeader(detailEl);
        const normTitle = _normalizeRouteTitleKey(domTitle);
        if (normTitle) {
            if (hoyolabRouteByTitleCache.has(normTitle)) {
                return hoyolabRouteByTitleCache.get(normTitle);
            }
            for (const [pid, meta] of hoyolabRouteMetaCache.entries()) {
                if (!meta || !hoyolabRoutePayloadCache.has(pid)) continue;
                const titles = Array.isArray(meta.titles) && meta.titles.length > 0 ? meta.titles : [meta.title];
                if (titles.some((t) => _normalizeRouteTitleKey(t) === normTitle)) {
                    return hoyolabRoutePayloadCache.get(pid);
                }
            }
            // Substring fallback when `.route-detail__row-header` appends a badge/count suffix.
            if (normTitle.length >= 4) {
                for (const [tk, entry] of hoyolabRouteByTitleCache.entries()) {
                    if (tk.length >= 4 && (normTitle.includes(tk) || tk.includes(normTitle))) {
                        return entry;
                    }
                }
                for (const [pid, meta] of hoyolabRouteMetaCache.entries()) {
                    if (!meta || !hoyolabRoutePayloadCache.has(pid)) continue;
                    const titles = Array.isArray(meta.titles) && meta.titles.length > 0 ? meta.titles : [meta.title];
                    if (titles.some((t) => {
                        const nt = _normalizeRouteTitleKey(t);
                        return nt.length >= 4 && (normTitle.includes(nt) || nt.includes(normTitle));
                    })) {
                        return hoyolabRoutePayloadCache.get(pid);
                    }
                }
            }
        }
        return null;
    }

    function tryCaptureOpenRouteFromVue(opts = {}) {
        try {
            const detailEl = document.querySelector('.route-detail');
            if (!detailEl) return false;
            const force = Boolean(opts.force);
            const curMapId = getCurrentWebMapId();

            const vuePathObj = _extractOpenRouteFromVue(detailEl);
            if (vuePathObj) {
                const sig = _computeRouteSig(vuePathObj, curMapId);
                if (!force && sig && sig === userClearedRouteSig) return false;
                if (!force && sig && sig === lastSyncedRouteSig && activeWebRoute && activeWebRoute.active) {
                    return true;
                }
                return handleHoyolabApiResponse('vue://route-detail', { data: { path: vuePathObj } }, { force });
            }

            const cachedEntry = _lookupCachedRouteForOpenDom(detailEl);
            if (cachedEntry && cachedEntry.raw) {
                const cachedPath = _extractHoyolabPathObj(cachedEntry.raw);
                const sig = cachedPath ? _computeRouteSig(cachedPath, curMapId) : '';
                if (!force && sig && sig === userClearedRouteSig) return false;
                if (!force && sig && sig === lastSyncedRouteSig && activeWebRoute && activeWebRoute.active) {
                    return true;
                }
                return handleHoyolabApiResponse('cache://route-detail', cachedEntry.raw, { force });
            }

            if (!opts.silent && !(activeWebRoute && activeWebRoute.active) && !window.__ghRouteReclickHintShown) {
                window.__ghRouteReclickHintShown = true;
                showMiniToast(tr('page.route.reclick_hint'), '#facc15');
            }
        } catch (e) {}
        return false;
    }

    function scheduleRouteDomCapture(opts = {}) {
        _clearRouteDetailTimers();
        const delays = Array.isArray(opts.delays) ? opts.delays : [140, 520];
        for (const delay of delays) {
            const tid = setTimeout(() => {
                try {
                    tryCaptureOpenRouteFromVue({ force: Boolean(opts.force), silent: opts.silent !== false });
                } catch (e) {}
            }, delay);
            routeDetailTimers.push(tid);
        }
    }

    function installRouteDetailDomObserver() {
        routeDomClickHandler = (ev) => {
            try {
                const target = ev && ev.target;
                if (!target || typeof target.closest !== 'function') return;
                if (target.closest('img.route-detail__close, .route-detail__close')) {
                    routeInspecting = false;
                    userClearedRouteSig = '';
                    lastObservedRouteDomFp = '';
                    if (activeWebRoute) renderRouteBar(activeWebRoute);
                    return;
                }
                const secEl = target.closest('.route-detail__section');
                if (secEl) {
                    if (activeWebRoute && activeWebRoute.active) {
                        const allSecs = Array.from(document.querySelectorAll('.route-detail .route-detail__section, .route-detail__section'));
                        const idx = allSecs.indexOf(secEl);
                        if (idx >= 0) {
                            sendRouteSectionSelect(idx);
                        }
                    } else {
                        userClearedRouteSig = '';
                        tryCaptureOpenRouteFromVue({ force: true, silent: true });
                    }
                    return;
                }
                // User clicked a route card/item/tab in HoYoLAB's Route drawer: clear any manual
                // stop suppression and schedule Vue/cache inspection in case HoYoLAB serves the
                // selected route from memory without issuing a fresh XHR.
                const routeUiHit = target.closest(
                    '[class*="route-item"], [class*="route-card"], [class*="route-list"], ' +
                    '[class*="path-item"], [class*="path-card"], [class*="path-list"], ' +
                    '[class*="route-panel"], [class*="recommend-route"], .route-detail'
                );
                if (routeUiHit) {
                    userClearedRouteSig = '';
                    scheduleRouteDomCapture({ force: true, silent: true });
                }
            } catch (e) {}
        };
        document.addEventListener('click', routeDomClickHandler, true);

        // Observe `.route-detail` mounting, switching routes in-place, or closing.
        if (!routeDetailObserver && typeof MutationObserver === 'function') {
            const root = document.body || document.documentElement;
            if (root) {
                routeDetailObserver = new MutationObserver(() => {
                    try {
                        const detailEl = document.querySelector('.route-detail');
                        if (!detailEl) {
                            if (lastObservedRouteDomFp) {
                                lastObservedRouteDomFp = '';
                                userClearedRouteSig = '';
                            }
                            return;
                        }
                        const fp = _readRouteDetailDomFp(detailEl);
                        if (fp && fp !== lastObservedRouteDomFp) {
                            lastObservedRouteDomFp = fp;
                            userClearedRouteSig = '';
                            scheduleRouteDomCapture({ force: false, silent: true });
                        }
                    } catch (e) {}
                });
                routeDetailObserver.observe(root, { childList: true, subtree: true });
            }
        }

        // HoYoLAB also updates `#/map/2?share_id=...&share_type=path` when switching routes.
        routeHashChangeHandler = () => {
            try {
                const href = window.location ? window.location.href : '';
                if (href.includes('share_id=') || href.includes('share_type=path') || document.querySelector('.route-detail')) {
                    userClearedRouteSig = '';
                    scheduleRouteDomCapture({ force: true, silent: true, delays: [180, 620] });
                }
            } catch (e) {}
        };
        window.addEventListener('hashchange', routeHashChangeHandler);
    }

    function syncInitialRouteState() {
        extRequest('/route', { method: 'GET' })
            .then((res) => {
                if (res && res.active) {
                    handleRouteUpdatedEvent(res, { fit: false });
                    return;
                }
                // App connected with no active route: replay any route load that failed while
                // offline, or capture a route already open in `.route-detail`.
                if (pendingRouteLoad && pendingRouteLoad.body) {
                    pendingRouteLoad.attempts = 0;
                    _dispatchRouteLoadAttempt(pendingRouteLoad);
                } else if (document.querySelector('.route-detail') && !userClearedRouteSig) {
                    tryCaptureOpenRouteFromVue({ force: false, silent: true });
                }
            })
            .catch(() => {});
    }

    function handleLocalCommand(cmdData) {
        if (!cmdData || typeof cmdData !== 'object') return;
        const cmd = cmdData.type || cmdData.kind;
        if (cmd === 'GH_CMD_PLAN_TSP') {
            requestPlanTspRoute(cmdData);
        } else if (cmd === 'GH_CMD_CLEAR_TSP') {
            clearTspRouteFromMap({ notifyToast: true });
            extRequest('/plan_tsp_route', {
                method: 'POST',
                body: { map_id: getCurrentWebMapId(), pins: [] }
            }).catch(() => {});
        } else if (cmd === 'GH_CMD_EXPORT_PINS') {
            exportMarkedPins(cmdData.targetFormat || cmdData.target_format || 'canonical');
        } else if (cmd === 'GH_CMD_ROUTE_EXPORT') {
            handleRouteExportCommand();
        } else if (cmd === 'GH_CMD_ROUTE_IMPORT') {
            extRequest('/route', { method: 'GET' })
                .then((res) => {
                    if (res && res.active) handleRouteUpdatedEvent(res, { fit: Boolean(cmdData.fit) });
                })
                .catch(() => {});
        } else if (cmd === 'GH_CMD_ROUTE_CLEAR') {
            userClearedRouteSig = lastSyncedRouteSig || '__manual_stop__';
            _clearPendingRouteLoad();
            handleRouteUpdatedEvent({ active: false });
        } else if (cmd === 'GH_CMD_REFRESH_PINS') {
            const midNow = getCurrentWebMapId();
            const c = findMapContext();
            if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                pushNearbyPinsToServer(
                    playerLastState.lat,
                    playerLastState.lng,
                    midNow,
                    playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                    { force: true }
                );
            } else {
                pushLanPinsSnapshot(c, midNow, true);
            }
        }
    }

    window.__GH_BRIDGE__ = Object.freeze({
        version: SCRIPT_VERSION,
        getContext: () => findMapContext(),
        getCurrentMapId: () => getCurrentWebMapId(),
        getPlayerState: () => (playerLastState ? { ...playerLastState } : null),
        updatePlayerMarker: (data) => {
            const ctx = findMapContext();
            if (ctx && ctx.map) {
                updatePlayerMarker(ctx, data);
                return true;
            }
            return false;
        },
        queryNearbyUncollectedPins,
        pushNearbyPinsToServer,
        renderTspRouteOnMap,
        clearTspRouteFromMap,
        requestPlanTspRoute,
        exportMarkedPins,
        refreshMarkedPins: (mapId) => {
            const mid = (mapId !== undefined && mapId !== null) ? Number(mapId) : getCurrentWebMapId();
            cachedMarkedFetchedAt.delete(mid);
            const ctx = findMapContext();
            const vm = resolveGisVm(ctx);
            if (vm && typeof vm.initMarkStatus === 'function') {
                try { vm.initMarkStatus(); } catch (e) {}
            } else if (vm && typeof vm.$gemit === 'function') {
                try { vm.$gemit('markStatusChanged'); } catch (e) {}
            }
            return _refreshMarkedPinIds(mid, true);
        },
        getActiveRoute: () => activeWebRoute,
        isRouteInspecting: () => routeInspecting,
        setRouteInspecting: (v) => {
            routeInspecting = Boolean(v);
            if (activeWebRoute) renderRouteBar(activeWebRoute);
        },
        getLanguage: () => (window.GHI18n ? window.GHI18n.getLang() : 'en'),
        setLanguage: (lang) => {
            if (window.GHI18n && typeof window.GHI18n.setLang === 'function') {
                return window.GHI18n.setLang(lang);
            }
            return Promise.resolve(lang);
        },
        isCompactUI: () => compactUiEnabled,
        setCompactUI: (enabled) => setCompactUI(Boolean(enabled)),
        toggleCompactUI: () => setCompactUI(!compactUiEnabled),
        isAutoPanEnabled: () => autoPanEnabled,
        setAutoPan: (enabled) => setAutoPanPinned(Boolean(enabled)),
        recenter: () => recenterToPlayer(),
        unwrapAngle
    });

    // Inject marker/toast/compact styles, restore opt-in compact mode, attach drag-unpin, and sync initial status
    injectThemeStyles();
    installHoYoLabRouteHooks();
    installRouteDetailDomObserver();
    installPopupNavigateObserver();
    if (compactUiEnabled && document.body) {
        document.body.classList.add('gh-compact-ui');
        applyCompactDomTags();
        setTimeout(applyCompactDomTags, 1200);
    }
    bindDragUnpinListeners(findMapContext());
    _ensureMarkRedrawHooks(findMapContext());
    setTimeout(() => {
        const c = findMapContext();
        bindDragUnpinListeners(c);
        _ensureMarkRedrawHooks(c);
        installPopupNavigateObserver();
        tryCaptureOpenRouteFromVue();
    }, 1000);
    _refreshMarkedPinIds(getCurrentWebMapId(), false);
    syncInitialRouteState();
    tryCaptureOpenRouteFromVue();
    const scheduleFilterPinRefresh = (userInitiated = true) => {
        if (filterRefreshTimer) clearTimeout(filterRefreshTimer);
        filterRefreshTimer = setTimeout(() => {
            filterRefreshTimer = null;
            try {
                const midNow = getCurrentWebMapId();
                const c = findMapContext();
                if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                    pushNearbyPinsToServer(
                        playerLastState.lat,
                        playerLastState.lng,
                        midNow,
                        playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                        { force: true, userInitiated: Boolean(userInitiated), filterChanged: true }
                    );
                } else {
                    pushLanPinsSnapshot(c, midNow, true, Boolean(userInitiated));
                }
            } catch (e) {}
        }, 220);
    };
    // Refresh marked pin cache after popup clicks, and force nearby/radar pin sync on filter toggles.
    const onDocClick = (ev) => {
        try {
            const popupTarget = ev.target && ev.target.closest ? ev.target.closest('.map-popup, .expand-popup, .leaflet-popup') : null;
            if (popupTarget) {
                reportTabVisibility(false, { interacted: true });
                _ensureMarkRedrawHooks(findMapContext());
                setTimeout(() => {
                    const midNow = getCurrentWebMapId();
                    _refreshMarkedPinIds(midNow, true).finally(() => {
                        const c = findMapContext();
                        _reconcileMarkedVisuals(c, { skipSelectedPopup: true });
                        if (playerLastState && typeof playerLastState.lat === 'number' && typeof playerLastState.lng === 'number') {
                            pushNearbyPinsToServer(
                                playerLastState.lat,
                                playerLastState.lng,
                                midNow,
                                playerLastState.floor_id !== undefined ? playerLastState.floor_id : (playerLastState.floor_short || null),
                                { force: true, userInitiated: true }
                            );
                        } else {
                            pushLanPinsSnapshot(c, midNow, true, true);
                        }
                    });
                }, 600);
                return;
            }
            const filterTarget = ev.target && ev.target.closest
                ? ev.target.closest('.filter-panel, [class*="filter-item"], [class*="label-item"], [class*="category-item"], [class*="selected-labels"]')
                : null;
            if (filterTarget) {
                reportTabVisibility(false, { interacted: true });
                scheduleFilterPinRefresh(true);
            }
        } catch (e) {}
    };
    document.addEventListener('click', onDocClick, true);
    window.__gh_teardown = () => {
        try { window.__ghOnHoyolabRouteApi = null; } catch (e) {}
        lastOpenedPopupRef = null;
        lastEnabledCatSig = null;
        try {
            if (popupNavObserver) {
                popupNavObserver.disconnect();
                popupNavObserver = null;
            }
        } catch (e) {}
        try {
            if (popupNavDebounceTimer) {
                clearTimeout(popupNavDebounceTimer);
                popupNavDebounceTimer = null;
            }
            if (filterRefreshTimer) {
                clearTimeout(filterRefreshTimer);
                filterRefreshTimer = null;
            }
        } catch (e) {}
        try { window.removeEventListener('message', onWindowMessage); } catch (e) {}
        try { document.removeEventListener('visibilitychange', onVisibilityChange); } catch (e) {}
        try { document.removeEventListener('visibilitychange', onVisibilityReport); } catch (e) {}
        try { window.removeEventListener('focus', onWindowFocusReport); } catch (e) {}
        try { window.removeEventListener('pagehide', onPageHideReport); } catch (e) {}
        try { clearInterval(tabVisibilityTimer); } catch (e) {}
        try { document.removeEventListener('click', onDocClick, true); } catch (e) {}
        try {
            if (routeDomClickHandler) document.removeEventListener('click', routeDomClickHandler, true);
            if (routeDetailObserver) {
                routeDetailObserver.disconnect();
                routeDetailObserver = null;
            }
            if (routeHashChangeHandler) {
                window.removeEventListener('hashchange', routeHashChangeHandler);
                routeHashChangeHandler = null;
            }
            _clearRouteDetailTimers();
            _clearPendingRouteLoad();
        } catch (e) {}
        try { clearInterval(reportStateTimer); } catch (e) {}
        try { clearLayerTimers(); } catch (e) {}
        try { clearMinimapAuxTimers(); } catch (e) {}
        try { if (typeof dragListenersCleanup === 'function') dragListenersCleanup(); } catch (e) {}
        try {
            if (ghRouteBarEl) {
                ghRouteBarEl.remove();
                ghRouteBarEl = null;
            }
            if (ghLockPillEl) {
                ghLockPillEl.remove();
                ghLockPillEl = null;
            }
            activeLockedPin = null;
        } catch (e) {}
        try {
            if (playerMarkerLayer && typeof playerMarkerLayer.remove === 'function') {
                playerMarkerLayer.remove();
            }
        } catch (e) {}
        try {
            if (playerDomMapRef && typeof playerDomMapRef.off === 'function') {
                playerDomMapRef.off('move zoom viewreset zoomend', _repositionDomPlayerMarker);
            }
            if (playerDomMarkerEl) {
                playerDomMarkerEl.remove();
            }
        } catch (e) {}
        playerMarkerLayer = null;
        playerMarkerMapRef = null;
        playerMarkerWrapEl = null;
        playerDomMarkerEl = null;
        playerDomWrapEl = null;
        playerDomMapRef = null;
        _cleanupOrphanPlayerMarkers(null);
    };
    fetchInitialStatus();
    }

    bootGenshinHelper();

})();
