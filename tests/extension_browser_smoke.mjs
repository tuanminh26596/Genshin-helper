// Real-browser smoke test for the HoYoLAB extension (no npm dependencies; Node >= 22).
//
// Loads extension/ unpacked into a throwaway headless Chromium profile, opens the HoYoLAB
// Interactive Map and checks, in the real page MAIN world:
//   * the current SCRIPT_BUILD booted and window.GHI18n exists (a content-script file listed in
//     two manifest entries is injected only once per frame — that left GHI18n undefined);
//   * language precedence: user choice (chrome.storage gh_lang) > desktop app language >
//     browser language; a stale page localStorage value never wins;
//   * the floating re-center button follows the language, also when switched while visible.
// The desktop app is mocked inside the service worker (CDP Fetch on http://127.0.0.1:8765), so
// the test never talks to a running GenshinHelper.
//
// Node simulations of i18n.js cannot catch Chromium injection rules; run this after any change
// to extension/manifest.json, i18n*.js, content_bridge.js, background.js or page_main.js:
//   node tests/extension_browser_smoke.mjs
// Browser: GH_CHROME=<chrome.exe>, else Playwright's Chromium under %LOCALAPPDATA%\ms-playwright.
// Needs network access to act.hoyolab.com. Exit code 0 = pass, 1 = fail, 2 = skipped.
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT_DIR = join(ROOT, 'extension');
const EXT_ID = 'nfkijdoblmphmdljfogooajhbldkeojh';
const MAP_URL = 'https://act.hoyolab.com/ys/app/interactive-map/index.html';
const APP_ORIGIN = 'http://127.0.0.1:8765';
const BOOT_TIMEOUT_MS = 45000;
// Hang guards: every CDP call / DevTools HTTP request gets its own deadline, and the whole run
// has a hard watchdog, so a stalled page or browser fails the check instead of blocking the gate.
const CDP_CALL_TIMEOUT_MS = 15000;
const HTTP_TIMEOUT_MS = 10000;
const WATCHDOG_MS = Number(process.env.GH_SMOKE_WATCHDOG_MS) || 300000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let abortRun = null;

const SCRIPT_BUILD = (readFileSync(join(EXT_DIR, 'page_main.js'), 'utf8').match(/const SCRIPT_BUILD = '([^']+)'/) || [])[1];

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
      const timer = setTimeout(() => j(new Error(`CDP socket did not open within ${CDP_CALL_TIMEOUT_MS} ms`)), CDP_CALL_TIMEOUT_MS);
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
    if (r.result && r.result.exceptionDetails) throw new Error(r.result.exceptionDetails.text);
    return r.result && r.result.result ? r.result.result.value : undefined;
  }
  close() { try { this.ws.close(); } catch (e) {} }
}

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: Boolean(cond), detail });
  console.log(`  [${cond ? 'OK' : 'FAIL'}] ${name}${cond ? '' : `  -> ${JSON.stringify(detail)}`}`);
}

