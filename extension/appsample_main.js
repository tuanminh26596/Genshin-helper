// Genshin Helper — appsample map adapter (page / MAIN world).
//
// Draws the live player marker (arrow + camera cone) on https://genshin-impact-map.appsample.com/
// and follows it with the map. Position events arrive from appsample_bridge.js.
//
// The site is a Leaflet 1.9 map (EPSG:3857) whose world is a small lat/lng box, so HoYoLAB map
// coordinates (map_id 2) are converted with a linear fit (calibrated from matched waypoints on
// both sites). Only map 2 (Teyvat) is supported; other maps hide the marker.
(function () {
  'use strict';

  if (window.location.hostname !== 'genshin-impact-map.appsample.com') return;
  if (window.__ghx_loaded) return;
  window.__ghx_loaded = true;

  // ---------------------------------------------------------------------------------------
  // HoYoLAB (lat, lng) -> appsample (lat, lng) calibration
  //   appsample.lat = LAT_C + LAT_K * hoyo.lat
  //   appsample.lng = LNG_C + LNG_K * hoyo.lng            (+ INAZUMA_LNG_SHIFT for Inazuma)
  // Fitted on 8 matched waypoints (Dawn Winery, Qingce, Liyue Harbor, Ritou, Tenshukaku,
  // Vimara, Court of Fontaine, Flower-Feather Clan); residuals <= ~6 HoYoLAB units.
  // appsample places Inazuma ~0.1895 deg west of where the single linear fit puts it, so it
  // gets its own shift. INAZUMA_MIN_LNG is a PROVISIONAL boundary (HoYoLAB lng) between the
  // Mondstadt/Liyue side (lng <= ~ -100) and Ritou (lng ~ 3065) -- tune it if the marker jumps.
  // ---------------------------------------------------------------------------------------
  const LAT_C = 0.198876;
  const LAT_K = -8.5694e-5;
  const LNG_C = 0.522898;
  const LNG_K = 8.58878e-5;
  const INAZUMA_MIN_LNG = 1800;
  const INAZUMA_LNG_SHIFT = -0.189517;

  const SUPPORTED_MAP_ID = 2;
  const FIRST_FIX_MIN_ZOOM = 14;
  const USER_DRAG_PAUSE_MS = 8000;
  const FAR_JUMP_PX = 700;
  const PANE_NAME = 'ghx-player-pane';

  function toSiteLatLng(lat, lng) {
    let siteLng = LNG_C + LNG_K * lng;
    if (lng >= INAZUMA_MIN_LNG) siteLng += INAZUMA_LNG_SHIFT;
    return [LAT_C + LAT_K * lat, siteLng];
  }

  // ----- Leaflet map capture -----------------------------------------------------------
  let map = null;
  let marker = null;
  let wrapEl = null;
  let lastData = null;
  let dirUnwrapped = null;
  let rotUnwrapped = null;
  let hadFirstFix = false;
  let userPausedUntil = 0;
  let connected = true;

  function adoptMap(m) {
    if (map || !m || typeof m.getCenter !== 'function' || typeof m.setView !== 'function') return;
    map = m;
    try {
      map.on('dragstart', () => { userPausedUntil = Date.now() + USER_DRAG_PAUSE_MS; });
    } catch (e) {}
    if (lastData) applyPosition(lastData);
  }

  function hookLeaflet(L) {
    if (!L || !L.Map || L.Map.__ghx_hooked) return;
    L.Map.__ghx_hooked = true;
    try {
      L.Map.addInitHook(function () { adoptMap(this); });
    } catch (e) {}
    // Fallback when the map was created before this script ran: every Leaflet map calls these
    // constantly, so the first call hands us the instance.
    ['getCenter', 'getZoom'].forEach((name) => {
      const orig = L.Map.prototype[name];
      if (typeof orig !== 'function') return;
      L.Map.prototype[name] = function () {
        if (!map) adoptMap(this);
        return orig.apply(this, arguments);
      };
    });
  }

  if (window.L) {
    hookLeaflet(window.L);
  } else {
    // Leaflet's UMD build ends with `window.L = exports`; catch that assignment so the init hook
    // is in place before the site constructs its map.
    let held;
    try {
      Object.defineProperty(window, 'L', {
        configurable: true,
        enumerable: true,
        get() { return held; },
        set(v) {
          held = v;
          try { hookLeaflet(v); } catch (e) {}
        }
      });
    } catch (e) {}
    let tries = 0;
    const poll = setInterval(() => {
      tries += 1;
      if (window.L) hookLeaflet(window.L);
      if (map || tries > 120) clearInterval(poll);
    }, 500);
  }

  // ----- Styles & marker ---------------------------------------------------------------
  function injectStyles() {
    if (document.getElementById('ghx-style')) return;
    const style = document.createElement('style');
    style.id = 'ghx-style';
    style.textContent = [
      '@keyframes ghx-pulse{0%{transform:translate(-50%,-50%) scale(.72);opacity:.85}75%{transform:translate(-50%,-50%) scale(1.65);opacity:0}100%{transform:translate(-50%,-50%) scale(1.65);opacity:0}}',
      '.ghx-wrap{position:relative;width:44px;height:44px;pointer-events:none;--ghx-dir:0deg;--ghx-rot:0deg}',
      '.ghx-wrap::before{content:"";position:absolute;left:50%;top:50%;width:28px;height:28px;transform:translate(-50%,-50%);border-radius:50%;background:rgba(15,23,42,.82);border:2px solid #fff;box-shadow:0 0 0 3px rgba(56,189,248,.55),0 2px 10px rgba(0,0,0,.55)}',
      '.ghx-pulse{position:absolute;left:50%;top:50%;width:34px;height:34px;border-radius:50%;border:2px solid #38bdf8;background:rgba(56,189,248,.22);animation:ghx-pulse 1.8s cubic-bezier(.2,.6,.3,1) infinite}',
      '.ghx-fov{position:absolute;left:50%;top:50%;width:72px;height:72px;transform-origin:50% 50%;transform:translate(-50%,-50%) rotate(var(--ghx-rot,0deg));opacity:0;transition:transform 90ms linear,opacity 160ms ease;pointer-events:none}',
      '.ghx-wrap.ghx-has-fov .ghx-fov{opacity:1}',
      '.ghx-arrow{position:absolute;left:50%;top:50%;width:30px;height:30px;transform-origin:50% 50%;transform:translate(-50%,-50%) rotate(var(--ghx-dir,0deg));transition:transform 90ms linear;filter:drop-shadow(0 2px 6px rgba(2,132,199,.85));pointer-events:none}',
      '.ghx-player-icon{background:transparent!important;border:0!important}'
    ].join('\n');
    (document.head || document.documentElement).appendChild(style);
  }

  function markerMarkup() {
    return (
      '<div class="ghx-wrap">' +
        '<div class="ghx-pulse"></div>' +
        '<svg class="ghx-fov" viewBox="0 0 64 64" aria-hidden="true">' +
          '<defs><radialGradient id="ghx-fov-grad" cx="50%" cy="50%" r="50%">' +
            '<stop offset="0%" stop-color="#38bdf8" stop-opacity="0.46"/>' +
            '<stop offset="68%" stop-color="#38bdf8" stop-opacity="0.16"/>' +
            '<stop offset="100%" stop-color="#38bdf8" stop-opacity="0"/>' +
          '</radialGradient></defs>' +
          '<path d="M32 32 L15.94 9.06 A28 28 0 0 1 48.06 9.06 Z" fill="url(#ghx-fov-grad)"/>' +
        '</svg>' +
        '<svg class="ghx-arrow" viewBox="0 0 32 32" aria-hidden="true">' +
          '<path d="M16 4.5 L25 24 L16 19.5 L7 24 Z" fill="#38bdf8" stroke="#ffffff" stroke-width="2" stroke-linejoin="round"/>' +
        '</svg>' +
      '</div>'
    );
  }

  function ensurePane() {
    if (!map) return null;
    let pane = typeof map.getPane === 'function' ? map.getPane(PANE_NAME) : null;
    if (!pane && typeof map.createPane === 'function') {
      pane = map.createPane(PANE_NAME);
      pane.style.zIndex = 640; // above markerPane (600), below popups (700)
      pane.style.pointerEvents = 'none';
    }
    return pane ? PANE_NAME : undefined;
  }

  function ensureMarker(latlng) {
    const L = window.L;
    if (!map || !L || typeof L.marker !== 'function') return false;
    injectStyles();
    if (!marker) {
      const pane = ensurePane();
      const opts = {
        icon: L.divIcon({
          className: 'ghx-player-icon',
          html: markerMarkup(),
          iconSize: [44, 44],
          iconAnchor: [22, 22]
        }),
        interactive: false,
        keyboard: false,
        zIndexOffset: 9000
      };
      if (pane) opts.pane = pane;
      marker = L.marker(latlng, opts).addTo(map);
      const el = marker.getElement();
      wrapEl = el ? el.querySelector('.ghx-wrap') : null;
    }
    return true;
  }

  function removeMarker() {
    if (marker) {
      try { marker.remove(); } catch (e) {}
    }
    marker = null;
    wrapEl = null;
    hadFirstFix = false;
  }

  function unwrapAngle(prev, next) {
    if (prev === null) return next;
    const d = ((next - prev + 540) % 360) - 180;
    return prev + d;
  }

  // ----- Position handling -------------------------------------------------------------
  function applyPosition(data) {
    if (!map || !data) return;
    const mapId = data.map_id ? parseInt(data.map_id, 10) : SUPPORTED_MAP_ID;
    if (mapId !== SUPPORTED_MAP_ID || !connected) {
      removeMarker();
      return;
    }

    const latlng = toSiteLatLng(data.lat, data.lng);
    if (!ensureMarker(latlng)) return;
    marker.setLatLng(latlng);

    if (!wrapEl || !wrapEl.isConnected) {
      const el = marker.getElement();
      wrapEl = el ? el.querySelector('.ghx-wrap') : null;
    }
    if (wrapEl) {
      if (typeof data.heading === 'number' && Number.isFinite(data.heading)) {
        dirUnwrapped = unwrapAngle(dirUnwrapped, data.heading);
        wrapEl.style.setProperty('--ghx-dir', dirUnwrapped.toFixed(2) + 'deg');
      }
      if (typeof data.camera_heading === 'number' && Number.isFinite(data.camera_heading)) {
        rotUnwrapped = unwrapAngle(rotUnwrapped, data.camera_heading);
        wrapEl.style.setProperty('--ghx-rot', rotUnwrapped.toFixed(2) + 'deg');
        wrapEl.classList.add('ghx-has-fov');
      } else {
        wrapEl.classList.remove('ghx-has-fov');
      }
    }

    followPlayer(latlng);
  }

  function followPlayer(latlng) {
    if (Date.now() < userPausedUntil) return;
    try {
      if (!hadFirstFix) {
        hadFirstFix = true;
        map.setView(latlng, Math.max(map.getZoom(), FIRST_FIX_MIN_ZOOM), { animate: false });
        return;
      }
      const target = map.latLngToContainerPoint(latlng);
      const center = map.latLngToContainerPoint(map.getCenter());
      const dist = Math.hypot(target.x - center.x, target.y - center.y);
      if (document.hidden || dist > FAR_JUMP_PX) {
        map.setView(latlng, map.getZoom(), { animate: false });
      } else if (dist > 2) {
        map.panTo(latlng, { animate: true, duration: 0.16, noMoveStart: true });
      }
    } catch (e) {}
  }

  window.addEventListener('message', (e) => {
    if (e.source !== window || !e.data || e.data.__gh_ext !== true || e.data.dir !== 'to-page') return;
    const msg = e.data;
    if (msg.kind === 'CONNECTION_STATUS') {
      connected = msg.connected !== false;
      if (!connected) removeMarker();
      return;
    }
    if (msg.kind === 'SERVER_EVENT' && msg.data) {
      const d = msg.data;
      if ((d.type === 'MINIMAP_POSITION' || d.type === 'LIVE_POSITION') &&
          typeof d.lat === 'number' && typeof d.lng === 'number') {
        connected = true;
        lastData = d;
        applyPosition(d);
      }
    }
  });
})();
