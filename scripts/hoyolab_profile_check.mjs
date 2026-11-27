// Headless check that the dedicated HoYoLAB test profile is still signed in (Node >= 22, no deps).
//
//   node scripts/hoyolab_profile_check.mjs <chrome.exe> <profile-dir> [hoyolab|appsample]
//   (normally run through: pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Verify [-Site appsample])
//
// hoyolab:   opens the Interactive Map headless and calls HoYoLAB's own mark_map_point_list endpoint
//            for map 2 from the page; prints ONLY retcode / message / number of marked pins.
// appsample: opens the AppSample map and reads its app store the same way extension/provider_runtime.js
//            does (React context with mapSync/store/catalog); prints ONLY signed-in yes/no and the
//            number of found pins.
// Never prints cookies, tokens, account ids, names or pin data.
// Exit codes: 0 = signed in (retcode 0), 3 = not signed in (e.g. retcode -2004 "Please log in first"),
// 1 = could not check (browser, network or timeout).
import { spawn } from 'node:child_process';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';

const [chrome, profile, site = 'hoyolab'] = process.argv.slice(2);
if (!chrome || !profile || !['hoyolab', 'appsample'].includes(site)) {
  console.log('usage: node scripts/hoyolab_profile_check.mjs <chrome.exe> <profile-dir> [hoyolab|appsample]');
  process.exit(1);
}
const MAP_URL = site === 'appsample'
  ? 'https://genshin-impact-map.appsample.com/'
  : 'https://act.hoyolab.com/ys/app/interactive-map/index.html?lang=en-us';
const LIST_URL = 'https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/mark_map_point_list?map_id=2&app_sn=ys_obc&lang=en-us';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// A DevToolsActivePort left by an earlier run would point at a dead port; the caller has already made
// sure no browser is using this profile, so remove it and wait for Chromium to write a fresh one.
try { rmSync(join(profile, 'DevToolsActivePort'), { force: true }); } catch (e) {}

const proc = spawn(chrome, [
  '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
  '--window-size=1920,1080', '--no-first-run', '--no-default-browser-check', 'about:blank',
], { stdio: 'ignore' });
const finish = (code, line) => {
  console.log(line);
  try { proc.kill(); } catch (e) {}
  setTimeout(() => process.exit(code), 1500);
};
setTimeout(() => finish(1, 'check: timed out after 90 s'), 90000);

async function main() {
  let port = null;
  for (let i = 0; i < 150 && !port; i++) {
    await sleep(200);
    const f = join(profile, 'DevToolsActivePort');
    if (existsSync(f)) port = readFileSync(f, 'utf8').split('\n')[0].trim();
  }
  if (!port) return finish(1, 'check: Chromium did not expose a DevTools port (profile in use?)');
  const target = await fetch(`http://127.0.0.1:${port}/json/new?${encodeURI(MAP_URL)}`, {
    method: 'PUT', signal: AbortSignal.timeout(10000),
  }).then((r) => r.json());
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('CDP socket error')); });
  const pending = new Map();
  let id = 0;
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  };
  const send = (method, params = {}) => new Promise((res) => {
    const i = ++id; pending.set(i, res); ws.send(JSON.stringify({ id: i, method, params }));
  });
  if (site === 'appsample') {
    let st = null;
    for (let attempt = 0; attempt < 15 && !st; attempt++) {
      await sleep(2000);
      const r = await send('Runtime.evaluate', {
        returnByValue: true,
        expression: `(() => {
          const lc = document.querySelector('.leaflet-container');
          if (!lc) return null;
          const fk = Object.keys(lc).find((k) => k.startsWith('__reactFiber$') || k.startsWith('__reactInternalInstance$'));
          let node = fk ? lc[fk] : null;
          for (let d = 0; node && d < 20; d++, node = node.return) {
            const v = node.memoizedProps && node.memoizedProps.value;
            if (v && typeof v === 'object' && v.mapSync && v.store && v.catalog) {
              const s = v.store.getState();
              const found = s.found instanceof Set ? s.found.size : (Array.isArray(s.found) ? s.found.length : 0);
              return { signedIn: Boolean(s.user), found };
            }
          }
          return null;
        })()`,
      });
      st = r.result && r.result.result ? r.result.result.value : null;
    }
    ws.close();
    if (!st) return finish(1, 'check: AppSample app store not found (page did not load or its structure changed)');
    const line = `check: appsample signed_in=${st.signedIn} found_pins=${st.found}`;
    if (st.signedIn) return finish(0, `${line} -> signed in`);
    return finish(3, `${line} -> NOT signed in; run: pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Login -Site appsample`);
  }

  // The request must come from the HoYoLAB page so the session cookies apply; give it time to load.
  let value = null;
  for (let attempt = 0; attempt < 6 && !value; attempt++) {
    await sleep(2000);
    const r = await send('Runtime.evaluate', {
      awaitPromise: true, returnByValue: true,
      expression: `(async () => {
        if (location.host !== 'act.hoyolab.com') return null;
        const r = await fetch(${JSON.stringify(LIST_URL)}, { credentials: 'include', headers: { 'x-rpc-map_version': '4.5' } });
        const j = await r.json();
        const d = (j && j.data) || {};
        const marked = Object.values(d).filter(Array.isArray).reduce((a, x) => a + x.length, 0);
        return { retcode: j ? j.retcode : null, message: j ? String(j.message || '') : '', marked };
      })()`,
    });
    value = r.result && r.result.result ? r.result.result.value : null;
  }
  ws.close();
  if (!value) return finish(1, 'check: map page did not load');
  const line = `check: retcode=${value.retcode} message="${value.message}" marked_on_map2=${value.marked}`;
  if (value.retcode === 0) return finish(0, `${line} -> signed in`);
  return finish(3, `${line} -> NOT signed in; run: pwsh -NoProfile -File scripts/hoyolab_test_profile.ps1 -Login`);
}

main().catch((e) => finish(1, `check: ${e && e.message ? e.message : e}`));