async function main() {
  const chrome = findChrome();
  if (!chrome) {
    console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)');
    return 2;
  }
  const profile = mkdtempSync(join(tmpdir(), 'gh_ext_smoke_'));
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    `--disable-extensions-except=${EXT_DIR}`, `--load-extension=${EXT_DIR}`,
    '--no-first-run', '--no-default-browser-check', '--lang=en-US', 'about:blank',
  ], { stdio: 'ignore' });
  const sessions = [];
  abortRun = () => {
    for (const c of sessions) c.close();
    try { proc.kill(); } catch (e) {}
    try { rmSync(profile, { recursive: true, force: true }); } catch (e) {}
  };
  try {
    let port = null;
    for (let i = 0; i < 150 && !port; i++) {
      await sleep(200);
      const f = join(profile, 'DevToolsActivePort');
      if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
    }
    if (!port) throw new Error('Chromium did not expose a DevTools port');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method, signal: AbortSignal.timeout(HTTP_TIMEOUT_MS) }).then((r) => r.json());

    let swTarget = null;
    for (let i = 0; i < 50 && !swTarget; i++) {
      swTarget = (await http('/json/list')).find((t) => t.type === 'service_worker' && t.url.includes(EXT_ID));
      if (!swTarget) await sleep(200);
    }
    if (!swTarget) throw new Error('extension service worker not found (extension failed to load?)');
    const sw = new Cdp(swTarget.webSocketDebuggerUrl);
    sessions.push(sw);
    await sw.ready;

    // Mock desktop app (language vi) inside the service worker.
    sw.handlers.push((m) => {
      if (m.method !== 'Fetch.requestPaused') return;
      const path = new URL(m.params.request.url).pathname;
      const body = path === '/status'
        ? { ok: true, language: 'vi', engine_active: true, latest_seq: 0 }
        : path === '/poll' ? { events: [], latest_seq: 0 } : { ok: true };
      sw.send('Fetch.fulfillRequest', {
        requestId: m.params.requestId,
        responseCode: 200,
        responseHeaders: [{ name: 'Content-Type', value: 'application/json' }, { name: 'Access-Control-Allow-Origin', value: '*' }],
        body: Buffer.from(JSON.stringify(body)).toString('base64'),
      });
    });
    await sw.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] });
    const setChoice = (lang) => sw.eval(`new Promise((r) => chrome.storage.local.${lang ? `set({ gh_lang: '${lang}' }` : `remove('gh_lang'`}, () => r(true)))`);

    const pageTarget = await http(`/json/new?${encodeURI(MAP_URL)}`, 'PUT');
    const page = new Cdp(pageTarget.webSocketDebuggerUrl);
    sessions.push(page);
    await page.ready;
    await page.send('Runtime.enable');
    // A stale value under the pre-r6 key, as left by older builds (browser default marked explicit).
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: "try { localStorage.setItem('gh_lang', 'en'); } catch (e) {}" });

    const state = `({
      build: window.__gh_booted_build || null,
      i18n: typeof window.GHI18n,
      lang: window.GHI18n ? GHI18n.getLang() : null,
      explicit: window.GHI18n ? GHI18n.hasExplicitPreference() : null,
      focus: window.GHI18n ? GHI18n.t('page.route.focus_btn') : null,
      recenter: (document.getElementById('gh-recenter-btn') || {}).textContent || null
    })`;
    const waitFor = async (pred, timeoutMs = BOOT_TIMEOUT_MS) => {
      const t0 = Date.now();
      let s = null;
      while (Date.now() - t0 < timeoutMs) {
        try { s = await page.eval(state); } catch (e) { s = null; }
        if (s && pred(s)) return s;
        await sleep(300);
      }
      return s;
    };
    const reload = async () => {
      await page.send('Page.reload', { ignoreCache: true });
      await sleep(1000);
    };
    const dragMap = () => page.eval(`(async () => {
      const c = document.querySelector('.leaflet-container');
      if (!c) return false;
      const r = c.getBoundingClientRect();
      const x = r.left + r.width / 2, y = r.top + r.height / 2;
      c.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, clientX: x, clientY: y, button: 0 }));
      await new Promise((res) => setTimeout(res, 160));
      c.dispatchEvent(new MouseEvent('mousemove', { bubbles: true, clientX: x + 60, clientY: y + 10, button: 0 }));
      window.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
      await new Promise((res) => setTimeout(res, 300));
      return true;
    })()`);
    // The map (and its drag-unpin listeners) may still be loading: retry until the button shows.
    const dragUntilRecenter = async (timeoutMs = 30000) => {
      const t0 = Date.now();
      let s = null;
      while (Date.now() - t0 < timeoutMs) {
        await dragMap().catch(() => false);
        s = await page.eval(state).catch(() => null);
        if (s && s.recenter) return s;
        await sleep(700);
      }
      return s;
    };

    console.log(`[1] no user choice, app = vi, browser = en, stale page localStorage gh_lang = en (build ${SCRIPT_BUILD})`);
    await setChoice(null);
    await reload();
    let s = await waitFor((x) => x.build === SCRIPT_BUILD && x.lang === 'vi');
    check('current SCRIPT_BUILD booted in the page', s && s.build === SCRIPT_BUILD, s);
    check('window.GHI18n exists in the page MAIN world', s && s.i18n === 'object', s);
    check('desktop app language applies (stale page value ignored)', s && s.lang === 'vi' && s.explicit === false, s);
    check('page labels translated (vi)', s && s.focus === 'Xem tuyến', s);
    s = await dragUntilRecenter();
    check('re-center button follows vi', s && s.recenter === 'Bám lại vị trí', s);

    console.log('[2] user picked EN in the popup (chrome.storage gh_lang = en), app = vi');
    await setChoice('en');
    await reload();
    s = await waitFor((x) => x.build === SCRIPT_BUILD && x.explicit === true);
    check('user choice beats the app language', s && s.lang === 'en' && s.focus === 'View Route', s);
    s = await dragUntilRecenter();
    check('re-center button follows en', s && s.recenter === 'Re-center', s);

    console.log('[3] user switches to VI while the re-center button is visible');
    await setChoice('vi');
    s = await waitFor((x) => x.lang === 'vi' && x.recenter === 'Bám lại vị trí', 8000);
    check('live switch re-renders the re-center button', s && s.lang === 'vi' && s.recenter === 'Bám lại vị trí', s);
  } finally {
    for (const c of sessions) c.close();
    proc.kill();
    await sleep(800);
    try { rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 300 }); } catch (e) {}
  }
  const failed = results.filter((r) => !r.ok);
  console.log(failed.length ? `FAILED: ${failed.length}/${results.length}` : `ALL ${results.length} EXTENSION BROWSER CHECKS PASSED`);
  return failed.length ? 1 : 0;
}

const watchdog = setTimeout(() => {
  console.error(`[ERROR] smoke test exceeded the ${WATCHDOG_MS} ms watchdog; aborting`);
  if (abortRun) abortRun();
  process.exit(1);
}, WATCHDOG_MS);
// Kept referenced on purpose: main() always ends in process.exit(), and a referenced timer stops
// Node from exiting 0 on an empty event loop while a promise is still pending.
void watchdog;

main().then((code) => process.exit(code), (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  process.exit(1);
});
