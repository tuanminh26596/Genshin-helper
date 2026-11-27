// Live end-to-end check of the extension's mark / unmark flow on the real HoYoLAB Interactive Map,
// using the dedicated signed-in test profile (scripts/hoyolab_test_profile.ps1). Node >= 22, no deps.
//
//   pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Verify     # must say "signed in"
//   node tests/hoyolab_live_mark.mjs [--lang en-us] [--capture-footer <file>]
//
// What it does (headless, the desktop app is mocked inside the extension service worker, so a
// running GenshinHelper is never contacted):
//   1. Loads extension/ into the signed-in profile and opens map 2 in the requested HoYoLAB language.
//   2. Picks the nearest unmarked pin from the extension's own nearby-pin query.
//   3. Sends NAV_MARK_PIN action=mark through the mocked /poll, expects /action_feedback "marked"
//      and HoYoLAB's mark_map_point_list to contain the pin.
//   4. Sends NAV_MARK_PIN action=unmark, expects "unmarked" (this needs findAlreadyMarkedButton() to
//      recognise the REAL marked-state popup) and the pin to be gone from the list again.
//   5. Always restores: if the pin is still marked at the end, it is unmarked through HoYoLAB's
//      batch_mark_map_point API, and the run fails loudly if even that does not work.
// Prints only pin ids, statuses and button labels; never cookies, tokens or account data.
// --capture-footer writes the live marked-state popup footer (buttons only, no item text) to a file.
// Exit codes: 0 pass, 1 fail, 2 skipped (no Chromium / profile), 3 profile not signed in.
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT_DIR = process.env.GH_EXTENSION_DIR ? resolve(process.env.GH_EXTENSION_DIR) : join(ROOT, 'extension');
const EXT_ID = 'nfkijdoblmphmdljfogooajhbldkeojh';
const APP_ORIGIN = 'http://127.0.0.1:8765';
const API = 'https://sg-public-api.hoyolab.com/common/map_user/ys_obc';
const CDP_CALL_TIMEOUT_MS = 20000;
const WATCHDOG_MS = Number(process.env.GH_LIVE_WATCHDOG_MS) || 240000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const argv = process.argv.slice(2);
const argVal = (name, dflt) => { const i = argv.indexOf(name); return i >= 0 && argv[i + 1] ? argv[i + 1] : dflt; };
const LANG = argVal('--lang', 'en-us');
const CAPTURE_FOOTER = argVal('--capture-footer', null);
const MAP_ID = 2;
const MAP_URL = `https://act.hoyolab.com/ys/app/interactive-map/index.html?lang=${LANG}#/map/${MAP_ID}?shown_types=5`;
const SCRIPT_BUILD = (readFileSync(join(EXT_DIR, 'page_main.js'), 'utf8').match(/const SCRIPT_BUILD = '([^']+)'/) || [])[1];

function findChrome() {
  if (process.env.GH_CHROME && existsSync(process.env.GH_CHROME)) return process.env.GH_CHROME;
  const base = join(process.env.LOCALAPPDATA || '', 'ms-playwright');
  if (!existsSync(base)) return null;
  const dirs = readdirSync(base).filter((d) => /^chromium-\d+$/.test(d))
    .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]));
  for (const d of dirs) {
    for (const sub of ['chrome-win64', 'chrome-win']) {
      const exe = join(base, d, sub, 'chrome.exe');
      if (existsSync(exe)) return exe;
    }
  }
  return null;
}

