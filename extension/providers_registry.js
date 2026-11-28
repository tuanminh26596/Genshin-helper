// Shared registry of supported Genshin Impact interactive map providers.
// Loaded in the MV3 Service Worker (via importScripts) and Extension Popup (<script>).
(function (root) {
  'use strict';

  const PROVIDERS = {
    hoyolab: {
      id: 'hoyolab',
      name: 'HoYoLAB Map',
      shortName: 'HoYoLAB',
      openSource: false,
      defaultUrl: 'https://act.hoyolab.com/ys/app/interactive-map/index.html#/map/2',
      tabQueryUrl: 'https://act.hoyolab.com/*',
      matchesUrl: (url) => {
        try {
          const u = new URL(String(url || ''));
          // Exact host or a real subdomain: a substring test would also accept evil-hoyolab.com
          // and hoyolab.com.evil.com.
          const host = u.hostname.toLowerCase();
          return (host === 'hoyolab.com' || host.endsWith('.hoyolab.com')) && u.pathname.includes('/interactive-map/');
        } catch (e) {
          return false;
        }
      },
      privacyMaskCss: [
        '.mhy-header, .hoyolab-header, .mhy-hoyolab-app-header,',
        '[class*="mhy-header"], [class*="hoyolab-header"], [class*="bbs-header"],',
        '[class*="avatar"], [class*="account"], [class*="user-info"], [class*="login-btn"] {',
        '  opacity: 0 !important;',
        '  pointer-events: none !important;',
        '}'
      ].join('\n')
    },
    kongying: {
      id: 'kongying',
      name: 'Kongying Tavern (空荧酒馆)',
      shortName: 'Kongying',
      openSource: true,
      defaultUrl: 'https://v3.yuanshen.site/',
      tabQueryUrl: 'https://v3.yuanshen.site/*',
      matchesUrl: (url) => {
        try {
          const u = new URL(String(url || ''));
          return u.hostname === 'v3.yuanshen.site' || u.hostname === 'yuanshen.site';
        } catch (e) {
          return false;
        }
      },
      privacyMaskCss: [
        '.extra_btn, .user_info, [class*="avatar"], [class*="account"], [class*="login"] {',
        '  opacity: 0 !important;',
        '  pointer-events: none !important;',
        '}'
      ].join('\n')
    },
    appsample: {
      id: 'appsample',
      name: 'AppSample Map',
      shortName: 'AppSample',
      openSource: false,
      defaultUrl: 'https://genshin-impact-map.appsample.com/',
      tabQueryUrl: 'https://genshin-impact-map.appsample.com/*',
      matchesUrl: (url) => {
        try {
          const u = new URL(String(url || ''));
          return u.hostname === 'genshin-impact-map.appsample.com';
        } catch (e) {
          return false;
        }
      },
      privacyMaskCss: [
        'header, [class*="avatar"], [class*="account"], [class*="profile"], [data-testid*="user"] {',
        '  opacity: 0 !important;',
        '  pointer-events: none !important;',
        '}'
      ].join('\n')
    }
  };

  const PROVIDER_ORDER = ['hoyolab', 'kongying', 'appsample'];
  const ALL_TAB_QUERY_URLS = PROVIDER_ORDER.map((id) => PROVIDERS[id].tabQueryUrl);

  function detectProviderFromUrl(url) {
    if (typeof url !== 'string' || !url) return null;
    for (const id of PROVIDER_ORDER) {
      if (PROVIDERS[id].matchesUrl(url)) return id;
    }
    return null;
  }

  function isSupportedMapUrl(url) {
    return detectProviderFromUrl(url) !== null;
  }

  function getProvider(id) {
    const key = typeof id === 'string' ? id.trim().toLowerCase() : '';
    return PROVIDERS[key] || PROVIDERS.hoyolab;
  }

  function getPrivacyMaskCss(id) {
    return getProvider(id).privacyMaskCss;
  }

  const api = {
    PROVIDERS,
    PROVIDER_ORDER,
    ALL_TAB_QUERY_URLS,
    detectProviderFromUrl,
    isSupportedMapUrl,
    getProvider,
    getPrivacyMaskCss
  };

  if (root && typeof root === 'object') {
    root.GHMapProviders = api;
  }
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : (typeof self !== 'undefined' ? self : this));
