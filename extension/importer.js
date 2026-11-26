'use strict';

(function () {
  const UNDO_STORAGE_KEY = 'gh_import_undo_history_v1';
  const BATCH_SIZE = 80;
  const BATCH_DELAY_MS = 500;
  const PERMANENT_MAP_IDS = [2, 7, 9, 34, 36, 37, 40];

  const t = (key, params) => (window.GHI18n ? window.GHI18n.t(key, params) : key);

  const $ = (id) => document.getElementById(id);

  const btnLangToggle = $('btn-lang-toggle');
  const dotEngine = $('dot-engine');
  const textEngine = $('text-engine');
  const dotHoyolab = $('dot-hoyolab');
  const textHoyolab = $('text-hoyolab');
  const btnOpenHoyolab = $('btn-open-hoyolab');
  const btnRecheck = $('btn-recheck');

  const srcTabAppsample = $('src-tab-appsample');
  const srcTabKongying = $('src-tab-kongying');
  const panelAppsample = $('panel-appsample');
  const panelKongying = $('panel-kongying');
  const btnReadAppsample = $('btn-read-appsample');
  const inputAppsampleFile = $('input-appsample-file');
  const btnReadKongying = $('btn-read-kongying');
  const inputKongyingFile = $('input-kongying-file');
  const sourceBanner = $('source-banner');

  const previewEmpty = $('preview-empty');
  const previewLoading = $('preview-loading');
  const previewLoadingText = $('preview-loading-text');
  const previewBody = $('preview-body');
  const kpiSource = $('kpi-source');
  const kpiAlready = $('kpi-already');
  const kpiSelected = $('kpi-selected');
  const kpiUnmatched = $('kpi-unmatched');
  const categoryChips = $('category-chips');
  const chkTierExact = $('chk-tier-exact');
  const chkTierMedium = $('chk-tier-medium');
  const chkTierAmbig = $('chk-tier-ambig');
  const cntTierExact = $('cnt-tier-exact');
  const cntTierMedium = $('cnt-tier-medium');
  const cntTierAmbig = $('cnt-tier-ambig');
  const mapTableBody = $('map-table-body');

  const btnStartSync = $('btn-start-sync');
  const btnCancelSync = $('btn-cancel-sync');
  const btnReloadMap = $('btn-reload-map');
  const progressWrap = $('progress-wrap');
  const progressLabel = $('progress-label');
  const progressPct = $('progress-pct');
  const progressBar = $('progress-bar');
  const syncBanner = $('sync-banner');
  const historyList = $('history-list');

  let engineOnline = false;
  let hoyolabReady = false;
  let hoyolabTabId = null;
  let hoyolabMarkedByMap = {};
  let currentPreview = null;
  let enabledMaps = new Set();
  let syncRunning = false;
  let cancelRequested = false;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function sendBg(msg) {
    return new Promise((resolve) => {
      try {
        chrome.runtime.sendMessage(msg, (res) => {
          const err = chrome.runtime.lastError;
          if (err) {
            resolve({ ok: false, error: err.message });
            return;
          }
          resolve(res || { ok: false, error: 'Empty response' });
        });
      } catch (err) {
        resolve({ ok: false, error: (err && err.message) || 'Extension error' });
      }
    });
  }

  function apiRequest(path, method = 'GET', body = null) {
    return sendBg({ kind: 'API_REQUEST', path, method, body });
  }

  function setBanner(el, text, kind = 'info') {
    if (!el) return;
    if (!text) {
      el.hidden = true;
      el.textContent = '';
      el.className = 'banner';
      return;
    }
    el.hidden = false;
    el.textContent = text;
    el.className = `banner is-${kind}`;
  }

  function updateLangToggleUi() {
    if (!btnLangToggle || !window.GHI18n) return;
    const lang = window.GHI18n.getLang();
    btnLangToggle.dataset.lang = lang;
    for (const opt of btnLangToggle.querySelectorAll('.lang-opt')) {
      opt.classList.toggle('is-active', opt.dataset.langOpt === lang);
    }
  }

  async function requestOptionalOrigins(origins) {
    if (!chrome.permissions || typeof chrome.permissions.request !== 'function') {
      return true;
    }
    const already = await new Promise((res) => {
      chrome.permissions.contains({ origins }, (ok) => {
        void chrome.runtime.lastError;
        res(Boolean(ok));
      });
    });
    if (already) return true;
    return new Promise((res) => {
      chrome.permissions.request({ origins }, (granted) => {
        void chrome.runtime.lastError;
        res(Boolean(granted));
      });
    });
  }

  async function refreshReadiness() {
    dotEngine.className = 'dot is-warn';
    textEngine.textContent = t('importer.status.checking');
    dotHoyolab.className = 'dot is-warn';
    textHoyolab.textContent = t('importer.status.checking');

    const [stRes, mkRes] = await Promise.all([
      apiRequest('/status', 'GET'),
      sendBg({ kind: 'HOYOLAB_GET_MARKED', mapIds: PERMANENT_MAP_IDS })
    ]);

    engineOnline = Boolean(stRes && stRes.ok);
    if (engineOnline) {
      dotEngine.className = 'dot is-ok';
      textEngine.textContent = t('importer.status.engine_online');
    } else {
      dotEngine.className = 'dot is-err';
      textEngine.textContent = t('importer.status.engine_offline');
    }

    if (mkRes && mkRes.ok && mkRes.loggedIn) {
      hoyolabReady = true;
      hoyolabTabId = mkRes.tabId || null;
      hoyolabMarkedByMap = mkRes.markedByMap || {};
      const totalExisting = Object.values(hoyolabMarkedByMap).reduce(
        (sum, arr) => sum + (Array.isArray(arr) ? arr.length : 0),
        0
      );
      dotHoyolab.className = 'dot is-ok';
      textHoyolab.textContent = t('importer.status.hoyolab_ready', { count: totalExisting });
    } else {
      hoyolabReady = false;
      hoyolabTabId = null;
      hoyolabMarkedByMap = {};
      dotHoyolab.className = 'dot is-err';
      textHoyolab.textContent = (mkRes && mkRes.noTab)
        ? t('importer.status.hoyolab_no_tab')
        : t('importer.status.hoyolab_need_login');
    }

    updateSelectionTotals();
  }

  function showPreviewState(mode) {
    previewEmpty.hidden = mode !== 'empty';
    previewLoading.hidden = mode !== 'loading';
    previewBody.hidden = mode !== 'ready';
  }

  function computeSelectedMapIds() {
    const out = {};
    let total = 0;
    if (!currentPreview || !Array.isArray(currentPreview.maps)) {
      return { byMap: out, total: 0 };
    }
    const wantExact = chkTierExact.checked;
    const wantMed = chkTierMedium.checked;
    const wantAmbig = chkTierAmbig.checked;

    for (const m of currentPreview.maps) {
      if (!enabledMaps.has(m.map_id)) continue;
      const ids = new Set();
      if (wantExact) {
        for (const id of (m.exact_ids || [])) ids.add(Number(id));
        for (const id of (m.high_ids || [])) ids.add(Number(id));
      }
      if (wantMed) {
        for (const id of (m.medium_ids || [])) ids.add(Number(id));
      }
      if (wantAmbig) {
        for (const id of (m.ambiguous_ids || [])) ids.add(Number(id));
      }
      if (ids.size > 0) {
        const sorted = Array.from(ids).sort((a, b) => a - b);
        out[String(m.map_id)] = sorted;
        total += sorted.length;
      }
    }
    return { byMap: out, total };
  }

  function updateSelectionTotals() {
    if (!currentPreview) {
      btnStartSync.disabled = true;
      return;
    }
    const { total } = computeSelectedMapIds();
    kpiSelected.textContent = String(total);
    btnStartSync.disabled = syncRunning || total === 0 || !hoyolabReady;
    const lbl = btnStartSync.querySelector('.btn-label');
    if (lbl) {
      lbl.textContent = t('importer.sync.start_btn_count', { count: total });
    }

    // Update per-map "ready" column
    for (const row of mapTableBody.querySelectorAll('tr[data-map-id]')) {
      const mid = Number(row.dataset.mapId);
      const m = currentPreview.maps.find((x) => x.map_id === mid);
      const readyCell = row.querySelector('.col-ready');
      if (!m || !readyCell) continue;
      let c = 0;
      if (chkTierExact.checked) c += (m.exact_ids || []).length + (m.high_ids || []).length;
      if (chkTierMedium.checked) c += (m.medium_ids || []).length;
      if (chkTierAmbig.checked) c += (m.ambiguous_ids || []).length;
      readyCell.textContent = String(c);
    }
  }

  function renderPreview(preview) {
    currentPreview = preview;
    enabledMaps = new Set((preview.maps || []).map((m) => m.map_id));

    const tot = preview.totals || {};
    kpiSource.textContent = String(tot.source_pins || 0);
    kpiAlready.textContent = String(tot.already_marked || 0);
    kpiUnmatched.textContent = String(tot.unmatched || 0);

    const exactHighCnt = (tot.exact || 0) + (tot.high || 0);
    cntTierExact.textContent = String(exactHighCnt);
    cntTierMedium.textContent = String(tot.medium || 0);
    cntTierAmbig.textContent = String(tot.ambiguous || 0);

    chkTierExact.checked = true;
    chkTierMedium.checked = true;
    chkTierAmbig.checked = false;

    // Render category chips safely via DOM APIs
    categoryChips.textContent = '';
    const catLabels = {
      oculi: t('importer.cat.oculi'),
      shrines: t('importer.cat.shrines'),
      chests: t('importer.cat.chests'),
      puzzles: t('importer.cat.puzzles'),
      waypoints: t('importer.cat.waypoints'),
      other: t('importer.cat.other')
    };
    const catCounts = tot.category_counts || {};
    for (const key of ['oculi', 'shrines', 'chests', 'puzzles', 'waypoints', 'other']) {
      const val = Number(catCounts[key] || 0);
      if (val <= 0) continue;
      const chip = document.createElement('span');
      chip.className = 'cat-chip';
      chip.textContent = `${catLabels[key]}: ${val}`;
      categoryChips.appendChild(chip);
    }

    // Render per-map rows
    mapTableBody.textContent = '';
    for (const m of (preview.maps || [])) {
      const tr = document.createElement('tr');
      tr.dataset.mapId = String(m.map_id);

      const tdChk = document.createElement('td');
      const chk = document.createElement('input');
      chk.type = 'checkbox';
      chk.checked = true;
      chk.addEventListener('change', () => {
        if (chk.checked) enabledMaps.add(m.map_id);
        else enabledMaps.delete(m.map_id);
        updateSelectionTotals();
      });
      tdChk.appendChild(chk);

      const tdName = document.createElement('td');
      tdName.textContent = `${m.map_name} (#${m.map_id})`;
      const tdSrc = document.createElement('td');
      tdSrc.textContent = String(m.source_count || 0);
      const tdReady = document.createElement('td');
      tdReady.className = 'col-ready';
      const tdAlready = document.createElement('td');
      tdAlready.textContent = String(m.already_marked_count || 0);
      const tdUnm = document.createElement('td');
      tdUnm.textContent = String(m.unmatched_count || 0);
      const tdRmse = document.createElement('td');
      tdRmse.textContent = m.rmse_px ? `${m.rmse_px} px` : '-';

      tr.append(tdChk, tdName, tdSrc, tdReady, tdAlready, tdUnm, tdRmse);
      mapTableBody.appendChild(tr);
    }

    setBanner(syncBanner, '');
    progressWrap.hidden = true;
    btnReloadMap.hidden = true;
    showPreviewState('ready');
    updateSelectionTotals();
  }

  async function runPreviewRequest(payload, loadingMsg) {
    if (!engineOnline) {
      setBanner(sourceBanner, t('importer.err.engine_offline'), 'error');
      return;
    }
    setBanner(sourceBanner, '');
    previewLoadingText.textContent = loadingMsg || t('importer.preview.loading');
    showPreviewState('loading');

    // Refresh HoYoLAB marked sets before computing delta if HoYoLAB tab is open
    if (hoyolabReady) {
      const mkRes = await sendBg({ kind: 'HOYOLAB_GET_MARKED', mapIds: PERMANENT_MAP_IDS });
      if (mkRes && mkRes.ok && mkRes.loggedIn) {
        hoyolabMarkedByMap = mkRes.markedByMap || {};
        hoyolabTabId = mkRes.tabId || hoyolabTabId;
      }
    }

    const res = await apiRequest('/cross_map_import_preview', 'POST', {
      ...payload,
      already_marked_by_map: hoyolabMarkedByMap
    });

    if (!res || !res.ok || !res.data || res.data.ok === false) {
      showPreviewState('empty');
      const errText = (res && res.data && res.data.error) || (res && res.error) || 'Preview failed';
      setBanner(sourceBanner, t('importer.err.preview_failed', { error: errText }), 'error');
      return;
    }

    const maps = res.data.maps || [];
    if (maps.length === 0) {
      showPreviewState('empty');
      setBanner(sourceBanner, t('importer.err.no_pins_found'), 'warn');
      return;
    }

    renderPreview(res.data);
  }

  // -------------------------------------------------------------------------
  // Source Actions
  // -------------------------------------------------------------------------

  async function handleReadAppsampleTab() {
    setBanner(sourceBanner, '');
    const granted = await requestOptionalOrigins(['https://genshin-impact-map.appsample.com/*']);
    if (!granted) {
      setBanner(sourceBanner, t('importer.err.permission_denied'), 'warn');
      return;
    }
    btnReadAppsample.disabled = true;
    const res = await sendBg({ kind: 'READ_APPSAMPLE_TAB' });
    btnReadAppsample.disabled = false;

    if (!res || !res.ok) {
      const msg = (res && res.noTab)
        ? t('importer.appsample.no_tab')
        : t('importer.err.read_failed', { error: (res && res.error) || 'Unknown error' });
      setBanner(sourceBanner, msg, 'warn');
      return;
    }
    if (!res.foundIds || res.foundIds.length === 0) {
      setBanner(sourceBanner, t('importer.appsample.zero_marked'), 'warn');
      return;
    }
    if (!res.markers || res.markers.length === 0) {
      setBanner(sourceBanner, t('importer.appsample.no_catalog'), 'warn');
      return;
    }

    setBanner(
      sourceBanner,
      t('importer.appsample.tab_read_ok', { count: res.foundIds.length }),
      'success'
    );
    await runPreviewRequest(
      {
        source: 'appsample',
        found_ids: res.foundIds,
        markers: res.markers
      },
      t('importer.preview.matching_appsample', { count: res.foundIds.length })
    );
  }

  async function handleAppsampleFile(file) {
    if (!file) return;
    try {
      const text = await file.text();
      const parsed = JSON.parse(text);
      const foundIds = parsed.found_ids || parsed.ids || null;
      const markers = parsed.markers || parsed.catalog || (Array.isArray(parsed) ? parsed : null);
      if (!markers) {
        setBanner(sourceBanner, t('importer.appsample.invalid_file'), 'error');
        return;
      }
      await runPreviewRequest(
        {
          source: 'appsample',
          ...(foundIds ? { found_ids: foundIds } : {}),
          markers
        },
        t('importer.preview.loading')
      );
    } catch (err) {
      setBanner(sourceBanner, t('importer.err.invalid_json', { error: err.message }), 'error');
    }
  }

  async function handleKongyingFile(file) {
    if (!file) return;
    try {
      const text = await file.text();
      const parsed = JSON.parse(text);
      await runPreviewRequest(
        {
          source: 'kongying',
          kongying_payload: parsed
        },
        t('importer.preview.matching_kongying')
      );
    } catch (err) {
      setBanner(sourceBanner, t('importer.err.invalid_json', { error: err.message }), 'error');
    }
  }

  async function handleReadKongyingTab() {
    setBanner(sourceBanner, '');
    const granted = await requestOptionalOrigins([
      'https://v3.yuanshen.site/*',
      'https://yuanshen.site/*'
    ]);
    if (!granted) {
      setBanner(sourceBanner, t('importer.err.permission_denied'), 'warn');
      return;
    }
    btnReadKongying.disabled = true;
    const res = await sendBg({ kind: 'READ_KONGYING_TAB' });
    btnReadKongying.disabled = false;

    if (!res || !res.ok) {
      const msg = (res && res.noTab)
        ? t('importer.kongying.no_tab')
        : t('importer.err.read_failed', { error: (res && res.error) || 'Unknown error' });
      setBanner(sourceBanner, msg, 'warn');
      return;
    }
    if (!res.ids || res.ids.length === 0) {
      setBanner(sourceBanner, t('importer.kongying.zero_marked'), 'warn');
      return;
    }

    setBanner(sourceBanner, t('importer.kongying.tab_read_ok', { count: res.ids.length }), 'success');
    await runPreviewRequest(
      {
        source: 'kongying',
        kongying_ids: res.ids
      },
      t('importer.preview.matching_kongying')
    );
  }

  // -------------------------------------------------------------------------
  // Undo Snapshot Persistence (chrome.storage.local + Rust backend mirror)
  // -------------------------------------------------------------------------

  function readLocalSnapshots() {
    return new Promise((resolve) => {
      try {
        chrome.storage.local.get([UNDO_STORAGE_KEY], (data) => {
          void chrome.runtime.lastError;
          resolve(Array.isArray(data && data[UNDO_STORAGE_KEY]) ? data[UNDO_STORAGE_KEY] : []);
        });
      } catch (e) {
        resolve([]);
      }
    });
  }

  function writeLocalSnapshots(list) {
    return new Promise((resolve) => {
      try {
        chrome.storage.local.set({ [UNDO_STORAGE_KEY]: list.slice(0, 10) }, () => {
          void chrome.runtime.lastError;
          resolve();
        });
      } catch (e) {
        resolve();
      }
    });
  }

  async function saveUndoSnapshot(snapshot) {
    const local = await readLocalSnapshots();
    const merged = [snapshot, ...local.filter((s) => s.id !== snapshot.id)].slice(0, 10);
    await writeLocalSnapshots(merged);
    if (engineOnline) {
      await apiRequest('/cross_map_import_history', 'POST', { action: 'save', snapshot });
    }
    await loadAndRenderHistory();
  }

  async function markSnapshotReverted(id) {
    const local = await readLocalSnapshots();
    const updated = local.map((s) =>
      s.id === id ? { ...s, reverted: true, reverted_at: Math.floor(Date.now() / 1000) } : s
    );
    await writeLocalSnapshots(updated);
    if (engineOnline) {
      await apiRequest('/cross_map_import_history', 'POST', { action: 'mark_reverted', id });
    }
    await loadAndRenderHistory();
  }

  async function loadAndRenderHistory() {
    const local = await readLocalSnapshots();
    let backend = [];
    if (engineOnline) {
      const res = await apiRequest('/cross_map_import_history', 'GET');
      if (res && res.ok && res.data && Array.isArray(res.data.snapshots)) {
        backend = res.data.snapshots;
      }
    }
    const byId = new Map();
    for (const s of [...backend, ...local]) {
      if (!s || !s.id) continue;
      const prev = byId.get(s.id);
      if (!prev || (s.reverted && !prev.reverted)) {
        byId.set(s.id, s);
      }
    }
    const sorted = Array.from(byId.values()).sort((a, b) => (b.created_at || 0) - (a.created_at || 0));

    historyList.textContent = '';
    if (sorted.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'history-empty';
      empty.textContent = t('importer.history.empty');
      historyList.appendChild(empty);
      return;
    }

    for (const snap of sorted) {
      const row = document.createElement('div');
      row.className = 'history-row';

      const meta = document.createElement('div');
      meta.className = 'history-meta';
      const title = document.createElement('div');
      title.className = 'history-title';
      const srcLabel = snap.source === 'kongying' ? 'Kongying Tavern' : 'Appsample';
      const mapCnt = Object.keys(snap.maps || {}).length;
      title.textContent = t('importer.history.item_title', {
        source: srcLabel,
        count: snap.total_applied || 0,
        maps: mapCnt
      });
      const sub = document.createElement('div');
      sub.className = 'history-sub';
      const dt = snap.created_at ? new Date(snap.created_at * 1000).toLocaleString() : '';
      sub.textContent = `${dt} · ID: ${snap.id}`;
      meta.append(title, sub);
      row.appendChild(meta);

      if (snap.reverted) {
        const badge = document.createElement('span');
        badge.className = 'badge-reverted';
        badge.textContent = t('importer.history.reverted_badge');
        row.appendChild(badge);
      } else {
        const undoBtn = document.createElement('button');
        undoBtn.type = 'button';
        undoBtn.className = 'btn btn-danger btn-sm';
        undoBtn.textContent = t('importer.history.undo_btn', { count: snap.total_applied || 0 });
        undoBtn.addEventListener('click', () => handleUndoSnapshot(snap, undoBtn));
        row.appendChild(undoBtn);
      }
      historyList.appendChild(row);
    }
  }

  // -------------------------------------------------------------------------
  // Live Batch Mark & 1-Click Rollback Engine
  // -------------------------------------------------------------------------

  async function executeBatchLoop(byMap, isDelete, onProgress) {
    const entries = Object.entries(byMap);
    const totalPins = entries.reduce((sum, [, arr]) => sum + arr.length, 0);
    let donePins = 0;
    const appliedByMap = {};

    for (const [midStr, allIds] of entries) {
      const mapId = Number(midStr);
      appliedByMap[midStr] = [];
      for (let i = 0; i < allIds.length; i += BATCH_SIZE) {
        if (cancelRequested) {
          return { cancelled: true, donePins, totalPins, appliedByMap };
        }
        const slice = allIds.slice(i, i + BATCH_SIZE);
        let batchOk = false;
        let lastErr = '';
        for (let attempt = 0; attempt < 3; attempt++) {
          const res = await sendBg({
            kind: 'HOYOLAB_BATCH_MARK',
            tabId: hoyolabTabId,
            mapId,
            pointIds: slice,
            isDelete
          });
          if (res && res.ok) {
            batchOk = true;
            break;
          }
          lastErr = (res && (res.message || res.error)) || 'Batch request failed';
          if (res && res.noTab) break;
          await sleep(900 * (attempt + 1));
        }
        if (!batchOk) {
          return { cancelled: false, error: lastErr, donePins, totalPins, appliedByMap };
        }
        appliedByMap[midStr].push(...slice);
        donePins += slice.length;
        onProgress(donePins, totalPins, mapId);
        if (i + BATCH_SIZE < allIds.length) {
          await sleep(BATCH_DELAY_MS);
        }
      }
    }
    return { cancelled: false, error: null, donePins, totalPins, appliedByMap };
  }

  async function handleStartSync() {
    if (syncRunning || !currentPreview || !hoyolabReady) return;
    const { byMap, total } = computeSelectedMapIds();
    if (total === 0) return;

    syncRunning = true;
    cancelRequested = false;
    btnStartSync.disabled = true;
    btnCancelSync.hidden = false;
    btnReloadMap.hidden = true;
    progressWrap.hidden = false;
    progressBar.style.width = '0%';
    progressLabel.textContent = `0 / ${total}`;
    progressPct.textContent = '0%';
    setBanner(syncBanner, '');

    const res = await executeBatchLoop(byMap, false, (done, tot, mapId) => {
      const pct = tot > 0 ? Math.round((done / tot) * 100) : 100;
      progressBar.style.width = `${pct}%`;
      progressLabel.textContent = t('importer.sync.progress', { done, total: tot, mapId });
      progressPct.textContent = `${pct}%`;
    });

    syncRunning = false;
    btnCancelSync.hidden = true;

    // ALWAYS save an Undo snapshot for every pin that actually reached HoYoLAB (even on cancel/error!)
    const nonEmptyMaps = {};
    let totalApplied = 0;
    for (const [midStr, arr] of Object.entries(res.appliedByMap || {})) {
      if (arr.length > 0) {
        nonEmptyMaps[midStr] = arr;
        totalApplied += arr.length;
      }
    }
    if (totalApplied > 0) {
      await saveUndoSnapshot({
        id: `imp_${Date.now()}`,
        created_at: Math.floor(Date.now() / 1000),
        source: currentPreview.source || 'appsample',
        total_applied: totalApplied,
        maps: nonEmptyMaps,
        reverted: false
      });
      await sendBg({ kind: 'HOYOLAB_REFRESH_AFTER_IMPORT', reload: false });
      btnReloadMap.hidden = false;
    }

    if (res.error) {
      setBanner(
        syncBanner,
        t('importer.sync.partial_error', { done: totalApplied, total, error: res.error }),
        'error'
      );
    } else if (res.cancelled) {
      setBanner(
        syncBanner,
        t('importer.sync.cancelled', { done: totalApplied, total }),
        'warn'
      );
    } else {
      setBanner(
        syncBanner,
        t('importer.sync.done', { count: totalApplied }),
        'success'
      );
    }

    await refreshReadiness();
  }

  async function handleUndoSnapshot(snap, btnEl) {
    if (syncRunning) return;
    if (!hoyolabReady) {
      setBanner(syncBanner, t('importer.status.hoyolab_need_login'), 'warn');
      return;
    }
    syncRunning = true;
    cancelRequested = false;
    btnEl.disabled = true;
    progressWrap.hidden = false;
    setBanner(syncBanner, '');

    const res = await executeBatchLoop(snap.maps || {}, true, (done, tot, mapId) => {
      const pct = tot > 0 ? Math.round((done / tot) * 100) : 100;
      progressBar.style.width = `${pct}%`;
      progressLabel.textContent = t('importer.history.undo_progress', { done, total: tot, mapId });
      progressPct.textContent = `${pct}%`;
    });

    syncRunning = false;
    if (res.error) {
      btnEl.disabled = false;
      setBanner(syncBanner, t('importer.history.undo_error', { error: res.error }), 'error');
    } else {
      await markSnapshotReverted(snap.id);
      await sendBg({ kind: 'HOYOLAB_REFRESH_AFTER_IMPORT', reload: false });
      btnReloadMap.hidden = false;
      setBanner(syncBanner, t('importer.history.undo_done', { count: res.donePins }), 'success');
    }
    await refreshReadiness();
  }

  // -------------------------------------------------------------------------
  // Drag & Drop + UI Listeners
  // -------------------------------------------------------------------------

  function bindDropzone(labelEl, inputEl, onFile) {
    if (!labelEl || !inputEl) return;
    inputEl.addEventListener('change', () => {
      const f = inputEl.files && inputEl.files[0];
      inputEl.value = '';
      if (f) onFile(f);
    });
    labelEl.addEventListener('dragover', (e) => {
      e.preventDefault();
      labelEl.classList.add('is-dragover');
    });
    labelEl.addEventListener('dragleave', () => labelEl.classList.remove('is-dragover'));
    labelEl.addEventListener('drop', (e) => {
      e.preventDefault();
      labelEl.classList.remove('is-dragover');
      const f = e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0];
      if (f) onFile(f);
    });
  }

  function switchSourceTab(source) {
    const isApp = source === 'appsample';
    srcTabAppsample.classList.toggle('is-active', isApp);
    srcTabAppsample.setAttribute('aria-selected', String(isApp));
    srcTabKongying.classList.toggle('is-active', !isApp);
    srcTabKongying.setAttribute('aria-selected', String(!isApp));
    panelAppsample.hidden = !isApp;
    panelKongying.hidden = isApp;
    setBanner(sourceBanner, '');
  }

  srcTabAppsample.addEventListener('click', () => switchSourceTab('appsample'));
  srcTabKongying.addEventListener('click', () => switchSourceTab('kongying'));

  btnReadAppsample.addEventListener('click', handleReadAppsampleTab);
  btnReadKongying.addEventListener('click', handleReadKongyingTab);
  bindDropzone(inputAppsampleFile.closest('label'), inputAppsampleFile, handleAppsampleFile);
  bindDropzone(inputKongyingFile.closest('label'), inputKongyingFile, handleKongyingFile);

  chkTierExact.addEventListener('change', updateSelectionTotals);
  chkTierMedium.addEventListener('change', updateSelectionTotals);
  chkTierAmbig.addEventListener('change', updateSelectionTotals);

  btnStartSync.addEventListener('click', handleStartSync);
  btnCancelSync.addEventListener('click', () => {
    cancelRequested = true;
  });
  btnReloadMap.addEventListener('click', async () => {
    await sendBg({ kind: 'HOYOLAB_REFRESH_AFTER_IMPORT', reload: true });
  });
  btnOpenHoyolab.addEventListener('click', () => sendBg({ kind: 'OPEN_MAP_TAB' }));
  btnRecheck.addEventListener('click', async () => {
    await refreshReadiness();
    await loadAndRenderHistory();
  });

  if (btnLangToggle) {
    btnLangToggle.addEventListener('click', async () => {
      if (!window.GHI18n) return;
      const next = window.GHI18n.getLang() === 'vi' ? 'en' : 'vi';
      await window.GHI18n.setLang(next);
      window.GHI18n.applyDom(document);
      updateLangToggleUi();
      if (currentPreview) renderPreview(currentPreview);
      await loadAndRenderHistory();
      await refreshReadiness();
    });
  }

  async function init() {
    if (window.GHI18n && typeof window.GHI18n.init === 'function') {
      await window.GHI18n.init();
      window.GHI18n.applyDom(document);
      updateLangToggleUi();
    }
    await refreshReadiness();
    await loadAndRenderHistory();
  }

  init();
})();