const PROFILE = process.env.GH_HOYOLAB_PROFILE
  ? resolve(process.env.GH_HOYOLAB_PROFILE)
  : join(process.env.LOCALAPPDATA || '', 'GenshinHelperDev', 'hoyolab-test-profile');

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
    this.ws.onclose = () => {
      for (const [, p] of this.pending) { clearTimeout(p.timer); p.reject(new Error(`CDP ${p.method}: socket closed`)); }
      this.pending.clear();
    };
    this.ready = new Promise((r, j) => {
      const t = setTimeout(() => j(new Error('CDP socket did not open')), CDP_CALL_TIMEOUT_MS);
      this.ws.onopen = () => { clearTimeout(t); r(); };
      this.ws.onerror = () => { clearTimeout(t); j(new Error('CDP socket error')); };
    });
  }
  send(method, params = {}, timeoutMs = CDP_CALL_TIMEOUT_MS) {
    return new Promise((resolveSend, rejectSend) => {
      const id = ++this.id;
      const timer = setTimeout(() => {
        if (this.pending.delete(id)) rejectSend(new Error(`CDP ${method} timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve: resolveSend, reject: rejectSend, timer, method });
      try { this.ws.send(JSON.stringify({ id, method, params })); } catch (e) { this.pending.delete(id); clearTimeout(timer); rejectSend(e); }
    });
  }
  async eval(expression, timeoutMs) {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }, timeoutMs);
    if (r.result && r.result.exceptionDetails) throw new Error(r.result.exceptionDetails.text);
    return r.result && r.result.result ? r.result.result.value : undefined;
  }
  close() { try { this.ws.close(); } catch (e) {} }
}

const results = [];
function check(name, cond, detail) {
  results.push({ name, ok: Boolean(cond) });
  console.log(`  [${cond ? 'OK' : 'FAIL'}] ${name}${detail !== undefined ? `  -> ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
}

let abortRun = null;
const watchdog = setTimeout(async () => {
  console.error(`[ERROR] live mark test exceeded the ${WATCHDOG_MS} ms watchdog`);
  if (abortRun) await abortRun();
  process.exit(1);
}, WATCHDOG_MS);
void watchdog;

async function main() {
  const chrome = findChrome();
  if (!chrome) { console.log('[SKIP] no Chromium found (set GH_CHROME or install Playwright Chromium)'); return 2; }
  if (!existsSync(join(PROFILE, 'Default'))) {
    console.log(`[SKIP] test profile missing: ${PROFILE} (run scripts/hoyolab_test_profile.ps1 -Login)`);
    return 2;
  }
  try { rmSync(join(PROFILE, 'DevToolsActivePort'), { force: true }); } catch (e) {}
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${PROFILE}`,
    `--disable-extensions-except=${EXT_DIR}`, `--load-extension=${EXT_DIR}`,
    // Desktop-sized viewport: at the small default headless size HoYoLAB lays the pin popup out with
    // 0x0 buttons, which the extension (correctly) refuses to click.
    '--window-size=1920,1080',
    '--no-first-run', '--no-default-browser-check', `--lang=${LANG}`, 'about:blank',
  ], { stdio: 'ignore' });
  const sessions = [];
  let page = null;
  let pin = null;
  let markedBefore = null;
  let originalHash = null;

  const markedHas = async (id) => page.eval(`(async () => {
    const r = await fetch(${JSON.stringify(`${API}/v1/map/point/mark_map_point_list?map_id=${MAP_ID}&app_sn=ys_obc&lang=${LANG}`)},
      { credentials: 'include', headers: { 'x-rpc-map_version': '4.5' } });
    const j = await r.json();
    if (!j || j.retcode !== 0) return { retcode: j ? j.retcode : null };
    return { retcode: 0, has: new RegExp('(^|[^0-9])' + ${JSON.stringify(String(id))} + '([^0-9]|$)').test(JSON.stringify(j.data || {})) };
  })()`);
  const forceUnmark = async (id) => page.eval(`(async () => {
    const r = await fetch(${JSON.stringify(`${API}/v1/map/point/batch_mark_map_point?app_sn=ys_obc&lang=${LANG}`)}, {
      method: 'POST', credentials: 'include',
      headers: { 'Content-Type': 'application/json', 'x-rpc-map_version': '4.5' },
      body: JSON.stringify({ map_id: ${MAP_ID}, app_sn: 'ys_obc', lang: ${JSON.stringify(LANG)}, list: [{ point_id: Number(${JSON.stringify(String(id))}), is_delete: true }] })
    });
    const j = await r.json().catch(() => null);
    return { retcode: j ? j.retcode : null };
  })()`);
  const restore = async () => {
    if (!page || !pin || markedBefore !== false) return true;
    try {
      const now = await markedHas(pin.id);
      if (now.retcode !== 0 || !now.has) return now.retcode === 0;
      console.log(`  [RESTORE] pin ${pin.id} is still marked; unmarking it through batch_mark_map_point`);
      const r = await forceUnmark(pin.id);
      const after = await markedHas(pin.id);
      const ok = r.retcode === 0 && after.retcode === 0 && !after.has;
      console.log(`  [RESTORE] ${ok ? 'restored' : 'FAILED: pin ' + pin.id + ' is still marked on the test account'}`);
      return ok;
    } catch (e) {
      console.log(`  [RESTORE] FAILED for pin ${pin.id}: ${e && e.message ? e.message : e}`);
      return false;
    }
  };
  abortRun = async () => {
    await restore();
    for (const c of sessions) c.close();
    try { proc.kill(); } catch (e) {}
  };

  try {
    let port = null;
    for (let i = 0; i < 150 && !port; i++) {
      await sleep(200);
      const f = join(PROFILE, 'DevToolsActivePort');
      if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
    }
    if (!port) throw new Error('Chromium did not expose a DevTools port (is the profile in use?)');
    const http = (p, method = 'GET') => fetch(`http://127.0.0.1:${port}${p}`, { method, signal: AbortSignal.timeout(10000) }).then((r) => r.json());

    let swTarget = null;
    for (let i = 0; i < 50 && !swTarget; i++) {
      swTarget = (await http('/json/list')).find((t) => t.type === 'service_worker' && t.url.includes(EXT_ID));
      if (!swTarget) await sleep(200);
    }
    if (!swTarget) throw new Error('extension service worker not found (extension failed to load?)');
    const sw = new Cdp(swTarget.webSocketDebuggerUrl);
    sessions.push(sw);
    await sw.ready;

    // Mocked desktop app: /poll hands out queued events one at a time, /action_feedback is recorded.
    let seq = 0;
    const queue = [];
    const feedback = [];
    const fulfill = (requestId, body) => sw.send('Fetch.fulfillRequest', {
      requestId, responseCode: 200,
      responseHeaders: [{ name: 'Content-Type', value: 'application/json' }, { name: 'Access-Control-Allow-Origin', value: '*' }],
      body: Buffer.from(JSON.stringify(body)).toString('base64'),
    }).catch(() => {});
    sw.handlers.push((m) => {
      if (m.method !== 'Fetch.requestPaused') return;
      const url = new URL(m.params.request.url);
      const rid = m.params.requestId;
      if (url.pathname === '/status') return fulfill(rid, { ok: true, language: LANG.startsWith('vi') ? 'vi' : 'en', engine_active: true, latest_seq: seq });
      if (url.pathname === '/poll') {
        const since = Number(url.searchParams.get('since') || -1);
        const ev = queue.find((e) => e._seq > since);
        if (ev) return fulfill(rid, ev);
        return setTimeout(() => fulfill(rid, { type: 'HEARTBEAT', _seq: seq }), 400);
      }
      if (url.pathname === '/action_feedback') {
        try { feedback.push(JSON.parse(m.params.request.postData || '{}')); } catch (e) {}
      }
      return fulfill(rid, { ok: true });
    });
    await sw.send('Fetch.enable', { patterns: [{ urlPattern: `${APP_ORIGIN}/*` }] });

    const pageTarget = await http(`/json/new?${encodeURI(MAP_URL)}`, 'PUT');
    page = new Cdp(pageTarget.webSocketDebuggerUrl);
    sessions.push(page);
    await page.ready;
    await page.send('Runtime.enable');
    // Diagnostics: the extension's own console warnings/errors and uncaught page exceptions.
    const extLogs = [];
    page.handlers.push((m) => {
      if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'warning')) {
        const text = (m.params.args || []).map((a) => a.value !== undefined ? String(a.value) : (a.description || '')).join(' ');
        if (/Genshin Helper|\[GH/.test(text)) extLogs.push(text.slice(0, 200));
      } else if (m.method === 'Runtime.exceptionThrown') {
        const d = m.params.exceptionDetails || {};
        const text = (d.exception && d.exception.description) || d.text || '';
        if (/page_main|Genshin/.test(text)) extLogs.push(('exception: ' + text).slice(0, 200));
      }
    });
    // Diagnostics: HoYoLAB map_user mark requests (path, HTTP status, retcode/message only).
    const markCalls = [];
    const markReqs = new Map();
    page.handlers.push((m) => {
      if (m.method === 'Network.responseReceived') {
        const u = m.params.response.url;
        if (/map_user\/ys_obc\/.*mark/.test(u) && !/mark_map_point_list/.test(u)) {
          markReqs.set(m.params.requestId, { path: new URL(u).pathname.replace(/^.*\/ys_obc/, ''), status: m.params.response.status });
        }
      } else if (m.method === 'Network.loadingFinished' && markReqs.has(m.params.requestId)) {
        const info = markReqs.get(m.params.requestId);
        markReqs.delete(m.params.requestId);
        page.send('Network.getResponseBody', { requestId: m.params.requestId }).then((r) => {
          let j = null;
          try { j = JSON.parse(r.result && r.result.body); } catch (e) {}
          markCalls.push({ ...info, retcode: j ? j.retcode : null, message: j ? String(j.message || '') : '' });
        }).catch(() => markCalls.push({ ...info, retcode: null, message: 'body unavailable' }));
      }
    });
    await page.send('Network.enable');
    const visibleNotices = () => page.eval(`(() => Array.from(document.querySelectorAll('[class*="dialog"], [class*="toast"], [class*="modal"], [class*="message"], [class*="confirm"]'))
      .filter((el) => el.offsetWidth > 0 && el.offsetHeight > 0 && !el.closest('.map-popup') && !el.closest('[id^="gh-"]'))
      .map((el) => el.textContent.replace(/\\s+/g, ' ').trim().slice(0, 120)).filter(Boolean).slice(0, 5))()`).catch(() => []);

    const waitFor = async (expr, timeoutMs) => {
      const t0 = Date.now();
      let v = null;
      while (Date.now() - t0 < timeoutMs) {
        try { v = await page.eval(expr, 10000); } catch (e) { v = null; }
        if (v) return v;
        await sleep(500);
      }
      return v;
    };

    console.log(`[1] boot extension ${SCRIPT_BUILD} on HoYoLAB (${LANG}), map ${MAP_ID}`);
    const booted = await waitFor(`window.__gh_booted_build === ${JSON.stringify(SCRIPT_BUILD)} && !!window.__GH_BRIDGE__ && !!(window.__GH_BRIDGE__.getContext() || {}).map`, 60000);
    check('extension booted with a map context', booted);
    if (!booted) return 1;

    const signedIn = await markedHas(0);
    if (signedIn.retcode !== 0) {
      console.log(`[NOT SIGNED IN] mark_map_point_list retcode=${signedIn.retcode}; run: pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Login`);
      return 3;
    }

    // A signed-in account restores its saved filter (often only waypoints/statues, which the nearby
    // query skips as navigation landmarks), so switch to the collectible category for the test and
    // put the original filter back at the end.
    originalHash = await page.eval('location.hash');
    if (!/shown_types=([0-9]+,)*5(,|&|$)/.test(originalHash || '')) {
      await page.eval(`(() => { location.hash = '#/map/${MAP_ID}?shown_types=5'; return true; })()`);
      await sleep(3000);
    }

    pin = await waitFor(`(() => {
      const ctx = window.__GH_BRIDGE__.getContext();
      const c = ctx && ctx.map && ctx.map.getCenter();
      if (!c) return null;
      const list = window.__GH_BRIDGE__.queryNearbyUncollectedPins(c.lat, c.lng, 600, 60) || [];
      const p = list.find((x) => x && x.id !== null && x.id !== undefined && /^[0-9]+$/.test(String(x.id)) && Number.isFinite(x.lat) && Number.isFinite(x.lng));
      return p ? { id: String(p.id), lat: p.lat, lng: p.lng } : null;
    })()`, 60000);
    check('found an unmarked pin near the map center', pin, pin ? `pin ${pin.id}` : 'none');
    if (!pin) {
      const diag = await page.eval(`(() => {
        const ctx = window.__GH_BRIDGE__.getContext();
        const c = ctx.map.getCenter();
        const all = window.__GH_BRIDGE__.queryNearbyUncollectedPins(c.lat, c.lng, 1e9, 100000) || [];
        return { hash: location.hash, center: [Math.round(c.lat), Math.round(c.lng)], zoom: ctx.map.getZoom(),
          pinsOnMap: all.length, numericIds: all.filter((x) => /^[0-9]+$/.test(String(x.id))).length,
          nearest: all.slice(0, 3).map((x) => ({ id: x.id, distance: x.distance })) };
      })()`).catch((e) => String(e));
      console.log(`  diagnostics: ${JSON.stringify(diag)}`);
      return 1;
    }
    const before = await markedHas(pin.id);
    markedBefore = before.retcode === 0 ? before.has : null;
    check('pin is unmarked on the account before the test', markedBefore === false, before);
    if (markedBefore !== false) { pin = null; return 1; }

    // Record every distinct popup footer the flow shows (labels only, plus the footer markup).
    await page.eval(`(() => {
      window.__ghLiveSnaps = [];
      const seen = new Set();
      const snap = () => {
        const pop = document.querySelector('.map-popup');
        const foot = pop && pop.querySelector('.map-popup__footer');
        if (!foot) return;
        const clone = foot.cloneNode(true);
        clone.querySelectorAll('#gh-popup-navigate-btn').forEach((n) => n.remove());
        const btns = Array.from(foot.querySelectorAll('button')).filter((b) => b.id !== 'gh-popup-navigate-btn');
        const labels = btns.map((b) => b.textContent.replace(/\\s+/g, ' ').trim());
        const state = btns.map((b) => ({
          disabled: Boolean(b.disabled) || b.getAttribute('aria-disabled') === 'true' || /disabled/.test(b.className),
          size: b.offsetWidth + 'x' + b.offsetHeight,
        }));
        const key = labels.join('|') + JSON.stringify(state);
        if (!labels.length || seen.has(key)) return;
        seen.add(key);
        window.__ghLiveSnaps.push({ labels, state, html: clone.outerHTML });
      };
      new MutationObserver(snap).observe(document.body, { subtree: true, childList: true, characterData: true });
    })()`);

    const runAction = async (action) => {
      const ev = { type: 'NAV_MARK_PIN', _seq: ++seq, action, pin_id: pin.id, lat: pin.lat, lng: pin.lng, map_id: MAP_ID, name: 'live-mark-test' };
      queue.push(ev);
      const t0 = Date.now();
      while (Date.now() - t0 < 45000) {
        const fb = feedback.find((f) => f.event_type === 'NAV_MARK' && f.seq === ev._seq);
        if (fb) return fb.status;
        await sleep(250);
      }
      return 'timeout';
    };

    console.log(`[2] NAV_MARK_PIN mark pin ${pin.id}`);
    const markStatus = await runAction('mark');
    check('extension reports the pin marked', markStatus === 'marked', markStatus);
    if (markStatus !== 'marked') {
      console.log(`  diagnostics: mark requests ${JSON.stringify(markCalls)}; notices ${JSON.stringify(await visibleNotices())}`);
    }
    const afterMark = await markedHas(pin.id);
    check('HoYoLAB lists the pin as marked', afterMark.retcode === 0 && afterMark.has, afterMark);

    console.log(`[3] NAV_MARK_PIN unmark pin ${pin.id} (needs the real marked-state popup)`);
    const unmarkStatus = await runAction('unmark');
    check('extension reports the pin unmarked', unmarkStatus === 'unmarked', unmarkStatus);
    const afterUnmark = await markedHas(pin.id);
    check('HoYoLAB no longer lists the pin', afterUnmark.retcode === 0 && !afterUnmark.has, afterUnmark);

    const snaps = (await page.eval('window.__ghLiveSnaps || []')) || [];
    for (const s of snaps) console.log(`  popup footer seen: ${JSON.stringify(s.labels)} ${JSON.stringify(s.state)}`);
    if (extLogs.length) console.log(`  extension console: ${JSON.stringify(extLogs.slice(-6))}`);
    if (CAPTURE_FOOTER && snaps.length) {
      mkdirSync(dirname(resolve(CAPTURE_FOOTER)), { recursive: true });
      writeFileSync(resolve(CAPTURE_FOOTER), JSON.stringify(snaps, null, 2));
      console.log(`  wrote ${snaps.length} footer snapshot(s) to ${CAPTURE_FOOTER}`);
    }
  } finally {
    const restored = await restore();
    if (!restored) check('test pin restored on the account', false);
    if (page && originalHash && /shown_types=/.test(originalHash)) {
      try {
        await page.eval(`(() => { location.hash = ${JSON.stringify(originalHash)}; return true; })()`);
        await sleep(2000);
      } catch (e) {}
    }
    for (const c of sessions) c.close();
    try { proc.kill(); } catch (e) {}
    await sleep(1500);
  }
  const failed = results.filter((r) => !r.ok);
  console.log(failed.length ? `LIVE MARK: FAILED ${failed.length}/${results.length}` : `LIVE MARK: ALL ${results.length} CHECKS PASSED`);
  return failed.length ? 1 : 0;
}

main().then((code) => process.exit(code), async (err) => {
  console.error('[ERROR]', err && err.stack ? err.stack : err);
  if (abortRun) await abortRun();
  process.exit(1);
});
